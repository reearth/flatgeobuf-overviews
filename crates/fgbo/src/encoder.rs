//! FGBO encoder: takes a valid FlatGeobuf file and appends the FGBO
//! extension sections (importance sidecar, overview levels, segments,
//! directory, footer). The fgb body is copied verbatim, so the output is
//! always a valid fgb file and the build is deterministic.

use crate::clip::{clip_geometry, Rect};
use crate::error::{Error, Result};
use crate::fgb::PropValue;
use crate::format::{Directory, Footer, ImportanceEntry, OverviewEntry, SegmentsEntry, SENTINEL};
use crate::importance::{geometry_importance, threshold_q, SidecarStreamWriter};
use crate::mercator::{project, sq_tolerance_for_zoom, unproject};
use crate::simplify::{coord_count, filter_geometry, is_too_small};
use flatgeobuf::{
    ColumnType, FallibleStreamingIterator, FgbCrs, FgbReader, FgbWriter, FgbWriterOptions,
    GeometryType,
};
use geo_types::Geometry;
use geozero::{ColumnValue, PropertyProcessor};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// A feature staged for writing: lon/lat geometry + owned property row.
type FeatureRecord = (Geometry<f64>, Vec<(u16, PropValue)>);

/// One overview level configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelSpec {
    pub min_zoom: u8,
    pub max_zoom: u8,
}

/// Encoder options.
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    /// Overview levels (zoom ranges must be disjoint and ascending).
    pub levels: Vec<LevelSpec>,
    /// MVT tile extent assumed when deriving tolerances.
    pub extent: u32,
    /// Vertex-count threshold for segmenting large features. 0 disables.
    pub v_max: u32,
    /// Clipping grid zoom for segments.
    pub zbase: u8,
    /// Feature-level thinning: drop features smaller than this many
    /// MVT-extent units (bbox, both dimensions) at a level's max zoom.
    /// 16 units == 1 screen pixel at extent 4096 / 256px tiles.
    pub drop_small_units: f64,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        EncodeOptions {
            levels: vec![
                LevelSpec {
                    min_zoom: 0,
                    max_zoom: 4,
                },
                LevelSpec {
                    min_zoom: 5,
                    max_zoom: 8,
                },
                LevelSpec {
                    min_zoom: 9,
                    max_zoom: 11,
                },
            ],
            extent: 4096,
            v_max: 16384,
            zbase: 12,
            drop_small_units: 16.0,
        }
    }
}

/// Encoding result summary.
#[derive(Debug, Clone)]
pub struct EncodeReport {
    pub body_size: u64,
    pub importance_size: u64,
    pub overview_sizes: Vec<(LevelSpec, u64, u64)>, // (level, size, feature_count)
    pub segments_size: u64,
    pub fragment_count: u64,
    pub segmented_features: u64,
    pub total_size: u64,
    pub features_count: u64,
}

/// Owned schema info extracted from the input header.
struct Schema {
    name: String,
    geometry_type: GeometryType,
    columns: Vec<(String, ColumnType)>,
    crs_org: Option<String>,
    crs_code: i32,
    crs_wkt: Option<String>,
    has_z: bool,
}

/// Property collector: reads all properties of the current feature into an
/// owned row.
#[derive(Default)]
struct PropCollector(Vec<(u16, PropValue)>);

impl PropertyProcessor for PropCollector {
    fn property(&mut self, i: usize, _name: &str, v: &ColumnValue) -> geozero::error::Result<bool> {
        let owned = match v {
            ColumnValue::Bool(v) => PropValue::Bool(*v),
            ColumnValue::Byte(v) => PropValue::Byte(*v),
            ColumnValue::UByte(v) => PropValue::UByte(*v),
            ColumnValue::Short(v) => PropValue::Short(*v),
            ColumnValue::UShort(v) => PropValue::UShort(*v),
            ColumnValue::Int(v) => PropValue::Int(*v),
            ColumnValue::UInt(v) => PropValue::UInt(*v),
            ColumnValue::Long(v) => PropValue::Long(*v),
            ColumnValue::ULong(v) => PropValue::ULong(*v),
            ColumnValue::Float(v) => PropValue::Float(*v),
            ColumnValue::Double(v) => PropValue::Double(*v),
            ColumnValue::String(v) => PropValue::String((*v).to_string()),
            ColumnValue::Json(v) => PropValue::Json((*v).to_string()),
            ColumnValue::DateTime(v) => PropValue::DateTime((*v).to_string()),
            ColumnValue::Binary(v) => PropValue::Binary(v.to_vec()),
        };
        self.0.push((i as u16, owned));
        Ok(false)
    }
}

/// Project a lon/lat geometry to unit mercator (y down), in place on a copy.
fn to_mercator(geom: &Geometry<f64>) -> Geometry<f64> {
    use geo::MapCoords;
    geom.map_coords(|c| {
        let (x, y) = project(c.x, c.y);
        geo_types::Coord { x, y }
    })
}

fn to_lonlat(geom: &Geometry<f64>) -> Geometry<f64> {
    use geo::MapCoords;
    geom.map_coords(|c| {
        let (lon, lat) = unproject(c.x, c.y);
        geo_types::Coord { x: lon, y: lat }
    })
}

/// Create a section writer (overview level / segments). `FgbWriter`
/// spools added features to a temp file, so feeding it during the input
/// pass keeps encoder memory independent of section content size.
fn new_section_writer<'a>(
    schema: &'a Schema,
    geometry_type: GeometryType,
) -> Result<FgbWriter<'a>> {
    let options = FgbWriterOptions {
        write_index: true,
        detect_type: false,
        promote_to_multi: false,
        crs: FgbCrs {
            org: schema.crs_org.as_deref(),
            code: schema.crs_code,
            wkt: schema.crs_wkt.as_deref(),
            ..Default::default()
        },
        has_z: false, // simplified outputs are 2D in v0
        ..Default::default()
    };
    let mut writer = FgbWriter::create_with_options(&schema.name, geometry_type, options)?;
    for (name, ty) in &schema.columns {
        writer.add_column(name, *ty, |_, _| {});
    }
    Ok(writer)
}

/// Add one feature (geometry + property row) to a section writer.
fn add_section_feature(
    writer: &mut FgbWriter<'_>,
    schema: &Schema,
    geom: Geometry<f64>,
    props: &[(u16, PropValue)],
) -> Result<()> {
    writer.add_feature_geom(geom, |feat| {
        for (idx, val) in props {
            let name = schema
                .columns
                .get(*idx as usize)
                .map(|(n, _)| n.as_str())
                .unwrap_or("");
            let _ = feat.property(*idx as usize, name, &val.as_column_value());
        }
    })?;
    Ok(())
}

/// Encode `input` (a valid fgb with index and known feature count) into an
/// FGBO file at `output`.
pub fn encode_file(input: &Path, output: &Path, opts: &EncodeOptions) -> Result<EncodeReport> {
    validate_levels(&opts.levels)?;

    // ---- inspect input ------------------------------------------------
    {
        let mut f = File::open(input)?;
        let len = f.metadata()?.len();
        if len >= 32 {
            f.seek(SeekFrom::End(-32))?;
            let mut tail = [0u8; 32];
            f.read_exact(&mut tail)?;
            if Footer::decode(&tail).is_ok() {
                return Err(Error::InvalidInput("input is already an FGBO file".into()));
            }
        }
    }

    let schema;
    {
        let reader = FgbReader::open(BufReader::new(File::open(input)?))?;
        let header = reader.header();
        if header.features_count() == 0 {
            return Err(Error::InvalidInput(
                "input fgb has no explicit features_count (FGBO requires it); rebuild with `fgbo convert`".into(),
            ));
        }
        if header.index_node_size() == 0 {
            return Err(Error::InvalidInput(
                "input fgb has no spatial index (FGBO requires it); rebuild with `fgbo convert`"
                    .into(),
            ));
        }
        if header.has_z() {
            // v0 limitation: importance/simplification is 2D
            tracing_warn("input has Z values; FGBO v0 treats geometries as 2D");
        }
        let crs = header.crs();
        schema = Schema {
            name: header.name().unwrap_or("layer").to_string(),
            geometry_type: header.geometry_type(),
            columns: crate::fgb::column_names(&header),
            crs_org: crs.and_then(|c| c.org()).map(|s| s.to_string()),
            crs_code: crs.map(|c| c.code()).unwrap_or(0),
            crs_wkt: crs.and_then(|c| c.wkt()).map(|s| s.to_string()),
            has_z: header.has_z(),
        };
    }
    let _ = schema.has_z;

    // ---- pass over features, streaming into section writers ------------
    // Memory stays bounded by per-feature transients + offset tables:
    // overview/segments features spool into FgbWriter temp files, the
    // sidecar payload into its own temp file.
    let mut sidecar = SidecarStreamWriter::new()?;
    let level_qs: Vec<u16> = opts
        .levels
        .iter()
        .map(|l| threshold_q(sq_tolerance_for_zoom(l.max_zoom, opts.extent)))
        .collect();
    let level_min_extents: Vec<f64> = opts
        .levels
        .iter()
        .map(|l| opts.drop_small_units / ((1u64 << l.max_zoom) as f64 * opts.extent as f64))
        .collect();

    let mut level_writers: Vec<FgbWriter<'_>> = opts
        .levels
        .iter()
        .map(|_| new_section_writer(&schema, schema.geometry_type))
        .collect::<Result<_>>()?;
    let mut level_counts: Vec<u64> = vec![0; opts.levels.len()];
    // fragments may change geometry type (clipping) => Unknown; created
    // lazily so files without large features never open the temp file
    let mut segments_writer: Option<FgbWriter<'_>> = None;
    let mut fragment_count: u64 = 0;
    let mut segmented_ordinals: Vec<u64> = Vec::new();

    let features_count: u64 = {
        let mut iter = FgbReader::open(BufReader::new(File::open(input)?))?.select_all()?;
        let mut ordinal: u64 = 0;
        while let Some(feature) = iter.next()? {
            use geozero::{FeatureProperties, GeozeroGeometry};

            // geometry in lon/lat as stored
            let mut gw = geozero::geo_types::GeoWriter::new();
            feature.process_geom(&mut gw)?;
            let geom = gw
                .take_geometry()
                .ok_or_else(|| Error::Format(format!("feature {ordinal}: unsupported geometry")))?;

            // properties (owned)
            let mut props = PropCollector::default();
            feature.process_properties(&mut props)?;
            let props = props.0;

            // importance in unit mercator
            let merc = to_mercator(&geom);
            let imp = geometry_importance(&merc);
            debug_assert_eq!(imp.len(), coord_count(&geom));
            sidecar.push(&imp)?;

            // overview levels
            for (li, q) in level_qs.iter().enumerate() {
                if let Some(filtered) = filter_geometry(&geom, &imp, *q) {
                    if !is_too_small(&filtered, level_min_extents[li]) {
                        add_section_feature(&mut level_writers[li], &schema, filtered, &props)?;
                        level_counts[li] += 1;
                    }
                }
            }

            // segments for large features
            if opts.v_max > 0 && coord_count(&geom) as u64 > opts.v_max as u64 {
                // transient: only this one feature's fragments in memory
                let mut fragments: Vec<FeatureRecord> = Vec::new();
                segment_feature(&merc, &props, opts.zbase, &mut fragments);
                if segments_writer.is_none() {
                    segments_writer = Some(new_section_writer(&schema, GeometryType::Unknown)?);
                }
                let writer = segments_writer.as_mut().unwrap();
                for (frag, frag_props) in fragments {
                    add_section_feature(writer, &schema, to_lonlat(&frag), &frag_props)?;
                    fragment_count += 1;
                }
                segmented_ordinals.push(ordinal);
            }

            ordinal += 1;
        }
        ordinal
    };

    // ---- write output ---------------------------------------------------
    let mut out = BufWriter::new(File::create(output)?);

    // body: verbatim copy of input
    let body_size = {
        let mut fin = File::open(input)?;
        std::io::copy(&mut fin, &mut out)?
    };

    // sentinel
    out.write_all(&SENTINEL)?;

    // importance sidecar (payload streamed from its temp file)
    let imp_offset = out.stream_position()?;
    let imp_size = sidecar.write_to(&mut out)?;
    let importance = ImportanceEntry {
        offset: imp_offset,
        size: imp_size,
        feature_count: features_count,
    };

    // overview sections
    let mut overview_entries = Vec::new();
    let mut overview_sizes = Vec::new();
    for ((level, writer), count) in opts
        .levels
        .iter()
        .zip(level_writers)
        .zip(level_counts.iter().copied())
    {
        let offset = out.stream_position()?;
        writer.write(&mut out)?;
        let size = out.stream_position()? - offset;
        overview_entries.push(OverviewEntry {
            offset,
            size,
            min_zoom: level.min_zoom,
            max_zoom: level.max_zoom,
            tolerance_q: level_qs[overview_entries.len()],
            feature_count: count,
        });
        overview_sizes.push((*level, size, count));
    }

    // segments section
    let mut segments = None;
    let mut segments_size = 0;
    if let Some(writer) = segments_writer {
        let offset = out.stream_position()?;
        writer.write(&mut out)?;
        let size = out.stream_position()? - offset;
        segments_size = size;
        segments = Some(SegmentsEntry {
            offset,
            size,
            zbase: opts.zbase,
            v_max: opts.v_max,
            fragment_count,
            segmented_ordinals: segmented_ordinals.clone(),
        });
    }

    // directory + footer
    let dir = Directory {
        importance: Some(importance.clone()),
        overviews: overview_entries,
        segments,
        build_info: format!("fgbo {}", env!("CARGO_PKG_VERSION")),
    };
    let dir_bytes = dir.encode();
    let dir_offset = out.stream_position()?;
    out.write_all(&dir_bytes)?;
    let footer = Footer {
        dir_offset,
        dir_size: dir_bytes.len() as u64,
        dir_crc32: Directory::crc32(&dir_bytes),
    };
    out.write_all(&footer.encode())?;
    let total_size = out.stream_position()?;
    out.flush()?;

    Ok(EncodeReport {
        body_size,
        importance_size: importance.size,
        overview_sizes,
        segments_size,
        fragment_count,
        segmented_features: segmented_ordinals.len() as u64,
        total_size,
        features_count,
    })
}

/// A fragment this small stops the quadtree descent early: descending
/// further would multiply fragment count without reducing read size.
/// Polygon interiors collapse to full-cell rectangles (5 coords) and stop
/// immediately, COPC-octree style, so a continent-sized polygon produces
/// thousands of fragments rather than one per zbase cell (millions).
const FRAGMENT_STOP_VERTS: usize = 64;

/// Clip a (mercator) geometry into grid cells and push fragments.
///
/// Recursive quadtree descent: each level clips against four child cells
/// and only recurses where geometry remains. Descent stops at `zbase`
/// (boundary detail) or as soon as a fragment is small
/// ([`FRAGMENT_STOP_VERTS`]). All emitted cells are power-of-two grid
/// cells at z ≤ zbase, so tile boundaries at z ≥ zbase still nest within
/// fragment edges.
fn segment_feature(
    merc: &Geometry<f64>,
    props: &[(u16, PropValue)],
    zbase: u8,
    out: &mut Vec<FeatureRecord>,
) {
    fn recurse(
        geom: &Geometry<f64>,
        z: u8,
        cx: u64,
        cy: u64,
        zbase: u8,
        props: &[(u16, PropValue)],
        out: &mut Vec<FeatureRecord>,
    ) {
        let n = (1u64 << z) as f64;
        let rect = Rect::new(
            cx as f64 / n,
            cy as f64 / n,
            (cx + 1) as f64 / n,
            (cy + 1) as f64 / n,
        );
        let Some(clipped) = clip_geometry(geom, rect) else {
            return;
        };
        if z == zbase || coord_count(&clipped) <= FRAGMENT_STOP_VERTS {
            out.push((clipped, props.to_vec()));
            return;
        }
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            recurse(&clipped, z + 1, cx * 2 + dx, cy * 2 + dy, zbase, props, out);
        }
    }
    recurse(merc, 0, 0, 0, zbase, props, out);
}

fn validate_levels(levels: &[LevelSpec]) -> Result<()> {
    let mut prev_max: Option<u8> = None;
    for l in levels {
        if l.min_zoom > l.max_zoom {
            return Err(Error::InvalidInput(format!(
                "invalid level: min_zoom {} > max_zoom {}",
                l.min_zoom, l.max_zoom
            )));
        }
        if let Some(p) = prev_max {
            if l.min_zoom <= p {
                return Err(Error::InvalidInput(
                    "overview levels must be ascending and disjoint".into(),
                ));
            }
        }
        prev_max = Some(l.max_zoom);
    }
    Ok(())
}

fn tracing_warn(msg: &str) {
    eprintln!("warning: {msg}");
}
