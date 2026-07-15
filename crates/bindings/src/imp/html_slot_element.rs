//! `HTMLSlotElement` implementation (`name` reflection + assigned nodes).
//!
//! v1 limits (ADR-0010): no `slotchange` event; `options.flatten` is ignored.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::node::attr_name;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

pub(crate) fn name(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom
        .node(this)
        .as_element()
        .and_then(|el| el.attr(&attr_name(LocalName::from("name"))))
        .map(|v| v.to_string())
        .unwrap_or_default())
}

pub(crate) fn set_name(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(this, attr_name(LocalName::from("name")), value.into());
    Ok(())
}

fn assigned(cx: &BindCx<'_>, this: NodeId, elements_only: bool) -> Result<JsValue, JsThrow> {
    let nodes: Vec<JsValue> = {
        let ids = {
            let dom = cx.state.dom.borrow();
            let mut ids = dom.assigned_slot_nodes(this);
            if elements_only {
                ids.retain(|&id| dom.node(id).as_element().is_some());
            }
            ids
        };
        ids.into_iter()
            .map(|id| cx.node_to_js(id))
            .collect::<Result<_, _>>()?
    };
    cx.scope
        .new_array(&nodes)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

pub(crate) fn assigned_nodes(
    cx: &BindCx<'_>,
    this: NodeId,
    _options: JsValue,
) -> Result<JsValue, JsThrow> {
    assigned(cx, this, false)
}

pub(crate) fn assigned_elements(
    cx: &BindCx<'_>,
    this: NodeId,
    _options: JsValue,
) -> Result<JsValue, JsThrow> {
    assigned(cx, this, true)
}
