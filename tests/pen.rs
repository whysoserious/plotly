//! Integration test for step 1.3: the exact wire traffic of a pen move.
//!
//! The order is the point, and there are two reasons for it.
//!
//! `F` is modal in Grbl, so the Z line must come first and the XY feed line
//! right after it — otherwise every later XY move would travel at the pen's Z
//! feed rate (DESIGN.org §2.2).
//!
//! The two `G4`s around the Z move are fences. A dwell runs only once the
//! planner buffer has emptied, so they turn "queued" into "done": the line is
//! finished before the pen moves, and the pen is there before XY moves again.
//! Without them Grbl's look-ahead carries speed through the corner and the pen
//! draws a hook as it lifts (`Driver::set_pen`).

use plotly::plotter::driver::{Driver, Pen, PenFence};
use plotly::plotter::mock::MockTransport;
use plotly::plotter::Connection;

fn driver_with_log() -> (Driver, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let transport = MockTransport::new();
    let sent = transport.sent_handle();
    let driver = Driver::new(Connection {
        transport: Box::new(transport),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    (driver, sent)
}

#[test]
fn pen_down_is_fenced_off_from_the_motion_around_it() {
    let (mut driver, sent) = driver_with_log();

    driver.pen_down().expect("the mock always answers ok");

    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            "G4 P0.01".to_owned(),
            "G1 G90 Z5.000 F5000".to_owned(),
            // No dwell after the landing: the settle is zero, and waiting for
            // the board to confirm a move whose ordering Grbl already
            // guarantees cost 298 ms with the tip on the paper (§2.7).
            "G1 F2000".to_owned(),
        ]
    );
    assert_eq!(driver.pen(), Pen::Down);
}

#[test]
fn pen_up_uses_the_raised_z_and_keeps_the_same_shape() {
    let (mut driver, sent) = driver_with_log();

    driver.pen_down().unwrap();
    sent.lock().unwrap().clear();
    driver.pen_up().unwrap();

    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            "G4 P0.01".to_owned(),
            "G1 G90 Z0.500 F5000".to_owned(),
            "G4 P0.050".to_owned(),
            "G1 F2000".to_owned(),
        ]
    );
    assert_eq!(driver.pen(), Pen::Up);
}

#[test]
fn toggle_alternates_between_the_two_z_heights() {
    let (mut driver, sent) = driver_with_log();

    driver.toggle_pen().unwrap();
    driver.toggle_pen().unwrap();

    let lines = sent.lock().unwrap().clone();
    let z_moves: Vec<&String> = lines.iter().filter(|l| l.contains('Z')).collect();
    assert_eq!(z_moves, vec!["G1 G90 Z5.000 F5000", "G1 G90 Z0.500 F5000"]);
    assert_eq!(driver.pen(), Pen::Up);
}

/// Lifting still fences on the far side, because there the settle is real:
/// stillness in the air costs only time, and it is what stops the pen swinging.
#[test]
fn a_lift_keeps_its_settle_but_a_landing_does_not() {
    let (mut driver, sent) = driver_with_log();
    driver.pen_down().unwrap();
    sent.lock().unwrap().clear();
    driver.pen_up().unwrap();

    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            "G4 P0.01".to_owned(),
            "G1 G90 Z0.500 F5000".to_owned(),
            "G4 P0.050".to_owned(),
            "G1 F2000".to_owned(),
        ]
    );
}

/// `off` is the traffic as it was before the fences: the Z move and the feed,
/// nothing else. Kept reachable because a fence that turns out not to fence
/// anything is pure cost, and the operator should be able to drop it without a
/// rebuild.
#[test]
fn the_fence_can_be_switched_off_entirely() {
    let (mut driver, sent) = driver_with_log();
    let mut profile = plotly::profiles::Profile::builtin("idraw-a0").unwrap();
    profile.pen.fence = PenFence::Off;
    driver.apply_profile(&profile);

    driver.pen_down().unwrap();

    let sent = sent.lock().unwrap();
    assert!(
        !sent.iter().any(|l| l.starts_with("G4")),
        "the off fence must not dwell: {sent:?}"
    );
    assert_eq!(sent.len(), 2, "only the Z move and the feed: {sent:?}");
}

/// `poll` asks the machine what it is doing instead of trusting `G4` to wait.
/// The dwell disappears from the wire; realtime `?` bytes take its place.
#[test]
fn the_poll_fence_asks_the_machine_instead_of_dwelling() {
    let transport = MockTransport::new();
    let sent = transport.sent_handle();
    let realtime = transport.realtime_handle();
    let mut driver = Driver::new(Connection {
        transport: Box::new(transport),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    let mut profile = plotly::profiles::Profile::builtin("idraw-a0").unwrap();
    profile.pen.fence = PenFence::Poll;
    profile.pen.settle_up_secs = 0.0;
    driver.apply_profile(&profile);

    driver.pen_down().unwrap();

    assert_eq!(
        *sent.lock().unwrap(),
        vec!["G1 G90 Z5.000 F5000".to_owned(), "G1 F2000".to_owned()],
        "a polling fence sends no G-code of its own"
    );
    let polls = realtime.lock().unwrap();
    assert!(
        polls.iter().filter(|b| **b == b'?').count() >= 2,
        "the pen move should be fenced on both sides: {polls:?}"
    );
    assert_eq!(driver.pen(), Pen::Down);
}
