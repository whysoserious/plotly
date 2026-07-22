//! Plotter I/O: the [`transport`] abstraction and its implementations. The
//! driver and worker arrive in later steps. DESIGN.org §4/§12.

pub mod mock;
pub mod serial;
pub mod transport;
