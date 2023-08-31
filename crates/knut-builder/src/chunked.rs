//! Generic id-keyed chunk file: chunks (sorted by first id) are zstd-compressed, appended
//! to a temporary file and read back through a memory map and a sharded LRU cache of
//! decoded chunks. Used for node histories and way summaries.

use crate::extsort::tmp_path;
use crate::{Result, err};
use lru::LruCache;
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SHARDS: usize = 64;
const PENDING: usize = 256;

pub struct ChunkWriter {
    path: PathBuf,
    file: BufWriter<File>,
    offset: u64,
    index: Vec<(i64, u64, u32)>,
    pending: Vec<(i64, Vec<u8>)>,
    raw_bytes: u64,
    done: bool,
}

impl Drop for ChunkWriter {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl ChunkWriter {
    pub fn new(dir: &Path, tag: &str) -> Result<Self> {
        let path = tmp_path(dir, tag);
        let f = File::create(&path).map_err(|e| err(format!("create {}: {e}", path.display())))?;
        Ok(ChunkWriter {
            path,
            file: BufWriter::with_capacity(4 << 20, f),
            offset: 0,
            index: Vec::new(),
            pending: Vec::new(),
            raw_bytes: 0,
            done: false,
        })
    }

    /// Adds an encoded (uncompressed) chunk whose smallest id is `first_id`.
    pub fn add(&mut self, first_id: i64, raw: Vec<u8>) -> Result<()> {
        self.raw_bytes += raw.len() as u64;
        self.pending.push((first_id, raw));
        if self.pending.len() >= PENDING {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let comp: Vec<Result<Vec<u8>>> =
            self.pending.par_iter().map(|(_, raw)| Ok(zstd::bulk::compress(raw, 1)?)).collect();
        for ((first, _), c) in self.pending.iter().zip(comp) {
            let c = c?;
            self.file.write_all(&c)?;
            self.index.push((*first, self.offset, c.len() as u32));
            self.offset += c.len() as u64;
        }
        self.pending.clear();
        Ok(())
    }

    /// Finishes writing. `cache_bytes / decoded_bytes_per_chunk` chunks are cached.
    pub fn finish<T: Send + Sync + 'static>(
        mut self,
        decode: fn(&[u8]) -> Result<T>,
        cache_bytes: usize,
        decoded_bytes_per_chunk: usize,
    ) -> Result<ChunkFile<T>> {
        self.flush()?;
        self.file.flush()?;
        self.done = true;
        let mmap = if self.offset > 0 {
            let f = File::open(&self.path)?;
            // SAFETY: private temporary file, not modified after this point.
            Some(unsafe { Mmap::map(&f) }.map_err(|e| err(format!("mmap {}: {e}", self.path.display())))?)
        } else {
            None
        };
        let per = (cache_bytes / decoded_bytes_per_chunk.max(1) / SHARDS).max(4);
        let per = NonZeroUsize::new(per).unwrap();
        Ok(ChunkFile {
            path: self.path.clone(),
            mmap,
            index: std::mem::take(&mut self.index),
            cache: (0..SHARDS).map(|_| Mutex::new(LruCache::new(per))).collect(),
            decode,
            disk_bytes: self.offset,
            raw_bytes: self.raw_bytes,
        })
    }
}

pub struct ChunkFile<T> {
    path: PathBuf,
    mmap: Option<Mmap>,
    index: Vec<(i64, u64, u32)>,
    cache: Vec<Mutex<LruCache<u32, Arc<T>>>>,
    decode: fn(&[u8]) -> Result<T>,
    pub disk_bytes: u64,
    pub raw_bytes: u64,
}

impl<T> ChunkFile<T> {
    /// The chunk that may contain `id` (the last chunk with `first_id <= id`).
    pub fn chunk_for(&self, id: i64) -> Result<Option<Arc<T>>> {
        let Some(ci) = self.index.partition_point(|e| e.0 <= id).checked_sub(1) else { return Ok(None) };
        let shard = &self.cache[ci % SHARDS];
        if let Some(c) = shard.lock().unwrap().get(&(ci as u32)) {
            return Ok(Some(c.clone()));
        }
        let (_, off, len) = self.index[ci];
        let m = self.mmap.as_ref().ok_or_else(|| err("chunk file empty"))?;
        let raw = zstd::bulk::decompress(&m[off as usize..off as usize + len as usize], 256 << 20)?;
        let c = Arc::new((self.decode)(&raw)?);
        shard.lock().unwrap().put(ci as u32, c.clone());
        Ok(Some(c))
    }

    pub fn n_chunks(&self) -> usize {
        self.index.len()
    }
}

impl<T> Drop for ChunkFile<T> {
    fn drop(&mut self) {
        self.mmap = None;
        let _ = std::fs::remove_file(&self.path);
    }
}
