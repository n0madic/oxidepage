//! The layout deadline and its `catch_unwind` boundary (ADR-0037).
//!
//! Every assertion is on layout output or on a counter, never on wall-clock
//! time — the same rule `grid.rs` states for the same reason: a timing
//! assertion flakes on a loaded machine. Determinism comes from
//! [`Duration::ZERO`], which trips at the first checkpoint by construction,
//! not by racing the clock.

use std::time::Duration;

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_layout::{LayoutAborted, LayoutEngine, arm_layout_budget};
use oxidepage_style::{StyleEngine, Viewport};

/// Enough boxes that the abort has somewhere to land, and enough geometry that
/// a recovered layout is worth comparing.
const DOC: &str = "<body style='margin: 0'>\
     <div id=a style='width: 100px; height: 20px'>one</div>\
     <div id=b style='width: 50%; height: 30px'><span>two</span></div>\
     <div id=c style='display: flex'><i style='width: 10px; height: 10px'></i></div>\
     </body>";

fn find_by_id(tree: &DomTree, id_attr: &str) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| el.id().map(|a| &**a) == Some(id_attr))
        })
        .unwrap_or_else(|| panic!("no element with id={id_attr}"))
}

fn parsed(html: &str) -> (DomTree, StyleEngine, LayoutEngine) {
    let dom = parse_document(html, ParseOptions::default()).tree;
    let style = StyleEngine::new(&dom, Viewport::default());
    let layout = LayoutEngine::new(Viewport::default());
    (dom, style, layout)
}

/// The border boxes of `DOC`'s three named elements, in order.
fn geometry(layout: &LayoutEngine, dom: &DomTree) -> Vec<oxidepage_base::Rect> {
    ["a", "b", "c"]
        .into_iter()
        .map(|id| {
            layout
                .border_box(find_by_id(dom, id))
                .unwrap_or_else(|| panic!("no box for #{id}"))
        })
        .collect()
}

#[test]
fn zero_budget_aborts_with_a_typed_error() {
    let (mut dom, mut style, mut layout) = parsed(DOC);
    let _budget = arm_layout_budget(Duration::ZERO);
    assert_eq!(
        layout.reflow(&mut dom, &mut style),
        Err(LayoutAborted::Deadline {
            limit: Duration::ZERO
        })
    );
    // The tree is discarded wholesale, so there is no half-laid-out geometry to
    // read back — a query answers what it answers for `display: none`.
    assert_eq!(layout.tree().box_count(), 0);
    assert!(layout.border_box(find_by_id(&dom, "a")).is_none());
}

#[test]
fn an_unarmed_reflow_is_unaffected() {
    // The control, as in `grid.rs`: with no budget armed the checkpoints are
    // dead code and the geometry is exactly what it was before they existed.
    let (mut dom, mut style, mut layout) = parsed(DOC);
    layout
        .reflow(&mut dom, &mut style)
        .expect("layout completes");
    let boxes = geometry(&layout, &dom);
    assert_eq!(boxes[0].size.width, 100.0);
    assert_eq!(boxes[0].size.height, 20.0);
    assert_eq!(boxes[1].size.width, 400.0);
    assert_eq!(boxes[1].size.height, 30.0);
    assert_eq!(boxes[2].origin.y, 50.0);
}

#[test]
fn a_second_reflow_at_the_same_stamp_fails_fast() {
    let (mut dom, mut style, mut layout) = parsed(DOC);
    {
        let _budget = arm_layout_budget(Duration::ZERO);
        layout
            .reflow(&mut dom, &mut style)
            .expect_err("the zero budget trips");
    }
    // The budget is gone, and yet: the stamp has not moved, so the flush is
    // known to fail and must not be attempted again. Counted rather than
    // timed — no work at all is the observable, and a clock cannot say that.
    let counts = layout.reflow_counts();
    let again = layout.reflow(&mut dom, &mut style);
    assert_eq!(
        again,
        Err(LayoutAborted::Deadline {
            limit: Duration::ZERO
        })
    );
    assert_eq!(layout.reflow_counts(), counts, "the retry redid the work");
}

#[test]
fn recovery_rebuilds_from_scratch() {
    // The reference: the same mutation applied to a document that never
    // aborted. A recovered layout must match it exactly — which it cannot if
    // the stale build snapshot survived (it would let the next reflow *patch* a
    // tree that was never laid out) or if `taffy_impl`'s style latches were
    // left flipped by the unwind.
    let expected = {
        let (mut dom, mut style, mut layout) = parsed(DOC);
        layout
            .reflow(&mut dom, &mut style)
            .expect("layout completes");
        mutate(&mut dom);
        layout
            .reflow(&mut dom, &mut style)
            .expect("layout completes");
        geometry(&layout, &dom)
    };

    let (mut dom, mut style, mut layout) = parsed(DOC);
    {
        let _budget = arm_layout_budget(Duration::ZERO);
        layout
            .reflow(&mut dom, &mut style)
            .expect_err("the zero budget trips");
    }
    // Any version bump moves the stamp and lifts the fail-fast block; no manual
    // reset anywhere.
    mutate(&mut dom);
    layout
        .reflow(&mut dom, &mut style)
        .expect("the recovered reflow completes");
    assert_eq!(geometry(&layout, &dom), expected);
    let (rebuilds, patches) = layout.reflow_counts();
    assert!(rebuilds >= 1, "recovery must rebuild");
    assert_eq!(patches, 0, "the drained restyle set cannot be patched from");
}

fn mutate(dom: &mut DomTree) {
    let a = find_by_id(dom, "a");
    dom.set_attribute(
        a,
        oxidepage_dom::node::attr_name(html5ever::local_name!("style")),
        "width: 120px; height: 44px".into(),
    );
}

#[test]
fn a_reflow_from_a_geometry_query_is_budgeted() {
    // The second level of arming (ADR-0037 D1): nothing above has armed a
    // budget, exactly as on the `el.offsetWidth` path, which never goes through
    // `Page::flush_layout`. The engine's own limit has to be what bounds it.
    let (mut dom, mut style, mut layout) = parsed(DOC);
    layout.set_budget(Duration::ZERO);
    assert_eq!(
        layout.reflow(&mut dom, &mut style),
        Err(LayoutAborted::Deadline {
            limit: Duration::ZERO
        })
    );
    assert_eq!(layout.tree().box_count(), 0);
}

#[test]
fn an_outer_budget_wins_over_the_engines_own() {
    // A whole-page flush arms once and every frame's engine inherits that one
    // deadline; a per-engine limit must not restart it.
    let (mut dom, mut style, mut layout) = parsed(DOC);
    layout.set_budget(Duration::ZERO);
    let _budget = arm_layout_budget(Duration::from_secs(3600));
    layout
        .reflow(&mut dom, &mut style)
        .expect("the outer budget owns the deadline");
    assert!(layout.tree().box_count() > 0);
}

#[test]
fn a_disabled_budget_never_trips() {
    let (mut dom, mut style, mut layout) = parsed(DOC);
    layout.set_budget(Duration::MAX);
    let _budget = arm_layout_budget(Duration::MAX);
    layout
        .reflow(&mut dom, &mut style)
        .expect("layout completes");
    assert!(layout.tree().box_count() > 0);
}

/// `catch_unwind` is not only for our own deadline: a panic raised *by the
/// layout pass* is classified and returned, where before it unwound through
/// `reflow` and killed the page thread (ADR-0037 D4).
///
/// `repeat(auto-fill, 1px)` past 65 535 repetitions overflows the `u16` in
/// taffy's `explicit_grid.rs` — the case ADR-0036 D5 named as this boundary's
/// target. Debug-only, because that is an `attempt to add with overflow`:
/// release wraps silently to zero tracks and lays out fine, so asserting on it
/// there would assert on the wrong thing.
#[cfg(debug_assertions)]
#[test]
fn a_foreign_panic_is_classified_not_propagated() {
    let (mut dom, mut style, mut layout) = parsed(
        "<div style='width: 70000px; height: 70000px; display: grid; \
         grid-template-columns: repeat(auto-fill, 1px); \
         grid-template-rows: repeat(auto-fill, 1px)'></div>",
    );
    match layout.reflow(&mut dom, &mut style) {
        Err(LayoutAborted::EnginePanic(message)) => {
            assert!(!message.is_empty(), "the panic's own message is kept");
        }
        other => panic!("expected a classified engine panic, got {other:?}"),
    }
    assert_eq!(layout.tree().box_count(), 0);
}
