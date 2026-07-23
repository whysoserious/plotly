//! Turning a drawing into something the plotter can execute. DESIGN.org §12.
//!
//! `svg` parses and flattens an SVG to polylines (step 2.1); this module turns
//! placed polylines into a flat [`Plan`] of [`Op`]s (step 2.3). The worker
//! (step 2.4) walks the plan and emits G-code.

pub mod svg;

use crate::geometry::{Placement, Point, Polyline};

/// One executable step. Coordinates are machine-logical millimetres; the worker
/// maps them to the wire with the axis [`crate::geometry::Transform`] at emit
/// time (§2.2), so a plan is independent of the machine's wiring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    /// Lift the pen.
    PenUp,
    /// Lower the pen onto the paper.
    PenDown,
    /// Move to an absolute machine-logical point (mm).
    MoveTo(Point),
    /// Set the feed rate for subsequent moves (mm/min). `F` is modal in Grbl.
    SetFeed(u32),
    /// Pause in place for a number of seconds.
    Dwell(f64),
}

/// A whole job as a flat list of ops.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub ops: Vec<Op>,
}

/// Feeds and the subdivision cap used when building a plan. Defaults are the
/// iDraw ones (§10); machine profiles will supply them per machine (step 5.1).
#[derive(Debug, Clone, Copy)]
pub struct PlanSettings {
    /// Longest a single [`Op::MoveTo`] segment may be, in mm. Long strokes are
    /// split into shorter ops so a stop or a crash lands on a fine boundary
    /// (§6) and stop-between-ops reacts quickly.
    pub max_segment_mm: f64,
    /// Feed while drawing (pen down), mm/min.
    pub draw_feed: u32,
    /// Feed while travelling (pen up), mm/min.
    pub travel_feed: u32,
}

impl Default for PlanSettings {
    fn default() -> Self {
        Self {
            max_segment_mm: 5.0,
            draw_feed: 2000,
            travel_feed: 8000,
        }
    }
}

impl Plan {
    /// Build a plan from drawing-logical polylines placed into the field.
    ///
    /// The pen starts up at the machine origin (where `$H` leaves it, §2.4).
    /// For each polyline: travel to its start with the pen up, lower the pen,
    /// draw its points, raise the pen. Long segments are subdivided.
    pub fn build(polylines: &[Polyline], placement: &Placement, settings: &PlanSettings) -> Self {
        let mut ops = vec![Op::PenUp, Op::SetFeed(settings.travel_feed)];
        // After homing the pen sits at the machine origin.
        let mut cursor = Point::new(0.0, 0.0);

        for polyline in polylines {
            let placed: Polyline = polyline.iter().map(|p| placement.place(*p)).collect();
            let Some(&start) = placed.first() else {
                continue;
            };

            // Travel to the start with the pen up.
            push_moves(&mut ops, cursor, &[start], settings.max_segment_mm);
            ops.push(Op::PenDown);
            ops.push(Op::SetFeed(settings.draw_feed));

            // Draw the rest of the polyline.
            push_moves(&mut ops, start, &placed[1..], settings.max_segment_mm);
            cursor = *placed.last().unwrap_or(&start);

            ops.push(Op::PenUp);
            ops.push(Op::SetFeed(settings.travel_feed));
        }
        Self { ops }
    }

    /// Number of pen-down strokes (polylines actually drawn).
    pub fn stroke_count(&self) -> usize {
        self.ops.iter().filter(|op| **op == Op::PenDown).count()
    }

    /// Number of [`Op::MoveTo`] ops.
    pub fn move_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op, Op::MoveTo(_)))
            .count()
    }
}

/// Push subdivided `MoveTo`s stepping from `from` through each of `points`.
///
/// `from` is where the head already is and is never re-emitted; every emitted
/// point advances by at most `cap` millimetres.
fn push_moves(ops: &mut Vec<Op>, from: Point, points: &[Point], cap: f64) {
    let mut prev = from;
    for &target in points {
        for step in subdivide(prev, target, cap) {
            ops.push(Op::MoveTo(step));
        }
        prev = target;
    }
}

/// Points strictly after `a`, up to and including `b`, so that no step exceeds
/// `cap` mm. A short segment yields just `[b]`.
fn subdivide(a: Point, b: Point, cap: f64) -> Vec<Point> {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let dist = dx.hypot(dy);
    if cap <= 0.0 || dist <= cap {
        return vec![b];
    }
    let steps = (dist / cap).ceil() as usize;
    (1..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            Point::new(a.x + dx * t, a.y + dy * t)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(a: Point, b: Point) -> f64 {
        (b.x - a.x).hypot(b.y - a.y)
    }

    #[test]
    fn a_short_segment_is_not_split() {
        let seg = subdivide(Point::new(0.0, 0.0), Point::new(3.0, 0.0), 5.0);
        assert_eq!(seg, vec![Point::new(3.0, 0.0)]);
    }

    #[test]
    fn a_100mm_segment_at_cap_5_makes_at_least_20_steps_preserving_geometry() {
        let a = Point::new(0.0, 0.0);
        let b = Point::new(100.0, 0.0);
        let steps = subdivide(a, b, 5.0);

        assert!(steps.len() >= 20, "got {} steps", steps.len());

        // Every step is within the cap.
        let mut prev = a;
        let mut total = 0.0;
        for &p in &steps {
            let d = dist(prev, p);
            assert!(d <= 5.0 + 1e-9, "step {d} exceeds cap");
            total += d;
            prev = p;
        }
        // Geometry preserved: ends at b, total length equals the original.
        assert_eq!(*steps.last().unwrap(), b);
        assert!((total - 100.0).abs() < 1e-6, "total {total} != 100");
    }

    #[test]
    fn subdivided_points_stay_on_the_original_segment() {
        let a = Point::new(10.0, 10.0);
        let b = Point::new(10.0, 40.0); // vertical, length 30
        for p in subdivide(a, b, 4.0) {
            assert!((p.x - 10.0).abs() < 1e-9, "drifted off the line: {p:?}");
            assert!(p.y >= 10.0 - 1e-9 && p.y <= 40.0 + 1e-9);
        }
    }

    #[test]
    fn plan_has_one_pen_down_per_polyline() {
        let square = vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
            Point::new(0.0, 0.0),
        ];
        let triangle = vec![
            Point::new(20.0, 0.0),
            Point::new(30.0, 0.0),
            Point::new(25.0, 8.0),
            Point::new(20.0, 0.0),
        ];
        let plan = Plan::build(
            &[square, triangle],
            &Placement::identity(),
            &PlanSettings::default(),
        );

        assert_eq!(plan.stroke_count(), 2);
        // Pen up at the very start and after each stroke.
        assert_eq!(plan.ops.first(), Some(&Op::PenUp));
        assert_eq!(
            plan.ops.iter().filter(|op| **op == Op::PenUp).count(),
            3,
            "one initial + one after each of two strokes"
        );
    }

    #[test]
    fn draw_moves_never_exceed_the_cap() {
        let long = vec![Point::new(0.0, 0.0), Point::new(0.0, 100.0)];
        let plan = Plan::build(&[long], &Placement::identity(), &PlanSettings::default());

        // Walk MoveTo ops and check each hop.
        let mut prev: Option<Point> = None;
        for op in &plan.ops {
            if let Op::MoveTo(p) = op {
                if let Some(prev) = prev {
                    assert!(dist(prev, *p) <= 5.0 + 1e-9);
                }
                prev = Some(*p);
            }
        }
    }

    #[test]
    fn an_empty_drawing_produces_no_strokes() {
        let plan = Plan::build(&[], &Placement::identity(), &PlanSettings::default());
        assert_eq!(plan.stroke_count(), 0);
        assert_eq!(plan.move_count(), 0);
    }
}
