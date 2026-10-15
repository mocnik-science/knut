//! Area rules (KNUT_LANG.md §5.2): which closed ways and relations count as areas.

use knut_format::Tags;

fn get<'a>(tags: &'a Tags, k: &str) -> Option<&'a str> {
    tags.iter().find(|(kk, _)| kk.as_ref() == k).map(|(_, v)| v.as_ref())
}

/// Keys whose presence makes a closed way an area (osmium/osm2pgsql style), with values
/// that are linear despite the key.
const AREA_KEYS: &[(&str, &[&str])] = &[
    ("building", &[]),
    ("building:part", &[]),
    ("landuse", &[]),
    ("amenity", &[]),
    ("leisure", &["track", "slipway"]),
    ("shop", &[]),
    ("tourism", &[]),
    ("office", &[]),
    ("craft", &[]),
    ("military", &[]),
    ("historic", &["citywalls", "city_wall"]),
    ("place", &[]),
    ("natural", &["coastline", "cliff", "ridge", "arete", "tree_row", "valley", "gorge"]),
    ("man_made", &["embankment", "breakwater", "groyne", "cutline", "dyke", "pipeline", "goods_conveyor"]),
    ("aeroway", &["taxiway", "runway", "parking_position"]),
    ("public_transport", &[]),
    ("power", &["line", "minor_line", "cable"]),
    ("water", &[]),
    ("wetland", &[]),
    ("boundary", &["administrative", "political", "maritime"]),
    ("healthcare", &[]),
    ("club", &[]),
    ("emergency", &[]),
    ("golf", &["hole"]),
    ("indoor", &[]),
    ("parking", &[]),
];

/// True if a closed way with these tags is an area.
pub fn way_is_area(tags: &Tags) -> bool {
    match get(tags, "area") {
        Some("yes") => return true,
        Some("no") => return false,
        _ => {}
    }
    if get(tags, "type") == Some("multipolygon") {
        return true;
    }
    if let Some(v) = get(tags, "waterway")
        && matches!(v, "riverbank" | "dock" | "boatyard" | "dam")
    {
        return true;
    }
    if let Some(v) = get(tags, "highway")
        && matches!(v, "rest_area" | "services")
    {
        return true;
    }
    if let Some(v) = get(tags, "railway")
        && matches!(v, "platform" | "station" | "turntable")
    {
        return true;
    }
    for (k, linear) in AREA_KEYS {
        if let Some(v) = get(tags, k)
            && v != "no"
            && !linear.contains(&v)
        {
            return true;
        }
    }
    false
}

/// True if a relation with these tags is an area (multipolygon or boundary).
pub fn relation_is_area(tags: &Tags) -> bool {
    matches!(get(tags, "type"), Some("multipolygon") | Some("boundary"))
}
