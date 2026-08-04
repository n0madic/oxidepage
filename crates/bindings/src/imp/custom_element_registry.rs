//! `CustomElementRegistry` (`window.customElements`), autonomous elements only.
//!
//! Definitions and `whenDefined` promises live in
//! [`WorldState::custom_elements`](crate::state::WorldState); the DOM tracks only
//! the set of defined names and a reaction-intent queue. `define` extracts the
//! constructor's `observedAttributes` and lifecycle callbacks, records the
//! definition, tells the DOM to upgrade matching existing elements, and
//! resolves any pending `whenDefined(name)`.

use std::rc::Rc;

use oxidepage_base::DomExceptionKind;
use oxidepage_dom::custom_element::{CustomElementState, is_valid_custom_element_name};
use oxidepage_js::{JsThrow, JsValue};

use crate::customreg::CustomElementDefinition;
use crate::cx::BindCx;

/// Reads a lifecycle callback off a definition's prototype: `Some` only for an
/// actual function.
fn callback(cx: &BindCx<'_>, proto: &oxidepage_js::JsObject, name: &str) -> Option<JsValue> {
    let value = cx.scope.get(proto, name).ok()?;
    if cx.scope.is_function(&value) {
        Some(value)
    } else {
        None
    }
}

/// Reads `constructor.observedAttributes` as a list of strings (array or any
/// array-like with a numeric `length`).
fn read_observed_attributes(cx: &BindCx<'_>, ctor: &oxidepage_js::JsObject) -> Vec<String> {
    let Ok(value) = cx.scope.get(ctor, "observedAttributes") else {
        return Vec::new();
    };
    let Some(array) = value.as_object() else {
        return Vec::new();
    };
    let Ok(len) = cx.scope.array_length(array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Ok(item) = cx.scope.array_get(array, i)
            && let Ok(s) = cx.scope.coerce_string(&item)
        {
            out.push(s);
        }
    }
    out
}

pub(crate) fn define(
    cx: &BindCx<'_>,
    _this: u64,
    name: String,
    constructor: JsValue,
    _options: JsValue,
) -> Result<JsValue, JsThrow> {
    if !is_valid_custom_element_name(&name) {
        return Err(cx.dom_throw(
            DomExceptionKind::SyntaxError,
            &format!("`{name}` is not a valid custom element name"),
        ));
    }
    let Some(ctor_obj) = constructor
        .as_object()
        .filter(|_| cx.scope.is_function(&constructor))
    else {
        return Err(JsThrow::Type("constructor is not a function".into()));
    };

    {
        let reg = cx.state.custom_elements.borrow();
        if reg.by_name(&name).is_some() {
            return Err(cx.dom_throw(
                DomExceptionKind::NotSupportedError,
                &format!("a custom element named `{name}` is already defined"),
            ));
        }
        if reg.by_constructor(cx.scope, &constructor).is_some() {
            return Err(cx.dom_throw(
                DomExceptionKind::NotSupportedError,
                "this constructor is already registered under another name",
            ));
        }
    }

    let prototype = cx.scope.get(ctor_obj, "prototype").map_err(JsThrow::from)?;
    let (connected, disconnected, attribute_changed) = match prototype.as_object() {
        Some(proto) => (
            callback(cx, proto, "connectedCallback"),
            callback(cx, proto, "disconnectedCallback"),
            callback(cx, proto, "attributeChangedCallback"),
        ),
        None => (None, None, None),
    };
    // `observedAttributes` is consulted only when there is an
    // `attributeChangedCallback` (per spec).
    let observed_attributes = if attribute_changed.is_some() {
        read_observed_attributes(cx, ctor_obj)
    } else {
        Vec::new()
    };

    let definition = Rc::new(CustomElementDefinition {
        name: name.clone(),
        constructor: constructor.clone(),
        prototype,
        observed_attributes,
        connected,
        disconnected,
        attribute_changed,
    });
    cx.state
        .custom_elements
        .borrow_mut()
        .definitions
        .push(definition);

    // Tell the DOM the name is defined; it enqueues `Upgrade` intents for
    // matching existing `Undefined` elements in tree order.
    cx.state
        .dom
        .borrow_mut()
        .define_custom_element(name.clone());

    // Resolve a pending `whenDefined(name)`, if any.
    let pending = cx
        .state
        .custom_elements
        .borrow_mut()
        .when_defined
        .remove(&name);
    if let Some((_promise, resolve)) = pending {
        cx.scope
            .call(&resolve, &JsValue::Undefined, &[constructor])
            .map_err(JsThrow::from)?;
    }

    Ok(JsValue::Undefined)
}

pub(crate) fn get(cx: &BindCx<'_>, _this: u64, name: String) -> Result<JsValue, JsThrow> {
    Ok(cx
        .state
        .custom_elements
        .borrow()
        .by_name(&name)
        .map_or(JsValue::Undefined, |d| d.constructor.clone()))
}

pub(crate) fn get_name(
    cx: &BindCx<'_>,
    _this: u64,
    constructor: JsValue,
) -> Result<JsValue, JsThrow> {
    Ok(cx
        .state
        .custom_elements
        .borrow()
        .by_constructor(cx.scope, &constructor)
        .map_or(JsValue::Null, |d| JsValue::String(d.name.clone())))
}

pub(crate) fn when_defined(cx: &BindCx<'_>, _this: u64, name: String) -> Result<JsValue, JsThrow> {
    if !is_valid_custom_element_name(&name) {
        return Err(cx.dom_throw(
            DomExceptionKind::SyntaxError,
            &format!("`{name}` is not a valid custom element name"),
        ));
    }
    // Already defined → a promise resolved with the constructor.
    if let Some(def) = cx.state.custom_elements.borrow().by_name(&name) {
        return cx.resolved_promise(def.constructor.clone());
    }
    // Otherwise return (creating on first request) the pending promise.
    if let Some((promise, _resolve)) = cx.state.custom_elements.borrow().when_defined.get(&name) {
        return Ok(promise.clone());
    }
    let (promise, resolve, _reject) = cx.make_promise()?;
    cx.state
        .custom_elements
        .borrow_mut()
        .when_defined
        .insert(name, (promise.clone(), resolve));
    Ok(promise)
}

pub(crate) fn upgrade(cx: &BindCx<'_>, _this: u64, root: JsValue) -> Result<(), JsThrow> {
    let root_id = cx.this_node(&root)?;
    // Enqueue upgrades for defined, still-undefined elements in the subtree.
    // Delivery is the `[CEReactions]` trampoline's job: it pops the element
    // queue this call opened before returning to script, which is what makes the
    // upgrade synchronous. Draining by hand here would over-drain — the whole
    // FIFO, including the parser's backup entries.
    let mut dom = cx.state.dom.borrow_mut();
    let ids: Vec<_> = dom.inclusive_descendants(root_id).collect();
    for id in ids {
        if dom.custom_state(id) == CustomElementState::Undefined {
            let local = dom
                .get(id)
                .and_then(oxidepage_dom::Node::as_element)
                .map(|el| el.name.local.to_string());
            if let Some(local) = local
                && dom.is_custom_element_defined(&local)
            {
                dom.push_custom_reaction(oxidepage_dom::CustomElementReaction::Upgrade(id));
            }
        }
    }
    Ok(())
}
