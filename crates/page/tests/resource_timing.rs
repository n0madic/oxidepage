//! Resource Timing (ADR-0044): `performance.getEntriesByType('resource')`
//! reports the document's completed subresource fetches, with detailed
//! timings and sizes only for same-origin or timing-allowed responses.

use std::time::Duration;

use oxidepage_page::{Page, PageOptions, ResourcePolicy, WaitUntil};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback server answering every path through `route`.
fn spawn_server(route: fn(&str, u16) -> Vec<u8>, peer: u16) -> u16 {
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
                    let _ = sock.write_all(&route(&path, peer)).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    rx.recv().unwrap()
}

fn resp(content_type: &str, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The other origin: one plain resource, one that opts in to timing.
fn other_origin(path: &str, _peer: u16) -> Vec<u8> {
    match path {
        "/allowed.css" => resp("text/css", "Timing-Allow-Origin: *\r\n", "p{}"),
        _ => resp("text/css", "", "p{}"),
    }
}

fn this_origin(path: &str, peer: u16) -> Vec<u8> {
    match path {
        "/" => resp(
            "text/html",
            "",
            &format!(
                "<!doctype html><link rel=stylesheet href=/own.css>\
                 <link rel=stylesheet href=http://127.0.0.1:{peer}/plain.css>\
                 <link rel=stylesheet href=http://127.0.0.1:{peer}/allowed.css>\
                 <link rel=stylesheet href='data:text/css,p{{}}'>"
            ),
        ),
        "/data.json" => resp("application/json", "", "{\"a\":1}"),
        "/framed" => resp(
            "text/html",
            "",
            "<!doctype html><script src=/frame.js></script>\
             <style>@import url(/imported.css);</style>",
        ),
        "/frame.js" => resp("text/javascript", "", "window.ran = true;"),
        "/top-import" => resp(
            "text/html",
            "",
            "<!doctype html><style>@import url(/imported.css);</style>",
        ),
        "/with-frame" => resp(
            "text/html",
            "",
            "<!doctype html><iframe src=/framed></iframe>",
        ),
        _ => resp("text/css", "", "div { color: red }"),
    }
}

fn page_at(port: u16) -> Page {
    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(&format!("http://127.0.0.1:{port}/"), WaitUntil::Load)
        .unwrap();
    page.settle(Duration::from_secs(5));
    page
}

#[test]
fn subresources_are_reported_with_cross_origin_detail_withheld() {
    let other = spawn_server(other_origin, 0);
    let port = spawn_server(this_origin, other);
    let page = page_at(port);
    let row = |entry: &str| {
        format!(
            "(e => e ? [e.initiatorType, e.responseStatus, e.encodedBodySize > 0,\
              e.transferSize >= e.encodedBodySize, e.responseStart > 0, e.requestStart > 0,\
              e.contentType, e.responseEnd >= e.fetchStart,\
              e instanceof PerformanceResourceTiming].join(',') : 'missing')({entry})"
        )
    };
    let by = |path: &str| {
        format!("performance.getEntriesByType('resource').find(e => e.name.endsWith('{path}'))")
    };
    let read = |expr: String| page.eval_to_string(&expr).unwrap();

    eprintln!(
        "DEBUG {}",
        read(
            "location.href + ' ' + JSON.stringify(performance.getEntriesByType('resource')[0])"
                .into()
        )
    );
    assert_eq!(
        read(row(&by("/own.css"))),
        "link,200,true,true,true,true,text/css,true,true"
    );
    // `Timing-Allow-Origin: *` opens the phases and sizes. Status and type
    // follow CORS instead, and a `no-cors` stylesheet is opaque.
    assert_eq!(
        read(row(&by("/allowed.css"))),
        "link,0,true,true,true,true,,true,true"
    );
    // No `Timing-Allow-Origin`: the phases and sizes read 0 as well.
    assert_eq!(
        read(row(&by("/plain.css"))),
        "link,0,false,true,false,false,,true,true"
    );
    // Neither the document itself nor a `data:` URL is a resource entry.
    assert_eq!(
        read("performance.getEntriesByType('resource').map(e => e.name.split('/').pop()).sort().join()".into()),
        "allowed.css,own.css,plain.css"
    );
}

#[test]
fn fetch_entries_arrive_and_the_buffer_obeys_its_controls() {
    let port = spawn_server(this_origin, 0);
    let page = page_at(port);
    page.eval_to_string("fetch('/data.json').then(r => r.text()); ''")
        .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string(
            "const list = () => performance.getEntriesByType('resource');\
             const f = list().find(e => e.name.endsWith('/data.json'));\
             const out = [f.initiatorType, list().length,\
               performance.getEntries().includes(f) || performance.getEntries().some(e => e.name === f.name),\
               performance.getEntriesByName(f.name, 'resource').length];\
             performance.clearResourceTimings(); out.push(list().length);\
             performance.setResourceTimingBufferSize(0); out.join(',')"
        )
        .unwrap(),
        "fetch,2,true,1,0"
    );
    // A full buffer drops what arrives after it filled.
    page.eval_to_string("fetch('/data.json'); ''").unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string("String(performance.getEntriesByType('resource').length)")
            .unwrap(),
        "0"
    );
}

#[test]
fn a_frames_fetches_are_its_own_and_not_its_parents() {
    // The frame's parser-blocking script and its `@import` are fetched on the
    // blocking path; each must land in the frame's timeline only.
    let port = spawn_server(this_origin, 0);
    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(
        &format!("http://127.0.0.1:{port}/with-frame"),
        WaitUntil::Load,
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    let names = "performance.getEntriesByType('resource')\
                 .map(e => e.name.split('/').pop()).sort().join()";
    assert_eq!(page.eval_to_string(names).unwrap(), "");
    let tree = page.frame_tree();
    let child = tree[1].context_id.expect("the frame has a realm");
    let result = page
        .evaluate_in(
            Some(child),
            names,
            &oxidepage_page::EvaluateOptions::default(),
        )
        .expect("the context exists")
        .expect_done();
    assert_eq!(
        result
            .result
            .value_json
            .unwrap_or_default()
            .trim_matches('"'),
        "frame.js,imported.css"
    );
}

#[test]
fn the_buffer_size_is_an_unsigned_long_and_entries_are_branded() {
    let port = spawn_server(this_origin, 0);
    let page = page_at(port);
    assert_eq!(
        page.eval_to_string(
            "const kind = f => { try { f(); return 'ok'; } catch (e) { return e.name; } };\
             performance.setResourceTimingBufferSize(-1);\
             const keeps = performance.getEntriesByType('resource').length > 0;\
             const get = Object.getOwnPropertyDescriptor(\
               PerformanceResourceTiming.prototype, 'initiatorType').get;\
             [keeps, kind(() => get.call(performance.mark('m'))),\
              kind(() => performance.mark()), kind(() => performance.measure())].join()"
        )
        .unwrap(),
        "true,TypeError,TypeError,TypeError"
    );
}

#[test]
fn an_inline_style_import_is_fetched_once() {
    // A `<style>` queues one update on insertion and one when its text
    // arrives; parsing it twice fetched its `@import` twice.
    let port = spawn_server(this_origin, 0);
    let page = Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap();
    page.navigate(
        &format!("http://127.0.0.1:{port}/top-import"),
        WaitUntil::Load,
    )
    .unwrap();
    page.settle(Duration::from_secs(5));
    assert_eq!(
        page.eval_to_string(
            "performance.getEntriesByType('resource').map(e => e.name.split('/').pop()).join()"
        )
        .unwrap(),
        "imported.css"
    );
    // Removed and reinserted in one batch, the sheet still applies.
    page.eval_to_string(
        "const s = document.querySelector('style'); s.remove(); document.head.append(s); ''",
    )
    .unwrap();
    page.settle(Duration::from_millis(200));
    assert_eq!(
        page.eval_to_string("String(document.styleSheets.length)")
            .unwrap(),
        "1"
    );
}
