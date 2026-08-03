//! The `Performance` domain.
//!
//! Puppeteer sends `Performance.enable` while creating every page, so this has
//! to answer or `browser.newPage()` throws before a driver can do anything.
//!
//! It is not a stub: `getMetrics` reports the counters the engine actually
//! keeps, and no others. CDP models metrics as a name/value list precisely
//! because the set varies between builds, so a short honest list is a valid
//! answer where inventing `LayoutCount: 0` would be a lie a profiler would act
//! on.

use std::sync::Arc;

use serde_json::json;

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        // Metrics are computed on demand, so there is nothing to switch on:
        // the state the caller asks for is the state it gets.
        "Performance.enable" | "Performance.disable" => {
            connection.require_session(request)?;
            Ok(json!({}))
        }
        "Performance.getMetrics" => get_metrics(connection, request),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

fn get_metrics(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let heap = session.page.js_heap_used()?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64());

    Ok(json!({
        "metrics": [
            { "name": "Timestamp", "value": timestamp },
            { "name": "JSHeapUsedSize", "value": heap },
        ]
    }))
}
