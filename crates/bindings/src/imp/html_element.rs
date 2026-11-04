//! `HTMLElement` implementation: the autonomous-custom-element constructor and
//! the CSSOM-View `offset*` family.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::custom_element::CustomElementState;
use oxidepage_dom::node::html_name;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::imp::geometry_support::flush_layout;
use crate::imp::reflect::{bool_reflector, string_reflector};

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

string_reflector!(title, set_title, "title");
string_reflector!(lang, set_lang, "lang");
string_reflector!(access_key, set_access_key, "accesskey");
bool_reflector!(inert, set_inert, "inert");
bool_reflector!(autofocus, set_autofocus, "autofocus");

/// `translate`: whether the element's *translation mode* is
/// translate-enabled. `translate="yes"`/`""` enables, `"no"` disables, and
/// anything else (or no attribute) inherits from the parent element; the root
/// default is enabled.
pub(crate) fn translate(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    let dom = cx.state.dom.borrow();
    let name = oxidepage_dom::node::attr_name(LocalName::from("translate"));
    let mut current = Some(this);
    while let Some(id) = current {
        let Some(node) = dom.get(id) else { break };
        if let Some(el) = node.as_element()
            && el.is_html_element()
            && let Some(value) = el.attr(&name)
        {
            if value.eq_ignore_ascii_case("no") {
                return Ok(false);
            }
            if value.is_empty() || value.eq_ignore_ascii_case("yes") {
                return Ok(true);
            }
        }
        current = node
            .parent()
            .filter(|&p| dom.get(p).is_some_and(|n| n.as_element().is_some()));
    }
    Ok(true)
}

pub(crate) fn set_translate(cx: &BindCx<'_>, this: NodeId, value: bool) -> Result<(), JsThrow> {
    crate::imp::reflect::set_string(
        cx,
        this,
        "translate",
        (if value { "yes" } else { "no" }).into(),
    );
    Ok(())
}

/// `hidden`: `"until-found"` when the attribute is in that state, otherwise
/// whether the attribute is present.
pub(crate) fn hidden(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    let dom = cx.state.dom.borrow();
    let value = dom
        .node(this)
        .as_element()
        .and_then(|el| el.attr(&oxidepage_dom::node::attr_name(LocalName::from("hidden"))))
        .map(|v| v.to_string());
    Ok(match value {
        Some(v) if v.eq_ignore_ascii_case("until-found") => JsValue::String("until-found".into()),
        Some(_) => JsValue::Bool(true),
        None => JsValue::Bool(false),
    })
}

/// The `hidden` setter (HTML "the hidden attribute"): `"until-found"` keeps
/// that state; `false`, `""`, `null`, `0` and `NaN` remove the attribute;
/// anything else sets it to the empty string.
pub(crate) fn set_hidden(cx: &BindCx<'_>, this: NodeId, value: JsValue) -> Result<(), JsThrow> {
    let state = match &value {
        JsValue::Undefined | JsValue::Null | JsValue::Bool(false) => None,
        JsValue::Bool(true) => Some(""),
        JsValue::Number(n) => (*n != 0.0 && !n.is_nan()).then_some(""),
        JsValue::String(s) if s.eq_ignore_ascii_case("until-found") => Some("until-found"),
        JsValue::String(s) => (!s.is_empty()).then_some(""),
        // An object is converted to a string, which is never "until-found"
        // after `[object …]` coercion unless its `toString` says so.
        other => {
            let s = cx.scope.coerce_string(other).map_err(JsThrow::from)?;
            if s.eq_ignore_ascii_case("until-found") {
                Some("until-found")
            } else {
                (!s.is_empty()).then_some("")
            }
        }
    };
    match state {
        Some(v) => crate::imp::reflect::set_string(cx, this, "hidden", v.into()),
        None => crate::imp::reflect::set_bool(cx, this, "hidden", false),
    }
    Ok(())
}

/// `tabIndex`: the `tabindex` attribute parsed by the HTML rules for parsing
/// integers, or the element's default: 0 for the natively focusable kinds
/// (and a `<summary>` that is its parent `<details>`'s summary), -1 otherwise.
pub(crate) fn tab_index(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    let dom = cx.state.dom.borrow();
    let Some(el) = dom.node(this).as_element() else {
        return Ok(-1.0);
    };
    if let Some(value) = el
        .attr(&oxidepage_dom::node::attr_name(LocalName::from("tabindex")))
        .and_then(|v| parse_html_integer(v))
    {
        return Ok(f64::from(value));
    }
    let default = match &**el.local_name() {
        "a" | "area" | "button" | "frame" | "iframe" | "input" | "object" | "select"
        | "textarea" => 0.0,
        "summary" => {
            let is_details_summary = dom.node(this).parent().is_some_and(|parent| {
                dom.node(parent)
                    .as_element()
                    .is_some_and(|p| p.is_html_element() && &**p.local_name() == "details")
                    && dom.children(parent).find(|&c| {
                        dom.node(c)
                            .as_element()
                            .is_some_and(|e| e.is_html_element() && &**e.local_name() == "summary")
                    }) == Some(this)
            });
            if is_details_summary { 0.0 } else { -1.0 }
        }
        _ => -1.0,
    };
    Ok(default)
}

pub(crate) fn set_tab_index(cx: &BindCx<'_>, this: NodeId, value: i32) -> Result<(), JsThrow> {
    crate::imp::reflect::set_string(cx, this, "tabindex", value.to_string());
    Ok(())
}

/// HTML "rules for parsing integers": optional leading ASCII whitespace, an
/// optional sign, then digits; trailing garbage is ignored. `None` on no
/// digits or an `i32` overflow.
pub(crate) fn parse_html_integer(input: &str) -> Option<i32> {
    let s = input.trim_start_matches(['\t', '\n', '\x0C', '\r', ' ']);
    let (negative, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let magnitude: i64 = rest[..digits].parse().ok()?;
    let value = if negative { -magnitude } else { magnitude };
    i32::try_from(value).ok()
}

/// `element.dataset` — the live `DOMStringMap` over the element's `data-*`
/// attributes. `[SameObject]`, so repeated reads return one cached Proxy.
pub(crate) fn dataset(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "dataset", |cx| cx.new_dataset(this))
}

// `click`/`focus`/`blur` are the synthetic user interactions; they live in
// `imp::interaction` next to `document.activeElement`, which they maintain.
pub(crate) use crate::imp::interaction::{blur, click, focus};
