//! Plotly: a TUI in Rust driving an iDraw 2.0 pen plotter (DrawCore firmware).
//!
//! All functionality lives in the library so the thin binary and integration
//! tests (`tests/`, the plan's "I" tests) share one crate. DESIGN.org §12.

pub mod app;
pub mod cli;
pub mod config;
pub mod fonts;
pub mod geometry;
pub mod job;
pub mod keys;
pub mod logging;
pub mod plan;
pub mod plotter;
pub mod profiles;
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
    // reported on a normal terminal. The shapes are kept as drawing-logical mm
    // and only turned into a plan on `Enter`, once the head is where the
    // operator wants the drawing to start (§2.4). Text (--text) and SVG feed
    // the same pipeline (§7).
    let shapes = if let Some(text) = &args.text {
        let polylines = plan::text::layout(text, args.text_height);
        tracing::info!(%text, height_mm = args.text_height, strokes = polylines.len(), "text loaded");
        // Glyph strokes have no source element to name them.
        polylines.into_iter().map(plan::Shape::unlabelled).collect()
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
                    svg.shapes
                }
                Err(err) => return Err(fail("cannot load the SVG", err)),
            },
            None => Vec::new(),
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

    // Settle the machine profile before anything is planned or moved: it fixes
    // the field, the feeds and the pen heights (§10). The board is asked what
    // it is (`$$`) and the user's TOML gets the last word.
    let mut driver = plotter::driver::Driver::new(connection);
    let reported = match driver.read_settings() {
        Ok(settings) => Some(settings),
        Err(err) => {
            tracing::warn!(%err, "could not read $$; falling back to the profile defaults");
            None
        }
    };
    let user_config = config::load().map_err(|err| fail("cannot use the config file", err))?;
    let profile = profiles::resolve(args.profile.as_deref(), reported.as_ref(), &user_config)
        .map_err(|err| fail("cannot use that profile", err))?;
    driver.apply_profile(&profile);

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
    run_tui(driver, profile, shapes, source, resume, log)
}

/// Build the plan to draw, with the shapes (mm) laid down from `at` — the
/// head's current position — on the machine `profile` describes. Shared by SVG
/// and text; covers the DEBUG checks of §2.2/§2.3 (execution is the worker).
///
/// The drawing's top-left corner goes on `at` rather than the middle of the
/// field: the operator jogs to the corner of their sheet and starts there.
pub fn build_plan(
    shapes: &[plan::Shape],
    at: geometry::Point,
    profile: &profiles::Profile,
) -> plan::Plan {
    let field = profile.field;
    let placement = match bounds_of(shapes) {
        Some(bounds) => {
            let placement = geometry::Placement::anchored_at(bounds, &field, DEFAULT_MARGIN_MM, at);
            let (min, max) = placement.place_bounds(bounds);
            tracing::debug!(
                at_x = at.x,
                at_y = at.y,
                x0 = min.x,
                y0 = min.y,
                x1 = max.x,
                y1 = max.y,
                "placed bbox (mm), anchored at the head"
            );
            // Anchoring can push a drawing past the edge; say so rather than
            // let the carriage find out. Bounds are the host's job (§2.4).
            if !field.contains(min) || !field.contains(max) {
                tracing::warn!(
                    x1 = max.x,
                    y1 = max.y,
                    width_mm = field.width_mm,
                    height_mm = field.height_mm,
                    "the drawing runs off the field from here; move the head or it will hit the frame"
                );
            }
            placement
        }
        None => geometry::Placement::identity(),
    };

    let settings = profile.plan;
    let job = plan::Plan::build_shapes(shapes, &placement, &settings);
    let dry_run = job.estimate(&plan::estimate::Machine::from_profile(profile));
    tracing::debug!(
        ops = job.ops.len(),
        strokes = job.stroke_count(),
        moves = job.move_count(),
        labelled = job.stroke_labels().len(),
        draw_mm = dry_run.draw_mm,
        travel_mm = dry_run.travel_mm,
        est_secs = dry_run.secs,
        cap_mm = settings.max_segment_mm,
        "plan built"
    );
    job
}

/// Axis-aligned bounds of a set of shapes, or `None` when empty.
pub fn bounds_of(shapes: &[plan::Shape]) -> Option<(geometry::Point, geometry::Point)> {
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
    profile: profiles::Profile,
    shapes: Vec<plan::Shape>,
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
    app::App::new(worker, machine, profile, shapes, source, resume, log).run(&mut terminal)
}
