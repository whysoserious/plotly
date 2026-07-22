//! Application state and the event/render loop. DESIGN.org §4 / step 0.5.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::logging::LogRing;
use crate::plotter::driver::{Driver, DriverError};
use crate::{tui, ui};

/// Idle poll timeout: bounds how often we wake to pick up new log lines while
/// keeping idle CPU negligible (we only redraw when something actually changed).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Top-level TUI application state.
pub struct App {
    driver: Driver,
    log: LogRing,
    last_log_len: usize,
    should_quit: bool,
}

impl App {
    pub fn new(driver: Driver, log: LogRing) -> Self {
        Self {
            driver,
            last_log_len: log.len(),
            log,
            should_quit: false,
        }
    }

    /// Run the event loop until the user quits. Event-driven + dirty: renders
    /// only on a key, a resize, or new log lines (DESIGN.org §4).
    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<()> {
        let mut needs_redraw = true;
        while !self.should_quit {
            if needs_redraw {
                let app: &App = self;
                terminal.draw(|frame| ui::draw(frame, app))?;
                needs_redraw = false;
            }

            if event::poll(POLL_INTERVAL)? {
                match event::read()? {
                    Event::Key(key) if tui::is_quit_key(&key) => {
                        tracing::info!("quit requested");
                        self.should_quit = true;
                    }
                    Event::Key(key) => needs_redraw |= self.on_key(key),
                    Event::Resize(_, _) => needs_redraw = true,
                    _ => {}
                }
            }

            let len = self.log.len();
            if len != self.last_log_len {
                self.last_log_len = len;
                needs_redraw = true;
            }
        }
        Ok(())
    }

    /// Handle one key press; returns whether the screen has to be redrawn.
    ///
    /// These are navigation-mode shortcuts. Once the raw G-code console exists
    /// (step 1.5), typing must not reach them (DESIGN.org §8).
    fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match key.code {
            KeyCode::Char('[') => self.pen(Driver::pen_up),
            KeyCode::Char(']') => self.pen(Driver::pen_down),
            KeyCode::Char(' ') => self.pen(Driver::toggle_pen),
            _ => false,
        }
    }

    /// Run one pen command, logging failures instead of tearing down the TUI:
    /// a refused or timed-out pen move is bad news, not a reason to lose the
    /// session — and the log panel shows it immediately.
    fn pen(&mut self, action: fn(&mut Driver) -> Result<(), DriverError>) -> bool {
        if let Err(err) = action(&mut self.driver) {
            tracing::error!(%err, "pen command failed");
        }
        true
    }

    pub fn log(&self) -> &LogRing {
        &self.log
    }

    pub fn driver(&self) -> &Driver {
        &self.driver
    }
}
