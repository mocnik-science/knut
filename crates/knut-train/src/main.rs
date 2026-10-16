//! Training data and evaluation for the language model of the simple page of `knut web`.
//!
//! `knut-train generate` makes questions from templates with the places, stations and years
//! of real databases, runs every query (only queries that run are kept) and writes chat
//! examples in the form the model sees in `knut web` (the same instructions, the same
//! corrections, the same results): questions → queries, questions back, corrections from
//! check messages, and answers from results. Questions with held-out things, places and
//! stations go into an evaluation set.
//!
//! `knut-train eval` asks a model (OpenAI-compatible server, e.g. `knut-llm serve`) these
//! questions, as `knut web` does, and compares the results of its queries with those of the
//! reference queries.

mod templates;
mod vocab;

use knut_exec::db::Db;
use knut_exec::{Collected, Options, control::Control};
use knut_web::chat::{self, Reply};
use serde_json::{Value as J, json};
use std::collections::HashSet;
use std::io::{BufRead, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use templates::{Ctx, Gen, Item, Rng, Start, TEMPLATES};

const USAGE: &str = "\
usage:
  knut-train generate --db FILE.knutdb [--db …] [--tags map_features.csv] [--out DIR] [--scale N] [--seed N] [--prompt short|full]
      writes DIR/train.jsonl, DIR/valid.jsonl (chat examples) and DIR/eval.jsonl; --prompt:
      the instructions in the examples, short (for a trained model, the default) or full
  knut-train eval --db FILE.knutdb --set FILE.jsonl [--set …] [--llm URL] [--model NAME] [--limit N] [--out FILE]
      asks the model the questions of the sets and compares the results
  knut-train refs --db FILE.knutdb --set FILE.jsonl
      runs the reference queries of a set (they must run and find something)";

/// Seconds a query may run while generating.
const QUERY_SECONDS: u64 = 30;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("generate") => generate(&args[1..]),
        Some("eval") => eval(&args[1..]),
        Some("refs") => refs(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// The values of an option (`--db a --db b`).
fn opts<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.windows(2).filter(|w| w[0] == name).map(|w| w[1].as_str()).collect()
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    opts(args, name).into_iter().last()
}

// ── running queries ──

struct Database {
    path: PathBuf,
    db: Arc<Db>,
    description: String,
}

impl Database {
    fn open(path: &str) -> Result<Database, String> {
        let path = std::fs::canonicalize(path).map_err(|e| format!("{path}: {e}"))?;
        let db = Arc::new(Db::open(&path)?);
        let description = chat::database_description(Some(&path), Some(&db));
        Ok(Database { path, db, description })
    }

    fn options(&self, control: Option<Arc<Control>>) -> Options {
        Options { db: Some(self.path.clone()), limit: knut_web::TEXT_ROWS, timing: false, control, shared_db: Some(self.db.clone()), ..Default::default() }
    }

    /// Runs a query; stops it after `seconds`.
    fn run(&self, query: &str, seconds: u64) -> Collected {
        let control = Control::new();
        let done = Arc::new(AtomicBool::new(false));
        let (c, d) = (control.clone(), done.clone());
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(seconds);
            while Instant::now() < until {
                if d.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            c.cancel();
        });
        let r = knut_exec::run_collect(query, Some("query.knut"), &self.path, &self.options(Some(control)), 1000);
        done.store(true, Ordering::Relaxed);
        r
    }

    /// The error messages of the check, as `knut web` gives them to the model.
    fn check(&self, query: &str) -> Vec<String> {
        knut_exec::quick_check(query, &self.options(None)).into_iter().filter(|p| p.severity == "error").map(|p| p.text).collect()
    }

    /// A list of texts from a query such as `(…).map(a => a:name)`.
    fn names(&self, query: &str) -> Vec<String> {
        let r = self.run(query, 120);
        let mut out: Vec<String> = r.outputs.first().and_then(|o| o.value.as_array().cloned()).unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
        out.retain(|n| n != "none" && !n.contains(['\'', '"', '\\', '(']) && n.len() < 40);
        let mut seen = HashSet::new();
        out.retain(|n| seen.insert(n.clone()));
        out.sort();
        out
    }
}

/// Some addresses of the database, chosen at random (the same ones for the same database).
fn addresses(d: &Database) -> Vec<templates::Addr> {
    let r = d.run("(pick 600 * x randomly with x:addr:street and x:addr:housenumber and x:addr:city).map(x => [x:addr:street, x:addr:housenumber, x:addr:city, x:addr:postcode ?? '', x.lon, x.lat])", 300);
    let rows = r.outputs.first().and_then(|o| o.value.as_array().cloned()).unwrap_or_default();
    let mut out = Vec::new();
    for row in rows {
        let Some(c) = row.as_array() else { continue };
        let s = |i: usize| c.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let (street, number, city) = (s(0), s(1), s(2));
        let ok = |t: &str| !t.is_empty() && !t.contains(['\'', '"', ';', ',']) && t.len() < 40;
        let (Some(lon), Some(lat)) = (c.get(4).and_then(|v| v.as_f64()), c.get(5).and_then(|v| v.as_f64())) else { continue };
        if ok(&street) && ok(&number) && ok(&city) && number.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            out.push(templates::Addr { street, number, city, postcode: s(3).replace("none", ""), lon, lat });
        }
    }
    out
}

fn year_of(t: i64) -> i32 {
    knut_lang::dates::format_instant(t)[..4].parse().unwrap_or(2024)
}

/// Places, stations and years of a database.
fn context(d: &Database, conf: &'static vocab::DbConf, tags: Arc<Vec<vocab::Tag>>, rng: &mut Rng) -> Ctx {
    let city = templates::region(conf.city);
    let districts = d.names(&format!("(areas a with a:boundary=administrative and a:admin_level='{}' within {city}).map(a => a:name)", conf.district_level));
    let levels = conf.place_levels.iter().map(|l| format!("'{l}'")).collect::<Vec<_>>().join(", ");
    let mut others = d.names(&format!("(areas a with a:boundary=administrative and a:admin_level in [{levels}]).map(a => a:name)"));
    others.retain(|n| !n.starts_with("Landkreis") && !n.starts_with("Bezirk") && !n.starts_with("Stadtbezirk") && !districts.contains(n));
    let mut chosen = Vec::new();
    while !others.is_empty() && chosen.len() < conf.place_sample {
        chosen.push(others.swap_remove(rng.below(others.len())));
    }
    let mut places = districts.clone();
    places.extend(chosen);
    places.push(conf.city.to_string());
    let mut starts: Vec<Start> = d
        .names(&format!("(nodes n with n:railway=station and n:name within {city}).map(n => n:name)"))
        .iter()
        .take(40)
        .map(|n| Start::station(n, conf.city))
        .collect();
    starts.extend(d.names(&format!("(* x with x:tourism=museum and x:name within {city}).map(x => x:name)")).iter().take(30).map(|n| Start::named(n)));
    let addresses = addresses(d);
    eprintln!("  {} districts, {} places, {} starts, {} addresses", districts.len(), places.len(), starts.len(), addresses.len());
    Ctx { conf, tags, addresses, districts, places, starts, first_year: year_of(d.db.earliest), last_year: year_of(d.db.latest) }
}

// ── generating ──

/// A kept question with its query, results and answer.
struct Kept {
    item: Item,
    db: usize,
    results: Option<String>,
    answer: Option<String>,
}

/// Whether the outputs say "nothing": 0, an empty set, an empty list.
fn nothing(r: &Collected) -> bool {
    r.outputs.iter().all(|o| o.value.as_i64() == Some(0) || o.value.as_array().is_some_and(|a| a.is_empty()) || o.value.is_null() || o.value["value"].as_f64() == Some(0.0))
}

fn reply_text(r: &Reply) -> String {
    match r {
        Reply::Query(q) => format!("QUERY:\n{q}"),
        Reply::Ask(a) => format!("ASK: {a}"),
    }
}

fn messages(system: &str, turns: &[(String, String)], reply: &str) -> J {
    let mut m = vec![json!({ "role": "system", "content": system })];
    m.extend(turns.iter().map(|(r, c)| json!({ "role": r, "content": c })));
    m.push(json!({ "role": "assistant", "content": reply }));
    json!({ "messages": m })
}

fn plain(msgs: &[(String, String)], reply: &str) -> J {
    let mut m: Vec<J> = msgs.iter().map(|(r, c)| json!({ "role": r, "content": c })).collect();
    m.push(json!({ "role": "assistant", "content": reply }));
    json!({ "messages": m })
}

/// Typical mistakes in a query, for examples of corrections from the check messages.
fn corruptions(q: &str, rng: &mut Rng) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |s: String| {
        if s != q {
            out.push(s)
        }
    };
    add(q.replacen(" with ", " where ", 1));
    add(q.replacen("count of ", "count ", 1));
    add(q.replacen("sum of ", "sum ", 1));
    for m in ["foot", "bicycle", "car"] {
        add(q.replace(&format!(" by {m}"), ""));
    }
    add(q.replace(" by foot", " by walking").replace(" by bicycle", " by bike"));
    add(q.replace(" by foot", " on foot"));
    add(q.replace("== none", "== null"));
    add(q.replace("== none", "is none"));
    add(q.replacen("isochrone I from", "isochrone from", 1));
    add(q.replacen("path P from", "path from", 1));
    add(q.replacen("nearest of (", "nearest (", 1));
    add(q.replacen(" within reach", " in reach", 1));
    add(q.replacen("* x with", "all x with", 1));
    add(q.replacen("ordered by -a.area", "ordered by a.area desc", 1));
    add(q.replacen(" per year from", " per year since", 1));
    if let Some(i) = q.find(" on each year from ") {
        let rest = &q[i + " on each year from ".len()..];
        if let Some(y) = rest.split_whitespace().next() {
            add(format!("{} on each year since {y}", &q[..i]));
        }
    }
    if let Some(i) = q.find("per (areas d") {
        add(format!("{}per district", &q[..i]));
    }
    if let Some(i) = q.rfind(')') {
        add(format!("{}{}", &q[..i], &q[i + 1..]));
    }
    // `x:shop=bakery` → `x:shop = bakery`
    if let Some(i) = q.find('=').filter(|&i| i > 0 && q.as_bytes()[i - 1].is_ascii_alphanumeric() && q.as_bytes().get(i + 1).is_some_and(|b| b.is_ascii_alphanumeric())) {
        add(format!("{} = {}", &q[..i], &q[i + 1..]));
    }
    // a quoted place without quotes
    if let Some(i) = q.find(" within '") {
        let rest = &q[i + 9..];
        if let Some(j) = rest.find('\'') {
            add(format!("{} within {}{}", &q[..i], &rest[..j], &rest[j + 1..]));
        }
    }
    // a place without its accents and quotes, or misspelt (fails when it runs)
    if let Some(i) = q.find(" within '") {
        let rest = &q[i + 9..];
        if let Some(j) = rest.find('\'') {
            let plain: String = rest[..j].chars().map(fold).filter(|c| c.is_ascii_alphanumeric() || *c == ' ').collect();
            if !plain.contains(' ') && plain != rest[..j] {
                add(format!("{} within {plain}{}", &q[..i], &rest[j + 1..]));
            }
        }
    }
    if let Some(i) = q.find(" within ").filter(|&i| q[i + 8..].starts_with(|c: char| c.is_ascii_uppercase())) {
        let rest = &q[i + 8..];
        let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
        if end > 5 {
            add(format!("{} within {}{}{}", &q[..i], &rest[..end / 2], &rest[end / 2 + 1..end], &rest[end..]));
        }
    }
    // the wrong variable in the condition
    for v in ["n", "x", "a", "w"] {
        let pat = format!(" {v} with {v}:");
        if q.contains(&pat) {
            add(q.replacen(&pat, &format!(" {v} with {}:", if v == "x" { "n" } else { "x" }), 1));
        }
    }
    // keep a few, in random order
    let mut picked = Vec::new();
    while !out.is_empty() && picked.len() < 4 {
        picked.push(out.swap_remove(rng.below(out.len())));
    }
    picked
}

/// A letter without its accent (ä → a, ß → s).
fn fold(c: char) -> char {
    match c {
        'ä' | 'á' | 'à' | 'â' | 'å' => 'a',
        'Ä' | 'Á' | 'À' | 'Â' | 'Å' => 'A',
        'ö' | 'ó' | 'ò' | 'ô' | 'ø' => 'o',
        'Ö' | 'Ó' | 'Ò' | 'Ô' | 'Ø' => 'O',
        'ü' | 'ú' | 'ù' | 'û' => 'u',
        'Ü' | 'Ú' | 'Ù' | 'Û' => 'U',
        'é' | 'è' | 'ê' => 'e',
        'É' | 'È' | 'Ê' => 'E',
        'ß' => 's',
        c => c,
    }
}

fn generate(args: &[String]) -> Result<(), String> {
    let dbs = opts(args, "--db");
    if dbs.is_empty() {
        return Err(USAGE.into());
    }
    let out = PathBuf::from(opt(args, "--out").unwrap_or("data/train"));
    let scale: usize = opt(args, "--scale").map(|s| s.parse().map_err(|_| "--scale needs a number")).transpose()?.unwrap_or(20);
    let short = match opt(args, "--prompt").unwrap_or("short") {
        "short" => true,
        "full" => false,
        p => return Err(format!("--prompt is short or full, not {p}")),
    };
    let tags = Arc::new(match opt(args, "--tags") {
        Some(p) => vocab::read_tags(std::path::Path::new(p))?,
        None => Vec::new(),
    });
    let seed: u64 = opt(args, "--seed").map(|s| s.parse().map_err(|_| "--seed needs a number")).transpose()?.unwrap_or(1);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let examples = knut_web::examples();
    let mut rng = Rng::new(seed);
    let mut databases = Vec::new();
    let mut ctxs = Vec::new();
    for p in &dbs {
        let d = Database::open(p)?;
        let stem = d.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let conf = vocab::DBS.iter().find(|c| c.stem == stem).ok_or_else(|| format!("{p}: no configuration for '{stem}' in vocab.rs (DBS)"))?;
        eprintln!("{stem}:");
        ctxs.push(context(&d, conf, tags.clone(), &mut rng));
        databases.push(d);
    }

    let mut kept: Vec<Kept> = Vec::new();
    let mut held: Vec<Kept> = Vec::new();
    let mut seen = HashSet::new();
    for (di, d) in databases.iter().enumerate() {
        let started = Instant::now();
        for &(name, weight, template) in TEMPLATES {
            for is_held in [false, true] {
                // per database; the evaluation gets a twentieth
                let want = if is_held { (weight * scale / 20).max(1) } else { weight * scale } / databases.len().max(1);
                let mut got = 0;
                let mut tries = 0;
                let mut g = Gen { rng: Rng::new(rng.next()), ctx: &ctxs[di], held: is_held };
                while got < want && tries < want * 5 {
                    tries += 1;
                    let Some(item) = template(&mut g) else { continue };
                    let key = format!("{di}|{:?}|{:?}", item.turns, reply_text(&item.reply));
                    if !seen.insert(key) {
                        continue;
                    }
                    let (results, answer) = match &item.reply {
                        Reply::Ask(_) => (None, None),
                        Reply::Query(q) => {
                            let r = d.run(q, QUERY_SECONDS);
                            if r.code != 0 || r.cancelled || r.outputs.is_empty() {
                                continue;
                            }
                            if nothing(&r) && !item.zero_ok && !g.rng.chance(0.1) {
                                continue;
                            }
                            let texts: Vec<(String, String)> = r.outputs.iter().map(|o| (o.expr.clone(), o.text.clone())).collect();
                            let results = chat::results_text(&texts);
                            let answer = item.answer.as_ref().and_then(|f| f(&r.outputs, &results));
                            (Some(results), answer)
                        }
                    };
                    got += 1;
                    let k = Kept { item, db: di, results, answer };
                    if is_held { held.push(k) } else { kept.push(k) }
                }
                if got < want {
                    eprintln!("  {name}{}: {got} of {want}", if is_held { " (held out)" } else { "" });
                }
            }
        }
        eprintln!("{}: {} questions in {:.0} s", d.path.display(), kept.iter().chain(&held).filter(|k| k.db == di).count(), started.elapsed().as_secs_f64());
    }

    // the chat examples
    let mut samples: Vec<(String, J)> = Vec::new();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for k in &kept {
        let d = &databases[k.db];
        let question = k.item.question().to_string();
        let system = if short { chat::short_prompt(&d.description) } else { chat::system_prompt(&d.description, &question, &examples) };
        let reply = reply_text(&k.item.reply);
        samples.push((k.item.template.to_string(), messages(&system, &k.item.turns, &reply)));
        *counts.entry("query or question back".into()).or_default() += 1;
        if let Reply::Query(good) = &k.item.reply {
            if rng.chance(0.3) {
                for bad in corruptions(good, &mut rng) {
                    // the messages of the check, else those of a failed run (as in knut web)
                    let mut problems = d.check(&bad);
                    if problems.is_empty() {
                        let r = d.run(&bad, 10);
                        if r.code != 0 && r.outputs.is_empty() && !r.cancelled {
                            problems.push(r.messages);
                        }
                    }
                    if problems.is_empty() {
                        continue;
                    }
                    let mut turns = k.item.turns.clone();
                    turns.push(("assistant".into(), format!("QUERY:\n{bad}")));
                    turns.push(("user".into(), chat::correction_message(&problems)));
                    samples.push((format!("{}/correction", k.item.template), messages(&system, &turns, &reply)));
                    *counts.entry("correction".into()).or_default() += 1;
                    break;
                }
            }
            if let (Some(results), Some(answer)) = (&k.results, &k.answer)
                && rng.chance(0.35)
            {
                samples.push((format!("{}/answer", k.item.template), plain(&chat::answer_messages(&question, good, results), answer)));
                *counts.entry("answer".into()).or_default() += 1;
            }
        }
    }
    // shuffle, then a twentieth for validation
    for i in (1..samples.len()).rev() {
        samples.swap(i, rng.below(i + 1));
    }
    let n_valid = (samples.len() / 20).max(1);
    let write = |name: &str, rows: &[(String, J)]| -> Result<(), String> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(out.join(name)).map_err(|e| e.to_string())?);
        for (_, j) in rows {
            writeln!(f, "{j}").map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    write("valid.jsonl", &samples[..n_valid])?;
    write("train.jsonl", &samples[n_valid..])?;

    // the evaluation set
    let mut f = std::io::BufWriter::new(std::fs::File::create(out.join("eval.jsonl")).map_err(|e| e.to_string())?);
    for k in &held {
        let (expect, reference) = match &k.item.reply {
            Reply::Query(q) => ("query", Some(q.clone())),
            Reply::Ask(_) => ("ask", None),
        };
        let row = json!({
            "db": databases[k.db].path,
            "template": k.item.template,
            "lang": format!("{:?}", k.item.lang),
            "turns": k.item.turns.iter().map(|(r, c)| json!({ "role": r, "content": c })).collect::<Vec<_>>(),
            "expect": expect,
            "reference": reference,
        });
        writeln!(f, "{row}").map_err(|e| e.to_string())?;
    }
    drop(f);

    let per_template = kept.iter().fold(std::collections::BTreeMap::<&str, usize>::new(), |mut m, k| {
        *m.entry(k.item.template).or_default() += 1;
        m
    });
    eprintln!("\nquestions per template: {per_template:?}");
    eprintln!("examples: {counts:?}");
    eprintln!("{} training, {} validation examples, {} evaluation questions in {}", samples.len() - n_valid, n_valid, held.len(), out.display());
    Ok(())
}

// ── evaluating ──

struct Case {
    db: Option<PathBuf>,
    template: String,
    turns: Vec<(String, String)>,
    expect_ask: bool,
    reference: Option<String>,
}

/// Reads a set: lines from `generate` (`turns`, `expect`, `reference`), or written by hand
/// (`question`, optionally `turns` before it, and `reference` or `"ask": true`).
fn read_set(path: &str) -> Result<Vec<Case>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut out = Vec::new();
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() || line.trim_start().starts_with("//") {
            continue;
        }
        let v: J = serde_json::from_str(&line).map_err(|e| format!("{path}:{}: {e}", i + 1))?;
        let mut turns: Vec<(String, String)> = v["turns"].as_array().into_iter().flatten().map(|t| (t["role"].as_str().unwrap_or("user").to_string(), t["content"].as_str().unwrap_or("").to_string())).collect();
        if let Some(q) = v["question"].as_str() {
            turns.push(("user".into(), q.to_string()));
        }
        out.push(Case {
            db: v["db"].as_str().map(PathBuf::from),
            template: v["template"].as_str().unwrap_or("hand-written").to_string(),
            turns,
            expect_ask: v["expect"] == "ask" || v["ask"] == true,
            reference: v["reference"].as_str().map(str::to_string),
        });
    }
    Ok(out)
}

fn refs(args: &[String]) -> Result<(), String> {
    let d = Database::open(opt(args, "--db").ok_or(USAGE)?)?;
    let mut bad = 0;
    for s in opts(args, "--set") {
        for c in read_set(s)? {
            let Some(q) = &c.reference else { continue };
            let r = d.run(q, 120);
            let first = r.outputs.iter().map(|o| o.text.lines().next().unwrap_or("").to_string()).collect::<Vec<_>>().join(" | ");
            let ok = r.code == 0 && !r.outputs.is_empty() && !nothing(&r);
            bad += !ok as usize;
            println!("{} {}  →  {}", if ok { "✓" } else { "✗" }, c.turns.last().map(|t| t.1.as_str()).unwrap_or(""), if r.code == 0 { first } else { r.messages.lines().next().unwrap_or("").to_string() });
        }
    }
    if bad > 0 { Err(format!("{bad} references failed or found nothing")) } else { Ok(()) }
}

/// Two results agree: numbers up to rounding, everything else exactly.
fn same(a: &J, b: &J) -> bool {
    match (a, b) {
        (J::Number(x), J::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
            (x - y).abs() <= 1e-6 * x.abs().max(y.abs()).max(1.0)
        }
        (J::Array(x), J::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (J::Object(x), J::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w))),
        _ => a == b,
    }
}

fn eval(args: &[String]) -> Result<(), String> {
    let sets = opts(args, "--set");
    if sets.is_empty() {
        return Err(USAGE.into());
    }
    let url = opt(args, "--llm").unwrap_or("http://127.0.0.1:8780");
    let model = chat::HttpModel::new(url, opt(args, "--model").unwrap_or("knut"));
    let limit: usize = opt(args, "--limit").map(|s| s.parse().map_err(|_| "--limit needs a number")).transpose()?.unwrap_or(usize::MAX);
    let default_db = opt(args, "--db");
    let mut cases = Vec::new();
    for s in &sets {
        cases.extend(read_set(s)?);
    }
    cases.truncate(limit);
    let examples = knut_web::examples();
    let mut dbs: Vec<(PathBuf, Database)> = Vec::new();
    let mut out = match opt(args, "--out") {
        Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p).map_err(|e| e.to_string())?)),
        None => None,
    };
    // per template: (cases, correct, valid queries, seconds)
    let mut score = std::collections::BTreeMap::<String, (usize, usize, usize, f64)>::new();
    for (i, c) in cases.iter().enumerate() {
        let path = c.db.clone().or_else(|| default_db.map(PathBuf::from)).ok_or("a case without database: give --db")?;
        let path = std::fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !dbs.iter().any(|(p, _)| *p == path) {
            dbs.push((path.clone(), Database::open(&path.to_string_lossy())?));
        }
        let d = &dbs.iter().find(|(p, _)| *p == path).expect("opened").1;
        let question = c.turns.last().map(|t| t.1.clone()).unwrap_or_default();
        let system = chat::instructions(&model, &d.description, &question, &examples);
        let started = Instant::now();
        let run = |q: &str| -> Result<Collected, String> {
            let r = d.run(q, 60);
            if r.code != 0 && r.outputs.is_empty() { Err(r.messages) } else { Ok(r) }
        };
        let outcome = chat::solve(&model, &system, &c.turns, &|q| d.check(q), &run, &|_| {});
        let seconds = started.elapsed().as_secs_f64();
        let (ok, valid, note, got) = match outcome {
            Err(e) => (false, false, format!("model error: {e}"), None),
            Ok(chat::Outcome::Ask(a)) => (c.expect_ask, false, format!("asked: {a}"), None),
            Ok(_) if c.expect_ask => (false, false, "did not ask".into(), None),
            Ok(chat::Outcome::Invalid { query, .. }) => (false, false, "invalid after the corrections".into(), query),
            Ok(chat::Outcome::Failed { query, error }) => (false, true, format!("run failed: {}", error.lines().next().unwrap_or("")), Some(query)),
            Ok(chat::Outcome::Ran { query, result: got }) => {
                let want = d.run(c.reference.as_deref().unwrap_or(""), 60);
                // every result of the reference must be among the results of the model
                let ok = !want.outputs.is_empty() && want.outputs.iter().all(|w| got.outputs.iter().any(|g| same(&g.value, &w.value)));
                (ok, true, if ok { "same results".into() } else { "other results".into() }, Some(query))
            }
        };
        let e = score.entry(c.template.clone()).or_default();
        e.0 += 1;
        e.1 += ok as usize;
        e.2 += valid as usize;
        e.3 += seconds;
        eprintln!("[{}/{}] {} {:.1} s  {}  ({note})", i + 1, cases.len(), if ok { "✓" } else { "✗" }, seconds, question.replace('\n', " "));
        if let Some(f) = out.as_mut() {
            let row = json!({ "template": c.template, "turns": c.turns, "reference": c.reference, "expect_ask": c.expect_ask, "query": got, "ok": ok, "valid": valid, "note": note, "seconds": seconds });
            writeln!(f, "{row}").map_err(|e| e.to_string())?;
        }
    }
    let total = score.values().fold((0, 0, 0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2, a.3 + b.3));
    println!("{:<22} {:>6} {:>8} {:>8} {:>8}", "template", "cases", "correct", "valid", "s/case");
    for (t, (n, ok, valid, s)) in score.iter().chain(std::iter::once((&"all".to_string(), &total))) {
        println!("{t:<22} {n:>6} {:>7.0}% {:>7.0}% {:>8.1}", 100.0 * *ok as f64 / *n as f64, 100.0 * *valid as f64 / *n as f64, s / *n as f64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_agree_up_to_rounding() {
        assert!(same(&json!({"unit": "km", "value": 1.0000000001}), &json!({"unit": "km", "value": 1.0})));
        assert!(!same(&json!(94), &json!(95)));
        assert!(same(&json!([{"count": 3, "year": "2020"}]), &json!([{"year": "2020", "count": 3}])));
    }

    #[test]
    fn places_are_quoted_when_needed() {
        assert_eq!(templates::region("Vegesack"), "Vegesack");
        assert_eq!(templates::region("Horn-Lehe"), "'Horn-Lehe'");
        assert_eq!(templates::region("Östliche Vorstadt"), "'Östliche Vorstadt'");
    }

    #[test]
    fn mistakes_change_the_query() {
        let mut rng = Rng::new(3);
        let c = corruptions("count of nodes n with n:amenity=bench within 'Horn-Lehe'", &mut rng);
        assert!(!c.is_empty());
        assert!(c.iter().all(|b| b != "count of nodes n with n:amenity=bench within 'Horn-Lehe'"));
    }
}
