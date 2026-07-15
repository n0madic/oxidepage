//! `DomTree::base_url`: `<base href>` resolution and cache invalidation.

use oxidepage_dom::node::attr_name;
use oxidepage_dom::{DomTree, LocalName, ParseOptions, QualName, parse_document};

const DOC_URL: &str = "http://example.test/a/b/page.html";

fn parse_at(html: &str, url: &str) -> DomTree {
    let mut tree = parse_document(html, ParseOptions::default()).tree;
    tree.set_document_url(url.to_owned());
    tree
}

fn find_element(tree: &DomTree, local: &str) -> oxidepage_dom::NodeId {
    tree.inclusive_descendants(tree.document())
        .find(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .unwrap_or_else(|| panic!("no <{local}> in document"))
}

fn href() -> QualName {
    attr_name(LocalName::from("href"))
}

#[test]
fn without_a_base_element_the_base_url_is_the_document_url() {
    let tree = parse_at("<html><body>hi</body></html>", DOC_URL);
    assert_eq!(tree.base_url(), DOC_URL);
}

#[test]
fn relative_base_href_resolves_against_the_document_url() {
    // `/a/b/page.html` + `../c/` → `/a/c/`.
    let tree = parse_at(r#"<head><base href="../c/"></head>"#, DOC_URL);
    assert_eq!(tree.base_url(), "http://example.test/a/c/");
}

#[test]
fn absolute_base_href_replaces_the_document_url() {
    let tree = parse_at(
        r#"<head><base href="https://cdn.example/x/"></head>"#,
        DOC_URL,
    );
    assert_eq!(tree.base_url(), "https://cdn.example/x/");
}

#[test]
fn a_base_without_href_does_not_set_the_base_url() {
    let tree = parse_at(r#"<head><base target="_blank"></head>"#, DOC_URL);
    assert_eq!(tree.base_url(), DOC_URL);
}

#[test]
fn only_the_first_base_href_in_tree_order_counts() {
    let tree = parse_at(
        r#"<head><base href="/first/"><base href="/second/"></head>"#,
        DOC_URL,
    );
    assert_eq!(tree.base_url(), "http://example.test/first/");
}

#[test]
fn an_unparseable_base_href_falls_back_to_the_document_url() {
    // A document URL that cannot itself be parsed makes the join impossible.
    let tree = parse_at(r#"<head><base href="../c/"></head>"#, "not a url");
    assert_eq!(tree.base_url(), "not a url");
}

#[test]
fn removing_the_base_element_reverts_the_base_url() {
    let mut tree = parse_at(r#"<head><base href="/x/"></head>"#, DOC_URL);
    assert_eq!(tree.base_url(), "http://example.test/x/");

    let base = find_element(&tree, "base");
    let head = tree.node(base).parent().expect("base has a parent");
    tree.remove_child(head, base).expect("remove <base>");
    assert_eq!(tree.base_url(), DOC_URL);
}

#[test]
fn the_cache_is_invalidated_by_a_child_list_mutation() {
    let mut tree = parse_at("<head></head><body></body>", DOC_URL);
    assert_eq!(tree.base_url(), DOC_URL);

    let head = find_element(&tree, "head");
    let base = tree.create_element(
        oxidepage_dom::node::html_name(LocalName::from("base")),
        Vec::new(),
    );
    tree.set_attribute(base, href(), "/x/".into());
    tree.append_child(head, base).expect("append <base>");
    assert_eq!(tree.base_url(), "http://example.test/x/");
}

#[test]
fn the_cache_is_invalidated_by_an_href_attribute_write() {
    let mut tree = parse_at(r#"<head><base href="/x/"></head>"#, DOC_URL);
    assert_eq!(tree.base_url(), "http://example.test/x/");

    let base = find_element(&tree, "base");
    tree.set_attribute(base, href(), "/y/".into());
    assert_eq!(tree.base_url(), "http://example.test/y/");

    tree.remove_attribute(base, &href());
    assert_eq!(tree.base_url(), DOC_URL);
}

#[test]
fn the_cache_is_invalidated_by_set_document_url() {
    // `set_document_url` does not move `structure_version`, so it must clear
    // the cache by hand — otherwise the base goes stale across navigation.
    let mut tree = parse_at("<html><body></body></html>", DOC_URL);
    assert_eq!(tree.base_url(), DOC_URL);

    tree.set_document_url("http://other.test/".to_owned());
    assert_eq!(tree.base_url(), "http://other.test/");
}

#[test]
fn a_relative_base_is_rejoined_after_set_document_url() {
    let mut tree = parse_at(r#"<head><base href="c/"></head>"#, DOC_URL);
    assert_eq!(tree.base_url(), "http://example.test/a/b/c/");

    tree.set_document_url("http://other.test/z/".to_owned());
    assert_eq!(tree.base_url(), "http://other.test/z/c/");
}
