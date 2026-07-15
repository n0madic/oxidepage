//! `ParentNode` mixin implementation (Document, DocumentFragment, Element).

use oxidepage_base::NodeId;
use oxidepage_dom::NodeKind;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::state::TAG_NODE;

/// Spec "converting nodes into a node": strings become Text nodes; multiple
/// items are wrapped in a DocumentFragment.
pub(crate) fn convert_nodes(cx: &BindCx<'_>, values: &[JsValue]) -> Result<NodeId, JsThrow> {
    let mut nodes = Vec::with_capacity(values.len());
    for value in values {
        match cx.scope.host_payload(value) {
            Some((TAG_NODE, _)) => nodes.push(cx.this_node(value)?),
            _ => {
                let text = cx.scope.coerce_string(value).map_err(JsThrow::from)?;
                nodes.push(cx.state.dom.borrow_mut().create_text(text.into()));
            }
        }
    }
    if nodes.len() == 1 {
        return Ok(nodes[0]);
    }
    let mut dom = cx.state.dom.borrow_mut();
    let fragment = dom.create_document_fragment();
    for node in nodes {
        dom.append_child(fragment, node)
            .map_err(|e| cx.dom_exception(e))?;
    }
    Ok(fragment)
}

/// The set of nodes named in a `before`/`after`/`replaceWith` call, used to
/// find viable siblings.
pub(crate) fn node_set(cx: &BindCx<'_>, values: &[JsValue]) -> Vec<NodeId> {
    values
        .iter()
        .filter_map(|value| match cx.scope.host_payload(value) {
            Some((TAG_NODE, _)) => cx.this_node(value).ok(),
            _ => None,
        })
        .collect()
}

pub(crate) fn children(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "children", |cx| {
        cx.new_collection("HTMLCollection", CollectionData::Children(this))
    })
}

pub(crate) fn first_element_child(
    cx: &BindCx<'_>,
    this: NodeId,
) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom
        .children(this)
        .find(|&c| dom.node(c).data().kind() == NodeKind::Element))
}

pub(crate) fn last_element_child(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    let mut last = None;
    for child in dom.children(this) {
        if dom.node(child).data().kind() == NodeKind::Element {
            last = Some(child);
        }
    }
    Ok(last)
}

pub(crate) fn child_element_count(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom
        .children(this)
        .filter(|&c| dom.node(c).data().kind() == NodeKind::Element)
        .count() as f64)
}

pub(crate) fn prepend(cx: &BindCx<'_>, this: NodeId, nodes: Vec<JsValue>) -> Result<(), JsThrow> {
    let node = convert_nodes(cx, &nodes)?;
    let mut dom = cx.state.dom.borrow_mut();
    let first = dom.node(this).first_child();
    dom.insert_before(this, node, first)
        .map_err(|e| cx.dom_exception(e))?;
    Ok(())
}

pub(crate) fn append(cx: &BindCx<'_>, this: NodeId, nodes: Vec<JsValue>) -> Result<(), JsThrow> {
    let node = convert_nodes(cx, &nodes)?;
    cx.state
        .dom
        .borrow_mut()
        .append_child(this, node)
        .map_err(|e| cx.dom_exception(e))?;
    Ok(())
}

pub(crate) fn replace_children(
    cx: &BindCx<'_>,
    this: NodeId,
    nodes: Vec<JsValue>,
) -> Result<(), JsThrow> {
    let node = convert_nodes(cx, &nodes)?;
    let removed: Vec<NodeId> = {
        let mut dom = cx.state.dom.borrow_mut();
        let old: Vec<NodeId> = dom.children(this).collect();
        for &child in &old {
            // The replacement node's contents may include old children that
            // were moved into the fragment; skip nodes no longer attached.
            if dom.node(child).parent() == Some(this) {
                dom.remove(child);
            }
        }
        dom.append_child(this, node)
            .map_err(|e| cx.dom_exception(e))?;
        old
    };
    cx.free_detached(&removed);
    Ok(())
}

pub(crate) fn query_selector(
    cx: &BindCx<'_>,
    this: NodeId,
    selectors: String,
) -> Result<Option<NodeId>, JsThrow> {
    let list = oxidepage_dom::parse_selector_list(&selectors).map_err(|e| cx.dom_exception(e))?;
    Ok(cx.state.dom.borrow().query_selector(this, &list))
}

pub(crate) fn query_selector_all(
    cx: &BindCx<'_>,
    this: NodeId,
    selectors: String,
) -> Result<JsValue, JsThrow> {
    let list = oxidepage_dom::parse_selector_list(&selectors).map_err(|e| cx.dom_exception(e))?;
    let items = cx.state.dom.borrow().query_selector_all(this, &list);
    cx.new_collection("NodeList", CollectionData::StaticNodes(items))
}
