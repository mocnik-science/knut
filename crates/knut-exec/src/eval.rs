//! The interpreter (KNUT_LANG.md §5).

use crate::db::Db;
use crate::geom::{self, BBoxF, Geom};
use crate::region::Region;
use crate::scan::{KindMask, ScanSpec, TimeMode};
use crate::set::{Hints, Query, Seq, SetSrc, SetV};
use crate::value::*;
use knut_format::ObjType;
use knut_lang::ast::*;
use knut_lang::dates::{self, DateVal, Precision};
use knut_lang::diag::{Diagnostic, Header, Span, Text2};
use knut_lang::units::{self, Dim};
use knut_plan::pushdown::{self, TagPred};
use knut_plan::{Checked, IdentRes};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub struct Scope {
    pub name: String,
    pub val: Value,
    pub parent: Sc,
    /// The current element of an iteration (for `nearest of S`, implicit fields).
    pub implicit: bool,
}

pub type Sc = Option<Arc<Scope>>;

pub struct Ctx {
    pub chk: &'static Checked,
    db: Option<Arc<Db>>,
    db_err: Option<Diagnostic>,
    pub base_dir: PathBuf,
    globals: HashMap<String, OnceLock<R<Value>>>,
    pub w001: Vec<AtomicU64>,
    w001_idx: HashMap<Span, usize>,
    pub caches: crate::cache::Caches,
    pub control: Option<Arc<crate::control::Control>>,
}

pub fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

impl Ctx {
    pub fn new(chk: &'static Checked, db: Option<Arc<Db>>, db_err: Option<Diagnostic>, base_dir: PathBuf) -> Ctx {
        let mut globals = HashMap::new();
        for name in chk.globals.keys() {
            globals.insert(name.clone(), OnceLock::new());
        }
        let w001_idx = chk.w001.iter().enumerate().map(|(i, s)| (s.span, i)).collect();
        Ctx {
            chk,
            db,
            db_err,
            base_dir,
            globals,
            w001: chk.w001.iter().map(|_| AtomicU64::new(0)).collect(),
            w001_idx,
            caches: Default::default(),
            control: None,
        }
    }

    /// A comparison. A value that is not a number where numbers are compared
    /// (`maxspeed=none`) counts as missing: the comparison is false, without a message.
    fn compare_or_false(&self, _span: Span, op: CmpOp, a: &Value, b: &Value, ea: &'static Expr, eb: &'static Expr) -> R<bool> {
        match crate::arith::compare(self, op, a, b, ea, eb) {
            Err(err) if err.diag.code == "E602" => Ok(false),
            r => r,
        }
    }

    /// Prepares what conditions evaluated for every element need and that does not depend
    /// on the element: the sets after `near … in` (with their spatial index) and regions
    /// after `within`. Errors are left to the real evaluation.
    pub(crate) fn warm_up(&self, preds: &[&'static Expr], sc: &Sc) {
        for p in preds {
            walk_static(p, &mut |x| {
                let target: Option<&'static Expr> = match &x.kind {
                    ExprKind::Rel(_, RelTail::Near { set, .. }) => Some(set),
                    // (not a selection with `to`: it is searched around each place first)
                    ExprKind::Of { func: OfFunc::Nearest, operand: OfOperand::Expr(set), clauses, .. }
                        if clauses.mode.is_none() && !(matches!(set.kind, ExprKind::Select(_)) && clauses.to.is_some()) =>
                    {
                        Some(set)
                    }
                    ExprKind::Rel(_, RelTail::Within(r)) | ExprKind::Rel(_, RelTail::Overlapping(r)) | ExprKind::Rel(_, RelTail::In(r)) => Some(r),
                    _ => None,
                };
                let Some(t) = target else { return };
                if !self.closed(t) {
                    return;
                }
                let v = match &t.kind {
                    ExprKind::Select(_) | ExprKind::Of { .. } => self.once(t, || self.eval(t, sc)),
                    _ => self.eval(t, sc),
                };
                if let Ok(Value::Set(s)) = v
                    && matches!(&x.kind, ExprKind::Rel(_, RelTail::Near { .. }) | ExprKind::Of { .. })
                {
                    let _ = crate::spatial::geo_index(self, &s, t.span);
                }
            });
        }
    }

    /// The value of a closed sub-expression, computed once.
    fn once(&self, e: &'static Expr, f: impl FnOnce() -> R<Value>) -> R<Value> {
        if let Some(v) = lock(&self.caches.consts).get(&e.span) {
            return Ok(v.clone());
        }
        let v = f()?;
        lock(&self.caches.consts).insert(e.span, v.clone());
        Ok(v)
    }

    /// True if `e` uses no variable bound outside of it (only globals and its own variables).
    pub(crate) fn closed(&self, e: &'static Expr) -> bool {
        if let Some(&c) = lock(&self.caches.closed).get(&e.span) {
            return c;
        }
        let mut bound: Vec<String> = Vec::new();
        let mut has_near = false;
        e.walk(&mut |x| match &x.kind {
            ExprKind::Select(s) => bound.extend(s.pattern.iter().map(|p| p.var.clone())),
            ExprKind::Lambda(ps, _) => bound.extend(ps.iter().cloned()),
            ExprKind::Quantified { var, .. } => bound.push(var.clone()),
            ExprKind::Of { operand, .. } => {
                if let OfOperand::Iter { var, .. } = operand {
                    bound.push(var.clone());
                }
                if let Some(vs) = self.chk.clause_vars.get(&x.span) {
                    bound.extend(vs.iter().cloned());
                }
            }
            ExprKind::Special(sp) => match &**sp {
                Special::Path { var, .. } | Special::Isochrone { var, .. } => bound.push(var.clone()),
                _ => {}
            },
            ExprKind::Rel(_, RelTail::Near { .. }) => has_near = true,
            _ => {}
        });
        let mut free = false;
        e.walk(&mut |x| match &x.kind {
            ExprKind::Ident(n) => match self.chk.idents.get(&x.span) {
                Some(IdentRes::Local(_)) => free |= !bound.iter().any(|b| b == n),
                Some(IdentRes::ImplicitField(_)) | Some(IdentRes::Projection { .. }) => free = true,
                _ => {}
            },
            ExprKind::Underscore => free |= !has_near,
            _ => {}
        });
        lock(&self.caches.closed).insert(e.span, !free);
        !free
    }

    /// E902 if the run was cancelled.
    pub fn check_cancel(&self) -> R<()> {
        match &self.control {
            Some(c) if c.is_cancelled() => Err(cancelled()),
            _ => Ok(()),
        }
    }

    pub fn db(&self) -> R<&Db> {
        match &self.db {
            Some(d) => Ok(d),
            None => Err(RtErr::fatal(self.db_err.clone().unwrap_or_else(|| {
                Diagnostic::error(
                    "E205",
                    Span::default(),
                    Text2::new("No database given.", "Keine Datenbank angegeben."),
                )
                .why("Add a line like this at the top of the script, or use --db:", "Ergänze oben im Skript eine Zeile wie diese oder nutze --db:")
                .sugg("knut_db = 'file.knutdb'")
            }))),
        }
    }

    pub fn db_arc(&self) -> Option<&Arc<Db>> {
        self.db.as_ref()
    }

    pub fn latest(&self) -> i64 {
        self.db.as_ref().map_or(i64::MAX / 4, |d| d.latest)
    }

    pub fn global(&self, name: &str) -> R<Value> {
        let Some(cell) = self.globals.get(name) else {
            return Err(RtErr::internal(Span::default(), format!("unknown variable {name}")));
        };
        cell.get_or_init(|| {
            let g = &self.chk.globals[name];
            let StmtKind::Assign { value, .. } = &self.chk.script.stmts[g.stmt].kind else { unreachable!() };
            self.eval(value, &None)
        })
        .clone()
    }

    pub fn bind(&self, sc: &Sc, name: &str, v: Value, implicit: bool) -> Sc {
        Some(Arc::new(Scope { name: name.to_string(), val: v, parent: sc.clone(), implicit }))
    }

    /// Binds the element variables of an iteration (tuples bind one name per position).
    pub fn bind_elem(&self, sc: &Sc, vars: &[String], v: &Value) -> Sc {
        let mut s = sc.clone();
        if let Value::Tuple(t) = v {
            for (n, e) in t.names.iter().zip(&t.items) {
                s = self.bind(&s, n, Value::Elem(e.clone()), false);
            }
            for n in vars {
                if !t.names.contains(n) {
                    s = self.bind(&s, n, v.clone(), false);
                }
            }
            return Some(Arc::new(Scope { name: "\u{0}".into(), val: v.clone(), parent: s, implicit: true }));
        }
        if vars.is_empty() {
            return self.bind(&s, "_", v.clone(), true);
        }
        for (i, n) in vars.iter().enumerate() {
            s = self.bind(&s, n, v.clone(), i + 1 == vars.len());
        }
        s
    }

    fn lookup(&self, sc: &Sc, name: &str) -> Option<Value> {
        let mut cur = sc.as_ref();
        while let Some(s) = cur {
            if s.name == name {
                return Some(s.val.clone());
            }
            cur = s.parent.as_ref();
        }
        None
    }

    pub fn implicit(&self, sc: &Sc) -> Option<Value> {
        let mut cur = sc.as_ref();
        while let Some(s) = cur {
            if s.implicit {
                return Some(s.val.clone());
            }
            cur = s.parent.as_ref();
        }
        None
    }

    fn w001_hit(&self, span: Span) {
        if let Some(&i) = self.w001_idx.get(&span) {
            self.w001[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn text(&self, s: Span) -> &str {
        &self.chk.src[s.start as usize..(s.end as usize).min(self.chk.src.len())]
    }

    pub fn de(&self) -> bool {
        self.chk.settings.lang == knut_lang::diag::Lang::De
    }

    // ----- truth ---------------------------------------------------------------------------

    pub fn truthy(&self, e: &'static Expr, sc: &Sc) -> R<bool> {
        let v = self.eval(e, sc)?;
        self.truth(&v, e.span)
    }

    pub fn truth(&self, v: &Value, span: Span) -> R<bool> {
        Ok(match v {
            Value::Bool(b) => *b,
            Value::None => false,
            Value::Text(_) | Value::Tag(_) | Value::Elem(_) | Value::Tuple(_) | Value::Time(_) => true,
            other => {
                return Err(RtErr::fatal(Diagnostic::error(
                    "E605",
                    span,
                    Text2::new(
                        format!("A condition must be true or false, but this is {}.", other.type_name(false)),
                        format!("Eine Bedingung muss wahr oder falsch sein, das hier ist aber {}.", other.type_name(true)),
                    ),
                )));
            }
        })
    }

    // ----- evaluation ------------------------------------------------------------------------

    pub fn eval(&self, e: &'static Expr, sc: &Sc) -> R<Value> {
        match &e.kind {
            ExprKind::Num(n) => Ok(Value::num(*n)),
            ExprKind::Str(s) => Ok(Value::text(s)),
            ExprKind::Date(d) => Ok(Value::Time(*d)),
            ExprKind::None => Ok(Value::None),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Ident(n) => self.ident(e, n, sc),
            ExprKind::Underscore => self.lookup(sc, "_").or_else(|| self.implicit(sc)).ok_or_else(|| RtErr::internal(e.span, "\"_\" has no value here")),
            ExprKind::List(xs) => Ok(Value::List(Arc::new(xs.iter().map(|x| self.eval(x, sc)).collect::<R<Vec<_>>>()?))),
            ExprKind::BoxLit(b) => Ok(Value::Region(Arc::new(Region::Box(geom::BBoxF { minx: b[0], maxx: b[1], miny: b[2], maxy: b[3] })))),
            ExprKind::Point([lon, lat]) => Ok(Value::Region(Arc::new(Region::from_geom(Arc::new(geom::Geom::Point((*lon, *lat))), self.text(e.span).to_string())))),
            ExprKind::Unit(inner, u, _) => match &inner.kind {
                ExprKind::Num(x) => Ok(Value::Num(Num::with_unit(*x, u))),
                _ => {
                    let v = self.eval(inner, sc)?;
                    self.with_unit(v, u, inner.span)
                }
            },
            ExprKind::Field(b, f, fs) => {
                let v = self.eval(b, sc)?;
                self.field(&v, f, *fs)
            }
            ExprKind::Tag(b, k, val) => {
                let base = self.eval(b, sc)?;
                let key = match k {
                    TagKey::Lit(s) => s.clone(),
                    TagKey::Expr(x) => match self.eval(x, sc)? {
                        Value::None => return Ok(Value::None),
                        v => v.as_text().map(|s| s.to_string()).unwrap_or_else(|| v.display()),
                    },
                };
                self.tag(&base, &key, val.as_deref(), e.span)
            }
            ExprKind::Call(f, args) => self.call(f, args, sc, e.span),
            ExprKind::Neg(x) => match self.eval(x, sc)? {
                Value::None => Ok(Value::None),
                v => {
                    let mut n = self.to_num(&v, None, x.span)?.unwrap();
                    n.v = -n.v;
                    Ok(Value::Num(n))
                }
            },
            ExprKind::Not(x) => Ok(Value::Bool(!self.truthy(x, sc)?)),
            ExprKind::Bin(op, a, b) => {
                let va = self.eval(a, sc)?;
                let vb = self.eval(b, sc)?;
                // a value that is not a number counts as missing
                match crate::arith::binop(self, *op, va, vb, a, b, e) {
                    Err(err) if err.diag.code == "E602" => Ok(Value::None),
                    r => r,
                }
            }
            ExprKind::And(a, b) => Ok(Value::Bool(self.truthy(a, sc)? && self.truthy(b, sc)?)),
            ExprKind::Or(a, b) => Ok(Value::Bool(self.truthy(a, sc)? || self.truthy(b, sc)?)),
            ExprKind::Fallback(a, b) => match self.eval(a, sc) {
                Ok(Value::None) => self.eval(b, sc),
                // `w:maxspeed ?? 0 km/h`: with a number as fallback the tag is read as a
                // number here, so that values like "none" or "walk" fall back too
                Ok(v @ (Value::Tag(_) | Value::Text(_))) => {
                    let fb = self.eval(b, sc)?;
                    let Value::Num(n) = &fb else { return Ok(v) };
                    match self.to_num(&v, Some(n.dim), a.span) {
                        Ok(Some(x)) => Ok(Value::Num(x)),
                        Ok(None) => Ok(fb),
                        Err(err) if err.catchable => Ok(fb),
                        Err(err) => Err(err),
                    }
                }
                Ok(v) => Ok(v),
                Err(err) if err.catchable => self.eval(b, sc),
                Err(err) => Err(err),
            },
            ExprKind::Rel(l, t) => self.rel(e, l, t, sc),
            ExprKind::Lambda(ps, body) => Ok(Value::Lambda(Arc::new(Closure { params: ps.clone(), body, scope: sc.clone() }))),
            // inside a condition or a function, a selection that does not use the element
            // is the same for every element: evaluate it once
            ExprKind::Select(s) if sc.is_some() && self.closed(e) => self.once(e, || self.select(e, s, sc)),
            ExprKind::Select(s) => self.select(e, s, sc),
            ExprKind::Of { func, func_span, operand, clauses } if sc.is_some() && self.closed(e) => {
                self.once(e, || crate::agg::of_call(self, e, *func, *func_span, operand, clauses, sc))
            }
            ExprKind::Quantified { q, kind, var, set, pred, cond, .. } => self.quantified(e, *q, *kind, var, set, pred.as_deref(), cond.as_deref(), sc),
            ExprKind::Of { func, func_span, operand, clauses } => crate::agg::of_call(self, e, *func, *func_span, operand, clauses, sc),
            ExprKind::Special(s) => crate::special::eval(self, e, s, sc),
        }
    }

    fn ident(&self, e: &'static Expr, n: &str, sc: &Sc) -> R<Value> {
        match self.chk.idents.get(&e.span) {
            Some(IdentRes::Local(_)) => self.lookup(sc, n).ok_or_else(|| RtErr::internal(e.span, format!("\"{n}\" has no value here"))),
            Some(IdentRes::Global(g)) => self.global(g),
            Some(IdentRes::Text(t)) => Ok(Value::text(t)),
            Some(IdentRes::Region(name)) => crate::places::region_value(self, name, e.span),
            Some(IdentRes::AllOfKind(k)) => self.all_of_kind(*k, e.span),
            Some(IdentRes::ImplicitField(f)) => match self.implicit(sc) {
                Some(v) => self.field(&v, f, e.span),
                None => Err(RtErr::internal(e.span, format!("\"{n}\" has no value here"))),
            },
            Some(IdentRes::Projection { var, pos }) => {
                let base = match self.lookup(sc, var) {
                    Some(v) => v,
                    None => self.global(var)?,
                };
                self.project(&base, *pos)
            }
            Some(IdentRes::Setting(s)) => Ok(match s.as_str() {
                "seed" => Value::num(self.chk.settings.seed as f64),
                "near_distance" => Value::Num(Num::base(self.chk.settings.near_distance, Dim::LENGTH)),
                "knut_db" => self.chk.settings.knut_db.as_ref().map_or(Value::None, |(p, _)| Value::text(p)),
                "messages" => Value::text(if self.de() { "de" } else { "en" }),
                _ => Value::Bool(self.chk.settings.ignore_warnings),
            }),
            None => self.lookup(sc, n).ok_or_else(|| RtErr::internal(e.span, format!("\"{n}\" is not defined"))),
        }
    }

    fn project(&self, base: &Value, pos: usize) -> R<Value> {
        if let Value::Tuple(t) = base {
            return Ok(t.items.get(pos).map_or(Value::None, |e| Value::Elem(e.clone())));
        }
        let Value::Set(s) = base else { return Ok(Value::None) };
        let parts = s.fold(self, Hints::default(), &Vec::new, &|acc: &mut Vec<Value>, v: Value| {
            if let Value::Tuple(t) = v
                && let Some(e) = t.items.get(pos)
            {
                acc.push(Value::Elem(e.clone()));
            }
            Ok(())
        })?;
        Ok(Value::Set(Arc::new(SetV::from_vec(parts.into_iter().flatten().collect()))))
    }

    pub fn all_of_kind(&self, k: Kind, span: Span) -> R<Value> {
        match k {
            Kind::Changeset => crate::cs::all_changesets(self, span),
            Kind::User => crate::cs::all_users(self, span),
            Kind::Note => crate::notes::all_notes(self, span),
            Kind::Key => crate::special::all_keys(self, self.latest(), span),
            _ => {
                let spec = ScanSpec {
                    bbox: None,
                    kinds: KindMask::of(k),
                    time: TimeMode::State(self.latest()),
                    region: None,
                    pred: TagPred::True,
                    need_geom: false,
                    need_hist: false,
                    attrs_history: false,
                };
                let q = Query { spec, vars: vec!["_".into()], preds: vec![], scope: None, seq: None };
                Ok(Value::Set(Arc::new(SetV::lazy(SetSrc::Query(Arc::new(q))))))
            }
        }
    }

    pub fn with_unit(&self, v: Value, u: &'static units::Unit, span: Span) -> R<Value> {
        match v {
            Value::None => Ok(Value::None),
            Value::Num(n) if n.dim.is_none() => Ok(Value::Num(Num::with_unit(n.v, u))),
            Value::Num(n) => Ok(Value::Num(n)),
            Value::Tag(_) | Value::Text(_) => {
                let txt = v.as_text().unwrap().to_string();
                match units::parse_value(&txt) {
                    Some((x, Some(own))) => {
                        if own.dim != u.dim {
                            return Err(self.e602(&v, span));
                        }
                        Ok(Value::Num(Num::with_unit(x, own)))
                    }
                    Some((x, None)) => Ok(Value::Num(Num::with_unit(x, u))),
                    None => Err(self.e602(&v, span)),
                }
            }
            other => Err(RtErr::fatal(Diagnostic::error(
                "E602",
                span,
                Text2::new(format!("{} cannot carry a unit.", cap(other.type_name(false))), format!("{} kann keine Einheit tragen.", cap(other.type_name(true)))),
            ))),
        }
    }

    pub fn e602(&self, v: &Value, span: Span) -> RtErr {
        self.e602_in(v, span, None)
    }

    /// E602; `want` is the measure expected by the other operand (for the fallback).
    fn e602_in(&self, v: &Value, span: Span, want: Option<Dim>) -> RtErr {
        let txt = v.as_text().unwrap_or("").to_string();
        let mut d = Diagnostic::error(
            "E602",
            span,
            Text2::new(format!("\"{txt}\" cannot be read as a number."), format!("\"{txt}\" lässt sich nicht als Zahl lesen.")),
        );
        if let Value::Tag(t) = v {
            let kind = t.owner.0;
            d = d.why(
                format!("A number is expected here, but {} {} has {}=\"{txt}\".", kind.name(), t.owner.1, t.key),
                format!("Hier wird eine Zahl erwartet, aber {} {} hat {}=\"{txt}\".", kind.name(), t.owner.1, t.key),
            );
            let code = self.text(span).to_string();
            d = d
                .header(Header::YouCan)
                .sugg_label(
                    "provide a fallback:",
                    "einen Ersatzwert angeben:",
                    match want.filter(|d| !d.is_none()).and_then(units::display_unit) {
                        Some(u) => format!("({code} ?? 0 {})", u.name),
                        None => format!("{code} ?? 0"),
                    },
                )
                .sugg_label(
                    &format!("skip such {}:", kind.plural()),
                    &format!("solche {} auslassen:", kind.plural()),
                    format!("{code} != '{txt}' and …"),
                );
        }
        RtErr::data(d)
    }

    /// Converts a value to a number. Tag texts are parsed; `want` is the dimension
    /// expected by the other operand (a missing unit is then E304).
    pub fn to_num(&self, v: &Value, want: Option<Dim>, span: Span) -> R<Option<Num>> {
        match v {
            Value::None => Ok(None),
            Value::Num(n) => Ok(Some(*n)),
            Value::Bool(b) => Ok(Some(Num::plain(if *b { 1.0 } else { 0.0 }))),
            Value::Tag(_) | Value::Text(_) => {
                let txt = v.as_text().unwrap();
                match units::parse_value(txt) {
                    Some((x, Some(u))) => Ok(Some(Num::with_unit(x, u))),
                    Some((x, None)) => match want {
                        Some(d) if !d.is_none() => {
                            // documented default unit of the key (maxspeed → km/h, height → m)
                            match v {
                                Value::Tag(t) if let Some(u) = units::default_unit(&t.key).filter(|u| u.dim == d) => Ok(Some(Num::with_unit(x, u))),
                                _ => Err(self.e304(v, span, d)),
                            }
                        }
                        _ => Ok(Some(Num::plain(x))),
                    },
                    None => Err(self.e602_in(v, span, want)),
                }
            }
            Value::Group(g) => self.to_num(&g.value, want, span),
            Value::Time(d) => Ok(Some(Num::plain(d.start as f64))),
            other => Err(RtErr::fatal(Diagnostic::error(
                "E602",
                span,
                Text2::new(
                    format!("A number is expected here, but this is {}.", other.type_name(false)),
                    format!("Hier wird eine Zahl erwartet, das ist aber {}.", other.type_name(true)),
                ),
            ))),
        }
    }

    fn e304(&self, v: &Value, span: Span, want: Dim) -> RtErr {
        let txt = v.as_text().unwrap_or("").to_string();
        let mut d = Diagnostic::error("E304", span, Text2::new("This value needs a unit.", "Dieser Wert braucht eine Einheit."));
        if let Value::Tag(t) = v {
            let k = t.owner.0;
            d = d.why(
                format!("{} {} has {}=\"{txt}\" without a unit.", cap(k.name()), t.owner.1, t.key),
                format!("{} {} hat {}=\"{txt}\" ohne Einheit.", cap(k.name()), t.owner.1, t.key),
            );
            let unit = units::display_unit(want).map_or("m", |u| u.name);
            let code = self.text(span).to_string();
            d = d
                .header(Header::YouCan)
                .sugg_label("state the unit for plain numbers:", "die Einheit für einfache Zahlen angeben:", format!("{code} {unit}"));
        }
        RtErr::data(d)
    }

    // ----- tags and fields -----------------------------------------------------------------

    fn tag(&self, base: &Value, key: &str, want: Option<&str>, span: Span) -> R<Value> {
        match base {
            Value::None => Ok(if want.is_some() { Value::Bool(false) } else { Value::None }),
            Value::Elem(e) => {
                let v = match &e.extra {
                    Extra::Changeset(c) => c.tags.iter().find(|(k, _)| k.as_ref() == key).map(|(_, v)| v.clone()),
                    _ => e.tag(key).cloned(),
                };
                Ok(match (v, want) {
                    (Some(v), Some(w)) => Value::Bool(v.as_ref() == w),
                    (None, Some(_)) => Value::Bool(false),
                    (Some(v), None) => Value::Tag(Arc::new(TagVal { text: v, key: Arc::from(key), owner: (e.kind, e.id) })),
                    (None, None) => Value::None,
                })
            }
            Value::Group(g) => self.tag(&g.key, key, want, span),
            Value::Set(_) | Value::List(_) => {
                // subset of the elements with the key (and value)
                let items = self.items_of(base, Hints::default(), span)?;
                let out: Vec<Value> = items
                    .iter()
                    .filter(|v| match v {
                        Value::Elem(e) => e.tag(key).is_some_and(|x| want.is_none_or(|w| x.as_ref() == w)),
                        _ => false,
                    })
                    .cloned()
                    .collect();
                Ok(match base {
                    Value::List(_) => Value::List(Arc::new(out)),
                    _ => Value::Set(Arc::new(SetV::from_vec(out))),
                })
            }
            Value::Record(r) => Ok(r.iter().find(|(k, _)| k == key).map_or(Value::None, |(_, v)| v.clone())),
            other => Err(RtErr::fatal(Diagnostic::error(
                "E602",
                span,
                Text2::new(
                    format!("Only elements have tags, but this is {}.", other.type_name(false)),
                    format!("Nur Elemente haben Tags, das ist aber {}.", other.type_name(true)),
                ),
            ))),
        }
    }

    pub fn field(&self, v: &Value, f: &str, span: Span) -> R<Value> {
        crate::fields::field(self, v, f, span)
    }

    /// Items of a collection value (materialized).
    pub fn items_of(&self, v: &Value, hints: Hints, span: Span) -> R<Arc<Vec<Value>>> {
        match v {
            Value::Set(s) => s.items(self, hints),
            Value::List(l) => Ok(l.clone()),
            Value::Grouped(g) => Ok(Arc::new(g.rows.iter().map(|r| Value::Group(Arc::new(r.clone()))).collect())),
            Value::None => Ok(Arc::new(Vec::new())),
            other => Err(RtErr::fatal(Diagnostic::error(
                "E601",
                span,
                Text2::new(
                    format!("A set is expected here, but this is {}.", other.type_name(false)),
                    format!("Hier wird eine Menge erwartet, das ist aber {}.", other.type_name(true)),
                ),
            ))),
        }
    }

    pub fn as_set(&self, v: &Value, span: Span) -> R<Arc<SetV>> {
        match v {
            Value::Set(s) => Ok(s.clone()),
            Value::List(l) => Ok(Arc::new(SetV::from_list(l.clone()))),
            Value::Grouped(_) | Value::None => Ok(Arc::new(SetV::from_vec((*self.items_of(v, Hints::default(), span)?).clone()))),
            other => Err(RtErr::fatal(Diagnostic::error(
                "E601",
                span,
                Text2::new(
                    format!("A set is expected here, but this is {}.", other.type_name(false)),
                    format!("Hier wird eine Menge erwartet, das ist aber {}.", other.type_name(true)),
                ),
            ))),
        }
    }

    pub fn apply(&self, f: &Value, args: Vec<Value>, span: Span) -> R<Value> {
        let Value::Lambda(c) = f else {
            return Err(RtErr::fatal(Diagnostic::error(
                "E606",
                span,
                Text2::new("A function (x => …) is expected here.", "Hier wird eine Funktion (x => …) erwartet."),
            )));
        };
        let mut sc = c.scope.clone();
        for (i, p) in c.params.iter().enumerate() {
            sc = self.bind(&sc, p, args.get(i).cloned().unwrap_or(Value::None), i == 0);
        }
        self.eval(c.body, &sc)
    }

    fn call(&self, f: &'static Expr, args: &'static [Expr], sc: &Sc, span: Span) -> R<Value> {
        let ExprKind::Field(base, name, _) = &f.kind else {
            return Err(RtErr::fatal(Diagnostic::error(
                "E606",
                f.span,
                Text2::new("Only .map(…) and .filter(…) can be called.", "Nur .map(…) und .filter(…) lassen sich aufrufen."),
            )));
        };
        let b = self.eval(base, sc)?;
        let fv = match args.first() {
            Some(a) => self.eval(a, sc)?,
            None => Value::None,
        };
        match name.as_str() {
            "map" | "filter" => {
                let is_map = name == "map";
                if let Value::Grouped(g) = &b {
                    let mut rows = Vec::new();
                    for r in &g.rows {
                        if is_map {
                            let v = self.apply(&fv, vec![Value::Group(Arc::new(r.clone()))], span)?;
                            rows.push(Group { key: r.key.clone(), value: v, count: r.count, items: None });
                        } else if self.truth(&self.apply(&fv, vec![Value::Group(Arc::new(r.clone()))], span)?, span)? {
                            rows.push(r.clone());
                        }
                    }
                    return Ok(Value::Grouped(Arc::new(Grouped { key_names: g.key_names.clone(), value_name: g.value_name.clone(), rows })));
                }
                let items = self.items_of(&b, Hints { geom: true, ..Default::default() }, base.span)?;
                if args.first().is_some_and(uses_geom) {
                    self.preload_geoms(&items);
                }
                // closed sets inside the function (near, nearest): built once, not by each thread
                if let Some(a) = args.first() {
                    self.warm_up(&[a], sc);
                }
                let out: Vec<Value> = if is_map {
                    use rayon::prelude::*;
                    items.par_iter().map(|x| self.apply(&fv, vec![x.clone()], span)).collect::<R<Vec<_>>>()?
                } else {
                    let mut out = Vec::new();
                    for x in items.iter() {
                        if self.truth(&self.apply(&fv, vec![x.clone()], span)?, span)? {
                            out.push(x.clone());
                        }
                    }
                    out
                };
                Ok(match b {
                    Value::Set(_) if is_map => Value::List(Arc::new(out)),
                    Value::Set(_) => Value::Set(Arc::new(SetV::from_vec(out))),
                    _ => Value::List(Arc::new(out)),
                })
            }
            other => Err(RtErr::fatal(Diagnostic::error(
                "E606",
                f.span,
                Text2::new(format!("\".{other}(…)\" is not a function."), format!("\".{other}(…)\" ist keine Funktion.")),
            ))),
        }
    }

    // ----- relations ----------------------------------------------------------------------

    fn rel(&self, e: &'static Expr, l: &'static Expr, t: &'static RelTail, sc: &Sc) -> R<Value> {
        match t {
            RelTail::Cmp(op, r) => {
                // presence tests
                if matches!(r.kind, ExprKind::None) || matches!(l.kind, ExprKind::None) {
                    let x = if matches!(r.kind, ExprKind::None) { self.eval(l, sc)? } else { self.eval(r, sc)? };
                    let missing = x.is_none();
                    return Ok(Value::Bool(match op {
                        CmpOp::Eq => missing,
                        CmpOp::Ne => !missing,
                        _ => false,
                    }));
                }
                let a = self.eval(l, sc)?;
                let b = self.eval(r, sc)?;
                if a.is_none() || b.is_none() {
                    self.w001_hit(e.span);
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(self.compare_or_false(e.span, *op, &a, &b, l, r)?))
            }
            RelTail::In(r) => {
                let a = self.eval(l, sc)?;
                if a.is_none() {
                    return Ok(Value::Bool(false));
                }
                let b = self.eval(r, sc)?;
                let k = a.key();
                let hit = match &b {
                    Value::List(xs) => {
                        let mut hit = false;
                        for x in xs.iter() {
                            if crate::arith::equal(self, &a, x, l, r)? {
                                hit = true;
                                break;
                            }
                        }
                        hit
                    }
                    Value::Set(s) => s.items(self, Hints::default())?.binary_search_by(|x| x.key().cmp(&k)).is_ok(),
                    Value::Grouped(g) => g.rows.iter().any(|row| row.key.key() == k),
                    Value::Region(_) => return self.within(&a, &b, e.span).map(Value::Bool),
                    Value::Text(_) | Value::Tag(_) => {
                        let hay = b.as_text().unwrap();
                        a.as_text().is_some_and(|n| hay.split(';').any(|p| p.trim() == n))
                    }
                    _ => false,
                };
                Ok(Value::Bool(hit))
            }
            RelTail::Between(a, b) => {
                let x = self.eval(l, sc)?;
                let lo = self.eval(a, sc)?;
                let hi = self.eval(b, sc)?;
                if x.is_none() || lo.is_none() || hi.is_none() {
                    return Ok(Value::Bool(false));
                }
                // inclusive range; spans include their whole extent
                let lo_ok = self.compare_or_false(e.span, CmpOp::Ge, &x, &start_of(&lo), l, a)?;
                let hi_ok = match &hi {
                    Value::Time(d) if d.prec != Precision::Instant => self.compare_or_false(e.span, CmpOp::Lt, &x, &Value::instant(d.end), l, b)?,
                    _ => self.compare_or_false(e.span, CmpOp::Le, &x, &hi, l, b)?,
                };
                Ok(Value::Bool(lo_ok && hi_ok))
            }
            RelTail::Within(r) => {
                let x = self.eval(l, sc)?;
                let reg = self.eval(r, sc)?;
                Ok(Value::Bool(self.within(&x, &reg, r.span)?))
            }
            RelTail::Overlapping(r) => {
                let x = self.eval(l, sc)?;
                let y = self.eval(r, sc)?;
                let (Some(gx), Some(gy)) = (self.geom_of(&x, l.span)?, self.geom_of(&y, r.span)?) else { return Ok(Value::Bool(false)) };
                Ok(Value::Bool(geom::overlapping(&gx, &gy)))
            }
            RelTail::Near { quant, set, threshold } => crate::spatial::near(self, l, *quant, set, threshold.as_ref(), sc, e.span),
            RelTail::NextOf(r) | RelTail::PreviousOf(r) => {
                let b = self.eval(l, sc)?;
                let a = self.eval(r, sc)?;
                let (Value::Elem(b), Value::Elem(a)) = (b, a) else { return Ok(Value::Bool(false)) };
                if a.otype != b.otype || a.id != b.id {
                    return Ok(Value::Bool(false));
                }
                let next = matches!(t, RelTail::NextOf(_));
                let n = crate::fields::neighbour(self, &a, next);
                Ok(Value::Bool(n.is_some_and(|n| n.v.version == b.v.version)))
            }
            RelTail::Before(r) | RelTail::After(r) => {
                let d = self.eval(l, sc)?;
                let t0 = self.eval(r, sc)?;
                let (Value::Num(d), Value::Time(t0)) = (d, t0) else { return Ok(Value::None) };
                let before = matches!(t, RelTail::Before(_));
                let start = t0.start;
                let t = match d.unit.and_then(|u| u.calendar) {
                    Some(units::Calendar::Months(m)) => {
                        let n = (d.shown() * m as f64).round() as i64;
                        dates::add_months(start, if before { -n } else { n })
                    }
                    None => start + if before { -(d.v as i64) } else { d.v as i64 },
                };
                // whole days from a day (or coarser) stay a day: `30 days before 2015-03-01`
                if t0.prec != dates::Precision::Instant && (t - start) % 86_400 == 0 {
                    return Ok(Value::Time(dates::day_of(t)));
                }
                Ok(Value::instant(t))
            }
        }
    }

    /// Geometry of a value (elements, regions, paths).
    pub fn geom_of(&self, v: &Value, span: Span) -> R<Option<Arc<Geom>>> {
        Ok(match v {
            Value::Elem(e) => Some(self.elem_geom(e)),
            Value::Region(r) => Some(Arc::new(r.geom())),
            Value::Path(p) => Some(p.geom.clone()),
            Value::Group(g) => return self.geom_of(&g.key, span),
            Value::None => None,
            _ => None,
        })
    }

    /// Loads the geometry of many elements in batched lookups (for elements decoded without
    /// geometry): each block is decoded once instead of once per element.
    pub fn preload_geoms(&self, items: &[Value]) {
        let Some(db) = &self.db else { return };
        let mut nodes = Vec::new();
        let mut ways = Vec::new();
        let mut rels = Vec::new();
        for it in items {
            let Value::Elem(e) = it else { continue };
            if e.geom.get().is_some() || !e.v.visible {
                continue;
            }
            match e.otype {
                Some(ObjType::Node) if e.v.pos.is_none() => nodes.push(e.clone()),
                Some(ObjType::Way) => ways.push(e.clone()),
                Some(ObjType::Relation) => rels.push(e.clone()),
                _ => {}
            }
        }
        // in chunks, so that the member geometries of huge sets need not be held at once
        for chunk in rels.chunks(20_000) {
            let reqs: Vec<(bool, &[knut_format::Member], i64)> =
                chunk.iter().map(|e| (e.kind == EKind::Area, &e.v.members[..], e.at)).collect();
            for (e, g) in chunk.iter().zip(db.relation_geoms(&reqs)) {
                let _ = e.geom.set(Arc::new(g));
            }
        }
        if !nodes.is_empty() {
            let pos = db.positions(&nodes.iter().map(|e| (e.id, e.at)).collect::<Vec<_>>());
            for e in &nodes {
                let g = pos.get(&(e.id, e.at)).copied().flatten().map_or(Geom::Empty, |p| Geom::Point(geom::e7(p)));
                let _ = e.geom.set(Arc::new(g));
            }
        }
        if !ways.is_empty() {
            let gs = db.way_geoms(&ways.iter().map(|e| (e.id, e.at)).collect::<Vec<_>>());
            for e in &ways {
                let g = way_geom(e.kind, gs.get(&(e.id, e.at)).and_then(|x| x.as_ref()).map(|(_, p)| &p[..]).unwrap_or(&[]));
                let _ = e.geom.set(Arc::new(g));
            }
        }
    }

    pub fn elem_geom(&self, e: &Elem) -> Arc<Geom> {
        e.geom
            .get_or_init(|| {
                let Some(db) = &self.db else { return Arc::new(Geom::Empty) };
                if !e.v.visible {
                    return Arc::new(Geom::Empty);
                }
                Arc::new(match (e.kind, e.otype) {
                    (_, Some(ObjType::Node)) => {
                        // decoded without geometry: look the position up
                        let p = e.v.pos.or_else(|| db.positions(&[(e.id, e.at)]).get(&(e.id, e.at)).copied().flatten());
                        p.map_or(Geom::Empty, |p| Geom::Point(geom::e7(p)))
                    }
                    (k, Some(ObjType::Way)) => {
                        let g = db.way_geoms(&[(e.id, e.at)]);
                        way_geom(k, g.get(&(e.id, e.at)).and_then(|x| x.as_ref()).map(|(_, p)| &p[..]).unwrap_or(&[]))
                    }
                    (EKind::Area, Some(ObjType::Relation)) => db.area_geom(&e.v.members, e.at),
                    (_, Some(ObjType::Relation)) => db.relation_geom(&e.v.members, e.at),
                    _ => match &e.extra {
                        Extra::Changeset(c) => Geom::Point(c.bbox.center()),
                        Extra::Note(n) => Geom::Point((n.lon, n.lat)),
                        _ => Geom::Empty,
                    },
                })
            })
            .clone()
    }

    pub fn rep_point(&self, v: &Value) -> Option<geom::P> {
        match v {
            Value::Elem(e) => {
                if let Some(g) = e.geom.get()
                    && let Some(p) = g.rep_point()
                {
                    return Some(p);
                }
                match &e.extra {
                    Extra::Changeset(c) => return Some(c.bbox.center()),
                    Extra::Note(n) => return Some((n.lon, n.lat)),
                    _ => {}
                }
                if let Some(p) = e.v.pos {
                    return Some(geom::e7(p));
                }
                self.elem_geom(e).rep_point().or_else(|| e.bbox.map(|b| b.center()))
            }
            Value::Region(r) => r.geom().rep_point(),
            Value::Group(g) => self.rep_point(&g.key),
            _ => None,
        }
    }

    pub fn region_of(&self, v: &Value, span: Span) -> R<Arc<Region>> {
        Ok(match v {
            Value::Region(r) => r.clone(),
            Value::Elem(e) => {
                let g = self.elem_geom(e);
                Arc::new(Region::from_geom(g, e.label()))
            }
            Value::Set(_) | Value::List(_) => {
                let items = self.items_of(v, Hints { geom: true, ..Default::default() }, span)?;
                self.preload_geoms(&items);
                let mut rs = Vec::new();
                let mut bb = geom::BBoxF::EMPTY;
                for it in items.iter() {
                    let r = self.region_of(it, span)?;
                    bb.union(&r.bbox());
                    rs.push(r);
                }
                Arc::new(Region::Union(crate::region::Union::new(rs, bb, format!("{} areas", items.len()))))
            }
            Value::Text(t) => crate::places::region_value(self, t, span).and_then(|v| self.region_of(&v, span))?,
            Value::Group(g) => self.region_of(&g.key, span)?,
            other => {
                return Err(RtErr::fatal(Diagnostic::error(
                    "E108",
                    span,
                    Text2::new(
                        format!("A region is expected here, but this is {}.", other.type_name(false)),
                        format!("Hier wird eine Region erwartet, das ist aber {}.", other.type_name(true)),
                    ),
                )));
            }
        })
    }

    pub fn within(&self, x: &Value, reg: &Value, span: Span) -> R<bool> {
        if x.is_none() || reg.is_none() {
            return Ok(false);
        }
        let r = self.region_of(reg, span)?;
        Ok(self.rep_point(x).is_some_and(|p| r.contains(p)))
    }

    // ----- selections ----------------------------------------------------------------------

    fn time_of(&self, e: &'static Expr, sc: &Sc) -> R<DateVal> {
        match self.eval(e, sc)? {
            Value::Time(d) => Ok(d),
            Value::Num(n) if n.dim.is_none() && n.v.fract() == 0.0 && (1000.0..10000.0).contains(&n.v) => Ok(dates::year(n.v as i64)),
            other => Err(RtErr::fatal(Diagnostic::error(
                "E108",
                e.span,
                Text2::new(
                    format!("A date is expected here, but this is {}.", other.type_name(false)),
                    format!("Hier wird ein Datum erwartet, das ist aber {}.", other.type_name(true)),
                ),
            ))),
        }
    }

    pub fn time_mode(&self, t: &'static TimeRef, sc: &Sc) -> R<(TimeMode, Option<dates::TimeUnit>)> {
        Ok(match t {
            TimeRef::On(d) => (TimeMode::State(self.time_of(d, sc)?.start), None),
            TimeRef::Range(a, b) => {
                let (a, b) = (self.time_of(a, sc)?, self.time_of(b, sc)?);
                (TimeMode::Events(a.start, b.end), None)
            }
            TimeRef::OnEach { unit, range, .. } => {
                let (a, b) = range.as_ref().unwrap();
                let (a, b) = (self.time_of(a, sc)?, self.time_of(b, sc)?);
                let mut starts = Vec::new();
                let mut s = dates::bucket(a.start, *unit).start;
                while s < b.end && starts.len() < 100_000 {
                    starts.push(s);
                    s = dates::next_bucket(s, *unit);
                }
                (TimeMode::Series(Arc::new(starts)), Some(*unit))
            }
        })
    }

    fn select(&self, e: &'static Expr, s: &'static Selection, sc: &Sc) -> R<Value> {
        self.select_in(e, s, sc, None)
    }

    /// A selection scanned only where it can meet `bbox` (it may give more: the box only
    /// skips blocks and segments). `None` if `e` is not a selection.
    pub(crate) fn select_in_box(&self, e: &'static Expr, sc: &Sc, bbox: BBoxF) -> R<Option<Value>> {
        match &e.kind {
            ExprKind::Select(s) => self.select_in(e, s, sc, Some(bbox)).map(Some),
            _ => Ok(None),
        }
    }

    fn select_in(&self, e: &'static Expr, s: &'static Selection, sc: &Sc, extra: Option<BBoxF>) -> R<Value> {
        let vars: Vec<String> = s.pattern.iter().map(|p| p.var.clone()).collect();
        let preds: Vec<&'static Expr> = [s.var_pred.as_deref(), s.cond.as_deref()].into_iter().flatten().collect();
        let region = match &s.within {
            Some(w) => {
                let v = self.eval(w, sc)?;
                Some(self.region_of(&v, w.span)?)
            }
            None => None,
        };
        let set: Arc<SetV> = if let Some(src) = &s.source {
            let base_v = self.eval(src, sc)?;
            let base = self.as_set(&base_v, src.span)?;
            crate::agg::restrict(self, base, vars.clone(), preds, sc.clone(), region, s.time.as_ref(), e.span)?
        } else {
            let (time, series) = match &s.time {
                Some(t) => self.time_mode(t, sc)?,
                None if s.pattern.len() > 1 => (TimeMode::History, None),
                None => (TimeMode::State(self.latest()), None),
            };
            let _ = series;
            let mut pred = TagPred::True;
            let mut exact = true;
            let mut need_geom = false;
            let mut need_hist = false;
            let mut bbox = None;
            if s.pattern.len() == 1 {
                let var = &s.pattern[0].var;
                bbox = crate::spatial::scan_hint(self, &preds, var, sc)?;
                if let Some(x) = extra {
                    bbox = Some(match bbox {
                        Some(b) => BBoxF { minx: b.minx.max(x.minx), miny: b.miny.max(x.miny), maxx: b.maxx.min(x.maxx), maxy: b.maxy.min(x.maxy) },
                        None => x,
                    });
                }
                for p in &preds {
                    let x = pushdown::extract(p, var, self.chk);
                    pred = if pred.is_true() { x.pred } else { TagPred::And(vec![pred, x.pred]) };
                    exact &= x.exact;
                    need_geom |= pushdown::needs_geometry(p, var);
                    let f = pushdown::fields_used(p, var);
                    need_hist |= f.iter().any(|x| matches!(x.as_str(), "next" | "previous" | "unchangedBefore" | "unchangedAfter" | "versions"));
                }
            } else {
                // sequences: any version must satisfy one position's tag condition
                let mut alts = Vec::new();
                for v in &vars {
                    let mut pv = TagPred::True;
                    for p in &preds {
                        let x = pushdown::extract(p, v, self.chk);
                        pv = if pv.is_true() { x.pred } else { TagPred::And(vec![pv, x.pred]) };
                    }
                    alts.push(pv);
                }
                pred = if alts.iter().any(|a| a.is_true()) { TagPred::True } else { TagPred::Or(alts) };
                exact = false;
            }
            let kind = s.pattern[0].kind;
            let mut kinds = KindMask::of(kind);
            if kind == Kind::Key {
                // keys in use at a moment
                let t = match &time {
                    TimeMode::State(t) => *t,
                    _ => self.latest(),
                };
                let base_v = crate::special::all_keys(self, t, e.span)?;
                let base = self.as_set(&base_v, e.span)?;
                let set = crate::agg::restrict(self, base, vars.clone(), preds, sc.clone(), region, None, e.span)?;
                return self.finish_select(s, set, sc);
            }
            if !kind.is_osm() {
                // changesets, users, notes: filter over the full set
                let base_v = self.all_of_kind(kind, e.span)?;
                let base = self.as_set(&base_v, e.span)?;
                let set = crate::agg::restrict(self, base, vars.clone(), preds, sc.clone(), region, s.time.as_ref(), e.span)?;
                return self.finish_select(s, set, sc);
            }
            if s.pattern.len() > 1 {
                kinds = KindMask::of(kind);
            }
            let seq = (s.pattern.len() > 1).then(|| -> R<Seq> {
                let max_dt = match &s.seq_threshold {
                    Some(th) => {
                        let v = self.eval(&th.value, sc)?;
                        let n = self.to_num(&v, Some(Dim::TIME), th.value.span)?.map(|n| n.v);
                        n.map(|x| (x, th.or_equal))
                    }
                    None => None,
                };
                Ok(Seq {
                    kinds: s.pattern.iter().map(|p| EKind::from_kind(p.kind).unwrap_or(EKind::Way)).collect(),
                    max_dt,
                    names: vars.clone().into(),
                    differ: differing_tags(&preds, &vars),
                    attrs_only: preds.iter().all(|p| only_attributes(p, &vars)),
                    t_range: match &time {
                        TimeMode::Events(a, b) => Some((*a, *b)),
                        _ => None,
                    },
                })
            });
            let seq = match seq {
                Some(r) => Some(r?),
                None => None,
            };
            let time = if seq.is_some() { TimeMode::History } else { time };
            let spec = ScanSpec { kinds, time, region, bbox, pred, need_geom, need_hist, attrs_history: false };
            let preds = if exact && s.pattern.len() == 1 { Vec::new() } else { preds };
            let q = Query { spec, vars: vars.clone(), preds, scope: sc.clone(), seq };
            Arc::new(SetV::lazy(SetSrc::Query(Arc::new(q))))
        };
        self.finish_select(s, set, sc)
    }

    fn finish_select(&self, s: &'static Selection, set: Arc<SetV>, sc: &Sc) -> R<Value> {
        let Some(pick) = &s.pick else { return Ok(Value::Set(set)) };
        let n = pick.n.unwrap_or(1) as usize;
        let seed = if s.randomly { Some(s.seed.unwrap_or(self.chk.settings.seed)) } else { None };
        let _ = sc;
        let picked = pick_n(self, &set, n, seed, s.order.as_deref(), &s.pattern.iter().map(|p| p.var.clone()).collect::<Vec<_>>())?;
        if pick.exactly && picked.len() < n {
            let kind = s.pattern[0].kind;
            let kp = if n == 1 { kind.name() } else { kind.plural() };
            return Err(RtErr::data(
                Diagnostic::error(
                    "E404",
                    pick.span,
                    Text2::new(
                        format!("Asked for exactly {n} {kp}, but only {} exist.", picked.len()),
                        format!("Verlangt waren genau {n} {kp}, es gibt aber nur {}.", picked.len()),
                    ),
                )
                .why(
                    format!("Without \"exactly\", you would get the {} that exist.", picked.len()),
                    format!("Ohne \"exactly\" bekämst du die {} vorhandenen.", picked.len()),
                ),
            ));
        }
        if pick.n.is_none() && !s.pattern[0].plural {
            return Ok(picked.into_iter().next().unwrap_or(Value::None));
        }
        Ok(Value::List(Arc::new(picked)))
    }

    #[allow(clippy::too_many_arguments)]
    fn quantified(
        &self,
        e: &'static Expr,
        q: QKind,
        kind: Option<Kind>,
        var: &str,
        set: &'static Expr,
        pred: Option<&'static Expr>,
        cond: Option<&'static Expr>,
        sc: &Sc,
    ) -> R<Value> {
        let base_v = self.eval(set, sc)?;
        let base = self.as_set(&base_v, set.span)?;
        let preds: Vec<&'static Expr> = [pred, cond].into_iter().flatten().collect();
        let ek = kind.and_then(EKind::from_kind);
        let kind_filter: Option<Arc<dyn Fn(&Ctx, &Value) -> R<bool> + Send + Sync>> = None;
        let filtered = Arc::new(SetV::lazy(SetSrc::Filter { base: base.clone(), vars: vec![var.to_string()], preds: preds.clone(), scope: sc.clone(), extra: kind_filter }));
        match q {
            QKind::All => Ok(Value::Set(filtered)),
            QKind::Groups => Ok(Value::Set(filtered)),
            QKind::Some => {
                let found = AtomicU64::new(0);
                let r = filtered.fold(self, Hints::default(), &|| (), &|_, _| {
                    found.fetch_add(1, Ordering::Relaxed);
                    Err(RtErr::data(Diagnostic::error("STOP", Span::default(), Text2::same(""))))
                });
                match r {
                    Ok(_) => Ok(Value::Bool(found.load(Ordering::Relaxed) > 0)),
                    Err(err) if err.diag.code == "STOP" => Ok(Value::Bool(true)),
                    Err(err) => Err(err),
                }
            }
            QKind::Pick { n, exactly } => {
                let k = n.unwrap_or(1) as usize;
                let picked = pick_n(self, &filtered, k, None, None, &[var.to_string()])?;
                if exactly && picked.len() < k {
                    return Err(RtErr::data(Diagnostic::error(
                        "E404",
                        e.span,
                        Text2::new(format!("Asked for exactly {k}, but only {} exist.", picked.len()), format!("Verlangt waren genau {k}, es gibt aber nur {}.", picked.len())),
                    )));
                }
                if n.is_none() {
                    return Ok(picked.into_iter().next().unwrap_or(Value::None));
                }
                Ok(Value::List(Arc::new(picked)))
            }
            QKind::The => {
                let items = filtered.items(self, Hints::default())?;
                let all: Vec<Value> = items.iter().cloned().collect();
                let what = self.text(set.span).to_string();
                let cond_txt = cond.map(|c| self.text(c.span).to_string());
                let the_span = Span { start: e.span.start, end: e.span.start + 3 };
                if all.is_empty() {
                    let rest = &self.chk.src[set.span.end as usize..e.span.end as usize];
                    let full = self.text(e.span).to_string();
                    let mut d = Diagnostic::error(
                        "E401",
                        the_span,
                        Text2::new("\"the\" expects exactly one result, but found none.", "\"the\" erwartet genau ein Ergebnis, hat aber keines gefunden."),
                    );
                    d = d.why(
                        format!("Nothing in '{what}' matches{}.", rest.trim_end().strip_prefix(' ').map(|r| format!(" ({r})")).unwrap_or_default()),
                        format!("Nichts in '{what}' passt{}.", rest.trim_end().strip_prefix(' ').map(|r| format!(" ({r})")).unwrap_or_default()),
                    );
                    let pick_txt = full.replacen("the", "pick", 1);
                    d = d
                        .header(Header::YouCan)
                        .sugg_label("provide a fallback:", "einen Ersatzwert angeben:", format!("({full}) ?? none"))
                        .sugg_label("take any result if any:", "ein beliebiges Ergebnis nehmen, falls vorhanden:", pick_txt);
                    return Err(RtErr::data(d));
                }
                if all.len() > 1 {
                    let full = self.text(e.span).to_string();
                    let n = all.len();
                    let ids: Vec<String> = all.iter().take(3).map(|v| v.display()).collect();
                    let mut d = Diagnostic::error(
                        "E402",
                        the_span,
                        Text2::new(
                            format!("\"the\" expects exactly one result, but found {n}."),
                            format!("\"the\" erwartet genau ein Ergebnis, hat aber {n} gefunden."),
                        ),
                    );
                    d = d.why(
                        format!("{n} entries in '{what}' match{}: {}{}.", cond_txt.as_ref().map(|c| format!(" {c}")).unwrap_or_default(), ids.join(", "), if n > 3 { ", …" } else { "" }),
                        format!("{n} Einträge in '{what}' passen{}: {}{}.", cond_txt.as_ref().map(|c| format!(" zu {c}")).unwrap_or_default(), ids.join(", "), if n > 3 { ", …" } else { "" }),
                    );
                    d = d
                        .header(Header::YouCan)
                        .sugg_label("narrow the condition", "die Bedingung einschränken", "… and …")
                        .sugg_label("take the first one:", "das erste nehmen:", full.replacen("the", "pick", 1))
                        .sugg_label("take them all:", "alle nehmen:", full.replacen("the", "all", 1));
                    return Err(RtErr::data(d));
                }
                let v = all.into_iter().next().unwrap();
                if let (Some(k), Value::Elem(el)) = (ek, &v)
                    && el.kind != k
                {
                    let full = self.text(e.span).to_string();
                    return Err(RtErr::data(
                        Diagnostic::error(
                            "E403",
                            Span { start: e.span.start, end: e.span.start + 3 + 1 + k.name().len() as u32 },
                            Text2::new(
                                format!("Expected {} {}, but the single result is {} {}.", article(k.name()), k.name(), article(el.kind.name()), el.kind.name()),
                                format!("Erwartet war {} {}, das einzige Ergebnis ist aber {} {}.", artikel(k.name()), k.name(), artikel(el.kind.name()), el.kind.name()),
                            ),
                        )
                        .why(format!("{} matches.", el.label()), format!("{} passt.", el.label()))
                        .header(Header::YouCan)
                        .sugg_label(&format!("leave out \"{}\":", k.name()), &format!("\"{}\" weglassen:", k.name()), full.replacen(&format!(" {} ", k.name()), " ", 1)),
                    ));
                }
                Ok(v)
            }
        }
    }
}

fn article(w: &str) -> &'static str {
    if w.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" }
}

fn artikel(_: &str) -> &'static str {
    "ein"
}

fn start_of(v: &Value) -> Value {
    match v {
        Value::Time(d) => Value::instant(d.start),
        _ => v.clone(),
    }
}

/// Deterministic random rank of an element for `randomly` (§5.6).
pub fn rand_rank(seed: u64, v: &Value) -> u64 {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&seed.to_le_bytes());
    match v.key() {
        Key::Elem(k, id, ver) => {
            buf.push(k);
            buf.extend_from_slice(&id.to_le_bytes());
            buf.extend_from_slice(&ver.to_le_bytes());
        }
        k => buf.extend_from_slice(format!("{k:?}").as_bytes()),
    }
    xxhash_rust::xxh3::xxh3_64(&buf)
}

/// The first `n` elements in (kind, id, version) order, by random rank, or by `order`.
pub fn pick_n(ctx: &Ctx, set: &Arc<SetV>, n: usize, seed: Option<u64>, order: Option<&'static Expr>, vars: &[String]) -> R<Vec<Value>> {
    if let Some(order) = order {
        let geom = uses_geom(order);
        let items = set.items(ctx, Hints { geom, ..Default::default() })?;
        if geom {
            ctx.preload_geoms(&items);
        }
        use rayon::prelude::*;
        let mut keyed = items
            .par_iter()
            .map(|v| {
                let sc = ctx.bind_elem(&None, vars, v);
                Ok((ctx.eval(order, &sc)?.key(), v.key(), v.clone()))
            })
            .collect::<R<Vec<_>>>()?;
        keyed.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        return Ok(keyed.into_iter().take(n).map(|x| x.2).collect());
    }
    let rank = |v: &Value| -> (u64, Key) {
        match seed {
            Some(s) => (rand_rank(s, v), v.key()),
            None => (0, v.key()),
        }
    };
    let parts = set.fold(ctx, Hints::default(), &Vec::new, &|acc: &mut Vec<((u64, Key), Value)>, v: Value| {
        acc.push((rank(&v), v));
        if acc.len() > 4 * n + 64 {
            acc.sort_by(|a, b| a.0.cmp(&b.0));
            acc.dedup_by(|a, b| a.0 == b.0);
            acc.truncate(n);
        }
        Ok(())
    })?;
    let mut all: Vec<((u64, Key), Value)> = parts.into_iter().flatten().collect();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all.dedup_by(|a, b| a.0 == b.0);
    all.truncate(n);
    Ok(all.into_iter().map(|x| x.1).collect())
}

/// Sliding windows of consecutive versions (`A -> B -> C`).
/// True if `e` uses the pattern variables only for tags and plain fields of their version.
fn only_attributes(e: &Expr, vars: &[String]) -> bool {
    const PLAIN: &[&str] = &["id", "version", "time", "user", "uid", "changeset", "deleted", "type", "osmType", "creator", "unchangedBefore", "unchangedAfter", "keys"];
    let is_var = |x: &Expr| matches!(&x.kind, ExprKind::Ident(n) if vars.contains(n));
    match &e.kind {
        ExprKind::Tag(b, k, _) if is_var(b) => match k {
            TagKey::Lit(_) => true,
            TagKey::Expr(k) => only_attributes(k, vars),
        },
        ExprKind::Field(b, name, _) if is_var(b) => PLAIN.contains(&name.as_str()),
        ExprKind::Ident(n) => !vars.contains(n),
        _ => e.children().into_iter().all(|c| only_attributes(c, vars)),
    }
}

/// The conditions `X:k != Y:k2` (tags of two pattern variables, literal keys, no units) among
/// the conjuncts of `preds`.
fn differing_tags(preds: &[&'static Expr], vars: &[String]) -> Vec<(usize, String, usize, String)> {
    fn tag_of(e: &Expr, vars: &[String]) -> Option<(usize, String)> {
        let ExprKind::Tag(base, TagKey::Lit(k), None) = &e.kind else { return None };
        let ExprKind::Ident(n) = &base.kind else { return None };
        Some((vars.iter().position(|v| v == n)?, k.clone()))
    }
    fn walk(e: &Expr, vars: &[String], out: &mut Vec<(usize, String, usize, String)>) {
        match &e.kind {
            ExprKind::And(a, b) => {
                walk(a, vars, out);
                walk(b, vars, out);
            }
            ExprKind::Rel(l, RelTail::Cmp(CmpOp::Ne, r)) => {
                if let (Some((i, ki)), Some((j, kj))) = (tag_of(l, vars), tag_of(r, vars)) {
                    out.push((i, ki, j, kj));
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for p in preds {
        walk(p, vars, &mut out);
    }
    out
}

pub fn match_sequence(ctx: &Ctx, q: &Query, seq: &Seq, h: &Arc<Hist>, f: &mut dyn FnMut(Value) -> R<()>) -> R<()> {
    let k = seq.kinds.len();
    let n = h.versions.len();
    if n < k {
        return Ok(());
    }
    let kind_ok = |v: &Ver, kind: EKind| -> bool {
        match (kind, h.otype) {
            (EKind::Node, ObjType::Node) | (EKind::Way, ObjType::Way) | (EKind::Relation, ObjType::Relation) => true,
            (EKind::Area, ObjType::Way) => v.closed() && crate::areas::way_is_area(&v.tags),
            (EKind::Area, ObjType::Relation) => crate::areas::relation_is_area(&v.tags),
            _ => false,
        }
    };
    for i in 0..=n - k {
        let win = &h.versions[i..i + k];
        if !win.iter().zip(&seq.kinds).all(|(v, kk)| kind_ok(v, *kk)) {
            continue;
        }
        if let Some((a, b)) = seq.t_range
            && !win.iter().all(|v| v.ts >= a && v.ts < b)
        {
            continue;
        }
        if let Some((d, or_eq)) = seq.max_dt {
            let dt = (win[k - 1].ts - win[0].ts) as f64;
            if !(dt < d || (or_eq && dt <= d)) {
                continue;
            }
        }
        // `X:k != Y:k2` is false when both are the same text (a missing tag is left to the
        // evaluation, which reports comparisons with missing values)
        if seq.differ.iter().any(|(i, ki, j, kj)| matches!((win[*i].tag(ki), win[*j].tag(kj)), (Some(a), Some(b)) if a == b)) {
            continue;
        }
        let items: Vec<Arc<Elem>> = win
            .iter()
            .zip(&seq.kinds)
            .map(|(v, kk)| {
                let e = Elem::new(*kk, Some(h.otype), h.id, v.clone(), v.ts);
                let _ = e.hist.set(h.clone());
                Arc::new(e)
            })
            .collect();
        if let Some(reg) = &q.spec.region {
            let p = ctx.rep_point(&Value::Elem(items[0].clone()));
            if !p.is_some_and(|p| reg.contains(p)) {
                continue;
            }
        }
        let first = items[0].clone();
        let t = Value::Tuple(Arc::new(Tuple { names: seq.names.clone(), items }));
        let sc = ctx.bind_elem(&q.scope, &q.vars, &t);
        let mut ok = true;
        for p in &q.preds {
            if !ctx.truthy(p, &sc).map_err(|e| e.at(&first))? {
                ok = false;
                break;
            }
        }
        if ok {
            f(t)?;
        }
    }
    Ok(())
}

/// Lock helper.
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Geometry of a way from its node positions: a polygon for areas, otherwise a line.
fn way_geom(kind: EKind, pos: &[Option<geom::P>]) -> Geom {
    let pts: Vec<geom::P> = pos.iter().flatten().copied().collect();
    if kind == EKind::Area && pts.len() >= 4 {
        let mut ring = pts;
        if ring.first() != ring.last() {
            ring.push(ring[0]);
        }
        Geom::Poly(vec![geom::Polygon { outer: ring, holes: vec![] }])
    } else {
        Geom::Line(pts)
    }
}

/// The error of a cancelled run.
pub fn cancelled() -> RtErr {
    RtErr::fatal(Diagnostic::error("E902", Span::default(), Text2::new("The run was cancelled.", "Der Lauf wurde abgebrochen.")))
}

/// Visits `e` and all its sub-expressions, keeping their `'static` lifetime.
/// Whether evaluating `e` needs the geometry of elements (fields such as `.area`, spatial
/// relations, nearest): then a set is better given its geometry in one batch.
pub fn uses_geom(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        found |= match &x.kind {
            ExprKind::Field(_, name, _) => matches!(name.as_str(), "area" | "length" | "geometry" | "center" | "lon" | "lat" | "nodes"),
            ExprKind::Rel(_, RelTail::Within(_) | RelTail::Overlapping(_) | RelTail::Near { .. }) => true,
            ExprKind::Of { func, .. } => matches!(func, OfFunc::Nearest | OfFunc::Spread | OfFunc::Density),
            _ => false,
        };
    });
    found
}

fn walk_static(e: &'static Expr, f: &mut dyn FnMut(&'static Expr)) {
    f(e);
    for c in e.children() {
        walk_static(c, f);
    }
}
