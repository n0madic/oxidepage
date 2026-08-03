//! The discovery endpoint: `/json/version`, `/json/list`, `/json/new`.
//!
//! Chrome exposes these over plain HTTP so a driver can find the WebSocket URL
//! before it can speak the protocol. Three GETs do not justify turning on
//! hyper's `server` feature — that is a `[workspace.dependencies]` change which
//! would pull server code into *every* binary in the workspace, including the
//! CLI's render path (ADR-0030). `xtask/src/testserver.rs` already demonstrates
//! the hand-rolled shape this follows: HTTP/1.1, `Connection: close`, one
//! request per socket.
//!
//! ## Routing: read the head, then replay it
//!
//! A WebSocket upgrade and a `/json/*` GET arrive as the same kind of request
//! head, but tokio-tungstenite's `accept_async` performs the handshake itself
//! and so wants a stream positioned at the *start* of the request.
//!
//! An earlier version tried to `peek` the request line without consuming it.
//! That is wrong twice over: `peek` returns as soon as the socket is readable
//! with whatever the kernel happens to hold, so a request line split across two
//! TCP segments — which the peer is entitled to do — never completes. Retrying
//! cannot help either, because nothing in a peek loop *waits* for the rest; it
//! either spins at 100% CPU or grows its window until it gives up and drops a
//! perfectly good connection.
//!
//! So the head is **read** (consumed) exactly once, and an upgrade gets a
//! [`PrefixedStream`] that replays those bytes before yielding the live socket.
//! Reading is the operation that actually blocks until more data arrives.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::target::TargetRegistry;

/// Largest request head accepted, matching `xtask/src/testserver.rs`. A client
/// that never terminates its headers must not grow a buffer without bound.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// One parsed HTTP request head, plus the bytes it was parsed from.
pub struct RequestHead {
    pub method: String,
    pub path: String,
    /// The `Host` header, needed to reject DNS-rebinding attempts.
    pub host: Option<String>,
    /// The `Origin` header. Present **only** when a web page made the request:
    /// no real driver sends one, and browsers always do.
    pub origin: Option<String>,
    /// Whether this is a WebSocket upgrade.
    pub upgrade: bool,
    /// Everything consumed, so an upgrade can replay it into the handshake.
    raw: Vec<u8>,
}

/// How long a peer gets to send a complete request head.
///
/// Bounding the bytes is not enough: a socket that connects and says nothing
/// holds a task and a descriptor forever, and enough of them exhaust the
/// process without ever exceeding [`MAX_HEAD_BYTES`].
pub const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads and parses one request head.
///
/// Returns `None` if the peer went away, went quiet for [`HEAD_TIMEOUT`], sent
/// no recognizable request line, or exceeded [`MAX_HEAD_BYTES`] without
/// terminating its headers.
pub async fn read_head(stream: &mut TcpStream) -> Option<RequestHead> {
    tokio::time::timeout(HEAD_TIMEOUT, read_head_inner(stream))
        .await
        .ok()?
}

async fn read_head_inner(stream: &mut TcpStream) -> Option<RequestHead> {
    let mut raw = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    loop {
        // `read` — not `peek` — is what waits for more bytes to arrive.
        let read = stream.read(&mut tmp).await.ok()?;
        if read == 0 {
            return None;
        }
        // Only the tail can complete a `\r\n\r\n` the last read split, so
        // rescan from three bytes back rather than over the whole buffer.
        let scan_from = raw.len().saturating_sub(3);
        raw.extend_from_slice(&tmp[..read]);
        if raw[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if raw.len() > MAX_HEAD_BYTES {
            return None;
        }
    }

    // **Only the head.** Everything up to the blank line — anything after it is
    // the request *body*, and parsing that as headers is a security hole, not
    // an untidiness: a page can `fetch(url, {method:'POST', body:'\r\nHost: …'})`
    // as a CORS simple request, and a last-wins `Host` read from the body then
    // defeats the loopback check that exists to stop DNS rebinding.
    let head_len = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw.len(), |end| end + 2);
    let text = String::from_utf8_lossy(&raw[..head_len]);

    let mut lines = text.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();

    let mut host = None;
    let mut origin = None;
    let mut upgrade = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Header names are case-insensitive, and clients differ.
        match name.trim().to_ascii_lowercase().as_str() {
            // A duplicate is a request-smuggling shape, not a client quirk:
            // HTTP/1.1 allows exactly one `Host`, so two means someone is
            // trying to make two parsers disagree. Refuse rather than pick.
            "host" if host.is_some() => return None,
            "host" => host = Some(value.to_owned()),
            "origin" if origin.is_some() => return None,
            "origin" => origin = Some(value.to_owned()),
            // Likewise last-wins here would let a second `Upgrade: nonsense`
            // hide a real handshake from the router.
            "upgrade" if upgrade => return None,
            "upgrade" => upgrade = value.eq_ignore_ascii_case("websocket"),
            _ => {}
        }
    }

    Some(RequestHead {
        method,
        path,
        host,
        origin,
        upgrade,
        raw,
    })
}

/// Whether a `Host` header names this machine.
///
/// The endpoint binds loopback, but a browser will happily resolve
/// `attacker.example` to `127.0.0.1` and then let a hostile page talk to it —
/// DNS rebinding. Chrome rejects any `Host` that is not a loopback name or an
/// IP literal for exactly this reason, and so does this. A request with no
/// `Host` at all is refused too: HTTP/1.1 requires one, and the only clients
/// that omit it are hand-rolled.
#[must_use]
pub fn host_is_local(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    // Strip the port. An IPv6 literal is bracketed, so split on the last colon
    // only when it is not inside brackets.
    let name = match host.rfind(']') {
        Some(end) => &host[..=end],
        None => host.split(':').next().unwrap_or(host),
    };
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether a request may be served at all.
///
/// **An `Origin` header disqualifies it.** Browsers apply neither CORS nor a
/// cross-origin block to `new WebSocket("ws://127.0.0.1:PORT/…")`, and such a
/// request carries a perfectly loopback `Host` — so [`host_is_local`] passes
/// with no rebinding involved at all, leaving only the path token, which
/// `token.rs` documents as *not* secret from anything that can read a reply.
/// A real driver never sends `Origin`; a web page always does. Chrome closes
/// the same vector with `--remote-allow-origins`, which defaults to refusing
/// every origin.
///
/// Applied to the `/json/*` endpoints too, not only to upgrades: the reply to a
/// cross-origin `fetch` is unreadable, but `/json/new` *acts*, and an effect
/// does not need a readable response to be worth having.
#[must_use]
pub fn origin_allowed(origin: Option<&str>) -> bool {
    origin.is_none()
}

/// A stream that replays already-read bytes before yielding the live socket.
///
/// This is what lets the router consume a request head to classify it and still
/// hand tokio-tungstenite a stream that looks untouched.
pub struct PrefixedStream {
    prefix: Vec<u8>,
    offset: usize,
    inner: TcpStream,
}

impl PrefixedStream {
    #[must_use]
    pub fn new(prefix: Vec<u8>, inner: TcpStream) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.offset < self.prefix.len() {
            let take = buf.remaining().min(self.prefix.len() - self.offset);
            let start = self.offset;
            buf.put_slice(&self.prefix[start..start + take]);
            self.offset += take;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl RequestHead {
    /// Consumes the head, handing back the bytes for replay.
    #[must_use]
    pub fn into_raw(self) -> Vec<u8> {
        self.raw
    }
}

/// Answers one `/json/*` request. The head has already been consumed.
pub async fn serve_http(
    mut stream: TcpStream,
    head: &RequestHead,
    registry: TargetRegistry,
    endpoint: &Endpoint,
) {
    let (status, body) = if !origin_allowed(head.origin.as_deref()) {
        (
            "403 Forbidden",
            String::from(r#"{"error":"Cross-origin requests are not accepted"}"#),
        )
    } else if host_is_local(head.host.as_deref()) {
        // `/json/new` creates a page, which spawns an OS thread and blocks
        // until its realm exists. The protocol runtime has two workers and no
        // business doing that, so the whole handler goes to a blocking pool —
        // the same rule the command lanes follow (see `session.rs`).
        let (method, path) = (head.method.clone(), head.path.clone());
        let endpoint = Endpoint {
            addr: endpoint.addr,
            token: endpoint.token.clone(),
        };
        tokio::task::spawn_blocking(move || respond(&method, &path, &registry, &endpoint))
            .await
            .unwrap_or_else(|_| {
                (
                    "500 Internal Server Error",
                    String::from(r#"{"error":"Handler panicked"}"#),
                )
            })
    } else {
        (
            "403 Forbidden",
            String::from(r#"{"error":"Host header is not a loopback address"}"#),
        )
    };

    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/json; charset=UTF-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let mut bytes = response.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    let _ = stream.write_all(&bytes).await;
    let _ = stream.flush().await;
}

/// Where this server is reachable, and under which token.
pub struct Endpoint {
    pub addr: SocketAddr,
    pub token: String,
}

impl Endpoint {
    /// The URL a driver connects to for browser-level control.
    #[must_use]
    pub fn browser_ws_url(&self) -> String {
        format!("ws://{}/devtools/browser/{}", self.addr, self.token)
    }

    /// The per-target URL. Not required by either driver — both attach through
    /// `Target.attachToTarget` on the browser socket — but `/json/list` is
    /// expected to carry one, and a missing member reads as a malformed entry.
    #[must_use]
    pub fn page_ws_url(&self, target_id: &str) -> String {
        format!(
            "ws://{}/devtools/page/{}/{target_id}",
            self.addr, self.token
        )
    }

    /// Whether `path` is an upgrade path bearing this server's token.
    ///
    /// Both `/devtools/browser/<token>` and `/devtools/page/<token>/<id>` are
    /// accepted; the token always sits in the third segment.
    #[must_use]
    pub fn authorizes(&self, path: &str) -> bool {
        let mut segments = path.trim_start_matches('/').split('/');
        if segments.next() != Some("devtools") {
            return false;
        }
        if !matches!(segments.next(), Some("browser" | "page")) {
            return false;
        }
        segments
            .next()
            .is_some_and(|provided| crate::token::token_matches(&self.token, provided))
    }
}

fn respond(
    method: &str,
    path: &str,
    registry: &TargetRegistry,
    endpoint: &Endpoint,
) -> (&'static str, String) {
    // Strip a query string before routing; `/json/new` carries its URL there.
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    match route {
        "/json/version" => ("200 OK", version_json(endpoint)),
        "/json" | "/json/list" => ("200 OK", list_json(registry, endpoint)),
        "/json/new" => new_target(method, query, registry, endpoint),
        _ => (
            "404 Not Found",
            String::from(r#"{"error":"Unknown endpoint"}"#),
        ),
    }
}

fn version_json(endpoint: &Endpoint) -> String {
    let navigator = oxidepage_engine::page_api::NavigatorProfile::default();
    serde_json::json!({
        "Browser": concat!("OxidePage/", env!("CARGO_PKG_VERSION")),
        "Protocol-Version": "1.3",
        "User-Agent": navigator.user_agent,
        // Reported empty rather than faked: there is no V8 and no WebKit here,
        // and a driver that branches on a version string should see the absence.
        "V8-Version": "",
        "WebKit-Version": "",
        "webSocketDebuggerUrl": endpoint.browser_ws_url(),
    })
    .to_string()
}

fn list_json(registry: &TargetRegistry, endpoint: &Endpoint) -> String {
    let entries: Vec<serde_json::Value> = registry
        .infos()
        .into_iter()
        .map(|info| {
            serde_json::json!({
                "description": "",
                "devtoolsFrontendUrl": "",
                "id": info.target_id,
                "title": info.title,
                "type": info.kind,
                "url": info.url,
                "webSocketDebuggerUrl": endpoint.page_ws_url(&info.target_id),
            })
        })
        .collect();
    serde_json::Value::Array(entries).to_string()
}

fn new_target(
    method: &str,
    query: &str,
    registry: &TargetRegistry,
    endpoint: &Endpoint,
) -> (&'static str, String) {
    // **PUT only**, which is a security boundary rather than pedantry, and the
    // reason Chrome moved this endpoint to PUT in the first place.
    //
    // `GET` and `POST` are CORS-*simple*: any page can issue one cross-origin
    // with no preflight and no permission, and `<img src="http://127.0.0.1:9222
    // /json/new?url=…">` does not even send an `Origin` for the check above to
    // catch. That is a page on the open web opening a target and navigating it
    // from the operator's network position, repeatable in a loop. `PUT` is not
    // simple: a browser must preflight it with `OPTIONS`, which this endpoint
    // never answers, so the request is refused before it is sent. A driver
    // that only knows the old `GET` spelling loses a route it does not use —
    // Puppeteer and Playwright both create targets over the protocol.
    if method != "PUT" {
        return (
            "405 Method Not Allowed",
            String::from(r#"{"error":"Use PUT /json/new"}"#),
        );
    }
    let url = if query.is_empty() {
        String::from("about:blank")
    } else {
        percent_decode(query)
    };

    let context = registry.browser().default_context();
    let options = oxidepage_engine::NewPageOptions {
        url: (url == "about:blank").then(|| url.clone()),
        ..Default::default()
    };
    let Ok(target_id) = registry.create_page(&context, options) else {
        return (
            "500 Internal Server Error",
            String::from(r#"{"error":"Failed to create a page"}"#),
        );
    };

    if url != "about:blank"
        && let Some(page) = registry.page(&target_id)
    {
        let target = url.clone();
        let _ = page.post(move |p| {
            let _ = p.navigate(&target, oxidepage_engine::page_api::WaitUntil::Load);
        });
    }

    (
        "200 OK",
        serde_json::json!({
            "description": "",
            "devtoolsFrontendUrl": "",
            "id": target_id,
            "title": "",
            "type": "page",
            "url": url,
            "webSocketDebuggerUrl": endpoint.page_ws_url(&target_id),
        })
        .to_string(),
    )
}

/// Minimal `%XX` + `+` decoding for the one query string this endpoint reads.
///
/// Deliberately not `percent-encoding`: that crate is `crates/net`'s, and this
/// is one query parameter on a loopback control endpoint, not URL parsing.
fn percent_decode(query: &str) -> String {
    let raw = query.strip_prefix("url=").unwrap_or(query);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            addr: "127.0.0.1:9222".parse().unwrap(),
            token: String::from("0123456789abcdef0123456789abcdef"),
        }
    }

    #[test]
    fn authorizes_the_browser_and_page_paths() {
        let endpoint = endpoint();
        assert!(endpoint.authorizes(&format!("/devtools/browser/{}", endpoint.token)));
        assert!(endpoint.authorizes(&format!("/devtools/page/{}/abc", endpoint.token)));
    }

    #[test]
    fn rejects_a_wrong_or_missing_token() {
        let endpoint = endpoint();
        assert!(!endpoint.authorizes("/devtools/browser/"));
        assert!(!endpoint.authorizes("/devtools/browser"));
        assert!(!endpoint.authorizes("/devtools/browser/deadbeef"));
        assert!(!endpoint.authorizes(&format!("/devtools/other/{}", endpoint.token)));
        assert!(!endpoint.authorizes(&format!("/{}", endpoint.token)));
        // A token that is a prefix of the real one must not pass.
        assert!(!endpoint.authorizes(&format!("/devtools/browser/{}", &endpoint.token[..16])));
    }

    #[test]
    fn builds_the_advertised_urls() {
        let endpoint = endpoint();
        assert_eq!(
            endpoint.browser_ws_url(),
            "ws://127.0.0.1:9222/devtools/browser/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            endpoint.page_ws_url("t1"),
            "ws://127.0.0.1:9222/devtools/page/0123456789abcdef0123456789abcdef/t1"
        );
    }

    #[test]
    fn decodes_the_new_target_query() {
        assert_eq!(percent_decode("url=about:blank"), "about:blank");
        assert_eq!(
            percent_decode("url=http%3A%2F%2F127.0.0.1%3A80%2Fa%20b"),
            "http://127.0.0.1:80/a b"
        );
        assert_eq!(percent_decode("url=a+b"), "a b");
        // A stray `%` at the end is kept literally rather than truncating.
        assert_eq!(percent_decode("url=a%"), "a%");
    }
}
