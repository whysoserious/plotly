//! High-level plotter commands on top of a [`Connection`]. DESIGN.org §2.2.
//!
//! Everything here is one round trip: send a line, wait for `ok`. Remember that
//! `ok` means "queued", not "finished" (§15.1) — good enough for pen moves,
//! but the job worker (step 2.4) will need `?` to know when motion really ends.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::time::{Duration, Instant};

use super::Connection;
use crate::geometry::{Point, Transform};
use crate::profiles::DEFAULT_JOG_FEED;

/// How long a command may take to be acknowledged.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the settings dump (`$$`) may take. It is a few dozen short lines.
const SETTINGS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a homing cycle may take. Unlike every other command, `$H` answers
/// `ok` only when the cycle *finishes* — measured at 3–7 s on an A0 machine
/// (spike 0.7, §15.1), but a full-length seek from the far corner is slower.
const HOMING_TIMEOUT: Duration = Duration::from_secs(120);

/// How long [`Driver::drain`] may wait for the machine to finish what it has.
/// The buffer holds 15 blocks (§15.1) of at most one subdivided segment each,
/// so a couple of seconds is the real figure; this is slack around it.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Default [`PenSettings::settle_secs`]. A guess, and the one pen number with
/// no firmware setting behind it; the time estimate assumes the same figure.
const DEFAULT_PEN_SETTLE_SECS: f64 = 0.05;

/// Grbl realtime soft reset (Ctrl-X).
const SOFT_RESET: u8 = 0x18;

/// How long to wait out the reboot banner after a soft reset (§15.2).
const RESET_SETTLE: Duration = Duration::from_millis(1500);

/// Where the pen is. Tracked here because the firmware cannot tell us: `$QP`
/// answers `1` regardless of the Z axis (spike 0.7, §15.3), so the host owns
/// this state — as it does the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pen {
    Up,
    Down,
}

impl std::fmt::Display for Pen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Up => write!(f, "up"),
            Self::Down => write!(f, "down"),
        }
    }
}

/// Pen geometry and speeds. Defaults are the iDraw ones from DESIGN.org §10;
/// machine profiles (step 5.1) will supply them per machine.
#[derive(Debug, Clone, Copy)]
pub struct PenSettings {
    /// Z with the pen lifted. Larger Z means *lower* on this machine (§2.2).
    pub up_z: f32,
    /// Z with the pen on the paper.
    pub down_z: f32,
    /// Feed rate for the Z move itself, mm/min.
    pub z_feed: u32,
    /// Feed rate XY travel should use afterwards, mm/min.
    pub xy_feed: u32,
    /// How long to hold still after the pen's Z move, seconds.
    ///
    /// The Z move is *finished* by then — the dwell that carries this waits
    /// for the planner (see [`Driver::set_pen`]) — so this is only the pen
    /// itself: a spring-loaded holder bounces on landing and swings after a
    /// lift. Zero is legal and still leaves the wait for the Z move.
    pub settle_secs: f64,
}

impl Default for PenSettings {
    fn default() -> Self {
        Self {
            up_z: 0.5,
            down_z: 5.0,
            z_feed: 5000,
            xy_feed: 2000,
            settle_secs: DEFAULT_PEN_SETTLE_SECS,
        }
    }
}

/// A command the board would not take.
#[derive(Debug)]
pub enum DriverError {
    Io(io::Error),
    /// The board answered `error:<n>` or `ALARM:<n>`.
    Refused {
        command: String,
        reply: String,
    },
    /// No `ok` within [`REPLY_TIMEOUT`].
    Timeout {
        command: String,
    },
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "serial I/O failed: {err}"),
            Self::Refused { command, reply } => {
                write!(f, "the plotter refused {command:?}: {reply}")
            }
            Self::Timeout { command } => write!(f, "no reply to {command:?}"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<io::Error> for DriverError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// What the firmware answered to `$$`: setting number to value (§15.1).
///
/// Kept as raw numbers rather than a named struct because that is what the
/// board sends and because the interesting ones differ per firmware; the
/// profile (step 5.1) picks out the handful it understands.
#[derive(Debug, Clone, Default)]
pub struct GrblSettings(HashMap<u16, f64>);

impl GrblSettings {
    /// Value of setting `$n`, if the board reported it.
    pub fn get(&self, n: u16) -> Option<f64> {
        self.0.get(&n).copied()
    }

    /// How many settings came back.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Build from `(number, value)` pairs — the parse result, and what tests
    /// stand a board up with.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (u16, f64)>) -> Self {
        Self(pairs.into_iter().collect())
    }
}

/// Parse one `$130=841.000` line into its number and value.
fn parse_setting(line: &str) -> Option<(u16, f64)> {
    let (number, value) = line.trim().strip_prefix('$')?.split_once('=')?;
    Some((number.trim().parse().ok()?, value.trim().parse().ok()?))
}

/// Owns the connection and turns intents into G-code.
pub struct Driver {
    connection: Connection,
    settings: PenSettings,
    /// Feed for interactive jogging, mm/min (from the machine profile).
    jog_feed: u32,
    transform: Transform,
    pen: Pen,
    /// Best-known machine-logical position (mm). Meaningful after `$H`; the
    /// host tracks it by dead reckoning since the firmware volunteers nothing
    /// but `?` (§2.4). Used for the preview cursor (step 2.5).
    pos: Point,
}

impl Driver {
    /// Take over a greeted connection. The pen is assumed to be up: we cannot
    /// ask, and lifting on the first command is the safe assumption.
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            settings: PenSettings::default(),
            jog_feed: DEFAULT_JOG_FEED,
            transform: Transform::idraw(),
            pen: Pen::Up,
            pos: Point::new(0.0, 0.0),
        }
    }

    /// Take the pen geometry and jog speed from a machine profile (step 5.1).
    ///
    /// Applied after construction because the profile is only settled once the
    /// board has been asked what it is (`$$`), which needs a driver first.
    pub fn apply_profile(&mut self, profile: &crate::profiles::Profile) {
        self.settings = profile.pen;
        self.jog_feed = profile.jog_feed;
        tracing::debug!(
            profile = %profile.name,
            down_z = self.settings.down_z,
            up_z = self.settings.up_z,
            jog_feed = self.jog_feed,
            "driver configured from the profile"
        );
    }

    /// Read the firmware's settings dump (`$$`).
    ///
    /// Lines look like `$130=841.000` and the block ends with `ok`. Anything
    /// unparseable is logged and skipped rather than failing the read: this is
    /// a best-effort look at the machine, and a profile without it still works.
    pub fn read_settings(&mut self) -> Result<GrblSettings, DriverError> {
        self.connection.transport.send_line("$$")?;
        let deadline = Instant::now() + SETTINGS_TIMEOUT;
        let mut found = HashMap::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.connection.transport.read_line_for(left)? {
                Some(line) if line == "ok" => break,
                Some(line) if line.starts_with("error:") || line.starts_with("ALARM") => {
                    return Err(DriverError::Refused {
                        command: "$$".to_owned(),
                        reply: line,
                    })
                }
                Some(line) => match parse_setting(&line) {
                    Some((number, value)) => {
                        found.insert(number, value);
                    }
                    None => tracing::debug!(%line, "unrecognised line in the settings dump"),
                },
                None => {
                    return Err(DriverError::Timeout {
                        command: "$$".to_owned(),
                    })
                }
            }
        }
        tracing::debug!(settings = found.len(), "read the firmware settings ($$)");
        Ok(GrblSettings(found))
    }

    pub fn version(&self) -> &str {
        &self.connection.version
    }

    pub fn port(&self) -> &str {
        &self.connection.port
    }

    pub fn pen(&self) -> Pen {
        self.pen
    }

    /// Best-known machine-logical position (mm).
    pub fn position(&self) -> Point {
        self.pos
    }

    /// Lift the pen (no-op if already up).
    pub fn pen_up(&mut self) -> Result<(), DriverError> {
        self.set_pen(Pen::Up)
    }

    /// Lower the pen onto the paper (no-op if already down).
    pub fn pen_down(&mut self) -> Result<(), DriverError> {
        self.set_pen(Pen::Down)
    }

    /// Flip the pen. Done with an explicit Z move rather than the firmware's
    /// `$TP`: `$TP` toggles relative to a state we cannot read back, so it
    /// would drift out of sync with ours after any missed command.
    pub fn toggle_pen(&mut self) -> Result<(), DriverError> {
        match self.pen {
            Pen::Up => self.pen_down(),
            Pen::Down => self.pen_up(),
        }
    }

    /// Run a homing cycle (`$H`).
    ///
    /// This blocks until the machine reports `ok`, which for `$H` means the
    /// cycle is over, not merely queued. Afterwards `MPos` is `0,0,0` — the
    /// home corner *is* the machine origin on this firmware (§2.4) — and Z is
    /// at 0, i.e. above the pen-up height, so the pen counts as up.
    pub fn home(&mut self) -> Result<(), DriverError> {
        tracing::info!("homing");
        self.command_within("$H", HOMING_TIMEOUT)?;
        self.pen = Pen::Up;
        self.pos = Point::new(0.0, 0.0);
        tracing::info!("homed; machine origin is now the home corner");
        Ok(())
    }

    /// Release the steppers (`$SLP`).
    ///
    /// After this the carriage can be pushed by hand, so the machine position
    /// stops being trustworthy: whatever we knew about `MPos` is stale until
    /// the next `$H` (§2.4).
    pub fn disable_motors(&mut self) -> Result<(), DriverError> {
        self.command("$SLP")?;
        tracing::warn!("motors disabled; position is unknown until the next homing");
        Ok(())
    }

    /// Wait until everything already sent has actually been drawn.
    ///
    /// `ok` means "queued", not "drawn" (§15.1), so after the last line of a
    /// shape the machine can still have a planner buffer of moves to make.
    /// Grbl executes `G4` only once that buffer has emptied and answers `ok`
    /// when the dwell is over, which makes it the standard way to ask "are you
    /// really finished?".
    ///
    /// ❓ not exercised in spike 0.7 (§15.3). A firmware that answered early
    /// would make a pause checkpoint optimistic, never unsafe: the ops in
    /// question are absolute, so re-sending them lands in the same place.
    pub fn drain(&mut self) -> Result<(), DriverError> {
        tracing::debug!("draining the planner buffer");
        self.command_within("G4 P0.01", DRAIN_TIMEOUT)
    }

    /// Jog by a logical delta in millimetres (right = +X, up the page = +Y).
    ///
    /// Uses Grbl's `$J=` jog, confirmed working in the spike (§15.1): it plans
    /// like a normal move but stays outside the job queue, so it is the right
    /// primitive for interactive nudging. The delta is mapped through the
    /// [`Transform`] first, so pressing "right" moves the carriage physically
    /// right whatever the wire axes turn out to be (§2.3).
    ///
    /// No bounds check yet — that arrives with host-side clipping in step 2.4;
    /// until then a jog can run the carriage into the frame, exactly as the
    /// reference driver's manual jog does.
    pub fn jog(&mut self, dx_mm: f64, dy_mm: f64) -> Result<(), DriverError> {
        let (wx, wy) = self.transform.map_vector(dx_mm, dy_mm);
        if wx == 0.0 && wy == 0.0 {
            return Ok(());
        }
        let mut line = String::from("$J=G91");
        if wx != 0.0 {
            let _ = write!(line, " X{wx:.3}");
        }
        if wy != 0.0 {
            let _ = write!(line, " Y{wy:.3}");
        }
        let _ = write!(line, " F{}", self.jog_feed);
        tracing::debug!(dx_mm, dy_mm, %line, "jog");
        self.command(&line)?;
        // Jog deltas are logical mm in the same frame as the tracked position.
        self.pos = Point::new(self.pos.x + dx_mm, self.pos.y + dy_mm);
        Ok(())
    }

    /// Move to an absolute machine-logical point (mm) at the current feed.
    ///
    /// The point is mapped to the wire with the axis [`Transform`] (§2.2) and
    /// sent as one `G1` — feed is modal, set separately by [`Driver::set_feed`].
    pub fn move_to(&mut self, target: Point) -> Result<(), DriverError> {
        let wire = self.transform.map_point(target);
        // Adding 0.0 turns a mapped -0.0 back into 0.0, so a Y=0 move prints
        // "Y0.000" rather than "Y-0.000".
        self.command(&format!("G1 X{:.3} Y{:.3}", wire.x + 0.0, wire.y + 0.0))?;
        self.pos = target;
        Ok(())
    }

    /// Set the modal feed rate (mm/min) for subsequent moves.
    pub fn set_feed(&mut self, feed: u32) -> Result<(), DriverError> {
        self.command(&format!("G1 F{feed}"))
    }

    /// Pause in place for `seconds` (`G4 P`).
    pub fn dwell(&mut self, seconds: f64) -> Result<(), DriverError> {
        self.command(&format!("G4 P{seconds:.3}"))
    }

    /// Send an arbitrary line typed by the user and collect the replies.
    ///
    /// Unlike [`Driver::command`], a refusal is *returned as text* rather than
    /// as an error: in a console the `error:20` line is the answer the user
    /// asked for, not a failure of the program.
    pub fn send_raw(&mut self, line: &str) -> Result<Vec<String>, DriverError> {
        self.connection.transport.send_line(line)?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut replies = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.connection.transport.read_line_for(left)? {
                Some(reply) => {
                    let done = reply == "ok" || reply.starts_with("error:");
                    replies.push(reply);
                    if done {
                        return Ok(replies);
                    }
                }
                None => return Ok(replies),
            }
        }
    }

    /// Abort: lift the pen, then soft-reset the board (DESIGN.org §9).
    ///
    /// Pen up goes *first* deliberately — after `0x18` the firmware reboots and
    /// would not run it. Interrupting a running job comes with the worker in
    /// step 2.7; right now there is no job to interrupt, only motion in flight.
    pub fn emergency_stop(&mut self) -> Result<(), DriverError> {
        tracing::warn!("emergency stop");
        if self.pen == Pen::Down {
            if let Err(err) = self.move_pen_now(Pen::Up) {
                tracing::warn!(%err, "pen up before the reset failed; resetting anyway");
            }
        }
        self.connection.transport.write_realtime(SOFT_RESET)?;
        self.settle_after_reset();
        Ok(())
    }

    /// Swallow the reboot banner so the next command is not sent into the boot
    /// (the trap from §15.2).
    fn settle_after_reset(&mut self) {
        let deadline = Instant::now() + RESET_SETTLE;
        while let Ok(Some(line)) = self
            .connection
            .transport
            .read_line_for(deadline.saturating_duration_since(Instant::now()))
        {
            tracing::debug!(%line, "after soft reset");
        }
        // The board comes up with default modal state; ours must match it.
        self.pen = Pen::Up;
    }

    /// Move the pen to `target`, fenced off from the XY motion around it, then
    /// restore the XY feed rate.
    ///
    /// The fences are the point. `ok` means "queued" (§15.1), so on their own
    /// the four lines below just join the planner's queue, and Grbl's
    /// look-ahead then carries speed *through* the corner between the last
    /// drawn segment and the Z move — junction deviation `$11` = 0.010 mm at
    /// `$120` = 3000 mm/s² is about 8 mm/s, taken instantly sideways. The
    /// machine cannot turn that sharply: belts and pen holder flex, and the
    /// tip draws a small hook towards wherever it goes next while it is still
    /// on the paper. The same happens in reverse at pen-down, as a tick at the
    /// start of the stroke.
    ///
    /// So the order the operator expects — finish the line, *then* lift, *then*
    /// travel — has to be asked for. `G4` is how: Grbl runs a dwell only once
    /// the planner buffer has emptied, which makes the one before the Z move a
    /// "the line is really drawn" barrier and the one after it "the pen is
    /// really up", with [`PenSettings::settle_secs`] of stillness on top for
    /// the pen to stop swinging. The cost is a real stop and two round trips
    /// per pen move; that is what a clean line end costs.
    ///
    /// The final line matters too: `F` is modal in Grbl, so without it every
    /// following XY move would inherit the fast Z feed (§2.2).
    fn set_pen(&mut self, target: Pen) -> Result<(), DriverError> {
        if self.pen == target {
            tracing::debug!(pen = %target, "pen already there");
            return Ok(());
        }
        self.drain()?;
        self.move_pen_now(target)?;
        self.dwell(self.settings.settle_secs.max(0.0))?;
        self.command(&format!("G1 F{}", self.settings.xy_feed))?;
        Ok(())
    }

    /// The pen's Z move on its own: queued behind whatever is already in the
    /// planner, with nothing waited for.
    ///
    /// Only the panic path wants this. Everywhere else goes through
    /// [`Driver::set_pen`], which fences the move so it does not blend into the
    /// motion around it — but an emergency stop cannot afford to wait out a
    /// buffer that may hold seconds of travel.
    fn move_pen_now(&mut self, target: Pen) -> Result<(), DriverError> {
        let z = match target {
            Pen::Up => self.settings.up_z,
            Pen::Down => self.settings.down_z,
        };
        self.command(&format!("G1 G90 Z{z:.3} F{}", self.settings.z_feed))?;
        self.pen = target;
        // Debug, not info: a drawing raises and lowers the pen once per shape,
        // which on a hatched plot is thousands of lines that would bury
        // everything else in the log (§5).
        tracing::debug!(pen = %target, z, "pen moved");
        Ok(())
    }

    /// Send one line and wait for its `ok`.
    fn command(&mut self, line: &str) -> Result<(), DriverError> {
        self.command_within(line, REPLY_TIMEOUT)
    }

    /// Send one line and wait up to `timeout` for its `ok`.
    fn command_within(&mut self, line: &str, timeout: Duration) -> Result<(), DriverError> {
        self.connection.transport.send_line(line)?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.connection.transport.read_line_for(left)? {
                Some(reply) if reply == "ok" => return Ok(()),
                Some(reply) if reply.starts_with("error:") || reply.starts_with("ALARM") => {
                    return Err(DriverError::Refused {
                        command: line.to_owned(),
                        reply,
                    })
                }
                // Status reports and banners can arrive between command and ok.
                Some(reply) => tracing::debug!(%reply, "unsolicited line while waiting for ok"),
                None => {
                    return Err(DriverError::Timeout {
                        command: line.to_owned(),
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plotter::mock::MockTransport;

    fn driver_on(transport: MockTransport) -> Driver {
        Driver::new(Connection {
            transport: Box::new(transport),
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
        })
    }

    #[test]
    fn pen_starts_up_and_tracks_moves() {
        let mut d = driver_on(MockTransport::new());
        assert_eq!(d.pen(), Pen::Up);
        d.pen_down().unwrap();
        assert_eq!(d.pen(), Pen::Down);
        d.toggle_pen().unwrap();
        assert_eq!(d.pen(), Pen::Up);
    }

    #[test]
    fn jog_right_maps_to_wire_plus_x() {
        let transport = MockTransport::new();
        let sent = transport.sent_handle();
        let mut d = driver_on(transport);

        d.jog(10.0, 0.0).unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["$J=G91 X10.000 F3000".to_owned()]
        );
    }

    #[test]
    fn jog_up_the_page_flips_to_wire_plus_y() {
        // Logical +Y is "up the page"; on this machine that is wire -Y... but a
        // positive logical Y is "down", so up-arrow (logical -Y) must be wire +Y.
        let transport = MockTransport::new();
        let sent = transport.sent_handle();
        let mut d = driver_on(transport);

        d.jog(0.0, -5.0).unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["$J=G91 Y5.000 F3000".to_owned()]
        );
    }

    #[test]
    fn a_zero_jog_sends_nothing() {
        let transport = MockTransport::new();
        let sent = transport.sent_handle();
        let mut d = driver_on(transport);

        d.jog(0.0, 0.0).unwrap();
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn repeating_a_pen_command_sends_nothing() {
        let transport = MockTransport::new();
        let sent = transport.sent_handle();
        let mut d = driver_on(transport);

        d.pen_up().unwrap();
        assert!(sent.lock().unwrap().is_empty(), "up-to-up must be a no-op");

        d.pen_down().unwrap();
        let after_first = sent.lock().unwrap().len();
        d.pen_down().unwrap();
        assert_eq!(sent.lock().unwrap().len(), after_first);
    }

    #[test]
    fn refusal_is_reported_and_the_state_does_not_move() {
        let mut transport = MockTransport::unresponsive();
        transport.push_unsolicited("error:20");
        let mut d = driver_on(transport);

        let err = d.pen_down().unwrap_err();
        assert!(
            matches!(&err, DriverError::Refused { reply, .. } if reply == "error:20"),
            "unexpected error: {err:?}"
        );
        assert_eq!(d.pen(), Pen::Up, "a refused move must not change state");
    }

    #[test]
    fn settings_are_read_from_the_dump() {
        let mut d = driver_on(MockTransport::new());
        let settings = d.read_settings().expect("$$ answered");

        // The mock answers with our machine's real dump (§15.1).
        assert_eq!(settings.get(130), Some(841.0), "max travel X");
        assert_eq!(settings.get(131), Some(1189.0), "max travel Y");
        assert_eq!(settings.get(110), Some(15_000.0), "max rate X");
        assert_eq!(settings.get(999), None, "a setting it never sent");
        assert!(!settings.is_empty());
    }

    #[test]
    fn a_settings_line_parses_into_its_number_and_value() {
        assert_eq!(parse_setting("$130=841.000"), Some((130, 841.0)));
        assert_eq!(parse_setting(" $27=1.000 "), Some((27, 1.0)));
        // Not settings: the `ok`, a status report, junk.
        assert_eq!(parse_setting("ok"), None);
        assert_eq!(parse_setting("<Idle|MPos:0.000,0.000,0.000>"), None);
        assert_eq!(parse_setting("$hello=world"), None);
    }

    /// A board that never answers must not hang the startup, and losing `$$`
    /// is not fatal — the profile has defaults.
    #[test]
    fn an_unanswered_settings_read_times_out() {
        let mut d = driver_on(MockTransport::unresponsive());
        assert!(matches!(
            d.read_settings().unwrap_err(),
            DriverError::Timeout { .. }
        ));
    }

    /// The profile owns the pen heights and the jog speed, so a config file can
    /// set them without a rebuild (§10 / §15.3).
    #[test]
    fn a_profile_sets_the_pen_heights_and_jog_feed() {
        let transport = MockTransport::new();
        let sent = transport.sent_handle();
        let mut d = driver_on(transport);

        let mut profile = crate::profiles::Profile::builtin("idraw-a3").unwrap();
        profile.pen.down_z = 6.5;
        profile.jog_feed = 1234;
        d.apply_profile(&profile);

        d.pen_down().unwrap();
        d.jog(10.0, 0.0).unwrap();

        let sent = sent.lock().unwrap();
        assert!(
            sent.iter().any(|l| l.contains("Z6.500")),
            "the profile's pen-down Z was not used: {sent:?}"
        );
        assert!(
            sent.iter().any(|l| l.ends_with("F1234")),
            "the profile's jog feed was not used: {sent:?}"
        );
    }

    /// A panic stop must not wait out a buffer that may hold seconds of travel;
    /// it lifts and resets. (Without the pen-up it would also be *queued*, but
    /// the reset that follows is what makes waiting pointless.)
    #[test]
    fn the_panic_stop_lifts_without_waiting_for_the_buffer() {
        let transport = MockTransport::new();
        let sent = transport.sent_handle();
        let mut d = driver_on(transport);
        d.pen_down().unwrap();
        sent.lock().unwrap().clear();

        d.emergency_stop().unwrap();

        let sent = sent.lock().unwrap();
        assert_eq!(
            *sent,
            vec!["G1 G90 Z0.500 F5000".to_owned()],
            "the panic path may only lift, then reset"
        );
        assert_eq!(d.pen(), Pen::Up);
    }

    #[test]
    fn silence_times_out_rather_than_hanging() {
        let mut d = driver_on(MockTransport::unresponsive());
        assert!(matches!(
            d.pen_down().unwrap_err(),
            DriverError::Timeout { .. }
        ));
    }
}
