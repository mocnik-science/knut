//! Question templates: each makes a question (in English or German), the knut query that
//! answers it (or the question back when something essential is missing) and, from the
//! results, the answer.

use crate::vocab::{LINES, MODES, Mode, THINGS, Thing};
use knut_exec::CollectedOutput;
use knut_web::chat::Reply;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    En,
    De,
}

/// The answer from the outputs of the query and from the results as the model reads them
/// (a fact the model cannot see must not be in the answer).
pub type AnswerFn = Box<dyn Fn(&[CollectedOutput], &str) -> Option<String>>;

pub struct Item {
    pub template: &'static str,
    pub lang: Lang,
    /// The conversation before the reply: user and assistant in turn, ending with the user.
    pub turns: Vec<(String, String)>,
    pub reply: Reply,
    pub answer: Option<AnswerFn>,
    /// A result of 0 (or nothing) is a fine answer, not a sign of a useless question.
    pub zero_ok: bool,
}

impl Item {
    fn query(template: &'static str, lang: Lang, question: String, query: String, answer: AnswerFn) -> Item {
        Item { template, lang, turns: vec![("user".into(), question)], reply: Reply::Query(query), answer: Some(answer), zero_ok: false }
    }

    fn ask(template: &'static str, lang: Lang, question: String, ask: String) -> Item {
        Item { template, lang, turns: vec![("user".into(), question)], reply: Reply::Ask(ask), answer: None, zero_ok: true }
    }

    pub fn question(&self) -> &str {
        &self.turns.last().expect("a question").1
    }
}

/// A place to start from: a station or a museum with its name.
pub struct Start {
    pub name: String,
    /// `pick node n with n:railway=station and n:name='…'`
    pub pick: String,
    /// "Bremen Hauptbahnhof", "the main station"
    pub en: String,
    /// "von Bremen Hauptbahnhof", "vom Hauptbahnhof"
    pub de_von: String,
    /// "bis Bremen Hauptbahnhof", "bis zum Hauptbahnhof"
    pub de_bis: String,
}

impl Start {
    /// A railway station: the questions say so ("Bremen-Burg station", "Bahnhof Bremen-Burg");
    /// the main station of the city of the database is also "the main station".
    /// A station; the main station also as people call it ("Bremen Central Station", "Bremen
    /// Hbf", "Hauptbahnhof Bremen", …), always found by its name in the data.
    pub fn station(name: &str, city: &str) -> Vec<Start> {
        let pick = format!("pick node n with n:railway=station and n:name='{name}'");
        let s = |en: String, von: String, bis: String| Start { name: name.into(), pick: pick.clone(), en, de_von: von, de_bis: bis };
        let main = [format!("{city} Hauptbahnhof"), format!("{city} Hbf"), format!("{city} Centralstation"), format!("{city} Central")].contains(&name.to_string());
        if !main {
            return vec![s(format!("{name} station"), format!("vom Bahnhof {name}"), format!("bis zum Bahnhof {name}"))];
        }
        vec![
            s("the main station".into(), "vom Hauptbahnhof".into(), "bis zum Hauptbahnhof".into()),
            s(format!("{city} Central Station"), format!("vom {city} Hbf"), format!("bis zum {city} Hbf")),
            s(format!("{city} central station"), format!("vom Hauptbahnhof {city}"), format!("bis zum Hauptbahnhof {city}")),
            s(format!("{city} main station"), format!("vom {city} Hauptbahnhof"), format!("bis zum {city} Hauptbahnhof")),
            s(format!("{city} Hbf"), format!("vom Hbf {city}"), format!("bis zum Hbf {city}")),
            s(format!("the central station of {city}"), format!("vom Hauptbahnhof in {city}"), format!("bis zum Hauptbahnhof in {city}")),
        ]
    }

    /// A place known by its name only (a museum, …): found by the name; also "… in TOWN",
    /// found by the name within the town.
    pub fn named(name: &str, city: &str) -> Vec<Start> {
        vec![
            Start { name: name.into(), pick: format!("pick * x with x:name='{name}'"), en: name.into(), de_von: format!("von {name}"), de_bis: format!("bis {name}") },
            Start {
                name: name.into(),
                pick: format!("pick * x with x:name='{name}' within {}", region(city)),
                en: format!("{name} in {city}"),
                de_von: format!("von {name} in {city}"),
                de_bis: format!("bis {name} in {city}"),
            },
        ]
    }
}

/// What a database offers to the questions.
/// An address of the database (for geocoding questions).
pub struct Addr {
    pub street: String,
    pub number: String,
    pub city: String,
    pub postcode: String,
    pub lon: f64,
    pub lat: f64,
}

impl Addr {
    /// As people write it: `Am Wall 201, Bremen`, sometimes with the postcode or without the
    /// comma.
    fn text(&self, rng: &mut Rng) -> String {
        match rng.below(5) {
            0 if !self.postcode.is_empty() => format!("{} {}, {} {}", self.street, self.number, self.postcode, self.city),
            1 => format!("{} {} {}", self.street, self.number, self.city),
            2 => format!("{} {} in {}", self.street, self.number, self.city),
            _ => format!("{} {}, {}", self.street, self.number, self.city),
        }
    }
}

pub struct Ctx {
    pub conf: &'static crate::vocab::DbConf,
    pub addresses: Vec<Addr>,
    /// The map features (map_features.csv), if given.
    pub tags: std::sync::Arc<Vec<crate::vocab::Tag>>,
    pub districts: Vec<String>,
    /// Districts, towns and the city.
    pub places: Vec<String>,
    pub starts: Vec<Start>,
    /// Named objects in the city whose names sound like a kind of thing ("Gymnasium
    /// Petrinum", "Rhododendron-Park"): asked for by their names.
    pub named: Vec<String>,
    pub first_year: i32,
    pub last_year: i32,
}

/// A small random generator (xorshift*), so that the data is the same for the same seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Whether a name belongs to the held-out part (places and starts).
pub fn held_name(name: &str) -> bool {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h % 5 == 0
}

/// A place as a region in a query: plain when it is a plain name, else quoted.
pub fn region(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain { name.to_string() } else { format!("'{name}'") }
}

pub struct Gen<'a> {
    pub rng: Rng,
    pub ctx: &'a Ctx,
    /// Questions for the evaluation: held-out places, starts and (often) things.
    pub held: bool,
}

impl<'a> Gen<'a> {
    fn lang(&mut self) -> Lang {
        if self.rng.chance(0.5) { Lang::De } else { Lang::En }
    }

    fn thing_from(&mut self, from: &'static [Thing], ok: &dyn Fn(&Thing) -> bool) -> &'static Thing {
        let held = self.held && self.rng.chance(0.5);
        let xs: Vec<&'static Thing> = from.iter().filter(|t| ok(t) && (!t.held_out() || held) && (!held || t.held_out())).collect();
        let xs = if xs.is_empty() { from.iter().filter(|t| ok(t) && (self.held || !t.held_out())).collect() } else { xs };
        xs[self.rng.below(xs.len())]
    }

    fn thing(&mut self) -> &'static Thing {
        self.thing_from(THINGS, &|_| true)
    }

    fn names(&mut self, xs: &[String]) -> Option<String> {
        let held = self.held;
        let ys: Vec<&String> = xs.iter().filter(|n| held_name(n) == held).collect();
        if ys.is_empty() { None } else { Some(ys[self.rng.below(ys.len())].clone()) }
    }

    fn place(&mut self) -> Option<String> {
        let ps = self.ctx.places.clone();
        self.names(&ps)
    }

    fn district(&mut self) -> Option<String> {
        let ds = self.ctx.districts.clone();
        self.names(&ds)
    }

    fn start(&mut self) -> Option<&'a Start> {
        let held = self.held;
        let ctx: &'a Ctx = self.ctx;
        let ys: Vec<&'a Start> = ctx.starts.iter().filter(|s| held_name(&s.name) == held).collect();
        if ys.is_empty() { None } else { Some(ys[self.rng.below(ys.len())]) }
    }

    fn year(&mut self) -> i32 {
        let (a, b) = (self.ctx.first_year.max(2010), self.ctx.last_year - 2);
        a + self.rng.below((b - a + 1).max(1) as usize) as i32
    }

    /// One of the phrasings, in the language, sometimes written casually (lower case, no
    /// final sign), as people type into a search field.
    fn phrase(&mut self, lang: Lang, en: &[&str], de: &[&str], fill: &[(&str, &str)]) -> String {
        let mut s = self.rng.pick(if lang == Lang::En { en } else { de }).to_string();
        for (k, v) in fill {
            s = s.replace(&format!("{{{k}}}"), v);
        }
        if self.rng.chance(0.15) {
            s = s.to_lowercase().trim_end_matches(['?', '.', '!']).to_string();
        }
        s
    }

    fn mode(&mut self) -> &'static Mode {
        self.rng.pick(MODES)
    }
}

fn pl(t: &Thing, lang: Lang) -> &'static str {
    if lang == Lang::En { t.en[1] } else { t.de[1] }
}

fn sg(t: &Thing, lang: Lang) -> &'static str {
    if lang == Lang::En { t.en[0] } else { t.de[0] }
}

fn says(lang: Lang, en: String, de: String) -> String {
    if lang == Lang::En { en } else { de }
}

// ── reading the results ──

fn int(o: &CollectedOutput) -> Option<i64> {
    o.value.as_i64().or_else(|| o.value.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
}

/// A measure: (value, unit).
fn qty(o: &CollectedOutput) -> Option<(f64, String)> {
    let v = &o.value;
    Some((v["value"].as_f64()?, v["unit"].as_str()?.to_string()))
}

fn metres(o: &CollectedOutput) -> Option<f64> {
    let (v, u) = qty(o)?;
    match u.as_str() {
        "m" => Some(v),
        "km" => Some(v * 1000.0),
        _ => None,
    }
}

fn square_metres(o: &CollectedOutput) -> Option<f64> {
    let (v, u) = qty(o)?;
    match u.as_str() {
        "m2" | "m²" => Some(v),
        "km2" | "km²" => Some(v * 1e6),
        "ha" => Some(v * 1e4),
        _ => None,
    }
}

fn minutes(o: &CollectedOutput) -> Option<f64> {
    let (v, u) = qty(o)?;
    match u.as_str() {
        "min" => Some(v),
        "s" => Some(v / 60.0),
        "h" => Some(v * 60.0),
        _ => None,
    }
}

fn dec(s: String, lang: Lang) -> String {
    if lang == Lang::De { s.replace('.', ",") } else { s }
}

fn fmt_len(m: f64, lang: Lang) -> String {
    dec(if m >= 1000.0 { format!("{:.1} km", m / 1000.0) } else { format!("{:.0} m", m) }, lang)
}

fn fmt_area(m2: f64, lang: Lang) -> String {
    dec(
        if m2 >= 1e6 {
            format!("{:.2} km²", m2 / 1e6)
        } else if m2 >= 1e4 {
            format!("{:.1} ha", m2 / 1e4)
        } else {
            format!("{:.0} m²", m2)
        },
        lang,
    )
}

fn fmt_min(min: f64, lang: Lang) -> String {
    let n = min.round().max(1.0) as i64;
    match lang {
        Lang::En => format!("about {n} minute{}", if n == 1 { "" } else { "s" }),
        Lang::De => format!("etwa {n} Minute{}", if n == 1 { "" } else { "n" }),
    }
}

/// The number of elements of a set.
fn n_set(o: &CollectedOutput) -> Option<usize> {
    if let Some(a) = o.value.as_array() {
        return Some(a.len());
    }
    o.text.split_whitespace().next()?.parse().ok()
}

/// Rows of a grouped result: (name of the group, count).
fn groups(o: &CollectedOutput) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for row in o.value.as_array().into_iter().flatten() {
        let Some(obj) = row.as_object() else { continue };
        let Some(count) = obj.get("count").and_then(|c| c.as_i64()) else { continue };
        let Some((_, key)) = obj.iter().find(|(k, _)| *k != "count") else { continue };
        let name = match key {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(_) => match key["tags"]["name"].as_str() {
                Some(n) => n.to_string(),
                None => continue,
            },
            other => other.to_string(),
        };
        out.push((name, count));
    }
    out
}

/// The largest groups, if all of them can be seen in the results.
fn top_groups(o: &CollectedOutput, results: &str, n: usize) -> Option<Vec<(String, i64)>> {
    let mut g = groups(o);
    g.retain(|x| x.1 > 0);
    g.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    g.truncate(n);
    let distinct = g.windows(2).all(|w| w[0].1 != w[1].1);
    (!g.is_empty() && distinct && g.iter().all(|(k, c)| results.contains(k.as_str()) && results.contains(&c.to_string()))).then_some(g)
}

fn list(items: &[String], lang: Lang) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        n => format!("{} {} {}", items[..n - 1].join(", "), if lang == Lang::En { "and" } else { "und" }, items[n - 1]),
    }
}

// ── the templates ──

pub type Template = fn(&mut Gen) -> Option<Item>;

/// The templates with their weights (how many questions of each kind).
pub const TEMPLATES: &[(&str, usize, Template)] = &[
    ("count_all", 4, count_all),
    ("count_place", 10, count_place),
    ("show_place", 6, show_place),
    ("pick_one", 3, pick_one),
    ("named_where", 4, named_where),
    ("per_area", 3, per_area),
    ("follow_show", 3, follow_show),
    ("hist_revert", 4, hist_revert),
    ("hist_renamed", 2, hist_renamed),
    ("hist_changed", 2, hist_changed),
    ("hist_versions", 2, hist_versions),
    ("hist_who", 2, hist_who),
    ("which_place", 8, which_place),
    ("trend", 5, trend),
    ("over_the_years", 4, over_the_years),
    ("at_date", 3, at_date),
    ("per_district", 4, per_district),
    ("route_nearest", 6, route_nearest),
    ("route_nearest_ask", 4, route_nearest_ask),
    ("route_between", 3, route_between),
    ("isochrone_show", 3, isochrone_show),
    ("isochrone_count", 4, isochrone_count),
    ("isochrone_ask_mode", 2, isochrone_ask_mode),
    ("isochrone_ask_start", 2, isochrone_ask_start),
    ("near_some", 4, near_some),
    ("near_no", 3, near_no),
    ("length", 4, length),
    ("area_total", 2, area_total),
    ("largest", 3, largest),
    ("attribute", 5, attribute),
    ("share", 3, share),
    ("new_per_year", 2, new_per_year),
    ("creators", 2, creators),
    ("two_counts", 3, two_counts),
    ("compare_places", 3, compare_places),
    ("compare_years", 4, compare_years),
    ("compare_things", 4, compare_things),
    ("compare_places_map", 2, compare_places_map),
    ("per_value", 3, per_value),
    ("outside", 2, outside),
    ("follow_place", 4, follow_place),
    ("follow_thing", 3, follow_thing),
    ("density", 2, density),
    ("vague", 8, vague),
    ("tag_count", 10, tag_count),
    ("tag_show", 5, tag_show),
    ("geocode_show", 3, geocode_show),
    ("geocode_parts", 3, geocode_parts),
    ("geocode_route", 4, geocode_route),
    ("reverse_point", 4, reverse_point),
    ("near_point", 2, near_point),
    ("place_address", 2, place_address),
    ("per_city", 4, per_city),
    ("in_which", 3, in_which),
];

fn count_all(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let lang = g.lang();
    let q = g.phrase(
        lang,
        &["How many {pl} are there?", "How many {pl} are in the database?", "Count all {pl}.", "number of {pl}"],
        &["Wie viele {pl} gibt es?", "Wie viele {pl} gibt es insgesamt?", "Zähle alle {pl}.", "Anzahl {pl}"],
        &[("pl", pl(t, lang))],
    );
    let p = pl(t, lang);
    Some(Item::query(
        "count_all",
        lang,
        q,
        format!("count of {}", t.sel()),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("There are {n} {p} in the database."), format!("In der Datenbank gibt es {n} {p}.")))
        }),
    ))
}

fn count_place(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let place = g.place()?;
    let lang = g.lang();
    let q = g.phrase(
        lang,
        &["How many {pl} are there in {P}?", "How many {pl} does {P} have?", "Count the {pl} in {P}.", "{pl} in {P}: how many?", "number of {pl} in {P}"],
        &["Wie viele {pl} gibt es in {P}?", "Wie viele {pl} hat {P}?", "Zähle die {pl} in {P}.", "Anzahl der {pl} in {P}", "Wie viele {pl} stehen in {P}?"],
        &[("pl", pl(t, lang)), ("P", &place)],
    );
    let p = pl(t, lang);
    Some(Item::query(
        "count_place",
        lang,
        q,
        format!("count of {} within {}", t.sel(), region(&place)),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("There are {n} {p} in {place}."), format!("In {place} gibt es {n} {p}.")))
        }),
    ))
}

/// "Which parks are in Bremen?": the elements themselves (a map), never a question back or a
/// count.
fn which_place(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let place = g.place()?;
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(
        lang,
        &["Which {pl} are in {P}?", "Which {pl} are there in {P}?", "What {pl} are there in {P}?", "{pl} in {P}?", "Are there {pl} in {P}?", "which {pl} does {P} have"],
        &["Welche {pl} gibt es in {P}?", "Welche {pl} liegen in {P}?", "Was für {pl} gibt es in {P}?", "{pl} in {P}?", "Gibt es {pl} in {P}?", "welche {pl} hat {P}"],
        &[("pl", p), ("P", &place)],
    );
    let mut it = Item::query(
        "which_place",
        lang,
        q,
        format!("{} within {}", t.sel(), region(&place)),
        Box::new(move |o, _| {
            let n = n_set(o.first()?)?;
            Some(match n {
                0 => says(lang, format!("There are no {p} in {place}."), format!("In {place} gibt es keine {p}.")),
                _ => says(lang, format!("The map shows {n} {p} in {place}."), format!("Die Karte zeigt {n} {p} in {place}.")),
            })
        }),
    );
    // "none there" is an answer too
    it.zero_ok = g.rng.chance(0.2);
    Some(it)
}

/// "How has the number of parks changed over the years?": one value per year over the whole
/// time of the data (a line).
fn over_the_years(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let (first, last) = (g.ctx.first_year.max(2007) + 1, g.ctx.last_year);
    let lang = g.lang();
    let place = if g.rng.chance(0.5) { Some(g.place()?) } else { None };
    let p = pl(t, lang);
    let in_p = place.as_ref().map(|x| format!(" in {x}")).unwrap_or_default();
    let q = g.phrase(
        lang,
        &["How has the number of {pl}{in} changed over the years?", "How many {pl} were there{in} over the years?", "{pl}{in} over time", "Show the number of {pl}{in} year by year."],
        &["Wie hat sich die Zahl der {pl}{in} über die Jahre entwickelt?", "Wie viele {pl} gab es{in} im Laufe der Jahre?", "{pl}{in} im Zeitverlauf", "Zeige die Zahl der {pl}{in} Jahr für Jahr."],
        &[("pl", p), ("in", &in_p)],
    );
    let within = place.as_ref().map(|x| format!(" within {}", region(x))).unwrap_or_default();
    Some(Item::query(
        "over_the_years",
        lang,
        q,
        format!("count of {}{within} on each year from {first} to {last}", t.sel()),
        Box::new(move |o, _| series_answer(lang, p, o.first()?, place.as_deref())),
    ))
}

fn show_place(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let place = g.place()?;
    let lang = g.lang();
    let q = g.phrase(
        lang,
        &["Show me all {pl} in {P}.", "Where are the {pl} in {P}?", "Map of the {pl} in {P}", "List the {pl} in {P}.", "show {pl} in {P}"],
        &["Zeige mir alle {pl} in {P}.", "Wo gibt es {pl} in {P}?", "Karte der {pl} in {P}", "Liste die {pl} in {P} auf.", "{pl} in {P} anzeigen"],
        &[("pl", pl(t, lang)), ("P", &place)],
    );
    let p = pl(t, lang);
    Some(Item::query(
        "show_place",
        lang,
        q,
        format!("{} within {}", t.sel(), region(&place)),
        Box::new(move |o, _| {
            let n = n_set(o.first()?)?;
            (n >= 2).then(|| says(lang, format!("The map shows {n} {p} in {place}."), format!("Die Karte zeigt {n} {p} in {place}.")))
        }),
    ))
}

// ── history: reverts, edit wars, renamings, versions ──

/// The tags of streets whose changes are asked about: key, English, German.
const HIST_KEYS: &[(&str, &str, &str)] = &[
    ("name", "name", "Namen"),
    ("highway", "road class", "Straßenklasse"),
    ("maxspeed", "speed limit", "Höchstgeschwindigkeit"),
    ("surface", "surface", "Oberfläche"),
];

/// The answer for a count or a set of cases.
fn cases_answer(lang: Lang, o: &CollectedOutput, count: bool, place: &str) -> Option<String> {
    let n = if count { int(o)? as usize } else { n_set(o)? };
    Some(match (n, count) {
        (0, _) => says(lang, format!("There are no such cases in {place}."), format!("In {place} gibt es keine solchen Fälle.")),
        (n, true) => says(lang, format!("There were {n} such cases in {place}."), format!("In {place} gab es {n} solche Fälle.")),
        (n, false) => says(lang, format!("The map shows {n} cases in {place}."), format!("Die Karte zeigt {n} Fälle in {place}.")),
    })
}

/// "A changed the name of a street, B changed it, A changed it back" (an edit war), or a
/// change undone within a few days (a quick revert); as cases on the map or counted.
fn hist_revert(g: &mut Gen) -> Option<Item> {
    let &(k, en, de) = g.rng.pick(HIST_KEYS);
    let place = g.place()?;
    let lang = g.lang();
    let count = g.rng.chance(0.4);
    let war = g.rng.chance(0.6);
    let days = *g.rng.pick(&[1, 3, 7, 30]);
    let r = region(&place);
    let (q, query) = if war {
        let q = if count {
            g.phrase(
                lang,
                &["How many edit wars about the {k} of streets were there in {P}?", "How often did someone change the {k} of a street in {P}, and the mapper before changed it back?"],
                &["Wie viele Edit-Wars um die {k} von Straßen gab es in {P}?", "Wie oft hat jemand in {P} die {k} einer Straße geändert und die Person davor hat sie zurückgeändert?"],
                &[("k", if lang == Lang::En { en } else { de }), ("P", &place)],
            )
        } else {
            g.phrase(
                lang,
                &[
                    "Show me cases in {P} where a person A has given a street its {k}, person B has changed it, and person A has reverted it thereafter.",
                    "edit wars about the {k} of streets in {P}",
                    "Where in {P} was the {k} of a street changed by one mapper and restored by the one before?",
                ],
                &[
                    "Zeig mir Fälle in {P}, in denen Person A einer Straße ihre {k} gegeben hat, Person B sie geändert hat und Person A sie danach zurückgesetzt hat.",
                    "Edit-Wars um die {k} von Straßen in {P}",
                    "Wo in {P} wurde die {k} einer Straße von jemandem geändert und von der Person davor wiederhergestellt?",
                ],
                &[("k", if lang == Lang::En { en } else { de }), ("P", &place)],
            )
        };
        (q, format!("way A -> way B -> way C with A.user == C.user and B.user != A.user and A:{k} == C:{k} and A:{k} != B:{k} and A:highway within {r}"))
    } else {
        let d = days.to_string();
        let q = if count {
            g.phrase(
                lang,
                &["How often was the {k} of a street in {P} changed and changed back within {d} days?", "number of quick reverts of the {k} of streets in {P} (within {d} days)"],
                &["Wie oft wurde in {P} die {k} einer Straße geändert und innerhalb von {d} Tagen zurückgeändert?", "Anzahl schneller Reverts der {k} von Straßen in {P} (innerhalb von {d} Tagen)"],
                &[("k", if lang == Lang::En { en } else { de }), ("P", &place), ("d", &d)],
            )
        } else {
            g.phrase(
                lang,
                &["Where in {P} was the {k} of a street changed and changed back within {d} days?", "quick reverts of the {k} of streets in {P} within {d} days"],
                &["Wo in {P} wurde die {k} einer Straße geändert und innerhalb von {d} Tagen zurückgeändert?", "schnelle Reverts der {k} von Straßen in {P} innerhalb von {d} Tagen"],
                &[("k", if lang == Lang::En { en } else { de }), ("P", &place), ("d", &d)],
            )
        };
        (q, format!("way A -> way B -> way C closer than {days} days with A:{k} == C:{k} and A:{k} != B:{k} and A:highway within {r}"))
    };
    let query = if count { format!("count of {query}") } else { query };
    let mut it = Item::query("hist_revert", lang, q, query, Box::new(move |o, _| cases_answer(lang, o.first()?, count, &place)));
    it.zero_ok = true;
    Some(it)
}

/// "Which streets in Findorff were renamed?": two consecutive versions with different names.
fn hist_renamed(g: &mut Gen) -> Option<Item> {
    let place = g.place()?;
    let lang = g.lang();
    let count = g.rng.chance(0.4);
    let q = if count {
        g.phrase(lang, &["How many times were streets renamed in {P}?", "number of street renamings in {P}"], &["Wie oft wurden in {P} Straßen umbenannt?", "Anzahl der Umbenennungen von Straßen in {P}"], &[("P", &place)])
    } else {
        g.phrase(lang, &["Which streets in {P} were renamed?", "renamed streets in {P}", "Show me the streets in {P} whose name changed."], &["Welche Straßen in {P} wurden umbenannt?", "umbenannte Straßen in {P}", "Zeig mir die Straßen in {P}, deren Name sich geändert hat."], &[("P", &place)])
    };
    let sel = format!("way A -> way B with A:name != B:name and B:highway within {}", region(&place));
    let query = if count { format!("count of {sel}") } else { sel };
    let mut it = Item::query("hist_renamed", lang, q, query, Box::new(move |o, _| cases_answer(lang, o.first()?, count, &place)));
    it.zero_ok = true;
    Some(it)
}

/// "In how many streets in Walle did the latest edit change the speed limit?"
fn hist_changed(g: &mut Gen) -> Option<Item> {
    let &(k, en, de) = g.rng.pick(&HIST_KEYS[1..]);
    let place = g.place()?;
    let lang = g.lang();
    let q = g.phrase(
        lang,
        &["In how many streets in {P} did the latest edit change the {k}?", "How many streets in {P} had their {k} changed in their last edit?"],
        &["Bei wie vielen Straßen in {P} hat die letzte Bearbeitung die {k} geändert?", "Wie viele Straßen in {P} haben bei ihrer letzten Bearbeitung eine andere {k} bekommen?"],
        &[("k", if lang == Lang::En { en } else { de }), ("P", &place)],
    );
    let mut it = Item::query(
        "hist_changed",
        lang,
        q,
        format!("count of ways w with w:highway and w.previous:{k} != w:{k} within {}", region(&place)),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("In {place}, the latest edit changed the {en} of {n} streets."), format!("In {place} hat die letzte Bearbeitung bei {n} Straßen die {de} geändert.")))
        }),
    );
    it.zero_ok = true;
    Some(it)
}

/// "How often has Bremen Hauptbahnhof been edited?": its versions.
fn hist_versions(g: &mut Gen) -> Option<Item> {
    let s = g.start()?;
    let (name, pick) = (s.name.clone(), s.pick.clone());
    let lang = g.lang();
    let q = g.phrase(lang, &["How often has {S} been edited?", "How many versions does {S} have?"], &["Wie oft wurde {S} bearbeitet?", "Wie viele Versionen hat {S}?"], &[("S", &name)]);
    Some(Item::query(
        "hist_versions",
        lang,
        q,
        format!("count of ({pick}).versions"),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            (n >= 1).then(|| says(lang, format!("{name} has {n} versions."), format!("{name} hat {n} Versionen.")))
        }),
    ))
}

/// "Who was involved in edit wars about speed limits in Bremen?": the mappers at position B.
fn hist_who(g: &mut Gen) -> Option<Item> {
    let &(k, en, de) = g.rng.pick(HIST_KEYS);
    let place = g.place()?;
    let lang = g.lang();
    let q = g.phrase(
        lang,
        &["Who was involved in edit wars about the {k} of streets in {P}?", "Which mappers changed the {k} of a street in {P} that was then changed back?"],
        &["Wer war an Edit-Wars um die {k} von Straßen in {P} beteiligt?", "Welche Mapper haben in {P} die {k} einer Straße geändert, die dann zurückgeändert wurde?"],
        &[("k", if lang == Lang::En { en } else { de }), ("P", &place)],
    );
    let mut it = Item::query(
        "hist_who",
        lang,
        q,
        format!(
            "war = way A -> way B -> way C with A.user == C.user and B.user != A.user and A:{k} == C:{k} and A:{k} != B:{k} and A:highway within {}\ndistinct of war_B.map(b => b.user)",
            region(&place)
        ),
        Box::new(move |o, _| {
            let n = o.first()?.value.as_array()?.len();
            Some(match n {
                0 => says(lang, format!("Nobody: there were no such edit wars in {place}."), format!("Niemand: In {place} gab es keine solchen Edit-Wars.")),
                n => says(lang, format!("{n} mappers were involved in {place}."), format!("In {place} waren {n} Mapper beteiligt.")),
            })
        }),
    );
    it.zero_ok = true;
    Some(it)
}

/// "How many benches are there per park in Vegesack?": counted per area of a kind.
fn per_area(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &|t| t.kind == crate::vocab::Kind::Nodes);
    let place = g.place()?;
    let lang = g.lang();
    let (cond, en_sg, de_sg, en_pl, de_pl) = *g.rng.pick(&[
        ("d:leisure=park", "park", "Park", "parks", "Parks"),
        ("d:leisure=playground", "playground", "Spielplatz", "playgrounds", "Spielplätze"),
        ("d:landuse=cemetery", "cemetery", "Friedhof", "cemeteries", "Friedhöfe"),
        ("d:amenity=school", "school", "Schule", "schools", "Schulen"),
    ]);
    let q = g.phrase(
        lang,
        &["How many {pl} are there per {a} in {P}?", "Number of {pl} per {a} in {P}", "{pl} per {a} in {P}", "How many {pl} does each {a} in {P} have?"],
        &["Wie viele {pl} gibt es pro {a} in {P}?", "Anzahl der {pl} je {a} in {P}", "{pl} pro {a} in {P}", "Wie viele {pl} gibt es je {a} in {P}?"],
        &[("pl", pl(t, lang)), ("a", if lang == Lang::En { en_sg } else { de_sg }), ("P", &place)],
    );
    let (p, areas) = (pl(t, lang), if lang == Lang::En { en_pl } else { de_pl });
    Some(Item::query(
        "per_area",
        lang,
        q,
        format!("count of {} per (areas d with {cond} within {})", t.sel(), region(&place)),
        Box::new(move |o, _| {
            let gs = groups(o.first()?);
            let (name, n) = gs.iter().max_by_key(|g| g.1)?;
            (*n >= 2).then(|| {
                says(lang, format!("Of the {areas} in {place}, {name} has the most {p} ({n})."), format!("Von den {areas} in {place} hat {name} die meisten {p} ({n})."))
            })
        }),
    ))
}

/// "Where is Gymnasium Petrinum in Dorsten?": an object by its name (not the kind of thing
/// its name sounds like), within the town if one is named.
fn named_where(g: &mut Gen) -> Option<Item> {
    // a station as people call it ("where is Bremen Hbf?"): the station by its name in the data
    if g.rng.chance(0.25) {
        let s = g.start()?;
        let (en, pick, name) = (s.en.clone(), s.pick.clone(), s.name.clone());
        if en.starts_with("the ") || !pick.contains("railway=station") {
            return None;
        }
        let q = g.phrase(Lang::En, &["Where is {S}?", "where is {S}", "Show me {S} on the map."], &[], &[("S", &en)]);
        return Some(Item::query(
            "named_where",
            Lang::En,
            q,
            pick,
            Box::new(move |o, _| o.first().filter(|o| !o.value.is_null()).map(|_| format!("The map shows {name}."))),
        ));
    }
    let name = g.names(&g.ctx.named.clone())?;
    let city = g.ctx.conf.city.to_string();
    let lang = g.lang();
    let in_town = g.rng.chance(0.8);
    let q = if in_town {
        g.phrase(
            lang,
            &["Where is {N} in {P}?", "Where is \"{N}\" in {P}?", "Show me {N} in {P}.", "Where can I find {N} in {P}?", "{N} in {P}"],
            &["Wo ist {N} in {P}?", "Wo ist \"{N}\" in {P}?", "Zeig mir {N} in {P}.", "Wo finde ich {N} in {P}?", "{N} in {P}"],
            &[("N", &name), ("P", &city)],
        )
    } else {
        g.phrase(lang, &["Where is {N}?", "Show me \"{N}\" on the map."], &["Wo liegt {N}?", "Zeig mir \"{N}\" auf der Karte."], &[("N", &name)])
    };
    let query = if in_town { format!("* x with x:name='{name}' within {}", region(&city)) } else { format!("* x with x:name='{name}'") };
    Some(Item::query(
        "named_where",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            (n_set(o.first()?)? >= 1).then(|| says(lang, format!("The map shows {name}."), format!("Die Karte zeigt {name}.")))
        }),
    ))
}

/// "Provide exactly one library in Bremen", "Pick a bakery in Findorff": `pick`.
fn pick_one(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let place = g.place()?;
    let lang = g.lang();
    let a = if t.en[0].starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    let (ein, eine) = match t.g {
        crate::vocab::Gender::M => ("einen", "ein"),
        crate::vocab::Gender::F => ("eine", "eine"),
        crate::vocab::Gender::N => ("ein", "ein"),
    };
    let q = g.phrase(
        lang,
        &["Provide exactly one {sg} in {P}.", "Pick {a} {sg} in {P}.", "Show me one {sg} in {P}.", "Give me a single {sg} in {P}.", "just one {sg} in {P}", "any {sg} in {P}, one is enough"],
        &["Zeige mir genau {ein} {sg} in {P}.", "Such {ein} {sg} in {P} aus.", "Nenne mir {ein} {sg} in {P}.", "nur {ein} {sg} in {P}"],
        &[("sg", sg(t, lang)), ("P", &place), ("a", a), ("ein", ein)],
    );
    let s = sg(t, lang);
    Some(Item::query(
        "pick_one",
        lang,
        q,
        format!("pick {} within {}", t.sel(), region(&place)),
        Box::new(move |o, _| {
            let v = &o.first()?.value;
            let name = v["tags"]["name"].as_str();
            Some(match name {
                Some(n) => says(lang, format!("Here is one {s} in {place}: {n}."), format!("Hier ist {eine} {s} in {place}: {n}.")),
                None => says(lang, format!("The map shows one {s} in {place}."), format!("Die Karte zeigt {eine} {s} in {place}.")),
            })
        }),
    ))
}

/// Answer for a time series `[{count, year}]`: the first and the last value.
fn series_answer(lang: Lang, p: &'static str, o: &CollectedOutput, place: Option<&str>) -> Option<String> {
    let rows = o.value.as_array()?;
    let (a, b) = (rows.first()?, rows.last()?);
    let (ya, na, yb, nb) = (a["year"].as_str()?, a["count"].as_i64()?, b["year"].as_str()?, b["count"].as_i64()?);
    if ya == yb {
        return None;
    }
    let (en_in, de_in) = match place {
        Some(pl) => (format!(" in {pl}"), format!(" in {pl}")),
        None => (String::new(), String::new()),
    };
    let (en_trend, de_trend) = if nb > na {
        ("it has grown", "die Zahl ist gestiegen")
    } else if nb < na {
        ("it has fallen", "die Zahl ist gesunken")
    } else {
        ("it has stayed the same", "die Zahl ist gleich geblieben")
    };
    Some(says(
        lang,
        format!("At the start of {ya} there were {na} {p}{en_in}, at the start of {yb} {nb}: {en_trend}."),
        format!("Anfang {ya} gab es{de_in} {na} {p}, Anfang {yb} {nb}: {de_trend}."),
    ))
}

fn trend(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let y = g.year();
    let last = g.ctx.last_year;
    let lang = g.lang();
    let place = if g.rng.chance(0.4) { Some(g.place()?) } else { None };
    let ys = y.to_string();
    let p = pl(t, lang);
    let mut q = g.phrase(
        lang,
        &["How has the number of {pl} changed since {y}?", "Show the development of the {pl} since {y}.", "How many {pl} were there each year since {y}?", "number of {pl} every year since {y}"],
        &["Wie hat sich die Zahl der {pl} seit {y} entwickelt?", "Entwicklung der {pl} seit {y}", "Wie viele {pl} gab es jedes Jahr seit {y}?", "Anzahl der {pl} jedes Jahr seit {y}"],
        &[("pl", p), ("y", &ys)],
    );
    let within = match &place {
        Some(pl) => {
            q = format!("{} in {pl}{}", q.trim_end_matches(['?', '.']), if q.ends_with('?') { "?" } else { "" });
            format!(" within {}", region(pl))
        }
        None => String::new(),
    };
    Some(Item::query(
        "trend",
        lang,
        q,
        format!("count of {}{within} on each year from {y} to {last}", t.sel()),
        Box::new(move |o, _| series_answer(lang, p, o.first()?, place.as_deref())),
    ))
}

fn at_date(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let y = g.year();
    let lang = g.lang();
    let p = pl(t, lang);
    let ys = y.to_string();
    let mid = g.rng.chance(0.3);
    let (q, date) = if mid {
        (g.phrase(lang, &["How many {pl} were there in mid-{y}?", "number of {pl} in the middle of {y}"], &["Wie viele {pl} gab es Mitte {y}?", "Anzahl der {pl} Mitte {y}"], &[("pl", p), ("y", &ys)]), format!("{y}-07-01"))
    } else {
        (
            g.phrase(
                lang,
                &["How many {pl} were there at the start of {y}?", "How many {pl} existed on 1 January {y}?", "number of {pl} at the beginning of {y}"],
                &["Wie viele {pl} gab es Anfang {y}?", "Wie viele {pl} gab es am 1. Januar {y}?", "Anzahl der {pl} zu Beginn von {y}"],
                &[("pl", p), ("y", &ys)],
            ),
            format!("{y}-01-01"),
        )
    };
    Some(Item::query(
        "at_date",
        lang,
        q,
        format!("count of {} on {date}", t.sel()),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(if mid {
                says(lang, format!("In mid-{y} there were {n} {p}."), format!("Mitte {y} gab es {n} {p}."))
            } else {
                says(lang, format!("At the start of {y} there were {n} {p}."), format!("Anfang {y} gab es {n} {p}."))
            })
        }),
    ))
}

fn per_district(g: &mut Gen) -> Option<Item> {
    if g.ctx.districts.is_empty() {
        return None;
    }
    let t = g.thing();
    let lang = g.lang();
    let conf = g.ctx.conf;
    let (d, ds) = if lang == Lang::En { (conf.district_en[0], conf.district_en[1]) } else { (conf.district_de[0], conf.district_de[1]) };
    let p = pl(t, lang);
    let most = g.rng.chance(0.4);
    let q = if most {
        g.phrase(lang, &["Which {d} has the most {pl}?", "In which {d} are there the most {pl}?"], &["Welcher {d} hat die meisten {pl}?", "In welchem {d} gibt es die meisten {pl}?"], &[("pl", p), ("d", d)])
    } else {
        g.phrase(
            lang,
            &["How many {pl} are there per {d}?", "Number of {pl} in each {d}", "{pl} by {d}"],
            &["Wie viele {pl} gibt es pro {d}?", "Anzahl der {pl} je {d}", "{pl} nach {ds}"],
            // after "nach" the dative: Stadtteilen, Bezirken
            &[("pl", p), ("d", d), ("ds", &if lang == Lang::De && !ds.ends_with('n') { format!("{ds}n") } else { ds.to_string() })],
        )
    };
    let city = conf.city;
    let within = if g.ctx.districts.len() > 1 { format!(" within {}", region(city)) } else { String::new() };
    Some(Item::query(
        "per_district",
        lang,
        q,
        // the districts by name (admin level 10: x.suburb, 9: x.district), as the chart shows them
        format!("count of {}{within} per {}.{}", t.sel(), t.var(), if conf.district_level == "10" { "suburb" } else { "district" }),
        Box::new(move |o, results| {
            let top = top_groups(o.first()?, results, 3)?;
            let parts: Vec<String> = top.iter().map(|(k, c)| format!("{k} ({c})")).collect();
            Some(says(lang, format!("Most {p} are in {}.", list(&parts, lang)), format!("Die meisten {p} gibt es in {}.", list(&parts, lang))))
        }),
    ))
}

/// The query for the way from a start to the nearest thing.
fn nearest_route_query(s: &Start, t: &Thing, mode: &str) -> String {
    format!("s = {}\ndest = nearest of ({}) to s\np = path P from s to dest by {mode}\ndest:name, p.length, p.duration\np", s.pick, t.sel())
}

fn route_answer(lang: Lang, t: &'static Thing, mode: &'static Mode) -> AnswerFn {
    Box::new(move |o, _| {
        let name = o.first()?.value.as_str().filter(|n| *n != "none").map(str::to_string);
        let m = metres(o.get(1)?)?;
        let min = minutes(o.get(2)?)?;
        let s = sg(t, lang);
        let named = name.map(|n| format!(" ({n})")).unwrap_or_default();
        Some(says(
            lang,
            format!("The nearest {s}{named} is {} away; that takes {} {}.", fmt_len(m, lang), fmt_min(min, lang), mode.en[0]),
            format!("{} nächsten {s}{named} sind es {}, {} {}.", if t.g == crate::vocab::Gender::F { "Bis zur" } else { "Bis zum" }, fmt_len(m, lang), fmt_min(min, lang), mode.de[0]),
        ))
    })
}

fn route_question(g: &mut Gen, lang: Lang, s: &Start, t: &Thing, mode: Option<&Mode>) -> String {
    let s_ = sg(t, lang);
    match (lang, mode) {
        (Lang::En, Some(m)) => {
            let me = *g.rng.pick(m.en);
            g.phrase(
                lang,
                &["How long does it take to {verb} from {S} to the nearest {sg}?", "Route {mode} from {S} to the nearest {sg}", "How far is it {mode} from {S} to the next {sg}?"],
                &[],
                &[("verb", m.en_verb), ("S", &s.en), ("sg", s_), ("mode", me)],
            )
        }
        (Lang::De, Some(m)) => {
            let md = *g.rng.pick(m.de);
            g.phrase(
                lang,
                &[],
                &["Wie lange brauche ich {mode} {von} {zum} nächsten {sg}?", "Weg {mode} {von} {zum} nächsten {sg}", "Wie weit ist es {mode} {von} bis {zum} nächsten {sg}?"],
                &[("mode", md), ("von", &s.de_von), ("zum", t.de_zum()), ("sg", s_)],
            )
        }
        (Lang::En, None) => g.phrase(
            lang,
            &["How long does it take from {S} to the nearest {sg}?", "How do I get from {S} to the nearest {sg}?", "Route from {S} to the nearest {sg}"],
            &[],
            &[("S", &s.en), ("sg", s_)],
        ),
        (Lang::De, None) => g.phrase(
            lang,
            &[],
            &["Wie lange brauche ich {von} {zum} nächsten {sg}?", "Wie komme ich {von} {zum} nächsten {sg}?", "Weg {von} {zum} nächsten {sg}"],
            &[("von", &s.de_von), ("zum", t.de_zum()), ("sg", s_)],
        ),
    }
}

/// Things one goes to.
fn destination(t: &Thing) -> bool {
    t.poi || ["toilets", "drinking_water", "playground", "bus_stop", "atm", "post_box", "bicycle_parking", "charging_station", "=parking", "=park", "=bench"].iter().any(|k| t.cond.contains(k))
}

fn route_nearest(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &destination);
    let s = g.start()?;
    let mode = g.mode();
    let lang = g.lang();
    let q = route_question(g, lang, s, t, Some(mode));
    Some(Item::query("route_nearest", lang, q, nearest_route_query(s, t, mode.key), route_answer(lang, t, mode)))
}

/// The question back for the way of moving.
fn ask_mode(lang: Lang) -> String {
    says(lang, "On foot, by bicycle or by car?".into(), "Zu Fuß, mit dem Fahrrad oder mit dem Auto?".into())
}

/// Without the way of moving: the question back, and (as a second turn) the query after the
/// reply.
fn route_nearest_ask(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &destination);
    let s = g.start()?;
    let mode = g.mode();
    let lang = g.lang();
    let q = route_question(g, lang, s, t, None);
    if g.rng.chance(0.4) {
        return Some(Item::ask("route_nearest_ask", lang, q, ask_mode(lang)));
    }
    let reply = g.rng.pick(if lang == Lang::En { mode.en_reply } else { mode.de_reply }).to_string();
    let mut it = Item::query("route_nearest_ask", lang, reply, nearest_route_query(s, t, mode.key), route_answer(lang, t, mode));
    it.turns = vec![("user".into(), q), ("assistant".into(), format!("ASK: {}", ask_mode(lang))), it.turns.remove(0)];
    Some(it)
}

fn route_between(g: &mut Gen) -> Option<Item> {
    let a = g.start()?;
    let b = g.start()?;
    if a.name == b.name {
        return None;
    }
    let mode = g.mode();
    let lang = g.lang();
    let q = if lang == Lang::En {
        let me = *g.rng.pick(mode.en);
        g.phrase(
            lang,
            &["How long does it take to {verb} from {A} to {B}?", "Route {mode} from {A} to {B}", "How far is it from {A} to {B} {mode}?"],
            &[],
            &[("verb", mode.en_verb), ("A", &a.en), ("B", &b.en), ("mode", me)],
        )
    } else {
        let md = *g.rng.pick(mode.de);
        g.phrase(
            lang,
            &[],
            &["Wie lange brauche ich {mode} {von} {bis}?", "Weg {mode} {von} {bis}", "Wie weit ist es {von} {bis} {mode}?"],
            &[("mode", md), ("von", &a.de_von), ("bis", &b.de_bis)],
        )
    };
    let query = format!("a = {}\nb = {}\np = path P from a to b by {}\np.length, p.duration\np", a.pick, b.pick, mode.key);
    Some(Item::query(
        "route_between",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let m = metres(o.first()?)?;
            let min = minutes(o.get(1)?)?;
            Some(says(lang, format!("The way is {} long and takes {} {}.", fmt_len(m, lang), fmt_min(min, lang), mode.en[0]), format!("Der Weg ist {} lang und dauert {} {}.", fmt_len(m, lang), fmt_min(min, lang), mode.de[0])))
        }),
    ))
}

fn isochrone_show(g: &mut Gen) -> Option<Item> {
    let s = g.start()?;
    let mode = g.mode();
    let lang = g.lang();
    let km = g.rng.chance(0.2);
    let (amount, limit) = if km {
        let k = *g.rng.pick(&[1u32, 2, 3, 5]);
        (says(lang, format!("{k} km"), format!("{k} km")), format!("{k} km"))
    } else {
        let m = *g.rng.pick(mode.minutes);
        (says(lang, format!("{m} minutes"), format!("{m} Minuten")), format!("{m} min"))
    };
    let q = if lang == Lang::En {
        let me = *g.rng.pick(mode.en);
        g.phrase(
            lang,
            &["What can I reach within {amount} {mode} from {S}?", "Show the area reachable in {amount} {mode} from {S}.", "How far do I get {mode} in {amount} from {S}?"],
            &[],
            &[("amount", &amount), ("mode", me), ("S", &s.en)],
        )
    } else {
        let md = *g.rng.pick(mode.de);
        g.phrase(
            lang,
            &[],
            &["Was erreiche ich {von} aus in {amount} {mode}?", "Zeige die Fläche, die ich {von} aus in {amount} {mode} erreiche.", "Wohin komme ich {von} aus {mode} in {amount}?"],
            &[("amount", &amount), ("mode", md), ("von", &s.de_von)],
        )
    };
    let from = s.en.clone();
    let from_de = s.de_von.clone();
    Some(Item::query(
        "isochrone_show",
        lang,
        q,
        format!("s = {}\nisochrone I from s closer than {limit} by {}", s.pick, mode.key),
        Box::new(move |_, _| {
            Some(says(
                lang,
                format!("The map shows the area that can be reached from {from} within {amount} {}.", mode.en[0]),
                format!("Die Karte zeigt die Fläche, die man {from_de} aus in {amount} {} erreicht.", mode.de[0]),
            ))
        }),
    ))
}

fn isochrone_count_question(g: &mut Gen, lang: Lang, t: &Thing, m: u32, mode: Option<&Mode>, s: Option<&Start>) -> String {
    let p = pl(t, lang);
    let ms = m.to_string();
    match lang {
        Lang::En => {
            let me = mode.map(|x| format!(" {}", g.rng.pick(x.en))).unwrap_or_default();
            let from = s.map(|x| format!(" from {}", x.en)).unwrap_or_default();
            g.phrase(
                lang,
                &["How many {pl} can I reach within {m} minutes{mode}{from}?", "How many {pl} are within {m} minutes{mode}{from}?", "{pl} reachable in {m} minutes{mode}{from}"],
                &[],
                &[("pl", p), ("m", &ms), ("mode", &me), ("from", &from)],
            )
        }
        Lang::De => {
            let md = mode.map(|x| format!(" {}", g.rng.pick(x.de))).unwrap_or_default();
            let from = s.map(|x| format!(" {} aus", x.de_von)).unwrap_or_default();
            g.phrase(
                lang,
                &[],
                &["Wie viele {pl} erreiche ich{from} in {m} Minuten{mode}?", "Wie viele {pl} liegen{from} innerhalb von {m} Minuten{mode}?", "{pl} in {m} Minuten{mode} erreichbar{from}"],
                &[("pl", p), ("m", &ms), ("mode", &md), ("from", &from)],
            )
        }
    }
}

fn isochrone_count_query(s: &Start, t: &Thing, m: u32, mode: &str) -> String {
    format!("s = {}\nreach = isochrone I from s closer than {m} min by {mode}\ncount of {} within reach", s.pick, t.sel())
}

fn isochrone_count_answer(lang: Lang, t: &'static Thing, m: u32, mode: &'static Mode, from_en: String, from_de: String) -> AnswerFn {
    Box::new(move |o, _| {
        let n = int(o.first()?)?;
        let p = pl(t, lang);
        Some(says(
            lang,
            format!("From {from_en}, {n} {p} can be reached within {m} minutes {}.", mode.en[0]),
            format!("{} aus erreicht man in {m} Minuten {} {n} {p}.", capital(&from_de), mode.de[0]),
        ))
    })
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn isochrone_count(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let s = g.start()?;
    let mode = g.mode();
    let m = *g.rng.pick(mode.minutes);
    let lang = g.lang();
    let q = isochrone_count_question(g, lang, t, m, Some(mode), Some(s));
    Some(Item::query("isochrone_count", lang, q, isochrone_count_query(s, t, m, mode.key), isochrone_count_answer(lang, t, m, mode, s.en.clone(), s.de_von.clone())))
}

fn isochrone_ask_mode(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let s = g.start()?;
    let mode = g.mode();
    let m = *g.rng.pick(&[5u32, 10, 15]);
    let lang = g.lang();
    let q = isochrone_count_question(g, lang, t, m, None, Some(s));
    if g.rng.chance(0.4) {
        return Some(Item::ask("isochrone_ask_mode", lang, q, ask_mode(lang)));
    }
    let reply = g.rng.pick(if lang == Lang::En { mode.en_reply } else { mode.de_reply }).to_string();
    let mut it = Item::query("isochrone_ask_mode", lang, reply, isochrone_count_query(s, t, m, mode.key), isochrone_count_answer(lang, t, m, mode, s.en.clone(), s.de_von.clone()));
    it.turns = vec![("user".into(), q), ("assistant".into(), format!("ASK: {}", ask_mode(lang))), it.turns.remove(0)];
    Some(it)
}

fn isochrone_ask_start(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let s = g.start()?;
    let mode = g.mode();
    let m = *g.rng.pick(mode.minutes);
    let lang = g.lang();
    let q = isochrone_count_question(g, lang, t, m, Some(mode), None);
    let ask = says(lang, "From where?".into(), "Von wo aus?".into());
    if g.rng.chance(0.4) {
        return Some(Item::ask("isochrone_ask_start", lang, q, ask));
    }
    let reply = match lang {
        Lang::En => format!("{}{}", if g.rng.chance(0.5) { "from " } else { "" }, s.en),
        Lang::De => if g.rng.chance(0.5) { s.de_von.clone() } else { s.name.clone() },
    };
    let mut it = Item::query("isochrone_ask_start", lang, reply, isochrone_count_query(s, t, m, mode.key), isochrone_count_answer(lang, t, m, mode, s.en.clone(), s.de_von.clone()));
    it.turns = vec![("user".into(), q), ("assistant".into(), format!("ASK: {ask}")), it.turns.remove(0)];
    Some(it)
}

/// Two different things: points (benches, stops, …) and what they are near.
fn two_things(g: &mut Gen) -> (&'static Thing, &'static Thing) {
    let a = g.thing_from(THINGS, &|t| t.kind == crate::vocab::Kind::Nodes || t.cond.contains("bicycle_parking"));
    loop {
        let b = g.thing_from(THINGS, &|t| t.kind != crate::vocab::Kind::Ways && !t.cond.contains("building") && !t.cond.contains("tree"));
        if b.cond != a.cond {
            return (a, b);
        }
    }
}

fn near_some(g: &mut Gen) -> Option<Item> {
    let (a, b) = two_things(g);
    let d = *g.rng.pick(&[50u32, 100, 200, 300, 500]);
    let lang = g.lang();
    let place = if g.rng.chance(0.5) { Some(g.place()?) } else { None };
    let ds = d.to_string();
    let in_p = place.as_ref().map(|p| format!(" in {p}")).unwrap_or_default();
    let q = g.phrase(
        lang,
        &["How many {plA}{in} are within {d} m of {a} {sgB}?", "Count the {plA}{in} less than {d} m from {a} {sgB}.", "{plA}{in} near {a} {sgB} (less than {d} m)"],
        &["Wie viele {plA}{in} sind weniger als {d} m von {einem} {sgB} entfernt?", "Zähle die {plA}{in}, die höchstens {d} m von {einem} {sgB} entfernt sind.", "{plA}{in} in der Nähe {einem_g} {sgB} (unter {d} m)"],
        &[("plA", pl(a, lang)), ("in", &in_p), ("d", &ds), ("a", b.en_a()), ("sgB", sg(b, lang)), ("einem", b.de_einem()), ("einem_g", if b.g == crate::vocab::Gender::F { "einer" } else { "eines" })],
    );
    // the genitive of the German phrasing needs another form of the noun; keep the plain ones
    if q.contains("eines ") {
        return None;
    }
    let set = b.set_name();
    let va = a.var();
    let within = place.as_ref().map(|p| format!(" within {}", region(p))).unwrap_or_default();
    let query = format!("{set} = {}\ncount of {} and {va} near some _ in {set} closer than {d} m{within}", b.sel_as("y"), a.sel());
    let (pa, sb) = (pl(a, lang), sg(b, lang));
    let einem = b.de_einem();
    let an = b.en_a();
    Some(Item::query(
        "near_some",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("{n} {pa}{in_p} are less than {d} m from {an} {sb}."), format!("{n} {pa}{in_p} sind weniger als {d} m von {einem} {sb} entfernt.")))
        }),
    ))
}

fn near_no(g: &mut Gen) -> Option<Item> {
    let (a, b) = two_things(g);
    let d = *g.rng.pick(&[50u32, 100, 200, 300]);
    let place = g.place()?;
    let lang = g.lang();
    let ds = d.to_string();
    let q = g.phrase(
        lang,
        &["Which {plA} in {P} have no {sgB} within {d} m?", "Show the {plA} in {P} without {a} {sgB} nearby (closer than {d} m).", "{plA} in {P} more than {d} m from any {sgB}"],
        &["Welche {plA} in {P} haben {kein} {sgB} im Umkreis von {d} m?", "Zeige die {plA} in {P}, die mehr als {d} m von jeder {sgB} entfernt sind.", "{plA} in {P} ohne {sgB} in {d} m Umkreis"],
        &[("plA", pl(a, lang)), ("P", &place), ("d", &ds), ("sgB", sg(b, lang)), ("kein", b.de_kein()), ("a", b.en_a())],
    );
    if lang == Lang::De && q.contains("von jeder") && b.g != crate::vocab::Gender::F {
        return None;
    }
    let set = b.set_name();
    let va = a.var();
    let query = format!("{set} = {}\n{} and {va} near no _ in {set} closer than {d} m within {}", b.sel_as("y"), a.sel(), region(&place));
    let (pa, sb) = (pl(a, lang), sg(b, lang));
    let kein = b.de_kein();
    let an = b.en_a();
    Some(Item::query(
        "near_no",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let n = n_set(o.first()?)?;
            (n >= 2).then(|| says(lang, format!("The map shows {n} {pa} in {place} without {an} {sb} within {d} m."), format!("Die Karte zeigt {n} {pa} in {place}, die {kein} {sb} im Umkreis von {d} m haben.")))
        }),
    ))
}

fn length(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(LINES, &|_| true);
    let place = g.place()?;
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(
        lang,
        &["How long are all {pl} in {P} together?", "Total length of the {pl} in {P}", "How many kilometres of {pl} are there in {P}?"],
        &["Wie lang sind alle {pl} in {P} zusammen?", "Gesamtlänge der {pl} in {P}", "Wie viele Kilometer {pl} gibt es in {P}?"],
        &[("pl", p), ("P", &place)],
    );
    Some(Item::query(
        "length",
        lang,
        q,
        format!("sum of ({} within {}).map(w => w.length)", t.sel_as("w"), region(&place)),
        Box::new(move |o, _| {
            let m = metres(o.first()?)?;
            (m > 0.0).then(|| says(lang, format!("The {p} in {place} are {} long in total.", fmt_len(m, lang)), format!("Die {p} in {place} sind zusammen {} lang.", fmt_len(m, lang))))
        }),
    ))
}

fn area_things(t: &Thing) -> bool {
    t.kind == crate::vocab::Kind::Areas || t.cond.contains("playground")
}

fn area_total(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &area_things);
    let place = g.place()?;
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(
        lang,
        &["What is the total area of the {pl} in {P}?", "How much area do the {pl} in {P} cover?", "area of all {pl} in {P}"],
        &["Wie groß ist die Fläche aller {pl} in {P}?", "Wie viel Fläche nehmen die {pl} in {P} ein?", "Gesamtfläche der {pl} in {P}"],
        &[("pl", p), ("P", &place)],
    );
    Some(Item::query(
        "area_total",
        lang,
        q,
        format!("sum of ({} within {}).map(a => a.area)", t.sel_as("a").replacen("* a", "areas a", 1), region(&place)),
        Box::new(move |o, _| {
            let m2 = square_metres(o.first()?)?;
            (m2 > 0.0).then(|| says(lang, format!("The {p} in {place} cover {} together.", fmt_area(m2, lang)), format!("Die {p} in {place} sind zusammen {} groß.", fmt_area(m2, lang))))
        }),
    ))
}

fn largest(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &|t| area_things(t) && !t.cond.contains("building"));
    let k = *g.rng.pick(&[3usize, 5]);
    let lang = g.lang();
    let place = if g.rng.chance(0.5) { Some(g.place()?) } else { None };
    let p = pl(t, lang);
    let ks = k.to_string();
    let in_p = place.as_ref().map(|x| format!(" in {x}")).unwrap_or_default();
    let q = g.phrase(
        lang,
        &["What are the {k} largest {pl}{in}?", "Which are the {k} biggest {pl}{in}?", "the {k} largest {pl}{in}"],
        &["Welches sind die {k} größten {pl}{in}?", "Was sind die {k} größten {pl}{in}?", "die {k} größten {pl}{in}"],
        &[("pl", p), ("k", &ks), ("in", &in_p)],
    );
    let within = place.as_ref().map(|x| format!(" within {}", region(x))).unwrap_or_default();
    let query = format!("largest = pick {k} {}{within} ordered by -a.area\nlargest.map(a => [a:name, a.area])\nlargest", t.sel_as("a").replacen("* a", "areas a", 1));
    Some(Item::query(
        "largest",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let rows = o.first()?.value.as_array()?;
            let mut parts = Vec::new();
            for r in rows {
                let name = r[0].as_str().filter(|n| *n != "none").map(str::to_string).unwrap_or_else(|| says(lang, "unnamed".into(), "ohne Namen".into()));
                let m2 = match (r[1]["value"].as_f64(), r[1]["unit"].as_str()) {
                    (Some(v), Some("m2" | "m²")) => v,
                    (Some(v), Some("km2" | "km²")) => v * 1e6,
                    (Some(v), Some("ha")) => v * 1e4,
                    _ => return None,
                };
                parts.push(format!("{name} ({})", fmt_area(m2, lang)));
            }
            (parts.len() >= 2).then(|| says(lang, format!("The largest {p}{in_p} are {}.", list(&parts, lang)), format!("Die größten {p}{in_p} sind {}.", list(&parts, lang))))
        }),
    ))
}

/// A property of a shop or amenity: condition, question words, answer words.
struct Attr {
    cond: &'static str,
    en_q: &'static str,
    de_q: &'static str,
    en_a: &'static str,
    de_a: &'static str,
}

const ATTRS: &[Attr] = &[
    Attr { cond: "{v}:name", en_q: "have a name", de_q: "haben einen Namen", en_a: "have a name", de_a: "haben einen Namen" },
    Attr { cond: "{v}:name == none", en_q: "have no name", de_q: "haben keinen Namen", en_a: "have no name", de_a: "haben keinen Namen" },
    Attr { cond: "{v}:wheelchair=yes", en_q: "are wheelchair accessible", de_q: "sind rollstuhlgerecht", en_a: "are wheelchair accessible", de_a: "sind rollstuhlgerecht" },
    Attr { cond: "{v}:opening_hours == none", en_q: "have no opening hours", de_q: "haben keine Öffnungszeiten", en_a: "have no opening hours", de_a: "haben keine Öffnungszeiten" },
    Attr { cond: "{v}:opening_hours", en_q: "have opening hours", de_q: "haben Öffnungszeiten", en_a: "have opening hours", de_a: "haben Öffnungszeiten" },
    Attr { cond: "{v}:website", en_q: "have a website", de_q: "haben eine Webseite", en_a: "have a website", de_a: "haben eine Webseite" },
];

fn attribute(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &|t| t.poi);
    let at = g.rng.pick(ATTRS);
    let lang = g.lang();
    let place = if g.rng.chance(0.5) { Some(g.place()?) } else { None };
    let p = pl(t, lang);
    let in_p = place.as_ref().map(|x| format!(" in {x}")).unwrap_or_default();
    let q = g.phrase(lang, &["How many {pl}{in} {what}?", "Count the {pl}{in} that {what}."], &["Wie viele {pl}{in} {what}?", "Zähle die {pl}{in}, die {what_end}."], &[
        ("pl", p),
        ("in", &in_p),
        ("what", if lang == Lang::En { at.en_q } else { at.de_q }),
        ("what_end", &verb_last(at.de_q)),
    ]);
    let v = t.var();
    let within = place.as_ref().map(|x| format!(" within {}", region(x))).unwrap_or_default();
    let query = format!("count of {} and {}{within}", t.sel(), at.cond.replace("{v}", v));
    let (en_a, de_a) = (at.en_a, at.de_a);
    let mut it = Item::query(
        "attribute",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("{n} {p}{in_p} {en_a}."), format!("{n} {p}{in_p} {de_a}.")))
        }),
    );
    it.zero_ok = true;
    Some(it)
}

/// German subordinate clause: the verb goes last (`haben keinen Namen` → `keinen Namen haben`).
fn verb_last(s: &str) -> String {
    match s.split_once(' ') {
        Some((verb, rest)) => format!("{rest} {verb}"),
        None => s.to_string(),
    }
}

fn share(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &|t| t.poi);
    let at = g.rng.pick(&ATTRS[2..]);
    let lang = g.lang();
    let place = if g.rng.chance(0.6) { Some(g.place()?) } else { None };
    let p = pl(t, lang);
    let in_p = place.as_ref().map(|x| format!(" in {x}")).unwrap_or_default();
    // German: "Welcher Anteil … hat/ist …" (singular), "Wie viel Prozent … haben/sind …"
    let one = at.de_q.replacen("haben", "hat", 1).replacen("sind", "ist", 1);
    let q = g.phrase(lang, &["What share of the {pl}{in} {what}?", "What percentage of {pl}{in} {what}?"], &["Welcher Anteil der {pl}{in} {one}?", "Wie viel Prozent der {pl}{in} {what}?"], &[("pl", p), ("in", &in_p), ("what", if lang == Lang::En { at.en_q } else { at.de_q }), ("one", &one)]);
    let v = t.var();
    let within = place.as_ref().map(|x| format!(" within {}", region(x))).unwrap_or_default();
    let query = format!("share of ({}{within}) with {}", t.sel(), at.cond.replace("{v}", v));
    let (en_a, de_a) = (at.en_a, at.de_a);
    let mut it = Item::query(
        "share",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let f = o.first()?.value.as_f64()?;
            let pct = dec(format!("{:.0} %", f * 100.0), lang);
            Some(says(lang, format!("{pct} of the {p}{in_p} {en_a}."), format!("{pct} der {p}{in_p} {de_a}.")))
        }),
    );
    it.zero_ok = true;
    Some(it)
}

fn new_per_year(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let y = g.year();
    let last = g.ctx.last_year;
    let lang = g.lang();
    let p = pl(t, lang);
    let ys = y.to_string();
    let q = g.phrase(
        lang,
        &["How many {pl} were added per year since {y}?", "How many new {pl} were mapped each year since {y}?"],
        &["Wie viele {pl} wurden seit {y} pro Jahr neu eingetragen?", "Wie viele neue {pl} kamen seit {y} jedes Jahr dazu?"],
        &[("pl", p), ("y", &ys)],
    );
    let v = t.var();
    Some(Item::query(
        "new_per_year",
        lang,
        q,
        format!("count of {} and {v}.version == 1 per year from {y} to {last}", t.sel()),
        Box::new(move |o, results| {
            let top = top_groups(o.first()?, results, 1)?;
            let (yr, n) = &top[0];
            Some(says(lang, format!("Most {p} were added in {yr} ({n})."), format!("Die meisten {p} wurden {yr} eingetragen ({n}).")))
        }),
    ))
}

fn creators(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(lang, &["Who mapped most of the {pl}?", "Which users created the most {pl}?"], &["Wer hat die meisten {pl} eingetragen?", "Welche Nutzer haben die meisten {pl} angelegt?"], &[("pl", p)]);
    let v = t.var();
    Some(Item::query(
        "creators",
        lang,
        q,
        format!("distribution of ({}).map({v} => {v}.creator)", t.sel()),
        Box::new(move |o, _| {
            // the first row of the text: `user 1737985 (gm-bremen)    560`
            let row = o.first()?.text.lines().nth(2)?;
            let name = &row[row.find('(')? + 1..row.rfind(')')?];
            let n: i64 = row.split_whitespace().last()?.parse().ok()?;
            Some(says(lang, format!("Most {p} were created by {name} ({n})."), format!("Die meisten {p} hat {name} angelegt ({n}).")))
        }),
    ))
}

fn two_counts(g: &mut Gen) -> Option<Item> {
    let a = g.thing();
    let b = g.thing();
    if a.cond == b.cond {
        return None;
    }
    let place = g.place()?;
    let lang = g.lang();
    let (pa, pb) = (pl(a, lang), pl(b, lang));
    let q = g.phrase(lang, &["How many {A} and how many {B} are there in {P}?", "Count {A} and {B} in {P}."], &["Wie viele {A} und wie viele {B} gibt es in {P}?", "Zähle {A} und {B} in {P}."], &[("A", pa), ("B", pb), ("P", &place)]);
    let r = region(&place);
    Some(Item::query(
        "two_counts",
        lang,
        q,
        format!("count of {} within {r}, count of {} within {r}", a.sel(), b.sel()),
        Box::new(move |o, _| {
            let (x, y) = (int(o.first()?)?, int(o.get(1)?)?);
            Some(says(lang, format!("In {place} there are {x} {pa} and {y} {pb}."), format!("In {place} gibt es {x} {pa} und {y} {pb}.")))
        }),
    ))
}

fn compare_places(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let a = g.place()?;
    let b = g.place()?;
    if a == b {
        return None;
    }
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(lang, &["Are there more {pl} in {A} or in {B}?", "Does {A} or {B} have more {pl}?"], &["Gibt es in {A} oder in {B} mehr {pl}?", "Hat {A} oder {B} mehr {pl}?"], &[("pl", p), ("A", &a), ("B", &b)]);
    let s = t.sel();
    Some(Item::query(
        "compare_places",
        lang,
        q,
        format!("count of {s} within {}, count of {s} within {}", region(&a), region(&b)),
        Box::new(move |o, _| {
            let (x, y) = (int(o.first()?)?, int(o.get(1)?)?);
            if x == y {
                return None;
            }
            let more = if x > y { &a } else { &b };
            Some(says(lang, format!("{a} has {x} {p}, {b} has {y}: there are more in {more}."), format!("In {a} gibt es {x} {p}, in {b} {y}: also mehr in {more}.")))
        }),
    ))
}

fn per_value(g: &mut Gen) -> Option<Item> {
    let place = g.place()?;
    let lang = g.lang();
    let (q, query, en_what, de_what) = match g.rng.below(3) {
        0 => (
            g.phrase(lang, &["Which kinds of shops are there in {P}?", "What types of shops does {P} have?"], &["Welche Arten von Geschäften gibt es in {P}?", "Welche Läden gibt es in {P}?"], &[("P", &place)]),
            format!("count of * x with x:shop within {} per x:shop", region(&place)),
            "kinds of shops",
            "Arten von Geschäften",
        ),
        1 => (
            g.phrase(lang, &["Which cuisines do the restaurants in {P} offer?", "What kind of food do restaurants in {P} serve?"], &["Welche Küchen bieten die Restaurants in {P}?", "Was für Essen gibt es in den Restaurants in {P}?"], &[("P", &place)]),
            format!("count of * x with x:amenity=restaurant and x:cuisine within {} per x:cuisine", region(&place)),
            "cuisines",
            "Küchen",
        ),
        _ => (
            g.phrase(lang, &["Which sports are played on the pitches in {P}?", "What sports pitches are there in {P}?"], &["Welche Sportarten gibt es auf den Sportplätzen in {P}?", "Für welche Sportarten gibt es in {P} Sportplätze?"], &[("P", &place)]),
            format!("count of areas a with a:leisure=pitch and a:sport within {} per a:sport", region(&place)),
            "sports",
            "Sportarten",
        ),
    };
    Some(Item::query(
        "per_value",
        lang,
        q,
        query,
        Box::new(move |o, results| {
            let top = top_groups(o.first()?, results, 3)?;
            let parts: Vec<String> = top.iter().map(|(k, c)| format!("{k} ({c})")).collect();
            Some(says(lang, format!("The most common {en_what} in {place} are {}.", list(&parts, lang)), format!("Die häufigsten {de_what} in {place} sind {}.", list(&parts, lang))))
        }),
    ))
}

fn outside(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let d = g.district()?;
    let city = g.ctx.conf.city;
    if d == city {
        return None;
    }
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(lang, &["How many {pl} are there in {C} outside {D}?", "{pl} in {C} but not in {D}: how many?"], &["Wie viele {pl} gibt es in {C} außerhalb von {D}?", "Anzahl der {pl} in {C} ohne {D}"], &[("pl", p), ("C", city), ("D", &d)]);
    let v = t.var();
    Some(Item::query(
        "outside",
        lang,
        q,
        format!("count of {} and {v} within {} and {v} not within {}", t.sel(), region(city), region(&d)),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("Outside {d}, there are {n} {p} in {city}."), format!("Außerhalb von {d} gibt es in {city} {n} {p}.")))
        }),
    ))
}

fn follow_place(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let a = g.place()?;
    let b = g.place()?;
    if a == b {
        return None;
    }
    let lang = g.lang();
    let p = pl(t, lang);
    let q1 = g.phrase(lang, &["How many {pl} are there in {P}?"], &["Wie viele {pl} gibt es in {P}?"], &[("pl", p), ("P", &a)]);
    let q2 = g.phrase(lang, &["And in {P}?", "what about {P}?", "And {P}?"], &["Und in {P}?", "und in {P}", "Und wie viele in {P}?"], &[("P", &b)]);
    let s = t.sel();
    let mut it = Item::query(
        "follow_place",
        lang,
        q2,
        format!("count of {s} within {}", region(&b)),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("There are {n} {p} in {b}."), format!("In {b} gibt es {n} {p}.")))
        }),
    );
    it.turns.insert(0, ("user".into(), q1));
    it.turns.insert(1, ("assistant".into(), format!("QUERY:\ncount of {s} within {}", region(&a))));
    Some(it)
}

fn follow_thing(g: &mut Gen) -> Option<Item> {
    let a = g.thing();
    let b = g.thing();
    if a.cond == b.cond {
        return None;
    }
    let place = g.place()?;
    let lang = g.lang();
    let (pa, pb) = (pl(a, lang), pl(b, lang));
    let q1 = g.phrase(lang, &["How many {pl} are there in {P}?"], &["Wie viele {pl} gibt es in {P}?"], &[("pl", pa), ("P", &place)]);
    let q2 = g.phrase(lang, &["And {pl}?", "and how many {pl}?"], &["Und {pl}?", "und wie viele {pl}?"], &[("pl", pb)]);
    let r = region(&place);
    let mut it = Item::query(
        "follow_thing",
        lang,
        q2,
        format!("count of {} within {r}", b.sel()),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("There are {n} {pb} in {place}."), format!("In {place} gibt es {n} {pb}.")))
        }),
    );
    it.turns.insert(0, ("user".into(), q1));
    it.turns.insert(1, ("assistant".into(), format!("QUERY:\ncount of {} within {r}", a.sel())));
    Some(it)
}

/// After a count, "Where are they?" or "Where are the bakeries in Findorff?": the map (the
/// selection), not another count.
fn follow_show(g: &mut Gen) -> Option<Item> {
    let a = g.thing();
    let place = g.place()?;
    let lang = g.lang();
    let same = g.rng.chance(0.5);
    let (b, place2) = if same { (a, place.clone()) } else { (g.thing(), g.place()?) };
    let (pa, pb) = (pl(a, lang), pl(b, lang));
    let q1 = g.phrase(lang, &["How many {pl} are there in {P}?"], &["Wie viele {pl} gibt es in {P}?"], &[("pl", pa), ("P", &place)]);
    let q2 = if same {
        g.phrase(lang, &["Where are they?", "Show them on the map.", "where are the {pl}?"], &["Wo sind sie?", "Zeig sie auf der Karte.", "wo sind die {pl}?"], &[("pl", pb)])
    } else {
        g.phrase(lang, &["Where are the {pl} in {P}?", "where are {pl} in {P}?", "Show the {pl} in {P}."], &["Wo sind die {pl} in {P}?", "wo gibt es {pl} in {P}?", "Zeig die {pl} in {P}."], &[("pl", pb), ("P", &place2)])
    };
    let mut it = Item::query(
        "follow_show",
        lang,
        q2,
        format!("{} within {}", b.sel(), region(&place2)),
        Box::new(move |o, _| {
            let n = n_set(o.first()?)?;
            (n >= 1).then(|| says(lang, format!("The map shows {n} {pb} in {place2}."), format!("Die Karte zeigt {n} {pb} in {place2}.")))
        }),
    );
    it.turns.insert(0, ("user".into(), q1));
    it.turns.insert(1, ("assistant".into(), format!("QUERY:\ncount of {} within {}", a.sel(), region(&place))));
    Some(it)
}

fn density(g: &mut Gen) -> Option<Item> {
    let t = g.thing_from(THINGS, &|t| t.kind != crate::vocab::Kind::Areas);
    let place = g.place()?;
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(lang, &["How many {pl} are there per square kilometre in {P}?", "density of {pl} in {P}"], &["Wie viele {pl} gibt es pro Quadratkilometer in {P}?", "Dichte der {pl} in {P}"], &[("pl", p), ("P", &place)]);
    Some(Item::query(
        "density",
        lang,
        q,
        format!("density of {} within {}", t.sel(), region(&place)),
        Box::new(move |o, _| {
            let text = o.first()?.text.trim().to_string();
            (!text.contains('\n') && !text.is_empty()).then(|| says(lang, format!("In {place} there are {text} {p} per square kilometre."), format!("In {place} gibt es {} {p} pro Quadratkilometer.", dec(text.clone(), lang))))
        }),
    ))
}

/// Questions that miss what they are about: the model asks back.
fn vague(g: &mut Gen) -> Option<Item> {
    let lang = g.lang();
    let t = g.thing();
    let place = g.place()?;
    let s = sg(t, lang);
    let (q, ask) = match g.rng.below(6) {
        0 => (g.phrase(lang, &["How many are there in {P}?", "how many in {P}"], &["Wie viele gibt es in {P}?", "wie viele in {P}"], &[("P", &place)]), says(lang, "What should be counted?".into(), "Was soll gezählt werden?".into())),
        1 => (g.phrase(lang, &["How far is it?", "How long does it take?"], &["Wie weit ist es?", "Wie lange dauert es?"], &[]), says(lang, "From where to where, and on foot, by bicycle or by car?".into(), "Von wo nach wo, und zu Fuß, mit dem Fahrrad oder mit dem Auto?".into())),
        2 => (
            g.phrase(lang, &["How long does it take to the nearest {sg}?", "Where is the nearest {sg}?"], &["Wie lange brauche ich {zum} nächsten {sg}?", "Wo ist {zum_art} nächste {sg}?"], &[("sg", s), ("zum", t.de_zum()), ("zum_art", if t.g == crate::vocab::Gender::F { "die" } else { "der" })]),
            says(lang, "From where?".into(), "Von wo aus?".into()),
        ),
        3 => (g.phrase(lang, &["What can I reach in 10 minutes?", "What is within 15 minutes?"], &["Was erreiche ich in 10 Minuten?", "Was liegt in 15 Minuten Entfernung?"], &[]), says(lang, "From where, and on foot, by bicycle or by car?".into(), "Von wo aus, und zu Fuß, mit dem Fahrrad oder mit dem Auto?".into())),
        4 => (g.phrase(lang, &["Show me the map.", "show me something"], &["Zeig mir die Karte.", "zeig mir was"], &[]), says(lang, "What should the map show?".into(), "Was soll die Karte zeigen?".into())),
        _ => (
            g.phrase(lang, &["Are there any {pl} near me?", "{pl} nearby"], &["Gibt es {pl} in meiner Nähe?", "{pl} in der Nähe"], &[("pl", pl(t, lang))]),
            says(lang, "Where are you? Name a place or a station.".into(), "Wo sind Sie? Nennen Sie einen Ort oder eine Haltestelle.".into()),
        ),
    };
    if lang == Lang::De && q.contains("Wo ist") && t.g == crate::vocab::Gender::N {
        return None;
    }
    Some(Item::ask("vague", lang, q, ask))
}

// ── the map features ──

/// A tag of the map features (held out by its name, like places).
fn tag<'a>(g: &mut Gen<'a>) -> Option<&'a crate::vocab::Tag> {
    let held = g.held;
    let ctx: &'a Ctx = g.ctx;
    let ys: Vec<&'a crate::vocab::Tag> = ctx.tags.iter().filter(|t| held_name(&format!("{}={}", t.key, t.value)) == held).collect();
    if ys.is_empty() { None } else { Some(ys[g.rng.below(ys.len())]) }
}

/// The condition for a tag: `x:shop=bakery`, or with quotes when the value needs them.
fn tag_cond(t: &crate::vocab::Tag) -> String {
    let plain = |s: &str| s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
    if plain(&t.key) && plain(&t.value) { format!("x:{}={}", t.key, t.value) } else { format!("x:['{}'] == '{}'", t.key, t.value) }
}

/// How people name the thing: the description, or the words of the value.
fn tag_words(g: &mut Gen, t: &crate::vocab::Tag) -> String {
    if t.value != "yes" && g.rng.chance(0.3) { t.value.replace('_', " ") } else { t.desc.clone() }
}

fn tag_count(g: &mut Gen) -> Option<Item> {
    let t = tag(g)?;
    let place = if g.rng.chance(0.7) { Some(g.place()?) } else { None };
    let d = tag_words(g, t);
    let in_p = place.as_ref().map(|p| format!(" in {p}")).unwrap_or_default();
    let q = g.phrase(Lang::En, &["Count every {d}{in}.", "Number of features{in} that are: {d}", "How many of these are there{in}: {d}?", "count: {d}{in}"], &[], &[("d", &d), ("in", &in_p)]);
    let within = place.as_ref().map(|p| format!(" within {}", region(p))).unwrap_or_default();
    let tag_text = format!("{}={}", t.key, t.value);
    let mut it = Item::query(
        "tag_count",
        Lang::En,
        q,
        format!("count of * x with {}{within}", tag_cond(t)),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(format!("There are {n} objects tagged {tag_text}{in_p}."))
        }),
    );
    it.zero_ok = g.rng.chance(0.15);
    Some(it)
}

fn tag_show(g: &mut Gen) -> Option<Item> {
    let t = tag(g)?;
    let place = g.place()?;
    let d = tag_words(g, t);
    let q = g.phrase(Lang::En, &["Show every {d} in {P}.", "Where can I find {d} in {P}?", "map of {d} in {P}"], &[], &[("d", &d), ("P", &place)]);
    let tag_text = format!("{}={}", t.key, t.value);
    Some(Item::query(
        "tag_show",
        Lang::En,
        q,
        format!("* x with {} within {}", tag_cond(t), region(&place)),
        Box::new(move |o, _| {
            let n = n_set(o.first()?)?;
            (n >= 2).then(|| format!("The map shows {n} objects tagged {tag_text} in {place}."))
        }),
    ))
}

// ── geocoding ──

impl<'a> Gen<'a> {
    fn address(&mut self) -> Option<&'a Addr> {
        let held = self.held;
        let ctx: &'a Ctx = self.ctx;
        let ys: Vec<&'a Addr> = ctx.addresses.iter().filter(|a| held_name(&format!("{} {} {}", a.street, a.number, a.city)) == held).collect();
        if ys.is_empty() { None } else { Some(ys[self.rng.below(ys.len())]) }
    }
}

/// A point near an address: as the query writes it, and as the question does (sometimes
/// latitude first without letters, as copied from a map).
fn point_texts(g: &mut Gen, lon: f64, lat: f64) -> (String, String) {
    // up to about 50 m away from the address
    let (lon, lat) = (lon + (g.rng.below(1000) as f64 - 500.0) * 1e-6, lat + (g.rng.below(1000) as f64 - 500.0) * 0.6e-6);
    let q = format!("{:.4}{}, {:.4}{}", lon.abs(), if lon < 0.0 { 'W' } else { 'E' }, lat.abs(), if lat < 0.0 { 'S' } else { 'N' });
    let shown = match g.rng.below(4) {
        0 => format!("{lat:.4}, {lon:.4}"),
        1 => format!("{:.4}° {}, {:.4}° {}", lat.abs(), if lat < 0.0 { 'S' } else { 'N' }, lon.abs(), if lon < 0.0 { 'W' } else { 'E' }),
        _ => q.clone(),
    };
    (shown, q)
}

/// The address of a hit in the results (`… · Am Wall 201, 28195 Bremen · score 1`).
fn hit_address(results: &str) -> Option<(String, Option<String>)> {
    let line = results.lines().find(|l| l.contains(" · "))?;
    let mut parts = line.split(" · ");
    let head = parts.next()?;
    let addr = parts.next()?.trim().to_string();
    let name = head.rfind('(').and_then(|i| head[i + 1..].strip_suffix(')')).map(str::to_string).filter(|n| !n.contains(' ') || !n.starts_with(char::is_numeric));
    Some((addr, name.filter(|n| !n.starts_with("relation ") && !n.starts_with("way "))))
}

fn geocode_show(g: &mut Gen) -> Option<Item> {
    let a = g.address()?;
    let text = a.text(&mut g.rng);
    let lang = g.lang();
    let q = g.phrase(lang, &["Where is {A}?", "Show {A} on the map.", "find the address {A}", "Where can I find {A}?"], &["Wo ist {A}?", "Zeige {A} auf der Karte.", "finde die Adresse {A}", "Wo liegt {A}?"], &[("A", &text)]);
    Some(Item::query(
        "geocode_show",
        lang,
        q,
        format!("geocoded '{text}'"),
        Box::new(move |_, results| {
            let (addr, name) = hit_address(results)?;
            let named = name.map(|n| format!(" ({n})")).unwrap_or_default();
            Some(says(lang, format!("The map shows {addr}{named}."), format!("Die Karte zeigt {addr}{named}.")))
        }),
    ))
}

fn geocode_parts(g: &mut Gen) -> Option<Item> {
    let a = g.address()?;
    let text = a.text(&mut g.rng);
    let lang = g.lang();
    let (field, en, de, en_a, de_a) = *g.rng.pick(&[
        ("suburb", "In which part of town is {A}?", "In welchem Stadtteil liegt {A}?", "part of town", "Stadtteil"),
        ("postcode", "What is the postcode of {A}?", "Welche Postleitzahl hat {A}?", "postcode", "Postleitzahl"),
        ("city", "In which town is {A}?", "In welcher Stadt liegt {A}?", "town", "Stadt"),
        ("state", "In which state is {A}?", "In welchem Bundesland liegt {A}?", "state", "Bundesland"),
        ("country", "In which country is {A}?", "In welchem Land liegt {A}?", "country", "Land"),
        ("district", "In which district is {A}?", "In welchem Bezirk liegt {A}?", "district", "Bezirk"),
    ]);
    let q = g.phrase(lang, &[en], &[de], &[("A", &text)]);
    Some(Item::query(
        "geocode_parts",
        lang,
        q,
        format!("(geocoded '{text}').{field}"),
        Box::new(move |o, _| {
            let v = o.first()?.value.as_str()?.to_string();
            (v != "none").then(|| says(lang, format!("The {en_a} of {text} is {v}."), format!("{de_a} von {text}: {v}.")))
        }),
    ))
}

fn geocode_route(g: &mut Gen) -> Option<Item> {
    let a = g.address()?;
    let text = a.text(&mut g.rng);
    let mode = g.mode();
    let lang = g.lang();
    if g.rng.chance(0.5) {
        let s = g.start()?;
        let q = match lang {
            Lang::En => {
                let me = *g.rng.pick(mode.en);
                g.phrase(lang, &["How long does it take to {verb} from {A} to {S}?", "Route {mode} from {A} to {S}"], &[], &[("verb", mode.en_verb), ("A", &text), ("S", &s.en), ("mode", me)])
            }
            Lang::De => {
                let md = *g.rng.pick(mode.de);
                g.phrase(lang, &[], &["Wie lange brauche ich {mode} von {A} {bis}?", "Weg {mode} von {A} {bis}"], &[("mode", md), ("A", &text), ("bis", &s.de_bis)])
            }
        };
        let query = format!("a = geocoded '{text}'\ns = {}\np = path P from a to s by {}\np.length, p.duration\np", s.pick, mode.key);
        return Some(Item::query(
            "geocode_route",
            lang,
            q,
            query,
            Box::new(move |o, _| {
                let m = metres(o.first()?)?;
                let min = minutes(o.get(1)?)?;
                Some(says(lang, format!("The way is {} long and takes {} {}.", fmt_len(m, lang), fmt_min(min, lang), mode.en[0]), format!("Der Weg ist {} lang und dauert {} {}.", fmt_len(m, lang), fmt_min(min, lang), mode.de[0])))
            }),
        ));
    }
    let t = g.thing_from(THINGS, &destination);
    let q = match lang {
        Lang::En => {
            let me = *g.rng.pick(mode.en);
            g.phrase(lang, &["How far is the nearest {sg} {mode} from {A}?", "Route {mode} from {A} to the nearest {sg}"], &[], &[("sg", t.en[0]), ("A", &text), ("mode", me)])
        }
        Lang::De => {
            let md = *g.rng.pick(mode.de);
            g.phrase(lang, &[], &["Wie weit ist es {mode} von {A} {zum} nächsten {sg}?", "Weg {mode} von {A} {zum} nächsten {sg}"], &[("mode", md), ("A", &text), ("zum", t.de_zum()), ("sg", t.de[0])])
        }
    };
    let query = format!("a = geocoded '{text}'\ndest = nearest of ({}) to a\np = path P from a to dest by {}\ndest:name, p.length, p.duration\np", t.sel(), mode.key);
    Some(Item::query("geocode_route", lang, q, query, route_answer(lang, t, mode)))
}

fn reverse_point(g: &mut Gen) -> Option<Item> {
    let a = g.address()?;
    let (shown, pt) = point_texts(g, a.lon, a.lat);
    let lang = g.lang();
    let q = g.phrase(
        lang,
        &["What is at {P}?", "Which address is at {P}?", "What is the nearest address to {P}?", "address of {P}"],
        &["Was ist bei {P}?", "Welche Adresse hat der Punkt {P}?", "Welche Adresse liegt am nächsten an {P}?", "Adresse zu {P}"],
        &[("P", &shown)],
    );
    Some(Item::query(
        "reverse_point",
        lang,
        q,
        format!("reverse-geocoded {pt}"),
        Box::new(move |_, results| {
            let (addr, name) = hit_address(results)?;
            let named = name.map(|n| format!(" ({n})")).unwrap_or_default();
            Some(says(lang, format!("The nearest address is {addr}{named}."), format!("Die nächste Adresse ist {addr}{named}.")))
        }),
    ))
}

fn near_point(g: &mut Gen) -> Option<Item> {
    let a = g.address()?;
    let (shown, pt) = point_texts(g, a.lon, a.lat);
    let t = g.thing();
    let d = *g.rng.pick(&[100u32, 200, 300, 500, 1000]);
    let lang = g.lang();
    let p = pl(t, lang);
    let ds = d.to_string();
    let q = g.phrase(lang, &["How many {pl} are within {d} m of {P}?", "{pl} less than {d} m from {P}"], &["Wie viele {pl} gibt es im Umkreis von {d} m um {P}?", "{pl} weniger als {d} m von {P} entfernt"], &[("pl", p), ("d", &ds), ("P", &shown)]);
    let v = t.var();
    Some(Item::query(
        "near_point",
        lang,
        q,
        format!("count of {} and {v} near some _ in [{pt}] closer than {d} m", t.sel()),
        Box::new(move |o, _| {
            let n = int(o.first()?)?;
            Some(says(lang, format!("There are {n} {p} within {d} m of the point."), format!("Im Umkreis von {d} m um den Punkt gibt es {n} {p}.")))
        }),
    ))
}

fn place_address(g: &mut Gen) -> Option<Item> {
    let s = g.start()?;
    let lang = g.lang();
    let q = g.phrase(lang, &["What is the address of {S}?", "address of {S}"], &["Welche Adresse hat {S}?", "Adresse von {S}"], &[("S", &s.en)]);
    let q = if lang == Lang::De { q.replace("the main station", "der Hauptbahnhof").replace("Adresse von the main station", "Adresse vom Hauptbahnhof") } else { q };
    if lang == Lang::De && q.contains(" station") {
        return None;
    }
    Some(Item::query(
        "place_address",
        lang,
        q,
        format!("reverse-geocoded ({})", s.pick),
        Box::new(move |_, results| {
            let (addr, _) = hit_address(results)?;
            Some(says(lang, format!("The address is {addr}."), format!("Die Adresse ist {addr}.")))
        }),
    ))
}

fn per_city(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let lang = g.lang();
    let p = pl(t, lang);
    let (field, q) = if g.rng.chance(0.5) {
        ("city", g.phrase(lang, &["How many {pl} are there in each town?", "{pl} per town"], &["Wie viele {pl} gibt es in jedem Ort?", "{pl} pro Ort"], &[("pl", p)]))
    } else {
        ("suburb", g.phrase(
            lang,
            &["How many {pl} are there in each part of town?", "{pl} per part of town", "How many {pl} are there in each part of {C}?", "In which parts of {C} are there the most {pl}?"],
            &["Wie viele {pl} gibt es in jedem Ortsteil?", "{pl} pro Ortsteil", "Wie viele {pl} gibt es in den Stadtteilen von {C}?", "In welchen Stadtteilen von {C} gibt es die meisten {pl}?"],
            &[("pl", p), ("C", g.ctx.conf.city)],
        ))
    };
    let v = t.var();
    let city = g.ctx.conf.city;
    let within = if field == "suburb" { format!(" within {}", region(city)) } else { String::new() };
    Some(Item::query(
        "per_city",
        lang,
        q,
        format!("count of {}{within} per {v}.{field}", t.sel()),
        Box::new(move |o, results| {
            let top = top_groups(o.first()?, results, 3)?;
            if top.iter().any(|(k, _)| k == "none") {
                return None;
            }
            let parts: Vec<String> = top.iter().map(|(k, c)| format!("{k} ({c})")).collect();
            Some(says(lang, format!("Most {p} are in {}.", list(&parts, lang)), format!("Die meisten {p} gibt es in {}.", list(&parts, lang))))
        }),
    ))
}

fn in_which(g: &mut Gen) -> Option<Item> {
    let s = g.start()?;
    let lang = g.lang();
    let (field, en, de) = *g.rng.pick(&[
        ("suburb", "In which part of town is {S}?", "In welchem Stadtteil liegt {S}?"),
        ("city", "In which town is {S}?", "In welchem Ort liegt {S}?"),
        ("state", "In which state is {S}?", "In welchem Bundesland liegt {S}?"),
        ("postcode", "Which postcode does {S} have?", "Welche Postleitzahl hat {S}?"),
    ]);
    let name = if lang == Lang::En { s.en.clone() } else { s.de_von.replacen("vom Bahnhof ", "der Bahnhof ", 1).replacen("vom Hauptbahnhof", "der Hauptbahnhof", 1).replacen("von ", "", 1) };
    let q = g.phrase(lang, &[en], &[de], &[("S", &name)]);
    let query = if field == "postcode" { format!("(reverse-geocoded ({})).postcode", s.pick) } else { format!("({}).{field}", s.pick) };
    Some(Item::query(
        "in_which",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let v = o.first()?.value.as_str()?.to_string();
            (v != "none").then(|| says(lang, format!("{}: {v}.", capital(&name)), format!("{}: {v}.", capital(&name))))
        }),
    ))
}

// ── comparisons: two results (two numbers, or two sets in two colours on the map) ──

/// The state at the start of a year, or the current one for the last year of the data.
fn at_year(y: i32, last: i32) -> String {
    if y >= last { String::new() } else { format!(" on {y}-01-01") }
}

/// "Compare the benches in 2010 and 2026": the state in both years.
fn compare_years(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let last = g.ctx.last_year;
    let y1 = g.year().min(last - 4);
    let y2 = if g.rng.chance(0.6) { last } else { (y1 + 3 + g.rng.below((last - y1 - 2).max(1) as usize) as i32).min(last) };
    let place = g.place()?;
    let lang = g.lang();
    let p = pl(t, lang);
    let (a, b) = (y1.to_string(), y2.to_string());
    let numbers = g.rng.chance(0.5);
    let q = if numbers {
        g.phrase(
            lang,
            &["Please compare the number of {pl} in {P} in {A} and {B}.", "Compare the number of {pl} in {P}: {A} vs. {B}", "How many {pl} were there in {P} in {A} compared to {B}?"],
            &["Vergleiche bitte die Zahl der {pl} in {P} {A} und {B}.", "Vergleich der Anzahl der {pl} in {P}: {A} und {B}", "Wie viele {pl} gab es in {P} {A} im Vergleich zu {B}?"],
            &[("pl", p), ("P", &place), ("A", &a), ("B", &b)],
        )
    } else {
        g.phrase(
            lang,
            &["Please compare the {pl} in {P} in {A} and {B}.", "Compare the {pl} of {P} in {A} with those in {B}.", "{pl} in {P}: {A} compared to {B}"],
            &["Vergleiche bitte die {pl} in {P} {A} und {B}.", "Vergleiche die {pl} in {P} von {A} mit denen von {B}.", "{pl} in {P}: {A} im Vergleich zu {B}"],
            &[("pl", p), ("P", &place), ("A", &a), ("B", &b)],
        )
    };
    let (sel, r) = (t.sel(), region(&place));
    let query = if numbers {
        format!("count of {sel} within {r}{}, count of {sel} within {r}{}", at_year(y1, last), at_year(y2, last))
    } else {
        format!("{sel} within {r}{}, {sel} within {r}{}", at_year(y1, last), at_year(y2, last))
    };
    let mut it = Item::query(
        "compare_years",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let (n1, n2) = if numbers { (int(o.first()?)? as usize, int(o.get(1)?)? as usize) } else { (n_set(o.first()?)?, n_set(o.get(1)?)?) };
            let trend = |en: bool| match n2.cmp(&n1) {
                std::cmp::Ordering::Greater => if en { "more" } else { "mehr" },
                std::cmp::Ordering::Less => if en { "fewer" } else { "weniger" },
                std::cmp::Ordering::Equal => if en { "as many" } else { "gleich viele" },
            };
            Some(if numbers {
                says(lang, format!("In {place} there were {n1} {p} in {y1} and {n2} in {y2}: {} than before.", trend(true)), format!("Im Jahr {y1} gab es in {place} {n1} {p}, im Jahr {y2} {n2}: {} als vorher.", trend(false)))
            } else {
                says(lang, format!("The map shows the {n1} {p} of {y1} and the {n2} of {y2} in {place}."), format!("Die Karte zeigt die {n1} {p} von {y1} und die {n2} von {y2} in {place}."))
            })
        }),
    );
    it.zero_ok = true;
    Some(it)
}

/// "Compare the cafés with the restaurants in Mitte": both, as numbers or on the map.
fn compare_things(g: &mut Gen) -> Option<Item> {
    let a = g.thing();
    let b = g.thing();
    if a.cond == b.cond {
        return None;
    }
    let place = g.place()?;
    let lang = g.lang();
    let (pa, pb) = (pl(a, lang), pl(b, lang));
    let numbers = g.rng.chance(0.5);
    let q = if numbers {
        g.phrase(
            lang,
            &["Compare the number of {A} with the number of {B} in {P}.", "Compare how many {A} and {B} there are in {P}.", "number of {A} compared to {B} in {P}"],
            &["Vergleiche die Zahl der {A} mit der Zahl der {B} in {P}.", "Vergleiche, wie viele {A} und {B} es in {P} gibt.", "Anzahl der {A} im Vergleich zu {B} in {P}"],
            &[("A", pa), ("B", pb), ("P", &place)],
        )
    } else {
        g.phrase(
            lang,
            &["Compare {A} with {B} in {P}.", "Compare {A} to {B} in {P}.", "{A} and {B} in {P} compared"],
            &["Vergleiche {A} mit {B} in {P}.", "Vergleiche {A} und {B} in {P}.", "{A} und {B} in {P} im Vergleich"],
            &[("A", pa), ("B", pb), ("P", &place)],
        )
    };
    let r = region(&place);
    // both on the map, in two colours (also when the numbers are asked for: the answer says them)
    let query = format!("{} within {r}\n{} within {r}", a.sel(), b.sel());
    let mut it = Item::query(
        "compare_things",
        lang,
        q,
        query,
        Box::new(move |o, _| {
            let (x, y) = (n_set(o.first()?)?, n_set(o.get(1)?)?);
            Some(if numbers {
                says(lang, format!("In {place} there are {x} {pa} and {y} {pb}."), format!("In {place} gibt es {x} {pa} und {y} {pb}."))
            } else {
                says(lang, format!("The map shows {x} {pa} and {y} {pb} in {place}."), format!("Die Karte zeigt {x} {pa} und {y} {pb} in {place}."))
            })
        }),
    );
    it.zero_ok = true;
    Some(it)
}

/// "Compare the playgrounds in Vegesack with those in Findorff": both on the map.
fn compare_places_map(g: &mut Gen) -> Option<Item> {
    let t = g.thing();
    let a = g.place()?;
    let b = g.place()?;
    if a == b {
        return None;
    }
    let lang = g.lang();
    let p = pl(t, lang);
    let q = g.phrase(
        lang,
        &["Compare the {pl} in {A} with those in {B}.", "Compare the {pl} of {A} to those of {B}.", "{pl}: {A} compared to {B}"],
        &["Vergleiche die {pl} in {A} mit denen in {B}.", "Vergleiche die {pl} von {A} und {B}.", "{pl}: {A} im Vergleich zu {B}"],
        &[("pl", p), ("A", &a), ("B", &b)],
    );
    let s = t.sel();
    let mut it = Item::query(
        "compare_places_map",
        lang,
        q,
        format!("{s} within {}, {s} within {}", region(&a), region(&b)),
        Box::new(move |o, _| {
            let (x, y) = (n_set(o.first()?)?, n_set(o.get(1)?)?);
            Some(says(lang, format!("The map shows {x} {p} in {a} and {y} in {b}."), format!("Die Karte zeigt {x} {p} in {a} und {y} in {b}.")))
        }),
    );
    it.zero_ok = true;
    Some(it)
}
