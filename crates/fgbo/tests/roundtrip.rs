//! End-to-end: synthetic fgb -> FGBO encode -> plain-fgb compat ->
//! tile queries on every path (overview / importance / segments) ->
//! determinism.

use fgbo::{encode_file, render_tile, EncodeOptions, FgboReader, TileOptions, TileSource};
use flatgeobuf::{
    ColumnType, FallibleStreamingIterator, FgbReader, FgbWriter, FgbWriterOptions, GeometryType,
};
use geo_types::{polygon, Coord, Geometry, LineString, Polygon};
use geozero::{ColumnValue, PropertyProcessor};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

/// A closed ring approximating a circle around (lon, lat).
fn circle(lon: f64, lat: f64, r_deg: f64, n: usize) -> Polygon<f64> {
    let mut coords: Vec<Coord<f64>> = (0..n)
        .map(|i| {
            let a = i as f64 / n as f64 * std::f64::consts::TAU;
            Coord {
                x: lon + r_deg * a.cos(),
                y: lat + r_deg * 0.7 * a.sin(),
            }
        })
        .collect();
    coords.push(coords[0]);
    Polygon::new(LineString(coords), vec![])
}

/// Long jagged line with many vertices (spans ~1 degree).
fn jagged_line(n: usize) -> LineString<f64> {
    LineString(
        (0..n)
            .map(|i| {
                let t = i as f64 / (n - 1) as f64;
                Coord {
                    x: 130.0 + 1.0 * t,
                    y: 33.0 + 0.5 * t + 0.05 * ((i % 7) as f64 - 3.0) / 3.0,
                }
            })
            .collect(),
    )
}

/// Tile coordinates containing the given lon/lat at zoom z.
fn tile_at(z: u8, lon: f64, lat: f64) -> (u32, u32) {
    let (mx, my) = fgbo::mercator::project(lon, lat);
    let n = (1u64 << z) as f64;
    ((mx * n) as u32, (my * n) as u32)
}

fn write_test_fgb(path: &PathBuf, big_vertices: usize) {
    let mut w = FgbWriter::create_with_options(
        "testlayer",
        GeometryType::Unknown,
        FgbWriterOptions {
            write_index: true,
            detect_type: false,
            promote_to_multi: false,
            ..Default::default()
        },
    )
    .unwrap();
    w.add_column("name", ColumnType::String, |_, _| {});
    w.add_column("pop", ColumnType::Int, |_, _| {});

    // a few hundred small circles spread over Japan + one big jagged line
    let mut id = 0;
    for i in 0..15 {
        for j in 0..15 {
            let lon = 128.0 + i as f64 * 0.9;
            let lat = 31.0 + j as f64 * 0.7;
            let geom = Geometry::Polygon(circle(lon, lat, 0.05 + 0.01 * (id % 5) as f64, 64));
            w.add_feature_geom(geom, |feat| {
                feat.property(0, "name", &ColumnValue::String(&format!("c{id}")))
                    .unwrap();
                feat.property(1, "pop", &ColumnValue::Int(id)).unwrap();
            })
            .unwrap();
            id += 1;
        }
    }
    let big = Geometry::LineString(jagged_line(big_vertices));
    w.add_feature_geom(big, |feat| {
        feat.property(0, "name", &ColumnValue::String("big"))
            .unwrap();
        feat.property(1, "pop", &ColumnValue::Int(-1)).unwrap();
    })
    .unwrap();

    // one tiny polygon that must be thinned out of coarse overviews
    let tiny = Geometry::Polygon(polygon![
        (x: 135.0, y: 35.0), (x: 135.0001, y: 35.0), (x: 135.0001, y: 35.0001), (x: 135.0, y: 35.0)
    ]);
    w.add_feature_geom(tiny, |feat| {
        feat.property(0, "name", &ColumnValue::String("tiny"))
            .unwrap();
        feat.property(1, "pop", &ColumnValue::Int(0)).unwrap();
    })
    .unwrap();

    let mut out = std::io::BufWriter::new(File::create(path).unwrap());
    w.write(&mut out).unwrap();
}

struct Counter(usize);
impl PropertyProcessor for Counter {
    fn property(&mut self, _: usize, _: &str, _: &ColumnValue) -> geozero::error::Result<bool> {
        self.0 += 1;
        Ok(false)
    }
}

#[test]
fn end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.fgb");
    let out = dir.path().join("out.fgb");
    write_test_fgb(&src, 40_000);

    // ---- encode ------------------------------------------------------
    let opts = EncodeOptions {
        v_max: 16384,
        ..Default::default()
    };
    let report = encode_file(&src, &out, &opts).unwrap();
    assert_eq!(report.features_count, 15 * 15 + 2);
    assert_eq!(
        report.segmented_features, 1,
        "jagged line must be segmented"
    );
    assert!(report.fragment_count > 1);
    assert_eq!(report.overview_sizes.len(), 3);
    // coarsest overview must thin features
    let (_, _, coarse_count) = report.overview_sizes[0];
    assert!(
        coarse_count < report.features_count,
        "coarse overview must drop small features ({coarse_count} vs {})",
        report.features_count
    );

    // ---- plain fgb compatibility (Rust reference reader) --------------
    {
        let mut fgb = FgbReader::open(BufReader::new(File::open(&out).unwrap()))
            .unwrap()
            .select_all()
            .unwrap();
        let mut n = 0;
        while let Some(f) = fgb.next().unwrap() {
            let mut c = Counter(0);
            use geozero::FeatureProperties;
            f.process_properties(&mut c).unwrap();
            assert_eq!(c.0, 2);
            n += 1;
        }
        assert_eq!(n, 227, "plain reader must see exactly the body features");
    }
    {
        // bbox query must also work and stay inside the body
        let mut fgb = FgbReader::open(BufReader::new(File::open(&out).unwrap()))
            .unwrap()
            .select_bbox(128.0, 30.0, 142.0, 42.0)
            .unwrap();
        let mut n = 0;
        while let Some(_f) = fgb.next().unwrap() {
            n += 1;
        }
        assert!(n > 0);
    }
    {
        // sequential/streaming read (the EOF-reader risk path) must stop at
        // features_count and never reach the trailer
        let mut fgb = FgbReader::open(BufReader::new(File::open(&out).unwrap()))
            .unwrap()
            .select_all_seq()
            .unwrap();
        let mut n = 0;
        while let Some(_f) = fgb.next().unwrap() {
            n += 1;
        }
        assert_eq!(n, 227, "select_all_seq must stop at features_count");
    }

    // ---- FGBO reader ---------------------------------------------------
    let mut reader = FgboReader::open_file(&out).unwrap();
    assert!(reader.is_fgbo());
    let directory = reader.directory().unwrap().clone();
    assert_eq!(directory.overviews.len(), 3);
    assert!(directory.importance.is_some());
    assert!(directory.segments.is_some());

    // low zoom -> coarsest overview level
    let (x2, y2) = tile_at(2, 135.0, 35.0);
    let q = reader.query_tile(2, x2, y2, 4096, 64.0 / 4096.0).unwrap();
    assert!(
        matches!(q.source, TileSource::Overview(0)),
        "{:?}",
        q.source
    );
    assert!(!q.features.is_empty());

    // z10 -> finest overview level (9-11)
    let (x10, y10) = tile_at(10, 135.0, 35.0);
    let q = reader
        .query_tile(10, x10, y10, 4096, 64.0 / 4096.0)
        .unwrap();
    assert!(
        matches!(q.source, TileSource::Overview(2)),
        "{:?}",
        q.source
    );

    // high zoom (>= zbase): body + importance; segmented feature served
    // as fragments
    let (x13, y13) = tile_at(13, 130.5, 33.25);
    let q = reader
        .query_tile(13, x13, y13, 4096, 64.0 / 4096.0)
        .unwrap();
    assert!(matches!(q.source, TileSource::BodyImportance));
    assert!(
        q.fragment_features > 0,
        "z13 tile over the big line must include fragments"
    );
    // every returned big-line piece must be a fragment, not the whole line
    for f in &q.features {
        if let Geometry::LineString(ls) = &f.geometry {
            assert!(
                ls.0.len() < 40_000,
                "fragments must be cell-clipped, not the full line"
            );
        }
    }

    // ---- tiles -----------------------------------------------------------
    let topts = TileOptions::default();
    let t = render_tile(&mut reader, 2, x2, y2, &topts).unwrap();
    assert!(!t.data.is_empty());
    assert!(t.feature_count > 0, "z2 tile must contain features");

    let t = render_tile(&mut reader, 10, x10, y10, &topts).unwrap();
    assert!(!t.data.is_empty());

    // baseline (plain fgb behavior) still works
    let topts_base = TileOptions {
        baseline: true,
        ..Default::default()
    };
    let t = render_tile(&mut reader, 2, x2, y2, &topts_base).unwrap();
    assert!(matches!(t.source, TileSource::BodyLive));

    // ---- io stats: overview tile reads far less than baseline -----------
    reader.stats.reset();
    let _ = render_tile(&mut reader, 1, 1, 0, &TileOptions::default()).unwrap();
    let overview_bytes = reader.stats.bytes();
    reader.stats.reset();
    let _ = render_tile(&mut reader, 1, 1, 0, &topts_base).unwrap();
    let baseline_bytes = reader.stats.bytes();
    assert!(
        overview_bytes < baseline_bytes / 2,
        "overview path ({overview_bytes} B) must read much less than baseline ({baseline_bytes} B)"
    );

    // ---- determinism ----------------------------------------------------
    let out2 = dir.path().join("out2.fgb");
    encode_file(&src, &out2, &opts).unwrap();
    let a = std::fs::read(&out).unwrap();
    let b = std::fs::read(&out2).unwrap();
    assert_eq!(a, b, "encode must be byte-deterministic");
}

#[test]
fn rejects_double_encode() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.fgb");
    let out = dir.path().join("out.fgb");
    let out2 = dir.path().join("out2.fgb");
    write_test_fgb(&src, 100);
    encode_file(&src, &out, &EncodeOptions::default()).unwrap();
    let err = encode_file(&out, &out2, &EncodeOptions::default());
    assert!(err.is_err(), "encoding an FGBO file again must fail");
}

#[test]
fn plain_fgb_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.fgb");
    write_test_fgb(&src, 100);
    let mut reader = FgboReader::open_file(&src).unwrap();
    assert!(!reader.is_fgbo());
    let q = reader.query_tile(3, 7, 3, 4096, 0.0).unwrap();
    assert!(matches!(q.source, TileSource::BodyLive));
    let t = render_tile(&mut reader, 3, 7, 3, &TileOptions::default()).unwrap();
    assert!(!t.data.is_empty());
}
