//! [`BrowserContext`]: the isolation boundary between groups of pages.
//!
//! Pages of one context share a cookie jar and an HTTP cache partition; pages
//! of different contexts share neither. What they *do* share browser-wide is
//! the tokio runtime, the hyper connection pool and the resource policy — see
//! [`NetPool`](oxidepage_net::NetPool) and ADR-0027 D7/D8.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oxidepage_net::{CachePartition, CookieJar};
use oxidepage_page::{SharedLocalStorage, SharedNetConfig};

use oxidepage_net::NetPool;

use crate::error::{EngineError, EngineResult};
use crate::options::{ContextOptions, NewPageOptions};
use crate::page::{PageHandle, PageId, spawn_page};

/// The browser-level knobs a page thread needs, copied into each context so a
/// context never has to reach back at its browser (see [`ContextInner`]).
#[derive(Copy, Clone, Debug)]
pub(crate) struct PageSettings {
    pub(crate) event_capacity: usize,
    pub(crate) command_timeout: std::time::Duration,
    pub(crate) close_timeout: std::time::Duration,
    pub(crate) max_pages: usize,
}

/// Identifies a context within a [`Browser`](crate::Browser).
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ContextId(pub u64);

pub(crate) struct ContextInner {
    id: ContextId,
    /// The browser's shared net stack and page-thread settings, held **by
    /// value** rather than through a back-reference to `BrowserInner`.
    ///
    /// A `ContextInner` that pointed back at the `BrowserInner` whose
    /// `contexts` list owns it would be a reference cycle: the browser's
    /// refcount could never reach zero, `Drop for BrowserInner` would never
    /// run, and every page thread plus the tokio runtime would leak for the
    /// life of the process. Copying the two things a context actually needs
    /// costs one `Arc` clone and a `Copy` struct, and breaks the cycle by
    /// construction rather than by remembering to use `Weak`.
    net_pool: Arc<NetPool>,
    page_settings: PageSettings,
    options: ContextOptions,
    cookies: Arc<Mutex<CookieJar>>,
    partition: CachePartition,
    /// Every origin's `localStorage`, shared by all pages of this context
    /// (ADR-0027 D13). `sessionStorage` is per page and never lives here.
    local_storage: SharedLocalStorage,
    pages: Mutex<Vec<PageHandle>>,
    next_page: AtomicU64,
    /// Where this context's pages write a `Content-Disposition: attachment`
    /// response (ADR-0032 D13).
    ///
    /// Mutable, unlike the rest of `options`, because
    /// `Browser.setDownloadBehavior` is a *runtime* command and a driver
    /// routinely sends it before it has created a page. Storing it here is what
    /// makes it apply to the pages created afterwards, rather than to nothing.
    download_path: Mutex<Option<std::path::PathBuf>>,
    /// Set by [`BrowserContext::close`] before it takes the page list.
    ///
    /// Without it, a `window.open` in flight during the close pushes its new
    /// page into the emptied list *after* `close` has read it: nobody ever asks
    /// that page to stop, its thread runs on holding the `Arc<NetPool>`, and
    /// the tokio runtime outlives the `Browser::close` that promised to join
    /// everything.
    closed: AtomicBool,
}

/// A group of pages sharing cookies and cache. `Send + Sync`; cloning gives
/// another handle to the same context.
#[derive(Clone)]
pub struct BrowserContext(pub(crate) Arc<ContextInner>);

impl std::fmt::Debug for BrowserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserContext")
            .field("id", &self.0.id)
            .finish_non_exhaustive()
    }
}

impl BrowserContext {
    pub(crate) fn new(
        id: ContextId,
        net_pool: Arc<NetPool>,
        page_settings: PageSettings,
        options: ContextOptions,
    ) -> Self {
        let options_download_path = options.download_path.clone();
        Self(Arc::new(ContextInner {
            id,
            net_pool,
            page_settings,
            options,
            cookies: Arc::new(Mutex::new(CookieJar::new())),
            // One partition per context, so a shared cache cannot leak one
            // context's history to another through hit/miss timing.
            partition: CachePartition(id.0),
            local_storage: SharedLocalStorage::default(),
            pages: Mutex::new(Vec::new()),
            next_page: AtomicU64::new(1),
            download_path: Mutex::new(options_download_path),
            closed: AtomicBool::new(false),
        }))
    }

    #[must_use]
    pub fn id(&self) -> ContextId {
        self.0.id
    }

    /// The options this context was created with.
    ///
    /// Exposed so a caller can create a *sibling* configured like an existing
    /// one. A driver's `createBrowserContext` is the case that needs it: the
    /// operator configured the browser's default context (viewport, dialog
    /// policy), and an incognito context that silently reverted to the stock
    /// defaults would answer dialogs differently from every other page in the
    /// same process.
    #[must_use]
    pub fn options(&self) -> ContextOptions {
        self.0.options.clone()
    }

    /// This context's cookie jar, shared by all its pages.
    #[must_use]
    pub fn cookies(&self) -> Arc<Mutex<CookieJar>> {
        Arc::clone(&self.0.cookies)
    }

    /// This context's `localStorage`, keyed by origin and shared by all its
    /// pages — the thing a driver's `storageState()` will read (ADR-0027 D13).
    #[must_use]
    pub fn local_storage(&self) -> SharedLocalStorage {
        Arc::clone(&self.0.local_storage)
    }

    /// Opens a page on a fresh thread.
    ///
    /// Blocks until the page's realm exists, so a construction failure is a
    /// `Result` here rather than a panic on a thread nobody is watching.
    pub fn new_page(&self, options: NewPageOptions) -> EngineResult<PageHandle> {
        self.open_page(options, None)
    }

    /// `window.open` from a page of this context (ADR-0027 D12).
    ///
    /// Called **on the opener's thread**, with JavaScript on its stack and its
    /// DOM borrowed. That is only safe because nothing the new thread does
    /// touches the opener: it builds its own `Page` and answers a one-shot
    /// channel. The context lock is taken to record the page and released
    /// before anything else, so two pages opening each other cannot deadlock.
    pub(crate) fn open_window(
        &self,
        request: &oxidepage_page::OpenWindowRequest,
    ) -> Option<oxidepage_page::OpenedWindow> {
        let inner = &self.0;
        // The popup cap is enforced inside `open_page`, under the same lock as
        // the push — checking it here and spawning after would let two page
        // threads calling `window.open` at the limit both pass. `None` from
        // this hook is `window.open` returning `null`, exactly what a browser
        // does under a popup blocker (D12).
        //
        // Named targets are not implemented, so every `open` makes a page:
        // `window.open(u, "x")` twice opens two (D12).
        let handle = self
            .open_page(inner.options.page_defaults(), Some(self.clone()))
            .ok()?;

        if let Some(url) = request.url.clone() {
            // Fire-and-forget. `post` never blocks — which matters here,
            // because this runs on the *opener's* thread with JavaScript on
            // its stack.
            let _ = handle.post(move |page| {
                let _ = page.navigate(&url, oxidepage_page::WaitUntil::Load);
            });
        }

        let closed = handle.closed_flag();
        let ops = handle.window_ops();
        Some(oxidepage_page::OpenedWindow {
            closed,
            ops: Arc::new(move |op| ops.apply(op)),
        })
    }

    fn open_page(
        &self,
        options: NewPageOptions,
        opener_context: Option<BrowserContext>,
    ) -> EngineResult<PageHandle> {
        let script_opened = opener_context.is_some();
        // A driver asking gets the driver's timeout; script asking blocks its
        // own page thread, so it gets the far shorter script-blocking budget.
        let launch_timeout = if script_opened {
            crate::page::OPEN_WINDOW_TIMEOUT
        } else {
            self.0.page_settings.command_timeout
        };
        let inner = &self.0;
        // Context in the high half, page counter in the low half, so a page id
        // names its context and no two contexts can mint the same one.
        let id = PageId((inner.id.0 << 32) | inner.next_page.fetch_add(1, Ordering::Relaxed));
        let mut options = inner.options.merge(options);
        // The *live* download path wins over the one the context was built
        // with: a driver's `Browser.setDownloadBehavior` before this page
        // existed has to reach it.
        options.download_path = options.download_path.or_else(|| {
            inner
                .download_path
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        let net = SharedNetConfig {
            pool: Arc::clone(&inner.net_pool),
            cookies: Arc::clone(&inner.cookies),
            partition: inner.partition,
        };
        if inner.closed.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        let handle = spawn_page(
            id,
            options,
            net,
            Arc::clone(&inner.local_storage),
            // A page can open siblings only into the context it belongs to.
            opener_context.unwrap_or_else(|| self.clone()),
            inner.page_settings,
            launch_timeout,
        )?;
        // Re-checked under the page-list lock: `close` sets the flag and then
        // takes the list, so a page that gets here after that must be closed
        // itself rather than pushed into a list nobody will read again.
        let mut pages = inner.pages.lock().unwrap_or_else(|e| e.into_inner());
        // Drop the handles of pages whose threads have finished. Nothing else
        // does: `PageHandle::close` has no way back to its context, so a driver
        // looping open/navigate/close would retain one `PageInner` — and its
        // whole undrained event channel — per page it had ever created.
        pages.retain(|page| !page.has_exited());
        // Each refusal names itself: a shut-down context and a context at its
        // popup limit mean opposite things about the context, and the cap is
        // the one worth logging and testing.
        let refused = if inner.closed.load(Ordering::Acquire) {
            Some(EngineError::Closed)
        } else if script_opened && pages.len() >= inner.page_settings.max_pages {
            // The popup cap, checked under the lock that pushes.
            Some(EngineError::PopupBlocked)
        } else {
            None
        };
        if let Some(refused) = refused {
            drop(pages);
            handle.close();
            return Err(refused);
        }
        pages.push(handle.clone());
        drop(pages);
        Ok(handle)
    }

    /// `Browser.setDownloadBehavior` for this context (ADR-0032 D13).
    ///
    /// Applied to the pages that exist **and** remembered for the ones that do
    /// not yet: a driver routinely sets the behavior before it creates a page,
    /// and applying it only to the current list would make that call a no-op it
    /// had every reason to believe worked.
    pub fn set_download_path(&self, path: Option<std::path::PathBuf>) {
        *self
            .0
            .download_path
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = path.clone();
        let behavior = match path {
            Some(path) => oxidepage_page::DownloadBehavior::Allow(path),
            None => oxidepage_page::DownloadBehavior::Deny,
        };
        for page in self.pages() {
            let behavior = behavior.clone();
            // Best effort: a page whose thread has already gone is not an
            // error here — `Browser.close` races this by design.
            let _ = page.with(move |p| p.set_download_behavior(behavior));
        }
    }

    /// Every page of this context that has not exited.
    #[must_use]
    pub fn pages(&self) -> Vec<PageHandle> {
        let mut pages = self.0.pages.lock().unwrap_or_else(|e| e.into_inner());
        // Retained on thread liveness, not on `is_closed`: a page a sibling's
        // `w.close()` marked closed is still running, and dropping its handle
        // here would leave nobody to join it.
        pages.retain(|page| !page.has_exited());
        pages.clone()
    }

    /// Closes every page of this context and joins their threads.
    pub fn close(&self) {
        // Flag first, list second: anything that spawns a page between the two
        // sees the flag under the same lock and closes it itself.
        self.0.closed.store(true, Ordering::Release);
        let pages = {
            let mut pages = self.0.pages.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *pages)
        };
        for page in &pages {
            page.close();
        }
    }
}
