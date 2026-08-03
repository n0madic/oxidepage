//! A download is a navigation that does not commit (ADR-0032 D13).
//!
//! The failure this replaces was silent and bad: a `Content-Disposition:
//! attachment` response was decoded as UTF-8 and handed to the HTML parser, so
//! navigating to a PDF replaced the document with the parser's reading of a
//! binary file.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use oxidepage_page::{
    DownloadBehavior, DownloadEvent, DownloadState, Page, PageOptions, PageRecord, ResourcePolicy,
    WaitUntil,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback server whose `/download*` routes answer with an attachment.
fn spawn_server() -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
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
                    let mut tmp = [0u8; 2048];
                    let read = sock.read(&mut tmp).await.unwrap_or(0);
                    let path = String::from_utf8_lossy(&tmp[..read])
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_owned();
                    let (disposition, body) = match path.as_str() {
                        "/report.csv" => (
                            Some("attachment; filename=\"report.csv\""),
                            "a,b\n1,2\n".to_owned(),
                        ),
                        // A hostile filename: separators must never survive.
                        "/evil" => (
                            Some("attachment; filename=\"../../etc/passwd\""),
                            String::from("pwned"),
                        ),
                        // RFC 6266 `filename*`, which wins over `filename`.
                        "/utf8" => (
                            Some(
                                "attachment; filename=\"fallback.txt\"; \
                                 filename*=UTF-8''na%C3%AFve%20r%C3%A9sum%C3%A9.txt",
                            ),
                            String::from("unicode"),
                        ),
                        // No filename at all: derived from the URL path.
                        "/unnamed.bin" => (Some("attachment"), String::from("bytes")),
                        // `inline` is *not* a download.
                        "/inline.html" => (
                            Some("inline; filename=\"page.html\""),
                            String::from("<title>inline</title>"),
                        ),
                        _ => (None, String::from("<title>doc</title>")),
                    };
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    if let Some(disposition) = disposition {
                        head.push_str(&format!("Content-Disposition: {disposition}\r\n"));
                    }
                    head.push_str("\r\n");
                    head.push_str(&body);
                    let _ = sock.write_all(head.as_bytes()).await;
                });
            }
        });
    });
    rx.recv().expect("server failed to start")
}

fn page_with(download_path: Option<std::path::PathBuf>) -> Page {
    Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        download_path,
        ..PageOptions::default()
    })
    .unwrap()
}

/// Records every download event the page emits.
fn watch(page: &Page) -> Rc<RefCell<Vec<DownloadEvent>>> {
    let seen: Rc<RefCell<Vec<DownloadEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    page.set_event_sink(Some(Rc::new(move |record: PageRecord| {
        if let PageRecord::Download(event) = record {
            sink.borrow_mut().push(event);
        }
    })));
    seen
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "oxidepage-dl-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

#[test]
fn an_attachment_does_not_commit_a_document() {
    let port = spawn_server();
    let directory = temp_dir("commit");
    let page = page_with(Some(directory.clone()));

    page.navigate(&format!("http://127.0.0.1:{port}/a.html"), WaitUntil::Load)
        .expect("first navigation");
    let before = page.dom().document_url().to_owned();

    page.navigate(
        &format!("http://127.0.0.1:{port}/report.csv"),
        WaitUntil::Load,
    )
    .expect("the download navigation answers");

    // The document *stays* — what a browser does. Parsing the attachment as
    // HTML is the bug this replaces.
    assert_eq!(
        page.dom().document_url(),
        before,
        "a download must not replace the document"
    );
    assert_eq!(
        page.eval_to_string("document.title").unwrap(),
        "doc",
        "and the old document is still the live one"
    );

    let written = std::fs::read_to_string(directory.join("report.csv")).expect("the download");
    assert_eq!(written, "a,b\n1,2\n");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_download_reports_a_begin_and_an_end() {
    let port = spawn_server();
    let directory = temp_dir("events");
    let page = page_with(Some(directory.clone()));
    let seen = watch(&page);

    page.navigate(
        &format!("http://127.0.0.1:{port}/report.csv"),
        WaitUntil::Load,
    )
    .expect("navigation");

    let events = seen.borrow().clone();
    assert_eq!(events.len(), 2, "one begin, one end: {events:?}");
    assert_eq!(events[0].state, DownloadState::InProgress);
    assert_eq!(events[0].suggested_filename, "report.csv");
    assert!(events[0].path.is_none(), "the begin knows no path yet");
    assert_eq!(events[1].state, DownloadState::Completed);
    assert_eq!(
        events[0].guid, events[1].guid,
        "a driver pairs the two by guid"
    );
    assert!(
        events[1].path.as_deref().unwrap().ends_with("report.csv"),
        "the end carries where it landed: {:?}",
        events[1].path
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn deny_refuses_the_navigation_and_still_reports_it() {
    let port = spawn_server();
    // No download path: deny, which is the default.
    let page = page_with(None);
    let seen = watch(&page);
    assert_eq!(page.download_behavior(), DownloadBehavior::Deny);

    page.navigate(
        &format!("http://127.0.0.1:{port}/report.csv"),
        WaitUntil::Load,
    )
    .expect("the navigation answers rather than erroring");

    // Refused, but *recorded*: a driver that asked for a download and got
    // silence cannot tell a refusal from a broken link.
    let events = seen.borrow().clone();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].state, DownloadState::Canceled);
    assert!(events[1].path.is_none());

    // And nothing was parsed: the page never left `about:blank`.
    assert_eq!(page.dom().document_url(), "about:blank");
}

#[test]
fn a_traversing_filename_cannot_escape_the_directory() {
    // The filename is attacker-controlled. `net::sanitize_filename` strips every
    // separator at the parse, and `write_download` re-checks the joined path.
    let port = spawn_server();
    let directory = temp_dir("evil");
    let page = page_with(Some(directory.clone()));
    let seen = watch(&page);

    page.navigate(&format!("http://127.0.0.1:{port}/evil"), WaitUntil::Load)
        .expect("navigation");

    let events = seen.borrow().clone();
    assert_eq!(events[1].state, DownloadState::Completed, "{events:?}");
    let written = std::path::Path::new(events[1].path.as_deref().unwrap());
    // Compared against the *canonical* directory: macOS resolves `/var` to
    // `/private/var`, and the write path canonicalizes while the test's own
    // `temp_dir` does not.
    let canonical = directory.canonicalize().expect("canonical temp dir");
    assert!(
        written.starts_with(&canonical),
        "`{}` escaped `{}`",
        written.display(),
        canonical.display()
    );
    assert_eq!(
        written.file_name().and_then(std::ffi::OsStr::to_str),
        Some("....etcpasswd"),
        "every separator must be stripped, not honoured"
    );
    assert!(
        !canonical
            .join("..")
            .join("..")
            .join("etc")
            .join("passwd")
            .exists()
            || written.parent() == Some(canonical.as_path()),
        "nothing was written outside the directory"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_existing_file_is_never_overwritten() {
    let port = spawn_server();
    let directory = temp_dir("collide");
    std::fs::write(directory.join("report.csv"), b"do not clobber me").unwrap();
    let page = page_with(Some(directory.clone()));
    let seen = watch(&page);

    page.navigate(
        &format!("http://127.0.0.1:{port}/report.csv"),
        WaitUntil::Load,
    )
    .expect("navigation");

    assert_eq!(
        std::fs::read_to_string(directory.join("report.csv")).unwrap(),
        "do not clobber me",
        "a page must not be able to replace an earlier download by naming it"
    );
    let written = seen.borrow()[1].path.clone().unwrap();
    assert!(written.ends_with("report (1).csv"), "got {written}");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_extended_filename_wins_and_is_percent_decoded() {
    let port = spawn_server();
    let directory = temp_dir("utf8");
    let page = page_with(Some(directory.clone()));
    let seen = watch(&page);

    page.navigate(&format!("http://127.0.0.1:{port}/utf8"), WaitUntil::Load)
        .expect("navigation");

    // RFC 6266: `filename*` wins where both are present.
    assert_eq!(seen.borrow()[0].suggested_filename, "naïve résumé.txt");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_unnamed_attachment_takes_its_name_from_the_url() {
    let port = spawn_server();
    let directory = temp_dir("unnamed");
    let page = page_with(Some(directory.clone()));
    let seen = watch(&page);

    page.navigate(
        &format!("http://127.0.0.1:{port}/unnamed.bin"),
        WaitUntil::Load,
    )
    .expect("navigation");

    assert_eq!(seen.borrow()[0].suggested_filename, "unnamed.bin");
    assert!(directory.join("unnamed.bin").exists());
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_inline_disposition_is_an_ordinary_document() {
    // `Content-Disposition: inline` is not a download, and treating any
    // `Content-Disposition` as one would break every server that sets it to
    // name a file without asking for a save.
    let port = spawn_server();
    let directory = temp_dir("inline");
    let page = page_with(Some(directory.clone()));
    let seen = watch(&page);

    page.navigate(
        &format!("http://127.0.0.1:{port}/inline.html"),
        WaitUntil::Load,
    )
    .expect("navigation");

    assert!(seen.borrow().is_empty(), "inline is not a download");
    assert_eq!(page.eval_to_string("document.title").unwrap(), "inline");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn set_download_behavior_switches_a_live_page() {
    let port = spawn_server();
    let directory = temp_dir("switch");
    let page = page_with(None);
    let seen = watch(&page);

    page.navigate(
        &format!("http://127.0.0.1:{port}/report.csv"),
        WaitUntil::Load,
    )
    .expect("navigation");
    assert_eq!(seen.borrow()[1].state, DownloadState::Canceled);

    page.set_download_behavior(DownloadBehavior::Allow(directory.clone()));
    page.navigate(
        &format!("http://127.0.0.1:{port}/report.csv"),
        WaitUntil::Load,
    )
    .expect("navigation");
    assert_eq!(seen.borrow()[3].state, DownloadState::Completed);

    page.settle(Duration::from_millis(50));
    assert!(directory.join("report.csv").exists());
    let _ = std::fs::remove_dir_all(&directory);
}
