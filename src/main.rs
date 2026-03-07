use std::num::{NonZeroUsize};
use std::time::Duration;
use crate::lru_cache::LruCache;

pub mod lru_cache;

pub fn main() {
    let mut lru_cache: LruCache<usize,usize> = LruCache::new(NonZeroUsize::new(5).unwrap());
    lru_cache.put(5,3,Duration::from_secs(50));
    println!("{:?}", lru_cache)
}

