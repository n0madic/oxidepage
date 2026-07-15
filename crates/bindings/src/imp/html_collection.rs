//! `HTMLCollection` implementation.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::node::attr_name;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::collection_items;

pub(crate) fn length(cx: &BindCx<'_>, this: u64) -> Result<f64, JsThrow> {
    Ok(collection_items(cx, this).len() as f64)
}

pub(crate) fn item(cx: &BindCx<'_>, this: u64, index: u32) -> Result<Option<NodeId>, JsThrow> {
    Ok(collection_items(cx, this).get(index as usize).copied())
}

pub(crate) fn named_item(
    cx: &BindCx<'_>,
    this: u64,
    name: String,
) -> Result<Option<NodeId>, JsThrow> {
    if name.is_empty() {
        return Ok(None);
    }
    let items = collection_items(cx, this);
    let dom = cx.state.dom.borrow();
    let name_attr = attr_name(LocalName::from("name"));
    Ok(items.into_iter().find(|&id| {
        let Some(el) = dom.node(id).as_element() else {
            return false;
        };
        if el.id().is_some_and(|el_id| **el_id == *name) {
            return true;
        }
        el.is_html_element() && el.attr(&name_attr).is_some_and(|v| **v == *name)
    }))
}
