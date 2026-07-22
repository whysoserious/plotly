//! Application state and the event/render loop. DESIGN.org §4 / step 0.5.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::keys::{action_for, Action};
use crate::logging::LogRing;
use crate::plotter::driver::{Driver, DriverError};
use crate::ui;

/// Idle poll timeout: bounds how often we wake to pick up new log lines while
/// keeping idle CPU negligible (we only redraw when something actually changed).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Top-level TUI application state.
pub struct App {
    driver: Driver,
    /// What the machine is doing while a blocking command runs, for the status
    /// bar. Goes away with the job worker and its channels (step 2.4).
    busy: Option<&'static str>,
    log: LogRing,
    last_log_len: usize,
    should_quit: bool,
}

impl App {
    pub fn new(driver: Driver, log: LogRing) -> Self {
        Self {
            driver,
            busy: None,
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
                self.draw(terminal)?;
                needs_redraw = false;
            }

            if event::poll(POLL_INTERVAL)? {
                match event::read()? {
                    Event::Key(key) => needs_redraw |= self.on_key(key, terminal)?,
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

    fn draw<B: Backend>(&self, terminal: &mut Terminal<B>) -> io::Result<()> {
        terminal.draw(|frame| ui::draw(frame, self))?;
        Ok(())
    }

    /// Handle one key press; returns whether the screen has to be redrawn.
    fn on_key<B: Backend>(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<B>,
    ) -> io::Result<bool> {
        let Some(action) = action_for(&key) else {
            return Ok(false);
        };
        match action {
            Action::Quit => {
                tracing::info!("quit requested");
                self.should_quit = true;
                Ok(true)
            }
            Action::PenUp => self.run_command(terminal, "pen up", Driver::pen_up),
            Action::PenDown => self.run_command(terminal, "pen down", Driver::pen_down),
            Action::PenToggle => self.run_command(terminal, "pen", Driver::toggle_pen),
            Action::Home => self.run_command(terminal, "homing", Driver::home),
            Action::DisableMotors => {
                self.run_command(terminal, "disabling motors", Driver::disable_motors)
            }
        }
    }

    /// Show what is happening, then run a blocking driver command.
    ///
    /// The extra draw before the call is the point: `$H` takes seconds and the
    /// driver is synchronous, so without it the TUI would simply freeze with no
    /// explanation. The job worker (step 2.4) moves this off the UI thread.
    ///
    /// A failed command is logged, not propagated: a refused pen move or a
    /// timeout is bad news, not a reason to lose the session — and the log
    /// panel shows it immediately.
    fn run_command<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        label: &'static str,
        action: fn(&mut Driver) -> Result<(), DriverError>,
    ) -> io::Result<bool> {
        self.busy = Some(label);
        self.draw(terminal)?;

        if let Err(err) = action(&mut self.driver) {
            tracing::error!(%err, "{label} failed");
        }

        self.busy = None;
        Ok(true)
    }

    pub fn log(&self) -> &LogRing {
        &self.log
    }

    pub fn driver(&self) -> &Driver {
        &self.driver
    }

    pub fn busy(&self) -> Option<&'static str> {
        self.busy
    }
}
