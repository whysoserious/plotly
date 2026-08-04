//! Text → polylines in millimetres, using the built-in single-stroke font.
//! DESIGN.org §7 / step 4.1.
//!
//! The output is the same shape as a parsed SVG (polylines in the logical
//! y-down frame), so it feeds the identical draw pipeline — fit to field, build
//! a plan, draw.

use crate::fonts::{self, ADVANCE_UNITS, CAP_UNITS};
use crate::geometry::{Point, Polyline};

/// Lay out `text` at `cap_height_mm` (baseline-to-cap height), returning pen
/// strokes in logical millimetres. The baseline is y=0 and the text grows
/// upward (negative y), starting at x=0.
pub fn layout(text: &str, cap_height_mm: f64) -> Vec<Polyline> {
    let scale = cap_height_mm / CAP_UNITS;
    let mut polylines = Vec::new();
    let mut cursor_x = 0.0;

    for c in text.chars() {
        if let Some(strokes) = fonts::strokes(c) {
            for stroke in strokes {
                let polyline: Polyline = stroke
                    .iter()
                    .map(|&(gx, gy)| {
                        Point::new(
                            (cursor_x + f64::from(gx)) * scale,
                            // Grid y is up from the baseline; the logical frame
                            // is y-down, so the cap top lands at negative y.
                            -f64::from(gy) * scale,
                        )
                    })
                    .collect();
                polylines.push(polyline);
            }
            cursor_x += ADVANCE_UNITS;
        } else {
            // Unknown glyph: leave a blank of one advance so spacing is stable.
            cursor_x += ADVANCE_UNITS;
        }
    }
    polylines
}

/// Width of `text` in millimetres at `cap_height_mm` (advance of every char).
pub fn width_mm(text: &str, cap_height_mm: f64) -> f64 {
    let scale = cap_height_mm / CAP_UNITS;
    text.chars().count() as f64 * ADVANCE_UNITS * scale
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vertical extent (mm) of a set of polylines.
    fn height(polylines: &[Polyline]) -> f64 {
        let ys: Vec<f64> = polylines.iter().flatten().map(|p| p.y).collect();
        let max = ys.iter().cloned().fold(f64::MIN, f64::max);
        let min = ys.iter().cloned().fold(f64::MAX, f64::min);
        max - min
    }

    #[test]
    fn a_glyph_reaches_the_requested_cap_height() {
        let polylines = layout("A", 10.0);
        assert!(!polylines.is_empty());
        // "A" spans the full cap height; allow a hair of rounding.
        assert!(
            (height(&polylines) - 10.0).abs() < 1e-9,
            "{}",
            height(&polylines)
        );
    }

    #[test]
    fn cap_height_scales_the_drawing() {
        let small = height(&layout("H", 5.0));
        let big = height(&layout("H", 20.0));
        assert!((small - 5.0).abs() < 1e-9);
        assert!((big - 20.0).abs() < 1e-9);
    }

    #[test]
    fn text_advances_left_to_right_without_overlap() {
        // Two H's: the second starts a full advance to the right of the first.
        let polylines = layout("HH", 7.0);
        let max_x = polylines
            .iter()
            .flatten()
            .map(|p| p.x)
            .fold(f64::MIN, f64::max);
        // Advance is 6 units → 6 mm at cap 7; second H's right edge ~ 6 + 4 = 10.
        assert!(max_x > 6.0, "second glyph did not advance: {max_x}");
        assert!((width_mm("HH", 7.0) - 12.0).abs() < 1e-9);
    }

    #[test]
    fn a_space_leaves_a_gap_and_draws_nothing() {
        // "A A" has the same stroke count as "AA" (space draws nothing).
        assert_eq!(layout("A A", 10.0).len(), layout("AA", 10.0).len());
        // …but is wider by one advance.
        assert!(width_mm("A A", 10.0) > width_mm("AA", 10.0));
    }

    #[test]
    fn text_grows_upward_from_the_baseline() {
        // Baseline is y=0; the glyph is above it, so all y ≤ 0.
        let polylines = layout("E", 10.0);
        assert!(polylines.iter().flatten().all(|p| p.y <= 1e-9));
    }
}
