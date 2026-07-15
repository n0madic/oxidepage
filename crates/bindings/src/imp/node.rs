//! `Node` implementation.

use oxidepage_base::NodeId;
use oxidepage_dom::{DomTree, Namespace, NodeData, NodeKind};
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::names::XMLNS_NS;

pub(crate) fn node_type(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(match dom.node(this).data().kind() {
        NodeKind::Element => 1.0,
        NodeKind::Text => 3.0,
        NodeKind::CdataSection => 4.0,
        NodeKind::ProcessingInstruction => 7.0,
        NodeKind::Comment => 8.0,
        NodeKind::Document => 9.0,
        NodeKind::Doctype => 10.0,
        NodeKind::DocumentFragment => 11.0,
    })
}

pub(crate) fn node_name(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(match dom.node(this).data() {
        NodeData::Element(el) => {
            // The *qualified* name, so a prefixed element reports `x:b`, not `b`.
            let name = crate::imp::names::qualified_name(&el.name);
            if el.is_html_element() {
                name.to_ascii_uppercase()
            } else {
                name
            }
        }
        NodeData::Text(_) => "#text".to_owned(),
        NodeData::CdataSection(_) => "#cdata-section".to_owned(),
        NodeData::Comment(_) => "#comment".to_owned(),
        NodeData::Document(_) => "#document".to_owned(),
        NodeData::DocumentFragment { .. } => "#document-fragment".to_owned(),
        NodeData::Doctype { name, .. } => name.to_string(),
        NodeData::ProcessingInstruction { target, .. } => target.to_string(),
    })
}

pub(crate) fn base_uri(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    let document = dom.node_document(this);
    Ok(dom.base_url_of(document))
}

/// Spec `isConnected`: the shadow-including root is a Document — true inside a
/// `new Document()`, which the engine's `IS_CONNECTED` flag deliberately does
/// not cover (that flag means "in the *rendered* document" and gates style,
/// layout, resource loads and event bubbling to the Window).
pub(crate) fn is_connected(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    Ok(cx.state.dom.borrow().is_spec_connected(this))
}

pub(crate) fn owner_document(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().owner_document(this))
}

pub(crate) fn get_root_node(
    cx: &BindCx<'_>,
    this: NodeId,
    options: JsValue,
) -> Result<NodeId, JsThrow> {
    let composed = match &options {
        JsValue::Object(obj) => cx
            .scope
            .get(obj, "composed")
            .map_err(JsThrow::from)?
            .truthy(),
        _ => false,
    };
    let dom = cx.state.dom.borrow();
    let mut root = dom.inclusive_ancestors(this).last().unwrap_or(this);
    if composed {
        // Composed root: keep crossing shadow root → host boundaries.
        while let Some(host) = dom.shadow_host(root) {
            root = dom.inclusive_ancestors(host).last().unwrap_or(host);
        }
    }
    Ok(root)
}

pub(crate) fn parent_node(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().node(this).parent())
}

pub(crate) fn parent_element(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom
        .node(this)
        .parent()
        .filter(|&p| dom.node(p).data().kind() == NodeKind::Element))
}

pub(crate) fn has_child_nodes(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    Ok(cx.state.dom.borrow().node(this).first_child().is_some())
}

pub(crate) fn child_nodes(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "childNodes", |cx| {
        cx.new_collection("NodeList", CollectionData::ChildNodes(this))
    })
}

pub(crate) fn first_child(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().node(this).first_child())
}

pub(crate) fn last_child(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().node(this).last_child())
}

pub(crate) fn previous_sibling(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().node(this).prev_sibling())
}

pub(crate) fn next_sibling(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().node(this).next_sibling())
}

pub(crate) fn node_value(cx: &BindCx<'_>, this: NodeId) -> Result<Option<String>, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom.node(this).character_data().map(|d| d.to_string()))
}

pub(crate) fn set_node_value(
    cx: &BindCx<'_>,
    this: NodeId,
    value: Option<String>,
) -> Result<(), JsThrow> {
    let mut dom = cx.state.dom.borrow_mut();
    if dom.node(this).character_data().is_some() {
        dom.set_character_data(this, value.unwrap_or_default().into());
    }
    Ok(())
}

pub(crate) fn text_content(cx: &BindCx<'_>, this: NodeId) -> Result<Option<String>, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(match dom.node(this).data().kind() {
        NodeKind::Document | NodeKind::Doctype => None,
        NodeKind::Element | NodeKind::DocumentFragment => Some(dom.text_content(this)),
        _ => dom.node(this).character_data().map(|d| d.to_string()),
    })
}

pub(crate) fn set_text_content(
    cx: &BindCx<'_>,
    this: NodeId,
    value: Option<String>,
) -> Result<(), JsThrow> {
    let value = value.unwrap_or_default();
    let kind = cx.state.dom.borrow().node(this).data().kind();
    match kind {
        NodeKind::Element | NodeKind::DocumentFragment => {
            replace_all_with_text(cx, this, &value)?;
        }
        NodeKind::Text
        | NodeKind::CdataSection
        | NodeKind::Comment
        | NodeKind::ProcessingInstruction => {
            cx.state
                .dom
                .borrow_mut()
                .set_character_data(this, value.into());
        }
        NodeKind::Document | NodeKind::Doctype => {}
    }
    Ok(())
}

/// Spec "string replace all": removes all children, inserts one Text node
/// when the string is non-empty — as a *single* observable operation, so it
/// queues one childList record rather than one per removed child plus one for
/// the insertion (`MutationObserver-textContent.html` pins this).
pub(crate) fn replace_all_with_text(
    cx: &BindCx<'_>,
    parent: NodeId,
    value: &str,
) -> Result<(), JsThrow> {
    let removed: Vec<NodeId> = {
        let mut dom = cx.state.dom.borrow_mut();
        let text = (!value.is_empty()).then(|| {
            let owner = dom.node_document(parent);
            dom.create_text_in(owner, value.into())
        });
        dom.replace_all(parent, text)
    };
    cx.free_detached(&removed);
    Ok(())
}

pub(crate) fn normalize(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    let detached = cx.state.dom.borrow_mut().normalize(this);
    cx.free_detached(&detached);
    Ok(())
}

pub(crate) fn clone_node(cx: &BindCx<'_>, this: NodeId, deep: bool) -> Result<NodeId, JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .clone_subtree(this, deep)
        .map_err(|e| cx.dom_exception(e))
}

pub(crate) fn is_equal_node(
    cx: &BindCx<'_>,
    this: NodeId,
    other: Option<NodeId>,
) -> Result<bool, JsThrow> {
    Ok(match other {
        Some(other) => cx.state.dom.borrow().is_equal_node(this, other),
        None => false,
    })
}

pub(crate) fn is_same_node(
    _cx: &BindCx<'_>,
    this: NodeId,
    other: Option<NodeId>,
) -> Result<bool, JsThrow> {
    Ok(other == Some(this))
}

pub(crate) fn compare_document_position(
    cx: &BindCx<'_>,
    this: NodeId,
    other: NodeId,
) -> Result<f64, JsThrow> {
    Ok(f64::from(
        cx.state.dom.borrow().compare_document_position(this, other),
    ))
}

pub(crate) fn contains(
    cx: &BindCx<'_>,
    this: NodeId,
    other: Option<NodeId>,
) -> Result<bool, JsThrow> {
    let Some(other) = other else {
        return Ok(false);
    };
    let dom = cx.state.dom.borrow();
    Ok(dom.inclusive_ancestors(other).any(|a| a == this))
}

/// A node's "parent element" (DOM §4.4): its parent, if that parent is an
/// element, else `None`. This does *not* climb past a non-element parent —
/// a comment whose parent is the document has no parent element at all.
fn node_parent_element(dom: &DomTree, node: NodeId) -> Option<NodeId> {
    let parent = dom.node(node).parent()?;
    (dom.node(parent).data().kind() == NodeKind::Element).then_some(parent)
}

/// DOM §4.4 "locate a namespace": the algorithm behind `lookupNamespaceURI`
/// and `isDefaultNamespace`. `prefix` is already normalized (empty string ⇒
/// `None`) by the caller.
fn locate_namespace(dom: &DomTree, node: NodeId, prefix: Option<&str>) -> Option<String> {
    match dom.node(node).data() {
        NodeData::Element(el) => {
            // `xml`/`xmlns` are intrinsically bound, regardless of the
            // element's own namespace or attributes.
            if prefix == Some("xml") {
                return Some(crate::imp::names::XML_NS.to_owned());
            }
            if prefix == Some("xmlns") {
                return Some(XMLNS_NS.to_owned());
            }
            if !el.name.ns.is_empty() && el.name.prefix.as_deref() == prefix {
                return Some(el.name.ns.to_string());
            }
            let xmlns_ns = Namespace::from(XMLNS_NS);
            for attr in el.attrs() {
                if attr.name.ns != xmlns_ns {
                    continue;
                }
                let is_match = match prefix {
                    Some(p) => {
                        attr.name.prefix.as_deref() == Some("xmlns") && &*attr.name.local == p
                    }
                    None => attr.name.prefix.is_none() && &*attr.name.local == "xmlns",
                };
                if is_match {
                    return (!attr.value.is_empty()).then(|| attr.value.to_string());
                }
            }
            node_parent_element(dom, node).and_then(|p| locate_namespace(dom, p, prefix))
        }
        // *This* document's element, not the page's: a `createDocument(null,
        // null, null)` document has none, and must report no namespace rather
        // than borrowing the page's <html>.
        NodeData::Document(_) => dom
            .document_element_of(node)
            .and_then(|doc_el| locate_namespace(dom, doc_el, prefix)),
        NodeData::Doctype { .. } | NodeData::DocumentFragment { .. } => None,
        _ => node_parent_element(dom, node).and_then(|p| locate_namespace(dom, p, prefix)),
    }
}

/// DOM §4.4 "locate a namespace prefix": the algorithm behind `lookupPrefix`.
/// `namespace` is already normalized to non-null, non-empty by the caller.
fn locate_namespace_prefix(dom: &DomTree, element: NodeId, namespace: &str) -> Option<String> {
    let el = dom.node(element).as_element().expect("element receiver");
    if el.name.prefix.is_some() && &*el.name.ns == namespace {
        return el.name.prefix.as_ref().map(ToString::to_string);
    }
    for attr in el.attrs() {
        if attr.name.prefix.as_deref() == Some("xmlns") && &*attr.value == namespace {
            return Some(attr.name.local.to_string());
        }
    }
    node_parent_element(dom, element).and_then(|p| locate_namespace_prefix(dom, p, namespace))
}

pub(crate) fn lookup_namespace_uri(
    cx: &BindCx<'_>,
    this: NodeId,
    prefix: Option<String>,
) -> Result<Option<String>, JsThrow> {
    let prefix = prefix.filter(|p| !p.is_empty());
    let dom = cx.state.dom.borrow();
    Ok(locate_namespace(&dom, this, prefix.as_deref()))
}

pub(crate) fn lookup_prefix(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
) -> Result<Option<String>, JsThrow> {
    let Some(namespace) = namespace.filter(|ns| !ns.is_empty()) else {
        return Ok(None);
    };
    let dom = cx.state.dom.borrow();
    Ok(match dom.node(this).data() {
        NodeData::Element(_) => locate_namespace_prefix(&dom, this, &namespace),
        NodeData::Document(_) => dom
            .document_element_of(this)
            .and_then(|doc_el| locate_namespace_prefix(&dom, doc_el, &namespace)),
        NodeData::Doctype { .. } | NodeData::DocumentFragment { .. } => None,
        _ => node_parent_element(&dom, this)
            .and_then(|p| locate_namespace_prefix(&dom, p, &namespace)),
    })
}

pub(crate) fn is_default_namespace(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
) -> Result<bool, JsThrow> {
    let namespace = namespace.filter(|ns| !ns.is_empty());
    let dom = cx.state.dom.borrow();
    Ok(locate_namespace(&dom, this, None) == namespace)
}

pub(crate) fn insert_before(
    cx: &BindCx<'_>,
    this: NodeId,
    node: NodeId,
    child: Option<NodeId>,
) -> Result<NodeId, JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .insert_before(this, node, child)
        .map_err(|e| cx.dom_exception(e))
}

pub(crate) fn append_child(cx: &BindCx<'_>, this: NodeId, node: NodeId) -> Result<NodeId, JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .append_child(this, node)
        .map_err(|e| cx.dom_exception(e))
}

pub(crate) fn replace_child(
    cx: &BindCx<'_>,
    this: NodeId,
    node: NodeId,
    child: NodeId,
) -> Result<NodeId, JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .replace_child(this, node, child)
        .map_err(|e| cx.dom_exception(e))
}

pub(crate) fn remove_child(
    cx: &BindCx<'_>,
    this: NodeId,
    child: NodeId,
) -> Result<NodeId, JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .remove_child(this, child)
        .map_err(|e| cx.dom_exception(e))
}
