//! [`Browser`]: the process-level object that owns the shared net stack and
//! hands out contexts.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oxidepage_net::NetPool;

use crate::context::{BrowserContext, ContextId, PageSettings};
use crate::error::{EngineError, EngineResult};
use crate::options::{BrowserOptions, ContextOptions};

pub(crate) struct BrowserInner {
    pub(crate) options: BrowserOptions,
    /// One tokio runtime, one hyper connection pool, one HTTP cache for the
    /// whole browser (design §7, ADR-0027 D7).
    pub(crate) net_pool: Arc<NetPool>,
    contexts: Mutex<Vec<BrowserContext>>,
    next_context: AtomicU64,
    /// Set by [`Browser::close`] before it walks the contexts.
    ///
    /// The same race `ContextInner::closed` closes one level down: without it a
    /// context (or a page inside one) created after the walk is spawned holding
    /// the `Arc<NetPool>` and is never joined by the `close` that promised to
    /// join everything.
    closed: AtomicBool,
}

/// A browser: shared network stack, one or more [`BrowserContext`]s, and the
/// pages inside them. `Send + Sync`; cloning gives another handle to the same
/// browser.
#[derive(Clone)]
pub struct Browser(pub(crate) Arc<BrowserInner>);

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser").finish_non_exhaustive()
    }
}

impl Browser {
    /// Builds a browser and its default context.
    pub fn new(options: BrowserOptions) -> EngineResult<Self> {
        let net_pool = NetPool::with_options(
            Arc::new(options.policy.clone()),
            oxidepage_net::NetPoolOptions {
                worker_threads: options.net_worker_threads,
                cache_entries: options.cache_entries,
            },
        )
        .map_err(|e| EngineError::Launch(e.to_string()))?;
        let inner = Arc::new(BrowserInner {
            options,
            net_pool,
            contexts: Mutex::new(Vec::new()),
            next_context: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        let browser = Self(inner);
        // Context 0 always exists, so `default_context()` never fails.
        browser.spawn_context(ContextOptions::default());
        Ok(browser)
    }

    /// The context created with the browser.
    ///
    /// Survives [`Browser::close`]: closing marks a context closed rather than
    /// forgetting it, so this getter cannot start panicking under a second
    /// handle that has not observed the close. Opening a page on a closed
    /// context returns [`EngineError::Closed`].
    ///
    /// # Panics
    ///
    /// Never: the default context is created by [`Browser::new`] and is never
    /// removed from the list.
    #[must_use]
    pub fn default_context(&self) -> BrowserContext {
        self.contexts()
            .into_iter()
            .next()
            .expect("the default context is created with the browser and never removed")
    }

    /// Creates an isolated context: its own cookie jar and cache partition.
    pub fn new_context(&self, options: ContextOptions) -> BrowserContext {
        self.spawn_context(options)
    }

    fn spawn_context(&self, options: ContextOptions) -> BrowserContext {
        let id = ContextId(self.0.next_context.fetch_add(1, Ordering::Relaxed));
        let settings = PageSettings {
            event_capacity: self.0.options.event_capacity,
            command_timeout: self.0.options.command_timeout,
            close_timeout: self.0.options.close_timeout,
            max_pages: self.0.options.max_pages_per_context,
        };
        let context = BrowserContext::new(id, Arc::clone(&self.0.net_pool), settings, options);
        // Recovered, not skipped: dropping the context on a poisoned lock would
        // leave `default_context()` — documented never to panic — with an empty
        // list, and `close()` joining nothing while the page threads run on.
        let mut contexts = self.0.contexts.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.closed.load(Ordering::Acquire) {
            drop(contexts);
            context.close();
            return context;
        }
        contexts.push(context.clone());
        drop(contexts);
        context
    }

    #[must_use]
    pub fn contexts(&self) -> Vec<BrowserContext> {
        self.0
            .contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Closes every context, every page, and joins their threads.
    ///
    /// Bounded: each page gets [`BrowserOptions::close_timeout`] before being
    /// detached, so no page can hold the browser open. Idempotent.
    ///
    /// Ordering matters. Page threads are joined *first*, and only then does
    /// the last `Arc<NetPool>` reference drop — dropping a tokio `Runtime` from
    /// one of its own worker threads panics, and a page thread that still held
    /// the pool could be the one to drop it.
    ///
    /// Dropping the last `Browser` handle does the same thing through
    /// [`Drop for BrowserInner`](BrowserInner), which is only reachable because
    /// a context does **not** hold an `Arc` back to its browser.
    pub fn close(&self) {
        // Flag first: a context spawned after this point closes itself rather
        // than outliving the close. The list is *not* emptied — a `Browser` is
        // `Clone`, and a second holder calling `default_context()` after a
        // close must get a closed context, not a panic.
        self.0.closed.store(true, Ordering::Release);
        for context in self.contexts() {
            context.close();
        }
    }
}

impl Drop for BrowserInner {
    fn drop(&mut self) {
        // Best-effort: a `Browser` dropped with live pages still tears them
        // down in the right order. `close()` is the version that reports.
        let contexts = match self.contexts.lock() {
            Ok(mut contexts) => std::mem::take(&mut *contexts),
            Err(_) => Vec::new(),
        };
        for context in &contexts {
            context.close();
        }
    }
}
