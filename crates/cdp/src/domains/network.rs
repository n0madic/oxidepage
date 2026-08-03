//! The `Network` domain: request milestones, response bodies, cookies.
//!
//! # `requestId` is not the engine's `RequestId`
//!
//! The engine mints `RequestId` from a **per-page** counter, so page A's
//! request 3 and page B's request 3 are different requests with the same
//! number. A driver holding one socket sees both. So the protocol id is
//! `"{targetId}.{index}v{generation}"` — scoped by the target, and carrying the
//! generation the arena uses to make a recycled slot detectable.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use oxidepage_engine::page_api::{CookieView, NetworkEvent, RequestId};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::base64;
use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Network.enable" => set_enabled(connection, request, true),
        "Network.disable" => set_enabled(connection, request, false),
        "Network.setExtraHTTPHeaders" => set_extra_headers(connection, request),
        "Network.setUserAgentOverride" => set_user_agent(connection, request),
        "Network.setCacheDisabled" => set_cache_disabled(connection, request),
        "Network.getResponseBody" => get_response_body(connection, request),
        "Network.getAllCookies" => get_all_cookies(connection, request),
        "Network.getCookies" => get_cookies(connection, request),
        "Network.setCookie" => set_cookie(connection, request),
        "Network.setCookies" => set_cookies(connection, request),
        "Network.deleteCookies" => delete_cookies(connection, request),
        "Network.clearBrowserCookies" => clear_cookies(connection, request),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

fn set_enabled(connection: &Arc<Connection>, request: &Request, enabled: bool) -> CommandResult {
    let session = connection.require_session(request)?;
    session.flags.network.store(enabled, Ordering::Relaxed);
    Ok(json!({}))
}

// === request ids ===

/// The protocol id for one of a target's requests.
pub fn request_id(target_id: &str, id: RequestId) -> String {
    format!("{target_id}.{}v{}", id.index(), id.generation())
}

/// Parses back what [`request_id`] produced, checking it names *this* target.
fn parse_request_id(target_id: &str, encoded: &str) -> Result<RequestId, ProtocolError> {
    let unknown = || {
        ProtocolError::server(format!(
            "No resource with given identifier found: {encoded}"
        ))
    };
    let rest = encoded
        .strip_prefix(target_id)
        .and_then(|rest| rest.strip_prefix('.'))
        .ok_or_else(unknown)?;
    let (index, generation) = rest.split_once('v').ok_or_else(unknown)?;
    let index: u32 = index.parse().map_err(|_| unknown())?;
    let generation: u32 = generation.parse().map_err(|_| unknown())?;
    let generation = std::num::NonZeroU32::new(generation).ok_or_else(unknown)?;
    Ok(RequestId::from_parts(index, generation))
}

// === events ===

/// Turns one network milestone into the protocol event for it.
pub fn network_event(target_id: &str, event: &NetworkEvent) -> crate::message::Event {
    let id = request_id(target_id, event.request_id());
    match event {
        NetworkEvent::Requested {
            url,
            method,
            headers,
            timestamp,
            ..
        } => crate::message::Event::browser(
            "Network.requestWillBeSent",
            json!({
                "requestId": id,
                "loaderId": "",
                "documentURL": url,
                "request": {
                    "url": url,
                    "method": method,
                    "headers": header_map(headers),
                    "initialPriority": "Medium",
                    "referrerPolicy": "strict-origin-when-cross-origin",
                },
                "timestamp": seconds(*timestamp),
                "wallTime": *timestamp / 1000.0,
                "initiator": { "type": "other" },
                "frameId": target_id,
            }),
        ),
        NetworkEvent::Responded {
            status,
            status_text,
            headers,
            final_url,
            mime_type,
            timestamp,
            ..
        } => crate::message::Event::browser(
            "Network.responseReceived",
            json!({
                "requestId": id,
                "loaderId": "",
                "timestamp": seconds(*timestamp),
                "type": "Other",
                "response": {
                    "url": final_url,
                    "status": status,
                    "statusText": status_text,
                    "headers": header_map(headers),
                    "mimeType": mime_type,
                    // No timing breakdown is recorded: the stack measures no
                    // DNS/connect/TLS phases, and inventing zeros would look
                    // like a page that loaded instantly.
                    "connectionReused": false,
                    "connectionId": 0,
                    "fromDiskCache": false,
                    "fromServiceWorker": false,
                    "encodedDataLength": -1,
                    "securityState": if final_url.starts_with("https:") { "secure" } else { "insecure" },
                },
                "frameId": target_id,
            }),
        ),
        NetworkEvent::Finished {
            encoded_len,
            timestamp,
            ..
        } => crate::message::Event::browser(
            "Network.loadingFinished",
            json!({
                "requestId": id,
                "timestamp": seconds(*timestamp),
                "encodedDataLength": encoded_len,
            }),
        ),
        NetworkEvent::Failed {
            error, timestamp, ..
        } => crate::message::Event::browser(
            "Network.loadingFailed",
            json!({
                "requestId": id,
                "timestamp": seconds(*timestamp),
                "type": "Other",
                "errorText": error,
                "canceled": false,
            }),
        ),
    }
}

fn seconds(epoch_ms: f64) -> f64 {
    epoch_ms / 1000.0
}

/// CDP models headers as an object, not a list. Duplicates are joined with a
/// newline, which is Chrome's own encoding for repeated headers.
fn header_map(headers: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        match map.get_mut(name) {
            Some(Value::String(existing)) => {
                existing.push('\n');
                existing.push_str(value);
            }
            _ => {
                map.insert(name.clone(), json!(value));
            }
        }
    }
    Value::Object(map)
}

// === commands ===

fn set_extra_headers(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        headers: serde_json::Map<String, Value>,
    }
    connection.require_session(request)?;
    let params: Params = request.parse()?;
    // Refused rather than accepted-and-ignored: a test that sets an auth header
    // and gets a 401 would otherwise blame the server. There is no per-page
    // header override in the net stack (`RequestDefaults` is built once, at
    // page construction), so honoring this needs engine work of its own.
    if params.headers.is_empty() {
        return Ok(json!({}));
    }
    Err(ProtocolError::server(
        "Network.setExtraHTTPHeaders is not implemented: headers cannot be overridden per page",
    ))
}

fn set_user_agent(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    connection.require_session(request)?;
    // The same command exists in `Emulation`, which is where it is implemented.
    crate::domains::emulation::set_user_agent_override(connection, request)
}

fn set_cache_disabled(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        cache_disabled: bool,
    }
    connection.require_session(request)?;
    let params: Params = request.parse()?;
    // Accepting `false` is honest — that is the state anyway. Accepting `true`
    // would not be: the HTTP cache is shared browser-wide and has no per-page
    // bypass short of `Page.reload`, which already bypasses it.
    if params.cache_disabled {
        return Err(ProtocolError::server(
            "Network.setCacheDisabled(true) is not implemented: the HTTP cache is browser-wide; \
             use Page.reload, which always bypasses it",
        ));
    }
    Ok(json!({}))
}

fn get_response_body(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        request_id: String,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let id = parse_request_id(&session.target_id, &params.request_id)?;

    let Some((bytes, text)) = session.page.response_body(id)? else {
        return Err(ProtocolError::server(
            "No resource with given identifier found",
        ));
    };
    if text && let Ok(body) = String::from_utf8(bytes.clone()) {
        return Ok(json!({ "body": body, "base64Encoded": false }));
    }
    // Bytes, or text that turned out not to be valid UTF-8 after all: base64 is
    // always correct, where a lossy conversion silently corrupts.
    Ok(json!({ "body": base64::encode(&bytes), "base64Encoded": true }))
}

// === cookies ===

fn cookie_json(cookie: &CookieView) -> Value {
    json!({
        "name": cookie.name,
        "value": cookie.value,
        "domain": cookie.domain,
        "path": cookie.path,
        // CDP counts seconds since the epoch, and -1 means "session cookie".
        "expires": cookie.expires.map_or(-1.0, |at| {
            at.duration_since(std::time::UNIX_EPOCH)
                .map_or(-1.0, |since| since.as_secs_f64())
        }),
        "size": cookie.name.len() + cookie.value.len(),
        "httpOnly": cookie.http_only,
        "secure": cookie.secure,
        "session": cookie.expires.is_none(),
        "sameSite": cookie.same_site,
    })
}

fn get_all_cookies(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let context = connection
        .registry
        .context_of(&session.target_id)
        .ok_or_else(|| ProtocolError::no_target(&session.target_id))?;
    let jar = context.cookies();
    let cookies = jar
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot(std::time::SystemTime::now());
    Ok(json!({ "cookies": cookies.iter().map(cookie_json).collect::<Vec<_>>() }))
}

fn get_cookies(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(default)]
        urls: Vec<String>,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let context = connection
        .registry
        .context_of(&session.target_id)
        .ok_or_else(|| ProtocolError::no_target(&session.target_id))?;

    // No `urls` means "the ones for the page's own document", which is what
    // `page.cookies()` with no arguments asks for.
    let urls = if params.urls.is_empty() {
        vec![session.page.url()?]
    } else {
        params.urls
    };

    let jar = context.cookies();
    let jar = jar.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::SystemTime::now();
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<(String, String, String)> = Vec::new();
    for url in &urls {
        let Ok(parsed) = url::Url::parse(url) else {
            continue;
        };
        for cookie in jar.snapshot_for(&parsed, now) {
            // Two URLs of the same site match the same cookie; reporting it
            // twice would make a driver's `cookies.length` wrong.
            let key = (
                cookie.name.clone(),
                cookie.domain.clone(),
                cookie.path.clone(),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(cookie_json(&cookie));
        }
    }
    Ok(json!({ "cookies": out }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CookieParam {
    name: String,
    value: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    secure: Option<bool>,
    #[serde(default)]
    http_only: Option<bool>,
    #[serde(default)]
    same_site: Option<String>,
    /// Seconds since the epoch.
    #[serde(default)]
    expires: Option<f64>,
}

impl CookieParam {
    /// The `Set-Cookie` header this parameter describes, plus the URL to store
    /// it against.
    fn to_set_cookie(&self) -> Result<(String, String), ProtocolError> {
        let url = match (&self.url, &self.domain) {
            (Some(url), _) => url.clone(),
            // A `domain` with no `url` still needs one to store against; a
            // secure cookie must be stored over https or the jar refuses it.
            (None, Some(domain)) => {
                let host = domain.trim_start_matches('.');
                let scheme = if self.secure.unwrap_or(false) {
                    "https"
                } else {
                    "http"
                };
                format!("{scheme}://{host}/")
            }
            (None, None) => {
                return Err(ProtocolError::invalid_params(
                    "At least one of the url or domain needs to be specified",
                ));
            }
        };

        let mut header = format!("{}={}", self.name, self.value);
        if let Some(domain) = &self.domain {
            header.push_str(&format!("; Domain={domain}"));
        }
        if let Some(path) = &self.path {
            header.push_str(&format!("; Path={path}"));
        }
        if self.secure.unwrap_or(false) {
            header.push_str("; Secure");
        }
        if self.http_only.unwrap_or(false) {
            header.push_str("; HttpOnly");
        }
        if let Some(same_site) = &self.same_site {
            header.push_str(&format!("; SameSite={same_site}"));
        }
        if let Some(expires) = self.expires
            && expires >= 0.0
        {
            header.push_str(&format!("; Max-Age={}", max_age_from(expires)));
        }
        Ok((url, header))
    }
}

/// `Max-Age` seconds from an absolute epoch-seconds expiry.
///
/// Max-Age rather than a formatted `Expires` date: the jar parses both, and a
/// relative number cannot be got wrong by a date-format mismatch.
fn max_age_from(expires_epoch_seconds: f64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64());
    (expires_epoch_seconds - now).max(0.0) as i64
}

fn store_cookies(
    connection: &Arc<Connection>,
    request: &Request,
    cookies: &[CookieParam],
) -> Result<usize, ProtocolError> {
    let session = connection.require_session(request)?;
    let context = connection
        .registry
        .context_of(&session.target_id)
        .ok_or_else(|| ProtocolError::no_target(&session.target_id))?;
    let jar = context.cookies();
    let mut jar = jar.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::SystemTime::now();

    let mut stored = 0;
    for cookie in cookies {
        let (url, header) = cookie.to_set_cookie()?;
        let parsed = url::Url::parse(&url)
            .map_err(|_| ProtocolError::invalid_params(format!("Invalid cookie url: {url}")))?;
        // `CookieSource::Http`, not `Script`: a driver setting a cookie is the
        // operator, and refusing it `HttpOnly` would make `setCookie` unable to
        // reproduce a session a real server had handed out.
        if jar.set_cookie(
            &parsed,
            &header,
            oxidepage_engine::page_api::CookieSource::Http,
            now,
        ) {
            stored += 1;
        }
    }
    Ok(stored)
}

fn set_cookie(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let cookie: CookieParam = request.parse()?;
    let stored = store_cookies(connection, request, std::slice::from_ref(&cookie))?;
    Ok(json!({ "success": stored == 1 }))
}

fn set_cookies(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        cookies: Vec<CookieParam>,
    }
    let params: Params = request.parse()?;
    store_cookies(connection, request, &params.cookies)?;
    Ok(json!({}))
}

fn delete_cookies(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        name: String,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        domain: Option<String>,
        #[serde(default)]
        path: Option<String>,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let context = connection
        .registry
        .context_of(&session.target_id)
        .ok_or_else(|| ProtocolError::no_target(&session.target_id))?;

    // A `url` names a host; the jar matches on the canonical domain.
    let domain = params.domain.clone().or_else(|| {
        params
            .url
            .as_deref()
            .and_then(|url| url::Url::parse(url).ok())
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
    });

    let jar = context.cookies();
    jar.lock().unwrap_or_else(|e| e.into_inner()).remove(
        &params.name,
        domain.as_deref(),
        params.path.as_deref(),
    );
    Ok(json!({}))
}

fn clear_cookies(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let context = connection
        .registry
        .context_of(&session.target_id)
        .ok_or_else(|| ProtocolError::no_target(&session.target_id))?;
    let jar = context.cookies();
    jar.lock().unwrap_or_else(|e| e.into_inner()).clear();
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(index: u32, generation: u32) -> RequestId {
        RequestId::from_parts(index, std::num::NonZeroU32::new(generation).unwrap())
    }

    #[test]
    fn a_request_id_is_scoped_to_its_target() {
        // The engine's counter is per page, so two targets both have a request
        // 3. A driver on one socket sees both, and a bare number would collide.
        let a = request_id("targetA", id(3, 1));
        let b = request_id("targetB", id(3, 1));
        assert_ne!(a, b);
        assert_eq!(a, "targetA.3v1");
    }

    #[test]
    fn a_request_id_round_trips() {
        let original = id(42, 7);
        let encoded = request_id("t1", original);
        assert_eq!(parse_request_id("t1", &encoded).unwrap(), original);
    }

    #[test]
    fn a_request_id_from_another_target_is_refused() {
        let encoded = request_id("t1", id(3, 1));
        // Silently reading t2's request 3 instead would hand a driver another
        // page's response body.
        assert!(parse_request_id("t2", &encoded).is_err());
    }

    #[test]
    fn a_malformed_request_id_is_refused() {
        for bad in ["", "t1", "t1.", "t1.x v1", "t1.3", "t1.3v", "t1.3v0"] {
            assert!(parse_request_id("t1", bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn repeated_headers_are_joined_the_way_chrome_joins_them() {
        let headers = vec![
            (String::from("set-cookie"), String::from("a=1")),
            (String::from("set-cookie"), String::from("b=2")),
            (String::from("content-type"), String::from("text/html")),
        ];
        let map = header_map(&headers);
        assert_eq!(map["set-cookie"], "a=1\nb=2");
        assert_eq!(map["content-type"], "text/html");
    }

    #[test]
    fn a_cookie_param_becomes_a_set_cookie_header() {
        let cookie = CookieParam {
            name: String::from("sid"),
            value: String::from("abc"),
            url: Some(String::from("https://example.com/app")),
            domain: None,
            path: Some(String::from("/app")),
            secure: Some(true),
            http_only: Some(true),
            same_site: Some(String::from("Lax")),
            expires: None,
        };
        let (url, header) = cookie.to_set_cookie().unwrap();
        assert_eq!(url, "https://example.com/app");
        assert_eq!(header, "sid=abc; Path=/app; Secure; HttpOnly; SameSite=Lax");
    }

    #[test]
    fn a_cookie_with_only_a_domain_gets_a_url_that_the_jar_will_accept() {
        // A `Secure` cookie stored over `http://` is refused by the jar, so the
        // synthesized URL has to match the cookie's own requirements.
        let cookie = CookieParam {
            name: String::from("s"),
            value: String::from("1"),
            url: None,
            domain: Some(String::from(".example.com")),
            path: None,
            secure: Some(true),
            http_only: None,
            same_site: None,
            expires: None,
        };
        let (url, header) = cookie.to_set_cookie().unwrap();
        assert_eq!(url, "https://example.com/");
        assert!(header.contains("Domain=.example.com"));
        assert!(header.contains("Secure"));
    }

    #[test]
    fn a_cookie_with_neither_url_nor_domain_is_refused() {
        let cookie = CookieParam {
            name: String::from("s"),
            value: String::from("1"),
            url: None,
            domain: None,
            path: None,
            secure: None,
            http_only: None,
            same_site: None,
            expires: None,
        };
        assert!(cookie.to_set_cookie().is_err());
    }
}
