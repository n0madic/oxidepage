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
use oxidepage_dom::DomTree;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::events::{EventData, EventTargetKey, dispatch_event};
use crate::state::PendingNavigation;

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

/// What an element does when activated, once the `click` event has come back
/// un-cancelled. `None` from [`activation_target`] means "nothing does".
enum Activation {
    /// A checkbox or radio, whose *pre*-activation already ran.
    Checkable,
    /// `<a href>`, `<area href>`: navigate to the resolved href.
    Hyperlink,
    /// `<button type=submit>`, `<input type=submit>`.
    Submit { form: NodeId },
    /// `<button type=reset>`, `<input type=reset>`.
    Reset { form: NodeId },
    /// `<label>`. `control` is `None` when the label has no labeled control, or
    /// when the click started on interactive content inside it — in both cases
    /// the behavior is to do **nothing**.
    ///
    /// A no-op behavior is not the same as no behavior, and the difference is
    /// the whole point of having this variant: a `<label>` still *shadows* an
    /// ancestor that would otherwise be activated, because the activation
    /// target is the innermost element that has a behavior at all.
    Label { control: Option<NodeId> },
}

/// The DOM spec's **activation target**: the nearest inclusive ancestor with an
/// activation behavior.
///
/// It walks `flat_tree_parent`, not `parent`, so a click on a `<span>` inside an
/// `<a>` activates the link — and a click inside a shadow tree resolves out
/// through its host, which is the whole reason the flat tree is the one
/// authoritative tree.
fn activation_target(dom: &DomTree, node: NodeId) -> Option<(NodeId, Activation)> {
    let mut current = Some(node);
    while let Some(id) = current {
        if let Some(behavior) = activation_of(dom, id, node) {
            return Some((id, behavior));
        }
        current = dom.flat_tree_parent(id);
    }
    None
}

/// The activation behavior of one element, if it has one. `clicked` is the node
/// the click started at, which only `<label>` needs (its behavior depends on
/// whether the click landed on interactive content inside it).
fn activation_of(dom: &DomTree, id: NodeId, clicked: NodeId) -> Option<Activation> {
    let el = dom.get(id)?.as_element()?;
    if !el.is_html_element() {
        return None;
    }
    // A disabled control has no activation behavior at all — it is not that its
    // behavior is suppressed, so the walk continues past it to an ancestor.
    if dom.is_actually_disabled(id) {
        return None;
    }
    let local = el.local_name();
    match &**local {
        "a" | "area" => el
            .attr(&oxidepage_dom::node::attr_name("href".into()))
            .is_some()
            .then_some(Activation::Hyperlink),
        "button" => {
            let form = dom.form_owner(id)?;
            match button_type(el) {
                "reset" => Some(Activation::Reset { form }),
                "button" => None,
                _ => Some(Activation::Submit { form }),
            }
        }
        "input" => match oxidepage_dom::input_type(el) {
            "checkbox" | "radio" => Some(Activation::Checkable),
            "submit" => dom.form_owner(id).map(|form| Activation::Submit { form }),
            "reset" => dom.form_owner(id).map(|form| Activation::Reset { form }),
            _ => None,
        },
        "label" => Some(Activation::Label {
            control: label_forward_target(dom, id, clicked),
        }),
        _ => None,
    }
}

/// Which control a `<label>` forwards its activation to, or `None` for "do
/// nothing".
///
/// A label does nothing when the click started on **interactive content**
/// inside it — clicking the checkbox itself must toggle it once, not twice —
/// and nothing when it labels nothing or labels the clicked element itself.
fn label_forward_target(dom: &DomTree, label: NodeId, clicked: NodeId) -> Option<NodeId> {
    let control = dom.label_control(label)?;
    if control == clicked {
        return None;
    }
    // "Interactive content" between the click and the label: its own activation
    // (or lack of one) is what the click means, so the label stays out of it.
    let mut current = Some(clicked);
    while let Some(id) = current {
        if id == label {
            break;
        }
        if is_interactive_content(dom, id) {
            return None;
        }
        current = dom.flat_tree_parent(id);
    }
    Some(control)
}

/// HTML's **interactive content** category, restricted to the elements this
/// engine can act on. `<label>` itself is interactive content, but a nested
/// label is resolved by the activation-target walk, not here.
fn is_interactive_content(dom: &DomTree, id: NodeId) -> bool {
    let Some(el) = dom.get(id).and_then(|n| n.as_element()) else {
        return false;
    };
    if !el.is_html_element() {
        return false;
    }
    match &**el.local_name() {
        "a" | "area" => el
            .attr(&oxidepage_dom::node::attr_name("href".into()))
            .is_some(),
        "button" | "details" | "embed" | "iframe" | "select" | "textarea" => true,
        "input" => oxidepage_dom::input_type(el) != "hidden",
        _ => false,
    }
}

/// `button.type`'s missing-value default is `submit`; only `reset` and `button`
/// are the other two keywords.
fn button_type(el: &oxidepage_dom::ElementData) -> &'static str {
    let raw = el
        .attr(&oxidepage_dom::node::attr_name("type".into()))
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    match raw.as_str() {
        "reset" => "reset",
        "button" => "button",
        _ => "submit",
    }
}

/// `HTMLElement.click()`: run the element's **legacy-pre-activation behavior**,
/// fire a cancellable `click`, then either run the activation behavior or, if a
/// listener cancelled, undo the pre-activation.
///
/// Two nodes are in play and they are not the same one. The event is dispatched
/// **at the clicked node**; the activation behavior belongs to the
/// [`activation_target`], the nearest ancestor that has one. Clicking a `<span>`
/// inside an `<a>` fires `click` at the span and follows the link.
///
/// The order of the checkable pre-activation is the whole point, and it is the
/// order DOM §2.9 dispatches in: the checkbox is toggled **before** the `click`
/// event propagates, so a `click` listener reads the *new* checkedness. React
/// depends on exactly this — its `onChange` for a checkbox or radio is
/// synthesised from the native `click` event, and it decides whether anything
/// changed by comparing `node.checked` against the value it recorded at mount.
/// Toggling after the dispatch left that comparison equal, so `onChange` never
/// fired at all.
///
/// Activation is wired to `click()` and nothing else. `dispatchEvent(new
/// Event("click"))` still does not activate, which is correct: the spec's
/// activation trigger is a `MouseEvent`, and that interface does not exist yet.
pub(crate) fn click(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    // HTML `click()` step 1: a disabled form control's click() does nothing —
    // not even fire the event.
    if cx.state.dom.borrow().is_actually_disabled(this) {
        return Ok(());
    }
    let target = {
        let dom = cx.state.dom.borrow();
        activation_target(&dom, this)
    };
    // Speculative until the dispatch comes back un-cancelled.
    let pre = match target {
        Some((node, Activation::Checkable)) => {
            cx.state.dom.borrow_mut().legacy_pre_activation(node)
        }
        _ => None,
    };

    let proceed = fire(
        cx, this, "click", /* bubbles */ true, /* cancelable */ true,
    )?;

    if !proceed {
        if let Some(a) = pre {
            cx.state.dom.borrow_mut().legacy_canceled_activation(a);
        }
        crate::microtask_checkpoint(cx);
        return Ok(());
    }
    if let Some((node, behavior)) = target {
        run_activation(cx, node, behavior, pre.is_some())?;
    }
    crate::microtask_checkpoint(cx);
    Ok(())
}

/// Runs one activation behavior. `toggled` reports whether the checkable
/// pre-activation actually changed anything — an already-checked radio yields
/// no `input`/`change`, exactly as a real click on it fires nothing.
fn run_activation(
    cx: &BindCx<'_>,
    node: NodeId,
    behavior: Activation,
    toggled: bool,
) -> Result<(), JsThrow> {
    match behavior {
        Activation::Checkable => {
            if toggled {
                fire(cx, node, "input", true, false)?;
                fire(cx, node, "change", true, false)?;
            }
        }
        Activation::Hyperlink => follow_hyperlink(cx, node),
        Activation::Submit { form } => {
            crate::imp::form_submit::submit(cx, form, Some(node), /* fire_event */ true)?;
        }
        Activation::Reset { form } => {
            crate::imp::form_submit::reset(cx, form)?;
        }
        // "Run synthetic click activation steps on the labeled control": a
        // fresh `click()`, which is why this recurses through the public entry
        // point rather than duplicating the pre-activation dance.
        Activation::Label { control } => {
            if let Some(control) = control {
                click(cx, control)?;
            }
        }
    }
    Ok(())
}

/// HTML's **"follow the hyperlink"**, trimmed to what a single-context headless
/// engine can honestly do.
///
/// The v1 limits are warned about rather than silently ignored, because each of
/// them changes what the page does: `download` suppresses the navigation, a
/// `javascript:` URL runs script, and `target` opens a second browsing context
/// — none of which exist here.
fn follow_hyperlink(cx: &BindCx<'_>, node: NodeId) {
    let (download, target) = {
        let dom = cx.state.dom.borrow();
        let Some(el) = dom.get(node).and_then(|n| n.as_element()) else {
            return;
        };
        let attr = |name: &str| {
            el.attr(&oxidepage_dom::node::attr_name(name.into()))
                .map(ToString::to_string)
        };
        (attr("download").is_some(), attr("target"))
    };
    if download {
        cx.warn("link activation: `download` is not implemented; the navigation was skipped");
        return;
    }
    // `reflect_url` resolves the `href` attribute against the base URL and
    // yields `""` when it is absent or will not parse — the spec's "if url is
    // failure, then return".
    let resolved = crate::imp::reflect::reflect_url(cx, node, "href");
    if resolved.is_empty() {
        return;
    }
    if resolved.starts_with("javascript:") {
        cx.warn("link activation: `javascript:` URLs are not implemented");
        return;
    }
    if let Some(target) = target.filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("_self")) {
        cx.warn(&format!(
            "link activation: target=`{target}` is not implemented (one browsing context); \
             navigating in place"
        ));
    }
    cx.state.request_navigation(PendingNavigation::Load {
        url: resolved,
        replace: false,
        body: None,
        reload: false,
    });
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
