//! Criterion benchmarks for reference queries.
//!
//! `KNUT_BENCH_FILE=data/knut/bremen.knutdb KNUT_BENCH_BBOX=8.78,53.06,8.84,53.09 cargo bench -p knut`

use criterion::{Criterion, criterion_group, criterion_main};
use knut_format::{BBox, ObjType};
use knut::{Filter, Reader};
use std::hint::black_box;

fn bench(c: &mut Criterion) {
    let path = std::env::var("KNUT_BENCH_FILE").unwrap_or_else(|_| "../../data/knut/bremen.knutdb".into());
    let Ok(r) = Reader::open(&path) else {
        eprintln!("skipping benchmarks: cannot open {path} (set KNUT_BENCH_FILE)");
        return;
    };
    let bbox = std::env::var("KNUT_BENCH_BBOX")
        .ok()
        .and_then(|s| {
            let v: Vec<f64> = s.split(',').filter_map(|x| x.parse().ok()).collect();
            (v.len() == 4).then(|| BBox::from_deg(v[0], v[1], v[2], v[3]))
        })
        .unwrap_or_else(|| {
            let (cx, cy) = r.header.bbox.center();
            BBox::new(cx - 100_000, cy - 100_000, cx + 100_000, cy + 100_000)
        });
    let now = r.header.tmax;
    let year_ago = now - 365 * 86_400;
    let pick = |t: ObjType| -> Vec<i64> {
        let ids = r.all_ids(t).unwrap();
        let step = (ids.len() / 1000).max(1);
        ids.into_iter().step_by(step).take(1000).collect()
    };
    let nodes = pick(ObjType::Node);
    let ways = pick(ObjType::Way);
    let rels = pick(ObjType::Relation);

    let mut g = c.benchmark_group("knut");
    g.sample_size(20);
    let mut i = 0usize;
    g.bench_function("get_node", |b| {
        b.iter(|| {
            i = (i + 1) % nodes.len();
            black_box(r.get(ObjType::Node, nodes[i]).unwrap())
        })
    });
    g.bench_function("get_way", |b| {
        b.iter(|| {
            i = (i + 1) % ways.len();
            black_box(r.get(ObjType::Way, ways[i]).unwrap())
        })
    });
    if !rels.is_empty() {
        g.bench_function("get_relation", |b| {
            b.iter(|| {
                i = (i + 1) % rels.len();
                black_box(r.get(ObjType::Relation, rels[i]).unwrap())
            })
        });
    }
    g.bench_function("way_geometry_at", |b| {
        b.iter(|| {
            i = (i + 1) % ways.len();
            black_box(r.way_geometry_at(ways[i], now).unwrap())
        })
    });
    g.bench_function("snapshot_bbox", |b| b.iter(|| black_box(r.snapshot(&bbox, now, &Filter::default()).unwrap())));
    g.bench_function("snapshot_bbox_highway", |b| {
        b.iter(|| black_box(r.snapshot(&bbox, now, &Filter::default().tag("highway", None)).unwrap()))
    });
    g.bench_function("changes_bbox_1y", |b| {
        b.iter(|| black_box(r.changes(&bbox, year_ago, now, &Filter::default()).unwrap()))
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
