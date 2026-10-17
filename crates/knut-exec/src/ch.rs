//! Speed-ups of the routing network (container elements): contraction hierarchies (CH) and
//! customizable contraction hierarchies (CCH), per mode of travel. Both give a hierarchy of
//! the junctions with upward arcs; a route is the best meeting of two upward searches, an
//! isochrone an upward search followed by one sweep downwards (PHAST). Times are whole
//! deciseconds (see `RoutingNet::time`).
//!
//! CH orders the junctions by how few shortcuts their removal needs (with witness searches,
//! so the order depends on the travel times); the dense rest (the core) is ordered like a
//! CCH. CCH orders them by nested dissection of the graph alone (separators last); the
//! shortcuts follow from the order, and the travel times are filled in afterwards
//! ("customization") in seconds, for any mode or moment.

use crate::db::Db;
use crate::routing::{Route, RoutingNet, Snap, St, entries, mode_idx};
use knut_lang::ast::Mode;
use rayon::prelude::*;
use std::collections::BinaryHeap;
use std::sync::Arc;

const NONE: u32 = u32::MAX;
/// Time of an arc a mode cannot use.
const INF: u32 = u32::MAX;
pub const MODES: [Mode; 3] = [Mode::Foot, Mode::Bicycle, Mode::Car];

/// Sum of two times (unusable stays unusable).
fn add(a: u32, b: u32) -> u32 {
    if a == INF || b == INF { INF } else { a + b }
}

/// A hierarchy for one mode: rank of each junction; upward arcs `u -> v` (rank v > rank u)
/// stored at u, and arcs `v -> u` from higher v stored at u ("down", used backwards).
/// `mid` is the junction a shortcut bypasses, or NONE for an edge of the network. A CCH
/// shares rank and arcs between its modes (up and down arcs are the same pairs, arcs a mode
/// cannot use take INF) and keeps no `mid`: a shortcut is unpacked by the lowest of its
/// lower triangles with the same time, which is the one the customization chose. Arcs the
/// perfect customization found unnecessary are marked in `pruned_*` and skipped by searches.
#[derive(Default)]
pub struct Hierarchy {
    pub rank: Arc<Vec<u32>>,
    pub up_off: Arc<Vec<u32>>,
    pub up_to: Arc<Vec<u32>>,
    pub up_w: Vec<u32>,
    pub up_mid: Vec<u32>,
    pub dn_off: Arc<Vec<u32>>,
    pub dn_to: Arc<Vec<u32>>,
    pub dn_w: Vec<u32>,
    pub dn_mid: Vec<u32>,
    pub pruned_up: Vec<u64>,
    pub pruned_dn: Vec<u64>,
    /// CCH: the lower neighbours of each junction (CSR, ascending rank)
    lower: Option<Arc<(Vec<u32>, Vec<u32>)>>,
}

fn bit(bits: &[u64], i: usize) -> bool {
    !bits.is_empty() && bits[i >> 6] >> (i & 63) & 1 == 1
}

impl Hierarchy {
    fn up(&self, u: u32) -> std::ops::Range<usize> {
        self.up_off[u as usize] as usize..self.up_off[u as usize + 1] as usize
    }

    fn dn(&self, u: u32) -> std::ops::Range<usize> {
        self.dn_off[u as usize] as usize..self.dn_off[u as usize + 1] as usize
    }

    /// Time of up arc i for searches (INF if unusable or pruned).
    fn w_up(&self, i: usize) -> u32 {
        if bit(&self.pruned_up, i) { INF } else { self.up_w[i] }
    }

    fn w_dn(&self, i: usize) -> u32 {
        if bit(&self.pruned_dn, i) { INF } else { self.dn_w[i] }
    }

    /// Deciseconds of the arc a -> b (either stored up at a or down at b) and the junction
    /// it bypasses.
    fn arc(&self, net: &RoutingNet, m: Mode, a: u32, b: u32) -> Option<(u32, u32)> {
        let upward = self.rank[b as usize] > self.rank[a as usize];
        let (lo, hi) = if upward { (a, b) } else { (b, a) };
        let (range, to, ws) = if upward { (self.up(lo), &self.up_to, &self.up_w) } else { (self.dn(lo), &self.dn_to, &self.dn_w) };
        let Some(lower) = &self.lower else {
            let mids = if upward { &self.up_mid } else { &self.dn_mid };
            return range.filter(|&i| to[i] == hi).map(|i| (ws[i], mids[i])).min_by_key(|x| x.0);
        };
        let w = ws[self.index(lo, hi)?];
        if w == INF {
            return None;
        }
        // an edge of the network with this time wins (the customization keeps it on ties)
        let edge = net.edges(a).filter(|&e| net.to[e] == b).filter_map(|e| net.time(e, m, false)).min().unwrap_or(INF);
        if edge == w {
            return Some((w, NONE));
        }
        let (off, nb) = (&lower.0, &lower.1);
        for &v in &nb[off[lo as usize] as usize..off[lo as usize + 1] as usize] {
            let (Some(i_lo), Some(i_hi)) = (self.index(v, lo), self.index(v, hi)) else { continue };
            let via = if upward { add(self.dn_w[i_lo], self.up_w[i_hi]) } else { add(self.dn_w[i_hi], self.up_w[i_lo]) };
            if via == w {
                return Some((w, v));
            }
        }
        Some((w, NONE))
    }

    /// CCH: position of the arc {lo, hi} (rank lo < rank hi) among the arcs of lo.
    fn index(&self, lo: u32, hi: u32) -> Option<usize> {
        let r = self.up(lo);
        let start = r.start;
        self.up_to[r].binary_search(&hi).ok().map(|i| start + i)
    }

    /// The edges of the network along the arc a -> b.
    fn unpack(&self, net: &RoutingNet, m: Mode, a: u32, b: u32, out: &mut Vec<(usize, bool)>) {
        let Some((_, mid)) = self.arc(net, m, a, b) else { return };
        if mid == NONE {
            // the quickest edge of the network from a to b, or from b to a used backwards
            let fwd = net.edges(a).filter(|&e| net.to[e] == b).filter_map(|e| net.time(e, m, false).map(|c| (c, e, false)));
            let bwd = net.edges(b).filter(|&e| net.to[e] == a).filter_map(|e| net.time(e, m, true).map(|c| (c, e, true)));
            if let Some((_, e, rev)) = fwd.chain(bwd).min_by_key(|x| x.0) {
                out.push((e, rev));
            }
            return;
        }
        self.unpack(net, m, a, mid, out);
        self.unpack(net, m, mid, b, out);
    }

    /// Best route between start and end entries (deciseconds): two upward searches.
    fn query(&self, src: &[(u32, f64)], dst: &[(u32, f64)]) -> Option<(f64, Vec<u32>)> {
        let n = self.rank.len();
        let mut df = Dist::new(n);
        let mut dr = Dist::new(n);
        let (mut hf, mut hr) = (BinaryHeap::new(), BinaryHeap::new());
        for &(v, c) in src {
            if df.improve(v, c, NONE) {
                hf.push(St(c, v));
            }
        }
        for &(v, c) in dst {
            if dr.improve(v, c, NONE) {
                hr.push(St(c, v));
            }
        }
        let mut best = f64::INFINITY;
        let mut meet = NONE;
        let check = |v: u32, df: &Dist, dr: &Dist, best: &mut f64, meet: &mut u32| {
            let s = df.get(v) + dr.get(v);
            if s < *best {
                *best = s;
                *meet = v;
            }
        };
        for &(v, _) in src {
            check(v, &df, &dr, &mut best, &mut meet);
        }
        loop {
            let kf = hf.peek().map_or(f64::INFINITY, |x: &St| x.0);
            let kr = hr.peek().map_or(f64::INFINITY, |x: &St| x.0);
            if kf.min(kr) >= best || (!kf.is_finite() && !kr.is_finite()) {
                break;
            }
            if kf <= kr {
                let St(k, u) = hf.pop().unwrap();
                if k > df.get(u) {
                    continue;
                }
                for i in self.up(u) {
                    let w = self.w_up(i);
                    if w == INF {
                        continue;
                    }
                    let v = self.up_to[i];
                    if df.improve(v, k + w as f64, u) {
                        hf.push(St(k + w as f64, v));
                        check(v, &df, &dr, &mut best, &mut meet);
                    }
                }
            } else {
                let St(k, u) = hr.pop().unwrap();
                if k > dr.get(u) {
                    continue;
                }
                for i in self.dn(u) {
                    let w = self.w_dn(i);
                    if w == INF {
                        continue;
                    }
                    let v = self.dn_to[i];
                    if dr.improve(v, k + w as f64, u) {
                        hr.push(St(k + w as f64, v));
                        check(v, &df, &dr, &mut best, &mut meet);
                    }
                }
            }
        }
        if meet == NONE {
            return None;
        }
        // junctions from the start to the end
        let mut path = Vec::new();
        let mut v = meet;
        while v != NONE {
            path.push(v);
            v = df.pred(v);
        }
        path.reverse();
        let mut v = dr.pred(meet);
        while v != NONE {
            path.push(v);
            v = dr.pred(v);
        }
        Some((best, path))
    }

    /// Deciseconds from the start entries to every junction (PHAST): an upward search, then
    /// the junctions from the highest rank down, each taking the best of its down arcs.
    fn phast(&self, order: &[u32], src: &[(u32, f64)], limit: f64) -> Vec<f64> {
        let n = self.rank.len();
        let mut d = vec![f64::INFINITY; n];
        let mut h = BinaryHeap::new();
        for &(v, c) in src {
            if c < d[v as usize] {
                d[v as usize] = c;
                h.push(St(c, v));
            }
        }
        while let Some(St(k, u)) = h.pop() {
            if k > d[u as usize] || k > limit {
                continue;
            }
            for i in self.up(u) {
                let w = self.w_up(i);
                if w == INF {
                    continue;
                }
                let v = self.up_to[i] as usize;
                let nd = k + w as f64;
                if nd < d[v] {
                    d[v] = nd;
                    h.push(St(nd, v as u32));
                }
            }
        }
        for &v in order.iter().rev() {
            let mut best = d[v as usize];
            for i in self.dn(v) {
                let w = self.w_dn(i);
                if w == INF {
                    continue;
                }
                let x = self.dn_to[i] as usize;
                if d[x] <= limit {
                    best = best.min(d[x] + w as f64);
                }
            }
            d[v as usize] = best;
        }
        d
    }
}

/// Distances of a search, sparse (only touched junctions).
struct Dist {
    d: rustc_hash::FxHashMap<u32, (f64, u32)>,
}

impl Dist {
    fn new(_n: usize) -> Dist {
        Dist { d: Default::default() }
    }
    fn get(&self, v: u32) -> f64 {
        self.d.get(&v).map_or(f64::INFINITY, |x| x.0)
    }
    fn pred(&self, v: u32) -> u32 {
        self.d.get(&v).map_or(NONE, |x| x.1)
    }
    fn improve(&mut self, v: u32, c: f64, from: u32) -> bool {
        match self.d.get_mut(&v) {
            Some(x) if x.0 <= c => false,
            Some(x) => {
                *x = (c, from);
                true
            }
            None => {
                self.d.insert(v, (c, from));
                true
            }
        }
    }
}

/// Arcs of the network for a mode: (from, to, deciseconds), the quickest of parallel ones.
fn arcs(net: &RoutingNet, m: Mode) -> Vec<(u32, u32, u32)> {
    let mut a: Vec<(u32, u32, u32)> = (0..net.n() as u32)
        .into_par_iter()
        .flat_map_iter(|u| net.edges(u).filter_map(move |e| net.time(e, m, false).map(|c| (u, net.to[e], c))).filter(|x| x.0 != x.1))
        .collect();
    a.par_sort_unstable();
    a.dedup_by_key(|x| (x.0, x.1));
    a
}

/// Monotone priority queue of (deciseconds, junction) (radix heap: a bucket per highest bit
/// in which a key differs from the last one taken).
struct Radix {
    last: u32,
    b: Vec<Vec<u64>>,
    len: usize,
}

impl Radix {
    fn new() -> Radix {
        Radix { last: 0, b: (0..33).map(|_| Vec::new()).collect(), len: 0 }
    }

    fn clear(&mut self) {
        for x in &mut self.b {
            x.clear();
        }
        self.last = 0;
        self.len = 0;
    }

    fn bucket(&self, k: u32) -> usize {
        if k == self.last { 0 } else { 32 - (k ^ self.last).leading_zeros() as usize }
    }

    fn push(&mut self, k: u32, v: u32) {
        let i = self.bucket(k);
        self.b[i].push((k as u64) << 32 | v as u64);
        self.len += 1;
    }

    fn pop(&mut self) -> Option<(u32, u32)> {
        if self.len == 0 {
            return None;
        }
        if self.b[0].is_empty() {
            let i = (1..33).find(|&i| !self.b[i].is_empty())?;
            let mut items = std::mem::take(&mut self.b[i]);
            self.last = items.iter().map(|x| (x >> 32) as u32).min()?;
            for &x in &items {
                let j = self.bucket((x >> 32) as u32);
                self.b[j].push(x);
            }
            items.clear();
            self.b[i] = items;
        }
        let x = self.b[0].pop()?;
        self.len -= 1;
        Some(((x >> 32) as u32, x as u32))
    }
}

// ----- CH -----------------------------------------------------------------------------------

/// An arc while contracting: other end, deciseconds, bypassed junction, and the number of
/// edges of the network it stands for.
#[derive(Clone, Copy)]
struct Link {
    to: u32,
    w: u32,
    mid: u32,
    hops: u32,
}

type Lists = Vec<Vec<Link>>;

/// Witness searches: settled junctions at most while estimating a priority, and while
/// contracting.
const PRIO_SETTLE: u32 = 100;
const CONTRACT_SETTLE: u32 = 500;
/// Share of the junctions left when contracting stops; this core is ordered by nested
/// dissection (contracting it greedily is slow: few junctions per round, dense lists).
const CORE_SHARE: f64 = 0.01;

thread_local! {
    static WITNESS: std::cell::RefCell<Option<Witness>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with this thread's witness search (arrays for `n` junctions, kept between calls).
fn with_witness<T>(n: usize, f: impl FnOnce(&mut Witness) -> T) -> T {
    WITNESS.with(|w| {
        let mut w = w.borrow_mut();
        if w.as_ref().is_none_or(|x| x.d.len() != n) {
            *w = Some(Witness::new(n));
        }
        f(w.as_mut().unwrap())
    })
}

/// Shortcuts (u, w, deciseconds, hops) that removing `v` would need: for every pair of active
/// neighbours u -> v -> w without a path of at most the same time that avoids v and all
/// inactive junctions. The witness searches of the neighbours u run in parallel.
fn shortcuts(v: u32, out: &Lists, inc: &Lists, state: &[u8], settle: u32) -> Vec<(u32, u32, u32, u32)> {
    let n = state.len();
    let from: Vec<Link> = inc[v as usize].iter().filter(|x| state[x.to as usize] == 0).copied().collect();
    let search = |l: &Link| -> Vec<(u32, u32, u32, u32)> {
        let targets: Vec<(u32, u32, u32)> =
            out[v as usize].iter().filter(|x| state[x.to as usize] == 0 && x.to != l.to).map(|x| (x.to, l.w + x.w, l.hops + x.hops)).collect();
        if targets.is_empty() {
            return Vec::new();
        }
        let max = targets.iter().map(|t| t.1).max().unwrap_or(0);
        with_witness(n, |w| {
            w.run(l.to, v, max, &targets, out, state, settle);
            targets.iter().filter(|t| w.dist(t.0) > t.1).map(|t| (l.to, t.0, t.1, t.2)).collect()
        })
    };
    if from.len() >= 8 {
        from.par_iter().map(search).collect::<Vec<_>>().concat()
    } else {
        from.iter().flat_map(search).collect()
    }
}

/// Priority of removing `v` (lower first): its level in the hierarchy, and how many arcs and
/// edges of the network its shortcuts would add per arc and edge it removes.
fn priority(v: u32, out: &Lists, inc: &Lists, state: &[u8], level: &[u32]) -> i64 {
    let sc = shortcuts(v, out, inc, state, PRIO_SETTLE);
    let (mut removed, mut removed_hops) = (0i64, 0i64);
    for x in out[v as usize].iter().chain(&inc[v as usize]) {
        if state[x.to as usize] == 0 {
            removed += 1;
            removed_hops += x.hops as i64;
        }
    }
    let added = sc.len() as i64;
    let added_hops: i64 = sc.iter().map(|x| x.3 as i64).sum();
    1 + 1000 * level[v as usize] as i64 + 1000 * added / removed.max(1) + 1000 * added_hops / removed_hops.max(1)
}

/// Contraction hierarchy for a mode, built in rounds: all junctions whose priority is below
/// that of each active neighbour are removed together, their witness searches in parallel
/// (they avoid every junction of the round, so a few shortcuts may be superfluous, none is
/// missing). Priorities of changed neighbours are only renewed when they are about to be
/// removed (lazy). The last junctions (the core) are ordered by nested dissection and
/// contracted without witness searches, like a CCH.
pub fn build_ch(net: &RoutingNet, m: Mode, progress: &(dyn Fn(f64) + Sync)) -> Hierarchy {
    let n = net.n();
    let mut out: Lists = vec![Vec::new(); n];
    let mut inc: Lists = vec![Vec::new(); n];
    for (a, b, w) in arcs(net, m) {
        out[a as usize].push(Link { to: b, w, mid: NONE, hops: 1 });
        inc[b as usize].push(Link { to: a, w, mid: NONE, hops: 1 });
    }
    // 0 active, 1 removed in this round, 2 removed
    let mut state = vec![0u8; n];
    let mut level = vec![0u32; n];
    let mut dirty = vec![false; n];
    let mut rank = vec![0u32; n];
    // the arcs of each junction when it is removed, by rank: up to the active neighbours,
    // down from them (arcs to junctions removed before were kept with those)
    let mut up = ByRank::new(n);
    let mut dn = ByRank::new(n);
    let mut prio: Vec<i64> = (0..n as u32).into_par_iter().map(|v| priority(v, &out, &inc, &state, &level)).collect();
    let tie = |v: u32| (v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut remaining: Vec<u32> = (0..n as u32).collect();
    let mut next_rank = 0u32;
    let core_size = (n as f64 * CORE_SHARE) as usize;
    while remaining.len() > core_size {
        // local minima; stale priorities among them are renewed first (a few times)
        let mut round: Vec<u32>;
        let mut renewals = 0;
        loop {
            let key = |v: u32| (prio[v as usize], tie(v));
            round = remaining
                .par_iter()
                .copied()
                .filter(|&v| out[v as usize].iter().chain(&inc[v as usize]).all(|x| state[x.to as usize] != 0 || x.to == v || key(v) < key(x.to)))
                .collect();
            let stale: Vec<u32> = round.iter().copied().filter(|&v| dirty[v as usize]).collect();
            if stale.is_empty() || renewals == 3 {
                break;
            }
            renewals += 1;
            let p: Vec<i64> = stale.par_iter().map(|&v| priority(v, &out, &inc, &state, &level)).collect();
            for (&v, p) in stale.iter().zip(p) {
                prio[v as usize] = p;
                dirty[v as usize] = false;
            }
        }
        for &v in &round {
            state[v as usize] = 1;
        }
        let found: Vec<Vec<(u32, u32, u32, u32)>> = round.par_iter().map(|&v| shortcuts(v, &out, &inc, &state, CONTRACT_SETTLE)).collect();
        for (&v, sc) in round.iter().zip(found) {
            rank[v as usize] = next_rank;
            next_rank += 1;
            up.push(out[v as usize].iter().filter(|x| state[x.to as usize] == 0).map(|x| (x.to, x.w, x.mid)));
            dn.push(inc[v as usize].iter().filter(|x| state[x.to as usize] == 0).map(|x| (x.to, x.w, x.mid)));
            let lv = level[v as usize] + 1;
            for x in out[v as usize].iter().chain(inc[v as usize].iter()) {
                if state[x.to as usize] == 0 {
                    level[x.to as usize] = level[x.to as usize].max(lv);
                    dirty[x.to as usize] = true;
                }
            }
            for (a, b, c, h) in sc {
                add_link(&mut out[a as usize], Link { to: b, w: c, mid: v, hops: h });
                add_link(&mut inc[b as usize], Link { to: a, w: c, mid: v, hops: h });
            }
        }
        // removed junctions leave the lists of their neighbours
        let mut nbs: Vec<u32> = Vec::new();
        for &v in &round {
            state[v as usize] = 2;
            nbs.extend(out[v as usize].iter().chain(&inc[v as usize]).map(|x| x.to));
            out[v as usize] = Vec::new();
            inc[v as usize] = Vec::new();
        }
        nbs.par_sort_unstable();
        nbs.dedup();
        {
            let state = &state;
            let prune = |l: &mut Vec<Link>| {
                if l.iter().any(|x| state[x.to as usize] == 2) {
                    l.retain(|x| state[x.to as usize] != 2);
                }
            };
            let (po, pi) = (SyncPtr(out.as_mut_ptr()), SyncPtr(inc.as_mut_ptr()));
            nbs.par_iter().for_each(|&x| {
                // SAFETY: `nbs` holds distinct junctions, so each list is touched by one task.
                unsafe {
                    prune(&mut *po.get().add(x as usize));
                    prune(&mut *pi.get().add(x as usize));
                }
            });
        }
        remaining.retain(|&v| state[v as usize] == 0);
        progress(0.9 * next_rank as f64 / n as f64);
    }
    drop((prio, dirty, level));
    if !remaining.is_empty() {
        contract_core(net, &remaining, &out, &inc, &state, &mut rank, &mut next_rank, &mut up, &mut dn);
    }
    drop((out, inc, state, remaining));
    let (up_off, up_to, up_w, up_mid) = up.by_junction(&rank);
    let (dn_off, dn_to, dn_w, dn_mid) = dn.by_junction(&rank);
    progress(1.0);
    Hierarchy {
        rank: Arc::new(rank),
        up_off: Arc::new(up_off),
        up_to: Arc::new(up_to),
        up_w,
        up_mid,
        dn_off: Arc::new(dn_off),
        dn_to: Arc::new(dn_to),
        dn_w,
        dn_mid,
        pruned_up: Vec::new(),
        pruned_dn: Vec::new(),
        lower: None,
    }
}

/// The core of a CH: ordered by nested dissection, then contracted without witness searches
/// (every pair of higher neighbours gets an arc, its time from the lower triangles).
#[allow(clippy::too_many_arguments)]
fn contract_core(net: &RoutingNet, core: &[u32], out: &Lists, inc: &Lists, state: &[u8], rank: &mut [u32], next_rank: &mut u32, up: &mut ByRank, dn: &mut ByRank) {
    let n = state.len();
    let mut lid = vec![NONE; n];
    for (i, &v) in core.iter().enumerate() {
        lid[v as usize] = i as u32;
    }
    let nb: Vec<Vec<u32>> = core
        .par_iter()
        .map(|&v| {
            let mut x: Vec<u32> = out[v as usize].iter().chain(&inc[v as usize]).filter(|x| state[x.to as usize] == 0).map(|x| lid[x.to as usize]).collect();
            x.sort_unstable();
            x.dedup();
            x
        })
        .collect();
    let coords: Vec<(i32, i32)> = core.iter().map(|&v| net.coords[v as usize]).collect();
    let order = nd_order(&coords, nb.clone());
    let mut lrank = vec![0u32; core.len()];
    for (i, &v) in order.iter().enumerate() {
        lrank[v as usize] = i as u32;
    }
    let topo = chordal(&nb, lrank, &|_| {});
    drop(nb);
    let lid = &lid;
    let arcs = core.iter().enumerate().flat_map(|(i, &v)| out[v as usize].iter().filter(|x| state[x.to as usize] == 0).map(move |x| (i as u32, lid[x.to as usize], x.w, x.mid)));
    let met = topo.customize_arcs(arcs, Some(core));
    for &i in &order {
        let v = core[i as usize];
        rank[v as usize] = *next_rank;
        *next_rank += 1;
        let base = topo.off[i as usize] as usize;
        let hs = topo.higher(i);
        up.push(hs.iter().enumerate().filter(|&(j, _)| met.up[base + j] != INF).map(|(j, &h)| (core[h as usize], met.up[base + j], met.mid_up[base + j])));
        dn.push(hs.iter().enumerate().filter(|&(j, _)| met.dn[base + j] != INF).map(|(j, &h)| (core[h as usize], met.dn[base + j], met.mid_dn[base + j])));
    }
}

struct SyncPtr<T>(*mut T);
impl<T> Clone for SyncPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SyncPtr<T> {}
unsafe impl<T> Send for SyncPtr<T> {}
unsafe impl<T> Sync for SyncPtr<T> {}
impl<T> SyncPtr<T> {
    fn get(self) -> *mut T {
        self.0
    }
}

/// Arcs (to, deciseconds, mid) of the junctions in the order of their ranks.
struct ByRank {
    off: Vec<u64>,
    arcs: Vec<(u32, u32, u32)>,
}

impl ByRank {
    fn new(n: usize) -> ByRank {
        let mut off = Vec::with_capacity(n + 1);
        off.push(0);
        ByRank { off, arcs: Vec::new() }
    }

    fn push(&mut self, arcs: impl Iterator<Item = (u32, u32, u32)>) {
        self.arcs.extend(arcs);
        self.off.push(self.arcs.len() as u64);
    }

    /// CSR by junction, the arcs of each junction sorted by their other end.
    fn by_junction(self, rank: &[u32]) -> (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>) {
        let n = rank.len();
        let mut order = vec![0u32; n];
        for (v, &r) in rank.iter().enumerate() {
            order[r as usize] = v as u32;
        }
        let mut off = vec![0u32; n + 1];
        for r in 0..n {
            off[order[r] as usize + 1] = (self.off[r + 1] - self.off[r]) as u32;
        }
        for v in 0..n {
            off[v + 1] += off[v];
        }
        let mut arcs = vec![(0u32, 0u32, 0u32); self.arcs.len()];
        for r in 0..n {
            let src = &self.arcs[self.off[r] as usize..self.off[r + 1] as usize];
            let v = order[r] as usize;
            arcs[off[v] as usize..off[v + 1] as usize].copy_from_slice(src);
        }
        drop(self);
        let mut rest: &mut [(u32, u32, u32)] = &mut arcs;
        let mut slices = Vec::with_capacity(n);
        for v in 0..n {
            let (a, b) = rest.split_at_mut((off[v + 1] - off[v]) as usize);
            slices.push(a);
            rest = b;
        }
        slices.into_par_iter().for_each(|a| a.sort_unstable_by_key(|x| x.0));
        let to = arcs.iter().map(|x| x.0).collect();
        let w = arcs.iter().map(|x| x.1).collect();
        let mid = arcs.iter().map(|x| x.2).collect();
        (off, to, w, mid)
    }
}

fn add_link(list: &mut Vec<Link>, l: Link) {
    match list.iter_mut().find(|x| x.to == l.to) {
        Some(x) if x.w <= l.w => {}
        Some(x) => *x = l,
        None => list.push(l),
    }
}

/// Bounded Dijkstra for witness searches (reuses its arrays).
struct Witness {
    d: Vec<u32>,
    touched: Vec<u32>,
    heap: Radix,
}

impl Witness {
    fn new(n: usize) -> Witness {
        Witness { d: vec![INF; n], touched: Vec::new(), heap: Radix::new() }
    }

    fn dist(&self, v: u32) -> u32 {
        self.d[v as usize]
    }

    /// Stops after `settle` settled junctions, beyond `max`, or once all targets are
    /// settled (their times cannot change any more).
    #[allow(clippy::too_many_arguments)]
    fn run(&mut self, s: u32, skip: u32, max: u32, targets: &[(u32, u32, u32)], out: &Lists, state: &[u8], settle: u32) {
        for &t in &self.touched {
            self.d[t as usize] = INF;
        }
        self.touched.clear();
        self.heap.clear();
        self.d[s as usize] = 0;
        self.touched.push(s);
        self.heap.push(0, s);
        let mut settled = 0;
        let mut open = targets.len();
        while let Some((k, u)) = self.heap.pop() {
            if k > self.d[u as usize] {
                continue;
            }
            settled += 1;
            if settled > settle || k > max {
                break;
            }
            if targets.iter().any(|t| t.0 == u) {
                open -= 1;
                if open == 0 {
                    break;
                }
            }
            for x in &out[u as usize] {
                if x.to == skip || state[x.to as usize] != 0 {
                    continue;
                }
                let nd = k + x.w;
                if nd < self.d[x.to as usize] {
                    if self.d[x.to as usize] == INF {
                        self.touched.push(x.to);
                    }
                    self.d[x.to as usize] = nd;
                    self.heap.push(nd, x.to);
                }
            }
        }
    }
}

// ----- CCH ----------------------------------------------------------------------------------

/// The topology of a CCH: the order and, per junction, its higher neighbours in the
/// chordal supergraph (sorted by junction).
pub struct CchTopology {
    pub rank: Vec<u32>,
    pub off: Vec<u32>,
    pub nb: Vec<u32>,
}

/// Parts of at most this many junctions are ordered by minimum degree instead of by cuts.
const MIN_DEGREE_PART: usize = 1000;
/// Parts of more junctions are cut on a coarser graph first (clusters of this size), then
/// the cut is refined in the band around it.
const COARSE_PART: usize = 200_000;
const CLUSTER: usize = 16;
/// Rings of clusters around the coarse separator that the refinement may cut through.
const BAND: usize = 2;

/// The order of the junctions for a CCH (rank per junction).
pub fn cch_order(net: &RoutingNet) -> Vec<u32> {
    let order = nd_order(&net.coords, neighbours(net));
    let mut rank = vec![0u32; net.n()];
    for (i, &v) in order.iter().enumerate() {
        rank[v as usize] = i as u32;
    }
    rank
}

/// Neighbours of each junction, ignoring directions and modes.
fn neighbours(net: &RoutingNet) -> Vec<Vec<u32>> {
    let n = net.n();
    let mut nb: Vec<Vec<u32>> = vec![Vec::new(); n];
    for u in 0..n as u32 {
        for e in net.edges(u) {
            let v = net.to[e];
            if v != u {
                nb[u as usize].push(v);
                nb[v as usize].push(u);
            }
        }
    }
    nb.par_iter_mut().for_each(|x| {
        x.sort_unstable();
        x.dedup();
    });
    nb
}

/// Elimination order (lowest rank first) of an undirected graph with positions: junctions of
/// degree at most two first, repeatedly (trees and chains need no separator; a chain leaves
/// an edge between its ends), then the rest by nested dissection with inertial flow: a part
/// is split by the smallest set of junctions that separates its first quarter from its last
/// quarter along one of eight directions (a minimum vertex cut); the separator comes after
/// both halves; small parts are ordered by minimum degree.
fn nd_order(coords: &[(i32, i32)], mut nb: Vec<Vec<u32>>) -> Vec<u32> {
    let n = nb.len();
    let mut gone = vec![false; n];
    let mut first: Vec<u32> = Vec::new();
    let mut stack: Vec<u32> = (0..n as u32).rev().filter(|&v| nb[v as usize].len() <= 2).collect();
    while let Some(v) = stack.pop() {
        let vi = v as usize;
        if gone[vi] || nb[vi].len() > 2 {
            continue;
        }
        gone[vi] = true;
        first.push(v);
        let ns = std::mem::take(&mut nb[vi]);
        for &a in &ns {
            nb[a as usize].retain(|&x| x != v);
        }
        if let [a, b] = ns[..]
            && !nb[a as usize].contains(&b)
        {
            nb[a as usize].push(b);
            nb[b as usize].push(a);
        }
        for &a in &ns {
            if nb[a as usize].len() <= 2 {
                stack.push(a);
            }
        }
    }
    let core: Vec<u32> = (0..n as u32).filter(|&v| !gone[v as usize]).collect();
    let mut lid = vec![NONE; n];
    for (i, &v) in core.iter().enumerate() {
        lid[v as usize] = i as u32;
    }
    let cnb: Vec<Vec<u32>> = core.par_iter().map(|&v| nb[v as usize].iter().map(|&w| lid[w as usize]).collect()).collect();
    drop(nb);
    let ccoords: Vec<(i32, i32)> = core.iter().map(|&v| coords[v as usize]).collect();
    let local: Vec<std::sync::atomic::AtomicU32> = (0..core.len()).map(|_| std::sync::atomic::AtomicU32::new(u32::MAX)).collect();
    let order = order_part(&ccoords, (0..core.len() as u32).collect(), &cnb, &local);
    first.extend(order.iter().map(|&i| core[i as usize]));
    first
}

/// The elimination order of a part (lowest rank first): its halves, then the separator.
fn order_part(coords: &[(i32, i32)], part: Vec<u32>, nb: &[Vec<u32>], local: &[std::sync::atomic::AtomicU32]) -> Vec<u32> {
    use std::sync::atomic::Ordering::Relaxed;
    for (i, &v) in part.iter().enumerate() {
        local[v as usize].store(i as u32, Relaxed);
    }
    let view = LocalView { local };
    let reset = |part: &[u32]| {
        for &v in part {
            local[v as usize].store(u32::MAX, Relaxed);
        }
    };
    if part.len() <= MIN_DEGREE_PART {
        let order = min_degree(&part, nb, &view);
        reset(&part);
        return order;
    }
    let pieces = pieces(&part, nb, &view);
    if pieces.len() > 1 {
        reset(&part);
        return pieces.into_par_iter().flat_map_iter(|p| order_part(coords, p, nb, local)).collect();
    }
    let (sep, a, b) = if part.len() > COARSE_PART { coarse_cut(coords, &part, nb, &view) } else { inertial_cut(coords, &part, nb, &view) };
    reset(&part);
    let (mut oa, ob) = rayon::join(|| order_part(coords, a, nb, local), || order_part(coords, b, nb, local));
    oa.extend(ob);
    oa.extend(sep);
    oa
}

/// Read access to the positions of the junctions of the current part.
struct LocalView<'a> {
    local: &'a [std::sync::atomic::AtomicU32],
}

impl LocalView<'_> {
    fn get(&self, v: u32) -> u32 {
        self.local[v as usize].load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Connected pieces of a part (`local` maps its junctions to positions in it).
fn pieces(part: &[u32], nb: &[Vec<u32>], local: &LocalView) -> Vec<Vec<u32>> {
    let mut seen = vec![false; part.len()];
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for i in 0..part.len() {
        if seen[i] {
            continue;
        }
        let mut piece = Vec::new();
        seen[i] = true;
        stack.push(i as u32);
        while let Some(x) = stack.pop() {
            let v = part[x as usize];
            piece.push(v);
            for &w in &nb[v as usize] {
                let j = local.get(w);
                if j != u32::MAX && (j as usize) < part.len() && part[j as usize] == w && !seen[j as usize] {
                    seen[j as usize] = true;
                    stack.push(j);
                }
            }
        }
        out.push(piece);
    }
    out
}

/// Minimum degree order of a part: repeatedly the junction with the fewest neighbours,
/// whose neighbours then become neighbours of each other.
fn min_degree(part: &[u32], nb: &[Vec<u32>], local: &LocalView) -> Vec<u32> {
    let k = part.len();
    let pos = |w: u32| {
        let j = local.get(w);
        (j != u32::MAX && (j as usize) < k && part[j as usize] == w).then_some(j)
    };
    let mut adj: Vec<Vec<u32>> = part
        .iter()
        .map(|&v| {
            let mut a: Vec<u32> = nb[v as usize].iter().filter_map(|&w| pos(w)).collect();
            a.sort_unstable();
            a.dedup();
            a
        })
        .collect();
    let mut gone = vec![false; k];
    let mut heap: BinaryHeap<std::cmp::Reverse<(u32, u32)>> = (0..k).map(|i| std::cmp::Reverse((adj[i].len() as u32, i as u32))).collect();
    let mut order = Vec::with_capacity(k);
    let mut merged = Vec::new();
    while let Some(std::cmp::Reverse((d, i))) = heap.pop() {
        let i = i as usize;
        if gone[i] || adj[i].len() as u32 != d {
            continue;
        }
        gone[i] = true;
        order.push(part[i]);
        let ns = std::mem::take(&mut adj[i]);
        for &a in &ns {
            // a loses i and gains the other neighbours of i
            let au = a as usize;
            merged.clear();
            let (x, y) = (&adj[au], &ns);
            let (mut p, mut q) = (0, 0);
            while p < x.len() || q < y.len() {
                let next = match (x.get(p), y.get(q)) {
                    (Some(&s), Some(&t)) if s < t => {
                        p += 1;
                        s
                    }
                    (Some(&s), Some(&t)) if s > t => {
                        q += 1;
                        t
                    }
                    (Some(&s), Some(_)) => {
                        p += 1;
                        q += 1;
                        s
                    }
                    (Some(&s), None) => {
                        p += 1;
                        s
                    }
                    (None, Some(&t)) => {
                        q += 1;
                        t
                    }
                    (None, None) => unreachable!(),
                };
                if next != a && next as usize != i {
                    merged.push(next);
                }
            }
            std::mem::swap(&mut adj[au], &mut merged);
            heap.push(std::cmp::Reverse((adj[au].len() as u32, a)));
        }
    }
    order
}

/// A cut of a large part: connected clusters of junctions (grown breadth-first) form a
/// coarse graph, which is cut by inertial flow; the clusters of the coarse separator and
/// their neighbours form a band, and the smallest set of junctions in the band separating
/// the two sides is the separator.
fn coarse_cut(coords: &[(i32, i32)], part: &[u32], nb: &[Vec<u32>], local: &LocalView) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let k = part.len();
    let pos = |w: u32| {
        let j = local.get(w);
        (j != u32::MAX && (j as usize) < k && part[j as usize] == w).then_some(j as usize)
    };
    let mut cl = vec![u32::MAX; k];
    let mut nc = 0u32;
    let mut queue = std::collections::VecDeque::new();
    for i in 0..k {
        if cl[i] != u32::MAX {
            continue;
        }
        cl[i] = nc;
        queue.clear();
        queue.push_back(i);
        let mut size = 1;
        while let Some(x) = queue.pop_front() {
            for &w in &nb[part[x] as usize] {
                if size >= CLUSTER {
                    break;
                }
                if let Some(j) = pos(w)
                    && cl[j] == u32::MAX
                {
                    cl[j] = nc;
                    size += 1;
                    queue.push_back(j);
                }
            }
        }
        nc += 1;
    }
    let nc = nc as usize;
    let mut cnb: Vec<Vec<u32>> = vec![Vec::new(); nc];
    let mut sum = vec![(0i64, 0i64, 0i64); nc];
    for i in 0..k {
        let c = cl[i] as usize;
        let (x, y) = coords[part[i] as usize];
        sum[c] = (sum[c].0 + x as i64, sum[c].1 + y as i64, sum[c].2 + 1);
        for &w in &nb[part[i] as usize] {
            if let Some(j) = pos(w)
                && cl[j] as usize != c
            {
                cnb[c].push(cl[j]);
            }
        }
    }
    cnb.par_iter_mut().for_each(|x| {
        x.sort_unstable();
        x.dedup();
    });
    let ccoords: Vec<(i32, i32)> = sum.iter().map(|s| ((s.0 / s.2) as i32, (s.1 / s.2) as i32)).collect();
    let clocal: Vec<std::sync::atomic::AtomicU32> = (0..nc as u32).map(std::sync::atomic::AtomicU32::new).collect();
    let cpart: Vec<u32> = (0..nc as u32).collect();
    let (csep, ca, _) = inertial_cut(&ccoords, &cpart, &cnb, &LocalView { local: &clocal });
    // 0 band, 1 source side, 2 sink side
    let mut crole = vec![2u8; nc];
    for &c in &ca {
        crole[c as usize] = 1;
    }
    let mut ring: Vec<u32> = csep.clone();
    for &c in &ring {
        crole[c as usize] = 0;
    }
    for _ in 0..BAND {
        let mut next = Vec::new();
        for &c in &ring {
            for &d in &cnb[c as usize] {
                if crole[d as usize] != 0 {
                    crole[d as usize] = 0;
                    next.push(d);
                }
            }
        }
        ring = next;
    }
    let role: Vec<u8> = cl.iter().map(|&c| crole[c as usize]).collect();
    split_by_cut(part, min_vertex_cut(part, nb, local, &role))
}

/// The separator and the two sides of a part from a cut and the source side per junction.
fn split_by_cut(part: &[u32], (cut, src_side): (Vec<u32>, Vec<bool>)) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let in_cut: rustc_hash::FxHashSet<u32> = cut.iter().copied().collect();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for (i, &v) in part.iter().enumerate() {
        if in_cut.contains(&v) {
            continue;
        }
        if src_side[i] { a.push(v) } else { b.push(v) }
    }
    (cut, a, b)
}

/// A minimum vertex cut between the first and the last quarter of a part along the best of
/// eight directions; returns the separator and the two sides.
fn inertial_cut(coords: &[(i32, i32)], part: &[u32], nb: &[Vec<u32>], local: &LocalView) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let k = part.len();
    let mid_lat = part.iter().map(|&v| coords[v as usize].1 as f64).sum::<f64>() / k as f64 * 1e-7;
    let kx = mid_lat.to_radians().cos();
    let dirs: Vec<(f64, f64)> = (0..8).map(|i| (i as f64 * std::f64::consts::PI / 8.0).sin_cos()).map(|(s, c)| (c, s)).collect();
    let cuts: Vec<(Vec<u32>, Vec<bool>)> = dirs
        .par_iter()
        .map(|&(dx, dy)| {
            let mut idx: Vec<u32> = (0..k as u32).collect();
            let proj = |i: u32| {
                let (x, y) = coords[part[i as usize] as usize];
                x as f64 * kx * dx + y as f64 * dy
            };
            idx.sort_by(|&a, &b| proj(a).total_cmp(&proj(b)).then(a.cmp(&b)));
            let q = (k / 4).max(1);
            let mut role = vec![0u8; k]; // 1 source, 2 sink
            for &i in &idx[..q] {
                role[i as usize] = 1;
            }
            for &i in &idx[k - q..] {
                role[i as usize] = 2;
            }
            min_vertex_cut(part, nb, local, &role)
        })
        .collect();
    split_by_cut(part, cuts.into_iter().min_by_key(|c| c.0.len()).unwrap())
}

/// Minimum vertex cut between sources and sinks (unit capacity per junction, Dinic on the
/// split graph); returns the cut nearest to the sources and, per junction, whether it is on
/// the source side. The quarters cannot be cut, so they are merged into the source and the
/// sink; the cut nearest to the sources does not depend on how the flow was found.
fn min_vertex_cut(part: &[u32], nb: &[Vec<u32>], local: &LocalView, role: &[u8]) -> (Vec<u32>, Vec<bool>) {
    let k = part.len();
    let pos = |w: u32| {
        let j = local.get(w);
        (j != u32::MAX && (j as usize) < k && part[j as usize] == w).then_some(j as usize)
    };
    // quarters that touch each other would allow unlimited flow: the general network
    if (0..k).any(|i| role[i] == 1 && nb[part[i] as usize].iter().any(|&w| pos(w).is_some_and(|j| role[j] == 2))) {
        return min_vertex_cut_split(part, nb, local, role);
    }
    let mut id = vec![u32::MAX; k];
    let mut free: Vec<u32> = Vec::new();
    for i in 0..k {
        if role[i] == 0 {
            id[i] = free.len() as u32;
            free.push(i as u32);
        }
    }
    // nodes: 2c = in, 2c + 1 = out of the c-th free junction, then source and sink
    let f = free.len();
    let (s, t) = (2 * f, 2 * f + 1);
    let nn = 2 * f + 2;
    let big = i32::MAX / 4;
    let mut arcs: Vec<(u32, u32, i32)> = Vec::new();
    for (c, &i) in free.iter().enumerate() {
        arcs.push((2 * c as u32, 2 * c as u32 + 1, 1));
        let (mut from_s, mut to_t) = (false, false);
        for &w in &nb[part[i as usize] as usize] {
            let Some(j) = pos(w) else { continue };
            match role[j] {
                0 => arcs.push((2 * c as u32 + 1, 2 * id[j], big)),
                1 => from_s = true,
                _ => to_t = true,
            }
        }
        if from_s {
            arcs.push((s as u32, 2 * c as u32, big));
        }
        if to_t {
            arcs.push((2 * c as u32 + 1, t as u32, big));
        }
    }
    // CSR; `rev` gives the reverse of each arc
    let mut off = vec![0u32; nn + 1];
    for &(a, b, _) in &arcs {
        off[a as usize + 1] += 1;
        off[b as usize + 1] += 1;
    }
    for i in 0..nn {
        off[i + 1] += off[i];
    }
    let m = off[nn] as usize;
    let mut fill = off.clone();
    let (mut to, mut cap, mut rev) = (vec![0u32; m], vec![0i32; m], vec![0u32; m]);
    for (a, b, c) in arcs {
        let (ea, eb) = (fill[a as usize] as usize, fill[b as usize] as usize);
        fill[a as usize] += 1;
        fill[b as usize] += 1;
        (to[ea], cap[ea], rev[ea]) = (b, c, eb as u32);
        (to[eb], cap[eb], rev[eb]) = (a, 0, ea as u32);
    }
    drop(fill);
    // most of the flow by Boykov-Kolmogorov (search trees kept between augmentations), the
    // rest, if any, by Dinic on the residual graph
    grow_and_augment(s, t, &off, &to, &mut cap, &rev);
    let mut level = vec![-1i32; nn];
    let mut it = vec![0u32; nn];
    let mut queue: Vec<u32> = Vec::with_capacity(nn);
    loop {
        // levels by BFS in the residual graph, up to the level of the sink (beyond it no
        // junction leads to the sink)
        level.iter_mut().for_each(|x| *x = -1);
        queue.clear();
        level[s] = 0;
        queue.push(s as u32);
        let mut q = 0;
        'bfs: while q < queue.len() {
            let u = queue[q] as usize;
            q += 1;
            for e in off[u] as usize..off[u + 1] as usize {
                let v = to[e] as usize;
                if cap[e] > 0 && level[v] < 0 {
                    level[v] = level[u] + 1;
                    if v == t {
                        break 'bfs;
                    }
                    queue.push(v as u32);
                }
            }
        }
        if level[t] < 0 {
            break;
        }
        it.copy_from_slice(&off[..nn]);
        // blocking flow by iterative DFS
        let mut path: Vec<u32> = Vec::new();
        loop {
            path.clear();
            let mut u = s;
            let found = loop {
                if u == t {
                    break true;
                }
                let mut advanced = false;
                while it[u] < off[u + 1] {
                    let e = it[u] as usize;
                    let v = to[e] as usize;
                    if cap[e] > 0 && level[v] == level[u] + 1 {
                        path.push(e as u32);
                        u = v;
                        advanced = true;
                        break;
                    }
                    it[u] += 1;
                }
                if !advanced {
                    if u == s {
                        break false;
                    }
                    // dead end: retreat
                    level[u] = -1;
                    let e = path.pop().unwrap() as usize;
                    u = to[rev[e] as usize] as usize;
                    it[u] += 1;
                }
            };
            if !found {
                break;
            }
            let fl = path.iter().map(|&e| cap[e as usize]).min().unwrap_or(0);
            for &e in &path {
                cap[e as usize] -= fl;
                cap[rev[e as usize] as usize] += fl;
            }
        }
    }
    // reachable from the source in the residual graph
    let mut reach = vec![false; nn];
    let mut stack = vec![s];
    reach[s] = true;
    while let Some(u) = stack.pop() {
        for e in off[u] as usize..off[u + 1] as usize {
            let v = to[e] as usize;
            if cap[e] > 0 && !reach[v] {
                reach[v] = true;
                stack.push(v);
            }
        }
    }
    let cut: Vec<u32> = (0..k).filter(|&i| role[i] == 0 && reach[2 * id[i] as usize] && !reach[2 * id[i] as usize + 1]).map(|i| part[i]).collect();
    let side: Vec<bool> = (0..k).map(|i| match role[i] { 0 => reach[2 * id[i] as usize + 1], 1 => true, _ => false }).collect();
    (cut, side)
}

/// Boykov-Kolmogorov max-flow on a CSR network (arc e has reverse rev[e]): a search tree
/// grows from the source and one from the sink; where they touch there is an augmenting
/// path; arcs it saturates cut subtrees off, which find a new parent nearby or become free.
fn grow_and_augment(s: usize, t: usize, off: &[u32], to: &[u32], cap: &mut [i32], rev: &[u32]) {
    const FREE: u8 = 0;
    const SRC: u8 = 1;
    const SNK: u8 = 2;
    const ORPHAN: u32 = u32::MAX;
    const ROOT: u32 = u32::MAX - 1;
    let nn = off.len() - 1;
    let mut tree = vec![FREE; nn];
    // the arc to the parent: in the source tree parent -> v, in the sink tree v -> parent
    let mut parent = vec![ORPHAN; nn];
    // validated in round `mark[v]` at distance `dist[v]` from its root
    let mut mark = vec![0u32; nn];
    let mut dist = vec![0u32; nn];
    let mut round = 1u32;
    let parent_of = |v: usize, tree: &[u8], parent: &[u32]| -> usize {
        let e = parent[v] as usize;
        if tree[v] == SRC { to[rev[e] as usize] as usize } else { to[e] as usize }
    };
    tree[s] = SRC;
    tree[t] = SNK;
    parent[s] = ROOT;
    parent[t] = ROOT;
    mark[s] = round;
    mark[t] = round;
    let mut active: std::collections::VecDeque<u32> = [s as u32, t as u32].into();
    let mut orphans: Vec<u32> = Vec::new();
    let mut path: Vec<u32> = Vec::new();
    loop {
        // grow until the trees touch: arc `meet` from the source tree to the sink tree
        let mut meet = None;
        while let Some(&p) = active.front() {
            let p = p as usize;
            let tp = tree[p];
            if tp != FREE {
                for e in off[p] as usize..off[p + 1] as usize {
                    let q = to[e] as usize;
                    let (res, toward) = if tp == SRC { (cap[e], e as u32) } else { (cap[rev[e] as usize], rev[e]) };
                    if res <= 0 {
                        continue;
                    }
                    if tree[q] == FREE {
                        tree[q] = tp;
                        parent[q] = toward;
                        mark[q] = mark[p];
                        dist[q] = dist[p] + 1;
                        active.push_back(q as u32);
                    } else if tree[q] != tp {
                        meet = Some(toward as usize);
                        break;
                    }
                }
            }
            if meet.is_some() {
                break;
            }
            active.pop_front();
        }
        let Some(e) = meet else { break };
        // augment along source ... -> e -> ... sink
        path.clear();
        path.push(e as u32);
        let mut v = to[rev[e] as usize] as usize;
        while v != s {
            path.push(parent[v]);
            v = parent_of(v, &tree, &parent);
        }
        let mut v = to[e] as usize;
        while v != t {
            path.push(parent[v]);
            v = parent_of(v, &tree, &parent);
        }
        let b = path.iter().map(|&a| cap[a as usize]).min().unwrap();
        for &a in &path {
            cap[a as usize] -= b;
            cap[rev[a as usize] as usize] += b;
        }
        // saturated tree arcs make orphans of their children
        for &a in &path[1..] {
            if cap[a as usize] == 0 {
                let a = a as usize;
                let (x, y) = (to[rev[a] as usize] as usize, to[a] as usize);
                let child = if tree[x] == SRC && tree[y] == SRC { y } else { x };
                parent[child] = ORPHAN;
                mark[child] = 0;
                orphans.push(child as u32);
            }
        }
        round += 1;
        // adoption
        while let Some(p) = orphans.pop() {
            let p = p as usize;
            let tp = tree[p];
            let mut best: Option<(u32, u32)> = None; // (arc, distance)
            for e in off[p] as usize..off[p + 1] as usize {
                let q = to[e] as usize;
                if tree[q] != tp {
                    continue;
                }
                let (res, toward) = if tp == SRC { (cap[rev[e] as usize], rev[e]) } else { (cap[e], e as u32) };
                if res <= 0 {
                    continue;
                }
                // does q still hang at a root?
                let mut x = q;
                let mut d = 0u32;
                let ok = loop {
                    if mark[x] == round {
                        d += dist[x];
                        break true;
                    }
                    match parent[x] {
                        ROOT => {
                            mark[x] = round;
                            dist[x] = 0;
                            break true;
                        }
                        ORPHAN => break false,
                        _ => {
                            d += 1;
                            x = parent_of(x, &tree, &parent);
                        }
                    }
                };
                if ok {
                    // remember the distances along the way
                    let mut x = q;
                    let mut dx = d;
                    while mark[x] != round {
                        mark[x] = round;
                        dist[x] = dx;
                        dx -= 1;
                        x = parent_of(x, &tree, &parent);
                    }
                    if best.is_none_or(|b| d < b.1) {
                        best = Some((toward, d));
                    }
                }
            }
            if let Some((a, d)) = best {
                parent[p] = a;
                mark[p] = round;
                dist[p] = d + 1;
                continue;
            }
            // no new parent: p becomes free, its neighbours may grow into it again
            for e in off[p] as usize..off[p + 1] as usize {
                let q = to[e] as usize;
                if tree[q] != tp {
                    continue;
                }
                let res = if tp == SRC { cap[rev[e] as usize] } else { cap[e] };
                if res > 0 {
                    active.push_back(q as u32);
                }
                if parent[q] != ORPHAN && parent[q] != ROOT && parent_of(q, &tree, &parent) == p {
                    parent[q] = ORPHAN;
                    mark[q] = 0;
                    orphans.push(q as u32);
                }
            }
            tree[p] = FREE;
        }
    }
}

/// The same on the full split graph (any sources and sinks).
fn min_vertex_cut_split(part: &[u32], nb: &[Vec<u32>], local: &LocalView, role: &[u8]) -> (Vec<u32>, Vec<bool>) {
    let k = part.len();
    // nodes: 2i = in, 2i + 1 = out, 2k = source, 2k + 1 = sink
    let (s, t) = (2 * k, 2 * k + 1);
    let nn = 2 * k + 2;
    let big = i32::MAX / 4;
    let mut head: Vec<u32> = vec![u32::MAX; nn];
    let mut next: Vec<u32> = Vec::new();
    let mut to: Vec<u32> = Vec::new();
    let mut cap: Vec<i32> = Vec::new();
    let mut add = |a: usize, b: usize, c: i32, head: &mut Vec<u32>| {
        for (x, y, cc) in [(a, b, c), (b, a, 0)] {
            to.push(y as u32);
            cap.push(cc);
            next.push(head[x]);
            head[x] = (to.len() - 1) as u32;
        }
    };
    for i in 0..k {
        let inner = if role[i] == 0 { 1 } else { big };
        add(2 * i, 2 * i + 1, inner, &mut head);
        if role[i] == 1 {
            add(s, 2 * i, big, &mut head);
        }
        if role[i] == 2 {
            add(2 * i + 1, t, big, &mut head);
        }
        for &w in &nb[part[i] as usize] {
            let j = local.get(w);
            if j != u32::MAX && (j as usize) < k && part[j as usize] == w {
                add(2 * i + 1, 2 * j as usize, big, &mut head);
            }
        }
    }
    let mut level = vec![-1i32; nn];
    let mut it = vec![0u32; nn];
    loop {
        // levels by BFS in the residual graph
        level.iter_mut().for_each(|x| *x = -1);
        let mut queue = std::collections::VecDeque::new();
        level[s] = 0;
        queue.push_back(s);
        while let Some(u) = queue.pop_front() {
            let mut e = head[u];
            while e != u32::MAX {
                let v = to[e as usize] as usize;
                if cap[e as usize] > 0 && level[v] < 0 {
                    level[v] = level[u] + 1;
                    queue.push_back(v);
                }
                e = next[e as usize];
            }
        }
        if level[t] < 0 {
            break;
        }
        it.copy_from_slice(&head);
        // blocking flow by iterative DFS
        loop {
            let mut path: Vec<u32> = Vec::new();
            let mut u = s;
            let found = loop {
                if u == t {
                    break true;
                }
                let mut advanced = false;
                while it[u] != u32::MAX {
                    let e = it[u] as usize;
                    let v = to[e] as usize;
                    if cap[e] > 0 && level[v] == level[u] + 1 {
                        path.push(e as u32);
                        u = v;
                        advanced = true;
                        break;
                    }
                    it[u] = next[e];
                }
                if !advanced {
                    if u == s {
                        break false;
                    }
                    // dead end: retreat
                    level[u] = -1;
                    let e = path.pop().unwrap() as usize;
                    u = to[e ^ 1] as usize;
                    it[u] = next[it[u] as usize];
                }
            };
            if !found {
                break;
            }
            let f = path.iter().map(|&e| cap[e as usize]).min().unwrap_or(0);
            for &e in &path {
                cap[e as usize] -= f;
                cap[e as usize ^ 1] += f;
            }
        }
    }
    // reachable from the source in the residual graph
    let mut reach = vec![false; nn];
    let mut stack = vec![s];
    reach[s] = true;
    while let Some(u) = stack.pop() {
        let mut e = head[u];
        while e != u32::MAX {
            let v = to[e as usize] as usize;
            if cap[e as usize] > 0 && !reach[v] {
                reach[v] = true;
                stack.push(v);
            }
            e = next[e as usize];
        }
    }
    let cut: Vec<u32> = (0..k).filter(|&i| reach[2 * i] && !reach[2 * i + 1]).map(|i| part[i]).collect();
    let side: Vec<bool> = (0..k).map(|i| reach[2 * i + 1]).collect();
    (cut, side)
}


/// The chordal supergraph of the network for an order (see `chordal`).
pub fn cch_topology(net: &RoutingNet, rank: Vec<u32>, progress: &dyn Fn(f64)) -> CchTopology {
    chordal(&neighbours(net), rank, progress)
}

/// The chordal supergraph for an order: removing a junction connects all its higher
/// neighbours; per junction its higher neighbours, sorted by junction.
fn chordal(nb: &[Vec<u32>], rank: Vec<u32>, progress: &dyn Fn(f64)) -> CchTopology {
    let n = rank.len();
    let mut order = vec![0u32; n];
    for v in 0..n {
        order[rank[v] as usize] = v as u32;
    }
    let mut up: Vec<Vec<u32>> = nb.par_iter().enumerate().map(|(u, ns)| ns.iter().copied().filter(|&v| rank[v as usize] > rank[u]).collect()).collect();
    for (k, &v) in order.iter().enumerate() {
        let mut ns = std::mem::take(&mut up[v as usize]);
        ns.sort_unstable_by_key(|&x| rank[x as usize]);
        ns.dedup();
        // the lowest higher neighbour inherits all others (fill-in)
        if let Some((&first, rest)) = ns.split_first() {
            up[first as usize].extend_from_slice(rest);
        }
        up[v as usize] = ns;
        if k % 65_536 == 0 {
            progress(k as f64 / n as f64);
        }
    }
    let mut off = vec![0u32; n + 1];
    for v in 0..n {
        off[v + 1] = off[v] + up[v].len() as u32;
    }
    let mut nb = Vec::with_capacity(off[n] as usize);
    for mut ns in up {
        ns.sort_unstable();
        nb.extend_from_slice(&ns);
    }
    progress(1.0);
    CchTopology { rank, off, nb }
}

/// Times per arc {lo, hi} of a topology, upwards and downwards, with the bypassed junction
/// (only when asked for).
struct Metric {
    up: Vec<u32>,
    dn: Vec<u32>,
    mid_up: Vec<u32>,
    mid_dn: Vec<u32>,
}

/// The travel times of a mode for a CCH: the basic times per arc (used to unpack shortcuts)
/// and the arcs that searches can skip.
pub struct Weights {
    pub up: Vec<u32>,
    pub dn: Vec<u32>,
    pub pruned_up: Vec<u64>,
    pub pruned_dn: Vec<u64>,
}

impl CchTopology {
    fn higher(&self, v: u32) -> &[u32] {
        &self.nb[self.off[v as usize] as usize..self.off[v as usize + 1] as usize]
    }

    fn arc_index(&self, lo: u32, hi: u32) -> Option<usize> {
        self.higher(lo).binary_search(&hi).ok().map(|i| self.off[lo as usize] as usize + i)
    }

    /// Basic customization: every arc takes the best of its arcs (a, b, deciseconds, mid)
    /// and its lower triangles, from the lowest junctions upwards; on ties the given arc,
    /// then the lowest triangle. With `ids`, the bypassed junctions are kept (as ids[v]).
    fn customize_arcs(&self, arcs: impl Iterator<Item = (u32, u32, u32, u32)>, ids: Option<&[u32]>) -> Metric {
        let n = self.rank.len();
        let k = self.nb.len();
        let mut up = vec![INF; k];
        let mut dn = vec![INF; k];
        let with_mid = ids.is_some();
        let (mut mid_up, mut mid_dn) = if with_mid { (vec![NONE; k], vec![NONE; k]) } else { (Vec::new(), Vec::new()) };
        for (a, b, c, mid) in arcs {
            let (lo, hi, upward) = if self.rank[a as usize] < self.rank[b as usize] { (a, b, true) } else { (b, a, false) };
            let Some(i) = self.arc_index(lo, hi) else { continue };
            let (w, m) = if upward { (&mut up, &mut mid_up) } else { (&mut dn, &mut mid_dn) };
            if c < w[i] {
                w[i] = c;
                if with_mid {
                    m[i] = mid;
                }
            }
        }
        let mut order = vec![0u32; n];
        for v in 0..n {
            order[self.rank[v] as usize] = v as u32;
        }
        for &v in &order {
            let hs = self.higher(v);
            let base = self.off[v as usize] as usize;
            for (i, &x) in hs.iter().enumerate() {
                for (j, &y) in hs.iter().enumerate() {
                    if i == j || self.rank[x as usize] > self.rank[y as usize] {
                        continue;
                    }
                    // arc {x, y} with x lower: x -> y via v and y -> x via v
                    let Some(a) = self.arc_index(x, y) else { continue };
                    let via_up = add(dn[base + i], up[base + j]);
                    let via_dn = add(dn[base + j], up[base + i]);
                    if via_up < up[a] {
                        up[a] = via_up;
                        if let Some(ids) = ids {
                            mid_up[a] = ids[v as usize];
                        }
                    }
                    if via_dn < dn[a] {
                        dn[a] = via_dn;
                        if let Some(ids) = ids {
                            mid_dn[a] = ids[v as usize];
                        }
                    }
                }
            }
        }
        Metric { up, dn, mid_up, mid_dn }
    }

    /// The times of all modes, with perfect customization. Basic: every arc takes the best of
    /// its edge and its lower triangles (on ties the edge, then the lowest triangle); then,
    /// from the highest junctions down, an arc x -> y whose time a path over a higher
    /// junction z beats (x -> z, then the best time from z to y) is never needed and marked
    /// for searches to skip. Both run level by level of the elimination tree, the junctions
    /// of a level in parallel (each writes only its own arcs), all modes at once.
    pub fn customize_all(&self, net: &RoutingNet) -> Vec<Weights> {
        let n = self.rank.len();
        let k = self.nb.len();
        let mut up = vec![[INF; 3]; k];
        let mut dn = vec![[INF; 3]; k];
        for (mi, m) in MODES.iter().enumerate() {
            for (a, b, c) in arcs(net, *m) {
                let (lo, hi, upward) = if self.rank[a as usize] < self.rank[b as usize] { (a, b, true) } else { (b, a, false) };
                let Some(i) = self.arc_index(lo, hi) else { continue };
                let w = if upward { &mut up[i][mi] } else { &mut dn[i][mi] };
                *w = (*w).min(c);
            }
        }
        // lower neighbours (ascending rank) and levels from below and from above
        let mut order = vec![0u32; n];
        for v in 0..n {
            order[self.rank[v] as usize] = v as u32;
        }
        let mut loff = vec![0u32; n + 1];
        for &x in &self.nb {
            loff[x as usize + 1] += 1;
        }
        for v in 0..n {
            loff[v + 1] += loff[v];
        }
        let mut fill = loff.clone();
        let mut lnb = vec![0u32; k];
        for &v in &order {
            for &x in self.higher(v) {
                lnb[fill[x as usize] as usize] = v;
                fill[x as usize] += 1;
            }
        }
        drop(fill);
        let lower = |x: u32| &lnb[loff[x as usize] as usize..loff[x as usize + 1] as usize];
        let mut level = vec![0u32; n];
        for &x in &order {
            level[x as usize] = lower(x).iter().map(|&v| level[v as usize] + 1).max().unwrap_or(0);
        }
        let groups = |level: &[u32]| {
            let top = level.iter().copied().max().unwrap_or(0) as usize;
            let mut g: Vec<Vec<u32>> = vec![Vec::new(); top + 1];
            for v in 0..n as u32 {
                g[level[v as usize] as usize].push(v);
            }
            g
        };
        let add3 = |a: [u32; 3], b: [u32; 3]| [add(a[0], b[0]), add(a[1], b[1]), add(a[2], b[2])];
        // basic, from below: the arcs of x from its lower triangles
        for g in groups(&level) {
            let (pu, pd) = (SyncPtr(up.as_mut_ptr()), SyncPtr(dn.as_mut_ptr()));
            g.par_iter().for_each(|&x| {
                let base = self.off[x as usize] as usize;
                let len = self.higher(x).len();
                // SAFETY: only the arcs of x are written; the arcs read belong to lower junctions,
                // which are on earlier levels.
                let (ux, dx) = unsafe { (std::slice::from_raw_parts_mut(pu.get().add(base), len), std::slice::from_raw_parts_mut(pd.get().add(base), len)) };
                let (upr, dnr) = (|i: usize| unsafe { *pu.get().add(i) }, |i: usize| unsafe { *pd.get().add(i) });
                if len * lower(x).len() > 20_000 {
                    // a large junction (high in the hierarchy): its arcs in parallel, each from
                    // the lower triangles in ascending rank
                    let lw: Vec<(u32, [u32; 3], [u32; 3])> =
                        lower(x).iter().filter_map(|&v| self.arc_index(v, x).map(|ix| (v, upr(ix), dnr(ix)))).collect();
                    let hs = self.higher(x);
                    ux.par_iter_mut().zip(dx.par_iter_mut()).enumerate().for_each(|(a, (u, d))| {
                        let y = hs[a];
                        for &(v, vx, xv) in &lw {
                            let Some(iy) = self.arc_index(v, y) else { continue };
                            let via_up = add3(xv, upr(iy));
                            let via_dn = add3(dnr(iy), vx);
                            for mi in 0..3 {
                                if via_up[mi] < u[mi] {
                                    u[mi] = via_up[mi];
                                }
                                if via_dn[mi] < d[mi] {
                                    d[mi] = via_dn[mi];
                                }
                            }
                        }
                    });
                    return;
                }
                for &v in lower(x) {
                    let vb = self.off[v as usize] as usize;
                    let Some(ix) = self.arc_index(v, x) else { continue };
                    let (vx, xv) = (upr(ix), dnr(ix));
                    for (j, &y) in self.higher(v).iter().enumerate() {
                        if self.rank[y as usize] <= self.rank[x as usize] {
                            continue;
                        }
                        let Some(a) = self.arc_index(x, y) else { continue };
                        let a = a - base;
                        let via_up = add3(xv, upr(vb + j));
                        let via_dn = add3(dnr(vb + j), vx);
                        for mi in 0..3 {
                            if via_up[mi] < ux[a][mi] {
                                ux[a][mi] = via_up[mi];
                            }
                            if via_dn[mi] < dx[a][mi] {
                                dx[a][mi] = via_dn[mi];
                            }
                        }
                    }
                }
            });
        }
        // perfect, from above
        let mut best_up = up.clone();
        let mut best_dn = dn.clone();
        let mut top = vec![0u32; n];
        for &x in order.iter().rev() {
            top[x as usize] = self.higher(x).iter().map(|&y| top[y as usize] + 1).max().unwrap_or(0);
        }
        for g in groups(&top) {
            let (pu, pd) = (SyncPtr(best_up.as_mut_ptr()), SyncPtr(best_dn.as_mut_ptr()));
            let (up, dn) = (&up, &dn);
            g.par_iter().for_each(|&x| {
                let hs = self.higher(x);
                let base = self.off[x as usize] as usize;
                // SAFETY: only the arcs of x are written; the arcs read join higher junctions
                // and belong to them, which are on earlier levels.
                let (bu, bd) = unsafe { (std::slice::from_raw_parts_mut(pu.get().add(base), hs.len()), std::slice::from_raw_parts_mut(pd.get().add(base), hs.len())) };
                let (bur, bdr) = (|i: usize| unsafe { *pu.get().add(i) }, |i: usize| unsafe { *pd.get().add(i) });
                let arc = |i: usize, y: u32, u: &mut [u32; 3], d: &mut [u32; 3]| {
                    for (j, &z) in hs.iter().enumerate() {
                        if i == j {
                            continue;
                        }
                        let (zy, yz) = if self.rank[z as usize] < self.rank[y as usize] {
                            let a = self.arc_index(z, y).unwrap();
                            (bur(a), bdr(a))
                        } else {
                            let a = self.arc_index(y, z).unwrap();
                            (bdr(a), bur(a))
                        };
                        let c_up = add3(up[base + j], zy);
                        let c_dn = add3(yz, dn[base + j]);
                        for mi in 0..3 {
                            if c_up[mi] < u[mi] {
                                u[mi] = c_up[mi];
                            }
                            if c_dn[mi] < d[mi] {
                                d[mi] = c_dn[mi];
                            }
                        }
                    }
                };
                if hs.len() * hs.len() > 20_000 {
                    bu.par_iter_mut().zip(bd.par_iter_mut()).enumerate().for_each(|(i, (u, d))| arc(i, hs[i], u, d));
                } else {
                    for i in 0..hs.len() {
                        arc(i, hs[i], &mut bu[i], &mut bd[i]);
                    }
                }
            });
        }
        (0..3)
            .map(|mi| {
                let mut pruned_up = vec![0u64; k.div_ceil(64)];
                let mut pruned_dn = vec![0u64; k.div_ceil(64)];
                for i in 0..k {
                    if best_up[i][mi] < up[i][mi] {
                        pruned_up[i >> 6] |= 1 << (i & 63);
                    }
                    if best_dn[i][mi] < dn[i][mi] {
                        pruned_dn[i >> 6] |= 1 << (i & 63);
                    }
                }
                Weights { up: up.iter().map(|x| x[mi]).collect(), dn: dn.iter().map(|x| x[mi]).collect(), pruned_up, pruned_dn }
            })
            .collect()
    }
}

// ----- storage ------------------------------------------------------------------------------

const MAGIC: &[u8; 8] = b"KHIER002";
const CCH_TOPOLOGY: &[u8; 8] = b"KCCHTOP2";
const CCH_WEIGHTS: &[u8; 8] = b"KCCHWGT2";

fn put<T: Copy>(b: &mut Vec<u8>, xs: &[T]) {
    b.extend_from_slice(&(xs.len() as u64).to_le_bytes());
    // SAFETY: plain old data written as bytes.
    let bytes = unsafe { std::slice::from_raw_parts(xs.as_ptr() as *const u8, std::mem::size_of_val(xs)) };
    b.extend_from_slice(bytes);
    while b.len() % 8 != 0 {
        b.push(0);
    }
}

fn take<T: Copy + Default>(b: &[u8], o: &mut usize) -> Option<Vec<T>> {
    let n = u64::from_le_bytes(b.get(*o..*o + 8)?.try_into().ok()?) as usize;
    *o += 8;
    let size = n.checked_mul(std::mem::size_of::<T>())?;
    let src = b.get(*o..*o + size)?;
    let mut v = vec![T::default(); n];
    // SAFETY: `v` holds exactly `size` bytes of plain old data.
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), v.as_mut_ptr() as *mut u8, size) };
    *o += size;
    *o = o.div_ceil(8) * 8;
    Some(v)
}

/// CH: one section per mode (u8 mode first).
pub fn encode(m: Mode, h: &Hierarchy) -> Vec<u8> {
    let mut b = MAGIC.to_vec();
    b.extend_from_slice(&[mode_idx(m) as u8, 0, 0, 0, 0, 0, 0, 0]);
    put(&mut b, &h.rank);
    put(&mut b, &h.up_off);
    put(&mut b, &h.up_to);
    put(&mut b, &h.up_w);
    put(&mut b, &h.up_mid);
    put(&mut b, &h.dn_off);
    put(&mut b, &h.dn_to);
    put(&mut b, &h.dn_w);
    put(&mut b, &h.dn_mid);
    b
}

fn decode(b: &[u8], n: usize) -> Option<(usize, Hierarchy)> {
    if b.get(0..8)? != MAGIC {
        return None;
    }
    let mode = b[8] as usize;
    let mut o = 16;
    let h = Hierarchy {
        rank: Arc::new(take(b, &mut o)?),
        up_off: Arc::new(take(b, &mut o)?),
        up_to: Arc::new(take(b, &mut o)?),
        up_w: take(b, &mut o)?,
        up_mid: take(b, &mut o)?,
        dn_off: Arc::new(take(b, &mut o)?),
        dn_to: Arc::new(take(b, &mut o)?),
        dn_w: take(b, &mut o)?,
        dn_mid: take(b, &mut o)?,
        pruned_up: Vec::new(),
        pruned_dn: Vec::new(),
        lower: None,
    };
    let ok = h.rank.len() == n
        && h.up_off.len() == n + 1
        && h.dn_off.len() == n + 1
        && h.up_off.last().copied() == Some(h.up_to.len() as u32)
        && h.dn_off.last().copied() == Some(h.dn_to.len() as u32)
        && h.up_w.len() == h.up_to.len()
        && h.dn_w.len() == h.dn_to.len()
        && h.up_mid.len() == h.up_to.len()
        && h.dn_mid.len() == h.dn_to.len()
        && h.up_to.iter().chain(h.dn_to.iter()).all(|&v| (v as usize) < n)
        && h.up_mid.iter().chain(&h.dn_mid).all(|&v| v == NONE || (v as usize) < n);
    ok.then_some((mode, h))
}

/// CCH: the topology, shared by all modes (rank; higher neighbours per junction as CSR).
pub fn encode_topology(t: &CchTopology) -> Vec<u8> {
    let mut b = CCH_TOPOLOGY.to_vec();
    put(&mut b, &t.rank);
    put(&mut b, &t.off);
    put(&mut b, &t.nb);
    b
}

/// CCH: the topology of a stored element, if it is valid for `n` junctions.
fn decode_topology(b: &[u8], n: usize) -> Option<CchTopology> {
    if b.get(0..8) != Some(CCH_TOPOLOGY) {
        return None;
    }
    let mut o = 8;
    let rank: Vec<u32> = take(b, &mut o)?;
    let off: Vec<u32> = take(b, &mut o)?;
    let nb: Vec<u32> = take(b, &mut o)?;
    let ok = rank.len() == n
        && off.len() == n + 1
        && off.windows(2).all(|x| x[0] <= x[1])
        && off.last().copied() == Some(nb.len() as u32)
        && rank.iter().all(|&r| (r as usize) < n)
        && (0..n).all(|v| nb[off[v] as usize..off[v + 1] as usize].iter().all(|&x| (x as usize) < n && rank[x as usize] > rank[v]));
    ok.then_some(CchTopology { rank, off, nb })
}

/// CCH: the times of a mode per arc of the topology, upwards and downwards, and the arcs
/// searches skip.
pub fn encode_weights(m: Mode, w: &Weights) -> Vec<u8> {
    let mut b = CCH_WEIGHTS.to_vec();
    b.extend_from_slice(&[mode_idx(m) as u8, 0, 0, 0, 0, 0, 0, 0]);
    put(&mut b, &w.up);
    put(&mut b, &w.dn);
    put(&mut b, &w.pruned_up);
    put(&mut b, &w.pruned_dn);
    b
}

/// CCH: the hierarchies of the modes from the topology and the weight sections.
fn decode_cch(sections: &[&[u8]], n: usize) -> Option<Vec<(usize, Hierarchy)>> {
    let t = decode_topology(sections.iter().find(|b| b.get(0..8) == Some(CCH_TOPOLOGY))?, n)?;
    // lower neighbours, ascending rank
    let mut order = vec![0u32; n];
    for (v, &r) in t.rank.iter().enumerate() {
        order[r as usize] = v as u32;
    }
    let mut loff = vec![0u32; n + 1];
    for &x in &t.nb {
        loff[x as usize + 1] += 1;
    }
    for v in 0..n {
        loff[v + 1] += loff[v];
    }
    let mut fill = loff.clone();
    let mut lnb = vec![0u32; t.nb.len()];
    for &v in &order {
        for &x in t.higher(v) {
            lnb[fill[x as usize] as usize] = v;
            fill[x as usize] += 1;
        }
    }
    let lower = Some(Arc::new((loff, lnb)));
    let k = t.nb.len();
    let (rank, off, nb) = (Arc::new(t.rank), Arc::new(t.off), Arc::new(t.nb));
    let mut out = Vec::new();
    for b in sections.iter().filter(|b| b.get(0..8) == Some(CCH_WEIGHTS)) {
        let mode = b[8] as usize;
        let mut o = 16;
        let up_w: Vec<u32> = take(b, &mut o)?;
        let dn_w: Vec<u32> = take(b, &mut o)?;
        let pruned_up: Vec<u64> = take(b, &mut o)?;
        let pruned_dn: Vec<u64> = take(b, &mut o)?;
        if up_w.len() != k || dn_w.len() != k || pruned_up.len() != k.div_ceil(64) || pruned_dn.len() != k.div_ceil(64) {
            return None;
        }
        out.push((
            mode,
            Hierarchy {
                rank: rank.clone(),
                up_off: off.clone(),
                up_to: nb.clone(),
                up_w,
                up_mid: Vec::new(),
                dn_off: off.clone(),
                dn_to: nb.clone(),
                dn_w,
                dn_mid: Vec::new(),
                pruned_up,
                pruned_dn,
                lower: lower.clone(),
            },
        ));
    }
    Some(out)
}

/// The hierarchies of one element (CH or CCH), per mode: hierarchy and junctions by rank.
type Loaded = [Option<Arc<(Hierarchy, Vec<u32>)>>; 3];

/// The hierarchies of an element of the database, loaded once (and only when asked for).
fn load_kind(db: &Db, net: &RoutingNet, kind: knut_format::container::ElementKind) -> Option<Arc<Loaded>> {
    use knut_format::container::ElementKind;
    static CACHE: std::sync::Mutex<Vec<(std::path::PathBuf, u64, Arc<Loaded>)>> = std::sync::Mutex::new(Vec::new());
    let el = db.reader.element(kind)?;
    // built on another routing network (its junctions may be numbered differently): unusable
    if el.params.iter().any(|p| p.0 == "network" && p.1 != crate::indexes::network_checksum(&db.reader)) {
        return None;
    }
    let key: u64 = el.sections.iter().map(|s| s.checksum).fold(kind as u64, |a, c| a ^ c);
    let mut c = CACHE.lock().ok()?;
    if let Some(x) = c.iter().find(|x| x.0 == db.path && x.1 == key) {
        return Some(x.2.clone());
    }
    let bytes: Vec<&[u8]> = el.sections.iter().filter_map(|s| db.reader.section_bytes(s).ok()).collect();
    let hs: Vec<(usize, Hierarchy)> = if kind == ElementKind::RoutingCch {
        decode_cch(&bytes, net.n()).unwrap_or_default()
    } else {
        bytes.iter().filter_map(|b| decode(b, net.n())).collect()
    };
    let mut by_mode: Loaded = Default::default();
    for (m, h) in hs {
        let mut order = vec![0u32; h.rank.len()];
        for (v, &r) in h.rank.iter().enumerate() {
            order[r as usize] = v as u32;
        }
        if m < 3 {
            by_mode[m] = Some(Arc::new((h, order)));
        }
    }
    let l = Arc::new(by_mode);
    c.push((db.path.clone(), key, l.clone()));
    Some(l)
}

/// Loads both kinds of hierarchies now (for a server that should answer the first question
/// as fast as the later ones).
pub fn preload(db: &Db, net: &RoutingNet) {
    use knut_format::container::ElementKind::{RoutingCch, RoutingCh};
    load_kind(db, net, RoutingCh);
    load_kind(db, net, RoutingCch);
}

/// The hierarchy that answers a question of a mode fastest, if the container has one: CH
/// for routes by car and bicycle and for isochrones by car (its order is made for the
/// network of the mode, which is thin for cars), CCH otherwise (its order suits the dense
/// network on foot, and its sweep over all junctions is cheaper); the other one if only one
/// exists (measured on Germany).
fn pick(db: &Db, net: &RoutingNet, m: Mode, isochrone: bool) -> Option<Arc<(Hierarchy, Vec<u32>)>> {
    use knut_format::container::ElementKind::{RoutingCch, RoutingCh};
    let i = mode_idx(m);
    let prefer_ch = match i {
        2 => true,
        1 => !isochrone,
        _ => false,
    };
    let (first, second) = if prefer_ch { (RoutingCh, RoutingCch) } else { (RoutingCch, RoutingCh) };
    load_kind(db, net, first).and_then(|l| l[i].clone()).or_else(|| load_kind(db, net, second).and_then(|l| l[i].clone()))
}

/// A route through the fastest hierarchy of the mode, if the container has one.
pub fn route(db: &Db, net: &RoutingNet, a: &Snap, b: &Snap, m: Mode) -> Option<Route> {
    let h = pick(db, net, m, false)?;
    let src = entries(net, a, m, true);
    let dst = entries(net, b, m, false);
    let direct = crate::routing::direct(net, a, b, m);
    let (best, path) = match h.0.query(&src, &dst) {
        Some(x) => x,
        None => {
            return direct.map(|s| Route { seconds: s / 10.0, edges: Vec::new(), from: a.clone(), to: b.clone(), direct: true });
        }
    };
    if let Some(s) = direct
        && s <= best
    {
        return Some(Route { seconds: s / 10.0, edges: Vec::new(), from: a.clone(), to: b.clone(), direct: true });
    }
    let mut edges = Vec::new();
    for w in path.windows(2) {
        h.0.unpack(net, m, w[0], w[1], &mut edges);
    }
    Some(Route { seconds: best / 10.0, edges, from: a.clone(), to: b.clone(), direct: false })
}

/// Deciseconds to every junction up to `limit` seconds (PHAST), if the container has a
/// hierarchy for the mode.
pub fn reach(db: &Db, net: &RoutingNet, sources: &[(u32, f64)], m: Mode, limit: f64) -> Option<Vec<f64>> {
    let h = pick(db, net, m, true)?;
    Some(h.0.phast(&h.1, sources, limit * 10.0))
}
