//! Experiment: alternative encodings for private-node coordinates (pe_lon/pe_lat).
//! Usage: cargo run --release --example coordexp -- FILE.knutdb [sample_every]
use knut_format::codec::*;
use knut_format::{ObjType, SegmentBody};
use knut::Reader;
use rayon::prelude::*;

fn z(b: &[u8]) -> usize {
    zstd::bulk::compress(b, 19).unwrap().len()
}

fn col(vals: &[u64]) -> Vec<u8> {
    let mut b = Vec::new();
    put_u64_column(&mut b, vals);
    b
}

/// Patched FOR: width chosen to minimize size; exceptions stored as (pos, value) columns.
fn pfor(vals: &[u64]) -> Vec<u8> {
    let mut best: Option<Vec<u8>> = None;
    for w in 0..=32u32 {
        let lim = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
        let mut main = Vec::with_capacity(vals.len());
        let (mut pos, mut exc) = (Vec::new(), Vec::new());
        let mut last = 0u64;
        for (i, &v) in vals.iter().enumerate() {
            if v > lim {
                pos.push(i as u64 - last);
                last = i as u64;
                exc.push(v);
                main.push(0);
            } else {
                main.push(v);
            }
        }
        let mut b = vec![w as u8];
        bitpack(main.iter().copied(), w, &mut b);
        put_u64_column(&mut b, &pos);
        put_u64_column(&mut b, &exc);
        if best.as_ref().map_or(true, |x| b.len() < x.len()) {
            best = Some(b);
        }
    }
    best.unwrap()
}

/// Little-endian byte planes of 32-bit values (shuffle), compressed by zstd afterwards.
fn planes(vals: &[u64]) -> Vec<u8> {
    let mut b = Vec::with_capacity(vals.len() * 4);
    for k in 0..4 {
        b.extend(vals.iter().map(|v| (v >> (8 * k)) as u8));
    }
    b
}

/// Run-length of "previous + 1" ids: (zigzag(gap), run length) pairs.
fn plus1_runs(vals: &[i64]) -> Vec<u8> {
    let (mut gaps, mut lens) = (Vec::new(), Vec::new());
    let mut prev = 0i64;
    let mut i = 0;
    while i < vals.len() {
        gaps.push(zigzag(vals[i] - prev - 1));
        let mut j = i + 1;
        while j < vals.len() && vals[j] == vals[j - 1] + 1 {
            j += 1;
        }
        lens.push((j - i - 1) as u64);
        prev = vals[j - 1];
        i = j;
    }
    let mut b = col(&gaps);
    b.extend(col(&lens));
    b
}

fn slot_main(r: &Reader, blocks: &[u32]) {
    let res: Vec<[(usize, usize); 3]> = blocks
        .par_iter()
        .map(|&no| {
            let b = r.block(no).unwrap();
            let ids: Vec<i64> = b
                .segs
                .iter()
                .flat_map(|s| match &s.body {
                    SegmentBody::Way { slots, .. } => slots.iter().map(|x| x.node_id).collect::<Vec<_>>(),
                    _ => vec![],
                })
                .collect();
            let mut prev = 0;
            let d: Vec<u64> = ids
                .iter()
                .map(|&x| {
                    let v = zigzag(x - prev);
                    prev = x;
                    v
                })
                .collect();
            let enc = [col(&d), pfor(&d), plus1_runs(&ids)];
            let mut out = [(0, 0); 3];
            for (k, e) in enc.iter().enumerate() {
                out[k] = (e.len(), z(e));
            }
            out
        })
        .collect();
    let mut tot = [(0usize, 0usize); 3];
    for x in res {
        for k in 0..3 {
            tot[k].0 += x[k].0;
            tot[k].1 += x[k].1;
        }
    }
    for (k, n) in ["slot ids: current", "slot ids: pfor", "slot ids: +1 runs"].iter().enumerate() {
        println!("{n:<30} raw {:>12}  zstd {:>12}", tot[k].0, tot[k].1);
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let every: u32 = std::env::args().nth(2).map_or(10, |s| s.parse().unwrap());
    let r = Reader::open(&path).unwrap();
    let blocks: Vec<u32> = (0..r.n_blocks() as u32)
        .filter(|&b| r.block_entry(b).unwrap().obj_type == ObjType::Way && b % every == 0)
        .collect();
    slot_main(&r, &blocks);
    if std::env::var("SLOTS_ONLY").is_ok() {
        return;
    }
    // [name] -> (raw bytes, zstd bytes)
    let names = [
        "A current delta(block)", "B pfor delta(block)", "C delta, seg-start separate", "D 2nd order (seg)",
        "E planes delta(block)", "F pfor 2nd order",
    ];
    let res: Vec<[(usize, usize); 6]> = blocks
        .par_iter()
        .map(|&no| {
            let b = r.block(no).unwrap();
            let mut out = [(0, 0); 6];
            for dim in 0..2 {
                // sequences of visible private coordinates per segment, in encoding order
                let mut seqs: Vec<Vec<i64>> = Vec::new();
                for s in &b.segs {
                    if let SegmentBody::Way { slots, .. } = &s.body {
                        let mut v = Vec::new();
                        for sl in slots {
                            for e in sl.private.as_deref().unwrap_or(&[]) {
                                if e.visible {
                                    v.push(if dim == 0 { e.lon } else { e.lat } as i64);
                                }
                            }
                        }
                        seqs.push(v);
                    }
                }
                let mut a = Vec::new();
                let mut prev = 0i64;
                for s in &seqs {
                    for &x in s {
                        a.push(zigzag(x - prev));
                        prev = x;
                    }
                }
                let (mut cin, mut cstart) = (Vec::new(), Vec::new());
                let mut d2 = Vec::new();
                let mut prev_start = 0i64;
                for s in &seqs {
                    for (i, &x) in s.iter().enumerate() {
                        if i == 0 {
                            cstart.push(zigzag(x - prev_start));
                            prev_start = x;
                        } else {
                            cin.push(zigzag(x - s[i - 1]));
                        }
                        if i >= 2 {
                            d2.push(zigzag(x - (2 * s[i - 1] - s[i - 2])));
                        } else if i == 1 {
                            d2.push(zigzag(x - s[0]));
                        }
                    }
                }
                let mut cpair = col(&cin);
                cpair.extend(col(&cstart));
                let mut d2b = col(&d2);
                d2b.extend(col(&cstart));
                let mut d2p = pfor(&d2);
                d2p.extend(col(&cstart));
                let enc = [col(&a), pfor(&a), cpair, d2b, planes(&a), d2p];
                for (k, e) in enc.iter().enumerate() {
                    out[k].0 += e.len();
                    out[k].1 += z(e);
                }
            }
            out
        })
        .collect();
    let mut tot = [(0usize, 0usize); 6];
    for r in res {
        for k in 0..6 {
            tot[k].0 += r[k].0;
            tot[k].1 += r[k].1;
        }
    }
    println!("{} way blocks sampled (every {every})", blocks.len());
    for k in 0..6 {
        println!("{:<30} raw {:>12}  zstd {:>12}  ({:.2}x of A)", names[k], tot[k].0, tot[k].1, tot[k].1 as f64 / tot[0].1 as f64);
    }
}
