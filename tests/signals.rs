//! Integration test for step 3.5: a termination signal drives a graceful exit —
//! the app loop stops, and the worker lifts the pen and releases the motors.

use std::sync::{Arc, Mutex};

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use plotly::app::App;
use plotly::geometry::Point;
use plotly::logging::LogRing;
use plotly::plotter::driver::{Driver, Pen};
use plotly::plotter::mock::MockTransport;
use plotly::plotter::worker::{MachineState, Worker};
use plotly::plotter::Connection;
use plotly::tui::request_shutdown;

fn app_over_mock() -> (App, Arc<Mutex<Vec<String>>>) {
    let mock = MockTransport::new();
    let sent = mock.sent_handle();
    let driver = Driver::new(Connection {
        transport: Box::new(mock),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    let worker = Worker::spawn(driver);
    let machine = MachineState {
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
        pen: Pen::Up,
        position: Point::new(0.0, 0.0),
    };
    let app = App::new(worker, machine, Vec::new(), None, None, LogRing::new());
    (app, sent)
}

#[test]
fn a_shutdown_request_exits_the_loop_and_releases_the_motors() {
    let (mut app, sent) = app_over_mock();

    // Ask for a graceful shutdown before the loop starts; the first iteration
    // must see the flag and stop without ever polling the (absent) terminal.
    request_shutdown();

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    app.run(&mut terminal).expect("graceful exit");

    // The worker's shutdown released the motors on the way out (§2.8/§3.5).
    let sent = sent.lock().unwrap();
    assert_eq!(
        sent.last().map(String::as_str),
        Some("$SLP"),
        "motors not released on signal exit: {sent:?}"
    );
}
