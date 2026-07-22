//! Application state and the event/render loop. DESIGN.org §4 / step 0.5.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::keys::{action_for, Action, Mode};
use crate::logging::LogRing;
use crate::plotter::driver::{Driver, DriverError};
use crate::ui;

/// Idle poll timeout: bounds how often we wake to pick up new log lines while
/// keeping idle CPU negligible (we only redraw when something actually changed).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Top-level TUI application state.
pub struct App {
    driver: Driver,
    /// Raw G-code console (step 1.5): `Some` while it is open, holding the
    /// line being typed. Its presence is what switches the key map to text.
    console: Option<String>,
    /// Whether the key overview is covering the screen.
    help: bool,
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
            console: None,
            help: false,
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
        let Some(action) = action_for(self.mode(), &key) else {
            return Ok(false);
        };
        // While the overview is up, any key dismisses it and does nothing else
        // — except quitting, which people expect to work from anywhere.
        if self.help && action != Action::Quit {
            self.help = false;
            return Ok(true);
        }
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
            Action::EmergencyStop => {
                self.run_command(terminal, "emergency stop", Driver::emergency_stop)
            }
            Action::OpenConsole => {
                tracing::info!("raw G-code console open");
                self.console = Some(String::new());
                Ok(true)
            }
            Action::CloseConsole => {
                tracing::info!("raw G-code console closed");
                self.console = None;
                Ok(true)
            }
            Action::Input(c) => {
                if let Some(line) = &mut self.console {
                    line.push(c);
                }
                Ok(true)
            }
            Action::Backspace => {
                if let Some(line) = &mut self.console {
                    line.pop();
                }
                Ok(true)
            }
            Action::Submit => self.submit_console(terminal),
            Action::ToggleHelp => {
                self.help = !self.help;
                Ok(true)
            }
        }
    }

    /// Which key map applies right now — the console makes input textual.
    fn mode(&self) -> Mode {
        match self.console {
            Some(_) => Mode::Console,
            None => Mode::Navigation,
        }
    }

    /// Send the typed line and keep the console open for the next one.
    fn submit_console<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<bool> {
        let Some(line) = self.console.as_mut().map(std::mem::take) else {
            return Ok(false);
        };
        let line = line.trim().to_owned();
        if line.is_empty() {
            return Ok(true);
        }

        self.busy = Some("sending");
        self.draw(terminal)?;
        match self.driver.send_raw(&line) {
            // The replies are already in the log panel via the TRACE wire log;
            // this line names the command they belong to.
            Ok(replies) => tracing::info!(%line, reply = %replies.join(" | "), "console"),
            Err(err) => tracing::error!(%err, %line, "console command failed"),
        }
        self.busy = None;
        Ok(true)
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

    /// The line being typed, when the console is open.
    pub fn console(&self) -> Option<&str> {
        self.console.as_deref()
    }

    pub fn help_visible(&self) -> bool {
        self.help
    }

    pub fn busy(&self) -> Option<&'static str> {
        self.busy
    }
}
