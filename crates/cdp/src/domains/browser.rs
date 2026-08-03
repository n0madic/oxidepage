//! The `Browser` domain: identity, shutdown, download policy.

use std::sync::Arc;

use oxidepage_engine::page_api::NavigatorProfile;
use serde::Deserialize;

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

/// The protocol revision this server is written against.
///
/// Both drivers read it and neither gates on it, but reporting a version we do
/// not implement would be a lie in the one field whose whole job is to say what
/// we implement. `1.3` is the stable protocol both Puppeteer and Playwright
/// target.
const PROTOCOL_VERSION: &str = "1.3";

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Browser.getVersion" => get_version(),
        "Browser.close" => close(connection),
        "Browser.setDownloadBehavior" => set_download_behavior(request),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

fn get_version() -> CommandResult {
    let navigator = NavigatorProfile::default();
    Ok(serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "product": concat!("OxidePage/", env!("CARGO_PKG_VERSION")),
        "revision": env!("CARGO_PKG_VERSION"),
        "userAgent": navigator.user_agent,
        // QuickJS-NG, reported honestly rather than as a V8 version. A driver
        // that branches on this would otherwise take a V8-specific path through
        // an engine that is not V8.
        "jsVersion": "QuickJS-NG",
    }))
}

fn close(connection: &Arc<Connection>) -> CommandResult {
    connection.registry.browser().close();
    // The endpoint exists to serve this browser, so closing it ends the server
    // too. Without this, `oxidepage serve` would keep a port open onto a browser
    // with no pages and no way to make one.
    //
    // Armed, not fired. Signalling here would race the reply: this handler
    // *returns* the result, and only then does the lane queue it to the writer
    // — so stopping the accept loop now can tear the runtime down with the
    // response still unsent. Puppeteer waits for that reply before disposing
    // its transport, which would turn every clean shutdown into a protocol
    // error. The lane fires the signal after the send instead.
    connection.arm_shutdown();
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDownloadBehaviorParams {
    behavior: String,
}

fn set_download_behavior(request: &Request) -> CommandResult {
    let params: SetDownloadBehaviorParams = request.parse()?;
    match params.behavior.as_str() {
        // `deny` and `default` are both "do not download": there is no download
        // machinery at all until stage 8, and `default` for a headless browser
        // with no download directory *is* deny.
        "deny" | "default" => Ok(serde_json::json!({})),
        "allow" | "allowAndName" => Err(ProtocolError::server(
            "Downloads are not implemented: only setDownloadBehavior('deny') is supported",
        )),
        other => Err(ProtocolError::invalid_params(format!(
            "Unknown download behavior: {other}"
        ))),
    }
}
