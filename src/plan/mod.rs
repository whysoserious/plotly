//! Turning a drawing into something the plotter can execute. DESIGN.org §12.
//!
//! `svg` parses and flattens an SVG to polylines (step 2.1); this module turns
//! placed polylines into a flat [`Plan`] of [`Op`]s (step 2.3). The worker
//! (step 2.4) walks the plan and emits G-code.

pub mod estimate;
pub mod svg;
pub mod text;

use crate::geometry::{Placement, Point, Polyline};

/// One executable step. Coordinates are machine-logical millimetres; the worker
/// maps them to the wire with the axis [`crate::geometry::Transform`] at emit
/// time (§2.2), so a plan is independent of the machine's wiring.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// Geometry to draw, tagged with where it came from: one polyline plus the
/// label of the source element (an SVG `id`, a layer). Each shape becomes
/// exactly one pen-down [`Stroke`] in the plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shape {
    /// Source label, when the element had a usable one.
    pub label: Option<String>,
    pub points: Polyline,
}

impl Shape {
    /// A shape whose source has no usable label (text mode, tests).
    pub fn unlabelled(points: Polyline) -> Self {
        Self {
            label: None,
            points,
        }
    }

    pub fn new(label: Option<String>, points: Polyline) -> Self {
        Self { label, points }
    }
}

/// One pen-down run of the plan — pen down, draw, pen up.
///
/// This is the unit the progress panel counts ("12 of 148 drawn"): it is what
/// actually appears on the paper as one shape, and it maps to a contiguous
/// range of [`Op`]s, so the op index the worker reports is enough to say how
/// many strokes are finished — no extra bookkeeping on the worker side.
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    /// Label of the source element, when it had a usable one.
    pub label: Option<String>,
    /// Index of the [`Op::PenDown`] that starts the stroke.
    pub start_op: usize,
    /// Index of the last op belonging to the stroke (its final [`Op::MoveTo`]).
    pub end_op: usize,
    /// Where the pen lands before drawing: the stroke's first point.
    pub start: Point,
    /// Drawn length in millimetres (pen-down only).
    pub length_mm: f64,
}

/// A whole job as a flat list of ops, plus an index of its pen-down strokes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub ops: Vec<Op>,
    /// Pen-down runs in draw order. Everything but the labels is derived from
    /// `ops`, so a plan read back from `plan.jsonl` rebuilds it ([`Plan::from_ops`]).
    pub strokes: Vec<Stroke>,
}

/// How far a print has got, counted in [`Stroke`]s instead of ops.
///
/// Two measures, because they disagree: a drawing of 147 dots and one long
/// outline is "99% of the strokes" while barely any of the line is on the
/// paper. The stroke count answers "how many shapes are done", the millimetres
/// answer "how much ink is down".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeProgress {
    /// Strokes finished.
    pub done: usize,
    /// Strokes in the plan.
    pub total: usize,
    /// The stroke being drawn right now, if the pen is inside one.
    pub current: Option<usize>,
    /// Pen-down millimetres already drawn.
    pub drawn_mm: f64,
    /// Pen-down millimetres in the whole plan.
    pub total_mm: f64,
}

impl StrokeProgress {
    /// Percent of strokes finished. An empty plan counts as complete.
    pub fn percent(&self) -> u8 {
        percent(self.done as f64, self.total as f64)
    }

    /// Percent of the drawn line laid down.
    pub fn percent_mm(&self) -> u8 {
        percent(self.drawn_mm, self.total_mm)
    }
}

/// `part / whole` as a percentage, clamped to 0–100; a zero whole is complete.
fn percent(part: f64, whole: f64) -> u8 {
    if whole <= 0.0 {
        return 100;
    }
    (part / whole * 100.0).clamp(0.0, 100.0) as u8
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
    /// How much of each end of a stroke to draw slowly, mm. Zero disables the
    /// ramp and every stroke runs at [`PlanSettings::draw_feed`] throughout.
    pub ramp_mm: f64,
    /// Feed for those ends, mm/min (§2.9a).
    ///
    /// A tube nib dragged at speed deflects backwards, and when the machine
    /// stops — 11 ms at `$120 = 3000` — the deflection unloads into a hook at
    /// the end of the stroke. Drawing the whole stroke slowly removes it and
    /// costs the entire plot; drawing only the last few millimetres slowly
    /// removes it where it happens, because the middle of a stroke never
    /// stops. The same ramp opens a stroke, where the nib is loading up
    /// instead.
    pub ramp_feed: u32,
}

impl Default for PlanSettings {
    fn default() -> Self {
        Self {
            max_segment_mm: 5.0,
            draw_feed: 2000,
            travel_feed: 8000,
            // Half a millimetre is about the size of the artefact: the nib's
            // own deflection plus the 0.18 mm the machine needs to stop from
            // `draw_feed` at `$120 = 3000`. Longer costs real time for nothing
            // — on a 6312-shape plot, 2 mm at F300 adds 70 minutes where
            // 0.5 mm adds 18, and 0.5 mm at F600 adds 8.
            ramp_mm: 0.5,
            ramp_feed: 300,
        }
    }
}

impl Plan {
    /// Build a plan from bare polylines, with no source labels (text mode,
    /// tests). See [`Plan::build_shapes`] for the labelled form.
    pub fn build(polylines: &[Polyline], placement: &Placement, settings: &PlanSettings) -> Self {
        assemble(
            polylines.iter().map(|points| (None, points)),
            placement,
            settings,
        )
    }

    /// Build a plan from labelled shapes placed into the field.
    ///
    /// The pen starts up at the machine origin (where `$H` leaves it, §2.4).
    /// For each shape: travel to its start with the pen up, lower the pen,
    /// draw its points, raise the pen. Long segments are subdivided.
    pub fn build_shapes(shapes: &[Shape], placement: &Placement, settings: &PlanSettings) -> Self {
        assemble(
            shapes
                .iter()
                .map(|shape| (shape.label.as_deref(), &shape.points)),
            placement,
            settings,
        )
    }

    /// Wrap a bare op list, rebuilding the stroke index from it.
    ///
    /// Used when a plan comes back from `plan.jsonl` (§6): the ops are the
    /// source of truth, and everything about a stroke except its label follows
    /// from them. Labels are restored separately by [`Plan::apply_labels`].
    pub fn from_ops(ops: Vec<Op>) -> Self {
        let strokes = index_strokes(&ops);
        Self { ops, strokes }
    }

    /// Restore stroke labels saved alongside a job, as `(stroke index, label)`
    /// pairs. Out-of-range indices are ignored, so an older job file cannot
    /// break the load.
    pub fn apply_labels(&mut self, labels: &[(usize, String)]) {
        for (index, label) in labels {
            if let Some(stroke) = self.strokes.get_mut(*index) {
                stroke.label = Some(label.clone());
            }
        }
    }

    /// The labels of this plan's strokes, as sparse `(index, label)` pairs —
    /// what [`Plan::apply_labels`] takes back.
    pub fn stroke_labels(&self) -> Vec<(usize, String)> {
        self.strokes
            .iter()
            .enumerate()
            .filter_map(|(i, stroke)| stroke.label.clone().map(|label| (i, label)))
            .collect()
    }

    /// Number of pen-down strokes (shapes actually drawn).
    pub fn stroke_count(&self) -> usize {
        self.strokes.len()
    }

    /// Total pen-down length in the plan, millimetres — the line that ends up
    /// on the paper, without the pen-up travel between strokes.
    pub fn total_stroke_length_mm(&self) -> f64 {
        self.strokes.iter().map(|s| s.length_mm).sum()
    }

    /// How far the print has got, counted in strokes, given `ops_done` ops
    /// executed. Cheap enough to call every render: one pass over the strokes
    /// plus a walk of the one stroke in progress.
    pub fn stroke_progress(&self, ops_done: usize) -> StrokeProgress {
        let mut progress = StrokeProgress {
            done: 0,
            total: self.strokes.len(),
            current: None,
            drawn_mm: 0.0,
            total_mm: 0.0,
        };
        for (index, stroke) in self.strokes.iter().enumerate() {
            progress.total_mm += stroke.length_mm;
            // Ops `0..ops_done` have run, so a stroke is finished once its last
            // op is below that mark.
            if stroke.end_op < ops_done {
                progress.done += 1;
                progress.drawn_mm += stroke.length_mm;
            } else if stroke.start_op < ops_done && progress.current.is_none() {
                progress.current = Some(index);
                progress.drawn_mm += self.partial_length_mm(stroke, ops_done);
            }
        }
        progress
    }

    /// Millimetres drawn inside `stroke` after `ops_done` ops — the part of the
    /// stroke in progress that is already on the paper.
    fn partial_length_mm(&self, stroke: &Stroke, ops_done: usize) -> f64 {
        let end = ops_done.min(stroke.end_op + 1);
        let mut pos = stroke.start;
        let mut drawn = 0.0;
        for op in &self.ops[stroke.start_op..end] {
            if let Op::MoveTo(p) = op {
                drawn += (p.x - pos.x).hypot(p.y - pos.y);
                pos = *p;
            }
        }
        drawn
    }

    /// Bounding box of the *drawn* geometry, in machine-logical mm, or `None`
    /// when nothing is drawn.
    ///
    /// Only pen-down segments count. A plan opens with a travel from wherever
    /// the head was to the drawing, and counting that would put the box round
    /// the journey rather than round the picture — wrong both for the preview
    /// and for the frame the operator traces before committing paper to it.
    pub fn drawn_bounds(&self) -> Option<(Point, Point)> {
        let mut pen_down = false;
        let mut pos = Point::new(0.0, 0.0);
        let mut acc: Option<(Point, Point)> = None;
        let include = |p: Point, acc: &mut Option<(Point, Point)>| {
            *acc = Some(match *acc {
                None => (p, p),
                Some((min, max)) => (
                    Point::new(min.x.min(p.x), min.y.min(p.y)),
                    Point::new(max.x.max(p.x), max.y.max(p.y)),
                ),
            });
        };
        for op in &self.ops {
            match op {
                Op::PenDown => pen_down = true,
                Op::PenUp => pen_down = false,
                Op::MoveTo(p) => {
                    if pen_down {
                        // Both ends: the segment starts where the pen landed.
                        include(pos, &mut acc);
                        include(*p, &mut acc);
                    }
                    pos = *p;
                }
                Op::SetFeed(_) | Op::Dwell(_) => {}
            }
        }
        acc
    }

    /// The drawn bounding box as a closed rectangle to trace, starting and
    /// ending at its top-left corner — what the `f` frame follows (step 5.3).
    pub fn frame_outline(&self) -> Option<Vec<Point>> {
        let (min, max) = self.drawn_bounds()?;
        Some(vec![
            Point::new(min.x, min.y),
            Point::new(max.x, min.y),
            Point::new(max.x, max.y),
            Point::new(min.x, max.y),
            Point::new(min.x, min.y),
        ])
    }

    /// Number of [`Op::MoveTo`] ops.
    pub fn move_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op, Op::MoveTo(_)))
            .count()
    }

    /// Total XY travel distance, millimetres — every `MoveTo` segment, pen up
    /// and down alike (the head still moves).
    pub fn total_distance_mm(&self) -> f64 {
        let mut pos = Point::new(0.0, 0.0);
        let mut total = 0.0;
        for op in &self.ops {
            if let Op::MoveTo(p) = op {
                total += (p.x - pos.x).hypot(p.y - pos.y);
                pos = *p;
            }
        }
        total
    }

    /// Dry-run time and distances on `machine` — see [`estimate`].
    pub fn estimate(&self, machine: &estimate::Machine) -> estimate::Estimate {
        estimate::estimate(self, machine)
    }
}

/// Emit the ops for a sequence of `(label, polyline)` items and index the
/// strokes they produce. The one place a plan is assembled — both
/// [`Plan::build`] and [`Plan::build_shapes`] funnel through here.
fn assemble<'a>(
    items: impl IntoIterator<Item = (Option<&'a str>, &'a Polyline)>,
    placement: &Placement,
    settings: &PlanSettings,
) -> Plan {
    let mut ops = vec![Op::PenUp, Op::SetFeed(settings.travel_feed)];
    // After homing the pen sits at the machine origin.
    let mut cursor = Point::new(0.0, 0.0);
    // Labels of the strokes emitted so far, in the same order as the strokes
    // the index below finds.
    let mut labels: Vec<Option<String>> = Vec::new();

    for (label, polyline) in items {
        let placed: Polyline = polyline.iter().map(|p| placement.place(*p)).collect();
        let Some(&start) = placed.first() else {
            continue;
        };

        // Travel to the start with the pen up.
        push_moves(&mut ops, cursor, &[start], settings.max_segment_mm);
        ops.push(Op::PenDown);

        // Draw the rest of the polyline, slow at both ends.
        push_stroke(&mut ops, start, &placed[1..], settings);
        cursor = *placed.last().unwrap_or(&start);

        ops.push(Op::PenUp);
        ops.push(Op::SetFeed(settings.travel_feed));
        labels.push(label.map(str::to_owned));
    }

    let mut strokes = index_strokes(&ops);
    for (stroke, label) in strokes.iter_mut().zip(labels) {
        stroke.label = label;
    }
    Plan { ops, strokes }
}

/// Find the pen-down runs in an op list: where each starts and ends, where the
/// pen lands and how far it draws. Labels are not in the ops, so they come out
/// empty here and are filled in by the caller.
fn index_strokes(ops: &[Op]) -> Vec<Stroke> {
    let mut strokes = Vec::new();
    let mut pos = Point::new(0.0, 0.0);
    let mut current: Option<Stroke> = None;

    for (index, op) in ops.iter().enumerate() {
        match op {
            Op::PenDown => {
                current = Some(Stroke {
                    label: None,
                    start_op: index,
                    end_op: index,
                    start: pos,
                    length_mm: 0.0,
                });
            }
            Op::PenUp => strokes.extend(current.take()),
            Op::MoveTo(p) => {
                if let Some(stroke) = &mut current {
                    stroke.length_mm += (p.x - pos.x).hypot(p.y - pos.y);
                    stroke.end_op = index;
                }
                pos = *p;
            }
            Op::SetFeed(_) | Op::Dwell(_) => {}
        }
    }
    // A plan cut short mid-stroke still has that stroke; keep it rather than
    // silently dropping geometry from the count.
    strokes.extend(current);
    strokes
}

/// Emit one pen-down stroke: subdivided moves with a slow lead-in and
/// lead-out, and the drawing feed in between.
///
/// The feed ops go *between* moves, so the machine changes speed at a point on
/// the path rather than mid-segment; a segment straddling a boundary is split
/// at it. A stroke too short to hold both ramps is drawn slowly throughout —
/// it is all end.
fn push_stroke(ops: &mut Vec<Op>, from: Point, points: &[Point], settings: &PlanSettings) {
    let steps: Vec<Point> = {
        let mut out = Vec::new();
        let mut prev = from;
        for &target in points {
            out.extend(subdivide(prev, target, settings.max_segment_mm));
            prev = target;
        }
        out
    };
    let total: f64 = {
        let mut prev = from;
        steps.iter().fold(0.0, |acc, p| {
            let d = (p.x - prev.x).hypot(p.y - prev.y);
            prev = *p;
            acc + d
        })
    };

    let ramp = settings.ramp_mm.max(0.0);
    // Nothing to ramp into, or no room for a fast middle: one feed throughout.
    if ramp == 0.0 || total <= 2.0 * ramp {
        let feed = if ramp == 0.0 {
            settings.draw_feed
        } else {
            settings.ramp_feed
        };
        ops.push(Op::SetFeed(feed));
        ops.extend(steps.into_iter().map(Op::MoveTo));
        return;
    }

    ops.push(Op::SetFeed(settings.ramp_feed));
    let mut feed = settings.ramp_feed;
    let mut prev = from;
    let mut done = 0.0;
    for step in steps {
        let seg = (step.x - prev.x).hypot(step.y - prev.y);
        // Boundaries this segment crosses, in the order it meets them.
        for (at, next) in [
            (ramp, settings.draw_feed),
            (total - ramp, settings.ramp_feed),
        ] {
            if feed == next || done >= at || done + seg <= at {
                continue;
            }
            // Land exactly on the boundary, then change feed there.
            let t = (at - done) / seg;
            let split = to_micron(Point::new(
                prev.x + (step.x - prev.x) * t,
                prev.y + (step.y - prev.y) * t,
            ));
            ops.push(Op::MoveTo(split));
            ops.push(Op::SetFeed(next));
            feed = next;
        }
        // A boundary landing exactly on a segment start is not "crossed" above.
        if done >= total - ramp && feed != settings.ramp_feed {
            ops.push(Op::SetFeed(settings.ramp_feed));
            feed = settings.ramp_feed;
        }
        ops.push(Op::MoveTo(step));
        done += seg;
        prev = step;
    }
}

/// Round a computed point to the micron.
///
/// Two reasons, and either would do. The wire carries three decimals
/// (`Driver::move_to`) and the machine steps at 0.01 mm (`$100`), so anything
/// finer is invented. And a full-precision `f64` does not survive
/// `plan.jsonl`: `1.1094003924504545` comes back from `serde_json` as `…543`,
/// which is physically nothing and yet enough to make a resumed plan differ
/// from the one that was saved. Points that come straight from the drawing are
/// left alone; this is for the ones we compute.
fn to_micron(p: Point) -> Point {
    Point::new(
        (p.x * 1000.0).round() / 1000.0,
        (p.y * 1000.0).round() / 1000.0,
    )
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

    /// Feeds only; a stroke long enough for both ramps and a fast middle.
    fn stroke_feeds(len_mm: f64, settings: &PlanSettings) -> Vec<u32> {
        let plan = Plan::build(
            &[vec![Point::new(0.0, 0.0), Point::new(len_mm, 0.0)]],
            &Placement::identity(),
            settings,
        );
        plan.ops[plan.strokes[0].start_op..]
            .iter()
            .filter_map(|op| match op {
                Op::SetFeed(f) => Some(*f),
                _ => None,
            })
            .collect()
    }

    /// The ends of a stroke are drawn slowly and the middle is not, and the
    /// feed changes land *on* the boundary rather than at whatever subdivided
    /// point happens to be nearest.
    #[test]
    fn a_stroke_is_slow_at_both_ends_and_fast_in_the_middle() {
        let settings = PlanSettings {
            ramp_mm: 2.0,
            ramp_feed: 300,
            draw_feed: 2000,
            ..PlanSettings::default()
        };
        assert_eq!(stroke_feeds(40.0, &settings), vec![300, 2000, 300, 8000]);

        let plan = Plan::build(
            &[vec![Point::new(0.0, 0.0), Point::new(40.0, 0.0)]],
            &Placement::identity(),
            &settings,
        );
        let xs: Vec<f64> = plan.ops[plan.strokes[0].start_op..]
            .iter()
            .filter_map(|op| match op {
                Op::MoveTo(p) => Some(p.x),
                _ => None,
            })
            .collect();
        assert!(xs.contains(&2.0), "no move ends at the lead-in: {xs:?}");
        assert!(xs.contains(&38.0), "no move ends at the lead-out: {xs:?}");
    }

    /// A stroke with no room for two ramps is all end, so it goes slowly
    /// throughout rather than being given a nonsensical middle.
    #[test]
    fn a_stroke_shorter_than_two_ramps_is_slow_all_through() {
        let settings = PlanSettings {
            ramp_mm: 2.0,
            ramp_feed: 300,
            ..PlanSettings::default()
        };
        assert_eq!(stroke_feeds(3.0, &settings), vec![300, 8000]);
    }

    /// Zero turns the ramp off: one feed for the whole stroke, as before.
    #[test]
    fn a_zero_ramp_draws_the_whole_stroke_at_the_drawing_feed() {
        let settings = PlanSettings {
            ramp_mm: 0.0,
            draw_feed: 2000,
            ..PlanSettings::default()
        };
        assert_eq!(stroke_feeds(40.0, &settings), vec![2000, 8000]);
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
        // Nothing to move and nothing to draw.
        assert_eq!(plan.total_distance_mm(), 0.0);
        assert_eq!(plan.drawn_bounds(), None);
        assert_eq!(plan.frame_outline(), None);
    }

    /// Two 10 mm strokes, far apart so the travel between them is obvious.
    fn two_stroke_plan() -> Plan {
        let a = vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)];
        let b = vec![Point::new(50.0, 0.0), Point::new(60.0, 0.0)];
        Plan::build_shapes(
            &[
                Shape::new(Some("first".to_owned()), a),
                Shape::unlabelled(b),
            ],
            &Placement::identity(),
            &PlanSettings::default(),
        )
    }

    #[test]
    fn each_shape_becomes_one_stroke_carrying_its_label_and_length() {
        let plan = two_stroke_plan();
        assert_eq!(plan.stroke_count(), 2);

        let first = &plan.strokes[0];
        assert_eq!(first.label.as_deref(), Some("first"));
        assert!((first.length_mm - 10.0).abs() < 1e-6, "{}", first.length_mm);
        assert_eq!(first.start, Point::new(0.0, 0.0));
        assert_eq!(plan.ops[first.start_op], Op::PenDown);
        // The stroke ends on its last drawn point, before the closing pen-up.
        assert_eq!(plan.ops[first.end_op], Op::MoveTo(Point::new(10.0, 0.0)));

        assert_eq!(plan.strokes[1].label, None);
        assert!((plan.total_stroke_length_mm() - 20.0).abs() < 1e-6);
    }

    #[test]
    fn stroke_progress_counts_finished_strokes_not_ops() {
        let plan = two_stroke_plan();

        // Nothing sent yet.
        let start = plan.stroke_progress(0);
        assert_eq!((start.done, start.total, start.current), (0, 2, None));
        assert_eq!(start.percent(), 0);

        // Halfway through the first stroke: none finished, one in progress,
        // and half its line drawn.
        // Pen down, set feed, then one of the two subdivided draw moves.
        let mid = plan.strokes[0].start_op + 3;
        let half = plan.stroke_progress(mid);
        assert_eq!(half.done, 0);
        assert_eq!(half.current, Some(0));
        assert!(half.drawn_mm > 0.0 && half.drawn_mm < 10.0, "{half:?}");

        // Just past the first stroke's last op: one finished, 10 of 20 mm.
        let after_first = plan.stroke_progress(plan.strokes[0].end_op + 1);
        assert_eq!(after_first.done, 1);
        assert_eq!(after_first.percent(), 50);
        assert!((after_first.drawn_mm - 10.0).abs() < 1e-6);
        assert_eq!(after_first.percent_mm(), 50);

        // Every op executed: both strokes done, nothing in progress.
        let end = plan.stroke_progress(plan.ops.len());
        assert_eq!((end.done, end.current), (2, None));
        assert_eq!(end.percent(), 100);
        assert!((end.drawn_mm - end.total_mm).abs() < 1e-6);
    }

    /// The pen-up travel between strokes must not count as drawn line — that is
    /// the whole difference between "how much ink is down" and total travel.
    #[test]
    fn travel_between_strokes_is_not_drawn_length() {
        let plan = two_stroke_plan();
        // 10 + 10 mm drawn, but 60 mm of head movement in total.
        assert!((plan.total_stroke_length_mm() - 20.0).abs() < 1e-6);
        assert!(plan.total_distance_mm() > 55.0);
    }

    /// A plan read back from `plan.jsonl` is ops only, so the stroke index has
    /// to be rebuilt from them — and must match what `build` produced.
    #[test]
    fn from_ops_rebuilds_the_same_strokes_minus_labels() {
        let plan = two_stroke_plan();
        let mut rebuilt = Plan::from_ops(plan.ops.clone());
        assert_eq!(rebuilt.strokes.len(), plan.strokes.len());
        assert!(rebuilt.strokes.iter().all(|s| s.label.is_none()));

        rebuilt.apply_labels(&plan.stroke_labels());
        assert_eq!(rebuilt, plan);
    }

    #[test]
    fn applying_labels_ignores_indices_that_do_not_exist() {
        let mut plan = two_stroke_plan();
        plan.apply_labels(&[(99, "ghost".to_owned())]);
        assert_eq!(plan.stroke_count(), 2);
    }

    #[test]
    fn an_empty_plan_reads_as_complete_rather_than_dividing_by_zero() {
        let plan = Plan::build(&[], &Placement::identity(), &PlanSettings::default());
        let progress = plan.stroke_progress(0);
        assert_eq!(progress.total, 0);
        assert_eq!(progress.percent(), 100);
        assert_eq!(progress.percent_mm(), 100);
    }

    #[test]
    fn distance_measures_a_known_line() {
        // A single 100 mm stroke from the origin: travel 0 (starts at origin),
        // then 100 mm drawn. Total distance is 100 mm. Timing it is
        // `plan::estimate`, which has its own tests.
        let line = vec![Point::new(0.0, 0.0), Point::new(100.0, 0.0)];
        let plan = Plan::build(&[line], &Placement::identity(), &PlanSettings::default());
        assert!((plan.total_distance_mm() - 100.0).abs() < 1e-6);
    }

    /// The box goes round the *drawing*, not round the journey to it: a plan
    /// opens with a travel from the head, and counting that would frame the
    /// wrong thing entirely.
    #[test]
    fn drawn_bounds_ignore_the_travel_to_the_drawing() {
        let square = vec![
            Point::new(100.0, 200.0),
            Point::new(140.0, 200.0),
            Point::new(140.0, 230.0),
            Point::new(100.0, 200.0),
        ];
        let plan = Plan::build(&[square], &Placement::identity(), &PlanSettings::default());

        let (min, max) = plan.drawn_bounds().expect("something is drawn");
        assert_eq!(min, Point::new(100.0, 200.0));
        assert_eq!(max, Point::new(140.0, 230.0));
        // The head travelled from the origin to get there, and that is not
        // part of the drawing.
        assert!(plan.total_distance_mm() > plan.total_stroke_length_mm());
    }

    #[test]
    fn the_frame_outline_is_a_closed_rectangle_of_the_drawn_bounds() {
        let line = vec![Point::new(10.0, 20.0), Point::new(50.0, 60.0)];
        let plan = Plan::build(&[line], &Placement::identity(), &PlanSettings::default());

        let outline = plan.frame_outline().expect("something is drawn");
        assert_eq!(
            outline,
            vec![
                Point::new(10.0, 20.0),
                Point::new(50.0, 20.0),
                Point::new(50.0, 60.0),
                Point::new(10.0, 60.0),
                Point::new(10.0, 20.0),
            ]
        );
        assert_eq!(
            outline.first(),
            outline.last(),
            "the frame has to come back to where it started"
        );
    }
}
