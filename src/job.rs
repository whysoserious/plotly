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
const PROGRESS_FILE: &str = "progress.json";

/// Grbl planner buffer depth (`[OPT:…,15,…]`, §15.1). An `ok` means "queued",
/// not "drawn", so up to this many trailing ops may be lost on a hard kill.
pub const PLANNER_BLOCKS: usize = 15;

/// The op index that is safe to treat as drawn, given `sent` ops acknowledged
/// and a planner `buffer_depth` (0 = variant A, we know it finished;
/// [`PLANNER_BLOCKS`] = variant B, `ok` only meant queued — §6).
pub fn committed_index(sent: usize, buffer_depth: usize) -> usize {
    sent.saturating_sub(buffer_depth)
}

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

    /// Path of the progress file.
    pub fn progress_path(&self) -> PathBuf {
        self.dir.join(PROGRESS_FILE)
    }

    /// A sink the worker uses to checkpoint progress as it draws.
    pub fn progress_writer(&self) -> ProgressWriter {
        ProgressWriter {
            path: self.progress_path(),
            job_id: self.id,
        }
    }
}

/// Resume checkpoint, rewritten atomically as the print advances (§6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub job_id: u64,
    /// Ops safe to treat as drawn — the next op to send on resume.
    pub committed_index: usize,
    /// Total ops in the plan (the resume denominator).
    pub total: usize,
    /// Last commanded position, machine-logical mm (not `MPos`; §6).
    pub pos: [f64; 2],
    /// Whether the pen was down at the checkpoint.
    pub pen_down: bool,
    pub updated_at_ms: u64,
}

impl Progress {
    /// Whether this job still has ops left to draw.
    pub fn is_unfinished(&self) -> bool {
        self.committed_index < self.total
    }

    /// Percent complete, for the resume prompt.
    pub fn percent(&self) -> u8 {
        match self.total {
            0 => 100,
            total => (self.committed_index * 100 / total) as u8,
        }
    }
}

/// Writes [`Progress`] to a job directory. Held by the worker while drawing so
/// it can checkpoint without knowing about the [`Job`] type.
#[derive(Debug, Clone)]
pub struct ProgressWriter {
    path: PathBuf,
    job_id: u64,
}

impl ProgressWriter {
    /// Atomically record a checkpoint: `sent` ops acknowledged out of `total`,
    /// at commanded position `pos`, pen `pen_down`. The committed index lags
    /// `sent` by the planner depth so a hard kill loses no drawn geometry (§6).
    pub fn checkpoint(
        &self,
        sent: usize,
        total: usize,
        pos: [f64; 2],
        pen_down: bool,
    ) -> io::Result<()> {
        let progress = Progress {
            job_id: self.job_id,
            committed_index: committed_index(sent, PLANNER_BLOCKS),
            total,
            pos,
            pen_down,
            updated_at_ms: now_ms(),
        };
        write_atomic(&self.path, &progress)
    }

    /// Record the plan as fully drawn (`committed_index == total`).
    pub fn finish(&self, total: usize, pos: [f64; 2]) -> io::Result<()> {
        let progress = Progress {
            job_id: self.job_id,
            committed_index: total,
            total,
            pos,
            pen_down: false,
            updated_at_ms: now_ms(),
        };
        write_atomic(&self.path, &progress)
    }
}

/// Read a job's progress checkpoint.
pub fn read_progress(dir: &Path) -> io::Result<Progress> {
    let text = fs::read_to_string(dir.join(PROGRESS_FILE))?;
    serde_json::from_str(&text).map_err(invalid_data)
}

/// Write JSON to `path` atomically: to a sibling temp file, then rename over
/// the target. A crash mid-write leaves either the old file or the new, never
/// a half-written one (rename is atomic on a POSIX filesystem).
fn write_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    write_json(&tmp, value)?;
    fs::rename(&tmp, path)
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
    fn committed_index_lags_by_the_buffer_depth() {
        // Variant A: we know the move finished, so nothing lags.
        assert_eq!(committed_index(100, 0), 100);
        // Variant B: `ok` only meant queued, so trail by the planner depth.
        assert_eq!(committed_index(100, PLANNER_BLOCKS), 85);
        // Early on, fewer ops than the buffer → clamp to zero, never negative.
        assert_eq!(committed_index(10, PLANNER_BLOCKS), 0);
    }

    #[test]
    fn a_checkpoint_is_written_atomically_and_reads_back() {
        let tmp = TempDir::new("progress");
        let job = Job::create(&tmp.0, &sample_plan(), None).unwrap();
        let writer = job.progress_writer();

        writer.checkpoint(50, 100, [12.5, -3.0], true).unwrap();

        let progress = read_progress(&job.dir).unwrap();
        assert_eq!(progress.committed_index, 50 - PLANNER_BLOCKS);
        assert_eq!(progress.total, 100);
        assert_eq!(progress.pos, [12.5, -3.0]);
        assert!(progress.pen_down);
        assert!(progress.is_unfinished());

        // No temp file is left behind by the rename.
        assert!(!job.dir.join("progress.json.tmp").exists());
    }

    #[test]
    fn checkpoints_overwrite_and_finish_marks_complete() {
        let tmp = TempDir::new("finish");
        let job = Job::create(&tmp.0, &sample_plan(), None).unwrap();
        let writer = job.progress_writer();

        writer.checkpoint(30, 100, [1.0, 2.0], false).unwrap();
        writer.finish(100, [0.0, 0.0]).unwrap();

        let progress = read_progress(&job.dir).unwrap();
        assert_eq!(progress.committed_index, 100);
        assert!(!progress.is_unfinished());
        assert_eq!(progress.percent(), 100);
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
