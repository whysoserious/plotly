//! Logging setup: non-blocking file appender + panic hook. See DESIGN.org §5.
//!
//! Logs go to a file (not stdout) because the TUI owns the terminal. The line
//! format is `<ts.mmm+zz:zz>  LEVEL target: message field=value` so a pasted log
//! is enough to replay a session.

use std::path::{Path, PathBuf};

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::util::SubscriberInitExt;

use crate::cli::{Args, LogLevel};

/// Local timestamp with millisecond precision and timezone offset (DESIGN.org §5).
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";

/// Map the resolved [`LogLevel`] to a tracing [`LevelFilter`].
pub fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Off => LevelFilter::OFF,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// Build the file/format subscriber over an arbitrary writer.
///
/// Shared by [`init`] (non-blocking file writer) and tests (temp-file writer).
/// ANSI is off so the log file stays plain text.
pub fn build_subscriber<W>(filter: LevelFilter, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_timer(ChronoLocal::new(TIME_FORMAT.to_owned()))
        .with_target(true)
        .with_max_level(filter)
        .finish()
}

/// Install logging and the panic hook for the running app.
///
/// Returns the appender's [`WorkerGuard`]; the caller must keep it alive until
/// exit so buffered lines are flushed. Returns `None` when logging is disabled
/// (`--no-log` or `--log-level off`) — the panic hook is still installed.
pub fn init(args: &Args) -> Option<WorkerGuard> {
    install_panic_hook();

    let filter = level_filter(args.resolved_log_level());
    if filter == LevelFilter::OFF {
        return None;
    }

    let (dir, file) = split_log_path(&args.log_file);
    let appender = tracing_appender::rolling::never(dir, file);
    let (writer, guard) = tracing_appender::non_blocking(appender);

    build_subscriber(filter, writer).init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "plotly logging initialized"
    );
    Some(guard)
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
/// Terminal restore (leaving the alt-screen) is wired in step 0.4 once the TUI
/// owns the screen; until then the default hook's stderr output is fine.
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
    fn log_line_has_ms_timestamp_level_and_target() {
        use std::io::Read;

        let path = std::env::temp_dir().join(format!("plotly-logtest-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let writer_path = path.clone();
        let make = move || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&writer_path)
                .expect("open temp log")
        };

        let subscriber = build_subscriber(LevelFilter::INFO, make);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "plotly::selftest", answer = 42, "hello world");
        });

        let mut contents = String::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(contents.contains(" INFO "), "level missing: {contents:?}");
        assert!(
            contents.contains("plotly::selftest"),
            "target missing: {contents:?}"
        );
        assert!(
            contents.contains("hello world"),
            "message missing: {contents:?}"
        );
        assert!(
            contents.contains("answer=42"),
            "field missing: {contents:?}"
        );

        // First token is the timestamp; require `.` followed by 3 digits (milliseconds).
        let ts = contents.split_whitespace().next().unwrap_or_default();
        let ms_ok = ts.contains('T')
            && ts
                .split_once('.')
                .map(|(_, frac)| frac.chars().take(3).all(|c| c.is_ascii_digit()))
                .unwrap_or(false);
        assert!(ms_ok, "millisecond timestamp missing: {ts:?}");
    }
}
