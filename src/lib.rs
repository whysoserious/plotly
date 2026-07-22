//! Plotly: a TUI in Rust driving an iDraw 2.0 pen plotter (DrawCore firmware).
//!
//! All functionality lives in the library so the thin binary and integration
//! tests (`tests/`, the plan's "I" tests) share one crate. DESIGN.org §12.

pub mod app;
pub mod cli;
pub mod logging;
pub mod plotter;
pub mod tui;
pub mod ui;

use std::io;

use clap::Parser;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Parse arguments, set up logging + the terminal, and run the TUI loop.
pub fn run() -> io::Result<()> {
    let args = cli::Args::parse();
    // Keep the appender guard alive for the whole run so logs flush on exit.
    let (_log_guard, log) = logging::init(&args);

    if args.panic_test {
        panic!("synthetic panic to exercise the logging panic hook");
    }

    // Resolve and greet the plotter before entering the alternate screen, so
    // failures land on a normal terminal instead of being wiped by the TUI.
    let port = plotter::serial::resolve_port(args.simulate, args.port.as_deref())
        .map_err(|err| fail("no plotter to connect to", err))?;
    match &port {
        plotter::serial::PortChoice::Mock => tracing::info!("using the mock plotter (--simulate)"),
        plotter::serial::PortChoice::Serial(path) => {
            tracing::info!(%path, baud = args.baud, "plotter port selected");
        }
    }
    let connection =
        plotter::connect(&port, args.baud).map_err(|err| fail("handshake failed", err))?;

    run_tui(plotter::driver::Driver::new(connection), log)
}

/// Report a startup failure on stderr and in the log, as an `io::Error`.
fn fail<E: std::error::Error + Send + Sync + 'static>(context: &str, err: E) -> io::Error {
    tracing::error!(%err, "{context}");
    eprintln!("plotly: {err}");
    io::Error::other(err)
}

/// Enter the terminal, wire restore-on-panic/-signal, and run the TUI app.
fn run_tui(driver: plotter::driver::Driver, log: logging::LogRing) -> io::Result<()> {
    let _guard = tui::TerminalGuard::enter()?;
    tui::install_panic_restore();
    tui::install_signal_restore();

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    app::App::new(driver, log).run(&mut terminal)
}
