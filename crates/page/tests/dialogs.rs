//! Stage 3 verification (ADR-0025): `window.alert`/`confirm`/`prompt` exist,
//! answer through the embedder handler, pause the page while they are open,
//! and land in a bounded `DialogEvent` stream.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use oxidepage_page::{
    DialogHandler, DialogKind, DialogRequest, DialogResponse, Page, PageOptions, load_html_page,
};

fn eval_string(page: &Page, source: &str) -> String {
    page.eval_to_string(source).expect("eval")
}

/// A handler that answers everything the same way and records what it saw.
fn scripted(response: DialogResponse) -> (DialogHandler, Rc<RefCell<Vec<DialogRequest>>>) {
    let seen: Rc<RefCell<Vec<DialogRequest>>> = Rc::default();
    let sink = Rc::clone(&seen);
    let handler: DialogHandler = Rc::new(move |request: &DialogRequest| {
        sink.borrow_mut().push(request.clone());
        response.clone()
    });
    (handler, seen)
}

#[test]
fn the_default_policy_dismisses_every_dialog() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    // The point of the whole feature: a page that calls these keeps running
    // instead of dying on a `ReferenceError`.
    assert_eq!(eval_string(&page, "typeof alert"), "function");
    assert_eq!(eval_string(&page, "String(alert('hi'))"), "undefined");
    assert_eq!(eval_string(&page, "String(confirm('sure?'))"), "false");
    assert_eq!(eval_string(&page, "String(prompt('name?'))"), "null");
    assert!(page.drain_errors().is_empty());
}

#[test]
fn a_handler_answers_all_three_kinds() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();

    let (handler, _) = scripted(DialogResponse::Accept);
    page.set_dialog_handler(Some(handler));
    assert_eq!(eval_string(&page, "String(confirm('sure?'))"), "true");
    // Accepting without typing keeps the page's own default text.
    assert_eq!(eval_string(&page, "prompt('name?', 'ada')"), "ada");
    assert_eq!(eval_string(&page, "prompt('name?')"), "");

    let (handler, _) = scripted(DialogResponse::AcceptWith("typed".into()));
    page.set_dialog_handler(Some(handler));
    assert_eq!(eval_string(&page, "prompt('name?', 'ada')"), "typed");
    // `confirm` ignores the text; accepting is accepting.
    assert_eq!(eval_string(&page, "String(confirm('sure?'))"), "true");

    // Removing the handler restores auto-dismiss.
    page.set_dialog_handler(None);
    assert_eq!(eval_string(&page, "String(confirm('sure?'))"), "false");
}

#[test]
fn the_request_carries_the_kind_message_default_and_url() {
    let page = Page::new(PageOptions {
        url: Some("https://example.test/app".into()),
        ..PageOptions::default()
    })
    .unwrap();
    page.load_html("<html><body></body></html>").unwrap();
    let (handler, seen) = scripted(DialogResponse::Dismiss);
    page.set_dialog_handler(Some(handler));

    eval_string(&page, "String(alert('hello'))");
    eval_string(&page, "String(prompt('name?', 'ada'))");
    let seen = seen.borrow();
    assert_eq!(seen[0].kind, DialogKind::Alert);
    assert_eq!(seen[0].message, "hello");
    assert_eq!(seen[0].default_value, "");
    assert_eq!(seen[0].url, "https://example.test/app");
    assert_eq!(seen[1].kind, DialogKind::Prompt);
    assert_eq!(seen[1].default_value, "ada");
}

#[test]
fn the_event_stream_records_the_ask_and_the_answer() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    let (handler, _) = scripted(DialogResponse::AcceptWith("typed".into()));
    page.set_dialog_handler(Some(handler));

    eval_string(&page, "String(alert('a'))");
    eval_string(&page, "String(confirm('b'))");
    eval_string(&page, "String(prompt('c', 'def'))");

    let events = page.drain_dialog_events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].kind, DialogKind::Alert);
    assert_eq!(events[0].message, "a");
    assert_eq!(events[1].kind, DialogKind::Confirm);
    assert_eq!(
        events[2].response,
        DialogResponse::AcceptWith("typed".into())
    );
    assert_eq!(events[2].default_value, "def");
    assert!(
        events.windows(2).all(|w| w[0].timestamp <= w[1].timestamp),
        "timestamps must not go backwards: {events:?}"
    );
    // Draining is destructive.
    assert!(page.drain_dialog_events().is_empty());
}

#[test]
fn a_parse_time_dialog_needs_the_option_not_the_setter() {
    // `load_html_page` runs inline scripts *during* the call, so a
    // post-construction setter would be installed too late. This is the case
    // `PageOptions::dialog_handler` exists for.
    let (handler, seen) = scripted(DialogResponse::Accept);
    let page = load_html_page(
        "<script>globalThis.answer = confirm('during the parse');</script>",
        PageOptions {
            dialog_handler: Some(handler),
            ..PageOptions::default()
        },
    )
    .unwrap();
    assert_eq!(eval_string(&page, "String(answer)"), "true");
    assert_eq!(seen.borrow().len(), 1);
    assert_eq!(seen.borrow()[0].message, "during the parse");
}

#[test]
fn dialogs_work_from_a_timer_and_from_a_listener() {
    let (handler, seen) = scripted(DialogResponse::Accept);
    let page = load_html_page(
        "<button id='b'></button>
         <script>
           globalThis.fromTimer = null;
           globalThis.fromListener = null;
           setTimeout(() => { globalThis.fromTimer = confirm('timer'); }, 0);
           document.getElementById('b').addEventListener('click', () => {
             globalThis.fromListener = prompt('listener', 'x');
           });
         </script>",
        PageOptions {
            dialog_handler: Some(handler),
            ..PageOptions::default()
        },
    )
    .unwrap();
    page.settle(Duration::from_secs(1));
    assert_eq!(eval_string(&page, "String(fromTimer)"), "true");

    eval_string(&page, "document.getElementById('b').click(), ''");
    assert_eq!(eval_string(&page, "String(fromListener)"), "x");

    let kinds: Vec<_> = seen.borrow().iter().map(|r| r.kind).collect();
    assert_eq!(kinds, [DialogKind::Confirm, DialogKind::Prompt]);
}

/// The handler is cloned out of its slot before being called, so a handler
/// that replaces itself is legal rather than a `BorrowMutError`.
#[test]
fn a_handler_may_reinstall_itself() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    let calls: Rc<std::cell::Cell<u32>> = Rc::default();
    let counter = Rc::clone(&calls);
    let page_ref = Rc::new(page);
    let weak = Rc::downgrade(&page_ref);
    let handler: DialogHandler = Rc::new(move |_req| {
        counter.set(counter.get() + 1);
        // Replacing the handler from *inside* the handler: the page must have
        // released its borrow on the slot before calling us.
        if let Some(page) = weak.upgrade() {
            let inner = Rc::clone(&counter);
            page.set_dialog_handler(Some(Rc::new(move |_req| {
                inner.set(inner.get() + 10);
                DialogResponse::Accept
            })));
        }
        DialogResponse::Dismiss
    });
    page_ref.set_dialog_handler(Some(handler));

    assert_eq!(eval_string(&page_ref, "String(confirm('first'))"), "false");
    assert_eq!(calls.get(), 1);
    // The replacement is in force for the next dialog.
    assert_eq!(eval_string(&page_ref, "String(confirm('second'))"), "true");
    assert_eq!(calls.get(), 11);
}

/// HTML says these three "pause" the page. The handler is synchronous on the
/// page thread, so the event loop simply never regains control — no timer, no
/// frame callback, nothing can interleave.
///
/// The handler sleeps past the timer's deadline, so the timer is genuinely
/// *due* while the dialog is open: a page that resumed the loop for it would
/// be caught here.
#[test]
fn nothing_runs_while_a_dialog_is_open() {
    let handler: DialogHandler = Rc::new(|_req| {
        std::thread::sleep(Duration::from_millis(30));
        DialogResponse::Dismiss
    });
    let page = load_html_page(
        "<script>
           globalThis.ticked = false;
           setTimeout(() => { globalThis.ticked = true; }, 5);
           alert('blocking');
           globalThis.tickedDuringDialog = ticked;
         </script>",
        PageOptions {
            dialog_handler: Some(handler),
            ..PageOptions::default()
        },
    )
    .unwrap();

    assert_eq!(eval_string(&page, "String(tickedDuringDialog)"), "false");
    // And the timer does run once the page is allowed to continue.
    page.settle(Duration::from_secs(1));
    assert_eq!(eval_string(&page, "String(ticked)"), "true");
}

#[test]
fn missing_arguments_default_to_empty_strings() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    let (handler, seen) = scripted(DialogResponse::Dismiss);
    page.set_dialog_handler(Some(handler));

    eval_string(&page, "String(alert())");
    eval_string(&page, "String(prompt('q'))");
    // Unqualified calls resolve on the global (WebIDL substitutes the global
    // for a null/undefined receiver).
    eval_string(&page, "String(window.alert.call(null, 'via call'))");

    let seen = seen.borrow();
    assert_eq!(seen[0].message, "");
    assert_eq!(seen[1].default_value, "");
    assert_eq!(seen[2].message, "via call");
}

/// The documented micro-divergence (ADR-0025): the spec's two `alert`
/// overloads make Chrome print `"undefined"`, while our single
/// optional-argument signature defaults. Pinned so it cannot drift silently.
#[test]
fn alert_undefined_shows_the_default_not_the_word() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    let (handler, seen) = scripted(DialogResponse::Dismiss);
    page.set_dialog_handler(Some(handler));
    eval_string(&page, "String(alert(undefined))");
    eval_string(&page, "String(alert(null))");
    assert_eq!(seen.borrow()[0].message, "");
    // `null` is a real value and still coerces, as everywhere else.
    assert_eq!(seen.borrow()[1].message, "null");
}

#[test]
fn the_dialog_stream_is_bounded_and_keeps_the_newest() {
    let page = load_html_page("<html><body></body></html>", PageOptions::default()).unwrap();
    eval_string(
        &page,
        "for (let i = 0; i < 400; i++) confirm('dialog ' + i); ''",
    );
    let events = page.drain_dialog_events();
    assert_eq!(events.len(), oxidepage_page::MAX_DIALOG_EVENTS);
    assert_eq!(events.last().unwrap().message, "dialog 399");
}

#[test]
fn the_handler_and_the_stream_survive_a_navigation() {
    let (handler, seen) = scripted(DialogResponse::Accept);
    let page = load_html_page(
        "<script>confirm('doc 1');</script>",
        PageOptions {
            dialog_handler: Some(handler),
            ..PageOptions::default()
        },
    )
    .unwrap();
    page.load_html("<script>confirm('doc 2');</script>")
        .unwrap();
    let messages: Vec<_> = seen.borrow().iter().map(|r| r.message.clone()).collect();
    assert_eq!(messages, ["doc 1", "doc 2"]);
    let events = page.drain_dialog_events();
    assert_eq!(events.len(), 2, "a navigation must not erase the stream");
}
