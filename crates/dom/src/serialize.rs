//! HTML serialization over the arena via html5ever's serializer:
//! `innerHTML` / `outerHTML` round-tripping (design doc §5.2).

use std::io;

use html5ever::QualName;
use html5ever::serialize::{Serialize, SerializeOpts, Serializer, TraversalScope};
use html5ever::{local_name, ns};
use oxidepage_base::NodeId;

use crate::node::NodeData;
use crate::tree::DomTree;

/// A `(tree, node)` pair html5ever's serializer can walk.
pub struct SerializableHandle<'a> {
    pub tree: &'a DomTree,
    pub node: NodeId,
}

impl Serialize for SerializableHandle<'_> {
    fn serialize<S>(&self, serializer: &mut S, traversal_scope: TraversalScope) -> io::Result<()>
    where
        S: Serializer,
    {
        serialize_node(self.tree, self.node, serializer, traversal_scope)
    }
}

/// A unit of pending serialization work on the explicit stack.
enum Work {
    /// Serialize this node with the given scope.
    Node(NodeId, TraversalScope),
    /// Emit the closing tag for an element whose start tag was already written.
    EndElem(QualName),
}

fn serialize_node<S>(
    tree: &DomTree,
    node: NodeId,
    serializer: &mut S,
    scope: TraversalScope,
) -> io::Result<()>
where
    S: Serializer,
{
    // Explicit-stack traversal so deeply nested content cannot overflow the
    // native stack: emit each start tag, push the children (in reverse so they
    // pop in document order) and a matching `EndElem` beneath them.
    let mut stack: Vec<Work> = vec![Work::Node(node, scope)];
    while let Some(work) = stack.pop() {
        match work {
            Work::EndElem(name) => serializer.end_elem(name)?,
            Work::Node(node, TraversalScope::ChildrenOnly(_)) => {
                push_children(tree, node, &mut stack);
            }
            Work::Node(node, TraversalScope::IncludeNode) => match tree.node(node).data() {
                NodeData::Element(el) => {
                    serializer.start_elem(
                        el.name.clone(),
                        el.attrs().iter().map(|a| (&a.name, &a.value[..])),
                    )?;
                    stack.push(Work::EndElem(el.name.clone()));
                    push_children(tree, node, &mut stack);
                }
                // The HTML serializer has no CDATA production — a CDATASection
                // can only reach it by being adopted into an HTML document,
                // where it serializes as its text content.
                NodeData::Text(t) | NodeData::CdataSection(t) => serializer.write_text(t)?,
                NodeData::Comment(t) => serializer.write_comment(t)?,
                NodeData::Doctype { name, .. } => serializer.write_doctype(name)?,
                NodeData::ProcessingInstruction { target, data } => {
                    serializer.write_processing_instruction(target, data)?;
                }
                NodeData::Document(_) | NodeData::DocumentFragment { .. } => {
                    push_children(tree, node, &mut stack);
                }
            },
        }
    }
    Ok(())
}

/// Pushes `node`'s children onto `stack` in reverse order, so the traversal
/// pops them in document order.
fn push_children(tree: &DomTree, node: NodeId, stack: &mut Vec<Work>) {
    // Per the HTML serialization spec, a template element serializes its
    // template contents in place of its (empty) child list.
    let container = match tree.node(node).as_element() {
        Some(el) if el.name.ns == ns!(html) && el.name.local == local_name!("template") => {
            el.template_contents().unwrap_or(node)
        }
        _ => node,
    };
    let children: Vec<NodeId> = tree.children(container).collect();
    for child in children.into_iter().rev() {
        stack.push(Work::Node(child, TraversalScope::IncludeNode));
    }
}

/// A unit of pending XML serialization work on the explicit stack.
enum XmlWork {
    /// Serialize this node and its descendants.
    Node(NodeId),
    /// Emit the closing tag for an element whose start tag was already written.
    EndElem(String),
}

/// XML serialization of `node` and its descendants (DOM Parsing and
/// Serialization, "XML serialization"), backing `XMLSerializer`.
///
/// This is not the HTML serializer with different escapes: XML self-closes an
/// empty element, has a CDATA production, and has no `<template>` contents
/// rule.
///
/// **Deliberate limit — no namespace prefix generation.** The spec's namespace
/// prefix map, its invention of `xmlns`/`ns1:` declarations for nodes whose
/// namespace no in-scope prefix covers, and its well-formedness errors are not
/// implemented. Whatever prefix a node or attribute stores is emitted as-is,
/// and `xmlns` attributes are emitted as the ordinary attributes they are
/// stored as. So a tree parsed from markup round-trips, but a tree assembled
/// from script with `createElementNS` and no matching `xmlns` attribute
/// serializes without a namespace declaration, where the spec would invent
/// one — and no input is ever rejected as not well-formed.
#[must_use]
pub fn xml_serialize(tree: &DomTree, node: NodeId) -> String {
    let mut out = String::new();
    // Explicit-stack traversal, for the same reason `serialize_node` uses one:
    // deeply nested content must not overflow the native stack.
    let mut stack: Vec<XmlWork> = vec![XmlWork::Node(node)];
    while let Some(work) = stack.pop() {
        match work {
            XmlWork::EndElem(name) => {
                out.push_str("</");
                out.push_str(&name);
                out.push('>');
            }
            XmlWork::Node(node) => match tree.node(node).data() {
                NodeData::Element(el) => {
                    let name = xml_qualified_name(&el.name);
                    out.push('<');
                    out.push_str(&name);
                    for attr in el.attrs() {
                        out.push(' ');
                        out.push_str(&xml_qualified_name(&attr.name));
                        out.push_str("=\"");
                        escape_attribute(&attr.value, &mut out);
                        out.push('"');
                    }
                    // XML serialization has no template-contents rule: a
                    // `<template>` serializes its *actual* children (its
                    // contents fragment is a separate tree that the XML
                    // production knows nothing about), unlike `push_children`.
                    let children: Vec<NodeId> = tree.children(node).collect();
                    if children.is_empty() {
                        // An empty element self-closes — the XML rule, and the
                        // one shape the HTML serializer never emits.
                        out.push_str("/>");
                    } else {
                        out.push('>');
                        stack.push(XmlWork::EndElem(name));
                        for child in children.into_iter().rev() {
                            stack.push(XmlWork::Node(child));
                        }
                    }
                }
                NodeData::Text(t) => escape_text(t, &mut out),
                // The one place a `CDATASection` must *not* be flattened into
                // Text: XML has a CDATA production, and its content is not
                // escaped.
                NodeData::CdataSection(t) => {
                    out.push_str("<![CDATA[");
                    out.push_str(t);
                    out.push_str("]]>");
                }
                NodeData::Comment(t) => {
                    out.push_str("<!--");
                    out.push_str(t);
                    out.push_str("-->");
                }
                NodeData::Doctype {
                    name,
                    public_id,
                    system_id,
                } => {
                    out.push_str("<!DOCTYPE ");
                    out.push_str(name);
                    if !public_id.is_empty() {
                        out.push_str(" PUBLIC \"");
                        out.push_str(public_id);
                        out.push('"');
                    } else if !system_id.is_empty() {
                        out.push_str(" SYSTEM");
                    }
                    if !system_id.is_empty() {
                        out.push_str(" \"");
                        out.push_str(system_id);
                        out.push('"');
                    }
                    out.push('>');
                }
                NodeData::ProcessingInstruction { target, data } => {
                    out.push_str("<?");
                    out.push_str(target);
                    out.push(' ');
                    out.push_str(data);
                    out.push_str("?>");
                }
                NodeData::Document(_) | NodeData::DocumentFragment { .. } => {
                    let children: Vec<NodeId> = tree.children(node).collect();
                    for child in children.into_iter().rev() {
                        stack.push(XmlWork::Node(child));
                    }
                }
            },
        }
    }
    out
}

/// `prefix:local`, or bare `local` when no prefix is stored. The equivalent of
/// `bindings`' `imp::names::qualified_name`, written out here rather than
/// depended on — `dom` sits below `bindings`.
fn xml_qualified_name(name: &QualName) -> String {
    match &name.prefix {
        Some(prefix) => format!("{prefix}:{}", name.local),
        None => name.local.to_string(),
    }
}

/// Escapes an attribute value. Distinct from [`escape_text`] in exactly one
/// way, and it is the load-bearing one: `"` closes the value here, so it
/// **must** be escaped.
fn escape_attribute(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Escapes character data. A `"` is ordinary data in text and must **not** be
/// escaped — see [`escape_attribute`] for the other mode.
fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Spec `innerHTML` getter: serializes the node's descendants.
#[must_use]
pub fn inner_html(tree: &DomTree, node: NodeId) -> String {
    serialize_to_string(tree, node, TraversalScope::ChildrenOnly(None))
}

/// Spec `outerHTML` getter: serializes the node itself and its descendants.
#[must_use]
pub fn outer_html(tree: &DomTree, node: NodeId) -> String {
    serialize_to_string(tree, node, TraversalScope::IncludeNode)
}

fn serialize_to_string(tree: &DomTree, node: NodeId, scope: TraversalScope) -> String {
    let mut out = Vec::new();
    let handle = SerializableHandle { tree, node };
    html5ever::serialize(
        &mut out,
        &handle,
        SerializeOpts {
            traversal_scope: scope,
            ..SerializeOpts::default()
        },
    )
    .expect("serialization to a Vec cannot fail");
    String::from_utf8(out).expect("serializer emits UTF-8")
}
