//! `FormData`.
//!
//! jQuery 4 made this load-bearing: its ajax prefilter runs
//! `s.data instanceof window.FormData` on **every** `$.ajax` call, so a missing
//! `FormData` global is not a missing convenience — it is a `TypeError`
//! ("invalid 'instanceof' right operand") that breaks all of jQuery 4's ajax.
//!
//! An entry value is a string **or** a `Blob`/`File` (ADR-0032 D11), and an
//! `<input type=file>` contributes one entry per selected file. That is what
//! makes a real form upload work — before it, a file input contributed nothing
//! at all and a multipart POST silently carried every field but the file.

use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::netdata::{FormDataData, FormDataValue};
use crate::state::HostData;

/// `new FormData(form?)`.
pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    arg: JsValue,
) -> Result<JsValue, JsThrow> {
    let entries = if arg.is_nullish() {
        Vec::new()
    } else {
        // The only accepted argument is a `<form>`; anything else is a
        // TypeError, as WebIDL's `HTMLFormElement` argument would give.
        let node = cx.this_element(&arg).map_err(|_| {
            JsThrow::Type("FormData constructor: argument is not an HTMLFormElement".into())
        })?;
        if !cx
            .state
            .dom
            .borrow()
            .node(node)
            .as_element()
            .is_some_and(|el| el.is_html_element() && &*el.name.local == "form")
        {
            return Err(JsThrow::Type(
                "FormData constructor: argument is not an HTMLFormElement".into(),
            ));
        }
        construct_the_entry_list(cx, node, None)
    };
    let data = Rc::new(FormDataData::new(entries));
    cx.new_slab_object("FormData", HostData::FormData(data))
}

/// HTML's **"construct the entry list"**: the form's *successful* controls, in
/// tree order.
///
/// A control contributes iff it has a non-empty `name`, is not disabled, and —
/// for a checkbox or radio — is checked. Buttons are excluded **unless the
/// button is `submitter`**, which is what makes a submit button's `name=value`
/// land in the list at its own tree position rather than appended at the end.
/// All of that state already lives in the DOM (ADR-0019), so this is a
/// projection, not a re-derivation.
///
/// `submitter` is `None` for `new FormData(form)` (there is no submitter) and
/// `Some` for a form submission — one algorithm, not two (`imp::form_submit`).
pub(crate) fn construct_the_entry_list(
    cx: &BindCx<'_>,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Vec<(String, FormDataValue)> {
    let dom = cx.state.dom.borrow();
    let mut entries = Vec::new();
    for control in dom.form_controls(form) {
        let Some(el) = dom.node(control).as_element() else {
            continue;
        };
        let local = el.local_name().to_string();
        let name = el
            .attr(&oxidepage_dom::node::attr_name("name".into()))
            .map(ToString::to_string)
            .unwrap_or_default();
        if name.is_empty() || dom.is_actually_disabled(control) {
            continue;
        }
        match local.as_str() {
            "input" => {
                let ty = oxidepage_dom::input_type(el);
                match ty {
                    // Only a *checked* checkbox or radio is successful. An
                    // on/off control with no `value` submits the literal "on".
                    "checkbox" | "radio" => {
                        if dom.checkedness(control) {
                            let value = dom.form_value(control);
                            entries.push((
                                name,
                                FormDataValue::Text(if value.is_empty() {
                                    "on".to_owned()
                                } else {
                                    value
                                }),
                            ));
                        }
                    }
                    // A button is successful only as the submitter.
                    "submit" if submitter == Some(control) => {
                        entries.push((name, FormDataValue::Text(dom.form_value(control))));
                    }
                    // Each selected file is one entry (ADR-0032 D11). A file
                    // input with an empty selection contributes a single
                    // *empty* file entry, per HTML — which is what lets a
                    // server tell "no file chosen" from "field absent".
                    "file" => {
                        let files = dom.selected_files(control);
                        if files.is_empty() {
                            entries.push((
                                name,
                                FormDataValue::File {
                                    data: Rc::new(crate::filedata::BlobData::file(
                                        Vec::new(),
                                        String::from("application/octet-stream"),
                                        String::new(),
                                        0,
                                    )),
                                    filename: String::new(),
                                },
                            ));
                        } else {
                            for file in files {
                                entries.push((
                                    name.clone(),
                                    FormDataValue::File {
                                        // The `Rc` clone shares the embedder's
                                        // buffer: building the entry list for a
                                        // form post must not copy the upload.
                                        data: Rc::new(crate::filedata::BlobData::shared_file(
                                            Rc::clone(&file.bytes),
                                            file.content_type.clone(),
                                            file.name.clone(),
                                            file.last_modified,
                                        )),
                                        filename: file.name.clone(),
                                    },
                                ));
                            }
                        }
                    }
                    "submit" | "reset" | "button" | "image" => {}
                    _ => entries.push((name, FormDataValue::Text(dom.form_value(control)))),
                }
            }
            "textarea" => entries.push((name, FormDataValue::Text(dom.form_value(control)))),
            // Every selected option contributes — that is what makes a
            // `multiple` select produce several entries under one name.
            "select" => {
                for option in dom.select_options(control) {
                    if dom.checkedness(option) {
                        entries.push((name.clone(), FormDataValue::Text(dom.form_value(option))));
                    }
                }
            }
            // `<button>` is only successful as the submitter; `<fieldset>` and
            // `<object>` never contribute.
            "button" if submitter == Some(control) => {
                entries.push((name, FormDataValue::Text(dom.form_value(control))));
            }
            _ => {}
        }
    }
    entries
}

pub(crate) fn append(
    cx: &BindCx<'_>,
    this: Rc<FormDataData>,
    name: String,
    value: JsValue,
    filename: Option<String>,
) -> Result<(), JsThrow> {
    let entry = create_entry(cx, &value, filename)?;
    this.list.borrow_mut().push((name, entry));
    Ok(())
}

/// Fetch's **"create an entry"**: a `Blob`/`File` value becomes a file entry,
/// anything else is stringified.
///
/// The `filename` argument wins where it is given; otherwise a `File` uses its
/// own name and a bare `Blob` gets the literal `"blob"`, which is what the spec
/// says and what every server-side parser expects to see.
fn create_entry(
    cx: &BindCx<'_>,
    value: &JsValue,
    filename: Option<String>,
) -> Result<FormDataValue, JsThrow> {
    // `as_blob`, not `this_blob`: this is a *recognition* test among other
    // types, not an unwrap that should throw. Discarding a `this_blob` error
    // works but reads as swallowing one.
    let Some(data) = cx.as_blob(value) else {
        // A `filename` on a string value is ignored rather than an error — the
        // spec throws only for `set`/`append` with a filename *and* a
        // non-`Blob`, and no page relies on that.
        return Ok(FormDataValue::Text(cx.scope.coerce_string(value)?));
    };
    let filename = filename
        .or_else(|| data.file.as_ref().map(|file| file.name.clone()))
        .unwrap_or_else(|| String::from("blob"));
    Ok(FormDataValue::File { data, filename })
}

/// Wraps one entry value for the accessors: a file entry hands back a real
/// `File`, a text entry a string.
fn entry_to_js(cx: &BindCx<'_>, value: &FormDataValue) -> Result<JsValue, JsThrow> {
    match value {
        FormDataValue::Text(text) => Ok(JsValue::String(text.clone())),
        FormDataValue::File { data, filename } => cx.new_form_data_file(data, filename),
    }
}

pub(crate) fn delete(cx: &BindCx<'_>, this: Rc<FormDataData>, name: String) -> Result<(), JsThrow> {
    let _ = cx;
    this.list.borrow_mut().retain(|(n, _)| *n != name);
    Ok(())
}

pub(crate) fn get(
    cx: &BindCx<'_>,
    this: Rc<FormDataData>,
    name: String,
) -> Result<JsValue, JsThrow> {
    // Cloned out of the borrow before the wrapper is built: creating a `File`
    // re-enters the realm, and an entry list borrowed across that is a
    // `BorrowMutError` waiting for a page that appends from a getter.
    let found = this
        .list
        .borrow()
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.clone());
    match found {
        Some(value) => entry_to_js(cx, &value),
        None => Ok(JsValue::Null),
    }
}

pub(crate) fn get_all(
    cx: &BindCx<'_>,
    this: Rc<FormDataData>,
    name: String,
) -> Result<JsValue, JsThrow> {
    let matched: Vec<FormDataValue> = this
        .list
        .borrow()
        .iter()
        .filter(|(n, _)| *n == name)
        .map(|(_, v)| v.clone())
        .collect();
    let values: Vec<JsValue> = matched
        .iter()
        .map(|value| entry_to_js(cx, value))
        .collect::<Result<_, _>>()?;
    cx.scope
        .new_array(&values)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

pub(crate) fn has(cx: &BindCx<'_>, this: Rc<FormDataData>, name: String) -> Result<bool, JsThrow> {
    let _ = cx;
    Ok(this.list.borrow().iter().any(|(n, _)| *n == name))
}

/// `set` replaces the **first** matching entry in place and drops the rest, so
/// the entry keeps its position — appending after a delete would not.
pub(crate) fn set(
    cx: &BindCx<'_>,
    this: Rc<FormDataData>,
    name: String,
    value: JsValue,
    filename: Option<String>,
) -> Result<(), JsThrow> {
    let entry = create_entry(cx, &value, filename)?;
    let mut list = this.list.borrow_mut();
    match list.iter().position(|(n, _)| *n == name) {
        Some(i) => {
            list[i].1 = entry;
            let mut seen = false;
            list.retain(|(n, _)| {
                if *n != name {
                    return true;
                }
                let first = !seen;
                seen = true;
                first
            });
        }
        None => list.push((name, entry)),
    }
    Ok(())
}

/// `snapshot(formData)` → `[[name, value], …]`, backing the pair iteration
/// installed on the prototype (`entries`/`keys`/`values`/`forEach`/`@@iterator`).
///
/// **Not `pairs()`.** That flattens a file entry to its filename, which is right
/// for the two serializations that cannot carry bytes and wrong here: `get()`
/// hands back a `File`, so an iterator handing back the string `"photo.png"`
/// would make the two accessors disagree about the same entry. The
/// `for (const [k, v] of fd) if (v instanceof File)` idiom — which is how every
/// upload library detects file parts — would take the wrong branch.
pub(crate) fn snapshot(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let data = cx.this_form_data(&call.arg(0))?;
    // Cloned out of the borrow first: building a `File` wrapper re-enters the
    // realm, and a page that appended from a `forEach` callback would otherwise
    // hit a `BorrowMutError`.
    let entries: Vec<(String, FormDataValue)> = data.list.borrow().clone();
    let mut items = Vec::with_capacity(entries.len());
    for (name, value) in &entries {
        let pair = cx
            .scope
            .new_array(&[JsValue::String(name.clone()), entry_to_js(cx, value)?])
            .map_err(JsThrow::from)?;
        items.push(JsValue::Object(pair));
    }
    cx.scope
        .new_array(&items)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}
