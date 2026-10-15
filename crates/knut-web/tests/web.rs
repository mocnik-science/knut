//! knut web: the examples and the HTTP interface.

use knut_builder::{BuildOptions, Element, RawMeta, RawNode, VecSource};
use knut_format::{Str, Tags, deg_to_e7};
use serde_json::{Value as J, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
fn hundred_examples_that_pass_the_check() {
    let ex = knut_web::examples();
    assert_eq!(ex.len(), 100);
    for e in &ex {
        assert!(!e.title.is_empty() && !e.description.is_empty(), "{e:?}");
        // one or two sentences
        assert!(e.description.matches(". ").count() <= 1, "too long: {}", e.description);
        let problems = knut_exec::quick_check(&e.code, &knut_exec::Options::default());
        let errors: Vec<_> = problems.iter().filter(|p| p.severity == "error").collect();
        assert!(errors.is_empty(), "{}:\n{}\n{}", e.title, e.code, errors.iter().map(|p| p.text.as_str()).collect::<String>());
    }
}

/// Runs every example on a real database: `KNUT_WEB_EXAMPLES_DB=data/knut/bremen.knutdb`.
#[test]
fn examples_run_on_a_real_database() {
    let Ok(db) = std::env::var("KNUT_WEB_EXAMPLES_DB") else { return };
    for e in knut_web::examples() {
        let opts = knut_exec::Options { db: Some(PathBuf::from(&db)), timing: false, ..Default::default() };
        let r = knut_exec::run_collect(&e.code, Some("example.knut"), std::path::Path::new("."), &opts, 10);
        assert_eq!(r.code, 0, "{}:\n{}\n{}", e.title, e.code, r.messages);
    }
}

// ----- HTTP ---------------------------------------------------------------------------

fn small_db() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knut-web-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.knutdb");
    let tags: Tags = vec![(Str::from("amenity"), Str::from("bench"))].into();
    let e: Vec<Element> = (1..=5)
        .map(|i| {
            Element::Node(RawNode {
                id: i,
                meta: RawMeta { version: 1, timestamp: 1_420_070_400 + i, changeset: 1, uid: 1, user: Str::from("u"), visible: true },
                tags: tags.clone(),
                lon: deg_to_e7(8.8 + i as f64 * 0.001),
                lat: deg_to_e7(53.07),
            })
        })
        .collect();
    knut_builder::build(&VecSource::new(e), &path, &BuildOptions::default(), &|_| {}).unwrap();
    path
}

fn request(port: u16, method: &str, path: &str, body: Option<&J>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default();
    (status, body)
}

fn json_of(port: u16, method: &str, path: &str, body: Option<&J>) -> J {
    let (status, b) = request(port, method, path, body);
    assert_eq!(status, 200, "{path}: {b}");
    serde_json::from_str(&b).unwrap_or_else(|e| panic!("{path}: {e}: {b}"))
}

#[test]
fn http_interface() {
    let db = small_db();
    let port = 18_000 + (std::process::id() % 2000) as u16;
    let opts = knut_web::WebOptions { db: Some(db.clone()), host: "127.0.0.1".parse().unwrap(), port, open: false, style: None };
    std::thread::spawn(move || knut_web::serve(opts).unwrap());
    // wait for the server
    let t0 = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(10), "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    let (status, page) = request(port, "GET", "/", None);
    assert_eq!(status, 200);
    assert!(page.contains("vue@3") && page.contains("maplibre-gl") && page.contains("importmap"));
    for asset in ["/app.js", "/editor.js", "/app.css", "/docs.html", "/examples.json"] {
        assert_eq!(request(port, "GET", asset, None).0, 200, "{asset}");
    }

    let info = json_of(port, "GET", "/api/info", None);
    assert!(info["db"].as_str().unwrap().ends_with("t.knutdb"));
    let b = info["bbox"].as_array().unwrap();
    assert!(b[0].as_f64().unwrap() > 8.79 && b[2].as_f64().unwrap() < 8.81, "{info}");

    // the check while typing reports positions
    let c = json_of(port, "POST", "/api/check", Some(&json!({ "script": "count of libary" })));
    let p = &c["problems"][0];
    assert_eq!((p["code"].as_str(), p["from"].as_u64(), p["to"].as_u64()), (Some("E201"), Some(9), Some(15)), "{c}");

    // a run: job, then the result with text, value and GeoJSON
    let job = json_of(port, "POST", "/api/run", Some(&json!({ "script": "count of nodes n with n:amenity=bench\nnodes n with n:amenity=bench" })))["job"].as_u64().unwrap();
    let result = loop {
        let j = json_of(port, "GET", &format!("/api/job/{job}"), None);
        if j["state"] != "running" {
            break j;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(result["state"], "done", "{result}");
    let outs = result["result"]["outputs"].as_array().unwrap();
    assert_eq!(outs[0]["value"], 5);
    assert_eq!(outs[1]["features"], 5);
    assert_eq!(outs[1]["geojson"]["features"][0]["geometry"]["type"], "Point");
    // a finished job is fetched once
    assert_eq!(request(port, "GET", &format!("/api/job/{job}"), None).0, 404);

    // cancelling
    let job = json_of(port, "POST", "/api/run", Some(&json!({ "script": "count of nodes n with n:amenity=bench" })))["job"].as_u64().unwrap();
    let _ = request(port, "POST", &format!("/api/job/{job}/cancel"), None);
    let result = loop {
        let (status, b) = request(port, "GET", &format!("/api/job/{job}"), None);
        assert_eq!(status, 200);
        let j: J = serde_json::from_str(&b).unwrap();
        if j["state"] != "running" {
            break j;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // the run may have finished before the cancel arrived
    assert!(result["state"] == "cancelled" || result["state"] == "done", "{result}");
}
