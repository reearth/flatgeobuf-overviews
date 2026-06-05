//! Axis-aligned rectangle clipping for geo-types geometries.
//!
//! Polygons use Sutherland–Hodgman (clip window is convex, subject may be
//! concave); lines use per-segment Liang–Barsky walking, splitting into
//! multiple parts on exit/re-entry. Deterministic, allocation-light.

use geo_types::{
    Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

/// Clip rectangle. Works in any planar CRS (we use unit mercator, y down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Rect {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    pub fn intersects_bbox(&self, min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> bool {
        !(max_x < self.min_x || min_x > self.max_x || max_y < self.min_y || min_y > self.max_y)
    }
}

/// Clip any geometry to `rect`. Returns `None` if nothing remains.
/// LineStrings may become MultiLineStrings; Polygons may lose rings.
pub fn clip_geometry(geom: &Geometry<f64>, rect: Rect) -> Option<Geometry<f64>> {
    match geom {
        Geometry::Point(p) => rect.contains(p.x(), p.y()).then(|| geom.clone()),
        Geometry::MultiPoint(mp) => {
            let pts: Vec<Point<f64>> =
                mp.0.iter()
                    .filter(|p| rect.contains(p.x(), p.y()))
                    .cloned()
                    .collect();
            (!pts.is_empty()).then_some(Geometry::MultiPoint(MultiPoint(pts)))
        }
        Geometry::LineString(ls) => {
            let parts = clip_line(ls, rect);
            match parts.len() {
                0 => None,
                1 => Some(Geometry::LineString(parts.into_iter().next().unwrap())),
                _ => Some(Geometry::MultiLineString(MultiLineString(parts))),
            }
        }
        Geometry::MultiLineString(mls) => {
            let parts: Vec<LineString<f64>> =
                mls.0.iter().flat_map(|ls| clip_line(ls, rect)).collect();
            (!parts.is_empty()).then_some(Geometry::MultiLineString(MultiLineString(parts)))
        }
        Geometry::Polygon(p) => clip_polygon(p, rect).map(Geometry::Polygon),
        Geometry::MultiPolygon(mp) => {
            let polys: Vec<Polygon<f64>> =
                mp.0.iter().filter_map(|p| clip_polygon(p, rect)).collect();
            (!polys.is_empty()).then_some(Geometry::MultiPolygon(MultiPolygon(polys)))
        }
        Geometry::GeometryCollection(gc) => {
            let geoms: Vec<Geometry<f64>> =
                gc.iter().filter_map(|g| clip_geometry(g, rect)).collect();
            (!geoms.is_empty()).then(|| Geometry::GeometryCollection(geoms.into()))
        }
        _ => Some(geom.clone()),
    }
}

/// Liang–Barsky clip of one segment; returns clipped endpoints if any part
/// is inside.
fn clip_segment(a: Coord<f64>, b: Coord<f64>, r: Rect) -> Option<(Coord<f64>, Coord<f64>)> {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;

    for (p, q) in [
        (-dx, a.x - r.min_x),
        (dx, r.max_x - a.x),
        (-dy, a.y - r.min_y),
        (dy, r.max_y - a.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                if t > t1 {
                    return None;
                }
                if t > t0 {
                    t0 = t;
                }
            } else {
                if t < t0 {
                    return None;
                }
                if t < t1 {
                    t1 = t;
                }
            }
        }
    }

    Some((
        Coord {
            x: a.x + t0 * dx,
            y: a.y + t0 * dy,
        },
        Coord {
            x: a.x + t1 * dx,
            y: a.y + t1 * dy,
        },
    ))
}

/// Clip a linestring, splitting into parts where it leaves the rect.
fn clip_line(ls: &LineString<f64>, rect: Rect) -> Vec<LineString<f64>> {
    let coords = &ls.0;
    let mut parts: Vec<Vec<Coord<f64>>> = Vec::new();
    let mut cur: Vec<Coord<f64>> = Vec::new();

    for w in coords.windows(2) {
        let (a, b) = (w[0], w[1]);
        match clip_segment(a, b, rect) {
            Some((ca, cb)) => {
                if cur.is_empty() {
                    cur.push(ca);
                } else if *cur.last().unwrap() != ca {
                    // discontinuity: segment re-enters at a different point
                    if cur.len() >= 2 {
                        parts.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                    cur.push(ca);
                }
                cur.push(cb);
            }
            None => {
                if cur.len() >= 2 {
                    parts.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
        }
    }
    if cur.len() >= 2 {
        parts.push(cur);
    }
    parts.into_iter().map(LineString).collect()
}

/// Sutherland–Hodgman clip of a ring against one half-plane.
fn clip_ring_edge<F, G>(ring: &[Coord<f64>], inside: F, intersect: G) -> Vec<Coord<f64>>
where
    F: Fn(Coord<f64>) -> bool,
    G: Fn(Coord<f64>, Coord<f64>) -> Coord<f64>,
{
    let mut out = Vec::with_capacity(ring.len());
    if ring.is_empty() {
        return out;
    }
    let n = ring.len();
    for i in 0..n {
        let cur = ring[i];
        let prev = ring[(i + n - 1) % n];
        let cur_in = inside(cur);
        let prev_in = inside(prev);
        if cur_in {
            if !prev_in {
                out.push(intersect(prev, cur));
            }
            out.push(cur);
        } else if prev_in {
            out.push(intersect(prev, cur));
        }
    }
    out
}

/// Sutherland–Hodgman clip of a ring (open representation, no closing dup)
/// against the rect.
fn clip_ring(ring: &[Coord<f64>], r: Rect) -> Vec<Coord<f64>> {
    let lerp_x = |a: Coord<f64>, b: Coord<f64>, x: f64| {
        let t = (x - a.x) / (b.x - a.x);
        Coord {
            x,
            y: a.y + t * (b.y - a.y),
        }
    };
    let lerp_y = |a: Coord<f64>, b: Coord<f64>, y: f64| {
        let t = (y - a.y) / (b.y - a.y);
        Coord {
            x: a.x + t * (b.x - a.x),
            y,
        }
    };

    let ring = clip_ring_edge(ring, |c| c.x >= r.min_x, |a, b| lerp_x(a, b, r.min_x));
    let ring = clip_ring_edge(&ring, |c| c.x <= r.max_x, |a, b| lerp_x(a, b, r.max_x));
    let ring = clip_ring_edge(&ring, |c| c.y >= r.min_y, |a, b| lerp_y(a, b, r.min_y));
    clip_ring_edge(&ring, |c| c.y <= r.max_y, |a, b| lerp_y(a, b, r.max_y))
}

fn close_ring(mut coords: Vec<Coord<f64>>) -> Option<LineString<f64>> {
    if coords.len() < 3 {
        return None;
    }
    if coords.first() != coords.last() {
        coords.push(coords[0]);
    }
    if coords.len() < 4 {
        return None;
    }
    Some(LineString(coords))
}

fn clip_polygon(p: &Polygon<f64>, rect: Rect) -> Option<Polygon<f64>> {
    // open representation: drop the closing duplicate before S-H
    let open = |ls: &LineString<f64>| -> Vec<Coord<f64>> {
        let c = &ls.0;
        if c.len() >= 2 && c.first() == c.last() {
            c[..c.len() - 1].to_vec()
        } else {
            c.clone()
        }
    };

    let ext = close_ring(clip_ring(&open(p.exterior()), rect))?;
    let interiors: Vec<LineString<f64>> = p
        .interiors()
        .iter()
        .filter_map(|r| close_ring(clip_ring(&open(r), rect)))
        .collect();
    Some(Polygon::new(ext, interiors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{line_string, polygon};

    const R: Rect = Rect {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };

    #[test]
    fn point_clip() {
        let inside = Geometry::Point(Point::new(0.5, 0.5));
        let outside = Geometry::Point(Point::new(2.0, 0.5));
        assert!(clip_geometry(&inside, R).is_some());
        assert!(clip_geometry(&outside, R).is_none());
    }

    #[test]
    fn line_pass_through() {
        let ls = line_string![(x: -1.0, y: 0.5), (x: 2.0, y: 0.5)];
        let g = clip_geometry(&Geometry::LineString(ls), R).unwrap();
        match g {
            Geometry::LineString(l) => {
                assert_eq!(l.0.first().unwrap().x, 0.0);
                assert_eq!(l.0.last().unwrap().x, 1.0);
            }
            other => panic!("expected linestring, got {other:?}"),
        }
    }

    #[test]
    fn line_exit_reenter_splits() {
        let ls = line_string![
            (x: 0.1, y: 0.5),
            (x: 2.0, y: 0.5),   // exits east
            (x: 2.0, y: 0.8),
            (x: 0.9, y: 0.8)    // re-enters
        ];
        let g = clip_geometry(&Geometry::LineString(ls), R).unwrap();
        match g {
            Geometry::MultiLineString(ml) => assert_eq!(ml.0.len(), 2),
            other => panic!("expected multilinestring, got {other:?}"),
        }
    }

    #[test]
    fn line_fully_outside() {
        let ls = line_string![(x: 2.0, y: 2.0), (x: 3.0, y: 3.0)];
        assert!(clip_geometry(&Geometry::LineString(ls), R).is_none());
    }

    #[test]
    fn polygon_corner_clip() {
        // polygon overlapping the NE corner
        let p = polygon![
            (x: 0.5, y: 0.5), (x: 2.0, y: 0.5), (x: 2.0, y: 2.0), (x: 0.5, y: 2.0), (x: 0.5, y: 0.5)
        ];
        let g = clip_geometry(&Geometry::Polygon(p), R).unwrap();
        match g {
            Geometry::Polygon(p) => {
                for c in p.exterior().coords() {
                    assert!(c.x <= 1.0 + 1e-12 && c.y <= 1.0 + 1e-12);
                }
                // clipped to the square [0.5,1]x[0.5,1]
                assert_eq!(p.exterior().0.len(), 5);
            }
            other => panic!("expected polygon, got {other:?}"),
        }
    }

    #[test]
    fn polygon_fully_outside() {
        let p = polygon![(x: 2.0, y: 2.0), (x: 3.0, y: 2.0), (x: 3.0, y: 3.0), (x: 2.0, y: 2.0)];
        assert!(clip_geometry(&Geometry::Polygon(p), R).is_none());
    }

    #[test]
    fn polygon_fully_inside_unchanged() {
        let p = polygon![(x: 0.2, y: 0.2), (x: 0.8, y: 0.2), (x: 0.8, y: 0.8), (x: 0.2, y: 0.2)];
        let g = clip_geometry(&Geometry::Polygon(p.clone()), R).unwrap();
        match g {
            Geometry::Polygon(p2) => assert_eq!(p2.exterior().0.len(), p.exterior().0.len()),
            other => panic!("expected polygon, got {other:?}"),
        }
    }

    #[test]
    fn polygon_hole_clipped() {
        let p = polygon![
            exterior: [(x: -1.0, y: -1.0), (x: 2.0, y: -1.0), (x: 2.0, y: 2.0), (x: -1.0, y: 2.0), (x: -1.0, y: -1.0)],
            interiors: [[(x: 0.4, y: 0.4), (x: 0.6, y: 0.4), (x: 0.6, y: 0.6), (x: 0.4, y: 0.6), (x: 0.4, y: 0.4)]]
        ];
        let g = clip_geometry(&Geometry::Polygon(p), R).unwrap();
        match g {
            Geometry::Polygon(p) => {
                assert_eq!(p.interiors().len(), 1, "hole inside rect must survive");
            }
            other => panic!("expected polygon, got {other:?}"),
        }
    }
}
