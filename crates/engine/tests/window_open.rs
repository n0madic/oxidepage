//! `window.open` and `<a target=_blank>` (ADR-0027 D12).
//!
//! The hazard this file exists for is a deadlock: the hook runs on the
//! *opener's* thread, with JavaScript on its stack and its DOM borrowed, and it
//! blocks until the new page's realm exists. That is only sound because nothing
//! the new thread does needs anything from the opener — which is what
//! `two_pages_can_open_each_other` proves the hard way.

mod common;

use std::time::{Duration, Instant};

use common::{spawn_server, test_options};
use oxidepage_engine::{Browser, BrowserOptions, NewPageOptions, PageEvent, PageHandle, WaitUntil};

fn browser() -> Browser {
    Browser::new(test_options()).expect("browser")
}

fn eval(page: &PageHandle, source: &str) -> String {
    page.eval_to_string(source)
        .expect("page answered")
        .expect("eval succeeded")
}

/// The page of `context` that is not `known`, once one shows up.
fn sibling_of(context: &oxidepage_engine::BrowserContext, known: &PageHandle) -> PageHandle {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(page) = context.pages().into_iter().find(|p| p.id() != known.id()) {
            return page;
        }
        assert!(Instant::now() < deadline, "no sibling page was opened");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn window_open_creates_a_sibling_in_the_same_context() {
    let server = spawn_server();
    let browser = browser();
    let context = browser.default_context();
    let opener = context.new_page(NewPageOptions::default()).unwrap();

    opener
        .navigate(&server.url("/opener"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    assert_eq!(
        eval(&opener, "typeof (window.w = window.open('/hello'))"),
        "object",
        "window.open must return a WindowProxy when the driver can open one"
    );

    let sibling = sibling_of(&context, &opener);
    assert_eq!(context.pages().len(), 2);

    // The sibling navigates to the requested URL on its own thread.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if eval(&sibling, "document.title") == "hello" {
            break;
        }
        assert!(Instant::now() < deadline, "the sibling never navigated");
        std::thread::sleep(Duration::from_millis(20));
    }

    browser.close();
}

#[test]
fn the_returned_proxy_exposes_only_what_works() {
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.set_content("<script>window.w = window.open();</script>")
        .unwrap()
        .unwrap();

    assert_eq!(eval(&page, "window.w instanceof WindowProxy"), "true");
    assert_eq!(eval(&page, "typeof w.close"), "function");
    assert_eq!(eval(&page, "typeof w.focus"), "function");
    assert_eq!(eval(&page, "w.closed"), "false");

    // P6: what is not implemented is not installed, so feature detection is
    // honest rather than being fooled by an always-failing stub.
    assert_eq!(eval(&page, "'postMessage' in w"), "false");
    assert_eq!(eval(&page, "'opener' in w"), "false");
    assert_eq!(eval(&page, "'document' in w"), "false");

    // Reading a sibling's location is what it is for a cross-origin
    // `WindowProxy` in a browser: a `SecurityError`.
    assert_eq!(
        eval(
            &page,
            "(() => { try { return w.location, 'no throw'; } catch (e) { return e.name; } })()"
        ),
        "SecurityError"
    );

    browser.close();
}

#[test]
fn closing_the_opened_window_is_visible_to_the_opener() {
    let browser = browser();
    let context = browser.default_context();
    let opener = context.new_page(NewPageOptions::default()).unwrap();
    opener
        .set_content("<script>window.w = window.open();</script>")
        .unwrap()
        .unwrap();
    let sibling = sibling_of(&context, &opener);

    assert_eq!(eval(&opener, "w.closed"), "false");
    // `w.close()` reads back as closed on the very next line, as in a browser.
    assert_eq!(eval(&opener, "w.close(), String(w.closed)"), "true");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !sibling.is_closed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        sibling.is_closed(),
        "close() must reach the sibling's thread"
    );

    browser.close();
}

#[test]
fn writing_location_navigates_the_sibling() {
    let server = spawn_server();
    let browser = browser();
    let context = browser.default_context();
    let opener = context.new_page(NewPageOptions::default()).unwrap();
    opener
        .navigate(&server.url("/opener"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    opener
        .eval_to_string("window.w = window.open()")
        .unwrap()
        .unwrap();
    let sibling = sibling_of(&context, &opener);

    // Relative, so this also pins that the write resolves against the
    // *opener's* document, which is what HTML says.
    opener
        .eval_to_string("w.location = '/moved'")
        .unwrap()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if eval(&sibling, "document.title") == "moved" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the sibling never navigated to the written location"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    browser.close();
}

#[test]
fn focus_is_reported_on_the_siblings_bus_rather_than_silently_dropped() {
    let browser = browser();
    let context = browser.default_context();
    let opener = context.new_page(NewPageOptions::default()).unwrap();
    opener
        .set_content("<script>window.w = window.open();</script>")
        .unwrap()
        .unwrap();
    let sibling = sibling_of(&context, &opener);
    let events = sibling.events();

    opener.eval_to_string("w.focus()").unwrap().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(PageEvent::FocusRequested) => {
                saw = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw, "focus() must reach the embedder as an event");

    browser.close();
}

#[test]
fn a_target_blank_link_opens_a_new_page() {
    let server = spawn_server();
    let browser = browser();
    let context = browser.default_context();
    let page = context.new_page(NewPageOptions::default()).unwrap();

    page.navigate(&server.url("/opener"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    page.eval_to_string(
        r"(() => {
            const a = document.createElement('a');
            a.href = '/hello';
            a.target = '_blank';
            document.body.appendChild(a);
            a.click();
          })()",
    )
    .unwrap()
    .unwrap();

    let sibling = sibling_of(&context, &page);
    // The opener stayed where it was — the whole point of a `_blank` target.
    assert_eq!(eval(&page, "document.title"), "opener");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if eval(&sibling, "document.title") == "hello" {
            break;
        }
        assert!(Instant::now() < deadline, "the new page never navigated");
        std::thread::sleep(Duration::from_millis(20));
    }

    browser.close();
}

#[test]
fn a_current_context_target_navigates_in_place_instead_of_opening() {
    let server = spawn_server();
    let browser = browser();
    let context = browser.default_context();
    let page = context.new_page(NewPageOptions::default()).unwrap();
    page.navigate(&server.url("/opener"), WaitUntil::Load)
        .unwrap()
        .unwrap();

    // `_self`, `_parent` and `_top` all name the one browsing context there is,
    // so they navigate the caller — opening a sibling for them would leave the
    // caller sitting where it was, which is the opposite of what script asked.
    assert_eq!(
        eval(&page, "window.open('/moved', '_self') === window"),
        "true"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if eval(&page, "document.title") == "moved" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "`_self` must navigate the caller"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(context.pages().len(), 1, "no sibling may have been opened");

    // Same for a `_top` link.
    page.eval_to_string(
        r"(() => {
            const a = document.createElement('a');
            a.href = '/opener';
            a.target = '_top';
            document.body.appendChild(a);
            a.click();
          })()",
    )
    .unwrap()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if eval(&page, "document.title") == "opener" {
            break;
        }
        assert!(Instant::now() < deadline, "`_top` must navigate in place");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(context.pages().len(), 1);

    // An *empty* target is the other way round: HTML maps it to `_blank`, so
    // `window.open(url, "")` opens a page even though `<a target="">` does not.
    // A WPT `cssom-view` test calls exactly this, and treating it as `_self`
    // navigated the harness away mid-run.
    assert_eq!(eval(&page, "typeof window.open('/hello', '')"), "object");
    sibling_of(&context, &page);
    assert_eq!(context.pages().len(), 2);

    browser.close();
}

#[test]
fn closing_a_sibling_from_script_does_not_make_close_block() {
    // `w.close()` marks the page closed for script on the next line, but the
    // sibling's *thread* is still running. If the two facts shared one flag,
    // `join_bounded` would skip its poll and call `JoinHandle::join` on a live
    // thread — an unbounded wait, defeating `close_timeout`.
    let browser = browser();
    let context = browser.default_context();
    let opener = context.new_page(NewPageOptions::default()).unwrap();
    opener
        .set_content("<script>window.w = window.open();</script>")
        .unwrap()
        .unwrap();
    let sibling = sibling_of(&context, &opener);

    assert_eq!(eval(&opener, "w.close(), String(w.closed)"), "true");
    assert!(sibling.is_closed(), "script's view is immediate");

    let started = Instant::now();
    browser.close();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "close must stay bounded, took {:?}",
        started.elapsed()
    );
    assert!(sibling.has_exited(), "the thread really did finish");
}

#[test]
fn two_pages_can_open_each_other() {
    // The deadlock check. Each `open` blocks its caller's thread until the new
    // page's realm exists; if the new thread needed anything from its opener —
    // a lock, a reply — this would hang instead of finishing.
    let browser = browser();
    let context = browser.default_context();
    let a = context.new_page(NewPageOptions::default()).unwrap();

    a.set_content("<script>window.w = window.open();</script>")
        .unwrap()
        .unwrap();
    let b = sibling_of(&context, &a);

    let started = Instant::now();
    // B opens a third page while A is alive and holding a proxy on B.
    b.eval_to_string("window.w = window.open()")
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "opening a window from an opened window must not deadlock"
    );
    assert_eq!(context.pages().len(), 3);

    browser.close();
}

#[test]
fn a_popup_loop_is_capped_rather_than_exhausting_the_host() {
    // Every `window.open` spawns an OS thread and a whole `Page`. Nothing else
    // bounds it — the `ScriptBudget` is per task and each `open` is fast — so
    // without a cap `for(;;) window.open()` on attacker-controlled content
    // exhausts the host's threads. Past the cap the answer is `null`, which is
    // what a browser with a popup blocker returns.
    let browser = Browser::new(BrowserOptions {
        max_pages_per_context: 4,
        ..test_options()
    })
    .unwrap();
    let context = browser.default_context();
    let page = context.new_page(NewPageOptions::default()).unwrap();

    let opened = eval(
        &page,
        "(() => { let n = 0; for (let i = 0; i < 50; i++) { if (window.open()) n++; } return String(n); })()",
    );
    assert_eq!(opened, "3", "the opener itself counts against the cap");
    assert_eq!(context.pages().len(), 4);

    browser.close();
}

#[test]
fn a_bare_page_returns_null_from_window_open() {
    // No driver, no second browsing context: `window.open` returns `null`, the
    // same answer a browser gives for a blocked popup. Not a stub — a real
    // answer (P6).
    let page = oxidepage_page::load_html_page(
        "<script>window.result = window.open('https://example.com/');</script>",
        oxidepage_page::PageOptions::default(),
    )
    .expect("page");
    assert_eq!(
        page.eval_to_string("String(window.result)").unwrap(),
        "null"
    );
}
