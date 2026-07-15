//! `NetService`: the async net ↔ sync page bridge (design doc §7, ADR-0004).
//!
//! `NetService` owns a multi-thread tokio [`Runtime`] living *on the page
//! thread* (the page is not itself a runtime worker, so `handle.spawn` and
//! `block_on` are legal). Requests are `spawn`ed onto the runtime; their
//! progress flows back to the page as [`NetEvent`]s over a
//! `crossbeam_channel`. The page's event loop unifies "a net event OR the
//! next timer deadline OR the settle budget" into one blocking wait via
//! `Receiver::recv_deadline`, so timers and network both progress with no
//! busy-wait.
//!
//! The module loader (`js` ES modules) uses [`NetService::fetch_blocking`]:
//! it blocks only the page thread while tokio workers deliver the bytes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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

use crate::cookies::CookieJar;
use crate::error::{NetError, NetResult};
use crate::fetch::{FetchEngine, FetchOutcome, NetRequest, RequestDefaults, ResponseType};
use crate::policy::ResourcePolicy;

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

/// Owns the tokio runtime and the fetch engine; the single point the page
/// starts loads and blocks-on synchronous fetches through.
pub struct NetService {
    runtime: Runtime,
    engine: FetchEngine,
    policy: Arc<ResourcePolicy>,
    cookies: Arc<Mutex<CookieJar>>,
    tx: Sender<NetEvent>,
    next_id: Cell<u32>,
    cancels: RefCell<HashMap<RequestId, Arc<AtomicBool>>>,
    /// Bounds simultaneous in-flight spawned fetches.
    sem: Arc<Semaphore>,
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| {
                NetError::new(
                    oxidepage_base::NetErrorKind::Io,
                    format!("tokio runtime: {e}"),
                )
            })?;
        let policy = Arc::new(policy);
        let cookies = Arc::new(Mutex::new(CookieJar::new()));
        let engine = FetchEngine::new_with_defaults(
            Arc::clone(&policy),
            Arc::clone(&cookies),
            request_defaults,
        )?;
        let (tx, rx) = crossbeam_channel::unbounded();
        Ok((
            Self {
                runtime,
                engine,
                policy,
                cookies,
                tx,
                next_id: Cell::new(1),
                cancels: RefCell::new(HashMap::new()),
                sem: Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES)),
            },
            rx,
        ))
    }

    /// The runtime handle (for the module loader's `block_on`).
    #[must_use]
    pub fn handle(&self) -> Handle {
        self.runtime.handle().clone()
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

    fn spawn_fetch(&self, request: NetRequest) -> RequestId {
        let id = self.next_request_id();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.borrow_mut().insert(id, Arc::clone(&cancel));
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        let sem = Arc::clone(&self.sem);
        self.runtime.spawn(async move {
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
        self.runtime.block_on(self.engine.fetch(request))
    }
}
