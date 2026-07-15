//! A backend-neutral path sink plus the shared path-emission routines that feed
//! it. The raster (`oxidepage-raster-skia`) and PDF (`oxidepage-export-pdf`)
//! backends implement [`PathSink`] over their native path builders so both
//! build rounded-rect outlines and glyph paths from one source of truth, keeping
//! their geometry identical (ADR-0007 D7).

use oxidepage_base::Rect;

use crate::display_list::{BorderRadii, KAPPA};
use crate::glyphs::PathCommand;

/// A sink for path-construction operations in device coordinates. Backends
/// implement it over their native path builder (tiny_skia `PathBuilder`, a PDF
/// content stream) so the shared emitters below produce identical geometry.
pub trait PathSink {
    /// Begins a new subpath at `(x, y)`.
    fn move_to(&mut self, x: f32, y: f32);
    /// Appends a straight segment to `(x, y)`.
    fn line_to(&mut self, x: f32, y: f32);
    /// Appends a cubic Bézier from the current point through the two control
    /// points to `(x, y)`.
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32);
    /// Appends an axis-aligned rectangle as a closed subpath. Backends map this
    /// to their native rectangle primitive so the emitted geometry is unchanged
    /// from before this shared emitter existed.
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32);
    /// Closes the current subpath.
    fn close(&mut self);
    /// Appends a quadratic Bézier from `cur` (the current point) through control
    /// point `(cx, cy)` to `(x, y)`. The default elevates it to a cubic for
    /// sinks without a native quadratic (e.g. PDF); sinks that have one
    /// (tiny_skia) override this and ignore `cur`. Elevation commutes with the
    /// affine mapping a sink may apply, so the result is coordinate-space
    /// independent.
    fn quad_to(&mut self, cur: (f32, f32), cx: f32, cy: f32, x: f32, y: f32) {
        let (p0x, p0y) = cur;
        let c1 = (p0x + 2.0 / 3.0 * (cx - p0x), p0y + 2.0 / 3.0 * (cy - p0y));
        let c2 = (x + 2.0 / 3.0 * (cx - x), y + 2.0 / 3.0 * (cy - y));
        self.curve_to(c1.0, c1.1, c2.0, c2.1, x, y);
    }
}

/// Emits a (possibly rounded) rectangle subpath into `sink`. Radii are clamped
/// so adjacent corners never overlap; a zero-radius rect emits a plain
/// rectangle via [`PathSink::rect`]. Corner arcs use the shared [`KAPPA`] cubic
/// approximation. Shared by the raster and PDF backends (ADR-0007 D7).
pub fn emit_rounded_rect<S: PathSink + ?Sized>(sink: &mut S, rect: Rect, radii: &BorderRadii) {
    let (x, y) = (rect.origin.x, rect.origin.y);
    let (w, h) = (rect.size.width, rect.size.height);

    if radii.is_zero() {
        sink.rect(x, y, w, h);
        return;
    }

    let r = radii.clamped_to(rect.size);
    let (tl, tr, br, bl) = (r.top_left, r.top_right, r.bottom_right, r.bottom_left);

    // Start after the top-left corner, on the top edge.
    sink.move_to(x + tl.width, y);
    // Top edge → top-right corner.
    sink.line_to(x + w - tr.width, y);
    sink.curve_to(
        x + w - tr.width + tr.width * KAPPA,
        y,
        x + w,
        y + tr.height - tr.height * KAPPA,
        x + w,
        y + tr.height,
    );
    // Right edge → bottom-right corner.
    sink.line_to(x + w, y + h - br.height);
    sink.curve_to(
        x + w,
        y + h - br.height + br.height * KAPPA,
        x + w - br.width + br.width * KAPPA,
        y + h,
        x + w - br.width,
        y + h,
    );
    // Bottom edge → bottom-left corner.
    sink.line_to(x + bl.width, y + h);
    sink.curve_to(
        x + bl.width - bl.width * KAPPA,
        y + h,
        x,
        y + h - bl.height + bl.height * KAPPA,
        x,
        y + h - bl.height,
    );
    // Left edge → top-left corner.
    sink.line_to(x, y + tl.height);
    sink.curve_to(
        x,
        y + tl.height - tl.height * KAPPA,
        x + tl.width - tl.width * KAPPA,
        y,
        x + tl.width,
        y,
    );
    sink.close();
}

/// Walks backend-neutral [`PathCommand`]s into `sink`. Quadratics are forwarded
/// to [`PathSink::quad_to`] with the current point, so sinks without a native
/// quadratic can elevate them to cubics. Shared by the raster and PDF glyph
/// backends (ADR-0007 D1).
pub fn emit_path_commands<S: PathSink + ?Sized>(sink: &mut S, commands: &[PathCommand]) {
    let mut cur = (0.0f32, 0.0f32);
    for cmd in commands {
        match *cmd {
            PathCommand::MoveTo { x, y } => {
                sink.move_to(x, y);
                cur = (x, y);
            }
            PathCommand::LineTo { x, y } => {
                sink.line_to(x, y);
                cur = (x, y);
            }
            PathCommand::QuadTo { cx, cy, x, y } => {
                sink.quad_to(cur, cx, cy, x, y);
                cur = (x, y);
            }
            PathCommand::CurveTo {
                c1x,
                c1y,
                c2x,
                c2y,
                x,
                y,
            } => {
                sink.curve_to(c1x, c1y, c2x, c2y, x, y);
                cur = (x, y);
            }
            PathCommand::Close => sink.close(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records emitted operations, for asserting the shared
    /// emitters produce the exact operator sequence the backends expect.
    #[derive(Default)]
    struct RecordSink(Vec<String>);

    impl PathSink for RecordSink {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("M {x:.1} {y:.1}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("L {x:.1} {y:.1}"));
        }
        fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
            self.0.push(format!(
                "C {c1x:.1} {c1y:.1} {c2x:.1} {c2y:.1} {x:.1} {y:.1}"
            ));
        }
        fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
            self.0.push(format!("R {x:.1} {y:.1} {w:.1} {h:.1}"));
        }
        fn close(&mut self) {
            self.0.push("Z".into());
        }
    }

    #[test]
    fn zero_radius_uses_native_rect() {
        let mut sink = RecordSink::default();
        emit_rounded_rect(
            &mut sink,
            Rect::from_xywh(1.0, 2.0, 3.0, 4.0),
            &BorderRadii::ZERO,
        );
        assert_eq!(sink.0, vec!["R 1.0 2.0 3.0 4.0"]);
    }

    #[test]
    fn rounded_rect_emits_four_corner_cubics() {
        let mut sink = RecordSink::default();
        let radii = BorderRadii::uniform(5.0);
        emit_rounded_rect(&mut sink, Rect::from_xywh(0.0, 0.0, 20.0, 20.0), &radii);
        // move + (line + curve) per corner + close.
        assert_eq!(sink.0.first().unwrap(), "M 5.0 0.0");
        assert_eq!(sink.0.last().unwrap(), "Z");
        assert_eq!(sink.0.iter().filter(|s| s.starts_with('C')).count(), 4);
        assert_eq!(sink.0.iter().filter(|s| s.starts_with('L')).count(), 4);
    }

    #[test]
    fn quad_default_elevates_to_cubic() {
        let mut sink = RecordSink::default();
        emit_path_commands(
            &mut sink,
            &[
                PathCommand::MoveTo { x: 0.0, y: 0.0 },
                PathCommand::QuadTo {
                    cx: 3.0,
                    cy: 3.0,
                    x: 6.0,
                    y: 0.0,
                },
                PathCommand::Close,
            ],
        );
        // Quadratic (0,0)-(3,3)-(6,0) elevates to cubic control points at
        // (2,2) and (4,2).
        assert_eq!(sink.0, vec!["M 0.0 0.0", "C 2.0 2.0 4.0 2.0 6.0 0.0", "Z"]);
    }
}
