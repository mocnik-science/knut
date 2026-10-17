use knut_lang::diag::{Lang, render};
use knut_lang::parse;

fn ok(src: &str) {
    let (s, d) = parse(src);
    if !d.is_empty() {
        for x in &d {
            eprintln!("{}", render(x, src, None, Lang::En));
        }
        panic!("unexpected diagnostics for {src}");
    }
    assert!(!s.stmts.is_empty());
}

fn err(src: &str, code: &str) -> String {
    let (_, d) = parse(src);
    let all: Vec<_> = d.iter().map(|x| x.code).collect();
    assert!(all.contains(&code), "expected {code} for {src}, got {all:?}");
    d.iter().find(|x| x.code == code).map(|x| render(x, src, None, Lang::En)).unwrap()
}

#[test]
fn examples_from_description() {
    ok("seed = 99\nmessages = de\nignore_warnings = false\nknut_db = 'x.knutdb'");
    ok("someLibrary = pick area A with A:amenity=library");
    ok("fiveLibraries = pick 5 area A with A:amenity=library");
    ok("libraries = area A with A:amenity=library within 17.8419...18.1915, 59.2008...59.4449 on 8 Oct 2024");
    ok("nodesBicycleParkingNearALibrary = * b near some _ in libraries with b:amenity=bicycle_parking");
    ok("nodesChanging = node A -> node B -> node C with A:highway and C:highway\nnodesChanging_B:highway.exists");
    ok("war = way A -> way B -> way C closer than 30 days\n      with A.user == C.user and B.user != A.user\n       and A:maxspeed == C:maxspeed and A:maxspeed != B:maxspeed\ncount of war, count of (distinct of war_B.map(b => b.user))");
    ok("x = split changesets by user at unchangedBefore > 30 min");
    ok("p = path P from a to b by foot using net\ni = isochrone I from s closer than 15 min by foot");
    ok("h = homes h with h near some _ in schools closer than 300 m");
    ok("count of hot within Turkey per week from 2023-01 to 2023-03");
    ok("roads w with w:maxspeed > 50 km/h\nbuildings b with b:height m > 20 m\nwalkways w with w:incline % < 6 %\nroads w with w:maxweight t < 7.5 t\nparks p with p.area > 2 ha\nbuildings.map(b => b:height m ?? b:building:levels * 3 m)");
    ok("shops = areas shop with shop:shop\nshare of shop in shops per shop.type\nshare of (areas shop with shop:shop) per shop.type\nshare of (areas shop with shop:shop) per shop:shop");
    ok("define user U: firstEdit  = earliest of U.changesets\ndefine user U: activeSpan = U.lastEdit - U.firstEdit\ndefine user U: activeDays = count of days of U.changesets\ndefine *    X: creator    = user of first version of X");
    ok("shelters = areas s with s:amenity=shelter\nload     = count of h in homes per (nearest of shelters by foot)\ngroups g in load with g.count > (g.key:capacity ?? 0)\nshare of s in shelters per (s:capacity != none)");
    ok("users u with share of u.changesets within AT > 0.9");
    ok("x = n:['whatever.key'].length\ny = libraries:[K].count");
    ok("pick 4 areas A randomly using seed 7 with A:amenity=library");
    ok("the k in kataster overlapping b\nall c in census with c:id == d:ref\nthe area k in kataster overlapping b");
    ok("count of shops on each month from 2010 to 2020");
    ok("average of x in nodesInArea per chunk of 100 ordered by x.time");
    ok("count of x in xs per cell of 1 km\ncount of x in xs per tile at zoom 14");
    ok("u.firstEdit between 2020 and 2022\n30 days before T");
    ok("forecast 6 months of (count of c in cs per month)\nfit powerlaw to xs\ncorrelate a with b\nregress a on b\nconfusion of a with b");
    ok("classify buildings by x => x:building using x => [x.area, x:levels]");
    ok("d = dataset 'census.csv'\nw.previous:keys\nx.next:highway\nx.next ?? x");
    ok("count of shops on 7 Mar 2020\ncount of shops on Mar 2020\ncount of shops on 2020-03-07T14:30Z");
    ok("ways w with w:highway in [footway, path, steps]\nways w with not w:highway");
    ok("closer = 3\nx = h near all _ in s closer than or equal 30 m by foot");
}

#[test]
fn errors() {
    let e = err("ways w with w:highway = primary", "E101");
    assert!(e.contains("w:highway=primary"), "{e}");
    assert!(e.contains("w:highway == primary"), "{e}");
    let e = err("nodes n with n : amenity", "E102");
    assert!(e.contains("n:amenity"), "{e}");
    let e = err("count of hot per week from 2023-01 to 2023-03 within Turkey", "E104");
    assert!(e.contains("count of hot within Turkey per week from 2023-01 to 2023-03"), "{e}");
    let e = err("h near some _ closer than 300 m in schools", "E105");
    assert!(e.contains("h near some _ in schools closer than 300 m"), "{e}");
    let e = err("pick 4 areas A using seed 7 with A:amenity=library", "E106");
    assert!(e.contains("pick 4 areas A randomly using seed 7 with A:amenity=library"), "{e}");
    err("x = 'abc", "E107");
    let e = err("changesets from 7/3/2020 to 2020-12", "E301");
    assert!(e.contains("7 Mar 2020") && e.contains("3 Jul 2020"), "{e}");
    err("count of shops on each month", "E110");
    err("homes h with h near some _ closer than 300 m", "E501");
}

#[test]
fn render_style() {
    let src = "a = 1\nb = 2\nways w with w:highway = primary";
    let (_, d) = parse(src);
    let r = render(&d[0], src, None, Lang::En);
    assert!(r.starts_with("Error [E101]:  This looks like"), "{r}");
    assert!(r.contains("  3 | ways w with w:highway = primary\n                            ^\n"), "{r}");
}

#[test]
fn geocoding_and_points() {
    use knut_lang::ast::{ExprKind, Special, StmtKind};
    ok("geocode 'Am Wall 201, Bremen'");
    ok("a = geocoded 'Am Wall 201, Bremen'\na.city, a.state");
    ok("reverse-geocoded 8.8079E, 53.0758N");
    ok("(reverse-geocode 53.0758N, 8.8079E).map(x => [x.address, x.distance])");
    ok("first of geocode 'Am Wall, Bremen', last of geocode 'Am Wall, Bremen'");
    ok("path P from 73.98W, 40.75N to 74.01W, 40.70S by foot");
    // the point, whatever the order of the coordinates
    let (s, d) = knut_lang::parse("p = 53.0758N, 8.8079E");
    assert!(d.is_empty(), "{d:?}");
    let StmtKind::Assign { value, .. } = &s.stmts[0].kind else { panic!() };
    assert_eq!(value.kind, ExprKind::Point([8.8079, 53.0758]));
    let (s, _) = knut_lang::parse("reverse-geocode 8.8W, 53.07S");
    let StmtKind::Output(o) = &s.stmts[0].kind else { panic!() };
    let ExprKind::Special(sp) = &o[0].kind else { panic!() };
    let Special::ReverseGeocode { one: false, at } = &**sp else { panic!() };
    assert_eq!(at.kind, ExprKind::Point([-8.8, -53.07]));
    // two longitudes, or one coordinate alone
    err("8.8E, 9.1E", "E108");
    err("x = 8.8E", "E108");
}

#[test]
fn nearest_with_a_selection() {
    use knut_lang::ast::{ExprKind, OfFunc, OfOperand, StmtKind};
    ok("a = geocoded 'Am Wall 201, Bremen'\nnearest * x to a with x:amenity=school");
    ok("a = geocoded 'Am Wall 201, Bremen'\nnearest nodes n with n:amenity=bench to a by foot");
    ok("a = geocoded 'Am Wall 201, Bremen'\nd = nearest areas p to a with p:leisure=park within Bremen by bicycle\nd:name");
    // read as `nearest of (* x with …) to a`
    let (s, d) = knut_lang::parse("a = 8.8E, 53.07N\nnearest * x to a with x:amenity=school by foot");
    assert!(d.is_empty(), "{d:?}");
    let StmtKind::Output(o) = &s.stmts[1].kind else { panic!() };
    let ExprKind::Of { func: OfFunc::Nearest, operand: OfOperand::Expr(sel), clauses, .. } = &o[0].kind else { panic!("{:?}", o[0].kind) };
    let ExprKind::Select(sel) = &sel.kind else { panic!() };
    assert!(sel.cond.is_some());
    assert!(clauses.to.is_some() && clauses.mode.is_some());
}
