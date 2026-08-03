//! The `IO` domain: reading back a stream the browser produced.
//!
//! Exactly one producer today — `Page.printToPDF` with
//! `transferMode: "ReturnAsStream"`, which is what Puppeteer sends by default,
//! so `page.pdf()` does not work without this.
//!
//! The "stream" is a buffer held per connection. That is not a shortcut around
//! a real stream: the PDF is generated in one pass into a `Vec<u8>` anyway
//! (`export-pdf` has no incremental writer), so a chunked reader over it is the
//! honest shape rather than a pretence of streaming generation.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use crate::base64;
use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

/// Bytes handed back per `IO.read` when the caller names no size.
///
/// Chrome's own default. Large enough that a typical PDF is two or three round
/// trips, small enough that one read is not a multi-megabyte JSON string.
pub const DEFAULT_CHUNK: usize = 1 << 20;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "IO.read" => read(connection, request),
        "IO.close" => close(connection, request),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadParams {
    handle: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    size: Option<u64>,
}

fn read(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: ReadParams = request.parse()?;
    let Some(bytes) = connection.stream_bytes(&params.handle) else {
        return Err(ProtocolError::server(format!(
            "Invalid stream handle: {}",
            params.handle
        )));
    };

    // CDP allows an explicit `offset`, but a caller that omits it means "carry
    // on from where the last read stopped", so the position is tracked per
    // handle rather than assumed to be zero.
    let start = match params.offset {
        Some(offset) => usize::try_from(offset).unwrap_or(usize::MAX),
        None => connection.stream_position(&params.handle),
    }
    .min(bytes.len());
    let size = params
        .size
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or(DEFAULT_CHUNK)
        .max(1);
    let end = start.saturating_add(size).min(bytes.len());
    connection.set_stream_position(&params.handle, end);

    Ok(json!({
        "data": base64::encode(&bytes[start..end]),
        // Always base64: a PDF is bytes, and a lossy conversion to text would
        // corrupt it silently.
        "base64Encoded": true,
        "eof": end >= bytes.len(),
    }))
}

fn close(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        handle: String,
    }
    let params: Params = request.parse()?;
    // Closing a handle that is already gone is not an error: a driver
    // abandoning a read legitimately does it.
    connection.close_stream(&params.handle);
    Ok(json!({}))
}
