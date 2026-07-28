//! Web Storage: the Rust side of `localStorage` / `sessionStorage`
//! (ADR-0027 D13).
//!
//! Storage used to be a JavaScript `Map` in `bootstrap.js`, which is fine for
//! one page and wrong for a browser: `localStorage` is shared by every document
//! of one origin in one browsing context, and a write in one of them fires a
//! `storage` event in the others. Neither is expressible from inside a single
//! realm, so the area moved here — behind an `Arc<Mutex<_>>` that all the pages
//! of a [`BrowserContext`] share.
//!
//! The `Storage` *interface* is a real IDL interface (`HostData::Storage`),
//! which is the idiom of this codebase: ADR-0022 removed the `__oxide_*` global
//! pattern in favour of real interfaces, and codegen then makes an IDL change
//! surface as a compile error rather than as silent drift. The JS `Proxy` in
//! `bootstrap.js` stays, because it *is* the named-property surface
//! (`s.foo`, `delete s.foo`, `Object.keys(s)`) that WebIDL's
//! `[LegacyUnenumerableNamedProperties]` describes and this engine has no
//! other way to express.
//!
//! [`BrowserContext`]: https://docs.rs/oxidepage-engine

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Per-origin quota, matching what browsers converge on. A write that would
/// exceed it throws `QuotaExceededError` and stores nothing.
pub const STORAGE_QUOTA_BYTES: usize = 5 * 1024 * 1024;

/// Which of a document's two storage areas.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum StorageAreaKind {
    /// Shared by every page of one browsing context at one origin.
    Local,
    /// Private to one page.
    Session,
}

impl StorageAreaKind {
    /// The `Window` property this area is exposed as.
    #[must_use]
    pub fn global_name(self) -> &'static str {
        match self {
            Self::Local => "localStorage",
            Self::Session => "sessionStorage",
        }
    }
}

/// A write another document of the same area must be told about — the payload
/// of HTML's `storage` event.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StorageNotification {
    /// The key that changed; `None` for `clear()`.
    pub key: Option<String>,
    /// The value before the change, if any.
    pub old_value: Option<String>,
    /// The value after the change; `None` for a removal or a `clear()`.
    pub new_value: Option<String>,
    /// Which area changed, so a listener can compare `event.storageArea`.
    pub kind: StorageAreaKind,
    /// The URL of the document that *wrote*. HTML's `StorageEvent.url` is the
    /// source document's, not the receiver's — cross-tab sync libraries route
    /// on it, and filling it in from the receiver would make every peer look
    /// identical.
    pub url: String,
    /// The storage key of the area this happened in.
    ///
    /// Delivery is deliberately not done under the area's lock, so a subscriber
    /// list is a *snapshot*: a page can unsubscribe and navigate to another
    /// origin between the snapshot and the call. Without this the notification
    /// would still land in that page's queue and be dispatched as a `storage`
    /// event at the new document — leaking the old origin's key and values to
    /// script that must not see them. The receiver compares it against the
    /// origin it is currently on and drops anything stale.
    pub origin: String,
}

/// Identifies one subscriber, so a write is never delivered back to the
/// document that made it (HTML: the *other* documents).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct StorageSubscriber(u64);

static NEXT_SUBSCRIBER: AtomicU64 = AtomicU64::new(1);

impl StorageSubscriber {
    #[must_use]
    pub fn next() -> Self {
        Self(NEXT_SUBSCRIBER.fetch_add(1, Ordering::Relaxed))
    }

    /// A process-unique number for this subscriber, used to key the storage of
    /// documents that have no shareable origin (see `storage_origin_of`).
    #[must_use]
    pub fn id(self) -> u64 {
        self.0
    }
}

/// A live subscription: whoever installed it gets every write made by someone
/// else. `Send + Sync` because the subscriber is on another OS thread.
///
/// Returns whether the subscriber is **still alive**. A page thread that
/// panics never reaches its `unsubscribe`, so without a way to say "I am gone"
/// its entry would sit on a context-lifetime area forever, being handed a
/// notification on every sibling write.
pub type Notify = Arc<dyn Fn(StorageNotification) -> bool + Send + Sync>;

/// One origin's storage.
///
/// A `BTreeMap`, not a `HashMap`: `Storage.key(i)` indexes into the key list,
/// and while the spec leaves the order to the user agent, an order that changed
/// between two reads would make `for (let i = 0; i < s.length; i++) s.key(i)`
/// — the documented way to enumerate — skip and repeat keys. Sorted order costs
/// nothing here and no dependency.
#[derive(Default)]
pub struct StorageArea {
    map: BTreeMap<String, String>,
    /// Bumped on every mutation, so a cached key list can tell whether it is
    /// still current without comparing the lists.
    version: u64,
    /// Cumulative UTF-16 code-unit cost of every key and value, against
    /// [`STORAGE_QUOTA_BYTES`] — the unit browsers charge, so a page that
    /// budgets against the documented 5 MiB gets the same answer here whether
    /// it stores ASCII, CJK or emoji.
    bytes: usize,
    subscribers: Vec<(StorageSubscriber, Notify)>,
}

/// A storage area shared by every page that should see the same data.
pub type SharedStorage = Arc<Mutex<StorageArea>>;

/// A write refused because it would exceed the quota.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct QuotaExceeded;

/// The quota cost of one entry, in UTF-16 code units (what browsers charge).
/// `str::len` would be UTF-8 bytes and would give a CJK or emoji page 1.5–3×
/// the advertised quota.
fn cost(key: &str, value: &str) -> usize {
    key.encode_utf16().count() + value.encode_utf16().count()
}

impl StorageArea {
    #[must_use]
    pub fn shared() -> SharedStorage {
        Arc::new(Mutex::new(Self::default()))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The mutation counter a cached key list is validated against.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Every key, in the order `Storage.key(i)` indexes them.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        self.map.get(key).cloned()
    }

    /// Stores `value`, returning the notification other documents must get, or
    /// [`QuotaExceeded`] if the write does not fit.
    ///
    /// `Ok(None)` means the write changed nothing: HTML's `setItem` returns
    /// early when the stored value already equals `value`, and fires no event.
    /// Checked here so no caller has to.
    pub fn set(
        &mut self,
        key: &str,
        value: &str,
        kind: StorageAreaKind,
        url: &str,
        origin: &str,
        author: StorageSubscriber,
    ) -> Result<Option<StorageNotification>, QuotaExceeded> {
        let old = self.map.get(key).cloned();
        if old.as_deref() == Some(value) {
            // "If oldValue is value, then return" — no write, no event.
            return Ok(None);
        }
        let freed = old.as_ref().map_or(0, |old| cost(key, old));
        let added = cost(key, value);
        // `saturating_sub`, so this is arithmetic and not a panic candidate.
        // `bytes` cannot legitimately be below `freed`, but a panic here would
        // poison the mutex of an area a whole browsing context shares, which is
        // a far worse failure than an accounting drift nobody can observe.
        let next = self.bytes.saturating_sub(freed) + added;
        if next > STORAGE_QUOTA_BYTES {
            return Err(QuotaExceeded);
        }
        self.bytes = next;
        self.map.insert(key.to_owned(), value.to_owned());
        self.version += 1;
        if !self.has_other_subscribers(author) {
            return Ok(None);
        }
        Ok(Some(StorageNotification {
            key: Some(key.to_owned()),
            old_value: old,
            new_value: Some(value.to_owned()),
            kind,
            url: url.to_owned(),
            origin: origin.to_owned(),
        }))
    }

    pub fn remove(
        &mut self,
        key: &str,
        kind: StorageAreaKind,
        url: &str,
        origin: &str,
        author: StorageSubscriber,
    ) -> Option<StorageNotification> {
        let old = self.map.remove(key)?;
        self.version += 1;
        self.bytes = self.bytes.saturating_sub(cost(key, &old));
        if !self.has_other_subscribers(author) {
            return None;
        }
        Some(StorageNotification {
            key: Some(key.to_owned()),
            old_value: Some(old),
            new_value: None,
            kind,
            url: url.to_owned(),
            origin: origin.to_owned(),
        })
    }

    /// Empties the area. `None` when it was already empty — HTML fires nothing
    /// for a `clear()` that clears nothing.
    pub fn clear(
        &mut self,
        kind: StorageAreaKind,
        url: &str,
        origin: &str,
        author: StorageSubscriber,
    ) -> Option<StorageNotification> {
        if self.map.is_empty() {
            return None;
        }
        self.map.clear();
        self.bytes = 0;
        self.version += 1;
        if !self.has_other_subscribers(author) {
            return None;
        }
        Some(StorageNotification {
            key: None,
            old_value: None,
            new_value: None,
            kind,
            url: url.to_owned(),
            origin: origin.to_owned(),
        })
    }

    /// Registers a listener for writes made through *other* handles.
    pub fn subscribe(&mut self, subscriber: StorageSubscriber, notify: Notify) {
        self.subscribers.push((subscriber, notify));
    }

    pub fn unsubscribe(&mut self, subscriber: StorageSubscriber) {
        self.subscribers.retain(|(id, _)| *id != subscriber);
    }

    /// Whether anyone other than `author` would be told about a write.
    ///
    /// Checked *before* a notification is built. A notification carries a copy
    /// of the value just written, so minting one for an area nobody else is
    /// watching — a standalone `Page`, the CLI, any single-page browser — would
    /// memcpy every `setItem` payload a second time for nothing.
    #[must_use]
    pub fn has_other_subscribers(&self, author: StorageSubscriber) -> bool {
        self.subscribers.iter().any(|(id, _)| *id != author)
    }

    /// The subscribers to notify about a write made by `author`, paired with
    /// their ids so a dead one can be pruned afterwards.
    ///
    /// Returns them rather than calling them: the caller holds this area's
    /// `Mutex`, and a subscriber that wanted it back would deadlock.
    #[must_use]
    pub fn others(&self, author: StorageSubscriber) -> Vec<(StorageSubscriber, Notify)> {
        self.subscribers
            .iter()
            .filter(|(id, _)| *id != author)
            .map(|(id, notify)| (*id, Arc::clone(notify)))
            .collect()
    }
}

/// Origins whose storage one map keeps before the unreferenced ones are
/// dropped.
///
/// Nothing else bounds a storage map: a crawler across a hundred thousand
/// origins would otherwise retain a [`StorageArea`] — up to
/// [`STORAGE_QUOTA_BYTES`] each — for every one, forever.
pub const MAX_STORAGE_ORIGINS: usize = 256;

/// Drops entries of a storage map that nothing but the map still holds, once it
/// has outgrown [`MAX_STORAGE_ORIGINS`].
///
/// Both maps go through this: the per-page one in [`PrivateStorageAreas`] and
/// the context-wide `localStorage` map an embedder owns. An area whose only
/// remaining owner is the map is unreachable by any document, so dropping it is
/// what makes the bound a bound — sparing the non-empty ones would spare
/// exactly the ones that cost something. The price is that an origin's data
/// need not survive until a page returns to it.
///
/// Refcounts only — no area is locked, so a large map cannot turn this into a
/// lock storm on every cross-origin navigation. The entry a caller has just
/// handed out is safe by construction: it was cloned first, so its count is at
/// least two.
pub fn evict_unreferenced_areas<K>(areas: &mut std::collections::HashMap<K, SharedStorage>)
where
    K: std::cmp::Eq + std::hash::Hash,
{
    if areas.len() <= MAX_STORAGE_ORIGINS {
        return;
    }
    areas.retain(|_, held| Arc::strong_count(held) > 1);
}

/// A ready-made [`HostHooks::storage`] backing for an embedder with exactly one
/// page: every `(kind, origin)` gets its own area, shared with nobody.
///
/// [`HostHooks::storage`] cannot have a trait default — the areas have to live
/// *somewhere*, and a default body returning a fresh one per call would hand
/// out a new empty `localStorage` on every `getItem`. So the state is offered
/// as a field to embed instead, and the impl becomes one delegating line. That
/// keeps the standalone case free without making the trait lie.
///
/// [`HostHooks::storage`]: crate::state::HostHooks::storage
#[derive(Default)]
pub struct PrivateStorageAreas {
    areas: std::cell::RefCell<std::collections::HashMap<(StorageAreaKind, String), SharedStorage>>,
}

impl PrivateStorageAreas {
    /// The area for `(kind, origin)`, created on first use and bounded by
    /// [`evict_unreferenced_areas`].
    #[must_use]
    pub fn area(&self, kind: StorageAreaKind, origin: &str) -> SharedStorage {
        let mut areas = self.areas.borrow_mut();
        let area = Arc::clone(
            areas
                .entry((kind, origin.to_owned()))
                .or_insert_with(StorageArea::shared),
        );
        evict_unreferenced_areas(&mut areas);
        area
    }
}

/// What one `Storage` wrapper is bound to.
///
/// The area sits behind a `RefCell` because the realm outlives a navigation: a
/// script that did `window.ls = localStorage` on one origin still holds this
/// handle after the page moves to another. Re-pointing the handle itself —
/// rather than minting a fresh wrapper and leaving the old one live — is what
/// stops that captured reference writing the *previous* origin's data from the
/// new document, which is precisely the bug moving Web Storage into Rust was
/// meant to close.
pub(crate) struct StorageHandle {
    area: RefCell<SharedStorage>,
    /// The storage key of the area currently bound, stamped onto every
    /// notification this handle produces.
    origin: RefCell<String>,
    /// `(area version, keys)`. The `ownKeys` trap and `key(i)` both want the
    /// key list, and building it is a lock plus an O(n) B-tree walk — so the
    /// documented `for (i = 0; i < length; i++) key(i)` enumeration was O(n²)
    /// walks and one lock per step on an area a whole context contends on.
    /// Cached against the area's mutation counter.
    keys: RefCell<Option<(u64, Rc<Vec<String>>)>>,
    pub(crate) kind: StorageAreaKind,
    /// This document's identity among the area's subscribers, so its own
    /// writes are not delivered back to it.
    pub(crate) subscriber: StorageSubscriber,
}

impl StorageHandle {
    pub(crate) fn new(
        area: SharedStorage,
        origin: String,
        kind: StorageAreaKind,
        subscriber: StorageSubscriber,
    ) -> Self {
        Self {
            area: RefCell::new(area),
            origin: RefCell::new(origin),
            keys: RefCell::new(None),
            kind,
            subscriber,
        }
    }

    /// The storage key of the area currently bound.
    pub(crate) fn origin(&self) -> String {
        self.origin.borrow().clone()
    }

    /// The area this handle currently names.
    pub(crate) fn area(&self) -> SharedStorage {
        Arc::clone(&self.area.borrow())
    }

    /// Re-points this handle at `area` — the document changed origin.
    pub(crate) fn retarget(&self, area: SharedStorage, origin: String) {
        *self.area.borrow_mut() = area;
        *self.origin.borrow_mut() = origin;
        self.keys.borrow_mut().take();
    }

    /// Every key of the bound area, in `Storage.key(i)` order, cached against the
    /// area's mutation counter.
    pub(crate) fn keys(&self) -> Rc<Vec<String>> {
        let area = self.area();
        let area = area.lock().unwrap_or_else(|e| e.into_inner());
        let version = area.version();
        if let Some((cached_at, keys)) = self.keys.borrow().as_ref()
            && *cached_at == version
        {
            return Rc::clone(keys);
        }
        let keys = Rc::new(area.keys());
        *self.keys.borrow_mut() = Some((version, Rc::clone(&keys)));
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://writer.test/page";
    const ORIGIN: &str = "https://writer.test";

    /// An area with one *other* subscriber, plus the id to write as.
    ///
    /// A notification is only built when somebody else is listening, so a bare
    /// `StorageArea` reports `None` for every write — these tests are about
    /// what a notification says, so they need a listener to exist.
    fn watched() -> (StorageArea, StorageSubscriber) {
        let mut area = StorageArea::default();
        area.subscribe(StorageSubscriber::next(), Arc::new(|_| true));
        (area, StorageSubscriber::next())
    }

    fn set(
        area: &mut StorageArea,
        key: &str,
        value: &str,
        author: StorageSubscriber,
    ) -> Result<Option<StorageNotification>, QuotaExceeded> {
        area.set(key, value, StorageAreaKind::Local, URL, ORIGIN, author)
    }

    #[test]
    fn keys_are_enumerated_in_a_stable_order() {
        let (mut area, me) = watched();
        for key in ["zeta", "alpha", "mu"] {
            set(&mut area, key, "v", me).unwrap();
        }
        assert_eq!(area.keys(), vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn rewriting_the_same_value_notifies_nobody() {
        let (mut area, me) = watched();
        assert!(set(&mut area, "k", "v", me).unwrap().is_some());
        assert!(
            set(&mut area, "k", "v", me).unwrap().is_none(),
            "an unchanged value is not a write"
        );
        let changed = set(&mut area, "k", "w", me).unwrap().unwrap();
        assert_eq!(changed.old_value.as_deref(), Some("v"));
        assert_eq!(changed.new_value.as_deref(), Some("w"));
        assert_eq!(changed.url, URL, "the writer's URL, not the receiver's");
    }

    #[test]
    fn a_write_nobody_is_watching_builds_no_notification() {
        // The common case — a standalone `Page`, the CLI, a single-page
        // browser. A notification carries a copy of the value, so minting one
        // here would memcpy every `setItem` payload for nothing.
        let mut area = StorageArea::default();
        let me = StorageSubscriber::next();
        assert!(
            area.set("k", "v", StorageAreaKind::Local, URL, ORIGIN, me)
                .unwrap()
                .is_none()
        );
        assert_eq!(area.get("k").as_deref(), Some("v"), "the write still lands");
        assert!(
            area.remove("k", StorageAreaKind::Local, URL, ORIGIN, me)
                .is_none()
        );
        assert!(area.get("k").is_none(), "the removal still lands");
    }

    #[test]
    fn the_quota_counts_utf16_code_units() {
        let (mut area, me) = watched();
        // One emoji is 2 UTF-16 code units but 4 UTF-8 bytes: charging bytes
        // would give this page half the quota it is promised.
        let emoji = "\u{1F600}".repeat(STORAGE_QUOTA_BYTES / 2);
        assert_eq!(set(&mut area, "k", &emoji, me), Err(QuotaExceeded));
        let fits = "\u{1F600}".repeat(STORAGE_QUOTA_BYTES / 2 - 1);
        assert!(set(&mut area, "k", &fits, me).is_ok());
    }

    #[test]
    fn the_quota_is_enforced_and_refunded() {
        let (mut area, me) = watched();
        let big = "x".repeat(STORAGE_QUOTA_BYTES - 1);
        set(&mut area, "k", &big, me).unwrap();
        assert_eq!(
            set(&mut area, "other", "y", me),
            Err(QuotaExceeded),
            "a write past the quota must be refused"
        );
        // Overwriting refunds the old value's cost rather than double-counting.
        set(&mut area, "k", "small", me).unwrap();
        assert!(set(&mut area, "other", "y", me).is_ok());
        // ... and so does removal: only once *both* keys are gone does the
        // whole quota become available again.
        area.remove("k", StorageAreaKind::Local, URL, ORIGIN, me)
            .unwrap();
        assert_eq!(
            set(&mut area, "k", &big, me),
            Err(QuotaExceeded),
            "`other` still occupies its share"
        );
        area.remove("other", StorageAreaKind::Local, URL, ORIGIN, me)
            .unwrap();
        assert!(set(&mut area, "k", &big, me).is_ok());
    }

    #[test]
    fn clearing_an_empty_area_notifies_nobody() {
        let (mut area, me) = watched();
        assert!(
            area.clear(StorageAreaKind::Local, URL, ORIGIN, me)
                .is_none()
        );
        set(&mut area, "k", "v", me).unwrap();
        assert!(
            area.clear(StorageAreaKind::Local, URL, ORIGIN, me)
                .is_some()
        );
        assert!(area.is_empty());
    }

    #[test]
    fn an_author_never_hears_its_own_write() {
        let mut area = StorageArea::default();
        let (a, b) = (StorageSubscriber::next(), StorageSubscriber::next());
        area.subscribe(a, Arc::new(|_| true));
        area.subscribe(b, Arc::new(|_| true));
        assert!(area.has_other_subscribers(a));
        assert_eq!(area.others(a).len(), 1);
        area.unsubscribe(b);
        assert!(!area.has_other_subscribers(a));
        assert!(area.others(a).is_empty());
    }
}
