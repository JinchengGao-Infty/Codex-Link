//! Age-based garbage collection for Link sidecar files.
//!
//! Link writes three kinds of per-thread sidecar files under
//! `<codex_home>/link/`: durable context state (`context/<thread_id>.json`),
//! spilled tool output (`tool-output/<thread_id>/*.txt`), and background exec
//! logs (`jobs/<thread_id>/exec-*.log`). Nothing else reclaims them, so each
//! thread start sweeps files whose last modification is older than
//! [`MAX_SIDECAR_AGE`]. Anything still in use is rewritten or re-created by
//! its thread, which makes mtime a safe liveness signal.

use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

const MAX_SIDECAR_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Removes stale Link sidecar files, skipping everything that belongs to
/// `current_thread_id` (a resumed thread's state must survive its own sweep).
/// Best-effort: failures downgrade to debug logs; GC must never affect the
/// session.
pub(crate) fn collect_stale_sidecar_files(codex_home: &Path, current_thread_id: &str) {
    collect_stale_sidecar_files_with_max_age(codex_home, current_thread_id, MAX_SIDECAR_AGE);
}

fn collect_stale_sidecar_files_with_max_age(
    codex_home: &Path,
    current_thread_id: &str,
    max_age: Duration,
) {
    let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
        return;
    };
    let link_dir = codex_home.join("link");
    gc_flat_dir(&link_dir.join("context"), current_thread_id, cutoff);
    for per_thread_dir in ["tool-output", "jobs"] {
        gc_thread_dirs(&link_dir.join(per_thread_dir), current_thread_id, cutoff);
    }
}

/// Sweeps a directory of `<thread_id>.json` files.
fn gc_flat_dir(dir: &Path, current_thread_id: &str, cutoff: SystemTime) {
    for entry in read_dir_entries(dir) {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(current_thread_id) {
            continue;
        }
        remove_if_stale(&entry.path(), cutoff);
    }
}

/// Sweeps directories of per-thread subdirectories, removing subdirectories
/// that end up empty.
fn gc_thread_dirs(dir: &Path, current_thread_id: &str, cutoff: SystemTime) {
    for thread_dir in read_dir_entries(dir) {
        if thread_dir.file_name().to_string_lossy() == current_thread_id {
            continue;
        }
        let thread_dir_path = thread_dir.path();
        if !thread_dir_path.is_dir() {
            remove_if_stale(&thread_dir_path, cutoff);
            continue;
        }
        for entry in read_dir_entries(&thread_dir_path) {
            remove_if_stale(&entry.path(), cutoff);
        }
        // Only succeeds once the directory is empty; otherwise it still has
        // fresh files and stays.
        let _ = std::fs::remove_dir(&thread_dir_path);
    }
}

fn read_dir_entries(dir: &Path) -> Vec<std::fs::DirEntry> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(Result::ok).collect(),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("link gc: failed to read {}: {err}", dir.display());
            }
            Vec::new()
        }
    }
}

fn remove_if_stale(path: &Path, cutoff: SystemTime) {
    let modified = match std::fs::metadata(path).and_then(|meta| meta.modified()) {
        Ok(modified) => modified,
        Err(err) => {
            tracing::debug!("link gc: failed to stat {}: {err}", path.display());
            return;
        }
    };
    if modified >= cutoff {
        return;
    }
    if let Err(err) = std::fs::remove_file(path) {
        tracing::debug!("link gc: failed to remove {}: {err}", path.display());
    } else {
        tracing::debug!("link gc: removed stale sidecar file {}", path.display());
    }
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
