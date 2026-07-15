//! `Headers` implementation.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::netdata::HeadersData;
use crate::state::HostData;

type HeadersRef = Rc<RefCell<HeadersData>>;

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    init: JsValue,
) -> Result<JsValue, JsThrow> {
    let mut data = HeadersData::default();
    if let Ok(other) = cx.this_headers(&init) {
        data.entries = other.borrow().entries.clone();
    } else if !init.is_nullish() {
        for (name, value) in cx.entries_of(&init)? {
            data.append(&name, &value)?;
        }
    }
    cx.new_net_object("Headers", HostData::Headers(Rc::new(RefCell::new(data))))
}

pub(crate) fn append(
    _cx: &BindCx<'_>,
    this: HeadersRef,
    name: String,
    value: String,
) -> Result<(), JsThrow> {
    this.borrow_mut().append(&name, &value)?;
    Ok(())
}

pub(crate) fn delete(_cx: &BindCx<'_>, this: HeadersRef, name: String) -> Result<(), JsThrow> {
    this.borrow_mut().delete(&name);
    Ok(())
}

pub(crate) fn get(
    _cx: &BindCx<'_>,
    this: HeadersRef,
    name: String,
) -> Result<Option<String>, JsThrow> {
    Ok(this.borrow().get(&name))
}

pub(crate) fn has(_cx: &BindCx<'_>, this: HeadersRef, name: String) -> Result<bool, JsThrow> {
    Ok(this.borrow().has(&name))
}

pub(crate) fn set(
    _cx: &BindCx<'_>,
    this: HeadersRef,
    name: String,
    value: String,
) -> Result<(), JsThrow> {
    this.borrow_mut().set(&name, &value)?;
    Ok(())
}

pub(crate) fn for_each(
    cx: &BindCx<'_>,
    this: HeadersRef,
    callback: JsValue,
) -> Result<(), JsThrow> {
    if !cx.scope.is_function(&callback) {
        return Err(JsThrow::Type(
            "Headers.forEach: callback is not a function".into(),
        ));
    }
    let pairs = this.borrow().sorted_combined();
    for (name, value) in pairs {
        cx.scope
            .call(
                &callback,
                &JsValue::Undefined,
                &[JsValue::String(value), JsValue::String(name)],
            )
            .map_err(JsThrow::from)?;
    }
    Ok(())
}
