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
use crate::window_open::OpenWindowRequest;

/// [`fire`], reachable from outside this crate — for an embedder command that
/// owes a page an event.
///
/// The `input`/`change` pair `DOM.setFileInputFiles` fires (ADR-0032 D11) sits
/// on the same trust boundary ADR-0023 drew for synthetic input: an event the
/// *driver* caused is trusted, one page script caused is not.
pub fn fire_trusted_event(
    cx: &BindCx<'_>,
    node: NodeId,
    event_type: &str,
    bubbles: bool,
    cancelable: bool,
) -> Result<bool, JsThrow> {
    fire(cx, node, event_type, bubbles, cancelable)
}

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
    let data = cx.new_event_data("Event", data);
    dispatch_event(cx, EventTargetKey::Node(node), &data)
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
    /// `<input type=file>`: show a file chooser (ADR-0032 D12).
    FileChooser,
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
/// `bubbles` is the dispatched event's flag, and it gates the *ancestor* walk
/// only: DOM's dispatch sets the activation target from an ancestor solely
/// "if event's bubbles is true". A non-bubbling `click` at a text node inside a
/// checkbox therefore activates nothing, while a bubbling one toggles the box.
fn activation_target(dom: &DomTree, node: NodeId, bubbles: bool) -> Option<(NodeId, Activation)> {
    if let Some(behavior) = activation_of(dom, node, node) {
        return Some((node, behavior));
    }
    if !bubbles {
        return None;
    }
    let mut current = dom.flat_tree_parent(node);
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
    let local = el.local_name();
    let behavior = activation_kind(dom, el, id, clicked, local);
    // A disabled control has no activation behavior — so the walk continues
    // past it to an ancestor, rather than finding a suppressed one.
    //
    // A checkbox or radio is the exception, and it is a real one: HTML's input
    // activation behavior returns early only when the element is *neither* a
    // checkbox nor a radio, so the legacy pre-activation toggles a disabled one
    // and `preventDefault()` still undoes it. `Event-dispatch-click.html`
    // asserts exactly that, four times.
    match behavior {
        Some(Activation::Checkable) => behavior,
        Some(_) if dom.is_actually_disabled(id) => None,
        other => other,
    }
}

fn activation_kind(
    dom: &DomTree,
    el: &oxidepage_dom::node::ElementData,
    id: NodeId,
    clicked: NodeId,
    local: &str,
) -> Option<Activation> {
    match local {
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
            "file" => Some(Activation::FileChooser),
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
/// `click()` dispatches an untrusted-shaped plain `Event`; a synthesized mouse
/// click (`imp::input_synth`) dispatches a real `MouseEvent`. Both run the same
/// activation through [`activate_around`], which is the point: a second path
/// is how `<label>`, submit and hyperlink activation drift apart.
pub(crate) fn click(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    // HTML `click()` step 1: a disabled form control's click() does nothing —
    // not even fire the event.
    if cx.state.dom.borrow().is_actually_disabled(this) {
        return Ok(());
    }
    activate_around(cx, this, |cx| {
        fire(
            cx, this, "click", /* bubbles */ true, /* cancelable */ true,
        )
    })?;
    crate::microtask_checkpoint(cx);
    Ok(())
}

/// Runs `dispatch` — which must dispatch the `click` event at `node` and report
/// whether it went un-cancelled — wrapped in the activation protocol: resolve
/// the activation target, run legacy pre-activation, and afterwards either run
/// the activation behavior or undo the pre-activation.
///
/// The wrapping is what makes the order right, and the order is the whole
/// point: the checkbox is toggled **before** the `click` event propagates
/// (DOM §2.9), so a `click` listener reads the *new* checkedness. React depends
/// on exactly this — its `onChange` for a checkbox or radio is synthesised from
/// the native `click` event, and it decides whether anything changed by
/// comparing `node.checked` against the value it recorded at mount. Toggling
/// after the dispatch left that comparison equal, so `onChange` never fired.
pub(crate) fn activate_around(
    cx: &BindCx<'_>,
    node: NodeId,
    dispatch: impl FnOnce(&BindCx<'_>) -> Result<bool, JsThrow>,
) -> Result<(), JsThrow> {
    // `click()` fires a bubbling event, so ancestors are eligible.
    let state = begin_activation(cx, node, /* bubbles */ true);
    let proceed = dispatch(cx)?;
    finish_activation(cx, state, proceed)
}

/// The activation state resolved before a `click` dispatch: the target that
/// owns the behavior, and whatever the legacy pre-activation speculatively
/// changed. Held across the dispatch by [`activate_around`] and by
/// [`crate::events::dispatch_event`].
pub(crate) struct ActivationState {
    target: Option<(NodeId, Activation)>,
    pre: Option<oxidepage_dom::ClickActivation>,
}

/// DOM dispatch step 5: resolve the activation target and run the legacy
/// pre-activation behavior. Speculative — [`finish_activation`] either commits
/// or undoes it once the dispatch reports whether a listener cancelled.
pub(crate) fn begin_activation(cx: &BindCx<'_>, node: NodeId, bubbles: bool) -> ActivationState {
    let target = {
        let dom = cx.state.dom.borrow();
        activation_target(&dom, node, bubbles)
    };
    let pre = match target {
        Some((node, Activation::Checkable)) => {
            cx.state.dom.borrow_mut().legacy_pre_activation(node)
        }
        _ => None,
    };
    ActivationState { target, pre }
}

/// The other half of [`begin_activation`].
pub(crate) fn finish_activation(
    cx: &BindCx<'_>,
    state: ActivationState,
    proceed: bool,
) -> Result<(), JsThrow> {
    let ActivationState { target, pre } = state;
    if !proceed {
        if let Some(a) = pre {
            cx.state.dom.borrow_mut().legacy_canceled_activation(a);
        }
        return Ok(());
    }
    if let Some((node, behavior)) = target {
        run_activation(cx, node, behavior, pre.is_some())?;
    }
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
    // The activation target was resolved *before* the dispatch (that is when
    // the spec picks it), so a listener has had a chance to disable it since.
    // Re-check here: `Event-dispatch-click.html` disables a submit button from
    // its own click listener and requires the form not to be submitted. The
    // checkable behavior is exempt for the same reason it is exempt above.
    if !matches!(behavior, Activation::Checkable)
        && cx.state.dom.borrow().is_actually_disabled(node)
    {
        return Ok(());
    }
    match behavior {
        Activation::Checkable => {
            if toggled {
                fire(cx, node, "input", true, false)?;
                fire(cx, node, "change", true, false)?;
            }
        }
        Activation::Hyperlink => follow_hyperlink(cx, node),
        // Unlike a modal dialog, this does **not** park the page: a chooser has
        // no return value the activation needs, so the click completes and the
        // driver answers later with `DOM.setFileInputFiles` (ADR-0032 D12).
        // With no interception installed the hook records the event and does
        // nothing, which is the honest headless answer — the shape ADR-0025
        // chose for `alert`.
        Activation::FileChooser => {
            let multiple = cx
                .state
                .dom
                .borrow()
                .node(node)
                .as_element()
                .is_some_and(|el| {
                    el.attr(&oxidepage_dom::node::attr_name("multiple".into()))
                        .is_some()
                });
            cx.state.hooks.open_file_chooser(node, multiple);
        }
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
/// them changes what the page does: a `javascript:` URL runs script, and
/// `target` opens a second browsing context — neither of which exists here.
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
        (attr("download"), attr("target"))
    };
    // `reflect_url` resolves the `href` attribute against the base URL and
    // yields `""` when it is absent or will not parse — the spec's "if url is
    // failure, then return".
    let resolved = crate::imp::reflect::reflect_url(cx, node, "href");
    if resolved.is_empty() {
        return;
    }
    // `<a download>` makes the response a download regardless of what the
    // server said, which is HTML's rule and Chrome's behaviour (ADR-0032 D13).
    //
    // Deferring to `Content-Disposition` alone was the earlier reading, and it
    // is wrong in the case the attribute exists for: a static file server sends
    // `application/pdf` with no disposition header at all, so `<a download>`
    // committed the response *as a document* and the current page was replaced
    // by a PDF parsed as HTML. Honouring the attribute is not the page deciding
    // the operator's policy either — a download still only reaches disk if the
    // operator set a download directory, and `DownloadBehavior` denies by
    // default.
    //
    // Same-origin only, as in Chrome: a cross-origin `download` is ignored and
    // the link navigates, so a page cannot use the attribute to make another
    // site's response land on disk under a filename of its choosing.
    let download = download.filter(|_| {
        let current = cx.state.dom.borrow().document_url().to_owned();
        if crate::imp::history::same_origin(&resolved, &current) {
            return true;
        }
        cx.warn(&format!(
            "link activation: `download` on the cross-origin `{resolved}` is ignored; \
             navigating instead"
        ));
        false
    });
    // HTML "navigate to a javascript: URL": the payload is evaluated as a
    // classic script, and only a *string* result replaces the document —
    // `javascript:void 0` and every `javascript:doSomething()` handler return
    // undefined and must leave the page alone.
    if let Some(encoded) = resolved.strip_prefix("javascript:") {
        cx.state
            .request_navigation(PendingNavigation::JavaScriptUrl {
                source: percent_decode(encoded),
            });
        return;
    }
    // `_self`, `_parent`, `_top` and a name all resolve to a real browsing
    // context now (ADR-0035 D10) — the navigation is queued on *that* context,
    // which is what makes a `target="side"` link drive the frame called `side`.
    let navigate = PendingNavigation::Load {
        url: resolved.clone(),
        replace: false,
        body: None,
        reload: false,
        download,
    };
    let target = target.unwrap_or_default();
    if let Some(context) = crate::window_open::resolve_target(&cx.state.frame, &target) {
        context.request_navigation(navigate);
        return;
    }
    // A *name* that answers to nothing does **not** open a page. HTML would
    // create a context under that name and reuse it for every later link
    // naming it; there is no registry of page names here, so each click would
    // open one more — unbounded until `max_pages_per_context`, after which the
    // same link would silently start navigating in place instead. Warning and
    // navigating in place is what `form_submit.rs` does for the identical case,
    // and the two callers must not disagree.
    if !target.eq_ignore_ascii_case("_blank") {
        cx.warn(&format!(
            "link activation: target=`{target}` names no browsing context; navigating in place"
        ));
        cx.state.request_navigation(navigate);
        return;
    }
    // `_blank` genuinely asks for a fresh one — same hook, same plain-data
    // contract as `window.open` (ADR-0027 D12). Without a hook at all (a bare
    // `Page`, the CLI) this warns and navigates in place, which is the older
    // behaviour and still the least surprising one for a headless run.
    let opener_url = cx
        .state
        .dom
        .borrow()
        .document_url_of(cx.state.frame.document())
        .to_owned();
    let opened = cx.state.hooks.open_window(OpenWindowRequest {
        url: Some(resolved),
        target: target.clone(),
        features: String::new(),
        opener_url,
    });
    if opened.is_none() {
        cx.warn("link activation: target=`_blank` could not open a page; navigating in place");
        cx.state.request_navigation(navigate);
    }
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

    // A text control fires `change` on **blur**, and only when the value
    // actually differs from the one it had when focus arrived — which is why
    // the snapshot is taken below rather than derived. Getting this wrong is
    // invisible to a unit test that only checks the final value, and breaks
    // every form that validates on `change`.
    //
    // It fires before `blur`, as the spec's "user agent must... fire change,
    // then unfocus" orders it.
    if let Some(old) = blurred {
        let owed = {
            let dom = cx.state.dom.borrow();
            dom.is_text_entry(old)
                .then(|| dom.value_at_focus(old))
                .flatten()
                .is_some_and(|before| before != dom.form_value(old))
        };
        cx.state.dom.borrow_mut().set_value_at_focus(old, None);
        if owed {
            fire(cx, old, "change", true, false)?;
        }
    }
    if let Some(new) = focused {
        let value = {
            let dom = cx.state.dom.borrow();
            dom.is_text_entry(new).then(|| dom.form_value(new))
        };
        if let Some(value) = value {
            cx.state
                .dom
                .borrow_mut()
                .set_value_at_focus(new, Some(value));
            // Focus places a collapsed caret at the end of the value, which is
            // where typing continues from — but only when nothing asked for a
            // selection by name. A driver may select *before* it focuses
            // (Playwright's `fill` runs `select()` then `focus()`), and
            // collapsing there would append its replacement text instead of
            // replacing the value.
            if !cx.state.dom.borrow().selection_explicit(new) {
                let end = cx.state.dom.borrow().form_value(new).encode_utf16().count();
                cx.state.dom.borrow_mut().collapse_selection_to(new, end);
            }
        }
    }

    // Each half names the other as `relatedTarget`: on `blur` that is the
    // element gaining focus, on `focus` the one that lost it. A focus manager
    // reads it to decide whether focus left its subtree at all.
    if let Some(old) = blurred {
        fire_focus(cx, old, "blur", false, focused)?;
        fire_focus(cx, old, "focusout", true, focused)?;
    }
    if let Some(new) = focused {
        fire_focus(cx, new, "focus", false, blurred)?;
        fire_focus(cx, new, "focusin", true, blurred)?;
    }
    crate::microtask_checkpoint(cx);
    Ok(())
}

/// Fires one half of a focus transfer as a real `FocusEvent`.
fn fire_focus(
    cx: &BindCx<'_>,
    target: NodeId,
    event_type: &str,
    bubbles: bool,
    related: Option<NodeId>,
) -> Result<(), JsThrow> {
    let mut data = crate::events::EventData::new(
        event_type.to_owned(),
        bubbles,
        /* cancelable */ false,
        /* composed */ true,
    );
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    // The payload pins the related node for as long as the event lives, and
    // stores no wrapper — see `MouseFields::related`.
    let related = related.map(|id| crate::events::PinnedNode::new(&cx.state.dom, id));
    let mut payload = crate::events::UiPayload::new(crate::events::UiKind::Focus { related });
    payload.has_view = true;
    data.ui = Some(Box::new(payload));
    let data = cx.new_event_data("FocusEvent", data);
    crate::events::dispatch_event(cx, crate::events::EventTargetKey::Node(target), &data)?;
    Ok(())
}

/// Moves focus in response to synthesized input (a click, or Tab). Unlike
/// `focus()` this accepts `None`, which blurs whatever holds focus — clicking
/// on nothing focusable takes focus away from the current element.
pub(crate) fn set_focus_from_input(cx: &BindCx<'_>, to: Option<NodeId>) -> Result<(), JsThrow> {
    move_focus(cx, to)
}

/// `document.activeElement`: the focused element, or `<body>` when nothing has
/// focus — the fallback every browser reports, and what jQuery's
/// `safeActiveElement()` expects.
/// Focus is one page-wide slot, but the answer is **per document** (ADR-0035
/// D8): a document whose descendant frame holds the focus reports the
/// `<iframe>` element embedding it, which is what a browser does and what lets
/// a page tell "focus is somewhere inside that frame" from "focus is nowhere".
pub(crate) fn active_element(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    // Only a *rendered* document has a focus ring; an inert `DOMParser` /
    // `createHTMLDocument` document reports null (ADR-0017).
    let focused = {
        let dom = cx.state.dom.borrow();
        if !dom.is_rendered_root(this) {
            return Ok(None);
        }
        dom.focused().and_then(|focused| {
            // Walk out of the focused node's document until one of them is
            // `this`; the element to report is whatever we were standing on.
            let mut node = focused;
            loop {
                match dom.containing_document(node) {
                    Some(doc) if doc == this => return Some(node),
                    Some(doc) => node = dom.owner_of_content_document(doc)?,
                    None => return None,
                }
            }
        })
    };
    // The borrow must be released: `html_child_of_root` takes its own.
    Ok(focused.or_else(|| super::document::html_child_of_root(cx, this, &["body", "frameset"])))
}

/// Percent-decodes a `javascript:` URL payload. `reflect_url` returns the
/// serialized URL, in which the script text has been percent-encoded — running
/// it verbatim would fail on the first `%20`.
fn percent_decode(input: &str) -> String {
    percent_encoding::percent_decode_str(input)
        .decode_utf8_lossy()
        .into_owned()
}
