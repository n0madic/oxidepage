//! Event dispatch skeleton (DOM spec §2): capture → target → bubble, with
//! `stopPropagation` / `stopImmediatePropagation` / `preventDefault`.
//!
//! Listeners are engine-agnostic Rust callbacks in a side table; in Phase 2
//! they hold JS function handles and dispatch calls into the realm
//! (design doc §5.2 "Events are native"). No shadow trees, no `Window`
//! target, and no activation behavior yet.

use std::collections::HashMap;
use std::rc::Rc;

use html5ever::LocalName;
use oxidepage_base::{DomException, DomExceptionKind, NodeId};

use crate::tree::DomTree;

/// A listener callback. Receives the tree (mutations during dispatch are
/// legal) and the event being dispatched.
pub type ListenerCallback = Rc<dyn Fn(&mut DomTree, &mut Event)>;

/// Identifies a registered listener for targeted removal.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ListenerId(u64);

/// `addEventListener` options subset meaningful without an event loop.
#[derive(Clone, Copy, Debug, Default)]
pub struct AddEventListenerOptions {
    pub capture: bool,
    pub once: bool,
}

#[derive(Clone)]
struct ListenerEntry {
    id: ListenerId,
    event_type: LocalName,
    callback: ListenerCallback,
    capture: bool,
    once: bool,
}

/// Per-node listener lists (sparse side table; design doc §5.2).
#[derive(Default)]
pub(crate) struct ListenerRegistry {
    next_id: u64,
    map: HashMap<NodeId, Vec<ListenerEntry>>,
}

impl ListenerRegistry {
    fn add(
        &mut self,
        node: NodeId,
        event_type: LocalName,
        callback: ListenerCallback,
        options: AddEventListenerOptions,
    ) -> ListenerId {
        let list = self.map.entry(node).or_default();
        // Spec: ignore an add whose (type, callback, capture) already exists.
        if let Some(existing) = list.iter().find(|e| {
            e.event_type == event_type
                && Rc::ptr_eq(&e.callback, &callback)
                && e.capture == options.capture
        }) {
            return existing.id;
        }
        self.next_id += 1;
        let id = ListenerId(self.next_id);
        list.push(ListenerEntry {
            id,
            event_type,
            callback,
            capture: options.capture,
            once: options.once,
        });
        id
    }

    fn remove_by_id(&mut self, node: NodeId, id: ListenerId) -> bool {
        if let Some(list) = self.map.get_mut(&node) {
            let before = list.len();
            list.retain(|e| e.id != id);
            // Report removal only if an entry with this exact id was dropped,
            // not merely that some listener existed on the node (L5).
            let removed = list.len() != before;
            if list.is_empty() {
                self.map.remove(&node);
            }
            return removed;
        }
        false
    }

    fn contains(&self, node: NodeId, id: ListenerId) -> bool {
        self.map
            .get(&node)
            .is_some_and(|l| l.iter().any(|e| e.id == id))
    }

    fn snapshot(&self, node: NodeId, event_type: &LocalName) -> Vec<ListenerEntry> {
        self.map
            .get(&node)
            .map(|l| {
                l.iter()
                    .filter(|e| e.event_type == *event_type)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn remove_node(&mut self, node: NodeId) {
        self.map.remove(&node);
    }
}

/// Dispatch phase, as observed by `Event.eventPhase`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventPhase {
    None,
    Capturing,
    AtTarget,
    Bubbling,
}

/// A dispatchable event (spec `Event`, skeleton subset).
pub struct Event {
    event_type: LocalName,
    bubbles: bool,
    cancelable: bool,
    target: Option<NodeId>,
    current_target: Option<NodeId>,
    phase: EventPhase,
    stop_propagation: bool,
    stop_immediate_propagation: bool,
    canceled: bool,
    dispatching: bool,
}

impl Event {
    #[must_use]
    pub fn new(event_type: LocalName, bubbles: bool, cancelable: bool) -> Self {
        Self {
            event_type,
            bubbles,
            cancelable,
            target: None,
            current_target: None,
            phase: EventPhase::None,
            stop_propagation: false,
            stop_immediate_propagation: false,
            canceled: false,
            dispatching: false,
        }
    }

    #[must_use]
    pub fn event_type(&self) -> &LocalName {
        &self.event_type
    }

    #[must_use]
    pub fn bubbles(&self) -> bool {
        self.bubbles
    }

    #[must_use]
    pub fn cancelable(&self) -> bool {
        self.cancelable
    }

    #[must_use]
    pub fn target(&self) -> Option<NodeId> {
        self.target
    }

    #[must_use]
    pub fn current_target(&self) -> Option<NodeId> {
        self.current_target
    }

    #[must_use]
    pub fn phase(&self) -> EventPhase {
        self.phase
    }

    pub fn stop_propagation(&mut self) {
        self.stop_propagation = true;
    }

    pub fn stop_immediate_propagation(&mut self) {
        self.stop_propagation = true;
        self.stop_immediate_propagation = true;
    }

    /// Spec `preventDefault()`: only cancelable events can be canceled.
    pub fn prevent_default(&mut self) {
        if self.cancelable {
            self.canceled = true;
        }
    }

    #[must_use]
    pub fn default_prevented(&self) -> bool {
        self.canceled
    }
}

impl DomTree {
    /// Spec `addEventListener`. Returns the id for targeted removal; adding
    /// a duplicate (same type, callback, capture) returns the existing id.
    pub fn add_event_listener(
        &mut self,
        node: NodeId,
        event_type: LocalName,
        callback: ListenerCallback,
        options: AddEventListenerOptions,
    ) -> ListenerId {
        self.listeners.add(node, event_type, callback, options)
    }

    /// Removes a listener previously returned by [`Self::add_event_listener`].
    pub fn remove_event_listener(&mut self, node: NodeId, id: ListenerId) -> bool {
        self.listeners.remove_by_id(node, id)
    }

    /// Spec `dispatch` (flattened: no shadow trees, no window target).
    ///
    /// Returns `Ok(false)` if the event was canceled, `Ok(true)` otherwise;
    /// `InvalidStateError` if the event is already being dispatched.
    pub fn dispatch_event(
        &mut self,
        target: NodeId,
        event: &mut Event,
    ) -> Result<bool, DomException> {
        if event.dispatching {
            return Err(DomException::new(
                DomExceptionKind::InvalidStateError,
                "event is already being dispatched",
            ));
        }
        event.dispatching = true;
        event.target = Some(target);

        // Event path: target first, then ancestors toward the root.
        let path: Vec<NodeId> = self.inclusive_ancestors(target).collect();

        // Capture phase: root → target's parent.
        event.phase = EventPhase::Capturing;
        for &node in path[1..].iter().rev() {
            if event.stop_propagation {
                break;
            }
            self.invoke_listeners(node, event, EventPhase::Capturing);
        }

        // Target phase: capture and bubble listeners in registration order.
        if !event.stop_propagation {
            event.phase = EventPhase::AtTarget;
            self.invoke_listeners(target, event, EventPhase::AtTarget);
        }

        // Bubble phase: target's parent → root, bubbling events only.
        if event.bubbles {
            event.phase = EventPhase::Bubbling;
            for &node in &path[1..] {
                if event.stop_propagation {
                    break;
                }
                self.invoke_listeners(node, event, EventPhase::Bubbling);
            }
        }

        event.phase = EventPhase::None;
        event.current_target = None;
        event.dispatching = false;
        event.stop_propagation = false;
        event.stop_immediate_propagation = false;
        Ok(!event.canceled)
    }

    fn invoke_listeners(&mut self, node: NodeId, event: &mut Event, phase: EventPhase) {
        // Spec "inner invoke": iterate over a snapshot; listeners added
        // during dispatch on this node do not run for this event.
        let snapshot = self.listeners.snapshot(node, &event.event_type);
        if snapshot.is_empty() {
            return;
        }
        event.current_target = Some(node);
        for entry in snapshot {
            match phase {
                EventPhase::Capturing if !entry.capture => continue,
                EventPhase::Bubbling if entry.capture => continue,
                _ => {}
            }
            // A listener removed by an earlier listener must not run.
            if !self.listeners.contains(node, entry.id) {
                continue;
            }
            if entry.once {
                self.listeners.remove_by_id(node, entry.id);
            }
            (entry.callback)(self, event);
            if event.stop_immediate_propagation {
                break;
            }
        }
    }
}
