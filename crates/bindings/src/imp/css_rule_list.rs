//! `CSSRuleList` implementation over a sheet's top-level rules.

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};
use oxidepage_style::cssom;

use crate::cssdata::{RuleData, RuleListData};
use crate::cx::BindCx;

pub(crate) fn length(cx: &BindCx<'_>, this: Rc<RuleListData>) -> Result<f64, JsThrow> {
    let Some(sheet) = cx.sheet_for(this.owner) else {
        return Ok(0.0);
    };
    Ok(cssom::sheet_rules(&cx.style_lock(), &sheet).len() as f64)
}

pub(crate) fn item(
    cx: &BindCx<'_>,
    this: Rc<RuleListData>,
    index: u32,
) -> Result<JsValue, JsThrow> {
    // `[SameObject]`: return the cached wrapper for this index if present.
    if let Some(cached) = this.items.borrow().get(&index).cloned() {
        return Ok(cached);
    }
    let Some(sheet) = cx.sheet_for(this.owner) else {
        return Ok(JsValue::Null);
    };
    let rules = cssom::sheet_rules(&cx.style_lock(), &sheet);
    match rules.get(index as usize) {
        Some(rule) => {
            let wrapper = cx.new_css_rule(RuleData {
                owner: this.owner,
                rule: rule.clone(),
                style: std::cell::RefCell::new(None),
            })?;
            this.items.borrow_mut().insert(index, wrapper.clone());
            Ok(wrapper)
        }
        None => Ok(JsValue::Null),
    }
}
