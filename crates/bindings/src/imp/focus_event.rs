//! `FocusEvent`: `focus`/`blur`/`focusin`/`focusout`, whose one extra member is
//! the element focus moved to or from.

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::UiKind;
use crate::imp::event::EventRef;
use crate::imp::ui_event::{member_node, parse_ui_init, payload};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    event_type: String,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let related = member_node(cx, &init, "relatedTarget");
    let data = parse_ui_init(cx, event_type, &init, UiKind::Focus { related })?;
    let (value, _) = cx.new_event_object("FocusEvent", data)?;
    Ok(value)
}

pub(crate) fn related_target(cx: &BindCx<'_>, this: EventRef) -> Result<JsValue, JsThrow> {
    let related = payload(&this, "FocusEvent", |p| match &p.kind {
        UiKind::Focus { related } => Some(*related),
        _ => None,
    })?;
    cx.opt_node_to_js(related)
}
