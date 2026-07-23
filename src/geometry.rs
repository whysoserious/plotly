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
#[derive(Debug, Clone, Copy, PartialEq)]
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

/// Linear part of the logical↔wire map (`L`): optional axis swap, then a
/// per-axis scale carrying both the unit factor and the sign.
#[derive(Debug, Clone, Copy)]
pub struct Transform {
    /// Whether wire X comes from logical Y and vice versa. Kept for other iDraw
    /// models even though ours does not swap — the reference driver did (§2.3).
    swap_xy: bool,
    /// Wire units per logical mm along (post-swap) X, sign included.
    x_scale: f64,
    /// Wire units per logical mm along (post-swap) Y, sign included.
    y_scale: f64,
}

impl Transform {
    /// The transform measured on our iDraw 2.0 in spike 0.7 (§2.3):
    /// `x_wire = x_logical`, `y_wire = -y_logical`, 1 mm to 1 unit, no swap.
    pub fn idraw() -> Self {
        Self {
            swap_xy: false,
            x_scale: 1.0,
            y_scale: -1.0,
        }
    }

    /// Map a logical delta (mm) to a wire delta. The translation (origin) drops
    /// out of a difference, so a *displacement* needs only the linear part —
    /// which is exactly what a relative jog is.
    pub fn map_vector(&self, dx: f64, dy: f64) -> (f64, f64) {
        let (dx, dy) = if self.swap_xy { (dy, dx) } else { (dx, dy) };
        (dx * self.x_scale, dy * self.y_scale)
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
        };
        // logical (dx, dy) -> swap -> (dy, dx) -> scale -> (-dy, -dx)
        assert_eq!(map(&t, 3.0, 7.0), (-7.0, -3.0));
    }
}
