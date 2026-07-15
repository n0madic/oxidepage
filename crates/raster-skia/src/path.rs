//! Path construction: (possibly rounded) rectangles as `tiny_skia::Path`.

use oxidepage_base::Rect;
use oxidepage_paint::{BorderRadii, PathSink, emit_rounded_rect};
use tiny_skia::{Path, PathBuilder};

/// A [`PathSink`] over a tiny_skia `PathBuilder`, so the shared rounded-rect and
/// glyph-path emitters in `oxidepage-paint` build `tiny_skia::Path`s. Unlike the
/// PDF sink it has a native quadratic and rectangle primitive.
pub(crate) struct PathBuilderSink(pub(crate) PathBuilder);

impl PathSink for PathBuilderSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.0.cubic_to(c1x, c1y, c2x, c2y, x, y);
    }
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
            self.0.push_rect(r);
        }
    }
    fn close(&mut self) {
        self.0.close();
    }
    fn quad_to(&mut self, _cur: (f32, f32), cx: f32, cy: f32, x: f32, y: f32) {
        self.0.quad_to(cx, cy, x, y);
    }
}

/// Builds a rounded-rectangle path. Radii are clamped so adjacent corners
/// never overlap. Returns `None` for empty rects. The outline itself is emitted
/// by the shared [`emit_rounded_rect`], so it stays geometry-identical with the
/// PDF backend.
pub(crate) fn rounded_rect(rect: Rect, radii: &BorderRadii) -> Option<Path> {
    if rect.size.width <= 0.0 || rect.size.height <= 0.0 {
        return None;
    }
    let mut sink = PathBuilderSink(PathBuilder::new());
    emit_rounded_rect(&mut sink, rect, radii);
    sink.0.finish()
}

/// Builds a simple quadrilateral (border-edge trapezoid) path.
pub(crate) fn quad(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(p0.0, p0.1);
    pb.line_to(p1.0, p1.1);
    pb.line_to(p2.0, p2.1);
    pb.line_to(p3.0, p3.1);
    pb.close();
    pb.finish()
}
