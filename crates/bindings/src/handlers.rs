//! Event handler *content* attributes (`<body onload="…">`, `<div onclick="…">`).
//!
//! HTML installs an event handler two ways: the IDL attribute (`el.onclick = fn`)
//! and the content attribute, whose value is author source compiled into a
//! function. Both feed the same slot — `PageState::event_handlers`, which
//! dispatch already consults — so this module is only the content-attribute
//! half: it decides when a slot is stale with respect to the attribute, and
//! compiles the source when it is.
//!
//! Compilation is lazy (spec: the handler is an "internal raw uncompiled
//! handler" until first use) and cached against the source it came from:
//! `PageState::handler_attr_seen` records the attribute value each slot
//! currently reflects. A dispatch therefore recompiles only when the attribute
//! actually changed, and that same record is what lets an IDL assignment win
//! over an unchanged content attribute — while a *later* edit of the attribute
//! wins back, as the spec requires.
//!
//! Because the DOM never has to notify anyone, this stays entirely inside
//! `bindings`: parser-set and script-set attributes are picked up by the same
//! lookup, with no queue to drain and no new hook in the tree's mutation path.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::node::attr_name;
use oxidepage_js::JsValue;

use crate::cx::BindCx;
use crate::events::EventTargetKey;

/// Event types that have an event-handler content attribute (`on` + type).
///
/// Generated from the `EventHandler` attributes in the IDL, so the content
/// attribute and the IDL attribute always cover the same set — the two halves of
/// HTML's "install an event handler" cannot drift. Types outside it have no
/// handler: `<div onfoo="…">` is an ordinary attribute, not a handler, and must
/// not be compiled.
const HANDLER_EVENT_TYPES: &[&str] = crate::generated::EVENT_HANDLER_TYPES;

/// Types whose content attribute *on `<body>`/`<frameset>`* installs the
/// **Window's** handler rather than the element's: HTML's "Window-reflecting
/// body element event handler set" (`blur`, `error`, `focus`, `load`, `resize`,
/// `scroll`) plus every `WindowEventHandlers` member.
///
/// This is what makes `<body onload="…">` work: the `load` event is fired at the
/// window ([`crate::events::EventTargetKey::Window`]), never at the body, so a
/// handler filed under the body element would never run.
const WINDOW_REFLECTED: &[&str] = &[
    "afterprint",
    "beforeprint",
    "beforeunload",
    "blur",
    "error",
    "focus",
    "hashchange",
    "languagechange",
    "load",
    "message",
    "messageerror",
    "offline",
    "online",
    "pagehide",
    "pageshow",
    "popstate",
    "rejectionhandled",
    "resize",
    "scroll",
    "storage",
    "unhandledrejection",
    "unload",
];

/// The slot an event-handler *IDL* attribute really addresses.
///
/// Normally the target itself. But `<body>`/`<frameset>` reflect the types in
/// [`WINDOW_REFLECTED`] onto the Window — `document.body.onload = fn` installs
/// the *Window's* `load` handler, because the `load` event is fired at the window
/// and never at the body, so a handler filed under the body would never run.
///
/// Content attributes reach the same conclusion from the other end: the Window's
/// slot resolves its source back to the body (see [`source_element`]). Both paths
/// therefore agree on one slot, and `<body onload>` and `document.body.onload`
/// are the same handler.
pub(crate) fn handler_key(
    cx: &BindCx<'_>,
    key: EventTargetKey,
    event_type: &str,
) -> EventTargetKey {
    let EventTargetKey::Node(id) = key else {
        return key;
    };
    if !WINDOW_REFLECTED.contains(&event_type) {
        return key;
    }
    let is_body = cx
        .state
        .dom
        .borrow()
        .get(id)
        .and_then(|node| node.as_element())
        .is_some_and(|el| el.is_html_element() && matches!(&*el.name.local, "body" | "frameset"));
    if is_body { EventTargetKey::Window } else { key }
}

/// The handler installed for `key`/`event_type`, compiling the content
/// attribute first if the slot no longer reflects it.
///
/// This is the single read path for event handlers: both dispatch and the IDL
/// getter go through it, so a handler set in markup and one assigned from script
/// are indistinguishable afterwards.
pub(crate) fn resolve(cx: &BindCx<'_>, key: EventTargetKey, event_type: &str) -> Option<JsValue> {
    let slot = (key, event_type.to_owned());
    let installed = cx.state.event_handlers.borrow().get(&slot).cloned();
    if !HANDLER_EVENT_TYPES.contains(&event_type) {
        return installed;
    }
    let Some(element) = source_element(cx, key, event_type) else {
        return installed;
    };
    let source = attribute_source(cx, element, event_type);
    let reflected = cx.state.handler_attr_seen.borrow().get(&slot).cloned();
    if source == reflected {
        // The slot is already in sync with the attribute (including "both
        // absent", and an IDL assignment that superseded this exact value).
        return installed;
    }
    install_from_attribute(cx, key, element, event_type, source)
}

/// Records that the slot's handler now reflects the element's *current* content
/// attribute, so [`resolve`] leaves it alone until that attribute changes again.
///
/// Called after an IDL assignment (`el.onclick = fn`): per spec the assignment
/// replaces the handler even though the content attribute is untouched, and only
/// a later edit of the attribute replaces it back.
pub(crate) fn mark_reflects_current_attribute(
    cx: &BindCx<'_>,
    key: EventTargetKey,
    event_type: &str,
) {
    if !HANDLER_EVENT_TYPES.contains(&event_type) {
        return;
    }
    let slot = (key, event_type.to_owned());
    let source = source_element(cx, key, event_type)
        .and_then(|element| attribute_source(cx, element, event_type));
    let mut seen = cx.state.handler_attr_seen.borrow_mut();
    match source {
        Some(src) => seen.insert(slot, src),
        None => seen.remove(&slot),
    };
}

/// Compiles `source` (or clears the slot when the attribute is gone) and records
/// what the slot now reflects.
fn install_from_attribute(
    cx: &BindCx<'_>,
    key: EventTargetKey,
    element: NodeId,
    event_type: &str,
    source: Option<String>,
) -> Option<JsValue> {
    // Compile before taking any borrow: this re-enters JS.
    let compiled = source
        .as_deref()
        .and_then(|src| compile(cx, element, event_type, src));
    let slot = (key, event_type.to_owned());
    {
        let mut handlers = cx.state.event_handlers.borrow_mut();
        match &compiled {
            Some(function) => handlers.insert(slot.clone(), function.clone()),
            // A removed attribute clears the handler — and so does one whose
            // source does not compile: the spec reports the syntax error and
            // leaves the handler null rather than throwing at dispatch.
            None => handlers.remove(&slot),
        };
    }
    let mut seen = cx.state.handler_attr_seen.borrow_mut();
    match source {
        // Recorded even when compilation failed, so a broken handler is not
        // recompiled (and re-reported) on every dispatch.
        Some(src) => seen.insert(slot, src),
        None => seen.remove(&slot),
    };
    compiled
}

/// The element whose content attribute backs this slot: the target itself, or —
/// for a window-reflected type — the `<body>`/`<frameset>` element.
fn source_element(cx: &BindCx<'_>, key: EventTargetKey, event_type: &str) -> Option<NodeId> {
    match key {
        EventTargetKey::Node(id) => {
            let dom = cx.state.dom.borrow();
            dom.get(id)?.as_element().map(|_| id)
        }
        EventTargetKey::Window if WINDOW_REFLECTED.contains(&event_type) => {
            // The Window reflects the *page* document's body, by definition.
            let document = cx.state.dom.borrow().document();
            crate::imp::document::html_child_of_root(cx, document, &["body", "frameset"])
        }
        _ => None,
    }
}

/// The element's `on<type>` attribute value, if present.
fn attribute_source(cx: &BindCx<'_>, element: NodeId, event_type: &str) -> Option<String> {
    let name = attr_name(LocalName::from(format!("on{event_type}")));
    let dom = cx.state.dom.borrow();
    dom.get(element)?
        .as_element()?
        .attr(&name)
        .map(std::string::ToString::to_string)
}

/// Compiles an event-handler content attribute into a callable function.
///
/// The body is evaluated with the spec's scope chain — the element, then the
/// document, then the global — which is what makes `<body onload="checkLayout()">`
/// find a global function and `<button onclick="disabled = true">` find the
/// element's own property. A `with` chain around the *function expression*
/// (rather than around its body) reproduces that chain through the closure's
/// captured environment, and keeps the attribute's source the function's whole
/// body, so a stray `}` in it is the syntax error it is in a browser.
///
/// `this` is bound by the dispatcher, not here: it is the handler's target, so a
/// body handler that reflects onto the window sees the window, as the spec says.
fn compile(cx: &BindCx<'_>, element: NodeId, event_type: &str, source: &str) -> Option<JsValue> {
    let element_js = cx.node_to_js(element).ok()?;
    let document_js = {
        let document = cx.state.dom.borrow().document();
        cx.node_to_js(document).ok()?
    };
    let factory_source = format!(
        "(function (element, document) {{ \
           with (document) with (element) {{ \
             return function (event) {{\n{source}\n}}; \
           }} \
         }})"
    );
    let filename = format!("on{event_type} attribute");
    let factory = match cx.scope.eval(&factory_source, &filename) {
        Ok(factory) => factory,
        Err(error) => {
            // Spec: a handler that does not compile is reported and left null.
            cx.report_callback_error(error);
            return None;
        }
    };
    match cx
        .scope
        .call(&factory, &JsValue::Undefined, &[element_js, document_js])
    {
        Ok(handler) if cx.scope.is_function(&handler) => Some(handler),
        Ok(_) => None,
        Err(error) => {
            cx.report_callback_error(error);
            None
        }
    }
}
