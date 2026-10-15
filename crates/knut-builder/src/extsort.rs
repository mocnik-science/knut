//! External sorting with a memory budget: records are buffered, sorted and spilled as
//! zstd-compressed runs to a temporary directory, then merged with a k-way heap merge.
//! Without spilling, everything stays in memory.

use crate::{Result, err};
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fixed-size record that can be spilled.
pub trait Rec: Copy + Ord + Send + Sync + 'static {
    const SIZE: usize;
    fn put(&self, out: &mut Vec<u8>);
    fn get(b: &[u8]) -> Self;
}

impl Rec for i64 {
    const SIZE: usize = 8;
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        i64::from_le_bytes(b[..8].try_into().unwrap())
    }
}

impl Rec for (i64, i64) {
    const SIZE: usize = 16;
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
        out.extend_from_slice(&self.1.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        (i64::from_le_bytes(b[..8].try_into().unwrap()), i64::from_le_bytes(b[8..16].try_into().unwrap()))
    }
}

impl Rec for (i64, u32) {
    const SIZE: usize = 12;
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
        out.extend_from_slice(&self.1.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        (i64::from_le_bytes(b[..8].try_into().unwrap()), u32::from_le_bytes(b[8..12].try_into().unwrap()))
    }
}

/// Creates a unique temporary file path inside `dir`.
pub fn tmp_path(dir: &Path, tag: &str) -> PathBuf {
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("knut-{}-{tag}-{n}.tmp", std::process::id()))
}

/// Writes records as a zstd stream (level 1).
fn write_run<T: Rec>(path: &Path, recs: &[T]) -> Result<()> {
    let f = File::create(path).map_err(|e| err(format!("create {}: {e}", path.display())))?;
    let mut z = zstd::stream::Encoder::new(BufWriter::with_capacity(1 << 20, f), 1)?;
    let mut buf = Vec::with_capacity(T::SIZE * 4096);
    for c in recs.chunks(4096) {
        buf.clear();
        for r in c {
            r.put(&mut buf);
        }
        z.write_all(&buf)?;
    }
    z.finish()?.flush()?;
    Ok(())
}

struct RunReader<T> {
    r: BufReader<zstd::stream::Decoder<'static, BufReader<File>>>,
    buf: Vec<u8>,
    _p: std::marker::PhantomData<T>,
}

impl<T: Rec> RunReader<T> {
    fn open(path: &Path) -> Result<Self> {
        let f = File::open(path)?;
        Ok(RunReader {
            r: BufReader::with_capacity(1 << 18, zstd::stream::Decoder::new(f)?),
            buf: vec![0u8; T::SIZE],
            _p: std::marker::PhantomData,
        })
    }

    fn next(&mut self) -> Result<Option<T>> {
        match self.r.read_exact(&mut self.buf) {
            Ok(()) => Ok(Some(T::get(&self.buf))),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

pub struct ExtSorter<T: Rec> {
    buf: Vec<T>,
    cap: usize,
    dedup: bool,
    dir: PathBuf,
    tag: &'static str,
    runs: Vec<PathBuf>,
    pub spilled_bytes: u64,
}

impl<T: Rec> ExtSorter<T> {
    /// `mem_bytes`: buffer budget; `dedup`: drop equal records.
    pub fn new(dir: &Path, tag: &'static str, mem_bytes: usize, dedup: bool) -> Self {
        let cap = (mem_bytes / std::mem::size_of::<T>().max(1)).max(1024);
        ExtSorter { buf: Vec::new(), cap, dedup, dir: dir.to_path_buf(), tag, runs: Vec::new(), spilled_bytes: 0 }
    }

    #[inline]
    pub fn push(&mut self, v: T) -> Result<()> {
        self.buf.push(v);
        if self.buf.len() >= self.cap {
            self.spill()?;
        }
        Ok(())
    }

    pub fn extend(&mut self, it: impl IntoIterator<Item = T>) -> Result<()> {
        for v in it {
            self.push(v)?;
        }
        Ok(())
    }

    fn sort_buf(&mut self) {
        self.buf.par_sort_unstable();
        if self.dedup {
            self.buf.dedup();
        }
    }

    fn spill(&mut self) -> Result<()> {
        self.sort_buf();
        // several runs written in parallel (each sorted; the merge handles any number)
        let parts: Vec<&[T]> = self.buf.chunks(self.buf.len().div_ceil(8).max(1)).collect();
        let paths: Vec<PathBuf> = parts.iter().map(|_| tmp_path(&self.dir, self.tag)).collect();
        parts.par_iter().zip(&paths).try_for_each(|(p, path)| write_run(path, p))?;
        for p in &paths {
            self.spilled_bytes += std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        }
        self.runs.extend(paths);
        self.buf = Vec::new();
        Ok(())
    }

    pub fn spilled(&self) -> bool {
        !self.runs.is_empty()
    }

    /// Finishes input and returns the records in sorted order.
    pub fn finish(mut self) -> Result<Sorted<T>> {
        if self.runs.is_empty() {
            self.sort_buf();
            let buf = std::mem::take(&mut self.buf);
            return Ok(Sorted { mem: buf.into_iter(), runs: Vec::new(), heap: BinaryHeap::new(), paths: Vec::new(), last: None, dedup: self.dedup });
        }
        if !self.buf.is_empty() {
            self.spill()?;
        }
        let mut runs = Vec::with_capacity(self.runs.len());
        let mut heap = BinaryHeap::new();
        for (i, p) in self.runs.iter().enumerate() {
            let mut r = RunReader::<T>::open(p)?;
            if let Some(v) = r.next()? {
                heap.push(Reverse((v, i)));
            }
            runs.push(r);
        }
        Ok(Sorted {
            mem: Vec::new().into_iter(),
            runs,
            heap,
            paths: std::mem::take(&mut self.runs),
            last: None,
            dedup: self.dedup,
        })
    }
}

impl<T: Rec> Drop for ExtSorter<T> {
    fn drop(&mut self) {
        for p in &self.runs {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Sorted output of an [`ExtSorter`].
pub struct Sorted<T: Rec> {
    mem: std::vec::IntoIter<T>,
    runs: Vec<RunReader<T>>,
    heap: BinaryHeap<Reverse<(T, usize)>>,
    paths: Vec<PathBuf>,
    last: Option<T>,
    dedup: bool,
}

impl<T: Rec> Sorted<T> {
    pub fn next_rec(&mut self) -> Result<Option<T>> {
        if self.runs.is_empty() {
            return Ok(self.mem.next());
        }
        loop {
            let Some(Reverse((v, i))) = self.heap.pop() else { return Ok(None) };
            if let Some(n) = self.runs[i].next()? {
                self.heap.push(Reverse((n, i)));
            }
            if self.dedup && self.last == Some(v) {
                continue;
            }
            self.last = Some(v);
            return Ok(Some(v));
        }
    }
}

impl<T: Rec> Drop for Sorted<T> {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_and_merge_matches_in_memory() {
        let dir = std::env::temp_dir();
        let mut x = 0x1234_5678u64;
        let mut data = Vec::new();
        for _ in 0..300_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            data.push(((x % 50_000) as i64, (x >> 40) as i64 % 7));
        }
        let mut a = ExtSorter::<(i64, i64)>::new(&dir, "t", 1 << 20, true);
        a.extend(data.iter().copied()).unwrap();
        assert!(a.spilled());
        let mut s = a.finish().unwrap();
        let mut got = Vec::new();
        while let Some(v) = s.next_rec().unwrap() {
            got.push(v);
        }
        data.sort_unstable();
        data.dedup();
        assert_eq!(got, data);
    }
}
