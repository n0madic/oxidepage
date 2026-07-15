//! `URLSearchParams` implementation.

use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::netdata::UrlSearchParamsData;
use crate::state::HostData;

type Params = Rc<UrlSearchParamsData>;

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let list = parse_init(cx, &init)?;
    cx.new_net_object(
        "URLSearchParams",
        HostData::UrlSearchParams(Rc::new(UrlSearchParamsData::standalone(list))),
    )
}

/// Parses the constructor init: a query string, another `URLSearchParams`, or
/// a sequence of `[name, value]` pairs.
fn parse_init(cx: &BindCx<'_>, init: &JsValue) -> Result<Vec<(String, String)>, JsThrow> {
    match init {
        JsValue::Undefined | JsValue::Null => Ok(Vec::new()),
        JsValue::String(s) => Ok(parse_query(s)),
        JsValue::Object(_) => {
            if let Ok(other) = cx.this_url_search_params(init) {
                return Ok(other.pairs());
            }
            // A record, an array of `[name, value]` pairs, or an iterable.
            cx.entries_of(init)
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_query(s: &str) -> Vec<(String, String)> {
    let s = s.strip_prefix('?').unwrap_or(s);
    url::form_urlencoded::parse(s.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub(crate) fn size(_cx: &BindCx<'_>, this: Params) -> Result<f64, JsThrow> {
    Ok(this.pairs().len() as f64)
}

pub(crate) fn append(
    _cx: &BindCx<'_>,
    this: Params,
    name: String,
    value: String,
) -> Result<(), JsThrow> {
    let mut pairs = this.pairs();
    pairs.push((name, value));
    this.set_pairs(pairs);
    Ok(())
}

pub(crate) fn delete(
    _cx: &BindCx<'_>,
    this: Params,
    name: String,
    value: Option<String>,
) -> Result<(), JsThrow> {
    let mut pairs = this.pairs();
    match value {
        Some(v) => pairs.retain(|(n, val)| !(n == &name && val == &v)),
        None => pairs.retain(|(n, _)| n != &name),
    }
    this.set_pairs(pairs);
    Ok(())
}

pub(crate) fn get(_cx: &BindCx<'_>, this: Params, name: String) -> Result<Option<String>, JsThrow> {
    Ok(this
        .pairs()
        .into_iter()
        .find(|(n, _)| n == &name)
        .map(|(_, v)| v))
}

pub(crate) fn get_all(cx: &BindCx<'_>, this: Params, name: String) -> Result<JsValue, JsThrow> {
    let values: Vec<JsValue> = this
        .pairs()
        .into_iter()
        .filter(|(n, _)| n == &name)
        .map(|(_, v)| JsValue::String(v))
        .collect();
    cx.scope
        .new_array(&values)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

pub(crate) fn has(
    _cx: &BindCx<'_>,
    this: Params,
    name: String,
    value: Option<String>,
) -> Result<bool, JsThrow> {
    let pairs = this.pairs();
    Ok(match value {
        Some(v) => pairs.iter().any(|(n, val)| n == &name && val == &v),
        None => pairs.iter().any(|(n, _)| n == &name),
    })
}

pub(crate) fn set(
    _cx: &BindCx<'_>,
    this: Params,
    name: String,
    value: String,
) -> Result<(), JsThrow> {
    let mut pairs = this.pairs();
    if let Some(pos) = pairs.iter().position(|(n, _)| n == &name) {
        // Set the first occurrence and drop any later duplicates.
        pairs[pos].1 = value;
        let mut i = 0;
        pairs.retain(|(n, _)| {
            let keep = n != &name || i == pos;
            i += 1;
            keep
        });
    } else {
        pairs.push((name, value));
    }
    this.set_pairs(pairs);
    Ok(())
}

pub(crate) fn sort(_cx: &BindCx<'_>, this: Params) -> Result<(), JsThrow> {
    let mut pairs = this.pairs();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    this.set_pairs(pairs);
    Ok(())
}

pub(crate) fn to_string(_cx: &BindCx<'_>, this: Params) -> Result<String, JsThrow> {
    Ok(this.serialize())
}

/// Builds the `[[name, value], …]` JS array that backs pair iteration
/// (`forEach`/`entries`/`keys`/`values`/`Symbol.iterator`), hand-registered
/// because the generic `iterable<>` codegen only handles single-value lists.
///
/// Shared with `FormData`, which is the same shape.
pub(crate) fn pairs_to_js(cx: &BindCx<'_>, pairs: &[(String, String)]) -> Result<JsValue, JsThrow> {
    let mut items = Vec::new();
    for (name, value) in pairs {
        let pair = cx
            .scope
            .new_array(&[
                JsValue::String(name.clone()),
                JsValue::String(value.clone()),
            ])
            .map_err(JsThrow::from)?;
        items.push(JsValue::Object(pair));
    }
    cx.scope
        .new_array(&items)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

/// `snapshot(params)` → `[[name, value], …]`.
pub(crate) fn snapshot(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let this = cx.this_url_search_params(&call.arg(0))?;
    pairs_to_js(cx, &this.pairs())
}
