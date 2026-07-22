//! Blocking serial [`Transport`] to a real DrawCore board. DESIGN.org §2.1.
//!
//! Wire settings are the ones the reference driver uses: 115200 8N1, no flow
//! control, RTS and DTR held low (raising either resets the board), `\r` as the
//! command terminator. All traffic is logged at TRACE like the mock, so a log
//! file replays the session.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use serialport::{
    ClearBuffer, DataBits, FlowControl, Parity, SerialPort, SerialPortInfo, SerialPortType,
    StopBits,
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
    match serialport::available_ports() {
        Ok(ports) => filter_idraw_ports(&ports),
        Err(err) => {
            tracing::warn!(%err, "port enumeration failed");
            Vec::new()
        }
    }
}

/// Names of the ports in `ports` that are iDraw boards.
///
/// Matching is by USB VID:PID only, never by port name: the board enumerates
/// as CDC-ACM (`/dev/ttyACM0`) on some kernels and as a plain CH340
/// (`/dev/ttyUSB0`) on others (DESIGN.org §2.1).
pub fn filter_idraw_ports(ports: &[SerialPortInfo]) -> Vec<String> {
    ports
        .iter()
        .filter(|p| match &p.port_type {
            SerialPortType::UsbPort(usb) => usb.vid == IDRAW_VID && IDRAW_PIDS.contains(&usb.pid),
            _ => false,
        })
        .map(|p| p.port_name.clone())
        .collect()
}

/// Where the plotter I/O should go for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortChoice {
    /// `--simulate`: the in-process [`super::mock::MockTransport`].
    Mock,
    /// A serial port path, either forced with `--port` or auto-detected.
    Serial(String),
}

/// Why no plotter could be selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    /// Nothing matching the iDraw VID:PID is plugged in.
    NotFound,
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(
                f,
                "no iDraw found (USB {IDRAW_VID:04X}:{:04X}/{:04X}). \
                 Plug it in and switch it on, pass --port <PATH>, or run with --simulate.",
                IDRAW_PIDS[0], IDRAW_PIDS[1]
            ),
        }
    }
}

impl std::error::Error for PortError {}

/// Pick the plotter for this run: mock, an explicit `--port`, or auto-detect.
///
/// `--simulate` wins over `--port` so a forced path cannot accidentally open
/// real hardware during a dry run.
pub fn resolve_port(simulate: bool, requested: Option<&str>) -> Result<PortChoice, PortError> {
    if simulate {
        return Ok(PortChoice::Mock);
    }
    if let Some(path) = requested {
        return Ok(PortChoice::Serial(path.to_owned()));
    }
    let found = find_idraw_ports();
    if found.len() > 1 {
        tracing::warn!(?found, "several iDraw ports found; using the first");
    }
    found
        .into_iter()
        .next()
        .map(PortChoice::Serial)
        .ok_or(PortError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serialport::UsbPortInfo;

    /// A USB serial port as `available_ports()` would report it.
    fn usb_port(name: &str, vid: u16, pid: u16) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_owned(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid,
                serial_number: None,
                manufacturer: Some("QinHeng Electronics".to_owned()),
                product: Some("USB CDC-Serial".to_owned()),
            }),
        }
    }

    #[test]
    fn filter_keeps_both_idraw_pids_whatever_the_port_is_called() {
        let ports = [
            usb_port("/dev/ttyACM0", IDRAW_VID, 0x8040),
            usb_port("/dev/ttyUSB0", IDRAW_VID, 0x7523),
        ];
        assert_eq!(
            filter_idraw_ports(&ports),
            vec!["/dev/ttyACM0".to_owned(), "/dev/ttyUSB0".to_owned()]
        );
    }

    #[test]
    fn filter_rejects_other_usb_devices_and_non_usb_ports() {
        let ports = [
            usb_port("/dev/ttyUSB1", 0x0403, 0x6001),    // FTDI
            usb_port("/dev/ttyACM1", IDRAW_VID, 0x5523), // same vendor, wrong product
            SerialPortInfo {
                port_name: "/dev/ttyS0".to_owned(),
                port_type: SerialPortType::PciPort,
            },
        ];
        assert!(filter_idraw_ports(&ports).is_empty());
    }

    #[test]
    fn simulate_wins_over_an_explicit_port() {
        assert_eq!(
            resolve_port(true, Some("/dev/ttyACM0")),
            Ok(PortChoice::Mock)
        );
    }

    #[test]
    fn explicit_port_is_used_verbatim_without_probing() {
        assert_eq!(
            resolve_port(false, Some("/dev/ttyS9")),
            Ok(PortChoice::Serial("/dev/ttyS9".to_owned()))
        );
    }

    #[test]
    fn not_found_explains_all_three_ways_out() {
        let message = PortError::NotFound.to_string();
        assert!(message.contains("1A86:7523/8040"), "{message}");
        assert!(message.contains("--port"), "{message}");
        assert!(message.contains("--simulate"), "{message}");
    }

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
