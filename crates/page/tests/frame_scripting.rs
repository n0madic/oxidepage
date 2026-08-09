//! Cross-frame scripting (ADR-0035 D4): the window family across a frame
//! boundary, and `postMessage`.
//!
//! The rule that shapes all of it: **no `JsValue` crosses a frame boundary**.
//! `contentWindow` is a `WindowProxy` minted in the *accessing* realm, and a
//! message is serialized in the sender's realm and deserialized in the
//! receiver's. That is what lets each (frame, world) keep its own `Runtime`,
//! which is in turn what makes nested delivery legal at all.

use std::time::Duration;

use oxidepage_page::{PageOptions, load_html_page};

fn page(html: &str) -> oxidepage_page::Page {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.settle(Duration::from_millis(500));
    page
}

fn s(page: &oxidepage_page::Page, expr: &str) -> String {
    page.eval_to_string(expr).unwrap()
}

/// How many documents the page is rendering, top-level one included.
fn rendered_roots_count(page: &oxidepage_page::Page) -> usize {
    page.dom().rendered_roots().count()
}

#[test]
fn the_top_level_context_is_its_own_parent_and_top() {
    let page = page("<!DOCTYPE html><body></body>");
    assert_eq!(s(&page, "window.parent === window"), "true");
    assert_eq!(s(&page, "window.top === window"), "true");
    assert_eq!(s(&page, "window.frames === window"), "true");
    assert_eq!(s(&page, "window.frameElement"), "null");
    assert_eq!(s(&page, "window.length"), "0");
}

#[test]
fn length_counts_the_nested_contexts() {
    let page = page("<!DOCTYPE html><body><iframe></iframe><iframe></iframe></body>");
    assert_eq!(s(&page, "window.length"), "2");
}

/// A frame reaches its embedder through `parent`, and its own element through
/// `frameElement`. Both are same-origin here — `srcdoc` inherits the
/// embedder's URL rather than deriving an origin from a URL it has not got.
#[test]
fn a_frame_reaches_its_embedder() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<script>\
           window.parentIsSelf = (window.parent === window);\
           window.topIsSelf = (window.top === window);\
           window.hasElement = (window.frameElement !== null);\
           window.elementTag = window.frameElement ? window.frameElement.tagName : \"\";\
           window.myLength = window.length;\
         </script>'></iframe></body>",
    );
    // Read back through the parent realm — the frame's own globals are not
    // reachable from here, so the assertions run inside and land on its
    // document instead.
    assert_eq!(
        s(&page, "document.getElementById('f').contentWindow !== null"),
        "true"
    );
}

/// The parent posts to the frame; the frame's listener sees the body, the
/// sender's origin, and a `source` it can post back to.
#[test]
fn post_message_reaches_a_frame_and_comes_back() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<script>\
           window.addEventListener(\"message\", (e) => {\
             document.title = e.data.hello + \"|\" + typeof e.source;\
             e.source.postMessage({ pong: e.data.hello.length }, \"*\");\
           });\
         </script>'></iframe>\
         <script>\
           window.replies = [];\
           window.addEventListener(\"message\", (e) => { window.replies.push(e.data.pong); });\
         </script></body>",
    );
    page.eval_to_string(
        "document.getElementById('f').contentWindow.postMessage({ hello: 'world' }, '*'); 0",
    )
    .unwrap();
    page.settle(Duration::from_millis(500));

    // The frame received it, deserialized in *its* realm…
    assert_eq!(
        s(&page, "document.getElementById('f').contentDocument.title"),
        "world|object"
    );
    // …and its reply came back to the parent, deserialized in *this* one.
    assert_eq!(s(&page, "window.replies.length"), "1");
    assert_eq!(s(&page, "window.replies[0]"), "5");
}

/// Delivery is a task. A `postMessage` is never observable before the calling
/// script yields — which is what stops a ping-pong from riding the native
/// stack.
#[test]
fn post_message_is_delivered_as_a_task() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<script>\
           window.addEventListener(\"message\", () => { document.title = \"got\"; });\
         </script>'></iframe></body>",
    );
    let synchronous = page
        .eval_to_string(
            "const w = document.getElementById('f').contentWindow;\
             w.postMessage(1, '*');\
             document.getElementById('f').contentDocument.title",
        )
        .unwrap();
    assert_eq!(synchronous, "", "not delivered while the sender still runs");

    page.settle(Duration::from_millis(500));
    assert_eq!(
        s(&page, "document.getElementById('f').contentDocument.title"),
        "got"
    );
}

/// `postMessage` carries a JSON subset. Anything outside it is **refused with
/// `DataCloneError`**, not silently flattened — the difference between a
/// documented limit and a lie.
#[test]
fn post_message_refuses_what_it_cannot_clone() {
    let page = page("<!DOCTYPE html><body><iframe id='f'></iframe></body>");
    let thrown = s(
        &page,
        "(() => { try { \
           document.getElementById('f').contentWindow.postMessage(new Map(), '*'); \
           return 'no throw'; \
         } catch (e) { return e.name; } })()",
    );
    assert_eq!(thrown, "DataCloneError");

    // A cycle is equally outside the subset.
    let cyclic = s(
        &page,
        "(() => { const a = {}; a.self = a; try { \
           document.getElementById('f').contentWindow.postMessage(a, '*'); \
           return 'no throw'; \
         } catch (e) { return e.name; } })()",
    );
    assert_eq!(cyclic, "DataCloneError");
}

/// `MessageEvent` is constructible, and its fields round-trip.
#[test]
fn message_event_is_constructible() {
    let page = page("<!DOCTYPE html><body></body>");
    assert_eq!(
        s(
            &page,
            "new MessageEvent('message', { data: { a: 1 }, origin: 'https://x.example' }).data.a"
        ),
        "1"
    );
    assert_eq!(
        s(
            &page,
            "new MessageEvent('message', { origin: 'https://x.example' }).origin"
        ),
        "https://x.example"
    );
    assert_eq!(s(&page, "new MessageEvent('message').source"), "null");
    assert_eq!(
        s(&page, "new MessageEvent('message') instanceof Event"),
        "true"
    );
}

/// `contentWindow` is a `WindowProxy` of the *accessing* realm, so it is not
/// the child's global and the child's own variables stay unreachable — the
/// documented divergence that lets each frame keep its own runtime.
#[test]
fn a_childs_globals_are_unreachable() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<script>window.secret = 42;</script>'></iframe></body>",
    );
    assert_eq!(
        s(&page, "document.getElementById('f').contentWindow.secret"),
        "undefined"
    );
    // The document, though, is genuinely reachable — one arena.
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument instanceof Document"
        ),
        "true"
    );
}

/// A `WindowProxy` for a removed frame reports itself closed rather than
/// dangling.
#[test]
fn a_detached_frames_proxy_reports_closed() {
    let page = page("<!DOCTYPE html><body><iframe id='f'></iframe></body>");
    page.eval_to_string("window.w = document.getElementById('f').contentWindow; 0")
        .unwrap();
    assert_eq!(s(&page, "window.w.closed"), "false");

    page.eval_to_string("document.getElementById('f').remove(); 0")
        .unwrap();
    page.settle(Duration::from_millis(300));
    assert_eq!(s(&page, "window.w.closed"), "true");
}

/// `sandbox` without `allow-scripts` runs no script in the frame — and says so
/// rather than leaving a page to wonder why its frame does nothing
/// (ADR-0035 D11).
///
/// `allow-same-origin` is granted so the embedder can *see* the result; the two
/// tokens are independent, and this is the one that is being withheld.
#[test]
fn a_sandboxed_frame_without_allow_scripts_runs_none() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' sandbox='allow-same-origin' srcdoc='<p id=p>x</p><script>\
           document.getElementById(\"p\").textContent = \"ran\";\
         </script>'></iframe></body>",
    );
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument.getElementById('p').textContent"
        ),
        "x",
        "the script must not have run"
    );
}

/// `allow-scripts` gives it back.
#[test]
fn allow_scripts_lets_a_sandboxed_frame_run() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' sandbox='allow-scripts allow-same-origin' \
          srcdoc='<p id=p>x</p><script>\
           document.getElementById(\"p\").textContent = \"ran\";\
         </script>'></iframe></body>",
    );
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument.getElementById('p').textContent"
        ),
        "ran"
    );
}

/// Without `allow-same-origin` the frame has an **opaque** origin, so its
/// embedder cannot reach into it — `contentDocument` is `null`, as in a
/// browser.
#[test]
fn a_sandboxed_frame_gets_an_opaque_origin() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='opaque' sandbox srcdoc='<p>x</p>'></iframe>\
         <iframe id='same' sandbox='allow-same-origin' srcdoc='<p>x</p>'></iframe>\
         </body>",
    );
    assert_eq!(
        s(&page, "document.getElementById('opaque').contentDocument"),
        "null",
        "an opaque-origin frame is not reachable from its embedder"
    );
    assert_ne!(
        s(&page, "document.getElementById('same').contentDocument"),
        "null",
        "allow-same-origin gives access back"
    );
    // The context still exists and still rendered — only reaching into it is
    // refused.
    assert_eq!(rendered_roots_count(&page), 3);
}

/// The attribute round-trips exactly. It is a string, not a `DOMTokenList`:
/// only two tokens are enforced, and a `sandbox.add('allow-forms')` that looked
/// like it granted something would be the fake P6 forbids.
#[test]
fn the_sandbox_attribute_reflects() {
    let page = page(
        "<!DOCTYPE html><body><iframe id='f' sandbox='allow-scripts allow-forms'></iframe></body>",
    );
    assert_eq!(
        s(&page, "document.getElementById('f').sandbox"),
        "allow-scripts allow-forms"
    );
    assert_eq!(
        s(&page, "typeof document.getElementById('f').sandbox"),
        "string"
    );
}

/// A frame's own script navigating **itself**.
///
/// The navigation queue is per browsing context, and only the top-level one was
/// ever drained — so `location.href = …` inside a frame was silently dropped
/// (ADR-0035 D5).
#[test]
fn a_frame_navigates_itself() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<p id=p>before</p><script>\
           setTimeout(() => { location.href = \"about:blank\"; }, 0);\
         </script>'></iframe></body>",
    );
    page.settle(Duration::from_millis(600));
    assert_eq!(
        s(
            &page,
            "!!document.getElementById('f').contentDocument.getElementById('p')"
        ),
        "false",
        "the frame replaced its own document"
    );
    // And the page around it is untouched.
    assert_eq!(rendered_roots_count(&page), 2);
}

/// `location` inside a frame describes the **frame's** document, not the
/// embedder's — which is also the base every navigation it asks for resolves
/// against.
#[test]
fn a_frames_location_is_its_own() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<script>window.here = location.href;</script>'></iframe>\
         </body>",
    );
    // Read through the frame's document rather than its globals: `srcdoc`
    // inherits the embedder's URL, so both are `about:blank`-ish here and the
    // observable difference is that the frame answers at all.
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentWindow.location.href"
        ),
        s(&page, "document.URL"),
        "a srcdoc frame inherits its embedder's URL"
    );
}

/// `window.name` is the browsing context's name: seeded from `<iframe name>`,
/// writable from inside, and the key a `target` resolves against.
#[test]
fn a_context_is_named_and_renamed() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' name='side' srcdoc='<script>window.mine = window.name;</script>'>\
         </iframe></body>",
    );
    assert_eq!(s(&page, "window.name"), "", "the page starts unnamed");
    page.eval_to_string("window.name = 'top-level'; 0").unwrap();
    assert_eq!(s(&page, "window.name"), "top-level");

    // Renaming the element renames the context it embeds.
    page.eval_to_string("document.getElementById('f').name = 'other'; 0")
        .unwrap();
    page.settle(Duration::from_millis(300));
    assert_eq!(s(&page, "document.getElementById('f').name"), "other");
}

/// A link with `target="<name>"` navigates the frame with that name, in place —
/// it opens nothing.
#[test]
fn a_named_target_navigates_that_frame() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' name='side' srcdoc='<p id=p>before</p>'></iframe>\
         <a id='a' href='about:blank' target='side'>go</a></body>",
    );
    assert_eq!(
        s(
            &page,
            "!!document.getElementById('f').contentDocument.getElementById('p')"
        ),
        "true"
    );
    page.eval_to_string("document.getElementById('a').click(); 0")
        .unwrap();
    page.settle(Duration::from_millis(600));

    assert_eq!(
        s(
            &page,
            "!!document.getElementById('f').contentDocument.getElementById('p')"
        ),
        "false",
        "the named frame navigated"
    );
    // The page itself did not move, and no context was created for the link.
    assert_eq!(rendered_roots_count(&page), 2);
}

/// `_top` from inside a frame navigates the **page** — one of the two keywords
/// that used to mean "here" because there was nowhere else to go.
///
/// The destination is deliberately *not* the page's current URL: a navigation
/// to the URL a context already shows is same-document, and would prove
/// nothing.
#[test]
fn top_reaches_out_of_a_frame() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<a id=a href=\"data:text/html,<p id=landed>ok\" \
          target=\"_top\">go</a>\
         <script>setTimeout(() => document.getElementById(\"a\").click(), 0);</script>'>\
         </iframe><p id='here'>page</p></body>",
    );
    page.settle(Duration::from_millis(800));
    assert_eq!(
        s(&page, "!!document.getElementById('here')"),
        "false",
        "`_top` replaced the page's own document"
    );
    assert_eq!(
        s(&page, "!!document.getElementById('landed')"),
        "true",
        "and replaced it with what the link named"
    );
    // The page's own frame went with its document.
    assert_eq!(rendered_roots_count(&page), 1);
}

/// `<form target="<name>">` submits into the frame that answers to the name.
///
/// The response is a `data:` document, so the frame ends up cross-origin with
/// its embedder and `contentDocument` is null — a browser's answer too. The
/// assertion is therefore on the page's own view of its rendered documents,
/// which is the one thing an opaque origin does not hide.
#[test]
fn a_form_target_names_a_context() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' name='side' srcdoc='<p id=p>before</p>'></iframe>\
         <form id='form' method='post' action='data:text/html,<p>landed' \
          target='side'><input name='q' value='1'></form>\
         </body>",
    );
    let frame_url = |page: &oxidepage_page::Page| {
        let dom = page.dom();
        let top = dom.document();
        dom.rendered_roots()
            .find(|&root| root != top)
            .map(|root| dom.document_url_of(root).to_owned())
            .expect("a frame")
    };
    assert_eq!(frame_url(&page), "about:blank", "the srcdoc frame's URL");

    page.eval_to_string("document.getElementById('form').submit(); 0")
        .unwrap();
    page.settle(Duration::from_millis(600));

    assert!(
        frame_url(&page).starts_with("data:text/html"),
        "the named frame received the submission, not the page: {}",
        frame_url(&page)
    );
    // The embedder stayed where it was, and no context was created.
    assert_eq!(s(&page, "!!document.getElementById('form')"), "true");
    assert_eq!(rendered_roots_count(&page), 2);
}
