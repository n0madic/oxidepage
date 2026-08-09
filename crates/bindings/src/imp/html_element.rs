//! `HTMLElement` implementation: the autonomous-custom-element constructor and
//! the CSSOM-View `offset*` family.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::custom_element::CustomElementState;
use oxidepage_dom::node::html_name;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::imp::geometry_support::flush_layout;

/// The `HTMLElement()` constructor, reached whenever an autonomous custom
/// element runs `super()` — during a registry-driven upgrade, a synchronous
/// `document.createElement`, or an author `new X()`.
///
/// `new.target` (passed as `call.this` by the QuickJS subclass trampoline)
/// identifies the author class `X`; the trampoline also pins the returned
/// wrapper's prototype to `X.prototype`. There must be a matching definition:
/// `new HTMLElement()` directly (or an unregistered subclass) is an illegal
/// constructor, exactly as in a real browser.
pub(crate) fn constructor(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let new_target = &call.this;
    let def = cx
        .state
        .custom_elements
        .borrow()
        .by_constructor(cx.scope, new_target);
    let Some(def) = def else {
        return Err(JsThrow::Type("Illegal constructor".into()));
    };

    // An upgrade or synchronous createElement pre-created the node and pushed
    // it onto the construction stack; bind this constructor call to it.
    let staged = cx
        .state
        .custom_elements
        .borrow_mut()
        .construction_stack
        .pop();
    if let Some(node) = staged {
        return cx.node_to_js(node);
    }

    // Author `new X()`: create a fresh, disconnected, already-custom element.
    let name = html_name(LocalName::from(def.name.as_str()));
    let node = cx.state.dom.borrow_mut().create_element(name, Vec::new());
    // `create_element` may have enqueued an `Upgrade(node)` intent (the name is
    // defined); marking the node `Custom` here makes that intent a no-op when
    // the queue later drains (the upgrade handler skips non-`Undefined` nodes).
    cx.state
        .dom
        .borrow_mut()
        .set_custom_state(node, CustomElementState::Custom);
    // Retain the wrapper strongly: its subclass prototype and instance state
    // live only here and the generic node-wrapper cache is weak.
    crate::retain_custom_wrapper(cx, node);
    cx.node_to_js(node)
}

pub(crate) fn offset_parent(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(flush_layout(cx, this, |dom, layout| {
        layout.offset_box(dom, this).and_then(|b| b.parent)
    }))
}

fn offset_field(
    cx: &BindCx<'_>,
    this: NodeId,
    f: impl Fn(oxidepage_layout::OffsetBox) -> f32,
) -> Result<f64, JsThrow> {
    Ok(flush_layout(cx, this, |dom, layout| {
        layout
            .offset_box(dom, this)
            .map(|b| f64::from(f(b)).round())
            .unwrap_or(0.0)
    }))
}

pub(crate) fn offset_top(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    offset_field(cx, this, |b| b.top)
}

pub(crate) fn offset_left(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    offset_field(cx, this, |b| b.left)
}

pub(crate) fn offset_width(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    offset_field(cx, this, |b| b.width)
}

pub(crate) fn offset_height(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    offset_field(cx, this, |b| b.height)
}

/// `dir` reflection (HTML "limited to only known values" is v1-relaxed:
/// the attribute value is reflected verbatim; absent reflects as "").
pub(crate) fn dir(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom
        .node(this)
        .as_element()
        .and_then(|el| el.attr(&oxidepage_dom::node::attr_name(LocalName::from("dir"))))
        .map(|v| v.to_string())
        .unwrap_or_default())
}

pub(crate) fn set_dir(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().set_attribute(
        this,
        oxidepage_dom::node::attr_name(LocalName::from("dir")),
        value.into(),
    );
    Ok(())
}

/// `element.dataset` — the live `DOMStringMap` over the element's `data-*`
/// attributes. `[SameObject]`, so repeated reads return one cached Proxy.
pub(crate) fn dataset(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "dataset", |cx| cx.new_dataset(this))
}

// `click`/`focus`/`blur` are the synthetic user interactions; they live in
// `imp::interaction` next to `document.activeElement`, which they maintain.
pub(crate) use crate::imp::interaction::{blur, click, focus};
