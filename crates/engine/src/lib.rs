//! Public embedding API: `Browser` → `BrowserContext` → `PageHandle`
//! (design doc §7, ADR-0027).
//!
//! A [`Page`](oxidepage_page::Page) is permanently `!Send`: rquickjs is pinned
//! without its `parallel` feature, and stylo keeps thread-local caches
//! (ADR-0005 D3). So a driver that wants several pages at once runs **one page
//! per OS thread** and talks to each over a channel. Everything in this crate
//! is the `Send + Sync` half of that arrangement.
//!
//! ```no_run
//! use oxidepage_engine::{Browser, BrowserOptions, NewPageOptions};
//! use oxidepage_engine::page_api::WaitUntil;
//!
//! let browser = Browser::new(BrowserOptions::default())?;
//! let context = browser.default_context();
//! let page = context.new_page(NewPageOptions::default())?;
//!
//! page.navigate("https://example.com/", WaitUntil::Load)?.ok();
//! let title = page.eval_to_string("document.title")?;
//! browser.close();
//! # Ok::<_, oxidepage_engine::EngineError>(())
//! ```
//!
//! ## What is shared, and what is not
//!
//! | | Scope |
//! |---|---|
//! | tokio runtime, hyper connection pool, [`ResourcePolicy`](oxidepage_net::ResourcePolicy) | browser |
//! | HTTP cache (partitioned per context) | browser |
//! | cookie jar | context |
//! | byte/request budgets, connection ceiling, web fonts | page |
//!
//! The resource policy is browser-wide because the SSRF connector is baked
//! into the shared client (ADR-0027 D8); the byte and request budgets stay per
//! page because sharing the fetch engine would silently turn a per-page bound
//! into a browser-wide one.
//!
//! ## Talking to a busy page
//!
//! Commands are boxed closures run *on* the page thread ([`PageHandle::with`]),
//! so a `Ref<'_, DomTree>` never has to cross a channel. They are a task source
//! of the page's own event loop, under the same guard a queued navigation uses:
//! a command that arrives mid-navigation is parked until the loop is back at
//! top level. Closing a page and answering a dialog are the exceptions — they
//! touch nothing but `Cell`s, so they run at whatever wait point receives them.

mod browser;
mod context;
mod dialog;
mod error;
mod event;
mod options;
mod page;

pub use browser::Browser;
pub use context::{BrowserContext, ContextId};
pub use dialog::{DEFAULT_DIALOG_TIMEOUT, DialogPolicy};
pub use error::{EngineError, EngineResult};
pub use event::PageEvent;
pub use options::{
    BrowserOptions, ContextOptions, DEFAULT_CLOSE_TIMEOUT, DEFAULT_COMMAND_TIMEOUT,
    DEFAULT_EVENT_CAPACITY, DEFAULT_MAX_PAGES_PER_CONTEXT, DEFAULT_SHARED_CACHE_ENTRIES,
    NewPageOptions,
};
pub use page::{PageHandle, PageId};

/// The page API a [`PageHandle::with`] closure works against, re-exported so an
/// embedder needs no `oxidepage-page` dependency of its own.
pub mod page_api {
    pub use oxidepage_page::{
        CallArgument, ConsoleLevel, ConsoleMessage, CookieSource, CookieView, DialogEvent,
        DialogKind, DialogRequest, DialogResponse, EvaluateOptions, EvaluationResult,
        ExceptionDetails, HistoryEntryInfo, ImageFormat, LoopStats, Margins, NavigationEvent,
        NavigationEventKind, NavigationHistory, NavigatorProfile, NetworkEvent, NodeId, Page,
        PaintOptions, PaperSize, PdfOptions, Point, PropertyDescriptor, Rect, RemoteError,
        RemoteObject, RemoteSubtype, RemoteType, RequestId, ResourcePolicy, ScreenProfile,
        ScreenshotOptions, ScriptError, ScriptErrorKind, Size, StackFrame, ValuePreview, Viewport,
        WaitUntil, render_preview_top,
    };
}

pub use page_api::{DialogResponse, ResourcePolicy, Viewport, WaitUntil};
