//! `MediaQueryList` state, handler property, and legacy listener aliases.

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::EventTargetKey;
use crate::imp::event_target;
use crate::state::MediaQueryListData;

fn target(this: &MediaQueryListData) -> Result<EventTargetKey, JsThrow> {
    this.key
        .get()
        .map(EventTargetKey::MediaQueryList)
        .ok_or_else(|| JsThrow::Type("MediaQueryList is not installed".into()))
}

pub(crate) fn media(_cx: &BindCx<'_>, this: Rc<MediaQueryListData>) -> Result<String, JsThrow> {
    Ok(this.media.clone())
}

pub(crate) fn matches(_cx: &BindCx<'_>, this: Rc<MediaQueryListData>) -> Result<bool, JsThrow> {
    Ok(this.matches.get())
}

pub(crate) fn onchange(cx: &BindCx<'_>, this: Rc<MediaQueryListData>) -> Result<JsValue, JsThrow> {
    Ok(cx
        .state
        .event_handlers
        .borrow()
        .get(&(target(&this)?, "change".to_owned()))
        .cloned()
        .unwrap_or(JsValue::Null))
}

pub(crate) fn set_onchange(
    cx: &BindCx<'_>,
    this: Rc<MediaQueryListData>,
    value: JsValue,
) -> Result<(), JsThrow> {
    let key = (target(&this)?, "change".to_owned());
    if cx.scope.is_function(&value) {
        cx.state.event_handlers.borrow_mut().insert(key, value);
    } else {
        cx.state.event_handlers.borrow_mut().remove(&key);
    }
    Ok(())
}

pub(crate) fn add_listener(
    cx: &BindCx<'_>,
    this: Rc<MediaQueryListData>,
    callback: JsValue,
) -> Result<(), JsThrow> {
    event_target::add_event_listener(
        cx,
        target(&this)?,
        "change".to_owned(),
        callback,
        JsValue::Bool(false),
    )
}

pub(crate) fn remove_listener(
    cx: &BindCx<'_>,
    this: Rc<MediaQueryListData>,
    callback: JsValue,
) -> Result<(), JsThrow> {
    event_target::remove_event_listener(
        cx,
        target(&this)?,
        "change".to_owned(),
        callback,
        JsValue::Bool(false),
    )
}
