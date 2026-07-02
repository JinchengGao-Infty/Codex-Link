use super::*;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::turn_context::TurnEnvironment;
use codex_exec_server::Environment;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::items::AgentMessageContent;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some("msg-1".to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn local_environments(cwd: &Path) -> TurnEnvironmentSnapshot {
    TurnEnvironmentSnapshot {
        turn_environments: vec![TurnEnvironment::new(
            "local".to_string(),
            Arc::new(Environment::create_for_tests(/*exec_server_url*/ None).expect("environment")),
            PathUri::from_host_native_path(cwd).expect("cwd URI"),
            /*shell*/ None,
        )],
        starting: Vec::new(),
    }
}

#[tokio::test]
async fn agents_md_focus_paths_are_collected_from_text_file_paths() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let environments = local_environments(cwd.path());
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "fix `codex-rs/tui/src/app.rs`, then check https://example.com/a/b".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];

    let focus_paths = collect_agents_md_focus_paths(&input, &environments);
    let expected_path = environments.turn_environments[0]
        .cwd()
        .join("codex-rs/tui/src/app.rs")
        .expect("expected focus path");

    assert_eq!(
        focus_paths,
        vec![AgentsMdFocusPath {
            environment_id: "local".to_string(),
            path: expected_path,
        }]
    );
}

#[tokio::test]
async fn agents_md_focus_paths_ignore_urls_and_non_path_text() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let environments = local_environments(cwd.path());
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "read https://example.com/a/b and summarize normally".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];

    assert_eq!(
        collect_agents_md_focus_paths(&input, &environments),
        Vec::<AgentsMdFocusPath>::new()
    );
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}
