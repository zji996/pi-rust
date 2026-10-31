//! Minimal P0 JSONL protocol for the native rpi RPC host.
use serde_json::{json, Value};

pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const PI_BASELINE: &str = "1.2.0";

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(flatten)]
    pub command: Command,
}

/// Payload schema for `Request`. Only deserialization matters: the dispatcher
/// matches on the raw `type` string, so the parsed fields are never read.
#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Prompt {
        #[serde(flatten)]
        content: PromptPayload,
    },
    Abort,
    GetState,
    /// Versioned, bounded exact startup resource observations.
    GetResourceState {
        version: Option<u32>,
    },
    /// List extension slash commands (name and description).
    GetCommands,
    /// Invoke an extension handler and return its JSON result as response data.
    /// `command` accepts a registered name with or without a leading slash.
    ExecuteCommand {
        command: String,
        #[serde(default)]
        args: String,
    },
    /// Read-only snapshot of the current branch's message entries.
    GetMessages {
        limit: Option<u64>,
        #[serde(rename = "beforeId")]
        before_id: Option<String>,
    },
    /// Optional bounded user summaries and transcript windows.
    GetMessageSummaries {
        limit: Option<u64>,
        role: Option<String>,
        #[serde(rename = "beforeId")]
        before_id: Option<String>,
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    GetMessageWindow {
        limit: Option<u64>,
        #[serde(rename = "anchorId")]
        anchor_id: Option<String>,
        #[serde(rename = "beforeId")]
        before_id: Option<String>,
        #[serde(rename = "afterId")]
        after_id: Option<String>,
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    GetHistory {
        limit: Option<u64>,
        #[serde(rename = "afterSeq")]
        after_seq: Option<u64>,
        #[serde(rename = "beforeSeq")]
        before_seq: Option<u64>,
    },
    /// Query custom entries on the selected branch. Query validation is shared
    /// with the runtime action (required non-empty `customTypes`; optional
    /// `afterSeq`, `limit`, `order`), after removing the RPC envelope.
    GetSessionBranch,
    /// Append a protocol-neutral custom entry to the current branch.
    AppendEntry {
        #[serde(rename = "customType")]
        custom_type: String,
        data: Option<Value>,
    },
    GetStats,
    GetQueue,
    QueueMessage {
        message: String,
        #[serde(rename = "deliverAs")]
        deliver_as: Option<String>,
    },
    CancelQueued {
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    ClearQueue,
    Navigate {
        #[serde(rename = "targetId")]
        target_id: Option<String>,
    },
    /// Continue the run that recovery left interrupted.
    ResumePending,
    /// Rewrite the model's view of the branch into a summary.
    Compact {
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
    },
    GetAvailableModels,
    GetAuthStatus,
    #[serde(rename = "oauth")]
    OAuth {
        provider: String,
        action: String,
        input: Option<Value>,
    },
    SetApiKey {
        provider: String,
        #[serde(rename = "apiKey")]
        api_key: String,
    },
    SetModel {
        provider: Option<String>,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    GetAvailableThinkingLevels,
    SetThinkingLevel {
        level: rpi_ai::types::ThinkingLevel,
    },
    ConfigureFileIo {
        #[serde(rename = "readText")]
        read_text: bool,
        #[serde(rename = "writeText")]
        write_text: bool,
    },
    FileIoResponse {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    ConfigureExternalTools {
        tools: Vec<rpi_tools::tools::external::ExternalToolDefinition>,
    },
    // Outcome validation is presence-aware in ExternalTools::respond: Option
    // would conflate explicit null with an absent outcome.
    ExternalToolResponse {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    GetAvailableTools,
    SetActiveTools {
        tools: Vec<String>,
    },
    UiDialogResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        answer: Option<Value>,
        #[serde(default)]
        cancelled: bool,
    },
    /// The host's answer to a `host_state_call`.
    HostStateResult {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(default)]
        text: String,
        #[serde(rename = "isError", default)]
        is_error: bool,
    },
    /// The host's timeouts for `--host-state` calls.
    HostStateConfig {
        #[serde(rename = "timeoutMs", default)]
        timeout_ms: Option<u64>,
        #[serde(rename = "waitTimeoutMs", default)]
        wait_timeout_ms: Option<u64>,
        #[serde(rename = "waitPaths", default)]
        wait_paths: Option<Vec<String>>,
    },
    Ping,
    Stop,
}

/// Shape-only deserialization; pixel validation belongs to the owned run worker.
/// Presence, not serde aliases, determines legacy precedence.
#[allow(dead_code)] // Shape-only schema; production dispatch uses the owned raw frame.
#[derive(Debug)]
pub enum PromptPayload {
    Legacy(String),
    Blocks(Vec<Value>),
}
impl<'de> serde::Deserialize<'de> for PromptPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <Value as serde::Deserialize>::deserialize(deserializer)?;
        let invalid = |error: String| <D::Error as serde::de::Error>::custom(error);
        super::prompt_content::validate_prompt_shape(&value).map_err(invalid)?;
        if let Some(blocks) = value.get("contentBlocks") {
            Ok(Self::Blocks(
                blocks.as_array().expect("validated blocks").clone(),
            ))
        } else {
            Ok(Self::Legacy(
                value
                    .get("message")
                    .or_else(|| value.get("content"))
                    .or_else(|| value.get("prompt"))
                    .and_then(Value::as_str)
                    .expect("validated legacy string")
                    .to_owned(),
            ))
        }
    }
}

pub const SUPPORTED_COMMANDS: &[&str] = &[
    "get_resource_state",
    "get_state",
    "get_commands",
    "execute_command",
    "get_messages",
    "get_message_summaries",
    "get_message_window",
    "get_history",
    "get_session_branch",
    "append_entry",
    "get_stats",
    "get_queue",
    "queue_message",
    "cancel_queued",
    "clear_queue",
    "navigate",
    "prompt",
    "resume_pending",
    "compact",
    "abort",
    "get_available_models",
    "get_auth_status",
    "oauth",
    "set_api_key",
    "set_model",
    "get_available_thinking_levels",
    "set_thinking_level",
    "configure_file_io",
    "file_io_response",
    "configure_external_tools",
    "external_tool_response",
    "get_available_tools",
    "set_active_tools",
    "ui_dialog_response",
    "host_state_result",
    "host_state_config",
    "ping",
    "stop",
];

pub fn response(id: Option<Value>, command: &str, result: Result<Option<Value>, String>) -> Value {
    let mut out = json!({
        "type": "response",
        "command": command,
        "success": result.is_ok(),
        // Keep the original remote-client response fields during the RPC
        // protocol transition. New clients use success/data; older clients
        // still require status/result to deserialize the response.
        "status": if result.is_ok() { "ok" } else { "error" },
    });
    if let Some(id) = id {
        out["id"] = id;
    }
    match result {
        Ok(Some(data)) => {
            out["data"] = data.clone();
            out["result"] = data;
        }
        Ok(None) => {}
        Err(error) => out["error"] = json!(error),
    }
    out
}

pub fn ready(
    mode: crate::args::RpcMessageUpdates,
    host_state: Option<&crate::host_state::HostStateAdvertisement>,
) -> Value {
    // `--host-state` is off by default, and then `ready` is upstream's payload
    // byte for byte: the feature's command, events and settings only appear when
    // the flag is given.
    let enabled = host_state.is_some();
    let commands: Vec<&str> = SUPPORTED_COMMANDS
        .iter()
        .copied()
        .filter(|command| enabled || !matches!(*command, "host_state_result" | "host_state_config"))
        .collect();
    let mut events = vec![
        "session_entry_committed",
        "external_tool_request",
        "external_tool_request_closed",
        "file_io_request",
        "file_io_request_closed",
    ];
    if enabled {
        events.push("host_state_call");
        events.push("host_state_cancelled");
    }
    let mut value = json!({
        "type": "ready",
        "protocol": "rpi",
        "piCompatibilityTarget": PI_BASELINE,
        "supportedCommands": commands,
        "supportedEvents": events,
        "promptContent": super::prompt_content::prompt_capabilities(),
        "fileIO": super::file_io::capabilities(),
        "semantics": {
            "messageUpdates": mode.as_str(),
            "messageIds": "sessionEntryId-v1",
            "promptResponse": "accepted",
            "settlementEvent": "agent_settled",
            "agentEndIsTerminal": false
        },
        "limits": {"frameBytes": MAX_FRAME_BYTES, "maxConcurrentPrompts": 1},
        "parityComplete": false
    });
    if let Some(host_state) = host_state {
        value["hostState"] = json!(host_state);
    }
    value
}

/// Only expose capability metadata, never provider credentials or headers.
pub fn model_descriptor(model: &rpi_ai::Model) -> Value {
    json!({
        "id": model.id,
        "name": model.name,
        "api": model.api,
        "provider": model.provider,
        "reasoning": model.reasoning,
        "input": model.input,
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_commands_are_unique() {
        let commands: std::collections::HashSet<_> = SUPPORTED_COMMANDS.iter().collect();
        assert_eq!(commands.len(), SUPPORTED_COMMANDS.len());
    }

    #[test]
    fn extension_commands_are_advertised_and_args_default_to_empty() {
        for name in ["get_commands", "execute_command"] {
            assert!(SUPPORTED_COMMANDS.contains(&name));
        }
        let request: Request = serde_json::from_value(json!({
            "type": "execute_command", "command": "/example"
        }))
        .unwrap();
        assert!(
            matches!(request.command, Command::ExecuteCommand { command, args }
            if command == "/example" && args.is_empty())
        );
        assert!(serde_json::from_value::<Request>(json!({
            "type": "execute_command", "command": "example", "args": 42
        }))
        .is_err());
    }

    #[test]
    fn ready_advertises_the_p0_contract() {
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            assert_eq!(
                ready(mode, None)["semantics"]["messageUpdates"],
                mode.as_str()
            );
        }
        let value = ready(crate::args::RpcMessageUpdates::default(), None);
        assert_eq!(value["protocol"], "rpi");
        assert_eq!(value["semantics"]["messageIds"], "sessionEntryId-v1");
        assert_eq!(value["semantics"]["settlementEvent"], "agent_settled");
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "prompt"));
    }

    #[test]
    fn ready_advertises_the_read_only_history_snapshot() {
        let value = ready(crate::args::RpcMessageUpdates::default(), None);
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "get_messages"));
    }

    #[test]
    fn ready_advertises_resuming_an_interrupted_run() {
        let value = ready(crate::args::RpcMessageUpdates::default(), None);
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "resume_pending"));
    }

    #[test]
    fn ready_advertises_compaction() {
        let value = ready(crate::args::RpcMessageUpdates::default(), None);
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "compact"));
    }

    #[test]
    fn compact_payload_carries_instructions() {
        let request: Request = serde_json::from_value(json!({
            "type": "compact", "id": 5, "customInstructions": "keep the decisions"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::Compact {
                custom_instructions: Some(_)
            }
        ));
    }

    #[test]
    fn resume_pending_is_a_bare_command() {
        let request: Request =
            serde_json::from_value(json!({"type": "resume_pending", "id": 4})).unwrap();
        assert!(matches!(request.command, Command::ResumePending));
    }

    #[test]
    fn get_messages_payload_keeps_its_limit_and_cursor() {
        let request: Request = serde_json::from_value(json!({
            "type": "get_messages", "id": 3, "limit": 25, "beforeId": "entry-12"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::GetMessages {
                limit: Some(25),
                before_id: Some(_)
            }
        ));
    }

    #[test]
    fn auth_commands_keep_secret_fields_in_request_only() {
        let request: Request = serde_json::from_value(json!({
            "type": "set_api_key",
            "id": 8,
            "provider": "anthropic",
            "apiKey": "secret-fixture"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::SetApiKey { provider, api_key }
                if provider == "anthropic" && api_key == "secret-fixture"
        ));
        let status = response(
            Some(json!(8)),
            "set_api_key",
            Ok(Some(json!({
                "provider": "anthropic",
                "ready": true,
                "source": "auth.json",
                "requiresRestart": true
            }))),
        );
        assert!(!status.to_string().contains("secret-fixture"));
    }
    #[test]
    fn prompt_payload_accepts_legacy_content_and_prompt_fields() {
        for field in ["content", "prompt"] {
            let request: Request = serde_json::from_value(json!({
                "type": "prompt",
                field: "legacy text"
            }))
            .unwrap();
            assert!(
                matches!(request.command, Command::Prompt { content: PromptPayload::Legacy(message) } if message == "legacy text")
            );
        }
    }

    #[test]
    fn oauth_payload_keeps_provider_action_and_input() {
        let request: Request = serde_json::from_value(json!({
            "type": "oauth",
            "id": 9,
            "provider": "example",
            "action": "begin",
            "input": {"code": "fixture"}
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::OAuth { provider, action, input: Some(input) }
                if provider == "example"
                    && action == "begin"
                    && input["code"] == "fixture"
        ));
    }

    #[test]
    fn ready_without_host_state_hides_its_commands_and_events() {
        // `--host-state` is off by default: the payload is upstream's, so a
        // host that does not use the flag sees no trace of it.
        let value = ready(crate::args::RpcMessageUpdates::default(), None);
        assert!(value.get("hostState").is_none());
        let commands = value["supportedCommands"].as_array().unwrap();
        for command in ["host_state_result", "host_state_config"] {
            assert!(!commands.iter().any(|value| value == command), "{command}");
        }
        let events = value["supportedEvents"].as_array().unwrap();
        for event in ["host_state_call", "host_state_cancelled"] {
            assert!(!events.iter().any(|value| value == event), "{event}");
        }
    }

    #[test]
    fn ready_advertises_host_state_when_enabled() {
        let advertisement = crate::host_state::HostStateAdvertisement {
            prefix: ".mu/state".to_string(),
            timeout_ms: 30_000,
            wait_timeout_ms: 120_000,
        };
        let value = ready(
            crate::args::RpcMessageUpdates::default(),
            Some(&advertisement),
        );
        let commands = value["supportedCommands"].as_array().unwrap();
        for command in ["host_state_result", "host_state_config"] {
            assert!(commands.iter().any(|value| value == command), "{command}");
        }
        let events = value["supportedEvents"].as_array().unwrap();
        for event in ["host_state_call", "host_state_cancelled"] {
            assert!(events.iter().any(|value| value == event), "{event}");
        }
        assert_eq!(
            value["hostState"],
            json!({"prefix": ".mu/state", "timeoutMs": 30_000, "waitTimeoutMs": 120_000})
        );
    }

    #[test]
    fn host_state_command_payloads_keep_their_fields() {
        let request: Request = serde_json::from_value(json!({
            "type": "host_state_result", "requestId": "r1", "text": "Asked topic.", "isError": true
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::HostStateResult { request_id, text, is_error }
                if request_id == "r1" && text == "Asked topic." && is_error
        ));
        // `text` and `isError` are optional; a bare acceptance is not enough to
        // read as success, but it must parse.
        let request: Request = serde_json::from_value(json!({
            "type": "host_state_result", "requestId": "r2"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::HostStateResult { request_id, text, is_error }
                if request_id == "r2" && text.is_empty() && !is_error
        ));

        let request: Request = serde_json::from_value(json!({
            "type": "host_state_config", "timeoutMs": 1000, "waitTimeoutMs": 5000,
            "waitPaths": ["ask"]
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::HostStateConfig { timeout_ms, wait_timeout_ms, wait_paths }
                if timeout_ms == Some(1000) && wait_timeout_ms == Some(5000)
                    && wait_paths == Some(vec!["ask".to_string()])
        ));
        // Every field is optional: setting one timeout leaves the other values.
        let request: Request = serde_json::from_value(json!({
            "type": "host_state_config", "timeoutMs": 1000
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::HostStateConfig { timeout_ms, wait_timeout_ms, wait_paths }
                if timeout_ms == Some(1000) && wait_timeout_ms.is_none() && wait_paths.is_none()
        ));
    }

    #[test]
    fn response_has_canonical_success_data_shape() {
        let value = response(
            Some(json!(7)),
            "prompt",
            Ok(Some(json!({"accepted": true}))),
        );
        assert_eq!(value["type"], "response");
        assert_eq!(value["success"], true);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["data"]["accepted"], true);
        assert_eq!(value["result"]["accepted"], true);
        let legacy: crate::remote::protocol::RemoteResponse =
            serde_json::from_value(value).unwrap();
        assert!(legacy.is_ok());
        assert_eq!(legacy.result.unwrap()["accepted"], true);
    }
}
