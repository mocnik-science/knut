//! Units of measure (KNUT_LANG.md §2.5, §5.5).

/// Dimension as exponents of length, time and mass; `pct` marks slopes (`%`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Dim {
    pub l: i8,
    pub t: i8,
    pub m: i8,
    pub pct: bool,
}

impl Dim {
    pub const NONE: Dim = Dim { l: 0, t: 0, m: 0, pct: false };
    pub const LENGTH: Dim = Dim { l: 1, t: 0, m: 0, pct: false };
    pub const AREA: Dim = Dim { l: 2, t: 0, m: 0, pct: false };
    pub const SPEED: Dim = Dim { l: 1, t: -1, m: 0, pct: false };
    pub const SLOPE: Dim = Dim { l: 0, t: 0, m: 0, pct: true };
    pub const MASS: Dim = Dim { l: 0, t: 0, m: 1, pct: false };
    pub const TIME: Dim = Dim { l: 0, t: 1, m: 0, pct: false };

    pub fn is_none(&self) -> bool {
        *self == Dim::NONE
    }

    pub fn mul(self, o: Dim) -> Option<Dim> {
        if self.pct || o.pct {
            return if self.is_none() || o.is_none() { Some(if self.pct { self } else { o }) } else { None };
        }
        Some(Dim { l: self.l + o.l, t: self.t + o.t, m: self.m + o.m, pct: false })
    }

    pub fn div(self, o: Dim) -> Option<Dim> {
        if o.pct {
            return if self.pct { Some(Dim::NONE) } else { None };
        }
        if self.pct {
            return if o.is_none() { Some(self) } else { None };
        }
        Some(Dim { l: self.l - o.l, t: self.t - o.t, m: self.m - o.m, pct: false })
    }

    /// Name of the measure, as used in messages.
    pub fn name(&self, de: bool) -> String {
        let (en, g) = match *self {
            Dim::LENGTH => ("length", "Länge"),
            Dim::AREA => ("area", "Fläche"),
            Dim::SPEED => ("speed", "Geschwindigkeit"),
            Dim::SLOPE => ("slope", "Neigung"),
            Dim::MASS => ("weight", "Gewicht"),
            Dim::TIME => ("time", "Dauer"),
            Dim::NONE => ("plain number", "einfache Zahl"),
            _ => return self.formula(),
        };
        if de { g.to_string() } else { en.to_string() }
    }

    /// Indefinite article form for messages ("a length", "an area").
    pub fn a_name(&self) -> String {
        let n = self.name(false);
        if n.starts_with(['a', 'e', 'i', 'o', 'u']) { format!("an {n}") } else { format!("a {n}") }
    }

    /// SI formula like `m3` or `m/s2`.
    pub fn formula(&self) -> String {
        let mut num = Vec::new();
        let mut den = Vec::new();
        for (e, s) in [(self.l, "m"), (self.m, "kg"), (self.t, "s")] {
            let p = |e: i8| if e.abs() == 1 { s.to_string() } else { format!("{s}{}", e.abs()) };
            if e > 0 {
                num.push(p(e));
            } else if e < 0 {
                den.push(p(e));
            }
        }
        let n = if num.is_empty() { "1".to_string() } else { num.join("·") };
        if den.is_empty() { n } else { format!("{n}/{}", den.join("·")) }
    }
}

/// Calendar units add calendar months on dates instead of a fixed number of seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Calendar {
    Months(i32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Unit {
    pub name: &'static str,
    pub dim: Dim,
    /// Value of one unit in base units (m, m², m/s, %, kg, s).
    pub factor: f64,
    pub calendar: Option<Calendar>,
}

const MILE: f64 = 1609.344;
const KMH: f64 = 1000.0 / 3600.0;
/// Mean Gregorian year in seconds (365.2425 days).
pub const YEAR_S: f64 = 365.2425 * 86_400.0;

macro_rules! u {
    ($n:expr, $d:expr, $f:expr) => {
        Unit { name: $n, dim: $d, factor: $f, calendar: None }
    };
    ($n:expr, $d:expr, $f:expr, $c:expr) => {
        Unit { name: $n, dim: $d, factor: $f, calendar: Some($c) }
    };
}

pub static UNITS: &[Unit] = &[
    u!("m", Dim::LENGTH, 1.0),
    u!("km", Dim::LENGTH, 1000.0),
    u!("mi", Dim::LENGTH, MILE),
    u!("mile", Dim::LENGTH, MILE),
    u!("miles", Dim::LENGTH, MILE),
    u!("mil", Dim::LENGTH, 10_000.0),
    u!("m2", Dim::AREA, 1.0),
    u!("km2", Dim::AREA, 1e6),
    u!("mi2", Dim::AREA, MILE * MILE),
    u!("mile2", Dim::AREA, MILE * MILE),
    u!("miles2", Dim::AREA, MILE * MILE),
    u!("mil2", Dim::AREA, 1e8),
    u!("ha", Dim::AREA, 10_000.0),
    u!("km/h", Dim::SPEED, KMH),
    u!("mph", Dim::SPEED, MILE / 3600.0),
    u!("knots", Dim::SPEED, 1.852 * KMH),
    u!("knot", Dim::SPEED, 1.852 * KMH),
    u!("%", Dim::SLOPE, 1.0),
    u!("t", Dim::MASS, 1000.0),
    u!("kg", Dim::MASS, 1.0),
    u!("g", Dim::MASS, 0.001),
    u!("s", Dim::TIME, 1.0),
    u!("min", Dim::TIME, 60.0),
    u!("h", Dim::TIME, 3600.0),
    u!("days", Dim::TIME, 86_400.0),
    u!("weeks", Dim::TIME, 604_800.0),
    u!("months", Dim::TIME, YEAR_S / 12.0, Calendar::Months(1)),
    u!("years", Dim::TIME, YEAR_S, Calendar::Months(12)),
    // derived units used for display only
    u!("m/s", Dim::SPEED, 1.0),
];

/// Unit names accepted in scripts (all but display-only ones).
pub fn unit(name: &str) -> Option<&'static Unit> {
    if name == "m/s" {
        return None;
    }
    UNITS.iter().find(|u| u.name == name)
}

/// Unit used to display a computed quantity of dimension `d`.
pub fn display_unit(d: Dim) -> Option<&'static Unit> {
    let name = match d {
        Dim::LENGTH => "m",
        Dim::AREA => "m2",
        Dim::SPEED => "km/h",
        Dim::SLOPE => "%",
        Dim::MASS => "kg",
        Dim::TIME => "s",
        _ => return None,
    };
    UNITS.iter().find(|u| u.name == name)
}

/// Words that can be units; used for the W005 check.
pub fn is_unit_word(w: &str) -> bool {
    unit(w).is_some() || w == "km" || w == "h"
}

/// Parses a tag value such as `35`, `35 mph`, `7.5t`, `50 km/h`, `12%`.
/// Returns the number and the unit written in the value (if any).
/// Unit that the OSM documentation gives for plain numbers in a tag (`maxspeed=50` means
/// km/h, `height=12` means metres). Used when a measure is expected and the value has no
/// unit of its own and none is written after the key.
pub fn default_unit(key: &str) -> Option<&'static Unit> {
    let parts: Vec<&str> = key.split(':').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    // qualifiers that never hold a plain measure
    const NOT_A_VALUE: &[&str] = &["type", "source", "note", "conditional", "signed", "variable", "zone", "description", "fixme", "unit"];
    if parts.len() > 1 && NOT_A_VALUE.contains(&last) {
        return None;
    }
    let name = match () {
        _ if matches!(first, "maxspeed" | "minspeed") => "km/h",
        _ if key == "distance" => "km",
        _ if matches!(
            last,
            "height" | "min_height" | "width" | "length" | "maxheight" | "maxwidth" | "maxlength" | "est_width" | "est_height" | "ele" | "depth"
        ) =>
        {
            "m"
        }
        _ if matches!(last, "maxweight" | "maxaxleload" | "maxweightrating" | "maxbogieweight") => "t",
        _ => return None,
    };
    unit(name)
}

pub fn parse_value(s: &str) -> Option<(f64, Option<&'static Unit>)> {
    let s = s.trim();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && (c == '-' || c == '+'))))
        .map_or(s.len(), |(i, _)| i);
    let (num, rest) = s.split_at(end);
    if num.is_empty() || num == "-" || num == "+" {
        return None;
    }
    let v: f64 = num.parse().ok()?;
    let rest = rest.trim();
    if rest.is_empty() {
        return Some((v, None));
    }
    let u = match rest {
        "kmh" | "km/h" | "kph" => unit("km/h"),
        "knots" | "knot" | "kn" => unit("knots"),
        "'" => None,
        r => unit(r),
    }?;
    Some((v, Some(u)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_units() {
        assert_eq!(default_unit("maxspeed").map(|u| u.name), Some("km/h"));
        assert_eq!(default_unit("maxspeed:forward").map(|u| u.name), Some("km/h"));
        assert_eq!(default_unit("maxspeed:type"), None);
        assert_eq!(default_unit("building:height").map(|u| u.name), Some("m"));
        assert_eq!(default_unit("maxweight").map(|u| u.name), Some("t"));
        assert_eq!(default_unit("distance").map(|u| u.name), Some("km"));
        assert_eq!(default_unit("capacity"), None);
    }

    #[test]
    fn values() {
        assert_eq!(parse_value("35").unwrap().0, 35.0);
        assert_eq!(parse_value("35 mph").unwrap().1.unwrap().name, "mph");
        assert_eq!(parse_value("7.5t").unwrap().1.unwrap().name, "t");
        assert!(parse_value("about 200").is_none());
        assert!(parse_value("DE:urban").is_none());
        assert_eq!(parse_value("12%").unwrap().1.unwrap().name, "%");
    }

    #[test]
    fn algebra() {
        assert_eq!(Dim::LENGTH.mul(Dim::LENGTH), Some(Dim::AREA));
        assert_eq!(Dim::LENGTH.div(Dim::TIME), Some(Dim::SPEED));
        assert_eq!(Dim::LENGTH.div(Dim::SPEED), Some(Dim::TIME));
    }
}
