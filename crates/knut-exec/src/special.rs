//! Special forms (KNUT_LANG.md §3.6) and the non-aggregating `… of …` functions.

use crate::eval::{Ctx, Sc, lock};
use crate::fields;
use crate::geom::BBoxF;
use crate::net::Metric;
use crate::region::Region;
use crate::scan::{self, Item, KindMask, ScanSpec, TimeMode};
use crate::set::{Hints, SetV};
use crate::spatial;
use crate::stats;
use crate::value::*;
use knut_lang::ast::*;
use knut_lang::dates;
use knut_lang::diag::{Diagnostic, Span, Text2};
use knut_lang::units::Dim;
use knut_plan::pushdown::TagPred;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::sync::Arc;

fn rec(fields: Vec<(&str, Value)>) -> Value {
    Value::Record(Arc::new(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
}

/// Pairs of numbers from two collections: grouped results are joined on their keys,
/// lists are paired by position.
fn paired(ctx: &Ctx, a: &Value, b: &Value, span: Span) -> R<Vec<(Value, Value)>> {
    match (a, b) {
        (Value::Grouped(x), Value::Grouped(y)) => {
            let m: HashMap<Key, &Value> = y.rows.iter().map(|r| (r.key.key(), &r.value)).collect();
            Ok(x.rows.iter().filter_map(|r| m.get(&r.key.key()).map(|v| (r.value.clone(), (*v).clone()))).collect())
        }
        _ => {
            let xa = ctx.items_of(a, Hints::default(), span)?;
            let xb = ctx.items_of(b, Hints::default(), span)?;
            Ok(xa.iter().cloned().zip(xb.iter().cloned()).collect())
        }
    }
}

fn nums(ctx: &Ctx, pairs: &[(Value, Value)], span: Span) -> R<(Vec<f64>, Vec<f64>)> {
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for (a, b) in pairs {
        let (Some(x), Some(y)) = (ctx.to_num(a, None, span)?, ctx.to_num(b, None, span)?) else { continue };
        xs.push(x.v);
        ys.push(y.v);
    }
    Ok((xs, ys))
}

fn point(ctx: &Ctx, v: &Value, span: Span) -> R<crate::geom::P> {
    if let Value::Set(_) | Value::List(_) = v {
        let items = ctx.items_of(v, Hints::default(), span)?;
        if items.len() == 1 {
            return point(ctx, &items[0], span);
        }
        return Err(RtErr::data(
            Diagnostic::error(
                "E108",
                span,
                Text2::new(
                    format!("A single place is expected here, but this is a set of {} elements.", items.len()),
                    format!("Hier wird ein einzelner Ort erwartet, das ist aber eine Menge mit {} Elementen.", items.len()),
                ),
            )
.why("Choose one of them with \"pick\" or \"the\".", "Wähle eines davon mit \"pick\" oder \"the\" aus."),
        ));
    }
    ctx.rep_point(v).ok_or_else(|| {
        RtErr::data(Diagnostic::error(
            "E108",
            span,
            Text2::new(format!("{} has no location.", v.display()), format!("{} hat keinen Ort.", v.display())),
        ))
    })
}

pub fn eval(ctx: &Ctx, e: &'static Expr, s: &'static Special, sc: &Sc) -> R<Value> {
    match s {
        Special::Dataset(f) => crate::dataset::load(ctx, f, e.span),
        Special::Path { from, to, mode, .. } => {
            let a = ctx.eval(from, sc)?;
            let b = ctx.eval(to, sc)?;
            if a.is_none() || b.is_none() {
                return Ok(Value::None);
            }
            let (pa, pb) = (point(ctx, &a, from.span)?, point(ctx, &b, to.span)?);
            // the routing network of the container, if there is one
            if mode.using.is_none()
                && let Some(v) = stored_path(ctx, pa, pb, mode.mode)?
            {
                return Ok(v);
            }
            let mut bb = BBoxF::EMPTY;
            bb.add(pa);
            bb.add(pb);
            let d = crate::geom::haversine(pa, pb);
            // The network of a box around both points; the box grows until the route found
            // is certainly the shortest: a route that leaves the box takes at least the time
            // to its border from both ends at the highest speed.
            let extent = ctx.db().map(|db| db.extent).unwrap_or(BBoxF::WORLD);
            // On foot and by bicycle all ways are about equally fast: a margin of 0.7 times
            // the distance usually proves the route at once (detours up to 40 %).
            let mut margin = match mode.mode {
                Mode::Foot | Mode::Bicycle => (d * 0.7).max(2_000.0),
                _ => (d * 0.3).max(5_000.0),
            };
            loop {
                let bx = bb.expand_m(margin);
                let net = spatial::network(ctx, mode.mode, mode.using.as_deref(), bx, sc, e.span)?;
                let whole = mode.using.is_some() || bx.contains_box(&extent);
                let found = net.path(pa, pb, mode.mode);
                let vmax = net.max_speed(mode.mode).max(0.1);
                let border = |p: crate::geom::P| {
                    let k = 111_195.0;
                    let kx = k * p.1.to_radians().cos();
                    ((p.0 - bx.minx) * kx).min((bx.maxx - p.0) * kx).min((p.1 - bx.miny) * k).min((bx.maxy - p.1) * k)
                };
                let bound = (border(pa) + border(pb)) / vmax;
                match found {
                    Some((len, secs, ways, g)) if whole || secs <= bound => {
                        return Ok(Value::Path(Arc::new(PathV { length: len, duration: secs, ways, geom: Arc::new(g) })));
                    }
                    None if whole => return Ok(Value::None),
                    _ => margin *= 2.0,
                }
            }
        }
        Special::Isochrone { from, threshold, .. } => {
            let a = ctx.eval(from, sc)?;
            if a.is_none() {
                return Ok(Value::None);
            }
            let p = point(ctx, &a, from.span)?;
            let tv = ctx.eval(&threshold.value, sc)?;
            let n = ctx.to_num(&tv, None, threshold.value.span)?.unwrap_or(Num::plain(0.0));
            let Some(m) = &threshold.mode else {
                return Err(RtErr::fatal(
                    Diagnostic::error("E302", threshold.span, Text2::new("An isochrone needs a way of moving.", "Eine Isochrone braucht eine Fortbewegungsart."))
                        .why("For example:", "Zum Beispiel:")
                        .sugg(format!("{} by foot", ctx.text(threshold.span))),
                ));
            };
            let metric = if n.dim == Dim::TIME { Metric::Time } else { Metric::Length };
            let sp = match m.mode {
                Mode::Foot => 5.0 / 3.6,
                Mode::Bicycle => 15.0 / 3.6,
                _ => 130.0 / 3.6,
            };
            let radius = match metric {
                Metric::Time => n.v * sp,
                Metric::Length => n.v,
            };
            let cell = (radius / 200.0).clamp(25.0, 500.0);
            // the routing network of the container, if there is one
            if m.using.is_none()
                && let Some(v) = stored_isochrone(ctx, p, m.mode, metric, n.v, cell)?
            {
                return Ok(v);
            }
            // otherwise the network of the ways around the point (only then: reading them
            // takes longer than the isochrone in the stored network)
            let mut bb = BBoxF::EMPTY;
            bb.add(p);
            let net = spatial::network(ctx, m.mode, m.using.as_deref(), bb.expand_m(radius * 1.05 + 500.0), sc, e.span)?;
            Ok(match net.isochrone(p, m.mode, metric, n.v, cell) {
                Some(r) => Value::Region(Arc::new(Region::Raster(r))),
                None => Value::None,
            })
        }
        Special::Split { set, by, at } => {
            let sv = ctx.eval(set, sc)?;
            let items = ctx.items_of(&sv, Hints::default(), set.span)?;
            let mut keyed: Vec<(Key, i64, Value)> = Vec::with_capacity(items.len());
            for it in items.iter() {
                let isc = ctx.bind(sc, "_", it.clone(), true);
                let k = ctx.eval(by, &isc)?.key();
                keyed.push((k, fields::time_of(ctx, it).unwrap_or(0), it.clone()));
            }
            keyed.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
            let mut out: Vec<Value> = Vec::new();
            let mut cur: Vec<Value> = Vec::new();
            let mut cur_key: Option<Key> = None;
            for (k, _, it) in keyed {
                let isc = ctx.bind(sc, "_", it.clone(), true);
                let new_group = cur_key.as_ref() != Some(&k) || ctx.truthy(at, &isc)?;
                if new_group && !cur.is_empty() {
                    out.push(Value::Set(Arc::new(SetV::from_vec(std::mem::take(&mut cur)))));
                }
                cur_key = Some(k);
                cur.push(it);
            }
            if !cur.is_empty() {
                out.push(Value::Set(Arc::new(SetV::from_vec(cur))));
            }
            Ok(Value::List(Arc::new(out)))
        }
        Special::Correlate(a, b) => {
            let (va, vb) = (ctx.eval(a, sc)?, ctx.eval(b, sc)?);
            let (xs, ys) = nums(ctx, &paired(ctx, &va, &vb, e.span)?, e.span)?;
            Ok(stats::pearson(&xs, &ys).map_or(Value::None, Value::num))
        }
        Special::Regress(a, b) => {
            let (va, vb) = (ctx.eval(a, sc)?, ctx.eval(b, sc)?);
            let (ys, xs) = nums(ctx, &paired(ctx, &va, &vb, e.span)?, e.span)?;
            Ok(match stats::ols(&xs, &ys) {
                Some((icpt, slope, r2, n)) => rec(vec![("slope", Value::num(slope)), ("intercept", Value::num(icpt)), ("r2", Value::num(r2)), ("n", Value::num(n as f64))]),
                None => Value::None,
            })
        }
        Special::Confusion(a, b) => {
            let (va, vb) = (ctx.eval(a, sc)?, ctx.eval(b, sc)?);
            let pairs = paired(ctx, &va, &vb, e.span)?;
            let mut m: HashMap<(Key, Key), (Value, Value, u64)> = HashMap::default();
            for (x, y) in pairs {
                let x = if let Value::Tag(t) = x { Value::Text(t.text.clone()) } else { x };
                let y = if let Value::Tag(t) = y { Value::Text(t.text.clone()) } else { y };
                m.entry((x.key(), y.key())).or_insert_with(|| (x.clone(), y.clone(), 0)).2 += 1;
            }
            let mut rows: Vec<((Key, Key), (Value, Value, u64))> = m.into_iter().collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(Value::Grouped(Arc::new(Grouped {
                key_names: vec![ctx.text(a.span).to_string(), ctx.text(b.span).to_string()],
                value_name: "count".into(),
                rows: rows
                    .into_iter()
                    .map(|(_, (x, y, n))| Group { key: Value::List(Arc::new(vec![x, y])), value: Value::num(n as f64), count: n, items: None })
                    .collect(),
            })))
        }
        Special::FitPowerlaw(a) => {
            let v = ctx.eval(a, sc)?;
            let items = ctx.items_of(&v, Hints::default(), a.span)?;
            let mut xs = Vec::new();
            for it in items.iter() {
                if let Some(n) = ctx.to_num(it, None, a.span)? {
                    xs.push(n.v);
                }
            }
            Ok(match stats::powerlaw(&xs) {
                Some((alpha, xmin, n)) => rec(vec![("alpha", Value::num(alpha)), ("xmin", Value::num(xmin)), ("n", Value::num(n as f64))]),
                None => Value::None,
            })
        }
        Special::Forecast { n, unit, of } => {
            let v = ctx.eval(of, sc)?;
            let Value::Grouped(g) = &v else {
                return Err(RtErr::fatal(
                    Diagnostic::error("E601", of.span, Text2::new("\"forecast\" needs a time series.", "\"forecast\" braucht eine Zeitreihe."))
                        .why("For example:", "Zum Beispiel:")
                        .sugg(format!("forecast {n} {}s of (count of x in xs per {})", unit.name(), unit.name())),
                ));
            };
            let mut xs = Vec::new();
            let mut ys = Vec::new();
            let mut last: Option<i64> = None;
            for (i, r) in g.rows.iter().enumerate() {
                if let Some(y) = ctx.to_num(&r.value, None, of.span)? {
                    xs.push(i as f64);
                    ys.push(y.v);
                }
                if let Value::Time(d) = &r.key {
                    last = Some(last.map_or(d.start, |l: i64| l.max(d.start)));
                }
            }
            let Some((a, b, _, _)) = stats::ols(&xs, &ys) else { return Ok(Value::None) };
            let tmpl = g.rows.iter().find_map(|r| if let Value::Num(n) = &r.value { Some(*n) } else { None });
            let mut rows = Vec::new();
            let mut t = last.unwrap_or(0);
            for k in 0..*n as usize {
                t = dates::next_bucket(dates::bucket(t, *unit).start, *unit);
                let x = (g.rows.len() + k) as f64;
                let y = a + b * x;
                let value = match tmpl {
                    Some(n) => Value::Num(Num { v: y, ..n }),
                    None => Value::num(y),
                };
                rows.push(Group { key: Value::Time(dates::bucket(t, *unit)), value, count: 0, items: None });
            }
            Ok(Value::Grouped(Arc::new(Grouped { key_names: vec![unit.name().into()], value_name: "forecast".into(), rows })))
        }
        Special::Geocode { one, query } => match ctx.eval(query, sc)? {
            Value::None => Ok(Value::None),
            q => {
                let q = match &q {
                    Value::Text(t) => t.to_string(),
                    Value::Tag(t) => t.text.to_string(),
                    other => other.display(),
                };
                crate::address::geocode(ctx, &q, *one)
            }
        },
        Special::ReverseGeocode { one, at } => {
            let a = ctx.eval(at, sc)?;
            if a.is_none() {
                return Ok(Value::None);
            }
            let p = point(ctx, &a, at.span)?;
            crate::address::reverse(ctx, p, *one)
        }
        Special::Classify { set, label, features } => {
            let sv = ctx.eval(set, sc)?;
            let lf = ctx.eval(label, sc)?;
            let ff = ctx.eval(features, sc)?;
            let items = ctx.items_of(&sv, Hints { geom: true, ..Default::default() }, set.span)?;
            ctx.preload_geoms(&items);
            // `x.lon, x.lat` next to each other in the feature list form one place
            let pairs = lon_lat_pairs(features);
            let mut train = Vec::new();
            let mut query = Vec::new();
            let mut qitems = Vec::new();
            let mut shape: Option<Vec<bool>> = None;
            for it in items.iter() {
                let fv = ctx.apply(&ff, vec![it.clone()], features.span)?;
                let fl = ctx.items_of(&fv, Hints::default(), features.span)?;
                let mut x = Vec::new();
                let mut ok = true;
                let mut i = 0;
                while i < fl.len() {
                    let f = &fl[i];
                    if let Some(&(lon_first, _)) = pairs.iter().find(|p| p.1 == i) {
                        let (a, b) = (ctx.to_num(f, None, features.span), fl.get(i + 1).map(|g| ctx.to_num(g, None, features.span)));
                        match (a, b) {
                            (Ok(Some(a)), Some(Ok(Some(b)))) => {
                                x.push(stats::Feature::Place(if lon_first { (a.v, b.v) } else { (b.v, a.v) }));
                                i += 2;
                                continue;
                            }
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    match f {
                        Value::Elem(_) | Value::Region(_) => match ctx.rep_point(f) {
                            Some(p) => x.push(stats::Feature::Place(p)),
                            None => {
                                ok = false;
                                break;
                            }
                        },
                        _ => match ctx.to_num(f, None, features.span) {
                            Ok(Some(n)) => x.push(stats::Feature::Num(n.v)),
                            _ => {
                                ok = false;
                                break;
                            }
                        },
                    }
                    i += 1;
                }
                let s: Vec<bool> = x.iter().map(|f| matches!(f, stats::Feature::Place(_))).collect();
                if !ok || shape.get_or_insert_with(|| s.clone()) != &s {
                    continue;
                }
                let l = match ctx.apply(&lf, vec![it.clone()], label.span) {
                    Ok(v) => v,
                    Err(e) if e.catchable => Value::None,
                    Err(e) => return Err(e),
                };
                let l = if let Value::Tag(t) = l { Value::Text(t.text.clone()) } else { l };
                if l.is_none() {
                    query.push(x);
                    qitems.push(it.clone());
                } else {
                    train.push((x, l));
                }
            }
            let (preds, acc) = stats::knn(&train, &query);
            let rows: Vec<Group> = qitems
                .into_iter()
                .zip(preds)
                .map(|(it, p)| Group { key: it, value: p.unwrap_or(Value::None), count: 1, items: None })
                .collect();
            let g = Value::Grouped(Arc::new(Grouped { key_names: vec!["element".into()], value_name: "predicted".into(), rows }));
            Ok(rec(vec![("predictions", g), ("accuracy", acc.map_or(Value::None, Value::num)), ("trained on", Value::num(train.len() as f64))]))
        }
    }
}

pub fn non_aggregate(ctx: &Ctx, e: &'static Expr, func: OfFunc, fsp: Span, operand: &'static OfOperand, cl: &'static Clauses, sc: &Sc) -> R<Value> {
    let _ = fsp;
    // `nearest of SELECTION to x`: only around x, not the whole selection (also for an `x`
    // that changes in an iteration: the boxes cost about as much as reading the selection once
    // where it is dense, and far less where only a part of it is near the places)
    if func == OfFunc::Nearest
        && let OfOperand::Expr(x) = operand
        && matches!(&x.kind, ExprKind::Select(_))
        && let Some(t) = &cl.to
    {
        let origin = ctx.eval(t, sc)?;
        if !origin.is_none() {
            let mode = cl.mode.as_ref().map(|m| (m.mode, m.using.as_deref()));
            if let Some(v) = spatial::nearest_around(ctx, x, &origin, mode, sc, e.span)? {
                return Ok(v);
            }
        }
    }
    let v = match operand {
        OfOperand::Expr(x) => ctx.eval(x, sc)?,
        OfOperand::Iter { set, .. } => ctx.eval(set, sc)?,
    };
    match func {
        OfFunc::User => match &v {
            Value::Elem(_) | Value::Tuple(_) => ctx.field(&v, "user", e.span),
            Value::None => Ok(Value::None),
            _ => {
                let items = ctx.items_of(&v, Hints::default(), e.span)?;
                let mut out = Vec::new();
                for it in items.iter() {
                    let u = ctx.field(it, "user", e.span)?;
                    if !u.is_none() {
                        out.push(u);
                    }
                }
                Ok(Value::Set(Arc::new(SetV::from_vec(out))))
            }
        },
        OfFunc::FirstVersion | OfFunc::LastVersion => match &v {
            Value::Elem(el) => {
                let Some(h) = fields::hist(ctx, el) else { return Ok(v.clone()) };
                let ver = if func == OfFunc::FirstVersion { h.versions.first() } else { h.versions.last() };
                Ok(match ver {
                    Some(x) => {
                        let n = Elem::new(el.kind, el.otype, el.id, x.clone(), x.ts);
                        let _ = n.hist.set(h.clone());
                        Value::Elem(Arc::new(n))
                    }
                    None => Value::None,
                })
            }
            _ => Ok(Value::None),
        },
        OfFunc::Nearest => {
            let set = ctx.as_set(&v, e.span)?;
            let origin = match &cl.to {
                Some(t) => ctx.eval(t, sc)?,
                None => ctx.implicit(sc).unwrap_or(Value::None),
            };
            if origin.is_none() {
                return Err(RtErr::fatal(
                    Diagnostic::error("E108", e.span, Text2::new("\"nearest of\" needs a starting point.", "\"nearest of\" braucht einen Ausgangspunkt."))
                        .why("Use it inside an iteration or add \"to\":", "Nutze es in einer Iteration oder ergänze \"to\":")
                        .sugg(format!("{} to x", ctx.text(e.span))),
                ));
            }
            let mode = cl.mode.as_ref().map(|m| (m.mode, m.using.as_deref()));
            spatial::nearest(ctx, &set, &origin, mode, sc, e.span)
        }
        OfFunc::First | OfFunc::Last => {
            if v.is_none() {
                return Ok(Value::None);
            }
            let items = ctx.items_of(&v, Hints::default(), e.span)?;
            Ok(if func == OfFunc::First { items.iter().next().cloned() } else { items.iter().last().cloned() }.unwrap_or(Value::None))
        }
        OfFunc::Graph => {
            let set = ctx.as_set(&v, e.span)?;
            Ok(Value::Net(Arc::new(spatial::net_from(ctx, &set, e.span)?)))
        }
        _ => unreachable!(),
    }
}

/// All keys used at time `t` with their number of elements and distinct values.
pub fn all_keys(ctx: &Ctx, t: i64, _span: Span) -> R<Value> {
    if let Some(v) = lock(&ctx.caches.keys).get(&t) {
        return Ok(v.clone());
    }
    let db = ctx.db()?;
    // the tag statistics answer the last moment
    let from_index = match db.reader.element(knut_format::container::ElementKind::TagStatistics) {
        Some(el) if t == db.latest => el.sections.first().and_then(|s| db.reader.section_bytes(s).ok()).and_then(decode_key_stats).filter(|x| x.0 == t),
        _ => None,
    };
    let keys = match from_index {
        Some((_, k)) => k,
        None => key_stats(db, ctx.control.as_deref(), t)?,
    };
    let vals: Vec<Value> = keys
        .into_iter()
        .map(|(k, n, nv)| {
            let rec = Arc::new(KeyRec { name: k.clone(), count: n, values: nv });
            let mut v = Ver::empty();
            v.ts = t;
            let id = (xxhash_rust::xxh3::xxh3_64(k.as_bytes()) >> 1) as i64;
            let mut el = Elem::new(EKind::Key, None, id, v, t);
            el.extra = Extra::Key(rec);
            Value::Elem(Arc::new(el))
        })
        .collect();
    let v = Value::Set(Arc::new(SetV::from_vec(vals)));
    lock(&ctx.caches.keys).insert(t, v.clone());
    Ok(v)
}

/// Every key in use at `t`: (key, elements with it, distinct values), sorted by key.
pub fn key_stats(db: &crate::db::Db, ctl: Option<&crate::control::Control>, t: i64) -> R<Vec<(Arc<str>, u64, u64)>> {
    let spec = ScanSpec {
        bbox: None,
        kinds: KindMask { node: true, way: true, relation: true, ..Default::default() },
        time: TimeMode::State(t),
        region: None,
        pred: TagPred::Not(Box::new(TagPred::False)),
        need_geom: false,
        need_hist: false,
        attrs_history: false,
    };
    type Acc = HashMap<Arc<str>, (u64, HashSet<u64>)>;
    let parts = scan::fold(db, ctl, &spec, &Acc::default, &|acc: &mut Acc, it: Item| {
        if let Item::Elem(e) = it {
            for (k, v) in e.v.tags.iter() {
                let ent = acc.entry(k.clone()).or_default();
                ent.0 += 1;
                ent.1.insert(xxhash_rust::xxh3::xxh3_64(v.as_bytes()));
            }
        }
        Ok(())
    })?;
    let mut all: Acc = HashMap::default();
    for p in parts {
        for (k, (n, vs)) in p {
            let ent = all.entry(k).or_default();
            ent.0 += n;
            ent.1.extend(vs);
        }
    }
    let mut v: Vec<(Arc<str>, u64, u64)> = all.into_iter().map(|(k, (n, vs))| (k, n, vs.len() as u64)).collect();
    v.sort();
    Ok(v)
}

const KEYS_MAGIC: &[u8; 8] = b"KKEYS001";

/// The section of the tag statistics (zstd): magic, i64 moment, u32 keys, then per key
/// u32 length, name, u64 count, u64 distinct values.
pub fn encode_key_stats(t: i64, keys: &[(Arc<str>, u64, u64)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(KEYS_MAGIC);
    b.extend_from_slice(&t.to_le_bytes());
    b.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for (k, n, nv) in keys {
        b.extend_from_slice(&(k.len() as u32).to_le_bytes());
        b.extend_from_slice(k.as_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&nv.to_le_bytes());
    }
    zstd::bulk::compress(&b, 19).unwrap_or(b)
}

#[allow(clippy::type_complexity)]
pub fn decode_key_stats(z: &[u8]) -> Option<(i64, Vec<(Arc<str>, u64, u64)>)> {
    let b = zstd::stream::decode_all(z).ok()?;
    if b.get(0..8)? != KEYS_MAGIC {
        return None;
    }
    let u64_at = |o: usize| b.get(o..o + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap()));
    let t = u64_at(8)? as i64;
    let n = u32::from_le_bytes(b.get(16..20)?.try_into().ok()?) as usize;
    let mut o = 20;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let l = u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize;
        let k: Arc<str> = Arc::from(std::str::from_utf8(b.get(o + 4..o + 4 + l)?).ok()?);
        o += 4 + l;
        out.push((k, u64_at(o)?, u64_at(o + 8)?));
        o += 16;
    }
    Some((t, out))
}

/// Positions in a feature list `x => [… x.lon, x.lat …]` where a `.lon`/`.lat` pair starts:
/// (lon comes first, position).
fn lon_lat_pairs(features: &Expr) -> Vec<(bool, usize)> {
    let ExprKind::Lambda(_, body) = &features.kind else { return Vec::new() };
    let ExprKind::List(xs) = &body.kind else { return Vec::new() };
    fn field(e: &Expr) -> Option<(&str, &ExprKind)> {
        match &e.kind {
            ExprKind::Field(base, name, _) => Some((name.as_str(), &base.kind)),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < xs.len() {
        match (field(&xs[i]), field(&xs[i + 1])) {
            (Some(("lon", a)), Some(("lat", b))) if a == b => {
                out.push((true, i));
                i += 2;
            }
            (Some(("lat", a)), Some(("lon", b))) if a == b => {
                out.push((false, i));
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

/// A route through the stored routing network (`None` if the container has none).
fn stored_path(ctx: &Ctx, pa: crate::geom::P, pb: crate::geom::P, m: Mode) -> R<Option<Value>> {
    let db = ctx.db()?;
    let Some(net) = crate::routing::load(db) else { return Ok(None) };
    let (Some(a), Some(b)) = (crate::routing::snap(db, &net, pa, m), crate::routing::snap(db, &net, pb, m)) else {
        return Ok(Some(Value::None));
    };
    let route = crate::ch::route(db, &net, &a, &b, m).or_else(|| crate::routing::astar(&net, &a, &b, m));
    Ok(Some(match route {
        Some(r) => {
            let (len, ways, g) = crate::routing::draw(db, &net, &r, m);
            Value::Path(Arc::new(PathV { length: len, duration: r.seconds, ways, geom: Arc::new(g) }))
        }
        None => Value::None,
    }))
}

/// An isochrone in the stored routing network (`None` if the container has none).
fn stored_isochrone(ctx: &Ctx, p: crate::geom::P, m: Mode, metric: Metric, limit: f64, cell: f64) -> R<Option<Value>> {
    if !matches!(metric, Metric::Time) {
        return Ok(None);
    }
    let db = ctx.db()?;
    let Some(net) = crate::routing::load(db) else { return Ok(None) };
    let Some(s) = crate::routing::snap(db, &net, p, m) else { return Ok(Some(Value::None)) };
    let sources = crate::routing::entries(&net, &s, m, true);
    // a search bounded by the time first (it touches only what is reachable); if that is a
    // large share of the network, a sweep over the hierarchy (PHAST) is faster
    // (a sweep at once if even at the highest speed the area would cover the database)
    let r_max = limit * net.max_speed(m) / 1000.0;
    let bounded = std::f64::consts::PI * r_max * r_max < db.extent.area_km2();
    let (dist, reached) = match bounded.then(|| crate::routing::reach_bounded(&net, &sources, m, limit, net.n() / 10)).flatten() {
        Some((d, r)) => (d, Some(r)),
        None => (crate::ch::reach(db, &net, &sources, m, limit).unwrap_or_else(|| crate::routing::reach(&net, &sources, m, limit)), None),
    };
    let r = crate::routing::isochrone_raster(db, &net, &s, &dist, reached.as_deref(), m, limit, cell);
    Ok(Some(Value::Region(Arc::new(Region::Raster(r)))))
}
