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
    Geom { geom: Arc<Geom>, bbox: BBoxF, label: String, prep: std::sync::OnceLock<Option<Prepared>> },
    Raster(Raster),
    Union(Union),
}

/// Several regions together (`within (areas p with …)`), with a grid over their boxes so that
/// a point or block is tested only against the regions near it.
#[derive(Debug)]
pub struct Union {
    pub parts: Vec<Arc<Region>>,
    pub bbox: BBoxF,
    pub label: String,
    grid: std::sync::OnceLock<PartGrid>,
}

#[derive(Debug)]
struct PartGrid {
    cell: f64,
    cells: FxHashMap<(i32, i32), Vec<u32>>,
    /// parts spanning many cells: always tested
    big: Vec<u32>,
}

impl Union {
    pub fn new(parts: Vec<Arc<Region>>, bbox: BBoxF, label: String) -> Union {
        Union { parts, bbox, label, grid: Default::default() }
    }

    fn grid(&self) -> &PartGrid {
        self.grid.get_or_init(|| {
            let boxes: Vec<BBoxF> = self.parts.iter().map(|r| r.bbox()).collect();
            let mut sizes: Vec<f64> = boxes.iter().filter(|b| !b.is_empty()).map(|b| (b.maxx - b.minx).max(b.maxy - b.miny)).collect();
            sizes.sort_by(f64::total_cmp);
            let cell = sizes.get(sizes.len() / 2).copied().unwrap_or(1.0).max(1e-4) * 2.0;
            let mut cells: FxHashMap<(i32, i32), Vec<u32>> = FxHashMap::default();
            let mut big = Vec::new();
            for (i, b) in boxes.iter().enumerate() {
                if b.is_empty() {
                    continue;
                }
                let (x0, y0, x1, y1) = cell_range(b, cell);
                if (x1 - x0 + 1) as i64 * (y1 - y0 + 1) as i64 > 64 {
                    big.push(i as u32);
                    continue;
                }
                for x in x0..=x1 {
                    for y in y0..=y1 {
                        cells.entry((x, y)).or_default().push(i as u32);
                    }
                }
            }
            PartGrid { cell, cells, big }
        })
    }

    /// The parts whose box can intersect `b`.
    fn near(&self, b: &BBoxF) -> Vec<u32> {
        let g = self.grid();
        let (x0, y0, x1, y1) = cell_range(b, g.cell);
        if (x1 - x0 + 1) as i64 * (y1 - y0 + 1) as i64 > 4096 {
            return (0..self.parts.len() as u32).collect();
        }
        let mut out: Vec<u32> = g.big.clone();
        for x in x0..=x1 {
            for y in y0..=y1 {
                if let Some(v) = g.cells.get(&(x, y)) {
                    out.extend(v);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

fn cell_range(b: &BBoxF, cell: f64) -> (i32, i32, i32, i32) {
    let f = |v: f64| (v / cell).floor().clamp(-1e9, 1e9) as i32;
    (f(b.minx), f(b.miny), f(b.maxx), f(b.maxy))
}

impl Region {
    pub fn from_geom(g: Arc<Geom>, label: String) -> Region {
        let bbox = g.bbox();
        Region::Geom { geom: g, bbox, label, prep: Default::default() }
    }

    pub fn bbox(&self) -> BBoxF {
        match self {
            Region::World => BBoxF::WORLD,
            Region::Box(b) => *b,
            Region::Geom { bbox, .. } => *bbox,
            Region::Raster(r) => r.bbox,
            Region::Union(u) => u.bbox,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Region::World => "world".into(),
            Region::Box(b) => format!("{}...{}, {}...{}", b.minx, b.maxx, b.miny, b.maxy),
            Region::Geom { label, .. } => label.clone(),
            Region::Raster(r) => format!("isochrone ({} cells)", r.cells.len()),
            Region::Union(u) => u.label.clone(),
        }
    }

    pub fn contains(&self, p: P) -> bool {
        match self {
            Region::World => true,
            Region::Box(b) => b.contains(p),
            Region::Geom { geom, bbox, prep, .. } => {
                bbox.contains(p)
                    && match prepared(geom, prep) {
                        Some(pr) => pr.contains(p),
                        None => geom.contains_point(p),
                    }
            }
            Region::Raster(r) => r.bbox.contains(p) && r.contains(p),
            Region::Union(u) => {
                u.bbox.contains(p) && u.near(&BBoxF { minx: p.0, miny: p.1, maxx: p.0, maxy: p.1 }).iter().any(|&i| u.parts[i as usize].contains(p))
            }
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
            Region::Geom { geom, bbox, prep, .. } => {
                if !bbox.intersects(b) {
                    return Cover::Outside;
                }
                if let Some(pr) = prepared(geom, prep) {
                    return pr.cover(b);
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
            Region::Union(u) => {
                if !u.bbox.intersects(b) {
                    return Cover::Outside;
                }
                let mut any_partial = false;
                for i in u.near(b) {
                    match u.parts[i as usize].cover(b) {
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
            Region::Union(u) => u.parts.iter().map(|r| r.area_m2()).sum(),
        }
    }

    /// Geometry for maps and output: an isochrone as its outline instead of single cells.
    pub fn display_geom(&self) -> Geom {
        match self {
            Region::Raster(r) => Geom::Poly(r.outline()),
            Region::Union(u) => Geom::Multi(u.parts.iter().map(|r| r.display_geom()).collect()),
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
            Region::Union(u) => Geom::Multi(u.parts.iter().map(|r| r.geom()).collect()),
        }
    }
}

/// The prepared form of a region with many corners, made on first use (small ones are tested
/// directly).
fn prepared<'a>(g: &Geom, prep: &'a std::sync::OnceLock<Option<Prepared>>) -> Option<&'a Prepared> {
    prep.get_or_init(|| Prepared::new(g)).as_ref()
}

/// A polygon prepared for many tests: a grid over its box whose cells are inside, outside or
/// on the boundary; boundary cells keep the edges that may pass through them, and each row the
/// edges that reach into it. A point in an inside or outside cell is answered at once; else
/// only the edges of its row to its right are counted (the same rule as `geom::ring_contains`).
/// A box is answered from its cells, with the edges of its boundary cells tested against it.
pub struct Prepared {
    bbox: BBoxF,
    nx: usize,
    ny: usize,
    cw: f64,
    ch: f64,
    /// 0 outside, 1 inside, 2 boundary
    state: Vec<u8>,
    cells: Vec<Vec<u32>>,
    /// per row: edges reaching into it, the largest x first
    rows: Vec<Vec<u32>>,
    edges: Vec<(P, P, u32)>,
    polys: usize,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Prepared({}×{} cells, {} edges)", self.nx, self.ny, self.edges.len())
    }
}

impl Prepared {
    fn new(g: &Geom) -> Option<Prepared> {
        let polys = g.polygons();
        let mut edges: Vec<(P, P, u32)> = Vec::new();
        for (k, poly) in polys.iter().enumerate() {
            for ring in std::iter::once(&poly.outer).chain(poly.holes.iter()) {
                let n = ring.len();
                for i in 0..n {
                    let (a, b) = (ring[if i == 0 { n - 1 } else { i - 1 }], ring[i]);
                    if a != b {
                        edges.push((a, b, k as u32));
                    }
                }
            }
        }
        // few corners: testing them directly is as fast
        if edges.len() < 64 {
            return None;
        }
        let bbox = g.bbox();
        let (w, h) = ((bbox.maxx - bbox.minx).max(1e-9), (bbox.maxy - bbox.miny).max(1e-9));
        let target = (edges.len() as f64).clamp(256.0, 262_144.0);
        let nx = ((target * w / h).sqrt().round() as usize).clamp(1, 1024);
        let ny = ((target / nx as f64).round() as usize).clamp(1, 1024);
        let (cw, ch) = (w / nx as f64, h / ny as f64);
        let mut pr = Prepared { bbox, nx, ny, cw, ch, state: vec![0; nx * ny], cells: vec![Vec::new(); nx * ny], rows: vec![Vec::new(); ny], edges, polys: polys.len() };
        for (i, &(a, b, _)) in pr.edges.iter().enumerate() {
            let (x0, x1) = (pr.col(a.0.min(b.0)), pr.col(a.0.max(b.0)));
            let (y0, y1) = (pr.row(a.1.min(b.1)), pr.row(a.1.max(b.1)));
            for y in y0..=y1 {
                pr.rows[y].push(i as u32);
                for x in x0..=x1 {
                    pr.cells[y * nx + x].push(i as u32);
                    pr.state[y * nx + x] = 2;
                }
            }
        }
        for r in &mut pr.rows {
            let e = &pr.edges;
            r.sort_by(|&i, &j| {
                let m = |k: u32| e[k as usize].0.0.max(e[k as usize].1.0);
                m(j).total_cmp(&m(i))
            });
        }
        // cells without edges: inside or outside as their centre, from the crossings of the
        // row's middle line, swept from the right
        for y in 0..ny {
            let yc = bbox.miny + (y as f64 + 0.5) * ch;
            let mut xs: Vec<(f64, u32)> = pr.rows[y]
                .iter()
                .filter_map(|&i| {
                    let ((x1, y1), (x2, y2), k) = pr.edges[i as usize];
                    ((y1 > yc) != (y2 > yc)).then(|| ((x2 - x1) * (yc - y1) / (y2 - y1) + x1, k))
                })
                .collect();
            xs.sort_by(|a, b| b.0.total_cmp(&a.0));
            let mut parity = vec![false; pr.polys];
            let mut odd = 0usize;
            let mut it = xs.iter().peekable();
            for x in (0..nx).rev() {
                let xc = bbox.minx + (x as f64 + 0.5) * cw;
                while let Some(&&(cx, k)) = it.peek()
                    && cx > xc
                {
                    let p = &mut parity[k as usize];
                    *p = !*p;
                    if *p { odd += 1 } else { odd -= 1 }
                    it.next();
                }
                if pr.state[y * nx + x] != 2 && odd > 0 {
                    pr.state[y * nx + x] = 1;
                }
            }
        }
        Some(pr)
    }

    fn col(&self, x: f64) -> usize {
        (((x - self.bbox.minx) / self.cw).floor().max(0.0) as usize).min(self.nx - 1)
    }

    fn row(&self, y: f64) -> usize {
        (((y - self.bbox.miny) / self.ch).floor().max(0.0) as usize).min(self.ny - 1)
    }

    pub fn contains(&self, p: P) -> bool {
        if !self.bbox.contains(p) {
            return false;
        }
        let (x, y) = (self.col(p.0), self.row(p.1));
        match self.state[y * self.nx + x] {
            0 => false,
            1 => true,
            _ => {
                // as `ring_contains`: crossings of the ray to the right, per polygon
                let mut parity = vec![false; self.polys];
                for &i in &self.rows[y] {
                    let ((x1, y1), (x2, y2), k) = self.edges[i as usize];
                    if x1.max(x2) < p.0 {
                        break;
                    }
                    if (y1 > p.1) != (y2 > p.1) && p.0 < (x2 - x1) * (p.1 - y1) / (y2 - y1) + x1 {
                        parity[k as usize] = !parity[k as usize];
                    }
                }
                parity.iter().any(|&b| b)
            }
        }
    }

    pub fn cover(&self, b: &BBoxF) -> Cover {
        let c = BBoxF { minx: b.minx.max(self.bbox.minx), miny: b.miny.max(self.bbox.miny), maxx: b.maxx.min(self.bbox.maxx), maxy: b.maxy.min(self.bbox.maxy) };
        if c.minx > c.maxx || c.miny > c.maxy {
            return Cover::Outside;
        }
        let within = self.bbox.contains_box(b);
        let (x0, x1, y0, y1) = (self.col(c.minx), self.col(c.maxx), self.row(c.miny), self.row(c.maxy));
        let (mut ins, mut outs, mut edge) = (false, false, Vec::new());
        for y in y0..=y1 {
            for x in x0..=x1 {
                match self.state[y * self.nx + x] {
                    0 => outs = true,
                    1 => ins = true,
                    _ => edge.push(y * self.nx + x),
                }
                if ins && outs {
                    return Cover::Partial;
                }
            }
        }
        if !edge.is_empty() {
            let mut es: Vec<u32> = edge.iter().flat_map(|&i| self.cells[i].iter().copied()).collect();
            es.sort_unstable();
            es.dedup();
            if es.iter().any(|&i| segment_meets_box(self.edges[i as usize].0, self.edges[i as usize].1, b)) {
                return Cover::Partial;
            }
            // no edge inside the box: all of it is on one side
            ins = self.contains(((c.minx + c.maxx) / 2.0, (c.miny + c.maxy) / 2.0));
            outs = !ins;
        }
        match (ins, outs) {
            (true, false) if within => Cover::Inside,
            (true, _) => Cover::Partial,
            _ => Cover::Outside,
        }
    }
}

/// Whether the segment a–b has a point in the box (Liang–Barsky).
fn segment_meets_box(a: P, b: P, r: &BBoxF) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, a.0 - r.minx), (dx, r.maxx - a.0), (-dy, a.1 - r.miny), (dy, r.maxy - a.1)] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
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

    /// A wavy ring with many corners, a hole and a second polygon: the prepared tests answer
    /// as the direct ones, for points and boxes all over the box.
    #[test]
    fn prepared_polygon_answers_as_the_direct_tests() {
        let ring = |cx: f64, cy: f64, r: f64, n: usize| -> Vec<P> {
            let mut v: Vec<P> = (0..n)
                .map(|i| {
                    let a = i as f64 / n as f64 * std::f64::consts::TAU;
                    let rr = r * (1.0 + 0.3 * (7.0 * a).sin());
                    (cx + rr * a.cos(), cy + rr * a.sin())
                })
                .collect();
            v.push(v[0]);
            v
        };
        let g = Geom::Poly(vec![
            Polygon { outer: ring(0.0, 0.0, 1.0, 300), holes: vec![ring(0.1, 0.0, 0.3, 80)] },
            Polygon { outer: ring(3.0, 0.5, 0.5, 120), holes: vec![] },
        ]);
        let reg = Region::from_geom(Arc::new(g.clone()), "test".into());
        let mut seed = 7u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..20_000 {
            let p = (-1.6 + 5.0 * next(), -1.6 + 3.2 * next());
            assert_eq!(reg.contains(p), g.contains_point(p), "{p:?}");
        }
        for _ in 0..3_000 {
            let (x, y, w, h) = (-1.8 + 5.4 * next(), -1.8 + 3.6 * next(), 0.6 * next() * next(), 0.6 * next() * next());
            let b = BBoxF { minx: x, miny: y, maxx: x + w, maxy: y + h };
            let pts: Vec<P> = (0..=6).flat_map(|i| (0..=6).map(move |j| (x + w * i as f64 / 6.0, y + h * j as f64 / 6.0))).collect();
            let inside = pts.iter().filter(|p| g.contains_point(**p)).count();
            match reg.cover(&b) {
                Cover::Inside => assert_eq!(inside, pts.len(), "{b:?}"),
                Cover::Outside => assert_eq!(inside, 0, "{b:?}"),
                Cover::Partial => {}
            }
            if inside > 0 && inside < pts.len() {
                assert_eq!(reg.cover(&b), Cover::Partial, "{b:?}");
            }
        }
    }
}
