// Plotly — TUI in Rust for the iDraw 2.0 pen plotter.
// Wires CLI -> logging -> terminal -> TUI app. The plotter worker arrives later.

mod app;
mod cli;
mod logging;
mod tui;
mod ui;

use std::io;

use clap::Parser;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

fn main() -> io::Result<()> {
    let args = cli::Args::parse();
    // Keep the appender guard alive for the whole run so logs flush on exit.
    let (_log_guard, log) = logging::init(&args);

    if args.panic_test {
        panic!("synthetic panic to exercise the logging panic hook");
    }

    run(log)
}

/// Enter the terminal, wire restore-on-panic/-signal, and run the TUI loop.
fn run(log: logging::LogRing) -> io::Result<()> {
    let _guard = tui::TerminalGuard::enter()?;
    tui::install_panic_restore();
    tui::install_signal_restore();

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    app::App::new(log).run(&mut terminal)
}

#[cfg(test)]
mod tests {
    /// Smoke test: pins that `cargo test` actually runs and the crate builds.
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
