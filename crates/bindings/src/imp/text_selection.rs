//! The text-entry selection API, shared by `HTMLInputElement` and
//! `HTMLTextAreaElement`.
//!
//! One module rather than two copies: the members are defined once in HTML (as
//! part of the "text control" concept) and behave identically. The two `imp`
//! modules are thin forwarders.
//!
//! The offsets are **UTF-16 code units**, which is what script compares against
//! `value.length` and what the DOM layer stores.

use oxidepage_dom::SelectionDirection;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use oxidepage_base::NodeId;

/// `selectionStart`/`selectionEnd`/`selectionDirection` report `null` for a
/// control with no text entry — a checkbox, a button. Feature-detecting code
/// reads them exactly that way, so the `Option` is the answer, not an error.
fn text_entry(cx: &BindCx<'_>, this: NodeId) -> bool {
    cx.state.dom.borrow().is_text_entry(this)
}

pub(crate) fn selection_start(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    if !text_entry(cx, this) {
        return Ok(JsValue::Null);
    }
    let (start, _, _) = cx.state.dom.borrow().selection(this);
    Ok(JsValue::Number(start as f64))
}

pub(crate) fn selection_end(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    if !text_entry(cx, this) {
        return Ok(JsValue::Null);
    }
    let (_, end, _) = cx.state.dom.borrow().selection(this);
    Ok(JsValue::Number(end as f64))
}

pub(crate) fn selection_direction(
    cx: &BindCx<'_>,
    this: NodeId,
) -> Result<Option<String>, JsThrow> {
    if !text_entry(cx, this) {
        return Ok(None);
    }
    let (_, _, direction) = cx.state.dom.borrow().selection(this);
    Ok(Some(direction.as_str().to_owned()))
}

/// Setting `selectionStart` past `selectionEnd` drags the end along with it,
/// per HTML — the selection can never be inverted.
pub(crate) fn set_selection_start(
    cx: &BindCx<'_>,
    this: NodeId,
    value: JsValue,
) -> Result<(), JsThrow> {
    if !text_entry(cx, this) {
        return Ok(());
    }
    let (_, end, direction) = cx.state.dom.borrow().selection(this);
    let start = to_offset(cx, &value);
    cx.state
        .dom
        .borrow_mut()
        .set_selection(this, start, end.max(start), direction);
    Ok(())
}

pub(crate) fn set_selection_end(
    cx: &BindCx<'_>,
    this: NodeId,
    value: JsValue,
) -> Result<(), JsThrow> {
    if !text_entry(cx, this) {
        return Ok(());
    }
    let (start, _, direction) = cx.state.dom.borrow().selection(this);
    let end = to_offset(cx, &value);
    cx.state
        .dom
        .borrow_mut()
        .set_selection(this, start, end, direction);
    Ok(())
}

pub(crate) fn set_selection_direction(
    cx: &BindCx<'_>,
    this: NodeId,
    value: Option<String>,
) -> Result<(), JsThrow> {
    if !text_entry(cx, this) {
        return Ok(());
    }
    let (start, end, _) = cx.state.dom.borrow().selection(this);
    let direction = SelectionDirection::parse(value.as_deref().unwrap_or("none"));
    cx.state
        .dom
        .borrow_mut()
        .set_selection(this, start, end, direction);
    Ok(())
}

pub(crate) fn set_selection_range(
    cx: &BindCx<'_>,
    this: NodeId,
    start: u32,
    end: u32,
    direction: Option<String>,
) -> Result<(), JsThrow> {
    if !text_entry(cx, this) {
        return Ok(());
    }
    let direction = SelectionDirection::parse(direction.as_deref().unwrap_or("none"));
    cx.state
        .dom
        .borrow_mut()
        .set_selection(this, start as usize, end as usize, direction);
    Ok(())
}

/// `select()`: the whole value.
pub(crate) fn select(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    if !text_entry(cx, this) {
        return Ok(());
    }
    let len = cx
        .state
        .dom
        .borrow()
        .form_value(this)
        .encode_utf16()
        .count();
    cx.state
        .dom
        .borrow_mut()
        .set_selection(this, 0, len, SelectionDirection::None);
    Ok(())
}

/// `maxLength`/`minLength` reflect their content attributes, with `-1` for
/// absent — the value HTML specifies for a missing limited-to-non-negative
/// reflection.
pub(crate) fn max_length(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(length_attr(cx, this, "maxlength"))
}

pub(crate) fn min_length(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(length_attr(cx, this, "minlength"))
}

pub(crate) fn set_max_length(cx: &BindCx<'_>, this: NodeId, value: i32) -> Result<(), JsThrow> {
    set_length_attr(cx, this, "maxlength", value)
}

pub(crate) fn set_min_length(cx: &BindCx<'_>, this: NodeId, value: i32) -> Result<(), JsThrow> {
    set_length_attr(cx, this, "minlength", value)
}

fn length_attr(cx: &BindCx<'_>, this: NodeId, name: &str) -> f64 {
    cx.state
        .dom
        .borrow()
        .get(this)
        .and_then(|n| n.as_element())
        .and_then(|el| el.attr(&crate::imp::reflect::attr(name)))
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&n| n >= 0)
        .map_or(-1.0, |n| n as f64)
}

fn set_length_attr(cx: &BindCx<'_>, this: NodeId, name: &str, value: i32) -> Result<(), JsThrow> {
    // A negative value is an IndexSizeError, per HTML's "limited to only
    // non-negative numbers" reflection.
    if value < 0 {
        return Err(cx.dom_throw(
            oxidepage_base::DomExceptionKind::IndexSizeError,
            "maxLength/minLength must not be negative",
        ));
    }
    crate::imp::reflect::set_string(cx, this, name, value.to_string());
    Ok(())
}

/// An `unsigned long` offset from an arbitrary JS value. `null` is 0 — HTML
/// defines these setters as nullable-to-zero rather than as an error.
fn to_offset(cx: &BindCx<'_>, value: &JsValue) -> usize {
    if value.is_nullish() {
        return 0;
    }
    let n = cx.scope.coerce_number(value).unwrap_or(0.0);
    if !n.is_finite() || n < 0.0 {
        0
    } else {
        n as usize
    }
}
