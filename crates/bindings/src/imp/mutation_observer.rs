//! `MutationObserver` implementation.

use oxidepage_base::NodeId;
use oxidepage_dom::observer::MutationObserverId;
use oxidepage_dom::{LocalName, MutationRecord, ObserveInit};
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::{HostData, ObserverEntry, RecordView};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    callback: JsValue,
) -> Result<JsValue, JsThrow> {
    if !cx.scope.is_function(&callback) {
        return Err(JsThrow::Type(
            "MutationObserver constructor requires a callback function".into(),
        ));
    }
    let id = cx.state.dom.borrow_mut().observers_mut().create_observer();
    let wrapper = cx.new_slab_object("MutationObserver", HostData::Observer(id))?;
    cx.state.observers.borrow_mut().push(ObserverEntry {
        id,
        wrapper: wrapper.clone(),
        callback,
    });
    Ok(wrapper)
}

fn bool_prop(cx: &BindCx<'_>, options: &JsValue, name: &str) -> Option<bool> {
    let JsValue::Object(obj) = options else {
        return None;
    };
    match cx.scope.get(obj, name) {
        Ok(JsValue::Undefined) | Err(_) => None,
        Ok(value) => Some(value.truthy()),
    }
}

fn attribute_filter(cx: &BindCx<'_>, options: &JsValue) -> Result<Option<Vec<LocalName>>, JsThrow> {
    let JsValue::Object(obj) = options else {
        return Ok(None);
    };
    let value = match cx.scope.get(obj, "attributeFilter") {
        Ok(JsValue::Undefined) | Err(_) => return Ok(None),
        Ok(value) => value,
    };
    let JsValue::Object(array) = &value else {
        return Err(JsThrow::Type("attributeFilter must be a sequence".into()));
    };
    let length = cx.scope.array_length(array).map_err(JsThrow::from)?;
    let mut filter = Vec::with_capacity(length);
    for i in 0..length {
        let item = cx.scope.array_get(array, i).map_err(JsThrow::from)?;
        let name = cx.scope.coerce_string(&item).map_err(JsThrow::from)?;
        filter.push(LocalName::from(name));
    }
    Ok(Some(filter))
}

pub(crate) fn observe(
    cx: &BindCx<'_>,
    this: MutationObserverId,
    target: NodeId,
    options: JsValue,
) -> Result<(), JsThrow> {
    let init = ObserveInit {
        child_list: bool_prop(cx, &options, "childList").unwrap_or(false),
        attributes: bool_prop(cx, &options, "attributes"),
        character_data: bool_prop(cx, &options, "characterData"),
        subtree: bool_prop(cx, &options, "subtree").unwrap_or(false),
        attribute_old_value: bool_prop(cx, &options, "attributeOldValue"),
        character_data_old_value: bool_prop(cx, &options, "characterDataOldValue"),
        attribute_filter: attribute_filter(cx, &options)?,
    };
    cx.state
        .dom
        .borrow_mut()
        .observers_mut()
        .observe(this, target, init)
        .map_err(|e| JsThrow::Type(e.to_string()))
}

pub(crate) fn disconnect(cx: &BindCx<'_>, this: MutationObserverId) -> Result<(), JsThrow> {
    cx.state.dom.borrow_mut().observers_mut().disconnect(this);
    Ok(())
}

pub(crate) fn take_records(cx: &BindCx<'_>, this: MutationObserverId) -> Result<JsValue, JsThrow> {
    let records = cx.state.dom.borrow_mut().observers_mut().take_records(this);
    records_to_js(cx, records)
}

/// Builds the JS array of `MutationRecord` wrappers for a delivery.
pub(crate) fn records_to_js(
    cx: &BindCx<'_>,
    records: Vec<MutationRecord>,
) -> Result<JsValue, JsThrow> {
    let mut items = Vec::with_capacity(records.len());
    for record in records {
        items.push(cx.new_mutation_record(RecordView {
            record,
            added_nodes_js: std::cell::RefCell::new(None),
            removed_nodes_js: std::cell::RefCell::new(None),
        })?);
    }
    cx.scope
        .new_array(&items)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}
