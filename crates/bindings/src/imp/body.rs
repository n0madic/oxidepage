//! Extracting a request body (Fetch's "extract a body").
//!
//! One definition, shared by `fetch()` and `XMLHttpRequest.send()`, because a
//! body type that only one of them understood would be a trap: `FormData` would
//! reach the wire as the string `"[object FormData]"` through the other.
//!
//! Each body type carries a **default `Content-Type`**, which the caller applies
//! only if it did not set one itself. For `FormData` that is not a nicety: the
//! header has to name the multipart boundary, and only this code knows it.

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;

pub(crate) struct Body {
    pub bytes: Vec<u8>,
    /// The body's default `Content-Type`, if it has one.
    pub content_type: Option<String>,
}

/// Extracts a body from a JS value. `None` for a nullish value.
pub(crate) fn extract(cx: &BindCx<'_>, value: &JsValue) -> Result<Option<Body>, JsThrow> {
    if value.is_nullish() {
        return Ok(None);
    }

    // A `Blob`/`File` contributes its bytes, and its `type` becomes the default
    // `Content-Type` — but only when it has one. An empty `type` must leave the
    // header *absent* rather than send `Content-Type: `, which is why this
    // branch is the one body type whose `content_type` can be `None`.
    if let Some(blob) = cx.as_blob(value) {
        return Ok(Some(Body {
            bytes: blob.view().to_vec(),
            content_type: (!blob.type_.is_empty()).then(|| blob.type_.clone()),
        }));
    }

    if let Some(form) = cx.as_form_data(value) {
        let boundary = multipart_boundary()?;
        return Ok(Some(Body {
            bytes: form.to_multipart(&boundary),
            content_type: Some(format!("multipart/form-data; boundary={boundary}")),
        }));
    }

    if let Ok(params) = cx.this_url_search_params(value) {
        return Ok(Some(Body {
            bytes: params.serialize().into_bytes(),
            content_type: Some("application/x-www-form-urlencoded;charset=UTF-8".to_owned()),
        }));
    }

    // Everything else stringifies, as the spec's USVString branch does.
    Ok(Some(Body {
        bytes: cx
            .scope
            .coerce_string(value)
            .map_err(JsThrow::from)?
            .into_bytes(),
        content_type: Some("text/plain;charset=UTF-8".to_owned()),
    }))
}

/// A boundary that cannot occur in the payload it delimits: the fixed prefix
/// makes it recognisable, and the 128 random bits are what make a collision with
/// user data infeasible rather than merely unlikely (a guessable boundary lets
/// a hostile *value* close the part early and forge the rest of the body).
fn multipart_boundary() -> Result<String, JsThrow> {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| JsThrow::Type(format!("random source unavailable: {e}")))?;
    let mut s = String::from("----OxidePageFormBoundary");
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}
