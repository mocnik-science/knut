//! End-to-end tests on synthetic histories: build → read → query.

use knut_builder::{BuildOptions, Element, RawMeta, RawNode, RawRel, RawWay, VecSource};
use knut_format::layout::TREE_LARGE;
use knut_format::{BBox, Member, ObjType, Str, Tags, deg_to_e7};
use knut::{Filter, Geometry, OpenOptions, Reader};
use std::collections::BTreeMap;

const T0: i64 = 1_300_000_000; // 2011-03-13
const DAY: i64 = 86_400;

fn tags(kv: &[(&str, &str)]) -> Tags {
    kv.iter().map(|(k, v)| (Str::from(*k), Str::from(*v))).collect::<Vec<_>>().into()
}

fn meta(version: u32, t: i64, visible: bool) -> RawMeta {
    RawMeta {
        version,
        timestamp: t,
        changeset: 1000 + (t - T0) as u64 / 3600,
        uid: 42 + version,
        user: Str::from(format!("user{}", 42 + version)),
        visible,
    }
}

/// Bremen-ish coordinates.
fn bremen(dx: f64, dy: f64) -> (i32, i32) {
    (deg_to_e7(8.80 + dx), deg_to_e7(53.07 + dy))
}

fn node(id: i64, v: u32, t: i64, pos: Option<(i32, i32)>, kv: &[(&str, &str)]) -> Element {
    let (lon, lat) = pos.unwrap_or((0, 0));
    Element::Node(RawNode { id, meta: meta(v, t, pos.is_some()), tags: tags(kv), lon, lat })
}

fn way(id: i64, v: u32, t: i64, refs: &[i64], kv: &[(&str, &str)]) -> Element {
    Element::Way(RawWay { id, meta: meta(v, t, !refs.is_empty()), tags: tags(kv), refs: refs.to_vec() })
}

fn rel(id: i64, v: u32, t: i64, members: &[(ObjType, i64, &str)], kv: &[(&str, &str)]) -> Element {
    Element::Relation(RawRel {
        id,
        meta: meta(v, t, !members.is_empty()),
        tags: tags(kv),
        members: members.iter().map(|&(k, i, r)| Member { kind: k, id: i, role: Str::from(r) }).collect(),
    })
}

/// A small town grid plus vandalism cases.
fn dataset() -> Vec<Element> {
    let mut e = Vec::new();
    // Grid of 20x20 tagged POIs (shared nodes) with a few versions each.
    for i in 0..20 {
        for j in 0..20 {
            let id = 1 + i * 20 + j;
            let p = bremen(i as f64 * 0.001, j as f64 * 0.001);
            e.push(node(id, 1, T0 + id * 60, Some(p), &[("amenity", "bench")]));
            if id % 3 == 0 {
                e.push(node(id, 2, T0 + 400 * DAY + id, Some((p.0 + 5, p.1 + 5)), &[("amenity", "bench"), ("backrest", "yes")]));
            }
        }
    }
    // Streets: ways with private nodes 1000.. and shared junction nodes.
    let mut nid = 1000;
    for s in 0..30 {
        let wid = 100 + s;
        let mut refs = Vec::new();
        for k in 0..6 {
            let p = bremen(s as f64 * 0.0007, k as f64 * 0.0005);
            e.push(node(nid, 1, T0 + DAY, Some(p), &[]));
            refs.push(nid);
            nid += 1;
        }
        // shared junction node: POI 1 is in every street
        refs.push(1);
        e.push(way(wid, 1, T0 + DAY, &refs, &[("highway", "residential")]));
        e.push(way(wid, 2, T0 + 200 * DAY, &refs[1..], &[("highway", "residential"), ("name", "Straße")]));
    }

    // Vandalism 1: shared node 5000 (tagged) moved to 0,0 and back.
    let home = bremen(0.01, 0.01);
    e.push(node(5000, 1, T0 + 10 * DAY, Some(home), &[("shop", "bakery")]));
    e.push(node(5000, 2, T0 + 500 * DAY, Some((0, 0)), &[("shop", "bakery")]));
    e.push(node(5000, 3, T0 + 501 * DAY, Some(home), &[("shop", "bakery")]));

    // Vandalism 2: private node 6001 of way 6000 moved to 0,0 and back.
    e.push(node(6000 + 1, 1, T0 + 20 * DAY, Some(bremen(0.002, 0.003)), &[]));
    e.push(node(6000 + 1, 2, T0 + 600 * DAY, Some((0, 0)), &[]));
    e.push(node(6000 + 1, 3, T0 + 601 * DAY, Some(bremen(0.002, 0.003)), &[]));
    e.push(node(6002, 1, T0 + 20 * DAY, Some(bremen(0.003, 0.003)), &[]));
    e.push(node(6003, 1, T0 + 20 * DAY, Some(bremen(0.003, 0.004)), &[]));
    e.push(way(6000, 1, T0 + 20 * DAY, &[6001, 6002, 6003], &[("building", "yes")]));

    // Vandalism 3: way 7000 replaced by nodes across the globe, then reverted.
    e.push(node(7001, 1, T0 + 30 * DAY, Some(bremen(0.004, 0.0)), &[]));
    e.push(node(7002, 1, T0 + 30 * DAY, Some(bremen(0.005, 0.0)), &[]));
    e.push(node(7003, 1, T0 + 700 * DAY, Some((deg_to_e7(-120.0), deg_to_e7(-40.0))), &[]));
    e.push(node(7004, 1, T0 + 700 * DAY, Some((deg_to_e7(150.0), deg_to_e7(60.0))), &[]));
    e.push(way(7000, 1, T0 + 30 * DAY, &[7001, 7002], &[("highway", "path")]));
    e.push(way(7000, 2, T0 + 700 * DAY, &[7001, 7003, 7004, 7002], &[("highway", "path")]));
    e.push(way(7000, 3, T0 + 702 * DAY, &[7001, 7002], &[("highway", "path")]));

    // Multiple lifespans: node 8000 created, deleted, recreated; versions with a gap.
    e.push(node(8000, 1, T0 + 50 * DAY, Some(bremen(0.006, 0.006)), &[("amenity", "atm")]));
    e.push(node(8000, 2, T0 + 60 * DAY, None, &[]));
    e.push(node(8000, 5, T0 + 90 * DAY, Some(bremen(0.006, 0.006)), &[("amenity", "atm")]));

    // Relation with way members and a child relation.
    e.push(rel(9000, 1, T0 + 40 * DAY, &[(ObjType::Way, 100, "outer"), (ObjType::Way, 101, "inner")], &[("type", "multipolygon")]));
    e.push(rel(9000, 2, T0 + 300 * DAY, &[(ObjType::Way, 100, "outer"), (ObjType::Way, 102, "inner"), (ObjType::Node, 2, "label")], &[("type", "multipolygon")]));
    e.push(rel(9001, 1, T0 + 41 * DAY, &[(ObjType::Relation, 9000, "sub")], &[("type", "collection")]));
    e
}

fn build(elements: Vec<Element>, opt: BuildOptions) -> Reader {
    let dir = std::env::temp_dir().join(format!("knut-test-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.knutdb");
    let src = VecSource::new(elements);
    knut_builder::build(&src, &path, &opt, &|_| {}).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    Reader::from_bytes(bytes, OpenOptions::default()).unwrap()
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn small_blocks() -> BuildOptions {
    // Small blocks so that the dataset spans many blocks and the index is exercised.
    BuildOptions { block_size: 2048, zstd_dict: false, keyframe_interval: 2, ..Default::default() }
}

#[test]
fn roundtrip_all_versions() {
    let data = dataset();
    let r = build(data.clone(), small_blocks());
    let mut by_obj: BTreeMap<(ObjType, i64), Vec<&Element>> = BTreeMap::new();
    for e in &data {
        by_obj.entry((e.obj_type(), e.id())).or_default().push(e);
    }
    for ((t, id), vs) in by_obj {
        let got = r.get(t, id).unwrap();
        assert_eq!(got.len(), vs.len(), "{t:?} {id}");
        for (g, e) in got.iter().zip(&vs) {
            let m = e.meta();
            assert_eq!((g.meta.version, g.meta.timestamp, g.meta.changeset, g.meta.uid, g.meta.visible),
                (m.version, m.timestamp, m.changeset, m.uid, m.visible), "{t:?} {id}");
            match e {
                Element::Node(n) => {
                    assert_eq!(g.tags, n.tags);
                    if n.meta.visible {
                        assert_eq!(g.geometry, Geometry::Point(n.lon, n.lat));
                    }
                }
                Element::Way(w) => {
                    assert_eq!(g.tags, w.tags);
                    assert_eq!(g.nodes, w.refs);
                }
                Element::Relation(rr) => {
                    assert_eq!(g.tags, rr.tags);
                    assert_eq!(g.members, rr.members);
                }
            }
        }
    }
    assert_eq!(r.header.n_versions[0] as usize, data.iter().filter(|e| e.obj_type() == ObjType::Node).count());
    r.verify(true).unwrap();
}

#[test]
fn private_nodes_are_not_duplicated() {
    let r = build(dataset(), small_blocks());
    // Node 1000 is private (only in way 100): it has no node segment of its own.
    let blocks = r.lookup(ObjType::Node, 1000).unwrap();
    assert!(!blocks.is_empty());
    for b in blocks {
        assert_eq!(r.block_entry(b).unwrap().obj_type, ObjType::Way);
    }
    // Node 1 is shared (tagged, in many ways): stored in a node block.
    let blocks = r.lookup(ObjType::Node, 1).unwrap();
    assert!(blocks.iter().all(|&b| r.block_entry(b).unwrap().obj_type == ObjType::Node));
}

#[test]
fn vandalism_results_are_correct() {
    let r = build(dataset(), small_blocks());
    let near_zero = BBox::from_deg(-0.01, -0.01, 0.01, 0.01);
    let town = BBox::from_deg(8.79, 53.06, 8.83, 53.10);
    let pts = |fs: &[knut::Feature], t: ObjType, id: i64| fs.iter().any(|f| f.obj_type == t && f.id == id);

    // shared node 5000
    let at_outlier = r.at(ObjType::Node, 5000, T0 + 500 * DAY + 10).unwrap().unwrap();
    assert_eq!(at_outlier.geometry, Geometry::Point(0, 0));
    let s = r.snapshot(&near_zero, T0 + 500 * DAY + 10, &Filter::default()).unwrap();
    assert!(pts(&s, ObjType::Node, 5000));
    let s = r.snapshot(&near_zero, T0 + 502 * DAY, &Filter::default()).unwrap();
    assert!(!pts(&s, ObjType::Node, 5000));
    let s = r.snapshot(&town, T0 + 502 * DAY, &Filter::default()).unwrap();
    assert!(pts(&s, ObjType::Node, 5000));
    let s = r.snapshot(&town, T0 + 500 * DAY + 10, &Filter::default()).unwrap();
    assert!(!pts(&s, ObjType::Node, 5000));

    // private node 6001 of way 6000
    let g = r.way_geometry_at(6000, T0 + 600 * DAY + 5).unwrap().unwrap();
    assert_eq!(g[0], (6001, Some((0, 0))));
    let g = r.way_geometry_at(6000, T0 + 602 * DAY).unwrap().unwrap();
    assert_eq!(g[0], (6001, Some(bremen(0.002, 0.003))));
    let s = r.snapshot(&near_zero, T0 + 600 * DAY + 5, &Filter::default().types(&[ObjType::Way])).unwrap();
    assert!(pts(&s, ObjType::Way, 6000), "way must be found at the outlier location");
    let s = r.snapshot(&near_zero, T0 + 602 * DAY, &Filter::default().types(&[ObjType::Way])).unwrap();
    assert!(!pts(&s, ObjType::Way, 6000));
    let hist = r.get(ObjType::Node, 6001).unwrap();
    assert_eq!(hist.iter().map(|f| f.meta.version).collect::<Vec<_>>(), vec![1, 2, 3]);

    // way 7000 across the globe
    let g = r.way_geometry_at(7000, T0 + 701 * DAY).unwrap().unwrap();
    assert_eq!(g.len(), 4);
    let pacific = BBox::from_deg(-121.0, -41.0, -119.0, -39.0);
    let s = r.snapshot(&pacific, T0 + 701 * DAY, &Filter::default().types(&[ObjType::Way])).unwrap();
    assert!(pts(&s, ObjType::Way, 7000));
    let s = r.snapshot(&pacific, T0 + 703 * DAY, &Filter::default().types(&[ObjType::Way])).unwrap();
    assert!(!pts(&s, ObjType::Way, 7000));
    let s = r.snapshot(&town, T0 + 703 * DAY, &Filter::default().types(&[ObjType::Way])).unwrap();
    assert!(pts(&s, ObjType::Way, 7000));

    // changes in the outlier window near 0,0
    let c = r.changes(&near_zero, T0 + 499 * DAY, T0 + 601 * DAY, &Filter::default()).unwrap();
    assert!(pts(&c, ObjType::Node, 5000) && pts(&c, ObjType::Node, 6001));
}

#[test]
fn vandalism_does_not_inflate_block_boxes() {
    let r = build(dataset(), small_blocks());
    // Every non-large block must stay compact: either in the town or near an outlier.
    let one_deg = 10_000_000i64;
    for no in 0..r.n_blocks() as u32 {
        let e = r.block_entry(no).unwrap();
        if e.section >= 3 {
            continue;
        }
        let (_, dirs) = r.block_dirs(no).unwrap();
        let mut bb = BBox::EMPTY;
        for d in &dirs {
            bb.union(&d.bbox);
        }
        assert!(bb.width() < one_deg && bb.height() < one_deg, "block {no} ({:?}) is inflated: {bb:?}", e.obj_type);
    }
    // The globe-spanning way version lives in a large-object segment.
    let th = r.tree_header(TREE_LARGE);
    assert!(th.n_nodes > 0);
    // Way 7000 and nodes 5000 are split into several segments.
    for (t, id, min) in [(ObjType::Way, 7000, 3), (ObjType::Node, 5000, 3), (ObjType::Way, 6000, 3)] {
        let mut n = 0;
        for b in r.lookup(t, id).unwrap() {
            n += r.block(b).unwrap().segments_of(id).len();
        }
        assert!(n >= min, "{t:?} {id}: {n} segments");
    }
}

#[test]
fn deletion_and_recreation() {
    let r = build(dataset(), small_blocks());
    let a = r.at(ObjType::Node, 8000, T0 + 70 * DAY).unwrap().unwrap();
    assert!(!a.meta.visible);
    let b = r.at(ObjType::Node, 8000, T0 + 95 * DAY).unwrap().unwrap();
    assert_eq!(b.meta.version, 5);
    let town = BBox::from_deg(8.79, 53.06, 8.83, 53.10);
    let f = Filter::default().tag("amenity", Some("atm"));
    assert!(r.snapshot(&town, T0 + 70 * DAY, &f).unwrap().is_empty());
    assert_eq!(r.snapshot(&town, T0 + 95 * DAY, &f).unwrap().len(), 1);
    assert_eq!(r.snapshot(&town, T0 + 55 * DAY, &f).unwrap().len(), 1);
    assert!(r.at(ObjType::Node, 8000, T0).unwrap().is_none());
}

#[test]
fn snapshot_filters_and_geometry() {
    let r = build(dataset(), small_blocks());
    let town = BBox::from_deg(8.79, 53.06, 8.83, 53.10);
    let t = T0 + 450 * DAY;
    let benches = r.snapshot(&town, t, &Filter::default().tag("amenity", Some("bench"))).unwrap();
    assert_eq!(benches.len(), 400);
    let backrest = r.snapshot(&town, t, &Filter::default().tag("backrest", None)).unwrap();
    assert_eq!(backrest.len(), (1..=400).filter(|i| i % 3 == 0).count());
    let streets = r.snapshot(&town, t, &Filter::default().tag("highway", Some("residential"))).unwrap();
    assert_eq!(streets.len(), 30);
    for s in &streets {
        assert_eq!(s.tag("name"), Some("Straße"));
        let Geometry::Line(pts) = &s.geometry else { panic!() };
        assert_eq!(pts.len(), 6);
        assert!(pts.iter().all(|p| p.is_some()), "all node positions resolved");
    }
    // Before version 2 the streets had 7 nodes incl. the shared junction node 1.
    let early = r.snapshot(&town, T0 + 2 * DAY, &Filter::default().tag("highway", None)).unwrap();
    assert!(early.iter().all(|s| matches!(&s.geometry, Geometry::Line(p) if p.len() == 7 && p[6].is_some())));
    // Private nodes are included without tag filter.
    let nodes = r.snapshot(&town, t, &Filter::default().types(&[ObjType::Node])).unwrap();
    assert!(nodes.iter().any(|f| f.id == 1000));
    let no_priv = Filter { include_private_nodes: false, ..Filter::default().types(&[ObjType::Node]) };
    assert!(!r.snapshot(&town, t, &no_priv).unwrap().iter().any(|f| f.id == 1000));
    // uid filter
    let by_uid = r.snapshot(&town, t, &Filter { uid: Some(44), ..Filter::default() }).unwrap();
    assert!(!by_uid.is_empty() && by_uid.iter().all(|f| f.meta.uid == 44));
    // relations: child relation box derived from parent, found in town
    let rels = r.snapshot(&town, t, &Filter::default().types(&[ObjType::Relation])).unwrap();
    assert_eq!(rels.iter().map(|f| f.id).collect::<Vec<_>>(), vec![9000, 9001]);
    // antimeridian-crossing query box (minx > maxx) is split
    let wrap = BBox::from_deg(170.0, -90.0, 9.0, 90.0);
    assert!(r.snapshot(&wrap, t, &Filter::default().tag("amenity", Some("bench"))).unwrap().len() == 400);
}

#[test]
fn way_geometry_changes_lists_all_states() {
    let r = build(dataset(), small_blocks());
    let ch = r.way_geometry_changes(6000).unwrap();
    // created, node to 0,0, node back
    assert_eq!(ch.len(), 3);
    assert_eq!(ch[1].1[0].1, Some((0, 0)));
}

#[test]
fn options_variants_roundtrip() {
    let data = dataset();
    for opt in [
        BuildOptions { zstd_dict: true, block_size: 1024, ..Default::default() },
        BuildOptions { pseudonymize_uids: Some("secret".into()), strip_users: true, ..small_blocks() },
    ] {
        let r = build(data.clone(), opt.clone());
        r.verify(true).unwrap();
        let g = r.get(ObjType::Node, 1000).unwrap();
        assert_eq!(g.len(), 1);
        if opt.pseudonymize_uids.is_some() {
            assert!(r.users().unwrap().is_empty());
            assert!(r.get(ObjType::Way, 100).unwrap().iter().all(|f| f.meta.uid != 43 && f.meta.uid != 44));
        } else {
            assert!(!r.users().unwrap().is_empty());
        }
    }
}

#[test]
fn corrupted_files_never_panic() {
    let dir = std::env::temp_dir().join(format!("knut-fuzz-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.knutdb");
    knut_builder::build(&VecSource::new(dataset()), &path, &small_blocks(), &|_| {}).unwrap();
    let orig = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let town = BBox::from_deg(8.79, 53.06, 8.83, 53.10);
    for i in 0..3000 {
        let mut b = orig.clone();
        let flips = 1 + (rnd() % 8) as usize;
        for _ in 0..flips {
            let pos = (rnd() as usize) % b.len();
            b[pos] ^= (rnd() as u8) | 1;
        }
        if i % 7 == 0 {
            b.truncate((rnd() as usize) % b.len());
        }
        if let Ok(dump) = std::env::var("KNUT_FUZZ_DUMP") {
            if dump.parse::<usize>() == Ok(i) {
                std::fs::write("/tmp/knut-fuzz.knutdb", &b).unwrap();
                return;
            }
        }
        let res = std::panic::catch_unwind(move || {
            // Checksums off: exercise the decoders on corrupt data.
            let Ok(r) = Reader::from_bytes(b, OpenOptions { verify_checksums: false, cache_blocks: 16 }) else {
                return;
            };
            let _ = r.verify(true);
            let _ = r.get(ObjType::Way, 100);
            let _ = r.get(ObjType::Node, 1000);
            let _ = r.get(ObjType::Relation, 9000);
            let _ = r.snapshot(&town, T0 + 450 * DAY, &Filter::default());
            let _ = r.changes(&BBox::WORLD, T0, T0 + 1000 * DAY, &Filter::default());
            let _ = r.way_geometry_changes(7000);
        });
        assert!(res.is_ok(), "panic on corrupted input (iteration {i})");
    }
}

#[test]
fn attribute_only_queries_match_full_queries() {
    let r = build(dataset(), small_blocks());
    // separate reader so that attribute-only queries never see fully cached blocks
    let r2 = build(dataset(), small_blocks());
    let key = |fs: Vec<knut::Feature>| -> Vec<(ObjType, i64, u32)> {
        let mut v: Vec<_> = fs.into_iter().map(|f| (f.obj_type, f.id, f.meta.version)).collect();
        v.sort();
        v
    };
    let filters = [
        Filter { include_private_nodes: false, ..Filter::default() },
        Filter::default().tag("amenity", None),
        Filter::default().tag("highway", Some("path")),
        Filter { include_private_nodes: false, ..Filter::default().types(&[ObjType::Node]) },
    ];
    for f in &filters {
        let nogeo = Filter { geometry: false, ..f.clone() };
        for day in [0, 15, 55, 70, 95, 300, 450, 500, 600, 601, 701, 703, 900] {
            let t = T0 + day * DAY + 10;
            let a = key(r.snapshot(&BBox::WORLD, t, f).unwrap());
            let b = r2.snapshot(&BBox::WORLD, t, &nogeo).unwrap();
            assert!(b.iter().all(|x| x.geometry == Geometry::None));
            assert_eq!(a, key(b), "snapshot day {day} filter {f:?}");
        }
        let a = key(r.changes(&BBox::WORLD, T0, T0 + 1000 * DAY, f).unwrap());
        let b = key(r2.changes(&BBox::WORLD, T0, T0 + 1000 * DAY, &nogeo).unwrap());
        assert_eq!(a, b, "changes filter {f:?}");
    }
}

#[test]
fn spill_mode_output_is_byte_identical() {
    let dir = std::env::temp_dir().join(format!("knut-spill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut data = dataset();
    // more data so that tiny budgets really spill (pairs, rows, id index)
    let mut nid = 100_000;
    for w in 0..400 {
        let mut refs = Vec::new();
        for k in 0..20 {
            data.push(node(nid, 1, T0 + w * 60, Some(bremen(w as f64 * 0.0003, k as f64 * 0.0002)), &[]));
            refs.push(nid);
            nid += 1;
        }
        data.push(way(50_000 + w, 1, T0 + w * 60, &refs, &[("building", "yes")]));
    }
    let out_mem = dir.join("mem.knutdb");
    let out_spill = dir.join("spill.knutdb");
    let base = BuildOptions { block_size: 4096, created: Some(0), ..Default::default() };
    knut_builder::build(&VecSource::new(data.clone()), &out_mem, &base, &|_| {}).unwrap();
    let tiny = BuildOptions { mem_budget: 0, tmp_dir: Some(dir.clone()), ..base };
    let st = knut_builder::build(&VecSource::new(data), &out_spill, &tiny, &|_| {}).unwrap();
    assert!(st.spilled_bytes > 0, "test must exercise spilling");
    let a = std::fs::read(&out_mem).unwrap();
    let b = std::fs::read(&out_spill).unwrap();
    assert!(a == b, "spill output differs from in-memory output");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Adding and removing an element, compacting: the data stays readable and identical.
#[test]
fn container_changes_keep_the_data() {
    use knut::container;
    use knut_format::container::{ElementKind, Temporal};
    let dir = std::env::temp_dir().join(format!("knut-test-{}-container", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.knutdb");
    knut_builder::build(&VecSource::new(dataset()), &path, &small_blocks(), &|_| {}).unwrap();
    let all = |p: &std::path::Path| {
        let r = Reader::open(p).unwrap();
        r.verify(true).unwrap();
        let mut v: Vec<(ObjType, i64, u32)> = r
            .snapshot(&BBox::WORLD, T0 + 400 * DAY, &Filter::default())
            .unwrap()
            .iter()
            .map(|f| (f.obj_type, f.id, f.meta.version))
            .collect();
        v.sort();
        v
    };
    let before = all(&path);
    let h = container::header(&path).unwrap();
    assert!(h.directory.elements.iter().all(|e| e.info().base));
    assert_eq!(container::freeable(&path, &h).unwrap(), 0);
    let n_base = h.directory.elements.len();

    let sec = |b: &[u8]| container::NewSection { kind: 1000, flags: 0, bytes: b.to_vec() };
    let id = container::add_element(&path, ElementKind::TagStatistics, Temporal::Latest, vec![], vec![sec(&[7; 100])]).unwrap();
    let id2 = container::add_element(&path, ElementKind::PlaceIndex, Temporal::Monthly, vec![("a".into(), "b".into())], vec![sec(&[9; 50])]).unwrap();
    assert_eq!((id, id2), (n_base as u32, n_base as u32 + 1));
    let h = container::header(&path).unwrap();
    assert_eq!(h.generation, 3);
    assert_eq!(container::element_bytes(&path, h.directory.get(id2).unwrap()).unwrap(), vec![(1000, vec![9; 50])]);
    assert_eq!(all(&path), before);

    // the database itself cannot be removed
    assert!(container::remove_element(&path, 0).is_err());
    container::remove_element(&path, id).unwrap();
    let h = container::header(&path).unwrap();
    assert!(h.directory.get(id).is_none());
    assert!(container::freeable(&path, &h).unwrap() >= 100);

    let freed = container::compact(&path, &mut |_, _| {}).unwrap();
    assert!(freed >= 100);
    let h = container::header(&path).unwrap();
    assert_eq!(container::freeable(&path, &h).unwrap(), 0);
    assert_eq!(h.directory.elements.len(), n_base + 1);
    assert_eq!(container::element_bytes(&path, h.directory.get(id2).unwrap()).unwrap(), vec![(1000, vec![9; 50])]);
    assert_eq!(all(&path), before);
    // ids are never reused
    let id3 = container::add_element(&path, ElementKind::TagStatistics, Temporal::Latest, vec![], vec![]).unwrap();
    assert_eq!(id3, id2 + 1);
    std::fs::remove_dir_all(&dir).ok();
}
