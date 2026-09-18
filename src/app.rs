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
use crate::plotter::worker::{Command, Event, MachineState, StopCause, Worker};
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

/// How much `,` / `.` change the pen-down height, mm.
///
/// Fine, because the useful band is narrow: on our machine the lightest height
/// that still draws was 0.2 mm above the one that stopped touching at all
/// (§2.5), and that whole band is what separates a clean line from ink flooding
/// at the point of contact.
pub const PEN_Z_STEP_MM: f32 = 0.05;

/// How far the pen may be driven, mm. `$132` says 10 on our machine; the pen
/// cannot usefully go above the homed top either, so both ends are held.
const PEN_Z_RANGE: std::ops::RangeInclusive<f32> = 0.0..=10.0;

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
    /// The plan is held at a pause, pen up. `enter` means "get going again"
    /// here rather than "start a plot", so the half-drawn sheet is not drawn
    /// over from the top.
    paused: bool,
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
            paused: false,
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
                    self.paused = false;
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
                    self.paused = true;
                    self.ops_done = done;
                    self.activity =
                        Activity::Busy(format!("paused {done}/{total}, pen up (r resume)"));
                }
                Event::PlanDone {
                    elapsed_secs,
                    motors_released,
                } => {
                    self.pausing = false;
                    self.paused = false;
                    if let Some(plan) = &self.plan {
                        self.ops_done = plan.ops.len();
                    }
                    // How long it took is the first thing asked once a plot
                    // ends, and by then the live counter is gone from the
                    // status bar — so the note keeps the number on screen.
                    //
                    // Released motors go in the same note, because they change
                    // what the *next* `enter` means: the drawing takes its
                    // origin from where the head is (§2.4 C), and once the
                    // carriage can be pushed by hand that is only true until
                    // somebody pushes it. `h` re-establishes it.
                    let time = ui::fmt_time(elapsed_secs);
                    self.note = Some(if motors_released {
                        format!("done in {time} - motors released, home (h) before the next plot")
                    } else {
                        format!("done in {time}")
                    });
                }
                Event::Aborted(cause) => {
                    self.pausing = false;
                    self.paused = false;
                    self.note = Some(self.stop_note(cause));
                    self.disarm(cause);
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
        // The missing link when a key "stops working": this says whether it
        // reached the program at all and what it became, which separates a
        // terminal or keyboard problem from a binding one. Debug, because it
        // fires on every arrow press while jogging.
        tracing::debug!(key = ?key.code, ?action, "key");
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
            Action::PenDeeper => self.nudge_pen_depth(PEN_Z_STEP_MM),
            Action::PenShallower => self.nudge_pen_depth(-PEN_Z_STEP_MM),
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
            Action::StartPlot => self.start_or_resume_plot(),
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
                // The prompt says "[Enter] resume", so Enter resumes. Loading
                // the job and then waiting for a *second* enter looked from the
                // operator's chair exactly like a resume that did nothing: the
                // overlay went away, the log said "resuming job", and the
                // machine stood still. The preamble homes first (§6), so this
                // is safe to start from wherever the head happens to be.
                if self.resume_from.is_some() {
                    self.start_plot();
                }
                ResumeReply::Handled
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                tracing::info!("resume declined; starting fresh");
                self.resume = None;
                ResumeReply::Handled
            }
            _ => {
                // Info, not debug: this key is about to act as well (the
                // fall-through in `on_key`), so if it armed a cutoff or moved
                // the head, the log has to say what was pressed.
                tracing::info!(key = ?key.code, "resume dismissed by another key; starting fresh");
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
                self.note = Some(format!("resuming from {}%", resumable.percent()));
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

    /// What `enter` does: resume a held plan, or start the loaded one.
    ///
    /// At a paused machine "draw the loaded SVG" can only mean "carry on" —
    /// starting afresh would redraw the whole sheet over what is already on the
    /// paper, and open a second job for the same drawing. To start over
    /// instead, stop the plan with `S` first.
    fn start_or_resume_plot(&mut self) {
        if self.paused {
            tracing::info!("enter at a paused plan: resuming it");
            self.worker.send(Command::Resume);
            return;
        }
        self.start_plot();
    }

    /// Start drawing the loaded plan, if there is one, arming the safety timer.
    fn start_plot(&mut self) {
        // A fresh plot is laid down from where the head is *now*: jog to the
        // corner of the sheet, press enter, and the drawing starts under the
        // pen. A resume keeps the plan it was interrupted with — re-placing it
        // would tear the drawing in two.
        if self.resume_from.is_none() && !self.shapes.is_empty() {
            self.place_at_head();
            // Off-field first, stale head second: when both apply the stale
            // head is the reason the bounds look wrong, so it is the note worth
            // keeping on screen.
            self.warn_if_off_field();
            self.warn_if_head_is_stale();
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

    /// Say so when the drawing is about to be placed from a head position the
    /// machine can no longer vouch for.
    ///
    /// After the steppers are released the carriage can be pushed by hand, and
    /// nothing on an open-loop machine notices. The plot would still run — from
    /// wherever the head was *last known* to be — so this is a warning, not a
    /// refusal: `h` re-establishes the reference, and if nobody touched the
    /// carriage the old position is still right.
    fn warn_if_head_is_stale(&mut self) {
        if self.machine.position_trusted {
            return;
        }
        tracing::warn!(
            x = self.machine.position.x,
            y = self.machine.position.y,
            "placing the drawing from a head position the machine cannot vouch for"
        );
        self.note = Some("warning: motors were released - press h to home, or draw from the last known head position".to_owned());
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

    /// What to put on screen when a plan ends early. A cutoff names itself and
    /// the key that turns it off: "stopped" alone sends the operator looking at
    /// the machine for something the app did on purpose.
    fn stop_note(&self, cause: StopCause) -> String {
        match cause {
            StopCause::Asked => "stopped".to_owned(),
            StopCause::Timer => match self.stop_timer_minutes() {
                Some(m) => format!("stopped by the {m}m safety timer (t: off)"),
                None => "stopped by the safety timer".to_owned(),
            },
            StopCause::Distance => match self.stop_distance_mm() {
                Some(mm) => format!(
                    "stopped by the {} distance cutoff (m: off)",
                    ui::fmt_dist(mm)
                ),
                None => "stopped by the distance cutoff".to_owned(),
            },
        }
    }

    /// A cutoff that fired is spent. It is re-armed on every `enter`, so left
    /// armed it stops the next attempt at the same place, and the one after
    /// that — which reads as the plotter dying early for no reason. Arming is
    /// one keypress away when the next run should stop too.
    fn disarm(&mut self, cause: StopCause) {
        match cause {
            StopCause::Asked => {}
            StopCause::Timer => self.stop_timer = None,
            StopCause::Distance => self.stop_distance = None,
        }
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

    /// Press the pen harder (positive) or more lightly, and say where it ended
    /// up — the number is only useful if the operator can copy it into
    /// `config.toml` afterwards, so it goes on screen and into the log.
    ///
    /// Held-key repeats are folded into one command, exactly as jogging does.
    /// Without that, a second of auto-repeat is 25 key events, and with the pen
    /// down each of them is a 240 ms Z move (§2.6) — six seconds of backlog per
    /// second of holding, behind which every later key, `space` included, sits
    /// and waits.
    fn nudge_pen_depth(&mut self, delta_mm: f32) {
        let mut steps = delta_mm;
        while matches!(event::poll(Duration::ZERO), Ok(true)) {
            let Ok(event) = event::read() else { break };
            match pen_depth_of(&event) {
                Some(delta) => steps += delta,
                None => {
                    self.pending_events.push_back(event);
                    break;
                }
            }
        }
        let wanted = self.profile.pen.down_z + steps;
        // Clamp rather than refuse: a coalesced burst can overshoot the end of
        // the range, and stopping at the limit is what the operator means.
        let wanted = wanted.clamp(*PEN_Z_RANGE.start(), *PEN_Z_RANGE.end());
        if wanted == self.profile.pen.down_z {
            self.note = Some(format!("pen Z stays at {wanted:.2}"));
            return;
        }
        self.profile.pen.down_z = wanted;
        self.worker.send(Command::SetPenDownZ(wanted));
        self.note = Some(format!("pen_down_z = {wanted:.2}"));
    }

    /// The pen-down height in force, for the status bar.
    pub fn pen_down_z(&self) -> f32 {
        self.profile.pen.down_z
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
/// The pen-depth change an event asks for, or `None` if it is a boundary that
/// must not be folded into the current one. Sibling of [`jog_of`].
fn pen_depth_of(event: &TermEvent) -> Option<f32> {
    let TermEvent::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        // Not a boundary: skip it, as a zero-sized step.
        return Some(0.0);
    }
    match action_for(Mode::Navigation, key) {
        Some(Action::PenDeeper) => Some(PEN_Z_STEP_MM),
        Some(Action::PenShallower) => Some(-PEN_Z_STEP_MM),
        _ => None,
    }
}

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
            position_trusted: true,
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
    /// The band that matters is narrow and the keys are meant to be held, so
    /// the ends of the range have to hold rather than wrap or run off into a Z
    /// the machine cannot reach.
    #[test]
    fn the_pen_depth_stops_at_the_ends_of_its_range() {
        let mut app = app_with_a_drawing();
        for _ in 0..500 {
            app.nudge_pen_depth(PEN_Z_STEP_MM);
        }
        assert!(
            PEN_Z_RANGE.contains(&app.pen_down_z()),
            "ran past the top: {}",
            app.pen_down_z()
        );
        for _ in 0..500 {
            app.nudge_pen_depth(-PEN_Z_STEP_MM);
        }
        assert!(
            PEN_Z_RANGE.contains(&app.pen_down_z()),
            "ran past the bottom: {}",
            app.pen_down_z()
        );
    }

    /// The same app, plus a handle on what actually reaches the wire — the only
    /// way to tell "the key is bound" from "the key does something".
    fn app_and_wire() -> (App, Arc<Mutex<Vec<String>>>) {
        let transport = MockTransport::new();
        let sent = transport.sent_handle();
        let driver = Driver::new(Connection {
            transport: Box::new(transport),
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
        });
        let worker = Worker::spawn(driver);
        let machine = MachineState {
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
            pen: Pen::Up,
            position: Point::new(0.0, 0.0),
            position_trusted: true,
        };
        let app = App::new(
            worker,
            machine,
            test_profile(),
            Vec::new(),
            None,
            None,
            LogRing::new(),
        );
        (app, sent)
    }

    /// Wait for one exact line to reach the wire; the worker runs the command
    /// on its own thread, so the check has to be given time rather than taken
    /// once.
    fn wire_has(sent: &Arc<Mutex<Vec<String>>>, line: &str) -> bool {
        for _ in 0..200 {
            if sent.lock().unwrap().iter().any(|l| l == line) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Wait for the worker thread to catch up with what it was sent.
    fn wire_settles(sent: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(5));
            let lines = sent.lock().unwrap().clone();
            if !lines.is_empty() {
                return lines;
            }
        }
        Vec::new()
    }

    /// Space must reach the machine, not merely resolve to an action. The key
    /// map has a test of its own and it passes; this covers the rest of the
    /// path, which is where a regression would actually sit.
    #[test]
    fn space_toggles_the_pen_all_the_way_to_the_wire() {
        let (mut app, sent) = app_and_wire();
        app.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let lines = wire_settles(&sent);
        assert!(
            lines.iter().any(|l| l.contains(" Z")),
            "space sent no Z move: {lines:?}"
        );
    }

    /// A pen-depth nudge must not swallow the key that ended it.
    #[test]
    fn a_depth_nudge_leaves_the_next_key_alone() {
        let (mut app, sent) = app_and_wire();
        app.on_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        sent.lock().unwrap().clear();
        app.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let lines = wire_settles(&sent);
        assert!(
            lines.iter().any(|l| l.contains(" Z")),
            "space after a nudge sent no Z move: {lines:?}"
        );
    }

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
            position_trusted: true,
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
    /// A cutoff is re-armed on every `enter`, so one that has fired must let
    /// go — otherwise the next run stops at the same distance, and the one
    /// after that, which on paper looks like the plotter dying early. The note
    /// has to name it too: "stopped" alone sends the operator to the machine
    /// looking for a fault the app caused on purpose.
    #[test]
    fn a_fired_distance_cutoff_names_itself_and_disarms() {
        let mut app = app_with_a_drawing();
        app.on_key(press('m'));
        assert_eq!(app.stop_distance_mm(), Some(STOP_DISTANCE_MM[0]));

        let note = app.stop_note(StopCause::Distance);
        app.disarm(StopCause::Distance);

        assert!(
            note.contains("distance cutoff") && note.contains('m'),
            "the note does not say what stopped the plot: {note}"
        );
        assert_eq!(app.stop_distance_mm(), None, "the cutoff stayed armed");
    }

    /// The timer is the same bargain, and a stop asked for by hand is not: it
    /// says nothing about whether the cutoffs are still wanted.
    #[test]
    fn only_the_cutoff_that_fired_is_disarmed() {
        let mut app = app_with_a_drawing();
        app.on_key(press('t'));
        app.on_key(press('m'));

        app.disarm(StopCause::Asked);
        assert!(
            app.stop_timer_minutes().is_some(),
            "a manual stop disarmed the timer"
        );
        assert!(
            app.stop_distance_mm().is_some(),
            "a manual stop disarmed the cutoff"
        );

        app.disarm(StopCause::Timer);
        assert!(app.stop_timer_minutes().is_none(), "the timer stayed armed");
        assert!(
            app.stop_distance_mm().is_some(),
            "the timer took the cutoff with it"
        );
    }

    /// The key that dismisses the resume prompt is then handled normally
    /// (§3.3), which also means `m` there arms a cutoff. That is how a plot
    /// came to stop after 50 cm with nobody having asked for it, so the
    /// behaviour is pinned down rather than left to be rediscovered.
    #[test]
    fn a_key_dismissing_the_resume_prompt_still_acts() {
        let (mut app, _sent) = app_with_resume_prompt();

        app.on_key(press('m'));

        assert!(app.resume_prompt().is_none(), "the prompt survived the key");
        assert_eq!(
            app.stop_distance_mm(),
            Some(STOP_DISTANCE_MM[0]),
            "the dismissing key never reached the normal handler"
        );
    }

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

    /// The prompt says "[Enter] resume", so enter has to resume — all the way
    /// to the machine. It used to only load the job and leave a note asking for
    /// a *second* enter: the overlay vanished, the log said "resuming job", and
    /// the plotter stood still, which reads as a resume that is broken.
    #[test]
    fn enter_on_the_resume_prompt_resumes_all_the_way_to_the_wire() {
        let (mut app, sent) = app_with_resume_prompt();

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.resume_prompt().is_none(), "enter answers the prompt");
        // The preamble homes before it travels to the stop point (§6), so `$H`
        // is the proof that a plan went out rather than just being loaded.
        assert!(
            wire_has(&sent, "$H"),
            "enter left the machine idle: {:?}",
            sent.lock().unwrap()
        );
    }

    /// The resume starts from the committed index, which lags the ops sent by
    /// the planner depth (§6) — the checkpoint above wrote 20 sent.
    #[test]
    fn the_resume_starts_from_the_committed_index() {
        let (mut app, _sent) = app_with_resume_prompt();
        let expected = 20 - crate::job::PLANNER_BLOCKS;

        app.accept_resume();

        assert_eq!(app.resume_from(), Some(expected));
    }

    /// The operator's exact sequence: start a plot, press esc to pause, then
    /// press `r` to resume. The plan has to go on reaching the wire.
    #[test]
    fn esc_then_r_pauses_and_resumes_the_plot() {
        let transport = MockTransport::with_read_delay(Duration::from_millis(1));
        let sent = transport.sent_handle();
        let driver = Driver::new(Connection {
            transport: Box::new(transport),
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
        });
        let worker = Worker::spawn(driver);
        let machine = MachineState {
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
            pen: Pen::Up,
            position: Point::new(0.0, 0.0),
            position_trusted: true,
        };
        // Many short strokes: plenty of pen-up boundaries for a pause to land on.
        let shapes: Vec<Shape> = (0..60)
            .map(|i| {
                let y = f64::from(i);
                Shape::unlabelled(vec![Point::new(0.0, y), Point::new(10.0, y)])
            })
            .collect();
        let mut app = App::new(
            worker,
            machine,
            test_profile(),
            shapes,
            None,
            None,
            LogRing::new(),
        );

        app.on_key(key(KeyCode::Enter)); // start the plot
        std::thread::sleep(Duration::from_millis(100));
        app.on_key(key(KeyCode::Esc)); // pause

        // Spin the app's own event folding until the hold engages.
        let mut paused = false;
        for _ in 0..400 {
            app.drain_worker_events();
            if matches!(&app.activity, Activity::Busy(label) if label.starts_with("paused")) {
                paused = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(paused, "esc never paused the plot: {:?}", app.activity);
        let at_pause = sent.lock().unwrap().len();

        app.on_key(press('r')); // resume
        std::thread::sleep(Duration::from_millis(300));
        app.drain_worker_events();
        let after_resume = sent.lock().unwrap().len();

        assert!(
            after_resume > at_pause,
            "`r` did not resume: the wire stayed at {at_pause} lines, activity {:?}",
            app.activity
        );
    }

    /// §2.4 C: the head's position when `enter` is pressed is the drawing's
    /// origin. A plot that has just finished must not move that goalpost — the
    /// next drawing starts where the pen actually stands, not at the bed's
    /// corner.
    #[test]
    fn a_finished_plot_leaves_the_next_one_starting_at_the_head() {
        let (mut app, _sent) = app_and_wire();
        app.shapes = vec![Shape::unlabelled(vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
        ])];
        // The head is parked somewhere in the middle of the sheet, as it would
        // be after jogging to the corner of the paper.
        app.machine.position = Point::new(120.0, 250.0);

        app.on_key(key(KeyCode::Enter)); // draw it
        for _ in 0..400 {
            app.drain_worker_events();
            if app
                .note
                .as_deref()
                .is_some_and(|n| n.starts_with("done in"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            app.note
                .as_deref()
                .is_some_and(|n| n.starts_with("done in")),
            "the plot never finished: {:?}",
            app.note
        );

        let head_after = app.machine.position;
        app.on_key(key(KeyCode::Enter)); // and again, from where the pen is
        let placed = app.plan().and_then(Plan::drawn_bounds).expect("a plan").0;

        assert!(
            close(placed, head_after),
            "the second plot ignored the head: drawing starts at {placed:?}, \
             head is at {head_after:?}"
        );
    }

    /// An app mid-plot, held at a pause with the pen up.
    fn paused_mid_plot() -> (App, Arc<Mutex<Vec<String>>>) {
        let transport = MockTransport::with_read_delay(Duration::from_millis(1));
        let sent = transport.sent_handle();
        let driver = Driver::new(Connection {
            transport: Box::new(transport),
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
        });
        let worker = Worker::spawn(driver);
        let machine = MachineState {
            version: "DrawCore V2.10".to_owned(),
            port: "mock".to_owned(),
            pen: Pen::Up,
            position: Point::new(0.0, 0.0),
            position_trusted: true,
        };
        let shapes: Vec<Shape> = (0..60)
            .map(|i| {
                let y = f64::from(i);
                Shape::unlabelled(vec![Point::new(0.0, y), Point::new(10.0, y)])
            })
            .collect();
        let mut app = App::new(
            worker,
            machine,
            test_profile(),
            shapes,
            None,
            None,
            LogRing::new(),
        );
        app.on_key(key(KeyCode::Enter));
        std::thread::sleep(Duration::from_millis(100));
        app.on_key(key(KeyCode::Esc));
        for _ in 0..400 {
            app.drain_worker_events();
            if app.paused {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.paused, "the plot never paused: {:?}", app.activity);
        (app, sent)
    }

    /// `enter` at a paused machine carries on. It used to open a *second* job
    /// and send a `RunPlan` that the held worker dropped on the floor: the log
    /// said "starting plot" three times over and the carriage never moved.
    #[test]
    fn enter_at_a_paused_plan_resumes_it_instead_of_starting_a_second_job() {
        let (mut app, sent) = paused_mid_plot();
        let job_before = app.job.as_ref().map(|j| j.id);
        let at_pause = sent.lock().unwrap().len();

        app.on_key(key(KeyCode::Enter));

        std::thread::sleep(Duration::from_millis(300));
        app.drain_worker_events();
        assert!(
            sent.lock().unwrap().len() > at_pause,
            "enter left the machine idle at {at_pause} lines"
        );
        assert_eq!(
            app.job.as_ref().map(|j| j.id),
            job_before,
            "enter opened a second job for the drawing already in progress"
        );
        assert!(!app.paused, "the app still thinks the plan is held");
    }

    /// A pause is when the operator reaches for the pen keys, so they have to
    /// work. Held commands used to vanish in the worker's catch-all arm, which
    /// reads as a dead keyboard.
    #[test]
    fn the_pen_keys_still_work_while_the_plan_is_paused() {
        let (mut app, sent) = paused_mid_plot();
        let at_pause = sent.lock().unwrap().len();

        app.on_key(press(']')); // pen down

        for _ in 0..200 {
            app.drain_worker_events();
            if sent.lock().unwrap().len() > at_pause {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let after = sent.lock().unwrap().clone();
        assert!(
            after.len() > at_pause,
            "the pen key did nothing while paused: {after:?}"
        );
        assert!(app.paused, "the pen key ended the pause");
    }

    /// Releasing the steppers costs the origin: the carriage can be pushed by
    /// hand and an open-loop machine never notices. The next `enter` still
    /// draws — from the last known head — but it has to say so, or the drawing
    /// lands somewhere nobody chose (§2.4 C).
    #[test]
    fn enter_warns_when_the_head_is_no_longer_vouched_for() {
        let mut app = app_with_a_drawing();
        app.machine.position_trusted = false;

        app.on_key(key(KeyCode::Enter));

        let note = app.note.clone().unwrap_or_default();
        assert!(
            note.contains("motors were released") && note.contains('h'),
            "enter placed the drawing from a stale head without a word: {note:?}"
        );
    }

    /// ...and says nothing when the machine can still vouch for the head, so
    /// the warning keeps its meaning.
    #[test]
    fn enter_is_quiet_when_the_head_is_known() {
        let mut app = app_with_a_drawing();
        assert!(app.machine.position_trusted);

        app.on_key(key(KeyCode::Enter));

        let note = app.note.clone().unwrap_or_default();
        assert!(
            !note.contains("motors were released"),
            "warned about a head the machine knows: {note:?}"
        );
    }
}
