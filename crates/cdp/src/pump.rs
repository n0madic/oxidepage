//! Turning one page event into the protocol events a session asked for.
//!
//! The engine already records exactly the milestones CDP wants —
//! [`NavigationEventKind`] was shaped for this in stage 1 (ADR-0022) — so this
//! module is a rename, not a state machine. What it does own is *policy*:
//!
//! * which events a session hears, since domains are enabled per session and
//!   two sessions on one target may want different things;
//! * the `Page.lifecycleEvent` names, which are what a driver's `waitUntil`
//!   actually matches on. Puppeteer's `LifecycleWatcher` keys off the strings
//!   `load`, `DOMContentLoaded`, `networkIdle` and `networkAlmostIdle`; get one
//!   wrong and `page.goto` hangs until its own timeout rather than failing.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use oxidepage_engine::PageEvent;
use oxidepage_engine::page_api::{
    ConsoleLevel, ConsoleMessage, DownloadState, NavigationEvent, NavigationEventKind,
    NetworkEvent, ScriptError, ScriptErrorKind, StackFrame, render_preview_top,
};
use serde_json::json;

use crate::message::Event;
use crate::session::{Connection, SessionState};

/// CDP timestamps are seconds, and the engine records epoch milliseconds.
fn seconds(epoch_ms: f64) -> f64 {
    epoch_ms / 1000.0
}

/// Fans one page event out to every session of `target_id` on this connection.
pub fn dispatch_page_event(connection: &Arc<Connection>, target_id: &str, event: &PageEvent) {
    let sessions = connection.sessions_for(target_id);
    if sessions.is_empty() {
        return;
    }
    // The frame id is the target id. There is one frame per page until stage 11
    // (real iframes), and minting a second opaque id for a one-to-one mapping
    // would be two things to keep in sync for no gain.
    let frame_id = target_id;
    // The loader of the load *in flight*, not of the committed document: every
    // event of a navigation belongs to the document it is producing, and
    // `Page.lifecycleEvent { name: "init" }` in particular is the only thing
    // that moves Puppeteer's `frame._loaderId` (ADR-0032 D6a). After the commit
    // the two are the same value.
    let loader_id = connection
        .registry
        .loading_loader_id(target_id)
        .unwrap_or_else(|| String::from("0"));
    let url = connection
        .registry
        .info(target_id)
        .map(|info| info.url)
        .unwrap_or_default();

    for session in &sessions {
        match event {
            PageEvent::Navigation(navigation) => {
                navigation_events(connection, session, frame_id, &loader_id, &url, navigation);
            }
            PageEvent::DialogOpening(request) if session.flags.page.load(Ordering::Relaxed) => {
                {
                    connection.emit(Event::session(
                        &session.id,
                        "Page.javascriptDialogOpening",
                        json!({
                            "url": request.url,
                            "message": request.message,
                            "type": request.kind.as_str(),
                            // Honest, not hardcoded. Under `DialogPolicy::Dismiss`
                            // or `Accept` the page answers itself and
                            // `Page.handleJavaScriptDialog` can only fail, so a
                            // driver that read `true` here would promise its user
                            // a dialog that was already gone.
                            "hasBrowserHandler": session.page.awaits_dialog_answer(),
                            "defaultPrompt": request.default_value.clone(),
                        }),
                    ));
                }
            }
            PageEvent::Dialog(dialog) if session.flags.page.load(Ordering::Relaxed) => {
                connection.emit(Event::session(
                    &session.id,
                    "Page.javascriptDialogClosed",
                    json!({
                        "result": dialog.response.accepted(),
                        "userInput": dialog_input(&dialog.response),
                    }),
                ));
            }
            PageEvent::Console(message) if session.flags.runtime.load(Ordering::Relaxed) => {
                connection.emit(Event::session(
                    &session.id,
                    "Runtime.consoleAPICalled",
                    console_json(session, message),
                ));
            }
            PageEvent::Error(error) => script_error(connection, session, error),
            // A pause belongs to `Fetch`, not `Network`, and is gated on
            // `Fetch.enable` alone: a driver may intercept without ever having
            // sent `Network.enable`, and Puppeteer does exactly that.
            PageEvent::Network(network)
                if session.flags.fetch.load(Ordering::Relaxed)
                    && matches!(
                        network,
                        NetworkEvent::Paused { .. } | NetworkEvent::AuthRequired { .. }
                    ) =>
            {
                fetch_event(connection, session, target_id, network);
            }
            PageEvent::Network(network) if session.flags.network.load(Ordering::Relaxed) => {
                // Built once per session rather than once per target: the
                // envelope carries the `sessionId`, so two sessions watching one
                // page each need their own copy.
                let document_loader = connection
                    .registry
                    .document_loader(target_id, network.request_id());
                if let Some(mut event) = crate::domains::network::network_event(
                    target_id,
                    network,
                    &loader_id,
                    document_loader.as_deref(),
                ) {
                    event.session_id = Some(session.id.clone());
                    connection.emit(event);
                }
            }
            // Only to the sessions that installed *this* name. Broadcasting to
            // every session on the target fires a driver's callback once per
            // attached session for a single page call, and delivers the event to
            // a session that never asked for the binding at all.
            PageEvent::Binding {
                name,
                payload,
                context_id,
            } if session.wants_binding(name) => {
                connection.emit(Event::session(
                    &session.id,
                    "Runtime.bindingCalled",
                    json!({
                        "name": name,
                        "payload": payload,
                        // The world the binding was called from, carried on
                        // the event — not a guess at the main context.
                        "executionContextId": context_id,
                    }),
                ));
            }
            // `Page.fileChooserOpened` (ADR-0032 D12).
            //
            // `backendNodeId` is **not** optional in practice, whatever the
            // protocol says: Puppeteer's `#onFileChooser` immediately calls
            // `adoptBackendNode(event.backendNodeId)`, so an event without one
            // leaves `page.waitForFileChooser()` hanging until its own timeout.
            // The page mints it when the chooser opens, because the handle
            // table lives there.
            PageEvent::FileChooser(chooser) if session.flags.page.load(Ordering::Relaxed) => {
                let mut params = json!({
                    "frameId": frame_id,
                    "mode": if chooser.multiple { "selectMultiple" } else { "selectSingle" },
                });
                if let Some(handle) = chooser.backend_node_id
                    && let Ok(id) = i64::try_from(handle)
                {
                    params["backendNodeId"] = json!(id);
                }
                connection.emit(Event::session(
                    &session.id,
                    "Page.fileChooserOpened",
                    params,
                ));
            }
            // `Page.downloadWillBegin` / `downloadProgress` (ADR-0032 D13).
            // Chrome deprecated these in favour of the `Browser` domain's pair
            // and still emits them; Puppeteer's `page.waitForEvent` listens on
            // the `Page` ones, so those are what ship.
            PageEvent::Download(download) if session.flags.page.load(Ordering::Relaxed) => {
                if download.state == DownloadState::InProgress {
                    connection.emit(Event::session(
                        &session.id,
                        "Page.downloadWillBegin",
                        json!({
                            "frameId": frame_id,
                            "guid": download.guid,
                            "url": download.url,
                            "suggestedFilename": download.suggested_filename,
                        }),
                    ));
                }
                connection.emit(Event::session(
                    &session.id,
                    "Page.downloadProgress",
                    json!({
                        "guid": download.guid,
                        // No resume and no streaming: a download is written
                        // whole or not at all, so the two counts are equal for a
                        // completed one and the total is known up front.
                        "totalBytes": download.total_bytes,
                        "receivedBytes": match download.state {
                            DownloadState::Completed => download.total_bytes,
                            _ => 0,
                        },
                        "state": download.state.as_str(),
                    }),
                ));
            }
            // Not in the command allow-list — there is no `Inspector` domain to
            // enable — but it is how both drivers learn a page died, and
            // silence would leave them waiting on a thread that is gone.
            PageEvent::Crashed { .. } => {
                connection.emit(Event::session(
                    &session.id,
                    "Inspector.targetCrashed",
                    json!({}),
                ));
            }
            // Console and errors belong to `Runtime`/`Log`, not `Page`; the
            // rest have no protocol counterpart.
            _ => {}
        }
    }
}

/// Turns a pause into the `Fetch` event for it.
///
/// The `requestId` is **exactly** what `Network.requestWillBeSent` carried for
/// this request — including the document's substituted loader id (ADR-0032
/// D6a). Puppeteer pairs `requestPaused.networkId` against its stored
/// `requestWillBeSent`, and drops the pairing outright if the two disagree.
fn fetch_event(
    connection: &Arc<Connection>,
    session: &Arc<SessionState>,
    target_id: &str,
    network: &NetworkEvent,
) {
    let document_loader = connection
        .registry
        .document_loader(target_id, network.request_id());
    let request_id = match document_loader {
        Some(loader) => loader,
        None => crate::domains::network::request_id(target_id, network.request_id()),
    };
    // The frame id is the target id, as everywhere else in this module.
    let event = match network {
        NetworkEvent::Paused {
            url,
            method,
            headers,
            resource_type,
            ..
        } => crate::domains::fetch::request_paused(
            &session.id,
            &request_id,
            target_id,
            url,
            method,
            headers,
            *resource_type,
        ),
        NetworkEvent::AuthRequired {
            url,
            challenge,
            resource_type,
            ..
        } => crate::domains::fetch::auth_required(
            &session.id,
            &request_id,
            target_id,
            url,
            challenge,
            *resource_type,
        ),
        _ => return,
    };
    connection.emit(event);
}

/// `Runtime.consoleAPICalled`.
///
/// The arguments travel as bounded [`ValuePreview`]s, not live values — the
/// console payload deliberately holds no `JsValue` (ADR-0025), so the engine
/// never has to keep a page's object graph alive to describe a log line. That
/// means a driver's `msg.args()[0].jsonValue()` sees a *snapshot*, which is the
/// documented trade and the reason `Runtime.evaluate` exists.
fn console_json(session: &Arc<SessionState>, message: &ConsoleMessage) -> serde_json::Value {
    let args: Vec<serde_json::Value> = if message.args.is_empty() {
        // Engine-originated lines carry no arguments, only the rendered text.
        vec![json!({ "type": "string", "value": message.message })]
    } else {
        message
            .args
            .iter()
            .map(|preview| json!({ "type": "string", "value": render_preview_top(preview) }))
            .collect()
    };
    json!({
        "type": console_type(message.level),
        "args": args,
        "executionContextId": session.page.execution_context_id().unwrap_or(1),
        "timestamp": message.timestamp,
        "stackTrace": { "callFrames": message.location.iter().map(frame_json_for_stack).collect::<Vec<_>>() },
    })
}

/// The `Runtime.consoleAPICalled.type` for a console level.
///
/// Only one name differs from the method that produced it, and it is the one a
/// driver branches on: CDP spells `console.warn`'s type `warning`. Puppeteer
/// maps `warning` to its own `warn`, so emitting `warn` here leaves
/// `msg.type()` reporting something no consumer expects.
fn console_type(level: ConsoleLevel) -> &'static str {
    match level {
        ConsoleLevel::Warn => "warning",
        other => other.as_str(),
    }
}

fn frame_json_for_stack(frame: &StackFrame) -> serde_json::Value {
    json!({
        "functionName": frame.function.clone().unwrap_or_default(),
        "scriptId": "0",
        "url": frame.url,
        // CDP counts from zero; the engine's frames count from one, as every
        // stack trace a human reads does.
        "lineNumber": frame.line.saturating_sub(1),
        "columnNumber": frame.column.saturating_sub(1),
    })
}

/// Routes one script error to the domain that owns it.
///
/// `Resource` is not an uncaught exception — it is a stylesheet 404 or an
/// unresolvable module specifier — so it goes to `Log`, where Chrome puts the
/// same messages. Filing it under `Runtime.exceptionThrown` would make that
/// event mean two different things.
fn script_error(connection: &Arc<Connection>, session: &Arc<SessionState>, error: &ScriptError) {
    let text = match &error.name {
        Some(name) if !name.is_empty() => format!("{name}: {}", error.message),
        _ => error.message.clone(),
    };
    if error.kind == ScriptErrorKind::Resource {
        if session.flags.log.load(Ordering::Relaxed) {
            connection.emit(Event::session(
                &session.id,
                "Log.entryAdded",
                json!({
                    "entry": {
                        "source": "network",
                        "level": "error",
                        "text": text,
                        "timestamp": error.timestamp,
                        "url": error.location().map(|f| f.url.clone()).unwrap_or_default(),
                    }
                }),
            ));
        }
        return;
    }
    if !session.flags.runtime.load(Ordering::Relaxed) {
        return;
    }
    let frame = error.location();
    connection.emit(Event::session(
        &session.id,
        "Runtime.exceptionThrown",
        json!({
            "timestamp": error.timestamp,
            "exceptionDetails": {
                "exceptionId": 1,
                "text": text,
                "lineNumber": frame.map_or(0, |f| f.line.saturating_sub(1)),
                "columnNumber": frame.map_or(0, |f| f.column.saturating_sub(1)),
                "url": frame.map_or_else(String::new, |f| f.url.clone()),
                "stackTrace": {
                    "callFrames": error.stack.iter().map(frame_json_for_stack).collect::<Vec<_>>()
                },
            },
        }),
    ));
}

fn dialog_input(response: &oxidepage_engine::page_api::DialogResponse) -> String {
    match response {
        oxidepage_engine::page_api::DialogResponse::AcceptWith(text) => text.clone(),
        _ => String::new(),
    }
}

fn navigation_events(
    connection: &Arc<Connection>,
    session: &Arc<SessionState>,
    frame_id: &str,
    loader_id: &str,
    url: &str,
    navigation: &NavigationEvent,
) {
    let page_on = session.flags.page.load(Ordering::Relaxed);
    // `Page.setLifecycleEventsEnabled` is a separate switch from `Page.enable`:
    // Puppeteer turns it on by itself for `waitUntil: 'networkidle0'`.
    let lifecycle_on = session.flags.lifecycle.load(Ordering::Relaxed);
    if !page_on && !lifecycle_on {
        return;
    }
    let timestamp = seconds(navigation.timestamp);

    let lifecycle = |name: &str| {
        if lifecycle_on {
            connection.emit(Event::session(
                &session.id,
                "Page.lifecycleEvent",
                json!({
                    "frameId": frame_id,
                    "loaderId": loader_id,
                    "name": name,
                    "timestamp": timestamp,
                }),
            ));
        }
    };

    match navigation.kind {
        NavigationEventKind::Started => {
            lifecycle("init");
            if page_on {
                connection.emit(Event::session(
                    &session.id,
                    "Page.frameStartedLoading",
                    json!({ "frameId": frame_id }),
                ));
            }
        }
        NavigationEventKind::Committed => {
            if page_on {
                connection.emit(Event::session(
                    &session.id,
                    "Page.frameNavigated",
                    json!({
                        "frame": frame_json(frame_id, loader_id, &navigation.url),
                        "type": "Navigation",
                    }),
                ));
            }
            // A new document is a new execution context, and saying so is not
            // optional. A driver keys its context map by id and *drops* any
            // event naming an id it does not know — so without this, every
            // `consoleAPICalled` and `bindingCalled` after the first navigation
            // is silently discarded by the driver, not by us.
            if session.flags.runtime.load(Ordering::Relaxed) {
                connection.emit(Event::session(
                    &session.id,
                    "Runtime.executionContextsCleared",
                    json!({}),
                ));
                connection.emit(crate::domains::runtime::execution_context_created(
                    connection, session,
                ));
                // The isolated worlds go back up with their **new** ids: a
                // commit tears each one down and rebuilds it against a fresh
                // global (ADR-0033 D9), so the ids a driver holds are dead. The
                // registry is the page's, not the session's, which is what
                // makes two sessions on one target see the same worlds.
                for world in session.page.worlds().unwrap_or_default() {
                    if world.is_default {
                        continue;
                    }
                    connection.emit(crate::domains::runtime::execution_context_created_named(
                        connection,
                        session,
                        &world.name,
                        false,
                        world.context_id,
                    ));
                }
            }
            // Every node id this connection ever handed out named a node of the
            // outgoing document, and they are all dead now: the page cleared its
            // handle table and the fresh arena is seeded above the old
            // generation high-water mark. `DOM.documentUpdated` is how a driver
            // learns that without probing each id (ADR-0031 D2), and it is the
            // one and only consequence of `DOM.enable`.
            if session.flags.dom.load(Ordering::Relaxed) {
                connection.emit(Event::session(
                    &session.id,
                    "DOM.documentUpdated",
                    json!({}),
                ));
            }
        }
        NavigationEventKind::SameDocument => {
            if page_on {
                connection.emit(Event::session(
                    &session.id,
                    "Page.navigatedWithinDocument",
                    json!({ "frameId": frame_id, "url": navigation.url }),
                ));
            }
        }
        NavigationEventKind::DomContentLoaded => {
            lifecycle("DOMContentLoaded");
            if page_on {
                connection.emit(Event::session(
                    &session.id,
                    "Page.domContentEventFired",
                    json!({ "timestamp": timestamp }),
                ));
            }
        }
        NavigationEventKind::Load => {
            lifecycle("load");
            if page_on {
                connection.emit(Event::session(
                    &session.id,
                    "Page.loadEventFired",
                    json!({ "timestamp": timestamp }),
                ));
            }
        }
        NavigationEventKind::NetworkIdle => {
            // Chrome distinguishes "no requests for 500 ms" from "at most two".
            // The engine records one idle milestone, and a driver waiting for
            // the stricter one must not wait forever for a signal that will
            // never come — so both names are emitted, `networkAlmostIdle`
            // first, which is the order Chrome sends them in.
            lifecycle("networkAlmostIdle");
            lifecycle("networkIdle");
            if page_on {
                connection.emit(Event::session(
                    &session.id,
                    "Page.frameStoppedLoading",
                    json!({ "frameId": frame_id }),
                ));
            }
        }
        NavigationEventKind::Failed => {
            if page_on {
                connection.emit(Event::session(
                    &session.id,
                    "Page.frameStoppedLoading",
                    json!({ "frameId": frame_id }),
                ));
            }
        }
    }
    let _ = url;
}

/// CDP's `Page.Frame`.
pub fn frame_json(frame_id: &str, loader_id: &str, url: &str) -> serde_json::Value {
    json!({
        "id": frame_id,
        "loaderId": loader_id,
        "url": url,
        // There is one browsing context per page until stage 11, so a frame
        // never has a parent and the origin is always the document's own.
        "securityOrigin": security_origin(url),
        "mimeType": "text/html",
    })
}

/// The serialized origin of `url`, or `"://"` for one that has none — which is
/// what Chrome reports for `about:blank`.
pub fn security_origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) if parsed.host_str().is_some() => {
            let scheme = parsed.scheme();
            let host = parsed.host_str().unwrap_or_default();
            match parsed.port() {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            }
        }
        _ => String::from("://"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_seconds() {
        assert!((seconds(1_500.0) - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn security_origin_drops_the_path_and_keeps_a_nondefault_port() {
        assert_eq!(
            security_origin("http://example.com/a/b?c"),
            "http://example.com"
        );
        assert_eq!(
            security_origin("http://127.0.0.1:8080/x"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            security_origin("https://example.com/"),
            "https://example.com"
        );
    }

    #[test]
    fn an_opaque_origin_is_reported_as_such() {
        // Chrome reports `://` for a document with no tuple origin. Reporting
        // the URL instead would let a driver believe `about:blank` is
        // same-origin with something.
        assert_eq!(security_origin("about:blank"), "://");
        assert_eq!(security_origin("data:text/html,hi"), "://");
        assert_eq!(security_origin("not a url"), "://");
    }

    #[test]
    fn a_frame_carries_the_ids_a_driver_matches_on() {
        let frame = frame_json("t1", "l1", "http://example.com/");
        assert_eq!(frame["id"], "t1");
        assert_eq!(frame["loaderId"], "l1");
        assert_eq!(frame["url"], "http://example.com/");
        assert_eq!(frame["securityOrigin"], "http://example.com");
    }
}
