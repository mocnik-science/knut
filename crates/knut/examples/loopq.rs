//! Repeats a file-wide tag snapshot (for profiling). Usage: loopq FILE KEY TYPE N
use knut_format::{BBox, ObjType};
use knut::{Filter, Reader};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let r = Reader::open(&a[1]).unwrap();
    let t = ObjType::parse(&a[3]).unwrap();
    let geometry = a.get(5).is_none_or(|g| g != "nogeom");
    for _ in 0..a[4].parse::<usize>().unwrap() {
        let st = std::time::Instant::now();
        let n = r.snapshot(&BBox::WORLD, r.header.tmax, &Filter { geometry, ..Filter::default().types(&[t]).tag(&a[2], None) }).unwrap().len();
        eprintln!("{n} in {:.0} ms", st.elapsed().as_secs_f64() * 1e3);
    }
}
