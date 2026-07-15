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
}
