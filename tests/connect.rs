//! Integration test for step 1.2: the whole `--simulate` startup path, from
//! resolving the port to a greeted plotter, through the public API only.

use plotly::plotter::mock::MOCK_VERSION;
use plotly::plotter::serial::{resolve_port, PortChoice, DEFAULT_BAUD};

#[test]
fn simulate_resolves_and_completes_the_handshake() {
    let choice = resolve_port(true, None).expect("--simulate always resolves");
    assert_eq!(choice, PortChoice::Mock);

    let connection = plotly::plotter::connect(&choice, DEFAULT_BAUD)
        .expect("the mock plotter must complete the handshake");

    assert_eq!(connection.version, MOCK_VERSION);
    assert_eq!(connection.port, "mock");
}

#[test]
fn opening_a_nonexistent_port_fails_before_any_handshake() {
    let choice = PortChoice::Serial("/dev/definitely-not-a-plotter".to_owned());
    let err = plotly::plotter::connect(&choice, DEFAULT_BAUD)
        .expect_err("a missing device cannot be opened");

    let message = err.to_string();
    assert!(
        message.contains("cannot open the serial port"),
        "unexpected message: {message}"
    );
}
