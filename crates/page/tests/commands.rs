//! The embedder command port (ADR-0027 D2–D5): a page driven from another
//! thread over a channel of [`PageJob`]s.
//!
//! Two properties matter here and are asserted directly rather than inferred:
//!
//! 1. **A command is answered while the page is mid-`settle`.** That is the
//!    whole point of the port — a protocol server must be able to talk to a
//!    page that is busy, not only to one that has gone quiet.
//! 2. **The single blocking wait survives.** ADR-0004's property is that the
//!    loop parks *once* per iteration and never spins. A `Select` over two
//!    channels is easy to get wrong in exactly that direction — a disconnected
//!    receiver is permanently ready — so the counters are asserted, not the
//!    wall clock, which would be flaky and would not distinguish "parked" from
//!    "spun quickly".

use std::sync::mpsc;
use std::time::{Duration, Instant};

use oxidepage_page::{
    EvaluateOptions, EvaluateOutcome, Page, PageJob, PageOptions, ResourcePolicy, WaitUntil,
    load_html_page,
};

/// Runs `page` on this thread from a fresh port, handing the sender back so the
/// test thread can drive it.
fn spawn_loop(
    html: &'static str,
) -> (
    crossbeam_channel::Sender<PageJob>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = crossbeam_channel::unbounded::<PageJob>();
    let handle = std::thread::spawn(move || {
        let page = load_html_page(html, PageOptions::default()).expect("page");
        page.run_command_loop(rx);
    });
    await_running(&tx);
    (tx, handle)
}

/// Blocks until the page thread is actually running its command loop.
///
/// **Every stopwatch in this file depends on it.** Building a `Page` scans the
/// system fonts, which costs well over a second on the first page in a process
/// (the scan is cached process-wide afterwards, so which test pays depends on
/// which one ran first — that is where the intermittency came from). Jobs sent
/// before the loop starts sit in the channel, so a test that starts timing in
/// the meantime measures `Page::new`, not command latency: the control-job
/// assertion below was really asserting "the font scan finishes within 1.5 s",
/// and failed whenever it did not. The page itself answers a control job ~15 ms
/// after its loop starts, mid-navigation, every time.
fn await_running(tx: &crossbeam_channel::Sender<PageJob>) {
    call(tx, Duration::from_secs(60), |_| ());
}

/// Sends a job and waits for its answer, failing the test rather than hanging.
fn call<T: Send + 'static>(
    tx: &crossbeam_channel::Sender<PageJob>,
    timeout: Duration,
    f: impl FnOnce(&Page) -> T + Send + 'static,
) -> T {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(PageJob::new(move |page| {
        let _ = reply_tx.send(f(page));
    }))
    .expect("page thread alive");
    reply_rx
        .recv_timeout(timeout)
        .expect("page answered within the timeout")
}

#[test]
fn a_job_runs_on_the_page_thread_and_returns_an_owned_projection() {
    let (tx, handle) = spawn_loop("<title>hello</title><p id=x>text</p>");
    // `dom()` hands back a `Ref<'_, DomTree>` — it cannot cross a channel, and
    // does not have to: the closure runs where the borrow is legal and only the
    // `String` travels.
    let title = call(&tx, Duration::from_secs(5), |page| {
        page.dom().document_url().to_owned()
    });
    assert!(!title.is_empty());
    let two = call(&tx, Duration::from_secs(5), |page| {
        page.eval_to_string("1 + 1").unwrap()
    });
    assert_eq!(two, "2");

    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

#[test]
fn a_command_is_answered_while_the_page_is_mid_settle() {
    let (tx, handle) = spawn_loop(
        r"<script>
          window.ticks = 0;
          (function tick() {
            window.ticks++;
            setTimeout(tick, 20);
          })();
        </script>",
    );

    // Ask the page — which is chewing through an endless 20 ms timer chain — to
    // answer a question. The chain never ends, so an implementation that only
    // serviced commands at idle would never answer at all; the bound is
    // generous on purpose, because what is being asserted is "does not wait for
    // the workload", not a scheduling latency.
    let started = Instant::now();
    let answer = call(&tx, Duration::from_secs(5), |page| {
        page.eval_to_string("1 + 1").unwrap()
    });
    let elapsed = started.elapsed();
    assert_eq!(answer, "2");
    assert!(
        elapsed < Duration::from_secs(2),
        "a busy page must still answer, took {elapsed:?}"
    );

    // The timer chain kept running across the command, so the page really was
    // busy rather than merely idle-looking.
    let ticks = call(&tx, Duration::from_secs(5), |page| {
        page.eval_to_string("String(window.ticks)").unwrap()
    });
    assert!(
        ticks.parse::<u32>().unwrap() >= 1,
        "the page's own work must continue across a command, got {ticks}"
    );

    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

#[test]
fn an_idle_page_parks_instead_of_spinning() {
    let (tx, handle) = spawn_loop("<p>quiet</p>");

    // Let the page reach quiescence and note where the counters stand.
    let before = call(&tx, Duration::from_secs(5), Page::loop_stats);
    std::thread::sleep(Duration::from_millis(300));
    let after = call(&tx, Duration::from_secs(5), Page::loop_stats);

    // 300 ms of nothing to do: with an indefinite park the loop makes a
    // handful of turns (the two commands themselves), and a spin would make
    // many thousands. The bound is deliberately loose — it is there to catch a
    // busy-wait, not to pin an exact iteration count.
    assert!(
        after.blocking_waits - before.blocking_waits <= 4,
        "an idle page must park, not spin: {} extra waits",
        after.blocking_waits - before.blocking_waits
    );
    assert!(
        after.turns - before.turns <= 40,
        "an idle page must not churn the loop: {} extra turns",
        after.turns - before.turns
    );

    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

/// A page holding a deferred await still **parks** (ADR-0034 D1).
///
/// The drain is a task source, and a task source that always reports progress
/// turns the loop into a spin — which is exactly the busy-wait ADR-0004 exists
/// to forbid, and it would be visible only as a pegged core. A parked await is
/// not progress until its promise settles or its budget runs out.
#[test]
fn a_page_with_a_deferred_await_still_parks() {
    let (tx, handle) = spawn_loop("<p>quiet</p>");

    let deferred = call(&tx, Duration::from_secs(5), |page| {
        // A sink first: a token nobody can answer is never issued, so without
        // one `defer_await` falls back to the blocking path and this would
        // block for the whole await budget instead of deferring.
        page.set_event_sink(Some(std::rc::Rc::new(|_record| {})));
        matches!(
            page.evaluate(
                "new Promise(() => {})",
                &EvaluateOptions {
                    await_promise: true,
                    defer_await: true,
                    ..EvaluateOptions::default()
                },
            ),
            EvaluateOutcome::Deferred(_)
        )
    });
    assert!(deferred, "a promise nothing resolves must defer");

    let before = call(&tx, Duration::from_secs(5), Page::loop_stats);
    std::thread::sleep(Duration::from_millis(300));
    let after = call(&tx, Duration::from_secs(5), Page::loop_stats);

    assert!(
        after.blocking_waits - before.blocking_waits <= 4,
        "a page holding a deferred await must park, not spin: {} extra waits",
        after.blocking_waits - before.blocking_waits
    );
    assert!(
        after.turns - before.turns <= 40,
        "a deferred await must not churn the loop: {} extra turns",
        after.turns - before.turns
    );

    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

#[test]
fn settle_with_a_far_timer_still_parks_once_per_wait() {
    let (tx, handle) = spawn_loop("<script>setTimeout(() => { window.done = 1; }, 300);</script>");

    let stats = call(&tx, Duration::from_secs(5), |page| {
        let before = page.loop_stats();
        page.settle(Duration::from_millis(600));
        let after = page.loop_stats();
        let done = page.eval_to_string("String(window.done)").unwrap();
        (before, after, done)
    });
    let (before, after, done) = stats;
    assert_eq!(done, "1", "the timer must have fired");
    assert!(
        after.blocking_waits - before.blocking_waits <= 4,
        "settle over a 300 ms timer must park a couple of times, not spin: {} waits",
        after.blocking_waits - before.blocking_waits
    );

    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

#[test]
fn a_disconnected_port_ends_the_loop_without_spinning() {
    let (tx, rx) = crossbeam_channel::unbounded::<PageJob>();
    let (stats_tx, stats_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let page = load_html_page("<p>x</p>", PageOptions::default()).expect("page");
        page.run_command_loop(rx);
        // A disconnected receiver is permanently *ready* in a `Select`. If the
        // loop re-selected it instead of returning, this count would be
        // enormous — that is the regression this test exists for.
        let _ = stats_tx.send(page.loop_stats());
    });

    // Let the page settle, then drop the only sender.
    std::thread::sleep(Duration::from_millis(50));
    drop(tx);

    handle.join().unwrap();
    let stats = stats_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        stats.blocking_waits <= 8,
        "dropping the port must end the loop, not spin it: {} waits",
        stats.blocking_waits
    );
}

/// A loopback server that answers `/slow.css` only after `delay`, so a page
/// loading it is genuinely parked inside `await_pending_stylesheets` — the
/// nested wait point where an ordinary job must *not* be executed.
fn spawn_slow_css_server(delay: Duration) -> u16 {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (port_tx, port_rx) = mpsc::channel();
    std::thread::spawn(move || {
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 2048];
                    if sock.read(&mut buf).await.is_err() {
                        return;
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_owned();
                    let (ctype, body) = match path.as_str() {
                        "/slow.css" => {
                            tokio::time::sleep(delay).await;
                            ("text/css", "p { color: red }".to_owned())
                        }
                        _ => (
                            "text/html",
                            "<link rel=stylesheet href=/slow.css><script>window.loaded = 1;</script>"
                                .to_owned(),
                        ),
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    port_rx.recv_timeout(Duration::from_secs(5)).unwrap()
}

fn permissive_options() -> PageOptions {
    PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    }
}

/// A page thread in the shape a driver builds one: construct, then hand the
/// whole rest of its life to the command loop. Every subsequent action —
/// navigation included — arrives as a job.
fn spawn_driven_page() -> (
    crossbeam_channel::Sender<PageJob>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = crossbeam_channel::unbounded::<PageJob>();
    let handle = std::thread::spawn(move || {
        let page = Page::new(permissive_options()).expect("page");
        page.run_command_loop(rx);
    });
    await_running(&tx);
    (tx, handle)
}

#[test]
fn a_job_sent_during_a_navigation_runs_after_it_rather_than_panicking() {
    let port = spawn_slow_css_server(Duration::from_millis(400));
    let (tx, handle) = spawn_driven_page();

    // Job one: a navigation whose render-blocking stylesheet parks the load
    // inside `await_pending_stylesheets` — a nested wait point that holds
    // parser handles and DOM/style borrows.
    let (nav_tx, nav_rx) = mpsc::channel();
    let url = format!("http://127.0.0.1:{port}/");
    tx.send(PageJob::new(move |page| {
        let _ = nav_tx.send(page.navigate(&url, WaitUntil::Load).is_ok());
    }))
    .unwrap();

    // Job two, sent while job one is parked in that wait. Reaching into JS and
    // the DOM there is a guaranteed `BorrowMutError`; parking it is the
    // contract.
    std::thread::sleep(Duration::from_millis(120));
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(PageJob::new(move |page| {
        let _ = reply_tx.send(page.eval_to_string("String(window.loaded)").unwrap());
    }))
    .unwrap();

    assert!(
        nav_rx.recv_timeout(Duration::from_secs(20)).unwrap(),
        "the navigation must succeed"
    );
    let answer = reply_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the parked job must run once the load finishes");
    assert_eq!(
        answer, "1",
        "a job parked during a navigation must observe the finished document"
    );

    let stats = call(&tx, Duration::from_secs(5), Page::loop_stats);
    assert_eq!(
        stats.jobs_deferred, 1,
        "the job must have been parked, not run under the load's borrows"
    );

    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

#[test]
fn a_control_job_is_answered_during_a_navigation() {
    // A long stylesheet delay so the gap between "answered during the load" and
    // "answered after it" is decisive rather than a race with the scheduler.
    let port = spawn_slow_css_server(Duration::from_secs(3));
    let (tx, handle) = spawn_driven_page();

    let (nav_tx, nav_rx) = mpsc::channel();
    let url = format!("http://127.0.0.1:{port}/");
    tx.send(PageJob::new(move |page| {
        let _ = nav_tx.send(page.navigate(&url, WaitUntil::Load).is_ok());
    }))
    .unwrap();

    std::thread::sleep(Duration::from_millis(120));
    let started = Instant::now();
    let (reply_tx, reply_rx) = mpsc::channel();
    // A control job touches `Cell`s only, so it runs at the wait point that
    // receives it — even one nested inside `load_document`.
    tx.send(PageJob::control(move |page| {
        let _ = reply_tx.send(page.is_closing());
    }))
    .unwrap();

    let answered = reply_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("a control job must be answered");
    assert!(!answered);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "a control job must not wait for the navigation to finish, waited {:?}",
        started.elapsed()
    );

    assert!(nav_rx.recv_timeout(Duration::from_secs(20)).unwrap());
    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

/// `suspend()` freezes the page's **own** sources, not the driver's turn
/// (ADR-0034 D3).
///
/// This test used to assert the opposite — that an ordinary job parks until
/// `resume()` — which was the semantics before `waitForDebuggerOnStart` became
/// a real pause. It kept passing only by a race: it gave the page 300 ms to not
/// answer, and on a cold machine `load_html_page` alone took longer than that.
/// On a warm one the job was answered inside the window and the assertion
/// failed, which is what made it a CI flake rather than a caught regression.
#[test]
fn a_suspended_page_serves_jobs_but_freezes_its_own_sources() {
    let (tx, rx) = crossbeam_channel::unbounded::<PageJob>();
    let handle = std::thread::spawn(move || {
        let page = load_html_page("<p>x</p>", PageOptions::default()).expect("page");
        page.suspend();
        page.run_command_loop(rx);
    });

    // An ordinary job runs *while suspended*, and that is the whole point: a
    // driver sends its entire session setup before `runIfWaitingForDebugger`,
    // so deferring it would deadlock the setup the pause exists to allow. The
    // generous timeout is `await_running`'s, for its reason — the first `Page`
    // in the process pays for the system font scan.
    let armed = call(&tx, Duration::from_secs(60), |page| {
        page.eval_to_string(
            "window.fired = false; setTimeout(() => { window.fired = true; }, 0); 40 + 2",
        )
        .unwrap()
    });
    assert_eq!(armed, "42", "a suspended page must still serve the driver");

    // The page's own scheduling stays frozen, though: the timer that job armed
    // does not fire. Read through another job, which is the only thing that
    // *does* run here — so this is not a "wait and hope" window.
    std::thread::sleep(Duration::from_millis(200));
    let fired = call(&tx, Duration::from_secs(5), |page| {
        page.eval_to_string("window.fired").unwrap()
    });
    assert_eq!(fired, "false", "a suspended page ran a timer");

    // …and it fires once the pause is lifted. Polled rather than slept on: the
    // assertion is "it eventually runs", and a fixed sleep would only make that
    // flaky under load.
    tx.send(PageJob::control(Page::resume)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut fired = String::new();
    while Instant::now() < deadline {
        fired = call(&tx, Duration::from_secs(5), |page| {
            page.eval_to_string("window.fired").unwrap()
        });
        if fired == "true" {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fired, "true", "the timer must run once the page resumes");

    tx.send(PageJob::control(Page::request_close)).unwrap();
    handle.join().unwrap();
}

/// Without an event sink, `defer_await` falls back to the blocking path rather
/// than issuing a token nobody can answer (ADR-0034 D1).
///
/// `LoopHooks::emit` drops the record when no sink is installed, so a pull-API
/// embedder would get a `Deferred` that can never resolve — and the parked
/// entry would report an elapsed deadline forever, which `next_wakeup` turns
/// into a park on a past instant: the ADR-0004 busy-wait, reached through the
/// feature meant to prevent a deadlock.
#[test]
fn deferring_without_a_sink_falls_back_to_blocking() {
    let page = load_html_page("<p>x</p>", PageOptions::default()).expect("page");
    assert!(!page.has_event_sink());

    let before = page.loop_stats();
    let outcome = page.evaluate(
        "Promise.resolve(7)",
        &EvaluateOptions {
            by_value: true,
            await_promise: true,
            defer_await: true,
            ..EvaluateOptions::default()
        },
    );
    // Answered, not deferred — and answered *correctly*, which is the point of
    // falling back rather than refusing.
    let result = outcome.expect_done();
    assert_eq!(result.result.value_json.as_deref(), Some("7"));

    // And no entry was parked to spin on: the loop is not now waking on an
    // elapsed deadline.
    let after = page.loop_stats();
    assert!(
        after.turns - before.turns < 100,
        "the fallback must not leave the loop spinning: {} turns",
        after.turns - before.turns
    );
}
