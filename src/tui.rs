//! Terminal lifecycle: an RAII guard for raw mode + the alternate screen, plus
//! best-effort restore wired into the panic hook and signal handlers so the
//! terminal is never left in a broken state. See DESIGN.org §4 / step 0.4.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::{Hide, Show};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;

/// RAII guard owning the raw-mode + alternate-screen state.
///
/// [`TerminalGuard::enter`] is the only constructor; [`Drop`] restores the
/// terminal, so an early return or a `?` unwinding both leave a clean screen.
pub struct TerminalGuard {
    // Private field: forces construction through `enter`.
    _private: (),
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen, hiding the cursor.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(EnterAlternateScreen)?;
        out.execute(Hide)?;
        tracing::debug!("terminal: entered raw mode + alternate screen");
        Ok(Self { _private: () })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
        tracing::debug!("terminal: restored");
    }
}

/// Best-effort terminal restore: show cursor, leave the alternate screen and
/// disable raw mode. Ignores errors and is safe to call repeatedly and from a
/// panic hook or signal handler.
pub fn restore() {
    let mut out = io::stdout();
    let _ = out.execute(Show);
    let _ = out.execute(LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Extend the current panic hook so it restores the terminal first, then chains
/// to the previous hook (which logs the panic and prints the backtrace to a now
/// usable screen). Call after [`TerminalGuard::enter`].
pub fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Set by the signal handler; polled by the app loop for a graceful exit.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether a termination signal has asked the app to shut down (step 3.5).
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

/// Ask for a graceful shutdown (used by tests; the signal handler does the same).
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

/// On SIGINT/SIGTERM/SIGHUP (e.g. `kill -INT`), ask the app loop to stop rather
/// than exiting outright: it then lifts the pen, releases the motors, writes a
/// final progress checkpoint and restores the terminal — the graceful path of
/// step 3.5. Just flipping an atomic here keeps the handler async-signal-safe.
///
/// Keyboard Ctrl-C is delivered as a key event in raw mode (handled by
/// [`crate::keys::action_for`]), so this only fires for external signals.
pub fn install_signal_handler() {
    let result = ctrlc::set_handler(|| SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed));
    if let Err(err) = result {
        tracing::warn!(%err, "could not install signal handler");
    }
}
