//! fgbo — CLI for FlatGeobuf Overviews.

mod serve;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fgbo::{encode_file, render_tile, EncodeOptions, FgboReader, LevelSpec, TileOptions};
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "fgbo", version, about = "FlatGeobuf Overviews (FGBO) tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build an FGBO file from a valid FlatGeobuf file
    Build {
        /// Input .fgb (must have spatial index and explicit feature count)
        input: PathBuf,
        /// Output path
        #[arg(short, long)]
        output: PathBuf,
        /// Overview zoom levels, e.g. "0-4,5-8,9-11"
        #[arg(long, default_value = "0-4,5-8,9-11")]
        levels: String,
        /// Vertex count threshold for segmenting large features (0 = off)
        #[arg(long, default_value_t = 16384)]
        vmax: u32,
        /// Clipping grid zoom for segments
        #[arg(long, default_value_t = 12)]
        zbase: u8,
        /// Assumed MVT extent for tolerance derivation
        #[arg(long, default_value_t = 4096)]
        extent: u32,
        /// Feature thinning threshold in extent units (16 = 1 screen px)
        #[arg(long, default_value_t = 16.0)]
        drop_small: f64,
    },
    /// Show fgb header and FGBO directory information
    Info { file: PathBuf },
    /// Render one MVT tile
    Tile {
        file: PathBuf,
        z: u8,
        x: u32,
        y: u32,
        /// Output path (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Ignore FGBO sections (plain fgb baseline)
        #[arg(long)]
        baseline: bool,
        /// Print I/O statistics to stderr
        #[arg(long)]
        stats: bool,
    },
    /// Serve tiles over HTTP with a MapLibre debug page
    Serve {
        file: PathBuf,
        /// Listen address
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
    },
    /// Rebuild any fgb/GeoJSON input as a clean indexed FlatGeobuf
    /// (FGBO build input requirements: index + explicit feature count)
    Convert {
        /// Input .geojson or .fgb
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Layer name for the output
        #[arg(long, default_value = "layer")]
        name: String,
    },
}

fn parse_levels(s: &str) -> Result<Vec<LevelSpec>> {
    let mut levels = Vec::new();
    for part in s.split(',') {
        let (a, b) = part
            .trim()
            .split_once('-')
            .with_context(|| format!("invalid level spec: {part}"))?;
        levels.push(LevelSpec {
            min_zoom: a.parse()?,
            max_zoom: b.parse()?,
        });
    }
    Ok(levels)
}

fn human(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    match Cli::parse().command {
        Command::Build {
            input,
            output,
            levels,
            vmax,
            zbase,
            extent,
            drop_small,
        } => {
            let opts = EncodeOptions {
                levels: parse_levels(&levels)?,
                extent,
                v_max: vmax,
                zbase,
                drop_small_units: drop_small,
            };
            let report = encode_file(&input, &output, &opts)?;
            println!("FGBO written: {}", output.display());
            println!("  features        : {}", report.features_count);
            println!(
                "  body            : {} (plain fgb, readable by any fgb reader)",
                human(report.body_size)
            );
            println!("  importance      : {}", human(report.importance_size));
            for (level, size, count) in &report.overview_sizes {
                println!(
                    "  overview z{:>2}-{:<2} : {} ({count} features)",
                    level.min_zoom,
                    level.max_zoom,
                    human(*size),
                );
            }
            if report.segments_size > 0 {
                println!(
                    "  segments        : {} ({} fragments from {} features)",
                    human(report.segments_size),
                    report.fragment_count,
                    report.segmented_features
                );
            }
            println!(
                "  total           : {} (+{:.1}% over body)",
                human(report.total_size),
                (report.total_size as f64 / report.body_size as f64 - 1.0) * 100.0
            );
        }

        Command::Info { file } => {
            let reader = FgboReader::open_file(&file)?;
            let body = reader.body();
            let header = body.header();
            println!("file            : {}", file.display());
            println!("size            : {}", human(reader.file_len()));
            println!("layer           : {}", header.name().unwrap_or(""));
            println!("geometry type   : {:?}", header.geometry_type());
            println!("features        : {}", header.features_count());
            println!(
                "index           : {}",
                if body.has_index() {
                    format!("packed R-tree, node size {}", header.index_node_size())
                } else {
                    "none".into()
                }
            );
            let cols: Vec<String> = fgbo::fgb::column_names(&header)
                .iter()
                .map(|(n, t)| format!("{n} ({t:?})"))
                .collect();
            println!("columns         : {}", cols.join(", "));

            match reader.directory() {
                None => println!("FGBO            : no (plain FlatGeobuf)"),
                Some(dir) => {
                    println!("FGBO            : yes ({})", dir.build_info);
                    if let Some(imp) = &dir.importance {
                        println!(
                            "  importance    : {} ({} features)",
                            human(imp.size),
                            imp.feature_count
                        );
                    }
                    for o in &dir.overviews {
                        println!(
                            "  overview      : z{}-{} {} ({} features, tol_q={})",
                            o.min_zoom,
                            o.max_zoom,
                            human(o.size),
                            o.feature_count,
                            o.tolerance_q
                        );
                    }
                    if let Some(s) = &dir.segments {
                        println!(
                            "  segments      : zbase={} vmax={} {} ({} fragments, {} features)",
                            s.zbase,
                            s.v_max,
                            human(s.size),
                            s.fragment_count,
                            s.segmented_ordinals.len()
                        );
                    }
                }
            }
        }

        Command::Tile {
            file,
            z,
            x,
            y,
            output,
            baseline,
            stats,
        } => {
            let mut reader = FgboReader::open_file(&file)?;
            reader.stats.reset();
            let opts = TileOptions {
                baseline,
                ..Default::default()
            };
            let tile = render_tile(&mut reader, z, x, y, &opts)?;
            if stats {
                eprintln!(
                    "tile {z}/{x}/{y}: {} features, {} bytes MVT, source {:?}, {} range reads, {} read",
                    tile.feature_count,
                    tile.data.len(),
                    tile.source,
                    reader.stats.requests(),
                    human(reader.stats.bytes()),
                );
            }
            match output {
                Some(p) => std::fs::write(p, &tile.data)?,
                None => std::io::stdout().write_all(&tile.data)?,
            }
        }

        Command::Serve { file, addr } => {
            serve::run(file, &addr)?;
        }

        Command::Convert {
            input,
            output,
            name,
        } => {
            convert(&input, &output, &name)?;
            println!("written: {}", output.display());
        }
    }
    Ok(())
}

/// Rebuild input as a clean, indexed fgb with explicit feature count.
fn convert(input: &PathBuf, output: &PathBuf, name: &str) -> Result<()> {
    use flatgeobuf::{FgbCrs, FgbReader, FgbWriter, FgbWriterOptions, GeometryType};
    use geozero::GeozeroDatasource;
    use std::fs::File;
    use std::io::{BufReader, BufWriter};

    let options = FgbWriterOptions {
        write_index: true,
        detect_type: false,
        promote_to_multi: false,
        crs: FgbCrs {
            code: 4326,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut writer = FgbWriter::create_with_options(name, GeometryType::Unknown, options)?;

    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "json" | "geojson" => {
            let mut fin = BufReader::new(File::open(input)?);
            let mut reader = geozero::geojson::GeoJsonReader(&mut fin);
            reader.process(&mut writer)?;
        }
        "fgb" => {
            let fin = BufReader::new(File::open(input)?);
            let mut fgb = FgbReader::open(fin)?.select_all_seq()?;
            fgb.process_features(&mut writer)?;
        }
        other => bail!("unsupported input extension: {other:?} (use .geojson or .fgb)"),
    }

    let mut out = BufWriter::new(File::create(output)?);
    writer.write(&mut out)?;
    Ok(())
}
