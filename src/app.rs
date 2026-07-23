//! Application state and the event/render loop. DESIGN.org §4 / step 0.5.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::keys::{action_for, Action, Mode};
use crate::logging::LogRing;
use crate::plotter::driver::{Driver, DriverError};
use crate::ui;

/// Idle poll timeout: bounds how often we wake to pick up new log lines while
/// keeping idle CPU negligible (we only redraw when something actually changed).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Jog step sizes cycled by `+`/`-`, in millimetres (DESIGN.org §8).
const JOG_STEPS_MM: [f64; 4] = [0.1, 1.0, 5.0, 10.0];
/// Index into [`JOG_STEPS_MM`] the app starts on (1 mm).
const DEFAULT_STEP_INDEX: usize = 1;

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
    /// Current jog step, as an index into [`JOG_STEPS_MM`].
    step_index: usize,
    /// Events read ahead of the loop (during jog coalescing) and not yet
    /// handled. Drained before the terminal is polled again.
    pending_events: VecDeque<Event>,
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
            step_index: DEFAULT_STEP_INDEX,
            pending_events: VecDeque::new(),
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

            if let Some(event) = self.next_event()? {
                match event {
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

    /// Next event to process: a read-ahead one first, else poll the terminal.
    fn next_event(&mut self) -> io::Result<Option<Event>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        if event::poll(POLL_INTERVAL)? {
            return Ok(Some(event::read()?));
        }
        Ok(None)
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
            Action::Jog { dx, dy } => self.jog(terminal, dx, dy),
            Action::StepBigger => {
                self.step_index = (self.step_index + 1).min(JOG_STEPS_MM.len() - 1);
                Ok(true)
            }
            Action::StepSmaller => {
                self.step_index = self.step_index.saturating_sub(1);
                Ok(true)
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

    /// Jog by one step, coalescing a held arrow key into a single move.
    ///
    /// Key auto-repeat can deliver arrows far faster than the (synchronous)
    /// driver can round-trip each `$J=`. So before sending, drain the arrow
    /// presses already waiting and sum them: holding "right" becomes one longer
    /// jog instead of a backlog the carriage keeps chewing through after you let
    /// go. Non-jog events found while draining are put back for the next loop.
    fn jog<B: Backend>(&mut self, terminal: &mut Terminal<B>, dx: i8, dy: i8) -> io::Result<bool> {
        let (mut sx, mut sy) = (i32::from(dx), i32::from(dy));
        while event::poll(Duration::ZERO)? {
            let event = event::read()?;
            match jog_of(&event) {
                Some((jx, jy)) => {
                    sx += i32::from(jx);
                    sy += i32::from(jy);
                }
                None => {
                    self.pending_events.push_back(event);
                    break;
                }
            }
        }

        let step = JOG_STEPS_MM[self.step_index];
        let (dx_mm, dy_mm) = (f64::from(sx) * step, f64::from(sy) * step);

        self.busy = Some("jogging");
        self.draw(terminal)?;
        if let Err(err) = self.driver.jog(dx_mm, dy_mm) {
            tracing::error!(%err, "jog failed");
        }
        self.busy = None;
        Ok(true)
    }

    /// The current jog step in millimetres, for the status bar.
    pub fn jog_step_mm(&self) -> f64 {
        JOG_STEPS_MM[self.step_index]
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

/// The jog delta of an event, if it is a navigation-mode jog key press. Key
/// releases (which some terminals emit) are ignored here so they neither add to
/// the sum nor stop coalescing.
fn jog_of(event: &Event) -> Option<(i8, i8)> {
    let Event::Key(key) = event else { return None };
    if key.kind == KeyEventKind::Release {
        // Treat as "not a boundary": skip it. Represented by a zero jog.
        return Some((0, 0));
    }
    match action_for(Mode::Navigation, key) {
        Some(Action::Jog { dx, dy }) => Some((dx, dy)),
        _ => None,
    }
}
