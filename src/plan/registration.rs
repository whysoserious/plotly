//! Registration marks: a way for the paper to say whether the machine lost its
//! place during a plot, and when. DESIGN.org §2.11.
//!
//! The host cannot see a slip. Steppers run open loop, so a lost step or a belt
//! that jumps a tooth leaves every later stroke displaced while the log reads
//! exactly as if nothing happened. On a hatched drawing the displaced lines
//! land on lines drawn before the slip and look *missing*: the job of
//! 2026-10-01 lost its place twice, and about 150 hatch slots came out empty.
//!
//! So every so often (by the time estimate) the plan draws a mark at a fixed
//! point in the drawing's frame:
//!
//! - a **cross**, the same one every time. As long as the machine knows where it
//!   is, every cross lands on the first. After a slip the new ones land beside
//!   it, displaced by exactly the slip: one cross per position the machine has
//!   been in, and the offset between them, both axes, under a loupe.
//! - a **tick**, one per mark, in a row to the right of the cross. This is the
//!   clock: the row steps (up or down, or a gap that is not the pitch) between
//!   the last tick before a slip and the first one after it. Each tick's stroke
//!   number and wall-clock time are in the log, so the slip is pinned to a
//!   window of strokes and to the lines on the wire around them.
//!
//! Marks are ordinary shapes, labelled `registration <n>`, interleaved with the
//! drawing before the plan is built. They resume, count and preview like
//! everything else.

use crate::geometry::Point;
use crate::plan::Shape;

/// Label prefix of every mark stroke; the full label is `registration <n>`.
pub const LABEL_PREFIX: &str = "registration";

/// Half the length of each arm of the cross, mm.
const ARM_MM: f64 = 1.5;
/// Half the height of a tick, mm.
const TICK_HALF_MM: f64 = 1.5;
/// From the centre of the cross to the first tick, mm.
const FIRST_TICK_MM: f64 = 3.0;
/// Between ticks, mm. Three pen widths: a 1 mm slip is half a pitch out of
/// step either way, and nothing lands exactly on a neighbour by accident.
const TICK_PITCH_MM: f64 = 1.5;

/// Whether a stroke label belongs to a registration mark.
pub fn is_mark(label: Option<&str>) -> bool {
    label.is_some_and(|label| label.starts_with(LABEL_PREFIX))
}

/// The strokes of mark number `n` with its cross centred on `at` (drawing mm,
/// +y down): the cross's two arms, then the tick.
pub fn mark(n: usize, at: Point) -> [Shape; 3] {
    let label = || Some(format!("{LABEL_PREFIX} {n}"));
    let x = at.x + FIRST_TICK_MM + n as f64 * TICK_PITCH_MM;
    [
        Shape::new(
            label(),
            vec![
                Point::new(at.x - ARM_MM, at.y),
                Point::new(at.x + ARM_MM, at.y),
            ],
        ),
        Shape::new(
            label(),
            vec![
                Point::new(at.x, at.y - ARM_MM),
                Point::new(at.x, at.y + ARM_MM),
            ],
        ),
        Shape::new(
            label(),
            vec![
                Point::new(x, at.y - TICK_HALF_MM),
                Point::new(x, at.y + TICK_HALF_MM),
            ],
        ),
    ]
}

/// `shapes` with a mark before the first of them, another before the first
/// stroke due `every_secs` after the previous mark, and a last one after the
/// end.
///
/// `starts` is when each stroke begins, from
/// [`crate::plan::estimate::stroke_start_secs`] on a plan built from these
/// same shapes: one entry per shape that has points (an empty shape makes no
/// stroke). A stroke longer than an interval delays the next mark rather than
/// earning several in a row — a burst of ticks at one moment would read as
/// time that never passed.
pub fn interleave(shapes: &[Shape], starts: &[f64], every_secs: f64, at: Point) -> Vec<Shape> {
    let mut out = Vec::with_capacity(shapes.len() + 3 * (starts.len() / 64 + 2));
    let mut next = 0;
    out.extend(mark(next, at));
    next += 1;

    let mut stroke = 0;
    for shape in shapes {
        if !shape.points.is_empty() {
            if starts
                .get(stroke)
                .is_some_and(|&secs| secs >= next as f64 * every_secs)
            {
                out.extend(mark(next, at));
                next += 1;
            }
            stroke += 1;
        }
        out.push(shape.clone());
    }
    out.extend(mark(next, at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(x: f64) -> Shape {
        Shape::unlabelled(vec![Point::new(x, 0.0), Point::new(x, 10.0)])
    }

    fn marks_in(shapes: &[Shape]) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for shape in shapes {
            if let Some(label) = shape.label.as_deref().filter(|l| is_mark(Some(l))) {
                if seen.last().map(String::as_str) != Some(label) {
                    seen.push(label.to_owned());
                }
            }
        }
        seen
    }

    /// Every cross is the same cross: that is what makes a slip show up as a
    /// second one. Only the tick moves, one pitch per mark.
    #[test]
    fn every_mark_repeats_the_cross_and_moves_the_tick_along() {
        let at = Point::new(5.0, 5.0);
        let first = mark(0, at);
        let fourth = mark(3, at);

        assert_eq!(first[0].points, fourth[0].points);
        assert_eq!(first[1].points, fourth[1].points);
        let dx = fourth[2].points[0].x - first[2].points[0].x;
        assert!((dx - 3.0 * TICK_PITCH_MM).abs() < 1e-9, "{dx}");
        // The tick stands clear of the cross.
        assert!(first[2].points[0].x > at.x + ARM_MM);
        assert_eq!(first[0].label.as_deref(), Some("registration 0"));
        assert_eq!(fourth[2].label.as_deref(), Some("registration 3"));
    }

    /// A mark opens the plot, one goes in each time the clock passes another
    /// interval, and one closes it.
    #[test]
    fn marks_go_in_at_the_start_on_the_interval_and_at_the_end() {
        let shapes: Vec<Shape> = (0..10).map(|i| line(i as f64)).collect();
        // One stroke every 30 s: 0, 30, …, 270.
        let starts: Vec<f64> = (0..10).map(|i| i as f64 * 30.0).collect();
        let out = interleave(&shapes, &starts, 100.0, Point::new(0.0, 0.0));

        assert_eq!(
            marks_in(&out),
            [
                "registration 0",
                "registration 1",
                "registration 2",
                "registration 3"
            ]
        );
        // The drawing itself is all there, in its own order.
        let drawing: Vec<&Shape> = out.iter().filter(|s| s.label.is_none()).collect();
        assert_eq!(drawing.len(), 10);
        assert!(drawing
            .windows(2)
            .all(|w| w[0].points[0].x < w[1].points[0].x));
        // Mark 1 goes in before the first stroke due at or after 100 s, the one
        // starting at 120 s.
        let one = out
            .iter()
            .position(|s| s.label.as_deref() == Some("registration 1"))
            .unwrap();
        assert_eq!(out[one + 3].points[0].x, 4.0);
        // And the plot ends on a mark.
        assert!(is_mark(out.last().unwrap().label.as_deref()));
    }

    /// One very long stroke must not be followed by a burst of marks drawn at
    /// the same moment.
    #[test]
    fn a_long_stroke_delays_the_next_mark_rather_than_stacking_them() {
        let shapes = vec![line(0.0), line(1.0), line(2.0)];
        let starts = [0.0, 1000.0, 1001.0];
        let out = interleave(&shapes, &starts, 100.0, Point::new(0.0, 0.0));
        // Start, one before the stroke at 1000 s, one before 1001 s (it is
        // still past due), and the closing one.
        assert_eq!(marks_in(&out).len(), 4);
    }

    /// An empty shape makes no stroke, so it must not eat a start time.
    #[test]
    fn empty_shapes_do_not_shift_the_clock() {
        let shapes = vec![line(0.0), Shape::unlabelled(Vec::new()), line(1.0)];
        let starts = [0.0, 500.0];
        let out = interleave(&shapes, &starts, 100.0, Point::new(0.0, 0.0));
        let one = out
            .iter()
            .position(|s| s.label.as_deref() == Some("registration 1"))
            .unwrap();
        assert_eq!(out[one + 3].points[0].x, 1.0);
    }
}
