//! Changesets and users (KNUT_LANG.md §7.2): an index derived from the history once and
//! stored in the container (element "Changesets and users"; older databases: a side file
//! `x.changesets.knutidx`). Comments and tags come from an optional changeset dump.

use crate::arith::duration;
use crate::db::Db;
use crate::eval::{Ctx, lock};
use crate::geom::{BBoxF, Geom};
use crate::region::Region;
use crate::set::{SetSrc, SetV};
use crate::value::*;
use knut_format::SegmentBody;
use knut_lang::dates;
use knut_lang::diag::{Diagnostic, Span, Text2};
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct RawCs {
    id: u64,
    uid: u32,
    t0: i64,
    t1: i64,
    edits: u64,
    bbox: [f64; 4],
    comment: Option<String>,
    tags: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct File {
    /// Identifies the database the index was built from.
    stamp: (i64, i64, u64, u64),
    recs: Vec<RawCs>,
}

pub struct CsIndex {
    pub recs: Vec<Arc<CsRec>>,
    by_id: HashMap<u64, u32>,
    pub users: Vec<Arc<UserRec>>,
    by_uid: HashMap<u32, u32>,
}

impl CsIndex {
    pub fn get(&self, id: u64) -> Option<&Arc<CsRec>> {
        self.by_id.get(&id).map(|&i| &self.recs[i as usize])
    }

    pub fn user(&self, uid: u32) -> Option<&Arc<UserRec>> {
        self.by_uid.get(&uid).map(|&i| &self.users[i as usize])
    }
}

pub fn sidecar(db: &Path, what: &str) -> PathBuf {
    let s = db.to_string_lossy();
    let base = s.strip_suffix(".knutdb").unwrap_or(&s);
    PathBuf::from(format!("{base}.{what}.knutidx"))
}

fn stamp(db: &Db) -> (i64, i64, u64, u64) {
    let h = &db.reader.header;
    (h.tmin, h.tmax, h.n_versions.iter().sum(), h.n_blocks)
}

#[derive(Clone, Copy)]
struct Part {
    uid: u32,
    t0: i64,
    t1: i64,
    edits: u64,
    bbox: BBoxF,
}

fn add(m: &mut HashMap<u64, Part>, cs: u64, uid: u32, t: i64, b: &BBoxF) {
    let e = m.entry(cs).or_insert(Part { uid, t0: t, t1: t, edits: 0, bbox: BBoxF::EMPTY });
    e.t0 = e.t0.min(t);
    e.t1 = e.t1.max(t);
    e.edits += 1;
    e.bbox.union(b);
}

/// Builds the changeset index from all versions in the database.
fn build(db: &Db) -> Result<Vec<RawCs>, String> {
    let n = db.reader.n_blocks() as u32;
    let parts: Vec<HashMap<u64, Part>> = (0..n)
        .into_par_iter()
        .map(|no| -> Result<HashMap<u64, Part>, String> {
            let b = db.reader.block(no).map_err(|e| e.to_string())?;
            let mut m: HashMap<u64, Part> = HashMap::default();
            for (si, s) in b.segs.iter().enumerate() {
                let sb = BBoxF::from_e7(&b.dirs[si].bbox);
                match &s.body {
                    SegmentBody::Node(vs) => {
                        for v in vs.iter().filter(|v| !v.meta.continuation) {
                            let bb = if v.meta.visible {
                                let p = crate::geom::e7((v.lon, v.lat));
                                BBoxF { minx: p.0, maxx: p.0, miny: p.1, maxy: p.1 }
                            } else {
                                BBoxF::EMPTY
                            };
                            add(&mut m, v.meta.changeset, v.meta.uid, v.meta.timestamp, &bb);
                        }
                    }
                    SegmentBody::Way { slots, versions } => {
                        for v in versions.iter().filter(|v| !v.meta.continuation) {
                            add(&mut m, v.meta.changeset, v.meta.uid, v.meta.timestamp, &sb);
                        }
                        for sl in slots {
                            for e in sl.private.as_deref().unwrap_or(&[]) {
                                if e.continuation || e.changeset == 0 {
                                    continue;
                                }
                                let bb = if e.visible {
                                    let p = crate::geom::e7((e.lon, e.lat));
                                    BBoxF { minx: p.0, maxx: p.0, miny: p.1, maxy: p.1 }
                                } else {
                                    BBoxF::EMPTY
                                };
                                add(&mut m, e.changeset, e.uid, e.timestamp, &bb);
                            }
                        }
                    }
                    SegmentBody::Relation(vs) => {
                        for v in vs.iter().filter(|v| !v.meta.continuation) {
                            add(&mut m, v.meta.changeset, v.meta.uid, v.meta.timestamp, &sb);
                        }
                    }
                }
            }
            Ok(m)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut all: HashMap<u64, Part> = HashMap::default();
    for p in parts {
        for (k, v) in p {
            match all.get_mut(&k) {
                Some(a) => {
                    a.t0 = a.t0.min(v.t0);
                    a.t1 = a.t1.max(v.t1);
                    a.edits += v.edits;
                    a.bbox.union(&v.bbox);
                }
                None => {
                    all.insert(k, v);
                }
            }
        }
    }
    let mut recs: Vec<RawCs> = all
        .into_iter()
        .filter(|(id, _)| *id != 0)
        .map(|(id, p)| RawCs {
            id,
            uid: p.uid,
            t0: p.t0,
            t1: p.t1,
            edits: p.edits,
            bbox: [p.bbox.minx, p.bbox.miny, p.bbox.maxx, p.bbox.maxy],
            comment: None,
            tags: Vec::new(),
        })
        .collect();
    recs.sort_by_key(|r| r.id);
    Ok(recs)
}

fn encode(f: &File) -> Result<Vec<u8>, String> {
    let bytes = postcard::to_allocvec(f).map_err(|e| e.to_string())?;
    let z = zstd::encode_all(&bytes[..], 3).map_err(|e| e.to_string())?;
    let mut out = b"KNUTCS1\0".to_vec();
    out.extend_from_slice(&z);
    Ok(out)
}

fn decode(b: &[u8]) -> Option<File> {
    let z = b.strip_prefix(b"KNUTCS1\0")?;
    let raw = zstd::decode_all(z).ok()?;
    postcard::from_bytes(&raw).ok()
}

fn to_index(recs: Vec<RawCs>, db: &Db) -> CsIndex {
    let recs: Vec<Arc<CsRec>> = recs
        .into_iter()
        .map(|r| {
            Arc::new(CsRec {
                id: r.id,
                uid: r.uid,
                t0: r.t0,
                t1: r.t1,
                edits: r.edits,
                bbox: BBoxF { minx: r.bbox[0], miny: r.bbox[1], maxx: r.bbox[2], maxy: r.bbox[3] },
                comment: r.comment.map(|c| Arc::from(c.as_str())),
                tags: r.tags.into_iter().map(|(k, v)| (Arc::from(k.as_str()), Arc::from(v.as_str()))).collect::<Vec<_>>().into(),
            })
        })
        .collect();
    let by_id = recs.iter().enumerate().map(|(i, r)| (r.id, i as u32)).collect();
    let mut per_user: HashMap<u32, Vec<(i64, u64, u64)>> = HashMap::default();
    for r in &recs {
        per_user.entry(r.uid).or_default().push((r.t0, r.id, r.edits));
    }
    let mut users: Vec<Arc<UserRec>> = per_user
        .into_iter()
        .map(|(uid, mut cs)| {
            cs.sort_unstable();
            Arc::new(UserRec {
                uid,
                name: if db.users { db.user_name(uid) } else { None },
                edits: cs.iter().map(|c| c.2).sum(),
                changesets: cs.into_iter().map(|c| c.1).collect(),
            })
        })
        .collect();
    users.sort_by_key(|u| u.uid);
    let by_uid = users.iter().enumerate().map(|(i, u)| (u.uid, i as u32)).collect();
    CsIndex { recs, by_id, users, by_uid }
}

/// Changeset indexes decoded so far, per database: kept for the process (decoding the one of
/// Germany takes seconds).
static SHARED: std::sync::Mutex<Vec<(std::path::PathBuf, i64, Arc<CsIndex>)>> = std::sync::Mutex::new(Vec::new());

fn remembered(db: &Db) -> Option<Arc<CsIndex>> {
    lock(&SHARED).iter().find(|x| x.0 == db.path && x.1 == db.latest).map(|x| x.2.clone())
}

fn remember(db: &Db, ix: Arc<CsIndex>) -> Arc<CsIndex> {
    let mut all = lock(&SHARED);
    if let Some(x) = all.iter().find(|x| x.0 == db.path && x.1 == db.latest) {
        return x.2.clone();
    }
    all.push((db.path.clone(), db.latest, ix.clone()));
    ix
}

/// The stored changeset index (container element or side file), decoded once per process;
/// `None` if there is none.
pub fn shared(db: &Db) -> Option<Arc<CsIndex>> {
    if let Some(ix) = remembered(db) {
        return Some(ix);
    }
    let f = match db.reader.element(knut_format::container::ElementKind::Changesets) {
        Some(el) => el.sections.first().and_then(|s| db.reader.section_bytes(s).ok()).and_then(decode),
        None => std::fs::read(sidecar(&db.path, "changesets")).ok().as_deref().and_then(decode).filter(|f| f.stamp == stamp(db)),
    }?;
    Some(remember(db, Arc::new(to_index(f.recs, db))))
}

/// The changeset index: from the container element, else from an older side file next to
/// the database, else built now (and not stored: `knut manage FILE add changesets` stores it).
/// Kept for the process once it is there.
pub fn index(ctx: &Ctx) -> R<Arc<CsIndex>> {
    ctx.caches
        .cs
        .get_or_init(|| {
            let db = ctx.db()?;
            if let Some(ix) = shared(db) {
                return Ok(ix);
            }
            lock(&ctx.caches.notices).push(format!(
                "Deriving changesets and users from all versions; to keep them: knut manage {} add changesets",
                db.path.display()
            ));
            let recs = build(db).map_err(|e| RtErr::internal(Span::default(), e))?;
            Ok(remember(db, Arc::new(to_index(recs, db))))
        })
        .clone()
}

/// The section of the element "Changesets and users", optionally with comments and tags
/// from a changeset dump.
pub fn build_section(db: &Db, dump: Option<&Path>, log: &mut dyn FnMut(&str)) -> Result<Vec<u8>, String> {
    log("reading all versions …");
    let mut recs = build(db)?;
    if let Some(d) = dump {
        log("reading the changeset dump …");
        let by_id: HashMap<u64, usize> = recs.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
        crate::notes::read_changeset_dump(d, &mut |id, tags| {
            if let Some(&i) = by_id.get(&id) {
                recs[i].comment = tags.iter().find(|(k, _)| k == "comment").map(|x| x.1.clone());
                recs[i].tags = tags;
            }
        })?;
    }
    encode(&File { stamp: stamp(db), recs })
}

pub fn cs_value(r: &Arc<CsRec>) -> Value {
    let mut v = Ver::empty();
    v.ts = r.t0;
    v.uid = r.uid;
    v.changeset = r.id;
    let mut e = Elem::new(EKind::Changeset, None, r.id as i64, v, r.t0);
    e.extra = Extra::Changeset(r.clone());
    if !r.bbox.is_empty() {
        let _ = e.geom.set(Arc::new(Geom::Point(r.bbox.center())));
    }
    Value::Elem(Arc::new(e))
}

pub fn user_value(u: &Arc<UserRec>) -> Value {
    let mut v = Ver::empty();
    v.uid = u.uid;
    let mut e = Elem::new(EKind::User, None, u.uid as i64, v, 0);
    e.extra = Extra::User(u.clone());
    Value::Elem(Arc::new(e))
}

pub fn user_elem(ctx: &Ctx, uid: u32) -> Value {
    if let Some(Ok(ix)) = ctx.caches.cs.get()
        && let Some(u) = ix.user(uid)
    {
        return user_value(u);
    }
    let name = ctx.db_arc().and_then(|d| if d.users { d.user_name(uid) } else { None });
    user_value(&Arc::new(UserRec { uid, name, changesets: Vec::new(), edits: 0 }))
}

pub fn changeset_elem(ctx: &Ctx, id: u64, uid: u32, ts: i64) -> Value {
    if id == 0 {
        return Value::None;
    }
    if let Some(Ok(ix)) = ctx.caches.cs.get()
        && let Some(r) = ix.get(id)
    {
        return cs_value(r);
    }
    // minimal record; fields that need the index load it on demand
    cs_value(&Arc::new(CsRec { id, uid, t0: ts, t1: ts, edits: 0, bbox: BBoxF::EMPTY, comment: None, tags: Arc::from(Vec::new()) }))
}

fn full_cs(ctx: &Ctx, c: &Arc<CsRec>) -> R<Arc<CsRec>> {
    if c.edits > 0 {
        return Ok(c.clone());
    }
    let ix = index(ctx)?;
    Ok(ix.get(c.id).cloned().unwrap_or_else(|| c.clone()))
}

fn full_user(ctx: &Ctx, u: &Arc<UserRec>) -> R<Arc<UserRec>> {
    if !u.changesets.is_empty() {
        return Ok(u.clone());
    }
    let ix = index(ctx)?;
    Ok(ix.user(u.uid).cloned().unwrap_or_else(|| u.clone()))
}

pub fn all_changesets(ctx: &Ctx, _span: Span) -> R<Value> {
    let ix = index(ctx)?;
    let n = ix.recs.len();
    let get = Arc::new(move |i: usize| cs_value(&ix.recs[i]));
    Ok(Value::Set(Arc::new(SetV::lazy(SetSrc::Indexed { len: n, get }))))
}

pub fn all_users(ctx: &Ctx, span: Span) -> R<Value> {
    let db = ctx.db()?;
    if !db.users {
        return Err(RtErr::fatal(
            Diagnostic::error("E207", span, Text2::new("This database contains no user information.", "Diese Datenbank enthält keine Angaben zu Nutzer:innen."))
                .why("It was built with --strip-users.", "Sie wurde mit --strip-users gebaut."),
        ));
    }
    let ix = index(ctx)?;
    let n = ix.users.len();
    let get = Arc::new(move |i: usize| user_value(&ix.users[i]));
    Ok(Value::Set(Arc::new(SetV::lazy(SetSrc::Indexed { len: n, get }))))
}

/// Previous / next changeset of the same user.
pub fn neighbour(ctx: &Ctx, e: &Elem, next: bool) -> Option<Arc<Elem>> {
    let ix = index(ctx).ok()?;
    let u = ix.user(e.v.uid)?;
    let i = u.changesets.iter().position(|&c| c == e.id as u64)?;
    let j = if next { i + 1 } else { i.checked_sub(1)? };
    let id = *u.changesets.get(j)?;
    match cs_value(ix.get(id)?) {
        Value::Elem(x) => Some(x),
        _ => None,
    }
}

pub fn changeset_field(ctx: &Ctx, e: &Arc<Elem>, c: &Arc<CsRec>, f: &str, span: Span) -> R<Value> {
    let c = match f {
        "id" | "user" => c.clone(),
        _ => full_cs(ctx, c)?,
    };
    Ok(match f {
        "id" => Value::num(c.id as f64),
        "type" => Value::text("changeset"),
        "user" => user_elem(ctx, c.uid),
        "time" => Value::instant(c.t0),
        "closedAt" | "lastTime" => Value::instant(c.t1),
        "duration" => Value::Num(duration((c.t1 - c.t0) as f64)),
        "count" | "edits" => Value::num(c.edits as f64),
        "comment" => c.comment.clone().map_or(Value::None, Value::Text),
        "bbox" | "geometry" => {
            let b = c.bbox;
            Value::Region(Arc::new(Region::Box(b)))
        }
        "center" => Value::Region(Arc::new(Region::from_geom(Arc::new(Geom::Point(c.bbox.center())), format!("changeset {}", c.id)))),
        "next" | "previous" => neighbour(ctx, e, f == "next").map_or(Value::None, Value::Elem),
        "unchangedBefore" => match neighbour(ctx, e, false) {
            Some(p) => match &p.extra {
                Extra::Changeset(pc) => Value::Num(duration((c.t0 - pc.t1).max(0) as f64)),
                _ => Value::None,
            },
            None => Value::None,
        },
        "unchangedAfter" => match neighbour(ctx, e, true) {
            Some(n) => match &n.extra {
                Extra::Changeset(nc) => Value::Num(duration((nc.t0 - c.t1).max(0) as f64)),
                _ => Value::None,
            },
            None => Value::None,
        },
        _ => {
            return Err(RtErr::fatal(Diagnostic::error(
                "E203",
                span,
                Text2::new(format!("\"{f}\" is not a field of changesets."), format!("\"{f}\" ist kein Feld von Changesets.")),
            )));
        }
    })
}

pub fn user_field(ctx: &Ctx, e: &Arc<Elem>, u: &Arc<UserRec>, f: &str, span: Span) -> R<Value> {
    let _ = e;
    match f {
        "id" => return Ok(Value::num(u.uid as f64)),
        "name" => return Ok(u.name.clone().map_or(Value::None, Value::Text)),
        "type" => return Ok(Value::text("user")),
        "home" => {
            return Err(RtErr::fatal(
                Diagnostic::error("E203", span, Text2::new("\"home\" is not a field of users.", "\"home\" ist kein Feld von Nutzer:innen."))
                    .why(
                        "There is no home location per person, on purpose: it would make\nindividuals identifiable.",
                        "Einen Wohnort pro Person gibt es absichtlich nicht: Er würde\nEinzelne identifizierbar machen.",
                    ),
            ));
        }
        _ => {}
    }
    let u = full_user(ctx, u)?;
    let ix = index(ctx)?;
    let recs: Vec<&Arc<CsRec>> = u.changesets.iter().filter_map(|c| ix.get(*c)).collect();
    Ok(match f {
        "changesets" => {
            let n = u.changesets.len();
            let ids = u.changesets.clone();
            let ix2 = ix.clone();
            let get = Arc::new(move |i: usize| ix2.get(ids[i]).map_or(Value::None, cs_value));
            let _ = n;
            Value::Set(Arc::new(SetV::from_vec((0..u.changesets.len()).map(|i| get(i)).collect())))
        }
        "edits" => Value::num(u.edits as f64),
        "firstEdit" => recs.iter().map(|r| r.t0).min().map_or(Value::None, Value::instant),
        "lastEdit" => recs.iter().map(|r| r.t1).max().map_or(Value::None, Value::instant),
        "activeSpan" => match (recs.iter().map(|r| r.t0).min(), recs.iter().map(|r| r.t1).max()) {
            (Some(a), Some(b)) => Value::Num(duration((b - a) as f64)),
            _ => Value::None,
        },
        "activeDays" => {
            let mut days: Vec<i64> = recs.iter().map(|r| dates::bucket(r.t0, dates::TimeUnit::Day).start).collect();
            days.sort_unstable();
            days.dedup();
            Value::num(days.len() as f64)
        }
        _ => {
            return Err(RtErr::fatal(Diagnostic::error(
                "E203",
                span,
                Text2::new(format!("\"{f}\" is not a field of users."), format!("\"{f}\" ist kein Feld von Nutzer:innen.")),
            )));
        }
    })
}
