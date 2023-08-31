//! Pushdown analysis (KNUT_LANG.md §7.4): tag predicates that the scan evaluates once per
//! tagset of a block, and the set of columns a condition needs.

use knut_lang::ast::*;
use std::collections::HashSet;

/// A predicate over the tags of one version.
#[derive(Clone, Debug, PartialEq)]
pub enum TagPred {
    True,
    False,
    Has(String),
    Eq(String, String),
    In(String, Vec<String>),
    Not(Box<TagPred>),
    And(Vec<TagPred>),
    Or(Vec<TagPred>),
}

impl std::fmt::Display for TagPred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = |f: &mut std::fmt::Formatter<'_>, xs: &[TagPred], op: &str| -> std::fmt::Result {
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    write!(f, " {op} ")?;
                }
                if matches!(x, TagPred::And(_) | TagPred::Or(_)) { write!(f, "({x})")? } else { write!(f, "{x}")? }
            }
            Ok(())
        };
        match self {
            TagPred::True => write!(f, "true"),
            TagPred::False => write!(f, "false"),
            TagPred::Has(k) => write!(f, "{k}"),
            TagPred::Eq(k, v) => write!(f, "{k}={v}"),
            TagPred::In(k, vs) => write!(f, "{k} in [{}]", vs.join(", ")),
            TagPred::Not(x) => write!(f, "not {x}"),
            TagPred::And(xs) => list(f, xs, "and"),
            TagPred::Or(xs) => list(f, xs, "or"),
        }
    }
}

impl TagPred {
    pub fn eval<K: AsRef<str>, V: AsRef<str>>(&self, tags: &[(K, V)]) -> bool {
        let get = |k: &str| tags.iter().find(|(kk, _)| kk.as_ref() == k).map(|(_, v)| v.as_ref());
        match self {
            TagPred::True => true,
            TagPred::False => false,
            TagPred::Has(k) => get(k).is_some(),
            TagPred::Eq(k, v) => get(k) == Some(v.as_str()),
            TagPred::In(k, vs) => get(k).map_or(false, |x| vs.iter().any(|v| v == x)),
            TagPred::Not(p) => !p.eval(tags),
            TagPred::And(ps) => ps.iter().all(|p| p.eval(tags)),
            TagPred::Or(ps) => ps.iter().any(|p| p.eval(tags)),
        }
    }

    /// Keys (and values) that must be present for the predicate to hold; used with the
    /// R-tree bloom filters.
    pub fn required(&self) -> Vec<(String, Option<String>)> {
        match self {
            TagPred::Has(k) => vec![(k.clone(), None)],
            TagPred::Eq(k, v) => vec![(k.clone(), Some(v.clone()))],
            TagPred::In(k, vs) if vs.len() == 1 => vec![(k.clone(), Some(vs[0].clone()))],
            TagPred::In(k, _) => vec![(k.clone(), None)],
            TagPred::And(ps) => ps.iter().flat_map(|p| p.required()).collect(),
            _ => Vec::new(),
        }
    }

    pub fn is_true(&self) -> bool {
        matches!(self, TagPred::True)
    }

    fn and(a: TagPred, b: TagPred) -> TagPred {
        match (a, b) {
            (TagPred::True, x) | (x, TagPred::True) => x,
            (TagPred::False, _) | (_, TagPred::False) => TagPred::False,
            (TagPred::And(mut xs), TagPred::And(ys)) => {
                xs.extend(ys);
                TagPred::And(xs)
            }
            (TagPred::And(mut xs), y) | (y, TagPred::And(mut xs)) => {
                xs.push(y);
                TagPred::And(xs)
            }
            (x, y) => TagPred::And(vec![x, y]),
        }
    }

    fn or(a: TagPred, b: TagPred) -> TagPred {
        match (a, b) {
            (TagPred::True, _) | (_, TagPred::True) => TagPred::True,
            (TagPred::False, x) | (x, TagPred::False) => x,
            (TagPred::Or(mut xs), y) => {
                xs.push(y);
                TagPred::Or(xs)
            }
            (x, y) => TagPred::Or(vec![x, y]),
        }
    }
}

/// Result of extracting a tag predicate: `exact` if the predicate is equivalent to the
/// whole condition (no residual evaluation needed), otherwise it is a necessary condition.
#[derive(Clone, Debug)]
pub struct Extracted {
    pub pred: TagPred,
    pub exact: bool,
}

/// Bare words that stand for text values (`w:highway == primary`); provided by the checker.
pub trait TextIdents {
    fn is_text(&self, e: &Expr) -> bool;
    /// True for a comparison with a W001 warning: elements without the tag are then kept
    /// for the residual, so that they can be counted (data part of W001).
    fn counts_missing(&self, _e: &Expr) -> bool {
        false
    }
}

fn text_of(e: &Expr, ti: &dyn TextIdents) -> Option<String> {
    match &e.kind {
        ExprKind::Str(s) => Some(s.clone()),
        ExprKind::Num(n) => Some(knut_lang::lexer::fmt_num(*n)),
        ExprKind::Ident(w) if ti.is_text(e) => Some(w.clone()),
        _ => None,
    }
}

/// `var:key` with a literal key.
fn tag_of<'a>(e: &'a Expr, var: &str) -> Option<(&'a str, Option<&'a str>)> {
    match &e.kind {
        ExprKind::Tag(base, TagKey::Lit(k), v) => match &base.kind {
            ExprKind::Ident(n) if n == var => Some((k.as_str(), v.as_deref())),
            _ => None,
        },
        _ => None,
    }
}

/// Extracts the tag predicate on `var` from condition `e` (KNUT_LANG.md §5.3: missing
/// values compare false, `== none` tests absence).
pub fn extract(e: &Expr, var: &str, ti: &dyn TextIdents) -> Extracted {
    let unknown = Extracted { pred: TagPred::True, exact: false };
    match &e.kind {
        ExprKind::Tag(..) => match tag_of(e, var) {
            Some((k, None)) => Extracted { pred: TagPred::Has(k.into()), exact: true },
            Some((k, Some(v))) => Extracted { pred: TagPred::Eq(k.into(), v.into()), exact: true },
            None => unknown,
        },
        ExprKind::Bool(b) => Extracted { pred: if *b { TagPred::True } else { TagPred::False }, exact: true },
        ExprKind::Not(x) => {
            let i = extract(x, var, ti);
            if i.exact { Extracted { pred: TagPred::Not(Box::new(i.pred)), exact: true } } else { unknown }
        }
        ExprKind::And(a, b) => {
            let (a, b) = (extract(a, var, ti), extract(b, var, ti));
            Extracted { pred: TagPred::and(a.pred, b.pred), exact: a.exact && b.exact }
        }
        ExprKind::Or(a, b) => {
            let (a, b) = (extract(a, var, ti), extract(b, var, ti));
            Extracted { pred: TagPred::or(a.pred, b.pred), exact: a.exact && b.exact }
        }
        ExprKind::Rel(..) if ti.counts_missing(e) => unknown,
        ExprKind::Rel(l, RelTail::Cmp(op @ (CmpOp::Eq | CmpOp::Ne), r)) => {
            let (tag, other) = match (tag_of(l, var), tag_of(r, var)) {
                (Some(t), _) => (t, &**r),
                (_, Some(t)) => (t, &**l),
                _ => return unknown,
            };
            let (k, None) = tag else { return unknown };
            if matches!(other.kind, ExprKind::None) {
                let has = TagPred::Has(k.into());
                return Extracted { pred: if *op == CmpOp::Eq { TagPred::Not(Box::new(has)) } else { has }, exact: true };
            }
            match text_of(other, ti) {
                Some(v) => {
                    let eq = TagPred::Eq(k.into(), v);
                    let pred = if *op == CmpOp::Eq {
                        eq
                    } else {
                        TagPred::And(vec![TagPred::Has(k.into()), TagPred::Not(Box::new(eq))])
                    };
                    // numbers compare numerically ("50" == "50.0"); keep the residual then
                    let exact = !matches!(other.kind, ExprKind::Num(_));
                    if exact {
                        Extracted { pred, exact }
                    } else {
                        Extracted { pred: TagPred::Has(k.into()), exact: false }
                    }
                }
                None => Extracted { pred: TagPred::Has(k.into()), exact: false },
            }
        }
        ExprKind::Rel(l, RelTail::Cmp(_, _)) | ExprKind::Rel(l, RelTail::Between(..)) => match tag_of(l, var) {
            Some((k, None)) => Extracted { pred: TagPred::Has(k.into()), exact: false },
            _ => unknown,
        },
        ExprKind::Unit(inner, _, _) => match tag_of(inner, var) {
            Some((k, None)) => Extracted { pred: TagPred::Has(k.into()), exact: false },
            _ => unknown,
        },
        ExprKind::Rel(l, RelTail::In(r)) => {
            let Some((k, None)) = tag_of(l, var) else { return unknown };
            if let ExprKind::List(xs) = &r.kind {
                let vals: Option<Vec<String>> = xs.iter().map(|x| text_of(x, ti)).collect();
                if let Some(vals) = vals {
                    if xs.iter().all(|x| !matches!(x.kind, ExprKind::Num(_))) {
                        return Extracted { pred: TagPred::In(k.into(), vals), exact: true };
                    }
                }
            }
            Extracted { pred: TagPred::Has(k.into()), exact: false }
        }
        _ => unknown,
    }
}

/// Fields of an element that need its geometry.
pub const GEOM_FIELDS: &[&str] = &["geometry", "length", "area", "center", "lon", "lat"];

/// True if evaluating `e` may need the geometry of `var`: geometric fields, spatial
/// relations, or passing the element on (to a lambda, a function or an output).
pub fn needs_geometry(e: &Expr, var: &str) -> bool {
    let mut need = false;
    walk_uses(e, var, &mut |use_| {
        if use_ {
            need = true;
        }
    });
    need
}

fn is_var(e: &Expr, var: &str) -> bool {
    matches!(&e.kind, ExprKind::Ident(n) if n == var)
}

/// Calls `f(true)` for every use of `var` that may require geometry.
fn walk_uses(e: &Expr, var: &str, f: &mut dyn FnMut(bool)) {
    match &e.kind {
        ExprKind::Field(b, name, _) if is_var(b, var) => {
            f(GEOM_FIELDS.contains(&name.as_str()));
            return;
        }
        ExprKind::Tag(b, k, _) if is_var(b, var) => {
            if let TagKey::Expr(k) = k {
                walk_uses(k, var, f);
            }
            return;
        }
        ExprKind::Ident(n) if n == var => {
            // bare use (spatial relation, comparison, passed on)
            f(true);
            return;
        }
        ExprKind::Rel(l, RelTail::NextOf(r) | RelTail::PreviousOf(r)) if is_var(l, var) || is_var(r, var) => {
            if !is_var(l, var) {
                walk_uses(l, var, f);
            }
            if !is_var(r, var) {
                walk_uses(r, var, f);
            }
            return;
        }
        _ => {}
    }
    for c in e.children() {
        walk_uses(c, var, f);
    }
}

/// Fields of `var` used in `e` (for deciding which columns to decode).
pub fn fields_used(e: &Expr, var: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    e.walk(&mut |x| {
        if let ExprKind::Field(b, name, _) = &x.kind {
            if is_var(b, var) {
                out.insert(name.clone());
            }
        }
    });
    out
}
