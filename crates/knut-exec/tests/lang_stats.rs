//! Statistics (KNUT_LANG.md, Anhang A) with values computed by hand.

mod common;
use common::*;
use serde_json::json;

fn list(xs: impl Iterator<Item = String>) -> String {
    format!("[{}]", xs.collect::<Vec<_>>().join(", "))
}

#[test]
fn distribution_of_few_values() {
    let v = one("distribution of [1, 1, 2]");
    assert_eq!(rows(&v, "value", "count"), vec![(json!(1), json!(2)), (json!(2), json!(1))]);
}

#[test]
fn distribution_in_linear_classes() {
    let v = one(&format!("distribution of {}", list((0..100).map(|i| i.to_string()))));
    let r = v.as_array().unwrap();
    // (99 − 0) / 20 → step 5: [0, 5), [5, 10), … [95, 100)
    assert_eq!(r.len(), 20, "{v}");
    assert!(r.iter().all(|x| num(&x["count"]) == 5.0), "{v}");
    assert_eq!((num(&r[0]["from"]), num(&r[0]["to"])), (0.0, 5.0));
}

#[test]
fn distribution_in_log_classes() {
    // 10^(i/100) for i = 0 … 300: 1 to 1000
    let v = one(&format!("distribution of {}", list((0..=300).map(|i| format!("{:.6}", 10f64.powf(i as f64 / 100.0))))));
    let r = v.as_array().unwrap();
    let bounds: Vec<f64> = r.iter().map(|x| num(&x["from"])).collect();
    assert_eq!(bounds, vec![1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0], "{v}");
    let total: f64 = r.iter().map(|x| num(&x["count"])).sum();
    assert_eq!(total, 301.0);
}

#[test]
fn entropy_gini_variance() {
    let v = values(
        "c = count of nodes s with s:shop per s:shop\n\
         entropy of c\n\
         entropy of (nodes s with s:shop).map(s => s:shop)\n\
         gini of c\n\
         gini of [5, 5, 5]\n\
         variance of c\n\
         variance of [1]",
    );
    let h = -(0.5f64 * 0.5f64.log2() + 0.3 * 0.3f64.log2() + 0.2 * 0.2f64.log2());
    approx(&v[0], h, 1e-12);
    approx(&v[1], h, 1e-12);
    // sorted 4, 6, 10: 2·(4 + 12 + 30) / (3 · 20) − 4/3
    approx(&v[2], 0.2, 1e-12);
    assert_eq!(num(&v[3]), 0.0);
    approx(&v[4], 28.0 / 3.0, 1e-12);
    assert_eq!(v[5], J::Null);
}

#[test]
fn spread_and_density() {
    let v = values(
        "spread of all n in (nodes x with x:amenity=bench) with n.id <= 2\n\
         density of nodes n with n:amenity=bench within Mitte",
    );
    // two points: each half the distance from the centre
    let d = 6_371_008.8 * 53.07f64.to_radians().cos() * 0.001f64.to_radians();
    assert!((base(&v[0]) - d / 2.0).abs() < 0.01, "{}", v[0]);
    // 4 benches in 0.006° × 0.001°
    let km2 = d * 6.0 * 6_371_008.8 * 0.001f64.to_radians() / 1e6;
    approx(&v[1], 4.0 / km2, 0.01);
}

#[test]
fn streaks_and_transitions() {
    let v = values(
        "streaks of (nodes s with s:shop)\n\
         transitions of ['a', 'b', 'b', 'a', 'b']",
    );
    // latest shop versions: days 1–19 (shops 601–619) and day 40 (shop 600 v2)
    let s: Vec<(f64, String)> = v[0].as_array().unwrap().iter().map(|x| (num(x), unit(x))).collect();
    assert_eq!(s, vec![(19.0, "days".into()), (1.0, "days".into())]);
    assert_eq!(rows(&v[1], "transition", "count"), vec![(json!("a → b"), json!(2)), (json!("b → a"), json!(1))]);
}

#[test]
fn correlation_regression_powerlaw() {
    let v = values(
        "correlate [1, 2, 3] with [2, 4, 6]\n\
         correlate [1, 2, 3] with [3, 2, 1]\n\
         r = regress [2, 4, 6] on [1, 2, 3]\n\
         r.slope\n\
         r.intercept\n\
         r.r2\n\
         r.n\n\
         p = fit powerlaw to [1, 2, 4, 8]\n\
         p.alpha\n\
         p.xmin",
    );
    approx(&v[0], 1.0, 1e-12);
    approx(&v[1], -1.0, 1e-12);
    approx(&v[2], 2.0, 1e-12);
    assert!(num(&v[3]).abs() < 1e-12);
    approx(&v[4], 1.0, 1e-12);
    assert_eq!(num(&v[5]), 3.0);
    approx(&v[6], 1.0 + 4.0 / (6.0 * 2f64.ln()), 1e-12);
    assert_eq!(num(&v[7]), 1.0);
}

#[test]
fn correlation_pairs_groups_by_key() {
    // the same days in both series, in a different order of computation
    let v = one(
        "a = count of nodes s with s:shop per day from 2015-01-01 to 2015-01-10\n\
         b = count of nodes s with s:shop and s.id >= 600 per day from 2015-01-01 to 2015-01-10\n\
         correlate a with b",
    );
    // both series are constant 1: no correlation can be computed
    assert_eq!(v, J::Null);
}

#[test]
fn forecast_and_confusion() {
    let v = values(
        "forecast 2 days of (count of nodes s with s:shop per day from 2015-01-01 to 2015-01-05)\n\
         confusion of ['a', 'a', 'b'] with ['a', 'b', 'b']",
    );
    let f: Vec<(J, f64)> = v[0].as_array().unwrap().iter().map(|r| (r["day"].clone(), num(&r["forecast"]))).collect();
    assert_eq!(f, vec![(json!("2015-01-06"), 1.0), (json!("2015-01-07"), 1.0)]);
    let c = v[1].as_array().unwrap();
    assert_eq!(c.len(), 3, "{}", v[1]);
    assert!(c.iter().all(|r| num(&r["count"]) == 1.0), "{}", v[1]);
}

#[test]
fn classify_by_nearest_neighbours() {
    let v = values(
        "c = classify (nodes s with s:shop) by s => s:shop using s => [s.lon]\n\
         c.accuracy",
    );
    // shops of one kind lie next to each other: only the edges of the groups can be wrong
    let a = num(&v[0]);
    assert!((0.7..=1.0).contains(&a), "{a}");
}
