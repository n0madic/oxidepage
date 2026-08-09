//! Per-page bindings state: the DOM tree, the host-object table, wrapper
//! bookkeeping, listener and observer registries, and embedder hooks.
//!
//! `WorldState` is installed into the realm as its host state; host callbacks
//! retrieve it through the scope instead of capturing it, so no JS object
//! ever holds a strong reference back to the page (see `oxidepage-js` docs).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};

use oxidepage_base::{FrameId, NodeId, RequestId};
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
use crate::filedata::{BlobData, FileReaderData};
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
    /// Schedules a timer in `world`; returns its id.
    ///
    /// The world is carried because the callback is a `JsValue` of that world
    /// and can be invoked nowhere else (ADR-0033 D5). Ids stay page-global, so
    /// `clearTimeout` needs no world.
    fn schedule_timer(
        &self,
        world: WorldId,
        callback: JsValue,
        args: Vec<JsValue>,
        delay_ms: f64,
        repeat: bool,
    ) -> f64;
    fn clear_timer(&self, id: f64);

    /// `requestAnimationFrame`: registers `callback` to run in `world` at the
    /// next rendering opportunity; returns its id.
    fn request_animation_frame(&self, world: WorldId, callback: JsValue) -> f64;
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

    /// An `<input type=file>` was activated (ADR-0032 D12).
    ///
    /// **Does not park the page**, unlike a modal dialog: a chooser has no
    /// return value the activation needs, so the click completes and the files
    /// arrive later through the embedder. The default does nothing, which is
    /// the honest headless answer — a page with no driver watching gets no
    /// chooser, exactly as a real browser with no user gets no selection.
    fn open_file_chooser(&self, input: NodeId, multiple: bool) {
        let _ = (input, multiple);
    }

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
    /// `performance.timing` (state lives in [`WorldState::timing`]).
    PerformanceTiming,
    /// `document.fonts` (one per document; state lives in
    /// [`WorldState::fonts_loading`]/[`WorldState::font_ready_resolvers`]).
    FontFaceSet,
    /// The single `window.customElements` registry brand (state lives in
    /// [`WorldState::custom_elements`]).
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
    /// A `Blob` **or** a `File`: one backing record for both, distinguished by
    /// [`BlobData::file`] (ADR-0032 D10). `slice` shares the buffer, so the
    /// `Rc<Vec<u8>>` inside is what several of these commonly point at.
    Blob(Rc<BlobData>),
    /// An `<input type=file>`'s `files` (fixed snapshot; `item()` mints fresh
    /// `File` wrappers over the same `BlobData`s).
    FileList(Rc<Vec<Rc<BlobData>>>),
    FileReader(Rc<FileReaderData>),
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
    /// `new XMLSerializer()`: likewise a brand — the node to serialize is the
    /// argument, not state.
    XmlSerializer,
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
    /// [`WorldState::history`].
    History,
}

/// A navigation script has asked for but the page has not yet performed.
///
/// Navigation cannot happen inline: a `location.href` write runs under live
/// `RefCell` borrows on the DOM, style and layout engines, and committing a
/// document replaces all three. So it is a task source, drained by the page's
/// event loop exactly like [`WorldState::pending_scroll_targets`].
pub enum PendingNavigation {
    /// A load of `url`, which is already absolute. `replace` overwrites the
    /// current session-history entry instead of pushing a new one.
    Load {
        url: String,
        replace: bool,
        body: Option<NavigationBody>,
        /// `location.reload()`: skip the HTTP cache.
        reload: bool,
        /// `<a download>`: this navigation is a download request, and the
        /// payload is the attribute's value — the author's suggested filename,
        /// empty when the attribute was bare.
        ///
        /// `Some` makes the response a download **whatever** its
        /// `Content-Disposition` says, which is what the attribute means and
        /// what Chrome does for a same-origin link. It does not decide whether
        /// anything is *written*: that is still the operator's
        /// `DownloadBehavior`, which denies by default.
        download: Option<String>,
    },
    /// `history.go(delta)`. The entry list lives here in the bindings, but a
    /// traversal may need a document load, so the page performs the move.
    Traverse { delta: i32 },
    /// A `javascript:` URL, already percent-decoded. Queued rather than run
    /// inline for the same reason every other navigation is: the activation
    /// that produced it runs under live borrows, and the script may replace the
    /// document.
    JavaScriptUrl { source: String },
    /// The markup a script-created parser collected between `document.open()`
    /// and `document.close()` (ADR-0034 D2).
    ///
    /// A navigation and not a special case: it replaces the document in place,
    /// keeps the URL, and must reach the same commit path so the milestones,
    /// the world rebuild and the context re-announcement all happen. Queued for
    /// exactly the reason `JavaScriptUrl` is — `close()` runs inside JS, under
    /// live borrows on the DOM, style and layout.
    ReplaceDocument {
        html: String,
        /// Whether the realms survive it (ADR-0034 D2).
        ///
        /// True for `document.open()`, where HTML keeps the `Document`, the
        /// `Window` and the environment settings object. False for an
        /// embedder's `Page::load_html`, which is a navigation in every way
        /// that matters — it is queued through here only when the page is
        /// suspended, and must not quietly acquire `open()`'s semantics by
        /// sharing its variant.
        preserve_contexts: bool,
    },
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
    /// The `history.state` for this entry, as JSON text; `None` reads back as
    /// `null`.
    ///
    /// **Not a `JsValue`** (ADR-0033 D3). The session history is page-level and
    /// outlives any one world, so holding a live handle would both pin one
    /// world's object graph from `FrameShared` — the invariant that keeps
    /// teardown from aborting in `JS_FreeRuntime` — and make `history.state`
    /// readable in exactly one world. Serialized, every world materializes its
    /// own copy, which is also what the spec's structured clone implies.
    pub state: Option<String>,
    /// Which loaded document this entry belongs to. Traversing to an entry
    /// whose sequence differs from the current one needs a document load.
    pub document_seq: u64,
}

/// The session history of the page's one browsing context.
///
/// Bounded on purpose: an entry holds a serialized state across navigations, so
/// an unbounded list is unbounded retention.
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

/// The deepest [`WorldState::pending_navigation`] queue. Requests past it are
/// dropped: the page performs at most `MAX_CHAINED_NAVIGATIONS` off one entry
/// point regardless, so anything queueing more than this in a single task is a
/// runaway loop rather than a page with intent.
pub const MAX_PENDING_NAVIGATIONS: usize = 32;

impl SessionHistory {
    fn new(url: String) -> Self {
        Self {
            entries: vec![HistoryEntry {
                url,
                state: None,
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
    pub fn push(&mut self, url: String, state: Option<String>, document_seq: u64) {
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
    pub fn replace(&mut self, url: String, state: Option<String>, document_seq: u64) {
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
    /// `navigator.languages`. Interior mutability because
    /// `Emulation.setLocaleOverride` moves it at runtime (ADR-0034 D6) and this
    /// whole struct is shared as an `Rc` by every world.
    pub languages: RefCell<Vec<String>>,
    pub hardware_concurrency: u64,
    pub webdriver: bool,
    pub max_touch_points: u32,
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
            languages: RefCell::new(languages),
            hardware_concurrency,
            webdriver,
            max_touch_points,
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

/// A document-lifecycle milestone the page records on [`WorldState::timing`].
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
    /// Pristine `JSON.stringify` / `JSON.parse`, for the serialized
    /// `history.state` round trip (ADR-0033 D3).
    pub json_stringify: JsValue,
    pub json_parse: JsValue,
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
    /// `(parts) => Array` — normalizes `new Blob(parts)`'s
    /// `sequence<BlobPart>` argument, which the codegen cannot express.
    pub blob_parts: JsValue,
    /// `(part) => string | null` — an `ArrayBuffer`/`ArrayBufferView` part's
    /// bytes as a Latin-1 string. `JsScope` can create an `ArrayBuffer` but
    /// not read one.
    pub blob_part_bytes: JsValue,
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
/// the registry in [`WorldState::resize_observers`] is cleared on navigation so
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

/// Dense index of an execution world within a page; `MAIN_WORLD` is the page's
/// own world (ADR-0033).
///
/// Deliberately **not** part of any embedder-facing API. A `WorldId` is reused
/// when a world is rebuilt at a commit, so a stale one would silently name a
/// live world; the monotonic `context_id` is what crosses the thread boundary
/// (D10), where a stale value is a clean error.
pub type WorldId = usize;

/// The main world: page script, inline event handlers, custom elements, and
/// every task source the DOM drives (script updates, custom-element reactions,
/// activation behaviour) run here.
pub const MAIN_WORLD: WorldId = 0;

/// A script run at the start of every new document
/// (CDP `Page.addScriptToEvaluateOnNewDocument`).
///
/// Page-level, and deliberately **not** cleared on navigation: surviving a
/// navigation is the whole point. Each carries the world it belongs to, so a
/// driver's `worldName` script runs in its utility world and nowhere else.
#[derive(Clone, Debug)]
pub struct InitScript {
    pub id: u64,
    pub source: String,
    /// `None` — or `Some("")` — is the main world.
    pub world: Option<String>,
}

/// A payload an `addBinding` function handed back, tagged with the context id
/// of the world it was called from so `Runtime.bindingCalled` attributes it
/// correctly instead of guessing the main world.
#[derive(Clone, Debug)]
pub struct BindingCall {
    pub name: String,
    pub payload: String,
    pub context_id: u64,
}

/// Enters another world from a host callback (ADR-0033 D4).
///
/// `bindings` must not depend on `page`, so the page's world table implements
/// this and [`FrameShared`] holds a [`Weak`] to it. The `Weak` is load-bearing:
/// a strong edge would close the `Page` → `WorldTable` → realm → `WorldState`
/// → `FrameShared` cycle, leak every runtime, and turn a dropped page into the
/// `JS_FreeRuntime` abort.
pub trait WorldEnter {
    /// Runs `f` with a [`crate::cx::BindCx`] bound to `world`.
    ///
    /// Returns `false` — the call skipped, never retried — when the world is
    /// gone, is **already on the stack** (entering a live `Context` twice is a
    /// `BorrowMutError`), or the nesting cap is hit.
    fn enter(&self, world: WorldId, f: &mut dyn FnMut(&crate::cx::BindCx<'_>)) -> bool;
    /// Live world ids **of one frame**, main first then creation order — the
    /// cross-world listener order rule (D6). Scoped to a frame because events
    /// do not cross a document boundary (ADR-0035 D8), so a dispatch must
    /// never reach a sibling context's listeners.
    fn world_ids_of(&self, frame: FrameId) -> Vec<WorldId>;
    /// Whether `world` has any listener for `event_type` on `target`.
    ///
    /// A plain map read on the page thread: **no scope is entered**, which is
    /// what keeps cross-world dispatch free for a page whose utility worlds are
    /// idle. Entering a world to discover it had nothing to do would cost a
    /// realm entry per node per phase.
    fn has_listener(
        &self,
        world: WorldId,
        target: crate::events::EventTargetKey,
        event_type: &str,
    ) -> bool;
}

/// State shared by every browsing context of one page (ADR-0035 D2).
///
/// A page holds one [`FrameShared`] per frame; this is what those frames share.
/// The rule for what belongs here is "would two frames disagreeing about it be
/// a bug?" — id minting must be page-unique or a handle would reach the wrong
/// frame's object; a driver's `addScriptToEvaluateOnNewDocument` and
/// `addBinding` apply to every frame, as they do in Chrome; and the
/// connectivity log tracks nodes of one shared arena.
///
/// Everything that is genuinely per-document — the engines, the session
/// history, the parser flags, the ready state — stays on [`FrameShared`].
///
/// Like [`FrameShared`], this **holds no `JsValue`**.
pub struct PageGlobal {
    /// Scripts run at the start of every new document, in insertion order.
    ///
    /// Deliberately **not** cleared by `reset_for_navigation`: the whole point
    /// is that they survive navigation. That is what makes a driver's
    /// `exposeFunction` and `evaluateOnNewDocument` still be there on the next
    /// page (CDP `Page.addScriptToEvaluateOnNewDocument`).
    pub init_scripts: RefCell<Vec<InitScript>>,
    pub next_init_script: Cell<u64>,
    /// Payloads delivered by `Runtime.addBinding` functions, drained by the
    /// page into `Runtime.bindingCalled`.
    pub binding_calls: RefCell<VecDeque<BindingCall>>,
    /// Mints `Runtime.ExecutionContextId`s. Monotonic across documents, frames
    /// *and* worlds, which is what `ISOLATED_WORLD_ID_OFFSET` was faking (D10).
    pub(crate) next_context_id: Cell<u64>,
    /// `Runtime.addBinding` registrations, `(name, world name)`. Re-applied to
    /// every world at every commit, because a commit rebuilds the worlds.
    pub(crate) bindings: RefCell<Vec<(String, Option<String>)>>,
    /// Which world started each in-flight `fetch`/XHR, so a `NetEvent` is
    /// delivered to the world whose promise is waiting on it.
    pub(crate) net_world: RefCell<HashMap<RequestId, WorldId>>,
    /// Append-only connectivity log behind ADR-0018's retention guarantee.
    ///
    /// `DomTree::take_pinned_connectivity` is destructive, so with N worlds the
    /// first to drain would starve every other and the expando guarantee would
    /// silently break for the rest (ADR-0033 D7). Each entry is
    /// `(sequence, node, connected)`; a world consumes from its own cursor and
    /// the log is trimmed below the minimum live cursor. Page-level rather than
    /// per-frame because the arena it describes is one.
    pub(crate) connectivity: RefCell<VecDeque<(u64, NodeId, bool)>>,
    pub(crate) connectivity_seq: Cell<u64>,
    /// How far each live world has consumed the log. Kept here rather than on
    /// `WorldState` because trimming needs the *minimum* across worlds, and a
    /// world cannot read another world's state. A torn-down world's entry is
    /// removed, so it never holds the trim back.
    pub(crate) conn_cursors: RefCell<HashMap<WorldId, u64>>,
    /// Mints page-unique `objectId`s (see `next_object_id`).
    pub(crate) next_object_id: Cell<u64>,
    /// Which world's object store holds each live `objectId`, so a handle is
    /// always called in the world that owns it (ADR-0033 D10).
    pub(crate) object_worlds: RefCell<HashMap<u64, WorldId>>,
    /// Reaches another world's realm from a host callback. **Weak**: a strong
    /// edge would close the `Page -> WorldTable -> realm -> WorldState ->
    /// FrameShared -> PageGlobal` cycle and leak every runtime (D4).
    pub(crate) enter: RefCell<Weak<dyn WorldEnter>>,
}

impl PageGlobal {
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            init_scripts: RefCell::new(Vec::new()),
            next_init_script: Cell::new(0),
            binding_calls: RefCell::new(VecDeque::new()),
            // 0 is never handed out: the first world takes 1, matching the id
            // the protocol has always reported for the main context.
            next_context_id: Cell::new(0),
            bindings: RefCell::new(Vec::new()),
            net_world: RefCell::new(HashMap::new()),
            connectivity: RefCell::new(VecDeque::new()),
            connectivity_seq: Cell::new(0),
            conn_cursors: RefCell::new(HashMap::new()),
            next_object_id: Cell::new(0),
            object_worlds: RefCell::new(HashMap::new()),
            enter: RefCell::new(Weak::<crate::state::NoWorlds>::new()),
        })
    }

    /// Mints the next `Runtime.ExecutionContextId`.
    pub(crate) fn next_context_id(&self) -> u64 {
        let id = self.next_context_id.get() + 1;
        self.next_context_id.set(id);
        id
    }

    /// Installs the hop into other worlds. Called once, by the page, after the
    /// main world exists — the table cannot exist before its first world does.
    pub fn set_world_enter(&self, enter: Weak<dyn WorldEnter>) {
        *self.enter.borrow_mut() = enter;
    }

    /// Runs `f` in `world`. See [`WorldEnter::enter`] for when this is skipped.
    pub(crate) fn in_world(
        &self,
        world: WorldId,
        mut f: impl FnMut(&crate::cx::BindCx<'_>),
    ) -> bool {
        let Some(table) = self.enter.borrow().upgrade() else {
            return false;
        };
        table.enter(world, &mut f)
    }

    /// Whether `world` has a listener for `event_type` on `target`.
    #[must_use]
    pub(crate) fn world_has_listener(
        &self,
        world: WorldId,
        target: crate::events::EventTargetKey,
        event_type: &str,
    ) -> bool {
        self.enter
            .borrow()
            .upgrade()
            .is_some_and(|table| table.has_listener(world, target, event_type))
    }

    /// Mints `objectId`s for **every** world's object store.
    ///
    /// Page-unique rather than per-store: an id names one value, and two
    /// worlds handing out `1` would let `Runtime.callFunctionOn` reach the
    /// wrong world's object. Monotonic and never recycled, so a stale id names
    /// nothing.
    pub(crate) fn next_object_id(&self) -> u64 {
        let id = self.next_object_id.get() + 1;
        self.next_object_id.set(id);
        id
    }

    /// The world whose store holds `object_id`.
    #[must_use]
    pub fn object_world(&self, object_id: u64) -> Option<WorldId> {
        self.object_worlds.borrow().get(&object_id).copied()
    }

    pub(crate) fn note_object_world(&self, object_id: u64, world: WorldId) {
        self.object_worlds.borrow_mut().insert(object_id, world);
    }

    pub fn forget_object(&self, object_id: u64) {
        self.object_worlds.borrow_mut().remove(&object_id);
    }

    /// Forgets every handle of one world (a commit, or a world teardown).
    pub fn forget_objects_of(&self, world: WorldId) {
        self.object_worlds.borrow_mut().retain(|_, w| *w != world);
    }

    /// Records which world started a script-initiated request, so its
    /// `NetEvent`s are delivered where the waiting promise lives.
    pub(crate) fn note_net_world(&self, id: RequestId, world: WorldId) {
        self.net_world.borrow_mut().insert(id, world);
    }

    /// The world that started `id`, if it is still tracked.
    #[must_use]
    pub fn net_world_of(&self, id: RequestId) -> Option<WorldId> {
        self.net_world.borrow().get(&id).copied()
    }

    /// Drops every request-to-world mapping (a commit aborts them all).
    pub fn clear_net_worlds(&self) {
        self.net_world.borrow_mut().clear();
    }

    /// Forgets a request that has reached a terminal event.
    pub fn forget_net_world(&self, id: RequestId) {
        self.net_world.borrow_mut().remove(&id);
    }

    /// How far `world` has consumed the connectivity log.
    #[must_use]
    pub(crate) fn conn_cursor(&self, world: WorldId) -> u64 {
        self.conn_cursors.borrow().get(&world).copied().unwrap_or(0)
    }

    pub(crate) fn set_conn_cursor(&self, world: WorldId, seq: u64) {
        self.conn_cursors.borrow_mut().insert(world, seq);
    }

    /// Empties the log and every cursor at a document commit.
    ///
    /// The log is trimmed below the *minimum* live cursor, so without this a
    /// world that has not drained since the outgoing document keeps every entry
    /// alive across navigations — and the ids in them name an arena that is
    /// gone.
    pub fn reset_connectivity(&self) {
        self.connectivity.borrow_mut().clear();
        self.conn_cursors.borrow_mut().clear();
        self.connectivity_seq.set(0);
    }

    /// Forgets a torn-down world, so its stale cursor stops pinning the log.
    pub fn forget_world_cursor(&self, world: WorldId) {
        self.conn_cursors.borrow_mut().remove(&world);
    }

    /// `Runtime.addBinding` registrations, re-applied to every rebuilt world.
    pub fn bindings(&self) -> std::cell::RefMut<'_, Vec<(String, Option<String>)>> {
        self.bindings.borrow_mut()
    }
}

/// One browsing context's bindings state: everything that is one-per-*document*
/// rather than one-per-*world* (ADR-0033 D3).
///
/// ADR-0033 named this `PageShared`, because a page had exactly one browsing
/// context and therefore one document. The partition it drew was already the
/// right one; nested browsing contexts (ADR-0035 D2) simply make it plural, so
/// a page now holds one of these per frame. What is genuinely page-wide —
/// context and object id minting, driver registrations, the connectivity log —
/// moves to `PageGlobal`.
///
/// **Holds no `JsValue`.** That is the invariant that makes teardown tractable
/// with one `Runtime` per world: a `Persistent` outliving its runtime aborts
/// the process in `JS_FreeRuntime`, and a shared container could otherwise
/// hold values from several worlds at once. `SessionHistory` was the one
/// violator and now stores serialized state (D3/D5); the only shared owner
/// of JS values left is `LoopHooks`, whose timer and rAF callbacks each carry
/// the world that created them.
pub struct FrameShared {
    /// State shared by every browsing context of the page (ADR-0035 D2).
    pub global: Rc<PageGlobal>,
    /// Which browsing context this is. Fixed for the state's whole life — a
    /// commit replaces the *document*, never the frame.
    frame: FrameId,
    /// This frame's **default** world — its `window`, the one a driver sees as
    /// `isDefault`.
    ///
    /// Not the `MAIN_WORLD` constant: world ids are unique page-wide because
    /// the table is flat, so only the top-level frame's default world is id 0
    /// (ADR-0035 D3). Reading the constant instead is how `customElements`
    /// ends up installed in exactly one frame.
    default_world: Cell<WorldId>,
    /// The document tree, shared with the parser during loads.
    ///
    /// The arena is shared by every browsing context of the page; **this**
    /// context's document is [`Self::document`] (ADR-0035 D1).
    pub dom: Rc<RefCell<DomTree>>,
    /// The rendered document of the browsing context this state belongs to.
    ///
    /// A `Cell` because a commit replaces the document node. For the top-level
    /// context that node is always arena slot `(0, gen 1)`, which is what lets
    /// the JS `document` wrapper survive navigation; a nested context gets a
    /// fresh id per commit.
    pub(crate) document: Cell<NodeId>,
    /// The style engine (stylo): document stylesheet set + computed values.
    pub style: Rc<RefCell<StyleEngine>>,
    /// The layout engine (box tree + taffy/parley): backs the geometry APIs.
    pub layout: Rc<RefCell<LayoutEngine>>,
    pub(crate) hooks: Rc<dyn HostHooks>,
    /// Immutable Navigator profile associated with this realm.
    pub(crate) navigator: Rc<NavigatorData>,
    /// Immutable Screen profile and its realm-stable wrapper.
    pub(crate) screen: Rc<ScreenData>,
    /// This document's identity among the subscribers of its storage areas, so
    /// a `storage` event is delivered to the *other* documents and never back
    /// to the one that wrote (HTML).
    pub(crate) storage_subscriber: crate::storage::StorageSubscriber,
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
    /// HTML's **firing submission events** flag: set only while the `submit`
    /// event of a submission is being dispatched, so a `requestSubmit()` or a
    /// submit-button activation raised from inside `onsubmit` cannot recurse
    /// forever. `form.submit()` fires no event and is not blocked by it, which
    /// is what makes validate-then-submit work.
    pub(crate) firing_submission_events: Cell<bool>,
    /// While the parser holds handles into the tree, detached subtrees must
    /// not be freed under it.
    pub(crate) parsing: Cell<bool>,
    /// `document.readyState`, driven by [`WorldState::mark_timing`].
    pub(crate) ready_state: Cell<ReadyState>,
    /// True only while a parser-inserted classic script is evaluating.
    pub(crate) parser_script_active: Cell<bool>,
    /// Text produced by `document.write` during the current parser script.
    pub(crate) pending_parser_write: RefCell<String>,
    /// Per-document bounds for parser writes.
    pub(crate) parser_write_calls: Cell<usize>,
    pub(crate) parser_write_bytes: Cell<usize>,
    /// Markup a **script-created parser** has collected (ADR-0034 D2).
    ///
    /// `document.open()` opens this buffer, `write`/`writeln` append to it, and
    /// `close()` hands it to the page as a document replacement. `None` means
    /// no script-created parser is open, which is the ordinary state and the
    /// one in which `write` still goes to the real parser.
    pub(crate) script_parser_buffer: RefCell<Option<String>>,
    /// How much of [`Self::script_parser_buffer`] the task-boundary flush has
    /// already committed, so an unclosed parser re-commits only what is new.
    pub(crate) script_parser_flushed: Cell<usize>,
    /// `console.group` nesting depth. Grouping has no other observable effect
    /// in a headless console, so the depth *is* the feature — it rides on
    /// every `ConsoleMessage` and the CLI indents by it.
    pub(crate) console_group_depth: Cell<u32>,
    /// The classic `<script>` element whose source is currently evaluating.
    /// Modules and callbacks outside direct script evaluation observe `None`.
    pub current_script: Cell<Option<NodeId>>,
    /// Mirrors `Page::pending_fonts.is_empty()` (the page owns the actual
    /// in-flight `@font-face` loads; it writes this directly through its
    /// `Rc<WorldState>` whenever that set changes — `pub`, unlike this
    /// struct's other bindings-internal cells, because the page crate sets
    /// it). Backs `FontFaceSet.status` and the synchronous-resolve check in
    /// `ready`.
    pub fonts_loading: Cell<bool>,
    /// Document navigation/timing milestones behind `performance.timing`.
    pub(crate) timing: RefCell<DocumentTiming>,
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
    /// The page's world registry, main world first then creation order.
    ///
    /// Ids only, deliberately: a world's name and `context_id` live on the
    /// world table and on the live [`WorldState::context_id`], and mirroring
    /// them here bought nothing but drift — `reset_for_navigation` mints a new
    /// context id on the `Cell` and a copy stored here would keep answering
    /// with the dead one.
    pub(crate) worlds: RefCell<Vec<WorldId>>,
}

/// All bindings state for one *world* — one `Runtime`, one `Context`, one
/// global (ADR-0033 D1). This is what `realm.set_state` installs and what
/// `BindCx.state` points at.
///
/// It keeps `Rc`-clones of `dom`, `style`, `layout` and `hooks` so the ~334
/// `cx.state.dom` / `.style` / `.layout` / `.hooks` sites across `imp/` read
/// identically whichever world they run in; page-level state is one hop away
/// through [`WorldState::page`].
pub struct WorldState {
    /// State shared with every other world of this page.
    pub frame: Rc<FrameShared>,
    /// This world's dense index; `MAIN_WORLD` for the page's own world.
    pub id: WorldId,
    /// `""` for the main world, else the name a driver created it under.
    pub name: String,
    /// CDP's `Runtime.ExecutionContextId` for this world, re-minted on every
    /// commit because a commit rebuilds the world against a fresh global.
    pub context_id: Cell<u64>,
    /// The document tree, shared with the parser during loads.
    pub dom: Rc<RefCell<DomTree>>,
    /// The style engine (stylo): document stylesheet set + computed values.
    pub style: Rc<RefCell<StyleEngine>>,
    /// The layout engine (box tree + taffy/parley): backs the geometry APIs.
    pub layout: Rc<RefCell<LayoutEngine>>,
    pub(crate) hooks: Rc<dyn HostHooks>,
    /// Immutable Navigator profile associated with this realm.
    pub(crate) navigator: Rc<NavigatorData>,
    /// Immutable Screen profile and its realm-stable wrapper.
    pub(crate) screen: Rc<ScreenData>,
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
    /// Every `Storage` handle installed in this realm, so a navigation can
    /// re-point them all at the new origin's areas (see `refresh_storage`).
    pub(crate) storage_handles: RefCell<Vec<Rc<StorageHandle>>>,
    /// In-flight `fetch`/XHR requests awaiting completion, keyed by id.
    pub(crate) pending_net: RefCell<HashMap<RequestId, PendingNet>>,
    /// Realm-stable `location` / `history` wrappers (the `navigator_js`
    /// pattern: one object per realm, surviving navigation).
    pub(crate) location_js: RefCell<Option<JsValue>>,
    pub(crate) history_js: RefCell<Option<JsValue>>,
    /// The DOM spec's "mutation observer microtask queued" flag. Set when a
    /// record is queued and the compound microtask is enqueued; cleared when
    /// that microtask runs, so at most one is outstanding.
    pub(crate) mutation_microtask_queued: Cell<bool>,
    /// The realm's associated Navigator wrapper (`navigator` and
    /// `clientInformation` both return this exact object).
    pub(crate) navigator_js: RefCell<Option<JsValue>>,
    pub(crate) screen_js: RefCell<Option<JsValue>>,
    /// Realm-stable `performance` wrapper.
    pub(crate) performance_js: RefCell<Option<JsValue>>,
    /// Realm-stable `performance.timing` wrapper (`[SameObject]`).
    pub(crate) performance_timing_js: RefCell<Option<JsValue>>,
    /// Realm-stable `document.fonts` wrapper (`[SameObject]`; one document,
    /// so — like `performance_js` — a single cell rather than a
    /// per-node `same_object` entry).
    pub(crate) font_face_set_js: RefCell<Option<JsValue>>,
    /// `document.fonts.ready` promises stashed by `imp::font_face_set::ready`
    /// when it could not resolve synchronously (a font load is in flight, or
    /// the document is still parsing so more `@font-face` rules could still
    /// turn up). The page's event loop resolves and drains these once
    /// `fonts_loading` goes false and parsing has finished
    /// (`oxidepage_bindings::resolve_font_ready`).
    pub(crate) font_ready_resolvers: RefCell<Vec<JsValue>>,
    /// Pending `document.fonts.load(...)` promise resolvers, drained alongside
    /// [`WorldState::font_ready_resolvers`] once fonts settle. Each resolves with
    /// an (empty) `sequence<FontFace>`.
    pub(crate) font_load_resolvers: RefCell<Vec<JsValue>>,
    /// Live MediaQueryList objects created in this realm.
    pub(crate) media_queries: RefCell<Vec<Rc<MediaQueryListData>>>,
    /// Registered `ResizeObserver`s (delivery source), cleared on navigation.
    pub(crate) resize_observers: RefCell<Vec<Rc<ResizeObserverData>>>,
    /// Registered `IntersectionObserver`s (delivery source), cleared on navigation.
    pub(crate) intersection_observers: RefCell<Vec<Rc<IntersectionObserverData>>>,
    /// Last observer-delivery gate stamp (skips reflow when nothing changed).
    ///
    /// **Per world**, next to the registries it gates. Page-level it was a
    /// starvation bug: `deliver_observations` fans out over the worlds, the
    /// main world's pass re-stamps the gate, and every world after it then
    /// short-circuits — so an isolated world's `ResizeObserver` /
    /// `IntersectionObserver` callbacks never fired at all whenever the main
    /// world had an observer of its own.
    pub(crate) obs_gate: Cell<Option<ObsGate>>,
    /// Set by `observe()` to force exactly one geometry pass even when the gate
    /// is unchanged (the initial delivery of a freshly observed target). Cleared
    /// after that pass, so a boxless target does not keep the gate bypassed.
    /// Per world, for the reason [`WorldState::obs_gate`] gives.
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
    /// `navigator.languages` / `navigator.plugins` / `navigator.mimeTypes`,
    /// cached per world.
    ///
    /// These used to live on the shared `NavigatorData`, which was wrong twice
    /// once there is more than one world: the value is a `JsValue` of whichever
    /// world asked first, so a second world would get a foreign handle
    /// (`restore` refuses it), and a page-level holder of JS values breaks the
    /// teardown invariant (ADR-0033 D3).
    pub(crate) languages_js: RefCell<Option<JsValue>>,
    pub(crate) plugins_js: RefCell<Option<JsValue>>,
    pub(crate) mime_types_js: RefCell<Option<JsValue>>,
    /// Events whose subinterface payload holds a `JsValue` **of this world**,
    /// held weakly (ADR-0033 D5).
    ///
    /// An `EventData` is shared by every world that wraps it, and a world's
    /// slab is not cleared on navigation — so a main-world wrapper can keep a
    /// utility world's `CustomEvent.detail` alive past that world's teardown,
    /// and freeing the runtime under it aborts the process in
    /// `JS_FreeRuntime`. `release_js` walks this and clears the values it owns
    /// while the runtime is still alive. Weak, so an event nobody holds is
    /// collected normally and this never keeps one alive.
    pub(crate) owned_event_values: RefCell<Vec<std::rc::Weak<RefCell<crate::events::EventData>>>>,
    /// This world's materialized `history.state`, keyed by the serialized text
    /// it was parsed from, so `history.state === history.state` holds within a
    /// world (ADR-0033 D5).
    pub(crate) history_state_cache: RefCell<Option<(String, JsValue)>>,
    /// Wrappers minted for nodes that were *disconnected* at mint time, held
    /// until this world's next connectivity drain promotes them into
    /// `connected_wrappers` or drops them.
    ///
    /// Closes the window where another world connects a node and triggers a GC
    /// before this one next runs. It also fixes a latent single-world bug: a
    /// node wrapped while detached and then connected by the *parser* could
    /// lose its expandos to a GC before the deferred drain.
    pub(crate) pending_conn: RefCell<HashMap<NodeId, JsValue>>,
}

impl FrameShared {
    /// Builds the page-level half, including the engines every world shares.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        global: Rc<PageGlobal>,
        frame: FrameId,
        document: NodeId,
        dom: Rc<RefCell<DomTree>>,
        hooks: Rc<dyn HostHooks>,
        viewport: Viewport,
        navigator: NavigatorData,
        screen: ScreenData,
    ) -> Rc<Self> {
        Self::from_parts(
            global,
            frame,
            document,
            dom,
            hooks,
            viewport,
            Rc::new(navigator),
            Rc::new(screen),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        global: Rc<PageGlobal>,
        frame: FrameId,
        document: NodeId,
        dom: Rc<RefCell<DomTree>>,
        hooks: Rc<dyn HostHooks>,
        viewport: Viewport,
        navigator: Rc<NavigatorData>,
        screen: Rc<ScreenData>,
    ) -> Rc<Self> {
        let mut style_engine = StyleEngine::for_document(&dom.borrow(), document, viewport);
        let layout = Rc::new(RefCell::new(LayoutEngine::new(viewport)));
        // Wire real font metrics (parley/skrifa) into the cascade so
        // `ex`/`ch`/`ic` units resolve against actual fonts (WP-H).
        style_engine.set_font_metrics_provider(layout.borrow().font_metrics_factory());
        let style = Rc::new(RefCell::new(style_engine));
        let initial_url = dom.borrow().document_url_of(document).to_owned();
        let start = std::time::Instant::now();
        let time_origin_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0;
        Rc::new(Self {
            global,
            frame,
            default_world: Cell::new(MAIN_WORLD),
            dom,
            document: Cell::new(document),
            style,
            layout,
            hooks,
            navigator,
            screen,
            storage_subscriber: crate::storage::StorageSubscriber::next(),
            pending_scroll_targets: RefCell::new(Vec::new()),
            pending_navigation: RefCell::new(VecDeque::new()),
            history: RefCell::new(SessionHistory::new(initial_url)),
            referrer: RefCell::new(String::new()),
            firing_submission_events: Cell::new(false),
            parsing: Cell::new(false),
            ready_state: Cell::new(ReadyState::default()),
            parser_script_active: Cell::new(false),
            pending_parser_write: RefCell::new(String::new()),
            parser_write_calls: Cell::new(0),
            parser_write_bytes: Cell::new(0),
            script_parser_buffer: RefCell::new(None),
            script_parser_flushed: Cell::new(0),
            console_group_depth: Cell::new(0),
            current_script: Cell::new(None),
            fonts_loading: Cell::new(false),
            timing: RefCell::new(DocumentTiming::default()),
            start,
            time_origin_epoch_ms,
            whole_document_visible: Cell::new(false),
            worlds: RefCell::new(Vec::new()),
        })
    }

    /// Which browsing context this state belongs to.
    #[must_use]
    pub fn frame(&self) -> FrameId {
        self.frame
    }

    /// This frame's default world — its `window`.
    #[must_use]
    pub fn default_world(&self) -> WorldId {
        self.default_world.get()
    }

    /// Names this frame's default world. Called once, when the frame's own
    /// realm is installed.
    pub fn set_default_world(&self, world: WorldId) {
        self.default_world.set(world);
    }

    /// State for a browsing context nested in this one.
    ///
    /// Shares the page-wide state, the arena, the hooks and the immutable
    /// `Navigator`/`Screen` profiles — a nested context is the same
    /// environment, one document down — and builds its own engine pair over
    /// `document`.
    #[must_use]
    pub fn new_child(&self, frame: FrameId, document: NodeId, viewport: Viewport) -> Rc<Self> {
        Self::from_parts(
            Rc::clone(&self.global),
            frame,
            document,
            Rc::clone(&self.dom),
            Rc::clone(&self.hooks),
            viewport,
            Rc::clone(&self.navigator),
            Rc::clone(&self.screen),
        )
    }

    /// The rendered document of this browsing context.
    ///
    /// Reach for this — not `dom.document()` — wherever the question is "which
    /// document is this realm's": the arena holds one per browsing context
    /// (ADR-0035 D1), and `dom.document()` is only ever the top-level one.
    #[must_use]
    pub fn document(&self) -> NodeId {
        self.document.get()
    }

    /// Points this browsing context at the document a commit produced.
    pub fn set_document(&self, document: NodeId) {
        self.document.set(document);
    }

    /// Live world ids, main first.
    ///
    /// The world table is authoritative when one is installed, because only it
    /// knows which worlds are *live* — a world torn down at a commit is gone
    /// from the table before its registry entry is pruned. Without a table
    /// (a direct `install_world` embedder, and every `bindings` test) the
    /// registry is the answer: falling back to an empty list would mean no
    /// world ever matched, and event dispatch would silently deliver nothing.
    #[must_use]
    pub fn world_ids(&self) -> Vec<WorldId> {
        if let Some(table) = self.global.enter.borrow().upgrade() {
            return table.world_ids_of(self.frame);
        }
        self.worlds.borrow().clone()
    }

    /// Drops every isolated world from the registry.
    ///
    /// The page calls this at a commit, *before* rebuilding them: the rebuild
    /// goes through `install_world`, which re-appends, and an entry left behind
    /// would keep a torn-down world in `world_ids`' no-table fallback.
    pub fn forget_isolated_worlds(&self) {
        self.worlds.borrow_mut().retain(|id| *id == MAIN_WORLD);
    }
}

/// Uninhabited stand-in so `PageGlobal::enter` can start as a dangling `Weak`
/// before the world table exists. `Weak::new` needs a sized type; this is never
/// constructed.
pub(crate) enum NoWorlds {}

impl WorldEnter for NoWorlds {
    fn enter(&self, _world: WorldId, _f: &mut dyn FnMut(&crate::cx::BindCx<'_>)) -> bool {
        match *self {}
    }
    fn world_ids_of(&self, _frame: FrameId) -> Vec<WorldId> {
        match *self {}
    }
    fn has_listener(
        &self,
        _world: WorldId,
        _target: crate::events::EventTargetKey,
        _event_type: &str,
    ) -> bool {
        match *self {}
    }
}

impl WorldState {
    /// Builds one world over the shared state of a browsing context.
    #[must_use]
    pub fn new(frame: Rc<FrameShared>, id: WorldId, name: String) -> Self {
        let context_id = frame.global.next_context_id();
        Self {
            dom: Rc::clone(&frame.dom),
            style: Rc::clone(&frame.style),
            layout: Rc::clone(&frame.layout),
            hooks: Rc::clone(&frame.hooks),
            navigator: Rc::clone(&frame.navigator),
            screen: Rc::clone(&frame.screen),
            frame,
            id,
            name,
            context_id: Cell::new(context_id),
            slab: RefCell::new(Slab::default()),
            js: RefCell::new(None),
            listeners: RefCell::new(ListenerRegistry::default()),
            event_handlers: RefCell::new(HashMap::new()),
            handler_attr_seen: RefCell::new(HashMap::new()),
            observers: RefCell::new(Vec::new()),
            interfaces: RefCell::new(HashMap::new()),
            pending_consts: RefCell::new(Vec::new()),
            same_object: RefCell::new(HashMap::new()),
            storage_handles: RefCell::new(Vec::new()),
            pending_net: RefCell::new(HashMap::new()),
            location_js: RefCell::new(None),
            history_js: RefCell::new(None),
            mutation_microtask_queued: Cell::new(false),
            navigator_js: RefCell::new(None),
            screen_js: RefCell::new(None),
            performance_js: RefCell::new(None),
            performance_timing_js: RefCell::new(None),
            font_face_set_js: RefCell::new(None),
            font_ready_resolvers: RefCell::new(Vec::new()),
            font_load_resolvers: RefCell::new(Vec::new()),
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
            connected_wrappers: RefCell::new(HashMap::new()),
            adopted_sheets: RefCell::new(HashMap::new()),
            pending_conn: RefCell::new(HashMap::new()),
            owned_event_values: RefCell::new(Vec::new()),
            history_state_cache: RefCell::new(None),
            languages_js: RefCell::new(None),
            plugins_js: RefCell::new(None),
            mime_types_js: RefCell::new(None),
        }
    }

    /// This document's identity among the subscribers of its storage areas.
    #[must_use]
    pub fn storage_subscriber(&self) -> crate::storage::StorageSubscriber {
        self.frame.storage_subscriber
    }

    /// True for the page's own world. Custom elements, inline event handlers,
    /// activation behaviour and the DOM-driven task sources are main-world
    /// only (ADR-0033 D8).
    #[must_use]
    pub fn is_main(&self) -> bool {
        self.id == MAIN_WORLD
    }

    /// Whether this world has any listener (or `on*` handler) for `event_type`
    /// on `target`. Scope-free, for the cross-world dispatch probe.
    #[must_use]
    pub fn has_listener(&self, target: crate::events::EventTargetKey, event_type: &str) -> bool {
        !self
            .listeners
            .borrow()
            .snapshot(target, event_type)
            .is_empty()
            || self
                .event_handlers
                .borrow()
                .contains_key(&(target, event_type.to_owned()))
    }

    /// Takes this world's in-flight `fetch`/XHR ids, so the page can abort
    /// them before the world is destroyed.
    #[must_use]
    pub fn take_pending_net(&self) -> Vec<RequestId> {
        self.pending_net
            .borrow_mut()
            .drain()
            .map(|(id, _)| id)
            .collect()
    }

    /// Drops every JavaScript value this world holds.
    ///
    /// Called by the page's teardown **while this world's runtime is still
    /// alive** (ADR-0033 D4). A `Persistent` freed after `JS_FreeRuntime`
    /// aborts the process on a non-empty `gc_obj_list`, so the values must go
    /// first — and *releasing the values* is the mechanism rather than
    /// *counting the owners*, because `Page` deliberately keeps its own
    /// `Rc<WorldState>` for the main world and the realm holds a third as
    /// `Rc<dyn Any>`. Dropping one strong reference proves nothing; emptying
    /// the containers does.
    ///
    /// A new `JsValue`-holding field on `WorldState` must be cleared here. The
    /// cost of forgetting is a process abort at page teardown, which is what
    /// `dropping_a_page_with_live_worlds_is_clean` is for.
    pub fn release_js(&self) {
        // First: any event payload holding a value of *this* world. These live
        // in shared `EventData`s that other worlds' wrappers may still hold, so
        // dropping this world's containers alone would not reach them.
        for weak in self.owned_event_values.borrow_mut().drain(..) {
            if let Some(data) = weak.upgrade() {
                data.borrow_mut().release_values_of(self.id);
            }
        }
        // The wrapper cache and the interface table hold the prototypes
        // everything else points at, so they go last among the big ones; order
        // is not load-bearing within this function, only that it runs before
        // the runtime dies.
        self.slab.borrow_mut().clear();
        self.listeners.borrow_mut().clear();
        self.event_handlers.borrow_mut().clear();
        self.handler_attr_seen.borrow_mut().clear();
        self.observers.borrow_mut().clear();
        self.same_object.borrow_mut().clear();
        self.storage_handles.borrow_mut().clear();
        self.pending_net.borrow_mut().clear();
        self.font_ready_resolvers.borrow_mut().clear();
        self.font_load_resolvers.borrow_mut().clear();
        self.media_queries.borrow_mut().clear();
        self.resize_observers.borrow_mut().clear();
        self.intersection_observers.borrow_mut().clear();
        self.custom_elements.borrow_mut().clear();
        self.custom_wrappers.borrow_mut().clear();
        self.connected_wrappers.borrow_mut().clear();
        self.adopted_sheets.borrow_mut().clear();
        self.pending_conn.borrow_mut().clear();
        *self.history_state_cache.borrow_mut() = None;
        self.forget_navigator_caches();
        // The page's `objectId -> world` index must forget them too, or it
        // grows by one entry per handle the main world ever minted, for the
        // life of the page. Isolated worlds are pruned at teardown; this is the
        // main world's equivalent.
        for id in self.remote_objects.borrow().ids() {
            self.frame.global.forget_object(id);
        }
        self.remote_objects.borrow_mut().clear();
        self.pending_consts.borrow_mut().clear();
        self.named_prop_keys.borrow_mut().clear();
        *self.location_js.borrow_mut() = None;
        *self.history_js.borrow_mut() = None;
        *self.navigator_js.borrow_mut() = None;
        *self.screen_js.borrow_mut() = None;
        *self.performance_js.borrow_mut() = None;
        *self.performance_timing_js.borrow_mut() = None;
        *self.font_face_set_js.borrow_mut() = None;
        *self.named_props.borrow_mut() = None;
        self.interfaces.borrow_mut().clear();
        *self.js.borrow_mut() = None;
    }

    /// Marks the parser as owning handles into the tree (suspends freeing).
    pub fn set_parsing(&self, parsing: bool) {
        self.frame.parsing.set(parsing);
    }

    /// True while the parser holds handles into the tree.
    #[must_use]
    pub fn parsing(&self) -> bool {
        self.frame.parsing.get()
    }

    /// See [`WorldState::whole_document_visible`]. Set by the embedder before
    /// the page runs, so the observers script installs at startup see it.
    pub fn set_whole_document_visible(&self, whole: bool) {
        self.frame.whole_document_visible.set(whole);
    }

    /// The time origin behind [`WorldState::epoch_now_ms`]: the monotonic base
    /// and the wall-clock reading paired with it.
    ///
    /// Exposed so the embedder's own event loop can stamp its payloads off the
    /// *same* clock — a second, independently-seeded origin would put console
    /// lines and loop-emitted events on timescales that cannot be merged.
    #[must_use]
    pub fn time_origin(&self) -> (std::time::Instant, f64) {
        (self.frame.start, self.frame.time_origin_epoch_ms)
    }

    /// A monotonic Unix-epoch timestamp in milliseconds (time origin plus the
    /// monotonic clock elapsed since — never a fresh `SystemTime`, so timing
    /// milestones stay ordered even if the wall clock steps).
    #[must_use]
    pub fn epoch_now_ms(&self) -> f64 {
        self.frame.time_origin_epoch_ms + self.frame.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Records a document-lifecycle milestone on [`WorldState::timing`].
    /// `document.readyState`.
    #[must_use]
    pub fn ready_state(&self) -> ReadyState {
        self.frame.ready_state.get()
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
            M::NavigationStart => self.frame.ready_state.set(ReadyState::Loading),
            M::DomInteractive => self.frame.ready_state.set(ReadyState::Interactive),
            M::DomCompleteLoadStart => self.frame.ready_state.set(ReadyState::Complete),
            _ => {}
        }
        let mut t = self.frame.timing.borrow_mut();
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
        self.frame.parser_script_active.set(active);
    }

    /// Queues markup for the suspended HTML parser. `Ok(false)` means the
    /// call occurred outside an active parser-inserted script.
    pub(crate) fn queue_parser_write(&self, text: &str) -> Result<bool, &'static str> {
        const MAX_CALLS: usize = 1024;
        const MAX_BYTES: usize = 1024 * 1024;
        if !self.frame.parsing.get() || !self.frame.parser_script_active.get() {
            return Ok(false);
        }
        let calls = self.frame.parser_write_calls.get().saturating_add(1);
        let bytes = self
            .frame
            .parser_write_bytes
            .get()
            .saturating_add(text.len());
        if calls > MAX_CALLS || bytes > MAX_BYTES {
            return Err("document.write budget exceeded");
        }
        self.frame.parser_write_calls.set(calls);
        self.frame.parser_write_bytes.set(bytes);
        self.frame.pending_parser_write.borrow_mut().push_str(text);
        Ok(true)
    }

    pub fn take_parser_write(&self) -> String {
        std::mem::take(&mut *self.frame.pending_parser_write.borrow_mut())
    }

    // === the navigator profile (ADR-0034 D6) ===

    /// Replaces `navigator.languages` for every world at once.
    ///
    /// The data is page-level (one `Rc<NavigatorData>` shared by all worlds),
    /// so this is one write; the per-world frozen arrays are dropped by
    /// [`Self::forget_navigator_caches`].
    pub fn set_navigator_languages(&self, languages: Vec<String>) {
        *self.frame.navigator.languages.borrow_mut() = languages;
    }

    /// Drops the frozen `navigator.languages`/`plugins`/`mimeTypes` values
    /// this world handed out.
    ///
    /// They are `[SameObject]`-style caches, so a changed profile is invisible
    /// until they are dropped — which is what makes `Page::set_languages`
    /// observable to script that already read the list once.
    pub fn forget_navigator_caches(&self) {
        *self.languages_js.borrow_mut() = None;
        *self.plugins_js.borrow_mut() = None;
        *self.mime_types_js.borrow_mut() = None;
    }

    // === the script-created parser (ADR-0034 D2) ===

    /// Whether the HTML parser is running a parser-inserted script right now.
    ///
    /// HTML's document-open steps return early in exactly this state, and the
    /// reason is concrete: the write belongs to the parser that is running, not
    /// to a buffer that would later replace what it produced.
    #[must_use]
    pub fn parser_script_is_active(&self) -> bool {
        self.frame.parsing.get() && self.frame.parser_script_active.get()
    }

    /// `document.open()`: starts collecting markup that will replace the
    /// document. Re-opening an already-open buffer discards what it held,
    /// which is what a second `open()` means.
    pub fn open_script_parser(&self) {
        *self.frame.script_parser_buffer.borrow_mut() = Some(String::new());
        self.frame.script_parser_flushed.set(0);
    }

    /// Whether a script-created parser is collecting.
    #[must_use]
    pub fn script_parser_is_open(&self) -> bool {
        self.frame.script_parser_buffer.borrow().is_some()
    }

    /// The markup an **unclosed** script-created parser has accumulated, if it
    /// has grown since the last time this asked.
    ///
    /// The parser stays *open*: `document.open()` opens a document until
    /// `close()` or a navigation, so a `write` from a later task must still
    /// land. Taking the buffer here — which is what closing it means — made
    /// `document.open(); write(a);` followed by `setTimeout(() => write(b))`
    /// drop `b` on the floor, warn-and-noop, with no parser left to receive it.
    ///
    /// `None` when nothing is open, when nothing has been written, or when
    /// nothing has been written *since the last flush* — the last being what
    /// keeps the task boundary from re-committing the same markup every turn of
    /// the loop.
    #[must_use]
    pub fn unflushed_script_parser(&self) -> Option<String> {
        let buffer = self.frame.script_parser_buffer.borrow();
        let buffer = buffer.as_ref()?;
        if buffer.is_empty() || buffer.len() == self.frame.script_parser_flushed.get() {
            return None;
        }
        self.frame.script_parser_flushed.set(buffer.len());
        Some(buffer.clone())
    }

    /// Appends to the script-created parser, if one is open.
    ///
    /// Returns `false` when there is none, so the caller can fall through to
    /// the real parser — which is what keeps `document.write` **without**
    /// `open()` on exactly the path it always took.
    pub fn append_script_parser(&self, text: &str) -> Result<bool, &'static str> {
        const MAX_BYTES: usize = 1024 * 1024;
        let mut buffer = self.frame.script_parser_buffer.borrow_mut();
        let Some(buffer) = buffer.as_mut() else {
            return Ok(false);
        };
        if buffer.len().saturating_add(text.len()) > MAX_BYTES {
            return Err("document.write budget exceeded");
        }
        buffer.push_str(text);
        Ok(true)
    }

    /// Closes the script-created parser and hands back what it collected, if a
    /// commit is still owed.
    ///
    /// `None` when none was open — `close()` on a document nobody opened is a
    /// no-op, not an error — **and** when every byte it holds has already been
    /// committed by a task-boundary flush. Re-committing there would queue a
    /// second identical replacement: another `Started`/`Committed`/`load`, the
    /// document rebuilt from scratch, and every inline script in it run twice.
    ///
    /// An `open()` with nothing written is still a commit: it replaces the
    /// document with an empty one, which is what `open()` means.
    pub fn take_script_parser(&self) -> Option<String> {
        let flushed = self.frame.script_parser_flushed.replace(0);
        let buffer = self.frame.script_parser_buffer.borrow_mut().take()?;
        (flushed == 0 || buffer.len() > flushed).then_some(buffer)
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
        self.reset_for_navigation_with(/* new_context */ true)
    }

    /// [`Self::reset_for_navigation`], with the option to keep the execution
    /// context alive.
    ///
    /// `new_context == false` is the `document.open()` replacement (ADR-0034
    /// D2): HTML keeps the same `Window` and environment settings object, so
    /// renumbering would tell a driver its context died when it did not — and
    /// the driver would drop every later event naming the id it still holds.
    #[must_use]
    pub fn reset_for_navigation_with(&self, new_context: bool) -> Vec<RequestId> {
        let aborted = self
            .pending_net
            .borrow_mut()
            .drain()
            .map(|(id, _)| id)
            .collect();
        self.frame.pending_scroll_targets.borrow_mut().clear();
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
        // Provisional retentions name the outgoing document too, and each holds
        // a *strong* wrapper — leaving them would pin one detached subtree per
        // entry for the life of the world.
        self.pending_conn.borrow_mut().clear();
        // Parsed from the outgoing document's history entry; the incoming one
        // materializes its own.
        *self.history_state_cache.borrow_mut() = None;
        self.adopted_sheets.borrow_mut().clear();
        // Page-level, and only the main world's reset runs at a commit: the log
        // and every cursor name nodes of the document being replaced.
        if self.is_main() {
            self.frame.global.reset_connectivity();
        }
        // Every `objectId` named a value of the outgoing document. Keeping them
        // would pin that document's whole object graph for the life of the
        // realm, and would let a driver read a stale handle as if it were live.
        // The bumped context id is how the driver learns they all died at once.
        //
        // A replacement that keeps its context keeps them too (ADR-0034 D2):
        // the realm is the same one, so the values are genuinely still live,
        // and there is no bumped id to have told the driver otherwise. A handle
        // naming a *node* of the replaced tree is safe regardless — node ids
        // are generation-checked, so a stale one is a clean error rather than a
        // hit on whatever now occupies the slot.
        if new_context {
            self.remote_objects.borrow_mut().clear();
        }
        // A payload the outgoing document queued belongs to a world that is
        // gone; reporting it against the new document would attribute it to the
        // wrong execution context.
        //
        // A replacement destroys no world, so the payload is still owed to the
        // driver — and dropping it is the `exposeBinding` deadlock D1 exists to
        // remove: the driver is never told the binding was called, so it never
        // resolves the promise the page is waiting on. Reachable whenever a
        // `document.close()` lands before the loop reaches
        // `drain_binding_events`.
        if new_context {
            self.frame.global.binding_calls.borrow_mut().clear();
        }
        // Minted from the page's counter rather than bumped locally: ids must
        // be unique across documents *and* worlds, so a driver holding one from
        // the outgoing document gets a clean "no such context" (ADR-0033 D10).
        if new_context {
            self.context_id.set(self.frame.global.next_context_id());
        }
        self.dom.borrow_mut().clear_custom_elements();
        self.frame.current_script.set(None);
        self.frame.parser_script_active.set(false);
        self.mutation_microtask_queued.set(false);
        self.frame.pending_parser_write.borrow_mut().clear();
        self.frame.parser_write_calls.set(0);
        self.frame.parser_write_bytes.set(0);
        // A script-created parser belongs to the document that opened it, and a
        // real navigation ends it. A *replacement* does not: the commit it is
        // reacting to is the one this very parser produced, and script keeps
        // the document open until `close()`. Clearing it there would close the
        // parser as a side effect of its own first flush, and every later write
        // would land nowhere.
        if new_context {
            self.frame.script_parser_buffer.borrow_mut().take();
            self.frame.script_parser_flushed.set(0);
        }
        // A group the outgoing document opened must not indent the next one.
        self.frame.console_group_depth.set(0);
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
        std::mem::take(&mut self.frame.pending_scroll_targets.borrow_mut())
    }

    /// Queues a `scroll` event for `target` (`None` = the viewport) on the
    /// page's event loop — the same path a script scroll takes, so an
    /// embedder-driven scroll has no event path of its own.
    pub fn queue_scroll_event(&self, target: Option<oxidepage_base::NodeId>) {
        self.frame.pending_scroll_targets.borrow_mut().push(target);
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
        let mut queue = self.frame.pending_navigation.borrow_mut();
        let dropped = queue.len();
        queue.clear();
        dropped
    }

    /// Queues the replacement document a script-created parser collected
    /// (ADR-0034 D2).
    pub fn queue_document_replacement(&self, html: String) {
        self.request_navigation(PendingNavigation::ReplaceDocument {
            html,
            preserve_contexts: true,
        });
    }

    /// Queues an embedder's whole-document replacement (`Page::load_html`),
    /// which does **not** keep its realms.
    pub fn queue_embedder_document(&self, html: String) {
        self.request_navigation(PendingNavigation::ReplaceDocument {
            html,
            preserve_contexts: false,
        });
    }

    /// Queues a navigation for the page's event loop (see
    /// [`WorldState::pending_navigation`] for what does and does not collapse).
    pub fn request_navigation(&self, navigation: PendingNavigation) {
        let mut queue = self.frame.pending_navigation.borrow_mut();
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
        self.frame.pending_navigation.borrow_mut().pop_front()
    }

    /// True when script has queued a navigation the page has not run yet.
    #[must_use]
    pub fn has_pending_navigation(&self) -> bool {
        !self.frame.pending_navigation.borrow().is_empty()
    }

    /// The session history, for the page's navigation driver.
    #[must_use]
    pub fn history(&self) -> std::cell::RefMut<'_, SessionHistory> {
        self.frame.history.borrow_mut()
    }

    /// `document.referrer` — the URL the current document was navigated from.
    #[must_use]
    pub fn referrer(&self) -> String {
        self.frame.referrer.borrow().clone()
    }

    /// Sets `document.referrer` (the page, at commit time).
    pub fn set_referrer(&self, referrer: String) {
        *self.frame.referrer.borrow_mut() = referrer;
    }
}

/// Key-addressed storage for non-node host data.
#[derive(Default)]
pub(crate) struct Slab {
    next: u64,
    entries: HashMap<u64, HostData>,
}

impl Slab {
    /// Drops every host object. Called only by [`WorldState::release_js`], at
    /// page teardown, while this world's runtime is still alive.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

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
