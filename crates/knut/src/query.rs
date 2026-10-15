//! Query engine: get / at / snapshot / changes / way geometry (SPEC.md §11).

use crate::file::{DecodedBlock, Reader};
use crate::{Result, corrupt};
use knut_format::block::SegDir;
use knut_format::bloom::{self, Bloom};
use knut_format::layout::{RNode, TREE_LARGE};
use knut_format::*;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Geometry of a feature at a point in time.
#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    None,
    Point(i32, i32),
    /// One entry per node reference; `None` if the node position is unknown.
    Line(Vec<Option<(i32, i32)>>),
}

/// One object version (history record or snapshot state).
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    pub obj_type: ObjType,
    pub id: i64,
    pub meta: Meta,
    pub tags: Tags,
    /// Way node ids.
    pub nodes: Vec<i64>,
    /// Relation members.
    pub members: Vec<Member>,
    pub geometry: Geometry,
}

impl Feature {
    pub fn tag(&self, k: &str) -> Option<&str> {
        self.tags.iter().find(|(kk, _)| kk.as_ref() == k).map(|(_, v)| v.as_ref())
    }
}

/// Attribute filter for spatial queries.
#[derive(Clone, Debug)]
pub struct Filter {
    pub types: [bool; 3],
    pub key: Option<String>,
    pub value: Option<String>,
    pub uid: Option<u32>,
    pub changeset: Option<u64>,
    /// Include private (untagged, single-way) nodes when nodes are requested and no tag
    /// filter is set.
    pub include_private_nodes: bool,
    /// Return geometry (node positions, way coordinates and node lists). `false` allows
    /// attribute-only decoding of blocks that lie completely inside the query box.
    pub geometry: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            types: [true; 3],
            key: None,
            value: None,
            uid: None,
            changeset: None,
            include_private_nodes: true,
            geometry: true,
        }
    }
}

impl Filter {
    pub fn types(mut self, t: &[ObjType]) -> Self {
        self.types = [false; 3];
        for x in t {
            self.types[*x as usize] = true;
        }
        self
    }

    pub fn tag(mut self, key: &str, value: Option<&str>) -> Self {
        self.key = Some(key.to_string());
        self.value = value.map(|v| v.to_string());
        self
    }
}

struct Compiled<'a> {
    f: &'a Filter,
    /// `None` if the key is not in the file at all (no results possible).
    key_hash: Option<Option<u64>>,
    kv_hash: Option<u64>,
}

impl<'a> Compiled<'a> {
    fn new(r: &Reader, f: &'a Filter) -> Compiled<'a> {
        let mut key_hash = None;
        let mut kv_hash = None;
        if let Some(k) = &f.key {
            match r.key_id(k) {
                Some(kid) => {
                    key_hash = Some(Some(bloom::hash_key(kid)));
                    if let Some(vid) = f.value.as_deref().and_then(|v| r.value_id(v)) {
                        kv_hash = Some(bloom::hash_kv(kid, vid));
                    }
                }
                None => key_hash = Some(None),
            }
        }
        Compiled { f, key_hash, kv_hash }
    }

    fn impossible(&self) -> bool {
        matches!(self.key_hash, Some(None))
    }

    fn bloom_ok(&self, b: &Bloom) -> bool {
        if let Some(Some(h)) = self.key_hash
            && !bloom::test(b, h) {
                return false;
            }
        if let Some(h) = self.kv_hash
            && !bloom::test(b, h) {
                return false;
            }
        true
    }

    /// The hashes every matching block's sized filter must contain.
    fn hashes(&self) -> Vec<u64> {
        let mut v = Vec::new();
        if let Some(Some(h)) = self.key_hash {
            v.push(h);
        }
        v.extend(self.kv_hash);
        v
    }

    fn has_tag_filter(&self) -> bool {
        self.f.key.is_some()
    }

    fn meta_ok(&self, m: &Meta, tags: &Tags) -> bool {
        if let Some(u) = self.f.uid
            && m.uid != u {
                return false;
            }
        if let Some(c) = self.f.changeset
            && m.changeset != c {
                return false;
            }
        if let Some(k) = &self.f.key {
            match tags.iter().find(|(kk, _)| kk.as_ref() == k.as_str()) {
                None => return false,
                Some((_, v)) => {
                    if let Some(want) = &self.f.value
                        && v.as_ref() != want.as_str() {
                            return false;
                        }
                }
            }
        }
        true
    }

    /// True if attribute filters exist that allow skipping segments while decoding.
    fn selective(&self) -> bool {
        self.f.key.is_some() || self.f.uid.is_some() || self.f.changeset.is_some()
    }

    fn tags_ok(&self, tags: &Tags) -> bool {
        let Some(k) = &self.f.key else { return true };
        match tags.iter().find(|(kk, _)| kk.as_ref() == k.as_str()) {
            None => false,
            Some((_, v)) => self.f.value.as_ref().is_none_or(|w| v.as_ref() == w.as_str()),
        }
    }

    fn attr_ok(&self, m: &Meta, tag_ok: bool) -> bool {
        tag_ok && self.f.uid.is_none_or(|u| m.uid == u) && self.f.changeset.is_none_or(|c| m.changeset == c)
    }

    fn private_ok(&self, e: &PrivEntry) -> bool {
        self.f.include_private_nodes
            && self.f.types[0]
            && !self.has_tag_filter()
            && self.f.uid.is_none_or(|u| u == e.uid)
            && self.f.changeset.is_none_or(|c| c == e.changeset)
    }
}

fn priv_feature(node_id: i64, e: &PrivEntry) -> Feature {
    Feature {
        obj_type: ObjType::Node,
        id: node_id,
        meta: Meta {
            version: e.version,
            timestamp: e.timestamp,
            changeset: e.changeset,
            uid: e.uid,
            visible: e.visible,
            continuation: false,
        },
        tags: Tags::from(Vec::new()),
        nodes: Vec::new(),
        members: Vec::new(),
        geometry: if e.visible { Geometry::Point(e.lon, e.lat) } else { Geometry::None },
    }
}

fn version_feature(b: &DecodedBlock, si: usize, vi: usize) -> Result<Feature> {
    let mut f = version_feature_raw(&b.segs[si], vi);
    if b.attr_only {
        f.geometry = Geometry::None;
    }
    if f.obj_type == ObjType::Relation {
        f.members = b.members(si, vi)?;
    }
    Ok(f)
}

/// All non-continuation versions of segment `si`.
fn segment_versions(b: &DecodedBlock, si: usize) -> Result<Vec<Feature>> {
    let s = &b.segs[si];
    let members = if s.obj_type() == ObjType::Relation { b.members_all(si)? } else { Vec::new() };
    let mut out = Vec::new();
    for (vi, m) in versions_meta(s).iter().enumerate() {
        if !m.continuation {
            let mut f = version_feature_raw(s, vi);
            if let Some(ms) = members.get(vi) {
                f.members = ms.clone();
            }
            out.push(f);
        }
    }
    Ok(out)
}

fn version_feature_raw(s: &Segment, vi: usize) -> Feature {
    match &s.body {
        SegmentBody::Node(vs) => {
            let v = &vs[vi];
            Feature {
                obj_type: ObjType::Node,
                id: s.id,
                meta: v.meta.clone(),
                tags: v.tags.clone(),
                nodes: Vec::new(),
                members: Vec::new(),
                geometry: if v.meta.visible { Geometry::Point(v.lon, v.lat) } else { Geometry::None },
            }
        }
        SegmentBody::Way { slots, versions } => {
            let v = &versions[vi];
            Feature {
                obj_type: ObjType::Way,
                id: s.id,
                meta: v.meta.clone(),
                tags: v.tags.clone(),
                nodes: v.nodes.iter().map(|&i| slots[i as usize].node_id).collect(),
                members: Vec::new(),
                geometry: Geometry::None,
            }
        }
        SegmentBody::Relation(vs) => {
            let v = &vs[vi];
            Feature {
                obj_type: ObjType::Relation,
                id: s.id,
                meta: v.meta.clone(),
                tags: v.tags.clone(),
                nodes: Vec::new(),
                members: Vec::new(),
                geometry: Geometry::None,
            }
        }
    }
}

fn versions_meta(s: &Segment) -> Vec<&Meta> {
    match &s.body {
        SegmentBody::Node(vs) => vs.iter().map(|v| &v.meta).collect(),
        SegmentBody::Way { versions, .. } => versions.iter().map(|v| &v.meta).collect(),
        SegmentBody::Relation(vs) => vs.iter().map(|v| &v.meta).collect(),
    }
}

/// Index of the last version with timestamp ≤ t (including continuation records).
fn version_at(s: &Segment, t: i64) -> Option<usize> {
    let ms = versions_meta(s);
    let mut best = None;
    for (i, m) in ms.iter().enumerate() {
        if m.timestamp <= t {
            best = Some(i);
        }
    }
    best
}

/// Latest private entry with timestamp ≤ t.
fn entry_at(es: &[PrivEntry], t: i64) -> Option<&PrivEntry> {
    es.iter().rev().find(|e| e.timestamp <= t)
}

/// Segment-box/line intersection test.
pub fn line_intersects(coords: &[(i32, i32)], b: &BBox) -> bool {
    if coords.iter().any(|&(x, y)| b.contains(x, y)) {
        return true;
    }
    coords.windows(2).any(|w| seg_intersects_box(w[0], w[1], b))
}

fn seg_intersects_box(a: (i32, i32), c: (i32, i32), b: &BBox) -> bool {
    // Liang–Barsky clipping in f64.
    let (x0, y0, x1, y1) = (a.0 as f64, a.1 as f64, c.0 as f64, c.1 as f64);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    for (p, q) in [
        (-dx, x0 - b.minx as f64),
        (dx, b.maxx as f64 - x0),
        (-dy, y0 - b.miny as f64),
        (dy, b.maxy as f64 - y0),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

/// True if `outer` fully contains the non-empty box `inner`.
fn contains(outer: &BBox, inner: &BBox) -> bool {
    !inner.is_empty()
        && outer.minx <= inner.minx
        && outer.miny <= inner.miny
        && outer.maxx >= inner.maxx
        && outer.maxy >= inner.maxy
}

/// Splits a box crossing the antimeridian (`minx > maxx`) into two.
fn split_bbox(b: &BBox) -> Vec<BBox> {
    if b.minx > b.maxx {
        vec![
            BBox::new(b.minx, b.miny, BBox::WORLD.maxx, b.maxy),
            BBox::new(BBox::WORLD.minx, b.miny, b.maxx, b.maxy),
        ]
    } else {
        vec![*b]
    }
}

impl Reader {
    fn rel(&self, t: i64) -> u32 {
        rel_time(t, self.time_base())
    }

    /// All versions of an object, in version order.
    pub fn get(&self, t: ObjType, id: i64) -> Result<Vec<Feature>> {
        let mut out = Vec::new();
        for no in self.lookup(t, id)? {
            let b = self.block(no)?;
            if b.obj_type == t {
                for &si in b.segments_of(id) {
                    out.extend(segment_versions(&b, si as usize)?);
                }
            } else if t == ObjType::Node && b.obj_type == ObjType::Way {
                for &(si, k) in b.private_slots(id) {
                    if let SegmentBody::Way { slots, .. } = &b.segs[si as usize].body {
                        for e in slots[k as usize].private.as_deref().unwrap_or(&[]) {
                            if !e.continuation {
                                out.push(priv_feature(id, e));
                            }
                        }
                    }
                }
            }
        }
        out.sort_by_key(|f| (f.meta.version, f.meta.timestamp));
        out.dedup_by(|a, b| a.meta == b.meta && a.id == b.id);
        Ok(out)
    }

    /// The version valid at time `ts` (may be a deleted version).
    pub fn at(&self, t: ObjType, id: i64, ts: i64) -> Result<Option<Feature>> {
        let vs = self.get(t, id)?;
        let mut best = None;
        for v in vs {
            if v.meta.timestamp <= ts {
                best = Some(v);
            }
        }
        if let Some(f) = &mut best
            && t == ObjType::Way && f.meta.visible
                && let Some(g) = self.way_geometry_at(id, ts)? {
                    f.geometry = Geometry::Line(g.into_iter().map(|(_, p)| p).collect());
                }
        Ok(best)
    }

    /// Positions of shared nodes at time `ts`, resolved via the ID index.
    pub fn node_positions(&self, ids: &[i64], ts: i64) -> Result<HashMap<i64, Option<(i32, i32)>>> {
        let rt = self.rel(ts);
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        // id -> candidate blocks, grouped by block so every block is decoded once
        let blocks: Vec<Vec<u32>> = sorted
            .par_chunks(4096)
            .map(|c| self.lookup_sorted(ObjType::Node, c))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut by_block: HashMap<u32, Vec<i64>> = HashMap::new();
        for (&id, nos) in sorted.iter().zip(&blocks) {
            for &no in nos {
                by_block.entry(no).or_default().push(id);
            }
        }
        let by_block: Vec<(u32, Vec<i64>)> = by_block.into_iter().collect();
        // (id, position) for every block in which the node is valid at `ts`
        let found: Vec<Vec<(i64, Option<(i32, i32)>)>> = by_block
            .par_iter()
            .map(|(no, want)| -> Result<Vec<(i64, Option<(i32, i32)>)>> {
                let mut out = Vec::new();
                if self.block_entry(*no)?.obj_type != ObjType::Node {
                    let b = self.block(*no)?;
                    for &id in want {
                        if let Some(p) = private_pos(&b, id, ts) {
                            out.push((id, p));
                        }
                    }
                    return Ok(out);
                }
                let set: std::collections::HashSet<i64> = want.iter().copied().collect();
                let tags = |_: &Tags| true;
                let keep = |d: &knut_format::block::DirEntry, _: &[Meta], _: &[bool]| set.contains(&d.id);
                let b = self.block_filtered(*no, &knut_format::block::SegFilter { tags: &tags, keep: &keep })?;
                for &id in want {
                    for &si in b.segments_of(id) {
                        let s = &b.segs[si as usize];
                        if b.dirs[si as usize].valid_at(rt) {
                            if let (Some(vi), SegmentBody::Node(vs)) = (version_at(s, ts), &s.body) {
                                let v = &vs[vi];
                                out.push((id, v.meta.visible.then_some((v.lon, v.lat))));
                            }
                        }
                    }
                }
                Ok(out)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut m: HashMap<i64, Option<(i32, i32)>> = sorted.iter().map(|&id| (id, None)).collect();
        for part in found {
            for (id, p) in part {
                if p.is_some() || !m.contains_key(&id) {
                    m.insert(id, p);
                }
            }
        }
        Ok(m)
    }

    /// Node ids and positions of a way at time `ts`; `None` if the way does not exist or
    /// is deleted at `ts`.
    pub fn way_geometry_at(&self, id: i64, ts: i64) -> Result<Option<Vec<(i64, Option<(i32, i32)>)>>> {
        let rt = self.rel(ts);
        for no in self.lookup(ObjType::Way, id)? {
            let b = self.block(no)?;
            for &si in b.segments_of(id) {
                if !b.dirs[si as usize].valid_at(rt) {
                    continue;
                }
                let s = &b.segs[si as usize];
                let (pending, geom) = way_geom_local(s, ts);
                let Some(mut geom) = geom else { return Ok(None) };
                let resolved = self.node_positions(&pending, ts)?;
                for (nid, p) in geom.iter_mut() {
                    if p.is_none()
                        && let Some(Some(q)) = resolved.get(nid) {
                            *p = Some(*q);
                        }
                }
                return Ok(Some(geom));
            }
        }
        Ok(None)
    }

    /// All distinct geometries of a way over time: `(time, nodes with positions)`.
    pub fn way_geometry_changes(&self, id: i64) -> Result<Vec<(i64, Vec<(i64, Option<(i32, i32)>)>)>> {
        let versions = self.get(ObjType::Way, id)?;
        let Some(first) = versions.first() else { return Ok(Vec::new()) };
        let mut times: Vec<i64> = versions.iter().map(|v| v.meta.timestamp).collect();
        let mut nodes: HashSet<i64> = HashSet::new();
        for v in &versions {
            nodes.extend(v.nodes.iter().copied());
        }
        let mut nodes: Vec<i64> = nodes.into_iter().collect();
        nodes.sort_unstable();
        for n in nodes {
            times.extend(self.get(ObjType::Node, n)?.iter().map(|v| v.meta.timestamp));
        }
        let t0 = first.meta.timestamp;
        times.retain(|&t| t >= t0);
        times.sort_unstable();
        times.dedup();
        let mut out: Vec<(i64, Vec<(i64, Option<(i32, i32)>)>)> = Vec::new();
        let mut last: Option<Option<Vec<(i64, Option<(i32, i32)>)>>> = None;
        for t in times {
            let g = self.way_geometry_at(id, t)?;
            if last.as_ref() != Some(&g) {
                if let Some(gg) = &g {
                    out.push((t, gg.clone()));
                } else {
                    out.push((t, Vec::new()));
                }
                last = Some(g);
            }
        }
        Ok(out)
    }

    fn blocks_for(&self, bbox: &BBox, t0: u32, t1: u32, c: &Compiled, tree_types: [bool; 3]) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let pred = |n: &RNode, b: &Bloom| -> bool {
            n.bbox.intersects(bbox) && n.tmin <= t1 && t0 <= n.tmax && c.bloom_ok(b)
        };
        let pred_nobloom =
            |n: &RNode, _: &Bloom| -> bool { n.bbox.intersects(bbox) && n.tmin <= t1 && t0 <= n.tmax };
        for t in ObjType::ALL {
            if tree_types[t as usize] {
                out.extend(self.query_tree(t as usize, &pred)?);
            }
        }
        // Large objects of all types share one tree; type filtering happens per block.
        out.extend(self.query_tree(TREE_LARGE, &pred)?.into_iter().filter(|&no| {
            self.block_entry(no).map(|e| tree_types[e.obj_type as usize]).unwrap_or(false)
        }));
        let hashes = c.hashes();
        if !hashes.is_empty() {
            out.retain(|&no| self.block_may_contain(no, &hashes));
        }
        // Private nodes live in way blocks without tags: bloom must not prune them.
        if tree_types[0] && c.f.include_private_nodes && !c.has_tag_filter() && !tree_types[1] {
            out.extend(self.query_tree(ObjType::Way as usize, &pred_nobloom)?);
            out.extend(self.query_tree(TREE_LARGE, &pred_nobloom)?.into_iter().filter(|&no| {
                self.block_entry(no).map(|e| e.obj_type == ObjType::Way).unwrap_or(false)
            }));
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// State of all matching objects within `bbox` at time `ts`.
    pub fn snapshot(&self, bbox: &BBox, ts: i64, filter: &Filter) -> Result<Vec<Feature>> {
        let mut all = Vec::new();
        for b in split_bbox(bbox) {
            all.extend(self.snapshot_one(&b, ts, filter)?);
        }
        all.sort_by_key(|f| (f.obj_type, f.id, std::cmp::Reverse(f.meta.version)));
        all.dedup_by_key(|f| (f.obj_type, f.id));
        Ok(all)
    }

    fn snapshot_one(&self, bbox: &BBox, ts: i64, filter: &Filter) -> Result<Vec<Feature>> {
        let c = Compiled::new(self, filter);
        if c.impossible() && !(filter.types[0] && filter.include_private_nodes && filter.key.is_none()) {
            return Ok(Vec::new());
        }
        let rt = self.rel(ts);
        let want_private = filter.types[0] && filter.include_private_nodes && !c.has_tag_filter();
        let mut tree_types = filter.types;
        if want_private {
            tree_types[1] = true;
        }
        let blocks = self.blocks_for(bbox, rt, rt, &c, tree_types)?;

        struct Partial {
            features: Vec<Feature>,
            pending: Vec<i64>,
            /// attribute-only candidates: (feature, matches); resolved per object below
            attr: Vec<(Feature, bool)>,
        }
        let parts: Vec<Result<Partial>> = blocks
            .par_iter()
            .map(|&no| {
                let mut p = Partial { features: Vec::new(), pending: Vec::new(), attr: Vec::new() };
                if !filter.geometry && !want_private && contains(bbox, &self.block_header(no)?.bbox) {
                    // Block lies inside the query box: no spatial test, no geometry needed.
                    // Segment time intervals are not exact without geometry, so the newest
                    // version <= ts over all segments of an object wins (multi-segment
                    // objects also contribute non-matching candidates).
                    let tags = |t: &Tags| c.tags_ok(t);
                    let keep = |d: &knut_format::block::DirEntry, v: &[Meta], ok: &[bool]| {
                        v.iter().zip(ok).rev().find(|(m, _)| m.timestamp <= ts).is_some_and(|(m, ok)| {
                            (m.visible && c.attr_ok(m, *ok)) || d.flags & SEG_MULTI != 0
                        })
                    };
                    let b = self.block_attrs(no, &knut_format::block::SegFilter { tags: &tags, keep: &keep })?;
                    for (si, s) in b.segs.iter().enumerate() {
                        if !filter.types[s.obj_type() as usize] {
                            continue;
                        }
                        let Some(vi) = version_at(s, ts) else { continue };
                        let mut f = version_feature(&b, si, vi)?;
                        f.geometry = Geometry::None;
                        let ok = f.meta.visible && c.meta_ok(&f.meta, &f.tags);
                        if ok || s.flags & SEG_MULTI != 0 {
                            p.attr.push((f, ok));
                        }
                    }
                    return Ok(p);
                }
                let hit = |d: &SegDir| d.valid_at(rt) && (d.bbox.intersects(bbox));
                let b = if c.selective() && !want_private {
                    let tags = |t: &Tags| c.tags_ok(t);
                    // keep segments whose version at `ts` is visible and matches
                    let keep = |_: &knut_format::block::DirEntry, v: &[Meta], ok: &[bool]| {
                        v.iter().zip(ok).rev().find(|(m, _)| m.timestamp <= ts).is_some_and(|(m, ok)| m.visible && c.attr_ok(m, *ok))
                    };
                    self.block_filtered(no, &knut_format::block::SegFilter { tags: &tags, keep: &keep })?
                } else {
                    self.block(no)?
                };
                if !b.dirs.iter().any(hit) {
                    return Ok(p);
                }
                for (si, s) in b.segs.iter().enumerate() {
                    if !hit(&b.dirs[si]) {
                        continue;
                    }
                    match &s.body {
                        SegmentBody::Node(_) if filter.types[0] => {
                            let Some(vi) = version_at(s, ts) else { continue };
                            let f = version_feature(&b, si, vi)?;
                            if f.meta.visible && c.meta_ok(&f.meta, &f.tags)
                                && let Geometry::Point(x, y) = f.geometry
                                    && bbox.contains(x, y) {
                                        p.features.push(f);
                                    }
                        }
                        SegmentBody::Way { slots, .. } => {
                            if want_private {
                                for sl in slots {
                                    if let Some(es) = &sl.private
                                        && let Some(e) = entry_at(es, ts)
                                            && e.visible && bbox.contains(e.lon, e.lat) && c.private_ok(e) {
                                                p.features.push(priv_feature(sl.node_id, e));
                                            }
                                }
                            }
                            if !filter.types[1] {
                                continue;
                            }
                            let Some(vi) = version_at(s, ts) else { continue };
                            let mut f = version_feature(&b, si, vi)?;
                            if !f.meta.visible || !c.meta_ok(&f.meta, &f.tags) {
                                continue;
                            }
                            let (pending, geom) = way_geom_local(s, ts);
                            p.pending.extend(pending);
                            f.geometry =
                                Geometry::Line(geom.unwrap_or_default().into_iter().map(|(_, p)| p).collect());
                            p.features.push(f);
                        }
                        SegmentBody::Relation(vs) if filter.types[2] => {
                            let Some(vi) = version_at(s, ts) else { continue };
                            let v = &vs[vi];
                            if v.meta.visible && c.meta_ok(&v.meta, &v.tags) {
                                p.features.push(version_feature(&b, si, vi)?);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(p)
            })
            .collect();
        let mut features = Vec::new();
        let mut pending = Vec::new();
        let mut attr = Vec::new();
        for p in parts {
            let p = p?;
            features.extend(p.features);
            pending.extend(p.pending);
            attr.extend(p.attr);
        }
        attr.sort_by(|a, b| {
            (a.0.obj_type, a.0.id, std::cmp::Reverse(a.0.meta.version))
                .cmp(&(b.0.obj_type, b.0.id, std::cmp::Reverse(b.0.meta.version)))
        });
        attr.dedup_by_key(|x| (x.0.obj_type, x.0.id));
        let attr_features: Vec<Feature> = attr.into_iter().filter(|x| x.1).map(|x| x.0).collect();
        pending.sort_unstable();
        pending.dedup();
        let resolved = self.node_positions(&pending, ts)?;
        let mut out = Vec::with_capacity(features.len());
        for mut f in features {
            if f.obj_type == ObjType::Way
                && let Geometry::Line(g) = &mut f.geometry {
                    for (k, p) in g.iter_mut().enumerate() {
                        if p.is_none()
                            && let Some(Some(q)) = resolved.get(&f.nodes[k]) {
                                *p = Some(*q);
                            }
                    }
                    let coords: Vec<(i32, i32)> = g.iter().flatten().copied().collect();
                    if !line_intersects(&coords, bbox) {
                        continue;
                    }
                }
            if !filter.geometry {
                f.geometry = Geometry::None;
            }
            out.push(f);
        }
        out.extend(attr_features);
        Ok(out)
    }

    /// All versions with `t0 <= timestamp <= t1` within `bbox`.
    pub fn changes(&self, bbox: &BBox, t0: i64, t1: i64, filter: &Filter) -> Result<Vec<Feature>> {
        let mut all = Vec::new();
        for b in split_bbox(bbox) {
            all.extend(self.changes_one(&b, t0, t1, filter)?);
        }
        all.sort_by_key(|f| (f.obj_type, f.id, f.meta.version, f.meta.timestamp));
        all.dedup_by(|a, b| a.obj_type == b.obj_type && a.id == b.id && a.meta == b.meta);
        Ok(all)
    }

    fn changes_one(&self, bbox: &BBox, t0: i64, t1: i64, filter: &Filter) -> Result<Vec<Feature>> {
        if t1 < t0 {
            return Err(corrupt("t1 < t0"));
        }
        let c = Compiled::new(self, filter);
        let want_private = filter.types[0] && filter.include_private_nodes && !c.has_tag_filter();
        if c.impossible() && !want_private {
            return Ok(Vec::new());
        }
        let (r0, r1) = (self.rel(t0), self.rel(t1));
        let mut tree_types = filter.types;
        if want_private {
            tree_types[1] = true;
        }
        let blocks = self.blocks_for(bbox, r0, r1, &c, tree_types)?;
        let parts: Vec<Result<Vec<Feature>>> = blocks
            .par_iter()
            .map(|&no| {
                let mut out = Vec::new();
                let inside = !filter.geometry && !want_private && contains(bbox, &self.block_header(no)?.bbox);
                let hit = |d: &SegDir| inside || (d.overlaps(r0, r1) && d.bbox.intersects(bbox));
                let b = if inside {
                    let tags = |t: &Tags| c.tags_ok(t);
                    let keep = |_: &knut_format::block::DirEntry, v: &[Meta], ok: &[bool]| {
                        v.iter().zip(ok).any(|(m, ok)| {
                            !m.continuation && m.timestamp >= t0 && m.timestamp <= t1 && c.attr_ok(m, *ok)
                        })
                    };
                    self.block_attrs(no, &knut_format::block::SegFilter { tags: &tags, keep: &keep })?
                } else if c.selective() && !want_private {
                    let tags = |t: &Tags| c.tags_ok(t);
                    let keep = |_: &knut_format::block::DirEntry, v: &[Meta], ok: &[bool]| {
                        v.iter().zip(ok).any(|(m, ok)| {
                            !m.continuation && m.timestamp >= t0 && m.timestamp <= t1 && c.attr_ok(m, *ok)
                        })
                    };
                    self.block_filtered(no, &knut_format::block::SegFilter { tags: &tags, keep: &keep })?
                } else {
                    self.block(no)?
                };
                if !b.dirs.iter().any(hit) {
                    return Ok(out);
                }
                for (si, s) in b.segs.iter().enumerate() {
                    if !hit(&b.dirs[si]) {
                        continue;
                    }
                    if let SegmentBody::Way { slots, .. } = &s.body
                        && want_private {
                            for sl in slots {
                                for e in sl.private.as_deref().unwrap_or(&[]) {
                                    if !e.continuation
                                        && e.timestamp >= t0
                                        && e.timestamp <= t1
                                        && (!e.visible || bbox.contains(e.lon, e.lat))
                                        && c.private_ok(e)
                                    {
                                        out.push(priv_feature(sl.node_id, e));
                                    }
                                }
                            }
                        }
                    if !filter.types[s.obj_type() as usize] {
                        continue;
                    }
                    let mut rel_members: Option<Vec<Vec<Member>>> = None;
                    for (vi, m) in versions_meta(s).iter().enumerate() {
                        if m.continuation || m.timestamp < t0 || m.timestamp > t1 {
                            continue;
                        }
                        let mut f = version_feature_raw(s, vi);
                        if !c.meta_ok(&f.meta, &f.tags) {
                            continue;
                        }
                        if s.obj_type() == ObjType::Relation {
                            if rel_members.is_none() {
                                rel_members = Some(b.members_all(si)?);
                            }
                            f.members = rel_members.as_ref().unwrap()[vi].clone();
                        }
                        if let Geometry::Point(x, y) = f.geometry
                            && !inside
                            && !bbox.contains(x, y)
                        {
                            continue;
                        }
                        if !filter.geometry {
                            f.geometry = Geometry::None;
                        }
                        out.push(f);
                    }
                }
                Ok(out)
            })
            .collect();
        let mut out = Vec::new();
        for p in parts {
            out.extend(p?);
        }
        Ok(out)
    }

    /// Iterates every stored version record (excluding continuations) of the whole file
    /// in block order. Private node versions are reported as nodes.
    pub fn for_each_version(&self, f: &(dyn Fn(&Feature) + Sync)) -> Result<()> {
        let n = self.n_blocks() as u32;
        (0..n).into_par_iter().try_for_each(|no| -> Result<()> {
            let b: Arc<DecodedBlock> = self.block(no)?;
            for (si, s) in b.segs.iter().enumerate() {
                if let SegmentBody::Way { slots, .. } = &s.body {
                    for sl in slots {
                        for e in sl.private.as_deref().unwrap_or(&[]) {
                            if !e.continuation {
                                f(&priv_feature(sl.node_id, e));
                            }
                        }
                    }
                }
                for v in segment_versions(&b, si)? {
                    f(&v);
                }
            }
            Ok(())
        })
    }
}

fn private_pos(b: &DecodedBlock, id: i64, ts: i64) -> Option<Option<(i32, i32)>> {
    for &(si, k) in b.private_slots(id) {
        if let SegmentBody::Way { slots, .. } = &b.segs[si as usize].body
            && let Some(es) = &slots[k as usize].private
                && let Some(e) = entry_at(es, ts) {
                    return Some(e.visible.then_some((e.lon, e.lat)));
                }
    }
    None
}

/// Geometry of a way segment at `ts` with private slots resolved; returns shared node ids
/// that still need resolution. `None` geometry if the way is deleted/absent at `ts`.
fn way_geom_local(s: &Segment, ts: i64) -> (Vec<i64>, Option<Vec<(i64, Option<(i32, i32)>)>>) {
    let SegmentBody::Way { slots, versions } = &s.body else { return (Vec::new(), None) };
    let Some(vi) = version_at(s, ts) else { return (Vec::new(), None) };
    let v = &versions[vi];
    if !v.meta.visible {
        return (Vec::new(), None);
    }
    let mut pending = Vec::new();
    let geom = v
        .nodes
        .iter()
        .map(|&i| {
            let sl = &slots[i as usize];
            let p = match &sl.private {
                Some(es) => entry_at(es, ts).and_then(|e| e.visible.then_some((e.lon, e.lat))),
                None => {
                    pending.push(sl.node_id);
                    None
                }
            };
            (sl.node_id, p)
        })
        .collect();
    (pending, Some(geom))
}
