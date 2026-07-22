//! Logging setup: file appender + in-TUI log ring, plus the panic hook.
//! See DESIGN.org §5.
//!
//! Two `tracing` layers run in parallel: a non-blocking file appender and a
//! bounded in-memory ring buffer that the TUI's log panel tails. Both share one
//! plain-text format (`<ts.mmm+zz:zz>  LEVEL target: message field=value`).

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{ChronoLocal, FormatTime};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::cli::{Args, LogLevel};

/// Local timestamp with millisecond precision and timezone offset (DESIGN.org §5).
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";

/// Maximum number of formatted log lines retained for the in-TUI panel.
const RING_CAPACITY: usize = 1000;

/// Shared, bounded ring buffer of formatted log lines for the TUI log panel.
///
/// Cloning shares the underlying buffer (used both as a `MakeWriter` layer sink
/// and as a read handle for rendering).
#[derive(Clone, Default)]
pub struct LogRing {
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl LogRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of retained lines.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("log ring poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The last `n` lines (oldest-first), for rendering the live tail.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let buf = self.inner.lock().expect("log ring poisoned");
        let start = buf.len().saturating_sub(n);
        buf.iter().skip(start).cloned().collect()
    }

    fn push_line(&self, line: String) {
        let mut buf = self.inner.lock().expect("log ring poisoned");
        if buf.len() >= RING_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(line);
    }
}

/// `MakeWriter` sink that funnels each formatted event into the [`LogRing`].
impl<'a> MakeWriter<'a> for LogRing {
    type Writer = RingLineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RingLineWriter {
            ring: self.clone(),
            buf: Vec::new(),
        }
    }
}

/// Per-event writer: buffers bytes, then splits into lines on drop.
pub struct RingLineWriter {
    ring: LogRing,
    buf: Vec<u8>,
}

impl io::Write for RingLineWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RingLineWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.buf);
        for line in text.lines() {
            let line = line.trim_end();
            if !line.is_empty() {
                self.ring.push_line(line.to_owned());
            }
        }
    }
}

/// Map the resolved [`LogLevel`] to a tracing [`LevelFilter`].
pub fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Off => LevelFilter::OFF,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// Build one configured fmt layer over an arbitrary writer (file or ring).
/// ANSI is off so both the log file and the TUI panel stay plain text.
fn fmt_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .event_format(PlotlyFormat::default())
}

/// Line format of DESIGN.org §5: `<timestamp> LEVEL target: message field=value`.
///
/// Written by hand rather than configured on the built-in formatter because of
/// one detail: the crate prefix is stripped from every target. Each line is
/// already inside plotly's own log, so `plotly::` on all of them is noise that
/// costs eight columns in the TUI panel.
struct PlotlyFormat {
    timer: ChronoLocal,
}

impl Default for PlotlyFormat {
    fn default() -> Self {
        Self {
            timer: ChronoLocal::new(TIME_FORMAT.to_owned()),
        }
    }
}

impl<S, N> FormatEvent<S, N> for PlotlyFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        self.timer.format_time(&mut writer)?;
        let meta = event.metadata();
        write!(
            writer,
            " {:>5} {}: ",
            meta.level(),
            short_target(meta.target())
        )?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Drop the crate prefix: `plotly::plotter::handshake` -> `plotter::handshake`.
fn short_target(target: &str) -> &str {
    target
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(target)
}

/// Build a single-layer subscriber over `writer` (used by tests).
#[cfg(test)]
pub fn build_subscriber<W>(filter: LevelFilter, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer(writer))
}

/// Install logging (file + TUI ring layers) and the panic hook.
///
/// Returns the appender's [`WorkerGuard`] (keep alive until exit so buffered
/// lines flush) and the [`LogRing`] the TUI reads. When logging is disabled
/// (`--no-log` or `--log-level off`) no subscriber is installed and the ring
/// stays empty; the panic hook is installed regardless.
pub fn init(args: &Args) -> (Option<WorkerGuard>, LogRing) {
    install_panic_hook();

    let ring = LogRing::new();
    let filter = level_filter(args.resolved_log_level());
    if filter == LevelFilter::OFF {
        return (None, ring);
    }

    let (dir, file) = split_log_path(&args.log_file);
    let appender = tracing_appender::rolling::never(dir, file);
    let (writer, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer(writer))
        .with(fmt_layer(ring.clone()))
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "plotly logging initialized"
    );
    (Some(guard), ring)
}

/// Split a log path into `(directory, file_name)`, defaulting to the CWD.
fn split_log_path(path: &Path) -> (PathBuf, PathBuf) {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file = path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("plotly.log"));
    (dir, file)
}

/// Install a panic hook that records the panic in the log, then chains to the
/// previous hook (so the backtrace still reaches stderr).
///
/// Terminal restore on panic is layered on top in [`crate::tui`] once the TUI
/// owns the screen.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_owned());
        tracing::error!(target: "plotly::panic", location, "panic: {}", panic_message(info));
        previous(info);
    }));
}

/// Best-effort extraction of a panic payload as text.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn level_filter_maps_each_variant() {
        assert_eq!(level_filter(LogLevel::Off), LevelFilter::OFF);
        assert_eq!(level_filter(LogLevel::Info), LevelFilter::INFO);
        assert_eq!(level_filter(LogLevel::Debug), LevelFilter::DEBUG);
        assert_eq!(level_filter(LogLevel::Trace), LevelFilter::TRACE);
    }

    #[test]
    fn no_log_resolves_to_off_filter() {
        let args = Args::try_parse_from(["plotly", "--no-log", "--log-level", "trace"]).unwrap();
        assert_eq!(level_filter(args.resolved_log_level()), LevelFilter::OFF);
    }

    #[test]
    fn split_log_path_defaults_to_cwd() {
        assert_eq!(
            split_log_path(Path::new("plotly.log")),
            (PathBuf::from("."), PathBuf::from("plotly.log"))
        );
        assert_eq!(
            split_log_path(Path::new("./logs/run.log")),
            (PathBuf::from("./logs"), PathBuf::from("run.log"))
        );
    }

    #[test]
    fn log_ring_retains_tail_within_capacity() {
        let ring = LogRing::new();
        assert!(ring.is_empty());
        for i in 0..5 {
            ring.push_line(format!("line {i}"));
        }
        assert_eq!(ring.len(), 5);
        assert_eq!(ring.tail(2), vec!["line 3".to_owned(), "line 4".to_owned()]);
        assert_eq!(ring.tail(100).len(), 5);
    }

    #[test]
    fn log_ring_drops_oldest_past_capacity() {
        let ring = LogRing::new();
        for i in 0..(RING_CAPACITY + 10) {
            ring.push_line(format!("line {i}"));
        }
        assert_eq!(ring.len(), RING_CAPACITY);
        // Oldest 10 dropped: first retained line is "line 10".
        assert_eq!(ring.tail(RING_CAPACITY)[0], "line 10");
    }

    #[test]
    fn ring_writer_captures_formatted_line() {
        let ring = LogRing::new();
        let subscriber = build_subscriber(LevelFilter::INFO, ring.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "plotly::selftest", answer = 42, "hello world");
        });

        let lines = ring.tail(10);
        assert_eq!(lines.len(), 1, "expected one captured line: {lines:?}");
        let line = &lines[0];
        assert!(line.contains(" INFO "), "level missing: {line:?}");
        assert!(line.contains("selftest:"), "target missing: {line:?}");
        assert!(
            !line.contains("plotly::"),
            "crate prefix should be stripped: {line:?}"
        );
        assert!(line.contains("hello world"), "message missing: {line:?}");
        assert!(line.contains("answer=42"), "field missing: {line:?}");

        // First token is the timestamp; require `.` followed by 3 digits (ms).
        let ts = line.split_whitespace().next().unwrap_or_default();
        let ms_ok = ts.contains('T')
            && ts
                .split_once('.')
                .map(|(_, frac)| frac.chars().take(3).all(|c| c.is_ascii_digit()))
                .unwrap_or(false);
        assert!(ms_ok, "millisecond timestamp missing: {ts:?}");
    }
}
