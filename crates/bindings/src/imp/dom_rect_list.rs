//! `DOMRectList` implementation: an indexed, read-only list of `DOMRect`s.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::RectData;

type RectListRef = Rc<Vec<Rc<RefCell<RectData>>>>;

pub(crate) fn length(_cx: &BindCx<'_>, this: RectListRef) -> Result<f64, JsThrow> {
    Ok(this.len() as f64)
}

pub(crate) fn item(cx: &BindCx<'_>, this: RectListRef, index: u32) -> Result<JsValue, JsThrow> {
    match this.get(index as usize) {
        // A fresh wrapper each call (no identity requirement); the rect data is
        // copied out so the returned `DOMRect` is independent of the list.
        Some(rect) => cx.new_dom_rect("DOMRect", *rect.borrow()),
        None => Ok(JsValue::Null),
    }
}
