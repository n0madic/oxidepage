//! The page half of the pause point (ADR-0032 D1–D3).
//!
//! `crates/net/tests/intercept.rs` covers the funnel itself. What is left — and
//! what can only be tested here — is the page's *event loop* under a pause: the
//! borrows a blocking pause is taken under, whether a navigation cleans up the
//! requests it abandons, and whether resolving a pause costs the loop an extra
//! park (ADR-0004's one-blocking-wait property).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use oxidepage_page::{
    InterceptCommand, NetworkEvent, Page, PageOptions, PageRecord, ResourcePolicy, WaitUntil,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback server serving a small fixed site.
///
/// `/imports.css` is the load-bearing route: a blocking `@import` is resolved
/// from *inside* stylo's cascade, with `dom` and `style` borrowed, which is the
/// borrow state a blocking pause has to survive.
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
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut tmp = [0u8; 4096];
                    let read = sock.read(&mut tmp).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&tmp[..read]);
                    let path = head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_owned();
                    let (content_type, body) = match path.as_str() {
                        "/imports.css" => ("text/css", String::from("@import url(inner.css);")),
                        "/inner.css" => ("text/css", String::from("#a { color: rgb(1, 2, 3); }")),
                        "/slow.html" => {
                            tokio::time::sleep(Duration::from_millis(120)).await;
                            ("text/html", String::from("<title>slow</title>"))
                        }
                        "/withimage.html" => (
                            "text/html",
                            format!("<img src=\"http://127.0.0.1:{port}/pixel.png\">"),
                        ),
                        "/pixel.png" => ("image/png", String::new()),
                        "/import.html" => (
                            "text/html",
                            format!(
                                "<style>@import url(http://127.0.0.1:{port}/imports.css);</style>\
                                 <p id=a>x</p>"
                            ),
                        ),
                        _ => ("text/html", String::from("<title>doc</title>")),
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
    });
    rx.recv().expect("server failed to start")
}

fn loopback_page() -> Page {
    Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap()
}

/// Installs an event sink that resolves every pause with `answer`, and reports
/// the URLs it saw paused.
fn auto_resolve(
    page: &Page,
    answer: impl Fn(&oxidepage_page::InterceptControl, oxidepage_page::RequestId) + 'static,
) -> Rc<RefCell<Vec<String>>> {
    let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    let control = page.intercept();
    page.set_event_sink(Some(Rc::new(move |record: PageRecord| {
        if let PageRecord::Network {
            event: NetworkEvent::Paused { id, url, .. },
            ..
        } = record
        {
            sink.borrow_mut().push(url);
            answer(&control, id);
        }
    })));
    seen
}

#[test]
fn a_blocking_import_pause_survives_the_borrows_it_is_taken_under() {
    // The regression this file exists for. `PageCssFetcher::fetch_css` is called
    // from *inside* stylo's `@import` resolution, with `dom` and `style`
    // borrowed; `ModuleLoader::load` is called from inside QuickJS. If the
    // blocking park serviced net events or control jobs, either would enter JS
    // under those borrows and panic with a `BorrowMutError` — deterministically,
    // not as a race.
    let port = spawn_server();
    let page = loopback_page();
    page.intercept().enable(0, "s", Vec::new(), false);
    let seen = auto_resolve(&page, |control, id| {
        control.send(InterceptCommand::release(id));
    });

    page.navigate(
        &format!("http://127.0.0.1:{port}/import.html"),
        WaitUntil::Load,
    )
    .expect("navigation");

    let urls = seen.borrow().clone();
    assert!(
        urls.iter().any(|url| url.ends_with("/imports.css")),
        "the `<style>@import` sheet must have paused: {urls:?}"
    );
    assert!(
        urls.iter().any(|url| url.ends_with("/inner.css")),
        "the *nested* @import — resolved under live borrows — must have paused too: {urls:?}"
    );

    // And the whole chain actually applied, so the pause resumed rather than
    // timing out into an empty sheet.
    let color = page
        .eval_to_string("getComputedStyle(document.getElementById('a')).color")
        .expect("computed style");
    assert_eq!(color, "rgb(1, 2, 3)");
}

#[test]
fn the_document_request_itself_pauses() {
    // `commit_document` goes through `fetch_blocking`, which is exactly why the
    // gate lives in `NetService` and not in `page`: put it a layer up and the
    // one request a driver most wants to intercept is the one it misses.
    let port = spawn_server();
    let page = loopback_page();
    page.intercept().enable(0, "s", Vec::new(), false);
    let seen = auto_resolve(&page, |control, id| {
        control.send(InterceptCommand::release(id));
    });

    page.navigate(&format!("http://127.0.0.1:{port}/a.html"), WaitUntil::Load)
        .expect("navigation");

    assert!(
        seen.borrow().iter().any(|url| url.ends_with("/a.html")),
        "the document request must pause: {:?}",
        seen.borrow()
    );
}

#[test]
fn navigating_away_from_a_paused_subresource_leaves_nothing_behind() {
    // `reset_document_state` aborts every pending load on navigation. A parked
    // `NetRequest` left in the map would leak one per navigation — and a late
    // `continueRequest` would resurrect a dead document's request into the live
    // one, under an id the new document has since reissued.
    //
    // The abandoned request is an **image**, deliberately: an image load is
    // asynchronous, so it can still be held when the navigation happens. A
    // blocking `@import` cannot be — the page is inside the fetch, so the pause
    // resolves (or times out) before the navigation can start.
    let port = spawn_server();
    let page = loopback_page();
    page.intercept().enable(0, "s", Vec::new(), false);

    // Only the *document* is answered; the image is abandoned mid-pause.
    let control = page.intercept();
    page.set_event_sink(Some(Rc::new(move |record: PageRecord| {
        if let PageRecord::Network {
            event: NetworkEvent::Paused { id, url, .. },
            ..
        } = record
            && url.ends_with(".html")
        {
            control.send(InterceptCommand::release(id));
        }
    })));

    page.navigate(
        &format!("http://127.0.0.1:{port}/withimage.html"),
        WaitUntil::DomContentLoaded,
    )
    .expect("first navigation");
    page.settle(Duration::from_millis(150));
    assert!(
        !page.intercept().paused_ids().is_empty(),
        "the abandoned image should still be held"
    );

    page.navigate(
        &format!("http://127.0.0.1:{port}/b.html"),
        WaitUntil::DomContentLoaded,
    )
    .expect("second navigation");
    page.settle(Duration::from_millis(150));

    assert!(
        page.intercept().paused_ids().is_empty(),
        "navigating away must release the previous document's paused requests: {:?}",
        page.intercept().paused_ids()
    );
}

#[test]
fn a_fulfilled_document_never_reaches_the_network() {
    let port = spawn_server();
    let page = loopback_page();
    page.intercept().enable(0, "s", Vec::new(), false);

    let control = page.intercept();
    page.set_event_sink(Some(Rc::new(move |record: PageRecord| {
        if let PageRecord::Network {
            event: NetworkEvent::Paused { id, .. },
            ..
        } = record
        {
            control.send(InterceptCommand::Fulfill {
                id,
                response: Box::new(oxidepage_page::FulfilledResponse {
                    status: 200,
                    status_text: String::from("OK"),
                    headers: vec![(String::from("content-type"), String::from("text/html"))],
                    body: b"<title>stub</title><p id=a>from the driver</p>".to_vec(),
                }),
            });
        }
    })));

    page.navigate(&format!("http://127.0.0.1:{port}/a.html"), WaitUntil::Load)
        .expect("navigation");

    let text = page
        .eval_to_string("document.getElementById('a').textContent")
        .expect("text");
    assert_eq!(text, "from the driver");
}

#[test]
fn resolving_pauses_costs_the_loop_no_extra_parks() {
    // ADR-0004's criterion, applied to the fourth `Select` arm: a decision is
    // *one more arm on the existing park*, not a park of its own. A loop that
    // spun on a permanently-ready receiver — which is what a decision channel
    // with no page-side sender would be — shows up here as a wait count orders
    // of magnitude above the uninstrumented load, not as a wrong answer
    // anywhere.
    let port = spawn_server();

    let plain = loopback_page();
    plain
        .navigate(
            &format!("http://127.0.0.1:{port}/import.html"),
            WaitUntil::Load,
        )
        .expect("navigation");
    let baseline = plain.loop_stats().blocking_waits;

    let page = loopback_page();
    page.intercept().enable(0, "s", Vec::new(), false);
    let _seen = auto_resolve(&page, |control, id| {
        control.send(InterceptCommand::release(id));
    });
    page.navigate(
        &format!("http://127.0.0.1:{port}/import.html"),
        WaitUntil::Load,
    )
    .expect("navigation");
    let intercepted = page.loop_stats().blocking_waits;

    assert!(
        intercepted <= baseline + 8,
        "interception must not add parks: {baseline} without, {intercepted} with"
    );
}

#[test]
fn the_intercept_timeout_stays_below_the_command_timeout() {
    // The constraint D7 names, asserted rather than trusted to a comment. A
    // `Page.navigate` whose document pause goes unanswered must give up
    // *before* the driver's command times out — otherwise the driver is told
    // its navigation failed while the page is still loading, and then the page
    // proceeds anyway.
    //
    // Not tested by waiting: the timeout is 20 s by design, and a test that
    // spent it would be the slowest in the suite for no extra confidence.
    assert!(
        oxidepage_page::DEFAULT_INTERCEPT_TIMEOUT < Duration::from_secs(30),
        "the intercept timeout must stay below the engine's command timeout"
    );
}

#[test]
fn a_suspended_page_resolves_nothing() {
    // The decision arm is under the same `!suspended` gate as the net arm:
    // resolving a pause spawns or synthesizes a response, and a frozen page must
    // run neither. The decision stays in the channel until it resumes.
    //
    // The held request is the **image**, not the document: the document's pause
    // is a blocking one, and a page parked inside a blocking fetch cannot be
    // suspended in the first place.
    let port = spawn_server();
    let page = loopback_page();
    page.intercept().enable(0, "s", Vec::new(), false);

    let control = page.intercept();
    page.set_event_sink(Some(Rc::new(move |record: PageRecord| {
        if let PageRecord::Network {
            event: NetworkEvent::Paused { id, url, .. },
            ..
        } = record
            && url.ends_with(".html")
        {
            control.send(InterceptCommand::release(id));
        }
    })));
    page.navigate(
        &format!("http://127.0.0.1:{port}/withimage.html"),
        WaitUntil::DomContentLoaded,
    )
    .expect("navigation");
    page.settle(Duration::from_millis(150));

    let id = *page
        .intercept()
        .paused_ids()
        .first()
        .expect("the image request should be held");

    page.suspend();
    page.intercept().send(InterceptCommand::release(id));
    page.settle(Duration::from_millis(150));
    assert!(
        page.intercept().paused_ids().contains(&id),
        "a suspended page must not resolve a pause"
    );

    page.resume();
    page.settle(Duration::from_millis(300));
    assert!(
        !page.intercept().paused_ids().contains(&id),
        "and it must resolve it on resume"
    );
}
