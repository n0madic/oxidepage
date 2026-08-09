//! Nested browsing contexts (ADR-0035): an `<iframe>` owns a real document in
//! the shared arena, with its own style and layout engines and its own realm.
//!
//! These cover the *context*, not navigation: HTML creates a nested browsing
//! context when the element is inserted and discards it when the element
//! leaves, independently of `src`.

use oxidepage_page::{PageOptions, load_html_page};

fn page(html: &str) -> oxidepage_page::Page {
    load_html_page(html, PageOptions::default()).unwrap()
}

fn s(page: &oxidepage_page::Page, expr: &str) -> String {
    page.eval_to_string(expr).unwrap()
}

/// Every rendered document of the page, top-level one included.
fn rendered_roots(page: &oxidepage_page::Page) -> Vec<oxidepage_base::NodeId> {
    let dom = page.dom();
    let mut roots: Vec<_> = dom.rendered_roots().collect();
    // The set is unordered; sort so assertions are stable.
    roots.sort_by_key(|id| (id.index(), id.generation()));
    roots
}

#[test]
fn an_iframe_owns_a_rendered_document() {
    let page = page("<!DOCTYPE html><body><iframe id='f'></iframe></body>");
    let roots = rendered_roots(&page);
    assert_eq!(roots.len(), 2, "the page document plus the frame's");

    let dom = page.dom();
    // The top-level document is still slot 0, and the frame's is a different
    // node in the *same* arena.
    assert_eq!(roots[0], dom.document());
    assert_ne!(roots[1], dom.document());
    // A fresh browsing context starts at `about:blank`, whatever `src` says.
    assert_eq!(dom.document_url_of(roots[1]), "about:blank");
    // It is connected — that is what makes style, layout and loading reach it.
    assert!(dom.is_rendered_root(roots[1]));
    assert!(dom.node(roots[1]).is_connected());
}

#[test]
fn a_document_with_no_iframe_has_one_browsing_context() {
    let page = page("<!DOCTYPE html><body><div></div><p>text</p></body>");
    assert_eq!(rendered_roots(&page).len(), 1);
}

#[test]
fn each_iframe_gets_its_own_context() {
    let page = page("<!DOCTYPE html><body><iframe></iframe><iframe></iframe></body>");
    let roots = rendered_roots(&page);
    assert_eq!(roots.len(), 3);
    assert_ne!(roots[1], roots[2], "two frames, two documents");
}

#[test]
fn a_script_created_iframe_gets_a_context_on_insertion() {
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           const f = document.createElement('iframe');\
           window.beforeInsert = document.querySelectorAll('iframe').length;\
           document.body.appendChild(f);\
         </script>\
         </body>",
    );
    assert_eq!(s(&page, "window.beforeInsert"), "0");
    assert_eq!(s(&page, "document.querySelectorAll('iframe').length"), "1");
    assert_eq!(
        rendered_roots(&page).len(),
        2,
        "insertion creates the context, not construction"
    );
}

/// A detached `<iframe>` element is *not* a browsing context: HTML creates one
/// on insertion into a document that has one, and `createElement` alone does
/// not insert.
#[test]
fn a_detached_iframe_has_no_context() {
    let page = page(
        "<!DOCTYPE html><body>\
         <script>window.f = document.createElement('iframe');</script>\
         </body>",
    );
    assert_eq!(rendered_roots(&page).len(), 1);
}

#[test]
fn removing_an_iframe_discards_its_context() {
    let page = page("<!DOCTYPE html><body><iframe id='f'></iframe></body>");
    assert_eq!(rendered_roots(&page).len(), 2);

    page.eval_to_string("document.getElementById('f').remove(); 0")
        .unwrap();
    page.settle(std::time::Duration::from_millis(200));
    assert_eq!(
        rendered_roots(&page).len(),
        1,
        "the frame's document stops being rendered when its element leaves"
    );
}

/// A frame's context outlives the task that created it, and the page keeps
/// running normally around it: creating a realm mid-loop must not disturb the
/// main world's own scheduling.
#[test]
fn the_page_keeps_running_around_a_frame() {
    let page = page(
        "<!DOCTYPE html><body><iframe id='f'></iframe>\
         <script>\
           window.ticks = 0;\
           setTimeout(() => { window.ticks += 1; }, 0);\
         </script>\
         </body>",
    );
    page.settle(std::time::Duration::from_millis(200));
    assert_eq!(s(&page, "window.ticks"), "1");
    assert_eq!(rendered_roots(&page).len(), 2);
}

/// The frame's document is a real document of the *same* arena, so the parent
/// realm can reach it — but it is a different document, so the parent's
/// `getElementById` must not see into it and vice versa.
#[test]
fn a_frame_document_is_scoped_for_id_lookups() {
    let page = page("<!DOCTYPE html><body><iframe></iframe><p id='here'>x</p></body>");
    let dom = page.dom();
    let roots = rendered_roots(&page);
    let child = roots[1];

    assert!(dom.element_by_id(dom.document(), "here").is_some());
    assert!(
        dom.element_by_id(child, "here").is_none(),
        "the id index spans every rendered document and must be scoped"
    );
}

/// A fresh context's document is genuinely empty — the `<iframe>`'s own DOM
/// children are *not* its content, and nothing has been parsed into it yet.
///
/// HTML would give an initial `about:blank` document an
/// `<html><head></head><body></body></html>`; that arrives with navigation,
/// and this test is what will notice when it does.
#[test]
fn a_fresh_context_starts_empty() {
    let page = page("<!DOCTYPE html><body><iframe><p id='inside'>x</p></iframe></body>");
    let dom = page.dom();
    let child = rendered_roots(&page)[1];

    assert_eq!(dom.document_element_of(child), None);
    assert_eq!(dom.children(child).count(), 0);
    // And the markup written inside the element is not content either: HTML
    // parses `<iframe>` as raw text, so that `<p>` is a text node in the
    // *parent* document, never an element and never the frame's.
    assert!(dom.element_by_id(dom.document(), "inside").is_none());
    assert!(dom.element_by_id(child, "inside").is_none());
}
