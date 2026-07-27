//! The box tree: a flat arena of [`LayoutBox`]es owned by [`LayoutTree`],
//! separate from the DOM (ADR-0006). Anonymous boxes exist only here;
//! `NodeId ↔ BoxId` is a side map.

use std::collections::HashMap;
use std::num::NonZeroU32;

use oxidepage_base::NodeId;
use oxidepage_base::geometry::Rect;
use oxidepage_style::Viewport;
use smallvec::SmallVec;
use style::Atom;
use style::values::computed::TextIndent;

/// Which `taffy::Style` field a `min-content`/`max-content` keyword was
/// mapped from (`stylo_taffy::convert` collapses all of these to `AUTO`; see
/// `crate::intrinsic_size`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntrinsicSizeTarget {
    Width,
    Height,
    MinWidth,
    MinHeight,
    MaxWidth,
    MaxHeight,
    /// Axis (width vs height) depends on the containing flex container's
    /// `flex-direction`, resolved at intrinsic-size-pass time.
    FlexBasis,
}

/// A size keyword this project resolves to a concrete pixel size before
/// taffy's real layout pass runs. `fit-content`/`fit-content(<length>)` is
/// intentionally excluded — it depends on the containing block's available
/// space, which isn't known until layout runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntrinsicSizeKeyword {
    MinContent,
    MaxContent,
}

/// Index of a box in the [`LayoutTree`] arena. The tree is rebuilt as a whole,
/// so ids are plain indices without generations.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BoxId(pub(crate) u32);

impl BoxId {
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl From<BoxId> for taffy::NodeId {
    fn from(id: BoxId) -> Self {
        taffy::NodeId::from(id.0 as u64)
    }
}

impl From<taffy::NodeId> for BoxId {
    fn from(id: taffy::NodeId) -> Self {
        BoxId(u64::from(id) as u32)
    }
}

/// Parley brush carrying the [`NodeId`] of the DOM node that owns a text
/// span, packed into a `u64` (`0` = no node, valid because a `NodeId`
/// generation is non-zero).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct TextBrush {
    packed: u64,
}

impl TextBrush {
    #[must_use]
    pub fn from_node(node: NodeId) -> Self {
        let packed = (u64::from(node.generation().get()) << 32) | u64::from(node.index());
        Self { packed }
    }

    #[must_use]
    pub fn node(self) -> Option<NodeId> {
        let generation = NonZeroU32::new((self.packed >> 32) as u32)?;
        Some(NodeId::from_parts(self.packed as u32, generation))
    }
}

/// What kind of box this is (drives the compute dispatcher).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoxKind {
    /// A block/flex/grid container (final dispatch is on `style.display`).
    Block,
    /// Root of an inline formatting context (all children participate in one
    /// parley layout stored in [`LayoutBox::ifc`]).
    InlineRoot,
    /// An anonymous block wrapping inline content in a mixed container. Has
    /// no DOM node.
    AnonymousBlock,
    /// A replaced element or leaf-sized form control
    /// (see [`LayoutBox::replaced`]).
    Replaced,
    /// A `display: table` root laid out as CSS grid (WP-M).
    TableRoot,
    /// A multi-column container (CSS Multicol, ADR-0016). Owns exactly one
    /// child — an anonymous *flow* box holding all of the element's content —
    /// which the compute pass slices into columns (see [`crate::multicol`]).
    MulticolRoot,
}

/// A `::before` / `::after` / list-marker tag on a box whose `dom_node` is the
/// owning element (geometry APIs must skip pseudo boxes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PseudoBox {
    Before,
    After,
    /// The bullet/number box of a `display: list-item` element (see
    /// [`crate::marker`]). Only `list-style-position: outside` markers get a box
    /// of their own; an `inside` marker is inline content of the item.
    Marker,
}

/// Intrinsic + attribute sizes for a replaced element (adapted from blitz-dom
/// `layout/replaced.rs`).
#[derive(Debug, Clone)]
pub struct ReplacedContext {
    /// The content's natural size (0×0 until an image decodes).
    pub inherent_size: taffy::Size<f32>,
    /// `width`/`height` element attributes, if present and parseable.
    pub attr_size: taffy::Size<Option<f32>>,
    /// The decoded image backing this box, once loaded (Phase 6, WP-J).
    pub data: Option<std::sync::Arc<crate::images::DecodedImage>>,
}

/// Captured sizing input for [`BoxKind::Replaced`] boxes.
#[derive(Debug, Clone)]
pub enum ReplacedContent {
    /// `<img>`/`<canvas>`/`<svg>`: sized by the replaced-element algorithm.
    Image(ReplacedContext),
    /// Single/multi-line text controls, sized by the simplified leaf rules
    /// (blitz-dom `layout/mod.rs`).
    TextInput {
        rows: f32,
        cols: Option<f32>,
        multiline: bool,
    },
    /// `<input type=checkbox|radio>`: square, min of styled width/height.
    Checkbox,
}

/// A parley inline layout plus the text it was shaped from.
pub struct IfcData {
    pub layout: parley::Layout<TextBrush>,
    /// The collapsed text content the layout was built from.
    pub text: String,
    /// DOM nodes contributing text runs or style spans to this IFC (used by
    /// geometry queries to find per-line rects for inline elements).
    pub contributors: Vec<NodeId>,
}

impl std::fmt::Debug for IfcData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IfcData")
            .field("text", &self.text)
            .field("contributors", &self.contributors)
            .finish_non_exhaustive()
    }
}

/// One box in the layout tree. Everything the taffy/parley compute passes
/// need is captured here at construction time.
pub struct LayoutBox {
    pub kind: BoxKind,
    /// The DOM node this box was generated for (`None` for anonymous blocks;
    /// the owning element for pseudo boxes).
    pub dom_node: Option<NodeId>,
    /// Set when this box is a `::before`/`::after` pseudo-element box.
    pub pseudo: Option<PseudoBox>,
    pub parent: Option<BoxId>,
    pub children: Vec<BoxId>,
    /// For an out-of-flow box that was re-parented onto its containing block
    /// (see `positioning`): the parent it was built under, whose content-box
    /// origin is its static position.
    pub static_parent: Option<BoxId>,
    /// The out-of-flow boxes that were built under this one and hoisted away.
    /// Layout follows `children` (the containing-block tree); paint order and
    /// stacking follow the DOM, so the painter walks these here — where CSS
    /// says they stack — while taking their geometry from the layout tree.
    pub hoisted_children: Vec<BoxId>,

    /// Taffy style converted from the stylo computed values at build time.
    pub style: taffy::Style<Atom>,
    /// Layout-only BFC isolation for containers that use an in-tree
    /// `clear` box to contain their own floats (the classic clearfix).
    /// The computed overflow remains unchanged for paint and geometry.
    pub force_bfc: bool,
    /// Computed `position` (taffy's style collapses static/relative/sticky,
    /// but `offsetParent` and hit-testing need the real value).
    pub position: style::computed_values::position::T,
    /// Computed `z-index` (integer part; `auto` → 0) for approximate paint
    /// order in hit-testing.
    pub z_index: i32,
    /// Computed `pointer-events: none`, which makes the box transparent to hit
    /// testing: the point falls through to whatever is behind it. Overlays,
    /// decorative gradients and "click-through" scrims all rely on it, and
    /// without it a great many real pages have an unclickable body.
    pub pointer_events_none: bool,
    /// Whether any of `transform`/`translate`/`rotate`/`scale` is set
    /// ([`crate::transform::has_transform`]). Captured at construction because
    /// two passes need the answer before the matrix exists: `positioning` runs
    /// before layout (a transformed box is a containing block for absolute
    /// *and* fixed descendants), and the post-layout resolve pass uses it to
    /// skip the overwhelming majority of boxes.
    pub has_transform: bool,
    /// The box's resolved transform in its **own** coordinate space (border-box
    /// top-left at the origin, `transform-origin` baked in), filled by
    /// [`crate::transform::resolve_transforms`] after rounding. `None` for an
    /// untransformed box and for a list that resolves to the identity.
    ///
    /// Geometry and hit-testing read it here because they have no access to
    /// computed styles; paint resolves the same function against the absolute
    /// border box. The two agree by [`oxidepage_base::Transform2D::at_origin`].
    pub transform: Option<oxidepage_base::Transform2D>,
    /// Computed `order` (initial 0). Taffy has no `order` field — it expects
    /// the caller to pre-sort `children` by it, which
    /// `construct::collect_flex_grid_children` does for flex/grid containers
    /// only (DOM/tab order is unaffected: only this layout-tree-internal
    /// list is reordered).
    pub order: i32,
    /// `taffy::Style` fields whose raw stylo value was a `min-content`/
    /// `max-content` keyword — resolved to a concrete pixel `Dimension` by
    /// `intrinsic_size::resolve_intrinsic_size_keywords` before the real
    /// layout pass. Empty for the overwhelming majority of boxes.
    pub intrinsic_size_keywords: SmallVec<[(IntrinsicSizeTarget, IntrinsicSizeKeyword); 2]>,
    /// `text-align`, captured for the IFC alignment pass.
    pub text_align: parley::layout::Alignment,
    /// `text-indent`, captured (resolved against the container width during
    /// compute).
    pub text_indent: TextIndent,
    /// Used `font-size` in CSS px.
    pub font_size: f32,
    /// Resolved `line-height` in CSS px.
    pub line_height: f32,

    /// Present iff `kind == Replaced`.
    pub replaced: Option<ReplacedContent>,
    /// Present iff `kind == InlineRoot`.
    pub ifc: Option<Box<IfcData>>,
    /// Present iff `kind == TableRoot` (grid translation of the table).
    pub table: Option<std::rc::Rc<crate::table::TableContext>>,
    /// Present iff `kind == MulticolRoot`: the captured `column-*` inputs plus
    /// the per-column flow slices the compute pass derives from them. Boxed —
    /// only a handful of boxes in a document are multicol roots.
    pub multicol: Option<Box<crate::multicol::MulticolContext>>,

    /// Taffy measurement cache.
    pub cache: taffy::Cache,
    /// Layout output before whole-pixel rounding.
    pub unrounded_layout: taffy::Layout,
    /// Visual correction applied after taffy's block pass for float-relative
    /// insets and absolute auto margins. Retaining the offset lets the next
    /// cached reflow remove it before recomputing, so corrections are
    /// idempotent.
    pub post_layout_offset: taffy::Point<f32>,
    /// Final (rounded) layout: size + location relative to the parent box.
    pub final_layout: taffy::Layout,
    /// Scrollable-overflow rectangle in this box's own coordinate space
    /// (filled by the post-layout overflow pass).
    pub scrollable_overflow: Rect,
}

impl LayoutBox {
    #[must_use]
    pub fn new(kind: BoxKind, dom_node: Option<NodeId>, style: taffy::Style<Atom>) -> Self {
        Self {
            kind,
            dom_node,
            pseudo: None,
            parent: None,
            children: Vec::new(),
            static_parent: None,
            hoisted_children: Vec::new(),
            style,
            force_bfc: false,
            position: style::computed_values::position::T::Static,
            z_index: 0,
            pointer_events_none: false,
            has_transform: false,
            transform: None,
            order: 0,
            intrinsic_size_keywords: SmallVec::new(),
            text_align: parley::layout::Alignment::Start,
            text_indent: TextIndent::zero(),
            font_size: 16.0,
            line_height: 16.0 * crate::text::NORMAL_LINE_HEIGHT,
            replaced: None,
            ifc: None,
            table: None,
            multicol: None,
            cache: taffy::Cache::new(),
            unrounded_layout: taffy::Layout::new(),
            post_layout_offset: taffy::Point::ZERO,
            final_layout: taffy::Layout::new(),
            scrollable_overflow: Rect::default(),
        }
    }

    /// Whether this is the marker box of a `list-style-position: outside` list
    /// item, i.e. a box that sits *outside* its parent's principal box and is
    /// placed by [`crate::marker::place_markers`] rather than by taffy.
    ///
    /// Construction is what makes the pair authoritative: an outside marker is
    /// the only box that is tagged [`PseudoBox::Marker`] *and* forced to taffy
    /// `position: absolute` (an `inside` marker is inline content, and gets a
    /// static box only when the item's content is block-level).
    #[must_use]
    pub fn is_outside_marker(&self) -> bool {
        self.pseudo == Some(PseudoBox::Marker) && self.style.position == taffy::Position::Absolute
    }
}

/// The box tree for one document layout.
pub struct LayoutTree {
    pub(crate) boxes: Vec<LayoutBox>,
    pub(crate) root: Option<BoxId>,
    pub(crate) node_to_box: HashMap<NodeId, BoxId>,
    pub(crate) viewport: Viewport,
}

impl LayoutTree {
    #[must_use]
    pub fn new(viewport: Viewport) -> Self {
        Self {
            boxes: Vec::new(),
            root: None,
            node_to_box: HashMap::new(),
            viewport,
        }
    }

    /// Drops all boxes (start of a full rebuild).
    pub fn clear(&mut self) {
        self.boxes.clear();
        self.root = None;
        self.node_to_box.clear();
    }

    /// Adds `layout_box` to the arena, recording the `NodeId → BoxId` mapping
    /// for principal (non-pseudo) boxes of DOM nodes.
    pub fn push_box(&mut self, layout_box: LayoutBox) -> BoxId {
        let id = BoxId(u32::try_from(self.boxes.len()).expect("box tree exceeds u32 indices"));
        if layout_box.pseudo.is_none()
            && let Some(node) = layout_box.dom_node
        {
            self.node_to_box.insert(node, id);
        }
        self.boxes.push(layout_box);
        id
    }

    #[must_use]
    pub fn root(&self) -> Option<BoxId> {
        self.root
    }

    pub fn set_root(&mut self, root: Option<BoxId>) {
        self.root = root;
    }

    #[must_use]
    pub fn box_(&self, id: BoxId) -> &LayoutBox {
        &self.boxes[id.index()]
    }

    pub fn box_mut(&mut self, id: BoxId) -> &mut LayoutBox {
        &mut self.boxes[id.index()]
    }

    /// The principal box generated for `node`, if any.
    #[must_use]
    pub fn box_for_node(&self, node: NodeId) -> Option<BoxId> {
        self.node_to_box.get(&node).copied()
    }

    /// The multi-column container whose continuous flow `id` holds, if `id` is
    /// a multicol *flow* box. The flow is anonymous and carries no kind of its
    /// own (it is an `AnonymousBlock`, or an `InlineRoot` once promoted), so it
    /// is identified structurally: it is the box its parent's
    /// [`crate::multicol::MulticolContext`] points at.
    #[must_use]
    pub fn multicol_root_of_flow(&self, id: BoxId) -> Option<BoxId> {
        let parent = self.box_(id).parent?;
        let mc = self.box_(parent).multicol.as_deref()?;
        (mc.flow() == id).then_some(parent)
    }

    #[must_use]
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.viewport = viewport;
    }

    #[must_use]
    pub fn box_count(&self) -> usize {
        self.boxes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_brush_roundtrip() {
        let node = NodeId::from_parts(7, NonZeroU32::new(42).unwrap());
        let brush = TextBrush::from_node(node);
        assert_eq!(brush.node(), Some(node));

        assert_eq!(TextBrush::default().node(), None);
    }

    #[test]
    fn box_id_taffy_roundtrip() {
        let id = BoxId(123);
        let taffy_id: taffy::NodeId = id.into();
        assert_eq!(BoxId::from(taffy_id), id);
    }
}
