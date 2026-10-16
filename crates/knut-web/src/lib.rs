//! `knut web`: a web interface for the knut language on a MapLibre GL JS map.
//!
//! The server is small and local: it serves the page (embedded in the binary) and runs
//! scripts as jobs. A job runs in its own thread; the page polls its progress and can
//! cancel it.
//!
//! | Request | Answer |
//! | --- | --- |
//! | `GET /` and assets | the page |
//! | `GET /api/info` | default database, its extent and time range, map style |
//! | `POST /api/check` `{script, db?}` | `{problems}`: parse and check only, while typing |
//! | `POST /api/run` `{script, db?}` | `{job}` |
//! | `GET /api/job/ID` | `{state: running, progress}` or `{state: done, result}` |
//! | `POST /api/job/ID/cancel` | `{}` |

use knut_exec::control::Control;
use knut_exec::db::Db;
use knut_exec::{Collected, Options};
use serde_json::{Value as J, json};
use std::collections::HashMap;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const EDITOR_JS: &str = include_str!("../assets/editor.js");
const APP_CSS: &str = include_str!("../assets/app.css");
const EXAMPLES: &str = include_str!("../assets/examples.json");
const DOCS: &str = include_str!("../assets/docs.html");
const LOGO: &str = include_str!("../assets/knut.svg");
const ICONS: [(&str, &[u8]); 4] = [
    ("/icon-32.png", include_bytes!("../assets/icon-32.png")),
    ("/icon-180.png", include_bytes!("../assets/icon-180.png")),
    ("/icon-192.png", include_bytes!("../assets/icon-192.png")),
    ("/icon-512.png", include_bytes!("../assets/icon-512.png")),
];

/// Rows per result in the text shown next to the map.
const TEXT_ROWS: usize = 500;
/// Features per result drawn on the map.
const MAX_FEATURES: usize = 100_000;

pub struct WebOptions {
    /// Database used when the script names none.
    pub db: Option<PathBuf>,
    pub host: IpAddr,
    pub port: u16,
    /// Open the page in the default browser.
    pub open: bool,
    /// URL of a MapLibre style; default: OpenStreetMap raster tiles.
    pub style: Option<String>,
}

/// The examples shown in the page: `[{title, description, code}]`.
pub fn examples() -> Vec<Example> {
    serde_json::from_str(EXAMPLES).expect("examples.json is valid")
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Example {
    pub category: String,
    pub title: String,
    pub description: String,
    pub code: String,
}

struct Job {
    control: Arc<Control>,
    started: Instant,
    result: Mutex<Option<(Collected, f64)>>,
}

struct State {
    default_db: Option<PathBuf>,
    base_dir: PathBuf,
    style: Option<String>,
    dbs: Mutex<HashMap<PathBuf, Arc<Db>>>,
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
    next: AtomicU64,
}

impl State {
    /// The opened database for a path (opened once, then shared: keeps the block cache).
    fn db(&self, p: &Path) -> Option<Arc<Db>> {
        let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let mut m = self.dbs.lock().ok()?;
        if let Some(d) = m.get(&key) {
            return Some(d.clone());
        }
        let d = Arc::new(Db::open(p).ok()?);
        m.insert(key, d.clone());
        Some(d)
    }
}

/// Starts the server and blocks.
pub fn serve(opts: WebOptions) -> Result<(), String> {
    if let Some(d) = &opts.db
        && !d.exists()
    {
        return Err(format!("{}: no such file", d.display()));
    }
    // the first free port from the one asked for
    let mut server = None;
    let mut port = opts.port;
    for p in opts.port..opts.port.saturating_add(20) {
        if let Ok(s) = Server::http((opts.host, p)) {
            server = Some(s);
            port = p;
            break;
        }
    }
    let server = server.ok_or_else(|| format!("no free port from {} on", opts.port))?;
    let host = if opts.host.is_unspecified() { "localhost".to_string() } else { opts.host.to_string() };
    let url = format!("http://{host}:{port}/");
    eprintln!("knut web: {url}  (Ctrl+C to stop)");
    if let Some(d) = &opts.db {
        eprintln!("database: {}", d.display());
    }
    if opts.open {
        open_browser(&url);
    }
    let state = Arc::new(State {
        default_db: opts.db,
        base_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        style: opts.style,
        dbs: Mutex::new(HashMap::new()),
        jobs: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
    });
    // open the default database now, so that the first run is fast
    if let Some(d) = &state.default_db {
        let _ = state.db(d);
    }
    for req in server.incoming_requests() {
        let st = state.clone();
        std::thread::spawn(move || handle(&st, req));
    }
    Ok(())
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("cmd", vec!["/C", "start", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(cmd.0).args(cmd.1).spawn();
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

fn send(req: Request, status: u16, ctype: &str, body: String) {
    let r = Response::from_string(body).with_status_code(status).with_header(header("Content-Type", ctype)).with_header(header("Cache-Control", "no-store"));
    let _ = req.respond(r);
}

fn send_bytes(req: Request, ctype: &str, body: &[u8]) {
    let r = Response::from_data(body.to_vec()).with_status_code(200).with_header(header("Content-Type", ctype)).with_header(header("Cache-Control", "max-age=86400"));
    let _ = req.respond(r);
}

fn send_json(req: Request, status: u16, v: &J) {
    send(req, status, "application/json; charset=utf-8", v.to_string());
}

fn handle(st: &State, mut req: Request) {
    let url = req.url().split('?').next().unwrap_or("/").to_string();
    match (req.method(), url.as_str()) {
        (Method::Get, "/") | (Method::Get, "/index.html") => send(req, 200, "text/html; charset=utf-8", INDEX.to_string()),
        (Method::Get, "/app.js") => send(req, 200, "text/javascript; charset=utf-8", APP_JS.to_string()),
        (Method::Get, "/editor.js") => send(req, 200, "text/javascript; charset=utf-8", EDITOR_JS.to_string()),
        (Method::Get, "/app.css") => send(req, 200, "text/css; charset=utf-8", APP_CSS.to_string()),
        (Method::Get, "/examples.json") => send(req, 200, "application/json; charset=utf-8", EXAMPLES.to_string()),
        (Method::Get, "/docs.html") => send(req, 200, "text/html; charset=utf-8", DOCS.to_string()),
        (Method::Get, "/knut.svg") => send_bytes(req, "image/svg+xml", LOGO.as_bytes()),
        (Method::Get, path) if ICONS.iter().any(|i| i.0 == path) => {
            let icon = ICONS.iter().find(|i| i.0 == path).map_or(&[][..], |i| i.1);
            send_bytes(req, "image/png", icon)
        }
        (Method::Get, "/api/info") => {
            let info = info(st);
            send_json(req, 200, &info)
        }
        (Method::Post, "/api/run") => {
            let mut body = String::new();
            let _ = req.as_reader().take(16 << 20).read_to_string(&mut body);
            match serde_json::from_str::<J>(&body) {
                Ok(v) => {
                    let script = v["script"].as_str().unwrap_or_default().to_string();
                    let db = v["db"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from);
                    let id = start(st, script, db);
                    send_json(req, 200, &json!({ "job": id }))
                }
                Err(e) => send_json(req, 400, &json!({ "error": e.to_string() })),
            }
        }
        (Method::Post, "/api/check") => {
            let mut body = String::new();
            let _ = req.as_reader().take(16 << 20).read_to_string(&mut body);
            let v: J = serde_json::from_str(&body).unwrap_or(J::Null);
            let script = v["script"].as_str().unwrap_or_default().to_string();
            let typed = v["db"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from);
            let (_, path) = database_of(st, &script, typed);
            let shared_db = path.as_deref().and_then(|p| st.db(p));
            let opts = Options { shared_db, ..Default::default() };
            let problems = knut_exec::quick_check(&script, &opts);
            send_json(req, 200, &json!({ "problems": problems }))
        }
        (m, u) if u.starts_with("/api/job/") => {
            let rest = &u["/api/job/".len()..];
            let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
            let Ok(id) = id.parse::<u64>() else { return send_json(req, 404, &json!({ "error": "no such job" })) };
            let job = st.jobs.lock().ok().and_then(|j| j.get(&id).cloned());
            let Some(job) = job else { return send_json(req, 404, &json!({ "error": "no such job" })) };
            match (m, action) {
                (Method::Post, "cancel") => {
                    job.control.cancel();
                    send_json(req, 200, &json!({}))
                }
                (Method::Get, "") => {
                    let done = job.result.lock().ok().and_then(|mut r| r.take());
                    match done {
                        Some((result, secs)) => {
                            if let Ok(mut j) = st.jobs.lock() {
                                j.remove(&id);
                            }
                            let state = if result.cancelled { "cancelled" } else { "done" };
                            send_json(req, 200, &json!({ "state": state, "seconds": secs, "result": result }))
                        }
                        None => send_json(
                            req,
                            200,
                            &json!({ "state": "running", "seconds": job.started.elapsed().as_secs_f64(), "cancelling": job.control.is_cancelled(), "progress": job.control.progress() }),
                        ),
                    }
                }
                _ => send_json(req, 405, &json!({ "error": "method not allowed" })),
            }
        }
        _ => send(req, 404, "text/plain; charset=utf-8", "not found".into()),
    }
}

fn info(st: &State) -> J {
    let db = st.default_db.as_ref().and_then(|p| st.db(p));
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "db": st.default_db.as_ref().map(|p| p.display().to_string()),
        "bbox": db.as_ref().map(|d| {
            let f = d.focus();
            [f.minx, f.miny, f.maxx, f.maxy]
        }),
        "time": db.as_ref().map(|d| [knut_lang::dates::format_instant(d.earliest), knut_lang::dates::format_instant(d.latest)]),
        "style": st.style,
    })
}

/// The database a run uses: the one typed in the page, else `knut_db` of the script,
/// else the default.
fn database_of(st: &State, script: &str, typed: Option<PathBuf>) -> (Option<PathBuf>, Option<PathBuf>) {
    if let Some(p) = typed {
        let p = if p.is_absolute() { p } else { st.base_dir.join(p) };
        return (Some(p.clone()), Some(p));
    }
    let (parsed, _) = knut_lang::parse(script);
    if let Some((p, _)) = knut_plan::settings_of(&parsed).knut_db {
        let p = PathBuf::from(p);
        let p = if p.is_absolute() { p } else { st.base_dir.join(p) };
        // the script names it: no override, so that messages point at `knut_db`
        return (None, Some(p));
    }
    (st.default_db.clone(), st.default_db.clone())
}

fn start(st: &State, script: String, typed: Option<PathBuf>) -> u64 {
    let id = st.next.fetch_add(1, Ordering::Relaxed);
    let control = Control::new();
    let job = Arc::new(Job { control: control.clone(), started: Instant::now(), result: Mutex::new(None) });
    if let Ok(mut j) = st.jobs.lock() {
        j.insert(id, job.clone());
    }
    let (db, path) = database_of(st, &script, typed);
    let shared_db = path.as_deref().and_then(|p| st.db(p));
    let opts = Options { db, limit: TEXT_ROWS, timing: false, control: Some(control), shared_db, ..Default::default() };
    let base = st.base_dir.clone();
    std::thread::spawn(move || {
        let result = knut_exec::run_collect(&script, Some("query.knut"), &base, &opts, MAX_FEATURES);
        if let Ok(mut r) = job.result.lock() {
            *r = Some((result, job.started.elapsed().as_secs_f64()));
        }
    });
    id
}
