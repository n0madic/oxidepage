//! `Document` implementation (DOM core plus the trimmed HTML additions).

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_dom::node::{DocumentData, html_name};
use oxidepage_dom::{LocalName, Namespace, NodeKind, Prefix, QualName, QuirksMode};
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::events::{EventData, UiKind, UiPayload};
use crate::imp::names::{NameKind, validate, validate_and_extract, validate_xml_name};

/// Whether `this` is the one *rendered* document.
///
/// Members that reflect the browsing context — `defaultView`, `currentScript`,
/// `styleSheets`, `readyState`, layout geometry — are the page document's
/// alone; a second document has no browsing context, and the spec says so.
fn is_page_document(cx: &BindCx<'_>, this: NodeId) -> bool {
    this == cx.state.dom.borrow().document()
}

/// `new Document()`: an XML document with no browsing context. It exposes
/// `Document`, not `XMLDocument` — only `createDocument()` does that.
pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    let document = cx.state.dom.borrow_mut().create_document(DocumentData::xml(
        "about:blank".to_owned(),
        "application/xml".to_owned(),
        /* xml_document_interface */ false,
    ));
    cx.node_to_js(document)
}

pub(crate) fn implementation(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "implementation", move |cx| {
        cx.new_dom_implementation(this)
    })
}

fn write_text(cx: &BindCx<'_>, text: String) -> Result<(), JsThrow> {
    match cx.state.queue_parser_write(&text) {
        Ok(true) => Ok(()),
        Ok(false) => {
            cx.state.hooks.console_message(
                crate::state::ConsoleLevel::Warn,
                "document.write outside an active parser script was ignored".to_owned(),
            );
            Ok(())
        }
        Err(message) => Err(JsThrow::Range(message.to_owned())),
    }
}

/// `document.write` on an XML document throws; on a second HTML document there
/// is no parser to write into, so it warns and no-ops as it already did off the
/// parser path. (`open()`/`close()` are not implemented — see ADR-0017.)
fn ensure_writable(cx: &BindCx<'_>, this: NodeId) -> Result<(), JsThrow> {
    if cx.state.dom.borrow().is_html_document(this) {
        Ok(())
    } else {
        Err(cx.dom_throw(
            DomExceptionKind::InvalidStateError,
            "document.write on an XML document",
        ))
    }
}

pub(crate) fn write(cx: &BindCx<'_>, this: NodeId, text: Vec<String>) -> Result<(), JsThrow> {
    ensure_writable(cx, this)?;
    write_text(cx, text.concat())
}

pub(crate) fn writeln(cx: &BindCx<'_>, this: NodeId, text: Vec<String>) -> Result<(), JsThrow> {
    ensure_writable(cx, this)?;
    let mut text = text.concat();
    text.push('\n');
    write_text(cx, text)
}

/// The document URL, not the base URL: `<base href>` moves `document.baseURI`
/// and relative-URL resolution, never `document.URL`/`documentURI`.
pub(crate) fn url(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx.state.dom.borrow().document_url_of(this).to_owned())
}

/// The referrer of the navigation that created this document: the URL of the
/// document it was navigated *from*, written by the page at commit time
/// (ADR-0022). `""` when the navigation had no predecessor — an embedder
/// `Page::navigate`, or the initial document.
///
/// Only the rendered document has a browsing context to have been navigated
/// within; an inert `DOMParser`/`createHTMLDocument` document reports `""`.
pub(crate) fn referrer(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    if !is_page_document(cx, this) {
        return Ok(String::new());
    }
    Ok(cx.state.referrer())
}

/// A second document's sheets are never registered with the page's stylist, so
/// its `styleSheets` is honestly empty rather than reporting the page's
/// (see `style_sheet_list::sheet_owners`).
pub(crate) fn style_sheets(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    cx.same_object(this, "styleSheets", |cx| cx.new_style_sheet_list(this))
}

pub(crate) fn document_uri(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    url(cx, this)
}

pub(crate) fn compat_mode(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(match cx.state.dom.borrow().quirks_mode_of(this) {
        QuirksMode::Quirks => "BackCompat".to_owned(),
        _ => "CSS1Compat".to_owned(),
    })
}

pub(crate) fn character_set(_cx: &BindCx<'_>, _this: NodeId) -> Result<String, JsThrow> {
    Ok("UTF-8".to_owned())
}

/// Legacy aliases of `characterSet`, both required by the DOM standard.
pub(crate) fn charset(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    character_set(cx, this)
}

pub(crate) fn input_encoding(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    character_set(cx, this)
}

/// `"loading"` while the parser runs, `"interactive"` from `domInteractive`
/// (so deferred scripts and `DOMContentLoaded` listeners see it), `"complete"`
/// from `domComplete` — the `load` event fires with it already set.
pub(crate) fn ready_state(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    if !is_page_document(cx, this) {
        // Nothing is ever loading in a document with no browsing context.
        return Ok("complete".to_owned());
    }
    Ok(cx.state.ready_state().as_str().to_owned())
}

pub(crate) fn onreadystatechange(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    Ok(crate::imp::event_handler(cx, this, "readystatechange"))
}

pub(crate) fn set_onreadystatechange(
    cx: &BindCx<'_>,
    this: NodeId,
    value: JsValue,
) -> Result<(), JsThrow> {
    crate::imp::set_event_handler(cx, this, "readystatechange", value);
    Ok(())
}

pub(crate) fn content_type(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(cx
        .state
        .dom
        .borrow()
        .document_data(this)
        .map_or_else(|| "text/html".to_owned(), |d| d.content_type.clone()))
}

pub(crate) fn doctype(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    let dom = cx.state.dom.borrow();
    Ok(dom
        .children(this)
        .find(|&c| dom.node(c).data().kind() == NodeKind::Doctype))
}

pub(crate) fn document_element(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(cx.state.dom.borrow().document_element_of(this))
}

pub(crate) use crate::imp::interaction::active_element;

pub(crate) fn get_elements_by_tag_name(
    cx: &BindCx<'_>,
    this: NodeId,
    name: String,
) -> Result<JsValue, JsThrow> {
    super::element::by_tag_name(cx, this, name)
}

pub(crate) fn get_elements_by_tag_name_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    local_name: String,
) -> Result<JsValue, JsThrow> {
    super::element::by_tag_name_ns(cx, this, namespace, local_name)
}

pub(crate) fn get_elements_by_class_name(
    cx: &BindCx<'_>,
    this: NodeId,
    classes: String,
) -> Result<JsValue, JsThrow> {
    super::element::by_class_name(cx, this, classes)
}

/// Spec `createElement`. The two document-dependent rules:
///
/// * the name is ASCII-lowercased **only** in an HTML document, and
/// * the namespace is HTML only for an HTML document or an XHTML content type;
///   in a plain XML document (`new Document()`) it is **null**.
///
/// So `xmlDoc.createElement("DIV")` keeps its case and gets the `Element`
/// prototype, while `document.createElement("DIV")` is an `HTMLDivElement`.
pub(crate) fn create_element(
    cx: &BindCx<'_>,
    this: NodeId,
    local: String,
    _options: JsValue,
) -> Result<NodeId, JsThrow> {
    validate(cx, NameKind::Element, &local)?;
    let (is_html_document, html_namespace) = {
        let dom = cx.state.dom.borrow();
        let is_html = dom.is_html_document(this);
        let xhtml = dom
            .document_data(this)
            .is_some_and(|d| d.content_type == "application/xhtml+xml");
        (is_html, is_html || xhtml)
    };
    let local = if is_html_document {
        LocalName::from(local.to_ascii_lowercase())
    } else {
        LocalName::from(local)
    };
    let name = if html_namespace {
        html_name(local.clone())
    } else {
        QualName::new(None, Namespace::from(""), local.clone())
    };
    let element = {
        let mut dom = cx.state.dom.borrow_mut();
        let element = dom.create_element_in(this, name, Vec::new());
        if html_namespace {
            if &*local == "template" {
                dom.ensure_template_contents(element);
            } else if &*local == "script" {
                dom.set_script_force_async(element, true);
            }
        }
        element
    };
    // Spec: create a custom element with the synchronous flag — if the name is
    // defined, run its constructor now rather than deferring to the reaction
    // queue. Only a document with a browsing context has definitions to look
    // up, so a second document never upgrades (and never strands a strong
    // wrapper in `custom_wrappers` keyed by a node that document outlives).
    if is_page_document(cx, this) {
        crate::upgrade_element(cx, element);
    }
    Ok(element)
}

pub(crate) fn create_element_ns(
    cx: &BindCx<'_>,
    this: NodeId,
    namespace: Option<String>,
    qualified: String,
) -> Result<NodeId, JsThrow> {
    let (prefix, local) =
        validate_and_extract(cx, NameKind::Element, namespace.as_deref(), &qualified)?;
    let ns = namespace.filter(|ns| !ns.is_empty()).unwrap_or_default();
    let name = QualName::new(
        prefix.map(Prefix::from),
        Namespace::from(ns),
        LocalName::from(local),
    );
    let (element, is_html) = {
        let mut dom = cx.state.dom.borrow_mut();
        let element = dom.create_element_in(this, name, Vec::new());
        let is_html = dom
            .node(element)
            .as_element()
            .is_some_and(|el| el.is_html_element());
        if is_html
            && dom
                .node(element)
                .as_element()
                .is_some_and(|el| &*el.name.local == "script")
        {
            dom.set_script_force_async(element, true);
        }
        (element, is_html)
    };
    // Spec: create an element with the synchronous custom elements flag set, so
    // a defined custom element in the HTML namespace runs its constructor now
    // rather than deferring to the reaction queue. Only HTML-namespace elements
    // can be custom, and only a document with a browsing context has
    // definitions to find.
    if is_html && is_page_document(cx, this) {
        crate::upgrade_element(cx, element);
    }
    Ok(element)
}

pub(crate) fn create_document_fragment(cx: &BindCx<'_>, this: NodeId) -> Result<NodeId, JsThrow> {
    Ok(cx.state.dom.borrow_mut().create_document_fragment_in(this))
}

pub(crate) fn create_text_node(
    cx: &BindCx<'_>,
    this: NodeId,
    data: String,
) -> Result<NodeId, JsThrow> {
    Ok(cx.state.dom.borrow_mut().create_text_in(this, data.into()))
}

/// Spec `createCDATASection`: `NotSupportedError` in an HTML document (a
/// CDATA section cannot occur in HTML), `InvalidCharacterError` if the data
/// would close the section early.
pub(crate) fn create_cdata_section(
    cx: &BindCx<'_>,
    this: NodeId,
    data: String,
) -> Result<NodeId, JsThrow> {
    if cx.state.dom.borrow().is_html_document(this) {
        return Err(cx.dom_throw(
            DomExceptionKind::NotSupportedError,
            "createCDATASection on an HTML document",
        ));
    }
    if data.contains("]]>") {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidCharacterError,
            "CDATA section data must not contain \"]]>\"",
        ));
    }
    Ok(cx
        .state
        .dom
        .borrow_mut()
        .create_cdata_section_in(this, data.into()))
}

pub(crate) fn create_comment(
    cx: &BindCx<'_>,
    this: NodeId,
    data: String,
) -> Result<NodeId, JsThrow> {
    Ok(cx
        .state
        .dom
        .borrow_mut()
        .create_comment_in(this, data.into()))
}

pub(crate) fn create_processing_instruction(
    cx: &BindCx<'_>,
    this: NodeId,
    target: String,
    data: String,
) -> Result<NodeId, JsThrow> {
    // The one entry point still held to the strict XML `Name` production.
    validate_xml_name(cx, &target)?;
    if data.contains("?>") {
        return Err(cx.dom_throw(
            DomExceptionKind::InvalidCharacterError,
            "processing instruction data must not contain \"?>\"",
        ));
    }
    Ok(cx
        .state
        .dom
        .borrow_mut()
        .create_processing_instruction_in(this, target.into(), data.into()))
}

/// Spec `importNode`: clone `node` **into this document**, leaving the source
/// where it is. A Document or a ShadowRoot cannot be imported.
pub(crate) fn import_node(
    cx: &BindCx<'_>,
    this: NodeId,
    node: NodeId,
    deep: bool,
) -> Result<NodeId, JsThrow> {
    let mut dom = cx.state.dom.borrow_mut();
    if dom.is_shadow_root(node) {
        return Err(cx.dom_throw(
            DomExceptionKind::NotSupportedError,
            "cannot import a ShadowRoot",
        ));
    }
    dom.clone_subtree_into(node, deep, this)
        .map_err(|e| cx.dom_exception(e))
}

/// Spec `adoptNode`: move `node` into this document, detaching it from its old
/// parent. No longer "just removal" — the node's whole subtree changes node
/// document, and its owner pin moves with it.
pub(crate) fn adopt_node(cx: &BindCx<'_>, this: NodeId, node: NodeId) -> Result<NodeId, JsThrow> {
    let mut dom = cx.state.dom.borrow_mut();
    if dom.node(node).data().kind() == NodeKind::Document {
        return Err(cx.dom_throw(
            DomExceptionKind::NotSupportedError,
            "cannot adopt a Document",
        ));
    }
    if dom.is_shadow_root(node) {
        return Err(cx.dom_throw(
            DomExceptionKind::HierarchyRequestError,
            "cannot adopt a ShadowRoot",
        ));
    }
    dom.adopt(node, this);
    Ok(node)
}

pub(crate) fn create_event(
    cx: &BindCx<'_>,
    _this: NodeId,
    interface: String,
) -> Result<JsValue, JsThrow> {
    // DOM's `createEvent` table, restricted to the interfaces this engine has.
    // The legacy plural aliases are part of it — `"MouseEvents"` is what every
    // pre-constructor test and library passes, and it is the only spelling some
    // of them know.
    let (iface, ui) = match interface.to_ascii_lowercase().as_str() {
        "event" | "events" | "htmlevents" | "svgevents" => ("Event", None),
        "customevent" => ("CustomEvent", None),
        "uievent" | "uievents" => ("UIEvent", Some(UiKind::Plain)),
        "mouseevent" | "mouseevents" => ("MouseEvent", Some(UiKind::Mouse(Box::default()))),
        "keyboardevent" => ("KeyboardEvent", Some(UiKind::Keyboard(Box::default()))),
        "focusevent" => ("FocusEvent", Some(UiKind::Focus { related: None })),
        // The spec maps `"textevent"` to CompositionEvent, not to a `TextEvent`
        // interface — there is none.
        "compositionevent" | "textevent" => (
            "CompositionEvent",
            Some(UiKind::Composition {
                data: String::new(),
            }),
        ),
        _ => {
            return Err(cx.dom_throw(
                DomExceptionKind::NotSupportedError,
                "unsupported event interface",
            ));
        }
    };
    let mut data = EventData::uninitialized();
    data.time_stamp = cx.now_ms();
    // A `createEvent` event is uninitialized but *not* payload-less: its
    // getters must answer their defaults before `initUIEvent` is called, and a
    // missing payload would make them throw instead.
    if let Some(kind) = ui {
        data.ui = Some(Box::new(UiPayload::new(kind)));
    }
    let (value, _) = cx.new_event_object(iface, data)?;
    Ok(value)
}

// === HTML additions ===

/// The `Window`, but only for the document that *has* a browsing context.
pub(crate) fn default_view(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    if !is_page_document(cx, this) {
        return Ok(JsValue::Null);
    }
    let js = cx.state.js.borrow();
    Ok(match js.as_ref() {
        Some(refs) => JsValue::Object(refs.global.clone()),
        None => JsValue::Null,
    })
}

pub(crate) fn current_script(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    if !is_page_document(cx, this) {
        // No script ever runs in a document without a browsing context.
        return Ok(None);
    }
    Ok(cx.state.current_script.get())
}

fn title_element(cx: &BindCx<'_>, document: NodeId) -> Option<NodeId> {
    let dom = cx.state.dom.borrow();
    dom.inclusive_descendants(document).find(|&id| {
        dom.node(id)
            .as_element()
            .is_some_and(|el| el.is_html_element() && &*el.name.local == "title")
    })
}

pub(crate) fn title(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    let Some(title) = title_element(cx, this) else {
        return Ok(String::new());
    };
    let text = cx.state.dom.borrow().text_content(title);
    // Spec: strip and collapse ASCII whitespace.
    Ok(text.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
}

pub(crate) fn set_title(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    let title = match title_element(cx, this) {
        Some(title) => title,
        None => {
            // Create <title> in <head> when a head exists; otherwise no-op.
            let Some(head) = head(cx, this)? else {
                return Ok(());
            };
            let mut dom = cx.state.dom.borrow_mut();
            let title =
                dom.create_element_in(this, html_name(LocalName::from("title")), Vec::new());
            dom.append_child(head, title)
                .map_err(|e| cx.dom_exception(e))?;
            title
        }
    };
    super::node::replace_all_with_text(cx, title, &value)
}

pub(crate) fn html_child_of_root(
    cx: &BindCx<'_>,
    document: NodeId,
    names: &[&str],
) -> Option<NodeId> {
    let dom = cx.state.dom.borrow();
    let root = dom.document_element_of(document)?;
    dom.children(root).find(|&c| {
        dom.node(c)
            .as_element()
            .is_some_and(|el| el.is_html_element() && names.contains(&&*el.name.local))
    })
}

pub(crate) fn body(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(html_child_of_root(cx, this, &["body", "frameset"]))
}

pub(crate) fn head(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    Ok(html_child_of_root(cx, this, &["head"]))
}

// === CSSOM-View geometry (Phase 5) ===

/// Hit testing is a property of the viewport. A second document has none, so
/// these return empty **without flushing layout** — reflowing the page for a
/// document that is not in it would be wrong and expensive.
pub(crate) fn element_from_point(
    cx: &BindCx<'_>,
    this: NodeId,
    x: f64,
    y: f64,
) -> Result<Option<NodeId>, JsThrow> {
    if !is_page_document(cx, this) {
        return Ok(None);
    }
    Ok(crate::imp::geometry_support::flush_layout(
        cx,
        |dom, layout| layout.element_from_point(dom, x as f32, y as f32),
    ))
}

pub(crate) fn elements_from_point(
    cx: &BindCx<'_>,
    this: NodeId,
    x: f64,
    y: f64,
) -> Result<JsValue, JsThrow> {
    if !is_page_document(cx, this) {
        return cx
            .scope
            .new_array(&[])
            .map(JsValue::Object)
            .map_err(JsThrow::from);
    }
    let hits = crate::imp::geometry_support::flush_layout(cx, |dom, layout| {
        layout.elements_from_point(dom, x as f32, y as f32)
    });
    let wrapped = hits
        .into_iter()
        .map(|node| cx.node_to_js(node))
        .collect::<Result<Vec<JsValue>, JsThrow>>()?;
    cx.scope
        .new_array(&wrapped)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

/// `document.scrollingElement`. Standards mode: always the document element.
/// Quirks mode: the body element, unless it is "potentially scrollable"
/// (CSSOM-View) — the root element's own overflow is the initial `visible`
/// on both axes (so it propagates the body's overflow to the viewport,
/// leaving the body's own box `overflow: visible`), or, when the root's
/// overflow is *not* visible (no propagation), the body's own overflow is
/// visible on both axes. Otherwise `null` (nothing propagates the body's
/// scroll to the viewport, and the body itself no longer represents it).
pub(crate) fn scrolling_element(cx: &BindCx<'_>, this: NodeId) -> Result<Option<NodeId>, JsThrow> {
    if !is_page_document(cx, this) {
        // No viewport, and layout must not be flushed for a document outside it.
        return Ok(None);
    }
    if cx.state.dom.borrow().quirks_mode_of(this) != QuirksMode::Quirks {
        return Ok(cx.state.dom.borrow().document_element_of(this));
    }
    let Some(body) = html_child_of_root(cx, this, &["body", "frameset"]) else {
        return Ok(None);
    };
    let potentially_scrollable = crate::imp::geometry_support::flush_layout(cx, |dom, layout| {
        let root_visible = dom
            .document_element()
            .and_then(|root| layout.overflow_is_visible(root))
            .unwrap_or(true);
        if root_visible {
            // Propagation: the root's overflow governs the viewport, and the
            // body's own box behaves as `overflow: visible`.
            false
        } else {
            layout
                .overflow_is_visible(body)
                .is_some_and(|visible| !visible)
        }
    });
    Ok((!potentially_scrollable).then_some(body))
}

/// `[SameObject] readonly attribute any fonts;` (CSS Font Loading). The page
/// document's `FontFaceSet` is the engine's one real font state, cached in a
/// `PageState` cell so `document.fonts === document.fonts` holds.
///
/// A second document gets its own `[SameObject]` wrapper (so identity still
/// holds per document) over that same state — font loading is a property of
/// the browsing context this document does not have. Documented in ADR-0017
/// rather than faked as a separate, always-empty set.
pub(crate) fn fonts(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    if !is_page_document(cx, this) {
        return cx.same_object(this, "fonts", |cx| cx.new_font_face_set());
    }
    if let Some(cached) = cx.state.font_face_set_js.borrow().clone() {
        return Ok(cached);
    }
    let value = cx.new_font_face_set()?;
    *cx.state.font_face_set_js.borrow_mut() = Some(value.clone());
    Ok(value)
}

pub(crate) fn adopted_style_sheets(cx: &BindCx<'_>, this: NodeId) -> Result<JsValue, JsThrow> {
    super::shadow_root::adopted_sheets_get(cx, this)
}

pub(crate) fn set_adopted_style_sheets(
    cx: &BindCx<'_>,
    this: NodeId,
    value: JsValue,
) -> Result<(), JsThrow> {
    super::shadow_root::adopted_sheets_set(cx, this, value)
}
