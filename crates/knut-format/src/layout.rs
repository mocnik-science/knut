//! Fixed on-disk layouts: header, section table, footer, block header, block table,
//! R-tree nodes and ID-index chunks. See SPEC.md §4–§9.

use crate::bloom::{BLOOM_WORDS, Bloom};
use crate::codec::le;
use crate::error::corrupt;
use crate::{BBox, Error, ObjType, Result};
use xxhash_rust::xxh3::xxh3_64;

pub const MAGIC: [u8; 8] = *b"KNUT\0\r\n\x1a";
pub const FOOTER_MAGIC: [u8; 8] = *b"KNUTEND\0";
pub const FORMAT_MAJOR: u32 = 1;
pub const FORMAT_MINOR: u32 = 0;
pub const HEADER_SIZE: usize = 1024;
pub const MAX_SECTIONS: usize = 24;
const SECTION_TABLE_OFF: usize = 136;
const HEADER_CHECKSUM_OFF: usize = 1016;

pub const FEAT_PRIVATE_META: u64 = 1 << 0;
pub const FEAT_USERS: u64 = 1 << 1;
pub const FEAT_USERS_STRIPPED: u64 = 1 << 2;
pub const FEAT_UIDS_PSEUDONYMIZED: u64 = 1 << 3;
pub const FEAT_ZSTD_DICT: u64 = 1 << 4;
/// A filter per block, sized to its tags (section `BlockBlooms`).
pub const FEAT_BLOCK_BLOOMS: u64 = 1 << 5;
pub const FEAT_KNOWN: u64 = 0x3f;
/// Feature bits at or above this position are incompatible if unknown.
pub const FEAT_INCOMPATIBLE_MASK: u64 = !0xffff_ffff;

pub const SEC_FLAG_ZSTD: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum SectionKind {
    Keys = 1,
    Values = 2,
    Roles = 3,
    Users = 4,
    ZdictNode = 5,
    ZdictWay = 6,
    ZdictRel = 7,
    BlockTable = 8,
    RTree = 9,
    Blocks = 10,
    IdxNode = 11,
    IdxWay = 12,
    IdxRel = 13,
    Meta = 14,
    BlockBlooms = 15,
}

impl SectionKind {
    pub fn from_u32(v: u32) -> Option<SectionKind> {
        use SectionKind::*;
        Some(match v {
            1 => Keys,
            2 => Values,
            3 => Roles,
            4 => Users,
            5 => ZdictNode,
            6 => ZdictWay,
            7 => ZdictRel,
            8 => BlockTable,
            9 => RTree,
            10 => Blocks,
            11 => IdxNode,
            12 => IdxWay,
            13 => IdxRel,
            14 => Meta,
            15 => BlockBlooms,
            _ => return None,
        })
    }

    pub fn zdict(t: ObjType) -> SectionKind {
        match t {
            ObjType::Node => SectionKind::ZdictNode,
            ObjType::Way => SectionKind::ZdictWay,
            ObjType::Relation => SectionKind::ZdictRel,
        }
    }

    pub fn idx(t: ObjType) -> SectionKind {
        match t {
            ObjType::Node => SectionKind::IdxNode,
            ObjType::Way => SectionKind::IdxWay,
            ObjType::Relation => SectionKind::IdxRel,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionEntry {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub length: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub format_major: u32,
    pub format_minor: u32,
    pub features: u64,
    pub time_base: i64,
    pub tmin: i64,
    pub tmax: i64,
    pub bbox: BBox,
    pub n_objects: [u64; 3],
    pub n_versions: [u64; 3],
    pub n_segments: u64,
    pub n_blocks: u64,
    pub sections: Vec<SectionEntry>,
}

impl Header {
    pub fn to_bytes(&self) -> Vec<u8> {
        assert!(self.sections.len() <= MAX_SECTIONS);
        let mut b = vec![0u8; HEADER_SIZE];
        b[0..8].copy_from_slice(&MAGIC);
        put32(&mut b, 8, self.format_major);
        put32(&mut b, 12, self.format_minor);
        put64(&mut b, 16, self.features);
        put64(&mut b, 24, self.time_base as u64);
        put64(&mut b, 32, self.tmin as u64);
        put64(&mut b, 40, self.tmax as u64);
        put_bbox(&mut b, 48, &self.bbox);
        for i in 0..3 {
            put64(&mut b, 64 + 8 * i, self.n_objects[i]);
            put64(&mut b, 88 + 8 * i, self.n_versions[i]);
        }
        put64(&mut b, 112, self.n_segments);
        put64(&mut b, 120, self.n_blocks);
        put32(&mut b, 128, self.sections.len() as u32);
        for (i, s) in self.sections.iter().enumerate() {
            let o = SECTION_TABLE_OFF + 32 * i;
            put32(&mut b, o, s.kind);
            put32(&mut b, o + 4, s.flags);
            put64(&mut b, o + 8, s.offset);
            put64(&mut b, o + 16, s.length);
        }
        let cs = xxh3_64(&b[..HEADER_CHECKSUM_OFF]);
        put64(&mut b, HEADER_CHECKSUM_OFF, cs);
        b
    }

    /// Parses and validates the header (magic, version, checksum, section bounds).
    pub fn parse(buf: &[u8], file_len: u64) -> Result<Header> {
        let b = le::slice(buf, 0, HEADER_SIZE)?;
        if b[0..8] != MAGIC {
            return Err(Error::BadMagic("header"));
        }
        let cs = le::u64(b, HEADER_CHECKSUM_OFF)?;
        if xxh3_64(&b[..HEADER_CHECKSUM_OFF]) != cs {
            return Err(Error::Checksum("header".into()));
        }
        let format_major = le::u32(b, 8)?;
        if format_major != FORMAT_MAJOR {
            return Err(Error::Unsupported(format!("format major version {format_major}")));
        }
        let features = le::u64(b, 16)?;
        if features & FEAT_INCOMPATIBLE_MASK & !FEAT_KNOWN != 0 {
            return Err(Error::Unsupported(format!("feature flags {features:#x}")));
        }
        let n_sections = le::u32(b, 128)? as usize;
        if n_sections > MAX_SECTIONS {
            return Err(corrupt("too many sections"));
        }
        let mut sections = Vec::with_capacity(n_sections);
        for i in 0..n_sections {
            let o = SECTION_TABLE_OFF + 32 * i;
            let s = SectionEntry {
                kind: le::u32(b, o)?,
                flags: le::u32(b, o + 4)?,
                offset: le::u64(b, o + 8)?,
                length: le::u64(b, o + 16)?,
            };
            let end = s.offset.checked_add(s.length).ok_or_else(|| corrupt("section overflow"))?;
            if end > file_len || s.offset < HEADER_SIZE as u64 {
                return Err(corrupt(format!("section {} out of bounds", s.kind)));
            }
            sections.push(s);
        }
        let mut n_objects = [0u64; 3];
        let mut n_versions = [0u64; 3];
        for i in 0..3 {
            n_objects[i] = le::u64(b, 64 + 8 * i)?;
            n_versions[i] = le::u64(b, 88 + 8 * i)?;
        }
        Ok(Header {
            format_major,
            format_minor: le::u32(b, 12)?,
            features,
            time_base: le::i64(b, 24)?,
            tmin: le::i64(b, 32)?,
            tmax: le::i64(b, 40)?,
            bbox: get_bbox(b, 48)?,
            n_objects,
            n_versions,
            n_segments: le::u64(b, 112)?,
            n_blocks: le::u64(b, 120)?,
            sections,
        })
    }

    pub fn section(&self, kind: SectionKind) -> Option<&SectionEntry> {
        self.sections.iter().find(|s| s.kind == kind as u32)
    }
}

pub fn footer_bytes(checksums: &[u64]) -> Vec<u8> {
    let mut b = Vec::with_capacity(checksums.len() * 8 + 16);
    for c in checksums {
        b.extend_from_slice(&c.to_le_bytes());
    }
    b.extend_from_slice(&(checksums.len() as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&FOOTER_MAGIC);
    b
}

/// Returns the per-section checksums stored in the footer.
pub fn parse_footer(buf: &[u8]) -> Result<Vec<u64>> {
    if buf.len() < 16 {
        return Err(Error::Truncated("footer"));
    }
    let end = buf.len();
    if buf[end - 8..] != FOOTER_MAGIC {
        return Err(Error::BadMagic("footer"));
    }
    let n = le::u32(buf, end - 16)? as usize;
    if n > MAX_SECTIONS {
        return Err(corrupt("footer section count"));
    }
    let start = end - 16 - 8 * n;
    (0..n).map(|i| le::u64(buf, start + 8 * i)).collect()
}

// ---------------------------------------------------------------------------------------
// Blocks

pub const BLOCK_MAGIC: u32 = 0x3142_4E4B; // "KNB1"
pub const BLOCK_HEADER_SIZE: usize = 72;
pub const BLOCK_FLAG_DICT: u8 = 1;
pub const BLOCK_FLAG_LARGE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub obj_type: ObjType,
    pub flags: u8,
    pub n_segments: u32,
    pub dir_comp: u32,
    pub dir_raw: u32,
    /// Attribute frame (metadata, tags, changesets).
    pub data_comp: u32,
    pub data_raw: u32,
    /// Geometry frame (coordinates, slots, node/member lists).
    pub geom_comp: u32,
    pub geom_raw: u32,
    pub tmin: u32,
    pub tmax: u32,
    pub bbox: BBox,
    pub checksum: u64,
}

impl BlockHeader {
    pub fn to_bytes(&self) -> [u8; BLOCK_HEADER_SIZE] {
        let mut b = [0u8; BLOCK_HEADER_SIZE];
        put32(&mut b, 0, BLOCK_MAGIC);
        b[4] = self.obj_type as u8;
        b[5] = self.flags;
        put32(&mut b, 8, self.n_segments);
        put32(&mut b, 12, self.dir_comp);
        put32(&mut b, 16, self.dir_raw);
        put32(&mut b, 20, self.data_comp);
        put32(&mut b, 24, self.data_raw);
        put32(&mut b, 28, self.geom_comp);
        put32(&mut b, 32, self.geom_raw);
        put32(&mut b, 36, self.tmin);
        put32(&mut b, 40, self.tmax);
        put_bbox(&mut b, 44, &self.bbox);
        put64(&mut b, 64, self.checksum);
        b
    }

    pub fn parse(buf: &[u8]) -> Result<BlockHeader> {
        if le::u32(buf, 0)? != BLOCK_MAGIC {
            return Err(Error::BadMagic("block"));
        }
        Ok(BlockHeader {
            obj_type: ObjType::from_u8(le::u8(buf, 4)?).ok_or_else(|| corrupt("block type"))?,
            flags: le::u8(buf, 5)?,
            n_segments: le::u32(buf, 8)?,
            dir_comp: le::u32(buf, 12)?,
            dir_raw: le::u32(buf, 16)?,
            data_comp: le::u32(buf, 20)?,
            data_raw: le::u32(buf, 24)?,
            geom_comp: le::u32(buf, 28)?,
            geom_raw: le::u32(buf, 32)?,
            tmin: le::u32(buf, 36)?,
            tmax: le::u32(buf, 40)?,
            bbox: get_bbox(buf, 44)?,
            checksum: le::u64(buf, 64)?,
        })
    }
}

pub const BLOCK_ENTRY_SIZE: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockEntry {
    pub offset: u64,
    pub length: u32,
    pub obj_type: ObjType,
    pub flags: u8,
    pub section: u16,
}

impl BlockEntry {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
        out.push(self.obj_type as u8);
        out.push(self.flags);
        out.extend_from_slice(&self.section.to_le_bytes());
    }

    pub fn parse(buf: &[u8], idx: usize) -> Result<BlockEntry> {
        let o = idx.checked_mul(BLOCK_ENTRY_SIZE).ok_or(Error::Truncated("block entry"))?;
        Ok(BlockEntry {
            offset: le::u64(buf, o)?,
            length: le::u32(buf, o + 8)?,
            obj_type: ObjType::from_u8(le::u8(buf, o + 12)?).ok_or_else(|| corrupt("block type"))?,
            flags: le::u8(buf, o + 13)?,
            section: le::u16(buf, o + 14)?,
        })
    }
}

// ---------------------------------------------------------------------------------------
// R-tree

pub const RTREE_FANOUT: u32 = 16;
pub const RNODE_SIZE: usize = 32;
pub const BLOOM_SIZE: usize = BLOOM_WORDS * 8;
pub const N_TREES: usize = 4;
pub const TREE_LARGE: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RNode {
    pub bbox: BBox,
    pub tmin: u32,
    pub tmax: u32,
    pub first: u32,
    /// 0 = leaf (`first` is the global block number).
    pub count: u32,
}

impl RNode {
    pub fn write(&self, out: &mut Vec<u8>) {
        for v in [self.bbox.minx, self.bbox.miny, self.bbox.maxx, self.bbox.maxy] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [self.tmin, self.tmax, self.first, self.count] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    pub fn parse(buf: &[u8], off: usize) -> Result<RNode> {
        Ok(RNode {
            bbox: get_bbox(buf, off)?,
            tmin: le::u32(buf, off + 16)?,
            tmax: le::u32(buf, off + 20)?,
            first: le::u32(buf, off + 24)?,
            count: le::u32(buf, off + 28)?,
        })
    }
}

pub fn write_bloom(b: &Bloom, out: &mut Vec<u8>) {
    for w in b {
        out.extend_from_slice(&w.to_le_bytes());
    }
}

pub fn parse_bloom(buf: &[u8], off: usize) -> Result<Bloom> {
    let mut b = [0u64; BLOOM_WORDS];
    for (i, w) in b.iter_mut().enumerate() {
        *w = le::u64(buf, off + 8 * i)?;
    }
    Ok(b)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeHeader {
    pub n_levels: u32,
    pub n_nodes: u32,
    pub node_start: u32,
}

// ---------------------------------------------------------------------------------------
// ID index

pub const IDX_HEADER_SIZE: usize = 32;
pub const IDX_CHUNK_SIZE: usize = 24;
pub const IDX_ENTRIES_PER_CHUNK: usize = 512;

/// One ID-index chunk. The payload holds three columns (SPEC §9): id deltas, run block
/// numbers (minus `blk_base`) and run lengths minus one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdxChunk {
    pub first_id: i64,
    pub payload_off: u32,
    pub count: u16,
    pub blk_base: u32,
    pub n_runs: u32,
}

impl IdxChunk {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.first_id.to_le_bytes());
        out.extend_from_slice(&self.payload_off.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.blk_base.to_le_bytes());
        out.extend_from_slice(&self.n_runs.to_le_bytes());
    }

    pub fn parse(buf: &[u8], off: usize) -> Result<IdxChunk> {
        Ok(IdxChunk {
            first_id: le::i64(buf, off)?,
            payload_off: le::u32(buf, off + 8)?,
            count: le::u16(buf, off + 12)?,
            blk_base: le::u32(buf, off + 16)?,
            n_runs: le::u32(buf, off + 20)?,
        })
    }
}

// ---------------------------------------------------------------------------------------

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_bbox(b: &mut [u8], o: usize, bb: &BBox) {
    for (i, v) in [bb.minx, bb.miny, bb.maxx, bb.maxy].into_iter().enumerate() {
        b[o + 4 * i..o + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
}

fn get_bbox(b: &[u8], o: usize) -> Result<BBox> {
    Ok(BBox {
        minx: le::i32(b, o)?,
        miny: le::i32(b, o + 4)?,
        maxx: le::i32(b, o + 8)?,
        maxy: le::i32(b, o + 12)?,
    })
}

/// Pads `out` with zeros to the next multiple of 8.
pub fn pad8(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(8) {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = Header {
            format_major: FORMAT_MAJOR,
            format_minor: FORMAT_MINOR,
            features: FEAT_PRIVATE_META | FEAT_USERS,
            time_base: 946_684_800,
            tmin: 1,
            tmax: 2,
            bbox: BBox::new(-1, -2, 3, 4),
            n_objects: [1, 2, 3],
            n_versions: [4, 5, 6],
            n_segments: 7,
            n_blocks: 8,
            sections: vec![SectionEntry { kind: 1, flags: 1, offset: 1024, length: 10 }],
        };
        let b = h.to_bytes();
        assert_eq!(Header::parse(&b, 2000).unwrap(), h);
        let mut bad = b.clone();
        bad[20] ^= 1;
        assert!(Header::parse(&bad, 2000).is_err());
        assert!(Header::parse(&b, 1030).is_err());
    }
}
