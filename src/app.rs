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
use crate::plan::Plan;
use crate::plotter::worker::{Command, Event, MachineState, Worker};
use crate::ui;

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
    /// The plan built from the loaded SVG, if any; `Enter` draws it.
    plan: Option<Plan>,
    /// Cached plan estimate: total travel (mm) and time (s), for the ETA.
    estimate: Option<(f64, f64)>,
    /// Source SVG path, recorded in a job's metadata (§6).
    source: Option<String>,
    /// The job directory for the current print, created when it starts (§3.1).
    job: Option<Job>,
    /// An unfinished job found at startup, prompting to resume (§3.3). While
    /// `Some`, the resume overlay is shown and captures the next key.
    resume: Option<Resumable>,
    /// Op index to resume drawing from, set when a resume is accepted (§3.4).
    resume_from: Option<usize>,
    /// Raw G-code console (step 1.5): `Some` while open, holding the typed line.
    console: Option<String>,
    /// Whether the key overview is covering the screen.
    help: bool,
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
        plan: Option<Plan>,
        source: Option<String>,
        resume: Option<Resumable>,
        log: LogRing,
    ) -> Self {
        let estimate = plan
            .as_ref()
            .map(|p| (p.total_distance_mm(), p.estimated_secs()));
        Self {
            worker,
            machine,
            activity: Activity::Idle,
            note: None,
            plan,
            estimate,
            source,
            job: None,
            resume,
            resume_from: None,
            console: None,
            help: false,
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
                Event::Busy(label) => self.activity = Activity::Busy(label),
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
                    self.activity = Activity::Drawing {
                        done,
                        total,
                        distance_mm,
                        elapsed_secs,
                    }
                }
                Event::Paused { done, total } => {
                    self.activity = Activity::Busy(format!("paused {done}/{total} (r resume)"));
                }
                Event::PlanDone => self.note = Some("done".to_owned()),
                Event::Aborted => self.note = Some("stopped".to_owned()),
                Event::Error(err) => self.note = Some(format!("error: {err}")),
            }
        }
        changed
    }

    /// Handle one key press; returns whether the screen has to be redrawn.
    fn on_key(&mut self, key: KeyEvent) -> bool {
        // The resume prompt owns the keyboard until it is answered (§3.3).
        if self.resume.is_some() {
            return self.answer_resume(key);
        }
        let Some(action) = action_for(self.mode(), &key) else {
            return false;
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
            Action::CycleStopTimer => self.cycle_stop_timer(),
            Action::CycleStopDistance => self.cycle_stop_distance(),
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

    /// Answer the startup resume prompt: Enter resumes, `n`/Esc starts fresh.
    /// Any other key is ignored so the choice is deliberate.
    fn answer_resume(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match key.code {
            KeyCode::Enter => {
                self.accept_resume();
                true
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                tracing::info!("resume declined; starting fresh");
                self.resume = None;
                true
            }
            _ => false,
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
                self.estimate = Some((plan.total_distance_mm(), plan.estimated_secs()));
                self.plan = Some(plan);
                self.source = resumable.meta.source.clone();
                self.job = Some(Job {
                    id: resumable.meta.job_id,
                    dir: resumable.dir.clone(),
                });
                self.resume_from = Some(committed);
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
        let Some(plan) = &self.plan else {
            self.note = Some("no SVG loaded".to_owned());
            return;
        };
        tracing::info!(ops = plan.ops.len(), "starting plot");

        // Persist the job so the print can be resumed after a crash (§3.1),
        // and hand the worker a checkpoint writer for progress.json (§3.2).
        self.job = self.create_job(plan);
        let progress = self.job.as_ref().map(Job::progress_writer);

        self.worker.send(Command::RunPlan {
            plan: plan.clone(),
            progress,
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

    /// Cached plan estimate `(total_distance_mm, estimated_secs)`, for the ETA.
    pub fn estimate(&self) -> Option<(f64, f64)> {
        self.estimate
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

    /// The loaded plan, if any, for the toolpath preview.
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    pub fn activity(&self) -> &Activity {
        &self.activity
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
