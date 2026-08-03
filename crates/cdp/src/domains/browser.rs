//! The `Browser` domain: identity, shutdown, download policy.

use std::sync::Arc;

use oxidepage_engine::page_api::{DownloadBehavior, NavigatorProfile};
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
        "Browser.setDownloadBehavior" => set_download_behavior(connection, request),
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
    download_path: Option<String>,
    browser_context_id: Option<String>,
}

/// `Browser.setDownloadBehavior` (ADR-0032 D13).
///
/// Applied to every target of the connection, or to one browsing context when
/// `browserContextId` names one. Per *target*, not per session, because a
/// download is a property of the page rather than of who is watching it.
fn set_download_behavior(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: SetDownloadBehaviorParams = request.parse()?;
    let behavior = match params.behavior.as_str() {
        // `default` for a headless browser with no download directory *is*
        // deny, and saying so is what stops an attachment being parsed as HTML.
        "deny" | "default" => DownloadBehavior::Deny,
        "allow" | "allowAndName" => {
            let Some(path) = params.download_path.as_deref().filter(|p| !p.is_empty()) else {
                return Err(ProtocolError::invalid_params(
                    "setDownloadBehavior('allow') requires a downloadPath: with nowhere to \
                     write, `allow` and `deny` would behave identically",
                ));
            };
            DownloadBehavior::Allow(validated_download_path(path)?)
        }
        other => {
            return Err(ProtocolError::invalid_params(format!(
                "Unknown download behavior: {other}"
            )));
        }
    };

    // Set on the *context*, not on the current target list. A driver commonly
    // sends this before it has created a page, and applying it only to the
    // pages that happen to exist would make that call a silent no-op — the very
    // failure the previous refusal existed to prevent.
    let path = match behavior {
        DownloadBehavior::Deny => None,
        DownloadBehavior::Allow(path) => Some(path),
    };
    let contexts =
        match params.browser_context_id.as_deref() {
            Some(id) => vec![connection.registry.context(id).ok_or_else(|| {
                ProtocolError::server(format!("No browser context with id {id}"))
            })?],
            None => connection.registry.all_contexts(),
        };
    for context in contexts {
        context.set_download_path(path.clone());
    }
    Ok(serde_json::json!({}))
}

/// Turns a driver-supplied download path into one it is safe to write under.
///
/// The path comes off an untrusted frame, so it is resolved to an absolute,
/// symlink-free directory *here* — creating it if need be — rather than being
/// joined with a filename later and hoping. A relative path is accepted (it is
/// resolved against the server's working directory, as Chrome does) but a
/// traversing one is refused outright: `../../etc` names a real directory, and
/// "it resolved fine" is not the question.
fn validated_download_path(path: &str) -> Result<std::path::PathBuf, ProtocolError> {
    let candidate = std::path::Path::new(path);
    if candidate
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ProtocolError::invalid_params(format!(
            "setDownloadBehavior: downloadPath `{path}` traverses upward; pass an absolute path"
        )));
    }
    std::fs::create_dir_all(candidate).map_err(|e| {
        ProtocolError::server(format!(
            "setDownloadBehavior: cannot use `{path}` as a download directory: {e}"
        ))
    })?;
    candidate.canonicalize().map_err(|e| {
        ProtocolError::server(format!("setDownloadBehavior: cannot resolve `{path}`: {e}"))
    })
}
