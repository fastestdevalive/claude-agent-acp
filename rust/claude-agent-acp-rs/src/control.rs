//! Control channel: the **single** writer to the child's stdin (Decision D6,
//! item 4.1).
//!
//! Every frame that goes to `claude` — a user message, a control_request, a
//! control_response, a control_cancel_request — passes through one writer task
//! that owns the child's stdin. This models the adapter's single serialized
//! control channel (R5) explicitly and guarantees INV-8: exactly one writer,
//! so 100 concurrent sends produce 100 whole, non-interleaved lines.
//!
//! The module also owns the two correlation maps:
//!
//! - **`pending`**: outbound `control_request`s await their `control_response`,
//!   keyed by the 13-char random `request_id` (INV-9). A response for an
//!   unknown id is dropped, never a panic.
//! - **`inflight`**: inbound `control_request`s are handled in spawned tasks
//!   (D14); a `control_cancel_request` aborts the matching task and writes no
//!   response (INV-10).
//!
//! Inbound frames that are *not* control traffic (user/assistant/result/system)
//! are not consumed here: the caller feeds this module only the control-traffic
//! stream (the phase-5 dispatch routes it). `keep_alive` frames are consumed
//! silently (INV-7 / R6) — the caller sees nothing.
//!
//! stdin is closed only when the last [`Control`] handle is dropped (session
//! teardown, D7), never after a turn.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// A pinned boxed future returning an optional `Value` (used by the inbound
/// request handler). Defined without the `futures` crate to avoid a new
/// dependency.
type BoxFuture = Pin<Box<dyn Future<Output = Option<Value>> + Send + 'static>>;

/// Handles an inbound `control_request` (from `claude` to us).
///
/// Receives the full inbound `control_request` frame and returns the full
/// `control_response` frame to write back — or `None` to write **nothing**
/// (`sdk.mjs` `suppressControlResponse`: the `Z4` machine-only subtypes and a
/// `request_user_dialog` with no handler). Runs in a spawned task (D14) so the
/// control task never awaits it inline.
pub type RequestHandler = Box<dyn Fn(Value) -> BoxFuture + Send + Sync>;

/// Inbound subtypes the SDK never answers ("for the machine serving this
/// session's tools — leaving it unanswered", `sdk.mjs` `Z4`). They are
/// suppressed even when a handler is registered.
pub const SUPPRESSED_SUBTYPES: [&str; 4] = [
    "remote_tool_call",
    "remote_plumbing_call",
    "remote_tools_probe",
    "remote_tools_reannounce",
];

/// Options controlling the control channel.
#[derive(Default)]
pub struct ControlOptions {
    /// Handler for inbound `control_request` frames. When `None` the module
    /// applies the SDK's own defaults (`sdk.mjs` `processControlRequest`):
    /// silence for `request_user_dialog` and the [`SUPPRESSED_SUBTYPES`],
    /// `{action:"decline"}` for `elicitation`, and an error response carrying
    /// an `error` string for every other subtype (4.6 / 4.T9).
    pub on_request: Option<Arc<RequestHandler>>,
}

/// Errors produced by the control channel.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("control channel closed")]
    Closed,
    #[error("failed to write a frame to child stdin: {0}")]
    Stdin(#[from] std::io::Error),
    #[error("failed to serialise a frame: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The CLI answered an outbound `control_request` with an `error` verdict.
    /// The string is the response's `error` field (`sdk.mjs` `request()`
    /// rejects with `Error(response.error)`); it may be empty when the CLI
    /// omitted the field.
    #[error("{0}")]
    Remote(String),
}

/// What the control task is asked to do by the session actor.
enum Outbound {
    /// Write a full `user` frame to stdin.
    User(Value),
    /// Write a `control_request` frame (adding a fresh `request_id`) and resolve
    /// `reply` with the correlated `control_response` frame.
    Request {
        payload: Value,
        reply: oneshot::Sender<Result<Value, ControlError>>,
    },
    /// Write a full `control_response` frame and forget the matching in-flight
    /// inbound handler.
    Response(Value),
    /// Write a full `control_cancel_request` frame.
    Cancel(Value),
    /// Replay a `pending_permission_requests` entry through the inbound handler
    /// (4.5).
    Replay(Value),
    /// Resolve the sender once every frame enqueued before this marker was written
    /// to the child's stdin ([`Control::flushed`]).
    Barrier(oneshot::Sender<()>),
    /// Mark an inbound handler finished so its `request_id` is no longer
    /// in-flight. Sent after the handler wrote a response **or** chose to stay
    /// silent (`sdk.mjs` `handleControlRequest`'s `finally` deletes the
    /// cancel controller on both paths).
    Finished(String),
    /// Stop answering inbound requests (`sdk.mjs` `cleanupPerformed`): frames
    /// that still arrive are skipped, never answered. Outbound frames keep
    /// flowing until the handle is dropped.
    Close,
    /// Continue on another child's stdin (a clear-context restart): the old
    /// writer is dropped, which closes the old child's stdin, every request the
    /// old child never answered fails, its in-flight inbound handlers are
    /// aborted and inbound requests are answered again.
    Rebind(Box<dyn AsyncWrite + Unpin + Send>),
}

/// A handle to the control channel. Cloneable: the session actor and any
/// spawned task can each send frames through it. The child's stdin is closed
/// when the last handle is dropped.
#[derive(Clone)]
pub struct Control {
    tx: mpsc::UnboundedSender<Outbound>,
}

/// The parsed `initialize` control_response (4.4).
#[derive(Debug, Clone, Default)]
pub struct InitializeInfo {
    /// `commands` from the initialize response.
    pub commands: Vec<Value>,
    /// `models` from the initialize response.
    pub models: Vec<Value>,
    /// `account` from the initialize response, if present.
    pub account: Option<Value>,
    /// `pending_permission_requests` from the initialize response (4.5).
    pub pending_permission_requests: Vec<Value>,
    /// `agents` from the initialize response (`supportedAgents()`; feeds
    /// `session_config::discover_custom_agents`, A-04).
    pub agents: Option<Value>,
    /// `fast_mode_state` (`on`/`off`/`cooldown`), seeds the Fast mode option.
    pub fast_mode_state: Option<String>,
    /// `fast_mode_disabled_reason`, normalized by the Fast mode option.
    pub fast_mode_disabled_reason: Option<String>,
}

/// Options used to build the `initialize` control_request payload (4.4).
///
/// Fields are optional and omitted from the wire frame when unset, mirroring
/// the adapter which drops `undefined` fields. The captured
/// `porting/fixtures/initialize.json` shows the exact subset present when none
/// of the optional fields are configured.
#[derive(Debug, Clone, Default)]
pub struct InitializeOptions {
    /// `systemPrompt`.
    pub system_prompt: Option<Value>,
    /// `appendSystemPrompt` (client `systemPrompt` append mode and AIR
    /// `customInstructions`, rust-air-gaps Decision 10).
    pub append_system_prompt: Option<String>,
    /// `agents`.
    pub agents: Option<Value>,
    /// `hooks` (the `initHooksPayload`).
    pub hooks: Option<Value>,
    /// `promptSuggestions` (2.7 / E-14): emitted only when `Some`; the live
    /// agent leaves it `None`.
    pub prompt_suggestions: Option<bool>,
    /// `skills`.
    pub skills: Option<Value>,
    /// `toolAliases`.
    pub tool_aliases: Option<Value>,
    /// `toolConfig`.
    pub tool_config: Option<Value>,
    /// `forwardSubagentText`.
    pub forward_subagent_text: bool,
    /// `perTaskStopAffordance` (O-84): emitted only when `Some`.
    pub per_task_stop_affordance: Option<bool>,
    /// `agentProgressSummaries` (O-153): emitted only when `Some`.
    pub agent_progress_summaries: Option<bool>,
    /// `planModeInstructions` (O-154): emitted only when `Some`.
    pub plan_mode_instructions: Option<String>,
    /// `enableFileCheckpointing` (O-136). It is NOT part of the initialize
    /// request — the SDK turns it into the
    /// `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` env var — so it rides here
    /// only to reach `agent.rs`'s spawn options.
    pub enable_file_checkpointing: bool,
}

impl InitializeOptions {
    /// Build the `request` object (the `{subtype, ...}` payload) of the
    /// `initialize` control_request.
    pub fn build_request(&self) -> Value {
        let mut request = serde_json::Map::new();
        request.insert(
            "subtype".to_string(),
            Value::String("initialize".to_string()),
        );
        if let Some(hooks) = &self.hooks {
            request.insert("hooks".to_string(), hooks.clone());
        }
        if let Some(prompt_suggestions) = &self.prompt_suggestions {
            request.insert(
                "promptSuggestions".to_string(),
                Value::Bool(*prompt_suggestions),
            );
        }
        if let Some(system_prompt) = &self.system_prompt {
            request.insert("systemPrompt".to_string(), system_prompt.clone());
        }
        if let Some(append) = &self.append_system_prompt {
            request.insert(
                "appendSystemPrompt".to_string(),
                Value::String(append.clone()),
            );
        }
        if let Some(instructions) = &self.plan_mode_instructions {
            request.insert(
                "planModeInstructions".to_string(),
                Value::String(instructions.clone()),
            );
        }
        if let Some(agents) = &self.agents {
            request.insert("agents".to_string(), agents.clone());
        }
        if let Some(skills) = &self.skills {
            request.insert("skills".to_string(), skills.clone());
        }
        if let Some(tool_aliases) = &self.tool_aliases {
            request.insert("toolAliases".to_string(), tool_aliases.clone());
        }
        if let Some(tool_config) = &self.tool_config {
            request.insert("toolConfig".to_string(), tool_config.clone());
        }
        // `forwardSubagentText` is always sent (even when false), matching the
        // captured fixture.
        request.insert(
            "forwardSubagentText".to_string(),
            Value::Bool(self.forward_subagent_text),
        );
        if let Some(summaries) = &self.agent_progress_summaries {
            request.insert(
                "agentProgressSummaries".to_string(),
                Value::Bool(*summaries),
            );
        }
        if let Some(per_task_stop) = &self.per_task_stop_affordance {
            request.insert(
                "perTaskStopAffordance".to_string(),
                Value::Bool(*per_task_stop),
            );
        }
        Value::Object(request)
    }
}

impl Control {
    /// Spawn the single-writer control task.
    ///
    /// `stdin` is the child's stdin writer (owned exclusively by this task —
    /// D6). `inbound` is the stream of *control-traffic* frames parsed from the
    /// child's stdout (keep_alive, control_response, control_request,
    /// control_cancel_request); the phase-5 dispatch routes those here and
    /// session frames to the session actor.
    pub fn spawn(
        stdin: impl AsyncWrite + Unpin + Send + 'static,
        inbound: mpsc::UnboundedReceiver<Value>,
        opts: ControlOptions,
    ) -> Control {
        let (tx, rx) = mpsc::unbounded_channel::<Outbound>();
        let tx_for_tasks = tx.clone();
        tokio::spawn(control_task(
            Box::new(stdin),
            inbound,
            rx,
            tx_for_tasks,
            opts,
        ));
        Control { tx }
    }

    /// Write a `user` message frame to the child's stdin.
    pub async fn send_user(&self, frame: Value) -> Result<(), ControlError> {
        self.tx
            .send(Outbound::User(frame))
            .map_err(|_| ControlError::Closed)
    }

    /// Write a `control_request` and await its correlated `control_response`.
    ///
    /// `payload` is the `request` object (`{subtype, ...}`). A fresh 13-char
    /// `request_id` is added; the returned frame is the full `control_response`.
    pub async fn send_request(&self, payload: Value) -> Result<Value, ControlError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Outbound::Request {
                payload,
                reply: reply_tx,
            })
            .map_err(|_| ControlError::Closed)?;
        reply_rx.await.map_err(|_| ControlError::Closed)?
    }

    /// Enqueue a `control_request` on the single unbounded writer NOW and hand back
    /// the receiver of its correlated `control_response`, so a caller can write
    /// further requests in call order before awaiting the reply (Node's
    /// `query.interrupt()` writes synchronously and is awaited after
    /// `stopBackgroundWorkflows`).
    pub fn enqueue_request(
        &self,
        payload: Value,
    ) -> Result<oneshot::Receiver<Result<Value, ControlError>>, ControlError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Outbound::Request {
                payload,
                reply: reply_tx,
            })
            .map_err(|_| ControlError::Closed)?;
        Ok(reply_rx)
    }

    /// Enqueue a `control_request` on the single unbounded writer WITHOUT
    /// awaiting its response (2.13 / E-8). The request is given a fresh
    /// `request_id`, and like every `Outbound::Request` the writer DOES register
    /// a `pending` correlation entry (`control.rs:373`) — but its receiver is
    /// dropped immediately (`oneshot::channel().0`), so the actor never awaits
    /// the client round-trip inline (INV-27 holds). Used for the per-uuid
    /// `cancel_async_message` frames sent before `interrupt` while a cancel
    /// settles.
    ///
    /// Boundedness of the correlation entry (review follow-up 3.17-D): the entry
    /// is removed the moment a `control_response` bearing the request_id arrives
    /// (`route_inbound`, `pending.remove(id)`, with the dropped-receiver
    /// `send` being an empty no-op); if the child never answers, the entry
    /// simply lives in `pending` for the control task's lifetime and is dropped
    /// with the map when the task ends (writer failure / all `Control` handles
    /// dropped / inbound stream EOF). The set is small (one per abandoned uuid
    /// from a cancel) and always bounded by the session/child lifetime — it
    /// cannot grow without bound.
    pub fn send_request_detached(&self, payload: Value) -> Result<(), ControlError> {
        self.tx
            .send(Outbound::Request {
                payload,
                reply: oneshot::channel().0,
            })
            .map_err(|_| ControlError::Closed)
    }

    /// Write a full `control_response` frame to the child's stdin.
    pub async fn send_response(&self, frame: Value) -> Result<(), ControlError> {
        self.tx
            .send(Outbound::Response(frame))
            .map_err(|_| ControlError::Closed)
    }

    /// Write a full `control_cancel_request` frame to the child's stdin.
    pub async fn send_cancel(&self, frame: Value) -> Result<(), ControlError> {
        self.tx
            .send(Outbound::Cancel(frame))
            .map_err(|_| ControlError::Closed)
    }

    /// Wait until every frame enqueued so far has been written to the child's
    /// stdin. Teardown uses it to be sure the `interrupt` is on the wire before the
    /// child is killed, without waiting for the CLI's reply (Node's
    /// `awaitInterrupt: false`).
    pub async fn flushed(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(Outbound::Barrier(ack_tx)).is_ok() {
            let _ = ack_rx.await;
        }
    }

    /// Stop answering inbound `control_request`s (`sdk.mjs`
    /// `cleanupPerformed`). Queued inbound frames are drained without a
    /// response; outbound frames (the teardown `interrupt`) still flow.
    pub fn close(&self) -> Result<(), ControlError> {
        self.tx
            .send(Outbound::Close)
            .map_err(|_| ControlError::Closed)
    }

    /// Move every clone of this handle to another child's stdin. The previous
    /// child's stdin is closed, what it never answered fails with
    /// [`ControlError::Closed`], and inbound requests are answered again. The
    /// inbound stream stays the one the session actor feeds, so it carries the
    /// new child's control traffic once the actor reads that child.
    pub fn rebind(
        &self,
        stdin: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Result<(), ControlError> {
        self.tx
            .send(Outbound::Rebind(Box::new(stdin)))
            .map_err(|_| ControlError::Closed)
    }

    /// Perform the `initialize` handshake (4.4): send the `initialize`
    /// control_request built from `opts` and parse the correlated response.
    pub async fn initialize(
        &self,
        opts: &InitializeOptions,
    ) -> Result<InitializeInfo, ControlError> {
        let frame = self.send_request(opts.build_request()).await?;
        Ok(parse_initialize_response(&frame))
    }

    /// `query.mcpServerStatus()` (rust-air-gaps A-23): `{subtype:"mcp_status"}`,
    /// whose reply payload carries `mcpServers` (`sdk.mjs` `mcpServerStatus`).
    pub async fn mcp_status(&self) -> Result<Value, ControlError> {
        self.send_request(json!({ "subtype": "mcp_status" })).await
    }

    /// `query.reconnectMcpServer(serverName)` (rust-air-gaps A-24):
    /// `{subtype:"mcp_reconnect", serverName}`.
    pub async fn mcp_reconnect(&self, server_name: &str) -> Result<(), ControlError> {
        self.send_request(json!({
            "subtype": "mcp_reconnect",
            "serverName": server_name,
        }))
        .await
        .map(|_| ())
    }

    /// `query.toggleMcpServer(serverName, enabled)` (rust-air-gaps A-24):
    /// `{subtype:"mcp_toggle", serverName, enabled}`.
    pub async fn mcp_toggle(&self, server_name: &str, enabled: bool) -> Result<(), ControlError> {
        self.send_request(json!({
            "subtype": "mcp_toggle",
            "serverName": server_name,
            "enabled": enabled,
        }))
        .await
        .map(|_| ())
    }

    /// Replay `pending_permission_requests` from an initialize response (4.5).
    ///
    /// Each entry is a full `control_request` frame (e.g. `can_use_tool`); it
    /// is routed through the same inbound handler as a freshly-arrived inbound
    /// request, in its own spawned task (D14).
    pub async fn replay_pending_permissions(&self, entries: &[Value]) -> Result<(), ControlError> {
        for entry in entries {
            self.tx
                .send(Outbound::Replay(entry.clone()))
                .map_err(|_| ControlError::Closed)?;
        }
        Ok(())
    }
}

/// The payload of a control reply: `frame.response.response`. The SDK's
/// `request()` resolves with `e.response` (the `{subtype, request_id, response}`
/// object), and each typed method reads `.response` of that.
fn reply_payload(frame: &Value) -> Option<&Value> {
    frame.get("response").and_then(|r| r.get("response"))
}

/// `rawMaxTokens` from a `get_context_usage` reply (A-21). `None` if absent.
pub fn raw_max_tokens(frame: &Value) -> Option<u64> {
    reply_payload(frame)?
        .get("rawMaxTokens")
        .and_then(Value::as_u64)
}

/// `still_queued` from an `interrupt` reply (B-33). `None` when the field is absent.
pub fn still_queued_of(frame: &Value) -> Option<Vec<String>> {
    reply_payload(frame)?
        .get("still_queued")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
}

/// Parse the `initialize` control_response frame for the fields the client
/// needs (4.4). Non-fatal: missing fields become empty.
pub fn parse_initialize_response(frame: &Value) -> InitializeInfo {
    let response = frame.get("response").and_then(|r| r.get("response"));
    let arr = |key: &str| -> Vec<Value> {
        response
            .and_then(|r| r.get(key))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    InitializeInfo {
        commands: arr("commands"),
        models: arr("models"),
        account: response.and_then(|r| r.get("account")).cloned(),
        pending_permission_requests: arr("pending_permission_requests"),
        agents: response.and_then(|r| r.get("agents")).cloned(),
        fast_mode_state: response
            .and_then(|r| r.get("fast_mode_state"))
            .and_then(Value::as_str)
            .map(String::from),
        fast_mode_disabled_reason: response
            .and_then(|r| r.get("fast_mode_disabled_reason"))
            .and_then(Value::as_str)
            .map(String::from),
    }
}

/// Generate a 13-char `[0-9a-z]` request id (the adapter uses
/// `Math.random().toString(36).substring(2, 15)` — 13 base36 chars). Seeded
/// from the monotonic-ish nanos plus a per-process counter so collisions within
/// a session are not observed.
fn request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x = nanos ^ ((counter as u128) << 40);
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(13);
    for _ in 0..13 {
        out.push(ALPHABET[(x % 36) as usize] as char);
        x /= 36;
    }
    out
}

/// Build a `control_response` frame echoing an inbound request's `request_id`
/// with a success payload (`sdk.mjs` `handleControlRequest`).
fn build_success_response(request_id: &str, payload: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": payload,
        }
    })
}

/// Build an `error` `control_response` carrying the `error` string the SDK
/// always writes (`sdk.mjs` `handleControlRequest`'s catch: `error: ot(r)`).
fn build_error_response(request_id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error,
        }
    })
}

/// The SDK's own response for an inbound subtype when no adapter callback is
/// registered (`sdk.mjs` `processControlRequest`). `None` means "write
/// nothing" (`suppressControlResponse`).
fn sdk_default_response(subtype: &str, request_id: &str) -> Option<Value> {
    match subtype {
        // No `onUserDialog` handler: log and stay silent, so a capable client
        // (or the worker's park deadline) settles it.
        "request_user_dialog" => None,
        // No `onElicitation` handler: a real success verdict.
        "elicitation" => Some(build_success_response(
            request_id,
            json!({"action": "decline"}),
        )),
        "can_use_tool" => Some(build_error_response(
            request_id,
            "canUseTool callback is not provided.",
        )),
        "hook_callback" => Some(build_error_response(
            request_id,
            "hookCallback callback is not provided.",
        )),
        other => Some(build_error_response(
            request_id,
            &format!("Unsupported control request subtype: {other}"),
        )),
    }
}

/// Serialise a frame into a newline-terminated line and write it to stdin.
async fn write_frame(
    stdin: &mut (impl AsyncWrite + Unpin),
    frame: &Value,
) -> Result<(), ControlError> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    Ok(())
}

/// The single-writer control task. Owns the child's stdin, the outbound-request
/// correlation map (`pending`) and the inbound-handler map (`inflight`).
async fn control_task(
    mut stdin: Box<dyn AsyncWrite + Unpin + Send>,
    mut inbound: mpsc::UnboundedReceiver<Value>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
    outbound_tx: mpsc::UnboundedSender<Outbound>,
    opts: ControlOptions,
) {
    let mut pending: HashMap<String, oneshot::Sender<Result<Value, ControlError>>> = HashMap::new();
    let mut inflight: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // `sdk.mjs` `cleanupPerformed`: once the session is closing, inbound
    // requests that still arrive are skipped rather than answered.
    let mut closed = false;

    loop {
        tokio::select! {
            maybe = outbound.recv() => {
                let Some(msg) = maybe else {
                    // All Control handles dropped: session teardown (D7).
                    // Close stdin and finish.
                    break;
                };
                match msg {
                    Outbound::User(frame) => {
                        if write_frame(&mut stdin, &frame).await.is_err() {
                            break;
                        }
                    }
                    Outbound::Request { payload, reply } => {
                        let id = request_id();
                        let frame = json!({
                            "request_id": id,
                            "type": "control_request",
                            "request": payload,
                        });
                        // Serialise the request_id into the map BEFORE writing so
                        // a response racing the write still resolves.
                        pending.insert(id.clone(), reply);
                        if write_frame(&mut stdin, &frame).await.is_err() {
                            break;
                        }
                    }
                    Outbound::Response(frame) => {
                        if let Some(id) = frame.pointer("/response/request_id").and_then(Value::as_str) {
                            inflight.remove(id);
                        }
                        if write_frame(&mut stdin, &frame).await.is_err() {
                            break;
                        }
                    }
                    Outbound::Cancel(frame) => {
                        if write_frame(&mut stdin, &frame).await.is_err() {
                            break;
                        }
                    }
                    Outbound::Replay(frame) => {
                        spawn_inbound_handler(frame, &mut inflight, &outbound_tx, &opts);
                    }
                    Outbound::Barrier(ack) => {
                        // Every earlier frame was written (`write_frame` awaited).
                        let _ = ack.send(());
                    }
                    Outbound::Finished(id) => {
                        inflight.remove(&id);
                    }
                    Outbound::Close => {
                        closed = true;
                    }
                    Outbound::Rebind(next) => {
                        stdin = next;
                        pending.clear();
                        for (_, task) in inflight.drain() {
                            task.abort();
                        }
                        closed = false;
                    }
                }
            }
            maybe = inbound.recv() => {
                let Some(frame) = maybe else {
                    // The inbound control stream closed (child stdout EOF). The
                    // child is gone; stop.
                    break;
                };
                route_inbound(frame, &mut pending, &mut inflight, &outbound_tx, &opts, closed);
            }
        }
    }
}

/// Route one inbound control-traffic frame (4.3).
fn route_inbound(
    frame: Value,
    pending: &mut HashMap<String, oneshot::Sender<Result<Value, ControlError>>>,
    inflight: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    outbound: &mpsc::UnboundedSender<Outbound>,
    opts: &ControlOptions,
    closed: bool,
) {
    let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
    match frame_type {
        // R6 / INV-7: keep_alive is consumed silently — the caller sees nothing.
        "keep_alive" => {}
        "control_response" => {
            if let Some(id) = frame
                .pointer("/response/request_id")
                .and_then(Value::as_str)
            {
                if let Some(reply) = pending.remove(id) {
                    // `sdk.mjs` `request()`: a non-success verdict rejects with
                    // `Error(response.error)`; only success resolves.
                    let subtype = frame
                        .pointer("/response/subtype")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if subtype == "success" {
                        let _ = reply.send(Ok(frame));
                    } else {
                        let error = frame
                            .pointer("/response/error")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let _ = reply.send(Err(ControlError::Remote(error)));
                    }
                }
                // Unknown request_id (INV-9 / 4.T3): dropped, no panic.
            }
        }
        "control_request" => {
            // `cleanupPerformed`: a request that arrives after close is
            // skipped, never answered.
            if !closed {
                spawn_inbound_handler(frame, inflight, outbound, opts);
            }
        }
        "control_cancel_request" => {
            if let Some(id) = frame.get("request_id").and_then(Value::as_str) {
                if let Some(task) = inflight.remove(id) {
                    task.abort();
                }
            }
        }
        // Anything else is not this module's concern (session frames are routed
        // elsewhere by the phase-5 dispatch).
        _ => {}
    }
}

/// Spawn a task to handle an inbound `control_request` (D14) and track it in
/// `inflight` so a `control_cancel_request` can abort it (INV-10) and a
/// duplicate delivery of the same `request_id` can be skipped while it is
/// in-flight (`sdk.mjs` `cancelControllers.has`).
fn spawn_inbound_handler(
    frame: Value,
    inflight: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    outbound: &mpsc::UnboundedSender<Outbound>,
    opts: &ControlOptions,
) {
    let subtype = frame
        .pointer("/request/subtype")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // `Z4`: for the machine serving this session's tools — never answered,
    // even when a handler exists.
    if SUPPRESSED_SUBTYPES.contains(&subtype.as_str()) {
        return;
    }
    let request_id = frame
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // A duplicate delivery of an in-flight request is skipped outright.
    if !request_id.is_empty() && inflight.contains_key(&request_id) {
        return;
    }
    let respond_id = request_id.clone();
    let tx = outbound.clone();
    let handler = opts.on_request.clone();
    let task = tokio::spawn(async move {
        let response = match handler {
            Some(handler) => handler(frame).await,
            None => sdk_default_response(&subtype, &respond_id),
        };
        if let Some(response) = response {
            let _ = tx.send(Outbound::Response(response));
        }
        // Both the response path and the silent path release the in-flight id
        // (the SDK's `finally` deletes the cancel controller either way).
        let _ = tx.send(Outbound::Finished(respond_id));
    });
    inflight.insert(request_id, task);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::io::AsyncBufReadExt;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    // A-21 / B-33: the CLI reply payload sits at `frame.response.response`
    // (SDK `request()` resolves with `e.response`; `getContextUsage` returns
    // `.response` of that). Readers must not look at the frame's top level.
    #[test]
    fn raw_max_tokens_reads_the_nested_reply_payload() {
        let frame = json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "r1",
            "response": {"rawMaxTokens": 200000}}});
        assert_eq!(raw_max_tokens(&frame), Some(200000));
        // The frame's top level is not the payload level.
        let top_level_only = json!({"type": "control_response", "rawMaxTokens": 5});
        assert_eq!(raw_max_tokens(&top_level_only), None);
    }

    #[test]
    fn still_queued_reads_the_nested_reply_payload() {
        let frame = json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "r2",
            "response": {"still_queued": ["u1", "u2"]}}});
        assert_eq!(
            still_queued_of(&frame),
            Some(vec!["u1".to_string(), "u2".to_string()])
        );
        let top_level_only = json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "r2", "still_queued": ["u1"]}});
        assert_eq!(still_queued_of(&top_level_only), None);
    }

    /// A test harness: builds a [`Control`] over an in-memory duplex stdin and
    /// returns:
    /// - the [`Control`] handle,
    /// - a receiver of every whole line written to "stdin" (trimmed strings),
    /// - a sender to push inbound control-traffic frames into the control task.
    fn harness(
        opts: ControlOptions,
    ) -> (
        Control,
        mpsc::UnboundedReceiver<String>,
        mpsc::UnboundedSender<Value>,
    ) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (itx, irx) = mpsc::unbounded_channel::<Value>();
        let control = Control::spawn(a, irx, opts);

        // Reader task: turn the "stdin" bytes into whole lines.
        let (ltx, lrx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim().to_string();
                        if !trimmed.is_empty() {
                            let _ = ltx.send(trimmed);
                        }
                    }
                }
            }
        });
        (control, lrx, itx)
    }

    async fn next_line(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
        let line = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("line available")
            .expect("stream open");
        serde_json::from_str(&line).expect("line is JSON")
    }

    async fn assert_no_line(rx: &mut mpsc::UnboundedReceiver<String>, within: Duration) {
        // Give the writer a moment; a line must NOT arrive.
        let res = timeout(within, rx.recv()).await;
        assert!(res.is_err(), "expected no line written, but got one");
    }

    /// 4.T1 (INV-7) — `keep_alive` is consumed; the caller sees nothing.
    #[tokio::test]
    async fn inv_07_keep_alive_consumed() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = control;

        // Feed a keep_alive; then a non-control frame must still be routed (the
        // keep_alive must not have disturbed the stream). Send a user frame and
        // a control_cancel_request; only those appear as lines.
        inbound.send(json!({"type": "keep_alive"})).unwrap();
        // Give the task a moment to consume the keep_alive.
        tokio::time::sleep(Duration::from_millis(50)).await;

        control
            .send_user(json!({"type":"user","message":{}}))
            .await
            .unwrap();
        let line = next_line(&mut lines).await;
        assert_eq!(
            line["type"], "user",
            "user frame written, keep_alive not surfaced"
        );
    }

    /// 4.T2 (INV-8) — 100 concurrent frames of mixed kinds produce exactly 100
    /// whole lines on stdin.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_08_single_stdin_writer() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = inbound;

        let mut tasks = Vec::new();
        for i in 0..100 {
            let control = control.clone();
            tasks.push(tokio::spawn(async move {
                match i % 4 {
                    0 => {
                        control
                            .send_user(json!({"type":"user","n":i}))
                            .await
                            .unwrap();
                    }
                    1 => {
                        control
                            .send_response(json!({"type":"control_response","response":{"subtype":"success","request_id":format!("resp{i}")}}))
                            .await
                            .unwrap();
                    }
                    2 => {
                        control
                            .send_cancel(json!({"type":"control_cancel_request","request_id":format!("cancel{i}")}))
                            .await
                            .unwrap();
                    }
                    _ => {
                        // control_request: spawn but ignore the (never-answered)
                        // response future.
                        let control2 = control.clone();
                        tokio::spawn(async move {
                            let _ = control2
                                .send_request(json!({"subtype":"get_context_usage","request_id_hint":i}))
                                .await;
                        });
                    }
                }
            }));
        }
        for t in tasks {
            timeout(Duration::from_secs(5), t)
                .await
                .expect("task completes")
                .unwrap();
        }

        // Every write produced exactly one whole line.
        let mut count = 0;
        for _ in 0..100 {
            let line = next_line(&mut lines).await;
            count += 1;
            let t = line["type"].as_str().expect("type field");
            assert!(
                matches!(
                    t,
                    "user" | "control_response" | "control_cancel_request" | "control_request"
                ),
                "unexpected type {t}"
            );
        }
        assert_eq!(count, 100, "exactly 100 whole lines written");
    }

    /// 4.T3 (INV-9) — a control_response for an unknown request_id is dropped,
    /// no panic; the channel keeps working.
    #[tokio::test]
    async fn inv_09_unknown_id_dropped() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());

        // A response for a request_id nobody sent.
        inbound
            .send(json!({"type":"control_response","response":{"subtype":"success","request_id":"ghostid"}}))
            .unwrap();

        // The module must not panic and must keep working: a later user frame
        // is still written.
        tokio::time::sleep(Duration::from_millis(50)).await;
        control.send_user(json!({"type":"user"})).await.unwrap();
        let line = next_line(&mut lines).await;
        assert_eq!(line["type"], "user");
    }

    /// 4.T4 (INV-10) — an inbound control_cancel_request aborts the pending
    /// handler and writes no response.
    #[tokio::test]
    async fn inv_10_inbound_cancel_aborts_handler() {
        // A handler that signals it started, then blocks until aborted.
        let (started_tx, mut started_rx) = mpsc::unbounded_channel::<()>();
        let handler: Arc<RequestHandler> = Arc::new(Box::new(move |_frame| {
            let started_tx = started_tx.clone();
            Box::pin(async move {
                let _ = started_tx.send(());
                tokio::time::sleep(Duration::from_secs(60)).await;
                Some(
                    json!({"type":"control_response","response":{"subtype":"success","request_id":"x"}}),
                )
            })
        }));
        let (control, mut lines, inbound) = harness(ControlOptions {
            on_request: Some(handler),
        });
        let _ = control;

        // Inbound control_request -> handler starts.
        inbound
            .send(json!({"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool"}}))
            .unwrap();
        timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("handler started")
            .expect("started signal");

        // Inbound control_cancel_request -> handler aborted.
        inbound
            .send(json!({"type":"control_cancel_request","request_id":"req-1"}))
            .unwrap();

        // No response is written (the aborted handler never sends one).
        assert_no_line(&mut lines, Duration::from_millis(300)).await;
    }

    /// 4.T5 (INV-11) — the emitted `initialize` frame equals
    /// `porting/fixtures/initialize.json` after id normalisation.
    #[tokio::test]
    async fn inv_11_initialize_matches_adapter() {
        let root = repo_root();
        let fixture = root.join("porting/fixtures/initialize.json");
        assert!(fixture.exists(), "initialize fixture exists");

        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = inbound;

        // Build an initialize request with the same hooks payload the fixture
        // captured (no optional systemPrompt/agents/skills/toolAliases set, so
        // they are omitted — matching the isolated capture). 2.7: the hooks are
        // the 0.94.0 set (PostModelSwitch/PostCompact added) and
        // `promptSuggestions` is set to match the regenerated fixture.
        let opts = InitializeOptions {
            hooks: Some(json!({
                "PostToolUse": [{"hookCallbackIds": ["hook_0"]}],
                "PostModelSwitch": [{"hookCallbackIds": ["hook_1"]}],
                "PostCompact": [{"hookCallbackIds": ["hook_2"]}],
                "TaskCreated": [{"hookCallbackIds": ["hook_3"]}],
                "TaskCompleted": [{"hookCallbackIds": ["hook_4"]}],
            })),
            prompt_suggestions: Some(true),
            forward_subagent_text: false,
            ..Default::default()
        };

        let _control = control.clone();
        let emit_task = tokio::spawn(async move {
            let _ = _control.send_request(opts.build_request()).await;
        });
        std::mem::drop(emit_task);

        let frame = next_line(&mut lines).await;
        let normalized = normalize_like_capture(&frame);
        let expected: Value = serde_json::from_str(&std::fs::read_to_string(&fixture).unwrap())
            .expect("fixture is valid json");
        assert_eq!(
            normalized, expected,
            "4.T5: initialize frame must match fixture"
        );
    }

    /// 4.T8 — a `pending_permission_requests` entry in the initialize response
    /// is replayed to the handler.
    #[tokio::test]
    async fn pending_permission_replayed() {
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<Value>();
        let handler: Arc<RequestHandler> = Arc::new(Box::new(move |frame| {
            let seen_tx = seen_tx.clone();
            Box::pin(async move {
                let _ = seen_tx.send(frame);
                Some(
                    json!({"type":"control_response","response":{"subtype":"success","request_id":"x"}}),
                )
            })
        }));
        let (control, _lines, _inbound) = harness(ControlOptions {
            on_request: Some(handler),
        });

        // Parse an initialize response carrying a pending can_use_tool request.
        let response = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "abc",
                "response": {
                    "models": [],
                    "commands": [],
                    "pending_permission_requests": [
                        {"type": "control_request", "request_id": "ppr-1",
                         "request": {"subtype": "can_use_tool", "tool_name": "Bash"}}
                    ]
                }
            }
        });
        let info = parse_initialize_response(&response);
        assert_eq!(info.pending_permission_requests.len(), 1);

        control
            .replay_pending_permissions(&info.pending_permission_requests)
            .await
            .unwrap();

        let replayed = timeout(Duration::from_secs(5), seen_rx.recv())
            .await
            .expect("handler invoked")
            .expect("replayed frame");
        assert_eq!(replayed["request"]["subtype"], "can_use_tool");
        assert_eq!(replayed["request_id"], "ppr-1");
    }

    /// 4.T9 — an inbound control request with an unknown subtype gets
    /// `{subtype:"error"}` written back.
    #[tokio::test]
    async fn unknown_subtype_errors() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = control;

        inbound
            .send(json!({"type":"control_request","request_id":"req-9","request":{"subtype":"bogus_subtype"}}))
            .unwrap();

        let frame = next_line(&mut lines).await;
        assert_eq!(frame["type"], "control_response");
        assert_eq!(frame["response"]["subtype"], "error");
        assert_eq!(frame["response"]["request_id"], "req-9");
        // R-07: the SDK's error response always carries the thrown message.
        assert_eq!(
            frame["response"]["error"],
            "Unsupported control request subtype: bogus_subtype"
        );
    }

    /// 4.T7 — a slow outbound control_request does not block inbound stream
    /// parsing (R5 is outbound only).
    #[tokio::test]
    async fn slow_outbound_does_not_block_inbound() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());

        // Start a slow outbound request (its response never arrives).
        let control2 = control.clone();
        let request_task = tokio::spawn(async move {
            let _ = control2
                .send_request(json!({"subtype":"get_context_usage"}))
                .await;
        });
        std::mem::drop(request_task);

        // The control_request is written.
        let frame = next_line(&mut lines).await;
        assert_eq!(frame["type"], "control_request");

        // While the request is still pending (no response yet), an inbound
        // keep_alive + control_request must still be processed (parsing is not
        // blocked).
        inbound.send(json!({"type":"keep_alive"})).unwrap();
        inbound
            .send(json!({"type":"control_request","request_id":"req-7","request":{"subtype":"unknown"}}))
            .unwrap();

        // The unknown inbound request is answered -> a response line is written,
        // proving inbound parsing continued despite the pending outbound request.
        let response = next_line(&mut lines).await;
        assert_eq!(response["type"], "control_response");
        assert_eq!(response["response"]["subtype"], "error");
    }

    /// A-44 — an outbound `control_request` rejects on an `error`
    /// `control_response` (`sdk.mjs` `request()` rejects with
    /// `Error(response.error)`), and resolves on a `success` one.
    #[tokio::test]
    async fn outbound_error_response_rejects() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());

        let control2 = control.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = reply_tx.send(
                control2
                    .send_request(json!({"subtype":"set_model","model":"x"}))
                    .await,
            );
        });
        let frame = next_line(&mut lines).await;
        assert_eq!(frame["type"], "control_request");
        let id = frame["request_id"]
            .as_str()
            .expect("request_id")
            .to_string();

        inbound
            .send(json!({"type":"control_response","response":{
                "subtype":"error","request_id":id,"error":"model not found"}}))
            .unwrap();

        let result = timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("reply arrives")
            .expect("reply channel open");
        match result {
            Err(ControlError::Remote(message)) => assert_eq!(message, "model not found"),
            other => panic!("an error verdict must reject, got {other:?}"),
        }

        // A success verdict still resolves with the full frame.
        let control3 = control.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = reply_tx.send(
                control3
                    .send_request(json!({"subtype":"set_model","model":"y"}))
                    .await,
            );
        });
        let frame = next_line(&mut lines).await;
        let id = frame["request_id"]
            .as_str()
            .expect("request_id")
            .to_string();
        inbound
            .send(json!({"type":"control_response","response":{
                "subtype":"success","request_id":id,"response":{}}}))
            .unwrap();
        let result = timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("reply arrives")
            .expect("reply channel open");
        assert!(result.is_ok(), "a success verdict must resolve: {result:?}");
    }

    /// F3a (a) — the `Z4` machine-only subtypes are never answered, even with a
    /// handler that would return a response (`sdk.mjs` `processControlRequest`).
    #[tokio::test]
    async fn sdkctl_z4_subtypes_are_never_answered() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler: Arc<RequestHandler> = {
            let calls = calls.clone();
            Arc::new(Box::new(move |_frame| {
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Some(json!({"type":"control_response","response":{
                        "subtype":"success","request_id":"x","response":{}}}))
                })
            }))
        };
        let (control, mut lines, inbound) = harness(ControlOptions {
            on_request: Some(handler),
        });
        let _ = control;

        for subtype in SUPPRESSED_SUBTYPES {
            inbound
                .send(
                    json!({"type":"control_request","request_id":format!("z4-{subtype}"),
                    "request":{"subtype":subtype}}),
                )
                .unwrap();
        }
        assert_no_line(&mut lines, Duration::from_millis(300)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the Z4 subtypes must be suppressed before any handler runs"
        );
    }

    /// F3a (a) — `request_user_dialog` with no handler stays silent so a
    /// capable client (or the worker's park deadline) settles it.
    #[tokio::test]
    async fn sdkctl_dialog_without_handler_stays_silent() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = control;

        inbound
            .send(json!({"type":"control_request","request_id":"dialog-1",
                "request":{"subtype":"request_user_dialog","dialog_kind":"refusal_fallback",
                    "payload":{"message":"continue?"}}}))
            .unwrap();
        assert_no_line(&mut lines, Duration::from_millis(300)).await;
    }

    /// F3a (a) — the general handler contract: `None` means write nothing.
    #[tokio::test]
    async fn handler_returning_none_writes_nothing() {
        let handler: Arc<RequestHandler> =
            Arc::new(Box::new(|_frame| Box::pin(async move { None })));
        let (control, mut lines, inbound) = harness(ControlOptions {
            on_request: Some(handler),
        });
        let _ = control;

        inbound
            .send(json!({"type":"control_request","request_id":"silent-1",
                "request":{"subtype":"can_use_tool","tool_name":"Bash"}}))
            .unwrap();
        assert_no_line(&mut lines, Duration::from_millis(300)).await;
    }

    /// F3a (b) — an `elicitation` with no handler gets a success
    /// `{action:"decline"}` (not an error verdict).
    #[tokio::test]
    async fn sdkctl_elicitation_without_handler_declines() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = control;

        inbound
            .send(json!({"type":"control_request","request_id":"elic-1",
                "request":{"subtype":"elicitation","mcp_server_name":"mcp",
                    "message":"pick one"}}))
            .unwrap();

        let frame = next_line(&mut lines).await;
        assert_eq!(frame["type"], "control_response");
        assert_eq!(frame["response"]["subtype"], "success");
        assert_eq!(frame["response"]["request_id"], "elic-1");
        assert_eq!(frame["response"]["response"], json!({"action":"decline"}));
    }

    /// F3a (c) — every error response carries the `error` string, even when no
    /// handler is registered (`sdk.mjs` `handleControlRequest`'s catch).
    #[tokio::test]
    async fn sdkctl_unknown_subtype_error_string_without_handler() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = control;

        inbound
            .send(json!({"type":"control_request","request_id":"unknown-1",
                "request":{"subtype":"oauth_token_refresh"}}))
            .unwrap();

        let frame = next_line(&mut lines).await;
        assert_eq!(frame["type"], "control_response");
        assert_eq!(frame["response"]["subtype"], "error");
        assert_eq!(frame["response"]["request_id"], "unknown-1");
        assert_eq!(
            frame["response"]["error"],
            "Unsupported control request subtype: oauth_token_refresh"
        );
    }

    /// F3a (d) — a duplicate delivery of an in-flight `request_id` is skipped
    /// (`sdk.mjs` `cancelControllers.has`): the handler runs once and exactly
    /// one response is written.
    #[tokio::test]
    async fn sdkctl_duplicate_request_is_skipped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let handler: Arc<RequestHandler> = {
            let calls = calls.clone();
            let gate = gate.clone();
            Arc::new(Box::new(move |frame| {
                let calls = calls.clone();
                let gate = gate.clone();
                let id = frame
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    gate.notified().await;
                    Some(json!({"type":"control_response","response":{
                        "subtype":"success","request_id":id,
                        "response":{"behavior":"allow"}}}))
                })
            }))
        };
        let (control, mut lines, inbound) = harness(ControlOptions {
            on_request: Some(handler),
        });
        let _ = control;

        let request = json!({"type":"control_request","request_id":"dup-1",
            "request":{"subtype":"can_use_tool","tool_name":"Bash"}});
        inbound.send(request.clone()).unwrap();
        // Wait until the first handler is running (it is now in flight).
        timeout(Duration::from_secs(5), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("first handler starts");

        inbound.send(request).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the duplicate delivery must not start a second handler"
        );

        gate.notify_one();
        let frame = next_line(&mut lines).await;
        assert_eq!(frame["response"]["request_id"], "dup-1");
        assert_eq!(frame["response"]["response"]["behavior"], "allow");
        assert_no_line(&mut lines, Duration::from_millis(200)).await;
    }

    /// F3a (e) — a request that arrives after close is skipped
    /// (`sdk.mjs` `cleanupPerformed`).
    #[tokio::test]
    async fn sdkctl_request_after_close_is_skipped() {
        let handler: Arc<RequestHandler> = Arc::new(Box::new(|frame| {
            let id = frame
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Box::pin(async move {
                Some(json!({"type":"control_response","response":{
                    "subtype":"success","request_id":id,"response":{}}}))
            })
        }));
        let (control, mut lines, inbound) = harness(ControlOptions {
            on_request: Some(handler),
        });

        inbound
            .send(json!({"type":"control_request","request_id":"before-close",
                "request":{"subtype":"can_use_tool","tool_name":"Bash"}}))
            .unwrap();
        let frame = next_line(&mut lines).await;
        assert_eq!(frame["response"]["request_id"], "before-close");

        control.close().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        inbound
            .send(json!({"type":"control_request","request_id":"after-close",
                "request":{"subtype":"can_use_tool","tool_name":"Bash"}}))
            .unwrap();
        assert_no_line(&mut lines, Duration::from_millis(300)).await;
    }

    /// F3a (c) — a `can_use_tool` with no handler errors with the SDK's
    /// callback-not-provided message.
    #[tokio::test]
    async fn can_use_tool_without_handler_errors() {
        let (control, mut lines, inbound) = harness(ControlOptions::default());
        let _ = control;

        inbound
            .send(json!({"type":"control_request","request_id":"perm-1",
                "request":{"subtype":"can_use_tool","tool_name":"Bash"}}))
            .unwrap();
        let frame = next_line(&mut lines).await;
        assert_eq!(frame["response"]["subtype"], "error");
        assert_eq!(
            frame["response"]["error"],
            "canUseTool callback is not provided."
        );
    }

    /// Normalise a frame the way `capture.sh`'s `normalize_object` does for the
    /// initialize fixture: uuids, timestamps and short `[a-z0-9]{8,20}` ids
    /// (the request_id and the subtype) become first-appearance placeholders.
    fn normalize_like_capture(value: &Value) -> Value {
        fn walk(v: &Value, map: &mut HashMap<String, String>, next: &mut usize) -> Value {
            match v {
                Value::String(s) => {
                    let is_id = s.len() >= 8
                        && s.len() <= 20
                        && s.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
                    if is_id {
                        let ph = map
                            .entry(s.clone())
                            .or_insert_with(|| {
                                let p = format!("${next}");
                                *next += 1;
                                p
                            })
                            .clone();
                        Value::String(ph)
                    } else {
                        Value::String(s.clone())
                    }
                }
                Value::Object(m) => Value::Object(
                    m.iter()
                        .map(|(k, v)| (k.clone(), walk(v, map, next)))
                        .collect(),
                ),
                Value::Array(a) => Value::Array(a.iter().map(|v| walk(v, map, next)).collect()),
                other => other.clone(),
            }
        }
        walk(value, &mut HashMap::new(), &mut 0)
    }

    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .expect("repo root resolves")
    }

    /// O-84 — `perTaskStopAffordance` is serialized only when the client (or
    /// the RFD default) defines it; `forwardSubagentText` stays always present.
    #[test]
    fn initialize_request_carries_per_task_stop_affordance() {
        let opts = InitializeOptions {
            per_task_stop_affordance: Some(false),
            ..Default::default()
        };
        let request = opts.build_request();
        assert_eq!(request["perTaskStopAffordance"], json!(false));
        assert_eq!(request["forwardSubagentText"], json!(false));

        let absent = InitializeOptions::default().build_request();
        assert!(absent.get("perTaskStopAffordance").is_none());
    }

    /// O-154 — `planModeInstructions` is serialized only when the client set it.
    #[test]
    fn t3a_initialize_request_carries_plan_mode_instructions_only_when_set() {
        let opts = InitializeOptions {
            plan_mode_instructions: Some("Plan with care.".to_string()),
            ..Default::default()
        };
        assert_eq!(
            opts.build_request()["planModeInstructions"],
            json!("Plan with care.")
        );
        assert!(InitializeOptions::default()
            .build_request()
            .get("planModeInstructions")
            .is_none());
    }

    /// O-153 — `agentProgressSummaries` is serialized only when the client set it.
    #[test]
    fn initialize_request_carries_agent_progress_summaries_only_when_set() {
        let opts = InitializeOptions {
            agent_progress_summaries: Some(true),
            ..Default::default()
        };
        assert_eq!(opts.build_request()["agentProgressSummaries"], json!(true));
        assert!(InitializeOptions::default()
            .build_request()
            .get("agentProgressSummaries")
            .is_none());
    }

    /// O-155, O-161 — `toolAliases` and `toolConfig` are serialized into initialize payload when provided.
    #[test]
    fn initialize_request_carries_tool_aliases_and_tool_config() {
        let opts = InitializeOptions {
            tool_aliases: Some(json!({"Read": "CustomRead"})),
            tool_config: Some(json!({"Bash": {"timeout": 60}})),
            ..Default::default()
        };
        let request = opts.build_request();
        assert_eq!(request["toolAliases"], json!({"Read": "CustomRead"}));
        assert_eq!(request["toolConfig"], json!({"Bash": {"timeout": 60}}));

        let absent = InitializeOptions::default().build_request();
        assert!(absent.get("toolAliases").is_none());
        assert!(absent.get("toolConfig").is_none());
    }

    /// B-74 — `rebind` moves every clone to another child's stdin: frames follow
    /// the new writer, a request the old child never answered fails, and the old
    /// child's stdin is closed.
    #[tokio::test]
    async fn t3a_rebind_moves_the_writer_and_fails_unanswered_requests() {
        let (control, mut first_lines, _inbound) = harness(ControlOptions::default());
        let waiting = {
            let control = control.clone();
            tokio::spawn(async move { control.send_request(json!({"subtype": "probe"})).await })
        };
        let line = next_line(&mut first_lines).await;
        assert_eq!(line["request"]["subtype"], "probe");

        let (next_stdin, next_reader) = tokio::io::duplex(64 * 1024);
        control.rebind(next_stdin).unwrap();
        let outcome = timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the unanswered request settles")
            .expect("join");
        assert!(
            matches!(outcome, Err(ControlError::Closed)),
            "the old child's request fails: {outcome:?}"
        );
        // The old stdin was closed by the rebind.
        assert!(
            timeout(Duration::from_secs(5), first_lines.recv())
                .await
                .expect("the old reader ends")
                .is_none(),
            "no further line on the old stdin"
        );

        control
            .send_user(json!({"type": "user", "message": {"content": "hi"}}))
            .await
            .unwrap();
        let mut reader = tokio::io::BufReader::new(next_reader);
        let mut line = String::new();
        timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("a line on the new stdin")
            .unwrap();
        let written: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(written["type"], "user");
    }
}
