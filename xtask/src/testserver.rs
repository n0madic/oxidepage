//! A minimal in-process HTTP test server (design doc §9, Phase 3) for the
//! WPT `url/` subset: it serves vendored files over loopback and generates
//! `.any.html`/`.window.html` harness wrappers on the fly, so the engine
//! loads and runs those tests over the *real* network stack (navigation +
//! external scripts + `fetch()` of the test's JSON resources).
//!
//! Raw HTTP/1.1 with `Connection: close` per request — enough for the runner,
//! and it needs no hyper server dependency.

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
    let head = String::from_utf8_lossy(&buf);
    let raw_path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    // Strip a query string (WPT variants) before resolving to a file.
    let path = raw_path.split('?').next().unwrap_or("/");

    let (status, content_type, body) = respond(path, root, report_hook);
    let mut out = format!("HTTP/1.1 {status}\r\n");
    out.push_str(&format!("Content-Type: {content_type}\r\n"));
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&body);
    let _ = sock.write_all(&bytes).await;
    let _ = sock.flush().await;
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
