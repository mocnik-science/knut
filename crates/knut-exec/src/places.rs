//! Place names for `within Vienna` / `within AT` (KNUT_LANG.md §7.2): boundary and place
//! relations matched by name or ISO code.

use crate::eval::{Ctx, lock};
use knut_format::ObjType;
use knut_format::container::ElementKind;
use crate::region::Region;
use crate::scan::{self, Item, KindMask, ScanSpec, TimeMode};
use crate::value::*;
use knut_lang::diag::{self, Diagnostic, Header, Span, Text2};
use knut_plan::pushdown::TagPred;
use std::sync::{Arc, Mutex};

const NAME_KEYS: &[(&str, u8)] = &[
    ("name", 0),
    ("ISO3166-1", 0),
    ("ISO3166-1:alpha2", 0),
    ("ISO3166-1:alpha3", 0),
    ("ISO3166-2", 0),
    ("name:en", 1),
    ("int_name", 1),
    ("name:de", 2),
    ("official_name", 2),
    ("short_name", 2),
    ("alt_name", 3),
];

struct Cand {
    elem: Arc<Elem>,
    prio: u8,
    level: u32,
    admin: bool,
}

fn admin_level(e: &Elem) -> u32 {
    e.tag("admin_level").and_then(|s| s.parse().ok()).unwrap_or(99)
}

/// The region for a place name (cached).
pub fn region_value(ctx: &Ctx, name: &str, span: Span) -> R<Value> {
    if let Some(v) = lock(&ctx.caches.places).get(name) {
        return v.clone();
    }
    let r = lookup(ctx, name, span);
    lock(&ctx.caches.places).insert(name.to_string(), r.clone());
    r
}

fn spec(db: &crate::db::Db) -> ScanSpec {
    ScanSpec {
        bbox: None,
        kinds: KindMask { relation: true, ..Default::default() },
        time: TimeMode::State(db.latest),
        region: None,
        pred: TagPred::Or(vec![TagPred::Has("boundary".into()), TagPred::Has("place".into()), TagPred::Has("ISO3166-1".into())]),
        need_geom: false,
        need_hist: false,
        attrs_history: false,
    }
}

fn is_place(e: &Elem) -> bool {
    crate::areas::relation_is_area(&e.v.tags) || e.tag("boundary").is_some()
}

fn lookup(ctx: &Ctx, name: &str, span: Span) -> R<Value> {
    let db = ctx.db()?;
    let lname = name.to_lowercase();
    let (cands, names) = match db.reader.element(ElementKind::PlaceIndex) {
        Some(el) => from_index(db, el, &lname)?,
        None => from_scan(ctx, &lname)?,
    };
    choose(ctx, name, span, cands, names)
}

/// Candidates from the place index; the names for suggestions only if there is none.
fn from_index(db: &crate::db::Db, el: &knut_format::container::Element, lname: &str) -> R<(Vec<Cand>, Vec<String>)> {
    let err = |e: String| RtErr::internal(Span::default(), e);
    let s = el.sections.first().ok_or_else(|| err("empty place index".into()))?;
    let ix = PlaceIx::new(db.reader.section_bytes(s).map_err(|e| err(e.to_string()))?).map_err(err)?;
    let mut cands = Vec::new();
    for (entry, prio) in ix.find(lname) {
        let (otype, id, level, admin) = ix.entry(entry);
        let h = db.history(otype, id);
        let Some(v) = h.versions.iter().rev().find(|v| v.ts <= db.latest).filter(|v| v.visible) else { continue };
        let e = Arc::new(Elem::new(EKind::Relation, Some(otype), id, v.clone(), db.latest));
        cands.push(Cand { elem: e, prio, level, admin });
    }
    let names = if cands.is_empty() { ix.names_up_to_level(8) } else { Vec::new() };
    Ok((cands, names))
}

fn from_scan(ctx: &Ctx, lname: &str) -> R<(Vec<Cand>, Vec<String>)> {
    let db = ctx.db()?;
    let spec = spec(db);
    let all_names: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let parts = scan::fold(db, ctx.control.as_deref(), &spec, &Vec::new, &|acc: &mut Vec<Cand>, it: Item| {
        let Item::Elem(e) = it else { return Ok(()) };
        if !is_place(&e) {
            return Ok(());
        }
        let mut best: Option<u8> = None;
        for (k, p) in NAME_KEYS {
            if let Some(v) = e.tag(k)
                && v.to_lowercase() == lname
            {
                best = Some(best.map_or(*p, |b| b.min(*p)));
            }
        }
        if let Some(n) = e.tag("name")
            && admin_level(&e) <= 8
        {
            lock(&all_names).push(n.to_string());
        }
        if let Some(p) = best {
            let admin = e.tag("boundary").is_some_and(|b| b.as_ref() == "administrative");
            acc.push(Cand { level: admin_level(&e), elem: e, prio: p, admin });
        }
        Ok(())
    })?;
    Ok((parts.into_iter().flatten().collect(), all_names.into_inner().unwrap_or_default()))
}

fn choose(ctx: &Ctx, name: &str, span: Span, mut cands: Vec<Cand>, names: Vec<String>) -> R<Value> {
    if cands.is_empty() {
        let mut d = Diagnostic::error(
            "E210",
            span,
            Text2::new(format!("No region called \"{name}\" is known."), format!("Eine Region namens \"{name}\" ist nicht bekannt.")),
        )
        .why(
            "Regions are found by the name or ISO code of boundaries in the database.",
            "Regionen werden über Namen oder ISO-Codes von Grenzen in der Datenbank gefunden.",
        );
        if let Some(c) = diag::closest(name, names.iter().map(|s| s.as_str())) {
            d = d.header(Header::DidYouMean).sugg(format!("within '{c}'"));
        }
        return Err(RtErr::fatal(d));
    }
    cands.sort_by_key(|c| (c.prio, !c.admin, c.level, c.elem.id));
    let best = (cands[0].prio, cands[0].admin, cands[0].level);
    let tied: Vec<&Cand> = cands.iter().filter(|c| (c.prio, c.admin, c.level) == best).collect();
    if tied.len() > 1 {
        let mut d = Diagnostic::error(
            "E211",
            span,
            Text2::new(
                format!("\"{name}\" matches {} regions.", tied.len()),
                format!("\"{name}\" passt auf {} Regionen.", tied.len()),
            ),
        )
        .why("Choose one by its relation id, for example:", "Wähle eine über ihre Relations-ID, zum Beispiel:");
        for c in tied.iter().take(5) {
            let label = c.elem.tag("name").map(|s| s.to_string()).unwrap_or_default();
            d.suggestions.push(diag::Sugg {
                label: None,
                code: format!("within (pick area a with a.id == {} and a.osmType == 'relation')", c.elem.id),
                note: Some(Text2::same(format!("{label}, admin_level {}", c.level))),
            });
        }
        return Err(RtErr::fatal(d));
    }
    let e = &tied[0].elem;
    let mut ar = Elem::new(EKind::Area, e.otype, e.id, e.v.clone(), e.at);
    ar.bbox = e.bbox;
    // read from the index of the boundaries if it holds this version, else assembled
    let g = match crate::boundaries::area_geom(ctx.db()?, &ar) {
        Some(g) => g,
        None => ctx.elem_geom(&ar),
    };
    if g.is_empty() {
        return Err(RtErr::fatal(Diagnostic::error(
            "E210",
            span,
            Text2::new(
                format!("The boundary of \"{name}\" is incomplete in this database."),
                format!("Die Grenze von \"{name}\" ist in dieser Datenbank unvollständig."),
            ),
        )));
    }
    Ok(Value::Region(Arc::new(Region::from_geom(g, name.to_string()))))
}

// ----- the place index (container element) ------------------------------------------------

/// One place: a boundary or place relation with all its names (lowercase) and their
/// priority (see `NAME_KEYS`).
pub struct PlaceEntry {
    pub otype: ObjType,
    pub id: i64,
    pub level: u32,
    pub admin: bool,
    pub names: Vec<(u8, String)>,
}

/// All places at the last moment of the data.
pub fn collect(db: &crate::db::Db, ctl: Option<&crate::control::Control>) -> R<Vec<PlaceEntry>> {
    let spec = spec(db);
    let parts = scan::fold(db, ctl, &spec, &Vec::new, &|acc: &mut Vec<PlaceEntry>, it: Item| {
        let Item::Elem(e) = it else { return Ok(()) };
        if !is_place(&e) {
            return Ok(());
        }
        let mut names: Vec<(u8, String)> = Vec::new();
        for (k, p) in NAME_KEYS {
            if let Some(v) = e.tag(k) {
                let l = v.to_lowercase();
                match names.iter_mut().find(|x| x.1 == l) {
                    Some(x) => x.0 = x.0.min(*p),
                    None => names.push((*p, l)),
                }
            }
        }
        if names.is_empty() {
            return Ok(());
        }
        acc.push(PlaceEntry {
            otype: e.otype.unwrap_or(ObjType::Relation),
            id: e.id,
            level: admin_level(&e),
            admin: e.tag("boundary").is_some_and(|b| b.as_ref() == "administrative"),
            names,
        });
        Ok(())
    })?;
    let mut v: Vec<PlaceEntry> = parts.into_iter().flatten().collect();
    v.sort_by_key(|p| (p.otype, p.id));
    v.dedup_by_key(|p| (p.otype, p.id));
    Ok(v)
}

const PLACE_MAGIC: &[u8; 8] = b"KPLACE01";

/// The section of the place index:
/// magic, u32 entries, u32 names, u32 postings;
/// entries (16 B: u8 type, u8 administrative, u16 0, u32 admin level, i64 id);
/// (names + 1) × u32 offsets into the name bytes; (names + 1) × u32 first posting;
/// postings (8 B: u32 entry, u8 priority, 3 × 0); name bytes (lowercase, sorted).
pub fn encode(entries: &[PlaceEntry]) -> Vec<u8> {
    let mut names: Vec<(&str, u32, u8)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        for (p, n) in &e.names {
            names.push((n.as_str(), i as u32, *p));
        }
    }
    names.sort();
    let mut b = Vec::new();
    b.extend_from_slice(PLACE_MAGIC);
    let distinct: Vec<&str> = {
        let mut d: Vec<&str> = names.iter().map(|x| x.0).collect();
        d.dedup();
        d
    };
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    b.extend_from_slice(&(distinct.len() as u32).to_le_bytes());
    b.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for e in entries {
        b.push(e.otype as u8);
        b.push(e.admin as u8);
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&e.level.to_le_bytes());
        b.extend_from_slice(&e.id.to_le_bytes());
    }
    let mut off = 0u32;
    for n in &distinct {
        b.extend_from_slice(&off.to_le_bytes());
        off += n.len() as u32;
    }
    b.extend_from_slice(&off.to_le_bytes());
    let mut k = 0usize;
    for n in &distinct {
        b.extend_from_slice(&(k as u32).to_le_bytes());
        while k < names.len() && names[k].0 == *n {
            k += 1;
        }
    }
    b.extend_from_slice(&(k as u32).to_le_bytes());
    for (_, e, p) in &names {
        b.extend_from_slice(&e.to_le_bytes());
        b.extend_from_slice(&[*p, 0, 0, 0]);
    }
    for n in &distinct {
        b.extend_from_slice(n.as_bytes());
    }
    b
}

/// Reads the place index in place (binary search over the sorted names).
pub struct PlaceIx<'a> {
    b: &'a [u8],
    n_entries: usize,
    n_names: usize,
    entries: usize,
    name_off: usize,
    post_start: usize,
    posts: usize,
    blob: usize,
}

impl<'a> PlaceIx<'a> {
    pub fn new(b: &'a [u8]) -> Result<PlaceIx<'a>, String> {
        let u = |o: usize| b.get(o..o + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()) as usize).ok_or("place index truncated");
        if b.get(0..8) != Some(PLACE_MAGIC) {
            return Err("not a place index".into());
        }
        let (n_entries, n_names, n_posts) = (u(8)?, u(12)?, u(16)?);
        let entries = 20;
        let name_off = entries + 16 * n_entries;
        let post_start = name_off + 4 * (n_names + 1);
        let posts = post_start + 4 * (n_names + 1);
        let blob = posts + 8 * n_posts;
        let ix = PlaceIx { b, n_entries, n_names, entries, name_off, post_start, posts, blob };
        if blob + ix.u32(name_off + 4 * n_names) > b.len() {
            return Err("place index truncated".into());
        }
        Ok(ix)
    }

    fn u32(&self, o: usize) -> usize {
        u32::from_le_bytes(self.b[o..o + 4].try_into().unwrap()) as usize
    }

    fn name(&self, i: usize) -> &'a [u8] {
        let (a, z) = (self.u32(self.name_off + 4 * i), self.u32(self.name_off + 4 * i + 4));
        &self.b[self.blob + a..self.blob + z]
    }

    /// (entry, priority) of every place with this lowercase name.
    pub fn find(&self, lname: &str) -> Vec<(usize, u8)> {
        let key = lname.as_bytes();
        let (mut lo, mut hi) = (0, self.n_names);
        while lo < hi {
            let m = (lo + hi) / 2;
            if self.name(m) < key { lo = m + 1 } else { hi = m }
        }
        if lo >= self.n_names || self.name(lo) != key {
            return Vec::new();
        }
        let (a, z) = (self.u32(self.post_start + 4 * lo), self.u32(self.post_start + 4 * lo + 4));
        (a..z).map(|k| (self.u32(self.posts + 8 * k), self.b[self.posts + 8 * k + 4])).filter(|x| x.0 < self.n_entries).collect()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.n_entries
    }

    pub fn is_empty(&self) -> bool {
        self.n_entries == 0
    }

    /// (type, id, admin level, administrative) of an entry.
    pub fn entry(&self, i: usize) -> (ObjType, i64, u32, bool) {
        let o = self.entries + 16 * i;
        let id = i64::from_le_bytes(self.b[o + 8..o + 16].try_into().unwrap());
        (ObjType::from_u8(self.b[o]).unwrap_or(ObjType::Relation), id, self.u32(o + 4) as u32, self.b[o + 1] != 0)
    }

    /// Names of places up to an admin level (for suggestions).
    pub fn names_up_to_level(&self, level: u32) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..self.n_names {
            let (a, z) = (self.u32(self.post_start + 4 * i), self.u32(self.post_start + 4 * i + 4));
            if (a..z).any(|k| self.u32(self.posts + 8 * k) < self.n_entries && self.entry(self.u32(self.posts + 8 * k)).2 <= level) {
                out.push(String::from_utf8_lossy(self.name(i)).into_owned());
            }
        }
        out
    }
}
