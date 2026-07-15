//! MutationObserver record queuing (DOM spec §4.3).
//!
//! Phase 1 implements registration, the "queue a mutation record" algorithm,
//! transient registered observers, and `takeRecords`. Delivery via microtask
//! checkpoints attaches in Phase 2 with the event loop.

use std::collections::HashMap;

use html5ever::tendril::StrTendril;
use html5ever::{LocalName, QualName};
use oxidepage_base::NodeId;

/// Identifies a registered `MutationObserver`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MutationObserverId(u64);

/// Normalized observation options (after the spec's `observe()` defaulting).
#[derive(Clone, Debug, Default)]
pub struct ObserveOptions {
    pub child_list: bool,
    pub attributes: bool,
    pub character_data: bool,
    pub subtree: bool,
    pub attribute_old_value: bool,
    pub character_data_old_value: bool,
    /// `None` means "no filter" (observe all attributes).
    pub attribute_filter: Option<Vec<LocalName>>,
}

/// `observe()` init dictionary before normalization, mirroring the IDL
/// optionality that the spec's validation steps depend on.
#[derive(Clone, Debug, Default)]
pub struct ObserveInit {
    pub child_list: bool,
    pub attributes: Option<bool>,
    pub character_data: Option<bool>,
    pub subtree: bool,
    pub attribute_old_value: Option<bool>,
    pub character_data_old_value: Option<bool>,
    pub attribute_filter: Option<Vec<LocalName>>,
}

/// Validation failure for `observe()`; surfaced to script as a `TypeError`
/// by the bindings (Phase 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("invalid MutationObserver options: {0}")]
pub struct InvalidObserveInit(pub &'static str);

impl ObserveInit {
    /// Spec `observe()` steps 1–5: defaulting and consistency checks.
    pub fn normalize(self) -> Result<ObserveOptions, InvalidObserveInit> {
        let attributes = self
            .attributes
            .unwrap_or(self.attribute_old_value.is_some() || self.attribute_filter.is_some());
        let character_data = self
            .character_data
            .unwrap_or(self.character_data_old_value.is_some());
        if !(self.child_list || attributes || character_data) {
            return Err(InvalidObserveInit(
                "one of childList, attributes, or characterData must be true",
            ));
        }
        if self.attribute_old_value == Some(true) && !attributes {
            return Err(InvalidObserveInit(
                "attributeOldValue requires attributes: true",
            ));
        }
        if self.attribute_filter.is_some() && !attributes {
            return Err(InvalidObserveInit(
                "attributeFilter requires attributes: true",
            ));
        }
        if self.character_data_old_value == Some(true) && !character_data {
            return Err(InvalidObserveInit(
                "characterDataOldValue requires characterData: true",
            ));
        }
        Ok(ObserveOptions {
            child_list: self.child_list,
            attributes,
            character_data,
            subtree: self.subtree,
            attribute_old_value: self.attribute_old_value.unwrap_or(false),
            character_data_old_value: self.character_data_old_value.unwrap_or(false),
            attribute_filter: self.attribute_filter,
        })
    }
}

/// The type of a mutation, together with its type-specific payload.
#[derive(Clone, Debug)]
pub enum RecordKind {
    ChildList,
    Attributes {
        name: QualName,
        old_value: Option<StrTendril>,
    },
    CharacterData {
        old_value: StrTendril,
    },
}

/// Node-list payload of a `childList` record.
#[derive(Clone, Debug, Default)]
pub struct RecordContents {
    pub added_nodes: Vec<NodeId>,
    pub removed_nodes: Vec<NodeId>,
    pub previous_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
}

/// A queued mutation record (spec `MutationRecord`).
///
/// The `target`, `added_nodes`, `removed_nodes`, and sibling [`NodeId`]s are
/// snapshots taken when the record was queued; the referenced nodes (in
/// particular `removed_nodes`) may have been freed and their arena slots
/// recycled before the record is delivered. Generation-tagged ids make a
/// recycled slot resolve to `None`, so consumers reading these ids back must
/// revalidate liveness (e.g. via `DomTree::get`) at the delivery boundary (L3).
#[derive(Clone, Debug)]
pub struct MutationRecord {
    pub record_type: MutationRecordType,
    pub target: NodeId,
    pub added_nodes: Vec<NodeId>,
    pub removed_nodes: Vec<NodeId>,
    pub previous_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub attribute_name: Option<LocalName>,
    pub attribute_namespace: Option<html5ever::Namespace>,
    pub old_value: Option<StrTendril>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MutationRecordType {
    ChildList,
    Attributes,
    CharacterData,
}

struct RegisteredObserver {
    observer: MutationObserverId,
    options: ObserveOptions,
    /// `Some(source_node)` for transient registered observers: the node the
    /// original (source) registration lives on.
    transient_source: Option<NodeId>,
}

#[derive(Default)]
struct ObserverState {
    queue: Vec<MutationRecord>,
    /// Nodes this observer is registered on (incl. transients), for cleanup.
    nodes: Vec<NodeId>,
}

/// Registry of observers and their per-node registrations.
#[derive(Default)]
pub struct MutationObserverRegistry {
    next_id: u64,
    observers: HashMap<MutationObserverId, ObserverState>,
    registered: HashMap<NodeId, Vec<RegisteredObserver>>,
}

impl MutationObserverRegistry {
    /// Creates a new observer (spec: `new MutationObserver(callback)`; the
    /// callback attaches in Phase 2 with microtask delivery).
    pub fn create_observer(&mut self) -> MutationObserverId {
        self.next_id += 1;
        let id = MutationObserverId(self.next_id);
        self.observers.insert(id, ObserverState::default());
        id
    }

    /// Spec `observe(target, options)` steps 6–8: (re-)registration.
    pub fn observe(
        &mut self,
        observer: MutationObserverId,
        target: NodeId,
        init: ObserveInit,
    ) -> Result<(), InvalidObserveInit> {
        let options = init.normalize()?;
        let list = self.registered.entry(target).or_default();
        let existing = list
            .iter_mut()
            .find(|r| r.observer == observer && r.transient_source.is_none());
        match existing {
            Some(registered) => {
                registered.options = options;
                // Spec: remove transient observers whose source is this
                // registration.
                self.remove_transients_with_source(observer, target);
            }
            None => {
                list.push(RegisteredObserver {
                    observer,
                    options,
                    transient_source: None,
                });
                if let Some(state) = self.observers.get_mut(&observer)
                    && !state.nodes.contains(&target)
                {
                    state.nodes.push(target);
                }
            }
        }
        Ok(())
    }

    /// Spec `disconnect()`: drops all registrations and queued records.
    pub fn disconnect(&mut self, observer: MutationObserverId) {
        if let Some(state) = self.observers.get_mut(&observer) {
            let nodes = std::mem::take(&mut state.nodes);
            state.queue.clear();
            for node in nodes {
                if let Some(list) = self.registered.get_mut(&node) {
                    list.retain(|r| r.observer != observer);
                    if list.is_empty() {
                        self.registered.remove(&node);
                    }
                }
            }
        }
    }

    /// Spec `takeRecords()`: drains this observer's record queue only.
    ///
    /// Per spec, transient registered observers are cleared exclusively in the
    /// microtask "notify mutation observers" step (see
    /// [`take_records_for_notify`](Self::take_records_for_notify)), *not* here:
    /// a script calling `takeRecords()` mid-task must keep observing the
    /// removed subtree for the rest of that task.
    pub fn take_records(&mut self, observer: MutationObserverId) -> Vec<MutationRecord> {
        match self.observers.get_mut(&observer) {
            Some(state) => std::mem::take(&mut state.queue),
            None => Vec::new(),
        }
    }

    /// The "notify mutation observers" delivery: drains the record queue and
    /// clears this observer's transient registrations, as the spec's notify
    /// step does. Only the microtask notify path may call this; the script
    /// `takeRecords()` binding uses [`take_records`](Self::take_records).
    pub fn take_records_for_notify(&mut self, observer: MutationObserverId) -> Vec<MutationRecord> {
        self.clear_transients(observer);
        self.take_records(observer)
    }

    /// True if any observer has queued records (Phase 2's rendering step
    /// polls this).
    #[must_use]
    pub fn has_pending_records(&self) -> bool {
        self.observers.values().any(|s| !s.queue.is_empty())
    }

    /// Spec `remove` step: for each inclusive ancestor of the removed node's
    /// parent with a `subtree` registration, register a transient observer
    /// on the removed node itself.
    pub(crate) fn register_transients(
        &mut self,
        parent_inclusive_ancestors: &[NodeId],
        removed: NodeId,
    ) {
        let mut transients: Vec<(MutationObserverId, ObserveOptions, NodeId)> = Vec::new();
        for &ancestor in parent_inclusive_ancestors {
            if let Some(list) = self.registered.get(&ancestor) {
                for r in list {
                    if r.options.subtree && r.transient_source.is_none() {
                        transients.push((r.observer, r.options.clone(), ancestor));
                    }
                }
            }
        }
        for (observer, options, source) in transients {
            self.registered
                .entry(removed)
                .or_default()
                .push(RegisteredObserver {
                    observer,
                    options,
                    transient_source: Some(source),
                });
            if let Some(state) = self.observers.get_mut(&observer)
                && !state.nodes.contains(&removed)
            {
                state.nodes.push(removed);
            }
        }
    }

    fn clear_transients(&mut self, observer: MutationObserverId) {
        let Some(state) = self.observers.get_mut(&observer) else {
            return;
        };
        let mut still_registered = Vec::new();
        for node in std::mem::take(&mut state.nodes) {
            if let Some(list) = self.registered.get_mut(&node) {
                list.retain(|r| r.observer != observer || r.transient_source.is_none());
                if list.iter().any(|r| r.observer == observer) {
                    still_registered.push(node);
                }
                if list.is_empty() {
                    self.registered.remove(&node);
                }
            }
        }
        state.nodes = still_registered;
    }

    fn remove_transients_with_source(&mut self, observer: MutationObserverId, source: NodeId) {
        let nodes: Vec<NodeId> = self
            .observers
            .get(&observer)
            .map(|s| s.nodes.clone())
            .unwrap_or_default();
        for node in nodes {
            if let Some(list) = self.registered.get_mut(&node) {
                list.retain(|r| !(r.observer == observer && r.transient_source == Some(source)));
                if list.is_empty() {
                    self.registered.remove(&node);
                }
            }
        }
    }

    /// The "queue a mutation record" algorithm. `inclusive_ancestors` is the
    /// target-first ancestor chain (the caller owns tree access).
    pub(crate) fn queue_record(
        &mut self,
        inclusive_ancestors: &[NodeId],
        target: NodeId,
        kind: RecordKind,
        contents: RecordContents,
    ) {
        if self.registered.is_empty() {
            return;
        }
        // interested observers, in discovery order, with their old-value flag
        let mut interested: Vec<(MutationObserverId, bool)> = Vec::new();
        for &node in inclusive_ancestors {
            let Some(list) = self.registered.get(&node) else {
                continue;
            };
            for r in list {
                let o = &r.options;
                if node != target && !o.subtree {
                    continue;
                }
                let wants_old = match &kind {
                    RecordKind::ChildList => {
                        if !o.child_list {
                            continue;
                        }
                        false
                    }
                    RecordKind::Attributes { name, .. } => {
                        if !o.attributes {
                            continue;
                        }
                        if let Some(filter) = &o.attribute_filter {
                            // Spec: filtered out when the attribute has a
                            // namespace or its local name is not listed.
                            if name.ns != html5ever::ns!() || !filter.contains(&name.local) {
                                continue;
                            }
                        }
                        o.attribute_old_value
                    }
                    RecordKind::CharacterData { .. } => {
                        if !o.character_data {
                            continue;
                        }
                        o.character_data_old_value
                    }
                };
                match interested.iter_mut().find(|(id, _)| *id == r.observer) {
                    Some((_, old)) => *old |= wants_old,
                    None => interested.push((r.observer, wants_old)),
                }
            }
        }
        for (observer, wants_old) in interested {
            let record = match &kind {
                RecordKind::ChildList => MutationRecord {
                    record_type: MutationRecordType::ChildList,
                    target,
                    added_nodes: contents.added_nodes.clone(),
                    removed_nodes: contents.removed_nodes.clone(),
                    previous_sibling: contents.previous_sibling,
                    next_sibling: contents.next_sibling,
                    attribute_name: None,
                    attribute_namespace: None,
                    old_value: None,
                },
                RecordKind::Attributes { name, old_value } => MutationRecord {
                    record_type: MutationRecordType::Attributes,
                    target,
                    added_nodes: Vec::new(),
                    removed_nodes: Vec::new(),
                    previous_sibling: None,
                    next_sibling: None,
                    attribute_name: Some(name.local.clone()),
                    attribute_namespace: (name.ns != html5ever::ns!()).then(|| name.ns.clone()),
                    old_value: wants_old.then(|| old_value.clone()).flatten(),
                },
                RecordKind::CharacterData { old_value } => MutationRecord {
                    record_type: MutationRecordType::CharacterData,
                    target,
                    added_nodes: Vec::new(),
                    removed_nodes: Vec::new(),
                    previous_sibling: None,
                    next_sibling: None,
                    attribute_name: None,
                    attribute_namespace: None,
                    old_value: wants_old.then(|| old_value.clone()),
                },
            };
            if let Some(state) = self.observers.get_mut(&observer) {
                state.queue.push(record);
            }
        }
    }

    /// Drops all registrations touching `node` (subtree freeing).
    pub(crate) fn remove_node(&mut self, node: NodeId) {
        if let Some(list) = self.registered.remove(&node) {
            for r in list {
                if let Some(state) = self.observers.get_mut(&r.observer) {
                    state.nodes.retain(|&n| n != node);
                }
            }
        }
    }
}
