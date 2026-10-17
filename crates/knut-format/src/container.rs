//! The container: a `.knutdb` file holds elements, the parts of the database (nodes, the
//! spatial index, …) and optional indexes added later (place index, tag statistics, …).
//! The directory lists the elements with their sections; it is written at the end of the
//! file, and the header points to the newest directory (SPEC.md §4).

use crate::codec::le;
use crate::error::corrupt;
use crate::layout::SectionEntry;
use crate::{Error, Result};
use xxhash_rust::xxh3::xxh3_64;

pub const DIR_MAGIC: [u8; 8] = *b"KNUTDIR\0";

/// Element kinds, in the order in which they are listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum ElementKind {
    Metadata = 1,
    Dictionaries = 2,
    Users = 3,
    CompressionDictionaries = 4,
    Nodes = 5,
    Ways = 6,
    Relations = 7,
    LargeObjects = 8,
    BlockTable = 9,
    SpatialIndex = 10,
    TagFilters = 11,
    IdIndex = 12,
    PlaceIndex = 100,
    NameIndex = 101,
    AddressIndex = 102,
    AdminBoundaries = 103,
    TagStatistics = 110,
    Changesets = 120,
    RoutingNetwork = 130,
    RoutingCh = 131,
    RoutingCch = 132,
}

/// What the overview says about an element kind.
pub struct KindInfo {
    pub kind: ElementKind,
    pub name: &'static str,
    /// The sort of element ("data", "search · spatial", …).
    pub sort: &'static str,
    /// Part of every database; cannot be removed.
    pub base: bool,
    /// Can be added with `knut manage … add`.
    pub addable: bool,
    /// Command name for `knut manage … add`.
    pub command: &'static str,
    pub about: &'static str,
}

impl KindInfo {
    /// The name inside a sentence: the first letter small, unless the first word is an
    /// abbreviation ("routing · CH", "ID index").
    pub fn name_in_text(&self) -> String {
        let first = self.name.split_whitespace().next().unwrap_or("");
        if first.chars().skip(1).any(|c| c.is_lowercase()) {
            let mut c = self.name.chars();
            c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
        } else {
            self.name.to_string()
        }
    }
}

pub const KINDS: &[KindInfo] = &[
    KindInfo { kind: ElementKind::Metadata, name: "Metadata", sort: "data", base: true, addable: false, command: "",
        about: "Build parameters, source, licence and the counts shown by knut info." },
    KindInfo { kind: ElementKind::Dictionaries, name: "Dictionaries", sort: "strings", base: true, addable: false, command: "",
        about: "Tag keys, frequent tag values and member roles; elements refer to them by number." },
    KindInfo { kind: ElementKind::Users, name: "User names", sort: "strings", base: true, addable: false, command: "",
        about: "The names of the users who edited the data (absent if the database was built with --strip-users)." },
    KindInfo { kind: ElementKind::CompressionDictionaries, name: "Compression dictionaries", sort: "zstd", base: true, addable: false, command: "",
        about: "Trained zstd dictionaries that make the small data blocks compress well." },
    KindInfo { kind: ElementKind::Nodes, name: "Nodes", sort: "data", base: true, addable: false, command: "",
        about: "All versions of all nodes, in blocks sorted by place and time." },
    KindInfo { kind: ElementKind::Ways, name: "Ways", sort: "data", base: true, addable: false, command: "",
        about: "All versions of all ways, with the positions of the nodes that belong only to them." },
    KindInfo { kind: ElementKind::Relations, name: "Relations", sort: "data", base: true, addable: false, command: "",
        about: "All versions of all relations." },
    KindInfo { kind: ElementKind::LargeObjects, name: "Large objects", sort: "data", base: true, addable: false, command: "",
        about: "Ways and relations that span large areas (kept apart so that they do not inflate the blocks)." },
    KindInfo { kind: ElementKind::BlockTable, name: "Block table", sort: "lookup", base: true, addable: false, command: "",
        about: "Where each data block lies in the file." },
    KindInfo { kind: ElementKind::SpatialIndex, name: "Spatial index", sort: "R-tree", base: true, addable: false, command: "",
        about: "R-trees over place and time of the data blocks: queries read only the blocks they need." },
    KindInfo { kind: ElementKind::TagFilters, name: "Tag filters", sort: "Bloom filter", base: true, addable: false, command: "",
        about: "A filter per block that tells which tags cannot occur in it; queries for rare tags skip most blocks." },
    KindInfo { kind: ElementKind::IdIndex, name: "ID index", sort: "lookup", base: true, addable: false, command: "",
        about: "Which blocks hold an object with a given id (history of an element, node positions of ways)." },
    KindInfo { kind: ElementKind::PlaceIndex, name: "Place index", sort: "gazetteer", base: false, addable: true, command: "places",
        about: "Settlements, administrative areas and postcodes with all their names, their hierarchy and simplified borders. \
                Speeds up `within <place name>`, geocoding of places and reverse geocoding down to the municipality." },
    KindInfo { kind: ElementKind::NameIndex, name: "Name index", sort: "full-text search", base: false, addable: true, command: "names",
        about: "All named objects (shops, streets, rivers, …) for search by name, also by prefix and with typos. \
                Uses the place index for context." },
    KindInfo { kind: ElementKind::AddressIndex, name: "Address index", sort: "geocoding", base: false, addable: true, command: "addresses",
        about: "House numbers with street, postcode and place: geocoding of addresses and reverse geocoding of points. \
                Uses the place index." },
    KindInfo { kind: ElementKind::AdminBoundaries, name: "Administrative boundaries", sort: "areas", base: false, addable: true, command: "boundaries",
        about: "All administrative boundaries (countries, states, counties, municipalities, …) with their areas already assembled: \
                address fields such as `x.city` and `x.state` and `within <place name>` read them instead of assembling them from the ways." },
    KindInfo { kind: ElementKind::TagStatistics, name: "Tag statistics", sort: "statistics", base: false, addable: true, command: "keys",
        about: "Keys and values with their counts and their development over time; answers `keys`, `k.count` and `k.values` at once." },
    KindInfo { kind: ElementKind::Changesets, name: "Changesets and users", sort: "lookup", base: false, addable: true, command: "changesets",
        about: "Changesets and users derived from the history (optionally with comments from a changeset dump); \
                answers `changesets` and `users` without reading all versions." },
    KindInfo { kind: ElementKind::RoutingNetwork, name: "Street network", sort: "routing", base: false, addable: true, command: "routing",
        about: "The road and path network as a graph: routes and isochrones need not build it from the ways first." },
    KindInfo { kind: ElementKind::RoutingCh, name: "Contraction hierarchies", sort: "routing speed-up", base: false, addable: true, command: "routing-ch",
        about: "Contraction hierarchies, one per mode of travel: routes in milliseconds, isochrones with PHAST. \
                Made for the network of each mode, it is the faster choice for routes by car and bicycle and for isochrones by car; \
                with CH and CCH both present, each question uses the faster one. Needs the street network." },
    KindInfo { kind: ElementKind::RoutingCch, name: "Customizable contraction hierarchies", sort: "routing speed-up", base: false, addable: true, command: "routing-cch",
        about: "Customizable contraction hierarchies: one order for all modes of travel, the travel times filled in within seconds. \
                The faster choice for routes on foot and for isochrones on foot and by bicycle; with CH and CCH both present, \
                each question uses the faster one. Needs the street network." },
];

impl ElementKind {
    pub fn from_u16(v: u16) -> Option<ElementKind> {
        KINDS.iter().map(|k| k.kind).find(|k| *k as u16 == v)
    }

    pub fn info(self) -> &'static KindInfo {
        KINDS.iter().find(|k| k.kind == self).expect("every kind is listed")
    }
}

/// Which moments an index describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Temporal {
    /// Not applicable (parts of the database).
    #[default]
    None = 0,
    /// The last moment of the data.
    Latest = 1,
    Daily = 2,
    Weekly = 3,
    Monthly = 4,
    Yearly = 5,
    /// Every moment.
    Full = 6,
}

impl Temporal {
    pub fn from_u8(v: u8) -> Option<Temporal> {
        use Temporal::*;
        Some(match v {
            0 => None,
            1 => Latest,
            2 => Daily,
            3 => Weekly,
            4 => Monthly,
            5 => Yearly,
            6 => Full,
            _ => return Option::None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Temporal::None => "",
            Temporal::Latest => "latest",
            Temporal::Daily => "daily",
            Temporal::Weekly => "weekly",
            Temporal::Monthly => "monthly",
            Temporal::Yearly => "yearly",
            Temporal::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Temporal> {
        [Temporal::Latest, Temporal::Daily, Temporal::Weekly, Temporal::Monthly, Temporal::Yearly, Temporal::Full]
            .into_iter()
            .find(|t| t.name() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub id: u32,
    pub kind: ElementKind,
    pub temporal: Temporal,
    /// Unix seconds.
    pub created: i64,
    pub sections: Vec<SectionEntry>,
    /// Free-form parameters (`key`, `value`), e.g. the moment an index describes.
    pub params: Vec<(String, String)>,
}

impl Element {
    pub fn size(&self) -> u64 {
        self.sections.iter().map(|s| s.length).sum()
    }

    pub fn info(&self) -> &'static KindInfo {
        self.kind.info()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Directory {
    pub next_id: u32,
    pub elements: Vec<Element>,
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

fn get_str(b: &[u8], o: &mut usize) -> Result<String> {
    let n = le::u32(b, *o)? as usize;
    let s = le::slice(b, *o + 4, n)?;
    *o += 4 + n;
    String::from_utf8(s.to_vec()).map_err(|_| corrupt("directory string"))
}

impl Directory {
    /// `DIR_MAGIC`, u32 next_id, u32 n, elements, u64 checksum of everything before it.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&DIR_MAGIC);
        b.extend_from_slice(&self.next_id.to_le_bytes());
        b.extend_from_slice(&(self.elements.len() as u32).to_le_bytes());
        for e in &self.elements {
            b.extend_from_slice(&e.id.to_le_bytes());
            b.extend_from_slice(&(e.kind as u16).to_le_bytes());
            b.push(e.temporal as u8);
            b.push(0);
            b.extend_from_slice(&e.created.to_le_bytes());
            b.extend_from_slice(&(e.sections.len() as u32).to_le_bytes());
            for s in &e.sections {
                b.extend_from_slice(&s.kind.to_le_bytes());
                b.extend_from_slice(&s.flags.to_le_bytes());
                b.extend_from_slice(&s.offset.to_le_bytes());
                b.extend_from_slice(&s.length.to_le_bytes());
                b.extend_from_slice(&s.checksum.to_le_bytes());
            }
            b.extend_from_slice(&(e.params.len() as u32).to_le_bytes());
            for (k, v) in &e.params {
                put_str(&mut b, k);
                put_str(&mut b, v);
            }
        }
        let cs = xxh3_64(&b);
        b.extend_from_slice(&cs.to_le_bytes());
        b
    }

    pub fn parse(b: &[u8], file_len: u64) -> Result<Directory> {
        if b.len() < 24 || b[0..8] != DIR_MAGIC {
            return Err(Error::BadMagic("directory"));
        }
        let body = &b[..b.len() - 8];
        if xxh3_64(body) != le::u64(b, b.len() - 8)? {
            return Err(Error::Checksum("directory".into()));
        }
        let next_id = le::u32(b, 8)?;
        let n = le::u32(b, 12)? as usize;
        let mut o = 16;
        let mut elements = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let id = le::u32(b, o)?;
            let kind = le::slice(b, o + 4, 2).map(|x| u16::from_le_bytes([x[0], x[1]]))?;
            let kind = ElementKind::from_u16(kind).ok_or_else(|| Error::Unsupported(format!("element kind {kind}")))?;
            let temporal = Temporal::from_u8(le::slice(b, o + 6, 1)?[0]).ok_or_else(|| corrupt("temporal"))?;
            let created = le::i64(b, o + 8)?;
            let ns = le::u32(b, o + 16)? as usize;
            o += 20;
            let mut sections = Vec::with_capacity(ns.min(1024));
            for _ in 0..ns {
                let s = SectionEntry {
                    kind: le::u32(b, o)?,
                    flags: le::u32(b, o + 4)?,
                    offset: le::u64(b, o + 8)?,
                    length: le::u64(b, o + 16)?,
                    checksum: le::u64(b, o + 24)?,
                };
                let end = s.offset.checked_add(s.length).ok_or_else(|| corrupt("section overflow"))?;
                if end > file_len || s.offset < crate::layout::HEADER_SIZE as u64 {
                    return Err(corrupt(format!("section {} out of bounds", s.kind)));
                }
                sections.push(s);
                o += 32;
            }
            let np = le::u32(b, o)? as usize;
            o += 4;
            let mut params = Vec::with_capacity(np.min(64));
            for _ in 0..np {
                let k = get_str(body, &mut o)?;
                let v = get_str(body, &mut o)?;
                params.push((k, v));
            }
            elements.push(Element { id, kind, temporal, created, sections, params });
        }
        Ok(Directory { next_id, elements })
    }

    /// The elements in the order of their kinds (and ids within a kind).
    pub fn sorted(&self) -> Vec<&Element> {
        let mut v: Vec<&Element> = self.elements.iter().collect();
        v.sort_by_key(|e| (KINDS.iter().position(|k| k.kind == e.kind), e.id));
        v
    }

    pub fn get(&self, id: u32) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }
}

/// The elements of a database as built: its sections grouped by meaning, with ids from 0.
pub fn base_directory(sections: &[SectionEntry], created: i64) -> Directory {
    use crate::layout::SectionKind as S;
    let groups: &[(ElementKind, &[S])] = &[
        (ElementKind::Metadata, &[S::Meta]),
        (ElementKind::Dictionaries, &[S::Keys, S::Values, S::Roles]),
        (ElementKind::Users, &[S::Users]),
        (ElementKind::CompressionDictionaries, &[S::ZdictNode, S::ZdictWay, S::ZdictRel]),
        (ElementKind::Nodes, &[S::BlocksNode]),
        (ElementKind::Ways, &[S::BlocksWay]),
        (ElementKind::Relations, &[S::BlocksRelation]),
        (ElementKind::LargeObjects, &[S::BlocksLarge]),
        (ElementKind::BlockTable, &[S::BlockTable]),
        (ElementKind::SpatialIndex, &[S::RTree]),
        (ElementKind::TagFilters, &[S::BlockBlooms]),
        (ElementKind::IdIndex, &[S::IdxNode, S::IdxWay, S::IdxRel]),
    ];
    let mut d = Directory::default();
    for (kind, ks) in groups {
        let secs: Vec<SectionEntry> = sections.iter().filter(|s| ks.iter().any(|k| *k as u32 == s.kind)).copied().collect();
        if secs.is_empty() {
            continue;
        }
        d.elements.push(Element { id: d.next_id, kind: *kind, temporal: Temporal::None, created, sections: secs, params: vec![] });
        d.next_id += 1;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_roundtrip() {
        let d = Directory {
            next_id: 14,
            elements: vec![
                Element {
                    id: 4,
                    kind: ElementKind::Nodes,
                    temporal: Temporal::None,
                    created: 1_760_000_000,
                    sections: vec![SectionEntry { kind: 16, flags: 0, offset: 1024, length: 99, checksum: 7 }],
                    params: vec![],
                },
                Element {
                    id: 13,
                    kind: ElementKind::TagStatistics,
                    temporal: Temporal::Monthly,
                    created: 1_760_000_100,
                    sections: vec![],
                    params: vec![("at".into(), "2026-10-05".into())],
                },
            ],
        };
        let b = d.to_bytes();
        assert_eq!(Directory::parse(&b, 4096).unwrap(), d);
        let mut bad = b.clone();
        bad[20] ^= 1;
        assert!(Directory::parse(&bad, 4096).is_err());
        assert!(Directory::parse(&b, 1100).is_err());
        // every kind is listed once, and the codes are distinct
        let mut codes: Vec<u16> = KINDS.iter().map(|k| k.kind as u16).collect();
        codes.dedup();
        assert_eq!(codes.len(), KINDS.len());
    }
}
