//! Apply a per-vertex importance filter to a geometry: the O(n) replacement
//! for running Douglas–Peucker per request.
//!
//! The importance array must be in the same coordinate traversal order as
//! [`crate::importance::geometry_importance`] produced it (which matches
//! FlatGeobuf/geozero traversal).

use crate::mercator::project;
use geo_types::{Geometry, LineString, MultiLineString, MultiPolygon, Polygon};

/// Streaming cursor over an importance array.
struct ImpCursor<'a> {
    imp: &'a [u16],
    pos: usize,
}

impl<'a> ImpCursor<'a> {
    fn take(&mut self, n: usize) -> &'a [u16] {
        let s = &self.imp[self.pos..(self.pos + n).min(self.imp.len())];
        self.pos += n;
        s
    }
}

fn filter_line(ls: &LineString<f64>, imp: &[u16], q: u16) -> LineString<f64> {
    LineString(
        ls.0.iter()
            .zip(imp)
            .filter(|(_, &v)| v >= q)
            .map(|(c, _)| *c)
            .collect(),
    )
}

fn filter_ring(ls: &LineString<f64>, imp: &[u16], q: u16) -> Option<LineString<f64>> {
    let r = filter_line(ls, imp, q);
    // closed ring needs at least 4 coords (3 distinct + closing dup)
    (r.0.len() >= 4).then_some(r)
}

/// Filter `geom` keeping vertices with `importance >= q`. Returns `None`
/// when the geometry degenerates (e.g. polygon ring collapses).
///
/// `imp` must have exactly as many entries as `geom` has coordinates.
pub fn filter_geometry(geom: &Geometry<f64>, imp: &[u16], q: u16) -> Option<Geometry<f64>> {
    let mut cur = ImpCursor { imp, pos: 0 };
    filter_inner(geom, &mut cur, q)
}

fn filter_inner(geom: &Geometry<f64>, cur: &mut ImpCursor<'_>, q: u16) -> Option<Geometry<f64>> {
    match geom {
        Geometry::Point(_) => {
            cur.take(1);
            Some(geom.clone())
        }
        Geometry::MultiPoint(mp) => {
            cur.take(mp.0.len());
            Some(geom.clone())
        }
        Geometry::LineString(ls) => {
            let f = filter_line(ls, cur.take(ls.0.len()), q);
            (f.0.len() >= 2).then_some(Geometry::LineString(f))
        }
        Geometry::MultiLineString(mls) => {
            let parts: Vec<LineString<f64>> = mls
                .0
                .iter()
                .filter_map(|ls| {
                    let f = filter_line(ls, cur.take(ls.0.len()), q);
                    (f.0.len() >= 2).then_some(f)
                })
                .collect();
            (!parts.is_empty()).then_some(Geometry::MultiLineString(MultiLineString(parts)))
        }
        Geometry::Polygon(p) => filter_polygon(p, cur, q).map(Geometry::Polygon),
        Geometry::MultiPolygon(mp) => {
            let polys: Vec<Polygon<f64>> =
                mp.0.iter()
                    .filter_map(|p| filter_polygon(p, cur, q))
                    .collect();
            (!polys.is_empty()).then_some(Geometry::MultiPolygon(MultiPolygon(polys)))
        }
        Geometry::GeometryCollection(gc) => {
            let geoms: Vec<Geometry<f64>> =
                gc.iter().filter_map(|g| filter_inner(g, cur, q)).collect();
            (!geoms.is_empty()).then(|| Geometry::GeometryCollection(geoms.into()))
        }
        other => Some(other.clone()),
    }
}

fn filter_polygon(p: &Polygon<f64>, cur: &mut ImpCursor<'_>, q: u16) -> Option<Polygon<f64>> {
    let ext = filter_ring(p.exterior(), cur.take(p.exterior().0.len()), q);
    let interiors: Vec<LineString<f64>> = p
        .interiors()
        .iter()
        .filter_map(|r| filter_ring(r, cur.take(r.0.len()), q))
        .collect();
    // exterior collapse drops the whole polygon (cursor already advanced)
    ext.map(|e| Polygon::new(e, interiors))
}

/// Bounding box of a geometry in unit mercator: (min_x, min_y, max_x, max_y).
pub fn mercator_bbox(geom: &Geometry<f64>) -> Option<(f64, f64, f64, f64)> {
    let mut bbox: Option<(f64, f64, f64, f64)> = None;
    visit_coords(geom, &mut |x, y| {
        let (mx, my) = project(x, y);
        bbox = Some(match bbox {
            None => (mx, my, mx, my),
            Some((a, b, c, d)) => (a.min(mx), b.min(my), c.max(mx), d.max(my)),
        });
    });
    bbox
}

/// Drop test for feature-level thinning: true when the geometry's mercator
/// bbox is smaller than `min_extent` in both dimensions (lines/polygons only).
pub fn is_too_small(geom: &Geometry<f64>, min_extent: f64) -> bool {
    match geom {
        Geometry::Point(_) | Geometry::MultiPoint(_) => false,
        _ => match mercator_bbox(geom) {
            Some((min_x, min_y, max_x, max_y)) => {
                (max_x - min_x) < min_extent && (max_y - min_y) < min_extent
            }
            None => true,
        },
    }
}

fn visit_coords<F: FnMut(f64, f64)>(geom: &Geometry<f64>, f: &mut F) {
    match geom {
        Geometry::Point(p) => f(p.x(), p.y()),
        Geometry::MultiPoint(mp) => mp.0.iter().for_each(|p| f(p.x(), p.y())),
        Geometry::Line(l) => {
            f(l.start.x, l.start.y);
            f(l.end.x, l.end.y);
        }
        Geometry::LineString(ls) => ls.0.iter().for_each(|c| f(c.x, c.y)),
        Geometry::MultiLineString(mls) => mls
            .0
            .iter()
            .for_each(|ls| ls.0.iter().for_each(|c| f(c.x, c.y))),
        Geometry::Polygon(p) => {
            p.exterior().0.iter().for_each(|c| f(c.x, c.y));
            p.interiors()
                .iter()
                .for_each(|r| r.0.iter().for_each(|c| f(c.x, c.y)));
        }
        Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| {
            p.exterior().0.iter().for_each(|c| f(c.x, c.y));
            p.interiors()
                .iter()
                .for_each(|r| r.0.iter().for_each(|c| f(c.x, c.y)));
        }),
        Geometry::GeometryCollection(gc) => gc.iter().for_each(|g| visit_coords(g, f)),
        _ => {}
    }
}

/// Count coordinates of a geometry (must match importance array length).
pub fn coord_count(geom: &Geometry<f64>) -> usize {
    let mut n = 0;
    visit_coords(geom, &mut |_, _| n += 1);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::importance::{geometry_importance, ALWAYS};
    use geo_types::{line_string, polygon};

    #[test]
    fn filter_keeps_endpoints() {
        let ls = line_string![
            (x: 0.0, y: 0.0), (x: 0.25, y: 1e-9), (x: 0.5, y: 0.1),
            (x: 0.75, y: 1e-9), (x: 1.0, y: 0.0)
        ];
        let g = Geometry::LineString(ls);
        let imp = geometry_importance(&g);
        // very high threshold: only ALWAYS survives
        let f = filter_geometry(&g, &imp, ALWAYS).unwrap();
        match f {
            Geometry::LineString(l) => assert_eq!(l.0.len(), 2),
            _ => panic!(),
        }
        // moderate threshold keeps the big spike but drops the small
        // deviations (whose post-spike DP distance is ~0.0025)
        let q = crate::importance::threshold_q(0.005);
        let f = filter_geometry(&g, &imp, q).unwrap();
        match f {
            Geometry::LineString(l) => assert_eq!(l.0.len(), 3),
            _ => panic!(),
        }
    }

    #[test]
    fn filter_consistency_with_count() {
        let p = polygon![
            (x: 0.0, y: 0.0), (x: 0.5, y: 0.001), (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0), (x: 0.0, y: 1.0), (x: 0.0, y: 0.0)
        ];
        let g = Geometry::Polygon(p);
        let imp = geometry_importance(&g);
        assert_eq!(imp.len(), coord_count(&g));
        // moderate threshold: square corners survive, near-collinear
        // vertex (deviation 0.001 -> d2 ~ 1e-6) drops
        let q = crate::importance::threshold_q(0.01);
        let f = filter_geometry(&g, &imp, q).unwrap();
        match f {
            Geometry::Polygon(p) => assert_eq!(p.exterior().0.len(), 5),
            _ => panic!(),
        }
        // extreme threshold collapses the ring entirely
        assert!(filter_geometry(&g, &imp, ALWAYS).is_none());
    }

    #[test]
    fn small_feature_detection() {
        let tiny = Geometry::Polygon(polygon![
            (x: 139.0, y: 35.0), (x: 139.0001, y: 35.0), (x: 139.0001, y: 35.0001), (x: 139.0, y: 35.0)
        ]);
        assert!(is_too_small(&tiny, 0.001));
        assert!(!is_too_small(&tiny, 1e-9));
    }
}
