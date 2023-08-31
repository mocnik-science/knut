//! Minimal, history-aware OSM PBF reader (hand-written protobuf decoding).
//!
//! Reads `*.osh.pbf` with full metadata including `visible`. Blobs are decoded in parallel
//! directly from a memory map.

use crate::{Element, RawMeta, RawNode, RawRel, RawWay, Result, err};
use flate2::read::ZlibDecoder;
use memmap2::Mmap;
use knut_format::{Member, ObjType, Str, Tags};
use rayon::prelude::*;
use std::io::Read;
use std::ops::Range;
use std::path::Path;

// ---------------------------------------------------------------------------------------
// Protobuf wire format

enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

struct Pb<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Pb<'a> {
    fn new(b: &'a [u8]) -> Self {
        Pb { b, p: 0 }
    }

    fn varint(&mut self) -> Result<u64> {
        let mut r = 0u64;
        let mut shift = 0;
        loop {
            let byte = *self.b.get(self.p).ok_or_else(|| err("pbf: truncated varint"))?;
            self.p += 1;
            r |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(r);
            }
            shift += 7;
            if shift > 63 {
                return Err(err("pbf: varint too long"));
            }
        }
    }

    fn next(&mut self) -> Result<Option<(u32, Wire<'a>)>> {
        if self.p >= self.b.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = (key >> 3) as u32;
        let w = match key & 7 {
            0 => Wire::Varint(self.varint()?),
            1 => {
                self.p += 8;
                Wire::Fixed
            }
            2 => {
                let len = self.varint()? as usize;
                let end = self.p.checked_add(len).ok_or_else(|| err("pbf: length overflow"))?;
                let s = self.b.get(self.p..end).ok_or_else(|| err("pbf: truncated field"))?;
                self.p = end;
                Wire::Bytes(s)
            }
            5 => {
                self.p += 4;
                Wire::Fixed
            }
            t => return Err(err(format!("pbf: unsupported wire type {t}"))),
        };
        if self.p > self.b.len() {
            return Err(err("pbf: truncated"));
        }
        Ok(Some((field, w)))
    }
}

struct Packed<'a> {
    pb: Pb<'a>,
}

impl Iterator for Packed<'_> {
    type Item = Result<u64>;
    fn next(&mut self) -> Option<Result<u64>> {
        if self.pb.p >= self.pb.b.len() { None } else { Some(self.pb.varint()) }
    }
}

fn packed(b: &[u8]) -> Packed<'_> {
    Packed { pb: Pb::new(b) }
}

#[inline]
fn zz(v: u64) -> i64 {
    ((v >> 1) as i64) ^ (-((v & 1) as i64))
}

// ---------------------------------------------------------------------------------------
// Blob level

#[derive(Clone, Copy, Debug)]
pub struct BlobRef {
    pub data_off: usize,
    pub data_len: usize,
}

/// Indexes all `OSMData` blobs of a PBF file.
pub fn index_blobs(buf: &[u8]) -> Result<Vec<BlobRef>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < buf.len() {
        let hl = u32::from_be_bytes(buf.get(p..p + 4).ok_or_else(|| err("pbf: truncated header length"))?.try_into().unwrap())
            as usize;
        p += 4;
        let hdr = buf.get(p..p + hl).ok_or_else(|| err("pbf: truncated blob header"))?;
        p += hl;
        let mut typ: &[u8] = b"";
        let mut size = 0usize;
        let mut pb = Pb::new(hdr);
        while let Some((f, w)) = pb.next()? {
            match (f, w) {
                (1, Wire::Bytes(b)) => typ = b,
                (3, Wire::Varint(v)) => size = v as usize,
                _ => {}
            }
        }
        if p + size > buf.len() {
            return Err(err("pbf: truncated blob"));
        }
        match typ {
            b"OSMData" => out.push(BlobRef { data_off: p, data_len: size }),
            b"OSMHeader" => check_header(&decompress(&buf[p..p + size])?)?,
            _ => {}
        }
        p += size;
    }
    Ok(out)
}

fn check_header(raw: &[u8]) -> Result<()> {
    let mut pb = Pb::new(raw);
    while let Some((f, w)) = pb.next()? {
        if let (4, Wire::Bytes(b)) = (f, w) {
            let feat = String::from_utf8_lossy(b);
            match feat.as_ref() {
                "OsmSchema-V0.6" | "DenseNodes" | "HistoricalInformation" => {}
                other => return Err(err(format!("pbf: unsupported required feature {other}"))),
            }
        }
    }
    Ok(())
}

pub fn decompress(blob: &[u8]) -> Result<Vec<u8>> {
    let mut raw_size = 0usize;
    let mut pb = Pb::new(blob);
    let mut out = None;
    while let Some((f, w)) = pb.next()? {
        match (f, w) {
            (1, Wire::Bytes(b)) => out = Some(b.to_vec()),
            (2, Wire::Varint(v)) => raw_size = v as usize,
            (3, Wire::Bytes(b)) => {
                let mut v = Vec::with_capacity(raw_size);
                ZlibDecoder::new(b).read_to_end(&mut v).map_err(|e| err(format!("pbf: zlib: {e}")))?;
                out = Some(v);
            }
            (7, Wire::Bytes(b)) => {
                out = Some(zstd::bulk::decompress(b, raw_size.max(1)).map_err(|e| err(format!("pbf: zstd: {e}")))?)
            }
            (4..=6, Wire::Bytes(_)) => return Err(err("pbf: unsupported blob compression (lzma/bzip2/lz4)")),
            _ => {}
        }
    }
    out.ok_or_else(|| err("pbf: empty blob"))
}

pub const WANT_NODES: u8 = 1;
pub const WANT_WAYS: u8 = 2;
pub const WANT_RELATIONS: u8 = 4;

/// Returns a bitmask of the primitive group kinds contained in a decompressed block.
fn group_kinds(raw: &[u8]) -> Result<u8> {
    let mut kinds = 0u8;
    let mut pb = Pb::new(raw);
    while let Some((f, w)) = pb.next()? {
        if let (2, Wire::Bytes(g)) = (f, w) {
            let mut gp = Pb::new(g);
            while let Some((gf, _)) = gp.next()? {
                kinds |= match gf {
                    1 | 2 => WANT_NODES,
                    3 => WANT_WAYS,
                    4 => WANT_RELATIONS,
                    _ => 0,
                };
            }
        }
    }
    Ok(kinds)
}

struct BlockCtx<'a> {
    strings: Vec<&'a [u8]>,
    interned: Vec<Option<Str>>,
    granularity: i64,
    date_granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl BlockCtx<'_> {
    fn s(&mut self, idx: u64) -> Result<Str> {
        let i = idx as usize;
        if i >= self.strings.len() {
            return Err(err("pbf: string index out of range"));
        }
        if let Some(s) = &self.interned[i] {
            return Ok(s.clone());
        }
        let s: Str = Str::from(String::from_utf8_lossy(self.strings[i]).as_ref());
        self.interned[i] = Some(s.clone());
        Ok(s)
    }

    fn tags(&mut self, keys: &[u64], vals: &[u64]) -> Result<Tags> {
        if keys.len() != vals.len() {
            return Err(err("pbf: keys/vals length mismatch"));
        }
        let mut t = Vec::with_capacity(keys.len());
        for (k, v) in keys.iter().zip(vals) {
            t.push((self.s(*k)?, self.s(*v)?));
        }
        Ok(Tags::from(t))
    }

    fn coord(&self, off: i64, v: i64) -> i32 {
        ((off + self.granularity * v) / 100) as i32
    }

    fn ts(&self, v: i64) -> i64 {
        v * self.date_granularity / 1000
    }
}

#[derive(Default)]
struct Info {
    version: i64,
    timestamp: i64,
    changeset: i64,
    uid: i64,
    user_sid: u64,
    visible: bool,
}

fn parse_info(b: &[u8]) -> Result<Info> {
    let mut i = Info { version: -1, visible: true, ..Default::default() };
    let mut pb = Pb::new(b);
    while let Some((f, w)) = pb.next()? {
        if let Wire::Varint(v) = w {
            match f {
                1 => i.version = v as i64 as i32 as i64,
                2 => i.timestamp = v as i64,
                3 => i.changeset = v as i64,
                4 => i.uid = v as i64 as i32 as i64,
                5 => i.user_sid = v,
                6 => i.visible = v != 0,
                _ => {}
            }
        }
    }
    Ok(i)
}

fn meta(ctx: &mut BlockCtx, i: &Info) -> Result<RawMeta> {
    Ok(RawMeta {
        version: i.version as u32,
        timestamp: ctx.ts(i.timestamp),
        changeset: i.changeset as u64,
        uid: i.uid as i32 as u32,
        user: ctx.s(i.user_sid)?,
        visible: i.visible,
    })
}

fn collect_packed(b: &[u8]) -> Result<Vec<u64>> {
    packed(b).collect()
}

/// Parses a decompressed primitive block into elements of the wanted kinds.
pub fn parse_block(raw: &[u8], want: u8, out: &mut Vec<Element>) -> Result<()> {
    let mut groups = Vec::new();
    let mut ctx = BlockCtx {
        strings: Vec::new(),
        interned: Vec::new(),
        granularity: 100,
        date_granularity: 1000,
        lat_offset: 0,
        lon_offset: 0,
    };
    let mut pb = Pb::new(raw);
    while let Some((f, w)) = pb.next()? {
        match (f, w) {
            (1, Wire::Bytes(st)) => {
                let mut sp = Pb::new(st);
                while let Some((sf, sw)) = sp.next()? {
                    if let (1, Wire::Bytes(s)) = (sf, sw) {
                        ctx.strings.push(s);
                    }
                }
            }
            (2, Wire::Bytes(g)) => groups.push(g),
            (17, Wire::Varint(v)) => ctx.granularity = v as i64,
            (18, Wire::Varint(v)) => ctx.date_granularity = v as i64,
            (19, Wire::Varint(v)) => ctx.lat_offset = v as i64,
            (20, Wire::Varint(v)) => ctx.lon_offset = v as i64,
            _ => {}
        }
    }
    ctx.interned = vec![None; ctx.strings.len()];
    for g in groups {
        let mut gp = Pb::new(g);
        while let Some((f, w)) = gp.next()? {
            let Wire::Bytes(b) = w else { continue };
            match f {
                1 if want & WANT_NODES != 0 => out.push(Element::Node(parse_node(&mut ctx, b)?)),
                2 if want & WANT_NODES != 0 => parse_dense(&mut ctx, b, out)?,
                3 if want & WANT_WAYS != 0 => out.push(Element::Way(parse_way(&mut ctx, b)?)),
                4 if want & WANT_RELATIONS != 0 => out.push(Element::Relation(parse_rel(&mut ctx, b)?)),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Extracts only node member ids of relations (cheap pass-1 parsing).
pub fn parse_relation_node_refs(raw: &[u8], out: &mut Vec<i64>) -> Result<()> {
    let mut pb = Pb::new(raw);
    while let Some((f, w)) = pb.next()? {
        let (2, Wire::Bytes(g)) = (f, w) else { continue };
        let mut gp = Pb::new(g);
        while let Some((gf, gw)) = gp.next()? {
            let (4, Wire::Bytes(b)) = (gf, gw) else { continue };
            let (mut memids, mut types): (&[u8], &[u8]) = (&[], &[]);
            let mut rp = Pb::new(b);
            while let Some((rf, rw)) = rp.next()? {
                match (rf, rw) {
                    (9, Wire::Bytes(p)) => memids = p,
                    (10, Wire::Bytes(p)) => types = p,
                    _ => {}
                }
            }
            let mut acc = 0i64;
            for (m, t) in packed(memids).zip(packed(types)) {
                acc += zz(m?);
                if t? == 0 {
                    out.push(acc);
                }
            }
        }
    }
    Ok(())
}

fn parse_node(ctx: &mut BlockCtx, b: &[u8]) -> Result<RawNode> {
    let (mut id, mut lat, mut lon) = (0i64, 0i64, 0i64);
    let (mut keys, mut vals) = (Vec::new(), Vec::new());
    let mut info = Info { version: -1, visible: true, ..Default::default() };
    let mut pb = Pb::new(b);
    while let Some((f, w)) = pb.next()? {
        match (f, w) {
            (1, Wire::Varint(v)) => id = zz(v),
            (2, Wire::Bytes(p)) => keys = collect_packed(p)?,
            (3, Wire::Bytes(p)) => vals = collect_packed(p)?,
            (4, Wire::Bytes(p)) => info = parse_info(p)?,
            (8, Wire::Varint(v)) => lat = zz(v),
            (9, Wire::Varint(v)) => lon = zz(v),
            _ => {}
        }
    }
    let tags = ctx.tags(&keys, &vals)?;
    Ok(RawNode {
        id,
        meta: meta(ctx, &info)?,
        tags,
        lon: ctx.coord(ctx.lon_offset, lon),
        lat: ctx.coord(ctx.lat_offset, lat),
    })
}

fn parse_dense(ctx: &mut BlockCtx, b: &[u8], out: &mut Vec<Element>) -> Result<()> {
    let (mut ids, mut lats, mut lons, mut kv): (&[u8], &[u8], &[u8], &[u8]) = (&[], &[], &[], &[]);
    let mut dinfo: &[u8] = &[];
    let mut pb = Pb::new(b);
    while let Some((f, w)) = pb.next()? {
        if let Wire::Bytes(p) = w {
            match f {
                1 => ids = p,
                5 => dinfo = p,
                8 => lats = p,
                9 => lons = p,
                10 => kv = p,
                _ => {}
            }
        }
    }
    let (mut ver, mut ts, mut cs, mut uid, mut usid, mut vis): (&[u8], &[u8], &[u8], &[u8], &[u8], &[u8]) =
        (&[], &[], &[], &[], &[], &[]);
    let mut ip = Pb::new(dinfo);
    while let Some((f, w)) = ip.next()? {
        if let Wire::Bytes(p) = w {
            match f {
                1 => ver = p,
                2 => ts = p,
                3 => cs = p,
                4 => uid = p,
                5 => usid = p,
                6 => vis = p,
                _ => {}
            }
        }
    }
    let ids = collect_packed(ids)?;
    let n = ids.len();
    let lats = collect_packed(lats)?;
    let lons = collect_packed(lons)?;
    if lats.len() != n || lons.len() != n {
        return Err(err("pbf: dense node array length mismatch"));
    }
    let get = |b: &[u8]| -> Result<Vec<u64>> {
        let v = collect_packed(b)?;
        if !v.is_empty() && v.len() != n {
            return Err(err("pbf: dense info length mismatch"));
        }
        Ok(v)
    };
    let (ver, ts, cs, uid, usid, vis) = (get(ver)?, get(ts)?, get(cs)?, get(uid)?, get(usid)?, get(vis)?);
    let mut kvi = packed(kv);
    let (mut id, mut lat, mut lon, mut t, mut c, mut u, mut us) = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    for i in 0..n {
        id += zz(ids[i]);
        lat += zz(lats[i]);
        lon += zz(lons[i]);
        if !ts.is_empty() {
            t += zz(ts[i]);
        }
        if !cs.is_empty() {
            c += zz(cs[i]);
        }
        if !uid.is_empty() {
            u += zz(uid[i]);
        }
        if !usid.is_empty() {
            us += zz(usid[i]);
        }
        let mut tags = Vec::new();
        if !kv.is_empty() {
            loop {
                let k = kvi.next().ok_or_else(|| err("pbf: truncated keys_vals"))??;
                if k == 0 {
                    break;
                }
                let v = kvi.next().ok_or_else(|| err("pbf: truncated keys_vals"))??;
                tags.push((ctx.s(k)?, ctx.s(v)?));
            }
        }
        let info = Info {
            version: if ver.is_empty() { -1 } else { ver[i] as i64 as i32 as i64 },
            timestamp: t,
            changeset: c,
            uid: u,
            user_sid: us as u64,
            visible: vis.is_empty() || vis[i] != 0,
        };
        out.push(Element::Node(RawNode {
            id,
            meta: meta(ctx, &info)?,
            tags: Tags::from(tags),
            lon: ctx.coord(ctx.lon_offset, lon),
            lat: ctx.coord(ctx.lat_offset, lat),
        }));
    }
    Ok(())
}

fn parse_way(ctx: &mut BlockCtx, b: &[u8]) -> Result<RawWay> {
    let mut id = 0i64;
    let (mut keys, mut vals) = (Vec::new(), Vec::new());
    let mut info = Info { version: -1, visible: true, ..Default::default() };
    let mut refs = Vec::new();
    let mut pb = Pb::new(b);
    while let Some((f, w)) = pb.next()? {
        match (f, w) {
            (1, Wire::Varint(v)) => id = v as i64,
            (2, Wire::Bytes(p)) => keys = collect_packed(p)?,
            (3, Wire::Bytes(p)) => vals = collect_packed(p)?,
            (4, Wire::Bytes(p)) => info = parse_info(p)?,
            (8, Wire::Bytes(p)) => {
                let mut acc = 0i64;
                for r in packed(p) {
                    acc += zz(r?);
                    refs.push(acc);
                }
            }
            _ => {}
        }
    }
    let tags = ctx.tags(&keys, &vals)?;
    Ok(RawWay { id, meta: meta(ctx, &info)?, tags, refs })
}

fn parse_rel(ctx: &mut BlockCtx, b: &[u8]) -> Result<RawRel> {
    let mut id = 0i64;
    let (mut keys, mut vals) = (Vec::new(), Vec::new());
    let mut info = Info { version: -1, visible: true, ..Default::default() };
    let (mut roles, mut memids, mut types) = (Vec::new(), Vec::new(), Vec::new());
    let mut pb = Pb::new(b);
    while let Some((f, w)) = pb.next()? {
        match (f, w) {
            (1, Wire::Varint(v)) => id = v as i64,
            (2, Wire::Bytes(p)) => keys = collect_packed(p)?,
            (3, Wire::Bytes(p)) => vals = collect_packed(p)?,
            (4, Wire::Bytes(p)) => info = parse_info(p)?,
            (8, Wire::Bytes(p)) => roles = collect_packed(p)?,
            (9, Wire::Bytes(p)) => {
                let mut acc = 0i64;
                for r in packed(p) {
                    acc += zz(r?);
                    memids.push(acc);
                }
            }
            (10, Wire::Bytes(p)) => types = collect_packed(p)?,
            _ => {}
        }
    }
    if roles.len() != memids.len() || types.len() != memids.len() {
        return Err(err("pbf: relation member arrays length mismatch"));
    }
    let mut members = Vec::with_capacity(memids.len());
    for i in 0..memids.len() {
        let kind = ObjType::from_u8(types[i] as u8).ok_or_else(|| err("pbf: bad member type"))?;
        members.push(Member { kind, id: memids[i], role: ctx.s(roles[i])? });
    }
    let tags = ctx.tags(&keys, &vals)?;
    Ok(RawRel { id, meta: meta(ctx, &info)?, tags, members })
}

// ---------------------------------------------------------------------------------------
// Source

/// A memory-mapped PBF file split into node / way / relation blob ranges.
pub struct PbfFile {
    mmap: Mmap,
    blobs: Vec<BlobRef>,
    node_range: Range<usize>,
    way_range: Range<usize>,
    rel_range: Range<usize>,
    batch: usize,
    /// Memory allowed for one batch of decoded relation blobs (0 = blob-count default).
    rel_batch_bytes: usize,
}

impl PbfFile {
    pub fn open(path: &Path) -> Result<PbfFile> {
        let f = std::fs::File::open(path).map_err(|e| err(format!("open {}: {e}", path.display())))?;
        // SAFETY: the file is opened read-only; concurrent modification is undefined
        // behaviour we accept for an input file (documented).
        let mmap = unsafe { Mmap::map(&f) }.map_err(|e| err(format!("mmap: {e}")))?;
        let blobs = index_blobs(&mmap)?;
        let kinds = |i: usize| -> u8 {
            decompress(&mmap[blobs[i].data_off..blobs[i].data_off + blobs[i].data_len])
                .and_then(|r| group_kinds(&r))
                .unwrap_or(0)
        };
        let n = blobs.len();
        // Files are sorted by type then id: nodes < ways < relations.
        let first_way = partition_point(n, |i| kinds(i) & (WANT_WAYS | WANT_RELATIONS) == 0);
        let first_rel = partition_point(n, |i| kinds(i) & WANT_RELATIONS == 0);
        Ok(PbfFile {
            node_range: 0..(first_way + 1).min(n),
            way_range: first_way..(first_rel + 1).min(n),
            rel_range: first_rel..n,
            mmap,
            blobs,
            batch: rayon::current_num_threads() * 2,
            rel_batch_bytes: 0,
        })
    }

    /// Limits the memory of one batch of decoded relation blobs (relation blobs expand
    /// roughly 800x when decoded because member lists repeat across versions).
    pub fn set_relation_batch_bytes(&mut self, bytes: usize) {
        self.rel_batch_bytes = bytes;
    }

    pub fn n_blobs(&self) -> usize {
        self.blobs.len()
    }

    /// (blob count, compressed bytes) of the node, way and relation ranges.
    pub fn range_sizes(&self) -> [(usize, usize); 3] {
        let f = |r: &Range<usize>| (r.len(), self.blobs[r.clone()].iter().map(|b| b.data_len).sum());
        [f(&self.node_range), f(&self.way_range), f(&self.rel_range)]
    }

    pub fn file_len(&self) -> usize {
        self.mmap.len()
    }

    fn run(&self, range: Range<usize>, want: u8, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        let blobs = &self.blobs[range];
        // Relation blobs expand enormously when decoded (repeated member lists), so they
        // are processed in small batches, sized by the memory budget if one is set.
        let batch = match want {
            WANT_NODES => self.batch * 2,
            WANT_WAYS => self.batch,
            _ => (self.batch / 8).max(2),
        };
        let mut chunks: Vec<&[BlobRef]> = Vec::new();
        if want == WANT_RELATIONS && self.rel_batch_bytes > 0 {
            let mut start = 0;
            let mut est = 0usize;
            for (i, b) in blobs.iter().enumerate() {
                let e = b.data_len.saturating_mul(800);
                if i > start && (est + e > self.rel_batch_bytes || i - start >= batch) {
                    chunks.push(&blobs[start..i]);
                    start = i;
                    est = 0;
                }
                est += e;
            }
            if start < blobs.len() {
                chunks.push(&blobs[start..]);
            }
        } else {
            chunks.extend(blobs.chunks(batch.max(1)));
        }
        for chunk in chunks {
            let parts: Vec<Result<Vec<Element>>> = chunk
                .par_iter()
                .map(|b| {
                    let raw = decompress(&self.mmap[b.data_off..b.data_off + b.data_len])?;
                    let mut v = Vec::new();
                    parse_block(&raw, want, &mut v)?;
                    Ok(v)
                })
                .collect();
            let mut all = Vec::new();
            for p in parts {
                all.extend(p?);
            }
            if !all.is_empty() {
                f(all)?;
            }
        }
        Ok(())
    }
}

fn partition_point(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

impl crate::Source for PbfFile {
    fn relation_node_members(&self, f: &mut dyn FnMut(Vec<i64>) -> Result<()>) -> Result<()> {
        for chunk in self.blobs[self.rel_range.clone()].chunks(self.batch.max(1)) {
            let parts: Vec<Result<Vec<i64>>> = chunk
                .par_iter()
                .map(|b| {
                    let raw = decompress(&self.mmap[b.data_off..b.data_off + b.data_len])?;
                    let mut v = Vec::new();
                    parse_relation_node_refs(&raw, &mut v)?;
                    Ok(v)
                })
                .collect();
            for p in parts {
                f(p?)?;
            }
        }
        Ok(())
    }

    fn nodes(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        self.run(self.node_range.clone(), WANT_NODES, f)
    }
    fn ways(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        self.run(self.way_range.clone(), WANT_WAYS, f)
    }
    fn relations(&self, f: &mut dyn FnMut(Vec<Element>) -> Result<()>) -> Result<()> {
        self.run(self.rel_range.clone(), WANT_RELATIONS, f)
    }
}
