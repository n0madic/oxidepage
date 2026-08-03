//! The `Target` domain: discovery, attachment, page and context lifecycle.
//!
//! This is where flat mode is enforced. In flat mode every message carries its
//! own `sessionId` and one socket multiplexes the browser plus every attached
//! target; the nested alternative tunnels traffic through
//! `Target.sendMessageToTarget`. Playwright requires `flatten: true`, Puppeteer
//! has sent it for years, and building both would mean a second routing path
//! with no caller — so an explicit `flatten: false` is refused rather than
//! silently served as flat (ADR-0030).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use oxidepage_engine::NewPageOptions;
use oxidepage_engine::page_api::WaitUntil;
use serde::Deserialize;

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Target.setDiscoverTargets" => set_discover_targets(connection, request),
        "Target.setAutoAttach" => set_auto_attach(connection, request),
        "Target.getTargets" => get_targets(connection),
        "Target.createTarget" => create_target(connection, request),
        "Target.closeTarget" => close_target(connection, request),
        "Target.activateTarget" => activate_target(connection, request),
        "Target.attachToTarget" => attach_to_target(connection, request),
        "Target.detachFromTarget" => detach_from_target(connection, request),
        "Target.createBrowserContext" => create_browser_context(connection),
        "Target.disposeBrowserContext" => dispose_browser_context(connection, request),
        "Target.getBrowserContexts" => get_browser_contexts(connection),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

/// Rejects the nested variant.
///
/// `None` means the member was absent, which every current driver treats as
/// "the server decides"; we decide flat.
fn require_flat(flatten: Option<bool>) -> Result<(), ProtocolError> {
    if flatten == Some(false) {
        return Err(ProtocolError::server(
            "Only flat session mode is supported: pass flatten: true",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDiscoverTargetsParams {
    discover: bool,
}

fn set_discover_targets(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: SetDiscoverTargetsParams = request.parse()?;
    let was_on = connection.discover.swap(params.discover, Ordering::Relaxed);
    // Turning discovery on reports the targets that already exist, so a driver
    // that connects to a running browser sees the same stream as one that was
    // watching from the start. Without this, `puppeteer.connect()` to a browser
    // with open pages finds none of them.
    if params.discover && !was_on {
        for info in connection.registry.infos() {
            connection.emit(crate::message::Event::browser(
                "Target.targetCreated",
                serde_json::json!({ "targetInfo": info }),
            ));
        }
    }
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetAutoAttachParams {
    auto_attach: bool,
    #[serde(default)]
    wait_for_debugger_on_start: bool,
    #[serde(default)]
    flatten: Option<bool>,
}

fn set_auto_attach(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: SetAutoAttachParams = request.parse()?;
    require_flat(params.flatten)?;

    // A `sessionId` on this command scopes auto-attach to that target's own
    // children — out-of-process iframes and workers. There are none (stage 11),
    // and reporting failure would break drivers that send it unconditionally,
    // so it is accepted and has no children to act on.
    if request.session_id.is_some() {
        return Ok(serde_json::json!({}));
    }

    connection
        .wait_for_debugger
        .store(params.wait_for_debugger_on_start, Ordering::Relaxed);
    let was_on = connection
        .auto_attach
        .swap(params.auto_attach, Ordering::Relaxed);

    if params.auto_attach && !was_on {
        for info in connection.registry.infos() {
            if connection.sessions_for(&info.target_id).is_empty() {
                connection.attach(&info.target_id)?;
            }
        }
    }
    Ok(serde_json::json!({}))
}

fn get_targets(connection: &Arc<Connection>) -> CommandResult {
    Ok(serde_json::json!({ "targetInfos": connection.registry.infos() }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTargetParams {
    url: String,
    #[serde(default)]
    browser_context_id: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

fn create_target(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: CreateTargetParams = request.parse()?;

    let context = match &params.browser_context_id {
        Some(id) => connection
            .registry
            .context(id)
            .ok_or_else(|| ProtocolError::server(format!("Failed to find browser context {id}")))?,
        None => connection.registry.browser().default_context(),
    };

    // `about:blank` is the overwhelmingly common case — it is what
    // `browser.newPage()` sends — and it is *not* a navigation: it is the
    // document URL a fresh page already has. Navigating to it would put a real
    // fetch of an unfetchable scheme on the critical path of every new page.
    let blank = params.url.is_empty() || params.url == "about:blank";
    let options = NewPageOptions {
        // Normalized, not echoed: `{"url": ""}` must still produce a document
        // whose `location.href` is `about:blank`, not the empty string.
        url: blank.then(|| String::from("about:blank")),
        viewport: viewport_override(params.width, params.height),
        // **Not suspended, even under `waitForDebuggerOnStart`.** A suspended
        // page defers every ordinary job until `resume()`, and a driver sends
        // its whole session setup — `Page.addScriptToEvaluateOnNewDocument`
        // among it — *before* `Runtime.runIfWaitingForDebugger`. So suspending
        // here does not delay the page, it deadlocks the setup until the
        // command timeout. Honouring the flag needs the setup path to be
        // suspension-safe first; until then the page simply starts running,
        // which is what a driver that never sends the flag already gets.
        suspended: false,
        ..NewPageOptions::default()
    };

    let target_id = connection.registry.create_page(&context, options)?;

    if !blank && let Some(page) = connection.registry.page(&target_id) {
        // Fire and forget: `Target.createTarget` answers with a targetId, it
        // does not wait for the load. A driver that wants the load waits for
        // `Page.loadEventFired` or calls `Page.navigate` itself.
        let url = params.url.clone();
        page.post(move |p| {
            let _ = p.navigate(&url, WaitUntil::Load);
        })?;
    }

    Ok(serde_json::json!({ "targetId": target_id }))
}

fn viewport_override(
    width: Option<u32>,
    height: Option<u32>,
) -> Option<oxidepage_engine::page_api::Viewport> {
    // CDP treats 0 as "unset" for both, and a zero-area viewport would lay out
    // nothing at all.
    let (width, height) = (width.filter(|w| *w > 0)?, height.filter(|h| *h > 0)?);
    Some(oxidepage_engine::page_api::Viewport {
        width: width as f32,
        height: height as f32,
        ..Default::default()
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetIdParams {
    target_id: String,
}

fn close_target(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: TargetIdParams = request.parse()?;
    let Some(page) = connection.registry.page(&params.target_id) else {
        return Err(ProtocolError::no_target(&params.target_id));
    };
    page.close();
    // The pump also removes the target when the stream ends; doing it here too
    // makes `closeTarget` synchronous from the driver's point of view, and
    // `destroy` is idempotent precisely so the two can race.
    connection.registry.destroy(&params.target_id);
    Ok(serde_json::json!({ "success": true }))
}

fn activate_target(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: TargetIdParams = request.parse()?;
    // Bringing a target to the front means something only with a window
    // manager. The target must still exist, though — answering success for an
    // id that names nothing would hide a driver bug.
    if connection.registry.info(&params.target_id).is_none() {
        return Err(ProtocolError::no_target(&params.target_id));
    }
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachToTargetParams {
    target_id: String,
    #[serde(default)]
    flatten: Option<bool>,
}

fn attach_to_target(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: AttachToTargetParams = request.parse()?;
    require_flat(params.flatten)?;
    let session_id = connection.attach(&params.target_id)?;
    Ok(serde_json::json!({ "sessionId": session_id }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetachFromTargetParams {
    #[serde(default)]
    session_id: Option<String>,
}

fn detach_from_target(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: DetachFromTargetParams = request.parse()?;
    // The parameter wins over the envelope: a driver detaching session A may
    // send the command *on* session B.
    let session_id = params
        .session_id
        .or_else(|| request.session_id.clone())
        .ok_or_else(|| {
            ProtocolError::invalid_params("Target.detachFromTarget requires a sessionId")
        })?;

    if connection.detach(&session_id).is_none() {
        return Err(ProtocolError::no_session(&session_id));
    }
    Ok(serde_json::json!({}))
}

fn create_browser_context(connection: &Arc<Connection>) -> CommandResult {
    // Configured like the default context, not like a stock one. Everything the
    // operator set on `oxidepage serve` — the viewport, and `DialogPolicy::Ask`,
    // without which `page.on('dialog', …)` can never be honoured — lives on the
    // default context, and `browser.createBrowserContext()` is how a driver
    // asks for an *isolated* context, not a differently configured browser.
    let browser = connection.registry.browser();
    let context = browser.new_context(browser.default_context().options());
    let id = connection.registry.adopt_context(&context);
    Ok(serde_json::json!({ "browserContextId": id }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisposeBrowserContextParams {
    browser_context_id: String,
}

fn dispose_browser_context(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let params: DisposeBrowserContextParams = request.parse()?;
    connection
        .registry
        .dispose_context(&params.browser_context_id)
        .map_err(|_| {
            ProtocolError::server(format!(
                "Failed to find context with id {}",
                params.browser_context_id
            ))
        })?;
    Ok(serde_json::json!({}))
}

fn get_browser_contexts(connection: &Arc<Connection>) -> CommandResult {
    Ok(serde_json::json!({ "browserContextIds": connection.registry.context_ids() }))
}
