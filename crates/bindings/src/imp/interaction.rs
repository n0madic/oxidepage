//! `HTMLElement.click()` / `focus()` / `blur()` and `document.activeElement`.
//!
//! These are the synthetic half of user interaction: headless, nobody clicks or
//! tabs, so script calling `el.click()` *is* the activation. What matters is
//! that the observable consequences match a real one — the event fires with the
//! right shape, and a checkbox that is clicked ends up checked (and fires
//! `input`/`change`), unless a listener calls `preventDefault()`.
//!
//! Focus is a single element on the DOM tree (`document.activeElement`), and
//! moving it fires the four-event sequence browsers fire. The `:focus` and
//! `:focus-within` element states follow from the DOM, not from here.

use oxidepage_base::NodeId;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::events::{EventData, EventTargetKey, dispatch_event};

/// Dispatches one trusted event at `node` and reports whether it went
/// un-cancelled (i.e. whether the default action should still run).
fn fire(
    cx: &BindCx<'_>,
    node: NodeId,
    event_type: &str,
    bubbles: bool,
    cancelable: bool,
) -> Result<bool, JsThrow> {
    let mut data = EventData::new(event_type.to_owned(), bubbles, cancelable, false);
    data.is_trusted = true;
    let (value, data) = cx.new_event_object("Event", data)?;
    dispatch_event(cx, EventTargetKey::Node(node), &value, &data)
}

/// `HTMLElement.click()`: run the element's **legacy-pre-activation behavior**,
/// fire a cancellable `click`, then either run the activation behavior or, if a
/// listener cancelled, undo the pre-activation.
///
/// The order is the whole point, and it is the order DOM §2.9 dispatches in: the
/// checkbox is toggled **before** the `click` event propagates, so a `click`
/// listener reads the *new* checkedness. React depends on exactly this — its
/// `onChange` for a checkbox or radio is synthesised from the native `click`
/// event, and it decides whether anything changed by comparing `node.checked`
/// against the value it recorded at mount. Toggling after the dispatch left that
/// comparison equal, so `onChange` never fired at all.
///
/// The only activation behaviors observable in a headless engine are the form
/// controls': a checkbox toggles, a radio becomes checked. Both then fire `input`
/// and `change`, in that order, as a real click does.
pub(crate) fn click(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    // HTML `click()` step 1: a disabled form control's click() does nothing —
    // not even fire the event.
    if cx.state.dom.borrow().is_actually_disabled(this) {
        return Ok(());
    }
    // Speculative until the dispatch comes back un-cancelled.
    let activation = cx.state.dom.borrow_mut().legacy_pre_activation(this);

    let proceed = fire(
        cx, this, "click", /* bubbles */ true, /* cancelable */ true,
    )?;

    if let Some(a) = activation {
        if proceed {
            fire(cx, this, "input", true, false)?;
            fire(cx, this, "change", true, false)?;
        } else {
            cx.state.dom.borrow_mut().legacy_canceled_activation(a);
        }
    }
    crate::microtask_checkpoint(cx);
    Ok(())
}

/// `HTMLElement.focus()`. Fires `blur`/`focusout` at the old element and
/// `focus`/`focusin` at the new one; the non-composed pair does not bubble, the
/// `focus{in,out}` pair does. jQuery delegates focus handling through
/// `focusin`, so both halves matter.
pub(crate) fn focus(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    // A disabled control cannot take focus.
    if cx.state.dom.borrow().is_actually_disabled(this) {
        return Ok(());
    }
    move_focus(cx, Some(this))
}

/// `HTMLElement.blur()`: only the currently focused element can be blurred.
pub(crate) fn blur(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    if cx.state.dom.borrow().focused() != Some(this) {
        return Ok(());
    }
    move_focus(cx, None)
}

fn move_focus(cx: &BindCx<'_>, to: Option<NodeId>) -> Result<(), JsThrow> {
    // The DOM performs the move and reports which elements changed, updating
    // `:focus`/`:focus-within` on both ancestor chains. The borrow is released
    // before any event fires — listeners will re-enter the DOM.
    let (blurred, focused) = cx.state.dom.borrow_mut().set_focused(to);
    if let Some(old) = blurred {
        fire(cx, old, "blur", false, false)?;
        fire(cx, old, "focusout", true, false)?;
    }
    if let Some(new) = focused {
        fire(cx, new, "focus", false, false)?;
        fire(cx, new, "focusin", true, false)?;
    }
    crate::microtask_checkpoint(cx);
    Ok(())
}

/// `document.activeElement`: the focused element, or `<body>` when nothing has
/// focus — the fallback every browser reports, and what jQuery's
/// `safeActiveElement()` expects.
pub(crate) fn active_element(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    // Only the rendered document has a focus ring; an inert `DOMParser` /
    // `createHTMLDocument` document reports null (ADR-0017).
    let focused = {
        let dom = cx.state.dom.borrow();
        if this != dom.document() {
            return Ok(None);
        }
        dom.focused()
    };
    // The borrow must be released: `html_child_of_root` takes its own.
    Ok(focused.or_else(|| super::document::html_child_of_root(cx, this, &["body", "frameset"])))
}
