//! Per-page bindings state: the DOM tree, the host-object table, wrapper
//! bookkeeping, listener and observer registries, and embedder hooks.
//!
//! `PageState` is installed into the realm as its host state; host callbacks
//! retrieve it through the scope instead of capturing it, so no JS object
//! ever holds a strong reference back to the page (see `oxidepage-js` docs).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use oxidepage_base::{NodeId, RequestId};
use oxidepage_dom::observer::MutationObserverId;
use oxidepage_dom::{DomTree, MutationRecord, QualName};
use oxidepage_js::{JsObject, JsValue};
use oxidepage_layout::{LayoutEngine, PaintStamp};
use oxidepage_net::NetRequest;
use oxidepage_style::{StyleEngine, Viewport};

use crate::collections::CollectionData;
use crate::console::{ConsoleMessage, ScriptError};
use crate::cssdata::{RuleData, RuleListData, SheetData, StyleDeclData};
use crate::customreg::CustomElementRegistry;
use crate::dialog::{DialogRequest, DialogResponse};
use crate::events::{EventData, ListenerRegistry};
use crate::netdata::{
    FormDataData, HeadersData, PendingNet, RequestData, ResponseData, UrlData, UrlSearchParamsData,
    XhrData,
};
use crate::storage::{SharedStorage, StorageAreaKind, StorageHandle};
use crate::window_open::{OpenWindowRequest, OpenedWindow, WindowProxyData};

/// Host-object tags (the `tag` half of the engine payload).
pub(crate) const TAG_NODE: u32 = 1;
pub(crate) const TAG_SLAB: u32 = 2;

/// Embedder hooks the bindings call out to (implemented by the page crate's
/// event loop).
pub trait HostHooks {
    /// Records one console line. The whole payload is built here, at the call
    /// site — it is the only place with a scope to read argument values and
    /// capture a stack from.
    fn console_message(&self, message: ConsoleMessage);
    /// Reports an error that must not break control flow (listener throws,
    /// observer callback throws).
    fn report_error(&self, error: ScriptError);

    /// Runs a `window.alert` / `confirm` / `prompt` and answers it.
    ///
    /// Synchronous by construction: these three return a value to the calling
    /// script, and the engine cannot suspend a running script. That is also
    /// where HTML's "pause the page" comes from — the event loop never
    /// regains control while the dialog is open.
    fn run_dialog(&self, request: DialogRequest) -> DialogResponse;
    /// Schedules a timer; returns its id.
    fn schedule_timer(
        &self,
        callback: JsValue,
        args: Vec<JsValue>,
        delay_ms: f64,
        repeat: bool,
    ) -> f64;
    fn clear_timer(&self, id: f64);

    /// `requestAnimationFrame`: registers `callback` to run at the next
    /// rendering opportunity; returns its id.
    fn request_animation_frame(&self, callback: JsValue) -> f64;
    /// `cancelAnimationFrame`: cancels a pending animation-frame callback.
    fn cancel_animation_frame(&self, id: f64);

    /// Starts a `fetch`/XHR network request; returns its id. Completion flows
    /// back as a `NetEvent` the page routes to `deliver_net_event`.
    fn start_fetch(&self, request: NetRequest) -> RequestId;
    /// Cancels an in-flight network request.
    fn abort(&self, id: RequestId);
    /// `document.cookie` getter: script-visible cookies for `document_url`
    /// (`HttpOnly` cookies excluded).
    fn get_cookie(&self, document_url: &str) -> String;
    /// `document.cookie` setter: stores one cookie from script.
    fn set_cookie(&self, document_url: &str, cookie: &str);

    /// HTML's "window open steps": open a new browsing context.
    ///
    /// `None` is a **real answer**, not a stub — it is what a browser does with
    /// a blocked popup, and `window.open` then returns `null`. It is also the
    /// default, because a bare `Page` has no sibling to open: only a driver
    /// that owns several pages ([`oxidepage-engine`]) can implement this.
    ///
    /// Called with JavaScript on the stack, so it takes and returns plain data
    /// for the same reason [`HostHooks::run_dialog`] does.
    ///
    /// [`oxidepage-engine`]: https://docs.rs/oxidepage-engine
    fn open_window(&self, request: OpenWindowRequest) -> Option<OpenedWindow> {
        let _ = request;
        None
    }

    /// The storage area backing `localStorage` / `sessionStorage` for the
    /// document at `origin`.
    ///
    /// One method rather than six accessors on purpose: quota accounting and
    /// sibling notification all live on [`StorageArea`], not smeared across
    /// this trait — which has three implementations, so every method added
    /// here is an edit in three places.
    ///
    /// [`StorageArea`]: crate::storage::StorageArea
    fn storage(&self, kind: StorageAreaKind, origin: &str) -> SharedStorage;
}

/// Host data addressed by slab key (everything host-backed except nodes,
/// which pack their `NodeId` directly into the payload).
pub(crate) enum HostData {
    Event(Rc<RefCell<EventData>>),
    Collection(CollectionData),
    /// Live attribute map for an element.
    NamedNodeMap(NodeId),
    /// Attribute wrapper identified by owner element and qualified name.
    Attr(Rc<AttrData>),
    Navigator(Rc<NavigatorData>),
    Screen(Rc<ScreenData>),
    Performance,
    /// `performance.timing` (state lives in [`PageState::timing`]).
    PerformanceTiming,
    /// `document.fonts` (one per document; state lives in
    /// [`PageState::fonts_loading`]/[`PageState::font_ready_resolvers`]).
    FontFaceSet,
    /// The single `window.customElements` registry brand (state lives in
    /// [`PageState::custom_elements`]).
    CustomElementRegistry,
    MediaQueryList(Rc<MediaQueryListData>),
    /// A handle on a sibling browsing context (`window.open`'s return value).
    WindowProxy(Rc<WindowProxyData>),
    /// `localStorage` / `sessionStorage`.
    Storage(Rc<StorageHandle>),
    /// An `AbortSignal`. The controller and its signal share one
    /// [`AbortSignalData`] behind an `Rc`.
    AbortSignal(Rc<AbortSignalData>),
    /// An `AbortController`, sharing its signal's [`AbortSignalData`].
    AbortController(Rc<AbortSignalData>),
    /// A standalone `new EventTarget()`.
    EventTarget(Rc<EventTargetData>),
    ResizeObserver(Rc<ResizeObserverData>),
    /// A delivered `ResizeObserverEntry` (precomputed wrapper values).
    ResizeObserverEntry(Rc<RoEntryView>),
    IntersectionObserver(Rc<IntersectionObserverData>),
    /// A delivered `IntersectionObserverEntry` (precomputed wrapper values).
    IntersectionObserverEntry(Rc<IoEntryView>),
    PluginArray,
    MimeTypeArray,
    Observer(MutationObserverId),
    MutationRecord(Rc<RecordView>),
    Url(Rc<UrlData>),
    UrlSearchParams(Rc<UrlSearchParamsData>),
    FormData(Rc<FormDataData>),
    Headers(Rc<RefCell<HeadersData>>),
    Request(Rc<RequestData>),
    Response(Rc<ResponseData>),
    Xhr(Rc<RefCell<XhrData>>),
    /// An `xhr.upload` object: a second event-target identity for the XHR that
    /// owns it. The back-reference is **weak** — see
    /// [`crate::cx::BindCx::new_xhr_upload`].
    XhrUpload(std::rc::Weak<RefCell<XhrData>>),
    StyleDecl(Rc<StyleDeclData>),
    StyleSheet(Rc<SheetData>),
    /// `document.styleSheets`: the document node whose author sheets it lists.
    StyleSheetList(oxidepage_base::NodeId),
    /// `document.implementation`: the document it was minted for. WPT saves an
    /// `implementation` and expects it to keep creating documents against that
    /// document, so it cannot be a singleton.
    DomImplementation(oxidepage_base::NodeId),
    /// `new DOMParser()`: a brand with no state of its own.
    DomParser,
    CssRule(Rc<RuleData>),
    CssRuleList(Rc<RuleListData>),
    /// A `DOMRect`/`DOMRectReadOnly`. The geometry is shared behind an `Rc` so
    /// a mutable `DOMRect` stays live through any aliasing wrapper.
    DomRect(Rc<RefCell<RectData>>),
    /// A `DOMRectList` (fixed snapshot of rects; `item()` mints fresh wrappers).
    DomRectList(Rc<Vec<Rc<RefCell<RectData>>>>),
    /// An `SVGAnimatedString` reflecting an element attribute (`<a href>`).
    /// Live like `DOMTokenList`: the value is read from the attribute on every
    /// access, so no invalidation protocol is needed.
    SvgAnimatedString {
        element: oxidepage_base::NodeId,
        attr: oxidepage_dom::LocalName,
    },
    /// `window.location`: a brand with no state of its own — a Location *is*
    /// the document URL, which lives in the DOM tree.
    Location,
    /// `window.history`: a brand; the entry list lives in
    /// [`PageState::history`].
    History,
}

/// A navigation script has asked for but the page has not yet performed.
///
/// Navigation cannot happen inline: a `location.href` write runs under live
/// `RefCell` borrows on the DOM, style and layout engines, and committing a
/// document replaces all three. So it is a task source, drained by the page's
/// event loop exactly like [`PageState::pending_scroll_targets`].
pub enum PendingNavigation {
    /// A load of `url`, which is already absolute. `replace` overwrites the
    /// current session-history entry instead of pushing a new one.
    Load {
        url: String,
        replace: bool,
        body: Option<NavigationBody>,
        /// `location.reload()`: skip the HTTP cache.
        reload: bool,
    },
    /// `history.go(delta)`. The entry list lives here in the bindings, but a
    /// traversal may need a document load, so the page performs the move.
    Traverse { delta: i32 },
    /// A `javascript:` URL, already percent-decoded. Queued rather than run
    /// inline for the same reason every other navigation is: the activation
    /// that produced it runs under live borrows, and the script may replace the
    /// document.
    JavaScriptUrl { source: String },
}

/// The request body of a form submission that navigates.
pub struct NavigationBody {
    pub method: String,
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// One session-history entry.
pub struct HistoryEntry {
    pub url: String,
    /// The `history.state` for this entry (a structured clone taken at
    /// `pushState`/`replaceState` time).
    pub state: JsValue,
    /// Which loaded document this entry belongs to. Traversing to an entry
    /// whose sequence differs from the current one needs a document load.
    pub document_seq: u64,
}

/// The session history of the page's one browsing context.
///
/// Bounded on purpose: an entry holds a live `JsValue` state across
/// navigations, so an unbounded list is an unbounded JS retention.
pub struct SessionHistory {
    entries: Vec<HistoryEntry>,
    index: usize,
    /// Bumped on every cross-document commit; stamped onto the entry that
    /// commit produces. This is what makes "is the target entry in the
    /// document I am looking at?" a single integer comparison.
    document_seq: u64,
    /// `history.scrollRestoration`. Stored and reflected, nothing more —
    /// there is no bfcache to restore a scroll position from.
    scroll_restoration: String,
}

/// The longest session history the page keeps. Older entries fall off the
/// front (the index moves with them), which is what a browser's own cap does.
pub const MAX_HISTORY_ENTRIES: usize = 50;

/// The deepest [`PageState::pending_navigation`] queue. Requests past it are
/// dropped: the page performs at most `MAX_CHAINED_NAVIGATIONS` off one entry
/// point regardless, so anything queueing more than this in a single task is a
/// runaway loop rather than a page with intent.
pub const MAX_PENDING_NAVIGATIONS: usize = 32;

impl SessionHistory {
    fn new(url: String) -> Self {
        Self {
            entries: vec![HistoryEntry {
                url,
                state: JsValue::Null,
                document_seq: 0,
            }],
            index: 0,
            document_seq: 0,
            scroll_restoration: "auto".to_owned(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    #[must_use]
    pub fn document_seq(&self) -> u64 {
        self.document_seq
    }

    #[must_use]
    pub fn current(&self) -> Option<&HistoryEntry> {
        self.entries.get(self.index)
    }

    #[must_use]
    pub fn entry(&self, index: usize) -> Option<&HistoryEntry> {
        self.entries.get(index)
    }

    #[must_use]
    pub fn scroll_restoration(&self) -> &str {
        &self.scroll_restoration
    }

    pub fn set_scroll_restoration(&mut self, value: &str) {
        if value == "auto" || value == "manual" {
            self.scroll_restoration = value.to_owned();
        }
    }

    /// The index `delta` steps away, or `None` when that is out of range (the
    /// spec's "if there is no such entry, then return" — a silent no-op).
    #[must_use]
    pub fn target_of(&self, delta: i32) -> Option<usize> {
        let target = i64::try_from(self.index).ok()? + i64::from(delta);
        let target = usize::try_from(target).ok()?;
        (target < self.entries.len()).then_some(target)
    }

    /// Moves the current index without loading anything (a same-document
    /// traversal). Returns the entry's state for `popstate`.
    pub fn set_index(&mut self, index: usize) {
        if index < self.entries.len() {
            self.index = index;
        }
    }

    /// Pushes a new entry after truncating everything forward of the current
    /// one, and returns the new index.
    pub fn push(&mut self, url: String, state: JsValue, document_seq: u64) {
        self.entries.truncate(self.index + 1);
        self.entries.push(HistoryEntry {
            url,
            state,
            document_seq,
        });
        self.index = self.entries.len() - 1;
        self.trim();
    }

    /// Overwrites the current entry (`replaceState`, `location.replace()`).
    pub fn replace(&mut self, url: String, state: JsValue, document_seq: u64) {
        if let Some(entry) = self.entries.get_mut(self.index) {
            entry.url = url;
            entry.state = state;
            entry.document_seq = document_seq;
        }
    }

    /// Re-stamps an entry with the document a traversal just loaded for it (and
    /// the URL that load ended on, which a redirect may have moved), so a second
    /// traversal back to it is same-document. The entry's state is preserved —
    /// that is what distinguishes this from [`SessionHistory::replace`].
    pub fn restamp(&mut self, index: usize, url: String, document_seq: u64) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.url = url;
            entry.document_seq = document_seq;
        }
    }

    /// Allocates the next document sequence number (one cross-document commit).
    pub fn next_document_seq(&mut self) -> u64 {
        self.document_seq += 1;
        self.document_seq
    }

    fn trim(&mut self) {
        if self.entries.len() > MAX_HISTORY_ENTRIES {
            let drop = self.entries.len() - MAX_HISTORY_ENTRIES;
            self.entries.drain(..drop);
            self.index = self.index.saturating_sub(drop);
        }
    }
}

/// Immutable identity and capability values behind one realm's Navigator.
pub struct NavigatorData {
    pub user_agent: String,
    pub vendor: String,
    pub platform: String,
    pub languages: Vec<String>,
    pub hardware_concurrency: u64,
    pub webdriver: bool,
    pub max_touch_points: u32,
    pub(crate) languages_js: RefCell<Option<JsValue>>,
    pub(crate) plugins_js: RefCell<Option<JsValue>>,
    pub(crate) mime_types_js: RefCell<Option<JsValue>>,
}

impl NavigatorData {
    #[must_use]
    pub fn new(
        user_agent: String,
        vendor: String,
        platform: String,
        languages: Vec<String>,
        hardware_concurrency: u64,
        webdriver: bool,
        max_touch_points: u32,
    ) -> Self {
        Self {
            user_agent,
            vendor,
            platform,
            languages,
            hardware_concurrency,
            webdriver,
            max_touch_points,
            languages_js: RefCell::new(None),
            plugins_js: RefCell::new(None),
            mime_types_js: RefCell::new(None),
        }
    }
}

impl Default for NavigatorData {
    fn default() -> Self {
        Self::new(
            format!(
                "Mozilla/5.0 (compatible) OxidePage/{}",
                env!("CARGO_PKG_VERSION")
            ),
            String::new(),
            std::env::consts::OS.to_owned(),
            vec!["en-US".to_owned()],
            std::thread::available_parallelism()
                .map_or(1, |value| value.get())
                .clamp(1, 8) as u64,
            false,
            0,
        )
    }
}

/// Immutable virtual-display values behind one realm's `Screen` object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenData {
    pub width: u32,
    pub height: u32,
    pub avail_width: u32,
    pub avail_height: u32,
    pub color_depth: u32,
    pub pixel_depth: u32,
}

/// One `MediaQueryList`, kept live so viewport changes can re-evaluate it.
pub(crate) struct MediaQueryListData {
    pub media: String,
    pub matches: Cell<bool>,
    pub key: Cell<Option<u64>>,
    pub wrapper: RefCell<Option<JsValue>>,
}

/// State shared by an `AbortController` and the `AbortSignal` it owns.
///
/// The controller's `signal` getter returns the same cached `wrapper`, so the
/// controller and signal wrappers each hold a strong reference to the other's
/// `AbortSignalData` (via the shared `Rc`) and the signal wrapper is cached in
/// `wrapper`. This is the same accepted wrapper-cycle leak class as
/// [`MediaQueryListData`]: the objects live until the realm is torn down.
pub(crate) struct AbortSignalData {
    pub aborted: Cell<bool>,
    pub reason: RefCell<JsValue>,
    pub key: Cell<Option<u64>>,
    pub wrapper: RefCell<Option<JsValue>>,
    /// In-flight `fetch` request ids to abort when the signal fires.
    pub pending_fetches: RefCell<Vec<RequestId>>,
}

/// A standalone `new EventTarget()`. It owns no state beyond its identity — the
/// listeners live in the shared registry, keyed by
/// [`crate::events::EventTargetKey::Host`] — but it still needs a wrapper cell so
/// `event.target` can hand back the very object the script constructed. Same
/// accepted wrapper-cycle leak class as [`AbortSignalData`].
pub(crate) struct EventTargetData {
    pub wrapper: RefCell<Option<JsValue>>,
}

impl ScreenData {
    #[must_use]
    pub fn from_viewport(viewport: Viewport) -> Self {
        let width = viewport.width.round().max(1.0) as u32;
        let height = viewport.height.round().max(1.0) as u32;
        Self {
            width,
            height,
            avail_width: width,
            avail_height: height,
            color_depth: 24,
            pixel_depth: 24,
        }
    }
}

pub(crate) struct AttrData {
    pub owner: NodeId,
    pub name: QualName,
}

/// `PerformanceTiming` fields (Unix epoch milliseconds; `0.0` = the milestone
/// has not been reached yet). Recorded by the page's lifecycle as it parses and
/// dispatches events. `unload*`/`redirect*`/`secureConnectionStart` are always
/// `0` (v1: synchronous HTML injection, no real navigation phases).
#[derive(Clone, Copy, Default)]
pub struct DocumentTiming {
    pub navigation_start: f64,
    pub fetch_start: f64,
    pub domain_lookup_start: f64,
    pub domain_lookup_end: f64,
    pub connect_start: f64,
    pub connect_end: f64,
    pub request_start: f64,
    pub response_start: f64,
    pub response_end: f64,
    pub dom_loading: f64,
    pub dom_interactive: f64,
    pub dom_content_loaded_event_start: f64,
    pub dom_content_loaded_event_end: f64,
    pub dom_complete: f64,
    pub load_event_start: f64,
    pub load_event_end: f64,
}

/// A document-lifecycle milestone the page records on [`PageState::timing`].
#[derive(Clone, Copy, Debug)]
pub enum TimingMilestone {
    /// Navigation began: collapses `navigationStart` through `responseStart`
    /// (v1 has no distinct network phases for injected HTML).
    NavigationStart,
    /// The response finished arriving and parsing started (`responseEnd` +
    /// `domLoading`).
    ResponseEndDomLoading,
    /// Parsing finished, just before `DOMContentLoaded` (`domInteractive`).
    DomInteractive,
    /// `DOMContentLoaded` dispatch started (`domContentLoadedEventStart`).
    DomContentLoadedStart,
    /// `DOMContentLoaded` dispatch finished (`domContentLoadedEventEnd`).
    DomContentLoadedEnd,
    /// Document fully loaded, just before the window `load` event
    /// (`domComplete` + `loadEventStart`).
    DomCompleteLoadStart,
    /// The window `load` event finished (`loadEventEnd`).
    LoadEnd,
}

/// `document.readyState`. The three transitions coincide exactly with the
/// [`TimingMilestone`]s the page already records, so the page drives both from
/// one call and the two can never disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ReadyState {
    /// The parser is still running.
    #[default]
    Loading,
    /// Parsing is done; deferred scripts and `DOMContentLoaded` come next
    /// (`domInteractive`).
    Interactive,
    /// The document and its subresources are done; the `load` event is next
    /// (`domComplete`).
    Complete,
}

impl ReadyState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Interactive => "interactive",
            Self::Complete => "complete",
        }
    }
}

/// The backing geometry of a `DOMRect`/`DOMRectReadOnly` (CSS pixels).
#[derive(Clone, Copy)]
pub(crate) struct RectData {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A delivered mutation record plus caches for its `[SameObject]` node lists.
pub(crate) struct RecordView {
    pub record: MutationRecord,
    pub added_nodes_js: RefCell<Option<JsValue>>,
    pub removed_nodes_js: RefCell<Option<JsValue>>,
}

/// JS-side references created by the bootstrap script.
pub(crate) struct JsRefs {
    pub global: JsObject,
    pub wrapper_map: JsValue,
    pub cache_get: JsValue,
    pub cache_set: JsValue,
    pub collection_proxy: JsValue,
    pub install_iterable: JsValue,
    /// Installs @@iterator = %Array.prototype.values% on an indexed-getter
    /// interface prototype that declares no `iterable<>` (WebIDL rule).
    pub install_value_iterator: JsValue,
    /// `(owner, sync, initial) => Proxy` — the ObservableArray stand-in
    /// behind `adoptedStyleSheets`: in-place mutations call `sync`.
    pub adopted_sheets_proxy: JsValue,
    pub set_to_string_tag: JsValue,
    pub make_dom_exception: JsValue,
    /// Pristine `structuredClone` (page script may replace the global one),
    /// used to clone `history.pushState`/`replaceState` state values.
    pub structured_clone: JsValue,
    /// `() => {promise, resolve, reject}` (deferred-promise construction).
    pub make_promise: JsValue,
    /// `(value) => Promise.resolve(value)`.
    pub resolved_promise: JsValue,
    /// `(init) => [[name, value], …]` (headers/params init normalization).
    pub record_pairs: JsValue,
    /// `(proto, snapshotFn)` installer for URLSearchParams pair iteration.
    pub install_params_iterable: JsValue,
    /// Pristine `Object.freeze` wrapper used for WebIDL FrozenArray values.
    pub freeze: JsValue,
    /// `(target) => Proxy` wrapping a `CSSStyleDeclaration` host object with
    /// camelCase/dashed property access and indexed (`style[0]`) access.
    pub style_proxy: JsValue,
    /// `(element, proto) => Proxy` backing `element.dataset`: a `DOMStringMap`
    /// exposing the element's `data-*` attributes as camelCased properties.
    pub dataset_proxy: JsValue,
    /// `(object, key) => boolean` — property removal, which `JsScope` lacks.
    pub delete_property: JsValue,
    /// The realm's `Object.prototype`, the prototype of every interface
    /// prototype object that declares no parent interface.
    pub object_prototype: JsObject,
    /// `(ctor) => Reflect.construct(ctor, [], ctor)` — runs a custom-element
    /// constructor as an upgrade so `new.target` pins the result's prototype.
    pub ce_construct: JsValue,
    /// Installs globals that depend on generated classes or other globals
    /// (`AbortSignal.abort`/`timeout`, `performance.mark`/`measure`/entries).
    /// Called once after `register_interfaces` and `install_window`.
    pub install_late_globals: JsValue,
    /// `(init) => new StorageEvent("storage", init)`, over the *pristine*
    /// constructor so page script cannot change what the engine dispatches.
    pub new_storage_event: JsValue,
    /// `(fn) => undefined` — hands the proxy's `ownKeys` trap its native
    /// one-pass key lister.
    pub set_storage_keys: JsValue,
    /// `(callback) => void` — the engine's own microtask enqueuer, over a
    /// pristine resolved promise. Queues the mutation-observer compound
    /// microtask so it is *ordered against* promise reactions.
    pub enqueue_microtask: JsValue,
    /// The host function the compound microtask runs: delivers queued
    /// `MutationObserver` records.
    pub mutation_notify: JsValue,
}

/// Which CSS box a `ResizeObserver` target watches (named after the
/// `ResizeObserverBoxOptions` enum values).
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum RoBoxKind {
    ContentBox,
    BorderBox,
    DevicePixelContentBox,
}

/// One observed element inside a `ResizeObserver`.
pub(crate) struct RoTarget {
    pub node: NodeId,
    pub box_kind: RoBoxKind,
    /// The last delivered size of the observed box (`None` before first
    /// delivery), used to suppress unchanged reports.
    pub last: Cell<Option<(f32, f32)>>,
    /// Set on `observe`; forces an initial delivery once the element has a box.
    pub initial_pending: Cell<bool>,
}

/// A registered `ResizeObserver`. The callback and wrapper are held for the
/// page's lifetime (the accepted `MediaQueryListData` wrapper-cycle leak class);
/// the registry in [`PageState::resize_observers`] is cleared on navigation so
/// stale `NodeId`s are never delivered against the new document.
pub(crate) struct ResizeObserverData {
    pub callback: JsValue,
    pub wrapper: RefCell<Option<JsValue>>,
    pub targets: RefCell<Vec<RoTarget>>,
}

/// A delivered `ResizeObserverEntry`: all member values are precomputed JS
/// wrappers (a real interface, not a plain object, because polyfills sniff its
/// prototype).
pub(crate) struct RoEntryView {
    pub target: JsValue,
    pub content_rect: JsValue,
    pub border_box_size: JsValue,
    pub content_box_size: JsValue,
    pub device_pixel_content_box_size: JsValue,
}

/// One `rootMargin` component: either CSS pixels or a percentage of the root
/// rect's relevant axis.
#[derive(Clone, Copy)]
pub(crate) enum IoMargin {
    Px(f32),
    Percent(f32),
}

impl IoMargin {
    /// Resolves against `basis` (the root width for left/right, height for
    /// top/bottom).
    pub fn resolve(self, basis: f32) -> f32 {
        match self {
            IoMargin::Px(v) => v,
            IoMargin::Percent(p) => p / 100.0 * basis,
        }
    }
}

impl std::fmt::Display for IoMargin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoMargin::Px(v) => write!(f, "{v}px"),
            IoMargin::Percent(p) => write!(f, "{p}%"),
        }
    }
}

/// One observed element inside an `IntersectionObserver`.
pub(crate) struct IoTarget {
    pub node: NodeId,
    /// The last delivered `(isIntersecting, bucket)` where `bucket` is the
    /// number of thresholds ≤ the intersection ratio.
    pub last: Cell<Option<(bool, usize)>>,
    pub initial_pending: Cell<bool>,
}

/// A registered `IntersectionObserver` (same lifetime/registry policy as
/// [`ResizeObserverData`]).
pub(crate) struct IntersectionObserverData {
    pub callback: JsValue,
    pub wrapper: RefCell<Option<JsValue>>,
    /// The intersection root: `None` = the viewport (also used for a
    /// `Document` root in v1).
    pub root: Option<NodeId>,
    /// `rootMargin` as [top, right, bottom, left].
    pub root_margin: [IoMargin; 4],
    /// Thresholds, sorted ascending and clamped to `[0, 1]` (never empty).
    pub thresholds: Vec<f64>,
    pub targets: RefCell<Vec<IoTarget>>,
}

/// A delivered `IntersectionObserverEntry` (precomputed member values).
pub(crate) struct IoEntryView {
    pub time: f64,
    pub root_bounds: JsValue,
    pub bounding_client_rect: JsValue,
    pub intersection_rect: JsValue,
    pub is_intersecting: bool,
    pub intersection_ratio: f64,
    pub target: JsValue,
}

/// The O(1) gate deciding whether observer delivery can be skipped: the live
/// DOM style/structure versions (which change on any mutation, reflowed or not),
/// the layout paint stamp (viewport, element scroll, images, fonts), and the
/// document (viewport) scroll version. The last is tracked separately because
/// document scroll is no longer part of the paint stamp — the display list is
/// scroll-independent — yet `IntersectionObserver` intersects against the
/// scrolled viewport and must rerun when the document scrolls. Delivery reruns
/// only when this differs from the last stamp.
pub(crate) type ObsGate = (u64, u64, PaintStamp, u64);

/// A registered interface: its prototype and constructor objects.
pub(crate) struct InterfaceEntry {
    pub proto: JsObject,
    #[allow(dead_code)]
    pub ctor: JsObject,
}

/// A registered `MutationObserver`: wrapper and callback are held strongly
/// for the lifetime of the page (observers are not collectable in Phase 2).
pub(crate) struct ObserverEntry {
    pub id: MutationObserverId,
    pub wrapper: JsValue,
    pub callback: JsValue,
}

/// All bindings state for one page/realm pair.
pub struct PageState {
    /// The document tree, shared with the parser during loads.
    pub dom: Rc<RefCell<DomTree>>,
    /// The style engine (stylo): document stylesheet set + computed values.
    pub style: Rc<RefCell<StyleEngine>>,
    /// The layout engine (box tree + taffy/parley): backs the geometry APIs.
    pub layout: Rc<RefCell<LayoutEngine>>,
    pub(crate) slab: RefCell<Slab>,
    pub(crate) js: RefCell<Option<JsRefs>>,
    pub(crate) listeners: RefCell<ListenerRegistry>,
    /// Event handlers (`onload`, `onerror`, ...) installed on an event target,
    /// keyed by event type — whether assigned through the IDL attribute or
    /// compiled from the content attribute. Listener registration remains in
    /// `listeners`.
    pub(crate) event_handlers: RefCell<HashMap<(crate::events::EventTargetKey, String), JsValue>>,
    /// The content-attribute source each `event_handlers` slot currently
    /// reflects. Absence means "no attribute backed this handler", so a slot is
    /// stale exactly when this disagrees with the element's `on<type>` attribute
    /// (`crate::handlers`).
    pub(crate) handler_attr_seen: RefCell<HashMap<(crate::events::EventTargetKey, String), String>>,
    pub(crate) observers: RefCell<Vec<ObserverEntry>>,
    pub(crate) interfaces: RefCell<HashMap<String, InterfaceEntry>>,
    /// Constants collected between `begin_interface` and `finish_interface`.
    pub(crate) pending_consts: RefCell<Vec<(String, f64)>>,
    /// `[SameObject]` wrapper cache: (node slot index, generation, member) →
    /// wrapper. The generation guards against a freed node's index being reused
    /// by a different node inheriting a stale cached wrapper.
    pub(crate) same_object: RefCell<HashMap<(u32, u32, &'static str), JsValue>>,
    pub(crate) hooks: Rc<dyn HostHooks>,
    /// This document's identity among the subscribers of its storage areas, so
    /// a `storage` event is delivered to the *other* documents and never back
    /// to the one that wrote (HTML).
    pub(crate) storage_subscriber: crate::storage::StorageSubscriber,
    /// Every `Storage` handle installed in this realm, so a navigation can
    /// re-point them all at the new origin's areas (see `refresh_storage`).
    pub(crate) storage_handles: RefCell<Vec<Rc<StorageHandle>>>,
    /// In-flight `fetch`/XHR requests awaiting completion, keyed by id.
    pub(crate) pending_net: RefCell<HashMap<RequestId, PendingNet>>,
    /// Scroll containers whose position changed from script (`None` = the
    /// viewport); the page's event loop drains this and dispatches `scroll`
    /// events as tasks.
    pub(crate) pending_scroll_targets: RefCell<Vec<Option<oxidepage_base::NodeId>>>,
    /// The navigations script has asked for and the page has not yet performed,
    /// in the order they were requested.
    ///
    /// A **queue**, not a single slot. Only a `Load` superseding a queued `Load`
    /// collapses (`location.href = a; location.href = b` navigates once, to `b`,
    /// as a browser does); a traversal is *cumulative* — `history.back();
    /// history.back()` must move two entries — and a `javascript:` URL is a
    /// script to run, so neither may be swallowed by whatever was queued next.
    pub(crate) pending_navigation: RefCell<VecDeque<PendingNavigation>>,
    /// The session history of this browsing context, shared by
    /// `history.pushState`/`go()` and the page's navigation driver.
    pub(crate) history: RefCell<SessionHistory>,
    /// `document.referrer`: the URL of the document this one was navigated
    /// from, written by the page at commit time.
    pub(crate) referrer: RefCell<String>,
    /// Realm-stable `location` / `history` wrappers (the `navigator_js`
    /// pattern: one object per realm, surviving navigation).
    pub(crate) location_js: RefCell<Option<JsValue>>,
    pub(crate) history_js: RefCell<Option<JsValue>>,
    /// HTML's **firing submission events** flag: set only while the `submit`
    /// event of a submission is being dispatched, so a `requestSubmit()` or a
    /// submit-button activation raised from inside `onsubmit` cannot recurse
    /// forever. `form.submit()` fires no event and is not blocked by it, which
    /// is what makes validate-then-submit work.
    pub(crate) firing_submission_events: Cell<bool>,
    /// While the parser holds handles into the tree, detached subtrees must
    /// not be freed under it.
    pub(crate) parsing: Cell<bool>,
    /// `document.readyState`, driven by [`PageState::mark_timing`].
    pub(crate) ready_state: Cell<ReadyState>,
    /// True only while a parser-inserted classic script is evaluating.
    pub(crate) parser_script_active: Cell<bool>,
    /// The DOM spec's "mutation observer microtask queued" flag. Set when a
    /// record is queued and the compound microtask is enqueued; cleared when
    /// that microtask runs, so at most one is outstanding.
    pub(crate) mutation_microtask_queued: Cell<bool>,
    /// Text produced by `document.write` during the current parser script.
    pub(crate) pending_parser_write: RefCell<String>,
    /// Per-document bounds for parser writes.
    pub(crate) parser_write_calls: Cell<usize>,
    pub(crate) parser_write_bytes: Cell<usize>,
    /// `console.group` nesting depth. Grouping has no other observable effect
    /// in a headless console, so the depth *is* the feature — it rides on
    /// every `ConsoleMessage` and the CLI indents by it.
    pub(crate) console_group_depth: Cell<u32>,
    /// The classic `<script>` element whose source is currently evaluating.
    /// Modules and callbacks outside direct script evaluation observe `None`.
    pub current_script: Cell<Option<NodeId>>,
    /// Immutable Navigator profile associated with this realm.
    pub(crate) navigator: Rc<NavigatorData>,
    /// The realm's associated Navigator wrapper (`navigator` and
    /// `clientInformation` both return this exact object).
    pub(crate) navigator_js: RefCell<Option<JsValue>>,
    /// Immutable Screen profile and its realm-stable wrapper.
    pub(crate) screen: Rc<ScreenData>,
    pub(crate) screen_js: RefCell<Option<JsValue>>,
    /// Realm-stable `performance` wrapper.
    pub(crate) performance_js: RefCell<Option<JsValue>>,
    /// Realm-stable `performance.timing` wrapper (`[SameObject]`).
    pub(crate) performance_timing_js: RefCell<Option<JsValue>>,
    /// Realm-stable `document.fonts` wrapper (`[SameObject]`; one document,
    /// so — like `performance_js` — a single cell rather than a
    /// per-node `same_object` entry).
    pub(crate) font_face_set_js: RefCell<Option<JsValue>>,
    /// Mirrors `Page::pending_fonts.is_empty()` (the page owns the actual
    /// in-flight `@font-face` loads; it writes this directly through its
    /// `Rc<PageState>` whenever that set changes — `pub`, unlike this
    /// struct's other bindings-internal cells, because the page crate sets
    /// it). Backs `FontFaceSet.status` and the synchronous-resolve check in
    /// `ready`.
    pub fonts_loading: Cell<bool>,
    /// `document.fonts.ready` promises stashed by `imp::font_face_set::ready`
    /// when it could not resolve synchronously (a font load is in flight, or
    /// the document is still parsing so more `@font-face` rules could still
    /// turn up). The page's event loop resolves and drains these once
    /// `fonts_loading` goes false and parsing has finished
    /// (`oxidepage_bindings::resolve_font_ready`).
    pub(crate) font_ready_resolvers: RefCell<Vec<JsValue>>,
    /// Pending `document.fonts.load(...)` promise resolvers, drained alongside
    /// [`PageState::font_ready_resolvers`] once fonts settle. Each resolves with
    /// an (empty) `sequence<FontFace>`.
    pub(crate) font_load_resolvers: RefCell<Vec<JsValue>>,
    /// Document navigation/timing milestones behind `performance.timing`.
    pub(crate) timing: RefCell<DocumentTiming>,
    /// Live MediaQueryList objects created in this realm.
    pub(crate) media_queries: RefCell<Vec<Rc<MediaQueryListData>>>,
    /// Registered `ResizeObserver`s (delivery source), cleared on navigation.
    pub(crate) resize_observers: RefCell<Vec<Rc<ResizeObserverData>>>,
    /// Registered `IntersectionObserver`s (delivery source), cleared on navigation.
    pub(crate) intersection_observers: RefCell<Vec<Rc<IntersectionObserverData>>>,
    /// Last observer-delivery gate stamp (skips reflow when nothing changed).
    pub(crate) obs_gate: Cell<Option<ObsGate>>,
    /// Set by `observe()` to force exactly one geometry pass even when the gate
    /// is unchanged (the initial delivery of a freshly observed target). Cleared
    /// after that pass, so a boxless target does not keep the gate bypassed.
    pub(crate) obs_dirty: Cell<bool>,
    /// The HTML *named properties object*: it sits between `Window.prototype`
    /// and `EventTarget.prototype` in the window's prototype chain, and carries
    /// one accessor per element `id` in the document, so `window.someId` and
    /// the bare `someId` resolve to that element.
    pub(crate) named_props: RefCell<Option<JsObject>>,
    /// The ids currently materialized as accessors on `named_props`.
    pub(crate) named_prop_keys: RefCell<HashSet<String>>,
    /// The `DomTree::id_version` those accessors were built from; `None` before
    /// the first sync.
    pub(crate) named_props_version: Cell<Option<u64>>,
    /// The realm's custom-element registry (`window.customElements`).
    /// Definitions and `whenDefined` promises live here; the DOM mirrors only
    /// the set of defined names and a reaction-intent queue.
    pub(crate) custom_elements: RefCell<CustomElementRegistry>,
    /// Strong references to the JS wrappers of upgraded custom elements. The
    /// generic node-wrapper cache is *weak* (QuickJS is reference-counted, so a
    /// wrapper with no strong reference is freed immediately), but a custom
    /// element's JS state (its subclass prototype and constructor-set instance
    /// fields) lives only on that wrapper and cannot be reconstructed — so it
    /// must be retained for the element's lifetime. Cleared on navigation.
    pub(crate) custom_wrappers: RefCell<HashMap<NodeId, JsValue>>,
    /// CDP's `objectId` table (ADR-0030).
    ///
    /// Here rather than in the protocol crate because it holds live `JsValue`s:
    /// they are `!Send`, and they must drop before the realm — which is what
    /// `Page`'s field order encodes. A store owned by a session on the driver's
    /// thread would outlive the realm it points into.
    pub remote_objects: RefCell<crate::remote::ObjectStore>,
    /// CDP's `Runtime.ExecutionContextId`, bumped on every document commit so a
    /// driver can tell a stale context from the live one.
    pub execution_context_id: Cell<u64>,
    /// Payloads delivered by `Runtime.addBinding` functions, drained by the
    /// page into `Runtime.bindingCalled`.
    pub binding_calls: RefCell<VecDeque<(String, String)>>,
    /// Scripts run at the start of every new document, in insertion order.
    ///
    /// Deliberately **not** cleared by `reset_for_navigation`: the whole point
    /// is that they survive navigation. That is what makes a driver's
    /// `exposeFunction` and `evaluateOnNewDocument` still be there on the next
    /// page (CDP `Page.addScriptToEvaluateOnNewDocument`).
    pub init_scripts: RefCell<Vec<(u64, String)>>,
    pub next_init_script: Cell<u64>,
    /// Strong references to the JS wrappers of *connected* nodes. The generic
    /// node-wrapper cache is weak, so a node kept alive only by tree
    /// connectedness (no JS reference to its wrapper) can have its wrapper GC'd
    /// and re-minted — silently dropping any author-set expando properties and
    /// breaking `===` identity (design §5.3). jQuery stores its data-cache id as
    /// an expando there, and Angular stores directive controllers through it, so
    /// the loss surfaces as `$compile:ctreq` on jQuery/Angular pages. Retaining
    /// the wrapper while the node is connected preserves it; the entry is dropped
    /// on disconnect (so detached subtrees still free) and on navigation.
    pub(crate) connected_wrappers: RefCell<HashMap<NodeId, JsValue>>,
    /// `adoptedStyleSheets` arrays per scope (shadow root or document node),
    /// held strongly so the JS array identity survives between reads. The
    /// engine-side sheet routing lives in `StyleEngine`; this is only the JS
    /// view. Cleared on navigation.
    pub(crate) adopted_sheets: RefCell<HashMap<NodeId, JsValue>>,
    /// Time origin for `Event.timeStamp` (and later `performance.now`).
    pub(crate) start: std::time::Instant,
    /// Wall-clock timestamp paired with `start`, in Unix milliseconds.
    pub(crate) time_origin_epoch_ms: f64,
    /// Whether the whole document counts as the visible region for an
    /// `IntersectionObserver` with the implicit (viewport) root, instead of
    /// just the viewport rectangle.
    ///
    /// Off by default — the viewport is the root, as the spec says. An embedder
    /// rendering the *whole document* (a full-page screenshot, a PDF) turns it
    /// on, because there the page below the fold is not "not yet seen": it is
    /// in the output. Script gates real content on this (a sponsor grid that
    /// only renders once observed), so with the viewport root that content is
    /// simply missing from the capture — the same failure `lazy_images` +
    /// `Page::load_deferred_images` already solve for `<img>`.
    pub(crate) whole_document_visible: Cell<bool>,
}

impl PageState {
    /// This document's identity among the subscribers of its storage areas.
    #[must_use]
    pub fn storage_subscriber(&self) -> crate::storage::StorageSubscriber {
        self.storage_subscriber
    }

    pub fn new(
        dom: Rc<RefCell<DomTree>>,
        hooks: Rc<dyn HostHooks>,
        viewport: Viewport,
        navigator: NavigatorData,
        screen: ScreenData,
    ) -> Self {
        let mut style_engine = StyleEngine::new(&dom.borrow(), viewport);
        let layout = Rc::new(RefCell::new(LayoutEngine::new(viewport)));
        // Wire real font metrics (parley/skrifa) into the cascade so
        // `ex`/`ch`/`ic` units resolve against actual fonts (WP-H).
        style_engine.set_font_metrics_provider(layout.borrow().font_metrics_factory());
        let style = Rc::new(RefCell::new(style_engine));
        let initial_url = dom.borrow().document_url().to_owned();
        let start = std::time::Instant::now();
        let time_origin_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0;
        Self {
            dom,
            style,
            layout,
            slab: RefCell::new(Slab::default()),
            js: RefCell::new(None),
            listeners: RefCell::new(ListenerRegistry::default()),
            event_handlers: RefCell::new(HashMap::new()),
            handler_attr_seen: RefCell::new(HashMap::new()),
            observers: RefCell::new(Vec::new()),
            interfaces: RefCell::new(HashMap::new()),
            pending_consts: RefCell::new(Vec::new()),
            same_object: RefCell::new(HashMap::new()),
            hooks,
            storage_subscriber: crate::storage::StorageSubscriber::next(),
            storage_handles: RefCell::new(Vec::new()),
            pending_net: RefCell::new(HashMap::new()),
            pending_scroll_targets: RefCell::new(Vec::new()),
            pending_navigation: RefCell::new(VecDeque::new()),
            history: RefCell::new(SessionHistory::new(initial_url)),
            referrer: RefCell::new(String::new()),
            location_js: RefCell::new(None),
            history_js: RefCell::new(None),
            firing_submission_events: Cell::new(false),
            parsing: Cell::new(false),
            ready_state: Cell::new(ReadyState::default()),
            parser_script_active: Cell::new(false),
            mutation_microtask_queued: Cell::new(false),
            pending_parser_write: RefCell::new(String::new()),
            parser_write_calls: Cell::new(0),
            parser_write_bytes: Cell::new(0),
            console_group_depth: Cell::new(0),
            current_script: Cell::new(None),
            navigator: Rc::new(navigator),
            navigator_js: RefCell::new(None),
            screen: Rc::new(screen),
            screen_js: RefCell::new(None),
            performance_js: RefCell::new(None),
            performance_timing_js: RefCell::new(None),
            font_face_set_js: RefCell::new(None),
            fonts_loading: Cell::new(false),
            font_ready_resolvers: RefCell::new(Vec::new()),
            font_load_resolvers: RefCell::new(Vec::new()),
            timing: RefCell::new(DocumentTiming::default()),
            media_queries: RefCell::new(Vec::new()),
            resize_observers: RefCell::new(Vec::new()),
            intersection_observers: RefCell::new(Vec::new()),
            obs_gate: Cell::new(None),
            obs_dirty: Cell::new(false),
            named_props: RefCell::new(None),
            named_prop_keys: RefCell::new(HashSet::new()),
            named_props_version: Cell::new(None),
            custom_elements: RefCell::new(CustomElementRegistry::default()),
            custom_wrappers: RefCell::new(HashMap::new()),
            remote_objects: RefCell::new(crate::remote::ObjectStore::default()),
            execution_context_id: Cell::new(1),
            binding_calls: RefCell::new(VecDeque::new()),
            init_scripts: RefCell::new(Vec::new()),
            next_init_script: Cell::new(0),
            connected_wrappers: RefCell::new(HashMap::new()),
            adopted_sheets: RefCell::new(HashMap::new()),
            start,
            time_origin_epoch_ms,
            whole_document_visible: Cell::new(false),
        }
    }

    /// Marks the parser as owning handles into the tree (suspends freeing).
    pub fn set_parsing(&self, parsing: bool) {
        self.parsing.set(parsing);
    }

    /// True while the parser holds handles into the tree.
    #[must_use]
    pub fn parsing(&self) -> bool {
        self.parsing.get()
    }

    /// See [`PageState::whole_document_visible`]. Set by the embedder before
    /// the page runs, so the observers script installs at startup see it.
    pub fn set_whole_document_visible(&self, whole: bool) {
        self.whole_document_visible.set(whole);
    }

    /// The time origin behind [`PageState::epoch_now_ms`]: the monotonic base
    /// and the wall-clock reading paired with it.
    ///
    /// Exposed so the embedder's own event loop can stamp its payloads off the
    /// *same* clock — a second, independently-seeded origin would put console
    /// lines and loop-emitted events on timescales that cannot be merged.
    #[must_use]
    pub fn time_origin(&self) -> (std::time::Instant, f64) {
        (self.start, self.time_origin_epoch_ms)
    }

    /// A monotonic Unix-epoch timestamp in milliseconds (time origin plus the
    /// monotonic clock elapsed since — never a fresh `SystemTime`, so timing
    /// milestones stay ordered even if the wall clock steps).
    #[must_use]
    pub fn epoch_now_ms(&self) -> f64 {
        self.time_origin_epoch_ms + self.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Records a document-lifecycle milestone on [`PageState::timing`].
    /// `document.readyState`.
    #[must_use]
    pub fn ready_state(&self) -> ReadyState {
        self.ready_state.get()
    }

    /// Records a lifecycle milestone: the timing entry `PerformanceTiming`
    /// reads, and — for the three milestones that are also readiness
    /// transitions — `document.readyState`.
    ///
    /// The caller fires `readystatechange` after this returns: dispatching an
    /// event needs a JS context, which the milestone marks (called between
    /// tasks) do not have.
    pub fn mark_timing(&self, milestone: crate::state::TimingMilestone) {
        use crate::state::TimingMilestone as M;
        let now = self.epoch_now_ms();
        match milestone {
            M::NavigationStart => self.ready_state.set(ReadyState::Loading),
            M::DomInteractive => self.ready_state.set(ReadyState::Interactive),
            M::DomCompleteLoadStart => self.ready_state.set(ReadyState::Complete),
            _ => {}
        }
        let mut t = self.timing.borrow_mut();
        match milestone {
            M::NavigationStart => {
                // Reset the previous document's timing, then collapse the
                // network phases (no distinct DNS/connect/request for injected
                // HTML) into one timestamp.
                *t = DocumentTiming::default();
                t.navigation_start = now;
                t.fetch_start = now;
                t.domain_lookup_start = now;
                t.domain_lookup_end = now;
                t.connect_start = now;
                t.connect_end = now;
                t.request_start = now;
                t.response_start = now;
            }
            M::ResponseEndDomLoading => {
                t.response_end = now;
                t.dom_loading = now;
            }
            M::DomInteractive => t.dom_interactive = now,
            M::DomContentLoadedStart => t.dom_content_loaded_event_start = now,
            M::DomContentLoadedEnd => t.dom_content_loaded_event_end = now,
            M::DomCompleteLoadStart => {
                t.dom_complete = now;
                t.load_event_start = now;
            }
            M::LoadEnd => t.load_event_end = now,
        }
    }

    /// Marks entry/exit of the parser-inserted classic script task.
    pub fn set_parser_script_active(&self, active: bool) {
        self.parser_script_active.set(active);
    }

    /// Queues markup for the suspended HTML parser. `Ok(false)` means the
    /// call occurred outside an active parser-inserted script.
    pub(crate) fn queue_parser_write(&self, text: &str) -> Result<bool, &'static str> {
        const MAX_CALLS: usize = 1024;
        const MAX_BYTES: usize = 1024 * 1024;
        if !self.parsing.get() || !self.parser_script_active.get() {
            return Ok(false);
        }
        let calls = self.parser_write_calls.get().saturating_add(1);
        let bytes = self.parser_write_bytes.get().saturating_add(text.len());
        if calls > MAX_CALLS || bytes > MAX_BYTES {
            return Err("document.write budget exceeded");
        }
        self.parser_write_calls.set(calls);
        self.parser_write_bytes.set(bytes);
        self.pending_parser_write.borrow_mut().push_str(text);
        Ok(true)
    }

    pub fn take_parser_write(&self) -> String {
        std::mem::take(&mut *self.pending_parser_write.borrow_mut())
    }

    /// Number of in-flight `fetch`/XHR requests (the page's event loop keeps
    /// settling while this is non-zero).
    #[must_use]
    pub fn net_pending(&self) -> usize {
        self.pending_net.borrow().len()
    }

    /// Discards the previous document's script-visible pending work on
    /// navigation, returning the request ids the caller must abort.
    ///
    /// The realm survives a navigation, so without this the old document's
    /// `fetch`/XHR completions would resolve their promises — running doc-1
    /// script against doc-2 — and its queued `scroll` targets would dispatch
    /// events at node ids belonging to the replaced tree.
    #[must_use]
    pub fn reset_for_navigation(&self) -> Vec<RequestId> {
        let aborted = self
            .pending_net
            .borrow_mut()
            .drain()
            .map(|(id, _)| id)
            .collect();
        self.pending_scroll_targets.borrow_mut().clear();
        self.event_handlers.borrow_mut().clear();
        // Observers hold stale NodeIds from the previous document; drop the
        // registries (and the delivery gate) so nothing is delivered against
        // the new tree.
        self.resize_observers.borrow_mut().clear();
        self.intersection_observers.borrow_mut().clear();
        self.obs_gate.set(None);
        self.obs_dirty.set(false);
        // Reset the custom-element registry and its DOM-side mirror; the realm
        // survives navigation, so stale definitions/promises would otherwise
        // leak into the new document.
        self.custom_elements.borrow_mut().clear();
        self.custom_wrappers.borrow_mut().clear();
        self.connected_wrappers.borrow_mut().clear();
        self.adopted_sheets.borrow_mut().clear();
        // Every `objectId` named a value of the outgoing document. Keeping them
        // would pin that document's whole object graph for the life of the
        // realm, and would let a driver read a stale handle as if it were live.
        // The bumped context id is how the driver learns they all died at once.
        self.remote_objects.borrow_mut().clear();
        // A payload the outgoing document queued belongs to a world that is
        // gone; reporting it against the new document would attribute it to the
        // wrong execution context.
        self.binding_calls.borrow_mut().clear();
        self.execution_context_id
            .set(self.execution_context_id.get() + 1);
        self.dom.borrow_mut().clear_custom_elements();
        self.current_script.set(None);
        self.parser_script_active.set(false);
        self.mutation_microtask_queued.set(false);
        self.pending_parser_write.borrow_mut().clear();
        self.parser_write_calls.set(0);
        self.parser_write_bytes.set(0);
        // A group the outgoing document opened must not indent the next one.
        self.console_group_depth.set(0);
        // `pending_navigation` is deliberately *not* cleared. The commit path
        // takes the request before it starts loading, so nothing stale is left
        // here for the incoming document — and a navigation the outgoing
        // document's unload-time script queued must survive into the next turn
        // of the loop rather than be dropped by the load it is chained off.
        aborted
    }

    /// Drains the scroll targets whose position changed since the last drain
    /// (`None` = the viewport). The page dispatches `scroll` events for them.
    #[must_use]
    pub fn take_pending_scroll_targets(&self) -> Vec<Option<oxidepage_base::NodeId>> {
        std::mem::take(&mut self.pending_scroll_targets.borrow_mut())
    }

    /// Queues a `scroll` event for `target` (`None` = the viewport) on the
    /// page's event loop — the same path a script scroll takes, so an
    /// embedder-driven scroll has no event path of its own.
    pub fn queue_scroll_event(&self, target: Option<oxidepage_base::NodeId>) {
        self.pending_scroll_targets.borrow_mut().push(target);
    }

    /// [`Self::queue_scroll_event`] for the viewport.
    pub fn queue_viewport_scroll_event(&self) {
        self.queue_scroll_event(None);
    }

    /// Drops every queued navigation, returning how many were dropped.
    ///
    /// This is CDP's `Page.stopLoading` reaching in. It cancels what has been
    /// *asked for* and not yet performed; a document fetch already in flight is
    /// beyond it, because the page thread is inside that fetch and services no
    /// ordinary job until it returns (ADR-0027 D3).
    pub fn clear_pending_navigations(&self) -> usize {
        let mut queue = self.pending_navigation.borrow_mut();
        let dropped = queue.len();
        queue.clear();
        dropped
    }

    /// Queues a navigation for the page's event loop (see
    /// [`PageState::pending_navigation`] for what does and does not collapse).
    pub fn request_navigation(&self, navigation: PendingNavigation) {
        let mut queue = self.pending_navigation.borrow_mut();
        // A load supersedes a load that has not started yet.
        if matches!(navigation, PendingNavigation::Load { .. })
            && matches!(queue.back(), Some(PendingNavigation::Load { .. }))
        {
            queue.pop_back();
        }
        // A runaway loop of traversals or `javascript:` activations cannot grow
        // this without bound. The page performs at most
        // `MAX_CHAINED_NAVIGATIONS` per chain anyway, so a queue this deep is
        // already a script that has lost control.
        if queue.len() >= MAX_PENDING_NAVIGATIONS {
            return;
        }
        queue.push_back(navigation);
    }

    /// Takes the next queued navigation, if any.
    #[must_use]
    pub fn take_pending_navigation(&self) -> Option<PendingNavigation> {
        self.pending_navigation.borrow_mut().pop_front()
    }

    /// True when script has queued a navigation the page has not run yet.
    #[must_use]
    pub fn has_pending_navigation(&self) -> bool {
        !self.pending_navigation.borrow().is_empty()
    }

    /// The session history, for the page's navigation driver.
    #[must_use]
    pub fn history(&self) -> std::cell::RefMut<'_, SessionHistory> {
        self.history.borrow_mut()
    }

    /// `document.referrer` — the URL the current document was navigated from.
    #[must_use]
    pub fn referrer(&self) -> String {
        self.referrer.borrow().clone()
    }

    /// Sets `document.referrer` (the page, at commit time).
    pub fn set_referrer(&self, referrer: String) {
        *self.referrer.borrow_mut() = referrer;
    }
}

/// Key-addressed storage for non-node host data.
#[derive(Default)]
pub(crate) struct Slab {
    next: u64,
    entries: HashMap<u64, HostData>,
}

impl Slab {
    pub fn insert(&mut self, data: HostData) -> u64 {
        let key = self.next;
        self.next += 1;
        self.entries.insert(key, data);
        key
    }

    pub fn get(&self, key: u64) -> Option<&HostData> {
        self.entries.get(&key)
    }

    pub fn remove(&mut self, key: u64) -> Option<HostData> {
        self.entries.remove(&key)
    }
}
