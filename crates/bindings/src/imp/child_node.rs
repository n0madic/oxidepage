//! `ChildNode` mixin implementation (Element, CharacterData, DocumentType).

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::imp::parent_node::{convert_nodes, node_set};

pub(crate) fn before(cx: &BindCx<'_>, this: NodeId, nodes: Vec<JsValue>) -> Result<(), JsThrow> {
    let parent = cx.state.dom.borrow().node(this).parent();
    let Some(parent) = parent else {
        return Ok(());
    };
    // Spec: find the first preceding sibling not in the inserted set.
    let set = node_set(cx, &nodes);
    let viable_prev = {
        let dom = cx.state.dom.borrow();
        let mut prev = dom.node(this).prev_sibling();
        while let Some(p) = prev {
            if !set.contains(&p) {
                break;
            }
            prev = dom.node(p).prev_sibling();
        }
        prev
    };
    let node = convert_nodes(cx, &nodes)?;
    let mut dom = cx.state.dom.borrow_mut();
    let reference = match viable_prev {
        Some(p) => dom.node(p).next_sibling(),
        None => dom.node(parent).first_child(),
    };
    dom.insert_before(parent, node, reference)
        .map_err(|e| cx.dom_exception(e))?;
    Ok(())
}

pub(crate) fn after(cx: &BindCx<'_>, this: NodeId, nodes: Vec<JsValue>) -> Result<(), JsThrow> {
    let parent = cx.state.dom.borrow().node(this).parent();
    let Some(parent) = parent else {
        return Ok(());
    };
    let set = node_set(cx, &nodes);
    let viable_next = {
        let dom = cx.state.dom.borrow();
        let mut next = dom.node(this).next_sibling();
        while let Some(n) = next {
            if !set.contains(&n) {
                break;
            }
            next = dom.node(n).next_sibling();
        }
        next
    };
    let node = convert_nodes(cx, &nodes)?;
    cx.state
        .dom
        .borrow_mut()
        .insert_before(parent, node, viable_next)
        .map_err(|e| cx.dom_exception(e))?;
    Ok(())
}

pub(crate) fn replace_with(
    cx: &BindCx<'_>,
    this: NodeId,
    nodes: Vec<JsValue>,
) -> Result<(), JsThrow> {
    let parent = cx.state.dom.borrow().node(this).parent();
    let Some(parent) = parent else {
        return Ok(());
    };
    let set = node_set(cx, &nodes);
    let viable_next = {
        let dom = cx.state.dom.borrow();
        let mut next = dom.node(this).next_sibling();
        while let Some(n) = next {
            if !set.contains(&n) {
                break;
            }
            next = dom.node(n).next_sibling();
        }
        next
    };
    let node = convert_nodes(cx, &nodes)?;
    let mut dom = cx.state.dom.borrow_mut();
    if dom.node(this).parent() == Some(parent) {
        dom.replace_child(parent, node, this)
            .map_err(|e| cx.dom_exception(e))?;
    } else {
        dom.insert_before(parent, node, viable_next)
            .map_err(|e| cx.dom_exception(e))?;
    }
    Ok(())
}

pub(crate) fn remove(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().remove(this);
    Ok(())
}
