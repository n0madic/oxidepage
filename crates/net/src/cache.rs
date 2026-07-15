//! An in-memory HTTP cache (design doc §5.5), RFC 9111 semantics via
//! `http-cache-semantics`.
//!
//! Keyed by `(method, URL)`; `Vary` is honored through the cache policy's
//! request matching. Error responses are never stored. The cache is a pure
//! performance optimization — correctness never depends on it — so a stale
//! entry is simply a miss (no conditional revalidation in Phase 3).

use std::collections::HashMap;
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderMap, StatusCode, Version};
use http_cache_semantics::{BeforeRequest, CachePolicy};

/// Default entry cap before oldest-accessed eviction.
const DEFAULT_CAP: usize = 256;

/// A response reconstructed from the cache.
pub struct CachedResponse {
    pub status: StatusCode,
    pub version: Version,
    pub headers: HeaderMap,
    pub body: Bytes,
}

struct Entry {
    policy: CachePolicy,
    status: StatusCode,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
    last_access: u64,
}

/// An LRU-ish in-memory response cache.
pub struct HttpCache {
    entries: HashMap<String, Entry>,
    cap: usize,
    tick: u64,
}

impl Default for HttpCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAP)
    }
}

impl HttpCache {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            cap,
            tick: 0,
        }
    }

    fn key(req: &http::request::Parts) -> String {
        format!("{} {}", req.method, req.uri)
    }

    /// Returns a fresh cached response for `req`, if one applies.
    pub fn get(&mut self, req: &http::request::Parts, now: SystemTime) -> Option<CachedResponse> {
        let key = Self::key(req);
        let tick = self.next_tick();
        let entry = self.entries.get_mut(&key)?;
        match entry.policy.before_request(req, now) {
            BeforeRequest::Fresh(_) => {
                entry.last_access = tick;
                Some(CachedResponse {
                    status: entry.status,
                    version: entry.version,
                    headers: entry.headers.clone(),
                    body: entry.body.clone(),
                })
            }
            // Stale (or Vary mismatch): treat as a miss.
            BeforeRequest::Stale { .. } => None,
        }
    }

    /// Stores a response if it is cacheable and not an error.
    pub fn store(
        &mut self,
        req: &http::request::Parts,
        res: &http::response::Parts,
        body: Bytes,
        now: SystemTime,
    ) {
        // Never cache server errors; only cache storable success/redirect/404.
        if res.status.is_server_error() {
            return;
        }
        let policy =
            CachePolicy::new_options(req, res, now, http_cache_semantics::CacheOptions::default());
        if !policy.is_storable() {
            return;
        }
        let tick = self.next_tick();
        let key = Self::key(req);
        self.entries.insert(
            key,
            Entry {
                policy,
                status: res.status,
                version: res.version,
                headers: res.headers.clone(),
                body,
                last_access: tick,
            },
        );
        self.evict();
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn evict(&mut self) {
        while self.entries.len() > self.cap {
            if let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&key);
            } else {
                break;
            }
        }
    }

    /// Number of cached entries (test/introspection aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(url: &str) -> http::request::Parts {
        http::Request::builder()
            .method("GET")
            .uri(url)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn res(cache_control: &str) -> http::response::Parts {
        http::Response::builder()
            .status(200)
            .header("cache-control", cache_control)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[test]
    fn fresh_entry_is_served() {
        let mut cache = HttpCache::default();
        let now = SystemTime::now();
        cache.store(
            &req("http://x/a"),
            &res("max-age=60"),
            Bytes::from_static(b"hi"),
            now,
        );
        let got = cache.get(&req("http://x/a"), now).expect("fresh hit");
        assert_eq!(&got.body[..], b"hi");
    }

    #[test]
    fn no_store_is_not_cached() {
        let mut cache = HttpCache::default();
        let now = SystemTime::now();
        cache.store(
            &req("http://x/b"),
            &res("no-store"),
            Bytes::from_static(b"hi"),
            now,
        );
        assert_eq!(cache.len(), 0);
        assert!(cache.get(&req("http://x/b"), now).is_none());
    }

    #[test]
    fn stale_entry_is_a_miss() {
        let mut cache = HttpCache::default();
        let now = SystemTime::now();
        cache.store(
            &req("http://x/c"),
            &res("max-age=1"),
            Bytes::from_static(b"hi"),
            now,
        );
        let later = now + std::time::Duration::from_secs(5);
        assert!(cache.get(&req("http://x/c"), later).is_none());
    }

    #[test]
    fn server_error_never_cached() {
        let mut cache = HttpCache::default();
        let now = SystemTime::now();
        let err_res = http::Response::builder()
            .status(500)
            .header("cache-control", "max-age=60")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        cache.store(
            &req("http://x/d"),
            &err_res,
            Bytes::from_static(b"err"),
            now,
        );
        assert_eq!(cache.len(), 0);
    }
}
