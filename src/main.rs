// Plotly — TUI in Rust for the iDraw 2.0 pen plotter.
// Thin binary: all logic lives in the `plotly` library crate (src/lib.rs).

fn main() -> std::io::Result<()> {
    plotly::run()
}
