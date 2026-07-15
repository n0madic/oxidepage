use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

fn attr_value(cx: &BindCx<'_>, owner: NodeId, index: usize) -> Result<JsValue, JsThrow> {
    let name = {
        let dom = cx.state.dom.borrow();
        let Some(element) = dom.get(owner).and_then(|node| node.as_element()) else {
            return Ok(JsValue::Null);
        };
        element.attrs().get(index).map(|attr| attr.name.clone())
    };
    match name {
        Some(name) => cx.new_attr(owner, name),
        None => Ok(JsValue::Null),
    }
}

pub(crate) fn item(cx: &BindCx<'_>, this: NodeId, index: u32) -> Result<JsValue, JsThrow> {
    attr_value(cx, this, index as usize)
}

pub(crate) fn get_named_item(
    cx: &BindCx<'_>,
    this: NodeId,
    qualified_name: String,
) -> Result<JsValue, JsThrow> {
    let qualified_name = crate::imp::element::html_lowercased(cx, this, &qualified_name);
    let index = {
        let dom = cx.state.dom.borrow();
        dom.get(this)
            .and_then(|node| node.as_element())
            .and_then(|element| {
                element.attrs().iter().position(|attr| {
                    crate::imp::names::qualified_name(&attr.name) == qualified_name
                })
            })
    };
    match index {
        Some(index) => attr_value(cx, this, index),
        None => Ok(JsValue::Null),
    }
}

pub(crate) fn get_named_item_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    local_name: String,
) -> Result<JsValue, JsThrow> {
    let namespace = namespace.unwrap_or_default();
    let index = {
        let dom = cx.state.dom.borrow();
        dom.get(this)
            .and_then(|node| node.as_element())
            .and_then(|element| {
                element
                    .attrs()
                    .iter()
                    .position(|attr| *attr.name.ns == namespace && *attr.name.local == local_name)
            })
    };
    match index {
        Some(index) => attr_value(cx, this, index),
        None => Ok(JsValue::Null),
    }
}

pub(crate) fn length(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    let len = cx
        .state
        .dom
        .borrow()
        .get(this)
        .and_then(|node| node.as_element())
        .map_or(0, |element| element.attrs().len());
    Ok(len as f64)
}
