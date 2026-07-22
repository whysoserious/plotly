//! Plotter I/O: the [`transport`] abstraction, its implementations and the
//! connection handshake. The driver and worker arrive in later steps.
//! DESIGN.org §4/§12.

pub mod driver;
pub mod handshake;
pub mod mock;
pub mod serial;
pub mod transport;

use std::io;
use std::time::Duration;

use mock::MockTransport;
use serial::{PortChoice, SerialTransport};
use transport::Transport;

/// Per-read timeout of the serial port; the caller's window bounds the wait.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// A greeted plotter: the open channel plus who answered on it.
pub struct Connection {
    pub transport: Box<dyn Transport>,
    /// Firmware version reported by `v`, e.g. `DrawCore V2.09.20230318`.
    pub version: String,
    /// Where it is: a serial path, or `mock` under `--simulate`.
    pub port: String,
}

/// Hand-written because the boxed transport is not `Debug`; the channel itself
/// has nothing worth printing anyway (its traffic goes to the TRACE log).
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("version", &self.version)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// Why connecting failed, as one message for the user.
#[derive(Debug)]
pub enum ConnectError {
    Open(io::Error),
    Handshake(handshake::HandshakeError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(err) => write!(f, "cannot open the serial port: {err}"),
            Self::Handshake(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Open the chosen port and greet the board (DESIGN.org §2.1).
pub fn connect(choice: &PortChoice, baud: u32) -> Result<Connection, ConnectError> {
    let (mut transport, port): (Box<dyn Transport>, String) = match choice {
        PortChoice::Mock => (Box::new(MockTransport::new()), "mock".to_owned()),
        PortChoice::Serial(path) => (
            Box::new(SerialTransport::open(path, baud, READ_TIMEOUT).map_err(ConnectError::Open)?),
            path.clone(),
        ),
    };
    let version = handshake::connect(transport.as_mut()).map_err(ConnectError::Handshake)?;
    Ok(Connection {
        transport,
        version,
        port,
    })
}
