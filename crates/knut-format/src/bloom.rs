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

// ---- block filters: one per block, sized to its tags (BlockBlooms section) ---------------

/// Bits per distinct key and key=value pair: about 1 % false positives with [`BLOCK_K`] probes.
pub const BLOCK_BITS_PER_ITEM: usize = 10;
pub const BLOCK_K: u32 = 7;

/// Number of 64-bit words for a block filter with `n_items` distinct hashes.
pub fn block_words(n_items: usize) -> usize {
    (n_items * BLOCK_BITS_PER_ITEM).div_ceil(64).max(1)
}

/// The bit positions of `h` in a filter of `m` bits (double hashing).
#[inline]
fn block_bits(h: u64, m: u64) -> impl Iterator<Item = u64> {
    let (h1, h2) = (h & 0xffff_ffff, (h >> 32) | 1);
    (0..BLOCK_K as u64).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % m)
}

/// A block filter containing `hashes`.
pub fn block_filter(hashes: &[u64]) -> Vec<u64> {
    let mut w = vec![0u64; block_words(hashes.len())];
    let m = w.len() as u64 * 64;
    for &h in hashes {
        for b in block_bits(h, m) {
            w[(b / 64) as usize] |= 1 << (b % 64);
        }
    }
    w
}

/// Whether a block filter (given as little-endian words) may contain `h`.
#[inline]
pub fn block_test(words: &[u8], h: u64) -> bool {
    let m = (words.len() / 8) as u64 * 64;
    if m == 0 {
        return true;
    }
    block_bits(h, m).all(|b| words[(b / 8) as usize] & (1 << (b % 8)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_filter_finds_its_items_and_rejects_most_others() {
        let items: Vec<u64> = (0..400u32).map(|i| hash_kv(1, i)).collect();
        let w = block_filter(&items);
        let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        assert!(items.iter().all(|&h| block_test(&bytes, h)));
        let fp = (1000..21_000u32).filter(|&i| block_test(&bytes, hash_kv(1, i))).count();
        assert!(fp < 400, "false positives: {fp} of 20000");
    }
}
