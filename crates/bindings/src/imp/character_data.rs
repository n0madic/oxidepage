//! `CharacterData` implementation. Offsets and lengths are UTF-16 code
//! units, per spec.

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_js::JsThrow;

use crate::cx::BindCx;

pub(crate) fn data_of(cx: &BindCx<'_>, this: NodeId) -> String {
    cx.state
        .dom
        .borrow()
        .node(this)
        .character_data()
        .map(|d| d.to_string())
        .unwrap_or_default()
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn from_units(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Spec "replace data" (offset, count, data), shared by all mutators.
fn splice_data(
    cx: &BindCx<'_>,
    this: NodeId,
    offset: usize,
    count: usize,
    data: &str,
) -> Result<(), JsThrow> {
    let current = units(&data_of(cx, this));
    if offset > current.len() {
        return Err(cx.dom_exception(oxidepage_base::DomException::new(
            DomExceptionKind::IndexSizeError,
            "offset is past the end of the data",
        )));
    }
    let count = count.min(current.len() - offset);
    let mut next = Vec::with_capacity(current.len());
    next.extend_from_slice(&current[..offset]);
    next.extend(data.encode_utf16());
    next.extend_from_slice(&current[offset + count..]);
    cx.state
        .dom
        .borrow_mut()
        .set_character_data(this, from_units(&next).into());
    Ok(())
}

pub(crate) fn data(cx: &BindCx<'_>, this: NodeId) -> Result<String, JsThrow> {
    Ok(data_of(cx, this))
}

pub(crate) fn set_data(cx: &BindCx<'_>, this: NodeId, value: String) -> Result<(), JsThrow> {
    cx.state
        .dom
        .borrow_mut()
        .set_character_data(this, value.into());
    Ok(())
}

pub(crate) fn length(cx: &BindCx<'_>, this: NodeId) -> Result<f64, JsThrow> {
    Ok(data_of(cx, this).encode_utf16().count() as f64)
}

pub(crate) fn substring_data(
    cx: &BindCx<'_>,
    this: NodeId,
    offset: u32,
    count: u32,
) -> Result<String, JsThrow> {
    let current = units(&data_of(cx, this));
    let offset = offset as usize;
    if offset > current.len() {
        return Err(cx.dom_exception(oxidepage_base::DomException::new(
            DomExceptionKind::IndexSizeError,
            "offset is past the end of the data",
        )));
    }
    let end = (offset + count as usize).min(current.len());
    Ok(from_units(&current[offset..end]))
}

pub(crate) fn append_data(cx: &BindCx<'_>, this: NodeId, data: String) -> Result<(), JsThrow> {
    let len = units(&data_of(cx, this)).len();
    splice_data(cx, this, len, 0, &data)
}

pub(crate) fn insert_data(
    cx: &BindCx<'_>,
    this: NodeId,
    offset: u32,
    data: String,
) -> Result<(), JsThrow> {
    splice_data(cx, this, offset as usize, 0, &data)
}

pub(crate) fn delete_data(
    cx: &BindCx<'_>,
    this: NodeId,
    offset: u32,
    count: u32,
) -> Result<(), JsThrow> {
    splice_data(cx, this, offset as usize, count as usize, "")
}

pub(crate) fn replace_data(
    cx: &BindCx<'_>,
    this: NodeId,
    offset: u32,
    count: u32,
    data: String,
) -> Result<(), JsThrow> {
    splice_data(cx, this, offset as usize, count as usize, &data)
}
