//! Integration test for step 1.3: the exact wire traffic of a pen move.
//!
//! The order is the point. `F` is modal in Grbl, so the Z line must come first
//! and the XY feed line right after it — otherwise every later XY move would
//! travel at the pen's Z feed rate (DESIGN.org §2.2).

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
fn pen_down_sends_the_z_move_then_the_xy_feed() {
    let (mut driver, sent) = driver_with_log();

    driver.pen_down().expect("the mock always answers ok");

    assert_eq!(
        *sent.lock().unwrap(),
        vec!["G1 G90 Z5.000 F5000".to_owned(), "G1 F2000".to_owned()]
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
        vec!["G1 G90 Z0.500 F5000".to_owned(), "G1 F2000".to_owned()]
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
