//! Construction options for a browser, a context and a page.
//!
//! These are deliberately *not* `oxidepage_page::PageOptions`. That type is
//! `!Send` — it carries an `Rc<dyn Fn>` dialog handler — so it cannot cross the
//! channel to the page thread. Everything here is flat `Send` data, turned into
//! a `PageOptions` **on the page thread**, where the `Rc` hooks are installed.

use std::time::Duration;

use oxidepage_page::{NavigatorProfile, ResourcePolicy, ScreenProfile, Viewport};

use crate::dialog::DialogPolicy;

/// Default wall-clock cap on waiting for a page to answer a command.
///
/// Every round trip is bounded, because a page parked in a dialog or inside a
/// synchronous document fetch legitimately cannot answer yet, and a driver
/// must learn that rather than hang.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on joining a page thread during close.
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default depth of a page's event channel before events are dropped.
pub const DEFAULT_EVENT_CAPACITY: usize = 4096;

/// Browser-wide construction options.
#[derive(Clone, Debug)]
pub struct BrowserOptions {
    /// The network resource policy for **every** page of this browser.
    ///
    /// Browser-wide and not per context on purpose: the SSRF connector is
    /// baked into the shared hyper client, so one connection pool means one
    /// policy (ADR-0027 D8).
    pub policy: ResourcePolicy,
    /// How long a call on a [`PageHandle`](crate::PageHandle) waits for its
    /// answer.
    pub command_timeout: Duration,
    /// How long [`Browser::close`](crate::Browser::close) waits for one page
    /// thread before giving up on it and detaching.
    pub close_timeout: Duration,
    /// Depth of each page's event channel.
    pub event_capacity: usize,
    /// Tokio worker threads for the browser's shared net pool.
    ///
    /// One runtime serves every page, and TLS handshakes plus gzip/brotli
    /// decode are CPU-bound, so the two threads a single page needed do not
    /// scale to sixty-four pages each allowed sixteen in-flight fetches.
    pub net_worker_threads: usize,
    /// Entries in the browser's shared HTTP cache.
    pub cache_entries: usize,
    /// Most pages one [`BrowserContext`](crate::BrowserContext) may hold.
    ///
    /// This is the popup blocker. Every `window.open` spawns an OS thread and a
    /// whole `Page` (a QuickJS realm, stylo data, a fetch engine), and nothing
    /// else bounds it: the `ScriptBudget` is per task and each `open` is fast,
    /// so `for (;;) window.open()` on attacker-controlled content would exhaust
    /// the host's threads and memory. Past the cap `window.open` returns `null`
    /// — which is what a browser with a popup blocker returns, and what
    /// ADR-0027 D12 already documents `None` from the hook to mean.
    ///
    /// Applies only to pages script opens. [`BrowserContext::new_page`] is the
    /// driver asking, and is never refused.
    ///
    /// [`BrowserContext::new_page`]: crate::BrowserContext::new_page
    pub max_pages_per_context: usize,
    /// Options for the default context, which
    /// [`Browser::new`](crate::Browser::new) creates before the caller can
    /// reach it.
    ///
    /// Without this the default context is stuck on `ContextOptions::default()`
    /// forever, and an embedder that wants — say — a viewport for it has no way
    /// to say so: `new_context` configures a *different* context, and pages
    /// created by a driver land in the default one.
    pub default_context: ContextOptions,
}

/// Default popup cap. Generous for real content, fatal to a loop.
pub const DEFAULT_MAX_PAGES_PER_CONTEXT: usize = 64;

/// Default size of the browser-wide HTTP cache. Larger than a single page's,
/// because every page of every context now shares this one.
pub const DEFAULT_SHARED_CACHE_ENTRIES: usize = 4096;

/// Worker threads for the shared net runtime: the machine's parallelism, kept
/// within sane bounds.
fn default_net_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(2, 16)
}

impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            policy: ResourcePolicy::default(),
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
            event_capacity: DEFAULT_EVENT_CAPACITY,
            max_pages_per_context: DEFAULT_MAX_PAGES_PER_CONTEXT,
            net_worker_threads: default_net_worker_threads(),
            cache_entries: DEFAULT_SHARED_CACHE_ENTRIES,
            default_context: ContextOptions::default(),
        }
    }
}

/// Per-context options. A context is the isolation boundary: its own cookie
/// jar, its own HTTP cache partition, its own `localStorage`.
#[derive(Clone, Debug, Default)]
pub struct ContextOptions {
    /// Default viewport for pages of this context; a page may override it.
    pub viewport: Option<Viewport>,
    /// Default navigator identity for pages of this context.
    pub navigator: Option<NavigatorProfile>,
    /// How dialogs are answered in pages of this context.
    pub dialog_policy: DialogPolicy,
    /// Virtual display profile for pages of this context.
    pub screen: Option<ScreenProfile>,
    /// Per-task JavaScript budget for pages of this context.
    pub script_budget: Option<Duration>,
    /// Fetch `<img>` resources only once they reach the viewport.
    pub lazy_images: bool,
    /// Let a viewport-rooted `IntersectionObserver` see the whole document.
    pub whole_document_visible: bool,
}

impl ContextOptions {
    /// The options a page **script** opened starts from.
    ///
    /// A popup has no [`NewPageOptions`] of its own — script asked for it, not
    /// the driver — so every per-page default has to come from the context, or
    /// the popup silently differs from its opener. Getting only `dialog_policy`
    /// from here meant a popup ran under the stock 10 s script budget while the
    /// page that opened it ran unbudgeted.
    #[must_use]
    pub(crate) fn page_defaults(&self) -> NewPageOptions {
        NewPageOptions {
            url: None,
            viewport: self.viewport,
            navigator: self.navigator.clone(),
            screen: self.screen,
            script_budget: self.script_budget,
            lazy_images: Some(self.lazy_images),
            whole_document_visible: Some(self.whole_document_visible),
            dialog_policy: Some(self.dialog_policy),
            suspended: false,
        }
    }

    /// Fills every unset field of `options` from this context.
    ///
    /// Used by **both** entry points. `new_page` merging only the `Option`
    /// fields is what let a context's `dialog_policy` and `lazy_images` reach
    /// pages script opened but not pages the driver asked for — the same
    /// context configured two different kinds of page.
    #[must_use]
    pub(crate) fn merge(&self, options: NewPageOptions) -> NewPageOptions {
        let defaults = self.page_defaults();
        NewPageOptions {
            viewport: options.viewport.or(defaults.viewport),
            navigator: options.navigator.clone().or(defaults.navigator),
            screen: options.screen.or(defaults.screen),
            script_budget: options.script_budget.or(defaults.script_budget),
            lazy_images: options.lazy_images.or(defaults.lazy_images),
            whole_document_visible: options
                .whole_document_visible
                .or(defaults.whole_document_visible),
            dialog_policy: options.dialog_policy.or(defaults.dialog_policy),
            ..options
        }
    }
}

/// Per-page options.
#[derive(Clone, Debug, Default)]
pub struct NewPageOptions {
    /// Document URL exposed as `document.URL` / `location.href` before any
    /// navigation. Does *not* navigate — call
    /// [`PageHandle::navigate`](crate::PageHandle::navigate) for that.
    pub url: Option<String>,
    /// Viewport override; falls back to the context's, then to the default.
    pub viewport: Option<Viewport>,
    /// Navigator identity override; falls back to the context's.
    pub navigator: Option<NavigatorProfile>,
    /// Virtual display profile. `None` derives one from the viewport.
    pub screen: Option<ScreenProfile>,
    /// Per-task wall-clock budget for JavaScript. `None` uses the page
    /// default; `Duration::MAX` disables it.
    pub script_budget: Option<Duration>,
    /// Fetch `<img>` resources only once they reach the viewport. `None`
    /// inherits the context's [`ContextOptions::lazy_images`].
    pub lazy_images: Option<bool>,
    /// Let a viewport-rooted `IntersectionObserver` see the whole document.
    /// `None` inherits the context's setting.
    pub whole_document_visible: Option<bool>,
    /// How `alert`/`confirm`/`prompt` are answered. `None` inherits the
    /// context's [`ContextOptions::dialog_policy`].
    pub dialog_policy: Option<DialogPolicy>,
    /// Start the page [suspended](crate::PageHandle::resume): its thread runs
    /// the loop but services only control work until `resume()`, so a driver
    /// can install instrumentation before anything executes.
    pub suspended: bool,
}

impl NewPageOptions {
    /// The policy this page answers dialogs with, once context defaults have
    /// been merged in.
    pub(crate) fn resolved_dialog_policy(&self) -> DialogPolicy {
        self.dialog_policy.unwrap_or_default()
    }
}
