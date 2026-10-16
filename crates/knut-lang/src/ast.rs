//! Abstract syntax tree (KNUT_LANG.md §3).

use crate::dates::{DateVal, TimeUnit};
use crate::diag::Span;
use crate::units::Unit;

pub type P<T> = Box<T>;

#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    Setting { name: String, name_span: Span, value: Expr },
    Define { kind: Kind, var: String, field: String, field_span: Span, body: Expr },
    Assign { name: String, name_span: Span, value: Expr },
    Output(Vec<Expr>),
}

/// Object kinds (§1.2 of the user description).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Node,
    Way,
    Area,
    Relation,
    Any,
    Changeset,
    User,
    Note,
    Key,
}

impl Kind {
    pub fn parse(s: &str) -> Option<(Kind, bool)> {
        Some(match s {
            "node" => (Kind::Node, false),
            "nodes" => (Kind::Node, true),
            "way" => (Kind::Way, false),
            "ways" => (Kind::Way, true),
            "area" => (Kind::Area, false),
            "areas" => (Kind::Area, true),
            "relation" => (Kind::Relation, false),
            "relations" => (Kind::Relation, true),
            "changeset" => (Kind::Changeset, false),
            "changesets" => (Kind::Changeset, true),
            "user" => (Kind::User, false),
            "users" => (Kind::User, true),
            "note" => (Kind::Note, false),
            "notes" => (Kind::Note, true),
            "key" => (Kind::Key, false),
            "keys" => (Kind::Key, true),
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Node => "node",
            Kind::Way => "way",
            Kind::Area => "area",
            Kind::Relation => "relation",
            Kind::Any => "*",
            Kind::Changeset => "changeset",
            Kind::User => "user",
            Kind::Note => "note",
            Kind::Key => "key",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::Node => "nodes",
            Kind::Way => "ways",
            Kind::Area => "areas",
            Kind::Relation => "relations",
            Kind::Any => "elements",
            Kind::Changeset => "changesets",
            Kind::User => "users",
            Kind::Note => "notes",
            Kind::Key => "keys",
        }
    }

    pub fn is_osm(self) -> bool {
        matches!(self, Kind::Node | Kind::Way | Kind::Area | Kind::Relation | Kind::Any)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Quant {
    Some,
    No,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Foot,
    Bicycle,
    Car,
    Transit,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "foot" => Mode::Foot,
            "bicycle" => Mode::Bicycle,
            "car" => Mode::Car,
            "transit" => Mode::Transit,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Foot => "foot",
            Mode::Bicycle => "bicycle",
            Mode::Car => "car",
            Mode::Transit => "transit",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModeClause {
    pub mode: Mode,
    pub mode_span: Span,
    pub using: Option<P<Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Threshold {
    pub value: P<Expr>,
    pub or_equal: bool,
    pub mode: Option<ModeClause>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TagKey {
    Lit(String),
    Expr(P<Expr>),
}

/// Relational suffix applied to an element (`x near …`, `x within R`, …).
#[derive(Clone, Debug, PartialEq)]
pub enum RelTail {
    Cmp(CmpOp, P<Expr>),
    In(P<Expr>),
    Between(P<Expr>, P<Expr>),
    Within(P<Expr>),
    Overlapping(P<Expr>),
    Near { quant: Quant, set: P<Expr>, threshold: Option<Threshold> },
    NextOf(P<Expr>),
    PreviousOf(P<Expr>),
    Before(P<Expr>),
    After(P<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TimeRef {
    On(P<Expr>),
    /// `on each U from A to B` (the range comes from the trailing `from … to`).
    OnEach { unit: TimeUnit, span: Span, range: Option<(P<Expr>, P<Expr>)> },
    Range(P<Expr>, P<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternItem {
    pub kind: Kind,
    pub plural: bool,
    pub var: String,
    pub var_span: Span,
    pub kind_span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub exactly: bool,
    pub n: Option<u64>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    /// `homes h with …`: filter over a named set instead of the database.
    pub source: Option<P<Expr>>,
    pub pick: Option<Pick>,
    pub pattern: Vec<PatternItem>,
    pub seq_threshold: Option<Threshold>,
    pub randomly: bool,
    pub seed: Option<u64>,
    /// Condition on the variable before `with` (`b near some _ in …`).
    pub var_pred: Option<P<Expr>>,
    pub cond: Option<P<Expr>>,
    pub within: Option<P<Expr>>,
    pub time: Option<TimeRef>,
    pub order: Option<P<Expr>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QKind {
    The,
    All,
    Some,
    Groups,
    /// `pick [exactly] [N] x in SET …`
    Pick { n: Option<u64>, exactly: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Grouping {
    Cell(P<Expr>),
    Tile(u32),
    Chunk(u64),
    Time(TimeUnit),
    Expr(P<Expr>),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Clauses {
    pub with: Option<P<Expr>>,
    pub within: Option<P<Expr>>,
    pub time: Option<TimeRef>,
    pub per: Vec<(Grouping, Span)>,
    pub having: Option<P<Expr>>,
    pub order: Option<P<Expr>>,
    pub to: Option<P<Expr>>,
    pub mode: Option<ModeClause>,
}

impl Clauses {
    pub fn is_empty(&self) -> bool {
        *self == Clauses::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OfFunc {
    Count,
    Sum,
    Average,
    Distribution,
    Share,
    Entropy,
    Gini,
    Variance,
    Density,
    Spread,
    Streaks,
    Transitions,
    Distinct,
    Earliest,
    Latest,
    Days,
    Weeks,
    Months,
    Years,
    User,
    FirstVersion,
    LastVersion,
    Nearest,
    Graph,
    /// `first of LIST`, `last of LIST`
    First,
    Last,
}

impl OfFunc {
    pub fn parse(s: &str) -> Option<OfFunc> {
        Some(match s {
            "count" => OfFunc::Count,
            "sum" => OfFunc::Sum,
            "average" => OfFunc::Average,
            "distribution" => OfFunc::Distribution,
            "share" => OfFunc::Share,
            "entropy" => OfFunc::Entropy,
            "gini" => OfFunc::Gini,
            "variance" => OfFunc::Variance,
            "density" => OfFunc::Density,
            "spread" => OfFunc::Spread,
            "streaks" => OfFunc::Streaks,
            "transitions" => OfFunc::Transitions,
            "distinct" => OfFunc::Distinct,
            "earliest" => OfFunc::Earliest,
            "latest" => OfFunc::Latest,
            "days" => OfFunc::Days,
            "weeks" => OfFunc::Weeks,
            "months" => OfFunc::Months,
            "years" => OfFunc::Years,
            "user" => OfFunc::User,
            "nearest" => OfFunc::Nearest,
            "graph" => OfFunc::Graph,
            "first" => OfFunc::First,
            "last" => OfFunc::Last,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            OfFunc::Count => "count",
            OfFunc::Sum => "sum",
            OfFunc::Average => "average",
            OfFunc::Distribution => "distribution",
            OfFunc::Share => "share",
            OfFunc::Entropy => "entropy",
            OfFunc::Gini => "gini",
            OfFunc::Variance => "variance",
            OfFunc::Density => "density",
            OfFunc::Spread => "spread",
            OfFunc::Streaks => "streaks",
            OfFunc::Transitions => "transitions",
            OfFunc::Distinct => "distinct",
            OfFunc::Earliest => "earliest",
            OfFunc::Latest => "latest",
            OfFunc::Days => "days",
            OfFunc::Weeks => "weeks",
            OfFunc::Months => "months",
            OfFunc::Years => "years",
            OfFunc::User => "user",
            OfFunc::FirstVersion => "first version",
            OfFunc::LastVersion => "last version",
            OfFunc::Nearest => "nearest",
            OfFunc::Graph => "graph",
            OfFunc::First => "first",
            OfFunc::Last => "last",
        }
    }

    /// Functions that take a whole set and reduce it.
    pub fn is_aggregate(self) -> bool {
        !matches!(self, OfFunc::User | OfFunc::FirstVersion | OfFunc::LastVersion | OfFunc::Nearest | OfFunc::Graph | OfFunc::First | OfFunc::Last)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OfOperand {
    /// `x in SET [rel_tail] [with COND]`
    Iter { var: String, var_span: Span, set: P<Expr>, pred: Option<P<Expr>>, cond: Option<P<Expr>> },
    Expr(P<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Special {
    Path { var: String, from: P<Expr>, to: P<Expr>, mode: ModeClause },
    Isochrone { var: String, from: P<Expr>, threshold: Threshold },
    Dataset(String),
    Split { set: P<Expr>, by: P<Expr>, at: P<Expr> },
    Correlate(P<Expr>, P<Expr>),
    Regress(P<Expr>, P<Expr>),
    Confusion(P<Expr>, P<Expr>),
    FitPowerlaw(P<Expr>),
    Forecast { n: u64, unit: TimeUnit, of: P<Expr> },
    Classify { set: P<Expr>, label: P<Expr>, features: P<Expr> },
    /// `geocode TEXT` (a ranked list of elements with that address) or `geocoded TEXT` (the
    /// best one).
    Geocode { one: bool, query: P<Expr> },
    /// `reverse-geocode X` (elements with addresses near a point or element, nearest first)
    /// or `reverse-geocoded X` (the nearest).
    ReverseGeocode { one: bool, at: P<Expr> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Num(f64),
    Str(String),
    Date(DateVal),
    None,
    Bool(bool),
    Ident(String),
    Underscore,
    List(Vec<Expr>),
    /// lon0, lon1, lat0, lat1
    BoxLit([f64; 4]),
    /// `8.8079E, 53.07N`: lon, lat
    Point([f64; 2]),
    /// Number or tag with a unit.
    Unit(P<Expr>, &'static Unit, Span),
    Field(P<Expr>, String, Span),
    Tag(P<Expr>, TagKey, Option<String>),
    Call(P<Expr>, Vec<Expr>),
    Neg(P<Expr>),
    Not(P<Expr>),
    Bin(BinOp, P<Expr>, P<Expr>),
    And(P<Expr>, P<Expr>),
    Or(P<Expr>, P<Expr>),
    Fallback(P<Expr>, P<Expr>),
    /// `left rel_tail`
    Rel(P<Expr>, RelTail),
    Lambda(Vec<String>, P<Expr>),
    Select(P<Selection>),
    Quantified { q: QKind, kind: Option<Kind>, var: String, var_span: Span, set: P<Expr>, pred: Option<P<Expr>>, cond: Option<P<Expr>> },
    Of { func: OfFunc, func_span: Span, operand: OfOperand, clauses: P<Clauses> },
    Special(P<Special>),
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }

    /// Visits all direct sub-expressions.
    pub fn children(&self) -> Vec<&Expr> {
        let mut v: Vec<&Expr> = Vec::new();
        fn mode<'a>(v: &mut Vec<&'a Expr>, m: &'a Option<ModeClause>) {
            if let Some(m) = m
                && let Some(u) = &m.using
            {
                v.push(u);
            }
        }
        fn thr<'a>(v: &mut Vec<&'a Expr>, t: &'a Option<Threshold>) {
            if let Some(t) = t {
                v.push(&t.value);
                mode(v, &t.mode);
            }
        }
        fn time<'a>(v: &mut Vec<&'a Expr>, t: &'a Option<TimeRef>) {
            match t {
                Some(TimeRef::On(e)) => v.push(e),
                Some(TimeRef::Range(a, b)) | Some(TimeRef::OnEach { range: Some((a, b)), .. }) => {
                    v.push(a);
                    v.push(b);
                }
                _ => {}
            }
        }
        match &self.kind {
            ExprKind::List(xs) => v.extend(xs.iter()),
            ExprKind::Unit(e, _, _) | ExprKind::Field(e, _, _) | ExprKind::Neg(e) | ExprKind::Not(e) => v.push(e),
            ExprKind::Tag(e, k, _) => {
                v.push(e);
                if let TagKey::Expr(k) = k {
                    v.push(k);
                }
            }
            ExprKind::Call(f, args) => {
                v.push(f);
                v.extend(args.iter());
            }
            ExprKind::Bin(_, a, b) | ExprKind::And(a, b) | ExprKind::Or(a, b) | ExprKind::Fallback(a, b) => {
                v.push(a);
                v.push(b);
            }
            ExprKind::Rel(l, t) => {
                v.push(l);
                match t {
                    RelTail::Cmp(_, e)
                    | RelTail::In(e)
                    | RelTail::Within(e)
                    | RelTail::Overlapping(e)
                    | RelTail::NextOf(e)
                    | RelTail::PreviousOf(e)
                    | RelTail::Before(e)
                    | RelTail::After(e) => v.push(e),
                    RelTail::Between(a, b) => {
                        v.push(a);
                        v.push(b);
                    }
                    RelTail::Near { set, threshold, .. } => {
                        v.push(set);
                        thr(&mut v, threshold);
                    }
                }
            }
            ExprKind::Lambda(_, b) => v.push(b),
            ExprKind::Select(s) => {
                if let Some(src) = &s.source {
                    v.push(src);
                }
                thr(&mut v, &s.seq_threshold);
                for e in [&s.var_pred, &s.cond, &s.within, &s.order].into_iter().flatten() {
                    v.push(e);
                }
                time(&mut v, &s.time);
            }
            ExprKind::Quantified { set, pred, cond, .. } => {
                v.push(set);
                for e in [pred, cond].into_iter().flatten() {
                    v.push(e);
                }
            }
            ExprKind::Of { operand, clauses, .. } => {
                match operand {
                    OfOperand::Iter { set, pred, cond, .. } => {
                        v.push(set);
                        for e in [pred, cond].into_iter().flatten() {
                            v.push(e);
                        }
                    }
                    OfOperand::Expr(e) => v.push(e),
                }
                for e in [&clauses.with, &clauses.within, &clauses.having, &clauses.order, &clauses.to].into_iter().flatten() {
                    v.push(e);
                }
                time(&mut v, &clauses.time);
                for (g, _) in &clauses.per {
                    match g {
                        Grouping::Cell(e) | Grouping::Expr(e) => v.push(e),
                        _ => {}
                    }
                }
                mode(&mut v, &clauses.mode);
            }
            ExprKind::Special(s) => match &**s {
                Special::Path { from, to, mode: m, .. } => {
                    v.push(from);
                    v.push(to);
                    if let Some(u) = &m.using {
                        v.push(u);
                    }
                }
                Special::Isochrone { from, threshold, .. } => {
                    v.push(from);
                    v.push(&threshold.value);
                    mode(&mut v, &threshold.mode);
                }
                Special::Dataset(_) => {}
                Special::Split { set, by, at } => {
                    v.push(set);
                    v.push(by);
                    v.push(at);
                }
                Special::Correlate(a, b) | Special::Regress(a, b) | Special::Confusion(a, b) => {
                    v.push(a);
                    v.push(b);
                }
                Special::FitPowerlaw(a) => v.push(a),
                Special::Forecast { of, .. } => v.push(of),
                Special::Classify { set, label, features } => {
                    v.push(set);
                    v.push(label);
                    v.push(features);
                }
                Special::Geocode { query, .. } => v.push(query),
                Special::ReverseGeocode { at, .. } => v.push(at),
            },
            _ => {}
        }
        v
    }

    /// Pre-order walk.
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        for c in self.children() {
            c.walk(f);
        }
    }

    /// True for `VAR` and `VAR.field` chains (values that can be elements).
    pub fn is_elem_path(&self) -> bool {
        match &self.kind {
            ExprKind::Ident(_) | ExprKind::Underscore => true,
            ExprKind::Field(e, _, _) => e.is_elem_path(),
            _ => false,
        }
    }

    /// True for `VAR`, `VAR.field`, `VAR:key` chains without a short-form value.
    pub fn is_path(&self) -> bool {
        match &self.kind {
            ExprKind::Ident(_) | ExprKind::Underscore => true,
            ExprKind::Field(e, _, _) => e.is_path(),
            ExprKind::Tag(e, _, None) => e.is_path(),
            ExprKind::Unit(e, _, _) => matches!(e.kind, ExprKind::Tag(..)) && e.is_path(),
            _ => false,
        }
    }
}
