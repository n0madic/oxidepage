//! The target registry: what a `targetId` names, and how a page's events reach
//! the sockets watching it.
//!
//! Three things live here because they are one problem:
//!
//! 1. **Identity.** CDP addresses everything by opaque hex ids. The engine
//!    addresses pages by [`PageId`] and contexts by [`ContextId`], both `u64`
//!    counters. The registry is the only place the two vocabularies meet.
//! 2. **The event pump.** [`PageHandle::events`] hands back a *clone of one
//!    receiver*, and cloning a `crossbeam` receiver splits the stream instead of
//!    duplicating it (`crates/engine/src/page.rs:173`). So there must be exactly
//!    one reader per page, forever, and it cannot be a connection — connections
//!    come and go while the page keeps running. The pump is that reader: one OS
//!    thread per page, started with the target, ended by `PageEvent::Closed`.
//! 3. **Fan-out.** The pump republishes onto a broadcast bus that every open
//!    connection subscribes to. A connection decides for itself which of those
//!    signals its sessions care about, because "is `Page` enabled" is per
//!    session, not per page.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use oxidepage_engine::page_api::{NavigationEventKind, NetworkEvent, RequestId, ResourceType};
use oxidepage_engine::{
    Browser, BrowserContext, ContextId, EngineError, NewPageOptions, PageEvent, PageHandle, PageId,
};
use serde::Serialize;

use crate::token::random_hex;

/// What a driver sees when it enumerates targets.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub url: String,
    pub attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_context_id: Option<String>,
}

/// A signal broadcast to every connection.
///
/// Deliberately *not* a protocol event: whether it becomes one — and with which
/// `sessionId` — is a per-connection decision that depends on which domains that
/// connection has enabled.
#[derive(Debug, Clone)]
pub enum TargetSignal {
    Created(TargetInfo),
    InfoChanged(TargetInfo),
    Destroyed {
        target_id: String,
    },
    /// One page event, tagged with the target it came from.
    Page {
        target_id: String,
        event: PageEvent,
    },
}

struct TargetEntry {
    info: TargetInfo,
    page: PageHandle,
    context: BrowserContext,
    /// CDP's `loaderId`: an opaque id for *this document load*, not for the
    /// frame. Drivers use it to tell a fresh document from a same-document
    /// change, so it must be re-minted on every commit and must not be reused.
    loader_id: String,
    /// The loader minted for the navigation now in flight (ADR-0032 D6a), and
    /// whether a commit has adopted it yet.
    pending_loader: Option<PendingLoad>,
}

/// The navigation in flight, and the loader id its document will have.
///
/// **Minted when the navigation *starts*, not when it commits**, because that
/// is what Chrome does and what two separate Puppeteer mechanisms depend on:
///
/// * `Page.lifecycleEvent { name: "init" }` is the **only** event that sets
///   `frame._loaderId`, and `LifecycleWatcher` resolves a navigation only once
///   that value differs from the one it captured before the navigation. An
///   `init` carrying the *outgoing* loader leaves `page.goto()` hanging until
///   its own timeout — which is exactly what happened after a navigation that
///   failed without committing, since the committed loader had not moved.
/// * `isNavigationRequest` is `requestId === loaderId && type === 'Document'`,
///   so the document request's protocol id is this same string.
///
/// The **committed** loader ([`TargetEntry::loader_id`]) only changes on a
/// commit, so a navigation that fails does not retire the current document's
/// id — a driver telling documents apart by loader must not see a phantom one.
#[derive(Debug, Clone)]
struct PendingLoad {
    /// The engine's id for the document request, once it has gone out. `None`
    /// between the navigation starting and its request being announced — and
    /// for a commit with no request at all (`about:blank`). Kept so
    /// `Network.getResponseBody` can map the substituted protocol id back.
    request: Option<RequestId>,
    loader: String,
    /// Set once a commit has taken this loader. A later commit with no
    /// navigation of its own must mint a fresh loader rather than re-use this.
    adopted: bool,
}

struct Registry {
    targets: HashMap<String, TargetEntry>,
    /// Insertion order, so `Target.getTargets` is stable across calls.
    order: Vec<String>,
    /// Protocol id -> engine context, the default context included: every
    /// target reports the id of the context it is in, and only
    /// `Target.getBrowserContexts` hides the default one.
    contexts: HashMap<String, BrowserContext>,
    subscribers: Vec<(SubscriptionId, Sender<TargetSignal>)>,
    next_subscription: u64,
}

/// Identifies one connection's subscription so it can be cancelled.
///
/// Cancellation has to be explicit: the registry holds the `Sender`, so the
/// `Receiver` never sees a disconnect on its own and a thread blocked in
/// `recv()` would never wake. Dropping the receiver is not enough either — the
/// registry would only notice on the next `publish`, which may never come for
/// an idle browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionId(u64);

/// A connection's handle on the broadcast bus.
pub struct Subscription {
    pub id: SubscriptionId,
    pub signals: Receiver<TargetSignal>,
}

/// Shared, `Send + Sync` state behind every connection.
#[derive(Clone)]
pub struct TargetRegistry(Arc<Shared>);

struct Shared {
    browser: Browser,
    /// The context the browser was born with.
    ///
    /// Read from the browser rather than assumed: contexts are numbered from
    /// **1** (`Browser::new` seeds `next_context` at 1), so a hardcoded
    /// `ContextId(0)` matches nothing and would report the default context as a
    /// disposable one — which a driver would then dispose, taking the browser's
    /// own pages with it.
    default_context: ContextId,
    inner: Mutex<Registry>,
}

impl TargetRegistry {
    /// Wraps a browser, adopting any pages it already has.
    #[must_use]
    pub fn new(browser: Browser) -> Self {
        let default_context = browser.default_context().id();
        let registry = TargetRegistry(Arc::new(Shared {
            browser,
            default_context,
            inner: Mutex::new(Registry {
                targets: HashMap::new(),
                order: Vec::new(),
                contexts: HashMap::new(),
                subscribers: Vec::new(),
                next_subscription: 0,
            }),
        }));
        for context in registry.0.browser.contexts() {
            let id = registry.adopt_context(&context);
            for page in context.pages() {
                // `None`: a pre-existing page may be anywhere, so its URL is
                // read back by the pump instead of guessed at here.
                registry.adopt_page(page, context.clone(), Some(id.clone()), None);
            }
        }
        registry
    }

    #[must_use]
    pub fn browser(&self) -> &Browser {
        &self.0.browser
    }

    /// Subscribes a connection to the broadcast bus.
    ///
    /// Unbounded on purpose: the alternative is dropping protocol events, and a
    /// driver that misses a `loadEventFired` waits for it until its own timeout
    /// fires. The engine's bus upstream is already bounded
    /// (`BrowserOptions::event_capacity`), so the backpressure that matters is
    /// applied where a page can be told about it — `PageEvent::Dropped`.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut inner = self.lock();
        inner.next_subscription += 1;
        let id = SubscriptionId(inner.next_subscription);
        inner.subscribers.push((id, tx));
        Subscription { id, signals: rx }
    }

    /// Ends a subscription, disconnecting the receiver so the connection's
    /// event thread returns from `recv()`.
    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.lock()
            .subscribers
            .retain(|(existing, _)| *existing != id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        // A poisoned registry means a connection thread panicked mid-update.
        // The map is still structurally sound (every mutation under this lock
        // is a single insert or remove), and refusing to serve any further
        // request would turn one bad command into a dead server.
        self.0.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(inner: &mut Registry, signal: TargetSignal) {
        inner
            .subscribers
            .retain(|(_, tx)| tx.send(signal.clone()).is_ok());
    }

    /// Registers a context under a protocol id, returning it.
    pub fn adopt_context(&self, context: &BrowserContext) -> String {
        let mut inner = self.lock();
        if let Some((id, _)) = inner
            .contexts
            .iter()
            .find(|(_, existing)| existing.id() == context.id())
        {
            return id.clone();
        }
        let id = random_hex();
        inner.contexts.insert(id.clone(), context.clone());
        id
    }

    #[must_use]
    pub fn context(&self, id: &str) -> Option<BrowserContext> {
        self.lock().contexts.get(id).cloned()
    }

    /// Every browsing context, the default one included.
    ///
    /// Unlike [`TargetRegistry::context_ids`], which deliberately hides the
    /// default context from `Target.getBrowserContexts`, a browser-level
    /// command with no `browserContextId` means *all* of them.
    #[must_use]
    pub fn all_contexts(&self) -> Vec<BrowserContext> {
        self.lock().contexts.values().cloned().collect()
    }

    /// Every context id except the default one's — Chrome does not list the
    /// default context in `Target.getBrowserContexts`, and Puppeteer treats a
    /// listed id as a disposable incognito context.
    #[must_use]
    pub fn context_ids(&self) -> Vec<String> {
        let default_context = self.0.default_context;
        let inner = self.lock();
        inner
            .contexts
            .iter()
            .filter(|(_, context)| context.id() != default_context)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Closes a context and every target in it.
    ///
    /// The default context is refused: it is the one context the browser cannot
    /// live without (`Browser::default_context` is documented never to panic),
    /// and it is never handed out by `getBrowserContexts`, so a request to
    /// dispose it means the driver is confused rather than deliberate.
    pub fn dispose_context(&self, id: &str) -> Result<(), EngineError> {
        let context = {
            let mut inner = self.lock();
            match inner.contexts.get(id) {
                Some(context) if context.id() == self.0.default_context => {
                    return Err(EngineError::Closed);
                }
                _ => inner.contexts.remove(id),
            }
        };
        let Some(context) = context else {
            return Err(EngineError::Closed);
        };
        for page in context.pages() {
            self.destroy(&self.target_id_for(page.id()).unwrap_or_default());
        }
        context.close();
        Ok(())
    }

    /// Creates a page target in `context` and starts its pump.
    pub fn create_page(
        &self,
        context: &BrowserContext,
        options: NewPageOptions,
    ) -> Result<String, EngineError> {
        let context_id = self.adopt_context(context);
        // The document URL of a page that has not navigated is exactly what was
        // asked for, so it is known without a round trip.
        let initial_url = options
            .url
            .clone()
            .unwrap_or_else(|| String::from("about:blank"));
        let page = context.new_page(options)?;
        // **Every** page reports a `browserContextId`, the default context's
        // included, which is what Chrome does — and Playwright asserts on it in
        // `_onAttachedToTarget`, so an absent one makes `connectOverCDP` throw
        // before a single check can run. What keeps a driver from *disposing*
        // the default context is that `Target.getBrowserContexts` still does
        // not list it (see `context_ids`), and `dispose_context` refuses it
        // outright.
        Ok(self.adopt_page(page, context.clone(), Some(context_id), Some(initial_url)))
    }

    /// Registers an already-running page and spawns its pump thread.
    ///
    /// `initial_url` is taken on trust rather than read back from the page. A
    /// probe would be a blocking `PageHandle::with`, and an ordinary job on a
    /// **suspended** page is deferred until `resume()` — which a driver using
    /// `setAutoAttach { waitForDebuggerOnStart: true }` can only send *after*
    /// `Target.createTarget` answers. That deadlock resolves only when the
    /// 30 s command timeout expires, long past the driver's own. The pump
    /// corrects the URL asynchronously for pages that were adopted rather than
    /// created here.
    fn adopt_page(
        &self,
        page: PageHandle,
        context: BrowserContext,
        browser_context_id: Option<String>,
        initial_url: Option<String>,
    ) -> String {
        let target_id = random_hex();
        let info = TargetInfo {
            target_id: target_id.clone(),
            kind: String::from("page"),
            title: String::new(),
            url: initial_url.clone().unwrap_or_default(),
            attached: false,
            browser_context_id: browser_context_id.clone(),
        };

        {
            let mut inner = self.lock();
            inner.order.push(target_id.clone());
            inner.targets.insert(
                target_id.clone(),
                TargetEntry {
                    info: info.clone(),
                    page: page.clone(),
                    context,
                    loader_id: random_hex(),
                    pending_loader: None,
                },
            );
            Self::publish(&mut inner, TargetSignal::Created(info));
        }

        // Only a page whose URL the caller could not supply needs the probe.
        self.spawn_pump(target_id.clone(), page, initial_url.is_none());
        target_id
    }

    /// The one reader of a page's event stream.
    ///
    /// It must outlive every connection and must never be duplicated — see the
    /// module header. The thread ends when the stream ends, which the engine
    /// guarantees by making `Closed` (or `Crashed`) the last event.
    fn spawn_pump(&self, target_id: String, page: PageHandle, probe_url: bool) {
        let events = page.events();
        let registry = self.clone();
        let name = format!("cdp-pump-{}", page.id());
        let pumped = target_id.clone();
        let probe = page.clone();
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            // Reconcile the URL of a page this registry *adopted* rather than
            // created, where the caller could not know it. Blocking is fine
            // here — this thread has nothing else to do yet, and a suspended
            // page simply answers once it resumes. That is precisely why the
            // probe does not belong on the creation path, which must answer
            // `Target.createTarget` promptly.
            if probe_url
                && let Ok(url) = probe.with(|p| p.dom().document_url().to_owned())
                && !url.is_empty()
            {
                registry.update_info(&pumped, Some(&url), None);
            }
            while let Ok(event) = events.recv() {
                let terminal = matches!(event, PageEvent::Closed | PageEvent::Crashed { .. });
                // A committed navigation changes what the target *is*, which
                // is registry state rather than per-connection state — so it
                // is folded in once, here, instead of by every connection.
                if let PageEvent::Navigation(navigation) = &event {
                    match navigation.kind {
                        // A navigation *starting* mints the loader its document
                        // will have (ADR-0032 D6a). It has to happen here,
                        // before the signal is published, because the `init`
                        // lifecycle event built from this very signal must
                        // carry it — an `init` naming the outgoing loader hangs
                        // `page.goto()` outright.
                        NavigationEventKind::Started => {
                            registry.begin_navigation(&pumped);
                        }
                        // A cross-document commit is a *new document*: it
                        // adopts the pending loader. Folded in once, here,
                        // rather than by every connection, so every session
                        // sees the same id.
                        NavigationEventKind::Committed => {
                            registry.update_info(&pumped, Some(&navigation.url), None);
                            registry.new_loader(&pumped);
                        }
                        NavigationEventKind::SameDocument => {
                            registry.update_info(&pumped, Some(&navigation.url), None);
                        }
                        // A navigation that never committed — it failed, or it
                        // turned out to be a download — must not leave its
                        // loader pending. `loading_loader_id` would keep
                        // reporting an id no document ever had, so every later
                        // lifecycle event for the document that *is* current
                        // would carry a phantom loader: exactly what D6a says a
                        // driver telling documents apart by loader must not see.
                        NavigationEventKind::Failed => {
                            registry.abandon_navigation(&pumped);
                        }
                        _ => {}
                    }
                }
                // The top-level document request is the one whose protocol id
                // *is* the loader; this records which request that is.
                if let PageEvent::Network(NetworkEvent::Requested {
                    id,
                    resource_type: ResourceType::Document,
                    ..
                }) = &event
                {
                    registry.begin_document_load(&pumped, *id);
                }
                {
                    let mut inner = registry.lock();
                    Self::publish(
                        &mut inner,
                        TargetSignal::Page {
                            target_id: pumped.clone(),
                            event,
                        },
                    );
                }
                if terminal {
                    break;
                }
            }
            registry.destroy(&pumped);
        });
        if spawned.is_err() {
            // Out of threads: the target exists but will never report an event.
            // Destroying it immediately is more honest than leaving a target a
            // driver can attach to and then wait on forever.
            self.destroy(&target_id);
        }
    }

    /// Removes a target and announces it. Idempotent — the pump and an explicit
    /// `Target.closeTarget` both call it, and they race by design.
    pub fn destroy(&self, target_id: &str) {
        let mut inner = self.lock();
        if inner.targets.remove(target_id).is_none() {
            return;
        }
        inner.order.retain(|id| id != target_id);
        Self::publish(
            &mut inner,
            TargetSignal::Destroyed {
                target_id: target_id.to_owned(),
            },
        );
    }

    #[must_use]
    pub fn page(&self, target_id: &str) -> Option<PageHandle> {
        self.lock().targets.get(target_id).map(|t| t.page.clone())
    }

    #[must_use]
    pub fn context_of(&self, target_id: &str) -> Option<BrowserContext> {
        self.lock()
            .targets
            .get(target_id)
            .map(|t| t.context.clone())
    }

    #[must_use]
    pub fn info(&self, target_id: &str) -> Option<TargetInfo> {
        self.lock().targets.get(target_id).map(|t| t.info.clone())
    }

    /// The id of the document currently **committed** in `target_id`.
    ///
    /// Unchanged by a navigation that fails, which is what keeps a driver from
    /// seeing a phantom document. For the loader a *loading* document will have,
    /// see [`TargetRegistry::loading_loader_id`].
    #[must_use]
    pub fn loader_id(&self, target_id: &str) -> Option<String> {
        self.lock()
            .targets
            .get(target_id)
            .map(|t| t.loader_id.clone())
    }

    /// The loader every event of the load in flight belongs to: the pending one
    /// if a navigation has started and not yet committed, else the committed
    /// one.
    ///
    /// This is what `Page.lifecycleEvent` must carry. `init` is the only event
    /// that sets Puppeteer's `frame._loaderId`, and `LifecycleWatcher` resolves
    /// a navigation only when that value has *changed* — so an `init` carrying
    /// the outgoing loader hangs `page.goto()` outright.
    #[must_use]
    pub fn loading_loader_id(&self, target_id: &str) -> Option<String> {
        let inner = self.lock();
        let entry = inner.targets.get(target_id)?;
        Some(match &entry.pending_loader {
            Some(pending) if !pending.adopted => pending.loader.clone(),
            _ => entry.loader_id.clone(),
        })
    }

    /// Mints the loader for a navigation that is *starting*, and returns it.
    ///
    /// Called on `NavigationEventKind::Started`, which is what makes the `init`
    /// lifecycle event carry the new document's loader — see [`PendingLoad`].
    pub fn begin_navigation(&self, target_id: &str) -> Option<String> {
        let loader = random_hex();
        let mut inner = self.lock();
        let entry = inner.targets.get_mut(target_id)?;
        entry.pending_loader = Some(PendingLoad {
            request: None,
            loader: loader.clone(),
            adopted: false,
        });
        Some(loader)
    }

    /// Commits the pending loader, or mints one for a commit that had no
    /// navigation of its own.
    ///
    /// Called on a *cross-document* commit only. A same-document navigation
    /// keeps its loader, which is exactly the distinction a driver reads it for.
    pub fn new_loader(&self, target_id: &str) -> Option<String> {
        let mut inner = self.lock();
        let entry = inner.targets.get_mut(target_id)?;
        let loader = match &mut entry.pending_loader {
            Some(pending) if !pending.adopted => {
                pending.adopted = true;
                pending.loader.clone()
            }
            _ => random_hex(),
        };
        entry.loader_id = loader.clone();
        Some(loader)
    }

    /// Drops the pending loader of a navigation that never committed.
    ///
    /// Only if it is still unadopted: a `Failed` that follows a *committed*
    /// navigation (a subresource giving up, a later same-document step) must
    /// leave the current document's loader alone.
    pub fn abandon_navigation(&self, target_id: &str) {
        let mut inner = self.lock();
        let Some(entry) = inner.targets.get_mut(target_id) else {
            return;
        };
        if entry
            .pending_loader
            .as_ref()
            .is_some_and(|pending| !pending.adopted)
        {
            entry.pending_loader = None;
        }
    }

    /// Records which request is the document request of the load in flight, so
    /// its protocol id can be the loader (ADR-0032 D6a).
    ///
    /// The loader is **not** minted here — `begin_navigation` already did, at
    /// the `Started` that preceded this. A request arriving with no pending
    /// navigation mints one anyway rather than losing the association.
    pub fn begin_document_load(&self, target_id: &str, request: RequestId) -> Option<String> {
        let mut inner = self.lock();
        let entry = inner.targets.get_mut(target_id)?;
        match &mut entry.pending_loader {
            Some(pending) if !pending.adopted => {
                pending.request = Some(request);
                Some(pending.loader.clone())
            }
            _ => {
                let loader = random_hex();
                entry.pending_loader = Some(PendingLoad {
                    request: Some(request),
                    loader: loader.clone(),
                    adopted: false,
                });
                Some(loader)
            }
        }
    }

    /// The substituted protocol id for `request`, iff it is the document
    /// request of the load now in flight (or the one that produced the current
    /// document).
    #[must_use]
    pub fn document_loader(&self, target_id: &str, request: RequestId) -> Option<String> {
        let inner = self.lock();
        let pending = inner.targets.get(target_id)?.pending_loader.as_ref()?;
        (pending.request == Some(request)).then(|| pending.loader.clone())
    }

    /// The inverse of [`TargetRegistry::document_loader`]: the engine request a
    /// substituted protocol id names, so `Network.getResponseBody` can answer
    /// for the document a driver just navigated to.
    #[must_use]
    pub fn request_for_loader(&self, target_id: &str, loader: &str) -> Option<RequestId> {
        let inner = self.lock();
        let pending = inner.targets.get(target_id)?.pending_loader.as_ref()?;
        (pending.loader == loader)
            .then_some(pending.request)
            .flatten()
    }

    #[must_use]
    pub fn target_id_for(&self, page_id: PageId) -> Option<String> {
        let inner = self.lock();
        inner
            .order
            .iter()
            .find(|id| {
                inner
                    .targets
                    .get(*id)
                    .is_some_and(|t| t.page.id() == page_id)
            })
            .cloned()
    }

    /// Every live target, in creation order.
    #[must_use]
    pub fn infos(&self) -> Vec<TargetInfo> {
        let inner = self.lock();
        inner
            .order
            .iter()
            .filter_map(|id| inner.targets.get(id).map(|t| t.info.clone()))
            .collect()
    }

    /// Records a new URL/title for a target and broadcasts `InfoChanged` — but
    /// only when something actually changed, so a page that fires several
    /// lifecycle events for one navigation does not produce four identical
    /// `Target.targetInfoChanged` events.
    pub fn update_info(&self, target_id: &str, url: Option<&str>, attached: Option<bool>) {
        let mut inner = self.lock();
        let Some(entry) = inner.targets.get_mut(target_id) else {
            return;
        };
        let mut changed = false;
        if let Some(url) = url
            && entry.info.url != url
        {
            entry.info.url = url.to_owned();
            changed = true;
        }
        if let Some(attached) = attached
            && entry.info.attached != attached
        {
            entry.info.attached = attached;
            changed = true;
        }
        if !changed {
            return;
        }
        let info = entry.info.clone();
        Self::publish(&mut inner, TargetSignal::InfoChanged(info));
    }
}
