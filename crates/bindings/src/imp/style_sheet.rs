//! `StyleSheet` base-interface implementation (shared by `CSSStyleSheet`).

use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::node::attr_name;
use oxidepage_js::{JsThrow, JsValue};
use style::stylesheets::DocumentStyleSheet;

use crate::cssdata::SheetData;
use crate::cx::BindCx;

/// An attribute value of the sheet's owner element, if the element still exists.
fn owner_attr(cx: &BindCx<'_>, this: &SheetData, name: &str) -> Option<String> {
    let owner = this.owner()?;
    let dom = cx.state.dom.borrow();
    dom.get(owner)
        .and_then(|n| n.as_element())
        .and_then(|el| el.attr(&attr_name(LocalName::from(name))))
        .map(|v| v.to_string())
}

/// The live stylesheet behind this wrapper (node-owned resolved from the
/// engine; constructed carried directly).
pub(crate) fn resolve_sheet(cx: &BindCx<'_>, this: &SheetData) -> Option<DocumentStyleSheet> {
    match this {
        SheetData::Node { owner } => cx.sheet_for(*owner),
        SheetData::Constructed { sheet } => Some(sheet.clone()),
    }
}

pub(crate) fn r#type(_cx: &BindCx<'_>, _this: Rc<SheetData>) -> Result<String, JsThrow> {
    Ok("text/css".to_owned())
}

pub(crate) fn href(cx: &BindCx<'_>, this: Rc<SheetData>) -> Result<Option<String>, JsThrow> {
    Ok(owner_attr(cx, &this, "href"))
}

pub(crate) fn owner_node(cx: &BindCx<'_>, this: Rc<SheetData>) -> Result<Option<NodeId>, JsThrow> {
    // A freed owner node yields `null`, not a thrown "stale node".
    Ok(this
        .owner()
        .filter(|&n| cx.state.dom.borrow().get(n).is_some()))
}

pub(crate) fn parent_style_sheet(
    _cx: &BindCx<'_>,
    _this: Rc<SheetData>,
) -> Result<JsValue, JsThrow> {
    // Document-owned author sheets have no parent sheet (v1: no nested import).
    Ok(JsValue::Null)
}

pub(crate) fn title(cx: &BindCx<'_>, this: Rc<SheetData>) -> Result<Option<String>, JsThrow> {
    Ok(owner_attr(cx, &this, "title").filter(|s| !s.is_empty()))
}

pub(crate) fn disabled(cx: &BindCx<'_>, this: Rc<SheetData>) -> Result<bool, JsThrow> {
    Ok(resolve_sheet(cx, &this).is_some_and(|s| s.0.disabled()))
}

pub(crate) fn set_disabled(
    cx: &BindCx<'_>,
    this: Rc<SheetData>,
    value: bool,
) -> Result<(), JsThrow> {
    if let Some(sheet) = resolve_sheet(cx, &this) {
        cx.state
            .style
            .borrow_mut()
            .set_sheet_disabled(&sheet, value);
    }
    Ok(())
}
