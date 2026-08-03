//! DOM events at the bindings level: event state, the listener registry
//! (JS callbacks with `===` dedup semantics), and the dispatch algorithm.
//!
//! Dispatch lives here rather than in `oxidepage-dom` because listeners call
//! back into JS: the tree must not stay mutably borrowed across a callback
//! (listeners mutate the DOM). Every borrow below is short; JS runs with no
//! borrow held. The `dom` crate's native dispatch remains for engine-internal
//! use without JS.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

/// Event phases as observed by `Event.eventPhase`.
pub const PHASE_NONE: u16 = 0;
pub const PHASE_CAPTURING: u16 = 1;
pub const PHASE_AT_TARGET: u16 = 2;
pub const PHASE_BUBBLING: u16 = 3;

/// An event target: a DOM node, the window, or a standalone host EventTarget.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EventTargetKey {
    Node(NodeId),
    Window,
    MediaQueryList(u64),
    AbortSignal(u64),
    /// A standalone `new EventTarget()`: it is in no tree, so it is its own
    /// whole propagation path.
    Host(u64),
}

/// Lets the generated event-handler accessors take whatever their interface's
/// `this`-unwrap yields — a `NodeId` for elements and documents, an
/// `EventTargetKey` already for the Window — without branching per interface.
impl From<NodeId> for EventTargetKey {
    fn from(id: NodeId) -> Self {
        Self::Node(id)
    }
}

/// The four modifier keys, shared by mouse and keyboard events (WebIDL's
/// `EventModifierInit`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Modifiers {
    /// `getModifierState(key)`. Only the four keys this engine tracks answer
    /// `true`; every other modifier name is honestly `false` rather than an
    /// error, which is what the method is specified to do.
    #[must_use]
    pub fn state(self, key: &str) -> bool {
        match key {
            "Control" => self.ctrl,
            "Shift" => self.shift,
            "Alt" => self.alt,
            "Meta" => self.meta,
            _ => false,
        }
    }
}

/// `MouseEvent` state, shared by `WheelEvent` and `PointerEvent` — both *are*
/// mouse events, so every mouse getter has to work on them.
#[derive(Clone, Default)]
pub struct MouseFields {
    pub screen_x: f64,
    pub screen_y: f64,
    pub client_x: f64,
    pub client_y: f64,
    /// `offsetX`/`offsetY`, relative to the target's padding-box origin,
    /// resolved when the event is synthesized because the target is fixed then.
    ///
    /// `None` for a *constructed* event, which has no target: the spec makes
    /// `offsetX` equal `pageX` in that case, and `pageX` tracks the document
    /// scroll at read time — so it cannot be precomputed.
    pub offset: Option<(f64, f64)>,
    pub button: i16,
    pub buttons: u16,
    /// The `relatedTarget` as the node's **wrapper**, not a bare id — the same
    /// choice `SubmitEvent.submitter` makes, and for the same reason: a wrapper
    /// pins its node, so an event parked in a listener's closure can never be
    /// left naming a freed arena slot. A bare id could, and the shadow-DOM
    /// retargeting walk in [`dispatch_event`] reads it through
    /// `DomTree::containing_shadow_root`, which **panics** on a stale id.
    /// The id is recovered, generation-checked, at every read.
    pub related: Option<JsValue>,
    pub wheel: Option<WheelFields>,
    pub pointer: Option<PointerFields>,
}

#[derive(Clone, Copy, Default)]
pub struct WheelFields {
    pub delta_x: f64,
    pub delta_y: f64,
    pub delta_z: f64,
    pub delta_mode: u32,
}

#[derive(Clone)]
pub struct PointerFields {
    pub pointer_id: i32,
    pub width: f64,
    pub height: f64,
    pub pressure: f64,
    pub pointer_type: String,
    pub is_primary: bool,
}

#[derive(Clone, Default)]
pub struct KeyboardFields {
    pub key: String,
    pub code: String,
    pub location: u32,
    pub repeat: bool,
    pub is_composing: bool,
    pub char_code: u32,
    pub key_code: u32,
}

#[derive(Clone, Default)]
pub struct InputFields {
    /// `null` for a deletion, a string for an insertion — hence `Option`.
    pub data: Option<String>,
    pub is_composing: bool,
    pub input_type: String,
}

/// Which subinterface a [`UiPayload`] belongs to. The variant *is* the brand:
/// a `MouseEvent` getter on a `KeyboardEvent` receiver fails here rather than
/// on the wrapper's prototype, which is what makes every interface's members
/// reject a foreign receiver without a per-interface slab tag.
#[derive(Clone)]
pub enum UiKind {
    /// A plain `UIEvent`.
    Plain,
    Mouse(Box<MouseFields>),
    Keyboard(Box<KeyboardFields>),
    /// `FocusEvent`, whose only extra member is `relatedTarget` — held as a
    /// wrapper for the reason [`MouseFields::related`] documents.
    Focus {
        related: Option<JsValue>,
    },
    Input(Box<InputFields>),
    Composition {
        data: String,
    },
    /// `ProgressEvent`, which is **not** a UI event — it inherits straight from
    /// `Event`. It reuses this slot anyway, because the slot is what gives an
    /// event interface its brand: a `ProgressEvent` getter called on a plain
    /// `Event` fails on the payload shape rather than needing a tag of its own
    /// (ADR-0024). The other [`UiPayload`] fields (`detail`, `has_view`,
    /// `modifiers`) stay at their defaults and are never read for it.
    ///
    /// `loaded`/`total` are `unsigned long long` in the IDL and JS numbers are
    /// doubles, so an `f64` is the exact script-visible type.
    Progress {
        length_computable: bool,
        loaded: f64,
        total: f64,
    },
}

/// The typed payload of a UI event.
#[derive(Clone)]
pub struct UiPayload {
    /// `UIEvent.detail` — a *different* member from [`EventData::detail`],
    /// which is `CustomEvent`'s `any`. Same name, different interfaces.
    pub detail: i32,
    /// Whether `view` is the Window (it is that or null; there is one Window).
    pub has_view: bool,
    pub modifiers: Modifiers,
    pub kind: UiKind,
}

impl UiPayload {
    #[must_use]
    pub fn new(kind: UiKind) -> Self {
        Self {
            detail: 0,
            has_view: false,
            modifiers: Modifiers::default(),
            kind,
        }
    }
}

/// State behind an `Event` wrapper.
pub struct EventData {
    pub event_type: String,
    pub bubbles: bool,
    pub cancelable: bool,
    pub composed: bool,
    pub target: Option<EventTargetKey>,
    pub current_target: Option<EventTargetKey>,
    pub phase: u16,
    pub stop_propagation: bool,
    pub stop_immediate_propagation: bool,
    pub canceled: bool,
    pub initialized: bool,
    pub dispatching: bool,
    pub is_trusted: bool,
    pub time_stamp: f64,
    /// The one extra value an event subinterface carries, read under a
    /// different name by each of them: `CustomEvent.detail`,
    /// `PopStateEvent.state`, and `SubmitEvent.submitter` (as the submitter's
    /// wrapper — the node id is recovered from it). Three interfaces, one
    /// slot, because no event is more than one of them.
    pub detail: JsValue,
    /// The typed payload of the UI event family, boxed and optional so that
    /// every non-UI event — `DOMContentLoaded`, `load`, every mutation-driven
    /// dispatch — pays one null pointer and no allocation for it.
    pub ui: Option<Box<UiPayload>>,
    /// The propagation path of the current/last dispatch (for `composedPath`).
    pub path: Vec<EventTargetKey>,
    /// Spec "in passive listener flag" (§2.8): set for the duration of
    /// invoking a listener whose "passive" flag is set. While set,
    /// `preventDefault()`/`returnValue = false` must not set the canceled
    /// flag — see `imp::event::set_canceled_flag`.
    pub in_passive_listener: bool,
}

impl EventData {
    pub fn new(event_type: String, bubbles: bool, cancelable: bool, composed: bool) -> Self {
        Self {
            event_type,
            bubbles,
            cancelable,
            composed,
            target: None,
            current_target: None,
            phase: PHASE_NONE,
            stop_propagation: false,
            stop_immediate_propagation: false,
            canceled: false,
            initialized: true,
            dispatching: false,
            is_trusted: false,
            time_stamp: 0.0,
            detail: JsValue::Null,
            ui: None,
            path: Vec::new(),
            in_passive_listener: false,
        }
    }

    /// Attaches a UI payload, for the `imp` constructors and the synthesis
    /// pipeline.
    #[must_use]
    pub fn with_ui(mut self, ui: UiPayload) -> Self {
        self.ui = Some(Box::new(ui));
        self
    }

    /// An uninitialized event, as `document.createEvent` returns.
    pub fn uninitialized() -> Self {
        let mut ev = Self::new(String::new(), false, false, false);
        ev.initialized = false;
        ev
    }
}

/// A registered JS event listener.
#[derive(Clone)]
pub(crate) struct JsListener {
    pub id: u64,
    pub event_type: String,
    pub callback: JsValue,
    pub capture: bool,
    pub once: bool,
    /// Spec "passive" flag: while a listener with this flag set is being
    /// invoked, the event's in-passive-listener flag is set (§2.8). Not part
    /// of listener identity — see `ListenerRegistry::matching`.
    pub passive: bool,
    /// The `AbortSignal` (slab key) whose abort removes this listener. Like
    /// `passive`/`once`, not part of listener identity.
    pub signal: Option<u64>,
}

/// Per-target listener lists with spec add/remove semantics.
#[derive(Default)]
pub(crate) struct ListenerRegistry {
    next_id: u64,
    map: HashMap<EventTargetKey, Vec<JsListener>>,
}

impl ListenerRegistry {
    /// The `(id, callback)` pairs registered for `target` matching `event_type`
    /// and `capture`. Callers compare callbacks with JS `===` *after* dropping
    /// the registry borrow, since `strict_equals` re-enters JS.
    pub fn matching(
        &self,
        target: EventTargetKey,
        event_type: &str,
        capture: bool,
    ) -> Vec<(u64, JsValue)> {
        self.map
            .get(&target)
            .map(|list| {
                list.iter()
                    .filter(|l| l.event_type == event_type && l.capture == capture)
                    .map(|l| (l.id, l.callback.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Appends a listener unconditionally (the caller has already ruled out an
    /// equivalent one via [`ListenerRegistry::matching`]).
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        target: EventTargetKey,
        event_type: String,
        callback: JsValue,
        capture: bool,
        once: bool,
        passive: bool,
        signal: Option<u64>,
    ) {
        self.next_id += 1;
        let id = self.next_id;
        self.map.entry(target).or_default().push(JsListener {
            id,
            event_type,
            callback,
            capture,
            once,
            passive,
            signal,
        });
    }

    /// Removes every listener added with `signal`, on every target — the
    /// listener records the signal, not the other way round, so an abort sweeps
    /// the registry. Called from `AbortSignal`'s abort steps.
    pub(crate) fn remove_by_signal(&mut self, signal: u64) {
        self.map.retain(|_, list| {
            list.retain(|l| l.signal != Some(signal));
            !list.is_empty()
        });
    }

    pub fn remove_by_id(&mut self, target: EventTargetKey, id: u64) {
        if let Some(list) = self.map.get_mut(&target) {
            list.retain(|l| l.id != id);
            if list.is_empty() {
                self.map.remove(&target);
            }
        }
    }

    /// Drops every registration for a target. Called when a host event target
    /// is finalized: the slab entry goes, and without this its listeners would
    /// stay in the map forever. Slab keys are never recycled, so this is a
    /// memory reclaim rather than a correctness fix — but an unbounded one.
    pub fn remove_target(&mut self, target: EventTargetKey) {
        self.map.remove(&target);
    }

    pub fn snapshot(&self, target: EventTargetKey, event_type: &str) -> Vec<JsListener> {
        self.map
            .get(&target)
            .map(|list| {
                list.iter()
                    .filter(|l| l.event_type == event_type)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn contains(&self, target: EventTargetKey, id: u64) -> bool {
        self.map
            .get(&target)
            .is_some_and(|list| list.iter().any(|l| l.id == id))
    }
}

/// Converts an event-target key to the JS value listeners observe
/// (`event.target`, listener `this`).
pub(crate) fn target_to_js(cx: &BindCx<'_>, key: EventTargetKey) -> Result<JsValue, JsThrow> {
    match key {
        EventTargetKey::Node(id) => cx.node_to_js(id),
        EventTargetKey::Window => {
            let js = cx.state.js.borrow();
            let refs = js
                .as_ref()
                .ok_or_else(|| JsThrow::Type("bootstrap not installed".into()))?;
            Ok(JsValue::Object(refs.global.clone()))
        }
        EventTargetKey::MediaQueryList(key) => {
            let data = {
                let slab = cx.state.slab.borrow();
                match slab.get(key) {
                    Some(crate::state::HostData::MediaQueryList(data)) => Rc::clone(data),
                    _ => return Err(JsThrow::Type("stale MediaQueryList target".into())),
                }
            };
            data.wrapper
                .borrow()
                .clone()
                .ok_or_else(|| JsThrow::Type("MediaQueryList wrapper is not installed".into()))
        }
        EventTargetKey::AbortSignal(key) => {
            let data = {
                let slab = cx.state.slab.borrow();
                match slab.get(key) {
                    Some(crate::state::HostData::AbortSignal(data)) => Rc::clone(data),
                    _ => return Err(JsThrow::Type("stale AbortSignal target".into())),
                }
            };
            data.wrapper
                .borrow()
                .clone()
                .ok_or_else(|| JsThrow::Type("AbortSignal wrapper is not installed".into()))
        }
        EventTargetKey::Host(key) => {
            // Both host event targets keep their own wrapper so `event.target`
            // hands back the very object the script holds.
            let wrapper = {
                let slab = cx.state.slab.borrow();
                match slab.get(key) {
                    Some(crate::state::HostData::EventTarget(data)) => {
                        data.wrapper.borrow().clone()
                    }
                    Some(crate::state::HostData::Xhr(xhr)) => xhr.borrow().wrapper.clone(),
                    // The upload object's wrapper lives on the XHR that owns it
                    // (it is that XHR's `[SameObject]` member).
                    Some(crate::state::HostData::XhrUpload(owner)) => {
                        owner.upgrade().and_then(|x| x.borrow().upload.clone())
                    }
                    Some(crate::state::HostData::FileReader(reader)) => {
                        reader.wrapper.borrow().clone()
                    }
                    _ => return Err(JsThrow::Type("stale EventTarget target".into())),
                }
            };
            wrapper.ok_or_else(|| JsThrow::Type("EventTarget wrapper is not installed".into()))
        }
    }
}

/// DOM's **retarget** algorithm: while `a` is a node whose root is a shadow
/// root and `b` is not a shadow-including inclusive descendant of that root,
/// replace `a` with the root's host.
///
/// This is what stops a `relatedTarget` inside a closed shadow tree from
/// leaking out of it: a listener in the light tree sees the *host*, never the
/// node the pointer actually came from.
///
/// Both ids must be **live**: the walk uses `DomTree::node`, which panics on a
/// stale one. [`ui_related_target`] is where `a` is checked and the dispatch
/// target is live by construction.
fn retarget(dom: &oxidepage_dom::DomTree, mut a: NodeId, b: Option<NodeId>) -> NodeId {
    // Bounded by the shadow nesting depth; the guard is against a malformed
    // tree rather than an expected case.
    for _ in 0..64 {
        let Some(root) = dom.containing_shadow_root(a) else {
            return a;
        };
        if let Some(b) = b
            && shadow_including_inclusive_descendant(dom, b, root)
        {
            return a;
        }
        let Some(host) = dom.shadow_host(root) else {
            return a;
        };
        a = host;
    }
    a
}

/// Whether `node` is `ancestor` or is contained by it, crossing shadow
/// boundaries upwards (a node in a shadow tree is a shadow-including descendant
/// of everything its host descends from).
fn shadow_including_inclusive_descendant(
    dom: &oxidepage_dom::DomTree,
    node: NodeId,
    ancestor: NodeId,
) -> bool {
    let mut current = Some(node);
    for _ in 0..1024 {
        let Some(id) = current else { return false };
        if id == ancestor {
            return true;
        }
        current = match dom.node(id).parent() {
            Some(parent) => Some(parent),
            // At a root: cross to the host if this is a shadow root.
            None => dom.shadow_host(id),
        };
    }
    false
}

/// Spec `dispatch`. The propagation path is *composed*: a `composed` event
/// crosses shadow root → host boundaries up to the document and window, a
/// non-composed event stops at its containing shadow root. v1 limitation
/// (ADR-0010): `event.target` is not retargeted per scope — listeners above
/// the boundary observe the real inner target. `event_value` is the JS
/// wrapper listeners receive. Returns `false` when canceled.
/// The flat-tree (composed-tree) parent of `node` for event dispatch, per the
/// DOM "get the parent" algorithm:
/// - a node assigned to a `<slot>` propagates into that slot (crossing into the
///   shadow tree it is slotted into);
/// - a shadow root propagates to its host, but a non-composed event stops at the
///   shadow root that is the root of the *original target*'s own tree;
/// - anything else uses its ordinary DOM parent (so unassigned light children
///   of a host still reach the host).
fn flat_event_parent(
    dom: &oxidepage_dom::DomTree,
    node: NodeId,
    original_target: NodeId,
    composed: bool,
) -> Option<NodeId> {
    if let Some(slot) = dom.assigned_slot(node) {
        return Some(slot);
    }
    match dom.node(node).parent() {
        Some(parent) => Some(parent),
        None => {
            // `node` is a shadow root fragment (or a detached tree root). Cross
            // to the host only for a composed event, or when this shadow root is
            // not the original target's own tree root (i.e. the target was
            // slotted in from outside).
            let host = dom.shadow_host(node)?;
            if composed || dom.containing_shadow_root(original_target) != Some(node) {
                Some(host)
            } else {
                None
            }
        }
    }
}

pub fn dispatch_event(
    cx: &BindCx<'_>,
    target: EventTargetKey,
    event_value: &JsValue,
    event: &Rc<RefCell<EventData>>,
) -> Result<bool, JsThrow> {
    {
        let ev = event.borrow();
        if ev.dispatching || !ev.initialized {
            return Err(cx.dom_throw(
                DomExceptionKind::InvalidStateError,
                "event is already being dispatched or not initialized",
            ));
        }
    }

    // DOM dispatch step 4: retarget the event's related target against the
    // target. A `relatedTarget` inside a shadow tree is reported as the host to
    // anything outside it — which is what stops a closed tree from leaking.
    //
    // The target need not be a node: dispatching on an `XMLHttpRequest` or the
    // Window still retargets, because a non-node can never be a
    // shadow-including descendant of a shadow root.
    let target_node = match target {
        EventTargetKey::Node(node) => Some(node),
        _ => None,
    };
    let related = {
        let ev = event.borrow();
        ev.ui.as_deref().and_then(|p| ui_related_target(cx, p))
    };
    if let Some(related) = related {
        let retargeted = {
            let dom = cx.state.dom.borrow();
            retarget(&dom, related, target_node)
        };
        if retargeted != related {
            let wrapper = cx.node_to_js(retargeted)?;
            set_ui_related_target(&mut event.borrow_mut(), Some(wrapper));
        }
    }

    // "Clear targets" is decided **now**, not after the dispatch: a listener is
    // free to move the target out of its shadow tree mid-dispatch, and the
    // decision must reflect where it was when the event started.
    let clear_targets = {
        let related = {
            let ev = event.borrow();
            ev.ui.as_deref().and_then(|p| ui_related_target(cx, p))
        };
        let dom = cx.state.dom.borrow();
        target_node.is_some_and(|t| dom.containing_shadow_root(t).is_some())
            || related.is_some_and(|r| dom.containing_shadow_root(r).is_some())
    };

    // Build the propagation path: target, ancestors, then the window when
    // the target lives in the document tree (HTML event loop integration).
    let mut path: Vec<EventTargetKey> = Vec::new();
    match target {
        EventTargetKey::Window
        | EventTargetKey::MediaQueryList(_)
        | EventTargetKey::AbortSignal(_)
        | EventTargetKey::Host(_) => path.push(target),
        EventTargetKey::Node(node) => {
            let dom = cx.state.dom.borrow();
            let composed = event.borrow().composed;
            let mut current = Some(node);
            while let Some(id) = current {
                path.push(EventTargetKey::Node(id));
                current = flat_event_parent(&dom, id, node, composed);
            }
            let reaches_document = composed || dom.containing_shadow_root(node).is_none();
            if dom.node(node).is_connected() && reaches_document {
                path.push(EventTargetKey::Window);
            }
        }
    }

    // DOM dispatch step 5: a `click` **MouseEvent** (which a `PointerEvent`
    // is) activates its target. This is the single activation trigger in the
    // engine — `HTMLElement.click()`, `dispatchEvent(new MouseEvent("click"))`
    // and a synthesized pointer click all reach activation through here, so
    // hyperlinks, submit buttons and `<label>` cannot behave differently
    // depending on which one drove them.
    //
    // A plain `Event` named "click" deliberately does not activate: the spec's
    // trigger is the interface, not the type, and that is what keeps
    // `dispatchEvent(new Event("click"))` inert.
    let activation = match target {
        EventTargetKey::Node(node) if is_activating_click(&event.borrow()) => {
            let bubbles = event.borrow().bubbles;
            Some(crate::imp::interaction::begin_activation(cx, node, bubbles))
        }
        _ => None,
    };

    {
        let mut ev = event.borrow_mut();
        ev.dispatching = true;
        ev.target = Some(target);
        ev.path = path.clone();
    }

    // Capture phase: root towards the target's parent.
    for &key in path[1..].iter().rev() {
        if event.borrow().stop_propagation {
            break;
        }
        invoke_listeners(cx, key, event_value, event, PHASE_CAPTURING)?;
    }
    // Target phase.
    if !event.borrow().stop_propagation {
        invoke_listeners(cx, path[0], event_value, event, PHASE_AT_TARGET)?;
    }
    // Bubble phase.
    if event.borrow().bubbles {
        for &key in &path[1..] {
            if event.borrow().stop_propagation {
                break;
            }
            invoke_listeners(cx, key, event_value, event, PHASE_BUBBLING)?;
        }
    }

    let canceled = {
        let mut ev = event.borrow_mut();
        ev.dispatching = false;
        ev.phase = PHASE_NONE;
        ev.current_target = None;
        ev.stop_propagation = false;
        ev.stop_immediate_propagation = false;
        // "Empty event's path" — the last step of dispatch. `composedPath()` is
        // only meaningful *during* a dispatch; afterwards it must report `[]`,
        // not the stale path of the dispatch that just ended.
        ev.path.clear();
        ev.canceled
    };

    // The last step of dispatch: if the target or the related target was rooted
    // in a shadow tree, null both out. A script that retained the event object
    // must not be able to read a node out of a closed tree afterwards. This
    // runs **before** the activation behavior, which the spec orders the same
    // way — an activation handler must not see them either.
    if clear_targets {
        let mut ev = event.borrow_mut();
        ev.target = None;
        set_ui_related_target(&mut ev, None);
    }

    if let Some(state) = activation {
        crate::imp::interaction::finish_activation(cx, state, !canceled)?;
    }
    Ok(!canceled)
}

/// The `relatedTarget` of a UI payload, whichever interface carries it.
///
/// This is the read boundary the stored wrapper's id is generation-checked at:
/// `this_node` refuses an id whose node is gone (a wrapper minted before a
/// navigation replaced the arena), so the callers below never hand a stale id
/// to the panicking `DomTree::node` family.
pub(crate) fn ui_related_target(cx: &BindCx<'_>, payload: &UiPayload) -> Option<NodeId> {
    let value = related_target_value(payload)?;
    cx.this_node(value).ok()
}

/// The stored `relatedTarget` wrapper, unvalidated — for the getters, which
/// hand back the very object script passed in so `e.relatedTarget === node`.
pub(crate) fn related_target_value(payload: &UiPayload) -> Option<&JsValue> {
    match &payload.kind {
        UiKind::Mouse(m) => m.related.as_ref(),
        UiKind::Focus { related } => related.as_ref(),
        _ => None,
    }
}

fn set_ui_related_target(event: &mut EventData, related: Option<JsValue>) {
    if let Some(payload) = event.ui.as_deref_mut() {
        match &mut payload.kind {
            UiKind::Mouse(m) => m.related = related,
            UiKind::Focus { related: slot } => *slot = related,
            _ => {}
        }
    }
}

/// Whether this event is the one that triggers activation behavior: type
/// `click`, carrying a mouse payload.
fn is_activating_click(ev: &EventData) -> bool {
    ev.event_type == "click" && matches!(ev.ui.as_deref().map(|p| &p.kind), Some(UiKind::Mouse(_)))
}

/// RAII guard for the spec "in passive listener flag" (§2.8). Sets
/// `EventData::in_passive_listener` on construction and restores the prior
/// value on drop, so the flag is cleared correctly even if a listener
/// invocation is left via an early return or a panic unwind.
struct PassiveListenerGuard<'a> {
    event: &'a Rc<RefCell<EventData>>,
    previous: bool,
}

impl<'a> PassiveListenerGuard<'a> {
    fn new(event: &'a Rc<RefCell<EventData>>, passive: bool) -> Self {
        let previous = event.borrow().in_passive_listener;
        event.borrow_mut().in_passive_listener = passive;
        Self { event, previous }
    }
}

impl Drop for PassiveListenerGuard<'_> {
    fn drop(&mut self) {
        self.event.borrow_mut().in_passive_listener = self.previous;
    }
}

fn invoke_listeners(
    cx: &BindCx<'_>,
    key: EventTargetKey,
    event_value: &JsValue,
    event: &Rc<RefCell<EventData>>,
    phase: u16,
) -> Result<(), JsThrow> {
    // Spec "inner invoke": snapshot; listeners added during dispatch on this
    // target do not run for this event.
    let snapshot = cx
        .state
        .listeners
        .borrow()
        .snapshot(key, &event.borrow().event_type);
    // An event handler IDL attribute *is* an event listener — HTML registers it
    // (non-capturing) when the handler is first set. So it participates at the
    // target and while bubbling, and never while capturing. Restricting it to
    // the target phase silently broke every `onclick` used for delegation on a
    // container, which is one of the most common shapes on the real web.
    let handler = if phase != PHASE_CAPTURING && !event.borrow().stop_immediate_propagation {
        let event_type = event.borrow().event_type.clone();
        // Resolves the handler assigned through the IDL attribute *or* declared
        // as a content attribute (`<body onload="…">`), compiling the latter on
        // first use (`crate::handlers`).
        crate::handlers::resolve(cx, key, &event_type)
    } else {
        None
    };
    if snapshot.is_empty() && handler.is_none() {
        return Ok(());
    }
    let this = target_to_js(cx, key)?;
    for listener in snapshot {
        match phase {
            PHASE_CAPTURING if !listener.capture => continue,
            PHASE_BUBBLING if listener.capture => continue,
            _ => {}
        }
        // A listener removed by an earlier listener must not run.
        if !cx.state.listeners.borrow().contains(key, listener.id) {
            continue;
        }
        if listener.once {
            cx.state
                .listeners
                .borrow_mut()
                .remove_by_id(key, listener.id);
        }
        {
            let mut ev = event.borrow_mut();
            ev.current_target = Some(key);
            ev.phase = phase;
        }

        // Spec §2.8: the in-passive-listener flag is set for the duration of
        // this call only. The guard restores the prior value on every exit
        // path, including a Rust panic unwinding through the callback.
        let _passive_guard = PassiveListenerGuard::new(event, listener.passive);

        // Callback: a function, or an object with `handleEvent`.
        let result = if cx.scope.is_function(&listener.callback) {
            cx.scope
                .call(&listener.callback, &this, std::slice::from_ref(event_value))
        } else if let JsValue::Object(obj) = &listener.callback {
            match cx.scope.get(obj, "handleEvent") {
                Ok(handler) if cx.scope.is_function(&handler) => cx.scope.call(
                    &handler,
                    &listener.callback,
                    std::slice::from_ref(event_value),
                ),
                Ok(_) => Err(oxidepage_js::JsError::Engine(
                    "handleEvent is not a function".into(),
                )),
                Err(e) => Err(e),
            }
        } else {
            Ok(JsValue::Undefined)
        };
        drop(_passive_guard);
        if let Err(error) = result {
            // Spec: report the exception; dispatch continues.
            cx.report_callback_error(&error);
        }

        // No microtask checkpoint here: the JS execution stack is not empty
        // between listeners of a script-initiated `dispatchEvent`, so draining
        // microtasks now would reorder them ahead of the remaining listeners.
        // The checkpoint runs when the stack empties — the Rust-driven entry
        // points (`fire_simple_event`) run it after the whole dispatch, and a
        // JS-driven `dispatchEvent` rides its caller's task checkpoint.
        if event.borrow().stop_immediate_propagation {
            break;
        }
    }

    // Event-handler IDL attributes participate at the target and in the bubble
    // phase. This practical layer stores them separately from addEventListener
    // registrations while preserving the same receiver and exception-reporting
    // behavior. Deliberate deviation: HTML puts the handler at the position in
    // the listener list where it was *first assigned*, so an `onclick` set
    // before an `addEventListener("click")` should run first; here it always
    // runs last on its target. Ordering between the two on one element is not
    // something real code depends on, and matching it would mean registering
    // handlers as real listeners.
    if !event.borrow().stop_immediate_propagation
        && let Some(handler) = handler
        && cx.scope.is_function(&handler)
    {
        match cx
            .scope
            .call(&handler, &this, std::slice::from_ref(event_value))
        {
            Err(error) => cx.report_callback_error(&error),
            // HTML's **event handler processing algorithm**, step 5: a handler
            // that returns `false` cancels the event. This is the `onsubmit=
            // "…; return false"` / `onclick="return false"` idiom, and it is
            // only reachable through the IDL-attribute path — a listener added
            // with `addEventListener` has no return value by design, which is
            // why the branch above discards its result.
            //
            // Only `false` exactly: `undefined` (the common case) must not
            // cancel. `onerror` on the Window inverts the test, and
            // `beforeunload` uses the value differently again — neither exists
            // here, so neither is special-cased.
            Ok(JsValue::Bool(false)) => crate::imp::event::set_canceled_flag(cx, event),
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Creates and dispatches a trusted `popstate` at the window, carrying the
/// state of the session-history entry just traversed to.
///
/// Not `fire_simple_event`: `popstate` is a `PopStateEvent`, and its `state`
/// is the only way script learns *which* entry it landed on.
pub fn fire_pop_state(cx: &BindCx<'_>, state: JsValue) -> Result<(), JsThrow> {
    let mut data = EventData::new("popstate".to_owned(), false, false, false);
    data.is_trusted = true;
    data.detail = state;
    let (value, data) = cx.new_event_object("PopStateEvent", data)?;
    dispatch_event(cx, EventTargetKey::Window, &value, &data)?;
    crate::microtask_checkpoint(cx);
    Ok(())
}

/// Creates and dispatches a simple engine-generated event (`DOMContentLoaded`,
/// `load`) with `isTrusted = true`.
pub fn fire_simple_event(
    cx: &BindCx<'_>,
    target: EventTargetKey,
    event_type: &str,
    bubbles: bool,
) -> Result<(), JsThrow> {
    let mut data = EventData::new(event_type.to_owned(), bubbles, false, false);
    data.is_trusted = true;
    let (value, data) = cx.new_event_object("Event", data)?;
    dispatch_event(cx, target, &value, &data)?;
    // This is a Rust-driven dispatch at a task boundary (the JS stack is now
    // empty), so run the microtask checkpoint that `invoke_listeners` no
    // longer performs per-listener.
    crate::microtask_checkpoint(cx);
    Ok(())
}
