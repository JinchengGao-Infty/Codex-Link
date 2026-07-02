//! Model-facing tool for recording durable task facts into the Link context
//! capsule. This is the write path for model-side facts (decisions, plan
//! updates, blockers, verified observations) that the host cannot observe on
//! its own and that must survive compaction.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use serde::Deserialize;
use serde_json::json;

use crate::LinkContextStore;
use crate::MAX_RECORDED_TOOL_EVENTS;
use crate::evidence::EvidenceKind;
use crate::evidence::EvidenceRecord;
use crate::evidence::push_evidence;

pub const RECORD_LINK_CONTEXT_TOOL_NAME: &str = "record_link_context";

#[derive(Clone)]
pub(crate) struct RecordLinkContextTool {
    store: Arc<LinkContextStore>,
}

impl RecordLinkContextTool {
    pub(crate) fn new(store: Arc<LinkContextStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RecordLinkContextArgs {
    kind: RecordKind,
    summary: String,
    related_paths: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordKind {
    Decision,
    Observation,
    TestResult,
    PlanUpdate,
    Blocker,
}

impl From<RecordKind> for EvidenceKind {
    fn from(kind: RecordKind) -> Self {
        match kind {
            RecordKind::Decision => EvidenceKind::Decision,
            RecordKind::Observation => EvidenceKind::Observation,
            RecordKind::TestResult => EvidenceKind::TestResult,
            RecordKind::PlanUpdate => EvidenceKind::PlanUpdate,
            RecordKind::Blocker => EvidenceKind::Blocker,
        }
    }
}

impl ToolExecutor<ToolCall> for RecordLinkContextTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(RECORD_LINK_CONTEXT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_record_link_context_tool()
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: RecordLinkContextArgs =
                serde_json::from_str(invocation.function_arguments()?)
                    .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            let summary = args.summary.trim().to_string();
            if summary.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "summary must not be empty".to_string(),
                ));
            }

            let record = EvidenceRecord::model(args.kind.into(), summary.clone())
                .with_source_ref(format!(
                    "turn {}, call {}",
                    invocation.turn_id, invocation.call_id
                ))
                .with_related_paths(args.related_paths.unwrap_or_default());
            self.store.update(|state| {
                match args.kind {
                    RecordKind::PlanUpdate => {
                        state.next_action = Some(summary.clone());
                    }
                    RecordKind::Blocker => {
                        if !state.open_blockers.iter().any(|value| value == &summary) {
                            state.open_blockers.push(summary.clone());
                        }
                    }
                    RecordKind::Decision | RecordKind::Observation | RecordKind::TestResult => {}
                }
                push_evidence(
                    &mut state.verified_evidence,
                    record,
                    MAX_RECORDED_TOOL_EVENTS,
                );
            });

            let output: Box<dyn ToolOutput> =
                Box::new(JsonToolOutput::new(json!({ "recorded": true })));
            Ok(output)
        })
    }
}

fn create_record_link_context_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "kind".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("decision"),
                    json!("observation"),
                    json!("test_result"),
                    json!("plan_update"),
                    json!("blocker"),
                ],
                Some(
                    "Required. `decision`: a choice you made and why. `observation`: a verified fact about the code or environment. `test_result`: an actually observed test/command outcome. `plan_update`: the next concrete action (also updates the capsule's next-action field). `blocker`: something that prevents progress (also added to open blockers)."
                        .to_string(),
                ),
            ),
        ),
        (
            "summary".to_string(),
            JsonSchema::string(Some(
                "Required. One or two sentences stating the fact. Keep exact identifiers (paths, symbols, commands, error text) verbatim so they remain searchable."
                    .to_string(),
            )),
        ),
        (
            "related_paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("File paths this fact is about.".to_string()),
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: RECORD_LINK_CONTEXT_TOOL_NAME.to_string(),
        description: r#"Record a durable task fact into the Link context capsule. The capsule is re-injected every turn and survives context compaction and process restarts, so use this for load-bearing facts that must not be lost: decisions and their rationale, verified observations or test results, plan updates, and blockers.
Do not use it for routine narration, speculation, or anything you have not verified. Record a fact once; re-recording the same summary is a no-op."#
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["kind".to_string(), "summary".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
