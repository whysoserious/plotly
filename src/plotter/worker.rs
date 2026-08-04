//! The plotter worker: a dedicated thread that owns the [`Driver`] and talks to
//! the board, driven by a channel of [`Command`]s and reporting [`Event`]s.
//! DESIGN.org §4 / step 2.4.
//!
//! `serialport` is blocking, so all board I/O lives off the UI thread. The app
//! never touches the transport directly — it sends commands and reacts to
//! events, which keeps the TUI responsive while `$H` or a long plan runs.

use std::ops::ControlFlow;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::geometry::Point;
use crate::job::ProgressWriter;
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
    /// Draw a whole plan, checkpointing progress through `progress` if given.
    RunPlan {
        plan: Plan,
        progress: Option<ProgressWriter>,
    },
    /// Panic abort: pen up, then soft reset (also aborts a running plan).
    EmergencyStop,
    /// Stop drawing the current plan (pen up), keep the connection.
    Stop,
    /// Feed-hold the running plan (pause).
    Pause,
    /// Resume a paused plan.
    Resume,
    /// Stop the running plan `after` has elapsed, lifting the pen if `pen_up`.
    /// A safety cutoff; acts on the next op boundary once the time is up (§9).
    StopAfter {
        after: Duration,
        pen_up: bool,
    },
    /// Stop the running plan once `mm` of travel is done, lifting the pen if
    /// `pen_up`. A distance safety cutoff, sibling of [`Command::StopAfter`].
    StopAfterDistance {
        mm: f64,
        pen_up: bool,
    },
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
    /// Progress through the current plan: ops done, distance travelled and time
    /// elapsed so far. One per executed op.
    Progress {
        done: usize,
        total: usize,
        distance_mm: f64,
        elapsed_secs: f64,
    },
    /// The plan is paused (feed-hold) at `done`/`total`.
    Paused { done: usize, total: usize },
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
            // A plan can absorb a Shutdown between ops; when it does, honour it
            // here too instead of looping back to a `recv` that would block.
            Command::RunPlan { plan, progress } => {
                if run_plan(&mut driver, &plan, progress.as_ref(), commands, events).is_break() {
                    break;
                }
            }
            other => {
                run_one(&mut driver, other, events);
                emit(events, Event::State(snapshot(&driver)));
            }
        }
    }

    // Leave the machine safe on exit: release the steppers (user request). The
    // carriage can then be moved by hand; the next session re-homes anyway.
    if let Err(err) = driver.disable_motors() {
        tracing::warn!(%err, "could not disable motors on exit");
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
        // Stop while idle just makes sure the pen is up.
        Command::Stop => ("stop", driver.pen_up()),
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
        // Pause/Resume/StopAfter only mean something during a plan; ignore here.
        // RunPlan/Shutdown are handled by the loop, never reach here.
        Command::Pause
        | Command::Resume
        | Command::StopAfter { .. }
        | Command::StopAfterDistance { .. }
        | Command::RunPlan { .. }
        | Command::Shutdown => ("", Ok(())),
    };
    if let Err(err) = result {
        tracing::error!(%err, "{label} failed");
        emit(events, Event::Error(err.to_string()));
    }
}

/// Draw a whole plan op by op, checking for a stop between ops.
///
/// Returns [`ControlFlow::Break`] when it consumed a `Shutdown` (or the channel
/// closed), so the caller ends the thread instead of blocking on the next recv.
fn run_plan(
    driver: &mut Driver,
    plan: &Plan,
    progress: Option<&ProgressWriter>,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
) -> ControlFlow<()> {
    let total = plan.ops.len();
    emit(events, Event::Busy("drawing".to_owned()));

    // Safety cutoffs, armed mid-plan and checked each boundary: a deadline
    // (`StopAfter`) and a travel budget in mm (`StopAfterDistance`).
    let mut cutoff: Option<(Instant, bool)> = None;
    let mut distance_cutoff: Option<(f64, bool)> = None;
    // Live metrics reported with each Progress event.
    let started = Instant::now();
    let mut distance_mm = 0.0_f64;

    for (index, op) in plan.ops.iter().enumerate() {
        // A stop/pause only has to land on an op boundary (short ops keep it
        // snappy). `index` is the count already drawn — the resume checkpoint.
        match commands.try_recv() {
            Ok(Command::StopAfter { after, pen_up }) => {
                tracing::info!(?after, pen_up, "timed stop armed");
                cutoff = Some((Instant::now() + after, pen_up));
            }
            Ok(Command::StopAfterDistance { mm, pen_up }) => {
                tracing::info!(mm, pen_up, "distance stop armed");
                distance_cutoff = Some((distance_mm + mm, pen_up));
            }
            Ok(command) => {
                match handle_interrupt(driver, command, index, total, commands, events) {
                    Interrupt::Continue => {}
                    Interrupt::Stopped => {
                        checkpoint(progress, index, total, driver);
                        return ControlFlow::Continue(());
                    }
                    Interrupt::Shutdown => {
                        checkpoint(progress, index, total, driver);
                        return ControlFlow::Break(());
                    }
                }
            }
            Err(TryRecvError::Disconnected) => {
                abort(driver, events);
                checkpoint(progress, index, total, driver);
                return ControlFlow::Break(());
            }
            Err(TryRecvError::Empty) => {}
        }

        // Either cutoff only has to fire by the next boundary once it is due.
        if let Some(pen_up) = cutoff_fired(cutoff, Instant::now()) {
            tracing::info!(done = index, pen_up, "timed stop");
            stop_plan(driver, events, pen_up);
            checkpoint(progress, index, total, driver);
            return ControlFlow::Continue(());
        }
        if let Some((budget, pen_up)) = distance_cutoff {
            if distance_mm >= budget {
                tracing::info!(done = index, distance_mm, pen_up, "distance stop");
                stop_plan(driver, events, pen_up);
                checkpoint(progress, index, total, driver);
                return ControlFlow::Continue(());
            }
        }

        let before = driver.position();
        if let Err(err) = apply(driver, op) {
            tracing::error!(%err, done = index, "plan op failed");
            emit(events, Event::Error(err.to_string()));
            abort(driver, events);
            checkpoint(progress, index, total, driver);
            return ControlFlow::Continue(());
        }
        let after = driver.position();
        distance_mm += (after.x - before.x).hypot(after.y - before.y);

        let sent = index + 1;
        emit(
            events,
            Event::Progress {
                done: sent,
                total,
                distance_mm,
                elapsed_secs: started.elapsed().as_secs_f64(),
            },
        );
        checkpoint(progress, sent, total, driver);
    }

    tracing::info!(ops = total, "plan done");
    if let Some(writer) = progress {
        let pos = driver.position();
        if let Err(err) = writer.finish(total, [pos.x, pos.y]) {
            tracing::warn!(%err, "final progress write failed");
        }
    }
    emit(events, Event::PlanDone);
    emit(events, Event::State(snapshot(driver)));
    ControlFlow::Continue(())
}

/// Atomically checkpoint progress, if a writer is attached. `sent` is the count
/// of ops acknowledged; the writer lags it by the planner depth (§6).
fn checkpoint(progress: Option<&ProgressWriter>, sent: usize, total: usize, driver: &Driver) {
    let Some(writer) = progress else { return };
    let pos = driver.position();
    let pen_down = driver.pen() == Pen::Down;
    if let Err(err) = writer.checkpoint(sent, total, [pos.x, pos.y], pen_down) {
        tracing::warn!(%err, "progress checkpoint failed");
    }
}

/// Outcome of a command received mid-plan.
enum Interrupt {
    /// Keep drawing.
    Continue,
    /// Stop this plan; keep serving other commands.
    Stopped,
    /// End the worker thread.
    Shutdown,
}

/// Act on a command that arrived between ops. Handles pause by blocking here
/// until resume (or a stop), so `done`/`total` — the resume checkpoint — is
/// preserved across the hold.
fn handle_interrupt(
    driver: &mut Driver,
    command: Command,
    done: usize,
    total: usize,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
) -> Interrupt {
    match command {
        Command::Stop => {
            tracing::info!(done, total, "plan stopped");
            abort(driver, events);
            Interrupt::Stopped
        }
        Command::EmergencyStop => {
            tracing::warn!(done, total, "plan aborted (panic)");
            if let Err(err) = driver.emergency_stop() {
                tracing::warn!(%err, "emergency stop failed");
            }
            emit(events, Event::Aborted);
            emit(events, Event::State(snapshot(driver)));
            Interrupt::Stopped
        }
        Command::Pause => pause(driver, done, total, commands, events),
        Command::Shutdown => {
            abort(driver, events);
            Interrupt::Shutdown
        }
        // Resume with no hold, or anything else, is a no-op mid-plan.
        _ => Interrupt::Continue,
    }
}

/// Feed-hold and block until resumed, stopped or shut down.
fn pause(
    driver: &mut Driver,
    done: usize,
    total: usize,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
) -> Interrupt {
    if let Err(err) = driver.feed_hold() {
        tracing::warn!(%err, "feed hold failed");
    }
    tracing::info!(done, total, "plan paused");
    emit(events, Event::Paused { done, total });

    while let Ok(command) = commands.recv() {
        match command {
            Command::Resume => {
                if let Err(err) = driver.resume() {
                    tracing::warn!(%err, "resume failed");
                }
                tracing::info!(done, total, "plan resumed");
                emit(events, Event::Busy("drawing".to_owned()));
                return Interrupt::Continue;
            }
            Command::Stop => {
                abort(driver, events);
                return Interrupt::Stopped;
            }
            Command::EmergencyStop => {
                if let Err(err) = driver.emergency_stop() {
                    tracing::warn!(%err, "emergency stop failed");
                }
                emit(events, Event::Aborted);
                emit(events, Event::State(snapshot(driver)));
                return Interrupt::Stopped;
            }
            Command::Shutdown => {
                abort(driver, events);
                return Interrupt::Shutdown;
            }
            // Ignore anything else (including a second Pause) while held.
            _ => {}
        }
    }
    // Channel closed while paused.
    abort(driver, events);
    Interrupt::Shutdown
}

/// Whether an armed cutoff has elapsed by `now`; yields its pen-up flag.
fn cutoff_fired(cutoff: Option<(Instant, bool)>, now: Instant) -> Option<bool> {
    cutoff.and_then(|(deadline, pen_up)| (now >= deadline).then_some(pen_up))
}

/// Stop the plan, lifting the pen only if asked, and report it.
fn stop_plan(driver: &mut Driver, events: &Sender<Event>, pen_up: bool) {
    if pen_up {
        if let Err(err) = driver.pen_up() {
            tracing::warn!(%err, "pen up during stop failed");
        }
    }
    emit(events, Event::Aborted);
    emit(events, Event::State(snapshot(driver)));
}

/// Lift the pen and report the abort, best-effort.
fn abort(driver: &mut Driver, events: &Sender<Event>) {
    stop_plan(driver, events, true);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disarmed_cutoff_never_fires() {
        assert_eq!(cutoff_fired(None, Instant::now()), None);
    }

    #[test]
    fn a_cutoff_fires_only_once_its_deadline_passes() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(50);
        assert_eq!(cutoff_fired(Some((deadline, true)), now), None);
        assert_eq!(
            cutoff_fired(Some((deadline, true)), deadline + Duration::from_millis(1)),
            Some(true)
        );
    }

    #[test]
    fn the_cutoff_carries_its_pen_up_choice() {
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            cutoff_fired(Some((past, false)), Instant::now()),
            Some(false)
        );
    }
}
