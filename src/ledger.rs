//! The per-job cost ledger: an append-only JSONL store of job
//! records under the broker's data directory.
//!
//! Lane hygiene is structural: a [`JobRecord`] has no field that
//! could hold prompt text or output content. The ledger records
//! metadata only — job id, route, lane, kind, units, cost,
//! status, timestamps, exit code, and a short status detail.
//!
//! Every state transition appends the job's full record as one
//! JSON line; the current state of a job is its last line. The
//! fold on load is therefore the whole recovery story, and a
//! crash leaves at most a torn final line, which load skips and
//! reports as a warning count.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::decide::{JobKind, Lane};

/// The ledger file's name inside the data directory.
pub const LEDGER_FILE_NAME: &str = "ledger.jsonl";

/// A job's lifecycle state. Terminal states are `Completed`,
/// `Failed`, `Cancelled`, and `Denied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Denied,
}

impl JobStatus {
    pub fn parse(text: &str) -> Option<JobStatus> {
        match text {
            "queued" => Some(JobStatus::Queued),
            "running" => Some(JobStatus::Running),
            "completed" => Some(JobStatus::Completed),
            "failed" => Some(JobStatus::Failed),
            "cancelled" => Some(JobStatus::Cancelled),
            "denied" => Some(JobStatus::Denied),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Denied => "denied",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled | JobStatus::Denied
        )
    }
}

/// One job as the ledger knows it. This is also the wire shape
/// the job tools return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub job_id: String,
    pub route: String,
    pub lane: Lane,
    pub kind: JobKind,
    pub units: u32,
    /// The route's unit cost at submit time (a later config edit
    /// never rewrites a recorded job's accounting).
    pub unit_cost: f64,
    /// `units * unit_cost` for allowed jobs; exactly 0 for denied
    /// jobs (a denied job consumes nothing).
    pub cost: f64,
    pub status: JobStatus,
    /// Unix epoch seconds.
    pub submitted_at: u64,
    /// Unix epoch seconds of the last transition.
    pub updated_at: u64,
    /// The executor's exit code, when a process ran and exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Short status detail (deny reason, failure cause). Metadata
    /// only — never job content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl JobRecord {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// The fold of the ledger: latest record per job, in submission
/// order.
#[derive(Debug, Default)]
pub struct LedgerState {
    pub jobs: Vec<JobRecord>,
    /// Ledger lines that failed to parse on load (a torn final
    /// line after a crash). Reported, never silently dropped.
    pub torn_lines: usize,
}

/// The append-only ledger file.
pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    /// Open (creating if needed) the ledger in `data_dir` and fold
    /// its current state.
    pub fn open(data_dir: &Path) -> Result<(Ledger, LedgerState), LedgerError> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| LedgerError::Io(format!("create {}: {e}", data_dir.display())))?;
        let path = data_dir.join(LEDGER_FILE_NAME);
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => fold(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => LedgerState::default(),
            Err(e) => {
                return Err(LedgerError::Io(format!("read {}: {e}", path.display())));
            }
        };
        Ok((Ledger { path }, state))
    }

    /// Append one transition (the job's full current record).
    pub fn append(&self, record: &JobRecord) -> Result<(), LedgerError> {
        let line = serde_json::to_string(record)
            .map_err(|e| LedgerError::Io(format!("serialize {}: {e}", record.job_id)))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| LedgerError::Io(format!("open {}: {e}", self.path.display())))?;
        writeln!(file, "{line}")
            .map_err(|e| LedgerError::Io(format!("append {}: {e}", self.path.display())))?;
        Ok(())
    }
}

/// Fold ledger text into current state: last record per job id
/// wins; order is first-appearance (submission) order.
fn fold(text: &str) -> LedgerState {
    let mut jobs: Vec<JobRecord> = Vec::new();
    let mut torn_lines = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<JobRecord>(line) {
            Ok(record) => {
                if let Some(slot) = jobs.iter_mut().find(|j| j.job_id == record.job_id) {
                    *slot = record;
                } else {
                    jobs.push(record);
                }
            }
            Err(_) => torn_lines += 1,
        }
    }
    LedgerState { jobs, torn_lines }
}

/// Ledger failure: the broker refuses to run without its books.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger io error: {0}")]
    Io(String),
}

/// Aggregated accounting over the current job states.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SummaryBucket {
    pub jobs: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub denied: u64,
    pub active: u64,
    pub total_cost: f64,
}

impl Default for SummaryBucket {
    fn default() -> Self {
        SummaryBucket {
            jobs: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            denied: 0,
            active: 0,
            total_cost: 0.0,
        }
    }
}

impl SummaryBucket {
    fn add(&mut self, record: &JobRecord) {
        self.jobs += 1;
        match record.status {
            JobStatus::Completed => self.completed += 1,
            JobStatus::Failed => self.failed += 1,
            JobStatus::Cancelled => self.cancelled += 1,
            JobStatus::Denied => self.denied += 1,
            JobStatus::Queued | JobStatus::Running => self.active += 1,
        }
        self.total_cost += record.cost;
    }
}

/// The full ledger summary: totals overall, per route, per lane.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LedgerSummary {
    pub totals: SummaryBucket,
    pub by_route: BTreeMap<String, SummaryBucket>,
    pub by_lane: BTreeMap<String, SummaryBucket>,
}

/// Aggregate job records into a summary. Pure.
pub fn summarize(jobs: &[JobRecord]) -> LedgerSummary {
    let mut summary = LedgerSummary {
        totals: SummaryBucket::default(),
        by_route: BTreeMap::new(),
        by_lane: BTreeMap::new(),
    };
    for record in jobs {
        summary.totals.add(record);
        summary
            .by_route
            .entry(record.route.clone())
            .or_default()
            .add(record);
        summary
            .by_lane
            .entry(record.lane.as_str().to_string())
            .or_default()
            .add(record);
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, status: JobStatus, cost: f64) -> JobRecord {
        JobRecord {
            job_id: id.to_string(),
            route: "wai".to_string(),
            lane: Lane::Sfw,
            kind: JobKind::Image,
            units: 2,
            unit_cost: cost / 2.0,
            cost,
            status,
            submitted_at: 100,
            updated_at: 100,
            exit_code: None,
            detail: None,
        }
    }

    #[test]
    fn fold_takes_the_last_record_per_job_in_submission_order() {
        let mut text = String::new();
        for rec in [
            record("job-000001", JobStatus::Queued, 2.0),
            record("job-000002", JobStatus::Queued, 2.0),
            record("job-000001", JobStatus::Completed, 2.0),
        ] {
            text.push_str(&serde_json::to_string(&rec).expect("ser"));
            text.push('\n');
        }
        let state = fold(&text);
        assert_eq!(state.jobs.len(), 2);
        assert_eq!(state.jobs[0].job_id, "job-000001");
        assert_eq!(state.jobs[0].status, JobStatus::Completed);
        assert_eq!(state.jobs[1].status, JobStatus::Queued);
        assert_eq!(state.torn_lines, 0);
    }

    #[test]
    fn fold_counts_a_torn_line_without_losing_the_rest() {
        let good =
            serde_json::to_string(&record("job-000001", JobStatus::Queued, 0.0)).expect("ser");
        let state = fold(&format!("{good}\n{{\"job_id\":\"job-0000"));
        assert_eq!(state.jobs.len(), 1);
        assert_eq!(state.torn_lines, 1);
    }

    #[test]
    fn summary_accounts_cost_and_status_per_route_and_lane() {
        let mut denied = record("job-000003", JobStatus::Denied, 0.0);
        denied.lane = Lane::Nsfw;
        denied.route = "runway".to_string();
        let jobs = vec![
            record("job-000001", JobStatus::Completed, 2.0),
            record("job-000002", JobStatus::Failed, 2.0),
            denied,
        ];
        let summary = summarize(&jobs);
        assert_eq!(summary.totals.jobs, 3);
        assert_eq!(summary.totals.completed, 1);
        assert_eq!(summary.totals.failed, 1);
        assert_eq!(summary.totals.denied, 1);
        assert_eq!(summary.totals.total_cost, 4.0);
        assert_eq!(summary.by_route["wai"].total_cost, 4.0);
        assert_eq!(summary.by_route["runway"].denied, 1);
        assert_eq!(summary.by_lane["nsfw"].jobs, 1);
        assert_eq!(summary.by_lane["sfw"].total_cost, 4.0);
    }

    #[test]
    fn append_and_reopen_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ledger, state) = Ledger::open(dir.path()).expect("open");
        assert!(state.jobs.is_empty());
        ledger
            .append(&record("job-000001", JobStatus::Queued, 2.0))
            .expect("append");
        ledger
            .append(&record("job-000001", JobStatus::Completed, 2.0))
            .expect("append");
        let (_ledger2, state2) = Ledger::open(dir.path()).expect("reopen");
        assert_eq!(state2.jobs.len(), 1);
        assert_eq!(state2.jobs[0].status, JobStatus::Completed);
    }
}
