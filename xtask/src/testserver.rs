//! A minimal in-process HTTP test server (design doc §9, Phase 3) for the
//! WPT `url/` subset: it serves vendored files over loopback and generates
//! `.any.html`/`.window.html` harness wrappers on the fly, so the engine
//! loads and runs those tests over the *real* network stack (navigation +
//! external scripts + `fetch()` of the test's JSON resources).
//!
//! Raw HTTP/1.1 with `Connection: close` per request — enough for the runner,
//! and it needs no hyper server dependency.
//!
//! # Behavioural routes
//!
//! Static files alone cannot express what the Puppeteer suite needs to test as
//! of ADR-0032: an upload target has to *read a request body*, a download needs
//! a `Content-Disposition` header, and `page.authenticate` needs a route that
//! answers 401 until credentials arrive. Those three live in [`behavioural`],
//! under reserved `/-/` paths so they can never collide with a vendored WPT
//! file.

use std::path::PathBuf;
use std::sync::mpsc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback file server rooted at a vendor directory.
pub struct TestServer {
    port: u16,
}

impl TestServer {
    /// Starts the server on a background thread, returning once it is bound.
    /// `report_hook` is served for any `testharnessreport.js` request (the
    /// runner's completion hook).
    pub fn start(vendor_root: PathBuf, report_hook: String) -> TestServer {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test-server runtime");
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                tx.send(listener.local_addr().unwrap().port()).unwrap();
                loop {
                    let Ok((sock, _)) = listener.accept().await else {
                        return;
                    };
                    let root = vendor_root.clone();
                    let hook = report_hook.clone();
                    tokio::spawn(async move { serve(sock, &root, &hook).await });
                }
            });
        });
        TestServer {
            port: rx.recv().expect("test server failed to start"),
        }
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Largest request head accepted. A client that never terminates its headers
/// would otherwise grow `buf` without bound.
const MAX_HEAD_BYTES: usize = 64 * 1024;

async fn serve(mut sock: tokio::net::TcpStream, root: &std::path::Path, report_hook: &str) {
    // Read request headers.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let Ok(n) = sock.read(&mut tmp).await else {
            return;
        };
        if n == 0 {
            return;
        }
        // Only the tail can complete a `\r\n\r\n` that the last read split, so
        // rescan from three bytes back instead of over the whole buffer (O(n²)).
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&tmp[..n]);
        if buf[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return;
        }
    }
    // The head is complete; split the bytes of the body that came with it.
    let head_len = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(buf.len(), |at| at + 4);
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut body_bytes = buf[head_len..].to_vec();

    let mut request_line = head.lines().next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or("GET").to_owned();
    let raw_path = request_line.next().unwrap_or("/");
    // Strip a query string (WPT variants) before resolving to a file.
    let path = raw_path.split('?').next().unwrap_or("/").to_owned();

    // Only a behavioural route ever needs the body, and reading one for every
    // WPT file would stall on a client that sent no `Content-Length`.
    if path.starts_with(BEHAVIOURAL_PREFIX)
        && let Some(declared) = header_value(&head, "content-length").and_then(|v| v.parse().ok())
    {
        read_body(&mut sock, &mut body_bytes, declared).await;
    }

    let response = if path.starts_with(BEHAVIOURAL_PREFIX) {
        behavioural(&method, &path, &head, &body_bytes)
    } else {
        let (status, content_type, body) = respond(&path, root, report_hook);
        Response {
            status: status.to_owned(),
            headers: vec![(String::from("Content-Type"), content_type.to_owned())],
            body,
        }
    };

    let mut out = format!("HTTP/1.1 {}\r\n", response.status);
    for (name, value) in &response.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&response.body);
    let _ = sock.write_all(&bytes).await;
    let _ = sock.flush().await;
}

/// One response, with whatever headers the route needs.
///
/// The old `(status, content_type, body)` triple could not express a
/// `Content-Disposition` or a `WWW-Authenticate`, which is exactly what the
/// download and auth routes are for.
struct Response {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Every behavioural route lives under this prefix, so none can shadow a
/// vendored WPT file.
const BEHAVIOURAL_PREFIX: &str = "/-/";

/// Largest request body accepted, so a bad `Content-Length` cannot make the
/// runner allocate without bound.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

fn header_value(head: &str, name: &str) -> Option<String> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with(&prefix))
        .map(|line| line[name.len() + 1..].trim().to_owned())
}

/// Reads the rest of a declared body into `body`.
async fn read_body(sock: &mut tokio::net::TcpStream, body: &mut Vec<u8>, declared: usize) {
    let declared = declared.min(MAX_BODY_BYTES);
    let mut tmp = [0u8; 8192];
    while body.len() < declared {
        let Ok(read) = sock.read(&mut tmp).await else {
            return;
        };
        if read == 0 {
            return;
        }
        body.extend_from_slice(&tmp[..read]);
    }
}

/// The routes that need behavior rather than a file (ADR-0032).
///
/// Modelled on `crates/cdp/tests/common/mod.rs`'s `/slow-<ms>`: the shortest
/// thing that expresses the property, and nothing configurable.
fn behavioural(method: &str, path: &str, head: &str, body: &[u8]) -> Response {
    let json = |status: &str, value: String| Response {
        status: status.to_owned(),
        headers: vec![(
            String::from("Content-Type"),
            String::from("application/json"),
        )],
        body: value.into_bytes(),
    };
    match path {
        // Echoes what an upload actually carried, so a test can assert the
        // multipart body rather than merely that a request happened.
        "/-/upload" => {
            let text = String::from_utf8_lossy(body);
            let content_type = header_value(head, "content-type").unwrap_or_default();
            // The filenames and the file bodies, extracted from the multipart
            // parts, so the check does not have to parse multipart in JS.
            let filenames: Vec<String> = text
                .split("filename=\"")
                .skip(1)
                .filter_map(|rest| rest.split('"').next().map(ToOwned::to_owned))
                .collect();
            json(
                "200 OK",
                format!(
                    r#"{{"method":{},"contentType":{},"filenames":{},"body":{}}}"#,
                    json_string(method),
                    json_string(&content_type),
                    serde_json_array(&filenames),
                    json_string(&text),
                ),
            )
        }
        // A download: the navigation must not commit.
        "/-/attachment" => Response {
            status: String::from("200 OK"),
            headers: vec![
                (String::from("Content-Type"), String::from("text/csv")),
                (
                    String::from("Content-Disposition"),
                    String::from("attachment; filename=\"report.csv\""),
                ),
            ],
            body: b"a,b\n1,2\n".to_vec(),
        },
        // 401 until an `Authorization` header arrives, then 200 naming it.
        "/-/auth" => match header_value(head, "authorization") {
            Some(credentials) => json(
                "200 OK",
                format!(r#"{{"seen":{}}}"#, json_string(&credentials)),
            ),
            None => Response {
                status: String::from("401 Unauthorized"),
                headers: vec![
                    (String::from("Content-Type"), String::from("text/html")),
                    (
                        String::from("WWW-Authenticate"),
                        String::from("Basic realm=\"automation\""),
                    ),
                ],
                body: b"<title>401</title>".to_vec(),
            },
        },
        // A plain document, for interception checks that need something real to
        // continue to.
        "/-/hello" => Response {
            status: String::from("200 OK"),
            headers: vec![(String::from("Content-Type"), String::from("text/html"))],
            body: b"<title>hello</title><p id=p>from the server</p>".to_vec(),
        },
        _ => Response {
            status: String::from("404 Not Found"),
            headers: vec![(String::from("Content-Type"), String::from("text/plain"))],
            body: b"no such behavioural route".to_vec(),
        },
    }
}

/// A JSON string literal. Hand-rolled because `xtask` has no `serde_json` and
/// the payload is a handful of fields.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn serde_json_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| json_string(v)).collect();
    format!("[{}]", items.join(","))
}

fn respond(
    path: &str,
    root: &std::path::Path,
    report_hook: &str,
) -> (&'static str, &'static str, Vec<u8>) {
    // The completion hook stands in for testharnessreport.js.
    if path.ends_with("testharnessreport.js") {
        return ("200 OK", "text/javascript", report_hook.as_bytes().to_vec());
    }
    // Generate `.any.html` / `.window.html` harness wrappers on the fly.
    if let Some(stem) = path
        .strip_suffix(".any.html")
        .or_else(|| path.strip_suffix(".window.html"))
    {
        let js_rel = format!("{stem}{}", any_or_window(path));
        let Some(file) = resolve_within(root, &js_rel) else {
            return traversal_rejected();
        };
        return match std::fs::read_to_string(&file) {
            Ok(source) => (
                "200 OK",
                "text/html",
                wrapper(&js_rel, &source).into_bytes(),
            ),
            Err(_) => (
                "404 Not Found",
                "text/plain",
                b"missing test script".to_vec(),
            ),
        };
    }

    // Serve a vendored file.
    let Some(file) = resolve_within(root, path) else {
        return traversal_rejected();
    };
    match std::fs::read(&file) {
        Ok(bytes) => ("200 OK", content_type(path), bytes),
        Err(_) => ("404 Not Found", "text/plain", b"not found".to_vec()),
    }
}

/// Joins a request path onto the vendor root, rejecting any path that would
/// escape the root via `..` (path traversal). A vendored/crafted WPT test doing
/// `fetch("/../../../../etc/passwd")` must not read files outside the root, so a
/// `..` (`ParentDir`) — or any non-`Normal`/`CurDir` — component yields `None`.
fn resolve_within(root: &std::path::Path, request_path: &str) -> Option<PathBuf> {
    let rel = request_path.trim_start_matches('/');
    for component in std::path::Path::new(rel).components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(root.join(rel))
}

/// The response for a rejected path-traversal request.
fn traversal_rejected() -> (&'static str, &'static str, Vec<u8>) {
    ("400 Bad Request", "text/plain", b"invalid path".to_vec())
}

fn any_or_window(html_path: &str) -> &'static str {
    if html_path.ends_with(".window.html") {
        ".window.js"
    } else {
        ".any.js"
    }
}

/// Builds the `.any.html` harness wrapper for a `.any.js`/`.window.js` test.
fn wrapper(js_url: &str, source: &str) -> String {
    let mut scripts = String::new();
    for meta_script in meta_scripts(source, js_url) {
        scripts.push_str(&format!("<script src=\"{meta_script}\"></script>\n"));
    }
    format!(
        "<!doctype html>\n<meta charset=utf-8>\n\
         <script>self.GLOBAL={{isWindow(){{return true}},isWorker(){{return false}},isShadowRealm(){{return false}}}};</script>\n\
         <script src=\"/resources/testharness.js\"></script>\n\
         <script src=\"/resources/testharnessreport.js\"></script>\n\
         <div id=log></div>\n{scripts}<script src=\"{js_url}\"></script>\n"
    )
}

/// Extracts `// META: script=X` directives, resolving each to a server path.
fn meta_scripts(source: &str, js_url: &str) -> Vec<String> {
    let dir = js_url.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    source
        .lines()
        .take_while(|l| l.starts_with("//") || l.trim().is_empty())
        .filter_map(|l| l.trim().strip_prefix("// META: script="))
        .map(|s| {
            if s.starts_with('/') {
                s.to_owned()
            } else {
                format!("{dir}/{}", s.trim_start_matches("./"))
            }
        })
        .collect()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html" | "htm" | "xhtml") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("json") => "application/json",
        Some("css") => "text/css",
        Some("txt") => "text/plain",
        // Images and fonts: a WPT layout test sized from `support/solidblue.png`
        // needs the decoder to actually run, and the decoder is picked by type.
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolve_within_rejects_parent_dir_traversal() {
        let root = Path::new("/vendor/root");
        assert_eq!(resolve_within(root, "/../../../../etc/passwd"), None);
        assert_eq!(resolve_within(root, "/url/../../etc/passwd"), None);
        assert_eq!(resolve_within(root, "url/../../secret"), None);
        // A `..` anywhere in the path is rejected, even after real segments.
        assert_eq!(resolve_within(root, "/resources/../../escape"), None);
    }

    #[test]
    fn resolve_within_allows_normal_paths() {
        let root = Path::new("/vendor/root");
        assert_eq!(
            resolve_within(root, "/resources/testharness.js"),
            Some(root.join("resources/testharness.js"))
        );
        assert_eq!(
            resolve_within(root, "/url/resources/a.json"),
            Some(root.join("url/resources/a.json"))
        );
        // A leading `./` is a no-op, not a traversal.
        assert_eq!(
            resolve_within(root, "/./resources/x.js"),
            Some(root.join("./resources/x.js"))
        );
    }

    #[test]
    fn respond_rejects_traversal_file_path() {
        // Root need not exist: the traversal check precedes any filesystem read.
        let root = Path::new("/nonexistent/vendor");
        let (status, _, _) = respond("/../../../../etc/passwd", root, "hook");
        assert!(
            status.starts_with("400"),
            "expected rejection, got {status}"
        );
    }

    #[test]
    fn respond_rejects_traversal_wrapper_path() {
        let root = Path::new("/nonexistent/vendor");
        let (status, _, _) = respond("/../../../../etc/evil.any.html", root, "hook");
        assert!(
            status.starts_with("400"),
            "expected rejection, got {status}"
        );
    }
}
