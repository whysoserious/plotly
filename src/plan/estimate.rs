//! How long a plan will take. DESIGN.org §3.6 / step 5.3.
//!
//! Dividing length by feed is not an estimate, it is a lower bound: the head
//! starts every stroke from a standstill, and it slows into every corner. On a
//! drawing made of many short strokes those two costs are most of the runtime,
//! which is why a naive figure reads "4 minutes" for a job that takes twelve.
//!
//! So this walks the plan the way the firmware's planner does:
//!
//! - the ops split into **runs** — a maximal stretch of moves at one feed with
//!   the pen neither raised nor lowered in between. A run starts and ends at a
//!   standstill, because whatever ended it (a pen move, a dwell) has to wait
//!   for the motion to finish;
//! - inside a run, each junction gets a **speed limit from its turn angle**,
//!   using Grbl's junction-deviation rule — straight-through costs nothing, a
//!   right angle costs a lot, a reversal means a full stop;
//! - a **forward and backward pass** then settles the speed at every point, so
//!   no segment is asked to accelerate or brake harder than the machine can;
//! - each segment is finally timed as the trapezoid (or triangle) it is.
//!
//! It still runs optimistic — it ignores the planner's finite lookahead and
//! anything mechanical — but it is the right shape of wrong, and it moves in
//! the right direction when the profile changes.

use crate::geometry::Point;
use crate::plan::{Op, Plan};
use crate::profiles::Profile;

/// The machine numbers a time estimate needs (§10).
#[derive(Debug, Clone, Copy)]
pub struct Machine {
    /// Acceleration, mm/s² (`$120`/`$121`).
    pub accel_mm_s2: f64,
    /// Grbl's junction deviation, mm (`$11`): how far off the corner the head
    /// is allowed to cut, which is what sets the speed it may carry through.
    pub junction_deviation_mm: f64,
    /// Time for one pen up or down, seconds — the Z move plus a settle.
    pub pen_secs: f64,
}

impl Machine {
    /// Take the numbers from a machine profile.
    pub fn from_profile(profile: &Profile) -> Self {
        // The pen's own move: the Z travel between up and down, at the Z feed.
        let z_mm = (f64::from(profile.pen.down_z) - f64::from(profile.pen.up_z)).abs();
        let z_mm_s = f64::from(profile.pen.z_feed) / 60.0;
        let z_secs = if z_mm_s > 0.0 { z_mm / z_mm_s } else { 0.0 };
        Self {
            accel_mm_s2: profile.accel_mm_s2,
            junction_deviation_mm: profile.junction_deviation_mm,
            // The settle is the profile's, so the estimate counts the same
            // stillness the driver actually holds (`Driver::set_pen`).
            pen_secs: z_secs + profile.pen.settle_secs.max(0.0),
        }
    }
}

/// What a dry run of a plan comes to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// Total time, seconds.
    pub secs: f64,
    /// Distance drawn with the pen down, mm — the ink on the paper.
    pub draw_mm: f64,
    /// Distance covered with the pen up, mm — the getting there.
    pub travel_mm: f64,
}

impl Estimate {
    /// Everything the head moves, drawn or not.
    pub fn total_mm(&self) -> f64 {
        self.draw_mm + self.travel_mm
    }
}

/// Dry-run `plan` on `machine` and report the time and distances.
pub fn estimate(plan: &Plan, machine: &Machine) -> Estimate {
    let mut out = Estimate {
        secs: 0.0,
        draw_mm: 0.0,
        travel_mm: 0.0,
    };
    /// The run being gathered, and the state it is being drawn under.
    struct Run {
        /// Where the head was before the run's first move.
        from: Point,
        points: Vec<Point>,
        feed_mm_min: f64,
        pen_down: bool,
    }
    let mut run = Run {
        from: Point::new(0.0, 0.0),
        points: Vec::new(),
        feed_mm_min: 0.0,
        pen_down: false,
    };

    for op in &plan.ops {
        // Only moves extend a run. Anything else ends it: a pen move has to
        // wait for the head to stop, and after a feed change it is running to
        // a different ceiling.
        if let Op::MoveTo(p) = op {
            run.points.push(*p);
            continue;
        }
        add_run(
            &mut out,
            &run.points,
            run.from,
            run.feed_mm_min,
            run.pen_down,
            machine,
        );
        if let Some(last) = run.points.last() {
            run.from = *last;
        }
        run.points.clear();

        match op {
            // A pen op that changes nothing sends nothing (the driver skips
            // it), so it costs nothing — a plan opens with a pen-up while the
            // pen is already up.
            Op::PenUp | Op::PenDown => {
                let target = matches!(op, Op::PenDown);
                if run.pen_down != target {
                    run.pen_down = target;
                    out.secs += machine.pen_secs;
                }
            }
            Op::SetFeed(f) => run.feed_mm_min = f64::from(*f),
            Op::Dwell(s) => out.secs += *s,
            Op::MoveTo(_) => unreachable!("moves are handled above"),
        }
    }
    add_run(
        &mut out,
        &run.points,
        run.from,
        run.feed_mm_min,
        run.pen_down,
        machine,
    );

    out
}

/// Fold one finished run into the totals.
fn add_run(
    out: &mut Estimate,
    points: &[Point],
    from: Point,
    feed_mm_min: f64,
    pen_down: bool,
    machine: &Machine,
) {
    if points.is_empty() {
        return;
    }
    let length = path_length(from, points);
    if pen_down {
        out.draw_mm += length;
    } else {
        out.travel_mm += length;
    }
    out.secs += run_secs(from, points, feed_mm_min / 60.0, machine);
}

/// Total length of the path `from` → each of `points`.
fn path_length(from: Point, points: &[Point]) -> f64 {
    let mut prev = from;
    let mut total = 0.0;
    for p in points {
        total += distance(prev, *p);
        prev = *p;
    }
    total
}

/// Time for one run: start at rest, visit every point, end at rest.
fn run_secs(from: Point, points: &[Point], cruise_mm_s: f64, machine: &Machine) -> f64 {
    if cruise_mm_s <= 0.0 || machine.accel_mm_s2 <= 0.0 {
        return 0.0;
    }
    // Segment lengths and unit directions; zero-length moves carry no time and
    // no direction, so they are dropped rather than poisoning the angles.
    let mut lengths = Vec::with_capacity(points.len());
    let mut dirs = Vec::with_capacity(points.len());
    let mut prev = from;
    for p in points {
        let (dx, dy) = (p.x - prev.x, p.y - prev.y);
        let len = dx.hypot(dy);
        if len > 0.0 {
            lengths.push(len);
            dirs.push((dx / len, dy / len));
        }
        prev = *p;
    }
    if lengths.is_empty() {
        return 0.0;
    }

    // Speed permitted at each point: nothing at the ends, the corner limit in
    // between.
    let mut speeds = vec![0.0; lengths.len() + 1];
    for i in 1..lengths.len() {
        speeds[i] = junction_speed(dirs[i - 1], dirs[i], machine).min(cruise_mm_s);
    }

    // Forward: no faster than we can accelerate to. Backward: no faster than
    // we can brake from. Together they leave a speed every segment can meet.
    for i in 0..lengths.len() {
        let reachable = (speeds[i].powi(2) + 2.0 * machine.accel_mm_s2 * lengths[i]).sqrt();
        speeds[i + 1] = speeds[i + 1].min(reachable).min(cruise_mm_s);
    }
    for i in (0..lengths.len()).rev() {
        let reachable = (speeds[i + 1].powi(2) + 2.0 * machine.accel_mm_s2 * lengths[i]).sqrt();
        speeds[i] = speeds[i].min(reachable).min(cruise_mm_s);
    }

    (0..lengths.len())
        .map(|i| {
            segment_secs(
                lengths[i],
                speeds[i],
                speeds[i + 1],
                cruise_mm_s,
                machine.accel_mm_s2,
            )
        })
        .sum()
}

/// Time to cover `length` entering at `v_in`, leaving at `v_out`, never above
/// `v_max`, accelerating at `a`.
fn segment_secs(length: f64, v_in: f64, v_out: f64, v_max: f64, a: f64) -> f64 {
    // Peak speed if the segment were all ramp: accelerate from v_in and brake
    // to v_out, meeting somewhere in the middle.
    let peak = (((2.0 * a * length + v_in * v_in + v_out * v_out) / 2.0).max(0.0)).sqrt();
    if peak <= v_max {
        // Triangular: never reaches the feed.
        return (peak - v_in).max(0.0) / a + (peak - v_out).max(0.0) / a;
    }
    // Trapezoidal: ramp up, hold the feed, ramp down.
    let d_up = (v_max * v_max - v_in * v_in) / (2.0 * a);
    let d_down = (v_max * v_max - v_out * v_out) / (2.0 * a);
    let cruise = (length - d_up - d_down).max(0.0);
    (v_max - v_in) / a + (v_max - v_out) / a + cruise / v_max
}

/// Speed that may be carried through the corner between two unit directions.
///
/// Grbl's junction-deviation rule: the corner is treated as an arc of the
/// largest circle that stays within `junction_deviation` of it, and the speed
/// is what centripetal acceleration allows on that arc. Straight through means
/// an infinite circle (no limit); a reversal means none at all.
fn junction_speed(prev: (f64, f64), next: (f64, f64), machine: &Machine) -> f64 {
    // Grbl's convention: the angle between the *reversed* incoming direction
    // and the outgoing one, so straight-through is cos = -1.
    let cos_theta = -(prev.0 * next.0 + prev.1 * next.1);
    // Straight (or as near as makes no difference): no corner, no limit.
    if cos_theta < -0.999_999 {
        return f64::INFINITY;
    }
    // A full reversal: the head has to stop.
    if cos_theta > 0.999_999 {
        return 0.0;
    }
    let sin_theta_d2 = (0.5 * (1.0 - cos_theta)).max(0.0).sqrt();
    let denominator = 1.0 - sin_theta_d2;
    if denominator <= 0.0 {
        return f64::INFINITY;
    }
    (machine.accel_mm_s2 * machine.junction_deviation_mm * sin_theta_d2 / denominator).sqrt()
}

fn distance(a: Point, b: Point) -> f64 {
    (b.x - a.x).hypot(b.y - a.y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Placement;
    use crate::plan::{PlanSettings, Shape};

    fn machine() -> Machine {
        Machine {
            accel_mm_s2: 3000.0,
            junction_deviation_mm: 0.01,
            pen_secs: 0.1,
        }
    }

    /// A plan of one straight stroke, `mm` long, drawn at the default feed.
    fn straight(mm: f64) -> Plan {
        Plan::build(
            &[vec![Point::new(0.0, 0.0), Point::new(mm, 0.0)]],
            &Placement::identity(),
            &PlanSettings::default(),
        )
    }

    #[test]
    fn a_long_straight_stroke_costs_about_its_length_over_the_feed() {
        let plan = straight(1000.0);
        let est = estimate(&plan, &machine());

        // 1000 mm at 2000 mm/min is 30 s of cruising; the ramps at each end add
        // a fraction of a second, and the two pen moves a fifth.
        assert!(
            est.secs > 30.0 && est.secs < 31.5,
            "estimate {} s out of range",
            est.secs
        );
        assert!((est.draw_mm - 1000.0).abs() < 1e-6);
        assert_eq!(est.travel_mm, 0.0, "the stroke starts at the origin");
    }

    /// The whole point of the model: acceleration costs time, so the estimate
    /// must come out above the length-over-feed floor.
    #[test]
    fn the_estimate_is_never_below_the_naive_floor() {
        for mm in [1.0, 10.0, 100.0, 1000.0] {
            let plan = straight(mm);
            let est = estimate(&plan, &machine());
            let floor = mm / (f64::from(PlanSettings::default().draw_feed) / 60.0);
            assert!(
                est.secs >= floor,
                "{mm} mm: estimated {} s, below the {floor} s floor",
                est.secs
            );
        }
    }

    /// Many short strokes cost far more than one long one of the same length:
    /// every stroke is a standstill at both ends plus two pen moves. This is
    /// exactly what dividing length by feed misses.
    #[test]
    fn many_short_strokes_cost_more_than_one_long_one() {
        let long = straight(200.0);
        let dashes: Vec<Shape> = (0..40)
            .map(|i| {
                let y = f64::from(i);
                Shape::unlabelled(vec![Point::new(0.0, y), Point::new(5.0, y)])
            })
            .collect();
        let short = Plan::build_shapes(&dashes, &Placement::identity(), &PlanSettings::default());

        let long_secs = estimate(&long, &machine()).secs;
        let short_secs = estimate(&short, &machine()).secs;
        assert!(
            short_secs > long_secs,
            "40 dashes ({short_secs} s) should cost more than one 200 mm line ({long_secs} s)"
        );
    }

    /// Pen-up travel is counted separately from ink, because they answer
    /// different questions ("how long" versus "how much of the pen").
    #[test]
    fn travel_and_ink_are_counted_apart() {
        let a = vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)];
        let b = vec![Point::new(100.0, 0.0), Point::new(110.0, 0.0)];
        let plan = Plan::build(&[a, b], &Placement::identity(), &PlanSettings::default());

        let est = estimate(&plan, &machine());
        assert!((est.draw_mm - 20.0).abs() < 1e-6, "drew {}", est.draw_mm);
        assert!(
            (est.travel_mm - 90.0).abs() < 1e-6,
            "travelled {}",
            est.travel_mm
        );
        assert!((est.total_mm() - 110.0).abs() < 1e-6);
    }

    #[test]
    fn an_empty_plan_costs_nothing_much() {
        let plan = Plan::build(&[], &Placement::identity(), &PlanSettings::default());
        let est = estimate(&plan, &machine());
        assert_eq!(est.draw_mm, 0.0);
        assert_eq!(est.travel_mm, 0.0);
        // Only the opening pen-up.
        assert!(est.secs < 1.0, "{} s for nothing", est.secs);
    }

    #[test]
    fn a_dwell_is_added_to_the_time() {
        let mut plan = straight(10.0);
        let before = estimate(&plan, &machine()).secs;
        plan.ops.push(Op::Dwell(2.5));
        let after = estimate(&plan, &machine()).secs;
        assert!((after - before - 2.5).abs() < 1e-6);
    }

    #[test]
    fn straight_through_a_junction_carries_no_speed_limit() {
        let m = machine();
        assert_eq!(junction_speed((1.0, 0.0), (1.0, 0.0), &m), f64::INFINITY);
    }

    #[test]
    fn a_reversal_stops_the_head() {
        let m = machine();
        assert_eq!(junction_speed((1.0, 0.0), (-1.0, 0.0), &m), 0.0);
    }

    /// A sharper corner must never allow more speed than a gentler one.
    #[test]
    fn corner_speed_falls_as_the_turn_sharpens() {
        let m = machine();
        let straight_ish = junction_speed((1.0, 0.0), (0.995, 0.0998), &m);
        let right_angle = junction_speed((1.0, 0.0), (0.0, 1.0), &m);
        let hairpin = junction_speed((1.0, 0.0), (-0.995, 0.0998), &m);

        assert!(
            straight_ish > right_angle,
            "{straight_ish} vs {right_angle}"
        );
        assert!(right_angle > hairpin, "{right_angle} vs {hairpin}");
        assert!(hairpin >= 0.0);
    }

    /// A square has four corners to brake into; the same length of straight
    /// line does not.
    #[test]
    fn corners_cost_time() {
        let square = vec![
            Point::new(0.0, 0.0),
            Point::new(50.0, 0.0),
            Point::new(50.0, 50.0),
            Point::new(0.0, 50.0),
            Point::new(0.0, 0.0),
        ];
        let boxed = Plan::build(&[square], &Placement::identity(), &PlanSettings::default());
        let line = straight(200.0);

        let boxed_secs = estimate(&boxed, &machine()).secs;
        let line_secs = estimate(&line, &machine()).secs;
        assert!(
            boxed_secs > line_secs,
            "a 200 mm square ({boxed_secs} s) should cost more than a 200 mm line ({line_secs} s)"
        );
    }

    /// A slower machine takes longer — the estimate has to follow the profile,
    /// or it is just a constant with extra steps.
    #[test]
    fn less_acceleration_means_more_time() {
        let plan = straight(100.0);
        let brisk = estimate(&plan, &machine()).secs;
        let sluggish = estimate(
            &plan,
            &Machine {
                accel_mm_s2: 50.0,
                ..machine()
            },
        )
        .secs;
        assert!(sluggish > brisk, "{sluggish} vs {brisk}");
    }

    #[test]
    fn a_segment_that_cannot_reach_the_feed_is_timed_as_a_triangle() {
        // 1 mm at 3000 mm/s² from rest to rest: it accelerates over half the
        // distance, so the peak is sqrt(a·L/... ) — sqrt(a·L) here — which is
        // 55 mm/s, far under the 1000 mm/s ceiling. It never cruises.
        let (length, a) = (1.0_f64, 3000.0_f64);
        let secs = segment_secs(length, 0.0, 0.0, 1000.0, a);
        let peak = (a * length).sqrt();
        assert!((secs - 2.0 * peak / a).abs() < 1e-9, "{secs}");
        // And half the distance really is spent accelerating.
        assert!((peak * peak / (2.0 * a) - length / 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_segment_that_reaches_the_feed_is_timed_as_a_trapezoid() {
        // Long enough to cruise: the time is length/v plus the two ramps.
        let (length, v, a) = (1000.0, 33.0, 3000.0);
        let secs = segment_secs(length, 0.0, 0.0, v, a);
        let expected = length / v + v / a;
        assert!((secs - expected).abs() < 1e-9, "{secs} vs {expected}");
    }

    /// The profile is where the numbers come from; a pen that moves further or
    /// slower costs more per stroke.
    #[test]
    fn the_machine_takes_its_numbers_from_the_profile() {
        let mut profile = Profile::builtin(crate::profiles::DEFAULT_PROFILE).unwrap();
        let quick = Machine::from_profile(&profile);

        profile.pen.z_feed /= 10;
        let slow = Machine::from_profile(&profile);

        assert!(
            slow.pen_secs > quick.pen_secs,
            "{} vs {}",
            slow.pen_secs,
            quick.pen_secs
        );
        assert_eq!(quick.accel_mm_s2, profile.accel_mm_s2);
    }
}
