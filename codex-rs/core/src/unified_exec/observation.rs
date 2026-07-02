//! Local observation primitives for unified-exec sessions.
//!
//! These back the model-facing `job_observe` / `job_cancel` tools. Everything
//! here runs in the harness: waiting for a process to exit blocks a tool call
//! on the process cancellation token, not a model reasoning loop, so watching
//! a long job costs zero inference between state changes.

use std::sync::Arc;
use std::time::Duration;

use codex_utils_path_uri::PathUri;
use tokio_util::sync::CancellationToken;

use super::UnifiedExecProcess;
use super::UnifiedExecProcessManager;

/// Point-in-time view of one tracked session. Exited sessions vanish from the
/// manager when reaped, so absence of a snapshot does not mean the job never
/// existed — its durable log outlives it.
#[derive(Clone, Debug)]
pub(crate) struct JobSnapshot {
    pub(crate) process_id: i32,
    pub(crate) command: String,
    pub(crate) cwd: PathUri,
    pub(crate) background_description: Option<String>,
    pub(crate) background_triggers: Vec<String>,
    pub(crate) running: bool,
    pub(crate) exit_code: Option<i32>,
    /// Wall-clock start (unix ms). No end timestamp: exited entries are
    /// reaped lazily, so stamping one at archive time would overstate runtime.
    pub(crate) started_at_unix_ms: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JobWaitOutcome {
    Exited { exit_code: Option<i32> },
    TimedOut,
    Interrupted,
    Unknown,
}

const MAX_COMPLETED_JOBS: usize = 64;

impl UnifiedExecProcessManager {
    /// Records a session that is leaving the live store so `job_observe` can
    /// still answer status and wait queries — exit code included — after the
    /// process is reaped.
    pub(super) fn archive_completed_entry(&self, entry: &super::ProcessEntry) {
        let mut snapshot = snapshot_entry(entry);
        snapshot.running = false;
        self.archive_completed(snapshot);
    }

    pub(super) fn archive_completed(&self, snapshot: JobSnapshot) {
        let Ok(mut completed) = self.completed_jobs.lock() else {
            return;
        };
        completed.retain(|job| job.process_id != snapshot.process_id);
        completed.push_back(snapshot);
        while completed.len() > MAX_COMPLETED_JOBS {
            completed.pop_front();
        }
    }

    pub(crate) fn completed_job(&self, process_id: i32) -> Option<JobSnapshot> {
        let completed = self.completed_jobs.lock().ok()?;
        completed
            .iter()
            .find(|job| job.process_id == process_id)
            .cloned()
    }

    /// Most recently completed first.
    pub(crate) fn recent_completed_jobs(&self, limit: usize) -> Vec<JobSnapshot> {
        match self.completed_jobs.lock() {
            Ok(completed) => completed.iter().rev().take(limit).cloned().collect(),
            Err(_) => Vec::new(),
        }
    }
    pub(crate) async fn job_snapshots(&self) -> Vec<JobSnapshot> {
        let store = self.process_store.lock().await;
        let mut snapshots: Vec<JobSnapshot> =
            store.processes.values().map(snapshot_entry).collect();
        snapshots.sort_by_key(|snapshot| snapshot.process_id);
        snapshots
    }

    pub(crate) async fn job_snapshot(&self, process_id: i32) -> Option<JobSnapshot> {
        let store = self.process_store.lock().await;
        store.processes.get(&process_id).map(snapshot_entry)
    }

    /// Blocks until the session exits, `timeout` elapses, or the tool call is
    /// interrupted — whichever comes first. Returns `Unknown` when no session
    /// with that id is tracked (never started, or already reaped).
    pub(crate) async fn wait_for_exit(
        &self,
        process_id: i32,
        timeout: Duration,
        interrupt: &CancellationToken,
    ) -> JobWaitOutcome {
        let process: Arc<UnifiedExecProcess> = {
            let store = self.process_store.lock().await;
            match store.processes.get(&process_id) {
                Some(entry) => Arc::clone(&entry.process),
                None => {
                    // Already reaped sessions still resolve through the
                    // completed archive instead of reporting Unknown.
                    return match self.completed_job(process_id) {
                        Some(job) => JobWaitOutcome::Exited {
                            exit_code: job.exit_code,
                        },
                        None => JobWaitOutcome::Unknown,
                    };
                }
            }
        };

        if !process.has_exited() {
            let exit_token = process.cancellation_token();
            tokio::select! {
                _ = exit_token.cancelled() => {}
                _ = interrupt.cancelled() => return JobWaitOutcome::Interrupted,
                _ = tokio::time::sleep(timeout) => return JobWaitOutcome::TimedOut,
            }
        }

        // The cancellation token fires at exit, slightly before the exit code
        // is recorded; give the reaper a moment before reading it.
        let mut exit_code = process.exit_code();
        if exit_code.is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            exit_code = process.exit_code();
        }
        JobWaitOutcome::Exited { exit_code }
    }
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;

fn snapshot_entry(entry: &super::ProcessEntry) -> JobSnapshot {
    JobSnapshot {
        process_id: entry.process_id,
        command: entry.hook_command.clone(),
        cwd: entry.cwd.clone(),
        background_description: entry.background_description.clone(),
        background_triggers: entry.background_triggers.clone(),
        running: !entry.process.has_exited(),
        exit_code: entry.process.exit_code(),
        started_at_unix_ms: entry.started_at_unix_ms,
    }
}
