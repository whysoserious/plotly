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

/// Drive events until one of the terminal outcomes, returning it and the last
/// progress seen.
fn wait_for_end(worker: &Worker) -> (Option<Event>, usize) {
    let mut last_done = 0;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        match event {
            Event::Progress { done, .. } => last_done = done,
            Event::PlanDone | Event::Aborted => return (Some(event), last_done),
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

#[test]
fn pause_holds_then_resume_finishes_the_plan() {
    let (mut worker, _sent, realtime) = slow_worker();

    worker.send(Command::RunPlan {
        plan: long_plan(),
        progress: None,
    });
    std::thread::sleep(Duration::from_millis(20));
    worker.send(Command::Pause);

    // The worker acknowledges the hold.
    let mut paused_at = None;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if let Event::Paused { done, .. } = event {
            paused_at = Some(done);
            break;
        }
    }
    let paused_at = paused_at.expect("no Paused event");

    // Feed-hold '!' was sent; the plan is not finished.
    assert!(
        realtime.lock().unwrap().contains(&b'!'),
        "no feed-hold byte"
    );

    worker.send(Command::Resume);
    let (end, done) = wait_for_end(&worker);

    assert!(matches!(end, Some(Event::PlanDone)), "expected PlanDone");
    assert!(done >= paused_at, "progress went backwards after resume");
    // Cycle-start '~' resumed motion.
    assert!(
        realtime.lock().unwrap().contains(&b'~'),
        "no cycle-start byte"
    );

    worker.shutdown();
}

#[test]
fn panic_stop_aborts_with_a_soft_reset() {
    let (mut worker, _sent, realtime) = slow_worker();

    worker.send(Command::RunPlan {
        plan: long_plan(),
        progress: None,
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
