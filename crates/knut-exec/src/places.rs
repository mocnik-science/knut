//! Place names for `within Vienna` / `within AT` (KNUT_LANG.md §7.2): boundary and place
//! relations matched by name or ISO code.

use crate::eval::{Ctx, lock};
use crate::region::Region;
use crate::scan::{self, Item, KindMask, ScanSpec, TimeMode};
use crate::value::*;
use knut_lang::diag::{self, Diagnostic, Header, Span, Text2};
use knut_plan::pushdown::TagPred;
use std::sync::{Arc, Mutex};

const NAME_KEYS: &[(&str, u8)] = &[
    ("name", 0),
    ("ISO3166-1", 0),
    ("ISO3166-1:alpha2", 0),
    ("ISO3166-1:alpha3", 0),
    ("ISO3166-2", 0),
    ("name:en", 1),
    ("int_name", 1),
    ("name:de", 2),
    ("official_name", 2),
    ("short_name", 2),
    ("alt_name", 3),
];

struct Cand {
    elem: Arc<Elem>,
    prio: u8,
    level: u32,
    admin: bool,
}

fn admin_level(e: &Elem) -> u32 {
    e.tag("admin_level").and_then(|s| s.parse().ok()).unwrap_or(99)
}

/// The region for a place name (cached).
pub fn region_value(ctx: &Ctx, name: &str, span: Span) -> R<Value> {
    if let Some(v) = lock(&ctx.caches.places).get(name) {
        return v.clone();
    }
    let r = lookup(ctx, name, span);
    lock(&ctx.caches.places).insert(name.to_string(), r.clone());
    r
}

fn lookup(ctx: &Ctx, name: &str, span: Span) -> R<Value> {
    let db = ctx.db()?;
    let spec = ScanSpec {
        kinds: KindMask { relation: true, ..Default::default() },
        time: TimeMode::State(db.latest),
        region: None,
        pred: TagPred::Or(vec![TagPred::Has("boundary".into()), TagPred::Has("place".into()), TagPred::Has("ISO3166-1".into())]),
        need_geom: false,
        need_hist: false,
    };
    let lname = name.to_lowercase();
    let all_names: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let parts = scan::fold(db, &spec, &Vec::new, &|acc: &mut Vec<Cand>, it: Item| {
        let Item::Elem(e) = it else { return Ok(()) };
        if !crate::areas::relation_is_area(&e.v.tags) && e.tag("boundary").is_none() {
            return Ok(());
        }
        let mut best: Option<u8> = None;
        for (k, p) in NAME_KEYS {
            if let Some(v) = e.tag(k)
                && v.to_lowercase() == lname
            {
                best = Some(best.map_or(*p, |b| b.min(*p)));
            }
        }
        if let Some(n) = e.tag("name")
            && admin_level(&e) <= 8
        {
            lock(&all_names).push(n.to_string());
        }
        if let Some(p) = best {
            let admin = e.tag("boundary").is_some_and(|b| b.as_ref() == "administrative");
            acc.push(Cand { level: admin_level(&e), elem: e, prio: p, admin });
        }
        Ok(())
    })?;
    let mut cands: Vec<Cand> = parts.into_iter().flatten().collect();
    if cands.is_empty() {
        let names = all_names.into_inner().unwrap_or_default();
        let mut d = Diagnostic::error(
            "E210",
            span,
            Text2::new(format!("No region called \"{name}\" is known."), format!("Eine Region namens \"{name}\" ist nicht bekannt.")),
        )
        .why(
            "Regions are found by the name or ISO code of boundaries in the database.",
            "Regionen werden über Namen oder ISO-Codes von Grenzen in der Datenbank gefunden.",
        );
        if let Some(c) = diag::closest(name, names.iter().map(|s| s.as_str())) {
            d = d.header(Header::DidYouMean).sugg(format!("within '{c}'"));
        }
        return Err(RtErr::fatal(d));
    }
    cands.sort_by_key(|c| (c.prio, !c.admin, c.level, c.elem.id));
    let best = (cands[0].prio, cands[0].admin, cands[0].level);
    let tied: Vec<&Cand> = cands.iter().filter(|c| (c.prio, c.admin, c.level) == best).collect();
    if tied.len() > 1 {
        let mut d = Diagnostic::error(
            "E211",
            span,
            Text2::new(
                format!("\"{name}\" matches {} regions.", tied.len()),
                format!("\"{name}\" passt auf {} Regionen.", tied.len()),
            ),
        )
        .why("Choose one by its relation id, for example:", "Wähle eine über ihre Relations-ID, zum Beispiel:");
        for c in tied.iter().take(5) {
            let label = c.elem.tag("name").map(|s| s.to_string()).unwrap_or_default();
            d.suggestions.push(diag::Sugg {
                label: None,
                code: format!("within (pick area a with a.id == {} and a.osmType == 'relation')", c.elem.id),
                note: Some(Text2::same(format!("{label}, admin_level {}", c.level))),
            });
        }
        return Err(RtErr::fatal(d));
    }
    let e = &tied[0].elem;
    let mut ar = Elem::new(EKind::Area, e.otype, e.id, e.v.clone(), e.at);
    ar.bbox = e.bbox;
    let g = ctx.elem_geom(&ar);
    if g.is_empty() {
        return Err(RtErr::fatal(Diagnostic::error(
            "E210",
            span,
            Text2::new(
                format!("The boundary of \"{name}\" is incomplete in this database."),
                format!("Die Grenze von \"{name}\" ist in dieser Datenbank unvollständig."),
            ),
        )));
    }
    Ok(Value::Region(Arc::new(Region::from_geom(g, name.to_string()))))
}
