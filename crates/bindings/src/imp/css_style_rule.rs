//! `CSSStyleRule` implementation: `selectorText` and the rule's `style` block.

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};
use oxidepage_style::cssom;

use crate::cssdata::{RuleData, StyleDeclData};
use crate::cx::BindCx;

pub(crate) fn selector_text(cx: &BindCx<'_>, this: Rc<RuleData>) -> Result<String, JsThrow> {
    Ok(cssom::style_rule_selector_text(&cx.style_lock(), &this.rule).unwrap_or_default())
}

pub(crate) fn set_selector_text(
    cx: &BindCx<'_>,
    this: Rc<RuleData>,
    value: String,
) -> Result<(), JsThrow> {
    if cssom::set_style_rule_selector_text(&cx.style_lock(), &this.rule, &value, &cx.doc_url()) {
        // A selector change re-affects which elements match; reprocess author
        // origins on the next resolution.
        cx.state.style.borrow_mut().note_sheets_changed();
    }
    Ok(())
}

pub(crate) fn style(cx: &BindCx<'_>, this: Rc<RuleData>) -> Result<JsValue, JsThrow> {
    // `[SameObject]`: one CSSStyleDeclaration per rule wrapper. The declaration
    // resolves its block live from the rule, so nothing is stored here.
    if let Some(cached) = this.style.borrow().clone() {
        return Ok(cached);
    }
    let decl = cx.new_style_decl(StyleDeclData::Rule {
        owner: this.owner,
        rule: this.rule.clone(),
    })?;
    *this.style.borrow_mut() = Some(decl.clone());
    Ok(decl)
}
