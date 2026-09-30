//! Command-line interface (clap derive). See DESIGN.org §13.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use clap::{Parser, ValueEnum};

/// Where a run's log goes unless `--log-file` says otherwise: one file per run,
/// relative to the working directory, kept out of git (`.gitignore`).
pub const LOG_DIR: &str = "logs";

/// Longest drawing name that goes into a log file's name, in characters. The
/// generators put their whole parameter set in the SVG's name, and a file
/// system caps a name at 255 bytes.
const LOG_NAME_MAX: usize = 150;

/// Plotly — TUI to drive an iDraw 2.0 pen plotter (DrawCore firmware).
#[derive(Debug, Parser)]
#[command(name = "plotly", version, about)]
pub struct Args {
    /// SVG file to load and draw.
    #[arg(value_name = "SVG_FILE")]
    pub svg_file: Option<PathBuf>,

    /// Write this text with the built-in single-stroke font instead of an SVG.
    #[arg(long, value_name = "STRING")]
    pub text: Option<String>,

    /// Cap height for --text, in millimetres.
    #[arg(long, value_name = "MM", default_value_t = 10.0)]
    pub text_height: f64,

    /// Force the serial port path; default is auto-detect (CH340 1A86:7523/8040).
    #[arg(long, value_name = "PATH")]
    pub port: Option<String>,

    /// Serial baud rate (firmware is fixed at 115200; rarely needed).
    #[arg(long, value_name = "N", default_value_t = 115_200)]
    pub baud: u32,

    /// Machine profile (idraw-a0, idraw-a1, idraw-a2, idraw-a3, idraw-a4,
    /// idraw-xlx, idraw-b6, idraw-minikit). Default: read from the machine.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Use the in-process MockTransport instead of real hardware.
    #[arg(long)]
    pub simulate: bool,

    /// Log file path. Default: a new file for every run in ./logs/, named after
    /// the start time and the drawing.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Log level for the file; overridden by -v/-vv and --no-log. The default
    /// records every line on the wire, so a plot can be debugged afterwards.
    #[arg(long, value_enum, value_name = "LEVEL", default_value_t = LogLevel::Trace)]
    pub log_level: LogLevel,

    /// Raise verbosity to at least -v = debug, -vv = trace.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Disable logging entirely (wins over --log-level and -v).
    #[arg(long)]
    pub no_log: bool,

    /// Resume an interrupted job: --resume (latest) or --resume=<JOB_ID>.
    #[arg(long, value_name = "JOB_ID", require_equals = true, num_args = 0..=1)]
    pub resume: Option<Option<u64>>,

    /// Repeat the last K pen-down segments when resuming (for ink continuity).
    #[arg(long, value_name = "K", default_value_t = 0)]
    pub resume_overlap: u32,

    /// Trigger a synthetic panic right after logging init (verifies the panic hook).
    #[arg(long, hide = true)]
    pub panic_test: bool,
}

/// Effective logging verbosity. `Off` disables logging entirely. Ordered from
/// quiet to loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum LogLevel {
    Off,
    Info,
    Debug,
    Trace,
}

impl Args {
    /// Resolve the effective log level.
    ///
    /// `--no-log` wins; otherwise `--log-level` (default `trace`), raised by
    /// `-v`/`-vv` but never lowered by them.
    pub fn resolved_log_level(&self) -> LogLevel {
        if self.no_log {
            return LogLevel::Off;
        }
        let floor = match self.verbose {
            0 => LogLevel::Off,
            1 => LogLevel::Debug,
            _ => LogLevel::Trace,
        };
        self.log_level.max(floor)
    }

    /// The log file for a run started at `now`: `--log-file` if given, else
    /// `logs/<date time> <drawing>.log`.
    ///
    /// One file per run rather than one file for ever: a plot is hours of wire
    /// traffic, and the question afterwards is always about one plot. The name
    /// sorts by time and says which drawing it was, so the log for "the
    /// tesseract of Sunday" is found without opening anything.
    pub fn log_path(&self, now: DateTime<Local>) -> PathBuf {
        if let Some(path) = &self.log_file {
            return path.clone();
        }
        let stamp = now.format("%Y-%m-%d %H.%M.%S");
        Path::new(LOG_DIR).join(format!("{stamp} {}.log", self.drawing_name()))
    }

    /// What this run draws, made safe for a file name.
    fn drawing_name(&self) -> String {
        let raw = match (&self.text, &self.svg_file) {
            (Some(text), _) => format!("text {text}"),
            (None, Some(path)) => path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "drawing".to_owned()),
            (None, None) => "no drawing".to_owned(),
        };
        let safe: String = raw
            .chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                c if c.is_control() => '_',
                c => c,
            })
            .take(LOG_NAME_MAX)
            .collect();
        safe.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse from a synthetic argv (program name prepended).
    fn parse(args: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("plotly").chain(args.iter().copied()))
            .expect("args should parse")
    }

    #[test]
    fn cli_config_is_valid() {
        // Catches clap misconfiguration (conflicting settings, bad value parsers, ...).
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    /// The `--profile` help spells the names out; keep it honest against the
    /// table they actually come from.
    #[test]
    fn the_help_lists_every_built_in_profile() {
        use clap::CommandFactory;
        let help = Args::command().render_long_help().to_string();
        for name in crate::profiles::Profile::names() {
            assert!(
                help.contains(name),
                "--profile help does not mention {name}:\n{help}"
            );
        }
    }

    /// Loud by default: a plot that went wrong is debugged from its log, and
    /// the log of a plot cannot be turned up afterwards.
    #[test]
    fn default_log_level_is_trace() {
        assert_eq!(parse(&[]).resolved_log_level(), LogLevel::Trace);
    }

    #[test]
    fn single_v_raises_a_quiet_level_to_debug() {
        assert_eq!(
            parse(&["--log-level", "info", "-v"]).resolved_log_level(),
            LogLevel::Debug
        );
    }

    /// `-v` asks for more, so under the trace default it must not mean less.
    #[test]
    fn v_never_lowers_the_level() {
        assert_eq!(parse(&["-v"]).resolved_log_level(), LogLevel::Trace);
    }

    #[test]
    fn double_v_is_trace() {
        assert_eq!(parse(&["-vv"]).resolved_log_level(), LogLevel::Trace);
        assert_eq!(parse(&["-v", "-v"]).resolved_log_level(), LogLevel::Trace);
    }

    #[test]
    fn no_log_wins_over_log_level() {
        assert_eq!(
            parse(&["--no-log", "--log-level", "trace"]).resolved_log_level(),
            LogLevel::Off
        );
    }

    #[test]
    fn no_log_wins_over_verbose() {
        assert_eq!(
            parse(&["--no-log", "-vv"]).resolved_log_level(),
            LogLevel::Off
        );
    }

    #[test]
    fn explicit_log_level_is_used_without_verbose() {
        assert_eq!(
            parse(&["--log-level", "debug"]).resolved_log_level(),
            LogLevel::Debug
        );
        assert_eq!(
            parse(&["--log-level", "off"]).resolved_log_level(),
            LogLevel::Off
        );
    }

    #[test]
    fn defaults_match_section_13() {
        let a = parse(&[]);
        assert_eq!(a.baud, 115_200);
        assert_eq!(a.resume_overlap, 0);
        assert_eq!(a.log_file, None);
        assert!(a.svg_file.is_none());
        assert!(a.port.is_none());
        assert!(a.profile.is_none());
        assert!(a.resume.is_none());
        assert!(!a.simulate);
        assert!(!a.no_log);
    }

    fn at_noon() -> DateTime<Local> {
        use chrono::TimeZone;
        Local.with_ymd_and_hms(2026, 9, 28, 16, 55, 54).unwrap()
    }

    #[test]
    fn each_run_logs_to_its_own_file_named_after_the_drawing() {
        let args = parse(&["/home/me/blocks tesseract seed14397.svg"]);
        assert_eq!(
            args.log_path(at_noon()),
            PathBuf::from("logs/2026-09-28 16.55.54 blocks tesseract seed14397.log")
        );
    }

    #[test]
    fn a_run_without_a_drawing_still_gets_a_named_log() {
        assert_eq!(
            parse(&[]).log_path(at_noon()),
            PathBuf::from("logs/2026-09-28 16.55.54 no drawing.log")
        );
    }

    /// Text is typed by the user and may hold anything, a slash included.
    #[test]
    fn text_is_made_safe_for_a_file_name() {
        let path = parse(&["--text", "a/b: c?"]).log_path(at_noon());
        assert_eq!(
            path,
            PathBuf::from("logs/2026-09-28 16.55.54 text a_b_ c_.log")
        );
    }

    #[test]
    fn a_long_drawing_name_is_cut_to_fit_a_file_name() {
        let long = format!("{}.svg", "x".repeat(400));
        let path = parse(&[long.as_str()]).log_path(at_noon());
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.len() < 255, "{} bytes", name.len());
    }

    #[test]
    fn an_explicit_log_file_wins() {
        assert_eq!(
            parse(&["--log-file", "/tmp/x.log", "a.svg"]).log_path(at_noon()),
            PathBuf::from("/tmp/x.log")
        );
    }

    #[test]
    fn resume_has_three_states() {
        assert_eq!(parse(&[]).resume, None);
        assert_eq!(parse(&["--resume"]).resume, Some(None));
        assert_eq!(parse(&["--resume=7"]).resume, Some(Some(7)));
    }

    #[test]
    fn positional_svg_file_is_captured() {
        assert_eq!(
            parse(&["logo.svg"]).svg_file,
            Some(PathBuf::from("logo.svg"))
        );
    }

    #[test]
    fn panic_test_flag_defaults_off_and_parses() {
        assert!(!parse(&[]).panic_test);
        assert!(parse(&["--panic-test"]).panic_test);
    }
}
