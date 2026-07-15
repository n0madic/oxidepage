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
