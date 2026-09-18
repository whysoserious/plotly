//! Integration test for step 1.3: the exact wire traffic of a pen move.
//!
//! The order is the point. `F` is modal in Grbl, so the Z line must come first
//! and the XY feed line right after it — otherwise every later XY move would
//! travel at the pen's Z feed rate (DESIGN.org §2.2).
//!
//! What is *not* there any more is the pair of `G4` fences around the Z move.
//! A dwell runs only once the planner buffer has emptied, so they turned
//! "queued" into "done": the line was finished before the pen moved, and the
//! pen was there before XY moved again. The theory was that without them
//! Grbl's look-ahead carries speed through the corner and the pen draws a hook
//! as it lifts — and a paired print said otherwise (§2.7), so the default is
//! now no fence at all and the traffic is two lines (§2.10). Both fences are
//! still reachable, and still tested, below.

use plotly::plotter::driver::{Driver, Pen, PenFence};
use plotly::plotter::mock::MockTransport;
use plotly::plotter::Connection;
use plotly::profiles::Profile;

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

/// A driver whose pen settings come from `edit`ing the A0 profile.
fn driver_with_pen(
    edit: impl FnOnce(&mut Profile),
) -> (Driver, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let (mut driver, sent) = driver_with_log();
    let mut profile = Profile::builtin("idraw-a0").unwrap();
    edit(&mut profile);
    driver.apply_profile(&profile);
    (driver, sent)
}

#[test]
fn pen_down_is_the_z_move_and_the_feed_that_follows_it() {
    let (mut driver, sent) = driver_with_log();

    driver.pen_down().expect("the mock always answers ok");

    assert_eq!(
        *sent.lock().unwrap(),
        vec!["G1 G90 Z5.000 F5000".to_owned(), "G1 F2000".to_owned(),]
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
        vec!["G1 G90 Z0.500 F5000".to_owned(), "G1 F2000".to_owned(),]
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

/// A settle asked for without a fence is queued behind the Z move rather than
/// waited out by the host: the machine holds still for exactly as long as was
/// asked, and nothing costs a round trip. A zero settle emits nothing at all,
/// because `G4 P0.000` would empty the planner for no stillness whatever.
#[test]
fn a_settle_without_a_fence_is_queued_and_only_when_asked_for() {
    let (mut driver, sent) = driver_with_pen(|p| {
        p.pen.settle_up_secs = 0.05;
        p.pen.settle_down_secs = 0.0;
    });

    driver.pen_down().unwrap();
    assert_eq!(
        *sent.lock().unwrap(),
        vec!["G1 G90 Z5.000 F5000".to_owned(), "G1 F2000".to_owned()],
        "a landing with no settle dwells for nothing"
    );

    sent.lock().unwrap().clear();
    driver.pen_up().unwrap();
    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            "G1 G90 Z0.500 F5000".to_owned(),
            "G4 P0.050".to_owned(),
            "G1 F2000".to_owned(),
        ]
    );
}

/// `dwell` is the traffic as it shipped until 2026-09-18: a barrier on each
/// side of the Z move. Kept reachable because the measurement that retired it
/// was about one pen on one machine.
#[test]
fn the_dwell_fence_brackets_the_z_move_with_barriers() {
    let (mut driver, sent) = driver_with_pen(|p| {
        p.pen.fence = PenFence::Dwell;
        p.pen.settle_up_secs = 0.05;
    });

    driver.pen_down().unwrap();
    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            "G4 P0.01".to_owned(),
            "G1 G90 Z5.000 F5000".to_owned(),
            // `P0.000` — no stillness asked for, but under this fence the
            // barrier still goes out: it is what makes the pen land at a
            // standstill instead of leaving the Z-to-XY junction already
            // moving sideways.
            "G4 P0.000".to_owned(),
            "G1 F2000".to_owned(),
        ]
    );

    sent.lock().unwrap().clear();
    driver.pen_up().unwrap();
    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            "G4 P0.01".to_owned(),
            "G1 G90 Z0.500 F5000".to_owned(),
            "G4 P0.050".to_owned(),
            "G1 F2000".to_owned(),
        ],
        "the lift carries its settle in the far barrier"
    );
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
    let mut profile = Profile::builtin("idraw-a0").unwrap();
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
