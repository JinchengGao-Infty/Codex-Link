use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const JOB_OBSERVE_TOOL_NAME: &str = "job_observe";
pub(crate) const JOB_CANCEL_TOOL_NAME: &str = "job_cancel";

pub(crate) fn create_job_observe_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("list"),
                    json!("status"),
                    json!("tail"),
                    json!("read"),
                    json!("wait"),
                ],
                Some(
                    "Required. `list`: all live sessions. `status`: one session's state, metadata, and log file. `tail`: last lines of the session's durable log (works after exit too). `read`: a byte range of the log. `wait`: block locally until the session exits or the timeout elapses, then return its exit code and recent output."
                        .to_string(),
                ),
            ),
        ),
        (
            "session_id".to_string(),
            JsonSchema::integer(Some(
                "The exec session id. Required for every action except `list`.".to_string(),
            )),
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::integer(Some(
                "`wait` only. How long to wait for exit, in milliseconds. Defaults to 30000; clamped to [1000, 600000]. On timeout the session keeps running and you can wait again."
                    .to_string(),
            )),
        ),
        (
            "tail_lines".to_string(),
            JsonSchema::integer(Some(
                "`tail` only. Number of trailing log lines to return. Defaults to 50; clamped to [1, 400].".to_string(),
            )),
        ),
        (
            "offset_bytes".to_string(),
            JsonSchema::integer(Some(
                "`read` only. Byte offset into the log file to start reading from. Defaults to 0."
                    .to_string(),
            )),
        ),
        (
            "limit_bytes".to_string(),
            JsonSchema::integer(Some(
                "`read` only. Maximum bytes to return. Defaults to 8192; clamped to [1, 16384]."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: JOB_OBSERVE_TOOL_NAME.to_string(),
        description: r#"Observe background exec sessions locally, without polling. One call returns one bounded result: the harness does the waiting and file reading, so watching a long-running job costs no turns between state changes.
Prefer `wait` (with a generous timeout) or `tail` over polling with empty `write_stdin` calls. Every session writes a durable log file that outlives the process; `tail` and `read` work on finished jobs too."#
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["action".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn create_job_cancel_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "session_id".to_string(),
        JsonSchema::integer(Some("The exec session id to terminate.".to_string())),
    )]);

    ToolSpec::Function(ResponsesApiTool {
        name: JOB_CANCEL_TOOL_NAME.to_string(),
        description: "Terminate a background exec session. The session's durable log file is kept."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["session_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
