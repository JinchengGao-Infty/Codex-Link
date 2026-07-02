use std::time::Duration;

use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::JobSnapshot;
use super::JobWaitOutcome;
use crate::unified_exec::UnifiedExecProcessManager;

fn completed_snapshot(process_id: i32, exit_code: Option<i32>) -> JobSnapshot {
    JobSnapshot {
        process_id,
        command: format!("echo job-{process_id}"),
        cwd: PathUri::from_host_native_path("/tmp").expect("path uri"),
        background_description: None,
        background_triggers: Vec::new(),
        running: false,
        exit_code,
    }
}

#[tokio::test]
async fn archive_answers_status_and_wait_for_reaped_jobs() {
    let manager = UnifiedExecProcessManager::default();
    manager.archive_completed(completed_snapshot(1000, Some(0)));

    let job = manager.completed_job(1000).expect("archived job");
    assert_eq!(job.exit_code, Some(0));
    assert!(!job.running);

    let outcome = manager
        .wait_for_exit(1000, Duration::from_millis(10), &CancellationToken::new())
        .await;
    assert_eq!(outcome, JobWaitOutcome::Exited { exit_code: Some(0) });

    let unknown = manager
        .wait_for_exit(9999, Duration::from_millis(10), &CancellationToken::new())
        .await;
    assert_eq!(unknown, JobWaitOutcome::Unknown);
}

#[tokio::test]
async fn archive_dedups_by_process_id_and_stays_bounded() {
    let manager = UnifiedExecProcessManager::default();
    manager.archive_completed(completed_snapshot(1000, Some(1)));
    manager.archive_completed(completed_snapshot(1000, Some(0)));
    assert_eq!(
        manager.completed_job(1000).expect("archived job").exit_code,
        Some(0),
        "re-archiving the same id should keep the latest record only"
    );

    for process_id in 0..200 {
        manager.archive_completed(completed_snapshot(process_id, Some(0)));
    }
    assert!(
        manager.completed_job(0).is_none(),
        "oldest records should be evicted past the cap"
    );
    let recent = manager.recent_completed_jobs(3);
    let ids: Vec<i32> = recent.iter().map(|job| job.process_id).collect();
    assert_eq!(ids, vec![199, 198, 197], "most recent first");
}
