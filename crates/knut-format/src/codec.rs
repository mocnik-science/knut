//! Primitive codecs: varint, zigzag, FOR bitpacking and self-describing columns.

use crate::error::corrupt;
use crate::{Error, Result};

/// Upper bound for the number of values in one column (protects against huge allocations
/// from corrupt input).
pub const MAX_COLUMN_LEN: usize = 1 << 24;

pub const CODEC_VARINT: u8 = 0;
pub const CODEC_BITPACK: u8 = 1;
pub const CODEC_CONST: u8 = 2;
pub const CODEC_BYTES: u8 = 3;

#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ (-((u & 1) as i64))
}

#[inline]
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline]
pub fn varint_len(v: u64) -> usize {
    let bits = 64 - (v | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

#[inline]
pub fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    // fast paths for the common 1- and 2-byte encodings
    if let Some(&b0) = buf.get(*pos) {
        if b0 < 0x80 {
            *pos += 1;
            return Ok(b0 as u64);
        }
        if let Some(&b1) = buf.get(*pos + 1) {
            if b1 < 0x80 {
                *pos += 2;
                return Ok((b0 & 0x7f) as u64 | (b1 as u64) << 7);
            }
        }
    }
    get_varint_slow(buf, pos)
}

#[cold]
fn get_varint_slow(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos).ok_or(Error::Truncated("varint"))?;
        *pos += 1;
        if shift == 63 && b > 1 {
            return Err(corrupt("varint overflow"));
        }
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 63 {
            return Err(corrupt("varint too long"));
        }
    }
}

#[inline]
pub fn get_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *buf.get(*pos).ok_or(Error::Truncated("u8"))?;
    *pos += 1;
    Ok(b)
}

/// Number of bits needed to represent `v`.
#[inline]
pub fn bit_width(v: u64) -> u32 {
    64 - v.leading_zeros()
}

/// Packs values LSB-first into `out` using `width` bits each.
pub fn bitpack(vals: impl IntoIterator<Item = u64>, width: u32, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    let mut acc: u128 = 0;
    let mut nbits = 0u32;
    for v in vals {
        acc |= (v as u128) << nbits;
        nbits += width;
        while nbits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            nbits -= 8;
        }
    }
    if nbits > 0 {
        out.push(acc as u8);
    }
}

/// Bytes occupied by `n` values of `width` bits.
#[inline]
pub fn bitpacked_len(n: usize, width: u32) -> usize {
    (n * width as usize).div_ceil(8)
}

/// Unpacks `n` values of `width` bits from the start of `buf`, calling `f` for each.
/// Returns the number of bytes consumed.
pub fn bitunpack(buf: &[u8], n: usize, width: u32, mut f: impl FnMut(u64)) -> Result<usize> {
    if width > 64 {
        return Err(corrupt("bit width > 64"));
    }
    let nbytes = bitpacked_len(n, width);
    if nbytes > buf.len() {
        return Err(Error::Truncated("bitpacked data"));
    }
    if width == 0 {
        for _ in 0..n {
            f(0);
        }
        return Ok(0);
    }
    let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    if width <= 56 {
        // word-at-a-time: read 8 bytes at the value's byte offset while 8 bytes remain
        let mut bit = 0usize;
        let mut i = 0usize;
        while i < n {
            let byte = bit >> 3;
            if byte + 8 > buf.len() {
                break;
            }
            let w = u64::from_le_bytes(buf[byte..byte + 8].try_into().unwrap());
            f((w >> (bit & 7)) & mask);
            bit += width as usize;
            i += 1;
        }
        // tail (last few values near the end of the buffer)
        while i < n {
            let mut v = 0u64;
            let mut got = 0u32;
            let mut b = bit;
            while got < width {
                let byte = buf[b >> 3] as u64;
                let off = (b & 7) as u32;
                let take = (8 - off).min(width - got);
                v |= ((byte >> off) & ((1u64 << take) - 1)) << got;
                got += take;
                b += take as usize;
            }
            f(v);
            bit += width as usize;
            i += 1;
        }
        return Ok(nbytes);
    }
    let mut acc: u128 = 0;
    let mut nbits = 0u32;
    let mut p = 0usize;
    for _ in 0..n {
        while nbits < width {
            acc |= (buf[p] as u128) << nbits;
            p += 1;
            nbits += 8;
        }
        f((acc as u64) & mask);
        acc >>= width;
        nbits -= width;
    }
    Ok(nbytes)
}

/// Writes a u64 column choosing the smallest of varint / FOR-bitpack / const.
pub fn put_u64_column(out: &mut Vec<u8>, vals: &[u64]) {
    let n = vals.len();
    if n == 0 {
        out.push(CODEC_CONST);
        put_varint(out, 0);
        put_varint(out, 0);
        return;
    }
    let mut min = u64::MAX;
    let mut max = 0u64;
    let mut vsize = 0usize;
    for &v in vals {
        min = min.min(v);
        max = max.max(v);
        vsize += varint_len(v);
    }
    if min == max {
        out.push(CODEC_CONST);
        put_varint(out, n as u64);
        put_varint(out, min);
        return;
    }
    let width = bit_width(max - min);
    let bsize = varint_len(min) + 1 + bitpacked_len(n, width);
    if bsize < vsize {
        out.push(CODEC_BITPACK);
        put_varint(out, n as u64);
        put_varint(out, min);
        out.push(width as u8);
        bitpack(vals.iter().map(|&v| v - min), width, out);
    } else {
        out.push(CODEC_VARINT);
        put_varint(out, n as u64);
        for &v in vals {
            put_varint(out, v);
        }
    }
}

/// Reads a u64 column written by [`put_u64_column`].
pub fn get_u64_column(buf: &[u8], pos: &mut usize) -> Result<Vec<u64>> {
    let mut budget = MAX_COLUMN_LEN;
    get_u64_column_budget(buf, pos, &mut budget)
}

/// Value budget for all columns of one frame: corrupt length fields cannot make the
/// decoder allocate more than a small multiple of the frame size.
pub fn frame_budget(frame_len: usize) -> usize {
    (frame_len.saturating_mul(64)).max(1 << 22)
}

/// Like [`get_u64_column`] but charges the number of values against `budget`.
pub fn get_u64_column_budget(buf: &[u8], pos: &mut usize, budget: &mut usize) -> Result<Vec<u64>> {
    let codec = get_u8(buf, pos)?;
    let n = get_varint(buf, pos)? as usize;
    if n > MAX_COLUMN_LEN || n > *budget {
        return Err(corrupt("column too long"));
    }
    *budget -= n;
    match codec {
        CODEC_CONST => {
            let v = get_varint(buf, pos)?;
            Ok(vec![v; n])
        }
        CODEC_VARINT => {
            if n > buf.len().saturating_sub(*pos) {
                return Err(Error::Truncated("varint column"));
            }
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(get_varint(buf, pos)?);
            }
            Ok(out)
        }
        CODEC_BITPACK => {
            let min = get_varint(buf, pos)?;
            let width = get_u8(buf, pos)? as u32;
            let rest = buf.get(*pos..).ok_or(Error::Truncated("bitpack column"))?;
            if bitpacked_len(n, width.min(64)) > rest.len() {
                return Err(Error::Truncated("bitpack column"));
            }
            let mut out = Vec::with_capacity(n);
            let used = bitunpack(rest, n, width, |v| out.push(min.wrapping_add(v)))?;
            *pos += used;
            Ok(out)
        }
        _ => Err(corrupt(format!("unknown column codec {codec}"))),
    }
}

pub fn put_bytes_column(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(CODEC_BYTES);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub fn get_bytes_column<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let codec = get_u8(buf, pos)?;
    if codec != CODEC_BYTES {
        return Err(corrupt("expected bytes column"));
    }
    let n = get_varint(buf, pos)? as usize;
    let end = pos.checked_add(n).ok_or(Error::Truncated("bytes column"))?;
    let s = buf.get(*pos..end).ok_or(Error::Truncated("bytes column"))?;
    *pos = end;
    Ok(s)
}

/// Sequential reader over a decoded column with bounds checking.
#[derive(Debug, Default)]
pub struct Col {
    vals: Vec<u64>,
    pos: usize,
    name: &'static str,
}

impl Col {
    pub fn read(buf: &[u8], pos: &mut usize, name: &'static str) -> Result<Col> {
        Ok(Col { vals: get_u64_column(buf, pos)?, pos: 0, name })
    }

    pub fn read_budget(buf: &[u8], pos: &mut usize, name: &'static str, budget: &mut usize) -> Result<Col> {
        Ok(Col { vals: get_u64_column_budget(buf, pos, budget)?, pos: 0, name })
    }

    pub fn from_vec(vals: Vec<u64>, name: &'static str) -> Col {
        Col { vals, pos: 0, name }
    }

    #[inline]
    pub fn next(&mut self) -> Result<u64> {
        let v = *self
            .vals
            .get(self.pos)
            .ok_or_else(|| corrupt(format!("column {} exhausted", self.name)))?;
        self.pos += 1;
        Ok(v)
    }

    #[inline]
    pub fn next_i(&mut self) -> Result<i64> {
        Ok(unzigzag(self.next()?))
    }

    #[inline]
    pub fn next_u32(&mut self) -> Result<u32> {
        let v = self.next()?;
        u32::try_from(v).map_err(|_| corrupt(format!("column {} value out of range", self.name)))
    }

    pub fn len(&self) -> usize {
        self.vals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vals.is_empty()
    }

    pub fn remaining(&self) -> usize {
        self.vals.len() - self.pos
    }

    pub fn values(&self) -> &[u64] {
        &self.vals
    }
}

/// Little-endian fixed-width reads with bounds checks.
pub mod le {
    use crate::{Error, Result};

    #[inline]
    pub fn slice(buf: &[u8], off: usize, len: usize) -> Result<&[u8]> {
        let end = off.checked_add(len).ok_or(Error::Truncated("offset overflow"))?;
        buf.get(off..end).ok_or(Error::Truncated("out of bounds"))
    }
    #[inline]
    pub fn u8(buf: &[u8], off: usize) -> Result<u8> {
        Ok(slice(buf, off, 1)?[0])
    }
    #[inline]
    pub fn u16(buf: &[u8], off: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(slice(buf, off, 2)?.try_into().unwrap()))
    }
    #[inline]
    pub fn u32(buf: &[u8], off: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(slice(buf, off, 4)?.try_into().unwrap()))
    }
    #[inline]
    pub fn i32(buf: &[u8], off: usize) -> Result<i32> {
        Ok(i32::from_le_bytes(slice(buf, off, 4)?.try_into().unwrap()))
    }
    #[inline]
    pub fn u64(buf: &[u8], off: usize) -> Result<u64> {
        Ok(u64::from_le_bytes(slice(buf, off, 8)?.try_into().unwrap()))
    }
    #[inline]
    pub fn i64(buf: &[u8], off: usize) -> Result<i64> {
        Ok(i64::from_le_bytes(slice(buf, off, 8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn varint_roundtrip(v: u64) {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            prop_assert_eq!(b.len(), varint_len(v));
            let mut p = 0;
            prop_assert_eq!(get_varint(&b, &mut p).unwrap(), v);
            prop_assert_eq!(p, b.len());
        }

        #[test]
        fn zigzag_roundtrip(v: i64) {
            prop_assert_eq!(unzigzag(zigzag(v)), v);
        }

        #[test]
        fn column_roundtrip(vals in prop::collection::vec(any::<u64>(), 0..300),
                            small in prop::collection::vec(0u64..1000, 0..300),
                            c in any::<u64>(), n in 0usize..50) {
            for v in [vals, small, vec![c; n]] {
                let mut b = Vec::new();
                put_u64_column(&mut b, &v);
                let mut p = 0;
                prop_assert_eq!(get_u64_column(&b, &mut p).unwrap(), v);
                prop_assert_eq!(p, b.len());
            }
        }

        #[test]
        fn column_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
            let mut p = 0;
            let _ = get_u64_column(&bytes, &mut p);
        }

        #[test]
        fn bitpack_roundtrip(vals in prop::collection::vec(any::<u64>(), 0..200), w in 0u32..=64) {
            let mask = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
            let vals: Vec<u64> = vals.into_iter().map(|v| v & mask).collect();
            let mut b = Vec::new();
            bitpack(vals.iter().copied(), w, &mut b);
            prop_assert_eq!(b.len(), bitpacked_len(vals.len(), w));
            let mut out = Vec::new();
            bitunpack(&b, vals.len(), w, |v| out.push(v)).unwrap();
            prop_assert_eq!(out, vals);
        }
    }
}
