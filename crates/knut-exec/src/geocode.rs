//! Name index and address index (container elements): search by name, geocoding of
//! addresses and reverse geocoding of points, at the last moment of the data.
//!
//! Both read their section in place (binary searches over sorted tables), so that a query
//! needs no more than a few page reads.

use crate::db::Db;
use crate::scan::{self, Item, KindMask, ScanSpec, TimeMode};
use crate::value::*;
use knut_format::ObjType;
use knut_plan::pushdown::TagPred;
use rustc_hash::FxHashMap as HashMap;

// ----- normalization ---------------------------------------------------------------------

/// Lowercase, without diacritics (ä → a, ß → ss), so that spellings compare equal.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'æ' => out.push_str("ae"),
            'ç' | 'ć' | 'č' => out.push('c'),
            'ď' | 'đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => out.push('i'),
            'ł' | 'ľ' | 'ĺ' => out.push('l'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
            'œ' => out.push_str("oe"),
            'ŕ' | 'ř' => out.push('r'),
            'ś' | 'š' | 'ş' | 'ș' => out.push('s'),
            'ß' => out.push_str("ss"),
            'ť' | 'ţ' | 'ț' => out.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'ź' | 'ż' | 'ž' => out.push('z'),
            'þ' => out.push_str("th"),
            c => out.push(c),
        }
    }
    out
}

/// Words of a normalized text; `straße`, `strasse` and `str.` all become `strasse`, also at
/// the end of a word (`Hauptstraße` → `haupt`, `strasse`).
pub fn tokens(s: &str) -> Vec<String> {
    let n = normalize(s);
    let mut out = Vec::new();
    for w in n.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        if w == "str" {
            out.push("strasse".to_string());
        } else if let Some(stem) = w.strip_suffix("strasse").filter(|s| s.len() >= 3) {
            out.push(stem.to_string());
            out.push("strasse".to_string());
        } else if let Some(stem) = w.strip_suffix("str").filter(|s| s.len() >= 4) {
            out.push(stem.to_string());
            out.push("strasse".to_string());
        } else {
            out.push(w.to_string());
        }
    }
    out
}

fn edit_distance(a: &[u8], b: &[u8], max: usize) -> usize {
    if a.len().abs_diff(b.len()) > max {
        return max + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let c = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + c);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return max + 1;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ----- small binary helpers --------------------------------------------------------------

struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn pos(&self) -> u32 {
        self.0.len() as u32
    }
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_i64(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn rd_varint(b: &[u8], o: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut s = 0;
    loop {
        let x = b[*o];
        *o += 1;
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return v;
        }
        s += 7;
    }
}

/// A table of strings: u32 n, (n + 1) × u32 offsets, bytes. Strings are addressed by number.
struct StrTable<'a> {
    b: &'a [u8],
    n: usize,
    offs: usize,
    data: usize,
}

impl<'a> StrTable<'a> {
    fn write(out: &mut Out, strs: &[&str]) {
        out.u32(strs.len() as u32);
        let mut o = 0u32;
        for s in strs {
            out.u32(o);
            o += s.len() as u32;
        }
        out.u32(o);
        for s in strs {
            out.0.extend_from_slice(s.as_bytes());
        }
    }

    fn read(b: &'a [u8], at: usize) -> (StrTable<'a>, usize) {
        let n = rd_u32(b, at) as usize;
        let offs = at + 4;
        let data = offs + 4 * (n + 1);
        let end = data + rd_u32(b, offs + 4 * n) as usize;
        (StrTable { b, n, offs, data }, end)
    }

    fn get(&self, i: usize) -> &'a str {
        let (a, z) = (rd_u32(self.b, self.offs + 4 * i) as usize, rd_u32(self.b, self.offs + 4 * i + 4) as usize);
        std::str::from_utf8(&self.b[self.data + a..self.data + z]).unwrap_or("")
    }

    /// First index whose string is ≥ `key` (the table is sorted).
    fn lower_bound(&self, key: &str) -> usize {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let m = (lo + hi) / 2;
            if self.get(m) < key { lo = m + 1 } else { hi = m }
        }
        lo
    }
}

fn point_of(e: &Elem) -> Option<(i32, i32)> {
    if let Some(p) = e.v.pos {
        return Some(p);
    }
    let b = e.bbox?;
    if b.is_empty() {
        return None;
    }
    let c = b.center();
    Some(((c.0 * 1e7).round() as i32, (c.1 * 1e7).round() as i32))
}

/// The tag that says what an object is (`amenity=school`), for display and ranking.
fn class_of(e: &Elem) -> String {
    const KEYS: &[&str] = &[
        "place", "boundary", "amenity", "shop", "tourism", "leisure", "railway", "public_transport", "aeroway", "highway", "waterway",
        "natural", "historic", "office", "craft", "healthcare", "sport", "man_made", "building", "landuse",
    ];
    for k in KEYS {
        if let Some(v) = e.tag(k) {
            return format!("{k}={v}");
        }
    }
    String::new()
}

fn state_spec(db: &Db, pred: TagPred) -> ScanSpec {
    ScanSpec {
        bbox: None,
        kinds: KindMask { node: true, way: true, relation: true, ..Default::default() },
        time: TimeMode::State(db.latest),
        region: None,
        pred,
        need_geom: false,
        need_hist: false,
        attrs_history: false,
    }
}

// ----- name index ------------------------------------------------------------------------

pub struct NameEntry {
    pub otype: ObjType,
    pub id: i64,
    pub point: (i32, i32),
    pub name: String,
    pub class: String,
}

/// All objects with a name at the last moment of the data.
pub fn collect_names(db: &Db, ctl: Option<&crate::control::Control>) -> R<Vec<NameEntry>> {
    let spec = state_spec(db, TagPred::Has("name".into()));
    let parts = scan::fold(db, ctl, &spec, &Vec::new, &|acc: &mut Vec<NameEntry>, it: Item| {
        let Item::Elem(e) = it else { return Ok(()) };
        let (Some(n), Some(p), Some(t)) = (e.tag("name"), point_of(&e), e.otype) else { return Ok(()) };
        acc.push(NameEntry { otype: t, id: e.id, point: p, name: n.to_string(), class: class_of(&e) });
        Ok(())
    })?;
    let mut v: Vec<NameEntry> = parts.into_iter().flatten().collect();
    v.sort_by_key(|x| (x.otype, x.id));
    v.dedup_by_key(|x| (x.otype, x.id));
    Ok(v)
}

const NAMES_MAGIC: &[u8; 8] = b"KNAMES01";

/// magic; u32 entries; entries (24 B: u8 type, u8 0, u16 class, i32 lon, i32 lat, i64 id,
/// u32 name); class strings; name strings; token strings (sorted); (tokens + 1) × u32
/// posting offsets; postings (per token: entry numbers, ascending, delta varints).
pub fn encode_names(entries: &[NameEntry]) -> Vec<u8> {
    let mut classes: Vec<&str> = entries.iter().map(|e| e.class.as_str()).collect();
    classes.sort_unstable();
    classes.dedup();
    let class_id: HashMap<&str, u16> = classes.iter().enumerate().map(|(i, c)| (*c, i.min(u16::MAX as usize) as u16)).collect();
    let mut postings: HashMap<String, Vec<u32>> = HashMap::default();
    for (i, e) in entries.iter().enumerate() {
        let mut ts = tokens(&e.name);
        ts.sort_unstable();
        ts.dedup();
        for t in ts {
            postings.entry(t).or_default().push(i as u32);
        }
    }
    let mut toks: Vec<(String, Vec<u32>)> = postings.into_iter().collect();
    toks.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut o = Out(Vec::new());
    o.0.extend_from_slice(NAMES_MAGIC);
    o.u32(entries.len() as u32);
    for (i, e) in entries.iter().enumerate() {
        o.u8(e.otype as u8);
        o.u8(0);
        o.u16(class_id[e.class.as_str()]);
        o.i32(e.point.0);
        o.i32(e.point.1);
        o.i64(e.id);
        o.u32(i as u32);
    }
    StrTable::write(&mut o, &classes);
    StrTable::write(&mut o, &entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>());
    StrTable::write(&mut o, &toks.iter().map(|t| t.0.as_str()).collect::<Vec<_>>());
    let mut post = Out(Vec::new());
    let mut offs = Vec::with_capacity(toks.len() + 1);
    for (_, ids) in &toks {
        offs.push(post.pos());
        let mut last = 0u32;
        for &i in ids {
            post.varint((i - last) as u64);
            last = i;
        }
    }
    offs.push(post.pos());
    for x in offs {
        o.u32(x);
    }
    o.0.extend_from_slice(&post.0);
    o.0
}

pub struct NameIx<'a> {
    b: &'a [u8],
    n: usize,
    entries: usize,
    classes: StrTable<'a>,
    names: StrTable<'a>,
    toks: StrTable<'a>,
    post_offs: usize,
    post: usize,
}

/// A hit of a name search.
pub struct NameHit {
    pub otype: ObjType,
    pub id: i64,
    pub lon: f64,
    pub lat: f64,
    pub name: String,
    pub class: String,
    pub score: f64,
}

impl<'a> NameIx<'a> {
    pub fn new(b: &'a [u8]) -> Result<NameIx<'a>, String> {
        if b.get(0..8) != Some(NAMES_MAGIC) {
            return Err("not a name index".into());
        }
        let n = rd_u32(b, 8) as usize;
        let entries = 12;
        let (classes, e1) = StrTable::read(b, entries + 24 * n);
        let (names, e2) = StrTable::read(b, e1);
        let (toks, e3) = StrTable::read(b, e2);
        let post_offs = e3;
        let post = post_offs + 4 * (toks.n + 1);
        Ok(NameIx { b, n, entries, classes, names, toks, post_offs, post })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn postings(&self, t: usize) -> Vec<u32> {
        let (a, z) = (rd_u32(self.b, self.post_offs + 4 * t) as usize, rd_u32(self.b, self.post_offs + 4 * t + 4) as usize);
        let mut o = self.post + a;
        let mut v = Vec::new();
        let mut last = 0u32;
        while o < self.post + z {
            last += rd_varint(self.b, &mut o) as u32;
            v.push(last);
        }
        v
    }

    /// Token numbers matching a query word: exactly, as a prefix (for the last word), or
    /// with one or two typos; with the cost of the match.
    fn matching(&self, w: &str, prefix: bool) -> Vec<(usize, f64)> {
        let mut out = Vec::new();
        let i = self.toks.lower_bound(w);
        if i < self.toks.n && self.toks.get(i) == w {
            out.push((i, 0.0));
        }
        if prefix {
            let mut j = i;
            while j < self.toks.n && self.toks.get(j).starts_with(w) && out.len() < 200 {
                if self.toks.get(j) != w {
                    out.push((j, 0.3));
                }
                j += 1;
            }
        }
        if out.is_empty() && w.chars().count() >= 4 {
            let max = if w.len() >= 8 { 2 } else { 1 };
            let first = &w[..w.chars().next().map_or(0, |c| c.len_utf8())];
            let (mut j, end) = (self.toks.lower_bound(first), self.toks.lower_bound(&format!("{first}\u{10FFFF}")));
            while j < end {
                let t = self.toks.get(j);
                let d = edit_distance(w.as_bytes(), t.as_bytes(), max);
                if d <= max {
                    out.push((j, 0.6 * d as f64));
                }
                j += 1;
            }
        }
        out
    }

    fn hit(&self, i: usize, score: f64) -> NameHit {
        let o = self.entries + 24 * i;
        NameHit {
            otype: ObjType::from_u8(self.b[o]).unwrap_or(ObjType::Node),
            id: rd_i64(self.b, o + 12),
            lon: rd_i32(self.b, o + 4) as f64 * 1e-7,
            lat: rd_i32(self.b, o + 8) as f64 * 1e-7,
            name: self.names.get(rd_u32(self.b, o + 20) as usize).to_string(),
            class: self.classes.get(rd_u16(self.b, o + 2) as usize).to_string(),
            score,
        }
    }

    /// The best `limit` objects whose name contains all words of `query` (the last one
    /// also as a prefix, all with small typos).
    pub fn search(&self, query: &str, limit: usize) -> Vec<NameHit> {
        let ws = tokens(query);
        if ws.is_empty() {
            return Vec::new();
        }
        // per entry: words matched and cost
        let mut acc: HashMap<u32, (usize, f64)> = HashMap::default();
        for (k, w) in ws.iter().enumerate() {
            let mut best: HashMap<u32, f64> = HashMap::default();
            for (t, cost) in self.matching(w, k + 1 == ws.len()) {
                for e in self.postings(t) {
                    let c = best.entry(e).or_insert(f64::INFINITY);
                    *c = c.min(cost);
                }
            }
            for (e, c) in best {
                let a = acc.entry(e).or_insert((0, 0.0));
                a.0 += 1;
                a.1 += c;
            }
        }
        let need = ws.len();
        let mut hits: Vec<(u32, f64)> = acc
            .into_iter()
            .filter(|(_, (n, _))| *n == need)
            .map(|(e, (_, c))| {
                // exact names first, then places, then by the length of the name
                let h = self.hit(e as usize, 0.0);
                let exact = if normalize(&h.name) == normalize(query) { -1.0 } else { 0.0 };
                (e, c + exact - importance(&h.class) + h.name.len() as f64 * 0.001)
            })
            .collect();
        hits.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        hits.into_iter().take(limit).map(|(e, s)| self.hit(e as usize, s)).collect()
    }
}

/// How much an object of this class is what people search for by name.
fn importance(class: &str) -> f64 {
    let key = class.split('=').next().unwrap_or("");
    let value = class.split('=').nth(1).unwrap_or("");
    match key {
        "place" => match value {
            "city" | "country" | "state" => 0.9,
            "town" => 0.8,
            _ => 0.6,
        },
        "boundary" => 0.5,
        "tourism" | "historic" => 0.45,
        "railway" if value == "station" => 0.45,
        "railway" | "public_transport" | "aeroway" => 0.3,
        "amenity" | "leisure" | "shop" | "office" | "healthcare" | "craft" | "sport" => 0.25,
        "natural" | "waterway" => 0.2,
        "highway" => match value {
            "bus_stop" | "platform" | "track" | "path" | "footway" | "service" => 0.0,
            _ => 0.1,
        },
        _ => 0.05,
    }
}

// ----- address index ---------------------------------------------------------------------

pub struct AddrEntry {
    pub point: (i32, i32),
    pub street: String,
    pub number: String,
    pub postcode: String,
    pub city: String,
    pub otype: ObjType,
    pub id: i64,
}

/// All objects with a house number at the last moment of the data.
pub fn collect_addresses(db: &Db, ctl: Option<&crate::control::Control>) -> R<Vec<AddrEntry>> {
    let spec = state_spec(db, TagPred::Has("addr:housenumber".into()));
    let parts = scan::fold(db, ctl, &spec, &Vec::new, &|acc: &mut Vec<AddrEntry>, it: Item| {
        let Item::Elem(e) = it else { return Ok(()) };
        let (Some(n), Some(p), Some(t)) = (e.tag("addr:housenumber"), point_of(&e), e.otype) else { return Ok(()) };
        let street = e.tag("addr:street").or_else(|| e.tag("addr:place")).map(|s| s.to_string()).unwrap_or_default();
        acc.push(AddrEntry {
            point: p,
            street,
            number: n.to_string(),
            postcode: e.tag("addr:postcode").map(|s| s.to_string()).unwrap_or_default(),
            city: e.tag("addr:city").map(|s| s.to_string()).unwrap_or_default(),
            otype: t,
            id: e.id,
        });
        Ok(())
    })?;
    let mut v: Vec<AddrEntry> = parts.into_iter().flatten().collect();
    v.sort_by_key(|x| (x.otype, x.id));
    v.dedup_by_key(|x| (x.otype, x.id));
    Ok(v)
}

const ADDR_MAGIC: &[u8; 8] = b"KADDRS01";
/// Cells for reverse geocoding: 1/256 degree.
const CELL: i64 = 10_000_000 / 256;

fn cell_key(lon: i32, lat: i32) -> u64 {
    let x = ((lon as i64 + 1_800_000_000) / CELL) as u64;
    let y = ((lat as i64 + 900_000_000) / CELL) as u64;
    (y << 32) | x
}

/// magic; strings (streets, numbers, postcodes, cities); normalized street of each group
/// (sorted); u32 groups; u32 houses; groups (16 B: u32 street, u32 postcode, u32 city, u32
/// first entry in the group list); group list (houses × u32, the houses of each group);
/// houses (24 B: i32 lon, i32 lat, u32 number, u32 group, i64 id with the type in the top
/// 2 bits) sorted by cell.
pub fn encode_addresses(entries: &[AddrEntry]) -> Vec<u8> {
    let mut strs: Vec<&str> = Vec::new();
    for e in entries {
        strs.extend([e.street.as_str(), e.number.as_str(), e.postcode.as_str(), e.city.as_str()]);
    }
    strs.sort_unstable();
    strs.dedup();
    let sid: HashMap<&str, u32> = strs.iter().enumerate().map(|(i, s)| (*s, i as u32)).collect();
    // groups: (street, postcode, city)
    let mut gkeys: Vec<(String, u32, u32, u32)> = entries
        .iter()
        .map(|e| (tokens(&e.street).join(" "), sid[e.street.as_str()], sid[e.postcode.as_str()], sid[e.city.as_str()]))
        .collect();
    gkeys.sort_unstable();
    gkeys.dedup();
    let gid: HashMap<(u32, u32, u32), u32> = gkeys.iter().enumerate().map(|(i, g)| ((g.1, g.2, g.3), i as u32)).collect();
    let mut houses: Vec<(u64, usize)> = entries.iter().enumerate().map(|(i, e)| (cell_key(e.point.0, e.point.1), i)).collect();
    houses.sort_unstable();
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); gkeys.len()];
    for (h, (_, i)) in houses.iter().enumerate() {
        let e = &entries[*i];
        members[gid[&(sid[e.street.as_str()], sid[e.postcode.as_str()], sid[e.city.as_str()])] as usize].push(h as u32);
    }
    let mut o = Out(Vec::new());
    o.0.extend_from_slice(ADDR_MAGIC);
    StrTable::write(&mut o, &strs);
    // normalized street names of the groups, for the binary search
    StrTable::write(&mut o, &gkeys.iter().map(|g| g.0.as_str()).collect::<Vec<_>>());
    o.u32(gkeys.len() as u32);
    o.u32(houses.len() as u32);
    let mut first = 0u32;
    for (g, m) in gkeys.iter().zip(&members) {
        o.u32(g.1);
        o.u32(g.2);
        o.u32(g.3);
        o.u32(first);
        first += m.len() as u32;
    }
    for m in &members {
        for h in m {
            o.u32(*h);
        }
    }
    for (_, i) in &houses {
        let e = &entries[*i];
        o.i32(e.point.0);
        o.i32(e.point.1);
        o.u32(sid[e.number.as_str()]);
        o.u32(gid[&(sid[e.street.as_str()], sid[e.postcode.as_str()], sid[e.city.as_str()])]);
        o.i64((e.id & 0x3fff_ffff_ffff_ffff) | ((e.otype as i64) << 62));
    }
    o.0
}

pub struct AddrIx<'a> {
    b: &'a [u8],
    strs: StrTable<'a>,
    gnames: StrTable<'a>,
    n_groups: usize,
    groups: usize,
    members: usize,
    n_houses: usize,
    houses: usize,
}

/// An address found by geocoding or reverse geocoding.
pub struct Address {
    pub otype: ObjType,
    pub id: i64,
    pub lon: f64,
    pub lat: f64,
    pub street: String,
    pub number: String,
    pub postcode: String,
    pub city: String,
    /// Metres from the point (reverse geocoding), or 0.
    pub distance: f64,
}

impl Address {
    pub fn text(&self) -> String {
        let mut s = format!("{} {}", self.street, self.number).trim().to_string();
        let place = format!("{} {}", self.postcode, self.city).trim().to_string();
        if !place.is_empty() {
            s = format!("{s}, {place}");
        }
        s
    }
}

impl<'a> AddrIx<'a> {
    pub fn new(b: &'a [u8]) -> Result<AddrIx<'a>, String> {
        if b.get(0..8) != Some(ADDR_MAGIC) {
            return Err("not an address index".into());
        }
        let (strs, e1) = StrTable::read(b, 8);
        let (gnames, e2) = StrTable::read(b, e1);
        let n_groups = rd_u32(b, e2) as usize;
        let n_houses = rd_u32(b, e2 + 4) as usize;
        let groups = e2 + 8;
        let members = groups + 16 * n_groups;
        let houses = members + 4 * n_houses;
        if houses + 24 * n_houses > b.len() {
            return Err("address index truncated".into());
        }
        Ok(AddrIx { b, strs, gnames, n_groups, groups, members, n_houses, houses })
    }

    fn group(&self, g: usize) -> (usize, usize, usize, usize, usize) {
        let o = self.groups + 16 * g;
        let first = rd_u32(self.b, o + 12) as usize;
        let end = if g + 1 < self.n_groups { rd_u32(self.b, o + 28) as usize } else { self.n_houses };
        (rd_u32(self.b, o) as usize, rd_u32(self.b, o + 4) as usize, rd_u32(self.b, o + 8) as usize, first, end)
    }

    fn house(&self, h: usize, distance: f64) -> Address {
        let o = self.houses + 24 * h;
        let (lon, lat) = (rd_i32(self.b, o), rd_i32(self.b, o + 4));
        let (street, postcode, city, _, _) = self.group(rd_u32(self.b, o + 12) as usize);
        let raw = rd_i64(self.b, o + 16);
        Address {
            otype: ObjType::from_u8(((raw as u64) >> 62) as u8).unwrap_or(ObjType::Node),
            id: raw & 0x3fff_ffff_ffff_ffff,
            lon: lon as f64 * 1e-7,
            lat: lat as f64 * 1e-7,
            street: self.strs.get(street).to_string(),
            number: self.strs.get(rd_u32(self.b, o + 8) as usize).to_string(),
            postcode: self.strs.get(postcode).to_string(),
            city: self.strs.get(city).to_string(),
            distance,
        }
    }

    /// Addresses for a query like `Am Wall 201, Bremen` or `Hauptstraße 5 28195`: the
    /// street, a house number and optionally postcode or city. Best first.
    pub fn geocode(&self, query: &str, limit: usize) -> Vec<Address> {
        let mut parts = query.split(',').map(str::trim);
        let first = parts.next().unwrap_or("");
        let rest: Vec<String> = parts.flat_map(tokens).collect();
        // the house number: the first word after the street that starts with a digit;
        // a postcode or city may follow it without a comma
        let words: Vec<&str> = first.split_whitespace().collect();
        let mut extra: Vec<String> = rest;
        let (number, street_words): (Option<&str>, Vec<&str>) =
            match words.iter().enumerate().skip(1).find(|(_, w)| w.chars().next().is_some_and(|c| c.is_ascii_digit())) {
                Some((i, w)) => {
                    extra.extend(words[i + 1..].iter().flat_map(|w| tokens(w)));
                    (Some(*w), words[..i].to_vec())
                }
                None => (None, words.clone()),
            };
        let street_key = tokens(&street_words.join(" ")).join(" ");
        if street_key.is_empty() {
            return Vec::new();
        }
        let num = number.map(normalize);
        let mut out: Vec<(f64, Address)> = Vec::new();
        let mut g = self.gnames.lower_bound(&street_key);
        while g < self.n_groups && self.gnames.get(g) == street_key {
            let (_, postcode, city, a, z) = self.group(g);
            let place = format!("{} {}", normalize(self.strs.get(postcode)), tokens(self.strs.get(city)).join(" "));
            let place_ok = extra.iter().all(|w| place.split_whitespace().any(|p| p == w || p.starts_with(w.as_str())));
            if place_ok {
                for k in a..z {
                    let h = rd_u32(self.b, self.members + 4 * k) as usize;
                    let ad = self.house(h, 0.0);
                    let score = match &num {
                        Some(n) if normalize(&ad.number) == *n => 0.0,
                        Some(n) if normalize(&ad.number).starts_with(n.as_str()) => 0.5,
                        Some(_) => continue,
                        None => 1.0,
                    };
                    out.push((score, ad));
                }
            }
            g += 1;
        }
        // with the exact house number found, others only where it is missing
        if out.iter().any(|x| x.0 == 0.0) {
            out.retain(|x| x.0 == 0.0);
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.number.cmp(&b.1.number)).then(a.1.id.cmp(&b.1.id)));
        out.into_iter().take(limit).map(|x| x.1).collect()
    }

    /// The nearest addresses to a point, within `radius` metres.
    pub fn reverse(&self, lon: f64, lat: f64, radius: f64, limit: usize) -> Vec<Address> {
        let (x, y) = ((lon * 1e7).round() as i32, (lat * 1e7).round() as i32);
        let dlat = (radius / 111_195.0 * 1e7) as i64 / CELL + 1;
        let dlon = (radius / (111_195.0 * lat.to_radians().cos().max(0.01)) * 1e7) as i64 / CELL + 1;
        let mut found: Vec<(f64, usize)> = Vec::new();
        let key = cell_key(x, y);
        let (cx, cy) = ((key & 0xffff_ffff) as i64, (key >> 32) as i64);
        for yy in cy - dlat..=cy + dlat {
            // keys are sorted by (y, x): one range per row
            let lo = ((yy as u64) << 32) | (cx - dlon).max(0) as u64;
            let hi = ((yy as u64) << 32) | (cx + dlon).max(0) as u64;
            let mut h = self.house_lower_bound(lo);
            while h < self.n_houses {
                let o = self.houses + 24 * h;
                let (hx, hy) = (rd_i32(self.b, o), rd_i32(self.b, o + 4));
                if cell_key(hx, hy) > hi {
                    break;
                }
                let d = crate::geom::haversine((lon, lat), (hx as f64 * 1e-7, hy as f64 * 1e-7));
                if d <= radius {
                    found.push((d, h));
                }
                h += 1;
            }
        }
        found.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        found.into_iter().take(limit).map(|(d, h)| self.house(h, d)).collect()
    }

    fn house_lower_bound(&self, key: u64) -> usize {
        let (mut lo, mut hi) = (0, self.n_houses);
        while lo < hi {
            let m = (lo + hi) / 2;
            let o = self.houses + 24 * m;
            if cell_key(rd_i32(self.b, o), rd_i32(self.b, o + 4)) < key { lo = m + 1 } else { hi = m }
        }
        lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_compare_equal() {
        assert_eq!(tokens("Hauptstraße 5"), vec!["haupt", "strasse", "5"]);
        assert_eq!(tokens("Haupt-Str."), vec!["haupt", "strasse"]);
        assert_eq!(tokens("Hauptstr"), vec!["haupt", "strasse"]);
        assert_eq!(normalize("Ærø Zürich"), "aero zurich");
        assert_eq!(edit_distance(b"bremen", b"bremne", 2), 2);
    }

    #[test]
    fn names_and_addresses_are_found() {
        let p = |x: f64, y: f64| ((x * 1e7) as i32, (y * 1e7) as i32);
        let names = vec![
            NameEntry { otype: ObjType::Node, id: 1, point: p(8.8, 53.07), name: "Bremen Hauptbahnhof".into(), class: "railway=station".into() },
            NameEntry { otype: ObjType::Way, id: 2, point: p(8.81, 53.07), name: "Hauptstraße".into(), class: "highway=residential".into() },
            NameEntry { otype: ObjType::Relation, id: 3, point: p(8.8, 53.08), name: "Bremen".into(), class: "place=city".into() },
        ];
        let b = encode_names(&names);
        let ix = NameIx::new(&b).unwrap();
        let ids = |q: &str| ix.search(q, 10).iter().map(|h| h.id).collect::<Vec<_>>();
        assert_eq!(ids("bremen"), vec![3, 1]);
        assert_eq!(ids("Hauptbahnhof Bremen"), vec![1]);
        assert_eq!(ids("Bremn Hauptbahnhof"), vec![1]);
        assert_eq!(ids("hauptstr"), vec![2]);
        assert_eq!(ids("haupt"), vec![1, 2]);
        let a = |id: i64, x: f64, s: &str, n: &str, pc: &str, c: &str| AddrEntry {
            point: p(x, 53.07),
            street: s.into(),
            number: n.into(),
            postcode: pc.into(),
            city: c.into(),
            otype: ObjType::Way,
            id,
        };
        let addrs = vec![
            a(10, 8.8000, "Am Wall", "201", "28195", "Bremen"),
            a(11, 8.8010, "Am Wall", "203", "28195", "Bremen"),
            a(12, 8.9000, "Am Wall", "201", "27711", "Osterholz"),
            a(13, 8.8020, "Hauptstraße", "5", "28195", "Bremen"),
        ];
        let b = encode_addresses(&addrs);
        let ix = AddrIx::new(&b).unwrap();
        let g = |q: &str| ix.geocode(q, 10).iter().map(|x| x.id).collect::<Vec<_>>();
        assert_eq!(g("Am Wall 201, Bremen"), vec![10]);
        assert_eq!(g("am wall 201"), vec![10, 12]);
        assert_eq!(g("Am Wall 201 28195"), vec![10]);
        assert_eq!(g("Hauptstr. 5"), vec![13]);
        assert_eq!(ix.geocode("Hauptstraße 5", 1)[0].text(), "Hauptstraße 5, 28195 Bremen");
        let r = ix.reverse(8.80095, 53.07, 200.0, 2);
        assert_eq!(r.iter().map(|x| x.id).collect::<Vec<_>>(), vec![11, 10]);
        assert!(ix.reverse(8.5, 53.07, 200.0, 2).is_empty());
    }
}
