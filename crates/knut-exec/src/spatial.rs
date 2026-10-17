//! Spatial joins (`near`, `nearest`) by straight line and on networks (KNUT_LANG.md §5.7,
//! §7.4 item 7). The smaller side goes into a grid index or a multi-source search; the
//! other side is probed per element.

use crate::eval::{Ctx, Sc, lock};
use crate::geom::{self, BBoxF, Geom, P};
use crate::net::{Metric, Net};
use crate::region::Region;
use crate::scan::{KindMask, ScanSpec, TimeMode};
use crate::set::{Hints, Query, SetSrc, SetV};
use crate::value::*;
use knut_format::ObjType;
use knut_lang::ast::{Expr, ExprKind, Kind, Mode, Quant, RelTail, Threshold};
use knut_lang::diag::{Diagnostic, Header, Span, Text2};
use knut_lang::units::Dim;
use knut_plan::pushdown::TagPred;
use rustc_hash::FxHashMap as HashMap;
use std::sync::Arc;

pub struct GeoIndex {
    pub items: Vec<(Value, Arc<Geom>, BBoxF)>,
    cell: f64,
    grid: HashMap<(i32, i32), Vec<u32>>,
}

impl GeoIndex {
    fn build(items: Vec<(Value, Arc<Geom>)>) -> GeoIndex {
        let items: Vec<(Value, Arc<Geom>, BBoxF)> = items.into_iter().filter(|x| !x.1.is_empty()).map(|(v, g)| {
            let b = g.bbox();
            (v, g, b)
        }).collect();
        // cell size from where most items lie: a few far-away items (history extracts keep
        // objects that once lay elsewhere) must not make the cells huge
        let n = items.len().max(1) as f64;
        let w = {
            let (mut xs, mut ys): (Vec<f64>, Vec<f64>) = items.iter().map(|i| i.2.center()).unzip();
            xs.sort_by(f64::total_cmp);
            ys.sort_by(f64::total_cmp);
            let q = |v: &[f64], p: f64| if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f64 * p).round() as usize] };
            (q(&xs, 0.95) - q(&xs, 0.05)).max(q(&ys, 0.95) - q(&ys, 0.05)).max(1e-4)
        };
        let cell = (w / n.sqrt()).clamp(0.0005, 1.0);
        let mut grid: HashMap<(i32, i32), Vec<u32>> = HashMap::default();
        for (i, (_, _, b)) in items.iter().enumerate() {
            let (x0, y0) = ((b.minx / cell).floor() as i32, (b.miny / cell).floor() as i32);
            let (x1, y1) = ((b.maxx / cell).floor() as i32, (b.maxy / cell).floor() as i32);
            for x in x0..=x1.min(x0 + 200) {
                for y in y0..=y1.min(y0 + 200) {
                    grid.entry((x, y)).or_default().push(i as u32);
                }
            }
        }
        GeoIndex { items, cell, grid }
    }

    fn candidates(&self, b: &BBoxF) -> Vec<u32> {
        let (x0, y0) = ((b.minx / self.cell).floor() as i32, (b.miny / self.cell).floor() as i32);
        let (x1, y1) = ((b.maxx / self.cell).floor() as i32, (b.maxy / self.cell).floor() as i32);
        let mut out = Vec::new();
        if (x1 - x0) as i64 * (y1 - y0) as i64 > 1_000_000 {
            return (0..self.items.len() as u32).collect();
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                if let Some(v) = self.grid.get(&(x, y)) {
                    out.extend_from_slice(v);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Nearest item by straight-line distance (ties: smaller key).
    pub fn nearest(&self, g: &Geom, skip: Option<&Key>) -> Option<(usize, f64)> {
        let Some(p) = g.rep_point() else { return None };
        let mut best: Option<(usize, f64)> = None;
        let mut r = 0usize;
        let (cx, cy) = ((p.0 / self.cell).floor() as i32, (p.1 / self.cell).floor() as i32);
        let max_r = 400usize;
        // distances are measured to `g`, not to `p`: an item outside the rings may still lie up
        // to the reach of `g` around `p` closer
        let b = g.bbox();
        let reach = [(b.minx, b.miny), (b.minx, b.maxy), (b.maxx, b.miny), (b.maxx, b.maxy)]
            .into_iter()
            .map(|q| geom::haversine(p, q))
            .fold(0.0, f64::max);
        while r <= max_r {
            let ri = r as i32;
            let mut seen_any = false;
            for dx in -ri..=ri {
                for dy in -ri..=ri {
                    if dx.abs() != ri && dy.abs() != ri {
                        continue;
                    }
                    let Some(v) = self.grid.get(&(cx + dx, cy + dy)) else { continue };
                    seen_any = true;
                    for &i in v {
                        let (val, ig, _) = &self.items[i as usize];
                        if skip.is_some_and(|k| *k == val.key()) {
                            continue;
                        }
                        let d = geom::distance_m(g, ig);
                        let better = match best {
                            None => true,
                            Some((bi, bd)) => d < bd || (d == bd && val.key() < self.items[bi].0.key()),
                        };
                        if better {
                            best = Some((i as usize, d));
                        }
                    }
                }
            }
            let _ = seen_any;
            if let Some((_, bd)) = best {
                // all closer items lie within the searched rings
                let ring_m = (r as f64) * self.cell * 111_000.0 * p.1.to_radians().cos().max(0.1);
                if bd + reach <= ring_m {
                    break;
                }
            }
            r += 1;
        }
        if best.is_none() && !self.items.is_empty() {
            // far away: brute force
            for (i, (val, ig, _)) in self.items.iter().enumerate() {
                if skip.is_some_and(|k| *k == val.key()) {
                    continue;
                }
                let d = geom::distance_m(g, ig);
                if best.is_none_or(|b| d < b.1) {
                    best = Some((i, d));
                }
            }
        }
        best
    }
}

pub fn geo_index(ctx: &Ctx, set: &Arc<SetV>, span: Span) -> R<Arc<GeoIndex>> {
    let slot = lock(&ctx.caches.geo).entry(Arc::as_ptr(set) as usize).or_insert_with(|| (set.clone(), Default::default())).1.clone();
    crate::cache::get_or_build(&slot, || {
        let items = set.items(ctx, Hints { geom: true, ..Default::default() })?;
        ctx.preload_geoms(&items);
        let mut v = Vec::with_capacity(items.len());
        for it in items.iter() {
            if let Some(g) = ctx.geom_of(it, span)? {
                v.push((it.clone(), g));
            }
        }
        Ok(Arc::new(GeoIndex::build(v)))
    })
}

fn speed(m: Mode) -> f64 {
    match m {
        Mode::Foot => 5.0 / 3.6,
        Mode::Bicycle => 15.0 / 3.6,
        _ => 100.0 / 3.6,
    }
}

/// The network for a mode: `using NET` or the standard network around `bbox`.
pub fn network(ctx: &Ctx, mode: Mode, using: Option<&'static Expr>, bbox: BBoxF, sc: &Sc, span: Span) -> R<Arc<Net>> {
    if mode == Mode::Transit {
        return Err(RtErr::fatal(
            Diagnostic::error(
                "E502",
                span,
                Text2::new("\"by transit\" needs a network, and none is available.", "\"by transit\" braucht ein Netz, und es gibt keines."),
            )
            .why(
                "Public transport timetables (GTFS) are not part of the history file.",
                "Fahrpläne des öffentlichen Verkehrs (GTFS) sind nicht Teil der Historie.",
            )
            .header(Header::YouCan)
            .sugg_label("use another way of moving:", "eine andere Fortbewegungsart nehmen:", "by foot"),
        ));
    }
    if let Some(u) = using {
        let v = ctx.eval(u, sc)?;
        let Value::Net(n) = v else {
            return Err(RtErr::fatal(Diagnostic::error(
                "E502",
                u.span,
                Text2::new(
                    format!("\"{}\" is not a network.", ctx.text(u.span)),
                    format!("\"{}\" ist kein Netz.", ctx.text(u.span)),
                ),
            )
            .why("Build one with \"graph of\", for example:", "Ein Netz entsteht mit \"graph of\", zum Beispiel:")
            .sugg("net = graph of ways w with w:highway")));
        };
        let name = ctx.text(u.span).to_string();
        if n.n_edges() == 0 {
            return Err(RtErr::fatal(
                Diagnostic::error(
                    "E502",
                    u.span,
                    Text2::new(format!("\"by {}\" needs a network, and none is available.", mode.name()), format!("\"by {}\" braucht ein Netz, und es gibt keines.", mode.name())),
                )
                .why(format!("\"{name}\" contains no edges that can be used."), format!("\"{name}\" enthält keine nutzbaren Kanten."))
                .header(Header::YouCan)
                .sugg_label("leave out \"using\" to use the standard network", "\"using\" weglassen, um das Standardnetz zu nutzen", "")
                .sugg_label("define the network:", "das Netz definieren:", format!("{name} = graph of ways w with w:highway")),
            ));
        }
        if n.usable(mode) == 0 {
            return Err(RtErr::fatal(
                Diagnostic::error(
                    "E503",
                    u.span,
                    Text2::new("This network does not match the way of moving.", "Dieses Netz passt nicht zur Fortbewegungsart."),
                )
                .why(
                    format!("\"{name}\" has no ways that can be used {}.", match mode {
                        Mode::Foot => "on foot",
                        Mode::Bicycle => "by bicycle",
                        _ => "by car",
                    }),
                    format!("\"{name}\" hat keine Wege, die sich {} nutzen lassen.", match mode {
                        Mode::Foot => "zu Fuß",
                        Mode::Bicycle => "mit dem Rad",
                        _ => "mit dem Auto",
                    }),
                )
                .header(Header::YouCan)
                .sugg_label("use a matching mode:", "eine passende Fortbewegungsart nehmen:", "by car")
                .sugg_label("build a walking network:", "ein Fußwegenetz bauen:", "walkNet = graph of ways w with w:highway in [footway, path, steps]"),
            ));
        }
        return Ok(n);
    }
    standard_net(ctx, bbox, span)
}

/// Standard network: all highway ways within `bbox` (latest state).
pub fn standard_net(ctx: &Ctx, bbox: BBoxF, span: Span) -> R<Arc<Net>> {
    let key = format!("{:.3},{:.3},{:.3},{:.3}", bbox.minx, bbox.miny, bbox.maxx, bbox.maxy);
    // reuse a finished network that covers the box
    for s in lock(&ctx.caches.nets).values() {
        if let Some(Ok(n)) = s.get()
            && n.bbox.contains_box(&bbox)
        {
            return Ok(n.clone());
        }
    }
    crate::cache::get_or_build(&crate::cache::slot(&ctx.caches.nets, key), || build_standard_net(ctx, bbox, span))
}

fn build_standard_net(ctx: &Ctx, bbox: BBoxF, span: Span) -> R<Arc<Net>> {
    let spec = ScanSpec {
        bbox: None,
        kinds: KindMask { way: true, ..Default::default() },
        time: TimeMode::State(ctx.latest()),
        region: Some(Arc::new(Region::Box(bbox))),
        pred: TagPred::Has("highway".into()),
        need_geom: false,
        need_hist: false,
        attrs_history: false,
    };
    let q = Query { spec, vars: vec!["_".into()], preds: vec![], scope: None, seq: None };
    let set = SetV::lazy(SetSrc::Query(Arc::new(q)));
    Ok(Arc::new(net_from(ctx, &set, span)?))
}

/// Builds a network from a set of ways.
pub fn net_from(ctx: &Ctx, set: &SetV, span: Span) -> R<Net> {
    let _ = span;
    let items = set.items(ctx, Hints::default())?;
    let ways: Vec<Arc<Elem>> = items
        .iter()
        .filter_map(|v| match v {
            Value::Elem(e) if e.otype == Some(ObjType::Way) => Some(e.clone()),
            _ => None,
        })
        .collect();
    let db = ctx.db()?;
    let reqs: Vec<(i64, i64)> = ways.iter().map(|e| (e.id, e.at)).collect();
    let geoms = db.way_geoms(&reqs);
    let mut input = Vec::with_capacity(ways.len());
    for e in ways {
        let Some(Some((nids, pts))) = geoms.get(&(e.id, e.at)) else { continue };
        let mut ni = Vec::new();
        let mut ps = Vec::new();
        for (n, p) in nids.iter().zip(pts) {
            if let Some(p) = p {
                ni.push(*n);
                ps.push(*p);
            }
        }
        input.push((e, ni, ps));
    }
    Ok(Net::build(&input))
}

fn threshold_value(ctx: &Ctx, th: Option<&'static Threshold>, sc: &Sc) -> R<(f64, Dim, bool, Option<Mode>, Option<&'static Expr>, Span)> {
    match th {
        None => Ok((ctx.chk.settings.near_distance, Dim::LENGTH, false, None, None, Span::default())),
        Some(t) => {
            let v = ctx.eval(&t.value, sc)?;
            let n = ctx.to_num(&v, None, t.value.span)?.unwrap_or(Num::plain(0.0));
            let mode = t.mode.as_ref().map(|m| m.mode);
            let using = t.mode.as_ref().and_then(|m| m.using.as_deref());
            Ok((n.v, n.dim, t.or_equal, mode, using, t.value.span))
        }
    }
}

/// Multi-source network search from the members of a set (cached).
pub struct NetNearest {
    pub net: Arc<Net>,
    pub dist: Vec<f64>,
    pub label: Vec<u32>,
    pub items: Arc<Vec<Value>>,
    /// Snap distance per item.
    pub snap: Vec<f64>,
}

pub fn net_nearest(ctx: &Ctx, set: &Arc<SetV>, mode: Mode, metric: Metric, using: Option<&'static Expr>, radius_m: f64, sc: &Sc, span: Span) -> R<Arc<NetNearest>> {
    let key = (Arc::as_ptr(set) as usize, mode as u8, matches!(metric, Metric::Time) as u8);
    let slot = lock(&ctx.caches.net_nearest).entry(key).or_insert_with(|| (set.clone(), Default::default())).1.clone();
    crate::cache::get_or_build(&slot, || build_net_nearest(ctx, set, mode, metric, using, radius_m, sc, span))
}

#[allow(clippy::too_many_arguments)]
fn build_net_nearest(ctx: &Ctx, set: &Arc<SetV>, mode: Mode, metric: Metric, using: Option<&'static Expr>, radius_m: f64, sc: &Sc, span: Span) -> R<Arc<NetNearest>> {
    let items = set.items(ctx, Hints { geom: true, ..Default::default() })?;
    let mut bb = BBoxF::EMPTY;
    for it in items.iter() {
        if let Some(p) = ctx.rep_point(it) {
            bb.add(p);
        }
    }
    if bb.is_empty() {
        bb = ctx.db().map(|d| d.extent).unwrap_or(BBoxF::WORLD);
    }
    let net = network(ctx, mode, using, bb.expand_m(radius_m), sc, span)?;
    let mut sources = Vec::new();
    let mut snap = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let p = ctx.rep_point(it);
        match p.and_then(|p| net.snap(p, mode)) {
            Some((n, d)) => {
                let c = match metric {
                    Metric::Time => d / speed(mode),
                    Metric::Length => d,
                };
                sources.push((n, c, i as u32));
                snap.push(d);
            }
            None => snap.push(f64::INFINITY),
        }
    }
    let (dist, label, _) = net.search(&sources, mode, metric, f64::INFINITY);
    Ok(Arc::new(NetNearest { net, dist, label, items: items.clone(), snap }))
}

pub fn near(ctx: &Ctx, l: &'static Expr, quant: Quant, set_e: &'static Expr, th: Option<&'static Threshold>, sc: &Sc, span: Span) -> R<Value> {
    let x = ctx.eval(l, sc)?;
    if x.is_none() {
        return Ok(Value::Bool(false));
    }
    let sv = ctx.eval(set_e, sc)?;
    let set = ctx.as_set(&sv, set_e.span)?;
    let (d, dim, or_eq, mode, using, tspan) = threshold_value(ctx, th, sc)?;
    let within = |dist: f64| dist < d || (or_eq && dist <= d);
    let self_key = x.key();
    match mode {
        None => {
            if dim != Dim::LENGTH {
                return Err(RtErr::fatal(
                    Diagnostic::error("E303", tspan, Text2::new("This unit does not fit here.", "Diese Einheit passt hier nicht."))
                        .why("Without \"by\", \"near\" measures straight-line distance.", "Ohne \"by\" misst \"near\" die Luftlinie.")
                        .header(Header::DidYouMean)
                        .sugg(format!("{} by foot", ctx.text(th.map_or(span, |t| t.span)))),
                ));
            }
            let ix = geo_index(ctx, &set, set_e.span)?;
            let Some(gx) = ctx.geom_of(&x, l.span)? else { return Ok(Value::Bool(false)) };
            let bb = gx.bbox().expand_m(d);
            let cands = ix.candidates(&bb);
            Ok(Value::Bool(match quant {
                Quant::Some | Quant::No => {
                    let hit = cands.iter().any(|&i| {
                        let (v, g, _) = &ix.items[i as usize];
                        v.key() != self_key && within(geom::distance_m(&gx, g))
                    });
                    (quant == Quant::Some) == hit
                }
                Quant::All => {
                    let n_other = ix.items.iter().filter(|(v, _, _)| v.key() != self_key).count();
                    let n_near = cands
                        .iter()
                        .filter(|&&i| {
                            let (v, g, _) = &ix.items[i as usize];
                            v.key() != self_key && within(geom::distance_m(&gx, g))
                        })
                        .count();
                    n_near == n_other
                }
            }))
        }
        Some(m) => {
            let metric = if dim == Dim::TIME { Metric::Time } else { Metric::Length };
            let radius = match metric {
                Metric::Time => d * speed(m),
                Metric::Length => d,
            };
            let Some(px) = ctx.rep_point(&x) else { return Ok(Value::Bool(false)) };
            match quant {
                Quant::Some | Quant::No => {
                    let nn = net_nearest(ctx, &set, m, metric, using, radius, sc, span)?;
                    let hit = match nn.net.snap(px, m) {
                        Some((n, sd)) => {
                            let extra = match metric {
                                Metric::Time => sd / speed(m),
                                Metric::Length => sd,
                            };
                            let c = nn.dist[n as usize] + extra;
                            // the nearest member may be the element itself
                            let lab = nn.label[n as usize];
                            if lab != u32::MAX && nn.items[lab as usize].key() == self_key {
                                // search again without the element
                                let others: Vec<Value> = nn.items.iter().filter(|v| v.key() != self_key).cloned().collect();
                                let tmp = Arc::new(SetV::from_vec(others));
                                let nn2 = net_nearest_uncached(ctx, &tmp, m, metric, &nn.net)?;
                                within(nn2.0[n as usize] + extra)
                            } else {
                                within(c)
                            }
                        }
                        None => false,
                    };
                    Ok(Value::Bool((quant == Quant::Some) == hit))
                }
                Quant::All => {
                    let items = set.items(ctx, Hints { geom: true, ..Default::default() })?;
                    let mut bb = BBoxF::EMPTY;
                    bb.add(px);
                    let net = network(ctx, m, using, bb.expand_m(radius), sc, span)?;
                    let Some((s, sd)) = net.snap(px, m) else { return Ok(Value::Bool(false)) };
                    let start = match metric {
                        Metric::Time => sd / speed(m),
                        Metric::Length => sd,
                    };
                    let (dist, _, _) = net.search(&[(s, start, 0)], m, metric, d);
                    for it in items.iter() {
                        if it.key() == self_key {
                            continue;
                        }
                        let Some(p) = ctx.rep_point(it) else { return Ok(Value::Bool(false)) };
                        let Some((n, _)) = net.snap(p, m) else { return Ok(Value::Bool(false)) };
                        if !within(dist[n as usize]) {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Ok(Value::Bool(true))
                }
            }
        }
    }
}

fn net_nearest_uncached(ctx: &Ctx, set: &Arc<SetV>, m: Mode, metric: Metric, net: &Net) -> R<(Vec<f64>, Vec<u32>)> {
    let items = set.items(ctx, Hints { geom: true, ..Default::default() })?;
    let mut sources = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if let Some(p) = ctx.rep_point(it)
            && let Some((n, d)) = net.snap(p, m)
        {
            let c = match metric {
                Metric::Time => d / speed(m),
                Metric::Length => d,
            };
            sources.push((n, c, i as u32));
        }
    }
    let (d, l, _) = net.search(&sources, m, metric, f64::INFINITY);
    Ok((d, l))
}

/// `nearest of S [to x] [by MODE]`.
pub fn nearest(ctx: &Ctx, set: &Arc<SetV>, origin: &Value, mode: Option<(Mode, Option<&'static Expr>)>, sc: &Sc, span: Span) -> R<Value> {
    match mode {
        None => {
            let ix = geo_index(ctx, set, span)?;
            let Some(g) = ctx.geom_of(origin, span)? else { return Ok(Value::None) };
            let k = origin.key();
            Ok(ix.nearest(&g, Some(&k)).map_or(Value::None, |(i, _)| ix.items[i].0.clone()))
        }
        Some((m, using)) => {
            let nn = net_nearest(ctx, set, m, Metric::Time, using, 20_000.0, sc, span)?;
            let Some(p) = ctx.rep_point(origin) else { return Ok(Value::None) };
            let Some((n, _)) = nn.net.snap(p, m) else { return Ok(Value::None) };
            let lab = nn.label[n as usize];
            Ok(if lab == u32::MAX { Value::None } else { nn.items[lab as usize].clone() })
        }
    }
}

/// The highest speed of a mode on the network (m/s): a travel time `t` covers at most
/// `t · vmax` in a straight line.
fn vmax(m: Mode) -> f64 {
    match m {
        Mode::Foot => 7.0 / 3.6,
        Mode::Bicycle => 35.0 / 3.6,
        _ => 250.0 / 3.6,
    }
}

/// `nearest of SELECTION to x` for one fixed place `x`: the selection is scanned only in boxes
/// around `x`, growing until the nearest match lies closer than anything outside the box can
/// be (along the network: a way is at least as long as the straight line). `None` when the
/// box would reach the whole data: then as without the boxes.
pub fn nearest_around(ctx: &Ctx, sel: &'static Expr, origin: &Value, mode: Option<(Mode, Option<&'static Expr>)>, sc: &Sc, span: Span) -> R<Option<Value>> {
    let Some(g) = ctx.geom_of(origin, span)? else { return Ok(Some(Value::None)) };
    let ob = g.bbox();
    if ob.is_empty() {
        return Ok(None);
    }
    let extent = ctx.db()?.extent;
    let key = origin.key();
    // along the stored routing network (as isochrones and paths): one search from `a`
    if let Some((m, None)) = mode
        && let Some(v) = nearest_stored(ctx, sel, origin, &g, m, sc)?
    {
        return Ok(Some(v));
    }
    let mut r = 1000.0;
    loop {
        let bx = ob.expand_m(r);
        if bx.contains_box(&extent) {
            return Ok(None);
        }
        let Some(v) = ctx.select_in_box(sel, sc, bx)? else { return Ok(None) };
        let set = ctx.as_set(&v, sel.span)?;
        match mode {
            None => {
                let ix = geo_index(ctx, &set, span)?;
                if let Some((i, d)) = ix.nearest(&g, Some(&key))
                    && d <= r
                {
                    return Ok(Some(ix.items[i].0.clone()));
                }
            }
            Some((m, using)) => {
                let Some(p) = ctx.rep_point(origin) else { return Ok(Some(Value::None)) };
                let items = set.items(ctx, Hints::default())?;
                if !items.is_empty() {
                    let nn = net_nearest(ctx, &set, m, Metric::Time, using, 2.0 * r, sc, span)?;
                    if let Some((n, snap)) = nn.net.snap(p, m) {
                        let lab = nn.label[n as usize];
                        // a match outside the box is more than r − snap away from the node
                        if lab != u32::MAX && nn.dist[n as usize] * vmax(m) <= r - snap {
                            return Ok(Some(nn.items[lab as usize].clone()));
                        }
                    }
                }
            }
        }
        // (measured on germany.knutdb: 3 is as good as 2 and 4 in a straight line, and the best
        // compromise along the network, where 4 overshoots and 2 loads the network too often)
        r *= 3.0;
    }
}

/// `nearest of SELECTION to a by MODE` in the stored routing network: a search from `a`
/// bounded by a time limit; the matches are read in the box that the limit can reach at the
/// highest speed. Done when the nearest match is reached within the limit (anything reached
/// later is farther); else the limit triples. `None` without a stored network.
fn nearest_stored(ctx: &Ctx, sel: &'static Expr, origin: &Value, g: &Geom, m: Mode, sc: &Sc) -> R<Option<Value>> {
    let db = ctx.db()?;
    let Some(net) = crate::routing::load(db) else { return Ok(None) };
    let Some(p) = ctx.rep_point(origin) else { return Ok(Some(Value::None)) };
    let Some(s) = crate::routing::snap(db, &net, p, m) else { return Ok(Some(Value::None)) };
    let sources = crate::routing::entries(&net, &s, m, true);
    let vmax = net.max_speed(m).max(0.1);
    let ob = g.bbox();
    let key = origin.key();
    // the first limit reaches 1 km at the highest speed
    let mut limit = 1000.0 / vmax;
    loop {
        let bx = ob.expand_m(limit * vmax);
        let whole = bx.contains_box(&db.extent);
        let Some(v) = ctx.select_in_box(sel, sc, bx)? else { return Ok(None) };
        let set = ctx.as_set(&v, sel.span)?;
        let items = set.items(ctx, Hints { geom: true, ..Default::default() })?;
        // nearest in a straight line first: a match d away takes at least d / vmax
        let mut cands: Vec<(f64, usize, P)> = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if it.key() == key {
                continue;
            }
            let Some(q) = ctx.rep_point(it) else { continue };
            cands.push((geom::haversine(p, q), i, q));
        }
        cands.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let lim = if whole { f64::INFINITY } else { limit };
        let dist = match crate::routing::reach_bounded(&net, &sources, m, lim, usize::MAX) {
            Some((d, _)) => d,
            None => crate::routing::reach(&net, &sources, m, lim),
        };
        // (times in tenths of seconds, as in the network). Attached to the network in groups,
        // nearest in a straight line first (one scan per group); the rest is left out once
        // even at the highest speed it cannot be faster than the best so far.
        let near: Vec<(f64, usize, P)> = cands.iter().copied().filter(|c| c.0 / vmax <= lim).collect();
        let mut best: Option<(f64, usize)> = None;
        for group in near.chunks(8) {
            if best.is_some_and(|b| group[0].0 / vmax * 10.0 >= b.0) {
                break;
            }
            let snaps = crate::routing::snap_many(db, &net, &group.iter().map(|c| c.2).collect::<Vec<_>>(), m);
            for (&(d, i, _), t) in group.iter().zip(&snaps) {
                if best.is_some_and(|b| d / vmax * 10.0 >= b.0) {
                    break;
                }
                let Some(t) = t else { continue };
                let secs = crate::routing::entries(&net, t, m, false).iter().map(|&(n, c)| dist[n as usize] + c).fold(f64::INFINITY, f64::min);
                if secs.is_finite() && best.is_none_or(|b| secs < b.0) {
                    best = Some((secs, i));
                }
            }
        }
        match best {
            Some((secs, i)) if whole || secs <= limit * 10.0 => return Ok(Some(items[i].clone())),
            None if whole => return Ok(Some(Value::None)),
            _ => limit *= 3.0,
        }
    }
}

pub fn point_of(ctx: &Ctx, v: &Value) -> Option<P> {
    ctx.rep_point(v)
}

pub fn kind_any() -> Kind {
    Kind::Any
}

/// A box that every match of `var` must intersect, derived from the conjuncts of the
/// condition: `var within X`, `var overlapping X` and `var near some/all _ in S closer than d`
/// with closed X and S. The scan skips blocks and segments outside it.
pub fn scan_hint(ctx: &Ctx, preds: &[&'static Expr], var: &str, sc: &Sc) -> R<Option<BBoxF>> {
    fn conjuncts(e: &'static Expr, out: &mut Vec<&'static Expr>) {
        match &e.kind {
            ExprKind::And(a, b) => {
                conjuncts(a, out);
                conjuncts(b, out);
            }
            _ => out.push(e),
        }
    }
    let mut cs = Vec::new();
    for p in preds {
        conjuncts(p, &mut cs);
    }
    let mut hint: Option<BBoxF> = None;
    let mut add = |b: BBoxF| {
        hint = Some(match hint {
            None => b,
            Some(h) => BBoxF { minx: h.minx.max(b.minx), miny: h.miny.max(b.miny), maxx: h.maxx.min(b.maxx), maxy: h.maxy.min(b.maxy) },
        });
    };
    for c in cs {
        let ExprKind::Rel(l, tail) = &c.kind else { continue };
        if !matches!(&l.kind, ExprKind::Ident(v) if v == var) {
            continue;
        }
        match tail {
            RelTail::Within(x) | RelTail::Overlapping(x) if ctx.closed(x) => {
                let v = ctx.eval(x, sc)?;
                if v.is_none() {
                    continue;
                }
                let b = ctx.region_of(&v, x.span)?.bbox();
                if !b.is_empty() {
                    add(b);
                }
            }
            RelTail::Near { quant: Quant::Some | Quant::All, set, threshold } if ctx.closed(set) && threshold.as_ref().is_none_or(|t| ctx.closed(&t.value)) => {
                let (d, dim, _, mode, _, _) = threshold_value(ctx, threshold.as_ref(), sc)?;
                let radius = match (mode, dim) {
                    (None, _) => d,
                    (Some(m), Dim::TIME) => d * speed(m),
                    (Some(_), _) => d,
                };
                if !radius.is_finite() {
                    continue;
                }
                let sv = ctx.eval(set, sc)?;
                let st = ctx.as_set(&sv, set.span)?;
                let ix = geo_index(ctx, &st, set.span)?;
                let mut b = BBoxF::EMPTY;
                for (_, _, bb) in &ix.items {
                    b.union(bb);
                }
                // an empty set: nothing is near some member
                // a little more than the radius: expand_m uses a slightly different metre per degree
                add(if b.is_empty() { BBoxF::EMPTY } else { b.expand_m(radius * 1.01 + 1.0) });
            }
            _ => {}
        }
    }
    Ok(hint)
}
