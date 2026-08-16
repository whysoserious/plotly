//! Plotly: a TUI in Rust driving an iDraw 2.0 pen plotter (DrawCore firmware).
//!
//! All functionality lives in the library so the thin binary and integration
//! tests (`tests/`, the plan's "I" tests) share one crate. DESIGN.org §12.

pub mod app;
pub mod cli;
pub mod fonts;
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
    // Text (--text) and SVG feed the same pipeline (§7).
    let plan = if let Some(text) = &args.text {
        let polylines = plan::text::layout(text, args.text_height);
        tracing::info!(%text, height_mm = args.text_height, strokes = polylines.len(), "text loaded");
        // Glyph strokes have no source element to name them.
        let shapes: Vec<plan::Shape> = polylines.into_iter().map(plan::Shape::unlabelled).collect();
        Some(build_plan(&shapes))
    } else {
        match &args.svg_file {
            Some(path) => match plan::svg::load(path) {
                Ok(svg) => {
                    tracing::info!(
                        file = %path.display(),
                        shapes = svg.shape_count(),
                        points = svg.point_count(),
                        "SVG loaded"
                    );
                    Some(build_plan(&svg.shapes))
                }
                Err(err) => return Err(fail("cannot load the SVG", err)),
            },
            None => None,
        }
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

    let source = match (&args.text, &args.svg_file) {
        (Some(text), _) => Some(format!("text: {text}")),
        (None, Some(path)) => Some(path.display().to_string()),
        (None, None) => None,
    };
    // Offer to resume an unfinished job from a previous run (§3.3). Started
    // bare, any unfinished job qualifies. Started on a file or some text —
    // the natural way to come back to a paused print is to repeat the command
    // that began it — only a job made from that same source does, so asking
    // for a different drawing never lands on someone else's leftovers.
    let resume = job::jobs_root().and_then(|root| match &source {
        Some(source) => job::latest_resumable_from(&root, source),
        None => job::latest_resumable(&root),
    });
    if let Some(candidate) = &resume {
        tracing::info!(
            job = candidate.meta.job_id,
            percent = candidate.percent(),
            "unfinished job found; offering resume"
        );
    }
    run_tui(
        plotter::driver::Driver::new(connection),
        plan,
        source,
        resume,
        log,
    )
}

/// Fit shapes (mm) to the field and build the plan to draw. Shared by SVG and
/// text; covers the DEBUG checks of §2.2/§2.3 (execution is the worker).
fn build_plan(shapes: &[plan::Shape]) -> plan::Plan {
    let field = geometry::Field::idraw_a0();
    let placement = match bounds_of(shapes) {
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
    let job = plan::Plan::build_shapes(shapes, &placement, &settings);
    tracing::debug!(
        ops = job.ops.len(),
        strokes = job.stroke_count(),
        moves = job.move_count(),
        labelled = job.stroke_labels().len(),
        draw_mm = job.total_stroke_length_mm(),
        cap_mm = settings.max_segment_mm,
        "plan built"
    );
    job
}

/// Axis-aligned bounds of a set of shapes, or `None` when empty.
fn bounds_of(shapes: &[plan::Shape]) -> Option<(geometry::Point, geometry::Point)> {
    let mut points = shapes.iter().flat_map(|s| &s.points);
    let first = *points.next()?;
    let (mut min, mut max) = (first, first);
    for p in points {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    Some((min, max))
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
    resume: Option<job::Resumable>,
    log: logging::LogRing,
) -> io::Result<()> {
    let _guard = tui::TerminalGuard::enter()?;
    tui::install_panic_restore();
    tui::install_signal_handler();

    // Snapshot the identity before the driver moves onto the worker thread.
    let machine = plotter::worker::MachineState {
        version: driver.version().to_owned(),
        port: driver.port().to_owned(),
        pen: driver.pen(),
        position: driver.position(),
    };
    let worker = plotter::worker::Worker::spawn(driver);

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    app::App::new(worker, machine, plan, source, resume, log).run(&mut terminal)
}
