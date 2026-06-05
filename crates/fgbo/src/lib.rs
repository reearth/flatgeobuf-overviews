//! # FGBO — FlatGeobuf Overviews
//!
//! Reference implementation of the FGBO format (Profile A): a
//! scale-optimized, fully FlatGeobuf-compatible extension that appends
//! three kinds of sections after the fgb data section:
//!
//! - **importance sidecar** — per-vertex "largest tolerance at which this
//!   vertex survives" (geojson-vt style), making per-request simplification
//!   an O(n) filter;
//! - **overview levels** — pre-simplified, pre-thinned feature sets stored
//!   as embedded mini-fgb (index + data), solving low-zoom I/O;
//! - **segments** — large features pre-clipped to a zbase grid, stored as
//!   an embedded mini-fgb of fragments, solving high-zoom big-feature I/O.
//!
//! Discovery is via a fixed 32-byte footer; readers that only understand
//! fgb read the body as a normal FlatGeobuf file.

pub mod clip;
pub mod decoder;
pub mod encoder;
pub mod error;
pub mod fgb;
pub mod format;
#[cfg(feature = "http")]
pub mod http;
pub mod importance;
pub mod mercator;
pub mod simplify;
pub mod tile;

pub use decoder::{FgboReader, TileFeature, TileQuery, TileSource};
pub use encoder::{encode_file, EncodeOptions, EncodeReport, LevelSpec};
pub use error::{Error, Result};
pub use format::{Directory, Footer};
#[cfg(feature = "http")]
pub use http::HttpRangeReader;
pub use tile::{render_tile, RenderedTile, TileOptions};
