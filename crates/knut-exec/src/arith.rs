//! Arithmetic and comparisons with units, dates and tag values (KNUT_LANG.md §5.3–5.5).

use crate::eval::{Ctx, cap};
use crate::value::*;
use knut_lang::ast::{BinOp, CmpOp, Expr, ExprKind};
use knut_lang::dates::{self, DateVal, Precision};
use knut_lang::diag::{Diagnostic, Span, Text2};
use knut_lang::units::{self, Calendar, Dim};
use std::cmp::Ordering;
use std::sync::Arc;

/// A duration in seconds with a readable display unit.
pub fn duration(secs: f64) -> Num {
    let a = secs.abs();
    let name = if a < 120.0 {
        "s"
    } else if a < 7200.0 {
        "min"
    } else if a < 2.0 * 86_400.0 {
        "h"
    } else {
        "days"
    };
    Num { v: secs, dim: Dim::TIME, unit: units::unit(name) }
}

fn e306(span: Span, a: Dim, b: Dim, op: BinOp) -> RtErr {
    let verb = if op == BinOp::Add { ("added", "addiert") } else { ("subtracted", "subtrahiert") };
    RtErr::fatal(
        Diagnostic::error(
            "E306",
            span,
            Text2::new(
                format!("{} and {} cannot be {}.", cap(&a.a_name()), b.a_name(), verb.0),
                format!("{} und {} lassen sich nicht {}.", cap(&a.name(true)), b.name(true), verb.1),
            ),
        )
        .why(
            "Units can be mixed within one kind of measure (300 m + 1 km),\nbut not across kinds.",
            "Einheiten lassen sich innerhalb eines Maßes mischen (300 m + 1 km),\naber nicht zwischen verschiedenen Maßen.",
        ),
    )
}

fn add_time(t: &DateVal, n: &Num, sign: i64) -> i64 {
    match n.unit.and_then(|u| u.calendar) {
        Some(Calendar::Months(m)) => dates::add_months(t.start, sign * (n.shown() * m as f64).round() as i64),
        None => t.start + sign * n.v.round() as i64,
    }
}

pub fn binop(ctx: &Ctx, op: BinOp, a: Value, b: Value, ea: &Expr, eb: &Expr, whole: &Expr) -> R<Value> {
    if a.is_none() || b.is_none() {
        return Ok(Value::None);
    }
    match (op, &a, &b) {
        (BinOp::Sub, Value::Time(x), Value::Time(y)) => return Ok(Value::Num(duration((x.start - y.start) as f64))),
        (BinOp::Add | BinOp::Sub, Value::Time(t), Value::Num(n)) if n.dim == Dim::TIME => {
            let s = if op == BinOp::Add { 1 } else { -1 };
            return Ok(Value::instant(add_time(t, n, s)));
        }
        (BinOp::Add, Value::Num(n), Value::Time(t)) if n.dim == Dim::TIME => return Ok(Value::instant(add_time(t, n, 1))),
        (BinOp::Add, Value::Text(_) | Value::Tag(_), Value::Text(_)) if !matches!(eb.kind, ExprKind::Num(_))
            && units::parse_value(a.as_text().unwrap()).is_none() => {
                return Ok(Value::text(&format!("{}{}", a.as_text().unwrap(), b.as_text().unwrap())));
            }
        _ => {}
    }
    // only + and − need the same measure on both sides; `levels * 3 m` takes a plain number
    let same = matches!(op, BinOp::Add | BinOp::Sub);
    let want_a = match &b {
        Value::Num(n) if same => Some(n.dim),
        _ => None,
    };
    let want_b = match &a {
        Value::Num(n) if same => Some(n.dim),
        _ => None,
    };
    let (Some(x), Some(y)) = (ctx.to_num(&a, want_a, ea.span)?, ctx.to_num(&b, want_b, eb.span)?) else { return Ok(Value::None) };
    Ok(Value::Num(match op {
        BinOp::Add | BinOp::Sub => {
            if x.dim != y.dim {
                return Err(e306(whole.span, x.dim, y.dim, op));
            }
            let v = if op == BinOp::Add { x.v + y.v } else { x.v - y.v };
            Num { v, dim: x.dim, unit: x.unit.or(y.unit) }
        }
        BinOp::Mul => {
            let dim = x.dim.mul(y.dim).unwrap_or(Dim::NONE);
            let unit = if x.dim.is_none() {
                y.unit
            } else if y.dim.is_none() {
                x.unit
            } else {
                units::display_unit(dim)
            };
            Num { v: x.v * y.v, dim, unit }
        }
        BinOp::Div => {
            if y.v == 0.0 {
                return Ok(Value::None);
            }
            let dim = x.dim.div(y.dim).unwrap_or(Dim::NONE);
            let unit = if y.dim.is_none() {
                x.unit
            } else if x.dim == y.dim {
                None
            } else {
                units::display_unit(dim)
            };
            Num { v: x.v / y.v, dim, unit }
        }
    }))
}

fn as_span(v: &Value) -> Option<DateVal> {
    match v {
        Value::Time(d) => Some(*d),
        Value::Num(n) if n.dim.is_none() && n.v.fract() == 0.0 && (1000.0..10000.0).contains(&n.v) => Some(dates::year(n.v as i64)),
        _ => None,
    }
}

fn cmp_time(op: CmpOp, a: &DateVal, b: &DateVal) -> bool {
    let (as_, ae, bs, be) = (a.start, a.end, b.start, b.end);
    let lt = ae <= bs;
    let gt = as_ >= be;
    match op {
        CmpOp::Lt => lt,
        CmpOp::Gt => gt,
        CmpOp::Le => !gt,
        CmpOp::Ge => !lt,
        CmpOp::Eq => (as_ >= bs && ae <= be) || (bs >= as_ && be <= ae && b.prec == Precision::Instant),
        CmpOp::Ne => !((as_ >= bs && ae <= be) || (bs >= as_ && be <= ae && b.prec == Precision::Instant)),
    }
}

fn ord_ok(op: CmpOp, o: Ordering) -> bool {
    match op {
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::Ne => o != Ordering::Equal,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Le => o != Ordering::Greater,
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Ge => o != Ordering::Less,
    }
}

fn is_textual(v: &Value) -> bool {
    matches!(v, Value::Text(_) | Value::Tag(_))
}

pub fn compare(ctx: &Ctx, op: CmpOp, a: &Value, b: &Value, ea: &Expr, eb: &Expr) -> R<bool> {
    // dates
    if matches!(a, Value::Time(_)) || matches!(b, Value::Time(_)) {
        if let (Some(x), Some(y)) = (as_span(a), as_span(b)) {
            return Ok(cmp_time(op, &x, &y));
        }
        if let (Value::Time(x), Some(t)) = (a, b.as_text().and_then(|s| dates::parse_iso(s.trim()))) {
            return Ok(cmp_time(op, x, &t));
        }
        return Ok(op == CmpOp::Ne);
    }
    // groups compare by value
    if let Value::Group(g) = a {
        return compare(ctx, op, &g.value, b, ea, eb);
    }
    match (a, b) {
        (Value::Elem(x), Value::Elem(y)) => {
            let (kx, ky) = (x.order(), y.order());
            let same_kind_id = |a: &Elem, b: &Elem| match (&a.extra, &b.extra) {
                (Extra::User(_), Extra::User(_)) | (Extra::Changeset(_), Extra::Changeset(_)) => a.id == b.id,
                _ => kx == ky,
            };
            return Ok(match op {
                CmpOp::Eq => same_kind_id(x, y),
                CmpOp::Ne => !same_kind_id(x, y),
                _ => ord_ok(op, kx.cmp(&ky)),
            });
        }
        (Value::Bool(x), Value::Bool(y)) => return Ok(ord_ok(op, x.cmp(y))),
        _ => {}
    }
    let numeric = matches!(a, Value::Num(_)) || matches!(b, Value::Num(_));
    if numeric {
        let want_a = if let Value::Num(n) = b { Some(n.dim) } else { None };
        let want_b = if let Value::Num(n) = a { Some(n.dim) } else { None };
        // equality with a text that is no number is simply false
        if matches!(op, CmpOp::Eq | CmpOp::Ne) {
            let pa = if is_textual(a) { a.as_text().and_then(units::parse_value) } else { Some((0.0, None)) };
            let pb = if is_textual(b) { b.as_text().and_then(units::parse_value) } else { Some((0.0, None)) };
            if pa.is_none() || pb.is_none() {
                return Ok(op == CmpOp::Ne);
            }
        }
        let x = ctx.to_num(a, want_a, ea.span)?;
        let y = ctx.to_num(b, want_b, eb.span)?;
        let (Some(x), Some(y)) = (x, y) else { return Ok(false) };
        if x.dim != y.dim {
            if x.dim.is_none() || y.dim.is_none() {
                // plain number against a quantity: read it in the quantity's base unit
            } else {
                return Err(RtErr::fatal(
                    Diagnostic::error("E303", ea.span.to(eb.span), Text2::new("This unit does not fit here.", "Diese Einheit passt hier nicht."))
                        .why(
                            format!("{} cannot be compared with {}.", cap(&x.dim.a_name()), y.dim.a_name()),
                            format!("{} lässt sich nicht mit {} vergleichen.", cap(&x.dim.name(true)), y.dim.name(true)),
                        ),
                ));
            }
        }
        let eps = 1e-9 * x.v.abs().max(y.v.abs()).max(1.0);
        let o = if (x.v - y.v).abs() <= eps { Ordering::Equal } else { x.v.total_cmp(&y.v) };
        return Ok(ord_ok(op, o));
    }
    if let (Some(x), Some(y)) = (a.as_text(), b.as_text()) {
        if matches!(op, CmpOp::Eq | CmpOp::Ne) {
            return Ok((x == y) == (op == CmpOp::Eq));
        }
        if let (Some((p, None)), Some((q, None))) = (units::parse_value(x), units::parse_value(y)) {
            return Ok(ord_ok(op, p.total_cmp(&q)));
        }
        return Ok(ord_ok(op, x.cmp(y)));
    }
    let o = a.key().cmp(&b.key());
    Ok(match op {
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::Ne => o != Ordering::Equal,
        _ => ord_ok(op, o),
    })
}

/// Equality without errors (for `in [...]`).
pub fn equal(ctx: &Ctx, a: &Value, b: &Value, ea: &Expr, eb: &Expr) -> R<bool> {
    match compare(ctx, CmpOp::Eq, a, b, ea, eb) {
        Ok(x) => Ok(x),
        Err(e) if e.catchable => Ok(false),
        Err(e) => Err(e),
    }
}

/// Sorting key for values in tables (numbers numerically).
pub fn sort_key(v: &Value) -> Key {
    v.key()
}

pub fn list(v: Vec<Value>) -> Value {
    Value::List(Arc::new(v))
}
