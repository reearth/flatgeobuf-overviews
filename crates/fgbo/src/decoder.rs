//! FGBO reader: footer discovery, directory, and the zoom-routed read
//! protocol (overview section / full-res body + importance filter /
//! segments).
//!
//! Also works on plain fgb files: every read falls back to the body with
//! on-the-fly Douglas–Peucker, which doubles as the baseline for
//! benchmarking FGBO against plain fgb.

use crate::error::{Error, Result};
use crate::fgb::{
    decode_properties, feature_root, feature_to_geo, read_range, FgbSection, IoStats, PropValue,
};
use crate::format::{Directory, Footer, FOOTER_SIZE};
use crate::importance::{geometry_importance, threshold_q};
use crate::mercator::{sq_tolerance_for_zoom, TileBounds};
use crate::simplify::filter_geometry;
use flatgeobuf::GeometryType;
use geo_types::Geometry;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// A feature returned by a tile query: geometry in lon/lat plus decoded
/// properties.
#[derive(Debug, Clone)]
pub struct TileFeature {
    pub geometry: Geometry<f64>,
    pub properties: Vec<(String, PropValue)>,
}

/// Which data path served a tile query (for reporting/benchmarks).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TileSource {
    /// Overview section at the given level index.
    Overview(usize),
    /// Full-resolution body with precomputed importance filter.
    BodyImportance,
    /// Full-resolution body with on-the-fly simplification (plain fgb).
    BodyLive,
}

/// Result of a tile query.
#[derive(Debug)]
pub struct TileQuery {
    pub features: Vec<TileFeature>,
    pub source: TileSource,
    /// Fragments from the segments section included in `features`.
    pub fragment_features: usize,
}

pub struct FgboReader<R> {
    r: R,
    pub stats: IoStats,
    len: u64,
    directory: Option<Directory>,
    body: FgbSection,
    /// Lazily opened overview sections (parallel to directory.overviews).
    overview_sections: Vec<Option<FgbSection>>,
    segments_section: Option<FgbSection>,
    /// Lazily loaded importance offset table (count+1 u64 entries, small).
    /// Per-feature importance arrays are range-read on demand so a tile
    /// query never loads the whole sidecar.
    sidecar_table: Option<SidecarTable>,
}

struct SidecarTable {
    /// Absolute offset of the payload (u16 arrays).
    payload_offset: u64,
    /// Byte offsets into the payload, count+1 entries.
    offsets: Vec<u64>,
}

impl FgboReader<BufReader<File>> {
    pub fn open_file(path: &Path) -> Result<Self> {
        let f = File::open(path)?;
        Self::open(BufReader::new(f))
    }
}

impl<R: Read + Seek> FgboReader<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let stats = IoStats::default();
        let len = r.seek(SeekFrom::End(0))?;

        // footer discovery (one small range read at EOF)
        let directory = if len >= FOOTER_SIZE as u64 {
            let tail = read_range(&mut r, &stats, len - FOOTER_SIZE as u64, FOOTER_SIZE)?;
            match Footer::decode(&tail) {
                Ok(footer) => {
                    if footer.dir_offset + footer.dir_size > len {
                        return Err(Error::Format("directory out of range".into()));
                    }
                    let dir_buf =
                        read_range(&mut r, &stats, footer.dir_offset, footer.dir_size as usize)?;
                    if Directory::crc32(&dir_buf) != footer.dir_crc32 {
                        return Err(Error::CrcMismatch);
                    }
                    Some(Directory::decode(&dir_buf)?)
                }
                Err(Error::NotFgbo) => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };

        let body = FgbSection::open(&mut r, &stats, 0, len)?;
        let n_overviews = directory.as_ref().map(|d| d.overviews.len()).unwrap_or(0);

        Ok(FgboReader {
            r,
            stats,
            len,
            directory,
            body,
            overview_sections: (0..n_overviews).map(|_| None).collect(),
            segments_section: None,
            sidecar_table: None,
        })
    }

    pub fn is_fgbo(&self) -> bool {
        self.directory.is_some()
    }

    pub fn directory(&self) -> Option<&Directory> {
        self.directory.as_ref()
    }

    pub fn body(&self) -> &FgbSection {
        &self.body
    }

    pub fn file_len(&self) -> u64 {
        self.len
    }

    fn ensure_overview(&mut self, idx: usize) -> Result<()> {
        if self.overview_sections[idx].is_none() {
            let e = &self.directory.as_ref().unwrap().overviews[idx];
            let s = FgbSection::open(&mut self.r, &self.stats, e.offset, e.size)?;
            self.overview_sections[idx] = Some(s);
        }
        Ok(())
    }

    fn ensure_segments(&mut self) -> Result<()> {
        if self.segments_section.is_none() {
            if let Some(e) = self.directory.as_ref().and_then(|d| d.segments.clone()) {
                let s = FgbSection::open(&mut self.r, &self.stats, e.offset, e.size)?;
                self.segments_section = Some(s);
            }
        }
        Ok(())
    }

    /// Load the importance offset table (small: count+1 u64s). The payload
    /// is range-read per feature in [`Self::importance_for`].
    fn ensure_sidecar_table(&mut self) -> Result<()> {
        if self.sidecar_table.is_none() {
            if let Some(e) = self.directory.as_ref().and_then(|d| d.importance.clone()) {
                let head = read_range(&mut self.r, &self.stats, e.offset, 8)?;
                let count = u64::from_le_bytes(head[0..8].try_into().unwrap()) as usize;
                let table_bytes =
                    read_range(&mut self.r, &self.stats, e.offset + 8, (count + 1) * 8)?;
                let offsets: Vec<u64> = table_bytes
                    .chunks_exact(8)
                    .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                    .collect();
                self.sidecar_table = Some(SidecarTable {
                    payload_offset: e.offset + 8 + (count as u64 + 1) * 8,
                    offsets,
                });
            }
        }
        Ok(())
    }

    /// Range-read the importance array of one feature ordinal. Loads the
    /// offset table on first use (lazy: a tile whose features all skip
    /// filtering never pays for the table).
    fn importance_for(&mut self, ordinal: u64) -> Result<Option<Vec<u16>>> {
        self.ensure_sidecar_table()?;
        let Some(table) = &self.sidecar_table else {
            return Ok(None);
        };
        let i = ordinal as usize;
        if i + 1 >= table.offsets.len() {
            return Ok(None);
        }
        let (start, end) = (table.offsets[i], table.offsets[i + 1]);
        let abs = table.payload_offset + start;
        let buf = read_range(&mut self.r, &self.stats, abs, (end - start) as usize)?;
        Ok(Some(
            buf.chunks_exact(2)
                .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
                .collect(),
        ))
    }

    /// Read all features of `section` intersecting the lon/lat bbox,
    /// skipping ordinals in `exclude` (sorted) *before* reading any bytes.
    /// Returns (ordinal, TileFeature) pairs in offset order.
    fn query_section(
        section: &FgbSection,
        r: &mut R,
        stats: &IoStats,
        bbox: (f64, f64, f64, f64),
        exclude: &[u64],
    ) -> Result<Vec<(u64, TileFeature)>> {
        if section.features_count == 0 {
            return Ok(Vec::new());
        }
        let mut items = section.search(r, stats, bbox.0, bbox.1, bbox.2, bbox.3)?;
        if !exclude.is_empty() {
            items.retain(|i| exclude.binary_search(&(i.index as u64)).is_err());
        }
        let header = section.header();
        let geometry_type = header.geometry_type();
        let columns = crate::fgb::column_names(&header);

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let buf = section.read_feature(r, stats, item.offset as u64)?;
            let feature = feature_root(&buf)?;
            let geometry = feature_to_geo(&feature, geometry_type)?;
            let header = section.header();
            let props = decode_properties(&header, &feature)?
                .into_iter()
                .map(|(idx, v)| {
                    let name = columns
                        .get(idx as usize)
                        .map(|(n, _)| n.clone())
                        .unwrap_or_default();
                    (name, v)
                })
                .collect();
            out.push((
                item.index as u64,
                TileFeature {
                    geometry,
                    properties: props,
                },
            ));
        }
        Ok(out)
    }

    /// Query features for tile z/x/y following the FGBO read protocol.
    ///
    /// `extent` is the assumed MVT extent (tolerance derivation),
    /// `buffer_frac` expands the query bbox by that fraction of the tile.
    pub fn query_tile(
        &mut self,
        z: u8,
        x: u32,
        y: u32,
        extent: u32,
        buffer_frac: f64,
    ) -> Result<TileQuery> {
        let bounds = TileBounds::new(z, x, y).buffered(buffer_frac);
        let bbox = bounds.to_lonlat();

        // 1. overview path
        let overview_pos = self.directory.as_ref().and_then(|dir| {
            dir.overviews
                .iter()
                .position(|e| z >= e.min_zoom && z <= e.max_zoom)
        });
        if let Some(pos) = overview_pos {
            self.ensure_overview(pos)?;
            let section = self.overview_sections[pos].take().unwrap();
            let result = Self::query_section(&section, &mut self.r, &self.stats, bbox, &[]);
            self.overview_sections[pos] = Some(section);
            let features = result?.into_iter().map(|(_, f)| f).collect();
            return Ok(TileQuery {
                features,
                source: TileSource::Overview(pos),
                fragment_features: 0,
            });
        }

        // 2. body path
        let q = threshold_q(sq_tolerance_for_zoom(z, extent));
        let use_segments = self
            .directory
            .as_ref()
            .and_then(|d| d.segments.as_ref())
            .map(|s| z >= s.zbase)
            .unwrap_or(false);

        // segmented features are served by fragments at z >= zbase; exclude
        // them from the body query *before* reading their (large) records
        let segmented: Vec<u64> = if use_segments {
            self.directory
                .as_ref()
                .and_then(|d| d.segments.as_ref())
                .map(|s| s.segmented_ordinals.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let body_features =
            Self::query_section(&self.body, &mut self.r, &self.stats, bbox, &segmented)?;

        let has_sidecar = self
            .directory
            .as_ref()
            .map(|d| d.importance.is_some())
            .unwrap_or(false);

        let mut features = Vec::with_capacity(body_features.len());
        let source = if has_sidecar {
            TileSource::BodyImportance
        } else {
            TileSource::BodyLive
        };

        // Reading the importance array costs one range request per feature.
        // For low-vertex geometries (e.g. building footprints) the filter
        // cannot meaningfully reduce anything, so skip the read entirely —
        // keeping extra vertices is always a valid (less aggressive)
        // simplification.
        const SKIP_FILTER_MAX_COORDS: usize = 32;

        for (ordinal, tf) in body_features {
            let TileFeature {
                geometry,
                properties,
            } = tf;
            let filtered = if has_sidecar {
                if crate::simplify::coord_count(&geometry) <= SKIP_FILTER_MAX_COORDS {
                    Some(geometry)
                } else {
                    match self.importance_for(ordinal)? {
                        Some(imp) => filter_geometry(&geometry, &imp, q),
                        None => Some(geometry),
                    }
                }
            } else {
                // plain fgb: live DP (baseline path), in Q32 space like
                // the precomputed sidecar
                let merc = {
                    use geo::MapCoords;
                    geometry.map_coords(|c| {
                        let (qx, qy) = crate::mercator::lonlat_to_q32(c.x, c.y);
                        geo_types::Coord {
                            x: qx as f64,
                            y: qy as f64,
                        }
                    })
                };
                let imp = geometry_importance(&merc);
                filter_geometry(&geometry, &imp, q)
            };
            if let Some(geometry) = filtered {
                features.push(TileFeature {
                    geometry,
                    properties,
                });
            }
        }

        // 3. segments path for large features at high zoom
        let mut fragment_features = 0;
        if use_segments {
            self.ensure_segments()?;
            if let Some(seg) = self.segments_section.take() {
                let result = Self::query_section(&seg, &mut self.r, &self.stats, bbox, &[]);
                self.segments_section = Some(seg);
                let frags = result?;
                fragment_features = frags.len();
                features.extend(frags.into_iter().map(|(_, f)| f));
            }
        }

        Ok(TileQuery {
            features,
            source,
            fragment_features,
        })
    }

    /// Plain-fgb baseline query: ignore all FGBO sections, read the body
    /// with on-the-fly simplification. For benchmarking.
    pub fn query_tile_baseline(
        &mut self,
        z: u8,
        x: u32,
        y: u32,
        extent: u32,
        buffer_frac: f64,
    ) -> Result<TileQuery> {
        let saved = self.directory.take();
        let saved_table = self.sidecar_table.take();
        let result = self.query_tile(z, x, y, extent, buffer_frac);
        self.directory = saved;
        self.sidecar_table = saved_table;
        result
    }

    /// Count body features intersecting a lon/lat bbox using only the
    /// index (no feature record reads). Useful for sampling/diagnostics.
    pub fn body_hit_count(
        &mut self,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    ) -> Result<usize> {
        if self.body.features_count == 0 || !self.body.has_index() {
            return Ok(0);
        }
        Ok(self
            .body
            .search(&mut self.r, &self.stats, min_x, min_y, max_x, max_y)?
            .len())
    }

    /// Geometry type of the body layer.
    pub fn geometry_type(&self) -> GeometryType {
        self.body.header().geometry_type()
    }

    /// Layer name from the body header.
    pub fn layer_name(&self) -> String {
        let n = self.body.header().name().unwrap_or("").to_string();
        if n.is_empty() {
            "layer".into()
        } else {
            n
        }
    }
}
