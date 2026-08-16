// Plotly — TUI in Rust for the iDraw 2.0 pen plotter.
// Thin binary: all logic lives in the `plotly` library crate (src/lib.rs).

use std::process::ExitCode;

fn main() -> ExitCode {
    // `plotly::run` has already told the user what went wrong, in their words
    // and on stderr. Returning the error from `main` instead would make Rust
    // print its `Debug` form underneath — a second, uglier copy of the same
    // news. So: report the exit status, not the error.
    match plotly::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
