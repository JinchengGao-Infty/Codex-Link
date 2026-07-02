use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::Sleep;

use super::BackgroundTriggerPolicy;
use super::UnifiedExecContext;
use super::background_triggers::BackgroundTriggerEvaluator;
use super::background_triggers::BackgroundTriggerFired;
use super::process::UnifiedExecProcess;
use crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::turn_timing::now_unix_timestamp_ms;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecBackgroundTriggerEvent;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecOutputStream;
use codex_utils_path_uri::PathUri;

pub(crate) const TRAILING_OUTPUT_GRACE: Duration = Duration::from_millis(100);

/// Upper bound for a single ExecCommandOutputDelta chunk emitted by unified exec.
///
/// The unified exec output buffer already caps *retained* output (see
/// `UNIFIED_EXEC_OUTPUT_MAX_BYTES`), but we also cap per-event payload size so
/// downstream event consumers (especially app-server JSON-RPC) don't have to
/// process arbitrarily large delta payloads.
const UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES: usize = 8192;
const BACKGROUND_EVENT_OUTPUT_TAIL_MAX_CHARS: usize = 2_000;

#[derive(Clone, Default)]
pub(crate) struct BackgroundEventNotifier {
    notified: Arc<Mutex<HashSet<String>>>,
}

impl BackgroundEventNotifier {
    async fn mark_notified(&self, process_id: &str, trigger: &str) -> bool {
        let mut notified = self.notified.lock().await;
        notified.insert(format!("{process_id}:{trigger}"))
    }
}

pub(crate) struct BackgroundTriggerWatchConfig {
    pub(crate) process_id: i32,
    pub(crate) command: Vec<String>,
    pub(crate) cwd: PathUri,
    pub(crate) description: Option<String>,
    pub(crate) declared_triggers: Vec<String>,
    pub(crate) policy: BackgroundTriggerPolicy,
}

struct BackgroundTriggerStreamWatcher {
    process_id: String,
    command: Vec<String>,
    cwd: PathUri,
    description: Option<String>,
    declared_triggers: Vec<String>,
    evaluator: BackgroundTriggerEvaluator,
}

impl BackgroundTriggerStreamWatcher {
    fn new(config: BackgroundTriggerWatchConfig, now: Instant) -> Self {
        Self {
            process_id: config.process_id.to_string(),
            command: config.command,
            cwd: config.cwd,
            description: config.description,
            declared_triggers: config.declared_triggers,
            evaluator: BackgroundTriggerEvaluator::new(config.policy, now),
        }
    }

    fn next_no_output_sleep(&self) -> Option<Pin<Box<Sleep>>> {
        self.evaluator
            .next_no_output_deadline()
            .map(|deadline| Box::pin(tokio::time::sleep_until(deadline)))
    }
}

/// Spawn a background task that continuously reads from the PTY, appends to the
/// shared transcript, and emits ExecCommandOutputDelta events on UTF‑8
/// boundaries.
pub(crate) fn start_streaming_output(
    process: &UnifiedExecProcess,
    context: &UnifiedExecContext,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    background_trigger_watch: Option<BackgroundTriggerWatchConfig>,
    background_event_notifier: BackgroundEventNotifier,
) {
    let mut receiver = process.output_receiver();
    let output_drained = process.output_drained_notify();
    let exit_token = process.cancellation_token();

    let session_ref = Arc::clone(&context.session);
    let turn_ref = Arc::clone(&context.turn);
    let call_id = context.call_id.clone();

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;

        let mut pending = Vec::<u8>::new();
        let mut emitted_deltas: usize = 0;
        let mut background_trigger_watch = background_trigger_watch
            .map(|config| Box::new(BackgroundTriggerStreamWatcher::new(config, Instant::now())));
        let mut no_output_sleep = background_trigger_watch
            .as_ref()
            .and_then(|watcher| watcher.next_no_output_sleep());

        let mut grace_sleep: Option<Pin<Box<Sleep>>> = None;

        loop {
            tokio::select! {
                _ = exit_token.cancelled(), if grace_sleep.is_none() => {
                    let deadline = Instant::now() + TRAILING_OUTPUT_GRACE;
                    grace_sleep.replace(Box::pin(tokio::time::sleep_until(deadline)));
                }

                _ = async {
                    if let Some(sleep) = no_output_sleep.as_mut() {
                        sleep.as_mut().await;
                    }
                }, if no_output_sleep.is_some() => {
                    if let Some(watcher) = background_trigger_watch.as_mut() {
                        let fired = watcher.evaluator.on_no_output_timeout(Instant::now());
                        emit_background_trigger_events(
                            Arc::clone(&session_ref),
                            Arc::clone(&turn_ref),
                            background_event_notifier.clone(),
                            call_id.clone(),
                            watcher.process_id.clone(),
                            watcher.command.clone(),
                            watcher.cwd.clone(),
                            watcher.description.clone(),
                            watcher.declared_triggers.clone(),
                            fired,
                        );
                        no_output_sleep = watcher.next_no_output_sleep();
                    } else {
                        no_output_sleep = None;
                    }
                }

                _ = async {
                    if let Some(sleep) = grace_sleep.as_mut() {
                        sleep.as_mut().await;
                    }
                }, if grace_sleep.is_some() => {
                    output_drained.notify_one();
                    break;
                }

                received = receiver.recv() => {
                    let chunk = match received {
                        Ok(chunk) => chunk,
                        Err(RecvError::Lagged(_)) => {
                            continue;
                        },
                        Err(RecvError::Closed) => {
                            output_drained.notify_one();
                            break;
                        }
                    };

                    process_chunk(
                        &mut pending,
                        &transcript,
                        &call_id,
                        &session_ref,
                        &turn_ref,
                        &mut emitted_deltas,
                        background_trigger_watch.as_deref_mut(),
                        &mut no_output_sleep,
                        background_event_notifier.clone(),
                        chunk,
                    ).await;
                }
            }
        }
    });
}

/// Spawn a background watcher that waits for the PTY to exit and then emits a
/// single ExecCommandEnd event with the aggregated transcript.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_exit_watcher(
    process: Arc<UnifiedExecProcess>,
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: i32,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    started_at: Instant,
    initial_exec_command_active: Arc<AtomicBool>,
    notify_background_exit: bool,
    description: Option<String>,
    declared_triggers: Vec<String>,
    background_event_notifier: BackgroundEventNotifier,
) {
    let exit_token = process.cancellation_token();
    let output_drained = process.output_drained_notify();

    tokio::spawn(async move {
        exit_token.cancelled().await;
        output_drained.notified().await;

        let duration = Instant::now().saturating_duration_since(started_at);
        let became_background = !initial_exec_command_active.load(Ordering::Acquire);
        let should_emit_background_exit = became_background && notify_background_exit;
        if let Some(message) = process.failure_message() {
            if should_emit_background_exit {
                let output_tail =
                    resolve_background_output_tail(&transcript, message.clone()).await;
                emit_background_exit_event(
                    Arc::clone(&session_ref),
                    Arc::clone(&turn_ref),
                    background_event_notifier.clone(),
                    call_id.clone(),
                    process_id.to_string(),
                    command.clone(),
                    cwd.clone(),
                    description.clone(),
                    declared_triggers.clone(),
                    "failure_exit".to_string(),
                    format!("process failed: {message}"),
                    output_tail,
                );
            }
            emit_failed_exec_end_for_unified_exec(
                session_ref,
                turn_ref,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                transcript,
                String::new(),
                message,
                duration,
            )
            .await;
        } else {
            let exit_code = process.exit_code().unwrap_or(-1);
            if should_emit_background_exit {
                let output_tail = resolve_background_output_tail(&transcript, String::new()).await;
                emit_background_exit_event(
                    Arc::clone(&session_ref),
                    Arc::clone(&turn_ref),
                    background_event_notifier.clone(),
                    call_id.clone(),
                    process_id.to_string(),
                    command.clone(),
                    cwd.clone(),
                    description.clone(),
                    declared_triggers.clone(),
                    "on_exit".to_string(),
                    format!("process exited with code {exit_code}"),
                    output_tail,
                );
            }
            emit_exec_end_for_unified_exec(
                session_ref,
                turn_ref,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                transcript,
                String::new(),
                exit_code,
                duration,
            )
            .await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn process_chunk(
    pending: &mut Vec<u8>,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &mut usize,
    background_trigger_watch: Option<&mut BackgroundTriggerStreamWatcher>,
    no_output_sleep: &mut Option<Pin<Box<Sleep>>>,
    background_event_notifier: BackgroundEventNotifier,
    chunk: Vec<u8>,
) {
    pending.extend_from_slice(&chunk);
    let mut background_trigger_watch = background_trigger_watch;
    while let Some(prefix) = split_valid_utf8_prefix(pending) {
        {
            let mut guard = transcript.lock().await;
            guard.push_chunk(prefix.to_vec());
        }
        if let Some(watcher) = background_trigger_watch.as_mut() {
            let text = String::from_utf8_lossy(&prefix);
            let fired = watcher.evaluator.on_output(&text, Instant::now());
            emit_background_trigger_events(
                Arc::clone(session_ref),
                Arc::clone(turn_ref),
                background_event_notifier.clone(),
                call_id.to_string(),
                watcher.process_id.clone(),
                watcher.command.clone(),
                watcher.cwd.clone(),
                watcher.description.clone(),
                watcher.declared_triggers.clone(),
                fired,
            );
            *no_output_sleep = watcher.next_no_output_sleep();
        }

        if *emitted_deltas >= MAX_EXEC_OUTPUT_DELTAS_PER_CALL {
            continue;
        }

        let event = ExecCommandOutputDeltaEvent {
            call_id: call_id.to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: prefix,
        };
        session_ref
            .send_event(turn_ref.as_ref(), EventMsg::ExecCommandOutputDelta(event))
            .await;
        *emitted_deltas += 1;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_background_trigger_events(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    background_event_notifier: BackgroundEventNotifier,
    call_id: String,
    process_id: String,
    command: Vec<String>,
    cwd: PathUri,
    description: Option<String>,
    declared_triggers: Vec<String>,
    fired: Vec<BackgroundTriggerFired>,
) {
    if fired.is_empty() {
        return;
    }

    tokio::spawn(async move {
        let turn = turn_ref.as_ref();
        let turn_id = turn.sub_id.clone();
        let triggered_at_ms = now_unix_timestamp_ms();

        for event in fired {
            if !background_event_notifier
                .mark_notified(&process_id, &event.trigger)
                .await
            {
                continue;
            }
            let trigger = event.trigger;
            let reason = event.reason;
            let output_tail = event.output_tail;
            session_ref
                .send_event(
                    turn,
                    EventMsg::ExecBackgroundTrigger(Box::new(ExecBackgroundTriggerEvent {
                        call_id: call_id.clone(),
                        process_id: process_id.clone(),
                        turn_id: turn_id.clone(),
                        triggered_at_ms,
                        command: command.clone(),
                        cwd: cwd.clone(),
                        description: description.clone(),
                        declared_triggers: declared_triggers.clone(),
                        trigger: trigger.clone(),
                        reason: reason.clone(),
                        output_tail: output_tail.clone(),
                    })),
                )
                .await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_background_exit_event(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    background_event_notifier: BackgroundEventNotifier,
    call_id: String,
    process_id: String,
    command: Vec<String>,
    cwd: PathUri,
    description: Option<String>,
    declared_triggers: Vec<String>,
    trigger: String,
    reason: String,
    output_tail: String,
) {
    emit_background_trigger_events(
        session_ref,
        turn_ref,
        background_event_notifier,
        call_id,
        process_id,
        command,
        cwd,
        description,
        declared_triggers,
        vec![BackgroundTriggerFired {
            trigger,
            reason,
            output_tail,
        }],
    );
}

/// Emit an ExecCommandEnd event for a unified exec session, using the transcript
/// as the primary source of aggregated_output and falling back to the provided
/// text when the transcript is empty.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: Option<String>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    exit_code: i32,
    duration: Duration,
) {
    let aggregated_output = resolve_aggregated_output(&transcript, fallback_output).await;
    let output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(aggregated_output.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Success {
                output,
                applied_patch_delta: None,
            },
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_failed_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: Option<String>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    message: String,
    duration: Duration,
) {
    let stdout = if fallback_output.is_empty() {
        resolve_aggregated_output(&transcript, fallback_output).await
    } else {
        fallback_output
    };
    let aggregated_output = if stdout.is_empty() {
        message.clone()
    } else {
        format!("{stdout}\n{message}")
    };
    let output = ExecToolCallOutput {
        exit_code: -1,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(message),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Failure(ToolEventFailure::Output(output)),
        )
        .await;
}

fn split_valid_utf8_prefix(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    split_valid_utf8_prefix_with_max(buffer, UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES)
}

fn split_valid_utf8_prefix_with_max(buffer: &mut Vec<u8>, max_bytes: usize) -> Option<Vec<u8>> {
    if buffer.is_empty() {
        return None;
    }

    let max_len = buffer.len().min(max_bytes);
    let mut split = max_len;
    while split > 0 {
        if std::str::from_utf8(&buffer[..split]).is_ok() {
            let prefix = buffer[..split].to_vec();
            buffer.drain(..split);
            return Some(prefix);
        }

        if max_len - split > 4 {
            break;
        }
        split -= 1;
    }

    // If no valid UTF-8 prefix was found, emit the first byte so the stream
    // keeps making progress and the transcript reflects all bytes.
    let byte = buffer.drain(..1).collect();
    Some(byte)
}

async fn resolve_aggregated_output(
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    fallback: String,
) -> String {
    let guard = transcript.lock().await;
    if guard.retained_bytes() == 0 {
        return fallback;
    }

    String::from_utf8_lossy(&guard.to_bytes()).to_string()
}

async fn resolve_background_output_tail(
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    fallback: String,
) -> String {
    bounded_tail(
        &resolve_aggregated_output(transcript, fallback).await,
        BACKGROUND_EVENT_OUTPUT_TAIL_MAX_CHARS,
    )
}

fn bounded_tail(text: &str, max_chars: usize) -> String {
    let mut tail = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    tail.reverse();
    tail.into_iter().collect()
}

#[cfg(test)]
#[path = "async_watcher_tests.rs"]
mod tests;
