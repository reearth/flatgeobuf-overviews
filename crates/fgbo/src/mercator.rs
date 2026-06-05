//! Unit Web Mercator projection and tile math.
//!
//! Coordinates are mapped to the unit square `[0, 1] x [0, 1]` with the
//! y axis pointing *down* (y = 0 at the north clamp, y = 1 at the south
//! clamp), matching slippy-map tile space. All FGBO tolerances and
//! importance values are squared distances in this space.

use std::f64::consts::PI;

/// Web Mercator latitude clamp.
pub const MAX_LAT: f64 = 85.051_128_779_806_59;

/// Project lon/lat (degrees) to unit mercator (y down).
pub fn project(lon: f64, lat: f64) -> (f64, f64) {
    let x = lon / 360.0 + 0.5;
    let lat = lat.clamp(-MAX_LAT, MAX_LAT);
    let s = (lat * PI / 180.0).sin();
    let y = 0.5 - 0.25 * ((1.0 + s) / (1.0 - s)).ln() / PI;
    (x, y.clamp(0.0, 1.0))
}

/// Inverse of [`project`].
pub fn unproject(x: f64, y: f64) -> (f64, f64) {
    let lon = (x - 0.5) * 360.0;
    let n = PI * (1.0 - 2.0 * y);
    let lat = (n.sinh()).atan() * 180.0 / PI;
    (lon, lat)
}

/// Tile bounds in unit mercator (y down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileBounds {
    /// West edge (unit mercator x).
    pub left: f64,
    /// North edge (unit mercator y-down; smaller value).
    pub top: f64,
    /// East edge.
    pub right: f64,
    /// South edge (larger value).
    pub bottom: f64,
}

impl TileBounds {
    pub fn new(z: u8, x: u32, y: u32) -> Self {
        let n = (1u64 << z) as f64;
        TileBounds {
            left: x as f64 / n,
            top: y as f64 / n,
            right: (x as f64 + 1.0) / n,
            bottom: (y as f64 + 1.0) / n,
        }
    }

    /// Expand bounds by `frac` of the tile size on each side (tile buffer).
    pub fn buffered(&self, frac: f64) -> Self {
        let bw = (self.right - self.left) * frac;
        let bh = (self.bottom - self.top) * frac;
        TileBounds {
            left: self.left - bw,
            top: self.top - bh,
            right: self.right + bw,
            bottom: self.bottom + bh,
        }
    }

    /// Bounds as lon/lat (west, south, east, north) for bbox queries.
    pub fn to_lonlat(&self) -> (f64, f64, f64, f64) {
        let (w, n) = unproject(self.left, self.top);
        let (e, s) = unproject(self.right, self.bottom);
        (w, s, e, n)
    }
}

/// Squared simplification tolerance for zoom `z`: one pixel at the given
/// tile extent, in unit mercator space.
pub fn sq_tolerance_for_zoom(z: u8, extent: u32) -> f64 {
    let px = 1.0 / ((1u64 << z) as f64 * extent as f64);
    px * px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_roundtrip() {
        for &(lon, lat) in &[(0.0, 0.0), (139.7, 35.6), (-122.4, 37.7), (179.9, -84.0)] {
            let (x, y) = project(lon, lat);
            let (lon2, lat2) = unproject(x, y);
            assert!((lon - lon2).abs() < 1e-9, "lon {lon} vs {lon2}");
            assert!((lat - lat2).abs() < 1e-9, "lat {lat} vs {lat2}");
        }
    }

    #[test]
    fn project_origin() {
        let (x, y) = project(0.0, 0.0);
        assert!((x - 0.5).abs() < 1e-12);
        assert!((y - 0.5).abs() < 1e-12);
    }

    #[test]
    fn tile_bounds_z0() {
        let b = TileBounds::new(0, 0, 0);
        assert_eq!(b.left, 0.0);
        assert_eq!(b.top, 0.0);
        assert_eq!(b.right, 1.0);
        assert_eq!(b.bottom, 1.0);
    }

    #[test]
    fn tile_bounds_nested() {
        // z1 tiles must subdivide z0
        let b = TileBounds::new(1, 1, 0);
        assert_eq!(b.left, 0.5);
        assert_eq!(b.top, 0.0);
        assert_eq!(b.right, 1.0);
        assert_eq!(b.bottom, 0.5);
    }

    #[test]
    fn tolerance_monotone() {
        assert!(sq_tolerance_for_zoom(0, 4096) > sq_tolerance_for_zoom(5, 4096));
        assert!(sq_tolerance_for_zoom(5, 4096) > sq_tolerance_for_zoom(14, 4096));
    }
}
