//! The page: event loop v2 and document lifecycle (design doc §5.4, §6).
//!
//! One [`Page`] owns one realm, one document, one single-threaded scheduler,
//! and one [`NetService`] (a tokio runtime living on the page thread). The
//! loop implements the HTML event-loop skeleton: a timer min-heap, microtask
//! checkpoints after every task and callback into JS, GC-finalization
//! processing between tasks, and — new in Phase 3 — a networking task source.
//!
//! [`Page::wait_for_work`] unifies "a net event OR an embedder command OR the
//! next timer deadline OR the settle budget" into one blocking wait
//! ([`Page::settle`]), so timers, network and a driver's commands all progress
//! with no busy-wait (ADR-0004, ADR-0027 D4).
//!
//! A page an embedder drives directly (the CLI, this crate's tests) has no
//! command port and behaves exactly as it did before one existed; a driver that
//! wants many pages puts each on its own thread and hands it
//! [`Page::run_command_loop`] (ADR-0027).
//!
//! Document loading is a streaming parse with script execution at
//! `</script>` suspension points. Classic inline scripts run immediately;
//! parser-blocking external scripts fetch synchronously and block the parse;
//! `defer` and module scripts run in order after parsing, before
//! `DOMContentLoaded`; `async` scripts run on arrival. `document.write`
//! remains unsupported (design §12).

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
// Geometry is part of this crate's own public surface (`layout_rect`,
// `content_quads`, `ScreenshotOptions::clip`), so an embedder needs no
// `oxidepage_base` dependency of its own.
pub use oxidepage_base::{NodeId, Point, Rect, RequestId, Size};
/// The remote object model (ADR-0030): CDP's `Runtime` vocabulary as plain,
/// `Send` Rust data. The live values stay in `WorldState`.
pub use oxidepage_bindings::remote::{
    EvaluationResult, ExceptionDetails, MAX_REMOTE_OBJECTS, PropertyDescriptor, RemoteObject,
    RemoteSubtype, RemoteType,
};
use oxidepage_bindings::{
    BindCx, EventTargetKey, HostHooks, NavigationBody, PendingNavigation, TimingMilestone,
    WorldEnter, WorldId, WorldState, is_classic_script_type,
};
use oxidepage_dom::{DomTree, ParseOptions, ParseSignal, Parser, StyleUpdate};
use oxidepage_js::{
    JsEngine, JsError, JsRealm, JsValue, ModuleSource, PromiseState, QuickJsEngine, QuickJsRealm,
    RealmOptions,
};
use oxidepage_layout::{LayoutEngine, PaintStamp};
use oxidepage_net::{FetchOutcome, NetEvent, NetRequest, NetService, decode_charset};
use oxidepage_style::{BlockingImportLoader, CssFetcher, StyleEngine};
pub use remote::{
    AwaitToken, CallArgument, EvaluateOptions, EvaluateOutcome, MAX_PROPERTIES, RemoteError,
};
use style::selector_parser::PseudoElement;
use style::stylesheets::Origin;

mod command;
mod domnode;
pub mod node_handle;
pub mod remote;
mod render;
mod worlds;
pub use command::{LoopStats, PageJob};
pub use domnode::{KeyEvent, LayoutMetrics, MAX_DESCRIPTION_DEPTH, NodeDescription, NodeRef};

pub use render::{ImageFormat, ScreenshotOptions};

// The observable page-event vocabulary is built in `bindings` (the call sites
// are the only places with a JS scope) but is an embedder-facing part of
// *this* crate's API, so it is re-exported wholesale: one import path, no
// `oxidepage_bindings` dependency in an embedder's Cargo.toml (ADR-0025 D9).
pub use oxidepage_bindings::{
    BindingCall, ConsoleLevel, ConsoleMessage, DialogEvent, DialogHandler, DialogKind,
    DialogRequest, DialogResponse, KeyEventKind, KeyInput, MAX_STORAGE_ORIGINS, Modifiers,
    MouseEventKind, MouseInput, OpenWindowRequest, OpenedWindow, PREVIEW_MAX_DEPTH,
    PREVIEW_MAX_ENTRIES, PREVIEW_MAX_NODES, PREVIEW_MAX_STRING, PrivateStorageAreas,
    STORAGE_QUOTA_BYTES, ScriptError, ScriptErrorKind, SharedStorage, StorageArea, StorageAreaKind,
    StorageNotification, StorageSubscriber, ValuePreview, WheelInput, WindowOp,
    evict_unreferenced_areas, key_for_code, render_preview, render_preview_top,
};
pub use oxidepage_export_pdf::{MAX_PDF_PAGES, Margins, PaperSize, PdfOptions};
pub use oxidepage_js::{StackFrame, parse_stack};
pub use oxidepage_layout::{BoxQuads, disable_system_fonts};
pub use oxidepage_net::{
    AuthChallenge, AuthResponse, AuthSource, CachePartition, CookieJar, CookieSource, CookieView,
    DEFAULT_INTERCEPT_TIMEOUT, FulfilledResponse, InterceptCommand, InterceptControl, NetPool,
    NetworkEvent, RequestOverrides, RequestPattern, ResourcePolicy, ResourceType, SharedNetConfig,
};
pub use oxidepage_paint::{DisplayList, PaintOptions};
pub use oxidepage_raster_skia::{RasterImage, RasterOptions};
pub use oxidepage_style::Viewport;

/// Wall-clock cap on waiting for subresources (async scripts, etc.) before
/// firing `load` regardless.
const SUBRESOURCE_BUDGET: Duration = Duration::from_secs(30);

/// How many ` (n)` suffixes a download filename may try before giving up.
///
/// An existing file is never overwritten — a page must not be able to replace
/// an earlier download by naming it — so a collision needs an alternative, and
/// the search needs a bound.
const MAX_DOWNLOAD_NAME_ATTEMPTS: u32 = 100;

/// HTML timer nesting level beyond which a sub-4ms timeout is clamped up to
/// 4ms ("timer initialization steps"). This also stops a zero-delay
/// self-reposting `setTimeout`/`setInterval` from starving the event loop: the
/// rescheduled timer stops being immediately due, so `run_until_stalled`
/// returns instead of spinning.
const MAX_TIMER_NESTING: u32 = 5;

/// Minimum timeout for a deeply nested timer (HTML clamps to 4ms).
const MIN_NESTED_TIMER_DELAY: Duration = Duration::from_millis(4);

/// Cap above which the `cleared` / `raf_cancelled` id sets are pruned against
/// live entries, bounding memory on long-lived pages that cancel timers/frames
/// that never existed or already fired.
const CANCELLED_SET_PRUNE_CAP: usize = 1024;

/// Consecutive navigations chained off one entry point before the page gives
/// up. `location.href = location.href` in a `load` handler is an infinite loop
/// in a browser too — the difference is that a browser has a user who can close
/// the tab, and a headless engine has a caller waiting for a return.
const MAX_CHAINED_NAVIGATIONS: usize = 20;

/// Milestones retained by [`Page::drain_navigation_events`] before the oldest
/// are dropped.
///
/// Draining is the embedder's job and nothing forces it: a driver session that
/// never calls it would otherwise accumulate one owned URL `String` per
/// milestone forever. Bounded for the same reason `MAX_HISTORY_ENTRIES` bounds
/// the session history — the newest events are the ones a driver acts on, so
/// the front of the stream is what gets dropped.
const MAX_NAVIGATION_EVENTS: usize = 1024;

/// Console lines, script errors and dialogs retained before the oldest are
/// dropped. Bounded for the reason [`MAX_NAVIGATION_EVENTS`] is — draining is
/// the embedder's job and nothing forces it — and more urgently here, because
/// a console message now retains an owned tree of argument previews.
pub const MAX_CONSOLE_MESSAGES: usize = 1024;
pub const MAX_SCRIPT_ERRORS: usize = 1024;
pub const MAX_DIALOG_EVENTS: usize = 256;

/// Sibling-page `storage` notifications retained before the oldest are dropped.
///
/// Bounded like every other stream on a `Page`, and for a sharper reason: the
/// producer is *another page's thread*. A sibling writing in a loop while this
/// page is parked in a long navigation would otherwise grow this queue without
/// any bound this page controls.
pub const MAX_STORAGE_EVENTS: usize = 1024;

/// Where a commit puts its session-history entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HistoryTarget {
    /// Truncate forward entries and append (`location.assign`, a link click).
    Push,
    /// Overwrite the current entry (`location.replace`, `reload`).
    Replace,
    /// A traversal: move the index to an entry that already exists.
    Traverse(usize),
}

/// What a load is, beyond its URL — the three things a
/// [`PendingNavigation::Load`] carries that change how the response is handled.
///
/// Grouped rather than passed as three parameters because they travel together
/// through every navigation path and are meaningless apart: each one on its own
/// is the difference between "stay in this document" and "fetch".
struct LoadKind {
    /// A form submission's request body.
    body: Option<NavigationBody>,
    /// `location.reload()`: skip the HTTP cache.
    reload: bool,
    /// `<a download>`'s value — `Some` makes the response a download whatever
    /// it says it is. See [`Page::take_download`].
    download: Option<String>,
}

impl LoadKind {
    /// An ordinary load: no body, no cache bypass, no download request.
    fn plain() -> Self {
        Self {
            body: None,
            reload: false,
            download: None,
        }
    }

    /// Whether this load can be satisfied without leaving the document, given a
    /// URL that differs only in its fragment.
    fn is_plain(&self) -> bool {
        self.body.is_none() && !self.reload && self.download.is_none()
    }
}

/// The fragment of a URL, `None` when it has none or does not parse. Comparing
/// these is what decides whether `hashchange` fires.
fn fragment_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.fragment().map(ToOwned::to_owned))
}

/// Percent-decodes a URL fragment as UTF-8, leaving invalid sequences as-is —
/// an `id` is matched against the decoded form (`#a%20b` finds `id="a b"`).
fn percent_decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map_or_else(|_| value.to_owned(), |s| s.into_owned())
}

/// Converts a script-supplied `delay_ms` into a scheduler `Duration`.
///
/// The HTML timeout is a signed 32-bit integer, so non-finite or out-of-range
/// values are clamped rather than panicking `Duration::from_secs_f64` /
/// `Instant::now() + delay` (e.g. `setTimeout(fn, Infinity)`). Deeply nested
/// timers are additionally raised to a 4ms floor.
fn clamp_timer_delay(delay_ms: f64, nesting: u32) -> Duration {
    let ms = if delay_ms.is_finite() {
        delay_ms.clamp(0.0, f64::from(i32::MAX))
    } else {
        0.0
    };
    let mut delay = Duration::from_millis(ms as u64);
    if nesting > MAX_TIMER_NESTING && delay < MIN_NESTED_TIMER_DELAY {
        delay = MIN_NESTED_TIMER_DELAY;
    }
    delay
}

/// A milestone in the life of one navigation.
///
/// The stream is the engine's own record of "what happened to this page", and
/// deliberately shaped like the protocol surface that will consume it: a CDP
/// layer renames `Committed` to `Page.frameNavigated` and the rest to
/// `Page.lifecycleEvent` without inventing anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavigationEventKind {
    /// A navigation began (before the request is made).
    Started,
    /// It resolved to a same-document navigation: no request, no new document.
    SameDocument,
    /// The response arrived and the new document replaced the old one.
    Committed,
    DomContentLoaded,
    Load,
    /// [`Page::settle`] returned having reached idle: no timer, no pending
    /// animation frame, nothing in flight.
    NetworkIdle,
    /// The navigation did not happen. `error` says why; the previous document
    /// is still the current one.
    Failed,
}

/// One entry in the navigation event stream (see [`Page::drain_navigation_events`]).
#[derive(Clone, Debug)]
pub struct NavigationEvent {
    pub kind: NavigationEventKind,
    /// The URL the event is about — the target for `Started`/`Failed`, the
    /// committed document URL afterwards.
    pub url: String,
    pub error: Option<String>,
    /// Unix-epoch milliseconds, from the page's monotonic time origin.
    pub timestamp: f64,
    /// Whether this commit kept the document's execution contexts alive
    /// (ADR-0034 D2).
    ///
    /// True only for a `document.open()`/`close()` replacement, where HTML
    /// keeps the same `Document`, the same `Window` and the same environment
    /// settings object — so the realms, their ids and the handles minted
    /// against them all survive. Every other commit renumbers the main world
    /// and rebuilds the isolated ones (ADR-0033 D9), and a driver must be told
    /// its contexts died.
    pub contexts_preserved: bool,
}

/// One observable thing the page did, pushed to an [`EventSink`] as it
/// happens (ADR-0027 D6).
///
/// Deliberately **not** `#[non_exhaustive]`: a driver maps these onto its own
/// event vocabulary, and a new variant must break that mapping at compile time
/// rather than be dropped silently — the same drift protection the IDL codegen
/// buys in `bindings`.
///
/// The same vocabulary the pull API exposes — [`Page::drain_console`],
/// [`Page::drain_errors`], [`Page::drain_dialog_events`],
/// [`Page::drain_navigation_events`] — delivered the other way round. A driver
/// that must forward events *while* a page works cannot poll a stream it is not
/// running the loop for; an embedder that drives the page itself (the CLI) has
/// no such problem and keeps the pull API.
#[derive(Clone, Debug)]
pub enum PageRecord {
    Navigation(NavigationEvent),
    Console(ConsoleMessage),
    Error(ScriptError),
    /// A dialog is **open** and the page is parked on it.
    ///
    /// Emitted before the handler runs, which is the only point at which
    /// telling a driver is useful: a handler that waits for an answer from
    /// another thread can only be answered by someone who knows the dialog
    /// exists. The completed [`PageRecord::Dialog`] still follows.
    DialogOpening(DialogRequest),
    /// A dialog that has been answered, with the answer it got.
    Dialog(DialogEvent),
    /// One step of a network request's life.
    ///
    /// The bus carried no network events before ADR-0030: nothing about a
    /// request was retained, and a driver's `Network` domain needs both the
    /// milestones and the body. Both arrived together rather than as a half
    /// version a later stage would replace (ADR-0027's recorded deviation).
    Network(oxidepage_net::NetworkEvent),
    /// Page script called a function installed by [`Page::add_binding`].
    ///
    /// A task source rather than something the embedder polls: a binding called
    /// from a timer, with no command in flight, must still reach the driver
    /// promptly — polling would hold it until the next unrelated command.
    Binding {
        name: String,
        payload: String,
        /// The execution context the call came from, so a binding installed
        /// in an isolated world is attributed to that world (ADR-0033 D10).
        context_id: u64,
    },
    /// A promise a deferred evaluation was parked on has settled, or its
    /// budget ran out (ADR-0034 D1).
    ///
    /// The answer to a [`Runtime.evaluate`](Page::evaluate_in) /
    /// `callFunctionOn` / `awaitPromise` that returned
    /// [`EvaluateOutcome::Deferred`](crate::EvaluateOutcome::Deferred). The
    /// embedder matches it back to the call by `token` and replies then — which
    /// is what frees the command channel to carry the very command that
    /// resolves the promise.
    AwaitSettled {
        token: remote::AwaitToken,
        result: oxidepage_bindings::remote::EvaluationResult,
    },
    /// An `<input type=file>` was activated and a driver asked to intercept
    /// the chooser (ADR-0032 D12).
    ///
    /// Recorded, not answered: unlike a dialog the page is **not** parked, so
    /// the driver replies whenever it likes with
    /// [`Page::set_file_input_files`].
    FileChooser(FileChooserEvent),
    /// A `Content-Disposition: attachment` navigation (ADR-0032 D13).
    ///
    /// Two per download — one `InProgress`, one `Completed`/`Canceled` — and
    /// the second carries the path. Recorded even under
    /// [`DownloadBehavior::Deny`], as a `Canceled`: a driver that asked for a
    /// download and got silence cannot tell a refusal from a broken link.
    Download(DownloadEvent),
}

/// Every origin's `localStorage`, shared by the pages of one browsing context.
///
/// A map rather than a single area because `localStorage` is keyed by origin,
/// and a page that navigates cross-origin must not carry the old origin's data
/// with it. Populated lazily: an origin gets an area the first time a document
/// there asks for one.
pub type SharedLocalStorage = Arc<Mutex<HashMap<String, SharedStorage>>>;

/// Opens a new browsing context for `window.open` and `<a target=_blank>`
/// (ADR-0027 D12).
///
/// **Runs with JavaScript on the stack**, under the same constraints as
/// [`DialogHandler`]: plain data in, plain data out, and no path back into the
/// `Page`. Returning `None` means "the popup was blocked" — a real browser
/// answer, and what `window.open` reports as `null`.
pub type OpenWindowHandler = Rc<dyn Fn(&OpenWindowRequest) -> Option<OpenedWindow>>;

/// A sink installed by [`Page::set_event_sink`].
///
/// `Rc`, and called with JS on the stack, for the same reason
/// [`DialogHandler`] is: it is invoked from deep inside the funnels that
/// produce these records. It must not re-enter the page — send the record
/// somewhere and return.
pub type EventSink = Rc<dyn Fn(PageRecord)>;

/// How far a [`Page::navigate`] waits before returning.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WaitUntil {
    /// Return once `DOMContentLoaded` has fired (parse + deferred scripts).
    DomContentLoaded,
    /// Return once `load` has fired (subresources settled).
    #[default]
    Load,
}

/// A `Send` snapshot of the session history, for a driver on another thread.
///
/// Deliberately not a view of [`oxidepage_bindings::SessionHistory`]: an entry
/// there holds the `history.state` as a live `JsValue`, which belongs to the
/// realm's thread and cannot cross a channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavigationHistory {
    /// Index of the entry the page is currently on.
    pub current_index: usize,
    pub entries: Vec<HistoryEntryInfo>,
}

/// One entry of a [`NavigationHistory`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntryInfo {
    pub url: String,
}

/// Immutable browser identity and practical Navigator capabilities for a Page.
#[derive(Clone, Debug)]
pub struct NavigatorProfile {
    pub user_agent: String,
    pub vendor: String,
    pub platform: String,
    pub languages: Vec<String>,
    pub hardware_concurrency: u64,
    pub webdriver: bool,
    pub max_touch_points: u32,
}

impl Default for NavigatorProfile {
    fn default() -> Self {
        let (ua_platform, platform) = default_navigator_platform();
        Self {
            user_agent: format!(
                "Mozilla/5.0 ({ua_platform}) OxidePage/{}",
                env!("CARGO_PKG_VERSION")
            ),
            vendor: String::new(),
            platform,
            languages: vec!["en-US".to_owned()],
            hardware_concurrency: std::thread::available_parallelism()
                .map_or(1, |value| value.get())
                .clamp(1, 8) as u64,
            webdriver: false,
            max_touch_points: 0,
        }
    }
}

/// The `Accept-Language` header a language list implies.
///
/// The first tag carries no `q`, each later one drops a tenth — the shape every
/// browser sends, and the one half of a locale override that a *header* can
/// express.
fn accept_language_for(languages: &[String]) -> String {
    languages
        .iter()
        .enumerate()
        .map(|(index, language)| {
            if index == 0 {
                language.clone()
            } else {
                format!("{language};q={:.1}", 1.0 - index as f64 / 10.0)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Checks a language list, shared by [`NavigatorProfile::validate`] and
/// [`Page::set_languages`] so a driver's override is held to exactly the rules
/// an embedder's profile is.
fn validate_languages(languages: &[String]) -> Result<(), JsError> {
    if languages.is_empty() || languages.len() > 10 {
        return Err(JsError::Engine(
            "navigator.languages must contain between 1 and 10 entries".into(),
        ));
    }
    if languages.iter().any(|language| {
        language.is_empty()
            || language
                .split('-')
                .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric()))
    }) {
        return Err(JsError::Engine(
            "navigator.languages contains an invalid language tag".into(),
        ));
    }
    Ok(())
}

impl NavigatorProfile {
    fn validate(&self) -> Result<(), JsError> {
        if self.user_agent.chars().any(char::is_control) {
            return Err(JsError::Engine(
                "navigator.userAgent must not contain control characters".into(),
            ));
        }
        validate_languages(&self.languages)?;
        if self.hardware_concurrency == 0 {
            return Err(JsError::Engine(
                "navigator.hardwareConcurrency must be greater than zero".into(),
            ));
        }
        oxidepage_net::RequestDefaults::new(&self.user_agent, &self.accept_language())
            .map_err(|error| JsError::Engine(format!("invalid navigator profile: {error}")))?;
        Ok(())
    }

    fn accept_language(&self) -> String {
        accept_language_for(&self.languages)
    }

    fn bindings_data(&self) -> oxidepage_bindings::NavigatorData {
        oxidepage_bindings::NavigatorData::new(
            self.user_agent.clone(),
            self.vendor.clone(),
            self.platform.clone(),
            self.languages.clone(),
            self.hardware_concurrency,
            self.webdriver,
            self.max_touch_points,
        )
    }
}

fn default_navigator_platform() -> (String, String) {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "windows" => (format!("Windows; {arch}"), "Win32".to_owned()),
        "macos" => (format!("Macintosh; {arch}"), format!("Mac {arch}")),
        "linux" => (format!("X11; Linux {arch}"), format!("Linux {arch}")),
        os => (format!("{os}; {arch}"), format!("{os} {arch}")),
    }
}

/// Immutable virtual-display metrics exposed through `window.screen`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenProfile {
    pub width: u32,
    pub height: u32,
    pub avail_width: u32,
    pub avail_height: u32,
    pub color_depth: u32,
    pub pixel_depth: u32,
}

impl ScreenProfile {
    #[must_use]
    pub fn from_viewport(viewport: Viewport) -> Self {
        let data = oxidepage_bindings::ScreenData::from_viewport(viewport);
        Self {
            width: data.width,
            height: data.height,
            avail_width: data.avail_width,
            avail_height: data.avail_height,
            color_depth: data.color_depth,
            pixel_depth: data.pixel_depth,
        }
    }

    fn validate(self) -> Result<(), JsError> {
        if self.width == 0 || self.height == 0 || self.avail_width == 0 || self.avail_height == 0 {
            return Err(JsError::Engine(
                "screen dimensions must be greater than zero".into(),
            ));
        }
        if self.avail_width > self.width || self.avail_height > self.height {
            return Err(JsError::Engine(
                "available screen dimensions must not exceed physical dimensions".into(),
            ));
        }
        if !(1..=64).contains(&self.color_depth) || !(1..=64).contains(&self.pixel_depth) {
            return Err(JsError::Engine(
                "screen color and pixel depth must be between 1 and 64".into(),
            ));
        }
        Ok(())
    }

    fn bindings_data(self) -> oxidepage_bindings::ScreenData {
        oxidepage_bindings::ScreenData {
            width: self.width,
            height: self.height,
            avail_width: self.avail_width,
            avail_height: self.avail_height,
            color_depth: self.color_depth,
            pixel_depth: self.pixel_depth,
        }
    }
}

/// Page construction options.
#[derive(Default)]
pub struct PageOptions {
    pub realm: RealmOptions,
    /// Document URL exposed as `document.URL` / `location.href`.
    pub url: Option<String>,
    /// Network resource policy. Defaults to the secure [`ResourcePolicy`].
    pub policy: Option<ResourcePolicy>,
    /// The viewport (CSS px + device pixel ratio) used for media queries and
    /// layout. Defaults to 800×600@1x.
    pub viewport: Option<Viewport>,
    /// JavaScript Navigator identity and matching HTTP request defaults.
    pub navigator: NavigatorProfile,
    /// Virtual display profile. `None` derives an honest profile from viewport.
    pub screen: Option<ScreenProfile>,
    /// Wall-clock budget for one task's stay in JavaScript. `None` uses
    /// [`DEFAULT_SCRIPT_BUDGET`]; [`Duration::MAX`] disables the budget.
    pub script_budget: Option<Duration>,
    /// Fetch `<img>` resources only once they reach the viewport (plus one
    /// viewport of margin), instead of eagerly on connect. Off by default: the
    /// whole document is what most embedders render.
    ///
    /// A lazy page is only complete for the *viewport*. Before a full-page
    /// screenshot or a PDF, call [`Page::load_deferred_images`] — otherwise
    /// everything below the fold paints as a hole. `background-image` is always
    /// eager (ADR-0014).
    pub lazy_images: bool,
    /// Let an `IntersectionObserver` with the implicit (viewport) root see the
    /// whole document as its root, not just the viewport rectangle.
    ///
    /// Off by default: the viewport is the root, as the spec says. Turn it on
    /// when the output *is* the whole document (full-page screenshot, PDF).
    /// Script routinely gates real content on being observed — a sponsor grid
    /// that renders only once it scrolls into view — and the page never
    /// scrolls here, so with the viewport root that content is simply missing
    /// from the capture. This is the script-driven twin of the `lazy_images` /
    /// [`Page::load_deferred_images`] problem, and wants the same answer.
    pub whole_document_visible: bool,
    /// Answers `window.alert`/`confirm`/`prompt`. `None` auto-dismisses, which
    /// is the policy both Puppeteer and Playwright apply when no `dialog`
    /// listener is attached.
    ///
    /// Here rather than only on [`Page::set_dialog_handler`] because
    /// [`load_html_page`] runs inline scripts *during* the call: a
    /// post-construction setter cannot answer a dialog raised at parse time.
    pub dialog_handler: Option<DialogHandler>,
    /// Opens a sibling browsing context for `window.open` and a link with a
    /// `target` other than `_self`.
    ///
    /// `None` — the default — means there is exactly one browsing context, so
    /// `window.open` returns `null` and a `target=_blank` link navigates in
    /// place with a warning. Only a driver that owns several pages
    /// (`oxidepage-engine`) can do better.
    pub open_window_handler: Option<OpenWindowHandler>,
    /// `localStorage` areas shared with the rest of a browsing context, keyed
    /// by origin (ADR-0027 D13).
    ///
    /// `None` — the default — gives the page its own areas, which is the
    /// behavior a standalone `Page` has always had. `sessionStorage` is always
    /// per page, so it is never shared through here.
    pub local_storage: Option<SharedLocalStorage>,
    /// Net stack shared with sibling pages: one tokio runtime and connection
    /// pool per browser, one cookie jar and cache partition per browsing
    /// context (ADR-0027 D7).
    ///
    /// `None` — the default — gives the page a private runtime, pool, cache and
    /// jar, which is what a standalone `Page` and the CLI want. When set,
    /// [`PageOptions::policy`] is ignored: the SSRF connector is baked into the
    /// shared pool's client, so the policy is the pool's (D8).
    pub net: Option<SharedNetConfig>,
    /// Where a `Content-Disposition: attachment` response is written, and
    /// whether it is written at all (ADR-0032 D13).
    ///
    /// `None` — the default — is **deny**: the navigation is refused and
    /// recorded rather than parsed as HTML, which is what
    /// `Browser.setDownloadBehavior` already says a target with no download
    /// directory does.
    pub download_path: Option<std::path::PathBuf>,
}

/// An `<input type=file>` asking for files.
#[derive(Clone, Debug)]
pub struct FileChooserEvent {
    /// The input that was activated. A snapshot, like every other id crossing
    /// a task boundary — re-validate it at the drain.
    pub input: NodeId,
    /// The **protocol handle** for that input, minted here.
    ///
    /// It has to be minted on the page thread, at the moment the chooser opens:
    /// the handle table lives on the page, and the driver reads this off
    /// `Page.fileChooserOpened` as `backendNodeId` — Puppeteer's
    /// `#onFileChooser` immediately calls `adoptBackendNode(event.backendNodeId)`
    /// and hangs on a missing one. `None` only if the handle table is
    /// exhausted, which every other node-minting path also reports rather than
    /// inventing an id for.
    pub backend_node_id: Option<u64>,
    /// Whether the input accepts more than one file.
    pub multiple: bool,
}

/// What a page does with a `Content-Disposition: attachment` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadBehavior {
    /// Refuse the navigation. The current document stays; nothing is written.
    Deny,
    /// Write the bytes into this directory. The current document stays, which
    /// is what a browser does — a download is a navigation that never commits.
    Allow(std::path::PathBuf),
}

/// One download, as an embedder hears about it.
#[derive(Clone, Debug)]
pub struct DownloadEvent {
    /// Stable for the life of the download, so a driver can pair the begin and
    /// the completion. There is no resume, so a download has exactly two.
    pub guid: String,
    pub url: String,
    /// The name derived from `Content-Disposition`, or from the URL path.
    pub suggested_filename: String,
    /// Where it was actually written, once it has been. `None` while it is
    /// beginning, and for a refused one.
    pub path: Option<String>,
    pub total_bytes: u64,
    /// `"inProgress"`, `"completed"` or `"canceled"` — CDP's own spelling, so
    /// the protocol layer renames nothing.
    pub state: DownloadState,
}

/// Where a download got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadState {
    InProgress,
    Completed,
    Canceled,
}

impl DownloadState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DownloadState::InProgress => "inProgress",
            DownloadState::Completed => "completed",
            DownloadState::Canceled => "canceled",
        }
    }
}

/// Default wall-clock budget for a single task's stay in JavaScript.
///
/// A runaway script (`for (;;) {}`, or a frame walk that never terminates)
/// otherwise wedges the event loop forever, since the loop only regains
/// control when JS returns.
pub const DEFAULT_SCRIPT_BUDGET: Duration = Duration::from_secs(10);

/// Wall-clock budget armed around each task that enters JavaScript, enforced
/// through the engine's interrupt callback.
///
/// The budget is per task, not per page: it is armed by the outermost
/// [`Page::with_cx`] and disarmed when that call returns, so a script's own
/// microtasks run under the same deadline while the next timer callback starts
/// with a fresh one.
struct ScriptBudget {
    limit: Duration,
    /// `Some` while a task is on the JS stack.
    deadline: Cell<Option<Instant>>,
    /// Set when the interrupt callback aborted the running task.
    tripped: Cell<bool>,
}

impl ScriptBudget {
    fn new(limit: Duration) -> Self {
        Self {
            limit,
            deadline: Cell::new(None),
            tripped: Cell::new(false),
        }
    }

    /// Starts the budget unless it is disabled or a task is already on the
    /// stack. Returns whether this call owns the deadline (and so must
    /// [`Self::disarm`] it).
    fn arm(&self) -> bool {
        if self.limit == Duration::MAX || self.deadline.get().is_some() {
            return false;
        }
        self.tripped.set(false);
        self.deadline.set(Some(Instant::now() + self.limit));
        true
    }

    fn disarm(&self) {
        self.deadline.set(None);
    }

    /// The interrupt callback: polled by the engine while JS runs.
    fn expired(&self) -> bool {
        match self.deadline.get() {
            Some(deadline) if Instant::now() >= deadline => {
                self.tripped.set(true);
                true
            }
            _ => false,
        }
    }

    /// Whether the currently unwinding task was aborted by the budget.
    fn tripped(&self) -> bool {
        self.tripped.get()
    }
}

/// A scheduled timer task.
struct Timer {
    deadline: Instant,
    /// Tie-breaker preserving registration order for equal deadlines.
    seq: u64,
    id: u64,
    /// The world that scheduled it. `callback` and `args` are that world's
    /// values and can be invoked nowhere else (ADR-0033 D5), so the loop enters
    /// this world to fire them.
    world: WorldId,
    callback: JsValue,
    args: Vec<JsValue>,
    repeat: Option<Duration>,
    /// HTML "timer nesting level"; drives the 4ms clamp for nested timers.
    nesting: u32,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        (self.deadline, self.seq) == (other.deadline, other.seq)
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Timer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.deadline, self.seq).cmp(&(other.deadline, other.seq))
    }
}

/// A promise rejection with no handler *yet*.
///
/// Reporting one is a judgement call this engine cannot make perfectly. A
/// browser reports at the end of the task and retracts later with
/// `rejectionhandled`; the push sink has no retraction, so a premature report
/// is a lie a driver cannot un-hear. The two obvious rules both fail: reporting
/// at the first quiescent point blames
/// `p = Promise.reject(); setTimeout(() => p.catch(f))`, and waiting for full
/// idleness never reports anything at all on a page with a live `setInterval`.
/// Counting loop iterations does not help either — *any* wakeup ends one, so a
/// polling driver's own commands would age a rejection past its handler.
///
/// So: report when the page is idle (nothing can still attach a handler), or
/// once [`UNHANDLED_REJECTION_GRACE`] of wall clock has passed. A handler
/// attached later than that is reported as an error it briefly was.
struct PendingRejection {
    /// `JsError::rendered()`, the key the tracker retracts by.
    key: String,
    error: ScriptError,
    queued_at: Instant,
}

/// How long a rejection is given to acquire a handler on a page that never goes
/// idle, before it is reported to the [`EventSink`].
///
/// A browser reports at the end of the task that rejected — zero grace — and
/// takes it back with `rejectionhandled`. With no retraction event to offer,
/// this errs the other way, by a factor of a couple of thousand. A handler
/// attached later than this is a page keeping a rejection alive for seconds,
/// which is worth reporting either way.
const UNHANDLED_REJECTION_GRACE: Duration = Duration::from_secs(2);

/// Scheduler state shared with the bindings through [`HostHooks`].
struct LoopHooks {
    timers: RefCell<BinaryHeap<Reverse<Timer>>>,
    cleared: RefCell<HashSet<u64>>,
    next_timer_id: Cell<u64>,
    next_seq: Cell<u64>,
    /// Pending `requestAnimationFrame` callbacks in registration order, fired
    /// (and drained) at each rendering opportunity (Phase 6, ADR-0007 D8).
    /// `(id, world, callback)` — the world is where the callback must run.
    raf_callbacks: RefCell<Vec<(u64, WorldId, JsValue)>>,
    next_raf_id: Cell<u64>,
    raf_cancelled: RefCell<HashSet<u64>>,
    /// `Page.setInterceptFileChooserDialog` (ADR-0032 D12). Off by default: a
    /// page nobody is driving records no chooser.
    intercept_file_chooser: Cell<bool>,
    /// File-input activations waiting to be announced, as `(input, multiple)`.
    ///
    /// A task source rather than an immediate emit, because the announcement
    /// has to carry a protocol handle for the input and only `Page` can mint
    /// one.
    pending_file_choosers: RefCell<Vec<(NodeId, bool)>>,
    /// Nesting level of the timer whose callback is currently running (0 when
    /// no timer callback is on the stack). Timers scheduled during a callback
    /// inherit `current + 1`.
    timer_nesting: Cell<u32>,
    console: RefCell<VecDeque<ConsoleMessage>>,
    errors: RefCell<VecDeque<ScriptError>>,
    /// Rejected promises that have no handler *yet*. A rejection is only an
    /// error if nothing ever handles it, and a handler may attach long after
    /// the rejection (`p = fetch(…)` now, `p.catch(…)` next tick), so these
    /// are held here and reported only if they survive to
    /// [`Page::drain_errors`]. Keyed by `JsError::rendered()`: the
    /// engine-neutral tracker carries no promise identity, and message plus
    /// stack is exactly the discriminating power it does have. (The bare
    /// message alone would retract more aggressively than the engine
    /// rejected.)
    pending_rejections: RefCell<VecDeque<PendingRejection>>,
    /// The embedder's answer for `alert`/`confirm`/`prompt`; `None`
    /// auto-dismisses.
    dialog_handler: RefCell<Option<DialogHandler>>,
    /// Opens a sibling browsing context; `None` means there is only one.
    open_window_handler: RefCell<Option<OpenWindowHandler>>,
    /// `localStorage` per origin. Owned by the page unless a driver handed in
    /// a map shared across a whole browsing context (ADR-0027 D13).
    local_storage: RefCell<SharedLocalStorage>,
    /// Areas private to this page: `sessionStorage` for every origin, plus the
    /// "local" area of any opaque-origin document (which shares with nobody, so
    /// a context-wide entry for it would only ever leak).
    private_storage: PrivateStorageAreas,
    dialogs: RefCell<VecDeque<DialogEvent>>,
    /// Time origin for the payload timestamps the hooks stamp themselves.
    ///
    /// Seeded provisionally at construction and then **replaced with
    /// `WorldState`'s** once the bindings are installed (`Page::new`), because
    /// the hooks exist before the state does. Sharing one origin is what makes
    /// a console message, an engine warning and a dialog event comparable:
    /// two independently-seeded clocks would order a merged view wrongly.
    start: Cell<Instant>,
    time_origin_epoch_ms: Cell<f64>,
    /// The net service, installed after the hooks (which the bindings
    /// captured) are created.
    net: RefCell<Option<Rc<NetService>>>,
    /// Set for the whole of a `run_dialog` call — from before the dialog is
    /// announced until after the answer is in.
    ///
    /// An `Arc<AtomicBool>` rather than a `Cell` because the party who answers
    /// is on another thread and has to know a dialog is open *before* it can
    /// answer one. Raised before [`PageRecord::DialogOpening`] is emitted, so a
    /// driver that answers the instant it sees that event cannot lose the race.
    dialog_open: Arc<AtomicBool>,
    /// The driver's push sink (ADR-0027 D6). While installed, records go
    /// *there* instead of into the bounded pull streams above — one consumer,
    /// not two, so an event cannot be delivered twice or held back by a stream
    /// nobody is draining.
    event_sink: RefCell<Option<EventSink>>,
}

impl Default for LoopHooks {
    fn default() -> Self {
        Self {
            timers: RefCell::new(BinaryHeap::new()),
            cleared: RefCell::new(HashSet::new()),
            next_timer_id: Cell::new(1),
            next_seq: Cell::new(1),
            raf_callbacks: RefCell::new(Vec::new()),
            intercept_file_chooser: Cell::new(false),
            pending_file_choosers: RefCell::new(Vec::new()),
            next_raf_id: Cell::new(1),
            raf_cancelled: RefCell::new(HashSet::new()),
            timer_nesting: Cell::new(0),
            console: RefCell::new(VecDeque::new()),
            errors: RefCell::new(VecDeque::new()),
            pending_rejections: RefCell::new(VecDeque::new()),
            dialog_handler: RefCell::new(None),
            open_window_handler: RefCell::new(None),
            local_storage: RefCell::new(Arc::new(Mutex::new(HashMap::new()))),
            private_storage: PrivateStorageAreas::default(),
            dialogs: RefCell::new(VecDeque::new()),
            start: Cell::new(Instant::now()),
            time_origin_epoch_ms: Cell::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1000.0,
            ),
            net: RefCell::new(None),
            dialog_open: Arc::new(AtomicBool::new(false)),
            event_sink: RefCell::new(None),
        }
    }
}

impl LoopHooks {
    fn set_net(&self, net: Rc<NetService>) {
        *self.net.borrow_mut() = Some(net);
    }

    /// Delivers `record` to the driver's sink, or `false` if there is none.
    ///
    /// The sink is cloned out before the call for the reason
    /// [`HostHooks::run_dialog`] clones the dialog handler: it is embedder code
    /// running with JS on the stack, and holding the borrow across it would
    /// turn a re-entrant `set_event_sink` into a panic.
    fn emit(&self, record: PageRecord) -> bool {
        let sink = self.event_sink.borrow().clone();
        match sink {
            Some(sink) => {
                sink(record);
                true
            }
            None => false,
        }
    }

    /// The one implementation of "a record goes to the sink **or** to its
    /// bounded pull stream, never both".
    ///
    /// Callers pass the constructor rather than pattern-matching a returned
    /// record: the `if let Some(PageRecord::X(x)) = emit(PageRecord::X(x))`
    /// shape this replaces was infallible in fact but refutable to the
    /// compiler, so naming the wrong variant compiled and silently dropped the
    /// stream entry — and a stream added later could omit the else half
    /// entirely and lose events whenever no sink was installed.
    fn emit_or_push<T>(
        &self,
        stream: &RefCell<VecDeque<T>>,
        cap: usize,
        value: T,
        wrap: fn(T) -> PageRecord,
    ) {
        // The value is moved down exactly one of the two paths, so neither
        // costs a clone.
        let sink = self.event_sink.borrow().clone();
        match sink {
            Some(sink) => sink(wrap(value)),
            None => push_bounded(stream, cap, value),
        }
    }

    /// Adopts the bindings' time origin, so every payload timestamp — console
    /// lines from script, engine warnings, dialogs — comes off one clock.
    fn adopt_time_origin(&self, origin: (Instant, f64)) {
        self.start.set(origin.0);
        self.time_origin_epoch_ms.set(origin.1);
    }

    /// A monotonic Unix-epoch timestamp in milliseconds — the same clock and
    /// origin as `WorldState::epoch_now_ms`, hence as `NavigationEvent`.
    fn now_ms(&self) -> f64 {
        self.time_origin_epoch_ms.get() + self.start.get().elapsed().as_secs_f64() * 1000.0
    }

    /// Reports an engine-side failure with no JS exception behind it: a
    /// subresource that would not load, a module specifier that would not
    /// resolve.
    fn report_resource_error(&self, message: String) {
        self.report_error(ScriptError::engine(
            ScriptErrorKind::Resource,
            message,
            self.now_ms(),
        ));
    }

    /// Records an engine-originated console line (no JS arguments, no call
    /// site) — the event loop's own diagnostics.
    fn engine_console(&self, level: ConsoleLevel, message: String) {
        self.console_message(ConsoleMessage::engine(level, message, self.now_ms()));
    }

    fn next_seq(&self) -> u64 {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        seq
    }

    /// Pops the next due, non-cleared timer.
    fn pop_due(&self, now: Instant) -> Option<Timer> {
        let mut timers = self.timers.borrow_mut();
        loop {
            let due = matches!(timers.peek(), Some(Reverse(t)) if t.deadline <= now);
            if !due {
                return None;
            }
            let Reverse(timer) = timers.pop().expect("peeked a due timer");
            if self.cleared.borrow_mut().remove(&timer.id) {
                continue;
            }
            return Some(timer);
        }
    }

    /// The earliest deadline still worth waiting for.
    ///
    /// `clear_timer` only records the id, so cleared timers sit in the heap until
    /// they come due. Discard them from the top here: otherwise a cleared
    /// far-future timer (`clearTimeout(setTimeout(f, 2e9))`) reports a deadline
    /// that keeps `settle` blocked for its whole budget instead of returning at
    /// quiescence. A cleared timer *below* a live one needs no pruning — the live
    /// one already governs the deadline.
    fn next_deadline(&self) -> Option<Instant> {
        let mut timers = self.timers.borrow_mut();
        let mut cleared = self.cleared.borrow_mut();
        loop {
            let Reverse(top) = timers.peek()?;
            if !cleared.remove(&top.id) {
                return Some(top.deadline);
            }
            timers.pop();
        }
    }

    /// Whether any timer is scheduled and not cancelled.
    ///
    /// [`Self::next_deadline`]'s answer, without its side effect: that one
    /// prunes cancelled timers off the heap as it looks, which is right for the
    /// loop's own deadline computation and wrong for a public `is_idle`
    /// predicate an embedder may call from anywhere.
    fn has_live_timer(&self) -> bool {
        let timers = self.timers.borrow();
        let cleared = self.cleared.borrow();
        timers.iter().any(|Reverse(t)| !cleared.contains(&t.id))
    }

    /// True while animation-frame callbacks are pending.
    fn has_pending_raf(&self) -> bool {
        !self.raf_callbacks.borrow().is_empty()
    }

    /// Removes and returns the current animation-frame callbacks. Callbacks
    /// registered during firing accumulate in the freshly emptied list and run
    /// at the next opportunity (spec behavior).
    fn take_raf_callbacks(&self) -> Vec<(u64, WorldId, JsValue)> {
        std::mem::take(&mut self.raf_callbacks.borrow_mut())
    }

    /// Drops every JS value these hooks hold.
    ///
    /// Called from `Drop for Page` **before** any world is torn down, so each
    /// `Persistent` is released while its runtime is still alive. Timers and
    /// rAF callbacks are the only page-level JS the design permits
    /// (ADR-0033 D3/D5); if that ever stops being true, this is where the new
    /// holder must be released too.
    fn clear_js(&self) {
        self.timers.borrow_mut().clear();
        self.raf_callbacks.borrow_mut().clear();
        self.pending_rejections.borrow_mut().clear();
    }

    /// Whether animation-frame `id` was cancelled (consumes the flag).
    fn take_raf_cancelled(&self, id: u64) -> bool {
        self.raf_cancelled.borrow_mut().remove(&id)
    }

    /// Drops every scheduled callback of the previous document on navigation.
    ///
    /// The realm outlives a navigation, so a surviving `setInterval` would keep
    /// firing doc-1's function against doc-2, and a pending `requestAnimationFrame`
    /// would run at doc-2's first rendering opportunity. Ids keep counting up, so
    /// a stale `clearTimeout` from doc-1 script cannot cancel a doc-2 timer.
    fn reset_for_navigation(&self) {
        self.timers.borrow_mut().clear();
        self.cleared.borrow_mut().clear();
        self.raf_callbacks.borrow_mut().clear();
        self.raf_cancelled.borrow_mut().clear();
        self.timer_nesting.set(0);
    }
}

/// Accumulates the time a blocking wait actually spent parked, whichever way
/// [`Page::wait_for_work`] returns.
struct ParkTimer<'a> {
    stats: &'a Cell<LoopStats>,
    entered: Instant,
}

impl Drop for ParkTimer<'_> {
    fn drop(&mut self) {
        let mut stats = self.stats.get();
        stats.parked_micros = stats
            .parked_micros
            .saturating_add(self.entered.elapsed().as_micros() as u64);
        self.stats.set(stats);
    }
}

/// Drops from the front so an undrained stream cannot grow without bound; the
/// newest entries are the ones an embedder acts on.
///
/// A `VecDeque`, not a `Vec`: once a stream is at capacity every further push
/// evicts one entry, and `Vec::drain(..1)` would memmove the whole retained
/// buffer each time — O(cap) per console line, on the path a chatty page hits
/// hardest.
fn push_bounded<T>(stream: &RefCell<VecDeque<T>>, cap: usize, item: T) {
    let mut stream = stream.borrow_mut();
    while stream.len() >= cap {
        stream.pop_front();
    }
    stream.push_back(item);
}

/// Empties a bounded stream into the `Vec` the embedder gets, oldest first.
fn drain_stream<T>(stream: &RefCell<VecDeque<T>>) -> Vec<T> {
    stream.borrow_mut().drain(..).collect()
}

impl HostHooks for LoopHooks {
    fn console_message(&self, message: ConsoleMessage) {
        self.emit_or_push(
            &self.console,
            MAX_CONSOLE_MESSAGES,
            message,
            PageRecord::Console,
        );
    }

    fn report_error(&self, error: ScriptError) {
        self.emit_or_push(&self.errors, MAX_SCRIPT_ERRORS, error, PageRecord::Error);
    }

    fn run_dialog(&self, request: DialogRequest) -> DialogResponse {
        // Raise the flag *before* announcing, and announce *before* answering.
        // Both orderings are load-bearing: a handler that gets its answer from
        // another thread blocks below, so a driver told only afterwards could
        // never be the one to answer — and a driver that answers the instant it
        // sees the announcement must find the flag already up, or its answer is
        // refused for arriving too early.
        self.dialog_open
            .store(true, std::sync::atomic::Ordering::Release);
        self.emit(PageRecord::DialogOpening(request.clone()));
        // Clone the handler out and *drop the borrow* before calling it: the
        // handler is embedder code, and one that reinstalls itself would
        // otherwise panic on the borrow. (It cannot reach the page any other
        // way — see `DialogHandler`.)
        let handler = self.dialog_handler.borrow().clone();
        let response = match handler {
            Some(handler) => handler(&request),
            // The default policy, and what both drivers do with no listener
            // attached.
            None => DialogResponse::Dismiss,
        };
        let DialogRequest {
            kind,
            message,
            default_value,
            ..
        } = request;
        let event = DialogEvent {
            kind,
            message,
            default_value,
            response: response.clone(),
            timestamp: self.now_ms(),
        };
        self.dialog_open
            .store(false, std::sync::atomic::Ordering::Release);
        self.emit_or_push(&self.dialogs, MAX_DIALOG_EVENTS, event, PageRecord::Dialog);
        response
    }

    fn schedule_timer(
        &self,
        world: WorldId,
        callback: JsValue,
        args: Vec<JsValue>,
        delay_ms: f64,
        repeat: bool,
    ) -> f64 {
        let nesting = self.timer_nesting.get().saturating_add(1);
        let delay = clamp_timer_delay(delay_ms, nesting);
        let id = self.next_timer_id.get();
        self.next_timer_id.set(id + 1);
        self.timers.borrow_mut().push(Reverse(Timer {
            deadline: Instant::now() + delay,
            seq: self.next_seq(),
            id,
            world,
            callback,
            args,
            repeat: repeat.then_some(delay),
            nesting,
        }));
        id as f64
    }

    fn clear_timer(&self, id: f64) {
        // Ignore ids that were never issued (spec no-op) so a bogus
        // `clearTimeout` cannot grow the set unboundedly.
        if id >= 1.0 && id.fract() == 0.0 && (id as u64) < self.next_timer_id.get() {
            let mut cleared = self.cleared.borrow_mut();
            cleared.insert(id as u64);
            if cleared.len() > CANCELLED_SET_PRUNE_CAP {
                let live: HashSet<u64> =
                    self.timers.borrow().iter().map(|Reverse(t)| t.id).collect();
                cleared.retain(|id| live.contains(id));
            }
        }
    }

    fn request_animation_frame(&self, world: WorldId, callback: JsValue) -> f64 {
        let id = self.next_raf_id.get();
        self.next_raf_id.set(id + 1);
        self.raf_callbacks.borrow_mut().push((id, world, callback));
        id as f64
    }

    fn cancel_animation_frame(&self, id: f64) {
        // Ignore ids that were never issued (spec no-op) and prune stale ids so
        // a page that cancels never-existent/already-fired frames cannot grow
        // the set unboundedly.
        if id >= 1.0 && id.fract() == 0.0 && (id as u64) < self.next_raf_id.get() {
            let mut cancelled = self.raf_cancelled.borrow_mut();
            cancelled.insert(id as u64);
            if cancelled.len() > CANCELLED_SET_PRUNE_CAP {
                let live: HashSet<u64> = self
                    .raf_callbacks
                    .borrow()
                    .iter()
                    .map(|(id, _, _)| *id)
                    .collect();
                cancelled.retain(|id| live.contains(id));
            }
        }
    }

    fn start_fetch(&self, request: NetRequest) -> RequestId {
        let net = self.net.borrow();
        let net = net.as_ref().expect("net service installed before fetch");
        net.start_resource(request)
    }

    fn abort(&self, id: RequestId) {
        if let Some(net) = self.net.borrow().as_ref() {
            net.abort(id);
        }
    }

    fn get_cookie(&self, document_url: &str) -> String {
        let Some(net) = self.net.borrow().as_ref().map(Rc::clone) else {
            return String::new();
        };
        let Ok(url) = url::Url::parse(document_url) else {
            return String::new();
        };
        net.cookies()
            .lock()
            .map(|mut jar| jar.document_cookie(&url, std::time::SystemTime::now()))
            .unwrap_or_default()
    }

    fn storage(&self, kind: StorageAreaKind, origin: &str) -> SharedStorage {
        // The decision is "shared with the context, or private to this page?",
        // so the branch asks exactly that rather than rewriting `kind` into a
        // `Session` it is not. An opaque-origin document shares with nobody, so
        // even its *local* area is private: putting it in the context-wide map
        // would add an entry no other document can ever reach, and a driver
        // cycling pages would grow that map without bound.
        let shared_with_context = kind == StorageAreaKind::Local && !origin.starts_with("opaque:");
        if shared_with_context {
            let areas = Arc::clone(&self.local_storage.borrow());
            let mut areas = areas.lock().unwrap_or_else(|e| e.into_inner());
            let area = Arc::clone(
                areas
                    .entry(origin.to_owned())
                    .or_insert_with(StorageArea::shared),
            );
            evict_unreferenced_areas(&mut areas);
            area
        } else {
            self.private_storage.area(kind, origin)
        }
    }

    fn open_window(&self, request: OpenWindowRequest) -> Option<OpenedWindow> {
        // Clone the handler out and drop the borrow before calling it, for the
        // reason `run_dialog` does: this is embedder code with JS on the stack.
        let handler = self.open_window_handler.borrow().clone();
        handler.and_then(|handler| handler(&request))
    }

    fn set_cookie(&self, document_url: &str, cookie: &str) {
        let Some(net) = self.net.borrow().as_ref().map(Rc::clone) else {
            return;
        };
        let Ok(url) = url::Url::parse(document_url) else {
            return;
        };
        if let Ok(mut jar) = net.cookies().lock() {
            jar.set_document_cookie(&url, cookie, std::time::SystemTime::now());
        }
    }

    fn open_file_chooser(&self, input: NodeId, multiple: bool) {
        // Only when a driver asked to see it. Without interception a headless
        // page has no chooser, and inventing one — or blocking — would be the
        // fake P6 forbids (ADR-0032 D12).
        if !self.intercept_file_chooser.get() {
            return;
        }
        // Queued rather than announced: the event has to carry a *protocol
        // handle* for the input, and the handle table lives on `Page`, not
        // here. Draining it as a task source also keeps the announcement off
        // the stack of the click that caused it, which is what D12 means by
        // "does not park the page".
        self.pending_file_choosers
            .borrow_mut()
            .push((input, multiple));
    }
}

/// A script deferred until after parsing, executed in document order before
/// `DOMContentLoaded`.
enum Deferred {
    /// External classic script with `defer`.
    ClassicExternal { node: NodeId, url: String },
    /// Inline module (modules are always deferred).
    ModuleInline { source: String, url: String },
    /// External module.
    ModuleExternal { url: String },
}

/// An in-flight `async` external script, accumulating its body.
struct AsyncScript {
    node: NodeId,
    url: String,
    content_type: Option<String>,
    status: u16,
    buffer: Vec<u8>,
    /// Dynamic scripts fire element `load`/`error`; parser async scripts use
    /// the parser lifecycle and currently retain their existing behavior.
    dispatch_events: bool,
    /// Insertion sequence for dynamic scripts whose `async` was set false.
    ordered: Option<u64>,
}

/// A completed ordered dynamic script waiting for every earlier inserted
/// ordered script to finish fetching.
struct CompletedDynamicScript {
    node: NodeId,
    url: String,
    result: Result<String, String>,
}

/// An in-flight external stylesheet (`<link rel=stylesheet>`), accumulating its
/// body until complete, then parsed and added to the style engine for its node.
struct PendingSheet {
    node: NodeId,
    url: String,
    content_type: Option<String>,
    /// The `media` attribute value, parsed into the sheet's media list.
    media: Option<String>,
    /// HTTP status from the response head; a non-success status means the body
    /// is an error page, not a stylesheet, and must not be applied.
    status: u16,
    buffer: Vec<u8>,
}

/// The resource a `<link rel=stylesheet>` node has obtained (or is obtaining)
/// for its current URL. Deduplicates the load: only a change of the resolved
/// URL re-fetches, while a `media`/`disabled` change re-evaluates applicability
/// from the cached bytes. Without this, `<link media="print"
/// onload="this.media='all'">` (a common non-blocking-CSS pattern) loops
/// load → onload → attribute change → re-fetch until the request budget trips.
struct LinkSheet {
    /// The resolved URL the resource was (or is being) obtained from.
    url: String,
    /// `None` while the fetch is in flight; the response bytes and content-type
    /// once obtained, so a media re-evaluation re-parses without the network.
    loaded: Option<(Vec<u8>, Option<String>)>,
}

/// An in-flight `<img>` load (or background image), accumulating its body
/// until complete, then decoded and inserted into the layout image store
/// keyed by its absolute URL (Phase 6, WP-K).
struct PendingImage {
    url: String,
    content_type: Option<String>,
    /// HTTP status; a non-success status means a broken image.
    status: u16,
    buffer: Vec<u8>,
}

/// An in-flight `@font-face` `src:` load, accumulating its body until complete,
/// then decoded (WOFF2/WOFF → sfnt) and registered into the layout font
/// collection under `family` (Phase 7, WP-D).
struct PendingFont {
    url: String,
    family: String,
    /// The rule's width/style/weight, used when registering the decoded font.
    attrs: oxidepage_layout::WebFontAttrs,
    /// The `src:` entries declared after this one, tried in order when this
    /// source fails to download or parse (CSS Fonts §4.3).
    fallbacks: Vec<String>,
    /// HTTP status; a non-success status means the body is not the font.
    status: u16,
    buffer: Vec<u8>,
}

/// A [`CssFetcher`] for blocking `@import` resolution, backed by the net stack.
struct PageCssFetcher {
    net: Rc<NetService>,
    doc_url: String,
}

impl CssFetcher for PageCssFetcher {
    fn fetch_css(&self, url: &url::Url) -> Result<(Vec<u8>, Option<String>, url::Url), String> {
        let out = self
            .net
            .fetch_blocking(
                NetRequest::subresource(url.as_str(), self.doc_url.clone())
                    .of_type(ResourceType::Stylesheet),
            )
            .map_err(|e| e.to_string())?;
        if out.head.status >= 400 {
            return Err(format!("@import `{url}`: HTTP {}", out.head.status));
        }
        let charset = header_content_type(&out.head.headers)
            .as_deref()
            .and_then(content_type_charset);
        let final_url = url::Url::parse(&out.head.final_url).unwrap_or_else(|_| url.clone());
        Ok((out.body.to_vec(), charset, final_url))
    }
}

/// The ES module loader: resolves specifiers and loads module text over the
/// net stack (blocking the page thread; tokio workers deliver the bytes).
struct ModuleLoader {
    net: Rc<NetService>,
    /// The current document, read for the initiator/referrer of a module load.
    dom: Rc<RefCell<DomTree>>,
}

impl ModuleSource for ModuleLoader {
    fn resolve(&self, referrer: &str, specifier: &str) -> Result<String, String> {
        let base = url::Url::parse(referrer)
            .map_err(|e| format!("cannot parse module referrer `{referrer}`: {e}"))?;
        base.join(specifier)
            .map(|u| u.to_string())
            .map_err(|e| format!("cannot resolve `{specifier}` against `{referrer}`: {e}"))
    }

    fn load(&self, url: &str) -> Result<String, String> {
        // The document is the initiator/referrer, so cross-origin module
        // imports are correctly detected (CORS-checked, no wrong SameSite
        // cookies) rather than treated as self-referential same-origin loads.
        let doc_url = self.dom.borrow().document_url().to_owned();
        let out = self
            .net
            .fetch_blocking(NetRequest::module(url, doc_url))
            .map_err(|e| e.to_string())?;
        if out.head.status >= 400 {
            return Err(format!("module `{url}`: HTTP {}", out.head.status));
        }
        let ct = header_content_type(&out.head.headers);
        Ok(decode_charset(&out.body, ct.as_deref()))
    }
}

/// A page: one realm, one document, one event loop, one net service.
pub struct Page {
    // Field order = drop order, and `impl Drop for Page` is the primary
    // guarantee rather than this ordering (ADR-0033 D4). `hooks` holds timer
    // and rAF callbacks — the only page-level JS values left — so it releases
    // first; `worlds` then destroys each world's state before its realm; every
    // world's module loader holds an `Rc<NetService>`, so `net` outlives them
    // all. `shared` deliberately holds no `JsValue` at all.
    hooks: Rc<LoopHooks>,
    /// Evaluations parked on a promise that had not settled (ADR-0034 D1).
    ///
    /// **Above `worlds` on purpose**: each entry holds a `JsValue` of some
    /// world's runtime, and a `Persistent` outliving its `Runtime` aborts the
    /// process. `Drop` clears it first for the same reason.
    pending_awaits: RefCell<Vec<remote::PendingAwait>>,
    state: Rc<WorldState>,
    shared: Rc<oxidepage_bindings::PageShared>,
    worlds: Rc<worlds::WorldTable>,
    net: Rc<NetService>,
    net_rx: Receiver<NetEvent>,
    /// What a `Content-Disposition: attachment` navigation does (ADR-0032 D13).
    ///
    /// A `RefCell` because `Browser.setDownloadBehavior` changes it at runtime;
    /// the default is [`DownloadBehavior::Deny`].
    download_behavior: RefCell<DownloadBehavior>,
    in_flight: Cell<usize>,
    pending_async: RefCell<HashMap<RequestId, AsyncScript>>,
    ordered_dynamic_ready: RefCell<BTreeMap<u64, CompletedDynamicScript>>,
    next_dynamic_order: Cell<u64>,
    next_dynamic_to_run: Cell<u64>,
    /// In-flight external stylesheets, keyed by request id.
    pending_sheets: RefCell<HashMap<RequestId, PendingSheet>>,
    /// The resource obtained per `<link rel=stylesheet>` node, so a `media`/
    /// `disabled` toggle re-applies from cache instead of re-fetching (and
    /// re-firing `load`) — see [`LinkSheet`].
    link_sheets: RefCell<HashMap<NodeId, LinkSheet>>,
    /// Count of stylesheets still loading (scripts block until this is zero).
    pending_stylesheets: Cell<usize>,
    /// In-flight image loads, keyed by request id (Phase 6, WP-K).
    pending_images: RefCell<HashMap<RequestId, PendingImage>>,
    /// Absolute URLs already loaded or in flight, to deduplicate image loads.
    requested_images: RefCell<HashSet<String>>,
    /// Viewport-driven `<img>` loading (ADR-0014). A `Cell` because
    /// [`Page::load_deferred_images`] turns the page eager for full-page output.
    lazy_images: Cell<bool>,
    /// `<img>` elements whose load is deferred until they reach the viewport.
    ///
    /// Nodes, not URLs: `src` can change while an image waits, and a deferred
    /// URL must stay out of `requested_images` (whose insert is what marks a URL
    /// as handled) until the load actually starts.
    deferred_images: RefCell<HashSet<NodeId>>,
    /// `<img>` elements awaiting a load outcome, keyed by the absolute URL they
    /// asked for (HTML "update the image data": each fires `load` or `error`).
    ///
    /// Keyed by URL and not by request id because the image pipeline downstream
    /// of here is *entirely* URL-keyed: `requested_images` deduplicates, so a
    /// second `<img>` pointing at an already-requested URL never produces a
    /// request of its own, and would wait forever on a net event that belongs to
    /// someone else. A `Vec` per URL, so every one of those elements is served
    /// by the single load that does happen.
    image_waiters: RefCell<HashMap<String, Vec<NodeId>>>,
    /// Layout/style inputs at the last visibility scan — see
    /// [`Page::start_visible_image_loads`].
    last_lazy_scan: Cell<Option<LazyScanGate>>,
    /// In-flight `@font-face` loads, keyed by request id (Phase 7, WP-D).
    pending_fonts: RefCell<HashMap<RequestId, PendingFont>>,
    /// `(family, url)` pairs already loaded or in flight, to deduplicate font
    /// loads across rescans.
    requested_fonts: RefCell<HashSet<(String, String)>>,
    /// Style version at the last `@font-face` scan (Phase 7, WP-D).
    last_fontface_scan: Cell<u64>,
    /// `(dom.style_version(), style.version())` at the last `background-image`
    /// scan (Phase 6, WP-L). Both counters matter — see
    /// [`Page::start_background_image_loads`].
    last_bg_scan: Cell<(u64, u64)>,
    /// `(dom.structure_version(), dom.style_version())` at the last
    /// inline-`<svg>` rasterization scan. Both counters matter — see
    /// [`Page::rasterize_inline_svgs`].
    last_inline_svg_scan: Cell<(u64, u64)>,
    deferred: RefCell<Vec<Deferred>>,
    /// `Cell`, not `bool`, so `load_document` can take `&self` — which is what
    /// lets the event loop drive a navigation (ADR-0022).
    load_fired: Cell<bool>,
    /// Whether the `NetworkIdle` milestone has been recorded for the current
    /// document. `settle` reaches idle on *every* call — each `eval`,
    /// `dispatch_mouse`, `dispatch_key` — and a milestone is a milestone of one
    /// navigation, not of one call.
    network_idle_recorded: Cell<bool>,
    /// Set for the extent of a `document.open()` replacement's load
    /// (ADR-0034 D2), which keeps its execution contexts.
    preserving_contexts: Cell<bool>,
    /// Set for the whole of a document load or a chain of navigations.
    ///
    /// This is the single thing standing between a nested `load_document` and a
    /// `BorrowMutError`: `load_document` runs the event loop internally (script
    /// tasks, subresource waits), and a script that navigates from there would
    /// otherwise re-enter `load_document` *under* the borrows the outer one
    /// holds. Guarded, the request simply stays queued until the outer
    /// `run_navigation` loop picks it up.
    navigating: Cell<bool>,
    /// The navigation milestone stream, drained by
    /// [`Page::drain_navigation_events`].
    navigation_events: RefCell<VecDeque<NavigationEvent>>,
    /// Current viewport, retained so a navigation can rebuild the style/layout
    /// engines for the fresh document.
    viewport: Cell<Viewport>,
    /// Cached display list + paint stamp (Phase 6, ADR-0007 D6).
    render: render::RenderState,
    /// Page start, used as the `requestAnimationFrame` timestamp origin.
    /// A `Cell` for the same reason as [`Page::load_fired`].
    start_time: Cell<Instant>,
    /// Earliest time the next rendering opportunity may run (16 ms cadence).
    next_render_at: Cell<Instant>,
    /// Per-task wall-clock budget enforced through the realm's interrupt.
    script_budget: Rc<ScriptBudget>,
    /// The realm limits every world of this page is built with.
    ///
    /// Kept because an isolated world is built *later*, from a driver command,
    /// and must be held to the same caps: an embedder that set `memory_limit`
    /// to contain a hostile page would otherwise have that cap apply to the
    /// main world alone, and `MAX_WORLDS` uncapped runtimes beside it.
    realm_options: RealmOptions,
    /// The embedder's command port, installed by [`Page::run_command_loop`].
    ///
    /// `None` for a page an embedder drives directly (the CLI, every test in
    /// this crate), and then every wait point behaves exactly as it did before
    /// the port existed.
    cmd_rx: RefCell<Option<Receiver<PageJob>>>,
    /// Jobs received at a wait point the page could not safely run them at.
    /// Drained as a task source at the top of the loop (ADR-0027 D3).
    pending_jobs: RefCell<VecDeque<PageJob>>,
    /// Set by a control job asking the loop to stop.
    closing: Cell<bool>,
    /// Set while the driver is holding the page suspended: ordinary jobs
    /// accumulate in `pending_jobs` and only control jobs run (ADR-0027 D10).
    suspended: Cell<bool>,
    /// Set while an ordinary job is on the stack. A job is allowed to drive the
    /// loop (`settle`, `navigate`), and that re-enters the drain — without this
    /// the next queued job would run *underneath* the current one, breaking
    /// their FIFO order and letting it observe borrows the outer job holds.
    /// Control jobs deliberately ignore this: an answer to a dialog, or a
    /// close, has to get through a page that is busy.
    in_job: Cell<bool>,
    /// Event-loop counters (see [`LoopStats`]).
    stats: Cell<LoopStats>,
    /// Nudges the event loop from another OS thread.
    ///
    /// A task source whose producer is not this thread — today only a sibling
    /// page's `storage` write — must have a way to *wake* the loop, not just
    /// leave work behind. An idle page parks indefinitely in
    /// [`Page::wait_for_work`], so a queue push that signals nothing is a
    /// notification that arrives only if something unrelated happens to wake
    /// the page. The sender is handed to those producers; the receiver joins
    /// the `Select`.
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    /// Storage writes made by *other* pages of this browsing context, queued
    /// from their threads and dispatched as `storage` events on this one
    /// (ADR-0027 D13).
    ///
    /// A `Mutex`, not a `RefCell`: the producer is another OS thread. It is
    /// only ever locked to push or to take the whole queue, never across a
    /// dispatch, so a `storage` listener that writes cannot deadlock. Bounded
    /// by [`MAX_STORAGE_EVENTS`], dropping the oldest.
    storage_events: Arc<Mutex<VecDeque<StorageNotification>>>,
    /// This page's subscriptions, kept so they can be dropped on navigation
    /// when the document's origin — and so its areas — change.
    storage_subs: RefCell<Vec<(SharedStorage, StorageSubscriber)>>,
    /// The storage key the current handles and subscriptions were built for.
    ///
    /// Named `_cache` to keep it distinguishable from the `storage_origin()`
    /// method, which recomputes the *live* key: they are used a line apart in
    /// `rebind_storage`, both forms type-check in most positions, and swapping
    /// them yields a page that writes to one area while filtering
    /// notifications against another.
    storage_origin_cache: RefCell<String>,
    /// `backendNodeId` ↔ [`NodeId`], for drivers that name nodes over a wire
    /// (ADR-0031 D1).
    ///
    /// Lives here and not on [`WorldState`] because it holds plain data:
    /// `ObjectStore` is on `WorldState` only because its `JsValue`s are `!Send`
    /// and must drop before the realm, and that argument does not transfer.
    /// `bindings` never reads this table and should not learn about it.
    node_handles: RefCell<node_handle::NodeHandleStore>,
}

/// One execution world, as an embedder sees it (ADR-0033 D10).
///
/// Carries the monotonic `context_id`, **not** the internal `WorldId`: a
/// `WorldId` is reused when a world is rebuilt at a commit, so a stale one
/// would silently name a live world, where a stale `context_id` is a clean
/// "no such context".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldInfo {
    /// `""` for the main world; a driver's chosen name otherwise.
    pub name: String,
    pub context_id: u64,
    /// True for the page's own world (CDP's `auxData.isDefault`).
    pub is_default: bool,
}

impl Drop for Page {
    /// Releases every JS value while the runtime that owns it is still alive.
    ///
    /// This is the primary drop-order guarantee, not the field order above: a
    /// `Persistent` outliving its `Runtime` aborts the process inside
    /// `JS_FreeRuntime` on a non-empty `gc_obj_list`, and with one runtime per
    /// world the ordering is spread across `Page`, `WorldTable` and `World`
    /// (ADR-0033 D4). `dropping_a_page_with_live_worlds_is_clean` exists
    /// because the failure mode is an abort rather than a test failure.
    fn drop(&mut self) {
        // 0. Promises a deferred `awaitPromise` is parked on (ADR-0034 D1).
        //    Nobody is left to answer, but the values are `Persistent`s of
        //    worlds `teardown` is about to free.
        self.pending_awaits.borrow_mut().clear();
        // 1. Page-level JS: timer callbacks and their arguments, rAF
        //    callbacks. `LoopHooks` is the only page-level owner left, and
        //    every value it holds belongs to some world's runtime.
        self.hooks.clear_js();
        // 2. Each world's state before its realm, newest world first.
        self.worlds.teardown();
    }
}

/// Routes a realm's unhandled promise rejections into the page's error stream.
///
/// A promise that rejects with nobody listening is how a broken page fails
/// *silently*: the module evaluated, the app never mounted, and nothing reached
/// the console. Browsers surface these as `unhandledrejection`; a headless
/// engine has no console to notice them in, so they become reported errors.
///
/// Installed on **every** world (ADR-0033 D1): a job queue is per runtime, so
/// an isolated world's rejections reach no tracker but its own — and a driver's
/// injected code failing invisibly is the same bug this exists to prevent.
fn install_rejection_tracker(realm: &QuickJsRealm, hooks: &Rc<LoopHooks>) {
    let hooks = Rc::clone(hooks);
    realm.set_rejection_tracker(Some(Box::new(move |reason, is_handled| {
        let key = reason.rendered();
        if is_handled {
            // A handler attached after the fact: retract the rejection.
            let mut pending = hooks.pending_rejections.borrow_mut();
            if let Some(at) = pending.iter().position(|p| p.key == key) {
                pending.remove(at);
            }
            return;
        }
        let error =
            ScriptError::from_js(ScriptErrorKind::UnhandledRejection, &reason, hooks.now_ms());
        // Bounded like every other stream: these are held until `drain_errors`,
        // and a page that rejects in a loop must not grow them without limit.
        // Dropping the oldest costs the ability to retract it, which is the same
        // trade the other streams make.
        push_bounded(
            &hooks.pending_rejections,
            MAX_SCRIPT_ERRORS,
            PendingRejection {
                key,
                error,
                queued_at: Instant::now(),
            },
        );
    })));
}

impl Page {
    /// A page over an empty document (`about:blank`).
    pub fn new(options: PageOptions) -> Result<Self, JsError> {
        let PageOptions {
            realm: realm_options,
            url,
            policy,
            viewport,
            navigator,
            screen,
            script_budget,
            lazy_images,
            whole_document_visible,
            dialog_handler,
            open_window_handler,
            local_storage,
            net: shared_net,
            download_path,
        } = options;
        let viewport = viewport.unwrap_or_default();
        let screen = screen.unwrap_or_else(|| ScreenProfile::from_viewport(viewport));
        navigator.validate()?;
        screen.validate()?;
        let request_defaults = oxidepage_net::RequestDefaults::new(
            &navigator.user_agent,
            &navigator.accept_language(),
        )
        .map_err(|error| JsError::Engine(error.to_string()))?;
        let navigator_data = navigator.bindings_data();
        let screen_data = screen.bindings_data();

        // A world's native-stack budget is ours, not QuickJS's 1 MiB default:
        // `MAX_WORLD_DEPTH` of them must fit one page thread (ADR-0033 D2).
        let realm_options = RealmOptions {
            max_stack_size: realm_options
                .max_stack_size
                .or(Some(worlds::WORLD_STACK_BYTES)),
            ..realm_options
        };
        let realm = QuickJsEngine.new_realm(realm_options)?;
        let script_budget = Rc::new(ScriptBudget::new(
            script_budget.unwrap_or(DEFAULT_SCRIPT_BUDGET),
        ));
        {
            let budget = Rc::clone(&script_budget);
            realm.set_interrupt(Some(Box::new(move || budget.expired())));
        }
        let mut tree = DomTree::new();
        if let Some(url) = url {
            tree.set_document_url(url);
        }
        let dom = Rc::new(RefCell::new(tree));
        let hooks = Rc::new(LoopHooks::default());
        *hooks.dialog_handler.borrow_mut() = dialog_handler;
        *hooks.open_window_handler.borrow_mut() = open_window_handler;
        if let Some(local_storage) = local_storage {
            *hooks.local_storage.borrow_mut() = local_storage;
        }
        install_rejection_tracker(&realm, &hooks);
        let shared = oxidepage_bindings::install_page(
            dom,
            Rc::clone(&hooks) as Rc<dyn HostHooks>,
            viewport,
            navigator_data,
            screen_data,
        );
        let state =
            oxidepage_bindings::install_world(&realm, &shared, oxidepage_bindings::MAIN_WORLD, "")?;
        let world_table = Rc::new(worlds::WorldTable::new());
        world_table.push(oxidepage_bindings::MAIN_WORLD, "", realm, Rc::clone(&state));
        // The hop a host callback in one world uses to reach another. Weak, so
        // the `Page -> WorldTable -> realm -> WorldState -> PageShared` chain
        // stays acyclic and every runtime is freed on drop.
        shared.set_world_enter(Rc::downgrade(&world_table) as std::rc::Weak<dyn WorldEnter>);
        state.set_whole_document_visible(whole_document_visible);
        hooks.adopt_time_origin(state.time_origin());

        // A shared pool carries its own policy (its client's SSRF connector is
        // bound to it), so `options.policy` only applies to a private stack.
        let (net, net_rx) = match shared_net {
            Some(config) => NetService::with_shared(config, request_defaults),
            None => NetService::new_with_defaults(policy.unwrap_or_default(), request_defaults)
                .map_err(|e| JsError::Engine(e.to_string()))?,
        };
        let net = Rc::new(net);
        hooks.set_net(Rc::clone(&net));
        // Installed on every runtime, so `import()` from an isolated world is
        // not a silent no-op. The loader's module cache is per runtime, which
        // is the isolation this stage is for.
        for world in world_table.all() {
            world.realm().set_module_loader(Rc::new(ModuleLoader {
                net: Rc::clone(&net),
                dom: Rc::clone(&state.dom),
            }));
        }

        // Capacity 1: this is a level trigger ("there is cross-thread work"),
        // not a queue. A second nudge before the loop drains is redundant, and
        // dropping it is what keeps a chatty sibling from growing anything.
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let page = Self {
            hooks,
            pending_awaits: RefCell::new(Vec::new()),
            state,
            shared,
            worlds: world_table,
            net,
            net_rx,
            download_behavior: RefCell::new(match download_path {
                Some(path) => DownloadBehavior::Allow(path),
                None => DownloadBehavior::Deny,
            }),
            in_flight: Cell::new(0),
            pending_async: RefCell::new(HashMap::new()),
            ordered_dynamic_ready: RefCell::new(BTreeMap::new()),
            next_dynamic_order: Cell::new(0),
            next_dynamic_to_run: Cell::new(0),
            pending_sheets: RefCell::new(HashMap::new()),
            link_sheets: RefCell::new(HashMap::new()),
            pending_stylesheets: Cell::new(0),
            pending_images: RefCell::new(HashMap::new()),
            requested_images: RefCell::new(HashSet::new()),
            lazy_images: Cell::new(lazy_images),
            deferred_images: RefCell::new(HashSet::new()),
            image_waiters: RefCell::new(HashMap::new()),
            last_lazy_scan: Cell::new(None),
            pending_fonts: RefCell::new(HashMap::new()),
            requested_fonts: RefCell::new(HashSet::new()),
            last_fontface_scan: Cell::new(u64::MAX),
            last_bg_scan: Cell::new((u64::MAX, u64::MAX)),
            last_inline_svg_scan: Cell::new((u64::MAX, u64::MAX)),
            deferred: RefCell::new(Vec::new()),
            load_fired: Cell::new(false),
            network_idle_recorded: Cell::new(false),
            preserving_contexts: Cell::new(false),
            navigating: Cell::new(false),
            navigation_events: RefCell::new(VecDeque::new()),
            viewport: Cell::new(viewport),
            render: render::RenderState::default(),
            start_time: Cell::new(Instant::now()),
            next_render_at: Cell::new(Instant::now()),
            script_budget,
            realm_options,
            cmd_rx: RefCell::new(None),
            pending_jobs: RefCell::new(VecDeque::new()),
            closing: Cell::new(false),
            suspended: Cell::new(false),
            in_job: Cell::new(false),
            stats: Cell::new(LoopStats::default()),
            wake_tx,
            wake_rx,
            storage_events: Arc::new(Mutex::new(VecDeque::new())),
            storage_subs: RefCell::new(Vec::new()),
            storage_origin_cache: RefCell::new(String::new()),
            node_handles: RefCell::new(node_handle::NodeHandleStore::default()),
        };
        // The initial document's areas. `install_storage` already resolved the
        // same two areas for the wrappers; this takes the subscriptions that
        // turn a sibling's write into a `storage` event here.
        *page.storage_origin_cache.borrow_mut() = page.storage_origin();
        page.resubscribe_storage();
        Ok(page)
    }

    /// Loads an in-memory HTML document as the current document (the document
    /// URL is whatever was configured). Scripts run per the Phase 3 timing
    /// rules; external references resolve against the document URL and load
    /// over the net stack.
    ///
    /// A script in the loaded document may navigate; that navigation is chained
    /// off this call, exactly as it would be off a `navigate`.
    pub fn load_html(&self, html: &str) -> Result<(), JsError> {
        // An embedder replacing the content is a navigation in every way that
        // matters: fresh globals, fresh context ids, dead handles (ADR-0033
        // D9). Only `document.open()` keeps its `Window`, and only because
        // HTML says so.
        self.commit_replacement_document(
            html,
            WaitUntil::Load,
            /* preserve_contexts */ false,
        )?;
        self.run_chained_navigations(WaitUntil::Load);
        Ok(())
    }

    /// Replaces the document in place, keeping the URL and the history entry.
    ///
    /// The one path for "new markup, same URL": [`Page::load_html`] and the
    /// script-created parser's `document.close()` (ADR-0034 D2). It records
    /// `Started`/`Committed` like any other commit, which is what a driver
    /// waiting on `Page.frameNavigated` and `Page.lifecycleEvent { load }`
    /// keys off — `load_html` recorded neither before, so an embedder's
    /// `set_content` looked to CDP like nothing had happened at all.
    fn commit_replacement_document(
        &self,
        html: &str,
        wait_until: WaitUntil,
        preserve_contexts: bool,
    ) -> Result<(), JsError> {
        let url = self.state.dom.borrow().document_url().to_owned();
        self.record_navigation_with(NavigationEventKind::Started, &url, None, preserve_contexts);
        self.commit_history(url.clone(), HistoryTarget::Replace);
        self.record_navigation_with(
            NavigationEventKind::Committed,
            &url,
            None,
            preserve_contexts,
        );
        self.preserving_contexts.set(preserve_contexts);
        let result = self.load_document(html, wait_until);
        self.preserving_contexts.set(false);
        result
    }

    /// Navigates to `url`: fetches the document over the net stack (SSRF- and
    /// policy-checked), decodes it, and loads it. The document URL becomes
    /// the final (post-redirect) URL.
    ///
    /// A network or policy failure is an `Err` here — an embedder asked for
    /// this URL and needs to hear that it did not load. A *script*-initiated
    /// navigation that fails keeps the current document instead, which is what
    /// browsers do; see [`Page::run_navigation`].
    pub fn navigate(&self, url: &str, wait_until: WaitUntil) -> Result<(), JsError> {
        let url = self.resolve_against_document(url);
        self.run_navigation(
            PendingNavigation::Load {
                url,
                replace: false,
                body: None,
                reload: false,
                download: None,
            },
            wait_until,
            /* embedder */ true,
        )
    }

    /// Reloads the current document, bypassing the HTTP cache.
    ///
    /// The session-history entry is *replaced*, not pushed: a reload does not
    /// add to the back stack. This is CDP's `Page.reload`.
    pub fn reload(&self, wait_until: WaitUntil) -> Result<(), JsError> {
        let url = self.state.dom.borrow().document_url().to_owned();
        self.run_navigation(
            PendingNavigation::Load {
                url,
                replace: true,
                body: None,
                reload: true,
                download: None,
            },
            wait_until,
            /* embedder */ true,
        )
    }

    /// Registers a script to run at the start of every new document,
    /// returning its identifier. CDP's
    /// `Page.addScriptToEvaluateOnNewDocument`.
    pub fn add_init_script(&self, source: &str) -> u64 {
        let id = self.state.page.next_init_script.get() + 1;
        self.state.page.next_init_script.set(id);
        self.state
            .page
            .init_scripts
            .borrow_mut()
            .push(oxidepage_bindings::InitScript {
                id,
                source: source.to_owned(),
                world: None,
            });
        id
    }

    /// Registers an init script for one world, or the main world when `world`
    /// is `None` (CDP's `Page.addScriptToEvaluateOnNewDocument { worldName }`).
    ///
    /// Naming a world that does not exist creates it, because a driver may send
    /// this before any `createIsolatedWorld`.
    pub fn add_init_script_for(&self, source: &str, world: Option<&str>) -> u64 {
        if let Some(name) = world
            && !name.is_empty()
            && let Err(error) = self.create_isolated_world(name)
        {
            self.hooks
                .report_resource_error(format!("could not create world {name:?}: {error}"));
        }
        let id = self.state.page.next_init_script.get() + 1;
        self.state.page.next_init_script.set(id);
        self.state
            .page
            .init_scripts
            .borrow_mut()
            .push(oxidepage_bindings::InitScript {
                id,
                source: source.to_owned(),
                world: world.filter(|n| !n.is_empty()).map(str::to_owned),
            });
        id
    }

    /// Removes an init script. `false` if no script has that id.
    pub fn remove_init_script(&self, id: u64) -> bool {
        let mut scripts = self.state.page.init_scripts.borrow_mut();
        let before = scripts.len();
        scripts.retain(|script| script.id != id);
        scripts.len() != before
    }

    /// Runs every registered init script against the current document.
    ///
    /// A script that throws is *reported and skipped*, not propagated: one bad
    /// init script must not stop the document from loading, and it must not
    /// stop the others from running either.
    fn run_init_scripts(&self) {
        let scripts: Vec<oxidepage_bindings::InitScript> =
            self.state.page.init_scripts.borrow().clone();
        for script in scripts {
            // Routed to its own world: a `worldName` init script is the
            // driver's own code and must not run against page globals.
            let world = match script.world.as_deref() {
                None | Some("") => Some(oxidepage_bindings::MAIN_WORLD),
                Some(name) => self.world_id_by_name(name),
            };
            let Some(world) = world else {
                self.hooks.report_resource_error(format!(
                    "init script names world {:?}, which does not exist",
                    script.world.unwrap_or_default()
                ));
                continue;
            };
            if let Some(Err(error)) = self.with_cx_in(world, |cx| {
                cx.scope.eval(&script.source, "oxidepage:initScript")
            }) {
                self.report_script_error(&error);
            }
        }
    }

    /// Bytes the JavaScript heap is currently using.
    ///
    /// Straight from the engine, not an estimate: `Performance.getMetrics`
    /// reports it as `JSHeapUsedSize`, and a driver watching for a leak needs
    /// the real number or none at all.
    #[must_use]
    pub fn js_heap_used(&self) -> i64 {
        // Summed across worlds: each world is its own runtime with its own
        // heap, and a driver watching `JSHeapUsedSize` for a leak needs the
        // page total or none at all.
        self.worlds
            .all()
            .iter()
            .map(|w| w.realm().memory_used())
            .sum()
    }

    /// The retained body of a completed request, and whether it is text.
    ///
    /// `false` means the bytes are not valid text and a caller putting them in
    /// a JSON string must base64-encode first. Backs CDP's
    /// `Network.getResponseBody`.
    #[must_use]
    pub fn response_body(&self, id: oxidepage_net::RequestId) -> Option<(Vec<u8>, bool)> {
        self.net.response_body(id)
    }

    /// Cancels every navigation that has been queued and not yet performed,
    /// returning how many were dropped. CDP's `Page.stopLoading`.
    ///
    /// **What it cannot do**, and the reason is structural: a document fetch
    /// already in flight is not interruptible from here. The page thread is
    /// *inside* that fetch and services no ordinary job until it returns
    /// (ADR-0027 D3), so this command cannot even run while the load it would
    /// cancel is happening. It stops what is queued, which is what a
    /// `stopLoading` sent between navigations means.
    pub fn stop_loading(&self) -> usize {
        self.state.clear_pending_navigations()
    }

    /// The session history, as an owned snapshot.
    ///
    /// Owned rather than borrowed because the caller is on another thread:
    /// `HistoryEntry` holds a `JsValue`, which is `!Send` and must not leave
    /// the realm's thread. Only the URL crosses.
    #[must_use]
    pub fn navigation_history(&self) -> NavigationHistory {
        let history = self.state.history();
        NavigationHistory {
            current_index: history.index(),
            entries: (0..history.len())
                .filter_map(|index| history.entry(index))
                .map(|entry| HistoryEntryInfo {
                    url: entry.url.clone(),
                })
                .collect(),
        }
    }

    /// Traverses to an absolute session-history index. CDP's
    /// `Page.navigateToHistoryEntry`.
    ///
    /// Expressed as a delta internally because that is what the traversal path
    /// takes, and what decides whether the move needs a document load at all.
    /// An out-of-range index is a silent no-op, matching `history.go`.
    pub fn navigate_to_history_entry(
        &self,
        index: usize,
        wait_until: WaitUntil,
    ) -> Result<(), JsError> {
        let delta = {
            let history = self.state.history();
            if index >= history.len() {
                return Ok(());
            }
            let (Ok(target), Ok(current)) = (i64::try_from(index), i64::try_from(history.index()))
            else {
                return Ok(());
            };
            match i32::try_from(target - current) {
                Ok(delta) => delta,
                Err(_) => return Ok(()),
            }
        };
        if delta == 0 {
            return Ok(());
        }
        self.run_navigation(
            PendingNavigation::Traverse { delta },
            wait_until,
            /* embedder */ true,
        )
    }

    /// Synthesizes one trusted mouse event at viewport CSS coordinates
    /// `(x, y)`, with everything a browser produces around it: the
    /// `mouseover`/`mouseenter` chain on a move, the focus transfer and
    /// `:active` state on a press, and the activation behavior — following a
    /// link, submitting a form — on the resulting `click`.
    ///
    /// Layout is flushed first, because hit testing needs boxes. The whole
    /// sequence runs inside one JS entry, then the event loop is drained: a
    /// listener that navigates queues the navigation and the drain performs it,
    /// which is the same contract [`Page::eval`] has.
    ///
    /// This is the shape CDP's `Input.dispatchMouseEvent` maps onto one-to-one.
    pub fn dispatch_mouse(&self, input: oxidepage_bindings::MouseInput) {
        self.flush_layout();
        let result = self.with_cx(|cx| oxidepage_bindings::imp_dispatch_mouse(cx, input));
        if let Err(throw) = result {
            report_throw(&self.hooks, throw);
        }
        self.run_until_stalled();
    }

    /// Synthesizes one trusted key event at the focused element (or the body).
    ///
    /// A `keydown` that is not cancelled runs the key's default action, which
    /// is where typing actually happens: `beforeinput` → mutate the value →
    /// `input`. `change` is deliberately **not** fired here — a text control
    /// fires it on blur, and only when the value differs from the one it had
    /// when focus arrived.
    ///
    /// `Enter` submits the form, `Escape` blurs, `Tab` moves sequential focus.
    pub fn dispatch_key(&self, input: oxidepage_bindings::KeyInput<'_>) {
        self.flush_layout();
        let result = self.with_cx(|cx| oxidepage_bindings::imp_dispatch_key(cx, input));
        if let Err(throw) = result {
            report_throw(&self.hooks, throw);
        }
        self.run_until_stalled();
    }

    /// Inserts text at the caret as a single edit, with no key events — a paste
    /// or an IME commit, which is what CDP's `Input.insertText` means.
    pub fn insert_text(&self, text: &str) {
        self.flush_layout();
        let result = self.with_cx(|cx| oxidepage_bindings::imp_insert_text(cx, text));
        if let Err(throw) = result {
            report_throw(&self.hooks, throw);
        }
        self.run_until_stalled();
    }

    /// Synthesizes a wheel tick at viewport CSS coordinates `(x, y)`.
    ///
    /// The `wheel` event is cancelable and is respected: a carousel or modal
    /// that calls `preventDefault()` to trap scrolling actually traps it.
    /// Otherwise the nearest scrollable ancestor that *can* move in that
    /// direction scrolls, so a wheel over a bottomed-out inner panel scrolls
    /// the page.
    pub fn dispatch_wheel(&self, input: oxidepage_bindings::WheelInput) {
        self.flush_layout();
        let result = self.with_cx(|cx| oxidepage_bindings::imp_dispatch_wheel(cx, input));
        if let Err(throw) = result {
            report_throw(&self.hooks, throw);
        }
        self.run_until_stalled();
    }

    /// Drains the navigation milestone stream (see [`NavigationEvent`]).
    #[must_use]
    pub fn drain_navigation_events(&self) -> Vec<NavigationEvent> {
        drain_stream(&self.navigation_events)
    }

    fn record_navigation(&self, kind: NavigationEventKind, url: &str, error: Option<String>) {
        self.record_navigation_with(kind, url, error, /* contexts_preserved */ false);
    }

    fn record_navigation_with(
        &self,
        kind: NavigationEventKind,
        url: &str,
        error: Option<String>,
        contexts_preserved: bool,
    ) {
        let event = NavigationEvent {
            kind,
            url: url.to_owned(),
            error,
            timestamp: self.state.epoch_now_ms(),
            contexts_preserved,
        };
        self.hooks.emit_or_push(
            &self.navigation_events,
            MAX_NAVIGATION_EVENTS,
            event,
            PageRecord::Navigation,
        );
    }

    // === Web Storage notifications (ADR-0027 D13) ===

    /// Subscribes to this document's storage areas, so a write by a sibling
    /// page of the same context and origin becomes a `storage` event here.
    ///
    /// Re-run on every commit: the areas are keyed by origin, so a cross-origin
    /// navigation must drop the old subscriptions and take new ones — otherwise
    /// the page would keep hearing about an origin it has left.
    fn resubscribe_storage(&self) {
        self.unsubscribe_storage();
        // Any notification queued for the document that is going away: the
        // subscriptions below are the *new* origin's, so anything still in the
        // queue belongs to the area this page is leaving.
        self.storage_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let subscriber = self.state.storage_subscriber();
        let mut subs = self.storage_subs.borrow_mut();
        // `Local` only. A `sessionStorage` area is private to this page by
        // construction, so `others()` on it is permanently empty and its
        // callback can never fire — subscribing would allocate a `Weak`, clone
        // the waker, take the area's lock and retain a second `Arc`, on every
        // cross-origin navigation, for nothing.
        {
            let area = self
                .hooks
                .storage(StorageAreaKind::Local, &self.storage_origin());
            // **`Weak`, not `Arc`.** A page thread that panics never reaches
            // `unsubscribe_storage`, and the area outlives it (it belongs to
            // the browsing context). A strong reference would keep this page's
            // queue alive forever, growing on every sibling write; a weak one
            // dies with the page and the callback then reports itself dead, so
            // the area prunes the entry on the next notification.
            let queue = Arc::downgrade(&self.storage_events);
            // Waking is half the job. The queue push alone leaves the work
            // where an idle page — parked indefinitely in `wait_for_work` —
            // will never look at it.
            let waker = self.waker();
            {
                let mut guard = area.lock().unwrap_or_else(|e| e.into_inner());
                // The callback runs on the *writing* page's thread, so it only
                // parks the notification. Dispatching it here would mean
                // entering another realm from the wrong thread.
                guard.subscribe(
                    subscriber,
                    Arc::new(move |notification| {
                        let Some(queue) = queue.upgrade() else {
                            return false; // this page is gone; prune me
                        };
                        let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                        while queue.len() >= MAX_STORAGE_EVENTS {
                            queue.pop_front();
                        }
                        queue.push_back(notification);
                        drop(queue);
                        // Level trigger, capacity 1: a full channel already
                        // means "there is work", so a failed send is success.
                        let _ = waker.try_send(());
                        true
                    }),
                );
            }
            subs.push((area, subscriber));
        }
    }

    /// Drops every subscription this page holds on a shared storage area.
    fn unsubscribe_storage(&self) {
        for (area, subscriber) in self.storage_subs.borrow_mut().drain(..) {
            let mut area = area.lock().unwrap_or_else(|e| e.into_inner());
            area.unsubscribe(subscriber);
        }
    }

    /// The key this document's storage areas are looked up by.
    ///
    /// Delegates to `bindings`, which is where the rule lives: the wrapper
    /// installation resolves the same key, and a second copy of the rule here
    /// is exactly how a page ends up writing to one area and listening on
    /// another.
    fn storage_origin(&self) -> String {
        let url = self.state.dom.borrow().document_url().to_owned();
        oxidepage_bindings::storage_origin_of(&url, self.state.storage_subscriber().id())
    }

    /// Re-points this document's storage handles and subscriptions at the areas
    /// of its current origin.
    ///
    /// A no-op for a same-origin navigation, which is the common case: the
    /// areas `hooks.storage` would return are the very same `Arc`s.
    fn rebind_storage(&self) {
        let origin = self.storage_origin();
        if *self.storage_origin_cache.borrow() == origin {
            return;
        }
        *self.storage_origin_cache.borrow_mut() = origin;
        // No `with_cx`: re-pointing the handles is pure Rust, and entering the
        // realm here would run `sync_named_properties` against the document
        // this commit is replacing.
        // Every world's `Storage` handles, not just the main world's: each
        // world installed its own pair, and a handle left pointing at the old
        // origin's area would read another origin's keys.
        for world in self.worlds.all() {
            if let Some(state) = world.state() {
                oxidepage_bindings::refresh_storage(&state);
            }
        }
        self.resubscribe_storage();
    }

    /// Task source: `storage` events queued by sibling pages. Returns whether
    /// any were dispatched.
    fn drain_storage_events(&self) -> bool {
        let pending: Vec<_> = {
            let mut queue = self
                .storage_events
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if queue.is_empty() {
                return false;
            }
            queue.drain(..).collect()
        };
        // The cached key, not a fresh parse: `rebind_storage` keeps it current
        // at every commit, and recomputing it here would re-parse the document
        // URL on every drain.
        let origin = self.storage_origin_cache.borrow().clone();
        let mut dispatched = false;
        for notification in &pending {
            // Delivery is not done under the area's lock, so a subscriber list
            // is a snapshot: this page can unsubscribe and navigate to another
            // origin between the snapshot and the call, and a notification for
            // the origin it left would otherwise be dispatched at the *new*
            // document — handing script another origin's keys and values.
            if notification.origin != origin {
                continue;
            }
            dispatched = true;
            // Every world: `localStorage` is a distinct `Storage` object per
            // world over the *same* area, so a sibling's write is news to all
            // of them. The subscriber id stays page-level, so this page is
            // still never echoed its own write.
            self.for_each_world(|cx| {
                if let Err(error) = oxidepage_bindings::dispatch_storage_event(cx, notification) {
                    report_throw(&self.hooks, error);
                }
            });
        }
        // Whether anything was *dispatched*, not whether the queue was
        // non-empty: a page that has just navigated cross-origin drops a whole
        // burst of stale notifications, and reporting that as progress would
        // force a redundant pass over every other task source.
        dispatched
    }

    /// Installs (or removes) the push sink for [`PageRecord`]s (ADR-0027 D6).
    ///
    /// While a sink is installed the four pull streams stay empty: a record
    /// goes to the sink *or* to its stream, never both. `None` restores the
    /// pull-only behavior, which is what the CLI and every direct embedder use.
    ///
    /// Promise rejections are the one thing not pushed at the moment they
    /// happen — a rejection with a handler attached later is retracted, so it
    /// is not an error yet. They are forwarded by
    /// [`Page::flush_unhandled_rejections`] once the page reaches
    /// [idle](Page::is_idle), or after a grace period on a page that never
    /// does.
    pub fn set_event_sink(&self, sink: Option<EventSink>) {
        *self.hooks.event_sink.borrow_mut() = sink;
        // The net stack reports through the same bus, so its observer is
        // installed and removed with the sink.
        //
        // **Weak, and that is load-bearing.** `Page`'s field order is
        // `state, hooks, realm, net` — which is drop order — so `net` outlives
        // `realm`. A strong `Rc<PageHooks>` in the observer would keep the
        // hooks alive past the realm's teardown, and the hooks hold pending
        // timer and animation-frame callbacks, which are `JsValue`s. That is a
        // `Persistent` outliving its runtime, and QuickJS aborts the process on
        // a non-empty `gc_obj_list` in `JS_FreeRuntime`.
        let hooks = Rc::downgrade(&self.hooks);
        self.net
            .set_observer(if self.hooks.event_sink.borrow().is_some() {
                Some(Rc::new(move |event| {
                    if let Some(hooks) = hooks.upgrade() {
                        hooks.emit(PageRecord::Network(event));
                    }
                }))
            } else {
                None
            });
    }

    /// Forwards promise rejections still unhandled to the installed
    /// [`EventSink`], if any. A no-op without a sink (the pull API's
    /// `drain_errors` reports them instead).
    ///
    /// Called at every quiescent point of the loop. A rejection is reported
    /// once the page is idle, or once it has gone unhandled for
    /// [`UNHANDLED_REJECTION_GRACE`]. See [`PendingRejection`] for why neither
    /// condition alone is enough.
    fn flush_unhandled_rejections(&self) {
        if self.hooks.event_sink.borrow().is_none()
            // Cheap emptiness check *before* `is_idle`, which walks the whole
            // timer heap. A driver always installs a sink, so the guard above
            // never fires under `engine` — and a polling app with thousands of
            // live timers would otherwise pay that scan on every loop turn to
            // discover there was nothing to report.
            || self.hooks.pending_rejections.borrow().is_empty()
        {
            return;
        }
        let idle = self.is_idle();
        let ripe: Vec<ScriptError> = {
            let mut pending = self.hooks.pending_rejections.borrow_mut();
            let mut ripe = Vec::new();
            pending.retain(|entry| {
                if idle || entry.queued_at.elapsed() >= UNHANDLED_REJECTION_GRACE {
                    ripe.push(entry.error.clone());
                    false
                } else {
                    true
                }
            });
            ripe
        };
        for error in ripe {
            self.hooks.emit(PageRecord::Error(error));
        }
    }

    /// Resolves a possibly-relative URL against the current document URL.
    fn resolve_against_document(&self, url: &str) -> String {
        let base = self.state.dom.borrow().document_url().to_owned();
        url::Url::parse(&base)
            .and_then(|base| base.join(url))
            .map_or_else(|_| url.to_owned(), |u| u.to_string())
    }

    // === Navigation (ADR-0022) ===

    /// Performs `first`, then any navigation it chained off itself, up to
    /// [`MAX_CHAINED_NAVIGATIONS`].
    ///
    /// `navigating` is held for the whole chain: `load_document` runs the event
    /// loop internally, and the loop's own navigation drain must stay out of the
    /// way until this returns.
    fn run_navigation(
        &self,
        first: PendingNavigation,
        wait_until: WaitUntil,
        embedder: bool,
    ) -> Result<(), JsError> {
        let was_navigating = self.navigating.replace(true);
        let result = self.run_navigation_chain(first, wait_until, embedder);
        self.navigating.set(was_navigating);
        result
    }

    fn run_navigation_chain(
        &self,
        first: PendingNavigation,
        wait_until: WaitUntil,
        embedder: bool,
    ) -> Result<(), JsError> {
        let mut pending = Some(first);
        let mut performed = 0usize;
        while let Some(navigation) = pending.take() {
            performed += 1;
            if performed > MAX_CHAINED_NAVIGATIONS {
                let url = self.state.dom.borrow().document_url().to_owned();
                let message = format!(
                    "navigation: more than {MAX_CHAINED_NAVIGATIONS} consecutive \
                     script-driven navigations; the chain was stopped"
                );
                self.hooks
                    .engine_console(ConsoleLevel::Error, message.clone());
                self.record_navigation(NavigationEventKind::Failed, &url, Some(message));
                return Ok(());
            }
            match navigation {
                PendingNavigation::Load {
                    url,
                    replace,
                    body,
                    reload,
                    download,
                } => {
                    let target = if replace {
                        HistoryTarget::Replace
                    } else {
                        HistoryTarget::Push
                    };
                    let load = LoadKind {
                        body,
                        reload,
                        download,
                    };
                    // A fragment-only change, a form POST and a reload are three
                    // different things and only the first stays in the document.
                    // A `download` request is none of them: `<a download>` on a
                    // same-page fragment still has to fetch, because only the
                    // response has bytes to hand to the download path.
                    if load.is_plain() && self.is_same_document(&url) {
                        self.commit_same_document(&url, target, None);
                    } else {
                        self.commit_document(&url, target, load, wait_until, embedder)?;
                    }
                }
                PendingNavigation::Traverse { delta } => {
                    self.commit_traversal(delta, wait_until, embedder)?;
                }
                PendingNavigation::JavaScriptUrl { source } => {
                    self.run_javascript_url(&source, wait_until)?;
                }
                PendingNavigation::ReplaceDocument { html } => {
                    // HTML's `document.open()` keeps the `Document`, the
                    // `Window` and the environment settings object, so the
                    // realms and every handle minted against them survive
                    // (ADR-0034 D2). A driver's own `callFunctionOn` is what
                    // asked for this replacement — tearing its context down
                    // under it would fail the very call that requested it.
                    self.commit_replacement_document(
                        &html, wait_until, /* preserve_contexts */ true,
                    )?;
                }
            }
            pending = self.state.take_pending_navigation();
        }
        Ok(())
    }

    /// HTML's "navigate to a `javascript:` URL": evaluate the payload as a
    /// classic script in the current realm, and replace the document **only**
    /// when the result is a string.
    ///
    /// That conditional is the whole behavior. `javascript:void 0`,
    /// `javascript:doThing()` and every `href="javascript:..."` handler on the
    /// real web return `undefined`, and must leave the page exactly as it was —
    /// treating the navigation as unconditional would blank the document on
    /// every such link.
    /// No `embedder` flag: it exists to suppress the `Referer` of an
    /// embedder-driven request, and this path issues none — the replacement
    /// document is the script's own return value.
    fn run_javascript_url(&self, source: &str, wait_until: WaitUntil) -> Result<(), JsError> {
        let result = self.with_cx(|cx| {
            let value = cx.scope.eval(source, "oxidepage:javascript-url");
            oxidepage_bindings::microtask_checkpoint(cx);
            match value {
                // Only a string navigates; everything else is discarded.
                Ok(JsValue::String(s)) => Ok(Some(s)),
                Ok(_) => Ok(None),
                Err(e) => Err(e),
            }
        });
        self.process_finalized();
        let html = match result {
            Ok(html) => html,
            Err(error) => {
                // A throwing `javascript:` URL reports and navigates nowhere.
                self.report_script_error(&error);
                return Ok(());
            }
        };
        let Some(html) = html else {
            return Ok(());
        };
        // The replacement document keeps the current URL and replaces the
        // history entry — a `javascript:` URL is never itself a history entry.
        let url = self.state.dom.borrow().document_url().to_owned();
        self.commit_history(url, HistoryTarget::Replace);
        self.load_document(&html, wait_until)?;
        Ok(())
    }

    /// Performs whatever navigation script has queued, if any. The entry point
    /// for callers that are not themselves a navigation (`load_html`).
    fn run_chained_navigations(&self, wait_until: WaitUntil) {
        if let Some(navigation) = self.state.take_pending_navigation() {
            // A script-initiated navigation never propagates its failure: the
            // page stays on the document it has.
            let _ = self.run_navigation(navigation, wait_until, /* embedder */ false);
        }
    }

    /// HTML's fragment-navigation test: `url` differs from the current document
    /// URL only in its fragment, **and** has a fragment.
    ///
    /// The second half is not a detail. `location.href = "page.html"` from
    /// `page.html#x` has no fragment, and HTML makes that a real (re-)load, not
    /// a silent fragment removal.
    fn is_same_document(&self, url: &str) -> bool {
        let current = self.state.dom.borrow().document_url().to_owned();
        let (Ok(target), Ok(mut current)) = (url::Url::parse(url), url::Url::parse(&current))
        else {
            return false;
        };
        if target.fragment().is_none() {
            return false;
        }
        let mut target = target;
        target.set_fragment(None);
        current.set_fragment(None);
        target == current
    }

    /// A same-document navigation: the URL moves, the document does not.
    ///
    /// `popstate` carries the state of the entry a traversal landed on;
    /// `hashchange` fires whenever the fragment actually changed. Both are
    /// dispatched after the scroll, per HTML's "apply the history step".
    fn commit_same_document(
        &self,
        url: &str,
        target: HistoryTarget,
        popstate: Option<Option<String>>,
    ) {
        let previous = self.state.dom.borrow().document_url().to_owned();
        self.state.dom.borrow_mut().set_document_url(url.to_owned());
        {
            let mut history = self.state.history();
            let seq = history.document_seq();
            match target {
                HistoryTarget::Push => history.push(url.to_owned(), None, seq),
                HistoryTarget::Replace => history.replace(url.to_owned(), None, seq),
                HistoryTarget::Traverse(index) => history.set_index(index),
            }
        }
        self.record_navigation(NavigationEventKind::SameDocument, url, None);
        self.scroll_to_fragment(url);

        if let Some(state) = popstate {
            // Delivered to every world, main first: each materializes the
            // serialized state itself, so a driver's utility world sees the
            // same navigation the page does.
            self.for_each_world(|cx| {
                if let Err(e) = oxidepage_bindings::fire_pop_state(cx, state.as_deref()) {
                    report_throw(&self.hooks, e);
                }
            });
        }
        if fragment_of(&previous) != fragment_of(url) {
            // `hashchange` is a plain `Event`: `HashChangeEvent` is not
            // implemented, so `e.oldURL` is honestly `undefined` (P6) rather
            // than a value we would have to invent.
            self.with_cx(|cx| {
                if let Err(e) = oxidepage_bindings::fire_simple_event(
                    cx,
                    EventTargetKey::Window,
                    "hashchange",
                    false,
                ) {
                    report_throw(&self.hooks, e);
                }
            });
        }
    }

    /// A cross-document navigation: fetch, decode, and replace the document.
    fn commit_document(
        &self,
        url: &str,
        target: HistoryTarget,
        load: LoadKind,
        wait_until: WaitUntil,
        embedder: bool,
    ) -> Result<(), JsError> {
        let LoadKind {
            body,
            reload,
            download,
        } = load;
        self.record_navigation(NavigationEventKind::Started, url, None);
        // The referrer of the new document is the URL of the one it left. An
        // embedder-driven navigation has no predecessor, so it sends none.
        let referrer = (!embedder).then(|| self.state.dom.borrow().document_url().to_owned());
        let request = match body {
            Some(body) => NetRequest::form_navigation(
                url.to_owned(),
                body.bytes,
                body.content_type,
                referrer.clone(),
            ),
            None => NetRequest::navigation_with(url.to_owned(), referrer.clone(), reload),
        };
        // Drop the outgoing document's retained bodies *before* the new
        // document is fetched, not in `reset_for_navigation`: that runs after
        // the fetch, so clearing there would throw away the body of the very
        // document just loaded — the one a driver most wants to read back.
        self.net.clear_log();
        let outcome = match self.net.fetch_blocking(request) {
            Ok(outcome) => outcome,
            Err(error) => {
                let message = error.to_string();
                self.record_navigation(NavigationEventKind::Failed, url, Some(message.clone()));
                if embedder {
                    // `Host`, not `Engine`: this is the network layer's own
                    // text, and it surfaces as `Page.navigate.errorText`, which
                    // a driver compares against Chrome's `net::ERR_…` names.
                    return Err(JsError::Host(message));
                }
                // A failed script-initiated navigation keeps the current
                // document — the page is not blanked, it simply did not move.
                self.hooks.engine_console(
                    ConsoleLevel::Error,
                    format!("navigation to `{url}` failed: {message}"),
                );
                return Ok(());
            }
        };

        let final_url = outcome.head.final_url.clone();

        // A download is a navigation that does **not** commit (ADR-0032 D13):
        // the bytes go to disk (or nowhere) and the current document stays,
        // which is what a browser does. Checked here rather than in `net`
        // because it is a *navigation* rule — the same `Content-Disposition` on
        // a subresource means nothing.
        if let Some(reason) = self.take_download(&final_url, &outcome, download.as_deref()) {
            self.record_navigation(NavigationEventKind::Failed, url, Some(reason));
            return Ok(());
        }

        self.state.set_referrer(referrer.unwrap_or_default());
        self.state
            .dom
            .borrow_mut()
            .set_document_url(final_url.clone());
        self.commit_history(final_url.clone(), target);
        // The realm survives a navigation, so `localStorage`/`sessionStorage`
        // must be re-pointed at the *new* origin's areas — and this page must
        // re-subscribe there. Done at the commit, after the document URL is
        // final and before any script of the new document can run.
        self.rebind_storage();
        self.record_navigation(NavigationEventKind::Committed, &final_url, None);

        // Decode with full spec sniffing (BOM > HTTP charset > `<meta charset>`)
        // rather than the transport charset alone.
        let transport = header_content_type(&outcome.head.headers)
            .as_deref()
            .and_then(content_type_charset)
            .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()));
        let decoded = oxidepage_dom::decode::decode_document_bytes(&outcome.body, transport);
        self.load_document(&decoded.text, wait_until)?;
        self.scroll_to_fragment(&final_url);
        Ok(())
    }

    /// Handles a response that is a download rather than a document.
    ///
    /// `Some(reason)` means the navigation must stop — the download was taken,
    /// or refused. `None` means this is an ordinary document, so commit it.
    ///
    /// The guid pairs the `InProgress` announcement with the terminal event.
    ///
    /// `requested` is `<a download>`'s value, and its presence alone makes this
    /// a download: the attribute says so, and the response's own
    /// `Content-Disposition` — usually absent on a static file server — does not
    /// get a veto. Its *value* is the author's suggested filename, which wins
    /// over the server's, per HTML; a bare `download` carries the empty string
    /// and falls through to the ordinary naming.
    fn take_download(
        &self,
        final_url: &str,
        outcome: &FetchOutcome,
        requested: Option<&str>,
    ) -> Option<String> {
        let disposition = header_value(&outcome.head.headers, "content-disposition")
            .map(|value| oxidepage_net::parse_content_disposition(&value))
            .unwrap_or_default();
        if !disposition.attachment && requested.is_none() {
            return None;
        }
        // Sanitized like every other attacker-influenced name: the value is the
        // page's, so `download="../../.ssh/authorized_keys"` must not become a
        // path. `write_download` checks containment again before writing.
        let suggested = requested
            .and_then(oxidepage_net::sanitize_filename)
            .or(disposition.filename)
            .or_else(|| filename_from_url(final_url))
            .unwrap_or_else(|| String::from("download"));
        let guid = next_download_guid();

        // Announced before it is written, so a driver sees a download start
        // even if the write then fails.
        self.hooks.emit(PageRecord::Download(DownloadEvent {
            guid: guid.clone(),
            url: final_url.to_owned(),
            suggested_filename: suggested.clone(),
            path: None,
            total_bytes: outcome.body.len() as u64,
            state: DownloadState::InProgress,
        }));

        let (state, path, reason) = match &*self.download_behavior.borrow() {
            DownloadBehavior::Deny => (
                DownloadState::Canceled,
                None,
                format!(
                    "download of `{suggested}` refused: no download directory is set for \
                     this page (Browser.setDownloadBehavior / --download-path)"
                ),
            ),
            DownloadBehavior::Allow(directory) => {
                match write_download(directory, &suggested, &outcome.body) {
                    Ok(written) => (
                        DownloadState::Completed,
                        Some(written.to_string_lossy().into_owned()),
                        format!("download: `{suggested}` written to `{}`", written.display()),
                    ),
                    Err(error) => (
                        DownloadState::Canceled,
                        None,
                        format!("download of `{suggested}` failed: {error}"),
                    ),
                }
            }
        };
        self.hooks.emit(PageRecord::Download(DownloadEvent {
            guid,
            url: final_url.to_owned(),
            suggested_filename: suggested,
            path,
            total_bytes: outcome.body.len() as u64,
            state,
        }));
        Some(reason)
    }

    /// `history.go(delta)`. The target entry is reachable without a load iff it
    /// belongs to the document currently loaded.
    ///
    /// There is no bfcache, so leaving the current document and coming back is
    /// a **reload** — correct, just slower than a browser, and the entry is
    /// re-stamped with the freshly loaded document so a second traversal back
    /// to it stays in-document.
    fn commit_traversal(
        &self,
        delta: i32,
        wait_until: WaitUntil,
        embedder: bool,
    ) -> Result<(), JsError> {
        let Some((index, url, state, same_document)) = ({
            let history = self.state.history();
            history.target_of(delta).and_then(|index| {
                history.entry(index).map(|entry| {
                    (
                        index,
                        entry.url.clone(),
                        entry.state.clone(),
                        entry.document_seq == history.document_seq(),
                    )
                })
            })
        }) else {
            return Ok(());
        };
        if same_document {
            self.commit_same_document(&url, HistoryTarget::Traverse(index), Some(state));
            return Ok(());
        }
        // A traversal re-fetches a document, never a download: the entry it
        // returns to is one that committed.
        self.commit_document(
            &url,
            HistoryTarget::Traverse(index),
            LoadKind::plain(),
            wait_until,
            embedder,
        )
    }

    /// Records the session-history effect of a cross-document commit: a fresh
    /// document sequence, and the entry it belongs to.
    fn commit_history(&self, url: String, target: HistoryTarget) {
        let mut history = self.state.history();
        let seq = history.next_document_seq();
        match target {
            // The initial `about:blank` entry is replaced, never left behind —
            // a fresh tab that loads a page has one entry, not two.
            HistoryTarget::Push if seq > 1 => history.push(url, None, seq),
            HistoryTarget::Push | HistoryTarget::Replace => {
                history.replace(url, None, seq);
            }
            HistoryTarget::Traverse(index) => {
                history.set_index(index);
                history.restamp(index, url, seq);
            }
        }
    }

    /// HTML's **"scroll to the fragment"**: `#id`, then `<a name>`, with an
    /// empty fragment and `#top` meaning the document origin.
    ///
    /// The `scroll` event goes through the *existing* path — a `None` target
    /// pushed onto `pending_scroll_targets`, which `drain_scroll_events`
    /// dispatches as a task. Firing it here would run script under the layout
    /// borrows this function takes.
    fn scroll_to_fragment(&self, url: &str) {
        let Ok(parsed) = url::Url::parse(url) else {
            return;
        };
        let Some(fragment) = parsed.fragment() else {
            return;
        };
        let fragment = percent_decode(fragment);
        self.flush_layout();
        let changed = if fragment.is_empty() || fragment.eq_ignore_ascii_case("top") {
            self.state
                .layout
                .borrow_mut()
                .set_viewport_scroll(0.0, 0.0)
                .changed
        } else {
            let Some(node) = self.fragment_target(&fragment) else {
                return;
            };
            let layout = self.state.layout.borrow();
            let Some(rect) = layout.border_box(node) else {
                return;
            };
            // `border_box` is viewport-relative, so adding the current scroll
            // gives the document-absolute position to scroll the element to.
            let current = layout.viewport_scroll();
            let (x, y) = (current.x + rect.origin.x, current.y + rect.origin.y);
            drop(layout);
            self.state
                .layout
                .borrow_mut()
                .set_viewport_scroll(x, y)
                .changed
        };
        if changed {
            self.state.queue_viewport_scroll_event();
        }
    }

    /// The fragment's target element: `id` first, then a named `<a>`.
    fn fragment_target(&self, fragment: &str) -> Option<NodeId> {
        let dom = self.state.dom.borrow();
        if let Some(node) = dom.element_by_id(fragment) {
            return Some(node);
        }
        let document = dom.document();
        dom.inclusive_descendants(document).find(|&id| {
            dom.get(id).and_then(|n| n.as_element()).is_some_and(|el| {
                el.is_html_element()
                    && &**el.local_name() == "a"
                    && el
                        .attr(&oxidepage_dom::node::attr_name("name".into()))
                        .is_some_and(|v| v == fragment)
            })
        })
    }

    /// Tears down the previous document so a fresh parse starts clean: aborts
    /// every in-flight subresource, clears all pending/dedup bookkeeping and
    /// counters, resets the rAF time origin, and replaces the DOM tree plus the
    /// style/layout engines. A new parse would otherwise append into the old
    /// document (the sink's `get_document()` returns the existing root), and a
    /// stale completion would drive the in-flight counter negative (H-page-2).
    fn reset_document_state(&self) {
        // Abort every in-flight subresource from the previous document so a
        // late completion cannot apply to — or corrupt the counters of — the
        // next one.
        let abort_ids: Vec<RequestId> = self
            .pending_async
            .borrow_mut()
            .drain()
            .map(|(id, _)| id)
            .chain(self.pending_sheets.borrow_mut().drain().map(|(id, _)| id))
            .chain(self.pending_images.borrow_mut().drain().map(|(id, _)| id))
            .chain(self.pending_fonts.borrow_mut().drain().map(|(id, _)| id))
            // Script-initiated `fetch`/XHR of the previous document; their
            // promises must never resolve into the new one. **Every** world's,
            // not just the main one's: an isolated world's `pending_net` is
            // dropped by `release_js` at teardown without ever reaching
            // `net.abort`, so a driver's utility-world request would keep
            // downloading across the commit.
            .chain(self.reset_worlds_pending_net())
            .collect();
        for id in abort_ids {
            self.net.abort(id);
        }
        // Timers, intervals and animation frames belong to the old document too.
        self.hooks.reset_for_navigation();
        // A `document.open()` replacement keeps its execution contexts, so the
        // three things below — all of which exist to make a *new* realm — are
        // exactly what it skips (ADR-0034 D2). Everything else in this reset
        // still runs, `RenderState::reset` above most of all.
        if !self.preserving_contexts.get() {
            // Evaluations parked on a promise of the outgoing document
            // (ADR-0034 D1). **Before** the worlds are torn down: an isolated
            // world's `JsValue` would otherwise outlive its runtime, which
            // aborts the process rather than failing a test. Every world, not
            // only the ones about to be destroyed — a main-world promise
            // belongs to the dead document just as much, and nothing will ever
            // settle it.
            self.fail_pending_awaits(None, "Execution context was destroyed.");
            // Every isolated world is destroyed and rebuilt against a fresh
            // global before any init script runs (ADR-0033 D9).
            self.reset_worlds_for_navigation();
        }
        // A request the outgoing document started must not be routed into the
        // new one; the ids were aborted just above.
        self.shared.clear_net_worlds();
        self.deferred.borrow_mut().clear();
        self.ordered_dynamic_ready.borrow_mut().clear();
        self.next_dynamic_order.set(0);
        self.next_dynamic_to_run.set(0);
        self.in_flight.set(0);
        self.pending_stylesheets.set(0);
        self.link_sheets.borrow_mut().clear();
        self.requested_images.borrow_mut().clear();
        // The deferred nodes and image waiters belong to the outgoing document;
        // their ids go stale with it, and touching a stale id panics. (The pins
        // the waiters hold go with the arena.)
        self.deferred_images.borrow_mut().clear();
        self.image_waiters.borrow_mut().clear();
        self.last_lazy_scan.set(None);
        self.requested_fonts.borrow_mut().clear();
        self.last_fontface_scan.set(u64::MAX);
        self.last_bg_scan.set((u64::MAX, u64::MAX));
        self.last_inline_svg_scan.set((u64::MAX, u64::MAX));
        self.load_fired.set(false);
        self.network_idle_recorded.set(false);
        self.render.reset();
        // Every handle named a node of the outgoing document. Keeping them
        // would be harmless only by luck: the fresh arena is seeded above the
        // old generation high-water mark, so they resolve to nothing — but a
        // driver's node cache must go stale at a commit, not linger, and
        // `DOM.documentUpdated` is what tells it so.
        self.node_handles.borrow_mut().clear();
        // The rAF timestamp origin restarts at navigation.
        self.start_time.set(Instant::now());
        self.next_render_at.set(Instant::now());

        // Replace the document tree and rebuild the style/layout engines so the
        // new parse begins from an empty document. The document node is always
        // arena slot (0, gen 1), so the JS `document` wrapper keeps resolving;
        // every *other* node of the old document must go stale, which is what
        // seeding the fresh arena above the outgoing generation high-water mark
        // buys — a fresh arena would otherwise re-issue the same generations and
        // the old ids would alias unrelated new nodes rather than die.
        let prev_url = self.state.dom.borrow().document_url().to_owned();
        let generation_base = self.state.dom.borrow().next_generation_base();
        let viewport = self.viewport.get();
        {
            let mut tree = DomTree::with_generation_base(generation_base);
            tree.set_document_url(prev_url);
            *self.state.dom.borrow_mut() = tree;
        }
        // Build the layout engine *first*: `font_metrics_factory` captures the
        // font collection of the engine it is called on, so taking it from the
        // outgoing engine would leave `ex`/`ch`/`ic` and `font-size-adjust`
        // resolving against the previous document's fonts — where the new
        // document's `@font-face` faces are never registered.
        let layout = LayoutEngine::new(viewport);
        let mut style = StyleEngine::new(&self.state.dom.borrow(), viewport);
        style.set_font_metrics_provider(layout.font_metrics_factory());
        *self.state.style.borrow_mut() = style;
        *self.state.layout.borrow_mut() = layout;
        // Reset per-document timing and stamp the navigation start. v1 collapses
        // the network phases (no distinct DNS/connect/request for the injected
        // HTML); real per-phase URL-navigation timing is a refinement.
        self.state.mark_timing(TimingMilestone::NavigationStart);
    }

    /// Streaming parse + script timing + lifecycle events.
    ///
    /// Holds the `navigating` guard for its whole extent: this function runs the
    /// event loop (parser scripts, `await_subresources`, the trailing
    /// `run_until_stalled`), and a navigation started from there must not
    /// re-enter here under the borrows already held.
    fn load_document(&self, html: &str, wait_until: WaitUntil) -> Result<(), JsError> {
        let was_navigating = self.navigating.replace(true);
        let result = self.load_document_inner(html, wait_until);
        self.navigating.set(was_navigating);
        result
    }

    fn load_document_inner(&self, html: &str, wait_until: WaitUntil) -> Result<(), JsError> {
        // Fully tear down any previous document before parsing the new one.
        self.reset_document_state();
        self.state.set_parsing(true);
        // Init scripts run against the *fresh* global, before a single byte of
        // the document is parsed. That ordering is the whole contract of
        // `Page.addScriptToEvaluateOnNewDocument`: a driver installs a binding
        // or a stub here precisely so page script sees it already in place.
        self.run_init_scripts();
        // The (synchronous) response has fully arrived and parsing begins.
        self.state
            .mark_timing(TimingMilestone::ResponseEndDomLoading);
        let mut parser =
            Parser::new_document_shared(Rc::clone(&self.state.dom), ParseOptions::default());
        parser.push_input(html.into());
        loop {
            let signal = parser.run();
            // Apply stylesheet changes the parser produced (`<style>` closed,
            // `<link>` connected) before any script runs against them.
            self.drain_style_updates();
            match signal {
                ParseSignal::InputExhausted => break,
                ParseSignal::Script(node) => {
                    self.state.set_parser_script_active(true);
                    self.execute_parsed_script(node);
                    self.state.set_parser_script_active(false);
                    let written = self.state.take_parser_write();
                    if !written.is_empty() {
                        parser.push_front_input(written.into());
                    }
                    // A parser script may connect DOM-created scripts. Prepare
                    // them at the script-task boundary before parsing resumes;
                    // repeat for inline scripts that insert more scripts.
                    while self.drain_script_updates() {
                        self.drain_style_updates();
                    }
                    // Deliver any custom-element reactions the script (or the
                    // markup it wrote) queued before parsing resumes.
                    self.drain_custom_element_reactions();
                }
            }
        }
        let _parse_errors = parser.finish_shared();
        self.drain_style_updates();
        self.state.set_parsing(false);
        self.process_finalized();

        // The parser has stopped: the document is now interactive. Per HTML
        // "the end", `domInteractive` is stamped before deferred scripts run,
        // so their runtime counts toward `domContentLoadedEventStart`, not
        // parsing.
        self.state.mark_timing(TimingMilestone::DomInteractive);
        self.fire_ready_state_change();

        // Deferred classic + module scripts, in document order, before DCL.
        self.run_deferred_scripts();

        // DOMContentLoaded on the document (bubbles), then queued work.
        self.state
            .mark_timing(TimingMilestone::DomContentLoadedStart);
        self.with_cx(|cx| {
            let document = cx.state.dom.borrow().document();
            if let Err(e) = oxidepage_bindings::fire_simple_event(
                cx,
                EventTargetKey::Node(document),
                "DOMContentLoaded",
                true,
            ) {
                report_throw(&self.hooks, e);
            }
            oxidepage_bindings::microtask_checkpoint(cx);
        });
        self.state.mark_timing(TimingMilestone::DomContentLoadedEnd);
        self.record_document_milestone(NavigationEventKind::DomContentLoaded);
        self.run_until_stalled();

        if wait_until == WaitUntil::DomContentLoaded {
            return Ok(());
        }

        // Wait for async scripts + subresources, then load on the window.
        self.await_subresources(SUBRESOURCE_BUDGET);
        // The document and its subresources are complete; the load event begins.
        self.state
            .mark_timing(TimingMilestone::DomCompleteLoadStart);
        // `readyState` is already "complete" when `readystatechange` — and then
        // `load` — is dispatched, which is what a listener on either expects.
        self.fire_ready_state_change();
        self.with_cx(|cx| {
            if let Err(e) =
                oxidepage_bindings::fire_simple_event(cx, EventTargetKey::Window, "load", false)
            {
                report_throw(&self.hooks, e);
            }
            oxidepage_bindings::microtask_checkpoint(cx);
        });
        self.state.mark_timing(TimingMilestone::LoadEnd);
        self.load_fired.set(true);
        self.record_document_milestone(NavigationEventKind::Load);
        self.run_until_stalled();
        Ok(())
    }

    /// Records a lifecycle milestone of the document now loaded.
    fn record_document_milestone(&self, kind: NavigationEventKind) {
        let url = self.state.dom.borrow().document_url().to_owned();
        self.record_navigation(kind, &url, None);
    }

    /// Dispatches `readystatechange` on the document after a readiness
    /// transition. Does not bubble (HTML "update the current document
    /// readiness"); `WorldState::mark_timing` has already moved the state, so a
    /// listener reads the new value.
    fn fire_ready_state_change(&self) {
        self.with_cx(|cx| {
            let document = cx.state.dom.borrow().document();
            if let Err(e) = oxidepage_bindings::fire_simple_event(
                cx,
                EventTargetKey::Node(document),
                "readystatechange",
                false,
            ) {
                report_throw(&self.hooks, e);
            }
            oxidepage_bindings::microtask_checkpoint(cx);
        });
    }

    /// Executes a `<script>` the parser just finished, classified by
    /// `type`/`src`/`async`/`defer` (the Phase 3 "prepare a script" subset).
    fn execute_parsed_script(&self, node: NodeId) {
        // Connected parser scripts are also observed by the generic DOM
        // script-update queue. Claim the element here before classification so
        // that queued candidates can never execute it a second time.
        if !self
            .state
            .dom
            .borrow_mut()
            .mark_script_already_started(node)
        {
            return;
        }
        let (src, script_type, is_async, defer, source) = {
            let dom = self.state.dom.borrow();
            let Some(el) = dom.node(node).as_element() else {
                return;
            };
            let attr = |name: &str| {
                el.attrs()
                    .iter()
                    .find(|a| a.name.ns.is_empty() && &*a.name.local == name)
                    .map(|a| a.value.to_string())
            };
            let has = |name: &str| {
                el.attrs()
                    .iter()
                    .any(|a| a.name.ns.is_empty() && &*a.name.local == name)
            };
            (
                attr("src"),
                attr("type").map(|t| t.trim().to_ascii_lowercase()),
                has("async"),
                has("defer"),
                dom.text_content(node),
            )
        };

        let is_module = script_type.as_deref() == Some("module");
        let is_classic = is_classic_script_type(script_type.as_deref());
        if !is_module && !is_classic {
            return; // non-JS script: ignored
        }

        if is_module {
            match src {
                Some(src) => match self.resolve_url(&src) {
                    Some(url) => self
                        .deferred
                        .borrow_mut()
                        .push(Deferred::ModuleExternal { url }),
                    None => self
                        .hooks
                        .report_resource_error(format!("cannot resolve module src `{src}`")),
                },
                None => {
                    let url = self.state.dom.borrow().document_url().to_owned();
                    self.deferred
                        .borrow_mut()
                        .push(Deferred::ModuleInline { source, url });
                }
            }
            return;
        }

        // Classic script.
        let Some(src) = src else {
            // A style sheet parsed before this script is render-blocking: the
            // script must see it (CSSOM) before running.
            self.await_pending_stylesheets(SUBRESOURCE_BUDGET);
            let url = self.state.dom.borrow().document_url().to_owned();
            self.eval_classic(&source, &url, Some(node));
            return;
        };
        let Some(url) = self.resolve_url(&src) else {
            self.hooks
                .report_resource_error(format!("cannot resolve script src `{src}`"));
            return;
        };
        if is_async {
            self.start_async_script(node, url);
        } else if defer {
            self.deferred
                .borrow_mut()
                .push(Deferred::ClassicExternal { node, url });
        } else {
            // Parser-blocking: fetch synchronously and execute now.
            self.fetch_and_eval_classic(&url, Some(node));
        }
    }

    /// Starts an `async` external script fetch, tracked until its body
    /// arrives (executed on arrival during the event loop).
    fn start_async_script(&self, node: NodeId, url: String) {
        self.start_external_classic(node, url, false, None);
    }

    /// Starts one external classic-script fetch. Dynamic scripts optionally
    /// carry an insertion-order token and dispatch completion events.
    fn start_external_classic(
        &self,
        node: NodeId,
        url: String,
        dispatch_events: bool,
        ordered: Option<u64>,
    ) {
        let doc_url = self.state.dom.borrow().document_url().to_owned();
        let id = self
            .net
            .start_resource(NetRequest::subresource(&url, doc_url).of_type(ResourceType::Script));
        self.in_flight.set(self.in_flight.get() + 1);
        self.pending_async.borrow_mut().insert(
            id,
            AsyncScript {
                node,
                url,
                content_type: None,
                status: 0,
                buffer: Vec::new(),
                dispatch_events,
                ordered,
            },
        );
    }

    /// Runs deferred classic + module scripts in document order.
    fn run_deferred_scripts(&self) {
        // Deferred scripts run after parsing but must still see all stylesheets.
        self.await_pending_stylesheets(SUBRESOURCE_BUDGET);
        let deferred = std::mem::take(&mut *self.deferred.borrow_mut());
        for item in deferred {
            match item {
                Deferred::ClassicExternal { node, url } => {
                    self.fetch_and_eval_classic(&url, Some(node));
                }
                Deferred::ModuleInline { source, url } => self.eval_module(&source, &url),
                Deferred::ModuleExternal { url } => {
                    let doc_url = self.state.dom.borrow().document_url().to_owned();
                    match self.net.fetch_blocking(NetRequest::module(&url, doc_url)) {
                        Ok(out) if out.head.status < 400 => {
                            let ct = header_content_type(&out.head.headers);
                            let source = decode_charset(&out.body, ct.as_deref());
                            self.eval_module(&source, &out.head.final_url);
                        }
                        Ok(out) => self.hooks.report_resource_error(format!(
                            "module `{url}`: HTTP {}",
                            out.head.status
                        )),
                        Err(e) => self
                            .hooks
                            .report_resource_error(format!("module `{url}`: {e}")),
                    }
                }
            }
        }
    }

    /// Fetches a classic script synchronously (blocking the parse) and evaluates it.
    fn fetch_and_eval_classic(&self, url: &str, node: Option<NodeId>) {
        // Render-blocking sheets ordered before this script must load first.
        self.await_pending_stylesheets(SUBRESOURCE_BUDGET);
        let doc_url = self.state.dom.borrow().document_url().to_owned();
        match self
            .net
            .fetch_blocking(NetRequest::subresource(url, doc_url).of_type(ResourceType::Script))
        {
            Ok(out) if out.head.status < 400 => {
                let ct = header_content_type(&out.head.headers);
                let source = decode_charset(&out.body, ct.as_deref());
                let _ = self.eval_classic(&source, url, node);
            }
            Ok(out) => self
                .hooks
                .report_resource_error(format!("script `{url}`: HTTP {}", out.head.status)),
            Err(e) => self
                .hooks
                .report_resource_error(format!("script `{url}`: {e}")),
        }
    }

    fn eval_classic(&self, source: &str, url: &str, node: Option<NodeId>) -> bool {
        let previous = self.state.page.current_script.replace(node);
        self.with_cx(|cx| {
            let result = cx.scope.eval(source, url);
            // `document.currentScript` is null in promise reactions and other
            // microtasks queued by the script, so restore before the checkpoint.
            cx.state.page.current_script.set(previous);
            let succeeded = match result {
                Ok(_) => true,
                Err(error) => {
                    self.report_script_error(&error);
                    false
                }
            };
            oxidepage_bindings::microtask_checkpoint(cx);
            succeeded
        })
    }

    fn eval_module(&self, source: &str, url: &str) {
        self.with_cx(|cx| match cx.scope.eval_module(source, url) {
            Ok(promise) => {
                oxidepage_bindings::microtask_checkpoint(cx);
                if cx.scope.promise_state(&promise) == Some(PromiseState::Rejected) {
                    // Report the rejection reason, not just the fact of it: a
                    // module that throws is otherwise indistinguishable from
                    // one that failed to load.
                    match cx.scope.promise_rejection(&promise) {
                        Some(error) => self.report_script_error(&error),
                        None => self
                            .hooks
                            .report_resource_error(format!("module `{url}` evaluation rejected")),
                    }
                }
            }
            Err(error) => self.report_script_error(&error),
        });
    }

    /// Resolves `rel` against the document base URL (`<base href>`, else the
    /// document URL), `None` on failure. Every subresource — classic scripts,
    /// `<link>` stylesheets, images, `background-image`, `@font-face` — goes
    /// through here.
    fn resolve_url(&self, rel: &str) -> Option<String> {
        let base = self.state.dom.borrow().base_url();
        url::Url::parse(&base)
            .ok()?
            .join(rel)
            .ok()
            .map(|u| u.to_string())
    }

    /// Prepares connected scripts discovered by DOM mutations. The DOM owns
    /// discovery and the sticky already-started bit; Page owns timing, fetch,
    /// evaluation, and events.
    fn drain_script_updates(&self) -> bool {
        let updates = self.state.dom.borrow_mut().take_script_updates();
        if updates.is_empty() {
            return false;
        }
        for node in updates {
            self.prepare_dynamic_script(node);
        }
        true
    }

    fn prepare_dynamic_script(&self, node: NodeId) {
        let prepared = {
            let dom = self.state.dom.borrow();
            let Some(dom_node) = dom.get(node) else {
                return;
            };
            let Some(el) = dom_node.as_element() else {
                return;
            };
            if !dom_node.is_connected()
                || !el.is_html_element()
                || &*el.name.local != "script"
                || dom.script_already_started(node)
            {
                return;
            }
            let attr = |name: &str| {
                el.attrs()
                    .iter()
                    .find(|a| a.name.ns.is_empty() && &*a.name.local == name)
                    .map(|a| a.value.to_string())
            };
            let has = |name: &str| {
                el.attrs()
                    .iter()
                    .any(|a| a.name.ns.is_empty() && &*a.name.local == name)
            };
            (
                attr("src"),
                attr("type").map(|value| value.trim().to_ascii_lowercase()),
                dom.script_force_async(node) || has("async"),
                has("nomodule"),
                dom.text_content(node),
            )
        };

        // Claim before anything that can evaluate JS or begin asynchronous
        // work. Duplicate queue entries and later mutations become no-ops.
        if !self
            .state
            .dom
            .borrow_mut()
            .mark_script_already_started(node)
        {
            return;
        }

        let (src, script_type, is_async, no_module, source) = prepared;
        let is_classic = is_classic_script_type(script_type.as_deref());
        // Dynamic modules/import maps and data blocks remain outside this
        // practical layer. With module support present, nomodule classics are
        // intentionally skipped.
        if !is_classic || no_module {
            return;
        }

        let Some(src) = src else {
            if !source.is_empty() {
                let url = self.state.dom.borrow().document_url().to_owned();
                let _ = self.eval_classic(&source, &url, Some(node));
            }
            return;
        };

        let Some(url) = self.resolve_url(&src) else {
            self.hooks
                .report_resource_error(format!("cannot resolve dynamic script src `{src}`"));
            self.fire_element_event(node, "error");
            return;
        };
        let ordered = (!is_async).then(|| {
            let order = self.next_dynamic_order.get();
            self.next_dynamic_order.set(order + 1);
            order
        });
        self.start_external_classic(node, url, true, ordered);
    }

    /// Fires a simple, non-bubbling event at an element — the `load`/`error` a
    /// `<script>` or `<link>` gets when its fetch settles.
    ///
    /// The id crossed a task boundary (it was captured when the fetch started),
    /// so it is re-validated here: the element may have been removed and freed in
    /// the meantime. A *detached but live* element still gets the event, as HTML
    /// requires — only a freed one is skipped.
    fn fire_element_event(&self, node: NodeId, event_type: &str) {
        if self.state.dom.borrow().get(node).is_none() {
            return;
        }
        self.with_cx(|cx| {
            if let Err(error) = oxidepage_bindings::fire_simple_event(
                cx,
                EventTargetKey::Node(node),
                event_type,
                false,
            ) {
                report_throw(&self.hooks, error);
            }
        });
    }

    fn finish_dynamic_script(&self, completed: CompletedDynamicScript) {
        match completed.result {
            Ok(source) => {
                if self.eval_classic(&source, &completed.url, Some(completed.node)) {
                    self.fire_element_event(completed.node, "load");
                } else {
                    self.fire_element_event(completed.node, "error");
                }
            }
            Err(message) => {
                self.hooks.report_resource_error(message);
                self.fire_element_event(completed.node, "error");
            }
        }
    }

    fn drain_ordered_dynamic_scripts(&self) {
        loop {
            let next = self.next_dynamic_to_run.get();
            let completed = self.ordered_dynamic_ready.borrow_mut().remove(&next);
            let Some(completed) = completed else {
                break;
            };
            self.next_dynamic_to_run.set(next + 1);
            self.finish_dynamic_script(completed);
        }
    }

    /// Evaluates script in the page, running a microtask checkpoint after.
    pub fn eval(&self, source: &str) -> Result<JsValue, JsError> {
        let result = self.with_cx(|cx| {
            let result = cx.scope.eval(source, "oxidepage:eval");
            oxidepage_bindings::microtask_checkpoint(cx);
            result
        });
        self.process_finalized();
        result
    }

    /// Renders an eval result the way a REPL would.
    pub fn eval_to_string(&self, source: &str) -> Result<String, JsError> {
        let value = self.eval(source)?;
        self.with_cx(|cx| match &value {
            JsValue::Undefined => Ok("undefined".to_owned()),
            JsValue::Null => Ok("null".to_owned()),
            other => cx.scope.coerce_string(other),
        })
    }

    /// Runs tasks that are runnable *now* (due timers, delivered net events)
    /// to quiescence, without waiting for future deadlines.
    pub fn run_until_stalled(&self) {
        self.run_until_stalled_until(Instant::now() + SUBRESOURCE_BUDGET);
    }

    // === The embedder command port (ADR-0027 D2–D5) ===

    /// The one blocking wait: park until a net event arrives, a command
    /// arrives, or `deadline` passes. `None` parks indefinitely, so an idle
    /// page with a command port costs no CPU at all.
    ///
    /// This is the single point ADR-0004's "one blocking wait, never a
    /// busy-wait" property lives at — every waiting caller in this file goes
    /// through it, and it parks exactly once per call whether one channel is
    /// registered or two.
    ///
    /// Returns `false` when the command channel has disconnected, which is the
    /// driver dropping its handle: the caller must stop looping. Re-selecting a
    /// disconnected receiver would be the busy-wait this exists to prevent — a
    /// disconnected `Receiver` is permanently *ready* in a `Select`, so
    /// `select_deadline` would return instantly, forever.
    fn wait_for_work(&self, deadline: Option<Instant>) -> bool {
        let mut stats = self.stats.get();
        stats.blocking_waits += 1;
        self.stats.set(stats);
        let entered = Instant::now();
        let _park = ParkTimer {
            stats: &self.stats,
            entered,
        };

        // A `crossbeam_channel::Receiver` is a cheap `Arc` clone onto the same
        // channel, so cloning it out releases the `RefCell` borrow before the
        // (possibly very long) park — otherwise a control job that takes the
        // port away would hit a `BorrowMutError`.
        let cmd_rx = self.cmd_rx.borrow().clone();

        // Every arm stays registered, suspended or not (ADR-0034 D3). A
        // suspended page freezes its *own* sources — timers, rendering, queued
        // navigations, subresource loads — but keeps serving the driver, and a
        // net event is how a driver's own `fetch` interception makes progress.
        // Deregistering the net arm was what made a paused page unable to
        // finish the setup it was paused for.
        let suspended = false;
        // Cloned out for the same reason `cmd_rx` is: the borrow must be
        // released before the park. A `crossbeam` receiver clone shares the
        // channel rather than duplicating the stream, and every clone feeds the
        // one consumer path, `NetService::apply_decision`.
        let decisions = self.net.decisions();
        let mut select = crossbeam_channel::Select::new();
        let net_op = (!suspended).then(|| select.recv(&self.net_rx));
        // Always registered, port or not: a `storage` write by a sibling page
        // must wake a page an embedder is driving by hand too.
        let wake_op = select.recv(&self.wake_rx);
        let cmd_op = cmd_rx.as_ref().map(|rx| select.recv(rx));
        // A driver's decision about a paused request (ADR-0032 D3). Under the
        // same `!suspended` gate as the net arm: resolving a pause spawns or
        // synthesizes a response, and a frozen page must run neither. The
        // decision stays in the channel until it resumes.
        let decision_op = (!suspended).then(|| select.recv(&decisions));
        let op = match deadline {
            Some(deadline) => match select.select_deadline(deadline) {
                Ok(op) => op,
                // Timed out: the caller re-runs whatever is now due.
                Err(_) => return true,
            },
            None => select.select(),
        };
        let index = op.index();
        if Some(index) == net_op {
            match op.recv(&self.net_rx) {
                Ok(event) => self.dispatch_net_event(event),
                // Unreachable: `NetService` owns the sender for as long as the
                // page owns the service. Returning `false` rather than `true`
                // anyway, because a disconnected receiver is permanently ready
                // in a `Select` — treating it as a spurious wakeup would turn
                // the one park into a pegged core if that invariant ever broke.
                Err(_) => {
                    debug_assert!(false, "net channel disconnected while the page owns it");
                    return false;
                }
            }
        } else if index == wake_op {
            // A level trigger: the work itself is picked up by the task source
            // that owns it on the next pass.
            let _ = op.recv(&self.wake_rx);
        } else if Some(index) == decision_op {
            match op.recv(&decisions) {
                Ok(command) => self.net.apply_decision(command),
                // Unreachable: the service owns a sender for as long as the
                // page owns the service, which is exactly what keeps a
                // permanently-ready receiver out of this `Select`.
                Err(_) => {
                    debug_assert!(
                        false,
                        "decision channel disconnected while the page owns it"
                    );
                    return false;
                }
            }
        } else {
            let cmd_rx = cmd_rx.as_ref().expect("command op registered");
            debug_assert_eq!(Some(index), cmd_op);
            match op.recv(cmd_rx) {
                Ok(job) => self.accept_job(job),
                Err(_) => return false,
            }
        }
        true
    }

    /// A handle other threads use to wake this page's event loop.
    fn waker(&self) -> Sender<()> {
        self.wake_tx.clone()
    }

    /// Whether the loop is at a point an ordinary job may run at.
    ///
    /// The `navigating`/`parsing` pair is the same guard the queued-navigation
    /// drain uses, and for the same reason: `await_subresources` and
    /// `await_pending_stylesheets` park *inside* `load_document`, which holds
    /// borrows on the DOM and style engines and live parser handles. A job that
    /// evaluated script there would panic on those borrows.
    /// A suspended page is deliberately **not** excluded (ADR-0034 D3): the
    /// point of the pause is to let a driver configure the page before it
    /// starts, so refusing its commands defeats it.
    fn can_run_jobs(&self) -> bool {
        !self.in_job.get() && !self.navigating.get() && !self.state.parsing()
    }

    /// Runs `job` now if it is safe to, otherwise parks it for the top of the
    /// loop.
    fn accept_job(&self, job: PageJob) {
        if job.is_control() || self.can_run_jobs() {
            self.run_job(job);
        } else {
            let mut stats = self.stats.get();
            stats.jobs_deferred += 1;
            self.stats.set(stats);
            self.pending_jobs.borrow_mut().push_back(job);
        }
    }

    fn run_job(&self, job: PageJob) {
        let mut stats = self.stats.get();
        stats.jobs_run += 1;
        self.stats.set(stats);
        // A control job runs *inside* whatever is on the stack (possibly
        // another job), so it must not disturb the flag.
        if job.is_control() {
            job.run(self);
            return;
        }
        let outer = self.in_job.replace(true);
        job.run(self);
        self.in_job.set(outer);
    }

    /// Task source: the jobs a nested wait parked. Returns whether any ran.
    ///
    /// Bounded by the queue length on entry, so a job that enqueues another
    /// cannot starve the page's own task sources.
    fn drain_commands(&self) -> bool {
        // A page with no port pays exactly this one branch per loop iteration.
        if self.cmd_rx.borrow().is_none() || !self.can_run_jobs() {
            return false;
        }
        let mut budget = self.pending_jobs.borrow().len();
        let mut ran = false;
        while budget > 0 {
            // Re-checked every iteration, not once for the batch: a job may
            // suspend the page or start a navigation, and the jobs behind it
            // must then park like any other.
            if !self.can_run_jobs() {
                break;
            }
            let Some(job) = self.pending_jobs.borrow_mut().pop_front() else {
                break;
            };
            self.run_job(job);
            ran = true;
            budget -= 1;
            if self.closing.get() {
                break;
            }
        }
        ran
    }

    /// Runs the loop until `done`, or until `budget` elapses.
    ///
    /// The one place a *budgeted* wait is expressed. There used to be three
    /// copies of this loop, and they had already drifted: `settle` folded the
    /// next rendering opportunity into its deadline while the two `await_*`
    /// loops did not, so a page with a pending animation frame and no timer
    /// parked for the whole remaining budget instead of waking to service it.
    /// Worse, that shape is what let the suspend busy-wait be fixed in one
    /// waiter and survive in the others. A fifth waiter added later gets all
    /// three rules — give up on close/suspend, fold in every deadline, stop
    /// when the command port disconnects — by construction.
    fn wait_until(&self, budget: Duration, done: impl Fn(&Self) -> bool) {
        let end = Instant::now() + budget;
        loop {
            self.run_until_stalled_until(end);
            if done(self) || self.stop_waiting() {
                return;
            }
            if Instant::now() >= end {
                return;
            }
            if !self.wait_for_work(Some(self.next_wakeup().map_or(end, |at| at.min(end)))) {
                return;
            }
        }
    }

    /// The earliest moment the loop has work of its own to do: the next timer,
    /// the next rendering opportunity when an animation frame is pending, or the
    /// moment a paused request gives up waiting for a decision.
    ///
    /// The pause deadline has to be here, not only in `drain_decisions`: an
    /// asynchronously paused request is *nothing waiting on a clock*, so a page
    /// whose only outstanding work is one would park until the caller's whole
    /// budget elapsed and only then sweep. That would put the effective release
    /// at `SUBRESOURCE_BUDGET` (30 s) rather than `DEFAULT_INTERCEPT_TIMEOUT`
    /// (20 s) — losing the one property that constant exists for, which is
    /// giving up *before* the driver's own command timeout (ADR-0032 D7).
    fn next_wakeup(&self) -> Option<Instant> {
        let render_at = self
            .hooks
            .has_pending_raf()
            .then(|| self.next_render_at.get());
        [
            self.hooks.next_deadline(),
            render_at,
            self.net.next_pause_deadline(),
            // A deferred await is the same shape of work as a paused request:
            // nothing is waiting on a clock, so an otherwise idle page would
            // park indefinitely and answer only when something unrelated woke
            // it. The driver would see a command that never returns.
            self.next_await_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether a waiting loop must give up rather than park again.
    ///
    /// Two reasons, and both have to be checked *in the loop* rather than only
    /// on entry, because a `control` job delivers either of them **at the wait
    /// point itself**:
    ///
    /// - closing: a `settle` that ran its full budget after a close request
    ///   would still be executing script when the driver's bounded join gave up
    ///   and detached the thread;
    /// - suspended: the page's own work cannot progress, so a *budgeted* wait
    ///   has nothing to wait for — and worse, `next_deadline` keeps yielding
    ///   the now-past deadline of a timer a suspended page will never fire, so
    ///   each iteration would park on an elapsed instant and return instantly.
    ///   That is a pegged core for the rest of the budget: the busy-wait
    ///   ADR-0004 exists to forbid, reached through the change that was meant
    ///   to freeze the page. (`run_command_loop` is not a budgeted wait and
    ///   keeps parking for commands — see [`Page::suspend`].)
    fn stop_waiting(&self) -> bool {
        self.closing.get() || self.suspended.get()
    }

    /// Asks the loop driven by [`Page::run_command_loop`] to stop.
    ///
    /// Safe from a control job: it sets a `Cell` and nothing else.
    pub fn request_close(&self) {
        self.closing.set(true);
    }

    /// Whether a close has been requested.
    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.closing.get()
    }

    /// Freezes the page's **own** work: no timers fire, no rendering
    /// opportunity runs, no queued navigation starts, and no subresource load
    /// begins.
    ///
    /// Not script-free, and deliberately so: a net event delivered while
    /// suspended still resolves the `fetch`/XHR promise waiting on it and runs
    /// a microtask checkpoint, and a driver's evaluate runs page script by
    /// definition. The line is between the page's own scheduling and the
    /// driver's turn.
    ///
    /// The driver keeps being served (ADR-0034 D3). Embedder jobs run and net
    /// events are delivered, because that is what
    /// `Target.setAutoAttach { waitForDebuggerOnStart: true }` actually means:
    /// a page paused *for* a debugger, whose whole purpose is to be inspected
    /// and configured before it starts. Freezing the protocol too is what made
    /// this unusable — a driver sends its entire session setup
    /// (`addScriptToEvaluateOnNewDocument` among it) *before*
    /// `Runtime.runIfWaitingForDebugger`, so a page that defers those commands
    /// deadlocks its own setup until the command timeout.
    pub fn suspend(&self) {
        self.suspended.set(true);
    }

    /// Whether the page is [suspended](Page::suspend).
    ///
    /// Read by a driver to answer `waitingForDebugger` honestly rather than
    /// from a flag it merely asked for.
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        self.suspended.get()
    }

    /// Resumes a [suspended](Page::suspend) page. Jobs parked meanwhile run at
    /// the next pass of the loop, in the order they arrived.
    pub fn resume(&self) {
        self.suspended.set(false);
    }

    /// Event-loop counters since construction (see [`LoopStats`]).
    #[must_use]
    pub fn loop_stats(&self) -> LoopStats {
        self.stats.get()
    }

    /// Announces the file-input activations queued since the last pass
    /// (ADR-0032 D12).
    ///
    /// The handle is minted **here**, not at the click: the table lives on this
    /// type, and a driver reads the value straight back as `backendNodeId` to
    /// adopt the element. The node id is re-validated first — it is a snapshot
    /// taken on another turn of the loop, and a `click()` followed by a
    /// navigation can retire it before this runs.
    fn drain_file_choosers(&self) -> bool {
        let queued = std::mem::take(&mut *self.hooks.pending_file_choosers.borrow_mut());
        if queued.is_empty() {
            return false;
        }
        for (input, multiple) in queued {
            if self.state.dom.borrow().get(input).is_none() {
                continue;
            }
            self.hooks.emit(PageRecord::FileChooser(FileChooserEvent {
                input,
                backend_node_id: self.node_handle(input).ok(),
                multiple,
            }));
        }
        true
    }

    /// True when the loop has nothing left to do: no timer, no pending
    /// animation frame, no in-flight subresource and no in-flight
    /// `fetch`/XHR. Exactly the condition [`Page::settle`] returns on.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        !self.hooks.has_live_timer()
            && !self.hooks.has_pending_raf()
            && self.in_flight.get() == 0
            && self.state.net_pending() == 0
    }

    /// Drives the page from `rx` until the channel closes or a control job
    /// calls [`Page::request_close`] — the whole life of a page thread.
    ///
    /// The loop lives here rather than in the driver because only this crate
    /// can install the receiver into [`Page::wait_for_work`], which is what
    /// keeps a command and a net event sharing *one* park. What a command
    /// *means* stays with the driver: a job is an opaque closure, so this crate
    /// learns nothing about browsers or protocols.
    pub fn run_command_loop(&self, rx: Receiver<PageJob>) {
        *self.cmd_rx.borrow_mut() = Some(rx);
        while !self.closing.get() {
            self.run_until_stalled_until(Instant::now() + SUBRESOURCE_BUDGET);
            if self.closing.get() {
                break;
            }
            self.flush_unhandled_rejections();
            // Park until something happens. With no timer, no pending frame and
            // no network, `None` is an indefinite park — the page sleeps until
            // the driver speaks.
            // A suspended page fires nothing, so waking at a timer deadline it
            // will not service would spin the loop at the timer's rate — the
            // busy-wait ADR-0004 exists to prevent, reached through the change
            // that was meant to freeze the page. It waits for a command.
            let deadline = (!self.suspended.get())
                .then(|| self.next_wakeup())
                .flatten();
            if !self.wait_for_work(deadline) {
                break;
            }
        }
        // Answer whatever is still parked on a promise before the port goes
        // (ADR-0034 D1). The thread is ending, so nothing will settle these —
        // and a driver told nothing waits out its own command timeout instead.
        self.fail_pending_awaits(None, "Target closed");
        // Drop the port so a late `send` fails fast rather than queueing into a
        // channel nobody will read.
        self.cmd_rx.borrow_mut().take();
        // Leave the shared storage areas' notify lists. They belong to the
        // browsing context, not to this page, so a browser that opens and
        // closes pages would otherwise accumulate one dead subscriber per page
        // for the life of the context.
        self.unsubscribe_storage();
    }

    /// [`Self::run_until_stalled`] bounded by a wall-clock `deadline`. The
    /// nesting clamp already keeps a zero-delay self-reposting timer from
    /// spinning; this deadline is a hard backstop so no now-runnable workload
    /// (timers, floods of net events) can keep the loop from returning.
    fn run_until_stalled_until(&self, deadline: Instant) {
        loop {
            let mut stats = self.stats.get();
            stats.turns += 1;
            self.stats.set(stats);
            self.process_finalized();
            // A suspended page runs nothing of its own. GC finalization above
            // is bookkeeping, not page work, and must keep running or the
            // wrapper cache would grow for as long as the page is held.
            // A suspended page runs none of its **own** sources, but it still
            // serves the driver (ADR-0034 D3) — so the command drain below
            // runs and everything after it does not.
            if self.suspended.get() {
                let ran = self.drain_commands();
                self.process_finalized();
                if !ran || self.closing.get() || Instant::now() >= deadline {
                    return;
                }
                continue;
            }
            // A script-created parser the task opened and wrote to but never
            // closed (ADR-0034 D2). `document.open(); document.write(…)` with
            // no `close()` is the legacy idiom, and a browser shows that
            // content; queueing the replacement here is what makes it appear.
            // Immediately before the navigation drain, so it commits on this
            // pass rather than the next.
            self.flush_unclosed_script_parser();
            // A navigation queued by script (ADR-0022). It comes first because
            // it invalidates everything below it: the document, and with it
            // every queued update keyed on a node of the outgoing tree.
            //
            // The `navigating` guard keeps `load_document`'s own internal loop
            // runs from starting a second load under the first; the `parsing`
            // guard keeps a parser-inserted script from pulling the tree out
            // from under the parser holding handles into it. In both cases the
            // request simply stays queued for the outer driver.
            if !self.navigating.get()
                && !self.state.parsing()
                && let Some(navigation) = self.state.take_pending_navigation()
            {
                let _ = self.run_navigation(navigation, WaitUntil::Load, /* embedder */ false);
                // The deadline is checked here too, not only at the bottom of
                // the loop: `MAX_CHAINED_NAVIGATIONS` bounds one *chain*, and a
                // fresh chain starts with its counter at zero every time this
                // branch runs. Two documents whose `load` handlers navigate to
                // each other would otherwise loop forever, and `settle(budget)`
                // would never return however small the budget.
                if Instant::now() >= deadline {
                    break;
                }
                // The document is gone; restart the drain order against the
                // new one rather than finishing this pass over the old.
                continue;
            }
            // The embedder's task source. After the navigation drain, because a
            // queued navigation invalidates every node id a job might be
            // holding; before the page's own sources, because a job is the
            // driver's turn and must not be starved by page work.
            if self.drain_commands() {
                if self.closing.get() || Instant::now() >= deadline {
                    break;
                }
                // A job may have navigated or run script; restart the drain
                // order rather than finishing this pass against a stale view.
                continue;
            }
            // Tasks (timers, event handlers) may have inserted/removed
            // `<style>`/`<link>` elements; apply those to the style engine.
            self.drain_style_updates();
            // DOM-created classic scripts are prepared after the mutation task
            // returns, before other newly relevant subresources.
            let mut progressed = self.drain_script_updates();
            // Deliver queued custom-element reactions (may run author JS that
            // mutates the DOM, so it counts as progress).
            progressed |= self.drain_custom_element_reactions();
            // Promise jobs of **every** world. Job queues are per runtime
            // (ADR-0033 D1), so a promise created in a utility world — which is
            // where a driver's whole injected surface lives — would otherwise
            // never settle: the main world's checkpoint cannot see its queue.
            progressed |= self.pump_non_main_jobs();
            // A mutation queues the compound microtask on the queue of the
            // world that *made* it, so every other world's observers would
            // never hear about it. Delivering per world at the task boundary is
            // what makes an injected `MutationObserver` work at all — which is
            // what a driver's waiting is built on.
            progressed |= self.deliver_mutation_records();
            // Newly connected `<img>` elements (or `src` changes) start loads.
            self.drain_image_updates();
            // Deferred `<img>` elements that have reached the viewport (lazy mode).
            self.start_visible_image_loads();
            // Elements with `background-image: url(...)` start loads too.
            self.start_background_image_loads();
            // Inline `<svg>` elements (logos, icons) rasterize locally.
            self.rasterize_inline_svgs();
            // `@font-face` rules fetch their `src:` fonts.
            self.start_font_face_loads();
            // Resolve any `document.fonts.ready` promises left pending by an
            // earlier read, now that this tick's font-face scan ran (a no-op
            // unless something is actually waiting).
            self.settle_font_ready();
            // A sibling page of this browsing context wrote to a shared storage
            // area; deliver the `storage` events it owes this document.
            progressed |= self.drain_storage_events();
            // Script scrolls queue `scroll` events; dispatch them as tasks.
            progressed |= self.drain_scroll_events();
            // Payloads page script handed to an `add_binding` function.
            progressed |= self.drain_binding_events();
            // Evaluations parked on a promise (ADR-0034 D1). **After** the
            // binding drain: `exposeBinding` is the shape this exists for, and
            // the driver cannot resolve the promise before it has been told the
            // binding was called.
            progressed |= self.drain_pending_awaits();
            // File-input activations, announced with a protocol handle for the
            // input (ADR-0032 D12).
            progressed |= self.drain_file_choosers();
            // Deliver ResizeObserver/IntersectionObserver notifications. Driven
            // here (not in `update_the_rendering`, which only runs with a
            // pending rAF, nor in every microtask checkpoint) so the initial
            // delivery lands before `settle()` returns and RO-mutation chains
            // converge across loop iterations (ADR-0011).
            progressed |= self.deliver_observations();
            // A driver's decisions about paused requests, immediately before
            // the net events they produce (ADR-0032 D3). After the suspended
            // early-out above, because resolving a pause runs page work.
            progressed |= self.net.drain_decisions();
            while let Ok(event) = self.net_rx.try_recv() {
                self.dispatch_net_event(event);
                progressed = true;
            }
            if let Some(timer) = self.hooks.pop_due(Instant::now()) {
                self.fire_timer(timer);
                progressed = true;
            }
            // A rendering opportunity: fire animation-frame callbacks and
            // update the rendering when the 16 ms cadence is due (D8).
            if self.hooks.has_pending_raf() && Instant::now() >= self.next_render_at.get() {
                self.update_the_rendering();
                self.next_render_at
                    .set(Instant::now() + Duration::from_millis(16));
                progressed = true;
            }
            if !progressed || Instant::now() >= deadline {
                break;
            }
        }
        self.process_finalized();
    }

    /// Runs the loop until nothing is left to do — waiting for future timers
    /// and in-flight network — or `budget` elapses.
    ///
    /// [`Page::wait_for_work`] is the crux: one blocking wait that wakes on a
    /// net event OR an embedder command OR the next timer deadline OR the
    /// budget end.
    pub fn settle(&self, budget: Duration) {
        self.wait_until(budget, Self::is_idle);
        // Idle reached rather than the budget running out — the distinction a
        // `NetworkIdle` milestone is there to record. Once per document: every
        // `eval`/`dispatch_*` ends in a settle that reaches idle, and one
        // milestone per call is noise the stream would never stop growing by.
        if self.is_idle() && !self.network_idle_recorded.replace(true) {
            self.record_document_milestone(NavigationEventKind::NetworkIdle);
        }
    }

    /// Blocks (bounded by `budget`) until no network request is in flight.
    fn await_subresources(&self, budget: Duration) {
        self.wait_until(budget, |page| page.in_flight.get() == 0);
    }

    /// Routes a net event to its handler: page-level async scripts stay here;
    /// everything else (fetch/XHR) is delivered to the bindings.
    fn dispatch_net_event(&self, event: NetEvent) {
        let id = event.request_id();
        // Retention and observation first, before routing consumes the event.
        // This is the one point every asynchronous response passes through;
        // the synchronous path records itself inside `fetch_blocking`.
        if !self.net.note_event(&event) {
            // An auth challenge the driver said it would answer: the service
            // re-parks the request under the same id and nothing downstream
            // ever learns the response arrived (ADR-0032 D8).
            self.net.begin_auth_pause(event);
            return;
        }
        if self.pending_async.borrow().contains_key(&id) {
            self.handle_async_script_event(id, event);
            return;
        }
        if self.pending_sheets.borrow().contains_key(&id) {
            self.handle_stylesheet_event(id, event);
            return;
        }
        if self.pending_images.borrow().contains_key(&id) {
            self.handle_image_event(id, event);
            return;
        }
        if self.pending_fonts.borrow().contains_key(&id) {
            self.handle_font_event(id, event);
            return;
        }
        // Release the net service's cancel-flag entry once a fetch/XHR request
        // reaches a terminal event (otherwise the bookkeeping grows unbounded).
        let terminal = matches!(event, NetEvent::Done { .. } | NetEvent::Error { .. });
        // Delivered to the world that started it: the `fetch` promise and the
        // `XMLHttpRequest` wrapper are that world's values, and settling them
        // from another is impossible (ADR-0033 D5). A request whose world is
        // gone — torn down by a commit — falls back to the main world, where
        // `deliver_net_event` finds no pending entry and drops it.
        let world = self
            .shared
            .net_world_of(id)
            .unwrap_or(oxidepage_bindings::MAIN_WORLD);
        self.with_cx_in(world, |cx| oxidepage_bindings::deliver_net_event(cx, event));
        if terminal {
            self.shared.forget_net_world(id);
            self.net.finish(id);
        }
    }

    /// Delivers any queued custom-element reactions (constructor upgrades and
    /// lifecycle callbacks) and runs a microtask checkpoint. During parsing,
    /// elements are created/connected between scripts without a checkpoint, so
    /// this must be pumped explicitly. Returns whether any reaction ran.
    fn drain_custom_element_reactions(&self) -> bool {
        let reacted = self.with_cx(|cx| {
            let reacted = oxidepage_bindings::drain_custom_element_reactions(cx);
            // Apply connect/disconnect wrapper-retention changes in the same
            // step: adds preserve expando state, drops let detached trees free.
            oxidepage_bindings::drain_pinned_connectivity(cx);
            oxidepage_bindings::microtask_checkpoint(cx);
            reacted
        });
        // Every other world's retention too (ADR-0033 D7). An isolated world
        // otherwise drains only from its own host calls, so an idle one — which
        // is what a driver's utility world is between commands — never advances
        // its cursor, and `PageShared::connectivity` is trimmed below the
        // *minimum* live cursor: the log then grows by one entry per pinned-node
        // connect/disconnect for the whole life of the document.
        for world in self.worlds.all() {
            if world.id == oxidepage_bindings::MAIN_WORLD {
                continue;
            }
            self.with_cx_in(world.id, oxidepage_bindings::drain_pinned_connectivity);
        }
        reacted
    }

    // === Images (Phase 6, WP-K) ===

    /// Starts loads for every `<img>` queued by the DOM (connect / `src`
    /// change). Loads are deduplicated by absolute URL and their `in_flight`
    /// count makes `load`/`settle` wait for them.
    fn drain_image_updates(&self) {
        let updates = self.state.dom.borrow_mut().take_image_updates();
        for node in updates {
            // `start_image_load` takes its own pin for the in-flight window, so
            // the queue's pin (`push_image_update`) is released after it, never
            // before: the settled path fires an event, and a GC inside that
            // callback would find the node unpinned.
            self.start_image_load(node);
            self.release_image_pin(node);
        }
    }

    /// Releases one image-related pin and retries the collection the wrapper
    /// finalizer may already have been refused. Without the retry a node whose
    /// wrapper was GC'd while a load was queued or in flight would never be
    /// freed at all — this pin is the last one, and nothing else comes back.
    fn release_image_pin(&self, node: NodeId) {
        let mut dom = self.state.dom.borrow_mut();
        dom.unpin(node);
        if !self.state.parsing() && !dom.observers().has_pending_records() {
            dom.free_detached_tree_if_unpinned(node);
        }
    }

    /// Resolves `node`'s `src`, deduplicates, and either decodes a `data:` URL
    /// inline or starts a network load.
    ///
    /// In lazy mode the load is deferred to [`Self::start_visible_image_loads`]
    /// instead — but *before* [`Self::start_image_load_url`], never inside it:
    /// that method's first act is to insert the URL into `requested_images`, and
    /// a URL sitting in that set is one nothing will ever fetch again.
    fn start_image_load(&self, node: NodeId) {
        let (src, loading, detached) = {
            let dom = self.state.dom.borrow();
            // The queued id is a snapshot taken before this drain: replacing a
            // subtree (`innerHTML`) frees the `<img>` outright, so the id can be
            // stale, not merely disconnected. Disconnected is fine and expected
            // — `new Image()` never enters the tree — but the node must still
            // belong to the rendered document and be outside any `<template>`
            // contents, the two things `IS_CONNECTED` used to stand in for.
            let Some(el) = dom.get(node).filter(|_| {
                dom.node_document(node) == dom.document() && !dom.in_template_contents(node)
            }) else {
                return;
            };
            (
                attr_value(el, "src"),
                attr_value(el, "loading"),
                !el.is_connected(),
            )
        };
        let Some(src) = src.filter(|s| !s.is_empty()) else {
            return;
        };
        let Some(url) = self.resolve_url(&src) else {
            return;
        };

        // `data:` decodes inline (no network to save), and `loading="eager"` is
        // the author asking for exactly that: no deferral. A detached image is
        // eager for a harder reason — deferral is keyed on intersecting the
        // viewport, which a node outside the tree can never do, so deferring it
        // would be dropping it.
        let eager = detached || url.starts_with("data:") || loading.as_deref() == Some("eager");
        if self.lazy_images.get() && !eager {
            // Whatever this node was waiting on, it is not waiting now: the
            // queue is the only thing holding it, and a wait left behind would
            // hold a pin nothing will ever release.
            self.unregister_image_waiter(node);
            self.deferred_images.borrow_mut().insert(node);
            return;
        }
        // Undeferred (`loading` flipped to `eager`, or a `src` change on an
        // already-deferred node): it loads now, so it leaves the queue.
        self.deferred_images.borrow_mut().remove(&node);
        self.begin_image_load(node, &url);
    }

    /// Fires the already-settled event, or makes `node` the sole waiter for
    /// `url` and starts the fetch.
    ///
    /// The one place an `<img>` starts waiting, so the waiter entry, its pin
    /// and the fetch cannot drift apart. Every caller reaches it having already
    /// decided that this node loads this URL *now*.
    fn begin_image_load(&self, node: NodeId, url: &str) {
        // A URL the store has already resolved fires its event right now. The
        // load that filled it is long finished, and `requested_images` means no
        // second request will ever be made for it — so an element registered as
        // a waiter here would wait forever. (Two `<img>` with the same `src` is
        // the ordinary case, not a corner one.)
        let settled = {
            let layout = self.state.layout.borrow();
            let images = layout.images();
            if images.get(url).is_some() {
                Some("load")
            } else if images.is_broken(url) {
                Some("error")
            } else {
                None
            }
        };
        if let Some(event) = settled {
            self.unregister_image_waiter(node);
            self.fire_element_event(node, event);
            return;
        }
        self.register_image_waiter(node, url);
        self.start_image_load_url(url);
    }

    /// Registers `node` as a waiter on `url` and pins it for the wait.
    ///
    /// A load in flight keeps its `<img>` alive. `new Image()` is routinely
    /// written as `const i = new Image(); i.onload = …; i.src = …;` inside a
    /// function — the moment it returns, the only reference left is the
    /// wrapper, and a GC before the load settles would free the node and
    /// swallow the event. HTML says as much ("the img element must not be
    /// garbage collected while it has pending activity").
    ///
    /// Exactly one entry per node, so exactly one pin and exactly one event:
    /// `el.src = …` on a detached `<img>` followed by `appendChild` queues the
    /// update twice (attribute change, then connection), and a preloader
    /// counting `load`s must not see two.
    fn register_image_waiter(&self, node: NodeId, url: &str) {
        // Pin before releasing the old wait, so the node is never momentarily
        // unpinned — `release_image_pin` collects.
        self.state.dom.borrow_mut().pin(node);
        self.unregister_image_waiter(node);
        self.image_waiters
            .borrow_mut()
            .entry(url.to_owned())
            .or_default()
            .push(node);
    }

    /// Drops every wait `node` holds, releasing a pin for each. A `src`
    /// reassigned mid-flight orphans the first wait: nothing will fetch that URL
    /// a second time, so its `notify_image_waiters` never comes.
    fn unregister_image_waiter(&self, node: NodeId) {
        let dropped = {
            let mut waiters = self.image_waiters.borrow_mut();
            let mut dropped = 0usize;
            waiters.retain(|_, nodes| {
                let before = nodes.len();
                nodes.retain(|waiter| *waiter != node);
                dropped += before - nodes.len();
                !nodes.is_empty()
            });
            dropped
        };
        for _ in 0..dropped {
            self.release_image_pin(node);
        }
    }

    /// Starts loads for deferred `<img>` elements that now intersect the
    /// viewport (plus margin). Runs once per event-loop iteration in lazy mode.
    ///
    /// Deliberately reports no progress to the loop, like the sibling resource
    /// scans: progress arrives on its own, as net events from the loads this
    /// starts. Saying "progressed" here would spin the loop on a page that has
    /// nothing left to do.
    ///
    /// The gate needs every input that can move an image into the viewport, and
    /// [`PaintStamp`] alone is not it. Its `style_version` is the one recorded at
    /// the *last reflow*, so an external sheet landing without a DOM mutation
    /// (the trap `start_background_image_loads` documents) would leave the gate
    /// shut while the first-screen layout it invalidates still has a hole in it.
    /// Document scroll is missing too — `PaintStamp` excludes it on purpose,
    /// because the rasterizer applies it after the display list. Hence the live
    /// `style.version()` and `document_scroll_version()` alongside the stamp.
    /// All of them are reads, so a shut gate costs nothing.
    fn start_visible_image_loads(&self) {
        if !self.lazy_images.get() || self.deferred_images.borrow().is_empty() {
            return;
        }
        if self.last_lazy_scan.get() == Some(self.lazy_scan_gate()) {
            return;
        }
        self.flush_layout();

        let visible: Vec<(NodeId, String)> = {
            let dom = self.state.dom.borrow();
            let layout = self.state.layout.borrow();
            let mut deferred = self.deferred_images.borrow_mut();
            // Nodes wait here indefinitely, so unlike a drain queue this set
            // outlives removals: an SPA that drops an `<img>` leaves a freed id
            // behind, and `bounding_client_rect` walks the DOM with it. Same
            // guard IntersectionObserver keeps over its targets.
            deferred.retain(|node| dom.get(*node).is_some());

            let viewport = layout.viewport();
            let mut visible = Vec::new();
            for &node in deferred.iter() {
                if !dom.node(node).is_connected() {
                    continue;
                }
                // No box (`display: none`) — no load, exactly as a browser does.
                // The node stays queued: revealing it restyles the document,
                // which reopens the gate.
                let Some(rect) = layout.bounding_client_rect(&dom, node) else {
                    continue;
                };
                // `loading="lazy"` is the author asking for no margin. Anything
                // else gets one viewport of lookahead (Chrome's is distance- and
                // network-dependent; one screen is the honest middle).
                let margin = if attr_value(dom.node(node), "loading").as_deref() == Some("lazy") {
                    0.0
                } else {
                    viewport.height
                };
                if !intersects_viewport(rect, viewport, margin) {
                    continue;
                }
                // Read `src` now, not at deferral time: it may have changed
                // while the image waited.
                if let Some(src) = attr_value(dom.node(node), "src").filter(|s| !s.is_empty()) {
                    visible.push((node, src));
                }
            }
            visible
        };

        // Loads start with the borrows dropped: a `data:` URL decodes inline and
        // re-enters layout through `finish_image`.
        for (node, src) in visible {
            // Off the queue as it starts, or every open gate re-walks the
            // geometry of every image ever deferred.
            self.deferred_images.borrow_mut().remove(&node);
            if let Some(url) = self.resolve_url(&src) {
                // Through `begin_image_load`, not `start_image_load_url`: a
                // deferred image registers no waiter (it holds no pin while it
                // waits for the viewport), so this is where it starts waiting —
                // and where an already-settled URL fires its event instead.
                self.begin_image_load(node, &url);
            }
        }
        self.last_lazy_scan.set(Some(self.lazy_scan_gate()));
    }

    /// Every input to the visibility scan, read without a reflow.
    fn lazy_scan_gate(&self) -> LazyScanGate {
        let dom = self.state.dom.borrow();
        let layout = self.state.layout.borrow();
        LazyScanGate {
            dom_style_version: dom.style_version(),
            dom_structure_version: dom.structure_version(),
            style_version: self.state.style.borrow().version(),
            paint: layout.paint_stamp(),
            document_scroll_version: layout.document_scroll_version(),
        }
    }

    /// Loads every `<img>` still deferred by lazy mode and waits for the loads to
    /// finish, leaving the page eager from here on.
    ///
    /// A lazy page is complete for its viewport and nowhere else, so this is a
    /// required step before full-page output (`screenshot_full_page`,
    /// `print_to_pdf`) — without it everything below the fold paints as a hole.
    /// A no-op on an eager page.
    pub fn load_deferred_images(&self, budget: Duration) {
        self.lazy_images.set(false);
        let deferred: Vec<NodeId> = self.deferred_images.borrow_mut().drain().collect();
        for node in deferred {
            // The queue can hold nodes freed since they were deferred.
            if self.state.dom.borrow().get(node).is_none() {
                continue;
            }
            self.start_image_load(node);
        }
        self.await_subresources(budget);
    }

    /// Starts a load for an absolute image `url`, deduplicating and decoding
    /// `data:` URLs inline (shared by `<img>` and background images).
    fn start_image_load_url(&self, url: &str) {
        // Deduplicate: a URL already loaded or in flight is not re-fetched.
        if !self.requested_images.borrow_mut().insert(url.to_owned()) {
            return;
        }

        // `data:` URLs decode inline (no network), so the pixels are available
        // to layout in this same turn. The decoder is the fetch stack's, and it
        // is handed the whole serialized remainder — fragment included, which
        // `net::data::decode` drops for exactly this caller — so an inline
        // decode and one that went over `net` agree byte for byte.
        if let Some(rest) = url.strip_prefix("data:") {
            match oxidepage_net::data::decode(rest) {
                Some(body) => {
                    self.finish_image(url, &body.bytes, Some(&body.content_type));
                }
                None => self.mark_image_broken(url),
            }
            return;
        }

        let doc_url = self.state.dom.borrow().document_url().to_owned();
        let id = self
            .net
            .start_resource(NetRequest::subresource(url, doc_url).of_type(ResourceType::Image));
        self.in_flight.set(self.in_flight.get() + 1);
        self.pending_images.borrow_mut().insert(
            id,
            PendingImage {
                url: url.to_owned(),
                content_type: None,
                status: 0,
                buffer: Vec::new(),
            },
        );
    }

    /// Scans resolved element styles for `background-image: url(...)` and starts
    /// loads for each (deduplicated), when styles changed since the last scan.
    ///
    /// The gate needs *both* version counters. Background images live in computed
    /// style, so a DOM mutation that restyles an element (a class toggle) bumps
    /// only `dom.style_version()`. But a sheet arriving without a DOM mutation —
    /// an external `<link>`/`@import` completing, or a CSSOM `insertRule` — bumps
    /// only `style.version()` (the same trap `start_font_face_loads` documents).
    /// Gating on either alone silently drops one of the two sources.
    fn start_background_image_loads(&self) {
        let version = (
            self.state.dom.borrow().style_version(),
            self.state.style.borrow().version(),
        );
        if self.last_bg_scan.get() == version {
            return;
        }
        self.last_bg_scan.set(version);

        // Resolve styles so `background-image` is available (idempotent when
        // already current).
        {
            let mut dom = self.state.dom.borrow_mut();
            let mut style = self.state.style.borrow_mut();
            style.resolve_styles(&mut dom);
        }

        let urls: Vec<String> = {
            let dom = self.state.dom.borrow();
            let mut urls = Vec::new();
            for node in dom.inclusive_descendants(dom.document()) {
                if let Some(style) = dom.primary_style(node) {
                    collect_background_image_urls(&style, &mut urls);
                }
                // `::before`/`::after` can carry their own `background-image`.
                for pseudo in [&PseudoElement::Before, &PseudoElement::After] {
                    if let Some(style) = dom.pseudo_style(node, pseudo) {
                        collect_background_image_urls(&style, &mut urls);
                    }
                }
            }
            urls
        };
        for url in urls {
            self.start_image_load_url(&url);
        }
    }

    /// Puts every inline `<svg>` element in the document into the image store,
    /// keyed by [`oxidepage_layout::images::inline_svg_key`]. Box construction
    /// sizes an `<svg>` as a replaced element from that entry, so without this
    /// pass an inline SVG — the way sites ship logos and icons — lays out 0×0
    /// and paints nothing.
    ///
    /// What is stored is the SVG *source*, not pixels: the backend rasterizes it
    /// at the size it paints at. The source is the element's `outer_html` with
    /// its computed `color` embedded ([`oxidepage_layout::images::inline_svg_source`]),
    /// which is what makes `fill="currentColor"` resolve — resvg renders the SVG
    /// in isolation and knows nothing of the surrounding cascade.
    ///
    /// The key covers markup *and* color, so the gate needs *both* DOM counters.
    /// `structure_version` alone is wrong: a style-only attribute write
    /// (`class`/`style`/`id`) changes an `<svg>`'s `outer_html` — or just its
    /// computed color — and so its key, without bumping `structure_version`
    /// (dom `is_style_only_attr`), yet every such write bumps `style_version`
    /// (via `note_subtree_mutation`). Gating on the pair therefore fires on every
    /// change to either input, so the new key is filled in before a box rebuild's
    /// `image_data()` looks it up and — on a miss — collapses the `<svg>` to 0×0.
    /// The store deduplicates repeated icons by key anyway.
    fn rasterize_inline_svgs(&self) {
        let version = {
            let dom = self.state.dom.borrow();
            (dom.structure_version(), dom.style_version())
        };
        if self.last_inline_svg_scan.get() == version {
            return;
        }

        // The key's color input comes from the cascade, so the styles have to be
        // resolved before it can be read — an unstyled `<svg>` would be skipped
        // here and never revisited, since the gate would already be shut.
        self.flush_layout();
        let version = {
            let dom = self.state.dom.borrow();
            (dom.structure_version(), dom.style_version())
        };
        self.last_inline_svg_scan.set(version);

        let sources: Vec<(String, String)> = {
            let dom = self.state.dom.borrow();
            dom.inclusive_descendants(dom.document())
                .filter(|&node| {
                    dom.node(node)
                        .as_element()
                        .is_some_and(|el| &*el.name.local == "svg")
                })
                .filter_map(|node| {
                    let style = dom.primary_style(node)?;
                    let color = oxidepage_layout::images::current_color(&style);
                    let markup = oxidepage_layout::images::inline_svg_markup(&dom, node);
                    let vars = oxidepage_layout::images::svg_var_substitutions(&markup, &style);
                    let key = oxidepage_layout::images::inline_svg_key(&markup, color, &vars);
                    let source = oxidepage_layout::images::inline_svg_source(&markup, color, &vars);
                    Some((key, source))
                })
                .collect()
        };

        for (key, source) in sources {
            {
                let layout = self.state.layout.borrow();
                if layout.images().get(&key).is_some() || layout.images().is_broken(&key) {
                    continue;
                }
            }
            self.finish_image(&key, source.as_bytes(), Some("image/svg+xml"));
        }
    }

    fn handle_image_event(&self, id: RequestId, event: NetEvent) {
        match event {
            NetEvent::Headers {
                status, headers, ..
            } => {
                if let Some(image) = self.pending_images.borrow_mut().get_mut(&id) {
                    image.status = status;
                    image.content_type = header_content_type(&headers);
                }
            }
            NetEvent::Chunk { data, .. } => {
                if let Some(image) = self.pending_images.borrow_mut().get_mut(&id) {
                    image.buffer.extend_from_slice(&data);
                }
            }
            NetEvent::Done { .. } => {
                let pending = self.pending_images.borrow_mut().remove(&id);
                if let Some(pending) = pending {
                    if pending.status >= 400 {
                        self.mark_image_broken(&pending.url);
                    } else {
                        self.finish_image(
                            &pending.url,
                            &pending.buffer,
                            pending.content_type.as_deref(),
                        );
                    }
                    self.decr_in_flight();
                    self.net.finish(id);
                }
            }
            NetEvent::Error { error, .. } => {
                let pending = self.pending_images.borrow_mut().remove(&id);
                if let Some(pending) = pending {
                    self.hooks
                        .report_resource_error(format!("image `{}`: {error}", pending.url));
                    self.mark_image_broken(&pending.url);
                    self.decr_in_flight();
                    self.net.finish(id);
                }
            }
            // Consumed by the net service before routing (ADR-0032 D8), so
            // this is unreachable rather than merely unhandled.
            NetEvent::AuthRequired { .. } => {}
        }
    }

    /// Decodes image `bytes` and inserts the result into the layout store, or
    /// marks the URL broken on a decode failure (never fatal). A raster image
    /// lands as pixels; an SVG lands as markup plus its intrinsic size, and is
    /// rasterized later by the backend at the size it paints at.
    fn finish_image(&self, url: &str, bytes: &[u8], content_type: Option<&str>) {
        use oxidepage_paint::DecodedImageData;

        match oxidepage_paint::decode_image(bytes, content_type) {
            Some(DecodedImageData::Raster(pixels)) => {
                self.state.layout.borrow_mut().insert_raster_image(
                    url.to_owned(),
                    pixels.width,
                    pixels.height,
                    std::sync::Arc::new(pixels.rgba),
                );
            }
            Some(DecodedImageData::Vector(vector)) => {
                self.state.layout.borrow_mut().insert_vector_image(
                    url.to_owned(),
                    vector.width,
                    vector.height,
                    std::sync::Arc::new(vector.svg),
                );
            }
            // Already reports `error` to the waiters, so this must not fall
            // through to the `load` below.
            None => {
                self.mark_image_broken(url);
                return;
            }
        }
        self.notify_image_waiters(url, "load");
    }

    fn mark_image_broken(&self, url: &str) {
        self.state
            .layout
            .borrow_mut()
            .mark_image_broken(url.to_owned());
        self.notify_image_waiters(url, "error");
    }

    /// Fires `load`/`error` at every `<img>` that was waiting on `url`.
    ///
    /// Both funnels are also used for resources no element is waiting on — a
    /// `background-image`, or an inline `<svg>` rasterized under a synthesized
    /// key — which simply have no waiters and cost a missed map lookup.
    ///
    /// The element's *current* `src` is re-resolved before firing: a script that
    /// reassigns `src` while the first load is in flight leaves a stale waiter
    /// behind, and that element's `load` belongs to the URL it points at now,
    /// not to the one that happened to finish.
    fn notify_image_waiters(&self, url: &str, event: &str) {
        let Some(waiters) = self.image_waiters.borrow_mut().remove(url) else {
            return;
        };
        for node in waiters {
            let still_ours = {
                let dom = self.state.dom.borrow();
                dom.get(node)
                    .and_then(|n| attr_value(n, "src"))
                    .and_then(|src| self.resolve_url(&src))
                    .is_some_and(|current| current == url)
            };
            if still_ours {
                self.fire_element_event(node, event);
            }
            // The other end of the pin `start_image_load` took, released after
            // dispatch.
            self.release_image_pin(node);
        }
    }

    // === Web fonts (Phase 7, WP-D) ===

    /// Scans the document's `@font-face` rules and starts a load for each rule's
    /// best-supported `src:` (deduplicated), when styles changed since the last
    /// scan. Fonts join `in_flight`, so screenshots/PDF/`load` wait for them.
    fn start_font_face_loads(&self) {
        // Gate on the style engine's version, not `dom.style_version()`: the
        // `@font-face` rules come from the author sheets, and a sheet arriving
        // without an accompanying DOM mutation (an external `<link>`/`@import`
        // completing, or a CSSOM `insertRule`) bumps only `style.version()`. A
        // dom-version gate would miss those and never fetch their web fonts.
        let version = self.state.style.borrow().version();
        if self.last_fontface_scan.get() == version {
            return;
        }
        self.last_fontface_scan.set(version);

        // `@font-face` rules come from the author sheets directly (no computed
        // style needed), so — unlike the background-image scan — no style
        // resolution is required here.
        let faces = self.state.style.borrow().font_faces();
        for info in faces {
            let urls: Vec<String> = supported_font_srcs(&info.sources)
                .filter_map(|src| self.resolve_url(src))
                .collect();
            let attrs = oxidepage_layout::WebFontAttrs::from_face(&info);
            self.start_font_load(&info.family, &urls, attrs);
        }
    }

    /// Starts a load for the first of `urls` (a `@font-face` rule's supported
    /// `src:` entries, in declaration order), deduplicating by `(family, url)`
    /// and decoding `data:` URLs inline.
    ///
    /// The remaining entries ride along as fallbacks: a source that fails to
    /// download or parse hands off to the next one (CSS Fonts §4.3), which is
    /// what makes `src: url(a.woff2), url(b.woff2)` survive `a.woff2` 404ing.
    fn start_font_load(
        &self,
        family: &str,
        urls: &[String],
        attrs: oxidepage_layout::WebFontAttrs,
    ) {
        let mut remaining = urls;
        while let Some((url, fallbacks)) = remaining.split_first() {
            if !self
                .requested_fonts
                .borrow_mut()
                .insert((family.to_owned(), url.clone()))
            {
                // Already loaded or in flight; that attempt owns the fallbacks.
                return;
            }

            // `data:` URLs decode inline (no network), so success or failure is
            // known immediately and a failure falls straight through.
            if let Some(rest) = url.strip_prefix("data:") {
                let decoded = oxidepage_net::data::decode(rest)
                    .is_some_and(|body| self.finish_font(family, &body.bytes, attrs));
                if decoded {
                    return;
                }
                remaining = fallbacks;
                continue;
            }

            let doc_url = self.state.dom.borrow().document_url().to_owned();
            let id = self
                .net
                .start_resource(NetRequest::subresource(url, doc_url).of_type(ResourceType::Font));
            self.in_flight.set(self.in_flight.get() + 1);
            self.pending_fonts.borrow_mut().insert(
                id,
                PendingFont {
                    url: url.clone(),
                    family: family.to_owned(),
                    attrs,
                    fallbacks: fallbacks.to_vec(),
                    status: 0,
                    buffer: Vec::new(),
                },
            );
            self.state.page.fonts_loading.set(true);
            return;
        }
        // Every source exhausted: the family never resolves and text keeps its
        // fallback font, which is the non-fatal spec behavior.
    }

    fn handle_font_event(&self, id: RequestId, event: NetEvent) {
        match event {
            NetEvent::Headers { status, .. } => {
                // The font decoder sniffs the sfnt/WOFF signature, so — unlike
                // images/sheets/scripts — the Content-Type header is not needed.
                if let Some(font) = self.pending_fonts.borrow_mut().get_mut(&id) {
                    font.status = status;
                }
            }
            NetEvent::Chunk { data, .. } => {
                if let Some(font) = self.pending_fonts.borrow_mut().get_mut(&id) {
                    font.buffer.extend_from_slice(&data);
                }
            }
            NetEvent::Done { .. } => {
                let pending = self.pending_fonts.borrow_mut().remove(&id);
                if let Some(pending) = pending {
                    let loaded = pending.status < 400
                        && self.finish_font(&pending.family, &pending.buffer, pending.attrs);
                    if !loaded {
                        // Downloaded but unusable (error status, or bytes we
                        // cannot decode): fall through to the next `src:`.
                        self.start_font_load(&pending.family, &pending.fallbacks, pending.attrs);
                    }
                    self.decr_in_flight();
                    self.net.finish(id);
                    self.settle_font_ready();
                }
            }
            NetEvent::Error { error, .. } => {
                let pending = self.pending_fonts.borrow_mut().remove(&id);
                if let Some(pending) = pending {
                    self.hooks
                        .report_resource_error(format!("web font `{}`: {error}", pending.url));
                    self.start_font_load(&pending.family, &pending.fallbacks, pending.attrs);
                    self.decr_in_flight();
                    self.net.finish(id);
                    self.settle_font_ready();
                }
            }
            // Consumed by the net service before routing (ADR-0032 D8), so
            // this is unreachable rather than merely unhandled.
            NetEvent::AuthRequired { .. } => {}
        }
    }

    /// Resolves any stashed `document.fonts.ready` promises
    /// (`imp::font_face_set::ready`) once every `@font-face` load this
    /// document started has settled: `pending_fonts` is empty *and* parsing
    /// has finished (mirrors `ready`'s own synchronous-resolve condition —
    /// a font-face rule further down an unparsed document could still start
    /// a load, so emptiness alone is not enough). Called after every point
    /// that could make `pending_fonts` newly empty or newly non-empty:
    /// `start_font_face_loads` (every `run_until_stalled_until` tick, which
    /// also covers "no `@font-face` rule ever needed loading") and here,
    /// after a font load's fallback chain is exhausted.
    fn settle_font_ready(&self) {
        let loading = !self.pending_fonts.borrow().is_empty();
        self.state.page.fonts_loading.set(loading);
        if loading || self.state.ready_state() == oxidepage_bindings::ReadyState::Loading {
            return;
        }
        // Per world: each world's `document.fonts.ready` promises are its own
        // values, and resolving them from another world is impossible.
        self.for_each_world(oxidepage_bindings::resolve_font_ready);
    }

    /// Decodes and registers font `bytes` under `family`, reporting whether the
    /// family now resolves to it. A decode failure is non-fatal: the caller
    /// either tries the next `src:` or leaves the text on its fallback font.
    fn finish_font(
        &self,
        family: &str,
        bytes: &[u8],
        attrs: oxidepage_layout::WebFontAttrs,
    ) -> bool {
        let outcome = self
            .state
            .layout
            .borrow_mut()
            .register_web_font(family, bytes, attrs);
        if outcome == oxidepage_layout::WebFontOutcome::Registered {
            // A new face changes what `ex`/`ch`/`ic` and `font-size-adjust`
            // resolve to; without this, stylo reuses the values cascaded before
            // it arrived.
            self.state.style.borrow_mut().note_fonts_changed();
        }
        outcome.is_usable()
    }

    fn handle_async_script_event(&self, id: RequestId, event: NetEvent) {
        match event {
            NetEvent::Headers {
                status, headers, ..
            } => {
                if let Some(script) = self.pending_async.borrow_mut().get_mut(&id) {
                    script.status = status;
                    script.content_type = header_content_type(&headers);
                }
            }
            NetEvent::Chunk { data, .. } => {
                if let Some(script) = self.pending_async.borrow_mut().get_mut(&id) {
                    script.buffer.extend_from_slice(&data);
                }
            }
            NetEvent::Done { .. } => {
                let script = self.pending_async.borrow_mut().remove(&id);
                if let Some(script) = script {
                    self.decr_in_flight();
                    self.net.finish(id);
                    let result = if script.status >= 400 {
                        Err(format!("script `{}`: HTTP {}", script.url, script.status))
                    } else {
                        Ok(decode_charset(
                            &script.buffer,
                            script.content_type.as_deref(),
                        ))
                    };
                    if script.dispatch_events {
                        let completed = CompletedDynamicScript {
                            node: script.node,
                            url: script.url,
                            result,
                        };
                        if let Some(order) = script.ordered {
                            self.ordered_dynamic_ready
                                .borrow_mut()
                                .insert(order, completed);
                            self.drain_ordered_dynamic_scripts();
                        } else {
                            self.finish_dynamic_script(completed);
                        }
                    } else {
                        match result {
                            Ok(source) => {
                                let _ = self.eval_classic(&source, &script.url, Some(script.node));
                            }
                            Err(message) => self.hooks.report_resource_error(message),
                        }
                    }
                }
            }
            NetEvent::Error { error, .. } => {
                let script = self.pending_async.borrow_mut().remove(&id);
                if let Some(script) = script {
                    self.decr_in_flight();
                    self.net.finish(id);
                    let completed = CompletedDynamicScript {
                        node: script.node,
                        url: script.url.clone(),
                        result: Err(format!("script `{}`: {error}", script.url)),
                    };
                    if script.dispatch_events {
                        if let Some(order) = script.ordered {
                            self.ordered_dynamic_ready
                                .borrow_mut()
                                .insert(order, completed);
                            self.drain_ordered_dynamic_scripts();
                        } else {
                            self.finish_dynamic_script(completed);
                        }
                    } else {
                        self.hooks.report_resource_error(match completed.result {
                            Ok(_) => unreachable!(),
                            Err(message) => message,
                        });
                    }
                }
            }
            // Consumed by the net service before routing (ADR-0032 D8), so
            // this is unreachable rather than merely unhandled.
            NetEvent::AuthRequired { .. } => {}
        }
    }

    fn decr_in_flight(&self) {
        self.in_flight.set(self.in_flight.get().saturating_sub(1));
    }

    // === Style loading (Phase 4) ===

    /// Reports binding payloads to the embedder.
    ///
    /// Returns whether anything was delivered, so the loop keeps turning while
    /// a page is producing them. Reporting cannot re-enter JS — it is a channel
    /// send — so it is safe at this point in the drain order.
    fn drain_binding_events(&self) -> bool {
        // Push and pull are alternatives, not a pipeline: without a sink the
        // queue belongs to `Page::drain_binding_calls`, and draining it here
        // would silently discard every payload an embedder using the pull API
        // is waiting for. The queue is bounded either way.
        if self.hooks.event_sink.borrow().is_none() {
            return false;
        }
        let calls: Vec<oxidepage_bindings::BindingCall> = self
            .state
            .page
            .binding_calls
            .borrow_mut()
            .drain(..)
            .collect();
        if calls.is_empty() {
            return false;
        }
        for call in calls {
            self.hooks.emit(PageRecord::Binding {
                name: call.name,
                payload: call.payload,
                context_id: call.context_id,
            });
        }
        true
    }

    /// Commits a script-created parser that was written to and never closed
    /// (ADR-0034 D2).
    ///
    /// **Only when it holds something.** An empty buffer is an `open()` whose
    /// `write()`s have not happened yet, and flushing that would replace the
    /// document with nothing and leave the writes with no parser to reach.
    fn flush_unclosed_script_parser(&self) {
        if !self.state.script_parser_is_open() {
            return;
        }
        match self.state.take_script_parser() {
            Some(html) if !html.is_empty() => self.state.queue_document_replacement(html),
            // Put an empty buffer back: taking it is what closes the parser.
            Some(_) => self.state.open_script_parser(),
            None => {}
        }
    }

    /// Answers the evaluations parked on a promise (ADR-0034 D1).
    ///
    /// A settled promise is described **in its own world** — the value is a
    /// `Persistent` of that runtime, and reading it from the main world reports
    /// `undefined` for a promise that really did fulfil (ADR-0033 D4). One that
    /// outlived its budget reports the pending promise itself, exactly as the
    /// blocking path does, so a driver waiting on a promise nothing will
    /// resolve gets an answer rather than silence.
    fn drain_pending_awaits(&self) -> bool {
        if self.pending_awaits.borrow().is_empty() {
            return false;
        }
        // Swapped out rather than held: describing a settled promise runs a
        // microtask checkpoint, which runs author JS, which can defer another
        // await — and that would be a `BorrowMutError` against this borrow.
        let parked = std::mem::take(&mut *self.pending_awaits.borrow_mut());
        let now = Instant::now();
        let mut answered = false;
        let mut still_waiting = Vec::new();
        for entry in parked {
            let settled = self.promise_is_settled(entry.world, &entry.promise);
            if !settled && now < entry.deadline {
                still_waiting.push(entry);
                continue;
            }
            let result = if settled {
                self.describe_settled(entry.world, &entry.promise, &entry.options)
            } else {
                self.describe_pending_await(&entry)
            };
            self.hooks.emit(PageRecord::AwaitSettled {
                token: entry.token,
                result,
            });
            answered = true;
        }
        // Appended, not assigned: a checkpoint above may have parked a new
        // entry, and overwriting would drop it — silently, since the only
        // symptom is a driver that never hears back.
        self.pending_awaits.borrow_mut().extend(still_waiting);
        answered
    }

    /// Reports every parked await with `text` as its exception, releasing the
    /// promises (ADR-0034 D1).
    ///
    /// Called wherever the answer can no longer arrive: the world is being torn
    /// down for a navigation, or the page is closing. Both halves matter — the
    /// driver would otherwise wait out its own timeout, and the `JsValue` would
    /// outlive the runtime that owns it, which aborts the process.
    fn fail_pending_awaits(&self, world: Option<oxidepage_bindings::WorldId>, text: &str) {
        let parked = std::mem::take(&mut *self.pending_awaits.borrow_mut());
        let mut kept = Vec::new();
        for entry in parked {
            if world.is_some_and(|only| only != entry.world) {
                kept.push(entry);
                continue;
            }
            self.hooks.emit(PageRecord::AwaitSettled {
                token: entry.token,
                result: oxidepage_bindings::remote::EvaluationResult {
                    result: oxidepage_bindings::remote::RemoteObject::undefined(),
                    exception: Some(oxidepage_bindings::remote::ExceptionDetails {
                        text: text.to_owned(),
                        ..Default::default()
                    }),
                },
            });
            // `entry` drops here, releasing the promise while its runtime is
            // still alive.
        }
        self.pending_awaits.borrow_mut().extend(kept);
    }

    /// Dispatches `scroll` events for the targets whose position changed
    /// from script (WP-G2): elements get a non-bubbling `scroll`; a viewport
    /// scroll fires on the document (bubbling on to the window).
    fn drain_scroll_events(&self) -> bool {
        let targets = self.state.take_pending_scroll_targets();
        if targets.is_empty() {
            return false;
        }
        self.with_cx(|cx| {
            for target in targets {
                let (key, bubbles) = match target {
                    Some(node) => (EventTargetKey::Node(node), false),
                    None => (EventTargetKey::Node(cx.state.dom.borrow().document()), true),
                };
                if let Err(e) = oxidepage_bindings::fire_simple_event(cx, key, "scroll", bubbles) {
                    report_throw(&self.hooks, e);
                }
            }
        });
        // One checkpoint across every world, because a scroll listener in one
        // world can queue a MutationObserver microtask in another.
        self.microtask_checkpoint_all();
        true
    }

    /// Delivers pending ResizeObserver/IntersectionObserver notifications
    /// (a `with_cx` wrapper over the bindings' `deliver_observations`).
    fn deliver_observations(&self) -> bool {
        // Each world owns its own `ResizeObserver`/`IntersectionObserver`
        // registrations and their callbacks, so delivery fans out. A utility
        // world observing the page's layout is exactly what a driver's
        // `waitForSelector` is built on.
        let mut delivered = false;
        for world in self.worlds.all() {
            delivered |= self
                .with_cx_in(world.id, oxidepage_bindings::deliver_observations)
                .unwrap_or(false);
        }
        delivered
    }

    /// Applies queued [`StyleUpdate`]s to the style engine: connects/removes
    /// `<style>` sheets and starts fetches for `<link rel=stylesheet>`.
    fn drain_style_updates(&self) {
        let updates = self.state.dom.borrow_mut().take_style_updates();
        for update in updates {
            match update {
                StyleUpdate::StyleElement(node) => self.upsert_inline_stylesheet(node),
                StyleUpdate::StyleElementRemoved(node) | StyleUpdate::LinkElementRemoved(node) => {
                    self.state.style.borrow_mut().remove_sheet_for_node(node);
                    self.link_sheets.borrow_mut().remove(&node);
                }
                StyleUpdate::LinkElement(node) => self.start_link_stylesheet(node),
            }
        }
    }

    /// Builds (or replaces) the stylesheet for a connected `<style>` element
    /// from its text content and `media` attribute.
    fn upsert_inline_stylesheet(&self, node: NodeId) {
        let dom = self.state.dom.borrow();
        // Snapshot id from the `StyleUpdate` queue: the `<style>` may have been
        // removed and freed between queueing and this drain.
        let Some(el) = dom.get(node).filter(|n| n.is_connected()) else {
            return;
        };
        let media = attr_value(el, "media");
        let css = dom.text_content(node);
        let url_data = dom.url_extra_data().clone();
        let doc_url = dom.document_url().to_owned();
        let fetcher = PageCssFetcher {
            net: Rc::clone(&self.net),
            doc_url,
        };
        let loader = BlockingImportLoader::new(
            &fetcher,
            self.state.style.borrow().lock().clone(),
            Origin::Author,
            None,
        );
        let sheet = self.state.style.borrow().make_stylesheet_with_loader(
            &css,
            &url_data,
            media.as_deref(),
            Some(&loader),
        );
        self.state
            .style
            .borrow_mut()
            .add_sheet_for_node(&dom, node, sheet);
    }

    /// Starts fetching an external stylesheet for a connected `<link>`; scripts
    /// block until it (and every other pending sheet) completes.
    fn start_link_stylesheet(&self, node: NodeId) {
        let (href, media, doc_url) = {
            let dom = self.state.dom.borrow();
            // Snapshot id from the `StyleUpdate` queue — see
            // `upsert_inline_stylesheet`.
            let Some(el) = dom.get(node).filter(|n| n.is_connected()) else {
                return;
            };
            (
                attr_value(el, "href"),
                attr_value(el, "media"),
                dom.document_url().to_owned(),
            )
        };
        let Some(href) = href else {
            // A `<link>` that lost its `href` obtains nothing; forget any prior
            // resource so a later re-add re-fetches.
            self.link_sheets.borrow_mut().remove(&node);
            return;
        };
        let Some(url) = self.resolve_url(&href) else {
            self.hooks
                .report_resource_error(format!("cannot resolve stylesheet href `{href}`"));
            return;
        };
        let url = url.to_string();

        // The resource is obtained once per URL. A repeat update for the *same*
        // URL is a `media`/`disabled` change, not a new resource: re-apply from
        // the cached bytes (never re-fetch, never re-fire `load`). This is both
        // the HTML behaviour and what stops `<link media="print"
        // onload="this.media='all'">` from looping the load until the request
        // budget trips.
        // `Some(loaded)` iff this node already has the same URL: `Some(bytes)`
        // when obtained (re-apply), `None` while still in flight (the pending
        // fetch will apply). Cloned out so the borrow is dropped before re-apply.
        let same_url = {
            let cache = self.link_sheets.borrow();
            match cache.get(&node) {
                Some(cached) if cached.url == url => Some(cached.loaded.clone()),
                _ => None,
            }
        };
        if let Some(loaded) = same_url {
            if let Some((bytes, content_type)) = loaded {
                self.reapply_link_stylesheet(node, &url, &bytes, content_type, media);
            }
            return;
        }

        let id = self.net.start_resource(
            NetRequest::subresource(&url, doc_url).of_type(ResourceType::Stylesheet),
        );
        self.in_flight.set(self.in_flight.get() + 1);
        self.pending_stylesheets
            .set(self.pending_stylesheets.get() + 1);
        self.link_sheets.borrow_mut().insert(
            node,
            LinkSheet {
                url: url.clone(),
                loaded: None,
            },
        );
        self.pending_sheets.borrow_mut().insert(
            id,
            PendingSheet {
                node,
                url,
                content_type: None,
                media,
                status: 0,
                buffer: Vec::new(),
            },
        );
    }

    /// Re-applies an already-obtained `<link>` stylesheet after a `media`/
    /// `disabled` change, re-parsing the cached bytes with the new media list.
    /// No network request and no `load` event — the resource was already
    /// obtained (see [`LinkSheet`]).
    fn reapply_link_stylesheet(
        &self,
        node: NodeId,
        url: &str,
        bytes: &[u8],
        content_type: Option<String>,
        media: Option<String>,
    ) {
        let dom = self.state.dom.borrow();
        if !dom.get(node).is_some_and(|n| n.is_connected()) {
            return;
        }
        let url_data = url::Url::parse(url)
            .map(style::stylesheets::UrlExtraData::from)
            .unwrap_or_else(|_| dom.url_extra_data().clone());
        let doc_url = dom.document_url().to_owned();
        let fetcher = PageCssFetcher {
            net: Rc::clone(&self.net),
            doc_url,
        };
        let engine = self.state.style.borrow();
        let loader =
            BlockingImportLoader::new(&fetcher, engine.lock().clone(), Origin::Author, None);
        let charset = content_type.as_deref().and_then(content_type_charset);
        let sheet = engine.make_stylesheet_from_bytes(
            bytes,
            url_data,
            charset.as_deref(),
            None,
            media.as_deref(),
            Some(&loader),
        );
        drop(engine);
        self.state
            .style
            .borrow_mut()
            .add_sheet_for_node(&dom, node, sheet);
    }

    fn handle_stylesheet_event(&self, id: RequestId, event: NetEvent) {
        match event {
            NetEvent::Headers {
                status, headers, ..
            } => {
                if let Some(sheet) = self.pending_sheets.borrow_mut().get_mut(&id) {
                    sheet.status = status;
                    sheet.content_type = header_content_type(&headers);
                }
            }
            NetEvent::Chunk { data, .. } => {
                if let Some(sheet) = self.pending_sheets.borrow_mut().get_mut(&id) {
                    sheet.buffer.extend_from_slice(&data);
                }
            }
            NetEvent::Done { .. } => {
                let pending = self.pending_sheets.borrow_mut().remove(&id);
                if let Some(pending) = pending {
                    let node = pending.node;
                    // A non-success status delivers an error page, not CSS; per
                    // the HTML spec the sheet load is a network error (skip it).
                    let ok = if pending.status >= 400 {
                        self.hooks.report_resource_error(format!(
                            "stylesheet `{}`: HTTP {}",
                            pending.url, pending.status
                        ));
                        false
                    } else {
                        // Cache the obtained bytes so a later `media`/`disabled`
                        // change on this `<link>` re-applies without re-fetching.
                        if let Some(ls) = self.link_sheets.borrow_mut().get_mut(&node)
                            && ls.url == pending.url
                        {
                            ls.loaded =
                                Some((pending.buffer.clone(), pending.content_type.clone()));
                        }
                        self.finish_link_stylesheet(pending);
                        true
                    };
                    self.decr_stylesheet(id);
                    // Fire *after* `finish_link_stylesheet` has dropped its DOM
                    // borrow: a listener re-enters JS and will touch the tree.
                    self.fire_element_event(node, if ok { "load" } else { "error" });
                }
            }
            NetEvent::Error { error, .. } => {
                let pending = self.pending_sheets.borrow_mut().remove(&id);
                if let Some(pending) = pending {
                    // A broken stylesheet must not hang the load.
                    self.hooks
                        .report_resource_error(format!("stylesheet `{}`: {error}", pending.url));
                    self.decr_stylesheet(id);
                    self.fire_element_event(pending.node, "error");
                }
            }
            // Consumed by the net service before routing (ADR-0032 D8), so
            // this is unreachable rather than merely unhandled.
            NetEvent::AuthRequired { .. } => {}
        }
    }

    /// Parses a completed external stylesheet and adds it to the engine.
    fn finish_link_stylesheet(&self, pending: PendingSheet) {
        let dom = self.state.dom.borrow();
        // The `<link>` can be removed and freed while its sheet is in flight.
        if !dom.get(pending.node).is_some_and(|n| n.is_connected()) {
            return;
        }
        let url_data = url::Url::parse(&pending.url)
            .map(style::stylesheets::UrlExtraData::from)
            .unwrap_or_else(|_| dom.url_extra_data().clone());
        let doc_url = dom.document_url().to_owned();
        let fetcher = PageCssFetcher {
            net: Rc::clone(&self.net),
            doc_url,
        };
        let engine = self.state.style.borrow();
        let loader =
            BlockingImportLoader::new(&fetcher, engine.lock().clone(), Origin::Author, None);
        // Spec-compliant charset detection on the raw bytes; the `media`
        // attribute becomes the sheet's real media list (not a textual wrapper,
        // which would break nested `@import`/`@charset` and CSSOM structure).
        let charset = pending
            .content_type
            .as_deref()
            .and_then(content_type_charset);
        let sheet = engine.make_stylesheet_from_bytes(
            &pending.buffer,
            url_data,
            charset.as_deref(),
            None,
            pending.media.as_deref(),
            Some(&loader),
        );
        drop(engine);
        self.state
            .style
            .borrow_mut()
            .add_sheet_for_node(&dom, pending.node, sheet);
    }

    fn decr_stylesheet(&self, id: RequestId) {
        self.decr_in_flight();
        self.pending_stylesheets
            .set(self.pending_stylesheets.get().saturating_sub(1));
        self.net.finish(id);
    }

    /// Blocks (bounded by `budget`) until no stylesheet is loading, so a
    /// following script sees a consistent style set (render-blocking sheets).
    fn await_pending_stylesheets(&self, budget: Duration) {
        self.wait_until(budget, |page| page.pending_stylesheets.get() == 0);
    }

    fn fire_timer(&self, timer: Timer) {
        // Expose this timer's nesting level so timers it schedules inherit
        // `current + 1` (HTML timer initialization steps).
        let prev_nesting = self.hooks.timer_nesting.replace(timer.nesting);
        // Fired in the world that scheduled it: the callback is that world's
        // value, and `restore` in any other would refuse it.
        self.with_cx_in(timer.world, |cx| {
            oxidepage_bindings::fire_timer_callback(cx, &timer.callback, &timer.args);
        });
        self.hooks.timer_nesting.set(prev_nesting);
        if let Some(interval) = timer.repeat {
            // An interval cleared during its own callback must not
            // reschedule.
            if self.hooks.cleared.borrow_mut().remove(&timer.id) {
                return;
            }
            // Each iteration deepens the nesting level; once it passes the
            // threshold a sub-4ms interval is clamped to 4ms, so an
            // `setInterval(fn, 0)` cannot busy-loop the scheduler.
            let nesting = timer.nesting.saturating_add(1);
            let interval = if nesting > MAX_TIMER_NESTING && interval < MIN_NESTED_TIMER_DELAY {
                MIN_NESTED_TIMER_DELAY
            } else {
                interval
            };
            let seq = self.hooks.next_seq();
            self.hooks.timers.borrow_mut().push(Reverse(Timer {
                deadline: Instant::now() + interval,
                seq,
                repeat: Some(interval),
                nesting,
                ..timer
            }));
        }
    }

    /// Every live world of this page, main world first.
    #[must_use]
    pub fn worlds(&self) -> Vec<WorldInfo> {
        self.worlds
            .all()
            .iter()
            .filter_map(|w| {
                let state = w.state()?;
                Some(WorldInfo {
                    name: w.name.borrow().clone(),
                    context_id: state.context_id.get(),
                    is_default: w.id == oxidepage_bindings::MAIN_WORLD,
                })
            })
            .collect()
    }

    /// Creates — or returns — the isolated world named `name`.
    ///
    /// **Idempotent by name within a document.** Chrome mints a fresh context
    /// per call, but drivers call this once per navigation expecting to rebind,
    /// and the protocol offers no way to destroy the surplus, so minting would
    /// leak a context per navigation for the life of the page (ADR-0033 D9).
    ///
    /// An empty name would collide with the main world, which is the one name a
    /// driver must never be able to take over.
    pub fn create_isolated_world(&self, name: &str) -> Result<WorldInfo, String> {
        if name.is_empty() {
            return Err("an isolated world needs a name".into());
        }
        if let Some(existing) = self.worlds.by_name(name) {
            let state = existing
                .state()
                .ok_or_else(|| "the world is being torn down".to_owned())?;
            return Ok(WorldInfo {
                name: name.to_owned(),
                context_id: state.context_id.get(),
                is_default: false,
            });
        }
        if self.worlds.len() >= worlds::MAX_WORLDS {
            return Err(format!(
                "a page may hold at most {} execution worlds",
                worlds::MAX_WORLDS
            ));
        }
        let info = self.install_isolated_world(name)?;
        // A binding registered for this world (or for every world) must exist
        // in it from the moment it is created, not from the next commit.
        self.apply_bindings_to(name);
        Ok(info)
    }

    /// Builds a world's runtime, installs the bindings and registers it.
    fn install_isolated_world(&self, name: &str) -> Result<WorldInfo, String> {
        // The page's own limits, not `Default`: an embedder that capped the
        // heap or the GC threshold to contain a hostile page meant the page,
        // and a world a driver adds later must not be the way out of that cap.
        let realm = QuickJsEngine
            .new_realm(self.realm_options)
            .map_err(|e| e.to_string())?;
        // Every per-runtime facility is replicated: one interrupt over the
        // *same* `ScriptBudget`, so a task that crosses worlds keeps one
        // deadline; the module loader, so `import()` is not a silent no-op; the
        // rejection tracker, so this world's unhandled rejections are reported
        // rather than swallowed (a job queue is per runtime).
        {
            let budget = Rc::clone(&self.script_budget);
            realm.set_interrupt(Some(Box::new(move || budget.expired())));
        }
        install_rejection_tracker(&realm, &self.hooks);
        realm.set_module_loader(Rc::new(ModuleLoader {
            net: Rc::clone(&self.net),
            dom: Rc::clone(&self.state.dom),
        }));
        let id = self.worlds.next_id();
        let state = oxidepage_bindings::install_world(&realm, &self.shared, id, name)
            .map_err(|e| e.to_string())?;
        let context_id = state.context_id.get();
        self.worlds.push(id, name, realm, state);
        Ok(WorldInfo {
            name: name.to_owned(),
            context_id,
            is_default: false,
        })
    }

    /// Tears every isolated world down and rebuilds it under the same name.
    ///
    /// Mandatory at a commit, not an optimisation to skip: a `worldName` init
    /// script must run against a *fresh* global, and a surviving world's
    /// wrapper cache, slab, listener registry and object store would all name
    /// the dead document (ADR-0033 D9). The rebuilt world keeps its name and
    /// gets a new `context_id`, which is how a driver learns the old one died.
    fn reset_worlds_for_navigation(&self) {
        let names = self.worlds.take_isolated();
        // The registry is rebuilt from scratch: `install_world` re-appends,
        // and a stale entry would advertise a world that no longer exists.
        self.shared.forget_isolated_worlds();
        for name in names {
            match self.install_isolated_world(&name) {
                Ok(_) => self.apply_bindings_to(&name),
                Err(error) => self
                    .hooks
                    .report_resource_error(format!("could not rebuild world {name:?}: {error}")),
            }
        }
    }

    /// Pumps the promise-job queue of every world but the main one.
    ///
    /// The main world's jobs are drained by its own microtask checkpoints,
    /// which run at every callback boundary; an isolated world has no such
    /// boundary of its own unless script is running in it, so the loop has to
    /// pump it as a task source.
    fn pump_non_main_jobs(&self) -> bool {
        let mut progressed = false;
        for world in self.worlds.all() {
            if world.id == oxidepage_bindings::MAIN_WORLD || !world.realm().has_pending_jobs() {
                continue;
            }
            progressed = true;
            self.with_cx_in(world.id, oxidepage_bindings::microtask_checkpoint);
        }
        progressed
    }

    /// Resets every world's per-document state, returning the request ids to
    /// abort.
    ///
    /// The main world is reset in place (it survives the commit); an isolated
    /// world is about to be destroyed, so only its in-flight requests are
    /// harvested here — `reset_worlds_for_navigation` does the teardown.
    fn reset_worlds_pending_net(&self) -> Vec<RequestId> {
        // A replacement keeps its contexts, so the main world keeps its id
        // (ADR-0034 D2); the isolated worlds are not touched at all, which is
        // why the loop below still only harvests their in-flight requests.
        let mut ids = self
            .state
            .reset_for_navigation_with(!self.preserving_contexts.get());
        for world in self.worlds.all() {
            if world.id == oxidepage_bindings::MAIN_WORLD {
                continue;
            }
            if let Some(state) = world.state() {
                ids.extend(state.take_pending_net());
            }
        }
        ids
    }

    /// Delivers every world's pending `MutationObserver` records.
    ///
    /// Records are partitioned by observer id, so a world only ever takes the
    /// ones belonging to observers it registered.
    fn deliver_mutation_records(&self) -> bool {
        let mut delivered = false;
        for world in self.worlds.all() {
            delivered |= self
                .with_cx_in(world.id, oxidepage_bindings::deliver_mutation_observers)
                .unwrap_or(false);
        }
        delivered
    }

    /// A microtask checkpoint across **every** world, run to quiescence.
    ///
    /// One pass per world is not enough: a mutation made in world A queues the
    /// MutationObserver compound microtask on world **B**'s own job queue, and
    /// B's reaction can mutate again. The loop is bounded because a runaway is
    /// the script budget's job, not this function's.
    fn microtask_checkpoint_all(&self) {
        const MAX_ROUNDS: usize = 16;
        for _ in 0..MAX_ROUNDS {
            let mut progressed = false;
            for world in self.worlds.all() {
                let had_jobs = world.realm().has_pending_jobs();
                self.with_cx_in(world.id, oxidepage_bindings::microtask_checkpoint);
                progressed |= had_jobs;
            }
            if !progressed {
                break;
            }
        }
    }

    /// Live world ids, main world first.
    pub(crate) fn world_ids(&self) -> Vec<oxidepage_bindings::WorldId> {
        self.worlds.all().iter().map(|w| w.id).collect()
    }

    /// The id of the world registered under `name` (`""` is the main world).
    pub(crate) fn world_id_by_name(&self, name: &str) -> Option<oxidepage_bindings::WorldId> {
        self.worlds.by_name(name).map(|w| w.id)
    }

    /// The id of the world a `Runtime.ExecutionContextId` names.
    pub(crate) fn world_id_by_context(
        &self,
        context_id: u64,
    ) -> Option<oxidepage_bindings::WorldId> {
        self.worlds
            .all()
            .iter()
            .find(|w| w.state().is_some_and(|s| s.context_id.get() == context_id))
            .map(|w| w.id)
    }

    /// The main world's realm. Every world is a whole realm (ADR-0033 D1); this
    /// is the one page script runs in.
    fn main_realm(&self) -> Rc<worlds::World> {
        self.worlds
            .get(oxidepage_bindings::MAIN_WORLD)
            .expect("the main world outlives the page")
    }

    /// Enters `world` and runs `f` against that world's bindings state.
    ///
    /// `None` when the world is gone, is already on the stack, or the nesting
    /// cap is hit — the same refusal [`WorldEnter::enter`] documents. Callers in
    /// the event loop treat that as "this world had nothing to do", which is
    /// correct: the work it would have done belongs to a world that is not in a
    /// state to do it.
    fn with_cx_in<T>(
        &self,
        world: oxidepage_bindings::WorldId,
        f: impl FnOnce(&BindCx<'_>) -> T,
    ) -> Option<T> {
        if world == oxidepage_bindings::MAIN_WORLD {
            // The main world takes the same latch as every other: `with_cx`
            // arms it for the duration of its scope, so a cross-world delivery
            // arriving back into main while main is live is **refused** here
            // rather than re-entering a borrowed `Context` and panicking.
            if self.worlds.is_entered(oxidepage_bindings::MAIN_WORLD) {
                return None;
            }
            return Some(self.with_cx(f));
        }
        let owns_budget = self.script_budget.arm();
        let mut slot = None;
        let mut once = Some(f);
        let entered = WorldEnter::enter(&*self.worlds, world, &mut |cx| {
            if let Some(f) = once.take() {
                slot = Some(f(cx));
            }
        });
        if owns_budget {
            self.script_budget.disarm();
        }
        entered.then_some(slot).flatten()
    }

    /// Runs `f` in every live world, main world first (ADR-0033 D6).
    fn for_each_world(&self, mut f: impl FnMut(&BindCx<'_>)) {
        for world in self.worlds.all() {
            self.with_cx_in(world.id, &mut f);
        }
    }

    /// Enters JS with the per-task script budget armed (a no-op when an outer
    /// `with_cx` already armed it, so a task keeps one deadline throughout).
    fn with_cx<T>(&self, f: impl FnOnce(&BindCx<'_>) -> T) -> T {
        let owns_budget = self.script_budget.arm();
        // Arms the main world's re-entry latch for the lifetime of this scope
        // (ADR-0033 D4). `with_cx` is the main world's only entry, so this is
        // where the guard has to live; `with_cx_in` checks it before delegating
        // here, and `WorldEnter::enter` checks it for a delivery from another
        // world.
        let _entered = self.worlds.mark_entered(oxidepage_bindings::MAIN_WORLD);
        let result = self.main_realm().realm().with_scope(|scope| {
            let cx = BindCx {
                scope,
                state: Rc::clone(&self.state),
            };
            // The only entry into JS from the event loop, so this is where the
            // parser's DOM mutations become visible to Window named access.
            // A no-op unless the document's element ids changed.
            if let Err(error) = oxidepage_bindings::sync_named_properties(&cx) {
                self.hooks.report_resource_error(format!(
                    "failed to sync window named properties: {error:?}"
                ));
            }
            f(&cx)
        });
        if owns_budget {
            self.script_budget.disarm();
        }
        result
    }

    /// Reports a JS error, naming the script budget when it caused the abort.
    /// The engine surfaces an interrupt as an opaque `InternalError`, which
    /// says nothing about why the script stopped.
    fn report_script_error(&self, error: &JsError) {
        let now = self.state.epoch_now_ms();
        if self.script_budget.tripped() {
            // The aborted script's own frames still name the function that
            // looped, which the opaque `InternalError` never did.
            let ms = self.script_budget.limit.as_millis();
            self.hooks.report_error(ScriptError {
                // Not the engine's placeholder `InternalError`: the frames are
                // the new information, and a driver reading `name` must not be
                // told the abort was an internal error.
                name: None,
                message: format!("script exceeded the {ms} ms execution budget"),
                ..ScriptError::from_js(ScriptErrorKind::ScriptBudget, error, now)
            });
        } else {
            self.hooks
                .report_error(ScriptError::from_js(ScriptErrorKind::Uncaught, error, now));
        }
    }

    /// Runs a GC cycle and processes wrapper finalizations (pin bookkeeping).
    pub fn collect_garbage(&self) {
        // Every world first, then every world's finalizer queue: no value can
        // cross a world (ADR-0033 D1), so a cross-world cycle cannot exist and
        // per-world collection is complete rather than approximate.
        for world in self.worlds.all() {
            world.realm().run_gc();
        }
        self.process_finalized();
    }

    /// Routes each world's finalized wrappers back to **that world's** state.
    ///
    /// `take_finalized` is already per realm, so a slab key can only ever name
    /// the world that minted it; handing world A's queue to world B's state
    /// would free an unrelated object at the same key.
    fn process_finalized(&self) {
        for world in self.worlds.all() {
            let finalized = world.realm().take_finalized();
            if finalized.is_empty() {
                continue;
            }
            let Some(state) = world.state() else { continue };
            oxidepage_bindings::process_finalized(&state, finalized);
        }
    }

    /// Drains console output captured so far.
    pub fn drain_console(&self) -> Vec<ConsoleMessage> {
        drain_stream(&self.hooks.console)
    }

    /// Drains reported script errors (uncaught exceptions in scripts,
    /// listeners, and timer callbacks), plus every promise rejection still
    /// unhandled at this point — the last moment a handler could have attached.
    #[must_use]
    pub fn drain_errors(&self) -> Vec<ScriptError> {
        let mut errors = drain_stream(&self.hooks.errors);
        errors.extend(
            drain_stream(&self.hooks.pending_rejections)
                .into_iter()
                .map(|pending| pending.error),
        );
        errors
    }

    /// Drains the dialog stream: every `alert`/`confirm`/`prompt` the page
    /// opened, with the answer it got.
    #[must_use]
    pub fn drain_dialog_events(&self) -> Vec<DialogEvent> {
        drain_stream(&self.hooks.dialogs)
    }

    /// Installs (or removes) the handler that answers `alert`/`confirm`/
    /// `prompt`. `None` restores the auto-dismiss default.
    ///
    /// For a dialog raised by a *parse-time* inline script this is too late —
    /// `load_html`/`navigate` run those scripts inside the call. Use
    /// [`PageOptions::dialog_handler`] for that case; this mirrors
    /// [`Page::set_viewport`] for a page already alive.
    pub fn set_dialog_handler(&self, handler: Option<DialogHandler>) {
        *self.hooks.dialog_handler.borrow_mut() = handler;
    }

    /// The flag a driver reads to know this page is parked on a dialog.
    ///
    /// Shared, so the answer needs no round trip — which is the point: a page
    /// inside `run_dialog` answers nothing.
    #[must_use]
    pub fn dialog_open_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.hooks.dialog_open)
    }

    /// `Page.setInterceptFileChooserDialog` (ADR-0032 D12).
    pub fn set_intercept_file_chooser(&self, intercept: bool) {
        self.hooks.intercept_file_chooser.set(intercept);
    }

    /// Whether an embedder is listening to this page's events.
    ///
    /// The precondition for request interception: `set_event_sink(None)` also
    /// removes the net observer, so a pause could never be *announced* and
    /// every matching request would wait out the whole timeout with nobody able
    /// to release it. `Fetch.enable` refuses rather than accepting a switch it
    /// knows cannot work.
    #[must_use]
    pub fn has_event_sink(&self) -> bool {
        self.hooks.event_sink.borrow().is_some()
    }

    /// `DOM.setFileInputFiles`: selects `paths` into an `<input type=file>`
    /// (ADR-0032 D11).
    ///
    /// Reads through `std::fs`, **not** `net::file::load_file`. That helper is
    /// gated on `ResourcePolicy::allow_file` — off by default — and is about
    /// *page-initiated* loads. A driver setting file inputs is the operator, so
    /// conflating the two would either break this command on every default
    /// policy or silently widen the page's own `file://` reach.
    ///
    /// The whole file is read into memory: the same is already true of every
    /// response the net stack produces, and a form post has to buffer the body
    /// regardless.
    pub fn set_file_input_files(&self, input: NodeId, paths: &[String]) -> Result<(), JsError> {
        if !self.state.dom.borrow().is_file_input(input) {
            return Err(JsError::Engine(String::from(
                "setFileInputFiles: the node is not an <input type=file>",
            )));
        }
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let path = std::path::Path::new(path);
            let bytes = std::fs::read(path).map_err(|e| {
                JsError::Engine(format!("setFileInputFiles: `{}`: {e}", path.display()))
            })?;
            let name = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            let last_modified = std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| {
                    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
                });
            files.push(oxidepage_dom::SelectedFile {
                content_type: mime_for_path(&name),
                name,
                bytes: std::rc::Rc::new(bytes),
                last_modified,
            });
        }
        // Through the `DomTree` primitive, so the one invalidation code path is
        // preserved: `:valid`/`:invalid` on a `required` file input turn on
        // whether this list is empty.
        if !self.state.dom.borrow_mut().set_selected_files(input, files) {
            return Err(JsError::Engine(String::from(
                "setFileInputFiles: the node is not an <input type=file>",
            )));
        }
        // Trusted `input` then `change`, in that order — what a user selecting
        // files produces, and what every upload widget listens for.
        // Trusted, because the *driver* caused them — the same boundary
        // ADR-0023 drew for synthetic input. A throw from a listener is
        // reported, not propagated: the files are already selected, and failing
        // the command would tell the driver its selection did not happen.
        self.with_cx(|cx| {
            for name in ["input", "change"] {
                if let Err(e) = oxidepage_bindings::fire_trusted_event(cx, input, name, true, false)
                {
                    report_throw(&self.hooks, e);
                }
            }
        });
        Ok(())
    }

    /// `Browser.setDownloadBehavior` (ADR-0032 D13).
    ///
    /// `Deny` is the default and the honest one: with no directory to write to,
    /// a download can only be refused, and parsing an attachment as HTML — what
    /// the engine used to do — silently replaced the document with the bytes of
    /// a PDF.
    pub fn set_download_behavior(&self, behavior: DownloadBehavior) {
        *self.download_behavior.borrow_mut() = behavior;
    }

    /// What an attachment navigation currently does.
    #[must_use]
    pub fn download_behavior(&self) -> DownloadBehavior {
        self.download_behavior.borrow().clone()
    }

    /// A driver's handle on this page's request interception (ADR-0032 D2).
    ///
    /// `Send`, unlike everything else about a page: the config behind it is a
    /// `Mutex` and the decisions travel on a channel, precisely so a driver
    /// thread can turn interception on and resolve a pause without a round trip
    /// through the command port — which would deadlock against the very
    /// navigation whose document fetch is paused.
    #[must_use]
    pub fn intercept(&self) -> oxidepage_net::InterceptControl {
        self.net.intercept()
    }

    /// Installs (or removes) the handler that opens sibling browsing contexts
    /// for `window.open` and `<a target=_blank>`.
    ///
    /// `None` restores the single-browsing-context default: `window.open`
    /// returns `null`, and a targeted link navigates in place with a warning.
    pub fn set_open_window_handler(&self, handler: Option<OpenWindowHandler>) {
        *self.hooks.open_window_handler.borrow_mut() = handler;
    }

    /// True once the `load` event has fired.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.load_fired.get()
    }

    /// Direct read access to the document tree.
    #[must_use]
    pub fn dom(&self) -> std::cell::Ref<'_, DomTree> {
        self.state.dom.borrow()
    }

    /// The document serialized back to HTML (spec `outerHTML` of the document
    /// node, doctype included) — the DOM after every mutation the engine has run
    /// so far, not an echo of the input file.
    #[must_use]
    pub fn document_html(&self) -> String {
        let dom = self.state.dom.borrow();
        oxidepage_dom::serialize::outer_html(&dom, dom.document())
    }

    /// The serialized computed value of `property` for `node` (resolving styles
    /// first). Mirrors `getComputedStyle(node).getPropertyValue(property)`.
    #[must_use]
    pub fn computed_style_value(&self, node: NodeId, property: &str) -> Option<String> {
        let mut dom = self.state.dom.borrow_mut();
        let mut engine = self.state.style.borrow_mut();
        let cv = oxidepage_style::computed_style_for(&mut engine, &mut dom, node, None)?;
        Some(oxidepage_style::serialize_property(&cv, property))
    }

    // === Layout access (Phase 5) ===

    /// Flushes styles and brings the layout up to date (cheap when clean).
    fn flush_layout(&self) {
        self.drain_style_updates();
        let mut dom = self.state.dom.borrow_mut();
        let mut style = self.state.style.borrow_mut();
        self.state.layout.borrow_mut().reflow(&mut dom, &mut style);
    }

    /// The current layout viewport, including its device pixel ratio.
    #[must_use]
    pub fn viewport(&self) -> Viewport {
        self.viewport.get()
    }

    /// Replaces the viewport: media queries re-evaluate and the next layout
    /// read reflows against the new size.
    /// Turns this page's HTTP cache use off (or back on).
    ///
    /// `Network.setCacheDisabled`. The cache itself is shared browser-wide, so
    /// this does not clear it — it makes *this page* neither read from nor
    /// write to it, which is what a driver asks for when it intercepts:
    /// a request served from cache never reaches the interceptor.
    pub fn set_cache_disabled(&self, disabled: bool) {
        self.net.set_cache_disabled(disabled);
    }

    /// Replaces `navigator.language`/`languages` **and** the `Accept-Language`
    /// header, together (ADR-0034 D6).
    ///
    /// Both halves or neither. A locale override that moved only the script
    /// side would leave every request advertising the old locale and every
    /// server answering in it — which is exactly the dishonesty
    /// `Emulation.setUserAgentOverride` is refused for, arrived at by a
    /// different road.
    ///
    /// Held to the same rules an embedder's [`NavigatorProfile`] is, so a
    /// driver cannot install a language list the profile API would reject.
    pub fn set_languages(&self, languages: Vec<String>) -> Result<(), JsError> {
        validate_languages(&languages)?;
        let header = accept_language_for(&languages);
        // The header is validated *before* anything is written, so a value the
        // net stack would refuse leaves both halves untouched rather than the
        // script side alone moved.
        self.net
            .set_accept_language(&header)
            .map_err(|error| JsError::Engine(error.to_string()))?;
        self.state.set_navigator_languages(languages);
        // Every world caches the frozen `languages` array it handed out; each
        // must mint a fresh one or script would keep reading the old list.
        for world in self.worlds.all() {
            if let Some(state) = world.state() {
                state.forget_navigator_caches();
            }
        }
        Ok(())
    }

    pub fn set_viewport(&self, viewport: Viewport) {
        self.viewport.set(viewport);
        self.state.style.borrow_mut().set_viewport(viewport);
        self.state.layout.borrow_mut().set_viewport(viewport);
        self.with_cx(oxidepage_bindings::reevaluate_media_queries);
    }

    /// The border-box rect of `node` in viewport coordinates (reflows
    /// first). Mirrors `node.getBoundingClientRect()` for box-generating
    /// elements.
    #[must_use]
    pub fn layout_rect(&self, node: NodeId) -> Option<Rect> {
        self.flush_layout();
        self.state.layout.borrow().border_box(node)
    }

    /// A debug dump of the box tree with computed layouts (reflows first).
    #[must_use]
    pub fn dump_layout(&self) -> String {
        self.flush_layout();
        self.state.layout.borrow().dump()
    }

    /// The topmost element at viewport coordinates `(x, y)`, per
    /// `document.elementFromPoint` (reflows first).
    #[must_use]
    pub fn element_from_point(&self, x: f32, y: f32) -> Option<NodeId> {
        self.flush_layout();
        let dom = self.state.dom.borrow();
        self.state.layout.borrow().element_from_point(&dom, x, y)
    }

    /// The painted quads of `node` in viewport coordinates: one per client rect
    /// (so one per line for a wrapped inline), each as its four corners in
    /// top-left, top-right, bottom-right, bottom-left order — CDP's
    /// `DOM.getContentQuads`.
    ///
    /// Unlike [`Page::layout_rect`] this is not bounding-boxed, so a rotated or
    /// skewed element reports the quadrilateral an actionability check has to
    /// aim inside of rather than a rect that is mostly empty space.
    #[must_use]
    pub fn content_quads(&self, node: NodeId) -> Vec<[Point; 4]> {
        self.flush_layout();
        let dom = self.state.dom.borrow();
        self.state.layout.borrow().content_quads(&dom, node)
    }

    /// Scrolls `node` into view if it is not already fully visible, aligning
    /// `nearest` on both axes — CDP's `DOM.scrollIntoViewIfNeeded`, and the
    /// second primitive every actionability check is built from. `rect` is an
    /// optional sub-rectangle to reveal, relative to the element's border-box
    /// origin.
    ///
    /// Returns whether anything actually scrolled. Every scroll container on the
    /// chain is scrolled, innermost first, and a `scroll` event is queued for
    /// each one that moved — through the page's own event loop, *after* the
    /// layout borrow is released, because the algorithm itself must never
    /// re-enter JS.
    pub fn scroll_into_view_if_needed(&self, node: NodeId, rect: Option<Rect>) -> bool {
        self.flush_layout();
        let scrolled = {
            let dom = self.state.dom.borrow();
            let mut layout = self.state.layout.borrow_mut();
            oxidepage_layout::scroll_into_view(
                &mut layout,
                &dom,
                node,
                rect,
                oxidepage_layout::Align::Nearest,
                oxidepage_layout::Align::Nearest,
            )
        };
        for target in &scrolled {
            self.state.queue_scroll_event(*target);
        }
        !scrolled.is_empty()
    }
}

/// Every input to [`Page::start_visible_image_loads`]'s gate, read live.
#[derive(Clone, Copy, PartialEq)]
struct LazyScanGate {
    dom_style_version: u64,
    dom_structure_version: u64,
    style_version: u64,
    paint: PaintStamp,
    document_scroll_version: u64,
}

/// Does `rect` (viewport-relative, as `bounding_client_rect` returns it) reach
/// the viewport expanded by `margin` on every side?
///
/// Inclusive on purpose. An image that has not loaded and carries no
/// `width`/`height`/`aspect-ratio` lays out 0×0, and a zero-area rect never
/// *overlaps* anything — a strict test would defer it forever, and it would
/// never load to gain the size that would undefer it.
fn intersects_viewport(rect: Rect, viewport: Viewport, margin: f32) -> bool {
    rect.min_y() <= viewport.height + margin
        && rect.max_y() >= -margin
        && rect.min_x() <= viewport.width + margin
        && rect.max_x() >= -margin
}

/// Reads a no-namespace attribute value off an element node.
fn attr_value(node: &oxidepage_dom::Node, name: &str) -> Option<String> {
    node.as_element()?
        .attrs()
        .iter()
        .find(|a| a.name.ns.is_empty() && &*a.name.local == name)
        .map(|a| a.value.to_string())
}

/// Collects absolute `background-image: url(...)` URLs from a computed style
/// into `out` (used to start background-image loads; WP-L).
fn collect_background_image_urls(style: &style::properties::ComputedValues, out: &mut Vec<String>) {
    for image in &style.get_background().background_image.0 {
        if let style::values::computed::image::Image::Url(url) = image
            && let Some(abs) = url.url()
        {
            out.push(abs.as_str().to_owned());
        }
    }
}

/// The `url()` sources of an `@font-face` rule that we could load, in
/// declaration order — the order the CSS Fonts algorithm tries them in, each
/// falling through to the next when it fails to download or parse.
///
/// `local()` sources and explicitly-unsupported formats (svg, embedded-opentype)
/// are skipped; we support WOFF2/WOFF/TrueType/OpenType and also try sources
/// with an unknown/absent `format(...)` hint (WP-D).
fn supported_font_srcs(
    sources: &[oxidepage_style::FontFaceSource],
) -> impl Iterator<Item = &str> + '_ {
    use oxidepage_style::FontFormatHint;

    sources.iter().filter_map(|source| {
        let url = source.url.as_deref()?;
        let format = source.format.or_else(|| format_from_ext(url));
        match format {
            // Supported, or an unknown/absent hint we still attempt.
            Some(
                FontFormatHint::Woff2
                | FontFormatHint::Woff
                | FontFormatHint::Truetype
                | FontFormatHint::Opentype,
            )
            | None => Some(url),
            // Explicitly unsupported (svg, embedded-opentype, …).
            Some(FontFormatHint::Other) => None,
        }
    })
}

/// Guesses a font format from a URL's file extension (`.woff2`/`.woff`/`.ttf`/
/// `.otf`), for `src:` entries that omit a `format(...)` hint.
fn format_from_ext(url: &str) -> Option<oxidepage_style::FontFormatHint> {
    use oxidepage_style::FontFormatHint;
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next()?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "woff2" => FontFormatHint::Woff2,
        "woff" => FontFormatHint::Woff,
        "ttf" => FontFormatHint::Truetype,
        "otf" => FontFormatHint::Opentype,
        _ => return None,
    })
}

/// Extracts the `Content-Type` header value from a header list.
fn header_content_type(headers: &[(String, String)]) -> Option<String> {
    header_value(headers, "content-type")
}

/// A header value by case-insensitive name.
fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// A MIME type guessed from a filename extension.
///
/// Deliberately a short table rather than a dependency: the caller is an
/// embedder command, the value lands in a multipart `Content-Type`, and
/// `application/octet-stream` is the correct answer for anything unrecognised
/// — the Fetch multipart serializer says so.
fn mime_for_path(name: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let mime = match extension.as_str() {
        "txt" | "text" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    };
    mime.to_owned()
}

/// Mints a download guid, unique across every page in the process.
///
/// Process-wide rather than per-`Page`, which is what Chrome's UUID is in
/// effect: a driver keys downloads **browser-wide** by guid — Playwright's
/// `_onDownloadCreated` stores them on the browser, not the page — so two pages
/// each counting from 1 hand it the same key twice, and the second page's
/// terminal event completes the first page's entry.
fn next_download_guid() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "dl-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// The last path segment of a URL, as a filename.
fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.next_back()?;
    // Percent-decoded so `my%20report.pdf` lands as `my report.pdf`, and
    // re-sanitized because decoding can reintroduce a separator.
    let decoded = percent_decode_path(last);
    oxidepage_net::sanitize_filename(&decoded)
}

fn percent_decode_path(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Writes a download into `directory`, without overwriting anything.
///
/// The filename is attacker-influenced, so this is defended twice: `net`'s
/// [`oxidepage_net::sanitize_filename`] has already removed every path
/// separator, and the joined path is checked to still be *inside* the directory
/// before a byte is written. A name that is already taken gets a ` (n)` suffix
/// rather than replacing the file — a page must not be able to overwrite an
/// earlier download by naming it.
fn write_download(
    directory: &std::path::Path,
    filename: &str,
    bytes: &[u8],
) -> Result<std::path::PathBuf, String> {
    // `std::fs`, deliberately, not `net::file::load_file`: that is gated on
    // `ResourcePolicy::allow_file` and is about *page-initiated* loads. A
    // download directory is the operator's choice, and conflating the two
    // would either break this or silently widen the page's own `file://`
    // reach (ADR-0032 D11's reasoning, applied the other way round).
    std::fs::create_dir_all(directory)
        .map_err(|e| format!("cannot create `{}`: {e}", directory.display()))?;
    let canonical = directory
        .canonicalize()
        .map_err(|e| format!("cannot resolve `{}`: {e}", directory.display()))?;

    let (stem, extension) = match filename.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (filename, String::new()),
    };
    for attempt in 0..MAX_DOWNLOAD_NAME_ATTEMPTS {
        let candidate = if attempt == 0 {
            filename.to_owned()
        } else {
            format!("{stem} ({attempt}){extension}")
        };
        let path = canonical.join(&candidate);
        // The separator strip should make this unreachable; it is here because
        // "should" is not a security property.
        if path.parent() != Some(canonical.as_path()) {
            return Err(String::from("the download filename escaped the directory"));
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes)
                    .map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create `{}`: {e}", path.display())),
        }
    }
    Err(format!(
        "`{filename}` and its first {MAX_DOWNLOAD_NAME_ATTEMPTS} alternatives are all taken"
    ))
}

/// Extracts the `charset` parameter from a `Content-Type` value.
fn content_type_charset(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("charset=")
            .map(|c| c.trim().trim_matches('"').to_owned())
    })
}

/// Reports a throw escaping a lifecycle-event dispatch (`load`, `popstate`,
/// `scroll`, a synthesized input sequence).
///
/// `Callback`, not `Resource`: page script ran and raised this, even in the
/// `JsThrow::Value` arm where the value cannot be structured from here — the
/// dispatch helpers hand back a `JsThrow`, not a caught `JsError`.
fn report_throw(hooks: &LoopHooks, throw: oxidepage_js::JsThrow) {
    let message = match throw {
        oxidepage_js::JsThrow::Type(m) | oxidepage_js::JsThrow::Range(m) => m,
        oxidepage_js::JsThrow::Value(_) => "exception while firing a lifecycle event".to_owned(),
    };
    hooks.report_error(ScriptError::engine(
        ScriptErrorKind::Callback,
        message,
        hooks.now_ms(),
    ));
}

/// Convenience: build a page and load `html` in one call.
pub fn load_html_page(html: &str, options: PageOptions) -> Result<Page, JsError> {
    let page = Page::new(options)?;
    page.load_html(html)?;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_TIMER_NESTING, MIN_NESTED_TIMER_DELAY, clamp_timer_delay, format_from_ext,
        is_classic_script_type, supported_font_srcs,
    };
    use oxidepage_style::{FontFaceSource, FontFormatHint};
    use std::time::Duration;

    #[test]
    fn clamp_timer_delay_handles_non_finite_and_overflow() {
        // Non-finite delays coerce to 0 (HTML `long`/ToInt32 semantics), never
        // panicking `Duration::from_secs_f64`.
        assert_eq!(clamp_timer_delay(f64::INFINITY, 1), Duration::ZERO);
        assert_eq!(clamp_timer_delay(f64::NEG_INFINITY, 1), Duration::ZERO);
        assert_eq!(clamp_timer_delay(f64::NAN, 1), Duration::ZERO);
        assert_eq!(clamp_timer_delay(-5.0, 1), Duration::ZERO);
        // A finite value far past i32::MAX ms is clamped, not overflowed.
        assert_eq!(
            clamp_timer_delay(1e15, 1),
            Duration::from_millis(i32::MAX as u64)
        );
    }

    #[test]
    fn clamp_timer_delay_floors_nested_timers() {
        // A shallow zero-delay timer stays immediate.
        assert_eq!(clamp_timer_delay(0.0, 1), Duration::ZERO);
        assert_eq!(clamp_timer_delay(0.0, MAX_TIMER_NESTING), Duration::ZERO);
        // Past the nesting threshold, a sub-4ms delay is raised to 4ms so a
        // self-reposting zero-delay timer stops being immediately due.
        assert_eq!(
            clamp_timer_delay(0.0, MAX_TIMER_NESTING + 1),
            MIN_NESTED_TIMER_DELAY
        );
        // A delay already ≥ 4ms is unaffected by the nesting floor.
        assert_eq!(
            clamp_timer_delay(10.0, MAX_TIMER_NESTING + 1),
            Duration::from_millis(10)
        );
    }

    fn src(
        url: Option<&str>,
        local: Option<&str>,
        format: Option<FontFormatHint>,
    ) -> FontFaceSource {
        FontFaceSource {
            url: url.map(str::to_owned),
            local: local.map(str::to_owned),
            format,
        }
    }

    #[test]
    fn supported_font_srcs_keeps_declaration_order() {
        // A `.ttf` declared before a `.woff2`: the CSS algorithm tries the first
        // supported source (the ttf) first, not the "preferred" woff2. The woff2
        // remains as the fallback if the ttf fails.
        let sources = vec![
            src(Some("a.ttf"), None, Some(FontFormatHint::Truetype)),
            src(Some("b.woff2"), None, Some(FontFormatHint::Woff2)),
        ];
        assert_eq!(
            supported_font_srcs(&sources).collect::<Vec<_>>(),
            ["a.ttf", "b.woff2"]
        );
    }

    #[test]
    fn supported_font_srcs_skips_local_and_unsupported_formats() {
        let sources = vec![
            src(None, Some("Local Font"), None), // local(): no url
            src(Some("i.svg"), None, Some(FontFormatHint::Other)), // svg: unsupported
            src(Some("c.woff"), None, Some(FontFormatHint::Woff)), // first supported
            src(Some("d.otf"), None, Some(FontFormatHint::Opentype)), // its fallback
        ];
        assert_eq!(
            supported_font_srcs(&sources).collect::<Vec<_>>(),
            ["c.woff", "d.otf"]
        );
    }

    #[test]
    fn format_from_ext_reads_the_extension() {
        assert_eq!(
            format_from_ext("https://x/y.woff2?v=1"),
            Some(FontFormatHint::Woff2)
        );
        assert_eq!(format_from_ext("x.ttf"), Some(FontFormatHint::Truetype));
        assert_eq!(format_from_ext("x.png"), None);
    }

    #[test]
    fn classic_script_type_accepts_standard_legacy_javascript_mimes() {
        assert!(is_classic_script_type(None));
        assert!(is_classic_script_type(Some("text/javascript")));
        assert!(is_classic_script_type(Some("application/x-javascript")));
        assert!(is_classic_script_type(Some("text/javascript1.5")));
        assert!(!is_classic_script_type(Some("module")));
        assert!(!is_classic_script_type(Some("application/json")));
    }
}
