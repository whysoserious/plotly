// Plotly — TUI in Rust for the iDraw 2.0 pen plotter.
// Wires CLI -> logging; TUI (0.5) and the worker arrive later.

mod cli;
mod logging;

use clap::Parser;

fn main() {
    let args = cli::Args::parse();
    // Keep the appender guard alive for the whole run so logs flush on exit.
    let _log_guard = logging::init(&args);

    if args.panic_test {
        panic!("synthetic panic to exercise the logging panic hook");
    }
}

#[cfg(test)]
mod tests {
    /// Smoke test: pins that `cargo test` actually runs and the crate builds.
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
