//! Host-handled state paths (`--host-state <prefix>`) bridged to a
//! `--mode rpc` client.
//!
//! A desktop host that drives `rpi --mode rpc` may keep some project paths in
//! its own memory instead of on disk: a plan card, a question to the user, a
//! timeline. Those paths are ordinary files to the model — it calls `read`,
//! `write`, `edit` or `ls` on them — but the engine never touches the disk for
//! them, it forwards the call to the host and waits for its answer:
//!
//! - declaration: `--host-state <prefix>`, a directory relative to the working
//!   directory (mu passes `.mu/state`). `--mode rpc` only; without the flag
//!   nothing below happens and the tools are upstream's byte for byte;
//! - match: the call's `path` argument is normalized lexically (after the
//!   working directory, `.`/`..` segments, and `\` separators are resolved) and
//!   compared component-wise with the prefix. Inside, the call is forwarded;
//!   outside, the tool runs unchanged. Normalization never touches the disk, so
//!   a `..` that leaves the prefix is a normal path and a state path that does
//!   not exist on disk still matches;
//! - call: the RPC loop writes
//!   `{"type":"host_state_call","requestId",…,"toolCallId",…,"tool":"read",
//!   "path":"ask/topic.md",…}` where `path` is relative to the prefix and uses
//!   `/`;
//! - result: the host writes
//!   `{"type":"host_state_result","requestId",…,"text":…,"isError"?}`; `text` is
//!   the tool result verbatim;
//! - cancel: an aborted run (or a wait that timed out) writes
//!   `{"type":"host_state_cancelled","requestId"}`, and a later result for that
//!   id is rejected as unknown.
//!
//! The tool schemas, names, descriptions and the tool list are unchanged: only
//! dispatch differs, so the provider prefix cache is unaffected.
//! `docs/host-state.md` is the protocol of record.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use rpi_harness::types::HarnessTool;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::args::{Args, Mode};

/// Wait for the host's answer this long unless the host says otherwise.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The tools whose calls may be routed to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateTool {
    Read,
    Write,
    Edit,
    Ls,
}

impl StateTool {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "edit" => Some(Self::Edit),
            "ls" => Some(Self::Ls),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Ls => "ls",
        }
    }
}

/// The optional arguments of one forwarded call, in protocol order.
#[derive(Debug, Clone, Default)]
pub(crate) struct CallFields {
    content: Option<String>,
    old_text: Option<String>,
    new_text: Option<String>,
    replace_all: Option<bool>,
    offset: Option<u64>,
    limit: Option<u64>,
}

/// What the RPC loop must write to the host: a call, or its cancellation.
#[derive(Debug, Clone)]
pub(crate) enum Event {
    Call {
        request_id: String,
        tool_call_id: String,
        tool: StateTool,
        path: String,
        fields: CallFields,
    },
    Cancelled {
        request_id: String,
    },
}

impl Event {
    /// The JSONL line the host reads. Field order is the documented order, and
    /// absent optional arguments are omitted rather than sent as `null`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Call {
                request_id,
                tool_call_id,
                tool,
                path,
                fields,
            } => {
                let mut value = serde_json::json!({
                    "type": "host_state_call",
                    "requestId": request_id,
                    "toolCallId": tool_call_id,
                    "tool": tool.as_str(),
                    "path": path,
                });
                let object = value
                    .as_object_mut()
                    .expect("host_state_call is an object by construction");
                if let Some(content) = &fields.content {
                    object.insert("content".into(), serde_json::json!(content));
                }
                if let Some(old_text) = &fields.old_text {
                    object.insert("oldText".into(), serde_json::json!(old_text));
                }
                if let Some(new_text) = &fields.new_text {
                    object.insert("newText".into(), serde_json::json!(new_text));
                }
                if let Some(replace_all) = fields.replace_all {
                    object.insert("replaceAll".into(), serde_json::json!(replace_all));
                }
                if let Some(offset) = fields.offset {
                    object.insert("offset".into(), serde_json::json!(offset));
                }
                if let Some(limit) = fields.limit {
                    object.insert("limit".into(), serde_json::json!(limit));
                }
                value
            }
            Self::Cancelled { request_id } => serde_json::json!({
                "type": "host_state_cancelled",
                "requestId": request_id,
            }),
        }
    }
}

/// The `host_state_result` command payload, as sent by the host.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStateResultCommand {
    pub request_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub is_error: bool,
}

/// The optional `host_state_config` command payload. Every field is optional and
/// only the ones present are changed, so a host may set the read wait once and
/// leave the default timeout alone.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStateConfigCommand {
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    /// Paths relative to the prefix whose reads may wait longer (for example
    /// `ask`, where the host waits for the user to answer).
    #[serde(default)]
    pub wait_paths: Option<Vec<String>>,
}

/// What `ready` advertises about the feature: the prefix as written on the
/// command line plus the current timeouts in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStateAdvertisement {
    pub prefix: String,
    pub timeout_ms: u64,
    pub wait_timeout_ms: u64,
}

#[derive(Debug, Clone)]
struct Timeouts {
    call: Duration,
    wait: Duration,
    wait_paths: Vec<String>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            call: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            wait: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            wait_paths: Vec::new(),
        }
    }
}

struct State {
    /// The prefix as written on the command line, for `ready` and errors.
    display: String,
    /// The working directory as given, which every relative call path is
    /// resolved against; normalization happens per call.
    cwd: String,
    prefix_root: String,
    prefix_segments: Vec<String>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<(String, bool), String>>>>,
    timeouts: Mutex<Timeouts>,
    events: mpsc::UnboundedSender<Event>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
    /// False once the RPC loop is gone (stdin EOF / `stop`): no new call can be
    /// answered, so the tool fails immediately instead of waiting forever.
    connected: AtomicBool,
}

/// Connects `--host-state` paths to the `--mode rpc` command loop.
///
/// Cheap to clone; every clone shares the same prefix and in-flight calls.
#[derive(Clone)]
pub struct HostStateBridge {
    /// `None` is the disabled bridge: no prefix, no relay, no wrapping.
    state: Option<Arc<State>>,
}

impl std::fmt::Debug for HostStateBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.state {
            Some(state) => formatter
                .debug_struct("HostStateBridge")
                .field("prefix", &state.display)
                .finish(),
            None => formatter.debug_struct("HostStateBridge").finish(),
        }
    }
}

impl Default for HostStateBridge {
    fn default() -> Self {
        Self::disabled()
    }
}

impl HostStateBridge {
    /// The common case: `--host-state` was not given.
    pub fn disabled() -> Self {
        Self { state: None }
    }

    fn new(cwd: &Path, display: String) -> Result<Self, String> {
        let cwd_raw = cwd.to_string_lossy().into_owned();
        let (cwd_root, cwd_segments) = normalize_lexical(&cwd_raw);
        let (prefix_root, prefix_segments) = resolve_lexical(&cwd_raw, &display);
        let inside = prefix_root == cwd_root
            && prefix_segments.len() >= cwd_segments.len()
            && cwd_segments
                .iter()
                .zip(&prefix_segments)
                .all(|(left, right)| segment_eq(left, right));
        if !inside {
            return Err(format!(
                "--host-state \"{display}\" must be a directory inside the working directory"
            ));
        }
        if prefix_segments.len() == cwd_segments.len() {
            return Err(format!(
                "--host-state \"{display}\" is the working directory itself; give a subdirectory such as .mu/state"
            ));
        }
        let (events, receiver) = mpsc::unbounded_channel();
        Ok(Self {
            state: Some(Arc::new(State {
                display,
                cwd: cwd_raw,
                prefix_root,
                prefix_segments,
                pending: Mutex::new(HashMap::new()),
                timeouts: Mutex::new(Timeouts::default()),
                events,
                receiver: Mutex::new(Some(receiver)),
                connected: AtomicBool::new(true),
            })),
        })
    }

    pub fn enabled(&self) -> bool {
        self.state.is_some()
    }

    /// The prefix as written on the command line (`None` when disabled).
    pub fn prefix(&self) -> Option<&str> {
        self.state.as_ref().map(|state| state.display.as_str())
    }

    pub fn advertisement(&self) -> Option<HostStateAdvertisement> {
        let state = self.state.as_ref()?;
        let timeouts = state.timeouts();
        Some(HostStateAdvertisement {
            prefix: state.display.clone(),
            timeout_ms: timeouts.call.as_millis() as u64,
            wait_timeout_ms: timeouts.wait.as_millis() as u64,
        })
    }

    /// The path relative to the prefix (`""` for the prefix itself, `"a/b.md"`
    /// for `<prefix>/a/b.md`), using `/`; `None` when the path is outside.
    ///
    /// Lexical only: nothing on disk is read or written, so this also answers
    /// for paths that only exist in the host.
    pub fn state_path(&self, path: &str) -> Option<String> {
        let state = self.state.as_ref()?;
        let (root, segments) = resolve_lexical(&state.cwd, path);
        if root != state.prefix_root || segments.len() < state.prefix_segments.len() {
            return None;
        }
        if !state
            .prefix_segments
            .iter()
            .zip(&segments)
            .all(|(prefix, segment)| segment_eq(prefix, segment))
        {
            return None;
        }
        Some(segments[state.prefix_segments.len()..].join("/"))
    }

    /// Wrap the built-in `read`/`write`/`edit`/`ls` tools so a call whose path
    /// falls inside the prefix is answered by the host. Disabled (no flag) or a
    /// tool with another name: the tool is left untouched, `Arc` identity
    /// included.
    pub fn wrap(&self, tools: &mut [HarnessTool]) {
        if !self.enabled() {
            return;
        }
        for tool in tools.iter_mut() {
            let Some(state_tool) = StateTool::from_name(&tool.tool.schema().name) else {
                continue;
            };
            tool.tool = Arc::new(HostStateTool {
                inner: Arc::clone(&tool.tool),
                bridge: self.clone(),
                tool: state_tool,
            });
        }
    }

    /// Hand the call stream to the single RPC loop relay. Returns `None` once
    /// it has been taken (or when the feature is off).
    pub(crate) fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.state.as_ref()?.receiver.lock().ok()?.take()
    }

    /// Answer the call with that request id. An unknown or already settled id
    /// (cancelled, timed out, answered, or never issued) is an error for the
    /// host.
    pub fn respond(&self, request_id: &str, text: String, is_error: bool) -> Result<(), String> {
        let Some(state) = self.state.as_ref() else {
            return Err("--host-state is not enabled".to_string());
        };
        let sender = state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(request_id);
        match sender {
            Some(sender) => {
                let _ = sender.send(Ok((text, is_error)));
                Ok(())
            }
            None => Err(format!("unknown host state request `{request_id}`")),
        }
    }

    /// Apply a `host_state_config` command. Only the fields present change.
    pub fn configure(&self, config: HostStateConfigCommand) -> Result<(), String> {
        let Some(state) = self.state.as_ref() else {
            return Err("--host-state is not enabled".to_string());
        };
        let mut timeouts = state
            .timeouts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(timeout_ms) = config.timeout_ms {
            if timeout_ms == 0 {
                return Err("host_state_config: timeoutMs must be at least 1".to_string());
            }
            timeouts.call = Duration::from_millis(timeout_ms);
        }
        if let Some(wait_timeout_ms) = config.wait_timeout_ms {
            if wait_timeout_ms == 0 {
                return Err("host_state_config: waitTimeoutMs must be at least 1".to_string());
            }
            timeouts.wait = Duration::from_millis(wait_timeout_ms);
        }
        if let Some(wait_paths) = config.wait_paths {
            timeouts.wait_paths = wait_paths
                .into_iter()
                .map(|path| normalize_relative(&path))
                .collect();
        }
        Ok(())
    }

    /// End the session: no further answer can be read, so every in-flight call
    /// fails now instead of waiting for a host that is gone.
    pub fn disconnect(&self) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        state.connected.store(false, Ordering::Release);
        let pending: Vec<_> = state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain()
            .map(|(_, sender)| sender)
            .collect();
        for sender in pending {
            let _ = sender.send(Err("host disconnected".to_string()));
        }
    }

    /// Route one call to the host and wait for `host_state_result`, the abort
    /// signal, or the timeout.
    async fn call(
        &self,
        tool_call_id: &str,
        tool: StateTool,
        path: String,
        fields: CallFields,
        signal: CancellationToken,
    ) -> Result<AgentToolResult, AgentError> {
        let Some(state) = self.state.as_ref() else {
            return Err(AgentError::Tool("--host-state is not enabled".to_string()));
        };
        if !state.connected.load(Ordering::Acquire) {
            return Err(AgentError::Tool("host disconnected".to_string()));
        }
        let timeout = state.timeout_for(&path);
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let (sender, receiver) = oneshot::channel();
        state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(request_id.clone(), sender);
        let sent = state.events.send(Event::Call {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool,
            path: path.clone(),
            fields,
        });
        // Re-check after announcing: `disconnect` drains whatever was pending at
        // that instant, so a call announced during shutdown must not be left
        // waiting for an answer nobody can send.
        if sent.is_err() || !state.connected.load(Ordering::Acquire) {
            state
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&request_id);
            return Err(AgentError::Tool("host disconnected".to_string()));
        }
        tokio::select! {
            biased;
            _ = signal.cancelled() => {
                self.retire(&request_id);
                Err(AgentError::Tool(format!(
                    "host state {} `{path}` cancelled",
                    tool.as_str()
                )))
            }
            answer = receiver => match answer {
                Ok(Ok((text, is_error))) => {
                    if is_error {
                        Err(AgentError::Tool(text))
                    } else {
                        Ok(AgentToolResult {
                            content: vec![TextContentOrImage::text(text)],
                            details: serde_json::Value::Null,
                            usage: None,
                            added_tool_names: Vec::new(),
                            terminate: false,
                        })
                    }
                }
                Ok(Err(error)) => Err(AgentError::Tool(error)),
                Err(_) => Err(AgentError::Tool("host disconnected".to_string())),
            },
            _ = tokio::time::sleep(timeout) => {
                self.retire(&request_id);
                Err(AgentError::Tool(format!(
                    "host state {} `{path}` got no answer within {} ms",
                    tool.as_str(),
                    timeout.as_millis()
                )))
            }
        }
    }

    /// Retire the request and tell the host the run no longer waits for it.
    fn retire(&self, request_id: &str) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let removed = state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(request_id);
        if removed.is_some() {
            let _ = state.events.send(Event::Cancelled {
                request_id: request_id.to_string(),
            });
        }
    }
}

impl State {
    fn timeouts(&self) -> Timeouts {
        self.timeouts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Reads of a declared wait path get the longer limit; everything else the
    /// normal one.
    fn timeout_for(&self, path: &str) -> Duration {
        let timeouts = self.timeouts();
        if timeouts.wait_paths.iter().any(|wait| {
            wait.is_empty()
                || path == wait
                || path
                    .strip_prefix(wait.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        }) {
            timeouts.wait
        } else {
            timeouts.call
        }
    }
}

/// One built-in tool with host-state dispatch in front of it.
struct HostStateTool {
    inner: Arc<dyn AgentTool>,
    bridge: HostStateBridge,
    tool: StateTool,
}

impl HostStateTool {
    /// The `(relative path, forwarded arguments)` pair when this call belongs to
    /// the host, `None` when the inner tool must run.
    fn forward(
        &self,
        params: &serde_json::Value,
    ) -> Result<Option<(String, CallFields)>, AgentError> {
        let Some(path) = params.get("path").and_then(|value| value.as_str()) else {
            // No path argument (`ls` defaults to the working directory) or an
            // unusable one: the inner tool owns the validation error.
            return Ok(None);
        };
        let Some(relative) = self.bridge.state_path(path) else {
            return Ok(None);
        };
        let number = |key: &str| params.get(key).and_then(|value| value.as_u64());
        let fields = match self.tool {
            StateTool::Read => CallFields {
                offset: number("offset"),
                limit: number("limit"),
                ..Default::default()
            },
            StateTool::Write => {
                let Some(content) = params.get("content").and_then(|value| value.as_str()) else {
                    return Ok(None);
                };
                CallFields {
                    content: Some(content.to_string()),
                    ..Default::default()
                }
            }
            StateTool::Edit => {
                let edits = edit_pairs(params);
                if edits.is_empty() {
                    return Ok(None);
                }
                if edits.len() > 1 {
                    return Err(AgentError::Tool(
                        "Host state paths take one replacement per call: send each oldText/newText as its own edit.".to_string(),
                    ));
                }
                let (old_text, new_text, replace_all) =
                    edits.into_iter().next().unwrap_or_default();
                CallFields {
                    old_text: Some(old_text),
                    new_text: Some(new_text),
                    replace_all,
                    ..Default::default()
                }
            }
            StateTool::Ls => CallFields {
                limit: number("limit"),
                ..Default::default()
            },
        };
        Ok(Some((relative, fields)))
    }
}

/// The replacements of an `edit` call: the prepared `edits` array, or the legacy
/// top-level `oldText`/`newText` pair an unprepared caller may still send.
fn edit_pairs(params: &serde_json::Value) -> Vec<(String, String, Option<bool>)> {
    if let Some(edits) = params.get("edits").and_then(|value| value.as_array()) {
        return edits
            .iter()
            .filter_map(|edit| {
                let old_text = edit.get("oldText").and_then(|value| value.as_str())?;
                let new_text = edit.get("newText").and_then(|value| value.as_str())?;
                let replace_all = edit.get("replaceAll").and_then(|value| value.as_bool());
                Some((old_text.to_string(), new_text.to_string(), replace_all))
            })
            .collect();
    }
    match (
        params.get("oldText").and_then(|value| value.as_str()),
        params.get("newText").and_then(|value| value.as_str()),
    ) {
        (Some(old_text), Some(new_text)) => vec![(
            old_text.to_string(),
            new_text.to_string(),
            params.get("replaceAll").and_then(|value| value.as_bool()),
        )],
        _ => Vec::new(),
    }
}

#[async_trait]
impl AgentTool for HostStateTool {
    fn schema(&self) -> &Tool {
        self.inner.schema()
    }

    fn label(&self) -> &str {
        self.inner.label()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.inner.execution_mode()
    }

    /// Keep the inner tool's argument folding (`edit` turns legacy
    /// `oldText`/`newText` into `edits`), so a forwarded call sees the same
    /// normalized arguments it would have seen on disk.
    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        self.inner.prepare_arguments(args)
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let Some((path, fields)) = self.forward(&params)? else {
            return self
                .inner
                .execute(tool_call_id, params, signal, on_update)
                .await;
        };
        self.bridge
            .call(tool_call_id, self.tool, path, fields, signal)
            .await
    }
}

/// Load `--host-state`. A missing flag is the disabled bridge.
///
/// A rejection is fatal at startup: the model would otherwise see a prefix that
/// never matches, or a feature the host cannot answer. `--mode rpc` only.
pub fn load(args: &Args, cwd: &Path) -> Result<HostStateBridge, String> {
    let Some(prefix) = args.host_state.as_deref() else {
        return Ok(HostStateBridge::disabled());
    };
    if args.mode != Mode::Rpc {
        return Err("--host-state requires --mode rpc".to_string());
    }
    HostStateBridge::new(cwd, prefix.to_string())
}

/// Split a path into `(root, segments)` with `.`/`..` resolved lexically:
/// `\` and `/` are both separators, `..` pops the last segment (and is clamped
/// at the root), and empty/`.` segments are dropped. Nothing on disk is read.
fn normalize_lexical(raw: &str) -> (String, Vec<String>) {
    let unified = raw.replace('\\', "/");
    let root = path_root(&unified);
    let rest = if root.is_empty() {
        unified.as_str()
    } else {
        // Drop the root marker itself (`/` or `C:/`); segments follow it.
        unified
            .get(root.len()..)
            .unwrap_or(unified.trim_start_matches('/'))
    };
    let mut segments: Vec<String> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other.to_string()),
        }
    }
    (root, segments)
}

/// The root marker of an absolute path: `/` (POSIX or UNC), `c:/` (Windows
/// drive), or empty for a relative path.
fn path_root(unified: &str) -> String {
    if unified.starts_with('/') {
        return "/".to_string();
    }
    let bytes = unified.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive: String = unified[..1].to_lowercase();
        return format!("{drive}:/");
    }
    String::new()
}

/// Resolve a call path against the working directory: absolute paths are taken
/// as they are, relative ones are joined first.
fn resolve_lexical(cwd: &str, raw: &str) -> (String, Vec<String>) {
    // The tools strip a leading `@` (`--files`-style mention); a state path
    // written that way must land on the same target.
    let raw = raw.strip_prefix('@').unwrap_or(raw);
    let unified = raw.replace('\\', "/");
    if path_root(&unified).is_empty() {
        normalize_lexical(&format!("{cwd}/{unified}"))
    } else {
        normalize_lexical(&unified)
    }
}

/// Segment equality: Windows and macOS filesystems are case-insensitive, so the
/// match has to be too (`.MU/state` must not fall through to the disk).
fn segment_eq(left: &str, right: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// Normalize a prefix-relative path for `waitPaths` comparison.
fn normalize_relative(raw: &str) -> String {
    normalize_lexical(raw).1.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(prefix: &str) -> Args {
        Args {
            mode: Mode::Rpc,
            host_state: Some(prefix.to_string()),
            ..Default::default()
        }
    }

    fn bridge(cwd: &Path, prefix: &str) -> HostStateBridge {
        load(&args(prefix), cwd).unwrap()
    }

    #[test]
    fn rejects_the_flag_outside_rpc_mode() {
        let directory = tempfile::tempdir().unwrap();
        let mut parsed = args(".mu/state");
        assert!(load(&parsed, directory.path()).is_ok());
        for mode in [Mode::Text, Mode::Json] {
            parsed.mode = mode;
            let error = load(&parsed, directory.path()).unwrap_err();
            assert!(error.contains("--mode rpc"), "{error}");
        }
    }

    #[test]
    fn rejects_prefixes_outside_the_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        for prefix in [".", "", ".mu/../..", "../elsewhere"] {
            let error = load(&args(prefix), directory.path()).unwrap_err();
            assert!(
                error.contains("working directory"),
                "{prefix:?} accepted: {error}"
            );
        }
        // A sibling that only shares a textual prefix is not inside either.
        assert!(load(&args("./.mu/state"), directory.path()).is_ok());
    }

    #[test]
    fn no_flag_is_the_disabled_bridge() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = load(&Args::default(), directory.path()).unwrap();
        assert!(!bridge.enabled());
        assert_eq!(bridge.prefix(), None);
        assert_eq!(bridge.advertisement(), None);
        assert!(bridge.state_path(".mu/state/x").is_none());
        assert!(bridge.take_receiver().is_none());
        assert!(bridge
            .respond("r1", "text".to_string(), false)
            .unwrap_err()
            .contains("not enabled"));
        assert!(bridge
            .configure(HostStateConfigCommand::default())
            .unwrap_err()
            .contains("not enabled"));
    }

    #[test]
    fn matches_paths_inside_the_prefix_only() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let bridge = bridge(cwd, ".mu/state");
        let absolute = cwd.join(".mu/state/ask/topic.md");
        let absolute = absolute.to_string_lossy().into_owned();

        for (raw, expected) in [
            (".mu/state/ask/topic.md", Some("ask/topic.md")),
            ("./.mu/state/ask/topic.md", Some("ask/topic.md")),
            (".mu/state//ask/./topic.md", Some("ask/topic.md")),
            (".mu\\state\\ask\\topic.md", Some("ask/topic.md")),
            ("./.mu\\state/ask\\topic.md", Some("ask/topic.md")),
            (".mu/state/plan.md", Some("plan.md")),
            (".mu/state", Some("")),
            (".mu/state/", Some("")),
            (".mu/state/sub/../plan.md", Some("plan.md")),
            ("@.mu/state/plan.md", Some("plan.md")),
            (absolute.as_str(), Some("ask/topic.md")),
            // Outside the prefix.
            (".mu/state-evil/x.md", None),
            (".mu/statex/a.md", None),
            (".mu/other/a.md", None),
            (".mu", None),
            ("plan.md", None),
            ("other/plan.md", None),
            (".mu/state/../other/a.md", None),
            (".mu/state/../../escape.md", None),
            ("..", None),
            ("/etc/passwd", None),
            ("", None),
        ] {
            assert_eq!(bridge.state_path(raw).as_deref(), expected, "path {raw:?}");
        }
    }

    #[test]
    fn matches_an_absolute_prefix_and_a_parent_cwd() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let absolute = cwd.join(".mu/state");
        let bridge = bridge(cwd, &absolute.to_string_lossy());
        assert_eq!(
            bridge.state_path("./.mu/state/plan.md").as_deref(),
            Some("plan.md")
        );
        assert_eq!(
            bridge
                .state_path(&cwd.join(".mu/state/plan.md").to_string_lossy())
                .as_deref(),
            Some("plan.md")
        );
        assert_eq!(
            bridge.state_path("sub/../.mu/state/a.md").as_deref(),
            Some("a.md")
        );
    }

    #[test]
    fn wrapping_leaves_other_tools_alone() {
        struct Probe {
            schema: Tool,
        }
        #[async_trait]
        impl AgentTool for Probe {
            fn schema(&self) -> &Tool {
                &self.schema
            }
            fn label(&self) -> &str {
                &self.schema.name
            }
            async fn execute(
                &self,
                _tool_call_id: &str,
                _params: serde_json::Value,
                _signal: CancellationToken,
                _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
            ) -> Result<AgentToolResult, AgentError> {
                Ok(AgentToolResult {
                    content: vec![TextContentOrImage::text("disk")],
                    details: serde_json::Value::Null,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let probe = |name: &str| {
            HarnessTool::new(Arc::new(Probe {
                schema: Tool {
                    name: name.to_string(),
                    description: format!("{name} description"),
                    parameters: rpi_ai::types::Schema::new(serde_json::json!({"type": "object"})),
                    constrained_sampling: None,
                },
            }))
        };
        // Disabled: everything keeps its identity, so an unset flag cannot
        // change what the model or the disk sees.
        let mut tools = vec![probe("read"), probe("ls"), probe("bash")];
        let before: Vec<_> = tools.iter().map(|tool| Arc::as_ptr(&tool.tool)).collect();
        HostStateBridge::disabled().wrap(&mut tools);
        let after: Vec<_> = tools.iter().map(|tool| Arc::as_ptr(&tool.tool)).collect();
        assert_eq!(before, after);

        // Enabled: exactly read/write/edit/ls are wrapped, schema untouched.
        let mut tools = vec![
            probe("read"),
            probe("write"),
            probe("edit"),
            probe("ls"),
            probe("bash"),
        ];
        let bash = Arc::as_ptr(&tools[4].tool);
        bridge(directory.path(), ".mu/state").wrap(&mut tools);
        let names: Vec<_> = tools
            .iter()
            .map(|tool| tool.tool.schema().name.clone())
            .collect();
        assert_eq!(names, ["read", "write", "edit", "ls", "bash"]);
        assert_eq!(tools[0].tool.schema().description, "read description");
        assert_eq!(tools[0].tool.label(), "read");
        assert_eq!(Arc::as_ptr(&tools[4].tool), bash);
    }

    #[test]
    fn configures_timeouts_and_wait_paths() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = bridge(directory.path(), ".mu/state");
        assert_eq!(
            bridge.advertisement(),
            Some(HostStateAdvertisement {
                prefix: ".mu/state".to_string(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                wait_timeout_ms: DEFAULT_TIMEOUT_MS,
            })
        );
        bridge
            .configure(HostStateConfigCommand {
                timeout_ms: Some(50),
                wait_timeout_ms: Some(90),
                wait_paths: Some(vec!["/ask/".to_string(), "plan.md".to_string()]),
            })
            .unwrap();
        let state = bridge.state.as_ref().unwrap();
        assert_eq!(state.timeout_for("other.md").as_millis(), 50);
        assert_eq!(state.timeout_for("ask/topic.md").as_millis(), 90);
        assert_eq!(state.timeout_for("ask").as_millis(), 90);
        assert_eq!(state.timeout_for("plan.md").as_millis(), 90);
        assert_eq!(state.timeout_for("plan.md.bak").as_millis(), 50);
        assert_eq!(state.timeout_for("asks/x.md").as_millis(), 50);
        // A zero timeout would hang nothing but break the contract.
        assert!(bridge
            .configure(HostStateConfigCommand {
                timeout_ms: Some(0),
                ..Default::default()
            })
            .unwrap_err()
            .contains("at least 1"));
    }

    /// An inner tool that stands in for a built-in: it reports `disk`, so any
    /// test that sees `host` instead proves the host answered.
    struct Probe {
        schema: Tool,
    }

    #[async_trait]
    impl AgentTool for Probe {
        fn schema(&self) -> &Tool {
            &self.schema
        }
        fn label(&self) -> &str {
            &self.schema.name
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, AgentError> {
            Ok(AgentToolResult {
                content: vec![TextContentOrImage::text("disk")],
                details: serde_json::Value::Null,
                usage: None,
                added_tool_names: Vec::new(),
                terminate: false,
            })
        }
    }

    fn probe(name: &str) -> HarnessTool {
        HarnessTool::new(Arc::new(Probe {
            schema: Tool {
                name: name.to_string(),
                description: format!("{name} description"),
                parameters: rpi_ai::types::Schema::new(serde_json::json!({"type": "object"})),
                constrained_sampling: None,
            },
        }))
    }

    /// One wrapped tool in a temp working directory with the bridge's call
    /// stream taken (the RPC relay's role).
    fn wrapped(
        name: &str,
        prefix: &str,
    ) -> (
        tempfile::TempDir,
        HostStateBridge,
        Arc<dyn AgentTool>,
        mpsc::UnboundedReceiver<Event>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let bridge = bridge(directory.path(), prefix);
        let mut tools = vec![probe(name)];
        bridge.wrap(&mut tools);
        let tool = Arc::clone(&tools[0].tool);
        let receiver = bridge.take_receiver().unwrap();
        (directory, bridge, tool, receiver)
    }

    async fn run(
        tool: Arc<dyn AgentTool>,
        params: serde_json::Value,
    ) -> Result<AgentToolResult, AgentError> {
        tool.execute("call-1", params, CancellationToken::new(), Arc::new(|_| {}))
            .await
    }

    #[tokio::test]
    async fn a_state_write_round_trips_through_the_host() {
        let (_directory, bridge, tool, mut calls) = wrapped("write", ".mu/state");
        let call = tokio::spawn(run(
            tool,
            serde_json::json!({"path": ".mu/state/ask/topic.md", "content": "line"}),
        ));
        let Some(Event::Call {
            request_id,
            tool_call_id,
            tool,
            path,
            fields,
        }) = calls.recv().await
        else {
            panic!("expected a host_state_call");
        };
        assert_eq!(tool, StateTool::Write);
        assert_eq!(tool_call_id, "call-1");
        assert_eq!(path, "ask/topic.md");
        assert_eq!(fields.content.as_deref(), Some("line"));
        let line = Event::Call {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool,
            path: path.clone(),
            fields: fields.clone(),
        }
        .to_json();
        assert_eq!(
            line,
            serde_json::json!({
                "type": "host_state_call",
                "requestId": request_id,
                "toolCallId": "call-1",
                "tool": "write",
                "path": "ask/topic.md",
                "content": "line",
            })
        );
        bridge
            .respond(&request_id, "Asked topic.".to_string(), false)
            .unwrap();
        let result = call.await.unwrap().unwrap();
        assert_eq!(
            result.content,
            vec![TextContentOrImage::text("Asked topic.")]
        );
        assert_eq!(result.details, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_state_path_outside_the_prefix_runs_the_inner_tool() {
        let (_directory, bridge, tool, mut calls) = wrapped("read", ".mu/state");
        for path in ["README.md", ".mu/state-evil/x.md", "src/lib.rs"] {
            let result = run(Arc::clone(&tool), serde_json::json!({"path": path}))
                .await
                .unwrap();
            assert_eq!(result.content, vec![TextContentOrImage::text("disk")]);
        }
        // An `ls` without a path lists the working directory, which is outside.
        let (_directory, _bridge, tool, mut calls2) = wrapped("ls", ".mu/state");
        let result = run(tool, serde_json::json!({})).await.unwrap();
        assert_eq!(result.content, vec![TextContentOrImage::text("disk")]);
        assert!(calls.try_recv().is_err());
        assert!(calls2.try_recv().is_err());
        let _ = bridge;
    }

    #[tokio::test]
    async fn forwarded_arguments_keep_read_and_edit_parameters() {
        // read: offset/limit
        let (_directory, bridge, tool, mut calls) = wrapped("read", ".mu/state");
        let call = tokio::spawn(run(
            tool,
            serde_json::json!({"path": ".mu/state/plan.md", "offset": 2, "limit": 5}),
        ));
        let Some(Event::Call {
            request_id, fields, ..
        }) = calls.recv().await
        else {
            panic!("expected a call");
        };
        assert_eq!(fields.offset, Some(2));
        assert_eq!(fields.limit, Some(5));
        assert!(fields.content.is_none() && fields.old_text.is_none());
        bridge
            .respond(&request_id, "plan".to_string(), false)
            .unwrap();
        assert_eq!(call.await.unwrap().unwrap().content.len(), 1);

        // edit: one replacement, forwarded verbatim.
        let (_directory, bridge, tool, mut calls) = wrapped("edit", ".mu/state");
        let call = tokio::spawn(run(
            tool,
            serde_json::json!({
                "path": ".mu/state/plan.md",
                "edits": [{"oldText": "- [ ] a", "newText": "- [x] a", "replaceAll": true}],
            }),
        ));
        let Some(Event::Call {
            request_id, fields, ..
        }) = calls.recv().await
        else {
            panic!("expected a call");
        };
        assert_eq!(fields.old_text.as_deref(), Some("- [ ] a"));
        assert_eq!(fields.new_text.as_deref(), Some("- [x] a"));
        assert_eq!(fields.replace_all, Some(true));
        bridge
            .respond(&request_id, "edited".to_string(), false)
            .unwrap();
        assert_eq!(
            call.await.unwrap().unwrap().content,
            vec![TextContentOrImage::text("edited")]
        );

        // edit: the legacy top-level shape an unprepared caller may still send.
        let (_directory, bridge, tool, mut calls) = wrapped("edit", ".mu/state");
        let call = tokio::spawn(run(
            tool,
            serde_json::json!({"path": ".mu/state/plan.md", "oldText": "a", "newText": "b"}),
        ));
        let Some(Event::Call {
            request_id, fields, ..
        }) = calls.recv().await
        else {
            panic!("expected a call");
        };
        assert_eq!(fields.old_text.as_deref(), Some("a"));
        assert_eq!(fields.new_text.as_deref(), Some("b"));
        assert_eq!(fields.replace_all, None);
        bridge
            .respond(&request_id, "edited".to_string(), false)
            .unwrap();
        assert_eq!(call.await.unwrap().unwrap().content.len(), 1);
    }

    #[tokio::test]
    async fn several_replacements_on_one_state_path_ask_for_separate_calls() {
        let (_directory, _bridge, tool, mut calls) = wrapped("edit", ".mu/state");
        let error = run(
            tool,
            serde_json::json!({
                "path": ".mu/state/plan.md",
                "edits": [
                    {"oldText": "a", "newText": "b"},
                    {"oldText": "c", "newText": "d"},
                ],
            }),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("one replacement"), "{error}");
        assert!(calls.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_error_answer_becomes_a_failed_call_with_the_host_text() {
        let (_directory, bridge, tool, mut calls) = wrapped("write", ".mu/state");
        let call = tokio::spawn(run(
            tool,
            serde_json::json!({"path": ".mu/state/ask/x.md", "content": "bad"}),
        ));
        let Some(Event::Call { request_id, .. }) = calls.recv().await else {
            panic!("expected a call");
        };
        bridge
            .respond(&request_id, "ask cards need a question".to_string(), true)
            .unwrap();
        let error = call.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("ask cards need a question"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_abort_tells_the_host_and_rejects_a_late_result() {
        let (_directory, bridge, tool, mut calls) = wrapped("read", ".mu/state");
        let signal = CancellationToken::new();
        let call = tokio::spawn({
            let signal = signal.clone();
            async move {
                tool.execute(
                    "call-1",
                    serde_json::json!({"path": ".mu/state/ask/x.md"}),
                    signal,
                    Arc::new(|_| {}),
                )
                .await
            }
        });
        let Some(Event::Call { request_id, .. }) = calls.recv().await else {
            panic!("expected a call");
        };
        signal.cancel();
        assert!(call.await.unwrap().is_err());
        assert_eq!(
            calls.recv().await.map(|event| event.to_json()),
            Some(serde_json::json!({
                "type": "host_state_cancelled",
                "requestId": request_id,
            }))
        );
        assert!(bridge
            .respond(&request_id, "late".to_string(), false)
            .unwrap_err()
            .contains("unknown host state request"));
    }

    #[tokio::test]
    async fn an_unanswered_call_times_out_and_retires_the_request() {
        let (_directory, bridge, tool, mut calls) = wrapped("read", ".mu/state");
        bridge
            .configure(HostStateConfigCommand {
                timeout_ms: Some(20),
                ..Default::default()
            })
            .unwrap();
        let error = run(tool, serde_json::json!({"path": ".mu/state/ask/x.md"}))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "tool error: host state read `ask/x.md` got no answer within 20 ms"
        );
        let Some(Event::Call { request_id, .. }) = calls.recv().await else {
            panic!("expected a call");
        };
        // The host is told to drop the call, and a late result is unknown.
        assert_eq!(
            calls.recv().await.map(|event| event.to_json()),
            Some(serde_json::json!({
                "type": "host_state_cancelled",
                "requestId": request_id,
            }))
        );
        assert!(bridge
            .respond(&request_id, "late".to_string(), false)
            .is_err());
    }

    #[tokio::test]
    async fn a_disconnect_fails_the_pending_call() {
        let (_directory, bridge, tool, mut calls) = wrapped("read", ".mu/state");
        let call = tokio::spawn(run(tool, serde_json::json!({"path": ".mu/state/plan.md"})));
        assert!(calls.recv().await.is_some());
        bridge.disconnect();
        let error = call.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("host disconnected"), "{error}");
    }
}
