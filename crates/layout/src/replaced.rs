//! Replaced-element sizing (adapted from blitz-dom `layout/replaced.rs`):
//! CSS 2.2 §10.4 constraint resolution over intrinsic size, attribute size,
//! style size, and aspect ratio.

use style::Atom;
use taffy::{
    AvailableSpace, BoxSizing, CoreStyle as _, MaybeMath, MaybeResolve, ResolveOrZero as _, Size,
};

use crate::taffy_impl::resolve_calc_value;
use crate::tree::ReplacedContext;

/// Whether a height/width value is violating its min- and max- constraints.
/// The min- and max- constraints cannot both be violated because the max
/// constraint is floored by the min constraint (min constraint takes priority).
enum Violation {
    /// Constraints are not violated
    None,
    /// Min constraint is violated
    Min,
    /// Max constraint is violated
    Max,
}

pub(crate) fn replaced_measure_function(
    known_dimensions: taffy::Size<Option<f32>>,
    parent_size: taffy::Size<Option<f32>>,
    available_space: taffy::Size<AvailableSpace>,
    image_context: &ReplacedContext,
    style: &taffy::Style<Atom>,
) -> taffy::Size<f32> {
    let inherent_size = image_context.inherent_size;

    let padding = style
        .padding()
        .resolve_or_zero(parent_size.width, resolve_calc_value);
    let border = style
        .border()
        .resolve_or_zero(parent_size.width, resolve_calc_value);
    let padding_border = padding + border;
    let pb_sum = Size {
        width: padding_border.left + padding_border.right,
        height: padding_border.top + padding_border.bottom,
    };
    let box_sizing_adjustment = if style.box_sizing() == BoxSizing::BorderBox {
        pb_sum
    } else {
        Size::ZERO
    };

    let attr_size = image_context.attr_size;

    // Use aspect_ratio from style, falling back to the inherent ratio and
    // then the width/height attributes (the HTML `img` aspect-ratio
    // mapping). With a 0×0 intrinsic size and no other source there is no
    // ratio at all — `None` here, so no arm below can fabricate a NaN
    // dimension out of `0.0 / 0.0`.
    let finite_ratio = |width: f32, height: f32| -> Option<f32> {
        let ratio = width / height;
        (ratio.is_finite() && ratio > 0.0).then_some(ratio)
    };
    let aspect_ratio = style
        .aspect_ratio
        .or_else(|| finite_ratio(inherent_size.width, inherent_size.height))
        .or_else(|| match (attr_size.width, attr_size.height) {
            (Some(width), Some(height)) => finite_ratio(width, height),
            _ => None,
        });
    // Derive one axis from the other via the ratio, or keep the fallback
    // when no ratio exists.
    let width_from_height =
        |height: f32, fallback: f32| aspect_ratio.map(|r| height * r).unwrap_or(fallback);
    let height_from_width =
        |width: f32, fallback: f32| aspect_ratio.map(|r| width / r).unwrap_or(fallback);

    // See https://www.w3.org/TR/css-sizing-3/#replaced-percentage-min-contribution
    //
    // The rule zeroes a percentage in the axis whose *min-content contribution*
    // is being computed — and only the width is ever asked for. Taffy probes
    // intrinsic widths with `available_space.height == MinContent` regardless,
    // so reading that as a zero basis collapses `height: <pct>` and then, via
    // the aspect ratio, the width too: every replaced element with a percentage
    // height inside an auto-height block measures 0×0 (mgid.com's hero image is
    // `width: 100%; height: 100%`). A percentage with nothing to resolve against
    // is `auto` (CSS 2.2 §10.5) — the `None` below — which yields the intrinsic
    // size, as in browsers.
    let basis_for_max_and_preferred = Size {
        width: if available_space.width == AvailableSpace::MinContent {
            Some(0.0)
        } else {
            parent_size.width
        },
        height: parent_size.height,
    };

    // Resolve sizes
    let style_size = style
        .size
        .maybe_resolve(basis_for_max_and_preferred, resolve_calc_value)
        .maybe_apply_aspect_ratio(aspect_ratio)
        .maybe_sub(box_sizing_adjustment);
    let min_size = style
        .min_size
        .maybe_resolve(parent_size, resolve_calc_value)
        .maybe_sub(box_sizing_adjustment);
    let max_size = style
        .max_size
        .maybe_resolve(basis_for_max_and_preferred, resolve_calc_value)
        .or(available_space.into_options())
        .maybe_min(available_space.into_options())
        .maybe_max(min_size)
        .maybe_sub(box_sizing_adjustment);

    let unclamped_size = 'size: {
        if known_dimensions.width.is_some() | known_dimensions.height.is_some() {
            let content_box_known_dimensions = known_dimensions.maybe_sub(pb_sum);
            break 'size content_box_known_dimensions
                .maybe_apply_aspect_ratio(aspect_ratio)
                .map(|s| s.unwrap_or(0.0));
        }

        if style_size.width.is_some() | style_size.height.is_some() {
            break 'size style_size
                .maybe_apply_aspect_ratio(aspect_ratio)
                .map(|s| s.unwrap_or(0.0));
        }

        if attr_size.width.is_some() | attr_size.height.is_some() {
            break 'size attr_size
                .maybe_apply_aspect_ratio(aspect_ratio)
                .map(|s| s.unwrap_or(0.0));
        }

        inherent_size
    };

    // Floor size at zero (also collapses the NaNs produced by a 0×0
    // intrinsic size with no other constraint).
    let size = unclamped_size.map(|s| if s.is_nan() { 0.0 } else { s.max(0.0) });

    // Violations
    let width_violation = if size.width < min_size.width.unwrap_or(0.0) {
        Violation::Min
    } else if size.width > max_size.width.unwrap_or(f32::INFINITY) {
        Violation::Max
    } else {
        Violation::None
    };

    let height_violation = if size.height < min_size.height.unwrap_or(0.0) {
        Violation::Min
    } else if size.height > max_size.height.unwrap_or(f32::INFINITY) {
        Violation::Max
    } else {
        Violation::None
    };

    // Clamp following rules in table at
    // https://www.w3.org/TR/CSS22/visudet.html#min-max-widths
    let size = match (width_violation, height_violation) {
        // No constraint violation
        (Violation::None, Violation::None) => size,
        // w > max-width
        (Violation::Max, Violation::None) => {
            let max_width = max_size.width.unwrap();
            Size {
                width: max_width,
                height: height_from_width(max_width, size.height).maybe_max(min_size.height),
            }
        }
        // w < min-width
        (Violation::Min, Violation::None) => {
            let min_width = min_size.width.unwrap();
            Size {
                width: min_width,
                height: height_from_width(min_width, size.height).maybe_min(max_size.height),
            }
        }
        // h > max-height
        (Violation::None, Violation::Max) => {
            let max_height = max_size.height.unwrap();
            Size {
                width: width_from_height(max_height, size.width).maybe_max(min_size.width),
                height: max_height,
            }
        }
        // h < min-height
        (Violation::None, Violation::Min) => {
            let min_height = min_size.height.unwrap();
            Size {
                width: width_from_height(min_height, size.width).maybe_min(max_size.width),
                height: min_height,
            }
        }
        // (w > max-width) and (h > max-height)
        (Violation::Max, Violation::Max) => {
            let max_width = max_size.width.unwrap();
            let max_height = max_size.height.unwrap();
            if max_width / size.width <= max_height / size.height {
                Size {
                    width: max_width,
                    height: height_from_width(max_width, size.height).maybe_max(min_size.height),
                }
            } else {
                Size {
                    width: width_from_height(max_height, size.width).maybe_max(min_size.width),
                    height: max_height,
                }
            }
        }
        // (w < min-width) and (h < min-height)
        (Violation::Min, Violation::Min) => {
            let min_width = min_size.width.unwrap();
            let min_height = min_size.height.unwrap();
            if min_width / size.width <= min_height / size.height {
                Size {
                    width: width_from_height(min_height, size.width).maybe_min(max_size.width),
                    height: min_height,
                }
            } else {
                Size {
                    width: min_width,
                    height: height_from_width(min_width, size.height).maybe_min(max_size.height),
                }
            }
        }
        // (w < min-width) and (h > max-height)
        (Violation::Min, Violation::Max) => {
            let min_width = min_size.width.unwrap();
            let max_height = max_size.height.unwrap();
            Size {
                width: min_width,
                height: max_height,
            }
        }
        // (w > max-width) and (h < min-height)
        (Violation::Max, Violation::Min) => {
            let max_width = max_size.width.unwrap();
            let min_height = min_size.height.unwrap();
            Size {
                width: max_width,
                height: min_height,
            }
        }
    };

    size + pb_sum
}
