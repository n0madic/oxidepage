//! The `DOM` domain: naming nodes over the wire, describing them, querying for
//! them and measuring them.
//!
//! **`nodeId` and `backendNodeId` are the same number** (ADR-0031 D2). Chrome
//! keeps two id spaces because it *pushes* a node tree to the client
//! (`DOM.setChildNodes`, `childNodeInserted`, …) and `nodeId` is the client's
//! cursor into that push. Those events are explicitly out of scope — they are
//! inspector features, not automation ones — so a second space would exist
//! solely to be a second name for the same node. Nothing in CDP requires them
//! disjoint.
//!
//! `DOM.enable` therefore has exactly one real consequence:
//! [`DOM.documentUpdated`](crate::pump) on every commit. That is the honest
//! signal that every id this domain ever issued is dead, and it is exactly true
//! of the engine's model — a new arena, seeded above the outgoing generation
//! high-water mark.
//!
//! The load-bearing pair is `describeNode` + `resolveNode`. Nearly every
//! Puppeteer `ElementHandle` method carries the `bindIsolatedHandle` decorator,
//! which round-trips a handle through both; without them, `page.$`, `$$`,
//! `$eval`, `waitForSelector`, `click` and `type` all fail.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use oxidepage_engine::page_api::{
    BoxQuads, LayoutMetrics, NodeDescription, NodeRef, Point, Rect, RemoteError,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domains::runtime::{remote_error, remote_object_json};
use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "DOM.enable" => set_enabled(connection, request, true),
        "DOM.disable" => set_enabled(connection, request, false),
        "DOM.getDocument" => get_document(connection, request),
        "DOM.describeNode" => describe_node(connection, request),
        "DOM.resolveNode" => resolve_node(connection, request),
        "DOM.requestNode" => request_node(connection, request),
        "DOM.querySelector" => query_selector(connection, request, false),
        "DOM.querySelectorAll" => query_selector(connection, request, true),
        "DOM.getBoxModel" => get_box_model(connection, request),
        "DOM.getContentQuads" => get_content_quads(connection, request),
        "DOM.scrollIntoViewIfNeeded" => scroll_into_view_if_needed(connection, request),
        "DOM.setFileInputFiles" => set_file_input_files(connection, request),
        "DOM.getFrameOwner" => get_frame_owner(connection, request),
        // There are no nested browsing contexts to own a frame, so there is
        // nothing to withhold — a driver can feature-detect this one.
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

fn set_enabled(connection: &Arc<Connection>, request: &Request, enabled: bool) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        /// Chrome's whitespace filter for the pushed node tree. There is no
        /// push here, so it changes nothing.
        #[serde(default)]
        include_whitespace: Option<String>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse().unwrap_or(Params {
        include_whitespace: None,
    });
    let _ = params.include_whitespace;
    session.flags.dom.store(enabled, Ordering::Relaxed);
    Ok(json!({}))
}

/// The three ways a `DOM` command may name its target.
///
/// Flattened into each command's params, with Chrome's precedence — `nodeId`,
/// then `backendNodeId`, then `objectId` — and Chrome's wording for the two
/// failures, because drivers match on the message.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct NodeTarget {
    #[serde(default)]
    node_id: Option<i64>,
    #[serde(default)]
    backend_node_id: Option<i64>,
    #[serde(default)]
    object_id: Option<String>,
}

impl NodeTarget {
    fn resolve(&self) -> Result<NodeRef, ProtocolError> {
        // `nodeId` first, then `backendNodeId` — and both name the same table,
        // because this endpoint has one id space.
        if let Some(id) = self.node_id.or(self.backend_node_id) {
            let handle = u64::try_from(id)
                .map_err(|_| ProtocolError::server("No node with given id found"))?;
            return Ok(NodeRef::Handle(handle));
        }
        if let Some(object_id) = &self.object_id {
            let id = object_id
                .parse::<u64>()
                .map_err(|_| ProtocolError::server("No node with given id found"))?;
            return Ok(NodeRef::Object(id));
        }
        Err(ProtocolError::invalid_params(
            "Either nodeId, backendNodeId or objectId must be specified",
        ))
    }
}

/// A resolution failure, in Chrome's wording.
fn node_error(error: RemoteError) -> ProtocolError {
    match error {
        RemoteError::NoSuchObject(_) => ProtocolError::server("No node with given id found"),
        other => remote_error(other),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DepthParams {
    #[serde(default)]
    depth: Option<i32>,
    #[serde(default)]
    pierce: Option<bool>,
}

fn get_document(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: DepthParams = request.parse().unwrap_or_default();
    let root = session
        .page
        .document_description(params.depth.unwrap_or(1), params.pierce.unwrap_or(false))?
        .map_err(node_error)?;
    Ok(json!({ "root": node_json(&root, &session.target_id) }))
}

fn describe_node(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(flatten)]
        target: NodeTarget,
        #[serde(flatten)]
        depth: DepthParams,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let target = params.target.resolve()?;
    // `describeNode` defaults to the node alone, unlike `getDocument`.
    let node = session
        .page
        .describe_node(
            target,
            params.depth.depth.unwrap_or(0),
            params.depth.pierce.unwrap_or(false),
        )?
        .map_err(node_error)?;
    Ok(json!({ "node": node_json(&node, &session.target_id) }))
}

/// `DOM.getFrameOwner`: the `<iframe>` embedding a frame (ADR-0035 D9).
///
/// The inverse of `DOM.Node.frameId`, and the direction a driver goes when it
/// has a frame and wants the element — Playwright's frame→element path.
/// Answers a `backendNodeId`, which is the id this engine's node handles are.
fn get_frame_owner(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        frame_id: String,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let contexts = session.page.frame_tree().unwrap_or_default();
    let frame = crate::frame::frame_by_cdp_id(&session.target_id, &contexts, &params.frame_id)
        .ok_or_else(|| ProtocolError::server("no frame with the given id"))?;
    // The top-level frame has no owning element, which is a real answer rather
    // than a missing one.
    let handle = session
        .page
        .frame_owner_handle(frame)?
        .ok_or_else(|| ProtocolError::server("the frame has no owner element"))?;
    Ok(json!({ "backendNodeId": handle, "nodeId": handle }))
}

fn resolve_node(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(flatten)]
        target: NodeTarget,
        #[serde(default)]
        execution_context_id: Option<i64>,
        #[serde(default)]
        object_group: Option<String>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let target = params.target.resolve()?;

    // The id now **selects the world** the handle is minted in (ADR-0033 D10),
    // superseding ADR-0031 D3's "validated, then ignored". A stale id — one
    // minted before a commit — is still an error rather than a silent
    // cross-document alias, because context ids are monotonic across documents
    // *and* worlds: nothing can be mistaken for anything else.
    let context_id = match params.execution_context_id {
        None => None,
        Some(raw) => Some(
            u64::try_from(raw)
                .map_err(|_| ProtocolError::server("Cannot find context with specified id"))?,
        ),
    };

    let object = session
        .page
        .resolve_node(target, context_id, params.object_group)?
        .map_err(node_error)?;
    Ok(json!({ "object": remote_object_json(&object) }))
}

fn request_node(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        object_id: String,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = params
        .object_id
        .parse::<u64>()
        .map_err(|_| ProtocolError::server("No node with given id found"))?;
    let handle = session
        .page
        .node_handle(NodeRef::Object(id))?
        .map_err(node_error)?;
    Ok(json!({ "nodeId": handle }))
}

fn query_selector(connection: &Arc<Connection>, request: &Request, all: bool) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        node_id: i64,
        selector: String,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let root = u64::try_from(params.node_id)
        .map(NodeRef::Handle)
        .map_err(|_| ProtocolError::server("No node with given id found"))?;
    let matches = session
        .page
        .query_selector(root, params.selector, all)?
        // A selector that does not parse is a `SyntaxError` in the DOM and a
        // refusal here — the string came off the wire.
        .map_err(ProtocolError::server)?;
    if all {
        return Ok(json!({ "nodeIds": matches }));
    }
    // Chrome answers `0` for "no match", which is why the handle counter starts
    // at 1 and never issues it.
    Ok(json!({ "nodeId": matches.first().copied().unwrap_or(0) }))
}

fn get_box_model(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let target: NodeTarget = request.parse()?;
    let target = target.resolve()?;
    let quads = session
        .page
        .box_quads(target)?
        .map_err(|error| match error {
            RemoteError::NoSuchObject(_) => ProtocolError::server("No node with given id found"),
            _ => ProtocolError::server("Could not compute box model."),
        })?;
    Ok(json!({ "model": box_model_json(&quads) }))
}

fn get_content_quads(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let target: NodeTarget = request.parse()?;
    let target = target.resolve()?;
    let quads = session.page.content_quads(target)?.map_err(node_error)?;
    if quads.is_empty() {
        return Err(ProtocolError::server("Could not compute content quads."));
    }
    let quads: Vec<Value> = quads.iter().map(quad_json).collect();
    Ok(json!({ "quads": quads }))
}

fn scroll_into_view_if_needed(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RectParams {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(flatten)]
        target: NodeTarget,
        #[serde(default)]
        rect: Option<RectParams>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let target = params.target.resolve()?;
    let rect = params
        .rect
        .map(|r| Rect::from_xywh(r.x as f32, r.y as f32, r.width as f32, r.height as f32));
    session
        .page
        .scroll_into_view_if_needed(target, rect)?
        .map_err(node_error)?;
    Ok(json!({}))
}

/// `DOM.setFileInputFiles` (ADR-0032 D11).
///
/// The files are read by the **server**, from its own filesystem, which is what
/// CDP defines: `elementHandle.uploadFile(path)` sends a path, not bytes. That
/// is deliberate and it is why this reads through `std::fs` rather than the
/// policy-gated `file://` loader — the driver is the operator, and the page's
/// own `file://` reach must not widen because of it.
fn set_file_input_files(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        files: Vec<String>,
        #[serde(flatten)]
        target: NodeTarget,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let target = params.target.resolve()?;
    session
        .page
        .set_file_input_files(target, params.files)?
        .map_err(node_error)?
        // The engine's own message names the file it could not read, or says the
        // node is not a file input — both are what a driver needs to see.
        .map_err(ProtocolError::server)?;
    Ok(json!({}))
}

/// CDP's `Page.getLayoutMetrics`.
///
/// Lives here rather than in `domains::page` because everything it reports is a
/// layout read, and this is the module that owns the geometry vocabulary.
pub fn get_layout_metrics(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let m: LayoutMetrics = session
        .page
        .layout_metrics()?
        .map_err(ProtocolError::server)?;
    let viewport = json!({
        "pageX": m.scroll_x,
        "pageY": m.scroll_y,
        "clientWidth": m.client_width,
        "clientHeight": m.client_height,
    });
    let content = json!({
        "x": 0, "y": 0,
        "width": m.content_width,
        "height": m.content_height,
    });
    Ok(json!({
        // The deprecated trio is still what older driver code reads, and the
        // `css*` trio is what current code reads. There is no visual viewport
        // separate from the layout one here, so both report the same numbers.
        "layoutViewport": viewport,
        "visualViewport": {
            "offsetX": 0, "offsetY": 0,
            "pageX": m.scroll_x, "pageY": m.scroll_y,
            "clientWidth": m.client_width, "clientHeight": m.client_height,
            "scale": 1,
        },
        "contentSize": content,
        "cssLayoutViewport": viewport,
        "cssVisualViewport": {
            "offsetX": 0, "offsetY": 0,
            "pageX": m.scroll_x, "pageY": m.scroll_y,
            "clientWidth": m.client_width, "clientHeight": m.client_height,
            "scale": 1,
        },
        "cssContentSize": content,
    }))
}

/// CDP's `DOM.Node`.
///
/// Three details are load-bearing:
///
/// 1. `attributes` is a **flat alternating** `[name, value, name, value, …]`
///    array, not a map or a list of pairs.
/// 2. `childNodeCount` is omitted for the kinds that cannot hold children, as
///    Chrome does.
/// 3. **`frameId` is emitted only on an `<iframe>` that owns a context.**
///    Puppeteer's `contentFrame()` returns `null` iff
///    `typeof node.frameId !== 'string'`, and Playwright's `frameLocator()`
///    resolves through the same member — so a `frameId` on anything else would
///    hand both a frame that is not there (ADR-0035 D9).
fn node_json(node: &NodeDescription, target_id: &str) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(String::from("nodeId"), json!(node.handle));
    out.insert(String::from("backendNodeId"), json!(node.handle));
    if let Some(parent) = node.parent {
        out.insert(String::from("parentId"), json!(parent));
    }
    out.insert(String::from("nodeType"), json!(node.node_type));
    out.insert(String::from("nodeName"), json!(node.node_name));
    out.insert(String::from("localName"), json!(node.local_name));
    out.insert(String::from("nodeValue"), json!(node.node_value));
    if let Some(count) = node.child_node_count {
        out.insert(String::from("childNodeCount"), json!(count));
    }
    if !node.attributes.is_empty() {
        let mut flat = Vec::with_capacity(node.attributes.len() * 2);
        for (name, value) in &node.attributes {
            flat.push(json!(name));
            flat.push(json!(value));
        }
        out.insert(String::from("attributes"), json!(flat));
    }
    if let Some(frame) = node.frame {
        out.insert(
            String::from("frameId"),
            json!(crate::frame::frame_id_for(target_id, frame, false)),
        );
    }
    if !node.children.is_empty() {
        let children: Vec<Value> = node
            .children
            .iter()
            .map(|child| node_json(child, target_id))
            .collect();
        out.insert(String::from("children"), json!(children));
    }
    if let Some(url) = &node.document_url {
        out.insert(String::from("documentURL"), json!(url));
    }
    if let Some(url) = &node.base_url {
        out.insert(String::from("baseURL"), json!(url));
    }
    if let Some(name) = &node.doctype_name {
        // A DocumentType's `name` is CDP's `name`, alongside the two ids.
        out.insert(String::from("name"), json!(name));
    }
    if let Some(public_id) = &node.public_id {
        out.insert(String::from("publicId"), json!(public_id));
    }
    if let Some(system_id) = &node.system_id {
        out.insert(String::from("systemId"), json!(system_id));
    }
    if let Some(mode) = node.shadow_root_mode {
        out.insert(String::from("shadowRootType"), json!(mode));
    }
    if !node.shadow_roots.is_empty() {
        let roots: Vec<Value> = node
            .shadow_roots
            .iter()
            .map(|root| node_json(root, target_id))
            .collect();
        out.insert(String::from("shadowRoots"), json!(roots));
    }
    Value::Object(out)
}

/// A quad as CDP spells it: eight numbers, `x1 y1 x2 y2 x3 y3 x4 y4`.
fn quad_json(quad: &[Point; 4]) -> Value {
    json!([
        quad[0].x, quad[0].y, quad[1].x, quad[1].y, quad[2].x, quad[2].y, quad[3].x, quad[3].y,
    ])
}

fn box_model_json(quads: &BoxQuads) -> Value {
    json!({
        "content": quad_json(&quads.content),
        "padding": quad_json(&quads.padding),
        "border": quad_json(&quads.border),
        "margin": quad_json(&quads.margin),
        "width": quads.width,
        "height": quads.height,
    })
}
