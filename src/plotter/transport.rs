//! Transport abstraction over the wire to the plotter. DESIGN.org §4.
//!
//! Implementations (mock now; serial/tcp later) own the byte channel and log all
//! wire traffic at TRACE — `-> ...` for sent lines, `<- ...` for received — so a
//! pasted log replays the full session (DESIGN.org §5).

use std::io;
use std::time::Duration;

/// A line/byte channel to a DrawCore plotter.
pub trait Transport {
    /// Send one command line; the implementation appends the `\r` terminator.
    fn send_line(&mut self, line: &str) -> io::Result<()>;

    /// Read one response line with the terminator stripped (e.g. `ok`,
    /// `error:<n>`, or a version string), waiting at most `window`.
    ///
    /// `Ok(None)` means the window elapsed with nothing to read — a normal
    /// outcome, not an error: the board stays silent for realtime bytes, and
    /// the boot banner (DESIGN.org §15.2) may or may not come.
    fn read_line_for(&mut self, window: Duration) -> io::Result<Option<String>>;

    /// Write a single realtime byte (e.g. `?`, `!`, `0x18`) with no terminator.
    fn write_realtime(&mut self, byte: u8) -> io::Result<()>;
}
