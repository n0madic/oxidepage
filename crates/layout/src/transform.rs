//! The **single** CSS transform resolver: computed values → a 2D affine
//! matrix.
//!
//! Paint applies transforms at raster time and geometry has to report the same
//! rects hit-testing has to invert (ADR-0026). Two resolvers would be two
//! answers, so there is one — living here, in `layout`, because `paint` depends
//! on `layout` and not the other way round (design §3 layering). It is the same
//! rule [`crate::multicol::map_flow_point`] states for the column transform.
//!
//! Two consumers, two coordinate spaces, one function:
//!
//! * paint passes the box's **absolute** border box and gets an absolute
//!   matrix, which it hangs on `DisplayItem::PushLayer`;
//! * layout resolves every transformed box once per reflow against its
//!   **local** border box (`0, 0, w, h`) and caches the result on
//!   [`crate::LayoutBox::transform`], because [`crate::geometry`] has no access
//!   to computed styles. The two agree exactly: a local matrix conjugated by
//!   the box's absolute origin *is* the absolute one (see
//!   [`Transform2D::at_origin`]).

use oxidepage_base::{Rect, Transform2D};
use oxidepage_dom::DomTree;
use servo_arc::Arc as ServoArc;
use style::properties::ComputedValues;
use style::properties::style_structs::Box as BoxStyle;
use style::selector_parser::PseudoElement;
use style::values::computed::Length;
use style::values::computed::transform::{Rotate, Scale, Transform, TransformOperation, Translate};

use crate::tree::{BoxId, LayoutTree, PseudoBox};

/// Whether `style` sets any of the four transform properties. A cheap
/// pre-filter (and the CSS "establishes a containing block for fixed and
/// absolute descendants" test), captured onto every box at construction time as
/// [`crate::LayoutBox::has_transform`].
#[must_use]
pub fn has_transform(style: &ComputedValues) -> bool {
    let box_style = style.get_box();
    !box_style.transform.0.is_empty()
        || !matches!(box_style.translate, Translate::None)
        || !matches!(box_style.rotate, Rotate::None)
        || !matches!(box_style.scale, Scale::None)
}

/// The transform functions of `box_style` in CSS Transforms 2 §"Individual
/// Transform Properties" order — `translate`, then `rotate`, then `scale`, then
/// the `transform` list, each applied after the ones before it. `None` when the
/// element is untransformed.
fn operations(box_style: &BoxStyle) -> Option<Vec<TransformOperation>> {
    let mut ops = Vec::new();
    match &box_style.translate {
        Translate::None => {}
        Translate::Translate(x, y, z) => {
            ops.push(TransformOperation::Translate3D(x.clone(), y.clone(), *z));
        }
    }
    match &box_style.rotate {
        Rotate::None => {}
        Rotate::Rotate(angle) => ops.push(TransformOperation::Rotate(*angle)),
        Rotate::Rotate3D(x, y, z, angle) => {
            ops.push(TransformOperation::Rotate3D(*x, *y, *z, *angle));
        }
    }
    match &box_style.scale {
        Scale::None => {}
        Scale::Scale(x, y, z) => ops.push(TransformOperation::Scale3D(*x, *y, *z)),
    }
    ops.extend(box_style.transform.0.iter().cloned());
    (!ops.is_empty()).then_some(ops)
}

/// The box's transform as an affine matrix **in the coordinate space of the
/// `border_box` passed in**: the transform origin is baked in, so passing the
/// absolute border box yields an absolute matrix and passing
/// `Rect::from_xywh(0, 0, w, h)` yields the box-local one. That is the whole
/// reason one function serves paint, geometry and hit-testing.
///
/// `None` for an untransformed element and for a transform list that resolves
/// to the identity (paint then skips the layer, and geometry the mapping).
///
/// A 3D transform list is flattened to its 2D affine part (the perspective and
/// z components are dropped), which is exact for the `translate3d(x, y, 0)` /
/// `translateZ(0)` compositing hints that dominate real pages and an
/// approximation for genuine 3D (ADR-0013, ADR-0026). Geometry is flattened the
/// same way, so the two stay consistent even where both approximate.
#[must_use]
pub fn resolve(style: &ComputedValues, border_box: Rect) -> Option<Transform2D> {
    use euclid::default::{Point2D, Rect as EuclidRect, Size2D};

    let box_style = style.get_box();
    let ops = operations(box_style)?;

    // Percentages in the transform functions resolve against the border box;
    // only its size is read, so the origin is irrelevant here.
    let reference = EuclidRect::new(
        Point2D::new(Length::new(0.0), Length::new(0.0)),
        Size2D::new(
            Length::new(border_box.size.width),
            Length::new(border_box.size.height),
        ),
    );
    let (m, _is_3d) = Transform::components_to_transform_3d_matrix(&ops, Some(&reference)).ok()?;
    let matrix = Transform2D {
        a: m.m11,
        b: m.m12,
        c: m.m21,
        d: m.m22,
        tx: m.m41,
        ty: m.m42,
    };
    if matrix == Transform2D::IDENTITY {
        return None;
    }

    let origin = &box_style.transform_origin;
    let ox = border_box.origin.x
        + origin
            .horizontal
            .resolve(Length::new(border_box.size.width))
            .px();
    let oy = border_box.origin.y
        + origin
            .vertical
            .resolve(Length::new(border_box.size.height))
            .px();

    // Around the origin: translate it to (0, 0), apply the matrix, translate back.
    Some(
        Transform2D::translation(-ox, -oy)
            .then(&matrix)
            .then(&Transform2D::translation(ox, oy)),
    )
}

/// The computed style a box paints with: its principal style, or the matching
/// pseudo-element style (a `::marker` inherits the item's, as `layout::marker`
/// makes it). `None` for anonymous boxes, which cannot be transformed.
fn style_for_box(dom: &DomTree, tree: &LayoutTree, id: BoxId) -> Option<ServoArc<ComputedValues>> {
    let b = tree.box_(id);
    let node = b.dom_node?;
    match b.pseudo {
        None | Some(PseudoBox::Marker) => dom.primary_style(node),
        Some(PseudoBox::Before) => dom.pseudo_style(node, &PseudoElement::Before),
        Some(PseudoBox::After) => dom.pseudo_style(node, &PseudoElement::After),
    }
}

/// Post-layout pass: caches every transformed box's **local** matrix on
/// [`crate::LayoutBox::transform`].
///
/// It runs after rounding because `transform-origin` percentages and
/// `translate: 50%` resolve against the used border-box size, which taffy only
/// settles then — and because geometry and hit-testing read the rounded boxes.
pub(crate) fn resolve_transforms(tree: &mut LayoutTree, dom: &DomTree) {
    for index in 0..tree.box_count() {
        crate::budget::checkpoint();
        let id = BoxId(index as u32);
        if !tree.box_(id).has_transform {
            // Cheap reset: a patched reflow reuses boxes, so a box that lost its
            // transform must lose the cached matrix with it.
            tree.box_mut(id).transform = None;
            continue;
        }
        let size = tree.box_(id).final_layout.size;
        let local = Rect::from_xywh(0.0, 0.0, size.width, size.height);
        let matrix = style_for_box(dom, tree, id).and_then(|style| resolve(&style, local));
        tree.box_mut(id).transform = matrix;
    }
}
