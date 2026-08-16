//! Integration test for step 3.2: the worker checkpoints progress.json as it
//! draws, and marks the job complete when the plan finishes.
//!
//! Also the round trip that matters most in practice (step 2.7 + 3.4): pause a
//! print, leave, come back, and finish it — with exactly the same ink on the
//! paper as an uninterrupted run.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plotly::geometry::{Placement, Point};
use plotly::job::{self, Job};
use plotly::plan::{Plan, PlanSettings};
use plotly::plotter::driver::Driver;
use plotly::plotter::mock::MockTransport;
use plotly::plotter::worker::{Command, Event, Worker};
use plotly::plotter::Connection;

const TIMEOUT: Duration = Duration::from_secs(10);

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("plotly-it-{tag}-{ms}"));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn worker_on_slow_mock() -> Worker {
    traced_worker().0
}

/// A worker over a slow mock, plus a handle to every line it puts on the wire.
fn traced_worker() -> (Worker, Arc<Mutex<Vec<String>>>) {
    let mock = MockTransport::with_read_delay(Duration::from_millis(2));
    let sent = mock.sent_handle();
    let driver = Driver::new(Connection {
        transport: Box::new(mock),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    let worker = Worker::spawn(driver);
    worker.recv_timeout(TIMEOUT); // drop the initial State
    (worker, sent)
}

/// The XY moves a run made with the pen on the paper, in order — the ink it
/// actually laid down, as opposed to the travel between shapes.
fn drawn_lines(sent: &[String]) -> Vec<String> {
    let mut pen_down = false;
    let mut drawn = Vec::new();
    for line in sent {
        if line.contains("Z5.000") {
            pen_down = true;
        } else if line.contains("Z0.500") {
            pen_down = false;
        } else if pen_down && line.starts_with("G1 X") {
            drawn.push(line.clone());
        }
    }
    drawn
}

/// Run `plan` start to finish and return the ink it draws.
fn ink_of_an_uninterrupted_run(plan: &Plan) -> Vec<String> {
    let (mut worker, sent) = traced_worker();
    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: None,
        start_index: 0,
    });
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::PlanDone) {
            break;
        }
    }
    worker.shutdown();
    let lines = drawn_lines(&sent.lock().unwrap());
    assert!(!lines.is_empty(), "the reference run drew nothing");
    lines
}

fn plan() -> Plan {
    let line = vec![Point::new(0.0, 0.0), Point::new(200.0, 0.0)];
    Plan::build(&[line], &Placement::identity(), &PlanSettings::default())
}

/// Two long strokes, so a pause can land inside the first and still leave a
/// whole shape to draw afterwards.
fn two_shape_plan() -> Plan {
    let a = vec![Point::new(0.0, 0.0), Point::new(200.0, 0.0)];
    let b = vec![Point::new(0.0, 50.0), Point::new(200.0, 50.0)];
    Plan::build(&[a, b], &Placement::identity(), &PlanSettings::default())
}

#[test]
fn a_finished_plan_leaves_a_complete_progress_file() {
    let tmp = TempDir::new("finish");
    let plan = plan();
    let job = Job::create(&tmp.0, &plan, Some("line.svg")).unwrap();

    let mut worker = worker_on_slow_mock();
    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: Some(job.progress_writer()),
        start_index: 0,
    });

    // Wait for completion.
    let mut done = false;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::PlanDone) {
            done = true;
            break;
        }
    }
    assert!(done, "plan never finished");
    worker.shutdown();

    let progress = job::read_progress(&job.dir).expect("progress.json exists");
    assert_eq!(progress.committed_index, plan.ops.len());
    assert!(!progress.is_unfinished());
}

/// A plan paused at a shape boundary records *exactly* where it stopped.
///
/// Everywhere else the committed index lags the acknowledged one by the planner
/// depth, because `ok` only means "queued" (§15.1). A pause drains the buffer
/// first, so it can commit the truth — and a resume then picks up the next
/// shape instead of redrawing the tail of the last one.
#[test]
fn a_paused_plan_commits_the_exact_op_it_reached() {
    let tmp = TempDir::new("pause");
    let plan = two_shape_plan();
    let job = Job::create(&tmp.0, &plan, Some("two.svg")).unwrap();

    let mut worker = worker_on_slow_mock();
    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: Some(job.progress_writer()),
        start_index: 0,
    });

    // Pause once the pen is on the paper, and note where the worker held.
    let mut paused_at = None;
    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        match event {
            Event::Progress { done, .. } => {
                if paused_at.is_none() && plan.stroke_progress(done).current == Some(0) {
                    worker.send(Command::Pause);
                }
            }
            Event::Paused { done, .. } => {
                paused_at = Some(done);
                break;
            }
            _ => {}
        }
    }
    let paused_at = paused_at.expect("the plan never paused");
    worker.shutdown();

    let progress = job::read_progress(&job.dir).expect("progress.json exists");
    assert_eq!(
        progress.committed_index, paused_at,
        "a paused job must commit exactly what it drew"
    );
    assert!(progress.is_unfinished(), "there is still a shape to draw");
    assert!(!progress.pen_down, "the pen must be up at the hold");

    // The resume point is a clean shape boundary: nothing is half-drawn, so
    // restarting from it redraws nothing.
    assert_eq!(
        plan.stroke_progress(progress.committed_index).current,
        None,
        "the committed index falls inside a shape"
    );
}

/// The whole point of the pause: stop a print, quit, come back later and finish
/// it — with the same ink on the paper as if it had never stopped.
///
/// Two separate workers over two separate mocks stand in for the two runs of
/// the program; the job directory is the only thing they share, exactly as it
/// would be after a restart.
#[test]
fn a_paused_job_resumes_in_a_later_run_and_draws_the_same_ink() {
    let tmp = TempDir::new("resume");
    let plan = two_shape_plan();
    let expected_ink = ink_of_an_uninterrupted_run(&plan);
    let job = Job::create(&tmp.0, &plan, Some("two.svg")).unwrap();

    // First run: draw, pause inside the first shape, quit.
    let (mut first, first_sent) = traced_worker();
    first.send(Command::RunPlan {
        plan: plan.clone(),
        progress: Some(job.progress_writer()),
        start_index: 0,
    });
    let mut asked = false;
    while let Some(event) = first.recv_timeout(TIMEOUT) {
        match event {
            Event::Progress { done, .. } if !asked => {
                if plan.stroke_progress(done).current == Some(0) {
                    first.send(Command::Pause);
                    asked = true;
                }
            }
            Event::Paused { .. } => break,
            _ => {}
        }
    }
    assert!(asked, "the first run never got inside a shape");
    first.shutdown();
    let first_ink = drawn_lines(&first_sent.lock().unwrap());

    // Second run: the job is offered for resume, at the shape boundary.
    let resumable = job::scan(&tmp.0)
        .into_iter()
        .next()
        .expect("no resumable job");
    let from = resumable.progress.committed_index;
    assert!(from > 0 && from < plan.ops.len(), "resuming from {from}");
    let reloaded = job::read_job_plan(&resumable.dir).expect("the plan reads back");

    let (mut second, second_sent) = traced_worker();
    second.send(Command::RunPlan {
        plan: reloaded,
        progress: Some(job.progress_writer()),
        start_index: from,
    });
    let mut finished = false;
    while let Some(event) = second.recv_timeout(TIMEOUT) {
        if matches!(event, Event::PlanDone) {
            finished = true;
            break;
        }
    }
    assert!(finished, "the resumed run never finished");
    second.shutdown();
    let second_ink = drawn_lines(&second_sent.lock().unwrap());

    // Neither run drew nothing, and between them they drew the whole drawing
    // once: no gap where the pause fell, no shape traced twice.
    assert!(!first_ink.is_empty(), "the first run drew nothing");
    assert!(!second_ink.is_empty(), "the resumed run drew nothing");
    assert_eq!(
        [first_ink, second_ink].concat(),
        expected_ink,
        "pausing and resuming changed what ends up on the paper"
    );

    let progress = job::read_progress(&job.dir).expect("progress.json exists");
    assert!(!progress.is_unfinished(), "the job should be complete now");
}

#[test]
fn a_stopped_plan_leaves_an_unfinished_checkpoint_to_resume_from() {
    let tmp = TempDir::new("stop");
    let plan = plan();
    let total = plan.ops.len();
    let job = Job::create(&tmp.0, &plan, None).unwrap();

    let mut worker = worker_on_slow_mock();
    worker.send(Command::RunPlan {
        plan: plan.clone(),
        progress: Some(job.progress_writer()),
        start_index: 0,
    });
    std::thread::sleep(Duration::from_millis(20));
    worker.send(Command::Stop);

    while let Some(event) = worker.recv_timeout(TIMEOUT) {
        if matches!(event, Event::Aborted) {
            break;
        }
    }
    worker.shutdown();

    let progress = job::read_progress(&job.dir).expect("progress.json exists");
    assert!(progress.is_unfinished(), "should have ops left to resume");
    assert!(
        progress.committed_index < total,
        "committed {} of {total}",
        progress.committed_index
    );
}
