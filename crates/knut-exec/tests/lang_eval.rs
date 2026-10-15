//! Evaluation: selections and time, fields, quantifiers, sequences, arithmetic and units,
//! conditions, grouping and aggregation, users, changesets and keys. Expected values follow
//! from the town data described in `common/mod.rs`.

mod common;
use common::*;
use serde_json::json;

fn ids(j: &J) -> Vec<i64> {
    j.as_array().unwrap_or_else(|| panic!("not a list: {j}")).iter().map(|e| e["id"].as_i64().unwrap()).collect()
}

// ----- selections and time ----------------------------------------------------------------

#[test]
fn state_at_a_moment() {
    let v = values(
        "count of nodes n with n:amenity=bench\n\
         count of nodes n with n:amenity=bench on 2015-01-05\n\
         count of nodes n with n:amenity=bench on 5 Jan 2015\n\
         count of nodes n with n:amenity=bench on 2015-02-01\n\
         count of nodes n with n:amenity=bench on Jan 2015\n\
         count of nodes n with n:amenity=bench on 2014",
    );
    // latest: benches 1, 2, 4–10 (3 and 50 are deleted)
    assert_eq!(num(&v[0]), 9.0);
    // 1–10 and 50
    assert_eq!(num(&v[1]), 11.0);
    assert_eq!(num(&v[2]), 11.0);
    // 50 is deleted after day 20 (in another segment than its first version)
    assert_eq!(num(&v[3]), 10.0);
    // a span stands for its start: at 2015-01-01 00:00 only bench 50 exists
    assert_eq!(num(&v[4]), 1.0);
    assert_eq!(num(&v[5]), 0.0);
}

#[test]
fn events_in_a_range() {
    let v = values(
        "count of nodes s with s:shop from 2015-01 to 2015-01\n\
         count of nodes s with s:shop from 2015-02 to 2015-02\n\
         count of nodes n with n:amenity=bench from 2015 to 2015\n\
         count of nodes n with n.deleted from 2015 to 2015",
    );
    assert_eq!(num(&v[0]), 20.0);
    // shop 600 v2 on 10 Feb
    assert_eq!(num(&v[1]), 1.0);
    // benches 1–10 v1, bench 4 v2, bench 50 v1 and v2 (deletions have no tags)
    assert_eq!(num(&v[2]), 13.0);
    // bench 3 v2 and bench 50 v3
    assert_eq!(num(&v[3]), 2.0);
}

#[test]
fn state_series() {
    let v = one("count of nodes n with n:amenity=bench on each month from 2015-01 to 2015-05");
    let r = rows(&v, "month", "count");
    let counts: Vec<f64> = r.iter().map(|x| num(&x.1)).collect();
    assert_eq!(counts, vec![1.0, 10.0, 10.0, 10.0, 9.0]);
    assert_eq!(r[0].0, json!("2015-01"));
}

#[test]
fn events_per_time_unit() {
    let v = one("count of nodes s with s:shop per day from 2015-01-01 to 2015-01-03");
    let r = rows(&v, "day", "count");
    assert_eq!(r.len(), 3, "{v}");
    assert!(r.iter().all(|x| num(&x.1) == 1.0), "{v}");
    let v = one("count of nodes s with s:shop per month from 2015-01 to 2015-02");
    let r = rows(&v, "month", "count");
    assert_eq!(r, vec![(json!("2015-01"), json!(20)), (json!("2015-02"), json!(1))]);
}

#[test]
fn kinds_and_whole_sets() {
    let v = values(
        "ignore_warnings = true\n\
         count of areas a with a:amenity=library\n\
         count of areas a with a:leisure=park\n\
         count of areas a with a:building\n\
         count of relations r with r:boundary\n\
         count of ways w with w:highway\n\
         count of * x with x:amenity=library\n\
         (pick * x with x:amenity=library).type\n\
         (pick * x with x:amenity=library).osmType\n\
         (pick * x with x:leisure=park).osmType",
    );
    assert_eq!(num(&v[0]), 1.0);
    assert_eq!(num(&v[1]), 1.0);
    assert_eq!(num(&v[2]), 5.0);
    assert_eq!(num(&v[3]), 3.0);
    assert_eq!(num(&v[4]), 3.0);
    // `*` lists an object once, areas as areas
    assert_eq!(num(&v[5]), 1.0);
    assert_eq!(v[6], json!("area"));
    assert_eq!(v[7], json!("way"));
    assert_eq!(v[8], json!("relation"));
}

#[test]
fn pick_forms() {
    let v = values(
        "(pick node n with n:amenity=bench).id\n\
         pick 3 nodes n with n:amenity=bench\n\
         pick nodes n with n:amenity=bench\n\
         pick 3 nodes n randomly using seed 7 with n:amenity=bench\n\
         pick 3 nodes n randomly using seed 7 with n:amenity=bench\n\
         pick 2 nodes n with n:amenity=bench ordered by -n.id\n\
         pick node n with n:amenity=nothing",
    );
    assert_eq!(num(&v[0]), 1.0);
    assert_eq!(ids(&v[1]), vec![1, 2, 4]);
    assert_eq!(ids(&v[2]), vec![1]);
    assert_eq!(ids(&v[3]).len(), 3);
    assert_eq!(v[3], v[4], "random picks are deterministic");
    assert_eq!(ids(&v[5]), vec![10, 9]);
    assert_eq!(v[6], J::Null);
}

#[test]
fn pick_exactly_too_few() {
    let r = fails("pick exactly 20 nodes n with n:amenity=bench", "E404");
    assert_eq!(r.code, 2);
}

#[test]
fn quantifiers() {
    let v = values(
        "benches = nodes b with b:amenity=bench\n\
         (the n in benches with n.id == 4).version\n\
         some n in benches with n.id == 4\n\
         some n in benches with n.id == 3\n\
         count of all n in benches with n.id < 3\n\
         (the n in benches with n.id == 99) ?? 'fallback'",
    );
    assert_eq!(num(&v[0]), 2.0);
    assert_eq!(v[1], json!(true));
    assert_eq!(v[2], json!(false));
    assert_eq!(num(&v[3]), 2.0);
    assert_eq!(v[4], json!("fallback"));
    fails("benches = nodes b with b:amenity=bench\nthe n in benches with n.id == 99", "E401");
    let r = fails("benches = nodes b with b:amenity=bench\nthe n in benches with n.id > 1", "E402");
    assert!(r.err.contains("found 8"), "{}", r.err);
    fails("the area n in (* m with m:amenity=bench) with n.id == 1", "E403");
}

// ----- fields -----------------------------------------------------------------------------

#[test]
fn element_fields() {
    let v = values(
        "v = pick node n with n:tourism=viewpoint\n\
         v.id\n\
         v.version\n\
         v.previous.version\n\
         v.previous:name\n\
         v.next\n\
         v.unchangedBefore\n\
         v.previous.unchangedAfter\n\
         v.keys\n\
         v.time\n\
         v:name.length\n\
         count of v.versions\n\
         v.type\n\
         v.deleted\n\
         v.next:name\n\
         v:name == 'Aussicht'",
    );
    assert_eq!(num(&v[0]), 61.0);
    assert_eq!(num(&v[1]), 3.0);
    assert_eq!(num(&v[2]), 2.0);
    assert_eq!(v[3], json!("View"));
    assert_eq!(v[4], J::Null);
    assert_eq!(num(&v[5]), 10.0);
    assert_eq!(unit(&v[5]), "days");
    assert_eq!(num(&v[6]), 10.0);
    assert_eq!(v[7], json!(["name", "tourism"]));
    assert_eq!(v[8], json!("2015-01-13T00:00:00Z"));
    assert_eq!(num(&v[9]), 8.0);
    assert_eq!(num(&v[10]), 3.0);
    assert_eq!(v[11], json!("node"));
    assert_eq!(v[12], json!(false));
    // access on none is none
    assert_eq!(v[13], J::Null);
    assert_eq!(v[14], json!(true));
}

#[test]
fn geometry_fields() {
    let v = values(
        "r = pick way w with w:highway=residential\n\
         r.length\n\
         count of r.nodes\n\
         count of (pick relation x with x:leisure=park).members\n\
         (pick node n with n:amenity=bench).lon\n\
         (pick node n with n:amenity=bench).lat",
    );
    // 0.01° of longitude at 53.07° N on a sphere of radius 6 371 008.8 m
    let len = 6_371_008.8 * 53.07f64.to_radians().cos() * 0.01f64.to_radians();
    approx(&v[0], len, 0.002);
    assert_eq!(unit(&v[0]), "m");
    assert_eq!(num(&v[1]), 11.0);
    assert_eq!(num(&v[2]), 2.0);
    approx(&v[3], 8.801, 1e-9);
    approx(&v[4], 53.07, 1e-9);
}

#[test]
fn own_fields_and_creator() {
    let v = values(
        "define node N: twice = N.version * 2\n\
         define node N: more = N.twice + 1\n\
         v = pick node n with n:tourism=viewpoint\n\
         v.twice\n\
         v.more\n\
         v.creator.id",
    );
    assert_eq!(num(&v[0]), 6.0);
    assert_eq!(num(&v[1]), 7.0);
    assert_eq!(num(&v[2]), 7.0);
}

// ----- sequences --------------------------------------------------------------------------

#[test]
fn sequences() {
    let v = values(
        "ignore_warnings = true\n\
         war = way A -> way B -> way C closer than 30 days with A.user == C.user and B.user != A.user and A:maxspeed == C:maxspeed and A:maxspeed != B:maxspeed\n\
         count of war\n\
         war.map(x => x_B:maxspeed)\n\
         count of way A -> way B\n\
         count of way A -> way B closer than 5 days\n\
         count of way A -> way B closer than or equal 5 days\n\
         pairs = way A -> way B\n\
         count of pairs_A\n\
         count of node A -> node B with A:name != B:name",
    );
    assert_eq!(num(&v[0]), 1.0);
    assert_eq!(v[1], json!(["30"]));
    // way 400: v1 → v2 (4 days), v2 → v3 (5 days)
    assert_eq!(num(&v[2]), 2.0);
    assert_eq!(num(&v[3]), 1.0);
    assert_eq!(num(&v[4]), 2.0);
    // projection: distinct A elements (v1, v2)
    assert_eq!(num(&v[5]), 2.0);
    // viewpoint v2 → v3 (v1 has no name)
    assert_eq!(num(&v[6]), 1.0);
}

// ----- arithmetic, units, none ------------------------------------------------------------

#[test]
fn arithmetic_and_units() {
    let v = values(
        "300 m + 1 km\n\
         1 km + 300 m\n\
         2 * 3 km\n\
         1 km > 900 m\n\
         10 / 4\n\
         none + 1\n\
         5 ?? 3\n\
         none ?? 3\n\
         3 days + 1 weeks\n\
         30 days before 2015-03-01\n\
         2 ha > 15000 m2\n\
         50 km/h < 40 mph\n\
         -3 + 1",
    );
    assert_eq!((num(&v[0]), unit(&v[0])), (1300.0, "m".into()));
    assert_eq!((num(&v[1]), unit(&v[1])), (1.3, "km".into()));
    assert_eq!((num(&v[2]), unit(&v[2])), (6.0, "km".into()));
    assert_eq!(v[3], json!(true));
    assert_eq!(num(&v[4]), 2.5);
    assert_eq!(v[5], J::Null);
    assert_eq!(num(&v[6]), 5.0);
    assert_eq!(num(&v[7]), 3.0);
    assert_eq!((num(&v[8]), unit(&v[8])), (10.0, "days".into()));
    assert_eq!(v[9], json!("2015-01-30"));
    assert_eq!(v[10], json!(true));
    // 40 mph ≈ 64.4 km/h
    assert_eq!(v[11], json!(true));
    assert_eq!(num(&v[12]), -2.0);
}

#[test]
fn tag_values_as_numbers() {
    let v = values(
        "ignore_warnings = true\n\
         count of ways b with b:building and (b:height ?? 0 m) > 20 m\n\
         (ways b with b:building).map(b => b:height m ?? b:building:levels * 3 m)\n\
         count of nodes s with s:amenity=shelter and (s:capacity ?? 0) > 10\n\
         count of ways w with w:highway and (w:maxspeed ?? 0 km/h) > 40 km/h\n\
         count of ways w with w:highway and w:maxspeed > 40 km/h",
    );
    // default unit of height is m: 25 and 0.03 km
    assert_eq!(num(&v[0]), 2.0);
    let hs: Vec<Option<f64>> = v[1].as_array().unwrap().iter().map(|x| if x.is_null() { None } else { Some(num(x)) }).collect();
    assert_eq!(hs, vec![Some(25.0), Some(12.0), Some(0.03), None, Some(12.0)]);
    // "about 200" falls back to 0
    assert_eq!(num(&v[2]), 1.0);
    // way 400 has maxspeed 50 (default unit km/h)
    assert_eq!(num(&v[3]), 1.0);
    assert_eq!(num(&v[4]), 1.0);
}

#[test]
fn values_that_are_not_numbers_count_as_missing() {
    // shelter 701 has capacity="about 200": the comparison is false, without a message
    let r = run_text("count of nodes s with s:amenity=shelter and s:capacity > 10");
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("\n1\n"), "{}", r.out);
    assert!(r.err.trim().is_empty(), "{}", r.err);
    // in arithmetic such a value gives none
    assert_eq!(one("(the s in (nodes x with x:amenity=shelter) with s.id == 701):capacity * 2"), J::Null);
    // capacity has no default unit: comparing it with a length is still an error
    let r = fails("count of nodes s with s:amenity=shelter and s:capacity > 10 m", "E304");
    assert_eq!(r.code, 2);
}

#[test]
fn conditions() {
    let v = values(
        "ignore_warnings = true\n\
         count of nodes s with s:shop in [bakery, clothes]\n\
         count of nodes s with s:shop not in [bakery, clothes]\n\
         count of nodes s with s.id between 600 and 609\n\
         count of nodes s with s:shop and not s:shop=bakery\n\
         count of nodes s with s:shop=bakery or s:shop=clothes\n\
         count of nodes s with s:shop and s:name\n\
         count of nodes s with s:shop and s:name == none\n\
         count of ways w with w:maxspeed != 30\n\
         count of nodes s with s:shop and s:['shop'] == 'bakery'",
    );
    assert_eq!(num(&v[0]), 16.0);
    assert_eq!(num(&v[1]), 4.0);
    assert_eq!(num(&v[2]), 10.0);
    assert_eq!(num(&v[3]), 10.0);
    assert_eq!(num(&v[4]), 16.0);
    assert_eq!(num(&v[5]), 1.0);
    assert_eq!(num(&v[6]), 19.0);
    // only ways with maxspeed take part: way 400 (50)
    assert_eq!(num(&v[7]), 1.0);
    assert_eq!(num(&v[8]), 10.0);
}

// ----- grouping and aggregation -----------------------------------------------------------

#[test]
fn grouping() {
    let v = values(
        "shops = nodes s with s:shop\n\
         count of shops per s:shop\n\
         count of shops per s:shop having count > 5\n\
         groups g in (count of shops per s:shop) with g.count > 5\n\
         share of s in shops per s:shop\n\
         share of s in shops with s:shop=bakery\n\
         count of shops per chunk of 8 ordered by s.id\n\
         count of nodes s with s:shop per s:shop per year from 2015 to 2015",
    );
    let c = rows(&v[0], "s:shop", "count");
    assert_eq!(c, vec![(json!("bakery"), json!(10)), (json!("clothes"), json!(6)), (json!("supermarket"), json!(4))]);
    assert_eq!(rows(&v[1], "s:shop", "count").len(), 2);
    assert_eq!(v[2].as_array().unwrap().len(), 2, "{}", v[2]);
    let sh: Vec<f64> = rows(&v[3], "s:shop", "share").iter().map(|x| num(&x.1)).collect();
    assert_eq!(sh, vec![0.5, 0.3, 0.2]);
    assert_eq!(num(&v[4]), 0.5);
    let ch: Vec<f64> = v[5].as_array().unwrap().iter().map(|r| num(&r["count"])).collect();
    assert_eq!(ch, vec![8.0, 8.0, 4.0]);
    // events in 2015: bakery 10 + shop 600 v2
    let n: Vec<(J, f64)> = v[6].as_array().unwrap().iter().map(|r| (r["s:shop"].clone(), num(&r["count"]))).collect();
    assert_eq!(n, vec![(json!("bakery"), 11.0), (json!("clothes"), 6.0), (json!("supermarket"), 4.0)]);
}

#[test]
fn simple_aggregates() {
    let v = values(
        "sum of [1, 2, 3]\n\
         average of [1, 2, 3]\n\
         sum of [1 km, 500 m]\n\
         count of distinct of [1, 1, 2]\n\
         earliest of (nodes s with s:shop)\n\
         latest of (nodes s with s:shop)\n\
         count of [1, 1, 1]\n\
         variance of [2, 4, 4, 4, 5, 5, 7, 9]",
    );
    assert_eq!(num(&v[0]), 6.0);
    assert_eq!(num(&v[1]), 2.0);
    assert_eq!((num(&v[2]), unit(&v[2])), (1.5, "km".into()));
    assert_eq!(num(&v[3]), 2.0);
    // latest versions: shop 601 (2 Jan) is the earliest, shop 600 v2 (10 Feb) the latest
    assert_eq!(v[4], json!("2015-01-02T00:00:00Z"));
    assert_eq!(v[5], json!("2015-02-10T00:00:00Z"));
    assert_eq!(num(&v[6]), 3.0);
    approx(&v[7], 32.0 / 7.0, 1e-12);
}

// ----- users, changesets, keys ------------------------------------------------------------

#[test]
fn users_and_changesets() {
    let v = values(
        "count of users\n\
         count of users u with count of u.changesets > 1\n\
         count of changesets from 2015-02 to 2015-02\n\
         (pick user u with u.id == 1).firstEdit\n\
         (pick way w with w:highway=primary).user.id",
    );
    assert_eq!(num(&v[0]), 4.0);
    // user 1 (two war edits) and user 7
    assert_eq!(num(&v[1]), 2.0);
    // shop 600 v2 and bench 4 v2
    assert_eq!(num(&v[2]), 2.0);
    assert_eq!(v[3], json!("2015-01-02T00:00:00Z"));
    assert_eq!(num(&v[4]), 1.0);
}

#[test]
fn keys() {
    let v = values(
        "early = keys on 2015-01-02\n\
         count of early\n\
         (keys k with k not in early).map(k => k.name)\n\
         (pick key k with k.name == 'shop').count\n\
         (pick key k with k.name == 'shop').values",
    );
    // amenity, tourism, name, leisure, type, boundary, admin_level, highway, maxspeed, shop,
    // capacity, building, height, building:levels, place
    assert_eq!(num(&v[0]), 15.0);
    assert_eq!(v[1], json!(["backrest"]));
    assert_eq!(num(&v[2]), 20.0);
    assert_eq!(num(&v[3]), 3.0);
}

#[test]
fn lists_keep_duplicates_and_maps() {
    let v = values(
        "m = (nodes n with n:amenity=bench).map(b => b:amenity)\n\
         count of m\n\
         (nodes s with s:shop).filter(s => s:shop == 'supermarket').count\n\
         [1, 2, 3].map(x => x * 2)",
    );
    assert_eq!(num(&v[0]), 9.0);
    assert_eq!(num(&v[1]), 4.0);
    assert_eq!(v[2], json!([2, 4, 6]));
}

/// An object split over blocks: its newer version lies in a block the tag filters skip, and
/// still decides that the old, matching version is no longer current.
#[test]
fn objects_over_several_blocks_count_with_their_newest_version() {
    let db = db_moved_road();
    let r = knut::Reader::open(&db).unwrap();
    assert!(r.lookup(knut_format::ObjType::Way, 1).unwrap().len() >= 2, "way 1 should lie in two blocks");
    let count = |s: &str| num(&values_on(&db, s)[0]);
    assert_eq!(count("count of ways w with w:highway=primary"), 1.0);
    assert_eq!(count("count of ways w with w:highway=primary and w:name='Old Road'"), 1.0);
    assert_eq!(count("count of ways w with w:highway=secondary"), 2.0);
    assert_eq!(count("count of ways w with w:highway=primary on 2015-02-01"), 2.0);
    // the same through the reader's API, with and without geometry
    for geometry in [false, true] {
        let f = knut::Filter { geometry, ..knut::Filter::default().types(&[knut_format::ObjType::Way]).tag("highway", Some("primary")) };
        let ids: Vec<i64> = r.snapshot(&knut_format::BBox::WORLD, r.header.tmax, &f).unwrap().iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![2], "geometry: {geometry}");
    }
}
