//! Codex-Link context capsule extension.
//!
//! This crate owns the compact-survival prompt capsule for Link-specific task
//! state. The first version deliberately keeps storage simple: other
//! lifecycle contributors can write a [`LinkContextState`] into the thread
//! extension store, this extension records host-observed tool status, and the
//! capsule is rendered on every turn.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;

use codex_core::ThreadManager;
use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::PromptSlot;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadResumeInput;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::TurnContextContributionInput;
use codex_extension_api::TurnEventContributor;
use codex_extension_api::TurnEventFuture;
use codex_extension_api::TurnEventInput;
use codex_extension_api::TurnItemContributor;
use codex_protocol::ThreadId;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecBackgroundTriggerEvent;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_state::ThreadGoal;
use codex_state::ThreadGoalStatus;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

const CAPSULE_START: &str = "<codex_link_context_capsule>";
const CAPSULE_END: &str = "</codex_link_context_capsule>";
const DEFAULT_MAX_CAPSULE_TOKENS: usize = 1_500;
const DEFAULT_MAX_FIELD_CHARS: usize = 1_200;
const MAX_RECORDED_TOOL_EVENTS: usize = 16;
const MAX_RECORDED_FILES_TOUCHED: usize = 32;
const MAX_TOOL_EVENT_CHARS: usize = 900;
const MAX_TOOL_OUTPUT_PREVIEW_CHARS: usize = 600;
const MAX_PENDING_BACKGROUND_EVENTS: usize = 16;
const MAX_RECORDED_BACKGROUND_EVENT_KEYS: usize = 128;
const MAX_BACKGROUND_EVENT_TAIL_CHARS: usize = 2_000;

/// Thread-scoped Link context that should survive transcript compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkContextState {
    pub active_goal: Option<String>,
    pub success_criteria: Vec<String>,
    pub current_progress: Vec<String>,
    pub next_action: Option<String>,
    pub files_touched: Vec<String>,
    pub verified_evidence: Vec<String>,
    pub active_background_jobs: Vec<String>,
    pub open_blockers: Vec<String>,
}

impl LinkContextState {
    /// Returns true when the state has no model-visible task facts.
    pub fn is_empty(&self) -> bool {
        self.active_goal
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
            && self
                .success_criteria
                .iter()
                .all(|value| value.trim().is_empty())
            && self
                .current_progress
                .iter()
                .all(|value| value.trim().is_empty())
            && self
                .next_action
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            && self
                .files_touched
                .iter()
                .all(|value| value.trim().is_empty())
            && self
                .verified_evidence
                .iter()
                .all(|value| value.trim().is_empty())
            && self
                .active_background_jobs
                .iter()
                .all(|value| value.trim().is_empty())
            && self
                .open_blockers
                .iter()
                .all(|value| value.trim().is_empty())
    }

    /// Renders a bounded prompt capsule, or `None` when there is no state yet.
    pub fn render_capsule(&self) -> Option<String> {
        render_capsule_with_limits(self, DEFAULT_MAX_CAPSULE_TOKENS, DEFAULT_MAX_FIELD_CHARS)
    }

    /// Merges non-empty facts from `other`, preserving existing list order.
    pub fn merge_from(&mut self, other: &Self) {
        merge_option(&mut self.active_goal, &other.active_goal);
        append_unique_values(&mut self.success_criteria, &other.success_criteria, None);
        append_unique_values(&mut self.current_progress, &other.current_progress, None);
        merge_option(&mut self.next_action, &other.next_action);
        append_unique_values(&mut self.files_touched, &other.files_touched, None);
        append_unique_values(&mut self.verified_evidence, &other.verified_evidence, None);
        append_unique_values(
            &mut self.active_background_jobs,
            &other.active_background_jobs,
            None,
        );
        append_unique_values(&mut self.open_blockers, &other.open_blockers, None);
    }

    fn merged_with_goal(&self, goal: &ThreadGoal) -> Self {
        let mut state = self.clone();
        state.active_goal = Some(goal.objective.clone());
        prepend_unique(
            &mut state.current_progress,
            format!("Goal status: {}", goal.status.as_str()),
        );
        prepend_unique(&mut state.current_progress, format_goal_usage(goal));
        match goal.status {
            ThreadGoalStatus::Blocked => push_unique(
                &mut state.open_blockers,
                "Goal is marked blocked; do not mark it complete until the blocker is resolved.",
            ),
            ThreadGoalStatus::Paused => push_unique(
                &mut state.open_blockers,
                "Goal is paused; wait for an explicit resume before continuing autonomous work.",
            ),
            ThreadGoalStatus::UsageLimited => push_unique(
                &mut state.open_blockers,
                "Goal is usage-limited; do not continue autonomous work without user direction.",
            ),
            ThreadGoalStatus::BudgetLimited => push_unique(
                &mut state.open_blockers,
                "Goal reached its token budget; do not continue autonomous work without user direction.",
            ),
            ThreadGoalStatus::Active | ThreadGoalStatus::Complete => {}
        }
        state
    }
}

/// Thread-scoped mutable Link context state maintained by lifecycle hooks.
#[derive(Debug, Default)]
pub struct LinkContextStore {
    state: Mutex<LinkContextState>,
    background_jobs: Mutex<BTreeMap<String, NativeBackgroundJob>>,
    pending_background_events: Mutex<VecDeque<BackgroundTriggerEvent>>,
    background_event_keys: Mutex<VecDeque<String>>,
}

impl LinkContextStore {
    /// Returns a point-in-time copy of the recorded Link context.
    pub fn snapshot(&self) -> LinkContextState {
        self.state().clone()
    }

    /// Applies an in-place update to the recorded Link context.
    pub fn update(&self, update: impl FnOnce(&mut LinkContextState)) {
        let mut state = self.state();
        update(&mut state);
    }

    fn record_tool_finish(&self, input: &ToolFinishInput<'_>) {
        let event = format_tool_finish_event(input);
        let background_job = native_background_job_from_tool_finish(input);
        let background_update = background_job.map(|job| self.upsert_background_job(job));
        if let Some((_old_summary, background_job)) = background_update.as_ref() {
            self.refresh_pending_background_events_for_job(background_job);
        }
        self.update(|state| {
            push_unique_capped(
                &mut state.verified_evidence,
                event,
                MAX_RECORDED_TOOL_EVENTS,
            );
            if let Some((old_summary, background_job)) = background_update.as_ref() {
                if let Some(old_summary) = old_summary {
                    state
                        .active_background_jobs
                        .retain(|existing| existing != old_summary);
                }
                let background_summary = background_job.summary();
                push_unique(&mut state.active_background_jobs, &background_summary);
                push_unique_capped(
                    &mut state.verified_evidence,
                    format!("Native background exec started: {background_summary}"),
                    MAX_RECORDED_TOOL_EVENTS,
                );
            }
        });
    }

    fn record_turn_event(&self, event: &EventMsg) {
        match event {
            EventMsg::ExecCommandBegin(event) => self.record_exec_command_begin(event),
            EventMsg::ExecCommandEnd(event) => self.record_exec_command_end(event),
            EventMsg::ExecBackgroundTrigger(event) => self.record_background_trigger(event),
            _ => {}
        }
    }

    fn record_exec_command_begin(&self, event: &ExecCommandBeginEvent) {
        let Some(job) = NativeBackgroundJob::from_exec_begin(event) else {
            return;
        };
        let (old_summary, job) = self.upsert_background_job(job);
        self.refresh_pending_background_events_for_job(&job);
        let summary = job.summary();
        self.update(|state| {
            if let Some(old_summary) = old_summary {
                state
                    .active_background_jobs
                    .retain(|existing| existing != &old_summary);
            }
            push_unique(&mut state.active_background_jobs, &summary);
            push_unique_capped(
                &mut state.verified_evidence,
                format!("Native background exec registered: {summary}"),
                MAX_RECORDED_TOOL_EVENTS,
            );
        });
    }

    fn record_exec_command_end(&self, event: &ExecCommandEndEvent) {
        let Some(process_id) = event.process_id.as_deref() else {
            return;
        };
        let Some(job) = self.background_jobs().remove(process_id) else {
            return;
        };

        let summary = job.summary();
        let exit_evidence = format!(
            "Native background exec session {} ended with exit_code={} status={:?}: {}",
            job.process_id,
            event.exit_code,
            event.status,
            truncate_chars(&job.description, MAX_TOOL_OUTPUT_PREVIEW_CHARS)
        );
        self.update(|state| {
            state
                .active_background_jobs
                .retain(|existing| existing != &summary);
            push_unique_capped(
                &mut state.verified_evidence,
                exit_evidence,
                MAX_RECORDED_TOOL_EVENTS,
            );
        });

        if !job.should_trigger_on_exit(event) {
            return;
        }

        let trigger_event = BackgroundTriggerEvent::from_exec_end(job, event);
        self.enqueue_background_event(trigger_event);
    }

    fn record_background_trigger(&self, event: &ExecBackgroundTriggerEvent) {
        let job = self.background_jobs().get(&event.process_id).cloned();
        let description = job
            .as_ref()
            .map(|job| job.description.clone())
            .or_else(|| event.description.clone())
            .unwrap_or_else(|| "(no description recorded)".to_string());
        let triggers = job
            .as_ref()
            .map(|job| job.triggers.clone())
            .unwrap_or_else(|| event.declared_triggers.clone());
        let log_path = job.as_ref().and_then(|job| job.log_path.clone());

        let evidence = format!(
            "Native background exec session {} fired trigger `{}`: {}",
            event.process_id,
            event.trigger,
            truncate_chars(&event.reason, MAX_TOOL_OUTPUT_PREVIEW_CHARS)
        );
        self.update(|state| {
            push_unique_capped(
                &mut state.verified_evidence,
                evidence,
                MAX_RECORDED_TOOL_EVENTS,
            );
        });

        let trigger_event = BackgroundTriggerEvent::from_exec_background_trigger(
            event,
            description,
            triggers,
            log_path,
        );
        self.enqueue_background_event(trigger_event);
    }

    fn enqueue_background_event(&self, event: BackgroundTriggerEvent) {
        if self.merge_pending_background_event(&event) {
            return;
        }
        self.remove_pending_background_events_superseded_by(&event);

        if !self.record_background_event_key(&event.event_key) {
            return;
        }

        let mut pending = self.pending_background_events();
        pending.push_back(event);
        let overflow = pending.len().saturating_sub(MAX_PENDING_BACKGROUND_EVENTS);
        if overflow > 0 {
            pending.drain(0..overflow);
        }
    }

    fn record_background_event_key(&self, event_key: &str) -> bool {
        let mut keys = self.background_event_keys();
        if keys.iter().any(|existing| existing == event_key) {
            return false;
        }

        keys.push_back(event_key.to_string());
        let overflow = keys
            .len()
            .saturating_sub(MAX_RECORDED_BACKGROUND_EVENT_KEYS);
        if overflow > 0 {
            keys.drain(0..overflow);
        }
        true
    }

    fn merge_pending_background_event(&self, event: &BackgroundTriggerEvent) -> bool {
        let mut pending = self.pending_background_events();
        if let Some(existing) = pending
            .iter_mut()
            .find(|existing| existing.event_key == event.event_key)
        {
            existing.merge_missing_from(event.clone());
            true
        } else {
            false
        }
    }

    fn remove_pending_background_events_superseded_by(&self, event: &BackgroundTriggerEvent) {
        if !event.is_terminal_exit() {
            return;
        }
        let mut pending = self.pending_background_events();
        pending.retain(|existing| !event.supersedes_pending(existing));
    }

    fn refresh_pending_background_events_for_job(&self, job: &NativeBackgroundJob) {
        let mut pending = self.pending_background_events();
        for event in pending
            .iter_mut()
            .filter(|event| event.process_id == job.process_id)
        {
            event.merge_job_metadata(job);
        }
    }

    fn upsert_background_job(
        &self,
        job: NativeBackgroundJob,
    ) -> (Option<String>, NativeBackgroundJob) {
        let mut jobs = self.background_jobs();
        let old_summary = jobs.get(&job.process_id).map(NativeBackgroundJob::summary);
        if let Some(existing) = jobs.get_mut(&job.process_id) {
            existing.merge_from(job);
            return (old_summary, existing.clone());
        }

        jobs.insert(job.process_id.clone(), job.clone());
        (old_summary, job)
    }

    fn pop_pending_background_events(&self) -> Vec<BackgroundTriggerEvent> {
        let mut pending = self.pending_background_events();
        if pending.is_empty() {
            return Vec::new();
        }
        pending.drain(..).collect()
    }

    fn requeue_background_events_front(&self, events: Vec<BackgroundTriggerEvent>) {
        if events.is_empty() {
            return;
        }
        let mut pending = self.pending_background_events();
        for event in events.into_iter().rev() {
            if pending
                .iter()
                .any(|existing| existing.event_key == event.event_key)
            {
                continue;
            }
            pending.push_front(event);
        }
        pending.truncate(MAX_PENDING_BACKGROUND_EVENTS);
    }

    fn record_file_change(&self, item: &FileChangeItem) {
        let paths = file_change_paths(item);
        if paths.is_empty() {
            return;
        }

        let status = patch_status_label(item.status.as_ref());
        let file_count = paths.len();
        let evidence = format!(
            "Patch {status} touched {file_count} file(s): {}",
            truncate_chars(&paths.join(", "), MAX_TOOL_OUTPUT_PREVIEW_CHARS)
        );
        self.update(|state| {
            if matches!(item.status, Some(PatchApplyStatus::Completed)) {
                append_unique_values_with_char_limit(
                    &mut state.files_touched,
                    &paths,
                    Some(MAX_RECORDED_FILES_TOUCHED),
                    DEFAULT_MAX_FIELD_CHARS,
                );
            }
            push_unique_capped(
                &mut state.verified_evidence,
                evidence,
                MAX_RECORDED_TOOL_EVENTS,
            );
        });
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LinkContextState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn background_jobs(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, NativeBackgroundJob>> {
        self.background_jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn pending_background_events(
        &self,
    ) -> std::sync::MutexGuard<'_, VecDeque<BackgroundTriggerEvent>> {
        self.pending_background_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn background_event_keys(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        self.background_event_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeBackgroundJob {
    process_id: String,
    description: String,
    triggers: Vec<String>,
    command: Option<String>,
    cwd: Option<String>,
    started_at_ms: Option<i64>,
    log_path: Option<String>,
}

impl NativeBackgroundJob {
    fn from_exec_begin(event: &ExecCommandBeginEvent) -> Option<Self> {
        let process_id = event.process_id.clone()?;
        let has_background_metadata =
            event.background_description.is_some() || !event.background_triggers.is_empty();
        if !has_background_metadata {
            return None;
        }

        Some(Self {
            process_id,
            description: event
                .background_description
                .clone()
                .unwrap_or_else(|| "(no description recorded)".to_string()),
            triggers: event.background_triggers.clone(),
            command: Some(event.command.join(" ")),
            cwd: Some(event.cwd.to_string()),
            started_at_ms: Some(event.started_at_ms),
            log_path: None,
        })
    }

    fn summary(&self) -> String {
        let triggers = if self.triggers.is_empty() {
            String::new()
        } else {
            format!("; triggers: {}", self.triggers.join(", "))
        };
        let command = self
            .command
            .as_ref()
            .map(|command| format!("; command: {command}"))
            .unwrap_or_default();
        let cwd = self
            .cwd
            .as_ref()
            .map(|cwd| format!("; cwd: {cwd}"))
            .unwrap_or_default();
        let log_path = self
            .log_path
            .as_ref()
            .map(|log_path| format!("; log: {log_path}"))
            .unwrap_or_default();
        truncate_chars(
            &format!(
                "exec session {} running: {}{}{}{}{}",
                self.process_id, self.description, command, cwd, log_path, triggers
            ),
            MAX_TOOL_EVENT_CHARS,
        )
    }

    fn merge_from(&mut self, other: Self) {
        if self.description == "(no description recorded)" {
            self.description = other.description;
        }
        append_unique_values(&mut self.triggers, &other.triggers, None);
        if self.command.is_none() {
            self.command = other.command;
        }
        if self.cwd.is_none() {
            self.cwd = other.cwd;
        }
        if self.started_at_ms.is_none() {
            self.started_at_ms = other.started_at_ms;
        }
        if self.log_path.is_none() {
            self.log_path = other.log_path;
        }
    }

    fn should_trigger_on_exit(&self, event: &ExecCommandEndEvent) -> bool {
        event.exit_code != 0
            || matches!(
                event.status,
                ExecCommandStatus::Failed | ExecCommandStatus::Declined
            )
            || self
                .triggers
                .iter()
                .any(|trigger| trigger.eq_ignore_ascii_case("on_exit"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BackgroundTriggerEvent {
    event_key: String,
    process_id: String,
    trigger: String,
    description: String,
    triggers: Vec<String>,
    command: String,
    cwd: String,
    exit_code: String,
    status: String,
    tail: String,
    log_path: Option<String>,
}

impl BackgroundTriggerEvent {
    fn from_exec_end(job: NativeBackgroundJob, event: &ExecCommandEndEvent) -> Self {
        let trigger = if event.exit_code == 0 && event.status == ExecCommandStatus::Completed {
            "on_exit"
        } else {
            "failure_exit"
        };
        Self {
            event_key: format!("exec:{}:exit:{}", job.process_id, event.exit_code),
            process_id: job.process_id,
            trigger: trigger.to_string(),
            description: job.description,
            triggers: job.triggers,
            command: event.command.join(" "),
            cwd: event.cwd.to_string(),
            exit_code: event.exit_code.to_string(),
            status: format!("{:?}", event.status),
            tail: truncate_chars(&event.aggregated_output, MAX_BACKGROUND_EVENT_TAIL_CHARS),
            log_path: job.log_path,
        }
    }

    fn from_exec_background_trigger(
        event: &ExecBackgroundTriggerEvent,
        description: String,
        triggers: Vec<String>,
        log_path: Option<String>,
    ) -> Self {
        let terminal = terminal_status_from_background_trigger(&event.trigger, &event.reason);
        let (event_key, exit_code, status) = if let Some((exit_code, status)) = terminal {
            (
                format!("exec:{}:exit:{exit_code}", event.process_id),
                exit_code,
                status,
            )
        } else {
            (
                format!(
                    "exec:{}:background_trigger:{}",
                    event.process_id, event.trigger
                ),
                "still running".to_string(),
                "running".to_string(),
            )
        };

        Self {
            event_key,
            process_id: event.process_id.clone(),
            trigger: event.trigger.clone(),
            description,
            triggers,
            command: event.command.join(" "),
            cwd: event.cwd.to_string(),
            exit_code,
            status,
            tail: truncate_chars(&event.output_tail, MAX_BACKGROUND_EVENT_TAIL_CHARS),
            log_path,
        }
    }

    fn merge_missing_from(&mut self, other: Self) {
        if self.description == "(no description recorded)" {
            self.description = other.description;
        }
        append_unique_values(&mut self.triggers, &other.triggers, None);
        if self.command.is_empty() {
            self.command = other.command;
        }
        if self.cwd.is_empty() {
            self.cwd = other.cwd;
        }
        if self.exit_code == "still running" && other.exit_code != "still running" {
            self.exit_code = other.exit_code;
        }
        if self.status == "running" && other.status != "running" {
            self.status = other.status;
        }
        if self.tail.is_empty() {
            self.tail = other.tail;
        }
        if self.log_path.is_none() {
            self.log_path = other.log_path;
        }
    }

    fn merge_job_metadata(&mut self, job: &NativeBackgroundJob) {
        if self.description == "(no description recorded)" {
            self.description = job.description.clone();
        }
        append_unique_values(&mut self.triggers, &job.triggers, None);
        if self.command.is_empty()
            && let Some(command) = &job.command
        {
            self.command = command.clone();
        }
        if self.cwd.is_empty()
            && let Some(cwd) = &job.cwd
        {
            self.cwd = cwd.clone();
        }
        if self.log_path.is_none() {
            self.log_path = job.log_path.clone();
        }
    }

    fn is_terminal_exit(&self) -> bool {
        self.is_exit_trigger() && self.exit_code != "still running"
    }

    fn is_exit_trigger(&self) -> bool {
        self.trigger.eq_ignore_ascii_case("on_exit")
            || self.trigger.eq_ignore_ascii_case("failure_exit")
    }

    fn supersedes_pending(&self, existing: &Self) -> bool {
        self.process_id == existing.process_id
            && self.is_terminal_exit()
            && existing.is_exit_trigger()
    }

    fn render(&self) -> String {
        let triggers = if self.triggers.is_empty() {
            "(none recorded)".to_string()
        } else {
            self.triggers.join(", ")
        };
        format!(
            "<codex_link_background_event>\n\
             Process: {}\n\
             Trigger: {}\n\
             Description: {}\n\
             Declared triggers: {}\n\
             Command: {}\n\
             CWD: {}\n\
             Log: {}\n\
             Exit code: {}\n\
             Status: {}\n\
             Output tail:\n{}\n\
             Instruction: This background event has already fired. Do at most one follow-up pass: inspect the result, update the plan or files if needed, and do not start another wait/poll loop unless you deliberately launch a new background command with fresh triggers.\n\
             </codex_link_background_event>",
            sanitize_for_event(&self.process_id),
            sanitize_for_event(&self.trigger),
            sanitize_for_event(&self.description),
            sanitize_for_event(&triggers),
            sanitize_for_event(&self.command),
            sanitize_for_event(&self.cwd),
            sanitize_for_event(self.log_path.as_deref().unwrap_or("(none recorded)")),
            sanitize_for_event(&self.exit_code),
            sanitize_for_event(&self.status),
            sanitize_for_event(&self.tail),
        )
    }
}

fn terminal_status_from_background_trigger(
    trigger: &str,
    reason: &str,
) -> Option<(String, String)> {
    if trigger.eq_ignore_ascii_case("on_exit") {
        let exit_code = reason
            .strip_prefix("process exited with code ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("0")
            .to_string();
        return Some((exit_code, format!("{:?}", ExecCommandStatus::Completed)));
    }

    if trigger.eq_ignore_ascii_case("failure_exit") {
        return Some((
            "failed".to_string(),
            format!("{:?}", ExecCommandStatus::Failed),
        ));
    }

    None
}

#[derive(Clone)]
struct LinkBackgroundRuntime {
    thread_id: ThreadId,
    thread_manager: Weak<ThreadManager>,
    context_store: Arc<LinkContextStore>,
}

impl LinkBackgroundRuntime {
    fn new(
        thread_id: ThreadId,
        thread_manager: Weak<ThreadManager>,
        context_store: Arc<LinkContextStore>,
    ) -> Self {
        Self {
            thread_id,
            thread_manager,
            context_store,
        }
    }

    async fn start_pending_turn_if_idle(&self) -> Result<(), String> {
        let events = self.context_store.pop_pending_background_events();
        if events.is_empty() {
            return Ok(());
        }

        let Some(thread_manager) = self.thread_manager.upgrade() else {
            self.context_store.requeue_background_events_front(events);
            return Ok(());
        };
        let thread = match thread_manager.get_thread(self.thread_id).await {
            Ok(thread) => thread,
            Err(_) => {
                self.context_store.requeue_background_events_front(events);
                return Ok(());
            }
        };

        let item = background_event_steering_item(&events);
        if let Err(err) = thread.try_start_turn_if_idle(vec![item]).await {
            let reason = err.reason();
            tracing::debug!(
                ?reason,
                "skipping Link background callback because automatic idle work was rejected"
            );
            self.context_store.requeue_background_events_front(events);
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
struct LinkContextExtension {
    goal_state: Option<Arc<codex_state::StateRuntime>>,
    thread_manager: Option<Weak<ThreadManager>>,
}

impl ContextContributor for LinkContextExtension {
    fn contribute_thread_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(std::future::ready(Vec::new()))
    }

    fn contribute_turn_context<'a>(
        &'a self,
        input: TurnContextContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            let mut state = input
                .thread_store
                .get::<LinkContextState>()
                .as_deref()
                .cloned()
                .unwrap_or_default();
            if let Some(store) = input.thread_store.get::<LinkContextStore>() {
                state.merge_from(&store.snapshot());
            }
            if let Some(turn_state) = input.turn_store.get::<LinkContextState>() {
                state.merge_from(&turn_state);
            }
            if let Some(goal) = self.current_goal(input.thread_id).await {
                state = state.merged_with_goal(&goal);
            }
            state
                .render_capsule()
                .map(|capsule| PromptFragment::new(PromptSlot::ContextualUser, capsule))
                .into_iter()
                .collect()
        })
    }
}

impl ThreadLifecycleContributor<codex_core::config::Config> for LinkContextExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, codex_core::config::Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.ensure_background_runtime(input.thread_store);
        })
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.ensure_background_runtime(input.thread_store);
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = input.thread_store.get::<LinkBackgroundRuntime>() else {
                return;
            };
            if let Err(err) = runtime.start_pending_turn_if_idle().await {
                tracing::warn!("failed to start Link background callback turn: {err}");
            }
        })
    }
}

impl ToolLifecycleContributor for LinkContextExtension {
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        let store = input.thread_store.get_or_init(LinkContextStore::default);
        Box::pin(async move {
            store.record_tool_finish(&input);
        })
    }
}

impl TurnItemContributor for LinkContextExtension {
    fn contribute<'a>(
        &'a self,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> ExtensionFuture<'a, Result<(), String>> {
        let store = thread_store.get_or_init(LinkContextStore::default);
        Box::pin(async move {
            if let TurnItem::FileChange(item) = item {
                store.record_file_change(item);
            }
            Ok(())
        })
    }
}

impl TurnEventContributor for LinkContextExtension {
    fn on_turn_event<'a>(&'a self, input: TurnEventInput<'a>) -> TurnEventFuture<'a> {
        Box::pin(async move {
            let store = input.thread_store.get_or_init(LinkContextStore::default);
            store.record_turn_event(input.event);
            if matches!(
                input.event,
                EventMsg::ExecBackgroundTrigger(_) | EventMsg::ExecCommandEnd(_)
            ) {
                self.ensure_background_runtime(input.thread_store);
                if let Some(runtime) = input.thread_store.get::<LinkBackgroundRuntime>()
                    && let Err(err) = runtime.start_pending_turn_if_idle().await
                {
                    tracing::warn!("failed to start Link background callback turn: {err}");
                }
            }
        })
    }
}

impl LinkContextExtension {
    async fn current_goal(&self, thread_id: codex_protocol::ThreadId) -> Option<ThreadGoal> {
        let goal_state = self.goal_state.as_ref()?;
        let goal = goal_state
            .thread_goals()
            .get_thread_goal(thread_id)
            .await
            .ok()??;
        should_include_goal_in_capsule(goal.status).then_some(goal)
    }

    fn ensure_background_runtime(&self, thread_store: &ExtensionData) {
        let Some(thread_manager) = self.thread_manager.as_ref() else {
            return;
        };
        let Ok(thread_id) = ThreadId::from_string(thread_store.level_id()) else {
            return;
        };
        let context_store = thread_store.get_or_init(LinkContextStore::default);
        thread_store.get_or_init::<LinkBackgroundRuntime>(|| {
            LinkBackgroundRuntime::new(thread_id, thread_manager.clone(), context_store)
        });
    }
}

/// Installs the Link context capsule contributor.
pub fn install<C: Sync>(registry: &mut ExtensionRegistryBuilder<C>) {
    let extension = Arc::new(LinkContextExtension::default());
    registry.prompt_contributor(extension.clone());
    registry.turn_item_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension.clone());
    registry.turn_event_contributor(extension);
}

/// Installs the Link context capsule contributor with read access to goal state.
pub fn install_with_goal_state<C: Sync>(
    registry: &mut ExtensionRegistryBuilder<C>,
    goal_state: Arc<codex_state::StateRuntime>,
) {
    let extension = Arc::new(LinkContextExtension {
        goal_state: Some(goal_state),
        thread_manager: None,
    });
    registry.prompt_contributor(extension.clone());
    registry.turn_item_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension.clone());
    registry.turn_event_contributor(extension);
}

/// Installs the Link context contributor with goal state and background callback support.
pub fn install_with_goal_state_and_thread_manager(
    registry: &mut ExtensionRegistryBuilder<codex_core::config::Config>,
    goal_state: Arc<codex_state::StateRuntime>,
    thread_manager: Weak<ThreadManager>,
) {
    let extension = Arc::new(LinkContextExtension {
        goal_state: Some(goal_state),
        thread_manager: Some(thread_manager),
    });
    registry.prompt_contributor(extension.clone());
    registry.turn_item_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension.clone());
    registry.turn_event_contributor(extension.clone());
    registry.thread_lifecycle_contributor(extension);
}

/// Installs the Link context contributor with background callback support only.
pub fn install_with_thread_manager(
    registry: &mut ExtensionRegistryBuilder<codex_core::config::Config>,
    thread_manager: Weak<ThreadManager>,
) {
    let extension = Arc::new(LinkContextExtension {
        goal_state: None,
        thread_manager: Some(thread_manager),
    });
    registry.prompt_contributor(extension.clone());
    registry.turn_item_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension.clone());
    registry.turn_event_contributor(extension.clone());
    registry.thread_lifecycle_contributor(extension);
}

fn render_capsule_with_limits(
    state: &LinkContextState,
    max_tokens: usize,
    max_field_chars: usize,
) -> Option<String> {
    if state.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str(CAPSULE_START);
    out.push('\n');
    render_single_section(
        &mut out,
        "Active goal",
        state.active_goal.as_deref(),
        max_field_chars,
    );
    render_list_section(
        &mut out,
        "Success criteria",
        &state.success_criteria,
        max_field_chars,
    );
    render_list_section(
        &mut out,
        "Current progress",
        &state.current_progress,
        max_field_chars,
    );
    render_single_section(
        &mut out,
        "Next action",
        state.next_action.as_deref(),
        max_field_chars,
    );
    render_list_section(
        &mut out,
        "Files touched",
        &state.files_touched,
        max_field_chars,
    );
    render_list_section(
        &mut out,
        "Verified evidence",
        &state.verified_evidence,
        max_field_chars,
    );
    render_list_section(
        &mut out,
        "Background jobs",
        &state.active_background_jobs,
        max_field_chars,
    );
    render_list_section(
        &mut out,
        "Open blockers",
        &state.open_blockers,
        max_field_chars,
    );
    out.push_str(CAPSULE_END);

    if approx_token_count(&out) > max_tokens {
        let body_budget = max_tokens.saturating_sub(64).max(1);
        let body = out
            .strip_prefix(CAPSULE_START)
            .and_then(|body| body.strip_suffix(CAPSULE_END))
            .unwrap_or(out.as_str());
        let truncated = truncate_text(body.trim(), TruncationPolicy::Tokens(body_budget));
        out = format!("{CAPSULE_START}\n{truncated}\n{CAPSULE_END}");
    }

    Some(out)
}

fn render_single_section(out: &mut String, heading: &str, value: Option<&str>, max_chars: usize) {
    out.push_str(heading);
    out.push_str(":\n");
    match value.and_then(|value| sanitize_value(value, max_chars)) {
        Some(value) => render_bullet(out, &value),
        None => render_bullet(out, "(none recorded)"),
    }
}

fn render_list_section(out: &mut String, heading: &str, values: &[String], max_chars: usize) {
    out.push_str(heading);
    out.push_str(":\n");
    let mut rendered = false;
    for value in values
        .iter()
        .filter_map(|value| sanitize_value(value, max_chars))
    {
        rendered = true;
        render_bullet(out, &value);
    }
    if !rendered {
        render_bullet(out, "(none recorded)");
    }
}

fn render_bullet(out: &mut String, value: &str) {
    out.push_str("- ");
    out.push_str(value);
    out.push('\n');
}

fn sanitize_value(value: &str, max_chars: usize) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let value = value
        .replace(CAPSULE_START, "[codex_link_context_capsule]")
        .replace(CAPSULE_END, "[/codex_link_context_capsule]");
    Some(truncate_chars(&value, max_chars))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }

    let keep = max_chars.saturating_sub(16).max(1);
    let prefix = value.chars().take(keep).collect::<String>();
    format!("{prefix} ... [truncated]")
}

fn should_include_goal_in_capsule(status: ThreadGoalStatus) -> bool {
    !matches!(status, ThreadGoalStatus::Complete)
}

fn format_tool_finish_event(input: &ToolFinishInput<'_>) -> String {
    let outcome = match input.outcome {
        ToolCallOutcome::Completed { success: true } => "completed successfully".to_string(),
        ToolCallOutcome::Completed { success: false } => {
            "completed with a reported failure".to_string()
        }
        ToolCallOutcome::Blocked => "was blocked by host policy".to_string(),
        ToolCallOutcome::Failed {
            handler_executed: true,
        } => "failed after its handler started".to_string(),
        ToolCallOutcome::Failed {
            handler_executed: false,
        } => "failed before its handler started".to_string(),
        ToolCallOutcome::Aborted => "was aborted".to_string(),
    };
    let source = format_tool_source(&input.source);
    let output_preview = input
        .output_preview
        .and_then(|preview| sanitize_value(preview, MAX_TOOL_OUTPUT_PREVIEW_CHARS))
        .map(|preview| format!(". Output preview: {preview}"))
        .unwrap_or_default();
    truncate_chars(
        &format!(
            "Tool {} {}{} (turn {}, call {}){}",
            input.tool_name, outcome, source, input.turn_id, input.call_id, output_preview
        ),
        MAX_TOOL_EVENT_CHARS,
    )
}

fn native_background_job_from_tool_finish(
    input: &ToolFinishInput<'_>,
) -> Option<NativeBackgroundJob> {
    if input.tool_name.namespace.is_some() || input.tool_name.name != "exec_command" {
        return None;
    }
    let preview = input.output_preview?;
    let header = unified_exec_output_header(preview)?;
    if !header.lines().any(|line| line == "Background mode: true") {
        return None;
    }
    let process_id = extract_header_line_suffix(header, "Process running with session ID ")?;
    let description = extract_header_line_suffix(header, "Background description: ")
        .unwrap_or_else(|| "(no description recorded)".to_string());
    let triggers = extract_header_line_suffix(header, "Background triggers: ")
        .map(|triggers| {
            triggers
                .split(',')
                .filter_map(|trigger| sanitize_value(trigger, MAX_TOOL_OUTPUT_PREVIEW_CHARS))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let log_path = extract_header_line_suffix(header, "Background log: ");
    Some(NativeBackgroundJob {
        process_id,
        description,
        triggers,
        command: None,
        cwd: None,
        started_at_ms: None,
        log_path,
    })
}

fn unified_exec_output_header(preview: &str) -> Option<&str> {
    if !preview.starts_with("Chunk ID: ") {
        return None;
    }
    let header_end = preview.find("\nOutput:").unwrap_or(preview.len());
    Some(&preview[..header_end])
}

fn extract_header_line_suffix(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .and_then(|value| sanitize_value(value, MAX_TOOL_OUTPUT_PREVIEW_CHARS))
}

fn background_event_steering_item(events: &[BackgroundTriggerEvent]) -> ResponseItem {
    let body = events
        .iter()
        .map(BackgroundTriggerEvent::render)
        .collect::<Vec<_>>()
        .join("\n\n");
    ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("link_background"),
        body,
    ))
}

fn sanitize_for_event(value: &str) -> String {
    value
        .replace(
            "<codex_link_background_event>",
            "[codex_link_background_event]",
        )
        .replace(
            "</codex_link_background_event>",
            "[/codex_link_background_event]",
        )
        .replace("<codex_internal_context", "[codex_internal_context")
        .replace("</codex_internal_context>", "[/codex_internal_context]")
}

fn format_tool_source(source: &ToolCallSource) -> String {
    match source {
        ToolCallSource::Direct => String::new(),
        ToolCallSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
        } => format!(" via code cell {cell_id}, runtime call {runtime_tool_call_id}"),
    }
}

fn file_change_paths(item: &FileChangeItem) -> Vec<String> {
    let mut paths = Vec::new();
    for (path, change) in &item.changes {
        paths.push(path.display().to_string());
        if let FileChange::Update {
            move_path: Some(move_path),
            ..
        } = change
        {
            paths.push(move_path.display().to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn patch_status_label(status: Option<&PatchApplyStatus>) -> &'static str {
    match status {
        Some(PatchApplyStatus::Completed) => "completed",
        Some(PatchApplyStatus::Failed) => "failed",
        Some(PatchApplyStatus::Declined) => "declined",
        None => "started",
    }
}

fn format_goal_usage(goal: &ThreadGoal) -> String {
    match goal.token_budget {
        Some(token_budget) => format!(
            "Goal usage: {} / {} tokens, {} seconds",
            goal.tokens_used, token_budget, goal.time_used_seconds
        ),
        None => format!(
            "Goal usage: {} tokens, {} seconds",
            goal.tokens_used, goal.time_used_seconds
        ),
    }
}

fn prepend_unique(values: &mut Vec<String>, value: String) {
    if values.iter().any(|existing| existing == &value) {
        return;
    }
    values.insert(0, value);
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if values.iter().any(|existing| existing == value) {
        return;
    }
    values.push(value.to_string());
}

fn push_unique_capped(values: &mut Vec<String>, value: String, max_len: usize) {
    append_unique_values_with_char_limit(values, &[value], Some(max_len), MAX_TOOL_EVENT_CHARS);
}

fn append_unique_values(values: &mut Vec<String>, additions: &[String], max_len: Option<usize>) {
    append_unique_values_with_char_limit(values, additions, max_len, usize::MAX);
}

fn append_unique_values_with_char_limit(
    values: &mut Vec<String>,
    additions: &[String],
    max_len: Option<usize>,
    max_chars: usize,
) {
    for value in additions {
        let Some(value) = sanitize_value(value, max_chars) else {
            continue;
        };
        if !values.iter().any(|existing| existing == &value) {
            values.push(value);
        }
    }
    if let Some(max_len) = max_len {
        let overflow = values.len().saturating_sub(max_len);
        if overflow > 0 {
            values.drain(0..overflow);
        }
    }
}

fn merge_option(target: &mut Option<String>, source: &Option<String>) {
    let Some(value) = source
        .as_deref()
        .and_then(|value| sanitize_value(value, usize::MAX))
    else {
        return;
    };
    *target = Some(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_extension_api::ToolCallOutcome;
    use codex_extension_api::ToolCallSource;
    use codex_extension_api::ToolFinishInput;
    use codex_extension_api::ToolLifecycleContributor;
    use codex_extension_api::ToolName;
    use codex_extension_api::TurnContextContributionInput;
    use codex_extension_api::TurnItemContributor;
    use codex_protocol::items::FileChangeItem;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::ContentItem;
    use codex_protocol::parse_command::ParsedCommand;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecCommandBeginEvent;
    use codex_protocol::protocol::ExecCommandEndEvent;
    use codex_protocol::protocol::ExecCommandSource;
    use codex_protocol::protocol::ExecCommandStatus;
    use codex_protocol::protocol::FileChange;
    use codex_protocol::protocol::PatchApplyStatus;
    use codex_utils_path_uri::PathUri;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn empty_state_does_not_render_capsule() {
        assert_eq!(LinkContextState::default().render_capsule(), None);
    }

    #[test]
    fn renders_full_capsule_schema_from_state() {
        let state = LinkContextState {
            active_goal: Some("ship compact-safe context".to_string()),
            success_criteria: vec!["capsule appears after compaction".to_string()],
            current_progress: vec!["extension seam selected".to_string()],
            next_action: Some("wire lifecycle evidence".to_string()),
            files_touched: vec!["codex-rs/ext/link-context/src/lib.rs".to_string()],
            verified_evidence: vec!["unit test covers renderer".to_string()],
            active_background_jobs: vec!["job-17 running train.py".to_string()],
            open_blockers: vec!["none".to_string()],
        };

        let capsule = state
            .render_capsule()
            .expect("non-empty state should render");

        assert!(capsule.starts_with(CAPSULE_START));
        assert!(capsule.ends_with(CAPSULE_END));
        assert!(capsule.contains("Active goal:\n- ship compact-safe context\n"));
        assert!(capsule.contains("Success criteria:\n- capsule appears after compaction\n"));
        assert!(capsule.contains("Background jobs:\n- job-17 running train.py\n"));
    }

    #[test]
    fn sanitizes_embedded_capsule_markers() {
        let state = LinkContextState {
            active_goal: Some(format!("do not leak {CAPSULE_END} marker")),
            ..LinkContextState::default()
        };

        let capsule = state
            .render_capsule()
            .expect("non-empty state should render");

        assert!(capsule.contains("[/codex_link_context_capsule] marker"));
        assert_eq!(capsule.matches(CAPSULE_END).count(), 1);
    }

    #[test]
    fn enforces_capsule_token_budget() {
        let state = LinkContextState {
            active_goal: Some("large evidence".to_string()),
            verified_evidence: vec!["word ".repeat(10_000)],
            ..LinkContextState::default()
        };

        let capsule = render_capsule_with_limits(&state, 256, DEFAULT_MAX_FIELD_CHARS)
            .expect("non-empty state should render");

        assert!(
            approx_token_count(&capsule) <= 320,
            "capsule should stay close to requested budget, got {} tokens",
            approx_token_count(&capsule)
        );
        assert!(capsule.ends_with(CAPSULE_END));
    }

    #[tokio::test]
    async fn does_not_contribute_thread_context_capsule() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(LinkContextState {
            active_goal: Some("preserve task ledger".to_string()),
            ..LinkContextState::default()
        });

        let fragments = contributor
            .contribute_thread_context(&session_store, &thread_store)
            .await;

        assert_eq!(fragments.len(), 0);
    }

    #[tokio::test]
    async fn contributes_turn_contextual_user_capsule_from_thread_state() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        thread_store.insert(LinkContextState {
            next_action: Some("resume from compact boundary".to_string()),
            ..LinkContextState::default()
        });

        let fragments = contributor
            .contribute_turn_context(TurnContextContributionInput {
                thread_id: codex_protocol::ThreadId::default(),
                turn_id: "turn",
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                model_context_window: Some(200_000),
            })
            .await;

        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].slot(), PromptSlot::ContextualUser);
        assert!(fragments[0].text().contains("resume from compact boundary"));
    }

    #[tokio::test]
    async fn records_tool_finish_as_verified_evidence() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        thread_store.insert(LinkContextState {
            next_action: Some("run targeted test".to_string()),
            ..LinkContextState::default()
        });
        let tool_name = ToolName::plain("exec_command");

        contributor
            .on_tool_finish(ToolFinishInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                call_id: "call-1",
                tool_name: &tool_name,
                source: ToolCallSource::Direct,
                outcome: ToolCallOutcome::Completed { success: true },
                output_preview: Some("ok: all targeted tests passed"),
            })
            .await;

        let fragments = contributor
            .contribute_turn_context(TurnContextContributionInput {
                thread_id: codex_protocol::ThreadId::default(),
                turn_id: "turn-2",
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                model_context_window: Some(200_000),
            })
            .await;

        assert_eq!(fragments.len(), 1);
        let capsule = fragments[0].text();
        assert!(capsule.contains("Next action:\n- run targeted test\n"));
        assert!(capsule.contains("Verified evidence:\n- Tool exec_command completed successfully"));
        assert!(capsule.contains("(turn turn-1, call call-1)"));
        assert!(capsule.contains("Output preview: ok: all targeted tests passed"));
    }

    #[tokio::test]
    async fn caps_recorded_tool_evidence() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let tool_name = ToolName::plain("exec_command");

        for index in 0..(MAX_RECORDED_TOOL_EVENTS + 3) {
            let call_id = format!("call-{index:02}");
            contributor
                .on_tool_finish(ToolFinishInput {
                    session_store: &session_store,
                    thread_store: &thread_store,
                    turn_store: &turn_store,
                    turn_id: "turn-1",
                    call_id: &call_id,
                    tool_name: &tool_name,
                    source: ToolCallSource::Direct,
                    outcome: ToolCallOutcome::Completed { success: true },
                    output_preview: Some("ok"),
                })
                .await;
        }

        let store = thread_store
            .get::<LinkContextStore>()
            .expect("tool finish should initialize Link context store");
        let state = store.snapshot();

        assert_eq!(state.verified_evidence.len(), MAX_RECORDED_TOOL_EVENTS);
        assert!(
            !state
                .verified_evidence
                .iter()
                .any(|value| value.contains("call-00"))
        );
        assert!(
            state
                .verified_evidence
                .iter()
                .any(|value| value.contains("call-18"))
        );
    }

    #[tokio::test]
    async fn records_completed_file_change_as_files_touched() {
        let contributor = LinkContextExtension::default();
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("src/lib.rs"),
            FileChange::Update {
                unified_diff: "@@ -1 +1 @@".to_string(),
                move_path: Some(PathBuf::from("src/link.rs")),
            },
        );
        changes.insert(
            PathBuf::from("README.md"),
            FileChange::Add {
                content: "hello".to_string(),
            },
        );
        let mut item = TurnItem::FileChange(FileChangeItem {
            id: "call-1".to_string(),
            changes,
            status: Some(PatchApplyStatus::Completed),
            auto_approved: None,
            stdout: None,
            stderr: None,
        });

        contributor
            .contribute(&thread_store, &turn_store, &mut item)
            .await
            .expect("file change contributor should succeed");

        let state = thread_store
            .get::<LinkContextStore>()
            .expect("file change should initialize Link context store")
            .snapshot();

        assert_eq!(
            state.files_touched,
            vec![
                "README.md".to_string(),
                "src/lib.rs".to_string(),
                "src/link.rs".to_string()
            ]
        );
        assert_eq!(
            state.verified_evidence,
            vec!["Patch completed touched 3 file(s): README.md, src/lib.rs, src/link.rs"]
        );
    }

    #[tokio::test]
    async fn records_native_exec_background_metadata_from_tool_output() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let tool_name = ToolName::plain("exec_command");

        contributor
            .on_tool_finish(ToolFinishInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                call_id: "call-bg",
                tool_name: &tool_name,
                source: ToolCallSource::Direct,
                outcome: ToolCallOutcome::Completed { success: true },
                output_preview: Some(
                    "Chunk ID: abc123\n\
                     Wall time: 1.0000 seconds\n\
                     Background mode: true\n\
                     Background description: train model until val_loss < 0.30\n\
                     Background triggers: on_exit, metric_plateau=10\n\
                     Background log: /tmp/codex-link/jobs/exec-1234.log\n\
                     Process running with session ID 1234\n\
                     Output:\nstarted",
                ),
            })
            .await;

        let state = thread_store
            .get::<LinkContextStore>()
            .expect("tool finish should initialize Link context store")
            .snapshot();

        assert_eq!(
            state.active_background_jobs,
            vec![
                "exec session 1234 running: train model until val_loss < 0.30; log: /tmp/codex-link/jobs/exec-1234.log; triggers: on_exit, metric_plateau=10"
                    .to_string()
            ]
        );
        assert!(
            state
                .verified_evidence
                .iter()
                .any(|value| value.contains("Native background exec started: exec session 1234"))
        );
    }

    #[tokio::test]
    async fn ignores_background_fixture_text_from_command_output() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let tool_name = ToolName::plain("exec_command");

        contributor
            .on_tool_finish(ToolFinishInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                call_id: "call-sed",
                tool_name: &tool_name,
                source: ToolCallSource::Direct,
                outcome: ToolCallOutcome::Completed { success: true },
                output_preview: Some(
                    "Chunk ID: abc123\n\
                     Wall time: 0.0000 seconds\n\
                     Process exited with code 0\n\
                     Background mode: true\n\
                     Background description: Auto-backgrounded if still running after the foreground wait\n\
                     Background triggers: on_exit\n\
                     Background log: /tmp/codex-link/jobs/exec-9999.log\n\
                     Output:\n\
                     output_preview: Some(\n\
                         \"Background mode: true\\n\\\n\
                          Background description: fake job\\n\\\n\
                          Background triggers: on_exit\\n\\\n\
                          Process running with session ID 1234\",\n\
                     )",
                ),
            })
            .await;

        let state = thread_store
            .get::<LinkContextStore>()
            .expect("tool finish should initialize Link context store")
            .snapshot();

        assert_eq!(state.active_background_jobs, Vec::<String>::new());
    }

    #[tokio::test]
    async fn records_native_exec_background_metadata_from_begin_event() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");

        let begin_event = ExecCommandBeginEvent {
            call_id: "call-bg".to_string(),
            process_id: Some("1234".to_string()),
            turn_id: "turn-1".to_string(),
            started_at_ms: 42,
            command: vec!["python".to_string(), "train.py".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            parsed_cmd: Vec::<ParsedCommand>::new(),
            source: ExecCommandSource::UnifiedExecStartup,
            interaction_input: None,
            background_description: Some("train model until val_loss < 0.30".to_string()),
            background_triggers: vec!["on_exit".to_string(), "metric_plateau=10".to_string()],
        };

        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecCommandBegin(begin_event),
            })
            .await;

        let state = thread_store
            .get::<LinkContextStore>()
            .expect("begin event should initialize Link context store")
            .snapshot();

        assert_eq!(
            state.active_background_jobs,
            vec![
                "exec session 1234 running: train model until val_loss < 0.30; command: python train.py; cwd: file:///repo; triggers: on_exit, metric_plateau=10"
                    .to_string()
            ]
        );
        assert!(
            state
                .verified_evidence
                .iter()
                .any(|value| value.contains("Native background exec registered: exec session 1234"))
        );
    }

    #[tokio::test]
    async fn tool_finish_metadata_merges_with_begin_event_job() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let tool_name = ToolName::plain("exec_command");

        let begin_event = ExecCommandBeginEvent {
            call_id: "call-bg".to_string(),
            process_id: Some("1234".to_string()),
            turn_id: "turn-1".to_string(),
            started_at_ms: 42,
            command: vec!["python".to_string(), "train.py".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            parsed_cmd: Vec::<ParsedCommand>::new(),
            source: ExecCommandSource::UnifiedExecStartup,
            interaction_input: None,
            background_description: Some("train model".to_string()),
            background_triggers: vec!["on_exit".to_string()],
        };

        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecCommandBegin(begin_event),
            })
            .await;

        contributor
            .on_tool_finish(ToolFinishInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                call_id: "call-bg",
                tool_name: &tool_name,
                source: ToolCallSource::Direct,
                outcome: ToolCallOutcome::Completed { success: true },
                output_preview: Some(
                    "Chunk ID: abc123\n\
                     Wall time: 1.0000 seconds\n\
                     Background mode: true\n\
                     Background description: train model\n\
                     Background triggers: metric_threshold:val_loss<0.30\n\
                     Process running with session ID 1234\n\
                     Output:\nstarted",
                ),
            })
            .await;

        let state = thread_store
            .get::<LinkContextStore>()
            .expect("store should exist")
            .snapshot();

        assert_eq!(
            state.active_background_jobs,
            vec![
                "exec session 1234 running: train model; command: python train.py; cwd: file:///repo; triggers: on_exit, metric_threshold:val_loss<0.30"
                    .to_string()
            ]
        );
    }

    #[tokio::test]
    async fn exec_end_for_native_background_enqueues_one_callback_event() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let tool_name = ToolName::plain("exec_command");

        contributor
            .on_tool_finish(ToolFinishInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                call_id: "call-bg",
                tool_name: &tool_name,
                source: ToolCallSource::Direct,
                outcome: ToolCallOutcome::Completed { success: true },
                output_preview: Some(
                    "Chunk ID: abc123\n\
                     Wall time: 1.0000 seconds\n\
                     Background mode: true\n\
                     Background description: train model until val_loss < 0.30\n\
                     Background log: /tmp/codex-link/jobs/exec-1234.log\n\
                     Background triggers: on_exit\n\
                     Process running with session ID 1234\n\
                     Output:\nstarted",
                ),
            })
            .await;

        let end_event = ExecCommandEndEvent {
            call_id: "call-bg".to_string(),
            process_id: Some("1234".to_string()),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 42,
            command: vec!["python".to_string(), "train.py".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            parsed_cmd: Vec::<ParsedCommand>::new(),
            source: ExecCommandSource::UnifiedExecStartup,
            interaction_input: None,
            stdout: "epoch 10 val_loss=0.42".to_string(),
            stderr: String::new(),
            aggregated_output: "epoch 10 val_loss=0.42".to_string(),
            exit_code: 0,
            duration: Duration::from_secs(60),
            formatted_output: "epoch 10 val_loss=0.42".to_string(),
            status: ExecCommandStatus::Completed,
        };

        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecCommandEnd(end_event),
            })
            .await;

        let store = thread_store
            .get::<LinkContextStore>()
            .expect("store should exist");
        let state = store.snapshot();
        assert!(state.active_background_jobs.is_empty());
        assert!(
            state
                .verified_evidence
                .iter()
                .any(|value| value.contains("ended with exit_code=0"))
        );

        let events = store.pop_pending_background_events();
        assert_eq!(events.len(), 1);
        let prompt = background_event_steering_item(&events);
        let ResponseItem::Message { content, .. } = prompt else {
            panic!("background callback should be a contextual message");
        };
        let text = content
            .iter()
            .find_map(|item| match item {
                ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("contextual message should contain text");
        assert!(text.contains("<codex_link_background_event>"));
        assert!(text.contains("Trigger: on_exit"));
        assert!(text.contains("python train.py"));
        assert!(text.contains("Log: /tmp/codex-link/jobs/exec-1234.log"));
    }

    #[tokio::test]
    async fn background_trigger_event_enqueues_callback_without_prior_tool_finish() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");

        let trigger_event = ExecBackgroundTriggerEvent {
            call_id: "call-bg".to_string(),
            process_id: "1234".to_string(),
            turn_id: "turn-1".to_string(),
            triggered_at_ms: 42,
            command: vec!["python".to_string(), "train.py".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            description: Some("train model until val_loss < 0.30".to_string()),
            declared_triggers: vec!["regex:CUDA out of memory".to_string()],
            trigger: "regex:CUDA out of memory".to_string(),
            reason: "regex pattern matched process output".to_string(),
            output_tail: "RuntimeError: CUDA out of memory".to_string(),
        };

        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecBackgroundTrigger(Box::new(trigger_event)),
            })
            .await;

        let store = thread_store
            .get::<LinkContextStore>()
            .expect("store should exist");
        let state = store.snapshot();
        assert!(
            state
                .verified_evidence
                .iter()
                .any(|value| value.contains("fired trigger `regex:CUDA out of memory`"))
        );

        let events = store.pop_pending_background_events();
        assert_eq!(events.len(), 1);
        let prompt = background_event_steering_item(&events);
        let ResponseItem::Message { content, .. } = prompt else {
            panic!("background callback should be a contextual message");
        };
        let text = content
            .iter()
            .find_map(|item| match item {
                ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("contextual message should contain text");
        assert!(text.contains("Trigger: regex:CUDA out of memory"));
        assert!(text.contains("still running"));
        assert!(text.contains("RuntimeError: CUDA out of memory"));
    }

    #[tokio::test]
    async fn duplicate_background_trigger_event_is_not_requeued_after_pop() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");

        let trigger_event = ExecBackgroundTriggerEvent {
            call_id: "call-bg".to_string(),
            process_id: "1234".to_string(),
            turn_id: "turn-1".to_string(),
            triggered_at_ms: 42,
            command: vec!["python".to_string(), "train.py".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            description: Some("train model until val_loss < 0.30".to_string()),
            declared_triggers: vec!["metric_threshold:val_loss < 0.30".to_string()],
            trigger: "metric_threshold:val_loss < 0.30".to_string(),
            reason: "val_loss < 0.3 matched with observed 0.298".to_string(),
            output_tail: "epoch=42 val_loss=0.298".to_string(),
        };

        for _ in 0..2 {
            contributor
                .on_turn_event(TurnEventInput {
                    session_store: &session_store,
                    thread_store: &thread_store,
                    turn_store: &turn_store,
                    turn_id: "turn-1",
                    event: &EventMsg::ExecBackgroundTrigger(Box::new(trigger_event.clone())),
                })
                .await;
        }

        let store = thread_store
            .get::<LinkContextStore>()
            .expect("store should exist");
        let events = store.pop_pending_background_events();
        assert_eq!(events.len(), 1);

        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecBackgroundTrigger(Box::new(trigger_event)),
            })
            .await;

        assert!(store.pop_pending_background_events().is_empty());
    }

    #[tokio::test]
    async fn background_exit_trigger_dedupes_exec_end_and_renders_terminal_status() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let tool_name = ToolName::plain("exec_command");

        contributor
            .on_tool_finish(ToolFinishInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                call_id: "call-bg",
                tool_name: &tool_name,
                source: ToolCallSource::Direct,
                outcome: ToolCallOutcome::Completed { success: true },
                output_preview: Some(
                    "Chunk ID: abc123\n\
                     Wall time: 1.0000 seconds\n\
                     Background mode: true\n\
                     Background description: smoke command\n\
                     Background log: /tmp/codex-link/jobs/exec-1234.log\n\
                     Background triggers: on_exit\n\
                     Process running with session ID 1234\n\
                     Output:\nstarted",
                ),
            })
            .await;

        let trigger_event = ExecBackgroundTriggerEvent {
            call_id: "call-bg".to_string(),
            process_id: "1234".to_string(),
            turn_id: "turn-1".to_string(),
            triggered_at_ms: 42,
            command: vec!["sh".to_string(), "-lc".to_string(), "echo done".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            description: Some("smoke command".to_string()),
            declared_triggers: vec!["on_exit".to_string()],
            trigger: "on_exit".to_string(),
            reason: "process exited with code 0".to_string(),
            output_tail: "done".to_string(),
        };
        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecBackgroundTrigger(Box::new(trigger_event)),
            })
            .await;

        let end_event = ExecCommandEndEvent {
            call_id: "call-bg".to_string(),
            process_id: Some("1234".to_string()),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 43,
            command: vec!["sh".to_string(), "-lc".to_string(), "echo done".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            parsed_cmd: Vec::<ParsedCommand>::new(),
            source: ExecCommandSource::UnifiedExecStartup,
            interaction_input: None,
            stdout: "done".to_string(),
            stderr: String::new(),
            aggregated_output: "done".to_string(),
            exit_code: 0,
            duration: Duration::from_secs(1),
            formatted_output: "done".to_string(),
            status: ExecCommandStatus::Completed,
        };
        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecCommandEnd(end_event),
            })
            .await;

        let store = thread_store
            .get::<LinkContextStore>()
            .expect("store should exist");
        let events = store.pop_pending_background_events();
        assert_eq!(events.len(), 1);
        let prompt = background_event_steering_item(&events);
        let ResponseItem::Message { content, .. } = prompt else {
            panic!("background callback should be a contextual message");
        };
        let text = content
            .iter()
            .find_map(|item| match item {
                ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("contextual message should contain text");
        assert_eq!(text.matches("<codex_link_background_event>").count(), 1);
        assert!(text.contains("Trigger: on_exit"));
        assert!(text.contains("Exit code: 0"));
        assert!(text.contains("Status: Completed"));
        assert!(text.contains("Log: /tmp/codex-link/jobs/exec-1234.log"));
        assert!(!text.contains("still running"));
    }

    #[tokio::test]
    async fn pending_background_trigger_event_gets_late_log_path_from_tool_finish() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let tool_name = ToolName::plain("exec_command");

        let trigger_event = ExecBackgroundTriggerEvent {
            call_id: "call-bg".to_string(),
            process_id: "1234".to_string(),
            turn_id: "turn-1".to_string(),
            triggered_at_ms: 42,
            command: vec!["python".to_string(), "train.py".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            description: Some("train model".to_string()),
            declared_triggers: vec!["metric_threshold:val_loss < 0.30".to_string()],
            trigger: "metric_threshold:val_loss < 0.30".to_string(),
            reason: "val_loss < 0.30 matched".to_string(),
            output_tail: "epoch=2 val_loss=0.298".to_string(),
        };
        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecBackgroundTrigger(Box::new(trigger_event)),
            })
            .await;

        contributor
            .on_tool_finish(ToolFinishInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                call_id: "call-bg",
                tool_name: &tool_name,
                source: ToolCallSource::Direct,
                outcome: ToolCallOutcome::Completed { success: true },
                output_preview: Some(
                    "Chunk ID: abc123\n\
                     Wall time: 1.0000 seconds\n\
                     Background mode: true\n\
                     Background description: train model\n\
                     Background log: /tmp/codex-link/jobs/exec-1234.log\n\
                     Background triggers: metric_threshold:val_loss < 0.30, on_exit\n\
                     Process running with session ID 1234\n\
                     Output:\nstarted",
                ),
            })
            .await;

        let store = thread_store
            .get::<LinkContextStore>()
            .expect("store should exist");
        let events = store.pop_pending_background_events();
        assert_eq!(events.len(), 1);
        let prompt = background_event_steering_item(&events);
        let ResponseItem::Message { content, .. } = prompt else {
            panic!("background callback should be a contextual message");
        };
        let text = content
            .iter()
            .find_map(|item| match item {
                ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("contextual message should contain text");
        assert!(text.contains("Trigger: metric_threshold:val_loss < 0.30"));
        assert!(text.contains("Log: /tmp/codex-link/jobs/exec-1234.log"));
        assert!(text.contains("Status: running"));
    }

    #[tokio::test]
    async fn background_trigger_callback_truncates_large_output_tail() {
        let contributor = LinkContextExtension::default();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        let long_tail = "x".repeat(MAX_BACKGROUND_EVENT_TAIL_CHARS + 128);

        let trigger_event = ExecBackgroundTriggerEvent {
            call_id: "call-bg".to_string(),
            process_id: "1234".to_string(),
            turn_id: "turn-1".to_string(),
            triggered_at_ms: 42,
            command: vec!["python".to_string(), "train.py".to_string()],
            cwd: PathUri::parse("file:///repo").expect("valid cwd uri"),
            description: Some("train model until val_loss < 0.30".to_string()),
            declared_triggers: vec!["regex:Traceback".to_string()],
            trigger: "regex:Traceback".to_string(),
            reason: "regex pattern matched process output".to_string(),
            output_tail: long_tail,
        };

        contributor
            .on_turn_event(TurnEventInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id: "turn-1",
                event: &EventMsg::ExecBackgroundTrigger(Box::new(trigger_event)),
            })
            .await;

        let store = thread_store
            .get::<LinkContextStore>()
            .expect("store should exist");
        let events = store.pop_pending_background_events();
        let prompt = background_event_steering_item(&events);
        let ResponseItem::Message { content, .. } = prompt else {
            panic!("background callback should be a contextual message");
        };
        let text = content
            .iter()
            .find_map(|item| match item {
                ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("contextual message should contain text");

        assert!(text.contains("[truncated]"));
        assert!(text.len() < MAX_BACKGROUND_EVENT_TAIL_CHARS + 1_000);
    }

    #[test]
    fn install_registers_prompt_and_tool_lifecycle_contributors() {
        let mut builder = codex_extension_api::ExtensionRegistryBuilder::<()>::new();

        install(&mut builder);

        let registry = builder.build();
        assert_eq!(registry.context_contributors().len(), 1);
        assert_eq!(registry.turn_item_contributors().len(), 1);
        assert_eq!(registry.tool_lifecycle_contributors().len(), 1);
        assert_eq!(registry.turn_event_contributors().len(), 1);
        assert_eq!(registry.tool_contributors().len(), 0);
    }
}
