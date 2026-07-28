//! The push event bus (ADR-0027 D6).
//!
//! The interesting case is the one that is *not* pushed: a rejected promise
//! whose handler attaches a tick later is not an error, and the rejection
//! tracker retracts it. Push and pull have to agree about that, or a driver
//! would report errors a browser never would.

mod common;

use std::time::{Duration, Instant};

use common::{spawn_server, test_options};
use oxidepage_engine::page_api::{ConsoleLevel, NavigationEventKind, ScriptErrorKind};
use oxidepage_engine::{Browser, NewPageOptions, PageEvent, WaitUntil};

/// Collects events until `want` returns true or the deadline passes.
fn wait_for(
    events: &crossbeam_channel::Receiver<PageEvent>,
    timeout: Duration,
    mut want: impl FnMut(&PageEvent) -> bool,
) -> Option<PageEvent> {
    let deadline = Instant::now() + timeout;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(event) if want(&event) => return Some(event),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn drain(events: &crossbeam_channel::Receiver<PageEvent>) -> Vec<PageEvent> {
    events.try_iter().collect()
}

#[test]
fn console_output_arrives_on_the_bus() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();

    page.set_content("<script>console.warn('from the page');</script>")
        .unwrap()
        .unwrap();

    let event = wait_for(&events, Duration::from_secs(5), |e| {
        matches!(e, PageEvent::Console(_))
    })
    .expect("a console event");
    let PageEvent::Console(message) = event else {
        unreachable!()
    };
    assert_eq!(message.level, ConsoleLevel::Warn);
    assert!(message.message.contains("from the page"));

    browser.close();
}

#[test]
fn an_uncaught_error_arrives_on_the_bus() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();

    page.set_content("<script>null.nope;</script>")
        .unwrap()
        .unwrap();

    let event = wait_for(&events, Duration::from_secs(5), |e| {
        matches!(e, PageEvent::Error(_))
    })
    .expect("an error event");
    let PageEvent::Error(error) = event else {
        unreachable!()
    };
    assert_eq!(error.kind, ScriptErrorKind::Uncaught);

    browser.close();
}

#[test]
fn navigation_milestones_arrive_on_the_bus() {
    let server = spawn_server();
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();

    page.navigate(&server.url("/hello"), WaitUntil::Load)
        .unwrap()
        .unwrap();

    let event = wait_for(&events, Duration::from_secs(5), |e| {
        matches!(
            e,
            PageEvent::Navigation(n) if n.kind == NavigationEventKind::Load
        )
    });
    assert!(event.is_some(), "the load milestone must reach the bus");

    browser.close();
}

#[test]
fn a_rejection_handled_a_tick_later_is_never_reported() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();

    page.set_content(
        r"<script>
          const p = Promise.reject(new Error('late handler'));
          setTimeout(() => p.catch(() => {}), 10);
        </script>",
    )
    .unwrap()
    .unwrap();
    // Give the loop plenty of chances to stall (and so to flush) after the
    // handler attaches.
    page.settle(Duration::from_millis(300)).unwrap();

    let reported: Vec<_> = drain(&events)
        .into_iter()
        .filter(|e| matches!(e, PageEvent::Error(_)))
        .collect();
    assert!(
        reported.is_empty(),
        "a rejection with a handler attached later is not an error: {reported:?}"
    );

    browser.close();
}

#[test]
fn a_rejection_nobody_handles_is_reported_once_the_loop_stalls() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();

    page.set_content("<script>Promise.reject(new Error('nobody catches me'));</script>")
        .unwrap()
        .unwrap();

    let event = wait_for(&events, Duration::from_secs(5), |e| {
        matches!(e, PageEvent::Error(error) if error.kind == ScriptErrorKind::UnhandledRejection)
    })
    .expect("the unhandled rejection must be reported");
    let PageEvent::Error(error) = event else {
        unreachable!()
    };
    assert!(error.message.contains("nobody catches me"));

    browser.close();
}

#[test]
fn an_unhandled_rejection_is_reported_on_a_page_that_never_goes_idle() {
    // Gating the report on full idleness meant any SPA with a `setInterval`
    // never reported one at all — and because the pending deque is bounded,
    // repeated rejections silently evicted the oldest rather than arriving late.
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();

    page.set_content(
        r"<script>
          setInterval(() => {}, 20);
          Promise.reject(new Error('never handled'));
        </script>",
    )
    .unwrap()
    .unwrap();

    // Longer than `UNHANDLED_REJECTION_GRACE`: on a page that never goes idle
    // the report is deliberately delayed, not immediate.
    let event = wait_for(&events, Duration::from_secs(15), |e| {
        matches!(e, PageEvent::Error(error) if error.kind == ScriptErrorKind::UnhandledRejection)
    })
    .expect("a busy page must still report unhandled rejections");
    let PageEvent::Error(error) = event else {
        unreachable!()
    };
    assert!(error.message.contains("never handled"));

    browser.close();
}

#[test]
fn a_page_with_a_sink_leaves_the_pull_streams_empty() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();

    page.set_content("<script>console.log('once');</script>")
        .unwrap()
        .unwrap();
    wait_for(&events, Duration::from_secs(5), |e| {
        matches!(e, PageEvent::Console(_))
    })
    .expect("a console event");

    // A record goes to the sink *or* to its stream, never both — otherwise a
    // driver that also polls would see everything twice.
    let pulled = page
        .with(oxidepage_engine::page_api::Page::drain_console)
        .unwrap();
    assert!(
        pulled.is_empty(),
        "the pull stream must stay empty while a sink is installed: {pulled:?}"
    );

    browser.close();
}

#[test]
fn closing_a_page_ends_its_event_stream() {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions::default())
        .unwrap();
    let events = page.events();
    page.close();

    let closed = wait_for(&events, Duration::from_secs(5), |e| {
        matches!(e, PageEvent::Closed)
    });
    assert!(closed.is_some(), "a closing page must say so on the bus");
    browser.close();
}
