//! Sets (KNUT_LANG.md §5.6): either materialized or lazy (a database scan, possibly with
//! filters on top). Lazy sets are streamed in parallel; they are only materialized when
//! needed (listing, `the`, joins).

use crate::eval::{Ctx, Sc};
use crate::scan::{self, Item, ScanSpec};
use crate::value::*;
use knut_lang::ast::Expr;
use rayon::prelude::*;
use std::sync::{Arc, OnceLock};

/// What the consumer of a set needs from its elements.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hints {
    pub geom: bool,
    pub hist: bool,
}

/// A database query: scan plus the selection's own conditions.
pub struct Query {
    pub spec: ScanSpec,
    /// Variable names bound to the element (pattern variables).
    pub vars: Vec<String>,
    /// Conditions (var_pred, cond) evaluated per element; `exact` ones are skipped when
    /// the tag prefilter already decides them.
    pub preds: Vec<&'static Expr>,
    pub scope: Sc,
    /// Sequence (`A -> B -> C`): pattern length and threshold in seconds.
    pub seq: Option<Seq>,
}

#[derive(Clone)]
pub struct Seq {
    pub kinds: Vec<EKind>,
    pub max_dt: Option<(f64, bool)>,
    pub names: Arc<[String]>,
    pub t_range: Option<(i64, i64)>,
}

pub enum SetSrc {
    Vec(Arc<Vec<Value>>),
    /// Items produced on demand from an index (changesets, users, notes), in order.
    Indexed { len: usize, get: Arc<dyn Fn(usize) -> Value + Send + Sync> },
    Query(Arc<Query>),
    /// Elements of `base` for which all conditions hold, with `var` bound.
    Filter { base: Arc<SetV>, vars: Vec<String>, preds: Vec<&'static Expr>, scope: Sc, extra: Option<Arc<dyn Fn(&Ctx, &Value) -> R<bool> + Send + Sync>> },
}

pub struct SetV {
    pub src: SetSrc,
    mat: OnceLock<Arc<Vec<Value>>>,
}

impl std::fmt::Debug for SetV {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.mat.get() {
            Some(v) => write!(f, "Set({} items)", v.len()),
            None => write!(f, "Set(lazy)"),
        }
    }
}

impl SetV {
    pub fn from_vec(mut v: Vec<Value>) -> SetV {
        v.sort_by_key(|x| x.key());
        v.dedup_by(|a, b| a.key() == b.key());
        let v = Arc::new(v);
        let s = SetV { src: SetSrc::Vec(v.clone()), mat: OnceLock::new() };
        let _ = s.mat.set(v);
        s
    }

    /// A list used where a set is expected: order and duplicates are kept, so that
    /// `count of`, `sum of` and `average of` over `xs.map(…)` see every value.
    pub fn from_list(v: Arc<Vec<Value>>) -> SetV {
        let s = SetV { src: SetSrc::Vec(v.clone()), mat: OnceLock::new() };
        let _ = s.mat.set(v);
        s
    }

    pub fn lazy(src: SetSrc) -> SetV {
        SetV { src, mat: OnceLock::new() }
    }

    pub fn len_hint(&self) -> Option<usize> {
        self.mat.get().map(|v| v.len())
    }

    pub fn is_query(&self) -> Option<&Arc<Query>> {
        match &self.src {
            SetSrc::Query(q) => Some(q),
            _ => None,
        }
    }

    /// Materialized, sorted by (kind, id, version) and without duplicates.
    pub fn items(&self, ctx: &Ctx, hints: Hints) -> R<Arc<Vec<Value>>> {
        if let Some(v) = self.mat.get() {
            return Ok(v.clone());
        }
        let parts = self.fold(ctx, hints, &Vec::new, &|acc: &mut Vec<Value>, v: Value| {
            acc.push(v);
            Ok(())
        })?;
        let mut all: Vec<Value> = parts.into_iter().flatten().collect();
        all.par_sort_by_key(|x| x.key());
        all.dedup_by(|a, b| a.key() == b.key());
        let v = Arc::new(all);
        let _ = self.mat.set(v.clone());
        Ok(v)
    }

    /// Parallel fold; accumulators come back in a deterministic order.
    pub fn fold<A: Send>(&self, ctx: &Ctx, hints: Hints, init: &(dyn Fn() -> A + Sync), step: &(dyn Fn(&mut A, Value) -> R<()> + Sync)) -> R<Vec<A>> {
        if let Some(v) = self.mat.get() {
            return fold_vec(v, init, step);
        }
        match &self.src {
            SetSrc::Vec(v) => fold_vec(v, init, step),
            SetSrc::Indexed { len, get } => {
                let n = *len;
                let chunk = 8192.max(n / 256 + 1);
                let starts: Vec<usize> = (0..n).step_by(chunk).collect();
                let parts: Vec<R<A>> = starts
                    .par_iter()
                    .map(|&s| {
                        let mut acc = init();
                        for i in s..(s + chunk).min(n) {
                            step(&mut acc, get(i))?;
                        }
                        Ok(acc)
                    })
                    .collect();
                let mut out = Vec::new();
                let mut errs = Vec::new();
                for p in parts {
                    match p {
                        Ok(a) => out.push(a),
                        Err(e) => errs.push(e),
                    }
                }
                if let Some(e) = first_error(errs) {
                    return Err(e);
                }
                if out.is_empty() {
                    out.push(init());
                }
                Ok(out)
            }
            SetSrc::Query(q) => fold_query(ctx, q, hints, init, step),
            SetSrc::Filter { base, vars, preds, scope, extra } => {
                let wrapped = |acc: &mut A, v: Value| -> R<()> {
                    if let Some(f) = extra {
                        if !f(ctx, &v)? {
                            return Ok(());
                        }
                    }
                    let sc = ctx.bind_elem(scope, vars, &v);
                    for p in preds {
                        if !ctx.truthy(p, &sc).map_err(|e| attach(e, &v))? {
                            return Ok(());
                        }
                    }
                    step(acc, v)
                };
                base.fold(ctx, hints, init, &wrapped)
            }
        }
    }

    /// Sequential iteration in a deterministic order (materializes).
    pub fn each(&self, ctx: &Ctx, hints: Hints, f: &mut dyn FnMut(&Value) -> R<()>) -> R<()> {
        for v in self.items(ctx, hints)?.iter() {
            f(v)?;
        }
        Ok(())
    }
}

fn attach(e: RtErr, v: &Value) -> RtErr {
    match v {
        Value::Elem(el) => e.at(el),
        _ => e,
    }
}

fn fold_vec<A: Send>(v: &Arc<Vec<Value>>, init: &(dyn Fn() -> A + Sync), step: &(dyn Fn(&mut A, Value) -> R<()> + Sync)) -> R<Vec<A>> {
    let parts: Vec<R<A>> = v
        .par_chunks(4096.max(v.len() / 256 + 1))
        .map(|c| {
            let mut acc = init();
            for x in c {
                step(&mut acc, x.clone())?;
            }
            Ok(acc)
        })
        .collect();
    let mut out = Vec::new();
    let mut errs = Vec::new();
    for p in parts {
        match p {
            Ok(a) => out.push(a),
            Err(e) => errs.push(e),
        }
    }
    if let Some(e) = first_error(errs) {
        return Err(e);
    }
    if out.is_empty() {
        out.push(init());
    }
    Ok(out)
}

fn fold_query<A: Send>(ctx: &Ctx, q: &Query, hints: Hints, init: &(dyn Fn() -> A + Sync), step: &(dyn Fn(&mut A, Value) -> R<()> + Sync)) -> R<Vec<A>> {
    let db = ctx.db()?;
    let mut spec = q.spec.clone();
    spec.need_geom |= hints.geom;
    spec.need_hist |= hints.hist;
    let wrapped = |acc: &mut A, it: Item| -> R<()> {
        match it {
            Item::Elem(e) => {
                let v = Value::Elem(e.clone());
                if !q.preds.is_empty() {
                    let sc = ctx.bind_elem(&q.scope, &q.vars, &v);
                    for p in &q.preds {
                        if !ctx.truthy(p, &sc).map_err(|x| x.at(&e))? {
                            return Ok(());
                        }
                    }
                }
                step(acc, v)
            }
            Item::Hist(h) => match &q.seq {
                Some(seq) => crate::eval::match_sequence(ctx, q, seq, &h, &mut |t| step(acc, t)),
                None => Ok(()),
            },
        }
    };
    scan::fold(db, &spec, init, &wrapped)
}
