//! Second Documents: node ownership, adoption, inertness, CDATASection, and
//! the owner-pin that keeps `ownerDocument` from dangling (ADR-0017).

use html5ever::ns;
use oxidepage_dom::node::{DocumentData, NodeKind, html_name, qual_name};
use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};

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

fn xml_doc(tree: &mut DomTree) -> NodeId {
    tree.create_document(DocumentData::xml(
        "about:blank".to_owned(),
        "application/xml".to_owned(),
        false,
    ))
}

// === Ownership ===

#[test]
fn page_document_owns_its_nodes_and_owns_no_one() {
    let tree = parse("<div id=a>x</div>");
    let div = find_element(&tree, "div");
    assert_eq!(tree.owner_document(div), Some(tree.document()));
    assert_eq!(tree.node_document(div), tree.document());

    // `ownerDocument` of a Document is null; its node document is itself.
    assert_eq!(tree.owner_document(tree.document()), None);
    assert_eq!(tree.node_document(tree.document()), tree.document());
}

#[test]
fn second_document_owns_the_nodes_it_creates() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let el = tree.create_element_in(doc2, html_name("p".into()), Vec::new());
    assert_eq!(tree.owner_document(el), Some(doc2));
    assert_ne!(tree.node_document(el), tree.document());
}

/// The document is allocated first into a fresh arena, so a *second* document
/// cannot disturb the "document is slot (0, gen 1)" invariant the JS
/// `document` wrapper depends on.
#[test]
fn second_document_does_not_take_slot_zero() {
    let mut tree = DomTree::new();
    let page = tree.document();
    let doc2 = xml_doc(&mut tree);
    assert_ne!(doc2, page);
    assert_eq!(tree.document(), page);
}

// === Inertness ===

#[test]
fn second_document_and_its_subtree_are_never_connected() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let root = tree.create_element_in(doc2, html_name("html".into()), Vec::new());
    tree.append_child(doc2, root).unwrap();
    let child = tree.create_element_in(doc2, html_name("div".into()), Vec::new());
    tree.append_child(root, child).unwrap();

    // The engine flag means "in the rendered document" — style, layout and all
    // resource hooks key on it, so a second document is inert for free.
    assert!(!tree.node(doc2).is_connected());
    assert!(!tree.node(root).is_connected());
    assert!(!tree.node(child).is_connected());

    // But the *spec* predicate is true: the shadow-including root is a Document.
    assert!(tree.is_spec_connected(doc2));
    assert!(tree.is_spec_connected(root));
    assert!(tree.is_spec_connected(child));
}

#[test]
fn detached_node_is_not_spec_connected() {
    let mut tree = parse("<div></div>");
    let orphan = tree.create_element(html_name("p".into()), Vec::new());
    assert!(!tree.is_spec_connected(orphan));
    // …and a node in the page tree is.
    assert!(tree.is_spec_connected(find_element(&tree, "div")));
}

/// `template.content` has no browsing context and is not connected, in either
/// sense: `composed_parent` deliberately does not cross to the template host.
#[test]
fn template_contents_are_not_spec_connected() {
    let tree = parse("<template><b>x</b></template>");
    let template = find_element(&tree, "template");
    let contents = tree
        .node(template)
        .as_element()
        .unwrap()
        .template_contents();
    let contents = contents.expect("template has contents");
    assert!(!tree.is_spec_connected(contents));
}

#[test]
fn a_document_can_never_acquire_a_parent() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let div = tree.create_element(html_name("div".into()), Vec::new());
    // Structurally an eternal detached root: hierarchy validity rejects it.
    assert!(tree.append_child(div, doc2).is_err());
    assert!(tree.append_child(tree.document(), doc2).is_err());
}

// === Adoption ===

#[test]
fn insertion_adopts_the_subtree_into_the_parents_document() {
    let mut tree = parse("<div id=host></div>");
    let host = find_element(&tree, "div");
    let doc2 = xml_doc(&mut tree);

    let el = tree.create_element_in(doc2, html_name("section".into()), Vec::new());
    let text = tree.create_text_in(doc2, "hi".into());
    tree.append_child(el, text).unwrap();
    assert_eq!(tree.owner_document(text), Some(doc2));

    // Inserting into the page document adopts the whole subtree.
    tree.append_child(host, el).unwrap();
    assert_eq!(tree.owner_document(el), Some(tree.document()));
    assert_eq!(tree.owner_document(text), Some(tree.document()));
    assert!(tree.node(el).is_connected());
    assert!(tree.node(text).is_connected());
}

#[test]
fn adopting_out_of_the_page_document_disconnects() {
    let mut tree = parse("<div><span>x</span></div>");
    let div = find_element(&tree, "div");
    let span = find_element(&tree, "span");
    let doc2 = xml_doc(&mut tree);

    tree.adopt(div, doc2);
    assert_eq!(tree.owner_document(div), Some(doc2));
    assert_eq!(tree.owner_document(span), Some(doc2));
    assert!(!tree.node(div).is_connected());
    assert!(!tree.node(span).is_connected());
    assert_eq!(tree.node(div).parent(), None);
}

#[test]
fn cloning_stays_in_the_source_document_and_import_moves_it() {
    let mut tree = parse("<div><b>x</b></div>");
    let div = find_element(&tree, "div");
    let doc2 = xml_doc(&mut tree);

    // cloneNode never changes documents.
    let clone = tree.clone_subtree(div, true).unwrap();
    assert_eq!(tree.owner_document(clone), Some(tree.document()));

    // importNode clones into the target document, deeply.
    let imported = tree.clone_subtree_into(div, true, doc2).unwrap();
    assert_eq!(tree.owner_document(imported), Some(doc2));
    let imported_child = tree.children(imported).next().expect("deep clone");
    assert_eq!(tree.owner_document(imported_child), Some(doc2));

    // Cloning a Document is still not supported.
    assert!(tree.clone_subtree(doc2, true).is_err());
}

// === The owner pin (the one genuinely new invariant) ===

/// A node created by `doc2.createElement()` and never inserted is its *own*
/// detached root — it is not in doc2's subtree, so `subtree_has_pins(doc2)`
/// cannot see it. Without the owner pin, GC of the doc2 wrapper would free
/// doc2 and leave this element's `ownerDocument` naming a freed slot.
#[test]
fn a_pinned_node_keeps_its_document_alive() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let el = tree.create_element_in(doc2, html_name("p".into()), Vec::new());
    assert_eq!(tree.node(el).parent(), None, "el is its own detached root");

    tree.pin(el); // el has a live JS wrapper; doc2's wrapper has been collected
    assert!(!tree.free_detached_tree_if_unpinned(doc2));
    assert!(tree.get(doc2).is_some());
    assert_eq!(tree.owner_document(el), Some(doc2));

    // Once the last wrapper into doc2 goes, doc2 is freed like any fragment.
    tree.unpin(el);
    assert!(tree.free_detached_tree_if_unpinned(doc2));
    assert!(tree.get(doc2).is_none());
}

#[test]
fn a_pinned_document_survives_on_its_own() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    tree.pin(doc2);
    assert!(!tree.free_detached_tree_if_unpinned(doc2));
    tree.unpin(doc2);
    assert!(tree.free_detached_tree_if_unpinned(doc2));
}

/// Adoption must *move* the owner pin, not duplicate or drop it: the old
/// document must become freeable, and the new one must not.
#[test]
fn adoption_transfers_the_owner_pin() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let doc3 = xml_doc(&mut tree);

    let el = tree.create_element_in(doc2, html_name("p".into()), Vec::new());
    tree.pin(el);
    assert!(!tree.free_detached_tree_if_unpinned(doc2));

    tree.adopt(el, doc3);
    // doc2 no longer holds the pin, so it can go…
    assert!(tree.free_detached_tree_if_unpinned(doc2));
    // …and doc3 now holds it, so it cannot.
    assert!(!tree.free_detached_tree_if_unpinned(doc3));
    assert_eq!(tree.owner_document(el), Some(doc3));

    tree.unpin(el);
    assert!(tree.free_detached_tree_if_unpinned(doc3));
}

/// Adopting *into* the page document releases the second document, and the
/// node stays alive because the page document's tree is never freed.
#[test]
fn adopting_into_the_page_document_releases_the_second_one() {
    let mut tree = parse("<div id=host></div>");
    let host = find_element(&tree, "div");
    let doc2 = xml_doc(&mut tree);
    let el = tree.create_element_in(doc2, html_name("p".into()), Vec::new());
    tree.pin(el);

    tree.append_child(host, el).unwrap();
    assert_eq!(tree.owner_document(el), Some(tree.document()));
    assert!(tree.free_detached_tree_if_unpinned(doc2));
    assert!(tree.get(el).is_some(), "el lives on in the page document");
}

// === CDATASection ===

#[test]
fn cdata_section_is_a_text_node_for_every_rule() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let cdata = tree.create_cdata_section_in(doc2, "a < b".into());

    assert_eq!(tree.node(cdata).data().kind(), NodeKind::CdataSection);
    assert!(tree.node(cdata).is_text());
    assert_eq!(&**tree.node(cdata).character_data().unwrap(), "a < b");
    assert_eq!(tree.text_content(cdata), "a < b");

    // It is a Text node, so a Document must not have it as a child…
    assert!(tree.append_child(doc2, cdata).is_err());
    // …but an element may.
    let root = tree.create_element_in(doc2, qual_name(ns!(), "root".into()), Vec::new());
    tree.append_child(doc2, root).unwrap();
    tree.append_child(root, cdata).unwrap();
    assert_eq!(tree.text_content(root), "a < b");
}

#[test]
fn cdata_section_compares_and_clones_as_itself() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let a = tree.create_cdata_section_in(doc2, "x".into());
    let b = tree.create_cdata_section_in(doc2, "x".into());
    let text = tree.create_text_in(doc2, "x".into());

    assert!(tree.is_equal_node(a, b));
    // Same content, different kind: not equal.
    assert!(!tree.is_equal_node(a, text));

    let clone = tree.clone_subtree(a, false).unwrap();
    assert_eq!(tree.node(clone).data().kind(), NodeKind::CdataSection);
    assert!(tree.is_equal_node(a, clone));
}

/// `normalize()` merges *exclusive* Text nodes — a CDATASection is not one.
#[test]
fn normalize_leaves_cdata_sections_alone() {
    let mut tree = DomTree::new();
    let doc2 = xml_doc(&mut tree);
    let root = tree.create_element_in(doc2, qual_name(ns!(), "root".into()), Vec::new());
    let t1 = tree.create_text_in(doc2, "a".into());
    let cdata = tree.create_cdata_section_in(doc2, "b".into());
    let t2 = tree.create_text_in(doc2, "c".into());
    for child in [t1, cdata, t2] {
        tree.append_child(root, child).unwrap();
    }

    tree.normalize(root);
    let kinds: Vec<NodeKind> = tree
        .children(root)
        .map(|c| tree.node(c).data().kind())
        .collect();
    assert_eq!(
        kinds,
        [NodeKind::Text, NodeKind::CdataSection, NodeKind::Text],
        "the CDATA section must not absorb or be absorbed by its Text siblings"
    );
}

// === Per-document metadata ===

#[test]
fn documents_carry_their_own_kind_and_url() {
    let mut tree = parse("<div></div>");
    let page = tree.document();
    tree.set_document_url("https://example.com/a".to_owned());
    let doc2 = xml_doc(&mut tree);
    tree.set_document_url_of(doc2, "https://other.example/b".to_owned());

    assert!(tree.is_html_document(page));
    assert!(!tree.is_html_document(doc2));
    assert_eq!(tree.document_url_of(page), "https://example.com/a");
    assert_eq!(tree.document_url_of(doc2), "https://other.example/b");
    assert_eq!(
        tree.document_data(doc2).map(|d| d.content_type.as_str()),
        Some("application/xml")
    );
    // The page document's URL accessor is unchanged.
    assert_eq!(tree.document_url(), "https://example.com/a");
}

#[test]
fn document_element_is_per_document() {
    let mut tree = parse("<div></div>");
    let doc2 = xml_doc(&mut tree);
    assert_eq!(tree.document_element_of(doc2), None);

    let root = tree.create_element_in(doc2, qual_name(ns!(), "root".into()), Vec::new());
    tree.append_child(doc2, root).unwrap();
    assert_eq!(tree.document_element_of(doc2), Some(root));
    // The page document still reports its own.
    assert_eq!(
        tree.document_element_of(tree.document()),
        tree.document_element()
    );
}
