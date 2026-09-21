//! Session actor — the single owner of all session state (Decision D4, item
//! 5.1, reworked for phase 8).
//!
//! One tokio task owns the session: the child `claude` process, the control
//! channel, the line codec, the [`TurnMachine`], the `MapState` and the map of
//! prompt uuids to their reply `oneshot`s. No other task holds a lock on
//! session state — operations arrive as [`Command`]s over an `mpsc` with a
//! bundled `oneshot` reply (D4). This matches the house actor pattern
//! (`vst-agents/src/acp_connection.rs`).
//!
//! The actor's read loop is a `tokio::select!` between the [`Command`] channel
//! and the codec's [`StreamMsg`] stream. On stream EOF / codec death it drives
//! [`TurnMachine::on_stream_end`] / `fail_all`, resolves or rejects every
//! outstanding prompt, and marks the session closed so later prompts reject up
//! front (8.9).
//!
//! ## Phase-8 turn wiring (review 04 § Phase 8 wiring)
//!
//! - **Prompt push is immediate.** Each `Command::Prompt` carries a fresh uuid
//!   stamped into the outbound `user` frame; the actor registers `uuid ->
//!   oneshot`, `enqueue(Turn::new(uuid, is_local_only))` and pushes the frame
//!   to `claude` at once (never gated on the active turn). Every
//!   `Settled{uuid, stop_reason, usage}` resolves the matching oneshot; every
//!   `Failed{uuid, kind, message}` rejects it.
//! - **`dispatch_stream` routes session frames in stream order** into the
//!   `TurnMachine` (`user`+`uuid` → `on_echo`; `result` →
//!   `on_result(.., emitted_assistant_text)`; `task_started` /
//!   `task_notification` / terminal `task_updated` →
//!   `on_task_started`/`on_task_ended`; `session_state_changed` →
//!   record state, `idle` → `on_idle()`; `stream_event` / `assistant` → the
//!   phase-7 mapper + `emitted_assistant_text` tracking).
//! - **`FinalText{text}`** is emitted as an `agent_message_chunk` *before* the
//!   same batch's `Settled` resolves the prompt.
//! - The one-active-turn high-water counter (INV-14) is keyed on
//!   `Activated` / `Settled` events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::control::{Control, ControlOptions, InitializeOptions, RequestHandler};
use crate::dispatch::{self, Route, StreamMsg};
use crate::map::{self, MapState, MsgRole};
use crate::permission;
use crate::process::{self, SpawnOptions, Timings};
use crate::turn::{FailureKind, StopReason, Turn, TurnEvent, TurnMachine};

/// Errors produced by the session actor.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session closed")]
    Closed,
    #[error("session/prompt failed: {message}")]
    PromptFailed {
        kind: FailureKind,
        message: String,
        /// Whether the turn's assistant message carried an `error` (the Node's
        /// `lastAssistantError`). The wire `data.errorKind` is only attached when
        /// this is true (`errorKindData(lastAssistantError)`, phase 9).
        assistant_had_error: bool,
    },
    #[error("failed to start the session: {0}")]
    Start(String),
}

/// One item on the session → agent outbound channel (phase 9 flush barrier).
///
/// The agent forwards `Update`s to the client as notifications and uses a
/// `Flush` barrier to guarantee the tool-call notifications emitted before a
/// turn settles are written to the wire *before* the prompt response (fixes the
/// notification-vs-response race that `inv_24_tools` exposed). The FIFO channel
/// means the forwarding task processes the `Flush` only after every `Update`
/// sent before it, so resolving its `oneshot` is a happens-after guarantee.
///
/// `large_enum_variant` allowed: `Update` (the common case) is intentionally
/// large; boxing it would only add an indirection to the hot path for no
/// benefit over the rare `Flush` marker.
#[allow(clippy::large_enum_variant)]
pub enum SessionOutbound {
    /// A `session/update` notification to forward to the client.
    Update(SessionNotification),
    /// A barrier: every `Update` sent before this has been forwarded. Resolving
    /// the `oneshot` signals the prompt task it may now write its response.
    Flush(oneshot::Sender<()>),
    /// Send a `session/request_permission` request to the client and await its
    /// outcome (phase 10). The agent's forwarding task resolves `reply` with the
    /// raw JSON-RPC `result` (`Ok`) or a transport error (`Err`).
    RequestPermission {
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

/// The result of a settled `session/prompt`.
pub struct PromptReply {
    /// The turn's ACP `stopReason`.
    pub stop_reason: String,
    /// Token usage reported on the response (the fixture's
    /// `result.usage` shape).
    pub usage: Option<PromptUsage>,
    /// The turn's streaming updates (e.g. a `FinalText` `agent_message_chunk`)
    /// that must be forwarded to the client *before* the prompt response.
    pub updates: Vec<SessionNotification>,
    /// Resolves once every session update emitted before this turn settled has
    /// been forwarded to the client (phase 9). The prompt task awaits it before
    /// writing the response so a `tool_call` lands ahead of the `result`.
    pub flush_rx: Option<oneshot::Receiver<()>>,
}

impl std::fmt::Debug for PromptReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptReply")
            .field("stop_reason", &self.stop_reason)
            .field("usage", &self.usage)
            .field("updates", &self.updates)
            .field("flush_rx", &self.flush_rx.as_ref().map(|_| "<oneshot>"))
            .finish()
    }
}

/// Token usage reported in a `session/prompt` response.
#[derive(Debug, Clone, Default)]
pub struct PromptUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_read_tokens: u64,
    pub cached_write_tokens: u64,
    pub total_tokens: u64,
}

/// A request sent by the ACP client to the session actor (D4). Each request
/// carries a `oneshot` reply resolved by the actor when the operation settles.
enum Command {
    /// Enqueue a prompt turn (8.7). `uuid` is stamped into `frame`'s `uuid`
    /// field by the caller; the actor registers `uuid -> reply`, enqueues the
    /// turn and pushes `frame` to `claude` immediately.
    Prompt {
        uuid: String,
        frame: Value,
        is_local_only: bool,
        reply: oneshot::Sender<Result<PromptReply, SessionError>>,
    },
    /// Cancel the active prompt (8.9): `machine.cancel()` then send an
    /// `interrupt` control_request.
    Cancel,
    /// The `interrupt` control_response receipt came back (phase 11): carry its
    /// `still_queued` (or `None` for a bare `{}` receipt) so the actor can
    /// reconcile the orphan-credit count (`machine.reconcile_orphan_receipt`,
    /// INV-23).
    InterruptReceipt { still_queued: Option<Vec<String>> },
    /// Handle an inbound `can_use_tool` control_request (phase 10). `reply`
    /// resolves with the `control_response` frame (the R28 deny / allow
    /// payload) once the client's permission outcome is known (10.1, D14).
    Permission {
        frame: Value,
        reply: oneshot::Sender<Value>,
    },
    /// Steer the running turn (12.1, `_session/steering`, R32). `frame` is the
    /// `user` message to inject; `is_prompt_required` is whether the client
    /// opted into the host-owned `promptRequired` idle fallback. `reply`
    /// resolves with the `{outcome: …}` wire value the actor decides.
    Steer {
        uuid: String,
        frame: Value,
        is_local_only: bool,
        is_prompt_required: bool,
        reply: oneshot::Sender<Value>,
    },
}

/// Configuration used to start a session (phase 8 / 8.2, 8.4).
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Spawn argv/env for the `claude` child (B2).
    pub spawn: SpawnOptions,
    /// Grace constants for the child lifecycle.
    pub timings: Timings,
    /// The `initialize` control_request to send during handshake (phase 4).
    pub initialize: InitializeOptions,
}

impl SessionOptions {
    /// Build options from a `ServeOptions`-style config for a fresh session.
    pub fn new(spawn: SpawnOptions, timings: Timings, initialize: InitializeOptions) -> Self {
        Self {
            spawn,
            timings,
            initialize,
        }
    }
}

/// A handle to the session actor. Cloneable: the ACP layer (agent.rs) can issue
/// [`Command`]s through it.
#[derive(Clone)]
pub struct Session {
    tx: mpsc::UnboundedSender<Command>,
    high_water: Arc<AtomicUsize>,
    session_id: String,
}

impl Session {
    /// Spawn the child, handshake, and start the session actor task.
    ///
    /// Returns the [`Session`] handle and a receiver of every `session/update`
    /// notification the actor emits (the agent layer forwards them to the ACP
    /// client). `session_id` is the ACP `sessionId` this session is known by
    /// (the minted uuid for `session/new`, or the resumed id for
    /// `session/load`).
    pub async fn start(
        opts: SessionOptions,
        session_id: String,
    ) -> Result<(Session, mpsc::UnboundedReceiver<SessionOutbound>), SessionError> {
        // Spawn the child and its stack: process -> control (stdin) + codec
        // (stdout lines) + stderr tail.
        let mut process = process::spawn(&opts.spawn)
            .await
            .map_err(|e| SessionError::Start(e.to_string()))?;
        let stdin = process
            .take_stdin()
            .ok_or_else(|| SessionError::Start("child stdin unavailable".into()))?;
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();

        // The actor's command channel must exist before the control channel is
        // spawned, so the inbound `can_use_tool` handler can route frames to the
        // actor (phase 10).
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(
            stdin,
            control_in_rx,
            ControlOptions {
                on_request: Some(build_permission_request_handler(tx.clone())),
            },
        );

        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let high_water = Arc::new(AtomicUsize::new(0));
        let hw = high_water.clone();

        // The `Process` owns the child (`kill_on_drop(true)`), so it must stay
        // alive for the session's lifetime — move it into the actor task. The
        // actor reads `process.lines` (the codec stream) and keeps `process`
        // alive until the task ends (then the child is killed on teardown).
        let mut process = process;
        let stream = std::mem::replace(&mut process.lines, mpsc::unbounded_channel().1);
        tokio::spawn(run(
            commands,
            tx.clone(),
            stream,
            process,
            control.clone(),
            control_in_tx,
            update_tx,
            opts.timings,
            session_id.clone(),
            hw,
        ));

        // Phase-4 initialize handshake. The actor loop is already running so it
        // routes the child's control_response back to the control task. The
        // fake-claude transcript expects the initialize control_request; parse
        // the response.
        let _info = control
            .initialize(&opts.initialize)
            .await
            .map_err(|e| SessionError::Start(format!("initialize handshake: {e}")))?;

        Ok((
            Session {
                tx,
                high_water,
                session_id,
            },
            update_rx,
        ))
    }

    /// The ACP `sessionId` this session is known by.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a `session/prompt` request to the actor and await its settlement.
    pub async fn prompt(
        &self,
        uuid: String,
        frame: Value,
        is_local_only: bool,
    ) -> Result<PromptReply, SessionError> {
        self.send_prompt(uuid, frame, is_local_only)
            .await
            .map_err(|_| SessionError::Closed)?
    }

    /// Enqueue a `session/prompt` on the actor, returning the `oneshot` that
    /// resolves when the turn settles. This is synchronous: the connection's
    /// single-task handler can send it immediately, so a later `session/cancel`
    /// notification is always processed *after* this prompt is enqueued (the
    /// queued-sweep ordering, 8.9).
    pub fn send_prompt(
        &self,
        uuid: String,
        frame: Value,
        is_local_only: bool,
    ) -> oneshot::Receiver<Result<PromptReply, SessionError>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::Prompt {
            uuid,
            frame,
            is_local_only,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Steer the session (12.1): inject a message into the running turn at
    /// `now` priority, or report the idle `promptRequired` / `startedNewTurn`
    /// outcome. Returns the `oneshot` resolving with the `{outcome: …}` value.
    pub fn steer(
        &self,
        uuid: String,
        frame: Value,
        is_local_only: bool,
        is_prompt_required: bool,
    ) -> oneshot::Receiver<Value> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::Steer {
            uuid,
            frame,
            is_local_only,
            is_prompt_required,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Send a `session/cancel` notification (8.9).
    pub async fn cancel(&self) -> Result<(), SessionError> {
        self.tx
            .send(Command::Cancel)
            .map_err(|_| SessionError::Closed)
    }

    /// The high-water mark of simultaneously-active turns observed so far
    /// (INV-14 / 5.4). Never above 1 by construction.
    pub fn high_water(&self) -> usize {
        self.high_water.load(Ordering::Relaxed)
    }
}

/// Build the inbound `control_request` handler injected into the control
/// channel (phase 10). A `can_use_tool` request is routed to the session actor
/// via [`Command::Permission`] (which owns the `MapState` needed to
/// `ensure_tool_call_emitted`); the reply resolves with the `control_response`.
/// Any other subtype falls through to the default `{subtype:"error"}` response
/// (4.6 / 4.T9).
fn build_permission_request_handler(
    command_tx: mpsc::UnboundedSender<Command>,
) -> Arc<RequestHandler> {
    Arc::new(Box::new(move |frame: Value| {
        let command_tx = command_tx.clone();
        Box::pin(async move {
            let request_id = frame
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let subtype = frame
                .pointer("/request/subtype")
                .and_then(Value::as_str)
                .unwrap_or("");
            if subtype == "can_use_tool" {
                let (reply_tx, reply_rx) = oneshot::channel();
                let _ = command_tx.send(Command::Permission {
                    frame,
                    reply: reply_tx,
                });
                reply_rx
                    .await
                    .unwrap_or_else(|_| permission::deny_control_response(&request_id))
            } else {
                json!({
                    "type": "control_response",
                    "response": { "subtype": "error", "request_id": request_id },
                })
            }
        })
    }))
}

/// The session actor task. Owns all session state; nothing else mutates it.
#[allow(clippy::too_many_arguments)]
async fn run(
    commands: mpsc::UnboundedReceiver<Command>,
    command_tx: mpsc::UnboundedSender<Command>,
    stream: mpsc::UnboundedReceiver<Value>,
    _process: crate::process::Process,
    control: Control,
    control_in_tx: mpsc::UnboundedSender<Value>,
    update_tx: mpsc::UnboundedSender<SessionOutbound>,
    timings: Timings,
    session_id: String,
    high_water: Arc<AtomicUsize>,
) {
    // The `_process` handle is held only for its lifetime (it owns the child and
    // kills it on drop); the actor loop itself never touches it, so the core
    // loop is factored into `run_loop` and exercised directly by unit tests
    // (INV-27 drives the real actor command path without spawning a child).
    // A clone of the command sender is threaded into the loop so the spawned
    // `interrupt` task can report its receipt back to the actor (phase 11).
    run_loop(
        commands,
        command_tx,
        stream,
        control,
        control_in_tx,
        update_tx,
        timings.force_cancel_grace,
        session_id,
        high_water,
    )
    .await
}

/// The session actor's core `select!` loop — the real command-dispatch path.
/// Factored out of [`run`] so tests can drive the actor without a child
/// process (INV-27).
#[allow(clippy::too_many_arguments)]
async fn run_loop(
    mut commands: mpsc::UnboundedReceiver<Command>,
    command_tx: mpsc::UnboundedSender<Command>,
    mut stream: mpsc::UnboundedReceiver<Value>,
    control: Control,
    control_in_tx: mpsc::UnboundedSender<Value>,
    update_tx: mpsc::UnboundedSender<SessionOutbound>,
    force_cancel_grace: Duration,
    session_id: String,
    high_water: Arc<AtomicUsize>,
) {
    let mut machine = TurnMachine::new();
    let mut map_state = MapState::default();
    // uuid -> prompt reply oneshot (8.7).
    let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
        HashMap::new();
    let mut emitted_assistant_text = false;
    // Whether the current turn's assistant message carried an `error` (the
    // Node's `lastAssistantError`, reset per prompt; phase 9). The wire
    // `data.errorKind` is only attached when this is true.
    let mut assistant_had_error = false;
    // Subagent attribution (10.5): `task_id -> parentToolUseId` for live
    // background tasks, mirroring the Node's `liveBackgroundTasks` so a
    // subagent's `can_use_tool` is attributed to the Agent/Task tool call that
    // spawned it.
    let mut live_background: HashMap<String, String> = HashMap::new();
    // Pending permission round-trips (phase 10): `request_id -> JoinHandle`.
    // `Command::Cancel` aborts every one so a pending permission resolves as
    // `Cancelled` (R28 deny written back to the control writer, INV-21 / 27;
    // CUJ 2).
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // The force-cancel backstop (11.2, INV-17, #680): the absolute deadline at
    // which a wedged stream's active turn is force-settled `cancelled`. Armed at
    // most once per cancel sequence (`!forceCancelTimer`), cleared as soon as no
    // active turn remains (upstream `disarmForceCancel` on every settle path).
    let mut force_cancel_deadline: Option<tokio::time::Instant> = None;

    loop {
        // The force-cancel sleep: armed -> wait until the deadline; otherwise
        // park (far future). Rebuilt each iteration from the absolute deadline,
        // so an armed floor always fires at the same instant.
        let force_cancel_sleep = match force_cancel_deadline {
            Some(deadline) => Box::pin(tokio::time::sleep_until(deadline)),
            None => Box::pin(tokio::time::sleep(Duration::from_secs(3600))),
        };
        tokio::select! {
            biased;
            _ = force_cancel_sleep => {
                if force_cancel_deadline.is_some() {
                    // The interrupt didn't make the SDK yield within the grace:
                    // force the active turn to settle `cancelled` (INV-17, #680).
                    force_cancel_deadline = None;
                    let events = machine.force_cancel();
                    handle_events(
                        &events,
                        &mut pending,
                        &update_tx,
                        &session_id,
                        &high_water,
                        assistant_had_error,
                    );
                }
            }
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    break;
                };
                match cmd {
                    Command::Prompt { uuid, frame, is_local_only, reply } => {
                        let _ = control.send_user(frame).await;
                        machine.enqueue(Turn::new(uuid.clone(), is_local_only));
                        pending.insert(uuid, reply);
                        assistant_had_error = false;
                    }
                    Command::Cancel => {
                        // Abort every pending permission round-trip (CUJ 2):
                        // dropping the `reply` oneshot makes control.rs's
                        // on-request handler resolve with the R28 deny payload,
                        // so the pending permission settles `Cancelled` (INV-21 /
                        // INV-27) while the actor stays free.
                        for handle in pending_permissions.values() {
                            handle.abort();
                        }
                        pending_permissions.clear();
                        // 8.9: settle swept queued turns, then interrupt.
                        let events = machine.cancel();
                        handle_events(
                            &events,
                            &mut pending,
                            &update_tx,
                            &session_id,
                            &high_water,
                            assistant_had_error,
                        );
                        // Arm the force-cancel backstop at most once per turn
                        // (11.2, INV-22): if the interrupt below doesn't make the
                        // SDK yield (a wedged TaskOutput block, #680), the active
                        // turn is force-settled `cancelled` once the grace elapses.
                        // Re-sent cancels retry interrupt() but never push the
                        // deadline out. Only armed while an active turn remains.
                        if machine.has_active() && force_cancel_deadline.is_none() {
                            force_cancel_deadline =
                                Some(tokio::time::Instant::now() + force_cancel_grace);
                        }
                        // Send the `interrupt` on a spawned task: awaiting the
                        // correlated control_response here would block the actor's
                        // single-task loop (which is what routes that response back
                        // to the Control task), deadlocking it. The task reports
                        // the receipt's `still_queued` back so the actor can
                        // reconcile the orphan-credit count (11.3, INV-23).
                        let control = control.clone();
                        let command_tx = command_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(receipt) = control
                                .send_request(json!({"subtype": "interrupt"}))
                                .await
                            {
                                let still_queued = receipt
                                    .get("response")
                                    .and_then(|r| r.get("still_queued"))
                                    .and_then(Value::as_array)
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(Value::as_str)
                                            .map(String::from)
                                            .collect()
                                    });
                                let _ = command_tx
                                    .send(Command::InterruptReceipt { still_queued });
                            }
                        });
                    }
                    Command::InterruptReceipt { still_queued } => {
                        // 11.3 / 11.4: reconcile the orphan count against the
                        // receipt. A bare `{}` receipt (field absent) is handled
                        // inside the machine as count-everything.
                        machine.reconcile_orphan_receipt(still_queued.as_deref());
                    }
                    Command::Permission { frame, reply } => {
                        // The client round-trip (10.1, D14) runs in a spawned task
                        // so the actor never awaits it inline (INV-27): a
                        // `session/cancel` notification stays processable while a
                        // permission is pending.
                        // Prune finished round-trips first so the registry stays a
                        // live-set of pending permissions (abortable on cancel).
                        pending_permissions.retain(|_, h| !h.is_finished());
                        handle_permission(
                            frame,
                            reply,
                            &mut map_state,
                            &mut live_background,
                            &update_tx,
                            &session_id,
                            &mut pending_permissions,
                        );
                    }
                    Command::Steer {
                        uuid,
                        frame,
                        is_local_only,
                        is_prompt_required,
                        reply,
                    } => {
                        // 12.1 (R32): decide the outcome from whether a turn is
                        // in flight. The agent already rejected a bad
                        // `idleBehavior`, so `steer_outcome` only maps the
                        // in-flight / idle decision here.
                        let turn_in_flight = machine.has_unsettled();
                        let idle_behavior = if is_prompt_required {
                            Some("promptRequired")
                        } else {
                            None
                        };
                        let outcome = match crate::agent::steer_outcome(
                            turn_in_flight,
                            idle_behavior,
                        ) {
                            Ok(v) => v,
                            Err(e) => serde_json::to_value(e).unwrap_or_else(|_| {
                                json!({ "error": { "code": -32602, "message": "invalid params" } })
                            }),
                        };
                        match outcome.get("outcome").and_then(Value::as_str) {
                            Some("injected") => {
                                // `now` priority injection into the running turn:
                                // push the user message on the same stdin writer
                                // (D6), pre-empting the current generation (R32).
                                let mut injected = frame;
                                injected["priority"] = json!("now");
                                let _ = control.send_user(injected).await;
                            }
                            Some("startedNewTurn") => {
                                // Idle, no opt-in: start a detached turn like a
                                // normal prompt (it streams updates and settles
                                // internally; nothing awaits its oneshot).
                                let (ptx, _prx) = oneshot::channel();
                                let _ = control.send_user(frame).await;
                                machine.enqueue(Turn::new(uuid.clone(), is_local_only));
                                pending.insert(uuid, ptx);
                                assistant_had_error = false;
                            }
                            _ => {}
                        }
                        let _ = reply.send(outcome);
                    }
                }
            }
            msg = stream.recv() => {
                let Some(line) = msg else {
                    // Stream EOF / codec death (8.9): settle/reject every turn.
                    let events = machine.on_stream_end();
                    handle_events(
                        &events,
                        &mut pending,
                        &update_tx,
                        &session_id,
                        &high_water,
                        assistant_had_error,
                    );
                    break;
                };
                dispatch_stream(
                    StreamMsg::new(line),
                    &control_in_tx,
                    &mut machine,
                    &mut map_state,
                    &mut live_background,
                    &mut pending,
                    &mut emitted_assistant_text,
                    &mut assistant_had_error,
                    &update_tx,
                    &session_id,
                    &high_water,
                )
                .await;
            }
        }
        // Disarm the force-cancel backstop as soon as no active turn remains —
        // upstream `disarmForceCancel` runs on every path that settles the
        // active turn (settle/fail/force-cancel/stream-end), so a timer can never
        // fire on an already-settled turn, and a LATER turn's cancel can arm it
        // fresh.
        if !machine.has_active() {
            force_cancel_deadline = None;
        }
    }
}

/// Route one inbound stream message (8.8): forward control traffic to
/// `control.rs`, drive the turn machine for session traffic.
#[allow(clippy::too_many_arguments)]
async fn dispatch_stream(
    msg: StreamMsg,
    control_in_tx: &mpsc::UnboundedSender<Value>,
    machine: &mut TurnMachine,
    map_state: &mut MapState,
    live_background: &mut HashMap<String, String>,
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
    emitted_assistant_text: &mut bool,
    assistant_had_error: &mut bool,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
) {
    let view = dispatch::TurnView::default();
    match dispatch::route(&msg, &view) {
        Route::Control => {
            let _ = control_in_tx.send(msg.line);
        }
        Route::Session => {
            handle_session_frame(
                msg.line,
                machine,
                map_state,
                live_background,
                pending,
                emitted_assistant_text,
                assistant_had_error,
                update_tx,
                session_id,
                high_water,
            );
        }
        Route::Ignore => {}
    }
}

/// Drive the turn machine with one session frame, in stream order (8.8).
#[allow(clippy::too_many_arguments)]
fn handle_session_frame(
    line: Value,
    machine: &mut TurnMachine,
    map_state: &mut MapState,
    live_background: &mut HashMap<String, String>,
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
    emitted_assistant_text: &mut bool,
    assistant_had_error: &mut bool,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
) {
    let frame_type = line.get("type").and_then(Value::as_str).unwrap_or("");
    match frame_type {
        // The CLI re-emits the user message (--replay-user-messages); its echo
        // promotes the queued turn. The turn's own echo is not forwarded.
        "user" => {
            if let Some(uuid) = line.get("uuid").and_then(Value::as_str) {
                let events = machine.on_echo(uuid);
                handle_events(
                    &events,
                    pending,
                    update_tx,
                    session_id,
                    high_water,
                    *assistant_had_error,
                );
            }
        }
        "result" => {
            let events = machine.on_result(&line, *emitted_assistant_text);
            // Upstream clears emittedAssistantText in the result finally for
            // non-autonomous results.
            if !is_autonomous(&line) {
                *emitted_assistant_text = false;
            }
            handle_events(
                &events,
                pending,
                update_tx,
                session_id,
                high_water,
                *assistant_had_error,
            );
        }
        "system" => {
            let subtype = line.get("subtype").and_then(Value::as_str).unwrap_or("");
            match subtype {
                "task_started" => {
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    let is_subagent = line.get("subagent_type").is_some();
                    machine.on_task_started(task_id, is_subagent);
                    // Subagent attribution (10.5): record the Agent/Task tool
                    // call (`tool_use_id`) that spawned this task, so a
                    // subagent's `can_use_tool` is attributed to it.
                    if is_subagent {
                        if let Some(parent) = line.get("tool_use_id").and_then(Value::as_str) {
                            live_background.insert(task_id.to_string(), parent.to_string());
                        }
                    }
                }
                "task_notification" | "task_updated" => {
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    if is_terminal_task_update(&line) {
                        let events = machine.on_task_ended(task_id);
                        live_background.remove(task_id);
                        handle_events(
                            &events,
                            pending,
                            update_tx,
                            session_id,
                            high_water,
                            *assistant_had_error,
                        );
                    }
                }
                "session_state_changed" => {
                    let state = line.get("state").and_then(Value::as_str).unwrap_or("");
                    machine.on_session_state(state);
                    if state == "idle" {
                        let events = machine.on_idle();
                        handle_events(
                            &events,
                            pending,
                            update_tx,
                            session_id,
                            high_water,
                            *assistant_had_error,
                        );
                    }
                }
                "commands_changed" => {
                    emit_commands_changed(&line, update_tx, session_id);
                }
                _ => {}
            }
        }
        "stream_event" | "assistant" => {
            // The Node snapshots `lastAssistantError` from a top-level `error`
            // on the assistant frame (`acp-agent.ts:4333-4335`); record it so
            // the turn's failure can attach `data.errorKind` only when present.
            if frame_type == "assistant" && line.get("error").is_some() {
                *assistant_had_error = true;
            }
            let updates = if frame_type == "stream_event" {
                map::map_stream_event(&line, map_state)
            } else {
                // A consolidated assistant/user message: Node does NOT deliver
                // its text/thinking as chunks (only the stream_event deltas do;
                // the consolidated text is the #453/fallback source). Emit only
                // the tool/plan updates, matching the captured fixtures.
                map::map_consolidated(
                    &line
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    MsgRole::Assistant,
                    map_state,
                )
                .into_iter()
                .filter(|u| {
                    !matches!(
                        u,
                        SessionUpdate::AgentMessageChunk(_)
                            | SessionUpdate::AgentThoughtChunk(_)
                            | SessionUpdate::UserMessageChunk(_)
                    )
                })
                .collect::<Vec<_>>()
            };
            for update in &updates {
                if is_top_level_text(update, &line) {
                    *emitted_assistant_text = true;
                }
            }
            emit_updates(&updates, update_tx, session_id);
        }
        _ => {}
    }
}

/// Handle an inbound `can_use_tool` control_request (10.1–10.5).
///
/// Emits the `tool_call` if the client has not seen it yet (`ensureToolCallEmitted`,
/// INV-20 / #851), then spawns a task (D14) that sends `session/request_permission`,
/// maps the outcome to the `control_response` (10.3 / 10.4) and resolves `reply`.
/// The actor never awaits the client round-trip inline (INV-27).
fn handle_permission(
    frame: Value,
    reply: oneshot::Sender<Value>,
    map_state: &mut MapState,
    live_background: &mut HashMap<String, String>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    pending_permissions: &mut HashMap<String, tokio::task::JoinHandle<()>>,
) {
    let can = permission::parse_can_use_tool(&frame);
    // Subagent attribution (10.5): the parent Agent/Task tool call that spawned
    // the subagent (`liveBackgroundTasks`).
    let parent = can
        .agent_id
        .as_deref()
        .and_then(|id| live_background.get(id).cloned());
    let request_id = can.request_id.clone();

    // ensureToolCallEmitted (10.2, INV-20): surface the tool_call before the
    // permission request references it, unless the streamed tool_use already did.
    if let Some(update) = permission::ensure_tool_call_emitted(
        &can.tool_name,
        &can.tool_input,
        &can.tool_use_id,
        parent.as_deref(),
        map_state,
    ) {
        let notif = SessionNotification::new(session_id.to_string(), update);
        let _ = update_tx.send(SessionOutbound::Update(notif));
    }

    // Build the `request_permission` params (10.1) before moving `can` into the
    // spawned round-trip task.
    let tool_call = permission::build_tool_call(
        &can.tool_name,
        &can.tool_input,
        &can.tool_use_id,
        parent.as_deref(),
    );
    let params = permission::build_request_permission(
        session_id,
        tool_call,
        &can.tool_name,
        &can.suggestions,
    );

    // Spawn the round-trip (D14): ask the agent layer to send
    // `session/request_permission` and await the outcome, map it, resolve `reply`
    // (the `control.rs` on-request task writes the response frame). The handle is
    // registered so `Command::Cancel` can abort it (CUJ 2, INV-27).
    let update_tx = update_tx.clone();
    let handle = tokio::spawn(async move {
        let (ptx, prx) = oneshot::channel();
        let _ = update_tx.send(SessionOutbound::RequestPermission { params, reply: ptx });
        let response = match prx.await {
            Ok(Ok(value)) => match permission::map_outcome(&value) {
                permission::Outcome::Allow => {
                    permission::allow_control_response(&can.request_id, &can.tool_input)
                }
                permission::Outcome::AllowAlways => permission::allow_always_control_response(
                    &can.request_id,
                    &can.tool_input,
                    &can.tool_name,
                    &can.suggestions,
                ),
                permission::Outcome::Deny | permission::Outcome::Cancelled => {
                    permission::deny_control_response(&can.request_id)
                }
            },
            // The request errored or the round-trip was dropped (e.g. the client
            // closed / cancelled): refuse the tool (R28 deny) so the turn settles
            // on the next `result` (INV-21).
            Ok(Err(_)) | Err(_) => permission::deny_control_response(&can.request_id),
        };
        let _ = reply.send(response);
    });
    pending_permissions.insert(request_id, handle);
}

/// Process a batch of [`TurnEvent`]s in order (8.8 / review-04 wiring):
/// `FinalText` is emitted as an `agent_message_chunk` before the same batch's
/// `Settled` resolves the prompt.
fn handle_events(
    events: &[TurnEvent],
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
    assistant_had_error: bool,
) {
    // A `FinalText` precedes its turn's `Settled` in the same batch; collect it
    // and hand it to the prompt task so it can forward the chunk before the
    // response (deterministic, review-04 wiring).
    let mut final_texts: Vec<SessionNotification> = Vec::new();
    for event in events {
        match event {
            TurnEvent::FinalText { text } => {
                let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(text)),
                ));
                let notif = SessionNotification::new(session_id.to_string(), update);
                final_texts.push(notif);
            }
            TurnEvent::Settled {
                prompt_uuid,
                stop_reason,
                usage,
            } => {
                high_water.fetch_max(1, Ordering::Relaxed);
                if let Some(reply) = pending.remove(prompt_uuid) {
                    let total_tokens = usage.input_tokens
                        + usage.output_tokens
                        + usage.cached_read_tokens
                        + usage.cached_write_tokens;
                    let usage = if *stop_reason == StopReason::Cancelled && total_tokens == 0 {
                        // A queued turn swept by cancel never ran, so upstream
                        // reports no usage for it (`turn.resolve({ stopReason:
                        // "cancelled" })`, no `usage`); a cancelled ACTIVE turn
                        // carries its accumulated spend instead.
                        None
                    } else {
                        Some(PromptUsage {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cached_read_tokens: usage.cached_read_tokens,
                            cached_write_tokens: usage.cached_write_tokens,
                            total_tokens,
                        })
                    };
                    let (flush_tx, flush_rx) = oneshot::channel();
                    // Phase 9 flush barrier: enqueued after every update this
                    // turn emitted, so the forwarding task writes them all
                    // before it resolves `flush_rx`; the prompt task awaits it
                    // before responding, keeping a `tool_call` ahead of the
                    // `result` on the wire.
                    let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                    let _ = reply.send(Ok(PromptReply {
                        stop_reason: stop_reason.as_str().to_string(),
                        usage,
                        updates: std::mem::take(&mut final_texts),
                        flush_rx: Some(flush_rx),
                    }));
                }
            }
            TurnEvent::Failed {
                prompt_uuid,
                kind,
                message,
            } => {
                if let Some(reply) = pending.remove(prompt_uuid) {
                    let _ = reply.send(Err(SessionError::PromptFailed {
                        kind: *kind,
                        message: message.clone(),
                        assistant_had_error,
                    }));
                }
            }
            TurnEvent::Activated { .. } => {
                high_water.fetch_max(1, Ordering::Relaxed);
            }
        }
    }
}

/// Whether a result frame carries an autonomous origin (mirrors `turn.rs`).
fn is_autonomous(result: &Value) -> bool {
    let Some(kind) = result
        .get("origin")
        .and_then(|o| o.get("kind"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    matches!(
        kind,
        "task-notification" | "peer" | "coordinator" | "observer" | "observer-activity"
    )
}

/// Whether a `task_updated` frame carries a terminal status.
fn is_terminal_task_update(line: &Value) -> bool {
    let status = line.get("status").and_then(Value::as_str).unwrap_or("");
    // `task_notification` is always terminal; a `task_updated` is terminal only
    // when it reports completed / failed / killed.
    if line.get("type").and_then(Value::as_str) == Some("system")
        && line.get("subtype").and_then(Value::as_str) == Some("task_notification")
    {
        return true;
    }
    matches!(status, "completed" | "failed" | "killed")
}

/// Whether an emitted update is top-level (non-subagent) assistant text — used
/// to set `emitted_assistant_text` (subagent chunks with `parent_tool_use_id`
/// are excluded).
fn is_top_level_text(update: &SessionUpdate, line: &Value) -> bool {
    // Subagent stream_event chunks carry a parent_tool_use_id.
    if line
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        return false;
    }
    matches!(update, SessionUpdate::AgentMessageChunk(_))
}

/// Emit a `commands_changed` system frame as an `AvailableCommandsUpdate`.
fn emit_commands_changed(
    line: &Value,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let commands = line.get("commands").cloned().unwrap_or(Value::Null);
    let terminal = line
        .get("terminal_commands")
        .cloned()
        .unwrap_or(Value::Null);
    let update = map::map_commands_changed(&commands, &terminal);
    let notif = SessionNotification::new(
        session_id.to_string(),
        SessionUpdate::AvailableCommandsUpdate(update),
    );
    let _ = update_tx.send(SessionOutbound::Update(notif));
}

/// Send a batch of updates to the client as `session/update` notifications.
fn emit_updates(
    updates: &[SessionUpdate],
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    for update in updates {
        let notif = SessionNotification::new(session_id.to_string(), update.clone());
        let _ = update_tx.send(SessionOutbound::Update(notif));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8.T7-style unit: a lagging trailing idle after the next echo is absorbed,
    /// not a false #825 fail. Exercises the machine via the actor-facing
    /// `TurnMachine` directly through `handle_session_frame`.
    #[tokio::test]
    async fn lagging_idle_absorbed_not_false_fail() {
        let mut machine = TurnMachine::new();
        let mut _map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
            HashMap::new();
        let (_tx, rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let _ = rx; // update sink unused here
        let (utx, _urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));

        // p1 echoed + result settles it (A's result).
        machine.enqueue(Turn::new("p1".into(), false));
        let _ = machine.on_echo("p1");
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"ok","usage":{}}), false);
        handle_events(&events, &mut pending, &utx, "s", &hw, false);

        // p2 queued + echoed (next echo).
        machine.enqueue(Turn::new("p2".into(), false));
        let _ = machine.on_echo("p2");

        // A's lagging trailing idle arrives while p2 is active+unsettled.
        let events = machine.on_idle();
        assert!(
            events.is_empty(),
            "a lagging trailing idle must be absorbed, not fail the next turn"
        );
        assert!(
            machine.has_active(),
            "p2 must remain active after the absorbed idle"
        );
    }

    /// 8.T9-style unit: a subagent hold followed by a followup result settles
    /// the held turn inside the turn.
    #[tokio::test]
    async fn subagent_hold_followup_settles() {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        let _ = machine.on_echo("p1");
        machine.on_task_started("sub1", true);
        // Result defers while the subagent is live.
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","usage":{}}), false);
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, TurnEvent::Settled { .. })),
            "must defer while the subagent is live"
        );
        machine.on_task_ended("sub1");
        // The followup autonomous result settles it.
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","origin":{"kind":"task-notification"},"usage":{}}), false);
        assert!(
            events.iter().any(
                |e| matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "p1")
            ),
            "the followup result must settle the held turn inside the turn"
        );
    }

    /// 8.T10-style unit: cancel with a queued prompt, then an echo-less next
    /// prompt — the queued prompt's late result is orphaned and never
    /// activates/settles the next prompt.
    #[tokio::test]
    async fn cancel_queued_echo_less_orphans_late_result() {
        let mut machine = TurnMachine::new();
        // p1 active, p2 queued (both pushed).
        machine.enqueue(Turn::new("p1".into(), false));
        let _ = machine.on_echo("p1");
        machine.enqueue(Turn::new("p2".into(), false));
        // Cancel sweeps p2 -> cancelled + orphan credit.
        let events = machine.cancel();
        assert!(
            events.iter().any(
                |e| matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "p2")
            ),
            "cancel must sweep the queued turn"
        );
        // p1 settles at idle.
        let _ = machine.on_idle();

        // p3 (echo-less, e.g. /context) enqueued.
        machine.enqueue(Turn::new("p3".into(), true));

        // p2's late result arrives: must be orphaned, NOT activate/settle p3.
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","usage":{}}), false);
        assert!(
            events.iter().all(
                |e| !matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "p3")
            ),
            "p2's late result must never settle p3"
        );
        assert!(
            !machine.has_active(),
            "p3 must not be activated by p2's orphaned result"
        );
    }

    /// 8.T11-style unit: stream end settles the active turn and rejects queued
    /// prompts; then the machine is closed.
    #[tokio::test]
    async fn stream_end_settles_active_and_rejects_queued() {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        let _ = machine.on_echo("p1");
        machine.enqueue(Turn::new("p2".into(), false));

        let events = machine.on_stream_end();
        assert!(
            events.iter().any(
                |e| matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "p1")
            ),
            "stream end must settle the active turn"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::Failed { prompt_uuid, kind, .. } if prompt_uuid == "p2" && *kind == FailureKind::SessionEnded)),
            "stream end must reject queued prompts with SessionEnded"
        );
    }

    /// 8.T2-style unit (agent-level, via the machine): an echo-less local-only
    /// prompt promotes and settles at its own result.
    #[tokio::test]
    async fn context_echo_less_promotes_and_settles() {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), true));
        // No echo; a result for /context arrives directly.
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"context output","usage":{"output_tokens":0}}), false);
        assert!(
            events.iter().any(
                |e| matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "p1")
            ),
            "an echo-less local-only command must settle at its own result"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::FinalText { .. })),
            "a local-only command's result text must be forwarded as FinalText"
        );
    }

    /// Review-05 row 2 (actor-level): a `session_state_changed{state:"running"}`
    /// frame routed through `handle_session_frame` overwrites a stale `"idle"`
    /// state, so a held turn cancelled mid-followup owes an interrupt trailer
    /// that absorbs the next prompt's trailing idle (no #825 false-fail).
    #[tokio::test]
    async fn session_state_running_frame_routes_through_actor() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
            HashMap::new();
        let (utx, _urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();

        // A active + held (deferred on a live subagent).
        machine.enqueue(Turn::new("pA".into(), false));
        machine.on_echo("pA");
        machine.on_task_started("sub1", true);
        let _ = machine.on_result(
            &json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","usage":{}}),
            false,
        );
        // The hold's own trailer idle arrives and is absorbed; A stays held and
        // the session records "idle".
        let _ = machine.on_idle();

        // Drive the `running` transition through the ACTOR frame path.
        handle_session_frame(
            json!({"type":"system","subtype":"session_state_changed","state":"running"}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
        );

        // Cancel the held turn: only if the routed state is "running" (not the
        // absorbed "idle") does cancel() owe an interrupt trailer.
        machine.cancel();

        // B enqueued + echoed (next prompt).
        machine.enqueue(Turn::new("pB".into(), false));
        machine.on_echo("pB");

        // The interrupt's trailing idle must be absorbed, not fail B (#825).
        let events = machine.on_idle();
        assert!(
            events.is_empty(),
            "the owed trailing idle must be absorbed, not fail B"
        );
        assert!(machine.has_active(), "B must remain active");
    }

    /// 10.T3 (INV-27, Unit — `session`): a permission round-trip is spawned, not
    /// awaited inline. Drives the REAL actor path (`run_loop` + a real
    /// `Control` over a duplex stdin): start a `can_use_tool` permission the
    /// client never answers, then deliver `Command::Cancel` while it is pending,
    /// and assert (a) the actor processes the cancel without being blocked, and
    /// (b) the pending permission resolves `Cancelled` — the R28 deny payload is
    /// written back to the control writer — and the prompt turn settles
    /// `cancelled`.
    #[tokio::test]
    async fn inv_27_no_inline_await() {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::timeout;

        // Build the real actor stack exactly as `Session::start` does: a
        // `Control` over a duplex stdin whose on-request handler routes
        // `can_use_tool` to the actor, plus the real `run_loop` actor.
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(
            a,
            control_in_rx,
            ControlOptions {
                on_request: Some(build_permission_request_handler(tx.clone())),
            },
        );
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx.clone(),
            update_tx,
            Duration::from_secs(30),
            "s1".into(),
            hw,
        ));

        // Reader: turn the duplex "stdin" (b) into whole lines.
        let (ltx, mut lrx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut reader = BufReader::new(b);
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

        // A prompt turn is active so we can observe it settle `cancelled`.
        let (preply_tx, preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            reply: preply_tx,
        })
        .unwrap();
        let _echo = timeout(Duration::from_secs(5), lrx.recv())
            .await
            .expect("user frame written to stdin")
            .expect("stream open");

        // Start a `can_use_tool` permission; the client (update_rx) never answers.
        let can_frame = json!({
            "type": "control_request",
            "request_id": "permreq1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "echo hi"},
                "tool_use_id": "toolu_01PERM",
                "agent_id": null,
                "permission_suggestions": [],
            }
        });
        control_in_tx.send(can_frame).unwrap();

        // The actor must spawn the round-trip and emit the `RequestPermission`
        // outbound (the agent layer would send `session/request_permission`).
        // The client never answers, so the permission stays pending — proof the
        // actor is NOT awaiting the round-trip inline. (An `Update(ToolCall)`
        // from `ensure_tool_call_emitted` precedes it on the channel.) The
        // outbound is KEPT alive (its `reply` oneshot is the round-trip's
        // pending sender): if we dropped it the round-trip would self-resolve as
        // a dropped error — a different deny path that would mask a missing
        // cancel-abort.
        let request_permission: SessionOutbound = loop {
            let outbound = timeout(Duration::from_secs(5), update_rx.recv())
                .await
                .expect("an outbound is emitted")
                .expect("outbound stream open");
            if matches!(outbound, SessionOutbound::RequestPermission { .. }) {
                break outbound;
            }
        };
        let _ = &request_permission;

        // Deliver `session/cancel` while the permission is still pending.
        tx.send(Command::Cancel).unwrap();

        // (a) + (b) The actor processes the cancel: it aborts the pending
        // permission round-trip, so control.rs's on-request handler resolves with
        // the R28 deny payload and writes it back to the control writer (stdin).
        let denied = find_deny_response(&mut lrx).await;
        assert_eq!(denied["type"], "control_response");
        assert_eq!(denied["response"]["response"]["behavior"], "deny");
        assert_eq!(
            denied["response"]["response"]["message"],
            "User refused permission to run tool"
        );
        assert!(
            denied["response"]["response"]["interrupt"].is_null(),
            "the R28 deny must not carry an interrupt"
        );

        // The actor remains responsive after the cancel: drive the trailing idle
        // so the active prompt turn settles `cancelled`.
        stream_tx
            .send(json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}))
            .unwrap();
        let reply = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("the prompt turn must settle")
            .expect("prompt reply resolved")
            .expect("the prompt turn settles Ok");
        assert_eq!(
            reply.stop_reason, "cancelled",
            "the prompt turn must settle cancelled after the pending permission is aborted"
        );
    }

    /// Read stdin lines until the R28 deny `control_response` is seen.
    async fn find_deny_response(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
        use std::time::Duration;
        use tokio::time::timeout;
        loop {
            let line = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a control_response is written to stdin")
                .expect("stream open");
            let v: Value = serde_json::from_str(&line).expect("line is JSON");
            if v.get("type").and_then(Value::as_str) == Some("control_response")
                && v.pointer("/response/response/behavior")
                    .and_then(Value::as_str)
                    == Some("deny")
            {
                return v;
            }
        }
    }

    /// 10.T5 (Regression): a subagent's permission request is attributed to its
    /// parent tool call. A `task_started` records the spawning Agent/Task call,
    /// and a `can_use_tool` with that `agent_id` emits a `tool_call` carrying the
    /// parent's `parentToolUseId` in `_meta.claudeCode`.
    #[tokio::test]
    async fn subagent_permission_attributed_to_parent() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;

        // The subagent starts, spawned by the Agent/Task call `toolu_PARENT`.
        handle_session_frame(
            json!({"type":"system","subtype":"task_started","task_id":"sub1","tool_use_id":"toolu_PARENT","subagent_type":"Task"}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
        );

        // A permission request from inside that subagent.
        let frame = json!({
            "type": "control_request",
            "request_id": "permreq1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "ls"},
                "tool_use_id": "toolu_SUB",
                "agent_id": "sub1",
                "permission_suggestions": [],
            }
        });
        let (reply_tx, _reply_rx) = oneshot::channel();
        let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        handle_permission(
            frame,
            reply_tx,
            &mut map_state,
            &mut live_background,
            &utx,
            "s",
            &mut pending_permissions,
        );

        // The emitted tool_call must carry the parent's parentToolUseId.
        let item = urx.recv().await.expect("a tool_call is emitted");
        let SessionOutbound::Update(notif) = item else {
            panic!("expected a session/update notification");
        };
        let json = serde_json::to_value(&notif.update).unwrap();
        assert_eq!(
            json["_meta"]["claudeCode"]["parentToolUseId"],
            json!("toolu_PARENT")
        );
        assert_eq!(json["_meta"]["claudeCode"]["toolName"], json!("Bash"));
        assert_eq!(json["toolCallId"], json!("toolu_SUB"));
    }

    /// Spawn a bare `run_loop` actor over a duplex stdin with an injected
    /// force-cancel grace, returning `(command_tx, stream_tx)` to drive it.
    /// Used by the phase-11 force-cancel / cancel-idempotency tests (INV-17,
    /// INV-22, 11.T5).
    fn spawn_actor(
        grace: Duration,
    ) -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedSender<Value>) {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            grace,
            "s1".into(),
            hw,
        ));
        // Drain the child stdin so the duplex never fills up (the actor writes
        // the prompt user frame and the interrupt request to it).
        tokio::spawn(async move {
            let mut reader = BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });
        (tx, stream_tx)
    }

    /// 11.T1 (INV-17) — a stream that never yields after cancel settles the
    /// active turn `cancelled` at the injected force-cancel grace (#680, R29),
    /// rather than hanging forever. Also asserts the settle did NOT happen
    /// immediately (it waited for the grace to elapse) — an implementation that
    /// force-cancels on the first cancel without waiting would otherwise pass
    /// this vacously.
    #[tokio::test]
    async fn inv_17_force_cancel_floor() {
        use tokio::time::timeout;
        const GRACE: Duration = Duration::from_millis(200);
        let (tx, stream_tx) = spawn_actor(GRACE);
        let (preply_tx, preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            reply: preply_tx,
        })
        .unwrap();
        // Feed the echo so p1 becomes active.
        stream_tx
            .send(json!({"type": "user", "uuid": "p1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        // Let the actor process the echo so p1 is ACTIVE (not merely queued)
        // before we cancel — otherwise `machine.cancel()` sweeps it as a queued
        // turn and settles immediately, never exercising the force-cancel floor.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // Cancel arms the force-cancel backstop (p1 is active, unsettled).
        let started = tokio::time::Instant::now();
        tx.send(Command::Cancel).unwrap();
        // Wedged: no result, no idle ever arrives. The grace elapses and the
        // backstop force-settles the active turn `cancelled`.
        let reply = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("the prompt must settle within the force-cancel grace")
            .expect("prompt reply resolved")
            .expect("the turn settles Ok");
        assert_eq!(
            reply.stop_reason, "cancelled",
            "a wedged stream settles cancelled at the force-cancel grace"
        );
        // The floor: it must not settle before the grace elapses. A buggy
        // force-cancel-on-first-cancel settles in ~0ms and fails this; the real
        // backstop waits the full grace.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= GRACE - Duration::from_millis(100),
            "the force-cancel backstop must wait the grace before settling \
             (elapsed {elapsed:?}, grace {GRACE:?})"
        );
    }

    /// 11.T2 (INV-22) — cancel is idempotent: repeated cancels SPACED across the
    /// force-cancel grace arm the deadline ONCE, never re-arming it to push the
    /// settle out. Runs under paused time so the settle instant is exact:
    /// cancel #1 at `t0` arms a single deadline at `t0 + G`; the later cancels
    /// (at `t0 + 0.2G` … `t0 + 0.8G`) must NOT re-arm it. A buggy
    /// re-arm-on-every-cancel implementation would push the deadline to the last
    /// cancel's `now + G` (= `t0 + 1.8G`), which the timing assertions below
    /// catch deterministically.
    #[tokio::test(start_paused = true)]
    async fn inv_22_cancel_idempotent() {
        use tokio::time::{advance, Instant};
        const G: Duration = Duration::from_millis(1000);

        let (tx, stream_tx) = spawn_actor(G);
        let (preply_tx, mut preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            reply: preply_tx,
        })
        .unwrap();
        stream_tx
            .send(json!({"type": "user", "uuid": "p1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        // Let the actor enqueue + echo p1 so it is active before we time cancel #1.
        tokio::task::yield_now().await;

        let t0 = Instant::now();
        // cancel #1 at t0: this is the ONLY cancel that may arm the deadline.
        tx.send(Command::Cancel).unwrap();
        // Let the actor process cancel #1 and arm the deadline at `t0 + G`
        // BEFORE any further time advances, so the armed instant is exactly t0.
        tokio::task::yield_now().await;

        // Further cancels spaced across the grace (0.2G, 0.4G, 0.6G, 0.8G).
        // A re-arming implementation would set deadline = last cancel's now + G
        // = (t0 + 0.8G) + G = t0 + 1.8G — far past the single-armed t0 + G.
        let spacings = [
            Duration::from_millis(200), // -> t0 + 0.2G
            Duration::from_millis(200), // -> t0 + 0.4G
            Duration::from_millis(200), // -> t0 + 0.6G
            Duration::from_millis(200), // -> t0 + 0.8G
        ];
        for d in spacings {
            advance(d).await;
            tokio::task::yield_now().await;
            tx.send(Command::Cancel).unwrap();
            tokio::task::yield_now().await;
        }

        // Poll in small steps until the turn settles, recording the virtual
        // instant (relative to t0) at which it did.
        let mut settled_at: Option<Duration> = None;
        let mut reply: Option<PromptReply> = None;
        for _ in 0..1000 {
            match preply_rx.try_recv() {
                Ok(Ok(r)) => {
                    settled_at = Some(Instant::now().duration_since(t0));
                    reply = Some(r);
                    break;
                }
                Ok(Err(_)) => panic!("the turn must settle Ok"),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    panic!("reply closed without resolving")
                }
            }
            advance(Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
        }

        let settled_at =
            settled_at.expect("the wedged turn must settle via the force-cancel backstop");
        let reply = reply.expect("the single prompt resolves exactly one reply");
        assert_eq!(reply.stop_reason, "cancelled");

        // (a) The single armed deadline fires at ~t0 + G — not before it, and
        // not pushed out by the later cancels.
        assert!(
            settled_at >= G,
            "the settle must not precede the single armed deadline (settled at {settled_at:?}, G = {G:?})"
        );
        assert!(
            settled_at <= G + Duration::from_millis(200),
            "the settle must happen at ~the single armed deadline, not pushed out \
             (settled at {settled_at:?}, expected ~{G:?})"
        );
        // (b) Strictly BEFORE where a re-arm-at-last-cancel (t0 + 0.8G + G = 1.8G)
        // deadline would fire — proving the deadline was armed exactly once.
        assert!(
            settled_at < G + Duration::from_millis(800),
            "repeated cancels must not re-arm the deadline to the last cancel's now + G \
             (settled at {settled_at:?}; a re-arming implementation would fire at ~{G:?} + 800ms)"
        );
        // The oneshot resolved exactly once (a single prompt yields exactly one
        // `Settled`/reply), so no double-settle of the active turn.
    }

    /// 11.T5 (regression) — a cancel followed by a new prompt on the SAME
    /// session works: after the cancelled turn settles, a fresh prompt
    /// activates, runs and settles normally (the cancelled flag is cleared on
    /// activation, `activate()`).
    #[tokio::test]
    async fn cancel_then_new_prompt_same_session() {
        use tokio::time::timeout;
        let (tx, stream_tx) = spawn_actor(Duration::from_millis(300));

        // p1: active, cancel, then the trailing idle settles it `cancelled`.
        let (p1_tx, p1_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            reply: p1_tx,
        })
        .unwrap();
        stream_tx
            .send(json!({"type": "user", "uuid": "p1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        tx.send(Command::Cancel).unwrap();
        // The trailing idle settles the cancelled active turn.
        stream_tx
            .send(json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}))
            .unwrap();
        let r1 = timeout(Duration::from_secs(5), p1_rx)
            .await
            .expect("p1 settles")
            .expect("p1 resolved")
            .expect("p1 Ok");
        assert_eq!(r1.stop_reason, "cancelled");

        // p2: a fresh prompt on the same session activates, runs and settles.
        let (p2_tx, p2_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p2".into(),
            frame: json!({"type": "user", "uuid": "p2", "message": {}}),
            is_local_only: false,
            reply: p2_tx,
        })
        .unwrap();
        stream_tx
            .send(json!({"type": "user", "uuid": "p2", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "second"}]}}))
            .unwrap();
        stream_tx
            .send(json!({"type": "result", "subtype": "success", "is_error": false,
                         "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}))
            .unwrap();
        stream_tx
            .send(json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}))
            .unwrap();
        let r2 = timeout(Duration::from_secs(5), p2_rx)
            .await
            .expect("p2 settles")
            .expect("p2 resolved")
            .expect("p2 Ok");
        assert_eq!(
            r2.stop_reason, "end_turn",
            "a new prompt on the same session settles normally after a cancel"
        );
    }
}
