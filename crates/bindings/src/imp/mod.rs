//! Hand-written implementations behind the generated WebIDL glue.
//!
//! Module names mirror the IDL: one module per interface or mixin; function
//! names are the snake-cased member names (`set_*` for attribute setters,
//! `constructor` for IDL constructors). Signatures are dictated by the
//! generated callers — changing the IDL changes the expected signatures at
//! compile time, which is the drift protection codegen buys us.

pub(crate) mod abort_controller;
pub(crate) mod abort_signal;
pub(crate) mod attr;
pub(crate) mod body;
pub(crate) mod character_data;
pub(crate) mod child_node;
pub(crate) mod comment;
pub(crate) mod css_rule;
pub(crate) mod css_rule_list;
pub(crate) mod css_style_declaration;
pub(crate) mod css_style_rule;
pub(crate) mod css_style_sheet;
pub(crate) mod custom_element_registry;
pub(crate) mod custom_event;
pub(crate) mod document;
pub(crate) mod document_fragment;
pub(crate) mod document_type;
pub(crate) mod dom_implementation;
pub(crate) mod dom_parser;
pub(crate) mod dom_rect;
pub(crate) mod dom_rect_list;
pub(crate) mod dom_rect_read_only;
pub(crate) mod dom_token_list;
pub(crate) mod element;
pub(crate) mod event;
pub(crate) mod event_target;
pub(crate) mod font_face_set;
pub(crate) mod form_data;
pub(crate) mod form_submit;
pub(crate) mod form_support;
pub(crate) mod geometry_support;
pub(crate) mod headers;
pub(crate) mod history;
pub(crate) mod html_anchor_element;
pub(crate) mod html_area_element;
pub(crate) mod html_button_element;
pub(crate) mod html_collection;
pub(crate) mod html_element;
pub(crate) mod html_field_set_element;
pub(crate) mod html_form_element;
pub(crate) mod html_hyperlink_element_utils;
pub(crate) mod html_image_element;
pub(crate) mod html_input_element;
pub(crate) mod html_label_element;
pub(crate) mod html_link_element;
pub(crate) mod html_opt_group_element;
pub(crate) mod html_option_element;
pub(crate) mod html_script_element;
pub(crate) mod html_select_element;
pub(crate) mod html_slot_element;
pub(crate) mod html_template_element;
pub(crate) mod html_text_area_element;
pub(crate) mod interaction;
pub(crate) mod intersection_observer;
pub(crate) mod intersection_observer_entry;
pub(crate) mod location;
pub(crate) mod media_query_list;
pub(crate) mod mime_type_array;
pub(crate) mod mutation_observer;
pub(crate) mod mutation_record;
pub(crate) mod named_node_map;
pub(crate) mod names;
pub(crate) mod navigator;
pub(crate) mod node;
pub(crate) mod node_list;
pub(crate) mod non_document_type_child_node;
pub(crate) mod non_element_parent_node;
pub(crate) mod parent_node;
pub(crate) mod performance;
pub(crate) mod performance_timing;
pub(crate) mod plugin_array;
pub(crate) mod pop_state_event;
pub(crate) mod processing_instruction;
pub(crate) mod reflect;
pub(crate) mod request;
pub(crate) mod resize_observer;
pub(crate) mod resize_observer_entry;
pub(crate) mod response;
pub(crate) mod screen;
pub(crate) mod shadow_root;
pub(crate) mod style_sheet;
pub(crate) mod style_sheet_list;
pub(crate) mod submit_event;
pub(crate) mod svg_animated_string;
pub(crate) mod svg_element;
// The codegen derives the imp module name from `snake("SVGAElement")`.
#[path = "svg_a_element.rs"]
pub(crate) mod svga_element;
pub(crate) mod text;
pub(crate) mod url;
pub(crate) mod url_parts;
pub(crate) mod url_search_params;
pub(crate) mod window;
// The codegen derives the imp module name from `snake("XMLHttpRequest")`.
#[path = "xhr.rs"]
pub(crate) mod xml_http_request;

use oxidepage_base::NodeId;
use oxidepage_js::{JsThrow, JsValue};

use crate::collections::CollectionData;
use crate::cx::BindCx;
use crate::events::EventTargetKey;

/// Reads an event-handler IDL attribute (`onclick`, `onload`, ...). Absent
/// handlers read as `null`.
///
/// `this` is whatever the interface's `this`-unwrap produced — a `NodeId` for an
/// element or document, an [`EventTargetKey`] for the Window — so the generated
/// accessors are the same shape on every interface that has handlers.
///
/// A handler declared as a content attribute (`<img onload="…">`) reads back as
/// the function it compiles to, so markup and script agree on what is installed.
pub(crate) fn event_handler(
    cx: &BindCx<'_>,
    this: impl Into<EventTargetKey>,
    name: &str,
) -> JsValue {
    let key = crate::handlers::handler_key(cx, this.into(), name);
    crate::handlers::resolve(cx, key, name).unwrap_or(JsValue::Null)
}

/// Writes an event-handler IDL attribute. Assigning a non-function (`null`,
/// `undefined`, anything else) removes the handler, per the IDL.
pub(crate) fn set_event_handler(
    cx: &BindCx<'_>,
    this: impl Into<EventTargetKey>,
    name: &str,
    value: JsValue,
) {
    let target = crate::handlers::handler_key(cx, this.into(), name);
    let key = (target, name.to_owned());
    if cx.scope.is_function(&value) {
        cx.state.event_handlers.borrow_mut().insert(key, value);
    } else {
        cx.state.event_handlers.borrow_mut().remove(&key);
    }
    // The assignment supersedes the content attribute as it currently stands;
    // only a later edit of that attribute may replace this handler again.
    crate::handlers::mark_reflects_current_attribute(cx, target, name);
}

/// Resolves a collection slab key to its data snapshot items.
pub(crate) fn collection_items(cx: &BindCx<'_>, key: u64) -> Vec<NodeId> {
    let slab = cx.state.slab.borrow();
    match slab.get(key) {
        Some(crate::state::HostData::Collection(data)) => data.items(&cx.state.dom.borrow()),
        _ => Vec::new(),
    }
}

/// Resolves a `DOMTokenList` slab key to `(element, attribute local name)`.
pub(crate) fn token_list_parts(
    cx: &BindCx<'_>,
    key: u64,
) -> Result<(NodeId, oxidepage_dom::LocalName), JsThrow> {
    let slab = cx.state.slab.borrow();
    match slab.get(key) {
        Some(crate::state::HostData::Collection(CollectionData::TokenList { element, attr })) => {
            Ok((*element, attr.clone()))
        }
        _ => Err(JsThrow::Type("receiver is not a DOMTokenList".into())),
    }
}

/// Resolves a `DOMTokenList` slab key to its current tokens.
pub(crate) fn token_list_tokens(cx: &BindCx<'_>, key: u64) -> Vec<String> {
    let slab = cx.state.slab.borrow();
    match slab.get(key) {
        Some(crate::state::HostData::Collection(data)) => data.tokens(&cx.state.dom.borrow()),
        _ => Vec::new(),
    }
}
