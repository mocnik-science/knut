//! Node histories of all nodes referenced by ways or relations, needed for way/relation
//! boxes and private node time series.
//!
//! **Shared** nodes keep only time and position in uncompressed memory-mapped columns
//! (random lookups without decoding; pages live in the reclaimable OS page cache).
//! **Private** nodes keep full metadata in compressed chunks with an LRU cache (they are
//! looked up with good locality: the nodes of one way).

use crate::chunked::{ChunkFile, ChunkWriter};
use crate::rawcols::{ColMap, ColWriter, IdCol};
use crate::{Result, err};
use knut_format::codec::{Col, put_u64_column, unzigzag, zigzag};
use knut_format::{BBox, PrivEntry};
use std::path::Path;

pub const CHUNK_NODES: usize = 1024;
const INVISIBLE: i32 = i32::MIN;

/// One node version as recorded in the store (metadata is zero for shared nodes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreEntry {
    pub t: i64,
    pub lon: i32,
    pub lat: i32,
    pub visible: bool,
    pub version: u32,
    pub changeset: u64,
    pub uid: u32,
}

/// History of one node.
#[derive(Clone, Debug)]
pub struct NodeHist {
    pub private: bool,
    pub entries: Vec<StoreEntry>,
}

impl NodeHist {
    /// Effective (monotonic) times of the entries.
    pub fn eff_times(&self) -> Vec<i64> {
        let mut m = i64::MIN;
        self.entries
            .iter()
            .map(|e| {
                m = m.max(e.t);
                m
            })
            .collect()
    }

    /// `(start, end, box)` validity intervals (box empty when deleted); `end` inclusive.
    pub fn intervals(&self) -> Vec<(i64, i64, BBox)> {
        let eff = self.eff_times();
        self.entries
            .iter()
            .enumerate()
            .map(|(k, e)| {
                let end = eff.get(k + 1).copied().unwrap_or(i64::MAX);
                let bb = if e.visible { BBox::point(e.lon, e.lat) } else { BBox::EMPTY };
                (eff[k], end.saturating_sub(1), bb)
            })
            .collect()
    }
}

pub fn priv_entry(e: &StoreEntry, continuation: bool) -> PrivEntry {
    PrivEntry {
        timestamp: e.t,
        lon: if e.visible { e.lon } else { 0 },
        lat: if e.visible { e.lat } else { 0 },
        visible: e.visible,
        continuation,
        version: e.version,
        changeset: e.changeset,
        uid: e.uid,
    }
}

#[derive(Default)]
struct ChunkBuf {
    ids: Vec<i64>,
    counts: Vec<u64>,
    entries: Vec<StoreEntry>,
}

impl ChunkBuf {
    fn encode(&self, with_meta: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev = 0i64;
        let ids: Vec<u64> = self
            .ids
            .iter()
            .map(|&i| {
                let d = zigzag(i - prev);
                prev = i;
                d
            })
            .collect();
        put_u64_column(&mut out, &ids);
        put_u64_column(&mut out, &self.counts);
        let (mut pt, mut px, mut py, mut pc) = (0i64, 0i64, 0i64, 0i64);
        let mut cols: [Vec<u64>; 7] = Default::default();
        for e in &self.entries {
            cols[0].push(zigzag(e.t - pt));
            pt = e.t;
            cols[1].push(zigzag(e.lon as i64 - px));
            cols[2].push(zigzag(e.lat as i64 - py));
            px = e.lon as i64;
            py = e.lat as i64;
            cols[3].push(e.visible as u64);
            if with_meta {
                cols[4].push(e.version as u64);
                cols[5].push(zigzag(e.changeset as i64 - pc));
                pc = e.changeset as i64;
                cols[6].push(e.uid as u64);
            }
        }
        for c in &cols[..if with_meta { 7 } else { 4 }] {
            put_u64_column(&mut out, c);
        }
        out
    }
}

/// Decoded chunk of private nodes (full metadata).
pub struct PrivateChunk {
    ids: Vec<i64>,
    start: Vec<u32>,
    entries: Vec<StoreEntry>,
}

fn decode_common(raw: &[u8], ncols: usize) -> Result<(Vec<i64>, Vec<u32>, Vec<Col>)> {
    let mut p = 0;
    let mut col = |name| Col::read(raw, &mut p, name).map_err(crate::BuildError::from);
    let mut ids_c = col("ids")?;
    let mut counts = col("counts")?;
    let cols: Vec<Col> = (0..ncols).map(|_| col("entries")).collect::<Result<_>>()?;
    let n = ids_c.len();
    let (mut ids, mut start) = (Vec::with_capacity(n), Vec::with_capacity(n + 1));
    let (mut prev, mut acc) = (0i64, 0u32);
    for _ in 0..n {
        prev += ids_c.next_i()?;
        ids.push(prev);
        start.push(acc);
        acc += counts.next()? as u32;
    }
    start.push(acc);
    Ok((ids, start, cols))
}

fn decode_private(raw: &[u8]) -> Result<PrivateChunk> {
    let (ids, start, mut c) = decode_common(raw, 7)?;
    let n = *start.last().unwrap() as usize;
    let mut entries = Vec::with_capacity(n);
    let (mut pt, mut px, mut py, mut pc) = (0i64, 0i64, 0i64, 0i64);
    for _ in 0..n {
        pt += c[0].next_i()?;
        px += c[1].next_i()?;
        py += c[2].next_i()?;
        let visible = c[3].next()? != 0;
        let version = c[4].next()? as u32;
        pc += unzigzag(c[5].next()?);
        entries.push(StoreEntry {
            t: pt,
            lon: px as i32,
            lat: py as i32,
            visible,
            version,
            changeset: pc as u64,
            uid: c[6].next()? as u32,
        });
    }
    Ok(PrivateChunk { ids, start, entries })
}

/// Builds the store during the node pass (ids must be pushed in increasing order).
pub struct NodeStoreWriter {
    shared_ids: ColWriter<i64>,
    shared_start: ColWriter<u64>,
    shared_t: ColWriter<i64>,
    shared_pos: ColWriter<[i32; 2]>,
    private: ChunkWriter,
    cur_private: ChunkBuf,
    stats: [(u64, u64, u64); 2], // (nodes, entries, chunks) shared/private
    last_id: Option<i64>,
}

impl NodeStoreWriter {
    pub fn new(dir: &Path) -> Result<Self> {
        Ok(NodeStoreWriter {
            shared_ids: ColWriter::new(dir, "shared-ids")?,
            shared_start: ColWriter::new(dir, "shared-start")?,
            shared_t: ColWriter::new(dir, "shared-t")?,
            shared_pos: ColWriter::new(dir, "shared-pos")?,
            private: ChunkWriter::new(dir, "nodes-private")?,
            cur_private: ChunkBuf::default(),
            stats: [(0, 0, 0); 2],
            last_id: None,
        })
    }

    pub fn push(&mut self, id: i64, private: bool, entries: impl IntoIterator<Item = StoreEntry>) -> Result<()> {
        if self.last_id.is_some_and(|l| l >= id) {
            return Err(err("node store: ids not increasing"));
        }
        self.last_id = Some(id);
        if !private {
            self.shared_ids.push(id)?;
            self.shared_start.push(self.stats[0].1)?;
            for e in entries {
                self.shared_t.push(e.t)?;
                self.shared_pos.push(if e.visible { [e.lon, e.lat] } else { [INVISIBLE, 0] })?;
                self.stats[0].1 += 1;
            }
            self.stats[0].0 += 1;
            return Ok(());
        }
        let buf = &mut self.cur_private;
        let before = buf.entries.len();
        buf.entries.extend(entries);
        let n = buf.entries.len() - before;
        buf.ids.push(id);
        buf.counts.push(n as u64);
        self.stats[1].0 += 1;
        self.stats[1].1 += n as u64;
        if buf.ids.len() == CHUNK_NODES {
            let b = std::mem::take(buf);
            self.stats[1].2 += 1;
            self.private.add(b.ids[0], b.encode(true))?;
        }
        Ok(())
    }

    /// `cache_bytes`: memory for decoded private-node chunks.
    pub fn finish(mut self, cache_bytes: usize) -> Result<NodeStore> {
        let b = std::mem::take(&mut self.cur_private);
        if !b.ids.is_empty() {
            self.stats[1].2 += 1;
            self.private.add(b.ids[0], b.encode(true))?;
        }
        self.shared_start.push(self.stats[0].1)?;
        let (n, e, c) = self.stats[1];
        let private_chunk = ((n * 12 + e * 40) / c.max(1)) as usize;
        Ok(NodeStore {
            shared_ids: IdCol::new(self.shared_ids.finish()?),
            shared_start: self.shared_start.finish()?,
            shared_t: self.shared_t.finish()?,
            shared_pos: self.shared_pos.finish()?,
            private: self.private.finish(decode_private, cache_bytes, private_chunk)?,
            n_nodes: self.stats[0].0 + self.stats[1].0,
            n_entries: self.stats[0].1 + self.stats[1].1,
            n_private: self.stats[1].0,
        })
    }
}

/// Read-only node history store.
pub struct NodeStore {
    shared_ids: IdCol,
    shared_start: ColMap<u64>,
    shared_t: ColMap<i64>,
    shared_pos: ColMap<[i32; 2]>,
    private: ChunkFile<PrivateChunk>,
    pub n_nodes: u64,
    pub n_entries: u64,
    pub n_private: u64,
}

impl NodeStore {
    /// Temporary disk use: raw shared columns plus compressed private chunks.
    pub fn disk_bytes(&self) -> u64 {
        self.shared_ids.bytes()
            + self.shared_start.bytes()
            + self.shared_t.bytes()
            + self.shared_pos.bytes()
            + self.private.disk_bytes
    }

    /// History of node `id`, if it is referenced by a way or relation and exists.
    pub fn get(&self, id: i64) -> Option<NodeHist> {
        if let Some(k) = self.shared_ids.find(id) {
            let st = self.shared_start.as_slice();
            let (t, pos) = (self.shared_t.as_slice(), self.shared_pos.as_slice());
            let entries = (st[k] as usize..st[k + 1] as usize)
                .map(|e| {
                    let [lon, lat] = pos[e];
                    StoreEntry {
                        t: t[e],
                        lon: if lon == INVISIBLE { 0 } else { lon },
                        lat,
                        visible: lon != INVISIBLE,
                        version: 0,
                        changeset: 0,
                        uid: 0,
                    }
                })
                .collect();
            return Some(NodeHist { private: false, entries });
        }
        let c = self.private.chunk_for(id).expect("node store chunk decodes")?;
        let k = c.ids.binary_search(&id).ok()?;
        Some(NodeHist { private: true, entries: c.entries[c.start[k] as usize..c.start[k + 1] as usize].to_vec() })
    }
}
