//! Spatial relations, areas, places, grids and networks on the town data.

mod common;
use common::*;
use serde_json::json;

/// Metres per degree on the sphere used for distances.
fn m_lon(deg: f64) -> f64 {
    6_371_008.8 * 53.07f64.to_radians().cos() * deg.to_radians()
}

fn m_lat(deg: f64) -> f64 {
    6_371_008.8 * deg.to_radians()
}

#[test]
fn within_box_place_and_area() {
    let v = values(
        "count of nodes n with n:amenity=bench within 8.8005...8.8035, 53.069...53.071\n\
         count of nodes n with n:amenity=bench within Mitte\n\
         count of nodes n with n:amenity=bench and n within Mitte\n\
         count of nodes n with n:amenity=bench and n not within Mitte\n\
         lib = pick area a with a:amenity=library\n\
         count of nodes x with x:amenity=bicycle_parking within lib\n\
         count of nodes n with n:amenity=bench within (areas r with r:boundary)",
    );
    // benches 1, 2 (3 is deleted)
    assert_eq!(num(&v[0]), 2.0);
    // benches 1, 2, 4, 5
    assert_eq!(num(&v[1]), 4.0);
    assert_eq!(num(&v[2]), 4.0);
    assert_eq!(num(&v[3]), 5.0);
    assert_eq!(num(&v[4]), 0.0);
    assert_eq!(num(&v[5]), 4.0);
}

#[test]
fn areas_are_geodesic() {
    let v = values(
        "(pick area a with a:amenity=library).area\n\
         (pick area p with p:leisure=park).area\n\
         (pick area p with p:leisure=park).area > 2 ha\n\
         average of (areas b with b:building).map(b => b.area)",
    );
    let lib = m_lon(0.001) * m_lat(0.001);
    approx(&v[0], lib, 0.01);
    assert_eq!(unit(&v[0]), "m2");
    // outer ring minus the hole
    let park = m_lon(0.002) * m_lat(0.002) - lib;
    approx(&v[1], park, 0.01);
    assert_eq!(v[2], json!(true));
    approx(&v[3], m_lon(0.0002) * m_lat(0.0002), 0.01);
}

#[test]
fn near_some_no_all() {
    let v = values(
        "libs = areas a with a:amenity=library\n\
         count of nodes b with b:amenity=bicycle_parking and b near some _ in libs closer than 50 m\n\
         count of nodes b with b:amenity=bicycle_parking and b near some _ in libs closer than 30 m\n\
         count of nodes b with b:amenity=bicycle_parking and b near no _ in libs closer than 50 m\n\
         count of nodes b with b:amenity=bicycle_parking and b near all _ in libs closer than 50 m\n\
         count of nodes b with b:amenity=bicycle_parking and b near some _ in libs\n\
         count of * b near some _ in libs closer than 50 m with b:amenity=bicycle_parking",
    );
    // parking 20 is 0.0005° of longitude (≈ 33 m) east of the library's edge
    assert!((m_lon(0.0005) - 33.4).abs() < 0.5);
    assert_eq!(num(&v[0]), 1.0);
    assert_eq!(num(&v[1]), 0.0);
    assert_eq!(num(&v[2]), 1.0);
    assert_eq!(num(&v[3]), 1.0);
    // default near_distance is 100 m
    assert_eq!(num(&v[4]), 1.0);
    assert_eq!(num(&v[5]), 1.0);
}

#[test]
fn near_distance_setting() {
    let v = values("near_distance = 20 m\nlibs = areas a with a:amenity=library\ncount of nodes b with b:amenity=bicycle_parking and b near some _ in libs");
    assert_eq!(num(&v[0]), 0.0);
}

#[test]
fn closer_than_is_strict() {
    // benches 1 and 2 are exactly 0.001° of longitude apart
    let d = m_lon(0.001);
    let v = values(&format!(
        "b2 = all n in (nodes x with x:amenity=bench) with n.id == 2\n\
         count of nodes n with n:amenity=bench and n.id == 1 and n near some _ in b2 closer than {} m\n\
         count of nodes n with n:amenity=bench and n.id == 1 and n near some _ in b2 closer than {} m",
        d + 0.5,
        d - 0.5
    ));
    assert_eq!(num(&v[0]), 1.0);
    assert_eq!(num(&v[1]), 0.0);
}

#[test]
fn overlapping() {
    let v = values(
        "lib = pick area l with l:amenity=library\n\
         count of areas a with a:amenity and a overlapping lib\n\
         count of areas b with b:building and b overlapping (pick area p with p:leisure=park)",
    );
    assert_eq!(num(&v[0]), 1.0);
    assert_eq!(num(&v[1]), 0.0);
}

#[test]
fn nearest() {
    let v = values(
        "shops = nodes s with s:shop\n\
         (nearest of shops to (pick node n with n:amenity=bench)).id",
    );
    assert_eq!(num(&v[0]), 600.0);
}

#[test]
fn grouping_by_areas_cells_and_tiles() {
    let v = values(
        "count of nodes n with n:amenity=bench per (areas r with r:boundary)\n\
         count of nodes n with n:amenity=bench per cell of 1 km\n\
         count of nodes n with n:amenity=bench per tile at zoom 14",
    );
    let a = v[0].as_array().unwrap();
    assert_eq!(a.len(), 1, "{}", v[0]);
    assert_eq!(num(&a[0]["count"]), 4.0);
    for t in &v[1..] {
        let sum: f64 = t.as_array().unwrap().iter().map(|r| num(&r["count"])).sum();
        assert_eq!(sum, 9.0, "{t}");
    }
    let cell = v[1].as_array().unwrap()[0].as_object().unwrap().values().find_map(|x| x.as_str().map(|s| s.to_string())).unwrap();
    assert!(cell.starts_with("1kmE") && cell.contains('N'), "{cell}");
    let tile = v[2].as_array().unwrap()[0].as_object().unwrap().values().find_map(|x| x.as_str().map(|s| s.to_string())).unwrap();
    assert!(tile.starts_with("14/"), "{tile}");
}

#[test]
fn routing_path_and_isochrone() {
    let v = values(
        "a = pick node n with n:amenity=bench\n\
         b = the n in (nodes x with x:amenity=bench) with n.id == 10\n\
         p = path P from a to b by foot\n\
         p.length\n\
         p.duration\n\
         p.edges\n\
         i = isochrone I from a closer than 5 min by foot\n\
         i.area",
    );
    // to the road (0.002° south), 9 road edges, back to the bench
    let len = 2.0 * m_lat(0.002) + m_lon(0.009);
    assert!((base(&v[0]) - len).abs() < 0.01 * len, "{} vs {len}", v[0]);
    // walking at 5 km/h
    let secs = len / (5.0 / 3.6);
    assert!((base(&v[1]) - secs).abs() < 0.01 * secs, "{} vs {secs} s", v[1]);
    assert_eq!(num(&v[2]), 1.0);
    let a = base(&v[3]);
    let r = 300.0 * 5.0 / 3.6;
    assert!(a > 0.0 && a < std::f64::consts::PI * r * r, "{a}");
}

#[test]
fn network_errors() {
    let r = fails("a = pick node n with n:amenity=bench\nb = pick node n with n:shop\npath P from a to b by transit", "E502");
    assert_eq!(r.code, 2);
    fails(
        "net = graph of ways w with w:highway=motorway\na = pick node n with n:amenity=bench\nb = pick node n with n:shop\npath P from a to b by foot using net",
        "E503",
    );
    let r = fails("benches = nodes n with n:amenity=bench\nb = pick node n with n:shop\npath P from benches to b by foot", "E108");
    assert!(r.err.contains("single place"), "{}", r.err);
}

/// The stored routing network, with CH and with CCH, routes like the network built for
/// the query.
#[test]
fn stored_routing_network_and_hierarchies() {
    use knut_exec::indexes::{self, Progress};
    use knut_format::container::{ElementKind, Temporal};
    let script = "a = pick node n with n:amenity=bench\n\
                  b = the n in (nodes x with x:amenity=bench) with n.id == 10\n\
                  p = path P from a to b by foot\n\
                  p.length\n\
                  p.duration\n\
                  (path P from a to b by car).duration\n\
                  (isochrone I from a closer than 5 min by foot).area";
    let plain = values_on(&db_copy("routing-plain"), script);
    let mut variants = Vec::new();
    for extra in [None, Some(ElementKind::RoutingCh), Some(ElementKind::RoutingCch)] {
        let d = db_copy(&format!("routing-{extra:?}"));
        indexes::add(&d, ElementKind::RoutingNetwork, Temporal::Latest, &Progress::default()).unwrap();
        if let Some(k) = extra {
            indexes::add(&d, k, Temporal::Latest, &Progress::default()).unwrap();
        }
        variants.push(values_on(&d, script));
    }
    let close = |a: &J, b: &J| {
        if a.is_null() || b.is_null() {
            return a.is_null() == b.is_null();
        }
        let (x, y) = (base(a), base(b));
        (x - y).abs() <= 0.02 * x.abs().max(1.0)
    };
    for v in &variants {
        for (a, b) in plain.iter().zip(v) {
            assert!(close(a, b), "{plain:?}\nvs\n{v:?}");
        }
    }
    // the three stored variants agree exactly on the routes
    assert_eq!(variants[0][..3], variants[1][..3]);
    assert_eq!(variants[0][..3], variants[2][..3]);
}
