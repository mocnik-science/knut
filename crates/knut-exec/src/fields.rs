//! Fields computed by the language (`.` access, KNUT_LANG.md §5.2a).

use crate::arith::duration;
use crate::eval::{Ctx, cap};
use crate::geom::Geom;
use crate::region::Region;
use crate::set::{Hints, SetV};
use crate::value::*;
use knut_format::ObjType;
use knut_lang::ast::{Kind, StmtKind};
use knut_lang::dates::{self, TimeUnit};
use knut_lang::diag::{Diagnostic, Span, Text2};
use knut_lang::units::{self, Dim};
use std::sync::Arc;

pub fn hist(ctx: &Ctx, e: &Elem) -> Option<Arc<Hist>> {
    let t = e.otype?;
    let db = ctx.db_arc()?;
    Some(e.hist.get_or_init(|| db.history(t, e.id)).clone())
}

/// The previous or next version of an element (same kind).
pub fn neighbour(ctx: &Ctx, e: &Elem, next: bool) -> Option<Arc<Elem>> {
    if let Extra::Changeset(_) = &e.extra {
        return crate::cs::neighbour(ctx, e, next);
    }
    let h = hist(ctx, e)?;
    let i = h.versions.iter().position(|v| v.version == e.v.version)?;
    let j = if next { i + 1 } else { i.checked_sub(1)? };
    let v = h.versions.get(j)?.clone();
    let ts = v.ts;
    let n = Elem::new(e.kind, e.otype, e.id, v, ts);
    let _ = n.hist.set(h);
    Some(Arc::new(n))
}

fn kind_matches(k: Kind, e: &Elem) -> bool {
    match k {
        Kind::Any => matches!(e.kind, EKind::Node | EKind::Way | EKind::Area | EKind::Relation),
        _ => EKind::from_kind(k) == Some(e.kind) || (k == Kind::Way && e.kind == EKind::Area && e.otype == Some(ObjType::Way)),
    }
}

fn unknown(ctx: &Ctx, v: &Value, f: &str, span: Span) -> RtErr {
    let _ = ctx;
    RtErr::fatal(Diagnostic::error(
        "E203",
        span,
        Text2::new(
            format!("\"{f}\" is not a field of {}.", v.type_name(false)),
            format!("\"{f}\" ist kein Feld von {}.", v.type_name(true)),
        ),
    ))
}

pub fn field(ctx: &Ctx, v: &Value, f: &str, span: Span) -> R<Value> {
    match v {
        Value::None => Ok(Value::None),
        Value::Elem(e) => elem_field(ctx, v, e, f, span),
        Value::Tuple(t) => match t.names.iter().position(|n| n == f) {
            Some(i) => Ok(Value::Elem(t.items[i].clone())),
            None => elem_field(ctx, &Value::Elem(t.items[0].clone()), &t.items[0], f, span),
        },
        Value::Tag(_) | Value::Text(_) => {
            let s = v.as_text().unwrap();
            Ok(match f {
                "length" => Value::num(s.chars().count() as f64),
                "number" => match units::parse_value(s) {
                    Some((x, _)) => Value::num(x),
                    None => return Err(ctx.e602(v, span)),
                },
                "exists" => Value::Bool(true),
                "lower" => Value::text(&s.to_lowercase()),
                "upper" => Value::text(&s.to_uppercase()),
                _ => return Err(unknown(ctx, v, f, span)),
            })
        }
        Value::Set(_) | Value::List(_) => {
            let items = ctx.items_of(v, Hints::default(), span)?;
            Ok(match f {
                "count" => Value::num(items.len() as f64),
                "first" => items.first().cloned().unwrap_or(Value::None),
                "last" => items.last().cloned().unwrap_or(Value::None),
                _ => return Err(unknown(ctx, v, f, span)),
            })
        }
        Value::Grouped(g) => Ok(match f {
            "count" => Value::num(g.rows.len() as f64),
            "keys" => Value::List(Arc::new(g.rows.iter().map(|r| r.key.clone()).collect())),
            "values" => Value::List(Arc::new(g.rows.iter().map(|r| r.value.clone()).collect())),
            _ => return Err(unknown(ctx, v, f, span)),
        }),
        Value::Group(g) => Ok(match f {
            "key" => g.key.clone(),
            "value" => g.value.clone(),
            "count" => match &g.value {
                Value::Num(_) if g.count == 0 => g.value.clone(),
                _ => Value::num(g.count as f64),
            },
            "items" => g.items.clone().unwrap_or(Value::None),
            _ => field(ctx, &g.key, f, span)?,
        }),
        Value::Path(p) => Ok(match f {
            "length" => Value::Num(Num::with_unit(p.length / 1000.0, units::unit("km").unwrap())),
            "duration" => Value::Num(duration(p.duration)),
            "edges" => Value::num(p.ways.len() as f64),
            "geometry" => Value::Region(Arc::new(Region::from_geom(p.geom.clone(), "path".into()))),
            _ => return Err(unknown(ctx, v, f, span)),
        }),
        Value::Region(r) => Ok(match f {
            "area" => Value::Num(area_num(r.area_m2())),
            "length" => Value::Num(Num::base(r.geom().length_m(), Dim::LENGTH)),
            _ => return Err(unknown(ctx, v, f, span)),
        }),
        Value::Record(r) => Ok(r.iter().find(|(k, _)| k == f).map_or(Value::None, |(_, x)| x.clone())),
        Value::Time(d) => {
            let (y, m, dd) = knut_format::time::civil_from_days(d.start.div_euclid(86_400));
            Ok(match f {
                "year" => Value::num(y as f64),
                "month" => Value::num(m as f64),
                "day" => Value::num(dd as f64),
                _ => return Err(unknown(ctx, v, f, span)),
            })
        }
        Value::Num(n) => Ok(match f {
            "number" | "value" => Value::num(n.shown()),
            _ => return Err(unknown(ctx, v, f, span)),
        }),
        _ => Err(unknown(ctx, v, f, span)),
    }
}

fn area_num(m2: f64) -> Num {
    Num { v: m2, dim: Dim::AREA, unit: units::unit("m2") }
}

fn elem_field(ctx: &Ctx, v: &Value, e: &Arc<Elem>, f: &str, span: Span) -> R<Value> {
    // user-defined fields first (§5.9)
    for d in &ctx.chk.defines {
        if d.field == f && kind_matches(d.kind, e) {
            let StmtKind::Define { body, .. } = &ctx.chk.script.stmts[d.stmt].kind else { continue };
            let sc = ctx.bind(&None, &d.var, v.clone(), true);
            return ctx.eval(body, &sc);
        }
    }
    match &e.extra {
        Extra::User(u) => return crate::cs::user_field(ctx, e, u, f, span),
        Extra::Changeset(c) => return crate::cs::changeset_field(ctx, e, c, f, span),
        Extra::Note(n) => {
            return Ok(match f {
                "id" => Value::num(n.id as f64),
                "status" => Value::Text(n.status.clone()),
                "text" => Value::Text(n.text.clone()),
                "created" | "time" => Value::instant(n.created),
                "closed" => n.closed.map_or(Value::None, Value::instant),
                "comments" => Value::num(n.comments as f64),
                "type" => Value::text("note"),
                "geometry" | "center" => Value::Region(Arc::new(Region::from_geom(Arc::new(Geom::Point((n.lon, n.lat))), format!("note {}", n.id)))),
                "lon" => Value::num(n.lon),
                "lat" => Value::num(n.lat),
                _ => return Err(unknown(ctx, v, f, span)),
            });
        }
        Extra::Key(k) => {
            return Ok(match f {
                "name" => Value::Text(k.name.clone()),
                "count" => Value::num(k.count as f64),
                "values" => Value::num(k.values as f64),
                "length" => Value::num(k.name.chars().count() as f64),
                "type" => Value::text("key"),
                _ => return Err(unknown(ctx, v, f, span)),
            });
        }
        _ => {}
    }
    Ok(match f {
        "type" => Value::text(e.kind.name()),
        "osmType" => Value::text(e.otype.map_or(e.kind.name(), |o| o.name())),
        "id" => Value::num(e.id as f64),
        "version" => Value::num(e.v.version as f64),
        "time" => Value::instant(e.v.ts),
        "deleted" => Value::Bool(!e.v.visible),
        "visible" => Value::Bool(e.v.visible),
        "name" => e.tag("name").map_or(Value::None, |s| Value::Text(s.clone())),
        "keys" => {
            let mut ks: Vec<Value> = e.v.tags.iter().map(|(k, _)| Value::Text(k.clone())).collect();
            ks.sort_by_key(|k| k.key());
            Value::List(Arc::new(ks))
        }
        "tags" => Value::Record(Arc::new(e.v.tags.iter().map(|(k, x)| (k.to_string(), Value::Text(x.clone()))).collect())),
        "user" => {
            if e.v.uid == 0 && e.kind != EKind::Row {
                Value::None
            } else {
                crate::cs::user_elem(ctx, e.v.uid)
            }
        }
        "creator" => {
            let h = hist(ctx, e);
            match h.and_then(|h| h.versions.first().map(|v| v.uid)) {
                Some(uid) if uid != 0 => crate::cs::user_elem(ctx, uid),
                _ => Value::None,
            }
        }
        "changeset" => crate::cs::changeset_elem(ctx, e.v.changeset, e.v.uid, e.v.ts),
        "next" | "previous" => neighbour(ctx, e, f == "next").map_or(Value::None, Value::Elem),
        "unchangedBefore" => match neighbour(ctx, e, false) {
            Some(p) => Value::Num(duration((e.v.ts - p.v.ts) as f64)),
            None => Value::None,
        },
        "unchangedAfter" => match neighbour(ctx, e, true) {
            Some(n) => Value::Num(duration((n.v.ts - e.v.ts) as f64)),
            None => Value::None,
        },
        "versions" => {
            let h = hist(ctx, e);
            match h {
                Some(h) => Value::List(Arc::new(
                    h.versions
                        .iter()
                        .map(|x| {
                            let n = Elem::new(e.kind, e.otype, e.id, x.clone(), x.ts);
                            let _ = n.hist.set(h.clone());
                            Value::Elem(Arc::new(n))
                        })
                        .collect(),
                )),
                None => Value::None,
            }
        }
        "geometry" => Value::Region(Arc::new(Region::from_geom(ctx.elem_geom(e), e.label()))),
        "center" => match ctx.rep_point(v) {
            Some(p) => Value::Region(Arc::new(Region::from_geom(Arc::new(Geom::Point(p)), format!("center of {}", e.label())))),
            None => Value::None,
        },
        "lon" => ctx.rep_point(v).map_or(Value::None, |p| Value::num(p.0)),
        "lat" => ctx.rep_point(v).map_or(Value::None, |p| Value::num(p.1)),
        "length" => Value::Num(Num { v: ctx.elem_geom(e).length_m(), dim: Dim::LENGTH, unit: units::unit("m") }),
        "area" => Value::Num(area_num(ctx.elem_geom(e).area_m2())),
        "nodes" => {
            let mut ids: Vec<i64> = e.v.nodes.to_vec();
            // ways decoded without geometry carry no node list
            if ids.is_empty() && e.otype == Some(ObjType::Way) && e.v.visible {
                if let Ok(db) = ctx.db() {
                    if let Some(Some((nids, _))) = db.way_geoms(&[(e.id, e.at)]).get(&(e.id, e.at)) {
                        ids = nids.clone();
                    }
                }
            }
            let reqs: Vec<(i64, i64)> = ids.iter().map(|&i| (i, e.at)).collect();
            let pos = ctx.db().map(|db| db.positions(&reqs)).unwrap_or_default();
            Value::List(Arc::new(
                ids.iter()
                    .map(|&id| {
                        let mut ver = Ver::empty();
                        ver.pos = pos.get(&(id, e.at)).copied().flatten();
                        ver.ts = e.at;
                        Value::Elem(Arc::new(Elem::new(EKind::Node, Some(ObjType::Node), id, ver, e.at)))
                    })
                    .collect(),
            ))
        }
        "members" => Value::List(Arc::new(
            e.v.members
                .iter()
                .map(|m| {
                    let k = match m.kind {
                        ObjType::Node => EKind::Node,
                        ObjType::Way => EKind::Way,
                        ObjType::Relation => EKind::Relation,
                    };
                    let mut ver = Ver::empty();
                    ver.ts = e.at;
                    Value::Elem(Arc::new(Elem::new(k, Some(m.kind), m.id, ver, e.at)))
                })
                .collect(),
        )),
        "roles" => Value::List(Arc::new(e.v.members.iter().map(|m| Value::Text(m.role.clone())).collect())),
        "count" => Value::num(1.0),
        _ => {
            let mut d = Diagnostic::error(
                "E203",
                span,
                Text2::new(format!("\"{f}\" is not a field of {}.", e.kind.plural()), format!("\"{f}\" ist kein Feld von {}.", e.kind.plural())),
            );
            if let Some(b) = knut_lang::diag::closest(f, knut_plan::types::all_fields(None)) {
                d = d.why(format!("Did you mean \".{b}\"?"), format!("Meintest du \".{b}\"?"));
            }
            return Err(RtErr::fatal(d));
        }
    })
}

/// Distinct buckets of the times of a set (`days of X`).
pub fn buckets_of(ctx: &Ctx, items: &[Value], u: TimeUnit) -> Value {
    let mut out = Vec::new();
    for it in items {
        if let Some(t) = time_of(ctx, it) {
            out.push(Value::Time(dates::bucket(t, u)));
        }
    }
    Value::Set(Arc::new(SetV::from_vec(out)))
}

/// The time of an item: element timestamp, changeset start, date value.
pub fn time_of(ctx: &Ctx, v: &Value) -> Option<i64> {
    let _ = ctx;
    match v {
        Value::Elem(e) => Some(match &e.extra {
            Extra::Changeset(c) => c.t0,
            Extra::Note(n) => n.created,
            _ => e.v.ts,
        }),
        Value::Time(d) => Some(d.start),
        Value::Tuple(t) => t.items.first().map(|e| e.v.ts),
        Value::Group(g) => time_of(ctx, &g.key),
        _ => None,
    }
}

pub fn label_kind(k: EKind) -> String {
    cap(k.name())
}
