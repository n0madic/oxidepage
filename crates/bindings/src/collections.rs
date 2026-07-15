//! Collection host data: `NodeList`, `HTMLCollection`, and `DOMTokenList`
//! share one storage shape; liveness comes from recomputing items on every
//! access (correct by construction — no invalidation protocol needed at
//! Phase 2 document sizes).

use oxidepage_base::NodeId;
use oxidepage_dom::{DomTree, LocalName, Namespace, NodeKind};

use crate::imp::names::qualified_name;

/// What a collection host object walks over.
pub(crate) enum CollectionData {
    /// Live `NodeList` of a node's children.
    ChildNodes(NodeId),
    /// Static `NodeList` snapshot (`querySelectorAll`, record node lists).
    StaticNodes(Vec<NodeId>),
    /// Live `HTMLCollection` of element children.
    Children(NodeId),
    /// Live `getElementsByTagName` collection. `name == None` means `*`. The
    /// query is matched against each candidate's *qualified* name (DOM
    /// §4.2.6): an HTML element in an HTML document matches when the query,
    /// ASCII-lowercased once here at creation, equals the candidate's
    /// qualified name *as stored* — so a candidate that isn't already
    /// lowercase (e.g. built via `createElementNS`) can never match, in
    /// either case of the query. Every other element matches literally.
    ByTagName { root: NodeId, name: Option<String> },
    /// Live `getElementsByTagNameNS` collection. `namespace == None` means the
    /// wildcard `"*"` (any namespace, including null); `Some(ns)` matches only
    /// that namespace, where the null namespace is `Namespace::from("")`.
    /// `local_name == None` means the wildcard `"*"`. Never case-folds.
    ByTagNameNS {
        root: NodeId,
        namespace: Option<Namespace>,
        local_name: Option<LocalName>,
    },
    /// Live `getElementsByClassName` collection (all classes must match).
    ByClassName {
        root: NodeId,
        classes: Vec<LocalName>,
    },
    /// Live `DOMTokenList` over an element attribute's tokens.
    TokenList { element: NodeId, attr: LocalName },
    /// Live `form.elements`: the listed controls owned by this form, in tree
    /// order — including ones associated only by a `form` content attribute.
    FormControls(NodeId),
    /// Live `fieldset.elements`: the fieldset's listed *descendants*.
    FieldSetControls(NodeId),
    /// Live `select.options`.
    SelectOptions(NodeId),
    /// Live `select.selectedOptions`.
    SelectedOptions(NodeId),
    /// Live `control.labels`.
    Labels(NodeId),
}

impl CollectionData {
    /// The current items, in tree order. Stale ids in static snapshots are
    /// skipped defensively.
    pub fn items(&self, dom: &DomTree) -> Vec<NodeId> {
        match self {
            CollectionData::ChildNodes(parent) => match dom.get(*parent) {
                Some(_) => dom.children(*parent).collect(),
                None => Vec::new(),
            },
            CollectionData::StaticNodes(nodes) => nodes
                .iter()
                .copied()
                .filter(|&id| dom.get(id).is_some())
                .collect(),
            CollectionData::Children(parent) => match dom.get(*parent) {
                Some(_) => dom
                    .children(*parent)
                    .filter(|&c| dom.node(c).data().kind() == NodeKind::Element)
                    .collect(),
                None => Vec::new(),
            },
            CollectionData::ByTagName { root, name } => {
                let Some(name) = name else {
                    return Self::element_descendants(dom, *root, |_, _| true);
                };
                let lower = name.to_ascii_lowercase();
                Self::element_descendants(dom, *root, |dom, id| {
                    let el = dom.node(id).as_element().expect("filtered to elements");
                    let qualified = qualified_name(&el.name);
                    if el.is_html_element() {
                        qualified == lower
                    } else {
                        qualified == *name
                    }
                })
            }
            CollectionData::ByTagNameNS {
                root,
                namespace,
                local_name,
            } => Self::element_descendants(dom, *root, |dom, id| {
                let el = dom.node(id).as_element().expect("filtered to elements");
                let ns_ok = namespace.as_ref().is_none_or(|ns| el.name.ns == *ns);
                let name_ok = local_name
                    .as_ref()
                    .is_none_or(|name| el.name.local == *name);
                ns_ok && name_ok
            }),
            CollectionData::ByClassName { root, classes } => {
                Self::element_descendants(dom, *root, |dom, id| {
                    // The query class tokens are html5ever atoms; the element's
                    // class cache is stylo atoms (Phase 4). Compare by string.
                    classes.iter().all(|class| {
                        let el = dom.node(id).as_element().expect("filtered to elements");
                        el.classes().iter().any(|c| **c == **class)
                    })
                })
            }
            CollectionData::TokenList { .. } => Vec::new(),
            // The form/select/label collections are derived in the DOM, which
            // owns the form-association rules (the `form` content attribute can
            // associate a control that is nowhere near its form).
            CollectionData::FormControls(form) => match dom.get(*form) {
                Some(_) => dom.form_controls(*form),
                None => Vec::new(),
            },
            CollectionData::FieldSetControls(fieldset) => match dom.get(*fieldset) {
                Some(_) => dom.fieldset_controls(*fieldset),
                None => Vec::new(),
            },
            CollectionData::SelectOptions(select) => match dom.get(*select) {
                Some(_) => dom.select_options(*select),
                None => Vec::new(),
            },
            CollectionData::SelectedOptions(select) => match dom.get(*select) {
                Some(_) => dom
                    .select_options(*select)
                    .into_iter()
                    .filter(|&o| dom.checkedness(o))
                    .collect(),
                None => Vec::new(),
            },
            CollectionData::Labels(control) => match dom.get(*control) {
                Some(_) => dom.labels_for(*control),
                None => Vec::new(),
            },
        }
    }

    /// The current tokens of a `TokenList` (ordered, deduplicated).
    pub fn tokens(&self, dom: &DomTree) -> Vec<String> {
        let CollectionData::TokenList { element, attr } = self else {
            return Vec::new();
        };
        let Some(node) = dom.get(*element) else {
            return Vec::new();
        };
        let Some(el) = node.as_element() else {
            return Vec::new();
        };
        let name = oxidepage_dom::node::attr_name(attr.clone());
        let Some(value) = el.attr(&name) else {
            return Vec::new();
        };
        let mut seen = Vec::new();
        for token in value.split_ascii_whitespace() {
            if !seen.iter().any(|s: &String| s == token) {
                seen.push(token.to_owned());
            }
        }
        seen
    }

    fn element_descendants(
        dom: &DomTree,
        root: NodeId,
        mut filter: impl FnMut(&DomTree, NodeId) -> bool,
    ) -> Vec<NodeId> {
        if dom.get(root).is_none() {
            return Vec::new();
        }
        dom.inclusive_descendants(root)
            .skip(1)
            .filter(|&id| dom.node(id).data().kind() == NodeKind::Element)
            .filter(|&id| filter(dom, id))
            .collect()
    }
}
