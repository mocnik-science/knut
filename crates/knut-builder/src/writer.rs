//! Final build stage: sorting, block formation, zstd dictionaries, R-trees, ID index and
//! file writing (SPEC.md §4–§10).

use crate::{BuildOptions, BuildStats, Collected, Result, err, peak_footprint_bytes, peak_rss_bytes};
use knut_format::block::{EncodeDicts, EncodeOptions, RawBlock, encode_block, frame_block};
use knut_format::bloom::{self, Bloom};
use knut_format::codec::{put_varint, zigzag};
use knut_format::layout::*;
use knut_format::strings::encode_strings;
use knut_format::*;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use crate::extsort::{ExtSorter, Sorted, tmp_path};
use std::time::Instant;
use xxhash_rust::xxh3::{Xxh3, xxh3_64, xxh3_64_with_seed};

const CHUNK_BYTES: usize = 256 << 20;
const MAX_SEGMENTS_PER_BLOCK: usize = 1 << 16;
const DICT_SAMPLE_BLOCKS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SegKey {
    pub section: u8,
    pub hilbert: u64,
    pub id: i64,
    pub seg_no: u32,
}

struct Entry {
    key: SegKey,
    bbox: BBox,
    chunk: u32,
    off: u32,
    len: u32,
}

/// One serialized segment.
pub struct Row {
    pub key: SegKey,
    pub bbox: BBox,
    pub bytes: Vec<u8>,
}

const ROW_HEADER: usize = 1 + 8 + 8 + 4 + 16 + 4;
const SAMPLE_EVERY: u64 = 32;
const SAMPLE_CAP: usize = 24 << 20;

/// Store of serialized segments (row format, postcard). Rows are buffered in memory and
/// spilled as sorted, zstd-compressed runs when the buffer exceeds the budget.
pub struct Sink {
    chunks: Vec<Vec<u8>>,
    entries: Vec<Entry>,
    bytes: usize,
    budget: usize,
    tmp: PathBuf,
    runs: Vec<PathBuf>,
    pub spilled_bytes: u64,
    pub n_rows: u64,
    pub total_bytes: u64,
    /// Deterministic sample of rows per object type (zstd dictionary training).
    samples: [Vec<Row>; 3],
    sample_bytes: [usize; 3],
}

impl Sink {
    pub fn new(tmp: &Path, budget: usize, _block_size: usize) -> Sink {
        Sink {
            chunks: Vec::new(),
            entries: Vec::new(),
            bytes: 0,
            budget: budget.max(1 << 20),
            tmp: tmp.to_path_buf(),
            runs: Vec::new(),
            spilled_bytes: 0,
            n_rows: 0,
            total_bytes: 0,
            samples: Default::default(),
            sample_bytes: [0; 3],
        }
    }

    pub fn push(&mut self, key: SegKey, bbox: BBox, bytes: &[u8]) -> Result<()> {
        let t = (key.section % 3) as usize;
        let h = xxh3_64_with_seed(&[key.id.to_le_bytes(), (key.seg_no as i64).to_le_bytes()].concat(), 7);
        if h % SAMPLE_EVERY == 0 && self.sample_bytes[t] < SAMPLE_CAP {
            self.sample_bytes[t] += bytes.len();
            self.samples[t].push(Row { key, bbox, bytes: bytes.to_vec() });
        }
        if self.chunks.last().is_none_or(|c| c.len() + bytes.len() > CHUNK_BYTES) {
            self.chunks.push(Vec::with_capacity(CHUNK_BYTES.max(bytes.len())));
        }
        let ci = self.chunks.len() - 1;
        let c = &mut self.chunks[ci];
        self.entries.push(Entry { key, bbox, chunk: ci as u32, off: c.len() as u32, len: bytes.len() as u32 });
        c.extend_from_slice(bytes);
        self.bytes += bytes.len() + std::mem::size_of::<Entry>();
        self.n_rows += 1;
        self.total_bytes += bytes.len() as u64;
        if self.bytes > self.budget {
            self.spill()?;
        }
        Ok(())
    }

    fn get(&self, e: &Entry) -> &[u8] {
        &self.chunks[e.chunk as usize][e.off as usize..(e.off + e.len) as usize]
    }

    fn spill(&mut self) -> Result<()> {
        self.entries.par_sort_unstable_by_key(|e| e.key);
        let parts: Vec<&[Entry]> = self.entries.chunks(self.entries.len().div_ceil(8).max(1)).collect();
        let paths: Vec<PathBuf> = parts.iter().map(|_| tmp_path(&self.tmp, "rows")).collect();
        parts.par_iter().zip(&paths).try_for_each(|(part, path)| -> Result<()> {
            let f = File::create(path)?;
            let mut z = zstd::stream::Encoder::new(BufWriter::with_capacity(1 << 20, f), 1)?;
            let mut hdr = Vec::with_capacity(ROW_HEADER);
            for e in part.iter() {
                hdr.clear();
                hdr.push(e.key.section);
                hdr.extend_from_slice(&e.key.hilbert.to_le_bytes());
                hdr.extend_from_slice(&e.key.id.to_le_bytes());
                hdr.extend_from_slice(&e.key.seg_no.to_le_bytes());
                for v in [e.bbox.minx, e.bbox.miny, e.bbox.maxx, e.bbox.maxy] {
                    hdr.extend_from_slice(&v.to_le_bytes());
                }
                hdr.extend_from_slice(&e.len.to_le_bytes());
                z.write_all(&hdr)?;
                z.write_all(self.get(e))?;
            }
            z.finish()?.flush()?;
            Ok(())
        })?;
        for p in &paths {
            self.spilled_bytes += std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        }
        self.runs.extend(paths);
        self.chunks = Vec::new();
        self.entries = Vec::new();
        self.bytes = 0;
        Ok(())
    }

    /// Rows in key order (in memory, or merged from the spilled runs).
    fn into_stream(mut self) -> Result<RowStream> {
        if self.runs.is_empty() {
            self.entries.par_sort_unstable_by_key(|e| e.key);
            return Ok(RowStream::Mem { chunks: std::mem::take(&mut self.chunks), entries: std::mem::take(&mut self.entries), pos: 0 });
        }
        if !self.entries.is_empty() {
            self.spill()?;
        }
        let mut readers = Vec::new();
        let mut heap = std::collections::BinaryHeap::new();
        let mut pending = Vec::new();
        for (i, p) in self.runs.iter().enumerate() {
            let f = File::open(p)?;
            let mut r = std::io::BufReader::with_capacity(1 << 18, zstd::stream::Decoder::new(f)?);
            let row = read_row(&mut r)?;
            if let Some(row) = &row {
                heap.push(std::cmp::Reverse((row.key, i)));
            }
            pending.push(row);
            readers.push(r);
        }
        Ok(RowStream::Merge { readers, heap, pending, paths: std::mem::take(&mut self.runs) })
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        for p in &self.runs {
            let _ = std::fs::remove_file(p);
        }
    }
}

type RunRead = std::io::BufReader<zstd::stream::Decoder<'static, std::io::BufReader<File>>>;

fn read_row(r: &mut RunRead) -> Result<Option<Row>> {
    use std::io::Read;
    let mut h = [0u8; ROW_HEADER];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let u64at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
    let i32at = |o: usize| i32::from_le_bytes(h[o..o + 4].try_into().unwrap());
    let key = SegKey {
        section: h[0],
        hilbert: u64at(1),
        id: u64at(9) as i64,
        seg_no: u32::from_le_bytes(h[17..21].try_into().unwrap()),
    };
    let bbox = BBox { minx: i32at(21), miny: i32at(25), maxx: i32at(29), maxy: i32at(33) };
    let len = u32::from_le_bytes(h[37..41].try_into().unwrap()) as usize;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    Ok(Some(Row { key, bbox, bytes }))
}

enum RowStream {
    Mem { chunks: Vec<Vec<u8>>, entries: Vec<Entry>, pos: usize },
    Merge {
        readers: Vec<RunRead>,
        heap: std::collections::BinaryHeap<std::cmp::Reverse<(SegKey, usize)>>,
        pending: Vec<Option<Row>>,
        paths: Vec<PathBuf>,
    },
}

impl RowStream {
    fn next(&mut self) -> Result<Option<Row>> {
        match self {
            RowStream::Mem { chunks, entries, pos } => {
                let Some(e) = entries.get(*pos) else { return Ok(None) };
                *pos += 1;
                let b = chunks[e.chunk as usize][e.off as usize..(e.off + e.len) as usize].to_vec();
                Ok(Some(Row { key: e.key, bbox: e.bbox, bytes: b }))
            }
            RowStream::Merge { readers, heap, pending, .. } => {
                let Some(std::cmp::Reverse((_, i))) = heap.pop() else { return Ok(None) };
                let row = pending[i].take();
                pending[i] = read_row(&mut readers[i])?;
                if let Some(n) = &pending[i] {
                    heap.push(std::cmp::Reverse((n.key, i)));
                }
                Ok(row)
            }
        }
    }
}

impl Drop for RowStream {
    fn drop(&mut self) {
        if let RowStream::Merge { paths, .. } = self {
            for p in paths.iter() {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Incremental block formation over key-ordered rows: blocks are cut by section, size,
/// segment count and spatial compactness (an outlier never inflates a block box).
struct BlockFormer<'a> {
    opt: &'a BuildOptions,
    cur: Vec<Row>,
    acc: usize,
    bb: BBox,
}

impl<'a> BlockFormer<'a> {
    fn new(opt: &'a BuildOptions) -> Self {
        BlockFormer { opt, cur: Vec::new(), acc: 0, bb: BBox::EMPTY }
    }

    fn compact(&self, s: &BBox) -> bool {
        if self.bb.is_empty() || s.is_empty() {
            return true;
        }
        let u = self.bb.unioned(s);
        u.size_m() <= (self.opt.seg.grow * self.bb.size_m()).max(self.opt.block_min_extent_m)
            || u.size_m() <= self.bb.size_m() * 1.01
    }

    fn push(&mut self, r: Row) -> Option<Vec<Row>> {
        let mut done = None;
        if let Some(first) = self.cur.first()
            && (r.key.section != first.key.section
                || self.acc + r.bytes.len() > self.opt.block_size
                || self.cur.len() >= MAX_SEGMENTS_PER_BLOCK
                || (r.key.section < 3 && !self.compact(&r.bbox)))
        {
            done = Some(std::mem::take(&mut self.cur));
            self.acc = 0;
            self.bb = BBox::EMPTY;
        }
        self.acc += r.bytes.len();
        self.bb.union(&r.bbox);
        self.cur.push(r);
        done
    }

    fn finish(&mut self) -> Option<Vec<Row>> {
        (!self.cur.is_empty()).then(|| std::mem::take(&mut self.cur))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RelRowVersion {
    meta: Meta,
    tags: Tags,
    prefix: u32,
    del: u32,
    ins: Vec<Member>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RelRow {
    id: i64,
    flags: u8,
    tmin: u32,
    tmax: u32,
    bbox: BBox,
    versions: Vec<RelRowVersion>,
}

/// Serializes a segment into the internal row format (postcard; relation member lists
/// are diff-coded against the previous version to keep memory small).
pub fn encode_row(s: &Segment) -> Result<Vec<u8>> {
    let e = |e: postcard::Error| err(format!("serialize: {e}"));
    match &s.body {
        SegmentBody::Relation(vs) => {
            let mut prev: &[Member] = &[];
            let versions = vs
                .iter()
                .map(|v| {
                    let (p, d, ins) = knut_format::diff::diff(prev, &v.members);
                    prev = &v.members;
                    RelRowVersion { meta: v.meta.clone(), tags: v.tags.clone(), prefix: p as u32, del: d as u32, ins: ins.to_vec() }
                })
                .collect();
            let row = RelRow { id: s.id, flags: s.flags, tmin: s.tmin, tmax: s.tmax, bbox: s.bbox, versions };
            let mut out = vec![1u8];
            out.extend(postcard::to_allocvec(&row).map_err(e)?);
            Ok(out)
        }
        _ => {
            let mut out = vec![0u8];
            out.extend(postcard::to_allocvec(s).map_err(e)?);
            Ok(out)
        }
    }
}

pub fn decode_row(b: &[u8]) -> Result<Segment> {
    let e = |e: postcard::Error| err(format!("deserialize: {e}"));
    match b.first() {
        Some(0) => postcard::from_bytes(&b[1..]).map_err(e),
        Some(1) => {
            let row: RelRow = postcard::from_bytes(&b[1..]).map_err(e)?;
            let mut out: Vec<RelVersion> = Vec::with_capacity(row.versions.len());
            for v in row.versions {
                let prev = out.last().map_or(&[][..], |p| &p.members[..]);
                let members = knut_format::diff::apply(prev, v.prefix as usize, v.del as usize, v.ins)?;
                out.push(RelVersion { meta: v.meta, tags: v.tags, members });
            }
            Ok(Segment { id: row.id, flags: row.flags, tmin: row.tmin, tmax: row.tmax, bbox: row.bbox, body: SegmentBody::Relation(out) })
        }
        _ => Err(err("bad row tag")),
    }
}

/// Global string dictionaries.
pub struct Dicts {
    keys: Vec<Str>,
    values: Vec<Str>,
    roles: Vec<Str>,
    key_map: HashMap<Str, u32>,
    value_map: HashMap<Str, u32>,
    role_map: HashMap<Str, u32>,
}

impl EncodeDicts for Dicts {
    fn key(&self, s: &str) -> Option<u32> {
        self.key_map.get(s).copied()
    }
    fn value(&self, s: &str) -> Option<u32> {
        self.value_map.get(s).copied()
    }
    fn role(&self, s: &str) -> Option<u32> {
        self.role_map.get(s).copied()
    }
}

fn ranked(counts: HashMap<Str, u64>, min: u64) -> (Vec<Str>, HashMap<Str, u32>) {
    let mut v: Vec<(Str, u64)> = counts.into_iter().filter(|(_, c)| *c >= min).collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let list: Vec<Str> = v.into_iter().map(|(s, _)| s).collect();
    let map = list.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
    (list, map)
}

struct Out {
    w: BufWriter<File>,
    pos: u64,
    sections: Vec<SectionEntry>,
    checksums: Vec<u64>,
    stream: Option<(u32, u64, Xxh3)>,
}

impl Out {
    fn write(&mut self, b: &[u8]) -> Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        if let Some((_, _, h)) = &mut self.stream {
            h.update(b);
        }
        Ok(())
    }

    fn align(&mut self) -> Result<()> {
        let pad = (8 - (self.pos % 8) as usize) % 8;
        self.write(&[0u8; 8][..pad])
    }

    fn section(&mut self, kind: SectionKind, flags: u32, bytes: &[u8]) -> Result<()> {
        self.align()?;
        let offset = self.pos;
        self.write(bytes)?;
        self.sections.push(SectionEntry { kind: kind as u32, flags, offset, length: bytes.len() as u64 });
        self.checksums.push(xxh3_64(bytes));
        Ok(())
    }

    fn zsection(&mut self, kind: SectionKind, raw: &[u8], level: i32) -> Result<()> {
        let c = zstd::bulk::compress(raw, level)?;
        self.section(kind, SEC_FLAG_ZSTD, &c)
    }

    fn begin_stream(&mut self, kind: SectionKind) -> Result<()> {
        self.align()?;
        self.stream = Some((kind as u32, self.pos, Xxh3::new()));
        Ok(())
    }

    fn end_stream(&mut self) {
        let (kind, offset, h) = self.stream.take().expect("stream open");
        self.sections.push(SectionEntry { kind, flags: 0, offset, length: self.pos - offset });
        self.checksums.push(h.digest());
    }
}

struct EncodedBlock {
    bytes: Vec<u8>,
    obj_type: ObjType,
    flags: u8,
    section: u8,
    leaf: RNode,
    bloom: Bloom,
    /// The block's sized filter (BlockBlooms section).
    filter: Vec<u64>,
    ids: Vec<i64>,
    private_nodes: Vec<i64>,
    n_large: u64,
}

fn pseudonym_map(users: &HashMap<(u32, Str), (i64, i64)>, secret: &str) -> HashMap<u32, u32> {
    let seed = xxh3_64(secret.as_bytes());
    let mut uids: Vec<u32> = users.keys().map(|k| k.0).collect();
    uids.sort_unstable();
    uids.dedup();
    let mut hashed: Vec<(u64, u32)> = uids.iter().map(|&u| (xxh3_64_with_seed(&u.to_le_bytes(), seed), u)).collect();
    hashed.sort_unstable();
    hashed.into_iter().enumerate().map(|(i, (_, u))| (u, i as u32 + 1)).collect()
}

fn apply_pseudonyms(s: &mut Segment, map: &HashMap<u32, u32>) {
    let m = |u: &mut u32| *u = map.get(u).copied().unwrap_or(0);
    match &mut s.body {
        SegmentBody::Node(vs) => vs.iter_mut().for_each(|v| m(&mut v.meta.uid)),
        SegmentBody::Way { slots, versions } => {
            versions.iter_mut().for_each(|v| m(&mut v.meta.uid));
            for sl in slots {
                if let Some(es) = &mut sl.private {
                    es.iter_mut().for_each(|e| m(&mut e.uid));
                }
            }
        }
        SegmentBody::Relation(vs) => vs.iter_mut().for_each(|v| m(&mut v.meta.uid)),
    }
}

/// BlockBlooms section: block count, reserved, `n + 1` word offsets (u32), padding to 8 bytes,
/// then the filter words (u64, little endian).
fn block_blooms_section(offsets: &[u32], words: &[u64]) -> Vec<u8> {
    let n = offsets.len() as u32 - 1;
    let mut b = Vec::with_capacity(8 + 4 * offsets.len() + 8 + 8 * words.len());
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    for o in offsets {
        b.extend_from_slice(&o.to_le_bytes());
    }
    b.resize(b.len().div_ceil(8) * 8, 0);
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b
}

fn build_tree(leaves: Vec<(RNode, Bloom)>) -> (TreeHeader, Vec<(RNode, Bloom)>) {
    if leaves.is_empty() {
        return (TreeHeader::default(), Vec::new());
    }
    let f = RTREE_FANOUT as usize;
    let mut levels: Vec<Vec<(RNode, Bloom)>> = vec![leaves];
    while levels.last().unwrap().len() > 1 {
        let child = levels.last().unwrap();
        let parent: Vec<(RNode, Bloom)> = child
            .chunks(f)
            .enumerate()
            .map(|(ci, ch)| {
                let mut n = RNode { bbox: BBox::EMPTY, tmin: T_OPEN, tmax: 0, first: (ci * f) as u32, count: ch.len() as u32 };
                let mut b = [0u64; bloom::BLOOM_WORDS];
                for (c, cb) in ch {
                    n.bbox.union(&c.bbox);
                    n.tmin = n.tmin.min(c.tmin);
                    n.tmax = n.tmax.max(c.tmax);
                    bloom::or(&mut b, cb);
                }
                (n, b)
            })
            .collect();
        levels.push(parent);
    }
    levels.reverse(); // root first
    let mut offsets = Vec::with_capacity(levels.len());
    let mut acc = 0usize;
    for l in &levels {
        offsets.push(acc);
        acc += l.len();
    }
    let mut out = Vec::with_capacity(acc);
    for (li, l) in levels.iter().enumerate() {
        for (n, b) in l {
            let mut n = *n;
            if n.count > 0 {
                n.first += offsets[li + 1] as u32;
            }
            out.push((n, *b));
        }
    }
    (TreeHeader { n_levels: levels.len() as u32, n_nodes: acc as u32, node_start: 0 }, out)
}

fn build_idindex(mut sorted: Sorted<(i64, u32)>) -> Result<Vec<u8>> {
    use knut_format::codec::put_u64_column;
    let mut table = Vec::new();
    let mut payload = Vec::new();
    let (mut n_entries, mut n_chunks) = (0u64, 0u32);
    let mut c: Vec<(i64, u32)> = Vec::with_capacity(IDX_ENTRIES_PER_CHUNK);
    loop {
        let next = sorted.next_rec()?;
        if let Some(e) = next {
            c.push(e);
        }
        if c.len() == IDX_ENTRIES_PER_CHUNK || (next.is_none() && !c.is_empty()) {
            let first_id = c[0].0;
            let blk_base = c.iter().map(|e| e.1).min().unwrap();
            // consecutive ids mostly live in the same block (e.g. private nodes of one
            // way): block numbers are run-length encoded
            let mut run_blocks: Vec<u64> = Vec::new();
            let mut run_lens: Vec<u64> = Vec::new();
            for &(_, b) in c.iter() {
                let v = (b - blk_base) as u64;
                if run_blocks.last() == Some(&v) {
                    *run_lens.last_mut().unwrap() += 1;
                } else {
                    run_blocks.push(v);
                    run_lens.push(0);
                }
            }
            IdxChunk {
                first_id,
                payload_off: u32::try_from(payload.len()).map_err(|_| err("id index payload exceeds 4 GiB"))?,
                count: c.len() as u16,
                blk_base,
                n_runs: run_blocks.len() as u32,
            }
            .write(&mut table);
            // ids as "+1 runs": zigzag(id - prev - 1) per run start, then run length - 1
            let (mut id_gaps, mut id_lens) = (Vec::new(), Vec::new());
            let mut prev = first_id - 1;
            let mut i = 0;
            while i < c.len() {
                id_gaps.push(zigzag(c[i].0 - prev - 1));
                let mut j = i + 1;
                while j < c.len() && c[j].0 == c[j - 1].0 + 1 {
                    j += 1;
                }
                id_lens.push((j - i - 1) as u64);
                prev = c[j - 1].0;
                i = j;
            }
            put_u64_column(&mut payload, &id_gaps);
            put_u64_column(&mut payload, &id_lens);
            put_u64_column(&mut payload, &run_blocks);
            put_u64_column(&mut payload, &run_lens);
            n_entries += c.len() as u64;
            n_chunks += 1;
            c.clear();
        }
        if next.is_none() {
            break;
        }
    }
    let mut out = Vec::with_capacity(IDX_HEADER_SIZE + table.len() + payload.len());
    out.extend_from_slice(&n_entries.to_le_bytes());
    out.extend_from_slice(&n_chunks.to_le_bytes());
    out.extend_from_slice(&(IDX_ENTRIES_PER_CHUNK as u32).to_le_bytes());
    out.extend_from_slice(&(IDX_HEADER_SIZE as u64).to_le_bytes());
    out.extend_from_slice(&((IDX_HEADER_SIZE + table.len()) as u64).to_le_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn users_section(users: &HashMap<(u32, Str), (i64, i64)>, tb: i64) -> Vec<u8> {
    let mut v: Vec<(u32, i64, i64, &Str)> = users.iter().map(|((u, n), (f, l))| (*u, *f, *l, n)).collect();
    v.sort_unstable_by(|a, b| (a.0, a.1, a.3).cmp(&(b.0, b.1, b.3)));
    let mut out = Vec::new();
    put_varint(&mut out, v.len() as u64);
    let mut prev = 0u64;
    for (u, f, l, n) in v {
        put_varint(&mut out, u as u64 - prev);
        prev = u as u64;
        put_varint(&mut out, n.len() as u64);
        out.extend_from_slice(n.as_bytes());
        put_varint(&mut out, zigzag(f - tb));
        put_varint(&mut out, (l - f) as u64);
    }
    out
}

pub(crate) fn write(c: Collected, path: &Path, opt: &BuildOptions, log: &(dyn Fn(&str) + Sync)) -> Result<BuildStats> {
    let Collected { mut sink, counters, mut stats } = c;
    let t0 = Instant::now();
    let tb = opt.seg.time_base;

    // dictionaries
    let (keys, key_map) = ranked(counters.keys, 1);
    let (values, value_map) = ranked(counters.values, opt.min_value_count.max(1));
    let (roles, role_map) = ranked(counters.roles, 1);
    let dicts = Dicts { keys, values, roles, key_map, value_map, role_map };
    let pseudo = opt.pseudonymize_uids.as_deref().map(|s| pseudonym_map(&counters.users, s));
    log(&format!(
        "dictionaries: {} keys, {} values, {} roles, {} user names",
        dicts.keys.len(),
        dicts.values.len(),
        dicts.roles.len(),
        counters.users.len()
    ));

    let eo = EncodeOptions { time_base: tb, keyframe_interval: opt.keyframe_interval };
    let decode_rows = |rows: &[Row]| -> Result<Vec<Segment>> {
        let mut segs = rows
            .iter()
            .map(|r| {
                let mut s = decode_row(&r.bytes)?;
                if let Some(m) = &pseudo {
                    apply_pseudonyms(&mut s, m);
                }
                Ok(s)
            })
            .collect::<Result<Vec<Segment>>>()?;
        if opt.sort_by_id_in_block {
            segs.sort_by_key(|s| (s.id, s.tmin));
        }
        Ok(segs)
    };

    // zstd dictionaries per object type, trained on blocks formed from the deterministic
    // row sample (identical in memory and spill mode)
    let mut zdicts: [Option<Vec<u8>>; 3] = [None, None, None];
    let samples = std::mem::take(&mut sink.samples);
    if opt.zstd_dict {
        for (t, mut rows) in ObjType::ALL.into_iter().zip(samples) {
            rows.sort_by_key(|r| r.key);
            let mut former = BlockFormer::new(opt);
            let mut blocks: Vec<Vec<Row>> = Vec::new();
            for r in rows {
                if let Some(b) = former.push(r) {
                    blocks.push(b);
                }
            }
            blocks.extend(former.finish());
            if blocks.len() < 8 {
                continue;
            }
            let step = (blocks.len() as f64 / DICT_SAMPLE_BLOCKS as f64).max(1.0);
            let picks: Vec<usize> = (0..DICT_SAMPLE_BLOCKS.min(blocks.len())).map(|k| (k as f64 * step) as usize).collect();
            let raws: Vec<Result<RawBlock>> = picks
                .par_iter()
                .map(|&bi| Ok(encode_block(&decode_rows(&blocks[bi])?, &dicts, &eo)?))
                .collect();
            let mut samples: Vec<Vec<u8>> = Vec::new();
            for r in raws {
                let r = r?;
                // zdict works best on many small samples
                samples.push(r.dir);
                for x in [r.data, r.geom] {
                    samples.extend(x.chunks(16 * 1024).map(|c| c.to_vec()));
                }
            }
            match zstd::dict::from_samples(&samples, opt.zstd_dict_size) {
                Ok(d) => zdicts[t as usize] = Some(d),
                Err(e) => log(&format!("zstd dictionary for {} skipped: {e}", t.name())),
            }
        }
        log(&format!("trained zstd dictionaries ({:.1}s)", t0.elapsed().as_secs_f64()));
    }
    let cdicts: Vec<Option<zstd::dict::EncoderDictionary<'static>>> = zdicts
        .iter()
        .map(|d| d.as_ref().map(|d| zstd::dict::EncoderDictionary::copy(d, opt.zstd_level)))
        .collect();

    // file
    let f = File::create(path)?;
    let mut out = Out { w: BufWriter::with_capacity(8 << 20, f), pos: 0, sections: Vec::new(), checksums: Vec::new(), stream: None };
    out.write(&[0u8; HEADER_SIZE])?;
    out.zsection(SectionKind::Keys, &encode_strings(&dicts.keys), 19)?;
    out.zsection(SectionKind::Values, &encode_strings(&dicts.values), 19)?;
    out.zsection(SectionKind::Roles, &encode_strings(&dicts.roles), 19)?;
    let users_present = !opt.strip_users && pseudo.is_none();
    if users_present {
        out.zsection(SectionKind::Users, &users_section(&counters.users, tb), 19)?;
    }
    for t in ObjType::ALL {
        if let Some(d) = &zdicts[t as usize] {
            out.section(SectionKind::zdict(t), 0, d)?;
        }
    }

    // blocks: stream rows in key order, form blocks, encode batches in parallel
    let t1 = Instant::now();
    stats.n_segments = sink.n_rows;
    let row_bytes = sink.total_bytes;
    let mut spilled = sink.spilled_bytes;
    let mut stream = sink.into_stream()?;
    log(&format!(
        "streaming {} segments ({} MiB row data, {} MiB spilled)",
        stats.n_segments,
        row_bytes >> 20,
        spilled >> 20
    ));
    out.begin_stream(SectionKind::Blocks)?;
    let mut block_table = Vec::new();
    // per block filters: word offsets and words
    let mut filter_offsets: Vec<u32> = vec![0];
    let mut filter_words: Vec<u64> = Vec::new();
    let mut leaves: [Vec<(RNode, Bloom)>; N_TREES] = Default::default();
    let tmp = opt.tmp_dir.clone().unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default());
    let mut idx_sorters: Vec<ExtSorter<(i64, u32)>> =
        ObjType::ALL.iter().map(|_| ExtSorter::new(&tmp, "idx", opt.mem_budget / 6, true)).collect();
    let mut bbox = BBox::EMPTY;
    let mut block_no = 0u32;
    let mut former = BlockFormer::new(opt);
    let mut batch: Vec<Vec<Row>> = Vec::new();
    let mut batch_bytes = 0usize;
    let mut ended = false;
    while !ended {
        match stream.next()? {
            Some(r) => {
                batch_bytes += r.bytes.len();
                if let Some(b) = former.push(r) {
                    batch.push(b);
                }
            }
            None => {
                batch.extend(former.finish());
                ended = true;
            }
        }
        if !(ended || batch.len() >= 2048 || batch_bytes >= 1 << 30) {
            continue;
        }
        batch_bytes = 0;
        let encoded: Vec<Result<EncodedBlock>> = batch
            .par_iter()
            .map(|rows| {
                let segs = decode_rows(rows)?;
                let raw = encode_block(&segs, &dicts, &eo)?;
                let t = raw.obj_type;
                let bytes = frame_block(&raw, opt.zstd_level, cdicts[t as usize].as_ref())?;
                let private_nodes = segs
                    .iter()
                    .flat_map(|s| match &s.body {
                        SegmentBody::Way { slots, .. } => {
                            slots.iter().filter(|s| s.private.is_some()).map(|s| s.node_id).collect::<Vec<_>>()
                        }
                        _ => Vec::new(),
                    })
                    .collect();
                Ok(EncodedBlock {
                    obj_type: t,
                    flags: bytes[5],
                    section: rows[0].key.section,
                    leaf: RNode { bbox: raw.bbox, tmin: raw.tmin, tmax: raw.tmax, first: 0, count: 0 },
                    bloom: raw.bloom,
                    filter: bloom::block_filter(&raw.hashes),
                    ids: segs.iter().map(|s| s.id).collect(),
                    private_nodes,
                    n_large: segs.iter().filter(|s| s.flags & SEG_LARGE != 0).count() as u64,
                    bytes,
                })
            })
            .collect();
        batch.clear();
        for e in encoded {
            let e = e?;
            let offset = out.pos;
            out.write(&e.bytes)?;
            BlockEntry {
                offset,
                length: e.bytes.len() as u32,
                obj_type: e.obj_type,
                flags: e.flags,
                section: e.section as u16,
            }
            .write(&mut block_table);
            filter_words.extend_from_slice(&e.filter);
            filter_offsets.push(u32::try_from(filter_words.len()).map_err(|_| err("block filters too large"))?);
            let tree = if e.section >= 3 { TREE_LARGE } else { e.section as usize };
            leaves[tree].push((RNode { first: block_no, ..e.leaf }, e.bloom));
            bbox.union(&e.leaf.bbox);
            idx_sorters[e.obj_type as usize].extend(e.ids.into_iter().map(|id| (id, block_no)))?;
            idx_sorters[0].extend(e.private_nodes.into_iter().map(|id| (id, block_no)))?;
            stats.n_large_segments += e.n_large;
            block_no += 1;
        }
    }
    out.end_stream();
    drop(stream);
    stats.n_blocks = block_no as u64;
    log(&format!("wrote {} blocks ({:.1}s)", block_no, t1.elapsed().as_secs_f64()));

    out.section(SectionKind::BlockTable, 0, &block_table)?;
    out.section(SectionKind::BlockBlooms, 0, &block_blooms_section(&filter_offsets, &filter_words))?;
    drop(filter_words);

    // R-trees
    let mut rt = Vec::new();
    rt.extend_from_slice(&(N_TREES as u32).to_le_bytes());
    rt.extend_from_slice(&RTREE_FANOUT.to_le_bytes());
    let mut trees = Vec::new();
    let mut node_start = 0u32;
    for l in leaves {
        let (mut h, nodes) = build_tree(l);
        h.node_start = node_start;
        node_start += h.n_nodes;
        trees.push((h, nodes));
    }
    for (h, _) in &trees {
        for v in [h.n_levels, h.n_nodes, h.node_start, 0] {
            rt.extend_from_slice(&v.to_le_bytes());
        }
    }
    for (_, nodes) in &trees {
        for (n, _) in nodes {
            n.write(&mut rt);
        }
    }
    for (_, nodes) in &trees {
        for (_, b) in nodes {
            write_bloom(b, &mut rt);
        }
    }
    out.section(SectionKind::RTree, 0, &rt)?;

    // ID index
    for (t, sorter) in ObjType::ALL.into_iter().zip(idx_sorters) {
        spilled += sorter.spilled_bytes;
        let idx = build_idindex(sorter.finish()?)?;
        out.section(SectionKind::idx(t), 0, &idx)?;
    }
    stats.spilled_bytes += spilled;

    // meta
    // private node metadata is always stored (lossless); the flag stays for readers
    let mut features = FEAT_PRIVATE_META;
    if users_present {
        features |= FEAT_USERS;
    } else {
        features |= FEAT_USERS_STRIPPED;
    }
    if pseudo.is_some() {
        features |= FEAT_UIDS_PSEUDONYMIZED;
    }
    if zdicts.iter().any(|d| d.is_some()) {
        features |= FEAT_ZSTD_DICT;
    }
    features |= FEAT_BLOCK_BLOOMS;
    let meta = format!(
        "generator=knut-builder {}\nsource={}\nlicense=ODbL-1.0 (c) OpenStreetMap contributors\n\
         block_size={}\nzstd_level={}\nzstd_dict={}\nkeyframe_interval={}\nseg_grow={}\nseg_min_size_m={}\n\
         seg_jump_m={}\nlarge_threshold_e7={}\nmin_value_count={}\nrelation_depth={}\nsort_key={}\n",
        env!("CARGO_PKG_VERSION"),
        opt.source_name,
        opt.block_size,
        opt.zstd_level,
        opt.zstd_dict,
        opt.keyframe_interval,
        opt.seg.grow,
        opt.seg.min_size_m,
        opt.seg.jump_m,
        opt.seg.large_threshold_e7,
        opt.min_value_count,
        opt.relation_depth,
        if opt.s2_sort_key { "s2" } else { "hilbert" }
    );
    out.section(SectionKind::Meta, 0, meta.as_bytes())?;

    out.align()?;
    let footer = footer_bytes(&out.checksums);
    out.write(&footer)?;
    let header = Header {
        format_major: FORMAT_MAJOR,
        format_minor: FORMAT_MINOR,
        features,
        time_base: tb,
        tmin: stats.tmin,
        tmax: stats.tmax,
        bbox,
        n_objects: stats.n_objects,
        n_versions: stats.n_versions,
        n_segments: stats.n_segments,
        n_blocks: stats.n_blocks,
        sections: out.sections.clone(),
    };
    out.w.flush()?;
    let mut f = out.w.into_inner().map_err(|e| err(format!("flush: {e}")))?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&header.to_bytes())?;
    f.sync_all()?;

    stats.bbox = bbox;
    stats.output_bytes = out.pos;
    stats.section_bytes = out
        .sections
        .iter()
        .map(|s| {
            let name = format!("{:?}", SectionKind::from_u32(s.kind).unwrap());
            (name, s.length)
        })
        .collect();
    stats.phase_secs.push(("write".into(), t0.elapsed().as_secs_f64()));
    stats.peak_rss_bytes = peak_rss_bytes();
    stats.peak_footprint_bytes = peak_footprint_bytes();
    Ok(stats)
}
