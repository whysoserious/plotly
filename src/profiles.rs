//! Machine profiles: what this particular plotter is and how fast it may move.
//! DESIGN.org §10 / step 5.1.
//!
//! Three sources, resolved in [`resolve`]:
//! 1. a **built-in** profile — the model table of §10, ours (the A0) first;
//! 2. the firmware's **own `$$` settings**, which know the machine's real
//!    travel and speed limits better than any table (§15.1);
//! 3. the user's **TOML**, which overrides whatever it names ([`crate::config`]).
//!
//! Naming a profile is a deliberate statement about which machine is on the
//! other end, so an explicit `--profile` is not second-guessed by `$$`. Without
//! one we take the firmware's word for it.

use crate::config::{Config, ProfileOverride};
use crate::geometry::Field;
use crate::plan::PlanSettings;
use crate::plotter::driver::{GrblSettings, PenSettings};

/// The profile used when none is asked for: our own machine.
pub const DEFAULT_PROFILE: &str = "idraw-a0";

/// Field sizes per model (§10), as `(name, width_mm, height_mm)`.
///
/// The A0 numbers are the ones measured on our machine with `$$` (`$130`/`$131`
/// = 841 × 1189, §15.1), not the vendor table's — that lists the A0 the other
/// way round, and the machine in the room wins.
const MODELS: &[(&str, f64, f64)] = &[
    ("idraw-a0", 841.0, 1189.0),
    ("idraw-a1", 864.0, 594.0),
    ("idraw-a2", 594.0, 432.0),
    ("idraw-a3", 430.0, 297.0),
    ("idraw-a4", 300.0, 210.0),
    ("idraw-xlx", 595.0, 218.0),
    ("idraw-b6", 190.0, 140.0),
    ("idraw-minikit", 160.0, 101.6),
];

/// Grbl setting numbers we care about (`$$`, §15.1).
mod grbl {
    /// Junction deviation, mm — how far off a corner the planner may cut.
    pub const JUNCTION_DEVIATION: u16 = 11;
    /// Max XY feed rates, mm/min.
    pub const MAX_RATE_X: u16 = 110;
    pub const MAX_RATE_Y: u16 = 111;
    /// Acceleration per axis, mm/s².
    pub const ACCEL_X: u16 = 120;
    pub const ACCEL_Y: u16 = 121;
    /// Maximum travel per axis, mm — the drawable field.
    pub const TRAVEL_X: u16 = 130;
    pub const TRAVEL_Y: u16 = 131;
}

/// Everything about the machine that is not protocol: how big it is, how fast
/// it moves, and where the pen sits.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub field: Field,
    pub pen: PenSettings,
    pub plan: PlanSettings,
    /// Feed for interactive jogging, mm/min.
    pub jog_feed: u32,
    /// Fastest XY feed to allow, mm/min — the slower of `$110`/`$111`. Every
    /// other XY feed is clamped to it, so neither a profile nor a slip in a
    /// config file can ask the carriage for more than it has.
    pub max_feed: u32,
    /// Acceleration, mm/s² (`$120`/`$121`). Only the time estimate uses it —
    /// the firmware does its own planning — but it is what makes the estimate
    /// track the machine instead of a constant (step 5.3).
    pub accel_mm_s2: f64,
    /// Junction deviation, mm (`$11`): how far off a corner the planner may
    /// cut, and so how much speed it may carry through one.
    pub junction_deviation_mm: f64,
}

/// Fallback XY speed limit when the firmware has not told us one: the slower of
/// our machine's `$110`/`$111` (15000 / 12000, §15.1).
const DEFAULT_MAX_FEED: u32 = 12_000;

/// Jog feed, well under the machine maximum so a held arrow key cannot slam the
/// carriage at full speed.
pub const DEFAULT_JOG_FEED: u32 = 3_000;

/// Fallback acceleration: our machine's `$120`/`$121` (§15.1).
const DEFAULT_ACCEL_MM_S2: f64 = 3_000.0;

/// Fallback junction deviation: Grbl's own default for `$11`.
const DEFAULT_JUNCTION_DEVIATION_MM: f64 = 0.01;

impl Profile {
    /// The built-in profile called `name`, if there is one.
    pub fn builtin(name: &str) -> Option<Self> {
        let (name, width_mm, height_mm) = MODELS.iter().find(|(model, ..)| *model == name)?;
        Some(Self {
            name: (*name).to_owned(),
            field: Field {
                width_mm: *width_mm,
                height_mm: *height_mm,
            },
            // The §10 defaults are also the code's, so the default profile
            // changes nothing about how the machine has behaved so far.
            pen: PenSettings::default(),
            plan: PlanSettings::default(),
            jog_feed: DEFAULT_JOG_FEED,
            max_feed: DEFAULT_MAX_FEED,
            accel_mm_s2: DEFAULT_ACCEL_MM_S2,
            junction_deviation_mm: DEFAULT_JUNCTION_DEVIATION_MM,
        })
    }

    /// Every built-in profile name, for `--help` and error messages.
    pub fn names() -> impl Iterator<Item = &'static str> {
        MODELS.iter().map(|(name, ..)| *name)
    }

    /// Take the field and speed limits the firmware reports for itself.
    ///
    /// Only what `$$` actually carries is touched: a board that answers
    /// nothing useful leaves the profile as it was.
    pub fn apply_reported(&mut self, reported: &GrblSettings) {
        if let Some(width) = reported.get(grbl::TRAVEL_X) {
            self.field.width_mm = width;
        }
        if let Some(height) = reported.get(grbl::TRAVEL_Y) {
            self.field.height_mm = height;
        }
        // Both axes have to keep up, so the slower one is the limit.
        if let Some(slowest) = slowest_of(reported, grbl::MAX_RATE_X, grbl::MAX_RATE_Y) {
            self.max_feed = slowest as u32;
        }
        if let Some(slowest) = slowest_of(reported, grbl::ACCEL_X, grbl::ACCEL_Y) {
            self.accel_mm_s2 = slowest;
        }
        if let Some(deviation) = reported.get(grbl::JUNCTION_DEVIATION).filter(|d| *d > 0.0) {
            self.junction_deviation_mm = deviation;
        }
        tracing::debug!(
            width_mm = self.field.width_mm,
            height_mm = self.field.height_mm,
            max_feed = self.max_feed,
            accel_mm_s2 = self.accel_mm_s2,
            junction_deviation_mm = self.junction_deviation_mm,
            "field and limits taken from the firmware"
        );
    }

    /// Apply the user's overrides — the last word on any field they name.
    pub fn apply_override(&mut self, over: &ProfileOverride) {
        let ProfileOverride {
            width_mm,
            height_mm,
            pen_up_z,
            pen_down_z,
            pen_z_feed,
            pen_settle_up_secs,
            pen_settle_down_secs,
            pen_fence,
            draw_feed,
            travel_feed,
            jog_feed,
            max_feed,
            max_segment_mm,
            ramp_mm,
            ramp_feed,
            accel_mm_s2,
            junction_deviation_mm,
        } = *over;

        set(&mut self.field.width_mm, width_mm);
        set(&mut self.field.height_mm, height_mm);
        set(&mut self.pen.up_z, pen_up_z);
        set(&mut self.pen.down_z, pen_down_z);
        set(&mut self.pen.z_feed, pen_z_feed);
        // A negative settle would make the dwell an `error:` from the board.
        set(
            &mut self.pen.settle_up_secs,
            pen_settle_up_secs.map(|s| s.max(0.0)),
        );
        set(
            &mut self.pen.settle_down_secs,
            pen_settle_down_secs.map(|s| s.max(0.0)),
        );
        set(&mut self.pen.fence, pen_fence);
        set(&mut self.plan.draw_feed, draw_feed);
        set(&mut self.plan.travel_feed, travel_feed);
        set(&mut self.jog_feed, jog_feed);
        set(&mut self.max_feed, max_feed);
        set(&mut self.plan.max_segment_mm, max_segment_mm);
        set(&mut self.plan.ramp_mm, ramp_mm.map(|mm| mm.max(0.0)));
        set(&mut self.plan.ramp_feed, ramp_feed);
        set(&mut self.accel_mm_s2, accel_mm_s2);
        set(&mut self.junction_deviation_mm, junction_deviation_mm);
    }

    /// The firmware settings this profile wants that the board does not
    /// already have, as `(number, value)` ready for [`Driver::write_setting`].
    ///
    /// Only what the *config* named: a profile carries an acceleration either
    /// way, and writing a built-in default back onto someone's machine would
    /// be a program with opinions about hardware it was only asked to drive.
    /// Values already correct are left alone, so a startup that changes
    /// nothing writes nothing.
    pub fn firmware_writes(
        &self,
        over: &ProfileOverride,
        reported: Option<&GrblSettings>,
    ) -> Vec<(u16, f64)> {
        let wanted = [
            (over.accel_mm_s2, grbl::ACCEL_X, self.accel_mm_s2),
            (over.accel_mm_s2, grbl::ACCEL_Y, self.accel_mm_s2),
            (
                over.junction_deviation_mm,
                grbl::JUNCTION_DEVIATION,
                self.junction_deviation_mm,
            ),
        ];
        wanted
            .into_iter()
            .filter(|(asked, _, _)| asked.is_some())
            .filter(|(_, number, value)| {
                // No dump means no idea what is on the board; write it rather
                // than assume, since the user asked for it by name.
                reported.and_then(|r| r.get(*number)) != Some(*value)
            })
            .map(|(_, number, value)| (number, value))
            .collect()
    }

    /// Hold every XY feed at or below [`Profile::max_feed`].
    ///
    /// The Z feed is left alone: it belongs to a different axis with its own
    /// limit (`$112`), and clamping it to an XY figure would only slow the pen
    /// down for no reason.
    pub fn clamp_feeds(&mut self) {
        for feed in [
            &mut self.plan.draw_feed,
            &mut self.plan.travel_feed,
            &mut self.jog_feed,
        ] {
            if *feed > self.max_feed {
                tracing::warn!(
                    asked = *feed,
                    max_feed = self.max_feed,
                    "feed above the machine limit; clamped"
                );
                *feed = self.max_feed;
            }
        }
        // The pen's XY feed is what a Z move restores afterwards (§2.2), so it
        // is an XY feed too.
        self.pen.xy_feed = self.pen.xy_feed.min(self.max_feed);
        // A ramp above the drawing feed would speed the ends *up*, which is the
        // opposite of the point.
        self.plan.ramp_feed = self.plan.ramp_feed.min(self.plan.draw_feed);
    }

    /// One line for the status bar: `idraw-a0 841×1189`.
    pub fn summary(&self) -> String {
        format!(
            "{} {:.0}×{:.0}",
            self.name, self.field.width_mm, self.field.height_mm
        )
    }
}

/// The slower of two per-axis settings, ignoring anything absent or nonsense —
/// a zero limit would clamp the machine to a standstill.
fn slowest_of(reported: &GrblSettings, x: u16, y: u16) -> Option<f64> {
    [x, y]
        .iter()
        .filter_map(|n| reported.get(*n))
        .filter(|v| *v > 0.0)
        .reduce(f64::min)
}

/// Overwrite `target` when the override carries a value.
fn set<T: Copy>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

/// `--profile` named something that does not exist.
#[derive(Debug)]
pub struct UnknownProfile {
    pub name: String,
}

impl std::fmt::Display for UnknownProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown profile {:?}; known profiles: {}",
            self.name,
            Profile::names().collect::<Vec<_>>().join(", ")
        )
    }
}

impl std::error::Error for UnknownProfile {}

/// Work out the profile to run with.
///
/// `requested` is `--profile`, `reported` is what the board answered to `$$`
/// (absent under `--simulate`, or if the read failed), `config` is the user's
/// TOML. An explicitly requested profile is *not* corrected by `$$`: asking for
/// an A3 while an A0 is plugged in is how you plan a drawing for the machine
/// you are about to move to, and the program should not argue.
pub fn resolve(
    requested: Option<&str>,
    reported: Option<&GrblSettings>,
    config: &Config,
) -> Result<Profile, UnknownProfile> {
    let name = requested.unwrap_or(DEFAULT_PROFILE);
    let mut profile = Profile::builtin(name).ok_or_else(|| UnknownProfile {
        name: name.to_owned(),
    })?;

    match (requested, reported) {
        (None, Some(reported)) => profile.apply_reported(reported),
        (Some(_), Some(_)) => {
            tracing::info!(
                profile = name,
                "profile named explicitly; not taking $$ over it"
            );
        }
        (_, None) => {}
    }

    profile.apply_override(&config.overrides_for(name));
    profile.clamp_feeds();

    // Everything a drawing artefact could be blamed on, on one line. The two
    // firmware numbers are here because they are the ones that turned out to
    // matter and the ones nobody can see: `$120` at its Grbl default threw
    // 0.3 g at an A0 gantry and bent the end of every stroke (§2.5), and the
    // only place that figure appeared was a DEBUG line nobody reads.
    tracing::info!(
        profile = %profile.name,
        width_mm = profile.field.width_mm,
        height_mm = profile.field.height_mm,
        draw_feed = profile.plan.draw_feed,
        travel_feed = profile.plan.travel_feed,
        accel_mm_s2 = profile.accel_mm_s2,
        junction_deviation_mm = profile.junction_deviation_mm,
        pen_down_z = profile.pen.down_z,
        pen_up_z = profile.pen.up_z,
        // How far the tip actually rises. Logged because "the pen is up" is an
        // assumption everywhere else, and a small lift is the first thing to
        // suspect when travel marks the paper (§2.5).
        pen_lift_mm = profile.pen.down_z - profile.pen.up_z,
        pen_z_feed = profile.pen.z_feed,
        // Stillness with the tip on the paper is ink; stillness in the air is
        // only time (§2.7).
        pen_settle_up_secs = profile.pen.settle_up_secs,
        pen_settle_down_secs = profile.pen.settle_down_secs,
        pen_fence = ?profile.pen.fence,
        "machine profile"
    );
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported(pairs: &[(u16, f64)]) -> GrblSettings {
        GrblSettings::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn the_default_profile_is_our_measured_a0() {
        let profile = Profile::builtin(DEFAULT_PROFILE).expect("the default exists");
        assert_eq!(profile.field.width_mm, 841.0);
        assert_eq!(profile.field.height_mm, 1189.0);
    }

    /// The §10 table, so a typo in the constant shows up as a failing test
    /// rather than as a drawing off the edge of someone's A4.
    #[test]
    fn the_built_in_models_have_their_catalogue_sizes() {
        for (name, width, height) in [
            ("idraw-a4", 300.0, 210.0),
            ("idraw-a3", 430.0, 297.0),
            ("idraw-a2", 594.0, 432.0),
            ("idraw-a1", 864.0, 594.0),
            ("idraw-xlx", 595.0, 218.0),
            ("idraw-b6", 190.0, 140.0),
            ("idraw-minikit", 160.0, 101.6),
        ] {
            let profile = Profile::builtin(name).unwrap_or_else(|| panic!("{name} is missing"));
            assert_eq!(profile.field.width_mm, width, "{name} width");
            assert_eq!(profile.field.height_mm, height, "{name} height");
        }
    }

    #[test]
    fn an_unknown_name_is_rejected_with_the_list_of_known_ones() {
        let err = resolve(Some("idraw-a5"), None, &Config::default()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("idraw-a5"), "{message}");
        assert!(message.contains("idraw-a3"), "{message}");
    }

    #[test]
    fn naming_a_profile_changes_the_field() {
        let a3 = resolve(Some("idraw-a3"), None, &Config::default()).unwrap();
        assert_eq!(a3.field.width_mm, 430.0);
        assert_eq!(a3.field.height_mm, 297.0);
    }

    /// Without `--profile`, the machine's own `$$` is the authority on how big
    /// it is and how fast it goes.
    #[test]
    fn the_firmware_report_fills_in_the_field_and_limits() {
        let report = reported(&[(130, 500.0), (131, 700.0), (110, 9000.0), (111, 6000.0)]);
        let profile = resolve(None, Some(&report), &Config::default()).unwrap();

        assert_eq!(profile.field.width_mm, 500.0);
        assert_eq!(profile.field.height_mm, 700.0);
        assert_eq!(profile.max_feed, 6000, "the slower axis is the limit");
    }

    /// Asking for a profile by name is a deliberate statement; `$$` does not
    /// get to overrule it.
    #[test]
    fn an_explicit_profile_survives_a_disagreeing_firmware() {
        let report = reported(&[(130, 841.0), (131, 1189.0)]);
        let profile = resolve(Some("idraw-a4"), Some(&report), &Config::default()).unwrap();
        assert_eq!(profile.field.width_mm, 300.0);
        assert_eq!(profile.field.height_mm, 210.0);
    }

    #[test]
    fn a_partial_report_leaves_the_rest_of_the_profile_alone() {
        let base = Profile::builtin(DEFAULT_PROFILE).unwrap();
        let report = reported(&[(130, 500.0)]);
        let profile = resolve(None, Some(&report), &Config::default()).unwrap();

        assert_eq!(profile.field.width_mm, 500.0);
        assert_eq!(profile.field.height_mm, base.field.height_mm);
        assert_eq!(profile.max_feed, base.max_feed);
    }

    #[test]
    fn the_config_file_has_the_last_word() {
        let config: Config = toml::from_str(
            r#"
            [profiles.idraw-a0]
            pen_down_z = 5.4
            draw_feed = 1500
            width_mm = 800.0
            "#,
        )
        .unwrap();
        // Even against a firmware that says otherwise.
        let report = reported(&[(130, 841.0), (131, 1189.0)]);
        let profile = resolve(None, Some(&report), &config).unwrap();

        assert_eq!(profile.pen.down_z, 5.4);
        assert_eq!(profile.plan.draw_feed, 1500);
        assert_eq!(profile.field.width_mm, 800.0);
        // Untouched keys keep the reported value.
        assert_eq!(profile.field.height_mm, 1189.0);
    }

    /// A feed above what the machine can do is held at the limit rather than
    /// sent and silently ignored — or worse, obeyed.
    #[test]
    fn feeds_are_clamped_to_the_machine_limit() {
        let config: Config = toml::from_str(
            r#"
            [profiles.idraw-a0]
            draw_feed = 99000
            travel_feed = 99000
            jog_feed = 99000
            max_feed = 8000
            "#,
        )
        .unwrap();
        let profile = resolve(None, None, &config).unwrap();

        assert_eq!(profile.plan.draw_feed, 8000);
        assert_eq!(profile.plan.travel_feed, 8000);
        assert_eq!(profile.jog_feed, 8000);
        assert!(profile.pen.xy_feed <= 8000);
    }

    /// The Z axis has its own limit, so an XY cap must not slow the pen lift.
    /// A startup that changes nothing must write nothing: `$` writes land in
    /// EEPROM, and reconfiguring someone's plotter as a side effect of opening
    /// the program would be a program with opinions about hardware.
    #[test]
    fn firmware_settings_are_written_only_when_asked_for_and_wrong() {
        let board = reported(&[(11, 0.010), (120, 3000.0), (121, 3000.0)]);
        let profile = Profile::builtin(DEFAULT_PROFILE).unwrap();

        // Nothing in the config: nothing to write, whatever the board says.
        assert!(profile
            .firmware_writes(&ProfileOverride::default(), Some(&board))
            .is_empty());

        // Asked for, and the board disagrees: both acceleration axes and the
        // junction deviation go out.
        let mut over = ProfileOverride {
            accel_mm_s2: Some(500.0),
            junction_deviation_mm: Some(0.002),
            ..Default::default()
        };
        let mut tuned = Profile::builtin(DEFAULT_PROFILE).unwrap();
        tuned.apply_override(&over);
        assert_eq!(
            tuned.firmware_writes(&over, Some(&board)),
            vec![(120, 500.0), (121, 500.0), (11, 0.002)]
        );

        // Asked for, and the board already agrees: silence.
        let agreed = reported(&[(11, 0.002), (120, 500.0), (121, 500.0)]);
        assert!(tuned.firmware_writes(&over, Some(&agreed)).is_empty());

        // No `$$` at all — write it rather than assume the board is right.
        over.junction_deviation_mm = None;
        assert_eq!(
            tuned.firmware_writes(&over, None),
            vec![(120, 500.0), (121, 500.0)]
        );
    }

    /// The settles are the pen numbers a user is expected to tune by eye, so
    /// they have to survive the TOML — and a negative one must not reach the
    /// board as a `G4 P-…`.
    #[test]
    fn the_pen_settles_come_from_the_config_and_never_go_negative() {
        let mut profile = Profile::builtin(DEFAULT_PROFILE).unwrap();
        profile.apply_override(&ProfileOverride {
            pen_settle_up_secs: Some(0.12),
            pen_settle_down_secs: Some(0.03),
            ..Default::default()
        });
        assert_eq!(profile.pen.settle_up_secs, 0.12);
        assert_eq!(profile.pen.settle_down_secs, 0.03);

        profile.apply_override(&ProfileOverride {
            pen_settle_up_secs: Some(-1.0),
            pen_settle_down_secs: Some(-1.0),
            ..Default::default()
        });
        assert_eq!(profile.pen.settle_up_secs, 0.0);
        assert_eq!(profile.pen.settle_down_secs, 0.0);
    }

    /// The two are not interchangeable: standing still on the paper is what
    /// puts a dot at the start of a stroke, so the landing ships at zero.
    #[test]
    fn the_pen_does_not_stand_still_on_the_paper_by_default() {
        let profile = Profile::builtin(DEFAULT_PROFILE).unwrap();
        assert_eq!(profile.pen.settle_down_secs, 0.0);
        assert!(profile.pen.settle_up_secs > 0.0);
    }

    #[test]
    fn clamping_leaves_the_pens_z_feed_alone() {
        let mut profile = Profile::builtin(DEFAULT_PROFILE).unwrap();
        let z_feed = profile.pen.z_feed;
        profile.max_feed = 1000;
        profile.clamp_feeds();
        assert_eq!(profile.pen.z_feed, z_feed);
    }

    #[test]
    fn the_summary_names_the_profile_and_its_field() {
        let profile = Profile::builtin("idraw-a3").unwrap();
        assert_eq!(profile.summary(), "idraw-a3 430×297");
    }

    /// A zero or missing rate must not become a zero speed limit that clamps
    /// every feed to a standstill.
    #[test]
    fn a_nonsense_rate_report_is_ignored() {
        let base = Profile::builtin(DEFAULT_PROFILE).unwrap();
        let profile = resolve(None, Some(&reported(&[(110, 0.0)])), &Config::default()).unwrap();
        assert_eq!(profile.max_feed, base.max_feed);
        assert!(profile.plan.draw_feed > 0);
    }
}
