use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::state::AttrData;

pub(crate) fn namespace_uri(
    _cx: &BindCx<'_>,
    this: Rc<AttrData>,
) -> Result<Option<String>, JsThrow> {
    Ok((!this.name.ns.is_empty()).then(|| this.name.ns.to_string()))
}

pub(crate) fn prefix(_cx: &BindCx<'_>, this: Rc<AttrData>) -> Result<Option<String>, JsThrow> {
    Ok(this.name.prefix.as_ref().map(ToString::to_string))
}

pub(crate) fn local_name(_cx: &BindCx<'_>, this: Rc<AttrData>) -> Result<String, JsThrow> {
    Ok(this.name.local.to_string())
}

pub(crate) fn name(_cx: &BindCx<'_>, this: Rc<AttrData>) -> Result<String, JsThrow> {
    Ok(crate::imp::names::qualified_name(&this.name))
}

pub(crate) fn value(cx: &BindCx<'_>, this: Rc<AttrData>) -> Result<String, JsThrow> {
    let value = cx
        .state
        .dom
        .borrow()
        .get(this.owner)
        .and_then(|node| node.as_element())
        .and_then(|element| element.attr(&this.name))
        .map(ToString::to_string)
        .unwrap_or_default();
    Ok(value)
}

pub(crate) fn set_value(cx: &BindCx<'_>, this: Rc<AttrData>, value: String) -> Result<(), JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(this.owner, this.name.clone(), value.into());
    Ok(())
}

pub(crate) fn owner_element(
    cx: &BindCx<'_>,
    this: Rc<AttrData>,
) -> Result<Option<NodeId>, JsThrow> {
    let attached = cx
        .state
        .dom
        .borrow()
        .get(this.owner)
        .and_then(|node| node.as_element())
        .is_some_and(|element| element.attr(&this.name).is_some());
    Ok(attached.then_some(this.owner))
}
