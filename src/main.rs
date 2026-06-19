// Plotly — TUI in Rust for the iDraw 2.0 pen plotter.
// CLI parsing lands here; logging (0.3), TUI (0.5) and the worker arrive later.

mod cli;

use clap::Parser;

fn main() {
    let args = cli::Args::parse();
    // Resolve verbosity now; logging init (0.3) will consume it, transport/resume later.
    let _level = args.resolved_log_level();
}

#[cfg(test)]
mod tests {
    /// Smoke test: pins that `cargo test` actually runs and the crate builds.
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
