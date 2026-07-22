//! Connection handshake with a DrawCore board. DESIGN.org §2.1, §15.2.
//!
//! The order matters and was paid for in the step 0.7 spike:
//!
//! 1. **Listen first.** Opening the port reboots the board (the kernel raises
//!    DTR before we can drop it) and the `Grbl …` banner arrives ~700 ms later.
//!    A command sent into that window is swallowed without a trace — that is
//!    exactly how the first spike run lost its `$H`.
//! 2. `$B` — the button query, answered with two lines (state + `ok`). The
//!    reference driver opens with it, so we keep it as a liveness probe.
//! 3. `v` — identity. A DrawCore answers `DrawCore V<x.yy>…`.

use std::io;
use std::time::{Duration, Instant};

use super::transport::Transport;

/// How long we listen for the boot banner before sending anything (§15.2).
const BANNER_WINDOW: Duration = Duration::from_millis(1500);
/// How long a reply to one command may take.
const REPLY_WINDOW: Duration = Duration::from_millis(800);
/// Prefix of the boot banner of a Grbl-based firmware.
const BANNER_PREFIX: &str = "Grbl";
/// Prefix of every identity reply of the DrawCore firmware.
const IDENTITY_PREFIX: &str = "DrawCore V";

/// Why a board could not be greeted.
#[derive(Debug)]
pub enum HandshakeError {
    /// The port itself failed (unplugged mid-handshake, permissions, …).
    Io(io::Error),
    /// The port is open but nothing answered `v`.
    Silent,
    /// Something answered, but it is not a DrawCore.
    Foreign(String),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "serial I/O failed during handshake: {err}"),
            Self::Silent => write!(
                f,
                "the plotter did not answer `v` — is it switched on, and is this the right port?"
            ),
            Self::Foreign(reply) => write!(
                f,
                "the device answered {reply:?}, which is not a DrawCore firmware"
            ),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<io::Error> for HandshakeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Greet the board and return its firmware version string.
pub fn connect(transport: &mut dyn Transport) -> Result<String, HandshakeError> {
    wait_for_banner(transport)?;
    probe_button(transport)?;
    identify(transport)
}

/// Wait out a possible reboot, so the first command is not sent into the boot.
fn wait_for_banner(transport: &mut dyn Transport) -> Result<(), HandshakeError> {
    let deadline = Instant::now() + BANNER_WINDOW;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match transport.read_line_for(left)? {
            Some(line) if line.starts_with(BANNER_PREFIX) => {
                tracing::info!(banner = %line, "board rebooted on port open");
                return Ok(());
            }
            Some(line) => tracing::debug!(%line, "pre-handshake chatter"),
            None => {
                tracing::debug!("no banner; the board was already up");
                return Ok(());
            }
        }
    }
}

/// Send `$B` and read its two lines (button state + `ok`).
fn probe_button(transport: &mut dyn Transport) -> Result<(), HandshakeError> {
    transport.send_line("$B")?;
    for _ in 0..2 {
        match transport.read_line_for(REPLY_WINDOW)? {
            Some(line) if line == "ok" => return Ok(()),
            Some(line) => tracing::debug!(button = %line, "button state"),
            None => break,
        }
    }
    // Not fatal: `v` decides whether we are talking to a plotter at all.
    Ok(())
}

/// Send `v` and pick the identity line out of the replies.
fn identify(transport: &mut dyn Transport) -> Result<String, HandshakeError> {
    transport.send_line("v")?;
    let deadline = Instant::now() + REPLY_WINDOW;
    let mut other = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match transport.read_line_for(left)? {
            Some(line) if line.starts_with(IDENTITY_PREFIX) => {
                tracing::info!(version = %line, "connected");
                return Ok(line);
            }
            // A late banner still counts as "a Grbl is there", but it is not
            // the identity line — keep reading for the real one.
            Some(line) if line.starts_with(BANNER_PREFIX) => {
                tracing::debug!(%line, "late banner during identify")
            }
            Some(line) => other = other.or(Some(line)),
            None => break,
        }
    }
    match other {
        Some(reply) => Err(HandshakeError::Foreign(reply)),
        None => Err(HandshakeError::Silent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plotter::mock::{MockTransport, MOCK_VERSION};

    #[test]
    fn handshake_returns_the_firmware_version() {
        let mut t = MockTransport::new();
        assert_eq!(connect(&mut t).unwrap(), MOCK_VERSION);
    }

    #[test]
    fn handshake_sends_button_query_before_identity() {
        let mut t = MockTransport::new();
        connect(&mut t).unwrap();
        assert_eq!(t.sent(), &["$B".to_owned(), "v".to_owned()]);
    }

    #[test]
    fn boot_banner_is_consumed_not_mistaken_for_identity() {
        let mut t = MockTransport::new();
        t.push_unsolicited("Grbl 1.1h DrawCore V2.09 ['$' for help]");
        assert_eq!(connect(&mut t).unwrap(), MOCK_VERSION);
    }

    #[test]
    fn foreign_firmware_is_rejected() {
        let mut t = MockTransport::with_version("Marlin 2.1.2");
        let err = connect(&mut t).unwrap_err();
        assert!(
            matches!(&err, HandshakeError::Foreign(reply) if reply == "Marlin 2.1.2"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn silent_board_is_reported_as_silent() {
        let mut t = MockTransport::unresponsive();
        assert!(matches!(
            connect(&mut t).unwrap_err(),
            HandshakeError::Silent
        ));
    }
}
