//! Sharded LRU cache of decoded blocks.

use crate::file::DecodedBlock;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

const SHARDS: usize = 16;

pub struct BlockCache {
    shards: Vec<Mutex<LruCache<u32, Arc<DecodedBlock>>>>,
}

impl BlockCache {
    pub fn new(capacity: usize) -> Self {
        let per = NonZeroUsize::new(capacity.div_ceil(SHARDS).max(1)).unwrap();
        BlockCache { shards: (0..SHARDS).map(|_| Mutex::new(LruCache::new(per))).collect() }
    }

    pub fn get(&self, no: u32) -> Option<Arc<DecodedBlock>> {
        self.shards[no as usize % SHARDS].lock().unwrap().get(&no).cloned()
    }

    pub fn put(&self, no: u32, b: Arc<DecodedBlock>) {
        self.shards[no as usize % SHARDS].lock().unwrap().put(no, b);
    }
}
