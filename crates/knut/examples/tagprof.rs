//! Where does time go in a file-wide tag query? Usage: tagprof FILE KEY
use knut_format::block::{self, DecodeOptions};
use knut_format::{BBox, ObjType};
use knut::{Filter, Reader};
use rayon::prelude::*;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let r = Reader::open(&a[1]).unwrap();
    let key = &a[2];
    for t in ObjType::ALL {
        let (e, runs, ch, idb) = r.index_stats(t).unwrap();
        println!("idx {:?}: entries={e} runs={runs} ({:.1} entries/run) chunks={ch} id-delta bytes={idb}", t, e as f64 / runs as f64);
    }
    let f = Filter::default().types(&[ObjType::Node, ObjType::Way]).tag(key, None);
    // blocks selected by the bloom filter
    let kid = r.key_id(key).unwrap();
    let h = knut_format::bloom::hash_key(kid);
    let mut sel = Vec::new();
    for tree in 0..4 {
        sel.extend(r.query_tree(tree, &|_, b| knut_format::bloom::test(b, h)).unwrap());
    }
    println!("blocks passing bloom for {key}: {} of {}", sel.len(), r.n_blocks());
    let t0 = Instant::now();
    let frames: Vec<_> = sel.par_iter().map(|&no| r.raw_frames(no).unwrap()).collect();
    println!("decompress (zstd) only:          {:>7.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    let tc = Instant::now();
    let nc: usize = frames
        .par_iter()
        .map(|(h, _dir, data, geom)| block::data_column_ranges(h.obj_type, data, geom).unwrap().len())
        .sum();
    println!("columns only (varint/bitpack):   {:>7.0} ms  ({nc} columns)", tc.elapsed().as_secs_f64() * 1e3);
    let t1 = Instant::now();
    let n: usize = frames
        .par_iter()
        .map(|(h, dir, data, geom)| {
            let dirs = block::decode_directory(dir, h.n_segments).unwrap();
            let (segs, _) = block::decode_data_lazy(h.obj_type, &dirs, data, geom, &r.dicts,
                &DecodeOptions { time_base: r.time_base() }, true).unwrap();
            segs.len()
        })
        .sum();
    println!("decode to segments (no zstd):    {:>7.0} ms  ({n} segments)", t1.elapsed().as_secs_f64() * 1e3);
    let t2 = Instant::now();
    let res = r.snapshot(&BBox::WORLD, r.header.tmax, &f).unwrap();
    println!("full snapshot query (cold cache): {:>7.0} ms  ({} results)", t2.elapsed().as_secs_f64() * 1e3, res.len());
    let ways: Vec<i64> = res.iter().filter(|f| f.obj_type == ObjType::Way).flat_map(|f| f.nodes.clone()).collect();
    println!("way node refs to resolve: {}", ways.len());
    let f2 = Filter { include_private_nodes: false, ..Filter::default().types(&[ObjType::Node]).tag(key, None) };
    let t3 = Instant::now();
    let n2 = r.snapshot(&BBox::WORLD, r.header.tmax, &f2).unwrap().len();
    println!("nodes only (warm):               {:>7.0} ms  ({n2})", t3.elapsed().as_secs_f64() * 1e3);
}
