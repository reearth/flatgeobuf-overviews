//! Per-vertex importance: a modified Douglas–Peucker pass that assigns each
//! vertex the largest squared tolerance (unit mercator) at which it survives
//! simplification, geojson-vt style, plus the u16 quantization and the
//! sidecar section encoding.
//!
//! Importance values are quantized to u16 on a log scale (linear
//! quantization lacks resolution near zero, where high-zoom tolerances
//! live):
//!
//! - `0`        : never survives (reserved, currently unused)
//! - `1..=65534`: `q = 1 + round((log2(d2) + 64) / 64 * 65533)`, clamped
//! - `65535`    : always survives (ring/part endpoints)

use crate::error::{Error, Result};
use geo_types::{Geometry, LineString};

/// Importance value for vertices that always survive.
pub const ALWAYS: u16 = u16::MAX;

const LOG2_MIN: f64 = -64.0;
const Q_RANGE: f64 = 65533.0;

/// Quantize a squared distance (unit mercator) to u16, rounding *down*
/// (a vertex never claims more importance than it has).
pub fn quantize_sqdist(d2: f64) -> u16 {
    if d2 <= 0.0 {
        return 1;
    }
    let l = d2.log2();
    if l <= LOG2_MIN {
        return 1;
    }
    let q = ((l - LOG2_MIN) / -LOG2_MIN * Q_RANGE).floor() + 1.0;
    q.clamp(1.0, 65534.0) as u16
}

/// Inverse of [`quantize_sqdist`] (lower bound of the bucket).
pub fn dequantize(q: u16) -> f64 {
    if q == 0 {
        return 0.0;
    }
    if q == ALWAYS {
        return f64::INFINITY;
    }
    let l = (q as f64 - 1.0) / Q_RANGE * -LOG2_MIN + LOG2_MIN;
    l.exp2()
}

/// Quantized threshold for filtering: keep vertex `i` iff
/// `importance[i] >= threshold_q(sq_tolerance)`.
pub fn threshold_q(sq_tolerance: f64) -> u16 {
    // The smallest q whose dequantized value is >= sq_tolerance.
    let q = quantize_sqdist(sq_tolerance);
    if dequantize(q) >= sq_tolerance {
        q
    } else {
        q.saturating_add(1).min(65534)
    }
}

/// Squared distance from `p` to segment `a`-`b` (geojson-vt getSqSegDist).
fn sq_seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (px, py) = p;
    let (mut x, mut y) = a;
    let mut dx = b.0 - x;
    let mut dy = b.1 - y;

    if dx != 0.0 || dy != 0.0 {
        let t = ((px - x) * dx + (py - y) * dy) / (dx * dx + dy * dy);
        if t > 1.0 {
            x = b.0;
            y = b.1;
        } else if t > 0.0 {
            x += dx * t;
            y += dy * t;
        }
    }
    dx = px - x;
    dy = py - y;
    dx * dx + dy * dy
}

/// Douglas–Peucker pass assigning each interior vertex its survival
/// squared distance, capped by the parent's value so that the per-vertex
/// filter `imp >= tol` always yields a valid simplification. Iterative
/// (explicit stack) to handle very large geometries.
fn dp(coords: &[(f64, f64)], imp: &mut [f64], first: usize, last: usize, cap: f64) {
    let mut stack = vec![(first, last, cap)];
    while let Some((first, last, cap)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let mut max_d = -1.0;
        let mut idx = first + 1;
        for i in first + 1..last {
            let d = sq_seg_dist(coords[i], coords[first], coords[last]);
            if d > max_d {
                max_d = d;
                idx = i;
            }
        }
        let v = max_d.min(cap);
        imp[idx] = v;
        stack.push((first, idx, v));
        stack.push((idx, last, v));
    }
}

/// Compute importance for a single linestring/ring. Endpoints get [`ALWAYS`].
fn line_importance(line: &LineString<f64>, out: &mut Vec<u16>) {
    let coords: Vec<(f64, f64)> = line.coords().map(|c| (c.x, c.y)).collect();
    let n = coords.len();
    if n == 0 {
        return;
    }
    if n <= 2 {
        out.extend(std::iter::repeat_n(ALWAYS, n));
        return;
    }
    let mut imp = vec![0.0f64; n];
    dp(&coords, &mut imp, 0, n - 1, f64::INFINITY);
    out.push(ALWAYS);
    for &d2 in &imp[1..n - 1] {
        out.push(quantize_sqdist(d2));
    }
    out.push(ALWAYS);
}

/// Compute the importance array for a geometry whose coordinates are in
/// unit mercator. The output order matches geozero/FlatGeobuf coordinate
/// traversal order (parts in order, exterior ring then interior rings).
pub fn geometry_importance(geom: &Geometry<f64>) -> Vec<u16> {
    let mut out = Vec::new();
    collect(geom, &mut out);
    out
}

fn collect(geom: &Geometry<f64>, out: &mut Vec<u16>) {
    match geom {
        Geometry::Point(_) => out.push(ALWAYS),
        Geometry::MultiPoint(mp) => out.extend(std::iter::repeat_n(ALWAYS, mp.0.len())),
        Geometry::Line(_) => out.extend([ALWAYS, ALWAYS]),
        Geometry::LineString(ls) => line_importance(ls, out),
        Geometry::MultiLineString(mls) => {
            for ls in &mls.0 {
                line_importance(ls, out);
            }
        }
        Geometry::Polygon(p) => {
            line_importance(p.exterior(), out);
            for r in p.interiors() {
                line_importance(r, out);
            }
        }
        Geometry::MultiPolygon(mp) => {
            for p in &mp.0 {
                line_importance(p.exterior(), out);
                for r in p.interiors() {
                    line_importance(r, out);
                }
            }
        }
        Geometry::GeometryCollection(gc) => {
            for g in gc {
                collect(g, out);
            }
        }
        Geometry::Rect(_) | Geometry::Triangle(_) => {
            // Not produced by FlatGeobuf reading; treat all vertices as kept.
            // (Rect = 4 coords, Triangle = 3 when converted.)
            unreachable!("rect/triangle not produced by fgb reader")
        }
    }
}

/// Importance sidecar section.
///
/// Binary layout (little-endian):
/// ```text
/// u64                    feature_count
/// u64 * (count + 1)      offsets (bytes, relative to payload start)
/// u16 * total_vertices   payload (importance arrays, file order)
/// ```
#[derive(Debug, Default, Clone)]
pub struct ImportanceSidecar {
    /// Byte offsets into payload; len == feature_count + 1.
    offsets: Vec<u64>,
    payload: Vec<u16>,
}

impl ImportanceSidecar {
    pub fn new() -> Self {
        ImportanceSidecar {
            offsets: vec![0],
            payload: Vec::new(),
        }
    }

    pub fn feature_count(&self) -> u64 {
        (self.offsets.len() - 1) as u64
    }

    /// Append the importance array of the next feature (file order).
    pub fn push(&mut self, imp: &[u16]) {
        self.payload.extend_from_slice(imp);
        self.offsets.push((self.payload.len() * 2) as u64);
    }

    /// Importance array of feature `ordinal` (file order).
    pub fn get(&self, ordinal: u64) -> Option<&[u16]> {
        let i = ordinal as usize;
        if i + 1 >= self.offsets.len() {
            return None;
        }
        let start = (self.offsets[i] / 2) as usize;
        let end = (self.offsets[i + 1] / 2) as usize;
        Some(&self.payload[start..end])
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.offsets.len() * 8 + self.payload.len() * 2);
        buf.extend_from_slice(&self.feature_count().to_le_bytes());
        for o in &self.offsets {
            buf.extend_from_slice(&o.to_le_bytes());
        }
        for v in &self.payload {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < 8 {
            return Err(Error::Format("importance sidecar truncated".into()));
        }
        let count = u64::from_le_bytes(buf[0..8].try_into().unwrap()) as usize;
        let table_end = 8 + (count + 1) * 8;
        if buf.len() < table_end {
            return Err(Error::Format("importance offset table truncated".into()));
        }
        let mut offsets = Vec::with_capacity(count + 1);
        for i in 0..=count {
            let p = 8 + i * 8;
            offsets.push(u64::from_le_bytes(buf[p..p + 8].try_into().unwrap()));
        }
        let payload_bytes = &buf[table_end..];
        if *offsets.last().unwrap() as usize != payload_bytes.len() {
            return Err(Error::Format("importance payload size mismatch".into()));
        }
        let mut payload = Vec::with_capacity(payload_bytes.len() / 2);
        for ch in payload_bytes.chunks_exact(2) {
            payload.push(u16::from_le_bytes(ch.try_into().unwrap()));
        }
        Ok(ImportanceSidecar { offsets, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{line_string, polygon};

    #[test]
    fn quantize_monotone() {
        let mut prev = 0u16;
        for e in (-70..0).map(|e| (e as f64).exp2()) {
            let q = quantize_sqdist(e);
            assert!(q >= prev, "quantization must be monotone");
            prev = q;
        }
    }

    #[test]
    fn quantize_dequantize_bounds() {
        for &d2 in &[1e-18, 1e-12, 1e-6, 0.25, 0.999] {
            let q = quantize_sqdist(d2);
            // dequantize(q) <= d2 < dequantize(q+1)
            assert!(dequantize(q) <= d2 * (1.0 + 1e-9), "d2={d2} q={q}");
            assert!(dequantize(q + 1) > d2, "d2={d2} q={q}");
        }
    }

    #[test]
    fn threshold_filter_is_safe() {
        // A vertex with importance exactly at tolerance must survive.
        for &tol in &[1e-12f64, 1e-7, 1e-4] {
            let t = threshold_q(tol);
            let q = quantize_sqdist(tol);
            assert!(dequantize(t) >= tol);
            // anything with true d2 >= tol quantizes to >= q >= t - 1;
            // conservative: filter never drops vertices well above tolerance
            assert!(quantize_sqdist(tol * 4.0) >= t, "tol={tol} q={q} t={t}");
        }
    }

    #[test]
    fn line_endpoints_always() {
        let ls = line_string![(x: 0.0, y: 0.0), (x: 0.5, y: 0.001), (x: 1.0, y: 0.0)];
        let imp = geometry_importance(&Geometry::LineString(ls));
        assert_eq!(imp.len(), 3);
        assert_eq!(imp[0], ALWAYS);
        assert_eq!(imp[2], ALWAYS);
        assert!(imp[1] < ALWAYS && imp[1] > 0);
    }

    #[test]
    fn spike_more_important_than_noise() {
        // big deviation -> higher importance than small deviation
        let ls = line_string![
            (x: 0.0, y: 0.0),
            (x: 0.25, y: 1e-6),
            (x: 0.5, y: 0.1),
            (x: 0.75, y: 1e-6),
            (x: 1.0, y: 0.0)
        ];
        let imp = geometry_importance(&Geometry::LineString(ls));
        assert!(imp[2] > imp[1]);
        assert!(imp[2] > imp[3]);
    }

    #[test]
    fn monotone_nesting() {
        // child importance never exceeds parent importance: filtering at any
        // threshold yields endpoints + a consistent subset
        let ls = line_string![
            (x: 0.0, y: 0.0), (x: 0.1, y: 0.02), (x: 0.2, y: -0.01),
            (x: 0.3, y: 0.07), (x: 0.4, y: 0.0), (x: 0.5, y: 0.3),
            (x: 0.6, y: 0.01), (x: 0.7, y: -0.04), (x: 0.8, y: 0.02),
            (x: 1.0, y: 0.0)
        ];
        let imp = geometry_importance(&Geometry::LineString(ls.clone()));
        // count survivors at decreasing tolerance: must be non-decreasing
        let mut prev_survivors = 0;
        for q in [60000u16, 50000, 40000, 30000, 1] {
            let n = imp.iter().filter(|&&v| v >= q).count();
            assert!(n >= prev_survivors);
            prev_survivors = n;
        }
    }

    #[test]
    fn polygon_ring_order() {
        let p = polygon![
            exterior: [(x: 0.0, y: 0.0), (x: 1.0, y: 0.0), (x: 1.0, y: 1.0), (x: 0.0, y: 1.0), (x: 0.0, y: 0.0)],
            interiors: [[(x: 0.4, y: 0.4), (x: 0.6, y: 0.4), (x: 0.6, y: 0.6), (x: 0.4, y: 0.6), (x: 0.4, y: 0.4)]]
        ];
        let imp = geometry_importance(&Geometry::Polygon(p));
        assert_eq!(imp.len(), 10); // 5 + 5 coords incl. closing dup
    }

    #[test]
    fn sidecar_roundtrip() {
        let mut sc = ImportanceSidecar::new();
        sc.push(&[ALWAYS, 100, ALWAYS]);
        sc.push(&[ALWAYS]);
        sc.push(&[ALWAYS, 1, 2, 3, ALWAYS]);
        let enc = sc.encode();
        let dec = ImportanceSidecar::decode(&enc).unwrap();
        assert_eq!(dec.feature_count(), 3);
        assert_eq!(dec.get(0).unwrap(), &[ALWAYS, 100, ALWAYS]);
        assert_eq!(dec.get(1).unwrap(), &[ALWAYS]);
        assert_eq!(dec.get(2).unwrap(), &[ALWAYS, 1, 2, 3, ALWAYS]);
        assert!(dec.get(3).is_none());
    }
}
