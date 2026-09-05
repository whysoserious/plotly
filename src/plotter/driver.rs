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

/// Gap between `?` polls in [`Driver::wait_idle`]. Short enough to fence a pen
/// move without adding much to it, long enough not to flood the board.
const POLL_GAP: Duration = Duration::from_millis(10);

/// How long to listen for the answer to one `?`.
const POLL_WINDOW: Duration = Duration::from_millis(80);

/// Consecutive `Idle` reports [`Driver::wait_idle`] needs before it believes
/// the machine. One is not enough: after `ok` the state stays `Idle` for a
/// moment before turning `Run` (§15.1), so a single early poll would wave a
/// still-moving machine through. Three polls is ~30 ms of doubt, which beats
/// drawing a hook.
const IDLE_STREAK: usize = 3;

/// Default [`PenSettings::settle_up_secs`]. A guess, and the one pen number
/// with no firmware setting behind it; the time estimate assumes the same.
const DEFAULT_SETTLE_UP_SECS: f64 = 0.05;

/// Default [`PenSettings::settle_down_secs`] — *zero*, deliberately.
///
/// A settle after a lift costs nothing but time. A settle after a landing is
/// time spent with the tip pressed on the paper and no motion to carry the ink
/// away, which is how a plot ends up with a dot at the start of every stroke.
/// The reference driver splits the two delays the same way and also ships the
/// lowering one at zero (`pen_delay_up` / `pen_delay_down`).
const DEFAULT_SETTLE_DOWN_SECS: f64 = 0.0;

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

/// How a pen move is fenced off from the XY motion around it (§2.5).
///
/// `ok` means "queued", not "drawn", so without a fence Grbl's look-ahead
/// carries speed through the corner between the last drawn segment and the Z
/// move, and the pen draws a hook as it lifts. The fence is whatever makes the
/// host wait for the motion to be *over* — and which of these actually does
/// that on DrawCore is a question for the machine, not for us:
/// `cargo run --example fence` asks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PenFence {
    /// No fence: the Z move joins the planner queue like any other block.
    /// Fastest, and what the machine did before 2026-09-02.
    Off,
    /// `G4` dwells around the Z move. Grbl runs a dwell only once the planner
    /// buffer has emptied — *if* the firmware implements it that way.
    #[default]
    Dwell,
    /// Poll `?` until the machine reports `Idle`. Slower per pen move than a
    /// dwell, but it asks about the state rather than trusting a side effect.
    Poll,
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
    /// How long to hold still after *lifting*, seconds.
    ///
    /// The Z move is finished by then — the dwell that carries this waits for
    /// the planner (see [`Driver::set_pen`]) — so this is only the pen itself
    /// swinging. Zero is legal and still leaves the wait for the Z move.
    pub settle_up_secs: f64,
    /// How long to hold still after *landing*, seconds.
    ///
    /// Split from the lift because the two are not the same trade at all: this
    /// one is time with the tip on the paper and nothing moving, so every
    /// millisecond of it feeds the dot at the start of a stroke. Raise it only
    /// if strokes start too faint to read.
    pub settle_down_secs: f64,
    /// What holds the XY motion off while the pen moves.
    pub fence: PenFence,
}

impl Default for PenSettings {
    fn default() -> Self {
        Self {
            up_z: 0.5,
            down_z: 5.0,
            z_feed: 5000,
            xy_feed: 2000,
            settle_up_secs: DEFAULT_SETTLE_UP_SECS,
            settle_down_secs: DEFAULT_SETTLE_DOWN_SECS,
            fence: PenFence::default(),
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

    /// Set the pen-down height and, if the pen is already down, land again at
    /// the new one so the change can be seen on the paper immediately.
    ///
    /// This is the number that decides how hard a tube nib presses, and getting
    /// it wrong floods ink at the point of contact (§2.5). It is worth a key
    /// rather than a config file and a restart: the only way to judge it is to
    /// watch a line being drawn.
    pub fn set_pen_down_z(&mut self, z: f32) -> Result<(), DriverError> {
        self.settings.down_z = z;
        tracing::info!(pen_down_z = z, "pen-down height changed");
        if self.pen == Pen::Down {
            self.move_pen_now(Pen::Down)?;
            self.command(&format!("G1 F{}", self.settings.xy_feed))?;
        }
        Ok(())
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

    /// Write one Grbl setting (`$<n>=<value>`).
    ///
    /// This persists in the board's EEPROM, so it outlives the program and
    /// every other sender — which is the point. `$120` and `$11` ship at Grbl
    /// defaults meant for a tool held rigidly in a spindle, and a pen on a
    /// sprung holder at the end of an A0 gantry is not that (§2.5). Getting
    /// them right by hand, in a console, and then remembering what was set,
    /// turned out to be the part that kept going wrong; a config file that
    /// says what the machine should be does not forget.
    ///
    /// Only ever called for values the user named explicitly, and only when
    /// the board disagrees — an EEPROM write per startup is nothing, one per
    /// pen move would not be.
    pub fn write_setting(&mut self, number: u16, value: f64) -> Result<(), DriverError> {
        tracing::info!(setting = number, value, "writing a firmware setting");
        self.command(&format!("${number}={value}"))
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
    /// the planner buffer has emptied (measured: it held 4.336 s for a 4.0 s
    /// move, §2.5), which makes the one *before* the Z move a "the line is
    /// really drawn" barrier.
    ///
    /// The barrier *after* the Z move is not about ordering — Grbl runs queued
    /// blocks in order, and sampling the axes at 6 ms found no overlap at all
    /// (§2.10). It is about the *velocity the pen lands with*. Without it, the
    /// planner joins the Z block to the XY block that follows and the head
    /// leaves that junction already moving: the tip touches down with sideways
    /// speed and drags a hook into the start of the stroke. With it, the
    /// machine reaches a standstill first and the pen lands still.
    ///
    /// This is empirical and it cost a detour to learn: dropping this barrier
    /// to save the 298 ms of stillness it caused (§2.7) brought the hooks
    /// straight back, and only at the landing, which is exactly where the
    /// junction is. So it goes out unconditionally — `G4 P0.000` when there is
    /// no settle to add. What a settle buys on top is stillness, which is a
    /// different thing and costs ink when the tip is down (§2.8).
    ///
    /// The final line matters too: `F` is modal in Grbl, so without it every
    /// following XY move would inherit the fast Z feed (§2.2).
    fn set_pen(&mut self, target: Pen) -> Result<(), DriverError> {
        if self.pen == target {
            tracing::debug!(pen = %target, "pen already there");
            return Ok(());
        }
        let settle = match target {
            Pen::Up => self.settings.settle_up_secs,
            Pen::Down => self.settings.settle_down_secs,
        }
        .max(0.0);
        match self.settings.fence {
            PenFence::Off => {
                self.move_pen_now(target)?;
            }
            PenFence::Dwell => {
                self.drain()?;
                self.move_pen_now(target)?;
                // Always, even for a zero settle: this one is not about
                // stillness, it is about *landing at a standstill*. See below.
                self.dwell(settle)?;
            }
            PenFence::Poll => {
                self.wait_idle()?;
                self.move_pen_now(target)?;
                self.wait_idle()?;
                if settle > 0.0 {
                    // The machine is stopped and we know it, so the settle is
                    // the host's to wait out — asking the board for a dwell
                    // would put the same trust back in `G4` that this variant
                    // exists to avoid.
                    std::thread::sleep(Duration::from_secs_f64(settle));
                }
            }
        }
        self.command(&format!("G1 F{}", self.settings.xy_feed))?;
        Ok(())
    }

    /// Block until the machine reports `Idle` — the barrier `G4` is only
    /// *assumed* to be.
    ///
    /// Grbl answers `?` at any time with `<State|MPos:…>`, and `Idle` means the
    /// planner is empty and the steppers are stopped. That is a question about
    /// the state rather than a side effect of a command, which is why this is
    /// the fallback when a dwell turns out not to wait.
    ///
    /// [`IDLE_STREAK`] reports are needed because a single one can be stale:
    /// the state lags `ok` by a moment. That narrows the race rather than
    /// closing it — a firmware that reported `Idle` for longer than the streak
    /// takes would still slip through, and the only airtight answer would be
    /// the planner-block count in `Bf:`, which needs `$10` changed on the
    /// machine.
    pub fn wait_idle(&mut self) -> Result<(), DriverError> {
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut seen_busy = false;
        let mut streak = 0;
        while Instant::now() < deadline {
            self.connection.transport.write_realtime(b'?')?;
            match self.read_status()? {
                Some(state) if state == "Idle" => {
                    streak += 1;
                    // Having watched the machine run, the first `Idle` is the
                    // end of that run and there is nothing to be careful
                    // about. The streak is only needed when we never saw it
                    // move, where an `Idle` may be the stale one that trails
                    // `ok` (§15.1). This is what makes the poll fence cheaper
                    // than the dwell: one poll gap against `G4`'s measured
                    // 50 ms floor, on every pen move.
                    if seen_busy || streak >= IDLE_STREAK {
                        return Ok(());
                    }
                }
                Some(_) => {
                    seen_busy = true;
                    streak = 0;
                }
                None => streak = 0,
            }
            std::thread::sleep(POLL_GAP);
        }
        tracing::warn!("machine never reported Idle; giving up on the fence");
        Err(DriverError::Timeout {
            command: "?".to_owned(),
        })
    }

    /// Read the state field of the next status report, if one arrives.
    fn read_status(&mut self) -> Result<Option<String>, DriverError> {
        let deadline = Instant::now() + POLL_WINDOW;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.connection.transport.read_line_for(left)? {
                Some(line) if line.starts_with('<') => return Ok(Some(state_of(&line).to_owned())),
                // Not a report: an `ok` we did not ask for, an alarm, a banner.
                // Logged and skipped — a fence is not the place to fail on it.
                Some(line) => tracing::debug!(%line, "unsolicited line while polling"),
                None => return Ok(None),
            }
        }
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

/// The state field of a `<Idle|MPos:…>` report, without any sub-state: `Idle`,
/// `Run`, `Hold` (from `Hold:0`), and so on.
fn state_of(report: &str) -> &str {
    let body = report.trim_start_matches('<');
    let field = body.split(['|', '>']).next().unwrap_or("");
    field.split(':').next().unwrap_or("")
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
    fn a_status_report_yields_its_state_without_the_sub_state() {
        assert_eq!(state_of("<Idle|MPos:0.000,0.000,0.000|FS:0,0>"), "Idle");
        assert_eq!(state_of("<Run|MPos:6.010,0.000,0.501|FS:300,0>"), "Run");
        // `Hold:0` is still a hold; the reason is not our business here.
        assert_eq!(state_of("<Hold:0|MPos:7.740,0.000,0.501|FS:0,0>"), "Hold");
        assert_eq!(state_of("<Idle>"), "Idle");
        assert_eq!(state_of("ok"), "ok");
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
