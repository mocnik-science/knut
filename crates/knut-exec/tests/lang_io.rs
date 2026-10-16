//! Input and output: formats, files, exit codes, run-time messages, data sets, notes,
//! settings.

mod common;
use common::*;
use knut_exec::Format;

#[test]
fn text_output() {
    let r = run_text("count of nodes n with n:amenity=bench\ncount of nodes s with s:shop per s:shop\npick 2 nodes n with n:amenity=bench");
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("── count of nodes n with n:amenity=bench\n9\n"), "{}", r.out);
    assert!(r.out.contains("s:shop       count"), "{}", r.out);
    assert!(r.out.contains("bakery          10"), "{}", r.out);
    // ids without digit grouping
    assert!(r.out.contains("node 1 v1"), "{}", r.out);
}

#[test]
fn csv_output() {
    let r = run_on(db(), "count of nodes s with s:shop per s:shop\ncount of nodes n with n:amenity=bench", Format::Csv, None);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("# count of nodes s with s:shop per s:shop\ns:shop,count\nbakery,10\nclothes,6\nsupermarket,4\n"), "{}", r.out);
    assert!(r.out.contains("value\n9\n"), "{}", r.out);
}

#[test]
fn json_output_shapes() {
    let r = run("count of nodes n with n:amenity=bench\n(pick area a with a:amenity=library).area\npick node n with n:amenity=bench");
    assert_eq!(r.code, 0, "{}", r.err);
    let j: J = serde_json::from_str(&r.out).unwrap();
    let a = j.as_array().unwrap();
    assert_eq!(a[0]["line"], 1);
    assert_eq!(a[0]["value"], 9, "integers stay integers");
    assert_eq!(a[1]["value"]["unit"], "m2");
    let e = &a[2]["value"];
    assert_eq!((e["type"].as_str(), e["id"].as_i64(), e["version"].as_i64()), (Some("node"), Some(1), Some(1)));
    assert_eq!(e["tags"]["amenity"], "bench");
    assert!(e["geometry"].as_str().unwrap_or_default().starts_with("POINT"), "{e}");
}

#[test]
fn files_in_out_dir_and_parquet_roundtrip() {
    let dir = scratch("out");
    let r = run_on(db(), "count of nodes s with s:shop per s:shop\nnodes s with s:shop=supermarket", Format::Parquet, Some(dir.join("sub")));
    assert_eq!(r.code, 0, "{}", r.err);
    let f = std::fs::File::open(dir.join("sub/output_1.parquet")).unwrap();
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).unwrap().build().unwrap();
    assert_eq!(reader.map(|b| b.unwrap().num_rows()).sum::<usize>(), 3);
    // a written file can be read back as a data set
    let v = values(&format!("d = dataset '{}'\ncount of d\ncount of all r in d with r:shop=supermarket", dir.join("sub/output_2.parquet").display()));
    assert_eq!(num(&v[0]), 4.0);
    let r = run_on(db(), "count of nodes s with s:shop per s:shop", Format::Csv, Some(dir.join("csv")));
    assert_eq!(r.code, 0);
    assert!(std::fs::read_to_string(dir.join("csv/output_1.csv")).unwrap().starts_with("s:shop,count\n"));
    let r = run_on(db(), "count of nodes n with n:amenity=bench", Format::Json, Some(dir.join("json")));
    assert_eq!(r.code, 0);
    assert!(std::fs::read_to_string(dir.join("json/outputs.json")).unwrap().contains("\"value\": 9"));
}

#[test]
fn exit_codes_and_stop_message() {
    assert_eq!(run_text("count of nodes n with n:amenity=bench").code, 0);
    assert_eq!(run_text("count of libary").code, 1);
    let r = run_text("count of nodes n with n:amenity=bench\ncount of nodes s with s:shop\nthe n in (nodes m with m:amenity=bench) with n:name='x'");
    assert_eq!(r.code, 2);
    assert!(r.err.contains("Stopped at line 3. Outputs 1–2 were written."), "{}", r.err);
    assert!(r.out.contains("── count of nodes s with s:shop"), "outputs before the stop are written: {}", r.out);
    let r = run_text("messages = de\nthe n in (nodes m with m:amenity=bench) with n:name='x'");
    assert!(r.err.contains("Fehler [E401]") && r.err.contains("Abgebrochen in Zeile 2"), "{}", r.err);
}

#[test]
fn data_warnings_after_the_run() {
    // W002: the query ran but found nothing
    let r = run_text("nodes n with n:amenity=nothing");
    assert_eq!(r.code, 0);
    assert!(r.err.contains("[W002]"), "{}", r.err);
    // values that are not numbers count as missing, without a message
    let r = run_text("count of ways w with w:highway and w:maxspeed > 40 km/h");
    assert_eq!(r.code, 0);
    assert!(r.err.trim().is_empty(), "{}", r.err);
}

#[test]
fn settings() {
    let a = values("seed = 1\npick 3 nodes n randomly with n:shop");
    let b = values("seed = 1\npick 3 nodes n randomly with n:shop");
    let c = values("pick 3 nodes n randomly using seed 1 with n:shop");
    assert_eq!(a, b);
    // `using seed` and the session seed give the same choice for the same number
    assert_eq!(a, c);
    let v = values("seed = 1\nseed");
    assert_eq!(num(&v[0]), 1.0);
}

#[test]
fn knut_db_relative_to_script() {
    let dir = scratch("rel");
    std::fs::copy(db(), dir.join("x.knutdb")).unwrap();
    let opts = knut_exec::Options { timing: false, ..Default::default() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = knut_exec::run_source("knut_db = 'x.knutdb'\ncount of nodes n with n:amenity=bench", Some("t.knut"), &dir, &opts, &mut out, &mut err);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert!(String::from_utf8(out).unwrap().contains("\n9\n"));
}

#[test]
fn datasets_csv_and_geojson() {
    let dir = scratch("data");
    std::fs::write(dir.join("points.csv"), "name,lon,lat,kind\nA,8.8015,53.0705,x\nB,9.5,54.0,y\nC,8.8021,53.0700,x\n").unwrap();
    std::fs::write(
        dir.join("zones.geojson"),
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"z1","properties":{"name":"west"},"geometry":{"type":"Polygon","coordinates":[[[8.7995,53.0695],[8.8035,53.0695],[8.8035,53.0705],[8.7995,53.0705],[8.7995,53.0695]]]}},
            {"type":"Feature","properties":{"name":"empty","n":3},"geometry":null}
        ]}"#,
    )
    .unwrap();
    let v = values(&format!(
        "d = dataset '{}'\n\
         z = dataset '{}'\n\
         count of d\n\
         count of all r in d with r:kind=x\n\
         count of nodes n with n:amenity=bench and n near some _ in d closer than 200 m\n\
         count of nodes n with n:amenity=bench within (pick r in z with r:name=west)\n\
         (pick r in z with r:name=empty):n\n\
         count of r in d per r:kind",
        dir.join("points.csv").display(),
        dir.join("zones.geojson").display()
    ));
    assert_eq!(num(&v[0]), 3.0);
    assert_eq!(num(&v[1]), 2.0);
    // benches 1, 2 and 4 lie within 200 m of A; bench 5 is 194 m from C
    assert_eq!(num(&v[2]), 4.0);
    // benches 1 and 2 (3 is deleted)
    assert_eq!(num(&v[3]), 2.0);
    assert_eq!(v[4], serde_json::json!("3"));
    assert_eq!(v[5].as_array().unwrap().len(), 2);
}

#[test]
fn notes() {
    let db = db_copy("notes");
    let dir = scratch("notes");
    let osn = dir.join("notes.osn");
    std::fs::write(
        &osn,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osm-notes>
<note id="1" lat="53.07" lon="8.80" created_at="2015-02-01T10:00:00Z">
<comment action="opened" timestamp="2015-02-01T10:00:00Z">bench missing &amp; broken</comment>
</note>
<note id="2" lat="53.07" lon="8.90" created_at="2015-03-01T10:00:00Z" closed_at="2015-03-05T10:00:00Z">
<comment action="opened" timestamp="2015-03-01T10:00:00Z">wrong name</comment>
<comment action="closed" timestamp="2015-03-05T10:00:00Z">fixed</comment>
</note>
</osm-notes>
"#,
    )
    .unwrap();
    knut_exec::notes::build_sidecar(&db, &osn).unwrap();
    let v = values_on(
        &db,
        "count of notes\n\
         count of notes n with n.status == 'open'\n\
         (the n in notes with n.status == 'open').text\n\
         (the n in notes with n.status == 'closed').comments\n\
         count of notes n with n within Mitte",
    );
    assert_eq!(num(&v[0]), 2.0);
    assert_eq!(num(&v[1]), 1.0);
    assert_eq!(v[2], serde_json::json!("bench missing & broken"));
    assert_eq!(num(&v[3]), 2.0);
    assert_eq!(num(&v[4]), 1.0);
}

#[test]
fn results_do_not_depend_on_threads() {
    let script = "count of nodes s with s:shop per s:shop\npick 3 nodes n randomly with n:shop\ndistribution of (nodes n with n:amenity=bench).map(b => b.version)";
    let one_thread = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap().install(|| run(script).out);
    let many = rayon::ThreadPoolBuilder::new().num_threads(8).build().unwrap().install(|| run(script).out);
    let strip = |s: &str| -> J {
        let mut j: J = serde_json::from_str(s).unwrap();
        for o in j.as_array_mut().unwrap() {
            o.as_object_mut().unwrap().remove("seconds");
        }
        j
    };
    assert_eq!(strip(&one_thread), strip(&many));
}

/// The place index and the tag statistics give the same answers as the scans they replace.
#[test]
fn indexes_answer_like_the_scans() {
    use knut_exec::indexes::{self, Progress};
    use knut_format::container::{ElementKind, Temporal};
    let scripts = [
        "count of nodes n with n:amenity=bench within Mitte",
        "count of nodes n with n:amenity=bench within 'mitte'",
        "count of keys",
        "(pick key k with k.name == 'amenity').count",
        "(pick key k with k.name == 'amenity').values",
    ];
    let plain = db_copy("indexes-plain");
    let indexed = db_copy("indexes");
    for k in [ElementKind::PlaceIndex, ElementKind::TagStatistics] {
        indexes::add(&indexed, k, Temporal::Latest, &Progress::default()).unwrap();
    }
    let h = knut::container::header(&indexed).unwrap();
    assert!(h.directory.elements.iter().any(|e| e.kind == ElementKind::PlaceIndex));
    for s in scripts {
        assert_eq!(values_on(&indexed, s), values_on(&plain, s), "{s}");
    }
    // ambiguous and unknown names behave as before
    for s in ["count of nodes n within Springfield", "count of nodes n within Nowhere"] {
        let a = run_on(&indexed, s, knut_exec::Format::Text, None);
        let b = run_on(&plain, s, knut_exec::Format::Text, None);
        assert_eq!((a.code, a.err.lines().next().map(str::to_string)), (b.code, b.err.lines().next().map(str::to_string)), "{s}");
    }
}
