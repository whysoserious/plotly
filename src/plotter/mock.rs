//! In-process [`Transport`] that fakes a DrawCore plotter, for `--simulate`
//! and tests. DESIGN.org §4 / step 0.6.

use std::collections::VecDeque;
use std::io;

use super::transport::Transport;

/// Firmware version the mock reports on `v`/`V`.
const MOCK_VERSION: &str = "DrawCore V2.10";

/// Fake plotter: auto-queues a canned response for each sent line and records
/// what was sent so tests can assert on the exact wire traffic.
#[derive(Debug, Default)]
pub struct MockTransport {
    responses: VecDeque<String>,
    sent: Vec<String>,
    realtime: Vec<u8>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// The canned response a real DrawCore would give for `line`.
    fn auto_response(line: &str) -> String {
        let cmd = line.trim();
        if cmd.starts_with('v') || cmd.starts_with('V') {
            MOCK_VERSION.to_owned()
        } else {
            "ok".to_owned()
        }
    }

    /// Lines sent so far (for assertions / inspection).
    pub fn sent(&self) -> &[String] {
        &self.sent
    }

    /// Realtime bytes written so far (for assertions / inspection).
    pub fn realtime(&self) -> &[u8] {
        &self.realtime
    }
}

impl Transport for MockTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        tracing::trace!(target: "plotly::transport", "-> {line:?}");
        self.sent.push(line.to_owned());
        self.responses.push_back(Self::auto_response(line));
        Ok(())
    }

    fn read_line(&mut self) -> io::Result<String> {
        let response = self
            .responses
            .pop_front()
            .unwrap_or_else(|| "ok".to_owned());
        tracing::trace!(target: "plotly::transport", "<- {response:?}");
        Ok(response)
    }

    fn write_realtime(&mut self, byte: u8) -> io::Result<()> {
        tracing::trace!(target: "plotly::transport", "-> realtime {byte:#04x}");
        self.realtime.push(byte);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_query_returns_version() {
        let mut t = MockTransport::new();
        t.send_line("v").unwrap();
        assert_eq!(t.read_line().unwrap(), MOCK_VERSION);
        assert_eq!(t.sent(), &["v".to_owned()]);
    }

    #[test]
    fn command_returns_ok() {
        let mut t = MockTransport::new();
        t.send_line("G1 X10 Y10 F2000").unwrap();
        assert_eq!(t.read_line().unwrap(), "ok");
    }

    #[test]
    fn write_realtime_is_recorded() {
        let mut t = MockTransport::new();
        t.write_realtime(b'?').unwrap();
        t.write_realtime(0x18).unwrap();
        assert_eq!(t.realtime(), &[b'?', 0x18]);
    }

    #[test]
    fn responses_are_fifo_per_send() {
        let mut t = MockTransport::new();
        t.send_line("v").unwrap();
        t.send_line("G1 X0 Y0").unwrap();
        assert_eq!(t.read_line().unwrap(), MOCK_VERSION);
        assert_eq!(t.read_line().unwrap(), "ok");
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
            let _ = t.read_line().unwrap();
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
