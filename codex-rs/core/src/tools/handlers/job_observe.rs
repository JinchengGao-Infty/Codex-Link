//! Model-facing observation tools for background exec sessions.
//!
//! `job_observe` gives the model list/status/tail/read/wait primitives that
//! run entirely in the harness; `job_cancel` terminates a session. Together
//! they replace the empty-`write_stdin` polling loop: one bounded tool call
//! per state change instead of a model turn per peek.

use std::path::Path;
use std::time::Duration;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::JobSnapshot;
use crate::unified_exec::JobWaitOutcome;
use crate::unified_exec::background_log_path;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use super::job_observe_spec::JOB_CANCEL_TOOL_NAME;
use super::job_observe_spec::JOB_OBSERVE_TOOL_NAME;
use super::job_observe_spec::create_job_cancel_tool;
use super::job_observe_spec::create_job_observe_tool;

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
const MIN_WAIT_TIMEOUT_MS: u64 = 1_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_TAIL_LINES: usize = 50;
const MAX_TAIL_LINES: usize = 400;
const DEFAULT_READ_LIMIT_BYTES: u64 = 8_192;
const MAX_READ_LIMIT_BYTES: u64 = 16_384;
/// Upper bound on bytes pulled from a log to extract a line tail.
const TAIL_SCAN_BYTES: u64 = 64 * 1024;
const WAIT_RESULT_TAIL_LINES: usize = 20;
const RECENT_COMPLETED_IN_LIST: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct JobObserveArgs {
    action: JobObserveAction,
    // The model is trained on `session_id` for unified exec.
    session_id: Option<i32>,
    timeout_ms: Option<u64>,
    tail_lines: Option<usize>,
    offset_bytes: Option<u64>,
    limit_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobObserveAction {
    List,
    Status,
    Tail,
    Read,
    Wait,
}

#[derive(Debug, Deserialize)]
struct JobCancelArgs {
    session_id: i32,
}

pub struct JobObserveHandler;

impl ToolExecutor<ToolInvocation> for JobObserveHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(JOB_OBSERVE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_job_observe_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_observe(invocation))
    }
}

impl CoreToolRuntime for JobObserveHandler {}

pub struct JobCancelHandler;

impl ToolExecutor<ToolInvocation> for JobCancelHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(JOB_CANCEL_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_job_cancel_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_cancel(invocation))
    }
}

impl CoreToolRuntime for JobCancelHandler {}

async fn handle_observe(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "job_observe handler received unsupported payload".to_string(),
        ));
    };
    let args: JobObserveArgs = parse_arguments(arguments)?;
    let manager = &invocation.session.services.unified_exec_manager;

    let text = match args.action {
        JobObserveAction::List => render_list(
            manager.job_snapshots().await,
            manager.recent_completed_jobs(RECENT_COMPLETED_IN_LIST),
            &invocation,
        ),
        JobObserveAction::Status => {
            let session_id = require_session_id(&args)?;
            // Live entry first; reaped sessions resolve through the completed
            // archive so their exit codes stay answerable.
            let snapshot = match manager.job_snapshot(session_id).await {
                Some(snapshot) => Some(snapshot),
                None => manager.completed_job(session_id),
            };
            render_status(snapshot, session_id, &invocation)
        }
        JobObserveAction::Tail => {
            let session_id = require_session_id(&args)?;
            let lines = args
                .tail_lines
                .unwrap_or(DEFAULT_TAIL_LINES)
                .clamp(1, MAX_TAIL_LINES);
            let log_path = job_log_path(&invocation, session_id);
            match read_log_tail(&log_path, lines) {
                Some((tail, total_bytes)) => format!(
                    "log: {} ({total_bytes} bytes total), last {lines} lines:\n{tail}",
                    log_path.display()
                ),
                None => missing_log_message(session_id, &log_path),
            }
        }
        JobObserveAction::Read => {
            let session_id = require_session_id(&args)?;
            let offset = args.offset_bytes.unwrap_or(0);
            let limit = args
                .limit_bytes
                .unwrap_or(DEFAULT_READ_LIMIT_BYTES)
                .clamp(1, MAX_READ_LIMIT_BYTES);
            let log_path = job_log_path(&invocation, session_id);
            match read_log_range(&log_path, offset, limit) {
                Some((chunk, total_bytes)) => {
                    let end = offset.saturating_add(chunk.len() as u64);
                    format!(
                        "log: {}, bytes {offset}..{end} of {total_bytes}:\n{chunk}",
                        log_path.display()
                    )
                }
                None => missing_log_message(session_id, &log_path),
            }
        }
        JobObserveAction::Wait => {
            let session_id = require_session_id(&args)?;
            let timeout_ms = args
                .timeout_ms
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
                .clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS);
            let outcome = manager
                .wait_for_exit(
                    session_id,
                    Duration::from_millis(timeout_ms),
                    &invocation.cancellation_token,
                )
                .await;
            render_wait(outcome, session_id, timeout_ms, &invocation)
        }
    };

    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        text,
        Some(true),
    )))
}

async fn handle_cancel(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "job_cancel handler received unsupported payload".to_string(),
        ));
    };
    let args: JobCancelArgs = parse_arguments(arguments)?;
    let terminated = invocation
        .session
        .services
        .unified_exec_manager
        .terminate_process(args.session_id)
        .await;
    let log_path = job_log_path(&invocation, args.session_id);
    let text = if terminated {
        format!(
            "terminated session {}. Its log file is kept: {}",
            args.session_id,
            log_path.display()
        )
    } else {
        format!(
            "no live session {} to terminate (it may have already exited).",
            args.session_id
        )
    };
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        text,
        Some(terminated),
    )))
}

fn require_session_id(args: &JobObserveArgs) -> Result<i32, FunctionCallError> {
    args.session_id.ok_or_else(|| {
        FunctionCallError::RespondToModel("session_id is required for this action".to_string())
    })
}

fn job_log_path(invocation: &ToolInvocation, session_id: i32) -> std::path::PathBuf {
    background_log_path(
        &invocation.turn.config.codex_home,
        invocation.session.thread_id(),
        session_id,
    )
}

fn render_list(
    snapshots: Vec<JobSnapshot>,
    completed: Vec<JobSnapshot>,
    invocation: &ToolInvocation,
) -> String {
    let mut lines = Vec::new();
    if snapshots.is_empty() {
        lines.push(
            "No live exec sessions. Finished sessions leave durable logs; use `status`, `tail`, or `read` with a session_id to inspect one.".to_string(),
        );
    } else {
        lines.push(format!("{} live session(s):", snapshots.len()));
        for snapshot in snapshots {
            lines.push(render_snapshot_line(&snapshot, invocation));
        }
    }
    if !completed.is_empty() {
        lines.push("Recently completed:".to_string());
        for snapshot in completed {
            lines.push(render_snapshot_line(&snapshot, invocation));
        }
    }
    lines.join("\n")
}

fn render_snapshot_line(snapshot: &JobSnapshot, invocation: &ToolInvocation) -> String {
    let state = if snapshot.running {
        "running".to_string()
    } else {
        match snapshot.exit_code {
            Some(code) => format!("exited ({code})"),
            None => "exited".to_string(),
        }
    };
    let mut line = format!(
        "- session_id {}: {state} | {} (cwd: {})",
        snapshot.process_id,
        truncate_command(&snapshot.command),
        snapshot.cwd
    );
    if snapshot.started_at_unix_ms > 0 {
        let elapsed_ms = crate::turn_timing::now_unix_timestamp_ms() - snapshot.started_at_unix_ms;
        line.push_str(&format!(" | started {} ago", format_elapsed(elapsed_ms)));
    }
    if let Some(description) = &snapshot.background_description {
        line.push_str(&format!(" | {description}"));
    }
    if !snapshot.background_triggers.is_empty() {
        line.push_str(&format!(
            " | triggers: {}",
            snapshot.background_triggers.join(", ")
        ));
    }
    line.push_str(&format!(
        " | log: {}",
        job_log_path(invocation, snapshot.process_id).display()
    ));
    line
}

fn render_status(
    snapshot: Option<JobSnapshot>,
    session_id: i32,
    invocation: &ToolInvocation,
) -> String {
    let log_path = job_log_path(invocation, session_id);
    match snapshot {
        Some(snapshot) => {
            let log_size = std::fs::metadata(&log_path)
                .map(|meta| format!("{} bytes", meta.len()))
                .unwrap_or_else(|_| "not created yet".to_string());
            let mut line = render_snapshot_line(&snapshot, invocation);
            line.push_str(&format!(" ({log_size})"));
            line
        }
        None => missing_log_message(session_id, &log_path),
    }
}

fn render_wait(
    outcome: JobWaitOutcome,
    session_id: i32,
    timeout_ms: u64,
    invocation: &ToolInvocation,
) -> String {
    let log_path = job_log_path(invocation, session_id);
    let tail = read_log_tail(&log_path, WAIT_RESULT_TAIL_LINES)
        .map(|(tail, _)| format!("\nRecent output ({}):\n{tail}", log_path.display()))
        .unwrap_or_default();
    match outcome {
        JobWaitOutcome::Exited { exit_code } => {
            let code = exit_code.map_or("unknown".to_string(), |code| code.to_string());
            format!("session {session_id} exited with code {code}.{tail}")
        }
        JobWaitOutcome::TimedOut => format!(
            "session {session_id} is still running after {timeout_ms} ms; wait again or tail its log.{tail}"
        ),
        JobWaitOutcome::Interrupted => format!("wait for session {session_id} was interrupted."),
        JobWaitOutcome::Unknown => missing_log_message(session_id, &log_path),
    }
}

fn missing_log_message(session_id: i32, log_path: &Path) -> String {
    if log_path.exists() {
        format!(
            "session {session_id} is not tracked anymore (it already exited or was cancelled); its exit was reported as a job event. Its durable log remains: {} — use `tail` or `read` to inspect it.",
            log_path.display()
        )
    } else {
        format!(
            "no session {session_id} in this thread: nothing is tracked under that id and no log file exists at {}.",
            log_path.display()
        )
    }
}

fn format_elapsed(elapsed_ms: i64) -> String {
    let seconds = elapsed_ms.max(0) / 1_000;
    if seconds < 120 {
        format!("{seconds}s")
    } else if seconds < 120 * 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3_600)
    }
}

fn truncate_command(command: &str) -> String {
    const MAX_COMMAND_CHARS: usize = 120;
    if command.chars().count() <= MAX_COMMAND_CHARS {
        return command.to_string();
    }
    let prefix: String = command.chars().take(MAX_COMMAND_CHARS).collect();
    format!("{prefix}…")
}

/// Returns the last `lines` lines of the log plus its total size, scanning at
/// most [`TAIL_SCAN_BYTES`] from the end. `None` when the file is unreadable.
fn read_log_tail(path: &Path, lines: usize) -> Option<(String, u64)> {
    let (chunk, total_bytes) = read_file_end(path, TAIL_SCAN_BYTES)?;
    let mut tail: Vec<&str> = chunk.lines().rev().take(lines).collect();
    tail.reverse();
    Some((tail.join("\n"), total_bytes))
}

/// Reads up to `limit` bytes starting at `offset`, plus the total file size.
fn read_log_range(path: &Path, offset: u64, limit: u64) -> Option<(String, u64)> {
    use std::io::Read;
    use std::io::Seek;

    let mut file = std::fs::File::open(path).ok()?;
    let total_bytes = file.metadata().ok()?.len();
    file.seek(std::io::SeekFrom::Start(offset.min(total_bytes)))
        .ok()?;
    let mut buffer = vec![0_u8; limit as usize];
    let mut read = 0;
    while read < buffer.len() {
        match file.read(&mut buffer[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    buffer.truncate(read);
    Some((String::from_utf8_lossy(&buffer).into_owned(), total_bytes))
}

fn read_file_end(path: &Path, max_bytes: u64) -> Option<(String, u64)> {
    let total_bytes = std::fs::metadata(path).ok()?.len();
    let offset = total_bytes.saturating_sub(max_bytes);
    let (chunk, _) = read_log_range(path, offset, max_bytes)?;
    Some((chunk, total_bytes))
}

#[cfg(test)]
#[path = "job_observe_tests.rs"]
mod tests;
