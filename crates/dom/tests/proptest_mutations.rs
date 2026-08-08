//! Property tests: random sequences of spec mutations keep the tree's
//! intrusive links consistent (Phase 1 exit criterion, design doc §10).
//!
//! The checker verifies, after every operation batch:
//! - parent/child link symmetry (`first_child`/`last_child` vs sibling list,
//!   every child's `parent` back-pointer),
//! - sibling list symmetry (`prev`/`next` mirror each other),
//! - no cycles (tree order enumeration terminates and visits each node once),
//! - `IS_CONNECTED` equals reachability from the document,
//! - document child constraints (≤1 element child, ≤1 doctype, text never a
//!   document child).

use proptest::prelude::*;

use html5ever::local_name;
use oxidepage_dom::node::html_name;
use oxidepage_dom::{DomTree, NodeData, NodeId, NodeKind};

/// A randomized mutation over a bounded node universe.
#[derive(Clone, Debug)]
enum Op {
    CreateElement,
    CreateText(String),
    /// Append node `a` to parent `b` (indices into the node universe).
    Append(usize, usize),
    /// Insert node `a` into parent `b` before `b`'s `k`-th child.
    InsertBefore(usize, usize, usize),
    /// Replace `b`'s `k`-th child with node `a`.
    ReplaceChild(usize, usize, usize),
    /// Remove node `a` from its parent.
    Remove(usize),
    /// Set an attribute on node `a` (exercises the non-structural path).
    SetAttr(usize, String),
    /// Change character data of node `a` if it is a text node.
    SetText(usize, String),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        1 => Just(Op::CreateElement),
        1 => "[a-z]{0,6}".prop_map(Op::CreateText),
        4 => (any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(a, b)| Op::Append(a.index(1 << 16), b.index(1 << 16))),
        3 => (
            any::<prop::sample::Index>(),
            any::<prop::sample::Index>(),
            any::<prop::sample::Index>()
        )
            .prop_map(|(a, b, k)| Op::InsertBefore(
                a.index(1 << 16),
                b.index(1 << 16),
                k.index(1 << 16)
            )),
        2 => (
            any::<prop::sample::Index>(),
            any::<prop::sample::Index>(),
            any::<prop::sample::Index>()
        )
            .prop_map(|(a, b, k)| Op::ReplaceChild(
                a.index(1 << 16),
                b.index(1 << 16),
                k.index(1 << 16)
            )),
        2 => any::<prop::sample::Index>().prop_map(|a| Op::Remove(a.index(1 << 16))),
        1 => (any::<prop::sample::Index>(), "[a-z]{0,4}")
            .prop_map(|(a, v)| Op::SetAttr(a.index(1 << 16), v)),
        1 => (any::<prop::sample::Index>(), "[a-z]{0,4}")
            .prop_map(|(a, v)| Op::SetText(a.index(1 << 16), v)),
    ]
}

fn check_invariants(tree: &DomTree, universe: &[NodeId]) {
    let document = tree.document();

    // Every tracked node is still alive (Phase 1 never frees on detach).
    for &id in universe {
        assert!(tree.get(id).is_some(), "universe node vanished: {id:?}");
    }

    let mut roots: Vec<NodeId> = vec![document];
    for &id in universe {
        if tree.node(id).parent().is_none() {
            roots.push(id);
        }
    }

    for &root in &roots {
        let mut visited = std::collections::HashSet::new();
        for node in tree.inclusive_descendants(root) {
            assert!(visited.insert(node), "cycle: {node:?} visited twice");

            // Sibling list symmetry + parent back-pointers.
            let children: Vec<NodeId> = tree.children(node).collect();
            let n = tree.node(node);
            assert_eq!(n.first_child(), children.first().copied());
            assert_eq!(n.last_child(), children.last().copied());
            for (i, &c) in children.iter().enumerate() {
                let cn = tree.node(c);
                assert_eq!(cn.parent(), Some(node), "child parent link broken");
                let expected_prev = if i > 0 { Some(children[i - 1]) } else { None };
                let expected_next = children.get(i + 1).copied();
                assert_eq!(cn.prev_sibling(), expected_prev, "prev link broken");
                assert_eq!(cn.next_sibling(), expected_next, "next link broken");
            }

            // Connectedness == reachability from the document node.
            let reachable_from_document = root == document;
            assert_eq!(
                tree.node(node).is_connected(),
                reachable_from_document,
                "IS_CONNECTED out of sync for {node:?}"
            );
        }
    }

    // Document child constraints.
    let doc_children: Vec<NodeId> = tree.children(document).collect();
    let element_children = doc_children
        .iter()
        .filter(|&&c| tree.node(c).data().kind() == NodeKind::Element)
        .count();
    let doctype_children = doc_children
        .iter()
        .filter(|&&c| tree.node(c).data().kind() == NodeKind::Doctype)
        .count();
    assert!(element_children <= 1, "document grew two element children");
    assert!(doctype_children <= 1, "document grew two doctypes");
    assert!(
        !doc_children
            .iter()
            .any(|&c| tree.node(c).data().kind() == NodeKind::Text),
        "document grew a text child"
    );

    check_id_index(tree, universe);
}

/// The `id` index holds exactly the connected elements carrying an `id`, and
/// `element_by_id` agrees with a linear tree-order scan. Catches any mutation
/// path that changes connectedness or the `id` attribute behind the index's
/// back.
fn check_id_index(tree: &DomTree, universe: &[NodeId]) {
    let mut expected: Vec<(String, NodeId)> = Vec::new();
    for node in tree.inclusive_descendants(tree.document()) {
        if let Some(id) = tree.node(node).as_element().and_then(|el| el.id()) {
            expected.push((id.to_string(), node));
        }
    }

    // No detached element may leak into the index.
    for &id in universe {
        if !tree.node(id).is_connected() {
            let el_id = tree.node(id).as_element().and_then(|el| el.id());
            if let Some(el_id) = el_id {
                assert_ne!(
                    tree.element_by_id(tree.document(), el_id),
                    Some(id),
                    "detached {id:?} is still indexed under `{el_id}`"
                );
            }
        }
    }

    let mut names: Vec<&str> = tree.id_names_in(tree.document()).collect();
    names.sort_unstable();
    let mut expected_names: Vec<&str> = expected.iter().map(|(name, _)| name.as_str()).collect();
    expected_names.sort_unstable();
    expected_names.dedup();
    assert_eq!(names, expected_names, "indexed id set diverged");

    for name in expected_names {
        let first = expected
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, node)| *node);
        assert_eq!(
            tree.element_by_id(tree.document(), name),
            first,
            "element_by_id(`{name}`) is not the first match in tree order"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_mutations_keep_links_consistent(ops in prop::collection::vec(op_strategy(), 1..120)) {
        let mut tree = DomTree::new();
        let html = tree.create_element(html_name(local_name!("html")), vec![]);
        tree.append_child(tree.document(), html).unwrap();
        let mut universe: Vec<NodeId> = vec![html];

        for op in ops {
            match op {
                Op::CreateElement => {
                    universe.push(tree.create_element(html_name(local_name!("div")), vec![]));
                }
                Op::CreateText(s) => {
                    universe.push(tree.create_text(s.as_str().into()));
                }
                Op::Append(a, b) => {
                    let node = universe[a % universe.len()];
                    let parent = universe[b % universe.len()];
                    // Result intentionally ignored: invalid operations must
                    // reject cleanly and leave the tree untouched.
                    let _ = tree.append_child(parent, node);
                }
                Op::InsertBefore(a, b, k) => {
                    let node = universe[a % universe.len()];
                    let parent = universe[b % universe.len()];
                    let children: Vec<NodeId> = tree.children(parent).collect();
                    let before = if children.is_empty() {
                        None
                    } else {
                        Some(children[k % children.len()])
                    };
                    let _ = tree.insert_before(parent, node, before);
                }
                Op::ReplaceChild(a, b, k) => {
                    let node = universe[a % universe.len()];
                    let parent = universe[b % universe.len()];
                    let children: Vec<NodeId> = tree.children(parent).collect();
                    if !children.is_empty() {
                        let child = children[k % children.len()];
                        let _ = tree.replace_child(parent, node, child);
                    }
                }
                Op::Remove(a) => {
                    let node = universe[a % universe.len()];
                    tree.remove(node);
                }
                Op::SetAttr(a, v) => {
                    let node = universe[a % universe.len()];
                    tree.set_attribute(
                        node,
                        oxidepage_dom::node::attr_name(local_name!("id")),
                        v.as_str().into(),
                    );
                }
                Op::SetText(a, v) => {
                    let node = universe[a % universe.len()];
                    if matches!(tree.node(node).data(), NodeData::Text(_)) {
                        tree.set_character_data(node, v.as_str().into());
                    }
                }
            }
            check_invariants(&tree, &universe);
        }
    }

    #[test]
    fn parsing_produces_consistent_trees(html in "[a-zA-Z<>/!= \"']{0,120}") {
        // Arbitrary tag soup must never break arena invariants.
        let parsed = oxidepage_dom::parse_document(&html, oxidepage_dom::ParseOptions::default());
        check_invariants(&parsed.tree, &[]);
    }
}
