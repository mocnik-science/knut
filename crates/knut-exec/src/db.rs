//! Database access on top of the knut reader: node positions at arbitrary times, way and
//! area geometries, object histories.

use crate::geom::{self, BBoxF, Geom, P};
use crate::value::{Hist, Ver};
use knut::{Geometry, OpenOptions, Reader};
use knut_format::{Member, Meta, ObjType, PrivEntry, SegmentBody, Str, Tags, block, rel_time};
use rayon::prelude::*;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub struct Db {
    pub reader: Reader,
    pub path: PathBuf,
    /// Newest timestamp in the file.
    pub latest: i64,
    pub earliest: i64,
    pub extent: BBoxF,
    pub users: bool,
    names: OnceLock<HashMap<u32, Str>>,
}

pub type Pos = Option<(i32, i32)>;

/// Index of the last version with timestamp ≤ t (including continuation records).
pub fn version_at<T>(vs: &[T], ts: impl Fn(&T) -> i64, t: i64) -> Option<usize> {
    let mut best = None;
    for (i, v) in vs.iter().enumerate() {
        if ts(v) <= t {
            best = Some(i);
        } else {
            break;
        }
    }
    best
}

pub fn entry_at(es: &[PrivEntry], t: i64) -> Option<&PrivEntry> {
    es.iter().rev().find(|e| e.timestamp <= t)
}

impl Db {
    pub fn open(path: &Path) -> Result<Db, String> {
        let reader = Reader::open_with(path, OpenOptions { verify_checksums: false, cache_blocks: 4096 }).map_err(|e| e.to_string())?;
        let h = &reader.header;
        let users = h.features & knut_format::layout::FEAT_USERS_STRIPPED == 0;
        Ok(Db {
            latest: h.tmax,
            earliest: h.tmin,
            extent: BBoxF::from_e7(&h.bbox),
            users,
            path: path.to_path_buf(),
            names: OnceLock::new(),
            reader,
        })
    }

    pub fn rel(&self, t: i64) -> u32 {
        rel_time(t, self.reader.time_base())
    }

    pub fn user_name(&self, uid: u32) -> Option<Str> {
        let m = self.names.get_or_init(|| {
            let mut m = HashMap::default();
            if let Ok(us) = self.reader.users() {
                for u in us {
                    m.insert(u.uid, u.name.clone());
                }
            }
            m
        });
        m.get(&uid).cloned()
    }

    /// Positions of nodes at individual times: `(node id, time) -> position`.
    pub fn positions(&self, reqs: &[(i64, i64)]) -> HashMap<(i64, i64), Pos> {
        let mut want: HashMap<i64, Vec<i64>> = HashMap::default();
        for &(id, t) in reqs {
            want.entry(id).or_default().push(t);
        }
        let mut ids: Vec<i64> = want.keys().copied().collect();
        ids.sort_unstable();
        let lists: Vec<Vec<u32>> = ids
            .par_chunks(4096)
            .map(|c| self.reader.lookup_sorted(ObjType::Node, c).unwrap_or_else(|_| vec![Vec::new(); c.len()]))
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect();
        let mut by_block: HashMap<u32, Vec<i64>> = HashMap::default();
        for (&id, nos) in ids.iter().zip(&lists) {
            for &no in nos {
                by_block.entry(no).or_default().push(id);
            }
        }
        let mut blocks: Vec<(u32, Vec<i64>)> = by_block.into_iter().collect();
        blocks.sort_unstable_by_key(|b| b.0);
        let parts: Vec<Vec<((i64, i64), Pos)>> = blocks
            .par_iter()
            .map(|(no, ids)| {
                let mut out = Vec::new();
                let Ok(e) = self.reader.block_entry(*no) else { return out };
                if e.obj_type != ObjType::Node {
                    let Ok(b) = self.reader.block(*no) else { return out };
                    for &id in ids {
                        for &(si, k) in b.private_slots(id) {
                            if let SegmentBody::Way { slots, .. } = &b.segs[si as usize].body
                                && let Some(es) = &slots[k as usize].private
                            {
                                for &t in &want[&id] {
                                    if let Some(e) = entry_at(es, t) {
                                        out.push(((id, t), e.visible.then_some((e.lon, e.lat))));
                                    }
                                }
                            }
                        }
                    }
                    return out;
                }
                let set: HashSet<i64> = ids.iter().copied().collect();
                let tags = |_: &Tags| true;
                let keep = |d: &block::DirEntry, _: &[Meta], _: &[bool]| set.contains(&d.id);
                let Ok(b) = self.reader.block_filtered(*no, &block::SegFilter { tags: &tags, keep: &keep }) else { return out };
                for &id in ids {
                    for &t in &want[&id] {
                        let rt = self.rel(t);
                        for &si in b.segments_of(id) {
                            let s = &b.segs[si as usize];
                            if !b.dirs[si as usize].valid_at(rt) {
                                continue;
                            }
                            if let SegmentBody::Node(vs) = &s.body
                                && let Some(vi) = version_at(vs, |v| v.meta.timestamp, t)
                            {
                                let v = &vs[vi];
                                out.push(((id, t), v.meta.visible.then_some((v.lon, v.lat))));
                            }
                        }
                    }
                }
                out
            })
            .collect();
        let mut m: HashMap<(i64, i64), Pos> = HashMap::default();
        for p in parts {
            for (k, v) in p {
                if v.is_some() || !m.contains_key(&k) {
                    m.insert(k, v);
                }
            }
        }
        m
    }

    /// Node ids and positions of ways at given times; `None` if absent or deleted.
    pub fn way_geoms(&self, reqs: &[(i64, i64)]) -> HashMap<(i64, i64), Option<(Vec<i64>, Vec<Option<P>>)>> {
        let mut want: HashMap<i64, Vec<i64>> = HashMap::default();
        for &(id, t) in reqs {
            want.entry(id).or_default().push(t);
        }
        let mut ids: Vec<i64> = want.keys().copied().collect();
        ids.sort_unstable();
        let lists = self.reader.lookup_sorted(ObjType::Way, &ids).unwrap_or_default();
        let mut by_block: HashMap<u32, Vec<i64>> = HashMap::default();
        for (&id, nos) in ids.iter().zip(&lists) {
            for &no in nos {
                by_block.entry(no).or_default().push(id);
            }
        }
        let mut blocks: Vec<(u32, Vec<i64>)> = by_block.into_iter().collect();
        blocks.sort_unstable_by_key(|b| b.0);
        type Local = ((i64, i64), Option<(Vec<i64>, Vec<Pos>)>);
        let parts: Vec<Vec<Local>> = blocks
            .par_iter()
            .map(|(no, ids)| {
                let mut out = Vec::new();
                let set: HashSet<i64> = ids.iter().copied().collect();
                let tags = |_: &Tags| true;
                let keep = |d: &block::DirEntry, _: &[Meta], _: &[bool]| set.contains(&d.id);
                let Ok(b) = self.reader.block_filtered(*no, &block::SegFilter { tags: &tags, keep: &keep }) else { return out };
                for &id in ids {
                    for &t in &want[&id] {
                        let rt = self.rel(t);
                        for &si in b.segments_of(id) {
                            if !b.dirs[si as usize].valid_at(rt) {
                                continue;
                            }
                            let SegmentBody::Way { slots, versions } = &b.segs[si as usize].body else { continue };
                            let Some(vi) = version_at(versions, |v| v.meta.timestamp, t) else { continue };
                            let v = &versions[vi];
                            if !v.meta.visible {
                                out.push(((id, t), None));
                                continue;
                            }
                            let mut nids = Vec::with_capacity(v.nodes.len());
                            let mut pos = Vec::with_capacity(v.nodes.len());
                            for &i in &v.nodes {
                                let sl = &slots[i as usize];
                                nids.push(sl.node_id);
                                pos.push(match &sl.private {
                                    Some(es) => entry_at(es, t).and_then(|e| e.visible.then_some((e.lon, e.lat))),
                                    None => None,
                                });
                            }
                            out.push(((id, t), Some((nids, pos))));
                        }
                    }
                }
                out
            })
            .collect();
        let mut local: HashMap<(i64, i64), Option<(Vec<i64>, Vec<Pos>)>> = HashMap::default();
        for p in parts {
            for (k, v) in p {
                if v.is_some() || !local.contains_key(&k) {
                    local.insert(k, v);
                }
            }
        }
        // shared nodes
        let mut pending = Vec::new();
        for ((_, t), v) in &local {
            if let Some((nids, pos)) = v {
                for (n, p) in nids.iter().zip(pos) {
                    if p.is_none() {
                        pending.push((*n, *t));
                    }
                }
            }
        }
        let resolved = self.positions(&pending);
        local
            .into_iter()
            .map(|(k, v)| {
                let v = v.map(|(nids, pos)| {
                    let pts = nids
                        .iter()
                        .zip(pos)
                        .map(|(n, p)| p.or_else(|| resolved.get(&(*n, k.1)).copied().flatten()).map(geom::e7))
                        .collect();
                    (nids, pts)
                });
                (k, v)
            })
            .collect()
    }

    /// Full history of an object.
    pub fn history(&self, t: ObjType, id: i64) -> Arc<Hist> {
        let fs = self.reader.get(t, id).unwrap_or_default();
        let versions = fs
            .into_iter()
            .map(|f| Ver {
                version: f.meta.version,
                ts: f.meta.timestamp,
                changeset: f.meta.changeset,
                uid: f.meta.uid,
                visible: f.meta.visible,
                tags: f.tags,
                nodes: Arc::from(f.nodes),
                members: Arc::from(f.members),
                pos: match f.geometry {
                    Geometry::Point(x, y) => Some((x, y)),
                    _ => None,
                },
            })
            .collect();
        Arc::new(Hist { otype: t, id, versions })
    }

    /// Multipolygon of a relation's way members at time `t` (nested relations ignored).
    pub fn area_geom(&self, members: &[Member], t: i64) -> Geom {
        let reqs: Vec<(i64, i64)> = members.iter().filter(|m| m.kind == ObjType::Way).map(|m| (m.id, t)).collect();
        let ways = self.way_geoms(&reqs);
        let pieces = members
            .iter()
            .filter(|m| m.kind == ObjType::Way)
            .filter_map(|m| {
                let (nids, pts) = ways.get(&(m.id, t))?.as_ref()?;
                if pts.iter().any(|p| p.is_none()) {
                    return None;
                }
                Some((m.role.as_ref() == "inner", nids.clone(), pts.iter().map(|p| p.unwrap()).collect()))
            })
            .collect();
        geom::assemble(pieces)
    }

    /// Geometry of a non-area relation: its node and way members at `t`.
    pub fn relation_geom(&self, members: &[Member], t: i64) -> Geom {
        let wreq: Vec<(i64, i64)> = members.iter().filter(|m| m.kind == ObjType::Way).map(|m| (m.id, t)).collect();
        let nreq: Vec<(i64, i64)> = members.iter().filter(|m| m.kind == ObjType::Node).map(|m| (m.id, t)).collect();
        let ways = self.way_geoms(&wreq);
        let nodes = self.positions(&nreq);
        let mut parts = Vec::new();
        for m in members {
            match m.kind {
                ObjType::Node => {
                    if let Some(Some(p)) = nodes.get(&(m.id, t)) {
                        parts.push(Geom::Point(geom::e7(*p)));
                    }
                }
                ObjType::Way => {
                    if let Some(Some((_, pts))) = ways.get(&(m.id, t)) {
                        let l: Vec<P> = pts.iter().flatten().copied().collect();
                        if !l.is_empty() {
                            parts.push(Geom::Line(l));
                        }
                    }
                }
                ObjType::Relation => {}
            }
        }
        Geom::Multi(parts)
    }
}
