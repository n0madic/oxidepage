//! The layout deadline as a page sees it (ADR-0037 D7): the abort reaches the
//! embedder, the last good display list survives it, and a later successful
//! flush clears the flag.
//!
//! No timing assertions — `Page::set_layout_budget(Duration::ZERO)` trips at
//! the first checkpoint by construction.

use std::time::Duration;

use oxidepage_page::{PageOptions, load_html_page};

const DOC: &str = "<!DOCTYPE html><html><body style='margin:0'>\
     <div id=a style='width:120px;height:40px;background:red'>hello</div>\
     </body></html>";

#[test]
fn a_page_with_no_budget_never_reports_an_abort() {
    let page = load_html_page(DOC, PageOptions::default()).unwrap();
    let list = page.display_list();
    assert!(!list.items.is_empty());
    assert!(page.take_layout_abort().is_none());
}

#[test]
fn an_aborted_flush_is_reported_and_keeps_the_last_display_list() {
    let page = load_html_page(DOC, PageOptions::default()).unwrap();
    // A good picture first: the budget is set at runtime rather than at
    // construction precisely so there is one to preserve.
    let good = page.display_list();
    assert!(!good.items.is_empty());
    assert!(page.take_layout_abort().is_none());

    page.set_layout_budget(Duration::ZERO);
    // Move the stamp, or the flush short-circuits and never reaches a
    // checkpoint.
    page.eval_to_string("document.getElementById('a').style.height='90px'; ''")
        .expect("eval");

    let stale = page.display_list();
    let abort = page.take_layout_abort().expect("the flush aborted");
    assert!(
        abort.to_string().contains("budget"),
        "unexpected reason: {abort}"
    );
    // The blank document is *not* what a consumer gets: painting a discarded
    // box tree would replace a good picture with an empty one.
    assert_eq!(stale.items.len(), good.items.len());

    // Taken once, gone: a leftover flag would fail an unrelated later capture.
    assert!(page.take_layout_abort().is_none());
}

#[test]
fn a_later_successful_flush_clears_the_flag() {
    let page = load_html_page(DOC, PageOptions::default()).unwrap();
    page.set_layout_budget(Duration::ZERO);
    page.eval_to_string("document.getElementById('a').style.height='90px'; ''")
        .expect("eval");
    let _ = page.display_list();
    assert!(page.take_layout_abort().is_some());

    page.set_layout_budget(Duration::MAX);
    page.eval_to_string("document.getElementById('a').style.height='91px'; ''")
        .expect("eval");
    let recovered = page.display_list();
    assert!(!recovered.items.is_empty());
    assert!(
        page.take_layout_abort().is_none(),
        "a successful flush must overwrite the abort, not leave it standing"
    );
}

#[test]
fn raising_the_budget_recovers_a_page_that_never_changes() {
    // The abort gate keys on the reflow stamp, and a static document never
    // moves it — so if a *budget change* did not lift the block, an embedder
    // that raised the limit in response to the failure could never render this
    // page again. No DOM mutation anywhere below: that is the whole point.
    let page = load_html_page(DOC, PageOptions::default()).unwrap();
    page.set_layout_budget(Duration::ZERO);
    page.eval_to_string("document.getElementById('a').style.height='90px'; ''")
        .expect("eval");
    let _ = page.display_list();
    assert!(page.take_layout_abort().is_some());

    page.set_layout_budget(Duration::MAX);
    let recovered = page.display_list();
    assert!(!recovered.items.is_empty());
    assert!(page.take_layout_abort().is_none());
}

#[test]
fn a_geometry_read_from_script_answers_zero_rather_than_half_a_rectangle() {
    // The engine arms the budget itself on this path, which never reaches
    // `Page::flush_layout` (ADR-0037 D1/D6). An aborted reflow leaves no boxes,
    // so the answer is the `display: none` one — zeros, never invented
    // geometry.
    let page = load_html_page(DOC, PageOptions::default()).unwrap();
    page.set_layout_budget(Duration::ZERO);
    let widths = page
        .eval_to_string(
            "const a=document.getElementById('a');\
             a.style.width='300px';\
             `${a.offsetWidth},${a.getBoundingClientRect().width}`",
        )
        .expect("eval");
    assert_eq!(widths, "0,0");
}
