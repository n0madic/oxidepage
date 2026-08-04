//! The wire envelope.
//!
//! CDP is JSON-RPC 2.0 without the `"jsonrpc"` member: a client sends
//! `{id, method, params?, sessionId?}` and gets back exactly one
//! `{id, result|error, sessionId?}`; the browser pushes `{method, params,
//! sessionId?}` with no `id`.
//!
//! **Flat mode is the only mode implemented** (ADR-0030). In flat mode a session
//! is addressed by a `sessionId` member on the envelope itself, so one socket
//! multiplexes the browser and every attached target. The nested alternative
//! wraps traffic in `Target.sendMessageToTarget` /
//! `Target.receivedMessageFromTarget`; Playwright requires `flatten: true` and
//! Puppeteer has defaulted to it for years, so building the nested variant would
//! be code with no caller.

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// A command from the client.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub id: i64,
    pub method: String,
    /// Absent and `null` both mean "no arguments"; several drivers send neither
    /// consistently, so this collapses to an empty object rather than failing.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
    /// The order this request was **received** in, stamped by the read loop
    /// from one endpoint-wide counter — the state it is compared against is
    /// page-level, and a target's sessions can span connections.
    ///
    /// Not from the wire: `id` is client-chosen and need not increase. This is
    /// the only place the driver's true ordering survives, because commands are
    /// then spread across lanes that run concurrently — and two of them,
    /// `Fetch.enable` and `Fetch.disable`, write the *same* page-wide config
    /// from *different* lanes (`is_priority`). Without it, an unawaited
    /// `setRequestInterception(true)` followed by `(false)` can apply in the
    /// wrong order and leave interception on.
    #[serde(skip)]
    pub seq: u64,
}

impl Request {
    /// The `params` object, defaulting to `{}`.
    #[must_use]
    pub fn params(&self) -> serde_json::Value {
        match &self.params {
            Some(serde_json::Value::Null) | None => {
                serde_json::Value::Object(serde_json::Map::new())
            }
            Some(value) => value.clone(),
        }
    }

    /// Deserializes `params` into a typed struct.
    ///
    /// A CDP parameter that is optional in the protocol is `Option<T>` in the
    /// struct, so a missing member is not an error; a member of the wrong *type*
    /// is, and becomes `InvalidParams` with serde's message attached rather than
    /// a silent default.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, ProtocolError> {
        serde_json::from_value(self.params()).map_err(|e| {
            ProtocolError::invalid_params(format!("Failed to deserialize params: {e}"))
        })
    }

    /// Like [`Request::parse`], but a wholly absent `params` deserializes as
    /// the type's default.
    ///
    /// For commands whose every member is optional (`Fetch.enable`): serde
    /// cannot default a struct from `null`, and Puppeteer does send
    /// `Fetch.enable` with no params at all.
    pub fn parse_or_default<T: serde::de::DeserializeOwned + Default>(
        &self,
    ) -> Result<T, ProtocolError> {
        match self.params.as_ref() {
            None | Some(serde_json::Value::Null) => Ok(T::default()),
            _ => self.parse(),
        }
    }

    /// The domain half of `Domain.method`.
    #[must_use]
    pub fn domain(&self) -> &str {
        self.method.split_once('.').map_or("", |(domain, _)| domain)
    }
}

/// The answer to exactly one [`Request`].
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl Response {
    #[must_use]
    pub fn ok(id: i64, session_id: Option<String>, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
            session_id,
        }
    }

    #[must_use]
    pub fn err(id: i64, session_id: Option<String>, error: ProtocolError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
            session_id,
        }
    }

    /// Builds the response for a dispatched command.
    #[must_use]
    pub fn from_result(
        id: i64,
        session_id: Option<String>,
        result: crate::error::CommandResult,
    ) -> Self {
        match result {
            Ok(value) => Self::ok(id, session_id, value),
            Err(error) => Self::err(id, session_id, error),
        }
    }
}

/// A pushed notification. No `id` — that is what distinguishes it from a
/// [`Response`] on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub method: String,
    pub params: serde_json::Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl Event {
    /// A browser-level event, addressed to no session.
    #[must_use]
    pub fn browser(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            method: method.into(),
            params,
            session_id: None,
        }
    }

    /// An event scoped to one attached session.
    #[must_use]
    pub fn session(
        session_id: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            method: method.into(),
            params,
            session_id: Some(session_id.into()),
        }
    }
}

/// Anything the server writes back to one socket.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Outbound {
    Response(Response),
    Event(Event),
}

impl Outbound {
    /// Serializes for the wire.
    ///
    /// Infallible in practice — every payload is built from `serde_json::Value`
    /// and plain structs, neither of which can fail to serialize — but a
    /// non-finite `f64` reaching a `Value::Number` is the one shape that could,
    /// and dropping the frame silently would hang the client on a reply that
    /// never comes. So the failure is rendered as text and surfaced.
    #[must_use]
    pub fn to_wire(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            let fallback = ProtocolError::internal(format!("failed to serialize response: {e}"));
            let id = match self {
                Outbound::Response(response) => response.id,
                Outbound::Event(_) => 0,
            };
            serde_json::to_string(&Response::err(id, None, fallback)).unwrap_or_else(|_| {
                String::from(r#"{"id":0,"error":{"code":-32603,"message":"serialization failed"}}"#)
            })
        })
    }
}

impl From<Response> for Outbound {
    fn from(response: Response) -> Self {
        Outbound::Response(response)
    }
}

impl From<Event> for Outbound {
    fn from(event: Event) -> Self {
        Outbound::Event(event)
    }
}

/// Parses one inbound text frame.
///
/// A frame that is not an object with an integer `id` cannot be answered — there
/// is nothing to correlate a reply with — so it is reported against `id: 0`, the
/// same thing Chrome does.
pub fn parse_request(frame: &str) -> Result<Request, Response> {
    let value: serde_json::Value = serde_json::from_str(frame)
        .map_err(|e| Response::err(0, None, ProtocolError::parse_error(e.to_string())))?;

    // Recover the id (and session) before full validation, so a request with a
    // bad `method` still gets a correlated failure rather than one against 0.
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let session_id = value
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    serde_json::from_value::<Request>(value).map_err(|e| {
        Response::err(
            id,
            session_id,
            ProtocolError::invalid_request(format!("Message has invalid shape: {e}")),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_request() {
        let request = parse_request(r#"{"id":1,"method":"Browser.getVersion"}"#).unwrap();
        assert_eq!(request.id, 1);
        assert_eq!(request.method, "Browser.getVersion");
        assert_eq!(request.domain(), "Browser");
        assert!(request.session_id.is_none());
        assert_eq!(request.params(), serde_json::json!({}));
    }

    #[test]
    fn null_params_collapse_to_an_empty_object() {
        let request = parse_request(r#"{"id":2,"method":"Page.enable","params":null}"#).unwrap();
        assert_eq!(request.params(), serde_json::json!({}));
    }

    #[test]
    fn keeps_the_session_id() {
        let request =
            parse_request(r#"{"id":3,"method":"Page.enable","sessionId":"abc"}"#).unwrap();
        assert_eq!(request.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn malformed_json_is_a_parse_error_against_id_zero() {
        let response = parse_request("not json").unwrap_err();
        assert_eq!(response.id, 0);
        assert_eq!(response.error.unwrap().code, ProtocolError::PARSE_ERROR);
    }

    #[test]
    fn a_bad_shape_still_answers_the_right_id() {
        // `method` must be a string; the id is recovered anyway so the client
        // can retire its pending promise instead of waiting forever.
        let response = parse_request(r#"{"id":7,"method":42}"#).unwrap_err();
        assert_eq!(response.id, 7);
        assert_eq!(response.error.unwrap().code, ProtocolError::INVALID_REQUEST);
    }

    #[test]
    fn a_response_omits_the_absent_half() {
        let wire =
            Outbound::Response(Response::ok(1, None, serde_json::json!({"ok":true}))).to_wire();
        assert_eq!(wire, r#"{"id":1,"result":{"ok":true}}"#);
    }

    #[test]
    fn an_error_response_carries_no_result() {
        let wire = Outbound::Response(Response::err(
            5,
            Some("s1".into()),
            ProtocolError::method_not_found("Foo.bar"),
        ))
        .to_wire();
        assert_eq!(
            wire,
            r#"{"id":5,"error":{"code":-32601,"message":"'Foo.bar' wasn't found"},"sessionId":"s1"}"#
        );
    }

    #[test]
    fn an_event_has_no_id() {
        let wire = Outbound::Event(Event::session(
            "s1",
            "Page.loadEventFired",
            serde_json::json!({"timestamp": 1.5}),
        ))
        .to_wire();
        assert_eq!(
            wire,
            r#"{"method":"Page.loadEventFired","params":{"timestamp":1.5},"sessionId":"s1"}"#
        );
    }
}
