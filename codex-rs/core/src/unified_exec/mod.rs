//! Unified Exec: interactive process execution orchestrated with approvals + sandboxing.
//!
//! Responsibilities
//! - Manages interactive processes (create, reuse, buffer output with caps).
//! - Uses the shared ToolOrchestrator to handle approval, sandbox selection, and
//!   retry semantics in a single, descriptive flow.
//! - Spawns the PTY from a sandbox-transformed `ExecRequest`; on sandbox denial,
//!   retries without sandbox when policy allows (no re‑prompt thanks to caching).
//! - Uses the shared `is_likely_sandbox_denied` heuristic to keep denial messages
//!   consistent with other exec paths.
//!
//! Flow at a glance (open process)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//! 3) Runtime: transform `SandboxTransformRequest` -> `ExecRequest` -> spawn PTY.
//! 4) If denial, orchestrator retries with `SandboxType::None`.
//! 5) Process handle is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/process
//! concerns remain isolated here. The implementation is split between:
//! - `process.rs`: PTY process lifecycle + output buffering.
//! - `process_state.rs`: shared exit/failure state for local and remote processes.
//! - `process_manager.rs`: orchestration (approvals, sandboxing, reuse) and request handling.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;

use codex_network_proxy::NetworkProxy;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_tools::UnifiedExecShellMode;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use rand::Rng;
use rand::rng;
use tokio::sync::Mutex;

use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::network_approval::DeferredNetworkApproval;

mod async_watcher;
mod background_output_log;
mod background_triggers;
mod errors;
mod head_tail_buffer;
mod observation;
mod process;
mod process_manager;
mod process_state;

pub(crate) use observation::JobSnapshot;
pub(crate) use observation::JobWaitOutcome;

/// Durable append-only log for a unified-exec session. Every session writes
/// one, and it outlives the process, so observation tools use it as the
/// ground-truth output archive for live and dead jobs alike.
pub(crate) fn background_log_path(
    codex_home: &std::path::Path,
    thread_id: codex_protocol::ThreadId,
    process_id: i32,
) -> PathBuf {
    codex_home
        .join("link")
        .join("jobs")
        .join(thread_id.to_string())
        .join(format!("exec-{process_id}.log"))
}

pub(crate) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    process_manager::set_deterministic_process_ids_for_tests(enabled);
}

pub(crate) use background_output_log::BackgroundOutputLog;
pub(crate) use background_triggers::BackgroundTriggerPolicy;
pub(crate) use errors::UnifiedExecError;
pub(crate) use process::NoopSpawnLifecycle;
#[cfg(unix)]
pub(crate) use process::SpawnLifecycle;
pub(crate) use process::SpawnLifecycleHandle;
pub(crate) use process::UnifiedExecProcess;

pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS: u64 = 2_000;
// Minimum yield time for an empty `write_stdin`.
pub(crate) const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub(crate) const DEFAULT_BACKGROUND_EXEC_YIELD_TIME_MS: u64 = MIN_YIELD_TIME_MS;
pub(crate) const DEFAULT_FOREGROUND_EXEC_YIELD_TIME_MS: u64 = 600_000;
pub(crate) const MAX_YIELD_TIME_MS: u64 = DEFAULT_FOREGROUND_EXEC_YIELD_TIME_MS;
pub(crate) const MAX_NON_EMPTY_STDIN_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 300_000;
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_TOKENS: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 4;
pub(crate) const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
}

impl UnifiedExecContext {
    pub fn new(session: Arc<Session>, turn: Arc<TurnContext>, call_id: String) -> Self {
        Self {
            session,
            turn,
            call_id,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCommandRequest {
    pub command: Vec<String>,
    pub shell_type: ShellType,
    pub hook_command: String,
    pub process_id: i32,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub cwd: PathUri,
    pub sandbox_cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub shell_mode: UnifiedExecShellMode,
    pub network: Option<NetworkProxy>,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_preapproved: bool,
    pub justification: Option<String>,
    pub prefix_rule: Option<Vec<String>>,
    pub background_description: Option<String>,
    pub background_triggers: Vec<String>,
    pub background_trigger_policy: Option<BackgroundTriggerPolicy>,
    pub background_log_path: Option<PathBuf>,
    pub background_declared: bool,
    pub end_turn_after_record: bool,
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub process_id: i32,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub truncation_policy: TruncationPolicy,
}

#[derive(Default)]
pub(crate) struct ProcessStore {
    processes: HashMap<i32, ProcessEntry>,
    reserved_process_ids: HashSet<i32>,
}

impl ProcessStore {
    fn remove(&mut self, process_id: i32) -> Option<ProcessEntry> {
        self.reserved_process_ids.remove(&process_id);
        self.processes.remove(&process_id)
    }
}

pub(crate) struct UnifiedExecProcessManager {
    process_store: Mutex<ProcessStore>,
    max_write_stdin_yield_time_ms: u64,
    /// Bounded archive of reaped sessions so observation tools can still
    /// answer status/wait queries (exit code included) after an entry leaves
    /// the live store. Std mutex: critical sections are short and sync.
    completed_jobs: std::sync::Mutex<std::collections::VecDeque<JobSnapshot>>,
}

impl UnifiedExecProcessManager {
    pub(crate) fn new(max_write_stdin_yield_time_ms: u64) -> Self {
        Self {
            process_store: Mutex::new(ProcessStore::default()),
            max_write_stdin_yield_time_ms: max_write_stdin_yield_time_ms
                .max(MIN_EMPTY_YIELD_TIME_MS),
            completed_jobs: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }
}

impl Default for UnifiedExecProcessManager {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
    }
}

struct ProcessEntry {
    process: Arc<UnifiedExecProcess>,
    call_id: String,
    process_id: i32,
    cwd: PathUri,
    initial_exec_command_active: Arc<std::sync::atomic::AtomicBool>,
    hook_command: String,
    tty: bool,
    network_approval: Option<DeferredNetworkApproval>,
    background_description: Option<String>,
    background_triggers: Vec<String>,
    session: Weak<Session>,
    last_used: tokio::time::Instant,
}

pub(crate) fn clamp_yield_time(yield_time_ms: u64) -> u64 {
    let yield_time_ms = if cfg!(windows) {
        yield_time_ms.max(WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS)
    } else {
        yield_time_ms
    };
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
}

pub(crate) fn resolve_max_tokens(max_tokens: Option<usize>) -> usize {
    max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

pub(crate) fn generate_chunk_id() -> String {
    let mut rng = rng();
    (0..6)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

#[cfg(test)]
#[cfg(unix)]
#[path = "process_tests.rs"]
mod process_tests;
#[cfg(test)]
#[cfg(unix)]
#[path = "mod_tests.rs"]
mod tests;
