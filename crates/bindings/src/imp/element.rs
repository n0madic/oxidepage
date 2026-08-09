//! `Element` implementation.

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_dom::node::attr_name;
use oxidepage_dom::{LocalName, Namespace, NodeKind, ParseOptions, Prefix, QualName};
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::imp::names::{NameKind, qualified_name, validate, validate_and_extract};

/// The attribute name as `getAttribute`/`setAttribute` see it: ASCII-lowercased
/// for HTML elements in HTML documents.
pub(crate) fn html_lowercased(cx: &BindCx<'_>, this: NodeId, name: &str) -> String {
    let dom = cx.state.dom.borrow();
    let is_html = dom
        .get(this)
        .and_then(|node| node.as_element())
        .is_some_and(|el| el.is_html_element());
    if is_html {
        name.to_ascii_lowercase()
    } else {
        name.to_owned()
    }
}

/// The DOM's "get an attribute by name": the stored `QualName` of the first
/// attribute whose *qualified* name is `name`. Matching on the qualified name
/// rather than the local one is what lets an attribute set through
/// `setAttributeNS("foo", "foo:bar")` answer to `getAttribute("foo:bar")`.
///
/// `name` must already be HTML-lowercased by the caller.
pub(crate) fn attr_by_qualified_name(
    cx: &BindCx<'_>,
    this: NodeId,
    name: &str,
) -> Option<QualName> {
    let dom = cx.state.dom.borrow();
    let el = dom.get(this)?.as_element()?;
    el.attrs()
        .iter()
        .find(|a| qualified_name(&a.name) == name)
        .map(|a| a.name.clone())
}

pub(crate) fn namespace_uri(cx: &BindCx<'_>, this: NodeId) -> Result<Option<String>, JsThrow> {
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element().expect("element receiver");
    if el.name.ns.is_empty() {
        Ok(None)
    } else {
        Ok(Some(el.name.ns.to_string()))
    }
}

pub(crate) fn prefix(cx: &BindCx<'_>, this: NodeId) -> Result<Option<String>, JsThrow> {
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element().expect("element receiver");
    Ok(el.name.prefix.as_ref().map(|p| p.to_string()))
}

pub(crate) fn local_name(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element().expect("element receiver");
    Ok(el.name.local.to_string())
}

pub(crate) fn tag_name(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    super::node::node_name(cx, this)
}

pub(crate) fn id(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    reflect_get(cx, this, "id")
}

pub(crate) fn set_id(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    reflect_set(cx, this, "id", value)
}

pub(crate) fn class_name(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    reflect_get(cx, this, "class")
}

pub(crate) fn set_class_name(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    reflect_set(cx, this, "class", value)
}

fn reflect_get(cx: &BindCx<'_>, this: NodeId, name: &str) -> Result<String, JsThrow> {
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element().expect("element receiver");
    Ok(el
        .attr(&attr_name(LocalName::from(name)))
        .map(|v| v.to_string())
        .unwrap_or_default())
}

fn reflect_set(cx: &BindCx<'_>, this: NodeId, name: &str, value: String) -> Result<(), JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(this, attr_name(LocalName::from(name)), value.into());
    Ok(())
}

pub(crate) fn class_list(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "classList", |cx| {
        cx.new_collection(
            "DOMTokenList",
            CollectionData::TokenList {
                element: this,
                attr: LocalName::from("class"),
            },
        )
    })
}

pub(crate) fn part(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "part", |cx| {
        cx.new_collection(
            "DOMTokenList",
            CollectionData::TokenList {
                element: this,
                attr: LocalName::from("part"),
            },
        )
    })
}

// === Shadow DOM ===

pub(crate) fn attach_shadow(
    cx: &BindCx<'_>,
    this: NodeId,
    init: JsValue,
) -> Result<NodeId, JsThrow> {
    let mode = match &init {
        JsValue::Object(obj) => {
            let value = cx.scope.get(obj, "mode").map_err(JsThrow::from)?;
            if value.is_undefined() {
                None
            } else {
                Some(cx.scope.coerce_string(&value).map_err(JsThrow::from)?)
            }
        }
        _ => None,
    };
    let mode = match mode.as_deref() {
        Some("open") => oxidepage_dom::ShadowMode::Open,
        Some("closed") => oxidepage_dom::ShadowMode::Closed,
        _ => {
            return Err(JsThrow::Type(
                "attachShadow: init.mode must be 'open' or 'closed'".into(),
            ));
        }
    };
    cx.state
        .dom
        .borrow_mut()
        .attach_shadow(this, mode)
        .map_err(|e| cx.dom_exception(e))
}

pub(crate) fn shadow_root(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    let open_root = {
        let dom = cx.state.dom.borrow();
        dom.shadow_root(this)
            .filter(|&sr| dom.shadow_mode(sr) == Some(oxidepage_dom::ShadowMode::Open))
    };
    cx.opt_node_to_js(open_root)
}

pub(crate) fn assigned_slot(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    let slot = {
        let dom = cx.state.dom.borrow();
        // The slot is exposed only when its shadow tree is open.
        dom.assigned_slot(this).filter(|&slot| {
            dom.containing_shadow_root(slot)
                .and_then(|sr| dom.shadow_mode(sr))
                == Some(oxidepage_dom::ShadowMode::Open)
        })
    };
    cx.opt_node_to_js(slot)
}

pub(crate) fn attributes(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "attributes", |cx| cx.new_named_node_map(this))
}

pub(crate) fn has_attributes(cx: &BindCx<'_>, this: NodeId) -> Result<bool, JsThrow> {
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element().expect("element receiver");
    Ok(!el.attrs().is_empty())
}

pub(crate) fn get_attribute_names(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    let names: Vec<JsValue> = {
        let dom = cx.state.dom.borrow();
        let el = dom.node(this).as_element().expect("element receiver");
        el.attrs()
            .iter()
            .map(|a| JsValue::String(qualified_name(&a.name)))
            .collect()
    };
    cx.scope
        .new_array(&names)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

pub(crate) fn get_attribute(
    cx: &BindCx<'_>,
    this: NodeId,
    name: String,
) -> Result<Option<String>, JsThrow> {
    let name = html_lowercased(cx, this, &name);
    let Some(qual) = attr_by_qualified_name(cx, this, &name) else {
        return Ok(None);
    };
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element().expect("element receiver");
    Ok(el.attr(&qual).map(|v| v.to_string()))
}

pub(crate) fn get_attribute_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    local: String,
) -> Result<Option<String>, JsThrow> {
    let ns = namespace.unwrap_or_default();
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element().expect("element receiver");
    Ok(el
        .attrs()
        .iter()
        .find(|a| *a.name.local == *local && *a.name.ns == *ns)
        .map(|a| a.value.to_string()))
}

pub(crate) fn set_attribute(
    cx: &BindCx<'_>,
    this: NodeId,
    name: String,
    value: String,
) -> Result<(), JsThrow> {
    validate(cx, NameKind::Attribute, &name)?;
    let name = html_lowercased(cx, this, &name);
    // An existing attribute with this qualified name keeps its prefix and
    // namespace; a fresh one takes the whole qualified name as its local name.
    let qual =
        attr_by_qualified_name(cx, this, &name).unwrap_or_else(|| attr_name(LocalName::from(name)));
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(this, qual, value.into());
    Ok(())
}

pub(crate) fn remove_attribute(cx: &BindCx<'_>, this: NodeId, name: String) -> Result<(), JsThrow> {
    let name = html_lowercased(cx, this, &name);
    if let Some(qual) = attr_by_qualified_name(cx, this, &name) {
        cx.state.dom.borrow_mut().remove_attribute(this, &qual);
    }
    Ok(())
}

pub(crate) fn toggle_attribute(
    cx: &BindCx<'_>,
    this: NodeId,
    name: String,
    force: Option<bool>,
) -> Result<bool, JsThrow> {
    validate(cx, NameKind::Attribute, &name)?;
    let name = html_lowercased(cx, this, &name);
    let existing = attr_by_qualified_name(cx, this, &name);
    let mut dom = cx.state.dom.borrow_mut();
    match (existing, force) {
        (None, None | Some(true)) => {
            dom.set_attribute(this, attr_name(LocalName::from(name)), "".into());
            Ok(true)
        }
        (None, Some(false)) => Ok(false),
        (Some(qual), None | Some(false)) => {
            dom.remove_attribute(this, &qual);
            Ok(false)
        }
        (Some(_), Some(true)) => Ok(true),
    }
}

pub(crate) fn has_attribute(cx: &BindCx<'_>, this: NodeId, name: String) -> Result<bool, JsThrow> {
    Ok(get_attribute(cx, this, name)?.is_some())
}

pub(crate) fn has_attribute_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    local: String,
) -> Result<bool, JsThrow> {
    Ok(get_attribute_ns(cx, this, namespace, local)?.is_some())
}

/// The exact stored `QualName` of an attribute matched by `(namespace, local)`
/// ignoring its prefix — the identity the DOM's attribute list matches on.
fn stored_attr_qual(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: &str,
    local: &str,
) -> Option<QualName> {
    let dom = cx.state.dom.borrow();
    let el = dom.node(this).as_element()?;
    el.attrs()
        .iter()
        .find(|a| *a.name.local == *local && *a.name.ns == *namespace)
        .map(|a| a.name.clone())
}

pub(crate) fn set_attribute_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    qualified_name: String,
    value: String,
) -> Result<(), JsThrow> {
    let (prefix, local) = validate_and_extract(
        cx,
        NameKind::Attribute,
        namespace.as_deref(),
        &qualified_name,
    )?;
    let ns = namespace.filter(|ns| !ns.is_empty()).unwrap_or_default();
    // Update an existing same-namespace, same-local attribute in place
    // regardless of its stored prefix; otherwise add a fresh one.
    let qual = stored_attr_qual(cx, this, &ns, &local).unwrap_or_else(|| {
        QualName::new(
            prefix.map(Prefix::from),
            Namespace::from(ns),
            LocalName::from(local),
        )
    });
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(this, qual, value.into());
    Ok(())
}

pub(crate) fn remove_attribute_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    local: String,
) -> Result<(), JsThrow> {
    let ns = namespace.filter(|ns| !ns.is_empty()).unwrap_or_default();
    if let Some(qual) = stored_attr_qual(cx, this, &ns, &local) {
        cx.state.dom.borrow_mut().remove_attribute(this, &qual);
    }
    Ok(())
}

pub(crate) fn matches(cx: &BindCx<'_>, this: NodeId, selectors: String) -> Result<bool, JsThrow> {
    let list = oxidepage_dom::parse_selector_list(&selectors).map_err(|e| cx.dom_exception(e))?;
    Ok(cx.state.dom.borrow().element_matches(this, &list))
}

pub(crate) fn closest(
    cx: &BindCx<'_>,
    this: NodeId,
    selectors: String,
) -> Result<Option<NodeId>, JsThrow> {
    let list = oxidepage_dom::parse_selector_list(&selectors).map_err(|e| cx.dom_exception(e))?;
    Ok(cx.state.dom.borrow().closest(this, &list))
}

pub(crate) fn get_elements_by_tag_name(
    cx: &BindCx<'_>,
    this: NodeId,
    name: String,
) -> Result<JsValue, JsThrow> {
    by_tag_name(cx, this, name)
}

pub(crate) fn by_tag_name(cx: &BindCx<'_>, root: NodeId, name: String) -> Result<JsValue, JsThrow> {
    let name = if name == "*" { None } else { Some(name) };
    cx.new_collection("HTMLCollection", CollectionData::ByTagName { root, name })
}

pub(crate) fn get_elements_by_tag_name_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    local_name: String,
) -> Result<JsValue, JsThrow> {
    by_tag_name_ns(cx, this, namespace, local_name)
}

/// DOM §4.2.6 "get elements by namespace and local name": `namespace == "*"`
/// is the wildcard; `None` (JS `null`) and `Some("")` both mean the null
/// namespace, matching only elements with no namespace at all.
/// `local_name == "*"` is the wildcard local name. Never case-folds — unlike
/// `getElementsByTagName`, the HTML-document special case does not apply.
pub(crate) fn by_tag_name_ns(
    cx: &BindCx<'_>,
    root: NodeId,
    namespace: Option<String>,
    local_name: String,
) -> Result<JsValue, JsThrow> {
    let namespace = match namespace.as_deref() {
        Some("*") => None,
        Some(ns) => Some(Namespace::from(ns)),
        None => Some(Namespace::from("")),
    };
    let local_name = if local_name == "*" {
        None
    } else {
        Some(LocalName::from(local_name))
    };
    cx.new_collection(
        "HTMLCollection",
        CollectionData::ByTagNameNS {
            root,
            namespace,
            local_name,
        },
    )
}

pub(crate) fn get_elements_by_class_name(
    cx: &BindCx<'_>,
    this: NodeId,
    classes: String,
) -> Result<JsValue, JsThrow> {
    by_class_name(cx, this, classes)
}

pub(crate) fn by_class_name(
    cx: &BindCx<'_>,
    root: NodeId,
    classes: String,
) -> Result<JsValue, JsThrow> {
    let classes: Vec<LocalName> = classes
        .split_ascii_whitespace()
        .map(LocalName::from)
        .collect();
    cx.new_collection(
        "HTMLCollection",
        CollectionData::ByClassName { root, classes },
    )
}

/// Spec "insert adjacent": resolves the insertion point for
/// `insertAdjacent{Element,Text,HTML}`. Returns `None` when the operation is
/// a no-op (`beforebegin`/`afterend` with no parent).
fn adjacent_insertion_point(
    cx: &BindCx<'_>,
    this: NodeId,
    position: &str,
) -> Result<Option<(NodeId, Option<NodeId>)>, JsThrow> {
    let dom = cx.state.dom.borrow();
    let parent = dom.node(this).parent();
    match position.to_ascii_lowercase().as_str() {
        "beforebegin" => Ok(parent.map(|p| (p, Some(this)))),
        "afterbegin" => Ok(Some((this, dom.node(this).first_child()))),
        "beforeend" => Ok(Some((this, None))),
        "afterend" => Ok(parent.map(|p| (p, dom.node(this).next_sibling()))),
        _ => Err(cx.dom_throw(
            DomExceptionKind::SyntaxError,
            "position must be beforebegin, afterbegin, beforeend, or afterend",
        )),
    }
}

pub(crate) fn insert_adjacent_element(
    cx: &BindCx<'_>,
    this: NodeId,
    position: String,
    element: NodeId,
) -> Result<Option<NodeId>, JsThrow> {
    let Some((parent, before)) = adjacent_insertion_point(cx, this, &position)? else {
        return Ok(None);
    };
    cx.state
        .dom
        .borrow_mut()
        .insert_before(parent, element, before)
        .map_err(|e| cx.dom_exception(e))?;
    Ok(Some(element))
}

pub(crate) fn insert_adjacent_text(
    cx: &BindCx<'_>,
    this: NodeId,
    position: String,
    data: String,
) -> Result<(), JsThrow> {
    let Some((parent, before)) = adjacent_insertion_point(cx, this, &position)? else {
        return Ok(());
    };
    let mut dom = cx.state.dom.borrow_mut();
    let text = dom.create_text(data.into());
    dom.insert_before(parent, text, before)
        .map_err(|e| cx.dom_exception(e))?;
    Ok(())
}

/// The fragment-parsing target: a `<template>` element's contents live in
/// its contents fragment.
fn inner_target(cx: &BindCx<'_>, this: NodeId) -> NodeId {
    let mut dom = cx.state.dom.borrow_mut();
    let is_template = dom
        .node(this)
        .as_element()
        .is_some_and(|el| el.is_html_element() && &*el.name.local == "template");
    if is_template {
        dom.ensure_template_contents(this)
    } else {
        this
    }
}

pub(crate) fn inner_html(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let target = inner_target(cx, this);
    Ok(oxidepage_dom::serialize::inner_html(
        &cx.state.dom.borrow(),
        target,
    ))
}

pub(crate) fn set_inner_html(cx: &BindCx<'_>, this: NodeId, html: String) -> Result<(), JsThrow> {
    let target = inner_target(cx, this);
    let context = {
        let dom = cx.state.dom.borrow();
        dom.node(this)
            .as_element()
            .expect("element receiver")
            .name
            .clone()
    };
    let removed: Vec<NodeId> = {
        let mut dom = cx.state.dom.borrow_mut();
        let owner = dom.node_document(target);
        let fragment = oxidepage_dom::parser::parse_fragment_into(
            &mut dom,
            &html,
            context,
            ParseOptions::default(),
            owner,
        );
        let old: Vec<NodeId> = dom.children(target).collect();
        for &child in &old {
            dom.remove(child);
        }
        dom.append_child(target, fragment)
            .map_err(|e| cx.dom_exception(e))?;
        old
    };
    cx.free_detached(&removed);
    Ok(())
}

pub(crate) fn outer_html(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(oxidepage_dom::serialize::outer_html(
        &cx.state.dom.borrow(),
        this,
    ))
}

pub(crate) fn set_outer_html(cx: &BindCx<'_>, this: NodeId, html: String) -> Result<(), JsThrow> {
    let parent = cx.state.dom.borrow().node(this).parent();
    let Some(parent) = parent else {
        return Err(cx.dom_throw(
            DomExceptionKind::NoModificationAllowedError,
            "outerHTML on an element without a parent",
        ));
    };
    if cx.state.dom.borrow().node(parent).data().kind() == NodeKind::Document {
        return Err(cx.dom_throw(
            DomExceptionKind::NoModificationAllowedError,
            "outerHTML on the document element",
        ));
    }
    // Context element: the parent, or `body` when the parent is a fragment.
    let context = {
        let dom = cx.state.dom.borrow();
        match dom.node(parent).as_element() {
            Some(el) => el.name.clone(),
            None => QualName::new(
                None,
                Namespace::from("http://www.w3.org/1999/xhtml"),
                LocalName::from("body"),
            ),
        }
    };
    {
        let mut dom = cx.state.dom.borrow_mut();
        let owner = dom.node_document(parent);
        let fragment = oxidepage_dom::parser::parse_fragment_into(
            &mut dom,
            &html,
            context,
            ParseOptions::default(),
            owner,
        );
        dom.replace_child(parent, fragment, this)
            .map_err(|e| cx.dom_exception(e))?;
    }
    cx.free_detached(&[this]);
    Ok(())
}

pub(crate) fn insert_adjacent_html(
    cx: &BindCx<'_>,
    this: NodeId,
    position: String,
    html: String,
) -> Result<(), JsThrow> {
    let lowered = position.to_ascii_lowercase();
    // Per spec, beforebegin/afterend require a non-document parent.
    let context_node = match lowered.as_str() {
        "beforebegin" | "afterend" => {
            let parent = cx.state.dom.borrow().node(this).parent();
            match parent {
                Some(p) if cx.state.dom.borrow().node(p).data().kind() != NodeKind::Document => p,
                _ => {
                    return Err(cx.dom_throw(
                        DomExceptionKind::NoModificationAllowedError,
                        "cannot insert HTML outside the root element",
                    ));
                }
            }
        }
        "afterbegin" | "beforeend" => this,
        _ => {
            return Err(cx.dom_throw(
                DomExceptionKind::SyntaxError,
                "position must be beforebegin, afterbegin, beforeend, or afterend",
            ));
        }
    };
    let Some((parent, before)) = adjacent_insertion_point(cx, this, &lowered)? else {
        return Ok(());
    };
    let context = {
        let dom = cx.state.dom.borrow();
        match dom.node(context_node).as_element() {
            Some(el) => el.name.clone(),
            None => QualName::new(
                None,
                Namespace::from("http://www.w3.org/1999/xhtml"),
                LocalName::from("body"),
            ),
        }
    };
    let mut dom = cx.state.dom.borrow_mut();
    let owner = dom.node_document(parent);
    let fragment = oxidepage_dom::parser::parse_fragment_into(
        &mut dom,
        &html,
        context,
        ParseOptions::default(),
        owner,
    );
    dom.insert_before(parent, fragment, before)
        .map_err(|e| cx.dom_exception(e))?;
    Ok(())
}

// === CSSOM-View geometry (Phase 5) ===

use crate::imp::geometry_support::{
    flush_layout, flush_layout_mut, is_document_element, note_scroll, rect_data,
};
/// `block`/`inline` alignment: `layout`'s, so the enum the Web API parses into
/// is the one the scroll algorithm matches on.
use oxidepage_layout::Align;

pub(crate) fn get_client_rects(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    let rects = flush_layout(cx, this, |dom, layout| layout.client_rects(dom, this));
    let rects = rects
        .into_iter()
        .map(|r| std::rc::Rc::new(std::cell::RefCell::new(rect_data(r))))
        .collect();
    cx.new_dom_rect_list(rects)
}

pub(crate) fn get_bounding_client_rect(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    let rect = flush_layout(cx, this, |dom, layout| {
        layout.bounding_client_rect(dom, this)
    });
    // No boxes → a zero DOMRect, per CSSOM-View.
    let data = rect.map(rect_data).unwrap_or(crate::state::RectData {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    });
    cx.new_dom_rect("DOMRect", data)
}

pub(crate) fn scroll_top(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(flush_layout(cx, this, |dom, layout| {
        if is_document_element(dom, this) {
            f64::from(layout.viewport_scroll().y)
        } else {
            f64::from(layout.scroll_offset(this).y)
        }
    }))
}

pub(crate) fn set_scroll_top(cx: &BindCx<'_>, this: NodeId, value: f64) -> Result<(), JsThrow> {
    let value = if value.is_finite() { value as f32 } else { 0.0 };
    let (target, changed) = flush_layout_mut(cx, this, |dom, layout| {
        if is_document_element(dom, this) {
            let x = layout.viewport_scroll().x;
            (None, layout.set_viewport_scroll(x, value).changed)
        } else {
            let x = layout.scroll_offset(this).x;
            (Some(this), layout.set_scroll_offset(this, x, value).changed)
        }
    });
    note_scroll(cx, target, changed);
    Ok(())
}

pub(crate) fn scroll_left(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(flush_layout(cx, this, |dom, layout| {
        if is_document_element(dom, this) {
            f64::from(layout.viewport_scroll().x)
        } else {
            f64::from(layout.scroll_offset(this).x)
        }
    }))
}

pub(crate) fn set_scroll_left(cx: &BindCx<'_>, this: NodeId, value: f64) -> Result<(), JsThrow> {
    let value = if value.is_finite() { value as f32 } else { 0.0 };
    let (target, changed) = flush_layout_mut(cx, this, |dom, layout| {
        if is_document_element(dom, this) {
            let y = layout.viewport_scroll().y;
            (None, layout.set_viewport_scroll(value, y).changed)
        } else {
            let y = layout.scroll_offset(this).y;
            (Some(this), layout.set_scroll_offset(this, value, y).changed)
        }
    });
    note_scroll(cx, target, changed);
    Ok(())
}

pub(crate) fn scroll_width(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(flush_layout(cx, this, |_, layout| {
        layout
            .scroll_size(this)
            .map(|(w, _)| f64::from(w).round())
            .unwrap_or(0.0)
    }))
}

pub(crate) fn scroll_height(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(flush_layout(cx, this, |_, layout| {
        layout
            .scroll_size(this)
            .map(|(_, h)| f64::from(h).round())
            .unwrap_or(0.0)
    }))
}

fn client_box_field(
    cx: &BindCx<'_>,
    this: NodeId,
    f: impl Fn(oxidepage_layout::ClientBox) -> f32,
) -> Result<f64, JsThrow> {
    Ok(flush_layout(cx, this, |_, layout| {
        layout
            .client_box(this)
            .map(|b| f64::from(f(b)).round())
            .unwrap_or(0.0)
    }))
}

pub(crate) fn client_top(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    client_box_field(cx, this, |b| b.top)
}

pub(crate) fn client_left(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    client_box_field(cx, this, |b| b.left)
}

pub(crate) fn client_width(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    client_box_field(cx, this, |b| b.width)
}

pub(crate) fn client_height(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    client_box_field(cx, this, |b| b.height)
}

/// `checkVisibility()`. Per spec: false if `this` has no associated box —
/// `display: none` on itself or an ancestor, including across a shadow-DOM
/// slot assignment, since box construction only reaches a slotted node once
/// every flat-tree ancestor up to it is displayed, so `box_for_node` alone
/// captures that case. `checkVisibilityCSS`/`visibilityProperty` additionally
/// require the *used* `visibility` to be `visible`; `visibility` is an
/// inherited property, so an ancestor's `visibility: hidden` is already
/// folded into the cascade result and needs no separate ancestor walk.
/// `checkOpacity`/`opacityProperty` walk the flat-tree ancestor chain for an
/// `opacity: 0`, since opacity (unlike visibility) is not inherited but
/// still hides descendants. `contentVisibilityAuto` is about
/// `content-visibility`, which we do not implement (P6 "absent beats fake")
/// — it is a documented no-op rather than a faked check.
pub(crate) fn check_visibility(
    cx: &BindCx<'_>,
    this: NodeId,
    options: JsValue,
) -> Result<bool, JsThrow> {
    use style::computed_values::visibility::T as Visibility;

    let dict_bool = |name: &str| -> Result<bool, JsThrow> {
        match &options {
            JsValue::Object(obj) => Ok(cx.scope.get(obj, name).map_err(JsThrow::from)?.truthy()),
            _ => Ok(false),
        }
    };
    let check_visibility_css = dict_bool("checkVisibilityCSS")? || dict_bool("visibilityProperty")?;
    let check_opacity = dict_bool("checkOpacity")? || dict_bool("opacityProperty")?;

    Ok(flush_layout(cx, this, |dom, layout| {
        if layout.tree().box_for_node(this).is_none() {
            return false;
        }
        if check_visibility_css {
            let visible = dom
                .primary_style(this)
                .is_some_and(|s| s.get_inherited_box().clone_visibility() == Visibility::Visible);
            if !visible {
                return false;
            }
        }
        if check_opacity {
            let mut cur = Some(this);
            while let Some(n) = cur {
                if dom
                    .primary_style(n)
                    .is_some_and(|s| s.get_effects().clone_opacity() <= 0.0)
                {
                    return false;
                }
                cur = dom.flat_tree_parent(n);
            }
        }
        true
    }))
}

/// `scrollParent()` (CSSOM-View, draft): delegates the containing-block walk
/// to the layout crate, then resolves its "reached the initial containing
/// block" answer to `document.scrollingElement` here — layout has no notion
/// of quirks-mode scrolling-element promotion.
pub(crate) fn scroll_parent(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    // An element that already *is* the document's scrolling element has no
    // scroll parent — its own scrolling *is* the document's. This covers
    // `documentElement.scrollParent() === null` in standards mode (redundant
    // with layout's own root-element check below) and, uniquely,
    // `body.scrollParent() === null` in quirks mode, where the *body*, not
    // the root, is the scrolling element (so the root-element check alone
    // would miss it).
    // `scrolling_element` is a *document* member: pass this element's node
    // document, not the element itself.
    let document = cx.state.dom.borrow().node_document(this);
    let scrolling_element = super::document::scrolling_element(cx, document)?;
    if scrolling_element == Some(this) {
        return Ok(None);
    }
    let result = flush_layout(cx, this, |dom, layout| layout.scroll_parent(dom, this));
    match result {
        oxidepage_layout::ScrollParent::None => Ok(None),
        oxidepage_layout::ScrollParent::Element(node) => Ok(Some(node)),
        oxidepage_layout::ScrollParent::DocumentScrollingElement => Ok(scrolling_element),
    }
}

/// `part` setter (`[PutForwards=value]`): assignment writes the token list's
/// `value`, i.e. the `part` attribute.
pub(crate) fn set_part(cx: &BindCx<'_>, this: NodeId, value: JsValue) -> Result<(), JsThrow> {
    let text = cx.scope.coerce_string(&value).map_err(JsThrow::from)?;
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(this, attr_name(LocalName::from("part")), text.into());
    Ok(())
}

/// `Element.scrollIntoView(arg)`, where `arg` is the legacy boolean or a
/// `ScrollIntoViewOptions`.
///
/// The algorithm itself lives in `layout::scroll_into_view` — the single
/// definition `Page::scroll_into_view_if_needed` shares (ADR-0026). This reads
/// the arguments, runs it under one layout borrow, and queues the `scroll`
/// events it reports afterwards, so nothing re-enters JS while layout is
/// borrowed.
pub(crate) fn scroll_into_view(cx: &BindCx<'_>, this: NodeId, arg: JsValue) -> Result<(), JsThrow> {
    let (block, inline) = scroll_alignment(cx, &arg);
    let scrolled = flush_layout_mut(cx, this, |dom, layout| {
        oxidepage_layout::scroll_into_view(layout, dom, this, None, block, inline)
    });
    for target in scrolled {
        note_scroll(cx, target, true);
    }
    Ok(())
}

/// Reads the argument: `true`/absent means `start`, `false` means `end`, and an
/// options dictionary names each axis.
fn scroll_alignment(cx: &BindCx<'_>, arg: &JsValue) -> (Align, Align) {
    match arg {
        // `scrollIntoView()` and `scrollIntoView(true)`: align to the start.
        JsValue::Undefined => (Align::Start, Align::Nearest),
        JsValue::Bool(true) => (Align::Start, Align::Nearest),
        JsValue::Bool(false) => (Align::End, Align::Nearest),
        JsValue::Object(_) => {
            let read = |name: &str, default: Align| {
                crate::imp::ui_event::member(cx, arg, name)
                    .and_then(|v| cx.scope.coerce_string(&v).ok())
                    .map_or(default, |s| match s.as_str() {
                        "start" => Align::Start,
                        "center" => Align::Center,
                        "end" => Align::End,
                        _ => Align::Nearest,
                    })
            };
            (read("block", Align::Start), read("inline", Align::Nearest))
        }
        _ => (Align::Start, Align::Nearest),
    }
}
