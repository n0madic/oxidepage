//! Members that name **this realm's** browsing context, not the page's
//! (ADR-0035 D1).
//!
//! `document.location`, `document.styleSheets`, `history.pushState`,
//! `new Image()` and every relative-URL resolution used to read
//! `dom.document()` / `dom.document_url()` — the *top-level* document. Inside a
//! frame each of those is a wrong answer, and three of them are wrong in a way
//! that reaches out of the frame: `pushState` rewrote the embedder's URL, and a
//! relative `fetch` aimed at the embedder's origin.
//!
//! Everything here is asserted from **inside** the frame's own realm, since
//! that is the realm whose answers changed; no `JsValue` crosses a frame
//! boundary, so `evaluate_in` is the only way to ask.

use std::time::Duration;

use oxidepage_page::{PageOptions, load_html_page};

fn page(html: &str) -> oxidepage_page::Page {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.settle(Duration::from_millis(500));
    page
}

/// The execution context of the page's `index`-th nested browsing context, in
/// tree order (`frame_tree()[0]` is the top-level one).
fn frame_context(page: &oxidepage_page::Page, index: usize) -> u64 {
    let tree = page.frame_tree();
    assert!(
        tree.len() > index,
        "expected at least {} contexts, got {}",
        index + 1,
        tree.len()
    );
    tree[index].context_id.expect("the frame has a realm")
}

fn eval_in(page: &oxidepage_page::Page, context_id: u64, source: &str) -> String {
    let result = page
        .evaluate_in(
            Some(context_id),
            source,
            &oxidepage_page::EvaluateOptions::default(),
        )
        .expect("the context exists")
        .expect_done();
    assert!(
        result.exception.is_none(),
        "{source} threw: {:?}",
        result.exception
    );
    result
        .result
        .value_json
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned()
}

/// Evaluates in the page's first nested context.
fn in_frame(page: &oxidepage_page::Page, source: &str) -> String {
    eval_in(page, frame_context(page, 1), source)
}

/// `document.location` is the frame's own `Location`, not `null`.
///
/// The getter compared `this` against the page's document, so inside any frame
/// it took the "document with no browsing context" branch and returned `null` —
/// `document.location.href` threw a `TypeError` on the most ordinary line of
/// script there is.
#[test]
fn document_location_is_not_null_inside_a_frame() {
    let page = page("<!DOCTYPE html><body><iframe id='f' srcdoc='<p>x</p>'></iframe></body>");
    assert_eq!(in_frame(&page, "document.location === null"), "false");
    assert_eq!(
        in_frame(&page, "document.location === window.location"),
        "true"
    );
    assert_eq!(in_frame(&page, "typeof document.location.href"), "string");
}

/// An inert document still reports `null` — the branch the gate exists for is
/// unchanged.
#[test]
fn document_location_is_still_null_without_a_browsing_context() {
    let page = page("<!DOCTYPE html><body></body>");
    assert_eq!(
        page.eval_to_string(
            "new DOMParser().parseFromString('<p>x</p>', 'text/html').location === null"
        )
        .unwrap(),
        "true"
    );
}

/// `document.styleSheets` lists the frame's own sheets.
///
/// They were registered in the frame's style engine all along — which is the
/// engine this realm's `cx.state.style` *is* — but the list was gated on the
/// page's document, so it reported an empty list next to a live stylist.
#[test]
fn style_sheets_are_the_frames_own() {
    let page = page(
        "<!DOCTYPE html><body><iframe id='f' srcdoc='\
         <style>p { color: rgb(9, 9, 9); }</style><p id=p>x</p>'></iframe></body>",
    );
    assert_eq!(in_frame(&page, "document.styleSheets.length"), "1");
    assert_eq!(
        in_frame(&page, "document.styleSheets[0].cssRules[0].style.color"),
        "rgb(9, 9, 9)"
    );
    // The embedder has none of its own, so the frame is not reading the page's
    // list by accident.
    assert_eq!(
        page.eval_to_string("document.styleSheets.length").unwrap(),
        "0"
    );
}

/// `history.pushState` inside a frame moves the *frame's* URL and leaves the
/// embedder's alone.
///
/// The old code read and wrote `dom.document_url()`, so a script in any frame
/// rewrote the URL of the document embedding it — after which every relative
/// subresource of the **page** resolved against a URL the page never navigated
/// to. The same-origin guard was evaluated against the embedder's URL too, so a
/// cross-origin frame was allowed to do it.
#[test]
fn push_state_in_a_frame_leaves_the_embedder_alone() {
    let page = page("<!DOCTYPE html><body><iframe id='f' srcdoc='<p>x</p>'></iframe></body>");
    let before = page.eval_to_string("document.URL").unwrap();

    assert_eq!(
        in_frame(&page, "history.pushState({}, '', '#deep'); location.hash"),
        "#deep"
    );
    assert_eq!(
        page.eval_to_string("document.URL").unwrap(),
        before,
        "pushState inside a frame moved the embedder's URL"
    );
}

/// `new Image()` belongs to the realm that called it.
///
/// Created in the page's document, an `<img>` minted inside a frame resolved
/// `src` against the embedder's base URL and routed its `load` into the
/// embedder's world — so the `new Image().src = …` preload idiom fetched the
/// wrong URL and fired nothing.
#[test]
fn new_image_belongs_to_the_calling_realm() {
    let page = page("<!DOCTYPE html><body><iframe id='f' srcdoc='<p>x</p>'></iframe></body>");
    assert_eq!(
        in_frame(&page, "new Image().ownerDocument === document"),
        "true"
    );
    assert_eq!(in_frame(&page, "new Image(3, 4).width"), "3");
}

/// A submit button with no `formaction` reads back **its own** document's URL.
#[test]
fn form_action_falls_back_to_the_frames_document_url() {
    let page = page(
        "<!DOCTYPE html><body><iframe id='f' srcdoc='\
         <form><button id=b>go</button></form>'></iframe></body>",
    );
    assert_eq!(
        in_frame(
            &page,
            "document.getElementById('b').formAction === document.URL"
        ),
        "true"
    );
}

/// A parent navigating itself must not leave its already-queued descendant
/// half-navigated.
///
/// `drain_frame_navigations` walks a `pre_order()` **snapshot**. Committing a
/// document in a frame retires everything below it (`detach_below`), so a
/// grandchild that queued in the same task is dead by the time the loop reaches
/// it: the referrer read `document_url_of(frame.document())` on a freed id — a
/// panic — and when a wrapper pinned that document instead, the load committed
/// into a context no longer in the table, leaving a rendered root nothing ever
/// removes.
#[test]
fn a_frame_navigation_does_not_strand_its_queued_descendant() {
    let page = page(
        "<!DOCTYPE html><body><iframe id='a' name='fa' srcdoc='\
         <iframe name=fb srcdoc=\"<p id=deep>deep</p>\"></iframe>'></iframe></body>",
    );
    assert_eq!(page.frame_tree().len(), 3, "main + a + b");

    // Both queued in **one** task, so both are in the batch the drain walks.
    // Through a *named* target, which is what lands on a context's own
    // `pending_navigation` queue — `srcdoc =` and `contentWindow.location =`
    // both write the `<iframe>`'s attributes instead, a different task source
    // that never reaches this code path.
    page.eval_to_string(
        "window.open('data:text/html,<p id=b2>b2</p>', 'fb');\
         window.open('data:text/html,<p id=a2>a2</p>', 'fa');\
         0",
    )
    .unwrap();
    page.settle(Duration::from_millis(800));

    // `a` committed, which discards `b`. Nothing may be left over from the
    // navigation `b` had queued.
    assert_eq!(
        page.frame_tree().len(),
        2,
        "the grandchild went with its parent's commit"
    );
    assert_eq!(
        page.dom().rendered_roots().count(),
        2,
        "a document was committed into a retired context and never released"
    );
    // …and `a` really did navigate — the drain is not simply skipping both.
    // Read off the frame tree rather than through `contentDocument`, which is
    // `null` here: a `data:` document has an opaque origin.
    let tree = page.frame_tree();
    assert!(
        tree[1].url.contains("a2"),
        "the parent frame's own navigation did not happen: {:?}",
        tree.iter().map(|f| f.url.as_str()).collect::<Vec<_>>()
    );
}

/// Which of two contexts sharing a name wins is decided by **tree order**, and
/// tree order is depth-first: a frame's own subtree precedes its next sibling.
///
/// The frontier that resolved this was LIFO, so `main > [a > deep, b]` came out
/// as `[main, a, b, deep]` — `b`'s whole subtree before `a`'s child. With two
/// contexts named `side` the winner then depended on the shape of the tree
/// rather than on document order.
#[test]
fn a_name_collision_is_broken_in_depth_first_order() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='a' srcdoc='<iframe name=side srcdoc=\"<p>deep</p>\"></iframe>'></iframe>\
         <iframe id='b' name='side' srcdoc='<p>flat</p>'></iframe>\
         </body>",
    );
    assert_eq!(page.frame_tree().len(), 4, "main + a + a's child + b");

    // `a`'s descendant precedes `b` in document order, so it is the `side` a
    // named target resolves to.
    page.eval_to_string("window.open('data:text/html,<p>landed', 'side'); 0")
        .unwrap();
    page.settle(Duration::from_millis(600));

    let tree = page.frame_tree();
    let urls: Vec<&str> = tree.iter().map(|frame| frame.url.as_str()).collect();
    let landed: Vec<usize> = urls
        .iter()
        .enumerate()
        .filter(|(_, url)| url.starts_with("data:text/html"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(landed, vec![2], "the deeper `side` wins; tree = {urls:?}");
}
