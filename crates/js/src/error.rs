//! JS boundary errors.
//!
//! [`JsError`] flows out of the engine (evaluation and calls that threw);
//! [`JsThrow`] flows into it (host callbacks raising exceptions that page
//! script can catch).

use thiserror::Error;

use crate::value::JsValue;

/// An error produced while evaluating or calling into a realm.
#[derive(Debug, Error)]
pub enum JsError {
    /// Script threw. Carries the rendered message (with stack when
    /// available) and the thrown value itself.
    #[error("{message}")]
    Exception {
        message: String,
        value: Option<JsValue>,
    },
    /// Engine-level failure (allocation, unrelated-runtime, …) with no JS
    /// exception value.
    #[error("js engine failure: {0}")]
    Engine(String),
}

/// An exception raised by a host callback, surfaced to page script.
///
/// Spec-named `DOMException`s are constructed as real JS objects by the
/// bindings and thrown via [`JsThrow::Value`]; the engine crate only knows
/// the ECMAScript-native error kinds it must be able to mint itself.
#[derive(Debug)]
pub enum JsThrow {
    /// `TypeError` (the WebIDL workhorse).
    Type(String),
    /// `RangeError`.
    Range(String),
    /// Throw this exact value.
    Value(JsValue),
}

impl From<JsError> for JsThrow {
    /// Propagates an error from a nested engine call out of a host callback,
    /// rethrowing the original exception value when there is one.
    fn from(err: JsError) -> Self {
        match err {
            JsError::Exception {
                value: Some(value), ..
            } => JsThrow::Value(value),
            JsError::Exception { message, .. } => JsThrow::Type(message),
            JsError::Engine(message) => JsThrow::Type(message),
        }
    }
}
