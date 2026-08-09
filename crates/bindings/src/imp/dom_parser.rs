//! `DOMParser`.
//!
//! Replaces the `bootstrap.js` shim that parsed everything as an `innerHTML`
//! fragment into a fake document object (ADR-0012 D4). `parseFromString` now
//! builds a **real** second Document and runs the real full-document parse into
//! it, so head-level content lands in `<head>` instead of being foster-parented
//! or dropped.
//!
//! **There is no XML parser in this engine.** The XML content types are parsed
//! with the HTML parser into a document carrying the requested content type —
//! strictly more capable than what the shim did, and recorded as a deviation in
//! ADR-0017 rather than hidden.

use oxidepage_base::NodeId;
use oxidepage_dom::node::DocumentData;
use oxidepage_dom::{ParseOptions, parser};
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::HostData;

pub(crate) fn constructor(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.new_slab_object("DOMParser", HostData::DomParser)
}

pub(crate) fn parse_from_string(
    cx: &BindCx<'_>,
    _this: u64,
    source: String,
    ty: String,
) -> Result<NodeId, JsThrow> {
    // The spec enumerates exactly these five; anything else is a TypeError
    // (not a DOMException), and feature-detecting code relies on that.
    // The parsed document inherits the URL of the realm that parsed it — this
    // frame's document, not the page's (ADR-0035 D1).
    let url = cx.state.frame.document_url();
    let data = match ty.as_str() {
        "text/html" => DocumentData::html(url),
        "text/xml" | "application/xml" | "application/xhtml+xml" | "image/svg+xml" => {
            DocumentData::xml(url, ty.clone(), /* xml_document_interface */ false)
        }
        _ => {
            return Err(JsThrow::Type(format!(
                "DOMParser.parseFromString: `{ty}` is not a supported type"
            )));
        }
    };

    let document = cx.state.dom.borrow_mut().create_document(data);
    // The borrow must be released before parsing: the sink borrows the tree
    // itself, repeatedly, for the length of the parse.
    parser::parse_into_document(
        &cx.state.dom,
        document,
        &source,
        ParseOptions {
            // A DOMParser document has no browsing context, so its scripts never
            // run — and `<noscript>` must parse as though scripting were off.
            scripting_enabled: false,
            ..ParseOptions::default()
        },
    );
    Ok(document)
}
