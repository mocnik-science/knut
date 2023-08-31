//! Geometry in degrees (lon, lat): representative points, lengths, geodesic areas,
//! distances (KNUT_LANG.md §5.7) and multipolygon assembly.

use geographiclib_rs::{Geodesic, PolygonArea, Winding};
use std::sync::OnceLock;

pub type P = (f64, f64);

/// Mean earth radius for Haversine distances (KNUT_LANG.md §5.7).
pub const R: f64 = 6_371_008.8;

#[derive(Clone, Debug, PartialEq)]
pub struct Polygon {
    pub outer: Vec<P>,
    pub holes: Vec<Vec<P>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Geom {
    Empty,
    Point(P),
    Line(Vec<P>),
    Poly(Vec<Polygon>),
    Multi(Vec<Geom>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBoxF {
    pub minx: f64,
    pub miny: f64,
    pub maxx: f64,
    pub maxy: f64,
}

impl BBoxF {
    pub const EMPTY: BBoxF = BBoxF { minx: f64::INFINITY, miny: f64::INFINITY, maxx: f64::NEG_INFINITY, maxy: f64::NEG_INFINITY };
    pub const WORLD: BBoxF = BBoxF { minx: -180.0, miny: -90.0, maxx: 180.0, maxy: 90.0 };

    pub fn is_empty(&self) -> bool {
        self.minx > self.maxx || self.miny > self.maxy
    }

    pub fn add(&mut self, p: P) {
        self.minx = self.minx.min(p.0);
        self.miny = self.miny.min(p.1);
        self.maxx = self.maxx.max(p.0);
        self.maxy = self.maxy.max(p.1);
    }

    pub fn union(&mut self, o: &BBoxF) {
        if o.is_empty() {
            return;
        }
        self.add((o.minx, o.miny));
        self.add((o.maxx, o.maxy));
    }

    pub fn intersects(&self, o: &BBoxF) -> bool {
        !self.is_empty() && !o.is_empty() && self.minx <= o.maxx && o.minx <= self.maxx && self.miny <= o.maxy && o.miny <= self.maxy
    }

    pub fn contains_box(&self, o: &BBoxF) -> bool {
        !o.is_empty() && self.minx <= o.minx && self.miny <= o.miny && self.maxx >= o.maxx && self.maxy >= o.maxy
    }

    pub fn contains(&self, p: P) -> bool {
        p.0 >= self.minx && p.0 <= self.maxx && p.1 >= self.miny && p.1 <= self.maxy
    }

    pub fn center(&self) -> P {
        ((self.minx + self.maxx) / 2.0, (self.miny + self.maxy) / 2.0)
    }

    pub fn expand_m(&self, m: f64) -> BBoxF {
        let dlat = m / 111_320.0;
        let lat = self.miny.abs().max(self.maxy.abs()).min(89.0).to_radians();
        let dlon = m / (111_320.0 * lat.cos().max(0.01));
        BBoxF {
            minx: (self.minx - dlon).max(-180.0),
            miny: (self.miny - dlat).max(-90.0),
            maxx: (self.maxx + dlon).min(180.0),
            maxy: (self.maxy + dlat).min(90.0),
        }
    }

    pub fn to_e7(&self) -> knut_format::BBox {
        knut_format::BBox::from_deg(self.minx, self.miny, self.maxx, self.maxy)
    }

    pub fn from_e7(b: &knut_format::BBox) -> BBoxF {
        if b.is_empty() {
            return BBoxF::EMPTY;
        }
        BBoxF { minx: b.minx as f64 * 1e-7, miny: b.miny as f64 * 1e-7, maxx: b.maxx as f64 * 1e-7, maxy: b.maxy as f64 * 1e-7 }
    }

    pub fn area_km2(&self) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        let ring = vec![(self.minx, self.miny), (self.maxx, self.miny), (self.maxx, self.maxy), (self.minx, self.maxy), (self.minx, self.miny)];
        ring_area_m2(&ring) / 1e6
    }
}

pub fn e7(p: (i32, i32)) -> P {
    (p.0 as f64 * 1e-7, p.1 as f64 * 1e-7)
}

pub fn haversine(a: P, b: P) -> f64 {
    let (la1, la2) = (a.1.to_radians(), b.1.to_radians());
    let dla = la2 - la1;
    let dlo = (b.0 - a.0).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * R * h.sqrt().min(1.0).asin()
}

fn wgs84() -> &'static Geodesic {
    static G: OnceLock<Geodesic> = OnceLock::new();
    G.get_or_init(Geodesic::wgs84)
}

/// Geodesic area of a closed ring on WGS84 in m² (absolute value).
pub fn ring_area_m2(ring: &[P]) -> f64 {
    if ring.len() < 4 {
        return 0.0;
    }
    let mut pa = PolygonArea::new(wgs84(), Winding::CounterClockwise);
    for p in &ring[..ring.len() - 1] {
        pa.add_point(p.1, p.0);
    }
    // signed result: a clockwise ring would otherwise yield the complement on the globe
    let (_, area, _) = pa.compute(true);
    area.abs()
}

pub fn line_length_m(l: &[P]) -> f64 {
    l.windows(2).map(|w| haversine(w[0], w[1])).sum()
}

impl Geom {
    pub fn bbox(&self) -> BBoxF {
        let mut b = BBoxF::EMPTY;
        self.each_point(&mut |p| b.add(p));
        b
    }

    pub fn each_point(&self, f: &mut dyn FnMut(P)) {
        match self {
            Geom::Empty => {}
            Geom::Point(p) => f(*p),
            Geom::Line(l) => l.iter().for_each(|p| f(*p)),
            Geom::Poly(ps) => {
                for p in ps {
                    p.outer.iter().for_each(|x| f(*x));
                    for h in &p.holes {
                        h.iter().for_each(|x| f(*x));
                    }
                }
            }
            Geom::Multi(gs) => gs.iter().for_each(|g| g.each_point(f)),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Geom::Empty => true,
            Geom::Line(l) => l.is_empty(),
            Geom::Poly(p) => p.is_empty(),
            Geom::Multi(g) => g.iter().all(|x| x.is_empty()),
            Geom::Point(_) => false,
        }
    }

    /// Representative point (§5.7): node position, vertex centroid of a line, point inside
    /// an area.
    pub fn rep_point(&self) -> Option<P> {
        match self {
            Geom::Empty => None,
            Geom::Point(p) => Some(*p),
            Geom::Line(l) => {
                if l.is_empty() {
                    return None;
                }
                let n = l.len() as f64;
                Some((l.iter().map(|p| p.0).sum::<f64>() / n, l.iter().map(|p| p.1).sum::<f64>() / n))
            }
            Geom::Poly(ps) => {
                let big = ps.iter().max_by(|a, b| ring_area_deg(&a.outer).partial_cmp(&ring_area_deg(&b.outer)).unwrap())?;
                Some(point_in_polygon_rep(big))
            }
            Geom::Multi(gs) => {
                // largest area member, else first member with a point
                gs.iter().find(|g| matches!(g, Geom::Poly(_))).or_else(|| gs.first()).and_then(|g| g.rep_point())
            }
        }
    }

    pub fn length_m(&self) -> f64 {
        match self {
            Geom::Line(l) => line_length_m(l),
            Geom::Poly(ps) => ps.iter().map(|p| line_length_m(&p.outer) + p.holes.iter().map(|h| line_length_m(h)).sum::<f64>()).sum(),
            Geom::Multi(gs) => gs.iter().map(|g| g.length_m()).sum(),
            _ => 0.0,
        }
    }

    pub fn area_m2(&self) -> f64 {
        match self {
            Geom::Poly(ps) => ps
                .iter()
                .map(|p| (ring_area_m2(&p.outer) - p.holes.iter().map(|h| ring_area_m2(h)).sum::<f64>()).max(0.0))
                .sum(),
            Geom::Multi(gs) => gs.iter().map(|g| g.area_m2()).sum(),
            _ => 0.0,
        }
    }

    pub fn polygons(&self) -> Vec<&Polygon> {
        match self {
            Geom::Poly(ps) => ps.iter().collect(),
            Geom::Multi(gs) => gs.iter().flat_map(|g| g.polygons()).collect(),
            _ => Vec::new(),
        }
    }

    pub fn contains_point(&self, p: P) -> bool {
        self.polygons().iter().any(|poly| poly_contains(poly, p))
    }
}

fn ring_area_deg(r: &[P]) -> f64 {
    let mut a = 0.0;
    for w in r.windows(2) {
        a += w[0].0 * w[1].1 - w[1].0 * w[0].1;
    }
    (a / 2.0).abs()
}

pub fn ring_contains(r: &[P], p: P) -> bool {
    let mut inside = false;
    let n = r.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = r[i];
        let (xj, yj) = r[j];
        if (yi > p.1) != (yj > p.1) && p.0 < (xj - xi) * (p.1 - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

pub fn poly_contains(poly: &Polygon, p: P) -> bool {
    ring_contains(&poly.outer, p) && !poly.holes.iter().any(|h| ring_contains(h, p))
}

/// A point inside the polygon: the centroid if it lies inside, otherwise the midpoint of
/// the widest inside segment of a horizontal line through the middle.
fn point_in_polygon_rep(poly: &Polygon) -> P {
    let r = &poly.outer;
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut a = 0.0;
    for w in r.windows(2) {
        let f = w[0].0 * w[1].1 - w[1].0 * w[0].1;
        cx += (w[0].0 + w[1].0) * f;
        cy += (w[0].1 + w[1].1) * f;
        a += f;
    }
    if a.abs() > 1e-18 {
        let c = (cx / (3.0 * a), cy / (3.0 * a));
        if poly_contains(poly, c) {
            return c;
        }
    }
    let b = Geom::Line(r.clone()).bbox();
    let y = (b.miny + b.maxy) / 2.0;
    let mut xs: Vec<f64> = Vec::new();
    for ring in std::iter::once(r).chain(poly.holes.iter()) {
        for w in ring.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            if (y0 > y) != (y1 > y) {
                xs.push(x0 + (y - y0) * (x1 - x0) / (y1 - y0));
            }
        }
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut best = (b.center(), -1.0);
    for pair in xs.chunks(2) {
        if pair.len() == 2 && pair[1] - pair[0] > best.1 {
            best = (((pair[0] + pair[1]) / 2.0, y), pair[1] - pair[0]);
        }
    }
    best.0
}

// ----- distances (local planar approximation in metres) -----------------------------------

struct Proj {
    lon0: f64,
    lat0: f64,
    kx: f64,
}

impl Proj {
    fn new(p: P) -> Proj {
        let k = std::f64::consts::PI / 180.0 * R;
        Proj { lon0: p.0, lat0: p.1, kx: k * p.1.to_radians().cos() }
    }

    fn xy(&self, p: P) -> (f64, f64) {
        let k = std::f64::consts::PI / 180.0 * R;
        let mut dl = p.0 - self.lon0;
        if dl > 180.0 {
            dl -= 360.0;
        } else if dl < -180.0 {
            dl += 360.0;
        }
        (dl * self.kx, (p.1 - self.lat0) * k)
    }
}

fn seg_point_dist(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let l2 = dx * dx + dy * dy;
    let t = if l2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / l2).clamp(0.0, 1.0) };
    let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - qx).powi(2) + (p.1 - qy).powi(2)).sqrt()
}

fn segs_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let o = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0);
    let (d1, d2, d3, d4) = (o(c, d, a), o(c, d, b), o(a, b, c), o(a, b, d));
    ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0))
}

/// Segments (as point lists) of a geometry; polygons contribute their rings.
fn parts(g: &Geom) -> Vec<Vec<P>> {
    match g {
        Geom::Empty => Vec::new(),
        Geom::Point(p) => vec![vec![*p]],
        Geom::Line(l) => vec![l.clone()],
        Geom::Poly(ps) => ps.iter().flat_map(|p| std::iter::once(p.outer.clone()).chain(p.holes.iter().cloned())).collect(),
        Geom::Multi(gs) => gs.iter().flat_map(parts).collect(),
    }
}

fn path_dist(pr: &Proj, a: &[P], b: &[P]) -> f64 {
    let a: Vec<(f64, f64)> = a.iter().map(|p| pr.xy(*p)).collect();
    let b: Vec<(f64, f64)> = b.iter().map(|p| pr.xy(*p)).collect();
    let mut best = f64::INFINITY;
    let segs = |v: &Vec<(f64, f64)>| -> Vec<((f64, f64), (f64, f64))> {
        if v.len() == 1 { vec![(v[0], v[0])] } else { v.windows(2).map(|w| (w[0], w[1])).collect() }
    };
    let (sa, sb) = (segs(&a), segs(&b));
    for &(p, q) in &sa {
        for &(r, s) in &sb {
            if p != q && r != s && segs_intersect(p, q, r, s) {
                return 0.0;
            }
            best = best.min(seg_point_dist(r, s, p)).min(seg_point_dist(r, s, q)).min(seg_point_dist(p, q, r)).min(seg_point_dist(p, q, s));
        }
    }
    best
}

/// Shortest distance between two geometries in metres (0 if they touch or one contains
/// the other).
pub fn distance_m(a: &Geom, b: &Geom) -> f64 {
    let (Some(ra), Some(_)) = (a.rep_point(), b.rep_point()) else { return f64::INFINITY };
    if let (Geom::Point(p), Geom::Point(q)) = (a, b) {
        return haversine(*p, *q);
    }
    // containment
    if let Some(p) = b.rep_point() {
        if a.contains_point(p) {
            return 0.0;
        }
    }
    if let Some(p) = a.rep_point() {
        if b.contains_point(p) {
            return 0.0;
        }
    }
    let (ba, bb) = (a.bbox(), b.bbox());
    let far = haversine(ba.center(), bb.center());
    if far > 200_000.0 {
        // far apart: closest vertices on the sphere are good enough
        let mut best = f64::INFINITY;
        let mut pa = Vec::new();
        a.each_point(&mut |p| pa.push(p));
        let mut pb = Vec::new();
        b.each_point(&mut |p| pb.push(p));
        let step_a = (pa.len() / 200).max(1);
        let step_b = (pb.len() / 200).max(1);
        for p in pa.iter().step_by(step_a) {
            for q in pb.iter().step_by(step_b) {
                best = best.min(haversine(*p, *q));
            }
        }
        return best;
    }
    let pr = Proj::new(ra);
    let mut best = f64::INFINITY;
    for x in parts(a) {
        for y in parts(b) {
            best = best.min(path_dist(&pr, &x, &y));
            if best == 0.0 {
                return 0.0;
            }
        }
    }
    best
}

/// True if the geometries share points; for two areas the overlap must have area (edges
/// crossing or one containing the other).
pub fn overlapping(a: &Geom, b: &Geom) -> bool {
    if !a.bbox().intersects(&b.bbox()) {
        return false;
    }
    let polys_a = a.polygons();
    let polys_b = b.polygons();
    if !polys_a.is_empty() && !polys_b.is_empty() {
        // a vertex strictly inside the other, or crossing edges
        let mut hit = false;
        a.each_point(&mut |p| {
            if !hit && polys_b.iter().any(|q| poly_contains(q, p)) {
                hit = true;
            }
        });
        if hit {
            return true;
        }
        b.each_point(&mut |p| {
            if !hit && polys_a.iter().any(|q| poly_contains(q, p)) {
                hit = true;
            }
        });
        if hit {
            return true;
        }
    }
    distance_m(a, b) == 0.0
}

// ----- multipolygon assembly --------------------------------------------------------------

/// Builds rings from way pieces `(is_inner, node ids, points)` by joining shared endpoints,
/// then assigns inner rings to the outer rings containing them.
pub fn assemble(pieces: Vec<(bool, Vec<i64>, Vec<P>)>) -> Geom {
    let mut open: Vec<(bool, Vec<i64>, Vec<P>)> = Vec::new();
    let mut rings: Vec<(bool, Vec<P>)> = Vec::new();
    for (inner, ids, pts) in pieces {
        if ids.len() < 2 || pts.len() != ids.len() {
            continue;
        }
        if ids.first() == ids.last() && ids.len() >= 4 {
            rings.push((inner, pts));
        } else {
            open.push((inner, ids, pts));
        }
    }
    // join open pieces
    while let Some((inner, mut ids, mut pts)) = open.pop() {
        loop {
            if ids.first() == ids.last() {
                break;
            }
            let end = *ids.last().unwrap();
            let start = ids[0];
            let pos = open.iter().position(|(_, o, _)| o[0] == end || *o.last().unwrap() == end || o[0] == start || *o.last().unwrap() == start);
            let Some(k) = pos else { break };
            let (_, mut oi, mut op) = open.swap_remove(k);
            if oi[0] == end {
                ids.extend_from_slice(&oi[1..]);
                pts.extend_from_slice(&op[1..]);
            } else if *oi.last().unwrap() == end {
                oi.reverse();
                op.reverse();
                ids.extend_from_slice(&oi[1..]);
                pts.extend_from_slice(&op[1..]);
            } else if *oi.last().unwrap() == start {
                oi.extend_from_slice(&ids[1..]);
                op.extend_from_slice(&pts[1..]);
                ids = oi;
                pts = op;
            } else {
                oi.reverse();
                op.reverse();
                oi.extend_from_slice(&ids[1..]);
                op.extend_from_slice(&pts[1..]);
                ids = oi;
                pts = op;
            }
        }
        if ids.first() == ids.last() && ids.len() >= 4 {
            rings.push((inner, pts));
        }
    }
    if rings.is_empty() {
        return Geom::Empty;
    }
    // nesting depth decides outer/inner when roles are missing or wrong
    let n = rings.len();
    let mut depth = vec![0usize; n];
    let bbs: Vec<BBoxF> = rings.iter().map(|r| Geom::Line(r.1.clone()).bbox()).collect();
    for i in 0..n {
        let p = rings[i].1[0];
        for j in 0..n {
            if i != j && bbs[j].contains_box(&bbs[i]) && ring_contains(&rings[j].1, p) {
                depth[i] += 1;
            }
        }
    }
    let mut polys: Vec<(usize, Polygon)> = Vec::new();
    for i in 0..n {
        if depth[i] % 2 == 0 {
            polys.push((i, Polygon { outer: rings[i].1.clone(), holes: Vec::new() }));
        }
    }
    for i in 0..n {
        if depth[i] % 2 == 1 {
            let p = rings[i].1[0];
            // innermost containing outer
            let mut best: Option<(usize, f64)> = None;
            for (k, (oi, poly)) in polys.iter().enumerate() {
                if bbs[*oi].contains_box(&bbs[i]) && ring_contains(&poly.outer, p) {
                    let a = ring_area_deg(&poly.outer);
                    if best.map_or(true, |b| a < b.1) {
                        best = Some((k, a));
                    }
                }
            }
            if let Some((k, _)) = best {
                polys[k].1.holes.push(rings[i].1.clone());
            }
        }
    }
    Geom::Poly(polys.into_iter().map(|x| x.1).collect())
}

/// WKB (little endian) for Arrow/Parquet export.
pub fn to_wkb(g: &Geom) -> Vec<u8> {
    fn pt(out: &mut Vec<u8>, p: P) {
        out.extend_from_slice(&p.0.to_le_bytes());
        out.extend_from_slice(&p.1.to_le_bytes());
    }
    fn ring(out: &mut Vec<u8>, r: &[P]) {
        out.extend_from_slice(&(r.len() as u32).to_le_bytes());
        r.iter().for_each(|p| pt(out, *p));
    }
    let mut out = vec![1u8];
    match g {
        Geom::Empty => {
            out.extend_from_slice(&7u32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        Geom::Point(p) => {
            out.extend_from_slice(&1u32.to_le_bytes());
            pt(&mut out, *p);
        }
        Geom::Line(l) => {
            out.extend_from_slice(&2u32.to_le_bytes());
            ring(&mut out, l);
        }
        Geom::Poly(ps) => {
            out.extend_from_slice(&6u32.to_le_bytes());
            out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
            for p in ps {
                out.push(1);
                out.extend_from_slice(&3u32.to_le_bytes());
                out.extend_from_slice(&(1 + p.holes.len() as u32).to_le_bytes());
                ring(&mut out, &p.outer);
                p.holes.iter().for_each(|h| ring(&mut out, h));
            }
        }
        Geom::Multi(gs) => {
            out.extend_from_slice(&7u32.to_le_bytes());
            out.extend_from_slice(&(gs.len() as u32).to_le_bytes());
            for g in gs {
                out.extend_from_slice(&to_wkb(g));
            }
        }
    }
    out
}

/// WKT for text output and CSV.
pub fn to_wkt(g: &Geom) -> String {
    fn pts(r: &[P]) -> String {
        r.iter().map(|p| format!("{} {}", fmt7(p.0), fmt7(p.1))).collect::<Vec<_>>().join(", ")
    }
    match g {
        Geom::Empty => "GEOMETRYCOLLECTION EMPTY".into(),
        Geom::Point(p) => format!("POINT ({} {})", fmt7(p.0), fmt7(p.1)),
        Geom::Line(l) => format!("LINESTRING ({})", pts(l)),
        Geom::Poly(ps) => format!(
            "MULTIPOLYGON ({})",
            ps.iter()
                .map(|p| format!("(({}){})", pts(&p.outer), p.holes.iter().map(|h| format!(", ({})", pts(h))).collect::<String>()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Geom::Multi(gs) => format!("GEOMETRYCOLLECTION ({})", gs.iter().map(to_wkt).collect::<Vec<_>>().join(", ")),
    }
}

fn fmt7(x: f64) -> String {
    let s = format!("{x:.7}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// Parses simple WKT (POINT, LINESTRING, POLYGON, MULTIPOLYGON) for datasets.
pub fn parse_wkt(s: &str) -> Option<Geom> {
    let s = s.trim();
    let up = s.to_ascii_uppercase();
    let nums = |t: &str| -> Option<Vec<P>> {
        t.split(',')
            .map(|p| {
                let mut it = p.split_whitespace();
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .collect()
    };
    fn inner(t: &str) -> Option<&str> {
        let a = t.find('(')?;
        let b = t.rfind(')')?;
        Some(&t[a + 1..b])
    }
    if up.starts_with("POINT") {
        let v = nums(inner(s)?)?;
        return v.first().map(|p| Geom::Point(*p));
    }
    if up.starts_with("LINESTRING") {
        return Some(Geom::Line(nums(inner(s)?)?));
    }
    let rings = |t: &str| -> Option<Vec<Vec<P>>> {
        let mut out = Vec::new();
        let mut rest = t;
        while let Some(a) = rest.find('(') {
            let b = rest[a..].find(')')? + a;
            out.push(nums(&rest[a + 1..b])?);
            rest = &rest[b + 1..];
        }
        Some(out)
    };
    if up.starts_with("POLYGON") {
        let mut rs = rings(inner(s)?)?;
        if rs.is_empty() {
            return None;
        }
        let outer = rs.remove(0);
        return Some(Geom::Poly(vec![Polygon { outer, holes: rs }]));
    }
    if up.starts_with("MULTIPOLYGON") {
        let body = inner(s)?;
        let mut polys = Vec::new();
        let mut depth = 0;
        let mut start = 0;
        for (i, c) in body.char_indices() {
            match c {
                '(' => {
                    if depth == 0 {
                        start = i;
                    }
                    depth += 1;
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let mut rs = rings(&body[start + 1..i])?;
                        if !rs.is_empty() {
                            let outer = rs.remove(0);
                            polys.push(Polygon { outer, holes: rs });
                        }
                    }
                }
                _ => {}
            }
        }
        return Some(Geom::Poly(polys));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let sq = vec![(0.0, 0.0), (0.01, 0.0), (0.01, 0.01), (0.0, 0.01), (0.0, 0.0)];
        let a = ring_area_m2(&sq);
        assert!((a - 1_236_000.0).abs() < 10_000.0, "{a}");
        let g = Geom::Poly(vec![Polygon { outer: sq.clone(), holes: vec![] }]);
        assert!(g.contains_point((0.005, 0.005)));
        assert_eq!(distance_m(&g, &Geom::Point((0.005, 0.005))), 0.0);
        let d = distance_m(&g, &Geom::Point((0.02, 0.005)));
        assert!((d - 1113.0).abs() < 5.0, "{d}");
        let asm = assemble(vec![
            (false, vec![1, 2, 3], vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]),
            (false, vec![3, 4, 1], vec![(1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]),
            (true, vec![5, 6, 7, 5], vec![(0.2, 0.2), (0.4, 0.2), (0.3, 0.4), (0.2, 0.2)]),
        ]);
        let Geom::Poly(ps) = &asm else { panic!() };
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].holes.len(), 1);
        assert!(!asm.contains_point((0.3, 0.25)));
        assert!(asm.contains_point((0.8, 0.8)));
        assert!(matches!(parse_wkt("POLYGON ((0 0, 1 0, 1 1, 0 0))"), Some(Geom::Poly(_))));
    }
}
