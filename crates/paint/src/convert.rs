//! Conversions from stylo computed values to backend-neutral display-list
//! types (colors, borders, radii). Paint reads styles at paint time via
//! `dom.primary_style`/`pseudo_style`, so no active-tree scope is needed
//! (ADR-0007 D2).

use oxidepage_base::{Rect, Size, Transform2D};
use style::color::{AbsoluteColor, ColorSpace};
use style::properties::ComputedValues;
use style::values::computed::{Color as ComputedColor, Length};
use style::values::specified::border::BorderStyle as StyloBorderStyle;

use crate::display_list::{BorderEdge, BorderRadii, BorderStyle, Color};

/// Converts an absolute stylo color to 8-bit sRGBA.
#[must_use]
pub(crate) fn absolute_to_color(color: &AbsoluteColor) -> Color {
    let srgb = if matches!(color.color_space, ColorSpace::Srgb) {
        *color
    } else {
        color.to_color_space(ColorSpace::Srgb)
    };
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::rgba(
        ch(srgb.components.0),
        ch(srgb.components.1),
        ch(srgb.components.2),
        ch(srgb.alpha),
    )
}

/// The element's resolved `color` (used for `currentColor`).
#[must_use]
pub(crate) fn current_color(style: &ComputedValues) -> AbsoluteColor {
    style.get_inherited_text().clone_color()
}

/// Resolves a possibly-`currentColor` computed color to 8-bit sRGBA.
#[must_use]
pub(crate) fn resolve_color(color: &ComputedColor, current: &AbsoluteColor) -> Color {
    absolute_to_color(&color.resolve_to_absolute(current))
}

/// The element's `background-color`, resolved to 8-bit sRGBA.
#[must_use]
pub(crate) fn background_color(style: &ComputedValues) -> Color {
    let current = current_color(style);
    resolve_color(&style.get_background().background_color, &current)
}

/// The box's `transform`, as an affine matrix in the same absolute coordinate
/// space as `border_box`: the computed transform list resolved about
/// `transform-origin` (percentages against the border box). `None` for
/// `transform: none` and for a list that resolves to the identity.
///
/// A 3D transform list is flattened to its 2D affine part (the perspective and
/// z components are dropped), which is exact for the `translate3d(x, y, 0)` /
/// `translateZ(0)` compositing hints that dominate real pages and an
/// approximation for genuine 3D (ADR-0013).
#[must_use]
pub(crate) fn transform(style: &ComputedValues, border_box: Rect) -> Option<Transform2D> {
    use euclid::default::{Point2D, Rect as EuclidRect, Size2D};

    let box_style = style.get_box();
    if box_style.transform.0.is_empty() {
        return None;
    }

    // Percentages in the transform list resolve against the border box; only
    // its size is read, so the origin is irrelevant here.
    let reference = EuclidRect::new(
        Point2D::new(Length::new(0.0), Length::new(0.0)),
        Size2D::new(
            Length::new(border_box.size.width),
            Length::new(border_box.size.height),
        ),
    );
    let (m, _is_3d) = box_style
        .transform
        .to_transform_3d_matrix(Some(&reference))
        .ok()?;
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

/// The element's `opacity`, clamped to `[0, 1]`.
#[must_use]
pub(crate) fn opacity(style: &ComputedValues) -> f32 {
    style.get_effects().clone_opacity().clamp(0.0, 1.0)
}

fn map_border_style(style: StyloBorderStyle) -> BorderStyle {
    match style {
        StyloBorderStyle::None => BorderStyle::None,
        StyloBorderStyle::Hidden => BorderStyle::Hidden,
        StyloBorderStyle::Solid => BorderStyle::Solid,
        StyloBorderStyle::Double => BorderStyle::Double,
        StyloBorderStyle::Dotted => BorderStyle::Dotted,
        StyloBorderStyle::Dashed => BorderStyle::Dashed,
        StyloBorderStyle::Groove => BorderStyle::Groove,
        StyloBorderStyle::Ridge => BorderStyle::Ridge,
        StyloBorderStyle::Inset => BorderStyle::Inset,
        StyloBorderStyle::Outset => BorderStyle::Outset,
    }
}

/// The four border edges (top, right, bottom, left), pairing the used width
/// from `layout_border` with the color/style from the computed values. A
/// `none`/`hidden` edge is forced to zero width (ADR-0007 D7).
#[must_use]
pub(crate) fn border_edges(
    style: &ComputedValues,
    layout_border: taffy::Rect<f32>,
) -> [BorderEdge; 4] {
    let border = style.get_border();
    let current = current_color(style);

    let edge = |width: f32, color: &ComputedColor, s: StyloBorderStyle| {
        let mapped = map_border_style(s);
        BorderEdge {
            width: if mapped.paints() { width } else { 0.0 },
            color: resolve_color(color, &current),
            style: mapped,
        }
    };

    [
        edge(
            layout_border.top,
            &border.border_top_color,
            border.border_top_style,
        ),
        edge(
            layout_border.right,
            &border.border_right_color,
            border.border_right_style,
        ),
        edge(
            layout_border.bottom,
            &border.border_bottom_color,
            border.border_bottom_style,
        ),
        edge(
            layout_border.left,
            &border.border_left_color,
            border.border_left_style,
        ),
    ]
}

/// The border-radii of `style`, resolving percentages against the border-box
/// size (x radii against width, y radii against height) and clamping adjacent
/// corners so they never overlap.
#[must_use]
pub(crate) fn border_radii(style: &ComputedValues, border_box: Size) -> BorderRadii {
    let border = style.get_border();
    let corner = |radius: &style::values::computed::BorderCornerRadius| {
        Size::new(
            radius.0.width.0.resolve(Length::new(border_box.width)).px(),
            radius
                .0
                .height
                .0
                .resolve(Length::new(border_box.height))
                .px(),
        )
    };
    BorderRadii {
        top_left: corner(&border.border_top_left_radius),
        top_right: corner(&border.border_top_right_radius),
        bottom_right: corner(&border.border_bottom_right_radius),
        bottom_left: corner(&border.border_bottom_left_radius),
    }
    .clamped_to(border_box)
}
