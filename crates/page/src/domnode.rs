//! The protocol-neutral node surface: naming a node across a thread boundary,
//! describing it, querying for it, and measuring it (ADR-0031).
//!
//! Nothing here knows what CDP is — the same rule `remote.rs` states. The
//! shapes match its vocabulary because that is the vocabulary every automation
//! protocol settled on, but they are plain Rust and the protocol crate does the
//! JSON.
//!
//! Almost nothing here enters JS: a node description is a pure [`DomTree`]
//! read, and so are the selector queries. Only [`Page::node_object`] and
//! [`Page::node_for_object`] need a realm, because a `RemoteObject` *is* a live
//! JS value by definition.

use oxidepage_base::NodeId;
use oxidepage_bindings::remote::{RemoteObject, RemoteOptions, describe};
use oxidepage_dom::node::NodeData;
use oxidepage_dom::{NodeKind, ShadowMode, parse_selector_list};
use oxidepage_layout::BoxQuads;

use crate::Page;
use crate::remote::RemoteError;

/// Deepest subtree one node description carries, and what `depth: -1` means.
///
/// **A bound, not a preference.** Building a description recurses once per
/// level, and so does every consumer of the result — the protocol layer's JSON
/// construction, `serde_json`'s serializer, and the nested `Value`'s own
/// recursive `Drop`. A page can nest to any depth it likes
/// (`document.body.innerHTML = '<div>'.repeat(12000)` is enough), so an
/// unbounded `depth: -1` is a native stack overflow — an *abort of the whole
/// endpoint process*, reachable from page content.
///
/// Truncation is not a lie: `child_node_count` still reports the real number of
/// children at the boundary, so a driver can re-root a second `describeNode`
/// deeper and continue. That split is exactly why CDP carries the count
/// separately from the children in the first place.
///
/// The value is ~10× the deepest real-world DOM and comfortably inside the
/// smallest stack any of the four passes runs on.
pub const MAX_DESCRIPTION_DEPTH: i32 = 1_000;

/// How a caller names a node across the thread boundary.
///
/// One type rather than two entry points, because a protocol command
/// legitimately offers the choice — and because resolving in one job and
/// *using* the result in the next would reopen the staleness window the handle
/// table exists to close. Every method that takes a `NodeRef` resolves it
/// inside the same closure that acts on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRef {
    /// A [`backend node handle`](crate::node_handle::NodeHandleStore).
    Handle(u64),
    /// A remote object handle whose value is a DOM node wrapper.
    Object(u64),
}

/// One node as a driver sees it — CDP's `DOM.Node`, minus the members that only
/// make sense for the inspector's pushed node tree.
#[derive(Clone, Debug, Default)]
pub struct NodeDescription {
    /// This node's backend handle.
    pub handle: u64,
    /// The parent's handle, absent for a root.
    pub parent: Option<u64>,
    pub node_type: u16,
    /// HTML-upper-cased and qualified, exactly as `Node.nodeName` reports it.
    pub node_name: String,
    /// The element's local name; empty for every other kind.
    pub local_name: String,
    /// The character data of a Text/CDATA/Comment/PI node; empty otherwise.
    pub node_value: String,
    /// Attributes in document order, name qualified.
    pub attributes: Vec<(String, String)>,
    /// Number of children — `None` for the kinds that cannot have any, which is
    /// how Chrome omits the member.
    pub child_node_count: Option<u32>,
    /// Populated only within the requested `depth`.
    pub children: Vec<NodeDescription>,
    /// Document only.
    pub document_url: Option<String>,
    /// Document only.
    pub base_url: Option<String>,
    /// DocumentType only.
    pub doctype_name: Option<String>,
    pub public_id: Option<String>,
    pub system_id: Option<String>,
    /// `"open"` / `"closed"` on a shadow-root fragment.
    pub shadow_root_mode: Option<&'static str>,
    /// A host element's shadow root, when piercing was asked for.
    pub shadow_roots: Vec<NodeDescription>,
}

/// The document's scroll position and the two sizes a driver measures against —
/// CDP's `Page.getLayoutMetrics`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutMetrics {
    pub scroll_x: f32,
    pub scroll_y: f32,
    /// The layout viewport, in CSS px.
    pub client_width: f32,
    pub client_height: f32,
    /// The document's full scrollable extent, in CSS px.
    pub content_width: f32,
    pub content_height: f32,
}

impl Page {
    /// The document node. Always arena slot `(0, generation 1)`, which is what
    /// lets it survive navigation while every other id of the outgoing document
    /// goes stale.
    #[must_use]
    pub fn document_node(&self) -> NodeId {
        self.state.dom.borrow().document()
    }

    /// Resolves either naming of a node to a live [`NodeId`].
    pub fn resolve_node_ref(&self, node: NodeRef) -> Result<NodeId, RemoteError> {
        match node {
            NodeRef::Handle(handle) => {
                let id = self
                    .node_handles
                    .borrow()
                    .get(handle)
                    .ok_or(RemoteError::NoSuchObject(handle))?;
                // The store does not pin, so the handle may name a node that has
                // since been freed. `Arena::get` is the generation check.
                if self.state.dom.borrow().get(id).is_none() {
                    return Err(RemoteError::NoSuchObject(handle));
                }
                Ok(id)
            }
            NodeRef::Object(object_id) => self.node_for_object(object_id),
        }
    }

    /// The stable handle for `node`, minting one on first sight.
    ///
    /// A full table is swept of entries whose nodes are gone before the failure
    /// is reported: a dead node's handle could not have been resolved anyway,
    /// so dropping it costs nothing and a long-lived page that churns its DOM
    /// does not hit a ceiling it can never come back from.
    pub fn node_handle(&self, node: NodeId) -> Result<u64, RemoteError> {
        let mut store = self.node_handles.borrow_mut();
        if let Some(handle) = store.intern(node) {
            return Ok(handle);
        }
        {
            let dom = self.state.dom.borrow();
            store.retain(|id| dom.get(id).is_some());
        }
        store.intern(node).ok_or(RemoteError::OutOfHandles)
    }

    /// The node a remote object handle names.
    ///
    /// A live value that is not a node wrapper is a [`RemoteError::WrongType`],
    /// never a panic — the id comes off the wire.
    pub fn node_for_object(&self, object_id: u64) -> Result<NodeId, RemoteError> {
        // Routed to the world that minted the handle: the value is a
        // `Persistent` of that runtime, and reading it anywhere else fails the
        // brand check (ADR-0033 D1).
        let world = self
            .shared
            .object_world(object_id)
            .ok_or(RemoteError::NoSuchObject(object_id))?;
        let value = self
            .worlds
            .get(world)
            .and_then(|w| w.state())
            .and_then(|state| state.remote_objects.borrow().get(object_id))
            .ok_or(RemoteError::NoSuchObject(object_id))?;
        self.with_cx_in(world, |cx| cx.this_node(&value))
            .unwrap_or(Err(oxidepage_js::JsThrow::Type("no such world".into())))
            .map_err(|_| {
                RemoteError::WrongType(String::from(
                    "Node with given id does not belong to the document",
                ))
            })
    }

    /// Mints a remote object handle for `node` — CDP's `DOM.resolveNode`.
    ///
    /// Goes through the wrapper cache, so a node the page already handed to
    /// script resolves to the *same* JS object, and creating the wrapper pins
    /// the node for as long as the driver holds it.
    pub fn node_object(
        &self,
        node: NodeId,
        group: Option<&str>,
    ) -> Result<RemoteObject, RemoteError> {
        self.node_object_in(node, None, group)
    }

    /// Mints the handle in the world a `Runtime.ExecutionContextId` names.
    ///
    /// ADR-0031 D3 validated this id and then ignored it, because there was one
    /// world under many names. ADR-0033 supersedes that: `DOM.resolveNode`'s
    /// `executionContextId` now selects the world, which is what makes
    /// Puppeteer's and Playwright's `adoptBackendNode` hand back a handle their
    /// utility world can actually call.
    pub fn node_object_in(
        &self,
        node: NodeId,
        context_id: Option<u64>,
        group: Option<&str>,
    ) -> Result<RemoteObject, RemoteError> {
        let world = match context_id {
            None => oxidepage_bindings::MAIN_WORLD,
            Some(id) => self.world_id_by_context(id).ok_or_else(|| {
                RemoteError::WrongType(String::from("Cannot find context with specified id"))
            })?,
        };
        let object = self
            .with_cx_in(world, |cx| {
                let value = cx.node_to_js(node).map_err(|_| {
                    RemoteError::WrongType(String::from(
                        "Node with given id does not belong to the document",
                    ))
                })?;
                Ok(describe(
                    cx,
                    &value,
                    RemoteOptions {
                        by_value: false,
                        group,
                    },
                ))
            })
            .unwrap_or_else(|| {
                Err(RemoteError::WrongType(String::from(
                    "the execution context could not be entered",
                )))
            })?;
        // A node wrapper is an object, so an absent `objectId` can only mean the
        // handle table is full — the same read `remote::describe` does.
        if object.object_id.is_none() {
            return Err(RemoteError::OutOfHandles);
        }
        Ok(object)
    }

    /// Describes `node` and, within `depth`, its subtree.
    ///
    /// `depth` counts levels of children: `0` describes the node alone, `1`
    /// adds its children, `-1` the whole subtree — capped at
    /// [`MAX_DESCRIPTION_DEPTH`]. `pierce` additionally descends into shadow
    /// roots.
    ///
    /// No JS is entered — this is a pure DOM read, which is what lets a
    /// driver's `describeNode` round trip stay cheap even though Puppeteer
    /// performs one on nearly every `ElementHandle` call.
    pub fn describe_node(
        &self,
        node: NodeId,
        depth: i32,
        pierce: bool,
    ) -> Result<NodeDescription, RemoteError> {
        {
            let dom = self.state.dom.borrow();
            if dom.get(node).is_none() {
                return Err(RemoteError::NoSuchObject(0));
            }
            // Handles are minted while `dom` is borrowed, so the borrow is
            // released before the walk: `node_handle` takes its own.
        }
        let depth = if (0..=MAX_DESCRIPTION_DEPTH).contains(&depth) {
            depth
        } else {
            MAX_DESCRIPTION_DEPTH
        };
        self.describe_tree(node, depth, pierce)
    }

    /// The four CSS boxes of `node` — CDP's `DOM.getBoxModel`. `None` when the
    /// element generates no box.
    #[must_use]
    pub fn box_quads(&self, node: NodeId) -> Option<BoxQuads> {
        self.flush_layout();
        self.state.layout.borrow().box_quads(node)
    }

    /// The document's scroll position, viewport size and content size.
    #[must_use]
    pub fn layout_metrics(&self) -> LayoutMetrics {
        self.flush_layout();
        let viewport = self.viewport();
        let layout = self.state.layout.borrow();
        // The *document* scroll, which lives on the viewport and not on the
        // document node's box — `scroll_offset` would report the element scroll
        // of a node that has none, i.e. always zero.
        let scroll = layout.viewport_scroll();
        let (content_width, content_height) = layout.document_content_extent();
        LayoutMetrics {
            scroll_x: scroll.x,
            scroll_y: scroll.y,
            client_width: viewport.width,
            client_height: viewport.height,
            // The scrollable area is at least the viewport, which is what
            // `Page.getLayoutMetrics` reports for a page shorter than its window.
            content_width: content_width.max(viewport.width),
            content_height: content_height.max(viewport.height),
        }
    }

    /// `querySelector` rooted at `node`. An invalid selector is an `Err`
    /// carrying the reason, never a panic — the string comes off the wire.
    pub fn query_selector(&self, root: NodeId, selector: &str) -> Result<Option<NodeId>, String> {
        let list = parse_selector_list(selector).map_err(|e| e.message.to_string())?;
        let dom = self.state.dom.borrow();
        if dom.get(root).is_none() {
            return Ok(None);
        }
        Ok(dom.query_selector(root, &list))
    }

    /// `querySelectorAll` rooted at `node`.
    pub fn query_selector_all(&self, root: NodeId, selector: &str) -> Result<Vec<NodeId>, String> {
        let list = parse_selector_list(selector).map_err(|e| e.message.to_string())?;
        let dom = self.state.dom.borrow();
        if dom.get(root).is_none() {
            return Ok(Vec::new());
        }
        Ok(dom.query_selector_all(root, &list))
    }

    // === internals ===

    /// [`Page::describe_node`]'s walk, on an **explicit stack**.
    ///
    /// Not recursive, for the same reason `dom::serialize` is not: a page nests
    /// as deep as it likes, and one native frame per level overflows the stack
    /// and aborts the process. The depth clamp bounds the *result*, which is
    /// what protects the recursive consumers downstream (JSON construction,
    /// serialization, and the nested value's own drop); this loop is what makes
    /// producing it free of stack cost.
    fn describe_tree(
        &self,
        node: NodeId,
        depth: i32,
        pierce: bool,
    ) -> Result<NodeDescription, RemoteError> {
        let mut stack = vec![self.begin_frame(node, depth, pierce, Slot::Root)?];
        loop {
            // The next step is decided under a short borrow of the top frame,
            // then taken outside it: `begin_frame` needs `&self` and the frame's
            // own iterator is being advanced.
            let step = {
                let top = stack.last_mut().expect("the stack is never empty");
                if top.depth > 0
                    && let Some(child) = top.children.next()
                {
                    Step::Push(child, top.depth - 1, Slot::Child)
                } else if let Some(root) = top.shadow.take() {
                    // A shadow root is a separate tree, so it is described at
                    // its host's depth rather than one level down: piercing is a
                    // sideways step, not a descent.
                    Step::Push(root, top.depth, Slot::Shadow)
                } else {
                    Step::Pop
                }
            };
            match step {
                Step::Push(id, depth, slot) => {
                    stack.push(self.begin_frame(id, depth, pierce, slot)?);
                }
                Step::Pop => {
                    let done = stack.pop().expect("the stack is never empty");
                    let Some(parent) = stack.last_mut() else {
                        return Ok(done.description);
                    };
                    match done.slot {
                        Slot::Shadow => parent.description.shadow_roots.push(done.description),
                        Slot::Root | Slot::Child => {
                            parent.description.children.push(done.description);
                        }
                    }
                }
            }
        }
    }

    /// One node's own description, plus what [`Page::describe_tree`] still owes
    /// it: its children and, when piercing, its shadow root.
    fn begin_frame(
        &self,
        node: NodeId,
        depth: i32,
        pierce: bool,
        slot: Slot,
    ) -> Result<Frame, RemoteError> {
        // Not `unwrap_or(0)`: `0` is the handle that means *no node*, so an
        // exhausted table would otherwise emit descriptions that look valid and
        // can never be addressed, with no error anywhere.
        let handle = self.node_handle(node)?;
        let dom = self.state.dom.borrow();
        let parent = dom.node(node).parent();
        let node_name = oxidepage_dom::node_name(&dom, node);
        let kind = dom.node(node).data().kind();
        let children: Vec<NodeId> = dom.children(node).collect();

        let mut description = NodeDescription {
            handle,
            parent: None,
            node_type: kind.node_type(),
            node_name,
            node_value: dom
                .node(node)
                .character_data()
                .map(ToString::to_string)
                .unwrap_or_default(),
            child_node_count: has_children_member(kind).then_some(children.len() as u32),
            ..NodeDescription::default()
        };

        let mut shadow_root = None;
        match dom.node(node).data() {
            NodeData::Element(el) => {
                description.local_name = el.name.local.to_string();
                description.attributes = el
                    .attrs()
                    .iter()
                    .map(|a| (oxidepage_dom::qualified_name(&a.name), a.value.to_string()))
                    .collect();
                shadow_root = el.shadow_root();
            }
            NodeData::Document(doc) => {
                description.document_url = Some(doc.url.clone());
                description.base_url = Some(dom.base_url_of(node));
            }
            NodeData::Doctype {
                name,
                public_id,
                system_id,
            } => {
                description.doctype_name = Some(name.to_string());
                description.public_id = Some(public_id.to_string());
                description.system_id = Some(system_id.to_string());
            }
            NodeData::DocumentFragment {
                shadow: Some(mode), ..
            } => description.shadow_root_mode = Some(shadow_mode_str(*mode)),
            _ => {}
        }
        drop(dom);

        if let Some(parent) = parent {
            description.parent = self.node_handle(parent).ok();
        }
        Ok(Frame {
            description,
            children: children.into_iter(),
            depth,
            shadow: pierce.then_some(shadow_root).flatten(),
            slot,
        })
    }
}

/// One node's place in its parent's description.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Root,
    Child,
    Shadow,
}

/// What [`Page::describe_tree`] does next, decided outside the frame borrow.
enum Step {
    Push(NodeId, i32, Slot),
    Pop,
}

/// A node whose own description is built but whose subtree is not.
struct Frame {
    description: NodeDescription,
    children: std::vec::IntoIter<NodeId>,
    /// Depth budget for this node's *children*.
    depth: i32,
    /// Shadow root still owed, when piercing.
    shadow: Option<NodeId>,
    slot: Slot,
}

/// Whether a node kind can hold children, and therefore whether a driver should
/// be told a child count at all. Chrome omits the member for the rest.
fn has_children_member(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Document | NodeKind::DocumentFragment | NodeKind::Element
    )
}

fn shadow_mode_str(mode: ShadowMode) -> &'static str {
    mode.as_str()
}

/// An owned key event, for the command boundary.
///
/// [`KeyInput`](oxidepage_bindings::KeyInput) borrows its strings on purpose —
/// it is built at the call site and consumed immediately — but a command
/// crossing a channel must be `Send + 'static`, and a `&str` cannot live in
/// that closure. So the boundary type owns, and the closure rebuilds the
/// borrowing form on the page thread.
#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub kind: oxidepage_bindings::KeyEventKind,
    /// The `KeyboardEvent.key` value.
    pub key: String,
    pub modifiers: oxidepage_bindings::Modifiers,
    pub repeat: bool,
    /// The text this key types, overriding the key table. `Some("")` means
    /// "types nothing" and is not the same as `None`.
    pub text: Option<String>,
    /// `KeyboardEvent.code` — the physical key, which the driver knows and the
    /// US-layout table can only guess at.
    pub code: Option<String>,
    /// `KeyboardEvent.location`.
    pub location: u32,
}
