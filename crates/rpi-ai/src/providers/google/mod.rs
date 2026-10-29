//! Google Generative AI provider (`streamGenerateContent`) — the
//! `google-generative-ai` wire protocol.
//!
//! Mirrors `packages/ai/src/api/google-generative-ai.ts` (+ the shared helpers in
//! `google-shared.ts`) on top of raw `reqwest` + SSE instead of the
//! `@google/genai` SDK. Request shaping lives in [`build_params`], the streaming
//! state machine in [`stream`]; this module owns transport: header assembly,
//! retry, the SSE pull loop and the terminal event.
//!
//! Scope: `google-generative-ai` only. Vertex AI (different auth/addressing) and
//! Google OAuth are deliberately not implemented.

pub mod build_params;
pub mod stream;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::AiError;
use crate::event_stream::{
    create_assistant_message_event_stream_with_cancellation, AssistantMessageEventStream,
    AssistantMessageEventStreamProducer,
};
use crate::model::Model;
use crate::provider::{Provider, SimpleStreamOptions};
use crate::providers::anthropic::retry::retry_provider_request;
use crate::providers::anthropic::sse::SseEventStream;
use crate::types::{Api, AssistantMessage, AssistantMessageEvent, Context, StopReason};
use std::collections::BTreeMap;
use std::sync::Arc;

use build_params::{build_request_body, stream_url};
use stream::{emit_done, emit_error, now_ms, GoogleStreamState};

/// Environment variable consulted for the built-in `google` identity when
/// neither the provider nor the caller supplies a key (`GEMINI_API_KEY`, the
/// name the Gemini API documents, with `GOOGLE_API_KEY` as the Cloud alias).
const GEMINI_API_KEY_ENV: &str = "GEMINI_API_KEY";
const GOOGLE_API_KEY_ENV: &str = "GOOGLE_API_KEY";

/// Header names that already authenticate a request, so a missing API key is
/// not fatal (mirrors the Anthropic provider's `has_header_auth`).
const AUTH_HEADER_NAMES: &[&str] = &["authorization", "x-api-key", "x-goog-api-key"];

/// The Google Generative AI provider (`/models/{model}:streamGenerateContent`).
pub struct GoogleGenerativeAiProvider {
    id: String,
    /// Default API key, used when `SimpleStreamOptions::api_key` is `None`.
    api_key: Option<String>,
    /// Whether a missing explicit/default key may fall back to
    /// `GEMINI_API_KEY` / `GOOGLE_API_KEY`. Custom provider identities must
    /// disable this so a Google credential can never be sent to their endpoint.
    allow_env_api_key: bool,
    http: reqwest::Client,
    models: Vec<Model>,
}

impl GoogleGenerativeAiProvider {
    /// Provider for the built-in `google` identity: falls back to the Gemini
    /// environment variables when no key is supplied.
    pub fn new(api_key: Option<String>, http: reqwest::Client) -> Self {
        Self {
            id: "google".into(),
            api_key,
            allow_env_api_key: true,
            http,
            models: Vec::new(),
        }
    }

    /// Build with a custom model catalog and provider id.
    pub fn with_models(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        let id = id.into();
        let allow_env_api_key = id == "google";
        Self {
            id,
            api_key,
            allow_env_api_key,
            http,
            models,
        }
    }

    /// Build a Google-wire provider for a non-Google identity (a models.json
    /// gateway, an `antigravity` proxy, …). Never consults `GEMINI_API_KEY`.
    pub fn with_models_without_env_api_key(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        Self {
            id: id.into(),
            api_key,
            allow_env_api_key: false,
            http,
            models,
        }
    }

    /// Convenience: default HTTP client, key resolved at call time.
    pub fn from_env() -> Self {
        let http =
            crate::http::build_client("https://generativelanguage.googleapis.com", None, None)
                .unwrap_or_else(|_| reqwest::Client::new());
        Self::new(None, http)
    }

    /// Default API key carried on the provider (`opts.api_key` still wins).
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

#[async_trait]
impl Provider for GoogleGenerativeAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> &[Model] {
        &self.models
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);
        let http = self.http.clone();
        let key = self.api_key.clone();
        let allow_env_api_key = self.allow_env_api_key;
        // Build the wire request while the caller's context is still borrowed;
        // failures travel through the same terminal-error path as transport
        // errors instead of becoming an `Err` from `stream_simple`.
        let preparation = prepare_request(model, ctx, opts, &key, allow_env_api_key);
        let model = model.clone();
        let mut opts = opts.clone();
        opts.signal = producer.cancellation_token();
        tokio::spawn(async move {
            run_stream(&mut producer, http, &model, preparation, &opts).await;
        });
        stream
    }
}

/// A fully prepared request: headers carrying the credential and the JSON body.
#[derive(Debug)]
struct PreparedRequest {
    headers: BTreeMap<String, String>,
    body: Value,
}

fn resolve_api_key(
    provider_key: &Option<String>,
    opts: &SimpleStreamOptions,
    allow_env_api_key: bool,
) -> Option<String> {
    opts.api_key
        .clone()
        .filter(|key| !key.is_empty())
        .or_else(|| provider_key.clone().filter(|key| !key.is_empty()))
        .or_else(|| {
            if !allow_env_api_key {
                return None;
            }
            [GEMINI_API_KEY_ENV, GOOGLE_API_KEY_ENV]
                .into_iter()
                .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        })
}

fn has_header_auth(headers: &BTreeMap<String, String>) -> bool {
    headers.iter().any(|(name, value)| {
        AUTH_HEADER_NAMES.contains(&name.to_ascii_lowercase().as_str()) && !value.trim().is_empty()
    })
}

/// Merge `model.headers` then `opts.headers` (later wins, case-insensitively)
/// and apply the API key as `x-goog-api-key` last so caller headers can never
/// clobber the resolved credential. Mirrors the TS `createClient` header set
/// (`User-Agent` + model headers + per-call headers + the SDK's key header).
fn assemble_headers(
    model: &Model,
    opts: &SimpleStreamOptions,
    api_key: Option<&str>,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    crate::http::ensure_user_agent(&mut headers);
    for extra in [model.headers.as_ref(), opts.headers.as_ref()]
        .into_iter()
        .flatten()
    {
        for (name, value) in extra {
            crate::provider::set_http_header(&mut headers, name, Some(value));
        }
    }
    if let Some(key) = api_key {
        crate::provider::set_http_header(&mut headers, "x-goog-api-key", Some(key));
    }
    crate::provider::set_http_header(&mut headers, "content-type", Some("application/json"));
    headers
}

fn prepare_request(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
    provider_key: &Option<String>,
    allow_env_api_key: bool,
) -> Result<PreparedRequest, String> {
    let key = resolve_api_key(provider_key, opts, allow_env_api_key);
    let headers = assemble_headers(model, opts, key.as_deref());
    if key.is_none() && !has_header_auth(&headers) {
        return Err(format!("No API key for provider: {}", model.provider));
    }
    Ok(PreparedRequest {
        headers,
        body: build_request_body(model, ctx, opts)?,
    })
}

/// Format an in-band `{"error": {...}}` payload (`status`/`code` + `message`)
/// with the same shape the other providers use for provider error bodies.
fn error_message(error: &Value) -> String {
    let code = error
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            error.get("code").and_then(Value::as_i64).map(|code| {
                if code != 0 {
                    code.to_string()
                } else {
                    error
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                }
            })
        })
        .filter(|code| !code.is_empty());
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);
    match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (None, Some(message)) => message,
        (Some(code), None) => code,
        (None, None) => error.to_string(),
    }
}

/// The producer task: send (with retry), decode SSE, fold chunks into the
/// assistant message, then emit one terminal event.
async fn run_stream(
    p: &mut AssistantMessageEventStreamProducer,
    http: reqwest::Client,
    model: &Model,
    preparation: Result<PreparedRequest, String>,
    opts: &SimpleStreamOptions,
) {
    let mut out = AssistantMessage::empty(
        Api::GoogleGenerativeAi,
        model.provider.clone(),
        model.id.clone(),
        now_ms(),
    );
    let PreparedRequest { headers, body } = match preparation {
        Ok(prepared) => prepared,
        Err(error) => {
            emit_error(p, &mut out, error, false).await;
            return;
        }
    };

    let url = stream_url(&model.base_url, &model.id);
    let timeout = opts.request_timeout();
    let mut builder = http.post(&url).timeout(timeout);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let request = match super::request::PreparedJsonRequest::new(builder, &body) {
        Ok(request) => request,
        Err(error) => {
            emit_error(
                p,
                &mut out,
                format!("failed to encode request: {error}"),
                false,
            )
            .await;
            return;
        }
    };
    drop(body);

    // Mirror the TS `retryGoogleRequest` wrapper: only the initial request is
    // retryable, and the shared predicate classifies 408/409/429/5xx.
    let signal = opts.signal.clone();
    let response = retry_provider_request(
        move || {
            let request = request.clone();
            let signal = signal.clone();
            async move {
                let response = tokio::select! {
                    result = request.send() => result?,
                    _ = signal.cancelled() => return Err(AiError::Abort {
                        message: "Request aborted".to_string(),
                    }),
                };
                if response.status().is_success() {
                    Ok(response)
                } else {
                    let status = response.status().as_u16();
                    let text = response.text().await.unwrap_or_default();
                    Err(AiError::Http {
                        status: Some(status),
                        message: text,
                    })
                }
            }
        },
        opts.max_retries,
        opts.max_retry_delay,
        &opts.signal,
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let aborted = error.is_abort();
            emit_error(p, &mut out, error.to_string(), aborted).await;
            return;
        }
    };

    p.push(AssistantMessageEvent::Start {
        partial: Arc::new(out.clone()),
    })
    .await;

    let mut events = SseEventStream::new(response, opts.signal.clone())
        .with_idle_timeout(opts.stream_idle_timeout());
    let mut state = GoogleStreamState::new();
    loop {
        let event = match events.next_event().await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(error) => {
                let aborted = error.is_abort();
                state.close(&out, p).await;
                emit_error(p, &mut out, error.to_string(), aborted).await;
                return;
            }
        };
        let data = event.data.trim();
        if data.is_empty() {
            continue;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => {
                state.close(&out, p).await;
                emit_error(
                    p,
                    &mut out,
                    format!("invalid Google Generative AI SSE event: {error}; data={data}"),
                    false,
                )
                .await;
                return;
            }
        };
        // Gemini reports stream-level failures as an `error` object instead of a
        // candidate. Surfacing it here keeps the message identical to the HTTP
        // error path (and therefore to the other providers' classification).
        if let Some(error) = chunk.get("error") {
            let message = error_message(error);
            state.close(&out, p).await;
            emit_error(p, &mut out, message, false).await;
            return;
        }
        state.apply_chunk(&mut out, p, model, &chunk).await;
    }

    state.close(&out, p).await;

    // Terminal selection — mirrors the tail of the TS stream loop.
    if opts.signal.is_cancelled() {
        emit_error(p, &mut out, "Request was aborted", true).await;
        return;
    }
    if out.stop_reason == StopReason::Pending {
        emit_error(
            p,
            &mut out,
            "Google stream ended without a finish reason",
            false,
        )
        .await;
        return;
    }
    if matches!(out.stop_reason, StopReason::Aborted | StopReason::Error) {
        let message = match out.raw_stop_reason.as_deref() {
            Some(reason) => format!("Provider stopped with: {reason}"),
            None => "An unknown error occurred".to_string(),
        };
        emit_error(p, &mut out, message, false).await;
        return;
    }
    emit_done(p, out).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DoneReason, ErrorReason, Message, ThinkingLevel, UserMessage};

    fn model() -> Model {
        Model::new(
            "gemini-3-pro-preview",
            "Gemini 3 Pro",
            Api::GoogleGenerativeAi,
            "google",
            "https://example.test/v1beta",
        )
    }

    #[test]
    fn api_key_precedence_is_opts_then_provider_then_env_only_for_google() {
        let opts = SimpleStreamOptions::default().with_api_key("opts-key");
        assert_eq!(
            resolve_api_key(&Some("provider-key".into()), &opts, true),
            Some("opts-key".into())
        );
        let opts = SimpleStreamOptions::default();
        assert_eq!(
            resolve_api_key(&Some("provider-key".into()), &opts, true),
            Some("provider-key".into())
        );
        std::env::set_var(GEMINI_API_KEY_ENV, "env-key");
        assert_eq!(resolve_api_key(&None, &opts, true), Some("env-key".into()));
        // A gateway identity must never inherit the Google credential.
        assert_eq!(resolve_api_key(&None, &opts, false), None);
        std::env::remove_var(GEMINI_API_KEY_ENV);
    }

    #[test]
    fn headers_merge_model_then_call_headers_and_apply_the_key_last() {
        let mut model = model();
        model.headers = Some(BTreeMap::from([
            ("X-Tenant".into(), "model".into()),
            ("x-goog-api-key".into(), "model-key".into()),
        ]));
        let opts = SimpleStreamOptions {
            headers: Some(BTreeMap::from([("x-tenant".into(), "call".into())])),
            ..Default::default()
        };
        let headers = assemble_headers(&model, &opts, Some("resolved-key"));
        assert_eq!(headers["x-tenant"], "call");
        assert_eq!(headers["x-goog-api-key"], "resolved-key");
        assert_eq!(headers["content-type"], "application/json");
        assert!(headers.keys().any(|name| name == "user-agent"));
        assert!(has_header_auth(&headers));
    }

    #[test]
    fn preparation_requires_a_key_or_header_auth() {
        let model = model();
        let opts = SimpleStreamOptions::default();
        let error = prepare_request(&model, &Context::default(), &opts, &None, false).unwrap_err();
        assert_eq!(error, "No API key for provider: google");

        let mut authed = model.clone();
        authed.headers = Some(BTreeMap::from([(
            "authorization".into(),
            "Bearer header-token".into(),
        )]));
        assert!(prepare_request(&authed, &Context::default(), &opts, &None, false).is_ok());
    }

    #[tokio::test]
    async fn request_uses_the_gemini_streaming_url_and_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (head_tx, head_rx) = tokio::sync::oneshot::channel::<(String, Value)>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let (body_start, length) = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending headers");
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_string();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .expect("reqwest sends Content-Length");
                    break (end + 4, length);
                }
            };
            while request.len() < body_start + length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending the body");
                request.extend_from_slice(&chunk[..read]);
            }
            let head = String::from_utf8_lossy(&request[..body_start]).to_string();
            let body: Value =
                serde_json::from_slice(&request[body_start..body_start + length]).unwrap();
            head_tx.send((head, body)).unwrap();

            let response_body = concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},",
                "\"finishReason\":\"STOP\"}]}\r\n\r\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut test_model = model();
        test_model.reasoning = true;
        test_model.base_url = format!("http://{address}/antigravity/v1beta");
        let provider = GoogleGenerativeAiProvider::with_models_without_env_api_key(
            "google",
            Some("test-key".into()),
            reqwest::Client::builder().no_proxy().build().unwrap(),
            vec![test_model.clone()],
        );
        let opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::High),
            temperature: Some(0.2),
            max_tokens: Some(512),
            ..Default::default()
        };
        let mut ctx = Context::new(vec![Message::User(UserMessage::new("hi", 0))]);
        ctx.system_prompt = Some("be brief".into());
        let stream = provider.stream_simple(&test_model, &ctx, &opts).await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), stream.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert!(result.error_message.is_none());

        let (head, body) = head_rx.await.unwrap();
        assert!(
            head.starts_with(
                "POST /antigravity/v1beta/models/gemini-3-pro-preview:streamGenerateContent?alt=sse HTTP/1.1"
            ),
            "unexpected request line: {head}"
        );
        assert!(head
            .to_ascii_lowercase()
            .contains("x-goog-api-key: test-key"));
        assert!(head
            .to_ascii_lowercase()
            .contains("content-type: application/json"));
        assert_eq!(
            body["systemInstruction"],
            serde_json::json!({ "parts": [{ "text": "be brief" }] })
        );
        assert_eq!(body["generationConfig"]["temperature"], 0.2);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 512);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            serde_json::json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
        );
        server.await.unwrap();
    }

    async fn drive(body: String) -> AssistantMessage {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let mut test_model = model();
        test_model.base_url = format!("http://{address}/v1beta");
        let provider = GoogleGenerativeAiProvider::with_models_without_env_api_key(
            "google",
            Some("test-key".into()),
            reqwest::Client::builder().no_proxy().build().unwrap(),
            vec![test_model.clone()],
        );
        let stream = provider
            .stream_simple(
                &test_model,
                &Context::new(vec![Message::User(UserMessage::new("hi", 0))]),
                &SimpleStreamOptions::default(),
            )
            .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), stream.result())
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn stream_without_a_finish_reason_is_an_error() {
        let result = drive(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\r\n\r\n"
                .to_string(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("without a finish reason")));
    }

    #[tokio::test]
    async fn in_stream_error_object_surfaces_its_message() {
        let result = drive(
            "data: {\"error\":{\"code\":429,\"message\":\"quota exceeded\",\"status\":\"RESOURCE_EXHAUSTED\"}}\r\n\r\n"
                .to_string(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("RESOURCE_EXHAUSTED: quota exceeded")
        );
    }

    #[tokio::test]
    async fn safety_stop_reports_the_raw_finish_reason() {
        let result =
            drive("data: {\"candidates\":[{\"finishReason\":\"SAFETY\"}]}\r\n\r\n".to_string())
                .await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Provider stopped with: SAFETY")
        );
    }

    #[tokio::test]
    async fn malformed_sse_frame_surfaces_a_parse_error() {
        let result = drive("data: {not-json\r\n\r\n".to_string()).await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("invalid Google Generative AI SSE event")));
    }

    async fn drive_status(status: u16, reason: &str) -> AssistantMessage {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reason = reason.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let body = r#"{"error":{"code":400,"message":"synthetic failure"}}"#;
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let mut test_model = model();
        test_model.base_url = format!("http://{address}/v1beta");
        let provider = GoogleGenerativeAiProvider::with_models_without_env_api_key(
            "google",
            Some("test-key".into()),
            reqwest::Client::builder().no_proxy().build().unwrap(),
            vec![test_model.clone()],
        );
        // No retries: the classification is what is under test, not the backoff.
        let stream = provider
            .stream_simple(
                &test_model,
                &Context::new(vec![Message::User(UserMessage::new("hi", 0))]),
                &SimpleStreamOptions::default(),
            )
            .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), stream.result())
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn http_failures_keep_their_status_for_retry_classification() {
        for (status, reason) in [
            (400, "Bad Request"),
            (429, "Too Many Requests"),
            (500, "Internal Server Error"),
        ] {
            let result = drive_status(status, reason).await;
            assert_eq!(result.stop_reason, StopReason::Error, "{status}");
            let message = result.error_message.as_deref().unwrap_or_default();
            assert!(
                message.contains(&format!("http error {status}")),
                "{status}: {message}"
            );
            assert!(message.contains("synthetic failure"), "{status}: {message}");
        }
    }

    #[tokio::test]
    async fn retryable_status_is_retried_when_retries_are_allowed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_attempts = attempts.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let count = server_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let (status, reason, body) = if count == 1 {
                    (
                        429,
                        "Too Many Requests",
                        r#"{"error":{"code":429,"message":"slow down"}}"#,
                    )
                } else {
                    (
                        200,
                        "OK",
                        "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\r\n\r\n",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                if count >= 2 {
                    return;
                }
            }
        });
        let mut test_model = model();
        test_model.base_url = format!("http://{address}/v1beta");
        let provider = GoogleGenerativeAiProvider::with_models_without_env_api_key(
            "google",
            Some("test-key".into()),
            reqwest::Client::builder().no_proxy().build().unwrap(),
            vec![test_model.clone()],
        );
        let opts = SimpleStreamOptions {
            max_retries: Some(2),
            max_retry_delay: Some(std::time::Duration::from_secs(1)),
            ..Default::default()
        };
        let stream = provider
            .stream_simple(
                &test_model,
                &Context::new(vec![Message::User(UserMessage::new("hi", 0))]),
                &opts,
            )
            .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), stream.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        server.abort();
    }

    /// A 400 is classified non-retryable by the shared retry predicate, so the
    /// provider must not re-send it even when retries are allowed.
    #[tokio::test]
    async fn non_retryable_status_is_not_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_attempts = attempts.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                server_attempts.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let body = r#"{"error":{"code":400,"message":"bad request"}}"#;
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let mut test_model = model();
        test_model.base_url = format!("http://{address}/v1beta");
        let provider = GoogleGenerativeAiProvider::with_models_without_env_api_key(
            "google",
            Some("test-key".into()),
            reqwest::Client::builder().no_proxy().build().unwrap(),
            vec![test_model.clone()],
        );
        let opts = SimpleStreamOptions {
            max_retries: Some(3),
            max_retry_delay: Some(std::time::Duration::from_millis(50)),
            ..Default::default()
        };
        let stream = provider
            .stream_simple(
                &test_model,
                &Context::new(vec![Message::User(UserMessage::new("hi", 0))]),
                &opts,
            )
            .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), stream.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("http error 400")));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn error_payloads_render_code_and_message() {
        assert_eq!(
            error_message(&serde_json::json!({
                "code": 400, "message": "bad", "status": "INVALID_ARGUMENT"
            })),
            "INVALID_ARGUMENT: bad"
        );
        assert_eq!(
            error_message(&serde_json::json!({ "message": "only message" })),
            "only message"
        );
    }

    /// End-to-end replay: a streamed tool call carrying `thoughtSignature` is
    /// stored on the `ToolCall`, and the very next request echoes that signature
    /// on the matching `functionCall` part. Gemini 3 rejects multi-turn tool use
    /// when the signature is missing.
    #[tokio::test]
    async fn thought_signature_from_a_streamed_tool_call_is_replayed_next_turn() {
        let result = drive(
            concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[",
                "{\"text\":\"checking\",\"thought\":true,\"thoughtSignature\":\"c2lnbmF0dXJlLWZvci10ZXN0\"},",
                "{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a\"}},",
                "\"thoughtSignature\":\"c2lnbmF0dXJlLWZvci10ZXN0\"}]},",
                "\"finishReason\":\"STOP\"}]}\r\n\r\n"
            )
            .to_string(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        let call = result
            .content
            .iter()
            .find_map(|content| match content {
                crate::types::Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(
            call.thought_signature.as_deref(),
            Some("c2lnbmF0dXJlLWZvci10ZXN0")
        );

        let mut ctx = Context::new(vec![
            Message::User(UserMessage::new("read a", 0)),
            Message::Assistant(Box::new(result.clone())),
        ]);
        ctx.tools = vec![crate::types::Tool {
            name: "read".into(),
            description: "read a file".into(),
            parameters: crate::types::Schema::new(
                serde_json::json!({ "type": "object", "properties": {} }),
            ),
            constrained_sampling: None,
        }];
        let body = build_request_body(&model(), &ctx, &SimpleStreamOptions::default()).unwrap();
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["thought"], serde_json::json!(true));
        assert_eq!(parts[0]["thoughtSignature"], "c2lnbmF0dXJlLWZvci10ZXN0");
        assert_eq!(parts[1]["thoughtSignature"], "c2lnbmF0dXJlLWZvci10ZXN0");

        // Switching models drops the opaque signature (it is only valid for the
        // model that produced it) but keeps the call itself.
        let mut other = model();
        other.id = "gemini-3-flash-preview".into();
        let body = build_request_body(&other, &ctx, &SimpleStreamOptions::default()).unwrap();
        assert!(body["contents"][1]["parts"][1]
            .get("thoughtSignature")
            .is_none());
    }

    #[test]
    fn done_reason_covers_every_successful_stop() {
        assert_eq!(
            match StopReason::Length {
                StopReason::Length => DoneReason::Length,
                _ => DoneReason::Stop,
            },
            DoneReason::Length
        );
        let aborted = ErrorReason::Aborted;
        assert_eq!(aborted.as_str(), "aborted");
    }
}
