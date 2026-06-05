//! Benchmark: plain-fgb baseline vs FGBO read path on the same file,
//! per zoom band, over randomly sampled non-empty tiles.

use crate::synth::Rng;
use crate::util;
use anyhow::{bail, Result};
use fgbo::{render_tile, TileOptions, TileSource};
use std::collections::BTreeMap;
use std::time::Instant;

pub struct BenchOptions {
    pub zooms: Vec<u8>,
    pub tiles_per_zoom: u32,
    pub seed: u64,
}

impl Default for BenchOptions {
    fn default() -> Self {
        BenchOptions {
            zooms: vec![8, 10, 12, 14],
            tiles_per_zoom: 8,
            seed: 1,
        }
    }
}

#[derive(Default, Clone)]
struct Sample {
    gen_ms: f64,
    bytes: u64,
    features: usize,
    source: String,
}

fn median<T: Copy + PartialOrd>(values: &mut [T]) -> T {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn fmt_bytes(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

/// Tile x/y containing the given lon/lat at zoom z.
fn tile_at(z: u8, lon: f64, lat: f64) -> (u32, u32) {
    let (mx, my) = fgbo::mercator::project(lon, lat);
    let n = (1u64 << z) as f64;
    ((mx * n).min(n - 1.0) as u32, (my * n).min(n - 1.0) as u32)
}

pub fn bench(file: &str, opts: &BenchOptions) -> Result<()> {
    let mut reader = util::open_reader(file)?;
    if !reader.is_fgbo() {
        bail!("{file} is not an FGBO file (run `fgbo build` first)");
    }

    // data envelope for tile sampling
    let envelope: Vec<f64> = {
        let header = reader.body().header();
        match header.envelope() {
            Some(e) if e.len() >= 4 => (0..4).map(|i| e.get(i)).collect(),
            _ => vec![-180.0, -85.0, 180.0, 85.0],
        }
    };
    let (west, south, east, north) = (envelope[0], envelope[1], envelope[2], envelope[3]);

    println!(
        "# fgbo bench — {} ({}, {} features)\n",
        file,
        fmt_bytes(reader.file_len()),
        reader.body().features_count,
    );
    println!(
        "{} tiles per zoom, sampled over non-empty data (seed {})\n",
        opts.tiles_per_zoom, opts.seed
    );
    println!("| z | tiles | baseline ms | FGBO ms | speedup | baseline read | FGBO read | I/O ratio | feats base | feats FGBO | FGBO source |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|");

    let mut rng = Rng::new(opts.seed);
    for &z in &opts.zooms {
        // ---- sample tiles, weighted by feature density -----------------
        // probe random points, count hits via the index only (no feature
        // reads), then pick tiles with probability proportional to their
        // feature count — approximating "tiles a viewer actually looks at"
        let mut candidates: Vec<((u32, u32), usize)> = Vec::new();
        let mut attempts = 0;
        while candidates.len() < 64 && attempts < 500 {
            attempts += 1;
            let lon = rng.range(west, east);
            let lat = rng.range(south, north);
            let (x, y) = tile_at(z, lon, lat);
            if candidates.iter().any(|(t, _)| *t == (x, y)) {
                continue;
            }
            let b = fgbo::mercator::TileBounds::new(z, x, y).to_lonlat();
            let hits = reader.body_hit_count(b.0, b.1, b.2, b.3)?;
            if hits > 0 {
                candidates.push(((x, y), hits));
            }
        }
        let mut tiles: Vec<(u32, u32)> = Vec::new();
        while tiles.len() < opts.tiles_per_zoom as usize && !candidates.is_empty() {
            let total: usize = candidates.iter().map(|(_, h)| h).sum();
            let mut pick = (rng.f64() * total as f64) as usize;
            let mut idx = 0;
            for (i, (_, h)) in candidates.iter().enumerate() {
                if pick < *h {
                    idx = i;
                    break;
                }
                pick -= h;
            }
            tiles.push(candidates.swap_remove(idx).0);
        }
        if tiles.is_empty() {
            println!("| {z} | 0 | – | – | – | – | – | – | – | – | – |");
            continue;
        }

        // ---- measure both modes ---------------------------------------
        let mut run = |baseline: bool| -> Result<Vec<Sample>> {
            let topts = TileOptions {
                baseline,
                ..Default::default()
            };
            let mut samples = Vec::new();
            for &(x, y) in &tiles {
                reader.stats.reset();
                let start = Instant::now();
                let tile = render_tile(&mut reader, z, x, y, &topts)?;
                let gen_ms = start.elapsed().as_secs_f64() * 1000.0;
                samples.push(Sample {
                    gen_ms,
                    bytes: reader.stats.bytes(),
                    features: tile.feature_count,
                    source: match tile.source {
                        TileSource::Overview(i) => format!("Overview({i})"),
                        TileSource::BodyImportance => "BodyImportance".into(),
                        TileSource::BodyLive => "BodyLive".into(),
                    },
                });
            }
            Ok(samples)
        };
        let base = run(true)?;
        let fgbo_s = run(false)?;

        let med = |s: &[Sample], f: &dyn Fn(&Sample) -> f64| -> f64 {
            let mut v: Vec<f64> = s.iter().map(f).collect();
            median(&mut v)
        };
        let b_ms = med(&base, &|s| s.gen_ms);
        let f_ms = med(&fgbo_s, &|s| s.gen_ms);
        let b_bytes = med(&base, &|s| s.bytes as f64);
        let f_bytes = med(&fgbo_s, &|s| s.bytes as f64);
        let b_feats = med(&base, &|s| s.features as f64);
        let f_feats = med(&fgbo_s, &|s| s.features as f64);

        // most common FGBO source
        let mut sources: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &fgbo_s {
            *sources.entry(&s.source).or_default() += 1;
        }
        let source = sources
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(s, _)| s.to_string())
            .unwrap_or_default();

        // ratios are meaningless against an (intentionally) empty result
        let speedup = if f_ms >= 0.05 && f_feats > 0.0 {
            format!("{:.1}x", b_ms / f_ms)
        } else {
            "–".into()
        };
        let io_ratio = if f_bytes >= 1.0 && f_feats > 0.0 {
            format!("{:.1}x", b_bytes / f_bytes)
        } else {
            "–".into()
        };
        println!(
            "| {z} | {} | {b_ms:.1} | {f_ms:.1} | {speedup} | {} | {} | {io_ratio} | {} | {} | {source} |",
            tiles.len(),
            fmt_bytes(b_bytes as u64),
            fmt_bytes(f_bytes as u64),
            b_feats as u64,
            f_feats as u64,
        );
    }

    println!("\nmedians over sampled tiles; \"read\" = bytes range-read from the file per tile");
    Ok(())
}
