use super::head_tail_buffer::HeadTailBuffer;
use super::*;
use crate::codex_thread::BackgroundTerminalInfo;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::sandboxing::ExecRequest;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::process::OutputHandles;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use codex_sandboxing::SandboxType;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use core_test_support::skip_if_no_remote_env;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::test_env as remote_test_env;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;

async fn test_session_and_turn() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, turn) = make_session_and_context().await;
    (Arc::new(session), Arc::new(turn))
}

async fn exec_command(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cmd: &str,
    yield_time_ms: u64,
    workdir: Option<PathBuf>,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    exec_command_with_tty(
        session,
        turn,
        cmd,
        yield_time_ms,
        workdir,
        /*tty*/ true,
    )
    .await
}

fn shell_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn test_exec_request(
    turn: &TurnContext,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    env: HashMap<String, String>,
) -> ExecRequest {
    let windows_sandbox_private_desktop = false;
    let permission_profile = turn.permission_profile();
    let network = None;
    let arg0 = None;
    ExecRequest::new(
        command,
        cwd,
        env,
        network,
        /*network_environment_id*/ None,
        ExecExpiration::DefaultTimeout,
        ExecCapturePolicy::ShellTool,
        SandboxType::None,
        turn.config.effective_workspace_roots(),
        turn.windows_sandbox_level,
        windows_sandbox_private_desktop,
        permission_profile,
        arg0,
    )
}

async fn exec_command_with_tty(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cmd: &str,
    yield_time_ms: u64,
    workdir: Option<PathBuf>,
    tty: bool,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    #[allow(deprecated)]
    let cwd = workdir
        .as_ref()
        .map_or_else(|| turn.cwd.clone(), |workdir| turn.cwd.join(workdir));
    let command = vec!["bash".to_string(), "-lc".to_string(), cmd.to_string()];
    let request = test_exec_request(turn, command.clone(), cwd.clone(), shell_env());

    let process = Arc::new(
        manager
            .open_session_with_prepared_exec_env(
                process_id,
                &request,
                tty,
                Box::new(NoopSpawnLifecycle),
                turn.environments
                    .primary()
                    .expect("turn environment")
                    .environment
                    .as_ref(),
                None,
            )
            .await?,
    );
    let context =
        UnifiedExecContext::new(Arc::clone(session), Arc::clone(turn), "call".to_string());
    let started_at = Instant::now();
    let process_started_alive = !process.has_exited() && process.exit_code().is_none();
    if process_started_alive {
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            call_id: context.call_id.clone(),
            process_id,
            cwd: cwd.clone().into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hook_command: cmd.to_string(),
            tty,
            network_approval: None,
            background_description: None,
            background_triggers: Vec::new(),
            session: Arc::downgrade(session),
            last_used: started_at,
            started_at_unix_ms: crate::turn_timing::now_unix_timestamp_ms(),
        };
        manager
            .process_store
            .lock()
            .await
            .processes
            .insert(process_id, entry);
    }

    let OutputHandles {
        output_buffer,
        output_notify,
        output_closed,
        output_closed_notify,
        cancellation_token,
    } = process.output_handles();
    let deadline = started_at + Duration::from_millis(yield_time_ms);
    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        Some(session.subscribe_out_of_band_elicitation_pause_state()),
        deadline,
    )
    .await;
    let wall_time = Instant::now().saturating_duration_since(started_at);
    let text = String::from_utf8_lossy(&collected).to_string();
    let has_exited = process.has_exited();
    let exit_code = process.exit_code();
    let response_process_id = if process_started_alive && !has_exited {
        Some(process_id)
    } else {
        manager.release_process_id(process_id).await;
        None
    };
    if response_process_id.is_some()
        && let Some(entry) = manager
            .process_store
            .lock()
            .await
            .processes
            .get_mut(&process_id)
    {
        entry
            .initial_exec_command_active
            .store(false, std::sync::atomic::Ordering::Release);
    }

    Ok(ExecCommandToolOutput {
        event_call_id: context.call_id,
        chunk_id: generate_chunk_id(),
        wall_time,
        raw_output: collected,
        truncation_policy: turn.model_info.truncation_policy.into(),
        max_output_tokens: None,
        process_id: response_process_id,
        exit_code,
        original_token_count: Some(approx_token_count(&text)),
        hook_command: Some(cmd.to_string()),
        background: None,
        end_turn_after_record: false,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_regex_trigger_emits_event_from_streaming_watcher() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let log_dir = tempfile::TempDir::new()?;
    let background_log_path = log_dir.path().join("exec-bg-trigger.log");
    let command = vec![
        "sh".to_string(),
        "-lc".to_string(),
        "sleep 0.5; printf 'warming up\\n'; sleep 0.1; printf 'CUDA out of memory\\n'; sleep 1"
            .to_string(),
    ];
    let triggers = vec!["regex:CUDA out of memory".to_string()];
    let background_trigger_policy = BackgroundTriggerPolicy::parse_declared(&triggers)
        .map_err(anyhow::Error::msg)?
        .expect("regex trigger should create executable policy");
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-bg-trigger".to_string(),
    );

    let response = manager
        .exec_command(
            ExecCommandRequest {
                command: command.clone(),
                shell_type: crate::shell::ShellType::Sh,
                hook_command: command.join(" "),
                process_id,
                yield_time_ms: 250,
                max_output_tokens: None,
                cwd,
                #[allow(deprecated)]
                sandbox_cwd: turn.cwd.clone().into(),
                turn_environment: turn
                    .environments
                    .primary()
                    .cloned()
                    .expect("primary environment"),
                shell_mode: codex_tools::UnifiedExecShellMode::Direct,
                network: None,
                tty: true,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                additional_permissions_preapproved: false,
                justification: None,
                prefix_rule: None,
                background_description: Some("watch training for OOM".to_string()),
                background_triggers: triggers.clone(),
                background_trigger_policy: Some(background_trigger_policy),
                background_log_path: Some(background_log_path.clone()),
                background_declared: true,
                end_turn_after_record: true,
            },
            &context,
        )
        .await?;

    let trigger_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = rx_event.recv().await.expect("event channel open");
            if let codex_protocol::protocol::EventMsg::ExecBackgroundTrigger(trigger_event) =
                event.msg
            {
                return trigger_event;
            }
        }
    })
    .await?;

    assert_eq!(trigger_event.call_id, "call-bg-trigger");
    assert_eq!(trigger_event.process_id, process_id.to_string());
    assert_eq!(trigger_event.trigger, "regex:CUDA out of memory");
    assert!(trigger_event.output_tail.contains("CUDA out of memory"));
    let log_text = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let log_text = tokio::fs::read_to_string(&background_log_path)
                .await
                .unwrap_or_default();
            if log_text.contains("CUDA out of memory") {
                return log_text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    assert!(
        log_text.contains("warming up"),
        "background log should include output emitted before the trigger: {log_text:?}"
    );
    assert!(log_text.contains("CUDA out of memory"));

    if let Some(process_id) = response.process_id {
        assert!(session.terminate_background_terminal(process_id).await);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_log_records_initial_output() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn, _rx_event) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let log_dir = tempfile::TempDir::new()?;
    let background_log_path = log_dir.path().join("exec-bg-initial-output.log");
    let command = vec![
        "sh".to_string(),
        "-lc".to_string(),
        "printf 'first line\\n'; sleep 0.2; printf 'second line\\n'; sleep 1".to_string(),
    ];
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-bg-log".to_string(),
    );

    let response = manager
        .exec_command(
            ExecCommandRequest {
                command: command.clone(),
                shell_type: crate::shell::ShellType::Sh,
                hook_command: command.join(" "),
                process_id,
                yield_time_ms: 250,
                max_output_tokens: None,
                cwd,
                #[allow(deprecated)]
                sandbox_cwd: turn.cwd.clone().into(),
                turn_environment: turn
                    .environments
                    .primary()
                    .cloned()
                    .expect("primary environment"),
                shell_mode: codex_tools::UnifiedExecShellMode::Direct,
                network: None,
                tty: true,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                additional_permissions_preapproved: false,
                justification: None,
                prefix_rule: None,
                background_description: Some("record early output".to_string()),
                background_triggers: Vec::new(),
                background_trigger_policy: None,
                background_log_path: Some(background_log_path.clone()),
                background_declared: true,
                end_turn_after_record: true,
            },
            &context,
        )
        .await?;

    let log_text = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let log_text = tokio::fs::read_to_string(&background_log_path)
                .await
                .unwrap_or_default();
            if log_text.contains("second line") {
                return log_text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    assert!(
        log_text.contains("first line"),
        "background log should include output emitted immediately after spawn: {log_text:?}"
    );
    assert!(log_text.contains("second line"));

    if let Some(process_id) = response.process_id {
        assert!(session.terminate_background_terminal(process_id).await);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_exit_emits_event_without_stdin_poll() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let command = vec![
        "sh".to_string(),
        "-lc".to_string(),
        "sleep 1; printf 'training done\\n'".to_string(),
    ];
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-bg-exit".to_string(),
    );

    let response = manager
        .exec_command(
            ExecCommandRequest {
                command: command.clone(),
                shell_type: crate::shell::ShellType::Sh,
                hook_command: command.join(" "),
                process_id,
                yield_time_ms: 250,
                max_output_tokens: None,
                cwd,
                #[allow(deprecated)]
                sandbox_cwd: turn.cwd.clone().into(),
                turn_environment: turn
                    .environments
                    .primary()
                    .cloned()
                    .expect("primary environment"),
                shell_mode: codex_tools::UnifiedExecShellMode::Direct,
                network: None,
                tty: true,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                additional_permissions_preapproved: false,
                justification: None,
                prefix_rule: None,
                background_description: Some("watch training completion".to_string()),
                background_triggers: vec!["on_exit".to_string()],
                background_trigger_policy: None,
                background_log_path: None,
                background_declared: true,
                end_turn_after_record: true,
            },
            &context,
        )
        .await?;
    assert_eq!(response.process_id, Some(process_id));

    let trigger_event = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = rx_event.recv().await.expect("event channel open");
            if let codex_protocol::protocol::EventMsg::ExecBackgroundTrigger(trigger_event) =
                event.msg
                && trigger_event.trigger == "on_exit"
            {
                return trigger_event;
            }
        }
    })
    .await?;

    assert_eq!(trigger_event.call_id, "call-bg-exit");
    assert_eq!(trigger_event.process_id, process_id.to_string());
    assert_eq!(trigger_event.reason, "process exited with code 0");
    assert!(trigger_event.output_tail.contains("training done"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_detached_short_yield_session_exit_does_not_emit_background_trigger()
-> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let command = vec![
        "sh".to_string(),
        "-lc".to_string(),
        "sleep 1.2; printf 'interactive done\\n'".to_string(),
    ];
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-short-yield".to_string(),
    );

    let response = manager
        .exec_command(
            ExecCommandRequest {
                command: command.clone(),
                shell_type: crate::shell::ShellType::Sh,
                hook_command: command.join(" "),
                process_id,
                yield_time_ms: 250,
                max_output_tokens: None,
                cwd,
                #[allow(deprecated)]
                sandbox_cwd: turn.cwd.clone().into(),
                turn_environment: turn
                    .environments
                    .primary()
                    .cloned()
                    .expect("primary environment"),
                shell_mode: codex_tools::UnifiedExecShellMode::Direct,
                network: None,
                tty: true,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                additional_permissions_preapproved: false,
                justification: None,
                prefix_rule: None,
                background_description: Some(
                    "Auto-backgrounded if still running after the foreground wait".to_string(),
                ),
                background_triggers: vec!["on_exit".to_string()],
                background_trigger_policy: None,
                background_log_path: None,
                background_declared: false,
                end_turn_after_record: false,
            },
            &context,
        )
        .await?;
    assert!(
        response.process_id.is_some(),
        "short-yield command should return a live process id"
    );

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = rx_event.recv().await.expect("event channel open");
            match event.msg {
                codex_protocol::protocol::EventMsg::ExecBackgroundTrigger(trigger_event)
                    if trigger_event.call_id == "call-short-yield" =>
                {
                    anyhow::bail!(
                        "non-detached short-yield session emitted background trigger: {trigger_event:?}"
                    );
                }
                codex_protocol::protocol::EventMsg::ExecCommandEnd(event)
                    if event.call_id == "call-short-yield" =>
                {
                    assert_eq!(event.exit_code, 0);
                    assert!(event.aggregated_output.contains("interactive done"));
                    break;
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;

    let late_background_trigger = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            let event = rx_event.recv().await.expect("event channel open");
            if let codex_protocol::protocol::EventMsg::ExecBackgroundTrigger(trigger_event) =
                event.msg
                && trigger_event.call_id == "call-short-yield"
            {
                return Some(trigger_event);
            }
        }
    })
    .await
    .ok()
    .flatten();
    assert!(
        late_background_trigger.is_none(),
        "non-detached short-yield session emitted late background trigger: {late_background_trigger:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_tty_short_yield_session_exit_emits_background_trigger() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let command = vec![
        "sh".to_string(),
        "-lc".to_string(),
        "sleep 1; printf 'non-tty done\\n'".to_string(),
    ];
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-short-yield-non-tty".to_string(),
    );

    let response = manager
        .exec_command(
            ExecCommandRequest {
                command: command.clone(),
                shell_type: crate::shell::ShellType::Sh,
                hook_command: command.join(" "),
                process_id,
                yield_time_ms: 250,
                max_output_tokens: None,
                cwd,
                #[allow(deprecated)]
                sandbox_cwd: turn.cwd.clone().into(),
                turn_environment: turn
                    .environments
                    .primary()
                    .cloned()
                    .expect("primary environment"),
                shell_mode: codex_tools::UnifiedExecShellMode::Direct,
                network: None,
                tty: false,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                additional_permissions_preapproved: false,
                justification: None,
                prefix_rule: None,
                background_description: Some(
                    "Auto-backgrounded non-interactive command".to_string(),
                ),
                background_triggers: vec!["on_exit".to_string()],
                background_trigger_policy: None,
                background_log_path: None,
                background_declared: false,
                end_turn_after_record: false,
            },
            &context,
        )
        .await?;

    assert_eq!(response.process_id, Some(process_id));
    assert!(response.background.is_some());
    assert!(response.end_turn_after_record);

    let trigger_event = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = rx_event.recv().await.expect("event channel open");
            if let codex_protocol::protocol::EventMsg::ExecBackgroundTrigger(trigger_event) =
                event.msg
                && trigger_event.call_id == "call-short-yield-non-tty"
            {
                return trigger_event;
            }
        }
    })
    .await?;

    assert_eq!(trigger_event.process_id, process_id.to_string());
    assert_eq!(trigger_event.trigger, "on_exit");
    assert_eq!(trigger_event.reason, "process exited with code 0");
    assert!(trigger_event.output_tail.contains("non-tty done"));

    Ok(())
}

#[derive(Debug)]
struct TestSpawnLifecycle {
    inherited_fds: Vec<i32>,
}

impl SpawnLifecycle for TestSpawnLifecycle {
    fn inherited_fds(&self) -> Vec<i32> {
        self.inherited_fds.clone()
    }
}

struct BlockingTerminateExecProcess {
    process_id: ProcessId,
    terminate_started: watch::Sender<bool>,
    allow_terminate: Arc<Notify>,
    wake_tx: watch::Sender<u64>,
}

impl BlockingTerminateExecProcess {
    async fn read(&self) -> Result<ReadResponse, codex_exec_server::ExecServerError> {
        Ok(ReadResponse {
            chunks: Vec::new(),
            next_seq: 1,
            exited: false,
            exit_code: None,
            closed: false,
            failure: None,
            sandbox_denied: false,
        })
    }

    async fn write(&self) -> Result<WriteResponse, codex_exec_server::ExecServerError> {
        Ok(WriteResponse {
            status: WriteStatus::Accepted,
        })
    }

    async fn terminate(&self) -> Result<(), codex_exec_server::ExecServerError> {
        let _ = self.terminate_started.send(true);
        self.allow_terminate.notified().await;
        Ok(())
    }
}

impl ExecProcess for BlockingTerminateExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(BlockingTerminateExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(BlockingTerminateExecProcess::write(self))
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(BlockingTerminateExecProcess::terminate(self))
    }
}

async fn blocking_terminate_unified_process(
    process_id: i32,
    terminate_started: watch::Sender<bool>,
    allow_terminate: Arc<Notify>,
) -> anyhow::Result<Arc<UnifiedExecProcess>> {
    let (wake_tx, _wake_rx) = watch::channel(0);
    Ok(Arc::new(
        UnifiedExecProcess::from_exec_server_started(
            StartedExecProcess {
                process: Arc::new(BlockingTerminateExecProcess {
                    process_id: process_id.to_string().into(),
                    terminate_started,
                    allow_terminate,
                    wake_tx,
                }),
            },
            None,
        )
        .await?,
    ))
}

async fn write_stdin(
    session: &Arc<Session>,
    process_id: i32,
    input: &str,
    yield_time_ms: u64,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    session
        .services
        .unified_exec_manager
        .write_stdin(WriteStdinRequest {
            process_id,
            input,
            yield_time_ms,
            max_output_tokens: None,
            truncation_policy: TruncationPolicy::Tokens(10_000),
        })
        .await
}

#[test]
fn push_chunk_preserves_prefix_and_suffix() {
    let mut buffer = HeadTailBuffer::default();
    buffer.push_chunk(vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
    buffer.push_chunk(vec![b'b']);
    buffer.push_chunk(vec![b'c']);

    assert_eq!(buffer.retained_bytes(), UNIFIED_EXEC_OUTPUT_MAX_BYTES);
    let snapshot = buffer.snapshot_chunks();

    let first = snapshot.first().expect("expected at least one chunk");
    assert_eq!(first.first(), Some(&b'a'));
    assert!(snapshot.iter().any(|chunk| chunk.as_slice() == b"b"));
    assert_eq!(
        snapshot
            .last()
            .expect("expected at least one chunk")
            .as_slice(),
        b"c"
    );
}

#[test]
fn head_tail_buffer_default_preserves_prefix_and_suffix() {
    let mut buffer = HeadTailBuffer::default();
    buffer.push_chunk(vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
    buffer.push_chunk(b"bc".to_vec());

    let rendered = buffer.to_bytes();
    assert_eq!(rendered.first(), Some(&b'a'));
    assert!(rendered.ends_with(b"bc"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_persists_across_requests() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();

    let open_shell = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell.process_id.expect("expected process_id");
    assert_eq!(
        session.list_background_terminals().await,
        vec![BackgroundTerminalInfo {
            item_id: "call".to_string(),
            process_id: process_id.to_string(),
            command: "bash -i".to_string(),
            cwd: cwd.into(),
            background_description: None,
            background_triggers: Vec::new(),
        }]
    );

    write_stdin(
        &session,
        process_id,
        "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = write_stdin(
        &session,
        process_id,
        "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;
    assert!(
        out_2
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex"),
        "expected environment variable output"
    );

    assert!(session.terminate_background_terminal(process_id).await);
    assert!(!session.terminate_background_terminal(process_id).await);
    assert!(session.list_background_terminals().await.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_unified_exec_sessions() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;

    let shell_a = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let session_a = shell_a.process_id.expect("expected process id");

    write_stdin(
        &session,
        session_a,
        "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = exec_command(
        &session,
        &turn,
        "echo $CODEX_INTERACTIVE_SHELL_VAR",
        /*yield_time_ms*/ 2_500,
        /*workdir*/ None,
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        out_2.process_id.is_none(),
        "short command should not report a process id if it exits quickly"
    );
    assert!(
        !out_2
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex"),
        "short command should run in a fresh shell"
    );

    let out_3 = write_stdin(
        &session,
        shell_a.process_id.expect("expected process id"),
        "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;
    assert!(
        out_3
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex"),
        "session should preserve state"
    );

    Ok(())
}

#[tokio::test]
async fn unified_exec_timeouts() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    const TEST_VAR_VALUE: &str = "unified_exec_var_123";

    let (session, turn) = test_session_and_turn().await;

    let open_shell = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell.process_id.expect("expected process id");

    write_stdin(
        &session,
        process_id,
        format!("export CODEX_INTERACTIVE_SHELL_VAR={TEST_VAR_VALUE}\n").as_str(),
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = write_stdin(
        &session,
        process_id,
        "sleep 5 && echo $CODEX_INTERACTIVE_SHELL_VAR\n",
        /*yield_time_ms*/ 10,
    )
    .await?;
    assert!(
        !out_2
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains(TEST_VAR_VALUE),
        "timeout too short should yield incomplete output"
    );

    tokio::time::sleep(Duration::from_secs(7)).await;

    let out_3 = write_stdin(&session, process_id, "", /*yield_time_ms*/ 100).await?;

    assert!(
        out_3
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains(TEST_VAR_VALUE),
        "subsequent poll should retrieve output"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_pause_blocks_yield_timeout() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;
    session.set_out_of_band_elicitation_pause_state(/*paused*/ true);

    let paused_session = Arc::clone(&session);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        paused_session.set_out_of_band_elicitation_pause_state(/*paused*/ false);
    });

    let started = tokio::time::Instant::now();
    let response = exec_command(
        &session,
        &turn,
        "sleep 1 && echo unified-exec-done",
        /*yield_time_ms*/ 250,
        /*workdir*/ None,
    )
    .await?;

    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "pause should block the unified exec yield timeout"
    );
    assert!(
        response
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("unified-exec-done"),
        "exec_command should wait for output after the pause lifts"
    );
    assert!(
        response.process_id.is_none(),
        "completed command should not leave a background process"
    );

    Ok(())
}

#[tokio::test]
#[ignore] // Ignored while we have a better way to test this.
async fn requests_with_large_timeout_are_capped() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;

    let result = exec_command(
        &session,
        &turn,
        "echo codex",
        /*yield_time_ms*/ 120_000,
        /*workdir*/ None,
    )
    .await?;

    assert!(result.process_id.is_some());
    assert!(
        result
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex")
    );

    Ok(())
}

#[tokio::test]
#[ignore] // Ignored while we have a better way to test this.
async fn completed_commands_do_not_persist_sessions() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let result = exec_command(
        &session,
        &turn,
        "echo codex",
        /*yield_time_ms*/ 2_500,
        /*workdir*/ None,
    )
    .await?;

    assert!(
        result.process_id.is_some(),
        "completed command should report a process id"
    );
    assert!(
        result
            .truncated_output(DEFAULT_MAX_OUTPUT_TOKENS)
            .contains("codex")
    );

    assert!(
        session
            .services
            .unified_exec_manager
            .process_store
            .lock()
            .await
            .processes
            .is_empty()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reusing_completed_process_returns_unknown_process() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn) = test_session_and_turn().await;

    let open_shell = exec_command(
        &session, &turn, "bash -i", /*yield_time_ms*/ 2_500, /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell.process_id.expect("expected process id");

    write_stdin(&session, process_id, "exit\n", /*yield_time_ms*/ 2_500).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let err = write_stdin(&session, process_id, "", /*yield_time_ms*/ 100)
        .await
        .expect_err("expected unknown process error");

    match err {
        UnifiedExecError::UnknownProcessId { process_id: err_id } => {
            assert_eq!(err_id, process_id, "process id should match request");
        }
        other => panic!("expected UnknownProcessId, got {other:?}"),
    }

    assert!(
        session
            .services
            .unified_exec_manager
            .process_store
            .lock()
            .await
            .processes
            .is_empty()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminating_initial_exec_command_rechecks_initial_response_state() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (terminate_started_tx, mut terminate_started_rx) = watch::channel(false);
    let allow_terminate = Arc::new(Notify::new());
    let process = blocking_terminate_unified_process(
        process_id,
        terminate_started_tx,
        Arc::clone(&allow_terminate),
    )
    .await?;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process,
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hook_command: "sleep 60".to_string(),
            tty: true,
            network_approval: None,
            background_description: None,
            background_triggers: Vec::new(),
            session: Arc::downgrade(&session),
            last_used: Instant::now(),
            started_at_unix_ms: crate::turn_timing::now_unix_timestamp_ms(),
        },
    );

    let terminate_task = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.terminate_background_terminal(process_id).await }
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        terminate_started_rx.wait_for(|started| *started),
    )
    .await
    .expect("terminate should start")
    .expect("terminate signal sender should stay open");

    {
        let mut store = manager.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .expect("process should remain stored until initial response returns");
        entry
            .initial_exec_command_active
            .store(false, std::sync::atomic::Ordering::Release);
    }

    allow_terminate.notify_waiters();
    let terminated = tokio::time::timeout(Duration::from_secs(2), terminate_task)
        .await
        .expect("terminate should finish")
        .expect("terminate task should not panic");
    assert!(terminated);
    assert!(
        !manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&process_id)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminating_during_stdin_poll_returns_exited_response() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (terminate_started_tx, _terminate_started_rx) = watch::channel(false);
    let allow_terminate = Arc::new(Notify::new());
    let process = blocking_terminate_unified_process(
        process_id,
        terminate_started_tx,
        Arc::clone(&allow_terminate),
    )
    .await?;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    let last_used = Instant::now() - Duration::from_secs(1);
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process: Arc::clone(&process),
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_command: "sleep 60".to_string(),
            tty: true,
            network_approval: None,
            background_description: None,
            background_triggers: Vec::new(),
            session: Arc::downgrade(&session),
            last_used,
            started_at_unix_ms: crate::turn_timing::now_unix_timestamp_ms(),
        },
    );

    let poll_task = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            write_stdin(&session, process_id, "", /*yield_time_ms*/ 60_000).await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let poll_started = manager
                .process_store
                .lock()
                .await
                .processes
                .get(&process_id)
                .is_some_and(|entry| entry.last_used != last_used);
            if poll_started {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("poll should clone process handles");

    manager.release_process_id(process_id).await;
    allow_terminate.notify_one();
    process.terminate_confirmed().await?;

    let output = tokio::time::timeout(Duration::from_secs(2), poll_task)
        .await
        .expect("poll should finish")
        .expect("poll task should not panic")?;
    assert_eq!(output.process_id, None);
    assert!(manager.process_store.lock().await.processes.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_pipe_commands_preserve_exit_code() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    let request = test_exec_request(
        &turn,
        vec!["bash".to_string(), "-lc".to_string(), "exit 17".to_string()],
        cwd,
        shell_env(),
    );

    let environment = codex_exec_server::Environment::default_for_tests();
    let process = UnifiedExecProcessManager::default()
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tty*/ false,
            Box::new(NoopSpawnLifecycle),
            &environment,
            None,
        )
        .await?;

    if !process.has_exited() {
        let exit_signal = process.cancellation_token();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), exit_signal.cancelled())
                .await
                .is_ok(),
            "process did not report exit within timeout"
        );
    }

    assert!(process.has_exited());
    assert_eq!(process.exit_code(), Some(17));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_uses_remote_exec_server_when_configured() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let remote_test_env = remote_test_env().await?;
    let (_, turn) = make_session_and_context().await;
    let request = test_exec_request(
        &turn,
        vec!["bash".to_string(), "-i".to_string()],
        remote_test_env.cwd().clone(),
        shell_env(),
    );

    let manager = UnifiedExecProcessManager::default();
    let process = manager
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tty*/ true,
            Box::new(NoopSpawnLifecycle),
            remote_test_env.environment(),
            None,
        )
        .await?;

    process.write(b"printf 'remote-unified-exec\\n'\n").await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let crate::unified_exec::process::OutputHandles {
        output_buffer,
        output_notify,
        output_closed,
        output_closed_notify,
        cancellation_token,
    } = process.output_handles();
    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        /*pause_state*/ None,
        Instant::now() + Duration::from_millis(2_500),
    )
    .await;

    assert!(String::from_utf8_lossy(&collected).contains("remote-unified-exec"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_exec_server_rejects_inherited_fd_launches() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let remote_test_env = remote_test_env().await?;
    let (_, mut turn) = make_session_and_context().await;
    turn.environments.turn_environments[0].environment =
        Arc::new(remote_test_env.environment().clone());

    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    let request = test_exec_request(
        &turn,
        vec!["bash".to_string(), "-lc".to_string(), "echo ok".to_string()],
        cwd,
        shell_env(),
    );

    let manager = UnifiedExecProcessManager::default();
    let err = manager
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tty*/ true,
            Box::new(TestSpawnLifecycle {
                inherited_fds: vec![42],
            }),
            turn.environments
                .primary()
                .expect("turn environment")
                .environment
                .as_ref(),
            None,
        )
        .await
        .expect_err("expected inherited fd rejection");

    assert_eq!(
        err.to_string(),
        "Failed to create unified exec process: remote exec-server does not support inherited file descriptors"
    );
    Ok(())
}
