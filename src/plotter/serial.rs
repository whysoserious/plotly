//! Blocking serial [`Transport`] to a real DrawCore board. DESIGN.org §2.1.
//!
//! Wire settings are the ones the reference driver uses: 115200 8N1, no flow
//! control, RTS and DTR held low (raising either resets the board), `\r` as the
//! command terminator. All traffic is logged at TRACE like the mock, so a log
//! file replays the session.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use serialport::{
    ClearBuffer, DataBits, FlowControl, Parity, SerialPort, SerialPortType, StopBits,
};

use super::transport::Transport;

/// USB vendor id of the CH340 bridge on iDraw 2.0 boards (DESIGN.org §2.1).
pub const IDRAW_VID: u16 = 0x1a86;
/// USB product ids seen on iDraw 2.0 boards.
pub const IDRAW_PIDS: [u16; 2] = [0x7523, 0x8040];
/// Default line speed of the DrawCore firmware.
pub const DEFAULT_BAUD: u32 = 115_200;
/// Command terminator expected by DrawCore.
const TERMINATOR: u8 = b'\r';

/// Serial line to a plotter, with a buffer holding bytes read past a line end.
pub struct SerialTransport {
    port: Box<dyn SerialPort>,
    pending: Vec<u8>,
}

impl SerialTransport {
    /// Open `path` with the DrawCore wire settings; `timeout` bounds each read.
    pub fn open(path: &str, baud: u32, timeout: Duration) -> io::Result<Self> {
        let mut port = serialport::new(path, baud)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .flow_control(FlowControl::None)
            .timeout(timeout)
            .open()
            .map_err(io::Error::from)?;

        // Keep both handshake lines low: toggling them resets the board.
        port.write_request_to_send(false).map_err(io::Error::from)?;
        port.write_data_terminal_ready(false)
            .map_err(io::Error::from)?;

        tracing::info!(%path, baud, "serial port open");
        Ok(Self {
            port,
            pending: Vec::new(),
        })
    }

    /// Drop everything the board sent so far, on the wire and in our buffer.
    pub fn clear_input(&mut self) -> io::Result<()> {
        self.pending.clear();
        self.port.clear(ClearBuffer::Input).map_err(io::Error::from)
    }

    /// Collect every complete line arriving within `window`. Used by the
    /// hardware spike, where a command may answer with 0, 1 or many lines.
    pub fn read_lines_for(&mut self, window: Duration) -> Vec<String> {
        let deadline = Instant::now() + window;
        let mut lines = Vec::new();
        loop {
            while let Some(line) = take_line(&mut self.pending) {
                tracing::trace!(target: "plotly::transport", "<- {line:?}");
                lines.push(line);
            }
            if Instant::now() >= deadline {
                return lines;
            }
            if let Err(err) = self.fill_once() {
                if err.kind() != io::ErrorKind::TimedOut {
                    tracing::warn!(%err, "serial read failed");
                    return lines;
                }
            }
        }
    }

    /// Read one chunk from the port into the pending buffer.
    fn fill_once(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 256];
        match self.port.read(&mut chunk) {
            Ok(0) => {
                // Nothing available yet; avoid a hot spin.
                std::thread::sleep(Duration::from_millis(2));
                Ok(())
            }
            Ok(n) => {
                self.pending.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

impl Transport for SerialTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        tracing::trace!(target: "plotly::transport", "-> {line:?}");
        self.port.write_all(line.as_bytes())?;
        self.port.write_all(&[TERMINATOR])?;
        self.port.flush()
    }

    fn read_line(&mut self) -> io::Result<String> {
        loop {
            if let Some(line) = take_line(&mut self.pending) {
                tracing::trace!(target: "plotly::transport", "<- {line:?}");
                return Ok(line);
            }
            self.fill_once()?;
        }
    }

    fn write_realtime(&mut self, byte: u8) -> io::Result<()> {
        tracing::trace!(target: "plotly::transport", "-> realtime {byte:#04x}");
        self.port.write_all(&[byte])?;
        self.port.flush()
    }
}

/// Pop the first complete, non-empty line from `buf`, stripping its terminator.
///
/// DrawCore ends lines with `\r`, `\n` or `\r\n` depending on the message, so
/// both bytes terminate a line and empty pieces are skipped.
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    loop {
        let end = buf.iter().position(|&b| b == b'\r' || b == b'\n')?;
        let line = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        buf.drain(..=end);
        if !line.is_empty() {
            return Some(line);
        }
    }
}

/// Port names of connected iDraw boards, matched on USB VID:PID.
pub fn find_idraw_ports() -> Vec<String> {
    let ports = match serialport::available_ports() {
        Ok(ports) => ports,
        Err(err) => {
            tracing::warn!(%err, "port enumeration failed");
            return Vec::new();
        }
    };
    ports
        .into_iter()
        .filter(|p| match &p.port_type {
            SerialPortType::UsbPort(usb) => usb.vid == IDRAW_VID && IDRAW_PIDS.contains(&usb.pid),
            _ => false,
        })
        .map(|p| p.port_name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_line_splits_on_cr_and_keeps_remainder() {
        let mut buf = b"ok\rerror:2\r".to_vec();
        assert_eq!(take_line(&mut buf).as_deref(), Some("ok"));
        assert_eq!(take_line(&mut buf).as_deref(), Some("error:2"));
        assert_eq!(take_line(&mut buf), None);
        assert!(buf.is_empty());
    }

    #[test]
    fn take_line_needs_a_terminator() {
        let mut buf = b"DrawCore V2.10".to_vec();
        assert_eq!(take_line(&mut buf), None);
        buf.push(b'\n');
        assert_eq!(take_line(&mut buf).as_deref(), Some("DrawCore V2.10"));
    }

    #[test]
    fn take_line_skips_empty_lines_and_crlf() {
        let mut buf = b"\r\n\r\nok\r\n".to_vec();
        assert_eq!(take_line(&mut buf).as_deref(), Some("ok"));
        assert_eq!(take_line(&mut buf), None);
    }
}
