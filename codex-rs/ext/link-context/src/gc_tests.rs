use std::time::Duration;

use super::collect_stale_sidecar_files_with_max_age;

#[test]
fn sweeps_stale_files_but_keeps_current_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let link = dir.path().join("link");

    let context_dir = link.join("context");
    std::fs::create_dir_all(&context_dir).expect("create context dir");
    std::fs::write(context_dir.join("thread-old.json"), "{}").expect("write old state");
    std::fs::write(context_dir.join("thread-current.json"), "{}").expect("write current state");

    let old_outputs = link.join("tool-output").join("thread-old");
    std::fs::create_dir_all(&old_outputs).expect("create old outputs dir");
    std::fs::write(old_outputs.join("call-1.txt"), "spill").expect("write old spill");

    let current_jobs = link.join("jobs").join("thread-current");
    std::fs::create_dir_all(&current_jobs).expect("create current jobs dir");
    std::fs::write(current_jobs.join("exec-1.log"), "log").expect("write current log");

    // Zero max age makes every file stale; only the current thread's files
    // must survive the sweep.
    collect_stale_sidecar_files_with_max_age(dir.path(), "thread-current", Duration::ZERO);

    assert!(!context_dir.join("thread-old.json").exists());
    assert!(context_dir.join("thread-current.json").exists());
    assert!(
        !old_outputs.exists(),
        "emptied thread dir should be removed"
    );
    assert!(current_jobs.join("exec-1.log").exists());
}

#[test]
fn keeps_fresh_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let context_dir = dir.path().join("link").join("context");
    std::fs::create_dir_all(&context_dir).expect("create context dir");
    std::fs::write(context_dir.join("thread-other.json"), "{}").expect("write state");

    collect_stale_sidecar_files_with_max_age(dir.path(), "thread-current", Duration::from_secs(60));

    assert!(context_dir.join("thread-other.json").exists());
}

#[test]
fn missing_link_dir_is_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    collect_stale_sidecar_files_with_max_age(dir.path(), "thread-current", Duration::ZERO);
}
