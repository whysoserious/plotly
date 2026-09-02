//! Application state and the event/render loop. DESIGN.org §4.
//!
//! The app owns no transport: the [`Worker`] thread does. The app sends
//! [`Command`]s and folds [`Event`]s into a local mirror of the machine state,
//! so the UI stays responsive while the board is busy.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::job::{self, Job, Resumable};
use crate::keys::{action_for, Action, Mode};
use crate::logging::LogRing;
use crate::plan::estimate::{self, Estimate};
use crate::plan::{Plan, Shape};
use crate::plotter::worker::{Command, Event, MachineState, Worker};
use crate::profiles::Profile;
use crate::{tui, ui};

/// Idle poll timeout: bounds how often we wake to pick up worker events and new
/// log lines while keeping idle CPU negligible (we redraw only on a change).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Jog step sizes cycled by `+`/`-`, in millimetres (DESIGN.org §8).
const JOG_STEPS_MM: [f64; 4] = [0.1, 1.0, 5.0, 10.0];
/// Index into [`JOG_STEPS_MM`] the app starts on (1 mm).
const DEFAULT_STEP_INDEX: usize = 1;

/// Safety-timer options cycled by `t`, in minutes (§9). `t` steps through these
/// then back to off; an armed timer stops the plot and lifts the pen.
const STOP_TIMER_MINUTES: [u64; 3] = [1, 5, 15];

/// Distance-cutoff options cycled by `m`, in millimetres. Stops the plot and
/// lifts the pen once that much travel is done.
const STOP_DISTANCE_MM: [f64; 3] = [500.0, 1000.0, 5000.0];

/// Outcome of a key press while the resume prompt is up.
enum ResumeReply {
    /// The prompt consumed the key (Enter/n/Esc or a release).
    Handled,
    /// The prompt was dismissed; the key should be handled normally.
    Dismissed,
}

/// What the machine is doing, for the status bar. Derived from worker events.
#[derive(Debug, Clone)]
pub enum Activity {
    Idle,
    Busy(String),
    Drawing {
        done: usize,
        total: usize,
        distance_mm: f64,
        elapsed_secs: f64,
    },
}

/// Top-level TUI application state.
pub struct App {
    worker: Worker,
    /// Last machine snapshot from the worker (pen, position, identity).
    machine: MachineState,
    activity: Activity,
    /// A short note shown after a plan ends ("done", "stopped", …).
    note: Option<String>,
    /// A pause was asked for and the plot is drawing out the current shape
    /// before it takes hold, for the status bar.
    pausing: bool,
    /// The machine this run is driving: field, feeds, pen heights (§10).
    profile: Profile,
    /// The loaded drawing in drawing-logical mm, before placement. Kept so the
    /// plan can be laid down afresh from wherever the head is when `Enter` is
    /// pressed; empty when nothing was loaded.
    shapes: Vec<Shape>,
    /// The plan to draw, placed into machine coordinates; `Enter` draws it.
    plan: Option<Plan>,
    /// Cached dry-run of the plan: time and distances, for the ETA (step 5.3).
    estimate: Option<Estimate>,
    /// Source SVG path, recorded in a job's metadata (§6).
    source: Option<String>,
    /// The job directory for the current print, created when it starts (§3.1).
    job: Option<Job>,
    /// An unfinished job found at startup, prompting to resume (§3.3). While
    /// `Some`, the resume overlay is shown and captures the next key.
    resume: Option<Resumable>,
    /// Op index to resume drawing from, set when a resume is accepted (§3.4).
    resume_from: Option<usize>,
    /// Ops of the plan executed so far — the anchor for stroke progress. Set
    /// from worker events, from the resume point, and back to 0 on a fresh
    /// start, so the stroke list agrees with the machine at all times.
    ops_done: usize,
    /// Strokes finished at the last update, to log each one exactly once.
    strokes_done: usize,
    /// Raw G-code console (step 1.5): `Some` while open, holding the typed line.
    console: Option<String>,
    /// Whether the key overview is covering the screen.
    help: bool,
    /// Whether the stroke list shares the canvas row.
    strokes_panel: bool,
    /// Current jog step, as an index into [`JOG_STEPS_MM`].
    step_index: usize,
    /// Armed safety timer as an index into [`STOP_TIMER_MINUTES`]; `None` = off.
    stop_timer: Option<usize>,
    /// Armed distance cutoff as an index into [`STOP_DISTANCE_MM`]; `None` = off.
    stop_distance: Option<usize>,
    /// Terminal events read ahead during jog coalescing, not yet handled.
    pending_events: VecDeque<TermEvent>,
    log: LogRing,
    last_log_len: usize,
    should_quit: bool,
}

impl App {
    pub fn new(
        worker: Worker,
        machine: MachineState,
        profile: Profile,
        shapes: Vec<Shape>,
        source: Option<String>,
        resume: Option<Resumable>,
        log: LogRing,
    ) -> Self {
        // A plan for the preview, laid down from where the head is now. It is
        // rebuilt when the plot starts, so jogging first moves the drawing.
        let plan =
            (!shapes.is_empty()).then(|| crate::build_plan(&shapes, machine.position, &profile));
        let dry_run = estimate::Machine::from_profile(&profile);
        let estimate = plan.as_ref().map(|p| p.estimate(&dry_run));
        Self {
            worker,
            machine,
            activity: Activity::Idle,
            note: None,
            pausing: false,
            profile,
            shapes,
            plan,
            estimate,
            source,
            job: None,
            resume,
            resume_from: None,
            ops_done: 0,
            strokes_done: 0,
            console: None,
            help: false,
            strokes_panel: true,
            step_index: DEFAULT_STEP_INDEX,
            stop_timer: None,
            stop_distance: None,
            pending_events: VecDeque::new(),
            last_log_len: log.len(),
            log,
            should_quit: false,
        }
    }

    /// Run the event loop until the user quits. Event-driven + dirty: renders
    /// only on a key, a resize, a worker event, or new log lines (DESIGN.org §4).
    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<()> {
        let mut needs_redraw = true;
        while !self.should_quit {
            // A termination signal (kill -INT/-TERM) asks for a graceful exit:
            // fall out of the loop so the worker lifts the pen, releases the
            // motors and checkpoints, and the terminal guard restores (§3.5).
            if tui::shutdown_requested() {
                tracing::info!("shutdown signal received; stopping");
                break;
            }
            if needs_redraw {
                terminal.draw(|frame| ui::draw(frame, self))?;
                needs_redraw = false;
            }

            if let Some(event) = self.next_term_event()? {
                match event {
                    TermEvent::Key(key) => needs_redraw |= self.on_key(key),
                    TermEvent::Resize(_, _) => needs_redraw = true,
                    _ => {}
                }
            }

            needs_redraw |= self.drain_worker_events();

            let len = self.log.len();
            if len != self.last_log_len {
                self.last_log_len = len;
                needs_redraw = true;
            }
        }
        self.worker.shutdown();
        Ok(())
    }

    /// Next terminal event: a read-ahead one first, else poll the terminal.
    fn next_term_event(&mut self) -> io::Result<Option<TermEvent>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        if event::poll(POLL_INTERVAL)? {
            return Ok(Some(event::read()?));
        }
        Ok(None)
    }

    /// Fold all pending worker events into the local state; returns whether the
    /// screen changed.
    fn drain_worker_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.worker.try_event() {
            changed = true;
            match event {
                // Any fresh activity — including the "drawing" that follows a
                // resume or a called-off pause — ends the pending pause.
                Event::Busy(label) => {
                    self.pausing = false;
                    self.activity = Activity::Busy(label);
                }
                Event::State(machine) => {
                    self.machine = machine;
                    self.activity = Activity::Idle;
                }
                Event::Progress {
                    done,
                    total,
                    distance_mm,
                    elapsed_secs,
                } => {
                    // Just the counter here: the stroke maths below runs once
                    // per batch, not once per op.
                    self.ops_done = done;
                    self.activity = Activity::Drawing {
                        done,
                        total,
                        distance_mm,
                        elapsed_secs,
                    }
                }
                Event::Pausing => self.pausing = true,
                Event::Paused { done, total } => {
                    self.pausing = false;
                    self.ops_done = done;
                    self.activity =
                        Activity::Busy(format!("paused {done}/{total}, pen up (r resume)"));
                }
                Event::PlanDone { elapsed_secs } => {
                    self.pausing = false;
                    if let Some(plan) = &self.plan {
                        self.ops_done = plan.ops.len();
                    }
                    // How long it took is the first thing asked once a plot
                    // ends, and by then the live counter is gone from the
                    // status bar — so the note keeps the number on screen.
                    self.note = Some(format!("done in {}", ui::fmt_time(elapsed_secs)));
                }
                Event::Aborted => {
                    self.pausing = false;
                    self.note = Some("stopped".to_owned());
                }
                Event::Error(err) => self.note = Some(format!("error: {err}")),
            }
        }
        if changed {
            self.log_strokes();
        }
        changed
    }

    /// Move the plan counter and report the strokes that reached the paper.
    fn set_ops_done(&mut self, ops_done: usize) {
        self.ops_done = ops_done;
        self.log_strokes();
    }

    /// Log the stroke counter whenever it moves.
    ///
    /// Called once per batch of worker events rather than per op: the board
    /// acknowledges ops far faster than the UI redraws, and §5 asks for
    /// coalesced progress instead of a line per acknowledgement. Counting the
    /// strokes costs a pass over them, which is another reason not to do it on
    /// every op.
    fn log_strokes(&mut self) {
        let Some(progress) = self.stroke_progress() else {
            return;
        };
        if progress.done != self.strokes_done {
            self.strokes_done = progress.done;
            tracing::debug!(
                strokes = progress.done,
                total = progress.total,
                percent = progress.percent(),
                drawn_mm = progress.drawn_mm,
                "strokes drawn"
            );
        }
    }

    /// Progress through the loaded plan counted in strokes, for the panel.
    ///
    /// Derived from the op index rather than reported by the worker: a stroke
    /// is a range of ops, so the index the worker already sends is all it takes
    /// (§3.7). One consequence worth having — a resumed job gets the right
    /// stroke count for free, from its committed index.
    pub fn stroke_progress(&self) -> Option<crate::plan::StrokeProgress> {
        self.plan
            .as_ref()
            .map(|plan| plan.stroke_progress(self.ops_done))
    }

    /// Whether the stroke list is switched on (`s`).
    pub fn strokes_visible(&self) -> bool {
        self.strokes_panel
    }

    /// Handle one key press; returns whether the screen has to be redrawn.
    fn on_key(&mut self, key: KeyEvent) -> bool {
        // The resume prompt catches its own keys (Enter/n/Esc); any other key
        // dismisses it and is then handled normally, so machine controls like
        // `h` are never blocked behind a leftover prompt (§3.3).
        let mut dismissed = false;
        if self.resume.is_some() {
            match self.answer_resume(key) {
                ResumeReply::Handled => return true,
                ResumeReply::Dismissed => dismissed = true, // fall through
            }
        }
        let Some(action) = action_for(self.mode(), &key) else {
            // Dismissing the overlay changed the screen even if the key is
            // otherwise unbound.
            return dismissed;
        };
        // While the overview is up, any key dismisses it and does nothing else
        // — except quitting, which people expect to work from anywhere.
        if self.help && action != Action::Quit {
            self.help = false;
            return true;
        }
        // A fresh action clears the last plan's note.
        self.note = None;
        match action {
            Action::Quit => {
                tracing::info!("quit requested");
                self.should_quit = true;
            }
            Action::PenUp => self.worker.send(Command::PenUp),
            Action::PenDown => self.worker.send(Command::PenDown),
            Action::PenToggle => self.worker.send(Command::PenToggle),
            Action::Home => self.worker.send(Command::Home),
            Action::DisableMotors => self.worker.send(Command::DisableMotors),
            Action::Pause => self.worker.send(Command::Pause),
            Action::Resume => self.worker.send(Command::Resume),
            Action::Stop => self.worker.send(Command::Stop),
            Action::PanicStop => self.worker.send(Command::EmergencyStop),
            Action::Jog { dx, dy } => self.jog(dx, dy),
            Action::StepBigger => {
                self.step_index = (self.step_index + 1).min(JOG_STEPS_MM.len() - 1);
            }
            Action::StepSmaller => self.step_index = self.step_index.saturating_sub(1),
            Action::StartPlot => self.start_plot(),
            Action::Frame => self.trace_frame(),
            Action::CycleStopTimer => self.cycle_stop_timer(),
            Action::CycleStopDistance => self.cycle_stop_distance(),
            Action::ToggleStrokes => {
                self.strokes_panel = !self.strokes_panel;
                tracing::info!(visible = self.strokes_panel, "stroke list");
            }
            Action::OpenConsole => {
                tracing::info!("raw G-code console open");
                self.console = Some(String::new());
            }
            Action::CloseConsole => {
                tracing::info!("raw G-code console closed");
                self.console = None;
            }
            Action::Input(c) => {
                if let Some(line) = &mut self.console {
                    line.push(c);
                }
            }
            Action::Backspace => {
                if let Some(line) = &mut self.console {
                    line.pop();
                }
            }
            Action::Submit => self.submit_console(),
            Action::ToggleHelp => self.help = !self.help,
        }
        true
    }

    /// Answer the startup resume prompt: Enter resumes, `n`/Esc declines. Any
    /// other key also declines but is then handled normally (see [`on_key`]),
    /// so the prompt never blocks the machine controls.
    fn answer_resume(&mut self, key: KeyEvent) -> ResumeReply {
        if key.kind != KeyEventKind::Press {
            return ResumeReply::Handled; // ignore key releases
        }
        match key.code {
            KeyCode::Enter => {
                self.accept_resume();
                ResumeReply::Handled
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                tracing::info!("resume declined; starting fresh");
                self.resume = None;
                ResumeReply::Handled
            }
            _ => {
                tracing::info!("resume dismissed by another key; starting fresh");
                self.resume = None;
                ResumeReply::Dismissed
            }
        }
    }

    /// Load the resumable job's plan and mark where to continue from (§3.3).
    /// The go-to-position drawing from that index is step 3.4.
    fn accept_resume(&mut self) {
        let Some(resumable) = self.resume.take() else {
            return;
        };
        match job::read_job_plan(&resumable.dir) {
            Ok(plan) => {
                let committed = resumable.progress.committed_index;
                tracing::info!(
                    job = resumable.meta.job_id,
                    from = committed,
                    "resuming job"
                );
                self.estimate = Some(plan.estimate(&self.estimate_machine()));
                self.plan = Some(plan);
                self.source = resumable.meta.source.clone();
                self.job = Some(Job {
                    id: resumable.meta.job_id,
                    dir: resumable.dir.clone(),
                });
                self.resume_from = Some(committed);
                // Show the job where it actually stands: everything below the
                // committed index is already on the paper.
                self.strokes_done = 0;
                self.set_ops_done(committed);
                self.note = Some(format!(
                    "resume from {}% — press enter",
                    resumable.percent()
                ));
            }
            Err(err) => {
                tracing::warn!(%err, "could not load the job to resume");
                self.note = Some("could not load job to resume".to_owned());
            }
        }
    }

    /// The pending resume prompt, for the overlay.
    pub fn resume_prompt(&self) -> Option<&Resumable> {
        self.resume.as_ref()
    }

    /// Op index to resume drawing from, once accepted (consumed in step 3.4).
    pub fn resume_from(&self) -> Option<usize> {
        self.resume_from
    }

    /// Start drawing the loaded plan, if there is one, arming the safety timer.
    fn start_plot(&mut self) {
        // A fresh plot is laid down from where the head is *now*: jog to the
        // corner of the sheet, press enter, and the drawing starts under the
        // pen. A resume keeps the plan it was interrupted with — re-placing it
        // would tear the drawing in two.
        if self.resume_from.is_none() && !self.shapes.is_empty() {
            self.place_at_head();
            self.warn_if_off_field();
        }
        // Cloned up front: the worker takes ownership of the plan, and the copy
        // the app keeps stays available for the preview and the stroke list.
        let Some(plan) = self.plan.clone() else {
            self.note = Some("no SVG loaded".to_owned());
            return;
        };
        tracing::info!(ops = plan.ops.len(), "starting plot");

        // Resuming an accepted job continues its own directory from the
        // committed index; a fresh plot starts a new job at 0 (§3.4).
        let start_index = self.resume_from.take().unwrap_or(0);
        if start_index == 0 {
            self.job = self.create_job(&plan);
        }
        // The stroke list starts from where this run starts, not from whatever
        // the previous plan left behind.
        self.strokes_done = 0;
        self.set_ops_done(start_index);
        let progress = self.job.as_ref().map(Job::progress_writer);

        self.worker.send(Command::RunPlan {
            plan,
            progress,
            start_index,
        });
        // Arm the safety cutoffs, if set, right after the plan starts (§2.8).
        if let Some(minutes) = self.stop_timer_minutes() {
            self.worker.send(Command::StopAfter {
                after: Duration::from_secs(minutes * 60),
                pen_up: true,
            });
        }
        if let Some(mm) = self.stop_distance_mm() {
            self.worker
                .send(Command::StopAfterDistance { mm, pen_up: true });
        }
    }

    /// Trace the drawing's outline with the pen up, so the operator can see
    /// where it will land before committing paper to it (step 5.3).
    ///
    /// The plan is placed at the head first, exactly as `Enter` would, so what
    /// the frame traces is what a plot started right now would fill.
    fn trace_frame(&mut self) {
        if self.resume_from.is_none() && !self.shapes.is_empty() {
            self.place_at_head();
        }
        let Some(outline) = self.plan.as_ref().and_then(Plan::frame_outline) else {
            self.note = Some("nothing to frame".to_owned());
            return;
        };
        self.warn_if_off_field();
        self.worker.send(Command::Frame {
            outline,
            feed: self.profile.plan.travel_feed,
        });
    }

    /// Put a note on screen when the placed drawing leaves the machine's field.
    ///
    /// `build_plan` already logs this, but the log scrolls and the carriage
    /// does not care: the operator about to press `enter` needs it in front of
    /// them. Clipping itself is still the host's job (§2.4).
    fn warn_if_off_field(&mut self) {
        let Some((min, max)) = self.plan.as_ref().and_then(Plan::drawn_bounds) else {
            return;
        };
        let field = &self.profile.field;
        if !field.contains(min) || !field.contains(max) {
            self.note = Some(format!(
                "warning: the drawing runs off the {} field from here",
                self.profile.name
            ));
        }
    }

    /// Rebuild the plan with the drawing anchored at the head's position, and
    /// refresh what the UI derives from it.
    fn place_at_head(&mut self) {
        let at = self.machine.position;
        let plan = crate::build_plan(&self.shapes, at, &self.profile);
        tracing::info!(
            x = at.x,
            y = at.y,
            strokes = plan.stroke_count(),
            "drawing placed at the head"
        );
        self.estimate = Some(plan.estimate(&self.estimate_machine()));
        self.plan = Some(plan);
    }

    /// Create the on-disk job for `plan`, or log why it could not be made.
    /// A persistence failure is not fatal — the print can still run.
    fn create_job(&self, plan: &Plan) -> Option<Job> {
        let root = job::jobs_root()?;
        match Job::create(&root, plan, self.source.as_deref()) {
            Ok(job) => Some(job),
            Err(err) => {
                tracing::warn!(%err, "could not persist the job; drawing anyway");
                None
            }
        }
    }

    /// Cycle the safety timer: off → 1 → 5 → 15 min → off.
    fn cycle_stop_timer(&mut self) {
        self.stop_timer = match self.stop_timer {
            None => Some(0),
            Some(i) if i + 1 < STOP_TIMER_MINUTES.len() => Some(i + 1),
            Some(_) => None,
        };
        match self.stop_timer_minutes() {
            Some(m) => tracing::info!(minutes = m, "safety timer armed"),
            None => tracing::info!("safety timer off"),
        }
    }

    /// Cycle the distance cutoff: off → 0.5 → 1 → 5 m → off.
    fn cycle_stop_distance(&mut self) {
        self.stop_distance = match self.stop_distance {
            None => Some(0),
            Some(i) if i + 1 < STOP_DISTANCE_MM.len() => Some(i + 1),
            Some(_) => None,
        };
        match self.stop_distance_mm() {
            Some(mm) => tracing::info!(mm, "distance cutoff armed"),
            None => tracing::info!("distance cutoff off"),
        }
    }

    /// The armed distance cutoff in mm, if any.
    pub fn stop_distance_mm(&self) -> Option<f64> {
        self.stop_distance.map(|i| STOP_DISTANCE_MM[i])
    }

    /// Cached dry-run of the loaded plan, for the ETA and the idle summary.
    pub fn estimate(&self) -> Option<Estimate> {
        self.estimate
    }

    /// The machine numbers the time estimate runs on, from the profile.
    fn estimate_machine(&self) -> estimate::Machine {
        estimate::Machine::from_profile(&self.profile)
    }

    /// The armed safety-timer duration in minutes, if any (for the status bar).
    pub fn stop_timer_minutes(&self) -> Option<u64> {
        self.stop_timer.map(|i| STOP_TIMER_MINUTES[i])
    }

    /// Jog by one step, coalescing a held arrow key into a single move.
    ///
    /// Key auto-repeat can deliver arrows faster than a jog round-trips, so
    /// before sending we drain the arrow presses already waiting and sum them:
    /// holding "right" becomes one longer jog instead of a backlog. Non-jog
    /// events found while draining are put back for the next loop.
    fn jog(&mut self, dx: i8, dy: i8) {
        let (mut sx, mut sy) = (i32::from(dx), i32::from(dy));
        while matches!(event::poll(Duration::ZERO), Ok(true)) {
            let Ok(event) = event::read() else { break };
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
        self.worker.send(Command::Jog {
            dx_mm: f64::from(sx) * step,
            dy_mm: f64::from(sy) * step,
        });
    }

    /// Send the typed console line and keep the console open for the next one.
    fn submit_console(&mut self) {
        let Some(line) = self.console.as_mut().map(std::mem::take) else {
            return;
        };
        let line = line.trim().to_owned();
        if !line.is_empty() {
            self.worker.send(Command::Raw(line));
        }
    }

    /// Which key map applies right now — the console makes input textual.
    fn mode(&self) -> Mode {
        match self.console {
            Some(_) => Mode::Console,
            None => Mode::Navigation,
        }
    }

    /// The current jog step in millimetres, for the status bar.
    pub fn jog_step_mm(&self) -> f64 {
        JOG_STEPS_MM[self.step_index]
    }

    pub fn log(&self) -> &LogRing {
        &self.log
    }

    pub fn machine(&self) -> &MachineState {
        &self.machine
    }

    /// The machine profile in force, for the status bar and the placement.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The loaded plan, if any, for the toolpath preview.
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    pub fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Whether a pause is waiting for the current shape to finish.
    pub fn pausing(&self) -> bool {
        self.pausing
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The line being typed, when the console is open.
    pub fn console(&self) -> Option<&str> {
        self.console.as_deref()
    }

    pub fn help_visible(&self) -> bool {
        self.help
    }
}

/// The jog delta of an event, if it is a navigation-mode jog key press. Key
/// releases (which some terminals emit) are ignored here so they neither add to
/// the sum nor stop coalescing.
fn jog_of(event: &TermEvent) -> Option<(i8, i8)> {
    let TermEvent::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        // Treat as "not a boundary": skip it. Represented by a zero jog.
        return Some((0, 0));
    }
    match action_for(Mode::Navigation, key) {
        Some(Action::Jog { dx, dy }) => Some((dx, dy)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Placement, Point};
    use crate::job::Job;
    use crate::plan::{Plan, PlanSettings};
    use crate::plotter::driver::{Driver, Pen};
    use crate::plotter::mock::MockTransport;
    use crate::plotter::Connection;
    use crossterm::event::KeyModifiers;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// An app whose startup found an unfinished job, so the resume prompt is up.
    fn app_with_resume_prompt() -> (App, Arc<Mutex<Vec<String>>>) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("plotly-app-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        let plan = Plan::build(
            &[vec![Point::new(0.0, 0.0), Point::new(30.0, 0.0)]],
            &Placement::identity(),
            &PlanSettings::default(),
        );
        let job = Job::create(&root, &plan, Some("x.svg")).unwrap();
        job.progress_writer()
            .checkpoint(20, plan.ops.len(), [1.0, 2.0], false)
            .unwrap();
        let resumable = crate::job::latest_resumable(&root).unwrap();

        let mock = MockTransport::new();
        let sent = mock.sent_handle();
        let driver = Driver::new(Connection {
            transport: Box::new(mock),
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
        });
        let worker = Worker::spawn(driver);
        let machine = MachineState {
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
            pen: Pen::Up,
            position: Point::new(0.0, 0.0),
        };
        let app = App::new(
            worker,
            machine,
            test_profile(),
            Vec::new(),
            None,
            Some(resumable),
            LogRing::new(),
        );
        (app, sent)
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The default machine, as resolved with no profile named and no `$$`.
    fn test_profile() -> Profile {
        Profile::builtin(crate::profiles::DEFAULT_PROFILE).expect("the default profile exists")
    }

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6
    }

    /// An app with a small drawing loaded and the head parked at the origin.
    fn app_with_a_drawing() -> App {
        let driver = Driver::new(Connection {
            transport: Box::new(MockTransport::new()),
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
        });
        let worker = Worker::spawn(driver);
        let machine = MachineState {
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
            pen: Pen::Up,
            position: Point::new(0.0, 0.0),
        };
        // The shape's first point is also its top-left corner, so "where the
        // drawing starts" and "where its corner sits" are the same assertion.
        let shapes = vec![Shape::unlabelled(vec![
            Point::new(0.0, 0.0),
            Point::new(20.0, 0.0),
            Point::new(20.0, 10.0),
        ])];
        App::new(
            worker,
            machine,
            test_profile(),
            shapes,
            None,
            None,
            LogRing::new(),
        )
    }

    /// The drawing follows the head: jog to the corner of the sheet, press
    /// enter, and the plot starts under the pen — not in the middle of the A0
    /// field, which is where fitting used to centre it.
    #[test]
    fn the_plot_starts_where_the_head_is_after_a_jog() {
        let mut app = app_with_a_drawing();
        let field = crate::geometry::Field::idraw_a0();

        let before = app.plan().expect("a preview plan").strokes[0].start;
        assert!(
            close(before, Point::new(0.0, 0.0)),
            "the drawing should start at the head, not at {before:?}"
        );

        // Jog right a few steps and let the worker report the new position.
        for _ in 0..3 {
            app.on_key(key(KeyCode::Right));
        }
        std::thread::sleep(Duration::from_millis(100));
        app.drain_worker_events();
        let head = app.machine().position;
        assert!(head.x > 0.0, "the jog never landed: {head:?}");

        app.on_key(key(KeyCode::Enter));

        let start = app.plan().expect("a plan to draw").strokes[0].start;
        assert!(
            close(start, head),
            "the plot starts at {start:?}, not at the head {head:?}"
        );
        assert!(
            start.x < field.width_mm / 4.0 && start.y < field.height_mm / 4.0,
            "the drawing was centred in the field again: {start:?}"
        );
    }

    /// `f` frames what `enter` would draw: the operator checks where the
    /// drawing lands, then commits — so the two must agree about the placement.
    #[test]
    fn the_frame_outlines_exactly_what_enter_would_draw() {
        let mut app = app_with_a_drawing();

        for _ in 0..5 {
            app.on_key(key(KeyCode::Right));
        }
        std::thread::sleep(Duration::from_millis(100));
        app.drain_worker_events();
        let head = app.machine().position;

        app.on_key(press('f'));
        let framed = app
            .plan()
            .and_then(Plan::frame_outline)
            .expect("the frame has an outline");

        // The frame starts at the head, and closes on itself.
        assert!(
            close(framed[0], head),
            "framed {:?} not {head:?}",
            framed[0]
        );
        assert_eq!(framed.first(), framed.last());

        // Pressing enter now draws inside that very box.
        app.on_key(key(KeyCode::Enter));
        let (min, max) = app
            .plan()
            .and_then(Plan::drawn_bounds)
            .expect("the plot has bounds");
        assert!(close(min, framed[0]), "{min:?} vs {:?}", framed[0]);
        assert!(close(max, framed[2]), "{max:?} vs {:?}", framed[2]);
    }

    /// Nothing loaded means nothing to frame — and it must say so rather than
    /// send the head somewhere on the strength of an empty box.
    #[test]
    fn framing_nothing_says_so_instead_of_moving() {
        let (mut app, sent) = app_with_resume_prompt();
        app.on_key(press('n')); // decline the resume; no drawing loaded
        app.on_key(press('f'));

        assert_eq!(app.note(), Some("nothing to frame"));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !sent.lock().unwrap().iter().any(|l| l.starts_with("G1 X")),
            "the head moved with nothing to frame"
        );
    }

    /// A resume must draw the plan it was interrupted with. Re-placing it at
    /// the head would leave the finished half and the rest in different places.
    #[test]
    fn resuming_keeps_the_original_placement() {
        let (mut app, _sent) = app_with_resume_prompt();
        app.on_key(key(KeyCode::Enter)); // accept the prompt
        let resumed = app.plan().expect("the job's plan").clone();

        app.on_key(key(KeyCode::Enter)); // start plotting
        assert_eq!(
            app.plan().expect("still the job's plan").strokes[0].start,
            resumed.strokes[0].start,
            "the resumed plan was moved to the head"
        );
    }

    #[test]
    fn h_dismisses_the_resume_prompt_and_homes() {
        let (mut app, sent) = app_with_resume_prompt();
        assert!(app.resume_prompt().is_some(), "prompt should start visible");

        let redraw = app.on_key(press('h'));

        assert!(redraw);
        assert!(app.resume_prompt().is_none(), "h should dismiss the prompt");
        // The worker processes Home asynchronously; give it a moment.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            sent.lock().unwrap().iter().any(|l| l == "$H"),
            "h did not reach homing: {:?}",
            sent.lock().unwrap()
        );
    }

    #[test]
    fn enter_accepts_the_resume_instead_of_dismissing() {
        let (mut app, _sent) = app_with_resume_prompt();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.resume_prompt().is_none(), "enter answers the prompt");
        assert_eq!(app.resume_from(), Some(20 - crate::job::PLANNER_BLOCKS));
    }
}
