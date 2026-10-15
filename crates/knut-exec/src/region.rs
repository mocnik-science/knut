//! Regions for `within` (KNUT_LANG.md §5.7): boxes, area geometries, unions and the raster
//! of an isochrone.

use crate::geom::{self, BBoxF, Geom, P, Polygon};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cover {
    Inside,
    Outside,
    Partial,
}

/// Equal-area-ish raster used for isochrones: cells of `size` metres in a local projection.
#[derive(Debug)]
pub struct Raster {
    pub lon0: f64,
    pub lat0: f64,
    pub size: f64,
    pub cells: FxHashSet<(i32, i32)>,
    pub bbox: BBoxF,
}

impl Raster {
    pub fn kx(&self) -> f64 {
        std::f64::consts::PI / 180.0 * geom::R * self.lat0.to_radians().cos()
    }

    pub fn ky() -> f64 {
        std::f64::consts::PI / 180.0 * geom::R
    }

    pub fn cell_of(&self, p: P) -> (i32, i32) {
        let x = (p.0 - self.lon0) * self.kx();
        let y = (p.1 - self.lat0) * Self::ky();
        ((x / self.size).floor() as i32, (y / self.size).floor() as i32)
    }

    pub fn contains(&self, p: P) -> bool {
        self.cells.contains(&self.cell_of(p))
    }

    /// The cells merged into polygons (outer rings counter-clockwise, holes clockwise), so that a
    /// map draws the border of the region instead of the border of every cell.
    pub fn outline(&self) -> Vec<Polygon> {
        type V = (i32, i32);
        // the cell borders that are not shared, directed so that the cell lies to the left
        let mut next: FxHashMap<V, Vec<V>> = Default::default();
        for &(x, y) in &self.cells {
            let free = |dx, dy| !self.cells.contains(&(x + dx, y + dy));
            let mut edge = |a: V, b: V| next.entry(a).or_default().push(b);
            if free(0, -1) {
                edge((x, y), (x + 1, y));
            }
            if free(1, 0) {
                edge((x + 1, y), (x + 1, y + 1));
            }
            if free(0, 1) {
                edge((x + 1, y + 1), (x, y + 1));
            }
            if free(-1, 0) {
                edge((x, y + 1), (x, y));
            }
        }
        // chain the borders into rings; where two cells touch only at a corner, turn left, which
        // keeps them apart
        let mut rings: Vec<Vec<V>> = Vec::new();
        let starts: Vec<V> = {
            let mut v: Vec<V> = next.keys().copied().collect();
            v.sort_unstable();
            v
        };
        for start in starts {
            while next.get(&start).is_some_and(|v| !v.is_empty()) {
                let mut ring = vec![start];
                let (mut prev, mut cur) = (start, next.get_mut(&start).unwrap().pop().unwrap());
                while cur != start {
                    let outs = next.get_mut(&cur).expect("closed border");
                    let d = (cur.0 - prev.0, cur.1 - prev.1);
                    let left = (cur.0 - d.1, cur.1 + d.0);
                    let i = outs.iter().position(|&o| o == left).unwrap_or(0);
                    let n = outs.swap_remove(i);
                    // only the corners
                    let d2 = (n.0 - cur.0, n.1 - cur.1);
                    if d2 != d {
                        ring.push(cur);
                    }
                    (prev, cur) = (cur, n);
                }
                // the start is a corner unless the ring arrives in its own direction
                let d_in = (start.0 - prev.0, start.1 - prev.1);
                let d_out = (ring.get(1).copied().unwrap_or(start).0 - start.0, ring.get(1).copied().unwrap_or(start).1 - start.1);
                if ring.len() > 1 && d_in.0 * d_out.1 == d_in.1 * d_out.0 && d_in.0 * d_out.0 + d_in.1 * d_out.1 > 0 {
                    ring.remove(0);
                }
                rings.push(ring);
            }
        }
        let area2 = |r: &[V]| -> i64 {
            (0..r.len()).map(|i| {
                let (a, b) = (r[i], r[(i + 1) % r.len()]);
                a.0 as i64 * b.1 as i64 - b.0 as i64 * a.1 as i64
            }).sum()
        };
        let (outers, holes): (Vec<Vec<V>>, Vec<Vec<V>>) = rings.into_iter().partition(|r| area2(r) > 0);
        // a hole belongs to the smallest outer ring around the cell next to its first border
        let inside = |r: &[V], p: (f64, f64)| {
            let mut c = false;
            for i in 0..r.len() {
                let (a, b) = (r[i], r[(i + 1) % r.len()]);
                let (ax, ay, bx, by) = (a.0 as f64, a.1 as f64, b.0 as f64, b.1 as f64);
                if (ay > p.1) != (by > p.1) && p.0 < ax + (p.1 - ay) / (by - ay) * (bx - ax) {
                    c = !c;
                }
            }
            c
        };
        let mut polys: Vec<(Vec<V>, Vec<Vec<V>>)> = outers.into_iter().map(|o| (o, vec![])).collect();
        for h in holes {
            let (a, b) = (h[0], h[1 % h.len()]);
            let d = ((b.0 - a.0).signum(), (b.1 - a.1).signum());
            let p = (a.0 as f64 + 0.5 * d.0 as f64 - 0.5 * d.1 as f64, a.1 as f64 + 0.5 * d.1 as f64 + 0.5 * d.0 as f64);
            if let Some(i) = (0..polys.len()).filter(|&i| inside(&polys[i].0, p)).min_by_key(|&i| area2(&polys[i].0)) {
                polys[i].1.push(h);
            }
        }
        let (kx, ky) = (self.kx(), Raster::ky());
        let to_deg = |r: &[V]| -> Vec<P> {
            let mut v: Vec<P> = r.iter().map(|&(x, y)| (self.lon0 + x as f64 * self.size / kx, self.lat0 + y as f64 * self.size / ky)).collect();
            v.push(v[0]);
            v
        };
        polys.iter().map(|(o, hs)| Polygon { outer: to_deg(o), holes: hs.iter().map(|h| to_deg(h)).collect() }).collect()
    }

    /// Adds the cells enclosed by marked cells (blocks surrounded by reachable streets):
    /// everything that cannot be reached from outside the bounding box of the cells.
    pub fn fill_holes(&mut self) {
        let Some(&(x0, y0)) = self.cells.iter().next() else { return };
        let (mut minx, mut miny, mut maxx, mut maxy) = (x0, y0, x0, y0);
        for &(x, y) in &self.cells {
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
        }
        // one cell of margin so that the outside is connected
        let (minx, miny, maxx, maxy) = (minx - 1, miny - 1, maxx + 1, maxy + 1);
        let w = (maxx - minx + 1) as usize;
        let h = (maxy - miny + 1) as usize;
        let idx = |x: i32, y: i32| (y - miny) as usize * w + (x - minx) as usize;
        let mut outside = vec![false; w * h];
        let mut stack = vec![(minx, miny)];
        outside[idx(minx, miny)] = true;
        while let Some((x, y)) = stack.pop() {
            for (nx, ny) in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] {
                if nx < minx || nx > maxx || ny < miny || ny > maxy {
                    continue;
                }
                let i = idx(nx, ny);
                if !outside[i] && !self.cells.contains(&(nx, ny)) {
                    outside[i] = true;
                    stack.push((nx, ny));
                }
            }
        }
        for y in miny..=maxy {
            for x in minx..=maxx {
                if !outside[idx(x, y)] {
                    self.cells.insert((x, y));
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum Region {
    World,
    Box(BBoxF),
    Geom { geom: Arc<Geom>, bbox: BBoxF, label: String },
    Raster(Raster),
    Union(Vec<Arc<Region>>, BBoxF, String),
}

impl Region {
    pub fn from_geom(g: Arc<Geom>, label: String) -> Region {
        let bbox = g.bbox();
        Region::Geom { geom: g, bbox, label }
    }

    pub fn bbox(&self) -> BBoxF {
        match self {
            Region::World => BBoxF::WORLD,
            Region::Box(b) => *b,
            Region::Geom { bbox, .. } => *bbox,
            Region::Raster(r) => r.bbox,
            Region::Union(_, b, _) => *b,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Region::World => "world".into(),
            Region::Box(b) => format!("{}...{}, {}...{}", b.minx, b.maxx, b.miny, b.maxy),
            Region::Geom { label, .. } => label.clone(),
            Region::Raster(r) => format!("isochrone ({} cells)", r.cells.len()),
            Region::Union(_, _, l) => l.clone(),
        }
    }

    pub fn contains(&self, p: P) -> bool {
        match self {
            Region::World => true,
            Region::Box(b) => b.contains(p),
            Region::Geom { geom, bbox, .. } => bbox.contains(p) && geom.contains_point(p),
            Region::Raster(r) => r.bbox.contains(p) && r.contains(p),
            Region::Union(rs, b, _) => b.contains(p) && rs.iter().any(|r| r.contains(p)),
        }
    }

    /// Whether a box lies fully inside, outside or across the region border.
    pub fn cover(&self, b: &BBoxF) -> Cover {
        if b.is_empty() {
            return Cover::Outside;
        }
        match self {
            Region::World => Cover::Inside,
            Region::Box(r) => {
                if r.contains_box(b) {
                    Cover::Inside
                } else if r.intersects(b) {
                    Cover::Partial
                } else {
                    Cover::Outside
                }
            }
            Region::Geom { geom, bbox, .. } => {
                if !bbox.intersects(b) {
                    return Cover::Outside;
                }
                let corners = [(b.minx, b.miny), (b.maxx, b.miny), (b.maxx, b.maxy), (b.minx, b.maxy)];
                let inside = corners.iter().filter(|c| geom.contains_point(**c)).count();
                if inside == 4 && !polygon_edges_cross_box(geom, b) {
                    return Cover::Inside;
                }
                if inside == 0 && !polygon_edges_cross_box(geom, b) && !geom.polygons().iter().any(|p| b.contains(p.outer[0])) {
                    return Cover::Outside;
                }
                Cover::Partial
            }
            Region::Raster(r) => {
                if r.bbox.intersects(b) {
                    Cover::Partial
                } else {
                    Cover::Outside
                }
            }
            Region::Union(rs, bb, _) => {
                if !bb.intersects(b) {
                    return Cover::Outside;
                }
                let mut any_partial = false;
                for r in rs {
                    match r.cover(b) {
                        Cover::Inside => return Cover::Inside,
                        Cover::Partial => any_partial = true,
                        Cover::Outside => {}
                    }
                }
                if any_partial { Cover::Partial } else { Cover::Outside }
            }
        }
    }

    pub fn area_m2(&self) -> f64 {
        match self {
            Region::World => 510_072_000e6,
            Region::Box(b) => b.area_km2() * 1e6,
            Region::Geom { geom, .. } => geom.area_m2(),
            Region::Raster(r) => r.cells.len() as f64 * r.size * r.size,
            Region::Union(rs, _, _) => rs.iter().map(|r| r.area_m2()).sum(),
        }
    }

    /// Geometry for maps: an isochrone as its outline instead of single cells.
    pub fn display_geom(&self) -> Geom {
        match self {
            Region::Raster(r) => Geom::Poly(r.outline()),
            Region::Union(rs, _, _) => Geom::Multi(rs.iter().map(|r| r.display_geom()).collect()),
            _ => self.geom(),
        }
    }

    /// Geometry for distance computations.
    pub fn geom(&self) -> Geom {
        match self {
            Region::World => Geom::Empty,
            Region::Box(b) => Geom::Poly(vec![Polygon {
                outer: vec![(b.minx, b.miny), (b.maxx, b.miny), (b.maxx, b.maxy), (b.minx, b.maxy), (b.minx, b.miny)],
                holes: vec![],
            }]),
            Region::Geom { geom, .. } => (**geom).clone(),
            Region::Raster(r) => {
                let (kx, ky) = (r.kx(), Raster::ky());
                Geom::Poly(
                    r.cells
                        .iter()
                        .map(|&(x, y)| {
                            let x0 = r.lon0 + x as f64 * r.size / kx;
                            let y0 = r.lat0 + y as f64 * r.size / ky;
                            let x1 = x0 + r.size / kx;
                            let y1 = y0 + r.size / ky;
                            Polygon { outer: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)], holes: vec![] }
                        })
                        .collect(),
                )
            }
            Region::Union(rs, _, _) => Geom::Multi(rs.iter().map(|r| r.geom()).collect()),
        }
    }
}

fn polygon_edges_cross_box(g: &Geom, b: &BBoxF) -> bool {
    let e7 = |p: P| ((p.0 * 1e7) as i32, (p.1 * 1e7) as i32);
    let bb = b.to_e7();
    for poly in g.polygons() {
        for ring in std::iter::once(&poly.outer).chain(poly.holes.iter()) {
            let pts: Vec<(i32, i32)> = ring.iter().map(|p| e7(*p)).collect();
            if knut::line_intersects(&pts, &bb) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(cells: &[(i32, i32)]) -> Raster {
        Raster { lon0: 0.0, lat0: 0.0, size: Raster::ky(), cells: cells.iter().copied().collect(), bbox: BBoxF::WORLD }
    }

    /// Rings in cell units, without the closing point.
    fn rings(r: &Raster) -> Vec<(Vec<(i32, i32)>, usize)> {
        let k = r.kx() / r.size;
        let cell = |p: &P| ((p.0 * k).round() as i32, p.1.round() as i32);
        let mut v: Vec<_> = r
            .outline()
            .iter()
            .map(|p| (p.outer[..p.outer.len() - 1].iter().map(cell).collect::<Vec<_>>(), p.holes.len()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn outline_merges_cells_into_one_ring_of_corners() {
        // an L of three cells: six corners, counter-clockwise
        let r = raster(&[(0, 0), (1, 0), (0, 1)]);
        let rs = rings(&r);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].0.len(), 6, "{rs:?}");
        assert_eq!(rs[0].1, 0);
    }

    #[test]
    fn outline_keeps_holes_and_separates_corner_neighbours() {
        // a ring of eight cells around a hole
        let ring: Vec<_> = (0..3).flat_map(|x| (0..3).map(move |y| (x, y))).filter(|&c| c != (1, 1)).collect();
        let rs = rings(&raster(&ring));
        assert_eq!(rs.len(), 1);
        assert_eq!((rs[0].0.len(), rs[0].1), (4, 1), "{rs:?}");
        // two cells touching at a corner: two squares
        let rs = rings(&raster(&[(0, 0), (1, 1)]));
        assert_eq!(rs.len(), 2);
        assert!(rs.iter().all(|r| r.0.len() == 4 && r.1 == 0), "{rs:?}");
    }
}
