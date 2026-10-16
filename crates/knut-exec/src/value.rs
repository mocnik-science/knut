//! Run-time values (KNUT_LANG.md §4).

use crate::geom::{BBoxF, Geom};
use crate::region::Region;
use crate::set::SetV;
use knut_format::{Member, ObjType, Str, Tags};
use knut_lang::dates::{self, DateVal, Precision};
use knut_lang::diag::{Diagnostic, Span, Text2};
use knut_lang::units::{self, Dim, Unit};
use std::cmp::Ordering;
use std::sync::{Arc, OnceLock};

// ----- errors -------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RtErr {
    /// Boxed so that `R<T>` stays small on the success path.
    pub diag: Box<Diagnostic>,
    /// Data errors can be caught by `??` (§5.3).
    pub catchable: bool,
    /// Element that caused the error, for deterministic reporting (§7.6).
    pub order: Option<(u8, i64, u32)>,
}

pub type R<T> = Result<T, RtErr>;

impl RtErr {
    pub fn data(d: Diagnostic) -> RtErr {
        RtErr { diag: Box::new(d), catchable: true, order: None }
    }

    pub fn fatal(d: Diagnostic) -> RtErr {
        RtErr { diag: Box::new(d), catchable: false, order: None }
    }

    pub fn internal(span: Span, msg: impl std::fmt::Display) -> RtErr {
        RtErr::fatal(Diagnostic::error(
            "E901",
            span,
            Text2::new(format!("The query could not be completed: {msg}"), format!("Die Abfrage ließ sich nicht abschließen: {msg}")),
        ))
    }

    pub fn at(mut self, e: &Elem) -> RtErr {
        if self.order.is_none() {
            self.order = Some(e.order());
        }
        self
    }
}

/// Picks the error to report among several (smallest element first).
pub fn first_error(errs: impl IntoIterator<Item = RtErr>) -> Option<RtErr> {
    errs.into_iter().min_by(|a, b| match (a.order, b.order) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    })
}

// ----- numbers ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Num {
    /// Value in base units.
    pub v: f64,
    pub dim: Dim,
    /// Unit used for display (and calendar arithmetic).
    pub unit: Option<&'static Unit>,
}

impl Num {
    pub fn plain(v: f64) -> Num {
        Num { v, dim: Dim::NONE, unit: None }
    }

    pub fn with_unit(x: f64, u: &'static Unit) -> Num {
        Num { v: x * u.factor, dim: u.dim, unit: Some(u) }
    }

    pub fn base(v: f64, dim: Dim) -> Num {
        Num { v, dim, unit: units::display_unit(dim) }
    }

    /// Value in the display unit.
    pub fn shown(&self) -> f64 {
        match self.unit {
            Some(u) => self.v / u.factor,
            None => self.v,
        }
    }

    pub fn unit_name(&self) -> String {
        match self.unit {
            Some(u) => u.name.to_string(),
            None if self.dim.is_none() => String::new(),
            None => self.dim.formula(),
        }
    }

    pub fn display(&self) -> String {
        let x = self.shown();
        let s = fmt_f64(x);
        let u = self.unit_name();
        if u.is_empty() { s } else { format!("{s} {u}") }
    }
}

pub fn fmt_f64(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.fract() == 0.0 && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    let a = x.abs();
    let s = if a >= 1000.0 {
        format!("{x:.1}")
    } else if a >= 1.0 {
        format!("{x:.3}")
    } else if a >= 0.001 || a == 0.0 {
        format!("{x:.4}")
    } else {
        // small values keep three significant digits (0.0000123)
        let d = (-a.log10().floor()) as usize + 2;
        format!("{x:.d$}")
    };
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".into() } else { s }
}

// ----- elements -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EKind {
    Node,
    Way,
    Area,
    Relation,
    Changeset,
    User,
    Note,
    Key,
    Row,
}

impl EKind {
    pub fn name(self) -> &'static str {
        match self {
            EKind::Node => "node",
            EKind::Way => "way",
            EKind::Area => "area",
            EKind::Relation => "relation",
            EKind::Changeset => "changeset",
            EKind::User => "user",
            EKind::Note => "note",
            EKind::Key => "key",
            EKind::Row => "row",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            EKind::Node => "nodes",
            EKind::Way => "ways",
            EKind::Area => "areas",
            EKind::Relation => "relations",
            EKind::Changeset => "changesets",
            EKind::User => "users",
            EKind::Note => "notes",
            EKind::Key => "keys",
            EKind::Row => "rows",
        }
    }

    pub fn from_kind(k: knut_lang::ast::Kind) -> Option<EKind> {
        use knut_lang::ast::Kind;
        Some(match k {
            Kind::Node => EKind::Node,
            Kind::Way => EKind::Way,
            Kind::Area => EKind::Area,
            Kind::Relation => EKind::Relation,
            Kind::Changeset => EKind::Changeset,
            Kind::User => EKind::User,
            Kind::Note => EKind::Note,
            Kind::Key => EKind::Key,
            Kind::Any => return None,
        })
    }
}

/// One version of an OSM object.
#[derive(Clone, Debug)]
pub struct Ver {
    pub version: u32,
    pub ts: i64,
    pub changeset: u64,
    pub uid: u32,
    pub visible: bool,
    pub tags: Tags,
    pub nodes: Arc<[i64]>,
    pub members: Arc<[Member]>,
    /// Node position (e7).
    pub pos: Option<(i32, i32)>,
}

impl Ver {
    pub fn tag(&self, k: &str) -> Option<&Str> {
        self.tags.iter().find(|(kk, _)| kk.as_ref() == k).map(|(_, v)| v)
    }

    pub fn empty() -> Ver {
        Ver {
            version: 0,
            ts: 0,
            changeset: 0,
            uid: 0,
            visible: true,
            tags: Arc::from(Vec::new()),
            nodes: Arc::from(Vec::new()),
            members: Arc::from(Vec::new()),
            pos: None,
        }
    }

    /// Closed way with at least 4 nodes.
    pub fn closed(&self) -> bool {
        self.nodes.len() >= 4 && self.nodes.first() == self.nodes.last()
    }
}

/// All versions of one object, in version order.
#[derive(Debug)]
pub struct Hist {
    pub otype: ObjType,
    pub id: i64,
    pub versions: Vec<Ver>,
}

#[derive(Debug, Clone)]
pub struct CsRec {
    pub id: u64,
    pub uid: u32,
    pub t0: i64,
    pub t1: i64,
    pub edits: u64,
    pub bbox: BBoxF,
    pub comment: Option<Str>,
    pub tags: Tags,
}

#[derive(Debug)]
pub struct UserRec {
    pub uid: u32,
    pub name: Option<Str>,
    /// Changeset ids of this user in time order.
    pub changesets: Vec<u64>,
    pub edits: u64,
}

#[derive(Debug)]
pub struct NoteRec {
    pub id: i64,
    pub lon: f64,
    pub lat: f64,
    pub created: i64,
    pub closed: Option<i64>,
    pub status: Str,
    pub text: Str,
    pub comments: u32,
}

#[derive(Debug)]
pub struct KeyRec {
    pub name: Str,
    pub count: u64,
    pub values: u64,
}

#[derive(Debug, Clone)]
pub enum Extra {
    None,
    Changeset(Arc<CsRec>),
    User(Arc<UserRec>),
    Note(Arc<NoteRec>),
    Key(Arc<KeyRec>),
    /// Data set row: (file label)
    Row(Arc<str>),
    /// An element found by `geocode` (score 0–1) or `reverse-geocode` (distance in metres).
    Hit { score: f64, distance: Option<f64> },
}

#[derive(Debug)]
pub struct Elem {
    pub kind: EKind,
    pub otype: Option<ObjType>,
    pub id: i64,
    pub v: Ver,
    /// Time at which the geometry is evaluated (state time, version time or bucket start).
    pub at: i64,
    /// Validity end of this version (next version's timestamp), if known.
    pub until: Option<i64>,
    /// Bounding box hint (segment bbox), used for relations without assembled geometry.
    pub bbox: Option<BBoxF>,
    pub geom: OnceLock<Arc<Geom>>,
    pub hist: OnceLock<Arc<Hist>>,
    pub extra: Extra,
}

impl Elem {
    pub fn new(kind: EKind, otype: Option<ObjType>, id: i64, v: Ver, at: i64) -> Elem {
        Elem {
            kind,
            otype,
            id,
            v,
            at,
            until: None,
            bbox: None,
            geom: OnceLock::new(),
            hist: OnceLock::new(),
            extra: Extra::None,
        }
    }

    pub fn order(&self) -> (u8, i64, u32) {
        (self.kind as u8, self.id, self.v.version)
    }

    /// Order of the object (kind ignoring area/way split for history lookups).
    /// For a hit of `geocode` or `reverse-geocode`: its address (from the tags) and score or
    /// distance, for the text of results.
    pub fn hit_text(&self) -> Option<String> {
        let Extra::Hit { score, distance } = &self.extra else { return None };
        let t = |k: &str| self.tag(k).map(|s| s.to_string());
        let line1 = [t("addr:street").or_else(|| t("addr:place")), t("addr:housenumber")].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let line2 = [t("addr:postcode"), t("addr:city")].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let addr = [line1, line2].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
        let how = match distance {
            Some(d) => format!("{} m", fmt_f64((d * 10.0).round() / 10.0)),
            None => format!("score {}", fmt_f64(*score)),
        };
        Some(format!("{addr} · {how}"))
    }

    pub fn label(&self) -> String {
        match self.kind {
            EKind::Node | EKind::Way | EKind::Area | EKind::Relation => {
                let t = self.otype.map_or(self.kind.name(), |o| o.name());
                if self.kind == EKind::Area {
                    format!("area ({t} {}) v{}", self.id, self.v.version)
                } else {
                    format!("{t} {} v{}", self.id, self.v.version)
                }
            }
            EKind::Row => format!("row {}", self.id),
            k => format!("{} {}", k.name(), self.id),
        }
    }

    pub fn tag(&self, k: &str) -> Option<&Str> {
        self.v.tag(k)
    }
}

// ----- composite values ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TagVal {
    pub text: Str,
    pub key: Str,
    pub owner: (EKind, i64),
}

#[derive(Debug)]
pub struct Tuple {
    pub names: Arc<[String]>,
    pub items: Vec<Arc<Elem>>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub key: Value,
    pub value: Value,
    pub count: u64,
    pub items: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Grouped {
    /// Names of the key columns (for tables).
    pub key_names: Vec<String>,
    pub value_name: String,
    pub rows: Vec<Group>,
}

pub struct Closure {
    pub params: Vec<String>,
    pub body: &'static knut_lang::ast::Expr,
    pub scope: crate::eval::Sc,
}

impl std::fmt::Debug for Closure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<function {:?}>", self.params)
    }
}

#[derive(Debug, Clone)]
pub struct PathV {
    pub length: f64,
    pub duration: f64,
    pub ways: Vec<i64>,
    pub geom: Arc<Geom>,
}

#[derive(Clone, Debug)]
pub enum Value {
    None,
    Bool(bool),
    Num(Num),
    Text(Str),
    /// Value of a tag, with its origin for messages.
    Tag(Arc<TagVal>),
    Time(DateVal),
    Elem(Arc<Elem>),
    Tuple(Arc<Tuple>),
    Set(Arc<SetV>),
    List(Arc<Vec<Value>>),
    Group(Arc<Group>),
    Grouped(Arc<Grouped>),
    Region(Arc<Region>),
    Net(Arc<crate::net::Net>),
    Path(Arc<PathV>),
    Record(Arc<Vec<(String, Value)>>),
    Lambda(Arc<Closure>),
}

impl Value {
    pub fn num(x: f64) -> Value {
        Value::Num(Num::plain(x))
    }

    pub fn text(s: &str) -> Value {
        Value::Text(Arc::from(s))
    }

    pub fn instant(t: i64) -> Value {
        Value::Time(dates::instant(t))
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Value::None)
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            Value::Tag(t) => Some(&t.text),
            _ => None,
        }
    }

    pub fn type_name(&self, de: bool) -> &'static str {
        let (en, g) = match self {
            Value::None => ("none", "none"),
            Value::Bool(_) => ("true or false", "wahr oder falsch"),
            Value::Num(_) => ("a number", "eine Zahl"),
            Value::Text(_) | Value::Tag(_) => ("a text", "ein Text"),
            Value::Time(_) => ("a date", "ein Datum"),
            Value::Elem(_) => ("a single element", "ein einzelnes Element"),
            Value::Tuple(_) => ("a sequence match", "ein Treffer einer Sequenz"),
            Value::Set(_) => ("a set", "eine Menge"),
            Value::List(_) => ("a list", "eine Liste"),
            Value::Group(_) => ("a group", "eine Gruppe"),
            Value::Grouped(_) => ("a grouped result", "ein gruppiertes Ergebnis"),
            Value::Region(_) => ("a region", "eine Region"),
            Value::Net(_) => ("a network", "ein Netz"),
            Value::Path(_) => ("a path", "ein Pfad"),
            Value::Record(_) => ("a record", "ein Datensatz"),
            Value::Lambda(_) => ("a function", "eine Funktion"),
        };
        if de { g } else { en }
    }

    /// Hashable key for grouping and set membership.
    pub fn key(&self) -> Key {
        match self {
            Value::None => Key::None,
            Value::Bool(b) => Key::Bool(*b),
            Value::Num(n) => Key::Num(OrdF(n.v), n.dim, n.unit.map(|u| u.name)),
            Value::Text(s) => Key::Text(s.clone()),
            Value::Tag(t) => Key::Text(t.text.clone()),
            Value::Time(d) => Key::Time(d.start, d.end, d.prec),
            Value::Elem(e) => {
                let (k, id, v) = e.order();
                Key::Elem(k, id, v)
            }
            Value::Tuple(t) => Key::Tuple(t.items.iter().map(|e| Key::Elem(e.kind as u8, e.id, e.v.version)).collect()),
            Value::List(xs) => Key::Tuple(xs.iter().map(|x| x.key()).collect()),
            Value::Group(g) => g.key.key(),
            Value::Record(r) => Key::Tuple(r.iter().map(|(_, v)| v.key()).collect()),
            Value::Region(r) => Key::Text(Arc::from(r.label())),
            Value::Set(s) => Key::Ptr(Arc::as_ptr(s) as usize),
            Value::Grouped(s) => Key::Ptr(Arc::as_ptr(s) as usize),
            Value::Net(s) => Key::Ptr(Arc::as_ptr(s) as usize),
            Value::Path(s) => Key::Ptr(Arc::as_ptr(s) as usize),
            Value::Lambda(s) => Key::Ptr(Arc::as_ptr(s) as usize),
        }
    }

    /// Short text for tables.
    pub fn display(&self) -> String {
        match self {
            Value::None => "none".into(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => n.display(),
            Value::Text(s) => s.to_string(),
            Value::Tag(t) => t.text.to_string(),
            Value::Time(d) => dates::format(d),
            Value::Elem(e) => e.label(),
            Value::Tuple(t) => {
                let e0 = &t.items[0];
                let t0 = e0.otype.map_or(e0.kind.name(), |o| o.name());
                format!("{t0} {} v{}", e0.id, t.items.iter().map(|e| e.v.version.to_string()).collect::<Vec<_>>().join(" → v"))
            }
            Value::Set(s) => format!("set of {}", s.len_hint().map_or("…".to_string(), |n| n.to_string())),
            Value::List(xs) => {
                if xs.len() <= 8 {
                    format!("[{}]", xs.iter().map(|x| x.display()).collect::<Vec<_>>().join(", "))
                } else {
                    format!("list of {}", xs.len())
                }
            }
            Value::Group(g) => format!("{} → {}", g.key.display(), g.value.display()),
            Value::Grouped(g) => format!("{} groups", g.rows.len()),
            Value::Region(r) => r.label(),
            Value::Net(n) => format!("network ({} edges)", n.n_edges()),
            Value::Path(p) => format!("path of {}", Num::with_unit(p.length / 1000.0, units::unit("km").unwrap()).display()),
            Value::Record(r) => r.iter().map(|(k, v)| format!("{k}: {}", v.display())).collect::<Vec<_>>().join(", "),
            Value::Lambda(_) => "<function>".into(),
        }
    }
}

/// Total-order float wrapper for keys.
#[derive(Clone, Copy, Debug)]
pub struct OrdF(pub f64);

impl PartialEq for OrdF {
    fn eq(&self, o: &Self) -> bool {
        self.0.to_bits() == o.0.to_bits() || self.0 == o.0
    }
}
impl Eq for OrdF {}
impl std::hash::Hash for OrdF {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        let v = if self.0 == 0.0 { 0.0f64 } else { self.0 };
        v.to_bits().hash(h)
    }
}
impl PartialOrd for OrdF {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for OrdF {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    None,
    Bool(bool),
    Num(OrdF, Dim, Option<&'static str>),
    Time(i64, i64, Precision),
    Text(Str),
    Elem(u8, i64, u32),
    Tuple(Vec<Key>),
    Ptr(usize),
}
