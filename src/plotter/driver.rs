//! High-level plotter commands on top of a [`Connection`]. DESIGN.org §2.2.
//!
//! Everything here is one round trip: send a line, wait for `ok`. Remember that
//! `ok` means "queued", not "finished" (§15.1) — good enough for pen moves,
//! but the job worker (step 2.4) will need `?` to know when motion really ends.

use std::io;
use std::time::{Duration, Instant};

use super::Connection;

/// How long a command may take to be acknowledged.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a homing cycle may take. Unlike every other command, `$H` answers
/// `ok` only when the cycle *finishes* — measured at 3–7 s on an A0 machine
/// (spike 0.7, §15.1), but a full-length seek from the far corner is slower.
const HOMING_TIMEOUT: Duration = Duration::from_secs(120);

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
}

impl Default for PenSettings {
    fn default() -> Self {
        Self {
            up_z: 0.5,
            down_z: 5.0,
            z_feed: 5000,
            xy_feed: 2000,
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

/// Owns the connection and turns intents into G-code.
pub struct Driver {
    connection: Connection,
    settings: PenSettings,
    pen: Pen,
}

impl Driver {
    /// Take over a greeted connection. The pen is assumed to be up: we cannot
    /// ask, and lifting on the first command is the safe assumption.
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            settings: PenSettings::default(),
            pen: Pen::Up,
        }
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

    /// Move the pen to `target`, then restore the XY feed rate.
    ///
    /// The second line matters: `F` is modal in Grbl, so without it every
    /// following XY move would inherit the fast Z feed (§2.2).
    fn set_pen(&mut self, target: Pen) -> Result<(), DriverError> {
        if self.pen == target {
            tracing::debug!(pen = %target, "pen already there");
            return Ok(());
        }
        let z = match target {
            Pen::Up => self.settings.up_z,
            Pen::Down => self.settings.down_z,
        };
        self.command(&format!("G1 G90 Z{z:.3} F{}", self.settings.z_feed))?;
        self.command(&format!("G1 F{}", self.settings.xy_feed))?;
        self.pen = target;
        tracing::info!(pen = %target, z, "pen moved");
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
    fn silence_times_out_rather_than_hanging() {
        let mut d = driver_on(MockTransport::unresponsive());
        assert!(matches!(
            d.pen_down().unwrap_err(),
            DriverError::Timeout { .. }
        ));
    }
}
