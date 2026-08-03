//! `Blob` implementation (File API), and the shared part-list marshalling the
//! `File` constructor reuses.
//!
//! The bytes live in a [`BlobData`] behind an `Rc`, and `slice()` produces a
//! *view* over the same buffer rather than a copy (ADR-0032 D10) — which is the
//! whole point of the interface: the chunked-upload idiom slices one big file
//! repeatedly, and a copying `slice` would duplicate it once per chunk.

use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::filedata::{BlobData, normalize_type};

type Blob = Rc<BlobData>;

/// Flattens a `sequence<BlobPart>` into bytes.
///
/// The three branches are the spec's: a `Blob`/`File` contributes its bytes,
/// a `BufferSource` contributes its bytes, and everything else is stringified
/// and UTF-8 encoded. Shared with the `File` constructor, whose `fileBits`
/// argument is the same list under a different name.
///
/// `endings: "native"` is deliberately absent: there is no platform line
/// ending to convert to in a headless engine, and silently ignoring the member
/// would be the half-truth P6 forbids — so the option is simply not read, and
/// `"transparent"` (the default, a no-op) is what every blob gets.
pub(crate) fn parts_to_bytes(cx: &BindCx<'_>, parts: &JsValue) -> Result<Vec<u8>, JsThrow> {
    let mut bytes = Vec::new();
    for part in cx.blob_parts(parts)? {
        if let Some(blob) = cx.as_blob(&part) {
            bytes.extend_from_slice(blob.view());
        } else if let Some(buffer) = cx.buffer_source_bytes(&part)? {
            bytes.extend_from_slice(&buffer);
        } else {
            let text = cx.scope.coerce_string(&part).map_err(JsThrow::from)?;
            bytes.extend_from_slice(text.as_bytes());
        }
    }
    Ok(bytes)
}

/// Reads the `type` member of a `BlobPropertyBag`, normalized.
pub(crate) fn options_type(cx: &BindCx<'_>, options: &JsValue) -> Result<String, JsThrow> {
    let JsValue::Object(obj) = options else {
        return Ok(String::new());
    };
    let value = cx.scope.get(obj, "type").map_err(JsThrow::from)?;
    if value.is_undefined() {
        return Ok(String::new());
    }
    let text = cx.scope.coerce_string(&value).map_err(JsThrow::from)?;
    Ok(normalize_type(&text))
}

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    parts: JsValue,
    options: JsValue,
) -> Result<JsValue, JsThrow> {
    let bytes = parts_to_bytes(cx, &parts)?;
    let type_ = options_type(cx, &options)?;
    cx.new_blob(Rc::new(BlobData::new(bytes, type_)))
}

pub(crate) fn size(_cx: &BindCx<'_>, this: Blob) -> Result<f64, JsThrow> {
    Ok(this.size() as f64)
}

pub(crate) fn r#type(_cx: &BindCx<'_>, this: Blob) -> Result<String, JsThrow> {
    Ok(this.type_.clone())
}

pub(crate) fn slice(
    cx: &BindCx<'_>,
    this: Blob,
    start: Option<i64>,
    end: Option<i64>,
    content_type: Option<String>,
) -> Result<JsValue, JsThrow> {
    // `contentType` is normalized like the constructor's `type`, and an omitted
    // one yields the empty string rather than inheriting the source blob's —
    // a slice of a `text/plain` blob is bytes with no claimed type.
    let type_ = content_type
        .as_deref()
        .map(normalize_type)
        .unwrap_or_default();
    cx.new_blob(Rc::new(this.slice(start, end, type_)))
}

pub(crate) fn text(cx: &BindCx<'_>, this: Blob) -> Result<JsValue, JsThrow> {
    // Always UTF-8, per the spec: `Blob.text()` has no encoding argument, and
    // the decode is lossy so a malformed sequence yields U+FFFD rather than a
    // rejection.
    let text = String::from_utf8_lossy(this.view()).into_owned();
    cx.resolved_promise(JsValue::String(text))
}

pub(crate) fn array_buffer(cx: &BindCx<'_>, this: Blob) -> Result<JsValue, JsThrow> {
    let buffer = cx.bytes_to_array_buffer(this.view())?;
    cx.resolved_promise(buffer)
}
