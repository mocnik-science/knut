//! The index of administrative boundaries (container element, `knut manage FILE add
//! boundaries`): every relation with `boundary=administrative` and an `admin_level`, at the
//! last moment of the data, with its name, ISO code, version and its area already assembled
//! from the member ways. Address fields (§5.2b) and `within <place>` then read a boundary
//! instead of assembling it from the data blocks; without the index (or for another version
//! of a relation) they assemble it as before.

use crate::db::Db;
use crate::geom::{self, BBoxF, Geom, P, Polygon};
use crate::scan::{self, Item, KindMask, ScanSpec, TimeMode};
use crate::value::*;
use knut_format::{ObjType, Str};
use knut_format::codec::{get_varint, le, put_varint, unzigzag, zigzag};
use knut_format::container::ElementKind;
use knut_plan::pushdown::TagPred;
use rayon::prelude::*;
use std::sync::Arc;

const MAGIC: &[u8; 8] = b"KBOUND01";
const HEADER: usize = 32;
const ENTRY: usize = 80;

/// One boundary as collected for the index.
pub struct Boundary {
    pub id: i64,
    pub version: u32,
    pub level: u32,
    /// the box of the relation in the data blocks (for boundaries cut off by the extract,
    /// whose area is empty)
    pub ebbox: Option<BBoxF>,
    pub name: Option<String>,
    /// ISO 3166-1 alpha-2 for countries, else ISO 3166-2
    pub iso: Option<String>,
    pub geom: Geom,
}

/// The ISO code of a boundary as the address fields use it.
pub fn iso_of(e: &Elem, level: u32) -> Option<Str> {
    e.tag(if level == 2 { "ISO3166-1:alpha2" } else { "ISO3166-2" }).or_else(|| if level == 2 { e.tag("ISO3166-1") } else { None }).cloned()
}

/// The admin level of a boundary, if its tag is a plain number (as the address fields look
/// it up: `admin_level=8`, not `08`).
fn level_of(e: &Elem) -> Option<u32> {
    let s = e.tag("admin_level")?;
    let l: u32 = s.parse().ok()?;
    (l.to_string() == s.as_ref()).then_some(l)
}

/// All administrative boundaries at the last moment of the data, with their areas.
pub fn collect(db: &Db, ctl: Option<&crate::control::Control>) -> R<Vec<Boundary>> {
    let spec = ScanSpec {
        bbox: None,
        kinds: KindMask { relation: true, ..Default::default() },
        time: TimeMode::State(db.latest),
        region: None,
        pred: TagPred::And(vec![TagPred::Eq("boundary".into(), "administrative".into()), TagPred::Has("admin_level".into())]),
        need_geom: false,
        need_hist: false,
        attrs_history: false,
    };
    let parts = scan::fold(db, ctl, &spec, &Vec::new, &|acc: &mut Vec<Arc<Elem>>, it: Item| {
        if let Item::Elem(e) = it
            && e.otype == Some(ObjType::Relation)
            && level_of(&e).is_some()
        {
            acc.push(e);
        }
        Ok(())
    })?;
    let mut elems: Vec<Arc<Elem>> = parts.into_iter().flatten().collect();
    elems.sort_by_key(|e| e.id);
    elems.dedup_by_key(|e| e.id);
    // the areas as `ctx.elem_geom` assembles them, in groups (one read of the member ways each);
    // an area with a point off the data's grid is left out (it would not come back exactly)
    let out: Vec<Boundary> = elems
        .par_chunks(256)
        .flat_map_iter(|chunk| {
            let reqs: Vec<(bool, &[knut_format::Member], i64)> = chunk.iter().map(|e| (true, &e.v.members[..], e.at)).collect();
            let geoms = db.relation_geoms(&reqs);
            chunk
                .iter()
                .zip(geoms)
                .map(|(e, g)| {
                    let level = level_of(e).unwrap_or(0);
                    Boundary {
                        id: e.id,
                        version: e.v.version,
                        level,
                        ebbox: e.bbox,
                        name: e.tag("name").map(|s| s.to_string()),
                        iso: iso_of(e, level).map(|s| s.to_string()),
                        geom: g,
                    }
                })
                .filter(|b| exact(&b.geom))
                .collect::<Vec<_>>()
        })
        .collect();
    Ok(out)
}

// ----- the section ------------------------------------------------------------------------

/// The section: a header (magic, u32 entries, u32 0, u64 start of the strings, u64 start of
/// the areas), the entries sorted by id (80 B: i64 id, u32 version, u32 level, 4 × i32 box of
/// the relation in 1e-7°, all `i32::MIN` if none; 4 × i32 box of its area, likewise; u32 offset and u32 length of the name and of
/// the ISO code, length `u32::MAX` if none; u64 offset, u32 length and u32 unpacked length of
/// the area), the strings, the areas (each zstd-packed: the geometry as a tree, coordinates
/// in 1e-7° as zigzag varints, each the difference to the one before).
pub fn encode(bs: &[Boundary]) -> Vec<u8> {
    let mut order: Vec<&Boundary> = bs.iter().collect();
    order.sort_by_key(|b| b.id);
    let mut strings: Vec<u8> = Vec::new();
    let mut areas: Vec<u8> = Vec::new();
    let mut entries: Vec<u8> = Vec::with_capacity(order.len() * ENTRY);
    let put_str = |strings: &mut Vec<u8>, s: &Option<String>| -> (u32, u32) {
        match s {
            Some(s) => {
                let o = strings.len() as u32;
                strings.extend_from_slice(s.as_bytes());
                (o, s.len() as u32)
            }
            None => (0, u32::MAX),
        }
    };
    for b in &order {
        let (no, nl) = put_str(&mut strings, &b.name);
        let (io, il) = put_str(&mut strings, &b.iso);
        let mut raw = Vec::new();
        let mut prev = (0i64, 0i64);
        put_geom(&mut raw, &b.geom, &mut prev);
        let packed = zstd::bulk::compress(&raw, 9).unwrap_or_else(|_| raw.clone());
        let off = areas.len() as u64;
        areas.extend_from_slice(&packed);
        entries.extend_from_slice(&b.id.to_le_bytes());
        entries.extend_from_slice(&b.version.to_le_bytes());
        entries.extend_from_slice(&b.level.to_le_bytes());
        let eb = b.ebbox.filter(|x| !x.is_empty()).map(|x| x.to_e7());
        for v in eb.map_or([i32::MIN; 4], |x| [x.minx, x.miny, x.maxx, x.maxy]) {
            entries.extend_from_slice(&v.to_le_bytes());
        }
        let gb = Some(b.geom.bbox()).filter(|x| !x.is_empty()).map(|x| x.to_e7());
        for v in gb.map_or([i32::MIN; 4], |x| [x.minx, x.miny, x.maxx, x.maxy]) {
            entries.extend_from_slice(&v.to_le_bytes());
        }
        for v in [no, nl, io, il] {
            entries.extend_from_slice(&v.to_le_bytes());
        }
        entries.extend_from_slice(&off.to_le_bytes());
        entries.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        entries.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    }
    let mut out = Vec::with_capacity(HEADER + entries.len() + strings.len() + areas.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(order.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let s0 = (HEADER + entries.len()) as u64;
    out.extend_from_slice(&s0.to_le_bytes());
    out.extend_from_slice(&(s0 + strings.len() as u64).to_le_bytes());
    out.extend_from_slice(&entries);
    out.extend_from_slice(&strings);
    out.extend_from_slice(&areas);
    out
}

/// Coordinates in 1e-7° (the data's own resolution, so that they come back exactly).
fn fix(p: P) -> (i64, i64) {
    ((p.0 * 1e7).round() as i64, (p.1 * 1e7).round() as i64)
}

fn put_pt(out: &mut Vec<u8>, p: P, prev: &mut (i64, i64)) {
    let q = fix(p);
    put_varint(out, zigzag(q.0 - prev.0));
    put_varint(out, zigzag(q.1 - prev.1));
    *prev = q;
}

fn put_ring(out: &mut Vec<u8>, r: &[P], prev: &mut (i64, i64)) {
    put_varint(out, r.len() as u64);
    for p in r {
        put_pt(out, *p, prev);
    }
}

fn put_geom(out: &mut Vec<u8>, g: &Geom, prev: &mut (i64, i64)) {
    match g {
        Geom::Empty => out.push(0),
        Geom::Point(p) => {
            out.push(1);
            put_pt(out, *p, prev);
        }
        Geom::Line(ps) => {
            out.push(2);
            put_ring(out, ps, prev);
        }
        Geom::Poly(polys) => {
            out.push(3);
            put_varint(out, polys.len() as u64);
            for p in polys {
                put_varint(out, p.holes.len() as u64);
                put_ring(out, &p.outer, prev);
                for h in &p.holes {
                    put_ring(out, h, prev);
                }
            }
        }
        Geom::Multi(gs) => {
            out.push(4);
            put_varint(out, gs.len() as u64);
            for g in gs {
                put_geom(out, g, prev);
            }
        }
    }
}

/// Whether every coordinate of the geometry is one of 1e-7° (as the data's are): only then
/// does the index give it back exactly.
pub fn exact(g: &Geom) -> bool {
    let ok = |p: &P| {
        let q = fix(*p);
        geom::e7((q.0 as i32, q.1 as i32)) == *p
    };
    match g {
        Geom::Empty => true,
        Geom::Point(p) => ok(p),
        Geom::Line(ps) => ps.iter().all(ok),
        Geom::Poly(polys) => polys.iter().all(|p| p.outer.iter().all(ok) && p.holes.iter().all(|h| h.iter().all(ok))),
        Geom::Multi(gs) => gs.iter().all(exact),
    }
}

fn get_pt(b: &[u8], pos: &mut usize, prev: &mut (i64, i64)) -> Result<P, String> {
    let dx = unzigzag(get_varint(b, pos).map_err(|e| e.to_string())?);
    let dy = unzigzag(get_varint(b, pos).map_err(|e| e.to_string())?);
    *prev = (prev.0 + dx, prev.1 + dy);
    Ok(geom::e7((prev.0 as i32, prev.1 as i32)))
}

fn get_n(b: &[u8], pos: &mut usize) -> Result<usize, String> {
    let n = get_varint(b, pos).map_err(|e| e.to_string())? as usize;
    // at least one byte per item follows
    if n > b.len() { Err("corrupt boundary".into()) } else { Ok(n) }
}

fn get_ring(b: &[u8], pos: &mut usize, prev: &mut (i64, i64)) -> Result<Vec<P>, String> {
    let n = get_n(b, pos)?;
    (0..n).map(|_| get_pt(b, pos, prev)).collect()
}

fn get_geom(b: &[u8], pos: &mut usize, prev: &mut (i64, i64)) -> Result<Geom, String> {
    let tag = *b.get(*pos).ok_or("corrupt boundary")?;
    *pos += 1;
    Ok(match tag {
        0 => Geom::Empty,
        1 => Geom::Point(get_pt(b, pos, prev)?),
        2 => Geom::Line(get_ring(b, pos, prev)?),
        3 => {
            let n = get_n(b, pos)?;
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                let holes = get_n(b, pos)?;
                let outer = get_ring(b, pos, prev)?;
                let holes = (0..holes).map(|_| get_ring(b, pos, prev)).collect::<Result<Vec<_>, _>>()?;
                polys.push(Polygon { outer, holes });
            }
            Geom::Poly(polys)
        }
        4 => {
            let n = get_n(b, pos)?;
            Geom::Multi((0..n).map(|_| get_geom(b, pos, prev)).collect::<Result<Vec<_>, _>>()?)
        }
        _ => return Err("corrupt boundary".into()),
    })
}

/// What the index says about one boundary (without its area).
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: i64,
    pub version: u32,
    pub level: u32,
    pub ebbox: Option<BBoxF>,
    /// the box of the area (none if it is empty)
    pub gbbox: Option<BBoxF>,
    pub name: Option<Str>,
    pub iso: Option<Str>,
    off: u64,
    len: u32,
    raw: u32,
}

/// The entries of the index of a database, read once per process.
pub struct Index {
    pub entries: Vec<Entry>,
    areas: u64,
}

/// The index of the database, if it has one (read once and kept, like the routing network).
pub fn index(db: &Db) -> Option<Arc<Index>> {
    static CACHE: std::sync::Mutex<Vec<(std::path::PathBuf, u64, Option<Arc<Index>>)>> = std::sync::Mutex::new(Vec::new());
    let el = db.reader.element(ElementKind::AdminBoundaries)?;
    let s = el.sections.first()?;
    let mut c = CACHE.lock().ok()?;
    if let Some(x) = c.iter().find(|x| x.0 == db.path && x.1 == s.checksum) {
        return x.2.clone();
    }
    let ix = db.reader.section_bytes(s).ok().and_then(|b| read(b).ok()).map(Arc::new);
    c.push((db.path.clone(), s.checksum, ix.clone()));
    ix
}

fn read(b: &[u8]) -> Result<Index, String> {
    let e = |x: knut_format::Error| x.to_string();
    if b.len() < HEADER || &b[..8] != MAGIC {
        return Err("not an index of administrative boundaries".into());
    }
    let n = le::u32(b, 8).map_err(e)? as usize;
    let s0 = le::u64(b, 16).map_err(e)? as usize;
    let a0 = le::u64(b, 24).map_err(e)?;
    let text = |o: u32, l: u32| -> Result<Option<Str>, String> {
        if l == u32::MAX {
            return Ok(None);
        }
        let bytes = le::slice(b, s0 + o as usize, l as usize).map_err(e)?;
        Ok(Some(Str::from(std::str::from_utf8(bytes).map_err(|x| x.to_string())?)))
    };
    let mut entries = Vec::with_capacity(n);
    for i in 0..n {
        let o = HEADER + i * ENTRY;
        let bbox_at = |at: usize| -> Result<Option<BBoxF>, String> {
            let bx: Vec<i32> = (0..4).map(|k| le::i32(b, at + 4 * k)).collect::<Result<_, _>>().map_err(e)?;
            Ok((bx[0] != i32::MIN).then(|| BBoxF::from_e7(&knut_format::BBox { minx: bx[0], miny: bx[1], maxx: bx[2], maxy: bx[3] })))
        };
        entries.push(Entry {
            id: le::i64(b, o).map_err(e)?,
            version: le::u32(b, o + 8).map_err(e)?,
            level: le::u32(b, o + 12).map_err(e)?,
            ebbox: bbox_at(o + 16)?,
            gbbox: bbox_at(o + 32)?,
            name: text(le::u32(b, o + 48).map_err(e)?, le::u32(b, o + 52).map_err(e)?)?,
            iso: text(le::u32(b, o + 56).map_err(e)?, le::u32(b, o + 60).map_err(e)?)?,
            off: le::u64(b, o + 64).map_err(e)?,
            len: le::u32(b, o + 72).map_err(e)?,
            raw: le::u32(b, o + 76).map_err(e)?,
        });
    }
    Ok(Index { entries, areas: a0 })
}

impl Index {
    /// The entry of a relation.
    pub fn get(&self, id: i64) -> Option<&Entry> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok().map(|i| &self.entries[i])
    }

    /// The area of a boundary, as `ctx.elem_geom` assembles it.
    pub fn geom(&self, db: &Db, e: &Entry) -> Option<Geom> {
        let el = db.reader.element(ElementKind::AdminBoundaries)?;
        let b = db.reader.section_bytes(el.sections.first()?).ok()?;
        let packed = le::slice(b, (self.areas + e.off) as usize, e.len as usize).ok()?;
        let raw = zstd::bulk::decompress(packed, e.raw as usize).ok()?;
        get_geom(&raw, &mut 0, &mut (0, 0)).ok()
    }
}

/// The area of a boundary relation from the index, if it holds this version of it (the
/// element must be as at the last moment of the data).
pub fn area_geom(db: &Db, e: &Elem) -> Option<Arc<Geom>> {
    if e.otype != Some(ObjType::Relation) || e.at < db.latest {
        return None;
    }
    let ix = index(db)?;
    let en = ix.get(e.id).filter(|x| x.version == e.v.version)?;
    ix.geom(db, en).map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Areas of every shape come back from the section exactly as they went in, with their
    /// names, codes and boxes.
    #[test]
    fn boundaries_come_back_exactly() {
        let p = |x: i32, y: i32| geom::e7((x, y));
        let ring = |x0: i32, y0: i32, d: i32| vec![p(x0, y0), p(x0 + d, y0), p(x0 + d, y0 + d), p(x0, y0 + d), p(x0, y0)];
        let geoms = [
            Geom::Empty,
            Geom::Poly(vec![Polygon { outer: ring(87_000_000, 530_000_000, 1_000_000), holes: vec![ring(87_200_000, 530_200_000, 100_000)] }]),
            Geom::Multi(vec![
                Geom::Poly(vec![Polygon { outer: ring(-1_234_567, -89_000_001, 7), holes: vec![] }]),
                Geom::Line(vec![p(1, 2), p(-3, 4)]),
                Geom::Point(p(1_799_999_999, 899_999_999)),
            ]),
        ];
        let bs: Vec<Boundary> = geoms
            .into_iter()
            .enumerate()
            .map(|(i, g)| Boundary {
                id: 100 - i as i64,
                version: 3 + i as u32,
                level: [2, 4, 8][i],
                ebbox: (i > 0).then(|| g.bbox()),
                name: (i != 1).then(|| format!("Gebiet {i} – Ä")),
                iso: (i == 0).then(|| "DE".to_string()),
                geom: g,
            })
            .collect();
        assert!(bs.iter().all(|b| exact(&b.geom)));
        let bytes = encode(&bs);
        let ix = read(&bytes).unwrap();
        assert_eq!(ix.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [98, 99, 100]);
        for b in &bs {
            let e = ix.get(b.id).unwrap();
            assert_eq!((e.version, e.level), (b.version, b.level));
            assert_eq!(e.name.as_deref(), b.name.as_deref());
            assert_eq!(e.iso.as_deref(), b.iso.as_deref());
            assert_eq!(e.gbbox, Some(b.geom.bbox()).filter(|x| !x.is_empty()));
            let a0 = ix.areas as usize + e.off as usize;
            let raw = zstd::bulk::decompress(&bytes[a0..a0 + e.len as usize], e.raw as usize).unwrap();
            assert_eq!(format!("{:?}", get_geom(&raw, &mut 0, &mut (0, 0)).unwrap()), format!("{:?}", b.geom));
        }
    }
}
