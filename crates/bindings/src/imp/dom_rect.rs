//! `DOMRect` / `DOMRectReadOnly` implementation.
//!
//! Both interfaces share this backing: `DOMRectReadOnly` re-exports the getters
//! and `toJSON` from here (see `dom_rect_read_only`), and only their
//! constructors differ (which interface prototype the wrapper carries). The
//! four derived edges (`top`/`right`/`bottom`/`left`) are NaN-propagating per
//! the Geometry spec, so they cannot use `f64::min`/`max` (which drop NaN).

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::RectData;

pub(crate) type RectRef = Rc<RefCell<RectData>>;

/// NaN-propagating minimum (`f64::min` returns the non-NaN operand).
fn min_edge(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a < b {
        a
    } else {
        b
    }
}

/// NaN-propagating maximum (`f64::max` returns the non-NaN operand).
fn max_edge(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else {
        b
    }
}

fn edge_top(r: &RectData) -> f64 {
    min_edge(r.y, r.y + r.height)
}

fn edge_right(r: &RectData) -> f64 {
    max_edge(r.x, r.x + r.width)
}

fn edge_bottom(r: &RectData) -> f64 {
    max_edge(r.y, r.y + r.height)
}

fn edge_left(r: &RectData) -> f64 {
    min_edge(r.x, r.x + r.width)
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<JsValue, JsThrow> {
    cx.new_dom_rect(
        "DOMRect",
        RectData {
            x,
            y,
            width,
            height,
        },
    )
}

/// `X.fromRect(init)`: reads a `DOMRectInit` (each member defaults to 0) and
/// builds an instance of `interface`. Shared by the hand-registered statics.
pub(crate) fn from_rect(
    cx: &BindCx<'_>,
    init: &JsValue,
    interface: &str,
) -> Result<JsValue, JsThrow> {
    let read = |name: &str| -> f64 {
        let JsValue::Object(obj) = init else {
            return 0.0;
        };
        match cx.scope.get(obj, name) {
            Ok(v) if !v.is_undefined() => cx.scope.coerce_number(&v).unwrap_or(0.0),
            _ => 0.0,
        }
    };
    cx.new_dom_rect(
        interface,
        RectData {
            x: read("x"),
            y: read("y"),
            width: read("width"),
            height: read("height"),
        },
    )
}

pub(crate) fn x(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(this.borrow().x)
}

pub(crate) fn y(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(this.borrow().y)
}

pub(crate) fn width(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(this.borrow().width)
}

pub(crate) fn height(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(this.borrow().height)
}

pub(crate) fn top(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(edge_top(&this.borrow()))
}

pub(crate) fn right(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(edge_right(&this.borrow()))
}

pub(crate) fn bottom(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(edge_bottom(&this.borrow()))
}

pub(crate) fn left(_cx: &BindCx<'_>, this: RectRef) -> Result<f64, JsThrow> {
    Ok(edge_left(&this.borrow()))
}

pub(crate) fn set_x(_cx: &BindCx<'_>, this: RectRef, value: f64) -> Result<(), JsThrow> {
    this.borrow_mut().x = value;
    Ok(())
}

pub(crate) fn set_y(_cx: &BindCx<'_>, this: RectRef, value: f64) -> Result<(), JsThrow> {
    this.borrow_mut().y = value;
    Ok(())
}

pub(crate) fn set_width(_cx: &BindCx<'_>, this: RectRef, value: f64) -> Result<(), JsThrow> {
    this.borrow_mut().width = value;
    Ok(())
}

pub(crate) fn set_height(_cx: &BindCx<'_>, this: RectRef, value: f64) -> Result<(), JsThrow> {
    this.borrow_mut().height = value;
    Ok(())
}

/// `toJSON()`: a plain object with all eight members (spec `[Default] object`).
pub(crate) fn to_json(cx: &BindCx<'_>, this: RectRef) -> Result<JsValue, JsThrow> {
    let r = *this.borrow();
    let obj = cx.scope.new_object().map_err(JsThrow::from)?;
    let members = [
        ("x", r.x),
        ("y", r.y),
        ("width", r.width),
        ("height", r.height),
        ("top", edge_top(&r)),
        ("right", edge_right(&r)),
        ("bottom", edge_bottom(&r)),
        ("left", edge_left(&r)),
    ];
    for (name, value) in members {
        cx.scope
            .set(&obj, name, &JsValue::Number(value))
            .map_err(JsThrow::from)?;
    }
    Ok(JsValue::Object(obj))
}
