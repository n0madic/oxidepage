//! html5lib-tests tree dump format ("| <html>" lines), used by the
//! tree-construction conformance runner and handy for debugging.

use std::fmt::Write;

use html5ever::{QualName, ns};
use oxidepage_base::NodeId;

use crate::node::NodeData;
use crate::tree::DomTree;

/// Dumps the subtree rooted at `root` (children only) in html5lib format.
#[must_use]
pub fn dump_tree(tree: &DomTree, root: NodeId) -> String {
    let mut out = String::new();
    for child in tree.children(root) {
        dump_node(tree, child, 0, &mut out);
    }
    out
}

/// Dumps the whole document in html5lib format.
#[must_use]
pub fn dump_document(tree: &DomTree) -> String {
    dump_tree(tree, tree.document())
}

fn indent(depth: usize, out: &mut String) {
    out.push_str("| ");
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn dump_node(tree: &DomTree, node: NodeId, depth: usize, out: &mut String) {
    indent(depth, out);
    match tree.node(node).data() {
        NodeData::Document(_) | NodeData::DocumentFragment { .. } => {
            unreachable!("containers are dumped via their children")
        }
        NodeData::Doctype {
            name,
            public_id,
            system_id,
        } => {
            if public_id.is_empty() && system_id.is_empty() {
                let _ = writeln!(out, "<!DOCTYPE {name}>");
            } else {
                let _ = writeln!(out, "<!DOCTYPE {name} \"{public_id}\" \"{system_id}\">");
            }
        }
        NodeData::Text(t) => {
            let _ = writeln!(out, "\"{t}\"");
        }
        NodeData::CdataSection(t) => {
            let _ = writeln!(out, "<![CDATA[{t}]]>");
        }
        NodeData::Comment(t) => {
            let _ = writeln!(out, "<!-- {t} -->");
        }
        NodeData::ProcessingInstruction { target, data } => {
            let _ = writeln!(out, "<?{target} {data}>");
        }
        NodeData::Element(el) => {
            let _ = writeln!(out, "<{}>", element_display_name(&el.name));

            let mut attrs: Vec<(String, &str)> = el
                .attrs()
                .iter()
                .map(|a| (attr_display_name(&a.name), &a.value[..]))
                .collect();
            attrs.sort();
            for (name, value) in attrs {
                indent(depth + 1, out);
                let _ = writeln!(out, "{name}=\"{value}\"");
            }

            if let Some(contents) = el.template_contents() {
                indent(depth + 1, out);
                out.push_str("content\n");
                for child in tree.children(contents) {
                    dump_node(tree, child, depth + 2, out);
                }
            }
            for child in tree.children(node) {
                dump_node(tree, child, depth + 1, out);
            }
        }
    }
}

fn element_display_name(name: &QualName) -> String {
    match name.ns {
        ref x if *x == ns!(svg) => format!("svg {}", name.local),
        ref x if *x == ns!(mathml) => format!("math {}", name.local),
        _ => name.local.to_string(),
    }
}

fn attr_display_name(name: &QualName) -> String {
    match name.ns {
        ref x if *x == ns!(xlink) => format!("xlink {}", name.local),
        ref x if *x == ns!(xml) => format!("xml {}", name.local),
        ref x if *x == ns!(xmlns) => format!("xmlns {}", name.local),
        _ => name.local.to_string(),
    }
}
