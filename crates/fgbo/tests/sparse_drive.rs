//! Drive the decoder through SparseReader the way a browser would:
//! fail → learn the missing range → feed bytes → retry. Verifies the
//! retry loop terminates and produces tiles identical to local reads.

use fgbo::sparse::{ChunkCache, MissingRange, SparseReader};
use fgbo::{encode_file, render_tile, EncodeOptions, FgboReader, TileOptions};
use flatgeobuf::{ColumnType, FgbWriter, FgbWriterOptions, GeometryType};
use geo_types::{Coord, Geometry, LineString, Polygon};
use geozero::{ColumnValue, PropertyProcessor};
use std::path::PathBuf;

fn write_test_fgb(path: &PathBuf) {
    let mut w = FgbWriter::create_with_options(
        "sparsetest",
        GeometryType::Polygon,
        FgbWriterOptions {
            write_index: true,
            detect_type: false,
            promote_to_multi: false,
            ..Default::default()
        },
    )
    .unwrap();
    w.add_column("name", ColumnType::String, |_, _| {});
    for i in 0..12 {
        for j in 0..12 {
            let (lon, lat) = (130.0 + i as f64 * 0.7, 31.0 + j as f64 * 0.5);
            let mut ring: Vec<Coord<f64>> = (0..33)
                .map(|k| {
                    let a = k as f64 / 32.0 * std::f64::consts::TAU;
                    Coord {
                        x: lon + 0.12 * a.cos(),
                        y: lat + 0.08 * a.sin(),
                    }
                })
                .collect();
            ring.push(ring[0]);
            let geom = Geometry::Polygon(Polygon::new(LineString(ring), vec![]));
            let label = format!("p{i}-{j}");
            w.add_feature_geom(geom, |feat| {
                feat.property(0, "name", &ColumnValue::String(&label))
                    .unwrap();
            })
            .unwrap();
        }
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    w.write(&mut out).unwrap();
}

#[test]
fn retry_loop_renders_identical_tiles() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.fgb");
    let out = dir.path().join("out.fgb");
    write_test_fgb(&src);
    encode_file(&src, &out, &EncodeOptions::default()).unwrap();
    let bytes = std::fs::read(&out).unwrap();

    let cache = ChunkCache::new();
    let missing = MissingRange::new();
    let mut reader: Option<FgboReader<SparseReader>> = None;
    let mut local = FgboReader::open_file(&out).unwrap();

    // simulated fetch: serve the requested range from the byte array,
    // padded to 16 KiB the way a real driver would
    let fetches = std::cell::Cell::new(0usize);
    let fetch = |cache: &ChunkCache, offset: u64, len: u64| {
        fetches.set(fetches.get() + 1);
        let padded = len.max(16 * 1024);
        let end = (offset + padded).min(bytes.len() as u64);
        cache.insert(offset, bytes[offset as usize..end as usize].to_vec());
    };

    for (z, x, y) in [(2u8, 3u32, 1u32), (6, 55, 25), (13, 7060, 3265)] {
        let tile = loop {
            // (re)open until the header state is available
            if reader.is_none() {
                let sr = SparseReader::new(cache.clone(), missing.clone(), bytes.len() as u64);
                match FgboReader::open(sr) {
                    Ok(r) => reader = Some(r),
                    Err(_) => {
                        let (o, l) = missing.take().expect("open failed without missing range");
                        fetch(&cache, o, l);
                        continue;
                    }
                }
            }
            match render_tile(reader.as_mut().unwrap(), z, x, y, &TileOptions::default()) {
                Ok(t) => break t,
                Err(e) => {
                    let (o, l) = missing
                        .take()
                        .unwrap_or_else(|| panic!("tile failed without missing range: {e}"));
                    fetch(&cache, o, l);
                }
            }
        };
        let want = render_tile(&mut local, z, x, y, &TileOptions::default()).unwrap();
        assert_eq!(tile.data, want.data, "tile {z}/{x}/{y} differs");
    }

    assert!(fetches.get() > 0, "loop must have fetched something");
    assert!(
        fetches.get() < 100,
        "too many round trips: {} (progress broken?)",
        fetches.get()
    );

    // warm tiles need no new fetches
    let before = fetches.get();
    let _ = loop {
        match render_tile(reader.as_mut().unwrap(), 2, 3, 1, &TileOptions::default()) {
            Ok(t) => break t,
            Err(_) => {
                let (o, l) = missing.take().unwrap();
                fetch(&cache, o, l);
            }
        }
    };
    assert_eq!(fetches.get(), before, "warm tile must be served from cache");
}
