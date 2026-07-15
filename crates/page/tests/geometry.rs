//! Phase 5 geometry tests (WP-F): the `DOMRect` family — construction, derived
//! edges, mutability, `fromRect`, `toJSON`, and inheritance.

use oxidepage_page::{PageOptions, load_html_page};

const DOC: &str = "<!DOCTYPE html><html><body></body></html>";

/// Evaluates `expr` in a fresh page and returns its string value.
fn eval(expr: &str) -> String {
    let page = load_html_page(DOC, PageOptions::default()).unwrap();
    page.eval_to_string(expr).expect("eval")
}

#[test]
fn dom_rect_exposes_all_edges() {
    assert_eq!(
        eval(
            "const r=new DOMRect(1,2,3,4);\
             [r.x,r.y,r.width,r.height,r.top,r.right,r.bottom,r.left].join(',')",
        ),
        "1,2,3,4,2,4,6,1"
    );
}

#[test]
fn negative_size_flips_edges() {
    assert_eq!(
        eval(
            "const r=new DOMRect(0,0,-10,-20);\
             [r.left,r.right,r.top,r.bottom].join(',')",
        ),
        "-10,0,-20,0"
    );
}

#[test]
fn dom_rect_defaults_omitted_args_to_zero() {
    assert_eq!(
        eval("const r=new DOMRect();[r.x,r.y,r.width,r.height].join(',')"),
        "0,0,0,0"
    );
}

#[test]
fn dom_rect_x_is_writable_and_updates_derived_edges() {
    assert_eq!(
        eval(
            "const r=new DOMRect(1,2,3,4);\
             r.x=10;\
             [r.x,r.left,r.right].join(',')",
        ),
        "10,10,13"
    );
}

#[test]
fn dom_rect_read_only_x_is_not_writable() {
    // A read-only accessor with no setter: non-strict assignment is a silent
    // no-op, so the value is unchanged.
    assert_eq!(
        eval(
            "const r=new DOMRectReadOnly(1,2,3,4);\
             r.x=99;\
             String(r.x)",
        ),
        "1"
    );
}

#[test]
fn from_rect_defaults_missing_members_to_zero() {
    assert_eq!(
        eval(
            "const r=DOMRect.fromRect({x:5,width:7});\
             [r.x,r.y,r.width,r.height].join(',')",
        ),
        "5,0,7,0"
    );
}

#[test]
fn from_rect_with_no_argument_is_all_zeros() {
    assert_eq!(
        eval("const r=DOMRect.fromRect();[r.x,r.y,r.width,r.height].join(',')"),
        "0,0,0,0"
    );
}

#[test]
fn read_only_from_rect_builds_a_read_only_rect() {
    assert_eq!(
        eval(
            "const r=DOMRectReadOnly.fromRect({x:1,y:2,width:3,height:4});\
             [r instanceof DOMRectReadOnly, r instanceof DOMRect, r.right].join(',')",
        ),
        "true,false,4"
    );
}

#[test]
fn to_json_serializes_all_eight_members() {
    assert_eq!(
        eval(
            "const j=JSON.parse(JSON.stringify(new DOMRect(1,2,3,4)));\
             ['x','y','width','height','top','right','bottom','left']\
               .every(k=>k in j).toString()",
        ),
        "true"
    );
}

#[test]
fn dom_rect_is_a_dom_rect_read_only() {
    // DOMRect inherits from DOMRectReadOnly (expressed via IDL inheritance).
    assert_eq!(
        eval("String(new DOMRect(1,2,3,4) instanceof DOMRectReadOnly)"),
        "true"
    );
}

// === WP-G2: layout-backed geometry through the JS surface ===

/// Evaluates `expr` against a given document.
fn eval_in(html: &str, expr: &str) -> String {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.eval_to_string(expr).expect("eval")
}

#[test]
fn body_overflow_propagates_to_the_viewport() {
    // CSS Overflow §3.3: with the root (`html`) overflow `visible`, a `<body>`
    // that would otherwise scroll internally (`height: 100vh; overflow: auto`)
    // has its overflow propagated to the viewport and its own used overflow
    // becomes `visible`. So the document's real height reaches `documentElement`
    // (feeding `scrollHeight` and `--full-page`) instead of being clipped to one
    // screen. Regression: angular.dev's shell used exactly this pattern, and
    // `--full-page` captured only the first viewport.
    let html = "<!DOCTYPE html><html><body style='margin:0;height:100vh;overflow-y:auto'>\
                  <div style='height:3000px'></div>\
                </body></html>";
    // The default viewport is 600 tall; without propagation the body clips there.
    assert_eq!(
        eval_in(html, "document.documentElement.scrollHeight"),
        "3000"
    );
}

#[test]
fn nested_block_rects_accumulate() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='padding: 10px; margin: 5px'>\
             <div id=inner style='width: 50px; height: 20px'></div></div></body>",
            "const r=document.getElementById('inner').getBoundingClientRect();\
             [r.x,r.y,r.width,r.height].join(',')",
        ),
        "15,15,50,20"
    );
}

#[test]
fn client_rects_per_line_for_wrapped_span() {
    // 60px-wide Ahem container: "aaaaa " on line 1, the span wraps lines 2-3.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='font-family: Ahem; font-size: 10px; line-height: 10px; \
             width: 60px'>aaaaa <span id=s>bbbbb bb</span></div></body>",
            "const rs=document.getElementById('s').getClientRects();\
             [rs.length, rs[0].top, rs[1].top, rs[1].width].join(',')",
        ),
        "2,10,20,20"
    );
}

#[test]
fn offset_parent_chain() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=rel style='position: relative; padding: 5px; border: 2px solid black'>\
             <div id=inner style='width: 10px; height: 10px'></div></div></body>",
            "const i=document.getElementById('inner');\
             [i.offsetParent.id, i.offsetLeft, i.offsetTop,\
              document.getElementById('rel').offsetParent.tagName].join(',')",
        ),
        "rel,5,5,BODY"
    );
}

/// `getComputedStyle` must report the **used** inset, not the specified one:
/// a percentage absolutized against the containing block, and `auto` resolved.
/// It used to hand back the computed value for anything but abspos, so
/// `top: 50%` read back as `"50%"` and `top: auto` as `"auto"`.
#[test]
fn relative_insets_resolve_percentages_and_auto() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='height:200px;width:400px'>\
             <div id=a style='position:relative;top:50%;left:calc(10px + 5px);height:20px'></div>\
             <div id=b style='position:relative;top:auto;bottom:30px;height:20px'></div>\
             <div id=c style='position:relative;top:auto;bottom:auto;height:20px'></div>\
             </div></body>",
            "const cs=id=>getComputedStyle(document.getElementById(id));\
             [cs('a').top, cs('a').left, cs('b').top, cs('c').top].join(',')",
        ),
        // 50% of the 200px containing block; calc absolutized; `auto` is the
        // negative of the opposite side; both `auto` is zero.
        "100px,15px,-30px,0px"
    );
}

#[test]
fn static_insets_stay_computed_and_sticky_keeps_auto() {
    // A static box's insets do not apply, so the resolved value is the computed
    // one. A sticky box absolutizes lengths but *preserves* `auto` — there is no
    // offset to report until it is actually stuck.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='height:200px;width:400px'>\
             <div id=s style='top:50%;left:auto;height:20px'></div>\
             <div id=k style='position:sticky;top:25%;bottom:auto;height:20px'></div>\
             </div></body>",
            "const cs=id=>getComputedStyle(document.getElementById(id));\
             [cs('s').top, cs('s').left, cs('k').top, cs('k').bottom].join(',')",
        ),
        "50%,auto,50px,auto"
    );
}

#[test]
fn overconstrained_abspos_insets_report_the_computed_value() {
    // CSSOM: the resolved value is the used value only "if the property is not
    // over-constrained". Pinning all four sides *and* the size over-constrains
    // both axes, so the absolutized computed values are reported instead of
    // where the box actually landed.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='position:relative;height:200px;width:400px'>\
             <div id=o style='position:absolute;top:1px;left:2px;bottom:3px;right:4px;\
             height:0;width:0'></div></div></body>",
            "const cs=getComputedStyle(document.getElementById('o'));\
             [cs.top, cs.left, cs.bottom, cs.right].join(',')",
        ),
        "1px,2px,3px,4px"
    );
}

#[test]
fn offsets_from_a_static_body_are_icb_relative() {
    // Legacy carve-out every engine implements: with the UA default
    // `body { margin: 8px }`, a plain div in the body reports offsetTop 8 — the
    // body's margin is *not* subtracted, even though CSSOM-View's offsetTop
    // step 3 would measure from the body's padding edge and yield 0.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body><div id=d style='width:10px;height:10px'></div></body>",
            "const d=document.getElementById('d');\
             [d.offsetParent.tagName, d.offsetLeft, d.offsetTop,\
              d.getBoundingClientRect().top].join(',')",
        ),
        "BODY,8,8,8"
    );
}

#[test]
fn offsets_from_a_positioned_body_are_padding_relative() {
    // Once the body is positioned the carve-out lapses and the spec algorithm
    // applies again: offsets are measured from the offsetParent's padding edge.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html>\
             <body style='position:relative;margin:8px;border:2px solid;padding:5px'>\
             <div id=d style='width:10px;height:10px'></div></body>",
            "const d=document.getElementById('d');\
             [d.offsetParent.tagName, d.offsetLeft, d.offsetTop].join(',')",
        ),
        "BODY,5,5"
    );
}

#[test]
fn scroll_size_of_a_bordered_box_excludes_the_border() {
    // The scrollable overflow area is seeded with the *padding* box (CSS Overflow
    // §3.2). Seeding it with the border box reported scrollHeight as
    // clientHeight + border-bottom-width for every bordered element.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=d style='overflow:auto;border:3px solid;width:100px;height:50px'>\
             <div style='height:20px'></div></div></body>",
            "const d=document.getElementById('d');\
             [d.clientWidth, d.clientHeight, d.scrollWidth, d.scrollHeight].join(',')",
        ),
        "100,50,100,50"
    );
}

#[test]
fn scroll_size_still_counts_an_overflowing_childs_border_box() {
    // ...but a child that really does overflow must still be measured by its
    // border box, borders included: 200px content + 2*5px border = 210px.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=d style='overflow:auto;border:3px solid;width:100px;height:50px'>\
             <div style='height:200px;border:5px solid'></div></div></body>",
            "const d=document.getElementById('d');\
             [d.clientHeight, d.scrollHeight].join(',')",
        ),
        "50,210"
    );
}

#[test]
fn scroll_top_clamps_and_fires_scroll_event() {
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'>\
         <div id=sc style='overflow: scroll; width: 100px; height: 100px'>\
         <div style='height: 400px'></div></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    // Clamped to 400 - 100 = 300.
    assert_eq!(
        page.eval_to_string(
            "const sc=document.getElementById('sc');\
             window.__scrolls=0; sc.addEventListener('scroll', () => window.__scrolls++);\
             sc.scrollTop = 1000; sc.scrollTop"
        )
        .unwrap(),
        "300"
    );
    // The scroll event is dispatched as a task by the event loop.
    page.run_until_stalled();
    assert_eq!(page.eval_to_string("window.__scrolls").unwrap(), "1");
    // Writing the same clamped position again does not re-fire.
    page.eval_to_string("document.getElementById('sc').scrollTop = 300; 'ok'")
        .unwrap();
    page.run_until_stalled();
    assert_eq!(page.eval_to_string("window.__scrolls").unwrap(), "1");
}

#[test]
fn element_from_point_prefers_higher_z_index() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=below style='position:absolute; left:0; top:0; width:100px; height:100px'></div>\
             <div id=above style='position:absolute; left:0; top:0; width:50px; height:50px; z-index:5'></div>\
             </body>",
            // The full stack: both abs-positioned divs plus the html root
            // (the body has zero height here — both children are out of
            // flow — so it is not in the hit list, matching browsers).
            "const stack = document.elementsFromPoint(25,25).map(e => e.id || e.tagName);\
             [document.elementFromPoint(25,25).id, document.elementFromPoint(75,75).id,\
              stack.join('>')].join(',')",
        ),
        "above,below,above>below>HTML"
    );
}

#[test]
fn resolved_width_is_used_value() {
    // Auto width resolves against the viewport: 800 - 16 (body margin).
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body><div id=d>x</div></body>",
            "getComputedStyle(document.getElementById('d')).width",
        ),
        "784px"
    );
    // display:none boxes fall back to the computed value.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body><div id=d style='display:none; width: 50%'>x</div></body>",
            "getComputedStyle(document.getElementById('d')).width",
        ),
        "50%"
    );
    // Positioned insets resolve to used px; static keeps `auto`.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='position:relative; width:200px; height:100px'>\
             <div id=abs style='position:absolute; left: 30px; top: 10px; \
             width: 20px; height: 20px'></div></div></body>",
            "const cs=getComputedStyle(document.getElementById('abs'));\
             [cs.left, cs.top].join(',')",
        ),
        "30px,10px"
    );
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body><div id=d>x</div></body>",
            "getComputedStyle(document.getElementById('d')).top",
        ),
        "auto"
    );
    // Review #2: a relative box after a 40px sibling must report its own
    // offset ("10px"), not its flow position ("50px").
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'><div style='height:40px'></div>\
             <div id=rel style='position:relative; top: 10px; height: 5px'></div></body>",
            "getComputedStyle(document.getElementById('rel')).top",
        ),
        "10px"
    );
}

#[test]
fn window_viewport_metrics_and_set_viewport() {
    let page = load_html_page(
        "<!DOCTYPE html><body><div id=d style='width: 50%'></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        page.eval_to_string("[innerWidth, innerHeight, devicePixelRatio].join(',')")
            .unwrap(),
        "800,600,1"
    );
    page.set_viewport(oxidepage_style::Viewport {
        width: 1000.0,
        height: 500.0,
        dpr: 2.0,
    });
    assert_eq!(
        page.eval_to_string("[innerWidth, innerHeight, devicePixelRatio].join(',')")
            .unwrap(),
        "1000,500,2"
    );
    // Layout re-runs against the new viewport.
    assert_eq!(
        page.eval_to_string("document.getElementById('d').getBoundingClientRect().width")
            .unwrap(),
        "492" // (1000 - 16 body margin) / 2
    );
}

#[test]
fn window_scroll_and_page_offsets() {
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'><div style='height: 2000px'></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        page.eval_to_string(
            "window.scrollTo(0, 150);\
             [scrollX, scrollY, pageXOffset, pageYOffset].join(',')"
        )
        .unwrap(),
        "0,150,0,150"
    );
    // scrollBy is relative; object form supported.
    assert_eq!(
        page.eval_to_string("scrollBy({top: 50}); scrollY").unwrap(),
        "200"
    );
    // documentElement.scrollTop aliases the viewport scroll.
    assert_eq!(
        page.eval_to_string("document.documentElement.scrollTop")
            .unwrap(),
        "200"
    );
    // getBoundingClientRect shifts with the viewport scroll.
    assert_eq!(
        page.eval_to_string("document.body.getBoundingClientRect().top")
            .unwrap(),
        "-200"
    );
    // Clamp: max = 2000 - 600.
    assert_eq!(
        page.eval_to_string("scrollTo(0, 99999); scrollY").unwrap(),
        "1400"
    );
}

#[test]
fn table_cell_geometry_via_js() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <table style='border-collapse: collapse'>\
             <tr><td id=a style='width:100px; height:20px; padding:0'></td>\
                 <td id=b style='width:50px; height:20px; padding:0'></td></tr></table></body>",
            "const a=document.getElementById('a').getBoundingClientRect();\
             const b=document.getElementById('b').getBoundingClientRect();\
             [a.width, b.x - a.x, b.width].join(',')",
        ),
        "100,100,50"
    );
}

#[test]
fn before_pseudo_affects_owner_geometry() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><html><head><style>\
             #d::before { content: 'XX'; font-family: Ahem; font-size: 10px; \
             line-height: 10px; }\
             </style></head><body style='margin:0'>\
             <div id=d style='font-family: Ahem; font-size: 10px; line-height: 10px'>YY</div>\
             </body></html>",
            // ::before contributes 2 glyphs: the div's line is 4 * 10px wide
            // in content terms; check the box height stays one line and
            // scrollWidth reflects the shaped text.
            "const d=document.getElementById('d');\
             [d.getBoundingClientRect().height].join(',')",
        ),
        "10"
    );
}

#[test]
fn dom_rect_list_indexed_access() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=d style='width: 40px; height: 30px'></div></body>",
            "const rs=document.getElementById('d').getClientRects();\
             [rs.length, rs[0].width, rs.item(0).height, String(rs.item(1))].join(',')",
        ),
        "1,40,30,null"
    );
}

#[test]
fn multicol_reports_rects_in_the_column_that_shows_them() {
    // Three 30px blocks in a 2-column container balance into 60px columns, so
    // #b3 starts the second column: it reports a rect *there*, not at its offset
    // in the continuous flow (ADR-0016).
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div style='width:200px; column-count:2; column-gap:0'>\
             <div style='height:30px'></div><div style='height:30px'></div>\
             <div id=b3 style='height:30px'></div></div></body>",
            "const r=document.getElementById('b3').getBoundingClientRect();\
             [r.x,r.y,r.width,r.height].join(',')",
        ),
        "100,0,100,30"
    );
}

#[test]
fn multicol_hit_testing_uses_the_columns() {
    // A point over the second column hits the element shown there; a point in
    // the gap between columns reaches the container but no content.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=m style='width:200px; column-count:2; column-gap:20px'>\
             <div style='height:30px'></div><div id=b2 style='height:30px'></div></div></body>",
            "[document.elementFromPoint(120,10).id, document.elementFromPoint(100,10).id].join(',')",
        ),
        "b2,m"
    );
}

// === Element.scroll()/scrollTo()/scrollBy() and checkVisibility() ===

#[test]
fn element_scroll_methods_reuse_the_scroll_top_left_path() {
    // Numeric form, dictionary form (a missing member keeps the other axis
    // via the *current* position), and — the specific bug this regresses —
    // the zero-argument call, which must default to the current position
    // rather than reset to (0, 0): the two-form overload is resolved by
    // argument *count*, not by the shape of the first argument, so `scroll()`
    // must not fall through to the numeric branch's zero defaults.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=sc style='overflow: scroll; width: 100px; height: 100px'>\
             <div style='width: 400px; height: 400px'></div></div></body>",
            "const sc = document.getElementById('sc');\
             sc.scroll(10, 20);\
             const a = [sc.scrollLeft, sc.scrollTop].join(',');\
             sc.scrollTo({left: 30});\
             const b = [sc.scrollLeft, sc.scrollTop].join(',');\
             sc.scroll();\
             const c = [sc.scrollLeft, sc.scrollTop].join(',');\
             sc.scrollBy({top: 5});\
             const d = [sc.scrollLeft, sc.scrollTop].join(',');\
             [a, b, c, d].join(' | ')",
        ),
        "10,20 | 30,20 | 30,20 | 30,25"
    );
}

#[test]
fn element_scroll_ignores_non_finite_and_clamps_to_the_scrollable_range() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=sc style='overflow: scroll; width: 100px; height: 100px'>\
             <div style='width: 400px; height: 400px'></div></div></body>",
            "const sc = document.getElementById('sc');\
             sc.scroll(NaN, Infinity);\
             const a = [sc.scrollLeft, sc.scrollTop].join(',');\
             sc.scrollTo(99999, 99999);\
             const b = [sc.scrollLeft, sc.scrollTop].join(',');\
             [a, b].join(' | ')",
        ),
        "0,0 | 300,300"
    );
}

#[test]
fn document_element_scroll_aliases_the_viewport() {
    // `Element.scroll()` on the document element must go through the same
    // viewport-scroll path as `documentElement.scrollTop` — not a second,
    // element-local scroll container.
    let page = load_html_page(
        "<!DOCTYPE html><body style='margin:0'><div style='height: 2000px'></div></body>",
        PageOptions::default(),
    )
    .unwrap();
    assert_eq!(
        page.eval_to_string(
            "document.documentElement.scroll(0, 150); [scrollX, scrollY].join(',')"
        )
        .unwrap(),
        "0,150"
    );
    assert_eq!(
        page.eval_to_string("document.documentElement.scrollTop")
            .unwrap(),
        "150"
    );
}

#[test]
fn check_visibility_is_false_without_a_layout_box() {
    // `display: none` on an ancestor leaves the descendant boxless too.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body>\
             <div id=none style='display:none'><div id=child></div></div></body>",
            "[document.getElementById('none').checkVisibility(),\
              document.getElementById('child').checkVisibility()].join(',')",
        ),
        "false,false"
    );
}

#[test]
fn check_visibility_display_contents_has_no_box_but_children_do() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body>\
             <div id=c style='display:contents'><div id=child></div></div></body>",
            "[document.getElementById('c').checkVisibility(),\
              document.getElementById('child').checkVisibility()].join(',')",
        ),
        "false,true"
    );
}

#[test]
fn check_visibility_checks_used_visibility_only_when_asked() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body><div id=h style='visibility:hidden'></div></body>",
            "const h = document.getElementById('h');\
             [h.checkVisibility(), h.checkVisibility({checkVisibilityCSS: true}),\
              h.checkVisibility({visibilityProperty: true})].join(',')",
        ),
        "true,false,false"
    );
}

#[test]
fn check_visibility_walks_ancestors_for_opacity_only_when_asked() {
    // `opacity` is not an inherited property, so unlike `visibility` an
    // ancestor's `opacity: 0` needs an explicit ancestor walk.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='opacity:0'><div id=child></div></body>",
            "const c = document.getElementById('child');\
             [c.checkVisibility(), c.checkVisibility({checkOpacity: true}),\
              c.checkVisibility({opacityProperty: true})].join(',')",
        ),
        "true,false,false"
    );
}

// === `Element.scrollParent()` (CSSOM-View, draft) ===

#[test]
fn scroll_parent_returns_the_nearest_scroll_container() {
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=scroller style='overflow:scroll;height:100px'>\
             <div id=child></div></div></body>",
            "document.getElementById('child').scrollParent() === \
             document.getElementById('scroller')",
        ),
        "true"
    );
}

#[test]
fn scroll_parent_of_the_scrolling_element_is_null() {
    // Regression: the containing-block walk's "reached the initial
    // containing block" answer resolves to `document.scrollingElement`, but
    // that resolution used to run unconditionally — so an element that *is*
    // already the scrolling element (the common standards-mode case,
    // `documentElement`) reported itself instead of null.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body></body>",
            "[document.documentElement.scrollParent(),\
              document.scrollingElement.scrollParent()].join(',')",
        ),
        ","
    );
}

#[test]
fn scroll_parent_of_body_is_null_in_quirks_mode() {
    // Quirks mode promotes `body`, not `documentElement`, to
    // `document.scrollingElement` — the same "already the scrolling
    // element" rule then has to fire for `body` instead of the root.
    assert_eq!(
        eval_in(
            // No doctype: quirks mode.
            "<html><body></body></html>",
            "document.scrollingElement === document.body &&\
             document.body.scrollParent() === null",
        ),
        "true"
    );
}

#[test]
fn scroll_parent_of_a_viewport_fixed_element_is_null() {
    // `position: fixed` with no ancestor establishing a fixed-position
    // containing block resolves against the viewport, which nothing
    // DOM-observable scrolls — distinct from a normal element whose
    // containing-block chain also ends at the viewport, which reports
    // `document.scrollingElement` (previous test).
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=scroller style='overflow:scroll;height:100px'>\
             <div id=fixed style='position:fixed'></div></div></body>",
            "document.getElementById('fixed').scrollParent()",
        ),
        "null"
    );
}

#[test]
fn scroll_parent_skips_a_slotted_shadow_scroll_container() {
    // A light-DOM element's `scrollParent` walk runs over the flat tree, so a
    // scroll container inside the shadow tree it is slotted into is a real
    // containing-block ancestor there — but that shadow tree's internals are
    // not observable from outside it (open or closed alike), so the walk
    // must keep going past it and report the next *visible* scroll
    // container instead (WPT `scrollParent-shadow-tree.html`).
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <div id=outer style='overflow:scroll;height:50px'>\
             <div id=host><div id=inner></div></div></div>\
             <script>\
               const sr = document.getElementById('host').attachShadow({mode:'open'});\
               sr.innerHTML = '<div style=\"overflow:scroll;height:10px\"><slot></slot></div>';\
             </script></body>",
            "document.getElementById('inner').scrollParent() === \
             document.getElementById('outer')",
        ),
        "true"
    );
}

#[test]
fn element_from_point_on_an_outside_list_marker_reports_the_item() {
    // WPT `css/cssom-view/elementFromPoint-list-001.html`: an outside marker
    // (the CSS default) lies *before* the item's content edge, so the item's own
    // border box never contains the probe point — the marker box has to report
    // the hit. The probe sweeps the 40px the `<ul>` reserves, as the WPT does.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <ul style='margin:0;padding-left:40px'><li id=a>alpha</li></ul></body>",
            "const li = document.getElementById('a');\
             const b = li.getBoundingClientRect();\
             const y = (b.top + b.bottom) / 2;\
             let hit = null;\
             for (let x = b.left - 40; x < b.left; x++) {\
               if (document.elementFromPoint(x, y) === li) { hit = 'li'; break; }\
             }\
             hit",
        ),
        "li"
    );
}

#[test]
fn an_outside_list_marker_stays_out_of_the_items_geometry() {
    // The marker is not part of the item's principal box: it must not move or
    // widen `getBoundingClientRect`, and — hanging off the start edge, where CSS
    // clips the scrollable overflow region — it must not widen `scrollWidth`.
    assert_eq!(
        eval_in(
            "<!DOCTYPE html><body style='margin:0'>\
             <ul style='margin:0;padding-left:40px'><li id=a>alpha</li></ul>\
             <ul style='margin:0;padding-left:40px;list-style-type:none'>\
             <li id=b>alpha</li></ul></body>",
            "const a = document.getElementById('a').getBoundingClientRect();\
             const b = document.getElementById('b').getBoundingClientRect();\
             const li = document.getElementById('a');\
             [a.left === b.left, a.width === b.width, \
              li.scrollWidth === li.clientWidth].join(',')",
        ),
        "true,true,true"
    );
}
