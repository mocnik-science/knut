//! Segmentation of object histories into spatially compact, time-contiguous segments
//! (SPEC.md §3).

use crate::store::{NodeHist, NodeStore, priv_entry};
use crate::{RawMeta, RawNode, RawRel, RawWay};
use knut_format::*;
use std::ops::Range;

#[derive(Clone, Debug)]
pub struct SegParams {
    /// X: growth/shrink factor on `max(width, height)`.
    pub grow: f64,
    /// S_min: minimum box size in meters used in the growth test.
    pub min_size_m: f64,
    /// Y: jump distance in meters that always starts a new segment.
    pub jump_m: f64,
    /// Segments wider or higher than this (1e-7 degrees) go to the large-object section.
    pub large_threshold_e7: i64,
    pub time_base: i64,
}

impl Default for SegParams {
    fn default() -> Self {
        SegParams {
            grow: 4.0,
            min_size_m: 1000.0,
            jump_m: 10_000.0,
            large_threshold_e7: 10_000_000,
            time_base: 946_684_800,
        }
    }
}

/// Decides whether geometry box `g` starts a new segment given the running box `sb`.
pub fn should_split(p: &SegParams, sb: &BBox, g: &BBox) -> bool {
    if sb.is_empty() || g.is_empty() {
        return false;
    }
    let u = sb.unioned(g);
    let s_sb = sb.size_m().max(p.min_size_m);
    let s_g = g.size_m().max(p.min_size_m);
    u.size_m() > p.grow * s_sb || sb.size_m() > p.grow * s_g || sb.distance_m(g) > p.jump_m
}

fn monotonic(ts: impl Iterator<Item = i64>) -> Vec<i64> {
    let mut m = i64::MIN;
    ts.map(|t| {
        m = m.max(t);
        m
    })
    .collect()
}

fn meta(m: &RawMeta, continuation: bool) -> Meta {
    Meta {
        version: m.version,
        timestamp: m.timestamp,
        changeset: m.changeset,
        uid: m.uid,
        visible: m.visible,
        continuation,
    }
}

/// Splits a version sequence at version boundaries.
fn split_versions(g: &[BBox], p: &SegParams) -> Vec<(Range<usize>, BBox)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut sb = BBox::EMPTY;
    for (i, gb) in g.iter().enumerate() {
        if should_split(p, &sb, gb) {
            out.push((start..i, sb));
            start = i;
            sb = BBox::EMPTY;
        }
        sb.union(gb);
    }
    out.push((start..g.len(), sb));
    out
}

fn finalize(segs: &mut [Segment], p: &SegParams) {
    let multi = segs.len() > 1;
    for s in segs {
        if multi {
            s.flags |= SEG_MULTI;
        }
        if s.bbox.width() > p.large_threshold_e7 || s.bbox.height() > p.large_threshold_e7 {
            s.flags |= SEG_LARGE;
        }
        if s.bbox.is_empty() {
            s.flags |= SEG_NO_BBOX;
        }
    }
}

fn interval_times(eff: &[i64], ranges: &[(Range<usize>, BBox)], last_deleted: bool, p: &SegParams) -> Vec<(u32, u32)> {
    let tb = p.time_base;
    (0..ranges.len())
        .map(|k| {
            let tmin = rel_time(eff[ranges[k].0.start], tb);
            let tmax = if k + 1 < ranges.len() {
                rel_time(eff[ranges[k + 1].0.start], tb)
            } else if last_deleted {
                rel_time(*eff.last().unwrap(), tb)
            } else {
                T_OPEN
            };
            (tmin, tmax.max(tmin))
        })
        .collect()
}

pub fn segment_node(id: i64, versions: &[RawNode], p: &SegParams) -> Vec<Segment> {
    let eff = monotonic(versions.iter().map(|v| v.meta.timestamp));
    let g: Vec<BBox> =
        versions.iter().map(|v| if v.meta.visible { BBox::point(v.lon, v.lat) } else { BBox::EMPTY }).collect();
    let ranges = split_versions(&g, p);
    let times = interval_times(&eff, &ranges, !versions.last().unwrap().meta.visible, p);
    let mut segs: Vec<Segment> = ranges
        .into_iter()
        .zip(times)
        .map(|((r, bbox), (tmin, tmax))| Segment {
            id,
            flags: 0,
            tmin,
            tmax,
            bbox,
            body: SegmentBody::Node(
                versions[r]
                    .iter()
                    .map(|v| NodeVersion {
                        meta: meta(&v.meta, false),
                        tags: v.tags.clone(),
                        lon: if v.meta.visible { v.lon } else { 0 },
                        lat: if v.meta.visible { v.lat } else { 0 },
                    })
                    .collect(),
            ),
        })
        .collect();
    finalize(&mut segs, p);
    segs
}

#[derive(Clone, Copy)]
enum Ev {
    Ver(u32),
    Node(u32, u32),
}

pub fn segment_way(id: i64, versions: &[RawWay], store: &NodeStore, p: &SegParams) -> Vec<Segment> {
    let nv = versions.len();
    let veff = monotonic(versions.iter().map(|v| v.meta.timestamp));

    // global slot table (first appearance order)
    let mut slot_ids: Vec<i64> = Vec::new();
    let mut slot_of: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();
    let vlists: Vec<Vec<u32>> = versions
        .iter()
        .map(|v| {
            v.refs
                .iter()
                .map(|&r| {
                    *slot_of.entry(r).or_insert_with(|| {
                        slot_ids.push(r);
                        (slot_ids.len() - 1) as u32
                    })
                })
                .collect()
        })
        .collect();
    let ns = slot_ids.len();
    let hist: Vec<Option<NodeHist>> = slot_ids.iter().map(|&n| store.get(n)).collect();
    let private: Vec<bool> = hist.iter().map(|o| o.as_ref().map_or(false, |h| h.private)).collect();
    let neff: Vec<Vec<i64>> = hist.iter().map(|o| o.as_ref().map_or_else(Vec::new, |h| h.eff_times())).collect();

    // events
    let mut evs: Vec<(i64, Ev)> = Vec::with_capacity(nv + neff.iter().map(|v| v.len()).sum::<usize>());
    for (vi, &t) in veff.iter().enumerate() {
        evs.push((t, Ev::Ver(vi as u32)));
    }
    for (s, ts) in neff.iter().enumerate() {
        for (k, &t) in ts.iter().enumerate() {
            evs.push((t, Ev::Node(s as u32, k as u32)));
        }
    }
    evs.sort_by_key(|e| e.0);

    let mut in_way = vec![0u32; ns];
    let mut pos: Vec<Option<(i32, i32)>> = vec![None; ns];
    let mut cur: Option<usize> = None;
    let mut starts: Vec<i64> = Vec::new();
    let mut sbs: Vec<BBox> = vec![BBox::EMPTY];
    let mut sbg = BBox::EMPTY;
    let mut touched: Vec<u32> = Vec::new();
    let mut i = 0;
    while i < evs.len() {
        let t = evs[i].0;
        touched.clear();
        while i < evs.len() && evs[i].0 == t {
            match evs[i].1 {
                Ev::Ver(vi) => {
                    if let Some(c) = cur {
                        for &s in &vlists[c] {
                            in_way[s as usize] -= 1;
                        }
                    }
                    for &s in &vlists[vi as usize] {
                        in_way[s as usize] += 1;
                    }
                    cur = Some(vi as usize);
                }
                Ev::Node(s, k) => {
                    let e = hist[s as usize].as_ref().unwrap().entries[k as usize];
                    pos[s as usize] = e.visible.then_some((e.lon, e.lat));
                    touched.push(s);
                }
            }
            i += 1;
        }
        let mut g = BBox::EMPTY;
        if let Some(c) = cur {
            if versions[c].meta.visible {
                for &s in &vlists[c] {
                    if let Some((x, y)) = pos[s as usize] {
                        g.extend(x, y);
                    }
                }
            }
        }
        let mut extra = BBox::EMPTY;
        for &s in &touched {
            if private[s as usize] && in_way[s as usize] == 0 {
                if let Some((x, y)) = pos[s as usize] {
                    extra.extend(x, y);
                }
            }
        }
        if should_split(p, &sbg, &g) {
            starts.push(t);
            sbs.push(BBox::EMPTY);
            sbg = BBox::EMPTY;
        }
        sbg.union(&g);
        let sb = sbs.last_mut().unwrap();
        sb.union(&g);
        sb.union(&extra);
    }

    let nseg = starts.len() + 1;
    let seg_of = |t: i64| starts.partition_point(|&s| s <= t);
    let mut seg_versions: Vec<Vec<usize>> = vec![Vec::new(); nseg];
    for (vi, &t) in veff.iter().enumerate() {
        seg_versions[seg_of(t)].push(vi);
    }
    let last_deleted = !versions[nv - 1].meta.visible;

    let tb = p.time_base;
    let mut local = vec![u32::MAX; ns];
    let mut segs = Vec::with_capacity(nseg);
    for k in 0..nseg {
        let start = if k == 0 { i64::MIN } else { starts[k - 1] };
        let end = if k + 1 < nseg { starts[k] } else { i64::MAX };
        let mut recs: Vec<(usize, bool)> = Vec::new();
        if k > 0 && seg_versions[k].first().map(|&vi| veff[vi]) != Some(start) {
            let n_before = veff.partition_point(|&t| t < start);
            if n_before > 0 {
                recs.push((n_before - 1, true));
            }
        }
        recs.extend(seg_versions[k].iter().map(|&vi| (vi, false)));

        local.iter_mut().for_each(|l| *l = u32::MAX);
        let mut order: Vec<u32> = Vec::new();
        for &(vi, _) in &recs {
            for &s in &vlists[vi] {
                if local[s as usize] == u32::MAX {
                    local[s as usize] = order.len() as u32;
                    order.push(s);
                }
            }
        }
        let mut entry_ranges: Vec<(usize, usize)> = vec![(0, 0); ns];
        for s in 0..ns {
            if private[s] {
                let lo = neff[s].partition_point(|&t| t < start);
                let hi = neff[s].partition_point(|&t| t < end);
                entry_ranges[s] = (lo, hi);
                if lo < hi && local[s] == u32::MAX {
                    local[s] = order.len() as u32;
                    order.push(s as u32);
                }
            }
        }

        let mut tmin_abs = i64::MAX;
        let mut tmax_rec = i64::MIN;
        for &(vi, cont) in &recs {
            if !cont {
                tmin_abs = tmin_abs.min(veff[vi]);
                tmax_rec = tmax_rec.max(veff[vi]);
            }
        }
        let mut all_private_deleted = true;
        let slots: Vec<Slot> = order
            .iter()
            .map(|&s| {
                let s = s as usize;
                let private_entries = private[s].then(|| {
                    let h = hist[s].as_ref().unwrap();
                    let (lo, hi) = entry_ranges[s];
                    let mut es = Vec::with_capacity(hi - lo + 1);
                    if k > 0 && lo > 0 {
                        es.push(priv_entry(&h.entries[lo - 1], true));
                    }
                    for j in lo..hi {
                        es.push(priv_entry(&h.entries[j], false));
                        tmin_abs = tmin_abs.min(neff[s][j]);
                        tmax_rec = tmax_rec.max(neff[s][j]);
                    }
                    if h.entries.last().map_or(false, |e| e.visible) {
                        all_private_deleted = false;
                    }
                    es
                });
                Slot { node_id: slot_ids[s], private: private_entries }
            })
            .collect();
        let tmin = if k == 0 { rel_time(tmin_abs.min(veff[0]), tb) } else { rel_time(start, tb) };
        let tmax = if k + 1 < nseg {
            rel_time(end, tb)
        } else if last_deleted && all_private_deleted {
            rel_time(tmax_rec.max(veff[nv - 1]), tb)
        } else {
            T_OPEN
        };
        let versions_out: Vec<WayVersion> = recs
            .iter()
            .map(|&(vi, cont)| WayVersion {
                meta: meta(&versions[vi].meta, cont),
                tags: versions[vi].tags.clone(),
                nodes: vlists[vi].iter().map(|&s| local[s as usize]).collect(),
            })
            .collect();
        segs.push(Segment {
            id,
            flags: 0,
            tmin,
            tmax: tmax.max(tmin),
            bbox: sbs[k],
            body: SegmentBody::Way { slots, versions: versions_out },
        });
    }
    finalize(&mut segs, p);
    segs
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct WaySeg {
    t0: i64,
    t1: i64,
    bbox: [i32; 4],
}

/// Per-way list of segment boxes over time, used to derive relation boxes. Written in way
/// id order into uncompressed memory-mapped columns (bounded memory, random access).
pub struct WaySumsWriter {
    ids: crate::rawcols::ColWriter<i64>,
    start: crate::rawcols::ColWriter<u64>,
    segs: crate::rawcols::ColWriter<WaySeg>,
    n_segs: u64,
}

impl WaySumsWriter {
    pub fn new(dir: &std::path::Path) -> crate::Result<Self> {
        Ok(WaySumsWriter {
            ids: crate::rawcols::ColWriter::new(dir, "way-ids")?,
            start: crate::rawcols::ColWriter::new(dir, "way-start")?,
            segs: crate::rawcols::ColWriter::new(dir, "way-segs")?,
            n_segs: 0,
        })
    }

    pub fn push(&mut self, id: i64, segs: &[Segment], tb: i64) -> crate::Result<()> {
        self.ids.push(id)?;
        self.start.push(self.n_segs)?;
        for s in segs {
            let b = s.bbox;
            self.segs.push(WaySeg {
                t0: abs_time(s.tmin, tb),
                t1: abs_time(s.tmax, tb),
                bbox: [b.minx, b.miny, b.maxx, b.maxy],
            })?;
            self.n_segs += 1;
        }
        Ok(())
    }

    pub fn finish(mut self, _cache_bytes: usize) -> crate::Result<WaySums> {
        self.start.push(self.n_segs)?;
        let n_ways = self.ids.len() as u64;
        Ok(WaySums {
            ids: crate::rawcols::IdCol::new(self.ids.finish()?),
            start: self.start.finish()?,
            segs: self.segs.finish()?,
            n_ways,
        })
    }
}

pub struct WaySums {
    ids: crate::rawcols::IdCol,
    start: crate::rawcols::ColMap<u64>,
    segs: crate::rawcols::ColMap<WaySeg>,
    pub n_ways: u64,
}

impl WaySums {
    pub fn segs_of(&self, id: i64) -> Vec<(i64, i64, BBox)> {
        let Some(k) = self.ids.find(id) else { return Vec::new() };
        let st = self.start.as_slice();
        self.segs.as_slice()[st[k] as usize..st[k + 1] as usize]
            .iter()
            .map(|s| (s.t0, s.t1, BBox::new(s.bbox[0], s.bbox[1], s.bbox[2], s.bbox[3])))
            .collect()
    }
}

/// Per-relation version boxes before/after resolving child relations.
pub struct RelBase {
    pub id: i64,
    pub eff: Vec<i64>,
    pub base: Vec<BBox>,
    pub boxes: Vec<BBox>,
    pub children: Vec<(u32, i64)>,
}

pub fn rel_base(id: i64, versions: &[RawRel], store: &NodeStore, ways: &WaySums) -> RelBase {
    use std::collections::HashMap;
    let eff = monotonic(versions.iter().map(|v| v.meta.timestamp));
    let mut base = Vec::with_capacity(versions.len());
    let mut children = Vec::new();
    // Per-member time intervals with boxes, looked up once per relation.
    let mut cache: HashMap<(ObjType, i64), Vec<(i64, i64, BBox)>> = HashMap::new();
    for (vi, v) in versions.iter().enumerate() {
        let mut bb = BBox::EMPTY;
        if v.meta.visible {
            let a = eff[vi];
            let b = eff.get(vi + 1).copied().unwrap_or(i64::MAX);
            for m in &v.members {
                if m.kind == ObjType::Relation {
                    children.push((vi as u32, m.id));
                    continue;
                }
                let iv = cache.entry((m.kind, m.id)).or_insert_with(|| match m.kind {
                    ObjType::Node => store.get(m.id).map_or_else(Vec::new, |h| h.intervals()),
                    _ => ways.segs_of(m.id),
                });
                for &(t0, t1, mb) in iv.iter() {
                    if t0 < b && t1 >= a {
                        bb.union(&mb);
                    }
                }
            }
        }
        base.push(bb);
    }
    RelBase { id, eff, boxes: base.clone(), base, children }
}

/// Adds child relation boxes iteratively up to `depth` levels. `rels` must be sorted by id.
pub fn resolve_relations(rels: &mut [RelBase], depth: u32) {
    let ids: Vec<i64> = rels.iter().map(|r| r.id).collect();
    let effs: Vec<Vec<i64>> = rels.iter().map(|r| r.eff.clone()).collect();
    for _ in 0..depth {
        let prev: Vec<Vec<BBox>> = rels.iter().map(|r| r.boxes.clone()).collect();
        let mut changed = false;
        for r in rels.iter_mut() {
            if r.children.is_empty() {
                continue;
            }
            let mut nb = r.base.clone();
            for &(vi, child) in &r.children {
                let vi = vi as usize;
                let a = r.eff[vi];
                let b = r.eff.get(vi + 1).copied().unwrap_or(i64::MAX);
                let Ok(ci) = ids.binary_search(&child) else { continue };
                let ceff = &effs[ci];
                for (cv, cb) in prev[ci].iter().enumerate() {
                    let ca = ceff[cv];
                    let cend = ceff.get(cv + 1).copied().unwrap_or(i64::MAX);
                    if ca < b && cend > a {
                        nb[vi].union(cb);
                    }
                }
            }
            if nb != r.boxes {
                r.boxes = nb;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

pub fn segment_rel(id: i64, versions: &[RawRel], boxes: &[BBox], p: &SegParams) -> Vec<Segment> {
    let eff = monotonic(versions.iter().map(|v| v.meta.timestamp));
    let g: Vec<BBox> =
        versions.iter().zip(boxes).map(|(v, b)| if v.meta.visible { *b } else { BBox::EMPTY }).collect();
    let ranges = split_versions(&g, p);
    let times = interval_times(&eff, &ranges, !versions.last().unwrap().meta.visible, p);
    let mut segs: Vec<Segment> = ranges
        .into_iter()
        .zip(times)
        .map(|((r, bbox), (tmin, tmax))| Segment {
            id,
            flags: 0,
            tmin,
            tmax,
            bbox,
            body: SegmentBody::Relation(
                versions[r]
                    .iter()
                    .map(|v| RelVersion { meta: meta(&v.meta, false), tags: v.tags.clone(), members: v.members.clone() })
                    .collect(),
            ),
        })
        .collect();
    finalize(&mut segs, p);
    segs
}
