//! ID-index id column: current FOR bitpacking vs "+1 run" coding per 512-entry chunk.
use knut_format::ObjType;
use knut_format::codec::*;
use knut::Reader;

fn col(v: &[u64]) -> usize {
    let mut b = Vec::new();
    put_u64_column(&mut b, v);
    b.len()
}

fn main() {
    let r = Reader::open(std::env::args().nth(1).unwrap()).unwrap();
    let ids = r.all_ids(ObjType::Node).unwrap();
    let (mut cur, mut runs_sz, mut nruns) = (0usize, 0usize, 0usize);
    for c in ids.chunks(512) {
        let mut prev = c[0];
        let d: Vec<u64> = c.iter().map(|&x| { let v = (x - prev) as u64; prev = x; v }).collect();
        cur += col(&d);
        let (mut gaps, mut lens) = (vec![], vec![]);
        let mut i = 0;
        let mut p = c[0] - 1;
        while i < c.len() {
            gaps.push((c[i] - p - 1) as u64);
            let mut j = i + 1;
            while j < c.len() && c[j] == c[j - 1] + 1 { j += 1; }
            lens.push((j - i - 1) as u64);
            p = c[j - 1];
            i = j;
        }
        nruns += gaps.len();
        runs_sz += col(&gaps) + col(&lens);
    }
    println!("{} ids: current {} bytes, +1-runs {} bytes ({} id runs, {:.1} ids/run)", ids.len(), cur, runs_sz, nruns, ids.len() as f64 / nruns as f64);
}
