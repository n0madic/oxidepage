//! `NonElementParentNode` mixin implementation (Document, DocumentFragment).

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

pub(crate) fn get_element_by_id(
    cx: &BindCx<'_>,
    this: NodeId,
    id: String,
) -> Result<Option<NodeId>, JsThrow> {
    if id.is_empty() {
        return Ok(None);
    }
    let dom = cx.state.dom.borrow();
    // `this` is a Document or a DocumentFragment. The tree's id index only
    // tracks connected elements, so it can answer for a *rendered* document
    // directly — scoped to that document, since the index spans every browsing
    // context (ADR-0035 D1). An inert document's contents are disconnected and
    // need the walk, exactly like a fragment's.
    if dom.is_rendered_root(this) {
        return Ok(dom.element_by_id(this, &id));
    }
    Ok(dom.inclusive_descendants(this).skip(1).find(|&node| {
        dom.node(node)
            .as_element()
            .and_then(|el| el.id())
            .is_some_and(|el_id| **el_id == *id)
    }))
}
