//! The `Log` domain: browser-originated messages, as opposed to `console.*`.
//!
//! The split is the engine's, not an invention here. `ScriptErrorKind::Resource`
//! is the kind ADR-0025 introduced for the ~15 event-loop sites that report a
//! stylesheet 404 or an unresolvable module specifier — none of which is an
//! uncaught exception, and filing them under `Runtime.exceptionThrown` would
//! make that event untrustworthy. They surface here instead, which is where
//! Chrome puts the same messages.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Log.enable" => set_enabled(connection, request, true),
        "Log.disable" => set_enabled(connection, request, false),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

fn set_enabled(connection: &Arc<Connection>, request: &Request, enabled: bool) -> CommandResult {
    let session = connection.require_session(request)?;
    session.flags.log.store(enabled, Ordering::Relaxed);
    Ok(serde_json::json!({}))
}
