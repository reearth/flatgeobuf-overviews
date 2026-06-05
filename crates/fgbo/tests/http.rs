//! HTTP range reader integration test: serve an FGBO file from a minimal
//! range-supporting HTTP server and read tiles through HttpRangeReader.
#![cfg(feature = "http")]

use fgbo::{encode_file, render_tile, EncodeOptions, FgboReader, HttpRangeReader, TileOptions};
use flatgeobuf::{ColumnType, FgbWriter, FgbWriterOptions, GeometryType};
use geo_types::{Coord, Geometry, LineString, Polygon};
use geozero::{ColumnValue, PropertyProcessor};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

fn write_test_fgb(path: &PathBuf) {
    let mut w = FgbWriter::create_with_options(
        "httptest",
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
    let mut id = 0;
    for i in 0..10 {
        for j in 0..10 {
            let (lon, lat) = (130.0 + i as f64 * 0.8, 31.0 + j as f64 * 0.6);
            let ring: Vec<Coord<f64>> = (0..17)
                .map(|k| {
                    let a = k as f64 / 16.0 * std::f64::consts::TAU;
                    Coord {
                        x: lon + 0.1 * a.cos(),
                        y: lat + 0.07 * a.sin(),
                    }
                })
                .collect();
            let mut ring = ring;
            ring.push(ring[0]);
            let geom = Geometry::Polygon(Polygon::new(LineString(ring), vec![]));
            w.add_feature_geom(geom, |feat| {
                feat.property(0, "name", &ColumnValue::String(&format!("f{id}")))
                    .unwrap();
            })
            .unwrap();
            id += 1;
        }
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    w.write(&mut out).unwrap();
}

/// Minimal HTTP/1.1 server answering GET with Range support, single file.
fn serve_file(bytes: Vec<u8>) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut range: Option<(u64, u64)> = None;
            let mut line = String::new();
            // request line
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let close = line.contains("/quit");
            // headers
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                let lower = h.to_ascii_lowercase();
                if let Some(spec) = lower.strip_prefix("range:") {
                    if let Some(r) = spec.trim().strip_prefix("bytes=") {
                        let mut parts = r.trim().splitn(2, '-');
                        let start: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
                        let end: u64 = parts
                            .next()
                            .filter(|s| !s.is_empty())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(bytes.len() as u64 - 1);
                        range = Some((start, end.min(bytes.len() as u64 - 1)));
                    }
                }
            }
            let response = match range {
                Some((s, e)) if s <= e => {
                    let body = &bytes[s as usize..=e as usize];
                    let mut head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        bytes.len(),
                        body.len()
                    )
                    .into_bytes();
                    head.extend_from_slice(body);
                    head
                }
                _ => {
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )
                    .into_bytes();
                    head.extend_from_slice(&bytes);
                    head
                }
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            if close {
                break;
            }
        }
    });
    (addr, handle)
}

#[test]
fn http_range_reading() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.fgb");
    let out = dir.path().join("out.fgb");
    write_test_fgb(&src);
    encode_file(&src, &out, &EncodeOptions::default()).unwrap();
    let bytes = std::fs::read(&out).unwrap();

    let (addr, handle) = serve_file(bytes.clone());
    let url = format!("http://{addr}/file.fgb");

    // open over HTTP and compare against local reads
    let http = HttpRangeReader::open(&url).unwrap();
    assert_eq!(http.len(), bytes.len() as u64);

    let mut remote = FgboReader::open(http).unwrap();
    assert!(remote.is_fgbo());
    let mut local = FgboReader::open_file(&out).unwrap();

    for (z, x, y) in [(2u8, 3u32, 1u32), (6, 55, 25), (13, 7060, 3260)] {
        remote.stats.reset();
        let rt = render_tile(&mut remote, z, x, y, &TileOptions::default()).unwrap();
        let lt = render_tile(&mut local, z, x, y, &TileOptions::default()).unwrap();
        assert_eq!(rt.data, lt.data, "tile {z}/{x}/{y} must match local read");
        assert!(
            remote.stats.requests() < 60,
            "tile {z}/{x}/{y}: {} requests (coalescing broken?)",
            remote.stats.requests()
        );
    }

    // shut the server down
    let _ = ureq::get(format!("http://{addr}/quit")).call();
    let _ = handle.join();
}
