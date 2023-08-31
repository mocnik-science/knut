//! Uncompressed, memory-mapped column files for randomly accessed build data (shared node
//! positions, way summaries). Pages live in the OS page cache, which is reclaimable and
//! does not count against the build's memory budget.

use crate::extsort::tmp_path;
use crate::{Result, err};
use bytemuck::Pod;
use memmap2::Mmap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

pub struct ColWriter<T: Pod> {
    path: PathBuf,
    w: BufWriter<File>,
    len: usize,
    done: bool,
    _t: PhantomData<T>,
}

impl<T: Pod> ColWriter<T> {
    pub fn new(dir: &Path, tag: &str) -> Result<Self> {
        let path = tmp_path(dir, tag);
        let f = File::create(&path).map_err(|e| err(format!("create {}: {e}", path.display())))?;
        Ok(ColWriter { path, w: BufWriter::with_capacity(1 << 20, f), len: 0, done: false, _t: PhantomData })
    }

    #[inline]
    pub fn push(&mut self, v: T) -> Result<()> {
        self.w.write_all(bytemuck::bytes_of(&v))?;
        self.len += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn finish(mut self) -> Result<ColMap<T>> {
        self.w.flush()?;
        self.done = true;
        let mmap = if self.len > 0 {
            let f = File::open(&self.path)?;
            // SAFETY: private temporary file, not modified after this point.
            Some(unsafe { Mmap::map(&f) }.map_err(|e| err(format!("mmap {}: {e}", self.path.display())))?)
        } else {
            None
        };
        Ok(ColMap { path: self.path.clone(), mmap, len: self.len, _t: PhantomData })
    }
}

impl<T: Pod> Drop for ColWriter<T> {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct ColMap<T: Pod> {
    path: PathBuf,
    mmap: Option<Mmap>,
    len: usize,
    _t: PhantomData<T>,
}

impl<T: Pod> ColMap<T> {
    /// The column as a slice (mmap is page aligned, so any `Pod` alignment holds).
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        match &self.mmap {
            Some(m) => bytemuck::cast_slice(&m[..self.len * std::mem::size_of::<T>()]),
            None => &[],
        }
    }

    pub fn bytes(&self) -> u64 {
        (self.len * std::mem::size_of::<T>()) as u64
    }
}

impl<T: Pod> Drop for ColMap<T> {
    fn drop(&mut self) {
        self.mmap = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sorted id column with a sparse in-memory index for fast lookups.
pub struct IdCol {
    ids: ColMap<i64>,
    sparse: Vec<i64>,
}

const SPARSE: usize = 256;

impl IdCol {
    pub fn new(ids: ColMap<i64>) -> IdCol {
        let sparse = ids.as_slice().iter().step_by(SPARSE).copied().collect();
        IdCol { ids, sparse }
    }

    /// Position of `id`, if present.
    #[inline]
    pub fn find(&self, id: i64) -> Option<usize> {
        let b = self.sparse.partition_point(|&x| x <= id).checked_sub(1)?;
        let lo = b * SPARSE;
        let s = self.ids.as_slice();
        let hi = (lo + SPARSE).min(s.len());
        s[lo..hi].binary_search(&id).ok().map(|k| lo + k)
    }

    pub fn bytes(&self) -> u64 {
        self.ids.bytes()
    }
}
