//! Plotly: a TUI in Rust driving an iDraw 2.0 pen plotter (DrawCore firmware).
//!
//! All functionality lives in the library so the thin binary and integration
//! tests (`tests/`, the plan's "I" tests) share one crate. DESIGN.org §12.

pub mod app;
pub mod cli;
pub mod geometry;
pub mod job;
pub mod keys;
pub mod logging;
pub mod plan;
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

    // Load the drawing early (before touching hardware), so a broken SVG is
    // reported on a normal terminal, and build the plan to draw on `Enter`.
    let plan = match &args.svg_file {
        Some(path) => match plan::svg::load(path) {
            Ok(svg) => Some(prepare_plan(path, &svg)),
            Err(err) => return Err(fail("cannot load the SVG", err)),
        },
        None => None,
    };

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

    let source = args.svg_file.as_ref().map(|p| p.display().to_string());
    run_tui(plotter::driver::Driver::new(connection), plan, source, log)
}

/// Log the loaded drawing, fit it to the field, and build the plan to draw.
/// Covers the DEBUG checks of §2.2/§2.3; execution is the worker (step 2.4).
fn prepare_plan(path: &std::path::Path, svg: &plan::svg::Svg) -> plan::Plan {
    tracing::info!(
        file = %path.display(),
        paths = svg.path_count(),
        points = svg.point_count(),
        "SVG loaded"
    );
    let field = geometry::Field::idraw_a0();
    let placement = match svg.bounds_mm() {
        Some(bounds) => {
            let placement = geometry::Placement::fit(bounds, &field, DEFAULT_MARGIN_MM);
            let (min, max) = placement.place_bounds(bounds);
            tracing::debug!(
                x0 = min.x,
                y0 = min.y,
                x1 = max.x,
                y1 = max.y,
                "placed bbox (mm) after fit to field"
            );
            placement
        }
        None => geometry::Placement::identity(),
    };

    let settings = plan::PlanSettings::default();
    let job = plan::Plan::build(&svg.polylines, &placement, &settings);
    tracing::debug!(
        ops = job.ops.len(),
        strokes = job.stroke_count(),
        moves = job.move_count(),
        cap_mm = settings.max_segment_mm,
        "plan built"
    );
    job
}

/// Default margin left around the drawing when fitting to the field (mm).
const DEFAULT_MARGIN_MM: f64 = 5.0;

/// Report a startup failure on stderr and in the log, as an `io::Error`.
fn fail<E: std::error::Error + Send + Sync + 'static>(context: &str, err: E) -> io::Error {
    tracing::error!(%err, "{context}");
    eprintln!("plotly: {err}");
    io::Error::other(err)
}

/// Enter the terminal, wire restore-on-panic/-signal, spawn the worker, and run
/// the TUI app against it.
fn run_tui(
    driver: plotter::driver::Driver,
    plan: Option<plan::Plan>,
    source: Option<String>,
    log: logging::LogRing,
) -> io::Result<()> {
    let _guard = tui::TerminalGuard::enter()?;
    tui::install_panic_restore();
    tui::install_signal_restore();

    // Snapshot the identity before the driver moves onto the worker thread.
    let machine = plotter::worker::MachineState {
        version: driver.version().to_owned(),
        port: driver.port().to_owned(),
        pen: driver.pen(),
        position: driver.position(),
    };
    let worker = plotter::worker::Worker::spawn(driver);

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    app::App::new(worker, machine, plan, source, log).run(&mut terminal)
}
