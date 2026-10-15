//! Block encoding/decoding (SPEC.md §6): columnar directory + data frames, zstd framing.

use crate::bloom::{self, Bloom};
use crate::codec::*;
use crate::diff;
use crate::error::corrupt;
use crate::layout::*;
use crate::model::*;
use crate::{Error, Result};
use rustc_hash::FxHashMap as HashMap;
use xxhash_rust::xxh3::xxh3_64;

/// Maximum decompressed frame size accepted by the reader.
pub const MAX_FRAME_RAW: u32 = 256 << 20;

/// Global dictionaries used while encoding. Strings not found go into the block table.
pub trait EncodeDicts: Sync {
    fn key(&self, s: &str) -> Option<u32>;
    fn value(&self, s: &str) -> Option<u32>;
    fn role(&self, s: &str) -> Option<u32>;
}

/// Encoder dictionaries that store every string locally.
pub struct NoDicts;

impl EncodeDicts for NoDicts {
    fn key(&self, _: &str) -> Option<u32> {
        None
    }
    fn value(&self, _: &str) -> Option<u32> {
        None
    }
    fn role(&self, _: &str) -> Option<u32> {
        None
    }
}

/// Global dictionaries used while decoding.
#[derive(Clone, Debug, Default)]
pub struct DecodeDicts {
    pub keys: Vec<Str>,
    pub values: Vec<Str>,
    pub roles: Vec<Str>,
}

#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    pub time_base: i64,
    pub keyframe_interval: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct DecodeOptions {
    pub time_base: i64,
}

/// Encoded but uncompressed block.
#[derive(Clone, Debug)]
pub struct RawBlock {
    pub obj_type: ObjType,
    pub n_segments: u32,
    pub dir: Vec<u8>,
    /// Attribute frame.
    pub data: Vec<u8>,
    /// Geometry frame.
    pub geom: Vec<u8>,
    pub bbox: BBox,
    pub tmin: u32,
    pub tmax: u32,
    pub bloom: Bloom,
    /// Distinct key and key=value hashes of the block, for its sized filter.
    pub hashes: Vec<u64>,
    pub large: bool,
}

/// Per-segment directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegDir {
    pub id: i64,
    pub flags: u8,
    pub tmin: u32,
    pub tmax: u32,
    pub bbox: BBox,
    pub n_versions: u32,
    pub n_slots: u32,
}

impl SegDir {
    /// True if the segment's validity interval contains `t` (half open, last inclusive).
    #[inline]
    pub fn valid_at(&self, t: u32) -> bool {
        self.tmin <= t && (self.tmax == T_OPEN || t < self.tmax)
    }

    /// True if `[t0, t1]` overlaps the segment's interval.
    #[inline]
    pub fn overlaps(&self, t0: u32, t1: u32) -> bool {
        self.tmin <= t1 && t0 <= self.tmax
    }
}

/// Values of `tmin`, `tmax` and (for nodes) `bbox` implied by a segment's records.
/// The directory stores these fields only where they differ (SPEC §6.2).
pub fn derived_fields(body: &SegmentBody, time_base: i64) -> (Option<u32>, u32, BBox) {
    let mut tmin: Option<i64> = None;
    let mut tmax: Option<i64> = None;
    let mut see = |t: i64| {
        tmin = Some(tmin.map_or(t, |m| m.min(t)));
        tmax = Some(tmax.map_or(t, |m| m.max(t)));
    };
    let mut bbox = BBox::EMPTY;
    
    let mut private_alive = false;
    let last_visible = match body {
        SegmentBody::Node(vs) => {
            for v in vs {
                if !v.meta.continuation {
                    see(v.meta.timestamp);
                }
                if v.meta.visible {
                    bbox.extend(v.lon, v.lat);
                }
            }
            vs.last().is_some_and(|v| v.meta.visible)
        }
        SegmentBody::Way { slots, versions } => {
            for v in versions.iter().filter(|v| !v.meta.continuation) {
                see(v.meta.timestamp);
            }
            for es in slots.iter().filter_map(|s| s.private.as_ref()) {
                for e in es.iter().filter(|e| !e.continuation) {
                    see(e.timestamp);
                }
                private_alive |= es.last().is_some_and(|e| e.visible);
            }
            versions.last().is_some_and(|v| v.meta.visible)
        }
        SegmentBody::Relation(vs) => {
            for v in vs.iter().filter(|v| !v.meta.continuation) {
                see(v.meta.timestamp);
            }
            vs.last().is_some_and(|v| v.meta.visible)
        }
    };
    let rmin = tmin.map(|t| rel_time(t, time_base));
    let rmax = match tmax {
        Some(t) if !last_visible && !private_alive => rel_time(t, time_base).max(rmin.unwrap_or(0)),
        _ => T_OPEN,
    };
    (rmin, rmax, bbox)
}

const VF_VISIBLE: u64 = 1;
const VF_CONT: u64 = 2;

// ---------------------------------------------------------------------------------------
// Encoding

struct StrTable<'s> {
    map: HashMap<&'s str, u32>,
    list: Vec<&'s str>,
}

impl<'s> StrTable<'s> {
    fn local(&mut self, s: &'s str) -> u64 {
        let next = self.list.len() as u32;
        let id = *self.map.entry(s).or_insert_with(|| next);
        if id == next {
            self.list.push(s);
        }
        ((id as u64) << 1) | 1
    }
}

struct Enc<'s, 'd> {
    dicts: &'d dyn EncodeDicts,
    opt: EncodeOptions,
    strs: StrTable<'s>,
    tagsets: HashMap<Vec<(u64, u64)>, u32>,
    tagset_list: Vec<Vec<(u64, u64)>>,
    bloom: Bloom,
    hashes: Vec<u64>,
    uids: Vec<u32>,
    /// (changeset, uid) -> (table index, base timestamp)
    cs_index: HashMap<(u64, u32), (u32, i64)>,
    // version columns
    c_ver: Vec<u64>,
    c_ts: Vec<u64>,
    c_cs: Vec<u64>,
    c_vf: Vec<u64>,
    c_tagset: Vec<u64>,
}

impl<'s, 'd> Enc<'s, 'd> {
    fn key_ref(&mut self, s: &'s str) -> u64 {
        match self.dicts.key(s) {
            Some(id) => (id as u64) << 1,
            None => self.strs.local(s),
        }
    }

    fn value_ref(&mut self, s: &'s str) -> u64 {
        match self.dicts.value(s) {
            Some(id) => (id as u64) << 1,
            None => self.strs.local(s),
        }
    }

    fn role_ref(&mut self, s: &'s str) -> u64 {
        match self.dicts.role(s) {
            Some(id) => (id as u64) << 1,
            None => self.strs.local(s),
        }
    }

    fn uid_idx(&self, uid: u32) -> u64 {
        self.uids.binary_search(&uid).expect("uid collected") as u64
    }

    fn cs(&self, cs: u64, uid: u32) -> (u64, i64) {
        let (i, base) = self.cs_index[&(cs, uid)];
        (i as u64, base)
    }

    /// Returns the tagset reference coded as distance from the next new tagset id.
    fn tagset(&mut self, tags: &'s Tags) -> u64 {
        let next_new = self.tagset_list.len() as u64;
        next_new - self.tagset_id(tags)
    }

    fn tagset_id(&mut self, tags: &'s Tags) -> u64 {
        let mut refs = Vec::with_capacity(tags.len());
        for (k, v) in tags.iter() {
            let kr = self.key_ref(k);
            let vr = self.value_ref(v);
            refs.push((kr, vr));
        }
        if let Some(&id) = self.tagsets.get(&refs) {
            return id as u64;
        }
        for &(kr, vr) in &refs {
            if kr & 1 == 0 {
                let kid = (kr >> 1) as u32;
                bloom::add(&mut self.bloom, bloom::hash_key(kid));
                self.hashes.push(bloom::hash_key(kid));
                if vr & 1 == 0 {
                    bloom::add(&mut self.bloom, bloom::hash_kv(kid, (vr >> 1) as u32));
                    self.hashes.push(bloom::hash_kv(kid, (vr >> 1) as u32));
                }
            }
        }
        let id = self.tagset_list.len() as u32;
        self.tagsets.insert(refs.clone(), id);
        self.tagset_list.push(refs);
        id as u64
    }

    fn meta(&mut self, m: &Meta, tags: &'s Tags, prev_ver: &mut i64) {
        self.c_ver.push(zigzag(m.version as i64 - *prev_ver));
        *prev_ver = m.version as i64;
        let (ci, base) = self.cs(m.changeset, m.uid);
        self.c_ts.push((m.timestamp - base) as u64);
        self.c_cs.push(ci);
        self.c_vf.push(if m.visible { VF_VISIBLE } else { 0 } | if m.continuation { VF_CONT } else { 0 });
        let ts = self.tagset(tags);
        self.c_tagset.push(ts);
    }
}

/// Encodes a list as keyframe or diff (SPEC §6.3) into `kind`, `nums` and `items`.
struct ListEnc {
    kind: Vec<u64>,
    nums: Vec<u64>,
    since_key: u32,
}

impl ListEnc {
    fn new() -> Self {
        ListEnc { kind: Vec::new(), nums: Vec::new(), since_key: 0 }
    }

    /// Returns the range of `cur` that must be emitted as items, plus whether it is a diff.
    fn choose<T: PartialEq>(&mut self, prev: Option<&[T]>, cur: &[T], interval: u32) -> (usize, usize, bool) {
        if let Some(prev) = prev
            && self.since_key + 1 < interval {
                let (p, d, ins) = diff::diff(prev, cur);
                if 3 + ins.len() < 1 + cur.len() {
                    self.kind.push(1);
                    self.nums.extend([p as u64, d as u64, ins.len() as u64]);
                    self.since_key += 1;
                    return (p, p + ins.len(), true);
                }
            }
        self.kind.push(0);
        self.nums.push(cur.len() as u64);
        self.since_key = 0;
        (0, cur.len(), false)
    }
}

/// Encodes segments of one object type into a raw (uncompressed) block.
pub fn encode_block(segs: &[Segment], dicts: &dyn EncodeDicts, opt: &EncodeOptions) -> Result<RawBlock> {
    let obj_type = segs.first().map(|s| s.obj_type()).ok_or_else(|| corrupt("empty block"))?;
    let mut cs_base: HashMap<(u64, u32), i64> = HashMap::default();
    let mut add_cs = |cs: u64, uid: u32, ts: i64| {
        let e = cs_base.entry((cs, uid)).or_insert(ts);
        *e = (*e).min(ts);
    };
    for s in segs {
        if s.obj_type() != obj_type {
            return Err(corrupt("mixed object types in block"));
        }
        match &s.body {
            SegmentBody::Node(vs) => vs.iter().for_each(|v| add_cs(v.meta.changeset, v.meta.uid, v.meta.timestamp)),
            SegmentBody::Way { slots, versions } => {
                versions.iter().for_each(|v| add_cs(v.meta.changeset, v.meta.uid, v.meta.timestamp));
                for sl in slots {
                    if let Some(es) = &sl.private {
                        es.iter().for_each(|e| add_cs(e.changeset, e.uid, e.timestamp));
                    }
                }
            }
            SegmentBody::Relation(vs) => {
                vs.iter().for_each(|v| add_cs(v.meta.changeset, v.meta.uid, v.meta.timestamp))
            }
        }
    }
    let mut cs_table: Vec<((u64, u32), i64)> = cs_base.into_iter().collect();
    cs_table.sort_unstable();
    let mut uids: Vec<u32> = cs_table.iter().map(|((_, u), _)| *u).collect();
    uids.sort_unstable();
    uids.dedup();
    let cs_index: HashMap<(u64, u32), (u32, i64)> =
        cs_table.iter().enumerate().map(|(i, (k, b))| (*k, (i as u32, *b))).collect();

    let mut e = Enc {
        dicts,
        opt: *opt,
        strs: StrTable { map: HashMap::default(), list: Vec::new() },
        tagsets: HashMap::default(),
        tagset_list: Vec::new(),
        bloom: [0; bloom::BLOOM_WORDS],
        hashes: Vec::new(),
        uids,
        cs_index,
        c_ver: Vec::new(),
        c_ts: Vec::new(),
        c_cs: Vec::new(),
        c_vf: Vec::new(),
        c_tagset: Vec::new(),
    };

    // directory columns
    let n = segs.len();
    let (mut d_id, mut d_flags, mut d_nv, mut d_ns) =
        (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut d_tmin, mut d_tmax) = (Vec::new(), Vec::new());
    let (mut d_minx, mut d_miny, mut d_w, mut d_h) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut prev_id, mut prev_minx, mut prev_miny) = (0i64, 0i64, 0i64);

    // type specific columns
    let (mut n_lon, mut n_lat) = (Vec::new(), Vec::new());
    let (mut prev_lon, mut prev_lat) = (0i64, 0i64);
    let (mut w_node, mut w_kind, mut w_t, mut w_f) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut w_lon, mut w_lat, mut w_ver, mut w_cs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut nl_items = Vec::new();
    let mut nl = ListEnc::new();
    let mut prev_slot_id = 0i64;
    let mut rm = ListEnc::new();
    let (mut m_type, mut m_ref, mut m_role) = (Vec::new(), Vec::new(), Vec::new());
    let mut prev_mref = 0i64;

    let mut bbox = BBox::EMPTY;
    let mut tmin = T_OPEN;
    let mut tmax = 0u32;
    let mut large = false;
    let kf = opt.keyframe_interval.max(1);

    for s in segs {
        if s.tmax != T_OPEN && s.tmax < s.tmin {
            return Err(corrupt("segment tmax < tmin"));
        }
        let (dmin, dmax, dbox) = derived_fields(&s.body, opt.time_base);
        let mut flags = s.flags & !SEG_LAYOUT_FLAGS;
        if s.bbox.is_empty() {
            flags |= SEG_NO_BBOX;
        }
        if dmin != Some(s.tmin) {
            flags |= SEG_EXPLICIT_TMIN;
            d_tmin.push(s.tmin as u64);
        }
        if dmax != s.tmax {
            flags |= SEG_EXPLICIT_TMAX;
            d_tmax.push(s.tmax as u64);
        }
        let store_bbox = !s.bbox.is_empty() && (s.obj_type() != ObjType::Node || dbox != s.bbox);
        if s.obj_type() == ObjType::Node && s.bbox.is_empty() && !dbox.is_empty() {
            return Err(corrupt("node segment without bbox has visible positions"));
        }
        if store_bbox {
            flags |= SEG_STORED_BBOX;
            d_minx.push(zigzag(s.bbox.minx as i64 - prev_minx));
            d_miny.push(zigzag(s.bbox.miny as i64 - prev_miny));
            prev_minx = s.bbox.minx as i64;
            prev_miny = s.bbox.miny as i64;
            d_w.push(s.bbox.width() as u64);
            d_h.push(s.bbox.height() as u64);
        }
        large |= flags & SEG_LARGE != 0;
        d_id.push(zigzag(s.id.wrapping_sub(prev_id)));
        prev_id = s.id;
        d_flags.push(flags as u64);
        d_nv.push(s.body.n_versions() as u64);
        bbox.union(&s.bbox);
        tmin = tmin.min(s.tmin);
        tmax = tmax.max(s.tmax);

        let mut prev_ver = 0i64;
        match &s.body {
            SegmentBody::Node(vs) => {
                d_ns.push(0);
                for v in vs {
                    e.meta(&v.meta, &v.tags, &mut prev_ver);
                    if v.meta.visible {
                        n_lon.push(zigzag(v.lon as i64 - prev_lon));
                        n_lat.push(zigzag(v.lat as i64 - prev_lat));
                        prev_lon = v.lon as i64;
                        prev_lat = v.lat as i64;
                    }
                }
            }
            SegmentBody::Way { slots, versions } => {
                d_ns.push(slots.len() as u64);
                for sl in slots {
                    w_node.push(zigzag(sl.node_id.wrapping_sub(prev_slot_id)));
                    prev_slot_id = sl.node_id;
                    match &sl.private {
                        None => w_kind.push(0),
                        Some(es) => {
                            w_kind.push(1 + es.len() as u64);
                            let mut pv = 0i64;
                            for pe in es {
                                let (ci, base) = e.cs(pe.changeset, pe.uid);
                                w_t.push((pe.timestamp - base) as u64);
                                w_cs.push(ci);
                                w_f.push(
                                    if pe.visible { VF_VISIBLE } else { 0 } | if pe.continuation { VF_CONT } else { 0 },
                                );
                                if pe.visible {
                                    w_lon.push(zigzag(pe.lon as i64 - prev_lon));
                                    w_lat.push(zigzag(pe.lat as i64 - prev_lat));
                                    prev_lon = pe.lon as i64;
                                    prev_lat = pe.lat as i64;
                                }
                                w_ver.push(zigzag(pe.version as i64 - pv));
                                pv = pe.version as i64;
                            }
                        }
                    }
                }
                let mut prev_list: Option<&[u32]> = None;
                nl.since_key = 0;
                for v in versions {
                    if let Some(&bad) = v.nodes.iter().find(|&&i| i as usize >= slots.len()) {
                        return Err(corrupt(format!("way {} slot index {bad} out of range", s.id)));
                    }
                    e.meta(&v.meta, &v.tags, &mut prev_ver);
                    let (a, b, is_diff) = nl.choose(prev_list, &v.nodes, kf);
                    let mut prev_item: i64 = if is_diff && a > 0 { prev_list.unwrap()[a - 1] as i64 } else { -1 };
                    for &it in &v.nodes[a..b] {
                        nl_items.push(zigzag(it as i64 - prev_item));
                        prev_item = it as i64;
                    }
                    prev_list = Some(&v.nodes);
                }
            }
            SegmentBody::Relation(vs) => {
                d_ns.push(0);
                let mut prev_list: Option<&[Member]> = None;
                rm.since_key = 0;
                for v in vs {
                    e.meta(&v.meta, &v.tags, &mut prev_ver);
                    let (a, b, _) = rm.choose(prev_list, &v.members, kf);
                    for m in &v.members[a..b] {
                        m_type.push(m.kind as u64);
                        m_ref.push(zigzag(m.id.wrapping_sub(prev_mref)));
                        prev_mref = m.id;
                        let r = e.role_ref(&m.role);
                        m_role.push(r);
                    }
                    prev_list = Some(&v.members);
                }
            }
        }
    }

    let mut dir = Vec::new();
    for c in [&d_id, &d_flags, &d_nv, &d_ns, &d_tmin, &d_tmax, &d_minx, &d_miny, &d_w, &d_h] {
        put_u64_column(&mut dir, c);
    }

    let mut data = Vec::new();
    let mut prev = 0u64;
    let uid_deltas: Vec<u64> = e
        .uids
        .iter()
        .map(|&u| {
            let d = u as u64 - prev;
            prev = u as u64;
            d
        })
        .collect();
    put_u64_column(&mut data, &uid_deltas);
    let lens: Vec<u64> = e.strs.list.iter().map(|s| s.len() as u64).collect();
    put_u64_column(&mut data, &lens);
    let bytes: Vec<u8> = e.strs.list.iter().flat_map(|s| s.as_bytes().iter().copied()).collect();
    put_bytes_column(&mut data, &bytes);
    let ts_lens: Vec<u64> = e.tagset_list.iter().map(|t| t.len() as u64).collect();
    let ts_keys: Vec<u64> = e.tagset_list.iter().flat_map(|t| t.iter().map(|x| x.0)).collect();
    let ts_vals: Vec<u64> = e.tagset_list.iter().flat_map(|t| t.iter().map(|x| x.1)).collect();
    let mut cs_cs = Vec::with_capacity(cs_table.len());
    let mut cs_uid = Vec::with_capacity(cs_table.len());
    let mut cs_ts = Vec::with_capacity(cs_table.len());
    let (mut pcs, mut pts) = (0u64, opt.time_base);
    for ((cs, uid), base) in &cs_table {
        cs_cs.push(cs - pcs);
        pcs = *cs;
        cs_uid.push(e.uid_idx(*uid));
        cs_ts.push(zigzag(base.wrapping_sub(pts)));
        pts = *base;
    }
    for c in [&ts_lens, &ts_keys, &ts_vals, &cs_cs, &cs_uid, &cs_ts, &e.c_ver, &e.c_ts, &e.c_cs, &e.c_vf, &e.c_tagset] {
        put_u64_column(&mut data, c);
    }
    let mut geom = Vec::new();
    match obj_type {
        ObjType::Node => {
            put_u64_column(&mut geom, &n_lon);
            put_u64_column(&mut geom, &n_lat);
        }
        ObjType::Way => {
            for c in [
                &w_node, &w_kind, &w_t, &w_f, &w_lon, &w_lat, &w_ver, &w_cs, &nl.kind, &nl.nums, &nl_items,
            ] {
                put_u64_column(&mut geom, c);
            }
        }
        ObjType::Relation => {
            for c in [&rm.kind, &rm.nums, &m_type, &m_ref, &m_role] {
                put_u64_column(&mut geom, c);
            }
        }
    }

    let _ = e.opt;
    Ok(RawBlock {
        obj_type,
        n_segments: n as u32,
        dir,
        data,
        geom,
        bbox,
        tmin,
        tmax,
        bloom: e.bloom,
        hashes: {
            let mut h = e.hashes;
            h.sort_unstable();
            h.dedup();
            h
        },
        large,
    })
}

// ---------------------------------------------------------------------------------------
// Framing

/// Compresses a raw block and returns the complete block bytes (header, frames, padding).
pub fn frame_block(
    raw: &RawBlock,
    level: i32,
    cdict: Option<&zstd::dict::EncoderDictionary<'_>>,
) -> Result<Vec<u8>> {
    let comp = |data: &[u8]| -> Result<Vec<u8>> {
        let r = match cdict {
            Some(d) => zstd::bulk::Compressor::with_prepared_dictionary(d).and_then(|mut c| c.compress(data)),
            None => zstd::bulk::compress(data, level),
        };
        r.map_err(|e| Error::Compression(e.to_string()))
    };
    let dir_c = comp(&raw.dir)?;
    let data_c = comp(&raw.data)?;
    let geom_c = comp(&raw.geom)?;
    let mut body = Vec::with_capacity(dir_c.len() + data_c.len() + geom_c.len());
    body.extend_from_slice(&dir_c);
    body.extend_from_slice(&data_c);
    body.extend_from_slice(&geom_c);
    let mut flags = 0u8;
    if cdict.is_some() {
        flags |= BLOCK_FLAG_DICT;
    }
    if raw.large {
        flags |= BLOCK_FLAG_LARGE;
    }
    let h = BlockHeader {
        obj_type: raw.obj_type,
        flags,
        n_segments: raw.n_segments,
        dir_comp: dir_c.len() as u32,
        dir_raw: raw.dir.len() as u32,
        data_comp: data_c.len() as u32,
        data_raw: raw.data.len() as u32,
        geom_comp: geom_c.len() as u32,
        geom_raw: raw.geom.len() as u32,
        tmin: raw.tmin,
        tmax: raw.tmax,
        bbox: raw.bbox,
        checksum: xxh3_64(&body),
    };
    let mut out = Vec::with_capacity(BLOCK_HEADER_SIZE + body.len() + 8);
    out.extend_from_slice(&h.to_bytes());
    out.extend_from_slice(&body);
    pad8(&mut out);
    Ok(out)
}

/// A parsed but not yet decompressed block.
#[derive(Clone, Copy, Debug)]
pub struct BlockFrame<'a> {
    pub header: BlockHeader,
    pub dir: &'a [u8],
    pub data: &'a [u8],
    pub geom: &'a [u8],
}

pub fn parse_block_frame(buf: &[u8], verify: bool) -> Result<BlockFrame<'_>> {
    let header = BlockHeader::parse(buf)?;
    let (dc, ac, gc) = (header.dir_comp as usize, header.data_comp as usize, header.geom_comp as usize);
    let dir = le::slice(buf, BLOCK_HEADER_SIZE, dc)?;
    let data = le::slice(buf, BLOCK_HEADER_SIZE + dc, ac)?;
    let geom = le::slice(buf, BLOCK_HEADER_SIZE + dc + ac, gc)?;
    if verify {
        let body = le::slice(buf, BLOCK_HEADER_SIZE, dc + ac + gc)?;
        if xxh3_64(body) != header.checksum {
            return Err(Error::Checksum("block".into()));
        }
    }
    Ok(BlockFrame { header, dir, data, geom })
}

pub fn decompress(comp: &[u8], raw_len: u32, ddict: Option<&zstd::dict::DecoderDictionary<'_>>) -> Result<Vec<u8>> {
    if raw_len > MAX_FRAME_RAW {
        return Err(corrupt("frame too large"));
    }
    let r = match ddict {
        Some(d) => zstd::bulk::Decompressor::with_prepared_dictionary(d)
            .and_then(|mut dc| dc.decompress(comp, raw_len as usize)),
        None => zstd::bulk::decompress(comp, raw_len as usize),
    };
    let out = r.map_err(|e| Error::Compression(e.to_string()))?;
    if out.len() != raw_len as usize {
        return Err(corrupt("decompressed size mismatch"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Decoding

/// Directory entry as stored; derived fields are `None` until the data is decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub id: i64,
    pub flags: u8,
    pub n_versions: u32,
    pub n_slots: u32,
    pub tmin: Option<u32>,
    pub tmax: Option<u32>,
    pub bbox: Option<BBox>,
}

pub fn decode_directory(dir: &[u8], n_segments: u32) -> Result<Vec<DirEntry>> {
    let mut p = 0;
    let mut budget = frame_budget(dir.len());
    let mut cols: Vec<Col> = Vec::with_capacity(10);
    for name in ["id", "flags", "nv", "ns", "tmin", "tmax", "minx", "miny", "w", "h"] {
        cols.push(Col::read_budget(dir, &mut p, name, &mut budget)?);
    }
    for c in &cols[..4] {
        if c.len() != n_segments as usize {
            return Err(corrupt("directory column length"));
        }
    }
    let mut out = Vec::with_capacity(n_segments as usize);
    let (mut prev_id, mut prev_minx, mut prev_miny) = (0i64, 0i64, 0i64);
    for _ in 0..n_segments {
        let id = prev_id.wrapping_add(cols[0].next_i()?);
        prev_id = id;
        let flags = cols[1].next()? as u8;
        let n_versions = cols[2].next_u32()?;
        let n_slots = cols[3].next_u32()?;
        let tmin = if flags & SEG_EXPLICIT_TMIN != 0 { Some(cols[4].next_u32()?) } else { None };
        let tmax = if flags & SEG_EXPLICIT_TMAX != 0 { Some(cols[5].next_u32()?) } else { None };
        let bbox = if flags & SEG_STORED_BBOX != 0 {
            let minx = prev_minx + cols[6].next_i()?;
            let miny = prev_miny + cols[7].next_i()?;
            prev_minx = minx;
            prev_miny = miny;
            let w = cols[8].next()? as i64;
            let h = cols[9].next()? as i64;
            let to32 = |v: i64| i32::try_from(v).map_err(|_| corrupt("bbox range"));
            Some(BBox { minx: to32(minx)?, miny: to32(miny)?, maxx: to32(minx + w)?, maxy: to32(miny + h)? })
        } else if flags & SEG_NO_BBOX != 0 {
            Some(BBox::EMPTY)
        } else {
            None
        };
        out.push(DirEntry { id, flags, n_versions, n_slots, tmin, tmax, bbox });
    }
    Ok(out)
}

fn resolve(r: u64, global: &[Str], local: &[Str]) -> Result<Str> {
    let id = (r >> 1) as usize;
    let tbl = if r & 1 == 0 { global } else { local };
    tbl.get(id).cloned().ok_or_else(|| corrupt("string reference out of range"))
}

struct Common {
    local: Vec<Str>,
    tagsets: Vec<Tags>,
    /// (changeset, uid, base timestamp)
    cs_table: Vec<(u64, u32, i64)>,
    ver: Col,
    ts: Col,
    cs: Col,
    vf: Col,
    tagset: Col,
    tagsets_assigned: usize,
}

impl Common {
    fn cs(&self, idx: u64, ts_off: u64) -> Result<(u64, u32, i64)> {
        let &(cs, uid, base) = self.cs_table.get(idx as usize).ok_or_else(|| corrupt("changeset index"))?;
        Ok((cs, uid, base.wrapping_add(ts_off as i64)))
    }

    fn meta(&mut self, prev_ver: &mut i64) -> Result<(Meta, Tags)> {
        let (m, ti) = self.meta_ti(prev_ver)?;
        Ok((m, self.tagsets[ti].clone()))
    }

    fn meta_ti(&mut self, prev_ver: &mut i64) -> Result<(Meta, usize)> {
        let version = (*prev_ver).wrapping_add(self.ver.next_i()?);
        *prev_ver = version;
        let ts_off = self.ts.next()?;
        let ci = self.cs.next()?;
        let (cs, uid, timestamp) = self.cs(ci, ts_off)?;
        let vf = self.vf.next()?;
        let back = self.tagset.next()? as usize;
        let ti = if back == 0 {
            self.tagsets_assigned += 1;
            self.tagsets_assigned - 1
        } else {
            self.tagsets_assigned.checked_sub(back).ok_or_else(|| corrupt("tagset back reference"))?
        };
        if ti >= self.tagsets.len() {
            return Err(corrupt("tagset index"));
        }
        Ok((
            Meta {
                version: version as u32,
                timestamp,
                changeset: cs,
                uid,
                visible: vf & VF_VISIBLE != 0,
                continuation: vf & VF_CONT != 0,
            },
            ti,
        ))
    }
}

fn decode_list<T: Clone>(
    kind: &mut Col,
    nums: &mut Col,
    prev: Option<&Vec<T>>,
    mut item: impl FnMut(Option<&T>) -> Result<T>,
) -> Result<Vec<T>> {
    let k = kind.next()?;
    match k {
        0 => {
            let len = nums.next()? as usize;
            if len > MAX_COLUMN_LEN {
                return Err(corrupt("list length"));
            }
            let mut out = Vec::with_capacity(len.min(1 << 16));
            for _ in 0..len {
                let it = item(out.last())?;
                out.push(it);
            }
            Ok(out)
        }
        1 => {
            let prev = prev.ok_or_else(|| corrupt("diff without previous list"))?;
            let p = nums.next()? as usize;
            let d = nums.next()? as usize;
            let ins_len = nums.next()? as usize;
            if p > prev.len() || ins_len > MAX_COLUMN_LEN {
                return Err(corrupt("list diff range"));
            }
            let mut ins: Vec<T> = Vec::with_capacity(ins_len.min(1 << 16));
            for _ in 0..ins_len {
                let last = ins.last().or(if p > 0 { prev.get(p - 1) } else { None });
                let it = item(last)?;
                ins.push(it);
            }
            diff::apply(prev, p, d, ins)
        }
        _ => Err(corrupt("list kind")),
    }
}

#[derive(Clone, Copy, Debug)]
struct ListOp {
    key: bool,
    prefix: usize,
    del: usize,
    start: usize,
    len: usize,
}

/// Lazily materialized relation member lists of a block. Version indexes are global
/// within the block (segments in directory order, versions in order).
#[derive(Clone, Debug)]
pub struct RelLists {
    ops: Vec<ListOp>,
    types: Vec<u8>,
    refs: Vec<i64>,
    roles: Vec<u64>,
    local: Vec<Str>,
    dict_roles: Vec<Str>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MRef {
    kind: u8,
    id: i64,
    role: u64,
}

impl RelLists {
    fn apply_raw(&self, prev: &[MRef], op: &ListOp) -> Result<Vec<MRef>> {
        let ins: Vec<MRef> = (op.start..op.start + op.len)
            .map(|i| MRef { kind: self.types[i], id: self.refs[i], role: self.roles[i] })
            .collect();
        if op.key { Ok(ins) } else { diff::apply(prev, op.prefix, op.del, ins) }
    }

    fn resolve_list(&self, raw: &[MRef]) -> Result<Vec<Member>> {
        raw.iter()
            .map(|m| {
                Ok(Member {
                    kind: ObjType::from_u8(m.kind).ok_or_else(|| corrupt("member type"))?,
                    id: m.id,
                    role: resolve(m.role, &self.dict_roles, &self.local)?,
                })
            })
            .collect()
    }

    fn raw_at(&self, v: usize) -> Result<Vec<MRef>> {
        let mut k = v;
        while !self.ops.get(k).ok_or_else(|| corrupt("relation version index"))?.key {
            k = k.checked_sub(1).ok_or_else(|| corrupt("missing keyframe"))?;
        }
        let mut cur = Vec::new();
        for op in &self.ops[k..=v] {
            cur = self.apply_raw(&cur, op)?;
        }
        Ok(cur)
    }

    /// Members of global version `v` (replays diffs from the preceding keyframe).
    pub fn materialize(&self, v: usize) -> Result<Vec<Member>> {
        self.resolve_list(&self.raw_at(v)?)
    }

    /// Members of global versions `first .. first + n` (one replay).
    pub fn materialize_run(&self, first: usize, n: usize) -> Result<Vec<Vec<Member>>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut cur = self.raw_at(first)?;
        let mut out = Vec::with_capacity(n);
        out.push(self.resolve_list(&cur)?);
        for v in first + 1..first + n {
            let op = self.ops.get(v).ok_or_else(|| corrupt("relation version index"))?;
            cur = self.apply_raw(&cur, op)?;
            out.push(self.resolve_list(&cur)?);
        }
        Ok(out)
    }
}

/// Decodes the data frame of a block into logical segments.
pub fn decode_data(
    obj_type: ObjType,
    dirs: &[DirEntry],
    data: &[u8],
    geom: &[u8],
    dicts: &DecodeDicts,
    opt: &DecodeOptions,
) -> Result<Vec<Segment>> {
    Ok(decode_data_lazy(obj_type, dirs, data, geom, dicts, opt, false)?.0)
}

/// Segment filter applied while decoding: non-matching node and way segments are skipped
/// without materializing slots, coordinates or node lists.
pub struct SegFilter<'a> {
    /// Evaluated once per tagset of the block.
    pub tags: &'a (dyn Fn(&Tags) -> bool + Sync),
    /// Decides per segment given its versions and the tag result of each version.
    pub keep: &'a (dyn Fn(&DirEntry, &[Meta], &[bool]) -> bool + Sync),
}

/// Result of a (possibly filtered) data-frame decode.
pub struct Decoded {
    pub segs: Vec<Segment>,
    /// Directory index of each returned segment.
    pub idx: Vec<u32>,
    pub rel: Option<RelLists>,
}

/// Like [`decode_data`]; with `lazy_relations` relation member lists are left empty and
/// returned as [`RelLists`] for on-demand materialization.
pub fn decode_data_lazy(
    obj_type: ObjType,
    dirs: &[DirEntry],
    data: &[u8],
    geom: &[u8],
    dicts: &DecodeDicts,
    opt: &DecodeOptions,
    lazy_relations: bool,
) -> Result<(Vec<Segment>, Option<RelLists>)> {
    let d = decode_data_filtered(obj_type, dirs, data, Some(geom), dicts, opt, lazy_relations, None)?;
    Ok((d.segs, d.rel))
}

/// Decodes a block's attribute frame and (if given) geometry frame, optionally keeping only
/// node/way segments accepted by `filter` (relation blocks always need the geometry frame;
/// their member lists are lazy anyway). Without the geometry frame node positions are 0,
/// ways have no slots or node lists, and derived node boxes are empty ("attributes only").
#[allow(clippy::too_many_arguments)]
pub fn decode_data_filtered(
    obj_type: ObjType,
    dirs: &[DirEntry],
    data: &[u8],
    geom: Option<&[u8]>,
    dicts: &DecodeDicts,
    opt: &DecodeOptions,
    lazy_relations: bool,
    filter: Option<&SegFilter>,
) -> Result<Decoded> {
    let mut rel_lists = None;
    let mut out_idx: Vec<u32> = Vec::new();
    let mut budget = frame_budget(data.len());
    let mut p = 0;
    let uid_deltas = get_u64_column_budget(data, &mut p, &mut budget)?;
    let mut uids = Vec::with_capacity(uid_deltas.len());
    let mut acc = 0u64;
    for d in uid_deltas {
        acc = acc.wrapping_add(d);
        uids.push(u32::try_from(acc).map_err(|_| corrupt("uid range"))?);
    }
    let lens = get_u64_column_budget(data, &mut p, &mut budget)?;
    let bytes = get_bytes_column(data, &mut p)?;
    let mut local = Vec::with_capacity(lens.len());
    let mut bp = 0usize;
    for l in lens {
        let end = bp.checked_add(l as usize).ok_or_else(|| corrupt("string length"))?;
        let b = bytes.get(bp..end).ok_or_else(|| corrupt("string bytes"))?;
        local.push(Str::from(std::str::from_utf8(b).map_err(|_| corrupt("utf-8"))?));
        bp = end;
    }
    let ts_lens = get_u64_column_budget(data, &mut p, &mut budget)?;
    let mut ts_keys = Col::read_budget(data, &mut p, "tag keys", &mut budget)?;
    let mut ts_vals = Col::read_budget(data, &mut p, "tag values", &mut budget)?;
    let mut tagsets = Vec::with_capacity(ts_lens.len());
    // Global strings are copied once per block so that threads decoding different blocks
    // never contend on the same reference counts.
    let mut kcache: HashMap<u64, Str> = HashMap::default();
    let mut vcache: HashMap<u64, Str> = HashMap::default();
    let cached = |r: u64, global: &[Str], cache: &mut HashMap<u64, Str>| -> Result<Str> {
        if r & 1 == 1 {
            return resolve(r, global, &local);
        }
        if let Some(s) = cache.get(&r) {
            return Ok(s.clone());
        }
        let s = Str::from(&*resolve(r, global, &local)?);
        cache.insert(r, s.clone());
        Ok(s)
    };
    for l in ts_lens {
        if l as usize > ts_keys.remaining() {
            return Err(corrupt("tagset length"));
        }
        let mut t = Vec::with_capacity(l as usize);
        for _ in 0..l {
            let k = cached(ts_keys.next()?, &dicts.keys, &mut kcache)?;
            let v = cached(ts_vals.next()?, &dicts.values, &mut vcache)?;
            t.push((k, v));
        }
        tagsets.push(Tags::from(t));
    }
    let mut cs_cs = Col::read_budget(data, &mut p, "cs table cs", &mut budget)?;
    let mut cs_uid = Col::read_budget(data, &mut p, "cs table uid", &mut budget)?;
    let mut cs_ts = Col::read_budget(data, &mut p, "cs table ts", &mut budget)?;
    let mut cs_table = Vec::with_capacity(cs_cs.len());
    let (mut pcs, mut pts) = (0u64, opt.time_base);
    for _ in 0..cs_cs.len() {
        pcs = pcs.wrapping_add(cs_cs.next()?);
        let uid = *uids.get(cs_uid.next()? as usize).ok_or_else(|| corrupt("uid index"))?;
        pts = pts.wrapping_add(cs_ts.next_i()?);
        cs_table.push((pcs, uid, pts));
    }
    let mut c = Common {
        local,
        tagsets,
        cs_table,
        ver: Col::read_budget(data, &mut p, "version", &mut budget)?,
        ts: Col::read_budget(data, &mut p, "timestamp", &mut budget)?,
        cs: Col::read_budget(data, &mut p, "changeset", &mut budget)?,
        vf: Col::read_budget(data, &mut p, "vflags", &mut budget)?,
        tagset: Col::read_budget(data, &mut p, "tagset", &mut budget)?,
        tagsets_assigned: 0,
    };
    let total_versions: u64 = dirs.iter().map(|d| d.n_versions as u64).sum();
    if total_versions != c.ver.len() as u64 {
        return Err(corrupt("version count mismatch"));
    }
    let tag_ok: Vec<bool> = match filter {
        Some(f) => c.tagsets.iter().map(|t| (f.tags)(t)).collect(),
        None => Vec::new(),
    };
    let (mut mbuf, mut obuf): (Vec<Meta>, Vec<bool>) = (Vec::new(), Vec::new());
    let mut decide = |d: &DirEntry, metas: &[(Meta, usize)]| -> bool {
        match filter {
            None => true,
            Some(f) => {
                mbuf.clear();
                obuf.clear();
                for (m, ti) in metas {
                    mbuf.push(m.clone());
                    obuf.push(tag_ok[*ti]);
                }
                (f.keep)(d, &mbuf, &obuf)
            }
        }
    };

    let mut out = Vec::with_capacity(dirs.len());
    let attr_only = geom.is_none();
    let gdata = geom.unwrap_or(&[]);
    let mut gp = 0usize;
    let mut gbudget = frame_budget(gdata.len());
    let mut gcol = |name: &'static str| -> Result<Col> {
        if attr_only { Ok(Col::from_vec(Vec::new(), name)) } else { Col::read_budget(gdata, &mut gp, name, &mut gbudget) }
    };
    let _ = &mut p;
    match obj_type {
        ObjType::Node => {
            let mut lon = gcol("lon")?;
            let mut lat = gcol("lat")?;
            let (mut plon, mut plat) = (0i64, 0i64);
            for (di, d) in dirs.iter().enumerate() {
                let mut pv = 0i64;
                let mut metas = Vec::with_capacity(d.n_versions as usize);
                for _ in 0..d.n_versions {
                    metas.push(c.meta_ti(&mut pv)?);
                }
                let metas = metas;
                let keep = decide(d, &metas);
                let mut vs = Vec::with_capacity(if keep { metas.len() } else { 0 });
                for (meta, ti) in metas {
                    let (mut x, mut y) = (0, 0);
                    if meta.visible && !attr_only {
                        plon = plon.wrapping_add(lon.next_i()?);
                        plat = plat.wrapping_add(lat.next_i()?);
                        x = i32::try_from(plon).map_err(|_| corrupt("lon range"))?;
                        y = i32::try_from(plat).map_err(|_| corrupt("lat range"))?;
                    }
                    if keep {
                        vs.push(NodeVersion { meta, tags: c.tagsets[ti].clone(), lon: x, lat: y });
                    }
                }
                if keep {
                    out.push(seg(d, SegmentBody::Node(vs), opt.time_base, attr_only)?);
                    out_idx.push(di as u32);
                }
            }
        }
        ObjType::Way => {
            let mut cols = Vec::with_capacity(11);
            for name in [
                "slot id", "slot kind", "pe t", "pe flags", "pe lon", "pe lat", "pe ver", "pe cs", "nl kind",
                "nl nums", "nl items",
            ] {
                cols.push(gcol(name)?);
            }
            let [w_node, w_kind, w_t, w_f, w_lon, w_lat, w_ver, w_cs, nl_kind, nl_nums, nl_items] = &mut cols[..]
            else {
                unreachable!()
            };
            let (mut prev_slot, mut plon, mut plat) = (0i64, 0i64, 0i64);
            for (di, d) in dirs.iter().enumerate() {
                if !attr_only && d.n_slots as usize > w_node.remaining() {
                    return Err(corrupt("slot count"));
                }
                let mut pv = 0i64;
                let mut metas = Vec::with_capacity(d.n_versions as usize);
                for _ in 0..d.n_versions {
                    metas.push(c.meta_ti(&mut pv)?);
                }
                let metas = metas;
                if attr_only {
                    if decide(d, &metas) {
                        let versions = metas
                            .into_iter()
                            .map(|(meta, ti)| WayVersion { meta, tags: c.tagsets[ti].clone(), nodes: Vec::new() })
                            .collect();
                        out.push(seg(d, SegmentBody::Way { slots: Vec::new(), versions }, opt.time_base, true)?);
                        out_idx.push(di as u32);
                    }
                    continue;
                }
                if !decide(d, &metas) {
                    // Skip: advance all cursors (running deltas must still be summed).
                    for _ in 0..d.n_slots {
                        prev_slot = prev_slot.wrapping_add(w_node.next_i()?);
                        let kind = w_kind.next()?;
                        for _ in 0..kind.saturating_sub(1) {
                            w_t.next()?;
                            w_cs.next()?;
                            w_ver.next()?;
                            if w_f.next()? & VF_VISIBLE != 0 {
                                plon += w_lon.next_i()?;
                                plat += w_lat.next_i()?;
                            }
                        }
                    }
                    for _ in 0..d.n_versions {
                        let n_items = match nl_kind.next()? {
                            0 => nl_nums.next()?,
                            1 => {
                                nl_nums.next()?;
                                nl_nums.next()?;
                                nl_nums.next()?
                            }
                            _ => return Err(corrupt("list kind")),
                        };
                        if n_items as usize > nl_items.remaining() {
                            return Err(corrupt("list items exhausted"));
                        }
                        for _ in 0..n_items {
                            nl_items.next()?;
                        }
                    }
                    continue;
                }
                let mut slots = Vec::with_capacity(d.n_slots as usize);
                for _ in 0..d.n_slots {
                    let node_id = prev_slot.wrapping_add(w_node.next_i()?);
                    prev_slot = node_id;
                    let kind = w_kind.next()?;
                    let private = if kind == 0 {
                        None
                    } else {
                        let k = (kind - 1) as usize;
                        if k > w_t.remaining() {
                            return Err(corrupt("private entry count"));
                        }
                        let mut es = Vec::with_capacity(k);
                        let mut pv = 0i64;
                        for _ in 0..k {
                            let off = w_t.next()?;
                            let (changeset, uid, timestamp) = c.cs(w_cs.next()?, off)?;
                            let f = w_f.next()?;
                            let visible = f & VF_VISIBLE != 0;
                            let (mut x, mut y) = (0, 0);
                            if visible {
                                plon += w_lon.next_i()?;
                                plat += w_lat.next_i()?;
                                x = i32::try_from(plon).map_err(|_| corrupt("lon range"))?;
                                y = i32::try_from(plat).map_err(|_| corrupt("lat range"))?;
                            }
                            pv = pv.wrapping_add(w_ver.next_i()?);
                            let version = pv as u32;
                            es.push(PrivEntry {
                                timestamp,
                                lon: x,
                                lat: y,
                                visible,
                                continuation: f & VF_CONT != 0,
                                version,
                                changeset,
                                uid,
                            });
                        }
                        Some(es)
                    };
                    slots.push(Slot { node_id, private });
                }
                let mut versions: Vec<WayVersion> = Vec::with_capacity(d.n_versions as usize);
                let n_slots = d.n_slots as u64;
                for (meta, ti) in metas {
                    let tags = c.tagsets[ti].clone();
                    let prev = versions.last().map(|v| &v.nodes);
                    let nodes = decode_list(nl_kind, nl_nums, prev, |last| {
                        let base = last.map_or(-1, |&v| v as i64);
                        let v = base.wrapping_add(nl_items.next_i()?);
                        if v < 0 || v as u64 >= n_slots {
                            return Err(corrupt("slot index out of range"));
                        }
                        Ok(v as u32)
                    })?;
                    versions.push(WayVersion { meta, tags, nodes });
                }
                out.push(seg(d, SegmentBody::Way { slots, versions }, opt.time_base, false)?);
                out_idx.push(di as u32);
            }
        }
        ObjType::Relation => {
            if attr_only {
                return Err(corrupt("relation blocks require the geometry frame"));
            }
            let mut kind = gcol("m kind")?;
            let mut nums = gcol("m nums")?;
            let mtype = gcol("m type")?;
            let mref = gcol("m ref")?;
            let mrole = gcol("m role")?;
            if mtype.len() != mref.len() || mtype.len() != mrole.len() {
                return Err(corrupt("member column lengths"));
            }
            let mut refs = Vec::with_capacity(mref.len());
            let mut acc = 0i64;
            for &d in mref.values() {
                acc = acc.wrapping_add(unzigzag(d));
                refs.push(acc);
            }
            let mut ops = Vec::with_capacity(total_versions as usize);
            let mut start = 0usize;
            for d in dirs {
                let mut vs: Vec<RelVersion> = Vec::with_capacity(d.n_versions as usize);
                let mut pv = 0i64;
                let mut prev_len: Option<usize> = None;
                for _ in 0..d.n_versions {
                    let (meta, tags) = c.meta(&mut pv)?;
                    let op = match kind.next()? {
                        0 => {
                            let len = nums.next()? as usize;
                            ListOp { key: true, prefix: 0, del: 0, start, len }
                        }
                        1 => {
                            let pl = prev_len.ok_or_else(|| corrupt("diff without previous list"))?;
                            let (prefix, del, len) = (nums.next()? as usize, nums.next()? as usize, nums.next()? as usize);
                            if prefix.checked_add(del).is_none_or(|x| x > pl) {
                                return Err(corrupt("list diff range"));
                            }
                            ListOp { key: false, prefix, del, start, len }
                        }
                        _ => return Err(corrupt("list kind")),
                    };
                    start = start.checked_add(op.len).ok_or_else(|| corrupt("member count"))?;
                    if start > refs.len() {
                        return Err(corrupt("member items exhausted"));
                    }
                    prev_len = Some(if op.key { op.len } else { prev_len.unwrap() - op.del + op.len });
                    ops.push(op);
                    vs.push(RelVersion { meta, tags, members: Vec::new() });
                }
                out.push(seg(d, SegmentBody::Relation(vs), opt.time_base, false)?);
            }
            let lists = RelLists {
                ops,
                types: mtype.values().iter().map(|&t| t as u8).collect(),
                refs,
                roles: mrole.values().to_vec(),
                // Private string copies: avoids refcount contention on shared role strings
                // when many threads materialize members concurrently.
                local: c.local.iter().map(|x| Str::from(&**x)).collect(),
                dict_roles: dicts.roles.iter().map(|x| Str::from(&**x)).collect(),
            };
            if lazy_relations {
                rel_lists = Some(lists);
            } else {
                let mut g = 0usize;
                for s in out.iter_mut() {
                    if let SegmentBody::Relation(vs) = &mut s.body {
                        let all = lists.materialize_run(g, vs.len())?;
                        for (v, m) in vs.iter_mut().zip(all) {
                            v.members = m;
                        }
                        g += vs.len();
                    }
                }
            }
        }
    }
    if out_idx.len() != out.len() {
        out_idx = (0..out.len() as u32).collect();
    }
    Ok(Decoded { segs: out, idx: out_idx, rel: rel_lists })
}

fn seg(d: &DirEntry, body: SegmentBody, time_base: i64, attr_only: bool) -> Result<Segment> {
    let (dmin, dmax, mut dbox) = derived_fields(&body, time_base);
    if attr_only {
        // positions unknown: derived node boxes are not available
        dbox = BBox::EMPTY;
    }
    let tmin = match d.tmin {
        Some(t) => t,
        // without geometry, records of private nodes are missing; intervals are then
        // only approximate and not used for validity decisions (see reader)
        None if attr_only => dmin.unwrap_or(0),
        None => dmin.ok_or_else(|| corrupt("segment tmin not derivable"))?,
    };
    let tmax = d.tmax.unwrap_or(dmax);
    let bbox = match d.bbox {
        Some(b) => b,
        None if body.obj_type() == ObjType::Node => dbox,
        None => return Err(corrupt("way/relation segment without bbox")),
    };
    Ok(Segment { id: d.id, flags: d.flags & !SEG_LAYOUT_FLAGS | if bbox.is_empty() { SEG_NO_BBOX } else { 0 }, tmin, tmax, bbox, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(kv: &[(&str, &str)]) -> Tags {
        kv.iter().map(|(k, v)| (Str::from(*k), Str::from(*v))).collect::<Vec<_>>().into()
    }

    fn meta(v: u32, t: i64, vis: bool) -> Meta {
        Meta { version: v, timestamp: t, changeset: 1000 + v as u64, uid: 7 * v, visible: vis, continuation: false }
    }

    struct Dicts;
    impl EncodeDicts for Dicts {
        fn key(&self, s: &str) -> Option<u32> {
            (s == "highway").then_some(0)
        }
        fn value(&self, s: &str) -> Option<u32> {
            (s == "residential").then_some(0)
        }
        fn role(&self, s: &str) -> Option<u32> {
            (s == "outer").then_some(0)
        }
    }

    fn ddicts() -> DecodeDicts {
        DecodeDicts { keys: vec!["highway".into()], values: vec!["residential".into()], roles: vec!["outer".into()] }
    }

    fn roundtrip(segs: Vec<Segment>) {
        let tb = 1_000_000_000;
        let eo = EncodeOptions { time_base: tb, keyframe_interval: 3 };
        let raw = encode_block(&segs, &Dicts, &eo).unwrap();
        let framed = frame_block(&raw, 3, None).unwrap();
        let f = parse_block_frame(&framed, true).unwrap();
        let dir = decompress(f.dir, f.header.dir_raw, None).unwrap();
        let data = decompress(f.data, f.header.data_raw, None).unwrap();
        let geom = decompress(f.geom, f.header.geom_raw, None).unwrap();
        let dirs = decode_directory(&dir, f.header.n_segments).unwrap();
        let dec = decode_data(
            f.header.obj_type,
            &dirs,
            &data,
            &geom,
            &ddicts(),
            &DecodeOptions { time_base: tb },
        )
        .unwrap();
        assert_eq!(dec, segs);
    }

    #[test]
    fn node_block_roundtrip() {
        let segs = vec![
            Segment {
                id: 5,
                flags: 0,
                tmin: 10,
                tmax: T_OPEN,
                bbox: BBox::new(10, 20, 30, 40),
                body: SegmentBody::Node(vec![
                    NodeVersion { meta: meta(1, 1_000_000_010, true), tags: tags(&[]), lon: 10, lat: 20 },
                    NodeVersion { meta: meta(3, 1_000_000_050, true), tags: tags(&[("a", "b")]), lon: 30, lat: 40 },
                    NodeVersion { meta: meta(4, 1_000_000_060, false), tags: tags(&[]), lon: 0, lat: 0 },
                ]),
            },
            Segment {
                id: 2,
                flags: SEG_MULTI | SEG_NO_BBOX,
                tmin: 0,
                tmax: 100,
                bbox: BBox::EMPTY,
                body: SegmentBody::Node(vec![NodeVersion {
                    meta: meta(1, 999_999_000, false),
                    tags: tags(&[("highway", "residential")]),
                    lon: 0,
                    lat: 0,
                }]),
            },
        ];
        roundtrip(segs);
    }

    #[test]
    fn way_block_roundtrip() {
        let pe = |t: i64, x: i32, cont: bool| PrivEntry {
            timestamp: t,
            lon: x,
            lat: -x,
            visible: true,
            continuation: cont,
            version: 2,
            changeset: 9,
            uid: 3,
        };
        let mut versions = Vec::new();
        let lists: [&[u32]; 6] = [&[0, 1, 2], &[0, 1, 2, 3], &[0, 2, 3], &[0, 2, 3, 1, 0], &[], &[3, 2, 1, 0]];
        for (i, l) in lists.iter().enumerate() {
            versions.push(WayVersion {
                meta: meta(i as u32 + 1, 1_000_000_100 + i as i64, i != 4),
                tags: tags(&[("highway", "residential"), ("name", "X")]),
                nodes: l.to_vec(),
            });
        }
        let segs = vec![Segment {
            id: 77,
            flags: 0,
            tmin: 100,
            tmax: T_OPEN,
            bbox: BBox::new(-5, -5, 5, 5),
            body: SegmentBody::Way {
                slots: vec![
                    Slot { node_id: 100, private: None },
                    Slot { node_id: 50, private: Some(vec![pe(999_999_000, 3, true), pe(1_000_000_200, 4, false)]) },
                    Slot { node_id: 51, private: Some(vec![]) },
                    Slot { node_id: 9, private: None },
                ],
                versions,
            },
        }];
        roundtrip(segs);
    }

    #[test]
    fn relation_block_roundtrip() {
        let m = |k: ObjType, id: i64, r: &str| Member { kind: k, id, role: r.into() };
        let base = vec![m(ObjType::Way, 10, "outer"), m(ObjType::Way, 11, "inner"), m(ObjType::Node, 1, "")];
        let mut v2 = base.clone();
        v2.insert(1, m(ObjType::Relation, 5, "subarea"));
        let segs = vec![Segment {
            id: 1,
            flags: SEG_LARGE,
            tmin: 0,
            tmax: 500,
            bbox: BBox::new(0, 0, 100_000_000, 100_000_000),
            body: SegmentBody::Relation(vec![
                RelVersion { meta: meta(1, 1_000_000_000, true), tags: tags(&[("type", "multipolygon")]), members: base },
                RelVersion { meta: meta(2, 1_000_000_100, true), tags: tags(&[("type", "multipolygon")]), members: v2 },
                RelVersion { meta: meta(3, 1_000_000_500, false), tags: tags(&[]), members: vec![] },
            ]),
        }];
        roundtrip(segs);
    }

    #[test]
    fn filtered_decode_skips_consistently() {
        let pe = |t: i64, x: i32| PrivEntry {
            timestamp: t, lon: x, lat: x + 1, visible: x % 3 != 0, continuation: false, version: 1, changeset: 9, uid: 3,
        };
        let mut segs = Vec::new();
        for k in 0..6i64 {
            let lists: [&[u32]; 3] = [&[0, 1], &[0, 1, 2], &[2, 1]];
            let versions = lists
                .iter()
                .enumerate()
                .map(|(i, l)| WayVersion {
                    meta: meta(i as u32 + 1, 1_000_000_100 + i as i64, true),
                    tags: tags(&[("highway", if k % 2 == 0 { "residential" } else { "path" })]),
                    nodes: l.to_vec(),
                })
                .collect();
            segs.push(Segment {
                id: 100 + k,
                flags: 0,
                tmin: 100,
                tmax: T_OPEN,
                bbox: BBox::new(-5, -5, 5, 5),
                body: SegmentBody::Way {
                    slots: vec![
                        Slot { node_id: 7 + k, private: None },
                        Slot { node_id: 50 + k, private: Some(vec![pe(1_000_000_090, 3 + k as i32), pe(1_000_000_200, 4)]) },
                        Slot { node_id: 60 + k, private: Some(vec![pe(1_000_000_095, 6)]) },
                    ],
                    versions,
                },
            });
        }
        {
            let tb = 1_000_000_000;
            let raw = encode_block(&segs, &Dicts, &EncodeOptions { time_base: tb, keyframe_interval: 2 }).unwrap();
            let dirs = decode_directory(&raw.dir, raw.n_segments).unwrap();
            let dopt = DecodeOptions { time_base: tb };
            let full = decode_data(ObjType::Way, &dirs, &raw.data, &raw.geom, &ddicts(), &dopt).unwrap();
            let tagf = |t: &Tags| t.iter().any(|(_, v)| v.as_ref() == "path");
            let keep = |_: &DirEntry, _: &[Meta], ok: &[bool]| ok.iter().any(|x| *x);
            let f = SegFilter { tags: &tagf, keep: &keep };
            let d = decode_data_filtered(ObjType::Way, &dirs, &raw.data, Some(&raw.geom), &ddicts(), &dopt, true, Some(&f)).unwrap();
            assert_eq!(d.idx, vec![1, 3, 5]);
            for (s, &i) in d.segs.iter().zip(&d.idx) {
                assert_eq!(s, &full[i as usize]);
            }
        }
    }

    #[test]
    fn corrupt_data_is_rejected_not_panicking() {
        let segs = vec![Segment {
            id: 5,
            flags: 0,
            tmin: 10,
            tmax: T_OPEN,
            bbox: BBox::new(10, 20, 30, 40),
            body: SegmentBody::Node(vec![NodeVersion {
                meta: meta(1, 1_000_000_010, true),
                tags: tags(&[("a", "b")]),
                lon: 10,
                lat: 20,
            }]),
        }];
        let eo = EncodeOptions { time_base: 0, keyframe_interval: 3 };
        let raw = encode_block(&segs, &Dicts, &eo).unwrap();
        for i in 0..raw.data.len() {
            for flip in [1u8, 0x80, 0xff] {
                let mut d = raw.data.clone();
                d[i] ^= flip;
                let dirs = decode_directory(&raw.dir, 1).unwrap();
                let _ = decode_data(
                    ObjType::Node,
                    &dirs,
                    &d,
                    &raw.geom,
                    &ddicts(),
                    &DecodeOptions { time_base: 0 },
                );
            }
        }
    }
}

/// Column names of a data frame in order (for statistics and debugging).
pub fn data_column_names(t: ObjType) -> Vec<&'static str> {
    let mut v = vec![
        "uid_table", "str_lens", "str_bytes", "tagset_lens", "tag_keys", "tag_vals", "cs_cs", "cs_uid", "cs_ts",
        "version", "timestamp", "cs_idx", "vflags", "tagset",
    ];
    match t {
        ObjType::Node => v.extend(["lon", "lat"]),
        ObjType::Way => v.extend([
            "slot_id", "slot_kind", "pe_t", "pe_flags", "pe_lon", "pe_lat", "pe_version", "pe_cs", "nl_kind",
            "nl_nums", "nl_items",
        ]),
        ObjType::Relation => v.extend(["m_kind", "m_nums", "m_type", "m_ref", "m_role"]),
    }
    v
}

/// Byte ranges of each column in the attribute frame followed by the geometry frame
/// (geometry ranges are offset by `data.len()`).
pub fn data_column_ranges(t: ObjType, data: &[u8], geom: &[u8]) -> Result<Vec<(&'static str, std::ops::Range<usize>)>> {
    let mut out = Vec::new();
    let mut p = 0;
    let names = data_column_names(t);
    for name in &names[..14] {
        let start = p;
        if *name == "str_bytes" {
            get_bytes_column(data, &mut p)?;
        } else {
            get_u64_column(data, &mut p)?;
        }
        out.push((*name, start..p));
    }
    let mut g = 0;
    for name in &names[14..] {
        let start = g;
        get_u64_column(geom, &mut g)?;
        out.push((*name, data.len() + start..data.len() + g));
    }
    Ok(out)
}

