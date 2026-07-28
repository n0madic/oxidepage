//! An in-memory HTTP cache (design doc §5.5), RFC 9111 semantics via
//! `http-cache-semantics`.
//!
//! Keyed by `(partition, method, URL)`; `Vary` is honored through the cache
//! policy's request matching. Error responses are never stored. The cache is a
//! pure performance optimization — correctness never depends on it — so a stale
//! entry is simply a miss (no conditional revalidation in Phase 3).
//!
//! One cache is shared by a whole [`Browser`](../../oxidepage_engine) (design
//! §7), so the key carries a [`CachePartition`]: a browsing context must not be
//! able to observe another's traffic through cache timing. A standalone
//! `NetService` uses the default partition and behaves exactly as before
//! (ADR-0027 D7).

use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderMap, StatusCode, Version};
use http_cache_semantics::{BeforeRequest, CachePolicy};

/// Default entry cap before oldest-accessed eviction, for a cache one page
/// owns privately.
pub const DEFAULT_CAP: usize = 256;

/// The isolation key of a cache entry.
///
/// A [`HttpCache`] shared across browsing contexts would otherwise let one
/// context probe another's history through hit/miss timing. Each
/// `BrowserContext` gets its own partition; a standalone `NetService` uses
/// [`CachePartition::default`], so nothing changes for a single page.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub struct CachePartition(pub u64);

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
    entries: HashMap<(CachePartition, String), Entry>,
    /// `last_access` → key, so eviction pops the oldest in `O(log n)`.
    ///
    /// A plain `min_by_key` over `entries` was fine while each page had its own
    /// 256-entry cache; once one cache is shared by every page of every context
    /// and sized for it, that scan runs on every store past the cap while
    /// holding the lock each of those pages does its lookups through.
    by_access: BTreeMap<u64, (CachePartition, String)>,
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
            by_access: BTreeMap::new(),
            cap,
            tick: 0,
        }
    }

    fn key(partition: CachePartition, req: &http::request::Parts) -> (CachePartition, String) {
        (partition, format!("{} {}", req.method, req.uri))
    }

    /// Returns a fresh cached response for `req` within `partition`, if one
    /// applies.
    pub fn get(
        &mut self,
        partition: CachePartition,
        req: &http::request::Parts,
        now: SystemTime,
    ) -> Option<CachedResponse> {
        let key = Self::key(partition, req);
        let tick = self.next_tick();
        let entry = self.entries.get_mut(&key)?;
        match entry.policy.before_request(req, now) {
            BeforeRequest::Fresh(_) => {
                self.by_access.remove(&entry.last_access);
                entry.last_access = tick;
                self.by_access.insert(tick, key.clone());
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

    /// Stores a response in `partition` if it is cacheable and not an error.
    pub fn store(
        &mut self,
        partition: CachePartition,
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
        let key = Self::key(partition, req);
        if let Some(previous) = self.entries.get(&key) {
            self.by_access.remove(&previous.last_access);
        }
        self.by_access.insert(tick, key.clone());
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
            let Some((&oldest, _)) = self.by_access.iter().next() else {
                break;
            };
            let Some(key) = self.by_access.remove(&oldest) else {
                break;
            };
            self.entries.remove(&key);
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
            CachePartition::default(),
            &req("http://x/a"),
            &res("max-age=60"),
            Bytes::from_static(b"hi"),
            now,
        );
        let got = cache
            .get(CachePartition::default(), &req("http://x/a"), now)
            .expect("fresh hit");
        assert_eq!(&got.body[..], b"hi");
    }

    #[test]
    fn no_store_is_not_cached() {
        let mut cache = HttpCache::default();
        let now = SystemTime::now();
        cache.store(
            CachePartition::default(),
            &req("http://x/b"),
            &res("no-store"),
            Bytes::from_static(b"hi"),
            now,
        );
        assert_eq!(cache.len(), 0);
        assert!(
            cache
                .get(CachePartition::default(), &req("http://x/b"), now)
                .is_none()
        );
    }

    #[test]
    fn stale_entry_is_a_miss() {
        let mut cache = HttpCache::default();
        let now = SystemTime::now();
        cache.store(
            CachePartition::default(),
            &req("http://x/c"),
            &res("max-age=1"),
            Bytes::from_static(b"hi"),
            now,
        );
        let later = now + std::time::Duration::from_secs(5);
        assert!(
            cache
                .get(CachePartition::default(), &req("http://x/c"), later)
                .is_none()
        );
    }

    #[test]
    fn partitions_do_not_share_entries() {
        let mut cache = HttpCache::default();
        let now = SystemTime::now();
        let (a, b) = (CachePartition(1), CachePartition(2));
        cache.store(
            a,
            &req("http://x/shared"),
            &res("max-age=60"),
            Bytes::from_static(b"from-a"),
            now,
        );
        // The same URL in another partition is a miss, however fresh the entry.
        assert!(cache.get(b, &req("http://x/shared"), now).is_none());
        let hit = cache.get(a, &req("http://x/shared"), now).expect("own hit");
        assert_eq!(&hit.body[..], b"from-a");

        // ... and a store in `b` does not overwrite `a`.
        cache.store(
            b,
            &req("http://x/shared"),
            &res("max-age=60"),
            Bytes::from_static(b"from-b"),
            now,
        );
        assert_eq!(cache.len(), 2);
        assert_eq!(
            &cache.get(a, &req("http://x/shared"), now).unwrap().body[..],
            b"from-a"
        );
        assert_eq!(
            &cache.get(b, &req("http://x/shared"), now).unwrap().body[..],
            b"from-b"
        );
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
            CachePartition::default(),
            &req("http://x/d"),
            &err_res,
            Bytes::from_static(b"err"),
            now,
        );
        assert_eq!(cache.len(), 0);
    }
}
