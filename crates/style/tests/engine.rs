//! Phase 4 style-engine tests: UA + author cascade, media queries, and
//! snapshot-driven incremental restyle.

use oxidepage_dom::select::NodeRef;
use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_style::{StyleEngine, Viewport};
use style::dom::TElement;
use style::properties::{LonghandId, PropertyDeclarationId};

fn parse(html: &str) -> DomTree {
    parse_document(html, ParseOptions::default()).tree
}

fn find_element(tree: &DomTree, local: &str) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .unwrap_or_else(|| panic!("no <{local}> in document"))
}

fn longhand(tree: &DomTree, id: NodeId, longhand: LonghandId) -> String {
    let scope = oxidepage_dom::select::enter_active_tree(tree);
    let node = NodeRef::new(&scope, id);
    let data = node.borrow_data().expect("element has cascade data");
    let primary = data
        .styles
        .get_primary()
        .expect("element has primary style");
    primary.computed_value_to_string(PropertyDeclarationId::Longhand(longhand))
}

fn display_of(tree: &DomTree, id: NodeId) -> String {
    longhand(tree, id, LonghandId::Display)
}

fn color_of(tree: &DomTree, id: NodeId) -> String {
    longhand(tree, id, LonghandId::Color)
}

#[test]
fn ua_stylesheet_provides_default_display() {
    let mut tree = parse("<div></div><span></span>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    engine.resolve_styles(&mut tree);

    let div = find_element(&tree, "div");
    let span = find_element(&tree, "span");
    let head = find_element(&tree, "head");
    assert_eq!(display_of(&tree, div), "block");
    assert_eq!(display_of(&tree, span), "inline");
    assert_eq!(display_of(&tree, head), "none");
}

#[test]
fn author_sheet_overrides_ua_and_sets_color() {
    let mut tree = parse("<div>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = find_element(&tree, "div");

    let sheet =
        engine.make_stylesheet("div { color: red; display: inline }", tree.url_extra_data());
    engine.add_sheet_for_node(&tree, div, sheet);
    engine.resolve_styles(&mut tree);

    assert_eq!(display_of(&tree, div), "inline", "author display beats UA");
    assert_eq!(color_of(&tree, div), "rgb(255, 0, 0)");
}

#[test]
fn media_query_activates_after_viewport_change() {
    let mut tree = parse("<div>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default()); // 800x600
    let div = find_element(&tree, "div");

    let sheet = engine.make_stylesheet(
        "@media (min-width: 1000px) { div { color: rgb(0, 128, 0) } }",
        tree.url_extra_data(),
    );
    engine.add_sheet_for_node(&tree, div, sheet);

    engine.resolve_styles(&mut tree);
    assert_eq!(
        color_of(&tree, div),
        "rgb(0, 0, 0)",
        "media query inert at 800px width"
    );

    engine.set_viewport(Viewport {
        width: 1024.0,
        height: 768.0,
        dpr: 1.0,
    });
    engine.resolve_styles(&mut tree);
    assert_eq!(
        color_of(&tree, div),
        "rgb(0, 128, 0)",
        "media query active at 1024px width"
    );
}

#[test]
fn class_change_triggers_restyle_via_snapshot() {
    let mut tree = parse("<div>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = find_element(&tree, "div");

    let sheet = engine.make_stylesheet(".hidden { display: none }", tree.url_extra_data());
    engine.add_sheet_for_node(&tree, div, sheet);
    engine.resolve_styles(&mut tree);
    assert_eq!(display_of(&tree, div), "block", "no class yet");

    // Adding the class must invalidate and restyle the element on the next pass.
    let class_attr = oxidepage_dom::node::attr_name(oxidepage_dom::LocalName::from("class"));
    tree.set_attribute(div, class_attr, "hidden".into());
    engine.resolve_styles(&mut tree);
    assert_eq!(
        display_of(&tree, div),
        "none",
        "class change restyled the div"
    );
}

#[test]
fn sheets_are_stored_in_document_order() {
    let tree = parse("<div id=a></div><div id=b></div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let a = find_element(&tree, "div");
    let b = tree
        .inclusive_descendants(tree.document())
        .filter(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == "div")
        })
        .nth(1)
        .unwrap();

    let sheet_b = engine.make_stylesheet("#b { color: blue }", tree.url_extra_data());
    let sheet_a = engine.make_stylesheet("#a { color: red }", tree.url_extra_data());
    // Insert out of order; the engine must reorder by document position.
    engine.add_sheet_for_node(&tree, b, sheet_b);
    engine.add_sheet_for_node(&tree, a, sheet_a);

    let ordered: Vec<NodeId> = engine.author_sheets().map(|(n, _)| n).collect();
    assert_eq!(ordered, vec![a, b], "sheets ordered by document position");
}

#[test]
fn anonymous_box_style_inherits_from_parent() {
    let mut tree = parse("<div style='color: rgb(1, 2, 3); font-size: 20px'>x</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    engine.resolve_styles(&mut tree);

    let div = find_element(&tree, "div");
    let parent = tree.primary_style(div).expect("div has a primary style");

    let _scope = oxidepage_dom::select::enter_active_tree(&tree);
    let anon = engine.anonymous_box_style(&parent);
    // Anonymous boxes inherit inherited properties from their parent. (Their
    // computed `display` stays `inline` — no UA rule targets the Servo
    // anonymous-box pseudo — but stylo_taffy maps inside:Flow to
    // taffy::Display::Block, which is what layout consumes; blitz-dom
    // behaves identically.)
    assert_eq!(
        anon.computed_value_to_string(PropertyDeclarationId::Longhand(LonghandId::Color)),
        "rgb(1, 2, 3)"
    );
    assert_eq!(
        anon.computed_value_to_string(PropertyDeclarationId::Longhand(LonghandId::FontSize)),
        "20px"
    );
}

#[test]
fn font_faces_are_discovered() {
    use oxidepage_style::{FontFaceStyle, FontFormatHint};

    let tree = parse("<div>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = find_element(&tree, "div");
    let css = r#"
        @font-face {
            font-family: "My Font";
            src: url(https://example.com/font.woff2) format("woff2"),
                 local("Fallback");
            font-weight: 400 700;
            font-style: italic;
        }
        @media screen {
            @font-face {
                font-family: "Media Font";
                src: url(https://example.com/m.ttf);
            }
        }
    "#;
    let sheet = engine.make_stylesheet(css, tree.url_extra_data());
    engine.add_sheet_for_node(&tree, div, sheet);

    let faces = engine.font_faces();
    assert_eq!(
        faces.len(),
        2,
        "both @font-face rules discovered: {faces:?}"
    );

    let my = faces
        .iter()
        .find(|f| f.family == "My Font")
        .expect("My Font present");
    assert_eq!(my.sources.len(), 2, "url + local sources");
    assert_eq!(
        my.sources[0].url.as_deref(),
        Some("https://example.com/font.woff2")
    );
    assert_eq!(my.sources[0].format, Some(FontFormatHint::Woff2));
    assert_eq!(my.sources[1].local.as_deref(), Some("Fallback"));
    assert_eq!(my.weight, (400.0, 700.0));
    assert_eq!(my.style, FontFaceStyle::Italic);

    // The @font-face nested inside an effective @media block is discovered too.
    assert!(
        faces.iter().any(|f| f.family == "Media Font"),
        "media-nested @font-face discovered"
    );
}

/// Regression: `effective_rules` walks the sheet *contents*, which know nothing
/// about the wrapper's `disabled` flag or its sheet-level media list. A disabled
/// or non-matching sheet used to contribute its `@font-face` rules anyway, so the
/// page fetched fonts it would never render with.
#[test]
fn font_faces_skip_disabled_and_non_matching_sheets() {
    let tree = parse("<div>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = find_element(&tree, "div");
    let face = |family: &str| {
        format!(
            r#"@font-face {{ font-family: "{family}"; src: url(https://example.com/f.woff2); }}"#
        )
    };

    // A `<style media="print">` sheet never matches the screen device.
    let print_sheet = engine.make_stylesheet_with_loader(
        &face("Print Font"),
        tree.url_extra_data(),
        Some("print"),
        None,
    );
    engine.add_sheet_for_node(&tree, div, print_sheet);

    // A sheet that matches, but is then disabled.
    let disabled_sheet = engine.make_stylesheet(&face("Disabled Font"), tree.url_extra_data());
    engine.add_sheet_for_node(&tree, div, disabled_sheet.clone());
    engine.set_sheet_disabled(&disabled_sheet, true);

    let families: Vec<String> = engine.font_faces().into_iter().map(|f| f.family).collect();
    assert!(
        families.is_empty(),
        "neither a print-only nor a disabled sheet contributes @font-face: {families:?}"
    );

    // Re-enabling brings its face back, proving the filter is the reason.
    engine.set_sheet_disabled(&disabled_sheet, false);
    let families: Vec<String> = engine.font_faces().into_iter().map(|f| f.family).collect();
    assert_eq!(families, ["Disabled Font"]);
}

/// Regression: a rejected `insertRule`/`deleteRule` must not bump the style
/// version — every computed-value cache in the document keys off it.
#[test]
fn failed_rule_mutations_do_not_bump_the_version() {
    let tree = parse("<div>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = find_element(&tree, "div");
    let sheet = engine.make_stylesheet("div { color: red }", tree.url_extra_data());
    engine.add_sheet_for_node(&tree, div, sheet.clone());

    let before = engine.version();
    for bad in ["", "}", "@charset \"utf-8\";"] {
        assert!(
            engine.insert_rule(&sheet, bad, 0).is_err(),
            "{bad:?} is not an insertable rule"
        );
    }
    assert!(
        engine.insert_rule(&sheet, "p { color: blue }", 99).is_err(),
        "an out-of-range index is rejected"
    );
    assert!(
        engine.delete_rule(&sheet, 99).is_err(),
        "deleting out of range is rejected"
    );
    assert_eq!(
        engine.version(),
        before,
        "a rejected mutation must leave the version untouched"
    );

    // A successful mutation still bumps it.
    assert!(engine.insert_rule(&sheet, "p { color: blue }", 0).is_ok());
    assert_ne!(engine.version(), before);
}

#[test]
fn oblique_range_is_not_flattened_to_normal() {
    use oxidepage_style::FontFaceStyle;

    let tree = parse("<div>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = find_element(&tree, "div");
    // `oblique 0deg 20deg` is a genuine oblique range whose lower endpoint is
    // 0deg. Regression: it used to match the `Oblique(0.0, _)` arm and be
    // flattened to upright `Normal`, dropping the non-zero endpoint.
    let css = r#"
        @font-face {
            font-family: "Slanted";
            src: url(https://example.com/s.woff2);
            font-style: oblique 0deg 20deg;
        }
    "#;
    let sheet = engine.make_stylesheet(css, tree.url_extra_data());
    engine.add_sheet_for_node(&tree, div, sheet);

    let face = engine
        .font_faces()
        .into_iter()
        .find(|f| f.family == "Slanted")
        .expect("Slanted face present");
    assert_eq!(
        face.style,
        FontFaceStyle::Oblique(20.0),
        "a 0deg-min oblique range keeps its non-zero endpoint, not Normal"
    );
}
