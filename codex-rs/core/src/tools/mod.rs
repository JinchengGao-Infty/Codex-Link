pub(crate) mod code_mode;
pub(crate) mod context;
pub(crate) mod events;
pub(crate) mod handlers;
pub(crate) mod hook_names;
pub(crate) mod hosted_spec;
pub(crate) mod lifecycle;
pub(crate) mod network_approval;
pub(crate) mod orchestrator;
pub(crate) mod output_spill;
pub(crate) mod parallel;
pub(crate) mod registry;
pub(crate) mod router;
pub(crate) mod runtimes;
pub(crate) mod sandboxing;
pub(crate) mod spec_plan;
pub(crate) mod tool_dispatch_trace;

use std::borrow::Cow;

use crate::session::turn_context::TurnContext;
use codex_features::Feature;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_output_truncation::truncate_text;
pub use router::ToolRouter;

// Telemetry preview limits: keep log events smaller than model budgets.
pub(crate) const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024; // 2 KiB
pub(crate) const TELEMETRY_PREVIEW_MAX_LINES: usize = 64; // lines
pub(crate) const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str =
    "[... telemetry preview truncated ...]";

/// Legacy boundaries such as hook payloads, telemetry tags, and Responses tool
/// names still require a single flattened string. Keep comparisons and sorting
/// on `ToolName` itself; use this only when crossing those boundaries.
pub(crate) fn flat_tool_name(tool_name: &ToolName) -> Cow<'_, str> {
    match tool_name.namespace.as_deref() {
        Some(namespace) => {
            let mut name = String::with_capacity(namespace.len() + tool_name.name.len());
            name.push_str(namespace);
            name.push_str(&tool_name.name);
            Cow::Owned(name)
        }
        None => Cow::Borrowed(tool_name.name.as_str()),
    }
}

pub(crate) fn tool_user_shell_type(
    user_shell: &crate::shell::Shell,
) -> codex_tools::ToolUserShellType {
    match user_shell.shell_type {
        crate::shell::ShellType::Zsh => codex_tools::ToolUserShellType::Zsh,
        crate::shell::ShellType::Bash => codex_tools::ToolUserShellType::Bash,
        crate::shell::ShellType::PowerShell => codex_tools::ToolUserShellType::PowerShell,
        crate::shell::ShellType::Sh => codex_tools::ToolUserShellType::Sh,
        crate::shell::ShellType::Cmd => codex_tools::ToolUserShellType::Cmd,
    }
}

fn effective_tool_mode(turn_context: &TurnContext) -> ToolMode {
    turn_context.model_info.tool_mode.unwrap_or_else(|| {
        if turn_context.config.features.enabled(Feature::CodeModeOnly) {
            ToolMode::CodeModeOnly
        } else if turn_context.config.features.enabled(Feature::CodeMode) {
            ToolMode::CodeMode
        } else {
            ToolMode::Direct
        }
    })
}

/// Format the combined exec output for sending back to the model.
/// Includes exit code and duration metadata; truncates large bodies safely.
/// When `spill` is provided and the body is truncated, the full output is
/// written to a sidecar file and its path is included so the model can
/// recover the dropped middle instead of guessing or re-running the command.
pub fn format_exec_output_for_model(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    spill: Option<output_spill::ExecOutputSpill<'_>>,
) -> String {
    // round to 1 decimal place
    let duration_seconds = ((exec_output.duration.as_secs_f32()) * 10.0).round() / 10.0;

    let content = build_content_with_timeout(exec_output);

    let total_lines = content.lines().count();

    // Ground truth for "was it truncated": run the policy once and compare,
    // instead of guessing from byte approximations that diverge from token
    // budgets on dense content.
    let formatted_full = truncate_text(&content, truncation_policy);
    let truncated = formatted_full.len() != content.len();
    // Below this budget the pointer line would displace the very content it
    // points at, so tightly-capped outputs keep the plain truncation behavior.
    const MIN_SPILL_BYTE_BUDGET: usize = 2_048;
    let spill_section = if truncated && truncation_policy.byte_budget() >= MIN_SPILL_BYTE_BUDGET {
        spill
            .and_then(|spill| output_spill::spill_exec_output(&spill, &content))
            .map(|full_output_path| {
                format!(
                    "Full untruncated output saved to: {} — if a detail from the truncated middle matters, search that file with rg/grep or tail it instead of re-running the command.",
                    full_output_path.display()
                )
            })
    } else {
        None
    };
    // Reserve the pointer line's cost from the body budget so the combined
    // message stays within the original policy and is not re-truncated by the
    // history-recording layer (which would cut through the path itself).
    let body_policy = match &spill_section {
        Some(section) => match truncation_policy {
            TruncationPolicy::Bytes(bytes) => {
                TruncationPolicy::Bytes(bytes.saturating_sub(section.len() + 1).max(1))
            }
            TruncationPolicy::Tokens(tokens) => TruncationPolicy::Tokens(
                tokens
                    .saturating_sub(codex_utils_output_truncation::approx_token_count(section) + 1)
                    .max(1),
            ),
        },
        None => truncation_policy,
    };

    let formatted_output = if spill_section.is_some() {
        truncate_text(&content, body_policy)
    } else {
        formatted_full
    };

    let mut sections = Vec::new();

    sections.push(format!("Exit code: {}", exec_output.exit_code));
    sections.push(format!("Wall time: {duration_seconds} seconds"));
    if total_lines != formatted_output.lines().count() {
        sections.push(format!("Total output lines: {total_lines}"));
    }
    if let Some(spill_section) = spill_section {
        sections.push(spill_section);
    }

    sections.push("Output:".to_string());
    sections.push(formatted_output);

    sections.join("\n")
}

pub fn format_exec_output_str(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
) -> String {
    let content = build_content_with_timeout(exec_output);

    // Truncate for model consumption before serialization.
    formatted_truncate_text(&content, truncation_policy)
}

/// Extracts exec output content and prepends a timeout message if the command timed out.
fn build_content_with_timeout(exec_output: &ExecToolCallOutput) -> String {
    if exec_output.timed_out {
        format!(
            "command timed out after {} milliseconds\n{}",
            exec_output.duration.as_millis(),
            exec_output.aggregated_output.text
        )
    } else {
        exec_output.aggregated_output.text.clone()
    }
}
