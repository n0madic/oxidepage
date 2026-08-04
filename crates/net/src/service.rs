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
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

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
    FetchEngine, FetchOutcome, NetRequest, RequestDefaults, ResourceType, ResponseHead,
    ResponseType, SharedFetchParts, basic_auth_header, parse_auth_challenge,
};
use crate::intercept::{
    AuthChallenge, AuthResponse, AuthSource, DEFAULT_INTERCEPT_TIMEOUT, FulfilledResponse,
    InterceptCommand, InterceptControl, RequestOverrides,
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
    /// **Internal.** A spawned fetch met an auth challenge the driver said it
    /// would answer (ADR-0032 D8). Carries the response *unstated* so
    /// `Default`/`CancelAuth` can deliver it unchanged, and the request so
    /// `ProvideCredentials` can re-issue it under the same id.
    ///
    /// [`NetService::note_event`] consumes this and answers `false`; it never
    /// reaches a consumer, which is why no `dispatch_net_event` arm handles it.
    AuthRequired {
        id: RequestId,
        challenge: Box<AuthChallenge>,
        outcome: Box<FetchOutcome>,
        request: Box<NetRequest>,
    },
}

impl NetEvent {
    /// The request this event belongs to.
    #[must_use]
    pub fn request_id(&self) -> RequestId {
        match self {
            NetEvent::Headers { id, .. }
            | NetEvent::Chunk { id, .. }
            | NetEvent::Done { id, .. }
            | NetEvent::Error { id, .. }
            | NetEvent::AuthRequired { id, .. } => *id,
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
    open: RefCell<HashMap<RequestId, OpenRequest>>,
    /// Told about every request's progress, when an embedder wants to know.
    ///
    /// `Rc`, not `Arc`: the service lives on the page thread and so does the
    /// observer. Nothing here crosses a thread — the driver side receives this
    /// as a `PageEvent`, which the page's own bus already carries.
    observer: RefCell<Option<NetObserver>>,
    /// The interception state a driver shares with this page (ADR-0032 D2).
    ///
    /// The service holding it is what keeps [`NetService::decisions`] from ever
    /// disconnecting: a disconnected `Receiver` is permanently *ready* in the
    /// event loop's `Select`, so a channel whose only sender lived on the
    /// driver side would turn the one park into a pegged core the moment the
    /// driver went away.
    intercept: InterceptControl,
    decisions: Receiver<InterceptCommand>,
    /// Asynchronously paused requests, parked here rather than spawned.
    ///
    /// Parked **before** `pool.handle().spawn`: the concurrency permit is taken
    /// inside the spawned task, so sixteen requests paused after the spawn
    /// would starve every unpaused request on the page (ADR-0032 D1).
    paused: RefCell<HashMap<RequestId, Pause>>,
    /// Decisions for *other* requests that arrived while the page thread was
    /// parked inside a blocking fetch. Applied at the top of the event loop,
    /// because applying one there would spawn or synthesize under the `dom`
    /// and `style` borrows the blocking caller holds (ADR-0032 D3).
    deferred: RefCell<VecDeque<InterceptCommand>>,
    /// Credential attempts already spent per request, carried across the
    /// re-issue so a re-challenge cannot loop (see [`MAX_AUTH_ATTEMPTS`]).
    ///
    /// Separate from [`NetService::paused`] because the count has to survive
    /// the window where the request is *not* parked — it is in flight on a
    /// tokio worker, having been resumed by `continueWithAuth`.
    retry_auth: RefCell<HashMap<RequestId, u32>>,
    /// Redirect hops a *fulfilled* response has produced, per request.
    ///
    /// The engine follows real redirects itself and caps them; a driver
    /// stubbing `302 Location:` is outside that loop, so a pair of stubs that
    /// point at each other would recurse without one.
    fulfilled_hops: RefCell<HashMap<RequestId, u32>>,
}

/// A request held at the pause point.
struct Pause {
    kind: PauseKind,
    /// When this pause gives up and proceeds unmodified.
    ///
    /// The asynchronous half needs this stored, because — unlike the blocking
    /// half, which parks on `recv_deadline` — nothing here is waiting on a
    /// clock. Without it a driver that goes quiet while holding the socket open
    /// leaves the request with **no terminal event at all**: `in_flight` never
    /// returns to zero, `is_idle` is false forever, and every later `settle`
    /// burns its whole budget (ADR-0032 D7).
    deadline: Instant,
}

enum PauseKind {
    /// Held before it was sent.
    Request(Box<NetRequest>),
    /// Held *after* a 401/407 the driver said it would answer. The response is
    /// stashed so `Default`/`CancelAuth` deliver it unchanged (ADR-0032 D8).
    Auth {
        request: Box<NetRequest>,
        outcome: Box<FetchOutcome>,
        /// Which side asked, so the credentials go in `Authorization` for a 401
        /// and `Proxy-Authorization` for a 407.
        source: AuthSource,
        /// Credentials already tried. A driver that answers
        /// `ProvideCredentials` to a server that keeps refusing them would
        /// otherwise loop forever — re-issue, 401, re-pause, re-issue — with no
        /// terminal event and a request per round trip.
        attempts: u32,
    },
}

/// How many times one request may be re-issued with credentials.
///
/// One retry is what a browser does and what the blocking half already did; the
/// async half needs the count stored because its retry goes back through the
/// same pause map rather than through a straight-line function.
const MAX_AUTH_ATTEMPTS: u32 = 1;

/// A callback told about each request's progress.
pub type NetObserver = Rc<dyn Fn(NetworkEvent)>;

/// What [`NetService::open`] remembers about a request still in flight.
#[derive(Clone, Debug, Default)]
struct OpenRequest {
    /// The response's `Content-Type`, once headers have arrived — so the body
    /// can be classified text-or-bytes when it lands.
    content_type: String,
    /// What the request was for. Carried here so a `Responded`/`Failed` can
    /// repeat it without the observer having to remember the `Requested`.
    resource_type: ResourceType,
}

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
        let (intercept, decisions) = InterceptControl::new();
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
                intercept,
                decisions,
                paused: RefCell::new(HashMap::new()),
                deferred: RefCell::new(VecDeque::new()),
                retry_auth: RefCell::new(HashMap::new()),
                fulfilled_hops: RefCell::new(HashMap::new()),
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
    fn close(&self, id: RequestId) -> Option<OpenRequest> {
        self.open.borrow_mut().remove(&id)
    }

    /// Announces an outgoing request and starts its record.
    fn note_request(&self, id: RequestId, request: &NetRequest) {
        // Opened here, not on the response: a request aborted before its
        // headers arrive still owes the observer a terminal event, and the
        // driver's in-flight count — hence every `networkidle` wait — never
        // settles without one.
        self.open.borrow_mut().insert(
            id,
            OpenRequest {
                content_type: String::new(),
                resource_type: request.resource_type,
            },
        );
        self.notify(NetworkEvent::Requested {
            id,
            url: request.url.clone(),
            method: request.method.clone(),
            headers: request.headers.clone(),
            resource_type: request.resource_type,
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
        let Some(open) = self.close(id) else {
            return;
        };
        let resource_type = open.resource_type;
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
                    resource_type,
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
                resource_type,
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
    ///
    /// Reports `false` when the service consumed the event itself, which the
    /// caller must take as "do not route this any further". Only the internal
    /// [`NetEvent::AuthRequired`] answers that way.
    pub fn note_event(&self, event: &NetEvent) -> bool {
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
                let resource_type = {
                    // Only if still open: a response that lands after the page
                    // aborted the request has already reported its outcome.
                    let mut open = self.open.borrow_mut();
                    let Some(slot) = open.get_mut(id) else {
                        return true;
                    };
                    slot.content_type.clone_from(&content_type);
                    slot.resource_type
                };
                self.notify(NetworkEvent::Responded {
                    id: *id,
                    status: *status,
                    status_text: status_text.clone(),
                    headers: headers.clone(),
                    final_url: final_url.clone(),
                    mime_type: mime_of(&content_type),
                    resource_type,
                    timestamp: epoch_ms(),
                });
            }
            NetEvent::Chunk { id, data } => {
                // Taken, not read: `Done` uses the entry's absence to tell a
                // body that already reported `Finished` from one that never
                // produced a chunk at all.
                let open = self.open.borrow_mut().remove(id);
                let Some(OpenRequest { content_type, .. }) = open else {
                    return true;
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
                if self.close(*id).is_some() {
                    self.notify(NetworkEvent::Finished {
                        id: *id,
                        encoded_len: 0,
                        timestamp: epoch_ms(),
                    });
                }
            }
            NetEvent::Error { id, error } => {
                if let Some(open) = self.close(*id) {
                    self.notify(NetworkEvent::Failed {
                        id: *id,
                        error: error.to_string(),
                        resource_type: open.resource_type,
                        timestamp: epoch_ms(),
                    });
                }
            }
            // Not a milestone but a *second pause* under the same id (ADR-0032
            // D8), reusing the same map, the same release paths and the same
            // timeout as the first one. Nothing downstream ever sees it.
            NetEvent::AuthRequired { .. } => return false,
        }
        true
    }

    /// Parks a request that met an auth challenge, taking the internal event
    /// apart. Separate from [`NetService::note_event`] only because the event
    /// is borrowed there and this needs to own the pieces.
    pub fn begin_auth_pause(&self, event: NetEvent) {
        let NetEvent::AuthRequired {
            id,
            challenge,
            outcome,
            request,
        } = event
        else {
            return;
        };
        // Aborted while the fetch was in flight: nothing is owed, and re-parking
        // would strand the request until the timeout.
        if self.open.borrow().get(&id).is_none() {
            return;
        }
        let attempts = self.retry_auth.borrow().get(&id).copied().unwrap_or(0);
        // The challenge's *source* is kept, not just announced: a 407 wants
        // `Proxy-Authorization` and a 401 wants `Authorization`, and answering
        // a proxy into the wrong header is refused by every proxy there is —
        // which is the whole reason `AuthSource` has two variants.
        let source = challenge.source;
        self.announce_auth(id, &request, &challenge);
        self.park(
            id,
            PauseKind::Auth {
                request,
                outcome,
                source,
                attempts,
            },
        );
    }

    fn spawn_fetch(&self, request: NetRequest) -> RequestId {
        let id = self.next_request_id();
        self.note_request(id, &request);
        // The pause point sits **above** the offline gate, as Chrome's `Fetch`
        // domain sits above the network stack: `setOfflineMode(true)` +
        // `setRequestInterception(true)` + `request.respond()` is the standard
        // way to test an offline page, and failing before the announcement
        // meant the driver never saw the request it meant to stub. The gate is
        // applied where the request would actually reach the network — in
        // `spawn_fetch_with`, which is also where a *continued* one arrives.
        if self.intercepts(&request) {
            self.announce_pause(id, &request);
            self.park(id, PauseKind::Request(Box::new(request)));
            return id;
        }
        self.spawn_fetch_with(id, request);
        id
    }

    /// Parks a request at the pause point with its give-up deadline.
    fn park(&self, id: RequestId, kind: PauseKind) {
        self.paused.borrow_mut().insert(
            id,
            Pause {
                kind,
                deadline: Instant::now() + DEFAULT_INTERCEPT_TIMEOUT,
            },
        );
    }

    /// Sends a request that already has an id and an announced record.
    ///
    /// Split out of [`NetService::spawn_fetch`] so a *resumed* request does not
    /// re-`note_request` and hand the observer a second `requestWillBeSent` for
    /// one request — which a driver reads as two, and never balances.
    /// Delivers a `fulfillRequest`, **following it** when it is a redirect.
    ///
    /// A driver stubbing `302` + `Location` is the ordinary way to test one, and
    /// the fulfilled outcome used to be handed straight to the consumer with
    /// `redirected: false` — a bodyless 302, which for a document is a blank
    /// page with no error anywhere. The engine's own redirect loop is inside
    /// `FetchEngine::fetch` and never sees a fulfilled response, so the follow
    /// happens here.
    fn deliver_fulfilled(
        &self,
        id: RequestId,
        mut request: NetRequest,
        response: FulfilledResponse,
    ) {
        if let Some(target) = fulfilled_redirect_target(&request.url, &response) {
            let hops = self.fulfilled_hops.borrow().get(&id).copied().unwrap_or(0);
            if hops < MAX_FULFILLED_REDIRECTS {
                self.fulfilled_hops.borrow_mut().insert(id, hops + 1);
                request.url = target;
                // A stubbed redirect changes the method exactly as a real one
                // does, or a stubbed `303` would re-POST to the target.
                if matches!(response.status, 301..=303) && request.method != "HEAD" {
                    request.method = String::from("GET");
                    request.body = None;
                }
                self.spawn_fetch_with(id, request);
                return;
            }
            let _ = self.tx.send(NetEvent::Error {
                id,
                error: NetError::new(
                    oxidepage_base::NetErrorKind::Protocol,
                    format!("exceeded {MAX_FULFILLED_REDIRECTS} fulfilled redirects"),
                ),
            });
            return;
        }
        emit_outcome(&self.tx, id, fulfilled_outcome(&request.url, response));
    }

    fn spawn_fetch_with(&self, id: RequestId, request: NetRequest) {
        // The single point at which a request leaves for the network — reached
        // both by one that was never intercepted and by one a driver continued
        // — so it is the one place the offline gate belongs.
        if self.is_offline_for(&request) {
            // Reported asynchronously, like any other failure: the caller has
            // already stored its `pending_*` state under this id and expects
            // exactly one terminal `NetEvent` for it.
            let _ = self.tx.send(NetEvent::Error {
                id,
                error: offline_error(),
            });
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.borrow_mut().insert(id, Arc::clone(&cancel));
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        let sem = Arc::clone(&self.sem);
        let latency = self.intercept.config().latency;
        // Auth is answered only for requests the driver actually asked to
        // intercept — the same predicate the blocking half applies. Gating on
        // `handle_auth` alone would raise `Fetch.authRequired` for requests
        // outside the driver's patterns, and a driver that answers only its own
        // would leave each of those paused until the timeout.
        let handle_auth = self.intercepts(&request) && self.intercept.config().handle_auth;
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
            // Emulated latency sleeps *outside* `FetchEngine::fetch`, which
            // wraps the whole request in `timeout(request_timeout)` — charging
            // the delay to the request budget would make a 5 s latency turn
            // every request into a timeout (ADR-0032 D9).
            if !latency.is_zero() {
                tokio::time::sleep(latency).await;
            }
            // Kept only when it may be needed again: `continueWithAuth`
            // re-issues the same request under the same id, and
            // `FetchEngine::fetch` consumes it. The clone is why the predicate
            // above is narrow — it duplicates the body, and a large POST is
            // exactly the request a driver is least likely to be intercepting.
            let retry = handle_auth.then(|| request.clone());
            match engine.fetch(request).await {
                Ok(out) if !cancel.load(Ordering::Relaxed) => {
                    if let Some(mut retry) = retry
                        && let Some(challenge) = parse_auth_challenge(
                            &out.head.headers,
                            out.head.status,
                            &out.head.final_url,
                        )
                    {
                        // The snapshot was taken before the redirect loop ran.
                        retarget_to_challenger(&mut retry, &out.head.final_url);
                        let _ = tx.send(NetEvent::AuthRequired {
                            id,
                            challenge: Box::new(challenge),
                            outcome: Box::new(out),
                            request: Box::new(retry),
                        });
                        return;
                    }
                    emit_outcome(&tx, id, out);
                }
                Ok(_) => {} // cancelled after completion; drop silently
                Err(error) if !cancel.load(Ordering::Relaxed) => {
                    let _ = tx.send(NetEvent::Error { id, error });
                }
                Err(_) => {}
            }
        });
    }

    // === the pause point (ADR-0032 D1–D3) ===

    /// The driver's handle on this page's interception.
    #[must_use]
    pub fn intercept(&self) -> InterceptControl {
        self.intercept.clone()
    }

    /// A clone of the decision receiver, for the event loop's `Select`.
    ///
    /// Cloning a `crossbeam` receiver *splits* the stream rather than
    /// duplicating it, which is exactly right: every clone feeds the same one
    /// consumer path — [`NetService::apply_decision`].
    #[must_use]
    pub fn decisions(&self) -> Receiver<InterceptCommand> {
        self.decisions.clone()
    }

    /// Whether emulated offline mode fails this request.
    ///
    /// **`http`/`https` only**, exactly like the pause gate below and for the
    /// same reason: `fetch_inner` answers `file://` and `data:` above the scheme
    /// gate (ADR-0029), and neither involves a network. Chrome resolves a
    /// `data:` URL with the network disabled — it is bytes already in hand —
    /// so failing one here would break every inline script, module, stylesheet
    /// and `fetch` under `emulateNetworkConditions { offline: true }`, which is
    /// precisely the emulation a driver reaches for to test an *offline* page.
    fn is_offline_for(&self, request: &NetRequest) -> bool {
        is_http_scheme(&request.url) && self.intercept.config().offline
    }

    /// Whether this request pauses.
    ///
    /// **`http`/`https` only.** `fetch_inner` answers `file://` and `data:`
    /// above the scheme gate (ADR-0029), and a driver stores no request record
    /// for a `data:` URL — so a pause announced for one is never continued, and
    /// every inline image, font and module would hang until the timeout.
    fn intercepts(&self, request: &NetRequest) -> bool {
        if !is_http_scheme(&request.url) {
            return false;
        }
        self.intercept
            .config()
            .matches(&request.url, request.resource_type)
    }

    /// Marks `id` paused and tells the observer, in that order.
    fn announce_pause(&self, id: RequestId, request: &NetRequest) {
        self.intercept.config().paused.insert(id);
        // Announced with the borrow released: an observer that resolves the
        // pause synchronously (a test, an in-process driver) re-enters here.
        self.notify(NetworkEvent::Paused {
            id,
            url: request.url.clone(),
            method: request.method.clone(),
            headers: request.headers.clone(),
            resource_type: request.resource_type,
            timestamp: epoch_ms(),
        });
    }

    fn announce_auth(&self, id: RequestId, request: &NetRequest, challenge: &AuthChallenge) {
        self.intercept.config().paused.insert(id);
        self.notify(NetworkEvent::AuthRequired {
            id,
            url: request.url.clone(),
            challenge: challenge.clone(),
            resource_type: request.resource_type,
            timestamp: epoch_ms(),
        });
    }

    /// Applies every decision that has arrived, deferred ones first.
    ///
    /// Called at the top of the event loop and after each `Select` wake-up.
    /// Reports whether anything ran, so the loop counts it as progress.
    pub fn drain_decisions(&self) -> bool {
        let mut ran = false;
        loop {
            let deferred = self.deferred.borrow_mut().pop_front();
            let Some(command) = deferred else {
                break;
            };
            self.apply_decision(command);
            ran = true;
        }
        while let Ok(command) = self.decisions.try_recv() {
            self.apply_decision(command);
            ran = true;
        }
        ran | self.release_expired_pauses()
    }

    /// Releases every pause whose deadline has passed (ADR-0032 D7).
    ///
    /// The asynchronous half's backstop. The blocking half gets this from
    /// `recv_deadline`; here nothing is waiting on a clock, so the sweep has to
    /// be explicit — and without it a driver that goes quiet while holding the
    /// socket open leaves a request with no terminal event at all, which is the
    /// exact failure ADR-0030 D5 records as having twice cost a driver.
    ///
    /// Swept rather than timer-driven because the event loop already runs this
    /// on every pass, and a paused request is not something to wake a parked
    /// page for: the page has other reasons to be idle, and the release only
    /// has to be eventual.
    fn release_expired_pauses(&self) -> bool {
        let now = Instant::now();
        // Collected before resolving: `apply_decision` takes the same borrow.
        let expired: Vec<RequestId> = self
            .paused
            .borrow()
            .iter()
            .filter(|(_, pause)| pause.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        if expired.is_empty() {
            return false;
        }
        for id in expired {
            // `Continue` unmodified, the same answer an explicit release gives:
            // a driver that merely stalled must not break the page.
            self.apply_decision(InterceptCommand::release(id));
        }
        true
    }

    /// The soonest a paused request needs attention, for the event loop's park
    /// deadline. `None` when nothing is paused.
    #[must_use]
    pub fn next_pause_deadline(&self) -> Option<Instant> {
        self.paused
            .borrow()
            .values()
            .map(|pause| pause.deadline)
            .min()
    }

    /// Brings every paused request's deadline forward to now.
    ///
    /// For tests only: the release *policy* is worth pinning, the 20-second
    /// constant is not, and a test that actually waited it out would be the
    /// slowest in the tree by two orders of magnitude.
    #[doc(hidden)]
    pub fn expire_pauses_for_test(&self) {
        let now = Instant::now();
        for pause in self.paused.borrow_mut().values_mut() {
            pause.deadline = now;
        }
    }

    /// Resolves one asynchronously paused request.
    ///
    /// A decision naming an id that is not parked is **dropped**, not an error:
    /// the request may have been aborted by a navigation, or resolved already.
    /// The protocol side answers `Invalid InterceptionId` before ever sending
    /// the second one (ADR-0032 D2), so reaching here means the race was lost
    /// to the page, not to another command.
    pub fn apply_decision(&self, command: InterceptCommand) {
        let id = command.request_id();
        let parked = self.paused.borrow_mut().remove(&id);
        let Some(parked) = parked else {
            return;
        };
        self.intercept.config().paused.remove(&id);
        // `wire`, not `new`: the driver handed us `errorReason`, the protocol
        // side turned it into Chrome's exact `net::ERR_…` name, and the driver
        // reads that same name back off `loadingFailed.errorText`. A category
        // prefix would break the round-trip.
        let fail = |error: String| {
            let _ = self.tx.send(NetEvent::Error {
                id,
                error: NetError::wire(oxidepage_base::NetErrorKind::Blocked, error),
            });
        };
        match (parked.kind, command) {
            (PauseKind::Request(mut request), InterceptCommand::Continue { overrides, .. }) => {
                apply_overrides(&mut request, &overrides);
                self.spawn_fetch_with(id, *request);
            }
            (PauseKind::Request(request), InterceptCommand::Fulfill { response, .. }) => {
                self.deliver_fulfilled(id, *request, *response);
            }
            (PauseKind::Request(_), InterceptCommand::Fail { error, .. }) => fail(error),
            // An auth answer for a request that is not at an auth pause: the
            // request has not been sent, so the only sane reading is "go".
            (PauseKind::Request(request), InterceptCommand::Auth { .. }) => {
                self.spawn_fetch_with(id, *request);
            }
            (
                PauseKind::Auth {
                    mut request,
                    outcome,
                    source,
                    attempts,
                },
                InterceptCommand::Auth {
                    response: AuthResponse::Provide { username, password },
                    ..
                },
            ) => {
                // A server that refuses the credentials re-challenges, and a
                // driver that answers unconditionally would loop forever — one
                // request per round trip and no terminal event ever. Past the
                // cap the stashed 401/407 goes through to the page instead,
                // which is what a browser shows when a user gives up.
                if attempts >= MAX_AUTH_ATTEMPTS {
                    emit_outcome(&self.tx, id, *outcome);
                    return;
                }
                apply_basic_auth(&mut request, source, &username, &password);
                self.retry_auth.borrow_mut().insert(id, attempts + 1);
                self.spawn_fetch_with(id, *request);
            }
            (PauseKind::Auth { .. }, InterceptCommand::Fail { error, .. }) => fail(error),
            (PauseKind::Auth { request, .. }, InterceptCommand::Fulfill { response, .. }) => {
                self.deliver_fulfilled(id, *request, *response);
            }
            // `Default`, `CancelAuth`, or a bare continue: the 401/407 the
            // driver never saw goes through to the page unchanged.
            (PauseKind::Auth { outcome, .. }, _) => emit_outcome(&self.tx, id, *outcome),
        }
    }

    /// Parks the page thread until `id`'s decision arrives, or the timeout.
    ///
    /// **Services nothing else** — not `net_rx`, not `wake_rx`, not control
    /// jobs. `dispatch_net_event` enters JS, and two of `fetch_blocking`'s
    /// callers park while holding live borrows: `@import` resolution (inside
    /// stylo, with `dom` and `style` borrowed) and the ES module loader (inside
    /// QuickJS). Running script here is a deterministic `BorrowMutError`, not a
    /// race. This is what `run_dialog` already does (ADR-0032 D3).
    ///
    /// `recv_deadline`, not `recv_timeout` in a loop: a decision for another id
    /// would otherwise restart the clock and extend the park without bound.
    /// `deadline` is shared by every pause of one blocking operation — see
    /// [`NetService::run_blocking`].
    fn await_decision(&self, id: RequestId, deadline: Instant) -> Option<InterceptCommand> {
        if let Some(command) = self.take_deferred(id) {
            return Some(command);
        }
        loop {
            match self.decisions.recv_deadline(deadline) {
                Ok(command) if command.request_id() == id => {
                    self.intercept.config().paused.remove(&id);
                    return Some(command);
                }
                Ok(command) => self.deferred.borrow_mut().push_back(command),
                // Timed out, or the channel died. Either way the request
                // proceeds unmodified, which is the release answer (D7).
                Err(_) => {
                    self.intercept.config().paused.remove(&id);
                    return None;
                }
            }
        }
    }

    /// Takes `id`'s decision out of the deferred queue, if one is waiting.
    fn take_deferred(&self, id: RequestId) -> Option<InterceptCommand> {
        let mut deferred = self.deferred.borrow_mut();
        let at = deferred
            .iter()
            .position(|command| command.request_id() == id)?;
        let command = deferred.remove(at);
        drop(deferred);
        if command.is_some() {
            self.intercept.config().paused.remove(&id);
        }
        command
    }

    /// Cancels an in-flight request (best-effort; a completed-but-undelivered
    /// response is dropped).
    pub fn abort(&self, id: RequestId) {
        // A parked request has no task to cancel, so drop it here — and drop it
        // from the shared set too. `reset_document_state` aborts every pending
        // script, sheet, image and font on *every* navigation, so leaving it
        // would leak one `NetRequest` per navigation, and a late
        // `continueRequest` would resurrect a dead document's request into the
        // live one.
        self.paused.borrow_mut().remove(&id);
        self.intercept.config().paused.remove(&id);
        self.retry_auth.borrow_mut().remove(&id);
        self.deferred
            .borrow_mut()
            .retain(|command| command.request_id() != id);
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
        if let Some(open) = self.close(id) {
            self.notify(NetworkEvent::Failed {
                id,
                error: String::from("net::ERR_ABORTED"),
                resource_type: open.resource_type,
                timestamp: epoch_ms(),
            });
        }
    }

    /// Forgets an in-flight request's cancel flag once the page has fully
    /// consumed it.
    pub fn finish(&self, id: RequestId) {
        self.cancels.borrow_mut().remove(&id);
        self.retry_auth.borrow_mut().remove(&id);
        self.fulfilled_hops.borrow_mut().remove(&id);
    }

    /// Runs a fetch to completion synchronously, blocking the page thread
    /// (used by the synchronous ES module loader). Tokio workers deliver the
    /// bytes; only the page thread parks.
    pub fn fetch_blocking(&self, request: NetRequest) -> NetResult<FetchOutcome> {
        self.fetch_blocking_tracked(request).1
    }

    /// Like [`NetService::fetch_blocking`], reporting the id it recorded under
    /// so a caller can read the body back.
    pub fn fetch_blocking_tracked(
        &self,
        request: NetRequest,
    ) -> (RequestId, NetResult<FetchOutcome>) {
        let id = self.next_request_id();
        self.note_request(id, &request);
        let outcome = self.run_blocking(id, request);
        self.note_blocking_outcome(id, &outcome);
        (id, outcome)
    }

    /// The blocking half of the pause point (ADR-0032 D3).
    ///
    /// Same gate, same timeout and same release semantics as the async half —
    /// but resolved *inline*, because the caller is waiting on the return value
    /// rather than on a `NetEvent`. This is the path the top-level document
    /// takes, which is the one request a driver most wants to intercept.
    fn run_blocking(&self, id: RequestId, mut request: NetRequest) -> NetResult<FetchOutcome> {
        // **One budget for the whole operation**, not one per pause. A single
        // request can park twice — at the request pause and again at the auth
        // pause — and two independent 20 s waits exceed the engine's 30 s
        // command timeout, so the driver was told its `Page.navigate` had timed
        // out while the load was still going. The bound this constant documents
        // is only true if it is measured once.
        let deadline = Instant::now() + DEFAULT_INTERCEPT_TIMEOUT;
        let intercepted = self.intercepts(&request);
        if intercepted {
            self.announce_pause(id, &request);
            match self.await_decision(id, deadline) {
                Some(InterceptCommand::Continue { overrides, .. }) => {
                    apply_overrides(&mut request, &overrides);
                }
                Some(InterceptCommand::Fulfill { response, .. }) => {
                    // A stubbed redirect is followed here too — this is the
                    // document path, where a bodyless 302 is a blank page.
                    match fulfilled_redirect_target(&request.url, &response) {
                        Some(target) => {
                            if matches!(response.status, 301..=303) && request.method != "HEAD" {
                                request.method = String::from("GET");
                                request.body = None;
                            }
                            request.url = target;
                        }
                        None => return Ok(fulfilled_outcome(&request.url, *response)),
                    }
                }
                Some(InterceptCommand::Fail { error, .. }) => {
                    // `wire`, as on the async path: this is the driver's own
                    // `errorReason` rendered as Chrome's `net::ERR_…` name, and
                    // it comes back to the driver as `Page.navigate.errorText`.
                    return Err(NetError::wire(oxidepage_base::NetErrorKind::Blocked, error));
                }
                // An auth answer at a request pause, or the timeout: proceed
                // unmodified, which is the release answer (D7).
                Some(InterceptCommand::Auth { .. }) | None => {}
            }
        }
        // The offline gate, applied after the pause so a driver could fulfil or
        // fail the request itself (both return above).
        if self.is_offline_for(&request) {
            return Err(offline_error());
        }
        let mut outcome = self.block_on_fetch(&request)?;
        // The retry is a *second pause* under the same id, not a new mechanism.
        if intercepted && self.intercept.config().handle_auth {
            let challenge = parse_auth_challenge(
                &outcome.head.headers,
                outcome.head.status,
                &outcome.head.final_url,
            );
            if let Some(challenge) = challenge {
                self.announce_auth(id, &request, &challenge);
                if let Some(InterceptCommand::Auth {
                    response: AuthResponse::Provide { username, password },
                    ..
                }) = self.await_decision(id, deadline)
                {
                    // Exactly one retry, as on the async path: a server that
                    // refuses the credentials re-challenges, and looping here
                    // would hold the page thread for as long as it kept saying
                    // no. The second 401 is what the page then sees.
                    retarget_to_challenger(&mut request, &outcome.head.final_url);
                    apply_basic_auth(&mut request, challenge.source, &username, &password);
                    outcome = self.block_on_fetch(&request)?;
                }
                // `Default`, `CancelAuth` and the timeout all leave the stashed
                // 401/407 standing, which is what the page then sees.
            }
        }
        Ok(outcome)
    }

    /// One `block_on`, with emulated latency charged outside the request's own
    /// timeout (ADR-0032 D9).
    fn block_on_fetch(&self, request: &NetRequest) -> NetResult<FetchOutcome> {
        let latency = self.intercept.config().latency;
        let engine = self.engine.clone();
        let request = request.clone();
        self.pool.handle().block_on(async move {
            if !latency.is_zero() {
                tokio::time::sleep(latency).await;
            }
            engine.fetch(request).await
        })
    }
}

/// Turns one completed response into the three [`NetEvent`]s every consumer
/// expects, in order.
///
/// Shared by the spawned fetch and by a driver's `fulfillRequest`, and that
/// sharing is the point: the `Chunk` is skipped for an empty body, which makes
/// [`NetService::note_event`]'s `Done` arm the **only** place a bodyless
/// response closes its record out. A fulfil path that closed out differently
/// would leak the request as in-flight forever and hang every `networkidle`
/// wait — the contract ADR-0030 D5 records as having already cost a driver
/// twice.
fn emit_outcome(tx: &Sender<NetEvent>, id: RequestId, outcome: FetchOutcome) {
    let FetchOutcome { head, body } = outcome;
    let _ = tx.send(NetEvent::Headers {
        id,
        status: head.status,
        status_text: head.status_text,
        headers: head.headers,
        final_url: head.final_url,
        redirected: head.redirected,
        response_type: head.response_type,
    });
    if !body.is_empty() {
        let _ = tx.send(NetEvent::Chunk { id, data: body });
    }
    let _ = tx.send(NetEvent::Done { id });
}

/// Most redirects a driver may stub before the chain is refused.
const MAX_FULFILLED_REDIRECTS: u32 = 20;

/// The `Location` a fulfilled redirect names, resolved against the request URL.
///
/// `None` when the response is not a redirect, or names no usable target — in
/// which case the fulfilled response is delivered as-is, which is what a driver
/// stubbing a bare 3xx asked for.
fn fulfilled_redirect_target(request_url: &str, response: &FulfilledResponse) -> Option<String> {
    if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.as_str())?;
    let base = url::Url::parse(request_url).ok()?;
    let target = base.join(location).ok()?;
    (target.as_str() != request_url).then(|| target.to_string())
}

/// Builds the response a driver fabricated.
///
/// **`ResponseType::Basic`**, deliberately (ADR-0032 D5): it lets script read a
/// cross-origin `no-cors` body it could never otherwise read. Chrome behaves the
/// same way and `request.respond()` depends on it — the driver is the operator,
/// so this is a considered hole rather than an oversight.
fn fulfilled_outcome(url: &str, response: FulfilledResponse) -> FetchOutcome {
    let FulfilledResponse {
        status,
        status_text,
        headers,
        body,
    } = response;
    FetchOutcome {
        head: ResponseHead {
            status,
            status_text,
            final_url: url.to_owned(),
            headers,
            redirected: false,
            response_type: ResponseType::Basic,
        },
        body: Bytes::from(body),
    }
}

/// Applies a `continueRequest`'s rewrites.
///
/// The URL is *not* re-validated here: the protocol side rejects a malformed or
/// non-`http(s)` override on the command itself (ADR-0032 D5), and the fetch
/// pipeline re-enters `fetch_inner` from the top anyway, so `scheme_allowed`,
/// the per-hop re-check and the connector's address filter all still apply.
fn apply_overrides(request: &mut NetRequest, overrides: &RequestOverrides) {
    if let Some(url) = &overrides.url {
        request.url.clone_from(url);
    }
    if let Some(method) = &overrides.method {
        request.method.clone_from(method);
    }
    if let Some(headers) = &overrides.headers {
        // Into `header_overrides`, **not** `headers`. The latter is the script
        // slot and is filtered by the forbidden list and the `no-cors` CORS
        // safelist — rules about what a *page* may set. A subresource is
        // `RequestMode::NoCors`, so a driver's `x-trace`, `user-agent` or
        // `referer` override silently vanished on every `<img>`/`<script>`
        // while working on documents (the same reasoning as `auth`, ADR-0032 D8).
        request.header_overrides = Some(headers.clone());
    }
    if let Some(body) = &overrides.post_data {
        request.body = Some(body.clone());
    }
}

/// Re-points a retry at the URL that actually issued the challenge.
///
/// `FetchEngine::fetch` follows redirects internally, so the request handed to
/// it names the *first* hop while the 401/407 comes from `final_url`. Retrying
/// the original URL sends the credentials to the wrong origin — and
/// `fetch::strip_auth_on_cross_origin` then removes them at the redirect, so the
/// challenging server never sees them at all and the page gets the 401 back
/// however correct the credentials were.
///
/// A no-op when nothing redirected, which is the common case.
fn retarget_to_challenger(request: &mut NetRequest, final_url: &str) {
    if request.url != final_url {
        request.url = final_url.to_owned();
    }
}

/// Attaches HTTP Basic credentials, replacing any the request already carried.
///
/// The header name comes from the challenge's **source**: a 407 answered into
/// `Authorization` instead of `Proxy-Authorization` is refused by every proxy,
/// and the request then re-challenges forever.
fn apply_basic_auth(request: &mut NetRequest, source: AuthSource, username: &str, password: &str) {
    // Into `NetRequest::auth`, **not** `headers`: both auth header names are on
    // Fetch's forbidden-request-header list, so anything put in `headers` is
    // stripped before it reaches the wire. That list governs what *script* may
    // set; these credentials are the user agent's own.
    request.auth = Some((
        source.header().to_owned(),
        basic_auth_header(username, password),
    ));
}

/// Whether a URL is one the pause point applies to.
///
/// `file://` and `data:` are answered above the scheme gate (ADR-0029) and must
/// never pause — see [`NetService::intercepts`] for why a paused `data:` URL
/// hangs the page.
fn is_http_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

/// Chrome's error for a request made while network emulation says offline.
fn offline_error() -> NetError {
    NetError::wire(
        oxidepage_base::NetErrorKind::Io,
        "net::ERR_INTERNET_DISCONNECTED",
    )
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
