//! `NonDocumentTypeChildNode` mixin implementation.

use oxidepage_base::NodeId;
use oxidepage_dom::NodeKind;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

pub(crate) fn previous_element_sibling(
    cx: &BindCx<'_>,
    this: NodeId,
) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    let mut current = dom.node(this).prev_sibling();
    while let Some(id) = current {
        if dom.node(id).data().kind() == NodeKind::Element {
            return Ok(Some(id));
        }
        current = dom.node(id).prev_sibling();
    }
    Ok(None)
}

pub(crate) fn next_element_sibling(
    cx: &BindCx<'_>,
    this: NodeId,
) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    let mut current = dom.node(this).next_sibling();
    while let Some(id) = current {
        if dom.node(id).data().kind() == NodeKind::Element {
            return Ok(Some(id));
        }
        current = dom.node(id).next_sibling();
    }
    Ok(None)
}
