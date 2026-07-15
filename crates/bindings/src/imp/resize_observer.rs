//! `ResizeObserver`: constructor, `observe`/`unobserve`/`disconnect`.

use std::cell::Cell;
use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::{ResizeObserverData, RoBoxKind, RoTarget};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    callback: JsValue,
) -> Result<JsValue, JsThrow> {
    if !cx.scope.is_function(&callback) {
        return Err(JsThrow::Type(
            "ResizeObserver constructor requires a callback function".into(),
        ));
    }
    cx.new_resize_observer(callback)
}

/// Reads the `box` observation option (defaults to `content-box`).
fn box_kind(cx: &BindCx<'_>, options: &JsValue) -> RoBoxKind {
    if let JsValue::Object(obj) = options
        && let Ok(value) = cx.scope.get(obj, "box")
        && let Some(s) = value.as_str()
    {
        match s {
            "border-box" => return RoBoxKind::BorderBox,
            "device-pixel-content-box" => return RoBoxKind::DevicePixelContentBox,
            _ => {}
        }
    }
    RoBoxKind::ContentBox
}

pub(crate) fn observe(
    cx: &BindCx<'_>,
    this: Rc<ResizeObserverData>,
    target: NodeId,
    options: JsValue,
) -> Result<(), JsThrow> {
    let kind = box_kind(cx, &options);
    let mut targets = this.targets.borrow_mut();
    // Re-observing an element replaces its previous observation (box option).
    targets.retain(|t| t.node != target);
    targets.push(RoTarget {
        node: target,
        box_kind: kind,
        last: Cell::new(None),
        initial_pending: Cell::new(true),
    });
    // Force one delivery pass even if the layout gate is unchanged.
    cx.state.obs_dirty.set(true);
    Ok(())
}

pub(crate) fn unobserve(
    _cx: &BindCx<'_>,
    this: Rc<ResizeObserverData>,
    target: NodeId,
) -> Result<(), JsThrow> {
    this.targets.borrow_mut().retain(|t| t.node != target);
    Ok(())
}

pub(crate) fn disconnect(_cx: &BindCx<'_>, this: Rc<ResizeObserverData>) -> Result<(), JsThrow> {
    this.targets.borrow_mut().clear();
    Ok(())
}
