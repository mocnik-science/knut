//! Statistics (KNUT_LANG.md Anhang A): distributions, entropy, Gini, variance, spread,
//! streaks, transitions, power laws, forecasts, regression, confusion, classification,
//! and the grids for `per cell` and `per tile`.

use crate::eval::Ctx;
use crate::fields;
use crate::geom::{self, P};
use crate::set::SetV;
use crate::value::*;
use knut_lang::ast::OfFunc;
use knut_lang::dates::TimeUnit;
use knut_lang::diag::Span;
use knut_lang::units::{self, Dim};
use rustc_hash::FxHashMap as HashMap;
use std::sync::Arc;

fn nums(ctx: &Ctx, vs: &[Value], span: Span) -> R<(Vec<f64>, Option<Num>)> {
    let mut out = Vec::new();
    let mut tmpl = None;
    for v in vs {
        let v = match v {
            Value::Group(g) => g.value.clone(),
            other => other.clone(),
        };
        if v.is_none() {
            continue;
        }
        if let Some(n) = ctx.to_num(&v, None, span)? {
            tmpl.get_or_insert(n);
            out.push(n.v);
        }
    }
    Ok((out, tmpl))
}

/// Counts of distinct values, most frequent first.
pub fn distribution(vs: &[Value]) -> Vec<(Value, u64)> {
    let mut m: HashMap<Key, (Value, u64)> = HashMap::default();
    for v in vs {
        let v = match v {
            Value::Group(g) => g.key.clone(),
            other => other.clone(),
        };
        m.entry(v.key()).or_insert_with(|| (v.clone(), 0)).1 += 1;
    }
    let mut out: Vec<(Key, (Value, u64))> = m.into_iter().collect();
    out.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(&b.0)));
    out.into_iter().map(|(_, x)| x).collect()
}

/// Distribution of many distinct numbers in classes (from, to, count): about 20 classes
/// with round bounds, logarithmic (1–2–5) for positive values spanning several orders of
/// magnitude. `None` if the values are not numbers or have few distinct values.
pub fn binned(vs: &[Value]) -> Option<Grouped> {
    let mut xs = Vec::with_capacity(vs.len());
    let mut tmpl: Option<Num> = None;
    for v in vs {
        match v {
            Value::Num(n) if n.v.is_finite() => {
                if tmpl.is_some_and(|t| t.dim != n.dim) {
                    return None;
                }
                tmpl.get_or_insert(*n);
                xs.push(n.v);
            }
            Value::None => {}
            _ => return None,
        }
    }
    let t = tmpl?;
    xs.sort_by(f64::total_cmp);
    let mut distinct = 1;
    for w in xs.windows(2) {
        if w[0] != w[1] {
            distinct += 1;
            if distinct > 50 {
                break;
            }
        }
    }
    if distinct <= 50 {
        return None;
    }
    let (lo, hi) = (xs[0], xs[xs.len() - 1]);
    let q = |p: f64| xs[((xs.len() - 1) as f64 * p) as usize];
    let bounds: Vec<f64> = if lo > 0.0 && q(0.99) / q(0.01).max(f64::MIN_POSITIVE) > 100.0 {
        // 1, 2, 5, 10, 20, 50, …: from the last bound <= lo to the first bound > hi
        let e0 = lo.log10().floor() as i32;
        let seq = (e0..e0 + 40).flat_map(|e| [1.0, 2.0, 5.0].map(|m| m * 10f64.powi(e)));
        let mut b: Vec<f64> = Vec::new();
        for x in seq {
            if x <= lo {
                b.clear();
            }
            b.push(x);
            if x > hi {
                break;
            }
        }
        b
    } else {
        let raw = (hi - lo) / 20.0;
        let mag = 10f64.powf(raw.log10().floor());
        let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
        let start = (lo / step).floor() * step;
        let mut b = vec![start];
        while *b.last().unwrap() <= hi {
            b.push(b.last().unwrap() + step);
        }
        b
    };
    let mut counts = vec![0u64; bounds.len() - 1];
    let mut i = 0;
    for x in &xs {
        while i + 1 < counts.len() && *x >= bounds[i + 1] {
            i += 1;
        }
        counts[i] += 1;
    }
    let num = |x: f64| Value::Num(Num { v: x, ..t });
    let rows = counts
        .iter()
        .enumerate()
        .map(|(i, &c)| Group { key: Value::List(Arc::new(vec![num(bounds[i]), num(bounds[i + 1])])), value: Value::num(c as f64), count: c, items: None })
        .collect();
    Some(Grouped { key_names: vec!["from".into(), "to".into()], value_name: "count".into(), rows })
}

/// Shannon entropy in bits. Groups with numbers (`count of … per k`) are weighted by their
/// value; other values by their frequency.
pub fn entropy(vs: &[Value]) -> f64 {
    let weights: Option<Vec<f64>> = vs
        .iter()
        .map(|v| match v {
            Value::Group(g) => match &g.value {
                Value::Num(n) if n.v >= 0.0 => Some(n.v),
                _ => None,
            },
            _ => None,
        })
        .collect();
    if let Some(w) = weights.filter(|w| !w.is_empty()) {
        let total: f64 = w.iter().sum();
        if total <= 0.0 {
            return 0.0;
        }
        return -w.iter().filter(|x| **x > 0.0).map(|x| {
            let p = x / total;
            p * p.log2()
        }).sum::<f64>();
    }
    let d = distribution(vs);
    let n: u64 = d.iter().map(|x| x.1).sum();
    if n == 0 {
        return 0.0;
    }
    -d.iter().map(|(_, c)| {
        let p = *c as f64 / n as f64;
        p * p.log2()
    }).sum::<f64>()
}

pub fn gini(xs: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len() as f64;
    let sum: f64 = v.iter().sum();
    if sum == 0.0 {
        return Some(0.0);
    }
    let w: f64 = v.iter().enumerate().map(|(i, x)| (i as f64 + 1.0) * x).sum();
    Some((2.0 * w) / (n * sum) - (n + 1.0) / n)
}

/// Sample variance (n − 1).
pub fn variance(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    Some(xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0))
}

/// Standard distance of points around their mean centre, in metres.
pub fn spread(ps: &[P]) -> Option<f64> {
    if ps.is_empty() {
        return None;
    }
    let n = ps.len() as f64;
    let c = (ps.iter().map(|p| p.0).sum::<f64>() / n, ps.iter().map(|p| p.1).sum::<f64>() / n);
    Some((ps.iter().map(|p| geom::haversine(*p, c).powi(2)).sum::<f64>() / n).sqrt())
}

/// Lengths (in days) of runs of consecutive days with activity, longest first.
pub fn streaks(times: &[i64]) -> Vec<u64> {
    let mut days: Vec<i64> = times.iter().map(|t| t.div_euclid(86_400)).collect();
    days.sort_unstable();
    days.dedup();
    let mut out = Vec::new();
    let mut run = 0u64;
    let mut prev: Option<i64> = None;
    for d in days {
        match prev {
            Some(p) if d == p + 1 => run += 1,
            _ => {
                if run > 0 {
                    out.push(run);
                }
                run = 1;
            }
        }
        prev = Some(d);
    }
    if run > 0 {
        out.push(run);
    }
    out.sort_unstable_by(|a, b| b.cmp(a));
    out
}

pub fn finish_values(ctx: &Ctx, f: OfFunc, vs: Vec<Value>, span: Span, set_txt: &str) -> R<Value> {
    let _ = set_txt;
    Ok(match f {
        OfFunc::Distribution => {
            if let Some(g) = binned(&vs) {
                return Ok(Value::Grouped(Arc::new(g)));
            }
            let rows = distribution(&vs)
                .into_iter()
                .map(|(k, c)| Group { key: k, value: Value::num(c as f64), count: c, items: None })
                .collect();
            Value::Grouped(Arc::new(Grouped { key_names: vec!["value".into()], value_name: "count".into(), rows }))
        }
        OfFunc::Entropy => Value::num(entropy(&vs)),
        OfFunc::Distinct => {
            let xs: Vec<Value> = vs.into_iter().map(|v| if let Value::Tag(t) = v { Value::Text(t.text.clone()) } else { v }).collect();
            Value::Set(Arc::new(SetV::from_vec(xs)))
        }
        OfFunc::Gini => {
            let (xs, _) = nums(ctx, &vs, span)?;
            gini(&xs).map_or(Value::None, Value::num)
        }
        OfFunc::Variance => {
            let (xs, t) = nums(ctx, &vs, span)?;
            match (variance(&xs), t) {
                (Some(v), Some(t)) if !t.dim.is_none() => {
                    let d = t.dim.mul(t.dim).unwrap_or(Dim::NONE);
                    Value::Num(Num { v, dim: d, unit: units::display_unit(d) })
                }
                (Some(v), _) => Value::num(v),
                _ => Value::None,
            }
        }
        OfFunc::Spread => {
            let ps: Vec<P> = vs.iter().filter_map(|v| ctx.rep_point(v)).collect();
            spread(&ps).map_or(Value::None, |m| Value::Num(Num::base(m, Dim::LENGTH)))
        }
        OfFunc::Streaks => {
            let ts: Vec<i64> = vs.iter().filter_map(|v| fields::time_of(ctx, v)).collect();
            Value::List(Arc::new(
                streaks(&ts).into_iter().map(|d| Value::Num(Num::with_unit(d as f64, units::unit("days").unwrap()))).collect(),
            ))
        }
        OfFunc::Transitions => {
            let mut m: HashMap<(Key, Key), (String, u64)> = HashMap::default();
            let labels: Vec<Value> = vs
                .iter()
                .map(|v| match v {
                    Value::Group(g) => g.value.clone(),
                    other => other.clone(),
                })
                .collect();
            for w in labels.windows(2) {
                if w[0].key() != w[1].key() {
                    m.entry((w[0].key(), w[1].key())).or_insert_with(|| (format!("{} → {}", w[0].display(), w[1].display()), 0)).1 += 1;
                }
            }
            let mut rows: Vec<((Key, Key), (String, u64))> = m.into_iter().collect();
            rows.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(&b.0)));
            Value::Grouped(Arc::new(Grouped {
                key_names: vec!["transition".into()],
                value_name: "count".into(),
                rows: rows.into_iter().map(|(_, (l, c))| Group { key: Value::text(&l), value: Value::num(c as f64), count: c, items: None }).collect(),
            }))
        }
        OfFunc::Earliest | OfFunc::Latest => {
            let mut best: Option<(Key, Value)> = None;
            for v in vs {
                let x = match (&v, fields::time_of(ctx, &v)) {
                    (Value::Elem(_) | Value::Group(_) | Value::Tuple(_), Some(t)) => Value::instant(t),
                    _ => v.clone(),
                };
                if x.is_none() {
                    continue;
                }
                let k = x.key();
                let better = match &best {
                    None => true,
                    Some((bk, _)) => {
                        if f == OfFunc::Earliest {
                            k < *bk
                        } else {
                            k > *bk
                        }
                    }
                };
                if better {
                    best = Some((k, x));
                }
            }
            best.map_or(Value::None, |b| b.1)
        }
        OfFunc::Days | OfFunc::Weeks | OfFunc::Months | OfFunc::Years => {
            let u = match f {
                OfFunc::Days => TimeUnit::Day,
                OfFunc::Weeks => TimeUnit::Week,
                OfFunc::Months => TimeUnit::Month,
                _ => TimeUnit::Year,
            };
            fields::buckets_of(ctx, &vs, u)
        }
        OfFunc::Sum | OfFunc::Average => {
            let (xs, t) = nums(ctx, &vs, span)?;
            if xs.is_empty() {
                return Ok(Value::None);
            }
            let s: f64 = xs.iter().sum();
            let v = if f == OfFunc::Sum { s } else { s / xs.len() as f64 };
            Value::Num(match t {
                Some(t) => Num { v, ..t },
                None => Num::plain(v),
            })
        }
        _ => Value::None,
    })
}

// ----- grids -------------------------------------------------------------------------------

/// EPSG:3035 (ETRS89 / LAEA Europe), ellipsoidal forward projection.
fn laea_europe(p: P) -> (f64, f64) {
    let a = 6_378_137.0f64;
    let f = 1.0 / 298.257_222_101;
    let e2: f64 = 2.0 * f - f * f;
    let e = e2.sqrt();
    let q = |phi: f64| -> f64 {
        let s = phi.sin();
        (1.0 - e2) * (s / (1.0 - e2 * s * s) - (1.0 / (2.0 * e)) * ((1.0 - e * s) / (1.0 + e * s)).ln())
    };
    let (lam0, phi0) = (10f64.to_radians(), 52f64.to_radians());
    let (lam, phi) = (p.0.to_radians(), p.1.to_radians());
    let qp = q(std::f64::consts::FRAC_PI_2);
    let rq = a * (qp / 2.0).sqrt();
    let beta = (q(phi) / qp).clamp(-1.0, 1.0).asin();
    let beta0 = (q(phi0) / qp).clamp(-1.0, 1.0).asin();
    let m0 = phi0.cos() / (1.0 - e2 * phi0.sin().powi(2)).sqrt();
    let d = a * m0 / (rq * beta0.cos());
    let b = rq * (2.0 / (1.0 + beta0.sin() * beta.sin() + beta0.cos() * beta.cos() * (lam - lam0).cos())).sqrt();
    let x = 4_321_000.0 + b * d * beta.cos() * (lam - lam0).sin();
    let y = 3_210_000.0 + (b / d) * (beta0.cos() * beta.sin() - beta0.sin() * beta.cos() * (lam - lam0).cos());
    (x, y)
}

/// EPSG:6933 (WGS 84 / NSIDC EASE-Grid 2.0 Global), cylindrical equal area.
fn ease_global(p: P) -> (f64, f64) {
    let a = 6_378_137.0f64;
    let f = 1.0 / 298.257_223_563;
    let e2: f64 = 2.0 * f - f * f;
    let e = e2.sqrt();
    let phi_ts = 30f64.to_radians();
    let k0 = phi_ts.cos() / (1.0 - e2 * phi_ts.sin().powi(2)).sqrt();
    let s = p.1.to_radians().sin();
    let q = (1.0 - e2) * (s / (1.0 - e2 * s * s) - (1.0 / (2.0 * e)) * ((1.0 - e * s) / (1.0 + e * s)).ln());
    (a * k0 * p.0.to_radians(), a * q / (2.0 * k0))
}

fn in_europe(p: P) -> bool {
    (-35.0..=45.0).contains(&p.0) && (25.0..=75.0).contains(&p.1)
}

fn size_label(m: f64) -> String {
    if m >= 1000.0 && (m / 1000.0).fract() == 0.0 { format!("{}km", (m / 1000.0) as i64) } else { format!("{}m", m as i64) }
}

/// Name of the equal-area grid cell of `size` metres containing `p` (EEA naming).
pub fn cell_name(p: P, size: f64) -> String {
    let (x, y, prefix) = if in_europe(p) {
        let (x, y) = laea_europe(p);
        (x, y, "")
    } else {
        let (x, y) = ease_global(p);
        (x, y, "EASE")
    };
    let (ix, iy) = ((x / size).floor() as i64, (y / size).floor() as i64);
    let unit = if size >= 1000.0 { 1000.0 } else { 1.0 };
    let fx = (ix as f64 * size / unit) as i64;
    let fy = (iy as f64 * size / unit) as i64;
    format!("{prefix}{}E{fx}N{fy}", size_label(size))
}

/// Web Mercator tile `z/x/y` containing `p`.
pub fn tile_name(p: P, z: u32) -> String {
    let n = 2f64.powi(z as i32);
    let x = ((p.0 + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0) as i64;
    let lat = p.1.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor().clamp(0.0, n - 1.0) as i64;
    format!("{z}/{x}/{y}")
}

// ----- models ------------------------------------------------------------------------------

/// Ordinary least squares `y = a + b x`: (a, b, r², n).
pub fn ols(xs: &[f64], ys: &[f64]) -> Option<(f64, f64, f64, usize)> {
    let n = xs.len().min(ys.len());
    if n < 2 {
        return None;
    }
    let mx = xs[..n].iter().sum::<f64>() / n as f64;
    let my = ys[..n].iter().sum::<f64>() / n as f64;
    let sxy: f64 = (0..n).map(|i| (xs[i] - mx) * (ys[i] - my)).sum();
    let sxx: f64 = (0..n).map(|i| (xs[i] - mx).powi(2)).sum();
    let syy: f64 = (0..n).map(|i| (ys[i] - my).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let b = sxy / sxx;
    let a = my - b * mx;
    let r2 = if syy == 0.0 { 1.0 } else { sxy * sxy / (sxx * syy) };
    Some((a, b, r2, n))
}

/// Pearson correlation.
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let (_, b, r2, _) = ols(xs, ys)?;
    Some(r2.sqrt() * b.signum())
}

/// Continuous power-law fit by maximum likelihood with x_min = smallest value > 0
/// (Clauset, Shalizi & Newman 2009): α = 1 + n / Σ ln(x / x_min).
pub fn powerlaw(xs: &[f64]) -> Option<(f64, f64, usize)> {
    let v: Vec<f64> = xs.iter().copied().filter(|x| *x > 0.0 && x.is_finite()).collect();
    if v.len() < 2 {
        return None;
    }
    let xmin = v.iter().copied().fold(f64::INFINITY, f64::min);
    let s: f64 = v.iter().map(|x| (x / xmin).ln()).sum();
    if s == 0.0 {
        return None;
    }
    Some((1.0 + v.len() as f64 / s, xmin, v.len()))
}

/// One feature of `classify`: a number, or a place (compared by distance on the sphere).
#[derive(Clone, Copy, Debug)]
pub enum Feature {
    Num(f64),
    Place(P),
}

/// k-nearest-neighbour classifier (k = 5, majority vote, ties to the smaller label).
/// Numbers are standardized (z-values); a place becomes a point on the unit sphere scaled
/// by the spread of the training places, so that neighbours by place are the nearest on the
/// sphere. Returns predictions and leave-one-out accuracy on the labelled items.
pub fn knn(train: &[(Vec<Feature>, Value)], query: &[Vec<Feature>]) -> (Vec<Option<Value>>, Option<f64>) {
    const K: usize = 5;
    if train.is_empty() {
        return (query.iter().map(|_| None).collect(), None);
    }
    let shape: Vec<bool> = train[0].0.iter().map(|f| matches!(f, Feature::Place(_))).collect();
    let n = train.len() as f64;
    // per feature: (offset, scale) for numbers; (centre, spread) for places
    let mut scales = Vec::with_capacity(shape.len());
    for (j, &place) in shape.iter().enumerate() {
        if place {
            let us: Vec<[f64; 3]> = train.iter().map(|(x, _)| unit(place_of(&x[j]))).collect();
            let mut c = [0.0; 3];
            for u in &us {
                for d in 0..3 {
                    c[d] += u[d];
                }
            }
            let len = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt().max(1e-12);
            let c = [c[0] / len, c[1] / len, c[2] / len];
            let ms: f64 = us.iter().map(|u| (0..3).map(|d| (u[d] - c[d]).powi(2)).sum::<f64>()).sum::<f64>() / n;
            scales.push((0.0, if ms > 0.0 { ms.sqrt() } else { 1.0 }));
        } else {
            let mean = train.iter().map(|(x, _)| num_of(&x[j])).sum::<f64>() / n;
            let var = train.iter().map(|(x, _)| (num_of(&x[j]) - mean).powi(2)).sum::<f64>() / n;
            scales.push((mean, if var > 0.0 { var.sqrt() } else { 1.0 }));
        }
    }
    let dims: usize = shape.iter().map(|&p| if p { 3 } else { 1 }).sum();
    let embed = |x: &[Feature]| -> Vec<f64> {
        let mut v = Vec::with_capacity(dims);
        for (j, f) in x.iter().enumerate() {
            match f {
                Feature::Place(p) => v.extend(unit(*p).iter().map(|c| c / scales[j].1)),
                Feature::Num(a) => v.push((a - scales[j].0) / scales[j].1),
            }
        }
        v
    };
    let pts: Vec<Vec<f64>> = train.iter().map(|(x, _)| embed(x)).collect();
    let tree = crate::kdtree::KdTree::build(&pts, dims.max(1));
    let vote = |q: &[f64], skip: Option<u32>| -> Option<Value> {
        let mut votes: HashMap<Key, (Value, u32)> = HashMap::default();
        for (_, i) in tree.nearest(q, K, skip) {
            let l = &train[i as usize].1;
            votes.entry(l.key()).or_insert_with(|| (l.clone(), 0)).1 += 1;
        }
        let mut vs: Vec<(Key, (Value, u32))> = votes.into_iter().collect();
        vs.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(&b.0)));
        vs.into_iter().next().map(|x| x.1.0)
    };
    let preds = crate::kdtree::par_map(query.len(), |i| vote(&embed(&query[i]), None));
    let acc = if train.len() > 1 {
        let ok = crate::kdtree::par_map(train.len(), |i| vote(&pts[i], Some(i as u32)).is_some_and(|p| p.key() == train[i].1.key()));
        Some(ok.iter().filter(|&&b| b).count() as f64 / train.len() as f64)
    } else {
        None
    };
    (preds, acc)
}

fn unit(p: P) -> [f64; 3] {
    let (l, f) = (p.0.to_radians(), p.1.to_radians());
    [f.cos() * l.cos(), f.cos() * l.sin(), f.sin()]
}

fn place_of(f: &Feature) -> P {
    match f {
        Feature::Place(p) => *p,
        Feature::Num(_) => (0.0, 0.0),
    }
}

fn num_of(f: &Feature) -> f64 {
    match f {
        Feature::Num(a) => *a,
        Feature::Place(_) => 0.0,
    }
}

#[cfg(test)]
mod knn_tests {
    use super::*;

    /// At 60° N a hundredth of a degree east is 556 m, eight thousandths north are 890 m:
    /// the eastern points are nearer, although their difference in degrees is larger.
    #[test]
    fn places_are_compared_by_distance_on_the_sphere() {
        let t = |p: P, l: &str| (vec![Feature::Place(p)], Value::Text(l.into()));
        let mut train = Vec::new();
        for i in 0..5 {
            let o = i as f64 * 1e-5;
            train.push(t((10.01 + o, 60.0), "east"));
            train.push(t((10.0 + o, 60.008), "north"));
        }
        let (p, acc) = knn(&train, &[vec![Feature::Place((10.0, 60.0))]]);
        assert_eq!(p.iter().map(|v| v.as_ref().map(|v| v.display())).collect::<Vec<_>>(), vec![Some("east".to_string())]);
        assert_eq!(acc, Some(1.0));
    }
}
