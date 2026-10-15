//! Shared synthetic data and helpers for the knut language tests.
//!
//! The data set is a small "town" whose expected results are computed by hand. All times
//! are relative to T0 = 2015-01-01T00:00:00Z; positions are offsets in degrees from
//! (8.80 E, 53.07 N), where 0.001° of longitude ≈ 66.8 m and 0.001° of latitude ≈ 111.2 m.
//!
//! | ids | what | facts used by the tests |
//! | --- | --- | --- |
//! | nodes 1–10 | benches at (0.001·i, 0), created T0 + i min by user 7 | bench 3 deleted at T0 + 100 d; bench 4 v2 (T0 + 50 d, user 8) adds `backrest=yes` |
//! | node 50 | bench at (0, 0.01), moves to (0.3, 0.01) at T0 + 10 d (new segment), deleted T0 + 20 d | |
//! | node 61 | `tourism=viewpoint`, v1 T0, v2 T0 + 2 d `name=View`, v3 T0 + 12 d `name=Aussicht` | |
//! | nodes 20, 21 | bicycle parking next to the library (≈ 33 m) and far away | |
//! | way 100 | library, closed square 0.001° × 0.001° (≈ 7 424 m²), `name=Lib` | |
//! | relation 900 | park multipolygon: outer 0.002° × 0.002° (≈ 29 700 m²), hole 0.001° × 0.001° | area ≈ 22 270 m² |
//! | way 200 | residential road, 11 nodes from (0, −0.002) to (0.010, −0.002), `maxspeed=30` | 10 edges ≈ 667.6 m |
//! | way 230 | motorway, 2 nodes, not connected | |
//! | way 400 | edit war: v1 (T0 + 1 d, user 1) `maxspeed=50`, v2 (T0 + 5 d, user 2) `30`, v3 (T0 + 10 d, user 1) `50` | |
//! | nodes 600–619 | shops created T0 + i d: 10 `bakery`, 6 `clothes`, 4 `supermarket` | shop 600 v2 (T0 + 40 d) adds a name |
//! | nodes 700–702 | shelters with `capacity` 12, "about 200", none | |
//! | ways 800–804 | buildings: `height` 25, "12 m", "0.03 km", none, `building:levels=4` | |
//! | relation 950 | boundary `Mitte` (admin_level 8) around benches 1–5 | |
//! | relations 960, 970 | two boundaries named `Springfield` (admin_level 8) | |
//! | node 990 | hamlet 2° east and 1° north: makes the data extent large (W003) | |

#![allow(dead_code)]

use knut_builder::{BuildOptions, Element, RawMeta, RawNode, RawRel, RawWay, VecSource};
use knut_exec::{Format, Options};
use knut_format::{Member, ObjType, Str, Tags, deg_to_e7};
pub use serde_json::Value as J;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const T0: i64 = 1_420_070_400; // 2015-01-01
pub const DAY: i64 = 86_400;

fn tags(kv: &[(&str, &str)]) -> Tags {
    kv.iter().map(|(k, v)| (Str::from(*k), Str::from(*v))).collect::<Vec<_>>().into()
}

fn meta(version: u32, t: i64, uid: u32, visible: bool) -> RawMeta {
    // one changeset per user and hour (a changeset belongs to one user)
    RawMeta { version, timestamp: t, changeset: 100_000 + (t - T0) as u64 / 3600 * 100 + uid as u64, uid, user: Str::from(format!("user{uid}")), visible }
}

pub fn pos(dx: f64, dy: f64) -> (i32, i32) {
    (deg_to_e7(8.80 + dx), deg_to_e7(53.07 + dy))
}

fn node(id: i64, v: u32, t: i64, uid: u32, p: Option<(i32, i32)>, kv: &[(&str, &str)]) -> Element {
    let (lon, lat) = p.unwrap_or((0, 0));
    Element::Node(RawNode { id, meta: meta(v, t, uid, p.is_some()), tags: tags(kv), lon, lat })
}

fn way(id: i64, v: u32, t: i64, uid: u32, refs: &[i64], kv: &[(&str, &str)]) -> Element {
    Element::Way(RawWay { id, meta: meta(v, t, uid, !refs.is_empty()), tags: tags(kv), refs: refs.to_vec() })
}

fn rel(id: i64, t: i64, members: &[(ObjType, i64, &str)], kv: &[(&str, &str)]) -> Element {
    Element::Relation(RawRel {
        id,
        meta: meta(1, t, 7, true),
        tags: tags(kv),
        members: members.iter().map(|&(k, i, r)| Member { kind: k, id: i, role: Str::from(r) }).collect(),
    })
}

/// A closed rectangular ring of four new nodes `first..first+3`.
fn ring(e: &mut Vec<Element>, first: i64, x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<i64> {
    for (i, (x, y)) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].into_iter().enumerate() {
        e.push(node(first + i as i64, 1, T0, 7, Some(pos(x, y)), &[]));
    }
    vec![first, first + 1, first + 2, first + 3, first]
}

pub fn town() -> Vec<Element> {
    let mut e = Vec::new();
    // benches
    for i in 1..=10 {
        e.push(node(i, 1, T0 + i * 60, 7, Some(pos(i as f64 * 0.001, 0.0)), &[("amenity", "bench")]));
    }
    e.push(node(3, 2, T0 + 100 * DAY, 7, None, &[]));
    e.push(node(4, 2, T0 + 50 * DAY, 8, Some(pos(0.004, 0.0)), &[("amenity", "bench"), ("backrest", "yes")]));
    e.push(node(50, 1, T0, 7, Some(pos(0.0, 0.01)), &[("amenity", "bench")]));
    e.push(node(50, 2, T0 + 10 * DAY, 7, Some(pos(0.3, 0.01)), &[("amenity", "bench")]));
    e.push(node(50, 3, T0 + 20 * DAY, 7, None, &[]));
    // a viewpoint with three versions
    e.push(node(61, 1, T0, 7, Some(pos(0.005, 0.005)), &[("tourism", "viewpoint")]));
    e.push(node(61, 2, T0 + 2 * DAY, 7, Some(pos(0.005, 0.005)), &[("tourism", "viewpoint"), ("name", "View")]));
    e.push(node(61, 3, T0 + 12 * DAY, 7, Some(pos(0.005, 0.005)), &[("tourism", "viewpoint"), ("name", "Aussicht")]));
    // library and bicycle parking
    let r = ring(&mut e, 101, 0.0, 0.002, 0.001, 0.003);
    e.push(way(100, 1, T0 + 60, 7, &r, &[("amenity", "library"), ("name", "Lib")]));
    e.push(node(20, 1, T0, 7, Some(pos(0.0015, 0.0025)), &[("amenity", "bicycle_parking")]));
    e.push(node(21, 1, T0, 7, Some(pos(0.05, 0.05)), &[("amenity", "bicycle_parking")]));
    // park with a hole
    let outer = ring(&mut e, 911, 0.010, 0.000, 0.012, 0.002);
    let inner = ring(&mut e, 921, 0.0105, 0.0005, 0.0115, 0.0015);
    e.push(way(901, 1, T0, 7, &outer, &[]));
    e.push(way(902, 1, T0, 7, &inner, &[]));
    e.push(rel(900, T0, &[(ObjType::Way, 901, "outer"), (ObjType::Way, 902, "inner")], &[("type", "multipolygon"), ("leisure", "park"), ("name", "Park")]));
    // roads
    for i in 0..=10 {
        e.push(node(300 + i, 1, T0, 7, Some(pos(i as f64 * 0.001, -0.002)), &[]));
    }
    let refs: Vec<i64> = (300..=310).collect();
    e.push(way(200, 1, T0, 7, &refs, &[("highway", "residential"), ("maxspeed", "30")]));
    e.push(node(330, 1, T0, 7, Some(pos(0.0, -0.01)), &[]));
    e.push(node(331, 1, T0, 7, Some(pos(0.01, -0.01)), &[]));
    e.push(way(230, 1, T0, 7, &[330, 331], &[("highway", "motorway")]));
    // edit war
    e.push(node(401, 1, T0, 7, Some(pos(0.0, -0.004)), &[]));
    e.push(node(402, 1, T0, 7, Some(pos(0.002, -0.004)), &[]));
    e.push(way(400, 1, T0 + DAY, 1, &[401, 402], &[("highway", "primary"), ("maxspeed", "50")]));
    e.push(way(400, 2, T0 + 5 * DAY, 2, &[401, 402], &[("highway", "primary"), ("maxspeed", "30")]));
    e.push(way(400, 3, T0 + 10 * DAY, 1, &[401, 402], &[("highway", "primary"), ("maxspeed", "50")]));
    // shops
    for i in 0..20 {
        let kind = if i < 10 { "bakery" } else if i < 16 { "clothes" } else { "supermarket" };
        e.push(node(600 + i, 1, T0 + i * DAY, 7, Some(pos(0.02 + 0.0001 * i as f64, 0.0)), &[("shop", kind)]));
    }
    e.push(node(600, 2, T0 + 40 * DAY, 7, Some(pos(0.02, 0.0)), &[("shop", "bakery"), ("name", "Bäcker Eins")]));
    // shelters
    e.push(node(700, 1, T0, 7, Some(pos(0.04, 0.0)), &[("amenity", "shelter"), ("capacity", "12")]));
    e.push(node(701, 1, T0, 7, Some(pos(0.041, 0.0)), &[("amenity", "shelter"), ("capacity", "about 200")]));
    e.push(node(702, 1, T0, 7, Some(pos(0.042, 0.0)), &[("amenity", "shelter")]));
    // buildings
    let heights: [&[(&str, &str)]; 5] = [
        &[("building", "yes"), ("height", "25")],
        &[("building", "yes"), ("height", "12 m")],
        &[("building", "yes"), ("height", "0.03 km")],
        &[("building", "yes")],
        &[("building", "yes"), ("building:levels", "4")],
    ];
    for (b, kv) in heights.iter().enumerate() {
        let x = 0.03 + 0.001 * b as f64;
        let r = ring(&mut e, 810 + 4 * b as i64, x, 0.0, x + 0.0002, 0.0002);
        e.push(way(800 + b as i64, 1, T0, 7, &r, kv));
    }
    // boundaries
    let r = ring(&mut e, 951, -0.0005, -0.0005, 0.0055, 0.0005);
    e.push(way(951, 1, T0, 7, &r, &[]));
    e.push(rel(950, T0, &[(ObjType::Way, 951, "outer")], &[("type", "boundary"), ("boundary", "administrative"), ("admin_level", "8"), ("name", "Mitte")]));
    for (i, (x, y)) in [(0.05, 0.05), (0.06, 0.06)].into_iter().enumerate() {
        let id = 960 + 10 * i as i64;
        let r = ring(&mut e, id + 1, x, y, x + 0.002, y + 0.002);
        e.push(way(id + 1, 1, T0, 7, &r, &[]));
        e.push(rel(id, T0, &[(ObjType::Way, id + 1, "outer")], &[("type", "boundary"), ("boundary", "administrative"), ("admin_level", "8"), ("name", "Springfield")]));
    }
    // far away
    e.push(node(990, 1, T0, 7, Some(pos(2.0, 1.0)), &[("place", "hamlet")]));
    e
}

fn build(name: &str, opt: BuildOptions) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knut-lang-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("town.knutdb");
    let opt = BuildOptions { block_size: 2048, zstd_dict: false, keyframe_interval: 2, ..opt };
    knut_builder::build(&VecSource::new(town()), &path, &opt, &|_| {}).unwrap();
    path
}

/// A road that was rebuilt 50 km away as a secondary road. Way 1 gets two segments, the
/// primary one at the old place and the secondary one at the new place; each has 150 nodes,
/// so that they do not fit into one block. The block at the new place holds no
/// `highway=primary`.
pub fn moved_road() -> Vec<Element> {
    let road = [("highway", "primary"), ("name", "Old Road")];
    let mut e = Vec::new();
    for i in 0..150 {
        e.push(node(10_000 + i, 1, T0, 7, Some(pos(i as f64 * 1e-4, 0.0)), &[]));
        e.push(node(20_000 + i, 1, T0, 7, Some(pos(0.7 + i as f64 * 1e-4, 0.0)), &[]));
    }
    let old: Vec<i64> = (10_000..10_150).collect();
    let new: Vec<i64> = (20_000..20_150).collect();
    e.push(way(1, 1, T0, 7, &old, &road));
    e.push(way(1, 2, T0 + 100 * DAY, 7, &new, &[("highway", "secondary"), ("name", "Old Road")]));
    // the same tags elsewhere, so that they are in the dictionaries
    e.push(way(2, 1, T0, 7, &old[..2], &road));
    e.push(way(3, 1, T0, 7, &old[..2], &[("highway", "secondary")]));
    e
}

pub fn db_moved_road() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knut-lang-test-{}-moved", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("moved.knutdb");
    let opt = BuildOptions { block_size: 2048, zstd_dict: false, min_value_count: 1, ..Default::default() };
    knut_builder::build(&VecSource::new(moved_road()), &path, &opt, &|_| {}).unwrap();
    path
}

/// The town database (built once per test binary).
pub fn db() -> &'static Path {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| build("main", BuildOptions::default()))
}

/// The town built with `--strip-users`.
pub fn db_without_users() -> &'static Path {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| build("nousers", BuildOptions { strip_users: true, ..Default::default() }))
}

/// A private copy of the town (for tests that add side files such as indexes).
pub fn db_copy(name: &str) -> PathBuf {
    build(name, BuildOptions::default())
}

/// A scratch directory for files a test writes.
pub fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("knut-lang-test-{}-scratch-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
}

pub fn run_on(db: &Path, script: &str, format: Format, out_dir: Option<PathBuf>) -> Run {
    let opts = Options { db: Some(db.to_path_buf()), format, out_dir, timing: false, ..Default::default() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = knut_exec::run_source(script, Some("t.knut"), Path::new("."), &opts, &mut out, &mut err);
    Run { code, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() }
}

pub fn run(script: &str) -> Run {
    run_on(db(), script, Format::Json, None)
}

pub fn run_text(script: &str) -> Run {
    run_on(db(), script, Format::Text, None)
}

/// Values of all outputs of a script that must succeed.
pub fn values_on(db: &Path, script: &str) -> Vec<J> {
    let r = run_on(db, script, Format::Json, None);
    assert_eq!(r.code, 0, "script failed:\n{script}\n---\n{}\n{}", r.err, r.out);
    let j: J = serde_json::from_str(&r.out).unwrap_or_else(|e| panic!("bad JSON ({e}):\n{}", r.out));
    j.as_array().unwrap().iter().map(|o| o["value"].clone()).collect()
}

pub fn values(script: &str) -> Vec<J> {
    values_on(db(), script)
}

/// The value of the last output.
pub fn one(script: &str) -> J {
    values(script).pop().expect("no output")
}

pub fn num(j: &J) -> f64 {
    match j {
        J::Number(n) => n.as_f64().unwrap(),
        J::Object(o) if o.contains_key("value") => num(&o["value"]),
        other => panic!("not a number: {other}"),
    }
}

/// Unit of a number output (`""` for plain numbers).
pub fn unit(j: &J) -> String {
    match j {
        J::Object(o) => o.get("unit").and_then(|u| u.as_str()).unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

/// A number output in base units (m, m², s, m/s), whatever unit it is shown in.
pub fn base(j: &J) -> f64 {
    let u = unit(j);
    if u.is_empty() {
        return num(j);
    }
    let unit = knut_lang::units::unit(&u).unwrap_or_else(|| panic!("unknown unit {u}"));
    num(j) * unit.factor
}

pub fn approx(j: &J, want: f64, rel_tol: f64) {
    let x = num(j);
    assert!((x - want).abs() <= rel_tol * want.abs().max(1e-12), "{x} is not within {rel_tol} of {want}");
}

/// Rows of a grouped result as `(key, value)` with the key and value columns given by name.
pub fn rows(j: &J, key: &str, val: &str) -> Vec<(J, J)> {
    j.as_array().unwrap_or_else(|| panic!("not a table: {j}")).iter().map(|r| (r[key].clone(), r[val].clone())).collect()
}

/// Asserts that a script is rejected or stopped with `code` and returns the run.
#[track_caller]
pub fn fails(script: &str, code: &str) -> Run {
    let r = run_text(script);
    assert_ne!(r.code, 0, "expected {code}, but the script ran:\n{script}\n{}", r.out);
    assert!(r.err.contains(&format!("[{code}]")), "expected {code}:\n{script}\n---\n{}", r.err);
    r
}

/// Asserts that a script reports `code` (anywhere in the messages) and returns the run.
#[track_caller]
pub fn reports(script: &str, code: &str) -> Run {
    let r = run_text(script);
    assert!(r.err.contains(&format!("[{code}]")), "expected {code}:\n{script}\n---\n{}", r.err);
    r
}

/// Asserts that a script passes the checks without messages.
#[track_caller]
pub fn clean(script: &str) {
    let r = run_text(script);
    assert_eq!(r.code, 0, "{script}\n---\n{}", r.err);
    assert!(r.err.trim().is_empty(), "unexpected messages:\n{script}\n---\n{}", r.err);
}
