//! knut language executor: runs `.knut` scripts against a `.knutdb` database
//! (KNUT_LANG.md §7–8).
//!
//! *knut* is short for *knutpunkt*, the Swedish word for ‘junction’ or ‘node’, which is
//! the building blocks of OpenStreetMap data.

pub mod address;
pub mod agg;
pub mod areas;
pub mod boundaries;
pub mod arith;
pub mod cache;
pub mod ch;
pub mod control;
pub mod cs;
pub mod dataset;
pub mod db;
pub mod eval;
pub mod fields;
pub mod geocode;
pub mod geojson;
pub mod indexes;
pub mod kdtree;
pub mod geom;
pub mod net;
pub mod notes;
pub mod output;
pub mod places;
pub mod region;
pub mod routing;
pub mod scan;
pub mod set;
pub mod spatial;
pub mod special;
pub mod stats;
pub mod value;

pub use output::Format;
pub use value::{R, RtErr, Value};

use eval::Ctx;
use knut_lang::ast::{ExprKind, Special, StmtKind};
use knut_lang::diag::{self, Diagnostic, Lang, Span, Text2};
use knut_plan::{Checked, DbCaps, IdentRes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Options {
    /// Overrides `knut_db` in the script.
    pub db: Option<PathBuf>,
    pub format: Format,
    pub out_dir: Option<PathBuf>,
    pub check_only: bool,
    pub explain: bool,
    /// Rows shown per result in text output.
    pub limit: usize,
    pub timing: bool,
    /// Progress and cancellation (`knut web`).
    pub control: Option<Arc<control::Control>>,
    /// An already opened database, used when the script asks for the same file (keeps
    /// the block cache between runs).
    pub shared_db: Option<Arc<db::Db>>,
}

impl Default for Options {
    fn default() -> Self {
        Options { db: None, format: Format::Text, out_dir: None, check_only: false, explain: false, limit: 20, timing: true, control: None, shared_db: None }
    }
}

/// Exit codes (KNUT_LANG.md §1).
pub const EXIT_OK: i32 = 0;
pub const EXIT_CHECK: i32 = 1;
pub const EXIT_RUNTIME: i32 = 2;
pub const EXIT_USAGE: i32 = 64;

/// A checked script ready to run.
pub struct Prepared {
    pub ctx: Ctx,
    pub chk: &'static Checked,
    pub file: Option<String>,
}

fn expand(p: &str, base: &Path) -> PathBuf {
    let p = if let Some(rest) = p.strip_prefix("~/") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|| PathBuf::from(p))
    } else {
        PathBuf::from(p)
    };
    if p.is_absolute() { p } else { base.join(p) }
}

/// Parses and checks a script (phases 1–4) and opens its database.
pub fn prepare(src: &str, file: Option<&str>, base_dir: &Path, opts: &Options) -> (Option<Prepared>, Vec<Diagnostic>, Lang) {
    let (script, mut diags) = knut_lang::parse(src);
    let settings = knut_plan::settings_of(&script);
    let lang = settings.lang;
    // database
    let mut db = None;
    let mut db_err = None;
    let db_path = match (&opts.db, &settings.knut_db) {
        (Some(p), _) => Some((p.clone(), Span::default())),
        (None, Some((p, sp))) => Some((expand(p, base_dir), *sp)),
        _ => None,
    };
    if let Some((p, sp)) = &db_path {
        let shared = opts.shared_db.as_ref().filter(|d| d.path == *p || std::fs::canonicalize(&d.path).ok() == std::fs::canonicalize(p).ok());
        let opened = match shared {
            Some(d) => Ok(d.clone()),
            None => db::Db::open(p).map(Arc::new),
        };
        match opened {
            Ok(d) => db = Some(d),
            Err(e) => {
                let shown = p.display().to_string();
                let d = Diagnostic::error(
                    "E205",
                    *sp,
                    Text2::new(format!("The database '{shown}' cannot be opened."), format!("Die Datenbank '{shown}' lässt sich nicht öffnen.")),
                )
                .why(e.clone(), e)
                .why(
                    "Relative paths start at the folder of the script.",
                    "Relative Pfade beginnen im Ordner des Skripts.",
                );
                db_err = Some(d.clone());
                diags.push(d);
            }
        }
    }
    let caps = db.as_ref().map(|d| DbCaps {
        users: d.users,
        notes: notes::exists(&d.path),
        extent_km2: d.extent.area_km2(),
        path: d.path.display().to_string(),
    });
    let chk = knut_plan::check(script, src, diags, caps.as_ref());
    let needs_db = chk.needs_db;
    let chk: &'static Checked = Box::leak(Box::new(chk));
    let mut diags = chk.diags.clone();
    if needs_db && db.is_none() && db_err.is_none() {
        let d = Diagnostic::error("E205", Span::default(), Text2::new("No database given.", "Keine Datenbank angegeben."))
            .why("Add a line like this at the top of the script, or use --db:", "Ergänze oben im Skript eine Zeile wie diese oder nutze --db:")
            .sugg("knut_db = 'file.knutdb'");
        diags.insert(0, d);
    }
    let mut ctx = Ctx::new(chk, db, db_err, base_dir.to_path_buf());
    ctx.control = opts.control.clone();
    // phase 2 checks that need the database: places, data sets
    if diags.iter().all(|d| !d.is_error()) {
        let mut extra = Vec::new();
        let mut names: Vec<(&Span, &String)> = chk
            .idents
            .iter()
            .filter_map(|(s, r)| match r {
                IdentRes::Region(n) => Some((s, n)),
                _ => None,
            })
            .collect();
        names.sort();
        for (sp, n) in names {
            if let Err(e) = places::region_value(&ctx, n, *sp) {
                let mut d = *e.diag;
                d.span = *sp;
                extra.push(d);
            }
        }
        for st in &chk.script.stmts {
            let es: Vec<&knut_lang::ast::Expr> = match &st.kind {
                StmtKind::Assign { value, .. } | StmtKind::Setting { value, .. } => vec![value],
                StmtKind::Define { body, .. } => vec![body],
                StmtKind::Output(xs) => xs.iter().collect(),
            };
            for e in es {
                e.walk(&mut |x| {
                    if let ExprKind::Special(s) = &x.kind
                        && let Special::Dataset(f) = &**s
                        && !dataset::resolve(&ctx, f).exists()
                    {
                        let p = dataset::resolve(&ctx, f).display().to_string();
                        extra.push(
                            Diagnostic::error(
                                "E213",
                                x.span,
                                Text2::new(format!("The data set '{f}' does not exist."), format!("Den Datensatz '{f}' gibt es nicht.")),
                            )
                            .why(format!("Looked for {p}."), format!("Gesucht wurde {p}.")),
                        );
                    }
                });
            }
        }
        diags.extend(extra);
        diag::sort(&mut diags);
    }
    (Some(Prepared { ctx, chk, file: file.map(|s| s.to_string()) }), diags, lang)
}

fn render_all(ds: &[Diagnostic], src: &str, file: Option<&str>, lang: Lang, err: &mut dyn Write) {
    for d in ds {
        let _ = writeln!(err, "{}", diag::render(d, src, file, lang));
    }
}

/// One evaluated output.
pub struct OutputValue {
    pub line: usize,
    pub expr: String,
    pub value: Value,
    pub seconds: f64,
}

/// Runs a script file. Writes results to `out` and messages to `err`; returns the exit code.
pub fn run_file(path: &Path, opts: &Options, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(err, "Error:  cannot read {}: {e}", path.display());
            return EXIT_USAGE;
        }
    };
    let base = path.parent().map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p }).unwrap_or(Path::new("."));
    run_source(&src, Some(&path.display().to_string()), base, opts, out, err)
}

pub fn run_source(src: &str, file: Option<&str>, base_dir: &Path, opts: &Options, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    run_source_with(src, file, base_dir, opts, out, err, None, None)
}

/// Receives every output instead of the formatted text (`run_collect`).
pub type Sink<'a> = &'a mut dyn FnMut(&Ctx, &OutputValue) -> R<()>;

#[allow(clippy::too_many_arguments)]
fn run_source_with(
    src: &str,
    file: Option<&str>,
    base_dir: &Path,
    opts: &Options,
    out: &mut dyn Write,
    err: &mut dyn Write,
    sink: Option<Sink>,
    mut collect: Option<&mut Vec<Diagnostic>>,
) -> i32 {
    let (prep, diags, lang) = prepare(src, file, base_dir, opts);
    // W001 is a hint while writing (a comparison with a missing value is false, which is
    // usually what is meant); it does not stop or clutter a run
    let diags: Vec<Diagnostic> = diags.into_iter().filter(|d| d.code != "W001").collect();
    if let Some(c) = collect.as_mut() {
        c.extend(diags.iter().cloned());
    }
    let errors = diags.iter().filter(|d| d.is_error()).count();
    let warnings = diags.len() - errors;
    let ignore = prep.as_ref().is_some_and(|p| p.chk.settings.ignore_warnings);
    let stop = errors > 0 || (warnings > 0 && !ignore);
    if !diags.is_empty() {
        render_all(&diags, src, file, lang, err);
        if diags.len() > 1 {
            let _ = write!(err, "{}", diag::summary(errors, warnings, stop, lang));
            let _ = writeln!(err);
            let _ = writeln!(err);
            let _ = write!(err, "{}", diag::compact_list(&diags, src, lang));
        } else {
            let _ = writeln!(err, "{}", diag::summary(errors, warnings, stop, lang));
        }
    }
    if stop {
        return EXIT_CHECK;
    }
    let prep = prep.unwrap();
    if opts.check_only {
        if diags.is_empty() {
            let _ = writeln!(err, "{}", if lang == Lang::De { "Keine Probleme gefunden." } else { "No problems found." });
        }
        return EXIT_OK;
    }
    if opts.explain {
        let _ = write!(out, "{}", explain(&prep));
        return EXIT_OK;
    }
    execute(&prep, opts, out, err, sink, collect)
}

/// Evaluates all outputs (used by the Python binding).
pub fn evaluate(prep: &Prepared, f: &mut dyn FnMut(OutputValue) -> R<()>) -> R<()> {
    let chk = prep.chk;
    let steps: usize = chk.outputs.iter().map(|&i| if let StmtKind::Output(es) = &chk.script.stmts[i].kind { es.len() } else { 0 }).sum();
    let mut step = 0;
    for &i in &chk.outputs {
        let StmtKind::Output(es) = &chk.script.stmts[i].kind else { continue };
        for e in es {
            if let Some(c) = &prep.ctx.control {
                let text: String = prep.ctx.text(e.span).chars().take(80).collect();
                c.begin_step(step, steps, &format!("line {}: {text}", chk.line_of(e.span)));
            }
            step += 1;
            prep.ctx.check_cancel()?;
            let t0 = Instant::now();
            let v = prep.ctx.eval(e, &None)?;
            // force lazy sets so that timing and errors belong to this output
            if let Value::Set(s) = &v {
                s.items(&prep.ctx, set::Hints::default())?;
            }
            f(OutputValue { line: chk.line_of(e.span), expr: prep.ctx.text(e.span).to_string(), value: v, seconds: t0.elapsed().as_secs_f64() })?;
        }
    }
    Ok(())
}

fn execute(prep: &Prepared, opts: &Options, out: &mut dyn Write, err: &mut dyn Write, mut sink: Option<Sink>, mut collect: Option<&mut Vec<Diagnostic>>) -> i32 {
    let ctx = &prep.ctx;
    let chk = prep.chk;
    let lang = chk.settings.lang;
    let src = &chk.src;
    let mut n_out = 0usize;
    let mut json_all = Vec::new();
    if let Some(d) = &opts.out_dir {
        // a failure shows up when the first output is written
        let _ = std::fs::create_dir_all(d);
    }
    let mut empties: Vec<(Span, String)> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    for &i in &chk.outputs {
        let StmtKind::Output(es) = &chk.script.stmts[i].kind else { continue };
        for e in es {
            spans.push(e.span);
        }
    }
    let mut k = 0usize;
    let res = evaluate(prep, &mut |o: OutputValue| -> R<()> {
        let span = spans[k];
        k += 1;
        n_out += 1;
        for n in eval::lock(&ctx.caches.notices).drain(..) {
            let _ = writeln!(err, "{n}");
        }
        if output::is_empty(ctx, &o.value) {
            empties.push((span, o.expr.clone()));
        }
        if let Some(s) = sink.as_mut() {
            return s(ctx, &o);
        }
        match opts.format {
            Format::Text => {
                let t = if opts.timing { format!("   ({}, {:.2} s)", if lang == Lang::De { format!("Zeile {}", o.line) } else { format!("line {}", o.line) }, o.seconds) } else { String::new() };
                let _ = writeln!(out, "── {}{t}", o.expr);
                let _ = write!(out, "{}", output::text(ctx, &o.value, opts.limit)?);
                let _ = writeln!(out);
            }
            Format::Csv => {
                let body = output::csv(ctx, &o.value)?;
                match &opts.out_dir {
                    Some(d) => {
                        let p = d.join(format!("output_{n_out}.csv"));
                        std::fs::write(&p, body).map_err(|e| RtErr::internal(span, e))?;
                        let _ = writeln!(err, "{} → {}", o.expr, p.display());
                    }
                    None => {
                        let _ = writeln!(out, "# {}", o.expr);
                        let _ = write!(out, "{body}");
                        let _ = writeln!(out);
                    }
                }
            }
            Format::Json => {
                json_all.push(serde_json::json!({"line": o.line, "expr": o.expr, "seconds": o.seconds, "value": output::json(ctx, &o.value)?}));
            }
            Format::Parquet => {
                let d = opts.out_dir.clone().unwrap_or_else(|| PathBuf::from("."));
                let p = d.join(format!("output_{n_out}.parquet"));
                output::parquet(ctx, &o.value, &p)?;
                let _ = writeln!(err, "{} → {}", o.expr, p.display());
            }
        }
        let _ = out.flush();
        Ok(())
    });
    if opts.format == Format::Json {
        let body = serde_json::to_string_pretty(&json_all).unwrap_or_default();
        match &opts.out_dir {
            Some(d) => {
                let p = d.join("outputs.json");
                let _ = std::fs::write(&p, body);
            }
            None => {
                let _ = writeln!(out, "{body}");
            }
        }
    }
    for n in eval::lock(&ctx.caches.notices).drain(..) {
        let _ = writeln!(err, "{n}");
    }
    if let Err(e) = res {
        if let Some(c) = collect.as_mut() {
            c.push((*e.diag).clone());
        }
        let line = chk.line_of(e.diag.span);
        let _ = writeln!(err, "{}", diag::render(&e.diag, src, prep.file.as_deref(), lang));
        let written = n_out;
        let msg = match lang {
            Lang::En if written == 0 => format!("Stopped at line {line}. No outputs were written."),
            Lang::En => format!("Stopped at line {line}. Outputs 1–{written} were written."),
            Lang::De if written == 0 => format!("Abgebrochen in Zeile {line}. Es wurden keine Ausgaben geschrieben."),
            Lang::De => format!("Abgebrochen in Zeile {line}. Ausgaben 1–{written} wurden geschrieben."),
        };
        let _ = writeln!(err, "{msg}");
        return EXIT_RUNTIME;
    }
    // data warnings (§8): after the results, never stopping
    let mut dw = Vec::new();
    for (site, cnt) in chk.w001.iter().zip(&ctx.w001) {
        let n = cnt.load(std::sync::atomic::Ordering::Relaxed);
        if n == 0 {
            continue;
        }
        let ns = knut_plan::fmt_int(n as f64);
        let (en, de) = if n == 1 {
            let k = site.kind.map_or("element", |k| k.name());
            (format!("1 {k} has no {} and was left out.", site.key), format!("1 {k} hat kein {} und fiel weg.", site.key))
        } else {
            let kp = site.kind.map_or("elements", |k| k.plural());
            (format!("{ns} {kp} have no {} and were left out.", site.key), format!("{ns} {kp} haben kein {} und fielen weg.", site.key))
        };
        dw.push(
            Diagnostic::warning("W001", site.span, Text2::new("A comparison with a missing value is always false.", "Ein Vergleich mit einem fehlenden Wert ist immer falsch."))
                .why(en, de),
        );
    }
    for (sp, _) in empties {
        dw.push(
            Diagnostic::warning("W002", sp, Text2::new("The result is empty.", "Das Ergebnis ist leer."))
                .why("The query ran but found nothing.", "Die Abfrage lief, fand aber nichts."),
        );
    }
    if !dw.is_empty() {
        render_all(&dw, src, prep.file.as_deref(), lang, err);
        if let Some(c) = collect.as_mut() {
            c.extend(dw.iter().cloned());
        }
    }
    EXIT_OK
}

/// A readable description of the scans (`--explain`).
pub fn explain(prep: &Prepared) -> String {
    use knut_lang::ast::Expr;
    use std::fmt::Write as _;
    let chk = prep.chk;
    let mut s = String::new();
    let describe = |e: &Expr, s: &mut String| {
        e.walk(&mut |x| {
            if let ExprKind::Select(sel) = &x.kind {
                let line = chk.line_of(x.span);
                let var = &sel.pattern[0].var;
                let preds: Vec<&Expr> = [sel.var_pred.as_deref(), sel.cond.as_deref()].into_iter().flatten().collect();
                let mut pred = knut_plan::pushdown::TagPred::True;
                let mut exact = true;
                let mut geom = false;
                for p in &preds {
                    let r = knut_plan::pushdown::extract(p, var, chk);
                    exact &= r.exact;
                    geom |= knut_plan::pushdown::needs_geometry(p, var);
                    pred = if pred.is_true() { r.pred } else { knut_plan::pushdown::TagPred::And(vec![pred, r.pred]) };
                }
                let src = match &sel.source {
                    Some(_) => "named set".to_string(),
                    None => format!("database scan of {}", sel.pattern.iter().map(|p| p.kind.plural()).collect::<Vec<_>>().join(" → ")),
                };
                let time = match &sel.time {
                    None if sel.pattern.len() > 1 => "whole history".to_string(),
                    None => "state at the end of the data".to_string(),
                    Some(knut_lang::ast::TimeRef::On(_)) => "state at a moment".to_string(),
                    Some(knut_lang::ast::TimeRef::Range(..)) => "versions in a period".to_string(),
                    Some(knut_lang::ast::TimeRef::OnEach { unit, .. }) => format!("state at the start of every {}", unit.name()),
                };
                let _ = writeln!(s, "line {line}: {src}");
                let _ = writeln!(s, "  time:        {time}");
                let _ = writeln!(s, "  region:      {}", if sel.within.is_some() { "R-tree query, then representative points" } else { "whole database" });
                let req: Vec<String> = pred.required().into_iter().map(|(k, v)| v.map_or(k.to_string(), |v| format!("{k}={v}"))).collect();
                let _ = writeln!(
                    s,
                    "  tag filter:  {} (evaluated once per tagset{})",
                    if pred.is_true() { "none".to_string() } else { pred.to_string() },
                    if req.is_empty() { String::new() } else { format!("; bloom filters: {}", req.join(", ")) }
                );
                let _ = writeln!(s, "  residual:    {}", if exact || preds.is_empty() { "none" } else { "condition evaluated per element" });
                let _ = writeln!(s, "  geometry:    {}", if geom { "decoded" } else { "not needed (attribute-only blocks where possible)" });
                if let Some(p) = &sel.pick {
                    let _ = writeln!(s, "  pick:        top {} by {}", p.n.unwrap_or(1), if sel.randomly { "random rank" } else { "id" });
                }
            }
        });
    };
    for st in &chk.script.stmts {
        match &st.kind {
            StmtKind::Assign { value, .. } => describe(value, &mut s),
            StmtKind::Output(es) => {
                for e in es {
                    describe(e, &mut s);
                }
            }
            _ => {}
        }
    }
    if s.is_empty() {
        s.push_str("No database scans.\n");
    }
    s
}

/// One output of `run_collect`.
#[derive(Debug, serde::Serialize)]
pub struct CollectedOutput {
    pub line: usize,
    pub expr: String,
    pub seconds: f64,
    /// Text rendering (as `--format text`).
    pub text: String,
    /// Value as `--format json`.
    pub value: serde_json::Value,
    /// GeoJSON feature collection, if the result has something to draw.
    pub geojson: Option<serde_json::Value>,
    pub features: usize,
    /// More features exist than were included.
    pub truncated: bool,
}

/// Result of `run_collect`.
#[derive(Debug, serde::Serialize)]
pub struct Collected {
    pub code: i32,
    /// All messages as `knut script.knut` prints them.
    pub messages: String,
    pub outputs: Vec<CollectedOutput>,
    pub cancelled: bool,
    /// The messages with positions (for editors).
    pub problems: Vec<Problem>,
}

/// Runs a script and collects its outputs as text, JSON and GeoJSON (for `knut web`).
/// `max_features` limits the features per output.
pub fn run_collect(src: &str, file: Option<&str>, base_dir: &Path, opts: &Options, max_features: usize) -> Collected {
    let mut outputs = Vec::new();
    let mut err = Vec::new();
    let limit = opts.limit;
    let mut sink = |ctx: &Ctx, o: &OutputValue| -> R<()> {
        let text = output::text(ctx, &o.value, limit)?;
        let value = output::json(ctx, &o.value)?;
        let f = geojson::features(ctx, &o.value, max_features)?;
        outputs.push(CollectedOutput {
            line: o.line,
            expr: o.expr.clone(),
            seconds: o.seconds,
            text,
            value,
            features: f.as_ref().map_or(0, |f| f.count),
            truncated: f.as_ref().is_some_and(|f| f.truncated),
            geojson: f.map(|f| f.collection),
        });
        Ok(())
    };
    let mut diags = Vec::new();
    let code = run_source_with(src, file, base_dir, opts, &mut std::io::sink(), &mut err, Some(&mut sink), Some(&mut diags));
    let cancelled = opts.control.as_ref().is_some_and(|c| c.is_cancelled());
    let lang = knut_plan::settings_of(&knut_lang::parse(src).0).lang;
    let problems = diags.iter().filter(|d| d.code != "E902").map(|d| Problem::of(d, src, lang)).collect();
    Collected { code, messages: String::from_utf8_lossy(&err).into_owned(), outputs, cancelled, problems }
}

/// A message with its position, for editors: offsets count UTF-16 code units.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Problem {
    pub code: &'static str,
    pub severity: &'static str,
    pub from: usize,
    pub to: usize,
    pub line: usize,
    pub title: String,
    /// The whole message as printed.
    pub text: String,
}

impl Problem {
    pub fn of(d: &Diagnostic, src: &str, lang: Lang) -> Problem {
        let utf16 = |b: u32| -> usize {
            let b = (b as usize).min(src.len());
            let b = (0..=b).rev().find(|&i| src.is_char_boundary(i)).unwrap_or(0);
            src[..b].encode_utf16().count()
        };
        Problem {
            code: d.code,
            // W001 is a hint, it never stops a run
            severity: if d.is_error() {
                "error"
            } else if d.code == "W001" {
                "info"
            } else {
                "warning"
            },
            from: utf16(d.span.start),
            to: utf16(d.span.end.max(d.span.start)),
            line: diag::line_col(src, d.span.start).0 + 1,
            title: d.title.get(lang).to_string(),
            text: diag::render(d, src, None, lang),
        }
    }
}

/// Fast check while typing: parse and check only (no place names, no data sets).
pub fn quick_check(src: &str, opts: &Options) -> Vec<Problem> {
    let (script, diags) = knut_lang::parse(src);
    let lang = knut_plan::settings_of(&script).lang;
    let caps = opts.shared_db.as_ref().map(|d| DbCaps {
        users: d.users,
        notes: notes::exists(&d.path),
        extent_km2: d.extent.area_km2(),
        path: d.path.display().to_string(),
    });
    let chk = knut_plan::check(script, src, diags, caps.as_ref());
    let mut ds = chk.diags.clone();
    diag::sort(&mut ds);
    ds.iter().map(|d| Problem::of(d, src, lang)).collect()
}
