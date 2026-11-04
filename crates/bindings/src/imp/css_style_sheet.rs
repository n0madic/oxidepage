//! `CSSStyleSheet` implementation: the rule list plus `insertRule`/`deleteRule`,
//! and the constructable-stylesheet surface (`new CSSStyleSheet()`,
//! `replaceSync`/`replace`) backing `adoptedStyleSheets`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use oxidepage_base::DomExceptionKind;
use oxidepage_js::{HostCall, JsThrow, JsValue};
use style::stylesheets::{CssRule, DocumentStyleSheet, RulesMutateError, StylesheetInDocument};

use crate::cssdata::{ConstructedSheet, RuleListData, SheetData};
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

/// `new CSSStyleSheet(options)`: an empty constructed sheet.
///
/// `options.media` becomes the sheet's media list, so its rules apply only
/// while the query matches (a `replaceSync` keeps it: it swaps the rules, not
/// the sheet). `options.disabled` starts it disabled. `title` is not a member
/// of `CSSStyleSheetInit`, so the title stays `null`. The `media` IDL
/// attribute (`MediaList`) is not implemented; the list only shapes the
/// cascade.
pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    options: JsValue,
) -> Result<JsValue, JsThrow> {
    let (media, disabled) = match &options {
        JsValue::Undefined | JsValue::Null => (None, false),
        JsValue::Object(obj) => {
            let media = match cx.scope.get(obj, "media").map_err(JsThrow::from)? {
                JsValue::Undefined => None,
                value => Some(cx.scope.coerce_string(&value).map_err(JsThrow::from)?),
            };
            let disabled = cx
                .scope
                .get(obj, "disabled")
                .map_err(JsThrow::from)?
                .truthy();
            (media, disabled)
        }
        _ => {
            return Err(JsThrow::Type(
                "CSSStyleSheet options must be a dictionary".into(),
            ));
        }
    };
    let sheet = cx.state.style.borrow().make_stylesheet_with_loader(
        "",
        &cx.doc_url(),
        media.as_deref().filter(|m| !m.is_empty()),
        None,
    );
    if disabled {
        sheet.0.set_disabled(true);
    }
    cx.new_constructed_sheet(Rc::new(ConstructedSheet {
        sheet,
        wrapper: RefCell::new(JsValue::Undefined),
    }))
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
            // CSSOM: a constructed sheet's `replace`/`replaceSync` drop every
            // `@import` (they are never fetched; `insertRule` already refuses
            // one), and the rules after it still apply.
            fresh
                .contents(&guard)
                .rules
                .read_with(&guard)
                .0
                .iter()
                .filter(|rule| !matches!(rule, CssRule::Import(_)))
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    {
        let mut write = lock.write();
        rules_handle.write_with(&mut write).0 = new_rules;
    }
    // Every rule the list handed out is a rule of the old contents.
    cx.invalidate_css_rule_list(&this);
    cx.state.style.borrow_mut().note_constructed_sheet_changed();
    Ok(())
}

/// `replace(text)`: the async variant; parsing is synchronous here, so it is
/// `replaceSync` resolved with the sheet (`Promise<CSSStyleSheet>`).
pub(crate) fn replace(
    cx: &BindCx<'_>,
    this: Rc<SheetData>,
    text: String,
) -> Result<JsValue, JsThrow> {
    let sheet = this.sheet_ref();
    match replace_sync(cx, this, text) {
        Ok(()) => cx.resolved_promise(cx.sheet_wrapper(&sheet)?),
        Err(JsThrow::Value(value)) => cx.rejected_promise(value),
        Err(other) => Err(other),
    }
}

pub(crate) fn owner_rule(_cx: &BindCx<'_>, _this: Rc<SheetData>) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Null)
}

/// `[SameObject]`: per owner node for a `<style>`/`<link>` sheet, and held by
/// the wrapper itself for a constructed one.
pub(crate) fn css_rules(cx: &BindCx<'_>, this: Rc<SheetData>) -> Result<JsValue, JsThrow> {
    let new_list = |cx: &BindCx<'_>| {
        cx.new_css_rule_list(RuleListData {
            owner: this.sheet_ref(),
            items: RefCell::new(HashMap::new()),
        })
    };
    match &*this {
        SheetData::Node { owner } => cx.same_object(*owner, crate::cx::CSS_RULES_MEMBER, new_list),
        SheetData::Constructed { rules, .. } => {
            if let Some(list) = rules.borrow().clone() {
                return Ok(list);
            }
            let list = new_list(cx)?;
            *rules.borrow_mut() = Some(list.clone());
            Ok(list)
        }
    }
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
    // The insert shifted every rule at or after `index`.
    cx.invalidate_css_rule_list(&this);
    if this.owner().is_none() {
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
    // The removal shifted every rule after `index`.
    cx.invalidate_css_rule_list(&this);
    if this.owner().is_none() {
        cx.state.style.borrow_mut().note_constructed_sheet_changed();
    }
    Ok(())
}
