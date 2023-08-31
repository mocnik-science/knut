//! Static types used for checking (KNUT_LANG.md §4). `Any` means "only known at run time";
//! checks are only reported when both sides are known.

use knut_lang::ast::Kind;
use knut_lang::units::Dim;

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Any,
    None,
    Bool,
    /// Number with a known dimension (`Dim::NONE` = plain number).
    Num(Dim),
    /// Number of unknown dimension.
    NumAny,
    Text,
    /// Value of a tag access: text, but converted to a number where one is expected.
    TagText,
    Time,
    Elem(Option<Kind>),
    Tuple(Vec<(String, Kind)>),
    Set(Box<Ty>),
    List(Box<Ty>),
    Group,
    Grouped,
    Region,
    Net,
    Path,
    Record,
    Lambda,
}

impl Ty {
    pub fn elem(&self) -> Ty {
        match self {
            Ty::Set(t) | Ty::List(t) => (**t).clone(),
            Ty::Grouped => Ty::Group,
            _ => Ty::Any,
        }
    }

    pub fn is_collection(&self) -> bool {
        matches!(self, Ty::Set(_) | Ty::List(_) | Ty::Grouped)
    }

    pub fn dim(&self) -> Option<Dim> {
        match self {
            Ty::Num(d) => Some(*d),
            _ => None,
        }
    }

    pub fn join(&self, o: &Ty) -> Ty {
        if self == o {
            return self.clone();
        }
        match (self, o) {
            (Ty::None, x) | (x, Ty::None) => x.clone(),
            (Ty::TagText, Ty::Num(d)) | (Ty::Num(d), Ty::TagText) => Ty::Num(*d),
            (Ty::Num(_), Ty::Num(_)) | (Ty::NumAny, Ty::Num(_)) | (Ty::Num(_), Ty::NumAny) => Ty::NumAny,
            (Ty::Elem(_), Ty::Elem(_)) => Ty::Elem(None),
            _ => Ty::Any,
        }
    }

    pub fn describe(&self, de: bool) -> String {
        let (en, g) = match self {
            Ty::Any => ("a value", "ein Wert"),
            Ty::None => ("none", "none"),
            Ty::Bool => ("true or false", "wahr oder falsch"),
            Ty::Num(_) | Ty::NumAny => ("a number", "eine Zahl"),
            Ty::Text | Ty::TagText => ("a text", "ein Text"),
            Ty::Time => ("a date", "ein Datum"),
            Ty::Elem(_) => ("a single element", "ein einzelnes Element"),
            Ty::Tuple(_) => ("a sequence match", "ein Treffer einer Sequenz"),
            Ty::Set(_) => ("a set", "eine Menge"),
            Ty::List(_) => ("a list", "eine Liste"),
            Ty::Group => ("a group", "eine Gruppe"),
            Ty::Grouped => ("a grouped result", "ein gruppiertes Ergebnis"),
            Ty::Region => ("a region", "eine Region"),
            Ty::Net => ("a network", "ein Netz"),
            Ty::Path => ("a path", "ein Pfad"),
            Ty::Record => ("a record", "ein Datensatz"),
            Ty::Lambda => ("a function", "eine Funktion"),
        };
        if de { g.into() } else { en.into() }
    }
}

/// Built-in fields per kind (KNUT_LANG.md §5.2a) and their types.
pub fn elem_field(kind: Option<Kind>, f: &str) -> Option<Ty> {
    use knut_lang::units::Dim as D;
    let common = match f {
        "type" | "osmType" | "name" => Some(Ty::Text),
        "id" | "version" => Some(Ty::Num(D::NONE)),
        "time" => Some(Ty::Time),
        "deleted" | "visible" => Some(Ty::Bool),
        "keys" => Some(Ty::List(Box::new(Ty::Text))),
        "tags" => Some(Ty::Record),
        "geometry" => Some(Ty::Region),
        "center" => Some(Ty::Region),
        "lon" | "lat" => Some(Ty::Num(D::NONE)),
        _ => None,
    };
    match kind {
        Some(Kind::Node | Kind::Way | Kind::Area | Kind::Relation | Kind::Any) | None => {
            let osm = match f {
                "user" | "creator" => Some(Ty::Elem(Some(Kind::User))),
                "changeset" => Some(Ty::Elem(Some(Kind::Changeset))),
                "next" | "previous" => Some(Ty::Elem(kind)),
                "unchangedBefore" | "unchangedAfter" => Some(Ty::Num(D::TIME)),
                "length" => Some(Ty::Num(D::LENGTH)),
                "area" => Some(Ty::Num(D::AREA)),
                "nodes" => Some(Ty::List(Box::new(Ty::Elem(Some(Kind::Node))))),
                "members" => Some(Ty::List(Box::new(Ty::Elem(None)))),
                "roles" => Some(Ty::List(Box::new(Ty::Text))),
                "versions" => Some(Ty::List(Box::new(Ty::Elem(kind)))),
                "count" => Some(Ty::Num(D::NONE)),
                _ => None,
            };
            let user_like = if kind.is_none() { user_field(f).or_else(|| changeset_field(f)).or_else(|| note_field(f)).or_else(|| key_field(f)) } else { None };
            common.or(osm).or(user_like)
        }
        Some(Kind::User) => user_field(f).or(match f {
            "id" => Some(Ty::Num(D::NONE)),
            "name" => Some(Ty::Text),
            _ => None,
        }),
        Some(Kind::Changeset) => changeset_field(f).or(common),
        Some(Kind::Note) => note_field(f).or(common),
        Some(Kind::Key) => key_field(f),
    }
}

fn user_field(f: &str) -> Option<Ty> {
    use knut_lang::units::Dim as D;
    Some(match f {
        "firstEdit" | "lastEdit" => Ty::Time,
        "activeSpan" => Ty::Num(D::TIME),
        "activeDays" => Ty::Num(D::NONE),
        "changesets" => Ty::Set(Box::new(Ty::Elem(Some(Kind::Changeset)))),
        "edits" => Ty::Num(D::NONE),
        _ => return None,
    })
}

fn changeset_field(f: &str) -> Option<Ty> {
    use knut_lang::units::Dim as D;
    Some(match f {
        "user" => Ty::Elem(Some(Kind::User)),
        "time" | "closedAt" | "lastTime" => Ty::Time,
        "count" | "edits" => Ty::Num(D::NONE),
        "next" | "previous" => Ty::Elem(Some(Kind::Changeset)),
        "unchangedBefore" | "unchangedAfter" | "duration" => Ty::Num(D::TIME),
        "bbox" | "geometry" | "center" => Ty::Region,
        "comment" => Ty::Text,
        _ => return None,
    })
}

fn note_field(f: &str) -> Option<Ty> {
    use knut_lang::units::Dim as D;
    Some(match f {
        "status" | "text" => Ty::Text,
        "created" | "closed" | "time" => Ty::Time,
        "comments" => Ty::Num(D::NONE),
        "geometry" | "center" => Ty::Region,
        _ => return None,
    })
}

fn key_field(f: &str) -> Option<Ty> {
    use knut_lang::units::Dim as D;
    Some(match f {
        "name" => Ty::Text,
        "count" | "values" | "length" => Ty::Num(D::NONE),
        _ => return None,
    })
}

/// Fields of texts (tag values).
pub const TEXT_FIELDS: &[&str] = &["length", "number", "exists", "lower", "upper"];

pub fn text_field(f: &str) -> Option<Ty> {
    Some(match f {
        "length" | "number" => Ty::Num(Dim::NONE),
        "exists" => Ty::Bool,
        "lower" | "upper" => Ty::Text,
        _ => return None,
    })
}

/// All field names that can be suggested for "did you mean".
pub fn all_fields(kind: Option<Kind>) -> Vec<&'static str> {
    const ALL: &[&str] = &[
        "type", "osmType", "id", "version", "time", "deleted", "visible", "keys", "tags", "geometry", "center", "lon",
        "lat", "user", "creator", "changeset", "next", "previous", "unchangedBefore", "unchangedAfter", "length", "area",
        "nodes", "members", "roles", "versions", "firstEdit", "lastEdit", "activeSpan", "activeDays", "changesets",
        "edits", "name", "count", "closedAt", "lastTime", "duration", "bbox", "comment", "status", "text", "created",
        "closed", "comments", "values",
    ];
    ALL.iter().copied().filter(|f| elem_field(kind, f).is_some()).collect()
}
