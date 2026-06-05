//! Per-vertex importance: a modified Douglas–Peucker pass that assigns each
//! vertex the largest squared tolerance (Q32 unit mercator) at which it
//! survives simplification, geojson-vt style, plus the u16 quantization
//! and the sidecar section encoding.
//!
//! Quantization and DP semantics:
//!
//! - distances are squared Q32 distances, log-quantized:
//!   `q = clamp(round(ln(1+d²) / ln(1+(2^32)²) × 65534), 1, 65534)`
//! - `1`     : default for interior vertices never chosen by DP
//!   (collinear/duplicate — droppable first)
//! - `65535` : always survives (ring/part endpoints)
//! - filtering keeps vertex `i` iff `imp[i] >= quantize(tol²(z))`

use crate::error::{Error, Result};
use crate::mercator::Q_SPAN;
use geo_types::{Geometry, LineString};

/// Importance value for vertices that always survive.
pub const ALWAYS: u16 = u16::MAX;

/// Largest possible squared distance in Q32 space: `(2^32)²`.
#[inline]
fn max_sqdist() -> f64 {
    let s = Q_SPAN as f64;
    s * s
}

/// Log-quantize a Q32 squared distance into `[1, 65534]` (monotonic).
/// Log scale (not linear) is required for resolution at small distances.
#[inline]
pub fn quantize_sqdist(d2: f64) -> u16 {
    let l = d2.max(0.0).ln_1p();
    let max_l = max_sqdist().ln_1p();
    let q = (l / max_l * (ALWAYS as f64 - 1.0)).round();
    q.clamp(1.0, (ALWAYS - 1) as f64) as u16
}

/// Approximate inverse of [`quantize_sqdist`] (for tests/diagnostics).
pub fn dequantize(q: u16) -> f64 {
    if q == ALWAYS {
        return f64::INFINITY;
    }
    let max_l = max_sqdist().ln_1p();
    (q as f64 / (ALWAYS as f64 - 1.0) * max_l).exp_m1()
}

/// Quantized threshold for filtering: keep vertex `i` iff
/// `importance[i] >= threshold_q(sq_tolerance)` — the tolerance is
/// quantized with the same function as the distances, so the comparison
/// is consistent by construction.
#[inline]
pub fn threshold_q(sq_tolerance: f64) -> u16 {
    quantize_sqdist(sq_tolerance)
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

/// Douglas–Peucker pass assigning each chosen interior vertex its
/// quantized max deviation (geojson-vt "z value" method): strict `>`
/// tie-break, recursion stops when the max deviation is exactly 0,
/// unchosen vertices keep the default `1`. Iterative (explicit stack) to
/// handle very large geometries without blowing the call stack.
fn dp(coords: &[(f64, f64)], imp: &mut [u16], first: usize, last: usize) {
    let mut stack = vec![(first, last)];
    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let mut max_d = 0.0f64;
        let mut idx = 0usize;
        for i in first + 1..last {
            let d = sq_seg_dist(coords[i], coords[first], coords[last]);
            if d > max_d {
                idx = i;
                max_d = d;
            }
        }
        if max_d > 0.0 {
            imp[idx] = quantize_sqdist(max_d);
            stack.push((first, idx));
            stack.push((idx, last));
        }
    }
}

/// Compute importance for a single linestring/ring. Endpoints get
/// [`ALWAYS`], unchosen interior vertices stay at `1`.
fn line_importance(line: &LineString<f64>, out: &mut Vec<u16>) {
    let coords: Vec<(f64, f64)> = line.coords().map(|c| (c.x, c.y)).collect();
    let n = coords.len();
    if n == 0 {
        return;
    }
    let start = out.len();
    out.extend(std::iter::repeat_n(1u16, n));
    out[start] = ALWAYS;
    out[start + n - 1] = ALWAYS;
    if n > 2 {
        let imp = &mut out[start..start + n];
        dp(&coords, imp, 0, n - 1);
    }
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

/// Streaming sidecar writer: the u16 payload goes to a temp file as
/// features are processed; only the offset table (8 B per feature) stays
/// in memory. Produces bytes identical to [`ImportanceSidecar::encode`].
pub struct SidecarStreamWriter {
    /// Byte offsets into payload; len == feature_count + 1.
    offsets: Vec<u64>,
    tmp: std::io::BufWriter<std::fs::File>,
}

impl SidecarStreamWriter {
    pub fn new() -> std::io::Result<Self> {
        Ok(SidecarStreamWriter {
            offsets: vec![0],
            tmp: std::io::BufWriter::new(tempfile::tempfile()?),
        })
    }

    pub fn feature_count(&self) -> u64 {
        (self.offsets.len() - 1) as u64
    }

    /// Append the importance array of the next feature (file order).
    pub fn push(&mut self, imp: &[u16]) -> std::io::Result<()> {
        use std::io::Write;
        for v in imp {
            self.tmp.write_all(&v.to_le_bytes())?;
        }
        self.offsets
            .push(self.offsets.last().unwrap() + (imp.len() * 2) as u64);
        Ok(())
    }

    /// Write the complete section (count + offset table + payload) and
    /// return its size in bytes.
    pub fn write_to<W: std::io::Write>(self, out: &mut W) -> std::io::Result<u64> {
        use std::io::{Seek, SeekFrom};
        out.write_all(&self.feature_count().to_le_bytes())?;
        for o in &self.offsets {
            out.write_all(&o.to_le_bytes())?;
        }
        let mut f = self.tmp.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(0))?;
        let copied = std::io::copy(&mut f, out)?;
        debug_assert_eq!(copied, *self.offsets.last().unwrap());
        Ok(8 + self.offsets.len() as u64 * 8 + copied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mercator::lonlat_to_q32;
    use geo_types::{line_string, polygon, Coord, LineString};

    /// World span as f64 — test coordinates are in Q32 units.
    const S: f64 = 4_294_967_296.0;

    #[test]
    fn quantize_monotone() {
        let mut prev = 0u16;
        for e in (0..64).map(|e| (e as f64).exp2()) {
            let q = quantize_sqdist(e);
            assert!(q >= prev, "quantization must be monotone");
            prev = q;
        }
    }

    #[test]
    fn quantize_dequantize_roundtrip() {
        for &d2 in &[1.0, 1e3, 1e6, 1e12, 1e18] {
            let q = quantize_sqdist(d2);
            // inverse lands in the same bucket
            assert_eq!(quantize_sqdist(dequantize(q)), q, "d2={d2} q={q}");
        }
    }

    #[test]
    fn line_endpoints_always() {
        let ls = line_string![(x: 0.0, y: 0.0), (x: 0.5 * S, y: 0.001 * S), (x: S, y: 0.0)];
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
            (x: 0.25 * S, y: 10.0),
            (x: 0.5 * S, y: 0.1 * S),
            (x: 0.75 * S, y: 10.0),
            (x: S, y: 0.0)
        ];
        let imp = geometry_importance(&Geometry::LineString(ls));
        assert!(imp[2] > imp[1]);
        assert!(imp[2] > imp[3]);
    }

    #[test]
    fn collinear_vertices_stay_droppable() {
        // exactly collinear interior vertices keep the default minimum
        let ls = line_string![
            (x: 0.0, y: 0.0), (x: 0.25 * S, y: 0.0), (x: 0.5 * S, y: 0.0), (x: S, y: 0.0)
        ];
        let imp = geometry_importance(&Geometry::LineString(ls));
        assert_eq!(imp, vec![ALWAYS, 1, 1, ALWAYS]);
    }

    #[test]
    fn polygon_ring_order() {
        let p = polygon![
            exterior: [(x: 0.0, y: 0.0), (x: S, y: 0.0), (x: S, y: S), (x: 0.0, y: S), (x: 0.0, y: 0.0)],
            interiors: [[(x: 0.4 * S, y: 0.4 * S), (x: 0.6 * S, y: 0.4 * S), (x: 0.6 * S, y: 0.6 * S), (x: 0.4 * S, y: 0.6 * S), (x: 0.4 * S, y: 0.4 * S)]]
        ];
        let imp = geometry_importance(&Geometry::Polygon(p));
        assert_eq!(imp.len(), 10); // 5 + 5 coords incl. closing dup
    }

    #[test]
    fn stream_writer_matches_encode() {
        let arrays: Vec<Vec<u16>> = vec![
            vec![ALWAYS, 100, ALWAYS],
            vec![ALWAYS],
            vec![ALWAYS, 1, 2, 3, ALWAYS],
        ];
        let mut sc = ImportanceSidecar::new();
        let mut sw = SidecarStreamWriter::new().unwrap();
        for a in &arrays {
            sc.push(a);
            sw.push(a).unwrap();
        }
        let mut streamed = Vec::new();
        let size = sw.write_to(&mut streamed).unwrap();
        assert_eq!(streamed, sc.encode());
        assert_eq!(size as usize, streamed.len());
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
