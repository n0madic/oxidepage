//! Dialogs answered from another thread (ADR-0027 D11, discharging the
//! stage-5 obligation ADR-0025 recorded).
//!
//! `DialogPolicy::Ask` parks the page thread with JavaScript on the stack.
//! Every way out of that park has to be bounded, because the `ScriptBudget`
//! cannot help: it is enforced through the JS engine's interrupt callback, and
//! this block is in Rust.

mod common;

use std::time::{Duration, Instant};

use common::test_options;
use oxidepage_engine::{
    Browser, BrowserOptions, DialogPolicy, DialogResponse, NewPageOptions, PageEvent, PageHandle,
};

fn page_with(policy: DialogPolicy) -> (Browser, PageHandle) {
    let browser = Browser::new(test_options()).unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions {
            dialog_policy: Some(policy),
            ..NewPageOptions::default()
        })
        .unwrap();
    (browser, page)
}

#[test]
fn the_default_policy_dismisses_exactly_as_a_bare_page_does() {
    let (browser, page) = page_with(DialogPolicy::Dismiss);
    page.set_content("<script>window.answer = confirm('ok?');</script>")
        .unwrap()
        .unwrap();
    assert_eq!(
        page.eval_to_string("String(window.answer)")
            .unwrap()
            .unwrap(),
        "false"
    );
    browser.close();
}

#[test]
fn the_accept_policy_accepts_without_asking_anyone() {
    let (browser, page) = page_with(DialogPolicy::Accept);
    page.set_content("<script>window.answer = confirm('ok?');</script>")
        .unwrap()
        .unwrap();
    assert_eq!(
        page.eval_to_string("String(window.answer)")
            .unwrap()
            .unwrap(),
        "true"
    );
    browser.close();
}

#[test]
fn ask_parks_the_page_until_the_driver_answers() {
    let (browser, page) = page_with(DialogPolicy::Ask {
        timeout: Duration::from_secs(10),
    });
    let events = page.events();

    // The load blocks inside `confirm`, so run it off this thread and answer
    // from here — which is the whole arrangement under test.
    let loader = {
        let page = page.clone();
        std::thread::spawn(move || {
            page.set_content(
                "<script>window.answer = confirm('really?'); window.done = 1;</script>",
            )
        })
    };

    // The page must not have finished while the dialog is open.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !loader.is_finished(),
        "the page must be parked on the dialog"
    );

    // No sleep before reading: the page raises its "dialog open" flag *before*
    // announcing, so answering the instant the event arrives must work. A flag
    // raised after the announcement would refuse this answer and leave the
    // dialog to time out.
    // The driver learns about the dialog from the bus and answers it — no
    // polling. `DialogOpening` arriving *before* the answer is the whole
    // contract: it is emitted before the handler blocks, so the only party who
    // can answer is told in time.
    let opening = events
        .recv_timeout(Duration::from_secs(5))
        .ok()
        .into_iter()
        .chain(events.try_iter())
        .find(|e| matches!(e, PageEvent::DialogOpening(_)))
        .expect("a DialogOpening event");
    let PageEvent::DialogOpening(request) = opening else {
        unreachable!()
    };
    assert_eq!(request.message, "really?");
    page.answer_dialog(DialogResponse::Accept)
        .expect("the answer must be accepted");

    loader.join().unwrap().unwrap().unwrap();
    assert_eq!(
        page.eval_to_string("String(window.answer)")
            .unwrap()
            .unwrap(),
        "true"
    );

    // ... and the completed record follows, carrying the answer it got.
    let dialog = events
        .recv_timeout(Duration::from_secs(5))
        .ok()
        .into_iter()
        .chain(events.try_iter())
        .find(|e| matches!(e, PageEvent::Dialog(_)))
        .expect("a completed dialog event");
    let PageEvent::Dialog(event) = dialog else {
        unreachable!()
    };
    assert_eq!(event.message, "really?");
    assert_eq!(event.response, DialogResponse::Accept);

    browser.close();
}

#[test]
fn answering_when_no_dialog_is_open_is_refused_rather_than_buffered() {
    let (browser, page) = page_with(DialogPolicy::Ask {
        timeout: Duration::from_secs(10),
    });
    // An answer nobody asked for must not sit in the channel waiting to
    // release the *next* dialog — and must not block the caller either.
    let started = Instant::now();
    assert!(page.answer_dialog(DialogResponse::Accept).is_err());
    assert!(started.elapsed() < Duration::from_millis(500));
    browser.close();
}

#[test]
fn an_unanswered_ask_times_out_into_a_dismiss() {
    let (browser, page) = page_with(DialogPolicy::Ask {
        timeout: Duration::from_millis(200),
    });

    let started = Instant::now();
    page.set_content("<script>window.answer = confirm('nobody is listening');</script>")
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(150),
        "the page really must have parked, took only {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the park must be bounded by the policy timeout, took {elapsed:?}"
    );
    assert_eq!(
        page.eval_to_string("String(window.answer)")
            .unwrap()
            .unwrap(),
        "false",
        "a timed-out dialog falls back to the auto-dismiss answer"
    );
    browser.close();
}

#[test]
fn closing_the_browser_during_a_dialog_does_not_hang() {
    // A short close timeout, because this test is *about* the detach path: the
    // page is parked in Rust and cannot be joined, so `close` must give up on
    // it rather than wait out the (deliberately long) dialog timeout.
    let browser = Browser::new(BrowserOptions {
        close_timeout: Duration::from_millis(500),
        ..test_options()
    })
    .unwrap();
    let page = browser
        .default_context()
        .new_page(NewPageOptions {
            dialog_policy: Some(DialogPolicy::Ask {
                timeout: Duration::from_secs(30),
            }),
            ..NewPageOptions::default()
        })
        .unwrap();

    let loader = {
        let page = page.clone();
        std::thread::spawn(move || {
            let _ = page.set_content("<script>alert('parked forever');</script>");
        })
    };
    std::thread::sleep(Duration::from_millis(150));

    // The answer channel's sender lives on the handle; dropping every handle
    // is what disconnects it. `close()` must not wait on the parked thread
    // beyond its bound.
    let started = Instant::now();
    browser.close();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "closing must be bounded even with a page parked on a dialog, took {:?}",
        started.elapsed()
    );

    // The page thread is left to finish on its own; do not join it here.
    drop(loader);
}
