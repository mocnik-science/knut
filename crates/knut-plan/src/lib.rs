//! knut language checker (KNUT_LANG.md §8, phases 2–4): name resolution, settings, types,
//! units and static warnings, plus the pushdown analysis used by the executor.

pub mod pushdown;
pub mod types;

use knut_lang::ast::*;
use knut_lang::diag::{self, Diagnostic, Header, Lang, Span, Text2};
use knut_lang::parser::{KEYWORDS, SETTINGS};
use knut_lang::units::{self, Dim};
use std::collections::{HashMap, HashSet};
pub use types::Ty;

#[derive(Clone, Debug)]
pub struct Settings {
    pub knut_db: Option<(String, Span)>,
    pub seed: u64,
    pub lang: Lang,
    pub ignore_warnings: bool,
    /// Metres.
    pub near_distance: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { knut_db: None, seed: 42, lang: Lang::En, ignore_warnings: false, near_distance: 100.0 }
    }
}

/// What the database offers; `None` while no database is open.
#[derive(Clone, Debug, Default)]
pub struct DbCaps {
    /// User ids and names are present (not built with `--strip-users`).
    pub users: bool,
    pub notes: bool,
    /// Area of the database extent in km².
    pub extent_km2: f64,
    /// Path of the database, for suggestions.
    pub path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IdentRes {
    Global(String),
    Local(String),
    Text(String),
    Region(String),
    AllOfKind(Kind),
    /// Field of the implicit element (`split … at unchangedBefore > 30 min`, `having count`).
    ImplicitField(String),
    Projection { var: String, pos: usize },
    /// A setting used as a value (`seed`).
    Setting(String),
}

#[derive(Clone, Debug)]
pub struct DefineInfo {
    pub kind: Kind,
    pub var: String,
    pub field: String,
    pub stmt: usize,
}

#[derive(Clone, Debug)]
pub struct GlobalInfo {
    pub stmt: usize,
    pub line: usize,
    pub span: Span,
    pub ty: Ty,
    /// Element variable names of the defining selection (for clauses).
    pub elem_vars: Vec<String>,
}

/// A comparison that leaves out elements without the tag (W001), counted at run time.
#[derive(Clone, Debug)]
pub struct W001Site {
    pub span: Span,
    pub key: String,
    pub kind: Option<Kind>,
}

pub struct Checked {
    pub script: Script,
    pub src: String,
    pub settings: Settings,
    pub diags: Vec<Diagnostic>,
    pub idents: HashMap<Span, IdentRes>,
    pub defines: Vec<DefineInfo>,
    pub globals: HashMap<String, GlobalInfo>,
    /// Names bound to the current element when evaluating the clauses of an `of` call,
    /// keyed by the span of the call.
    pub clause_vars: HashMap<Span, Vec<String>>,
    pub w001: Vec<W001Site>,
    /// Statements that are outputs, with their index in `script.stmts`.
    pub outputs: Vec<usize>,
    pub needs_db: bool,
}

impl Checked {
    pub fn errors(&self) -> usize {
        self.diags.iter().filter(|d| d.is_error()).count()
    }

    pub fn warnings(&self) -> usize {
        self.diags.len() - self.errors()
    }

    /// True if the script may run (no errors, warnings ignored or absent).
    pub fn runnable(&self) -> bool {
        self.errors() == 0 && (self.warnings() == 0 || self.settings.ignore_warnings)
    }

    pub fn line_of(&self, s: Span) -> usize {
        diag::line_col(&self.src, s.start).0 + 1
    }
}

impl pushdown::TextIdents for Checked {
    fn is_text(&self, e: &Expr) -> bool {
        matches!(self.idents.get(&e.span), Some(IdentRes::Text(_)))
    }

    fn counts_missing(&self, e: &Expr) -> bool {
        self.settings.ignore_warnings && self.w001.iter().any(|w| w.span == e.span)
    }
}

/// Reads the settings only (to find the database before the full check).
pub fn settings_of(script: &Script) -> Settings {
    let mut s = Settings::default();
    for st in &script.stmts {
        if let StmtKind::Setting { name, value, .. } = &st.kind {
            apply_setting(&mut s, name, value);
        }
    }
    s
}

fn apply_setting(s: &mut Settings, name: &str, v: &Expr) -> bool {
    match (name, &v.kind) {
        ("knut_db", ExprKind::Str(p)) => s.knut_db = Some((p.clone(), v.span)),
        ("seed", ExprKind::Num(n)) if n.fract() == 0.0 && *n >= 0.0 => s.seed = *n as u64,
        ("messages", ExprKind::Ident(l) | ExprKind::Str(l)) if l == "en" || l == "de" => {
            s.lang = if l == "de" { Lang::De } else { Lang::En }
        }
        ("ignore_warnings", ExprKind::Bool(b)) => s.ignore_warnings = *b,
        ("near_distance", ExprKind::Unit(n, u, _)) if u.dim == Dim::LENGTH => {
            if let ExprKind::Num(x) = n.kind {
                s.near_distance = x * u.factor;
            } else {
                return false;
            }
        }
        _ => return false,
    }
    true
}

/// Runs phases 2–4 on a parsed script.
pub fn check(script: Script, src: &str, mut diags: Vec<Diagnostic>, caps: Option<&DbCaps>) -> Checked {
    let mut c = Checker {
        src,
        caps,
        diags: Vec::new(),
        idents: HashMap::new(),
        defines: Vec::new(),
        globals: HashMap::new(),
        later: HashMap::new(),
        clause_vars: HashMap::new(),
        w001: Vec::new(),
        scopes: Vec::new(),
        stmt: 0,
        settings: Settings::default(),
        seen_settings: HashMap::new(),
        unit_words: Vec::new(),
        var_words: Vec::new(),
        needs_db: false,
        implicit: 0,
    };
    // names defined later (for "used before it is defined")
    for (i, st) in script.stmts.iter().enumerate() {
        if let StmtKind::Assign { name, name_span, .. } = &st.kind {
            c.later.entry(name.clone()).or_insert((i, *name_span));
        }
    }
    // define fields first: they may be used anywhere
    for (i, st) in script.stmts.iter().enumerate() {
        if let StmtKind::Define { kind, var, field, .. } = &st.kind {
            c.defines.push(DefineInfo { kind: *kind, var: var.clone(), field: field.clone(), stmt: i });
        }
    }
    let mut outputs = Vec::new();
    for (i, st) in script.stmts.iter().enumerate() {
        c.stmt = i;
        c.unit_words.clear();
        c.var_words.clear();
        match &st.kind {
            StmtKind::Setting { name, name_span, value } => c.setting(name, *name_span, value),
            StmtKind::Define { kind, var, field, field_span, body } => {
                if *field == "home" {
                    c.e203(*field_span, "home", Some(Kind::User));
                }
                c.scopes.push(vec![(var.clone(), Ty::Elem(Some(*kind)))]);
                c.var_words.push((var.clone(), Span::default()));
                c.infer(body);
                c.scopes.pop();
            }
            StmtKind::Assign { name, name_span, value } => {
                if let Some(g) = c.globals.get(name) {
                    let line = g.line;
                    c.diags.push(
                        Diagnostic::error(
                            "E208",
                            *name_span,
                            Text2::new(format!("\"{name}\" is already defined (line {line})."), format!("\"{name}\" ist schon definiert (Zeile {line}).")),
                        )
                        .why(
                            "Every name gets its value once. Use a new name for the new value.",
                            "Jeder Name bekommt nur einmal einen Wert. Für den neuen Wert braucht es einen neuen Namen.",
                        ),
                    );
                }
                if KEYWORDS.contains(&name.as_str()) || SETTINGS.contains(&name.as_str()) {
                    c.diags.push(Diagnostic::error(
                        "E108",
                        *name_span,
                        Text2::new(format!("\"{name}\" is a keyword and cannot be used as a name."), format!("\"{name}\" ist ein Schlüsselwort und kann kein Name sein.")),
                    ));
                }
                c.var_words.push((name.clone(), *name_span));
                let ty = c.infer(value);
                let elem_vars = elem_vars_of(value, &c.globals, &c.idents);
                let line = diag::line_col(src, name_span.start).0 + 1;
                // E212: `war_B` as a name and as the projection of a sequence `war`
                let positions = |t: &Ty| -> Vec<String> {
                    match t {
                        Ty::Set(x) => match &**x {
                            Ty::Tuple(items) => items.iter().map(|(v, _)| v.clone()).collect(),
                            _ => Vec::new(),
                        },
                        _ => Vec::new(),
                    }
                };
                let mut clashes: Vec<(String, String, Span)> = Vec::new();
                if let Some(k) = name.rfind('_') {
                    if let Some(g) = c.globals.get(&name[..k]) {
                        if positions(&g.ty).iter().any(|p| *p == name[k + 1..]) {
                            clashes.push((name.clone(), name[..k].to_string(), *name_span));
                        }
                    }
                }
                for p in positions(&ty) {
                    let proj = format!("{name}_{p}");
                    if let Some(g) = c.globals.get(&proj) {
                        clashes.push((proj, name.clone(), g.span));
                    }
                }
                for (proj, seq, sp) in clashes {
                    c.diags.push(
                        Diagnostic::error(
                            "E212",
                            sp,
                            Text2::new(
                                format!("\"{proj}\" is both a variable and a projection of \"{seq}\"."),
                                format!("\"{proj}\" ist zugleich eine Variable und eine Projektion von \"{seq}\"."),
                            ),
                        )
                        .why("Please choose another name for the variable.", "Bitte wähle für die Variable einen anderen Namen."),
                    );
                }
                c.globals.insert(name.clone(), GlobalInfo { stmt: i, line, span: *name_span, ty, elem_vars });
            }
            StmtKind::Output(es) => {
                outputs.push(i);
                for e in es {
                    let t = c.infer(e);
                    if matches!(t, Ty::Lambda) {
                        c.diags.push(Diagnostic::error(
                            "E606",
                            e.span,
                            Text2::new("A function on its own cannot be shown.", "Eine Funktion allein lässt sich nicht ausgeben."),
                        ));
                    }
                }
            }
        }
        c.check_w005();
    }
    c.check_define_cycles(&script);
    let settings = c.settings.clone();
    diags.append(&mut c.diags);
    diag::sort(&mut diags);
    diags.dedup_by(|a, b| a.code == b.code && a.span == b.span);
    Checked {
        src: src.to_string(),
        settings,
        diags,
        idents: c.idents,
        defines: c.defines,
        globals: c.globals,
        clause_vars: c.clause_vars,
        w001: c.w001,
        outputs,
        needs_db: c.needs_db,
        script,
    }
}

/// Element variable names of a value that is a selection.
fn elem_vars_of(e: &Expr, globals: &HashMap<String, GlobalInfo>, idents: &HashMap<Span, IdentRes>) -> Vec<String> {
    match &e.kind {
        ExprKind::Select(s) => s.pattern.iter().map(|p| p.var.clone()).collect(),
        ExprKind::Quantified { var, .. } => vec![var.clone()],
        ExprKind::Ident(_) => match idents.get(&e.span) {
            Some(IdentRes::Global(g)) => globals.get(g).map(|g| g.elem_vars.clone()).unwrap_or_default(),
            Some(IdentRes::Projection { var, pos }) => globals
                .get(var)
                .and_then(|g| g.elem_vars.get(*pos).cloned())
                .map(|v| vec![v])
                .unwrap_or_default(),
            _ => Vec::new(),
        },
        ExprKind::Of { func: OfFunc::Distinct, operand: OfOperand::Expr(x), .. } => elem_vars_of(x, globals, idents),
        _ => Vec::new(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Pos {
    Normal,
    Value,
    Region,
    Net,
}

struct Checker<'a> {
    src: &'a str,
    caps: Option<&'a DbCaps>,
    diags: Vec<Diagnostic>,
    idents: HashMap<Span, IdentRes>,
    defines: Vec<DefineInfo>,
    globals: HashMap<String, GlobalInfo>,
    later: HashMap<String, (usize, Span)>,
    clause_vars: HashMap<Span, Vec<String>>,
    w001: Vec<W001Site>,
    scopes: Vec<Vec<(String, Ty)>>,
    stmt: usize,
    settings: Settings,
    seen_settings: HashMap<String, Span>,
    /// Unit words used as units in the current statement (W005).
    unit_words: Vec<(String, Span)>,
    /// Variable names bound or used in the current statement (W005).
    var_words: Vec<(String, Span)>,
    needs_db: bool,
    /// > 0 while resolving names where bare fields of an implicit element are allowed.
    implicit: u32,
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

impl<'a> Checker<'a> {
    fn text(&self, s: Span) -> &'a str {
        &self.src[s.start as usize..(s.end as usize).min(self.src.len())]
    }

    fn line(&self, s: Span) -> usize {
        diag::line_col(self.src, s.start).0 + 1
    }

    fn setting(&mut self, name: &str, name_span: Span, value: &Expr) {
        if let Some(prev) = self.seen_settings.get(name) {
            let (l1, l2) = (self.line(*prev), self.line(name_span));
            self.diags.push(
                Diagnostic::error(
                    "E204",
                    name_span,
                    Text2::new(
                        format!("The setting \"{name}\" is set twice (lines {l1} and {l2})."),
                        format!("Die Einstellung \"{name}\" wird zweimal gesetzt (Zeilen {l1} und {l2})."),
                    ),
                )
                .why(
                    "Settings apply to the whole script, so each may appear only once.",
                    "Einstellungen gelten für das ganze Skript, deshalb darf jede nur einmal vorkommen.",
                ),
            );
            return;
        }
        self.seen_settings.insert(name.to_string(), name_span);
        if !apply_setting(&mut self.settings, name, value) {
            let (en, de, ex) = match name {
                "knut_db" => ("a file name in quotes", "einen Dateinamen in Anführungszeichen", "knut_db = 'sweden.knutdb'"),
                "seed" => ("a whole number", "eine ganze Zahl", "seed = 99"),
                "messages" => ("en or de", "en oder de", "messages = de"),
                "ignore_warnings" => ("true or false", "true oder false", "ignore_warnings = true"),
                _ => ("a length", "eine Länge", "near_distance = 100 m"),
            };
            self.diags.push(
                Diagnostic::error(
                    "E215",
                    value.span,
                    Text2::new(format!("The setting \"{name}\" expects {en}."), format!("Die Einstellung \"{name}\" erwartet {de}.")),
                )
                .header(Header::ForExample)
                .sugg(ex),
            );
        }
    }

    // ----- names ---------------------------------------------------------------------

    fn lookup_local(&self, n: &str) -> Option<Ty> {
        for s in self.scopes.iter().rev() {
            for (k, t) in s.iter().rev() {
                if k == n {
                    return Some(t.clone());
                }
            }
        }
        None
    }

    fn resolve(&mut self, e: &Expr, name: &str, pos: Pos) -> Ty {
        let span = e.span;
        let (res, ty) = self.resolve_inner(name, span, pos);
        if let Some(r) = res {
            if pos == Pos::Value && matches!(r, IdentRes::Global(_) | IdentRes::Local(_)) {
                self.diags.push(
                    Diagnostic::warning(
                        "W006",
                        span,
                        Text2::new(
                            format!("\"{name}\" is a variable here, not the tag value '{name}'."),
                            format!("\"{name}\" ist hier eine Variable, nicht der Tag-Wert '{name}'."),
                        ),
                    )
                    .why("If you mean the tag value, put it in quotes:", "Ist der Tag-Wert gemeint, gehört er in Anführungszeichen:")
                    .sugg(format!("'{name}'")),
                );
            }
            if matches!(r, IdentRes::Global(_) | IdentRes::Local(_)) {
                self.var_words.push((name.to_string(), span));
            }
            self.idents.insert(span, r);
        }
        ty
    }

    fn resolve_inner(&mut self, name: &str, span: Span, pos: Pos) -> (Option<IdentRes>, Ty) {
        if let Some(t) = self.lookup_local(name) {
            return (Some(IdentRes::Local(name.into())), t);
        }
        if let Some(g) = self.globals.get(name) {
            return (Some(IdentRes::Global(name.into())), g.ty.clone());
        }
        if SETTINGS.contains(&name) && pos == Pos::Normal {
            let t = if name == "seed" { Ty::Num(Dim::NONE) } else { Ty::Any };
            return (Some(IdentRes::Setting(name.into())), t);
        }
        // projection `war_B`
        if let Some(i) = name.rfind('_') {
            if i > 0 && i + 1 < name.len() {
                let (base, p) = (&name[..i], &name[i + 1..]);
                let tuple = self.lookup_local(base).or_else(|| self.globals.get(base).map(|g| g.ty.clone()));
                if let Some(Ty::Set(t)) = &tuple {
                    if let Ty::Tuple(items) = &**t {
                        if let Some(k) = items.iter().position(|(v, _)| v == p) {
                            let kind = items[k].1;
                            return (Some(IdentRes::Projection { var: base.into(), pos: k }), Ty::Set(Box::new(Ty::Elem(Some(kind)))));
                        }
                    }
                }
                // one match of a sequence: `x_B` is a single element
                if let Some(Ty::Tuple(items)) = &tuple {
                    if let Some(k) = items.iter().position(|(v, _)| v == p) {
                        let kind = items[k].1;
                        return (Some(IdentRes::Projection { var: base.into(), pos: k }), Ty::Elem(Some(kind)));
                    }
                }
            }
        }
        match pos {
            Pos::Value => return (Some(IdentRes::Text(name.into())), Ty::Text),
            Pos::Region => {
                self.needs_db = true;
                return (Some(IdentRes::Region(name.into())), Ty::Region);
            }
            _ => {}
        }
        if self.implicit > 0 && !KEYWORDS.contains(&name) {
            return (Some(IdentRes::ImplicitField(name.into())), Ty::Any);
        }
        if let Some((kind, true)) = Kind::parse(name) {
            self.needs_db = true;
            self.kind_caps(kind, span);
            // `count of nodes`: every element of the whole database (W003)
            if kind.is_osm() {
                if let Some(caps) = self.caps {
                    if caps.extent_km2 > 10_000.0 {
                        let a = caps.extent_km2;
                        self.diags.push(
                            Diagnostic::warning(
                                "W003",
                                span,
                                Text2::new(
                                    format!("This selects all {} of a very large region without a filter.", kind.plural()),
                                    format!("Das wählt alle {} einer sehr großen Region ohne Filter aus.", kind.plural()),
                                ),
                            )
                            .why(
                                format!("The region covers about {} km², so this can take long.", fmt_int(a)),
                                format!("Die Region umfasst etwa {} km², das kann lange dauern.", fmt_int(a)),
                            )
                            .header(Header::YouCan)
                            .sugg_label("add a condition:", "eine Bedingung ergänzen:", format!("{name} x with …"))
                            .sugg_label("run anyway:", "trotzdem ausführen:", "ignore_warnings = true"),
                        );
                    }
                }
            }
            return (Some(IdentRes::AllOfKind(kind)), Ty::Set(Box::new(Ty::Elem(Some(kind)))));
        }
        // undefined
        let code = if pos == Pos::Net { "E202" } else { "E201" };
        let mut d = Diagnostic::error(code, span, Text2::new(format!("\"{name}\" is not defined."), format!("\"{name}\" ist nicht definiert.")));
        if let Some(sp) = self.later.get(name).map(|(_, sp)| *sp).filter(|sp| sp.start > span.start) {
            let l = self.line(sp);
            d = d.why(
                format!("It is defined later, on line {l}. Names must be defined before they are used."),
                format!("Es wird erst später definiert, in Zeile {l}. Namen müssen vor ihrer Verwendung definiert sein."),
            );
        } else {
            let mut cands: Vec<(&str, usize)> = self.globals.iter().map(|(k, g)| (k.as_str(), g.line)).collect();
            let locals: Vec<String> = self.scopes.iter().flatten().map(|(k, _)| k.clone()).collect();
            for l in locals.iter().filter(|l| l.as_str() != "_") {
                cands.push((l.as_str(), 0));
            }
            if let Some(best) = diag::closest(name, cands.iter().map(|c| c.0)) {
                let line = cands.iter().find(|c| c.0 == best).map(|c| c.1).unwrap_or(0);
                if line > 0 {
                    d = d.why(
                        format!("Did you mean \"{best}\"? (defined on line {line})"),
                        format!("Meintest du \"{best}\"? (definiert in Zeile {line})"),
                    );
                } else {
                    d = d.why(format!("Did you mean \"{best}\"?"), format!("Meintest du \"{best}\"?"));
                }
            }
        }
        self.diags.push(d);
        (None, Ty::Any)
    }

    fn kind_caps(&mut self, kind: Kind, span: Span) {
        let Some(caps) = self.caps else { return };
        if kind == Kind::Note && !caps.notes {
            self.diags.push(
                Diagnostic::error(
                    "E206",
                    span,
                    Text2::new("This needs the notes index, which does not exist yet.", "Dafür braucht es den Notes-Index, den es noch nicht gibt."),
                )
                .why(
                    "Notes are not part of the history file. Build the index once from a notes dump:",
                    "Notes sind nicht Teil der Historie. Der Index wird einmal aus einem Notes-Dump gebaut:",
                )
                .sugg(format!("knut index notes {} planet-notes-latest.osn.bz2", caps.path)),
            );
        }
        if kind == Kind::User && !caps.users {
            self.e207(span);
        }
    }

    fn e207(&mut self, span: Span) {
        self.diags.push(
            Diagnostic::error(
                "E207",
                span,
                Text2::new("This database contains no user information.", "Diese Datenbank enthält keine Angaben zu Nutzer:innen."),
            )
            .why("It was built with --strip-users.", "Sie wurde mit --strip-users gebaut."),
        );
    }

    fn e203(&mut self, span: Span, f: &str, kind: Option<Kind>) {
        let kname = kind.map_or("elements", |k| k.plural());
        let kde = kind.map_or("Elementen", |k| match k {
            Kind::User => "Nutzer:innen",
            Kind::Changeset => "Changesets",
            Kind::Note => "Notes",
            Kind::Key => "Schlüsseln",
            _ => "OSM-Elementen",
        });
        let mut d = Diagnostic::error(
            "E203",
            span,
            Text2::new(format!("\"{f}\" is not a field of {kname}."), format!("\"{f}\" ist kein Feld von {kde}.")),
        );
        if f == "home" {
            d = d
                .why(
                    "There is no home location per person, on purpose: it would make\nindividuals identifiable.",
                    "Einen Wohnort pro Person gibt es absichtlich nicht: Er würde\nEinzelne identifizierbar machen.",
                )
                .header(Header::YouCan);
            d.header = Some(Text2::new("You can ask about the share of changes instead:", "Stattdessen lässt sich nach dem Anteil der Änderungen fragen:"));
            d = d.sugg("users u with share of u.changesets within AT > 0.9");
        } else {
            let mut names: Vec<String> = types::all_fields(kind).iter().map(|s| s.to_string()).collect();
            for d in &self.defines {
                if kind.is_none() || d.kind == Kind::Any || Some(d.kind) == kind {
                    names.push(d.field.clone());
                }
            }
            if let Some(b) = diag::closest(f, names.iter().map(|s| s.as_str())) {
                d = d.why(format!("Did you mean \".{b}\"?"), format!("Meintest du \".{b}\"?"));
            }
        }
        self.diags.push(d);
    }

    // ----- inference -------------------------------------------------------------------

    fn infer(&mut self, e: &Expr) -> Ty {
        self.infer_pos(e, Pos::Normal)
    }

    fn infer_pos(&mut self, e: &Expr, pos: Pos) -> Ty {
        match &e.kind {
            ExprKind::Num(_) => Ty::Num(Dim::NONE),
            ExprKind::Str(_) => Ty::Text,
            ExprKind::Date(_) => Ty::Time,
            ExprKind::None => Ty::None,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Ident(n) => self.resolve(e, n, pos),
            ExprKind::Underscore => self.lookup_local("_").unwrap_or(Ty::Any),
            ExprKind::List(xs) => {
                let mut t = Ty::Any;
                for (i, x) in xs.iter().enumerate() {
                    let xt = self.infer_pos(x, if pos == Pos::Value { Pos::Value } else { Pos::Normal });
                    t = if i == 0 { xt } else { t.join(&xt) };
                }
                Ty::List(Box::new(t))
            }
            ExprKind::BoxLit(_) => Ty::Region,
            ExprKind::Unit(inner, u, us) => {
                let word = self.text(*us).to_string();
                self.unit_words.push((word, *us));
                let _ = self.infer(inner);
                Ty::Num(u.dim)
            }
            ExprKind::Field(b, f, fs) => self.field(b, f, *fs),
            ExprKind::Tag(b, k, v) => {
                let bt = self.infer(b);
                if let TagKey::Expr(k) = k {
                    self.infer(k);
                }
                if let Ty::Elem(Some(Kind::User | Kind::Key)) = bt {
                    // users and keys have no tags
                }
                if matches!(bt, Ty::Set(_) | Ty::List(_)) {
                    // `libraries:[K]` – subset with the key
                    return bt;
                }
                if v.is_some() { Ty::Bool } else { Ty::TagText }
            }
            ExprKind::Call(f, args) => {
                let ft = match &f.kind {
                    ExprKind::Field(b, name, _) => {
                        let bt = self.infer(b);
                        Some((bt, name.clone()))
                    }
                    _ => {
                        self.infer(f);
                        None
                    }
                };
                for a in args {
                    self.infer_lambda_arg(a, ft.as_ref().map(|x| x.0.elem()).unwrap_or(Ty::Any));
                }
                match ft {
                    Some((Ty::Set(_), n)) if n == "map" => Ty::Set(Box::new(Ty::Any)),
                    Some((Ty::List(_), n)) if n == "map" => Ty::List(Box::new(Ty::Any)),
                    Some((Ty::Grouped, n)) if n == "map" => Ty::Grouped,
                    Some((_, n)) if n == "map" => Ty::Any,
                    Some((Ty::Set(t), n)) if n == "filter" => Ty::Set(t),
                    Some((Ty::List(t), n)) if n == "filter" => Ty::List(t),
                    _ => Ty::Any,
                }
            }
            ExprKind::Neg(x) => {
                let t = self.infer(x);
                if matches!(t, Ty::Num(_) | Ty::NumAny) { t } else { Ty::NumAny }
            }
            ExprKind::Not(x) => {
                self.cond(x);
                Ty::Bool
            }
            ExprKind::Bin(op, a, b) => self.binop(e, *op, a, b),
            ExprKind::And(a, b) | ExprKind::Or(a, b) => {
                self.cond(a);
                self.cond(b);
                Ty::Bool
            }
            ExprKind::Fallback(a, b) => {
                let ta = self.infer(a);
                let tb = self.infer(b);
                ta.join(&tb)
            }
            ExprKind::Rel(l, t) => self.rel(e, l, t),
            ExprKind::Lambda(ps, body) => {
                self.scopes.push(ps.iter().map(|p| (p.clone(), Ty::Any)).collect());
                for p in ps {
                    self.var_words.push((p.clone(), e.span));
                }
                self.infer(body);
                self.scopes.pop();
                Ty::Lambda
            }
            ExprKind::Select(s) => self.selection(e, s),
            ExprKind::Quantified { q, kind, var, var_span, set, pred, cond } => {
                let st = self.infer(set);
                let et = match kind {
                    Some(k) => Ty::Elem(Some(*k)),
                    None => st.elem(),
                };
                self.var_words.push((var.clone(), *var_span));
                self.scopes.push(vec![(var.clone(), et.clone())]);
                if let Some(p) = pred {
                    self.cond(p);
                }
                if let Some(c) = cond {
                    self.cond(c);
                    self.w001_scan(c);
                }
                self.scopes.pop();
                match q {
                    QKind::The => et,
                    QKind::All => Ty::Set(Box::new(et)),
                    QKind::Some => Ty::Bool,
                    QKind::Groups => Ty::Set(Box::new(Ty::Group)),
                    QKind::Pick { n: None, .. } => et,
                    QKind::Pick { .. } => Ty::List(Box::new(et)),
                }
            }
            ExprKind::Of { func, func_span, operand, clauses } => self.of_call(e, *func, *func_span, operand, clauses),
            ExprKind::Special(s) => self.special(s),
        }
    }

    fn infer_lambda_arg(&mut self, a: &Expr, elem: Ty) {
        if let ExprKind::Lambda(ps, body) = &a.kind {
            self.scopes.push(ps.iter().map(|p| (p.clone(), elem.clone())).collect());
            for p in ps {
                self.var_words.push((p.clone(), a.span));
            }
            self.infer(body);
            self.scopes.pop();
        } else {
            self.infer(a);
        }
    }

    fn field(&mut self, b: &Expr, f: &str, fs: Span) -> Ty {
        let bt = self.infer(b);
        // user-defined fields
        let kind = match &bt {
            Ty::Elem(k) => *k,
            _ => None,
        };
        if let Some(d) = self.defines.iter().find(|d| d.field == f && (d.kind == Kind::Any || kind.is_none() || Some(d.kind) == kind)) {
            let _ = d;
            return Ty::Any;
        }
        if f == "user" || f == "creator" {
            self.needs_db = true;
            if let Some(c) = self.caps {
                if !c.users {
                    self.e207(fs);
                }
            }
        }
        match &bt {
            Ty::Elem(k) => match types::elem_field(*k, f) {
                Some(t) => t,
                None => {
                    if f == "home" || k.is_some() {
                        self.e203(fs, f, *k);
                    }
                    Ty::Any
                }
            },
            Ty::TagText => match types::text_field(f) {
                Some(t) => t,
                None => {
                    // E103: a dot ends the tag key
                    let tag_text = self.text(b.span).to_string();
                    let (base, key) = tag_text.split_once(':').unwrap_or(("x", &tag_text));
                    let mut d = Diagnostic::error("E103", fs.to(b.span).to(fs), Text2::new("A dot ends a tag key.", "Ein Punkt beendet einen Tag-Schlüssel."));
                    d.span = Span { start: fs.start.saturating_sub(1), end: fs.end };
                    d = d.why(
                        format!("\"{tag_text}\" was read as the tag, and \".{f}\" as a computed field.\nIf the key itself contains a dot, put it in brackets:"),
                        format!("\"{tag_text}\" wurde als Tag gelesen und \".{f}\" als berechnetes Feld.\nEnthält der Schlüssel selbst einen Punkt, gehört er in Klammern:"),
                    );
                    d = d.sugg(format!("{base}:['{key}.{f}']"));
                    self.diags.push(d);
                    Ty::Any
                }
            },
            Ty::Text => types::text_field(f).unwrap_or(Ty::Any),
            Ty::Set(_) | Ty::List(_) => match f {
                "count" => Ty::Num(Dim::NONE),
                "first" | "last" => bt.elem(),
                _ => Ty::Any,
            },
            Ty::Grouped => match f {
                "count" => Ty::Num(Dim::NONE),
                "keys" | "values" => Ty::List(Box::new(Ty::Any)),
                _ => Ty::Any,
            },
            Ty::Group => match f {
                "count" => Ty::Num(Dim::NONE),
                "items" => Ty::Set(Box::new(Ty::Any)),
                _ => Ty::Any,
            },
            Ty::Path => match f {
                "length" => Ty::Num(Dim::LENGTH),
                "duration" => Ty::Num(Dim::TIME),
                "edges" => Ty::List(Box::new(Ty::Elem(Some(Kind::Way)))),
                _ => Ty::Any,
            },
            Ty::Region => match f {
                "area" => Ty::Num(Dim::AREA),
                "length" => Ty::Num(Dim::LENGTH),
                _ => Ty::Any,
            },
            _ => Ty::Any,
        }
    }

    fn cond(&mut self, e: &Expr) {
        let t = self.infer(e);
        if matches!(t, Ty::Num(_) | Ty::NumAny | Ty::Set(_) | Ty::List(_) | Ty::Grouped | Ty::Record | Ty::Region | Ty::Net | Ty::Path) {
            let what = t.describe(false);
            let wde = t.describe(true);
            self.diags.push(
                Diagnostic::error(
                    "E605",
                    e.span,
                    Text2::new(format!("A condition must be true or false, but this is {what}."), format!("Eine Bedingung muss wahr oder falsch sein, das hier ist aber {wde}.")),
                )
                .why("Compare it with something, for example:", "Vergleiche es mit etwas, zum Beispiel:")
                .sugg(format!("{} > 0", self.text(e.span))),
            );
        }
    }

    fn binop(&mut self, e: &Expr, op: BinOp, a: &Expr, b: &Expr) -> Ty {
        let ta = self.infer(a);
        let tb = self.infer(b);
        match op {
            BinOp::Add | BinOp::Sub => match (&ta, &tb) {
                (Ty::Time, Ty::Time) if op == BinOp::Sub => Ty::Num(Dim::TIME),
                (Ty::Time, Ty::Num(d)) if *d == Dim::TIME => Ty::Time,
                (Ty::Time, Ty::Num(d)) | (Ty::Num(d), Ty::Time) if *d != Dim::TIME => {
                    self.e306(e, &Dim::TIME, d, op);
                    Ty::Any
                }
                (Ty::Num(da), Ty::Num(db)) => {
                    if da != db {
                        self.e306(e, da, db, op);
                    }
                    Ty::Num(*da)
                }
                (Ty::Num(d), _) | (_, Ty::Num(d)) => Ty::Num(*d),
                _ => Ty::NumAny,
            },
            BinOp::Mul | BinOp::Div => match (ta.dim(), tb.dim()) {
                (Some(x), Some(y)) => {
                    let r = if op == BinOp::Mul { x.mul(y) } else { x.div(y) };
                    match r {
                        Some(d) => Ty::Num(d),
                        None => Ty::NumAny,
                    }
                }
                (Some(x), None) if op == BinOp::Mul => {
                    if matches!(tb, Ty::TagText) {
                        Ty::Num(x)
                    } else {
                        Ty::NumAny
                    }
                }
                (None, Some(y)) if op == BinOp::Mul && matches!(ta, Ty::TagText) => Ty::Num(y),
                _ => Ty::NumAny,
            },
        }
    }

    fn e306(&mut self, e: &Expr, a: &Dim, b: &Dim, op: BinOp) {
        let (an, bn) = (a.a_name(), b.a_name());
        let (ad, bd) = (a.name(true), b.name(true));
        let verb = if op == BinOp::Add { ("added", "addiert") } else { ("subtracted", "subtrahiert") };
        self.diags.push(
            Diagnostic::error(
                "E306",
                e.span,
                Text2::new(
                    format!("{} and {bn} cannot be {}.", cap(&an), verb.0),
                    format!("{ad} und {bd} lassen sich nicht {}.", verb.1),
                ),
            )
            .why(
                "Units can be mixed within one kind of measure (300 m + 1 km),\nbut not across kinds.",
                "Einheiten lassen sich innerhalb eines Maßes mischen (300 m + 1 km),\naber nicht zwischen verschiedenen Maßen.",
            ),
        );
    }

    fn is_tag_operand(e: &Expr) -> Option<(&Expr, &str)> {
        match &e.kind {
            ExprKind::Tag(b, TagKey::Lit(k), None) => Some((b, k.as_str())),
            ExprKind::Unit(x, _, _) => Self::is_tag_operand(x),
            _ => None,
        }
    }

    /// Variable whose tag a condition reads (`n` in `n:amenity=bench`).
    fn tag_base(e: &Expr) -> Option<&str> {
        match &e.kind {
            ExprKind::Tag(b, ..) | ExprKind::Field(b, ..) => match &b.kind {
                ExprKind::Ident(v) => Some(v),
                _ => Self::tag_base(b),
            },
            ExprKind::Rel(x, _) | ExprKind::Unit(x, ..) | ExprKind::Not(x) => Self::tag_base(x),
            _ => None,
        }
    }

    /// E608: the left side of `near`, `within` or `overlapping` must be an element, not a
    /// condition (`n:amenity=bench near …` instead of `n:amenity=bench and n near …`).
    fn spatial_left(&mut self, whole: &Expr, l: &Expr, lt: &Ty, kw: &str) {
        if *lt != Ty::Bool {
            return;
        }
        let ltxt = self.text(l.span);
        let rest = self.text(Span { start: l.span.end, end: whole.span.end }).trim();
        let mut d = Diagnostic::error(
            "E608",
            l.span,
            Text2::new(format!("\"{kw}\" needs an element on its left, but this is a condition."), format!("\"{kw}\" braucht links ein Element, das hier ist aber eine Bedingung.")),
        );
        if let Some(v) = Self::tag_base(l) {
            d = d.header(Header::DidYouMean).sugg(format!("{ltxt} and {v} {rest}"));
        }
        self.diags.push(d);
    }

    fn rel(&mut self, whole: &Expr, l: &Expr, t: &RelTail) -> Ty {
        match t {
            RelTail::Cmp(op, r) => {
                let l_tag = Self::is_tag_operand(l).is_some();
                let r_tag = Self::is_tag_operand(r).is_some();
                let lt = self.infer_pos(l, if r_tag { Pos::Value } else { Pos::Normal });
                let rt = self.infer_pos(r, if l_tag { Pos::Value } else { Pos::Normal });
                self.cmp_units(whole, l, r, &lt, &rt, *op);
                Ty::Bool
            }
            RelTail::In(r) => {
                let l_tag = Self::is_tag_operand(l).is_some();
                self.infer(l);
                if let (true, ExprKind::List(xs)) = (l_tag, &r.kind) {
                    for x in xs {
                        self.infer_pos(x, Pos::Value);
                    }
                } else {
                    self.infer(r);
                }
                Ty::Bool
            }
            RelTail::Between(a, b) => {
                self.infer(l);
                self.infer(a);
                self.infer(b);
                Ty::Bool
            }
            RelTail::Within(r) => {
                let lt = self.infer(l);
                self.spatial_left(whole, l, &lt, "within");
                self.infer_pos(r, Pos::Region);
                Ty::Bool
            }
            RelTail::Overlapping(r) => {
                let lt = self.infer(l);
                self.spatial_left(whole, l, &lt, "overlapping");
                self.infer(r);
                Ty::Bool
            }
            RelTail::Near { set, threshold, .. } => {
                let lt = self.infer(l);
                self.spatial_left(whole, l, &lt, "near");
                let st = self.infer(set);
                if matches!(st, Ty::Num(_) | Ty::NumAny | Ty::Text | Ty::Bool | Ty::Time) {
                    self.diags.push(Diagnostic::error(
                        "E601",
                        set.span,
                        Text2::new("\"near\" needs a set after \"in\", but this is a single value.", "\"near\" braucht nach \"in\" eine Menge, das ist aber ein einzelner Wert."),
                    ));
                }
                self.scopes.push(vec![("_".to_string(), st.elem())]);
                self.scopes.pop();
                if let Some(th) = threshold {
                    self.threshold(th, "near");
                }
                Ty::Bool
            }
            RelTail::NextOf(r) | RelTail::PreviousOf(r) => {
                self.infer(l);
                self.infer(r);
                Ty::Bool
            }
            RelTail::Before(r) | RelTail::After(r) => {
                let d = self.infer(l);
                self.infer(r);
                if let Ty::Num(x) = d {
                    if x != Dim::TIME {
                        self.diags.push(Diagnostic::error(
                            "E303",
                            l.span,
                            Text2::new("\"before\" and \"after\" need a duration, such as 30 days.", "\"before\" und \"after\" brauchen eine Dauer, etwa 30 days."),
                        ));
                    }
                }
                Ty::Time
            }
        }
    }

    fn cmp_units(&mut self, whole: &Expr, l: &Expr, r: &Expr, lt: &Ty, rt: &Ty, op: CmpOp) {
        let (Some(dl), Some(dr)) = (lt.dim(), rt.dim()) else { return };
        if dl == dr {
            return;
        }
        // E302: plain number on the right of a quantity
        if dr == Dim::NONE && matches!(r.kind, ExprKind::Num(_)) {
            let unit = units::display_unit(dl).map_or("m", |u| match u.name {
                "s" => "min",
                "kg" => "t",
                n => n,
            });
            self.diags.push(
                Diagnostic::error(
                    "E302",
                    r.span,
                    Text2::new(format!("\"{}\" needs a unit.", self.text(r.span)), format!("\"{}\" braucht eine Einheit.", self.text(r.span))),
                )
                .why(
                    format!("The left side is {}, so the number on the right needs a unit too.", dl.a_name()),
                    format!("Links steht eine {}, deshalb braucht auch die Zahl rechts eine Einheit.", dl.name(true)),
                )
                .header(Header::DidYouMean)
                .sugg(format!("{} {} {} {unit}", self.text(l.span), op.symbol(), self.text(r.span))),
            );
            return;
        }
        if dl == Dim::NONE {
            return;
        }
        // E305: unit after the tag has the wrong measure
        if let ExprKind::Unit(inner, u, us) = &l.kind {
            if matches!(inner.kind, ExprKind::Tag(..)) {
                let want = match &r.kind {
                    ExprKind::Unit(_, ru, _) => ru.name,
                    _ => units::display_unit(dr).map_or("m", |u| u.name),
                };
                self.diags.push(
                    Diagnostic::error(
                        "E305",
                        *us,
                        Text2::new(
                            format!("\"{}\" is {}, but {} is expected here.", u.name, dl.a_name(), dr.a_name()),
                            format!("\"{}\" ist eine {}, hier wird aber eine {} erwartet.", u.name, dl.name(true), dr.name(true)),
                        ),
                    )
                    .header(Header::DidYouMean)
                    .sugg(format!("{} {want} {} {}", self.text(inner.span), op.symbol(), self.text(r.span))),
                );
                return;
            }
        }
        self.diags.push(
            Diagnostic::error("E303", whole.span, Text2::new("This unit does not fit here.", "Diese Einheit passt hier nicht."))
                .why(
                    format!("{} cannot be compared with {}.", cap(&dl.a_name()), dr.a_name()),
                    format!("{} lässt sich nicht mit {} vergleichen.", cap(&dl.name(true)), dr.name(true)),
                ),
        );
    }

    fn threshold(&mut self, th: &Threshold, what: &str) {
        let t = self.infer(&th.value);
        if let Some(m) = &th.mode {
            if let Some(u) = &m.using {
                self.infer_pos(u, Pos::Net);
            }
        }
        let Some(d) = t.dim() else { return };
        let vt = self.text(th.value.span).to_string();
        let has_mode = th.mode.is_some();
        let mode = th.mode.as_ref().map_or("foot", |m| m.mode.name());
        if d == Dim::NONE {
            let mut dg = Diagnostic::error(
                "E302",
                th.value.span,
                Text2::new(format!("\"closer than {vt}\" needs a unit."), format!("\"closer than {vt}\" braucht eine Einheit.")),
            )
            .header(Header::DidYouMean);
            if what == "sequence" {
                dg = dg.sugg(format!("closer than {vt} days"));
            } else {
                dg = dg
                    .sugg_note(format!("closer than {vt} min by {mode}"), "time on the network", "Zeit im Netz")
                    .sugg_note(format!("closer than {vt} m"), "distance, straight line", "Luftlinie");
            }
            self.diags.push(dg);
            return;
        }
        let ok = match what {
            "sequence" => d == Dim::TIME,
            "isochrone" => d == Dim::TIME || d == Dim::LENGTH,
            _ => d == Dim::LENGTH || (d == Dim::TIME && has_mode),
        };
        if ok {
            let calendar_near =
                what == "near" && d == Dim::TIME && matches!(&th.value.kind, ExprKind::Unit(_, u, _) if u.factor >= 86_400.0);
            if !calendar_near {
                return;
            }
        }
        let num = match &th.value.kind {
            ExprKind::Unit(n, _, _) => self.text(n.span).to_string(),
            _ => vt.clone(),
        };
        let mut dg = Diagnostic::error("E303", th.value.span, Text2::new("This unit does not fit here.", "Diese Einheit passt hier nicht."));
        dg = match what {
            "sequence" => dg
                .why("A sequence is limited by time, such as 30 days.", "Eine Sequenz wird durch eine Zeit begrenzt, etwa 30 days.")
                .header(Header::DidYouMean)
                .sugg(format!("closer than {num} days")),
            _ => dg
                .why(
                    "\"near\" measures distance or travel time, not calendar time.",
                    "\"near\" misst Abstand oder Reisezeit, keine Kalenderzeit.",
                )
                .header(Header::DidYouMean)
                .sugg(format!("closer than {num} min by {mode}")),
        };
        self.diags.push(dg);
    }

    fn selection(&mut self, e: &Expr, s: &Selection) -> Ty {
        let src_ty = s.source.as_ref().map(|x| self.infer(x));
        if s.source.is_none() {
            self.needs_db = true;
        }
        if s.pattern.len() > 1 {
            let k0 = s.pattern[0].kind;
            for p in &s.pattern[1..] {
                if p.kind != k0 {
                    self.diags.push(
                        Diagnostic::error(
                            "E604",
                            p.kind_span,
                            Text2::new(
                                format!("All positions of a sequence must be the same kind, but here {} follows {}.", p.kind.name(), k0.name()),
                                format!("Alle Positionen einer Sequenz müssen dieselbe Sorte haben, hier folgt aber {} auf {}.", p.kind.name(), k0.name()),
                            ),
                        )
                        .why("\"->\" connects consecutive versions of the same object.", "\"->\" verbindet aufeinanderfolgende Versionen desselben Objekts."),
                    );
                }
            }
        }
        for p in &s.pattern {
            self.kind_caps(p.kind, p.kind_span);
        }
        if let Some(th) = &s.seq_threshold {
            self.threshold(th, "sequence");
        }
        let mut scope = Vec::new();
        for p in &s.pattern {
            let t = match (&src_ty, p.kind) {
                (Some(st), Kind::Any) => st.elem(),
                _ => Ty::Elem(Some(p.kind)),
            };
            if p.var != "_" {
                self.var_words.push((p.var.clone(), p.var_span));
            }
            scope.push((p.var.clone(), t));
        }
        self.scopes.push(scope);
        if let Some(v) = &s.var_pred {
            self.cond(v);
        }
        if let Some(c) = &s.cond {
            self.cond(c);
            self.w001_scan(c);
        }
        if let Some(o) = &s.order {
            self.infer(o);
        }
        self.scopes.pop();
        if let Some(w) = &s.within {
            self.infer_pos(w, Pos::Region);
        }
        match &s.time {
            Some(TimeRef::On(d)) => {
                self.infer(d);
            }
            Some(TimeRef::Range(a, b)) | Some(TimeRef::OnEach { range: Some((a, b)), .. }) => {
                self.infer(a);
                self.infer(b);
            }
            _ => {}
        }
        // W003
        if let Some(caps) = self.caps {
            if s.source.is_none() && s.cond.is_none() && s.var_pred.is_none() && s.pick.is_none() && s.pattern.len() == 1 && s.pattern[0].kind.is_osm() && caps.extent_km2 > 10_000.0 {
                let region_km2 = match s.within.as_deref().map(|w| &w.kind) {
                    None => Some(caps.extent_km2),
                    Some(ExprKind::BoxLit(b)) => Some(box_km2(b)),
                    _ => None,
                };
                if let Some(a) = region_km2 {
                    if a > 10_000.0 {
                        let k = s.pattern[0].kind;
                        self.diags.push(
                            Diagnostic::warning(
                                "W003",
                                e.span,
                                Text2::new(
                                    format!("This selects all {} of a very large region without a filter.", k.plural()),
                                    format!("Das wählt alle {} einer sehr großen Region ohne Filter aus.", k.plural()),
                                ),
                            )
                            .why(
                                format!("The region covers about {} km², so this can take long.", fmt_int(a)),
                                format!("Die Region umfasst etwa {} km², das kann lange dauern.", fmt_int(a)),
                            )
                            .header(Header::YouCan)
                            .sugg_label("add a condition:", "eine Bedingung ergänzen:", format!("{} with …", self.text(e.span)))
                            .sugg_label("run anyway:", "trotzdem ausführen:", "ignore_warnings = true"),
                        );
                    }
                }
            }
        }
        let elem = if s.pattern.len() > 1 {
            Ty::Tuple(s.pattern.iter().map(|p| (p.var.clone(), p.kind)).collect())
        } else {
            match (&src_ty, s.pattern[0].kind) {
                (Some(st), Kind::Any) => st.elem(),
                (_, k) => Ty::Elem(Some(k)),
            }
        };
        if let Some(TimeRef::OnEach { .. }) = s.time {
            return Ty::Grouped;
        }
        match &s.pick {
            Some(Pick { n: None, .. }) if !s.pattern[0].plural => elem,
            Some(_) => Ty::List(Box::new(elem)),
            None => Ty::Set(Box::new(elem)),
        }
    }

    fn of_call(&mut self, e: &Expr, func: OfFunc, fsp: Span, operand: &OfOperand, cl: &Clauses) -> Ty {
        let (set_ty, vars): (Ty, Vec<(String, Ty)>) = match operand {
            OfOperand::Iter { var, var_span, set, pred, cond } => {
                let st = self.infer(set);
                let et = st.elem();
                self.var_words.push((var.clone(), *var_span));
                self.scopes.push(vec![(var.clone(), et.clone())]);
                if let Some(p) = pred {
                    self.cond(p);
                }
                if let Some(c) = cond {
                    self.cond(c);
                    self.w001_scan(c);
                }
                self.scopes.pop();
                (st, vec![(var.clone(), et)])
            }
            OfOperand::Expr(x) => {
                let t = self.infer(x);
                let names = elem_vars_of(x, &self.globals, &self.idents);
                let et = t.elem();
                (t, names.into_iter().map(|n| (n, et.clone())).collect())
            }
        };
        // E601
        if func.is_aggregate() && !matches!(func, OfFunc::Days | OfFunc::Weeks | OfFunc::Months | OfFunc::Years) {
            if let OfOperand::Expr(x) = operand {
                if matches!(set_ty, Ty::Elem(_)) {
                    let mut d = Diagnostic::error(
                        "E601",
                        x.span,
                        Text2::new(
                            format!("\"{} of\" expects a set, but got a single element.", func.name()),
                            format!("\"{} of\" erwartet eine Menge, hat aber ein einzelnes Element bekommen.", func.name()),
                        ),
                    );
                    if let ExprKind::Select(s) = &x.kind {
                        if let Some(p) = &s.pick {
                            let fname = func.name();
                            let k = &s.pattern[0];
                            let after_kind = &self.src[k.kind_span.end as usize..x.span.end as usize];
                            let head = &self.src[p.span.start as usize..k.kind_span.start as usize];
                            let plural = k.kind.plural();
                            let open = if self.src[..x.span.start as usize].ends_with('(') { "(" } else { "" };
                            let close = if open.is_empty() { "" } else { ")" };
                            let rest = after_kind.trim_end_matches(')');
                            d = d
                                .header(Header::DidYouMean)
                                .sugg_note(format!("{fname} of ({head}{plural}{rest})"), "a list with one element", "eine Liste mit einem Element")
                                .sugg_note(format!("{fname} of ({plural}{rest})"), "all of them", "alle");
                            let _ = (open, close);
                        }
                    }
                    self.diags.push(d);
                }
            }
        }
        // clauses see the element variables
        let mut scope = vars.clone();
        if scope.is_empty() {
            scope.push(("_".into(), set_ty.elem()));
        }
        self.clause_vars.insert(e.span, scope.iter().map(|(n, _)| n.clone()).collect());
        self.scopes.push(scope);
        if let Some(w) = &cl.with {
            self.cond(w);
            self.w001_scan(w);
        }
        for (g, _) in &cl.per {
            match g {
                Grouping::Cell(x) => {
                    let t = self.infer(x);
                    if let Some(d) = t.dim() {
                        if d != Dim::LENGTH {
                            self.diags.push(Diagnostic::error(
                                "E303",
                                x.span,
                                Text2::new("\"per cell of\" needs a length, such as 1 km.", "\"per cell of\" braucht eine Länge, etwa 1 km."),
                            ));
                        }
                    }
                }
                Grouping::Expr(x) => {
                    if matches!(&x.kind, ExprKind::Ident(n) if n == "user") && self.lookup_local("user").is_none() && !self.globals.contains_key("user") {
                        self.idents.insert(x.span, IdentRes::ImplicitField("user".into()));
                        if let Some(c) = self.caps {
                            if !c.users {
                                self.e207(x.span);
                            }
                        }
                    } else {
                        self.infer(x);
                    }
                }
                Grouping::Chunk(_)
                    if cl.order.is_none() => {
                        let txt = self.text(e.span).to_string();
                        let var = vars.first().map_or("x".to_string(), |v| v.0.clone());
                        self.diags.push(
                            Diagnostic::error(
                                "E603",
                                g_span(cl, g),
                                Text2::new("\"per chunk\" needs an order.", "\"per chunk\" braucht eine Reihenfolge."),
                            )
                            .why("Elements have no natural order here.", "Elemente haben hier keine natürliche Reihenfolge.")
                            .header(Header::YouCan),
                        );
                        let d = self.diags.last_mut().unwrap();
                        d.header = Some(Text2::new("You can order them by time:", "Du kannst sie nach der Zeit ordnen:"));
                        d.suggestions.push(diag::Sugg { label: None, code: format!("{txt} ordered by {var}.time"), note: None });
                    }
                _ => {}
            }
        }
        if let Some(o) = &cl.order {
            self.infer(o);
        }
        if let Some(t) = &cl.to {
            self.infer(t);
        }
        self.scopes.pop();
        if let Some(h) = &cl.having {
            self.implicit += 1;
            self.scopes.push(vec![]);
            self.cond(h);
            self.scopes.pop();
            self.implicit -= 1;
        }
        if let Some(w) = &cl.within {
            self.infer_pos(w, Pos::Region);
        }
        match &cl.time {
            Some(TimeRef::On(d)) => {
                self.infer(d);
            }
            Some(TimeRef::Range(a, b)) | Some(TimeRef::OnEach { range: Some((a, b)), .. }) => {
                self.infer(a);
                self.infer(b);
            }
            _ => {}
        }
        if let Some(m) = &cl.mode {
            if let Some(u) = &m.using {
                self.infer_pos(u, Pos::Net);
            }
        }
        if func == OfFunc::User {
            if let Some(c) = self.caps {
                if !c.users {
                    self.e207(fsp);
                }
            }
        }
        let grouped = !cl.per.is_empty() || matches!(cl.time, Some(TimeRef::OnEach { .. })) || matches!(set_ty, Ty::Grouped) && func.is_aggregate();
        if grouped {
            return Ty::Grouped;
        }
        match func {
            OfFunc::Count => Ty::Num(Dim::NONE),
            OfFunc::Sum | OfFunc::Average => Ty::NumAny,
            OfFunc::Variance | OfFunc::Entropy | OfFunc::Gini | OfFunc::Density => Ty::Num(Dim::NONE),
            OfFunc::Share => Ty::Num(Dim::NONE),
            OfFunc::Spread => Ty::Num(Dim::LENGTH),
            OfFunc::Distribution | OfFunc::Transitions => Ty::Grouped,
            OfFunc::Streaks => Ty::List(Box::new(Ty::Num(Dim::NONE))),
            OfFunc::Distinct => Ty::Set(Box::new(Ty::Any)),
            OfFunc::Earliest | OfFunc::Latest => Ty::Any,
            OfFunc::Days | OfFunc::Weeks | OfFunc::Months | OfFunc::Years => Ty::Set(Box::new(Ty::Time)),
            OfFunc::User => Ty::Elem(Some(Kind::User)),
            OfFunc::FirstVersion | OfFunc::LastVersion => match set_ty {
                Ty::Elem(k) => Ty::Elem(k),
                _ => Ty::Elem(None),
            },
            OfFunc::Nearest => set_ty.elem(),
            OfFunc::Graph => Ty::Net,
        }
    }

    fn special(&mut self, s: &Special) -> Ty {
        match s {
            Special::Path { var, from, to, mode } => {
                self.infer(from);
                self.infer(to);
                if let Some(u) = &mode.using {
                    self.infer_pos(u, Pos::Net);
                }
                self.var_words.push((var.clone(), Span::default()));
                self.needs_db = true;
                Ty::Path
            }
            Special::Isochrone { var, from, threshold } => {
                self.infer(from);
                self.threshold(threshold, "isochrone");
                self.var_words.push((var.clone(), Span::default()));
                self.needs_db = true;
                Ty::Region
            }
            Special::Dataset(_) => Ty::Set(Box::new(Ty::Elem(None))),
            Special::Split { set, by, at } => {
                let st = self.infer(set);
                self.implicit += 1;
                self.scopes.push(vec![("_".into(), st.elem())]);
                self.infer(by);
                self.cond(at);
                self.scopes.pop();
                self.implicit -= 1;
                Ty::List(Box::new(st))
            }
            Special::Correlate(a, b) => {
                self.infer(a);
                self.infer(b);
                Ty::Num(Dim::NONE)
            }
            Special::Regress(a, b) => {
                self.infer(a);
                self.infer(b);
                Ty::Record
            }
            Special::Confusion(a, b) => {
                self.infer(a);
                self.infer(b);
                Ty::Grouped
            }
            Special::FitPowerlaw(a) => {
                self.infer(a);
                Ty::Record
            }
            Special::Forecast { of, .. } => {
                self.infer(of);
                Ty::Grouped
            }
            Special::Classify { set, label, features } => {
                let st = self.infer(set);
                self.infer_lambda_arg(label, st.elem());
                self.infer_lambda_arg(features, st.elem());
                // predictions, accuracy, trained on
                Ty::Record
            }
        }
    }

    // ----- warnings --------------------------------------------------------------------

    /// W001 (static part): comparisons in condition `c` that drop elements without the tag.
    fn w001_scan(&mut self, c: &Expr) {
        let mut handled: HashSet<String> = HashSet::new();
        c.walk(&mut |x| match &x.kind {
            ExprKind::Rel(l, RelTail::Cmp(CmpOp::Eq | CmpOp::Ne, r)) if matches!(r.kind, ExprKind::None) => {
                handled.insert(self.text(l.span).to_string());
            }
            ExprKind::Fallback(l, _) => {
                handled.insert(self.text(l.span).to_string());
                l.walk(&mut |y| {
                    if let ExprKind::Tag(..) = &y.kind {
                        handled.insert(self.text(y.span).to_string());
                    }
                });
            }
            _ => {}
        });
        // presence tests joined by `and`
        fn presence(e: &Expr, src: &str, out: &mut HashSet<String>) {
            match &e.kind {
                ExprKind::And(a, b) => {
                    presence(a, src, out);
                    presence(b, src, out);
                }
                ExprKind::Tag(_, _, None) => {
                    out.insert(src[e.span.start as usize..e.span.end as usize].to_string());
                }
                _ => {}
            }
        }
        presence(c, self.src, &mut handled);
        let mut sites = Vec::new();
        c.walk(&mut |x| {
            if let ExprKind::Rel(l, RelTail::Cmp(op, r)) = &x.kind {
                if matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge | CmpOp::Ne) && !matches!(r.kind, ExprKind::None) {
                    for side in [&**l, &**r] {
                        if let Some((base, key)) = Self::is_tag_operand(side) {
                            let tag_txt = match &side.kind {
                                ExprKind::Unit(t, _, _) => self.text(t.span).to_string(),
                                _ => self.text(side.span).to_string(),
                            };
                            if handled.contains(&tag_txt) {
                                continue;
                            }
                            sites.push((x.span, tag_txt, key.to_string(), base.span, self.text(x.span).to_string()));
                        }
                    }
                }
            }
        });
        for (span, tag_txt, key, base_span, cmp_txt) in sites {
            let kind = match self.idents.get(&base_span) {
                Some(IdentRes::Local(n)) => match self.lookup_local(n) {
                    Some(Ty::Elem(k)) => k,
                    _ => None,
                },
                _ => None,
            };
            let kp = kind.map_or("elements", |k| k.plural());
            self.diags.push(
                Diagnostic::warning(
                    "W001",
                    span,
                    Text2::new("A comparison with a missing value is always false.", "Ein Vergleich mit einem fehlenden Wert ist immer falsch."),
                )
                .why(format!("{} without {key} will be left out.", cap(kp)), format!("{} ohne {key} fallen weg.", cap(kp)))
                .header(Header::YouCan)
                .sugg_label("keep them:", "behalten:", format!("{cmp_txt} or {tag_txt} == none"))
                .sugg_label("run anyway:", "trotzdem ausführen:", "ignore_warnings = true"),
            );
            self.w001.push(W001Site { span, key, kind });
        }
    }

    fn check_w005(&mut self) {
        let mut seen = HashSet::new();
        for (w, us) in self.unit_words.clone() {
            if seen.contains(&w) {
                continue;
            }
            if let Some((_, vs)) = self.var_words.iter().find(|(v, _)| *v == w) {
                seen.insert(w.clone());
                let mut d = Diagnostic::warning(
                    "W005",
                    if vs.is_empty() { us } else { *vs },
                    Text2::new(format!("\"{w}\" is used both as a variable and as a unit."), format!("\"{w}\" wird zugleich als Variable und als Einheit verwendet.")),
                )
                .why(
                    format!("Within one statement, \"{w}\" after a number or a tag means a unit."),
                    format!("Innerhalb eines Statements bedeutet \"{w}\" nach einer Zahl oder einem Tag eine Einheit."),
                )
                .header(Header::Consider);
                // lambdas show the arrow, other bindings only the name
                let lambda = self.src.get(vs.end as usize..).map_or(false, |r| r.trim_start().starts_with("=>"));
                d = d.sugg(if lambda { format!("{} => …", better_name(&w)) } else { better_name(&w).to_string() });
                if !vs.is_empty() {
                    d = d.extra(us);
                }
                self.diags.push(d);
            }
        }
    }

    fn check_define_cycles(&mut self, script: &Script) {
        // field -> fields used in its body
        let fields: HashSet<String> = self.defines.iter().map(|d| d.field.clone()).collect();
        let mut uses: HashMap<String, Vec<String>> = HashMap::new();
        for d in &self.defines {
            if let StmtKind::Define { body, .. } = &script.stmts[d.stmt].kind {
                let mut u = Vec::new();
                body.walk(&mut |x| {
                    if let ExprKind::Field(_, f, _) = &x.kind {
                        if fields.contains(f) {
                            u.push(f.clone());
                        }
                    }
                });
                uses.entry(d.field.clone()).or_default().extend(u);
            }
        }
        for d in self.defines.clone() {
            // DFS for a path back to d.field
            let mut stack: Vec<(String, Vec<String>)> = vec![(d.field.clone(), vec![])];
            let mut seen = HashSet::new();
            while let Some((f, path)) = stack.pop() {
                for n in uses.get(&f).cloned().unwrap_or_default() {
                    if n == d.field {
                        let via = path.first().cloned().unwrap_or_else(|| d.field.clone());
                        let StmtKind::Define { field_span, .. } = &script.stmts[d.stmt].kind else { continue };
                        self.diags.push(
                            Diagnostic::error(
                                "E209",
                                *field_span,
                                Text2::new(
                                    format!("The field \"{}\" refers to itself (via \"{via}\").", d.field),
                                    format!("Das Feld \"{}\" verweist auf sich selbst (über \"{via}\").", d.field),
                                ),
                            )
                            .why("A field cannot be computed from itself.", "Ein Feld lässt sich nicht aus sich selbst berechnen."),
                        );
                        stack.clear();
                        break;
                    }
                    if seen.insert(n.clone()) {
                        let mut p = path.clone();
                        p.push(n.clone());
                        stack.push((n, p));
                    }
                }
            }
        }
    }
}

fn better_name(w: &str) -> &'static str {
    match w {
        "m" => "mode",
        "h" => "home",
        "s" => "shop",
        "t" => "tree",
        "g" => "group",
        "min" => "minimum",
        _ => "item",
    }
}

fn g_span(cl: &Clauses, g: &Grouping) -> Span {
    cl.per.iter().find(|(x, _)| std::ptr::eq(x, g)).map_or(Span::default(), |(_, s)| *s)
}

pub fn box_km2(b: &[f64; 4]) -> f64 {
    let lat = ((b[2] + b[3]) / 2.0).to_radians();
    let w = (b[1] - b[0]) * 111.32 * lat.cos();
    let h = (b[3] - b[2]) * 110.57;
    (w * h).abs()
}

pub fn fmt_int(x: f64) -> String {
    let s = format!("{}", x.round() as i64);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 && c != '-' {
            out.push(',');
        }
        out.push(c);
    }
    out
}
