//! Scans over the database (KNUT_LANG.md §7.3–7.4): the only source of OSM elements.
//! Region and time restrictions go to the R-trees and block directories, tag conditions
//! are evaluated once per tagset of a block, and blocks inside the region are decoded
//! without geometry when no geometry is needed.

use crate::areas;
use crate::db::{Db, entry_at, version_at};
use crate::geom::{self, BBoxF, Geom, P, Polygon};
use crate::region::{Cover, Region};
use crate::value::*;
use knut::DecodedBlock;
use knut_format::bloom::{self, Bloom};
use knut_format::layout::{RNode, TREE_LARGE};
use knut_format::*;
use knut_plan::pushdown::TagPred;
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KindMask {
    pub node: bool,
    pub way: bool,
    pub area: bool,
    pub relation: bool,
    /// `*`: every object once, closed areas as `area`.
    pub any: bool,
}

impl KindMask {
    pub fn of(k: knut_lang::ast::Kind) -> KindMask {
        use knut_lang::ast::Kind;
        let mut m = KindMask::default();
        match k {
            Kind::Node => m.node = true,
            Kind::Way => m.way = true,
            Kind::Area => m.area = true,
            Kind::Relation => m.relation = true,
            _ => m.any = true,
        }
        m
    }

    fn visits(&self, t: ObjType) -> bool {
        match t {
            ObjType::Node => self.node || self.any,
            ObjType::Way => self.way || self.area || self.any,
            ObjType::Relation => self.relation || self.area || self.any,
        }
    }
}

#[derive(Clone, Debug)]
pub enum TimeMode {
    /// State at a moment.
    State(i64),
    /// Versions with timestamp in `[t0, t1)`.
    Events(i64, i64),
    /// Whole histories (for sequences).
    History,
    /// State at the start of every bucket.
    Series(Arc<Vec<i64>>),
}

#[derive(Clone, Debug)]
pub struct ScanSpec {
    pub kinds: KindMask,
    pub time: TimeMode,
    pub region: Option<Arc<Region>>,
    /// Necessary condition on the tags (prefilter).
    pub pred: TagPred,
    pub need_geom: bool,
    pub need_hist: bool,
}

pub enum Item {
    Elem(Arc<Elem>),
    Hist(Arc<Hist>),
}

fn rt_err(e: impl std::fmt::Display) -> RtErr {
    RtErr::internal(knut_lang::diag::Span::default(), e)
}

/// Bloom hashes all matching blocks must contain; `None` if nothing can match.
fn bloom_hashes(db: &Db, pred: &TagPred) -> Option<Vec<u64>> {
    let mut out = Vec::new();
    for (k, v) in pred.required() {
        let kid = db.reader.key_id(&k)?;
        out.push(bloom::hash_key(kid));
        // values outside the dictionary are stored inline and have no bloom entry
        if let Some(vid) = v.and_then(|v| db.reader.value_id(&v)) {
            out.push(bloom::hash_kv(kid, vid));
        }
    }
    Some(out)
}

fn want_private(spec: &ScanSpec) -> bool {
    (spec.kinds.node || spec.kinds.any) && spec.pred.eval::<&str, &str>(&[])
}

fn time_range(db: &Db, t: &TimeMode) -> (u32, u32) {
    match t {
        TimeMode::State(t) => (db.rel(*t), db.rel(*t)),
        TimeMode::Events(a, b) => (db.rel(*a), db.rel(b.saturating_sub(1))),
        TimeMode::History => (0, u32::MAX),
        TimeMode::Series(s) => (db.rel(*s.first().unwrap_or(&0)), db.rel(*s.last().unwrap_or(&0))),
    }
}

/// Blocks that can contain matches, in ascending order.
pub fn blocks(db: &Db, spec: &ScanSpec) -> R<Vec<u32>> {
    let rb = spec.region.as_ref().map_or(knut_format::BBox::WORLD, |r| r.bbox().to_e7());
    let (t0, t1) = time_range(db, &spec.time);
    let private = want_private(spec);
    let hashes = bloom_hashes(db, &spec.pred);
    let mut out = Vec::new();
    let r = &db.reader;
    if let Some(hashes) = &hashes {
        let pred = |n: &RNode, b: &Bloom| -> bool {
            n.bbox.intersects(&rb) && n.tmin <= t1 && t0 <= n.tmax && hashes.iter().all(|h| bloom::test(b, *h))
        };
        for t in ObjType::ALL {
            if spec.kinds.visits(t) {
                out.extend(r.query_tree(t as usize, &pred).map_err(rt_err)?);
            }
        }
        out.extend(r.query_tree(TREE_LARGE, &pred).map_err(rt_err)?.into_iter().filter(|&no| {
            r.block_entry(no).map(|e| spec.kinds.visits(e.obj_type)).unwrap_or(false)
        }));
    }
    if private {
        let pred = |n: &RNode, _: &Bloom| -> bool { n.bbox.intersects(&rb) && n.tmin <= t1 && t0 <= n.tmax };
        out.extend(r.query_tree(ObjType::Way as usize, &pred).map_err(rt_err)?);
        out.extend(
            r.query_tree(TREE_LARGE, &pred)
                .map_err(rt_err)?
                .into_iter()
                .filter(|&no| r.block_entry(no).map(|e| e.obj_type == ObjType::Way).unwrap_or(false)),
        );
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Element waiting for de-duplication (objects split over several segments).
pub struct Deferred {
    item: Item,
    /// False for a stand-in that only records the newest version of a multi-segment
    /// object at a moment (deleted or not matching); it wins de-duplication but is not emitted.
    live: bool,
}

fn ver_node(v: &NodeVersion) -> Ver {
    Ver {
        version: v.meta.version,
        ts: v.meta.timestamp,
        changeset: v.meta.changeset,
        uid: v.meta.uid,
        visible: v.meta.visible,
        tags: v.tags.clone(),
        nodes: Arc::from(Vec::new()),
        members: Arc::from(Vec::new()),
        pos: v.meta.visible.then_some((v.lon, v.lat)),
    }
}

fn ver_priv(e: &PrivEntry) -> Ver {
    Ver {
        version: e.version,
        ts: e.timestamp,
        changeset: e.changeset,
        uid: e.uid,
        visible: e.visible,
        tags: Arc::from(Vec::new()),
        nodes: Arc::from(Vec::new()),
        members: Arc::from(Vec::new()),
        pos: e.visible.then_some((e.lon, e.lat)),
    }
}

fn ver_way(v: &WayVersion, slots: &[Slot]) -> Ver {
    Ver {
        version: v.meta.version,
        ts: v.meta.timestamp,
        changeset: v.meta.changeset,
        uid: v.meta.uid,
        visible: v.meta.visible,
        tags: v.tags.clone(),
        nodes: v.nodes.iter().map(|&i| slots[i as usize].node_id).collect::<Vec<_>>().into(),
        members: Arc::from(Vec::new()),
        pos: None,
    }
}

fn ver_rel(v: &RelVersion, members: Vec<Member>) -> Ver {
    Ver {
        version: v.meta.version,
        ts: v.meta.timestamp,
        changeset: v.meta.changeset,
        uid: v.meta.uid,
        visible: v.meta.visible,
        tags: v.tags.clone(),
        nodes: Arc::from(Vec::new()),
        members: members.into(),
        pos: None,
    }
}

fn metas(s: &Segment) -> Vec<&Meta> {
    match &s.body {
        SegmentBody::Node(vs) => vs.iter().map(|v| &v.meta).collect(),
        SegmentBody::Way { versions, .. } => versions.iter().map(|v| &v.meta).collect(),
        SegmentBody::Relation(vs) => vs.iter().map(|v| &v.meta).collect(),
    }
}

/// Candidate version indexes of a segment with the time at which each is evaluated.
/// With `all`, versions that are not visible are kept too (multi-segment objects decoded
/// without geometry, whose segment intervals are not exact).
fn candidates(spec: &ScanSpec, ms: &[&Meta], all: bool) -> Vec<(usize, i64)> {
    match &spec.time {
        TimeMode::State(t) => match version_at(ms, |m| m.timestamp, *t) {
            Some(vi) if all || ms[vi].visible => vec![(vi, *t)],
            _ => Vec::new(),
        },
        TimeMode::Events(a, b) => ms
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.continuation && m.timestamp >= *a && m.timestamp < *b)
            .map(|(i, m)| (i, m.timestamp))
            .collect(),
        TimeMode::Series(starts) => starts
            .iter()
            .filter_map(|&s| version_at(ms, |m| m.timestamp, s).filter(|&vi| all || ms[vi].visible).map(|vi| (vi, s)))
            .collect(),
        TimeMode::History => Vec::new(),
    }
}

/// Kind under which a version is emitted, if any.
fn classify(kinds: &KindMask, t: ObjType, v: &Ver) -> Vec<EKind> {
    match t {
        ObjType::Node => {
            if kinds.node || kinds.any {
                vec![EKind::Node]
            } else {
                vec![]
            }
        }
        ObjType::Way => {
            let area = (kinds.area || kinds.any) && v.closed() && areas::way_is_area(&v.tags);
            let mut out = Vec::new();
            if kinds.any {
                out.push(if area { EKind::Area } else { EKind::Way });
            } else {
                if kinds.way {
                    out.push(EKind::Way);
                }
                if kinds.area && area {
                    out.push(EKind::Area);
                }
            }
            out
        }
        ObjType::Relation => {
            let area = (kinds.area || kinds.any) && areas::relation_is_area(&v.tags);
            let mut out = Vec::new();
            if kinds.any {
                out.push(if area { EKind::Area } else { EKind::Relation });
            } else {
                if kinds.relation {
                    out.push(EKind::Relation);
                }
                if kinds.area && area {
                    out.push(EKind::Area);
                }
            }
            out
        }
    }
}

struct Cand {
    elem: Elem,
    /// Way node positions resolved inside the block (private nodes).
    local: Option<Vec<Option<(i32, i32)>>>,
    partial: bool,
    multi: bool,
}

/// Hist of a whole segment (non-continuation versions).
fn seg_hist(b: &DecodedBlock, si: usize) -> Arc<Hist> {
    let s = &b.segs[si];
    let versions: Vec<Ver> = match &s.body {
        SegmentBody::Node(vs) => vs.iter().filter(|v| !v.meta.continuation).map(ver_node).collect(),
        SegmentBody::Way { slots, versions } => versions.iter().filter(|v| !v.meta.continuation).map(|v| ver_way(v, slots)).collect(),
        SegmentBody::Relation(vs) => {
            let ms = b.members_all(si).unwrap_or_default();
            vs.iter()
                .enumerate()
                .filter(|(_, v)| !v.meta.continuation)
                .map(|(i, v)| ver_rel(v, ms.get(i).cloned().unwrap_or_default()))
                .collect()
        }
    };
    Arc::new(Hist { otype: s.obj_type(), id: s.id, versions })
}

fn priv_hist(id: i64, es: &[PrivEntry]) -> Arc<Hist> {
    Arc::new(Hist { otype: ObjType::Node, id, versions: es.iter().filter(|e| !e.continuation).map(ver_priv).collect() })
}

/// Scans one block. Elements of objects split over several segments go to `deferred`.
pub fn scan_block(db: &Db, spec: &ScanSpec, no: u32, sink: &mut dyn FnMut(Item) -> R<()>, deferred: &mut Vec<Deferred>) -> R<()> {
    let r = &db.reader;
    let entry = r.block_entry(no).map_err(rt_err)?;
    let ot = entry.obj_type;
    let private = ot == ObjType::Way && want_private(spec);
    let main = spec.kinds.visits(ot);
    if !main && !private {
        return Ok(());
    }
    let header = r.block_header(no).map_err(rt_err)?;
    let cover = match &spec.region {
        Some(reg) => reg.cover(&BBoxF::from_e7(&header.bbox)),
        None => Cover::Inside,
    };
    if cover == Cover::Outside {
        return Ok(());
    }
    let history = matches!(spec.time, TimeMode::History);
    let needs_nodes = ot == ObjType::Way && (spec.kinds.area || spec.kinds.any);
    let attr = !spec.need_geom && cover == Cover::Inside && !private && !needs_nodes && !history && ot != ObjType::Relation;
    let pred = &spec.pred;
    let tags = |t: &Tags| pred.eval(t);
    let time = spec.time.clone();
    let keep = move |d: &block::DirEntry, v: &[Meta], ok: &[bool]| -> bool {
        if attr && d.flags & SEG_MULTI != 0 && !matches!(time, TimeMode::Events(..)) {
            return true;
        }
        match &time {
            TimeMode::State(t) => {
                v.iter().zip(ok).rev().find(|(m, _)| m.timestamp <= *t).is_some_and(|(m, ok)| m.visible && *ok)
            }
            TimeMode::Events(a, b) => v.iter().zip(ok).any(|(m, ok)| *ok && !m.continuation && m.timestamp >= *a && m.timestamp < *b),
            _ => ok.iter().any(|x| *x),
        }
    };
    let filter = block::SegFilter { tags: &tags, keep: &keep };
    let selective = !pred.is_true();
    let b = if attr {
        r.block_attrs(no, &filter)
    } else if selective && !private {
        r.block_filtered(no, &filter)
    } else {
        r.block(no)
    }
    .map_err(rt_err)?;

    let (r0, r1) = time_range(db, &spec.time);
    let mut cands: Vec<Cand> = Vec::new();
    for (si, s) in b.segs.iter().enumerate() {
        let d = &b.dirs[si];
        let multi = s.flags & SEG_MULTI != 0;
        if !b.attr_only {
            let ok = match &spec.time {
                TimeMode::State(_) => d.valid_at(r0),
                _ => d.overlaps(r0, r1),
            };
            if !ok {
                continue;
            }
        }
        let seg_box = BBoxF::from_e7(&d.bbox);
        let seg_cover = match (&spec.region, cover) {
            (_, Cover::Inside) => Cover::Inside,
            (Some(reg), _) if !b.attr_only && s.flags & SEG_NO_BBOX == 0 => reg.cover(&seg_box),
            _ => Cover::Partial,
        };
        if seg_cover == Cover::Outside {
            continue;
        }
        // private nodes inside way segments
        if private && let SegmentBody::Way { slots, .. } = &s.body {
            for sl in slots {
                let Some(es) = &sl.private else { continue };
                if history {
                    let h = priv_hist(sl.node_id, es);
                    if !h.versions.is_empty() {
                        let it = Item::Hist(h);
                        if multi { deferred.push(Deferred { item: it, live: true }) } else { sink(it)? }
                    }
                    continue;
                }
                let picks: Vec<(&PrivEntry, i64)> = match &spec.time {
                    TimeMode::State(t) => entry_at(es, *t).filter(|e| e.visible).map(|e| vec![(e, *t)]).unwrap_or_default(),
                    TimeMode::Events(a, bb) => {
                        es.iter().filter(|e| !e.continuation && e.timestamp >= *a && e.timestamp < *bb).map(|e| (e, e.timestamp)).collect()
                    }
                    TimeMode::Series(st) => st.iter().filter_map(|&t| entry_at(es, t).filter(|e| e.visible).map(|e| (e, t))).collect(),
                    TimeMode::History => Vec::new(),
                };
                for (e, at) in picks {
                    if e.visible
                        && let Some(reg) = &spec.region
                        && !reg.contains(geom::e7((e.lon, e.lat)))
                    {
                        continue;
                    }
                    let el = Elem::new(EKind::Node, Some(ObjType::Node), sl.node_id, ver_priv(e), at);
                    if e.visible {
                        let _ = el.geom.set(Arc::new(Geom::Point(geom::e7((e.lon, e.lat)))));
                    }
                    if spec.need_hist && !multi {
                        let _ = el.hist.set(priv_hist(sl.node_id, es));
                    }
                    let it = Item::Elem(Arc::new(el));
                    if multi { deferred.push(Deferred { item: it, live: true }) } else { sink(it)? }
                }
            }
        }
        if !main {
            continue;
        }
        if history {
            let h = seg_hist(&b, si);
            let any_ok = h.versions.iter().any(|v| pred.eval(&v.tags));
            if any_ok {
                let it = Item::Hist(h);
                if multi { deferred.push(Deferred { item: it, live: true }) } else { sink(it)? }
            }
            continue;
        }
        let ms = metas(s);
        let stand_in = b.attr_only && multi;
        let cs = candidates(spec, &ms, stand_in);
        if cs.is_empty() {
            continue;
        }
        let hist = (spec.need_hist && !multi).then(|| seg_hist(&b, si));
        let rel_members = if s.obj_type() == ObjType::Relation { Some(b.members_all(si).map_err(rt_err)?) } else { None };
        for (vi, at) in cs {
            let (ver, local) = match &s.body {
                SegmentBody::Node(vs) => {
                    let mut v = ver_node(&vs[vi]);
                    if b.attr_only {
                        // positions were not decoded
                        v.pos = None;
                    }
                    (v, None)
                }
                SegmentBody::Way { slots, versions } => {
                    let v = &versions[vi];
                    let local: Vec<Option<(i32, i32)>> = v
                        .nodes
                        .iter()
                        .map(|&i| {
                            let sl = &slots[i as usize];
                            sl.private.as_ref().and_then(|es| entry_at(es, at)).and_then(|e| e.visible.then_some((e.lon, e.lat)))
                        })
                        .collect();
                    (ver_way(v, slots), Some(local))
                }
                SegmentBody::Relation(vs) => {
                    let m = rel_members.as_ref().and_then(|m| m.get(vi).cloned()).unwrap_or_default();
                    (ver_rel(&vs[vi], m), None)
                }
            };
            // states only contain visible versions; events also deletions
            let alive = ver.visible || matches!(spec.time, TimeMode::Events(..));
            if !alive || !pred.eval(&ver.tags) {
                if stand_in {
                    let el = Elem::new(EKind::Node, Some(s.obj_type()), s.id, ver, at);
                    deferred.push(Deferred { item: Item::Elem(Arc::new(el)), live: false });
                }
                continue;
            }
            for kind in classify(&spec.kinds, s.obj_type(), &ver) {
                let mut el = Elem::new(kind, Some(s.obj_type()), s.id, ver.clone(), at);
                el.bbox = Some(seg_box);
                if let Some(h) = &hist {
                    let _ = el.hist.set(h.clone());
                }
                if let Some(p) = ver.pos {
                    let _ = el.geom.set(Arc::new(Geom::Point(geom::e7(p))));
                }
                cands.push(Cand { elem: el, local: local.clone(), partial: seg_cover == Cover::Partial, multi });
            }
        }
    }
    // shared node positions for ways that need geometry
    let mut pending: Vec<(i64, i64)> = Vec::new();
    for c in &cands {
        if let Some(l) = &c.local
            && (spec.need_geom || c.partial)
        {
            for (n, p) in c.elem.v.nodes.iter().zip(l) {
                if p.is_none() {
                    pending.push((*n, c.elem.at));
                }
            }
        }
    }
    let resolved = if pending.is_empty() {
        HashMap::default()
    } else if let TimeMode::State(t) = spec.time {
        let ids: Vec<i64> = pending.iter().map(|p| p.0).collect();
        db.reader
            .node_positions(&ids, t)
            .map_err(rt_err)?
            .into_iter()
            .map(|(id, p)| ((id, t), p))
            .collect::<HashMap<(i64, i64), Option<(i32, i32)>>>()
    } else {
        db.positions(&pending)
    };
    for c in cands {
        let needs = spec.need_geom || c.partial;
        if needs {
            if let Some(l) = &c.local {
                let pts: Vec<P> = c
                    .elem
                    .v
                    .nodes
                    .iter()
                    .zip(l)
                    .filter_map(|(n, p)| p.or_else(|| resolved.get(&(*n, c.elem.at)).copied().flatten()).map(geom::e7))
                    .collect();
                let g = if c.elem.kind == EKind::Area && pts.len() >= 4 {
                    let mut ring = pts;
                    if ring.first() != ring.last() {
                        ring.push(ring[0]);
                    }
                    Geom::Poly(vec![Polygon { outer: ring, holes: vec![] }])
                } else {
                    Geom::Line(pts)
                };
                let _ = c.elem.geom.set(Arc::new(g));
            } else if c.elem.otype == Some(ObjType::Relation) && spec.need_geom {
                let g = if c.elem.kind == EKind::Area {
                    db.area_geom(&c.elem.v.members, c.elem.at)
                } else {
                    db.relation_geom(&c.elem.v.members, c.elem.at)
                };
                let _ = c.elem.geom.set(Arc::new(g));
            }
        }
        if c.partial
            && let Some(reg) = &spec.region
        {
            let p = match c.elem.geom.get() {
                Some(g) if !g.is_empty() => g.rep_point(),
                _ => c.elem.bbox.map(|b| b.center()),
            };
            match p {
                Some(p) if reg.contains(p) => {}
                _ => continue,
            }
        }
        let it = Item::Elem(Arc::new(c.elem));
        if c.multi { deferred.push(Deferred { item: it, live: true }) } else { sink(it)? }
    }
    Ok(())
}

/// De-duplicates elements of objects that span several segments and emits them.
pub fn finish_deferred(spec: &ScanSpec, deferred: Vec<Deferred>, sink: &mut dyn FnMut(Item) -> R<()>) -> R<()> {
    let mut elems: HashMap<(u8, Option<ObjType>, i64, i64, u32), Arc<Elem>> = HashMap::default();
    let mut hists: HashMap<(ObjType, i64), Vec<Ver>> = HashMap::default();
    let events = matches!(spec.time, TimeMode::Events(..));
    // newest version per object and moment over all segments, stand-ins included
    let mut newest: HashMap<(Option<ObjType>, i64, i64), u32> = HashMap::default();
    if !events {
        for d in &deferred {
            if let Item::Elem(e) = &d.item {
                let n = newest.entry((e.otype, e.id, e.at)).or_insert(0);
                *n = (*n).max(e.v.version);
            }
        }
    }
    for d in deferred {
        if !d.live {
            continue;
        }
        match d.item {
            Item::Elem(e) => {
                if !events && newest.get(&(e.otype, e.id, e.at)).is_some_and(|&n| n > e.v.version) {
                    continue;
                }
                // state/series: one per object and moment; events: one per version
                let key = (e.kind as u8, e.otype, e.id, e.at, if events { e.v.version } else { 0 });
                match elems.get(&key) {
                    Some(old) if old.v.version >= e.v.version => {}
                    _ => {
                        elems.insert(key, e);
                    }
                }
            }
            Item::Hist(h) => hists.entry((h.otype, h.id)).or_default().extend(h.versions.iter().cloned()),
        }
    }
    let mut es: Vec<Arc<Elem>> = elems.into_values().collect();
    es.sort_by_key(|e| (e.order(), e.at));
    for e in es {
        sink(Item::Elem(e))?;
    }
    let mut hs: Vec<((ObjType, i64), Vec<Ver>)> = hists.into_iter().collect();
    hs.sort_by_key(|h| h.0);
    for ((t, id), mut vs) in hs {
        vs.sort_by_key(|v| (v.version, v.ts));
        vs.dedup_by_key(|v| v.version);
        sink(Item::Hist(Arc::new(Hist { otype: t, id, versions: vs })))?;
    }
    Ok(())
}

/// Parallel fold over all matching items. Returns one accumulator per block (in block
/// order) plus one for de-duplicated multi-segment objects.
pub fn fold<A: Send>(db: &Db, spec: &ScanSpec, init: &(dyn Fn() -> A + Sync), step: &(dyn Fn(&mut A, Item) -> R<()> + Sync)) -> R<Vec<A>> {
    let bl = blocks(db, spec)?;
    let parts: Vec<(A, Vec<Deferred>, Option<RtErr>)> = bl
        .par_iter()
        .map(|&no| {
            let mut acc = init();
            let mut def = Vec::new();
            let r = scan_block(db, spec, no, &mut |it| step(&mut acc, it), &mut def);
            (acc, def, r.err())
        })
        .collect();
    let mut accs = Vec::with_capacity(parts.len() + 1);
    let mut defs = Vec::new();
    let mut errs = Vec::new();
    for (a, d, e) in parts {
        accs.push(a);
        defs.extend(d);
        if let Some(e) = e {
            errs.push(e);
        }
    }
    if let Some(e) = first_error(errs) {
        return Err(e);
    }
    let mut last = init();
    finish_deferred(spec, defs, &mut |it| step(&mut last, it))?;
    accs.push(last);
    Ok(accs)
}
