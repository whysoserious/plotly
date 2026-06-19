// Plotly — TUI in Rust for the iDraw 2.0 pen plotter.
// Wires CLI -> logging -> terminal. Rendering and the worker arrive later.

mod cli;
mod logging;
mod tui;

use std::io;

use clap::Parser;

fn main() -> io::Result<()> {
    let args = cli::Args::parse();
    // Keep the appender guard alive for the whole run so logs flush on exit.
    let _log_guard = logging::init(&args);

    if args.panic_test {
        panic!("synthetic panic to exercise the logging panic hook");
    }

    run()
}

/// Enter the terminal, wire restore-on-panic/-signal, and loop until quit.
fn run() -> io::Result<()> {
    let _guard = tui::TerminalGuard::enter()?;
    tui::install_panic_restore();
    tui::install_signal_restore();
    tui::run_until_quit()
}

#[cfg(test)]
mod tests {
    /// Smoke test: pins that `cargo test` actually runs and the crate builds.
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
