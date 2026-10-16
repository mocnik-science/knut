//! The words of the questions: things with their tags and names in English and German,
//! ways of moving, and the databases with their places.

/// How a thing is mapped, and so how it is selected.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Only points (benches, trees, …).
    Nodes,
    /// Points or areas (shops, schools, …): `* x`.
    Any,
    /// Areas (parks, buildings, …).
    Areas,
    /// Lines (roads, rivers, …).
    Ways,
}

/// German grammatical gender (articles: zum/zur, einem/einer, keinen/keine/kein).
#[derive(Clone, Copy, PartialEq)]
pub enum Gender {
    M,
    F,
    N,
}

pub struct Thing {
    /// The condition with `{v}` for the variable, e.g. `{v}:amenity=bench`.
    pub cond: &'static str,
    pub kind: Kind,
    pub en: [&'static str; 2],
    pub de: [&'static str; 2],
    pub g: Gender,
    /// Shops and amenities that have names, opening hours, websites.
    pub poi: bool,
}

const fn t(cond: &'static str, kind: Kind, en: [&'static str; 2], de: [&'static str; 2], g: Gender, poi: bool) -> Thing {
    Thing { cond, kind, en, de, g, poi }
}

use Gender::*;
use Kind::*;

pub const THINGS: &[Thing] = &[
    t("{v}:amenity=bench", Nodes, ["bench", "benches"], ["Bank", "Bänke"], F, false),
    t("{v}:amenity=waste_basket", Nodes, ["waste basket", "waste baskets"], ["Mülleimer", "Mülleimer"], M, false),
    t("{v}:shop=bakery", Any, ["bakery", "bakeries"], ["Bäckerei", "Bäckereien"], F, true),
    t("{v}:shop=supermarket", Any, ["supermarket", "supermarkets"], ["Supermarkt", "Supermärkte"], M, true),
    t("{v}:shop=butcher", Any, ["butcher", "butchers"], ["Metzgerei", "Metzgereien"], F, true),
    t("{v}:shop=kiosk", Any, ["kiosk", "kiosks"], ["Kiosk", "Kioske"], M, true),
    t("{v}:shop=hairdresser", Any, ["hairdresser", "hairdressers"], ["Friseur", "Friseure"], M, true),
    t("{v}:shop=bicycle", Any, ["bicycle shop", "bicycle shops"], ["Fahrradladen", "Fahrradläden"], M, true),
    t("{v}:shop=books", Any, ["bookshop", "bookshops"], ["Buchhandlung", "Buchhandlungen"], F, true),
    t("{v}:shop=florist", Any, ["florist", "florists"], ["Blumenladen", "Blumenläden"], M, true),
    t("{v}:shop=clothes", Any, ["clothes shop", "clothes shops"], ["Bekleidungsgeschäft", "Bekleidungsgeschäfte"], N, true),
    t("{v}:amenity=pharmacy", Any, ["pharmacy", "pharmacies"], ["Apotheke", "Apotheken"], F, true),
    t("{v}:amenity=doctors", Any, ["doctor's practice", "doctors' practices"], ["Arztpraxis", "Arztpraxen"], F, true),
    t("{v}:amenity=dentist", Any, ["dentist", "dentists"], ["Zahnarzt", "Zahnärzte"], M, true),
    t("{v}:amenity=hospital", Any, ["hospital", "hospitals"], ["Krankenhaus", "Krankenhäuser"], N, true),
    t("{v}:amenity=school", Any, ["school", "schools"], ["Schule", "Schulen"], F, true),
    t("{v}:amenity=kindergarten", Any, ["kindergarten", "kindergartens"], ["Kindergarten", "Kindergärten"], M, true),
    t("{v}:amenity=library", Any, ["library", "libraries"], ["Bibliothek", "Bibliotheken"], F, true),
    t("{v}:amenity=restaurant", Any, ["restaurant", "restaurants"], ["Restaurant", "Restaurants"], N, true),
    t("{v}:amenity=cafe", Any, ["café", "cafés"], ["Café", "Cafés"], N, true),
    t("{v}:amenity=fast_food", Any, ["fast food place", "fast food places"], ["Imbiss", "Imbisse"], M, true),
    t("{v}:amenity=pub", Any, ["pub", "pubs"], ["Kneipe", "Kneipen"], F, true),
    t("{v}:amenity=ice_cream", Any, ["ice cream parlour", "ice cream parlours"], ["Eisdiele", "Eisdielen"], F, true),
    t("{v}:amenity=cinema", Any, ["cinema", "cinemas"], ["Kino", "Kinos"], N, true),
    t("{v}:amenity=post_box", Nodes, ["post box", "post boxes"], ["Briefkasten", "Briefkästen"], M, false),
    t("{v}:amenity=post_office", Any, ["post office", "post offices"], ["Postfiliale", "Postfilialen"], F, true),
    t("{v}:amenity=atm", Nodes, ["cash machine", "cash machines"], ["Geldautomat", "Geldautomaten"], M, false),
    t("{v}:amenity=bank", Any, ["bank", "banks"], ["Bankfiliale", "Bankfilialen"], F, true),
    t("{v}:amenity=bicycle_parking", Any, ["bicycle stand", "bicycle stands"], ["Fahrradständer", "Fahrradständer"], M, false),
    t("{v}:amenity=drinking_water", Nodes, ["drinking fountain", "drinking fountains"], ["Trinkbrunnen", "Trinkbrunnen"], M, false),
    t("{v}:amenity=toilets", Any, ["public toilet", "public toilets"], ["Toilette", "Toiletten"], F, false),
    t("{v}:amenity=fuel", Any, ["petrol station", "petrol stations"], ["Tankstelle", "Tankstellen"], F, true),
    t("{v}:amenity=charging_station", Any, ["charging station", "charging stations"], ["Ladestation", "Ladestationen"], F, false),
    t("{v}:amenity=recycling", Any, ["recycling container", "recycling containers"], ["Wertstoffcontainer", "Wertstoffcontainer"], M, false),
    t("{v}:amenity=parking", Any, ["car park", "car parks"], ["Parkplatz", "Parkplätze"], M, false),
    t("{v}:amenity=fire_station", Any, ["fire station", "fire stations"], ["Feuerwache", "Feuerwachen"], F, true),
    t("{v}:amenity=police", Any, ["police station", "police stations"], ["Polizeiwache", "Polizeiwachen"], F, true),
    t("{v}:amenity=place_of_worship", Any, ["place of worship", "places of worship"], ["Gotteshaus", "Gotteshäuser"], N, true),
    t("{v}:tourism=hotel", Any, ["hotel", "hotels"], ["Hotel", "Hotels"], N, true),
    t("{v}:tourism=museum", Any, ["museum", "museums"], ["Museum", "Museen"], N, true),
    t("{v}:highway=bus_stop", Nodes, ["bus stop", "bus stops"], ["Bushaltestelle", "Bushaltestellen"], F, false),
    t("{v}:highway=traffic_signals", Nodes, ["traffic light", "traffic lights"], ["Ampel", "Ampeln"], F, false),
    t("{v}:natural=tree", Nodes, ["tree", "trees"], ["Baum", "Bäume"], M, false),
    t("{v}:leisure=playground", Any, ["playground", "playgrounds"], ["Spielplatz", "Spielplätze"], M, false),
    t("{v}:leisure=park", Areas, ["park", "parks"], ["Park", "Parks"], M, false),
    t("{v}:leisure=pitch", Areas, ["sports pitch", "sports pitches"], ["Sportplatz", "Sportplätze"], M, false),
    t("{v}:building", Areas, ["building", "buildings"], ["Gebäude", "Gebäude"], N, false),
    t("{v}:natural=water", Areas, ["body of water", "bodies of water"], ["Gewässer", "Gewässer"], N, false),
    t("{v}:landuse=forest", Areas, ["forest", "forests"], ["Wald", "Wälder"], M, false),
    t("{v}:landuse=cemetery", Areas, ["cemetery", "cemeteries"], ["Friedhof", "Friedhöfe"], M, false),
];

/// Lines, measured by their length.
pub const LINES: &[Thing] = &[
    t("{v}:highway=cycleway", Ways, ["cycleway", "cycleways"], ["Radweg", "Radwege"], M, false),
    t("{v}:highway=footway", Ways, ["footpath", "footpaths"], ["Fußweg", "Fußwege"], M, false),
    t("{v}:highway=residential", Ways, ["residential street", "residential streets"], ["Wohnstraße", "Wohnstraßen"], F, false),
    t("{v}:highway=motorway", Ways, ["motorway", "motorways"], ["Autobahn", "Autobahnen"], F, false),
    t("{v}:railway=rail", Ways, ["railway track", "railway tracks"], ["Bahngleis", "Bahngleise"], N, false),
    t("{v}:waterway=river", Ways, ["river", "rivers"], ["Fluss", "Flüsse"], M, false),
    t("{v}:waterway=stream", Ways, ["stream", "streams"], ["Bach", "Bäche"], M, false),
    t("{v}:highway=track", Ways, ["farm track", "farm tracks"], ["Feldweg", "Feldwege"], M, false),
];

/// Things and lines that appear only in the held-out questions: does the model find tags it
/// has not been trained on?
pub const HELD_OUT: &[&str] = &["{v}:amenity=pharmacy", "{v}:shop=kiosk", "{v}:amenity=cinema", "{v}:amenity=fire_station", "{v}:amenity=ice_cream", "{v}:highway=track"];

impl Thing {
    pub fn held_out(&self) -> bool {
        HELD_OUT.contains(&self.cond)
    }

    /// The variable for a selection of this thing.
    pub fn var(&self) -> &'static str {
        match self.kind {
            Nodes => "n",
            Any => "x",
            Areas => "a",
            Ways => "w",
        }
    }

    /// `nodes n with n:amenity=bench` (with the variable `v`).
    pub fn sel_as(&self, v: &str) -> String {
        let kind = match self.kind {
            Nodes => "nodes",
            Any => "*",
            Areas => "areas",
            Ways => "ways",
        };
        format!("{kind} {v} with {}", self.cond_as(v))
    }

    pub fn sel(&self) -> String {
        self.sel_as(self.var())
    }

    pub fn cond_as(&self, v: &str) -> String {
        self.cond.replace("{v}", v)
    }

    /// A name for a variable holding a set of these things: `bus_stops`.
    pub fn set_name(&self) -> String {
        self.en[1].replace(['\'', '’'], "").replace([' ', '-'], "_").replace('é', "e")
    }

    /// German dative after zu: `zum nächsten Supermarkt`, `zur nächsten Apotheke`.
    pub fn de_zum(&self) -> &'static str {
        if self.g == F { "zur" } else { "zum" }
    }

    /// German dative indefinite article: `von einer Schule`, `von einem Park`.
    pub fn de_einem(&self) -> &'static str {
        if self.g == F { "einer" } else { "einem" }
    }

    /// German accusative negation: `keinen Mülleimer`, `keine Schule`, `kein Café`.
    pub fn de_kein(&self) -> &'static str {
        match self.g {
            M => "keinen",
            F => "keine",
            N => "kein",
        }
    }

    /// English indefinite article.
    pub fn en_a(&self) -> &'static str {
        if self.en[0].starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" }
    }
}

/// A way of moving: the word in the query and in the questions.
pub struct Mode {
    pub key: &'static str,
    /// "on foot", "by bike", …
    pub en: &'static [&'static str],
    /// "walk", "cycle", "drive"
    pub en_verb: &'static str,
    pub de: &'static [&'static str],
    /// Short replies to the question back.
    pub en_reply: &'static [&'static str],
    pub de_reply: &'static [&'static str],
    /// Minutes that make sense for an isochrone.
    pub minutes: &'static [u32],
}

pub const MODES: &[Mode] = &[
    Mode {
        key: "foot",
        en: &["on foot", "walking"],
        en_verb: "walk",
        de: &["zu Fuß"],
        en_reply: &["on foot", "walking", "I walk", "by foot"],
        de_reply: &["zu Fuß", "laufen", "ich gehe zu Fuß", "Fuß"],
        minutes: &[5, 10, 15, 20, 30],
    },
    Mode {
        key: "bicycle",
        en: &["by bike", "by bicycle", "cycling"],
        en_verb: "cycle",
        de: &["mit dem Fahrrad", "mit dem Rad"],
        en_reply: &["by bike", "bicycle", "cycling"],
        de_reply: &["mit dem Rad", "Fahrrad", "mit dem Fahrrad", "Rad"],
        minutes: &[5, 10, 15, 20],
    },
    Mode {
        key: "car",
        en: &["by car", "driving"],
        en_verb: "drive",
        de: &["mit dem Auto"],
        en_reply: &["by car", "car", "driving"],
        de_reply: &["mit dem Auto", "Auto", "ich fahre Auto"],
        minutes: &[5, 10, 15, 20],
    },
];

/// What is known about a database before looking into it.
pub struct DbConf {
    /// File name without `.knutdb`.
    pub stem: &'static str,
    /// The main city: districts lie within it.
    pub city: &'static str,
    /// The admin level of the city's districts, and their names.
    pub district_level: &'static str,
    pub district_en: [&'static str; 2],
    pub district_de: [&'static str; 2],
    /// Further admin levels whose areas serve as places (towns around, …).
    pub place_levels: &'static [&'static str],
    /// At most this many places of those levels.
    pub place_sample: usize,
}

pub const DBS: &[DbConf] = &[
    DbConf {
        stem: "germany",
        city: "Hamburg",
        district_level: "10",
        district_en: ["district", "districts"],
        district_de: ["Stadtteil", "Stadtteile"],
        place_levels: &["6", "8"],
        place_sample: 120,
    },
    DbConf {
        stem: "sweden",
        city: "Stockholm",
        district_level: "10",
        district_en: ["district", "districts"],
        district_de: ["Stadtteil", "Stadtteile"],
        place_levels: &["7", "4"],
        place_sample: 80,
    },
    DbConf {
        stem: "bremen",
        city: "Bremen",
        district_level: "10",
        district_en: ["district", "districts"],
        district_de: ["Stadtteil", "Stadtteile"],
        place_levels: &["8", "6"],
        place_sample: 40,
    },
    DbConf {
        stem: "austria",
        city: "Wien",
        district_level: "9",
        district_en: ["district", "districts"],
        district_de: ["Bezirk", "Bezirke"],
        place_levels: &["6", "8"],
        place_sample: 80,
    },
];

/// A tag of the OpenStreetMap map features with a short description (map_features.csv in
/// knut-additional/training: `key; value; description`).
pub struct Tag {
    pub key: String,
    pub value: String,
    pub desc: String,
}

/// Reads `key; value; description` lines (the first line names the columns).
pub fn read_tags(path: &std::path::Path) -> Result<Vec<Tag>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, "; ").collect();
        let [key, value, desc] = parts[..] else { return Err(format!("{}:{}: expected `key; value; description`", path.display(), i + 1)) };
        out.push(Tag { key: key.trim().into(), value: value.trim().into(), desc: desc.trim().into() });
    }
    Ok(out)
}
