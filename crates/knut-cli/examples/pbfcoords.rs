//! Cost of node coordinates in PBF order (id order) with the knut column codec + zstd 19.
use knut_builder::pbf::PbfFile;
use knut_builder::{Element, Source};
use knut_format::codec::{put_u64_column, zigzag};
use rayon::prelude::*;

fn main() {
    let src = PbfFile::open(std::path::Path::new(&std::env::args().nth(1).unwrap())).unwrap();
    let (mut raw, mut comp, mut n, mut ids) = (0usize, 0usize, 0usize, 0usize);
    src.nodes(&mut |b| {
        // 32k-value chunks ~ one knut block worth of coordinates
        let pts: Vec<(i64, i32, i32)> = b.iter().filter_map(|e| match e {
            Element::Node(x) if x.meta.visible => Some((x.id, x.lon, x.lat)), _ => None }).collect();
        let r: Vec<(usize, usize, usize, usize)> = pts.par_chunks(32768).map(|c| {
            let (mut px, mut py, mut pid) = (0i64, 0i64, 0i64);
            let (mut dx, mut dy, mut di) = (vec![], vec![], vec![]);
            for &(id, x, y) in c {
                dx.push(zigzag(x as i64 - px)); dy.push(zigzag(y as i64 - py)); di.push(zigzag(id - pid));
                px = x as i64; py = y as i64; pid = id;
            }
            let (mut bx, mut by, mut bi) = (vec![], vec![], vec![]);
            put_u64_column(&mut bx, &dx); put_u64_column(&mut by, &dy); put_u64_column(&mut bi, &di);
            let z = |b: &[u8]| zstd::bulk::compress(b, 19).unwrap().len();
            (bx.len() + by.len(), z(&bx) + z(&by), c.len(), z(&bi))
        }).collect();
        for (a, c, k, i) in r { raw += a; comp += c; n += k; ids += i; }
        Ok(())
    }).unwrap();
    println!("visible node versions {n}: coords raw {raw} zstd {comp} ({:.2} bytes/version); id deltas zstd {ids}", comp as f64 / n as f64);
}
