//! One English next-step line after a failed built-in tool call, in the spirit
//! of Norvane's `tool_error_text` (`crates/kernel/src/tool_output.rs`).
//!
//! The error text itself is never touched: an `edit` diagnostic quotes the text
//! the model has to copy, so the hint is appended on its own line and the
//! original bytes stay first. Nothing here runs unless the host turns the
//! setting on (`toolErrorHints` in `settings.json`) — see
//! [`with_next_steps`] — so a default run sees upstream's messages verbatim.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use tokio_util::sync::CancellationToken;

/// The next step for a failed call, or `None` when the failure needs no advice
/// (an abort the user caused, a plain non-zero exit, an unclassified error).
///
/// `tool` is the tool's name and `message` its error text without the
/// `tool error: ` / `validation error: ` prefix the agent loop adds.
pub fn next_step(tool: &str, message: &str) -> Option<&'static str> {
    let m = message.trim();
    // A cancelled call (user abort, sibling failure) is not a mistake to
    // correct: advice would read as a demand to retry what the user stopped.
    if m.contains("aborted") || m.contains("[cancelled]") || m.contains("Operation aborted") {
        return None;
    }
    if m.contains("timed out") || m.starts_with("timeout") {
        return Some("Raise the timeout or make the command finish sooner.");
    }
    if tool == "edit"
        && (m.contains("oldText not found")
            || m.contains("oldText is empty")
            || m.contains("closest original"))
    {
        return Some(
            "Read the file, copy the original text exactly with enough context, then retry edit.",
        );
    }
    if m.contains("not_found")
        || m.contains("path not found")
        || m.contains("not a file")
        || m.contains("is_directory")
        || m.contains("not_directory")
        || m.contains("permission_denied")
    {
        return Some("Use ls or find to check the path.");
    }
    if m.contains("beyond end of file") || m.contains("Offset") {
        return Some("Read the file without an offset, or use an offset inside its line count.");
    }
    if m.contains("input invalid") || m.contains("validation error") {
        return Some("Check the tool arguments and retry with valid values.");
    }
    None
}

/// [`error_with_next_step`] for an already-typed error.
pub fn error_with_next_step(tool: &str, error: AgentError) -> AgentError {
    match error {
        AgentError::Tool(message) => match next_step(tool, &message) {
            Some(hint) => AgentError::Tool(format!("{message}\n{hint}")),
            None => AgentError::Tool(message),
        },
        AgentError::Validation(message) => match next_step(tool, &message) {
            Some(hint) => AgentError::Validation(format!("{message}\n{hint}")),
            None => AgentError::Validation(message),
        },
        other => other,
    }
}

/// Wrap `inner` so every error it raises gains the next-step line. The schema,
/// label, execution mode and argument preparation pass straight through, so the
/// wrapped tool is the same tool with a different error tail.
pub fn with_next_steps(inner: Arc<dyn AgentTool>) -> Arc<dyn AgentTool> {
    Arc::new(HintedTool { inner })
}

struct HintedTool {
    inner: Arc<dyn AgentTool>,
}

#[async_trait]
impl AgentTool for HintedTool {
    fn schema(&self) -> &Tool {
        self.inner.schema()
    }

    fn label(&self) -> &str {
        self.inner.label()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.inner.execution_mode()
    }

    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        self.inner
            .prepare_arguments(args)
            .map_err(|error| error_with_next_step(&self.inner.schema().name, error))
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.inner
            .execute(tool_call_id, params, signal, on_update)
            .await
            .map_err(|error| error_with_next_step(&self.inner.schema().name, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_cover_the_common_failures_and_leave_the_rest_alone() {
        assert_eq!(
            next_step("read", "not_found: path not found: /w/a.md (path: /w/a.md)"),
            Some("Use ls or find to check the path.")
        );
        assert_eq!(
            next_step(
                "edit",
                "error: edits[0].oldText not found in /w/a.md\n…closest original…"
            ),
            Some("Read the file, copy the original text exactly with enough context, then retry edit.")
        );
        assert_eq!(
            next_step("bash", "Command timed out after 2s"),
            Some("Raise the timeout or make the command finish sooner.")
        );
        assert_eq!(
            next_step("bash", "bash input invalid: missing field `command`"),
            Some("Check the tool arguments and retry with valid values.")
        );
        // No advice where retrying is wrong or the failure is self-explanatory.
        assert_eq!(next_step("bash", "aborted: operation aborted"), None);
        assert_eq!(next_step("bash", "Command exited with code 7"), None);
        assert_eq!(next_step("bash", "[exit code 7]"), None);
        assert_eq!(next_step("write", "Successfully wrote to a.md"), None);
    }

    /// The default (unwrapped) tool message and the wrapped one differ by
    /// exactly one appended line; the error text itself is untouched.
    #[tokio::test]
    async fn wrapping_appends_one_line_and_keeps_the_original_text() {
        struct Failing;
        #[async_trait]
        impl AgentTool for Failing {
            fn schema(&self) -> &Tool {
                static TOOL: std::sync::OnceLock<Tool> = std::sync::OnceLock::new();
                TOOL.get_or_init(|| Tool {
                    name: "read".into(),
                    description: String::new(),
                    parameters: rpi_ai::types::Schema::new(serde_json::json!({})),
                    constrained_sampling: None,
                })
            }
            fn label(&self) -> &str {
                "read"
            }
            async fn execute(
                &self,
                _id: &str,
                _params: serde_json::Value,
                _signal: CancellationToken,
                _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
            ) -> Result<AgentToolResult, AgentError> {
                Err(AgentError::Tool(
                    "not_found: path not found: /w/a.md (path: /w/a.md)".into(),
                ))
            }
        }

        let plain: Arc<dyn AgentTool> = Arc::new(Failing);
        let hinted = with_next_steps(plain.clone());
        let call = |tool: Arc<dyn AgentTool>| async move {
            tool.execute(
                "call-1",
                serde_json::json!({"path": "/w/a.md"}),
                CancellationToken::new(),
                Arc::new(|_p| ()),
            )
            .await
            .expect_err("always fails")
            .to_string()
        };
        let original = call(plain).await;
        let annotated = call(hinted).await;
        assert_eq!(
            annotated,
            format!("{original}\nUse ls or find to check the path."),
            "{annotated}"
        );
        // The model-facing text starts with the untouched original message.
        assert!(annotated.starts_with("tool error: not_found: path not found"));
    }
}
