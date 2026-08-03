//! The `Fetch` domain — the *off* half of it.
//!
//! Request interception is stage 8. Only `Fetch.disable` is implemented, and it
//! is not a stub: "stop intercepting" is a truthful description of what this
//! endpoint does, and the state the caller asks for is the state it already has.
//! Puppeteer's `NetworkManager` sends it unconditionally when a page is created,
//! so refusing it would make `browser.newPage()` throw before a driver could do
//! anything at all.
//!
//! `Fetch.enable` is refused, loudly, because a driver that believed
//! interception was on would sit waiting for `Fetch.requestPaused` events that
//! will never come.

use std::sync::Arc;

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Fetch.disable" => {
            connection.require_session(request)?;
            Ok(serde_json::json!({}))
        }
        "Fetch.enable" => Err(ProtocolError::server(
            "Fetch.enable is not implemented: request interception arrives in a later stage, and \
             a driver told it was enabled would wait forever for Fetch.requestPaused",
        )),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}
