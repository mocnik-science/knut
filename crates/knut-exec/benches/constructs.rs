//! Performance of the constructs of the knut language, on one data set so that the
//! numbers can be compared with each other and across versions.
//!
//! The data set is a synthetic town generated deterministically (no files needed):
//! a street grid with speed changes, benches with histories, shops, buildings, parks,
//! four districts and 500 users. Its size grows with `KNUT_BENCH_SCALE` (default 1:
//! about 58 000 nodes, 6 000 ways and 94 000 versions). It is built once into the
//! target directory and reused.
//!
//! `python3 scripts/bench_lang.py [BASELINE [BASELINE]]` prints the results as a table
//! or compares two runs.
//!
//! ```bash
//! cargo bench -p knut-exec --bench constructs                         # all constructs
//! cargo bench -p knut-exec --bench constructs -- near                 # only some
//! cargo bench -p knut-exec --bench constructs -- --save-baseline a    # before a change
//! cargo bench -p knut-exec --bench constructs -- --baseline a         # compare after it
//! KNUT_BENCH_SCALE=4 cargo bench -p knut-exec --bench constructs      # larger town
//! KNUT_BENCH_DB=data/knut/bremen.knutdb KNUT_BENCH_PLACE=Vegesack \
//!     cargo bench -p knut-exec --bench constructs                     # a real database
//! ```
//!
//! Every benchmark runs a whole script (check, plan, scan, output to memory), as a user
//! would. `overhead` measures a script that does not touch the data; subtract it to see
//! the cost of a construct itself.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use knut_builder::{BuildOptions, Element, RawMeta, RawNode, RawRel, RawWay, VecSource};
use knut_exec::{Format, Options};
use knut_format::{Member, ObjType, Str, Tags, deg_to_e7};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ----- synthetic town ---------------------------------------------------------------------

const T0: i64 = 1_262_304_000; // 2010-01-01
const DAY: i64 = 86_400;
const YEARS: i64 = 13;

/// Deterministic random numbers (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Zipf-like choice among `n` (index 0 most frequent, every index possible).
    fn zipf(&mut self, n: usize) -> usize {
        let u = self.unit();
        (((n + 1) as f64).powf(u) - 1.0).floor() as usize % n
    }
}

struct Town {
    e: Vec<Element>,
    next_node: i64,
    next_way: i64,
}

fn tags(kv: &[(&str, &str)]) -> Tags {
    kv.iter().map(|(k, v)| (Str::from(*k), Str::from(*v))).collect::<Vec<_>>().into()
}

fn meta(version: u32, t: i64, uid: u32, visible: bool) -> RawMeta {
    RawMeta { version, timestamp: t, changeset: ((t - T0) / 3600) as u64 * 1000 + uid as u64, uid, user: Str::from(format!("u{uid}")), visible }
}

/// Position in a 10 km × 10 km square south-west of (8.75, 53.05).
fn pos(x: f64, y: f64) -> (i32, i32) {
    (deg_to_e7(8.75 + x * 0.15), deg_to_e7(53.05 + y * 0.09))
}

/// (time, user, position or `None` for a deletion, tags)
type NodeVersion<'a> = (i64, u32, Option<(i32, i32)>, Vec<(&'a str, String)>);
/// (time, user, node ids, tags)
type WayVersion<'a> = (i64, u32, Vec<i64>, Vec<(&'a str, String)>);

impl Town {
    fn node(&mut self, versions: Vec<NodeVersion>) -> i64 {
        let id = self.next_node;
        self.next_node += 1;
        for (i, (t, uid, p, kv)) in versions.into_iter().enumerate() {
            let (lon, lat) = p.unwrap_or((0, 0));
            let kv: Vec<(&str, &str)> = kv.iter().map(|(k, v)| (*k, v.as_str())).collect();
            self.e.push(Element::Node(RawNode { id, meta: meta(i as u32 + 1, t, uid, p.is_some()), tags: tags(&kv), lon, lat }));
        }
        id
    }

    fn plain_node(&mut self, t: i64, p: (i32, i32)) -> i64 {
        self.node(vec![(t, 1, Some(p), vec![])])
    }

    fn way(&mut self, versions: Vec<WayVersion>) -> i64 {
        let id = self.next_way;
        self.next_way += 1;
        for (i, (t, uid, refs, kv)) in versions.into_iter().enumerate() {
            let kv: Vec<(&str, &str)> = kv.iter().map(|(k, v)| (*k, v.as_str())).collect();
            self.e.push(Element::Way(RawWay { id, meta: meta(i as u32 + 1, t, uid, !refs.is_empty()), tags: tags(&kv), refs }));
        }
        id
    }

    fn ring(&mut self, t: i64, x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<i64> {
        let ids: Vec<i64> = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].into_iter().map(|(x, y)| self.plain_node(t, pos(x, y))).collect();
        vec![ids[0], ids[1], ids[2], ids[3], ids[0]]
    }
}

fn s(x: &str) -> String {
    x.to_string()
}

pub fn town(scale: usize) -> Vec<Element> {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut t = Town { e: Vec::new(), next_node: 1, next_way: 1 };
    let users = 500u64;
    let when = |r: &mut Rng| T0 + r.below((YEARS * 365 * DAY) as u64) as i64;
    let user = |r: &mut Rng| 1 + r.zipf(users as usize) as u32;

    // street grid: n × n intersections, one way per block edge
    let n = (40.0 * (scale as f64).sqrt()) as usize;
    let mut grid = vec![vec![0i64; n]; n];
    for (i, row) in grid.iter_mut().enumerate() {
        for (j, id) in row.iter_mut().enumerate() {
            *id = t.plain_node(T0, pos(i as f64 / n as f64, j as f64 / n as f64));
        }
    }
    let classes = ["residential", "residential", "residential", "tertiary", "secondary", "primary", "footway", "service"];
    for i in 0..n {
        for j in 0..n {
            for (di, dj) in [(1usize, 0usize), (0, 1)] {
                if i + di >= n || j + dj >= n {
                    continue;
                }
                // four shape points between intersections
                let (a, b) = (grid[i][j], grid[i + di][j + dj]);
                let mut refs = vec![a];
                for k in 1..5 {
                    let f = k as f64 / 5.0;
                    let x = (i as f64 + di as f64 * f) / n as f64;
                    let y = (j as f64 + dj as f64 * f) / n as f64;
                    refs.push(t.plain_node(T0, pos(x, y)));
                }
                refs.push(b);
                let hw = classes[r.zipf(classes.len())];
                let mut versions = Vec::new();
                let mut time = when(&mut r);
                let mut speed = [30, 50, 70][r.below(3) as usize];
                for _ in 0..1 + r.below(4) {
                    let mut kv = vec![("highway", s(hw))];
                    if hw != "footway" && r.below(3) > 0 {
                        kv.push(("maxspeed", speed.to_string()));
                    }
                    if r.below(4) == 0 {
                        kv.push(("name", format!("Street {}", (i * 7 + j) % 300)));
                    }
                    versions.push((time, user(&mut r), refs.clone(), kv));
                    time += 1 + r.below(400 * DAY as u64) as i64;
                    speed = [30, 50, 70][r.below(3) as usize];
                }
                t.way(versions);
            }
        }
    }
    // benches with histories (moves, backrests, deletions)
    for _ in 0..20_000 * scale {
        let (x, y) = (r.unit(), r.unit());
        let mut v = Vec::new();
        let mut time = when(&mut r);
        let uid = user(&mut r);
        v.push((time, uid, Some(pos(x, y)), vec![("amenity", s("bench"))]));
        for _ in 0..r.below(3) {
            time += 1 + r.below(300 * DAY as u64) as i64;
            let kv = if r.below(2) == 0 { vec![("amenity", s("bench")), ("backrest", s(["yes", "no"][r.below(2) as usize]))] } else { vec![("amenity", s("bench"))] };
            v.push((time, user(&mut r), Some(pos(x + r.unit() * 1e-4, y)), kv));
        }
        if r.below(10) == 0 {
            v.push((time + DAY, user(&mut r), None, vec![]));
        }
        t.node(v);
    }
    // shops and other points of interest
    let shops = ["bakery", "supermarket", "clothes", "hairdresser", "kiosk", "car_repair", "florist", "books", "bicycle", "butcher"];
    for _ in 0..8_000 * scale {
        let (x, y) = (r.unit(), r.unit());
        let kind = shops[r.zipf(shops.len())];
        let mut v = vec![(when(&mut r), user(&mut r), Some(pos(x, y)), vec![("shop", s(kind))])];
        if r.below(3) == 0 {
            let t1 = v[0].0 + 1 + r.below(500 * DAY as u64) as i64;
            v.push((t1, user(&mut r), Some(pos(x, y)), vec![("shop", s(kind)), ("name", format!("{kind} {}", r.below(1000)))]));
        }
        t.node(v);
    }
    for _ in 0..4_000 * scale {
        let kind = ["bicycle_parking", "waste_basket", "school", "library"][r.zipf(4)];
        t.node(vec![(when(&mut r), user(&mut r), Some(pos(r.unit(), r.unit())), vec![("amenity", s(kind))])]);
    }
    // buildings (closed ways, some with heights)
    for _ in 0..3_000 * scale {
        let (x, y) = (r.unit() * 0.99, r.unit() * 0.99);
        let tm = when(&mut r);
        let ring = t.ring(tm, x, y, x + 0.002, y + 0.003);
        let mut kv = vec![("building", s("yes"))];
        if r.below(2) == 0 {
            kv.push(("height", (3 + r.below(40)).to_string()));
        }
        t.way(vec![(tm, user(&mut r), ring, kv)]);
    }
    // parks as multipolygons and four districts
    let mut rels = Vec::new();
    for k in 0..20 * scale as i64 {
        let (x, y) = (r.unit() * 0.9, r.unit() * 0.9);
        let ring = t.ring(T0, x, y, x + 0.05, y + 0.05);
        let w = t.way(vec![(T0, 1, ring, vec![])]);
        rels.push(Element::Relation(RawRel {
            id: 1 + k,
            meta: meta(1, T0, 1, true),
            tags: tags(&[("type", "multipolygon"), ("leisure", "park")]),
            members: vec![Member { kind: ObjType::Way, id: w, role: Str::from("outer") }],
        }));
    }
    for (k, (x, y)) in [(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)].into_iter().enumerate() {
        let ring = t.ring(T0, x, y, x + 0.5, y + 0.5);
        let w = t.way(vec![(T0, 1, ring, vec![])]);
        rels.push(Element::Relation(RawRel {
            id: 1_000 + k as i64,
            meta: meta(1, T0, 1, true),
            tags: tags(&[("type", "boundary"), ("boundary", "administrative"), ("admin_level", "9"), ("name", &format!("D{}", k + 1))]),
            members: vec![Member { kind: ObjType::Way, id: w, role: Str::from("outer") }],
        }));
    }
    t.e.extend(rels);
    t.e
}

/// The database to measure on: `KNUT_BENCH_DB`, or the synthetic town (built once).
fn database() -> PathBuf {
    if let Ok(p) = std::env::var("KNUT_BENCH_DB") {
        return PathBuf::from(p);
    }
    let scale: usize = std::env::var("KNUT_BENCH_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("knut-bench");
    std::fs::create_dir_all(&dir).unwrap();
    // the generator version and the known format features are part of the name: a changed
    // generator or format builds a new file
    let path = dir.join(format!("town-v2-f{:x}-scale{scale}.knutdb", knut_format::layout::FEAT_KNOWN));
    if !path.exists() {
        let t = std::time::Instant::now();
        let data = town(scale);
        let n = data.len();
        let tmp = path.with_extension("tmp");
        knut_builder::build(&VecSource::new(data), &tmp, &BuildOptions::default(), &|_| {}).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        eprintln!("built {} ({n} versions) in {:.1} s", path.display(), t.elapsed().as_secs_f64());
    }
    path
}

// ----- constructs -------------------------------------------------------------------------

/// (name, script). `PLACE` is replaced by the place name of the database.
const CONSTRUCTS: &[(&str, &str)] = &[
    ("overhead", "1 + 1"),
    // selections and time
    ("state/count_key", "count of nodes n with n:amenity"),
    ("state/count_key_value", "count of nodes n with n:amenity=bench"),
    ("state/count_ways", "count of ways w with w:highway"),
    ("state/areas", "count of areas a with a:building"),
    ("state/residual_field", "count of nodes n with n:amenity=bench and n.version > 1"),
    ("state/past", "count of nodes n with n:amenity=bench on 2015-06-01"),
    ("state/materialize_1000", "pick 1000 nodes n with n:amenity=bench"),
    ("events/range", "count of nodes n with n:amenity=bench from 2014 to 2016"),
    ("events/deletions", "count of nodes n with n.deleted from 2010 to 2025"),
    ("series/on_each_month", "count of nodes n with n:amenity=bench on each month from 2015-01 to 2020-12"),
    ("series/per_month", "count of nodes n with n:amenity=bench per month from 2015-01 to 2020-12"),
    // picking and quantifiers
    ("pick/first", "pick node n with n:shop=books"),
    ("pick/random_100", "pick 100 nodes n randomly with n:amenity=bench"),
    ("quantifier/the", "s = nodes x with x:shop=books\n(pick n in s).id"),
    ("quantifier/some", "some n in (nodes x with x:shop=books) with n.version > 1"),
    // history
    ("sequence/pairs", "count of way A -> way B with A:maxspeed != B:maxspeed"),
    ("sequence/triples_window", "count of way A -> way B -> way C closer than 365 days with A:maxspeed == C:maxspeed and A:maxspeed != B:maxspeed"),
    ("history/previous", "count of nodes n with n:amenity=bench and n.previous:backrest != n:backrest"),
    // spatial
    ("spatial/within_box", "count of nodes n with n:amenity=bench within 8.77...8.82, 53.06...53.09"),
    ("spatial/within_place", "count of nodes n with n:amenity=bench within PLACE"),
    ("spatial/within_areas", "count of nodes n with n:amenity=bench within (areas p with p:leisure=park)"),
    ("spatial/near_some_100m", "libs = nodes l with l:amenity=library\ncount of nodes n with n:amenity=bench and n near some _ in libs closer than 100 m"),
    ("spatial/near_no_50m", "s = nodes x with x:amenity=waste_basket\ncount of nodes n with n:amenity=bench and n near no _ in s closer than 50 m"),
    ("spatial/nearest", "schools = nodes s with s:amenity=school\n(pick 200 nodes n with n:shop).map(n => (nearest of schools to n).id)"),
    ("spatial/area_sum", "sum of (areas b with b:building).map(b => b.area)"),
    ("spatial/length_sum", "sum of (ways w with w:highway=primary).map(w => w.length)"),
    // grouping and aggregation
    ("group/per_tag", "count of nodes s with s:shop per s:shop"),
    ("group/per_two_keys", "count of nodes s with s:shop per s:shop per year from 2010 to 2025"),
    ("group/per_cell_1km", "count of nodes n with n:amenity=bench per cell of 1 km"),
    ("group/per_tile_z14", "count of nodes n with n:amenity=bench per tile at zoom 14"),
    ("group/per_areas", "count of nodes n with n:amenity=bench per (areas d with d:boundary)"),
    ("group/share", "share of s in (nodes x with x:shop) per s:shop"),
    ("group/having", "count of nodes s with s:shop per s:shop having count > 100"),
    ("stats/distribution_binned", "distribution of (ways w with w:highway).map(w => w.length)"),
    ("stats/entropy", "entropy of (count of nodes s with s:shop per s:shop)"),
    ("stats/spread", "spread of nodes n with n:amenity=library"),
    ("stats/regress", "regress (count of nodes n with n:amenity=bench per year from 2010 to 2024) on (count of nodes s with s:shop per year from 2010 to 2024)"),
    ("stats/forecast", "forecast 3 years of (count of nodes n with n:amenity=bench on each year from 2010 to 2024)"),
    // expressions
    ("expr/map_units", "average of (ways b with b:building).map(b => (b:height ?? 0 m) * 2)"),
    ("expr/define_field", "define node N: age = N.version * 2\ncount of nodes n with n:amenity=bench and n.age > 2"),
    // users, changesets, keys
    ("meta/users", "count of users u with count of u.changesets > 100"),
    ("meta/changesets_range", "count of changesets from 2015 to 2016"),
    ("meta/keys", "count of keys k with k.count > 100"),
    // networks
    ("network/path", "a = pick node n with n:shop=books\nb = pick node n with n:amenity=library\np = path P from a to b by foot\np.length"),
    ("network/isochrone", "a = pick node n with n:shop=books\ni = isochrone I from a closer than 10 min by foot\ni.area"),
];

fn run(db: &Path, script: &str) {
    let opts = Options { db: Some(db.to_path_buf()), format: Format::Json, timing: false, ..Default::default() };
    let src = format!("ignore_warnings = true\n{script}");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = knut_exec::run_source(&src, Some("bench.knut"), Path::new("."), &opts, &mut out, &mut err);
    assert_eq!(code, 0, "{script}\n{}", String::from_utf8_lossy(&err));
    std::hint::black_box(out);
}

fn constructs(c: &mut Criterion) {
    let db = database();
    let place = std::env::var("KNUT_BENCH_PLACE").unwrap_or_else(|_| "D1".into());
    // the changeset index is built on first use; build it before measuring
    run(&db, "count of changesets");
    let mut g = c.benchmark_group("knut-lang");
    g.sample_size(10).warm_up_time(Duration::from_millis(500)).measurement_time(Duration::from_secs(3));
    for (name, script) in CONSTRUCTS {
        let script = script.replace("PLACE", &place);
        g.bench_with_input(BenchmarkId::from_parameter(name), &script, |b, s| b.iter(|| run(&db, s)));
    }
    g.finish();
}

criterion_group!(benches, constructs);
criterion_main!(benches);
