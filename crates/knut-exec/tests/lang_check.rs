//! Problems found before anything runs: spelling (E1xx), names (E2xx), units (E3xx),
//! types (E6xx) and static warnings. Every code has at least one case that triggers it,
//! and the important ones a case that must not.

mod common;
use common::*;

// ----- E1xx: spelling ---------------------------------------------------------------------

#[test]
fn e101_single_equals_with_spaces() {
    let r = fails("ways w with w:highway = primary", "E101");
    assert!(r.err.contains("w:highway=primary") && r.err.contains("w:highway == primary"), "{}", r.err);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("Nothing was run."));
}

#[test]
fn e102_colon_with_spaces() {
    let r = fails("nodes n with n : amenity", "E102");
    assert!(r.err.contains("n:amenity"));
}

#[test]
fn e103_dot_ends_tag_key() {
    let r = fails("ignore_warnings = true\nnodes n with n:addr.housenumber == '1'", "E103");
    assert!(r.err.contains("n:['addr.housenumber']"), "{}", r.err);
}

#[test]
fn e104_from_to_last() {
    let r = fails("count of nodes n with n:amenity=bench per week from 2015-01 to 2015-03 within 8.7...8.9, 53.0...53.1", "E104");
    assert!(r.err.contains("within 8.7...8.9, 53.0...53.1 per week from 2015-01 to 2015-03"), "{}", r.err);
}

#[test]
fn e105_closer_than_after_set() {
    let r = fails("benches = nodes b with b:amenity=bench\nnodes h with h near some _ closer than 300 m in benches", "E105");
    assert!(r.err.contains("h near some _ in benches closer than 300 m"), "{}", r.err);
}

#[test]
fn e106_seed_without_randomly() {
    let r = fails("pick 4 nodes A using seed 7 with A:amenity=bench", "E106");
    assert!(r.err.contains("pick 4 nodes A randomly using seed 7 with A:amenity=bench"), "{}", r.err);
}

#[test]
fn e107_text_not_closed() {
    fails("nodes n with n:name == 'abc", "E107");
}

#[test]
fn e108_unexpected_word() {
    fails("count of nodes n with with n:amenity=bench", "E108");
}

#[test]
fn e109_bracket_not_closed_points_to_opening() {
    let r = fails("count of (nodes n with n:amenity=bench\ncount of nodes", "E109");
    assert!(r.err.contains("t.knut:1:10"), "{}", r.err);
    let r = fails("count of nodes n with n:amenity=bench)", "E109");
    assert!(r.err.contains("closes nothing"), "{}", r.err);
}

#[test]
fn e110_on_each_needs_range() {
    fails("count of nodes n with n:amenity=bench on each month", "E110");
}

#[test]
fn all_problems_at_once_sorted_by_line() {
    let r = run_text("ways w with w:highway = primary\ncount of libary\nnodes n with n : amenity");
    assert_eq!(r.code, 1);
    let a = r.err.find("[E101]").unwrap();
    let b = r.err.find("[E201]").unwrap();
    let c = r.err.find("[E102]").unwrap();
    assert!(a < b && b < c, "{}", r.err);
    assert!(r.err.contains("3 errors found. Nothing was run."), "{}", r.err);
    assert!(r.err.contains("[1] line 1:"), "{}", r.err);
}

#[test]
fn messages_in_german() {
    let r = fails("messages = de\nways w with w:highway = primary", "E101");
    assert!(r.err.contains("Fehler [E101]:  Das sieht nach einer Zuweisung aus"), "{}", r.err);
    assert!(r.err.contains("Es wurde nichts ausgeführt."), "{}", r.err);
}

// ----- E2xx: names, settings, files -------------------------------------------------------

#[test]
fn e201_undefined_with_suggestion_and_later_definition() {
    let r = fails("libraries = areas a with a:amenity=library\ncount of libary", "E201");
    assert!(r.err.contains("Did you mean \"libraries\"? (defined on line 1)"), "{}", r.err);
    let r = fails("count of shops\nshops = nodes s with s:shop", "E201");
    assert!(r.err.contains("defined later, on line 2"), "{}", r.err);
    // the internal "_" is never suggested
    let r = fails("count of nodes n with n:amenity=bench per s:shop", "E201");
    assert!(!r.err.contains("\"_\""), "{}", r.err);
}

#[test]
fn e202_undefined_network() {
    let r = fails(
        "net = graph of ways w with w:highway\na = pick node n with n:amenity=bench\nb = pick node n with n:shop\npath P from a to b by foot using myNet",
        "E202",
    );
    assert!(r.err.contains("\"net\""), "{}", r.err);
}

#[test]
fn e203_no_home_and_unknown_fields() {
    let r = fails("users u with u.home within 8.7...8.9, 53.0...53.1", "E203");
    assert!(r.err.contains("identifiable"), "{}", r.err);
    fails("define user U: home = U.firstEdit", "E203");
    let r = fails("nodes n with n.verison == 1", "E203");
    assert!(r.err.contains("version"), "{}", r.err);
}

#[test]
fn e204_setting_twice() {
    fails("seed = 1\nseed = 2\ncount of nodes n with n:amenity=bench", "E204");
}

#[test]
fn e205_no_database() {
    let opts = knut_exec::Options { timing: false, ..Default::default() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = knut_exec::run_source("count of nodes n with n:amenity=bench", Some("t.knut"), std::path::Path::new("."), &opts, &mut out, &mut err);
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, 1);
    assert!(err.contains("[E205]"), "{err}");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = knut_exec::run_source("knut_db = 'does-not-exist.knutdb'\ncount of nodes", Some("t.knut"), std::path::Path::new("."), &opts, &mut out, &mut err);
    assert_eq!(code, 1);
    assert!(String::from_utf8(err).unwrap().contains("[E205]"));
}

#[test]
fn e206_missing_notes_index() {
    let db = db_copy("e206");
    let r = run_on(&db, "count of notes", knut_exec::Format::Text, None);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("[E206]") && r.err.contains("knut index notes"), "{}", r.err);
}

#[test]
fn e207_no_user_information() {
    let r = run_on(db_without_users(), "(pick node n with n:amenity=bench).user", knut_exec::Format::Text, None);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("[E207]"), "{}", r.err);
}

#[test]
fn e208_defined_twice() {
    let r = fails("a = 1\na = 2\na", "E208");
    assert!(r.err.contains("line 1"), "{}", r.err);
}

#[test]
fn e209_field_cycle() {
    fails("define user U: a = U.b\ndefine user U: b = U.a\ncount of users u with u.a > 1", "E209");
}

#[test]
fn e210_e211_places() {
    let r = fails("count of nodes n with n:amenity=bench within Mitt", "E210");
    assert!(r.err.contains("within 'Mitte'"), "{}", r.err);
    let r = fails("count of nodes n with n:amenity=bench within Springfield", "E211");
    assert!(r.err.contains("matches 2 regions"), "{}", r.err);
}

#[test]
fn e212_variable_named_like_projection() {
    fails("war_B = 3\nwar = way A -> way B\nwar_B", "E212");
    fails("war = way A -> way B\nwar_B = 3\nwar_B", "E212");
}

#[test]
fn e213_dataset_missing() {
    fails("count of dataset '/does/not/exist.csv'", "E213");
}

#[test]
fn e215_bad_setting_value() {
    fails("seed = 'abc'\ncount of nodes n with n:amenity=bench", "E215");
    fails("messages = fr\ncount of nodes n with n:amenity=bench", "E215");
}

// ----- E3xx: dates and units --------------------------------------------------------------

#[test]
fn e301_ambiguous_date() {
    let r = fails("count of changesets from 7/3/2015 to 2015-12", "E301");
    assert!(r.err.contains("2015-03-07") && r.err.contains("2015-07-03"), "{}", r.err);
}

#[test]
fn e302_threshold_needs_unit() {
    let r = fails("s = nodes x with x:shop\nnodes h with h:amenity=bench and h near some _ in s closer than 15 by foot", "E302");
    assert!(r.err.contains("closer than 15 min by foot"), "{}", r.err);
}

#[test]
fn e303_wrong_measure() {
    fails("s = nodes x with x:shop\nnodes h with h:amenity=bench and h near some _ in s closer than 30 days", "E303");
}

#[test]
fn e305_unit_of_wrong_measure() {
    let r = fails("ways b with b:building and b:length mph > 20 m", "E305");
    assert!(r.err.contains("b:length m > 20 m"), "{}", r.err);
}

#[test]
fn e306_adding_different_measures() {
    fails("3 m + 2 km/h", "E306");
}

// ----- E5xx, E6xx -------------------------------------------------------------------------

#[test]
fn e501_near_needs_a_set() {
    let r = fails("nodes h with h near some _ closer than 300 m", "E501");
    assert!(r.err.contains("h near some _ in schools closer than 300 m"), "{}", r.err);
    // a single value after "in" is a type error found before running
    let r = fails("nodes n with n near some _ in 3", "E601");
    assert!(r.err.contains("Nothing was run."), "{}", r.err);
}

#[test]
fn e601_count_of_single_element() {
    fails("count of (pick node n with n:amenity=bench)", "E601");
}

#[test]
fn e603_chunk_needs_order() {
    fails("count of nodes n with n:shop per chunk of 5", "E603");
}

#[test]
fn e604_sequence_kinds_differ() {
    fails("way A -> node B", "E604");
}

#[test]
fn e605_condition_not_boolean() {
    fails("nodes n with n.version + 1", "E605");
}

#[test]
fn e606_function_alone() {
    fails("x => x", "E606");
}

#[test]
fn e608_spatial_left_side_is_a_condition() {
    let r = fails("s = nodes x with x:shop\ncount of nodes n with n:amenity=bench near some _ in s", "E608");
    assert!(r.err.contains("n:amenity=bench and n near some _ in s"), "{}", r.err);
    // `within` after a condition ends it and belongs to the selection (§3.4)
    assert_eq!(num(&one("count of nodes n with n:amenity=bench within Mitte")), 4.0);
}

// ----- static warnings --------------------------------------------------------------------

#[test]
fn w001_is_a_hint_and_does_not_stop() {
    // a comparison with a missing value is false: the run goes on without a message
    let r = run_text("count of ways w with w:highway and w:maxspeed > 40 km/h");
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!r.err.contains("[W001]"), "{}", r.err);
    // while typing, the check still points it out, as a hint
    let opts = knut_exec::Options { shared_db: None, ..Default::default() };
    let p = knut_exec::quick_check("count of ways w with w:maxspeed > 40 km/h", &opts);
    let w = p.iter().find(|p| p.code == "W001").expect("W001 hint");
    assert_eq!(w.severity, "info");
    assert!(w.text.contains("w:maxspeed > 40 km/h or w:maxspeed == none"), "{}", w.text);
}

#[test]
fn w003_large_region_without_filter() {
    reports("count of nodes", "W003");
    clean("count of nodes n with n:amenity=bench");
    clean("count of nodes within 8.79...8.81, 53.06...53.08");
}

#[test]
fn w005_unit_word_as_variable() {
    let r = reports("ignore_warnings = true\nc = count of nodes m with m:amenity=bench and m near some _ in (nodes x with x:shop) closer than 20000 m\nc", "W005");
    assert!(r.err.contains("\"m\""), "{}", r.err);
}

#[test]
fn w006_name_used_as_value() {
    let r = reports("ignore_warnings = true\nprimary = 3\ncount of ways w with w:highway == primary", "W006");
    assert!(r.err.contains("'primary'"), "{}", r.err);
    // an undefined name at a value position is text
    clean("count of ways w with w:highway == primary");
}

#[test]
fn check_only_runs_nothing() {
    let opts = knut_exec::Options { db: Some(db().to_path_buf()), check_only: true, timing: false, ..Default::default() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = knut_exec::run_source("count of nodes n with n:amenity=bench", None, std::path::Path::new("."), &opts, &mut out, &mut err);
    assert_eq!(code, 0);
    assert!(String::from_utf8(err).unwrap().contains("No problems found"));
    assert!(out.is_empty());
}

#[test]
fn explain_shows_pushdown() {
    let opts = knut_exec::Options { db: Some(db().to_path_buf()), explain: true, timing: false, ..Default::default() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = knut_exec::run_source("count of nodes n with n:amenity=bench and n.version > 1", None, std::path::Path::new("."), &opts, &mut out, &mut err);
    assert_eq!(code, 0);
    let s = String::from_utf8(out).unwrap() + &String::from_utf8(err).unwrap();
    assert!(s.contains("tag filter:  amenity=bench"), "{s}");
}
