//! String tables: `varint n`, then `n × (varint len, utf8 bytes)`.

use crate::codec::{get_varint, put_varint};
use crate::error::corrupt;
use crate::{Error, Result, Str};

pub fn encode_strings<S: AsRef<str>>(strings: &[S]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, strings.len() as u64);
    for s in strings {
        let b = s.as_ref().as_bytes();
        put_varint(&mut out, b.len() as u64);
        out.extend_from_slice(b);
    }
    out
}

pub fn decode_strings(buf: &[u8]) -> Result<Vec<Str>> {
    let mut pos = 0;
    let n = get_varint(buf, &mut pos)? as usize;
    if n > buf.len() {
        return Err(corrupt("string table count"));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = get_varint(buf, &mut pos)? as usize;
        let end = pos.checked_add(len).ok_or(Error::Truncated("string"))?;
        let b = buf.get(pos..end).ok_or(Error::Truncated("string"))?;
        let s = std::str::from_utf8(b).map_err(|_| corrupt("invalid utf-8 in string table"))?;
        out.push(Str::from(s));
        pos = end;
    }
    Ok(out)
}
