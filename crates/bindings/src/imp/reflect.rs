//! Shared IDL-attribute reflection helpers.
//!
//! Every per-tag HTML element interface is mostly a pile of content-attribute
//! reflectors; these are the primitives, plus macros that emit a getter/setter
//! pair for the common `DOMString` / nullable `DOMString` / `boolean` / URL
//! cases.

use oxidepage_base::NodeId;
use oxidepage_dom::LocalName;
use oxidepage_dom::node::attr_name;

use crate::cx::BindCx;

pub(crate) fn attr(name: &str) -> oxidepage_dom::QualName {
    attr_name(LocalName::from(name))
}

pub(crate) fn reflect_string(cx: &BindCx<'_>, this: NodeId, name: &str) -> String {
    cx.state
        .dom
        .borrow()
        .node(this)
        .as_element()
        .and_then(|el| el.attr(&attr(name)))
        .map(ToString::to_string)
        .unwrap_or_default()
}

pub(crate) fn set_string(cx: &BindCx<'_>, this: NodeId, name: &str, value: String) {
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(this, attr(name), value.into());
}

pub(crate) fn reflect_bool(cx: &BindCx<'_>, this: NodeId, name: &str) -> bool {
    cx.state
        .dom
        .borrow()
        .node(this)
        .as_element()
        .is_some_and(|el| el.attr(&attr(name)).is_some())
}

pub(crate) fn set_bool(cx: &BindCx<'_>, this: NodeId, name: &str, value: bool) {
    let mut dom = cx.state.dom.borrow_mut();
    if value {
        dom.set_attribute(this, attr(name), "".into());
    } else {
        dom.remove_attribute(this, &attr(name));
    }
}

/// Reflects a nullable `DOMString` attribute (`null` when absent), the shape
/// `crossOrigin` and friends use.
pub(crate) fn reflect_nullable_string(cx: &BindCx<'_>, this: NodeId, name: &str) -> Option<String> {
    cx.state
        .dom
        .borrow()
        .node(this)
        .as_element()
        .and_then(|el| el.attr(&attr(name)))
        .map(ToString::to_string)
}

pub(crate) fn set_nullable_string(
    cx: &BindCx<'_>,
    this: NodeId,
    name: &str,
    value: Option<String>,
) {
    let mut dom = cx.state.dom.borrow_mut();
    match value {
        Some(value) => dom.set_attribute(this, attr(name), value.into()),
        None => {
            dom.remove_attribute(this, &attr(name));
        }
    }
}

/// Reflects a URL attribute: the absolute URL obtained by resolving the
/// attribute against the document base URL. Absent → `""`; present but
/// unresolvable → the raw attribute value (HTML "reflect a URL attribute").
pub(crate) fn reflect_url(cx: &BindCx<'_>, this: NodeId, name: &str) -> String {
    let (raw, base) = {
        let dom = cx.state.dom.borrow();
        let Some(raw) = dom
            .node(this)
            .as_element()
            .and_then(|el| el.attr(&attr(name)))
            .map(ToString::to_string)
        else {
            return String::new();
        };
        // The base URL of the document **this node is in**, not the page's.
        // `img.src` / `a.href` written inside a frame are relative to that
        // frame's document, and resolving them against the embedder's reported
        // a URL from another origin — one that disagreed with the URL
        // `page::resolve_url_for` actually fetched (ADR-0035 D1).
        let doc = dom
            .containing_document(this)
            .unwrap_or_else(|| dom.document());
        (raw, dom.base_url_of(doc))
    };
    url::Url::parse(&base)
        .and_then(|base| base.join(&raw))
        .map_or(raw, |url| url.to_string())
}

/// Reflects an `unsigned long` content attribute; a missing or malformed value
/// is `0`.
pub(crate) fn reflect_u32(cx: &BindCx<'_>, this: NodeId, name: &str) -> u32 {
    reflect_string(cx, this, name).trim().parse().unwrap_or(0)
}

pub(crate) fn set_u32(cx: &BindCx<'_>, this: NodeId, name: &str, value: u32) {
    set_string(cx, this, name, value.to_string());
}

/// Emits a `DOMString` reflector pair for the content attribute `$name`.
macro_rules! string_reflector {
    ($get:ident, $set:ident, $name:literal) => {
        pub(crate) fn $get(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
        ) -> Result<String, oxidepage_js::JsThrow> {
            Ok($crate::imp::reflect::reflect_string(cx, this, $name))
        }

        pub(crate) fn $set(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
            value: String,
        ) -> Result<(), oxidepage_js::JsThrow> {
            $crate::imp::reflect::set_string(cx, this, $name, value);
            Ok(())
        }
    };
}

/// Emits a `boolean` reflector pair (presence of the content attribute).
macro_rules! bool_reflector {
    ($get:ident, $set:ident, $name:literal) => {
        pub(crate) fn $get(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
        ) -> Result<bool, oxidepage_js::JsThrow> {
            Ok($crate::imp::reflect::reflect_bool(cx, this, $name))
        }

        pub(crate) fn $set(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
            value: bool,
        ) -> Result<(), oxidepage_js::JsThrow> {
            $crate::imp::reflect::set_bool(cx, this, $name, value);
            Ok(())
        }
    };
}

/// Emits a nullable `DOMString` reflector pair (`null` when absent).
macro_rules! nullable_string_reflector {
    ($get:ident, $set:ident, $name:literal) => {
        pub(crate) fn $get(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
        ) -> Result<Option<String>, oxidepage_js::JsThrow> {
            Ok($crate::imp::reflect::reflect_nullable_string(
                cx, this, $name,
            ))
        }

        pub(crate) fn $set(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
            value: Option<String>,
        ) -> Result<(), oxidepage_js::JsThrow> {
            $crate::imp::reflect::set_nullable_string(cx, this, $name, value);
            Ok(())
        }
    };
}

/// Emits an `unsigned long` reflector pair (`<textarea rows>`, `<select size>`).
macro_rules! u32_reflector {
    ($get:ident, $set:ident, $name:literal) => {
        pub(crate) fn $get(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
        ) -> Result<f64, oxidepage_js::JsThrow> {
            Ok(f64::from($crate::imp::reflect::reflect_u32(
                cx, this, $name,
            )))
        }

        pub(crate) fn $set(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
            value: u32,
        ) -> Result<(), oxidepage_js::JsThrow> {
            $crate::imp::reflect::set_u32(cx, this, $name, value);
            Ok(())
        }
    };
}

/// Emits a URL reflector pair: the getter resolves against the document base
/// URL, the setter writes the raw string.
macro_rules! url_reflector {
    ($get:ident, $set:ident, $name:literal) => {
        pub(crate) fn $get(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
        ) -> Result<String, oxidepage_js::JsThrow> {
            Ok($crate::imp::reflect::reflect_url(cx, this, $name))
        }

        pub(crate) fn $set(
            cx: &$crate::cx::BindCx<'_>,
            this: oxidepage_base::NodeId,
            value: String,
        ) -> Result<(), oxidepage_js::JsThrow> {
            $crate::imp::reflect::set_string(cx, this, $name, value);
            Ok(())
        }
    };
}

pub(crate) use {
    bool_reflector, nullable_string_reflector, string_reflector, u32_reflector, url_reflector,
};
