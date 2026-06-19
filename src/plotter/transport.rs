//! Transport abstraction over the wire to the plotter. DESIGN.org §4.
//!
//! Implementations (mock now; serial/tcp later) own the byte channel and log all
//! wire traffic at TRACE — `-> ...` for sent lines, `<- ...` for received — so a
//! pasted log replays the full session (DESIGN.org §5).

use std::io;

/// A line/byte channel to a DrawCore plotter.
pub trait Transport {
    /// Send one command line; the implementation appends the `\r` terminator.
    fn send_line(&mut self, line: &str) -> io::Result<()>;

    /// Read one response line with the terminator stripped (e.g. `ok`,
    /// `error:<n>`, or a version string).
    fn read_line(&mut self) -> io::Result<String>;

    /// Write a single realtime byte (e.g. `?`, `!`, `0x18`) with no terminator.
    fn write_realtime(&mut self, byte: u8) -> io::Result<()>;
}
