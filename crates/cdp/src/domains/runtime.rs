//! The `Runtime` domain: evaluation, remote handles, console, exceptions.
//!
//! The engine work behind this is in `oxidepage_bindings::remote` and
//! `oxidepage_page::remote`; what lives here is the JSON shape and nothing
//! else. Notably a `RemoteObject`'s by-value payload arrives as JSON *text*
//! (the realm's own `JSON.stringify` produced it), so it is spliced back in as
//! a parsed value rather than re-serialized.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use oxidepage_engine::page_api::{
    CallArgument, EvaluateOptions, EvaluationResult, ExceptionDetails, PropertyDescriptor,
    RemoteError, RemoteObject,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::{Connection, SessionState};

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Runtime.enable" => set_enabled(connection, request, true),
        "Runtime.disable" => set_enabled(connection, request, false),
        "Runtime.evaluate" => evaluate(connection, request),
        "Runtime.callFunctionOn" => call_function_on(connection, request),
        "Runtime.getProperties" => get_properties(connection, request),
        "Runtime.releaseObject" => release_object(connection, request),
        "Runtime.releaseObjectGroup" => release_object_group(connection, request),
        "Runtime.awaitPromise" => await_promise(connection, request),
        "Runtime.runIfWaitingForDebugger" => run_if_waiting_for_debugger(connection, request),
        "Runtime.addBinding" => add_binding(connection, request),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

fn set_enabled(connection: &Arc<Connection>, request: &Request, enabled: bool) -> CommandResult {
    let session = connection.require_session(request)?;
    session.flags.runtime.store(enabled, Ordering::Relaxed);
    if enabled {
        // `Runtime.enable` reports the contexts that already exist, which is
        // how a driver learns the id to evaluate against without navigating
        // first.
        connection.emit(execution_context_created(connection, &session));
    }
    Ok(json!({}))
}

/// `Runtime.executionContextCreated` for the session's current document.
pub fn execution_context_created(
    connection: &Arc<Connection>,
    session: &Arc<SessionState>,
) -> crate::message::Event {
    let id = session.page.execution_context_id().unwrap_or(1);
    execution_context_created_named(connection, session, "", true, id)
}

/// Offset that gives a named "isolated" world an id of its own.
/// The same event for a named world.
///
/// Worlds are real now (ADR-0033): `id` is the world's own monotonic
/// `Runtime.ExecutionContextId`, minted page-side and unique across documents
/// *and* worlds, so the `ISOLATED_WORLD_ID_OFFSET` arithmetic this used to do
/// is gone. The name and `isDefault: false` are still what a driver matches on
/// to bind its utility realm.
pub fn execution_context_created_named(
    connection: &Arc<Connection>,
    session: &Arc<SessionState>,
    name: &str,
    is_default: bool,
    id: u64,
) -> crate::message::Event {
    let url = connection
        .registry
        .info(&session.target_id)
        .map(|info| info.url)
        .unwrap_or_default();
    crate::message::Event::session(
        &session.id,
        "Runtime.executionContextCreated",
        json!({
            "context": {
                "id": id,
                "origin": crate::pump::security_origin(&url),
                "name": name,
                "uniqueId": format!("{}.{}", session.target_id, id),
                // Both drivers key off `auxData`: `frameId` is how they map a
                // context to a frame, and `isDefault` is how they tell the main
                // world from an isolated one.
                "auxData": {
                    "frameId": session.target_id,
                    "isDefault": is_default,
                },
            }
        }),
    )
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct EvaluateParams {
    expression: String,
    #[serde(default)]
    return_by_value: Option<bool>,
    #[serde(default)]
    await_promise: Option<bool>,
    #[serde(default)]
    object_group: Option<String>,
    /// Accepted and ignored: there is exactly one world per page until stage 9,
    /// so any context id a driver sends can only be that one.
    #[serde(default)]
    context_id: Option<i64>,
    #[serde(default)]
    user_gesture: Option<bool>,
    #[serde(default)]
    silent: Option<bool>,
}

fn options_from(
    by_value: Option<bool>,
    await_promise: Option<bool>,
    group: Option<String>,
) -> EvaluateOptions {
    EvaluateOptions {
        by_value: by_value.unwrap_or(false),
        await_promise: await_promise.unwrap_or(false),
        group,
        source_url: None,
    }
}

fn evaluate(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: EvaluateParams = request.parse()?;
    let _ = (params.user_gesture, params.silent);

    // Routed by context id (ADR-0033 D10). Absent means the main world; an id
    // no live world carries is an error rather than a silent evaluation in the
    // wrong one — which is exactly what a driver's utility-world call would
    // have been under the one-world compromise.
    let context_id = match params.context_id {
        None => None,
        Some(raw) => Some(
            u64::try_from(raw)
                .map_err(|_| ProtocolError::server("Cannot find context with specified id"))?,
        ),
    };
    let options = options_from(
        params.return_by_value,
        params.await_promise,
        params.object_group,
    );
    let outcome = session
        .page
        .evaluate_in(context_id, params.expression.clone(), options)?
        .map_err(ProtocolError::server)?;
    Ok(evaluation_json(&outcome))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallFunctionOnParams {
    function_declaration: String,
    #[serde(default)]
    object_id: Option<String>,
    #[serde(default)]
    arguments: Vec<CallArgumentParams>,
    #[serde(default)]
    return_by_value: Option<bool>,
    #[serde(default)]
    await_promise: Option<bool>,
    #[serde(default)]
    object_group: Option<String>,
    #[serde(default)]
    execution_context_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallArgumentParams {
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    object_id: Option<String>,
    /// `NaN`, `Infinity`, `-Infinity`, `-0`, `1n` — the primitives JSON cannot
    /// spell, which a driver sends back exactly as it received them.
    #[serde(default)]
    unserializable_value: Option<String>,
}

/// CDP's `objectId` is an opaque *string*; the engine's is a `u64`.
///
/// Parsed strictly rather than defaulted: an id the endpoint never minted must
/// be an error, not a silent read of object 0.
fn parse_object_id(id: &str) -> Result<u64, ProtocolError> {
    id.parse::<u64>()
        .map_err(|_| ProtocolError::server(format!("Could not find object with given id: {id}")))
}

fn call_function_on(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: CallFunctionOnParams = request.parse()?;
    // Selects the world when no `objectId` is given; checked against the
    // handle's world when one is (ADR-0033 D10).
    let context_id = match params.execution_context_id {
        None => None,
        Some(raw) => Some(
            u64::try_from(raw)
                .map_err(|_| ProtocolError::server("Cannot find context with specified id"))?,
        ),
    };

    let object_id = params
        .object_id
        .as_deref()
        .map(parse_object_id)
        .transpose()?;
    let mut arguments = Vec::with_capacity(params.arguments.len());
    for argument in &params.arguments {
        arguments.push(CallArgument {
            object_id: argument
                .object_id
                .as_deref()
                .map(parse_object_id)
                .transpose()?,
            // `NaN`, `±Infinity`, `-0` and `1n` have no JSON spelling at all,
            // so they travel as *source* rather than as a JSON literal. Wrapping
            // them in a JSON string — which is what a naive encoding does —
            // delivers the string `"NaN"` to the page, not the number.
            unserializable: argument.unserializable_value.clone(),
            value_json: match (&argument.value, &argument.unserializable_value) {
                (_, Some(_)) => None,
                (Some(value), None) => Some(value.to_string()),
                (None, None) => None,
            },
        });
    }

    let options = options_from(
        params.return_by_value,
        params.await_promise,
        params.object_group,
    );
    let outcome = session
        .page
        .call_function_on(
            params.function_declaration,
            object_id,
            context_id,
            arguments,
            options,
        )?
        .map_err(remote_error)?;
    Ok(evaluation_json(&outcome))
}

fn get_properties(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        object_id: String,
        #[serde(default)]
        own_properties: Option<bool>,
        #[serde(default)]
        object_group: Option<String>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let _ = params.own_properties;
    let id = parse_object_id(&params.object_id)?;

    let properties = session
        .page
        .get_properties(id, params.object_group)?
        .map_err(remote_error)?;
    Ok(json!({
        "result": properties.iter().map(property_json).collect::<Vec<_>>(),
    }))
}

fn release_object(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        object_id: String,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    // Releasing an id that is already gone is not an error: a driver racing a
    // navigation legitimately does it, and Chrome answers `{}`.
    session
        .page
        .release_object(parse_object_id(&params.object_id)?)?;
    Ok(json!({}))
}

fn release_object_group(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        object_group: String,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    session.page.release_object_group(params.object_group)?;
    Ok(json!({}))
}

fn await_promise(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        promise_object_id: String,
        #[serde(default)]
        return_by_value: Option<bool>,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = parse_object_id(&params.promise_object_id)?;
    let outcome = session
        .page
        .await_promise(id, options_from(params.return_by_value, Some(true), None))?
        .map_err(remote_error)?;
    Ok(evaluation_json(&outcome))
}

fn run_if_waiting_for_debugger(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    // The other half of `Target.setAutoAttach { waitForDebuggerOnStart }`: the
    // page was created suspended (ADR-0027 D10) and this is what starts it.
    // Harmless on a page that was never suspended, which is why Chrome lets a
    // driver send it unconditionally.
    session.page.resume()?;
    Ok(json!({}))
}

fn add_binding(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        name: String,
        /// The world to install the binding in, created if it does not exist
        /// yet. Absent installs it in every world (ADR-0033 D9), which is what
        /// a driver expecting the main world gets.
        #[serde(default)]
        execution_context_name: Option<String>,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    session
        .page
        .add_binding_in(params.name.clone(), params.execution_context_name.clone())?
        .map_err(ProtocolError::invalid_params)?;
    session.remember_binding(&params.name);
    Ok(json!({}))
}

// === serialization ===

/// A [`RemoteError`] in the protocol's vocabulary. `pub(crate)` because the
/// `DOM` domain answers the same errors from the same engine calls.
pub(crate) fn remote_error(error: RemoteError) -> ProtocolError {
    match error {
        RemoteError::BadArgument(detail) => ProtocolError::invalid_params(detail),
        other => ProtocolError::server(other.to_string()),
    }
}

/// CDP's `RemoteObject`.
pub fn remote_object_json(object: &RemoteObject) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(kind) = object.kind {
        out.insert(String::from("type"), json!(kind.as_str()));
    }
    if let Some(subtype) = object.subtype {
        out.insert(String::from("subtype"), json!(subtype.as_str()));
    }
    if let Some(class_name) = &object.class_name {
        out.insert(String::from("className"), json!(class_name));
    }
    if let Some(description) = &object.description {
        out.insert(String::from("description"), json!(description));
    }
    if let Some(unserializable) = &object.unserializable {
        out.insert(String::from("unserializableValue"), json!(unserializable));
    }
    if let Some(text) = &object.value_json {
        // Text produced by the realm's `JSON.stringify`, spliced back in as a
        // value. A parse failure here would mean the engine emitted invalid
        // JSON, so the honest fallback is to omit `value` rather than to send
        // the raw text under a name that promises structure.
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            out.insert(String::from("value"), value);
        }
    }
    if let Some(id) = object.object_id {
        out.insert(String::from("objectId"), json!(id.to_string()));
    }
    Value::Object(out)
}

fn property_json(property: &PropertyDescriptor) -> Value {
    let mut out = json!({
        "name": property.name,
        "configurable": true,
        "enumerable": property.enumerable,
        "writable": true,
        "isOwn": property.is_own,
    });
    if let Some(value) = &property.value {
        out["value"] = remote_object_json(value);
    }
    out
}

pub fn exception_json(details: &ExceptionDetails) -> Value {
    let mut out = json!({
        // Chrome numbers exceptions per session; nothing reads the value, and a
        // constant is more honest than a counter that pretends to order them.
        "exceptionId": 1,
        "text": details.text,
        "lineNumber": details.line,
        "columnNumber": details.column,
        "url": details.url,
    });
    if let Some(exception) = &details.exception {
        out["exception"] = remote_object_json(exception);
    }
    out
}

fn evaluation_json(outcome: &EvaluationResult) -> Value {
    let mut out = json!({ "result": remote_object_json(&outcome.result) });
    if let Some(exception) = &outcome.exception {
        out["exceptionDetails"] = exception_json(exception);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepage_engine::page_api::{RemoteSubtype, RemoteType};

    #[test]
    fn an_object_id_round_trips_as_a_string() {
        assert_eq!(parse_object_id("42").unwrap(), 42);
        // An id the endpoint never minted must be an error, not object 0.
        assert!(parse_object_id("not-an-id").is_err());
        assert!(parse_object_id("").is_err());
        assert!(parse_object_id("-1").is_err());
    }

    #[test]
    fn a_by_value_payload_is_spliced_in_as_structure() {
        let object = RemoteObject {
            kind: Some(RemoteType::Object),
            value_json: Some(String::from(r#"{"a":[1,2]}"#)),
            ..RemoteObject::default()
        };
        let json = remote_object_json(&object);
        // Structure, not a string: a driver reads `result.value.a[0]`.
        assert_eq!(json["value"]["a"][0], 1);
        assert!(json.get("objectId").is_none());
    }

    #[test]
    fn a_handle_is_reported_as_a_string_id() {
        let object = RemoteObject {
            kind: Some(RemoteType::Object),
            subtype: Some(RemoteSubtype::Array),
            object_id: Some(7),
            ..RemoteObject::default()
        };
        let json = remote_object_json(&object);
        assert_eq!(json["objectId"], "7");
        assert_eq!(json["type"], "object");
        assert_eq!(json["subtype"], "array");
        assert!(json.get("value").is_none());
    }

    #[test]
    fn an_unserializable_primitive_keeps_its_own_member() {
        let object = RemoteObject {
            kind: Some(RemoteType::Number),
            unserializable: Some(String::from("NaN")),
            description: Some(String::from("NaN")),
            ..RemoteObject::default()
        };
        let json = remote_object_json(&object);
        assert_eq!(json["unserializableValue"], "NaN");
        // `value: null` would be a different value, so it must be absent.
        assert!(json.get("value").is_none());
    }
}
