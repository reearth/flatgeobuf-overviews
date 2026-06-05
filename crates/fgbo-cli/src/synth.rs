//! Synthetic building dataset generator.
//!
//! Produces an indexed FlatGeobuf of N building-footprint polygons
//! clustered around K city centers — the workload where fgb's low-zoom
//! problem actually bites (hundreds of thousands to millions of small
//! features, not a handful of world-sized polygons).
//!
//! Fully deterministic for a given seed (xorshift, no external RNG).

use anyhow::Result;
use flatgeobuf::{ColumnType, FgbCrs, FgbWriter, FgbWriterOptions, GeometryType};
use geo_types::{Coord, Geometry, LineString, Polygon};
use geozero::{ColumnValue, PropertyProcessor};
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

/// xorshift64* — deterministic, dependency-free.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform in [a, b).
    pub fn range(&mut self, a: f64, b: f64) -> f64 {
        a + self.f64() * (b - a)
    }
    /// Approximate standard normal (Irwin–Hall, 12 uniforms).
    pub fn gaussian(&mut self) -> f64 {
        (0..12).map(|_| self.f64()).sum::<f64>() - 6.0
    }
}

pub struct SynthOptions {
    pub features: u64,
    pub cities: u32,
    pub seed: u64,
    /// Region as (west, south, east, north) in degrees.
    pub bbox: (f64, f64, f64, f64),
}

impl Default for SynthOptions {
    fn default() -> Self {
        SynthOptions {
            features: 1_000_000,
            cities: 40,
            seed: 42,
            // Kanto-ish region
            bbox: (138.5, 34.8, 141.0, 36.5),
        }
    }
}

struct City {
    lon: f64,
    lat: f64,
    /// Spread in degrees.
    sigma: f64,
    /// Sampling weight (population).
    weight: f64,
}

/// Building footprint: a rotated rectangle, sometimes with an L-notch.
fn footprint(rng: &mut Rng, lon: f64, lat: f64, w_m: f64, h_m: f64) -> Polygon<f64> {
    let deg_per_m_lat = 1.0 / 111_320.0;
    let deg_per_m_lon = 1.0 / (111_320.0 * lat.to_radians().cos());
    let theta = rng.range(0.0, std::f64::consts::PI);
    let (sin, cos) = theta.sin_cos();

    // local meters -> degrees, rotated around (lon, lat)
    let pt = |x_m: f64, y_m: f64| Coord {
        x: lon + (x_m * cos - y_m * sin) * deg_per_m_lon,
        y: lat + (x_m * sin + y_m * cos) * deg_per_m_lat,
    };

    let (hw, hh) = (w_m / 2.0, h_m / 2.0);
    let mut ring: Vec<Coord<f64>> = if rng.f64() < 0.3 {
        // L-shape: rectangle minus one quadrant
        let (nx, ny) = (hw * rng.range(0.3, 0.7), hh * rng.range(0.3, 0.7));
        vec![
            pt(-hw, -hh),
            pt(hw, -hh),
            pt(hw, ny),
            pt(nx, ny),
            pt(nx, hh),
            pt(-hw, hh),
        ]
    } else {
        vec![pt(-hw, -hh), pt(hw, -hh), pt(hw, hh), pt(-hw, hh)]
    };
    ring.push(ring[0]);
    Polygon::new(LineString(ring), vec![])
}

pub fn synth(output: &Path, opts: &SynthOptions) -> Result<u64> {
    let mut rng = Rng::new(opts.seed);
    let (west, south, east, north) = opts.bbox;

    // city centers: a few metropolises, the rest towns
    let cities: Vec<City> = (0..opts.cities)
        .map(|i| {
            let metro = i < opts.cities / 8 + 1;
            City {
                lon: rng.range(west, east),
                lat: rng.range(south, north),
                sigma: if metro {
                    rng.range(0.04, 0.10)
                } else {
                    rng.range(0.008, 0.03)
                },
                weight: if metro {
                    rng.range(8.0, 20.0)
                } else {
                    rng.range(0.5, 2.0)
                },
            }
        })
        .collect();
    let total_weight: f64 = cities.iter().map(|c| c.weight).sum();

    let fgb_opts = FgbWriterOptions {
        write_index: true,
        detect_type: false,
        promote_to_multi: false,
        crs: FgbCrs {
            code: 4326,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut writer = FgbWriter::create_with_options("buildings", GeometryType::Polygon, fgb_opts)?;
    writer.add_column("id", ColumnType::Long, |_, _| {});
    writer.add_column("height", ColumnType::Double, |_, _| {});
    writer.add_column("kind", ColumnType::String, |_, _| {});

    for id in 0..opts.features {
        // pick a city by weight
        let mut pick = rng.f64() * total_weight;
        let mut city = &cities[0];
        for c in &cities {
            if pick < c.weight {
                city = c;
                break;
            }
            pick -= c.weight;
        }

        let lon = (city.lon + rng.gaussian() * city.sigma).clamp(west, east);
        let lat = (city.lat + rng.gaussian() * city.sigma * 0.8).clamp(south, north);

        // size distribution: mostly houses, some mid, few big-box
        let r = rng.f64();
        let (w_m, h_m, kind, height) = if r < 0.90 {
            (
                rng.range(8.0, 25.0),
                rng.range(8.0, 25.0),
                "house",
                rng.range(3.0, 12.0),
            )
        } else if r < 0.99 {
            (
                rng.range(40.0, 100.0),
                rng.range(25.0, 60.0),
                "mid",
                rng.range(10.0, 60.0),
            )
        } else {
            (
                rng.range(100.0, 300.0),
                rng.range(60.0, 150.0),
                "large",
                rng.range(15.0, 250.0),
            )
        };

        let geom = Geometry::Polygon(footprint(&mut rng, lon, lat, w_m, h_m));
        writer.add_feature_geom(geom, |feat| {
            let _ = feat.property(0, "id", &ColumnValue::Long(id as i64));
            let _ = feat.property(1, "height", &ColumnValue::Double(height));
            let _ = feat.property(2, "kind", &ColumnValue::String(kind));
        })?;
    }

    let mut out = BufWriter::new(File::create(output)?);
    writer.write(&mut out)?;
    Ok(opts.features)
}
