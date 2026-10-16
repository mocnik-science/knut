//! Changing a container (`knut manage`): adding and removing elements, compacting, and
//! converting files of format 1.0.
//!
//! A change appends to the file: new sections and a new directory go to the end, then the
//! header slot with the older generation is overwritten. An interrupted change leaves the
//! previous state readable. Readers that have the file open keep their view.

use crate::{Error, Result};
use knut_format::codec::le;
use knut_format::container::{Directory, Element, ElementKind, Temporal, base_directory};
use knut_format::layout::{BLOCK_ENTRY_SIZE, BlockEntry, HEADER_SIZE, HEADER_SLOT, Header, SectionEntry, SectionKind};
use memmap2::Mmap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn pad(n: u64) -> u64 {
    n.div_ceil(8) * 8
}

/// The header (with the directory) of a container.
pub fn header(path: impl AsRef<Path>) -> Result<Header> {
    let f = File::open(path.as_ref()).map_err(io)?;
    // SAFETY: read-only mapping; the file must not be truncated while open.
    let m = unsafe { Mmap::map(&f) }.map_err(io)?;
    Ok(Header::parse(&m, m.len() as u64)?)
}

/// Bytes that `compact` would free: everything that no element uses any more.
pub fn freeable(path: impl AsRef<Path>, h: &Header) -> Result<u64> {
    let len = std::fs::metadata(path.as_ref()).map_err(io)?.len();
    let used = HEADER_SIZE as u64 + h.sections.iter().map(|s| pad(s.length)).sum::<u64>() + pad(h.directory.to_bytes().len() as u64);
    Ok(len.saturating_sub(used))
}

/// A section of a new element: its kind (≥ [`SectionKind::Index`]), flags and bytes.
pub struct NewSection {
    pub kind: u32,
    pub flags: u32,
    pub bytes: Vec<u8>,
}

/// Appends an element; returns its id.
pub fn add_element(
    path: impl AsRef<Path>,
    kind: ElementKind,
    temporal: Temporal,
    params: Vec<(String, String)>,
    sections: Vec<NewSection>,
) -> Result<u32> {
    let path = path.as_ref();
    let mut h = header(path)?;
    let mut f = OpenOptions::new().read(true).write(true).open(path).map_err(io)?;
    let mut pos = f.seek(SeekFrom::End(0)).map_err(io)?;
    let mut entries = Vec::new();
    for s in sections {
        pos = write_padding(&mut f, pos)?;
        f.write_all(&s.bytes).map_err(io)?;
        entries.push(SectionEntry { kind: s.kind, flags: s.flags, offset: pos, length: s.bytes.len() as u64, checksum: xxh3_64(&s.bytes) });
        pos += s.bytes.len() as u64;
    }
    let id = h.directory.next_id;
    h.directory.next_id += 1;
    h.directory.elements.push(Element { id, kind, temporal, created: now(), sections: entries, params });
    commit(&mut f, pos, &mut h)?;
    Ok(id)
}

/// Removes an element from the directory (its bytes stay until `compact`).
pub fn remove_element(path: impl AsRef<Path>, id: u32) -> Result<Element> {
    let path = path.as_ref();
    let mut h = header(path)?;
    let Some(i) = h.directory.elements.iter().position(|e| e.id == id) else {
        return Err(Error::Io(format!("there is no element {id}")));
    };
    let e = &h.directory.elements[i];
    if e.info().base {
        return Err(Error::Io(format!("element {id} ({}) is part of the database and cannot be removed", e.info().name)));
    }
    let removed = h.directory.elements.remove(i);
    let mut f = OpenOptions::new().read(true).write(true).open(path).map_err(io)?;
    let pos = f.seek(SeekFrom::End(0)).map_err(io)?;
    commit(&mut f, pos, &mut h)?;
    Ok(removed)
}

fn write_padding(f: &mut File, pos: u64) -> Result<u64> {
    let p = pad(pos);
    if p > pos {
        f.write_all(&vec![0u8; (p - pos) as usize]).map_err(io)?;
    }
    Ok(p)
}

/// Writes the directory at `pos` and switches the header to it.
fn commit(f: &mut File, pos: u64, h: &mut Header) -> Result<()> {
    let pos = write_padding(f, pos)?;
    let d = h.directory.to_bytes();
    f.write_all(&d).map_err(io)?;
    f.sync_all().map_err(io)?;
    h.generation += 1;
    h.dir_offset = pos;
    h.dir_length = d.len() as u64;
    // overwrite the slot that does not hold the current header: the older generation
    let slot = (h.generation % 2) as u64 * HEADER_SLOT as u64;
    f.seek(SeekFrom::Start(slot)).map_err(io)?;
    f.write_all(&h.slot_bytes()).map_err(io)?;
    f.sync_all().map_err(io)?;
    Ok(())
}

/// Rewrites the file without unused bytes (into a new file that replaces it). Calls
/// `progress(done, total)` with bytes. Returns the bytes freed.
pub fn compact(path: impl AsRef<Path>, progress: &mut dyn FnMut(u64, u64)) -> Result<u64> {
    let path = path.as_ref();
    let h = header(path)?;
    let old_len = std::fs::metadata(path).map_err(io)?.len();
    let tmp = tmp_path(path);
    let src = File::open(path).map_err(io)?;
    // SAFETY: read-only mapping of the old file, which is replaced only by rename.
    let m = unsafe { Mmap::map(&src) }.map_err(io)?;
    let mut out = File::create(&tmp).map_err(io)?;
    out.write_all(&[0u8; HEADER_SIZE]).map_err(io)?;
    let total: u64 = h.sections.iter().map(|s| s.length).sum();
    let mut done = 0u64;
    let mut pos = HEADER_SIZE as u64;
    // old offset -> new offset per section, for the absolute offsets in the block table
    let mut moved: Vec<(u64, u64, u64)> = Vec::new();
    let mut d = h.directory.clone();
    let mut block_table: Option<(usize, usize)> = None;
    for (ei, e) in h.directory.elements.iter().enumerate() {
        for (si, s) in e.sections.iter().enumerate() {
            pos = write_padding(&mut out, pos)?;
            if s.kind == SectionKind::BlockTable as u32 {
                block_table = Some((ei, si));
            }
            let bytes = le::slice(&m, s.offset as usize, s.length as usize)?;
            for chunk in bytes.chunks(64 << 20) {
                out.write_all(chunk).map_err(io)?;
                done += chunk.len() as u64;
                progress(done, total);
            }
            moved.push((s.offset, s.offset + s.length, pos));
            d.elements[ei].sections[si].offset = pos;
            pos += s.length;
        }
    }
    // the block table holds absolute offsets of the blocks
    if let Some((ei, si)) = block_table {
        let s = d.elements[ei].sections[si];
        let old = le::slice(&m, h.directory.elements[ei].sections[si].offset as usize, s.length as usize)?;
        let mut new = Vec::with_capacity(old.len());
        for i in 0..old.len() / BLOCK_ENTRY_SIZE {
            let mut b = BlockEntry::parse(old, i)?;
            let &(a, _, to) = moved
                .iter()
                .find(|(a, z, _)| b.offset >= *a && b.offset < *z)
                .ok_or_else(|| Error::Io(format!("block {i} lies in no section")))?;
            b.offset = b.offset - a + to;
            b.write(&mut new);
        }
        out.seek(SeekFrom::Start(s.offset)).map_err(io)?;
        out.write_all(&new).map_err(io)?;
        d.elements[ei].sections[si].checksum = xxh3_64(&new);
        out.seek(SeekFrom::Start(pos)).map_err(io)?;
    }
    let mut nh = h.clone();
    nh.directory = d;
    nh.generation = 0;
    let pos = write_padding(&mut out, pos)?;
    let db = nh.directory.to_bytes();
    out.write_all(&db).map_err(io)?;
    nh.generation = 1;
    nh.dir_offset = pos;
    nh.dir_length = db.len() as u64;
    out.seek(SeekFrom::Start(0)).map_err(io)?;
    out.write_all(&nh.to_bytes()).map_err(io)?;
    out.sync_all().map_err(io)?;
    drop(m);
    let new_len = out.metadata().map_err(io)?.len();
    std::fs::rename(&tmp, path).map_err(io)?;
    Ok(old_len.saturating_sub(new_len))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".compact-tmp");
    PathBuf::from(p)
}

/// Converts a file of format 1.0 in place: the data stays where it is; a directory is
/// appended and the header is replaced. Calls `progress(done, total)` while it computes
/// checksums of the block sections.
pub fn upgrade(path: impl AsRef<Path>, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
    let path = path.as_ref();
    let created = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(now, |d| d.as_secs() as i64);
    let (mut h, sections) = {
        let f = File::open(path).map_err(io)?;
        // SAFETY: read-only mapping; dropped before the file is written.
        let m = unsafe { Mmap::map(&f) }.map_err(io)?;
        let h = Header::parse_legacy(&m, m.len() as u64)?;
        // the one section of all blocks becomes one per kind of block
        let blocks = h.section(SectionKind::Blocks).copied().ok_or_else(|| Error::Io("no blocks".into()))?;
        let bt = h.section(SectionKind::BlockTable).copied().ok_or_else(|| Error::Io("no block table".into()))?;
        let table = le::slice(&m, bt.offset as usize, bt.length as usize)?;
        let mut ranges: Vec<(SectionKind, u64, u64)> = Vec::new();
        for i in 0..table.len() / BLOCK_ENTRY_SIZE {
            let b = BlockEntry::parse(table, i)?;
            let kind = match b.section {
                0 => SectionKind::BlocksNode,
                1 => SectionKind::BlocksWay,
                2 => SectionKind::BlocksRelation,
                _ => SectionKind::BlocksLarge,
            };
            let end = b.offset + b.length as u64;
            match ranges.last_mut() {
                Some(r) if r.0 == kind => r.2 = end,
                _ => {
                    if ranges.iter().any(|r| r.0 == kind) {
                        return Err(Error::Io("blocks of one kind are not contiguous".into()));
                    }
                    ranges.push((kind, b.offset, end));
                }
            }
        }
        let total: u64 = ranges.iter().map(|r| r.2 - r.1).sum();
        let mut done = 0;
        let mut sections: Vec<SectionEntry> = h.sections.iter().filter(|s| s.kind != SectionKind::Blocks as u32).copied().collect();
        if ranges.first().is_some_and(|r| r.1 < blocks.offset) || ranges.last().is_some_and(|r| r.2 > blocks.offset + blocks.length) {
            return Err(Error::Io("blocks outside the block section".into()));
        }
        for (kind, a, z) in ranges {
            let mut hasher = Xxh3::new();
            for chunk in le::slice(&m, a as usize, (z - a) as usize)?.chunks(64 << 20) {
                hasher.update(chunk);
                done += chunk.len() as u64;
                progress(done, total);
            }
            sections.push(SectionEntry { kind: kind as u32, flags: 0, offset: a, length: z - a, checksum: hasher.digest() });
        }
        (h, sections)
    };
    h.version = knut_format::layout::FORMAT_VERSION;
    h.generation = 0;
    h.directory = base_directory(&sections, created);
    h.sections = sections;
    let mut f = OpenOptions::new().read(true).write(true).open(path).map_err(io)?;
    let pos = f.seek(SeekFrom::End(0)).map_err(io)?;
    // the directory first; only then the header, both slots
    let pos = write_padding(&mut f, pos)?;
    let d = h.directory.to_bytes();
    f.write_all(&d).map_err(io)?;
    f.sync_all().map_err(io)?;
    h.generation = 1;
    h.dir_offset = pos;
    h.dir_length = d.len() as u64;
    f.seek(SeekFrom::Start(0)).map_err(io)?;
    f.write_all(&h.to_bytes()).map_err(io)?;
    f.sync_all().map_err(io)?;
    Ok(())
}

/// True if the file is of format 1.0 (convert it with [`upgrade`]).
pub fn is_legacy(path: impl AsRef<Path>) -> bool {
    let mut b = [0u8; 12];
    File::open(path.as_ref()).and_then(|mut f| f.read_exact(&mut b)).is_ok()
        && b[0..8] == knut_format::layout::MAGIC
        && u32::from_le_bytes([b[8], b[9], b[10], b[11]]) == 1
}

/// Reads all sections of one element.
pub fn element_bytes(path: impl AsRef<Path>, e: &Element) -> Result<Vec<(u32, Vec<u8>)>> {
    let f = File::open(path.as_ref()).map_err(io)?;
    // SAFETY: read-only mapping.
    let m = unsafe { Mmap::map(&f) }.map_err(io)?;
    e.sections.iter().map(|s| Ok((s.kind, le::slice(&m, s.offset as usize, s.length as usize)?.to_vec()))).collect()
}

/// The newest element of a kind.
pub fn find(d: &Directory, kind: ElementKind) -> Option<&Element> {
    d.elements.iter().filter(|e| e.kind == kind).max_by_key(|e| e.id)
}
