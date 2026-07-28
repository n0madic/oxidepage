//! Web Storage across pages and contexts (ADR-0027 D13).
//!
//! `localStorage` is per (browsing context, origin) and `sessionStorage` is per
//! page — every assertion here is about one of those two boundaries, because
//! getting either wrong is the kind of bug that only shows up once a driver
//! runs two pages at once.

mod common;

use std::time::{Duration, Instant};

use common::{spawn_server, test_options};
use oxidepage_engine::{
    Browser, BrowserContext, ContextOptions, NewPageOptions, PageEvent, PageHandle, WaitUntil,
};

fn eval(page: &PageHandle, source: &str) -> String {
    page.eval_to_string(source)
        .expect("page answered")
        .expect("eval succeeded")
}

fn page_at(context: &BrowserContext, url: &str) -> PageHandle {
    let page = context.new_page(NewPageOptions::default()).unwrap();
    page.navigate(url, WaitUntil::Load).unwrap().unwrap();
    page
}

/// Polls `source` until it equals `want`, so a test never depends on how many
/// loop turns a cross-thread delivery takes.
fn wait_until(page: &PageHandle, source: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = eval(page, source);
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "`{source}` never became `{want}` (last saw `{got}`)"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn local_storage_is_shared_between_pages_of_one_context_and_origin() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let one = page_at(&context, &server.url("/a"));
    let two = page_at(&context, &server.url("/b"));

    one.eval_to_string("localStorage.setItem('token', 'abc')")
        .unwrap()
        .unwrap();
    assert_eq!(
        eval(&two, "localStorage.getItem('token')"),
        "abc",
        "a sibling page of the same origin must see the write"
    );

    browser.close();
}

#[test]
fn local_storage_is_isolated_between_contexts() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();

    let one = page_at(&browser.default_context(), &server.url("/a"));
    let two = page_at(
        &browser.new_context(ContextOptions::default()),
        &server.url("/a"),
    );

    one.eval_to_string("localStorage.setItem('token', 'abc')")
        .unwrap()
        .unwrap();
    assert_eq!(
        eval(&two, "String(localStorage.getItem('token'))"),
        "null",
        "another context is another storage partition"
    );

    browser.close();
}

#[test]
fn local_storage_is_isolated_between_origins() {
    let one_server = spawn_server();
    let other_server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let one = page_at(&context, &one_server.url("/a"));
    let two = page_at(&context, &other_server.url("/a"));

    one.eval_to_string("localStorage.setItem('token', 'abc')")
        .unwrap()
        .unwrap();
    assert_eq!(
        eval(&two, "String(localStorage.getItem('token'))"),
        "null",
        "a different port is a different origin"
    );

    browser.close();
}

#[test]
fn blank_pages_do_not_share_an_opaque_origins_storage() {
    // `about:blank` has an *opaque* origin, which by definition shares with
    // nobody. Keying such a document by its URL would give every blank page in
    // a context one `localStorage` — the exact opposite.
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();
    let one = context.new_page(NewPageOptions::default()).unwrap();
    let two = context.new_page(NewPageOptions::default()).unwrap();

    one.eval_to_string("localStorage.setItem('t', 'a')")
        .unwrap()
        .unwrap();
    assert_eq!(
        eval(&two, "String(localStorage.getItem('t'))"),
        "null",
        "two opaque-origin pages must not share storage"
    );

    browser.close();
}

#[test]
fn session_storage_is_private_to_a_page() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let one = page_at(&context, &server.url("/a"));
    let two = page_at(&context, &server.url("/b"));

    one.eval_to_string("sessionStorage.setItem('tab', '1')")
        .unwrap()
        .unwrap();
    assert_eq!(eval(&one, "sessionStorage.getItem('tab')"), "1");
    assert_eq!(
        eval(&two, "String(sessionStorage.getItem('tab'))"),
        "null",
        "sessionStorage is per page, never shared"
    );

    browser.close();
}

#[test]
fn a_sibling_write_delivers_a_storage_event() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let listener = page_at(&context, &server.url("/listener"));
    let writer = page_at(&context, &server.url("/writer"));

    listener
        .eval_to_string(
            r"(() => {
                window.seen = [];
                // `String(...)` around each field: `Array.join` renders a
                // null as the empty string, which would hide the difference
                // between `oldValue === null` and `oldValue === ''`.
                window.addEventListener('storage', e => {
                    window.seen.push([String(e.key), String(e.oldValue),
                                      String(e.newValue),
                                      e.storageArea === localStorage].join(','));
                });
              })()",
        )
        .unwrap()
        .unwrap();

    writer
        .eval_to_string("localStorage.setItem('k', 'v1')")
        .unwrap()
        .unwrap();
    wait_until(&listener, "String(window.seen.length)", "1");
    assert_eq!(eval(&listener, "window.seen[0]"), "k,null,v1,true");

    writer
        .eval_to_string("localStorage.setItem('k', 'v2')")
        .unwrap()
        .unwrap();
    wait_until(&listener, "String(window.seen.length)", "2");
    assert_eq!(eval(&listener, "window.seen[1]"), "k,v1,v2,true");

    browser.close();
}

#[test]
fn a_sibling_write_wakes_an_idle_page() {
    // The listener is left genuinely idle — no timer, no rAF, no network — so
    // its loop is parked indefinitely in one blocking wait. A notification that
    // only pushed onto a queue would sit there unseen; the write has to *wake*
    // the page. Deliberately never polled with `eval`, because every poll is a
    // command that would wake the loop by itself and hide the bug.
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let listener = page_at(&context, &server.url("/listener"));
    let writer = page_at(&context, &server.url("/writer"));

    let events = listener.events();
    listener
        .eval_to_string(
            "window.addEventListener('storage', e => { console.log('storage:' + e.key); })",
        )
        .unwrap()
        .unwrap();
    // Let it settle into the indefinite park.
    std::thread::sleep(Duration::from_millis(200));
    while events.try_recv().is_ok() {}

    writer
        .eval_to_string("localStorage.setItem('woke', 'yes')")
        .unwrap()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(PageEvent::Console(message)) if message.message.contains("storage:woke") => {
                saw = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(
        saw,
        "a sibling's write must wake a parked page, not just queue"
    );

    browser.close();
}

#[test]
fn a_storage_event_carries_the_writers_url() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let listener = page_at(&context, &server.url("/listener"));
    let writer = page_at(&context, &server.url("/writer"));

    listener
        .eval_to_string(
            "window.seen = null; window.addEventListener('storage', e => { window.seen = e.url; })",
        )
        .unwrap()
        .unwrap();
    writer
        .eval_to_string("localStorage.setItem('k', 'v')")
        .unwrap()
        .unwrap();

    // HTML: the URL of the document whose storage changed — the writer's.
    wait_until(&listener, "String(window.seen)", &server.url("/writer"));

    browser.close();
}

#[test]
fn a_captured_storage_reference_follows_the_document_across_origins() {
    // The realm survives a navigation, so a script can hold `localStorage` from
    // the previous document. If that reference stayed aimed at the old origin's
    // area, a b-origin document would write a-origin data — and notify
    // a-origin siblings about it. The handle is re-pointed instead.
    let one_server = spawn_server();
    let other_server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let page = page_at(&context, &one_server.url("/a"));
    page.eval_to_string("window.ls = localStorage; ls.setItem('who', 'first')")
        .unwrap()
        .unwrap();

    page.navigate(&other_server.url("/b"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    // The captured reference now names the *new* origin's area, which is empty.
    assert_eq!(eval(&page, "String(window.ls.getItem('who'))"), "null");
    page.eval_to_string("window.ls.setItem('who', 'second')")
        .unwrap()
        .unwrap();
    assert_eq!(eval(&page, "localStorage.getItem('who')"), "second");

    // ... and the first origin's data was not touched by the second document.
    let witness = page_at(&context, &one_server.url("/a"));
    assert_eq!(eval(&witness, "localStorage.getItem('who')"), "first");

    browser.close();
}

#[test]
fn a_page_never_hears_its_own_write() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let page = page_at(&browser.default_context(), &server.url("/a"));

    page.eval_to_string(
        r"(() => {
            window.seen = 0;
            window.addEventListener('storage', () => { window.seen++; });
            localStorage.setItem('k', 'v');
          })()",
    )
    .unwrap()
    .unwrap();
    page.settle(Duration::from_millis(200)).unwrap();

    assert_eq!(
        eval(&page, "String(window.seen)"),
        "0",
        "HTML fires `storage` at the *other* documents, never the writer"
    );

    browser.close();
}

#[test]
fn session_storage_never_crosses_pages() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let context = browser.default_context();

    let listener = page_at(&context, &server.url("/listener"));
    let writer = page_at(&context, &server.url("/writer"));

    listener
        .eval_to_string(
            "window.seen = 0; window.addEventListener('storage', () => { window.seen++; })",
        )
        .unwrap()
        .unwrap();
    writer
        .eval_to_string("sessionStorage.setItem('k', 'v')")
        .unwrap()
        .unwrap();
    listener.settle(Duration::from_millis(200)).unwrap();

    assert_eq!(
        eval(&listener, "String(window.seen)"),
        "0",
        "sessionStorage is per page, so it has no other document to notify"
    );

    browser.close();
}

#[test]
fn exceeding_the_quota_throws_quota_exceeded_error() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();

    let name = eval(
        &page,
        r"(() => {
            const big = 'x'.repeat(6 * 1024 * 1024);
            try { localStorage.setItem('k', big); return 'no throw'; }
            catch (e) { return e.name; }
          })()",
    );
    assert_eq!(name, "QuotaExceededError");
    // The refused write stored nothing.
    assert_eq!(eval(&page, "String(localStorage.getItem('k'))"), "null");

    browser.close();
}

#[test]
fn the_named_property_surface_still_works_over_the_rust_backend() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();

    assert_eq!(
        eval(
            &page,
            r"(() => {
                localStorage.alpha = 'a';
                localStorage.setItem('beta', 'b');
                const keys = Object.keys(localStorage).join(',');
                const has = 'alpha' in localStorage;
                delete localStorage.alpha;
                return [keys, has, localStorage.beta,
                        String(localStorage.alpha), localStorage.length].join('|');
              })()"
        ),
        "alpha,beta|true|b|undefined|1"
    );

    // A member name is never shadowed by a stored key: `Storage` has no
    // `[LegacyOverrideBuiltIns]`.
    assert_eq!(
        eval(
            &page,
            "localStorage.setItem('length', 'nope'), String(localStorage.length)"
        ),
        "2"
    );

    browser.close();
}

#[test]
fn storage_is_a_real_interface_that_script_can_brand_check_and_patch() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();

    // VueUse brand-checks; analytics wrappers monkey-patch. Both work only
    // because this is genuinely the prototype in use (ADR-0027 D13).
    assert_eq!(eval(&page, "localStorage instanceof Storage"), "true");
    assert_eq!(
        eval(
            &page,
            r"(() => {
                const real = Storage.prototype.setItem;
                let seen = null;
                Storage.prototype.setItem = function (k, v) {
                    seen = k;
                    return real.call(this, k, v);
                };
                localStorage.setItem('patched', '1');
                Storage.prototype.setItem = real;
                return [seen, localStorage.getItem('patched')].join(',');
              })()"
        ),
        "patched,1"
    );

    browser.close();
}
