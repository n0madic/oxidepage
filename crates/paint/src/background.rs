//! Background painting: the background color plus one fill per background
//! layer (ADR-0007 D7). WP-E handles gradient layers; `url()` layers are
//! filled in by WP-L.
//!
//! v1 scope: the positioning area is the padding box and the clip is the
//! border box (the CSS defaults); per-layer `background-origin`/`clip`
//! keywords and true tiling of explicitly-sized gradients are deferred.

use std::f32::consts::{FRAC_PI_2, PI, SQRT_2};

use oxidepage_base::{Point, Rect, Size};
use style::color::AbsoluteColor;
use style::properties::ComputedValues;
use style::values::computed::background::BackgroundRepeat;
use style::values::computed::image::{EndingShape, Gradient, Image, LineDirection};
use style::values::computed::{Length, LengthPercentage};
use style::values::generics::image::{Circle, Ellipse, GradientItem, ShapeExtent};
use style::values::specified::background::BackgroundRepeatKeyword;
use style::values::specified::position::{HorizontalPositionKeyword, VerticalPositionKeyword};

use crate::builder::PaintBuilder;
use crate::convert;
use crate::display_list::{
    BorderRadii, Brush, Color, DisplayItem, ExtendMode, GradientStop, LinearGradient,
    RadialGradient, TileMode,
};

/// The border edge widths of a box, for deriving the padding box (the default
/// background positioning area).
#[derive(Clone, Copy)]
pub(crate) struct Edges {
    pub border: taffy::Rect<f32>,
}

/// Paints the background of a styled box: the color at the bottom, then each
/// background layer (bottom-to-top; layer 0 is topmost, painted last).
pub(crate) fn paint(
    builder: &mut PaintBuilder<'_>,
    border_box: Rect,
    edges: Edges,
    radii: BorderRadii,
    style: &ComputedValues,
    paint_color: bool,
) {
    let bg = style.get_background();
    let current = convert::current_color(style);

    if paint_color {
        let color = convert::absolute_to_color(&bg.background_color.resolve_to_absolute(&current));
        if !color.is_transparent() {
            builder.push(DisplayItem::Fill {
                rect: border_box,
                radii,
                brush: Brush::Solid(color),
            });
        }
    }

    let images = &bg.background_image.0;
    if images.is_empty() {
        return;
    }
    let positioning_area = inset(border_box, edges.border);

    // Layers paint bottom-to-top: iterate the list in reverse.
    for i in (0..images.len()).rev() {
        let size = pick(&bg.background_size.0, i);
        let repeat = pick(&bg.background_repeat.0, i);
        let pos_x = pick(&bg.background_position_x.0, i);
        let pos_y = pick(&bg.background_position_y.0, i);
        let tile_mode = tile_mode_of(repeat);
        let repeats = tile_mode != TileMode::Stretch;

        match &images[i] {
            Image::Gradient(gradient) => {
                // Gradients have no intrinsic size (auto = positioning area).
                let tile = layer_tile(size, pos_x, pos_y, positioning_area, None);
                let Some(brush) = gradient_brush(gradient, tile, &current) else {
                    continue;
                };
                // no-repeat → the single tile; repeat → the whole border box
                // (one gradient covers it when the tile equals the area).
                let (rect, layer_radii) = if repeats {
                    (border_box, radii)
                } else if tile == border_box {
                    (tile, radii)
                } else {
                    (tile, BorderRadii::ZERO)
                };
                builder.push(DisplayItem::Fill {
                    rect,
                    radii: layer_radii,
                    brush,
                });
            }
            Image::Url(url) => {
                let Some(abs) = url.url() else { continue };
                let Some(image) = builder.engine().images().get(abs.as_str()) else {
                    continue; // not loaded / broken → nothing to paint
                };
                let intrinsic = Size::new(image.width as f32, image.height as f32);
                let tile = layer_tile(size, pos_x, pos_y, positioning_area, Some(intrinsic));
                let id = image.id;
                builder.add_image(image);
                // Every background layer is clipped to its background-clip
                // area, including a single no-repeat tile. `cover` commonly
                // makes that tile larger than the box on one axis.
                builder.push(DisplayItem::PushClip {
                    rect: border_box,
                    radii,
                });
                builder.push(DisplayItem::Image {
                    dst: tile,
                    image: id,
                    tile: tile_mode,
                    radii: BorderRadii::ZERO,
                });
                builder.push(DisplayItem::PopClip);
            }
            _ => {} // none / unsupported
        }
    }
}

/// Maps a `background-repeat` to a display-list [`TileMode`].
fn tile_mode_of(repeat: &BackgroundRepeat) -> TileMode {
    let x = repeat.0 != BackgroundRepeatKeyword::NoRepeat;
    let y = repeat.1 != BackgroundRepeatKeyword::NoRepeat;
    match (x, y) {
        (true, true) => TileMode::Repeat,
        (true, false) => TileMode::RepeatX,
        (false, true) => TileMode::RepeatY,
        (false, false) => TileMode::Stretch,
    }
}

/// The `i`-th layer value from a background list, repeating the shorter list
/// (`list[i % len]`).
fn pick<T>(list: &[T], i: usize) -> &T {
    &list[i % list.len()]
}

/// Insets `rect` by taffy edge widths.
fn inset(rect: Rect, e: taffy::Rect<f32>) -> Rect {
    Rect::from_xywh(
        rect.origin.x + e.left,
        rect.origin.y + e.top,
        (rect.size.width - e.left - e.right).max(0.0),
        (rect.size.height - e.top - e.bottom).max(0.0),
    )
}

/// The tile rect (size + origin) of a background layer within its positioning
/// area. `intrinsic` is the image's natural size (`None` for gradients, whose
/// `auto`/`cover`/`contain` all resolve to the positioning-area size); for
/// images, `auto` preserves the aspect ratio and `cover`/`contain` scale it.
pub(crate) fn layer_tile(
    size: &style::values::computed::BackgroundSize,
    pos_x: &LengthPercentage,
    pos_y: &LengthPercentage,
    area: Rect,
    intrinsic: Option<Size>,
) -> Rect {
    use style::values::generics::background::GenericBackgroundSize as Bg;
    use style::values::generics::length::GenericLengthPercentageOrAuto as LpAuto;

    let natural = intrinsic.unwrap_or(area.size);
    let ratio = (natural.height > 0.0).then(|| natural.width / natural.height);

    let tile_size = match size {
        Bg::Cover => scale_keep_ratio(natural, area.size, true),
        Bg::Contain => scale_keep_ratio(natural, area.size, false),
        Bg::ExplicitSize { width, height } => {
            let w = match width {
                LpAuto::Auto => None,
                LpAuto::LengthPercentage(lp) => {
                    Some(lp.0.resolve(Length::new(area.size.width)).px())
                }
            };
            let h = match height {
                LpAuto::Auto => None,
                LpAuto::LengthPercentage(lp) => {
                    Some(lp.0.resolve(Length::new(area.size.height)).px())
                }
            };
            match (w, h, intrinsic, ratio) {
                (Some(w), Some(h), _, _) => Size::new(w, h),
                // One axis auto: preserve the intrinsic aspect ratio if known.
                (Some(w), None, Some(_), Some(r)) => Size::new(w, w / r),
                (None, Some(h), Some(_), Some(r)) => Size::new(h * r, h),
                (Some(w), None, _, _) => Size::new(w, area.size.height),
                (None, Some(h), _, _) => Size::new(area.size.width, h),
                // Both auto: the intrinsic size (or the area, for gradients).
                (None, None, _, _) => natural,
            }
        }
    };

    // background-position: percentage resolves against (area − tile); length
    // is added (exactly what LengthPercentage::resolve computes).
    let dx = pos_x
        .resolve(Length::new(area.size.width - tile_size.width))
        .px();
    let dy = pos_y
        .resolve(Length::new(area.size.height - tile_size.height))
        .px();
    Rect::new(
        Point::new(area.origin.x + dx, area.origin.y + dy),
        tile_size,
    )
}

/// Scales `natural` uniformly to `cover` (fill) or contain (fit) `area`.
fn scale_keep_ratio(natural: Size, area: Size, cover: bool) -> Size {
    if natural.width <= 0.0 || natural.height <= 0.0 {
        return area;
    }
    let sx = area.width / natural.width;
    let sy = area.height / natural.height;
    let scale = if cover { sx.max(sy) } else { sx.min(sy) };
    Size::new(natural.width * scale, natural.height * scale)
}

/// Builds the display-list brush for a gradient layer over `tile`. Returns
/// `None` for unsupported gradients (conic).
fn gradient_brush(gradient: &Gradient, tile: Rect, current: &AbsoluteColor) -> Option<Brush> {
    match gradient {
        Gradient::Linear {
            direction,
            items,
            flags,
            ..
        } => {
            let (start, end) = linear_endpoints(direction, tile);
            let line_len = ((end.x - start.x).powi(2) + (end.y - start.y).powi(2)).sqrt();
            let stops = build_stops(items, current, line_len);
            Some(Brush::LinearGradient(LinearGradient {
                start,
                end,
                stops,
                extend: extend_of(*flags),
            }))
        }
        Gradient::Radial {
            shape,
            position,
            items,
            flags,
            ..
        } => {
            let (center, radius) = radial_geometry(shape, position, tile);
            let stops = build_stops(items, current, radius.width.max(1.0));
            Some(Brush::RadialGradient(RadialGradient {
                center,
                radius,
                stops,
                extend: extend_of(*flags),
            }))
        }
        Gradient::Conic { .. } => None, // deferred (ADR-0007 D7)
    }
}

fn extend_of(flags: style::values::generics::image::GradientFlags) -> ExtendMode {
    if flags.contains(style::values::generics::image::GradientFlags::REPEATING) {
        ExtendMode::Repeat
    } else {
        ExtendMode::Pad
    }
}

/// The start/end points of a linear gradient line over `tile`, in absolute
/// coordinates (CSS: 0deg points up, angle increases clockwise).
pub(crate) fn linear_endpoints(direction: &LineDirection, tile: Rect) -> (Point, Point) {
    let (w, h) = (tile.size.width, tile.size.height);
    let (ux, uy) = direction_unit(direction, w, h);
    let cx = tile.origin.x + w / 2.0;
    let cy = tile.origin.y + h / 2.0;
    // Half the gradient-line length: the projection of the box half-extents
    // onto the (unit) direction.
    let half = (w * ux.abs() + h * uy.abs()) / 2.0;
    (
        Point::new(cx - half * ux, cy - half * uy),
        Point::new(cx + half * ux, cy + half * uy),
    )
}

/// The unit direction vector (screen coordinates, y down) of a gradient line.
fn direction_unit(direction: &LineDirection, w: f32, h: f32) -> (f32, f32) {
    let angle = match direction {
        LineDirection::Angle(a) => Some(a.radians()),
        LineDirection::Horizontal(HorizontalPositionKeyword::Left) => Some(3.0 * FRAC_PI_2),
        LineDirection::Horizontal(HorizontalPositionKeyword::Right) => Some(FRAC_PI_2),
        LineDirection::Vertical(VerticalPositionKeyword::Top) => Some(0.0),
        LineDirection::Vertical(VerticalPositionKeyword::Bottom) => Some(PI),
        LineDirection::Corner(hx, vy) => {
            let dx = match hx {
                HorizontalPositionKeyword::Right => w,
                HorizontalPositionKeyword::Left => -w,
            };
            let dy = match vy {
                VerticalPositionKeyword::Top => -h,
                VerticalPositionKeyword::Bottom => h,
            };
            let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
            return (dx / len, dy / len);
        }
    };
    let a = angle.unwrap_or(PI);
    (a.sin(), -a.cos())
}

/// The center and (x, y) radii of a radial gradient over `tile`, absolute.
pub(crate) fn radial_geometry(
    shape: &EndingShape,
    position: &style::values::computed::Position,
    tile: Rect,
) -> (Point, Size) {
    let (w, h) = (tile.size.width, tile.size.height);
    let lcx = position.horizontal.resolve(Length::new(w)).px();
    let lcy = position.vertical.resolve(Length::new(h)).px();
    let center = Point::new(tile.origin.x + lcx, tile.origin.y + lcy);

    let fsx = lcx.max(w - lcx);
    let fsy = lcy.max(h - lcy);
    let csx = lcx.min(w - lcx);
    let csy = lcy.min(h - lcy);

    let radius = match shape {
        EndingShape::Circle(Circle::Radius(len)) => {
            let r = len.0.px();
            Size::new(r, r)
        }
        EndingShape::Circle(Circle::Extent(ext)) => {
            let r = match ext {
                ShapeExtent::ClosestSide | ShapeExtent::Contain => csx.min(csy),
                ShapeExtent::FarthestSide => fsx.max(fsy),
                ShapeExtent::ClosestCorner => (csx * csx + csy * csy).sqrt(),
                ShapeExtent::FarthestCorner | ShapeExtent::Cover => (fsx * fsx + fsy * fsy).sqrt(),
            };
            Size::new(r, r)
        }
        EndingShape::Ellipse(Ellipse::Radii(rx, ry)) => Size::new(
            rx.0.resolve(Length::new(w)).px(),
            ry.0.resolve(Length::new(h)).px(),
        ),
        EndingShape::Ellipse(Ellipse::Extent(ext)) => match ext {
            ShapeExtent::ClosestSide | ShapeExtent::Contain => Size::new(csx, csy),
            ShapeExtent::FarthestSide => Size::new(fsx, fsy),
            ShapeExtent::ClosestCorner => Size::new(SQRT_2 * csx, SQRT_2 * csy),
            ShapeExtent::FarthestCorner | ShapeExtent::Cover => {
                Size::new(SQRT_2 * fsx, SQRT_2 * fsy)
            }
        },
    };
    (center, radius)
}

/// Converts gradient items to normalized `[0, 1]` color stops, resolving
/// currentColor, positions, and filling implicit positions (CSS stop
/// normalization). Interpolation hints are skipped in v1.
fn build_stops(
    items: &[GradientItem<style::values::computed::Color, LengthPercentage>],
    current: &AbsoluteColor,
    line_len: f32,
) -> Vec<GradientStop> {
    let mut stops: Vec<(Color, Option<f32>)> = items
        .iter()
        .filter_map(|item| match item {
            GradientItem::SimpleColorStop(color) => {
                Some((convert::resolve_color(color, current), None))
            }
            GradientItem::ComplexColorStop { color, position } => {
                let p = position.to_percentage().map_or_else(
                    || {
                        if line_len > 0.0 {
                            position.resolve(Length::new(line_len)).px() / line_len
                        } else {
                            0.0
                        }
                    },
                    |pct| pct.0,
                );
                Some((convert::resolve_color(color, current), Some(p)))
            }
            GradientItem::InterpolationHint(_) => None,
        })
        .collect();

    if stops.is_empty() {
        return Vec::new();
    }

    // The first and last stops default to 0 and 1.
    if stops[0].1.is_none() {
        stops[0].1 = Some(0.0);
    }
    let last = stops.len() - 1;
    if stops[last].1.is_none() {
        stops[last].1 = Some(1.0);
    }
    // Enforce non-decreasing positions on the explicit stops.
    let mut max = 0.0f32;
    for s in stops.iter_mut() {
        if let Some(p) = s.1 {
            let clamped = p.max(max);
            s.1 = Some(clamped);
            max = clamped;
        }
    }
    // Distribute the implicit positions evenly between their bracketing
    // explicit stops.
    let mut i = 0;
    while i < stops.len() {
        if stops[i].1.is_some() {
            i += 1;
            continue;
        }
        let start = i - 1;
        let start_pos = stops[start].1.expect("index 0 is set");
        let mut j = i;
        while stops[j].1.is_none() {
            j += 1;
        }
        let end_pos = stops[j].1.expect("last is set");
        let gap = j - start;
        for (k, idx) in (i..j).enumerate() {
            let t = (k + 1) as f32 / gap as f32;
            stops[idx].1 = Some(start_pos + (end_pos - start_pos) * t);
        }
        i = j;
    }

    stops
        .into_iter()
        .map(|(color, offset)| GradientStop {
            offset: offset.unwrap_or(0.0),
            color,
        })
        .collect()
}
