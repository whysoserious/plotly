//! User configuration: machine-profile overrides in TOML. DESIGN.org §10 /
//! step 5.1.
//!
//! The file is optional and everything in it is optional. It exists so the
//! numbers that depend on *your* setup rather than on the model — above all the
//! pen-down Z for a particular pen and paper (§15.3) — live somewhere other
//! than a rebuild.
//!
//! ```toml
//! # <config_dir>/plotly/config.toml
//! [profiles.idraw-a0]
//! pen_down_z = 5.4      # this pen needs a little more
//! draw_feed = 1500      # and a slower hand
//! ```

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const CONFIG_FILE: &str = "config.toml";

/// The whole config file. Unknown keys are rejected rather than ignored: a
/// silently dropped `pen_dwon_z` is a setting the user believes is in effect.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Overrides per profile name, e.g. `[profiles.idraw-a3]`.
    #[serde(default)]
    pub profiles: HashMap<String, ProfileOverride>,
}

impl Config {
    /// The overrides for `name`, or an empty set.
    pub fn overrides_for(&self, name: &str) -> ProfileOverride {
        self.profiles.get(name).cloned().unwrap_or_default()
    }
}

/// What a user may override for one profile. Every field is optional; what is
/// not mentioned keeps the built-in — or firmware-reported — value.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileOverride {
    /// Drawable width, mm (`$130`).
    pub width_mm: Option<f64>,
    /// Drawable height, mm (`$131`).
    pub height_mm: Option<f64>,
    /// Z with the pen lifted. Larger Z is *lower* on this machine (§2.2).
    pub pen_up_z: Option<f32>,
    /// Z with the pen on the paper — the one worth calibrating per pen (§15.3).
    pub pen_down_z: Option<f32>,
    /// Feed for the pen's own Z move, mm/min.
    pub pen_z_feed: Option<u32>,
    /// Stillness after the pen's Z move, seconds — raise it if a spring-loaded
    /// holder still marks the paper as it lifts.
    pub pen_settle_secs: Option<f64>,
    /// Feed while drawing, mm/min.
    pub draw_feed: Option<u32>,
    /// Feed while travelling with the pen up, mm/min.
    pub travel_feed: Option<u32>,
    /// Feed for interactive jogging, mm/min.
    pub jog_feed: Option<u32>,
    /// Fastest XY feed to allow, mm/min. Other feeds are clamped to it.
    pub max_feed: Option<u32>,
    /// Longest single plan segment, mm — the stop/resume granularity (§6).
    pub max_segment_mm: Option<f64>,
}

/// Why a config file could not be used.
#[derive(Debug)]
pub enum ConfigError {
    Read { path: PathBuf, err: io::Error },
    Parse { path: PathBuf, err: toml::de::Error },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, err } => write!(f, "cannot read {}: {err}", path.display()),
            Self::Parse { path, err } => write!(f, "{} is not valid: {err}", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Where the config file lives, or `None` if the platform exposes no home.
pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("me", "ziniewicz", "plotly")
        .map(|dirs| dirs.config_dir().join(CONFIG_FILE))
}

/// Load the user's config, or the defaults when there is no file.
///
/// A file that exists but does not parse is an *error*, not a shrug: it was
/// written to change how the machine behaves, and carrying on with defaults
/// would quietly plot at the wrong pen height.
pub fn load() -> Result<Config, ConfigError> {
    match config_path() {
        Some(path) => load_from(&path),
        None => Ok(Config::default()),
    }
}

/// Load a config from an explicit path; a missing file is the default config.
pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "no config file; using defaults");
            return Ok(Config::default());
        }
        Err(err) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                err,
            })
        }
    };
    let config: Config = toml::from_str(&text).map_err(|err| ConfigError::Parse {
        path: path.to_path_buf(),
        err,
    })?;
    tracing::info!(
        path = %path.display(),
        profiles = config.profiles.len(),
        "config loaded"
    );
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_override_leaves_everything_else_unset() {
        let config: Config = toml::from_str(
            r#"
            [profiles.idraw-a0]
            pen_down_z = 5.4
            draw_feed = 1500
            "#,
        )
        .expect("valid config");

        let over = config.overrides_for("idraw-a0");
        assert_eq!(over.pen_down_z, Some(5.4));
        assert_eq!(over.draw_feed, Some(1500));
        assert_eq!(over.pen_up_z, None);
        assert_eq!(over.width_mm, None);
    }

    #[test]
    fn a_profile_with_no_entry_overrides_nothing() {
        let config = Config::default();
        let over = config.overrides_for("idraw-a3");
        assert!(over.width_mm.is_none() && over.draw_feed.is_none());
    }

    #[test]
    fn every_field_round_trips_from_toml() {
        let config: Config = toml::from_str(
            r#"
            [profiles.custom]
            width_mm = 300.0
            height_mm = 210.0
            pen_up_z = 0.4
            pen_down_z = 5.2
            pen_z_feed = 4000
            pen_settle_secs = 0.08
            draw_feed = 1800
            travel_feed = 7000
            jog_feed = 2500
            max_feed = 9000
            max_segment_mm = 2.5
            "#,
        )
        .expect("valid config");

        let o = config.overrides_for("custom");
        assert_eq!(o.width_mm, Some(300.0));
        assert_eq!(o.height_mm, Some(210.0));
        assert_eq!(o.pen_up_z, Some(0.4));
        assert_eq!(o.pen_down_z, Some(5.2));
        assert_eq!(o.pen_z_feed, Some(4000));
        assert_eq!(o.pen_settle_secs, Some(0.08));
        assert_eq!(o.draw_feed, Some(1800));
        assert_eq!(o.travel_feed, Some(7000));
        assert_eq!(o.jog_feed, Some(2500));
        assert_eq!(o.max_feed, Some(9000));
        assert_eq!(o.max_segment_mm, Some(2.5));
    }

    /// A typo must not be swallowed: the user believes the setting is live.
    #[test]
    fn an_unknown_key_is_an_error_not_a_shrug() {
        let err = toml::from_str::<Config>(
            r#"
            [profiles.idraw-a0]
            pen_dwon_z = 5.4
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("pen_dwon_z"),
            "the error should name the key: {err}"
        );
    }

    #[test]
    fn a_missing_file_is_the_default_config_not_an_error() {
        let missing = std::env::temp_dir().join("plotly-no-such-config-xyz.toml");
        let config = load_from(&missing).expect("a missing file is fine");
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn a_broken_file_is_reported_with_its_path() {
        let path =
            std::env::temp_dir().join(format!("plotly-bad-config-{}.toml", std::process::id()));
        std::fs::write(&path, "[profiles.idraw-a0\nwidth_mm = ").unwrap();

        let err = load_from(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);

        assert!(
            matches!(&err, ConfigError::Parse { .. }),
            "expected a parse error, got {err:?}"
        );
        assert!(
            err.to_string().contains("plotly-bad-config"),
            "the message should name the file: {err}"
        );
    }

    #[test]
    fn the_config_path_sits_under_a_plotly_directory() {
        let path = config_path().expect("a home directory on this platform");
        assert!(path.ends_with(CONFIG_FILE), "unexpected path: {path:?}");
        assert!(
            path.to_string_lossy().contains("plotly"),
            "unexpected path: {path:?}"
        );
    }
}
