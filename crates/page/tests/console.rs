//! ADR-0025: the parts of the console and script-error streams that need the
//! page — real script URLs, the event loop's error kinds, and the stream
//! bounds.
//!
//! Scripts are served from a loopback server on `127.0.0.1:0` (CI never
//! touches the internet), because a *location* is only meaningful when the
//! script has a URL of its own.

use std::time::Duration;

use oxidepage_page::{
    ConsoleLevel, MAX_CONSOLE_MESSAGES, MAX_SCRIPT_ERRORS, Page, PageOptions, ResourcePolicy,
    ScriptErrorKind, WaitUntil,
};
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
    let js = "text/javascript";
    let html = "text/html";
    match path {
        "/page.html" => resp(
            200,
            "OK",
            html,
            "<!doctype html><title>t</title><script src='/app.js'></script>",
        ),
        "/app.js" => resp(
            200,
            "OK",
            js,
            "function speak(){ console.log('from', 'the script', {a: 1}) }\nspeak();",
        ),
        "/throwing.html" => resp(
            200,
            "OK",
            html,
            "<!doctype html><title>t</title><script src='/throw.js'></script>",
        ),
        "/throw.js" => resp(
            200,
            "OK",
            js,
            "function inner(){ throw new TypeError('deep boom') }\n\
             function outer(){ inner() }\n\
             outer();",
        ),
        "/missing.css" => resp(404, "Not Found", "text/css", "nope"),
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

#[test]
fn a_console_call_reports_the_script_it_came_from() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/page.html"),
        WaitUntil::Load,
    )
    .unwrap();

    let console = page.drain_console();
    let message = console
        .iter()
        .find(|m| m.message.starts_with("from the script"))
        .unwrap_or_else(|| panic!("no console line, got {console:?}"));
    assert_eq!(message.level, ConsoleLevel::Log);
    // The object argument renders structurally now, not as `[object Object]`.
    assert_eq!(message.message, "from the script {a: 1}");
    assert_eq!(message.args.len(), 3);

    let at = message.location.as_ref().expect("a call-site location");
    assert_eq!(at.url, format!("http://127.0.0.1:{port}/app.js"));
    assert_eq!(at.function.as_deref(), Some("speak"));
    assert!(at.line >= 1);
    assert!(message.timestamp > 0.0);
}

#[test]
fn an_uncaught_exception_is_structured_with_its_frames() {
    let port = spawn_server();
    let page = loopback_page();
    page.navigate(
        &format!("http://127.0.0.1:{port}/throwing.html"),
        WaitUntil::Load,
    )
    .unwrap();

    let errors = page.drain_errors();
    let error = errors
        .iter()
        .find(|e| e.message == "deep boom")
        .unwrap_or_else(|| panic!("expected the throw to be reported, got {errors:?}"));
    assert_eq!(error.kind, ScriptErrorKind::Uncaught);
    assert_eq!(error.name.as_deref(), Some("TypeError"));
    let functions: Vec<_> = error
        .stack
        .iter()
        .map(|f| f.function.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(functions, ["inner", "outer", "<eval>"]);
    assert_eq!(error.location().unwrap().function.as_deref(), Some("inner"));
    assert_eq!(
        error.location().unwrap().url,
        format!("http://127.0.0.1:{port}/throw.js")
    );
    // `Display` stays a single readable line.
    assert_eq!(error.to_string(), "TypeError: deep boom");
}

#[test]
fn a_callback_exception_is_kind_callback() {
    let page = oxidepage_page::load_html_page(
        "<button id='b'></button>
         <script>
           document.getElementById('b').addEventListener('click', () => {
             throw new RangeError('listener boom');
           });
           setTimeout(() => { throw new Error('timer boom'); }, 0);
         </script>",
        PageOptions::default(),
    )
    .unwrap();
    page.eval_to_string("document.getElementById('b').click(), ''")
        .unwrap();
    page.settle(Duration::from_secs(1));

    let errors = page.drain_errors();
    let listener = errors
        .iter()
        .find(|e| e.message == "listener boom")
        .unwrap_or_else(|| panic!("got {errors:?}"));
    assert_eq!(listener.kind, ScriptErrorKind::Callback);
    assert_eq!(listener.name.as_deref(), Some("RangeError"));
    assert!(!listener.stack.is_empty());
    assert_eq!(
        errors
            .iter()
            .find(|e| e.message == "timer boom")
            .map(|e| e.kind),
        Some(ScriptErrorKind::Callback)
    );
}

#[test]
fn a_resource_failure_has_no_stack() {
    let port = spawn_server();
    let page = loopback_page();
    page.load_html(&format!(
        "<link rel='stylesheet' href='http://127.0.0.1:{port}/missing.css'>"
    ))
    .unwrap();
    page.settle(Duration::from_secs(5));

    let errors = page.drain_errors();
    let error = errors
        .iter()
        .find(|e| e.message.contains("missing.css"))
        .unwrap_or_else(|| panic!("got {errors:?}"));
    assert_eq!(error.kind, ScriptErrorKind::Resource);
    assert_eq!(error.name, None);
    assert!(error.stack.is_empty());
    assert!(error.location().is_none());
}

#[test]
fn the_script_budget_abort_names_the_loop() {
    let page = oxidepage_page::load_html_page(
        "<script>function spin(){ for (;;) {} }\nspin();</script>",
        PageOptions {
            script_budget: Some(Duration::from_millis(50)),
            ..PageOptions::default()
        },
    )
    .unwrap();
    let errors = page.drain_errors();
    let error = errors
        .iter()
        .find(|e| e.kind == ScriptErrorKind::ScriptBudget)
        .unwrap_or_else(|| panic!("got {errors:?}"));
    assert!(error.message.contains("50 ms execution budget"));
    // Not the engine's opaque placeholder: the abort is not an `InternalError`.
    assert_eq!(error.name, None);
    assert_eq!(
        error.to_string(),
        "script exceeded the 50 ms execution budget"
    );
    // New information: the aborted script's own frames name what looped.
    assert_eq!(
        error.stack.first().unwrap().function.as_deref(),
        Some("spin")
    );
}

#[test]
fn the_console_stream_is_bounded_and_keeps_the_newest() {
    let page = oxidepage_page::load_html_page(
        "<script>for (let i = 0; i < 1200; i++) console.log('line ' + i);</script>",
        PageOptions::default(),
    )
    .unwrap();
    let console = page.drain_console();
    assert_eq!(console.len(), MAX_CONSOLE_MESSAGES);
    assert_eq!(console.last().unwrap().message, "line 1199");
    assert_eq!(console.first().unwrap().message, "line 176");
}

#[test]
fn the_error_stream_is_bounded_and_keeps_the_newest() {
    // A listener that throws every time: 1200 reported errors, none of which
    // stops the dispatch loop.
    let page = oxidepage_page::load_html_page(
        "<script>
           globalThis.n = 0;
           addEventListener('boom', function () { throw new Error('e ' + n++); });
           for (let i = 0; i < 1200; i++) window.dispatchEvent(new Event('boom'));
         </script>",
        PageOptions::default(),
    )
    .unwrap();
    let errors = page.drain_errors();
    assert_eq!(errors.len(), MAX_SCRIPT_ERRORS);
    assert_eq!(errors.last().unwrap().message, "e 1199");
}

/// Rejections wait in their own buffer until `drain_errors` (the last moment a
/// handler could attach), so that buffer needs the same bound as the rest.
#[test]
fn pending_rejections_are_bounded() {
    let page = oxidepage_page::load_html_page(
        "<script>for (let i = 0; i < 1200; i++) Promise.reject(new Error('r ' + i));</script>",
        PageOptions::default(),
    )
    .unwrap();
    page.settle(Duration::from_secs(1));
    let errors = page.drain_errors();
    assert_eq!(errors.len(), MAX_SCRIPT_ERRORS);
    assert!(
        errors
            .iter()
            .all(|e| e.kind == ScriptErrorKind::UnhandledRejection)
    );
    assert_eq!(errors.last().unwrap().message, "r 1199");
}

/// A navigation must not erase the errors and console output that explain it.
#[test]
fn the_streams_survive_a_navigation() {
    let page = oxidepage_page::load_html_page(
        "<script>console.log('doc 1'); throw new Error('doc 1 broke');</script>",
        PageOptions::default(),
    )
    .unwrap();
    page.load_html("<script>console.log('doc 2');</script>")
        .unwrap();

    let console = page.drain_console();
    assert_eq!(
        console
            .iter()
            .map(|m| m.message.as_str())
            .collect::<Vec<_>>(),
        ["doc 1", "doc 2"]
    );
    let errors = page.drain_errors();
    assert!(
        errors.iter().any(|e| e.message == "doc 1 broke"),
        "got {errors:?}"
    );
}
