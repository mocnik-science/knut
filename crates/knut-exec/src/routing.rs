//! The routing network as a container element: a graph of the junctions of all ways with a
//! highway tag at the last moment of the data. Points are attached to it by a small scan of
//! the ways nearby; the shapes of the ways come from the database when a route is drawn.
//! Without speed-up, routes are found by bidirectional A*; with CH or CCH (see `ch`), by
//! searches in the hierarchy.
//!
//! Travel times are whole deciseconds per edge (`RoutingNet::time`), so that all searches
//! add the same integers and agree exactly. Junctions are numbered along a Hilbert curve:
//! junctions near each other lie near each other in memory.

use crate::db::Db;
use crate::geom::{self, BBoxF, Geom, P};
use crate::net::{WayProfile, profile};
use crate::region::{Raster, Region};
use crate::scan::{self, Item, KindMask, ScanSpec, TimeMode};
use crate::value::*;
use knut_lang::ast::Mode;
use knut_plan::pushdown::TagPred;
use rayon::prelude::*;
use rustc_hash::FxHashSet as HashSet;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

pub fn mode_idx(m: Mode) -> usize {
    match m {
        Mode::Foot => 0,
        Mode::Bicycle => 1,
        Mode::Car | Mode::Transit => 2,
    }
}

/// Speed on the way from a point to the network (and back), m/s.
pub fn access_speed(m: Mode) -> f64 {
    match m {
        Mode::Foot => 5.0 / 3.6,
        Mode::Bicycle => 15.0 / 3.6,
        _ => 30.0 / 3.6,
    }
}

const ALONG: u32 = 1 << 31;

/// The junction graph.
pub struct RoutingNet {
    /// Junctions: position (1e-7°) and node id.
    pub coords: Vec<(i32, i32)>,
    pub osm: Vec<i64>,
    /// Edges by their start: target, metres, way (top bit: along the way's node order),
    /// positions of start and end in the way.
    pub off: Vec<u32>,
    pub to: Vec<u32>,
    pub len: Vec<f32>,
    pub way: Vec<u32>,
    pub span: Vec<(u16, u16)>,
    /// Ways sorted by id, with their profile.
    pub ways: Vec<(i64, WayProfile)>,
    /// Junctions along each way: (position, junction), by position.
    pub wj_off: Vec<u32>,
    pub wj: Vec<(u32, u32)>,
    /// Per mode: connected part of each junction and the sizes of the parts (lazy).
    pub comps: [std::sync::OnceLock<(Vec<u32>, Vec<u32>)>; 3],
    /// Per mode: the highest speed of any way (m/s).
    pub vmax: [f64; 3],
}

impl RoutingNet {
    /// Connected parts of the network usable by a mode (ignoring direction).
    pub fn components(&self, m: Mode) -> &(Vec<u32>, Vec<u32>) {
        self.comps[mode_idx(m)].get_or_init(|| {
            let n = self.n();
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
                    for e in self.edges(u) {
                        let v = self.to[e];
                        if comp[v as usize] == u32::MAX && (self.time(e, m, false).is_some() || self.time(e, m, true).is_some()) {
                            comp[v as usize] = c;
                            stack.push(v);
                        }
                    }
                }
                sizes.push(size);
            }
            (comp, sizes)
        })
    }

    /// True if the junction lies in a part large enough to start from (small parts, such
    /// as a platform or a private yard, are skipped when a large part exists).
    pub fn well_connected(&self, v: u32, m: Mode) -> bool {
        let (comp, sizes) = self.components(m);
        let largest = sizes.iter().copied().max().unwrap_or(0);
        sizes[comp[v as usize] as usize] >= (largest / 10).min(1000)
    }
    pub fn n(&self) -> usize {
        self.coords.len()
    }

    pub fn pt(&self, v: u32) -> P {
        let (x, y) = self.coords[v as usize];
        (x as f64 * 1e-7, y as f64 * 1e-7)
    }

    pub fn edges(&self, u: u32) -> std::ops::Range<usize> {
        self.off[u as usize] as usize..self.off[u as usize + 1] as usize
    }

    /// Deciseconds to use edge `e` in its direction (or against it), if allowed: rounded
    /// to the nearest, but never below the time at the highest speed of the mode (rounded
    /// up), so that the A* estimates stay below every edge.
    pub fn time(&self, e: usize, m: Mode, reverse: bool) -> Option<u32> {
        let w = self.way[e];
        let p = &self.ways[(w & !ALONG) as usize].1;
        let along = (w & ALONG != 0) != reverse;
        let i = mode_idx(m);
        if p.speed[i] <= 0.0 || (along && !p.fwd[i]) || (!along && !p.bwd[i]) {
            return None;
        }
        let len = self.len[e] as f64;
        Some(((10.0 * len / p.speed[i] as f64).round()).max((10.0 * len / self.vmax[i]).ceil()) as u32)
    }

    pub fn max_speed(&self, m: Mode) -> f64 {
        self.vmax[mode_idx(m)]
    }

    fn with_vmax(mut self) -> RoutingNet {
        for i in 0..3 {
            self.vmax[i] = self.ways.iter().map(|w| w.1.speed[i] as f64).fold(0.1, f64::max);
        }
        self
    }

    fn way_index(&self, id: i64) -> Option<usize> {
        self.ways.binary_search_by_key(&id, |w| w.0).ok()
    }

    fn way_junctions(&self, w: usize) -> &[(u32, u32)] {
        &self.wj[self.wj_off[w] as usize..self.wj_off[w + 1] as usize]
    }
}

// ----- building -----------------------------------------------------------------------------

#[derive(Default)]
struct Part {
    ways: Vec<(i64, WayProfile)>,
    off: Vec<u32>,
    nids: Vec<i64>,
    pts: Vec<(i32, i32)>,
}

/// The network of all ways with a highway tag at the last moment of the data.
pub fn build_network(db: &Db, ctl: Option<&crate::control::Control>) -> R<RoutingNet> {
    let spec = ScanSpec {
        bbox: None,
        kinds: KindMask { way: true, ..Default::default() },
        time: TimeMode::State(db.latest),
        region: None,
        pred: TagPred::Has("highway".into()),
        need_geom: true,
        need_hist: false,
    };
    let parts = scan::fold(db, ctl, &spec, &Part::default, &|acc: &mut Part, it: Item| {
        let Item::Elem(e) = it else { return Ok(()) };
        let Some(p) = profile(&e) else { return Ok(()) };
        let Some(g) = e.geom.get() else { return Ok(()) };
        let Geom::Line(line) = &**g else { return Ok(()) };
        if line.len() != e.v.nodes.len() || line.len() < 2 || line.len() > u16::MAX as usize {
            return Ok(());
        }
        acc.ways.push((e.id, p));
        acc.off.push(acc.nids.len() as u32);
        acc.nids.extend(e.v.nodes.iter());
        acc.pts.extend(line.iter().map(|q| ((q.0 * 1e7).round() as i32, (q.1 * 1e7).round() as i32)));
        Ok(())
    })?;
    // all ways in id order, flat
    let mut order: Vec<(i64, usize, usize)> = Vec::new();
    for (pi, p) in parts.iter().enumerate() {
        for (wi, w) in p.ways.iter().enumerate() {
            order.push((w.0, pi, wi));
        }
    }
    order.par_sort_unstable();
    order.dedup_by_key(|x| x.0);
    let node_list = |pi: usize, wi: usize| -> (&[i64], &[(i32, i32)]) {
        let p = &parts[pi];
        let a = p.off[wi] as usize;
        let z = p.off.get(wi + 1).map_or(p.nids.len(), |x| *x as usize);
        (&p.nids[a..z], &p.pts[a..z])
    };
    // junctions: ends of ways and nodes used more than once
    let mut all: Vec<i64> = Vec::new();
    let mut ends: Vec<i64> = Vec::new();
    for &(_, pi, wi) in &order {
        let (ns, _) = node_list(pi, wi);
        all.extend_from_slice(ns);
        ends.push(ns[0]);
        ends.push(ns[ns.len() - 1]);
    }
    all.par_sort_unstable();
    let mut junctions: Vec<i64> = ends;
    let mut i = 0;
    while i < all.len() {
        let mut j = i + 1;
        while j < all.len() && all[j] == all[i] {
            j += 1;
        }
        if j - i >= 2 {
            junctions.push(all[i]);
        }
        i = j;
    }
    drop(all);
    junctions.par_sort_unstable();
    junctions.dedup();
    let jix = |id: i64| junctions.binary_search(&id).ok().map(|x| x as u32);
    let mut coords = vec![(0i32, 0i32); junctions.len()];
    let mut ways = Vec::with_capacity(order.len());
    let mut wj_off = vec![0u32];
    let mut wj = Vec::new();
    // directed edges: (from, to, len, way|along, span)
    let mut raw: Vec<(u32, u32, f32, u32, (u16, u16))> = Vec::new();
    for (w, &(id, pi, wi)) in order.iter().enumerate() {
        let (ns, ps) = node_list(pi, wi);
        ways.push((id, parts[pi].ways[wi].1));
        let mut last: Option<(usize, u32)> = None;
        let mut acc = 0.0f64;
        for k in 0..ns.len() {
            if k > 0 {
                let (a, b) = (ps[k - 1], ps[k]);
                acc += geom::haversine((a.0 as f64 * 1e-7, a.1 as f64 * 1e-7), (b.0 as f64 * 1e-7, b.1 as f64 * 1e-7));
            }
            let Some(j) = jix(ns[k]) else { continue };
            coords[j as usize] = ps[k];
            wj.push((k as u32, j));
            if let Some((lk, lj)) = last
                && lj != j
            {
                let l = acc as f32;
                raw.push((lj, j, l, w as u32 | ALONG, (lk as u16, k as u16)));
                raw.push((j, lj, l, w as u32, (k as u16, lk as u16)));
            }
            last = Some((k, j));
            acc = 0.0;
        }
        wj_off.push(wj.len() as u32);
    }
    // number the junctions along a Hilbert curve (ties by node id)
    let nj = junctions.len();
    let mut by_curve: Vec<(u32, u32)> = (0..nj as u32).map(|j| (hilbert(coords[j as usize]), j)).collect();
    by_curve.par_sort_unstable();
    let mut new_id = vec![0u32; nj];
    for (k, &(_, j)) in by_curve.iter().enumerate() {
        new_id[j as usize] = k as u32;
    }
    let coords: Vec<(i32, i32)> = by_curve.iter().map(|&(_, j)| coords[j as usize]).collect();
    let junctions: Vec<i64> = by_curve.iter().map(|&(_, j)| junctions[j as usize]).collect();
    drop(by_curve);
    for x in wj.iter_mut() {
        x.1 = new_id[x.1 as usize];
    }
    raw.par_iter_mut().for_each(|e| {
        e.0 = new_id[e.0 as usize];
        e.1 = new_id[e.1 as usize];
    });
    raw.par_sort_by_key(|e| e.0);
    let mut off = vec![0u32; junctions.len() + 1];
    for e in &raw {
        off[e.0 as usize + 1] += 1;
    }
    for i in 0..junctions.len() {
        off[i + 1] += off[i];
    }
    Ok(RoutingNet {
        comps: Default::default(),
        vmax: [0.0; 3],
        coords,
        osm: junctions,
        off,
        to: raw.iter().map(|e| e.1).collect(),
        len: raw.iter().map(|e| e.2).collect(),
        way: raw.iter().map(|e| e.3).collect(),
        span: raw.iter().map(|e| e.4).collect(),
        ways,
        wj_off,
        wj,
    }
    .with_vmax())
}

/// Position on a Hilbert curve over the globe (2^16 steps per axis).
fn hilbert((x, y): (i32, i32)) -> u32 {
    let n: u32 = 1 << 16;
    let mut px = (((x as i64 + 1_800_000_000) * n as i64) / 3_600_000_001) as u32;
    let mut py = (((y as i64 + 900_000_000) * n as i64) / 1_800_000_001) as u32;
    let mut d: u32 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = ((px & s) > 0) as u32;
        let ry = ((py & s) > 0) as u32;
        d = d.wrapping_add(s.wrapping_mul(s).wrapping_mul((3 * rx) ^ ry));
        if ry == 0 {
            if rx == 1 {
                px = n - 1 - px;
                py = n - 1 - py;
            }
            std::mem::swap(&mut px, &mut py);
        }
        s /= 2;
    }
    d
}

// ----- storage ------------------------------------------------------------------------------

const MAGIC: &[u8; 8] = b"KROUTE01";

fn put<T: Copy>(b: &mut Vec<u8>, xs: &[T]) {
    b.extend_from_slice(&(xs.len() as u64).to_le_bytes());
    // SAFETY: plain old data (integers, floats and tuples of them), written as bytes.
    let bytes = unsafe { std::slice::from_raw_parts(xs.as_ptr() as *const u8, std::mem::size_of_val(xs)) };
    b.extend_from_slice(bytes);
    while b.len() % 8 != 0 {
        b.push(0);
    }
}

fn take<T: Copy + Default>(b: &[u8], o: &mut usize) -> Result<Vec<T>, String> {
    let n = u64::from_le_bytes(b.get(*o..*o + 8).ok_or("routing network truncated")?.try_into().unwrap()) as usize;
    *o += 8;
    let size = n.checked_mul(std::mem::size_of::<T>()).ok_or("routing network corrupt")?;
    let src = b.get(*o..*o + size).ok_or("routing network truncated")?;
    let mut v = vec![T::default(); n];
    // SAFETY: `v` has room for exactly `size` bytes; T is plain old data.
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), v.as_mut_ptr() as *mut u8, size) };
    *o += size;
    *o = o.div_ceil(8) * 8;
    Ok(v)
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct WayRec {
    id: i64,
    speed: [f32; 3],
    flags: u32,
}

pub fn encode(n: &RoutingNet) -> Vec<u8> {
    let mut b = MAGIC.to_vec();
    put(&mut b, &n.coords);
    put(&mut b, &n.osm);
    put(&mut b, &n.off);
    put(&mut b, &n.to);
    put(&mut b, &n.len);
    put(&mut b, &n.way);
    put(&mut b, &n.span);
    let ways: Vec<WayRec> = n
        .ways
        .iter()
        .map(|(id, p)| {
            let mut flags = 0;
            for i in 0..3 {
                flags |= (p.fwd[i] as u32) << i | (p.bwd[i] as u32) << (i + 3);
            }
            WayRec { id: *id, speed: p.speed, flags }
        })
        .collect();
    put(&mut b, &ways);
    put(&mut b, &n.wj_off);
    put(&mut b, &n.wj);
    b
}

pub fn decode(b: &[u8]) -> Result<RoutingNet, String> {
    if b.get(0..8) != Some(MAGIC) {
        return Err("not a routing network".into());
    }
    let mut o = 8;
    let n = RoutingNet {
        comps: Default::default(),
        vmax: [0.0; 3],
        coords: take(b, &mut o)?,
        osm: take(b, &mut o)?,
        off: take(b, &mut o)?,
        to: take(b, &mut o)?,
        len: take(b, &mut o)?,
        way: take(b, &mut o)?,
        span: take(b, &mut o)?,
        ways: take::<WayRec>(b, &mut o)?
            .into_iter()
            .map(|w| {
                let f = |i: u32| w.flags & (1 << i) != 0;
                (w.id, WayProfile { speed: w.speed, fwd: [f(0), f(1), f(2)], bwd: [f(3), f(4), f(5)] })
            })
            .collect(),
        wj_off: take(b, &mut o)?,
        wj: take(b, &mut o)?,
    };
    let (nn, ne) = (n.coords.len(), n.to.len());
    if n.osm.len() != nn || n.off.len() != nn + 1 || [n.len.len(), n.way.len(), n.span.len()].iter().any(|&x| x != ne)
        || n.off.last().copied() != Some(ne as u32) || n.wj_off.len() != n.ways.len() + 1
        || n.to.iter().any(|&t| t as usize >= nn) || n.way.iter().any(|&w| (w & !ALONG) as usize >= n.ways.len())
    {
        return Err("routing network corrupt".into());
    }
    Ok(n.with_vmax())
}

// ----- attaching points -------------------------------------------------------------------

/// Where a point meets the network: the way, the projected point, its distance, and the
/// junctions before and after it on the way with the metres to them.
#[derive(Clone, Debug)]
pub struct Snap {
    pub way: usize,
    pub point: P,
    pub dist: f64,
    pub seg: usize,
    pub before: (u32, u32, f64),
    pub after: (u32, u32, f64),
}

/// The nearest way usable by the mode, found by a scan of the ways around the point.
pub fn snap(db: &Db, net: &RoutingNet, p: P, m: Mode) -> Option<Snap> {
    // the same point is often attached again (several isochrones or routes from it)
    type Key = (std::path::PathBuf, usize, u64, u64, usize);
    static CACHE: std::sync::Mutex<Vec<(Key, Option<Snap>)>> = std::sync::Mutex::new(Vec::new());
    let key: Key = (db.path.clone(), net.n(), p.0.to_bits(), p.1.to_bits(), mode_idx(m));
    if let Some(x) = CACHE.lock().ok().and_then(|c| c.iter().find(|x| x.0 == key).map(|x| x.1.clone())) {
        return x;
    }
    let found = snap_scan(db, net, p, m);
    if let Ok(mut c) = CACHE.lock() {
        if c.len() >= 256 {
            c.remove(0);
        }
        c.push((key, found.clone()));
    }
    found
}

fn snap_scan(db: &Db, net: &RoutingNet, p: P, m: Mode) -> Option<Snap> {
    let mi = mode_idx(m);
    let mut r = 250.0;
    while r <= 20_000.0 {
        let mut bb = BBoxF::EMPTY;
        bb.add(p);
        let spec = ScanSpec {
            bbox: None,
            kinds: KindMask { way: true, ..Default::default() },
            time: TimeMode::State(db.latest),
            region: Some(Arc::new(Region::Box(bb.expand_m(r)))),
            pred: TagPred::Has("highway".into()),
            need_geom: true,
            need_hist: false,
        };
        let found = scan::fold(db, None, &spec, &Vec::new, &|acc: &mut Vec<(f64, i64, usize, P)>, it: Item| {
            let Item::Elem(e) = it else { return Ok(()) };
            if profile(&e).is_none_or(|pr| pr.speed[mi] <= 0.0) {
                return Ok(());
            }
            let Some(g) = e.geom.get() else { return Ok(()) };
            let Geom::Line(line) = &**g else { return Ok(()) };
            if line.len() != e.v.nodes.len() {
                return Ok(());
            }
            for k in 0..line.len().saturating_sub(1) {
                let q = project(p, line[k], line[k + 1]);
                acc.push((geom::haversine(p, q), e.id, k, q));
            }
            Ok(())
        })
        .ok()?;
        let mut cands: Vec<(f64, i64, usize, P)> = found.into_iter().flatten().filter(|c| c.0 <= r).collect();
        cands.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        for (d, id, k, q) in cands {
            let Some(w) = net.way_index(id) else { continue };
            let js = net.way_junctions(w);
            let Some(b) = js.iter().rev().find(|x| x.0 as usize <= k) else { continue };
            let Some(a) = js.iter().find(|x| x.0 as usize > k) else { continue };
            if !net.well_connected(b.1, m) && !net.well_connected(a.1, m) {
                continue;
            }
            // metres along the way: from the junction before to the point, and on
            let geo = db.way_geoms(&[(id, db.latest)]);
            let Some(Some((_, pts))) = geo.get(&(id, db.latest)) else { continue };
            let pts: Vec<P> = pts.iter().map(|x| x.unwrap_or(q)).collect();
            let along = |from: usize, to: usize| (from..to).map(|i| geom::haversine(pts[i], pts[i + 1])).sum::<f64>();
            let db_ = along(b.0 as usize, k) + geom::haversine(pts[k], q);
            let da = geom::haversine(q, pts[k + 1]) + along(k + 1, a.0 as usize);
            return Some(Snap { way: w, point: q, dist: d, seg: k, before: (b.1, b.0, db_), after: (a.1, a.0, da) });
        }
        r *= 4.0;
    }
    None
}

fn project(p: P, a: P, b: P) -> P {
    let k = p.1.to_radians().cos();
    let (ax, ay, bx, by, px, py) = (a.0 * k, a.1, b.0 * k, b.1, p.0 * k, p.1);
    let (dx, dy) = (bx - ax, by - ay);
    let l = dx * dx + dy * dy;
    let t = if l > 0.0 { (((px - ax) * dx + (py - ay) * dy) / l).clamp(0.0, 1.0) } else { 0.0 };
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// Junctions reachable from a snapped point (as a start) or reaching it (as an end), with
/// the deciseconds between point and junction (whole numbers).
pub fn entries(net: &RoutingNet, s: &Snap, m: Mode, start: bool) -> Vec<(u32, f64)> {
    let p = &net.ways[s.way].1;
    let i = mode_idx(m);
    let sp = p.speed[i] as f64;
    if sp <= 0.0 {
        return Vec::new();
    }
    let access = s.dist / access_speed(m);
    let mut out = Vec::new();
    // to the junction before: against the way's order when starting, along it when ending
    if (start && p.bwd[i]) || (!start && p.fwd[i]) {
        out.push((s.before.0, (10.0 * (access + s.before.2 / sp)).round()));
    }
    if (start && p.fwd[i]) || (!start && p.bwd[i]) {
        out.push((s.after.0, (10.0 * (access + s.after.2 / sp)).round()));
    }
    out
}

// ----- searches -------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq)]
pub struct St(pub f64, pub u32);
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

/// A route through the junction graph: the edges in order (edge index, used against its
/// direction) and the total seconds between the two points.
pub struct Route {
    pub seconds: f64,
    pub edges: Vec<(usize, bool)>,
    pub from: Snap,
    pub to: Snap,
    /// Both points on the same edge, connected directly.
    pub direct: bool,
}

/// Direct connection of two points on the same piece of a way, if allowed (deciseconds).
pub fn direct(net: &RoutingNet, a: &Snap, b: &Snap, m: Mode) -> Option<f64> {
    if a.way != b.way || a.before.1 != b.before.1 {
        return None;
    }
    let p = &net.ways[a.way].1;
    let i = mode_idx(m);
    let along = (b.seg, b.before.2) >= (a.seg, a.before.2);
    if p.speed[i] <= 0.0 || (along && !p.fwd[i]) || (!along && !p.bwd[i]) {
        return None;
    }
    Some((10.0 * ((a.dist + b.dist) / access_speed(m) + (b.before.2 - a.before.2).abs() / p.speed[i] as f64)).round())
}

/// Bidirectional A* from `a` to `b` (exact; average potentials).
pub fn astar(net: &RoutingNet, a: &Snap, b: &Snap, m: Mode) -> Option<Route> {
    let n = net.n();
    let vmax = net.max_speed(m);
    let (ps, pt) = (a.point, b.point);
    // deciseconds at the highest speed (average of both directions' estimates)
    let pot = |v: u32| 10.0 * (geom::haversine(net.pt(v), pt) - geom::haversine(ps, net.pt(v))) / (2.0 * vmax);
    let mut df: Vec<f64> = vec![f64::INFINITY; n];
    let mut dr: Vec<f64> = vec![f64::INFINITY; n];
    let mut pf: Vec<u32> = vec![u32::MAX; n];
    let mut pr: Vec<u32> = vec![u32::MAX; n];
    let (mut hf, mut hr) = (BinaryHeap::new(), BinaryHeap::new());
    for (v, c) in entries(net, a, m, true) {
        if c < df[v as usize] {
            df[v as usize] = c;
            hf.push(St(c + pot(v), v));
        }
    }
    for (v, c) in entries(net, b, m, false) {
        if c < dr[v as usize] {
            dr[v as usize] = c;
            hr.push(St(c - pot(v), v));
        }
    }
    let mut best = direct(net, a, b, m).unwrap_or(f64::INFINITY);
    let mut meet = u32::MAX;
    // a junction that is both a start and an end entry
    for (v, _) in entries(net, a, m, true) {
        if df[v as usize] + dr[v as usize] < best {
            best = df[v as usize] + dr[v as usize];
            meet = v;
        }
    }
    loop {
        let kf = hf.peek().map_or(f64::INFINITY, |x: &St| x.0);
        let kr = hr.peek().map_or(f64::INFINITY, |x: &St| x.0);
        // keys: d_f + pot and d_r - pot; their sum bounds every route through unsettled nodes
        if kf + kr >= best - 1e-9 || (!kf.is_finite() && !kr.is_finite()) {
            break;
        }
        if kf <= kr {
            let St(k, u) = hf.pop().unwrap();
            if k > df[u as usize] + pot(u) + 1e-9 {
                continue;
            }
            for e in net.edges(u) {
                let Some(c) = net.time(e, m, false) else { continue };
                let v = net.to[e];
                let nd = df[u as usize] + c as f64;
                if nd < df[v as usize] {
                    df[v as usize] = nd;
                    pf[v as usize] = e as u32;
                    hf.push(St(nd + pot(v), v));
                    if nd + dr[v as usize] < best {
                        best = nd + dr[v as usize];
                        meet = v;
                    }
                }
            }
        } else {
            let St(k, u) = hr.pop().unwrap();
            if k > dr[u as usize] - pot(u) + 1e-9 {
                continue;
            }
            for e in net.edges(u) {
                // the edge u -> v used from v to u
                let Some(c) = net.time(e, m, true) else { continue };
                let v = net.to[e];
                let nd = dr[u as usize] + c as f64;
                if nd < dr[v as usize] {
                    dr[v as usize] = nd;
                    pr[v as usize] = e as u32;
                    hr.push(St(nd - pot(v), v));
                    if df[v as usize] + nd < best {
                        best = df[v as usize] + nd;
                        meet = v;
                    }
                }
            }
        }
    }
    if !best.is_finite() {
        return None;
    }
    if meet == u32::MAX {
        return Some(Route { seconds: best / 10.0, edges: Vec::new(), from: a.clone(), to: b.clone(), direct: true });
    }
    // edges from the start to `meet` (forward), then from `meet` to the end (backward)
    let mut fwd = Vec::new();
    let mut v = meet;
    while pf[v as usize] != u32::MAX {
        let e = pf[v as usize] as usize;
        fwd.push((e, false));
        v = source_of(net, e);
    }
    fwd.reverse();
    let mut v = meet;
    while pr[v as usize] != u32::MAX {
        let e = pr[v as usize] as usize;
        fwd.push((e, true));
        v = source_of(net, e);
    }
    Some(Route { seconds: best / 10.0, edges: fwd, from: a.clone(), to: b.clone(), direct: false })
}

/// The start of edge `e` (edges are sorted by start).
pub fn source_of(net: &RoutingNet, e: usize) -> u32 {
    (net.off.partition_point(|&o| o as usize <= e) - 1) as u32
}

/// Length, geometry and ways of a route; shapes from the database.
pub fn draw(db: &Db, net: &RoutingNet, r: &Route, m: Mode) -> (f64, Vec<i64>, Geom) {
    let _ = m;
    let mut wanted: Vec<i64> = r.edges.iter().map(|&(e, _)| net.ways[(net.way[e] & !ALONG) as usize].0).collect();
    wanted.push(net.ways[r.from.way].0);
    wanted.push(net.ways[r.to.way].0);
    wanted.sort_unstable();
    wanted.dedup();
    let geo = db.way_geoms(&wanted.iter().map(|&w| (w, db.latest)).collect::<Vec<_>>());
    let shape = |w: usize| -> Vec<P> {
        geo.get(&(net.ways[w].0, db.latest)).and_then(|x| x.as_ref()).map(|(_, p)| p.iter().map(|x| x.unwrap_or((0.0, 0.0))).collect()).unwrap_or_default()
    };
    let mut line: Vec<P> = Vec::new();
    let mut len = 0.0;
    let mut ways: Vec<i64> = Vec::new();
    let push = |pts: &[P], line: &mut Vec<P>| {
        for p in pts {
            if line.last() != Some(p) {
                line.push(*p);
            }
        }
    };
    // from the point to its first junction
    line.push(r.from.point);
    if r.direct {
        let pts = shape(r.from.way);
        let (a, b) = (r.from.seg, r.to.seg);
        if a <= b {
            push(&pts[(a + 1).min(pts.len())..(b + 1).min(pts.len())], &mut line);
        } else {
            let mut seg: Vec<P> = pts[(b + 1).min(pts.len())..(a + 1).min(pts.len())].to_vec();
            seg.reverse();
            push(&seg, &mut line);
        }
        line.push(r.to.point);
        ways.push(net.ways[r.from.way].0);
        len = geom::Geom::Line(line.clone()).length_m();
        return (len + r.from.dist + r.to.dist, ways, Geom::Line(line));
    }
    let first_junction = r.edges.first().map(|&(e, rev)| if rev { net.to[e] } else { source_of(net, e) });
    let last_junction = r.edges.last().map(|&(e, rev)| if rev { source_of(net, e) } else { net.to[e] });
    let piece_to_junction = |s: &Snap, j: Option<u32>, start: bool| -> Vec<P> {
        let pts = shape(s.way);
        let Some(j) = j else { return Vec::new() };
        let mut v: Vec<P> = if j == s.before.0 {
            let mut x = pts[s.before.1 as usize..=s.seg.min(pts.len().saturating_sub(1))].to_vec();
            x.reverse();
            x
        } else {
            pts[(s.seg + 1).min(pts.len())..=(s.after.1 as usize).min(pts.len().saturating_sub(1))].to_vec()
        };
        if !start {
            v.reverse();
        }
        v
    };
    push(&piece_to_junction(&r.from, first_junction, true), &mut line);
    ways.push(net.ways[r.from.way].0);
    for &(e, rev) in &r.edges {
        let w = (net.way[e] & !ALONG) as usize;
        let pts = shape(w);
        let (a, b) = net.span[e];
        let (a, b) = if rev { (b, a) } else { (a, b) };
        let seg: Vec<P> = if a <= b {
            pts[a as usize..=(b as usize).min(pts.len().saturating_sub(1))].to_vec()
        } else {
            let mut x = pts[b as usize..=(a as usize).min(pts.len().saturating_sub(1))].to_vec();
            x.reverse();
            x
        };
        push(&seg, &mut line);
        len += net.len[e] as f64;
        if ways.last() != Some(&net.ways[w].0) {
            ways.push(net.ways[w].0);
        }
    }
    push(&piece_to_junction(&r.to, last_junction, false), &mut line);
    if ways.last() != Some(&net.ways[r.to.way].0) {
        ways.push(net.ways[r.to.way].0);
    }
    line.push(r.to.point);
    let along = |s: &Snap, j: Option<u32>| match j {
        Some(j) if j == s.before.0 => s.before.2,
        Some(_) => s.after.2,
        None => 0.0,
    };
    len += along(&r.from, first_junction) + along(&r.to, last_junction) + r.from.dist + r.to.dist;
    (len, ways, Geom::Line(line))
}

/// Deciseconds from the start entries to every junction, up to `limit` seconds (Dijkstra).
pub fn reach(net: &RoutingNet, sources: &[(u32, f64)], m: Mode, limit: f64) -> Vec<f64> {
    reach_bounded(net, sources, m, limit, usize::MAX).map(|x| x.0).unwrap_or_default()
}

/// The same, with the junctions reached (in the order of their times); None once more than
/// `cap` junctions are settled (then a sweep over all junctions, PHAST, is faster).
pub fn reach_bounded(net: &RoutingNet, sources: &[(u32, f64)], m: Mode, limit: f64, cap: usize) -> Option<(Vec<f64>, Vec<u32>)> {
    let limit = limit * 10.0;
    let mut settled: Vec<u32> = Vec::new();
    let mut d = vec![f64::INFINITY; net.n()];
    let mut h = BinaryHeap::new();
    for &(v, c) in sources {
        if c < d[v as usize] {
            d[v as usize] = c;
            h.push(St(c, v));
        }
    }
    while let Some(St(k, u)) = h.pop() {
        if k > d[u as usize] || k > limit {
            continue;
        }
        settled.push(u);
        if settled.len() > cap {
            return None;
        }
        for e in net.edges(u) {
            let Some(c) = net.time(e, m, false) else { continue };
            let (v, c) = (net.to[e], c as f64);
            if k + c < d[v as usize] {
                d[v as usize] = k + c;
                h.push(St(k + c, v));
            }
        }
    }
    Some((d, settled))
}

/// The area reachable within `limit` seconds as a raster: the reachable parts of the edges
/// and what they enclose; `dist` in deciseconds, `reached` the junctions within the limit if
/// known (else all are looked at). An edge is drawn as a straight line between its junctions
/// when its shape cannot leave that line by more than half a cell (a way of length L between
/// points c apart stays within ½·√(L² − c²) of their line); only the other ways are read
/// for their shapes.
#[allow(clippy::too_many_arguments)]
pub fn isochrone_raster(db: &Db, net: &RoutingNet, from: &Snap, dist: &[f64], reached: Option<&[u32]>, m: Mode, limit: f64, cell_m: f64) -> Raster {
    let mut r = Raster { lon0: from.point.0, lat0: from.point.1, size: cell_m, cells: HashSet::default(), bbox: BBoxF::EMPTY };
    let limit_ds = limit * 10.0;
    let all: Vec<u32>;
    let junctions = match reached {
        Some(r) => r,
        None => {
            all = (0..net.n() as u32).collect();
            &all
        }
    };
    // In parallel, over runs of junctions (near each other, as they are numbered along a
    // Hilbert curve): the cell of every reached junction, the cells along the reachable part
    // of every edge that is longer than half a cell or reachable only in part (an edge fully
    // reachable and shorter only touches the cells of its junctions), straight where its
    // shape stays within half a cell of the line, else noted to be drawn along its shape.
    let half = cell_m / 2.0;
    let r0 = &r;
    let (cells, bbox, parts) = junctions
        .par_chunks(16_384)
        .map(|chunk| {
            let mut cells: HashSet<(i32, i32)> = HashSet::default();
            let mut bbox = BBoxF::EMPTY;
            let mut parts: Vec<(usize, u16, u16, f64)> = Vec::new();
            let mut mark = |a: P, b: P, frac: f64, len: f64| {
                let steps = (len * frac / half).ceil().max(1.0) as usize;
                for k in 0..=steps {
                    let t = frac * k as f64 / steps as f64;
                    let p = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                    cells.insert(r0.cell_of(p));
                    bbox.add(p);
                }
            };
            let mut parts_of = Vec::new();
            for &u in chunk {
                let du = dist[u as usize];
                if du > limit_ds {
                    continue;
                }
                let pu = net.pt(u);
                mark(pu, pu, 1.0, 0.0);
                for e in net.edges(u) {
                    let Some(c) = net.time(e, m, false) else { continue };
                    let frac = if c == 0 { 1.0 } else { ((limit_ds - du) / c as f64).clamp(0.0, 1.0) };
                    let l = net.len[e] as f64;
                    if frac >= 1.0 && l <= half {
                        continue;
                    }
                    let pv = net.pt(net.to[e]);
                    let chord = geom::haversine(pu, pv);
                    if 0.5 * (l * l - chord * chord).max(0.0).sqrt() <= half {
                        mark(pu, pv, frac, chord);
                    } else {
                        parts_of.push((e, net.span[e].0, net.span[e].1, frac));
                    }
                }
            }
            parts.append(&mut parts_of);
            (cells, bbox, parts)
        })
        .reduce(
            || (HashSet::default(), BBoxF::EMPTY, Vec::new()),
            |mut a, mut b| {
                if a.0.len() < b.0.len() {
                    std::mem::swap(&mut a, &mut b);
                }
                a.0.extend(b.0);
                if !b.1.is_empty() {
                    a.1.add((b.1.minx, b.1.miny));
                    a.1.add((b.1.maxx, b.1.maxy));
                }
                a.2.append(&mut b.2);
                a
            },
        );
    r.cells = cells;
    r.bbox = bbox;
    let mut ids: Vec<i64> = parts.iter().map(|p| net.ways[(net.way[p.0] & !ALONG) as usize].0).collect();
    ids.push(net.ways[from.way].0);
    ids.sort_unstable();
    ids.dedup();
    let geo = db.way_geoms(&ids.iter().map(|&w| (w, db.latest)).collect::<Vec<_>>());
    let mark = |a: P, b: P, frac: f64, r: &mut Raster| {
        let len = geom::haversine(a, b) * frac;
        let steps = (len / (cell_m / 2.0)).ceil().max(1.0) as usize;
        for k in 0..=steps {
            let t = frac * k as f64 / steps as f64;
            let p = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            r.cells.insert(r.cell_of(p));
            r.bbox.add(p);
        }
    };
    for (e, a, b, frac) in parts {
        let w = net.ways[(net.way[e] & !ALONG) as usize].0;
        let Some(Some((_, pts))) = geo.get(&(w, db.latest)) else { continue };
        let pts: Vec<P> = pts.iter().flatten().copied().collect();
        let (lo, hi) = (a.min(b) as usize, (a.max(b) as usize).min(pts.len().saturating_sub(1)));
        let mut seg: Vec<P> = pts.get(lo..=hi).map(|x| x.to_vec()).unwrap_or_default();
        if a > b {
            seg.reverse();
        }
        // the reachable fraction of the edge, along its shape
        let total: f64 = seg.windows(2).map(|x| geom::haversine(x[0], x[1])).sum();
        let mut left = total * frac;
        for x in seg.windows(2) {
            if left <= 0.0 {
                break;
            }
            let l = geom::haversine(x[0], x[1]);
            mark(x[0], x[1], (left / l.max(1e-9)).min(1.0), &mut r);
            left -= l;
        }
    }
    // from the point along its way to the junctions on both sides, as far as reachable
    if let Some(Some((_, pts))) = geo.get(&(net.ways[from.way].0, db.latest)) {
        let pts: Vec<P> = pts.iter().map(|x| x.unwrap_or(from.point)).collect();
        let sp = net.ways[from.way].1.speed[mode_idx(m)] as f64;
        let start = from.dist / access_speed(m);
        let walk = |path: Vec<P>, r: &mut Raster| {
            let mut left = (limit - start).max(0.0) * sp;
            for x in path.windows(2) {
                if left <= 0.0 {
                    break;
                }
                let l = geom::haversine(x[0], x[1]);
                mark(x[0], x[1], (left / l.max(1e-9)).min(1.0), r);
                left -= l;
            }
        };
        let k = from.seg.min(pts.len().saturating_sub(1));
        let mut back = vec![from.point];
        back.extend(pts[from.before.1 as usize..=k].iter().rev());
        let mut on = vec![from.point];
        on.extend(&pts[(k + 1).min(pts.len())..=(from.after.1 as usize).min(pts.len().saturating_sub(1))]);
        if sp > 0.0 {
            walk(back, &mut r);
            walk(on, &mut r);
        }
    }
    mark(from.point, from.point, 1.0, &mut r);
    r.fill_holes();
    r.bbox = r.bbox.expand_m(cell_m);
    r
}

/// The routing network of a database, loaded once.
pub fn load(db: &Db) -> Option<Arc<RoutingNet>> {
    static CACHE: std::sync::Mutex<Vec<(std::path::PathBuf, u64, Arc<RoutingNet>)>> = std::sync::Mutex::new(Vec::new());
    let el = db.reader.element(knut_format::container::ElementKind::RoutingNetwork)?;
    let s = el.sections.first()?;
    let mut c = CACHE.lock().ok()?;
    if let Some(x) = c.iter().find(|x| x.0 == db.path && x.1 == s.checksum) {
        return Some(x.2.clone());
    }
    let net = Arc::new(decode(db.reader.section_bytes(s).ok()?).ok()?);
    c.push((db.path.clone(), s.checksum, net.clone()));
    Some(net)
}
