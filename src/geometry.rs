//! Coordinate transform between the logical drawing frame and machine wire
//! coordinates. DESIGN.org §2.3 / §2.4.
//!
//! Two frames:
//! - **logical** — millimetres in the drawing, SVG convention: +X right, +Y
//!   *down* the page.
//! - **wire** — what goes into a G-code word. On this machine (spike 0.7) that
//!   is also millimetres, +X right, +Y *away* from the operator.
//!
//! The spike reduced the feared axis transform (§2.3: swap, mirror, step units,
//! CoreXY) to a trivial linear map: no swap, unit scale, and a single sign flip
//! on Y (logical "down the page" is wire "towards the operator"). This type
//! holds that linear part `L`; step 2.2 adds the translation for absolute
//! points on top of the same `L`.

/// A point in millimetres. Used for logical (drawing) coordinates throughout;
/// the wire mapping is done by [`Transform`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// An open or closed run of connected points (a flattened path). Closedness is
/// implicit: a closed subpath ends with a point equal to its first.
pub type Polyline = Vec<Point>;

/// The machine's drawable area, in machine-logical millimetres: `(0,0)` is the
/// home corner, `+X` right, `+Y` down the page (§2.3). Sizes come from `$130`/
/// `$131` (§15.1); step 5.1 will read them per profile.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub width_mm: f64,
    pub height_mm: f64,
}

impl Field {
    /// Our A0 iDraw: 841 × 1189 mm (`$130`/`$131`, §15.1).
    pub fn idraw_a0() -> Self {
        Self {
            width_mm: 841.0,
            height_mm: 1189.0,
        }
    }

    /// Whether a machine-logical point is inside the drawable area.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= 0.0 && p.x <= self.width_mm && p.y >= 0.0 && p.y <= self.height_mm
    }
}

/// Places a drawing into the machine field: a uniform scale then a translation,
/// mapping drawing-logical mm to machine-logical mm. Kept separate from
/// [`Transform`] so the fit scale never contaminates the measured axis map `L`
/// (§2.3) — `map_vector` stays the exact linear part of `map_point`.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    scale: f64,
    offset: Point,
}

impl Placement {
    /// Identity: drawing coordinates are already machine coordinates.
    pub fn identity() -> Self {
        Self {
            scale: 1.0,
            offset: Point::new(0.0, 0.0),
        }
    }

    /// Fit a drawing with `bounds` into `field`, leaving `margin_mm` on each
    /// side, and centre it. Never enlarges: a drawing that already fits keeps
    /// its intended physical size (scale ≤ 1).
    pub fn fit(bounds: (Point, Point), field: &Field, margin_mm: f64) -> Self {
        let (min, max) = bounds;
        let (dw, dh) = (max.x - min.x, max.y - min.y);
        let scale = fit_scale(bounds, field, margin_mm);

        // Centre the scaled drawing in the field.
        let offset = Point::new(
            (field.width_mm - dw * scale) / 2.0 - min.x * scale,
            (field.height_mm - dh * scale) / 2.0 - min.y * scale,
        );
        Self { scale, offset }
    }

    /// Place a drawing with `bounds` so that its top-left corner lands on `at`,
    /// at the same scale [`Placement::fit`] would choose.
    ///
    /// This is what "start drawing where the head is" means: the operator jogs
    /// to the corner of their sheet and the drawing grows from there. Centring
    /// in the field is the wrong default on this machine — the field is a whole
    /// A0 (§15.1) and the paper on it is usually much smaller, so a centred
    /// drawing lands on bare table.
    ///
    /// Nothing here keeps the result inside the field: an anchor near the far
    /// edge can push the drawing off it. The caller checks and warns — clipping
    /// is the host's job (§2.4) and belongs with the operator's own eyes on the
    /// machine, not with a silent shrink.
    pub fn anchored_at(bounds: (Point, Point), field: &Field, margin_mm: f64, at: Point) -> Self {
        let scale = fit_scale(bounds, field, margin_mm);
        let min = bounds.0;
        Self {
            scale,
            offset: Point::new(at.x - min.x * scale, at.y - min.y * scale),
        }
    }

    /// Map a drawing-logical point to a machine-logical point.
    pub fn place(&self, p: Point) -> Point {
        Point::new(
            p.x * self.scale + self.offset.x,
            p.y * self.scale + self.offset.y,
        )
    }

    /// Bounds after placement, for logging and bounds checks.
    pub fn place_bounds(&self, bounds: (Point, Point)) -> (Point, Point) {
        (self.place(bounds.0), self.place(bounds.1))
    }
}

/// The scale that makes `bounds` fit `field` with `margin_mm` on each side,
/// never enlarging: a drawing that already fits keeps its intended size.
fn fit_scale(bounds: (Point, Point), field: &Field, margin_mm: f64) -> f64 {
    let (min, max) = bounds;
    let (dw, dh) = (max.x - min.x, max.y - min.y);
    let avail_w = (field.width_mm - 2.0 * margin_mm).max(0.0);
    let avail_h = (field.height_mm - 2.0 * margin_mm).max(0.0);

    // Constrain by each dimension that is actually present; a zero-extent axis
    // (a horizontal or vertical line) imposes no limit of its own.
    let sx = if dw > 0.0 {
        avail_w / dw
    } else {
        f64::INFINITY
    };
    let sy = if dh > 0.0 {
        avail_h / dh
    } else {
        f64::INFINITY
    };
    sx.min(sy).min(1.0)
}

/// Linear part of the logical↔wire map (`L`) plus the machine origin: a point
/// maps as `wire = L·logical + t`, a vector as `L·delta` (t drops out).
#[derive(Debug, Clone, Copy)]
pub struct Transform {
    /// Whether wire X comes from logical Y and vice versa. Kept for other iDraw
    /// models even though ours does not swap — the reference driver did (§2.3).
    swap_xy: bool,
    /// Wire units per logical mm along (post-swap) X, sign included.
    x_scale: f64,
    /// Wire units per logical mm along (post-swap) Y, sign included.
    y_scale: f64,
    /// Machine origin in wire mm (`t`). Zero after `$H`, since the home corner
    /// is the machine origin on this firmware (§2.4).
    origin: Point,
}

impl Transform {
    /// The transform measured on our iDraw 2.0 in spike 0.7 (§2.3):
    /// `x_wire = x_logical`, `y_wire = -y_logical`, 1 mm to 1 unit, no swap,
    /// origin at the home corner.
    pub fn idraw() -> Self {
        Self {
            swap_xy: false,
            x_scale: 1.0,
            y_scale: -1.0,
            origin: Point::new(0.0, 0.0),
        }
    }

    /// The same transform with a different machine origin (wire mm).
    pub fn with_origin(mut self, origin: Point) -> Self {
        self.origin = origin;
        self
    }

    /// Map a logical delta (mm) to a wire delta. The translation (origin) drops
    /// out of a difference, so a *displacement* needs only the linear part —
    /// which is exactly what a relative jog is.
    pub fn map_vector(&self, dx: f64, dy: f64) -> (f64, f64) {
        let (dx, dy) = if self.swap_xy { (dy, dx) } else { (dx, dy) };
        (dx * self.x_scale, dy * self.y_scale)
    }

    /// Map an absolute machine-logical point (mm) to a wire point. This is
    /// `map_vector` plus the machine origin, so the two stay consistent (§2.2).
    pub fn map_point(&self, p: Point) -> Point {
        let (dx, dy) = self.map_vector(p.x, p.y);
        Point::new(dx + self.origin.x, dy + self.origin.y)
    }

    /// Inverse of [`Transform::map_point`]: wire back to machine-logical. Used
    /// to prove the mapping is lossless (round-trip identity).
    pub fn unmap_point(&self, w: Point) -> Point {
        let (wx, wy) = (w.x - self.origin.x, w.y - self.origin.y);
        // Invert the linear part: undo the scale, then the swap.
        let (a, b) = (wx / self.x_scale, wy / self.y_scale);
        if self.swap_xy {
            Point::new(b, a)
        } else {
            Point::new(a, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire deltas round to the precision we send on the wire, so compare there.
    fn map(t: &Transform, dx: f64, dy: f64) -> (f64, f64) {
        let (x, y) = t.map_vector(dx, dy);
        ((x * 1000.0).round() / 1000.0, (y * 1000.0).round() / 1000.0)
    }

    #[test]
    fn idraw_keeps_x_and_flips_y() {
        let t = Transform::idraw();
        // Right stays right; "up the page" (logical -Y) goes to wire +Y (away).
        assert_eq!(map(&t, 10.0, 0.0), (10.0, 0.0));
        assert_eq!(map(&t, -10.0, 0.0), (-10.0, 0.0));
        assert_eq!(map(&t, 0.0, 10.0), (0.0, -10.0));
        assert_eq!(map(&t, 0.0, -10.0), (0.0, 10.0));
    }

    #[test]
    fn idraw_is_unit_scale() {
        let t = Transform::idraw();
        assert_eq!(map(&t, 0.1, 0.1), (0.1, -0.1));
        assert_eq!(map(&t, 5.0, -2.5), (5.0, 2.5));
    }

    #[test]
    fn zero_maps_to_zero() {
        assert_eq!(Transform::idraw().map_vector(0.0, 0.0), (0.0, 0.0));
    }

    /// A hypothetical swap+mirror machine (the reference driver's path, §2.3):
    /// wire X from logical Y, wire Y from logical X, both negated.
    #[test]
    fn swap_and_signs_compose_as_expected() {
        let t = Transform {
            swap_xy: true,
            x_scale: -1.0,
            y_scale: -1.0,
            origin: Point::new(0.0, 0.0),
        };
        // logical (dx, dy) -> swap -> (dy, dx) -> scale -> (-dy, -dx)
        assert_eq!(map(&t, 3.0, 7.0), (-7.0, -3.0));
    }

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
    }

    #[test]
    fn known_point_maps_to_the_spike_oracle() {
        // Machine-logical (10, 10) → wire (10, -10) on our iDraw (§2.3).
        let t = Transform::idraw();
        assert!(close(
            t.map_point(Point::new(10.0, 10.0)),
            Point::new(10.0, -10.0)
        ));
    }

    #[test]
    fn map_point_is_map_vector_plus_origin() {
        // With a non-zero origin, differences must still equal map_vector — this
        // is the consistency the /Auto:/ test asks for (L shared, t cancels).
        let t = Transform::idraw().with_origin(Point::new(100.0, -50.0));
        let a = Point::new(30.0, 20.0);
        let b = Point::new(12.0, 7.0);
        let (wa, wb) = (t.map_point(a), t.map_point(b));
        let diff = (wa.x - wb.x, wa.y - wb.y);
        assert_eq!(diff, t.map_vector(a.x - b.x, a.y - b.y));
    }

    #[test]
    fn map_then_unmap_is_identity() {
        for t in [
            Transform::idraw(),
            Transform::idraw().with_origin(Point::new(841.0, 0.0)),
            Transform {
                swap_xy: true,
                x_scale: -1.27,
                y_scale: 1.27,
                origin: Point::new(5.0, -3.0),
            },
        ] {
            for p in [
                Point::new(0.0, 0.0),
                Point::new(120.5, 80.0),
                Point::new(-7.0, 42.0),
            ] {
                assert!(
                    close(t.unmap_point(t.map_point(p)), p),
                    "round-trip failed for {p:?}"
                );
            }
        }
    }

    #[test]
    fn fit_shrinks_a_large_drawing_and_centres_it() {
        let field = Field::idraw_a0(); // 841 × 1189
                                       // A 1682 mm wide drawing must shrink to fit the 831 mm usable width.
        let bounds = (Point::new(0.0, 0.0), Point::new(1682.0, 0.0));
        let placement = Placement::fit(bounds, &field, 5.0);
        let (min, max) = placement.place_bounds(bounds);
        assert!(min.x >= 5.0 - 1e-9 && max.x <= field.width_mm - 5.0 + 1e-9);
        // Centred: equal margins left and right.
        assert!((min.x - (field.width_mm - max.x)).abs() < 1e-6);
    }

    #[test]
    fn fit_never_enlarges_a_small_drawing() {
        let field = Field::idraw_a0();
        let bounds = (Point::new(0.0, 0.0), Point::new(10.0, 10.0));
        let placement = Placement::fit(bounds, &field, 5.0);
        let (min, max) = placement.place_bounds(bounds);
        // Kept at 10×10 mm (scale 1), just moved to the centre.
        assert!((max.x - min.x - 10.0).abs() < 1e-6);
        assert!((max.y - min.y - 10.0).abs() < 1e-6);
    }

    /// The head is where the drawing starts: its top-left corner lands exactly
    /// on the anchor, whatever the drawing's own coordinates were.
    #[test]
    fn anchoring_puts_the_drawings_corner_on_the_head() {
        let field = Field::idraw_a0();
        // Bounds that do not start at the origin, so an offset bug shows up.
        let bounds = (Point::new(-30.0, 12.0), Point::new(70.0, 62.0));
        let at = Point::new(120.0, 400.0);

        let placement = Placement::anchored_at(bounds, &field, 5.0, at);
        let (min, max) = placement.place_bounds(bounds);

        assert!(close(min, at), "corner landed at {min:?}, not {at:?}");
        // Unscaled, so the drawing keeps its 100 × 50 mm size.
        assert!((max.x - min.x - 100.0).abs() < 1e-6);
        assert!((max.y - min.y - 50.0).abs() < 1e-6);
    }

    /// Anchoring uses the same shrink-to-fit scale as centring, so an oversized
    /// drawing is still plottable — it just starts at the head.
    #[test]
    fn anchoring_shrinks_an_oversized_drawing_like_fit_does() {
        let field = Field::idraw_a0();
        let bounds = (Point::new(0.0, 0.0), Point::new(1682.0, 0.0));
        let at = Point::new(10.0, 20.0);

        let anchored = Placement::anchored_at(bounds, &field, 5.0, at);
        let centred = Placement::fit(bounds, &field, 5.0);
        assert!((anchored.scale - centred.scale).abs() < 1e-12);

        let (min, max) = anchored.place_bounds(bounds);
        assert!(close(min, at));
        assert!((max.x - min.x) <= field.width_mm - 10.0 + 1e-9);
    }

    /// Placement must not silently move a drawing back inside the field: an
    /// anchor near the edge pushes it off, and the caller warns (§2.4).
    #[test]
    fn an_anchor_near_the_edge_leaves_the_drawing_hanging_off() {
        let field = Field::idraw_a0();
        let bounds = (Point::new(0.0, 0.0), Point::new(100.0, 100.0));
        let at = Point::new(field.width_mm - 10.0, 0.0);

        let (_, max) = Placement::anchored_at(bounds, &field, 5.0, at).place_bounds(bounds);
        assert!(!field.contains(max), "expected {max:?} to be off the field");
    }

    #[test]
    fn a_point_outside_the_field_is_detected() {
        let field = Field::idraw_a0();
        assert!(field.contains(Point::new(0.0, 0.0)));
        assert!(field.contains(Point::new(841.0, 1189.0)));
        assert!(!field.contains(Point::new(900.0, 100.0)));
        assert!(!field.contains(Point::new(100.0, -1.0)));
    }
}
