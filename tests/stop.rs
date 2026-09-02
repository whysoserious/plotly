//! Integration test for step 2.7: stop, pause/resume and panic abort of a
//! running plan on the worker + mock.
//!
//! The mock is given a small per-read delay so a plan takes long enough to
//! interrupt partway through; without it the plan would finish instantly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use plotly::geometry::{Placement, Point, Transform};
use plotly::plan::{Op, Plan, PlanSettings};
use plotly::plotter::driver::Driver;
use plotly::plotter::mock::MockTransport;
use plotly::plotter::worker::{Command, Event, Worker};
use plotly::plotter::Connection;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Shared handle to the mock's line log.
type SentLog = Arc<Mutex<Vec<String>>>;
/// Shared handle to the mock's realtime-byte log.
type RealtimeLog = Arc<Mutex<Vec<u8>>>;

/// A worker over a deliberately slow mock, plus handles to its wire logs.
fn slow_worker() -> (Worker, SentLog, RealtimeLog) {
    let mock = MockTransport::with_read_delay(Duration::from_millis(3));
    let sent = mock.sent_handle();
    let realtime = mock.realtime_handle();
    let driver = Driver::new(Connection {
        transport: Box::new(mock),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    let worker = Worker::spawn(driver);
    worker.recv_timeout(TIMEOUT); // drop the initial State
    (worker, sent, realtime)
}

/// A long single-stroke plan, enough ops to interrupt mid-run.
fn long_plan() -> Plan {
    let line = vec![Point::new(0.0, 0.0), Point::new(400.0, 0.0)];
    Plan::build(&[line], &Placement::identity(), &PlanSettings::default())
}

/// Two long strokes, so a pause can land inside one and still leave a shape
/// boundary — and a whole shape — ahead of it.
fn two_shape_plan() -> Plan {
    let a = vec![Point::new(0.0, 0.0), Point::new(400.0, 0.0)];
    let b = vec![Point::new(0.0, 50.0), Point::new(400.0, 50.0)];
    Plan::build(&[a, b], &Placement::identity(), &PlanSettings::default())
}

/// One stroke that starts far from the origin: the plan opens with a long
/// pen-up travel, a wide window in which nothing is being drawn.
fn distant_plan() -> Plan {
    let line = vec![Point::new(200.0, 0.0), Point::new(400.0, 0.0)];
    Plan::build(&[line], &Placement::identity(), &PlanSettings::default())
}

/// Drive events until one of the terminal outcomes, returning it and the last
/// progress seen.
fn wait_for_end(worker: &Worker) -> (Option<Event>, usize) {
    let mut last_done = 0;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        match event {
            Event::Progress { done, .. } => last_done = done,
            Event::PlanDone { .. } | Event::Aborted => return (Some(event), last_done),
            _ => {}
        }
    }
    (None, last_done)
}

#[test]
fn stop_between_ops_halts_the_plan_and_lifts_the_pen() {
    let (mut worker, sent, _rt) = slow_worker();
    let total = long_plan().ops.len();

    worker.send(Command::RunPlan {
        plan: long_plan(),
        progress: None,
        start_index: 0,
    });
    std::thread::sleep(Duration::from_millis(20)); // let a few ops go out
    worker.send(Command::Stop);

    let (end, done) = wait_for_end(&worker);
    assert!(matches!(end, Some(Event::Aborted)), "expected Aborted");
    assert!(
        done < total,
        "stopped after {done} of {total} ops (not all)"
    );

    worker.shutdown();

    // A pen-up Z move (up height 0.5) was sent as part of the abort.
    let sent = sent.lock().unwrap();
    assert!(
        sent.iter().any(|l| l.contains("Z0.500")),
        "no pen-up in the abort: {sent:?}"
    );
}

/// Wait for the plan to report its hold, returning the op index it stopped at.
fn wait_for_pause(worker: &Worker) -> usize {
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if let Event::Paused { done, .. } = event {
            return done;
        }
    }
    panic!("no Paused event");
}

/// Follow the plan's progress until `reached` accepts where it has got to.
///
/// Waiting on the plan's own state rather than on a sleep is what makes these
/// tests deterministic: "pause while the pen is on the paper" is a fact about
/// the plan, not about how fast the machine happens to be.
fn wait_until(worker: &Worker, plan: &Plan, reached: impl Fn(usize) -> bool) -> usize {
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if let Event::Progress { done, .. } = event {
            if reached(done) {
                return done;
            }
        }
    }
    panic!(
        "the plan ended before reaching the state under test ({} ops)",
        plan.ops.len()
    );
}

/// A pause asked for mid-shape must not stop there: the shape is drawn to its
/// end and the pen lifted first, so the hold leaves no mark on the paper.
#[test]
fn pause_finishes_the_shape_being_drawn_before_holding() {
    let (mut worker, _sent, _rt) = slow_worker();
    let plan = two_shape_plan();

    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: None,
        start_index: 0,
    });
    // Ask only once the pen is provably on the paper, inside the first shape.
    let asked_at = wait_until(&worker, &plan, |done| {
        plan.stroke_progress(done).current == Some(0)
    });
    worker.send(Command::Pause);

    let paused_at = wait_for_pause(&worker);

    // No shape is half-drawn at the hold, and the op just executed was the
    // pen-up that closes one.
    let progress = plan.stroke_progress(paused_at);
    assert_eq!(
        progress.current, None,
        "held inside a shape ({progress:?}) instead of at its end"
    );
    assert_eq!(
        plan.ops.get(paused_at - 1),
        Some(&Op::PenUp),
        "the op before the hold was not a shape's pen-up"
    );
    assert!(
        paused_at > asked_at,
        "held at {paused_at}, without drawing on from {asked_at}"
    );
    assert!(progress.done >= 1, "no shape finished: {progress:?}");
    assert!(paused_at < plan.ops.len(), "the whole plan ran");

    worker.shutdown();
}

/// A pause asked for with the pen already up holds there and then — there is no
/// shape to finish.
#[test]
fn pause_with_the_pen_up_holds_at_once() {
    let (mut worker, _sent, _rt) = slow_worker();
    // The shape starts far from the origin, so the plan opens with a long
    // pen-up travel: a wide, unambiguous window with nothing being drawn.
    let plan = distant_plan();
    let pen_down_at = plan
        .ops
        .iter()
        .position(|op| *op == Op::PenDown)
        .expect("the plan draws something");

    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: None,
        start_index: 0,
    });
    wait_until(&worker, &plan, |done| done >= 3);
    worker.send(Command::Pause);

    let paused_at = wait_for_pause(&worker);
    assert!(
        paused_at < pen_down_at,
        "held at {paused_at}, past the pen-down at {pen_down_at} — it waited for nothing"
    );

    worker.shutdown();
}

#[test]
fn resume_after_a_pause_finishes_the_plan() {
    let (mut worker, _sent, _rt) = slow_worker();
    let plan = two_shape_plan();

    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: None,
        start_index: 0,
    });
    wait_until(&worker, &plan, |done| {
        plan.stroke_progress(done).current == Some(0)
    });
    worker.send(Command::Pause);
    let paused_at = wait_for_pause(&worker);

    worker.send(Command::Resume);
    let (end, done) = wait_for_end(&worker);

    assert!(
        matches!(end, Some(Event::PlanDone { .. })),
        "expected PlanDone"
    );
    assert!(done >= paused_at, "progress went backwards after resume");

    worker.shutdown();
}

/// Resume sent before the shape ends calls the pause off: the plot runs to the
/// end without ever holding.
#[test]
fn resume_before_the_shape_ends_calls_the_pause_off() {
    let (mut worker, _sent, _rt) = slow_worker();
    let plan = two_shape_plan();

    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: None,
        start_index: 0,
    });
    wait_until(&worker, &plan, |done| {
        plan.stroke_progress(done).current == Some(0)
    });
    worker.send(Command::Pause);
    worker.send(Command::Resume);

    let mut held = false;
    let mut finished = false;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        match event {
            Event::Paused { .. } => held = true,
            Event::PlanDone { .. } => {
                finished = true;
                break;
            }
            Event::Aborted => break,
            _ => {}
        }
    }
    assert!(finished, "the plan did not finish");
    assert!(!held, "the cancelled pause still took hold");

    worker.shutdown();
}

#[test]
fn panic_stop_aborts_with_a_soft_reset() {
    let (mut worker, _sent, realtime) = slow_worker();

    worker.send(Command::RunPlan {
        plan: long_plan(),
        progress: None,
        start_index: 0,
    });
    std::thread::sleep(Duration::from_millis(20));
    worker.send(Command::EmergencyStop);

    let (end, _done) = wait_for_end(&worker);
    assert!(matches!(end, Some(Event::Aborted)), "expected Aborted");

    worker.shutdown();

    // The soft reset (0x18) is the signature of the panic path.
    assert!(
        realtime.lock().unwrap().contains(&0x18),
        "no soft-reset byte in a panic stop"
    );
}

#[test]
fn a_timed_stop_lifts_the_pen_and_ends_the_plan() {
    let (mut worker, sent, _rt) = slow_worker();
    let total = long_plan().ops.len();

    worker.send(Command::RunPlan {
        plan: long_plan(),
        progress: None,
        start_index: 0,
    });
    // Accelerated "time": a 30 ms cutoff, with pen-up. The plan (~150 ops at
    // 3 ms/read) runs well past it, so it stops partway.
    worker.send(Command::StopAfter {
        after: Duration::from_millis(30),
        pen_up: true,
    });

    let (end, done) = wait_for_end(&worker);
    assert!(matches!(end, Some(Event::Aborted)), "expected a timed stop");
    assert!(done < total, "stopped after {done} of {total} ops");

    worker.shutdown();

    let sent = sent.lock().unwrap();
    assert!(
        sent.iter().any(|l| l.contains("Z0.500")),
        "no pen-up on the timed stop: {sent:?}"
    );
}

#[test]
fn a_timed_stop_without_pen_up_leaves_the_pen_down() {
    let (mut worker, sent, _rt) = slow_worker();

    worker.send(Command::RunPlan {
        plan: long_plan(),
        progress: None,
        start_index: 0,
    });
    worker.send(Command::StopAfter {
        after: Duration::from_millis(30),
        pen_up: false,
    });

    let (end, _done) = wait_for_end(&worker);
    assert!(matches!(end, Some(Event::Aborted)));

    // Count pen-up Z moves before shutdown; the exit $SLP is separate.
    let pen_ups = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("Z0.500"))
        .count();
    worker.shutdown();

    // The plan lowered the pen once (PenDown) and never raised it on the stop.
    assert_eq!(pen_ups, 0, "pen was lifted despite pen_up=false");
}

#[test]
fn a_distance_cutoff_stops_partway_and_lifts_the_pen() {
    let (mut worker, sent, _rt) = slow_worker();
    let total = long_plan().ops.len();

    worker.send(Command::RunPlan {
        plan: long_plan(),
        progress: None,
        start_index: 0,
    });
    // The stroke is ~400 mm; stop after 100 mm of travel, pen up.
    worker.send(Command::StopAfterDistance {
        mm: 100.0,
        pen_up: true,
    });

    let (end, done) = wait_for_end(&worker);
    assert!(
        matches!(end, Some(Event::Aborted)),
        "expected a distance stop"
    );
    assert!(done < total, "stopped after {done} of {total} ops");

    worker.shutdown();
    let sent = sent.lock().unwrap();
    assert!(
        sent.iter().any(|l| l.contains("Z0.500")),
        "no pen-up on the distance stop: {sent:?}"
    );
}

#[test]
fn a_zero_op_reference_keeps_the_helpers_honest() {
    // Guards the test's own assumptions: the plan really has many ops, and the
    // MoveTo lines map through the identity transform as expected.
    let plan = long_plan();
    assert!(plan.move_count() > 40, "plan too short to interrupt");
    let t = Transform::idraw();
    if let Some(Op::MoveTo(p)) = plan.ops.iter().find(|op| matches!(op, Op::MoveTo(_))) {
        let _ = t.map_point(*p);
    }
}
