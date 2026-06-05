//! Web Mercator projection and tile math.
//!
//! Two coordinate spaces are used:
//!
//! - **Q32 fixed-point unit mercator** (`[0, 2^32)`, y down): the space in
//!   which importance values and tolerances are computed and quantized.
//!   Coordinates snap to a 32-bit grid before any distance math, so
//!   builds are deterministic across platforms; the exact projection and
//!   tolerance formulas are pinned by fixtures (a bit-compatibility
//!   contract for sidecar interoperability).
//! - **f64 unit square** (`[0, 1]`, y down): used only at render time for
//!   tile clipping and MVT coordinate scaling.

use std::f64::consts::PI;

/// Number of bits in the Q32 fixed-point grid.
pub const Q_BITS: u32 = 32;

/// World span in Q32 units: `2^32` (u64 because it does not fit u32).
pub const Q_SPAN: u64 = 1u64 << Q_BITS;

/// Web Mercator latitude clamp.
pub const MAX_LAT: f64 = 85.051_128_779_806_59;

/// Project lon/lat (WGS84 degrees) to Q32 unit mercator (y down).
#[inline]
pub fn lonlat_to_q32(lon: f64, lat: f64) -> (u32, u32) {
    let x = (lon + 180.0) / 360.0;
    let lat_rad = lat.clamp(-89.999_999, 89.999_999).to_radians();
    // Normalized web-mercator y: (1 - asinh(tan φ)/π) / 2
    let y = (1.0 - (lat_rad.tan().asinh()) / PI) / 2.0;
    (to_q(x), to_q(y))
}

/// Map a normalized `[0,1]` coordinate to a Q32 integer, clamping
/// out-of-range input.
#[inline]
pub fn to_q(v: f64) -> u32 {
    let span = Q_SPAN as f64;
    let q = (v.clamp(0.0, 1.0) * span).floor();
    if q >= span {
        u32::MAX
    } else {
        q as u32
    }
}

/// Inverse-project Q32 coordinates (as f64) to lon/lat degrees.
#[inline]
pub fn q32_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let span = Q_SPAN as f64;
    let lon = (x / span) * 360.0 - 180.0;
    let lat = (PI * (1.0 - 2.0 * (y / span))).sinh().atan().to_degrees();
    (lon, lat)
}

/// Squared simplification tolerance for zoom `z` in Q32 units: one
/// extent-unit at zoom `z` spans `(2^32 >> z) / extent` Q32 units.
#[inline]
pub fn sq_tolerance_for_zoom(z: u8, extent: u32) -> f64 {
    let tile_width = (Q_SPAN >> (z as u32).min(Q_BITS)) as f64;
    let px = tile_width / (extent.max(1)) as f64;
    px * px
}

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

    // Pinned fixtures — the bit-compatibility contract for the Q32
    // projection. Changing these values breaks sidecar interoperability.
    #[test]
    fn q32_projection_fixtures() {
        assert_eq!(lonlat_to_q32(0.0, 0.0), (2147483648, 2147483648));
        assert_eq!(lonlat_to_q32(139.7, 35.6), (3814169568, 1692456229));
        assert_eq!(lonlat_to_q32(-122.4, 37.7), (687194767, 1661224733));
        assert_eq!(lonlat_to_q32(130.0, 33.0), (3698444060, 1730011504));
    }

    #[test]
    fn tolerance_fixtures() {
        assert_eq!(sq_tolerance_for_zoom(0, 4096), 1.099511627776e12);
        assert_eq!(sq_tolerance_for_zoom(4, 4096), 4.294967296e9);
        assert_eq!(sq_tolerance_for_zoom(12, 4096), 6.5536e4);
        assert_eq!(sq_tolerance_for_zoom(14, 4096), 4.096e3);
    }

    #[test]
    fn q32_roundtrip() {
        for &(lon, lat) in &[(139.7, 35.6), (-122.4, 37.7), (130.0, 33.0)] {
            let (x, y) = lonlat_to_q32(lon, lat);
            let (lon2, lat2) = q32_to_lonlat(x as f64, y as f64);
            assert!((lon - lon2).abs() < 1e-6);
            assert!((lat - lat2).abs() < 1e-6);
        }
    }
}
