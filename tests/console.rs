//! Integration test for step 1.5: the raw G-code console and its modal input.
//!
//! The case the plan names is `M3 S100`: every character of it, `S` included,
//! must reach the line buffer instead of triggering a command (DESIGN.org §8).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use plotly::keys::{action_for, Action, Mode};
use plotly::plotter::driver::Driver;
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

/// Replay a string through the console key map, as the app's buffer would.
fn type_line(text: &str) -> String {
    let mut buffer = String::new();
    for c in text.chars() {
        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        match action_for(Mode::Console, &key) {
            Some(Action::Input(c)) => buffer.push(c),
            other => panic!("{c:?} in the console produced {other:?}, not text"),
        }
    }
    buffer
}

#[test]
fn typing_m3_s100_produces_text_not_commands() {
    assert_eq!(type_line("M3 S100"), "M3 S100");
}

#[test]
fn the_same_keys_are_commands_outside_the_console() {
    let s = KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE);
    assert_eq!(
        action_for(Mode::Navigation, &s),
        Some(Action::EmergencyStop)
    );
    assert_eq!(action_for(Mode::Console, &s), Some(Action::Input('S')));
}

#[test]
fn a_typed_line_reaches_the_wire_verbatim_and_the_reply_comes_back() {
    let (mut driver, sent) = driver_with_log();

    let replies = driver
        .send_raw(&type_line("M3 S100"))
        .expect("the mock answers ok");

    assert_eq!(*sent.lock().unwrap(), vec!["M3 S100".to_owned()]);
    assert_eq!(replies, vec!["ok".to_owned()]);
}

#[test]
fn a_refusal_is_shown_as_a_reply_rather_than_failing() {
    let mut transport = MockTransport::unresponsive();
    transport.push_unsolicited("error:20");
    let mut driver = Driver::new(Connection {
        transport: Box::new(transport),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });

    let replies = driver
        .send_raw("G999")
        .expect("a refused command is an answer, not a program failure");
    assert_eq!(replies, vec!["error:20".to_owned()]);
}
