//! `FileList` implementation: an indexed, read-only list of `File`s.
//!
//! The `DOMRectList` shape, with one difference that matters — the entries are
//! shared `Rc<BlobData>`s rather than copies, so `input.files[0]` read twice
//! names the same bytes both times without holding a wrapper alive between
//! reads.

use std::rc::Rc;

use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::filedata::BlobData;

type FileListRef = Rc<Vec<Rc<BlobData>>>;

pub(crate) fn length(_cx: &BindCx<'_>, this: FileListRef) -> Result<f64, JsThrow> {
    Ok(this.len() as f64)
}

pub(crate) fn item(cx: &BindCx<'_>, this: FileListRef, index: u32) -> Result<JsValue, JsThrow> {
    match this.get(index as usize) {
        Some(file) => cx.new_blob(Rc::clone(file)),
        None => Ok(JsValue::Null),
    }
}
