//! `DOMImplementation` (`document.implementation`).
//!
//! Not node-backed: a slab object carrying the document it was minted for, so a
//! saved `implementation` keeps creating documents against *its* document, as
//! `DOMImplementation-createHTMLDocument-with-saved-implementation.html`
//! requires.

use oxidepage_base::NodeId;
use oxidepage_dom::node::{DocumentData, html_name};
use oxidepage_dom::{LocalName, Namespace, Prefix, QualName};
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::imp::names::{NameKind, validate_and_extract, validate_qname};

const HTML_NS: &str = "http://www.w3.org/1999/xhtml";
const SVG_NS: &str = "http://www.w3.org/2000/svg";

/// Spec `createDocumentType`. The doctype is owned by `this`'s document — WPT
/// checks `doctype.ownerDocument`, and an unowned node would dangle.
pub(crate) fn create_document_type(
    cx: &BindCx<'_>,
    this: NodeId,
    name: String,
    public_id: String,
    system_id: String,
) -> Result<NodeId, JsThrow> {
    validate_qname(cx, &name)?;
    Ok(cx.state.dom.borrow_mut().create_doctype_in(
        this,
        name.into(),
        public_id.into(),
        system_id.into(),
    ))
}

/// Spec `createDocument`: a new **XMLDocument**, optionally with a doctype and
/// a document element.
///
/// The content type follows the namespace, which is what `document.contentType`
/// on the result reports.
pub(crate) fn create_document(
    cx: &BindCx<'_>,
    _this: NodeId,
    namespace: Option<String>,
    qualified_name: Option<String>,
    doctype: Option<NodeId>,
) -> Result<NodeId, JsThrow> {
    let namespace = namespace.filter(|ns| !ns.is_empty());
    // A null or empty qualified name means "no document element" — both, which
    // is why the IDL takes `DOMString?` rather than faking
    // [LegacyNullToEmptyString].
    let qualified_name = qualified_name.filter(|n| !n.is_empty());

    // Validate before creating anything, so a throw leaves no orphan document.
    let name = match &qualified_name {
        Some(qualified) => {
            let (prefix, local) =
                validate_and_extract(cx, NameKind::Element, namespace.as_deref(), qualified)?;
            Some(QualName::new(
                prefix.map(Prefix::from),
                Namespace::from(namespace.clone().unwrap_or_default()),
                LocalName::from(local),
            ))
        }
        None => None,
    };

    let content_type = match namespace.as_deref() {
        Some(HTML_NS) => "application/xhtml+xml",
        Some(SVG_NS) => "image/svg+xml",
        _ => "application/xml",
    };

    let document = {
        let mut dom = cx.state.dom.borrow_mut();
        let document = dom.create_document(DocumentData::xml(
            "about:blank".to_owned(),
            content_type.to_owned(),
            /* xml_document_interface */ true,
        ));
        if let Some(doctype) = doctype {
            // Adopt first: the doctype may belong to another document, and the
            // spec requires `doctype.ownerDocument === doc` afterwards.
            dom.adopt(doctype, document);
            dom.append_child(document, doctype)
                .map_err(|e| cx.dom_exception(e))?;
        }
        if let Some(name) = name {
            let root = dom.create_element_in(document, name, Vec::new());
            dom.append_child(document, root)
                .map_err(|e| cx.dom_exception(e))?;
        }
        document
    };
    Ok(document)
}

/// Spec `createHTMLDocument`: built from the spec's steps directly rather than
/// by parsing a string, so it cannot drift from them.
pub(crate) fn create_html_document(
    cx: &BindCx<'_>,
    _this: NodeId,
    title: Option<String>,
) -> Result<NodeId, JsThrow> {
    let document = {
        let mut dom = cx.state.dom.borrow_mut();
        // A freshly created document's URL is `about:blank`; it does not inherit
        // the creating document's (WPT `Node-properties.html` pins this).
        let document = dom.create_document(DocumentData::html("about:blank".to_owned()));

        let doctype = dom.create_doctype_in(document, "html".into(), "".into(), "".into());
        dom.append_child(document, doctype)
            .map_err(|e| cx.dom_exception(e))?;

        let html = dom.create_element_in(document, html_name(LocalName::from("html")), Vec::new());
        dom.append_child(document, html)
            .map_err(|e| cx.dom_exception(e))?;

        let head = dom.create_element_in(document, html_name(LocalName::from("head")), Vec::new());
        dom.append_child(html, head)
            .map_err(|e| cx.dom_exception(e))?;

        // `createHTMLDocument()` with no argument creates no <title> at all;
        // `createHTMLDocument("")` creates an empty one.
        if let Some(title) = title {
            let title_el =
                dom.create_element_in(document, html_name(LocalName::from("title")), Vec::new());
            dom.append_child(head, title_el)
                .map_err(|e| cx.dom_exception(e))?;
            if !title.is_empty() {
                let text = dom.create_text_in(document, title.into());
                dom.append_child(title_el, text)
                    .map_err(|e| cx.dom_exception(e))?;
            }
        }

        let body = dom.create_element_in(document, html_name(LocalName::from("body")), Vec::new());
        dom.append_child(html, body)
            .map_err(|e| cx.dom_exception(e))?;

        document
    };
    Ok(document)
}

/// Spec: "useless; always returns true".
pub(crate) fn has_feature(_cx: &BindCx<'_>, _this: NodeId) -> Result<bool, JsThrow> {
    Ok(true)
}
