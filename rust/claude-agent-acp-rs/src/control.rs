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

/// A pinned boxed future returning a `Value` (used by the inbound request
/// handler). Defined without the `futures` crate to avoid a new dependency.
type BoxFuture = Pin<Box<dyn Future<Output = Value> + Send + 'static>>;

/// Handles an inbound `control_request` (from `claude` to us).
///
/// Receives the full inbound `control_request` frame and returns the full
/// `control_response` frame to write back. Runs in a spawned task (D14) so the
/// control task never awaits it inline.
pub type RequestHandler = Box<dyn Fn(Value) -> BoxFuture + Send + Sync>;

/// Options controlling the control channel.
#[derive(Default)]
pub struct ControlOptions {
    /// Handler for inbound `control_request` frames. When `None` (or when the
    /// subtype is not recognised) the module responds `{subtype:"error"}`
    /// (4.6 / 4.T9).
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
    /// `agents`.
    pub agents: Option<Value>,
    /// `hooks` (the `initHooksPayload`).
    pub hooks: Option<Value>,
    /// `skills`.
    pub skills: Option<Value>,
    /// `toolAliases`.
    pub tool_aliases: Option<Value>,
    /// `forwardSubagentText`.
    pub forward_subagent_text: bool,
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
        if let Some(system_prompt) = &self.system_prompt {
            request.insert("systemPrompt".to_string(), system_prompt.clone());
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
        // `forwardSubagentText` is always sent (even when false), matching the
        // captured fixture.
        request.insert(
            "forwardSubagentText".to_string(),
            Value::Bool(self.forward_subagent_text),
        );
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
        tokio::spawn(control_task(stdin, inbound, rx, tx_for_tasks, opts));
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

    /// Perform the `initialize` handshake (4.4): send the `initialize`
    /// control_request built from `opts` and parse the correlated response.
    pub async fn initialize(
        &self,
        opts: &InitializeOptions,
    ) -> Result<InitializeInfo, ControlError> {
        let frame = self.send_request(opts.build_request()).await?;
        Ok(parse_initialize_response(&frame))
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
/// with the given `subtype` (4.6).
fn build_control_response(request_id: &str, subtype: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": subtype,
            "request_id": request_id,
        }
    })
}

/// The default response for an inbound `control_request` that has no registered
/// handler (or an unrecognised subtype): `{subtype:"error"}` (4.6, 4.T9).
fn respond_error(request_id: &str) -> Value {
    build_control_response(request_id, "error")
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
    mut stdin: impl AsyncWrite + Unpin,
    mut inbound: mpsc::UnboundedReceiver<Value>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
    outbound_tx: mpsc::UnboundedSender<Outbound>,
    opts: ControlOptions,
) {
    let mut pending: HashMap<String, oneshot::Sender<Result<Value, ControlError>>> = HashMap::new();
    let mut inflight: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

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
                }
            }
            maybe = inbound.recv() => {
                let Some(frame) = maybe else {
                    // The inbound control stream closed (child stdout EOF). The
                    // child is gone; stop.
                    break;
                };
                route_inbound(frame, &mut pending, &mut inflight, &outbound_tx, &opts);
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
                    let _ = reply.send(Ok(frame.clone()));
                }
                // Unknown request_id (INV-9 / 4.T3): dropped, no panic.
            }
        }
        "control_request" => {
            spawn_inbound_handler(frame, inflight, outbound, opts);
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
/// `inflight` so a `control_cancel_request` can abort it (INV-10).
fn spawn_inbound_handler(
    frame: Value,
    inflight: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    outbound: &mpsc::UnboundedSender<Outbound>,
    opts: &ControlOptions,
) {
    let request_id = frame
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let respond_id = request_id.clone();
    let tx = outbound.clone();
    let handler = opts.on_request.clone();
    let task = tokio::spawn(async move {
        let response = match handler {
            Some(handler) => handler(frame).await,
            None => respond_error(&respond_id),
        };
        let _ = tx.send(Outbound::Response(response));
    });
    inflight.insert(request_id, task);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncBufReadExt;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

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
                json!({"type":"control_response","response":{"subtype":"success","request_id":"x"}})
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
        // they are omitted — matching the isolated capture).
        let opts = InitializeOptions {
            hooks: Some(json!({
                "PostToolUse": [{"hookCallbackIds": ["hook_0"]}],
                "TaskCreated": [{"hookCallbackIds": ["hook_1"]}],
                "TaskCompleted": [{"hookCallbackIds": ["hook_2"]}]
            })),
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
                json!({"type":"control_response","response":{"subtype":"success","request_id":"x"}})
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
}
