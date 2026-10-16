//! `… of …` calls: aggregates with `with`, `within`, time, `per`, `having` (KNUT_LANG.md §5.8).

use crate::eval::{Ctx, Sc};
use crate::fields;
use crate::geom;
use crate::region::Region;
use crate::scan::TimeMode;
use crate::set::{Hints, Query, SetSrc, SetV};
use crate::value::*;
use knut_lang::ast::*;
use knut_lang::dates::{self, TimeUnit};
use knut_lang::diag::{Diagnostic, Header, Span, Text2};
use knut_lang::units::{self, Dim};
use knut_plan::pushdown::{self, TagPred};
use rustc_hash::FxHashMap as HashMap;
use std::sync::Arc;

/// Restricts a set by conditions, a region and a time reference. Database queries absorb
/// the region, the time and a tag prefilter into their scan where possible.
#[allow(clippy::too_many_arguments)]
pub fn restrict(
    ctx: &Ctx,
    base: Arc<SetV>,
    vars: Vec<String>,
    preds: Vec<&'static Expr>,
    scope: Sc,
    region: Option<Arc<Region>>,
    time: Option<&'static TimeRef>,
    span: Span,
) -> R<Arc<SetV>> {
    let _ = span;
    if preds.is_empty() && region.is_none() && time.is_none() {
        return Ok(base);
    }
    let mut base = base;
    let mut region = region;
    let mut time = time;
    if let Some(q) = base.is_query()
        && q.seq.is_none()
    {
        let mut spec = q.spec.clone();
        let default_time = matches!(spec.time, TimeMode::State(t) if t == ctx.latest());
        if let Some(t) = time
            && default_time
        {
            spec.time = ctx.time_mode(t, &scope)?.0;
            time = None;
        }
        if let Some(r) = &region
            && spec.region.is_none()
        {
            spec.region = Some(r.clone());
            region = None;
        }
        for p in &preds {
            for v in &vars {
                let x = pushdown::extract(p, v, ctx.chk);
                if !x.pred.is_true() {
                    spec.pred = if spec.pred.is_true() { x.pred } else { TagPred::And(vec![spec.pred.clone(), x.pred]) };
                }
                spec.need_geom |= pushdown::needs_geometry(p, v);
            }
        }
        let nq = Query { spec, vars: q.vars.clone(), preds: q.preds.clone(), scope: q.scope.clone(), seq: None };
        base = Arc::new(SetV::lazy(SetSrc::Query(Arc::new(nq))));
        if preds.is_empty() && region.is_none() && time.is_none() {
            return Ok(base);
        }
    }
    let tfilter: Option<(i64, i64)> = match time {
        Some(t @ TimeRef::Range(..)) => match ctx.time_mode(t, &scope)?.0 {
            TimeMode::Events(x, y) => Some((x, y)),
            _ => None,
        },
        Some(t @ TimeRef::On(_)) => match ctx.time_mode(t, &scope)?.0 {
            TimeMode::State(t) => Some((i64::MIN, t + 1)),
            _ => None,
        },
        _ => None,
    };
    let extra: Option<Arc<dyn Fn(&Ctx, &Value) -> R<bool> + Send + Sync>> = if region.is_some() || tfilter.is_some() {
        Some(Arc::new(move |ctx: &Ctx, v: &Value| -> R<bool> {
            if let Some((a, b)) = tfilter {
                match fields::time_of(ctx, v) {
                    Some(t) if t >= a && t < b => {}
                    _ => return Ok(false),
                }
            }
            if let Some(r) = &region {
                return Ok(ctx.rep_point(v).is_some_and(|p| r.contains(p)));
            }
            Ok(true)
        }))
    } else {
        None
    };
    Ok(Arc::new(SetV::lazy(SetSrc::Filter { base, vars, preds, scope, extra })))
}

// ----- accumulators -------------------------------------------------------------------------

#[derive(Clone)]
enum Acc {
    Count(u64),
    Sum(f64, Option<Num>, u64),
    Vals(Vec<Value>),
    Share(u64, u64),
}

impl Acc {
    fn merge(&mut self, o: Acc) {
        match (self, o) {
            (Acc::Count(a), Acc::Count(b)) => *a += b,
            (Acc::Sum(a, t, n), Acc::Sum(b, u, m)) => {
                *a += b;
                *n += m;
                if t.is_none() {
                    *t = u;
                }
            }
            (Acc::Vals(a), Acc::Vals(b)) => a.extend(b),
            (Acc::Share(a, b), Acc::Share(c, d)) => {
                *a += c;
                *b += d;
            }
            _ => {}
        }
    }
}

fn init_acc(f: OfFunc) -> Acc {
    match f {
        OfFunc::Count | OfFunc::Density => Acc::Count(0),
        OfFunc::Sum | OfFunc::Average => Acc::Sum(0.0, None, 0),
        OfFunc::Share => Acc::Share(0, 0),
        _ => Acc::Vals(Vec::new()),
    }
}

fn e607(func: OfFunc, span: Span, set_txt: &str) -> RtErr {
    let f = func.name();
    RtErr::fatal(
        Diagnostic::error(
            "E607",
            span,
            Text2::new(format!("\"{f} of\" needs numbers, but got elements."), format!("\"{f} of\" braucht Zahlen, hat aber Elemente bekommen.")),
        )
        .why("Turn the elements into numbers first, for example:", "Wandle die Elemente zuerst in Zahlen um, zum Beispiel:")
        .sugg(format!("{f} of ({set_txt}.map(x => x.area))")),
    )
}

/// Adds one item to an accumulator.
fn add(ctx: &Ctx, func: OfFunc, acc: &mut Acc, v: &Value, hit: bool, span: Span, set_txt: &str) -> R<()> {
    match acc {
        Acc::Count(n) => *n += 1,
        Acc::Share(h, t) => {
            *t += 1;
            if hit {
                *h += 1;
            }
        }
        Acc::Sum(s, tmpl, n) => {
            if v.is_none() {
                return Ok(());
            }
            if let Value::Elem(_) | Value::Tuple(_) = v {
                return Err(e607(func, span, set_txt));
            }
            let Some(x) = ctx.to_num(v, None, span)? else { return Ok(()) };
            if let Some(t) = tmpl
                && t.dim != x.dim
            {
                return Err(RtErr::fatal(Diagnostic::error(
                    "E306",
                    span,
                    Text2::new("These values have different measures and cannot be added.", "Diese Werte haben verschiedene Maße und lassen sich nicht addieren."),
                )));
            }
            *s += x.v;
            *n += 1;
            if tmpl.is_none() {
                *tmpl = Some(x);
            }
        }
        Acc::Vals(xs) => xs.push(v.clone()),
    }
    Ok(())
}

fn finish(ctx: &Ctx, func: OfFunc, acc: Acc, span: Span, set_txt: &str) -> R<Value> {
    Ok(match (func, acc) {
        (OfFunc::Count, Acc::Count(n)) => Value::num(n as f64),
        (OfFunc::Density, Acc::Count(n)) => Value::num(n as f64),
        (OfFunc::Share, Acc::Share(h, t)) => {
            if t == 0 {
                Value::None
            } else {
                Value::num(h as f64 / t as f64)
            }
        }
        (OfFunc::Sum, Acc::Sum(s, t, _)) => Value::Num(match t {
            Some(t) => Num { v: s, ..t },
            None => Num::plain(0.0),
        }),
        (OfFunc::Average, Acc::Sum(s, t, n)) => {
            if n == 0 {
                Value::None
            } else {
                Value::Num(match t {
                    Some(t) => Num { v: s / n as f64, ..t },
                    None => Num::plain(s / n as f64),
                })
            }
        }
        (f, Acc::Vals(vs)) => crate::stats::finish_values(ctx, f, vs, span, set_txt)?,
        _ => Value::None,
    })
}

// ----- grouping keys --------------------------------------------------------------------------

/// Spatial grouping by a set of areas: (bbox, region, key).
struct AreaIndex {
    areas: Vec<(geom::BBoxF, Arc<Region>, Value)>,
}

fn area_index(ctx: &Ctx, v: &Value, span: Span) -> R<AreaIndex> {
    let items = ctx.items_of(v, Hints { geom: true, ..Default::default() }, span)?;
    let mut areas = Vec::new();
    for it in items.iter() {
        let r = ctx.region_of(it, span)?;
        areas.push((r.bbox(), r, it.clone()));
    }
    Ok(AreaIndex { areas })
}

enum GKey {
    Time(TimeUnit),
    Cell(f64),
    Tile(u32),
    Chunk(u64),
    Expr(&'static Expr),
    Areas(Arc<AreaIndex>),
    /// `on each`: bucket of the element's evaluation time.
    Series(TimeUnit),
}

fn group_keys(ctx: &Ctx, gk: &[GKey], v: &Value, sc: &Sc, idx: usize) -> R<Vec<Vec<Value>>> {
    let mut out: Vec<Vec<Value>> = vec![Vec::new()];
    for g in gk {
        let ks: Vec<Value> = match g {
            GKey::Series(u) => match v {
                Value::Elem(e) => vec![Value::Time(dates::bucket(e.at, *u))],
                _ => vec![Value::None],
            },
            GKey::Time(u) => match fields::time_of(ctx, v) {
                Some(t) => vec![Value::Time(dates::bucket(t, *u))],
                None => vec![Value::None],
            },
            GKey::Cell(size) => match ctx.rep_point(v) {
                Some(p) => vec![Value::text(&crate::stats::cell_name(p, *size))],
                None => vec![Value::None],
            },
            GKey::Tile(z) => match ctx.rep_point(v) {
                Some(p) => vec![Value::text(&crate::stats::tile_name(p, *z))],
                None => vec![Value::None],
            },
            GKey::Chunk(n) => vec![Value::num((idx as u64 / n + 1) as f64)],
            GKey::Areas(ix) => {
                let p = ctx.rep_point(v);
                let mut ks = Vec::new();
                if let Some(p) = p {
                    for (bb, r, key) in &ix.areas {
                        if bb.contains(p) && r.contains(p) {
                            ks.push(key.clone());
                        }
                    }
                }
                ks
            }
            GKey::Expr(x) => {
                let k = ctx.eval(x, sc)?;
                vec![k]
            }
        };
        let mut next = Vec::new();
        for prefix in &out {
            for k in &ks {
                let mut p = prefix.clone();
                p.push(k.clone());
                next.push(p);
            }
        }
        out = next;
    }
    Ok(out)
}

fn key_of(ks: &[Value]) -> Key {
    if ks.len() == 1 { ks[0].key() } else { Key::Tuple(ks.iter().map(|k| k.key()).collect()) }
}

fn key_value(ks: Vec<Value>) -> Value {
    if ks.len() == 1 { ks.into_iter().next().unwrap() } else { Value::List(Arc::new(ks)) }
}

// ----- the call ---------------------------------------------------------------------------

pub fn of_call(ctx: &Ctx, e: &'static Expr, func: OfFunc, fsp: Span, operand: &'static OfOperand, cl: &'static Clauses, sc: &Sc) -> R<Value> {
    if !func.is_aggregate() {
        return crate::special::non_aggregate(ctx, e, func, fsp, operand, cl, sc);
    }
    let clause_vars: Vec<String> = ctx.chk.clause_vars.get(&e.span).cloned().unwrap_or_default();
    let (src, vars, mut preds, set_txt): (Value, Vec<String>, Vec<&'static Expr>, String) = match operand {
        OfOperand::Iter { var, set, pred, cond, .. } => {
            let v = ctx.eval(set, sc)?;
            (v, vec![var.clone()], [pred.as_deref(), cond.as_deref()].into_iter().flatten().collect(), ctx.text(set.span).to_string())
        }
        OfOperand::Expr(x) => {
            let v = ctx.eval(x, sc)?;
            if let Value::Elem(_) = v
                && !matches!(func, OfFunc::Days | OfFunc::Weeks | OfFunc::Months | OfFunc::Years | OfFunc::Earliest | OfFunc::Latest)
            {
                return Err(RtErr::fatal(Diagnostic::error(
                    "E601",
                    x.span,
                    Text2::new(
                        format!("\"{} of\" expects a set, but got a single element.", func.name()),
                        format!("\"{} of\" erwartet eine Menge, hat aber ein einzelnes Element bekommen.", func.name()),
                    ),
                )));
            }
            (v, clause_vars.clone(), Vec::new(), ctx.text(x.span).to_string())
        }
    };
    if src.is_none() {
        return Ok(Value::None);
    }
    // `share of S with C` / `share of x in S with C`: C is the condition being measured;
    // with `per`, `with` filters as everywhere else
    let mut measured: Option<&'static Expr> = None;
    if func == OfFunc::Share && cl.per.is_empty() {
        if let Some(w) = &cl.with {
            measured = Some(w);
        } else if let OfOperand::Iter { cond: Some(c), .. } = operand {
            let c: &'static Expr = c;
            preds.retain(|p| !std::ptr::eq(*p, c));
            measured = Some(c);
        }
    }
    // aggregate applied to each group of a grouped set (`on each` results, groups of sets)
    if let Value::Grouped(g) = &src
        && g.rows.iter().all(|r| matches!(r.value, Value::Set(_) | Value::List(_)))
        && !g.rows.is_empty()
    {
        let mut rows = Vec::new();
        for r in &g.rows {
            let v = of_values(ctx, e, func, cl, &r.value, &vars, preds.clone(), measured, sc, &set_txt)?;
            rows.push(Group { key: r.key.clone(), value: v, count: r.count, items: None });
        }
        return Ok(Value::Grouped(Arc::new(Grouped { key_names: g.key_names.clone(), value_name: func.name().into(), rows })));
    }
    let src = match &src {
        Value::Elem(_) => Value::List(Arc::new(vec![src.clone()])),
        _ => src,
    };
    if measured.is_none()
        && let Some(w) = &cl.with
    {
        preds.push(w);
    }
    of_values(ctx, e, func, cl, &src, &vars, preds, measured, sc, &set_txt)
}

#[allow(clippy::too_many_arguments)]
fn of_values(
    ctx: &Ctx,
    e: &'static Expr,
    func: OfFunc,
    cl: &'static Clauses,
    src: &Value,
    vars: &[String],
    preds: Vec<&'static Expr>,
    measured: Option<&'static Expr>,
    sc: &Sc,
    set_txt: &str,
) -> R<Value> {
    let span = e.span;
    let base = ctx.as_set(src, span)?;
    let share_like = matches!(func, OfFunc::Share);
    let within = match &cl.within {
        Some(w) => {
            let v = ctx.eval(w, sc)?;
            Some(ctx.region_of(&v, w.span)?)
        }
        None => None,
    };
    let (filter_region, share_region) = if share_like && cl.per.is_empty() { (None, within.clone()) } else { (within.clone(), None) };
    let set = restrict(ctx, base, vars.to_vec(), preds, sc.clone(), filter_region, cl.time.as_ref(), span)?;
    // grouping
    let mut gk: Vec<GKey> = Vec::new();
    let mut key_names: Vec<String> = Vec::new();
    if let Some(q) = set.is_query()
        && let TimeMode::Series(_) = q.spec.time
        && let Some(TimeRef::OnEach { unit, .. }) = series_time(cl, e)
    {
        gk.push(GKey::Series(*unit));
        key_names.push(unit.name().into());
    } else if let Some(q) = set.is_query()
        && let TimeMode::Series(starts) = &q.spec.time
    {
        // series defined in the selection: find its unit from the step
        let u = if starts.len() > 1 {
            let d = starts[1] - starts[0];
            if d <= 86_400 {
                TimeUnit::Day
            } else if d <= 7 * 86_400 {
                TimeUnit::Week
            } else if d <= 31 * 86_400 {
                TimeUnit::Month
            } else {
                TimeUnit::Year
            }
        } else {
            TimeUnit::Year
        };
        gk.push(GKey::Series(u));
        key_names.push(u.name().into());
    }
    let mut chunk = None;
    let mut hints = Hints::default();
    for (g, gs) in &cl.per {
        key_names.push(ctx.text(*gs).trim_start_matches("per ").to_string());
        match g {
            Grouping::Time(u) => gk.push(GKey::Time(*u)),
            Grouping::Cell(x) => {
                let v = ctx.eval(x, sc)?;
                let m = ctx.to_num(&v, Some(Dim::LENGTH), x.span)?.map_or(1000.0, |n| n.v);
                hints.geom = true;
                gk.push(GKey::Cell(m));
            }
            Grouping::Tile(z) => {
                hints.geom = true;
                gk.push(GKey::Tile(*z));
            }
            Grouping::Chunk(n) => {
                chunk = Some(*n);
                gk.push(GKey::Chunk(*n));
            }
            Grouping::Expr(x) => {
                // a set of areas or a region groups spatially
                // an expression that does not use the element (`per districts`,
                // `per (areas r with r:boundary)`) is evaluated once
                let mut per_element = false;
                x.walk(&mut |y| match &y.kind {
                    ExprKind::Underscore => per_element = true,
                    ExprKind::Ident(n) => {
                        // only the outer iteration variables count; names bound inside
                        // (the `r` of `areas r with …`) do not
                        per_element |= vars.iter().any(|v| v == n) || matches!(ctx.chk.idents.get(&y.span), Some(knut_plan::IdentRes::ImplicitField(_)));
                    }
                    _ => {}
                });
                let once = if per_element { None } else { Some(ctx.eval(x, sc)?) };
                if let Some(v) = once.filter(|v| matches!(v, Value::Set(_) | Value::Region(_) | Value::List(_))) {
                    hints.geom = true;
                    gk.push(GKey::Areas(Arc::new(area_index(ctx, &v, x.span)?)));
                } else {
                    for v in vars {
                        hints.geom |= pushdown::needs_geometry(x, v);
                    }
                    gk.push(GKey::Expr(x));
                }
            }
        }
    }
    if matches!(func, OfFunc::Spread | OfFunc::Density) || within.is_some() {
        hints.geom = true;
    }
    let share_pred = measured;
    let ordered = chunk.is_some() || matches!(func, OfFunc::Transitions);
    hints.count_only = matches!(func, OfFunc::Count) && gk.is_empty() && share_pred.is_none() && share_region.is_none() && !hints.geom;
    let item_value = |v: &Value| -> Value {
        match (func, v) {
            (OfFunc::Spread | OfFunc::Density | OfFunc::Count | OfFunc::Share | OfFunc::Earliest | OfFunc::Latest, _) => v.clone(),
            (_, Value::Group(g)) if matches!(func, OfFunc::Sum | OfFunc::Average | OfFunc::Variance | OfFunc::Gini) => g.value.clone(),
            _ => v.clone(),
        }
    };
    let rows: Vec<(Vec<Value>, Acc, u64)> = if ordered {
        // sequential: order the items first
        let items = set.items(ctx, hints)?;
        let mut keyed: Vec<(Key, Value)> = Vec::with_capacity(items.len());
        for it in items.iter() {
            let k = match &cl.order {
                Some(o) => {
                    let isc = ctx.bind_elem(sc, vars, it);
                    ctx.eval(o, &isc)?.key()
                }
                // by time; values without a time keep their order (lists)
                None => match fields::time_of(ctx, it) {
                    Some(t) => Key::Num(OrdF(t as f64), Dim::NONE, None),
                    None => Key::Num(OrdF(keyed.len() as f64), Dim::NONE, None),
                },
            };
            keyed.push((k, it.clone()));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut groups: Vec<(Vec<Value>, Acc, u64)> = Vec::new();
        let mut index: HashMap<Key, usize> = HashMap::default();
        for (i, (_, it)) in keyed.iter().enumerate() {
            let isc = ctx.bind_elem(sc, vars, it);
            for ks in group_keys(ctx, &gk, it, &isc, i)? {
                let k = key_of(&ks);
                let gi = *index.entry(k).or_insert_with(|| {
                    groups.push((ks.clone(), init_acc(func), 0));
                    groups.len() - 1
                });
                let hit = share_hit(ctx, share_pred, share_region.as_deref(), it, &isc)?;
                add(ctx, func, &mut groups[gi].1, &item_value(it), hit, span, set_txt)?;
                groups[gi].2 += 1;
            }
        }
        groups
    } else {
        let parts = set.fold(
            ctx,
            hints,
            &HashMap::<Key, (Vec<Value>, Acc, u64)>::default,
            &|acc: &mut HashMap<Key, (Vec<Value>, Acc, u64)>, it: Value| {
                let isc = ctx.bind_elem(sc, vars, &it);
                let hit = share_hit(ctx, share_pred, share_region.as_deref(), &it, &isc)?;
                for ks in group_keys(ctx, &gk, &it, &isc, 0)? {
                    let k = key_of(&ks);
                    let ent = acc.entry(k).or_insert_with(|| (ks, init_acc(func), 0));
                    add(ctx, func, &mut ent.1, &item_value(&it), hit, span, set_txt)?;
                    ent.2 += 1;
                }
                Ok(())
            },
        )?;
        let mut merged: HashMap<Key, (Vec<Value>, Acc, u64)> = HashMap::default();
        let mut order: Vec<Key> = Vec::new();
        for p in parts {
            let mut entries: Vec<(Key, (Vec<Value>, Acc, u64))> = p.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, (ks, a, n)) in entries {
                match merged.get_mut(&k) {
                    Some(m) => {
                        m.1.merge(a);
                        m.2 += n;
                    }
                    None => {
                        order.push(k.clone());
                        merged.insert(k, (ks, a, n));
                    }
                }
            }
        }
        order.into_iter().map(|k| merged.remove(&k).unwrap()).collect()
    };
    if gk.is_empty() {
        let acc = rows.into_iter().next().map(|r| r.1).unwrap_or_else(|| init_acc(func));
        let mut v = finish(ctx, func, acc, span, set_txt)?;
        if func == OfFunc::Density {
            // the region of the clause, else the one of the selection (`nodes n with … within R`),
            // else the extent of the database
            let area = within
                .clone()
                .or_else(|| set.is_query().and_then(|q| q.spec.region.clone()))
                .map(|r| r.area_m2())
                .or_else(|| ctx.db_arc().map(|d| d.extent.area_km2() * 1e6))
                .unwrap_or(0.0);
            if let Value::Num(n) = v
                && area > 0.0
            {
                v = Value::num(n.v / (area / 1e6));
            }
        }
        return Ok(v);
    }
    // grouped result
    let total: u64 = rows.iter().map(|r| r.2).sum();
    let mut out: Vec<Group> = Vec::with_capacity(rows.len());
    for (ks, acc, n) in rows {
        let mut v = if func == OfFunc::Share {
            Value::num(if total == 0 { 0.0 } else { n as f64 / total as f64 })
        } else {
            finish(ctx, func, acc, span, set_txt)?
        };
        if func == OfFunc::Density
            && let Some(Value::Elem(_)) | Some(Value::Region(_)) = ks.last()
        {
            let r = ctx.region_of(ks.last().unwrap(), span)?;
            let a = r.area_m2() / 1e6;
            if let Value::Num(x) = v {
                v = if a > 0.0 { Value::num(x.v / a) } else { Value::None };
            }
        }
        out.push(Group { key: key_value(ks), value: v, count: n, items: None });
    }
    if let Some(h) = &cl.having {
        let mut kept = Vec::new();
        for g in out {
            let gv = Value::Group(Arc::new(g.clone()));
            let hsc = ctx.bind(sc, "\u{1}", gv, true);
            if ctx.truthy(h, &hsc)? {
                kept.push(g);
            }
        }
        out = kept;
    }
    if func == OfFunc::Distribution {
        // distribution keeps one level of groups; nested results stay as values
    }
    out.sort_by_key(|a| a.key.key());
    let value_name = if func == OfFunc::Share { "share".to_string() } else { func.name().to_string() };
    Ok(Value::Grouped(Arc::new(Grouped { key_names, value_name, rows: out })))
}

fn series_time(cl: &'static Clauses, _e: &Expr) -> Option<&'static TimeRef> {
    match &cl.time {
        Some(t @ TimeRef::OnEach { .. }) => Some(t),
        _ => None,
    }
}

fn share_hit(ctx: &Ctx, pred: Option<&'static Expr>, region: Option<&Region>, v: &Value, sc: &Sc) -> R<bool> {
    let mut hit = true;
    if let Some(p) = pred {
        hit &= ctx.truthy(p, sc)?;
    }
    if let Some(r) = region {
        hit &= ctx.rep_point(v).is_some_and(|p| r.contains(p));
    }
    Ok(hit)
}

/// `the` style suggestions for E601 at run time.
pub fn e601(func: OfFunc, span: Span) -> RtErr {
    RtErr::fatal(
        Diagnostic::error(
            "E601",
            span,
            Text2::new(
                format!("\"{} of\" expects a set, but got a single element.", func.name()),
                format!("\"{} of\" erwartet eine Menge, hat aber ein einzelnes Element bekommen.", func.name()),
            ),
        )
        .header(Header::DidYouMean),
    )
}

pub fn unit_km2() -> &'static units::Unit {
    units::unit("km2").unwrap()
}
