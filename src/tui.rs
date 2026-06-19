//! Terminal lifecycle: an RAII guard for raw mode + the alternate screen, plus
//! best-effort restore wired into the panic hook and signal handlers so the
//! terminal is never left in a broken state. See DESIGN.org §4 / step 0.4.

use std::io::{self, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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

/// Restore the terminal and exit on SIGINT/SIGTERM/SIGHUP (e.g. `kill -INT`),
/// which would otherwise terminate the process without running [`Drop`].
///
/// Keyboard Ctrl-C is delivered as a key event in raw mode (handled by
/// [`is_quit_key`]), so this only fires for external signals.
pub fn install_signal_restore() {
    let result = ctrlc::set_handler(|| {
        restore();
        // 128 + SIGINT(2); conventional exit code for signal termination.
        std::process::exit(130);
    });
    if let Err(err) = result {
        tracing::warn!(%err, "could not install signal handler");
    }
}

/// True if `key` is a quit request (`q` or Ctrl-C), ignoring key-release events
/// (Windows emits a Release alongside each Press).
pub fn is_quit_key(key: &KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
