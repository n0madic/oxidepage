//! Inline-content painting: glyph runs, text color, and decorations
//! (ADR-0007 D1/D3). Parley coordinates are relative to the IFC content box,
//! so glyph positions are shifted by the box's border + padding, matching
//! `layout::geometry`.

use oxidepage_base::{Point, Rect};
use oxidepage_layout::{BoxId, LayoutEngine, TextBrush};
use servo_arc::Arc as ServoArc;
use style::computed_values::position::T as Position;
use style::properties::ComputedValues;
use style::values::specified::text::TextDecorationLine;

use crate::builder::PaintBuilder;
use crate::convert;
use crate::display_list::{
    BorderRadii, Brush, Color, DisplayItem, FontId, FontResource, PositionedGlyph,
};

/// Converts a parley F2Dot14 normalized coordinate to a float in `[-1, 1]`.
fn f2dot14_to_f32(v: i16) -> f32 {
    f32::from(v) / 16384.0
}

/// Which slice of an IFC to paint. CSS 2.1 Appendix E paints in-flow inline
/// content (step 5) *before* positioned descendants (steps 6–8), and a
/// `position: relative` inline element is a positioned descendant — its text
/// therefore sits above an absolutely positioned sibling, not under it. Sites
/// rely on this: mgid.com underlines its section headings with an absolutely
/// positioned `::before` bar and keeps the words readable by wrapping them in a
/// relative `<span>`.
///
/// The IFC is one parley layout, so the two slices are separated by the owning
/// node of each glyph run rather than by box.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InlinePhase {
    /// Text (and atomic inline boxes) not inside a positioned inline element.
    InFlow,
    /// Text inside a positioned inline element.
    Positioned,
}

/// Paints the inline formatting context of `box_id`, if any. Emits a
/// [`DisplayItem::GlyphRun`] per shaped run plus underline/strikethrough
/// fills, and recurses into atomic inline boxes at their IFC positions.
pub(crate) fn paint_ifc(
    builder: &mut PaintBuilder<'_>,
    box_id: BoxId,
    abs_origin: Point,
    fallback_style: Option<&ComputedValues>,
    phase: InlinePhase,
) {
    // `engine`/`dom` borrow the underlying objects for the builder's whole
    // life, so they can be read while `builder` is mutated below.
    let engine = builder.engine();
    let dom = builder.dom();
    let tree = engine.tree();
    let b = tree.box_(box_id);
    let Some(ifc) = b.ifc.as_ref() else {
        return;
    };

    let content_origin = Point::new(
        abs_origin.x + b.final_layout.border.left + b.final_layout.padding.left,
        abs_origin.y + b.final_layout.border.top + b.final_layout.padding.top,
    );

    let ifc_node = ifc_element(tree, box_id);
    let mut positioned_owner = PositionedOwnerCache::default();

    for line in ifc.layout.lines() {
        for item in line.items() {
            match item {
                parley::PositionedLayoutItem::GlyphRun(glyph_run) => {
                    let owner = glyph_run.style().brush.node();
                    let in_positioned_inline = owner.is_some_and(|node| {
                        positioned_owner.is_inside_positioned_inline(dom, node, ifc_node)
                    });
                    let run_phase = if in_positioned_inline {
                        InlinePhase::Positioned
                    } else {
                        InlinePhase::InFlow
                    };
                    if run_phase != phase {
                        continue;
                    }
                    paint_glyph_run(
                        builder,
                        dom,
                        &ifc.text,
                        content_origin,
                        &glyph_run,
                        fallback_style,
                    );
                }
                parley::PositionedLayoutItem::InlineBox(ibox) => {
                    // An atomic inline is a box of its own carrying its
                    // `position` into the stacking order. It paints in the
                    // phase of its innermost positioned-inline ancestor
                    // (self-inclusive, like a glyph run's owner): an <img>
                    // inside a `position: relative` span is a positioned
                    // descendant and must stack with that span's text, not
                    // with the in-flow content (CSS 2.1 Appendix E).
                    let child = BoxId::from(taffy::NodeId::from(ibox.id));
                    let owner = tree.box_(child).dom_node;
                    let in_positioned_inline = owner.is_some_and(|node| {
                        positioned_owner.is_inside_positioned_inline(dom, node, ifc_node)
                    });
                    let box_phase = if in_positioned_inline {
                        InlinePhase::Positioned
                    } else {
                        InlinePhase::InFlow
                    };
                    if box_phase != phase {
                        continue;
                    }
                    builder.paint_inline_box(child);
                }
            }
        }
    }
}

/// Whether `box_id`'s own inline formatting context emits any content in the
/// [`InlinePhase::Positioned`] slice — a glyph run or atomic inline box whose
/// owner sits inside a positioned inline element. The paint builder uses this
/// to skip the positioned-inline paint pass over subtrees that have none (the
/// common case), instead of re-walking every in-flow IFC for nothing.
pub(crate) fn ifc_has_positioned_inline(
    engine: &LayoutEngine,
    dom: &oxidepage_dom::DomTree,
    box_id: BoxId,
) -> bool {
    let tree = engine.tree();
    let b = tree.box_(box_id);
    let Some(ifc) = b.ifc.as_ref() else {
        return false;
    };
    let ifc_node = ifc_element(tree, box_id);
    let mut positioned_owner = PositionedOwnerCache::default();
    for line in ifc.layout.lines() {
        for item in line.items() {
            let owner = match item {
                parley::PositionedLayoutItem::GlyphRun(run) => run.style().brush.node(),
                parley::PositionedLayoutItem::InlineBox(ibox) => {
                    tree.box_(BoxId::from(taffy::NodeId::from(ibox.id)))
                        .dom_node
                }
            };
            if owner.is_some_and(|node| {
                positioned_owner.is_inside_positioned_inline(dom, node, ifc_node)
            }) {
                return true;
            }
        }
    }
    false
}

/// The nearest DOM element enclosing an IFC box's content: its own node, or —
/// for an anonymous box, which has none — the node of its nearest boxed
/// ancestor. The `InlinePhase` split walks *up* from a glyph run's owner and
/// must stop here; `b.dom_node` (`None` on an anonymous box: the wrapper for an
/// inline run beside block siblings, and a multicol container's flow) would let
/// the walk run all the way to the document root, so any `<span>` under any
/// positioned ancestor would be mis-flagged as positioned-inline and painted in
/// the wrong phase. Mirrors `layout::geometry`'s `containing_element`.
fn ifc_element(
    tree: &oxidepage_layout::LayoutTree,
    box_id: BoxId,
) -> Option<oxidepage_dom::NodeId> {
    let mut current = Some(box_id);
    while let Some(id) = current {
        let b = tree.box_(id);
        if b.dom_node.is_some() {
            return b.dom_node;
        }
        current = b.parent;
    }
    None
}

/// Memoizes "does this text node sit inside a positioned inline element (below
/// the IFC root)?" — the answer is the same for every run of a given node, and
/// an IFC can hold many runs.
#[derive(Default)]
struct PositionedOwnerCache {
    answers: std::collections::HashMap<oxidepage_dom::NodeId, bool>,
}

impl PositionedOwnerCache {
    fn is_inside_positioned_inline(
        &mut self,
        dom: &oxidepage_dom::DomTree,
        node: oxidepage_dom::NodeId,
        ifc_node: Option<oxidepage_dom::NodeId>,
    ) -> bool {
        if let Some(cached) = self.answers.get(&node) {
            return *cached;
        }
        let mut current = Some(node);
        let mut answer = false;
        while let Some(id) = current {
            if Some(id) == ifc_node {
                break;
            }
            if dom
                .primary_style(id)
                .is_some_and(|s| s.get_box().clone_position() != Position::Static)
            {
                answer = true;
                break;
            }
            current = dom.flat_tree_parent(id);
        }
        self.answers.insert(node, answer);
        answer
    }
}

fn paint_glyph_run(
    builder: &mut PaintBuilder<'_>,
    dom: &oxidepage_dom::DomTree,
    ifc_text: &str,
    content_origin: Point,
    glyph_run: &parley::GlyphRun<'_, TextBrush>,
    fallback_style: Option<&ComputedValues>,
) {
    let run = glyph_run.run();
    let brush_node = glyph_run.style().brush.node();

    // The span's computed style drives text color and decorations.
    let span_style: Option<ServoArc<ComputedValues>> =
        brush_node.and_then(|n| dom.primary_style(n));
    let color = match span_style.as_deref().or(fallback_style) {
        Some(s) => convert::absolute_to_color(&convert::current_color(s)),
        None => Color::BLACK,
    };

    // Font resource + id.
    let font = run.font();
    let font_id = FontId {
        blob: font.data.id(),
        index: font.index,
    };
    builder.add_font(FontResource {
        id: font_id,
        data: font.data.clone(),
    });

    let normalized_coords: Vec<f32> = run
        .normalized_coords()
        .iter()
        .map(|&c| f2dot14_to_f32(c))
        .collect();

    let glyphs: Vec<PositionedGlyph> = glyph_run
        .positioned_glyphs()
        .map(|g| PositionedGlyph {
            id: g.id,
            x: content_origin.x + g.x,
            y: content_origin.y + g.y,
        })
        .collect();

    let debug_text = {
        let range = run.text_range();
        ifc_text.get(range).map(str::to_string)
    };

    builder.push(DisplayItem::GlyphRun {
        font: font_id,
        size: run.font_size(),
        color,
        normalized_coords,
        glyphs,
        debug_text,
    });

    // Decorations (underline / line-through) as thin fills of the text color.
    if let Some(style) = span_style.as_deref() {
        let lines = style.get_text().clone_text_decoration_line();
        let baseline = glyph_run.baseline();
        let x = content_origin.x + glyph_run.offset();
        let advance = glyph_run.advance();
        let metrics = run.metrics();
        let mut decoration = |top_from_baseline: f32, thickness: f32| {
            if thickness > 0.0 && advance > 0.0 {
                builder.push(DisplayItem::Fill {
                    rect: Rect::from_xywh(
                        x,
                        content_origin.y + baseline - top_from_baseline,
                        advance,
                        thickness,
                    ),
                    radii: BorderRadii::ZERO,
                    brush: Brush::Solid(color),
                });
            }
        };
        if lines.contains(TextDecorationLine::UNDERLINE) {
            decoration(metrics.underline_offset, metrics.underline_size);
        }
        if lines.contains(TextDecorationLine::LINE_THROUGH) {
            decoration(metrics.strikethrough_offset, metrics.strikethrough_size);
        }
    }
}
