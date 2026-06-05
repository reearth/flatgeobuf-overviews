//! MVT tile rendering: query → clip to tile → encode protobuf.

use crate::clip::{clip_geometry, Rect};
use crate::decoder::{FgboReader, TileQuery, TileSource};
use crate::error::Result;
use crate::mercator::{project, TileBounds};
use geo::MapCoords;
use geo_types::Geometry;
use geozero::mvt::{Message, MvtWriter, Tile};
use geozero::{FeatureProcessor, GeozeroGeometry, PropertyProcessor};
use std::io::{Read, Seek};

/// Tile rendering options.
#[derive(Debug, Clone)]
pub struct TileOptions {
    /// MVT extent (coordinate units per tile side).
    pub extent: u32,
    /// Tile buffer in extent units (clip margin).
    pub buffer: u32,
    /// Layer name override; defaults to the fgb layer name.
    pub layer_name: Option<String>,
    /// Use the plain-fgb baseline path (ignore FGBO sections).
    pub baseline: bool,
}

impl Default for TileOptions {
    fn default() -> Self {
        TileOptions {
            extent: 4096,
            buffer: 64,
            layer_name: None,
            baseline: false,
        }
    }
}

/// Tile rendering result.
#[derive(Debug)]
pub struct RenderedTile {
    /// MVT protobuf bytes (uncompressed).
    pub data: Vec<u8>,
    /// Number of features written.
    pub feature_count: usize,
    /// Which FGBO path served the query.
    pub source: TileSource,
}

/// Render tile z/x/y from an FGBO (or plain fgb) reader as MVT.
pub fn render_tile<R: Read + Seek>(
    reader: &mut FgboReader<R>,
    z: u8,
    x: u32,
    y: u32,
    opts: &TileOptions,
) -> Result<RenderedTile> {
    let buffer_frac = opts.buffer as f64 / opts.extent as f64;
    let query: TileQuery = if opts.baseline {
        reader.query_tile_baseline(z, x, y, opts.extent, buffer_frac)?
    } else {
        reader.query_tile(z, x, y, opts.extent, buffer_frac)?
    };

    let layer_name = opts
        .layer_name
        .clone()
        .unwrap_or_else(|| reader.layer_name());

    let bounds = TileBounds::new(z, x, y);
    let clip_bounds = bounds.buffered(buffer_frac);
    let clip_rect = Rect::new(
        clip_bounds.left,
        clip_bounds.top,
        clip_bounds.right,
        clip_bounds.bottom,
    );

    // MvtWriter scales (x - left) and flips y internally; passing y-down
    // bounds with bottom = south (larger value) yields a negative
    // multiplier that works out to correct tile coordinates.
    let mut mvt = MvtWriter::new(
        opts.extent,
        bounds.left,
        bounds.bottom,
        bounds.right,
        bounds.top,
    )?;

    let mut count: u64 = 0;
    for f in &query.features {
        // project to unit mercator (y down) and clip to the buffered tile
        let merc: Geometry<f64> = f.geometry.map_coords(|c| {
            let (mx, my) = project(c.x, c.y);
            geo_types::Coord { x: mx, y: my }
        });
        let Some(clipped) = clip_geometry(&merc, clip_rect) else {
            continue;
        };

        mvt.feature_begin(count)?;
        mvt.geometry_begin()?;
        clipped.process_geom(&mut mvt)?;
        mvt.geometry_end()?;
        for (i, (name, value)) in f.properties.iter().enumerate() {
            // binary properties are not representable in MVT; skip
            if matches!(value, crate::fgb::PropValue::Binary(_)) {
                continue;
            }
            mvt.property(i, name, &value.as_column_value())?;
        }
        mvt.feature_end(count)?;
        count += 1;
    }

    let layer = mvt.layer(&layer_name);
    let t = Tile {
        layers: vec![layer],
    };
    Ok(RenderedTile {
        data: t.encode_to_vec(),
        feature_count: count as usize,
        source: query.source,
    })
}
