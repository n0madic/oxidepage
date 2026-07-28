//! `Browser` → `BrowserContext` → `PageHandle` (ADR-0027).
//!
//! What is asserted here is what the stage is *for*: two pages really do run at
//! once on their own threads, a context really is an isolation boundary, and
//! neither a panicking page nor a busy one can wedge the browser.

mod common;

use std::time::{Duration, Instant};

use common::{spawn_server, test_options};
use oxidepage_engine::{Browser, ContextOptions, EngineError, NewPageOptions, WaitUntil};

fn browser() -> Browser {
    Browser::new(test_options()).expect("browser")
}

fn title(page: &oxidepage_engine::PageHandle) -> String {
    page.eval_to_string("document.title")
        .expect("page answered")
        .expect("eval succeeded")
}

#[test]
fn two_pages_in_one_context_share_cookies() {
    let server = spawn_server();
    let browser = browser();
    let context = browser.default_context();

    let one = context.new_page(NewPageOptions::default()).unwrap();
    let two = context.new_page(NewPageOptions::default()).unwrap();
    assert_ne!(one.id(), two.id());

    one.navigate(&server.url("/set-cookie"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    two.navigate(&server.url("/echo-cookie"), WaitUntil::Load)
        .unwrap()
        .unwrap();

    assert_eq!(
        title(&two),
        "sid=s1",
        "a sibling page must send the jar's cookie"
    );
    browser.close();
}

#[test]
fn two_contexts_do_not_share_cookies() {
    let server = spawn_server();
    let browser = browser();

    let one = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let two = browser
        .new_context(ContextOptions::default())
        .new_page(NewPageOptions::default())
        .unwrap();

    one.navigate(&server.url("/set-cookie"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    two.navigate(&server.url("/echo-cookie"), WaitUntil::Load)
        .unwrap()
        .unwrap();

    assert_eq!(
        title(&two),
        "none",
        "another context must not see the first's cookies"
    );
    browser.close();
}

#[test]
fn the_http_cache_is_shared_within_a_context_and_not_across() {
    let server = spawn_server();
    let browser = browser();

    let context = browser.default_context();
    let one = context.new_page(NewPageOptions::default()).unwrap();
    let two = context.new_page(NewPageOptions::default()).unwrap();

    // `/uses-cache` pulls `/cached` as a stylesheet; the server counts the hits
    // that actually reached the wire.
    one.navigate(&server.url("/uses-cache"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    two.navigate(&server.url("/uses-cache"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    assert_eq!(
        server.cache_hits(),
        1,
        "a sibling page must be served from the browser's shared cache"
    );

    let other = browser
        .new_context(ContextOptions::default())
        .new_page(NewPageOptions::default())
        .unwrap();
    other
        .navigate(&server.url("/uses-cache"), WaitUntil::Load)
        .unwrap()
        .unwrap();
    assert_eq!(
        server.cache_hits(),
        2,
        "another context is another partition, so it must miss"
    );

    browser.close();
}

#[test]
fn pages_run_concurrently_on_their_own_threads() {
    let server = spawn_server();
    let browser = browser();
    let context = browser.default_context();

    let one = context.new_page(NewPageOptions::default()).unwrap();
    let two = context.new_page(NewPageOptions::default()).unwrap();

    // Two 500 ms document loads. On one thread they would take a second; on
    // two they overlap.
    let url = server.url("/delay/500");
    let started = Instant::now();
    let other = {
        let url = url.clone();
        std::thread::spawn(move || two.navigate(&url, WaitUntil::Load).unwrap().unwrap())
    };
    one.navigate(&url, WaitUntil::Load).unwrap().unwrap();
    other.join().unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(900),
        "the two loads must overlap, took {elapsed:?}"
    );
    browser.close();
}

#[test]
fn a_panicking_page_does_not_take_its_sibling_or_the_browser_down() {
    let browser = browser();
    let context = browser.default_context();
    let victim = context.new_page(NewPageOptions::default()).unwrap();
    let bystander = context.new_page(NewPageOptions::default()).unwrap();

    let error = victim
        .with(|_page| panic!("boom"))
        .expect_err("a panicking job cannot return a value");
    assert_eq!(error, EngineError::Crashed("boom".to_owned()));

    // The page is gone, and says so without a round trip.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !victim.is_closed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(victim.is_closed());
    assert_eq!(
        victim.eval_to_string("1").unwrap_err(),
        EngineError::Crashed("boom".to_owned()),
        "every later call must report the crash rather than hang"
    );

    // The sibling is untouched.
    bystander
        .set_content("<title>alive</title>")
        .unwrap()
        .unwrap();
    assert_eq!(title(&bystander), "alive");

    let started = Instant::now();
    browser.close();
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn browser_close_finishes_with_live_pages() {
    let browser = browser();
    let context = browser.default_context();
    for _ in 0..3 {
        let page = context.new_page(NewPageOptions::default()).unwrap();
        // Leave real work behind: a repeating timer keeps the loop awake.
        page.set_content("<script>setInterval(() => {}, 5);</script>")
            .unwrap()
            .unwrap();
    }
    assert_eq!(context.pages().len(), 3);

    let started = Instant::now();
    browser.close();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "close must not wait on busy pages, took {:?}",
        started.elapsed()
    );
    assert!(context.pages().is_empty());
}

#[test]
fn a_closed_page_reports_closed_rather_than_hanging() {
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.close();
    assert!(page.is_closed());
    assert_eq!(page.eval_to_string("1").unwrap_err(), EngineError::Closed);
    // Idempotent.
    page.close();
    browser.close();
}

#[test]
fn a_suspended_page_runs_nothing_until_resumed() {
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions {
            suspended: true,
            ..NewPageOptions::default()
        })
        .unwrap();

    // A short-timeout browser would be nicer, but the point is that the call
    // does not complete: post it and check it stays unanswered.
    let (tx, rx) = std::sync::mpsc::channel();
    page.post(move |p| {
        let _ = tx.send(p.eval_to_string("1 + 1").unwrap());
    })
    .unwrap();
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "a suspended page must not run ordinary work"
    );

    page.resume().unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the parked job must run once resumed"),
        "2"
    );
    browser.close();
}

#[test]
fn dropping_the_browser_tears_down_its_page_threads() {
    // A `ContextInner` that held an `Arc<BrowserInner>` while the browser's
    // `contexts` list held an `Arc<ContextInner>` would be a reference cycle:
    // `Drop for BrowserInner` would never run and every page thread — plus the
    // tokio runtime — would leak for the life of the process.
    let before = std::thread::available_parallelism().is_ok();
    assert!(before);

    let page = {
        let browser = browser();
        let page = browser
            .default_context()
            .new_page(NewPageOptions::default())
            .unwrap();
        page.set_content("<script>setInterval(() => {}, 5);</script>")
            .unwrap()
            .unwrap();
        page
        // `browser` drops here with a page still running.
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while !page.has_exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        page.has_exited(),
        "dropping the last Browser handle must join its page threads"
    );
}

#[test]
fn suspending_a_running_page_freezes_its_own_work_too() {
    // Suspending must stop the page, not merely stop answering the driver.
    // A page that kept executing script while refusing every command would be
    // the opposite of what a driver suspends for.
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.set_content("<script>window.n = 0; setInterval(() => { window.n++; }, 5);</script>")
        .unwrap()
        .unwrap();

    let ticks = |page: &oxidepage_engine::PageHandle| -> u32 {
        page.eval_to_string("String(window.n)")
            .unwrap()
            .unwrap()
            .parse()
            .unwrap()
    };

    let before = ticks(&page);
    // A control call, so it lands even though the page is busy. A suspended
    // page answers no ordinary job, so the count is read *after* resuming —
    // which is enough: 300 ms of a 5 ms interval is ~60 ticks if the page kept
    // running, and ~0 if it really froze.
    page.suspend().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    page.resume().unwrap();
    let after_resume = ticks(&page);
    assert!(
        after_resume - before < 10,
        "a suspended page must not keep firing its timers: {} ticks in 300 ms",
        after_resume - before
    );

    std::thread::sleep(Duration::from_millis(200));
    assert!(
        ticks(&page) > after_resume,
        "resuming must let the page run again"
    );

    browser.close();
}

#[test]
fn a_suspended_page_with_a_live_timer_parks_instead_of_spinning() {
    // The subtle half of suspending. A suspended page runs no task source, so
    // it never *consumes* its timers — and a loop that still parked until the
    // next timer deadline would wake on an already-past `Instant`, find nothing
    // to do, and go straight round again: a pegged core for the whole
    // suspension. Counting ticks cannot see that; the loop counters can.
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.set_content("<script>setInterval(() => {}, 5);</script>")
        .unwrap()
        .unwrap();

    let before = page.loop_stats().unwrap();
    page.suspend().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    page.resume().unwrap();
    let after = page.loop_stats().unwrap();

    // The direct form of ADR-0004's property: a parked page accounts for
    // nearly all of its wall clock as time spent parked, a spinning one for
    // almost none of it — whatever the call count happens to say.
    let parked = after.parked_micros - before.parked_micros;
    assert!(
        parked > 200_000,
        "a suspended page must be parked for the wall clock it spends suspended, \
         but only {parked} µs of 300 ms were spent in a wait"
    );
    let waits = after.blocking_waits - before.blocking_waits;
    assert!(
        waits < 100,
        "a suspended page must park, not spin: {waits} blocking waits in 300 ms"
    );
    let turns = after.turns - before.turns;
    assert!(
        turns < 100,
        "a suspended page must not churn the loop: {turns} turns in 300 ms"
    );

    browser.close();
}

#[test]
fn suspending_a_page_mid_settle_parks_instead_of_spinning() {
    // The variant the sibling test misses. `suspend` is a *control* job, so it
    // lands at the wait point **inside** a running `settle` — and from then on
    // `run_until_stalled_until` returns immediately while `next_deadline` keeps
    // yielding the already-past deadline of a timer the suspended page will
    // never fire. Guarding only the top-level command loop leaves this one
    // spinning for the rest of the budget.
    let browser = browser();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    page.set_content("<script>setInterval(() => {}, 5);</script>")
        .unwrap()
        .unwrap();

    let before = page.loop_stats().unwrap();
    let settling = {
        let page = page.clone();
        std::thread::spawn(move || page.settle(Duration::from_secs(3)))
    };
    std::thread::sleep(Duration::from_millis(100));
    page.suspend().unwrap();
    std::thread::sleep(Duration::from_millis(400));

    let during = page.loop_stats().unwrap();
    let waits = during.blocking_waits - before.blocking_waits;
    assert!(
        waits < 500,
        "suspending mid-settle must not spin: {waits} blocking waits"
    );

    page.resume().unwrap();
    settling.join().unwrap().unwrap();
    browser.close();
}

#[test]
fn a_page_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Browser>();
    assert_send_sync::<oxidepage_engine::BrowserContext>();
    assert_send_sync::<oxidepage_engine::PageHandle>();
}
