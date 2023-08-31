//! 512-bit Bloom filters for tag keys and key=value pairs (block pruning).

use xxhash_rust::xxh3::xxh3_64;

pub const BLOOM_WORDS: usize = 8;
pub type Bloom = [u64; BLOOM_WORDS];

pub fn hash_key(key_id: u32) -> u64 {
    let mut b = [0u8; 5];
    b[0] = b'K';
    b[1..5].copy_from_slice(&key_id.to_le_bytes());
    xxh3_64(&b)
}

pub fn hash_kv(key_id: u32, value_id: u32) -> u64 {
    let mut b = [0u8; 9];
    b[0] = b'V';
    b[1..5].copy_from_slice(&key_id.to_le_bytes());
    b[5..9].copy_from_slice(&value_id.to_le_bytes());
    xxh3_64(&b)
}

#[inline]
pub fn add(bloom: &mut Bloom, h: u64) {
    for bit in [h & 511, (h >> 9) & 511] {
        bloom[(bit / 64) as usize] |= 1u64 << (bit % 64);
    }
}

#[inline]
pub fn test(bloom: &Bloom, h: u64) -> bool {
    [h & 511, (h >> 9) & 511].iter().all(|&bit| bloom[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0)
}

pub fn or(a: &mut Bloom, b: &Bloom) {
    for i in 0..BLOOM_WORDS {
        a[i] |= b[i];
    }
}
