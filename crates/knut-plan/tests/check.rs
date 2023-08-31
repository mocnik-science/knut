use knut_lang::diag::{Lang, render};
use knut_plan::{DbCaps, check};

fn run(src: &str) -> knut_plan::Checked {
    let (s, d) = knut_lang::parse(src);
    let caps = DbCaps { users: true, notes: false, extent_km2: 50_000.0, path: "x.knutdb".into() };
    check(s, src, d, Some(&caps))
}

fn has(src: &str, code: &str) -> String {
    let c = run(src);
    let codes: Vec<_> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.contains(&code), "expected {code} in {codes:?} for\n{src}");
    render(c.diags.iter().find(|d| d.code == code).unwrap(), src, None, Lang::En)
}

fn clean(src: &str) {
    let c = run(src);
    for d in &c.diags {
        eprintln!("{}", render(d, src, None, Lang::En));
    }
    assert!(c.diags.is_empty(), "unexpected diagnostics");
}

#[test]
fn clean_scripts() {
    clean("libraries = areas A with A:amenity=library within 16.18...16.58, 48.11...48.33\ncount of libraries");
    clean("war = way A -> way B -> way C closer than 30 days\n      with A.user == C.user and B.user != A.user\n       and A:maxspeed == C:maxspeed\ncount of war, count of (distinct of war_B.map(b => b.user))");
    clean("shops = areas shop with shop:shop within 16.18...16.58, 48.11...48.33\nshare of shop in shops per shop.type\nshare of (areas shop with shop:shop within 16.18...16.58, 48.11...48.33) per shop.type");
    clean("define user U: activeDays2 = count of days of U.changesets\nusers u with u.activeDays2 > 3");
    clean("s = split changesets by user at unchangedBefore > 30 min\ncount of s");
    clean("x = nodes n with n:amenity within 16.18...16.58, 48.11...48.33\ncount of x per user having count < 5");
    clean("ways w with w:maxspeed > 50 km/h or w:maxspeed == none within 16.18...16.58, 48.11...48.33");
}

#[test]
fn errors() {
    let e = has("libraries = areas a with a:amenity=library\ncount of libary", "E201");
    assert!(e.contains("Did you mean \"libraries\"? (defined on line 1)"), "{e}");
    let e = has("users u with u.home within AT", "E203");
    assert!(e.contains("share of u.changesets"), "{e}");
    let e = has("h = homes h with h near some _ in schools closer than 15 by foot", "E302");
    assert!(e.contains("closer than 15 min by foot"), "{e}");
    has("x = nodes h with h near some _ in nodes closer than 30 days", "E303");
    let e = has("buildings b with b:length mph > 20 m", "E305");
    assert!(e.contains("b:length m > 20 m"), "{e}");
    let e = has("300 m + 50 km/h", "E306");
    assert!(e.contains("A length and a speed cannot be added."), "{e}");
    let e = has("count of (pick area A with A:amenity=library)", "E601");
    assert!(e.contains("count of (pick areas A with A:amenity=library)"), "{e}");
    assert!(e.contains("count of (areas A with A:amenity=library)"), "{e}");
    has("average of x in nodes per chunk of 100", "E603");
    has("seed = 1\nseed = 2", "E204");
    has("a = 1\na = 2", "E208");
    has("n:addr.housenumber", "E103");
    has("way A -> node B", "E604");
    has("nodes n with n.id", "E605");
    has("notes n with n.status == open", "E206");
}

#[test]
fn warnings() {
    let e = has("ways w with w:maxspeed > 50 km/h", "W001");
    assert!(e.contains("w:maxspeed > 50 km/h or w:maxspeed == none"), "{e}");
    assert!(e.contains("Ways without maxspeed will be left out."), "{e}");
    has("m = 3\n[m].map(m => m * 2 m)", "W005");
    has("primary = 3\nways w with w:highway == primary", "W006");
    has("ways w", "W003");
}
