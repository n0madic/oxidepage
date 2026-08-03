//! The `Fetch` domain: request interception (ADR-0032).
//!
//! # The `interceptionId` is the `networkId`
//!
//! Chrome mints a separate `interceptionId` and pairs it back to a request
//! through `requestPaused.networkId`. This endpoint uses **one** id for both,
//! which is legal (the field is opaque) and removes a whole table: the
//! `interceptionId` a driver quotes back is the same `"{targetId}.{index}v{gen}"`
//! string `Network.requestWillBeSent` carried, so `parse_request_id` is the only
//! decoder and a pairing bug is a parse failure rather than a silent mismatch.
//!
//! # Idempotence lives here, not on the page
//!
//! Every resolution claims the id out of the shared paused set *before* it
//! sends (`InterceptControl::resolve`). A second `continueRequest` — a driver's
//! retry, or the loser when two sessions both intercept — answers `Invalid
//! InterceptionId`, which is what Chrome answers and what Puppeteer's
//! `_continue` already catches.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use oxidepage_engine::page_api::{
    AuthResponse, FulfilledResponse, InterceptCommand, InterceptControl, RequestOverrides,
    RequestPattern, ResourceType,
};
use serde::Deserialize;
use serde_json::json;

use crate::base64;
use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::{Connection, SessionState};

/// Most bytes a `fulfillRequest` body may carry.
///
/// Decoded on the shared priority lane (ADR-0032 D4), so an unbounded body
/// would let one session's stub stall every other target's urgent command while
/// it base64-decodes. 32 MiB matches the per-page retained-body budget.
const MAX_FULFILL_BODY: usize = 32 * 1024 * 1024;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Fetch.enable" => enable(connection, request),
        "Fetch.disable" => disable(connection, request),
        "Fetch.continueRequest" => continue_request(connection, request),
        "Fetch.fulfillRequest" => fulfill_request(connection, request),
        "Fetch.failRequest" => fail_request(connection, request),
        "Fetch.continueWithAuth" => continue_with_auth(connection, request),
        // Refused, not downgraded: there is no response-stage pause for these
        // to read from, and a driver told otherwise would wait for a
        // `requestPaused` carrying `responseStatusCode` that never comes.
        "Fetch.continueResponse" | "Fetch.getResponseBody" | "Fetch.takeResponseBodyAsStream" => {
            Err(ProtocolError::server(format!(
                "{} is not implemented: this endpoint pauses requests only, never responses \
                 (ADR-0032). Use Fetch.fulfillRequest to substitute a response.",
                request.method
            )))
        }
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

// === enable / disable ===

fn enable(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(default)]
        patterns: Vec<PatternParam>,
        #[serde(default)]
        handle_auth_requests: bool,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PatternParam {
        url_pattern: Option<String>,
        resource_type: Option<String>,
        request_stage: Option<String>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse_or_default()?;

    let mut patterns = Vec::with_capacity(params.patterns.len());
    for pattern in params.patterns {
        // Refused rather than silently served at the Request stage: a driver
        // that asked to see responses and got requests would rewrite the wrong
        // half of every exchange.
        if let Some(stage) = &pattern.request_stage
            && !stage.eq_ignore_ascii_case("Request")
        {
            return Err(ProtocolError::server(format!(
                "Fetch.enable: requestStage `{stage}` is not supported — this endpoint pauses \
                 requests only, never responses (ADR-0032)"
            )));
        }
        let resource_type = match &pattern.resource_type {
            Some(name) => Some(ResourceType::parse(name).ok_or_else(|| {
                ProtocolError::invalid_params(format!(
                    "Fetch.enable: unknown resourceType `{name}`"
                ))
            })?),
            None => None,
        };
        patterns.push(RequestPattern {
            url_pattern: pattern.url_pattern.unwrap_or_else(|| String::from("*")),
            resource_type,
        });
    }

    // A page with no event sink announces nothing, so a pause could never be
    // reported and every matching request would hold for the whole intercept
    // timeout. Refusing is the only honest answer (ADR-0032, deliberate limits).
    //
    // Asked, not assumed: a page created through `engine` always has a sink, so
    // this is a stated precondition rather than a live failure mode — but
    // `Page::set_event_sink` is public, and "it cannot happen here" is the kind
    // of claim that stops being true one refactor later.
    if session.page.is_closed() || !session.page.has_event_sink()? {
        return Err(ProtocolError::server(
            "Fetch.enable: this target announces no events, so a paused request could never be \
             reported and every request would stall until the intercept timeout",
        ));
    }

    // The flag **first**, then the shared config. The page thread runs
    // concurrently, and `pump::dispatch_page_event` gates `Fetch.requestPaused`
    // on this flag — so a request that paused between the two writes would be
    // announced to a session that drops it, and then hold for the full intercept
    // timeout with nobody able to release it. That is the exact wedge D2 exists
    // to prevent, reached through a two-line window.
    session.flags.fetch.store(true, Ordering::Relaxed);
    intercept(&session)?.enable(patterns, params.handle_auth_requests);
    Ok(json!({}))
}

fn disable(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    session.flags.fetch.store(false, Ordering::Relaxed);
    // Puppeteer's `NetworkManager` sends this unconditionally when a page is
    // created, before anything was ever enabled, so a missing control is not an
    // error — there is simply nothing to release.
    if let Some(control) = session.page.intercept() {
        for id in control.disable() {
            control.send(InterceptCommand::release(id));
        }
    }
    Ok(json!({}))
}

// === resolution ===

fn continue_request(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        request_id: String,
        url: Option<String>,
        method: Option<String>,
        post_data: Option<String>,
        headers: Option<Vec<HeaderEntry>>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = crate::domains::network::parse_request_id(
        connection,
        &session.target_id,
        &params.request_id,
    )?;

    // Validated *here*, at the pause boundary, not left to fail as a confusing
    // network error minutes later (ADR-0032 D5). The fetch pipeline still
    // re-checks: the override re-enters `fetch_inner` from the top, so the
    // scheme gate, the per-hop re-check and the connector's address filter all
    // apply unchanged — interception is not an SSRF bypass.
    if let Some(url) = &params.url {
        let parsed = url::Url::parse(url).map_err(|e| {
            ProtocolError::invalid_params(format!(
                "Fetch.continueRequest: invalid url `{url}`: {e}"
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ProtocolError::invalid_params(format!(
                "Fetch.continueRequest: url must be http(s), got `{}`",
                parsed.scheme()
            )));
        }
    }

    let post_data = match &params.post_data {
        Some(encoded) => Some(decode_body(encoded)?),
        None => None,
    };
    let overrides = RequestOverrides {
        url: params.url,
        method: params.method,
        post_data,
        headers: params.headers.map(headers_to_pairs),
    };
    send(
        &session,
        InterceptCommand::Continue {
            id,
            overrides: Box::new(overrides),
        },
        &params.request_id,
    )
}

fn fulfill_request(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        request_id: String,
        response_code: u16,
        #[serde(default)]
        response_headers: Option<Vec<HeaderEntry>>,
        response_phrase: Option<String>,
        body: Option<String>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = crate::domains::network::parse_request_id(
        connection,
        &session.target_id,
        &params.request_id,
    )?;

    let body = match &params.body {
        Some(encoded) => decode_body(encoded)?,
        None => Vec::new(),
    };
    let response = FulfilledResponse {
        status: params.response_code,
        status_text: params
            .response_phrase
            .unwrap_or_else(|| status_phrase(params.response_code).to_owned()),
        headers: params
            .response_headers
            .map(headers_to_pairs)
            .unwrap_or_default(),
        body,
    };
    send(
        &session,
        InterceptCommand::Fulfill {
            id,
            response: Box::new(response),
        },
        &params.request_id,
    )
}

fn fail_request(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        request_id: String,
        error_reason: String,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = crate::domains::network::parse_request_id(
        connection,
        &session.target_id,
        &params.request_id,
    )?;
    send(
        &session,
        InterceptCommand::Fail {
            id,
            error: error_text(&params.error_reason),
        },
        &params.request_id,
    )
}

fn continue_with_auth(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        request_id: String,
        auth_challenge_response: ChallengeResponse,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ChallengeResponse {
        response: String,
        username: Option<String>,
        password: Option<String>,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = crate::domains::network::parse_request_id(
        connection,
        &session.target_id,
        &params.request_id,
    )?;

    let answer = params.auth_challenge_response;
    let response = match answer.response.as_str() {
        "ProvideCredentials" => AuthResponse::Provide {
            username: answer.username.unwrap_or_default(),
            password: answer.password.unwrap_or_default(),
        },
        "Default" => AuthResponse::Default,
        "CancelAuth" => AuthResponse::Cancel,
        other => {
            return Err(ProtocolError::invalid_params(format!(
                "Fetch.continueWithAuth: unknown response `{other}`"
            )));
        }
    };
    send(
        &session,
        InterceptCommand::Auth { id, response },
        &params.request_id,
    )
}

// === events ===

/// `Fetch.requestPaused`.
///
/// `networkId` is **exactly** the `requestId` of the `Network.requestWillBeSent`
/// that preceded it, and the request is reported **as announced** — same url,
/// same method, same headers. A driver pairs the two and drops the pairing if
/// they disagree, which loses the request entirely.
pub fn request_paused(
    session_id: &str,
    request_id: &str,
    frame_id: &str,
    url: &str,
    method: &str,
    headers: &[(String, String)],
    resource_type: ResourceType,
) -> crate::message::Event {
    crate::message::Event::session(
        session_id,
        "Fetch.requestPaused",
        json!({
            "requestId": request_id,
            "networkId": request_id,
            "frameId": frame_id,
            "resourceType": resource_type.as_str(),
            "request": {
                "url": url,
                "method": method,
                "headers": crate::domains::network::header_map(headers),
                "initialPriority": "Medium",
                "referrerPolicy": "strict-origin-when-cross-origin",
            },
        }),
    )
}

/// `Fetch.authRequired`.
pub fn auth_required(
    session_id: &str,
    request_id: &str,
    frame_id: &str,
    url: &str,
    challenge: &oxidepage_engine::page_api::AuthChallenge,
    resource_type: ResourceType,
) -> crate::message::Event {
    let source = match challenge.source {
        oxidepage_engine::page_api::AuthSource::Server => "Server",
        oxidepage_engine::page_api::AuthSource::Proxy => "Proxy",
    };
    crate::message::Event::session(
        session_id,
        "Fetch.authRequired",
        json!({
            "requestId": request_id,
            "frameId": frame_id,
            "resourceType": resource_type.as_str(),
            "request": { "url": url, "method": "GET" },
            "authChallenge": {
                "source": source,
                "origin": challenge.origin,
                "scheme": challenge.scheme,
                "realm": challenge.realm,
            },
        }),
    )
}

// === helpers ===

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeaderEntry {
    name: String,
    value: String,
}

fn headers_to_pairs(headers: Vec<HeaderEntry>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|entry| (entry.name, entry.value))
        .collect()
}

fn intercept(session: &Arc<SessionState>) -> Result<InterceptControl, ProtocolError> {
    session.page.intercept().ok_or_else(|| {
        ProtocolError::server("the target has no interception control (its page thread is gone)")
    })
}

/// Claims the pause and queues the decision, or reports the id as spent.
fn send(session: &Arc<SessionState>, command: InterceptCommand, request_id: &str) -> CommandResult {
    if intercept(session)?.resolve(command) {
        return Ok(json!({}));
    }
    Err(ProtocolError::server(format!(
        "Invalid InterceptionId: {request_id}"
    )))
}

fn decode_body(encoded: &str) -> Result<Vec<u8>, ProtocolError> {
    // Checked before decoding, on the encoded length: 4 base64 characters carry
    // 3 bytes, so this bounds the allocation rather than discovering it after.
    if encoded.len() / 4 * 3 > MAX_FULFILL_BODY {
        return Err(ProtocolError::invalid_params(format!(
            "body exceeds the {MAX_FULFILL_BODY}-byte limit"
        )));
    }
    base64::decode(encoded).ok_or_else(|| ProtocolError::invalid_params("body is not valid base64"))
}

/// The `net::ERR_…` text for a CDP `ErrorReason`.
///
/// Puppeteer's `request.abort(errorCode)` maps its own names to these, and a
/// driver reads the result back off `Network.loadingFailed.errorText` — so the
/// names have to be Chrome's, not ours.
fn error_text(reason: &str) -> String {
    let text = match reason {
        "Failed" => "net::ERR_FAILED",
        "Aborted" => "net::ERR_ABORTED",
        "TimedOut" => "net::ERR_TIMED_OUT",
        "AccessDenied" => "net::ERR_ACCESS_DENIED",
        "ConnectionClosed" => "net::ERR_CONNECTION_CLOSED",
        "ConnectionReset" => "net::ERR_CONNECTION_RESET",
        "ConnectionRefused" => "net::ERR_CONNECTION_REFUSED",
        "ConnectionAborted" => "net::ERR_CONNECTION_ABORTED",
        "ConnectionFailed" => "net::ERR_CONNECTION_FAILED",
        "NameNotResolved" => "net::ERR_NAME_NOT_RESOLVED",
        "InternetDisconnected" => "net::ERR_INTERNET_DISCONNECTED",
        "AddressUnreachable" => "net::ERR_ADDRESS_UNREACHABLE",
        "BlockedByClient" => "net::ERR_BLOCKED_BY_CLIENT",
        "BlockedByResponse" => "net::ERR_BLOCKED_BY_RESPONSE",
        // An unknown reason is not worth refusing the abort over: the driver
        // asked for the request to fail, and it will.
        _ => "net::ERR_FAILED",
    };
    text.to_owned()
}

/// The reason phrase for a status a driver fabricated but did not name.
fn status_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_abort_reason_becomes_chromes_own_error_text() {
        // A driver reads this back off `loadingFailed.errorText` and compares
        // it to Chrome's spelling; ours would never match.
        assert_eq!(error_text("Aborted"), "net::ERR_ABORTED");
        assert_eq!(error_text("BlockedByClient"), "net::ERR_BLOCKED_BY_CLIENT");
        assert_eq!(error_text("nonsense"), "net::ERR_FAILED");
    }

    #[test]
    fn an_oversized_body_is_refused_before_it_is_decoded() {
        let encoded = "A".repeat(MAX_FULFILL_BODY / 3 * 4 + 8);
        assert!(decode_body(&encoded).is_err());
    }
}
