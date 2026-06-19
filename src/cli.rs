//! Command-line interface (clap derive). See DESIGN.org §13.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Plotly — TUI to drive an iDraw 2.0 pen plotter (DrawCore firmware).
#[derive(Debug, Parser)]
#[command(name = "plotly", version, about)]
pub struct Args {
    /// SVG file to load and draw.
    #[arg(value_name = "SVG_FILE")]
    pub svg_file: Option<PathBuf>,

    /// Force the serial port path; default is auto-detect (CH340 1A86:7523/8040).
    #[arg(long, value_name = "PATH")]
    pub port: Option<String>,

    /// Serial baud rate (firmware is fixed at 115200; rarely needed).
    #[arg(long, value_name = "N", default_value_t = 115_200)]
    pub baud: u32,

    /// Machine profile name (e.g. idraw-a4, idraw-a3).
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Use the in-process MockTransport instead of real hardware.
    #[arg(long)]
    pub simulate: bool,

    /// Log file path.
    #[arg(long, value_name = "PATH", default_value = "./plotly.log")]
    pub log_file: PathBuf,

    /// Log level; overridden by -v/-vv and --no-log.
    #[arg(long, value_enum, value_name = "LEVEL", default_value_t = LogLevel::Info)]
    pub log_level: LogLevel,

    /// Increase verbosity: -v = debug, -vv = trace (overrides --log-level).
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
}

/// Effective logging verbosity. `Off` disables logging entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    Off,
    Info,
    Debug,
    Trace,
}

impl Args {
    /// Resolve the effective log level.
    ///
    /// Precedence: `--no-log` > `-v`/`-vv` > `--log-level` (> default `info`).
    pub fn resolved_log_level(&self) -> LogLevel {
        if self.no_log {
            return LogLevel::Off;
        }
        match self.verbose {
            0 => self.log_level,
            1 => LogLevel::Debug,
            _ => LogLevel::Trace,
        }
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

    #[test]
    fn default_log_level_is_info() {
        assert_eq!(parse(&[]).resolved_log_level(), LogLevel::Info);
    }

    #[test]
    fn single_v_is_debug() {
        assert_eq!(parse(&["-v"]).resolved_log_level(), LogLevel::Debug);
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
        assert_eq!(a.log_file, PathBuf::from("./plotly.log"));
        assert!(a.svg_file.is_none());
        assert!(a.port.is_none());
        assert!(a.profile.is_none());
        assert!(a.resume.is_none());
        assert!(!a.simulate);
        assert!(!a.no_log);
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
}
