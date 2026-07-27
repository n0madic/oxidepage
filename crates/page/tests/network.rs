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
                    // while the request is still in flight.
                    if path == "/slow" || path == "/ordered-first.js" {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    let _ = sock.write_all(&route(&method, &path, &body, &head)).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    rx.recv().unwrap()
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
