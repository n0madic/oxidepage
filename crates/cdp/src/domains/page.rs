//! The `Page` domain: navigation, history, capture, dialogs.
//!
//! Every command here is session-scoped — it acts on the target the session is
//! attached to — so each begins by resolving the session rather than trusting
//! the envelope.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use oxidepage_engine::page_api::{
    ImageFormat, Margins, PaintOptions, PaperSize, PdfOptions, Point, Rect, ScreenshotOptions,
    Size, WaitUntil,
};
use serde::Deserialize;

use crate::base64;
use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Page.enable" => set_enabled(connection, request, true),
        "Page.disable" => set_enabled(connection, request, false),
        "Page.setLifecycleEventsEnabled" => set_lifecycle_enabled(connection, request),
        "Page.setInterceptFileChooserDialog" => set_intercept_file_chooser(connection, request),
        "Page.navigate" => navigate(connection, request),
        "Page.reload" => reload(connection, request),
        "Page.stopLoading" => stop_loading(connection, request),
        "Page.getFrameTree" => get_frame_tree(connection, request),
        "Page.getNavigationHistory" => get_navigation_history(connection, request),
        "Page.navigateToHistoryEntry" => navigate_to_history_entry(connection, request),
        "Page.captureScreenshot" => capture_screenshot(connection, request),
        "Page.printToPDF" => print_to_pdf(connection, request),
        "Page.bringToFront" => bring_to_front(connection, request),
        "Page.close" => close(connection, request),
        "Page.handleJavaScriptDialog" => handle_dialog(connection, request),
        "Page.addScriptToEvaluateOnNewDocument" => add_init_script(connection, request),
        "Page.removeScriptToEvaluateOnNewDocument" => remove_init_script(connection, request),
        "Page.createIsolatedWorld" => create_isolated_world(connection, request),
        "Page.setBypassCSP" => set_bypass_csp(connection, request),
        // The metrics are a pure layout read, so the handler lives beside the
        // rest of the geometry vocabulary in `domains::dom`.
        "Page.getLayoutMetrics" => crate::domains::dom::get_layout_metrics(connection, request),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

/// Accepted, not refused. There is no CSP enforcement anywhere in the engine,
/// so there is nothing to bypass and nothing this could be lying about — the
/// answer is already what the driver is asking for, and refusing would break
/// `browser.newContext({ bypassCSP: true })` over a capability the page does
/// not have in the first place.
///
/// It still requires a session, like every other `Page` method: a command that
/// named no target was never routed to one, and answering it `{}` hides exactly
/// the bookkeeping divergence `require_session` exists to surface.
fn set_bypass_csp(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    connection.require_session(request)?;
    Ok(serde_json::json!({}))
}

fn set_enabled(connection: &Arc<Connection>, request: &Request, enabled: bool) -> CommandResult {
    let session = connection.require_session(request)?;
    session.flags.page.store(enabled, Ordering::Relaxed);
    // `Page.enable` turns lifecycle events on as a side effect in Chrome, and
    // Puppeteer relies on that for `waitUntil: 'load'` without ever sending
    // `setLifecycleEventsEnabled`. Disabling the domain takes them with it.
    session.flags.lifecycle.store(enabled, Ordering::Relaxed);
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetLifecycleEventsEnabledParams {
    enabled: bool,
}

fn set_lifecycle_enabled(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: SetLifecycleEventsEnabledParams = request.parse()?;
    session
        .flags
        .lifecycle
        .store(params.enabled, Ordering::Relaxed);
    Ok(serde_json::json!({}))
}

/// `Page.setInterceptFileChooserDialog` (ADR-0032 D12).
///
/// Page state, not session state: the chooser is a property of the page, and
/// two sessions watching one target must not each get a different answer to
/// "does clicking a file input do anything". The *event* is still gated on
/// `Page.enable` per session, as every other `Page` event is.
fn set_intercept_file_chooser(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        enabled: bool,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    session.page.set_intercept_file_chooser(params.enabled)?;
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NavigateParams {
    url: String,
}

fn navigate(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: NavigateParams = request.parse()?;

    // `WaitUntil::Load` and not something shorter: CDP's `Page.navigate`
    // answers once the navigation has *committed*, and the closest the engine
    // offers is a completed load. Answering earlier is not available — the
    // navigation is a blocking call on the page thread — and answering later is
    // what a driver waiting on `Page.lifecycleEvent` expects anyway.
    let (outcome, was_download) = session
        .page
        .navigate_with_download_flag(&params.url, WaitUntil::Load)?;
    let loader_id = connection
        .registry
        .loader_id(&session.target_id)
        .unwrap_or_default();

    let mut result = serde_json::json!({
        "frameId": session.target_id,
        "loaderId": loader_id,
    });
    // A navigation that failed is *not* a protocol error: Chrome answers with
    // `errorText` and Puppeteer turns that into a rejected `page.goto`. Failing
    // the command instead would lose the URL that was attempted.
    if let Err(message) = outcome {
        result["errorText"] = serde_json::json!(message);
    } else if was_download {
        // A download navigation does not commit a document, and Chrome reports
        // that to `Page.navigate` callers as an aborted load.
        result["errorText"] = serde_json::json!("net::ERR_ABORTED");
    }
    Ok(result)
}

fn reload(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    // `ignoreCache` is accepted and ignored: the engine's reload always
    // bypasses the cache, so honoring `false` would mean serving a *staler*
    // document than asked for. Documented rather than silently divergent.
    session
        .page
        .reload(WaitUntil::Load)?
        .map_err(|message| ProtocolError::server(format!("Reload failed: {message}")))?;
    Ok(serde_json::json!({}))
}

fn stop_loading(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    session.page.stop_loading()?;
    Ok(serde_json::json!({}))
}

fn get_frame_tree(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    // One snapshot, so the URL and the loader describing it are the same
    // instant's. The **committed** loader and the frame's current URL:
    // `getFrameTree` describes the document the frame has right now, where
    // `frameNavigated` describes the one its own event is about — which is why
    // `Frame::json` takes both rather than reading them off the frame.
    let frame = connection
        .registry
        .frame(&session.target_id)
        .ok_or_else(|| ProtocolError::no_target(&session.target_id))?;
    // The engine's own view of the page's browsing contexts (ADR-0035). Every
    // frame's loader is a protocol fact the engine knows nothing about, so both
    // the top-level entry and each nested one read it from the registry.
    let contexts = session.page.frame_tree().unwrap_or_default();
    let tree = build_frame_tree(connection, &session.target_id, &frame, &contexts);
    Ok(serde_json::json!({ "frameTree": tree }))
}

/// Assembles `Page.FrameTree` from the engine's flat, parent-first list.
fn build_frame_tree(
    connection: &Arc<Connection>,
    target_id: &str,
    top: &crate::frame::Frame,
    contexts: &[oxidepage_engine::page_api::FrameInfo],
) -> serde_json::Value {
    let Some(root) = contexts.first() else {
        // No contexts reported (an embedder page with no frame plumbing):
        // answer for the one frame the target has, as before.
        return serde_json::json!({
            "frame": top.json(top.loader_id(), top.url()),
            "childFrames": [],
        });
    };
    frame_subtree(connection, target_id, top, contexts, root)
}

fn frame_subtree(
    connection: &Arc<Connection>,
    target_id: &str,
    top: &crate::frame::Frame,
    contexts: &[oxidepage_engine::page_api::FrameInfo],
    node: &oxidepage_engine::page_api::FrameInfo,
) -> serde_json::Value {
    let is_main = node.parent.is_none();
    let id = crate::frame::frame_id_for(target_id, node.id, is_main);
    let mut frame = if is_main {
        top.json(top.loader_id(), top.url())
    } else {
        serde_json::json!({
            "id": id,
            // Its own document's loader. The top frame's is the fallback for a
            // frame this connection has not seen an event for — a session that
            // enabled nothing and asked straight away — where a made-up id would
            // be worse than a stale one.
            "loaderId": connection
                .registry
                .child_loader(target_id, &id)
                .unwrap_or_else(|| top.loader_id().to_owned()),
            "url": node.url,
            "securityOrigin": crate::frame::security_origin(&node.url),
            "mimeType": "text/html",
        })
    };
    if let Some(parent) = node.parent
        && let Some(object) = frame.as_object_mut()
    {
        let parent_is_main = contexts
            .iter()
            .find(|c| c.id == parent)
            .is_some_and(|c| c.parent.is_none());
        object.insert(
            "parentId".to_owned(),
            serde_json::Value::String(crate::frame::frame_id_for(
                target_id,
                parent,
                parent_is_main,
            )),
        );
    }
    let children: Vec<serde_json::Value> = contexts
        .iter()
        .filter(|c| c.parent == Some(node.id))
        .map(|child| frame_subtree(connection, target_id, top, contexts, child))
        .collect();
    serde_json::json!({ "frame": frame, "childFrames": children })
}

fn get_navigation_history(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let history = session.page.navigation_history()?;
    let entries: Vec<serde_json::Value> = history
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            serde_json::json!({
                // The index *is* the id: `navigateToHistoryEntry` takes it back,
                // and the engine's history has no id of its own.
                "id": index,
                "url": entry.url,
                "userTypedURL": entry.url,
                // Titles are not retained per entry — the engine keeps only the
                // live document's — so reporting one for a past entry would be
                // a guess.
                "title": "",
                "transitionType": "link",
            })
        })
        .collect();
    Ok(serde_json::json!({
        "currentIndex": history.current_index,
        "entries": entries,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NavigateToHistoryEntryParams {
    entry_id: i64,
}

fn navigate_to_history_entry(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: NavigateToHistoryEntryParams = request.parse()?;
    let index = usize::try_from(params.entry_id)
        .map_err(|_| ProtocolError::invalid_params("entryId must be a non-negative index"))?;
    session
        .page
        .navigate_to_history_entry(index, WaitUntil::Load)?
        .map_err(|message| {
            ProtocolError::server(format!("History navigation failed: {message}"))
        })?;
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewportParams {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    #[serde(default)]
    scale: Option<f32>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CaptureScreenshotParams {
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    quality: Option<u8>,
    #[serde(default)]
    clip: Option<ViewportParams>,
    #[serde(default)]
    capture_beyond_viewport: Option<bool>,
    #[serde(default)]
    from_surface: Option<bool>,
}

fn capture_screenshot(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: CaptureScreenshotParams = request.parse()?;

    let format = match params.format.as_deref() {
        None | Some("png") => ImageFormat::Png,
        Some("jpeg") => ImageFormat::Jpeg,
        // WebP encoding is a documented non-goal (ADR-0026). Refusing is what
        // lets a driver fall back to PNG instead of shipping a broken file.
        Some(other) => {
            return Err(ProtocolError::invalid_params(format!(
                "Unsupported screenshot format: {other}"
            )));
        }
    };

    let clip = params.clip.as_ref().map(|clip| Rect {
        origin: Point {
            x: clip.x,
            y: clip.y,
        },
        size: Size {
            width: clip.width,
            height: clip.height,
        },
    });
    // Chrome's `captureBeyondViewport` is what Puppeteer's `fullPage` maps to
    // when there is no clip. With a clip the clip already says what to capture.
    let full_page = params.capture_beyond_viewport.unwrap_or(false) && clip.is_none();
    let _ = params.from_surface;

    // The clip's scale wins, then the page's own device pixel ratio — the one
    // `Emulation.setDeviceMetricsOverride` set. Defaulting to 1.0 instead made
    // `page.setViewport({deviceScaleFactor: 2})` silently produce 1x images.
    let scale = params
        .clip
        .as_ref()
        .and_then(|clip| clip.scale)
        .filter(|scale| *scale > 0.0)
        .unwrap_or_else(|| session.page.viewport().map_or(1.0, |view| view.dpr));
    let options = ScreenshotOptions {
        dpr: scale,
        full_page,
        clip,
        format,
        quality: params.quality.unwrap_or(80).clamp(1, 100),
        ..ScreenshotOptions::default()
    };

    // Two distinct failures, kept distinct: the layout abort carries its own
    // cause, and reusing the encoder's message for it would say the wrong
    // thing about why there is no picture (ADR-0037 D7).
    let bytes = session
        .page
        .screenshot(options)?
        .map_err(ProtocolError::server)?;
    if bytes.is_empty() {
        return Err(ProtocolError::server("Screenshot encoding failed"));
    }
    Ok(serde_json::json!({ "data": base64::encode(&bytes) }))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PrintToPdfParams {
    #[serde(default)]
    landscape: Option<bool>,
    #[serde(default)]
    print_background: Option<bool>,
    #[serde(default)]
    scale: Option<f32>,
    #[serde(default)]
    paper_width: Option<f32>,
    #[serde(default)]
    paper_height: Option<f32>,
    #[serde(default)]
    margin_top: Option<f32>,
    #[serde(default)]
    margin_bottom: Option<f32>,
    #[serde(default)]
    margin_left: Option<f32>,
    #[serde(default)]
    margin_right: Option<f32>,
    /// Chrome accepts `ReturnAsBase64` and `ReturnAsStream`. Only the former is
    /// implemented — there is no `IO` domain to read a stream through.
    #[serde(default)]
    transfer_mode: Option<String>,
}

/// CSS pixels per inch. `Page.printToPDF` states its paper and margins in
/// inches; the engine works in CSS px throughout.
const PX_PER_INCH: f32 = 96.0;

fn print_to_pdf(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: PrintToPdfParams = request.parse()?;

    let as_stream = match params.transfer_mode.as_deref() {
        None | Some("ReturnAsBase64") => false,
        Some("ReturnAsStream") => true,
        Some(mode) => {
            return Err(ProtocolError::invalid_params(format!(
                "Unsupported transferMode: {mode}"
            )));
        }
    };

    let defaults = PdfOptions::default();
    let paper = match (params.paper_width, params.paper_height) {
        (Some(width), Some(height)) if width > 0.0 && height > 0.0 => PaperSize {
            width: width * PX_PER_INCH,
            height: height * PX_PER_INCH,
        },
        _ => defaults.paper,
    };
    let inches = |value: Option<f32>, fallback: f32| {
        value.map_or(fallback, |inches| inches.max(0.0) * PX_PER_INCH)
    };
    let options = PdfOptions {
        paper,
        landscape: params.landscape.unwrap_or(defaults.landscape),
        scale: params.scale.unwrap_or(defaults.scale).clamp(0.1, 2.0),
        margins: Margins {
            top: inches(params.margin_top, defaults.margins.top),
            right: inches(params.margin_right, defaults.margins.right),
            bottom: inches(params.margin_bottom, defaults.margins.bottom),
            left: inches(params.margin_left, defaults.margins.left),
        },
        ..defaults
    };
    let paint = PaintOptions {
        print_background: params
            .print_background
            .unwrap_or(PaintOptions::default().print_background),
        // The page fills these in from its own frame tree at build time.
        ..PaintOptions::default()
    };

    let bytes = session
        .page
        .pdf(options, paint)?
        .map_err(ProtocolError::server)?;
    if bytes.is_empty() {
        return Err(ProtocolError::server("PDF generation failed"));
    }
    if as_stream {
        // Puppeteer asks for a stream by default and then drains it through
        // `IO.read`, so `page.pdf()` does not work without this path.
        return Ok(serde_json::json!({
            "data": "",
            "stream": connection.open_stream(bytes),
        }));
    }
    Ok(serde_json::json!({ "data": base64::encode(&bytes) }))
}

fn bring_to_front(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    // Nothing to raise without a window manager, but the session must exist —
    // and Puppeteer calls this on `page.bringToFront()` and before some
    // screenshots, so refusing outright would break ordinary scripts.
    connection.require_session(request)?;
    Ok(serde_json::json!({}))
}

fn close(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    session.page.close();
    connection.registry.destroy(&session.target_id);
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HandleDialogParams {
    accept: bool,
    #[serde(default)]
    prompt_text: Option<String>,
}

fn handle_dialog(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    use oxidepage_engine::page_api::DialogResponse;

    let session = connection.require_session(request)?;
    let params: HandleDialogParams = request.parse()?;
    let response = match (params.accept, params.prompt_text) {
        (false, _) => DialogResponse::Dismiss,
        (true, Some(text)) => DialogResponse::AcceptWith(text),
        (true, None) => DialogResponse::Accept,
    };
    // Both refusals below would otherwise surface as `EngineError::Timeout`,
    // i.e. "Timed out waiting for the target" — which describes an unresponsive
    // page and would send a driver looking for the wrong bug.
    if !session.page.awaits_dialog_answer() {
        return Err(ProtocolError::server(
            "No dialog handler is installed: this browser context answers dialogs \
             itself (DialogPolicy::Dismiss or ::Accept), so there is nothing to answer",
        ));
    }
    if !session.page.dialog_pending() {
        return Err(ProtocolError::server("No dialog is showing"));
    }

    // This must not go through a lane's ordinary path into the page: a page
    // parked in a dialog services no ordinary job, so the answer travels on the
    // dedicated rendezvous channel `PageHandle::answer_dialog` owns
    // (ADR-0027 D11).
    session.page.answer_dialog(response)?;
    Ok(serde_json::json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddInitScriptParams {
    source: String,
    /// The world to run the script in, created if it does not exist yet.
    /// Absent — or empty — is the main world.
    #[serde(default)]
    world_name: Option<String>,
}

/// `Page.addScriptToEvaluateOnNewDocument`.
///
/// `worldName` is honoured (ADR-0033 D9): the script runs in that world and
/// nowhere else, against a global rebuilt fresh at every commit — which is what
/// makes a driver's `addInitScript` invisible to page script. The registration
/// survives navigation, which is the whole point of the command.
fn add_init_script(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: AddInitScriptParams = request.parse()?;
    let id = session
        .page
        .add_init_script_for(params.source, params.world_name)?;
    Ok(serde_json::json!({ "identifier": id.to_string() }))
}

fn remove_init_script(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        identifier: String,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = params.identifier.parse::<u64>().map_err(|_| {
        ProtocolError::invalid_params(format!("Unknown identifier: {}", params.identifier))
    })?;
    // Removing one that is already gone is not an error: a driver tearing down
    // after a navigation legitimately does it.
    session.page.remove_init_script(id)?;
    Ok(serde_json::json!({}))
}

/// `Page.createIsolatedWorld` — a real isolated world (ADR-0033).
///
/// **Idempotent by name** within a document: asking twice returns the same
/// context and re-announces it. Chrome mints a fresh context per call, but
/// drivers call this once per navigation expecting to rebind, and the protocol
/// has no way to destroy the surplus, so minting would leak a context per
/// navigation for the life of the page (D9).
fn create_isolated_world(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(default)]
        world_name: Option<String>,
        #[serde(default)]
        frame_id: Option<String>,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let world_name = params.world_name.clone().unwrap_or_default();
    if world_name.is_empty() {
        return Err(ProtocolError::server("worldName is required"));
    }
    // **The frame matters.** Playwright evaluates every locator in a utility
    // world *of that frame*, so a world minted on the main frame would see the
    // wrong document and the wrong tree — which is exactly what left
    // `frameLocator()` waiting (ADR-0035 D3).
    let frame = match params.frame_id.as_deref() {
        // The main frame's CDP id *is* the target id, by construction — no
        // lookup, and no round trip to the page thread for the common case.
        None | Some("") => None,
        Some(id) if id == session.target_id => None,
        Some(id) => {
            let contexts = session.page.frame_tree().unwrap_or_default();
            Some(
                crate::frame::frame_by_cdp_id(&session.target_id, &contexts, id)
                    .ok_or_else(|| ProtocolError::server("no frame with the given id"))?,
            )
        }
    };
    let world = match frame {
        Some(frame) => session
            .page
            .create_isolated_world_in(frame, world_name.clone())?,
        None => session.page.create_isolated_world(world_name.clone())?,
    }
    .map_err(ProtocolError::server)?;
    let context_id = world.context_id;

    // Announcing the context is not optional. A driver does not use the id this
    // command returns — it waits for a `Runtime.executionContextCreated` whose
    // `auxData.isDefault` is false and whose `name` is the world it asked for,
    // and binds its isolated realm to *that*. Without the event, every
    // isolated-realm operation (`page.title`, `page.$`, `waitForSelector`,
    // `exposeFunction`) blocks until the driver's own protocol timeout.
    connection.emit(crate::domains::runtime::execution_context_created_named(
        connection,
        &session,
        &world_name,
        /* is_default */ false,
        context_id,
        params.frame_id.as_deref(),
    ));

    Ok(serde_json::json!({ "executionContextId": context_id }))
}
