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

/// A fresh context's document is an *empty HTML document*, not an empty node:
/// HTML gives an initial `about:blank` an `<html><head></head><body></body>`,
/// which is what makes `contentDocument.body` writable before any navigation —
/// the `createElement('iframe')` + `appendChild` + write idiom.
///
/// The `<iframe>`'s own DOM children are still not its content.
#[test]
fn a_fresh_context_starts_as_an_empty_html_document() {
    let page = page("<!DOCTYPE html><body><iframe><p id='inside'>x</p></iframe></body>");
    let dom = page.dom();
    let child = rendered_roots(&page)[1];

    let root = dom.document_element_of(child).expect("a document element");
    let name = |id| {
        dom.get(id)
            .and_then(|node| node.as_element())
            .map(|el| el.name.local.to_string())
    };
    assert_eq!(name(root).as_deref(), Some("html"));
    let mut kids = dom.children(root).filter_map(name);
    assert_eq!(kids.next().as_deref(), Some("head"));
    assert_eq!(kids.next().as_deref(), Some("body"));
    // And the markup written inside the element is not content either: HTML
    // parses `<iframe>` as raw text, so that `<p>` is a text node in the
    // *parent* document, never an element and never the frame's.
    assert!(dom.element_by_id(dom.document(), "inside").is_none());
    assert!(dom.element_by_id(child, "inside").is_none());
}

/// `iframe.contentDocument` is the frame's real `Document` — the arena is
/// shared, so the parent realm wraps it with no value crossing a runtime
/// boundary (ADR-0035 D4).
#[test]
fn content_document_is_the_frames_own_document() {
    let page = page("<!DOCTYPE html><body><iframe id='f'></iframe></body>");
    let child = rendered_roots(&page)[1];

    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument === document"
        ),
        "false"
    );
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument.nodeType"
        ),
        "9"
    );
    assert_eq!(
        s(&page, "document.getElementById('f').contentDocument.URL"),
        "about:blank"
    );
    // The wrapper names the very node the frame renders, and that node is a
    // real (if empty) HTML document.
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument.documentElement.tagName"
        ),
        "HTML"
    );
    assert!(page.dom().is_rendered_root(child));
}

/// A detached `<iframe>` has no context, so `contentDocument` is null — and
/// removing a frame clears the pointer rather than leaving it naming a
/// document nothing renders.
#[test]
fn content_document_is_null_without_a_context() {
    let page = page(
        "<!DOCTYPE html><body><iframe id='f'></iframe>\
         <script>window.detached = document.createElement('iframe');</script>\
         </body>",
    );
    assert_eq!(s(&page, "window.detached.contentDocument"), "null");

    page.eval_to_string("document.getElementById('f').remove(); 0")
        .unwrap();
    page.settle(std::time::Duration::from_millis(200));
    assert_eq!(rendered_roots(&page).len(), 1);
}

/// The frame element reflects the attributes that describe it.
#[test]
fn iframe_reflects_its_attributes() {
    let page = page("<!DOCTYPE html><body><iframe id='f' name='side' width='300'></iframe></body>");
    assert_eq!(s(&page, "document.getElementById('f').name"), "side");
    assert_eq!(s(&page, "document.getElementById('f').width"), "300");
    assert_eq!(s(&page, "document.getElementById('f').height"), "");

    assert_eq!(s(&page, "'name' in document.getElementById('f')"), "true");
    assert_eq!(s(&page, "'src' in document.getElementById('f')"), "true");
    assert_eq!(s(&page, "'srcdoc' in document.getElementById('f')"), "true");
}

/// `srcdoc` loads its markup into the frame's own document, and that document
/// really is separate: its elements are not in the parent's id index and the
/// parent's are not in its.
#[test]
fn srcdoc_loads_into_the_frame() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<p id=inner>hello</p>'></iframe>\
         <p id='outer'>x</p></body>",
    );
    let dom = page.dom();
    let child = rendered_roots(&page)[1];

    assert!(
        dom.document_element_of(child).is_some(),
        "the frame document was parsed into"
    );
    assert!(dom.element_by_id(child, "inner").is_some());
    assert!(dom.element_by_id(child, "outer").is_none());
    assert!(dom.element_by_id(dom.document(), "inner").is_none());
    assert!(dom.element_by_id(dom.document(), "outer").is_some());
}

/// A script inside a `srcdoc` frame runs in **that frame's** realm: its
/// `document` is the frame's, not the page's.
#[test]
fn a_frame_script_runs_in_the_frames_realm() {
    let page = page(
        "<!DOCTYPE html><body>\
         <iframe id='f' srcdoc='<p id=inner>hi</p><script>\
           document.getElementById(\"inner\").textContent = \"touched\";\
         </script>'></iframe></body>",
    );
    let dom = page.dom();
    let child = rendered_roots(&page)[1];
    let inner = dom
        .element_by_id(child, "inner")
        .expect("the frame parsed its markup");
    assert_eq!(dom.text_content(inner), "touched");
}

/// Writing `src`/`srcdoc` from script navigates the frame — the setter queues
/// it and the event loop performs the load, so it lands by the next settle.
#[test]
fn setting_srcdoc_navigates_the_frame() {
    let page = page("<!DOCTYPE html><body><iframe id='f'></iframe></body>");
    // Nothing of the new markup is there yet — the initial document is the
    // empty `about:blank` one.
    assert!(
        page.dom()
            .element_by_id(rendered_roots(&page)[1], "late")
            .is_none()
    );

    page.eval_to_string("document.getElementById('f').srcdoc = '<p id=late>after</p>'; 0")
        .unwrap();
    page.settle(std::time::Duration::from_millis(500));

    let dom = page.dom();
    let child = rendered_roots(&page)[1];
    assert!(
        dom.element_by_id(child, "late").is_some(),
        "the frame navigated to the new srcdoc"
    );
    // Still exactly two rendered documents: the navigation replaced the
    // frame's document rather than adding one.
    assert_eq!(dom.rendered_roots().count(), 2);
}

/// `load` fires on the element, in the embedding document — the element lives
/// there, not in the frame.
#[test]
fn load_fires_on_the_iframe_element() {
    let page = page(
        "<!DOCTYPE html><body>\
         <script>\
           window.loaded = 0;\
           const f = document.createElement('iframe');\
           f.addEventListener('load', () => { window.loaded += 1; });\
           f.srcdoc = '<p>x</p>';\
           document.body.appendChild(f);\
         </script></body>",
    );
    page.settle(std::time::Duration::from_millis(500));
    assert_eq!(s(&page, "window.loaded"), "1");
}

/// An `<iframe>` is a replaced element: 300×150 by CSS default, and its size
/// comes from CSS and its attributes, never from what it contains. That is
/// what makes one reflow pass enough — the child cannot resize its embedder.
#[test]
fn an_iframe_is_a_replaced_box() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <iframe id='default'></iframe>\
         <iframe id='attrs' width='400' height='90'></iframe>\
         <iframe id='styled' style='width:250px;height:60px'></iframe>\
         </body>",
    );
    assert_eq!(
        s(&page, "document.getElementById('default').offsetWidth"),
        "300"
    );
    assert_eq!(
        s(&page, "document.getElementById('default').offsetHeight"),
        "150"
    );
    assert_eq!(
        s(&page, "document.getElementById('attrs').offsetWidth"),
        "400"
    );
    assert_eq!(
        s(&page, "document.getElementById('attrs').offsetHeight"),
        "90"
    );
    assert_eq!(
        s(&page, "document.getElementById('styled').offsetWidth"),
        "250"
    );
    assert_eq!(
        s(&page, "document.getElementById('styled').offsetHeight"),
        "60"
    );
}

/// Tall content inside a frame does not grow the frame's box.
#[test]
fn frame_content_does_not_resize_the_frame() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <iframe id='f' srcdoc='<div style=\"height:5000px\"></div>'></iframe></body>",
    );
    page.settle(std::time::Duration::from_millis(500));
    assert_eq!(s(&page, "document.getElementById('f').offsetHeight"), "150");
}

/// The frame's own document lays out into the `<iframe>`'s content box, so a
/// block inside it fills that width rather than the page's.
#[test]
fn a_frame_lays_out_in_its_own_viewport() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <iframe id='f' width='200' height='100' \
          srcdoc='<body style=\"margin:0\"><div id=inner style=\"height:10px\"></div></body>'>\
         </iframe></body>",
    );
    page.settle(std::time::Duration::from_millis(500));

    // Measured from the *parent's* realm through `contentDocument`: geometry
    // routes to the engine of the frame that renders the node, so this is the
    // frame's layout, not the page's.
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument             .getElementById('inner').offsetWidth"
        ),
        "200"
    );
    assert_eq!(
        s(
            &page,
            "document.getElementById('f').contentDocument             .getElementById('inner').offsetHeight"
        ),
        "10"
    );
}

/// The `<iframe>`'s DOM children never render: HTML parses its contents as raw
/// text (fallback for UAs without frames), and the box is a leaf.
#[test]
fn iframe_children_do_not_render() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <iframe id='f' style='width:100px;height:40px'>fallback text</iframe></body>",
    );
    // The box keeps its specified size: the fallback text contributes nothing.
    assert_eq!(s(&page, "document.getElementById('f').offsetHeight"), "40");
}

/// The frame's content reaches the page's display list: `paint` is handed the
/// child's list and splices it inside a clip at the `<iframe>`'s content box
/// (ADR-0035 D7).
#[test]
fn frame_content_reaches_the_display_list() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <iframe id='f' width='200' height='100' \
          srcdoc='<body style=\"margin:0\">\
            <div style=\"width:50px;height:20px;background:rgb(1,2,3)\"></div></body>'>\
         </iframe></body>",
    );
    page.settle(std::time::Duration::from_millis(500));

    let json = page.display_list_json();
    // The splice is structural: a clip at the `<iframe>`'s content box, a layer
    // translating to its origin, the frame's own items, then the two pops.
    let clip = json
        .find("\"PushClip\"")
        .expect("the frame's content is clipped to its box");
    let layer = json[clip..]
        .find("\"PushLayer\"")
        .expect("and translated into place");
    let painted = json[clip + layer..]
        .find("#010203ff")
        .expect("the div painted inside the frame reaches the page's list");
    let pop = json[clip + layer + painted..]
        .find("\"PopLayer\"")
        .expect("and the layer closes after it");
    assert!(pop > 0);
    // The clip is the iframe's 200x100 content box, not the page viewport.
    assert!(
        json[clip..].contains("200.0"),
        "clipped to the frame box:\n{json}"
    );
}

/// A page with no frames pays nothing: no splice, so no clip/layer pair
/// appears around its content.
#[test]
fn a_frameless_page_gains_no_splice() {
    let page = page(
        "<!DOCTYPE html><body style='margin:0'>\
         <div style='width:10px;height:10px;background:rgb(4,5,6)'></div></body>",
    );
    let json = page.display_list_json();
    assert!(json.contains("#040506ff"), "the div painted:\n{json}");
    assert!(
        !json.contains("\"PushClip\""),
        "nothing to splice, so nothing is clipped:\n{json}"
    );
}

/// A scroll inside a frame fires `scroll` **in that frame**, on that frame's
/// document.
///
/// The queue lives on the context, one per frame, and the page used to drain
/// only the top-level one — so no `scroll` event ever fired in any frame and
/// the entries piled up for the life of the document.
#[test]
fn a_scroll_inside_a_frame_fires_its_own_event() {
    let page = page(
        "<!DOCTYPE html><body><iframe id='f' srcdoc='\
         <script>window.hits = 0; window.docHits = 0;\
           window.addEventListener(\"scroll\", () => { window.hits++; });\
           document.addEventListener(\"scroll\", () => { window.docHits++; });\
         </script><div style=\"height:3000px\"></div>'></iframe>\
         <script>window.outerHits = 0;\
           window.addEventListener(\"scroll\", () => { window.outerHits++; });\
         </script></body>",
    );
    page.settle(std::time::Duration::from_millis(500));

    let tree = page.frame_tree();
    let ctx = tree[1].context_id.expect("the frame has a realm");
    let read = |expr: &str| {
        page.evaluate_in(Some(ctx), expr, &oxidepage_page::EvaluateOptions::default())
            .expect("the context exists")
            .expect_done()
            .result
            .value_json
            .unwrap_or_default()
            .trim_matches('"')
            .to_owned()
    };
    read("window.scrollTo(0, 500)");
    page.settle(std::time::Duration::from_millis(300));

    assert_eq!(read("window.scrollY"), "500");
    assert_eq!(read("window.hits"), "1", "the frame's window heard it");
    // The viewport target is *this* frame's document, not the page's — so a
    // document listener in the frame hears it and bubbles on to its window.
    assert_eq!(read("window.docHits"), "1");
    // And the embedder heard nothing: events do not cross the boundary.
    assert_eq!(s(&page, "window.outerHits"), "0");
}

/// A link whose `target` names nothing navigates **in place**.
///
/// HTML would create a context under that name and reuse it for every later
/// link naming it. There is no registry of page names here, so opening one per
/// click would be unbounded — and would change behaviour once the popup cap was
/// reached, making the same link act differently depending on how often it had
/// been clicked. `form_submit.rs` already handles the identical case this way.
#[test]
fn a_link_naming_no_context_does_not_open_a_page() {
    let opened = std::rc::Rc::new(std::cell::Cell::new(0usize));
    // A fragment `href`, so navigating in place is same-document and the link
    // is still there for the next click — which is the point being made.
    let page = page("<!DOCTYPE html><body><a id='a' href='#x' target='report'>go</a></body>");
    {
        let opened = std::rc::Rc::clone(&opened);
        page.set_open_window_handler(Some(std::rc::Rc::new(move |_request: &_| {
            opened.set(opened.get() + 1);
            Some(oxidepage_bindings::OpenedWindow {
                closed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                ops: std::sync::Arc::new(|_op| {}),
            })
        })));
    }
    for _ in 0..3 {
        s(&page, "document.getElementById('a').click(); 0");
        page.settle(std::time::Duration::from_millis(200));
    }
    assert_eq!(opened.get(), 0, "a named target opened a page per click");

    // `_blank` still does, which is the case the hook exists for.
    s(
        &page,
        "document.getElementById('a').target = '_blank'; \
         document.getElementById('a').click(); 0",
    );
    page.settle(std::time::Duration::from_millis(200));
    assert_eq!(opened.get(), 1);
}
