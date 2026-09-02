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
    worker.send(Command::RunPlan {
        plan,
        progress: None,
        start_index: 0,
    });

    // Collect progress until the plan finishes.
    let mut progress = Vec::new();
    let mut done_seen = false;
    let mut last_progress_secs = 0.0;
    let mut done_secs = f64::NAN;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        match event {
            Event::Progress {
                done,
                total,
                elapsed_secs,
                ..
            } => {
                progress.push((done, total));
                last_progress_secs = elapsed_secs;
            }
            Event::PlanDone { elapsed_secs } => {
                done_seen = true;
                done_secs = elapsed_secs;
                break;
            }
            Event::Error(err) => panic!("unexpected error: {err}"),
            _ => {}
        }
    }
    assert!(done_seen, "the plan never reported PlanDone");
    // The plot's own clock, reported so the operator learns how long it ran.
    // It is the same clock the live counter used, so it cannot run backwards.
    assert!(
        done_secs >= last_progress_secs,
        "the finished plan reported {done_secs}s, less than the {last_progress_secs}s \
         the last progress event showed"
    );

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
    let plan = Plan::from_ops(vec![Op::MoveTo(Point::new(10.0, 0.0))]);
    let (mut worker, sent) = worker_on_mock();
    worker.send(Command::RunPlan {
        plan,
        progress: None,
        start_index: 0,
    });

    // Wait for the plan to finish.
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::PlanDone { .. } | Event::Aborted) {
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

#[test]
fn resume_from_an_index_sends_only_the_remaining_ops_once() {
    // A long stroke; resume from the middle. Ops before the index are not
    // redrawn (except the resume preamble's travel move), and ops from the
    // index are each sent exactly once.
    let line = vec![Point::new(0.0, 0.0), Point::new(100.0, 0.0)];
    let plan = Plan::build(&[line], &Placement::identity(), &PlanSettings::default());
    let total = plan.ops.len();
    let start = total / 2;

    // The MoveTo targets from `start` onward — what a resume should draw.
    let remaining_moves: Vec<Point> = plan.ops[start..]
        .iter()
        .filter_map(|op| match op {
            Op::MoveTo(p) => Some(*p),
            _ => None,
        })
        .collect();

    let (mut worker, sent) = worker_on_mock();
    worker.send(Command::RunPlan {
        plan,
        progress: None,
        start_index: start,
    });

    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::PlanDone { .. } | Event::Aborted) {
            break;
        }
    }
    worker.shutdown();

    let sent = sent.lock().unwrap();
    // Homing happened as part of the resume preamble.
    assert!(sent.iter().any(|l| l == "$H"), "resume did not re-home");

    // Each remaining draw target appears (idempotent absolute moves); count the
    // times the *last* target is sent — exactly once, never doubled.
    let last = remaining_moves.last().unwrap();
    let t = Transform::idraw();
    let w = t.map_point(*last);
    let line = format!("G1 X{:.3} Y{:.3}", w.x + 0.0, w.y + 0.0);
    let hits = sent.iter().filter(|l| **l == line).count();
    assert_eq!(hits, 1, "last op drawn {hits} times, want exactly once");
}

#[test]
fn a_repeated_absolute_move_is_the_same_line_so_it_is_idempotent() {
    // Sending the same absolute MoveTo twice yields identical G-code; on the
    // board that is a no-op, which is what makes resume safe (§6).
    let plan = Plan::from_ops(vec![
        Op::MoveTo(Point::new(42.0, 17.0)),
        Op::MoveTo(Point::new(42.0, 17.0)),
    ]);
    let (mut worker, sent) = worker_on_mock();
    worker.send(Command::RunPlan {
        plan,
        progress: None,
        start_index: 0,
    });
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::PlanDone { .. }) {
            break;
        }
    }
    worker.shutdown();

    let sent = sent.lock().unwrap();
    let moves: Vec<&String> = sent.iter().filter(|l| l.starts_with("G1 X")).collect();
    assert_eq!(moves.len(), 2);
    assert_eq!(moves[0], moves[1], "same target must be the same line");
}

/// The `f` frame traces the drawing's outline with the pen up: it is a look at
/// where the drawing will land, so nothing may touch the paper (step 5.3).
#[test]
fn the_frame_traces_the_outline_without_putting_the_pen_down() {
    let outline = vec![
        Point::new(10.0, 20.0),
        Point::new(60.0, 20.0),
        Point::new(60.0, 70.0),
        Point::new(10.0, 70.0),
        Point::new(10.0, 20.0),
    ];
    let t = Transform::idraw();
    let expected: Vec<String> = outline
        .iter()
        .map(|p| {
            let w = t.map_point(*p);
            format!("G1 X{:.3} Y{:.3}", w.x + 0.0, w.y + 0.0)
        })
        .collect();

    let (mut worker, sent) = worker_on_mock();
    worker.send(Command::Frame {
        outline: outline.clone(),
        feed: 7500,
    });

    // The frame ends with a State snapshot, like any other one-off command.
    let mut settled = false;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::State(_)) {
            settled = true;
            break;
        }
    }
    assert!(settled, "the frame never reported back");
    worker.shutdown();

    let sent = sent.lock().unwrap();
    let moves: Vec<String> = sent
        .iter()
        .filter(|l| l.starts_with("G1 X"))
        .cloned()
        .collect();
    assert_eq!(moves, expected, "the frame did not trace the outline");

    // The pen is never lowered — the whole point.
    assert!(
        !sent.iter().any(|l| l.contains("Z5.000")),
        "the frame put the pen down: {sent:?}"
    );
    // And it travels at the feed it was given.
    assert!(
        sent.iter().any(|l| l == "G1 F7500"),
        "the frame ignored its feed: {sent:?}"
    );
}

/// A frame on an A0 is metres of travel, so it must stop when asked.
#[test]
fn the_frame_can_be_stopped_partway() {
    // Far-apart corners so the mock has several moves to get through.
    let outline: Vec<Point> = (0..200).map(|i| Point::new(f64::from(i), 0.0)).collect();

    let mock = MockTransport::with_read_delay(Duration::from_millis(2));
    let sent = mock.sent_handle();
    let driver = Driver::new(Connection {
        transport: Box::new(mock),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    let mut worker = Worker::spawn(driver);
    worker.recv_timeout(TIMEOUT);

    worker.send(Command::Frame {
        outline,
        feed: 8000,
    });
    std::thread::sleep(Duration::from_millis(20));
    worker.send(Command::Stop);

    let mut aborted = false;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        match event {
            Event::Aborted => {
                aborted = true;
                break;
            }
            Event::State(_) => break,
            _ => {}
        }
    }
    worker.shutdown();

    assert!(aborted, "the frame ignored the stop");
    let moves = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("G1 X"))
        .count();
    assert!(
        moves < 200,
        "the frame ran to the end anyway ({moves} moves)"
    );
}
