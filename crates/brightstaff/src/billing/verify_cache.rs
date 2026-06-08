use super::talos::VerifyResponse;
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct CachedEntry {
    response: VerifyResponse,
    inserted_at: Instant,
}

pub struct VerifyCache {
    cache: Mutex<LruCache<String, CachedEntry>>,
    ttl: Duration,
}

fn cache_key(credential: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(credential.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl VerifyCache {
    pub fn new(max_entries: usize, ttl_secs: u64) -> Self {
        Self {
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::new(1024).unwrap()),
            )),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    pub fn get(&self, credential: &str) -> Option<VerifyResponse> {
        let key = cache_key(credential);
        let mut cache = self.cache.lock().expect("verify cache mutex poisoned");
        if let Some(entry) = cache.get(&key) {
            if entry.inserted_at.elapsed() < self.ttl {
                return Some(entry.response.clone());
            }
            cache.pop(&key);
        }
        None
    }

    pub fn insert(&self, credential: &str, response: VerifyResponse) {
        let mut cache = self.cache.lock().expect("verify cache mutex poisoned");
        cache.put(
            cache_key(credential),
            CachedEntry {
                response,
                inserted_at: Instant::now(),
            },
        );
    }
}
