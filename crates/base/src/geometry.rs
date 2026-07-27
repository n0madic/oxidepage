//! Minimal 2D geometry primitives (f32, CSS-pixel oriented).
//!
//! Hand-rolled rather than pulling in `euclid`: the engine needs points,
//! sizes, rects, and affine transforms — nothing that justifies a typed-unit
//! dependency in the base crate (design doc §5.1).

/// A 2D point.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    #[must_use]
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A 2D size. Negative dimensions are not prevented here; `Rect::is_empty`
/// treats them as empty.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };

    #[must_use]
    pub fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle: origin (top-left) plus size.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const ZERO: Self = Self {
        origin: Point::ZERO,
        size: Size::ZERO,
    };

    #[must_use]
    pub fn new(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    #[must_use]
    pub fn from_xywh(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self::new(Point::new(x, y), Size::new(width, height))
    }

    #[must_use]
    pub fn min_x(&self) -> f32 {
        self.origin.x
    }

    #[must_use]
    pub fn min_y(&self) -> f32 {
        self.origin.y
    }

    #[must_use]
    pub fn max_x(&self) -> f32 {
        self.origin.x + self.size.width
    }

    #[must_use]
    pub fn max_y(&self) -> f32 {
        self.origin.y + self.size.height
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        !(self.size.width > 0.0 && self.size.height > 0.0)
    }

    #[must_use]
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x() && p.x < self.max_x() && p.y >= self.min_y() && p.y < self.max_y()
    }

    #[must_use]
    pub fn translate(&self, dx: f32, dy: f32) -> Self {
        Self::from_xywh(
            self.origin.x + dx,
            self.origin.y + dy,
            self.size.width,
            self.size.height,
        )
    }

    /// Intersection; `None` when the rects do not overlap.
    #[must_use]
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.min_x().max(other.min_x());
        let y0 = self.min_y().max(other.min_y());
        let x1 = self.max_x().min(other.max_x());
        let y1 = self.max_y().min(other.max_y());
        (x1 > x0 && y1 > y0).then(|| Rect::from_xywh(x0, y0, x1 - x0, y1 - y0))
    }

    /// Smallest rect containing both. Empty rects do not contribute.
    #[must_use]
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x0 = self.min_x().min(other.min_x());
        let y0 = self.min_y().min(other.min_y());
        let x1 = self.max_x().max(other.max_x());
        let y1 = self.max_y().max(other.max_y());
        Rect::from_xywh(x0, y0, x1 - x0, y1 - y0)
    }
}

/// A 2D affine transform in row-major `[a b c d tx ty]` form:
///
/// ```text
/// | a  c  tx |   | x |
/// | b  d  ty | * | y |
/// | 0  0  1  |   | 1 |
/// ```
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Transform2D {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Default for Transform2D {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform2D {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    #[must_use]
    pub fn translation(tx: f32, ty: f32) -> Self {
        Self {
            tx,
            ty,
            ..Self::IDENTITY
        }
    }

    #[must_use]
    pub fn scale(sx: f32, sy: f32) -> Self {
        Self {
            a: sx,
            d: sy,
            ..Self::IDENTITY
        }
    }

    /// `self` applied *before* `other` (the matrix product `other × self`), so
    /// `a.then(&b).apply(p) == b.apply(a.apply(p))` — the euclid/kurbo meaning of
    /// `then`.
    #[must_use]
    pub fn then(&self, other: &Transform2D) -> Self {
        Self {
            a: other.a * self.a + other.c * self.b,
            b: other.b * self.a + other.d * self.b,
            c: other.a * self.c + other.c * self.d,
            d: other.b * self.c + other.d * self.d,
            tx: other.a * self.tx + other.c * self.ty + other.tx,
            ty: other.b * self.tx + other.d * self.ty + other.ty,
        }
    }

    #[must_use]
    pub fn apply(&self, p: Point) -> Point {
        Point::new(
            self.a * p.x + self.c * p.y + self.tx,
            self.b * p.x + self.d * p.y + self.ty,
        )
    }

    /// Re-expresses a matrix given in a box's **local** space (its border-box
    /// top-left at the origin) in the space that box sits in:
    /// `translate(-origin) ∘ self ∘ translate(origin)`.
    ///
    /// A transform resolved against a local border box and one resolved against
    /// the same box's absolute border box differ by exactly this conjugation,
    /// which is what lets layout cache one matrix per transformed box and hand
    /// out either (`layout::transform`).
    #[must_use]
    pub fn at_origin(&self, origin: Point) -> Transform2D {
        Self::translation(-origin.x, -origin.y)
            .then(self)
            .then(&Self::translation(origin.x, origin.y))
    }

    /// The inverse transform, or `None` when the matrix is singular
    /// (`scale(0)`, `scaleX(0)`, a degenerate `matrix()`): such a box collapses
    /// to a line or a point and is not hit-testable, which is what browsers do.
    /// A non-finite determinant is treated as singular for the same reason.
    ///
    /// Singularity is judged **relative to the matrix's own magnitude**. An
    /// absolute threshold declares `scale(0.0003)` singular — its determinant is
    /// 9e-8, below `f32::EPSILON` — and a legitimately tiny but perfectly
    /// invertible box would become un-hit-testable along with everything inside
    /// it.
    #[must_use]
    pub fn inverse(&self) -> Option<Transform2D> {
        let det = self.a * self.d - self.b * self.c;
        let scale = self
            .a
            .abs()
            .max(self.b.abs())
            .max(self.c.abs())
            .max(self.d.abs());
        if !det.is_finite() || det.abs() <= f32::EPSILON * scale * scale {
            return None;
        }
        Some(Self {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            tx: (self.c * self.ty - self.d * self.tx) / det,
            ty: (self.b * self.tx - self.a * self.ty) / det,
        })
    }

    /// The four corners of `rect` mapped through this transform, in order
    /// top-left, top-right, bottom-right, bottom-left. A rotation or a skew
    /// turns the rect into a genuine quadrilateral, which is why the corners
    /// are reported rather than a rect (CSSOM-View `DOMQuad`, CDP's
    /// `DOM.getContentQuads`).
    #[must_use]
    pub fn map_quad(&self, rect: Rect) -> [Point; 4] {
        [
            self.apply(Point::new(rect.min_x(), rect.min_y())),
            self.apply(Point::new(rect.max_x(), rect.min_y())),
            self.apply(Point::new(rect.max_x(), rect.max_y())),
            self.apply(Point::new(rect.min_x(), rect.max_y())),
        ]
    }

    /// The axis-aligned bounding box of [`Self::map_quad`] — what a `DOMRect`
    /// reports for a transformed box.
    #[must_use]
    pub fn map_rect(&self, rect: Rect) -> Rect {
        let quad = self.map_quad(rect);
        let (mut min_x, mut min_y) = (quad[0].x, quad[0].y);
        let (mut max_x, mut max_y) = (quad[0].x, quad[0].y);
        for p in &quad[1..] {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        Rect::from_xywh(min_x, min_y, max_x - min_x, max_y - min_y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_intersection_and_union() {
        let a = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
        let b = Rect::from_xywh(5.0, 5.0, 10.0, 10.0);
        assert_eq!(
            a.intersection(&b),
            Some(Rect::from_xywh(5.0, 5.0, 5.0, 5.0))
        );
        assert_eq!(a.union(&b), Rect::from_xywh(0.0, 0.0, 15.0, 15.0));
        let far = Rect::from_xywh(100.0, 100.0, 1.0, 1.0);
        assert_eq!(a.intersection(&far), None);
    }

    #[test]
    fn transform_compose_and_apply() {
        let translate = Transform2D::translation(10.0, 0.0);
        let scale = Transform2D::scale(2.0, 2.0);
        let t = translate.then(&scale);
        // Translate first, then scale: (1,1) -> (11,1) -> (22,2).
        assert_eq!(t.apply(Point::new(1.0, 1.0)), Point::new(22.0, 2.0));
        // `a.then(&b)` is exactly "apply a, then apply b".
        assert_eq!(
            t.apply(Point::new(1.0, 1.0)),
            scale.apply(translate.apply(Point::new(1.0, 1.0))),
            "then() composes left-to-right"
        );
        // Composition is not commutative: scaling first gives (12, 2).
        assert_eq!(
            scale.then(&translate).apply(Point::new(1.0, 1.0)),
            Point::new(12.0, 2.0)
        );
    }

    /// A quarter turn clockwise about the origin (CSS `rotate(90deg)`: +x → +y).
    fn rotate_90() -> Transform2D {
        Transform2D {
            a: 0.0,
            b: 1.0,
            c: -1.0,
            d: 0.0,
            tx: 0.0,
            ty: 0.0,
        }
    }

    fn assert_near(a: Point, b: Point) {
        assert!(
            (a.x - b.x).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4,
            "{a:?} != {b:?}"
        );
    }

    #[test]
    fn inverse_round_trips_a_point() {
        let t = Transform2D::translation(10.0, -5.0)
            .then(&Transform2D::scale(2.0, 4.0))
            .then(&rotate_90());
        let inverse = t.inverse().expect("invertible");
        let p = Point::new(3.0, 7.0);
        assert_near(inverse.apply(t.apply(p)), p);
        assert_near(t.apply(inverse.apply(p)), p);
    }

    #[test]
    fn inverse_of_identity_is_identity() {
        assert_eq!(Transform2D::IDENTITY.inverse(), Some(Transform2D::IDENTITY));
    }

    #[test]
    fn singular_matrices_have_no_inverse() {
        // `scale(0)` and a single collapsed axis both flatten the plane.
        assert_eq!(Transform2D::scale(0.0, 0.0).inverse(), None);
        assert_eq!(Transform2D::scale(0.0, 3.0).inverse(), None);
        assert_eq!(Transform2D::scale(3.0, 0.0).inverse(), None);
    }

    #[test]
    fn a_tiny_but_invertible_matrix_keeps_its_inverse() {
        // `scale(0.0003)` has determinant 9e-8 — below `f32::EPSILON`, and yet
        // perfectly invertible. An absolute threshold made it, and everything
        // inside it, un-hit-testable.
        let tiny = Transform2D::scale(0.0003, 0.0003);
        let inverse = tiny.inverse().expect("invertible");
        assert_near(
            inverse.apply(tiny.apply(Point::new(4.0, 9.0))),
            Point::new(4.0, 9.0),
        );
    }

    #[test]
    fn at_origin_matches_resolving_against_the_moved_box() {
        // A box at (100, 50) scaled ×2 about its own top-left. Resolved locally
        // it is a plain scale; re-expressed at the box's origin it must keep
        // that corner fixed and move the far corner by the same factor.
        let local = Transform2D::scale(2.0, 2.0);
        let at = local.at_origin(Point::new(100.0, 50.0));
        assert_near(at.apply(Point::new(100.0, 50.0)), Point::new(100.0, 50.0));
        assert_near(at.apply(Point::new(110.0, 70.0)), Point::new(120.0, 90.0));
        // The identity is unmoved by conjugation.
        assert_eq!(
            Transform2D::IDENTITY.at_origin(Point::new(7.0, 9.0)),
            Transform2D::IDENTITY
        );
    }

    #[test]
    fn map_quad_reports_corners_in_tl_tr_br_bl_order() {
        let rect = Rect::from_xywh(10.0, 20.0, 30.0, 40.0);
        let quad = Transform2D::IDENTITY.map_quad(rect);
        assert_eq!(
            quad,
            [
                Point::new(10.0, 20.0),
                Point::new(40.0, 20.0),
                Point::new(40.0, 60.0),
                Point::new(10.0, 60.0),
            ]
        );

        // Rotated a quarter turn about the origin, the top-left corner (10, 20)
        // lands at (-20, 10) and the corner order rotates with the box.
        let quad = rotate_90().map_quad(rect);
        assert_near(quad[0], Point::new(-20.0, 10.0));
        assert_near(quad[1], Point::new(-20.0, 40.0));
        assert_near(quad[2], Point::new(-60.0, 40.0));
        assert_near(quad[3], Point::new(-60.0, 10.0));
    }

    #[test]
    fn map_rect_is_the_bounding_box_of_the_quad() {
        // A 30×40 box rotated 90° has a 40×30 bounding box.
        let rect = Rect::from_xywh(10.0, 20.0, 30.0, 40.0);
        let bounds = rotate_90().map_rect(rect);
        assert!((bounds.origin.x - -60.0).abs() < 1e-4);
        assert!((bounds.origin.y - 10.0).abs() < 1e-4);
        assert!((bounds.size.width - 40.0).abs() < 1e-4);
        assert!((bounds.size.height - 30.0).abs() < 1e-4);

        // An axis-aligned scale keeps it a rect.
        assert_eq!(
            Transform2D::scale(2.0, 0.5).map_rect(rect),
            Rect::from_xywh(20.0, 10.0, 60.0, 20.0)
        );
    }
}
