//! Persistent job directory: a plan and its metadata on disk, so a print can be
//! resumed after a crash or a power cut. DESIGN.org §6.
//!
//! Layout under the per-OS data-local directory (`state_dir` is Linux-only, so
//! `data_local_dir` is used instead, §6):
//!
//! ```text
//! <data_local>/plotly/jobs/<job_id>/
//!   plan.jsonl     one Op per line, absolute mm — the source of truth
//!   meta.json      source file, op count, timestamps
//!   progress.json  committed_index + position (step 3.2)
//! ```

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::plan::{Op, Plan};

const PLAN_FILE: &str = "plan.jsonl";
const META_FILE: &str = "meta.json";

/// Base directory holding every job folder, or `None` if the platform exposes
/// no home directory.
pub fn jobs_root() -> Option<PathBuf> {
    directories::ProjectDirs::from("me", "ziniewicz", "plotly")
        .map(|dirs| dirs.data_local_dir().join("jobs"))
}

/// Metadata written once when a job starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub job_id: u64,
    /// Source SVG path, if the plan came from a file.
    pub source: Option<String>,
    /// Total ops in the plan (the resume denominator).
    pub ops: usize,
    /// Creation time, Unix milliseconds.
    pub created_at_ms: u64,
}

/// A job directory on disk.
pub struct Job {
    pub id: u64,
    pub dir: PathBuf,
}

impl Job {
    /// Create a fresh job under `root`: make the directory and write the plan
    /// and metadata. The id is the creation time in Unix milliseconds.
    pub fn create(root: &Path, plan: &Plan, source: Option<&str>) -> io::Result<Self> {
        let id = now_ms();
        let dir = root.join(id.to_string());
        fs::create_dir_all(&dir)?;

        write_plan(&dir.join(PLAN_FILE), plan)?;
        let meta = Meta {
            job_id: id,
            source: source.map(str::to_owned),
            ops: plan.ops.len(),
            created_at_ms: id,
        };
        write_json(&dir.join(META_FILE), &meta)?;

        tracing::info!(job = id, dir = %dir.display(), ops = plan.ops.len(), "job created");
        Ok(Self { id, dir })
    }

    /// Path of the progress file (written in step 3.2).
    pub fn progress_path(&self) -> PathBuf {
        self.dir.join("progress.json")
    }
}

/// Write a plan as JSON Lines: one [`Op`] per line, in order.
pub fn write_plan(path: &Path, plan: &Plan) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    for op in &plan.ops {
        writeln!(file, "{}", serde_json::to_string(op).map_err(invalid_data)?)?;
    }
    file.flush()
}

/// Read a plan back from a JSON Lines file. Blank lines are ignored.
pub fn read_plan(path: &Path) -> io::Result<Plan> {
    let file = fs::File::open(path)?;
    let mut ops = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        ops.push(serde_json::from_str::<Op>(&line).map_err(invalid_data)?);
    }
    Ok(Plan { ops })
}

/// Read a job's metadata.
pub fn read_meta(dir: &Path) -> io::Result<Meta> {
    let text = fs::read_to_string(dir.join(META_FILE))?;
    serde_json::from_str(&text).map_err(invalid_data)
}

/// Serialize `value` to a pretty JSON file.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(invalid_data)?,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn invalid_data(err: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Placement, Point};
    use crate::plan::PlanSettings;

    /// A unique scratch directory under the OS temp dir, removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("plotly-test-{tag}-{}", now_ms()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_plan() -> Plan {
        let square = vec![
            Point::new(0.0, 0.0),
            Point::new(30.0, 0.0),
            Point::new(30.0, 20.0),
            Point::new(0.0, 0.0),
        ];
        Plan::build(&[square], &Placement::identity(), &PlanSettings::default())
    }

    #[test]
    fn jobs_root_is_available_and_ends_in_jobs() {
        let root = jobs_root().expect("a home directory on this platform");
        assert!(root.ends_with("jobs"), "unexpected root: {root:?}");
    }

    #[test]
    fn plan_round_trips_through_jsonl() {
        let tmp = TempDir::new("roundtrip");
        let path = tmp.0.join("plan.jsonl");
        let plan = sample_plan();

        write_plan(&path, &plan).unwrap();
        let back = read_plan(&path).unwrap();
        assert_eq!(back, plan);

        // One JSON object per op, no blank lines.
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), plan.ops.len());
    }

    #[test]
    fn create_writes_the_plan_and_meta() {
        let tmp = TempDir::new("create");
        let plan = sample_plan();
        let job = Job::create(&tmp.0, &plan, Some("logo.svg")).unwrap();

        assert!(job.dir.join(PLAN_FILE).exists());
        assert!(job.dir.join(META_FILE).exists());

        let meta = read_meta(&job.dir).unwrap();
        assert_eq!(meta.job_id, job.id);
        assert_eq!(meta.ops, plan.ops.len());
        assert_eq!(meta.source.as_deref(), Some("logo.svg"));

        assert_eq!(read_plan(&job.dir.join(PLAN_FILE)).unwrap(), plan);
    }
}
