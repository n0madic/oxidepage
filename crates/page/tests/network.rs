//! Stage C + D verification: a real HTTP document loads over the net stack
//! and its inline + external + `defer` + `async` + module scripts run in the
//! correct order; a module with a static import and `import.meta.url`
//! evaluates; `load` waits for subresources; redirects update `document.URL`;
//! `fetch()`/XHR round-trip; and a `Set-Cookie` from a load reaches
//! `document.cookie`.

use std::sync::Mutex;
use std::time::Duration;

use oxidepage_page::{NavigatorProfile, Page, PageOptions, ResourcePolicy, WaitUntil};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Captures the `Referer` header seen on the `/dep.mjs` module load.
static DEP_REFERER: Mutex<Option<String>> = Mutex::new(None);
static PROFILE_HEADERS: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());

/// Reads a request header value from the raw request head.
fn header_value(head: &str, name: &str) -> Option<String> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&prefix))
        .map(|l| l[name.len() + 1..].trim().to_owned())
}

/// Runs a loopback HTTP server on its own thread/runtime, returning its port.
fn spawn_server() -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    let header_end = loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                    let mut fields = head.split_whitespace();
                    let method = fields.next().unwrap_or("GET").to_owned();
                    let path = fields.next().unwrap_or("/").to_owned();
                    let content_length = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l[15..].trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[header_end..]).into_owned();
                    // `/slow` answers late enough that a navigation can happen
                    // while the request is still in flight; `/delay/<ms>` is the
                    // parameterized form, so a test can name its own latency
                    // instead of a new hardcoded path per case.
                    if path == "/slow" || path == "/ordered-first.js" {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    if let Some(ms) = delay_ms(&path) {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    // `/chunked/<n>` writes `n` chunks with a pause between
                    // them, so the XHR progress loop can be exercised against a
                    // real chunk *stream* — the net layer buffers whole bodies
                    // today, but the XHR side is written for the streaming case.
                    if let Some(chunks) = chunk_count(&path) {
                        let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                                     Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                        let _ = sock.write_all(head).await;
                        let _ = sock.flush().await;
                        for i in 0..chunks {
                            let piece = format!("chunk{i};");
                            let framed = format!("{:x}\r\n{piece}\r\n", piece.len());
                            let _ = sock.write_all(framed.as_bytes()).await;
                            let _ = sock.flush().await;
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        let _ = sock.write_all(b"0\r\n\r\n").await;
                        let _ = sock.flush().await;
                        return;
                    }
                    let _ = sock.write_all(&route(&method, &path, &body, &head)).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    rx.recv().unwrap()
}

/// `/delay/<ms>` (optionally with a query string) → the delay to apply.
fn delay_ms(path: &str) -> Option<u64> {
    path_param(path, "/delay/")?.parse().ok()
}

/// `/chunked/<n>` → how many chunks to stream.
fn chunk_count(path: &str) -> Option<usize> {
    path_param(path, "/chunked/")?.parse().ok()
}

/// The single path segment following `prefix`, with any query string removed.
fn path_param<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    Some(rest.split(['/', '?']).next().unwrap_or(rest))
}

fn route(_method: &str, path: &str, body: &str, head: &str) -> Vec<u8> {
    let js = "text/javascript";
    let user_agent = header_value(head, "User-Agent").unwrap_or_default();
    if user_agent.contains("ProfileTest/9") {
        PROFILE_HEADERS.lock().unwrap().push((
            path.to_owned(),
            user_agent,
            header_value(head, "Accept-Language").unwrap_or_default(),
        ));
    }
    if delay_ms(path).is_some() {
        return resp(200, "OK", "text/plain", &[], "delayed");
    }
    match path {
        "/index.html" => resp(
            200,
            "OK",
            "text/html",
            &[("Set-Cookie", "pref=green; Path=/")],
            r#"<!DOCTYPE html><html><body>
               <script id="inline">window.order = ['inline1'];</script>
               <script id="external" src="/ext.js"></script>
               <script id="deferred" defer src="/defer.js"></script>
               <script id="asynchronous" async src="/async.js"></script>
               <script type="module" src="/main.mjs"></script>
               <script>window.order.push('inline2');</script>
             </body></html>"#,
        ),
        "/ext.js" => resp(
            200,
            "OK",
            js,
            &[],
            "window.externalCurrent = document.currentScript && document.currentScript.id; \
             window.order.push('ext');",
        ),
        "/defer.js" => resp(
            200,
            "OK",
            js,
            &[],
            "window.deferredCurrent = document.currentScript && document.currentScript.id; \
             window.order.push('defer');",
        ),
        "/async.js" => resp(
            200,
            "OK",
            js,
            &[],
            "window.asyncCurrent = document.currentScript && document.currentScript.id; \
             window.order.push('async'); window.asyncRan = true;",
        ),
        "/dynamic.js" => resp(
            200,
            "OK",
            js,
            &[],
            "window.dynamicExternalRuns = (window.dynamicExternalRuns || 0) + 1; \
             window.dynamicExternalCurrent = document.currentScript && document.currentScript.id;",
        ),
        "/dynamic-syntax-error.js" => resp(200, "OK", js, &[], "function ("),
        "/ordered-first.js" => resp(200, "OK", js, &[], "window.dynamicOrder.push('first');"),
        "/ordered-second.js" => resp(200, "OK", js, &[], "window.dynamicOrder.push('second');"),
        "/profile-redirect" => resp(
            302,
            "Found",
            "text/plain",
            &[("Location", "/profile.html")],
            "",
        ),
        "/profile.html" => resp(
            200,
            "OK",
            "text/html",
            &[],
            "<!doctype html><script src='/profile.js'></script>\
             <script type='module' src='/profile-module.mjs'></script>",
        ),
        "/profile.js" => resp(200, "OK", js, &[], "window.profileScriptLoaded = true;"),
        "/profile-module.mjs" => resp(
            200,
            "OK",
            js,
            &[],
            "import { value } from './profile-dep.mjs'; window.profileModule = value;",
        ),
        "/profile-dep.mjs" => resp(200, "OK", js, &[], "export const value = 9;"),
        "/profile-fetch" => resp(200, "OK", "text/plain", &[], "ok"),
        "/profile-xhr" => resp(200, "OK", "text/plain", &[], "xhr-ok"),
        "/main.mjs" => resp(
            200,
            "OK",
            js,
            &[],
            "import { v } from './dep.mjs';\n\
             window.moduleResult = v;\n\
             window.metaUrl = import.meta.url;",
        ),
        "/dep.mjs" => {
            *DEP_REFERER.lock().unwrap() = header_value(head, "Referer");
            resp(200, "OK", js, &[], "export const v = 7;")
        }
        // A windows-1252 document with no HTTP charset: decoding must honor
        // the `<meta charset>` rather than defaulting to UTF-8.
        "/latin1" => {
            let mut html = b"<!doctype html><meta charset=windows-1252><title>caf".to_vec();
            html.push(0xE9); // 'é' in windows-1252
            html.extend_from_slice(b"</title>");
            let mut out = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                html.len()
            )
            .into_bytes();
            out.extend_from_slice(&html);
            out
        }
        // A second document (no async script) for the stale-async test.
        "/index2.html" => resp(
            200,
            "OK",
            "text/html",
            &[],
            "<!DOCTYPE html><html><body><script>window.doc = 2;</script></body></html>",
        ),
        "/slow" => resp(200, "OK", "text/plain", &[], "late"),
        "/text" => resp(200, "OK", "text/plain", &[], "plain-text-body"),
        "/json" => resp(200, "OK", "application/json", &[], r#"{"msg":"hi","n":42}"#),
        "/echo" => resp(200, "OK", "text/plain", &[], body),
        // Echoes the request's `Content-Type` *and* body. A `FormData` body is
        // only correct if both agree: the header must carry the same multipart
        // boundary that delimits the parts.
        "/echo-with-type" => {
            let content_type = header_value(head, "Content-Type").unwrap_or_default();
            resp(
                200,
                "OK",
                "text/plain",
                &[],
                &format!("{content_type}\n{body}"),
            )
        }
        "/redirect" => resp(
            302,
            "Found",
            "text/plain",
            &[("Location", "/index.html")],
            "",
        ),
        // === XHR fixtures ===
        "/xhr-redirect" => resp(302, "Found", "text/plain", &[("Location", "/text")], ""),
        // A same-origin response carrying a `Set-Cookie`. The net layer forwards
        // the whole header map for a `basic` response, so this is what proves
        // XHR filters it back out.
        "/xhr-cookie" => resp(
            200,
            "OK",
            "text/plain",
            &[("Set-Cookie", "secret=1; Path=/"), ("X-Visible", "yes")],
            "cookie-body",
        ),
        // Deliberately out of alphabetical order, with one name repeated:
        // `getAllResponseHeaders` must sort and combine.
        "/xhr-headers" => resp(
            200,
            "OK",
            "text/plain",
            &[("X-Zebra", "z"), ("X-Alpha", "a1"), ("X-Alpha", "a2")],
            "headers-body",
        ),
        // Echoes one request header, so `setRequestHeader` combining and the
        // forbidden-name filter are observable from script.
        "/xhr-echo-header" => {
            let combined = header_value(head, "X-Combined").unwrap_or_default();
            let referer = header_value(head, "Referer").unwrap_or_default();
            resp(
                200,
                "OK",
                "text/plain",
                &[],
                &format!("{combined}|{}", !referer.contains("evil")),
            )
        }
        // A windows-1252 body with **no** charset in the `Content-Type`, so
        // `overrideMimeType('…; charset=windows-1252')` is what makes it decode.
        "/xhr-latin1" => {
            let mut bytes = b"caf".to_vec();
            bytes.push(0xE9); // 'e' with acute, in windows-1252
            let mut out = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .into_bytes();
            out.extend_from_slice(&bytes);
            out
        }
        "/xhr-html" => resp(
            200,
            "OK",
            "text/html",
            &[],
            "<!doctype html><title>doc-title</title><p id=p>hello</p>",
        ),
        "/xhr-xml" => resp(
            200,
            "OK",
            "application/xml",
            &[],
            "<root><item>xml-text</item></root>",
        ),
        "/xhr-500" => resp(500, "Internal Server Error", "text/plain", &[], "boom"),
        _ => resp(404, "Not Found", "text/plain", &[], "nope"),
    }
}

fn resp(
    status: u16,
    reason: &str,
    content_type: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    out.push_str(&format!("Content-Type: {content_type}\r\n"));
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    out.push_str("Connection: close\r\n");
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    out.into_bytes()
}

fn loopback_page() -> Page {
    Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap()
}

#[test]
fn http_page_runs_scripts_in_order_and_waits_for_load() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();

    // inline1, ext (parser-blocking), inline2 preserve their relative document
    // order. `async` can run as soon as its fetch is available — including
    // between ext and inline2 — while `defer` runs after parsing. Assert the
    // deterministic subsequence and that both non-blocking scripts ran once.
    let order = page.eval_to_string("window.order.join(',')").unwrap();
    let errors = page.drain_errors();
    let parts: Vec<&str> = order.split(',').collect();
    let parser_order: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| !matches!(*part, "async" | "defer"))
        .collect();
    assert_eq!(
        parser_order,
        ["inline1", "ext", "inline2"],
        "unexpected parse-time order in {order:?}, errors: {errors:?}"
    );
    let mut non_blocking: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| matches!(*part, "async" | "defer"))
        .collect();
    non_blocking.sort_unstable();
    assert_eq!(
        non_blocking,
        ["async", "defer"],
        "expected defer+async exactly once in {order:?}, errors: {errors:?}"
    );
    // `load` waited for the async subresource.
    assert!(page.is_loaded());
    assert_eq!(
        page.eval_to_string("window.asyncRan === true").unwrap(),
        "true"
    );
}

#[test]
fn document_current_script_tracks_external_classic_execution_modes() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();

    assert_eq!(
        page.eval_to_string("[externalCurrent, deferredCurrent, asyncCurrent].join(',')")
            .unwrap(),
        "external,deferred,asynchronous"
    );
    assert_eq!(
        page.eval_to_string("document.currentScript === null")
            .unwrap(),
        "true"
    );
}

#[test]
fn navigator_profile_matches_navigation_subresource_and_fetch_headers() {
    let port = spawn_server();
    PROFILE_HEADERS.lock().unwrap().clear();
    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        navigator: NavigatorProfile {
            user_agent: "Mozilla/5.0 ProfileTest/9".to_owned(),
            vendor: String::new(),
            platform: "TestOS".to_owned(),
            languages: vec!["uk-UA".to_owned(), "en-US".to_owned(), "en".to_owned()],
            hardware_concurrency: 2,
            webdriver: false,
            max_touch_points: 0,
        },
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(
        &format!("http://127.0.0.1:{port}/profile-redirect"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "fetch('/profile-fetch', {
           headers: {'Accept-Language': 'fr', 'User-Agent': 'Spoofed/1'}
         }).then(r => r.text()).then(t => { window.profileFetch = t; })",
    )
    .unwrap();
    page.eval(
        "const profileXhr = new XMLHttpRequest();
         profileXhr.open('GET', '/profile-xhr');
         profileXhr.setRequestHeader('Accept-Language', 'de');
         profileXhr.setRequestHeader('User-Agent', 'Spoofed/2');
         profileXhr.onload = () => { window.profileXhr = profileXhr.responseText; };
         profileXhr.send();",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("navigator.userAgent").unwrap(),
        "Mozilla/5.0 ProfileTest/9"
    );
    assert_eq!(page.eval_to_string("window.profileFetch").unwrap(), "ok");
    assert_eq!(page.eval_to_string("window.profileModule").unwrap(), "9");
    assert_eq!(page.eval_to_string("window.profileXhr").unwrap(), "xhr-ok");
    let headers = PROFILE_HEADERS.lock().unwrap().clone();
    for path in [
        "/profile-redirect",
        "/profile.html",
        "/profile.js",
        "/profile-module.mjs",
        "/profile-dep.mjs",
    ] {
        assert!(
            headers.iter().any(|record| {
                record.0 == path
                    && record.1 == "Mozilla/5.0 ProfileTest/9"
                    && record.2 == "uk-UA, en-US;q=0.9, en;q=0.8"
            }),
            "missing synchronized headers for {path}: {headers:?}"
        );
    }
    assert!(
        headers.iter().any(|record| {
            record.0 == "/profile-fetch"
                && record.1 == "Mozilla/5.0 ProfileTest/9"
                && record.2 == "fr"
        }),
        "fetch defaults/override mismatch: {headers:?}"
    );
    assert!(
        headers.iter().any(|record| {
            record.0 == "/profile-xhr"
                && record.1 == "Mozilla/5.0 ProfileTest/9"
                && record.2 == "de"
        }),
        "XHR defaults/override mismatch: {headers:?}"
    );
}

#[test]
fn dynamic_external_scripts_dispatch_load_and_error_and_do_not_repeat() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index2.html"),
        WaitUntil::Load,
    )
    .unwrap();

    page.eval(
        "window.dynamicEvents = [];
         const good = document.createElement('script');
         good.id = 'dynamic-external';
         good.onload = () => dynamicEvents.push('property-load');
         good.addEventListener('load', () => dynamicEvents.push('listener-load'));
         good.onerror = () => dynamicEvents.push('property-error-unexpected');
         good.src = '/dynamic.js';
         document.body.appendChild(good);

         const bad = document.createElement('script');
         bad.onload = () => dynamicEvents.push('load-unexpected');
         bad.onerror = () => dynamicEvents.push('property-error');
         bad.addEventListener('error', () => dynamicEvents.push('listener-error'));
         bad.src = '/dynamic-syntax-error.js';
         document.body.appendChild(bad);",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("window.dynamicExternalRuns").unwrap(),
        "1"
    );
    assert_eq!(
        page.eval_to_string("window.dynamicExternalCurrent")
            .unwrap(),
        "dynamic-external"
    );
    assert_eq!(
        page.eval_to_string("dynamicEvents.sort().join(',')")
            .unwrap(),
        "listener-error,listener-load,property-error,property-load"
    );

    page.eval(
        "const script = document.getElementById('dynamic-external');
         document.body.removeChild(script);
         script.src = '/dynamic.js?again';
         document.body.appendChild(script);",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.dynamicExternalRuns").unwrap(),
        "1"
    );
}

#[test]
fn dynamic_external_scripts_with_async_false_keep_insertion_order() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index2.html"),
        WaitUntil::Load,
    )
    .unwrap();

    page.eval(
        "window.dynamicOrder = [];
         for (const src of ['/ordered-first.js', '/ordered-second.js']) {
           const script = document.createElement('script');
           script.async = false;
           script.src = src;
           document.body.appendChild(script);
         }",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("dynamicOrder.join(',')").unwrap(),
        "first,second"
    );
}

#[test]
fn module_with_static_import_and_meta_url() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    assert_eq!(
        page.eval_to_string("window.moduleResult").unwrap(),
        "7",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(
        page.eval_to_string("window.metaUrl").unwrap(),
        format!("http://127.0.0.1:{port}/main.mjs")
    );
}

#[test]
fn redirect_updates_document_url() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/redirect"),
        WaitUntil::Load,
    )
    .unwrap();
    assert_eq!(
        page.eval_to_string("document.URL").unwrap(),
        format!("http://127.0.0.1:{port}/index.html")
    );
}

#[test]
fn fetch_get_text_and_json() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();

    page.eval("fetch('/text').then(r => r.text()).then(t => { window.fetchedText = t; })")
        .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.fetchedText").unwrap(),
        "plain-text-body"
    );

    page.eval("fetch('/json').then(r => r.json()).then(j => { window.fetchedN = j.n; window.fetchedMsg = j.msg; })")
        .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(page.eval_to_string("window.fetchedN").unwrap(), "42");
    assert_eq!(page.eval_to_string("window.fetchedMsg").unwrap(), "hi");
}

#[test]
fn fetch_post_echoes_body() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "fetch('/echo', { method: 'POST', body: 'ping-pong' })\
         .then(r => r.text()).then(t => { window.echoed = t; })",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(page.eval_to_string("window.echoed").unwrap(), "ping-pong");
}

/// A `fetch` aborted while in flight rejects its promise with the signal's
/// reason and never resolves, even though `/slow` answers later.
#[test]
fn fetch_aborted_in_flight_rejects_with_reason() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "(() => {
            window.abortErr = '';
            window.resolved = 0;
            const c = new AbortController();
            fetch('/slow', { signal: c.signal })
                .then(() => { window.resolved = 1; })
                .catch(e => { window.abortErr = e.name; });
            c.abort();
        })()",
    )
    .unwrap();
    // Wait past `/slow`'s 200 ms delay so a surviving request would resolve.
    page.settle(Duration::from_secs(2));
    assert_eq!(
        page.eval_to_string("window.abortErr").unwrap(),
        "AbortError",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(page.eval_to_string("window.resolved").unwrap(), "0");
}

/// A `fetch` given an already-aborted signal rejects immediately with that
/// signal's exact reason and never starts.
#[test]
fn fetch_with_pre_aborted_signal_rejects_immediately() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "(() => {
            window.preErr = '';
            window.sameReason = false;
            window.resolved = 0;
            const reason = new DOMException('nope', 'AbortError');
            const s = AbortSignal.abort(reason);
            fetch('/slow', { signal: s })
                .then(() => { window.resolved = 1; })
                .catch(e => { window.preErr = e.name; window.sameReason = (e === reason); });
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(2));
    assert_eq!(page.eval_to_string("window.preErr").unwrap(), "AbortError");
    assert_eq!(page.eval_to_string("window.sameReason").unwrap(), "true");
    assert_eq!(page.eval_to_string("window.resolved").unwrap(), "0");
}

#[test]
fn xhr_get_fires_load() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            window.states = [];
            x.onreadystatechange = () => window.states.push(x.readyState);
            x.onload = () => { window.xhrText = x.responseText; window.xhrStatus = x.status; };
            x.open('GET', '/text');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.xhrText").unwrap(),
        "plain-text-body",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(page.eval_to_string("window.xhrStatus").unwrap(), "200");
    // readyState progressed to DONE (4).
    assert_eq!(
        page.eval_to_string("window.states.includes(4)").unwrap(),
        "true"
    );
}

/// `XMLHttpRequest` is a real `EventTarget`: the events it fires are genuine
/// `Event` objects, `dispatchEvent` exists, and `addEventListener` honours the
/// options the hand-rolled registry used to ignore.
#[test]
fn xhr_is_a_real_event_target() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            window.log = [];
            window.probe = null;
            // `once` must fire exactly once; the duplicate registration of the
            // same callback must be deduplicated by `===`; a `handleEvent`
            // object must be accepted as a listener.
            const dup = () => window.log.push('dup');
            x.addEventListener('load', dup);
            x.addEventListener('load', dup);
            x.addEventListener('load', () => window.log.push('once'), { once: true });
            x.addEventListener('load', { handleEvent() { window.log.push('obj'); } });
            x.addEventListener('load', e => {
                window.probe = [
                    e instanceof Event,
                    e.type,
                    e.target === x,
                    e.currentTarget === x,
                    e.isTrusted,
                    typeof e.preventDefault,
                    typeof e.composedPath,
                ].join(',');
            });
            x.onload = () => window.log.push('handler');
            x.open('GET', '/text');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("window.probe").unwrap(),
        "true,load,true,true,true,function,function",
        "the event is a real Event, not a {{type, target}} stand-in; errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(
        page.eval_to_string("window.log.join(',')").unwrap(),
        "dup,once,obj,handler",
        "the duplicate callback is deduplicated by === and `once` fires once"
    );

    // `dispatchEvent` exists and reaches the same listeners.
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            window.synthetic = [];
            x.addEventListener('ping', e => window.synthetic.push(e.type + ':' + (e.target === x)));
            window.dispatchResult = x.dispatchEvent(new Event('ping'));
        })()",
    )
    .unwrap();
    assert_eq!(
        page.eval_to_string("window.synthetic.join(',')").unwrap(),
        "ping:true"
    );
    assert_eq!(
        page.eval_to_string("window.dispatchResult").unwrap(),
        "true"
    );
    assert_eq!(
        page.eval_to_string("new XMLHttpRequest() instanceof EventTarget")
            .unwrap(),
        "true"
    );
}

/// The `onX` handler properties live in the shared registry now, so they must
/// still behave like properties: readable, replaceable, and clearable.
#[test]
fn xhr_handler_properties_round_trip() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            const f = () => {};
            window.initial = x.onload;
            x.onload = f;
            window.readBack = x.onload === f;
            x.onload = null;
            window.cleared = x.onload;
            // Two instances must not share a slot.
            const y = new XMLHttpRequest();
            x.onerror = f;
            window.independent = y.onerror;
        })()",
    )
    .unwrap();
    assert_eq!(page.eval_to_string("window.initial").unwrap(), "null");
    assert_eq!(page.eval_to_string("window.readBack").unwrap(), "true");
    assert_eq!(page.eval_to_string("window.cleared").unwrap(), "null");
    assert_eq!(
        page.eval_to_string("window.independent").unwrap(),
        "null",
        "each XHR has its own handler slot"
    );
}

#[test]
fn set_cookie_from_load_reaches_document_cookie() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    // The document's Set-Cookie header is visible to document.cookie.
    assert_eq!(
        page.eval_to_string("document.cookie").unwrap(),
        "pref=green"
    );
}

#[test]
fn document_charset_falls_back_to_meta_when_no_http_charset() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(&format!("http://127.0.0.1:{port}/latin1"), WaitUntil::Load)
        .unwrap();
    // The `<meta charset=windows-1252>` decodes the 0xE9 byte to 'é'; the old
    // HTTP-charset-only path defaulted to UTF-8 and produced a replacement char.
    assert_eq!(
        page.eval_to_string("document.title").unwrap(),
        "café",
        "errors: {:?}",
        page.drain_errors()
    );
}

#[test]
fn module_load_referrer_is_the_document_not_the_module() {
    let port = spawn_server();
    *DEP_REFERER.lock().unwrap() = None;
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    // The nested `./dep.mjs` import carries the *document* as its referrer, not
    // the importing module URL (the old self-referential behavior). Match on the
    // path only: `DEP_REFERER` is process-global and other tests share it, but
    // every one navigates from `/index.html`, so the buggy `/dep.mjs` referrer
    // would still be caught.
    let referer = DEP_REFERER.lock().unwrap().clone();
    let referer = referer.expect("dep.mjs must have been loaded");
    assert!(
        referer.ends_with("/index.html"),
        "module referrer was `{referer}`, expected the document (…/index.html); errors: {:?}",
        page.drain_errors()
    );
}

#[test]
fn cross_origin_no_cors_response_is_opaque() {
    let port = spawn_server();
    // A second server on another port is a different origin.
    let other = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(&format!(
        "fetch('http://127.0.0.1:{other}/text', {{ mode: 'no-cors' }})\
         .then(r => {{ window.oType = r.type; window.oStatus = r.status; \
             window.oHasCT = r.headers.has('content-type'); return r.text(); }})\
         .then(t => {{ window.oBody = t; }})"
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));
    // An opaque response exposes nothing: type "opaque", status 0, no headers,
    // empty body.
    assert_eq!(page.eval_to_string("window.oType").unwrap(), "opaque");
    assert_eq!(page.eval_to_string("window.oStatus").unwrap(), "0");
    assert_eq!(page.eval_to_string("window.oHasCT").unwrap(), "false");
    assert_eq!(page.eval_to_string("window.oBody").unwrap(), "");
}

/// Regression: `fetch(url, {headers: {...}})` took the plain-object path, which
/// skipped the name/value checks the `Headers` constructor runs. An invalid
/// header surfaced late as a rejected promise from the net layer instead of the
/// synchronous `TypeError` Fetch specifies.
#[test]
fn fetch_init_headers_are_validated_synchronously() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();

    // A CRLF in the value and a space in the name are both invalid.
    for init in [
        r#"{headers: {"x-evil": "a\r\nInjected: 1"}}"#,
        r#"{headers: {"bad name": "v"}}"#,
    ] {
        let result = page
            .eval_to_string(&format!(
                "try {{ fetch('/text', {init}); 'no-throw' }} catch (e) {{ e.name }}"
            ))
            .unwrap();
        assert_eq!(result, "TypeError", "init {init} must throw synchronously");
    }

    // A valid record still works.
    assert_eq!(
        page.eval_to_string(
            r#"try { fetch('/text', {headers: {"x-ok": "1"}}); 'sent' } catch (e) { e.name }"#
        )
        .unwrap(),
        "sent"
    );
    page.settle(Duration::from_secs(5));
}

/// Regression: a script-initiated `fetch` that is still in flight when the page
/// navigates must be abandoned. The realm survives the navigation, so otherwise
/// the old document's `.then()` would run against the new document.
#[test]
fn navigation_abandons_a_scripts_in_flight_fetch() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();

    page.eval_to_string(
        "window.__fetched = 0; fetch('/slow').then(function(){ window.__fetched = 1; }); 'ok'",
    )
    .unwrap();

    // Navigate while `/slow` is still being served.
    page.load_html("<!DOCTYPE html><body>second</body>")
        .unwrap();

    // Give the response more than its 200 ms delay to arrive, then pump the loop
    // so any surviving completion would be delivered.
    std::thread::sleep(Duration::from_millis(400));
    page.run_until_stalled();

    assert_eq!(
        page.eval_to_string("window.__fetched").unwrap(),
        "0",
        "the previous document's fetch must not resolve into the new one"
    );
}

#[test]
fn second_navigation_discards_stale_async_state() {
    let port = spawn_server();
    let page = loopback_page();
    // Stop at DOMContentLoaded so the async subresource may still be in flight.
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::DomContentLoaded,
    )
    .unwrap();
    // A second navigation must abandon the first document's in-flight async
    // script bookkeeping (otherwise `load` could hang on a stale counter).
    page.navigate(
        &format!("http://127.0.0.1:{port}/index2.html"),
        WaitUntil::Load,
    )
    .unwrap();
    assert_eq!(page.eval_to_string("window.doc").unwrap(), "2");
    assert!(page.is_loaded());
}

#[test]
fn fetch_response_body_stream_reads_full_body() {
    // Regression: Angular's fetch backend reads a response through
    // `response.body.getReader()`; without the byte stream every runtime fetch
    // came back empty (ADR-0012).
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();

    page.eval(&format!(
        "window.__streamBody = 'pending';
         fetch('http://127.0.0.1:{port}/text').then(async (r) => {{
             const reader = r.body.getReader();
             const chunks = [];
             for (;;) {{
                 const {{ value, done }} = await reader.read();
                 if (done) break;
                 chunks.push(value);
             }}
             const total = chunks.reduce((a, c) => a + c.length, 0);
             const all = new Uint8Array(total);
             let offset = 0;
             for (const c of chunks) {{ all.set(c, offset); offset += c.length; }}
             window.__streamBody = new TextDecoder().decode(all);
         }});",
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("window.__streamBody").unwrap(),
        "plain-text-body"
    );
}

/// A `FormData` body reaches the wire as `multipart/form-data`, and the
/// `Content-Type` the engine sets carries the *same* boundary that delimits the
/// parts. Nothing else can set that header — the boundary is generated here —
/// which is why jQuery passes `contentType: false` for a FormData body.
#[test]
fn fetch_formdata_body_is_sent_as_multipart() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "const fd = new FormData();\
         fd.append('alpha', 'one');\
         fd.append('beta', 'two');\
         fetch('/echo-with-type', { method: 'POST', body: fd })\
             .then(r => r.text()).then(t => { window.echoed = t; });",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    let echoed = page.eval_to_string("window.echoed").unwrap();
    let (content_type, body) = echoed
        .split_once('\n')
        .expect("echo shape is `type\\nbody`");

    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .expect("multipart content type with a boundary");
    assert!(!boundary.is_empty());
    // The header's boundary must be the one actually delimiting the parts.
    assert!(
        body.contains(&format!("--{boundary}\r\n")),
        "parts must be delimited by the boundary the header names; body was:\n{body}"
    );
    assert!(body.contains("Content-Disposition: form-data; name=\"alpha\""));
    assert!(body.contains("one"));
    assert!(body.contains("Content-Disposition: form-data; name=\"beta\""));
    assert!(body.contains("two"));
    assert!(
        body.ends_with(&format!("--{boundary}--\r\n")),
        "the body must end with the closing boundary"
    );
}

/// An author-set `Content-Type` wins over a body's default one.
#[test]
fn explicit_content_type_beats_the_body_default() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "fetch('/echo-with-type', {\
             method: 'POST',\
             headers: { 'Content-Type': 'application/json' },\
             body: '{\"a\":1}',\
         }).then(r => r.text()).then(t => { window.echoed = t; });",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    let echoed = page.eval_to_string("window.echoed").unwrap();
    assert!(
        echoed.starts_with("application/json\n"),
        "author's Content-Type must survive; got: {echoed}"
    );
}

/// `XMLHttpRequest.send(FormData)` takes the same path as `fetch` — one body
/// extractor, so a FormData cannot reach the wire as `[object FormData]`
/// through the older API.
#[test]
fn xhr_send_formdata_is_multipart() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page.eval(
        "const fd = new FormData();\
         fd.append('k', 'v');\
         const x = new XMLHttpRequest();\
         x.open('POST', '/echo-with-type');\
         x.onload = () => { window.echoed = x.responseText; };\
         x.send(fd);",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));

    let echoed = page.eval_to_string("window.echoed").unwrap();
    assert!(
        echoed.starts_with("multipart/form-data; boundary="),
        "got: {echoed}"
    );
    assert!(echoed.contains("Content-Disposition: form-data; name=\"k\""));
    assert!(echoed.contains('v'));
}

// ===========================================================================
// XMLHttpRequest conformance (ADR-0024)
//
// WPT's `xhr/` suite is not vendored — it is almost entirely server-driven
// (`trickle.py`, `redirect.py`, `auth.py`, `.sub.` substitution) and `xtask`'s
// `TestServer` can parse none of that. The loopback server above is far more
// capable, so verification lives here. See ADR-0024.
// ===========================================================================

/// A loopback page, ready to run XHR script against the fixtures above.
fn xhr_page(port: u16) -> Page {
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/index.html"),
        WaitUntil::Load,
    )
    .unwrap();
    page
}

/// Installs a listener for every XHR event type, logging `type:readyState` into
/// `window.log`. `target` is the expression the listeners attach to.
fn log_all_events(target: &str) -> String {
    format!(
        "['loadstart','progress','abort','error','load','timeout','loadend','readystatechange']\
           .forEach(t => {target}.addEventListener(t, () => window.log.push(t + ':' + x.readyState)));"
    )
}

/// The successful sequence, in order: `loadstart` at `send()`, a
/// `readystatechange` per state, one `progress` per body chunk, then `load` and
/// `loadend` last.
#[test]
fn xhr_success_event_sequence() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(&format!(
        "(() => {{
            const x = new XMLHttpRequest();
            window.log = [];
            {}
            x.open('GET', '/text');
            x.send();
        }})()",
        log_all_events("x")
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.log.join(',')").unwrap(),
        "readystatechange:1,loadstart:1,readystatechange:2,readystatechange:3,progress:3,\
         readystatechange:4,load:4,loadend:4",
        "errors: {:?}",
        page.drain_errors()
    );
}

/// A network error: exactly one of the four terminal events fires, `loadend`
/// follows it, and the response is a network error (status 0, empty body).
#[test]
fn xhr_error_event_sequence_and_network_error_response() {
    let port = spawn_server();
    let page = xhr_page(port);
    // Port 1 refuses the connection, which is a network error rather than a
    // failing HTTP status.
    page.eval(&format!(
        "(() => {{
            const x = new XMLHttpRequest();
            window.log = [];
            {}
            x.addEventListener('error', () => {{
                window.errState = [x.status, x.statusText, x.responseText, x.getAllResponseHeaders()]
                    .join('|');
            }});
            x.open('GET', 'http://127.0.0.1:1/nothing');
            x.send();
        }})()",
        log_all_events("x")
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.log.join(',')").unwrap(),
        "readystatechange:1,loadstart:1,readystatechange:4,error:4,loadend:4",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(page.eval_to_string("window.errState").unwrap(), "0|||");
}

/// A failing HTTP *status* is not a network error: `load` fires, and the status
/// and body are readable.
#[test]
fn xhr_http_error_status_still_fires_load() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(&format!(
        "(() => {{
            const x = new XMLHttpRequest();
            window.log = [];
            {}
            x.addEventListener('loadend', () => {{
                window.result = x.status + '|' + x.statusText + '|' + x.responseText;
            }});
            x.open('GET', '/xhr-500');
            x.send();
        }})()",
        log_all_events("x")
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.result").unwrap(),
        "500|Internal Server Error|boom",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(
        page.eval_to_string("window.log.includes('load:4')")
            .unwrap(),
        "true"
    );
    assert_eq!(
        page.eval_to_string("window.log.includes('error:4')")
            .unwrap(),
        "false"
    );
}

/// `abort()` on an in-flight request fires the abort sequence, resets the
/// response, and leaves `readyState` at UNSENT afterwards. `abort()` on an XHR
/// that was never sent fires **nothing** — it used to fire a full sequence.
#[test]
fn xhr_abort_resets_the_response_and_only_fires_when_in_flight() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(&format!(
        "(() => {{
            const x = new XMLHttpRequest();
            window.log = [];
            {}
            x.addEventListener('abort', () => {{
                window.aborted = [x.readyState, x.status, x.responseText].join('|');
            }});
            x.open('GET', '/delay/400');
            x.send();
            x.abort();
            window.afterAbort = x.readyState;

            // A fresh, never-sent XHR: abort() is a no-op.
            const y = new XMLHttpRequest();
            window.quiet = [];
            ['abort','loadend','readystatechange'].forEach(
                t => y.addEventListener(t, () => window.quiet.push(t)));
            y.abort();
            y.open('GET', '/text');
            window.quietAfterOpen = window.quiet.join(',');
        }})()",
        log_all_events("x")
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.log.join(',')").unwrap(),
        "readystatechange:1,loadstart:1,readystatechange:4,abort:4,loadend:4",
        "errors: {:?}",
        page.drain_errors()
    );
    // The response is a network error by the time the `abort` listener runs.
    assert_eq!(page.eval_to_string("window.aborted").unwrap(), "4|0|");
    assert_eq!(page.eval_to_string("window.afterAbort").unwrap(), "0");
    // The never-sent XHR fired nothing for `abort()`; only `open()` spoke.
    assert_eq!(
        page.eval_to_string("window.quietAfterOpen").unwrap(),
        "readystatechange"
    );
}

/// **The reuse bug.** Every terminal transition releases the XHR's self-root;
/// `open()` has to put it back, or a second `send()` on the same object
/// delivers zero events.
#[test]
fn xhr_reused_fires_events_on_its_second_request() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            window.bodies = [];
            x.addEventListener('load', () => {
                window.bodies.push(x.responseText);
                if (window.bodies.length === 1) {
                    // Reopened from inside the terminating sequence: the root
                    // this request installs must survive the `loadend` that
                    // follows.
                    x.open('GET', '/json');
                    x.send();
                }
            });
            x.open('GET', '/text');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.bodies.join('|')").unwrap(),
        "plain-text-body|{\"msg\":\"hi\",\"n\":42}",
        "a reused XHR must fire events on its second request; errors: {:?}",
        page.drain_errors()
    );

    // The same thing across two separate tasks, which is the ordinary pattern.
    page.eval(
        "(() => {
            window.second = [];
            const x = new XMLHttpRequest();
            x.addEventListener('load', () => window.second.push(x.responseText));
            x.open('GET', '/text');
            x.send();
            window.reuse = x;
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    page.eval("window.reuse.open('GET', '/json'); window.reuse.send();")
        .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.second.length").unwrap(),
        "2",
        "errors: {:?}",
        page.drain_errors()
    );
}

/// `open()` terminates an in-flight request: the old one must not keep writing
/// into the reopened object.
#[test]
fn xhr_open_terminates_an_in_flight_request() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            window.loads = [];
            x.addEventListener('load', () => window.loads.push(x.responseText));
            x.open('GET', '/delay/400');
            x.send();
            // Reopened before the first response can arrive.
            x.open('GET', '/text');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.loads.join('|')").unwrap(),
        "plain-text-body",
        "the terminated request must deliver nothing; errors: {:?}",
        page.drain_errors()
    );
}

/// `send()` twice is an `InvalidStateError` (a `DOMException`, not a bare
/// `TypeError`), and so is `send()` before `open()`.
#[test]
fn xhr_send_state_errors() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const name = fn => { try { fn(); return 'no-throw'; }
                                 catch (e) { return e instanceof DOMException ? e.name : 'TypeError'; } };
            const x = new XMLHttpRequest();
            window.beforeOpen = name(() => x.send());
            x.open('GET', '/text');
            x.send();
            window.twice = name(() => x.send());
            window.headerAfterSend = name(() => x.setRequestHeader('X-Late', '1'));
            const y = new XMLHttpRequest();
            window.headerBeforeOpen = name(() => y.setRequestHeader('X-Early', '1'));
            window.syncMode = name(() => y.open('GET', '/text', false));
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.beforeOpen").unwrap(),
        "InvalidStateError"
    );
    assert_eq!(
        page.eval_to_string("window.twice").unwrap(),
        "InvalidStateError"
    );
    assert_eq!(
        page.eval_to_string("window.headerAfterSend").unwrap(),
        "InvalidStateError"
    );
    assert_eq!(
        page.eval_to_string("window.headerBeforeOpen").unwrap(),
        "InvalidStateError"
    );
    // No synchronous mode: it would block the page thread under live DOM
    // borrows, so it is refused rather than approximated (ADR-0024).
    assert_eq!(
        page.eval_to_string("window.syncMode").unwrap(),
        "InvalidAccessError"
    );
}

/// `ProgressEvent` is a real interface with real members, and `total` /
/// `lengthComputable` come from `Content-Length` — never from the bytes that
/// happen to have arrived.
#[test]
fn xhr_progress_event_members() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            x.addEventListener('progress', e => {
                window.progress = [
                    e instanceof ProgressEvent,
                    e instanceof Event,
                    e.type,
                    e.lengthComputable,
                    e.loaded,
                    e.total,
                    e.target === x,
                ].join(',');
            });
            x.addEventListener('loadstart', e => {
                window.start = [e.lengthComputable, e.loaded, e.total].join(',');
            });
            x.open('GET', '/text');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.progress").unwrap(),
        "true,true,progress,true,15,15,true",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(page.eval_to_string("window.start").unwrap(), "false,0,0");
    // Constructible, with the declared defaults.
    assert_eq!(
        page.eval_to_string(
            "(() => { const e = new ProgressEvent('p');
                      const f = new ProgressEvent('q', { lengthComputable: true, loaded: 5, total: 9 });
                      return [e.lengthComputable, e.loaded, e.total, e.bubbles,
                              f.lengthComputable, f.loaded, f.total].join(','); })()"
        )
        .unwrap(),
        "false,0,0,false,true,5,9"
    );
}

/// A chunked response with no `Content-Length`: `readyState` reaches LOADING,
/// progress is reported per chunk, and `total` is **not** invented.
///
/// The net layer buffers the whole body and emits one `Chunk` today (ADR-0004),
/// so the loop runs once — the assertions are written against the chunk stream
/// so they stay true when it learns to stream.
#[test]
fn xhr_chunked_response_reaches_loading_state() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            window.states = [];
            window.progressEvents = [];
            x.addEventListener('readystatechange', () => window.states.push(x.readyState));
            x.addEventListener('progress', e => {
                window.progressEvents.push([e.lengthComputable, e.loaded, e.total].join(':'));
            });
            x.addEventListener('load', () => { window.body = x.responseText; });
            x.open('GET', '/chunked/3');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.body").unwrap(),
        "chunk0;chunk1;chunk2;",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(
        page.eval_to_string("window.states.join(',')").unwrap(),
        "1,2,3,4",
        "readyState must reach LOADING (3)"
    );
    // A chunked response carries no `Content-Length`, so nothing is computable.
    assert_eq!(
        page.eval_to_string("window.progressEvents[0]").unwrap(),
        "false:21:0"
    );
    assert_eq!(
        page.eval_to_string(
            "window.progressEvents.length ===  window.states.filter(s => s === 3).length"
        )
        .unwrap(),
        "true",
        "one progress event per LOADING transition"
    );
}

/// The upload object is a second event target with its own handlers, and it
/// fires its own `loadstart`/`progress`/`load`/`loadend` when there is a body.
#[test]
fn xhr_upload_events() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            window.up = [];
            window.down = [];
            ['loadstart','progress','load','loadend'].forEach(t => {
                x.upload.addEventListener(t, e => window.up.push(t + ':' + (e.target === x.upload)));
                x.addEventListener(t, () => window.down.push(t));
            });
            window.sameObject = x.upload === x.upload;
            window.uploadIsTarget = x.upload instanceof XMLHttpRequestEventTarget
                && x.upload instanceof EventTarget
                && x instanceof XMLHttpRequestEventTarget;
            x.open('POST', '/echo');
            x.send('payload');
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(page.eval_to_string("window.sameObject").unwrap(), "true");
    assert_eq!(
        page.eval_to_string("window.uploadIsTarget").unwrap(),
        "true"
    );
    assert_eq!(
        page.eval_to_string("window.up.join(',')").unwrap(),
        "loadstart:true,progress:true,load:true,loadend:true",
        "errors: {:?}",
        page.drain_errors()
    );
    // The upload's `load` must precede the download's.
    assert_eq!(
        page.eval_to_string("window.down.join(',')").unwrap(),
        "loadstart,progress,load,loadend"
    );

    // With no request body the upload object stays silent.
    page.eval(
        "(() => {
            const y = new XMLHttpRequest();
            window.noBody = [];
            ['loadstart','progress','load','loadend'].forEach(
                t => y.upload.addEventListener(t, () => window.noBody.push(t)));
            y.open('GET', '/text');
            y.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(page.eval_to_string("window.noBody.length").unwrap(), "0");
}

/// `timeout` fires the timeout sequence, and clearing it before it elapses lets
/// the request finish normally.
#[test]
fn xhr_timeout_fires_and_is_cancellable() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(&format!(
        "(() => {{
            const x = new XMLHttpRequest();
            window.log = [];
            {}
            x.addEventListener('timeout', () => {{
                window.timedOut = [x.readyState, x.status, x.responseText].join('|');
            }});
            x.timeout = 100;
            x.open('GET', '/delay/900');
            x.send();
        }})()",
        log_all_events("x")
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.log.join(',')").unwrap(),
        "readystatechange:1,loadstart:1,readystatechange:4,timeout:4,loadend:4",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(page.eval_to_string("window.timedOut").unwrap(), "4|0|");

    // Cleared mid-flight: the request completes.
    page.eval(
        "(() => {
            const y = new XMLHttpRequest();
            window.cancelled = [];
            ['timeout','load'].forEach(t => y.addEventListener(t, () => window.cancelled.push(t)));
            y.timeout = 100;
            y.open('GET', '/delay/300');
            y.send();
            y.timeout = 0;
            window.timeoutValue = y.timeout;
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(page.eval_to_string("window.timeoutValue").unwrap(), "0");
    assert_eq!(
        page.eval_to_string("window.cancelled.join(',')").unwrap(),
        "load",
        "clearing `timeout` mid-flight must disarm it; errors: {:?}",
        page.drain_errors()
    );
}

/// Every supported `responseType`, plus the two getters that throw for the
/// wrong one and the `"blob"` value this engine deliberately does not have.
#[test]
fn xhr_response_types() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "window.get = (url, type) => new Promise(resolve => {
             const x = new XMLHttpRequest();
             x.open('GET', url);
             if (type !== undefined) x.responseType = type;
             x.addEventListener('load', () => resolve(x));
             x.send();
         });",
    )
    .unwrap();

    // json
    page.eval("window.get('/json', 'json').then(x => { window.json = x.response.msg + ':' + x.response.n; window.jsonSame = x.response === x.response; });").unwrap();
    // arraybuffer
    page.eval(
        "window.get('/text', 'arraybuffer').then(x => {
             const v = new Uint8Array(x.response);
             window.ab = [x.response instanceof ArrayBuffer, v.length, v[0]].join(',');
         });",
    )
    .unwrap();
    // document
    page.eval(
        "window.get('/xhr-html', 'document').then(x => {
             window.doc = [
                 x.response.querySelector('#p').textContent,
                 x.response.title,
                 x.response === x.responseXML,
             ].join(',');
         });",
    )
    .unwrap();
    // responseXML with the default responseType, on an XML document.
    page.eval(
        "window.get('/xhr-xml').then(x => {
             window.xml = [
                 x.responseXML !== null,
                 x.responseXML.documentElement.textContent,
                 x.responseText,
             ].join('|');
         });",
    )
    .unwrap();
    // responseXML is null for a MIME type that is neither HTML nor XML.
    page.eval("window.get('/text').then(x => { window.plainXml = x.responseXML; });")
        .unwrap();
    page.settle(Duration::from_secs(10));

    assert_eq!(
        page.eval_to_string("window.json").unwrap(),
        "hi:42",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(page.eval_to_string("window.jsonSame").unwrap(), "true");
    assert_eq!(page.eval_to_string("window.ab").unwrap(), "true,15,112");
    assert_eq!(
        page.eval_to_string("window.doc").unwrap(),
        "hello,doc-title,true"
    );
    assert_eq!(
        page.eval_to_string("window.xml").unwrap(),
        "true|xml-text|<root><item>xml-text</item></root>"
    );
    assert_eq!(page.eval_to_string("window.plainXml").unwrap(), "null");

    // The enumerated setter, `"blob"` included, and the two throwing getters.
    page.eval(
        "(() => {
            const name = fn => { try { fn(); return 'no-throw'; }
                                 catch (e) { return e instanceof DOMException ? e.name : 'TypeError'; } };
            const x = new XMLHttpRequest();
            x.responseType = 'json';
            x.responseType = 'nonsense';   // outside the enumeration: ignored
            window.afterNonsense = x.responseType;
            x.responseType = 'blob';       // unsupported: leaves the previous value
            window.afterBlob = x.responseType;
            window.textThrows = name(() => x.responseText);
            window.xmlThrows = name(() => x.responseXML);
            // Before LOADING, `response` is the empty string for a text type.
            const y = new XMLHttpRequest();
            window.beforeSend = JSON.stringify(y.response);
            y.responseType = 'json';
            window.beforeSendJson = String(y.response);
            y.open('GET', '/delay/300');
            y.send();
            window.responseTypeInFlight = name(() => { y.responseType = 'text'; });
        })()",
    )
    .unwrap();
    assert_eq!(page.eval_to_string("window.afterNonsense").unwrap(), "json");
    assert_eq!(page.eval_to_string("window.afterBlob").unwrap(), "json");
    assert_eq!(
        page.eval_to_string("window.textThrows").unwrap(),
        "InvalidStateError"
    );
    assert_eq!(
        page.eval_to_string("window.xmlThrows").unwrap(),
        "InvalidStateError"
    );
    assert_eq!(page.eval_to_string("window.beforeSend").unwrap(), "\"\"");
    assert_eq!(
        page.eval_to_string("window.beforeSendJson").unwrap(),
        "null"
    );
    // OPENED is still early enough to set `responseType`.
    assert_eq!(
        page.eval_to_string("window.responseTypeInFlight").unwrap(),
        "no-throw"
    );
    page.settle(Duration::from_secs(5));
}

/// `overrideMimeType` changes the charset `responseText` decodes with — the old
/// code read every response as lossy UTF-8.
#[test]
fn xhr_override_mime_type_changes_the_decoded_charset() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            window.results = {};
            const plain = new XMLHttpRequest();
            plain.addEventListener('load', () => { window.results.utf8 = plain.responseText; });
            plain.open('GET', '/xhr-latin1');
            plain.send();

            const over = new XMLHttpRequest();
            over.addEventListener('load', () => { window.results.latin1 = over.responseText; });
            over.open('GET', '/xhr-latin1');
            over.overrideMimeType('text/plain; charset=windows-1252');
            over.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    // Without the override the 0xE9 byte is not valid UTF-8 and decodes to the
    // replacement character; with it, the text is right.
    assert_eq!(
        page.eval_to_string("window.results.latin1").unwrap(),
        "café",
        "errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(
        page.eval_to_string("window.results.utf8 === 'café'")
            .unwrap(),
        "false"
    );
    // `overrideMimeType` is refused once the response is being delivered.
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            x.addEventListener('load', () => {
                try { x.overrideMimeType('text/plain'); window.lateOverride = 'no-throw'; }
                catch (e) { window.lateOverride = e.name; }
            });
            x.open('GET', '/text');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.lateOverride").unwrap(),
        "InvalidStateError"
    );
}

/// `responseURL` is the final post-redirect URL. It used to be discarded.
#[test]
fn xhr_response_url_after_a_redirect() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            x.addEventListener('load', () => { window.finalUrl = x.responseURL; });
            x.open('GET', '/xhr-redirect#frag');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.finalUrl").unwrap(),
        format!("http://127.0.0.1:{port}/text"),
        "errors: {:?}",
        page.drain_errors()
    );
}

/// **`Set-Cookie` must not be readable from script.** The net layer forwards
/// the whole header map for a same-origin (`basic`) response, so the
/// forbidden-response-header filter has to live in XHR.
///
/// The same test covers `getAllResponseHeaders` sorting and combining, which
/// reuse `HeadersData::sorted_combined`.
#[test]
fn xhr_response_headers_hide_set_cookie_and_are_sorted_and_combined() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const c = new XMLHttpRequest();
            c.addEventListener('load', () => {
                window.cookieHeader = String(c.getResponseHeader('Set-Cookie'));
                window.cookieHeader2 = String(c.getResponseHeader('set-cookie'));
                window.allHeaders = c.getAllResponseHeaders().toLowerCase();
                window.visible = c.getResponseHeader('x-visible');
            });
            c.open('GET', '/xhr-cookie');
            c.send();

            const h = new XMLHttpRequest();
            h.addEventListener('load', () => { window.headerBlock = h.getAllResponseHeaders(); });
            h.open('GET', '/xhr-headers');
            h.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.cookieHeader").unwrap(),
        "null",
        "Set-Cookie must not be readable; errors: {:?}",
        page.drain_errors()
    );
    assert_eq!(page.eval_to_string("window.cookieHeader2").unwrap(), "null");
    assert_eq!(
        page.eval_to_string("window.allHeaders.includes('set-cookie')")
            .unwrap(),
        "false"
    );
    // A non-forbidden header on the same response is still readable, so the
    // filter is a filter and not a blanket.
    assert_eq!(page.eval_to_string("window.visible").unwrap(), "yes");

    let block = page.eval_to_string("window.headerBlock").unwrap();
    assert!(
        block.contains("x-alpha: a1, a2\r\n"),
        "duplicate names must be combined with `, `; got:\n{block}"
    );
    let alpha = block.find("x-alpha").expect("x-alpha present");
    let zebra = block.find("x-zebra").expect("x-zebra present");
    assert!(
        alpha < zebra,
        "headers must be sorted by name; got:\n{block}"
    );
    assert!(
        block.ends_with("\r\n"),
        "each field ends with CRLF; got:\n{block}"
    );
}

/// `setRequestHeader` combines a repeated name with `, ` and silently ignores a
/// forbidden one.
#[test]
fn xhr_set_request_header_combines_and_ignores_forbidden_names() {
    let port = spawn_server();
    let page = xhr_page(port);
    page.eval(
        "(() => {
            const x = new XMLHttpRequest();
            x.addEventListener('load', () => { window.echoed = x.responseText; });
            x.open('GET', '/xhr-echo-header');
            x.setRequestHeader('X-Combined', 'a');
            x.setRequestHeader('x-combined', 'b');
            // Forbidden: silently ignored, not an error — feature-detecting code
            // sets these and carries on.
            x.setRequestHeader('Referer', 'http://evil.test/');
            x.setRequestHeader('Host', 'evil.test');
            x.send();
        })()",
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("window.echoed").unwrap(),
        "a, b|true",
        "the repeated name combines and the forbidden one never reached the wire; errors: {:?}",
        page.drain_errors()
    );
}
