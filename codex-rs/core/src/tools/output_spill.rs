//! Spills full tool output to a Link-owned sidecar file when the model-facing
//! copy is truncated.
//!
//! Middle-truncation keeps the head and tail of long output but drops the
//! middle — often exactly where a long log buries its first error. Writing the
//! complete output to disk turns the truncated copy into a bounded preview
//! with a lossless archive behind it: the model can search or tail the file
//! instead of guessing or re-running the command.

use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;

/// Identifies where a spilled exec output should be stored.
pub(crate) struct ExecOutputSpill<'a> {
    pub(crate) codex_home: &'a Path,
    pub(crate) thread_id: ThreadId,
    pub(crate) call_id: &'a str,
}

/// Writes `content` to `<codex_home>/link/tool-output/<thread_id>/<call_id>.txt`,
/// returning the path on success. Failures degrade to a warning: spilling is
/// best-effort and must never fail the tool call.
pub(crate) fn spill_exec_output(spill: &ExecOutputSpill<'_>, content: &str) -> Option<PathBuf> {
    let dir = spill
        .codex_home
        .join("link")
        .join("tool-output")
        .join(spill.thread_id.to_string());
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            "failed to create tool-output spill directory {}: {err}",
            dir.display()
        );
        return None;
    }
    let path = dir.join(format!("{}.txt", sanitize_call_id(spill.call_id)));
    match std::fs::write(&path, content) {
        Ok(()) => Some(path),
        Err(err) => {
            tracing::warn!(
                "failed to write tool-output spill file {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// Call ids are model-generated; keep only filesystem-safe characters so a
/// hostile id cannot traverse out of the spill directory.
fn sanitize_call_id(call_id: &str) -> String {
    let mut sanitized: String = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.truncate(128);
    if sanitized.trim_matches(['.', '_']).is_empty() {
        sanitized = "call".to_string();
    }
    sanitized
}

#[cfg(test)]
#[path = "output_spill_tests.rs"]
mod tests;
