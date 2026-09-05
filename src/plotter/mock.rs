//! In-process [`Transport`] that fakes a DrawCore plotter, for `--simulate`
//! and tests. DESIGN.org §4 / step 0.6.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::transport::Transport;

/// What [`crate::plotter::connect`] calls the port when there is no plotter.
/// The UI keys off it to say so out loud.
pub const MOCK_PORT: &str = "mock";

/// Firmware version the mock reports on `v`/`V`.
pub const MOCK_VERSION: &str = "DrawCore V2.10";

/// What the mock answers to a realtime `?`. Nothing moves in here, so the
/// machine is never anything but idle.
pub const MOCK_STATUS: &str = "<Idle|MPos:0.000,0.000,0.000|FS:0,0>";

/// The settings the mock answers `$$` with: the ones actually read off our
/// machine in spike 0.7 (§15.1), so `--simulate` resolves the same profile the
/// hardware does.
pub const MOCK_SETTINGS: &[(u16, f64)] = &[
    (13, 0.0),       // report in mm
    (20, 0.0),       // soft limits off
    (21, 0.0),       // hard limits off
    (23, 5.0),       // homing direction mask
    (27, 1.0),       // homing pull-off, mm
    (100, 100.0),    // steps/mm X
    (101, 100.0),    // steps/mm Y
    (102, 85.8),     // steps/mm Z
    (110, 15_000.0), // max rate X, mm/min
    (111, 12_000.0), // max rate Y, mm/min
    (112, 15_000.0), // max rate Z, mm/min
    (120, 3_000.0),  // acceleration X, mm/s²
    (121, 3_000.0),  // acceleration Y, mm/s²
    (130, 841.0),    // max travel X, mm
    (131, 1_189.0),  // max travel Y, mm
    (132, 10.0),     // max travel Z, mm
];

/// Fake plotter: auto-queues a canned response for each sent line and records
/// what was sent so tests can assert on the exact wire traffic.
#[derive(Debug)]
pub struct MockTransport {
    version: String,
    /// When set, the board answers nothing at all (a dead or wrong-baud link).
    mute: bool,
    /// Sleep before each read, to give a plan measurable duration so a test can
    /// inject a stop/pause mid-run. Zero by default (instant).
    read_delay: std::time::Duration,
    responses: VecDeque<String>,
    /// Shared so a test can still watch the traffic after the transport has
    /// been handed to a driver (see [`MockTransport::sent_handle`]).
    sent: Arc<Mutex<Vec<String>>>,
    realtime: Arc<Mutex<Vec<u8>>>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    pub fn new() -> Self {
        Self::with_version(MOCK_VERSION)
    }

    /// A board that identifies itself as `version` (use a foreign string to
    /// exercise the "this is not a DrawCore" path).
    pub fn with_version(version: &str) -> Self {
        Self {
            version: version.to_owned(),
            mute: false,
            read_delay: std::time::Duration::ZERO,
            responses: VecDeque::new(),
            sent: Arc::new(Mutex::new(Vec::new())),
            realtime: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A board that never answers.
    pub fn unresponsive() -> Self {
        Self {
            mute: true,
            ..Self::new()
        }
    }

    /// A board whose every read takes `delay`, so a plan runs slowly enough for
    /// a test to inject a stop or pause partway through.
    pub fn with_read_delay(delay: std::time::Duration) -> Self {
        Self {
            read_delay: delay,
            ..Self::new()
        }
    }

    /// Queue a line the board sends on its own, e.g. the boot banner that a
    /// real DrawCore emits ~700 ms after the port opens (DESIGN.org §15.2).
    pub fn push_unsolicited(&mut self, line: &str) {
        self.responses.push_back(line.to_owned());
    }

    /// The canned reply lines a real DrawCore would give for `line`.
    fn auto_response(&self, line: &str) -> Vec<String> {
        let cmd = line.trim();
        if cmd.starts_with('v') || cmd.starts_with('V') {
            vec![self.version.clone()]
        } else if cmd == "$B" {
            // Button state plus its `ok` — the two lines the reference reads.
            vec!["0".to_owned(), "ok".to_owned()]
        } else if cmd == "$$" {
            let mut lines: Vec<String> = MOCK_SETTINGS
                .iter()
                .map(|(n, v)| format!("${n}={v:.3}"))
                .collect();
            lines.push("ok".to_owned());
            lines
        } else {
            vec!["ok".to_owned()]
        }
    }

    /// Lines sent so far (for assertions / inspection).
    pub fn sent(&self) -> Vec<String> {
        self.sent.lock().expect("mock log poisoned").clone()
    }

    /// Handle to the sent-line log that outlives moving the transport.
    pub fn sent_handle(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.sent)
    }

    /// Realtime bytes written so far (for assertions / inspection).
    pub fn realtime(&self) -> Vec<u8> {
        self.realtime.lock().expect("mock log poisoned").clone()
    }

    /// Handle to the realtime-byte log that outlives moving the transport.
    pub fn realtime_handle(&self) -> Arc<Mutex<Vec<u8>>> {
        Arc::clone(&self.realtime)
    }
}

impl Transport for MockTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        tracing::trace!(target: "plotly::transport", "-> {line:?}");
        self.sent
            .lock()
            .expect("mock log poisoned")
            .push(line.to_owned());
        if !self.mute {
            self.responses.extend(self.auto_response(line));
        }
        Ok(())
    }

    /// Answers from the queue; `window` is irrelevant in-process. A configured
    /// `read_delay` slows each read so a plan takes measurable time.
    fn read_line_for(&mut self, _window: Duration) -> io::Result<Option<String>> {
        if !self.read_delay.is_zero() {
            std::thread::sleep(self.read_delay);
        }
        let response = self.responses.pop_front();
        if let Some(line) = &response {
            tracing::trace!(target: "plotly::transport", "<- {line:?}");
        }
        Ok(response)
    }

    fn write_realtime(&mut self, byte: u8) -> io::Result<()> {
        tracing::trace!(target: "plotly::transport", "-> realtime {byte:#04x}");
        self.realtime.lock().expect("mock log poisoned").push(byte);
        // A status query has to be answered or anything waiting on `?` — the
        // poll fence of §2.5 — would hang under `--simulate`. The mock has no
        // motion to be busy with, so it is always idle.
        if byte == b'?' && !self.mute {
            self.responses.push_back(MOCK_STATUS.to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_WAIT: Duration = Duration::from_millis(0);

    #[test]
    fn version_query_returns_version() {
        let mut t = MockTransport::new();
        t.send_line("v").unwrap();
        assert_eq!(
            t.read_line_for(NO_WAIT).unwrap().as_deref(),
            Some(MOCK_VERSION)
        );
        assert_eq!(t.sent(), vec!["v".to_owned()]);
    }

    #[test]
    fn command_returns_ok() {
        let mut t = MockTransport::new();
        t.send_line("G1 X10 Y10 F2000").unwrap();
        assert_eq!(t.read_line_for(NO_WAIT).unwrap().as_deref(), Some("ok"));
    }

    #[test]
    fn button_query_returns_state_and_ok() {
        let mut t = MockTransport::new();
        t.send_line("$B").unwrap();
        assert_eq!(t.read_line_for(NO_WAIT).unwrap().as_deref(), Some("0"));
        assert_eq!(t.read_line_for(NO_WAIT).unwrap().as_deref(), Some("ok"));
    }

    #[test]
    fn nothing_queued_reads_as_none() {
        let mut t = MockTransport::new();
        assert_eq!(t.read_line_for(NO_WAIT).unwrap(), None);
    }

    #[test]
    fn unresponsive_board_never_answers() {
        let mut t = MockTransport::unresponsive();
        t.send_line("v").unwrap();
        assert_eq!(t.read_line_for(NO_WAIT).unwrap(), None);
    }

    #[test]
    fn write_realtime_is_recorded() {
        let mut t = MockTransport::new();
        t.write_realtime(b'?').unwrap();
        t.write_realtime(0x18).unwrap();
        assert_eq!(t.realtime(), vec![b'?', 0x18]);
    }

    #[test]
    fn responses_are_fifo_per_send() {
        let mut t = MockTransport::new();
        t.send_line("v").unwrap();
        t.send_line("G1 X0 Y0").unwrap();
        assert_eq!(
            t.read_line_for(NO_WAIT).unwrap().as_deref(),
            Some(MOCK_VERSION)
        );
        assert_eq!(t.read_line_for(NO_WAIT).unwrap().as_deref(), Some("ok"));
    }

    #[test]
    fn trace_contains_sent_and_received_lines() {
        use crate::logging::{build_subscriber, LogRing};
        use tracing::level_filters::LevelFilter;

        let ring = LogRing::new();
        let subscriber = build_subscriber(LevelFilter::TRACE, ring.clone());
        tracing::subscriber::with_default(subscriber, || {
            let mut t = MockTransport::new();
            t.send_line("v").unwrap();
            let _ = t.read_line_for(NO_WAIT).unwrap();
        });

        let logs = ring.tail(100).join("\n");
        assert!(logs.contains("-> "), "missing sent trace:\n{logs}");
        assert!(logs.contains("<- "), "missing received trace:\n{logs}");
        assert!(
            logs.contains(MOCK_VERSION),
            "missing version in trace:\n{logs}"
        );
    }
}
