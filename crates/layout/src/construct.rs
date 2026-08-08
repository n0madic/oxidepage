//! Box tree construction: classify DOM children into block/inline formatting
//! contexts, insert anonymous blocks, and shape inline content with parley
//! (adapted from blitz-dom `layout/construct.rs`, restructured onto the
//! separate box tree of ADR-0006).
//!
//! Construction happens in two phases so parley contexts are never borrowed
//! re-entrantly (an inline-block inside an IFC starts its *own* IFC):
//!
//! 1. [`Builder`] walks the resolved DOM and builds the structural box tree,
//!    capturing taffy styles and recording every inline-formatting-context
//!    root together with its participating DOM nodes.
//! 2. [`build_ifc`] shapes each recorded IFC with parley, pushing
//!    `InlineBox` placeholders whose ids are the [`BoxId`]s of the atomic
//!    inline child boxes built in phase 1.
//!
//! The caller must hold an `enter_active_tree` scope for the whole build
//! (anonymous-box styling may create `NodeRef`s; ADR-0005). The compute
//! modules never touch the DOM — everything they need is captured here.

use std::collections::HashMap;

use html5ever::local_name;
use oxidepage_base::NodeId;
use oxidepage_dom::node::attr_name;
use oxidepage_dom::node::is_text_kind;
use oxidepage_dom::{DomTree, NodeData, NodeKind};
use oxidepage_style::{StyleEngine, Viewport};
use parley::{InlineBox, InlineBoxKind, TreeBuilder, WhiteSpaceCollapse};
use servo_arc::Arc as ServoArc;
use style::computed_values::pointer_events::T as PointerEvents;
use style::computed_values::position::T as Position;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement as StyloPseudoElement;
use style::values::computed::font::LineHeight;
use style::values::computed::{
    Clear, Content, ContentItem, Display, FlexBasis, Float, MaxSize as StyloMaxSize,
    Size as StyloSize, TextIndent, TextTransform,
};
use style::values::specified::align::AlignFlags;
use style::values::specified::box_::{DisplayInside, DisplayOutside};

use crate::fonts::FontSystem;
use crate::marker::MarkerPosition;
use crate::text;
use crate::tree::{
    BoxId, BoxKind, IfcData, IntrinsicSizeKeyword, IntrinsicSizeTarget, LayoutBox, LayoutTree,
    PseudoBox, ReplacedContent, ReplacedContext, TextBrush,
};

/// Builds the whole box tree for `dom` (styles must already be resolved).
#[must_use]
pub fn build_layout_tree(
    dom: &DomTree,
    style_engine: &StyleEngine,
    fonts: &mut FontSystem,
    viewport: Viewport,
    images: &crate::images::ImageStore,
) -> LayoutTree {
    let mut builder = Builder {
        dom,
        style: style_engine,
        tree: LayoutTree::new(viewport),
        ifc_sources: Vec::new(),
        contains_block_memo: HashMap::new(),
        list_ordinals: HashMap::new(),
        images,
        depth: 0,
    };

    // The style engine names the rendered document this box tree belongs to,
    // so the two cannot disagree about which frame is being laid out
    // (ADR-0035 D1).
    if let Some(root) = dom.document_element_of(style_engine.document()) {
        let root_box = builder.build_box(root);
        builder.tree.set_root(root_box);
    }

    let sources = std::mem::take(&mut builder.ifc_sources);
    let mut tree = builder.tree;
    for (box_id, source) in sources {
        build_ifc(dom, &mut tree, fonts, box_id, &source);
    }
    tree
}

/// The string content and computed style of a `::before`/`::after`
/// pseudo-element (WP-J: string `content` items only).
struct PseudoText {
    style: ServoArc<ComputedValues>,
    text: String,
}

/// One recorded inline formatting context, shaped in phase 2.
enum IfcSource {
    /// The IFC spans all children of `root`, with optional pseudo-element
    /// text at the edges.
    Element {
        root: NodeId,
        root_style: ServoArc<ComputedValues>,
        /// A `list-style-position: inside` marker: the first inline content of
        /// the item, shaped in the item's own style (see [`crate::marker`]).
        marker: Option<String>,
        before: Option<PseudoText>,
        after: Option<PseudoText>,
    },
    /// An anonymous block wrapping the inline-level `items` (children of a
    /// mixed container); `root_style` is the ServoAnonymousBox style.
    Anonymous {
        root_style: ServoArc<ComputedValues>,
        items: Vec<NodeId>,
    },
    /// A standalone pseudo-element box shaping its own `content` string.
    PseudoContent {
        root_style: ServoArc<ComputedValues>,
        text: String,
    },
}

struct Builder<'a> {
    dom: &'a DomTree,
    style: &'a StyleEngine,
    tree: LayoutTree,
    ifc_sources: Vec<(BoxId, IfcSource)>,
    /// Memo for [`Self::is_or_contains_block`] (an inline with an in-flow
    /// block descendant is treated as a block child; ADR-0006 §4).
    contains_block_memo: HashMap<NodeId, bool>,
    /// Marker ordinal per list item, filled a whole list at a time (see
    /// [`Self::list_item_ordinal`]).
    list_ordinals: HashMap<NodeId, i32>,
    /// Decoded images for intrinsic sizing of `<img>` replaced boxes (WP-J).
    images: &'a crate::images::ImageStore,
    /// Current `build_box` recursion depth, capped at [`MAX_CONSTRUCT_DEPTH`].
    depth: usize,
}

/// Hard cap on box-construction recursion depth. Matches paint's
/// `MAX_PAINT_DEPTH`: a box tree deeper than this would not paint anyway, so
/// there is nothing to gain by building it and risking a stack overflow.
const MAX_CONSTRUCT_DEPTH: usize = 256;

impl Builder<'_> {
    fn primary_style(&self, node: NodeId) -> Option<ServoArc<ComputedValues>> {
        self.dom.primary_style(node)
    }

    fn display_of(&self, node: NodeId) -> Display {
        self.primary_style(node)
            .map(|s| s.clone_display())
            .unwrap_or(Display::inline())
    }

    fn attr(&self, node: NodeId, name: html5ever::LocalName) -> Option<&str> {
        self.dom
            .node(node)
            .as_element()
            .and_then(|el| el.attr(&attr_name(name)))
            .map(|v| &**v)
    }

    fn is_hidden_input(&self, node: NodeId) -> bool {
        self.dom
            .node(node)
            .as_element()
            .is_some_and(|el| el.is_html_element() && el.name.local == local_name!("input"))
            && self.attr(node, local_name!("type")) == Some("hidden")
    }

    /// Whether `node` is an in-flow block or an inline that (transitively)
    /// contains one (mirrors blitz-dom `Node::is_or_contains_block`).
    fn is_or_contains_block(&mut self, node: NodeId) -> bool {
        if let Some(&memo) = self.contains_block_memo.get(&node) {
            return memo;
        }
        let result = (|| {
            if self.dom.node(node).data().kind() != NodeKind::Element {
                return false;
            }
            let (display, position, float) = flow_triple(self.primary_style(node).as_ref());
            let in_flow = matches!(
                position,
                Position::Static | Position::Relative | Position::Sticky
            ) && float == Float::None;
            if !in_flow {
                return false;
            }
            match display.outside() {
                DisplayOutside::None => false,
                DisplayOutside::Block
                | DisplayOutside::TableCaption
                | DisplayOutside::InternalTable => true,
                DisplayOutside::Inline => {
                    display.inside() == DisplayInside::Flow && {
                        let children = self.dom.flat_tree_children(node);
                        children.iter().any(|&c| self.is_or_contains_block(c))
                    }
                }
            }
        })();
        self.contains_block_memo.insert(node, result);
        result
    }

    /// Collects `node`'s layout-participating children, flattening
    /// `display: contents` elements (their children join `node`'s formatting
    /// context directly). Comments/doctypes/PIs generate no boxes.
    fn expanded_children(&self, node: NodeId, out: &mut Vec<NodeId>) {
        // Children come from the flat tree (shadow host -> shadow root's
        // children; <slot> -> assigned nodes or fallback), so shadow trees
        // lay out in place of the host's light children.
        for child in self.dom.flat_tree_children(node) {
            match self.dom.node(child).data().kind() {
                NodeKind::Element => {
                    if self.display_of(child).inside() == DisplayInside::Contents {
                        self.expanded_children(child, out);
                    } else {
                        out.push(child);
                    }
                }
                kind if is_text_kind(kind) => out.push(child),
                _ => {}
            }
        }
    }

    /// Builds the principal box for element `node` (and its subtree).
    /// Returns `None` for `display: none` and unstyled elements.
    fn build_box(&mut self, node: NodeId) -> Option<BoxId> {
        debug_assert_eq!(self.dom.node(node).data().kind(), NodeKind::Element);
        let style = self.primary_style(node)?;
        let display = style.clone_display();
        if display.inside() == DisplayInside::None {
            return None;
        }
        if self.is_hidden_input(node) {
            return None;
        }

        // Replaced elements and leaf-sized form controls first (they never
        // lay out their DOM children; blitz-dom `collect_layout_children`).
        if let Some(replaced) = self.replaced_content_for(node) {
            let mut layout_box =
                LayoutBox::new(BoxKind::Replaced, Some(node), taffy_style_for(&style));
            capture_text_fields(&mut layout_box, &style);
            layout_box.replaced = Some(replaced);
            return Some(self.tree.push_box(layout_box));
        }

        let mut layout_box = LayoutBox::new(BoxKind::Block, Some(node), taffy_style_for(&style));
        capture_text_fields(&mut layout_box, &style);
        let box_id = self.tree.push_box(layout_box);

        // Box construction recurses once per DOM nesting level
        // (build_box → collect_*_children → build_box). A pathologically deep
        // DOM (e.g. 100k nested elements) would overflow the stack, so past the
        // cap we keep the box but stop descending — mirroring paint's
        // `MAX_PAINT_DEPTH` guard and keeping every downstream pass (taffy,
        // rounding, paint) within a bounded tree depth.
        if self.depth >= MAX_CONSTRUCT_DEPTH {
            return Some(box_id);
        }
        self.depth += 1;

        // `display: list-item` generates a marker. An *outside* marker is a box
        // of its own (built here, so it is the item's first child in every
        // container type); an *inside* marker is inline content, and is handed
        // to the child collector to place ahead of `::before`.
        let marker = self.list_marker(node, &style, display);
        let inside_marker = match marker {
            Some((MarkerPosition::Outside, text)) => {
                let marker_box = self.build_marker_box(node, &style, text, MarkerPosition::Outside);
                self.attach_child(box_id, marker_box);
                None
            }
            Some((MarkerPosition::Inside, text)) => Some(text),
            None => None,
        };

        match display.inside() {
            DisplayInside::Flex | DisplayInside::Grid => {
                self.collect_flex_grid_children(box_id, node, &style, inside_marker);
            }
            // Tables lay out as CSS grid (WP-M, blitz-dom `table.rs`).
            DisplayInside::Table => {
                self.collect_table_children(box_id, node, &style);
            }
            // Flow / FlowRoot / TableCell, `display: contents` reaching here as
            // the document root, and any table-internal or ruby inside-value
            // used as a principal box all lay out as a flow container in v1.
            // (`None` returned early above.)
            _ => {
                // A block container with a non-`auto` `column-count`/`column-width`
                // is a multi-column container: its content goes into one anonymous
                // flow box that the compute pass slices into columns (ADR-0016).
                // Table cells and the like are deliberately excluded.
                if matches!(
                    display.inside(),
                    DisplayInside::Flow | DisplayInside::FlowRoot
                ) && crate::multicol::is_multicol(&style)
                {
                    self.collect_multicol_children(box_id, node, &style, inside_marker);
                } else {
                    self.collect_flow_children(box_id, node, &style, inside_marker);
                }
            }
        }

        self.depth -= 1;
        Some(box_id)
    }

    /// The captured replaced-content data for `node`, if it is a replaced
    /// element or a leaf-sized form control.
    fn replaced_content_for(&self, node: NodeId) -> Option<ReplacedContent> {
        let el = self.dom.node(node).as_element()?;
        if !el.is_html_element() && &*el.name.local != "svg" {
            return None;
        }
        let parse_f32 = |name: html5ever::LocalName| -> Option<f32> {
            self.attr(node, name).and_then(|v| v.parse::<f32>().ok())
        };
        match &*el.name.local {
            "img" | "canvas" | "svg" => {
                // A decoded image (WP-J) supplies the intrinsic size; until it
                // loads the box is 0×0 unless attribute sizes apply.
                let decoded = self.image_data(node);
                let inherent_size = match &decoded {
                    Some(image) => taffy::Size {
                        width: image.width as f32,
                        height: image.height as f32,
                    },
                    None => taffy::Size::ZERO,
                };
                Some(ReplacedContent::Image(ReplacedContext {
                    inherent_size,
                    attr_size: taffy::Size {
                        width: parse_f32(local_name!("width")),
                        height: parse_f32(local_name!("height")),
                    },
                    data: decoded,
                }))
            }
            "textarea" => Some(ReplacedContent::TextInput {
                // Per HTML, rows/cols must be positive; invalid values fall
                // back to the defaults (rows 2), never zero/negative sizes.
                rows: parse_f32(local_name!("rows"))
                    .filter(|v| *v >= 1.0)
                    .unwrap_or(2.0),
                cols: parse_f32(local_name!("cols")).filter(|v| *v >= 1.0),
                multiline: true,
            }),
            "input" => match self.attr(node, local_name!("type")) {
                None
                | Some("text" | "password" | "email" | "number" | "search" | "tel" | "url") => {
                    Some(ReplacedContent::TextInput {
                        rows: 1.0,
                        cols: None,
                        multiline: false,
                    })
                }
                Some("checkbox" | "radio") => Some(ReplacedContent::Checkbox),
                _ => None,
            },
            _ => None,
        }
    }

    /// The decoded image for `node`'s `src`, resolved against the document base
    /// URL and looked up in the store (WP-J). An inline `<svg>` has no `src`:
    /// it is keyed by its markup and its computed `color` (which the page embeds
    /// in the source it stores, so `currentColor` resolves), and the page fills
    /// that entry in (WP-K). Both sides must compute the key identically.
    fn image_data(&self, node: NodeId) -> Option<std::sync::Arc<crate::images::DecodedImage>> {
        let src = self
            .attr(node, local_name!("src"))
            .filter(|s| !s.is_empty());
        match src {
            Some(src) => {
                let url = self.dom.url_extra_data().0.join(src).ok()?;
                self.images.get(url.as_str())
            }
            None => {
                let el = self.dom.node(node).as_element()?;
                if &*el.name.local != "svg" {
                    return None;
                }
                let style = self.primary_style(node)?;
                let markup = crate::images::inline_svg_markup(self.dom, node);
                let color = crate::images::current_color(&style);
                let vars = crate::images::svg_var_substitutions(&markup, &style);
                let key = crate::images::inline_svg_key(&markup, color, &vars);
                self.images.get(&key)
            }
        }
    }

    /// Child collection for flow containers: classify children as all-inline
    /// / all-block / mixed and construct accordingly (blitz-dom
    /// `collect_layout_children`, `DisplayInside::Flow` arm).
    fn collect_flow_children(
        &mut self,
        container_box: BoxId,
        container_node: NodeId,
        container_style: &ServoArc<ComputedValues>,
        inside_marker: Option<String>,
    ) {
        let before = self.pseudo_text(container_node, PseudoBox::Before);
        let after = self.pseudo_text(container_node, PseudoBox::After);

        let mut children = Vec::new();
        self.expanded_children(container_node, &mut children);
        if children.is_empty() && before.is_none() && after.is_none() {
            // An empty list item still shows its marker: promote the item to an
            // IFC root whose only content is the inside marker's text.
            if let Some(marker) = inside_marker {
                self.make_inline_root(
                    container_box,
                    container_node,
                    container_style.clone(),
                    Some(marker),
                    None,
                    None,
                );
            }
            return;
        }

        let contains_float = children.iter().any(|&child| {
            self.primary_style(child)
                .is_some_and(|style| style.clone_float() != Float::None)
        });
        let contains_clear = before
            .as_ref()
            .is_some_and(|pseudo| pseudo.style.clone_clear() != Clear::None)
            || after
                .as_ref()
                .is_some_and(|pseudo| pseudo.style.clone_clear() != Clear::None)
            || children.iter().any(|&child| {
                self.primary_style(child)
                    .is_some_and(|style| style.clone_clear() != Clear::None)
            });
        self.tree.box_mut(container_box).force_bfc = contains_float && contains_clear;

        let mut all_block = true;
        let mut all_inline = true;
        let mut all_out_of_flow = true;
        // An inside marker is in-flow inline content of the item.
        if inside_marker.is_some() {
            all_out_of_flow = false;
            all_block = false;
        }
        // Pseudo-element boxes participate like children with their own
        // display (they are always in flow in v1).
        for pseudo in [&before, &after].into_iter().flatten() {
            all_out_of_flow = false;
            match pseudo.style.clone_display().outside() {
                DisplayOutside::Inline => all_block = false,
                DisplayOutside::None => {}
                _ => all_inline = false,
            }
        }
        for &child in &children {
            let (display, position, float) = flow_triple(self.primary_style(child).as_ref());

            // Ignore nodes that are entirely whitespace.
            if self.dom.is_whitespace_text(child) {
                continue;
            }

            let is_in_flow = matches!(
                position,
                Position::Static | Position::Relative | Position::Sticky
            ) && float == Float::None;
            if !is_in_flow {
                continue;
            }

            all_out_of_flow = false;
            match display.outside() {
                DisplayOutside::None => {}
                DisplayOutside::Block
                | DisplayOutside::TableCaption
                | DisplayOutside::InternalTable => all_inline = false,
                DisplayOutside::Inline => {
                    all_block = false;
                    // Block-in-inline is treated as a block child (no inline
                    // splitting; ADR-0006 §4).
                    if self.is_or_contains_block(child) {
                        all_inline = false;
                    }
                }
            }
        }

        if all_out_of_flow {
            self.push_element_boxes(container_box, &children, /* skip_whitespace */ true);
            return;
        }

        if all_inline {
            self.make_inline_root(
                container_box,
                container_node,
                container_style.clone(),
                inside_marker,
                before,
                after,
            );
            return;
        }

        // Block-level or mixed content: the marker and pseudo-element content
        // each get their own box at the edges (v1: a pseudo adjacent to an
        // inline run is not merged into that run's anonymous block).
        if let Some(marker) = inside_marker {
            let marker_box = self.build_marker_box(
                container_node,
                container_style,
                marker,
                MarkerPosition::Inside,
            );
            self.attach_child(container_box, marker_box);
        }
        if let Some(before) = before {
            let pseudo_box = self.build_pseudo_box(container_node, PseudoBox::Before, before);
            self.attach_child(container_box, pseudo_box);
        }

        if all_block {
            self.push_element_boxes(container_box, &children, /* skip_whitespace */ true);
        } else {
            // Mixed content: wrap runs of inline-level children in anonymous
            // blocks.
            self.collect_complex_children(
                container_box,
                children,
                container_style,
                /* hide_whitespace */ false,
                |builder, child, child_kind, display_outside| {
                    is_text_kind(child_kind)
                        || (display_outside == DisplayOutside::Inline
                            && !builder.is_or_contains_block(child))
                },
            );
        }

        if let Some(after) = after {
            let pseudo_box = self.build_pseudo_box(container_node, PseudoBox::After, after);
            self.attach_child(container_box, pseudo_box);
        }
    }

    /// Child collection for a multi-column container (ADR-0016): the element's
    /// content is built under one anonymous *flow* box — exactly as it would
    /// have been built under the element itself, including `::before`/`::after`,
    /// anonymous wrapping of inline runs, and promotion to an IFC root when the
    /// content is all-inline. The compute pass lays that flow out once at the
    /// used column width and slices it; paint shows each slice through a clip +
    /// translate.
    ///
    /// The flow box carries none of the element's box properties (padding,
    /// border, width, the `column-*` themselves) — only its inherited text
    /// properties, so an IFC it may become shapes with the right font. `column-*`
    /// do not inherit, so the anonymous style cannot recurse into a second
    /// multicol.
    fn collect_multicol_children(
        &mut self,
        container_box: BoxId,
        container_node: NodeId,
        container_style: &ServoArc<ComputedValues>,
        inside_marker: Option<String>,
    ) {
        let anon_style = self.style.anonymous_box_style(container_style);
        let mut flow = LayoutBox::new(BoxKind::AnonymousBlock, None, taffy_style_for(&anon_style));
        flow.style.display = taffy::Display::Block;
        capture_text_fields(&mut flow, &anon_style);
        // `dom_node: None`, so `push_box` leaves `node_to_box` alone: the element
        // still maps to its multicol *root* box.
        let flow_id = self.tree.push_box(flow);
        self.attach_child(container_box, flow_id);

        self.collect_flow_children(flow_id, container_node, container_style, inside_marker);

        crate::multicol::make_multicol_root(
            &mut self.tree,
            container_box,
            flow_id,
            container_style,
        );
    }

    /// Child collection for flex/grid containers: element children become
    /// items directly; text runs get anonymous wrappers.
    fn collect_flex_grid_children(
        &mut self,
        container_box: BoxId,
        container_node: NodeId,
        container_style: &ServoArc<ComputedValues>,
        inside_marker: Option<String>,
    ) {
        let mut children = Vec::new();
        self.expanded_children(container_node, &mut children);

        // An inside marker becomes the first flex/grid item.
        if let Some(marker) = inside_marker {
            let marker_box = self.build_marker_box(
                container_node,
                container_style,
                marker,
                MarkerPosition::Inside,
            );
            self.attach_child(container_box, marker_box);
        }

        // Pseudo-element content becomes a flex/grid item at the edges.
        if let Some(before) = self.pseudo_text(container_node, PseudoBox::Before) {
            let pseudo_box = self.build_pseudo_box(container_node, PseudoBox::Before, before);
            self.attach_child(container_box, pseudo_box);
        }

        let has_text_node = children.iter().any(|&c| self.dom.node(c).is_text());
        if has_text_node {
            self.collect_complex_children(
                container_box,
                children,
                container_style,
                /* hide_whitespace */ true,
                |_, _, child_kind, _| is_text_kind(child_kind),
            );
        } else {
            self.push_element_boxes(container_box, &children, /* skip_whitespace */ true);
        }

        if let Some(after) = self.pseudo_text(container_node, PseudoBox::After) {
            let pseudo_box = self.build_pseudo_box(container_node, PseudoBox::After, after);
            self.attach_child(container_box, pseudo_box);
        }

        // CSS `order` (default 0): affects flex/grid layout and paint order
        // only — never DOM/accessibility/tab order, since only this
        // layout-tree-internal `children` list is reordered (DOM/`NodeId`
        // lookups elsewhere stay keyed by identity, not position). Taffy has
        // no `order` field itself; it expects the caller to pre-sort
        // children, which is what this does. `sort_by_key` is stable,
        // matching the spec's "same `order` → source order" tiebreak with no
        // extra field needed.
        let mut ordered: Vec<(i32, BoxId)> = self
            .tree
            .box_(container_box)
            .children
            .iter()
            .map(|&child| (self.tree.box_(child).order, child))
            .collect();
        ordered.sort_by_key(|&(order, _)| order);
        self.tree.box_mut(container_box).children =
            ordered.into_iter().map(|(_, child)| child).collect();
    }

    /// Builds boxes for `children` and attaches them to `container_box`.
    fn push_element_boxes(&mut self, container_box: BoxId, children: &[NodeId], skip_ws: bool) {
        for &child in children {
            match self.dom.node(child).data().kind() {
                NodeKind::Element => {
                    if let Some(child_box) = self.build_box(child) {
                        self.attach_child(container_box, child_box);
                    }
                }
                kind if is_text_kind(kind) => {
                    debug_assert!(
                        !skip_ws || self.dom.is_whitespace_text(child),
                        "non-whitespace text must go through anonymous wrapping"
                    );
                }
                _ => {}
            }
        }
    }

    fn attach_child(&mut self, parent: BoxId, child: BoxId) {
        self.tree.box_mut(child).parent = Some(parent);
        self.tree.box_mut(parent).children.push(child);
    }

    /// Marks `container_box` as an IFC root: finds the atomic inline boxes
    /// embedded in the inline subtree (phase 1) and records the IFC for
    /// shaping (phase 2).
    fn make_inline_root(
        &mut self,
        container_box: BoxId,
        container_node: NodeId,
        root_style: ServoArc<ComputedValues>,
        marker: Option<String>,
        before: Option<PseudoText>,
        after: Option<PseudoText>,
    ) {
        self.tree.box_mut(container_box).kind = BoxKind::InlineRoot;
        self.find_embedded_inline_boxes(container_box, container_node);
        self.ifc_sources.push((
            container_box,
            IfcSource::Element {
                root: container_node,
                root_style,
                marker,
                before,
                after,
            },
        ));
    }

    /// The marker a `display: list-item` element generates: where it goes and
    /// what it says (see [`crate::marker`]). `None` when the element is not a
    /// list item, when `list-style-type: none`, or for the container kinds whose
    /// child lists are index-sensitive (tables) — an extra box there would
    /// desync the grid translation.
    fn list_marker(
        &mut self,
        node: NodeId,
        style: &ServoArc<ComputedValues>,
        display: Display,
    ) -> Option<(MarkerPosition, String)> {
        if !display.is_list_item()
            || !matches!(
                display.inside(),
                DisplayInside::Flow
                    | DisplayInside::FlowRoot
                    | DisplayInside::Flex
                    | DisplayInside::Grid
            )
        {
            return None;
        }
        let position = MarkerPosition::of(style);
        let ordinal = self.list_item_ordinal(node);
        let text = crate::marker::marker_text(
            style,
            ordinal,
            // An inside marker is separated from the item's text by the counter
            // style's suffix space; an outside one by its placement gap.
            /* trailing_space */
            position == MarkerPosition::Inside,
        )?;
        Some((position, text))
    }

    /// A list item's ordinal: its 1-based position among the list items of its
    /// flat-tree parent, honouring `<ol start>`, `<ol reversed>` and `<li value>`
    /// (HTML "ordinal value"). Numbering is derived from the list *structure* —
    /// CSS counters (`counter-reset`/`counter-increment`) are not implemented —
    /// so a nested list restarts simply by having a different parent.
    ///
    /// The whole sibling list is numbered on the first query and memoized, so a
    /// list of *n* items costs one pass, not *n*.
    fn list_item_ordinal(&mut self, node: NodeId) -> i32 {
        if let Some(&ordinal) = self.list_ordinals.get(&node) {
            return ordinal;
        }
        let Some(parent) = self.dom.flat_tree_parent(node) else {
            return 1;
        };

        let items: Vec<NodeId> = self
            .dom
            .flat_tree_children(parent)
            .into_iter()
            .filter(|&child| {
                self.primary_style(child)
                    .is_some_and(|style| style.clone_display().is_list_item())
            })
            .collect();

        let reversed =
            self.tag_is(parent, "ol") && self.attr(parent, local_name!("reversed")).is_some();
        let start = self
            .tag_is(parent, "ol")
            .then(|| self.attr(parent, local_name!("start")))
            .flatten()
            .and_then(|value| value.trim().parse::<i32>().ok());

        let step = if reversed { -1 } else { 1 };
        // `<ol reversed>` without `start` counts down from the number of items.
        let mut counter = start.unwrap_or(if reversed {
            i32::try_from(items.len()).unwrap_or(i32::MAX)
        } else {
            1
        });

        for &item in &items {
            if self.tag_is(item, "li")
                && let Some(value) = self
                    .attr(item, local_name!("value"))
                    .and_then(|v| v.trim().parse::<i32>().ok())
            {
                counter = value;
            }
            self.list_ordinals.insert(item, counter);
            counter = counter.saturating_add(step);
        }

        self.list_ordinals.get(&node).copied().unwrap_or(1)
    }

    /// Builds the marker box of a list item. `dom_node` is the *owning* element
    /// (as for `::before`/`::after`), so geometry APIs — which look up principal
    /// boxes only — never see it, while hit-testing still resolves a point on the
    /// marker to the list item.
    ///
    /// The box is styled as an anonymous box of the item: it inherits the item's
    /// font, colour and line-height (so the marker matches the text and shares
    /// its first baseline) but none of its box properties — an item's own
    /// `padding`/`border`/`width` must not be replayed on its marker.
    fn build_marker_box(
        &mut self,
        owner: NodeId,
        owner_style: &ServoArc<ComputedValues>,
        text: String,
        position: MarkerPosition,
    ) -> BoxId {
        let style = self.style.anonymous_box_style(owner_style);
        let mut layout_box =
            LayoutBox::new(BoxKind::InlineRoot, Some(owner), taffy_style_for(&style));
        layout_box.style.display = taffy::Display::Block;
        layout_box.style.item_is_table = false;
        capture_text_fields(&mut layout_box, &style);
        // `text-indent` inherits; indenting a shrink-to-fit marker box would
        // just push the bullet off its own line.
        layout_box.text_indent = TextIndent::zero();
        layout_box.pseudo = Some(PseudoBox::Marker);
        if position == MarkerPosition::Outside {
            // Out of the item's flow in every container kind. `inset` stays
            // `auto`; `marker::place_markers` does the placement.
            layout_box.style.position = taffy::Position::Absolute;
        }
        let box_id = self.tree.push_box(layout_box);
        self.ifc_sources.push((
            box_id,
            IfcSource::PseudoContent {
                root_style: style,
                text,
            },
        ));
        box_id
    }

    /// Builds a standalone box for a `::before`/`::after` pseudo-element.
    /// `dom_node` is the *owning* element with the pseudo tag set, so
    /// geometry APIs (which look up principal boxes only) never see it.
    fn build_pseudo_box(&mut self, owner: NodeId, which: PseudoBox, pseudo: PseudoText) -> BoxId {
        let mut layout_box =
            LayoutBox::new(BoxKind::Block, Some(owner), taffy_style_for(&pseudo.style));
        // Pseudo boxes use the block/IFC path below and never receive the
        // TableRoot context used for principal `display: table` elements.
        // Advertising them as table items sends clearfix pseudos through a
        // separate BFC path in Taffy that does not apply their `clear`.
        layout_box.style.display = taffy::Display::Block;
        layout_box.style.item_is_table = false;
        capture_text_fields(&mut layout_box, &pseudo.style);
        layout_box.pseudo = Some(which);
        let box_id = self.tree.push_box(layout_box);
        if !pseudo.text.is_empty() {
            self.tree.box_mut(box_id).kind = BoxKind::InlineRoot;
            self.ifc_sources.push((
                box_id,
                IfcSource::PseudoContent {
                    root_style: pseudo.style,
                    text: pseudo.text,
                },
            ));
        }
        box_id
    }

    /// The computed style and string `content` of a pseudo-element, if the
    /// cascade produced one that generates a box (WP-J: string items only;
    /// counters/attr()/quotes are Phase 6+).
    fn pseudo_text(&self, node: NodeId, which: PseudoBox) -> Option<PseudoText> {
        let pe = match which {
            PseudoBox::Before => StyloPseudoElement::Before,
            PseudoBox::After => StyloPseudoElement::After,
            // A marker's content comes from `list-style-type`, not from the
            // `content` cascade (see [`crate::marker`]).
            PseudoBox::Marker => return None,
        };
        let style = self.dom.pseudo_style(node, &pe)?;
        if style.clone_display().inside() == DisplayInside::None {
            return None;
        }
        let Content::Items(item_data) = &style.get_counters().content else {
            return None;
        };
        let mut text = String::new();
        for item in &item_data.items[..item_data.alt_start] {
            if let ContentItem::String(s) = item {
                text.push_str(s);
            }
        }
        Some(PseudoText { style, text })
    }

    /// Wraps runs of inline-level children in anonymous blocks; other
    /// children get their own boxes (blitz-dom
    /// `collect_complex_layout_children`).
    fn collect_complex_children(
        &mut self,
        container_box: BoxId,
        children: Vec<NodeId>,
        container_style: &ServoArc<ComputedValues>,
        hide_whitespace: bool,
        needs_wrap: impl Fn(&mut Self, NodeId, NodeKind, DisplayOutside) -> bool,
    ) {
        let mut inline_run: Vec<NodeId> = Vec::new();

        for child in children {
            let child_kind = self.dom.node(child).data().kind();
            let is_whitespace = self.dom.is_whitespace_text(child);
            if hide_whitespace && is_whitespace {
                continue;
            }

            let display_outside = if child_kind == NodeKind::Element {
                self.display_of(child).outside()
            } else {
                DisplayOutside::Inline
            };
            // Taffy handles `Display::None` children itself in blitz; with a
            // separate box tree we simply generate no box for them, but they
            // must not break an open inline run.
            if child_kind == NodeKind::Element
                && self.display_of(child).inside() == DisplayInside::None
            {
                continue;
            }

            if needs_wrap(self, child, child_kind, display_outside) {
                inline_run.push(child);
            } else {
                self.flush_inline_run(container_box, &mut inline_run, container_style);
                if child_kind == NodeKind::Element
                    && let Some(child_box) = self.build_box(child)
                {
                    self.attach_child(container_box, child_box);
                }
            }
        }
        self.flush_inline_run(container_box, &mut inline_run, container_style);
    }

    /// Closes an open inline run: wraps it in an anonymous block box that is
    /// itself an IFC root (runs that are only whitespace are dropped).
    fn flush_inline_run(
        &mut self,
        container_box: BoxId,
        run: &mut Vec<NodeId>,
        container_style: &ServoArc<ComputedValues>,
    ) {
        let items = std::mem::take(run);
        if items.is_empty() || items.iter().all(|&n| self.dom.is_whitespace_text(n)) {
            return;
        }

        let anon_style = self.style.anonymous_box_style(container_style);
        let mut anon = LayoutBox::new(BoxKind::AnonymousBlock, None, taffy_style_for(&anon_style));
        capture_text_fields(&mut anon, &anon_style);
        let anon_id = self.tree.push_box(anon);
        self.attach_child(container_box, anon_id);

        for &item in &items {
            if self.dom.node(item).data().kind() == NodeKind::Element {
                self.find_embedded_inline_boxes_for_node(anon_id, item);
            }
        }
        self.ifc_sources.push((
            anon_id,
            IfcSource::Anonymous {
                root_style: anon_style,
                items,
            },
        ));
    }

    /// Walks the inline subtree under `node`, building boxes for atomic
    /// inlines (and out-of-flow elements) that participate in `ifc_box`'s
    /// inline layout as `InlineBox` placeholders (blitz-dom
    /// `find_inline_layout_embedded_boxes`).
    fn find_embedded_inline_boxes(&mut self, ifc_box: BoxId, root: NodeId) {
        let children = self.dom.flat_tree_children(root);
        for child in children {
            self.find_embedded_inline_boxes_for_node(ifc_box, child);
        }
    }

    fn find_embedded_inline_boxes_for_node(&mut self, ifc_box: BoxId, node: NodeId) {
        match self.dom.node(node).data().kind() {
            NodeKind::Element => {}
            _ => return,
        }
        if self.is_hidden_input(node) {
            return;
        }

        let display = self.display_of(node);
        match (display.outside(), display.inside()) {
            (DisplayOutside::None, DisplayInside::None) => {}
            (DisplayOutside::None, DisplayInside::Contents) => {
                self.find_embedded_inline_boxes(ifc_box, node);
            }
            (DisplayOutside::Inline, DisplayInside::Flow) => {
                if is_atomic_inline_tag(self.dom, node) {
                    if let Some(child_box) = self.build_box(node) {
                        self.attach_child(ifc_box, child_box);
                    }
                } else if self.tag_is(node, "br") {
                    // No box; contributes a preserved "\n" during shaping.
                } else {
                    self.find_embedded_inline_boxes(ifc_box, node);
                }
            }
            // Atomic inline (inline-block/-flex/-grid) or out-of-flow block.
            (_, _) => {
                if let Some(child_box) = self.build_box(node) {
                    self.attach_child(ifc_box, child_box);
                }
            }
        }
    }

    /// Builds the grid translation of a table (WP-M): collects cells into a
    /// [`TableContext`], attaching each cell's box as a child of the table
    /// root box.
    fn collect_table_children(
        &mut self,
        table_box: BoxId,
        table_node: NodeId,
        table_style: &ServoArc<ComputedValues>,
    ) {
        let mut state = crate::table::TableBuildState::new(table_style);
        let children = self.dom.flat_tree_children(table_node);
        // A run of stray children — anything that is not a table-internal box —
        // is wrapped in an anonymous cell (occupying its own anonymous row) per
        // CSS 2.1 §17.2.1, so `display: table` used as a plain container (e.g.
        // Bootstrap's `.tab-content { display: table }`, whose `.tab-pane` is a
        // block) lays its content out instead of dropping it to a 0-height box.
        let mut stray: Vec<NodeId> = Vec::new();
        for child in children {
            if self.is_table_internal(child) {
                self.flush_anonymous_cell(table_box, &mut stray, &mut state, table_style);
                self.collect_table_cells(table_box, child, &mut state);
            } else {
                stray.push(child);
            }
        }
        self.flush_anonymous_cell(table_box, &mut stray, &mut state, table_style);
        let ctx = state.finish(table_style);
        let table = self.tree.box_mut(table_box);
        table.kind = BoxKind::TableRoot;
        table.table = Some(std::rc::Rc::new(ctx));
    }

    /// True for an element whose display makes it a table-internal box that
    /// [`Self::collect_table_cells`] handles directly (row groups, rows, cells,
    /// and the transparent `display: contents`). Everything else — blocks,
    /// inline content, text — is stray content that needs an anonymous cell.
    fn is_table_internal(&mut self, node: NodeId) -> bool {
        self.primary_style(node).is_some_and(|style| {
            matches!(
                style.clone_display().inside(),
                DisplayInside::TableRowGroup
                    | DisplayInside::TableHeaderGroup
                    | DisplayInside::TableFooterGroup
                    | DisplayInside::TableRow
                    | DisplayInside::TableCell
                    | DisplayInside::Contents
            )
        })
    }

    /// Wraps a run of stray table children in an anonymous cell that occupies
    /// its own anonymous row, building their content as ordinary flow (inline
    /// runs get their own anonymous IFC blocks). Whitespace-only runs between
    /// real rows generate nothing.
    fn flush_anonymous_cell(
        &mut self,
        table_box: BoxId,
        stray: &mut Vec<NodeId>,
        state: &mut crate::table::TableBuildState,
        table_style: &ServoArc<ComputedValues>,
    ) {
        let items = std::mem::take(stray);
        if items.is_empty() || items.iter().all(|&n| self.dom.is_whitespace_text(n)) {
            return;
        }
        let anon_style = self.style.anonymous_box_style(table_style);
        let mut cell = LayoutBox::new(BoxKind::Block, None, taffy_style_for(&anon_style));
        // The anonymous cell is a block container for its flow content, whatever
        // the anonymous style's own outer display resolves to.
        cell.style.display = taffy::Display::Block;
        capture_text_fields(&mut cell, &anon_style);
        let cell_box = self.tree.push_box(cell);
        self.attach_child(table_box, cell_box);
        self.collect_complex_children(
            cell_box,
            items,
            &anon_style,
            /* hide_whitespace */ true,
            |builder, child, child_kind, display_outside| {
                is_text_kind(child_kind)
                    || (display_outside == DisplayOutside::Inline
                        && !builder.is_or_contains_block(child))
            },
        );
        state.row += 1;
        state.col = 0;
        state.push_cell(cell_box, &anon_style, 1, 1);
    }

    fn collect_table_cells(
        &mut self,
        table_box: BoxId,
        node: NodeId,
        state: &mut crate::table::TableBuildState,
    ) {
        if self.dom.node(node).data().kind() != NodeKind::Element {
            return;
        }
        let Some(style) = self.primary_style(node) else {
            return;
        };
        let display = style.clone_display();
        if display.outside() == DisplayOutside::None {
            return;
        }

        match display.inside() {
            DisplayInside::TableRowGroup
            | DisplayInside::TableHeaderGroup
            | DisplayInside::TableFooterGroup
            | DisplayInside::Contents => {
                let children = self.dom.flat_tree_children(node);
                for child in children {
                    self.collect_table_cells(table_box, child, state);
                }
            }
            DisplayInside::TableRow => {
                state.row += 1;
                state.col = 0;
                let children = self.dom.flat_tree_children(node);
                for child in children {
                    self.collect_table_cells(table_box, child, state);
                }
            }
            DisplayInside::TableCell => {
                let colspan: u16 = self
                    .attr(node, local_name!("colspan"))
                    .and_then(|val| val.parse::<u16>().ok())
                    // Browsers clamp colspan to [1, 1000].
                    .map(|v| v.clamp(1, 1000))
                    .unwrap_or(1);
                let rowspan: u16 = self
                    .attr(node, local_name!("rowspan"))
                    .and_then(|val| val.parse::<u16>().ok())
                    .map(|v| v.clamp(1, 65534))
                    .unwrap_or(1);
                if let Some(cell_box) = self.build_box(node) {
                    self.attach_child(table_box, cell_box);
                    state.push_cell(cell_box, &style, colspan, rowspan);
                }
            }
            // Captions, column groups, and stray non-table content generate
            // no boxes in v1.
            _ => {}
        }
    }

    fn tag_is(&self, node: NodeId, tag: &str) -> bool {
        self.dom
            .node(node)
            .as_element()
            .is_some_and(|el| el.is_html_element() && &*el.name.local == tag)
    }
}

/// Elements that always participate in inline layout as atomic boxes even
/// with `display: inline` (replaced elements and form controls).
fn is_atomic_inline_tag(dom: &DomTree, node: NodeId) -> bool {
    dom.node(node).as_element().is_some_and(|el| {
        matches!(
            &*el.name.local,
            "img" | "svg" | "canvas" | "input" | "textarea" | "button"
        )
    })
}

/// The `(display, position, float)` a box-generating node contributes, using
/// the CSS initial values (`inline`, `static`, `none`) when the node has no
/// primary style. Read at the three sites that classify a child as in-flow /
/// block / inline. The default position is immaterial to the in-flow test
/// (both `static` and `relative` are in-flow) but `static` is the CSS initial.
fn flow_triple(style: Option<&ServoArc<ComputedValues>>) -> (Display, Position, Float) {
    (
        style
            .map(|s| s.clone_display())
            .unwrap_or_else(Display::inline),
        style
            .map(|s| s.clone_position())
            .unwrap_or(Position::Static),
        style.map(|s| s.clone_float()).unwrap_or(Float::None),
    )
}

pub(crate) fn taffy_style_for(style: &ComputedValues) -> taffy::Style<style::Atom> {
    let mut converted = stylo_taffy::to_taffy_style(style);

    // Taffy's experimental float fitter uses a strict `remaining >= 0`
    // comparison. Percentage columns that mathematically total 100% can
    // exceed it by one or two f32 rounding bits and spuriously wrap the last
    // float. Move only floated percentage widths down by one ULP; this is far
    // below CSS subpixel precision while preserving the intended fit.
    if converted.float != taffy::Float::None {
        let width = converted.size.width;
        if width.tag() == taffy::CompactLength::PERCENT_TAG {
            let percentage = width.value();
            if percentage.is_finite() && percentage > 0.0 {
                converted.size.width =
                    taffy::Dimension::percent(f32::from_bits(percentage.to_bits() - 1));
            }
        }
    }

    let pos = style.get_position();

    // `stylo_taffy::convert::dimension()`/`max_size_dimension()` map `stretch`
    // and `-webkit-fill-available` to `Dimension::AUTO` (upstream TODO).
    // `stretch` means "fill the available space on this axis", which for a
    // used-value resolution is exactly what a `100%` size resolves against.
    let is_stretch_size =
        |val: &StyloSize| matches!(val, StyloSize::Stretch | StyloSize::WebkitFillAvailable);
    let is_stretch_max_size = |val: &StyloMaxSize| {
        matches!(
            val,
            StyloMaxSize::Stretch | StyloMaxSize::WebkitFillAvailable
        )
    };
    if is_stretch_size(&pos.width) {
        converted.size.width = taffy::Dimension::percent(1.0);
    }
    if is_stretch_size(&pos.height) {
        converted.size.height = taffy::Dimension::percent(1.0);
    }
    if is_stretch_size(&pos.min_width) {
        converted.min_size.width = taffy::Dimension::percent(1.0);
    }
    if is_stretch_size(&pos.min_height) {
        converted.min_size.height = taffy::Dimension::percent(1.0);
    }
    if is_stretch_max_size(&pos.max_width) {
        converted.max_size.width = taffy::Dimension::percent(1.0);
    }
    if is_stretch_max_size(&pos.max_height) {
        converted.max_size.height = taffy::Dimension::percent(1.0);
    }

    // `content_alignment()` hardcodes physical `left`/`right` to `START`/`END`
    // with no access to `direction`. Resolve them against the container's own
    // (already-converted) direction: `justify-content` is the flex main axis,
    // so `justify_self`/`justify_items` (grid-only) are untouched. Per
    // css-align-3 §5.2, `left`/`right` behave as `start` when the property's
    // own axis isn't parallel to the physical left/right axis — for flexbox
    // that's a column/column-reverse main axis (a row main axis is always
    // parallel, under this project's horizontal-writing-mode-only scope).
    let column_main_axis = converted.display == taffy::Display::Flex
        && matches!(
            converted.flex_direction,
            taffy::FlexDirection::Column | taffy::FlexDirection::ColumnReverse
        );
    if let Some(justify_content) = converted.justify_content {
        let rtl = converted.direction == taffy::Direction::Rtl;
        let keyword = match pos.justify_content.primary().value() {
            AlignFlags::LEFT | AlignFlags::RIGHT if column_main_axis => {
                Some(taffy::AlignContentKeyword::Start)
            }
            AlignFlags::LEFT => Some(if rtl {
                taffy::AlignContentKeyword::End
            } else {
                taffy::AlignContentKeyword::Start
            }),
            AlignFlags::RIGHT => Some(if rtl {
                taffy::AlignContentKeyword::Start
            } else {
                taffy::AlignContentKeyword::End
            }),
            _ => None,
        };
        if let Some(keyword) = keyword {
            converted.justify_content = Some(taffy::AlignContent {
                keyword,
                safety: justify_content.safety,
            });
        }
    }

    // `content_alignment()`/`item_alignment()` call `.value()`/`.primary().value()`,
    // which mask out the `safe`/`unsafe` overflow-alignment prefix before
    // conversion, even though taffy's `AlignItems`/`AlignContent` carry a real
    // `safety` field. Re-check the raw flags and restore it.
    if pos.align_items.0.contains(AlignFlags::SAFE)
        && let Some(align_items) = converted.align_items.as_mut()
    {
        align_items.safety = taffy::AlignmentSafety::Safe;
    }
    if pos.align_self.0.contains(AlignFlags::SAFE)
        && let Some(align_self) = converted.align_self.as_mut()
    {
        align_self.safety = taffy::AlignmentSafety::Safe;
    }
    if pos.align_content.primary().contains(AlignFlags::SAFE)
        && let Some(align_content) = converted.align_content.as_mut()
    {
        align_content.safety = taffy::AlignmentSafety::Safe;
    }
    if pos.justify_content.primary().contains(AlignFlags::SAFE)
        && let Some(justify_content) = converted.justify_content.as_mut()
    {
        justify_content.safety = taffy::AlignmentSafety::Safe;
    }

    converted
}

/// Captures the text-related computed values the compute phase needs.
pub(crate) fn capture_text_fields(layout_box: &mut LayoutBox, style: &ComputedValues) {
    let font = style.get_font();
    let font_size = font.font_size.used_size.0.px();
    let line_height = match font.line_height {
        LineHeight::Normal => font_size * text::NORMAL_LINE_HEIGHT,
        LineHeight::Number(num) => font_size * num.0,
        LineHeight::Length(value) => value.0.px(),
    };
    layout_box.font_size = font_size;
    layout_box.line_height = line_height;
    layout_box.text_align = text::text_align(style);
    layout_box.text_indent = style.clone_text_indent();
    layout_box.position = style.clone_position();
    layout_box.z_index = style.clone_z_index().integer_or(0);
    layout_box.pointer_events_none =
        style.get_inherited_ui().clone_pointer_events() == PointerEvents::None;
    layout_box.has_transform = crate::transform::has_transform(style);
    layout_box.order = style.clone_order();
    layout_box.intrinsic_size_keywords = intrinsic_size_keywords_for(style);
}

/// `stylo_taffy::convert::dimension()`/`max_size_dimension()`/`flex_basis()`
/// collapse `min-content`/`max-content` to `Dimension::AUTO` (upstream TODO —
/// see `taffy_style_for`'s stretch/`-webkit-fill-available` fix for the same
/// seam). Recorded here, resolved by `intrinsic_size::resolve_intrinsic_size_keywords`.
fn intrinsic_size_keywords_for(
    style: &ComputedValues,
) -> smallvec::SmallVec<[(IntrinsicSizeTarget, IntrinsicSizeKeyword); 2]> {
    let pos = style.get_position();
    let mut keywords = smallvec::SmallVec::new();
    for (target, size) in [
        (IntrinsicSizeTarget::Width, &pos.width),
        (IntrinsicSizeTarget::Height, &pos.height),
        (IntrinsicSizeTarget::MinWidth, &pos.min_width),
        (IntrinsicSizeTarget::MinHeight, &pos.min_height),
    ] {
        match size {
            StyloSize::MinContent => keywords.push((target, IntrinsicSizeKeyword::MinContent)),
            StyloSize::MaxContent => keywords.push((target, IntrinsicSizeKeyword::MaxContent)),
            _ => {}
        }
    }
    for (target, size) in [
        (IntrinsicSizeTarget::MaxWidth, &pos.max_width),
        (IntrinsicSizeTarget::MaxHeight, &pos.max_height),
    ] {
        match size {
            StyloMaxSize::MinContent => keywords.push((target, IntrinsicSizeKeyword::MinContent)),
            StyloMaxSize::MaxContent => keywords.push((target, IntrinsicSizeKeyword::MaxContent)),
            _ => {}
        }
    }
    if let FlexBasis::Size(size) = &pos.flex_basis {
        match size {
            StyloSize::MinContent => keywords.push((
                IntrinsicSizeTarget::FlexBasis,
                IntrinsicSizeKeyword::MinContent,
            )),
            StyloSize::MaxContent => keywords.push((
                IntrinsicSizeTarget::FlexBasis,
                IntrinsicSizeKeyword::MaxContent,
            )),
            _ => {}
        }
    }
    keywords
}

// === Phase 2: parley shaping ===

/// Shapes one inline formatting context into `box_id`'s [`IfcData`]
/// (blitz-dom `build_inline_layout_into`). Runs at scale 1.0 — all values
/// are CSS px (ADR-0006).
fn build_ifc(
    dom: &DomTree,
    tree: &mut LayoutTree,
    fonts: &mut FontSystem,
    box_id: BoxId,
    source: &IfcSource,
) {
    let (root_style, root_brush) = match source {
        IfcSource::Element {
            root, root_style, ..
        } => (root_style, TextBrush::from_node(*root)),
        IfcSource::Anonymous { root_style, .. } | IfcSource::PseudoContent { root_style, .. } => {
            (root_style, TextBrush::default())
        }
    };

    // Map from DOM node to the atomic inline boxes built in phase 1. A pseudo
    // box (a marker) carries its *owner*'s node, so it must not be mistaken for
    // that element's atomic inline box.
    let boxes_by_node: HashMap<NodeId, BoxId> = tree
        .box_(box_id)
        .children
        .iter()
        .filter(|&&child| tree.box_(child).pseudo.is_none())
        .filter_map(|&child| tree.box_(child).dom_node.map(|n| (n, child)))
        .collect();

    let parley_style = text::style(root_brush, root_style);
    let root_line_height =
        text::resolve_line_height(parley_style.line_height, parley_style.font_size);
    let collapse_mode =
        text::white_space_collapse(root_style.get_inherited_text().white_space_collapse);
    let text_transform = root_style.clone_text_transform() & TextTransform::CASE_TRANSFORMS;

    let mut contributors: Vec<NodeId> = Vec::new();
    let (layout, ifc_text) = fonts.with_contexts(|font_cx, layout_cx| {
        let mut builder = layout_cx.tree_builder(font_cx, 1.0, true, &parley_style);
        builder.set_white_space_mode(collapse_mode);

        let walk = |builder: &mut TreeBuilder<'_, TextBrush>, contributors: &mut Vec<NodeId>| {
            let mut cx = IfcWalker {
                dom,
                boxes_by_node: &boxes_by_node,
                contributors,
                root_line_height,
            };
            match source {
                IfcSource::Element {
                    root,
                    marker,
                    before,
                    after,
                    ..
                } => {
                    // The inside marker leads the item's content, ahead of
                    // `::before`. It shapes in the item's own (root) style — and
                    // so carries the item's brush, which is exactly right: a hit
                    // on the bullet resolves to the item, and the bullet paints
                    // in the item's colour.
                    if let Some(marker) = marker {
                        builder.push_text(marker);
                    }
                    if let Some(pseudo) = before {
                        push_pseudo_span(builder, pseudo, root_line_height, collapse_mode);
                    }
                    for child in dom.flat_tree_children(*root) {
                        cx.walk(builder, child, collapse_mode, text_transform);
                    }
                    if let Some(pseudo) = after {
                        push_pseudo_span(builder, pseudo, root_line_height, collapse_mode);
                    }
                }
                IfcSource::Anonymous { items, .. } => {
                    for &item in items {
                        cx.walk(builder, item, collapse_mode, text_transform);
                    }
                }
                IfcSource::PseudoContent { text, .. } => {
                    builder.push_text(text);
                }
            }
        };
        walk(&mut builder, &mut contributors);

        let mut layout = parley::Layout::default();
        let ifc_text = builder.build_into(&mut layout);
        (layout, ifc_text)
    });

    tree.box_mut(box_id).ifc = Some(Box::new(IfcData {
        layout,
        text: ifc_text,
        contributors,
    }));
}

/// `text-transform: capitalize`: uppercases the first letter unit of each
/// word (v1: a word starts after any non-alphanumeric character; words split
/// across text nodes are not tracked).
fn capitalize_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut at_word_start = true;
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            if at_word_start {
                out.extend(ch.to_uppercase());
            } else {
                out.push(ch);
            }
            at_word_start = false;
        } else {
            at_word_start = true;
            out.push(ch);
        }
    }
    out
}

/// Pushes a `::before`/`::after` text span into the IFC being built. The
/// brush stays `None` (default), so geometry queries never attribute these
/// runs to a DOM node.
fn push_pseudo_span(
    builder: &mut TreeBuilder<'_, TextBrush>,
    pseudo: &PseudoText,
    root_line_height: f32,
    collapse_mode: WhiteSpaceCollapse,
) {
    if pseudo.text.is_empty() {
        return;
    }
    let mut span_style = text::style(TextBrush::default(), &pseudo.style);
    let font_size = span_style.font_size;
    span_style.line_height = text::parley::LineHeight::Absolute(
        text::resolve_line_height(span_style.line_height, font_size).max(root_line_height),
    );
    builder.push_style_span(span_style);
    builder.push_text(&pseudo.text);
    builder.pop_style_span();
    builder.set_white_space_mode(collapse_mode);
}

struct IfcWalker<'a> {
    dom: &'a DomTree,
    boxes_by_node: &'a HashMap<NodeId, BoxId>,
    contributors: &'a mut Vec<NodeId>,
    root_line_height: f32,
}

impl IfcWalker<'_> {
    fn walk(
        &mut self,
        builder: &mut TreeBuilder<'_, TextBrush>,
        node: NodeId,
        parent_collapse: WhiteSpaceCollapse,
        parent_text_transform: TextTransform,
    ) {
        match self.dom.node(node).data() {
            // A CDATASection is a Text node; it can reach the page's box tree
            // by being adopted out of an XML document.
            NodeData::Text(content) | NodeData::CdataSection(content) => {
                // TODO: optimize case transforms to be non-allocating
                match parent_text_transform {
                    TextTransform::UPPERCASE => builder.push_text(&content.to_uppercase()),
                    TextTransform::LOWERCASE => builder.push_text(&content.to_lowercase()),
                    TextTransform::CAPITALIZE => builder.push_text(&capitalize_text(content)),
                    _ => builder.push_text(content),
                }
            }
            NodeData::Element(el) => {
                if el.is_html_element()
                    && el.name.local == local_name!("input")
                    && el.attr(&attr_name(local_name!("type"))).map(|v| &**v) == Some("hidden")
                {
                    return;
                }

                let style = self.dom.primary_style(node);
                let collapse_mode = style
                    .as_ref()
                    .map(|s| text::white_space_collapse(s.clone_white_space_collapse()))
                    .unwrap_or(parent_collapse);
                builder.set_white_space_mode(collapse_mode);
                let text_transform = style
                    .as_ref()
                    .map(|s| s.clone_text_transform() & TextTransform::CASE_TRANSFORMS)
                    .unwrap_or(TextTransform::NONE);

                let (display, position, float) = flow_triple(style.as_ref());
                let box_kind = if position.is_absolutely_positioned() {
                    InlineBoxKind::OutOfFlow
                } else if float != Float::None {
                    InlineBoxKind::CustomOutOfFlow
                } else {
                    InlineBoxKind::InFlow
                };

                match (display.outside(), display.inside()) {
                    (DisplayOutside::None, DisplayInside::None) => {}
                    (DisplayOutside::None, DisplayInside::Contents) => {
                        let children = self.dom.flat_tree_children(node);
                        for child in children {
                            self.walk(builder, child, collapse_mode, text_transform);
                        }
                    }
                    (DisplayOutside::Inline, DisplayInside::Flow)
                        if !is_atomic_inline_tag(self.dom, node) =>
                    {
                        if el.is_html_element() && el.name.local == local_name!("br") {
                            builder.push_style_modification_span(&[]);
                            builder.set_white_space_mode(WhiteSpaceCollapse::Preserve);
                            builder.push_text("\n");
                            builder.pop_style_span();
                            builder.set_white_space_mode(collapse_mode);
                        } else {
                            let mut span_style = style
                                .as_ref()
                                .map(|s| text::style(TextBrush::from_node(node), s))
                                .unwrap_or_default();

                            // Floor the span's line-height by the inline
                            // context's (CSS 2.1 §10.8.1).
                            let font_size = span_style.font_size;
                            span_style.line_height = text::parley::LineHeight::Absolute(
                                text::resolve_line_height(span_style.line_height, font_size)
                                    .max(self.root_line_height),
                            );

                            builder.push_style_span(span_style);
                            self.contributors.push(node);

                            let children = self.dom.flat_tree_children(node);
                            for child in children {
                                self.walk(builder, child, collapse_mode, text_transform);
                            }

                            // Parley trims trailing whitespace at every
                            // style-span boundary (`pop_style_span` commits the
                            // span's text with `is_span_last = true`, which runs
                            // `trim_end`). CSS instead collapses an inline
                            // element's trailing whitespace with the following
                            // in-flow content, so flush the pending text through
                            // a throwaway span first — that commit uses
                            // `is_span_last = false` and keeps a single trailing
                            // space. Parley still excludes real line-end
                            // whitespace from line metrics, so nothing shifts at
                            // an actual end of line.
                            builder.push_style_modification_span(&[]);
                            builder.pop_style_span();

                            builder.pop_style_span();
                        }
                    }
                    // Atomic inline boxes (replaced, inline-block, …) and
                    // out-of-flow elements: placeholder referencing the box
                    // built in phase 1.
                    (_, _) => {
                        if let Some(&child_box) = self.boxes_by_node.get(&node) {
                            builder.push_inline_box(InlineBox {
                                id: u64::from(child_box.0),
                                kind: box_kind,
                                // Overridden by push_inline_box
                                index: 0,
                                // Width and height are set during layout
                                width: 0.0,
                                height: 0.0,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
