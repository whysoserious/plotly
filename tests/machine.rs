//! Integration test for step 1.4: homing and motor release, from the key press
//! down to the bytes on the wire.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use plotly::keys::{action_for, Action};
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

#[test]
fn h_is_bound_to_homing_and_sends_exactly_that() {
    assert_eq!(
        action_for(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        Some(Action::Home)
    );

    let (mut driver, sent) = driver_with_log();
    driver.home().expect("the mock answers ok");

    assert_eq!(*sent.lock().unwrap(), vec!["$H".to_owned()]);
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
        action_for(&KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        Some(Action::DisableMotors)
    );

    let (mut driver, sent) = driver_with_log();
    driver.disable_motors().expect("the mock answers ok");

    assert_eq!(*sent.lock().unwrap(), vec!["$SLP".to_owned()]);
}
