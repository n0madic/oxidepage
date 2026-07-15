//! Phase 4 computed-value + `@import` tests.

use std::collections::HashMap;

use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};
use oxidepage_style::{
    BlockingImportLoader, CssFetcher, StyleEngine, Viewport, computed_style_for, serialize_property,
};
use style::stylesheets::Origin;

fn parse(html: &str) -> DomTree {
    parse_document(html, ParseOptions::default()).tree
}

fn nth_element(tree: &DomTree, local: &str, n: usize) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .filter(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .nth(n)
        .unwrap_or_else(|| panic!("no <{local}>#{n} in document"))
}

fn computed(engine: &mut StyleEngine, tree: &mut DomTree, node: NodeId, prop: &str) -> String {
    let cv = computed_style_for(engine, tree, node, None).expect("has computed style");
    serialize_property(&cv, prop)
}

#[test]
fn inline_style_and_color_serialization() {
    let mut tree = parse("<div style='color: red'>hi</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = nth_element(&tree, "div", 0);
    assert_eq!(
        computed(&mut engine, &mut tree, div, "color"),
        "rgb(255, 0, 0)"
    );
}

#[test]
fn font_size_em_inherits_from_parent() {
    let mut tree =
        parse("<div style='font-size: 20px'><span style='font-size: 2em'>x</span></div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let span = nth_element(&tree, "span", 0);
    // 2em against the parent's 20px computes to 40px.
    assert_eq!(computed(&mut engine, &mut tree, span, "font-size"), "40px");
}

#[test]
fn custom_property_var_resolves() {
    let mut tree = parse("<div style='--brand: rgb(0, 128, 0); color: var(--brand)'>x</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = nth_element(&tree, "div", 0);
    assert_eq!(
        computed(&mut engine, &mut tree, div, "color"),
        "rgb(0, 128, 0)"
    );
}

#[test]
fn shorthand_computed_value_is_empty() {
    let mut tree = parse("<div style='margin: 5px'>x</div>");
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = nth_element(&tree, "div", 0);
    // Shorthands serialize to "" in the computed declaration (v1); the longhand
    // is available.
    assert_eq!(computed(&mut engine, &mut tree, div, "margin"), "");
    assert_eq!(computed(&mut engine, &mut tree, div, "margin-top"), "5px");
}

/// A fetcher serving a fixed map of URL → CSS bytes.
struct MapFetcher {
    files: HashMap<String, String>,
}

impl CssFetcher for MapFetcher {
    fn fetch_css(&self, url: &url::Url) -> Result<(Vec<u8>, Option<String>, url::Url), String> {
        match self.files.get(url.as_str()) {
            Some(css) => Ok((css.clone().into_bytes(), None, url.clone())),
            None => Err(format!("404 {url}")),
        }
    }
}

#[test]
fn import_chain_is_loaded_synchronously() {
    let mut files = HashMap::new();
    files.insert(
        "https://example.com/b.css".to_owned(),
        "div { color: rgb(0, 0, 255) }".to_owned(),
    );
    let fetcher = MapFetcher { files };

    let mut tree = parse("<div>x</div>");
    tree.set_document_url("https://example.com/a.html".to_owned());
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = nth_element(&tree, "div", 0);

    let loader = BlockingImportLoader::new(&fetcher, engine.lock().clone(), Origin::Author, None);
    let css = b"@import url('https://example.com/b.css'); div { display: block }";
    let sheet = engine.make_stylesheet_from_bytes(
        css,
        tree.url_extra_data().clone(),
        None,
        None,
        None,
        Some(&loader),
    );
    engine.add_sheet_for_node(&tree, div, sheet);

    assert_eq!(
        computed(&mut engine, &mut tree, div, "color"),
        "rgb(0, 0, 255)"
    );
}

#[test]
fn import_cycle_is_refused_without_hanging() {
    let mut files = HashMap::new();
    // a.css imports b.css, b.css imports a.css: the cycle must be broken.
    files.insert(
        "https://example.com/a.css".to_owned(),
        "@import url('https://example.com/b.css'); div { color: red }".to_owned(),
    );
    files.insert(
        "https://example.com/b.css".to_owned(),
        "@import url('https://example.com/a.css'); div { color: blue }".to_owned(),
    );
    let fetcher = MapFetcher { files };

    let mut tree = parse("<div>x</div>");
    tree.set_document_url("https://example.com/index.html".to_owned());
    let mut engine = StyleEngine::new(&tree, Viewport::default());
    let div = nth_element(&tree, "div", 0);

    let loader = BlockingImportLoader::new(&fetcher, engine.lock().clone(), Origin::Author, None);
    let css = b"@import url('https://example.com/a.css');";
    let sheet = engine.make_stylesheet_from_bytes(
        css,
        tree.url_extra_data().clone(),
        None,
        None,
        None,
        Some(&loader),
    );
    engine.add_sheet_for_node(&tree, div, sheet);

    // Terminates and applies a.css's rule (b.css re-import of a.css is refused).
    assert_eq!(
        computed(&mut engine, &mut tree, div, "color"),
        "rgb(255, 0, 0)"
    );
}
