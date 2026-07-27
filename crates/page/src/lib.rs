//! The page: event loop v2 and document lifecycle (design doc §5.4, §6).
//!
//! One [`Page`] owns one realm, one document, one single-threaded scheduler,
//! and one [`NetService`] (a tokio runtime living on the page thread). The
//! loop implements the HTML event-loop skeleton: a timer min-heap, microtask
//! checkpoints after every task and callback into JS, GC-finalization
//! processing between tasks, and — new in Phase 3 — a networking task source.
//!
//! `Receiver::recv_deadline` unifies "a net event OR the next timer deadline
//! OR the settle budget" into one blocking wait ([`Page::settle`]), so timers
//! and network both progress with no busy-wait (ADR-0004).
//!
//! Document loading is a streaming parse with script execution at
//! `</script>` suspension points. Classic inline scripts run immediately;
//! parser-blocking external scripts fetch synchronously and block the parse;
//! `defer` and module scripts run in order after parsing, before
//! `DOMContentLoaded`; `async` scripts run on arrival. `document.write`
//! remains unsupported (design §12).

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use oxidepage_base::{NodeId, Rect, RequestId};
use oxidepage_bindings::{
    BindCx, ConsoleLevel, EventTargetKey, HostHooks, NavigationBody, PageState, PendingNavigation,
    TimingMilestone, is_classic_script_type,
};
use oxidepage_dom::{DomTree, ParseOptions, ParseSignal, Parser, StyleUpdate};
use oxidepage_js::{
    JsEngine, JsError, JsRealm, JsValue, ModuleSource, PromiseState, QuickJsEngine, QuickJsRealm,
    RealmOptions,
};
use oxidepage_layout::{LayoutEngine, PaintStamp};
use oxidepage_net::{NetEvent, NetRequest, NetService, decode_charset};
use oxidepage_style::{BlockingImportLoader, CssFetcher, StyleEngine};
use style::selector_parser::PseudoElement;
use style::stylesheets::Origin;

mod render;

pub use oxidepage_layout::disable_system_fonts;
pub use oxidepage_net::ResourcePolicy;
pub use oxidepage_paint::DisplayList;
pub use oxidepage_raster_skia::{RasterImage, RasterOptions};
pub use oxidepage_style::Viewport;

/// Wall-clock cap on waiting for subresources (async scripts, etc.) before
/// firing `load` regardless.
const SUBRESOURCE_BUDGET: Duration = Duration::from_secs(30);

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

/// A console line captured from page script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleMessage {
    pub level: ConsoleLevel,
    pub message: String,
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
}

/// How far a [`Page::navigate`] waits before returning.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WaitUntil {
    /// Return once `DOMContentLoaded` has fired (parse + deferred scripts).
    DomContentLoaded,
    /// Return once `load` has fired (subresources settled).
    #[default]
    Load,
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

impl NavigatorProfile {
    fn validate(&self) -> Result<(), JsError> {
        if self.user_agent.chars().any(char::is_control) {
            return Err(JsError::Engine(
                "navigator.userAgent must not contain control characters".into(),
            ));
        }
        if self.languages.is_empty() || self.languages.len() > 10 {
            return Err(JsError::Engine(
                "navigator.languages must contain between 1 and 10 entries".into(),
            ));
        }
        if self.hardware_concurrency == 0 {
            return Err(JsError::Engine(
                "navigator.hardwareConcurrency must be greater than zero".into(),
            ));
        }
        if self.languages.iter().any(|language| {
            language.is_empty()
                || language
                    .split('-')
                    .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric()))
        }) {
            return Err(JsError::Engine(
                "navigator.languages contains an invalid language tag".into(),
            ));
        }
        oxidepage_net::RequestDefaults::new(&self.user_agent, &self.accept_language())
            .map_err(|error| JsError::Engine(format!("invalid navigator profile: {error}")))?;
        Ok(())
    }

    fn accept_language(&self) -> String {
        self.languages
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

/// Scheduler state shared with the bindings through [`HostHooks`].
struct LoopHooks {
    timers: RefCell<BinaryHeap<Reverse<Timer>>>,
    cleared: RefCell<HashSet<u64>>,
    next_timer_id: Cell<u64>,
    next_seq: Cell<u64>,
    /// Pending `requestAnimationFrame` callbacks in registration order, fired
    /// (and drained) at each rendering opportunity (Phase 6, ADR-0007 D8).
    raf_callbacks: RefCell<Vec<(u64, JsValue)>>,
    next_raf_id: Cell<u64>,
    raf_cancelled: RefCell<HashSet<u64>>,
    /// Nesting level of the timer whose callback is currently running (0 when
    /// no timer callback is on the stack). Timers scheduled during a callback
    /// inherit `current + 1`.
    timer_nesting: Cell<u32>,
    console: RefCell<Vec<ConsoleMessage>>,
    errors: RefCell<Vec<String>>,
    /// Rejected promises that have no handler *yet*, rendered. A rejection is
    /// only an error if nothing ever handles it, and a handler may attach long
    /// after the rejection (`p = fetch(…)` now, `p.catch(…)` next tick), so
    /// these are held here and reported only if they survive to
    /// [`Page::drain_errors`]. Keyed by rendered reason: the engine-neutral
    /// tracker carries no promise identity, and two rejections that render
    /// identically are indistinguishable anyway.
    pending_rejections: RefCell<Vec<String>>,
    /// The net service, installed after the hooks (which the bindings
    /// captured) are created.
    net: RefCell<Option<Rc<NetService>>>,
}

impl Default for LoopHooks {
    fn default() -> Self {
        Self {
            timers: RefCell::new(BinaryHeap::new()),
            cleared: RefCell::new(HashSet::new()),
            next_timer_id: Cell::new(1),
            next_seq: Cell::new(1),
            raf_callbacks: RefCell::new(Vec::new()),
            next_raf_id: Cell::new(1),
            raf_cancelled: RefCell::new(HashSet::new()),
            timer_nesting: Cell::new(0),
            console: RefCell::new(Vec::new()),
            errors: RefCell::new(Vec::new()),
            pending_rejections: RefCell::new(Vec::new()),
            net: RefCell::new(None),
        }
    }
}

impl LoopHooks {
    fn set_net(&self, net: Rc<NetService>) {
        *self.net.borrow_mut() = Some(net);
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

    /// True while animation-frame callbacks are pending.
    fn has_pending_raf(&self) -> bool {
        !self.raf_callbacks.borrow().is_empty()
    }

    /// Removes and returns the current animation-frame callbacks. Callbacks
    /// registered during firing accumulate in the freshly emptied list and run
    /// at the next opportunity (spec behavior).
    fn take_raf_callbacks(&self) -> Vec<(u64, JsValue)> {
        std::mem::take(&mut self.raf_callbacks.borrow_mut())
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

impl HostHooks for LoopHooks {
    fn console_message(&self, level: ConsoleLevel, message: String) {
        self.console
            .borrow_mut()
            .push(ConsoleMessage { level, message });
    }

    fn report_error(&self, message: String) {
        self.errors.borrow_mut().push(message);
    }

    fn schedule_timer(
        &self,
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

    fn request_animation_frame(&self, callback: JsValue) -> f64 {
        let id = self.next_raf_id.get();
        self.next_raf_id.set(id + 1);
        self.raf_callbacks.borrow_mut().push((id, callback));
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
                    .map(|(id, _)| *id)
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
            .fetch_blocking(NetRequest::subresource(url.as_str(), self.doc_url.clone()))
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
    // Field order = drop order: bindings state and hooks own persistent JS
    // references and must drop before the realm; the realm's module loader
    // holds an `Rc<NetService>`, so `net` (also an `Rc`) keeps the service
    // alive until after the realm's teardown.
    state: Rc<PageState>,
    hooks: Rc<LoopHooks>,
    realm: QuickJsRealm,
    net: Rc<NetService>,
    net_rx: Receiver<NetEvent>,
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
    navigation_events: RefCell<Vec<NavigationEvent>>,
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
        {
            // A promise that rejects with nobody listening is how a broken page
            // fails *silently*: the module evaluated, the app never mounted, and
            // nothing reached the console. Browsers surface these as
            // `unhandledrejection`; a headless engine has no console to notice
            // them in, so they become reported errors instead.
            let hooks = Rc::clone(&hooks);
            realm.set_rejection_tracker(Some(Box::new(move |reason, is_handled| {
                let mut pending = hooks.pending_rejections.borrow_mut();
                if is_handled {
                    // A handler attached after the fact: retract the rejection.
                    if let Some(at) = pending.iter().position(|r| *r == reason) {
                        pending.remove(at);
                    }
                } else {
                    pending.push(reason);
                }
            })));
        }
        let state = oxidepage_bindings::install_with_profiles(
            &realm,
            dom,
            Rc::clone(&hooks) as Rc<dyn HostHooks>,
            viewport,
            navigator_data,
            screen_data,
        )?;
        state.set_whole_document_visible(whole_document_visible);

        let policy = policy.unwrap_or_default();
        let (net, net_rx) = NetService::new_with_defaults(policy, request_defaults)
            .map_err(|e| JsError::Engine(e.to_string()))?;
        let net = Rc::new(net);
        hooks.set_net(Rc::clone(&net));
        realm.set_module_loader(Rc::new(ModuleLoader {
            net: Rc::clone(&net),
            dom: Rc::clone(&state.dom),
        }));

        Ok(Self {
            state,
            hooks,
            realm,
            net,
            net_rx,
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
            navigating: Cell::new(false),
            navigation_events: RefCell::new(Vec::new()),
            viewport: Cell::new(viewport),
            render: render::RenderState::default(),
            start_time: Cell::new(Instant::now()),
            next_render_at: Cell::new(Instant::now()),
            script_budget,
        })
    }

    /// Loads an in-memory HTML document as the current document (the document
    /// URL is whatever was configured). Scripts run per the Phase 3 timing
    /// rules; external references resolve against the document URL and load
    /// over the net stack.
    ///
    /// A script in the loaded document may navigate; that navigation is chained
    /// off this call, exactly as it would be off a `navigate`.
    pub fn load_html(&self, html: &str) -> Result<(), JsError> {
        let url = self.state.dom.borrow().document_url().to_owned();
        self.commit_history(url, HistoryTarget::Replace);
        self.load_document(html, WaitUntil::Load)?;
        self.run_chained_navigations(WaitUntil::Load);
        Ok(())
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
            },
            wait_until,
            /* embedder */ true,
        )
    }

    /// Drains the navigation milestone stream (see [`NavigationEvent`]).
    #[must_use]
    pub fn drain_navigation_events(&self) -> Vec<NavigationEvent> {
        std::mem::take(&mut self.navigation_events.borrow_mut())
    }

    fn record_navigation(&self, kind: NavigationEventKind, url: &str, error: Option<String>) {
        self.navigation_events.borrow_mut().push(NavigationEvent {
            kind,
            url: url.to_owned(),
            error,
            timestamp: self.state.epoch_now_ms(),
        });
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
                    .console_message(ConsoleLevel::Error, message.clone());
                self.record_navigation(NavigationEventKind::Failed, &url, Some(message));
                return Ok(());
            }
            match navigation {
                PendingNavigation::Load {
                    url,
                    replace,
                    body,
                    reload,
                } => {
                    let target = if replace {
                        HistoryTarget::Replace
                    } else {
                        HistoryTarget::Push
                    };
                    // A fragment-only change, a form POST and a reload are three
                    // different things and only the first stays in the document.
                    if body.is_none() && !reload && self.is_same_document(&url) {
                        self.commit_same_document(&url, target, None);
                    } else {
                        self.commit_document(&url, target, body, reload, wait_until, embedder)?;
                    }
                }
                PendingNavigation::Traverse { delta } => {
                    self.commit_traversal(delta, wait_until, embedder)?;
                }
            }
            pending = self.state.take_pending_navigation();
        }
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
    fn commit_same_document(&self, url: &str, target: HistoryTarget, popstate: Option<JsValue>) {
        let previous = self.state.dom.borrow().document_url().to_owned();
        self.state.dom.borrow_mut().set_document_url(url.to_owned());
        {
            let mut history = self.state.history();
            let seq = history.document_seq();
            match target {
                HistoryTarget::Push => history.push(url.to_owned(), JsValue::Null, seq),
                HistoryTarget::Replace => history.replace(url.to_owned(), JsValue::Null, seq),
                HistoryTarget::Traverse(index) => history.set_index(index),
            }
        }
        self.record_navigation(NavigationEventKind::SameDocument, url, None);
        self.scroll_to_fragment(url);

        if let Some(state) = popstate {
            self.with_cx(|cx| {
                if let Err(e) = oxidepage_bindings::fire_pop_state(cx, state) {
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
        body: Option<NavigationBody>,
        reload: bool,
        wait_until: WaitUntil,
        embedder: bool,
    ) -> Result<(), JsError> {
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
        let outcome = match self.net.fetch_blocking(request) {
            Ok(outcome) => outcome,
            Err(error) => {
                let message = error.to_string();
                self.record_navigation(NavigationEventKind::Failed, url, Some(message.clone()));
                if embedder {
                    return Err(JsError::Engine(message));
                }
                // A failed script-initiated navigation keeps the current
                // document — the page is not blanked, it simply did not move.
                self.hooks.console_message(
                    ConsoleLevel::Error,
                    format!("navigation to `{url}` failed: {message}"),
                );
                return Ok(());
            }
        };

        let final_url = outcome.head.final_url.clone();
        self.state.set_referrer(referrer.unwrap_or_default());
        self.state
            .dom
            .borrow_mut()
            .set_document_url(final_url.clone());
        self.commit_history(final_url.clone(), target);
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
        self.commit_document(
            &url,
            HistoryTarget::Traverse(index),
            None,
            /* reload */ false,
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
            HistoryTarget::Push if seq > 1 => history.push(url, JsValue::Null, seq),
            HistoryTarget::Push | HistoryTarget::Replace => {
                history.replace(url, JsValue::Null, seq);
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
            // promises must never resolve into the new one.
            .chain(self.state.reset_for_navigation())
            .collect();
        for id in abort_ids {
            self.net.abort(id);
        }
        // Timers, intervals and animation frames belong to the old document too.
        self.hooks.reset_for_navigation();
        self.deferred.borrow_mut().clear();
        self.ordered_dynamic_ready.borrow_mut().clear();
        self.next_dynamic_order.set(0);
        self.next_dynamic_to_run.set(0);
        self.in_flight.set(0);
        self.pending_stylesheets.set(0);
        self.link_sheets.borrow_mut().clear();
        self.requested_images.borrow_mut().clear();
        // The deferred nodes belong to the outgoing document; their ids go stale
        // with it, and touching a stale id panics.
        self.deferred_images.borrow_mut().clear();
        self.last_lazy_scan.set(None);
        self.requested_fonts.borrow_mut().clear();
        self.last_fontface_scan.set(u64::MAX);
        self.last_bg_scan.set((u64::MAX, u64::MAX));
        self.last_inline_svg_scan.set((u64::MAX, u64::MAX));
        self.load_fired.set(false);
        self.render.reset();
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
    /// readiness"); `PageState::mark_timing` has already moved the state, so a
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
                        .report_error(format!("cannot resolve module src `{src}`")),
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
                .report_error(format!("cannot resolve script src `{src}`"));
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
            .start_resource(NetRequest::subresource(&url, doc_url));
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
                        Ok(out) => self
                            .hooks
                            .report_error(format!("module `{url}`: HTTP {}", out.head.status)),
                        Err(e) => self.hooks.report_error(format!("module `{url}`: {e}")),
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
            .fetch_blocking(NetRequest::subresource(url, doc_url))
        {
            Ok(out) if out.head.status < 400 => {
                let ct = header_content_type(&out.head.headers);
                let source = decode_charset(&out.body, ct.as_deref());
                let _ = self.eval_classic(&source, url, node);
            }
            Ok(out) => self
                .hooks
                .report_error(format!("script `{url}`: HTTP {}", out.head.status)),
            Err(e) => self.hooks.report_error(format!("script `{url}`: {e}")),
        }
    }

    fn eval_classic(&self, source: &str, url: &str, node: Option<NodeId>) -> bool {
        let previous = self.state.current_script.replace(node);
        self.with_cx(|cx| {
            let result = cx.scope.eval(source, url);
            // `document.currentScript` is null in promise reactions and other
            // microtasks queued by the script, so restore before the checkpoint.
            cx.state.current_script.set(previous);
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
                            .report_error(format!("module `{url}` evaluation rejected")),
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
                .report_error(format!("cannot resolve dynamic script src `{src}`"));
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
                self.hooks.report_error(message);
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

    /// [`Self::run_until_stalled`] bounded by a wall-clock `deadline`. The
    /// nesting clamp already keeps a zero-delay self-reposting timer from
    /// spinning; this deadline is a hard backstop so no now-runnable workload
    /// (timers, floods of net events) can keep the loop from returning.
    fn run_until_stalled_until(&self, deadline: Instant) {
        loop {
            self.process_finalized();
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
                // The document is gone; restart the drain order against the
                // new one rather than finishing this pass over the old.
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
            // Script scrolls queue `scroll` events; dispatch them as tasks.
            progressed |= self.drain_scroll_events();
            // Deliver ResizeObserver/IntersectionObserver notifications. Driven
            // here (not in `update_the_rendering`, which only runs with a
            // pending rAF, nor in every microtask checkpoint) so the initial
            // delivery lands before `settle()` returns and RO-mutation chains
            // converge across loop iterations (ADR-0011).
            progressed |= self.deliver_observations();
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
    /// `recv_deadline` is the crux: one blocking wait that wakes on a net
    /// event OR the next timer deadline OR the budget end.
    pub fn settle(&self, budget: Duration) {
        let end = Instant::now() + budget;
        loop {
            self.run_until_stalled_until(end);
            let next_timer = self.hooks.next_deadline();
            let pending_raf = self.hooks.has_pending_raf();
            if next_timer.is_none()
                && !pending_raf
                && self.in_flight.get() == 0
                && self.state.net_pending() == 0
            {
                // Idle reached rather than the budget running out — the
                // distinction a `NetworkIdle` milestone is there to record.
                self.record_document_milestone(NavigationEventKind::NetworkIdle);
                return;
            }
            let now = Instant::now();
            if now >= end {
                return;
            }
            // Wake at the earliest of: the next timer, the next rendering
            // opportunity (bounding any endless rAF loop by the budget), or the
            // budget end.
            let render_at = pending_raf.then(|| self.next_render_at.get());
            let deadline = [next_timer, render_at, Some(end)]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(end);
            // On a deadline timeout the loop re-runs any now-due work.
            if let Ok(event) = self.net_rx.recv_deadline(deadline) {
                self.dispatch_net_event(event);
            }
        }
    }

    /// Blocks (bounded by `budget`) until no network request is in flight.
    fn await_subresources(&self, budget: Duration) {
        let end = Instant::now() + budget;
        loop {
            self.run_until_stalled_until(end);
            if self.in_flight.get() == 0 {
                return;
            }
            let now = Instant::now();
            if now >= end {
                return;
            }
            let deadline = self.hooks.next_deadline().map_or(end, |t| t.min(end));
            if let Ok(event) = self.net_rx.recv_deadline(deadline) {
                self.dispatch_net_event(event);
            }
        }
    }

    /// Routes a net event to its handler: page-level async scripts stay here;
    /// everything else (fetch/XHR) is delivered to the bindings.
    fn dispatch_net_event(&self, event: NetEvent) {
        let id = event.request_id();
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
        self.with_cx(|cx| oxidepage_bindings::deliver_net_event(cx, event));
        if terminal {
            self.net.finish(id);
        }
    }

    /// Delivers any queued custom-element reactions (constructor upgrades and
    /// lifecycle callbacks) and runs a microtask checkpoint. During parsing,
    /// elements are created/connected between scripts without a checkpoint, so
    /// this must be pumped explicitly. Returns whether any reaction ran.
    fn drain_custom_element_reactions(&self) -> bool {
        self.with_cx(|cx| {
            let reacted = oxidepage_bindings::drain_custom_element_reactions(cx);
            // Apply connect/disconnect wrapper-retention changes in the same
            // step: adds preserve expando state, drops let detached trees free.
            oxidepage_bindings::drain_pinned_connectivity(cx);
            oxidepage_bindings::microtask_checkpoint(cx);
            reacted
        })
    }

    // === Images (Phase 6, WP-K) ===

    /// Starts loads for every `<img>` queued by the DOM (connect / `src`
    /// change). Loads are deduplicated by absolute URL and their `in_flight`
    /// count makes `load`/`settle` wait for them.
    fn drain_image_updates(&self) {
        let updates = self.state.dom.borrow_mut().take_image_updates();
        for node in updates {
            self.start_image_load(node);
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
        let (src, loading) = {
            let dom = self.state.dom.borrow();
            // The queued id is a snapshot taken before this drain: replacing a
            // subtree (`innerHTML`) frees the `<img>` outright, so the id can be
            // stale, not merely disconnected.
            let Some(el) = dom.get(node).filter(|n| n.is_connected()) else {
                return;
            };
            (attr_value(el, "src"), attr_value(el, "loading"))
        };
        let Some(src) = src.filter(|s| !s.is_empty()) else {
            return;
        };
        let Some(url) = self.resolve_url(&src) else {
            return;
        };
        // A URL the store has already resolved fires its event right now. The
        // load that filled it is long finished, and `requested_images` means no
        // second request will ever be made for it — so an element registered as
        // a waiter here would wait forever. (Two `<img>` with the same `src` is
        // the ordinary case, not a corner one.)
        let settled = {
            let layout = self.state.layout.borrow();
            let images = layout.images();
            if images.get(&url).is_some() {
                Some("load")
            } else if images.is_broken(&url) {
                Some("error")
            } else {
                None
            }
        };
        if let Some(event) = settled {
            self.fire_element_event(node, event);
            return;
        }
        self.image_waiters
            .borrow_mut()
            .entry(url.clone())
            .or_default()
            .push(node);

        // `data:` decodes inline (no network to save), and `loading="eager"` is
        // the author asking for exactly that: no deferral.
        let eager = url.starts_with("data:") || loading.as_deref() == Some("eager");
        if self.lazy_images.get() && !eager {
            self.deferred_images.borrow_mut().insert(node);
            return;
        }
        // Undeferred (`loading` flipped to `eager`, or a `src` change on an
        // already-deferred node): it loads now, so it leaves the queue.
        self.deferred_images.borrow_mut().remove(&node);
        self.start_image_load_url(&url);
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
                self.start_image_load_url(&url);
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

        // `data:` URLs decode inline (no network).
        if let Some(rest) = url.strip_prefix("data:") {
            match decode_data_url(rest) {
                Some((bytes, content_type)) => {
                    self.finish_image(url, &bytes, content_type.as_deref());
                }
                None => self.mark_image_broken(url),
            }
            return;
        }

        let doc_url = self.state.dom.borrow().document_url().to_owned();
        let id = self
            .net
            .start_resource(NetRequest::subresource(url, doc_url));
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
                        .report_error(format!("image `{}`: {error}", pending.url));
                    self.mark_image_broken(&pending.url);
                    self.decr_in_flight();
                    self.net.finish(id);
                }
            }
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
                let decoded = decode_data_url(rest)
                    .is_some_and(|(bytes, _content_type)| self.finish_font(family, &bytes, attrs));
                if decoded {
                    return;
                }
                remaining = fallbacks;
                continue;
            }

            let doc_url = self.state.dom.borrow().document_url().to_owned();
            let id = self
                .net
                .start_resource(NetRequest::subresource(url, doc_url));
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
            self.state.fonts_loading.set(true);
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
                        .report_error(format!("web font `{}`: {error}", pending.url));
                    self.start_font_load(&pending.family, &pending.fallbacks, pending.attrs);
                    self.decr_in_flight();
                    self.net.finish(id);
                    self.settle_font_ready();
                }
            }
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
        self.state.fonts_loading.set(loading);
        if loading || self.state.ready_state() == oxidepage_bindings::ReadyState::Loading {
            return;
        }
        self.with_cx(oxidepage_bindings::resolve_font_ready);
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
                            Err(message) => self.hooks.report_error(message),
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
                        self.hooks.report_error(match completed.result {
                            Ok(_) => unreachable!(),
                            Err(message) => message,
                        });
                    }
                }
            }
        }
    }

    fn decr_in_flight(&self) {
        self.in_flight.set(self.in_flight.get().saturating_sub(1));
    }

    // === Style loading (Phase 4) ===

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
            oxidepage_bindings::microtask_checkpoint(cx);
        });
        true
    }

    /// Delivers pending ResizeObserver/IntersectionObserver notifications
    /// (a `with_cx` wrapper over the bindings' `deliver_observations`).
    fn deliver_observations(&self) -> bool {
        self.with_cx(oxidepage_bindings::deliver_observations)
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
                .report_error(format!("cannot resolve stylesheet href `{href}`"));
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

        let id = self
            .net
            .start_resource(NetRequest::subresource(&url, doc_url));
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
                        self.hooks.report_error(format!(
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
                        .report_error(format!("stylesheet `{}`: {error}", pending.url));
                    self.decr_stylesheet(id);
                    self.fire_element_event(pending.node, "error");
                }
            }
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
        let end = Instant::now() + budget;
        loop {
            self.run_until_stalled_until(end);
            if self.pending_stylesheets.get() == 0 {
                return;
            }
            let now = Instant::now();
            if now >= end {
                return;
            }
            let deadline = self.hooks.next_deadline().map_or(end, |t| t.min(end));
            if let Ok(event) = self.net_rx.recv_deadline(deadline) {
                self.dispatch_net_event(event);
            }
        }
    }

    fn fire_timer(&self, timer: Timer) {
        // Expose this timer's nesting level so timers it schedules inherit
        // `current + 1` (HTML timer initialization steps).
        let prev_nesting = self.hooks.timer_nesting.replace(timer.nesting);
        self.with_cx(|cx| {
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

    /// Enters JS with the per-task script budget armed (a no-op when an outer
    /// `with_cx` already armed it, so a task keeps one deadline throughout).
    fn with_cx<T>(&self, f: impl FnOnce(&BindCx<'_>) -> T) -> T {
        let owns_budget = self.script_budget.arm();
        let result = self.realm.with_scope(|scope| {
            let cx = BindCx {
                scope,
                state: Rc::clone(&self.state),
            };
            // The only entry into JS from the event loop, so this is where the
            // parser's DOM mutations become visible to Window named access.
            // A no-op unless the document's element ids changed.
            if let Err(error) = oxidepage_bindings::sync_named_properties(&cx) {
                self.hooks
                    .report_error(format!("failed to sync window named properties: {error:?}"));
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
        if self.script_budget.tripped() {
            let ms = self.script_budget.limit.as_millis();
            self.hooks
                .report_error(format!("script exceeded the {ms} ms execution budget"));
        } else {
            self.hooks.report_error(error.to_string());
        }
    }

    /// Runs a GC cycle and processes wrapper finalizations (pin bookkeeping).
    pub fn collect_garbage(&self) {
        self.realm.run_gc();
        self.process_finalized();
    }

    fn process_finalized(&self) {
        let finalized = self.realm.take_finalized();
        if !finalized.is_empty() {
            oxidepage_bindings::process_finalized(&self.state, finalized);
        }
    }

    /// Drains console output captured so far.
    pub fn drain_console(&self) -> Vec<ConsoleMessage> {
        std::mem::take(&mut self.hooks.console.borrow_mut())
    }

    /// Drains reported script errors (uncaught exceptions in scripts,
    /// listeners, and timer callbacks), plus every promise rejection still
    /// unhandled at this point — the last moment a handler could have attached.
    pub fn drain_errors(&self) -> Vec<String> {
        let mut errors = std::mem::take(&mut *self.hooks.errors.borrow_mut());
        errors.extend(
            std::mem::take(&mut *self.hooks.pending_rejections.borrow_mut())
                .into_iter()
                .map(|reason| format!("unhandled promise rejection: {reason}")),
        );
        errors
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

    /// Replaces the viewport: media queries re-evaluate and the next layout
    /// read reflows against the new size.
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
    pub fn layout_rect(&self, node: NodeId) -> Option<oxidepage_base::Rect> {
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

/// Decodes the body of a `data:` URL (the part after `data:`), returning the
/// bytes and optional MIME type. Handles `;base64` and percent-encoded data.
fn decode_data_url(rest: &str) -> Option<(Vec<u8>, Option<String>)> {
    let (meta, payload) = rest.split_once(',')?;
    let is_base64 = meta.ends_with(";base64");
    let mime = {
        let m = meta.strip_suffix(";base64").unwrap_or(meta);
        let m = m.split(';').next().unwrap_or(m);
        (!m.is_empty()).then(|| m.to_string())
    };
    let bytes = if is_base64 {
        base64_decode(payload)?
    } else {
        percent_encoding::percent_decode_str(payload).collect()
    };
    Some((bytes, mime))
}

/// Decodes standard base64 (padding optional; ASCII whitespace ignored).
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in input.as_bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = val(b)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
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
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
}

/// Extracts the `charset` parameter from a `Content-Type` value.
fn content_type_charset(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("charset=")
            .map(|c| c.trim().trim_matches('"').to_owned())
    })
}

fn report_throw(hooks: &LoopHooks, throw: oxidepage_js::JsThrow) {
    let message = match throw {
        oxidepage_js::JsThrow::Type(m) | oxidepage_js::JsThrow::Range(m) => m,
        oxidepage_js::JsThrow::Value(_) => "exception while firing a lifecycle event".to_owned(),
    };
    hooks.report_error(message);
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
