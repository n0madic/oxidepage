//! The DOM's name productions, and the algorithms that throw on them.
//!
//! Three *different* rules are in play, and `tests/wpt/vendor/dom/nodes`
//! pins each one:
//!
//! * `createProcessingInstruction` still holds its target to the strict XML
//!   `Name` production — `Document-createProcessingInstruction.js` requires
//!   `A\u{B7}A` to pass and `\u{B7}A` / `A\u{D7}` to throw, which only the real
//!   character classes get right.
//! * Element names use a looser rule the DOM Standard adopted so that every
//!   name the HTML parser can produce is also creatable from script:
//!   `Document-createElement.html` lists `f<oo`, `f}oo` and `\u{300}` as valid.
//! * Attribute names are looser still: `productions.js` lists `0`, `~`, `"` and
//!   `\` as valid attribute names.
//!
//! Only the `validate*` entry points throw; the predicates are pure.

use oxidepage_base::DomExceptionKind;
use oxidepage_dom::QualName;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

pub(crate) const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
pub(crate) const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Which production a name is held to. Elements and attributes differ only in
/// what they accept as a local name, so this is the whole difference between
/// `createElementNS` and `setAttributeNS`.
#[derive(Clone, Copy)]
pub(crate) enum NameKind {
    Element,
    Attribute,
}

impl NameKind {
    fn local_name_ok(self, local: &str) -> bool {
        match self {
            Self::Element => is_valid_element_local_name(local),
            Self::Attribute => is_valid_attribute_local_name(local),
        }
    }
}

/// The qualified name of a node or attribute: `prefix:local`, or bare `local`
/// when there is no prefix.
pub(crate) fn qualified_name(name: &QualName) -> String {
    match &name.prefix {
        Some(prefix) => format!("{prefix}:{}", name.local),
        None => name.local.to_string(),
    }
}

/// The code points that would not survive a round trip through the HTML
/// serializer inside a tag name or an attribute name.
fn breaks_serialization(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ' | '\0' | '/' | '>')
}

/// A valid element local name: non-empty, opening with an ASCII letter, `:`,
/// `_`, or any non-ASCII code point.
fn is_valid_element_local_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let start_ok = first.is_ascii_alphabetic() || matches!(first, ':' | '_') || first > '\u{7f}';
    start_ok && !name.chars().any(breaks_serialization)
}

/// A valid attribute local name. Unlike an element name this has no constraint
/// on its first character — `0` and `~` are legal attribute names.
fn is_valid_attribute_local_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| breaks_serialization(c) || c == '=')
}

/// A valid namespace prefix. Callers split on the *first* `:`, so a prefix can
/// never contain one.
fn is_valid_namespace_prefix(prefix: &str) -> bool {
    !prefix.is_empty() && !prefix.chars().any(breaks_serialization)
}

/// XML `NameStartChar`.
fn is_xml_name_start(c: char) -> bool {
    matches!(c,
        ':' | '_'
        | 'A'..='Z'
        | 'a'..='z'
        | '\u{C0}'..='\u{D6}'
        | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}'
        | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}'
        | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

/// XML `NameChar`.
fn is_xml_name_char(c: char) -> bool {
    is_xml_name_start(c)
        || matches!(c,
            '-' | '.'
            | '0'..='9'
            | '\u{B7}'
            | '\u{300}'..='\u{36F}'
            | '\u{203F}'..='\u{2040}')
}

fn invalid_character(cx: &BindCx<'_>, name: &str) -> JsThrow {
    cx.dom_throw(
        DomExceptionKind::InvalidCharacterError,
        &format!("`{name}` is not a valid name"),
    )
}

/// The XML `Name` production.
pub(crate) fn validate_xml_name(cx: &BindCx<'_>, name: &str) -> Result<(), JsThrow> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(is_xml_name_start) && chars.all(is_xml_name_char);
    if ok {
        Ok(())
    } else {
        Err(invalid_character(cx, name))
    }
}

/// The XML `NCName` production: a `Name` containing no colon.
fn is_xml_ncname(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c != ':' && is_xml_name_start(c))
        && chars.all(|c| c != ':' && is_xml_name_char(c))
}

/// The XML `QName` production: `NCName` or `NCName:NCName`.
///
/// `createDocumentType` validates against this and nothing else — there is no
/// namespace to check a prefix against. It is *stricter* than the element-name
/// check used by `createElement`, which is deliberately HTML-lenient (it lets
/// `createElement("f:o:o")` through).
pub(crate) fn validate_qname(cx: &BindCx<'_>, name: &str) -> Result<(), JsThrow> {
    let ok = match name.split_once(':') {
        Some((prefix, local)) => is_xml_ncname(prefix) && is_xml_ncname(local),
        None => is_xml_ncname(name),
    };
    if ok {
        Ok(())
    } else {
        Err(invalid_character(cx, name))
    }
}

/// The DOM's "validate" algorithm, for the namespace-less entry points
/// (`createElement`, `setAttribute`, `toggleAttribute`). The whole string is
/// the local name — `createElement("f:o:o")` really does make an element whose
/// local name contains colons.
pub(crate) fn validate(cx: &BindCx<'_>, kind: NameKind, name: &str) -> Result<(), JsThrow> {
    if kind.local_name_ok(name) {
        Ok(())
    } else {
        Err(invalid_character(cx, name))
    }
}

/// The DOM's "validate and extract" algorithm: split `qualified_name` on its
/// first `:` and check both halves, then check the prefix against `namespace`.
///
/// Character errors are reported before namespace errors — WPT's
/// `Document-createElementNS.js` pins that order (`xmlns` namespace + `1foo`
/// is an `InvalidCharacterError`, not a `NamespaceError`).
pub(crate) fn validate_and_extract(
    cx: &BindCx<'_>,
    kind: NameKind,
    namespace: Option<&str>,
    qualified_name: &str,
) -> Result<(Option<String>, String), JsThrow> {
    let namespace = namespace.filter(|ns| !ns.is_empty());
    let (prefix, local) = match qualified_name.split_once(':') {
        Some((prefix, local)) => {
            if !is_valid_namespace_prefix(prefix) {
                return Err(invalid_character(cx, qualified_name));
            }
            (Some(prefix.to_owned()), local.to_owned())
        }
        None => (None, qualified_name.to_owned()),
    };
    if !kind.local_name_ok(&local) {
        return Err(invalid_character(cx, qualified_name));
    }

    let ns_error = |msg: &str| cx.dom_throw(DomExceptionKind::NamespaceError, msg);
    if prefix.is_some() && namespace.is_none() {
        return Err(ns_error("a prefixed name needs a namespace"));
    }
    if prefix.as_deref() == Some("xml") && namespace != Some(XML_NS) {
        return Err(ns_error("the `xml` prefix is bound to the XML namespace"));
    }
    if (qualified_name == "xmlns" || prefix.as_deref() == Some("xmlns"))
        && namespace != Some(XMLNS_NS)
    {
        return Err(ns_error("`xmlns` is bound to the XMLNS namespace"));
    }
    if namespace == Some(XMLNS_NS)
        && qualified_name != "xmlns"
        && prefix.as_deref() != Some("xmlns")
    {
        return Err(ns_error("the XMLNS namespace is only for `xmlns` names"));
    }
    Ok((prefix, local))
}
