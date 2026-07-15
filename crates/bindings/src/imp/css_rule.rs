//! `CSSRule` base-interface implementation.

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};
use oxidepage_style::cssom;

use crate::cssdata::{RuleData, SheetData};
use crate::cx::BindCx;

/// The owning sheet wrapper for a rule (identity via the sheet's owner node).
pub(crate) fn parent_sheet_value(cx: &BindCx<'_>, this: &RuleData) -> Result<JsValue, JsThrow> {
    let owner = this.owner;
    cx.same_object(owner, "cssom-sheet", move |cx| {
        cx.new_style_sheet(SheetData::Node { owner })
    })
}

pub(crate) fn r#type(_cx: &BindCx<'_>, this: Rc<RuleData>) -> Result<f64, JsThrow> {
    Ok(f64::from(cssom::rule_type_number(&this.rule)))
}

pub(crate) fn css_text(cx: &BindCx<'_>, this: Rc<RuleData>) -> Result<String, JsThrow> {
    Ok(cssom::rule_css_text(&cx.style_lock(), &this.rule))
}

pub(crate) fn set_css_text(
    _cx: &BindCx<'_>,
    _this: Rc<RuleData>,
    _value: String,
) -> Result<(), JsThrow> {
    // CSSOM: the `cssText` setter on a rule is a no-op.
    Ok(())
}

pub(crate) fn parent_rule(_cx: &BindCx<'_>, _this: Rc<RuleData>) -> Result<JsValue, JsThrow> {
    // Top-level rules have no parent rule (v1: no grouping-rule nesting exposed).
    Ok(JsValue::Null)
}

pub(crate) fn parent_style_sheet(cx: &BindCx<'_>, this: Rc<RuleData>) -> Result<JsValue, JsThrow> {
    parent_sheet_value(cx, &this)
}
