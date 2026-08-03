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
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
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
    let outcome = session.page.navigate(&params.url, WaitUntil::Load)?;
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
    let url = connection
        .registry
        .info(&session.target_id)
        .map(|info| info.url)
        .unwrap_or_default();
    let loader_id = connection
        .registry
        .loader_id(&session.target_id)
        .unwrap_or_default();
    Ok(serde_json::json!({
        "frameTree": {
            "frame": crate::pump::frame_json(&session.target_id, &loader_id, &url),
            // No child frames until stage 11. The member is present and empty
            // rather than absent, because a driver iterates it unconditionally.
            "childFrames": [],
        }
    }))
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

    let bytes = session.page.screenshot(options)?;
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
    };

    let bytes = session.page.pdf(options, paint)?;
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
    /// Accepted, and **mapped to the main world** — see [`add_init_script`].
    #[serde(default)]
    world_name: Option<String>,
}

/// `Page.addScriptToEvaluateOnNewDocument`.
///
/// # The one-world compromise
///
/// `worldName` is accepted and the script runs in the **main** world, because
/// there is only one (isolated worlds are a later stage). This is a real,
/// documented divergence rather than a stub: the script genuinely runs, at
/// genuinely the right time, and the only property not delivered is isolation.
///
/// It is not free. A driver's injected helpers are visible to page script, and
/// a page that redefines `Array.prototype.map` or `JSON.stringify` can perturb
/// them. Refusing instead was tried and is worse: Puppeteer requests a utility
/// world while *creating every page*, so `browser.newPage()` throws and nothing
/// works at all. Recorded in ADR-0030's deliberate limits.
fn add_init_script(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: AddInitScriptParams = request.parse()?;
    let _ = params.world_name;
    let id = session.page.add_init_script(params.source)?;
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

/// `Page.createIsolatedWorld`, answering with the **main** world's context.
///
/// The same compromise [`add_init_script`] documents, and for the same reason:
/// both drivers create a utility world during page setup, so refusing makes the
/// endpoint unusable rather than honest. What a caller gets is a real, usable
/// execution context — just not an isolated one.
fn create_isolated_world(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(default)]
        world_name: Option<String>,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let world_name = params.world_name.clone().unwrap_or_default();
    // The index is what separates one world's context id from another's, and it
    // must come from the same function the announcement uses — otherwise the id
    // handed back names a context the driver will never be told about.
    let context_id = crate::domains::runtime::world_context_id(
        session.page.execution_context_id()?,
        session.isolated_world_index(&world_name),
    );

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
    ));

    Ok(serde_json::json!({ "executionContextId": context_id }))
}
