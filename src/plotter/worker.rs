//! The plotter worker: a dedicated thread that owns the [`Driver`] and talks to
//! the board, driven by a channel of [`Command`]s and reporting [`Event`]s.
//! DESIGN.org §4 / step 2.4.
//!
//! `serialport` is blocking, so all board I/O lives off the UI thread. The app
//! never touches the transport directly — it sends commands and reacts to
//! events, which keeps the TUI responsive while `$H` or a long plan runs.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use crate::geometry::Point;
use crate::plan::{Op, Plan};

use super::driver::{Driver, DriverError, Pen};

/// A request from the app to the worker.
#[derive(Debug)]
pub enum Command {
    PenUp,
    PenDown,
    PenToggle,
    Home,
    DisableMotors,
    /// Jog by a logical delta in mm (right = +X, up the page = +Y).
    Jog {
        dx_mm: f64,
        dy_mm: f64,
    },
    /// Send a raw line typed in the console.
    Raw(String),
    /// Draw a whole plan.
    RunPlan(Plan),
    /// Abort: pen up, then soft reset (also aborts a running plan between ops).
    EmergencyStop,
    /// Stop drawing the current plan (pen up), keep the connection.
    Stop,
    /// Finish and let the thread exit.
    Shutdown,
}

/// A snapshot of what the worker knows, shipped whenever it changes.
#[derive(Debug, Clone)]
pub struct MachineState {
    pub version: String,
    pub port: String,
    pub pen: Pen,
    pub position: Point,
}

/// Something the worker wants the app to know.
#[derive(Debug, Clone)]
pub enum Event {
    /// A long operation started; the string labels it for the status bar.
    Busy(String),
    /// An operation finished; carries the fresh machine snapshot (implies idle).
    State(MachineState),
    /// Progress through the current plan, one per executed op.
    Progress { done: usize, total: usize },
    /// The plan finished on its own.
    PlanDone,
    /// The plan was stopped or aborted before the end.
    Aborted,
    /// A command failed. Also logged; surfaced so the UI can flag it.
    Error(String),
}

/// Handle to a running worker: send commands, receive events, join on exit.
pub struct Worker {
    commands: Sender<Command>,
    events: Receiver<Event>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    /// Move `driver` onto a new thread and start serving commands.
    pub fn spawn(driver: Driver) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::channel::<Event>();
        let join = thread::Builder::new()
            .name("plotter".to_owned())
            .spawn(move || run(driver, &cmd_rx, &evt_tx))
            .expect("spawning the plotter thread");
        Self {
            commands: cmd_tx,
            events: evt_rx,
            join: Some(join),
        }
    }

    /// Queue a command; fails only if the worker thread is gone.
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::error!("plotter thread is gone; command dropped");
        }
    }

    /// Non-blocking event drain for the UI loop.
    pub fn try_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Block up to `timeout` for the next event. Used by tests; the UI loop
    /// polls with [`Worker::try_event`] instead.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Ask the thread to finish and wait for it (called on quit).
    pub fn shutdown(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The worker loop: greet with a state snapshot, then serve commands until
/// shutdown or the command channel closes.
fn run(mut driver: Driver, commands: &Receiver<Command>, events: &Sender<Event>) {
    emit(events, Event::State(snapshot(&driver)));

    while let Ok(command) = commands.recv() {
        match command {
            Command::Shutdown => break,
            Command::RunPlan(plan) => run_plan(&mut driver, &plan, commands, events),
            other => {
                run_one(&mut driver, other, events);
                emit(events, Event::State(snapshot(&driver)));
            }
        }
    }
    tracing::info!("plotter thread exiting");
}

/// Execute a single non-plan command, reporting a failure as an event.
fn run_one(driver: &mut Driver, command: Command, events: &Sender<Event>) {
    let (label, result): (&str, Result<(), DriverError>) = match command {
        Command::PenUp => ("pen up", driver.pen_up()),
        Command::PenDown => ("pen down", driver.pen_down()),
        Command::PenToggle => ("pen", driver.toggle_pen()),
        Command::Home => {
            emit(events, Event::Busy("homing".to_owned()));
            ("homing", driver.home())
        }
        Command::DisableMotors => ("disabling motors", driver.disable_motors()),
        Command::Jog { dx_mm, dy_mm } => ("jog", driver.jog(dx_mm, dy_mm)),
        Command::EmergencyStop => {
            emit(events, Event::Busy("emergency stop".to_owned()));
            ("emergency stop", driver.emergency_stop())
        }
        Command::Raw(line) => {
            emit(events, Event::Busy("sending".to_owned()));
            match driver.send_raw(&line) {
                Ok(replies) => {
                    tracing::info!(%line, reply = %replies.join(" | "), "console");
                    ("console", Ok(()))
                }
                Err(err) => ("console", Err(err)),
            }
        }
        // Stop/RunPlan/Shutdown are handled by the loop, never reach here.
        Command::Stop | Command::RunPlan(_) | Command::Shutdown => ("", Ok(())),
    };
    if let Err(err) = result {
        tracing::error!(%err, "{label} failed");
        emit(events, Event::Error(err.to_string()));
    }
}

/// Draw a whole plan op by op, checking for a stop between ops.
fn run_plan(
    driver: &mut Driver,
    plan: &Plan,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
) {
    let total = plan.ops.len();
    emit(events, Event::Busy("drawing".to_owned()));

    for (index, op) in plan.ops.iter().enumerate() {
        // A stop only has to land on an op boundary (short ops keep it snappy).
        match commands.try_recv() {
            Ok(Command::Stop | Command::EmergencyStop) => {
                tracing::info!(done = index, total, "plan stopped");
                abort(driver, events);
                return;
            }
            Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                abort(driver, events);
                return;
            }
            // Other commands are ignored while a plan runs (queue is drained).
            Ok(_) | Err(TryRecvError::Empty) => {}
        }

        if let Err(err) = apply(driver, op) {
            tracing::error!(%err, done = index, "plan op failed");
            emit(events, Event::Error(err.to_string()));
            abort(driver, events);
            return;
        }
        emit(
            events,
            Event::Progress {
                done: index + 1,
                total,
            },
        );
    }

    tracing::info!(ops = total, "plan done");
    emit(events, Event::PlanDone);
    emit(events, Event::State(snapshot(driver)));
}

/// Lift the pen and report the abort, best-effort.
fn abort(driver: &mut Driver, events: &Sender<Event>) {
    if let Err(err) = driver.pen_up() {
        tracing::warn!(%err, "pen up during abort failed");
    }
    emit(events, Event::Aborted);
    emit(events, Event::State(snapshot(driver)));
}

/// Translate one plan op into a driver call.
fn apply(driver: &mut Driver, op: &Op) -> Result<(), DriverError> {
    match op {
        Op::PenUp => driver.pen_up(),
        Op::PenDown => driver.pen_down(),
        Op::MoveTo(p) => driver.move_to(*p),
        Op::SetFeed(feed) => driver.set_feed(*feed),
        Op::Dwell(seconds) => driver.dwell(*seconds),
    }
}

fn snapshot(driver: &Driver) -> MachineState {
    MachineState {
        version: driver.version().to_owned(),
        port: driver.port().to_owned(),
        pen: driver.pen(),
        position: driver.position(),
    }
}

fn emit(events: &Sender<Event>, event: Event) {
    // A closed receiver means the app is shutting down; nothing to do.
    let _ = events.send(event);
}
