//! Stage 1 verification (ADR-0022): script, links and forms navigate; there is
//! a real session history with working traversal; and the page records a
//! navigation event stream.
//!
//! Everything runs against a loopback server on `127.0.0.1:0` — CI never
//! touches the internet — which also lets the assertions be about what actually
//! reached the wire (method, query, `Content-Type`, body, `Referer`) rather
//! than about what the engine believes it sent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxidepage_page::{
    NavigationEventKind, Page, PageOptions, ResourcePolicy, WaitUntil, load_html_page,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Every request one server has served. A test asserts against this rather
/// than against page state, so a navigation that never happened cannot pass by
/// accident.
///
/// Per-server, not global: the test binary runs its tests in parallel, and a
/// shared log would let one test's traffic satisfy another's assertion.
type RequestLog = Arc<Mutex<Vec<Request>>>;

#[derive(Clone, Debug)]
struct Request {
    method: String,
    /// Path including the query string, exactly as sent.
    path: String,
    content_type: String,
    body: String,
    referer: String,
}

fn header_value(head: &str, name: &str) -> Option<String> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&prefix))
        .map(|l| l[name.len() + 1..].trim().to_owned())
}

/// A loopback server: its port and the log of what it has been asked for.
struct Server {
    port: u16,
    log: RequestLog,
}

impl Server {
    /// The requests served since `at` — a mark taken with [`Server::mark`].
    fn since(&self, at: usize) -> Vec<Request> {
        self.log.lock().unwrap()[at..].to_vec()
    }

    fn mark(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// Runs a loopback HTTP server on its own thread/runtime.
fn spawn_server() -> Server {
    let (tx, rx) = std::sync::mpsc::channel();
    let log: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let server_log = Arc::clone(&log);
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
                let log = Arc::clone(&server_log);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    let header_end = loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                    let mut fields = head.split_whitespace();
                    let method = fields.next().unwrap_or("GET").to_owned();
                    let path = fields.next().unwrap_or("/").to_owned();
                    let content_length = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l[15..].trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[header_end..]).into_owned();
                    log.lock().unwrap().push(Request {
                        method: method.clone(),
                        path: path.clone(),
                        content_type: header_value(&head, "Content-Type").unwrap_or_default(),
                        body: body.clone(),
                        referer: header_value(&head, "Referer").unwrap_or_default(),
                    });
                    let _ = sock.write_all(&route(&path)).await;
                    let _ = sock.flush().await;
                });
            }
        });
    });
    Server {
        port: rx.recv().unwrap(),
        log,
    }
}

fn route(path: &str) -> Vec<u8> {
    // The query string never selects a document: a form GET must be able to hit
    // the same page with any query at all.
    let (bare, _) = path.split_once('?').unwrap_or((path, ""));
    let html = |body: &str| resp(200, "OK", "text/html", body);
    match bare {
        "/start.html" => html(
            "<!doctype html><title>start</title>\
             <a id='link' href='/next.html'>go</a>\
             <a id='wrapped' href='/next.html'><span id='inner'>go</span></a>\
             <a id='blank' href='/next.html' target='_blank'>go</a>\
             <a id='frag' href='#anchor'>frag</a>\
             <div style='height:3000px'></div>\
             <p id='anchor'>anchor</p>",
        ),
        "/next.html" => html("<!doctype html><title>next</title><p id='here'>next</p>"),
        "/other.html" => html("<!doctype html><title>other</title>"),
        // Navigates away from itself as soon as it parses.
        "/redirect-by-script.html" => html(
            "<!doctype html><title>bounce</title>\
             <script>location.href = '/next.html';</script>",
        ),
        // A navigation loop: every load navigates again. Must terminate.
        "/loop.html" => html(
            "<!doctype html><title>loop</title>\
             <script>location.href = '/loop.html?n=' + Math.random();</script>",
        ),
        // Every load queues *two* navigations — a load and a traversal, which
        // do not collapse into each other. A chain that stops at
        // `MAX_CHAINED_NAVIGATIONS` therefore leaves work queued, and the event
        // loop re-enters its navigation branch immediately: without a deadline
        // check on that path, it never reaches the one at the foot of the loop.
        "/double-nav.html" => html(
            "<!doctype html><title>double</title>\
             <script>location.href = '/double-nav.html?n=' + Math.random(); history.back();</script>",
        ),
        // Navigates from a *parser-inserted* script, i.e. while the parser
        // still holds handles into the tree.
        "/navigate-while-parsing.html" => html(
            "<!doctype html><title>parsing</title>\
             <script>location.href = '/next.html';</script>\
             <p>more markup the parser still has to get through</p>\
             <span>and more</span>",
        ),
        "/form-get.html" => html(
            "<!doctype html><form id='f' action='/submitted' method='get'>\
             <input name='a' value='1'><input name='b' value='two words'>\
             <input type='checkbox' name='c' value='on' checked>\
             <input type='checkbox' name='d' value='off'>\
             <button id='btn' type='submit' name='go' value='yes'>go</button>\
             <button id='alt' type='submit' name='go' value='alt' \
                     formaction='/elsewhere' formmethod='post'>alt</button>\
             </form>",
        ),
        // The classic cancel idiom: an inline handler returning `false`.
        "/form-inline-cancel.html" => html(
            "<!doctype html><form id='f' action='/submitted' method='post' \
                                 onsubmit='window.ran = true; return false;'>\
             <button id='btn' type='submit'>go</button></form>\
             <a id='link' href='/next.html' onclick='return false'>no</a>",
        ),
        "/form-post.html" => html(
            "<!doctype html><form id='f' action='/submitted' method='post'>\
             <input name='a' value='1'><textarea name='t'>hi</textarea>\
             <button id='btn' type='submit'>go</button></form>",
        ),
        // A file input, so a real upload can be asserted end to end. The page
        // has no way to select files itself (there is no `DataTransfer`), so
        // the test drives `Page::set_file_input_files` — the embedder path.
        "/form-file.html" => html(
            "<!doctype html><form id='f' action='/submitted' method='post'>\
             <input name='a' value='1'><input id='up' name='doc' type='file'>\
             <button id='btn' type='submit'>go</button></form>",
        ),
        // A file input under an enctype the author *declared*: the empty-part
        // rule is HTML's here, not an engine upgrade.
        "/form-file-multipart.html" => html(
            "<!doctype html><form id='f' action='/submitted' method='post' \
                                 enctype='multipart/form-data'>\
             <input name='a' value='1'><input id='up' name='doc' type='file'>\
             <button id='btn' type='submit'>go</button></form>",
        ),
        "/form-multipart.html" => html(
            "<!doctype html><form id='f' action='/submitted' method='post' \
                                 enctype='multipart/form-data'>\
             <input name='a' value='1'><button id='btn' type='submit'>go</button></form>",
        ),
        // The canonical validate-then-submit idiom: cancel the event, do the
        // work, then submit programmatically.
        "/form-validate.html" => html(
            "<!doctype html><form id='f' action='/submitted' method='post'>\
             <input name='a' value='1'>\
             <button id='btn' type='submit'>go</button></form>\
             <script>document.getElementById('f').addEventListener('submit', e => {\
                 e.preventDefault(); window.validated = true; e.target.submit();\
             });</script>",
        ),
        "/form-reset.html" => html(
            "<!doctype html><form id='f'>\
             <input id='i' name='a' value='default'>\
             <button id='btn' type='reset'>reset</button></form>",
        ),
        "/submitted" | "/elsewhere" => html("<!doctype html><title>submitted</title>"),
        _ => resp(404, "Not Found", "text/plain", "nope"),
    }
}

fn resp(status: u16, reason: &str, content_type: &str, body: &str) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    out.push_str(&format!("Content-Type: {content_type}\r\n"));
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    out.push_str("Connection: close\r\n\r\n");
    out.push_str(body);
    out.into_bytes()
}

fn loopback_page() -> Page {
    Page::new(PageOptions {
        policy: Some(ResourcePolicy::permissive_localhost()),
        ..PageOptions::default()
    })
    .unwrap()
}

/// A page already sitting on `/start.html`.
fn started(server: &Server) -> Page {
    let page = loopback_page();
    page.navigate(&server.url("/start.html"), WaitUntil::Load)
        .unwrap();
    page
}

fn eval(page: &Page, source: &str) -> String {
    page.eval_to_string(source).unwrap()
}

/// Runs `source`, then lets the event loop perform whatever navigation it
/// queued — the two halves that make a script navigation happen.
fn eval_and_settle(page: &Page, source: &str) {
    page.eval(source).unwrap();
    page.settle(Duration::from_secs(5));
}

fn document_url(page: &Page) -> String {
    page.dom().document_url().to_owned()
}

// ------------------------------------------------------------------ Location

#[test]
fn location_href_assign_and_replace_navigate() {
    let server = spawn_server();
    let page = started(&server);
    // The initial `about:blank` entry is replaced by the first load, so a page
    // that has loaded one document has one entry — as a fresh browser tab does.
    assert_eq!(eval(&page, "String(history.length)"), "1");

    eval_and_settle(&page, "location.href = '/next.html';");
    assert_eq!(document_url(&page), server.url("/next.html"));
    assert_eq!(eval(&page, "document.title"), "next");
    assert_eq!(eval(&page, "String(history.length)"), "2");

    eval_and_settle(&page, "location.assign('/other.html');");
    assert_eq!(document_url(&page), server.url("/other.html"));
    assert_eq!(eval(&page, "String(history.length)"), "3");

    // `replace()` overwrites the current entry: the history does not grow.
    eval_and_settle(&page, "location.replace('/start.html');");
    assert_eq!(document_url(&page), server.url("/start.html"));
    assert_eq!(eval(&page, "String(history.length)"), "3");
}

#[test]
fn window_location_assignment_is_put_forwards_href() {
    let server = spawn_server();
    let page = started(&server);
    eval_and_settle(&page, "window.location = '/next.html';");
    assert_eq!(document_url(&page), server.url("/next.html"));
    // The property still holds the Location object, not the string.
    assert!(page.eval_to_string("typeof location").unwrap() == "object");
}

#[test]
fn location_reload_refetches_the_current_url() {
    let server = spawn_server();
    let page = started(&server);
    let at = server.mark();
    eval_and_settle(&page, "location.reload();");
    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "expected exactly one refetch, got {seen:?}");
    assert_eq!(seen[0].path, "/start.html");
    assert_eq!(document_url(&page), server.url("/start.html"));
    // A reload replaces its entry rather than pushing a duplicate.
    assert_eq!(eval(&page, "String(history.length)"), "1");
}

#[test]
fn location_component_setters_navigate() {
    let server = spawn_server();
    let page = started(&server);
    eval_and_settle(&page, "location.pathname = '/next.html';");
    assert_eq!(document_url(&page), server.url("/next.html"));
    assert_eq!(eval(&page, "location.pathname"), "/next.html");
    assert_eq!(eval(&page, "location.protocol"), "http:");
    assert_eq!(
        eval(&page, "location.host"),
        format!("127.0.0.1:{}", server.port)
    );
    assert_eq!(eval(&page, "location.hostname"), "127.0.0.1");
    assert_eq!(eval(&page, "location.port"), server.port.to_string());
    assert_eq!(eval(&page, "String(location)"), server.url("/next.html"));
}

/// A fragment write is a *same-document* navigation: no request, `hashchange`
/// fires, and the value reads back.
#[test]
fn location_hash_is_a_same_document_navigation() {
    let server = spawn_server();
    let page = started(&server);
    let at = server.mark();
    page.eval("window.hashes = []; addEventListener('hashchange', () => window.hashes.push(location.hash));")
        .unwrap();
    eval_and_settle(&page, "location.hash = '#anchor';");

    assert!(
        server.since(at).is_empty(),
        "a fragment navigation must not hit the network: {:?}",
        server.since(at)
    );
    assert_eq!(eval(&page, "location.hash"), "#anchor");
    assert_eq!(eval(&page, "window.hashes.join(',')"), "#anchor");
    assert_eq!(document_url(&page), server.url("/start.html#anchor"));
    // The scroll went to the target rather than staying at the origin.
    assert!(
        page.eval_to_string("String(window.scrollY > 0)").unwrap() == "true",
        "scrollY was {}",
        eval(&page, "String(window.scrollY)")
    );
}

// ------------------------------------------------------------------- Anchors

#[test]
fn anchor_click_navigates() {
    let server = spawn_server();
    let page = started(&server);
    eval_and_settle(&page, "document.getElementById('link').click();");
    assert_eq!(document_url(&page), server.url("/next.html"));
    assert_eq!(eval(&page, "document.title"), "next");
}

/// The activation target is the nearest ancestor with an activation behavior,
/// so clicking a `<span>` inside the anchor follows the link.
#[test]
fn click_inside_an_anchor_activates_it() {
    let server = spawn_server();
    let page = started(&server);
    eval_and_settle(&page, "document.getElementById('inner').click();");
    assert_eq!(document_url(&page), server.url("/next.html"));
}

#[test]
fn preventing_the_click_cancels_the_navigation() {
    let server = spawn_server();
    let page = started(&server);
    let at = server.mark();
    eval_and_settle(
        &page,
        "document.getElementById('link').addEventListener('click', e => e.preventDefault());\
         document.getElementById('link').click();",
    );
    assert!(server.since(at).is_empty());
    assert_eq!(document_url(&page), server.url("/start.html"));
}

/// `target` needs a second browsing context, which does not exist. The roadmap
/// answer for Stage 1 is to navigate in place and say so.
#[test]
fn target_blank_navigates_in_place_with_a_warning() {
    let server = spawn_server();
    let page = started(&server);
    page.drain_console();
    eval_and_settle(&page, "document.getElementById('blank').click();");
    assert_eq!(document_url(&page), server.url("/next.html"));
    // The warning is on the *outgoing* document's console, drained after the
    // navigation because the console outlives the document.
    let warned = page
        .drain_console()
        .iter()
        .any(|m| m.message.contains("target=`_blank`"));
    assert!(warned, "expected a console warning about target");
}

/// A fragment anchor stays in the document: no request, `hashchange` fires.
#[test]
fn fragment_anchor_click_stays_in_the_document() {
    let server = spawn_server();
    let page = started(&server);
    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('frag').click();");
    assert!(server.since(at).is_empty());
    assert_eq!(document_url(&page), server.url("/start.html#anchor"));
}

#[test]
fn document_referrer_is_the_document_the_navigation_left() {
    let server = spawn_server();
    let page = started(&server);
    // An embedder navigation has no predecessor.
    assert_eq!(eval(&page, "document.referrer"), "");
    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('link').click();");
    assert_eq!(eval(&page, "document.referrer"), server.url("/start.html"));
    // And the header went out on the wire too.
    let seen = server.since(at);
    assert_eq!(seen[0].referer, server.url("/start.html"));
}

// ------------------------------------------------------------------- History

#[test]
fn push_state_then_back_fires_popstate_without_a_request() {
    let server = spawn_server();
    let page = started(&server);
    let at = server.mark();
    page.eval(
        "window.states = [];\
         addEventListener('popstate', e => window.states.push(JSON.stringify(e.state)));\
         history.pushState({n: 1}, '', '/one');\
         history.pushState({n: 2}, '', '/two');",
    )
    .unwrap();
    assert_eq!(eval(&page, "String(history.length)"), "3");
    assert_eq!(eval(&page, "JSON.stringify(history.state)"), r#"{"n":2}"#);
    assert_eq!(document_url(&page), server.url("/two"));

    eval_and_settle(&page, "history.back();");
    assert_eq!(document_url(&page), server.url("/one"));
    assert_eq!(eval(&page, "JSON.stringify(history.state)"), r#"{"n":1}"#);
    assert_eq!(eval(&page, "window.states.join('|')"), r#"{"n":1}"#);

    eval_and_settle(&page, "history.forward();");
    assert_eq!(document_url(&page), server.url("/two"));
    assert_eq!(eval(&page, "window.states.join('|')"), r#"{"n":1}|{"n":2}"#);
    assert!(
        server.since(at).is_empty(),
        "same-document traversal must not hit the network"
    );
}

/// Traversing out of the current document has no bfcache to fall back on, so it
/// is a reload — and the entry is re-stamped, so going forward and back again
/// stays in-document.
#[test]
fn cross_document_back_refetches() {
    let server = spawn_server();
    let page = started(&server);
    eval_and_settle(&page, "document.getElementById('link').click();");
    assert_eq!(document_url(&page), server.url("/next.html"));

    let at = server.mark();
    eval_and_settle(&page, "history.back();");
    assert_eq!(document_url(&page), server.url("/start.html"));
    assert_eq!(eval(&page, "document.title"), "start");
    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "expected one refetch, got {seen:?}");
    assert_eq!(seen[0].path, "/start.html");
    assert_eq!(eval(&page, "String(history.length)"), "2");
}

/// `history.go(0)` is a reload — the shim's no-op was only correct while reload
/// did not exist. Out-of-range deltas stay silent no-ops.
#[test]
fn history_go_zero_reloads_and_out_of_range_is_a_no_op() {
    let server = spawn_server();
    let page = started(&server);

    let at = server.mark();
    eval_and_settle(&page, "history.go(0);");
    assert_eq!(server.since(at).len(), 1);

    let at = server.mark();
    eval_and_settle(&page, "history.go(-5); history.go(99);");
    assert!(server.since(at).is_empty());
    assert_eq!(document_url(&page), server.url("/start.html"));
}

/// Two traversals queued in one task are **both** performed. A delta traversal
/// is cumulative — a browser moving one entry per `back()` is the whole point —
/// so the second must not supersede the first the way a second load supersedes
/// a first.
#[test]
fn two_traversals_in_one_task_both_happen() {
    let server = spawn_server();
    let page = started(&server);
    eval_and_settle(&page, "location.href = '/next.html';");
    eval_and_settle(&page, "location.href = '/other.html';");
    assert_eq!(document_url(&page), server.url("/other.html"));

    eval_and_settle(&page, "history.back(); history.back();");
    assert_eq!(
        document_url(&page),
        server.url("/start.html"),
        "two back() calls move two entries, not one"
    );
}

/// A `pushState` truncates the forward entries, as a real navigation does.
#[test]
fn push_state_truncates_forward_entries() {
    let server = spawn_server();
    let page = started(&server);
    page.eval("history.pushState(null, '', '/one'); history.pushState(null, '', '/two');")
        .unwrap();
    eval_and_settle(&page, "history.go(-2);");
    assert_eq!(document_url(&page), server.url("/start.html"));
    assert_eq!(eval(&page, "String(history.length)"), "3");
    page.eval("history.pushState(null, '', '/fresh');").unwrap();
    assert_eq!(eval(&page, "String(history.length)"), "2");
}

// --------------------------------------------------------- Form submission

#[test]
fn form_get_puts_the_entry_list_in_the_query() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-get.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('btn').click();");

    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].method, "GET");
    // Unchecked `d` is not successful; the submitter contributes `go=yes` at
    // its own tree position, after the controls that precede it.
    assert_eq!(seen[0].path, "/submitted?a=1&b=two+words&c=on&go=yes");
    // A GET submission's URL *is* the mutated action URL, query included.
    assert_eq!(
        document_url(&page),
        server.url("/submitted?a=1&b=two+words&c=on&go=yes")
    );
}

#[test]
fn submitter_formaction_and_formmethod_win_over_the_form() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-get.html"), WaitUntil::Load)
        .unwrap();
    // Feature detection sees the reflections.
    assert_eq!(
        eval(&page, "document.getElementById('alt').formMethod"),
        "post"
    );
    assert_eq!(
        eval(&page, "document.getElementById('alt').formAction"),
        server.url("/elsewhere")
    );

    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('alt').click();");
    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/elsewhere");
    assert_eq!(seen[0].body, "a=1&b=two+words&c=on&go=alt");
}

#[test]
fn form_post_sends_a_urlencoded_body() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-post.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('btn').click();");

    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].content_type, "application/x-www-form-urlencoded");
    assert_eq!(seen[0].body, "a=1&t=hi");
}

/// The multipart boundary is only correct if the header and the body agree —
/// which is exactly the bug a header-only assertion would miss.
#[test]
fn form_post_multipart_names_its_boundary_in_both_places() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-multipart.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('btn').click();");

    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "{seen:?}");
    let boundary = seen[0]
        .content_type
        .split_once("boundary=")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_else(|| panic!("no boundary in `{}`", seen[0].content_type));
    assert!(seen[0].content_type.starts_with("multipart/form-data;"));
    assert!(
        seen[0].body.starts_with(&format!("--{boundary}\r\n")),
        "body did not open with the header's boundary: {:?}",
        seen[0].body
    );
    assert!(seen[0].body.contains("name=\"a\""));
    assert!(
        seen[0]
            .body
            .trim_end()
            .ends_with(&format!("--{boundary}--"))
    );
}

#[test]
fn submit_is_cancelable_and_carries_the_submitter() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-post.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(
        &page,
        "window.seen = [];\
         document.getElementById('f').addEventListener('submit', e => {\
           window.seen.push(e.type + ':' + (e.submitter && e.submitter.id) \
                            + ':' + (e instanceof SubmitEvent));\
           e.preventDefault();\
         });\
         document.getElementById('btn').click();",
    );
    assert_eq!(eval(&page, "window.seen.join(',')"), "submit:btn:true");
    assert!(server.since(at).is_empty(), "preventDefault must stop it");
}

/// `form.submit()` submits *without* firing `submit`, per HTML — which is
/// exactly why it is not a synonym for clicking the button.
#[test]
fn form_submit_method_fires_no_submit_event() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-post.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(
        &page,
        "window.fired = 0;\
         document.getElementById('f').addEventListener('submit', () => window.fired++);\
         document.getElementById('f').submit();",
    );
    assert_eq!(eval(&page, "String(window.fired)"), "0");
    assert_eq!(server.since(at).len(), 1);
}

/// HTML's event handler processing algorithm: an **IDL-attribute** handler that
/// returns `false` cancels the event. `onsubmit="…; return false"` and
/// `onclick="return false"` are the canonical way to stop a default action, and
/// before navigation existed there was no default action for them to stop —
/// so the returned value was discarded and nothing noticed.
#[test]
fn an_inline_handler_returning_false_cancels_the_default_action() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-inline-cancel.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();

    eval_and_settle(&page, "document.getElementById('btn').click();");
    assert_eq!(
        eval(&page, "String(window.ran)"),
        "true",
        "handler must run"
    );
    assert!(server.since(at).is_empty(), "`return false` must cancel");

    eval_and_settle(&page, "document.getElementById('link').click();");
    assert!(server.since(at).is_empty(), "`return false` must cancel");
    assert_eq!(document_url(&page), server.url("/form-inline-cancel.html"));

    // A handler returning nothing (the common case) must *not* cancel.
    eval_and_settle(
        &page,
        "document.getElementById('link').onclick = () => {};\
         document.getElementById('link').click();",
    );
    assert_eq!(document_url(&page), server.url("/next.html"));
}

/// An `onsubmit` handler that re-submits must not recurse forever, and must
/// still reach the wire exactly once: `form.submit()` queues a navigation, the
/// outer submission queues another, and a queued load supersedes a queued load
/// — the same collapsing a browser does.
#[test]
fn a_resubmitting_submit_handler_terminates() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-post.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(
        &page,
        "const f = document.getElementById('f');\
         f.addEventListener('submit', () => f.submit());\
         document.getElementById('btn').click();",
    );
    assert_eq!(server.since(at).len(), 1);
}

/// `onsubmit = e => { e.preventDefault(); validate(); form.submit(); }` — the
/// idiom every hand-rolled validator uses. HTML's "firing submission events"
/// flag guards only the event-firing entry points, so `form.submit()`, which
/// fires no event, must go through. Guarding the whole of `submit()` with it
/// swallowed the submission silently and left the page looking hung.
#[test]
fn a_programmatic_submit_from_onsubmit_is_performed() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-validate.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('btn').click();");

    assert_eq!(eval(&page, "String(window.validated)"), "true");
    let seen = server.since(at);
    assert_eq!(
        seen.len(),
        1,
        "expected exactly one submission, got {seen:?}"
    );
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/submitted");
    assert_eq!(seen[0].body, "a=1");
}

#[test]
fn reset_button_fires_reset_and_restores_the_defaults() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-reset.html"), WaitUntil::Load)
        .unwrap();
    let at = server.mark();
    eval_and_settle(
        &page,
        "window.fired = 0;\
         document.getElementById('f').addEventListener('reset', () => window.fired++);\
         document.getElementById('i').value = 'typed';\
         document.getElementById('btn').click();",
    );
    assert_eq!(eval(&page, "String(window.fired)"), "1");
    assert_eq!(eval(&page, "document.getElementById('i').value"), "default");
    assert!(
        server.since(at).is_empty(),
        "a reset button must not submit"
    );

    // A cancelled `reset` leaves the value alone.
    eval_and_settle(
        &page,
        "document.getElementById('f').addEventListener('reset', e => e.preventDefault());\
         document.getElementById('i').value = 'typed again';\
         document.getElementById('btn').click();",
    );
    assert_eq!(
        eval(&page, "document.getElementById('i').value"),
        "typed again"
    );
}

// ---------------------------------------------------------------- Robustness

/// A script that navigates while the parser is still running: the request must
/// wait for the parser to finish rather than pull the tree out from under it.
#[test]
fn navigating_while_the_parser_runs_is_safe() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/navigate-while-parsing.html"), WaitUntil::Load)
        .unwrap();
    assert_eq!(document_url(&page), server.url("/next.html"));
    assert_eq!(eval(&page, "document.title"), "next");
    assert!(page.drain_errors().is_empty());
}

/// A document that navigates on load is chained off the navigation that
/// brought it in, so the caller returns on the final document.
#[test]
fn a_script_navigation_chains_off_the_load() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/redirect-by-script.html"), WaitUntil::Load)
        .unwrap();
    assert_eq!(document_url(&page), server.url("/next.html"));
}

/// A page that navigates forever must terminate, not hang the caller.
#[test]
fn an_endless_navigation_chain_is_capped() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/loop.html"), WaitUntil::Load)
        .unwrap();
    let stopped = page
        .drain_console()
        .iter()
        .any(|m| m.message.contains("consecutive script-driven navigations"));
    assert!(stopped, "expected the chain cap to report itself");
}

/// `MAX_CHAINED_NAVIGATIONS` bounds one *chain*, and the event loop's own
/// navigation branch starts a fresh chain with the counter back at zero every
/// time it runs. A page that leaves work queued after every chain therefore
/// re-enters that branch forever, and the settle deadline is the only thing
/// that can stop it — which the branch used to `continue` straight past.
#[test]
fn settle_respects_its_budget_while_the_page_keeps_navigating() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/double-nav.html"), WaitUntil::Load)
        .unwrap();
    let started = std::time::Instant::now();
    page.settle(Duration::from_millis(100));
    let elapsed = started.elapsed();
    // Measured: ~170 ms with the check, ~2.25 s without it — a whole further
    // chain of `MAX_CHAINED_NAVIGATIONS` document loads past the budget.
    assert!(
        elapsed < Duration::from_secs(1),
        "settle(100ms) took {elapsed:?} on a page that never stops navigating"
    );
}

/// `load_html` has no URL to fetch, but a script in it can still navigate.
#[test]
fn load_html_chains_a_script_navigation() {
    let server = spawn_server();
    let page = loopback_page();
    page.load_html(&format!(
        "<!doctype html><script>location.href = '{}';</script>",
        server.url("/next.html")
    ))
    .unwrap();
    assert_eq!(document_url(&page), server.url("/next.html"));
    assert_eq!(eval(&page, "document.title"), "next");
}

/// A script navigation that fails keeps the current document — the page did not
/// move, it is not blank. An *embedder* navigation reports the failure instead.
#[test]
fn a_failed_script_navigation_keeps_the_document() {
    let server = spawn_server();
    let page = started(&server);
    eval_and_settle(&page, "location.href = 'http://127.0.0.1:1/nope';");
    assert_eq!(document_url(&page), server.url("/start.html"));
    assert_eq!(eval(&page, "document.title"), "start");

    let fresh = loopback_page();
    assert!(
        fresh
            .navigate("http://127.0.0.1:1/nope", WaitUntil::Load)
            .is_err()
    );
}

// ------------------------------------------------------- Navigation events

#[test]
fn navigation_events_record_the_milestones_in_order() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/start.html"), WaitUntil::Load)
        .unwrap();
    let kinds: Vec<NavigationEventKind> = page
        .drain_navigation_events()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            NavigationEventKind::Started,
            NavigationEventKind::Committed,
            NavigationEventKind::DomContentLoaded,
            NavigationEventKind::Load,
        ]
    );

    // A fragment navigation commits nothing.
    eval_and_settle(&page, "location.hash = '#anchor';");
    let events = page.drain_navigation_events();
    assert!(
        events
            .iter()
            .any(|e| e.kind == NavigationEventKind::SameDocument
                && e.url.ends_with("/start.html#anchor")),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == NavigationEventKind::Committed)
    );
    // `settle` reaching idle records it.
    assert!(
        events
            .iter()
            .any(|e| e.kind == NavigationEventKind::NetworkIdle)
    );
}

/// `NetworkIdle` is a milestone of one *navigation*, not of one `settle` call.
/// Every `eval`/`dispatch_*` ends in a settle that reaches idle, so recording
/// it each time grew the stream — which nothing drains for the embedder —
/// without bound, one owned document URL at a time.
#[test]
fn network_idle_is_recorded_once_per_document() {
    let server = spawn_server();
    let page = started(&server);
    for _ in 0..5 {
        page.settle(Duration::from_millis(50));
    }
    let idle = page
        .drain_navigation_events()
        .into_iter()
        .filter(|e| e.kind == NavigationEventKind::NetworkIdle)
        .count();
    assert_eq!(idle, 1, "repeated settles must not re-record the milestone");

    // A navigation starts a new document, and with it a new milestone.
    eval_and_settle(&page, "location.href = '/next.html';");
    page.settle(Duration::from_millis(50));
    let idle = page
        .drain_navigation_events()
        .into_iter()
        .filter(|e| e.kind == NavigationEventKind::NetworkIdle)
        .count();
    assert_eq!(idle, 1, "the next document records its own");
}

#[test]
fn a_failed_navigation_is_recorded_with_its_error() {
    let server = spawn_server();
    let page = started(&server);
    let _ = page.drain_navigation_events();
    eval_and_settle(&page, "location.href = 'http://127.0.0.1:1/nope';");
    let events = page.drain_navigation_events();
    let failed = events
        .iter()
        .find(|e| e.kind == NavigationEventKind::Failed)
        .unwrap_or_else(|| panic!("no Failed event in {events:?}"));
    assert!(failed.error.is_some());
    assert!(failed.url.ends_with("/nope"));
}

// ------------------------------------------------------------------ Surface

#[test]
fn location_and_history_are_real_interfaces() {
    let page = load_html_page("<!doctype html><title>t</title>", PageOptions::default()).unwrap();
    for expr in [
        "location instanceof Location",
        "history instanceof History",
        "typeof Location === 'function'",
        "typeof History === 'function'",
        "typeof PopStateEvent === 'function'",
        "new PopStateEvent('popstate', {state: {a: 1}}).state.a === 1",
        // The old shims are gone, along with the native hook that backed them.
        "typeof globalThis.__oxide_setDocumentUrl === 'undefined'",
    ] {
        assert_eq!(
            page.eval_to_string(&format!("String({expr})")).unwrap(),
            "true",
            "{expr}"
        );
    }
}

/// A file input turns a form post into `multipart/form-data` whatever the
/// author's `enctype` says, and the file lands as a real part (ADR-0032 D11).
///
/// The failure this replaces was silent: a file input contributed *nothing*, so
/// a form that looked like it uploaded posted every other field and no file.
#[test]
fn a_file_input_forces_multipart_and_sends_the_bytes() {
    let directory = std::env::temp_dir().join(format!(
        "oxidepage-form-upload-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("notes.txt");
    std::fs::write(&path, b"line one\nline two\n").unwrap();

    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-file.html"), WaitUntil::Load)
        .unwrap();

    let root = page.dom().document();
    let input = page
        .query_selector(root, "#up")
        .expect("query")
        .expect("the file input");
    page.set_file_input_files(input, &[path.to_string_lossy().into_owned()])
        .expect("selection");

    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('btn').click();");

    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "{seen:?}");
    // The form declares no `enctype`, so it would default to urlencoded — which
    // cannot carry bytes. A non-empty file input forces multipart.
    assert!(
        seen[0].content_type.starts_with("multipart/form-data;"),
        "a file input must force multipart, got `{}`",
        seen[0].content_type
    );
    let body = &seen[0].body;
    assert!(
        body.contains("Content-Disposition: form-data; name=\"a\"\r\n\r\n1"),
        "the ordinary field is still there: {body}"
    );
    assert!(
        body.contains("Content-Disposition: form-data; name=\"doc\"; filename=\"notes.txt\""),
        "the file part names its filename: {body}"
    );
    assert!(
        body.contains("Content-Type: text/plain"),
        "the file part carries its own content type: {body}"
    );
    assert!(
        body.contains("line one\nline two"),
        "the bytes were sent: {body}"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// An **empty** file input does not upgrade the enctype.
///
/// ADR-0032 D11's upgrade exists because urlencoded cannot carry bytes, so
/// honouring the author's enctype would drop an upload. With nothing selected
/// there are no bytes to lose, and upgrading anyway silently rewrites the wire
/// format of an ordinary urlencoded post that merely *contains* a file input —
/// breaking every server that parses the default encoding. Chrome sends the
/// filename (here, the empty string) as an ordinary urlencoded field.
#[test]
fn an_empty_file_input_does_not_upgrade_the_enctype() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-file.html"), WaitUntil::Load)
        .unwrap();

    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('btn').click();");

    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0]
            .content_type
            .starts_with("application/x-www-form-urlencoded"),
        "the form declared no enctype and chose no file, so it stays \
         urlencoded: `{}`",
        seen[0].content_type
    );
    // "Field present, no file chosen" still reaches the server — the thing the
    // empty entry exists to say — just in the encoding that was asked for.
    assert_eq!(seen[0].body, "a=1&doc=", "got `{}`", seen[0].body);
}

/// An **empty** file input still contributes one empty part when the form is
/// genuinely multipart, per HTML — which is what lets a server tell "no file
/// chosen" from "field absent".
#[test]
fn an_empty_file_input_contributes_an_empty_part() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/form-file-multipart.html"), WaitUntil::Load)
        .unwrap();

    let at = server.mark();
    eval_and_settle(&page, "document.getElementById('btn').click();");

    let seen = server.since(at);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].content_type.starts_with("multipart/form-data;"),
        "the author declared multipart: `{}`",
        seen[0].content_type
    );
    assert!(
        seen[0]
            .body
            .contains("Content-Disposition: form-data; name=\"doc\"; filename=\"\""),
        "the empty part is present: {}",
        seen[0].body
    );
}

// === the script-created parser (ADR-0034 D2) ===

/// `document.open()` / `write()` / `close()` replaces the document, and the
/// replacement travels the ordinary commit path — so it records the same
/// milestones a URL navigation does. Playwright's `setContent` is exactly this
/// sequence, and it waits for the `load` at the end of it.
#[test]
fn open_write_close_replaces_the_document_and_records_a_commit() {
    let page = load_html_page(
        "<!doctype html><title>first</title><p>old</p>",
        PageOptions::default(),
    )
    .expect("page");
    let _ = page.drain_navigation_events();

    page.eval(
        "document.open();
         document.write('<!doctype html><title>second</title><p id=s>new</p>');
         document.close();",
    )
    .expect("eval");
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("document.getElementById('s').textContent")
            .unwrap(),
        "new"
    );
    assert_eq!(page.eval_to_string("document.title").unwrap(), "second");

    let kinds: Vec<NavigationEventKind> = page
        .drain_navigation_events()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    for expected in [
        NavigationEventKind::Started,
        NavigationEventKind::Committed,
        NavigationEventKind::Load,
    ] {
        assert!(
            kinds.contains(&expected),
            "a document replacement must record {expected:?}, got {kinds:?}"
        );
    }
}

/// The replacement keeps the document URL: `open()`/`close()` is not a
/// navigation to anywhere, and a driver reading `page.url()` afterwards must
/// see what it saw before.
#[test]
fn a_document_replacement_keeps_the_url() {
    let server = spawn_server();
    let page = loopback_page();
    page.navigate(&server.url("/start.html"), WaitUntil::Load)
        .unwrap();
    let before = page.eval_to_string("location.href").unwrap();

    page.eval("document.open(); document.write('<p>x</p>'); document.close();")
        .expect("eval");
    page.settle(Duration::from_secs(5));

    assert_eq!(page.eval_to_string("location.href").unwrap(), before);
}

/// `document.open()` and `write()` with no `close()` is the legacy idiom, and a
/// browser still shows the content. The task boundary commits it.
#[test]
fn an_unclosed_script_parser_still_commits_at_the_task_boundary() {
    let page = load_html_page("<!doctype html><p>old</p>", PageOptions::default()).expect("page");
    page.eval("document.open(); document.write('<p id=s>written</p>');")
        .expect("eval");
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("document.getElementById('s').textContent")
            .unwrap(),
        "written"
    );
}

/// `document.write` **without** `open()` keeps the behaviour it always had —
/// the script-created parser is opt-in, and the old path is untouched.
#[test]
fn write_without_open_is_still_ignored_outside_a_parser() {
    let page = load_html_page(
        "<!doctype html><p id=keep>original</p>",
        PageOptions::default(),
    )
    .expect("page");
    page.eval("document.write('<p id=s>ignored</p>')")
        .expect("eval");
    page.settle(Duration::from_secs(5));

    assert_eq!(
        page.eval_to_string("document.getElementById('keep').textContent")
            .unwrap(),
        "original"
    );
    assert_eq!(
        page.eval_to_string("String(document.getElementById('s'))")
            .unwrap(),
        "null"
    );
}

/// A document with no browsing context has nothing to replace, so `open()`
/// throws there rather than quietly building a buffer that would replace the
/// *rendered* document (ADR-0017's rule, kept).
#[test]
fn open_on_a_second_document_throws() {
    let page = load_html_page("<!doctype html><p>x</p>", PageOptions::default()).expect("page");
    assert_eq!(
        page.eval_to_string(
            "(() => { try { new DOMParser().parseFromString('<p>a</p>', 'text/html').open(); \
              return 'no throw'; } catch (e) { return e.name; } })()"
        )
        .unwrap(),
        "InvalidStateError"
    );
}

/// `Page::load_html` is the embedder's own document replacement and records the
/// same milestones — without them a driver's `set_content` looked like nothing
/// had happened at all.
#[test]
fn load_html_records_the_navigation_milestones() {
    let page = load_html_page("<!doctype html><p>first</p>", PageOptions::default()).expect("page");
    let _ = page.drain_navigation_events();

    page.load_html("<!doctype html><p>second</p>")
        .expect("load");

    let kinds: Vec<NavigationEventKind> = page
        .drain_navigation_events()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    for expected in [
        NavigationEventKind::Started,
        NavigationEventKind::Committed,
        NavigationEventKind::Load,
    ] {
        assert!(
            kinds.contains(&expected),
            "load_html must record {expected:?}, got {kinds:?}"
        );
    }
}

/// A parser-inserted script cannot hijack the parser it is running inside.
///
/// HTML's document-open steps return early when a script of the active parser
/// is running. Without that, `document.open()` inline in a page being parsed
/// diverts its own `write`s into the buffer and then replaces the document the
/// parser is still building.
#[test]
fn open_during_parsing_is_a_no_op_and_write_still_reaches_the_parser() {
    let page = load_html_page(
        "<!doctype html><p id=before>kept</p>\
         <script>document.open(); document.write('<p id=written>via parser</p>');</script>\
         <p id=after>kept too</p>",
        PageOptions::default(),
    )
    .expect("page");
    page.settle(Duration::from_secs(5));

    // The parsed document survived: `open()` did not queue a replacement.
    assert_eq!(
        page.eval_to_string("document.getElementById('before').textContent")
            .unwrap(),
        "kept"
    );
    assert_eq!(
        page.eval_to_string("document.getElementById('after').textContent")
            .unwrap(),
        "kept too"
    );
    // …and the write went to the real parser, in place.
    assert_eq!(
        page.eval_to_string("document.getElementById('written').textContent")
            .unwrap(),
        "via parser"
    );
}
