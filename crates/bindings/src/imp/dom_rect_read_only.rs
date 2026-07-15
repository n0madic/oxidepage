//! `DOMRectReadOnly`: the read-only base of `DOMRect`.
//!
//! All getters and `toJSON` are shared with `dom_rect` (re-exported below);
//! only the constructor differs, building a `DOMRectReadOnly`-proto wrapper.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::RectData;

pub(crate) use super::dom_rect::{bottom, height, left, right, to_json, top, width, x, y};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<JsValue, JsThrow> {
    cx.new_dom_rect(
        "DOMRectReadOnly",
        RectData {
            x,
            y,
            width,
            height,
        },
    )
}
