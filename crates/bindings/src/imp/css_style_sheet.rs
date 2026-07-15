//! `CSSStyleSheet` implementation: the rule list plus `insertRule`/`deleteRule`,
//! and the constructable-stylesheet surface (`new CSSStyleSheet()`,
//! `replaceSync`/`replace`) backing `adoptedStyleSheets`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use oxidepage_base::DomExceptionKind;
use oxidepage_js::{HostCall, JsThrow, JsValue};
use style::stylesheets::{DocumentStyleSheet, RulesMutateError, StylesheetInDocument};

use crate::cssdata::{RuleListData, SheetData};
use crate::cx::BindCx;

fn mutate_error(cx: &BindCx<'_>, error: RulesMutateError) -> JsThrow {
    let kind = match error {
        RulesMutateError::Syntax => DomExceptionKind::SyntaxError,
        RulesMutateError::IndexSize => DomExceptionKind::IndexSizeError,
        RulesMutateError::HierarchyRequest => DomExceptionKind::HierarchyRequestError,
        RulesMutateError::InvalidState => DomExceptionKind::InvalidStateError,
    };
    cx.dom_throw(kind, "rule mutation failed")
}

/// The current stylesheet for a sheet view, or an `InvalidStateError` if the
/// owning `<style>`/`<link>` no longer has an attached sheet.
fn sheet(cx: &BindCx<'_>, this: &SheetData) -> Result<DocumentStyleSheet, JsThrow> {
    super::style_sheet::resolve_sheet(cx, this).ok_or_else(|| {
        cx.dom_throw(
            DomExceptionKind::InvalidStateError,
            "stylesheet is detached",
        )
    })
}

/// `new CSSStyleSheet(options)`: an empty constructed sheet. `options.media`/
/// `options.disabled` are v1-ignored (Swiper and friends pass none).
pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    _options: JsValue,
) -> Result<JsValue, JsThrow> {
    let sheet = cx.state.style.borrow().make_stylesheet("", &cx.doc_url());
    cx.new_style_sheet(SheetData::Constructed { sheet })
}

/// `replaceSync(text)`: swaps the constructed sheet's rules in place, so every
/// adopting scope observes the new contents on the next cascade.
pub(crate) fn replace_sync(
    cx: &BindCx<'_>,
    this: Rc<SheetData>,
    text: String,
) -> Result<(), JsThrow> {
    let Some(sheet) = this.constructed_sheet() else {
        return Err(cx.dom_throw(
            DomExceptionKind::NotAllowedError,
            "replaceSync is only allowed on constructed stylesheets",
        ));
    };
    let fresh = cx
        .state
        .style
        .borrow()
        .make_stylesheet(&text, &cx.doc_url());
    let lock = cx.style_lock();
    let (rules_handle, new_rules) = {
        let guard = lock.read();
        (
            sheet.contents(&guard).rules.clone(),
            fresh.contents(&guard).rules.read_with(&guard).0.clone(),
        )
    };
    {
        let mut write = lock.write();
        rules_handle.write_with(&mut write).0 = new_rules;
    }
    cx.state.style.borrow_mut().note_constructed_sheet_changed();
    Ok(())
}

/// `replace(text)`: the async variant; parsing is synchronous here, so it is
/// `replaceSync` resolved with the sheet.
pub(crate) fn replace(
    cx: &BindCx<'_>,
    this: Rc<SheetData>,
    text: String,
) -> Result<JsValue, JsThrow> {
    match replace_sync(cx, this, text) {
        Ok(()) => cx.resolved_promise(JsValue::Undefined),
        Err(JsThrow::Value(value)) => cx.rejected_promise(value),
        Err(other) => Err(other),
    }
}

pub(crate) fn owner_rule(_cx: &BindCx<'_>, _this: Rc<SheetData>) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Null)
}

pub(crate) fn css_rules(cx: &BindCx<'_>, this: Rc<SheetData>) -> Result<JsValue, JsThrow> {
    // v1: the CSSOM rule views are owner-node-keyed; constructed sheets
    // expose no rule list (replace/replaceSync cover their use cases).
    let Some(owner) = this.owner() else {
        return Err(cx.dom_throw(
            DomExceptionKind::NotSupportedError,
            "cssRules is not supported on constructed stylesheets (v1)",
        ));
    };
    cx.same_object(owner, crate::cx::CSS_RULES_MEMBER, move |cx| {
        cx.new_css_rule_list(RuleListData {
            owner,
            items: RefCell::new(HashMap::new()),
        })
    })
}

pub(crate) fn insert_rule(
    cx: &BindCx<'_>,
    this: Rc<SheetData>,
    rule: String,
    index: u32,
) -> Result<f64, JsThrow> {
    let sheet = sheet(cx, &this)?;
    cx.state
        .style
        .borrow_mut()
        .insert_rule(&sheet, &rule, index as usize)
        .map_err(|e| mutate_error(cx, e))?;
    if let Some(owner) = this.owner() {
        // The insert shifted every rule at or after `index`.
        cx.invalidate_css_rule_list(owner);
    } else {
        cx.state.style.borrow_mut().note_constructed_sheet_changed();
    }
    Ok(f64::from(index))
}

pub(crate) fn delete_rule(cx: &BindCx<'_>, this: Rc<SheetData>, index: u32) -> Result<(), JsThrow> {
    let sheet = sheet(cx, &this)?;
    cx.state
        .style
        .borrow_mut()
        .delete_rule(&sheet, index as usize)
        .map_err(|e| mutate_error(cx, e))?;
    if let Some(owner) = this.owner() {
        // The removal shifted every rule after `index`.
        cx.invalidate_css_rule_list(owner);
    } else {
        cx.state.style.borrow_mut().note_constructed_sheet_changed();
    }
    Ok(())
}
