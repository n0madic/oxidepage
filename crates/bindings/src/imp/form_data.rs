//! `FormData`.
//!
//! jQuery 4 made this load-bearing: its ajax prefilter runs
//! `s.data instanceof window.FormData` on **every** `$.ajax` call, so a missing
//! `FormData` global is not a missing convenience — it is a `TypeError`
//! ("invalid 'instanceof' right operand") that breaks all of jQuery 4's ajax.
//!
//! Values are strings. `Blob`/`File` do not exist in this engine, so a file
//! entry has nothing to hold; `<input type=file>` contributes nothing, as it
//! would with an empty selection.

use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::netdata::FormDataData;
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
) -> Vec<(String, String)> {
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
                                if value.is_empty() {
                                    "on".to_owned()
                                } else {
                                    value
                                },
                            ));
                        }
                    }
                    // A button is successful only as the submitter. A file
                    // input has no files to contribute.
                    "submit" if submitter == Some(control) => {
                        entries.push((name, dom.form_value(control)));
                    }
                    "submit" | "reset" | "button" | "image" | "file" => {}
                    _ => entries.push((name, dom.form_value(control))),
                }
            }
            "textarea" => entries.push((name, dom.form_value(control))),
            // Every selected option contributes — that is what makes a
            // `multiple` select produce several entries under one name.
            "select" => {
                for option in dom.select_options(control) {
                    if dom.checkedness(option) {
                        entries.push((name.clone(), dom.form_value(option)));
                    }
                }
            }
            // `<button>` is only successful as the submitter; `<fieldset>` and
            // `<object>` never contribute.
            "button" if submitter == Some(control) => {
                entries.push((name, dom.form_value(control)));
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
    value: String,
) -> Result<(), JsThrow> {
    let _ = cx;
    this.list.borrow_mut().push((name, value));
    Ok(())
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
) -> Result<Option<String>, JsThrow> {
    let _ = cx;
    Ok(this
        .list
        .borrow()
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.clone()))
}

pub(crate) fn get_all(
    cx: &BindCx<'_>,
    this: Rc<FormDataData>,
    name: String,
) -> Result<JsValue, JsThrow> {
    let values: Vec<JsValue> = this
        .list
        .borrow()
        .iter()
        .filter(|(n, _)| *n == name)
        .map(|(_, v)| JsValue::String(v.clone()))
        .collect();
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
    value: String,
) -> Result<(), JsThrow> {
    let _ = cx;
    let mut list = this.list.borrow_mut();
    match list.iter().position(|(n, _)| *n == name) {
        Some(i) => {
            list[i].1 = value;
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
        None => list.push((name, value)),
    }
    Ok(())
}

/// `snapshot(formData)` → `[[name, value], …]`, backing the pair iteration
/// installed on the prototype (shared with `URLSearchParams`).
pub(crate) fn snapshot(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let data = cx.this_form_data(&call.arg(0))?;
    crate::imp::url_search_params::pairs_to_js(cx, &data.pairs())
}
