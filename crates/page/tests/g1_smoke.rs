//! WP-G1 smoke: geometry partials reachable from JS.

use oxidepage_page::{PageOptions, load_html_page};

fn eval(html: &str, expr: &str) -> String {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.eval_to_string(expr).expect("eval")
}

const DOC: &str = "<!DOCTYPE html><html><body style='margin:0'>\
    <div id=d style='width: 100px; height: 40px; padding: 5px; \
    border: 2px solid black; margin: 3px'>x</div></body></html>";

#[test]
fn geometry_partials_smoke() {
    assert_eq!(
        eval(
            DOC,
            "document.getElementById('d').getBoundingClientRect().width"
        ),
        "114"
    );
    assert_eq!(eval(DOC, "document.getElementById('d').clientTop"), "2");
    assert_eq!(eval(DOC, "document.getElementById('d').clientWidth"), "110");
    // A static body as offsetParent reports ICB-relative offsets, so both axes
    // are the div's border-box origin: its 3px margin, in full. (The vertical
    // margin collapses through the body, moving the body's border box down with
    // the div's — measuring from the body would cancel offsetTop out to 0.)
    assert_eq!(eval(DOC, "document.getElementById('d').offsetTop"), "3");
    assert_eq!(eval(DOC, "document.getElementById('d').offsetLeft"), "3");
    assert_eq!(
        eval(DOC, "document.getElementById('d').offsetParent.tagName"),
        "BODY"
    );
    assert_eq!(eval(DOC, "document.elementFromPoint(50, 20).id"), "d");
    assert_eq!(eval(DOC, "document.documentElement.clientWidth"), "800");
    assert_eq!(
        eval(
            DOC,
            "document.scrollingElement === document.documentElement"
        ),
        "true"
    );
    assert_eq!(
        eval(DOC, "document.getElementById('d').getClientRects().length"),
        "1"
    );
}
