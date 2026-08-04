//! Integration test for step 3.2: the worker checkpoints progress.json as it
//! draws, and marks the job complete when the plan finishes.

use std::fs;
use std::path::PathBuf;
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
    let mock = MockTransport::with_read_delay(Duration::from_millis(2));
    let driver = Driver::new(Connection {
        transport: Box::new(mock),
        version: "DrawCore V2.10".to_owned(),
        port: "mock".to_owned(),
    });
    let worker = Worker::spawn(driver);
    worker.recv_timeout(TIMEOUT); // drop the initial State
    worker
}

fn plan() -> Plan {
    let line = vec![Point::new(0.0, 0.0), Point::new(200.0, 0.0)];
    Plan::build(&[line], &Placement::identity(), &PlanSettings::default())
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
