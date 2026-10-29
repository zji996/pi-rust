//! Streaming state machine for the Google Generative AI protocol: turns
//! `streamGenerateContent` chunks into `AssistantMessageEvent`s.
//!
//! Mirrors the per-chunk loop of `api/google-generative-ai.ts::stream`:
//! `thought: true` parts become thinking blocks, everything else with `text`
//! becomes a text block, `functionCall` parts become tool calls, and the block
//! that is currently open is closed (`TextEnd` / `ThinkingEnd`) before a new
//! block kind opens. Thought signatures are retained on the block they arrived
//! on (see `google-shared.ts::retainThoughtSignature`) so the next request can
//! replay them verbatim.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::event_stream::AssistantMessageEventStreamProducer;
use crate::model::Model;
use crate::providers::anthropic::cost::calculate_cost;
use crate::providers::{DELTA_FLUSH_BYTES, DELTA_FLUSH_INTERVAL};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Content, DoneReason, ErrorReason, StopReason,
    TextContent, ThinkingContent, ToolCall, ToolCallType, Usage,
};

/// Monotonic suffix for generated tool-call ids, mirroring the module-level
/// `toolCallCounter` in the TS provider. Unique within a process; combined with
/// a millisecond timestamp and the tool name.
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone, Copy)]
struct OpenBlock {
    index: usize,
    kind: BlockKind,
}

/// Incremental stream state. One text or thinking block is open at a time,
/// exactly like the TS `currentBlock` variable.
pub(crate) struct GoogleStreamState {
    open: Option<OpenBlock>,
    /// Deltas accumulated but not yet emitted. Every event carries a full
    /// `partial` snapshot, so one event per wire chunk is quadratic in the
    /// message length — see [`DELTA_FLUSH_BYTES`].
    pending: String,
    last_flush: Instant,
}

impl Default for GoogleStreamState {
    fn default() -> Self {
        Self {
            open: None,
            pending: String::new(),
            last_flush: Instant::now(),
        }
    }
}

impl GoogleStreamState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn should_flush(&self) -> bool {
        self.pending.len() >= DELTA_FLUSH_BYTES || self.last_flush.elapsed() >= DELTA_FLUSH_INTERVAL
    }

    /// Emit whatever is batched. Called before every `*End` event and when the
    /// open block switches channel, so no delta is dropped or reordered.
    async fn flush(&mut self, out: &AssistantMessage, p: &mut AssistantMessageEventStreamProducer) {
        if self.pending.is_empty() {
            return;
        }
        let Some(open) = self.open else {
            self.pending.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending);
        self.last_flush = Instant::now();
        let partial = Arc::new(out.clone());
        let event = match open.kind {
            BlockKind::Text => AssistantMessageEvent::TextDelta {
                content_index: open.index,
                delta,
                partial,
            },
            BlockKind::Thinking => AssistantMessageEvent::ThinkingDelta {
                content_index: open.index,
                delta,
                partial,
            },
        };
        p.push(event).await;
    }

    /// Close the open block, emitting its batched tail and `*End` event.
    async fn close_open(
        &mut self,
        out: &AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        let Some(open) = self.open else {
            return;
        };
        self.flush(out, p).await;
        self.open = None;
        let partial = Arc::new(out.clone());
        let (content, event) = match open.kind {
            BlockKind::Text => {
                let text = match out.content.get(open.index) {
                    Some(Content::Text(block)) => block.text.clone(),
                    _ => String::new(),
                };
                (
                    text.clone(),
                    AssistantMessageEvent::TextEnd {
                        content_index: open.index,
                        content: text,
                        partial,
                    },
                )
            }
            BlockKind::Thinking => {
                let thinking = match out.content.get(open.index) {
                    Some(Content::Thinking(block)) => block.thinking.clone(),
                    _ => String::new(),
                };
                (
                    thinking.clone(),
                    AssistantMessageEvent::ThinkingEnd {
                        content_index: open.index,
                        content: thinking,
                        partial,
                    },
                )
            }
        };
        let _ = content;
        p.push(event).await;
    }

    /// Open a block of `kind` (closing the previous one when the channel
    /// switches) and emit its `*Start` event.
    async fn ensure_open(
        &mut self,
        kind: BlockKind,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        if matches!(self.open, Some(open) if open.kind == kind) {
            return;
        }
        self.close_open(out, p).await;
        let index = out.content.len();
        match kind {
            BlockKind::Text => out.content.push(Content::Text(TextContent {
                kind: Default::default(),
                text: String::new(),
                text_signature: None,
            })),
            BlockKind::Thinking => out.content.push(Content::Thinking(ThinkingContent {
                kind: Default::default(),
                thinking: String::new(),
                thinking_signature: None,
                redacted: false,
            })),
        }
        self.open = Some(OpenBlock { index, kind });
        self.last_flush = Instant::now();
        let partial = Arc::new(out.clone());
        p.push(match kind {
            BlockKind::Text => AssistantMessageEvent::TextStart {
                content_index: index,
                partial,
            },
            BlockKind::Thinking => AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial,
            },
        })
        .await;
    }

    /// Append one wire part's text (and its signature, when it carries one).
    ///
    /// Some backends only send `thoughtSignature` on the first delta of a part;
    /// later deltas may omit it. Keep the last non-empty signature for the
    /// current block and never move a signature between blocks.
    async fn append(
        &mut self,
        kind: BlockKind,
        text: &str,
        signature: Option<&str>,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        self.ensure_open(kind, out, p).await;
        let Some(open) = self.open else {
            return;
        };
        match (kind, out.content.get_mut(open.index)) {
            (BlockKind::Text, Some(Content::Text(block))) => {
                block.text.push_str(text);
                block.text_signature =
                    retain_thought_signature(block.text_signature.take(), signature);
            }
            (BlockKind::Thinking, Some(Content::Thinking(block))) => {
                block.thinking.push_str(text);
                block.thinking_signature =
                    retain_thought_signature(block.thinking_signature.take(), signature);
            }
            _ => {}
        }
        self.pending.push_str(text);
        if self.should_flush() {
            self.flush(out, p).await;
        }
    }

    /// Apply one decoded `GenerateContentResponse` chunk. Mirrors one iteration
    /// of the TS `for await (const chunk of googleStream)` loop.
    pub(crate) async fn apply_chunk(
        &mut self,
        out: &mut AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
        model: &Model,
        chunk: &Value,
    ) {
        // `responseId` is an output-only field identifying one response; keep
        // the first non-empty one.
        if out.response_id.is_none() {
            if let Some(id) = chunk
                .get("responseId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                out.response_id = Some(id.to_string());
            }
        }

        let candidate = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first());

        if let Some(parts) = candidate
            .and_then(|candidate| candidate.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    // `thought: true` is the definitive thinking marker.
                    // `thoughtSignature` can appear on any part type and does
                    // NOT indicate the part itself is thinking content.
                    let kind = if part.get("thought") == Some(&Value::Bool(true)) {
                        BlockKind::Thinking
                    } else {
                        BlockKind::Text
                    };
                    let signature = part.get("thoughtSignature").and_then(Value::as_str);
                    self.append(kind, text, signature, out, p).await;
                }

                if let Some(function_call) = part.get("functionCall") {
                    self.close_open(out, p).await;
                    let name = function_call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let call_id = tool_call_id(function_call, &name, out);
                    let arguments = function_call
                        .get("args")
                        .filter(|args| !args.is_null())
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let tool_call = ToolCall {
                        kind: ToolCallType,
                        id: call_id,
                        name,
                        arguments,
                        thought_signature: part
                            .get("thoughtSignature")
                            .and_then(Value::as_str)
                            .filter(|signature| !signature.is_empty())
                            .map(str::to_string),
                        namespace: None,
                    };
                    let index = out.content.len();
                    out.content.push(Content::ToolCall(tool_call.clone()));
                    p.push(AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        partial: Arc::new(out.clone()),
                    })
                    .await;
                    p.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta: serde_json::to_string(&tool_call.arguments)
                            .unwrap_or_else(|_| "{}".into()),
                        partial: Arc::new(out.clone()),
                    })
                    .await;
                    p.push(AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call,
                        partial: Arc::new(out.clone()),
                    })
                    .await;
                }
            }
        }

        if let Some(reason) = candidate
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(Value::as_str)
        {
            out.raw_stop_reason = Some(reason.to_string());
            out.stop_reason = map_stop_reason(reason);
            if out.stop_reason == StopReason::Stop
                && out
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::ToolCall(_)))
            {
                out.stop_reason = StopReason::ToolUse;
            }
        }

        if let Some(metadata) = chunk.get("usageMetadata") {
            out.usage = parse_usage(metadata);
            out.usage.cost = calculate_cost(&model.cost, &out.usage);
        }
    }

    /// Flush the last open block. Called once the body ends, before the terminal
    /// event is chosen.
    pub(crate) async fn close(
        &mut self,
        out: &AssistantMessage,
        p: &mut AssistantMessageEventStreamProducer,
    ) {
        self.close_open(out, p).await;
    }
}

/// Signature retention within one streamed block. Mirrors
/// `retainThoughtSignature`.
fn retain_thought_signature(existing: Option<String>, incoming: Option<&str>) -> Option<String> {
    match incoming {
        Some(signature) if !signature.is_empty() => Some(signature.to_string()),
        _ => existing,
    }
}

/// Tool-call id: Gemini omits it for most models and can repeat one when it
/// does send it. Mirrors the TS generation rule exactly, including the
/// process-wide counter, and guarantees uniqueness within the turn.
fn tool_call_id(function_call: &Value, name: &str, out: &AssistantMessage) -> String {
    let provided = function_call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let duplicate = provided.is_some_and(|id| {
        out.content
            .iter()
            .any(|content| matches!(content, Content::ToolCall(call) if call.id == id))
    });
    match provided {
        Some(id) if !duplicate => id.to_string(),
        _ => {
            let counter = TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            format!("{name}_{}_{}", now_ms(), counter)
        }
    }
}

/// Map a Gemini `FinishReason` string to the provider-neutral stop reason.
/// Mirrors `mapStopReason`: `STOP` → stop, `MAX_TOKENS` → length, everything
/// else (safety blocks, recitations, malformed calls, …) → error.
pub(crate) fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

/// `usageMetadata` → [`Usage`]. Mirrors the TS mapping: cached prompt tokens are
/// reported separately and subtracted from `input`; `thoughtsTokenCount` is part
/// of the billed output and is also surfaced as `reasoning`.
pub(crate) fn parse_usage(metadata: &Value) -> Usage {
    let count = |key: &str| metadata.get(key).and_then(Value::as_i64).unwrap_or(0);
    let prompt = count("promptTokenCount");
    let cached = count("cachedContentTokenCount");
    let candidates = count("candidatesTokenCount");
    let thoughts = count("thoughtsTokenCount");
    Usage {
        input: prompt - cached,
        output: candidates + thoughts,
        cache_read: cached,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: Some(thoughts),
        total_tokens: count("totalTokenCount"),
        cost: Default::default(),
    }
}

/// The terminal event for a stream that finished with a usable stop reason.
pub(crate) async fn emit_done(p: &mut AssistantMessageEventStreamProducer, out: AssistantMessage) {
    let reason = match out.stop_reason {
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Deferred => DoneReason::Deferred,
        _ => DoneReason::Stop,
    };
    p.push(AssistantMessageEvent::Done {
        reason,
        message: out,
    })
    .await;
}

/// The terminal event for any failure (transport, protocol, safety block).
pub(crate) async fn emit_error(
    p: &mut AssistantMessageEventStreamProducer,
    out: &mut AssistantMessage,
    message: impl Into<String>,
    aborted: bool,
) {
    out.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    out.error_message = Some(message.into());
    p.push(AssistantMessageEvent::Error {
        reason: if aborted {
            ErrorReason::Aborted
        } else {
            ErrorReason::Error
        },
        error: out.clone(),
    })
    .await;
}

pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::create_assistant_message_event_stream;
    use crate::types::Api;

    fn model() -> Model {
        Model::new(
            "gemini-3-pro-preview",
            "Gemini 3 Pro",
            Api::GoogleGenerativeAi,
            "google",
            "https://example.test/v1beta",
        )
    }

    fn empty_out(model: &Model) -> AssistantMessage {
        AssistantMessage::empty(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            0,
        )
    }

    async fn drain(
        stream: &mut crate::event_stream::AssistantMessageEventStream,
    ) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[test]
    fn usage_maps_cached_tokens_out_of_input_and_thoughts_into_output() {
        let usage = parse_usage(&json!({
            "promptTokenCount": 1000,
            "cachedContentTokenCount": 400,
            "candidatesTokenCount": 50,
            "thoughtsTokenCount": 25,
            "totalTokenCount": 1075
        }));
        assert_eq!(usage.input, 600);
        assert_eq!(usage.output, 75);
        assert_eq!(usage.cache_read, 400);
        assert_eq!(usage.cache_write, 0);
        assert_eq!(usage.reasoning, Some(25));
        assert_eq!(usage.total_tokens, 1075);
    }

    #[test]
    fn stop_reasons_follow_the_reference_table() {
        assert_eq!(map_stop_reason("STOP"), StopReason::Stop);
        assert_eq!(map_stop_reason("MAX_TOKENS"), StopReason::Length);
        for reason in [
            "SAFETY",
            "RECITATION",
            "MALFORMED_FUNCTION_CALL",
            "PROHIBITED_CONTENT",
            "SPII",
            "OTHER",
        ] {
            assert_eq!(map_stop_reason(reason), StopReason::Error, "{reason}");
        }
    }

    #[test]
    fn signature_retention_keeps_the_last_non_empty_value() {
        assert_eq!(
            retain_thought_signature(None, Some("abc")),
            Some("abc".into())
        );
        assert_eq!(
            retain_thought_signature(Some("abc".into()), None),
            Some("abc".into())
        );
        assert_eq!(
            retain_thought_signature(Some("abc".into()), Some("")),
            Some("abc".into())
        );
        assert_eq!(
            retain_thought_signature(Some("abc".into()), Some("def")),
            Some("def".into())
        );
    }

    #[tokio::test]
    async fn streaming_text_opens_one_block_and_ends_it() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut out = empty_out(&model);
        let mut state = GoogleStreamState::new();
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(out.clone()),
            })
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({ "candidates": [{ "content": { "parts": [{ "text": "hello " }] } }] }),
            )
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({
                    "candidates": [{
                        "content": { "parts": [{ "text": "world" }] },
                        "finishReason": "STOP"
                    }]
                }),
            )
            .await;
        state.close(&out, &mut producer).await;
        emit_done(&mut producer, out).await;
        let events = drain(&mut stream).await;
        let kinds: Vec<&str> = events.iter().map(|event| event.type_tag()).collect();
        assert_eq!(
            kinds,
            vec!["start", "text_start", "text_delta", "text_end", "done"]
        );
        // Both wire deltas were batched into one emitted delta, but nothing is lost.
        let text = match &events[3] {
            AssistantMessageEvent::TextEnd { content, .. } => content.clone(),
            other => panic!("unexpected event {other:?}"),
        };
        assert_eq!(text, "hello world");
        match events.last().unwrap() {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::Stop);
                assert_eq!(message.stop_reason, StopReason::Stop);
            }
            other => panic!("unexpected terminal event {other:?}"),
        }
    }

    #[tokio::test]
    async fn thinking_then_text_switches_channel_and_keeps_the_signature() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut out = empty_out(&model);
        let mut state = GoogleStreamState::new();
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(out.clone()),
            })
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({ "candidates": [{ "content": { "parts": [
                    { "text": "reasoning", "thought": true, "thoughtSignature": "c2ln" }
                ] } }] }),
            )
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({ "candidates": [{
                    "content": { "parts": [{ "text": "answer", "thoughtSignature": "c2ln" }] },
                    "finishReason": "STOP"
                }] }),
            )
            .await;
        state.close(&out, &mut producer).await;
        emit_done(&mut producer, out).await;
        let events = drain(&mut stream).await;
        let kinds: Vec<&str> = events.iter().map(|event| event.type_tag()).collect();
        assert_eq!(
            kinds,
            vec![
                "start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_end",
                "done"
            ]
        );
        match events.last().unwrap() {
            AssistantMessageEvent::Done { message, .. } => {
                assert_eq!(message.content.len(), 2);
                match &message.content[0] {
                    Content::Thinking(block) => {
                        assert_eq!(block.thinking, "reasoning");
                        assert_eq!(block.thinking_signature.as_deref(), Some("c2ln"));
                    }
                    other => panic!("unexpected block {other:?}"),
                }
                match &message.content[1] {
                    Content::Text(block) => {
                        assert_eq!(block.text, "answer");
                        assert_eq!(block.text_signature.as_deref(), Some("c2ln"));
                    }
                    other => panic!("unexpected block {other:?}"),
                }
            }
            other => panic!("unexpected terminal event {other:?}"),
        }
    }

    #[tokio::test]
    async fn parallel_function_calls_get_distinct_ids_and_arguments() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut out = empty_out(&model);
        let mut state = GoogleStreamState::new();
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(out.clone()),
            })
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({ "candidates": [{ "content": { "parts": [
                    { "functionCall": { "name": "read", "args": { "path": "a" } } },
                    { "functionCall": { "name": "read", "args": { "path": "b" } } }
                ] }, "finishReason": "STOP" }] }),
            )
            .await;
        state.close(&out, &mut producer).await;
        emit_done(&mut producer, out).await;
        let events = drain(&mut stream).await;
        let message = match events.last().unwrap() {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::ToolUse);
                message
            }
            other => panic!("unexpected terminal event {other:?}"),
        };
        let calls: Vec<&ToolCall> = message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].id, calls[1].id);
        assert_eq!(calls[0].arguments, json!({ "path": "a" }));
        assert_eq!(calls[1].arguments, json!({ "path": "b" }));
        assert_eq!(message.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn provided_ids_are_kept_unless_they_repeat() {
        let model = model();
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut out = empty_out(&model);
        let mut state = GoogleStreamState::new();
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(out.clone()),
            })
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({ "candidates": [{ "content": { "parts": [
                    { "functionCall": { "id": "call-1", "name": "read", "args": {} } },
                    { "functionCall": { "id": "call-1", "name": "read", "args": {} } }
                ] }, "finishReason": "STOP" }] }),
            )
            .await;
        let calls: Vec<&ToolCall> = out
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls[0].id, "call-1");
        assert_ne!(calls[1].id, "call-1");
        assert!(calls[1].id.starts_with("read_"));
        state.close(&out, &mut producer).await;
    }

    #[tokio::test]
    async fn function_call_carries_its_thought_signature_for_replay() {
        let model = model();
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut out = empty_out(&model);
        let mut state = GoogleStreamState::new();
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(out.clone()),
            })
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({ "candidates": [{ "content": { "parts": [
                    { "functionCall": { "name": "read", "args": {} }, "thoughtSignature": "c2ln" }
                ] }, "finishReason": "STOP" }] }),
            )
            .await;
        match &out.content[0] {
            Content::ToolCall(call) => {
                assert_eq!(call.thought_signature.as_deref(), Some("c2ln"))
            }
            other => panic!("unexpected block {other:?}"),
        }
        state.close(&out, &mut producer).await;
    }

    #[tokio::test]
    async fn max_tokens_maps_to_length_and_usage_lands_on_the_message() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut out = empty_out(&model);
        let mut state = GoogleStreamState::new();
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(out.clone()),
            })
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({
                    "responseId": "resp-1",
                    "candidates": [{
                        "content": { "parts": [{ "text": "partial" }] },
                        "finishReason": "MAX_TOKENS"
                    }],
                    "usageMetadata": {
                        "promptTokenCount": 10,
                        "candidatesTokenCount": 3,
                        "thoughtsTokenCount": 2,
                        "totalTokenCount": 15
                    }
                }),
            )
            .await;
        state.close(&out, &mut producer).await;
        emit_done(&mut producer, out).await;
        let events = drain(&mut stream).await;
        match events.last().unwrap() {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::Length);
                assert_eq!(message.stop_reason, StopReason::Length);
                assert_eq!(message.raw_stop_reason.as_deref(), Some("MAX_TOKENS"));
                assert_eq!(message.response_id.as_deref(), Some("resp-1"));
                assert_eq!(message.usage.input, 10);
                assert_eq!(message.usage.output, 5);
                assert_eq!(message.usage.total_tokens, 15);
            }
            other => panic!("unexpected terminal event {other:?}"),
        }
    }

    #[tokio::test]
    async fn safety_block_is_an_error_stop() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut out = empty_out(&model);
        let mut state = GoogleStreamState::new();
        producer
            .push(AssistantMessageEvent::Start {
                partial: Arc::new(out.clone()),
            })
            .await;
        state
            .apply_chunk(
                &mut out,
                &mut producer,
                &model,
                &json!({ "candidates": [{ "finishReason": "SAFETY" }] }),
            )
            .await;
        assert_eq!(out.stop_reason, StopReason::Error);
        state.close(&out, &mut producer).await;
        emit_error(
            &mut producer,
            &mut out,
            "Provider stopped with: SAFETY",
            false,
        )
        .await;
        let events = drain(&mut stream).await;
        assert!(matches!(
            events.last().unwrap(),
            AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                ..
            }
        ));
    }
}
