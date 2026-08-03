//! `NetService`: the async net ↔ sync page bridge (design doc §7, ADR-0004).
//!
//! A [`NetPool`] owns a multi-thread tokio [`Runtime`] living *off* any page
//! thread's critical path (a page is never itself a runtime worker, so
//! `handle.spawn` and `block_on` are legal from every page thread that shares
//! the pool). Requests are `spawn`ed onto the runtime; their progress flows
//! back to the page as [`NetEvent`]s over a `crossbeam_channel`. The page's
//! event loop unifies "a net event OR a command OR the next timer deadline OR
//! the settle budget" into one blocking wait, so timers and network both
//! progress with no busy-wait.
//!
//! [`NetService::new`] builds a private pool — one page, one runtime, one
//! cache, as before. [`NetService::with_shared`] takes a browser's pool plus a
//! context's cookie jar and cache partition (ADR-0027 D7).
//!
//! The module loader (`js` ES modules) uses [`NetService::fetch_blocking`]:
//! it blocks only the page thread while tokio workers deliver the bytes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use oxidepage_base::RequestId;
use oxidepage_base::id::FIRST_GENERATION;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::Semaphore;

/// Upper bound on simultaneously in-flight spawned fetches, so a page firing a
/// flood of requests cannot exhaust memory/sockets with unbounded concurrency.
const MAX_CONCURRENT_FETCHES: usize = 16;

/// How long a [`NetPool`]'s runtime is given to retire its blocking tasks.
///
/// The SSRF connector resolves DNS on `spawn_blocking`, so a slow resolver
/// would otherwise stall the drop — and with it `Browser::close`.
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

use crate::cache::{CachePartition, HttpCache};
use crate::client::HttpClient;
use crate::cookies::CookieJar;
use crate::error::{NetError, NetResult};
use crate::fetch::{
    FetchEngine, FetchOutcome, NetRequest, RequestDefaults, ResponseType, SharedFetchParts,
};
use crate::policy::ResourcePolicy;
use crate::record::{NetworkEvent, RequestLog};

/// A net→page progress event, tagged with the originating [`RequestId`].
#[derive(Debug)]
pub enum NetEvent {
    /// The response head arrived (after all redirects).
    Headers {
        id: RequestId,
        status: u16,
        status_text: String,
        headers: Vec<(String, String)>,
        final_url: String,
        redirected: bool,
        response_type: ResponseType,
    },
    /// A chunk of the (decompressed) response body.
    Chunk { id: RequestId, data: Bytes },
    /// The response finished successfully.
    Done { id: RequestId },
    /// The request failed.
    Error { id: RequestId, error: NetError },
}

impl NetEvent {
    /// The request this event belongs to.
    #[must_use]
    pub fn request_id(&self) -> RequestId {
        match self {
            NetEvent::Headers { id, .. }
            | NetEvent::Chunk { id, .. }
            | NetEvent::Done { id, .. }
            | NetEvent::Error { id, .. } => *id,
        }
    }
}

/// The net stack a whole browser shares: one tokio runtime, one hyper
/// connection pool, one HTTP cache (design §7, ADR-0027 D7).
///
/// The connection pool is here rather than per context because the SSRF
/// connector is baked into the client at construction ([`HttpClient::new`]),
/// so one pool means one [`ResourcePolicy`] — which is why the policy is a
/// browser-level, not a context-level, decision (ADR-0027 D8).
pub struct NetPool {
    /// `Option` only so [`Drop`] can take it: `Runtime::drop` blocks until
    /// every blocking task retires, and the SSRF connector resolves DNS on
    /// `spawn_blocking`, so a slow resolver would otherwise stall the close.
    runtime: Option<Runtime>,
    client: HttpClient,
    cache: Arc<Mutex<HttpCache>>,
    policy: Arc<ResourcePolicy>,
}

impl NetPool {
    /// A pool for a single page: the private stack `NetService::new` builds.
    ///
    /// Keeps the per-page cache size, because that is what it is — sizing it
    /// like a browser-wide cache would let one page retain sixteen times the
    /// response bodies it used to.
    pub fn new(policy: Arc<ResourcePolicy>) -> NetResult<Arc<Self>> {
        Self::with_options(policy, NetPoolOptions::default())
    }

    /// A pool several pages share. Every page on it is bound to `policy` — a
    /// per-context override is not possible (ADR-0027 D8).
    pub fn with_options(
        policy: Arc<ResourcePolicy>,
        options: NetPoolOptions,
    ) -> NetResult<Arc<Self>> {
        let runtime = build_runtime(options.worker_threads)?;
        let client = HttpClient::new(Arc::clone(&policy))?;
        Ok(Arc::new(Self {
            runtime: Some(runtime),
            client,
            cache: Arc::new(Mutex::new(HttpCache::new(options.cache_entries))),
            policy,
        }))
    }

    /// The runtime handle (for spawns and the module loader's `block_on`).
    #[must_use]
    pub fn handle(&self) -> Handle {
        self.runtime
            .as_ref()
            .expect("runtime taken only in Drop")
            .handle()
            .clone()
    }

    /// The policy every page on this pool is bound to.
    #[must_use]
    pub fn policy(&self) -> Arc<ResourcePolicy> {
        Arc::clone(&self.policy)
    }

    /// The client, its policy, and the cache — the only place these are paired.
    ///
    /// Public because it is the *only* way to obtain a [`SharedFetchParts`]:
    /// the policy field is private precisely so no caller can pair a client
    /// with a policy it was not built for (ADR-0004 D1).
    #[must_use]
    pub fn shared_parts(&self, partition: CachePartition) -> SharedFetchParts {
        SharedFetchParts {
            client: self.client.clone(),
            policy: Arc::clone(&self.policy),
            cache: Arc::clone(&self.cache),
            partition,
        }
    }
}

impl Drop for NetPool {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        // Dropping a tokio `Runtime` blocks, and blocking is forbidden inside
        // an async context — tokio panics rather than deadlocking. A pool
        // normally dies on the thread that built it, but it is an `Arc` shared
        // by every page of a browser, so *which* holder drops it last is not
        // something this type can dictate. Handing the shutdown to a plain
        // thread when we are on a runtime makes the drop safe everywhere,
        // instead of leaving a panic to be discovered by whoever happens to
        // hold the last reference.
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::spawn(move || {
                runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
            });
            return;
        }
        runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
    }
}

/// How a [`NetPool`] is sized.
#[derive(Copy, Clone, Debug)]
pub struct NetPoolOptions {
    /// Tokio worker threads. One pool serves every page of a browser, and TLS
    /// handshakes and gzip/brotli decode are CPU-bound, so a browser wants more
    /// than the two a single page needed.
    pub worker_threads: usize,
    /// Cache entries before oldest-accessed eviction.
    pub cache_entries: usize,
}

impl Default for NetPoolOptions {
    fn default() -> Self {
        Self {
            worker_threads: 2,
            cache_entries: crate::cache::DEFAULT_CAP,
        }
    }
}

fn build_runtime(worker_threads: usize) -> NetResult<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads.max(1))
        .enable_all()
        .build()
        .map_err(|e| {
            NetError::new(
                oxidepage_base::NetErrorKind::Io,
                format!("tokio runtime: {e}"),
            )
        })
}

/// Everything a page-scoped [`NetService`] borrows from its browser.
pub struct SharedNetConfig {
    pub pool: Arc<NetPool>,
    /// The context's cookie jar — shared by every page of one
    /// [`BrowserContext`](../../oxidepage_engine).
    pub cookies: Arc<Mutex<CookieJar>>,
    /// This context's isolation key in the shared HTTP cache.
    pub partition: CachePartition,
}

/// Owns (or borrows) the tokio runtime and owns the fetch engine; the single
/// point the page starts loads and blocks-on synchronous fetches through.
pub struct NetService {
    /// The runtime, connection pool and cache. Private to this service when
    /// built by [`NetService::new`]; shared with sibling pages when built by
    /// [`NetService::with_shared`].
    pool: Arc<NetPool>,
    engine: FetchEngine,
    policy: Arc<ResourcePolicy>,
    cookies: Arc<Mutex<CookieJar>>,
    tx: Sender<NetEvent>,
    next_id: Cell<u32>,
    cancels: RefCell<HashMap<RequestId, Arc<AtomicBool>>>,
    /// Bounds simultaneous in-flight spawned fetches.
    sem: Arc<Semaphore>,
    /// Retained response bodies, for a driver reading one back (ADR-0030).
    log: RefCell<RequestLog>,
    /// The requests announced to the observer that have not yet reported a
    /// terminal outcome — and, once headers arrive, each one's `Content-Type`.
    ///
    /// One map for two jobs on purpose: the content type is remembered so the
    /// body can be classified text-or-bytes when it lands, and *membership* is
    /// what makes a terminal event exactly-once. Every `Finished`/`Failed` goes
    /// through [`NetService::close`], so a request cannot report finished and
    /// then failed (an `xhr.abort()` after the response landed), nor fail twice,
    /// nor — the direction a plain "did headers arrive" test misses — vanish
    /// with no terminal event at all when it is aborted *before* its headers.
    open: RefCell<HashMap<RequestId, String>>,
    /// Told about every request's progress, when an embedder wants to know.
    ///
    /// `Rc`, not `Arc`: the service lives on the page thread and so does the
    /// observer. Nothing here crosses a thread — the driver side receives this
    /// as a `PageEvent`, which the page's own bus already carries.
    observer: RefCell<Option<NetObserver>>,
}

/// A callback told about each request's progress.
pub type NetObserver = Rc<dyn Fn(NetworkEvent)>;

impl NetService {
    /// Builds a service over `policy`, returning it plus the receiver the
    /// page drains net events from.
    pub fn new(policy: ResourcePolicy) -> NetResult<(Self, Receiver<NetEvent>)> {
        Self::new_with_defaults(policy, RequestDefaults::default())
    }

    pub fn new_with_defaults(
        policy: ResourcePolicy,
        request_defaults: RequestDefaults,
    ) -> NetResult<(Self, Receiver<NetEvent>)> {
        let pool = NetPool::new(Arc::new(policy))?;
        let cookies = Arc::new(Mutex::new(CookieJar::new()));
        Ok(Self::with_shared(
            SharedNetConfig {
                pool,
                cookies,
                partition: CachePartition::default(),
            },
            request_defaults,
        ))
    }

    /// Builds a service over a browser-shared [`NetPool`], a context-shared
    /// cookie jar, and that context's cache partition (ADR-0027 D7).
    ///
    /// The policy comes from the pool: it is baked into the shared connection
    /// pool's SSRF connector and cannot be overridden per page.
    #[must_use]
    pub fn with_shared(
        config: SharedNetConfig,
        request_defaults: RequestDefaults,
    ) -> (Self, Receiver<NetEvent>) {
        let SharedNetConfig {
            pool,
            cookies,
            partition,
        } = config;
        let policy = pool.policy();
        // The byte/request budgets this engine mints are its own: they are
        // per-page bounds and must not become browser-wide when the pool and
        // the cache are shared.
        let engine = FetchEngine::with_shared(
            pool.shared_parts(partition),
            Arc::clone(&cookies),
            request_defaults,
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        (
            Self {
                pool,
                engine,
                policy,
                cookies,
                tx,
                next_id: Cell::new(1),
                cancels: RefCell::new(HashMap::new()),
                sem: Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES)),
                log: RefCell::new(RequestLog::default()),
                open: RefCell::new(HashMap::new()),
                observer: RefCell::new(None),
            },
            rx,
        )
    }

    /// The runtime handle (for the module loader's `block_on`).
    #[must_use]
    pub fn handle(&self) -> Handle {
        self.pool.handle()
    }

    /// The page-scoped cookie jar (shared with `document.cookie`).
    #[must_use]
    pub fn cookies(&self) -> Arc<Mutex<CookieJar>> {
        Arc::clone(&self.cookies)
    }

    /// The resource policy.
    #[must_use]
    pub fn policy(&self) -> Arc<ResourcePolicy> {
        Arc::clone(&self.policy)
    }

    fn next_request_id(&self) -> RequestId {
        let n = self.next_id.get();
        self.next_id.set(n.wrapping_add(1).max(1));
        RequestId::from_parts(n, FIRST_GENERATION)
    }

    /// Starts a top-level document navigation, returning its request id.
    pub fn start_document(&self, url: &str) -> RequestId {
        self.spawn_fetch(NetRequest::navigation(url))
    }

    /// Starts a resource/subresource/fetch/XHR load.
    pub fn start_resource(&self, request: NetRequest) -> RequestId {
        self.spawn_fetch(request)
    }

    /// Installs the network observer, replacing any previous one.
    pub fn set_observer(&self, observer: Option<NetObserver>) {
        *self.observer.borrow_mut() = observer;
    }

    fn notify(&self, event: NetworkEvent) {
        // Cloned out of the `RefCell` before the call: an observer that starts
        // another request would otherwise re-enter this borrow.
        let observer = self.observer.borrow().clone();
        if let Some(observer) = observer {
            observer(event);
        }
    }

    /// The retained body of `id`, and whether it is text rather than bytes.
    #[must_use]
    pub fn response_body(&self, id: RequestId) -> Option<(Vec<u8>, bool)> {
        self.log
            .borrow()
            .body(id)
            .map(|(bytes, text)| (bytes.to_vec(), text))
    }

    /// Drops every retained body — a new document's requests replace them.
    pub fn clear_log(&self) {
        self.log.borrow_mut().clear();
    }

    /// Ends `id`'s record, reporting whether it was still open.
    ///
    /// The single gate on every terminal event: a request reports `Finished` or
    /// `Failed` once, or not at all.
    fn close(&self, id: RequestId) -> bool {
        self.open.borrow_mut().remove(&id).is_some()
    }

    /// Announces an outgoing request and starts its record.
    fn note_request(&self, id: RequestId, request: &NetRequest) {
        // Opened here, not on the response: a request aborted before its
        // headers arrive still owes the observer a terminal event, and the
        // driver's in-flight count — hence every `networkidle` wait — never
        // settles without one.
        self.open.borrow_mut().insert(id, String::new());
        self.notify(NetworkEvent::Requested {
            id,
            url: request.url.clone(),
            method: request.method.clone(),
            headers: request.headers.clone(),
            timestamp: epoch_ms(),
        });
    }

    /// Records and announces a completed synchronous fetch.
    ///
    /// The blocking path produces **no** `NetEvent`, so without this the main
    /// document — and every ES module and blocking `@import` — would be
    /// invisible to a driver and unavailable to `getResponseBody`. A hook on
    /// the async path alone misses exactly the request a driver cares most
    /// about.
    fn note_blocking_outcome(&self, id: RequestId, outcome: &NetResult<FetchOutcome>) {
        if !self.close(id) {
            return;
        }
        match outcome {
            Ok(FetchOutcome { head, body }) => {
                let content_type = header_value(&head.headers, "content-type");
                self.log
                    .borrow_mut()
                    .retain(id, body.clone(), &content_type);
                self.notify(NetworkEvent::Responded {
                    id,
                    status: head.status,
                    status_text: head.status_text.clone(),
                    headers: head.headers.clone(),
                    final_url: head.final_url.clone(),
                    mime_type: mime_of(&content_type),
                    timestamp: epoch_ms(),
                });
                self.notify(NetworkEvent::Finished {
                    id,
                    encoded_len: body.len() as u64,
                    timestamp: epoch_ms(),
                });
            }
            Err(error) => self.notify(NetworkEvent::Failed {
                id,
                error: error.to_string(),
                timestamp: epoch_ms(),
            }),
        }
    }

    /// Folds one asynchronous [`NetEvent`] into the log.
    ///
    /// Called by the page at the top of its `dispatch_net_event`, which is the
    /// one place every async response passes through. Doing it inside
    /// `spawn_fetch` is not possible: that runs on a tokio worker, and the log
    /// and observer belong to the page thread.
    pub fn note_event(&self, event: &NetEvent) {
        match event {
            NetEvent::Headers {
                id,
                status,
                status_text,
                headers,
                final_url,
                ..
            } => {
                let content_type = header_value(headers, "content-type");
                {
                    // Only if still open: a response that lands after the page
                    // aborted the request has already reported its outcome.
                    let mut open = self.open.borrow_mut();
                    let Some(slot) = open.get_mut(id) else {
                        return;
                    };
                    slot.clone_from(&content_type);
                }
                self.notify(NetworkEvent::Responded {
                    id: *id,
                    status: *status,
                    status_text: status_text.clone(),
                    headers: headers.clone(),
                    final_url: final_url.clone(),
                    mime_type: mime_of(&content_type),
                    timestamp: epoch_ms(),
                });
            }
            NetEvent::Chunk { id, data } => {
                // Taken, not read: `Done` uses the entry's absence to tell a
                // body that already reported `Finished` from one that never
                // produced a chunk at all.
                let content_type = self.open.borrow_mut().remove(id);
                let Some(content_type) = content_type else {
                    return;
                };
                // Exactly one chunk carries the whole body today, so this is a
                // retain rather than an append. If chunking ever becomes real,
                // this is the line that has to grow a buffer.
                self.log
                    .borrow_mut()
                    .retain(*id, data.clone(), &content_type);
                self.notify(NetworkEvent::Finished {
                    id: *id,
                    encoded_len: data.len() as u64,
                    timestamp: epoch_ms(),
                });
            }
            NetEvent::Done { id } => {
                // A response with an **empty body** produces no `Chunk` at all
                // (`spawn_fetch` skips it), so `Done` is the only place a 204, a
                // HEAD or a bodyless 404 can be closed out. Without this the
                // driver counts the request in flight forever and every
                // `networkidle` wait hangs.
                if self.close(*id) {
                    self.notify(NetworkEvent::Finished {
                        id: *id,
                        encoded_len: 0,
                        timestamp: epoch_ms(),
                    });
                }
            }
            NetEvent::Error { id, error } => {
                if self.close(*id) {
                    self.notify(NetworkEvent::Failed {
                        id: *id,
                        error: error.to_string(),
                        timestamp: epoch_ms(),
                    });
                }
            }
        }
    }

    fn spawn_fetch(&self, request: NetRequest) -> RequestId {
        let id = self.next_request_id();
        self.note_request(id, &request);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.borrow_mut().insert(id, Arc::clone(&cancel));
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        let sem = Arc::clone(&self.sem);
        self.pool.handle().spawn(async move {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            // Cap concurrency: block here until a permit is free. The permit is
            // held for the whole fetch and released on drop.
            let Ok(_permit) = sem.acquire_owned().await else {
                return; // semaphore closed → service shutting down
            };
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            match engine.fetch(request).await {
                Ok(out) if !cancel.load(Ordering::Relaxed) => {
                    let FetchOutcome { head, body } = out;
                    let _ = tx.send(NetEvent::Headers {
                        id,
                        status: head.status,
                        status_text: head.status_text,
                        headers: head.headers,
                        final_url: head.final_url,
                        redirected: head.redirected,
                        response_type: head.response_type,
                    });
                    if !body.is_empty() && !cancel.load(Ordering::Relaxed) {
                        let _ = tx.send(NetEvent::Chunk { id, data: body });
                    }
                    let _ = tx.send(NetEvent::Done { id });
                }
                Ok(_) => {} // cancelled after completion; drop silently
                Err(error) if !cancel.load(Ordering::Relaxed) => {
                    let _ = tx.send(NetEvent::Error { id, error });
                }
                Err(_) => {}
            }
        });
        id
    }

    /// Cancels an in-flight request (best-effort; a completed-but-undelivered
    /// response is dropped).
    pub fn abort(&self, id: RequestId) {
        if let Some(flag) = self.cancels.borrow_mut().remove(&id) {
            flag.store(true, Ordering::Relaxed);
        }
        // The cancelled task returns silently, so this is the only place the
        // `Requested` event already sent to an observer can be closed out.
        // `reset_document_state` aborts every pending script, sheet, image and
        // font on *every* navigation, so without this each `goto` permanently
        // leaks the previous document's in-flight count into the driver's
        // bookkeeping — and a `networkidle` wait never resolves.
        //
        // Only if still open, though: `abort` is reachable from `xhr.abort()`
        // and `AbortController` *after* the response has landed, and a
        // `loadingFailed` following a `loadingFinished` for a request that
        // succeeded is worse than no event at all.
        if self.close(id) {
            self.notify(NetworkEvent::Failed {
                id,
                error: String::from("net::ERR_ABORTED"),
                timestamp: epoch_ms(),
            });
        }
    }

    /// Forgets an in-flight request's cancel flag once the page has fully
    /// consumed it.
    pub fn finish(&self, id: RequestId) {
        self.cancels.borrow_mut().remove(&id);
    }

    /// Runs a fetch to completion synchronously, blocking the page thread
    /// (used by the synchronous ES module loader). Tokio workers deliver the
    /// bytes; only the page thread parks.
    pub fn fetch_blocking(&self, request: NetRequest) -> NetResult<FetchOutcome> {
        let id = self.next_request_id();
        self.note_request(id, &request);
        let outcome = self.pool.handle().block_on(self.engine.fetch(request));
        self.note_blocking_outcome(id, &outcome);
        outcome
    }

    /// Like [`NetService::fetch_blocking`], reporting the id it recorded under
    /// so a caller can read the body back.
    pub fn fetch_blocking_tracked(
        &self,
        request: NetRequest,
    ) -> (RequestId, NetResult<FetchOutcome>) {
        let id = self.next_request_id();
        self.note_request(id, &request);
        let outcome = self.pool.handle().block_on(self.engine.fetch(request));
        self.note_blocking_outcome(id, &outcome);
        (id, outcome)
    }
}

/// Unix-epoch milliseconds, the timestamp every [`NetworkEvent`] carries.
///
/// Wall clock rather than monotonic because a driver correlates these with the
/// page's own `Date.now()`-based records — console lines, navigation
/// milestones — which are all epoch-based.
fn epoch_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64() * 1000.0)
}

/// A header value by case-insensitive name.
fn header_value(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

/// The MIME type of a `Content-Type`, without parameters.
fn mime_of(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}
