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
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;

use crate::context_manager::truncate_function_output_payload;

/// Below this budget the pointer line would displace the very content it
/// points at, so tightly-capped outputs keep the plain truncation behavior.
pub(crate) const MIN_SPILL_BYTE_BUDGET: usize = 2_048;

/// Identifies where a spilled exec output should be stored.
pub(crate) struct ExecOutputSpill<'a> {
    pub(crate) codex_home: &'a Path,
    pub(crate) thread_id: ThreadId,
    pub(crate) call_id: &'a str,
}

/// Owned spill destination for outputs formatted away from the tool
/// invocation (e.g. `McpToolOutput::response_payload`, which runs when the
/// output is converted into a response item and no longer sees the session).
#[derive(Clone, Debug)]
pub(crate) struct ToolOutputSpillParams {
    pub(crate) codex_home: PathBuf,
    pub(crate) thread_id: ThreadId,
    pub(crate) call_id: String,
}

impl ToolOutputSpillParams {
    pub(crate) fn spill(&self, content: &str) -> Option<PathBuf> {
        spill_to_file(&self.codex_home, self.thread_id, &self.call_id, content)
    }
}

/// Writes `content` to `<codex_home>/link/tool-output/<thread_id>/<call_id>.txt`,
/// returning the path on success. Failures degrade to a warning: spilling is
/// best-effort and must never fail the tool call.
pub(crate) fn spill_exec_output(spill: &ExecOutputSpill<'_>, content: &str) -> Option<PathBuf> {
    spill_to_file(spill.codex_home, spill.thread_id, spill.call_id, content)
}

fn spill_to_file(
    codex_home: &Path,
    thread_id: ThreadId,
    call_id: &str,
    content: &str,
) -> Option<PathBuf> {
    let dir = codex_home
        .join("link")
        .join("tool-output")
        .join(thread_id.to_string());
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            "failed to create tool-output spill directory {}: {err}",
            dir.display()
        );
        return None;
    }
    let path = dir.join(format!("{}.txt", sanitize_call_id(call_id)));
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

/// The model-facing pointer to a spilled file. `redo_hint` names the wasteful
/// recovery action the pointer replaces ("re-running the command",
/// "re-calling the tool").
pub(crate) fn spill_pointer_line(path: &Path, redo_hint: &str) -> String {
    format!(
        "Full untruncated output saved to: {} — if a detail from the truncated middle matters, search that file with rg/grep or tail it instead of {redo_hint}.",
        path.display()
    )
}

/// Truncates `payload` under `policy`; when text was dropped and a spill
/// destination is available with enough budget for the pointer to be useful,
/// archives the full text to a sidecar file and appends a pointer line whose
/// cost is reserved from the body budget. Only text segments are truncated
/// (images and encrypted content pass through), so only text is spilled.
pub(crate) fn truncate_payload_with_spill(
    payload: &FunctionCallOutputPayload,
    policy: TruncationPolicy,
    spill: Option<&ToolOutputSpillParams>,
    redo_hint: &str,
) -> FunctionCallOutputPayload {
    let truncated = truncate_function_output_payload(payload, policy);
    if truncated == *payload {
        return truncated;
    }

    let spill = spill.filter(|_| policy.byte_budget() >= MIN_SPILL_BYTE_BUDGET);
    let Some(full_output_path) = spill.and_then(|spill| {
        payload
            .body
            .to_text()
            .and_then(|full_text| spill.spill(&full_text))
    }) else {
        return truncated;
    };
    let pointer_line = spill_pointer_line(&full_output_path, redo_hint);
    let body_policy = reserve_pointer_budget(policy, &pointer_line);
    let mut reduced = truncate_function_output_payload(payload, body_policy);
    match &mut reduced.body {
        FunctionCallOutputBody::Text(text) => {
            text.push('\n');
            text.push_str(&pointer_line);
        }
        FunctionCallOutputBody::ContentItems(items) => {
            items.push(FunctionCallOutputContentItem::InputText { text: pointer_line });
        }
    }
    reduced
}

/// Reserves the pointer line's cost from a truncation budget so the combined
/// body-plus-pointer message stays within the original policy and is not
/// re-truncated by the history-recording layer (which would cut through the
/// path itself).
pub(crate) fn reserve_pointer_budget(
    policy: TruncationPolicy,
    pointer_line: &str,
) -> TruncationPolicy {
    match policy {
        TruncationPolicy::Bytes(bytes) => {
            TruncationPolicy::Bytes(bytes.saturating_sub(pointer_line.len() + 1).max(1))
        }
        TruncationPolicy::Tokens(tokens) => TruncationPolicy::Tokens(
            tokens
                .saturating_sub(approx_token_count(pointer_line) + 1)
                .max(1),
        ),
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
