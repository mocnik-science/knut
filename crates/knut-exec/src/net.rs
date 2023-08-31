//! Networks and routing (KNUT_LANG.md §7.5): graphs built from highway ways, shortest
//! paths, multi-source searches for `nearest … by MODE`, isochrones.

use crate::geom::{self, BBoxF, Geom, P};
use crate::region::Raster;
use crate::value::Elem;
use knut_lang::ast::Mode;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Access and speed of one way for each mode.
#[derive(Clone, Copy, Debug)]
pub struct WayProfile {
    /// m/s per mode (foot, bicycle, car); 0 = not usable.
    pub speed: [f32; 3],
    /// Usable forward / backward for cars and bicycles (oneway).
    pub fwd: [bool; 3],
    pub bwd: [bool; 3],
}

fn mode_idx(m: Mode) -> usize {
    match m {
        Mode::Foot => 0,
        Mode::Bicycle => 1,
        Mode::Car | Mode::Transit => 2,
    }
}

fn tag<'a>(e: &'a Elem, k: &str) -> Option<&'a str> {
    e.tag(k).map(|s| s.as_ref())
}

fn no(v: Option<&str>) -> bool {
    matches!(v, Some("no") | Some("private"))
}

fn yes(v: Option<&str>) -> bool {
    matches!(v, Some("yes") | Some("designated") | Some("permissive"))
}

/// Default profiles (KNUT_LANG.md §7.5): walking 5 km/h, cycling 15 km/h, cars at
/// `maxspeed` or a default per road class.
pub fn profile(e: &Elem) -> Option<WayProfile> {
    let hw = tag(e, "highway")?;
    let access_no = no(tag(e, "access"));
    let kmh = |x: f32| x / 3.6;
    // foot
    let foot_class = !matches!(hw, "motorway" | "motorway_link" | "trunk" | "trunk_link" | "construction" | "proposed" | "raceway" | "bus_guideway");
    let foot = (foot_class && !access_no && !no(tag(e, "foot"))) || yes(tag(e, "foot"));
    let foot_speed = if hw == "steps" { kmh(3.0) } else { kmh(5.0) };
    // bicycle
    let bike_class = !matches!(hw, "motorway" | "motorway_link" | "trunk" | "trunk_link" | "steps" | "construction" | "proposed" | "raceway" | "bus_guideway" | "footway" | "pedestrian" | "corridor")
        || yes(tag(e, "bicycle"));
    let bike = bike_class && !access_no && !no(tag(e, "bicycle")) || yes(tag(e, "bicycle"));
    let bike_speed = if matches!(hw, "track" | "path" | "footway" | "pedestrian") { kmh(12.0) } else { kmh(15.0) };
    // car
    let default_car = match hw {
        "motorway" => 120.0,
        "motorway_link" => 60.0,
        "trunk" => 90.0,
        "trunk_link" => 50.0,
        "primary" | "primary_link" => 80.0,
        "secondary" | "secondary_link" => 70.0,
        "tertiary" | "tertiary_link" => 60.0,
        "unclassified" | "road" => 50.0,
        "residential" => 30.0,
        "living_street" => 7.0,
        "service" => 20.0,
        _ => 0.0,
    };
    let car_access = default_car > 0.0 && !access_no && !no(tag(e, "motor_vehicle")) && !no(tag(e, "motorcar"));
    let car_speed = if car_access {
        let ms = tag(e, "maxspeed").and_then(knut_lang::units::parse_value).map(|(x, u)| match u {
            Some(u) => x * u.factor * 3.6,
            None => x,
        });
        kmh(ms.filter(|x| *x > 0.0 && *x < 300.0).unwrap_or(default_car) as f32)
    } else {
        0.0
    };
    let oneway = tag(e, "oneway");
    let implied = matches!(hw, "motorway" | "motorway_link") || tag(e, "junction") == Some("roundabout");
    let (cf, cb) = match oneway {
        Some("yes") | Some("1") | Some("true") => (true, false),
        Some("-1") | Some("reverse") => (false, true),
        Some("no") => (true, true),
        _ if implied => (true, false),
        _ => (true, true),
    };
    let bike_oneway = !matches!(tag(e, "oneway:bicycle"), Some("no"));
    let (bf, bb) = if bike_oneway { (cf, cb) } else { (true, true) };
    Some(WayProfile {
        speed: [if foot { foot_speed } else { 0.0 }, if bike { bike_speed } else { 0.0 }, car_speed],
        fwd: [true, bf, cf],
        bwd: [true, bb, cb],
    })
}

#[derive(Clone, Copy, Debug)]
struct Edge {
    to: u32,
    len: f32,
    way: u32,
    /// Direction relative to the way (true = along the node order).
    fwd: bool,
}

#[derive(Debug)]
pub struct Net {
    pub pts: Vec<P>,
    adj_off: Vec<u32>,
    edges: Vec<Edge>,
    pub ways: Vec<(i64, WayProfile)>,
    grid: HashMap<(i32, i32), Vec<u32>>,
    pub bbox: BBoxF,
    /// Per mode: connected component of each node and the component sizes (lazy).
    comps: [std::sync::OnceLock<(Vec<u32>, Vec<u32>)>; 3],
}

const GRID: f64 = 0.005;

fn cell(p: P) -> (i32, i32) {
    ((p.0 / GRID).floor() as i32, (p.1 / GRID).floor() as i32)
}

#[derive(Clone, Copy, Debug)]
pub enum Metric {
    Time,
    Length,
}

#[derive(Copy, Clone, PartialEq)]
struct St(f64, u32);
impl Eq for St {}
impl PartialOrd for St {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for St {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0).then(o.1.cmp(&self.1))
    }
}

impl Net {
    /// Builds a network from ways with geometry (lines).
    pub fn build(ways: &[(std::sync::Arc<Elem>, Vec<i64>, Vec<P>)]) -> Net {
        let mut index: HashMap<i64, u32> = HashMap::default();
        let mut pts: Vec<P> = Vec::new();
        let mut raw: Vec<(u32, Edge)> = Vec::new();
        let mut wprof = Vec::new();
        let mut bbox = BBoxF::EMPTY;
        for (e, nids, line) in ways {
            let Some(p) = profile(e) else { continue };
            if nids.len() != line.len() || nids.len() < 2 {
                continue;
            }
            let wi = wprof.len() as u32;
            wprof.push((e.id, p));
            let mut prev: Option<u32> = None;
            for (nid, pt) in nids.iter().zip(line) {
                let i = *index.entry(*nid).or_insert_with(|| {
                    pts.push(*pt);
                    bbox.add(*pt);
                    (pts.len() - 1) as u32
                });
                if let Some(pi) = prev {
                    if pi != i {
                        let len = geom::haversine(pts[pi as usize], pts[i as usize]) as f32;
                        raw.push((pi, Edge { to: i, len, way: wi, fwd: true }));
                        raw.push((i, Edge { to: pi, len, way: wi, fwd: false }));
                    }
                }
                prev = Some(i);
            }
        }
        raw.sort_by_key(|(from, _)| *from);
        let mut adj_off = vec![0u32; pts.len() + 1];
        for (from, _) in &raw {
            adj_off[*from as usize + 1] += 1;
        }
        for i in 0..pts.len() {
            adj_off[i + 1] += adj_off[i];
        }
        let edges = raw.into_iter().map(|(_, e)| e).collect();
        let mut grid: HashMap<(i32, i32), Vec<u32>> = HashMap::default();
        for (i, p) in pts.iter().enumerate() {
            grid.entry(cell(*p)).or_default().push(i as u32);
        }
        Net { pts, adj_off, edges, ways: wprof, grid, bbox, comps: Default::default() }
    }

    pub fn n_edges(&self) -> usize {
        self.edges.len() / 2
    }

    /// Number of edges usable by a mode.
    pub fn usable(&self, m: Mode) -> usize {
        let i = mode_idx(m);
        self.ways.iter().filter(|w| w.1.speed[i] > 0.0).count()
    }

    fn adj(&self, n: u32) -> &[Edge] {
        &self.edges[self.adj_off[n as usize] as usize..self.adj_off[n as usize + 1] as usize]
    }

    fn cost(&self, e: &Edge, m: Mode, metric: Metric) -> Option<f64> {
        let i = mode_idx(m);
        let p = &self.ways[e.way as usize].1;
        if p.speed[i] <= 0.0 || (e.fwd && !p.fwd[i]) || (!e.fwd && !p.bwd[i]) {
            return None;
        }
        Some(match metric {
            Metric::Length => e.len as f64,
            Metric::Time => e.len as f64 / p.speed[i] as f64,
        })
    }

    /// Components of the network usable by a mode (ignoring direction).
    fn components(&self, m: Mode) -> &(Vec<u32>, Vec<u32>) {
        let i = mode_idx(m);
        self.comps[i].get_or_init(|| {
            let n = self.pts.len();
            let mut comp = vec![u32::MAX; n];
            let mut sizes = Vec::new();
            let mut stack = Vec::new();
            for s in 0..n {
                if comp[s] != u32::MAX {
                    continue;
                }
                let c = sizes.len() as u32;
                let mut size = 0u32;
                comp[s] = c;
                stack.push(s as u32);
                while let Some(u) = stack.pop() {
                    size += 1;
                    for e in self.adj(u) {
                        if self.ways[e.way as usize].1.speed[i] > 0.0 && comp[e.to as usize] == u32::MAX {
                            comp[e.to as usize] = c;
                            stack.push(e.to);
                        }
                    }
                }
                sizes.push(size);
            }
            (comp, sizes)
        })
    }

    /// Nearest network node usable by the mode, with its straight-line distance. Nodes of
    /// small isolated parts (a platform, a private yard) are skipped when a large part exists.
    pub fn snap(&self, p: P, m: Mode) -> Option<(u32, f64)> {
        let (comp, sizes) = self.components(m);
        let largest = sizes.iter().copied().max().unwrap_or(0);
        let min_size = (largest / 10).min(1000);
        let (cx, cy) = cell(p);
        let i = mode_idx(m);
        let mut best: Option<(u32, f64)> = None;
        for r in 0i32..=40 {
            for dx in -r..=r {
                for dy in -r..=r {
                    if dx.abs() != r && dy.abs() != r {
                        continue;
                    }
                    if let Some(ns) = self.grid.get(&(cx + dx, cy + dy)) {
                        for &n in ns {
                            if !self.adj(n).iter().any(|e| self.ways[e.way as usize].1.speed[i] > 0.0) || sizes[comp[n as usize] as usize] < min_size {
                                continue;
                            }
                            let d = geom::haversine(p, self.pts[n as usize]);
                            if best.map_or(true, |b| d < b.1) {
                                best = Some((n, d));
                            }
                        }
                    }
                }
            }
            if let Some(b) = best {
                if b.1 < r as f64 * GRID * 111_000.0 * 0.5 {
                    return best;
                }
            }
        }
        best
    }

    /// Dijkstra from several sources; returns cost and source label per node.
    pub fn search(&self, sources: &[(u32, f64, u32)], m: Mode, metric: Metric, limit: f64) -> (Vec<f64>, Vec<u32>, Vec<(u32, u32)>) {
        let n = self.pts.len();
        let mut dist = vec![f64::INFINITY; n];
        let mut label = vec![u32::MAX; n];
        let mut pred = vec![(u32::MAX, u32::MAX); n];
        let mut heap = BinaryHeap::new();
        for &(s, c, l) in sources {
            if c < dist[s as usize] || (c == dist[s as usize] && l < label[s as usize]) {
                dist[s as usize] = c;
                label[s as usize] = l;
                heap.push(St(c, s));
            }
        }
        while let Some(St(d, u)) = heap.pop() {
            if d > dist[u as usize] || d > limit {
                continue;
            }
            for (k, e) in self.adj(u).iter().enumerate() {
                let Some(c) = self.cost(e, m, metric) else { continue };
                let nd = d + c;
                let v = e.to as usize;
                if nd < dist[v] || (nd == dist[v] && label[u as usize] < label[v]) {
                    dist[v] = nd;
                    label[v] = label[u as usize];
                    pred[v] = (u, self.adj_off[u as usize] + k as u32);
                    heap.push(St(nd, e.to));
                }
            }
        }
        (dist, label, pred)
    }

    /// Shortest path by time: (length m, duration s, way ids, geometry).
    pub fn path(&self, a: P, b: P, m: Mode) -> Option<(f64, f64, Vec<i64>, Geom)> {
        let (sa, da) = self.snap(a, m)?;
        let (sb, db) = self.snap(b, m)?;
        let (dist, _, pred) = self.search(&[(sa, 0.0, 0)], m, Metric::Time, f64::INFINITY);
        if !dist[sb as usize].is_finite() {
            return None;
        }
        let mut len = da + db;
        let speed = match m {
            Mode::Foot => 5.0 / 3.6,
            Mode::Bicycle => 15.0 / 3.6,
            _ => 30.0 / 3.6,
        };
        let mut secs = dist[sb as usize] + (da + db) / speed;
        let mut line = vec![b, self.pts[sb as usize]];
        let mut ways = Vec::new();
        let mut cur = sb;
        while cur != sa {
            let (p, ei) = pred[cur as usize];
            if p == u32::MAX {
                break;
            }
            let e = &self.edges[ei as usize];
            len += e.len as f64;
            let wid = self.ways[e.way as usize].0;
            if ways.last() != Some(&wid) {
                ways.push(wid);
            }
            line.push(self.pts[p as usize]);
            cur = p;
        }
        line.push(a);
        line.reverse();
        ways.reverse();
        if secs < 0.0 {
            secs = 0.0;
        }
        Some((len, secs, ways, Geom::Line(line)))
    }

    /// Area reachable within `limit` (seconds or metres) as a raster of `cell` metres.
    pub fn isochrone(&self, from: P, m: Mode, metric: Metric, limit: f64, cell_m: f64) -> Option<Raster> {
        let (s, d0) = self.snap(from, m)?;
        let speed = match m {
            Mode::Foot => 5.0 / 3.6,
            Mode::Bicycle => 15.0 / 3.6,
            _ => 30.0 / 3.6,
        };
        let start = match metric {
            Metric::Time => d0 / speed,
            Metric::Length => d0,
        };
        let (dist, _, _) = self.search(&[(s, start, 0)], m, metric, limit);
        let mut r = Raster { lon0: from.0, lat0: from.1, size: cell_m, cells: HashSet::default(), bbox: BBoxF::EMPTY };
        for u in 0..self.pts.len() {
            let du = dist[u];
            if du > limit {
                continue;
            }
            for e in self.adj(u as u32) {
                let Some(c) = self.cost(e, m, metric) else { continue };
                // reachable part of the edge
                let frac = ((limit - du) / c).clamp(0.0, 1.0);
                let (pa, pb) = (self.pts[u], self.pts[e.to as usize]);
                let steps = ((e.len as f64 * frac) / (cell_m / 2.0)).ceil().max(1.0) as usize;
                for k in 0..=steps {
                    let t = frac * k as f64 / steps as f64;
                    let p = (pa.0 + (pb.0 - pa.0) * t, pa.1 + (pb.1 - pa.1) * t);
                    r.cells.insert(r.cell_of(p));
                    r.bbox.add(p);
                }
            }
        }
        // the area enclosed by reachable streets belongs to the isochrone
        r.fill_holes();
        r.bbox = r.bbox.expand_m(cell_m);
        Some(r)
    }
}
