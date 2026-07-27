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
            path: Vec::new(),
            in_passive_listener: false,
        }
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
            let data = {
                let slab = cx.state.slab.borrow();
                match slab.get(key) {
                    Some(crate::state::HostData::EventTarget(data)) => Rc::clone(data),
                    _ => return Err(JsThrow::Type("stale EventTarget target".into())),
                }
            };
            data.wrapper
                .borrow()
                .clone()
                .ok_or_else(|| JsThrow::Type("EventTarget wrapper is not installed".into()))
        }
    }
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
    Ok(!canceled)
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
    let handler = if phase == PHASE_AT_TARGET && !event.borrow().stop_immediate_propagation {
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
            cx.report_callback_error(error);
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

    // Event-handler IDL attributes participate at the target. This practical
    // layer stores them separately from addEventListener registrations while
    // preserving the same receiver and exception-reporting behavior.
    if !event.borrow().stop_immediate_propagation
        && let Some(handler) = handler
        && cx.scope.is_function(&handler)
    {
        match cx
            .scope
            .call(&handler, &this, std::slice::from_ref(event_value))
        {
            Err(error) => cx.report_callback_error(error),
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
