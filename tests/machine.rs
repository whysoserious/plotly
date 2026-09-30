//! Integration test for steps 1.4 and 1.6: homing, motor release and jog, from
//! the key press down to the bytes on the wire.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use plotly::keys::{action_for, Action, Mode};
use plotly::plotter::driver::{Driver, Pen};
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

/// `h` homes, and gets the pen out of the way first.
///
/// `$H` drives XY to the far corner and does not touch Z, so a nib left on the
/// paper would be dragged the length of the sheet. The lift goes out whatever
/// the tracked pen state says — that state is a belief the firmware cannot
/// confirm (§15.3), and after a crash it is exactly the belief that is wrong.
#[test]
fn h_is_bound_to_homing_and_clears_the_paper_first() {
    assert_eq!(
        action_for(
            Mode::Navigation,
            &KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)
        ),
        Some(Action::Home)
    );

    let (mut driver, sent) = driver_with_log();
    driver.home().expect("the mock answers ok");

    // The pen-up Z, the feed restore that Grbl's modal `F` needs after it
    // (§2.2), and then the homing move — in that order and nothing else.
    assert_eq!(
        *sent.lock().unwrap(),
        vec![
            "G1 G90 Z0.500 F5000".to_owned(),
            "G1 F2000".to_owned(),
            "$H".to_owned(),
        ]
    );
}

#[test]
fn homing_leaves_the_pen_counted_as_up() {
    let (mut driver, _sent) = driver_with_log();

    driver.pen_down().unwrap();
    assert_eq!(driver.pen(), Pen::Down);

    // Homing zeroes Z, which is above the pen-up height on this machine.
    driver.home().unwrap();
    assert_eq!(driver.pen(), Pen::Up);
}

#[test]
fn d_releases_the_motors_with_a_single_command() {
    assert_eq!(
        action_for(
            Mode::Navigation,
            &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)
        ),
        Some(Action::DisableMotors)
    );

    let (mut driver, sent) = driver_with_log();
    driver.disable_motors().expect("the mock answers ok");

    assert_eq!(*sent.lock().unwrap(), vec!["$SLP".to_owned()]);
}

#[test]
fn right_arrow_jogs_the_carriage_in_wire_plus_x() {
    // Right arrow -> logical +X -> wire +X on this machine (spike 0.7).
    assert_eq!(
        action_for(
            Mode::Navigation,
            &KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)
        ),
        Some(Action::Jog { dx: 1, dy: 0 })
    );

    let (mut driver, sent) = driver_with_log();
    // A 5 mm step to the right, as the app would compute it.
    driver.jog(5.0, 0.0).expect("the mock answers ok");

    assert_eq!(
        *sent.lock().unwrap(),
        vec!["$J=G91 X5.000 F3000".to_owned()]
    );
}

#[test]
fn up_arrow_jog_reaches_the_far_edge_as_wire_plus_y() {
    // Up arrow is "up the page" = logical -Y, which flips to wire +Y.
    let dy = match action_for(
        Mode::Navigation,
        &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    ) {
        Some(Action::Jog { dy, .. }) => dy,
        other => panic!("up arrow produced {other:?}"),
    };

    let (mut driver, sent) = driver_with_log();
    driver
        .jog(0.0, f64::from(dy) * 1.0)
        .expect("the mock answers ok");

    assert_eq!(
        *sent.lock().unwrap(),
        vec!["$J=G91 Y1.000 F3000".to_owned()]
    );
}
