//! Engine-neutral JS values.
//!
//! Primitives are converted eagerly at the boundary; everything else
//! (objects, functions, symbols, …) is held as an opaque engine reference
//! that keeps the underlying JS value alive and round-trips with identity
//! preserved.

use std::any::Any;
use std::fmt;
use std::rc::Rc;

/// A JS value crossing the engine boundary.
#[derive(Clone, Debug)]
pub enum JsValue {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    /// Any non-primitive value (object, function, symbol, …).
    Object(JsObject),
}

impl JsValue {
    #[must_use]
    pub fn is_undefined(&self) -> bool {
        matches!(self, JsValue::Undefined)
    }

    #[must_use]
    pub fn is_nullish(&self) -> bool {
        matches!(self, JsValue::Undefined | JsValue::Null)
    }

    #[must_use]
    pub fn as_object(&self) -> Option<&JsObject> {
        match self {
            JsValue::Object(o) => Some(o),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsValue::String(s) => Some(s),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            JsValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// JS `ToBoolean` (total, so it lives here rather than on the scope).
    #[must_use]
    pub fn truthy(&self) -> bool {
        match self {
            JsValue::Undefined | JsValue::Null => false,
            JsValue::Bool(b) => *b,
            JsValue::Number(n) => *n != 0.0 && !n.is_nan(),
            JsValue::String(s) => !s.is_empty(),
            JsValue::Object(_) => true,
        }
    }
}

impl From<bool> for JsValue {
    fn from(v: bool) -> Self {
        JsValue::Bool(v)
    }
}

impl From<f64> for JsValue {
    fn from(v: f64) -> Self {
        JsValue::Number(v)
    }
}

impl From<u32> for JsValue {
    fn from(v: u32) -> Self {
        JsValue::Number(f64::from(v))
    }
}

impl From<i32> for JsValue {
    fn from(v: i32) -> Self {
        JsValue::Number(f64::from(v))
    }
}

impl From<String> for JsValue {
    fn from(v: String) -> Self {
        JsValue::String(v)
    }
}

impl From<&str> for JsValue {
    fn from(v: &str) -> Self {
        JsValue::String(v.to_owned())
    }
}

impl From<JsObject> for JsValue {
    fn from(v: JsObject) -> Self {
        JsValue::Object(v)
    }
}

impl<T: Into<JsValue>> From<Option<T>> for JsValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => JsValue::Null,
        }
    }
}

/// An opaque, cloneable reference to a non-primitive JS value.
///
/// Holding a `JsObject` keeps the underlying JS value alive; it must be
/// dropped before its realm is torn down (the bindings state, which owns all
/// long-lived references, is dropped before the engine by construction).
#[derive(Clone)]
pub struct JsObject(pub(crate) Rc<dyn Any>);

impl JsObject {
    pub(crate) fn new(inner: Rc<dyn Any>) -> Self {
        Self(inner)
    }
}

impl fmt::Debug for JsObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JsObject")
    }
}
