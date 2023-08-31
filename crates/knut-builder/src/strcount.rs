//! Exact string frequency counting with bounded memory: the in-memory map is spilled as
//! string-sorted zstd runs; `finish` merges the runs and keeps strings above a threshold.

use crate::extsort::tmp_path;
use crate::{Result, err};
use knut_format::Str;
use knut_format::codec::put_varint;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub struct SpillingCounter {
    dir: PathBuf,
    max_entries: usize,
    runs: Vec<PathBuf>,
    pub spilled_bytes: u64,
}

impl SpillingCounter {
    pub fn new(dir: &Path, mem_bytes: usize) -> Self {
        // ~96 bytes per map entry (Arc<str> + count + table overhead + string)
        SpillingCounter { dir: dir.to_path_buf(), max_entries: (mem_bytes / 96).max(4096), runs: Vec::new(), spilled_bytes: 0 }
    }

    /// Spills `map` if it exceeds the budget (leaves it empty then).
    pub fn maybe_spill(&mut self, map: &mut HashMap<Str, u64>) -> Result<()> {
        if map.len() > self.max_entries {
            self.spill(map)?;
        }
        Ok(())
    }

    fn spill(&mut self, map: &mut HashMap<Str, u64>) -> Result<()> {
        let mut v: Vec<(Str, u64)> = map.drain().collect();
        v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let path = tmp_path(&self.dir, "strcount");
        let f = File::create(&path)?;
        let mut z = zstd::stream::Encoder::new(BufWriter::with_capacity(1 << 20, f), 1)?;
        let mut buf = Vec::new();
        for (s, c) in v {
            buf.clear();
            put_varint(&mut buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
            put_varint(&mut buf, c);
            z.write_all(&buf)?;
        }
        z.finish()?.flush()?;
        self.spilled_bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.runs.push(path);
        *map = HashMap::new();
        Ok(())
    }

    /// Exact totals of all strings with count >= `min`.
    pub fn finish(mut self, mut map: HashMap<Str, u64>, min: u64) -> Result<HashMap<Str, u64>> {
        if self.runs.is_empty() {
            map.retain(|_, c| *c >= min);
            return Ok(map);
        }
        if !map.is_empty() {
            self.spill(&mut map)?;
        }
        let mut readers: Vec<RunReader> = self.runs.iter().map(|p| RunReader::open(p)).collect::<Result<_>>()?;
        let mut heap = BinaryHeap::new();
        for (i, r) in readers.iter_mut().enumerate() {
            if let Some((s, c)) = r.next()? {
                heap.push(std::cmp::Reverse((s, c, i)));
            }
        }
        let mut out = HashMap::new();
        let mut cur: Option<(String, u64)> = None;
        while let Some(std::cmp::Reverse((s, c, i))) = heap.pop() {
            if let Some(n) = readers[i].next()? {
                heap.push(std::cmp::Reverse((n.0, n.1, i)));
            }
            match &mut cur {
                Some((cs, cc)) if *cs == s => *cc += c,
                _ => {
                    if let Some((cs, cc)) = cur.take() {
                        if cc >= min {
                            out.insert(Str::from(cs), cc);
                        }
                    }
                    cur = Some((s, c));
                }
            }
        }
        if let Some((cs, cc)) = cur {
            if cc >= min {
                out.insert(Str::from(cs), cc);
            }
        }
        Ok(out)
    }
}

impl Drop for SpillingCounter {
    fn drop(&mut self) {
        for p in &self.runs {
            let _ = std::fs::remove_file(p);
        }
    }
}

struct RunReader {
    r: BufReader<zstd::stream::Decoder<'static, BufReader<File>>>,
}

impl RunReader {
    fn open(p: &Path) -> Result<Self> {
        Ok(RunReader { r: BufReader::with_capacity(1 << 18, zstd::stream::Decoder::new(File::open(p)?)?) })
    }

    fn varint(&mut self) -> Result<Option<u64>> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let mut b = [0u8; 1];
            match self.r.read_exact(&mut b) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && shift == 0 => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            v |= ((b[0] & 0x7f) as u64) << shift;
            if b[0] & 0x80 == 0 {
                return Ok(Some(v));
            }
            shift += 7;
            if shift > 63 {
                return Err(err("strcount: bad varint"));
            }
        }
    }

    fn next(&mut self) -> Result<Option<(String, u64)>> {
        let Some(len) = self.varint()? else { return Ok(None) };
        let mut b = vec![0u8; len as usize];
        self.r.read_exact(&mut b)?;
        let c = self.varint()?.ok_or_else(|| err("strcount: truncated run"))?;
        Ok(Some((String::from_utf8(b).map_err(|_| err("strcount: utf-8"))?, c)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_counts_are_exact() {
        let dir = std::env::temp_dir();
        let mut sc = SpillingCounter::new(&dir, 0);
        let mut map: HashMap<Str, u64> = HashMap::new();
        let mut want: HashMap<String, u64> = HashMap::new();
        for i in 0..50_000u64 {
            let s = format!("v{}", (i * 7919) % 6000);
            *map.entry(Str::from(s.as_str())).or_default() += 1;
            *want.entry(s).or_default() += 1;
            sc.maybe_spill(&mut map).unwrap();
        }
        let got = sc.finish(map, 9).unwrap();
        let want: HashMap<Str, u64> =
            want.into_iter().filter(|(_, c)| *c >= 9).map(|(s, c)| (Str::from(s.as_str()), c)).collect();
        assert_eq!(got, want);
    }
}
