//! Runs each query against a (possibly corrupt) file with checksums disabled and prints
//! timings. Usage: `cargo run --release --example probe -- FILE`
use knut_format::{BBox, ObjType};
use knut::{Filter, OpenOptions, Reader};
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let b = std::fs::read(&path).unwrap();
    let r = match Reader::from_bytes(b, OpenOptions { verify_checksums: false, cache_blocks: 16 }) {
        Ok(r) => r,
        Err(e) => return println!("open: {e}"),
    };
    let t0 = 1_300_000_000i64;
    let town = BBox::from_deg(8.79, 53.06, 8.83, 53.10);
    let step = |name: &str, f: &dyn Fn() -> String| {
        let st = Instant::now();
        println!("{name}: start");
        let out = f();
        println!("{name}: {:.1} ms -> {}", st.elapsed().as_secs_f64() * 1e3, out.chars().take(100).collect::<String>());
    };
    step("verify", &|| format!("{:?}", r.verify(true).map(|_| ())));
    step("get way", &|| format!("{:?}", r.get(ObjType::Way, 100).map(|v| v.len())));
    step("get node", &|| format!("{:?}", r.get(ObjType::Node, 1000).map(|v| v.len())));
    step("get rel", &|| format!("{:?}", r.get(ObjType::Relation, 9000).map(|v| v.len())));
    step("snapshot", &|| format!("{:?}", r.snapshot(&town, t0 + 450 * 86400, &Filter::default()).map(|v| v.len())));
    step("changes", &|| format!("{:?}", r.changes(&BBox::WORLD, t0, t0 + 1000 * 86400, &Filter::default()).map(|v| v.len())));
    step("geom changes", &|| format!("{:?}", r.way_geometry_changes(7000).map(|v| v.len())));
}
