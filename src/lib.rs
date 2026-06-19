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
    if args.simulate {
        probe_mock();
    }

    run_tui(log)
}

/// Enter the terminal, wire restore-on-panic/-signal, and run the TUI app.
fn run_tui(log: logging::LogRing) -> io::Result<()> {
    let _guard = tui::TerminalGuard::enter()?;
    tui::install_panic_restore();
    tui::install_signal_restore();

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    app::App::new(log).run(&mut terminal)
}

/// Temporary 0.6 demo: probe the mock transport so `--simulate` shows wire
/// traffic in the log. Replaced by the worker that owns a `Transport` (step 2.4).
fn probe_mock() {
    use plotter::mock::MockTransport;
    use plotter::transport::Transport;

    let mut transport = MockTransport::new();
    let _ = transport.send_line("v");
    if let Ok(version) = transport.read_line() {
        tracing::info!(%version, "mock transport probe (--simulate)");
    }
}
