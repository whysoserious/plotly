//! Integration test for step 2.4: the worker thread draws a plan on the mock,
//! emitting one progress event per op and the right G-code per MoveTo.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use plotly::geometry::{Placement, Point, Transform};
use plotly::plan::{Op, Plan, PlanSettings};
use plotly::plotter::driver::Driver;
use plotly::plotter::mock::MockTransport;
use plotly::plotter::worker::{Command, Event, Worker};
use plotly::plotter::Connection;

const TIMEOUT: Duration = Duration::from_secs(5);

fn worker_on_mock() -> (Worker, Arc<Mutex<Vec<String>>>) {
    let mock = MockTransport::new();
    let sent = mock.sent_handle();
    let driver = Driver::new(Connection {
        transport: Box::new(mock),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    let worker = Worker::spawn(driver);
    // Drop the initial State snapshot the worker emits on startup.
    worker.recv_timeout(TIMEOUT);
    (worker, sent)
}

/// The wire lines a plan's MoveTo ops should produce, in order.
fn expected_move_lines(plan: &Plan) -> Vec<String> {
    let t = Transform::idraw();
    plan.ops
        .iter()
        .filter_map(|op| match op {
            Op::MoveTo(p) => {
                let w = t.map_point(*p);
                Some(format!("G1 X{:.3} Y{:.3}", w.x + 0.0, w.y + 0.0))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn worker_draws_the_whole_plan_in_order_with_progress() {
    let polyline = vec![
        Point::new(0.0, 0.0),
        Point::new(10.0, 0.0),
        Point::new(10.0, 10.0),
    ];
    let plan = Plan::build(
        &[polyline],
        &Placement::identity(),
        &PlanSettings::default(),
    );
    let total = plan.ops.len();
    let expected = expected_move_lines(&plan);

    let (mut worker, sent) = worker_on_mock();
    worker.send(Command::RunPlan(plan));

    // Collect progress until the plan finishes.
    let mut progress = Vec::new();
    let mut done_seen = false;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        match event {
            Event::Progress { done, total } => progress.push((done, total)),
            Event::PlanDone => {
                done_seen = true;
                break;
            }
            Event::Error(err) => panic!("unexpected error: {err}"),
            _ => {}
        }
    }
    assert!(done_seen, "the plan never reported PlanDone");

    // One progress event per op, strictly increasing, ending at total.
    assert_eq!(progress.len(), total, "one progress per op");
    for (i, (done, reported_total)) in progress.iter().enumerate() {
        assert_eq!(*done, i + 1);
        assert_eq!(*reported_total, total);
    }

    // Shut down first: the mock shares its `sent` lock, and shutdown now sends
    // $SLP, so inspecting the log while holding the lock would deadlock.
    worker.shutdown();

    // Every MoveTo produced the expected wire line, in order.
    let sent = sent.lock().unwrap();
    let moves: Vec<String> = sent
        .iter()
        .filter(|l| l.starts_with("G1 X"))
        .cloned()
        .collect();
    assert_eq!(moves, expected);
}

#[test]
fn motors_are_released_on_shutdown() {
    let (mut worker, sent) = worker_on_mock();
    worker.shutdown();

    // The worker's last act is $SLP, leaving the machine safe on exit.
    let sent = sent.lock().unwrap();
    assert_eq!(
        sent.last().map(String::as_str),
        Some("$SLP"),
        "sent: {sent:?}"
    );
}

#[test]
fn a_y_zero_move_prints_without_negative_zero() {
    // Machine-logical (10, 0) → wire (10, -0.0); the line must read Y0.000.
    let plan = Plan {
        ops: vec![Op::MoveTo(Point::new(10.0, 0.0))],
    };
    let (mut worker, sent) = worker_on_mock();
    worker.send(Command::RunPlan(plan));

    // Wait for the plan to finish.
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::PlanDone | Event::Aborted) {
            break;
        }
    }
    worker.shutdown();

    let sent = sent.lock().unwrap();
    assert!(
        sent.iter().any(|l| l == "G1 X10.000 Y0.000"),
        "sent lines: {sent:?}"
    );
}
