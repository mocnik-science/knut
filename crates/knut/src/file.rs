//! Opening and validating `.knutdb` files, block access, R-tree traversal and ID index.

use crate::cache::BlockCache;
use crate::{Error, Result, corrupt};
use memmap2::Mmap;
use knut_format::block::{self, DecodeDicts, DecodeOptions, SegDir};
use knut_format::bloom::{self, Bloom};
use knut_format::codec::{get_varint, le, unzigzag};
use knut_format::layout::*;
use knut_format::strings::decode_strings;
use knut_format::*;
use rustc_hash::FxHashMap as HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use xxhash_rust::xxh3::xxh3_64;

enum Data {
    Mmap(Mmap),
    Bytes(Vec<u8>),
}

impl Data {
    fn bytes(&self) -> &[u8] {
        match self {
            Data::Mmap(m) => m,
            Data::Bytes(b) => b,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct IdIndex {
    off: usize,
    len: usize,
    n_entries: u64,
    n_chunks: usize,
    table: usize,
    payload: usize,
}

#[derive(Clone, Debug)]
pub struct UserEntry {
    pub uid: u32,
    pub name: Str,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// A fully decoded block.
pub struct DecodedBlock {
    pub no: u32,
    pub obj_type: ObjType,
    pub dirs: Vec<SegDir>,
    /// Relation member lists are empty here; use [`DecodedBlock::members`].
    pub segs: Vec<Segment>,
    rel: Option<block::RelLists>,
    vbase: Vec<u32>,
    /// Decoded without the geometry frame (positions, slots and node lists missing).
    pub attr_only: bool,
    ids: HashMap<i64, Vec<u32>>,
    private: OnceLock<HashMap<i64, Vec<(u32, u32)>>>,
}

impl DecodedBlock {
    /// Members of relation version `vi` of segment `si`.
    pub fn members(&self, si: usize, vi: usize) -> Result<Vec<Member>> {
        match &self.rel {
            Some(r) => Ok(r.materialize(self.vbase[si] as usize + vi)?),
            None => Ok(Vec::new()),
        }
    }

    /// Members of all versions of segment `si`.
    pub fn members_all(&self, si: usize) -> Result<Vec<Vec<Member>>> {
        match &self.rel {
            Some(r) => Ok(r.materialize_run(self.vbase[si] as usize, self.segs[si].body.n_versions())?),
            None => Ok(Vec::new()),
        }
    }

    /// Segment indexes with object id `id`.
    pub fn segments_of(&self, id: i64) -> &[u32] {
        self.ids.get(&id).map_or(&[], |v| v.as_slice())
    }

    /// `(segment, slot)` pairs of private node `id` (way blocks only).
    pub fn private_slots(&self, id: i64) -> &[(u32, u32)] {
        let m = self.private.get_or_init(|| {
            let mut m: HashMap<i64, Vec<(u32, u32)>> = HashMap::default();
            for (si, s) in self.segs.iter().enumerate() {
                if let SegmentBody::Way { slots, .. } = &s.body {
                    for (k, sl) in slots.iter().enumerate() {
                        if sl.private.is_some() {
                            m.entry(sl.node_id).or_default().push((si as u32, k as u32));
                        }
                    }
                }
            }
            m
        });
        m.get(&id).map_or(&[], |v| v.as_slice())
    }
}

pub struct Reader {
    data: Data,
    pub header: Header,
    pub dicts: DecodeDicts,
    key_ids: HashMap<Str, u32>,
    value_ids: HashMap<Str, u32>,
    zdicts: [Option<zstd::dict::DecoderDictionary<'static>>; 3],
    block_table: (usize, usize),
    trees: [TreeHeader; N_TREES],
    rnodes_off: usize,
    blooms_off: usize,
    rnodes_total: usize,
    /// BlockBlooms section: absolute offsets of the word offsets and of the words.
    block_blooms: Option<(usize, usize)>,
    idx: [IdIndex; 3],
    cache: BlockCache,
    verify_checksums: bool,
    users: OnceLock<Result<Vec<UserEntry>>>,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// Verify per-block checksums on every decode.
    pub verify_checksums: bool,
    /// Number of decoded blocks kept in the LRU cache.
    pub cache_blocks: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions { verify_checksums: true, cache_blocks: 1024 }
    }
}

impl Reader {
    pub fn open(path: impl AsRef<Path>) -> Result<Reader> {
        Reader::open_with(path, OpenOptions::default())
    }

    pub fn open_with(path: impl AsRef<Path>, opt: OpenOptions) -> Result<Reader> {
        let f = std::fs::File::open(path.as_ref()).map_err(|e| Error::Io(e.to_string()))?;
        // SAFETY: read-only mapping; the file must not be modified while open.
        let m = unsafe { Mmap::map(&f) }.map_err(|e| Error::Io(e.to_string()))?;
        Reader::from_data(Data::Mmap(m), opt)
    }

    pub fn from_bytes(bytes: Vec<u8>, opt: OpenOptions) -> Result<Reader> {
        Reader::from_data(Data::Bytes(bytes), opt)
    }

    fn from_data(data: Data, opt: OpenOptions) -> Result<Reader> {
        let buf = data.bytes();
        let header = Header::parse(buf, buf.len() as u64)?;
        if header.features & FEAT_PRIVATE_META == 0 {
            return Err(Error::Format(knut_format::Error::Unsupported(
                "files without PRIVATE_META (geometry-only private nodes) are no longer supported".into(),
            )));
        }
        let sec = |k: SectionKind| -> Result<&[u8]> {
            let s = header.section(k).ok_or_else(|| corrupt(format!("missing section {k:?}")))?;
            Ok(le::slice(buf, s.offset as usize, s.length as usize)?)
        };
        let zsec = |k: SectionKind| -> Result<Vec<u8>> {
            let raw = sec(k)?;
            zstd::stream::decode_all(raw).map_err(|e| corrupt(format!("section {k:?}: {e}")))
        };
        let dicts = DecodeDicts {
            keys: decode_strings(&zsec(SectionKind::Keys)?)?,
            values: decode_strings(&zsec(SectionKind::Values)?)?,
            roles: decode_strings(&zsec(SectionKind::Roles)?)?,
        };
        let key_ids = dicts.keys.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
        let value_ids = dicts.values.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
        let mut zdicts: [Option<zstd::dict::DecoderDictionary<'static>>; 3] = [None, None, None];
        for t in ObjType::ALL {
            if header.section(SectionKind::zdict(t)).is_some() {
                zdicts[t as usize] = Some(zstd::dict::DecoderDictionary::copy(sec(SectionKind::zdict(t))?));
            }
        }
        let bt = header.section(SectionKind::BlockTable).ok_or_else(|| corrupt("missing block table"))?;
        let n_blocks = bt.length as usize / BLOCK_ENTRY_SIZE;
        if n_blocks as u64 != header.n_blocks {
            return Err(corrupt("block count mismatch"));
        }
        let rt_sec = header.section(SectionKind::RTree).ok_or_else(|| corrupt("missing rtree"))?;
        let rt = sec(SectionKind::RTree)?;
        if le::u32(rt, 0)? as usize != N_TREES {
            return Err(corrupt("rtree count"));
        }
        let mut trees = [TreeHeader::default(); N_TREES];
        let mut total = 0usize;
        for (i, t) in trees.iter_mut().enumerate() {
            let o = 8 + 16 * i;
            *t = TreeHeader { n_levels: le::u32(rt, o)?, n_nodes: le::u32(rt, o + 4)?, node_start: le::u32(rt, o + 8)? };
            total = total.max(t.node_start as usize + t.n_nodes as usize);
        }
        let rnodes_off = rt_sec.offset as usize + 8 + 16 * N_TREES;
        let blooms_off = rnodes_off + total * RNODE_SIZE;
        le::slice(buf, rnodes_off, total * (RNODE_SIZE + BLOOM_SIZE))?;
        let block_blooms = match header.section(SectionKind::BlockBlooms) {
            Some(s) if header.features & FEAT_BLOCK_BLOOMS != 0 => {
                let b = sec(SectionKind::BlockBlooms)?;
                if le::u32(b, 0)? as usize != n_blocks {
                    return Err(corrupt("block filter count"));
                }
                let offs = 8;
                let words = (offs + 4 * (n_blocks + 1)).div_ceil(8) * 8;
                let n_words = le::u32(b, offs + 4 * n_blocks)? as usize;
                le::slice(b, words, 8 * n_words)?;
                Some((s.offset as usize + offs, s.offset as usize + words))
            }
            _ => None,
        };
        let mut idx = [IdIndex { off: 0, len: 0, n_entries: 0, n_chunks: 0, table: 0, payload: 0 }; 3];
        for t in ObjType::ALL {
            let s = header.section(SectionKind::idx(t)).ok_or_else(|| corrupt("missing id index"))?;
            let b = sec(SectionKind::idx(t))?;
            let n_chunks = le::u32(b, 8)? as usize;
            let table = le::u64(b, 16)? as usize;
            let payload = le::u64(b, 24)? as usize;
            le::slice(b, table, n_chunks * IDX_CHUNK_SIZE)?;
            if payload > b.len() {
                return Err(corrupt("id index payload offset"));
            }
            idx[t as usize] = IdIndex {
                off: s.offset as usize,
                len: s.length as usize,
                n_entries: le::u64(b, 0)?,
                n_chunks,
                table,
                payload,
            };
        }
        Ok(Reader {
            block_table: (bt.offset as usize, n_blocks),
            header,
            dicts,
            key_ids,
            value_ids,
            zdicts,
            trees,
            rnodes_off,
            blooms_off,
            rnodes_total: total,
            block_blooms,
            idx,
            cache: BlockCache::new(opt.cache_blocks),
            verify_checksums: opt.verify_checksums,
            users: OnceLock::new(),
            data,
        })
    }

    /// The bytes of one section (of an optional index, for example).
    pub fn section_bytes(&self, s: &SectionEntry) -> Result<&[u8]> {
        Ok(le::slice(self.buf(), s.offset as usize, s.length as usize)?)
    }

    /// The newest element of a kind in the container, if any.
    pub fn element(&self, kind: knut_format::container::ElementKind) -> Option<&knut_format::container::Element> {
        self.header.directory.elements.iter().filter(|e| e.kind == kind).max_by_key(|e| e.id)
    }

    #[inline]
    pub(crate) fn buf(&self) -> &[u8] {
        self.data.bytes()
    }

    pub fn time_base(&self) -> i64 {
        self.header.time_base
    }

    pub fn n_blocks(&self) -> usize {
        self.block_table.1
    }

    pub fn key_id(&self, k: &str) -> Option<u32> {
        self.key_ids.get(k).copied()
    }

    pub fn value_id(&self, v: &str) -> Option<u32> {
        self.value_ids.get(v).copied()
    }

    pub fn meta_text(&self) -> Option<String> {
        let s = self.header.section(SectionKind::Meta)?;
        let b = le::slice(self.buf(), s.offset as usize, s.length as usize).ok()?;
        Some(String::from_utf8_lossy(b).into_owned())
    }

    pub fn users(&self) -> Result<&[UserEntry]> {
        let r = self.users.get_or_init(|| {
            let Some(s) = self.header.section(SectionKind::Users) else { return Ok(Vec::new()) };
            let raw = le::slice(self.buf(), s.offset as usize, s.length as usize)?;
            let b = zstd::stream::decode_all(raw).map_err(|e| corrupt(format!("users: {e}")))?;
            let mut p = 0;
            let n = get_varint(&b, &mut p)? as usize;
            if n > b.len() {
                return Err(corrupt("users count"));
            }
            let mut out = Vec::with_capacity(n);
            let mut uid = 0u64;
            for _ in 0..n {
                uid += get_varint(&b, &mut p)?;
                let l = get_varint(&b, &mut p)? as usize;
                let name = le::slice(&b, p, l)?;
                p += l;
                let first = self.header.time_base + unzigzag(get_varint(&b, &mut p)?);
                let last = first + get_varint(&b, &mut p)? as i64;
                out.push(UserEntry {
                    uid: uid as u32,
                    name: Str::from(String::from_utf8_lossy(name).as_ref()),
                    first_seen: first,
                    last_seen: last,
                });
            }
            Ok(out)
        });
        match r {
            Ok(v) => Ok(v),
            Err(e) => Err(corrupt(e.to_string())),
        }
    }

    pub fn block_entry(&self, no: u32) -> Result<BlockEntry> {
        if no as usize >= self.block_table.1 {
            return Err(corrupt("block number out of range"));
        }
        let t = le::slice(self.buf(), self.block_table.0, self.block_table.1 * BLOCK_ENTRY_SIZE)?;
        Ok(BlockEntry::parse(t, no as usize)?)
    }

    fn frame(&self, no: u32) -> Result<block::BlockFrame<'_>> {
        let e = self.block_entry(no)?;
        let b = le::slice(self.buf(), e.offset as usize, e.length as usize)?;
        let f = block::parse_block_frame(b, self.verify_checksums)?;
        if f.header.obj_type != e.obj_type {
            return Err(corrupt("block type mismatch"));
        }
        Ok(f)
    }

    fn zdict(&self, f: &block::BlockFrame) -> Result<Option<&zstd::dict::DecoderDictionary<'static>>> {
        if f.header.flags & BLOCK_FLAG_DICT == 0 {
            return Ok(None);
        }
        self.zdicts[f.header.obj_type as usize]
            .as_ref()
            .map(Some)
            .ok_or_else(|| corrupt("block requires missing zstd dictionary"))
    }

    /// Segment directory of a block (time intervals and boxes are partly derived from the
    /// records, so this decodes the whole block; the result is cached).
    pub fn block_dirs(&self, no: u32) -> Result<(ObjType, Vec<SegDir>)> {
        let b = self.block(no)?;
        Ok((b.obj_type, b.dirs.clone()))
    }

    /// Fully decodes a block (cached).
    pub fn block(&self, no: u32) -> Result<Arc<DecodedBlock>> {
        if let Some(b) = self.cache.get(no) {
            return Ok(b);
        }
        let b = Arc::new(self.decode_block(no, None, false)?);
        self.cache.put(no, b.clone());
        Ok(b)
    }

    /// Decodes only node/way segments accepted by `filter` (not cached). Returns the
    /// cached full block if present; relation blocks are always decoded fully.
    pub fn block_filtered(&self, no: u32, filter: &block::SegFilter) -> Result<Arc<DecodedBlock>> {
        if let Some(b) = self.cache.get(no) {
            return Ok(b);
        }
        if self.block_entry(no)?.obj_type == ObjType::Relation {
            return self.block(no);
        }
        Ok(Arc::new(self.decode_block(no, Some(filter), false)?))
    }

    /// Like [`Reader::block_filtered`] but decodes only the attribute frame (no geometry);
    /// relation blocks and cached blocks are returned complete.
    pub fn block_attrs(&self, no: u32, filter: &block::SegFilter) -> Result<Arc<DecodedBlock>> {
        if let Some(b) = self.cache.get(no) {
            return Ok(b);
        }
        if self.block_entry(no)?.obj_type == ObjType::Relation {
            return self.block(no);
        }
        Ok(Arc::new(self.decode_block(no, Some(filter), true)?))
    }

    /// Block header (cheap; no decompression).
    pub fn block_header(&self, no: u32) -> Result<BlockHeader> {
        Ok(self.frame(no)?.header)
    }

    fn decode_block(&self, no: u32, filter: Option<&block::SegFilter>, attr_only: bool) -> Result<DecodedBlock> {
        let f = self.frame(no)?;
        let zd = self.zdict(&f)?;
        let dir = block::decompress(f.dir, f.header.dir_raw, zd)?;
        let dirs = block::decode_directory(&dir, f.header.n_segments)?;
        let data = block::decompress(f.data, f.header.data_raw, zd)?;
        let geom = if attr_only { None } else { Some(block::decompress(f.geom, f.header.geom_raw, zd)?) };
        let d = block::decode_data_filtered(
            f.header.obj_type,
            &dirs,
            &data,
            geom.as_deref(),
            &self.dicts,
            &DecodeOptions { time_base: self.header.time_base },
            true,
            filter,
        )?;
        let (segs, rel) = (d.segs, d.rel);
        let dirs: Vec<SegDir> = segs
            .iter()
            .map(|s| SegDir {
                id: s.id,
                flags: s.flags,
                tmin: s.tmin,
                tmax: s.tmax,
                bbox: s.bbox,
                n_versions: s.body.n_versions() as u32,
                n_slots: match &s.body {
                    SegmentBody::Way { slots, .. } => slots.len() as u32,
                    _ => 0,
                },
            })
            .collect();
        let mut ids: HashMap<i64, Vec<u32>> = HashMap::with_capacity_and_hasher(segs.len(), Default::default());
        let mut vbase = Vec::with_capacity(segs.len());
        let mut acc = 0u32;
        for (i, s) in segs.iter().enumerate() {
            ids.entry(s.id).or_default().push(i as u32);
            vbase.push(acc);
            acc += s.body.n_versions() as u32;
        }
        Ok(DecodedBlock {
            no,
            obj_type: f.header.obj_type,
            dirs,
            segs,
            rel,
            vbase,
            attr_only,
            ids,
            private: OnceLock::new(),
        })
    }

    /// Raw (decompressed) directory, attribute and geometry frames of a block.
    pub fn raw_frames(&self, no: u32) -> Result<(BlockHeader, Vec<u8>, Vec<u8>, Vec<u8>)> {
        let f = self.frame(no)?;
        let zd = self.zdict(&f)?;
        Ok((
            f.header,
            block::decompress(f.dir, f.header.dir_raw, zd)?,
            block::decompress(f.data, f.header.data_raw, zd)?,
            block::decompress(f.geom, f.header.geom_raw, zd)?,
        ))
    }

    // ---- R-tree

    fn rnode(&self, abs: usize) -> Result<(RNode, Bloom)> {
        if abs >= self.rnodes_total {
            return Err(corrupt("rtree node index"));
        }
        let n = RNode::parse(self.buf(), self.rnodes_off + abs * RNODE_SIZE)?;
        let b = parse_bloom(self.buf(), self.blooms_off + abs * BLOOM_SIZE)?;
        Ok((n, b))
    }

    /// Whether block `no` may contain all of `hashes` (keys and key=value pairs, see
    /// [`knut_format::bloom`]): its sized filter, or true for files without one.
    pub fn block_may_contain(&self, no: u32, hashes: &[u64]) -> bool {
        let Some((offs, words)) = self.block_blooms else { return true };
        let buf = self.buf();
        let (Ok(a), Ok(b)) = (le::u32(buf, offs + 4 * no as usize), le::u32(buf, offs + 4 * (no as usize + 1))) else {
            return true;
        };
        let Ok(w) = le::slice(buf, words + 8 * a as usize, 8 * (b.saturating_sub(a)) as usize) else { return true };
        hashes.iter().all(|&h| bloom::block_test(w, h))
    }

    /// Returns all leaf block numbers of `tree` whose path satisfies `pred`.
    pub fn query_tree(&self, tree: usize, pred: &dyn Fn(&RNode, &Bloom) -> bool) -> Result<Vec<u32>> {
        let th = self.trees[tree];
        let mut out = Vec::new();
        if th.n_nodes == 0 {
            return Ok(out);
        }
        let mut stack = vec![0usize];
        let mut steps = 0usize;
        while let Some(i) = stack.pop() {
            steps += 1;
            if steps > 4 * th.n_nodes as usize + 16 || i >= th.n_nodes as usize {
                return Err(corrupt("rtree structure"));
            }
            let (n, b) = self.rnode(th.node_start as usize + i)?;
            if !pred(&n, &b) {
                continue;
            }
            if n.count == 0 {
                out.push(n.first);
            } else {
                let end = n.first as u64 + n.count as u64;
                if n.count > RTREE_FANOUT || end > th.n_nodes as u64 || (n.first as usize) <= i {
                    return Err(corrupt("rtree child range"));
                }
                for c in (n.first..end as u32).rev() {
                    stack.push(c as usize);
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    pub fn tree_header(&self, tree: usize) -> TreeHeader {
        self.trees[tree]
    }

    // ---- ID index

    fn idx_chunk(&self, t: ObjType, ci: usize) -> Result<IdxChunk> {
        let ix = &self.idx[t as usize];
        Ok(IdxChunk::parse(self.buf(), ix.off + ix.table + ci * IDX_CHUNK_SIZE)?)
    }

    fn idx_decode(&self, t: ObjType, c: &IdxChunk) -> Result<Vec<(i64, u32)>> {
        use knut_format::codec::get_u64_column;
        let ix = &self.idx[t as usize];
        let base = ix.off + ix.payload + c.payload_off as usize;
        let end = ix.off + ix.len;
        let b = self.buf().get(base..end).ok_or(knut_format::Error::Truncated("id index payload"))?;
        let mut p = 0;
        let n = c.count as usize;
        let id_gaps = get_u64_column(b, &mut p)?;
        let id_lens = get_u64_column(b, &mut p)?;
        if id_gaps.len() != id_lens.len() || id_gaps.len() > n {
            return Err(corrupt("id index chunk lengths"));
        }
        let mut ids = Vec::with_capacity(n);
        let mut prev = c.first_id.wrapping_sub(1);
        for (g, l) in id_gaps.iter().zip(&id_lens) {
            if *l as usize >= n - ids.len().min(n) {
                return Err(corrupt("id index id runs exceed count"));
            }
            let start = prev.wrapping_add(1).wrapping_add(unzigzag(*g));
            for k in 0..=*l as i64 {
                ids.push(start.wrapping_add(k));
            }
            prev = *ids.last().unwrap();
        }
        let run_blocks = get_u64_column(b, &mut p)?;
        let run_lens = get_u64_column(b, &mut p)?;
        if ids.len() != n || run_blocks.len() != c.n_runs as usize || run_lens.len() != run_blocks.len() {
            return Err(corrupt("id index chunk lengths"));
        }
        let mut out = Vec::with_capacity(n);
        let mut k = 0usize;
        for (rb, rl) in run_blocks.iter().zip(&run_lens) {
            let blk = c.blk_base.wrapping_add(*rb as u32);
            for _ in 0..=*rl {
                let id = *ids.get(k).ok_or_else(|| corrupt("id index runs exceed count"))?;
                out.push((id, blk));
                k += 1;
            }
        }
        if k != n {
            return Err(corrupt("id index runs do not cover chunk"));
        }
        Ok(out)
    }

    /// Block numbers containing segments of object `id` (and, for nodes, way blocks with
    /// its private time series).
    pub fn lookup(&self, t: ObjType, id: i64) -> Result<Vec<u32>> {
        let ix = self.idx[t as usize];
        let (mut lo, mut hi) = (0usize, ix.n_chunks);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.idx_chunk(t, mid)?.first_id < id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut out = Vec::new();
        let mut ci = lo.saturating_sub(1);
        while ci < ix.n_chunks {
            let c = self.idx_chunk(t, ci)?;
            if c.first_id > id {
                break;
            }
            for (i, b) in self.idx_decode(t, &c)? {
                if i == id {
                    out.push(b);
                }
            }
            ci += 1;
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Batched [`Reader::lookup`] for ascending `ids`: each index chunk is decoded once.
    pub fn lookup_sorted(&self, t: ObjType, ids: &[i64]) -> Result<Vec<Vec<u32>>> {
        let ix = self.idx[t as usize];
        let mut out = vec![Vec::new(); ids.len()];
        if ids.is_empty() || ix.n_chunks == 0 {
            return Ok(out);
        }
        let (mut lo, mut hi) = (0usize, ix.n_chunks);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.idx_chunk(t, mid)?.first_id < ids[0] {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut ci = lo.saturating_sub(1);
        let mut k = 0usize;
        while ci < ix.n_chunks && k < ids.len() {
            let c = self.idx_chunk(t, ci)?;
            // skip ahead quickly when the next id is beyond this chunk
            if ci + 1 < ix.n_chunks && self.idx_chunk(t, ci + 1)?.first_id < ids[k] {
                ci += 1;
                continue;
            }
            let entries = self.idx_decode(t, &c)?;
            let last = entries.last().map_or(i64::MIN, |e| e.0);
            let mut j = 0usize;
            let mut kk = k;
            while kk < ids.len() && ids[kk] <= last {
                while j < entries.len() && entries[j].0 < ids[kk] {
                    j += 1;
                }
                let mut jj = j;
                while jj < entries.len() && entries[jj].0 == ids[kk] {
                    out[kk].push(entries[jj].1);
                    jj += 1;
                }
                kk += 1;
            }
            // ids equal to `last` may continue in the next chunk: do not advance past them
            k = if kk > k && ids[kk - 1] == last { kk - 1 } else { kk };
            ci += 1;
        }
        for v in out.iter_mut() {
            v.sort_unstable();
            v.dedup();
        }
        Ok(out)
    }

    /// Iterates all distinct ids of a type in ascending order.
    pub fn all_ids(&self, t: ObjType) -> Result<Vec<i64>> {
        let ix = self.idx[t as usize];
        let mut out = Vec::with_capacity(ix.n_entries as usize);
        for ci in 0..ix.n_chunks {
            let c = self.idx_chunk(t, ci)?;
            for (i, _) in self.idx_decode(t, &c)? {
                if out.last() != Some(&i) {
                    out.push(i);
                }
            }
        }
        Ok(out)
    }

    /// (entries, runs, chunks, payload bytes of the id-delta column) of an ID index.
    pub fn index_stats(&self, t: ObjType) -> Result<(u64, u64, u64, u64)> {
        let ix = self.idx[t as usize];
        let (mut runs, mut idbytes) = (0u64, 0u64);
        for ci in 0..ix.n_chunks {
            let c = self.idx_chunk(t, ci)?;
            runs += c.n_runs as u64;
            let base = ix.off + ix.payload + c.payload_off as usize;
            let b = &self.buf()[base..ix.off + ix.len];
            let mut p = 0;
            knut_format::codec::get_u64_column(b, &mut p)?;
            idbytes += p as u64;
        }
        Ok((ix.n_entries, runs, ix.n_chunks as u64, idbytes))
    }

    pub fn n_index_entries(&self, t: ObjType) -> u64 {
        self.idx[t as usize].n_entries
    }

    // ---- verification

    /// Verifies section checksums (footer) and, if `deep`, decodes every block.
    pub fn verify(&self, deep: bool) -> Result<VerifyReport> {
        let buf = self.buf();
        for s in &self.header.sections {
            let b = le::slice(buf, s.offset as usize, s.length as usize)?;
            if xxh3_64(b) != s.checksum {
                return Err(Error::Format(knut_format::Error::Checksum(format!("section kind {}", s.kind))));
            }
        }
        let mut rep = VerifyReport { sections: self.header.sections.len(), ..Default::default() };
        if deep {
            use rayon::prelude::*;
            let n = self.n_blocks() as u32;
            let parts: Vec<Result<(u64, u64)>> = (0..n)
                .into_par_iter()
                .map(|no| {
                    let f = self.frame(no)?;
                    let zd = self.zdict(&f)?;
                    let dir = block::decompress(f.dir, f.header.dir_raw, zd)?;
                    let dirs = block::decode_directory(&dir, f.header.n_segments)?;
                    let data = block::decompress(f.data, f.header.data_raw, zd)?;
                    let geom = block::decompress(f.geom, f.header.geom_raw, zd)?;
                    let segs = block::decode_data(
                        f.header.obj_type,
                        &dirs,
                        &data,
                        &geom,
                        &self.dicts,
                        &DecodeOptions { time_base: self.header.time_base },
                    )?;
                    for s in &segs {
                        if !s.bbox.is_empty() && !f.header.bbox.intersects(&s.bbox) {
                            return Err(corrupt(format!("block {no}: segment bbox outside block bbox")));
                        }
                        // the block's filter knows every key and key=value pair of the dictionaries
                        let tags: Vec<&Tags> = match &s.body {
                            SegmentBody::Node(v) => v.iter().map(|v| &v.tags).collect(),
                            SegmentBody::Way { versions, .. } => versions.iter().map(|v| &v.tags).collect(),
                            SegmentBody::Relation(v) => v.iter().map(|v| &v.tags).collect(),
                        };
                        for (k, v) in tags.into_iter().flat_map(|t| t.iter()) {
                            let Some(kid) = self.key_id(k) else { continue };
                            let mut hs = vec![bloom::hash_key(kid)];
                            hs.extend(self.value_id(v).map(|vid| bloom::hash_kv(kid, vid)));
                            if !self.block_may_contain(no, &hs) {
                                return Err(corrupt(format!("block {no}: filter misses {k}={v}")));
                            }
                        }
                    }
                    Ok((segs.len() as u64, segs.iter().map(|s| s.body.n_versions() as u64).sum()))
                })
                .collect();
            for p in parts {
                let (s, v) = p?;
                rep.blocks += 1;
                rep.segments += s;
                rep.version_records += v;
            }
            if rep.segments != self.header.n_segments {
                return Err(corrupt("segment count mismatch"));
            }
        }
        Ok(rep)
    }
}

#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    pub sections: usize,
    pub blocks: u64,
    pub segments: u64,
    pub version_records: u64,
}
