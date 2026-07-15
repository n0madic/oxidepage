//! `ShadowRoot` implementation.

use oxidepage_base::NodeId;
use oxidepage_dom::ParseOptions;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

pub(crate) fn host(cx: &BindCx<'_>, this: NodeId) -> Result<NodeId, JsThrow> {
    cx.state
        .dom
        .borrow()
        .shadow_host(this)
        .ok_or_else(|| JsThrow::Type("shadow root has no host".into()))
}

pub(crate) fn mode(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let mode = cx
        .state
        .dom
        .borrow()
        .shadow_mode(this)
        .ok_or_else(|| JsThrow::Type("receiver is not a ShadowRoot".into()))?;
    Ok(mode.as_str().to_owned())
}

pub(crate) fn inner_html(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(oxidepage_dom::serialize::inner_html(
        &cx.state.dom.borrow(),
        this,
    ))
}

pub(crate) fn set_inner_html(cx: &BindCx<'_>, this: NodeId, html: String) -> Result<(), JsThrow> {
    // The fragment-parsing context element is the shadow host.
    let context = {
        let dom = cx.state.dom.borrow();
        let host = dom
            .shadow_host(this)
            .ok_or_else(|| JsThrow::Type("shadow root has no host".into()))?;
        dom.node(host)
            .as_element()
            .expect("shadow host is an element")
            .name
            .clone()
    };
    let removed: Vec<NodeId> = {
        let mut dom = cx.state.dom.borrow_mut();
        let owner = dom.node_document(this);
        let fragment = oxidepage_dom::parser::parse_fragment_into(
            &mut dom,
            &html,
            context,
            ParseOptions::default(),
            owner,
        );
        let old: Vec<NodeId> = dom.children(this).collect();
        for &child in &old {
            dom.remove(child);
        }
        dom.append_child(this, fragment)
            .map_err(|e| cx.dom_exception(e))?;
        old
    };
    cx.free_detached(&removed);
    Ok(())
}

pub(crate) fn adopted_style_sheets(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    adopted_sheets_get(cx, this)
}

pub(crate) fn set_adopted_style_sheets(
    cx: &BindCx<'_>,
    this: NodeId,
    value: JsValue,
) -> Result<(), JsThrow> {
    adopted_sheets_set(cx, this, value)
}

/// Shared `adoptedStyleSheets` getter for ShadowRoot and Document: the
/// stored ObservableArray stand-in, or a fresh empty one on first access
/// (the property must always be present — style injectors feature-detect
/// it). In-place mutations (push, indexed writes, …) sync through the
/// proxy's native callback ([`adopted_sheets_sync`]).
pub(crate) fn adopted_sheets_get(cx: &BindCx<'_>, node: NodeId) -> Result<JsValue, JsThrow> {
    if let Some(stored) = cx.state.adopted_sheets.borrow().get(&node) {
        return Ok(stored.clone());
    }
    let owner = cx.node_to_js(node)?;
    let proxy = cx.new_adopted_sheets_array(&owner, None)?;
    cx.state
        .adopted_sheets
        .borrow_mut()
        .insert(node, proxy.clone());
    Ok(proxy)
}

/// Shared `adoptedStyleSheets` setter: validates and routes the sheets, then
/// replaces the stored backing list with a fresh observable array seeded
/// from the assigned one (spec: assignment replaces the list wholesale).
pub(crate) fn adopted_sheets_set(
    cx: &BindCx<'_>,
    node: NodeId,
    value: JsValue,
) -> Result<(), JsThrow> {
    // Validate + apply first, so an invalid entry throws without storing.
    apply_adopted(cx, node, &value)?;
    let owner = cx.node_to_js(node)?;
    let proxy = cx.new_adopted_sheets_array(&owner, Some(&value))?;
    cx.state.adopted_sheets.borrow_mut().insert(node, proxy);
    Ok(())
}

/// Native callback behind the ObservableArray proxy: re-reads the mutated
/// backing array (`args`: owner wrapper, raw target array) and re-routes it
/// into the style engine.
pub(crate) fn adopted_sheets_sync(
    cx: &BindCx<'_>,
    call: &oxidepage_js::HostCall,
) -> Result<JsValue, JsThrow> {
    let node = cx.this_node(&call.arg(0))?;
    apply_adopted(cx, node, &call.arg(1))?;
    Ok(JsValue::Undefined)
}

/// Validates that `value` is an array of constructed `CSSStyleSheet`s and
/// routes them into `node`'s scope.
fn apply_adopted(cx: &BindCx<'_>, node: NodeId, value: &JsValue) -> Result<(), JsThrow> {
    let JsValue::Object(array) = value else {
        return Err(JsThrow::Type(
            "adoptedStyleSheets must be an array of CSSStyleSheet".into(),
        ));
    };
    let len = cx.scope.array_length(array).map_err(JsThrow::from)?;
    let mut sheets = Vec::with_capacity(len);
    for i in 0..len {
        let entry = cx.scope.array_get(array, i).map_err(JsThrow::from)?;
        let data = cx.this_style_sheet(&entry)?;
        let Some(sheet) = data.constructed_sheet() else {
            return Err(JsThrow::Type(
                "adoptedStyleSheets entries must be constructed CSSStyleSheet objects".into(),
            ));
        };
        sheets.push(sheet);
    }
    sync_adopted_sheets(cx, node, sheets);
    Ok(())
}

/// Pushes the adopted sheets into the style engine for `node`'s scope and
/// invalidates the cascade.
///
/// `node` is a shadow root (scoped cascade) or a document (the page's document
/// scope). A **second** document is neither: it has no stylist of its own, and
/// `scope = None` would route its sheets into the *page's* cascade and restyle
/// the page. Its sheets are stored and reflected back by the getter, but never
/// applied — nothing renders them.
pub(crate) fn sync_adopted_sheets(
    cx: &BindCx<'_>,
    node: NodeId,
    sheets: Vec<style::stylesheets::DocumentStyleSheet>,
) {
    let scope = {
        let dom = cx.state.dom.borrow();
        if dom.is_shadow_root(node) {
            Some(node)
        } else if node == dom.document() {
            None
        } else {
            return;
        }
    };
    cx.state
        .style
        .borrow_mut()
        .set_adopted_sheets(scope, sheets);
    cx.state.dom.borrow_mut().note_adopted_sheets_changed(node);
}
