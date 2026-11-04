//! `CSSPageRule`: an `@page` rule (ADR-0041) — `selectorText` and `style`.

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};
use oxidepage_style::page_rules;
use style::stylesheets::CssRule;

use crate::cssdata::RuleData;
use crate::cx::BindCx;

pub(crate) fn selector_text(cx: &BindCx<'_>, this: Rc<RuleData>) -> Result<String, JsThrow> {
    Ok(match &this.rule {
        CssRule::Page(page) => page_rules::selector_text(&cx.style_lock(), page),
        _ => String::new(),
    })
}

/// An invalid page selector leaves the rule as it was, like `CSSStyleRule`'s.
pub(crate) fn set_selector_text(
    cx: &BindCx<'_>,
    this: Rc<RuleData>,
    value: String,
) -> Result<(), JsThrow> {
    if let CssRule::Page(page) = &this.rule {
        page_rules::set_selector_text(&cx.style_lock(), page, &value, &cx.doc_url());
    }
    Ok(())
}

/// `[SameObject]`, and served by the same live declaration `CSSStyleRule.style`
/// uses: it resolves its block from the rule on every access.
pub(crate) fn style(cx: &BindCx<'_>, this: Rc<RuleData>) -> Result<JsValue, JsThrow> {
    crate::imp::css_style_rule::style(cx, this)
}
