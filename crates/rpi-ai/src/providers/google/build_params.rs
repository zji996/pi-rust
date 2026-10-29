//! Request shaping for the Google Generative AI (`streamGenerateContent`) wire
//! protocol. Mirrors `api/google-shared.ts::convertMessages` / `convertTools` /
//! the thinking helpers plus `api/google-generative-ai.ts::buildParams`.
//!
//! Only the `google-generative-ai` API is implemented here; Vertex AI shares the
//! wire shape upstream but has its own auth/addressing and is deliberately out
//! of scope.
//!
//! Everything in this file is pure: it takes the provider-neutral `Model` +
//! `Context` + `SimpleStreamOptions` and returns the JSON body Gemini expects,
//! so the request shape is unit-testable without a network.

use serde_json::{json, Map, Value};

use crate::model::Model;
use crate::provider::SimpleStreamOptions;
use crate::providers::anthropic::build_params::{
    clamp_max_tokens_to_context, transform_messages_with,
};
use crate::strict_schema::strict_tool_parameters;
use crate::types::{
    ConstrainedSamplingConfig, Content, Context, InputModality, Message, ThinkingBudgets,
    ThinkingLevel, Tool, UserContent,
};

// ----------------------------------------------------------------------------
// Model-id predicates — google-shared.ts
// ----------------------------------------------------------------------------

/// Major Gemini version parsed from a model id (`gemini-2.5-pro` → `2`,
/// `gemini-live-3-flash` → `3`). Mirrors `getGeminiMajorVersion`.
pub(crate) fn gemini_major_version(model_id: &str) -> Option<u32> {
    let id = model_id.to_ascii_lowercase();
    let rest = id.strip_prefix("gemini")?;
    let rest = rest.strip_prefix("-live").unwrap_or(rest);
    let rest = rest.strip_prefix('-')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Whether this model uses Gemini's discrete `thinkingLevel` control instead of
/// the token-based `thinkingBudget` control. Mirrors `usesGoogleThinkingLevel`.
pub(crate) fn uses_google_thinking_level(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    // Gemini 3 Pro/Flash with or without a minor version
    // (gemini-3-flash-preview, gemini-3.1-pro-preview, gemini-3.8-flash).
    gemini_3_pro_or_flash(&id)
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        // Hosted Gemma 4 naming: gemma-4-* and gemma4-*.
        || gemma_4(&id)
}

/// `/gemini-3(?:\.\d+)?-(?:pro|flash)/`
fn gemini_3_pro_or_flash(id: &str) -> bool {
    let Some(rest) = id.split_once("gemini-3").map(|(_, rest)| rest) else {
        return false;
    };
    // Optional minor version `.N`, then `-pro` or `-flash`.
    let rest = match rest.strip_prefix('.') {
        Some(after_dot) => match after_dot.find(|c: char| !c.is_ascii_digit()) {
            Some(index) if index > 0 => &after_dot[index..],
            _ => return false,
        },
        None => rest,
    };
    rest.starts_with("-pro") || rest.starts_with("-flash")
}

/// `/gemma-?4/`
fn gemma_4(id: &str) -> bool {
    fn digit_four_after(id: &str, start: usize) -> bool {
        id.get(start..).is_some_and(|rest| rest.starts_with('4'))
    }
    let mut search = 0;
    while let Some(found) = id[search..].find("gemma") {
        let after = search + found + "gemma".len();
        // `gemma-4` or `gemma4`
        if digit_four_after(id, after)
            || (id.get(after..).is_some_and(|rest| rest.starts_with('-'))
                && digit_four_after(id, after + 1))
        {
            return true;
        }
        search = after;
    }
    false
}

/// Models behind the Google APIs that require explicit tool-call ids in
/// function calls/responses. Mirrors `requiresToolCallId`.
pub(crate) fn requires_tool_call_id(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.starts_with("claude-")
        || id.starts_with("gpt-oss-")
        || gemini_major_version(&id).is_some_and(|major| major >= 3)
}

/// Gemini 3+ accepts images nested inside `functionResponse.parts`; older Gemini
/// (and non-Gemini hosts) need a separate user turn. Mirrors
/// `supportsMultimodalFunctionResponse`.
fn supports_multimodal_function_response(model_id: &str) -> bool {
    match gemini_major_version(model_id) {
        Some(major) => major >= 3,
        None => true,
    }
}

/// Normalize a tool-call id the way Google expects when the model requires
/// explicit ids. Mirrors the `normalizeToolCallId` closure in `convertMessages`:
/// every char outside `[a-zA-Z0-9_-]` becomes `_`, truncated to 64 chars.
fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

// ----------------------------------------------------------------------------
// Thinking — google-shared.ts thinking helpers + google-generative-ai.ts
// ----------------------------------------------------------------------------

/// A pi thinking level mapped through the model's `thinkingLevelMap` and proven
/// to be one of Google's discrete levels. Mirrors `ResolvedGoogleThinkingLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ResolvedThinkingLevel {
    fn as_wire(self) -> &'static str {
        match self {
            Self::Minimal => "MINIMAL",
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
        }
    }
}

/// Resolve a supported pi level or the model's own mapping to a Google level.
/// Mirrors `resolveGoogleThinkingLevel`, including the hard failure when the
/// model maps a level to something Google does not understand.
pub(crate) fn resolve_thinking_level(
    model: &Model,
    level: ThinkingLevel,
) -> Result<ResolvedThinkingLevel, String> {
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level))
        .and_then(|mapped| mapped.clone());
    let resolved = match mapped {
        Some(value) => value.to_ascii_lowercase(),
        None => thinking_level_key(level).to_string(),
    };
    match resolved.as_str() {
        "minimal" => Ok(ResolvedThinkingLevel::Minimal),
        "low" => Ok(ResolvedThinkingLevel::Low),
        "medium" => Ok(ResolvedThinkingLevel::Medium),
        "high" => Ok(ResolvedThinkingLevel::High),
        other => Err(format!(
            "Unsupported Google thinking level mapping for {}/{}: {} -> {other}",
            model.provider,
            model.id,
            thinking_level_key(level)
        )),
    }
}

/// The pi-level name used in diagnostics and as the wire/`thinkingLevelMap`
/// key. `ThinkingLevel` itself carries no `as_str`.
pub(crate) fn thinking_level_key(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

/// `thinkingConfig` for the disabled case. Mirrors
/// `getDisabledGoogleThinkingConfig`: token-budgeted models get
/// `thinkingBudget: 0`; level-based models fall back to the clamped `off`
/// mapping (or `thinkingBudget: 0` when `off` clamps to off).
fn disabled_thinking_config(model: &Model) -> Result<Value, String> {
    if !uses_google_thinking_level(model) {
        return Ok(json!({ "thinkingBudget": 0 }));
    }
    let fallback = model.clamp_thinking_level(ThinkingLevel::Off);
    if fallback == ThinkingLevel::Off {
        return Ok(json!({ "thinkingBudget": 0 }));
    }
    let resolved = resolve_thinking_level(model, fallback)?;
    Ok(json!({ "thinkingLevel": resolved.as_wire() }))
}

/// Per-level token budgets for budget-based thinking models. Mirrors
/// `getGoogleBudget`: caller budgets win, then the per-family defaults, then
/// `-1` ("dynamic").
fn google_budget(
    model: &Model,
    level: ResolvedThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> i64 {
    let custom = custom_budgets.and_then(|budgets| match level {
        ResolvedThinkingLevel::Minimal => budgets.minimal,
        ResolvedThinkingLevel::Low => budgets.low,
        ResolvedThinkingLevel::Medium => budgets.medium,
        ResolvedThinkingLevel::High => budgets.high,
    });
    if let Some(value) = custom {
        return value as i64;
    }
    let id = model.id.as_str();
    let budgets: Option<[i64; 4]> = if id.contains("2.5-pro") {
        Some([128, 2048, 8192, 32768])
    } else if id.contains("2.5-flash-lite") {
        Some([512, 2048, 8192, 24576])
    } else if id.contains("2.5-flash") {
        Some([128, 2048, 8192, 24576])
    } else {
        None
    };
    let Some(budgets) = budgets else {
        return -1;
    };
    match level {
        ResolvedThinkingLevel::Minimal => budgets[0],
        ResolvedThinkingLevel::Low => budgets[1],
        ResolvedThinkingLevel::Medium => budgets[2],
        ResolvedThinkingLevel::High => budgets[3],
    }
}

/// `thinkingConfig` for an enabled thinking level, or `None` when the request
/// should carry no `thinkingConfig` at all.
///
/// `SimpleStreamOptions::reasoning` folds upstream's `thinking: {enabled, level|
/// budgetTokens}` object: `None`/`Off` is the disabled case (upstream
/// `streamSimple` always passes a disabled object when reasoning is unset, so a
/// reasoning model always gets an explicit "off" on the wire), anything else is
/// the enabled case.
pub(crate) fn thinking_config(
    model: &Model,
    opts: &SimpleStreamOptions,
) -> Result<Option<Value>, String> {
    if !model.reasoning {
        return Ok(None);
    }
    let requested = opts.reasoning_level();
    let clamped = model.clamp_thinking_level(requested);
    if clamped == ThinkingLevel::Off {
        return disabled_thinking_config(model).map(Some);
    }
    let resolved = resolve_thinking_level(model, clamped)?;
    if uses_google_thinking_level(model) {
        return Ok(Some(json!({
            "includeThoughts": true,
            "thinkingLevel": resolved.as_wire(),
        })));
    }
    Ok(Some(json!({
        "includeThoughts": true,
        "thinkingBudget": google_budget(model, resolved, opts.thinking_budgets.as_ref()),
    })))
}

// ----------------------------------------------------------------------------
// Tool declarations
// ----------------------------------------------------------------------------

/// Gemini 3+ enforces required function parameters in validated tool-calling
/// modes. Mirrors `supportsGoogleStrictToolSampling`.
pub(crate) fn supports_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

/// Whether this tool opts into strict JSON-schema sampling on this model.
/// Mirrors `resolveJsonSchemaStrictSampling(tool, supportsStrictMode) === true`:
/// only the strict subset of a `json_schema` constrained-sampling tool counts.
fn tool_is_strict(tool: &Tool, supports_strict_mode: bool) -> bool {
    if !supports_strict_mode {
        return false;
    }
    let Some(ConstrainedSamplingConfig::JsonSchema { .. }) = tool.constrained_sampling.as_ref()
    else {
        return false;
    };
    strict_tool_parameters(tool).is_some()
}

/// Function declarations for `tools`. Mirrors `convertTools(tools, false, …)`:
/// `parametersJsonSchema` (full JSON Schema) rather than the legacy OpenAPI
/// `parameters` field, strict-transformed only when the tool opted in.
pub(crate) fn convert_tools(tools: &[Tool], supports_strict_mode: bool) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let parameters = if tool_is_strict(tool, supports_strict_mode) {
                strict_tool_parameters(tool).unwrap_or_else(|| tool.parameters.as_value().clone())
            } else {
                tool.parameters.as_value().clone()
            };
            json!({
                "name": tool.name,
                "description": tool.description,
                "parametersJsonSchema": parameters,
            })
        })
        .collect();
    Some(json!([{ "functionDeclarations": declarations }]))
}

/// `toolConfig` for the request. Mirrors `resolveGoogleFunctionCallingMode`
/// with no `toolChoice`: only the strict-mode `VALIDATED` mode is ever implied
/// (pi exposes no tool-choice knob on `SimpleStreamOptions`).
pub(crate) fn tool_config(tools: &[Tool], supports_strict_mode: bool) -> Option<Value> {
    if !tools
        .iter()
        .any(|tool| tool_is_strict(tool, supports_strict_mode))
    {
        return None;
    }
    Some(json!({ "functionCallingConfig": { "mode": "VALIDATED" } }))
}

// ----------------------------------------------------------------------------
// Thought signatures — google-shared.ts
// ----------------------------------------------------------------------------

/// Thought signatures must be base64 for the Google APIs (TYPE_BYTES).
/// Mirrors `isValidThoughtSignature`.
fn is_valid_thought_signature(signature: &str) -> bool {
    if signature.is_empty() || !signature.len().is_multiple_of(4) {
        return false;
    }
    let bytes = signature.as_bytes();
    let padding = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if padding > 2 {
        return false;
    }
    let body = &signature[..signature.len() - padding];
    !body.is_empty() && body.bytes().all(is_base64_body_byte)
}

fn is_base64_body_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'
}

/// Only keep signatures from the same provider/model and with valid base64.
/// Mirrors `resolveThoughtSignature`.
pub(crate) fn resolve_thought_signature(
    is_same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    let signature = signature?;
    (is_same_provider_and_model && is_valid_thought_signature(signature))
        .then(|| signature.to_string())
}

// ----------------------------------------------------------------------------
// convertMessages
// ----------------------------------------------------------------------------

fn image_part(data: &str, mime_type: &str) -> Value {
    json!({ "inlineData": { "mimeType": mime_type, "data": data } })
}

/// Convert internal messages to Gemini `Content[]`. Mirrors `convertMessages`.
///
/// The rust port has no in-band system messages (`Context::system_prompt` is
/// out-of-band), so `withoutInitialSystemMessage(collapseSystemMessages(...))`
/// is the identity and the prompt becomes `systemInstruction`.
pub(crate) fn convert_messages(model: &Model, ctx: &Context) -> Vec<Value> {
    let normalize: &dyn Fn(&str) -> String = if requires_tool_call_id(&model.id) {
        &normalize_tool_call_id
    } else {
        &|id: &str| id.to_string()
    };
    let messages = transform_messages_with(&ctx.messages, model, normalize);

    let supports_images = model.input.contains(&InputModality::Image);
    let include_ids = requires_tool_call_id(&model.id);
    let multimodal_function_response = supports_multimodal_function_response(&model.id);
    let mut contents: Vec<Value> = Vec::new();

    for message in &messages {
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    contents.push(json!({ "role": "user", "parts": [{ "text": text }] }));
                }
                UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            Content::Text(text) => Some(json!({ "text": text.text })),
                            Content::Image(image) => {
                                Some(image_part(&image.data, &image.mime_type))
                            }
                            _ => None,
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            },
            Message::Assistant(assistant) => {
                let same = assistant.provider == model.provider && assistant.model == model.id;
                let mut parts: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        Content::Text(text) => {
                            let signature =
                                resolve_thought_signature(same, text.text_signature.as_deref());
                            // Skip empty text blocks — unless they carry a thought
                            // signature. Gemini can attach the signature to a part
                            // whose visible text is empty and requires it echoed
                            // back; dropping it breaks the reasoning chain.
                            if text.text.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            parts.push(with_signature(json!({ "text": text.text }), signature));
                        }
                        Content::Thinking(thinking) => {
                            if same {
                                let signature = resolve_thought_signature(
                                    same,
                                    thinking.thinking_signature.as_deref(),
                                );
                                if thinking.thinking.trim().is_empty() && signature.is_none() {
                                    continue;
                                }
                                parts.push(with_signature(
                                    json!({ "thought": true, "text": thinking.thinking }),
                                    signature,
                                ));
                            } else {
                                // Cross-provider/model: the signature is unusable and
                                // empty blocks stay dropped. No `thought` flag so the
                                // model has no reason to mimic the channel.
                                if thinking.thinking.trim().is_empty() {
                                    continue;
                                }
                                parts.push(json!({ "text": thinking.thinking }));
                            }
                        }
                        Content::ToolCall(call) => {
                            let signature =
                                resolve_thought_signature(same, call.thought_signature.as_deref());
                            let arguments = if call.arguments.is_null() {
                                json!({})
                            } else {
                                call.arguments.clone()
                            };
                            let mut function_call = Map::new();
                            function_call.insert("name".into(), json!(call.name));
                            function_call.insert("args".into(), arguments);
                            if include_ids {
                                function_call.insert("id".into(), json!(call.id));
                            }
                            parts.push(with_signature(
                                json!({ "functionCall": Value::Object(function_call) }),
                                signature,
                            ));
                        }
                        Content::Image(_) => {
                            // Assistant-side images are not emitted by the protocol.
                        }
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            Message::ToolResult(result) => {
                let text_result = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Content::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images: Vec<&crate::types::ImageContent> = if supports_images {
                    result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            Content::Image(image) => Some(image),
                            _ => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let has_text = !text_result.is_empty();
                let has_images = !images.is_empty();
                let response_value = if has_text {
                    text_result
                } else if has_images {
                    "(see attached image)".to_string()
                } else {
                    String::new()
                };
                let image_parts: Vec<Value> = images
                    .iter()
                    .map(|image| image_part(&image.data, &image.mime_type))
                    .collect();
                let mut function_response = Map::new();
                function_response.insert("name".into(), json!(result.tool_name));
                // "output" key for success, "error" key for errors (SDK docs).
                function_response.insert(
                    "response".into(),
                    if result.is_error {
                        json!({ "error": response_value })
                    } else {
                        json!({ "output": response_value })
                    },
                );
                if has_images && multimodal_function_response {
                    function_response.insert("parts".into(), json!(image_parts));
                }
                if include_ids {
                    function_response.insert("id".into(), json!(result.tool_call_id));
                }
                let part = json!({ "functionResponse": Value::Object(function_response) });

                // All function responses for one turn must land in a single user
                // turn (Cloud Code Assist requirement), so merge with a trailing
                // user turn that already carries one.
                let merged = contents
                    .last_mut()
                    .filter(|last| {
                        last.get("role").and_then(Value::as_str) == Some("user")
                            && last
                                .get("parts")
                                .and_then(Value::as_array)
                                .is_some_and(|parts| {
                                    parts
                                        .iter()
                                        .any(|part| part.get("functionResponse").is_some())
                                })
                    })
                    .and_then(|last| last.get_mut("parts"))
                    .and_then(Value::as_array_mut);
                if let Some(parts) = merged {
                    parts.push(part);
                } else {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }

                // Gemini < 3 needs tool-result images in a separate user turn.
                if has_images && !multimodal_function_response {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(image_parts);
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    contents
}

fn with_signature(part: Value, signature: Option<String>) -> Value {
    match (part, signature) {
        (Value::Object(mut map), Some(signature)) => {
            map.insert("thoughtSignature".into(), Value::String(signature));
            Value::Object(map)
        }
        (part, _) => part,
    }
}

// ----------------------------------------------------------------------------
// buildParams
// ----------------------------------------------------------------------------

/// `maxOutputTokens`. Upstream always sends it (`streamSimple` resolves
/// `options.maxTokens ?? model.maxTokens` and clamps it to the context window),
/// so the port does the same — except for a model that declares no ceiling at
/// all, where sending a single token would be worse than omitting the field and
/// letting the API pick its default.
fn output_token_limit(model: &Model, ctx: &Context, opts: &SimpleStreamOptions) -> Option<u64> {
    let requested = opts.max_tokens.unwrap_or(model.max_tokens);
    if requested == 0 {
        return None;
    }
    Some(clamp_max_tokens_to_context(
        model,
        &ctx.messages,
        ctx.system_prompt.as_deref(),
        requested,
    ))
}

/// Build the `streamGenerateContent` request body. Mirrors `buildParams`.
pub(crate) fn build_request_body(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> Result<Value, String> {
    let supports_strict_mode = supports_strict_tool_sampling(&model.id);

    let mut generation_config = Map::new();
    if let Some(temperature) = opts.temperature {
        generation_config.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = output_token_limit(model, ctx, opts) {
        generation_config.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    // The SDK folds `config.thinkingConfig` into `generationConfig`; the REST
    // API rejects it at the top level.
    if let Some(thinking_config) = thinking_config(model, opts)? {
        generation_config.insert("thinkingConfig".into(), thinking_config);
    }

    let mut body = Map::new();
    body.insert("contents".into(), json!(convert_messages(model, ctx)));
    if !generation_config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation_config));
    }
    if let Some(prompt) = ctx.system_prompt.as_ref().filter(|p| !p.is_empty()) {
        body.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": prompt }] }),
        );
    }
    if let Some(tools) = convert_tools(&ctx.tools, supports_strict_mode) {
        body.insert("tools".into(), tools);
    }
    if let Some(tool_config) = tool_config(&ctx.tools, supports_strict_mode) {
        body.insert("toolConfig".into(), tool_config);
    }
    Ok(Value::Object(body))
}

/// Gemini's `streamGenerateContent` endpoint. Mirrors the SDK URL assembly with
/// `apiVersion: ""`: the configured base URL already carries the version path
/// (`.../v1beta`).
pub(crate) fn stream_url(base_url: &str, model_id: &str) -> String {
    format!(
        "{}/models/{model_id}:streamGenerateContent?alt=sse",
        base_url.trim_end_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Api, AssistantMessage, ImageContent, Schema, TextContent, ToolCall, ToolResultMessage,
    };

    const SIGNATURE: &str = "c2lnbmF0dXJlLWZvci10ZXN0";

    fn model(id: &str) -> Model {
        Model::new(
            id,
            id,
            Api::GoogleGenerativeAi,
            "google",
            "https://example.test/v1beta",
        )
    }

    fn reasoning_model(id: &str) -> Model {
        let mut model = model(id);
        model.reasoning = true;
        model
    }

    fn tool(name: &str, parameters: Value) -> Tool {
        Tool {
            name: name.into(),
            description: "synthetic tool".into(),
            parameters: Schema::new(parameters),
            constrained_sampling: None,
        }
    }

    fn assistant(model: &Model, content: Vec<Content>) -> Message {
        let mut message = AssistantMessage::empty(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            0,
        );
        message.content = content;
        message.stop_reason = crate::types::StopReason::ToolUse;
        Message::Assistant(Box::new(message))
    }

    fn text_signature(signature: &str) -> Content {
        Content::Text(TextContent {
            kind: Default::default(),
            text: "done".into(),
            text_signature: Some(signature.into()),
        })
    }

    fn tool_call(id: &str, signature: Option<&str>) -> Content {
        Content::ToolCall(ToolCall {
            kind: Default::default(),
            id: id.into(),
            name: "screenshot".into(),
            arguments: json!({ "selector": "#main" }),
            thought_signature: signature.map(str::to_string),
            namespace: None,
        })
    }

    #[test]
    fn system_prompt_becomes_system_instruction() {
        let model = model("gemini-2.5-flash");
        let mut ctx = Context::new(vec![Message::User(crate::types::UserMessage::new("hi", 0))]);
        ctx.system_prompt = Some("be brief".into());
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        assert_eq!(
            body["systemInstruction"],
            json!({ "parts": [{ "text": "be brief" }] })
        );
        assert_eq!(body["contents"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["contents"][0],
            json!({ "role": "user", "parts": [{ "text": "hi" }] })
        );
    }

    #[test]
    fn multi_turn_and_tool_results_replay_into_model_and_user_turns() {
        let model = model("gemini-2.5-flash");
        let ctx = Context::new(vec![
            Message::User(crate::types::UserMessage::new("screenshot it", 0)),
            assistant(&model, vec![tool_call("call-1", None)]),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: Default::default(),
                tool_call_id: "call-1".into(),
                tool_name: "screenshot".into(),
                content: vec![Content::text("png bytes")],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 0,
            })),
        ]);
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        assert_eq!(
            body["contents"],
            json!([
                { "role": "user", "parts": [{ "text": "screenshot it" }] },
                { "role": "model", "parts": [{ "functionCall": {
                    "name": "screenshot", "args": { "selector": "#main" }
                }}] },
                { "role": "user", "parts": [{ "functionResponse": {
                    "name": "screenshot", "response": { "output": "png bytes" }
                }}] },
            ])
        );
    }

    #[test]
    fn tool_declarations_use_parameters_json_schema_and_validated_strict_mode() {
        let model = model("gemini-3-pro-preview");
        let mut ctx = Context::new(vec![]);
        ctx.tools = vec![
            tool(
                "read",
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            ),
            Tool {
                constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                    strict: crate::types::ConstrainedStrictness::Require,
                }),
                ..tool(
                    "edit",
                    json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"],
                        "additionalProperties": false
                    }),
                )
            },
        ];
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        let declarations = &body["tools"][0]["functionDeclarations"];
        assert_eq!(declarations[0]["name"], "read");
        assert_eq!(
            declarations[0]["parametersJsonSchema"],
            json!({ "type": "object", "properties": { "path": { "type": "string" } } })
        );
        // Strict tools are completed to the strict subset and flip the mode.
        assert_eq!(
            declarations[1]["parametersJsonSchema"]["required"],
            json!(["path"])
        );
        assert_eq!(
            declarations[1]["parametersJsonSchema"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
    }

    #[test]
    fn non_strict_models_never_claim_validated_tool_mode() {
        let model = model("gemini-2.5-flash");
        let mut ctx = Context::new(vec![]);
        ctx.tools = vec![Tool {
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: crate::types::ConstrainedStrictness::Require,
            }),
            ..tool(
                "edit",
                json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            )
        }];
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        assert!(body.get("toolConfig").is_none());
        assert!(body["tools"][0]["functionDeclarations"][0]
            .get("parametersJsonSchema")
            .is_some());
    }

    #[test]
    fn images_are_inline_data_parts() {
        let mut model = model("gemini-2.5-flash");
        model.input = vec![InputModality::Text, InputModality::Image];
        let image = Content::Image(ImageContent {
            kind: Default::default(),
            data: "QUJD".into(),
            mime_type: "image/png".into(),
        });
        let ctx = Context::new(vec![Message::User(crate::types::UserMessage::new(
            UserContent::Blocks(vec![Content::text("look"), image]),
            0,
        ))]);
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][1],
            json!({ "inlineData": { "mimeType": "image/png", "data": "QUJD" } })
        );
    }

    #[test]
    fn gemini3_tool_results_carry_ids_and_nested_images() {
        let mut model = model("gemini-3-flash-preview");
        model.input = vec![InputModality::Text, InputModality::Image];
        let ctx = Context::new(vec![
            Message::User(crate::types::UserMessage::new("shot", 0)),
            assistant(&model, vec![tool_call("call|1", None)]),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: Default::default(),
                tool_call_id: "call|1".into(),
                tool_name: "screenshot".into(),
                content: vec![
                    Content::text("shot"),
                    Content::Image(ImageContent {
                        kind: Default::default(),
                        data: "QUJD".into(),
                        mime_type: "image/png".into(),
                    }),
                ],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 0,
            })),
        ]);
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        // Same-model histories are replayed verbatim (the TS pipeline only
        // normalizes ids on cross-model replay), so the odd id survives.
        assert_eq!(
            body["contents"][1]["parts"][0]["functionCall"]["id"],
            "call|1"
        );
        assert_eq!(
            body["contents"][2]["parts"][0],
            json!({ "functionResponse": {
                "name": "screenshot",
                "response": { "output": "shot" },
                "parts": [{ "inlineData": { "mimeType": "image/png", "data": "QUJD" } }],
                "id": "call|1"
            }})
        );
    }

    #[test]
    fn gemini2_tool_result_images_use_a_separate_user_turn() {
        let mut model = model("gemini-2.5-flash");
        model.input = vec![InputModality::Text, InputModality::Image];
        let ctx = Context::new(vec![Message::ToolResult(Box::new(ToolResultMessage {
            role: Default::default(),
            tool_call_id: "call-1".into(),
            tool_name: "screenshot".into(),
            content: vec![Content::Image(ImageContent {
                kind: Default::default(),
                data: "QUJD".into(),
                mime_type: "image/png".into(),
            })],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: true,
            timestamp: 0,
        }))]);
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(
            contents[0]["parts"][0]["functionResponse"]["response"],
            json!({ "error": "(see attached image)" })
        );
        assert_eq!(contents[1]["parts"][0]["text"], "Tool result image:");
        assert!(contents[1]["parts"][1]["inlineData"].is_object());
    }

    #[test]
    fn thinking_level_models_send_thinking_level() {
        let model = reasoning_model("gemini-3-pro-preview");
        let opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let body = build_request_body(&model, &Context::default(), &opts).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
        );
    }

    #[test]
    fn budget_models_send_thinking_budget() {
        let model = reasoning_model("gemini-2.5-flash");
        let opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..Default::default()
        };
        let body = build_request_body(&model, &Context::default(), &opts).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({ "includeThoughts": true, "thinkingBudget": 8192 })
        );

        // Caller budgets win over the family defaults.
        let opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Low),
            thinking_budgets: Some(ThinkingBudgets {
                low: Some(99),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = build_request_body(&model, &Context::default(), &opts).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(99)
        );
    }

    #[test]
    fn thinking_off_disables_or_omits_per_family() {
        let pro = reasoning_model("gemini-2.5-pro");
        let body =
            build_request_body(&pro, &Context::default(), &SimpleStreamOptions::default()).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({ "thinkingBudget": 0 })
        );

        let level_model = reasoning_model("gemini-3-flash-preview");
        let body = build_request_body(
            &level_model,
            &Context::default(),
            &SimpleStreamOptions::default(),
        )
        .unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({ "thinkingBudget": 0 })
        );

        // Non-reasoning models never carry a thinkingConfig.
        let plain = model("gemini-2.0-flash");
        let body = build_request_body(&plain, &Context::default(), &SimpleStreamOptions::default())
            .unwrap();
        assert!(body.pointer("/generationConfig/thinkingConfig").is_none());
    }

    #[test]
    fn max_output_tokens_defaults_to_the_model_ceiling() {
        let mut model = model("gemini-2.5-flash");
        model.max_tokens = 8192;
        let body = build_request_body(&model, &Context::default(), &SimpleStreamOptions::default())
            .unwrap();
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);

        // A pinned request value wins, clamped to the context window.
        let mut small = model.clone();
        small.context_window = 4096;
        let opts = SimpleStreamOptions {
            max_tokens: Some(100_000),
            ..Default::default()
        };
        let body = build_request_body(&small, &Context::default(), &opts).unwrap();
        let sent = body["generationConfig"]["maxOutputTokens"]
            .as_u64()
            .unwrap();
        assert!(sent < 100_000, "context clamp must apply: {sent}");
        assert!(sent >= 1);

        // No declared ceiling: omit the field rather than asking for one token.
        let bare = self::model("gemini-2.5-flash");
        let body = build_request_body(&bare, &Context::default(), &SimpleStreamOptions::default())
            .unwrap();
        assert!(body.get("generationConfig").is_none());
    }

    #[test]
    fn temperature_and_max_tokens_land_in_generation_config() {
        let model = model("gemini-2.5-flash");
        let opts = SimpleStreamOptions {
            temperature: Some(0.4),
            max_tokens: Some(1234),
            ..Default::default()
        };
        let body = build_request_body(&model, &Context::default(), &opts).unwrap();
        assert_eq!(
            body["generationConfig"],
            json!({ "temperature": 0.4, "maxOutputTokens": 1234 })
        );
    }

    #[test]
    fn same_model_signatures_are_replayed_on_the_matching_parts() {
        let model = model("gemini-3-pro-preview");
        let ctx = Context::new(vec![
            Message::User(crate::types::UserMessage::new("go", 0)),
            assistant(
                &model,
                vec![
                    Content::Thinking(crate::types::ThinkingContent {
                        kind: Default::default(),
                        thinking: "pondering".into(),
                        thinking_signature: Some(SIGNATURE.into()),
                        redacted: false,
                    }),
                    text_signature(SIGNATURE),
                    tool_call("call-1", Some(SIGNATURE)),
                ],
            ),
        ]);
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["thought"], json!(true));
        assert_eq!(parts[0]["thoughtSignature"], SIGNATURE);
        assert_eq!(parts[1]["text"], "done");
        assert_eq!(parts[1]["thoughtSignature"], SIGNATURE);
        assert_eq!(parts[2]["thoughtSignature"], SIGNATURE);
        // Gemini 3 requires explicit tool-call ids.
        assert_eq!(parts[2]["functionCall"]["id"], "call-1");
    }

    #[test]
    fn cross_model_signatures_are_dropped() {
        let target = model("gemini-3-pro-preview");
        let mut other = target.clone();
        other.id = "gemini-3-flash-preview".into();
        let ctx = Context::new(vec![
            Message::User(crate::types::UserMessage::new("go", 0)),
            assistant(
                &other,
                vec![
                    Content::Thinking(crate::types::ThinkingContent {
                        kind: Default::default(),
                        thinking: "pondering".into(),
                        thinking_signature: Some(SIGNATURE.into()),
                        redacted: false,
                    }),
                    text_signature(SIGNATURE),
                    tool_call("call-1", Some(SIGNATURE)),
                ],
            ),
        ]);
        let body = build_request_body(&target, &ctx, &SimpleStreamOptions::default()).unwrap();
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        // Thinking is downgraded to plain text without the `thought` channel.
        assert_eq!(parts[0], json!({ "text": "pondering" }));
        assert!(parts[0].get("thoughtSignature").is_none());
        assert_eq!(parts[1], json!({ "text": "done" }));
        assert!(parts[2].get("thoughtSignature").is_none());
    }

    #[test]
    fn non_base64_signatures_are_not_replayed() {
        let model = model("gemini-3-pro-preview");
        let ctx = Context::new(vec![
            Message::User(crate::types::UserMessage::new("go", 0)),
            assistant(&model, vec![text_signature("not base64!!")]),
        ]);
        let body = build_request_body(&model, &ctx, &SimpleStreamOptions::default()).unwrap();
        assert!(body["contents"][1]["parts"][0]
            .get("thoughtSignature")
            .is_none());
    }

    #[test]
    fn thought_level_map_overrides_the_level_and_rejects_unknown_values() {
        let mut model = reasoning_model("gemini-3-pro-preview");
        model.thinking_level_map = Some(std::collections::BTreeMap::from([(
            ThinkingLevel::Low,
            Some("minimal".into()),
        )]));
        let opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let body = build_request_body(&model, &Context::default(), &opts).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "MINIMAL"
        );

        model.thinking_level_map = Some(std::collections::BTreeMap::from([(
            ThinkingLevel::Low,
            Some("bogus".into()),
        )]));
        let error = build_request_body(&model, &Context::default(), &opts).unwrap_err();
        assert!(error.contains("Unsupported Google thinking level mapping"));
    }

    #[test]
    fn url_keeps_the_base_url_version_and_asks_for_sse() {
        assert_eq!(
            stream_url("https://generativelanguage.googleapis.com/v1beta/", "gemini-2.5-pro"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn model_predicates_follow_the_reference_table() {
        for id in [
            "gemini-3-pro-preview",
            "gemini-3.1-pro-preview",
            "gemini-3.8-flash",
            "gemini-flash-latest",
            "gemini-flash-lite-latest",
            "gemma-4-27b",
            "gemma4-27b",
        ] {
            assert!(uses_google_thinking_level(&model(id)), "{id}");
        }
        for id in ["gemini-2.5-pro", "gemini-2.5-flash", "claude-sonnet-4-5"] {
            assert!(!uses_google_thinking_level(&model(id)), "{id}");
        }
        assert_eq!(gemini_major_version("gemini-live-3-flash"), Some(3));
        assert_eq!(gemini_major_version("gemini-2.5-pro"), Some(2));
        assert_eq!(gemini_major_version("claude-sonnet-4-5"), None);
        assert!(requires_tool_call_id("gemini-3-pro-preview"));
        assert!(!requires_tool_call_id("gemini-2.5-pro"));
        assert!(requires_tool_call_id("claude-sonnet-4-5"));
        assert!(requires_tool_call_id("gpt-oss-120b"));
    }
}
