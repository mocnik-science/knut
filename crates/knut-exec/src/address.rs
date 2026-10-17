//! Geocoding in the language (KNUT_LANG.md §7.2a): `geocode`/`geocoded` (elements with an
//! address, best first), `reverse-geocode`/`reverse-geocoded` (elements with an address near
//! a point or element, nearest first), and the address fields of elements (§5.2b): from the
//! `addr:*` tags, else from the administrative boundaries around the element.
//!
//! The address index answers geocoding; without it, the addresses are collected once per run
//! by scanning the database (a notice says so). The boundaries are found by a spatial scan of
//! the boundary relations around the element, per admin level, only for the levels a field
//! needs.

use crate::eval::{Ctx, lock};
use crate::geocode::{AddrIx, Address};
use crate::geom::{BBoxF, P};
use crate::region::Region;
use crate::scan::{self, Item, KindMask, ScanSpec, TimeMode};
use knut_plan::pushdown::TagPred;
use crate::value::*;
use knut_format::{ObjType, Str};
use knut_format::container::ElementKind;
use knut_lang::diag::Span;
use std::sync::Arc;

/// Results of `geocode` and `reverse-geocode` at most.
pub const LIMIT: usize = 20;
/// `reverse-geocode` looks this far around the point.
pub const RADIUS_M: f64 = 200.0;

/// An administrative area: its name, its ISO code, and where it is.
pub struct Admin {
    pub name: Option<Str>,
    pub iso: Option<Str>,
    pub region: Region,
    pub bbox: BBoxF,
}

/// The address index of the database, or one built from a scan of it (once per run).
fn with_index<T>(ctx: &Ctx, f: impl FnOnce(&AddrIx) -> T) -> R<T> {
    let db = ctx.db()?;
    let err = |e: String| RtErr::internal(Span::default(), e);
    if db.reader.element(ElementKind::AddressIndex).is_some() {
        let b = crate::indexes::element_bytes(&db.reader, ElementKind::AddressIndex).map_err(err)?;
        return Ok(f(&AddrIx::new(b).map_err(err)?));
    }
    let built = ctx
        .caches
        .addresses
        .get_or_init(|| {
            lock(&ctx.caches.notices).push(format!(
                "Geocoding without the address index searches the whole database; it is fast with: knut manage {} add addresses",
                db.path.display()
            ));
            crate::geocode::collect_addresses(db, ctx.control.as_deref()).map(|a| Arc::new(crate::geocode::encode_addresses(&a)))
        })
        .clone()?;
    Ok(f(&AddrIx::new(&built).map_err(err)?))
}

/// The element of an address, as it is at the end of the data, with the score or distance
/// of the hit.
fn element(ctx: &Ctx, a: &Address, score: f64) -> R<Option<Value>> {
    let db = ctx.db()?;
    let h = db.history(a.otype, a.id);
    let Some(v) = h.versions.iter().rev().find(|v| v.ts <= db.latest).filter(|v| v.visible) else { return Ok(None) };
    let kind = match a.otype {
        ObjType::Node => EKind::Node,
        ObjType::Way if v.closed() && crate::areas::way_is_area(&v.tags) => EKind::Area,
        ObjType::Way => EKind::Way,
        ObjType::Relation if crate::areas::relation_is_area(&v.tags) => EKind::Area,
        ObjType::Relation => EKind::Relation,
    };
    let mut e = Elem::new(kind, Some(a.otype), a.id, v.clone(), db.latest);
    e.extra = Extra::Hit { score, distance: (a.distance > 0.0 || score < 0.0).then_some(a.distance) };
    let _ = e.hist.set(h);
    Ok(Some(Value::Elem(Arc::new(e))))
}

/// `geocode TEXT`: elements with this address, best first (score 1: street, house number and
/// place match; less: the house number only begins like the one asked for, or is missing).
pub fn geocode(ctx: &Ctx, query: &str, one: bool) -> R<Value> {
    let hits = with_index(ctx, |ix| ix.geocode(query, if one { 1 } else { LIMIT }))?;
    let mut out = Vec::new();
    let nq = crate::geocode::address_query(query).split(',').next().and_then(|s| s.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit()))).map(crate::geocode::normalize);
    for a in &hits {
        let score = match &nq {
            Some(n) if crate::geocode::normalize(&a.number) == *n => 1.0,
            Some(_) => 0.7,
            None => 0.5,
        };
        if let Some(v) = element(ctx, a, score)? {
            out.push(v);
        }
    }
    Ok(if one { out.into_iter().next().unwrap_or(Value::None) } else { Value::List(Arc::new(out)) })
}

/// `reverse-geocode X`: elements with an address within 200 m of the point, nearest first.
pub fn reverse(ctx: &Ctx, p: P, one: bool) -> R<Value> {
    let hits = with_index(ctx, |ix| ix.reverse(p.0, p.1, RADIUS_M, if one { 1 } else { LIMIT }))?;
    let mut out = Vec::new();
    for a in &hits {
        if let Some(v) = element(ctx, a, -1.0)? {
            out.push(v);
        }
    }
    Ok(if one { out.into_iter().next().unwrap_or(Value::None) } else { Value::List(Arc::new(out)) })
}

/// Cells in which boundaries are looked up: a tenth of a degree.
const CELL_DEG: f64 = 0.1;

/// The administrative areas of one level that may contain points of the cell around `p`:
/// a scan of the boundary relations in the cell (the database is spatially indexed), each
/// boundary assembled once per run. Cached per cell and level.
fn admins_near(ctx: &Ctx, level: u32, p: P) -> R<Arc<Vec<Arc<Admin>>>> {
    let cell = ((p.0 / CELL_DEG).floor() as i32, (p.1 / CELL_DEG).floor() as i32);
    if let Some(a) = lock(&ctx.caches.admin_cells).get(&(level, cell.0, cell.1)) {
        return Ok(a.clone());
    }
    let db = ctx.db()?;
    let bbox = BBoxF { minx: cell.0 as f64 * CELL_DEG, maxx: (cell.0 + 1) as f64 * CELL_DEG, miny: cell.1 as f64 * CELL_DEG, maxy: (cell.1 + 1) as f64 * CELL_DEG };
    let spec = ScanSpec {
        bbox: Some(bbox),
        kinds: KindMask { relation: true, ..Default::default() },
        time: TimeMode::State(db.latest),
        region: None,
        pred: TagPred::And(vec![TagPred::Eq("boundary".into(), "administrative".into()), TagPred::Eq("admin_level".into(), level.to_string())]),
        need_geom: false,
        need_hist: false,
        attrs_history: false,
    };
    let parts = scan::fold(db, ctx.control.as_deref(), &spec, &Vec::new, &|acc: &mut Vec<Arc<Elem>>, it: Item| {
        if let Item::Elem(e) = it {
            acc.push(e);
        }
        Ok(())
    })?;
    let mut out = Vec::new();
    for e in parts.into_iter().flatten() {
        let a = admin(ctx, &e, level);
        if !a.bbox.is_empty() && a.bbox.intersects(&bbox) || a.bbox.is_empty() && level == 2 {
            out.push(a);
        }
    }
    let out = Arc::new(out);
    lock(&ctx.caches.admin_cells).insert((level, cell.0, cell.1), out.clone());
    Ok(out)
}

/// A boundary relation as an administrative area (assembled once per run).
fn admin(ctx: &Ctx, e: &Elem, level: u32) -> Arc<Admin> {
    if let Some(a) = lock(&ctx.caches.admin_areas).get(&e.id) {
        return a.clone();
    }
    // the boundary relation as an area (its outer and inner rings); one cut off by the extract
    // keeps its name and code
    let mut ar = Elem::new(EKind::Area, e.otype, e.id, e.v.clone(), e.at);
    ar.bbox = e.bbox;
    let region = Region::from_geom(ctx.elem_geom(&ar), String::new());
    let bbox = region.bbox();
    let iso = e.tag(if level == 2 { "ISO3166-1:alpha2" } else { "ISO3166-2" }).or_else(|| if level == 2 { e.tag("ISO3166-1") } else { None }).cloned();
    let a = Arc::new(Admin { name: e.tag("name").cloned(), iso, region, bbox });
    lock(&ctx.caches.admin_areas).insert(e.id, a.clone());
    a
}

/// The administrative area of a level around a point.
fn admin_at(ctx: &Ctx, level: u32, p: P) -> R<Option<Arc<Admin>>> {
    Ok(admins_near(ctx, level, p)?.iter().find(|a| !a.bbox.is_empty() && a.bbox.contains(p) && a.region.contains(p)).cloned())
}

/// Name of the area of the first of these levels that has one around the point.
fn admin_name(ctx: &Ctx, p: P, levels: &[u32]) -> R<Option<Str>> {
    for &l in levels {
        if let Some(a) = admin_at(ctx, l, p)?
            && a.name.is_some()
        {
            return Ok(a.name.clone());
        }
    }
    Ok(None)
}

/// An address field of an element (texts), or `None` if `f` is no address field.
pub fn field(ctx: &Ctx, v: &Value, e: &Elem, f: &str) -> R<Option<Value>> {
    let text = |s: Option<&Str>| s.map_or(Value::None, |s| Value::Text(s.clone()));
    let point = || ctx.rep_point(v);
    let from = |levels: &[u32], tag: &str| -> R<Value> {
        if let Some(t) = e.tag(tag) {
            return Ok(Value::Text(t.clone()));
        }
        match point() {
            Some(p) => Ok(text(admin_name(ctx, p, levels)?.as_ref())),
            None => Ok(Value::None),
        }
    };
    Ok(Some(match f {
        "housenumber" => text(e.tag("addr:housenumber")),
        "street" => text(e.tag("addr:street").or_else(|| e.tag("addr:place"))),
        "postcode" => text(e.tag("addr:postcode")),
        "suburb" => from(&[10], "addr:suburb")?,
        "district" => from(&[9], "addr:district")?,
        // a municipality, else (cities that are districts of their own) the county level
        "city" => from(&[8, 7, 6], "addr:city")?,
        "county" => from(&[6], "addr:county")?,
        "state" => from(&[4], "addr:state")?,
        "state_code" => match point() {
            Some(p) => text(admin_at(ctx, 4, p)?.as_ref().and_then(|a| a.iso.as_ref())),
            None => Value::None,
        },
        "country" => {
            let around = match point() {
                Some(p) => admin_name(ctx, p, &[2])?,
                None => None,
            };
            match around {
                Some(n) => Value::Text(n),
                // the country boundary may be cut off by the extract: by its code
                None => match (country_code(ctx, e, point())?, point()) {
                    (Some(c), Some(p)) => text(admins_near(ctx, 2, p)?.iter().find(|a| a.iso.as_ref().is_some_and(|i| i.eq_ignore_ascii_case(&c))).and_then(|a| a.name.as_ref())),
                    _ => Value::None,
                },
            }
        }
        "country_code" => country_code(ctx, e, point())?.map_or(Value::None, |c| Value::text(&c)),
        "continent" => country_code(ctx, e, point())?.and_then(|c| continent(&c)).map_or(Value::None, Value::text),
        "address" => {
            let get = |f: &str| -> R<Option<String>> {
                Ok(match field(ctx, v, e, f)? {
                    Some(Value::Text(t)) => Some(t.to_string()),
                    _ => None,
                })
            };
            let line1 = [get("street")?, get("housenumber")?].into_iter().flatten().collect::<Vec<_>>().join(" ");
            let line2 = [get("postcode")?, get("city")?].into_iter().flatten().collect::<Vec<_>>().join(" ");
            let mut parts: Vec<String> = Vec::new();
            if !line1.is_empty() {
                parts.push(line1);
            } else {
                // no street: the name and the part of town
                if let Some(n) = e.tag("name") {
                    parts.push(n.to_string());
                }
                if let Some(s) = get("suburb")? {
                    parts.push(s);
                }
            }
            if !line2.is_empty() {
                parts.push(line2);
            }
            if let Some(c) = get("country")? {
                parts.push(c);
            }
            if parts.is_empty() { Value::None } else { Value::text(&parts.join(", ")) }
        }
        _ => return Ok(None),
    }))
}

/// The ISO 3166-1 code of the country (upper case): `addr:country`, else the country boundary.
fn country_code(ctx: &Ctx, e: &Elem, p: Option<P>) -> R<Option<String>> {
    if let Some(c) = e.tag("addr:country").filter(|c| c.len() == 2) {
        return Ok(Some(c.to_uppercase()));
    }
    let Some(p) = p else { return Ok(None) };
    Ok(admin_at(ctx, 2, p)?.as_ref().and_then(|a| a.iso.as_ref()).map(|c| c.to_uppercase()))
}

/// The continent of a country (ISO 3166-1 alpha-2), as Nominatim names them.
pub fn continent(code: &str) -> Option<&'static str> {
    const EUROPE: &str = "AD AL AT AX BA BE BG BY CH CY CZ DE DK EE ES FI FO FR GB GG GI GR HR HU IE IM IS IT JE LI LT LU LV MC MD ME MK MT NL NO PL PT RO RS RU SE SI SJ SK SM UA VA XK";
    const ASIA: &str = "AE AF AM AZ BD BH BN BT CN GE HK ID IL IN IQ IR JO JP KG KH KP KR KW KZ LA LB LK MM MN MO MV MY NP OM PH PK PS QA SA SG SY TH TJ TL TM TR TW UZ VN YE";
    const AFRICA: &str = "AO BF BI BJ BW CD CF CG CI CM CV DJ DZ EG EH ER ET GA GH GM GN GQ GW KE KM LR LS LY MA MG ML MR MU MW MZ NA NE NG RE RW SC SD SH SL SN SO SS ST SZ TD TG TN TZ UG YT ZA ZM ZW";
    const NORTH_AMERICA: &str = "AG AI AW BB BL BM BQ BS BZ CA CR CU CW DM DO GD GL GP GT HN HT JM KN KY LC MF MQ MS MX NI PA PM PR SV SX TC TT US VC VG VI";
    const SOUTH_AMERICA: &str = "AR BO BR CL CO EC FK GF GY PE PY SR UY VE";
    const OCEANIA: &str = "AS AU CK FJ FM GU KI MH MP NC NF NR NU NZ PF PG PN PW SB TK TO TV UM VU WF WS";
    const ANTARCTICA: &str = "AQ BV GS HM TF";
    let c = code.to_uppercase();
    [
        (EUROPE, "Europe"),
        (ASIA, "Asia"),
        (AFRICA, "Africa"),
        (NORTH_AMERICA, "North America"),
        (SOUTH_AMERICA, "South America"),
        (OCEANIA, "Oceania"),
        (ANTARCTICA, "Antarctica"),
    ]
    .iter()
    .find(|(list, _)| list.split(' ').any(|x| x == c))
    .map(|x| x.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countries_lie_on_continents() {
        assert_eq!(continent("de"), Some("Europe"));
        assert_eq!(continent("BR"), Some("South America"));
        assert_eq!(continent("ZZ"), None);
    }
}
