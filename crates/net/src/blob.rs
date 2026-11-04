//! The blob URL store (File API §8, ADR-0042).
//!
//! `URL.createObjectURL(blob)` mints a `blob:<origin>/<uuid>` URL and records
//! the blob's bytes here; every fetch of that URL is then answered from this
//! table, above the scheme gate, exactly as `data:` is (ADR-0029). So scripts,
//! modules, sheets, images, fonts, frames and `fetch`/XHR all resolve a blob
//! URL without one of them knowing the scheme exists, and a redirect from
//! `http:` to `blob:` stays a network error: the redirect loop re-checks the
//! scheme gate per hop, and `blob` is deliberately not in it.
//!
//! One store per page (one [`FetchEngine`](crate::FetchEngine), shared by its
//! clones). File API resolves a blob URL within the creator's *storage
//! partition*, and a page — its top-level document and every frame in it — is
//! that partition here. Another page, even of the same site, does not see
//! these entries: its URL resolves to nothing, which is a network error.

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use oxidepage_base::FrameId;
use url::{Position, Url};

/// One registered blob URL.
#[derive(Clone, Debug)]
struct Entry {
    bytes: Bytes,
    content_type: String,
    /// The browsing context whose document created it, so its unload can
    /// revoke it (File API: a blob URL lives as long as its creator's
    /// document).
    creator: FrameId,
}

/// Why a registration was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobUrlError {
    /// The store would grow past its byte cap.
    TooLarge,
}

/// The page's blob URL entries, keyed by URL without its fragment.
#[derive(Debug)]
pub struct BlobUrlStore {
    entries: Mutex<HashMap<String, Entry>>,
    /// The most bytes the store holds at once. Every entry is a copy of a
    /// script-built blob, so a loop of `createObjectURL` without a matching
    /// `revokeObjectURL` would otherwise grow the page without limit.
    max_bytes: u64,
}

impl BlobUrlStore {
    #[must_use]
    pub fn new(max_bytes: u64) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records `url` → `bytes`. Refused, recording nothing, when the store's
    /// total would pass its cap.
    pub fn register(
        &self,
        url: &str,
        bytes: Vec<u8>,
        content_type: String,
        creator: FrameId,
    ) -> Result<(), BlobUrlError> {
        let mut entries = self.lock();
        let held: u64 = entries.values().map(|e| e.bytes.len() as u64).sum();
        if held.saturating_add(bytes.len() as u64) > self.max_bytes {
            return Err(BlobUrlError::TooLarge);
        }
        entries.insert(
            url.to_owned(),
            Entry {
                bytes: Bytes::from(bytes),
                content_type,
                creator,
            },
        );
        Ok(())
    }

    /// `URL.revokeObjectURL`: forgets `url`. Unknown URLs are ignored.
    pub fn revoke(&self, url: &str) {
        self.lock().remove(url);
    }

    /// Forgets every URL `frame`'s documents created: that frame's document
    /// was replaced or the frame itself is gone.
    pub fn revoke_created_by(&self, frame: FrameId) {
        self.lock().retain(|_, entry| entry.creator != frame);
    }

    /// The bytes and type behind `url`, if it is registered. The fragment is
    /// not part of the key: `blob:…#page=2` names the same blob.
    #[must_use]
    pub fn resolve(&self, url: &Url) -> Option<(Bytes, String)> {
        let key = &url[..Position::AfterQuery];
        self.lock()
            .get(key)
            .map(|entry| (entry.bytes.clone(), entry.content_type.clone()))
    }

    /// How many URLs are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no URL is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u32) -> FrameId {
        FrameId::from_parts(n, std::num::NonZeroU32::new(1).unwrap())
    }

    #[test]
    fn a_registered_url_resolves_until_revoked_and_ignores_its_fragment() {
        let store = BlobUrlStore::new(1024);
        let url = "blob:https://a.test/0000";
        store
            .register(url, b"hi".to_vec(), "text/plain".into(), frame(1))
            .unwrap();
        let with_fragment = Url::parse(&format!("{url}#x")).unwrap();
        assert_eq!(
            store.resolve(&with_fragment),
            Some((Bytes::from_static(b"hi"), "text/plain".into()))
        );
        store.revoke(url);
        assert_eq!(store.resolve(&with_fragment), None);
    }

    #[test]
    fn unloading_the_creator_revokes_only_its_urls() {
        let store = BlobUrlStore::new(1024);
        store
            .register("blob:x/1", vec![1], String::new(), frame(1))
            .unwrap();
        store
            .register("blob:x/2", vec![2], String::new(), frame(2))
            .unwrap();
        store.revoke_created_by(frame(1));
        assert_eq!(store.len(), 1);
        assert!(store.resolve(&Url::parse("blob:x/2").unwrap()).is_some());
    }

    #[test]
    fn the_store_refuses_past_its_cap() {
        let store = BlobUrlStore::new(3);
        store
            .register("blob:x/1", vec![0; 2], String::new(), frame(1))
            .unwrap();
        assert_eq!(
            store.register("blob:x/2", vec![0; 2], String::new(), frame(1)),
            Err(BlobUrlError::TooLarge)
        );
        assert_eq!(store.len(), 1);
    }
}
