//! The broker: job queue + executor over the route table, with
//! every transition written to the ledger.
//!
//! Concurrency shape: one mutex holds the job states; execution
//! happens outside the lock in a spawned task per job. A cancel
//! lands in a flag set the executor polls while it waits on the
//! child, so a running job can be killed, not just a queued one.
//! Work is bounded: job count, units per job, and per-route
//! execution timeout all have hard caps (see below).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use crate::config::{Route, Routes};
use crate::decide::{self, Decision, JobKind, Lane};
use crate::ledger::{JobRecord, JobStatus, Ledger, LedgerSummary, summarize};

/// Most jobs the in-memory queue holds at once; submits beyond
/// this are refused with `queue_full` rather than growing without
/// bound. Terminal jobs still count until restart — the ledger is
/// the archive; this cap bounds one server's working set.
pub const MAX_JOBS: usize = 10_000;
/// Most units a single job may claim.
pub const MAX_UNITS: u32 = 10_000;
/// How often the executor polls the cancel flag while waiting.
const CANCEL_POLL: Duration = Duration::from_millis(50);

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A submit-time refusal that never reaches the ledger (client
/// error: the request names nothing real). Lane denials are NOT
/// this — they are ledgered job records (see [`Broker::submit`]).
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("unknown route {0:?}")]
    UnknownRoute(String),
    #[error("units must be 1..={MAX_UNITS}, got {0}")]
    UnitsOutOfRange(u32),
    #[error("job queue is full ({MAX_JOBS} jobs)")]
    QueueFull,
    #[error("ledger io error: {0}")]
    Ledger(String),
}

struct Inner {
    jobs: Vec<JobRecord>,
    cancelled: HashSet<String>,
    next_seq: u64,
    ledger: Ledger,
}

/// The broker handle: cheap to clone, shared by the MCP dispatch
/// and the executor tasks.
#[derive(Clone)]
pub struct Broker {
    routes: Arc<Routes>,
    inner: Arc<Mutex<Inner>>,
}

impl Broker {
    /// Open a broker over `routes` with its ledger in `data_dir`.
    /// Jobs left `queued`/`running` by a previous process are
    /// marked failed (`interrupted_by_restart`): the broker never
    /// pretends an execution it did not witness completed.
    pub fn open(routes: Routes, data_dir: &std::path::Path) -> Result<Broker, SubmitError> {
        let (ledger, state) =
            Ledger::open(data_dir).map_err(|e| SubmitError::Ledger(e.to_string()))?;
        let mut jobs = state.jobs;
        let mut next_seq = 1u64;
        for job in &jobs {
            if let Some(seq) = job
                .job_id
                .strip_prefix("job-")
                .and_then(|n| n.parse::<u64>().ok())
            {
                next_seq = next_seq.max(seq + 1);
            }
        }
        let now = now_epoch();
        for job in jobs.iter_mut() {
            if matches!(job.status, JobStatus::Queued | JobStatus::Running) {
                job.status = JobStatus::Failed;
                job.detail = Some("interrupted_by_restart".to_string());
                job.updated_at = now;
                ledger
                    .append(job)
                    .map_err(|e| SubmitError::Ledger(e.to_string()))?;
            }
        }
        Ok(Broker {
            routes: Arc::new(routes),
            inner: Arc::new(Mutex::new(Inner {
                jobs,
                cancelled: HashSet::new(),
                next_seq,
                ledger,
            })),
        })
    }

    /// The route table this broker dispatches over.
    pub fn routes(&self) -> &Routes {
        &self.routes
    }

    /// Submit a job. The `decide()` gate runs first: a denied job
    /// is recorded in the ledger as `denied` with cost 0 and the
    /// gate's reason, and returned as a normal record (the denial
    /// is the outcome, not a transport error). An allowed job is
    /// recorded `queued` and its execution is spawned.
    pub async fn submit(
        &self,
        route_name: &str,
        lane: Lane,
        kind: JobKind,
        units: u32,
    ) -> Result<JobRecord, SubmitError> {
        let Some(route) = self.routes.get(route_name).cloned() else {
            return Err(SubmitError::UnknownRoute(route_name.to_string()));
        };
        if units == 0 || units > MAX_UNITS {
            return Err(SubmitError::UnitsOutOfRange(units));
        }
        let decision = decide::decide(&route, lane, kind);
        let record = {
            let mut inner = self.inner.lock().await;
            if inner.jobs.len() >= MAX_JOBS {
                return Err(SubmitError::QueueFull);
            }
            let now = now_epoch();
            let (status, cost, detail) = match &decision {
                Decision::Allow => (JobStatus::Queued, units as f64 * route.unit_cost, None),
                Decision::Deny { reason, .. } => (JobStatus::Denied, 0.0, Some(reason.clone())),
            };
            let record = JobRecord {
                job_id: format!("job-{:06}", inner.next_seq),
                route: route.name.clone(),
                lane,
                kind,
                units,
                unit_cost: route.unit_cost,
                cost,
                status,
                submitted_at: now,
                updated_at: now,
                exit_code: None,
                detail,
            };
            inner.next_seq += 1;
            inner
                .ledger
                .append(&record)
                .map_err(|e| SubmitError::Ledger(e.to_string()))?;
            inner.jobs.push(record.clone());
            record
        };
        if record.status == JobStatus::Queued {
            let broker = self.clone();
            let job_id = record.job_id.clone();
            tokio::spawn(async move {
                broker.execute(&job_id, &route).await;
            });
        }
        Ok(record)
    }

    /// One job's current record, if it exists.
    pub async fn get(&self, job_id: &str) -> Option<JobRecord> {
        let inner = self.inner.lock().await;
        inner.jobs.iter().find(|j| j.job_id == job_id).cloned()
    }

    /// Current jobs, in submission order, optionally filtered by
    /// status and/or route, most recent last; `limit` caps how
    /// many of the newest matches are returned.
    pub async fn list(
        &self,
        status: Option<JobStatus>,
        route: Option<&str>,
        limit: usize,
    ) -> Vec<JobRecord> {
        let inner = self.inner.lock().await;
        let mut matches: Vec<JobRecord> = inner
            .jobs
            .iter()
            .filter(|j| status.is_none_or(|s| j.status == s))
            .filter(|j| route.is_none_or(|r| j.route == r))
            .cloned()
            .collect();
        if matches.len() > limit {
            matches = matches.split_off(matches.len() - limit);
        }
        matches
    }

    /// Cancel a job. A queued job is cancelled before it runs; a
    /// running job's executor is flagged and kills the child. A
    /// terminal job cannot be cancelled.
    pub async fn cancel(&self, job_id: &str) -> Result<JobRecord, CancelError> {
        let mut inner = self.inner.lock().await;
        let Some(pos) = inner.jobs.iter().position(|j| j.job_id == job_id) else {
            return Err(CancelError::NotFound(job_id.to_string()));
        };
        match inner.jobs[pos].status {
            JobStatus::Queued => {
                inner.jobs[pos].status = JobStatus::Cancelled;
                inner.jobs[pos].detail = Some("cancelled_before_start".to_string());
                inner.jobs[pos].updated_at = now_epoch();
                let record = inner.jobs[pos].clone();
                inner
                    .ledger
                    .append(&record)
                    .map_err(|e| CancelError::Ledger(e.to_string()))?;
                Ok(record)
            }
            JobStatus::Running => {
                inner.cancelled.insert(job_id.to_string());
                Ok(inner.jobs[pos].clone())
            }
            terminal => Err(CancelError::AlreadyTerminal {
                job_id: job_id.to_string(),
                status: terminal,
            }),
        }
    }

    /// Ledger accounting over current job states.
    pub async fn summary(&self) -> LedgerSummary {
        let inner = self.inner.lock().await;
        summarize(&inner.jobs)
    }

    /// Apply a transition to one job and ledger it.
    async fn transition(
        &self,
        job_id: &str,
        status: JobStatus,
        exit_code: Option<i32>,
        detail: Option<String>,
    ) {
        let mut inner = self.inner.lock().await;
        if let Some(job) = inner.jobs.iter_mut().find(|j| j.job_id == job_id) {
            job.status = status;
            job.exit_code = exit_code;
            job.detail = detail;
            job.updated_at = now_epoch();
            let record = job.clone();
            // A ledger write failure here cannot be surfaced to a
            // caller (the execution already happened); it is the
            // one place the broker logs-and-continues, to stderr.
            if let Err(e) = inner.ledger.append(&record) {
                eprintln!("cartone: ledger append failed for {job_id}: {e}");
            }
        }
    }

    async fn is_cancelled(&self, job_id: &str) -> bool {
        let inner = self.inner.lock().await;
        inner.cancelled.contains(job_id)
    }

    /// Execute one queued job: mark running, run the route's
    /// command template as a subprocess (direct exec, no shell)
    /// with the route's bounded timeout, and record the outcome.
    async fn execute(&self, job_id: &str, route: &Route) {
        // A cancel that landed between submit and start wins.
        if self.is_cancelled(job_id).await {
            self.transition(
                job_id,
                JobStatus::Cancelled,
                None,
                Some("cancelled_before_start".to_string()),
            )
            .await;
            return;
        }
        self.transition(job_id, JobStatus::Running, None, None)
            .await;
        if route.command.is_empty() {
            self.transition(
                job_id,
                JobStatus::Failed,
                None,
                Some(format!(
                    "route_not_configured: no command template is configured for route {:?}",
                    route.name
                )),
            )
            .await;
            return;
        }
        let record = self.get(job_id).await;
        let Some(record) = record else { return };
        let argv: Vec<String> = route
            .command
            .iter()
            .map(|arg| substitute(arg, &record))
            .collect();
        let outcome = self.run_command(job_id, &argv, route.timeout_secs).await;
        match outcome {
            RunOutcome::Exited(code) if code == Some(0) => {
                self.transition(job_id, JobStatus::Completed, code, None)
                    .await;
            }
            RunOutcome::Exited(code) => {
                self.transition(
                    job_id,
                    JobStatus::Failed,
                    code,
                    Some(format!("executor_exit_{}", code.unwrap_or(-1))),
                )
                .await;
            }
            RunOutcome::TimedOut => {
                self.transition(
                    job_id,
                    JobStatus::Failed,
                    None,
                    Some(format!("executor_timeout_{}s", route.timeout_secs)),
                )
                .await;
            }
            RunOutcome::Cancelled => {
                self.transition(
                    job_id,
                    JobStatus::Cancelled,
                    None,
                    Some("cancelled_while_running".to_string()),
                )
                .await;
            }
            RunOutcome::SpawnFailed(message) => {
                self.transition(
                    job_id,
                    JobStatus::Failed,
                    None,
                    Some(format!("executor_spawn_failed: {message}")),
                )
                .await;
            }
        }
    }

    /// Run one command, polling for cancellation and enforcing
    /// the timeout. The child's output is discarded: the broker
    /// brokers execution and accounting, not content (a route's
    /// own tooling owns its outputs).
    async fn run_command(&self, job_id: &str, argv: &[String], timeout_secs: u64) -> RunOutcome {
        let Some((program, args)) = argv.split_first() else {
            return RunOutcome::SpawnFailed("empty command".to_string());
        };
        let mut child = match tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return RunOutcome::SpawnFailed(e.to_string()),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return RunOutcome::Exited(status.code()),
                Ok(None) => {}
                Err(e) => return RunOutcome::SpawnFailed(format!("wait: {e}")),
            }
            if self.is_cancelled(job_id).await {
                let _ = child.kill().await;
                return RunOutcome::Cancelled;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill().await;
                return RunOutcome::TimedOut;
            }
            tokio::time::sleep(CANCEL_POLL).await;
        }
    }
}

enum RunOutcome {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
    SpawnFailed(String),
}

/// Cancel failure.
#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    #[error("unknown job {0:?}")]
    NotFound(String),
    #[error("job {job_id:?} is already {status:?} and cannot be cancelled")]
    AlreadyTerminal { job_id: String, status: JobStatus },
    #[error("ledger io error: {0}")]
    Ledger(String),
}

/// Substitute the per-job placeholders in one command argument.
/// Unknown placeholders are left untouched — substitution is
/// literal and total over the five known names.
fn substitute(arg: &str, record: &JobRecord) -> String {
    arg.replace("{job_id}", &record.job_id)
        .replace("{route}", &record.route)
        .replace("{kind}", record.kind.as_str())
        .replace("{lane}", record.lane.as_str())
        .replace("{units}", &record.units.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Routes;

    #[test]
    fn substitution_is_literal_and_total() {
        let record = JobRecord {
            job_id: "job-000007".to_string(),
            route: "wai".to_string(),
            lane: Lane::Nsfw,
            kind: JobKind::Image,
            units: 3,
            unit_cost: 1.0,
            cost: 3.0,
            status: JobStatus::Queued,
            submitted_at: 0,
            updated_at: 0,
            exit_code: None,
            detail: None,
        };
        assert_eq!(
            substitute(
                "run {route} {job_id} {kind} {lane} {units} {other}",
                &record
            ),
            "run wai job-000007 image nsfw 3 {other}"
        );
    }

    #[test]
    fn submit_rejects_unknown_routes_and_bad_units() {
        let runtime = tokio::runtime::Runtime::new().expect("rt");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let broker = Broker::open(Routes::defaults(), dir.path()).expect("open");
            assert!(matches!(
                broker.submit("nope", Lane::Sfw, JobKind::Image, 1).await,
                Err(SubmitError::UnknownRoute(_))
            ));
            assert!(matches!(
                broker.submit("wai", Lane::Sfw, JobKind::Image, 0).await,
                Err(SubmitError::UnitsOutOfRange(0))
            ));
            assert!(matches!(
                broker
                    .submit("wai", Lane::Sfw, JobKind::Image, MAX_UNITS + 1)
                    .await,
                Err(SubmitError::UnitsOutOfRange(_))
            ));
        });
    }
}
