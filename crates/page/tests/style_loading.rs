//! Phase 4 style-loading tests: inline `<style>` and external
//! `<link rel=stylesheet>` load into the engine, document order is respected,
//! `@import` from a linked sheet resolves, and a broken link does not hang the
//! load.

use oxidepage_dom::{DomTree, NodeId};
use oxidepage_page::{Page, PageOptions, ResourcePolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
                    loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let _ = sock.write_all(&route(&path)).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    rx.recv().unwrap()
}

fn route(path: &str) -> Vec<u8> {
    let css = "text/css";
    match path {
        "/a.css" => resp(200, "OK", css, "div { color: rgb(1, 2, 3) }"),
        "/b.css" => resp(200, "OK", css, "div { color: rgb(4, 5, 6) }"),
        "/imports.css" => resp(200, "OK", css, "@import url('/a.css');"),
        // A 404 whose error-page body happens to be valid CSS: the status must
        // win, so this never applies as an author style.
        "/broken.css" => resp(404, "Not Found", css, "div { color: rgb(200, 0, 0) }"),
        _ => resp(404, "Not Found", "text/plain", "nope"),
    }
}

fn resp(status: u16, reason: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn loopback_page() -> Page {
    Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap()
}

fn find(dom: &DomTree, local: &str) -> NodeId {
    dom.inclusive_descendants(dom.document())
        .find(|&id| {
            dom.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .unwrap_or_else(|| panic!("no <{local}>"))
}

#[test]
fn inline_style_element_applies() {
    let page = oxidepage_page::load_html_page(
        "<style>div { color: rgb(9, 9, 9) }</style><div>x</div>",
        PageOptions::default(),
    )
    .unwrap();
    let div = find(&page.dom(), "div");
    assert_eq!(
        page.computed_style_value(div, "color").as_deref(),
        Some("rgb(9, 9, 9)")
    );
}

/// A `<style>` or `<link>` queued as a `StyleUpdate` and then freed by a
/// subtree replacement leaves a stale id on the queue. The drain must
/// revalidate it, not panic.
///
/// The mutation runs *after* parsing on purpose: the parser holds off
/// `free_detached_tree_if_unpinned`, so a detach during parsing only
/// disconnects the node, while one after it frees the id outright.
#[test]
fn style_elements_freed_before_drain_are_not_fatal() {
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html("<div id=c></div><div>x</div>").unwrap();
    page.eval_to_string(&format!(
        "const c = document.getElementById('c');\
         c.innerHTML = '<style>div {{ color: rgb(7, 7, 7) }}</style>' +\
                       '<link rel=stylesheet href=\"http://127.0.0.1:{port}/a.css\">';\
         c.innerHTML = '';"
    ))
    .unwrap();
    page.settle(std::time::Duration::from_secs(5));
    // Neither freed sheet applies, and the page is still alive.
    let div = find(&page.dom(), "div");
    assert_eq!(
        page.computed_style_value(div, "color").as_deref(),
        Some("rgb(0, 0, 0)")
    );
}

#[test]
fn external_link_stylesheet_loads() {
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html(&format!(
        "<link rel=stylesheet href='http://127.0.0.1:{port}/a.css'><div>x</div>"
    ))
    .unwrap();
    let div = find(&page.dom(), "div");
    assert_eq!(
        page.computed_style_value(div, "color").as_deref(),
        Some("rgb(1, 2, 3)")
    );
}

#[test]
fn non_success_stylesheet_response_is_not_applied() {
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html(&format!(
        "<link rel=stylesheet href='http://127.0.0.1:{port}/broken.css'><div>x</div>"
    ))
    .unwrap();
    let div = find(&page.dom(), "div");
    assert_ne!(
        page.computed_style_value(div, "color").as_deref(),
        Some("rgb(200, 0, 0)"),
        "a 404 error-page body must not be applied as CSS"
    );
}

#[test]
fn a_link_stylesheet_fires_load_when_its_sheet_arrives() {
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html(&format!(
        "<script>\
           globalThis.fired = [];\
           const l = document.createElement('link');\
           l.rel = 'stylesheet';\
           l.href = 'http://127.0.0.1:{port}/a.css';\
           l.onload = () => globalThis.fired.push('load');\
           l.onerror = () => globalThis.fired.push('error');\
           document.head.appendChild(l);\
         </script>"
    ))
    .unwrap();
    page.settle(std::time::Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("globalThis.fired.join(',')").unwrap(),
        "load"
    );
}

#[test]
fn a_link_stylesheet_fires_error_when_its_fetch_fails() {
    // A sheet that 404s must fire `error` at the <link>. It used to fire nothing
    // at all — the failure was only reported to the host — so a page (or a WPT
    // test) waiting on `link.onerror` waited forever.
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html(&format!(
        "<script>\
           globalThis.fired = [];\
           const l = document.createElement('link');\
           l.rel = 'stylesheet';\
           l.href = 'http://127.0.0.1:{port}/broken.css';\
           l.onload = () => globalThis.fired.push('load');\
           l.onerror = () => globalThis.fired.push('error');\
           document.head.appendChild(l);\
         </script>"
    ))
    .unwrap();
    page.settle(std::time::Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("globalThis.fired.join(',')").unwrap(),
        "error"
    );
}

#[test]
fn later_stylesheet_wins_in_document_order() {
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html(&format!(
        "<link rel=stylesheet href='http://127.0.0.1:{port}/a.css'>\
         <link rel=stylesheet href='http://127.0.0.1:{port}/b.css'><div>x</div>"
    ))
    .unwrap();
    let div = find(&page.dom(), "div");
    // b.css comes second, so its color wins.
    assert_eq!(
        page.computed_style_value(div, "color").as_deref(),
        Some("rgb(4, 5, 6)")
    );
}

#[test]
fn import_from_linked_sheet_loads() {
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html(&format!(
        "<link rel=stylesheet href='http://127.0.0.1:{port}/imports.css'><div>x</div>"
    ))
    .unwrap();
    let div = find(&page.dom(), "div");
    assert_eq!(
        page.computed_style_value(div, "color").as_deref(),
        Some("rgb(1, 2, 3)")
    );
}

/// A `<link media="print" onload="this.media='all'">` (the common non-blocking
/// CSS pattern) must obtain its sheet exactly once: the `onload` media change
/// re-evaluates applicability from the cached bytes, it does not re-fetch. Left
/// un-deduplicated, each re-fetch re-fires `load`, whose handler re-sets `media`,
/// which re-fetches — a loop that exhausted the per-page request budget on
/// angular.dev and blocked every later resource.
#[test]
fn media_toggle_link_stylesheet_does_not_refetch_in_a_loop() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let hits = Arc::new(AtomicUsize::new(0));
    let port = spawn_counting_server(Arc::clone(&hits));
    let mut page = loopback_page();
    page.load_html(&format!(
        "<link rel=stylesheet media=print onload=\"this.media='all'\" \
         href='http://127.0.0.1:{port}/loop.css'><div>x</div>"
    ))
    .unwrap();
    page.settle(std::time::Duration::from_secs(5));

    let fetches = hits.load(Ordering::Relaxed);
    assert!(
        fetches <= 2,
        "stylesheet fetched {fetches} times — a `media` toggle must not re-fetch in a loop"
    );
    // The `onload` media='all' toggle must still apply the sheet to the screen
    // (re-evaluated from cache, not dropped).
    let div = find(&page.dom(), "div");
    assert_eq!(
        page.computed_style_value(div, "color").as_deref(),
        Some("rgb(1, 2, 3)")
    );
}

/// Like [`spawn_server`] but counts requests to `/loop.css` in `hits`, so a
/// re-fetch loop is observable.
fn spawn_counting_server(hits: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> u16 {
    use std::sync::atomic::Ordering;
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
                let hits = std::sync::Arc::clone(&hits);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    if path == "/loop.css" {
                        hits.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = sock
                        .write_all(&resp(200, "OK", "text/css", "div { color: rgb(1, 2, 3) }"))
                        .await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    rx.recv().unwrap()
}

#[test]
fn broken_link_does_not_hang_load() {
    let port = spawn_server();
    let mut page = loopback_page();
    page.load_html(&format!(
        "<link rel=stylesheet href='http://127.0.0.1:{port}/missing.css'><div>x</div>"
    ))
    .unwrap();
    assert!(page.is_loaded(), "load fired despite a 404 stylesheet");
    // The document is still usable; the div keeps its UA display.
    let div = find(&page.dom(), "div");
    assert_eq!(
        page.computed_style_value(div, "display").as_deref(),
        Some("block")
    );
}
