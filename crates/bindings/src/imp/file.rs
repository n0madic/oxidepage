//! `File` implementation: a [`BlobData`] carrying the name and timestamp that
//! make it a file (ADR-0032 D10).
//!
//! Everything byte-related is inherited from `Blob` — `size`, `type`, `slice`,
//! `text`, `arrayBuffer` are the *same* implementations, reached through the
//! prototype chain, because `this_blob` accepts a `File` receiver.

use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::filedata::{BlobData, FileMeta};

type File = Rc<BlobData>;

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    file_bits: JsValue,
    file_name: String,
    options: JsValue,
) -> Result<JsValue, JsThrow> {
    let bytes = super::blob::parts_to_bytes(cx, &file_bits)?;
    let type_ = super::blob::options_type(cx, &options)?;
    // The spec replaces U+002F with U+003A in the name. A file name is used to
    // derive an upload's `filename` parameter and, at the embedder, a path —
    // so a separator surviving here is a path-traversal primitive.
    let name = file_name.replace('/', ":");
    let last_modified = last_modified_option(cx, &options)?.unwrap_or_else(|| now_epoch_ms(cx));
    cx.new_blob(Rc::new(BlobData {
        file: Some(FileMeta {
            name,
            last_modified,
        }),
        ..BlobData::new(bytes, type_)
    }))
}

/// The `lastModified` member of a `FilePropertyBag`, when it has one.
fn last_modified_option(cx: &BindCx<'_>, options: &JsValue) -> Result<Option<i64>, JsThrow> {
    let JsValue::Object(obj) = options else {
        return Ok(None);
    };
    let value = cx.scope.get(obj, "lastModified").map_err(JsThrow::from)?;
    if value.is_nullish() {
        return Ok(None);
    }
    let n = cx.scope.coerce_number(&value).map_err(JsThrow::from)?;
    Ok(Some(if n.is_finite() { n.trunc() as i64 } else { 0 }))
}

/// "Now" on the page's own clock — the same monotonic-from-time-origin reading
/// every other timestamp in the realm uses, so two files created in one task
/// cannot be stamped out of order by a wall-clock step.
fn now_epoch_ms(cx: &BindCx<'_>) -> i64 {
    cx.state.epoch_now_ms() as i64
}

/// The two accessors below are the only members `File` adds. `this_file`
/// already demanded the metadata, so the `expect` is discharged by the brand
/// check rather than assumed.
fn meta(this: &File) -> &FileMeta {
    this.file
        .as_ref()
        .expect("this_file admits only a File receiver")
}

pub(crate) fn name(_cx: &BindCx<'_>, this: File) -> Result<String, JsThrow> {
    Ok(meta(&this).name.clone())
}

pub(crate) fn last_modified(_cx: &BindCx<'_>, this: File) -> Result<f64, JsThrow> {
    Ok(meta(&this).last_modified as f64)
}
