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
//!   phase-7 mapper + `emitted_assistant_text` tracking). Steered echoes
//!   (replayed `user` frames carrying a steer's uuid) feed `machine.on_echo`,
//!   and injected `_session/steering` steers call `machine.mark_steered`
//!   before the frame is pushed (R2).
//! - **`FinalText{text}`** is emitted as an `agent_message_chunk` *before* the
//!   same batch's `Settled` resolves the prompt.
//! - The one-active-turn high-water counter (INV-14) is keyed on
//!   `Activated` / `Settled` events.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionNotification, SessionUpdate, TextContent,
    ToolCallStatus,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::air::AirClient;
use crate::async_tasks::AsyncTaskRuntime;
use crate::control::{Control, ControlOptions, InitializeOptions, RequestHandler};
use crate::dispatch::{self, Route, StreamMsg};
use crate::map::{self, MapState};
use crate::permission;
use crate::permission_options;
use crate::process::{self, SpawnOptions, Timings};
use crate::session_ext::SessionExt;
use crate::turn::{FailureKind, StoppedWorkVerdict, Turn, TurnEvent, TurnMachine};

/// The two answers of [`Session::remote_control_checked`] (NEW-2).
pub struct RemoteControlChecked {
    /// Whether the actor took the command, before it spawns the control call.
    pub accepted: oneshot::Receiver<Result<(), SessionError>>,
    /// The prompt's reply, once the control call returned.
    pub reply: oneshot::Receiver<Result<PromptReply, PromptFailure>>,
}

/// Errors produced by the session actor.
#[derive(Debug, Clone, thiserror::Error)]
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
    /// The child exited after an error `result`. `sdk.mjs` `readMessages`
    /// replaces the transport error with
    /// `Error("Claude Code returned an error result: <text>")` (F3a (f)).
    #[error("Claude Code returned an error result: {text}")]
    ChildErrorResult { text: String },
    /// The child died and the client reads AIR session failures: a queued turn is
    /// rejected `internalErrorForClient({errorKind:"transport_lost"})`, without
    /// the raw detail (`acp-agent.ts:5584,8325-8330`).
    #[error("the connection to Claude was lost")]
    TransportLost,
    /// The worker announced its shutdown and the stream then ended under the
    /// turn of a client that does not read AIR session failures
    /// (`internalErrorForClient({errorKind:"worker_shutdown"})`).
    #[error("the Claude worker is shutting down")]
    WorkerShutdown,
    /// A clear-context restart could not start the fresh context: the pending
    /// prompt is rejected with the error that stopped it (a plain `Error`, which
    /// the router reports as `Internal error` with the message as its details).
    #[error("{0}")]
    RestartFailed(String),
    /// A defining `setModel` failure on a fresh session (`session-model.ts:574`):
    /// Node throws the SDK error, which the ACP router reports as
    /// `Internal error` with the message in `data.details`.
    #[error("{0}")]
    ModelSwitchFailed(String),
    /// A refusal of a well-formed request (`RequestError.invalidRequest`), e.g.
    /// `/remote-control` while Claude is working (S-03): `-32600` with the
    /// message as its own text.
    #[error("{0}")]
    InvalidRequest(String),
    #[error("failed to start the session: {0}")]
    Start(String),
}

/// A rejected `session/prompt`: the error plus the acknowledgement the prompt
/// task fires right after it wrote the error response, the counterpart of
/// [`PromptReply::ack`] (the forwarding task serialises every response of the
/// session on it, so a rejection never swaps places with a neighbour).
#[derive(Debug)]
pub struct PromptFailure {
    pub error: SessionError,
    pub ack: Option<oneshot::Sender<()>>,
}

impl From<SessionError> for PromptFailure {
    fn from(error: SessionError) -> Self {
        Self { error, ack: None }
    }
}

impl std::ops::Deref for PromptFailure {
    type Target = SessionError;
    fn deref(&self) -> &SessionError {
        &self.error
    }
}

/// The SDK's prefix for a child-exit error that follows an error `result`
/// (`sdk.mjs` `readMessages`).
pub(crate) const CHILD_EXIT_PREFIX: &str = "Claude Code returned an error result: ";

/// The [`SessionError`] a failed turn is answered with — the one identity that
/// both v1's rejection and v2's `idle error` read (NEW-5): the auth rejection
/// when `auth` (the derived failure category or the kind itself is
/// `auth_required`), the child-exit rewrite (F3a (f)), the worker-shutdown
/// failure, else the turn's own failure.
fn failed_turn_error(
    kind: FailureKind,
    message: &str,
    auth: bool,
    air_session_failure: bool,
    assistant_had_error: bool,
) -> SessionError {
    if auth {
        return SessionError::PromptFailed {
            kind: FailureKind::AuthRequired,
            message: message.to_string(),
            assistant_had_error,
        };
    }
    match message.strip_prefix(CHILD_EXIT_PREFIX) {
        // An AIR client reads the queued turns' rejection as
        // `internalErrorForClient({errorKind:"transport_lost"})`
        // (`acp-agent.ts:8325-8330`).
        Some(_) if air_session_failure => SessionError::TransportLost,
        Some(text) => SessionError::ChildErrorResult {
            text: text.to_string(),
        },
        // `internalErrorForClient({errorKind:"worker_shutdown"})`.
        None if kind == FailureKind::WorkerShutdown => SessionError::WorkerShutdown,
        None => SessionError::PromptFailed {
            kind,
            message: message.to_string(),
            assistant_had_error,
        },
    }
}

/// Record the AIR `sessionFailure` of a failed turn (`createSessionFailure` ->
/// `prepare`, `recordActive`, `acp-agent.ts:5591-5593`) and return the
/// `jetbrains.air` meta that carries it on the turn's answer/idle.
fn record_air_session_failure(
    failures: &mut failures::SessionFailures,
    session_id: &str,
    prompt_uuid: &str,
    kind: crate::session_failure::ClaudeFailureKind,
    title: &str,
) -> Value {
    let prepared = failures.prepare(
        session_id,
        kind,
        &failures::FailureOptions {
            turn_id: Some(prompt_uuid.to_string()),
            title: Some(title.to_string()),
            ..failures::FailureOptions::default()
        },
    );
    let meta = crate::session_failure::session_failure_meta(&prepared.failure);
    failures.record_active(prepared);
    meta
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
    /// An untyped AIR `session/update` (item 3.3), value = `{"sessionId",⋯,"update",{⋯}}`,
    /// forwarded as `UntypedMessage::new("session/update", value)` (async-task
    /// spawns/progress/state-updates).
    RawUpdate(Value),
    /// A barrier: every `Update` sent before this has been forwarded. Resolving
    /// the `oneshot` signals the prompt task it may now write its response.
    Flush(oneshot::Sender<()>),
    /// A [`Self::Flush`] barrier for a prompt response that must also keep its
    /// order against the other responses of the same settle batch: after
    /// resolving `done` the forwarding task waits (bounded) for `ack`, which the
    /// prompt task fires right after it queued its response. Node resolves
    /// several settled `session/prompt` promises in FIFO order; the prompt tasks
    /// wake concurrently, so without this the response order would be a race.
    FlushAcked {
        done: oneshot::Sender<()>,
        ack: oneshot::Receiver<()>,
    },
    /// An arbitrary client notification (the raw `_claude/sdkMessage` ext
    /// notification, U-10), forwarded as `UntypedMessage::new(method, params)`.
    ExtNotification { method: String, params: Value },
    /// The Claude process died unexpectedly (`processDied`): the agent layer
    /// forgets the session so a later `session/load` starts a new child from the
    /// transcript (`delete this.sessions[...]`, `acp-agent.ts:8300-8320`).
    ProcessDied,
    /// Send a `session/request_permission` request to the client and await its
    /// outcome (phase 10). The agent's forwarding task resolves `reply` with the
    /// raw JSON-RPC `result` (`Ok`) or a transport error (`Err`).
    RequestPermission {
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Send a `session/create_elicitation` (wire method `elicitation/create`,
    /// F23a/F23b) request to the client and await its answer. The forwarding
    /// task spawns the round trip, so neither the actor nor the connection loop
    /// waits on it; dropping `reply` cancels the request (`$/cancel_request`).
    CreateElicitation {
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// `elicitation/complete` (F23b/N-37): a notification, no answer expected.
    CompleteElicitation { params: Value },
    /// What the session index learns of this session (M-65): the forwarding task
    /// applies it to the connection's board.
    Board(BoardEvent),
    /// The intake answer of an ACP v2 prompt (F39): the forwarding task resolves
    /// `reply` only after every update ahead of it reached the wire, so the
    /// client sees the prompt's `user_message` and `running` state before the
    /// response (upstream's `send` then `resolve`).
    V2Intake {
        prompt_uuid: String,
        reply: crate::turn_events::V2InsertedSender,
        result: Result<String, crate::turn_events::V2IntakeError>,
    },
}

/// A fact about a session this connection runs that its row in the session index
/// shows (`Session.lastSessionState`, `lastTurnEndedAt`, `lastTurnFailed` and
/// `SessionIndexConnection.onQueryFailed` upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardEvent {
    /// The CLI's `session_state_changed`: `idle`, `running` or `requires_action`
    /// (a permission request is waiting). `running` also starts a turn, which
    /// ends an earlier failed query's mark (`acp-agent.ts:6089-6094`).
    State(crate::session_index::activity::ActivityState),
    /// The user turn ended with this outcome: an error result, or any other result
    /// (`session.lastTurnFailed = !cancelled && (is_error || subtype !==
    /// "success")`, `acp-agent.ts:7290`; an AIR failure answer sets it too,
    /// `acp-agent.ts:5561`).
    TurnFailed(bool),
    /// The query of the session failed (the child died): its row shows `error`
    /// until its transcript changes (`onQueryFailed`, `acp-agent.ts:8247`).
    QueryFailed,
}

impl BoardEvent {
    /// Record the event on `board` for `session_id`.
    pub fn apply(
        self,
        board: &crate::session_index::connection::OwnSessionBoard,
        session_id: &str,
    ) {
        use crate::session_index::activity::ActivityState;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0);
        match self {
            BoardEvent::State(state) => {
                board.note_session_state(session_id, state, now_ms);
                if state == ActivityState::Running {
                    board.note_turn_started(session_id);
                }
            }
            BoardEvent::TurnFailed(failed) => {
                board.update(session_id, |own| own.last_turn_failed = failed);
            }
            BoardEvent::QueryFailed => {
                board.note_turn_failed(session_id);
                board.note_query_failed(session_id, now_ms);
            }
        }
    }
}

/// The result of a settled `session/prompt`.
pub struct PromptReply {
    /// The turn's ACP `stopReason`.
    pub stop_reason: String,
    /// Token usage reported on the response (the fixture's
    /// `result.usage` shape).
    pub usage: Option<PromptUsage>,
    /// The `_meta.quota` totals object (2.12 / E-6), `Some` iff `had_usage`
    /// (never on the queued-sweep cancel). Written by the agent layer as
    /// `result["_meta"]["quota"]`.
    pub quota: Option<Value>,
    /// The turn's streaming updates (e.g. a `FinalText` `agent_message_chunk`)
    /// that must be forwarded to the client *before* the prompt response.
    pub updates: Vec<SessionNotification>,
    /// Resolves once every session update emitted before this turn settled has
    /// been forwarded to the client (phase 9). The prompt task awaits it before
    /// writing the response so a `tool_call` lands ahead of the `result`.
    pub flush_rx: Option<oneshot::Receiver<()>>,
    /// Fired by the prompt task right after it queued the response, releasing the
    /// forwarding task's [`SessionOutbound::FlushAcked`] wait (response order).
    pub ack: Option<oneshot::Sender<()>>,
}

impl std::fmt::Debug for PromptReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptReply")
            .field("stop_reason", &self.stop_reason)
            .field("usage", &self.usage)
            .field("quota", &self.quota)
            .field("updates", &self.updates)
            .field("flush_rx", &self.flush_rx.as_ref().map(|_| "<oneshot>"))
            .field("ack", &self.ack.as_ref().map(|_| "<oneshot>"))
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

/// Writes a client-chosen title (`setExplicitTitle`'s `persist`): returns what it
/// stored, if anything. Runs off the actor, after a title generation in flight.
pub type ExplicitPersist = Box<
    dyn FnOnce() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<String>, String>> + Send>,
        > + Send,
>;

/// The title state a rename reads from the session (`shownTitle`, `storedTitle`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleSnapshot {
    /// The title last published, as the client shows it.
    pub shown: Option<String>,
    /// The title the CLI holds.
    pub stored: Option<String>,
}

/// A request sent by the ACP client to the session actor (D4). Each request
/// carries a `oneshot` reply resolved by the actor when the operation settles.
pub(crate) enum Command {
    /// Enqueue a prompt turn (8.7). `uuid` is stamped into `frame`'s `uuid`
    /// field by the caller; the actor registers `uuid -> reply`, enqueues the
    /// turn and pushes `frame` to `claude` immediately.
    Prompt {
        uuid: String,
        frame: Value,
        is_local_only: bool,
        /// The prompt's plain ACP `type:"text"` chunks joined by `\n` — the
        /// title context source (`session-titles.ts` `onPrompt`), not the
        /// converted SDK frame (which embeds file contents and links).
        prompt_text: String,
        reply: oneshot::Sender<Result<PromptReply, PromptFailure>>,
    },
    /// The `_meta` of a prompt that is about to be enqueued (AG-3).
    PromptMeta { uuid: String, meta: Value },
    /// Enqueue an ACP v2 prompt (F39): the same turn machinery, but the client
    /// is answered at intake with the prompt's message id instead of at its
    /// settle, and the turn's work is reported as `state_update`s. `content` is
    /// the v2 `prompt` array, echoed back as the `user_message`.
    PromptV2 {
        uuid: String,
        frame: Value,
        is_local_only: bool,
        prompt_text: String,
        content: Value,
        inserted: crate::turn_events::V2InsertedSender,
        /// The turn's end, for the v1 bookkeeping the v2 prompt task runs
        /// (`turn_settled`, the sign-out respawn; NEW-3).
        ended: oneshot::Sender<crate::turn_events::V2TurnEnd>,
    },
    /// Register a `/mcp` prompt (rust-air-gaps G12, Decision 21) before the
    /// `Prompt` it belongs to. The actor spawns the control run after pushing
    /// the prompt frame, then replaces the CLI text with the produced Markdown.
    McpPrompt {
        uuid: String,
        command: crate::mcp_command::McpCommand,
    },
    /// Start a `/usage` turn's structured producer at activation (F24 / S-01,
    /// `startsAtActivation: true`): the run loop spawns the `get_usage` control
    /// request with its 5 s timeout.
    StartUsage { uuid: String },
    /// Re-entry from the spawned `/usage` producer: the replacement (or `None`
    /// to keep the CLI text).
    UsagePrepared {
        uuid: String,
        markdown: Option<String>,
    },
    /// Re-entry from the spawned `/mcp` control run: the replacement is ready
    /// (or `None` to keep the CLI text).
    McpPrepared {
        uuid: String,
        prepared: crate::mcp_command::McpPrepared,
    },
    /// Cancel the active prompt (8.9): `machine.cancel()` then send an
    /// `interrupt` control_request.
    Cancel,
    /// `session/close` (rust-air-gaps G6, Decision 14): cancel like
    /// `session/cancel`, send the `interrupt` (even when idle), then tear the
    /// session down. `reply` resolves once the updates are on the wire.
    Close { reply: oneshot::Sender<()> },
    /// The close's `interrupt` was written (or timed out): settle what is left,
    /// resolve `reply` behind a flush barrier and end the actor.
    CloseDone { reply: oneshot::Sender<()> },
    /// The session's foreground state now (F39; `foregroundState`,
    /// `acp-agent.ts:9636-9640`), for a v2 `session/resume` that must report the
    /// work it resumes before its response.
    ForegroundState {
        reply: oneshot::Sender<crate::turn_events::ForegroundState>,
    },
    /// The `interrupt` control_response receipt came back (phase 11): carry its
    /// `still_queued` (or `None` for a bare `{}` receipt) so the actor can
    /// reconcile the orphan-credit count (`machine.reconcile_orphan_receipt`,
    /// INV-23).
    InterruptReceipt {
        cancel_id: u64,
        still_queued: Option<Vec<String>>,
    },
    /// Handle an inbound `can_use_tool` control_request (phase 10). `reply`
    /// resolves with the `control_response` frame (the R28 deny / allow
    /// payload) once the client's permission outcome is known (10.1, D14).
    Permission {
        frame: Value,
        reply: oneshot::Sender<Value>,
    },
    /// A `can_use_tool` request that waited for the consumer to reach its tool use
    /// in the stopped-work window (F16b, B-62): the stream reached it, or the wait
    /// ran out. Re-enters as a [`Self::Permission`] unless the tool use was
    /// dropped as the cancelled work's.
    PermissionReleased {
        frame: Value,
        reply: oneshot::Sender<Value>,
    },
    /// An inbound MCP `elicitation` control request (F23b/R-10). The actor spawns
    /// the client round trip; the answer re-enters as
    /// [`Self::ElicitationControlAnswered`] and resolves `reply`.
    ElicitationControl {
        frame: Value,
        reply: oneshot::Sender<Value>,
    },
    /// The spawned MCP-elicitation round trip finished.
    ElicitationControlAnswered { request_id: String, response: Value },
    /// An inbound `request_user_dialog` control request (F23b/R-08).
    UserDialogControl {
        frame: Value,
        reply: oneshot::Sender<Value>,
    },
    /// The spawned refusal-fallback dialog round trip finished.
    UserDialogAnswered { request_id: String, response: Value },
    /// The spawned AskUserQuestion elicitation round trip finished (F23a/U-42).
    AskUserQuestionAnswered { response: Value, request_id: String },
    /// The CLI withdrew an inbound request (`control_cancel_request`, or the
    /// control channel rebound to a fresh child on a clear-context restart): the
    /// actor aborts the client round trip and ends the child's awaiting-user
    /// token as withdrawn.
    ElicitationWithdrawn { request_id: String },
    /// A permission round trip ended without the client answering (the request
    /// errored or was dropped): the child's awaiting-user token ends without an
    /// abort, so the child reports `running` again.
    AwaitEnded { request_id: String },
    /// Start MCP OAuth for the ACP servers of this session (F23c/A-01).
    /// `configured` is true for the session-start call, which also records the
    /// session's ACP server names for the `/mcp reconnect` follow-up.
    StartMcpAuth {
        servers: Vec<String>,
        configured: bool,
    },
    /// The spawned MCP OAuth run finished (clears the single-flight guard).
    McpAuthFinished,
    /// B-29: the ACP messageId -> SDK uuid map (`Session.messageIdToUuid`),
    /// read by `session/fork` to resolve an AIR fork point.
    MessageIds {
        reply: oneshot::Sender<crate::message_ids::MessageIdMap>,
    },
    /// B-54: merge the history replay's messageId map into the session's.
    SeedMessageIds {
        map: crate::message_ids::MessageIdMap,
    },
    /// Whether the adapter may answer `/remote-control` itself (F24/S-03): true
    /// unless Claude Code offers its own `remote-control` command.
    RemoteControlAvailability { reply: oneshot::Sender<bool> },
    /// A `/remote-control` prompt (F24/S-03): run `enableRemoteControl` off the
    /// actor and answer the prompt with its text. `accepted` reports whether the
    /// actor took the command, before the control call starts, so a v2 answer
    /// can refuse it before the prompt's `user_message` (NEW-2).
    RemoteControlPrompt {
        command: crate::control::RemoteControlCommand,
        reply: oneshot::Sender<Result<PromptReply, PromptFailure>>,
        accepted: Option<oneshot::Sender<Result<(), SessionError>>>,
    },
    /// The spawned `/remote-control` control call finished.
    RemoteControlDone {
        text: String,
        reply: oneshot::Sender<Result<PromptReply, PromptFailure>>,
    },
    /// The bounded wait for the consumer to reach `tool_use_id` ran out.
    ToolUseWaitElapsed { tool_use_id: String },
    /// Whether `session/cancel` also stops the background `local_workflow` tasks
    /// the interrupt spared (`Session.stopsWorkflowsOnCancel`, F16c / B-42).
    StopsWorkflowsOnCancel(bool),
    /// The connection serves ACP v2 (F39): the session reports its foreground
    /// work as `state_update`s and answers prompts at intake.
    V2Reporting(bool),
    /// Where the actor leaves its final modes + config options when it ends, so
    /// the handle can still answer for a session whose stream died (Node's
    /// `Session` object outlives its query stream).
    FinalStateSink(watch::Sender<Option<LiveState>>),
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
    /// Whether the session has an unsettled turn (`turnQueue.some(!settled)`),
    /// which covers a turn a steer started without a `session/prompt`. A provider
    /// update and the sign-out respawn wait for it.
    UnsettledTurn { reply: oneshot::Sender<bool> },
    /// `_session/async_task/stop` (rust-air-gaps G1, Decision 12): claim the
    /// stop synchronously, then ask the CLI to `stop_task` from a spawned task.
    /// `reply` resolves `Ok(true)` once the updates are on the wire, `Ok(false)`
    /// for an unknown / unstoppable task, `Err(message)` when the CLI refused.
    StopAsyncTask {
        task_id: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// A `session/cancel` for a native subagent's child session
    /// (`cancelChild`, `acp-agent.ts:8396-8409`): stop the child's task without
    /// cancelling the parent. `Ok(true)` when the session is a child of this
    /// session's runtime, `Ok(false)` otherwise (the caller falls through to the
    /// normal cancel).
    CancelChild {
        session_id: String,
        reply: oneshot::Sender<bool>,
    },
    /// The CLI confirmed `stop_task` (re-entry from the spawned request).
    TaskStopped {
        task_id: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// The CLI failed `stop_task`: give the claim back and fail the request.
    ReleaseStop {
        task_id: String,
        error: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// Update context window size from background get_context_usage. `model_id`
    /// is the model the request was issued for: the answer is dropped if the
    /// session switched models meanwhile (F5b/A-10 `stillCurrent`).
    UpdateContextWindow { size: u64, model_id: String },
    /// The forwarder could not write a raw (AIR) `session/update` to the client:
    /// the async-task fields recorded as published are rolled back so a retry
    /// carries them again (`recordPublished` only after a resolved send, U-75).
    UpdateSendFailed,
    /// Re-entry from the spawned `generate_session_title` control request (Decision 13):
    /// the generated title (or `None` if it failed/empty).
    TitleGenerated {
        title: Option<String>,
        fallback: Option<(String, Option<String>)>,
    },
    /// Re-entry from the spawned, off-actor transcript lookup at turn end
    /// (Decision 13): the resolved transcript path (cached once found) and the
    /// stored custom title / summary. `cancelled` is the idle transition's
    /// stopped-work snapshot.
    TitleInfo {
        transcript: Option<std::path::PathBuf>,
        info: Option<crate::session_store::StoredSessionInfo>,
        cancelled: bool,
        /// `explicit.renames` when the lookup started: a rename that ran during
        /// the read makes the stored title stale.
        renames: u64,
    },
    /// A client chose the title (`_session/rename`, archive, unarchive;
    /// `SessionTitles.setExplicitTitle`). The title settles for good; the change
    /// waits for a title generation in flight so that its persisted title cannot
    /// land after this one, then runs `persist` and publishes.
    SetExplicitTitle {
        publish: crate::session_titles::ExplicitPublish,
        persist: ExplicitPersist,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// `persist` of an explicit title change finished.
    ExplicitPersisted {
        outcome: Result<Option<String>, String>,
        publish: crate::session_titles::ExplicitPublish,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// The title last published and the title the CLI holds.
    TitleSnapshot {
        reply: oneshot::Sender<TitleSnapshot>,
    },
    /// Wait until every update queued so far was forwarded (a title update must
    /// precede the response of the rename that published it).
    FlushUpdates { reply: oneshot::Sender<()> },
    /// The CLI took this title (`rememberStoredTitle`).
    RememberStoredTitle { title: String },
    /// How a stored title reads on the client (`clientTitle`).
    SetClientTitle {
        map: crate::session_titles::ClientTitle,
    },
    /// Initialize session config state after initialize handshake.
    InitConfig {
        config: Box<crate::session_config::SessionConfigState>,
    },
    /// Seed the actor's slash-command list from the initialize handshake (D5 /
    /// Decision 9). The proactive advertisement already went out; this is the
    /// source for every later `commands_changed` / `system/init` re-advertise.
    InitCommands { commands: Vec<Value> },
    /// Read the actor's current command list (B-118: an already-live
    /// `session/resume` re-advertises what the actor holds, not the
    /// creation-time snapshot).
    CommandsSnapshot { reply: oneshot::Sender<Vec<Value>> },
    /// Set permission mode (`session/set_mode`).
    SetMode {
        mode_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Re-entry after `set_permission_mode` control request for `SetMode`.
    ModeApplied {
        effective_mode: String,
        requested_mode: String,
        result: Result<(), String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// The client's answer to a pending permission request, resolved by the
    /// actor. Node evaluates `isAutoUnavailable` AFTER the client answers
    /// (`acp-agent.ts:10219`), against the model current at that moment, so the
    /// actor owns the decision (it holds the live config). It applies
    /// `applyPermissionFallback` (`session-mode.ts:196-221`), publishes the
    /// advisory warning for any rewrite, publishes the ExitPlanMode mode
    /// updates, and then resolves `reply` (Node publishes before returning the
    /// permission result). Carrying `reply` here means a `Cancel` that aborts
    /// the permission task after the client answered cannot drop the allow
    /// (INV-27).
    PermissionAnswered {
        value: Value,
        ctx: Arc<permission_options::OptionsContext>,
        request_id: String,
        tool_use_id: String,
        tool_input: Value,
        reply: oneshot::Sender<Value>,
    },
    /// Set config option (`session/set_config_option`).
    SetConfigOption {
        config_id: String,
        value: Value,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
    },
    /// Re-entry after `set_permission_mode` control request for mode config option.
    ConfigModeApplied {
        requested_mode: String,
        effective_mode: String,
        result: Result<(), String>,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
    },
    /// Re-entry after `set_model` control request. `notify_client` publishes
    /// `config_option_update` after the bookkeeping (PostModelSwitch sync, which has
    /// no caller to return the options to); the user path returns them in `reply`.
    ///
    /// `inline` marks the external switches the consumer loop waits for: the
    /// refusal fallback (`acp-agent.ts:6702` awaits
    /// `syncModelAfterExternalSwitch`), unlike the PostModelSwitch hook, which
    /// runs it detached (`setImmediate`, `acp-agent.ts:11328-11337`) while the
    /// loop keeps draining the stream.
    ModelApplied {
        model_val: String,
        result: Result<(), String>,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
        notify_client: bool,
        inline: bool,
    },
    /// Re-entry after `apply_flag_settings` control request.
    EffortApplied {
        effort_val: String,
        result: Result<(), String>,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
    },
    /// Re-entry after the `apply_flag_settings {fastMode}` control request of a
    /// `session/set_config_option fast` (`applyFastMode`, `acp-agent.ts:10771-10781`).
    FastModeApplied {
        enabled: bool,
        result: Result<(), String>,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
    },
    /// Re-entry after the `apply_flag_settings {agent}` control request of a live
    /// Agent switch (`acp-agent.ts:10676-10688`).
    AgentApplied {
        value: String,
        result: Result<(), String>,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
    },
    /// Re-entry after the model switch's `set_permission_mode` (auto clamp) and
    /// `apply_flag_settings` (effort sync) control requests. Carries the outcome
    /// so the switch can restore or drop the effort option before replying
    /// (`acp-agent.ts:10606-10668`).
    ModelSwitchSynced {
        /// The switched-to model value.
        model_val: String,
        /// The effort apply outcome; `None` when no sync was needed.
        effort_result: Option<Result<(), String>>,
        /// The user pin before the switch (`session.effortPinnedLevel`).
        pinned_before: Option<String>,
        /// The last applied effort before the switch (`appliedEffortBeforeSwitch`).
        applied_before: Option<String>,
        /// Whether the old pin survives on the new model.
        effort_pinned_for_new: bool,
        /// The concrete displayed effort the sync applied.
        new_effort: Option<String>,
        /// Whether the client opted into the AIR `recommendedValue` capability.
        use_recommended_value: bool,
        /// The auto-mode downgrade target, if the switch invalidated Auto.
        downgraded_to: Option<String>,
        /// Whether the fallback warning is still to be shown.
        warn: bool,
        /// Whether the client is told (`config_option_update`) instead of the
        /// caller's reply carrying the options.
        notify_client: bool,
        /// Whether the switch is one the consumer loop waits for (the refusal
        /// fallback), so the stream stays gated until its round trips settle.
        inline: bool,
        /// Whether the switch actually changed the model (background context
        /// window refresh).
        model_changed: bool,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
    },
    /// Fetch the actor's LIVE modes + config options (MAJOR 5: `session/resume`
    /// on a live session must return the current state, not a creation-time copy).
    GetState {
        reply: oneshot::Sender<
            Result<
                (
                    crate::session_config::ModesState,
                    Vec<crate::session_config::ConfigOption>,
                ),
                String,
            >,
        >,
    },
    /// Seed the Task* state rebuilt by a `session/load` replay (PC-2.3), plus
    /// the last plan published during it for the AIR dedupe.
    SeedTaskState {
        state: Vec<(String, crate::hooks::TaskStateEntry)>,
        last_published: Option<String>,
        reply: oneshot::Sender<()>,
    },
    /// Read the session's failure book (a `session/load` replay of a live
    /// session continues from its revisions).
    FailuresSnapshot {
        reply: oneshot::Sender<failures::SessionFailures>,
    },
    /// Adopt the failure book a `session/load` replay rebuilt (N-53 / V-15):
    /// the restored records' revisions and the one active usage limit.
    SeedFailures {
        book: failures::SessionFailures,
        reply: oneshot::Sender<()>,
    },
    /// What the actor needs to bring up a fresh child for a clear-context
    /// restart (sent once by [`Session::start`]).
    InitRestarter(Box<clear_context::Restarter>),
    /// A clear-context restart finished its bring-up: the fresh child, or why
    /// there is none. `id` names the restart; a result of one the actor gave up
    /// is dropped, which takes the fresh child down.
    RestartDone {
        id: u64,
        result: clear_context::RestartResult,
    },
    /// Inbound `hook_callback` control_request (Decision 12).
    HookCallback {
        request_id: String,
        callback_id: String,
        input: Value,
        tool_use_id: Option<String>,
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
    /// The parsed AIR client declaration (Decision 11 / item 3.2), threaded from
    /// the connection Registry. Async-task updates (item 3.5) run only when the
    /// client advertised AIR `asyncTasks`.
    pub air: Option<AirClient>,
    /// The full `initialize.clientCapabilities` parse (ROADMAP F2a), threaded
    /// from the connection Registry.
    pub caps: crate::client_caps::ClientCapabilities,
    /// `_meta.claudeCode.emitRawSDKMessages` (U-10): raw SDK messages are
    /// forwarded as the `_claude/sdkMessage` ext notification when enabled.
    pub raw_sdk_messages: crate::sdk_options::RawSdkMessages,
    /// Environment snapshot for hermetic adapter reads.
    pub env: crate::env_snapshot::EnvSnapshot,
    /// The adapter logger (`CLAUDE_AGENT_LOGS`); `None` when logging is off.
    pub logger: Option<std::sync::Arc<crate::logger::Logger>>,
    /// The model ids a `providers/set` gateway listed and the Bedrock region prefix
    /// picked from them (O-74): the model picker offers only the SDK models they
    /// match. `None`: nothing is narrowed.
    pub gateway_models: Option<(Vec<String>, Option<String>)>,
    /// The connection serves ACP v2 (F39): the session reports its foreground
    /// work as `state_update`s, echoes a prompt as a `user_message`, and answers
    /// it at intake. Off on v1, whose wire stays unchanged.
    pub v2: bool,
    /// The workspace roots of the session besides `cwd` (ACP
    /// `additionalDirectories`, legacy `_meta.additionalRoots`): the file-change
    /// report validates reported paths against this same set
    /// (`file-change-audit.ts:250-266`, AG-3).
    pub additional_directories: Vec<std::path::PathBuf>,
}

impl SessionOptions {
    /// Build options from a `ServeOptions`-style config for a fresh session.
    pub fn new(spawn: SpawnOptions, timings: Timings, initialize: InitializeOptions) -> Self {
        Self {
            spawn,
            timings,
            initialize,
            air: None,
            caps: crate::client_caps::ClientCapabilities::default(),
            raw_sdk_messages: crate::sdk_options::RawSdkMessages::Off,
            env: crate::env_snapshot::EnvSnapshot::default(),
            logger: None,
            gateway_models: None,
            v2: false,
            additional_directories: Vec::new(),
        }
    }

    /// Set the workspace roots besides `cwd` (AG-3 file-change reports).
    #[must_use]
    pub fn with_additional_directories(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.additional_directories = dirs;
        self
    }

    /// Serve this session to an ACP v2 connection (F39).
    #[must_use]
    pub fn with_v2(mut self, v2: bool) -> Self {
        self.v2 = v2;
        self
    }

    /// Narrow the model picker to the models a gateway listed (O-74).
    #[must_use]
    pub fn with_gateway_models(mut self, listed: Option<(Vec<String>, Option<String>)>) -> Self {
        self.gateway_models = listed;
        self
    }

    /// Set the parsed AIR client (item 3.2).
    #[must_use]
    pub fn with_air(mut self, air: Option<AirClient>) -> Self {
        self.air = air;
        self
    }

    /// Set the full client-capability parse (ROADMAP F2a).
    #[must_use]
    pub fn with_caps(mut self, caps: crate::client_caps::ClientCapabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Set the raw SDK message forwarding config (U-10).
    #[must_use]
    pub fn with_raw_sdk_messages(
        mut self,
        raw_sdk_messages: crate::sdk_options::RawSdkMessages,
    ) -> Self {
        self.raw_sdk_messages = raw_sdk_messages;
        self
    }

    /// Set the environment snapshot.
    #[must_use]
    pub fn with_env(mut self, env: crate::env_snapshot::EnvSnapshot) -> Self {
        self.env = env;
        self
    }

    /// Attach the adapter logger (N-05 plugin-error logging).
    #[must_use]
    pub fn with_logger(mut self, logger: Option<std::sync::Arc<crate::logger::Logger>>) -> Self {
        self.logger = logger;
        self
    }
}

/// A handle to the session actor. Cloneable: the ACP layer (agent.rs) can issue
/// [`Command`]s through it.
#[derive(Clone)]
pub struct Session {
    tx: mpsc::UnboundedSender<Command>,
    high_water: Arc<AtomicUsize>,
    session_id: String,
    /// The `commands` array from the `initialize` control_response handshake
    /// (phase 15, Item 1): the source for the proactive `available_commands_update`
    /// sent right after `session/new` / `session/load` (upstream
    /// `sendAvailableCommandsUpdate` → `session.query.supportedCommands()`).
    commands: Vec<Value>,
    /// The `account` from the initialize response (the session account that
    /// feeds the `authStatus` push).
    account: Option<Value>,
    control: Control,
    modes: crate::session_config::ModesState,
    config_options: Vec<crate::session_config::ConfigOption>,
    /// The actor's modes + config options as it ended (see
    /// [`Command::FinalStateSink`]); `None` while it runs.
    final_state: watch::Receiver<Option<LiveState>>,
}

/// Closes the actor of a session whose [`Session::start`] did not finish: the
/// close is sent when the guard is dropped while still armed, and the actor
/// interrupts and drops its child.
struct StartGuard(Option<mpsc::UnboundedSender<Command>>);

impl Drop for StartGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let (reply, _closed) = oneshot::channel();
            let _ = tx.send(Command::Close { reply });
        }
    }
}

/// The live modes and config options of a session (`Session.modes` and friends).
pub type LiveState = (
    crate::session_config::ModesState,
    Vec<crate::session_config::ConfigOption>,
);

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
        let support = crate::elicitation::ElicitationSupport {
            form: opts.caps.elicitation_form,
            url: opts.caps.elicitation_url,
        };
        let control = Control::spawn(
            stdin,
            control_in_rx,
            ControlOptions {
                on_request: Some(build_permission_request_handler(tx.clone(), support)),
            },
        );

        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let high_water = Arc::new(AtomicUsize::new(0));
        let hw = high_water.clone();

        // The `Process` owns the child (`kill_on_drop(true)`), so it must stay
        // alive for the session's lifetime — move it into the actor task. The
        // actor reads `process.lines` (the codec stream) and keeps `process`
        // alive until the task ends (then the child is killed on teardown).
        let resumed = opts.spawn.resume.is_some();
        let mut process = process;
        let stream = std::mem::replace(&mut process.lines, mpsc::unbounded_channel().1);
        let (stop_first, stop_first_rx) = oneshot::channel();
        let _ = tx.send(Command::InitRestarter(Box::new(clear_context::Restarter {
            spawn: opts.spawn.clone(),
            timings: opts.timings.clone(),
            initialize: opts.initialize.clone(),
            stop_first: Some(stop_first),
        })));
        tokio::spawn(run(
            commands,
            tx.clone(),
            stream,
            process,
            stop_first_rx,
            control.clone(),
            control_in_tx,
            update_tx,
            opts.timings,
            session_id.clone(),
            hw,
            opts.air.clone(),
            opts.caps.clone(),
            opts.spawn
                .default_cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default(),
            resumed,
            opts.initialize.forward_subagent_text,
            opts.raw_sdk_messages.clone(),
            opts.env.clone(),
            opts.logger.clone(),
            opts.additional_directories.clone(),
        ));
        // `stopsWorkflowsOnCancel` (`acp-agent.ts:11687`): a client with the
        // per-task stop affordance and no async tasks of its own gets the
        // background workflows stopped after a cancel's interrupt.
        let stops_workflows = opts.initialize.per_task_stop_affordance == Some(true)
            && !opts
                .air
                .as_ref()
                .is_some_and(|air| air.supports("asyncTasks"));
        // From here the actor owns the child. Whatever ends this start early (an
        // error, or the future being dropped mid-handshake when a recreate is
        // aborted at connection end) must end the actor too, or its `claude`
        // child outlives the connection: the actor's own command-channel clones
        // keep it from ever seeing the channel close.
        let mut guard = StartGuard(Some(tx.clone()));
        let _ = tx.send(Command::StopsWorkflowsOnCancel(stops_workflows));
        let _ = tx.send(Command::V2Reporting(opts.v2));
        let (final_tx, final_state) = watch::channel(None);
        let _ = tx.send(Command::FinalStateSink(final_tx));

        // Phase-4 initialize handshake. The actor loop is already running so it
        // routes the child's control_response back to the control task. The
        // fake-claude transcript expects the initialize control_request; parse
        // the response. The `commands` (phase 15, Item 1) feed the proactive
        // `available_commands_update` the agent layer sends after session/new.
        let info = control
            .initialize(&opts.initialize)
            .await
            .map_err(|e| SessionError::Start(format!("initialize handshake: {e}")))?;

        let sdk_models: Vec<crate::session_config::ModelInfo> = info
            .models
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();

        let cwd = opts
            .spawn
            .default_cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let settings = crate::settings::load(&cwd, &opts.env);

        // F5b: the backend key scopes the cross-session context-window cache.
        // It is derived from the exact env handed to the SDK (the adapter env
        // with the spawn overrides on top), matching Node's `providerCacheKeyFor`.
        let provider_env = opts
            .spawn
            .extra_env
            .iter()
            .fold(opts.env.clone(), |env, (key, value)| {
                env.with_var(key.clone(), value.clone())
            });
        let provider_cache_key = crate::session_model::provider_cache_key_for(&provider_env);

        let is_resumed = opts.spawn.resume.is_some();
        // MAJOR 2 / P4: read the resumed tail WITH the model (acp-agent.js:8290-8293),
        // so the resumed-model hint actually reaches `get_available_models` and
        // re-selects the model the resumed transcript last used.
        let resumed_model_hint = opts.spawn.resume.as_ref().and_then(|id| {
            crate::resumed_session::read_resumed_tail_with_env(id, true, &opts.env).model
        });

        // F5b: apply the user's `availableModels` allowlist (with `modelOverrides`
        // as the surfaced/setModel target) and managed `deniedModels`, then the
        // gateway narrowing point. The `providers/set` listing that feeds
        // `narrowToGatewayModels` arrives with F33; with no listing the SDK list
        // passes through unchanged.
        let settings_models = match settings.available_models.as_deref() {
            Some(allowlist) => crate::session_model::apply_available_models_allowlist(
                &sdk_models,
                allowlist,
                settings.model_overrides.as_ref(),
                &settings.managed_denied_models,
                opts.env.get("ANTHROPIC_CUSTOM_MODEL_OPTION"),
            ),
            None => sdk_models.clone(),
        };
        let (models, narrowed_by_gateway) = crate::session_model::narrow_models_for_gateway(
            settings_models,
            opts.gateway_models
                .as_ref()
                .map(|(listed, _)| listed.as_slice()),
            opts.gateway_models
                .as_ref()
                .and_then(|(_, prefix)| prefix.as_deref()),
        );
        let selection = crate::session_model::get_available_models(
            &models,
            &sdk_models,
            settings.effective.get("model"),
            opts.env.get("ANTHROPIC_MODEL"),
            is_resumed,
            resumed_model_hint.as_deref(),
            narrowed_by_gateway,
        );
        if let Some(diagnostic) = selection.diagnostic.as_deref() {
            // Node's `logger.error` for a non-string settings.model.
            eprintln!("{diagnostic}");
        }
        let mut current_model_id = selection.current_model_id.clone();

        if !selection.skip_set_model {
            let outcome = control
                .send_request(json!({
                    "subtype": "set_model",
                    "model": &current_model_id,
                }))
                .await;
            if let Err(e) = outcome {
                let error = e.to_string();
                match crate::session_model::resolve_model_switch_failure(
                    is_resumed,
                    selection.switch_to_listed,
                    &error,
                    &models,
                    &selection.model_before_switch,
                    resumed_model_hint.as_deref(),
                ) {
                    crate::session_model::ModelSwitchFailure::Fail(message) => {
                        return Err(SessionError::ModelSwitchFailed(message));
                    }
                    crate::session_model::ModelSwitchFailure::StayOn(fallback) => {
                        if is_resumed {
                            eprintln!(
                                "Failed to re-assert model \"{current_model_id}\" on resume: {error}"
                            );
                        } else {
                            eprintln!(
                                "Model switch to \"{current_model_id}\" failed or was vetoed by a PreModelSwitch hook; staying on the default model: {error}"
                            );
                        }
                        current_model_id = fallback;
                    }
                }
            }
        }

        let allowlisted_info = models.iter().find(|m| m.value == current_model_id).cloned();
        let fallback_info = if allowlisted_info.is_none() {
            crate::session_config::resolve_model_preference(&sdk_models, &current_model_id)
        } else {
            None
        };
        let current_model_info = allowlisted_info.or(fallback_info.clone());
        let mut model_infos = models.clone();
        if let Some(fb) = fallback_info {
            let mut patched = fb;
            patched.value = current_model_id.clone();
            patched.display_name = current_model_id.clone();
            patched.description = String::new();
            patched.resolved_model = None;
            model_infos.push(patched);
        }

        let is_air = opts.air.is_some();
        // MAJOR 3/4 / P4: Node only seeds/syncs the concrete effort for clients
        // that declare the AIR `recommendedValue` capability
        // (`clientSupportsRecommendedConfigValue`, acp-agent.js:9510-9512).
        let use_recommended_value = opts
            .air
            .as_ref()
            .is_some_and(|a| a.supports("recommendedValue"));
        let allow_bypass = opts.spawn.allow_dangerously_skip_permissions;
        let available_modes = crate::session_config::build_available_modes(allow_bypass, is_air);

        let initial_mode = opts.spawn.permission_mode.as_deref().unwrap_or("default");
        let is_auto_unavailable = current_model_info
            .as_ref()
            .is_some_and(|m| m.supports_auto_mode != Some(true));
        let auto_clamped = initial_mode == "auto" && is_auto_unavailable;
        let mut effective_mode = if auto_clamped {
            crate::session_config::AUTO_MODE_FALLBACK.to_string()
        } else {
            initial_mode.to_string()
        };
        // `SessionModeManager.initialize` (`session-mode.ts:157-177`) logs the
        // clamp: the model cannot honour Auto, or the mode is not in the catalog.
        if let Some(message) = crate::agent::auto_mode_clamp_log(
            initial_mode,
            auto_clamped,
            current_model_id.as_str(),
            available_modes.iter().any(|m| m.id == effective_mode),
        ) {
            if let Some(logger) = &opts.logger {
                logger.error(&message);
            }
        }
        if !available_modes.iter().any(|m| m.id == effective_mode) {
            effective_mode = "default".to_string();
        }
        if effective_mode != initial_mode {
            let _ = control
                .send_request(json!({
                    "subtype": "set_permission_mode",
                    "mode": &effective_mode,
                }))
                .await;
        }

        let modes = crate::session_config::ModesState {
            current_mode_id: effective_mode,
            available_modes,
        };

        // O-50/O-56 (F11): the programmatic settings tier
        // (`_meta.claudeCode.options.settings` merged over `CLAUDE_MODEL_CONFIG`,
        // resolved into the spawn's `--settings` value) shadows the file-backed
        // settings for the effort seed and is retained as
        // `session.effortSettingsOverride` (`acp-agent.ts:11175-11184,11642`).
        let effort_settings_override = opts
            .spawn
            .settings_arg
            .as_deref()
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        let merged_settings = crate::session_config::merge_effort_settings(
            &settings.effective,
            effort_settings_override.as_ref(),
        );
        let current_effort = crate::session_config::seeded_effort(
            opts.spawn.effort.as_deref(),
            &settings.effective,
            effort_settings_override.as_ref(),
            current_model_info.as_ref(),
            &current_model_id,
        );
        let ultracode = merged_settings
            .get("ultracode")
            .and_then(Value::as_bool)
            .unwrap_or(settings.ultracode);

        // A-04 (acp-agent.ts:12324-12355): discover the main-thread agents the
        // SDK reports and adopt the requested one only when the picker surfaces
        // it.
        let agents = crate::session_config::discover_custom_agents(info.agents.as_ref());
        let current_agent =
            crate::session_config::resolve_current_agent(opts.spawn.agent.as_deref(), &agents);
        // N-03 seed (acp-agent.ts:11565-11584): the CLI may start a session with
        // Fast mode already on, or force it off when `fastModePerSessionOptIn`
        // is set. The toggle is only surfaced while the model supports it.
        let fast_mode = crate::session_config::fast_mode_option_state_from_initialize(
            &json!({
                "fast_mode_state": info.fast_mode_state.clone(),
                "fast_mode_disabled_reason": info.fast_mode_disabled_reason.clone(),
            }),
            crate::session_config::model_supports_fast_mode(&model_infos, &current_model_id),
            opts.caps.boolean_config,
        );

        let config_options = crate::session_config::build_config_options_with(
            &crate::session_config::ConfigOptionsInput {
                modes: &modes,
                available_models: &selection.available_models,
                current_model_id: &current_model_id,
                model_infos: &model_infos,
                current_effort: current_effort.as_deref(),
                agents: &agents,
                current_agent: &current_agent,
                fast_mode: Some(&fast_mode),
                use_recommended_value,
            },
        );

        // MAJOR 3 / P4 (acp-agent.js:8804-8807): Node seeds `apply_flag_settings
        // {effortLevel}` at session creation ONLY when the client opts in via the
        // AIR `useRecommendedValue` capability. A legacy (non-AIR) client gets no
        // effort flag at creation — the CLI resolves persisted effort itself.
        // O-56: the seed carries `ultracode` through `effortFlagSettings`
        // (`session-effort.ts:19-25`).
        if use_recommended_value {
            if let Some(effort_opt) = config_options
                .iter()
                .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
            {
                // `await q.applyFlagSettings(...)` with no catch
                // (`acp-agent.ts:11594-11609`): a rejection fails `session/new`
                // instead of leaving an effort on screen the SDK never applied.
                control
                    .send_request(json!({
                        "subtype": "apply_flag_settings",
                        "settings": crate::session_config::effort_flag_settings(
                            Some(&effort_opt.current_value),
                            ultracode,
                        ),
                    }))
                    .await
                    .map_err(|e| SessionError::Start(e.to_string()))?;
            }
        }

        let config_state = crate::session_config::SessionConfigState {
            modes: modes.clone(),
            available_models: selection.available_models.clone(),
            current_model_id: current_model_id.clone(),
            model_infos: model_infos.clone(),
            config_options: config_options.clone(),
            current_effort,
            effort_pinned: None,
            applied_effort: None,
            allow_bypass,
            is_air,
            ultracode,
            effort_settings_override,
            pre_plan_mode: None,
            agents,
            current_agent,
            fast_mode_enabled: fast_mode.enabled,
            fast_mode_disabled_reason: fast_mode.disabled_reason.clone(),
            pending_auto_fallback_warning: auto_clamped,
            provider_cache_key,
        };

        let _ = tx.send(Command::InitConfig {
            config: Box::new(config_state),
        });
        // D5 (Decision 9): the actor keeps the initialize handshake's command
        // list so a `system/init` terminal-command latch or a later
        // `commands_changed` can rebuild the advertisement without a round-trip.
        let _ = tx.send(Command::InitCommands {
            commands: info.commands.clone(),
        });

        guard.0 = None;
        Ok((
            Session {
                tx,
                high_water,
                session_id,
                commands: info.commands,
                account: info.account,
                control,
                modes,
                config_options,
                final_state,
            },
            update_rx,
        ))
    }

    /// Access the control handle to the child process.
    pub fn control(&self) -> &Control {
        &self.control
    }

    /// Access the command channel sender for this session actor.
    pub(crate) fn command_sender(&self) -> &mpsc::UnboundedSender<Command> {
        &self.tx
    }

    /// The ACP `sessionId` this session is known by.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The model the session is currently on, read from the config options
    /// snapshot (F5b/A-10: the background context-window refresh captures it so
    /// an answer for a model the session left is dropped).
    pub fn current_model_id(&self) -> Option<String> {
        self.config_options
            .iter()
            .find(|option| option.id == crate::session_model::MODEL_CONFIG_ID)
            .map(|option| option.current_value.clone())
    }

    /// The `commands` array from the `initialize` handshake — the source for
    /// the proactive `available_commands_update` (phase 15, Item 1).
    pub fn commands(&self) -> &[Value] {
        &self.commands
    }

    /// The actor's CURRENT command list: the initialize handshake's, replaced
    /// by every `commands_changed` frame (D5 / B-118). Falls back to the
    /// creation-time snapshot when the actor is gone.
    pub async fn live_commands(&self) -> Vec<Value> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::CommandsSnapshot { reply }).is_err() {
            return self.commands.clone();
        }
        rx.await.unwrap_or_else(|_| self.commands.clone())
    }

    /// The session account from the `initialize` handshake, if any.
    pub fn account(&self) -> Option<&Value> {
        self.account.as_ref()
    }

    /// Send a `session/prompt` request to the actor and await its settlement.
    pub async fn prompt(
        &self,
        uuid: String,
        frame: Value,
        is_local_only: bool,
        prompt_text: String,
    ) -> Result<PromptReply, PromptFailure> {
        self.send_prompt(uuid, frame, is_local_only, prompt_text)
            .await
            .map_err(|_| PromptFailure::from(SessionError::Closed))?
    }

    /// Record the ACP `session/prompt` params' `_meta` of a prompt about to be
    /// enqueued (AG-3: `agentFileChangeReportRequest`). Sent on the same
    /// unbounded channel right before the prompt, so the actor sees it first.
    pub fn note_prompt_meta(&self, uuid: String, meta: Value) {
        let _ = self.tx.send(Command::PromptMeta { uuid, meta });
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
        prompt_text: String,
    ) -> oneshot::Receiver<Result<PromptReply, PromptFailure>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::Prompt {
            uuid,
            frame,
            is_local_only,
            prompt_text,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Enqueue an ACP v2 `session/prompt` (F39, M-113), returning the `oneshot`
    /// that resolves once Claude Code takes the prompt in, with the id of the
    /// user message it became. The actor sends the prompt's `user_message` and
    /// the `running` state before it resolves, so the v2 answer never overtakes
    /// them (`v2Prompt`, `v2/prompt.ts:30-67`).
    /// Enqueue a v2 prompt. The first receiver answers at intake with the
    /// message id it became; the second reports the turn's end, so the caller
    /// runs the v1 bookkeeping (`turn_settled`, the sign-out respawn) for it
    /// too (NEW-3). A dropped end receiver means the session ended before the
    /// turn settled.
    pub fn send_prompt_v2(
        &self,
        uuid: String,
        frame: Value,
        is_local_only: bool,
        prompt_text: String,
        content: Value,
    ) -> (
        oneshot::Receiver<Result<String, crate::turn_events::V2IntakeError>>,
        oneshot::Receiver<crate::turn_events::V2TurnEnd>,
    ) {
        let (inserted_tx, inserted_rx) = oneshot::channel();
        let (ended_tx, ended_rx) = oneshot::channel();
        let _ = self.tx.send(Command::PromptV2 {
            uuid,
            frame,
            is_local_only,
            prompt_text,
            content,
            inserted: inserted_tx,
            ended: ended_tx,
        });
        (inserted_rx, ended_rx)
    }

    /// The session's foreground state now (F39), for a v2 `session/resume` that
    /// reports the work it resumes before its response (`foregroundState`,
    /// `acp-agent.ts:9636-9640`).
    pub async fn foreground_state(&self) -> Option<crate::turn_events::ForegroundState> {
        let (reply, done) = oneshot::channel();
        let _ = self.tx.send(Command::ForegroundState { reply });
        done.await.ok()
    }

    /// Register a `/mcp` prompt with the actor (rust-air-gaps G12). Sent before
    /// [`Self::send_prompt`] on the same channel, so the actor has the command
    /// when the prompt arrives.
    pub fn send_mcp(&self, uuid: String, command: crate::mcp_command::McpCommand) {
        let _ = self.tx.send(Command::McpPrompt { uuid, command });
    }

    /// Start the background MCP OAuth run (F23c/A-01). `configured` is true for
    /// the session-start call, which also records the session's ACP servers for
    /// the `/mcp reconnect` follow-up.
    pub fn start_mcp_auth(&self, servers: Vec<String>, configured: bool) {
        let _ = self.tx.send(Command::StartMcpAuth {
            servers,
            configured,
        });
    }

    /// Whether the adapter may answer `/remote-control` itself (F24/S-03): the
    /// CLI does not offer its own non-terminal `remote-control` command.
    pub async fn remote_control_available(&self) -> bool {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(Command::RemoteControlAvailability { reply })
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }

    /// Answer a `/remote-control` prompt (F24/S-03) without a model turn. The
    /// reply resolves once the control call returned and its text is queued.
    pub fn remote_control(
        &self,
        command: crate::control::RemoteControlCommand,
    ) -> oneshot::Receiver<Result<PromptReply, PromptFailure>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::RemoteControlPrompt {
            command,
            reply: reply_tx,
            accepted: None,
        });
        reply_rx
    }

    /// [`Self::remote_control`] with the actor's acceptance reported through its
    /// own oneshot before the control call starts (NEW-2), so a v2 answer can
    /// refuse the command before the prompt's `user_message`
    /// (`acp-agent.ts:3824-3838`). `reply` answers the prompt.
    pub fn remote_control_checked(
        &self,
        command: crate::control::RemoteControlCommand,
    ) -> RemoteControlChecked {
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::RemoteControlPrompt {
            command,
            reply: reply_tx,
            accepted: Some(accepted_tx),
        });
        RemoteControlChecked {
            accepted: accepted_rx,
            reply: reply_rx,
        }
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

    /// Whether the session still has a turn to settle (`turnQueue.some(!settled)`):
    /// a prompt, or a turn a steer started. A closed session answers `false`.
    pub async fn has_unsettled_turn(&self) -> bool {
        let (reply, reply_rx) = oneshot::channel();
        if self.tx.send(Command::UnsettledTurn { reply }).is_err() {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }

    /// B-29: the ACP messageId -> SDK uuid map the session recorded.
    pub async fn message_ids(&self) -> crate::message_ids::MessageIdMap {
        let (reply, reply_rx) = oneshot::channel();
        if self.tx.send(Command::MessageIds { reply }).is_err() {
            return crate::message_ids::MessageIdMap::default();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// B-54: merge the history replay's messageId map into the session's.
    pub fn seed_message_ids(&self, map: crate::message_ids::MessageIdMap) {
        let _ = self.tx.send(Command::SeedMessageIds { map });
    }

    /// Stop one background task without cancelling the turn (`_session/async_task/stop`).
    /// `Ok(false)` when the task is unknown or cannot be stopped.
    pub async fn stop_async_task(&self, task_id: String) -> Result<bool, SessionError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::StopAsyncTask { task_id, reply })
            .map_err(|_| SessionError::Closed)?;
        match reply_rx.await {
            Ok(Ok(stopped)) => Ok(stopped),
            Ok(Err(message)) => Err(SessionError::Start(message)),
            Err(_) => Err(SessionError::Closed),
        }
    }

    /// A handle with no actor behind it, for tests of the registry: the returned
    /// receiver sees every command sent to the session.
    #[cfg(test)]
    pub(crate) fn detached_for_test(
        session_id: &str,
    ) -> (
        Session,
        mpsc::UnboundedReceiver<Command>,
        tokio::io::DuplexStream,
    ) {
        let (a, b) = tokio::io::duplex(1024);
        let (_control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let (_final_tx, final_state) = watch::channel(None);
        let session = Session {
            tx,
            high_water: Arc::new(AtomicUsize::new(0)),
            session_id: session_id.to_string(),
            commands: Vec::new(),
            account: None,
            control,
            modes: crate::session_config::ModesState {
                current_mode_id: "default".to_string(),
                available_modes: Vec::new(),
            },
            config_options: Vec::new(),
            final_state,
        };
        (session, commands, b)
    }

    /// Close the session (`session/close`): cancel, interrupt, end the actor
    /// (which drops the child). Idempotent — a session that already ended
    /// returns at once.
    pub async fn close(&self) {
        let (reply, reply_rx) = oneshot::channel();
        if self.tx.send(Command::Close { reply }).is_err() {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), reply_rx).await;
    }

    /// Send a `session/cancel` notification (8.9).
    pub async fn cancel(&self) -> Result<(), SessionError> {
        self.tx
            .send(Command::Cancel)
            .map_err(|_| SessionError::Closed)
    }

    /// A `session/cancel` named the child session `session_id`: stop the task of
    /// that native subagent without cancelling this session
    /// (`cancelChildSession`, `acp-agent.ts:8396-8409`). `false` means the id is
    /// no announced child of this session's runtime.
    pub async fn cancel_child(&self, session_id: String) -> bool {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(Command::CancelChild { session_id, reply })
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }

    /// A client chose the title of this session (`_session/rename`, archive,
    /// unarchive; `SessionTitles.setExplicitTitle`, `session-titles.ts:230-265`).
    /// The title settles for good (no generated title replaces it), the change
    /// waits for a title generation in flight so that its persisted title cannot
    /// land after this one, `persist` writes the title and returns what it stored,
    /// if anything, and `publish` is then published as a `session_info_update`
    /// (without `updatedAt`). A change that fails, or stores and publishes
    /// nothing, leaves the title state as it was.
    pub async fn set_explicit_title(
        &self,
        publish: crate::session_titles::ExplicitPublish,
        persist: ExplicitPersist,
    ) -> Result<(), String> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::SetExplicitTitle {
                publish,
                persist,
                reply,
            })
            .map_err(|_| SESSION_GONE.to_string())?;
        reply_rx.await.map_err(|_| SESSION_GONE.to_string())?
    }

    /// The title last published and the title the CLI holds
    /// (`titles.shownTitle` / `titles.storedTitle`).
    pub async fn title_snapshot(&self) -> Option<TitleSnapshot> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx.send(Command::TitleSnapshot { reply }).ok()?;
        reply_rx.await.ok()
    }

    /// Wait until every update queued so far was forwarded to the client, so the
    /// title update of a rename precedes the response that follows it.
    pub async fn flush_updates(&self) {
        let (reply, reply_rx) = oneshot::channel();
        if self.tx.send(Command::FlushUpdates { reply }).is_ok() {
            let _ = reply_rx.await;
        }
    }

    /// Record a title the CLI took, as soon as it took it
    /// (`titles.rememberStoredTitle`).
    pub fn remember_stored_title(&self, title: String) {
        let _ = self.tx.send(Command::RememberStoredTitle { title });
    }

    /// How a stored title reads on the client (`ClaudeAcpAgent.clientTitle`): an
    /// AIR client gets the title of an archived session without the archive
    /// prefix. Applies to every title published from now on.
    pub fn set_client_title(&self, map: crate::session_titles::ClientTitle) {
        let _ = self.tx.send(Command::SetClientTitle { map });
    }

    /// The high-water mark of simultaneously-active turns observed so far
    /// (INV-14 / 5.4). Never above 1 by construction.
    pub fn high_water(&self) -> usize {
        self.high_water.load(Ordering::Relaxed)
    }

    /// Modes state of the session.
    pub fn modes(&self) -> &crate::session_config::ModesState {
        &self.modes
    }

    /// Config options exposed by the session.
    pub fn config_options(&self) -> &[crate::session_config::ConfigOption] {
        &self.config_options
    }

    /// Set permission mode (`session/set_mode`).
    pub async fn set_mode(&self, mode_id: String) -> Result<(), String> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::SetMode { mode_id, reply })
            .map_err(|_| SESSION_GONE.to_string())?;
        reply_rx.await.map_err(|_| SESSION_GONE.to_string())?
    }

    /// Set config option (`session/set_config_option`).
    pub async fn set_config_option(
        &self,
        config_id: String,
        value: Value,
    ) -> Result<Vec<crate::session_config::ConfigOption>, String> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::SetConfigOption {
                config_id,
                value,
                reply,
            })
            .map_err(|_| SESSION_GONE.to_string())?;
        reply_rx.await.map_err(|_| SESSION_GONE.to_string())?
    }

    /// Seed the Task* state rebuilt by a `session/load` replay (PC-2.3) so the
    /// live actor continues from the replayed plan (Node shares its
    /// `taskState` map with the replay).
    pub async fn seed_task_state(
        &self,
        state: Vec<(String, crate::hooks::TaskStateEntry)>,
        last_published: Option<String>,
    ) -> Result<(), String> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::SeedTaskState {
                state,
                last_published,
                reply,
            })
            .map_err(|_| SESSION_GONE.to_string())?;
        reply_rx.await.map_err(|_| SESSION_GONE.to_string())
    }

    /// The session's failure book, for a replay that continues its revisions.
    pub async fn failures_snapshot(&self) -> Result<failures::SessionFailures, String> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::FailuresSnapshot { reply })
            .map_err(|_| SESSION_GONE.to_string())?;
        reply_rx.await.map_err(|_| SESSION_GONE.to_string())
    }

    /// Adopt the failure book a `session/load` replay rebuilt.
    pub async fn seed_failures(&self, book: failures::SessionFailures) -> Result<(), String> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::SeedFailures { book, reply })
            .map_err(|_| SESSION_GONE.to_string())?;
        reply_rx.await.map_err(|_| SESSION_GONE.to_string())
    }

    /// Fetch the actor's LIVE modes + config options (MAJOR 5).
    ///
    /// A session whose actor ended (the stream died) answers with the state the
    /// actor left behind, not the creation-time copy.
    pub async fn live_state(&self) -> Result<LiveState, String> {
        let (reply, reply_rx) = oneshot::channel();
        let asked = match self.tx.send(Command::GetState { reply }) {
            Ok(()) => reply_rx.await.map_err(|_| SESSION_GONE.to_string())?,
            Err(_) => Err(SESSION_GONE.to_string()),
        };
        match asked {
            Err(_) => self
                .final_state
                .borrow()
                .clone()
                .ok_or_else(|| SESSION_GONE.to_string()),
            ok => ok,
        }
    }
}

/// Sends [`Command::ElicitationWithdrawn`] when the inbound handler future is
/// dropped before its reply resolved — a `control_cancel_request`, or the
/// control channel rebound to a fresh child on a clear-context restart. The
/// actor then aborts the client round trip (which sends `$/cancel_request`) and
/// ends the child's awaiting-user token as withdrawn.
struct WithdrawOnDrop {
    command_tx: mpsc::UnboundedSender<Command>,
    request_id: String,
    armed: bool,
}

impl WithdrawOnDrop {
    fn new(command_tx: mpsc::UnboundedSender<Command>, request_id: &str) -> Self {
        Self {
            command_tx,
            request_id: request_id.to_string(),
            armed: true,
        }
    }

    /// The handler resolved normally: no withdraw is owed.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for WithdrawOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.command_tx.send(Command::ElicitationWithdrawn {
                request_id: self.request_id.clone(),
            });
        }
    }
}

/// Build the inbound `control_request` handler injected into the control
/// channel (phase 10). A `can_use_tool` request is routed to the session actor
/// via [`Command::Permission`] (which owns the `MapState` needed to
/// `ensure_tool_call_emitted`); the reply resolves with the `control_response`.
///
/// F23b: a capable client's MCP `elicitation` and `request_user_dialog` are
/// routed to the actor, which spawns the client round trip. A client without
/// the capability keeps the SDK's defaults: an `elicitation` is answered
/// `{action:"decline"}` and a dialog stays silent.
///
/// The handler models the SDK's `processControlRequest` dispatch for the
/// subtypes the adapter does not own (F3a): the `Z4` machine-only subtypes and
/// a `request_user_dialog` with no dialog handler return `None` (write
/// nothing); an `elicitation` with no handler answers the SDK default
/// `{action:"decline"}`; every other subtype answers an `error` response
/// carrying the thrown message (`Unsupported control request subtype: X`).
fn build_permission_request_handler(
    command_tx: mpsc::UnboundedSender<Command>,
    support: crate::elicitation::ElicitationSupport,
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
                let guard = WithdrawOnDrop::new(command_tx.clone(), &request_id);
                let sent = command_tx.send(Command::Permission {
                    frame,
                    reply: reply_tx,
                });
                let answer = match sent {
                    // No session actor: Node's `canUseTool` early return
                    // (`acp-agent.ts:10030-10035`) is a plain deny without a
                    // toolUseID or a classification.
                    Err(_) => session_not_found_response(&request_id),
                    // The actor dropped the reply: a cancel or the session close
                    // aborted the round-trip, and Node throws "Tool use aborted"
                    // (`acp-agent.ts:9791`, `9833-9835`) — an error verdict, not
                    // the R28 deny (P-36).
                    Ok(()) => reply_rx.await.unwrap_or_else(|_| {
                        crate::permission_response::error_control_response(
                            &request_id,
                            crate::permission_response::TOOL_USE_ABORTED,
                        )
                    }),
                };
                guard.disarm();
                Some(answer)
            } else if subtype == "hook_callback" {
                let callback_id = frame
                    .pointer("/request/callback_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = frame
                    .pointer("/request/input")
                    .cloned()
                    .unwrap_or(Value::Null);
                let tool_use_id = frame
                    .pointer("/request/tool_use_id")
                    .and_then(Value::as_str)
                    .map(String::from);
                let (reply_tx, reply_rx) = oneshot::channel();
                let _ = command_tx.send(Command::HookCallback {
                    request_id: request_id.clone(),
                    callback_id,
                    input,
                    tool_use_id,
                    reply: reply_tx,
                });
                Some(
                    reply_rx
                        .await
                        .unwrap_or_else(|_| crate::hooks::hook_control_response_error(&request_id)),
                )
            } else if subtype == "request_user_dialog" {
                // F23b (R-08): only a form-capable client gets a dialog; every
                // other client keeps the SDK's silent default.
                if !support.form {
                    return None;
                }
                let (reply_tx, reply_rx) = oneshot::channel();
                let guard = WithdrawOnDrop::new(command_tx.clone(), &request_id);
                let _ = command_tx.send(Command::UserDialogControl {
                    frame,
                    reply: reply_tx,
                });
                let answer = reply_rx.await.unwrap_or_else(|_| {
                    json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": { "behavior": "cancelled" },
                        },
                    })
                });
                guard.disarm();
                Some(answer)
            } else if crate::control::SUPPRESSED_SUBTYPES.contains(&subtype) {
                // F3a (a): `Z4` is never answered.
                None
            } else if subtype == "elicitation" {
                // F23b (R-10): forward to the client when it advertised form or
                // url elicitation; otherwise the SDK's auto-decline is a SUCCESS
                // verdict (F3a (b)).
                if support.form || support.url {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let guard = WithdrawOnDrop::new(command_tx.clone(), &request_id);
                    let _ = command_tx.send(Command::ElicitationControl {
                        frame,
                        reply: reply_tx,
                    });
                    let answer = reply_rx.await.unwrap_or_else(|_| {
                        json!({
                            "type": "control_response",
                            "response": {
                                "subtype": "success",
                                "request_id": request_id,
                                "response": { "action": "decline" },
                            },
                        })
                    });
                    guard.disarm();
                    return Some(answer);
                }
                Some(json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": { "action": "decline" },
                    },
                }))
            } else {
                // F3a (c) / R-07: every error verdict carries the message.
                let name = if subtype.is_empty() {
                    "undefined"
                } else {
                    subtype
                };
                Some(json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "error",
                        "request_id": request_id,
                        "error": format!("Unsupported control request subtype: {name}"),
                    },
                }))
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
    process: crate::process::Process,
    stop_first: oneshot::Receiver<()>,
    control: Control,
    control_in_tx: mpsc::UnboundedSender<Value>,
    update_tx: mpsc::UnboundedSender<SessionOutbound>,
    timings: Timings,
    session_id: String,
    high_water: Arc<AtomicUsize>,
    air: Option<AirClient>,
    caps: crate::client_caps::ClientCapabilities,
    cwd: PathBuf,
    resumed: bool,
    forward_subagent_text: bool,
    raw_sdk_messages: crate::sdk_options::RawSdkMessages,
    env: crate::env_snapshot::EnvSnapshot,
    logger: Option<std::sync::Arc<crate::logger::Logger>>,
    additional_directories: Vec<PathBuf>,
) {
    // The `process` handle owns the child and kills it on drop; the actor loop
    // itself never touches it, so the core loop is factored into `run_loop` and
    // exercised directly by unit tests (INV-27 drives the real actor command path
    // without spawning a child). A clone of the command sender is threaded into
    // the loop so the spawned `interrupt` task can report its receipt back to the
    // actor (phase 11).
    //
    // Whether the stream ended because the child died is only known to the
    // process: the SDK ends the iterator cleanly on a clean exit but throws on a
    // non-zero exit or a signal, and the two settle the turns differently
    // (`acp-agent.ts:5764-5800` vs `8240-8330`). The stream is relayed through
    // this task, which appends a marker frame carrying the exit verdict before it
    // closes the relay.
    let (relay_tx, relay_rx) = mpsc::unbounded_channel::<Value>();
    // The relay owns the process: ending the relay kills the child.
    let mut relay = Box::pin(relay_stream(
        stream,
        relay_tx,
        clear_context::child_exit_verdict(process, timings.clone()),
    ));
    let mut actor = Box::pin(run_loop(
        commands,
        command_tx,
        relay_rx,
        control,
        control_in_tx,
        update_tx,
        timings.force_cancel_grace,
        session_id,
        high_water,
        air,
        caps,
        cwd,
        resumed,
        forward_subagent_text,
        raw_sdk_messages,
        env,
        logger,
        additional_directories,
    ));
    let mut stop_first = stop_first;
    tokio::select! {
        _ = &mut actor => return,
        _ = &mut relay => {}
        // A clear-context restart moved the session to a fresh child: the first
        // one goes. (A dropped sender never stops it: the pattern disables the
        // branch.)
        Ok(()) = &mut stop_first => {}
    }
    drop(relay);
    actor.await;
}

/// A clear-context restart whose fresh child is still starting.
struct InFlightRestart {
    id: u64,
    plan: state::RestartPlan,
    /// Aborts the bring-up (and a child that is still starting) when dropped.
    _task: clear_context::AbortOnDrop,
    /// Keeps the placeholder `stream` open (never ended) while the restart runs.
    _hold: mpsc::UnboundedSender<Value>,
}

/// Publish the state of the fresh context a clear-context restart started: the
/// accepted plan's mode (the session's mode and its config option), and the
/// approved plan as the session's markdown plan for a client with plan
/// operations (`publishSessionState`, `exit-plan.ts:199-224`). The new context
/// has no plan mode to come back from.
fn publish_restart_state(
    ext: &mut SessionExt,
    reset: &exit_plan::ContextReset,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    ext.config.modes.current_mode_id = reset.mode.clone();
    ext.config.pre_plan_mode = None;
    for option in &mut ext.config.config_options {
        if option.id == crate::session_config::MODE_CONFIG_ID {
            option.current_value = reset.mode.clone();
        }
    }
    let mut updates = vec![
        json!({"sessionUpdate": "current_mode_update", "currentModeId": reset.mode}),
        json!({"sessionUpdate": "config_option_update", "configOptions": ext.config.config_options}),
    ];
    if ext.caps.plan {
        updates.push(exit_plan::plan_mode_update(&reset.plan));
    }
    for update in updates {
        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
            "sessionId": session_id,
            "update": update,
        })));
    }
}

/// How long a fresh context may take to start before the restart fails.
const RESTART_DEADLINE: Duration = Duration::from_secs(120);

/// The tool-call mapper state of a query: a new session, or the fresh context a
/// clear-context restart starts.
fn fresh_map_state(ext: &SessionExt, forward_subagent_text: bool) -> MapState {
    let mut map_state = MapState::default();
    // Decision 6: every client gets the session cwd so tool titles/locations
    // are cwd-relative (Node passes `session.cwd` at every report site).
    map_state.set_cwd(ext.cwd.clone());
    // T-40: the client's `diffPatch` capability gates a Write's input diff and
    // turns on the exact patch of the approval preview and the PostToolUse hook.
    map_state.set_diff_patch(ext.caps.diff_patch);
    map_state.set_client_caps(ext.caps.clone());
    if ext.air.is_some() {
        map_state.configure_air(ext.cwd.clone());
    }
    // C-42: a client that gets a subagent's transcript (the historical
    // `_meta["subagent-transcript"]` capability), native subagent sessions, or
    // that passed `forwardSubagentText` receives the nested assistant
    // text/thinking, stamped with its parent (`forwardsSubagentText`,
    // `acp-agent.ts:10947-10953`).
    map_state.set_forward_subagent_text(forward_subagent_text);
    map_state.set_logger(ext.logger.clone());
    map_state
}

/// The turn machine of the fresh context a clear-context restart starts: a new
/// consumer, which keeps the client-level choices of the session.
fn fresh_context_machine(
    forward_subagent_text: bool,
    stops_workflows_on_cancel: bool,
) -> TurnMachine {
    let mut machine = TurnMachine::new();
    machine.set_forwards_subagent_text(forward_subagent_text);
    machine.set_stops_workflows_on_cancel(stops_workflows_on_cancel);
    machine
}

/// Drop what belonged to the old context from the session state: the fresh
/// context starts with none of the old worker's signals or background commands.
fn reset_ext_for_fresh_context(ext: &mut SessionExt) {
    ext.hooks = crate::hooks::HookRegistry::new();
    ext.task_state.clear();
    ext.last_published_task_plan = None;
    ext.mcp.clear();
    ext.t2a.usage_turns.clear();
    // F20/B-109: the fresh context gets a brand-new compaction lifecycle, like
    // the fresh consumer upstream builds (`acp-agent.ts:4542`): no open entity,
    // no pending hook summary, no interruption latch, and compaction frames are
    // accepted again. The old context's terminal was published by the caller
    // (`compaction.interrupt()`), not by this reset.
    ext.t2a.compaction = crate::context_compaction::ContextCompactionLifecycle::new(
        ext.t2a.compaction.session_id().to_string(),
        ext.t2a.compaction.presentation(),
        ext.caps.air_client,
    );
    ext.t2a.compaction_output_delivered = ext.t2a.compaction.has_delivered_output();
    ext.t2a.pending_worker_shutdown = false;
    ext.t2a.background_terminals.clear();
    ext.t2a.background = state::BackgroundFacts::default();
    ext.failure_ctx.reset();
    ext.usage.reset_turn_scratch();
    ext.usage.set_used(0);
}

/// Relay `stream` to the actor and, once it closed, append the child's exit
/// verdict (`exit` resolves to whether it was abnormal, `None` when unknown) as
/// the last frame. Never completes: the actor ending ends the select around it.
async fn relay_stream(
    mut stream: mpsc::UnboundedReceiver<Value>,
    relay_tx: mpsc::UnboundedSender<Value>,
    exit: impl std::future::Future<Output = Option<bool>>,
) {
    while let Some(frame) = stream.recv().await {
        if relay_tx.send(frame).is_err() {
            break;
        }
    }
    if let Some(abnormal) = exit.await {
        let _ = relay_tx.send(child_exit_marker(abnormal));
    }
    drop(relay_tx);
    std::future::pending::<()>().await;
}

/// How long the relay waits for the child to exit once its stdout closed.
const CHILD_EXIT_WAIT: Duration = Duration::from_secs(5);

/// The relay's last frame before the stream closes: the child exited, abnormally
/// (a non-zero status or a signal) or not. Never a CLI frame.
fn child_exit_marker(abnormal: bool) -> Value {
    json!({"type": "adapter_child_exit", "abnormal": abnormal})
}

/// The `abnormal` verdict of a [`child_exit_marker`] frame.
fn child_exit_of(frame: &Value) -> Option<bool> {
    if frame.get("type").and_then(Value::as_str) != Some("adapter_child_exit") {
        return None;
    }
    frame.get("abnormal").and_then(Value::as_bool)
}

/// The error the SDK throws when the child dies (`sdk.mjs` `ProcessTransport`,
/// matched by `processDied`, `acp-agent.ts:8248-8254`), as the adapter words it.
const PROCESS_DIED_MESSAGE: &str =
    "The Claude Agent process exited unexpectedly. Please start a new session.";

/// The error string a `Session` handle returns for a request the ended actor can
/// no longer take (the stream ended or the session closed). The agent maps it to
/// the "session ended" JSON-RPC error, as for a prompt on such a session
/// (`RequestError.internalError(undefined, SESSION_ENDED_MESSAGE)` once
/// `queryClosed`, `acp-agent.ts:8951-8953`, `session-mode.ts:318-320`).
pub const SESSION_GONE: &str = "Session ended";

/// Finish every live async task at teardown (`AsyncTaskRuntime.finishAll`): each
/// one is attempted even when an earlier terminal publication failed, a task whose
/// publication failed keeps its non-terminal state, and the aggregated failure is
/// logged (Node surfaces it as an `AggregateError`).
fn finish_async_tasks(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    state: &str,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    logger: Option<&crate::logger::Logger>,
) {
    // T4B hook 2 (`finishLifecycle`, `acp-agent.ts:4637-4660`): children that still
    // run are closed first; a stop is a cancel, anything else a failure.
    let native_state = if state == "stopped" {
        crate::acp_subagents::SubagentState::Cancelled
    } else {
        crate::acp_subagents::SubagentState::Failed
    };
    let native_errors = finish_all_subagents(tasks, map_state, native_state, update_tx, session_id);
    if let (false, Some(logger)) = (native_errors.is_empty(), logger) {
        logger.error(&format!(
            "Session {session_id}: failed to publish terminal subagent state: {}",
            native_errors.join("; ")
        ));
    }
    let errors = {
        let mut eager_session_of = |id: &str| map_state.eager_session(id);
        tasks.finish_all_with_routed(state, session_id, &mut eager_session_of, &mut |routed| {
            update_tx
                .send(SessionOutbound::RawUpdate(routed))
                .map_err(|error| error.to_string())
        })
    };
    if let (false, Some(logger)) = (errors.is_empty(), logger) {
        logger.error(&format!(
            "Session {session_id}: failed to finish async tasks: {}",
            errors.join("; ")
        ));
    }
}

/// The next stream frame: the frames replayed after a hold first, then the stream.
async fn next_stream_line(
    replay: &mut std::collections::VecDeque<Value>,
    stream: &mut mpsc::UnboundedReceiver<Value>,
) -> Option<Value> {
    if let Some(line) = replay.pop_front() {
        return Some(line);
    }
    stream.recv().await
}

/// Put the frames held behind a `/mcp` run back in front of what is still queued,
/// in their original order: frames already queued for replay come after them in
/// the stream, and appending would reorder a second `/mcp` result that holds
/// again.
///
/// A local-command run's release is NOT gated on an inline external switch being
/// in flight: replaying while the switch holds the stream is harmless because
/// the inline gate holds every non-control frame again in the same order, so the
/// switch's `config_option_update` still precedes them. (The old gate was an
/// equivalent mutant — it changed nothing observable.)
fn release_held_frames(replay: &mut std::collections::VecDeque<Value>, held: &mut Vec<Value>) {
    for frame in held.drain(..).rev() {
        replay.push_front(frame);
    }
}

/// Await a spawned producer's `JoinHandle`, answering `None` on a panic. The
/// producer's re-entry (`UsagePrepared` / `McpPrepared`) is what releases the
/// frames held behind it, so a panic must still report "keep the CLI text"
/// rather than leave the turn held forever.
async fn join_produced<T>(handle: tokio::task::JoinHandle<T>) -> Option<T> {
    handle.await.ok()
}

/// Whether `line` is an output shape of a turn whose local-command producer (a
/// `/usage` or `/mcp` turn) is still running: Node awaits the pending
/// replacement at that shape, so the frame — and every session frame behind it —
/// waits for the producer's re-entry (`takeLocalCommandMarkdown`,
/// `acp-agent.ts:5074-5088`).
fn holds_for_local_command_run(line: &Value, machine: &TurnMachine, ext: &mut SessionExt) -> bool {
    let Some(uuid) = machine.foreground_target().map(str::to_string) else {
        return false;
    };
    // `ensureLocalCommandMarkdown` starts `produce` lazily at take time
    // (`acp-agent.ts:5073`): a `/usage` output shape that arrives before the
    // turn was activated starts its producer here, so the frame can wait for
    // the re-entry instead of being dropped by the handler.
    let needs_start = ext.t2a.usage_turns.get(&uuid).is_some_and(|turn| {
        !turn.run_started && !turn.aborted && turn.prepared.is_none() && !turn.delivered
    });
    if needs_start {
        if let Some(turn) = ext.t2a.usage_turns.get_mut(&uuid) {
            turn.run_started = true;
        }
        if let Some(tx) = &ext.t2a.command_tx {
            let _ = tx.send(Command::StartUsage { uuid: uuid.clone() });
        }
    }
    let pending = ext
        .t2a
        .usage_turns
        .get(&uuid)
        .is_some_and(crate::usage::UsageTurn::waits_for_replacement)
        || ext
            .mcp
            .get(&uuid)
            .is_some_and(|turn| turn.run_started && turn.prepared.is_none() && !turn.delivered);
    if !pending {
        return false;
    }
    match line.get("type").and_then(Value::as_str) {
        Some("result") => !machine.result_is_autonomous(line),
        Some("system") => {
            line.get("subtype").and_then(Value::as_str) == Some("local_command_output")
        }
        Some("assistant") => {
            line.get("parent_tool_use_id") == Some(&Value::Null)
                && line.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
        }
        _ => false,
    }
}

/// Whether `uuid` names a turn with a local-command producer (a `/usage` or
/// `/mcp` prompt).
fn is_local_command_turn(ext: &SessionExt, uuid: &str) -> bool {
    ext.t2a.usage_turns.get(uuid).is_some() || ext.mcp.get(uuid).is_some()
}

/// The prompt text when the ACP prompt was exactly one `type:"text"` chunk
/// (`localCommandMarkdown`, `acp-agent.ts:3722-3726`). The converted frame holds
/// one text block exactly then: an image, a resource, a second text chunk or a
/// resource link all leave more blocks (or a non-text one), and Node then has no
/// local command.
fn single_text_prompt(frame: &Value) -> Option<&str> {
    let content = frame.pointer("/message/content")?.as_array()?;
    if content.len() != 1 {
        return None;
    }
    let block = &content[0];
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    block.get("text").and_then(Value::as_str)
}

/// The start of `prompt()`'s preamble that every prompt runs, including a steer
/// that starts a detached turn (upstream calls `this.prompt()` at
/// `acp-agent.ts:4353`): the stale prediction is cleared (`3887-3898`) and a
/// `/usage` prompt registers its structured producer (`3722-3734`).
fn prompt_preamble(
    uuid: &str,
    frame: &Value,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    if ext.t2a.last_published_prompt_suggestion.is_some() {
        publish_prompt_suggestion(ext.air.is_some(), &mut ext.t2a, None, update_tx, session_id);
    } else {
        ext.t2a.prompt_suggestion_expected = false;
    }
    if single_text_prompt(frame).is_some_and(crate::usage_markdown::is_usage_command_text) {
        ext.t2a.usage_turns.insert(uuid);
    }
}

/// What claiming a local-command replacement decided for one output shape
/// (`takeLocalCommandMarkdown`, `acp-agent.ts:5074-5088`).
enum LocalCommandTake {
    /// Not a local-command turn, or its producer failed: keep the CLI text.
    Keep,
    /// The replacement was already published for this exact output (a mirror
    /// of the same text from another SDK shape): publish nothing.
    Suppress,
    /// Publish this replacement instead of the CLI text.
    Replace(String),
    /// The producer is still running: the frame must wait for it (the run loop
    /// holds it, so a handler never sees this).
    Pending,
}

/// Claim the structured replacement for the turn currently producing a
/// local-command output (`takeLocalCommandMarkdown`): `Keep` means "not a
/// local-command turn (or its request failed)", `Suppress` means another SDK
/// message shape already delivered this exact output.
fn take_local_command_markdown(
    machine: &TurnMachine,
    ext: &mut SessionExt,
    original: &str,
) -> LocalCommandTake {
    let Some(uuid) = machine.foreground_target().map(str::to_string) else {
        return LocalCommandTake::Keep;
    };
    // A prompt is exactly one local command, so at most one producer owns it.
    if let Some(turn) = ext.t2a.usage_turns.get_mut(&uuid) {
        return match turn.prepared.clone() {
            None => LocalCommandTake::Pending,
            // `produce` failed, timed out or was aborted: keep the CLI text.
            Some(None) => LocalCommandTake::Keep,
            Some(Some(markdown)) => {
                if turn.delivered {
                    if turn.original_output.as_deref() == Some(original) {
                        LocalCommandTake::Suppress
                    } else {
                        LocalCommandTake::Keep
                    }
                } else {
                    turn.delivered = true;
                    turn.original_output = Some(original.to_string());
                    LocalCommandTake::Replace(markdown)
                }
            }
        };
    }
    if let Some(turn) = ext.mcp.get_mut(&uuid) {
        let Some(prepared) = turn.prepared.as_ref() else {
            return if turn.run_started && !turn.delivered {
                LocalCommandTake::Pending
            } else {
                LocalCommandTake::Keep
            };
        };
        let Some(markdown) = prepared.render(original) else {
            return LocalCommandTake::Keep;
        };
        if turn.delivered {
            if turn.original_output.as_deref() == Some(original) {
                LocalCommandTake::Suppress
            } else {
                LocalCommandTake::Keep
            }
        } else {
            turn.delivered = true;
            turn.original_output = Some(original.to_string());
            LocalCommandTake::Replace(markdown)
        }
    } else {
        LocalCommandTake::Keep
    }
}

/// Publish a local-command output shape as an `agent_message_chunk` (the
/// replacement, or the CLI's own text) and record it as delivered assistant
/// text (`sendUpdate`, `acp-agent.ts:4615-4622`).
fn publish_local_command_chunk(
    text: &str,
    message_id: Option<String>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    emitted_assistant_text: &mut bool,
    ext: &mut SessionExt,
) {
    // The lifecycle's failed compaction error also arrives as local-command
    // stdout: consume that one duplicate before it renders
    // (`consumeDuplicateErrorOutput`, `acp-agent.ts:4610-4618,6071`).
    if ext.t2a.compaction.consume_duplicate_error_output(text) {
        return;
    }
    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    if let Some(id) = message_id.filter(|id| !id.is_empty()) {
        chunk = chunk.message_id(MessageId::new(id));
    }
    let _ = update_tx.send(SessionOutbound::Update(SessionNotification::new(
        session_id.to_string(),
        SessionUpdate::AgentMessageChunk(chunk),
    )));
    *emitted_assistant_text = true;
    ext.titles.on_assistant_text(text);
}

/// Publish the ordered updates of the compaction lifecycle and mirror its
/// `hasDeliveredOutput` onto the actor state the result fallback reads.
///
/// AIR gets the changed-`_meta` filter last, exactly like every other final
/// notification (`ChangedMetaClient`, `acp-agent.ts:991-1010`): a legacy
/// `tool_call_update` carries only the keys that changed, and one left with
/// nothing is dropped. A `compaction_update` is not a tool call, so it passes
/// through whole.
fn publish_compaction_updates(
    ext: &mut SessionExt,
    map_state: &mut MapState,
    updates: Vec<Value>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    for update in updates {
        let Some(update) = map_state.filter_hook_meta(update) else {
            continue;
        };
        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
            "sessionId": session_id,
            "update": update,
        })));
    }
    ext.t2a.compaction_output_delivered = ext.t2a.compaction.has_delivered_output();
}

/// Start a `/usage` turn's producer when it activates (`activateTurn` ->
/// `ensureLocalCommandMarkdown`, `acp-agent.ts:4805-4807`).
fn start_usage_producers(events: &[TurnEvent], ext: &mut SessionExt) {
    // A turn activation proves the interrupted command's tail has drained in
    // the SDK's FIFO stream (`activateTurn` -> `compaction.resume()`,
    // `acp-agent.ts:4806`).
    if events
        .iter()
        .any(|event| matches!(event, TurnEvent::Activated { .. }))
    {
        ext.t2a.compaction.resume();
    }
    for event in events {
        let TurnEvent::Activated { prompt_uuid } = event else {
            continue;
        };
        let Some(turn) = ext.t2a.usage_turns.get_mut(prompt_uuid) else {
            continue;
        };
        if turn.run_started {
            continue;
        }
        turn.run_started = true;
        if let Some(tx) = &ext.t2a.command_tx {
            let _ = tx.send(Command::StartUsage {
                uuid: prompt_uuid.clone(),
            });
        }
    }
}

/// How long a `can_use_tool` request waits for the consumer to reach its tool use
/// while a cancel's window may be open (`TOOL_USE_WAIT_MS`, `acp-agent.ts:437`).
const TOOL_USE_WAIT: Duration = Duration::from_millis(2_000);

/// A `can_use_tool` request parked until the consumer reaches its tool use.
type ParkedPermission = (String, Value, oneshot::Sender<Value>);

/// The per-session native file-change report state (AG-3,
/// `createNativeFileChangeReporter`). The actor owns it; a spawned task runs the
/// bounded checkpoint preview of one turn.
/// One report job that left `FileChangeReports::requests` to run: the cancel
/// flag the actor sets when a cancel/close finished it, and the done flag the
/// task sets so the actor can prune it.
pub(crate) struct CollectingReport {
    pub request_id: String,
    pub handle: crate::file_change_audit::ReportHandle,
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
pub struct FileChangeReports {
    /// `None` for a client that did not negotiate `agentFileChangeReport`.
    reporter: Option<crate::file_change_audit::Reporter>,
    /// `prompt uuid -> (request id, handle)` of a turn that asked for a report.
    requests: HashMap<String, (String, crate::file_change_audit::ReportHandle)>,
    /// `prompt uuid -> (request id, handle, cancelled, done)` of a report job
    /// that is already running: taken out of `requests`, but kept here so a
    /// cancel or close can still finish it `cancelled` and mark the forwarded
    /// reply (`review-T6INT` finding 2). `done` lets the next prompt prune it.
    collecting: HashMap<String, CollectingReport>,
    /// `prompt uuid -> raw file paths of the file-tool edits its subagents made`
    /// (`NativeFileChangeReportTurn.editedPaths`).
    edited: HashMap<String, std::collections::BTreeSet<String>>,
    /// The control channel, for `rewindFiles(userMessageId, {dryRun:true})`.
    control: Option<Control>,
    /// The client update channel the report is published on.
    update_tx: Option<mpsc::UnboundedSender<SessionOutbound>>,
    session_id: String,
    logger: Option<std::sync::Arc<crate::logger::Logger>>,
}

impl std::fmt::Debug for FileChangeReports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileChangeReports")
            .field("enabled", &self.reporter.is_some())
            .field("requests", &self.requests.len())
            .field("edited", &self.edited.len())
            .finish()
    }
}

impl FileChangeReports {
    /// The number of report jobs still in `collecting` (M8 test).
    #[cfg(test)]
    pub(crate) fn collecting_len(&self) -> usize {
        self.collecting.len()
    }

    pub(crate) fn new(
        enabled: bool,
        cwd: &std::path::Path,
        additional_directories: &[std::path::PathBuf],
        control: Option<&Control>,
        update_tx: &mpsc::UnboundedSender<SessionOutbound>,
        session_id: &str,
        logger: Option<std::sync::Arc<crate::logger::Logger>>,
    ) -> Self {
        Self {
            reporter: enabled
                .then(|| crate::file_change_audit::Reporter::new(cwd, additional_directories)),
            requests: HashMap::new(),
            collecting: HashMap::new(),
            edited: HashMap::new(),
            control: enabled.then(|| control.cloned()).flatten(),
            update_tx: Some(update_tx.clone()),
            session_id: session_id.to_string(),
            logger,
        }
    }

    /// `session.fileChangeReporter?.request(params._meta)` at prompt intake
    /// (`acp-agent.ts:3881`): the request is accepted once per session.
    pub(crate) fn note_prompt(&mut self, prompt_uuid: &str, meta: &Value) {
        // Prune report jobs that already finished: their cancel flag is moot
        // and the entry must not outlive the report.
        self.collecting
            .retain(|_, job| !job.done.load(std::sync::atomic::Ordering::SeqCst));
        let Some(reporter) = self.reporter.as_mut() else {
            return;
        };
        if let Some((request_id, handle)) = reporter.request(meta) {
            self.requests
                .insert(prompt_uuid.to_string(), (request_id, handle));
        }
    }

    /// `recordSubagentFileEdits` (`acp-agent.ts:8016-8024`): a subagent message's
    /// successful file-tool edits, recorded before the tool-use cache is dropped.
    pub(crate) fn note_subagent_edits(
        &mut self,
        prompt_uuid: Option<&str>,
        parent_tool_use_id: Option<&str>,
        content: &Value,
        map_state: &MapState,
    ) {
        let Some(prompt_uuid) = prompt_uuid.filter(|uuid| self.requests.contains_key(*uuid)) else {
            return;
        };
        if parent_tool_use_id.is_none() {
            return;
        }
        // Node reads `session.toolUseCache` (`{name, input}` by id).
        let mut tool_uses: HashMap<String, (String, Value)> = HashMap::new();
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                    if let Some(tool_use) = map_state.tool_use_from_cache(id) {
                        tool_uses.insert(
                            id.to_string(),
                            (
                                tool_use
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                tool_use.get("input").cloned().unwrap_or(Value::Null),
                            ),
                        );
                    }
                }
            }
        }
        let edited = self.edited.entry(prompt_uuid.to_string()).or_default();
        crate::file_change_audit::record_subagent_file_edits(
            parent_tool_use_id,
            content,
            &tool_uses,
            edited,
        );
    }

    /// Take the settled turn's report request and its recorded edits. The job
    /// is registered as `collecting` until it finishes, so a cancel/close can
    /// still finish it `cancelled` (`review-T6INT` finding 2).
    pub(crate) fn take(&mut self, prompt_uuid: &str) -> Option<DeferredReportJob> {
        let (request_id, handle) = self.requests.remove(prompt_uuid)?;
        let edited = self
            .edited
            .remove(prompt_uuid)
            .map(|paths| paths.into_iter().collect())
            .unwrap_or_default();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.collecting.insert(
            prompt_uuid.to_string(),
            CollectingReport {
                request_id: request_id.clone(),
                handle: handle.clone(),
                cancelled: std::sync::Arc::clone(&cancelled),
                done: std::sync::Arc::clone(&done),
            },
        );
        Some(DeferredReportJob {
            request_id,
            handle,
            edited,
            cancelled,
            done,
        })
    }

    /// A cancel/close that lands while a report is previewing finishes it
    /// `cancelled` and marks the reply (`review-T6INT` finding 2).
    pub(crate) fn cancel_collecting(&mut self) {
        let jobs: Vec<CollectingReport> = self.collecting.drain().map(|(_, job)| job).collect();
        for job in jobs {
            job.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.finish(&job.handle, &job.request_id, "cancelled");
        }
    }

    /// Finish a cancelled/failed job's report and mark it done, so
    /// [`Self::note_prompt`]'s prune can drop the `collecting` entry (M8).
    pub(crate) fn finish_job(&self, job: &DeferredReportJob, reason: &str) {
        self.finish(&job.handle, &job.request_id, reason);
        job.done.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// `finish(state, reason)`: publish the `unavailable` report unless a
    /// terminal outcome already went out.
    pub(crate) fn finish(
        &self,
        handle: &crate::file_change_audit::ReportHandle,
        request_id: &str,
        reason: &str,
    ) {
        let publish = self.publish();
        if let Some(reporter) = &self.reporter {
            reporter.finish(handle, request_id, reason, &publish);
        }
    }

    /// Finish every open request (`cancelled`): the session closed or was torn
    /// down before its turns settled (`acp-agent.ts:3911`). A report job that is
    /// already previewing is finished too (`review-T6INT` finding 2).
    pub(crate) fn finish_all(&mut self, reason: &str) {
        let open: Vec<(String, crate::file_change_audit::ReportHandle)> =
            self.requests.drain().map(|(_, v)| v).collect();
        for (request_id, handle) in open {
            self.finish(&handle, &request_id, reason);
        }
        self.edited.clear();
        if reason == "cancelled" {
            self.cancel_collecting();
        } else {
            self.collecting.clear();
        }
    }

    pub(crate) fn publish(&self) -> crate::file_change_audit::Publish {
        let update_tx = self.update_tx.clone();
        let session_id = self.session_id.clone();
        std::sync::Arc::new(move |meta: Value| {
            let Some(update_tx) = &update_tx else {
                return;
            };
            let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": meta,
                },
            })));
        })
    }

    pub(crate) fn log_error(&self) -> crate::file_change_audit::LogError {
        let logger = self.logger.clone();
        std::sync::Arc::new(move |message: &str| {
            if let Some(logger) = &logger {
                logger.error(message);
            }
        })
    }
}

/// The `tool_use_id` of a `can_use_tool` control request.
fn can_use_tool_id(frame: &Value) -> String {
    frame
        .pointer("/request/tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The answer to an AskUserQuestion `can_use_tool` that was cancelled or could
/// not be presented: upstream throws `Tool use aborted` (`acp-agent.ts:10339-10353`),
/// which the SDK reports as an `error` control response.
fn ask_aborted_response(request_id: &str) -> Value {
    crate::control::build_error_response(request_id, "Tool use aborted")
}

/// The allow answer to an AskUserQuestion `can_use_tool`: upstream returns
/// `{behavior:"allow", updatedInput}` and the SDK appends only `toolUseID`
/// (`acp-agent.ts:10355`) — no `decisionClassification`.
fn ask_allow_response(request_id: &str, tool_use_id: &str, updated_input: &Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input,
                "toolUseID": tool_use_id,
            },
        },
    })
}

/// The answer to a `can_use_tool` that reached no session: Node's `canUseTool`
/// early return `{behavior:"deny", message:"Session not found"}`
/// (`acp-agent.ts:10030-10035`), which carries no `toolUseID` and no
/// `decisionClassification`.
fn session_not_found_response(request_id: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "deny",
                "message": "Session not found",
            },
        },
    })
}

/// The deny answer to an AskUserQuestion `can_use_tool`: upstream's
/// `{behavior:"deny", message}` plus the SDK's `toolUseID`, with no
/// `decisionClassification`.
fn ask_deny_response(request_id: &str, tool_use_id: &str, message: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "deny",
                "message": message,
                "toolUseID": tool_use_id,
            },
        },
    })
}

/// Deny a request for a tool use of the work a cancel stopped: its tool does not
/// run (`acp-agent.ts:9989-9996`, issue #1061).
fn deny_stopped_work(
    frame: &Value,
    reply: oneshot::Sender<Value>,
    logger: Option<&crate::logger::Logger>,
    session_id: &str,
) {
    let request_id = frame
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_use_id = can_use_tool_id(frame);
    let tool_name = frame
        .pointer("/request/tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Some(logger) = logger {
        logger.log(&format!(
            "Session {session_id}: denied {tool_name} {tool_use_id}, which the cancelled work asked for"
        ));
    }
    let mut response = permission::deny_control_response(request_id, &tool_use_id);
    response["response"]["response"]["message"] = json!("The user cancelled this prompt.");
    let _ = reply.send(response);
}

/// Re-enter the parked requests whose tool use the consumer reached (or, with
/// `elapsed`, the ones whose bounded wait ran out).
fn release_parked_permissions(
    parked: &mut Vec<ParkedPermission>,
    elapsed: Option<&str>,
    machine: &TurnMachine,
    command_tx: &mpsc::UnboundedSender<Command>,
) {
    let mut still_parked = Vec::new();
    for (tool_use_id, frame, reply) in parked.drain(..) {
        // The CLI withdrew the request while it waited (nobody reads the reply
        // any more): drop it. Re-entering it would show the client a tool call
        // and a dialog nobody waits for; upstream throws "Tool use aborted"
        // before sending anything (`requestPermissionFromClient`,
        // `acp-agent.ts:9800-9815`).
        if reply.is_closed() {
            continue;
        }
        let release = elapsed == Some(tool_use_id.as_str())
            || machine.reached_tool_use(&tool_use_id).is_some();
        if release {
            let _ = command_tx.send(Command::PermissionReleased { frame, reply });
        } else {
            still_parked.push((tool_use_id, frame, reply));
        }
    }
    *parked = still_parked;
}

/// The longest one MCP OAuth control request (authenticate / status) may take.
/// Upstream awaits the SDK's own request; bounding it keeps a wedged child from
/// pinning the OAuth task forever (the poll itself keeps its 10 min bound).
const MCP_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Send `elicitation/complete` and wait until the forwarder processed it: the
/// URL dialog's dismissal must be on the wire before the request is dropped,
/// which sends `$/cancel_request` (`acp-agent.ts:2645-2658`). The FIFO
/// [`SessionOutbound::Flush`] barrier resolves only after the notification.
async fn complete_elicitation_flushed(tx: &mpsc::UnboundedSender<SessionOutbound>, params: Value) {
    let _ = tx.send(SessionOutbound::CompleteElicitation { params });
    let (ack_tx, ack_rx) = oneshot::channel();
    if tx.send(SessionOutbound::Flush(ack_tx)).is_ok() {
        let _ = ack_rx.await;
    }
}

/// Bridge one server's startup OAuth flow (F23c/A-01, `authenticateMcpServer`,
/// `acp-agent.ts:2600-2660`): `mcpAuthenticate`, a URL elicitation, the status
/// poll up to 10 minutes, and `completeElicitation` on every outcome. Runs in a
/// task spawned by the session actor; the session's abort signal tears it down.
async fn authenticate_mcp_server(
    control: &Control,
    session_id: &str,
    server_name: &str,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    cancel: watch::Receiver<bool>,
) -> Result<bool, String> {
    let auth_control = control.clone();
    let auth_name = server_name.to_string();
    let status_control = control.clone();
    let status_name = server_name.to_string();
    let elicit_tx = update_tx.clone();
    let complete_tx = update_tx.clone();
    crate::elicitation::authenticate_single_mcp_server(
        session_id,
        server_name,
        move || async move {
            match tokio::time::timeout(
                MCP_CONTROL_TIMEOUT,
                auth_control.mcp_authenticate(&auth_name, None),
            )
            .await
            {
                // `Control::send_request` returns the whole control_response
                // frame; the SDK's `mcpAuthenticate` resolves with its inner
                // payload (`response.response`).
                Ok(Ok(frame)) => Ok(frame
                    .pointer("/response/response")
                    .cloned()
                    .unwrap_or(Value::Null)),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("The request timed out.".to_string()),
            }
        },
        move || {
            let control = status_control.clone();
            let name = status_name.clone();
            async move {
                let frame =
                    match tokio::time::timeout(MCP_CONTROL_TIMEOUT, control.mcp_status()).await {
                        Ok(Ok(frame)) => frame,
                        Ok(Err(error)) => return Err(error.to_string()),
                        Err(_) => return Err("The request timed out.".to_string()),
                    };
                let servers = frame
                    .pointer("/response/response/mcpServers")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                // A server missing from the status fails the wait
                // (`acp-agent.ts:2664-2670`).
                Ok(servers
                    .iter()
                    .find(|server| server.get("name").and_then(Value::as_str) == Some(&name))
                    .and_then(|server| server.get("status").and_then(Value::as_str))
                    .map(String::from))
            }
        },
        move |params| {
            let tx = elicit_tx.clone();
            async move {
                let (ptx, prx) = oneshot::channel();
                let _ = tx.send(SessionOutbound::CreateElicitation { params, reply: ptx });
                prx.await
                    .unwrap_or_else(|_| Err("Tool use aborted".to_string()))
            }
        },
        move |params| {
            let tx = complete_tx.clone();
            async move {
                complete_elicitation_flushed(&tx, params).await;
                Ok(())
            }
        },
        cancel,
    )
    .await
}

/// The next command: one a clear-context restart held back comes first, in the
/// order it arrived, ahead of anything the client sent since.
async fn next_command(
    held: &mut std::collections::VecDeque<Command>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> Option<Command> {
    match held.pop_front() {
        Some(command) => Some(command),
        None => commands.recv().await,
    }
}

/// The refusal of `/remote-control` while Claude is working
/// (`acp-agent.ts:3824-3832`): an unsettled turn, a model cycle Claude Code
/// started on its own (`foregroundWork.has`, which is not in `turnQueue`), or a
/// clear-context restart bringing the fresh context up.
fn remote_control_busy_error(machine: &TurnMachine, restarting: bool) -> Option<SessionError> {
    if machine.has_unsettled() || machine.runs_autonomous_cycle() || restarting {
        return Some(SessionError::InvalidRequest(
            "Remote Control can't be changed while Claude is working. Try again when the turn ends."
                .to_string(),
        ));
    }
    None
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
    air: Option<AirClient>,
    caps: crate::client_caps::ClientCapabilities,
    cwd: PathBuf,
    resumed: bool,
    forward_subagent_text: bool,
    raw_sdk_messages: crate::sdk_options::RawSdkMessages,
    env: crate::env_snapshot::EnvSnapshot,
    logger: Option<std::sync::Arc<crate::logger::Logger>>,
    additional_directories: Vec<PathBuf>,
) {
    let workspace_cwd = cwd.clone();
    let mut ext = SessionExt::new(air, cwd, env)
        .with_caps(caps)
        .with_logger(logger);
    // AG-3: the native file-change report state of the session. A client that
    // negotiated `agentFileChangeReport` gets the per-turn checkpoint report;
    // every other client never builds a reporter (`acp-agent.ts:11149-11160`).
    ext.file_change_reports = FileChangeReports::new(
        crate::file_change_audit::supports_agent_file_change_report(ext.air.as_ref()),
        &workspace_cwd,
        &additional_directories,
        Some(&control),
        &update_tx,
        &session_id,
        ext.logger.clone(),
    );
    ext.t2a.command_tx = Some(command_tx.clone());
    // F20: the per-session compaction lifecycle. The presentation is the ACP
    // compaction contract for a client that advertises `session.compaction`,
    // and the legacy "Compact conversation" tool call for every other client
    // (`acp-agent.ts:4544-4551`).
    ext.t2a.compaction = crate::context_compaction::ContextCompactionLifecycle::new(
        session_id.clone(),
        if ext.caps.compaction {
            crate::context_compaction::CompactionPresentation::CompactionUpdate
        } else {
            crate::context_compaction::CompactionPresentation::ToolCall
        },
        ext.caps.air_client,
    );
    let mut machine = TurnMachine::new();
    if resumed {
        machine.mark_resumed();
    }
    let mut map_state = fresh_map_state(&ext, forward_subagent_text);
    machine.set_forwards_subagent_text(forward_subagent_text);
    // AIR async-task runtime (item 3.4/3.5): enabled only for a client that
    // advertised the AIR `asyncTasks` capability.
    let mut tasks = AsyncTaskRuntime::new(ext.air_supports("asyncTasks"))
        // T4B hook 1: the native subagent runtime lives beside the task runtime.
        // T6B (B-98): with the real history lookups, so an agent that ran before
        // this load/resume is restored from its transcript (`subagentLineage`).
        .with_subagents(crate::subagent_history::native_runtime_with_history(
            ext.caps.subagent_form,
            &session_id,
            &ext.env,
        ));
    // uuid -> prompt reply oneshot (8.7).
    let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
        HashMap::new();
    let mut emitted_assistant_text = false;
    // Whether the current turn's assistant message carried an `error` (the
    // Node's `lastAssistantError`, reset per prompt; phase 9). The wire
    // `data.errorKind` is only attached when this is true.
    let mut assistant_had_error = false;
    // `sdk.mjs` `lastErrorResultText`: the diagnostic carried by the most recent
    // error `result` frame, cleared by every later frame but a state change. A
    // child that exits right after one reports `Claude Code returned an error
    // result: <text>` instead of the generic end-of-stream message (F3a (f)).
    let mut error_text = ErrorTextTracker::default();
    // How the child exited, once the relay in `run` reported it (`None` without a
    // process behind the stream).
    let mut child_exit: Option<bool> = None;
    // Where the final modes + config options go when the actor ends.
    let mut final_state_sink: Option<watch::Sender<Option<LiveState>>> = None;
    // Subagent attribution (10.5): `task_id -> parentToolUseId` for live
    // background tasks, mirroring the Node's `liveBackgroundTasks` so a
    // subagent's `can_use_tool` is attributed to the Agent/Task tool call that
    // spawned it.
    let mut live_background: HashMap<String, String> = HashMap::new();
    // AG-3: the ACP `session/prompt` `_meta` of each prompt about to be
    // enqueued (sent as `Command::PromptMeta` right before `Command::Prompt`).
    let mut prompt_metas: HashMap<String, Value> = HashMap::new();
    // Pending permission round-trips (phase 10): `request_id -> JoinHandle`.
    // `Command::Cancel` aborts every one so a pending permission resolves as
    // `Cancelled` (R28 deny written back to the control writer, INV-21 / 27;
    // CUJ 2).
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // F23b: inbound MCP `elicitation` / `request_user_dialog` round trips waiting
    // on the client. `request_id -> (reply, the CANCEL outcome to send when a
    // cancel or the session close withdraws the request, the task waiting on the
    // client)`. Aborting the task drops the client request's reply, which sends
    // `$/cancel_request` (as for permissions).
    let mut pending_elicitations: HashMap<
        String,
        (oneshot::Sender<Value>, Value, tokio::task::JoinHandle<()>),
    > = HashMap::new();
    // F23a: AskUserQuestion round trips (`request_id -> (reply, aborted answer)`).
    let mut pending_ask: HashMap<String, (oneshot::Sender<Value>, Value)> = HashMap::new();
    // The session's abort signal, handed to the elicitation helpers so a spawned
    // client wait settles as CANCEL when the session CLOSES. It is a close-only
    // latch: `session/cancel` aborts the pending round trips itself (below), and
    // must NOT tear an MCP OAuth flow down (upstream ends OAuth on close only,
    // `acp-agent.ts:2614-2617`). Never reset once set.
    let (elicit_cancel_tx, elicit_cancel_rx) = watch::channel(false);
    // A `/remote-control` control call in flight (S-03). A prompt that arrives
    // meanwhile waits for it (`acp-agent.ts:3904`): its reply bypasses the turn
    // queue, so letting the turn start would interleave the two.
    let mut remote_control_pending = false;
    let mut held_remote_control: std::collections::VecDeque<Command> =
        std::collections::VecDeque::new();
    // F23c: the ACP-provided MCP servers of this session (used to pick the OAuth
    // follow-up of a `/mcp reconnect`), and the single-flight guard of the
    // background OAuth run (upstream `mcpAuthentications`).
    let mut acp_mcp_servers: Vec<String> = Vec::new();
    let mut mcp_auth_running = false;
    // The open permission requests of native subagent children (`awaitingUser`):
    // `request_id -> token`, ended when the client answers or the request is
    // withdrawn (cancel/close).
    let mut pending_await_tokens: HashMap<String, crate::native_subagents::AwaitToken> =
        HashMap::new();
    // `can_use_tool` requests waiting for the consumer to reach their tool use in
    // the stopped-work window (F16b, B-62).
    let mut parked_permissions: Vec<ParkedPermission> = Vec::new();
    // Stream frames to process before reading more of the stream, and the session
    // frames held behind a `/mcp` result that waits for its control run (AG-4).
    let mut replay: std::collections::VecDeque<Value> = std::collections::VecDeque::new();
    let mut held_behind_mcp: Vec<Value> = Vec::new();
    // External model switches (`syncModelAfterExternalSwitch`) the consumer loop
    // waits for: only the refusal fallback runs inline upstream, while the
    // hook's sync is detached (`setImmediate`, `acp-agent.ts:11328-11337`) and
    // keeps reading the stream. Counted, never a bool, so an overlapping
    // user-driven switch (which never touches this) cannot clear it; each
    // decrements on its own `ModelSwitchSynced`, so a close, a failed control
    // round trip or a dead child cannot leave one stuck. While it is positive
    // every non-control frame is held (the switch's own `control_response`
    // lines still flow) and released when the last one finishes.
    let mut inline_external_switches_in_flight: usize = 0;
    // The stream closed while frames were still held behind an unfinished `/mcp`
    // run: they are processed without waiting (the run's replacement will never
    // come), so their results settle their turns.
    let mut stream_ended = false;
    // Child-stdout EOF was seen while an inline external switch was still in
    // flight (T5FA4). The switch's replies arrive on this stream and can no
    // longer come, so `fail_pending` settles its round trips and the stream
    // branch is gated off until its `ModelSwitchSynced` publishes the
    // `config_option_update` and releases the held frames. Cleared only when a
    // fresh child is adopted (`ExitPlanRestart`, `RestartDone`), because the
    // closed stream stays closed.
    let mut eof_seen = false;
    // The force-cancel backstop (11.2, INV-17, #680): the absolute deadline at
    // which a wedged stream's active turn is force-settled `cancelled`. Armed at
    // most once per cancel sequence (`!forceCancelTimer`), cleared as soon as no
    // active turn remains (upstream `disarmForceCancel` on every settle path).
    let mut force_cancel_deadline: Option<tokio::time::Instant> = None;
    // The clear-context restart (B-74 / B-75, `ExitPlanCoordinator`): what brings
    // up a fresh child, the child a restart already started (its relay owns the
    // process, so dropping the guard kills it), the restart in flight, and the
    // prompts that arrived meanwhile. While a restart runs `stream` is a
    // placeholder nothing writes to: the old child's frames are the old
    // context's, and the fresh child's are read once it is adopted.
    let mut restarter: Option<clear_context::Restarter> = None;
    let mut child_guard: Option<clear_context::AbortOnDrop> = None;
    let mut restart: Option<InFlightRestart> = None;
    let mut next_restart_id: u64 = 0;
    // The client's stop-workflows-on-cancel choice, applied again to the fresh
    // context's machine (upstream recomputes it for the fresh session).
    let mut stops_workflows_on_cancel = false;
    let mut deferred_commands: Vec<Command> = Vec::new();
    // The deferred prompts once the restart is over: handled ahead of the channel.
    let mut replay_commands: std::collections::VecDeque<Command> =
        std::collections::VecDeque::new();
    // Ends the turn a restart took out of the machine when the restart does not
    // complete: `cancelled` with the usage it had earned, or the error that
    // stopped the restart (`settleCancelledTurn` / `settleFailedTurn`).
    macro_rules! settle_restart_turn {
        ($plan:expr, cancelled) => {{
            // The prompts queued behind it end `cancelled` too, as a cancel
            // sweeps queued turns.
            let mut events = vec![$plan
                .turn
                .restart_settled(crate::turn::StopReason::Cancelled)];
            events.extend(
                $plan
                    .queued
                    .iter()
                    .map(|queued| queued.queued_settled(crate::turn::StopReason::Cancelled)),
            );
            settle_events(
                events,
                &mut tasks,
                &mut map_state,
                &live_background,
                &mut pending,
                &update_tx,
                &session_id,
                &high_water,
                &ext.failure_ctx,
                ext.air.as_ref(),
                None,
                &mut ext.titles,
                &mut ext.t2a.failures,
                &mut ext.t2a.usage_turns,
                &mut ext.file_change_reports,
            );
        }};
        ($plan:expr, failed $message:expr) => {{
            let message: String = $message;
            for turn in std::iter::once(&$plan.turn).chain($plan.queued.iter()) {
                if let Some(reply) = pending.remove(turn.prompt_uuid()) {
                    reply_error_after_flush(
                        &update_tx,
                        reply,
                        SessionError::RestartFailed(message.clone()),
                    );
                }
            }
        }};
    }

    // B-17 (`messageLoopYielder`, `acp-agent.ts:5651-5660`, `utils.ts:114-145`):
    // a backlog of buffered frames drains without an I/O wait between items, so
    // a stretch that ran past its budget gives the runtime a turn before the
    // next frame is processed. Messages keep their order: the next one is
    // processed only after the pause. A loop that waits on the stream between
    // items accepts at most one no-op pause per budget.
    let mut message_loop_yielder = crate::event_loop_yield::EventLoopYielder::with_clock(
        crate::event_loop_yield::MESSAGE_LOOP_BUDGET_MS,
    );
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
                            settle_events(
                                events,
                                &mut tasks,
                                &mut map_state,
                                &live_background,
                                &mut pending,
                                &update_tx,
                                &session_id,
                                &high_water,
                                &ext.failure_ctx,
                                ext.air.as_ref(),
                                None,
                                &mut ext.titles,
        &mut ext.t2a.failures,
        &mut ext.t2a.usage_turns,
                            &mut ext.file_change_reports,
                            );
                            // The cancelled turn's result may never come (that is why the
                            // backstop fired): close its delivery stretch here so partial
                            // streamed text cannot suppress the next turn's #453 fallback.
                            emitted_assistant_text = false;
                        }
                    }
                    cmd = next_command(&mut replay_commands, &mut commands) => {
                        let Some(cmd) = cmd else {
                            break;
                        };
                        // A prompt that arrives while a clear-context restart brings up
                        // the fresh context waits for it: the control channel is moving
                        // between children.
                        if restart.is_some()
                            && matches!(
                                cmd,
                                Command::Prompt { .. }
                                    | Command::PromptV2 { .. }
                                    | Command::McpPrompt { .. }
                                    | Command::Steer { .. }
                            )
                        {
                            deferred_commands.push(cmd);
                            continue;
                        }
                        // F24/S-03: a prompt that follows a `/remote-control`
                        // command waits for its reply (`acp-agent.ts:3904`).
                        if remote_control_pending
                            && matches!(
                                cmd,
                                Command::Prompt { .. } | Command::PromptV2 { .. }
                            )
                        {
                            held_remote_control.push_back(cmd);
                            continue;
                        }
                        // The work a cancel stopped shows the client nothing after it was
                        // told that the work ended: neither a tool call nor a request, and
                        // its tool does not run (`canUseTool`, `acp-agent.ts:9989-9996`).
                        // While the cancel window may be open, a request for a tool use the
                        // consumer has not reached yet waits (bounded) for it.
                        let cmd = match cmd {
                            Command::Permission { frame, reply } => {
                                let tool_use_id = can_use_tool_id(&frame);
                                match machine.stopped_work_verdict(&tool_use_id, false) {
                                    StoppedWorkVerdict::Stopped => {
                                        deny_stopped_work(&frame, reply, ext.logger.as_deref(), &session_id);
                                        continue;
                                    }
                                    StoppedWorkVerdict::Wait => {
                                        parked_permissions.push((tool_use_id.clone(), frame, reply));
                                        let command_tx = command_tx.clone();
                                        tokio::spawn(async move {
                                            tokio::time::sleep(TOOL_USE_WAIT).await;
                                            let _ = command_tx
                                                .send(Command::ToolUseWaitElapsed { tool_use_id });
                                        });
                                        continue;
                                    }
                                    StoppedWorkVerdict::Proceed => Command::Permission { frame, reply },
                                }
                            }
                            Command::PermissionReleased { frame, reply } => {
                                let tool_use_id = can_use_tool_id(&frame);
                                match machine.stopped_work_verdict(&tool_use_id, true) {
                                    StoppedWorkVerdict::Stopped => {
                                        deny_stopped_work(&frame, reply, ext.logger.as_deref(), &session_id);
                                        continue;
                                    }
                                    _ => Command::Permission { frame, reply },
                                }
                            }
                            Command::ToolUseWaitElapsed { tool_use_id } => {
                                release_parked_permissions(
                                    &mut parked_permissions,
                                    Some(&tool_use_id),
                                    &machine,
                                    &command_tx,
                                );
                                continue;
                            }
                            other => other,
                        };
                        match cmd {
                            Command::PermissionReleased { .. } | Command::ToolUseWaitElapsed { .. } => {}
                            Command::StopsWorkflowsOnCancel(stops) => {
                                stops_workflows_on_cancel = stops;
                                machine.set_stops_workflows_on_cancel(stops);
                            }
                            Command::V2Reporting(v2) => {
                                map_state.v2.set_reporting(v2);
                            }
                            Command::FinalStateSink(sink) => {
                                final_state_sink = Some(sink);
                            }
                            Command::InitRestarter(init) => {
                                restarter = Some(*init);
                            }
                            Command::RestartDone { id, result } => {
                                // A restart the actor gave up (cancel, close) reports
                                // after the fact: its child dies with the result.
                                let Some(current) = restart.take_if(|r| r.id == id) else {
                                    continue;
                                };
                                let InFlightRestart { plan, .. } = current;
                                match result {
                                    Err(message) => {
                                        // The restart failed: the turn is settled with its
                                        // error (turn-scoped, not a lost query), and the
                                        // session ends like one whose stream died.
                                        if let Some(logger) = ext.logger.as_deref() {
                                            logger.error(&format!(
                                                "Session {session_id}: clear-context restart failed: {message}"
                                            ));
                                        }
                                        settle_restart_turn!(plan, failed message);
                                        let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
                                        stream = closed_rx;
                                    }
                                    Ok(child) => {
                                        // Adopt the fresh context: its own machine, mapper
                                        // and task books (the old query's are gone with it),
                                        // the accepted plan's mode, and the turn at the head
                                        // of the queue (`continuePlanInFreshContext`).
                                        let fresh_fast_mode = child.fast_mode_enabled;
                                        let fresh_fast_mode_reason = child.fast_mode_disabled_reason;
                                        stream = child.stream;
                                        child_guard = Some(child.guard);
                                        replay.clear();
                                        replay.extend(child.buffered);
                                        held_behind_mcp.clear();
                                        stream_ended = false;
                                        // The old child's EOF is behind us: the fresh
                                        // child's stream is live again.
                                        eof_seen = false;
                                        machine = fresh_context_machine(
                                            forward_subagent_text,
                                            stops_workflows_on_cancel,
                                        );
                                        // F39: the v2 session state is the
                                        // session's, not the old child's: the
                                        // foreground work and the queued prompt
                                        // replies survive the fresh context.
                                        let v2_state = std::mem::take(&mut map_state.v2);
                                        map_state = fresh_map_state(&ext, forward_subagent_text);
                                        map_state.v2 = v2_state;
                                        tasks = AsyncTaskRuntime::new(ext.air_supports("asyncTasks"))
        // T4B hook 1: the native subagent runtime lives beside the task runtime.
        .with_subagents(crate::subagent_history::native_runtime_with_history(
            ext.caps.subagent_form,
            &session_id,
            &ext.env,
        ));
                                        live_background.clear();
                                        // The old context's open client forms are gone
                                        // with its control channel: withdraw them and
                                        // forget its awaiting-user tokens.
                                        let _ = abort_pending_elicitations(&mut pending_elicitations);
                                        for (_, (reply, response)) in pending_ask.drain() {
                                            let _ = reply.send(response);
                                        }
                                        pending_await_tokens.clear();
                                        // F39: the old context's open requests and
                                        // task placements are gone with its control
                                        // channel; the fresh context carries none
                                        // (finding A-5).
                                        map_state.v2.reset_for_fresh_context();
                                        drain_v2(&mut map_state, &update_tx, &session_id);
                                        // F3 (B-109): the old context's lifecycle is
                                        // closed `cancelled` — dropping a pending hook
                                        // summary — and replaced by a brand-new one,
                                        // exactly as the fresh consumer upstream builds a
                                        // new `ContextCompactionLifecycle`
                                        // (`acp-agent.ts:4542`). The terminal is published
                                        // after the restart's own state updates, like the
                                        // old consumer's result `finally` (`acp-agent.ts:7488`).
                                        let old_compaction_updates =
                                            ext.t2a.compaction.interrupt();
                                        reset_ext_for_fresh_context(&mut ext);
                                        // B-74: the fresh session's Fast mode is the
                                        // restored one (`applyFastMode` sets the toggle
                                        // and refreshes the option before the state is
                                        // published, `clear-context-coordinator.ts:143-152`).
                                        ext.config.fast_mode_enabled = fresh_fast_mode;
                                        ext.config.fast_mode_disabled_reason =
                                            fresh_fast_mode_reason;
                                        refresh_fast_mode_option(
                                            &mut ext.config,
                                            fresh_fast_mode,
                                            ext.caps.boolean_config,
                                        );
                                        emitted_assistant_text = false;
                                        assistant_had_error = false;
                                        error_text = ErrorTextTracker::default();
                                        child_exit = None;
                                        force_cancel_deadline = None;
                                        let state::RestartPlan { reset, turn, queued } = plan;
                                        let prompt_uuid = turn.prompt_uuid().to_string();
                                        // A held turn's continuation takes the work
                                        // over (`startForeground`, `acp-agent.ts:7119-7130`).
                                        let held_at_restart = turn.held_at_restart();
                                        publish_restart_state(&mut ext, &reset, &update_tx, &session_id);
                                        publish_compaction_updates(
                                            &mut ext,
                                            &mut map_state,
                                            old_compaction_updates,
                                            &update_tx,
                                            &session_id,
                                        );
                                        machine.adopt_restarted_turn(turn);
                                        resume_held_turn_after_restart(
                                            &mut map_state,
                                            &update_tx,
                                            &session_id,
                                            held_at_restart,
                                            &prompt_uuid,
                                        );
                                        // The prompts queued behind it keep their place:
                                        // their frames never reached this child, so they
                                        // are sent after the continuation, in order.
                                        let mut resend = Vec::new();
                                        for mut queued_turn in queued {
                                            match queued_turn.take_frame() {
                                                Some(frame) => {
                                                    resend.push(frame.clone());
                                                    machine.enqueue(queued_turn.with_frame(frame));
                                                }
                                                None => {
                                                    if let Some(reply) =
                                                        pending.remove(queued_turn.prompt_uuid())
                                                    {
                                                        reply_error_after_flush(
                                                            &update_tx,
                                                            reply,
                                                            SessionError::RestartFailed(
                                                                "Prompt could not be carried into the fresh context"
                                                                    .to_string(),
                                                            ),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        let _ = control
                                            .send_user(clear_context::continuation_frame(
                                                &session_id,
                                                &reset.plan,
                                                &prompt_uuid,
                                            ))
                                            .await;
                                        for frame in resend {
                                            let _ = control.send_user(frame).await;
                                        }
                                    }
                                }
                                if restart.is_none() {
                                    replay_commands.extend(deferred_commands.drain(..));
                                }
                            }
                            Command::PromptMeta { uuid, meta } => {
                                prompt_metas.insert(uuid, meta);
                            }
                            Command::Prompt {
                                uuid,
                                frame,
                                is_local_only,
                                prompt_text,
                                reply,
                            } => {
                                // P-06 (`session-mode.ts:287-290`,
                                // `acp-agent.ts:3863-3864`): the initial auto-mode
                                // clamp's fallback warning is deferred to the first
                                // prompt, and shown at most once per session.
                                if ext.config.pending_auto_fallback_warning {
                                    ext.config.pending_auto_fallback_warning = false;
                                    if !ext.auto_mode_fallback_warning_shown {
                                        ext.auto_mode_fallback_warning_shown = true;
                                        let _ = update_tx.send(SessionOutbound::RawUpdate(
                                            auto_mode_fallback_update(
                                                &session_id,
                                                ext.supports_notices(),
                                            ),
                                        ));
                                    }
                                }
                                // T-09 (`acp-agent.ts:3867-3869`): the prompt
                                // republishes the non-completed task snapshot
                                // before the user frame.
                                publish_prompt_start_task_plan(
                                    &session_id,
                                    &mut ext,
                                    &update_tx,
                                );
                                // AG-3 (`acp-agent.ts:3881`): the prompt may ask
                                // for a per-turn file-change report in its `_meta`.
                                let prompt_meta = prompt_metas.remove(&uuid);
                                ext.file_change_reports.note_prompt(
                                    &uuid,
                                    prompt_meta.as_ref().unwrap_or(&Value::Null),
                                );
                                // The title context comes from the plain ACP text
                                // chunks (Node `onPrompt(params.prompt)`), never from
                                // the converted frame, whose blocks carry embedded
                                // `<context ref=…>` file contents and `[@name](uri)`
                                // links.
                                ext.titles.on_prompt(&prompt_text);
                                // The stale-prediction clear and the `/usage`
                                // registration (`acp-agent.ts:3722-3734`,
                                // `3887-3898`), shared with a steer that starts a
                                // detached turn.
                                prompt_preamble(
                                    &uuid,
                                    &frame,
                                    &mut ext,
                                    &update_tx,
                                    &session_id,
                                );
                                // G8 (Decision 16): a submitted `/goal …` prompt
                                // publishes its optimistic AIR update before the CLI's
                                // own frames are read (`publishGoalFromPrompt`,
                                // `acp-agent.ts:3952-3954`). Read the text before the
                                // frame moves to the writer.
                                let first_text = crate::goal::first_prompt_text(&frame).to_string();
                                let kept_frame = frame.clone();
                                let _ = control.send_user(frame).await;
                                machine.enqueue(
                                    Turn::new(uuid.clone(), is_local_only).with_frame(kept_frame),
                                );
                                pending.insert(uuid.clone(), reply);
                                if let Some(goal) = ext.goal.note_prompt(&first_text, &uuid) {
                                    publish_goal(ext.air.is_some(), &goal, &update_tx, &session_id);
                                }
                                // G12 (Decision 21): a `/mcp` turn's replacement is
                                // produced by a spawned control run. It starts only
                                // after the prompt frame is on the wire, so the CLI
                                // sees the prompt first (the fake transcript's steps
                                // then expect the control requests).
                                if let Some(turn) = ext.mcp.get_mut(&uuid) {
                                    turn.run_started = true;
                                    let command = turn.command.clone();
                                    let control = control.clone();
                                    let command_tx = command_tx.clone();
                                    let prompt_uuid = uuid.clone();
                                    tokio::spawn(async move {
                                        // The producer always re-enters: a panic in the
                                        // control run answers "keep the CLI text" so the
                                        // held result (and the turn) still settle.
                                        let inner = tokio::spawn(async move {
                                            crate::mcp_command::run_mcp_command(&control, &command)
                                                .await
                                        });
                                        let prepared = join_produced(inner)
                                            .await
                                            .unwrap_or_default();
                                        let _ = command_tx.send(Command::McpPrepared {
                                            uuid: prompt_uuid,
                                            prepared,
                                        });
                                    });
                                }
                            }
                            Command::PromptV2 {
                                uuid,
                                frame,
                                is_local_only,
                                prompt_text,
                                content,
                                inserted,
                                ended,
                            } => {
                                // Same first-prompt bookkeeping as v1 (P-06).
                                if ext.config.pending_auto_fallback_warning {
                                    ext.config.pending_auto_fallback_warning = false;
                                    if !ext.auto_mode_fallback_warning_shown {
                                        ext.auto_mode_fallback_warning_shown = true;
                                        let _ = update_tx.send(SessionOutbound::RawUpdate(
                                            auto_mode_fallback_update(
                                                &session_id,
                                                ext.supports_notices(),
                                            ),
                                        ));
                                    }
                                }
                                // T-09: v2 prompts share the v1 `prompt()` path
                                // upstream (`startTurn`), so the task snapshot
                                // goes out here too.
                                publish_prompt_start_task_plan(
                                    &session_id,
                                    &mut ext,
                                    &update_tx,
                                );
                                ext.titles.on_prompt(&prompt_text);
                                // AG-3 (review-T6INT finding 3): a v2 prompt may
                                // ask for the same per-turn file-change report as
                                // v1 (`acp-agent.ts:3881`, one shared prompt path).
                                let prompt_meta = prompt_metas.remove(&uuid);
                                ext.file_change_reports.note_prompt(
                                    &uuid,
                                    prompt_meta.as_ref().unwrap_or(&Value::Null),
                                );
                                prompt_preamble(
                                    &uuid,
                                    &frame,
                                    &mut ext,
                                    &update_tx,
                                    &session_id,
                                );
                                let first_text = crate::goal::first_prompt_text(&frame).to_string();
                                let kept_frame = frame.clone();
                                let _ = control.send_user(frame).await;
                                machine.enqueue(
                                    Turn::new(uuid.clone(), is_local_only).with_frame(kept_frame),
                                );
                                // F39: the client is answered at intake; the
                                // session emits the user_message and the
                                // state updates on its ordered channel.
                                map_state.v2.register(uuid.clone(), content, inserted);
                                // NEW-3: the turn's end goes to the task that
                                // runs the v1 bookkeeping for this prompt.
                                map_state.v2.set_turn_end(&uuid, ended);
                                if let Some(goal) = ext.goal.note_prompt(&first_text, &uuid) {
                                    publish_goal(ext.air.is_some(), &goal, &update_tx, &session_id);
                                }
                            }
                            Command::McpPrompt { uuid, command } => {
                                ext.mcp.insert(&uuid, command);
                            }
                            Command::McpPrepared { uuid, prepared } => {
                                let reconnected = prepared.reconnected.clone();
                                let mut live_turn = false;
                                if let Some(turn) = ext.mcp.get_mut(&uuid) {
                                    turn.prepared = Some(prepared);
                                    live_turn = true;
                                }
                                // The replacement is ready: the result (and the session
                                // frames queued behind it) that waited for it go on, in
                                // stream order.
                                release_held_frames(&mut replay, &mut held_behind_mcp);
                                // S-02/A-01: a reconnect starts OAuth for the ACP
                                // servers it tried, as session start does. A cancelled
                                // turn (its `/mcp` state cleared) starts none.
                                if live_turn {
                                    let follow = crate::mcp_command::filter_reconnected_for_oauth(
                                        &reconnected,
                                        &acp_mcp_servers,
                                    );
                                    if !follow.is_empty() {
                                        let _ = command_tx.send(Command::StartMcpAuth {
                                            servers: follow,
                                            configured: false,
                                        });
                                    }
                                }
                            }
                            Command::StartUsage { uuid } => {
                                // The producer runs off-actor (INV-27): the control
                                // request carries its own 5 s timeout, and its answer
                                // re-enters as [`Command::UsagePrepared`]. Every stage
                                // is bounded and a panic answers "keep the CLI text":
                                // `UsagePrepared` is what releases the held frames, so
                                // it must always arrive.
                                let control = control.clone();
                                let command_tx = command_tx.clone();
                                tokio::spawn(async move {
                                    let inner = tokio::spawn(async move {
                                        let zone = crate::usage::system_zone_bounded().await?;
                                        crate::usage::structured_usage_markdown(&control, &zone)
                                            .await
                                    });
                                    let markdown = join_produced(inner).await.unwrap_or(None);
                                    let _ = command_tx.send(Command::UsagePrepared { uuid, markdown });
                                });
                            }
                            Command::UsagePrepared { uuid, markdown } => {
                                if let Some(turn) = ext.t2a.usage_turns.get_mut(&uuid) {
                                    if turn.aborted {
                                        turn.prepared.get_or_insert(None);
                                    } else {
                                        turn.prepared = Some(markdown);
                                    }
                                }
                                release_held_frames(&mut replay, &mut held_behind_mcp);
                            }
                            Command::Cancel => {
                                // A cancel during a clear-context restart abandons it
                                // (`ExitPlanCoordinator.cancel`): the fresh child goes, the
                                // pending turn ends `cancelled` with the usage it had
                                // earned, and the session is left without a query, as a
                                // closed one (later prompts fail with "session ended").
                                if let Some(abandoned) = restart.take() {
                                    settle_restart_turn!(abandoned.plan, cancelled);
                                    let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
                                    stream = closed_rx;
                                }
                                // Abort every pending permission round-trip (CUJ 2):
                                // dropping the `reply` oneshot makes control.rs's
                                // on-request handler resolve with the R28 deny payload,
                                // so the pending permission settles `Cancelled` (INV-21 /
                                // INV-27) while the actor stays free. The RFD form keeps
                                // the requests of a child that can keep running after a
                                // parent cancel (`agentclientprotocol/agent-client-protocol#2308`,
                                // `native-subagents.ts:570-580`).
                                // F39/B-60: the interrupt withdraws the requests open
                                // now, so their end reports no `running`
                                // (`cancelTurns` marks them first,
                                // `acp-agent.ts:8446-8449`).
                                map_state.v2.cancel_inputs();
                                cancel_parent_round_trips(
                                    &mut tasks,
                                    &mut map_state,
                                    &mut PendingClientRequests {
                                        permissions: &mut pending_permissions,
                                        elicitations: &mut pending_elicitations,
                                        ask: &mut pending_ask,
                                        await_tokens: &mut pending_await_tokens,
                                    },
                                    &update_tx,
                                    &session_id,
                                );
                                // A request still waiting for its tool use is withdrawn by
                                // the interrupt: dropping `reply` makes control.rs answer
                                // with the R28 deny.
                                parked_permissions.clear();
                                // `cancelTurns` forgets the pending ExitPlanMode lanes.
                                ext.t2a.exit_plan.clear();
                                // A file-change report still previewing is
                                // finished `cancelled` (`review-T6INT` finding 2).
                                ext.file_change_reports.cancel_collecting();
                                // G12: a cancelled `/mcp` turn publishes nothing, and its
                                // spawned control run's late result is dropped. What waited
                                // for that run goes on (the cancelled result is dropped).
                                ext.mcp.clear();
                                // S-01: a cancelled `/usage` producer is aborted
                                // (`localCommandAbort.abort()`), so its output is not
                                // published.
                                ext.t2a.usage_turns.abort_all();
                                // `compaction.interrupt()` at the turn boundary
                                // (`acp-agent.ts:8468-8474`): an in-progress
                                // `compaction_update` entity is closed `cancelled`,
                                // and the abandoned work's late frames and hooks
                                // are ignored until a new turn begins.
                                let updates = ext.t2a.compaction.interrupt();
                                publish_compaction_updates(
                                    &mut ext,
                                    &mut map_state,
                                    updates,
                                    &update_tx,
                                    &session_id,
                                );
                                release_held_frames(&mut replay, &mut held_behind_mcp);
                                // 8.9: settle swept queued turns, then interrupt.
                                let events = machine.cancel();
                                // Settling a held turn inline closes its delivery stretch:
                                // any text streamed since the last boundary was its
                                // follow-ups', and left latched it would suppress a
                                // following replayed turn's #453 fallback.
                                if events.iter().any(|e| {
                                    matches!(e, TurnEvent::Settled { had_usage: true, .. })
                                }) {
                                    emitted_assistant_text = false;
                                }
                                // Decision 18: a late consolidated assistant message
                                // must not emit its remainder while the stopped-work
                                // window is open (Node `session.cancelled`).
                                map_state.begin_stopped_work();
                            settle_events(
                                events,
                                &mut tasks,
                                &mut map_state,
                                &live_background,
                                &mut pending,
                                &update_tx,
                                &session_id,
                                &high_water,
                                &ext.failure_ctx,
                                ext.air.as_ref(),
                                None,
                                &mut ext.titles,
        &mut ext.t2a.failures,
        &mut ext.t2a.usage_turns,
                            &mut ext.file_change_reports,
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
                                // 2.13 (E-8): one `cancel_async_message{message_uuid}`
                                // per abandoned message — pending steered echoes then
                                // swept queued turns, in `take_abandoned()`'s order —
                                // BEFORE the `interrupt` (Node, `dist/acp-agent.js:6455-6457`).
                                // Sent detached (synchronous enqueue on the single
                                // unbounded writer; call order preserved; INV-27).
                                for uuid in machine.take_abandoned() {
                                    if control
                                        .send_request_detached(json!({
                                            "subtype": "cancel_async_message",
                                            "message_uuid": uuid,
                                        }))
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                // Send the `interrupt` on a spawned task: awaiting the
                                // correlated control_response here would block the actor's
                                // single-task loop (which is what routes that response back
                                // to the Control task), deadlocking it. The task reports
                                // the receipt's `still_queued` back so the actor can
                                // reconcile the orphan-credit count (11.3, INV-23).
                                // The interrupt is written NOW (in call order after the
                                // `cancel_async_message` frames); only its receipt is
                                // awaited off the actor.
                                let command_tx = command_tx.clone();
                                let cancel_id = machine.last_cancel_id();
                                if let Ok(receipt_rx) =
                                    control.enqueue_request(json!({"subtype": "interrupt"}))
                                {
                                    tokio::spawn(async move {
                                        if let Ok(Ok(receipt)) = receipt_rx.await {
                                            let still_queued =
                                                crate::control::still_queued_of(&receipt);
                                            let _ = command_tx.send(Command::InterruptReceipt {
                                                cancel_id,
                                                still_queued,
                                            });
                                        }
                                    });
                                }
                                // After the interrupt, stop each background workflow it
                                // spared (`stopBackgroundWorkflows`, B-42): Claude Code
                                // reports each one `stopped` and starts no model cycle for
                                // it, as when the interrupt killed it. Detached: a failure
                                // only costs the stop.
                                for task_id in machine.background_workflows_to_stop() {
                                    let _ = control.send_request_detached(json!({
                                        "subtype": "stop_task",
                                        "task_id": task_id,
                                    }));
                                }
                            }
                            Command::Close { reply } => {
                                // A close during a clear-context restart abandons it, and
                                // the fresh child with it.
                                if let Some(abandoned) = restart.take() {
                                    settle_restart_turn!(abandoned.plan, cancelled);
                                }
                                // Cancel exactly as `session/cancel` does (pending
                                // permissions refuse, swept turns settle, abandoned
                                // messages are cancelled), then interrupt.
                                abort_pending_permissions(
                                    &mut pending_permissions,
                                    &mut parked_permissions,
                                );
                                // AG-3 (`acp-agent.ts:3911`): a close ends every open
                                // report request as `cancelled`.
                                ext.file_change_reports.finish_all("cancelled");
                                // Close is the session's abort (`signal.aborted`):
                                // the close-only latch stops an OAuth flow.
                                let _ = elicit_cancel_tx.send(true);
                                // B-113: a close withdraws the open requests like
                                // a cancel (`cancelTurns` marks them first,
                                // `acp-agent.ts:8446-8449`), so the work reports
                                // no `running` before its idle cancelled.
                                map_state.v2.cancel_inputs();
                                cancel_parent_round_trips(
                                    &mut tasks,
                                    &mut map_state,
                                    &mut PendingClientRequests {
                                        permissions: &mut pending_permissions,
                                        elicitations: &mut pending_elicitations,
                                        ask: &mut pending_ask,
                                        await_tokens: &mut pending_await_tokens,
                                    },
                                    &update_tx,
                                    &session_id,
                                );
                                // A prompt held behind a `/remote-control` command
                                // ends `cancelled`, as the close ends a queued turn
                                // (`acp-agent.ts:3906-3912`).
                                cancel_held_remote_control_prompts(&mut held_remote_control);
                                // `cancelTurns` forgets the pending ExitPlanMode lanes.
                                ext.t2a.exit_plan.clear();
                                ext.mcp.clear();
                                release_held_frames(&mut replay, &mut held_behind_mcp);
                                let events = machine.cancel();
                                settle_events(
                                    events,
                                    &mut tasks,
                                    &mut map_state,
                                    &live_background,
                                    &mut pending,
                                    &update_tx,
                                    &session_id,
                                    &high_water,
                                    &ext.failure_ctx,
                                    ext.air.as_ref(),
                                    None,
                                    &mut ext.titles,
        &mut ext.t2a.failures,
        &mut ext.t2a.usage_turns,
                                &mut ext.file_change_reports,
                                );
                                for uuid in machine.take_abandoned() {
                                    if control
                                        .send_request_detached(json!({
                                            "subtype": "cancel_async_message",
                                            "message_uuid": uuid,
                                        }))
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                // Teardown writes the `interrupt` and does NOT wait for its
                                // reply (`cancelTurns({ awaitInterrupt: false })`, the SDK
                                // closes the query right after: the reply would only queue
                                // behind a slow control request, `acp-agent.ts:8466-8470`).
                                // It does wait (bounded, off the actor — INV-27) until the
                                // frame is on the child's stdin, so the child is not killed
                                // before it.
                                let _ = control.enqueue_request(json!({"subtype": "interrupt"}));
                                let control = control.clone();
                                let command_tx = command_tx.clone();
                                tokio::spawn(async move {
                                    let _ = tokio::time::timeout(
                                        Duration::from_millis(500),
                                        control.flushed(),
                                    )
                                    .await;
                                    let _ = command_tx.send(Command::CloseDone { reply });
                                });
                            }
                            Command::ForegroundState { reply } => {
                                let _ = reply.send(map_state.v2.foreground_state());
                            }
                            Command::CloseDone { reply } => {
                                let events = machine.on_stream_end();
                                // F39: a cycle the stream death ended reports its
                                // end with this batch.
                                if let Some(end) = machine.take_cycle_end() {
                                    map_state.v2.set_cycle_end(end);
                                }
                                settle_events(
                                    events,
                                    &mut tasks,
                                    &mut map_state,
                                    &live_background,
                                    &mut pending,
                                    &update_tx,
                                    &session_id,
                                    &high_water,
                                    &ext.failure_ctx,
                                    ext.air.as_ref(),
                                    None,
                                    &mut ext.titles,
        &mut ext.t2a.failures,
        &mut ext.t2a.usage_turns,
                                &mut ext.file_change_reports,
                                );
                                finish_async_tasks(&mut tasks, &mut map_state, "stopped", &update_tx, &session_id, ext.logger.as_deref());
                                let (flush_tx, flush_rx) = oneshot::channel();
                                let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                                tokio::spawn(async move {
                                    let _ = flush_rx.await;
                                    let _ = reply.send(());
                                });
                                break;
                            }
                            Command::InterruptReceipt {
                                cancel_id,
                                still_queued,
                            } => {
                                // 11.3 / 11.4: reconcile the orphan count against the
                                // receipt, for the cancel that sent the interrupt. A bare
                                // `{}` receipt (field absent) is handled inside the
                                // machine as count-everything.
                                machine.reconcile_cancel_receipt(cancel_id, still_queued.as_deref());
                            }
                            Command::Permission { frame, reply } => {
                                // The client round-trip (10.1, D14) runs in a spawned task
                                // so the actor never awaits it inline (INV-27): a
                                // `session/cancel` notification stays processable while a
                                // permission is pending.
                                // Prune finished round-trips first so the registry stays a
                                // live-set of pending permissions (abortable on cancel).
                                pending_permissions.retain(|_, h| !h.is_finished());
                                let available_modes: Vec<String> = ext
                                    .config
                                    .modes
                                    .available_modes
                                    .iter()
                                    .map(|m| m.id.clone())
                                    .collect();
                                let pre_plan_mode = ext.config.pre_plan_mode.as_deref();
                                // The Auto-unavailable decision is made when the client
                                // ANSWERS (`Command::PermissionAnswered`), not here: the
                                // model can change while the dialog is open
                                // (`acp-agent.ts:10219`).
                                let home_dir = ext.env.get_path("HOME");
                                // F39/B-60: an open request that blocks the
                                // foreground work makes it `requires_action`
                                // (`withPendingUserInput`, `acp-agent.ts:9588-9606`).
                                // NEW-6: `handle_permission` opens and drains it
                                // before it spawns the round-trip, so the state
                                // precedes the request.
                                handle_permission(
                                    &mut tasks,
                                    frame,
                                    reply,
                                    machine.foreground_target(),
                                    &mut map_state,
                                    &mut live_background,
                                    &update_tx,
                                    &session_id,
                                    &mut pending_permissions,
                                    &mut pending_await_tokens,
                                    &mut pending_ask,
                                    &elicit_cancel_rx,
                                    ext.caps.elicitation_form,
                                    &ext.cwd,
                                    ext.logger.as_deref(),
                                    &available_modes,
                                    pre_plan_mode,
                                    home_dir.as_deref(),
                                    ext.usage.used_percent(),
                                    &command_tx,
                                );
                            }
                            Command::ElicitationControl { frame, reply } => {
                                // F23b/R-10: forward an MCP elicitation to the
                                // client from a spawned task; the answer re-enters
                                // as `ElicitationControlAnswered`. A cancel or the
                                // session close answers the pending request with
                                // the CANCEL outcome itself, so it never settles as
                                // a decline.
                                let support = crate::elicitation::ElicitationSupport {
                                    form: ext.caps.elicitation_form,
                                    url: ext.caps.elicitation_url,
                                };
                                let request_id = frame
                                    .get("request_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                let cancel_response = json!({
                                    "type": "control_response",
                                    "response": {
                                        "subtype": "success",
                                        "request_id": request_id,
                                        "response": { "action": "cancel" },
                                    },
                                });
                                // B-60: the elicitation blocks the work while
                                // its form is open (`withPendingUserInput`).
                                map_state.v2.open_input(request_id.clone(), None);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                                let update_tx = update_tx.clone();
                                let command_tx = command_tx.clone();
                                let session_id = session_id.clone();
                                let cancel = elicit_cancel_rx.clone();
                                let request_id_for_task = request_id.clone();
                                let handle = tokio::spawn(async move {
                                    let send_tx = update_tx.clone();
                                    let response =
                                        crate::elicitation::handle_elicitation_control_request(
                                            &frame,
                                            &session_id,
                                            support,
                                            move |params| {
                                                let (ptx, prx) = oneshot::channel();
                                                let _ = send_tx.send(
                                                    SessionOutbound::CreateElicitation {
                                                        params,
                                                        reply: ptx,
                                                    },
                                                );
                                                async move {
                                                    prx.await.unwrap_or_else(|_| {
                                                        Err("Tool use aborted".to_string())
                                                    })
                                                }
                                            },
                                            cancel,
                                        )
                                        .await;
                                    let _ = command_tx.send(
                                        Command::ElicitationControlAnswered {
                                            request_id: request_id_for_task,
                                            response,
                                        },
                                    );
                                });
                                pending_elicitations
                                    .insert(request_id, (reply, cancel_response, handle));
                            }
                            Command::ElicitationControlAnswered {
                                request_id,
                                response,
                            } => {
                                if let Some((reply, _, _)) =
                                    pending_elicitations.remove(&request_id)
                                {
                                    let _ = reply.send(response);
                                }
                                map_state.v2.close_input(&request_id, false);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                            }
                            Command::UserDialogControl { frame, reply } => {
                                // F23b/R-08: the refusal-fallback consent dialog,
                                // rendered as a form elicitation off the actor.
                                let support = crate::elicitation::ElicitationSupport {
                                    form: ext.caps.elicitation_form,
                                    url: ext.caps.elicitation_url,
                                };
                                let request_id = frame
                                    .get("request_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                let cancel_response = json!({
                                    "type": "control_response",
                                    "response": {
                                        "subtype": "success",
                                        "request_id": request_id,
                                        "response": { "behavior": "cancelled" },
                                    },
                                });
                                // B-60: the dialog blocks the work while its
                                // form is open (`withPendingUserInput`).
                                map_state.v2.open_input(request_id.clone(), None);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                                let update_tx = update_tx.clone();
                                let command_tx = command_tx.clone();
                                let session_id = session_id.clone();
                                let cancel = elicit_cancel_rx.clone();
                                let request_id_for_task = request_id.clone();
                                let handle = tokio::spawn(async move {
                                    let send_tx = update_tx.clone();
                                    let response =
                                        crate::elicitation::handle_user_dialog_control_request(
                                            &frame,
                                            &session_id,
                                            support,
                                            move |params| {
                                                let (ptx, prx) = oneshot::channel();
                                                let _ = send_tx.send(
                                                    SessionOutbound::CreateElicitation {
                                                        params,
                                                        reply: ptx,
                                                    },
                                                );
                                                async move {
                                                    prx.await.unwrap_or_else(|_| {
                                                        Err("Tool use aborted".to_string())
                                                    })
                                                }
                                            },
                                            cancel,
                                        )
                                        .await;
                                    let _ = command_tx.send(Command::UserDialogAnswered {
                                        request_id: request_id_for_task,
                                        response,
                                    });
                                });
                                pending_elicitations
                                    .insert(request_id, (reply, cancel_response, handle));
                            }
                            Command::UserDialogAnswered {
                                request_id,
                                response,
                            } => {
                                if let Some((reply, _, _)) =
                                    pending_elicitations.remove(&request_id)
                                {
                                    let _ = reply.send(response);
                                }
                                map_state.v2.close_input(&request_id, false);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                            }
                            Command::AskUserQuestionAnswered {
                                request_id,
                                response,
                            } => {
                                // The question was presented: the child's request
                                // ended (`awaitingUser`'s finally).
                                if let Some(token) = pending_await_tokens.remove(&request_id) {
                                    let outs = tasks.subagents().awaiting_user_end(token, false);
                                    publish_subagent_outs(
                                        &mut tasks,
                                        &mut map_state,
                                        outs,
                                        &update_tx,
                                        &session_id,
                                        None,
                                    );
                                }
                                if let Some((reply, _)) = pending_ask.remove(&request_id) {
                                    let _ = reply.send(response);
                                }
                                // F39/B-60: the question was answered, so the
                                // work reports `running` again.
                                map_state.v2.close_input(&request_id, false);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                            }
                            Command::ElicitationWithdrawn { request_id } => {
                                // The CLI withdrew the request (`control_cancel_request`,
                                // or the control channel moved to a fresh child on a
                                // clear-context restart): the client's form is cancelled
                                // (`$/cancel_request`), and the child's request ends as
                                // withdrawn (`awaitingUser`'s aborted finally).
                                //
                                // A request parked in the cancel window is not in
                                // `pending_permissions`: drop it here so its bounded
                                // release never presents a dialog nobody waits for
                                // (`requestPermissionFromClient` throws before sending
                                // anything, `acp-agent.ts:9800-9815`).
                                parked_permissions.retain(|(_, frame, _)| {
                                    frame.get("request_id").and_then(Value::as_str)
                                        != Some(request_id.as_str())
                                });
                                withdraw_request(
                                    &mut tasks,
                                    &mut map_state,
                                    &mut PendingClientRequests {
                                        permissions: &mut pending_permissions,
                                        elicitations: &mut pending_elicitations,
                                        ask: &mut pending_ask,
                                        await_tokens: &mut pending_await_tokens,
                                    },
                                    &update_tx,
                                    &session_id,
                                    &request_id,
                                );
                                // F39/B-60: Claude Code withdrew the request
                                // (its signal aborted), which is not an answer:
                                // the work reports no `running` when a cancel
                                // marked it (`withdrawnByCancel`).
                                map_state.v2.close_input(&request_id, true);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                            }
                            Command::AwaitEnded { request_id } => {
                                // The permission round trip ended without an answer
                                // (the client errored or the request dropped): the
                                // child's request ends, un-aborted (`awaitingUser`'s
                                // finally), so it reports `running` again.
                                end_await_request(
                                    &mut tasks,
                                    &mut map_state,
                                    &mut pending_await_tokens,
                                    &update_tx,
                                    &session_id,
                                    &request_id,
                                );
                                // F39/B-60: a request that ended without an
                                // answer was not withdrawn by a cancel, so the
                                // work goes on (`running`).
                                map_state.v2.close_input(&request_id, false);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                            }
                            Command::StartMcpAuth {
                                servers,
                                configured,
                            } => {
                                // F23c/A-01: start OAuth for the ACP servers this
                                // session was created with. Runs off the actor so an
                                // interactive browser flow never delays a response;
                                // one run per session (upstream `mcpAuthentications`).
                                if configured {
                                    acp_mcp_servers = servers.clone();
                                }
                                if !ext.caps.elicitation_url || servers.is_empty() {
                                    continue;
                                }
                                if mcp_auth_running {
                                    continue;
                                }
                                mcp_auth_running = true;
                                let control = control.clone();
                                let command_tx = command_tx.clone();
                                let session_id = session_id.clone();
                                let cancel = elicit_cancel_rx.clone();
                                let logger = ext.logger.clone();
                                let update_tx = update_tx.clone();
                                tokio::spawn(async move {
                                    let statuses = tokio::time::timeout(
                                        MCP_CONTROL_TIMEOUT,
                                        control.mcp_status(),
                                    )
                                    .await
                                    .ok()
                                    .and_then(Result::ok);
                                    if let Some(statuses) = statuses {
                                        let list = statuses
                                            .pointer("/response/response/mcpServers")
                                            .and_then(Value::as_array)
                                            .cloned()
                                            .unwrap_or_default();
                                        for status in list {
                                            let name = status
                                                .get("name")
                                                .and_then(Value::as_str)
                                                .unwrap_or("");
                                            if !crate::elicitation::server_needs_auth(&status)
                                                || !servers.iter().any(|s| s == name)
                                            {
                                                continue;
                                            }
                                            let result = authenticate_mcp_server(
                                                &control,
                                                &session_id,
                                                name,
                                                &update_tx,
                                                cancel.clone(),
                                            )
                                            .await;
                                            if let Err(error) = result {
                                                if let Some(logger) = &logger {
                                                    logger.error(&format!(
                                                        "Failed to authenticate MCP server {name}: {error}"
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    let _ = command_tx.send(Command::McpAuthFinished);
                                });
                            }
                            Command::McpAuthFinished => {
                                mcp_auth_running = false;
                            }
                            Command::MessageIds { reply } => {
                                let _ = reply.send(map_state.message_ids());
                            }
                            Command::SeedMessageIds { map } => {
                                map_state.seed_message_ids(map);
                            }
                            Command::RemoteControlPrompt {
                                command,
                                reply,
                                accepted,
                            } => {
                                // F24/S-03: answer the prompt without a model turn.
                                // The control call runs off the actor (the reply
                                // bypasses the turn queue, as upstream's
                                // `runRemoteControlCommand`).
                                //
                                // The busy refusal is made HERE, atomically with
                                // the turn queue (`acp-agent.ts:3824-3832`).
                                if let Some(error) =
                                    remote_control_busy_error(&machine, restart.is_some())
                                {
                                    if let Some(accepted) = accepted {
                                        let _ = accepted.send(Err(error.clone()));
                                    }
                                    let _ = reply.send(Err(error.into()));
                                    continue;
                                }
                                if remote_control_pending {
                                    let error = SessionError::InvalidRequest(
                                        "Remote Control is already being changed. Try again in a moment."
                                            .to_string(),
                                    );
                                    if let Some(accepted) = accepted {
                                        let _ = accepted.send(Err(error.clone()));
                                    }
                                    let _ = reply.send(Err(error.into()));
                                    continue;
                                }
                                // NEW-2: the acceptance is reported before the
                                // control call is spawned, so the adapter's
                                // answer follows it (`acp-agent.ts:3836-3858`).
                                if let Some(accepted) = accepted {
                                    let _ = accepted.send(Ok(()));
                                }
                                remote_control_pending = true;
                                let control = control.clone();
                                let command_tx = command_tx.clone();
                                tokio::spawn(async move {
                                    let action = command.action.clone();
                                    let outcome = match &command.action {
                                        crate::control::RemoteControlAction::Disconnect => {
                                            control.enable_remote_control(false, None).await
                                        }
                                        crate::control::RemoteControlAction::Connect { name } => {
                                            control
                                                .enable_remote_control(true, name.as_deref())
                                                .await
                                        }
                                    };
                                    let text = match &outcome {
                                        Ok(value) => crate::control::format_remote_control_result(
                                            &action,
                                            Ok(value),
                                        ),
                                        Err(error) => {
                                            let message = error.to_string();
                                            crate::control::format_remote_control_result(
                                                &action,
                                                Err(&message),
                                            )
                                        }
                                    };
                                    let _ = command_tx.send(Command::RemoteControlDone { text, reply });
                                });
                            }
                            Command::RemoteControlDone { text, reply } => {
                                remote_control_pending = false;
                                // The prompts held behind the command go on, in
                                // arrival order.
                                replay_commands.extend(held_remote_control.drain(..));
                                let chunk =
                                    ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                                let update = SessionUpdate::AgentMessageChunk(chunk);
                                let notif = SessionNotification::new(
                                    session_id.clone(),
                                    update,
                                );
                                let _ = reply.send(Ok(PromptReply {
                                    stop_reason: "end_turn".to_string(),
                                    usage: None,
                                    quota: None,
                                    updates: vec![notif],
                                    flush_rx: None,
                                    ack: None,
                                }));
                            }
                            Command::RemoteControlAvailability { reply } => {
                                let offered = ext.commands.iter().any(|command| {
                                    command.get("name").and_then(Value::as_str)
                                        == Some("remote-control")
                                        && !ext
                                            .terminal_slash_commands
                                            .as_ref()
                                            .is_some_and(|names| {
                                                names.iter().any(|n| n == "remote-control")
                                            })
                                });
                                let _ = reply.send(!offered);
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
                                // G8: a steer that carries a `/goal …` command
                                // publishes its optimistic update after the push
                                // (`acp-agent.ts:4387`), on both the injected and the
                                // detached-new-turn path (the latter goes through
                                // `prompt()`).
                                let first_text = crate::goal::first_prompt_text(&frame).to_string();
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
                                        // 3.12 (R71-10/R86-6): while a permission is
                                        // awaiting the user, `later` — interrupting that
                                        // callback would cancel the ACP request
                                        // (`openUserInputs`, `dist/acp-agent.js:2646`).
                                        pending_permissions.retain(|_, h| !h.is_finished());
                                        injected["priority"] = if pending_permissions.is_empty() {
                                            json!("now")
                                        } else {
                                            json!("later")
                                        };
                                        // R2: mark the running turn steered so the
                                        // injected echo feeds `machine.on_echo`'s steer
                                        // lane instead of settling the turn outright.
                                        // A turn held for its background subagents
                                        // resumes its work on the steer
                                        // (`startForeground`, `acp-agent.ts:4380-4386`).
                                        machine.mark_steered(&uuid);
                                        if let Some(resumed) = machine.take_steer_resumed() {
                                            map_state.v2.note_resumed(&resumed);
                                            drain_v2(&mut map_state, &update_tx, &session_id);
                                        }
                                        let _ = control.send_user(injected).await;
                                        if let Some(goal) = ext.goal.note_prompt(&first_text, &uuid) {
                                            publish_goal(ext.air.is_some(), &goal, &update_tx, &session_id);
                                        }
                                    }
                                    Some("startedNewTurn") => {
                                        // Idle, no opt-in: start a detached turn like a
                                        // normal prompt (it streams updates and settles
                                        // internally; nothing awaits its oneshot). It
                                        // goes through `prompt()` upstream, so it runs
                                        // the same preamble: the stale prediction is
                                        // cleared and a `/usage` prompt registers its
                                        // producer (`acp-agent.ts:4353`).
                                        prompt_preamble(
                                            &uuid,
                                            &frame,
                                            &mut ext,
                                            &update_tx,
                                            &session_id,
                                        );
                                        let (ptx, _prx) = oneshot::channel();
                                        let _ = control.send_user(frame).await;
                                        machine.enqueue(Turn::new(uuid.clone(), is_local_only));
                                        pending.insert(uuid.clone(), ptx);
                                        if let Some(goal) = ext.goal.note_prompt(&first_text, &uuid) {
                                            publish_goal(ext.air.is_some(), &goal, &update_tx, &session_id);
                                        }
                                    }
                                    _ => {}
                                }
                                let _ = reply.send(outcome);
                            }
                            Command::UnsettledTurn { reply } => {
                                let _ = reply.send(machine.has_unsettled() || restart.is_some());
                            }
                            Command::StopAsyncTask { task_id, reply } => {
                                if !tasks.claim_stop(&task_id) {
                                    let _ = reply.send(Ok(false));
                                } else {
                                    // The CLI round-trip runs on a spawned task (INV-27);
                                    // it re-enters the actor with the outcome.
                                    let control = control.clone();
                                    let command_tx = command_tx.clone();
                                    tokio::spawn(async move {
                                        let outcome = control
                                            .send_request(json!({
                                                "subtype": "stop_task",
                                                "task_id": task_id,
                                            }))
                                            .await;
                                        let failure = match outcome {
                                            Ok(receipt) => control_response_error(&receipt),
                                            Err(e) => Some(e.to_string()),
                                        };
                                        let command = match failure {
                                            None => Command::TaskStopped { task_id, reply },
                                            Some(error) => Command::ReleaseStop { task_id, error, reply },
                                        };
                                        // A gone actor drops `reply`: the caller sees Closed.
                                        let _ = command_tx.send(command);
                                    });
                                }
                            }
                            Command::CancelChild {
                                session_id: child_session,
                                reply,
                            } => {
                                // `NativeSubagentRuntime.cancelChild`: accepting the
                                // stop is no evidence that the child stopped, so
                                // nothing is reported here; the SDK's task events
                                // report the child `idle cancelled`
                                // (`native-subagents.ts:594-603`).
                                match tasks.subagents().cancel_child(&child_session) {
                                    Some(task_id) => {
                                        let control = control.clone();
                                        let logger = ext.logger.clone();
                                        let root = session_id.clone();
                                        let child = child_session.clone();
                                        tokio::spawn(async move {
                                            let outcome = control
                                                .send_request(json!({
                                                    "subtype": "stop_task",
                                                    "task_id": task_id,
                                                }))
                                                .await;
                                            let failure = match outcome {
                                                Ok(receipt) => control_response_error(&receipt),
                                                Err(error) => Some(error.to_string()),
                                            };
                                            if let (Some(error), Some(logger)) =
                                                (failure, logger)
                                            {
                                                logger.error(&format!(
                                                    "Session {root}: failed to stop the task {task_id} of subagent session {child}: {error}"
                                                ));
                                            }
                                        });
                                        let _ = reply.send(true);
                                    }
                                    None => {
                                        let _ = reply.send(false);
                                    }
                                }
                            }
                            Command::TaskStopped { task_id, reply } => {
                                let stopped = tasks.task_stopped(&task_id);
                                emit_task_updates(
                                    &mut tasks,
                                    &map_state,
                                    stopped.updates,
                                    &update_tx,
                                    &session_id,
                                );
                                if let Some(name) = stopped.notice_name {
                                    // The only acknowledgement the user gets: the SDK
                                    // injects nothing for a stopped task. A client on
                                    // the notice contract gets a live `notice`; every
                                    // other client gets the bold-label transcript line.
                                    let notice = crate::notices::SessionNotice::new(
                                        "info",
                                        "Task stopped by user",
                                        Some(format!("{name}.")),
                                    );
                                    let update = crate::notices::notice_or_transcript_update(
                                        &notice,
                                        ext.supports_notices(),
                                        None,
                                        None,
                                    );
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                        "sessionId": session_id,
                                        "update": update,
                                    })));
                                }
                                // The response must not overtake its updates: resolve it
                                // behind a Flush barrier (INV-20).
                                let (flush_tx, flush_rx) = oneshot::channel();
                                let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                                tokio::spawn(async move {
                                    let _ = flush_rx.await;
                                    let _ = reply.send(Ok(true));
                                });
                            }
                            Command::ReleaseStop { task_id, error, reply } => {
                                tasks.release_stop(&task_id);
                                let _ = reply.send(Err(error));
                            }
                            Command::InitCommands { commands } => {
                                ext.commands = commands;
                            }
                            Command::CommandsSnapshot { reply } => {
                                let _ = reply.send(ext.commands.clone());
                            }
                            Command::UpdateContextWindow { size, model_id } => {
                                // MAJOR 1 / P4 (acp-agent.js:7884-7891): the background
                                // get_context_usage answer only refines a NON-authoritative
                                // window. Once the window is authoritative (seeded from the
                                // model row, learned from `result.modelUsage`/cache), the
                                // background result is dropped. F5b/A-10: it is also
                                // dropped when the session switched models while the
                                // request was in flight (`stillCurrent`).
                                apply_context_window_answer(
                                    &mut ext.usage,
                                    &ext.config.current_model_id,
                                    size,
                                    &model_id,
                                );
                            }
                            Command::UpdateSendFailed => {
                                tasks.rollback();
                            }
                            Command::TitleInfo {
                                transcript,
                                info,
                                cancelled,
                                renames,
                            } => {
                                if transcript.is_some() {
                                    ext.titles.transcript = transcript;
                                }
                                apply_title_info(
                                    &mut ext,
                                    &control,
                                    &command_tx,
                                    &session_id,
                                    &update_tx,
                                    TitleRead {
                                        stored: info,
                                        cancelled,
                                        renames,
                                    },
                                );
                            }
                            Command::SetExplicitTitle {
                                publish,
                                persist,
                                reply,
                            } => {
                                ext.titles.begin_explicit_change();
                                let change = state::ParkedExplicit {
                                    publish,
                                    persist,
                                    reply,
                                };
                                // A generation in flight persists its title first.
                                if ext.titles.explicit.generation_in_flight() {
                                    ext.t2a.parked_explicit.push(change);
                                } else {
                                    start_explicit_persist(change, &command_tx);
                                }
                            }
                            Command::ExplicitPersisted {
                                outcome,
                                publish,
                                reply,
                            } => {
                                let publishes = !matches!(publish, crate::session_titles::ExplicitPublish::None);
                                let stored = outcome.as_ref().ok().map(|stored| stored.as_deref());
                                ext.titles.end_explicit_change(stored, publishes);
                                let result = outcome.map(|stored| {
                                    let title = match publish {
                                        crate::session_titles::ExplicitPublish::None => None,
                                        crate::session_titles::ExplicitPublish::Title(title) => Some(title),
                                        crate::session_titles::ExplicitPublish::FromStored(make) => {
                                            Some(make(stored.as_deref()))
                                        }
                                    };
                                    if let Some(title) = title {
                                        let update = ext.titles.publish_explicit(title);
                                        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                            "sessionId": session_id,
                                            "update": update,
                                        })));
                                    }
                                });
                                let _ = reply.send(result);
                            }
                            Command::TitleSnapshot { reply } => {
                                let _ = reply.send(TitleSnapshot {
                                    shown: ext.titles.shown_title().map(str::to_string),
                                    stored: ext.titles.stored_title().map(str::to_string),
                                });
                            }
                            Command::FlushUpdates { reply } => {
                                let _ = update_tx.send(SessionOutbound::Flush(reply));
                            }
                            Command::RememberStoredTitle { title } => {
                                ext.titles.remember_stored_title(&title);
                            }
                            Command::SetClientTitle { map } => {
                                ext.titles.client_title = Some(map);
                            }
                            Command::TitleGenerated { title, fallback } => {
                                let published = ext.titles.on_title_generated(
                                    title.as_deref(),
                                    fallback
                                        .as_ref()
                                        .map(|(t, m)| (t.as_str(), m.as_deref())),
                                );
                                // The generation ended: the client title changes that
                                // waited for it persist now, in the order they came.
                                for change in std::mem::take(&mut ext.t2a.parked_explicit) {
                                    start_explicit_persist(change, &command_tx);
                                }
                                if let Some((sanitized_title, updated_at)) = published {
                                    let mut update = json!({
                                        "sessionUpdate": "session_info_update",
                                        "title": sanitized_title,
                                    });
                                    if let Some(up) = updated_at {
                                        update["updatedAt"] = json!(up);
                                    }
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                        "sessionId": session_id,
                                        "update": update,
                                    })));
                                }
                            }
                            Command::InitConfig { config } => {
                                // MAJOR 1 / P4 + F5b/B-71: seed the context window at
                                // session creation with `immediateContextWindow`: the
                                // cross-session cached authoritative value (keyed by the
                                // backend and the model's resolved id), else the
                                // id/displayName/description heuristic, else the default.
                                ext.usage.provider_cache_key = config.provider_cache_key.clone();
                                let info = config
                                    .model_infos
                                    .iter()
                                    .find(|m| m.value == config.current_model_id);
                                let seeded = crate::session_model::immediate_context_window(
                                    &config.provider_cache_key,
                                    &config.current_model_id,
                                    info,
                                );
                                ext.usage.context_window = seeded.size;
                                ext.usage.authoritative = seeded.authoritative;
                                ext.config = *config;
                            }
                            Command::SetMode { mode_id, reply } => {
                                match mode_id.as_str() {
                                    "auto" | "default" | "acceptEdits" | "bypassPermissions" | "dontAsk" | "plan" => {}
                                    _ => {
                                        let _ = reply.send(Err("Invalid Mode".to_string()));
                                        continue;
                                    }
                                }
                                if !ext.config.modes.available_modes.iter().any(|m| m.id == mode_id) {
                                    let _ = reply.send(Err(format!("Mode {mode_id} is not available in this session")));
                                    continue;
                                }
                                let current_model_info = ext.config.model_infos.iter().find(|m| m.value == ext.config.current_model_id);
                                let is_auto_unavailable = current_model_info.is_some_and(|m| m.supports_auto_mode != Some(true));
                                let effective_mode = if mode_id == "auto" && is_auto_unavailable {
                                    crate::session_config::AUTO_MODE_FALLBACK.to_string()
                                } else {
                                    mode_id.clone()
                                };
                                let control = control.clone();
                                let command_tx = command_tx.clone();
                                tokio::spawn(async move {
                                    let outcome = control
                                        .send_request(json!({
                                            "subtype": "set_permission_mode",
                                            "mode": &effective_mode,
                                        }))
                                        .await;
                                    let mode_result = match outcome {
                                        Ok(_) => Ok(()),
                                        Err(e) => Err(permission_mode_error(&e)),
                                    };
                                    let _ = command_tx.send(Command::ModeApplied {
                                        effective_mode,
                                        requested_mode: mode_id,
                                        result: mode_result,
                                        reply,
                                    });
                                });
                            }
                            Command::ModeApplied {
                                effective_mode,
                                requested_mode,
                                result,
                                reply,
                            } => {
                                match result {
                                    Err(e) => {
                                        let _ = reply.send(Err(e));
                                    }
                                    Ok(()) => {
                                        let prev_mode = ext.config.modes.current_mode_id.clone();
                                        if effective_mode != "plan" {
                                            ext.config.pre_plan_mode = None;
                                        } else if prev_mode != "plan" {
                                            ext.config.pre_plan_mode = Some(prev_mode);
                                        }
                                        ext.config.modes.current_mode_id = effective_mode.clone();
                                        for opt in &mut ext.config.config_options {
                                            if opt.id == crate::session_config::MODE_CONFIG_ID {
                                                opt.current_value = effective_mode.clone();
                                            }
                                        }
                                        if effective_mode != requested_mode {
                                            if requested_mode == "auto" && !ext.auto_mode_fallback_warning_shown {
                                                ext.auto_mode_fallback_warning_shown = true;
                                                let _ = update_tx.send(SessionOutbound::RawUpdate(
                                                    auto_mode_fallback_update(
                                                        &session_id,
                                                        ext.supports_notices(),
                                                    ),
                                                ));
                                            }
                                            let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                                "sessionId": session_id,
                                                "update": {
                                                    "sessionUpdate": "current_mode_update",
                                                    "currentModeId": effective_mode,
                                                }
                                            })));
                                        }
                                        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                            "sessionId": session_id,
                                            "update": {
                                                "sessionUpdate": "config_option_update",
                                                "configOptions": ext.config.config_options,
                                            }
                                        })));
                                        // Node emits the update BEFORE the response; the
                                        // Flush barrier keeps that wire order (INV-20).
                                        let (flush_tx, flush_rx) = oneshot::channel();
                                        let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                                        tokio::spawn(async move {
                                            let _ = flush_rx.await;
                                            let _ = reply.send(Ok(()));
                                        });
                                    }
                                }
                            }
                            Command::PermissionAnswered { value, ctx, request_id, tool_use_id, tool_input, reply } => {
                                // The client answered: the child's request ended
                                // (`awaitingUser`'s finally, `native-subagents.ts:633-644`).
                                if let Some(token) = pending_await_tokens.remove(&request_id) {
                                    let outs = tasks.subagents().awaiting_user_end(token, false);
                                    publish_subagent_outs(
                                        &mut tasks,
                                        &mut map_state,
                                        outs,
                                        &update_tx,
                                        &session_id,
                                        None,
                                    );
                                }
                                // Node resolves the answer after the client's response
                                // (`acp-agent.js:10219-10237`): the Auto check uses the
                                // model current NOW, the fallback rewrite warns for any
                                // tool, and an ExitPlanMode allow publishes the mode
                                // updates before the permission result returns.
                                let response = resolve_permission_answer(
                                    PermissionAnswer {
                                        value: &value,
                                        ctx: &ctx,
                                        request_id: &request_id,
                                        tool_use_id: &tool_use_id,
                                        tool_input: &tool_input,
                                    },
                                    &mut ext,
                                    &update_tx,
                                    &session_id,
                                );
                                let _ = reply.send(response);
                                // F39/B-60: the answered request no longer blocks
                                // the work (`withPendingUserInput`'s finally).
                                map_state.v2.close_input(&request_id, false);
                                drain_v2(&mut map_state, &update_tx, &session_id);
                            }
                            Command::SetConfigOption {
                                config_id,
                                value,
                                reply,
                            } => {
                                let Some(opt) = ext.config.config_options.iter().find(|o| o.id == config_id).cloned() else {
                                    let _ = reply.send(Err(format!("Unknown config option: {config_id}")));
                                    continue;
                                };
                                // M-37 (`acp-agent.ts:8963-8967`, `12464-12478`):
                                // Fast mode carries a boolean (for Clients that
                                // opted into boolean config options) or the
                                // "on"/"off" select fallback, so it bypasses the
                                // string-only validation below.
                                if config_id == crate::session_config::FAST_MODE_CONFIG_ID {
                                    match crate::session_config::resolve_fast_mode_enabled(Some(&value)) {
                                        Err(message) => {
                                            let _ = reply.send(Err(message));
                                        }
                                        Ok(enabled) => {
                                            // A-02: apply the SDK flag first so a
                                            // rejected control request leaves both
                                            // the session state and the option
                                            // untouched (`applyFastMode`,
                                            // `acp-agent.ts:10771-10781`).
                                            let control = control.clone();
                                            let command_tx = command_tx.clone();
                                            tokio::spawn(async move {
                                                let outcome = control
                                                    .send_request(json!({
                                                        "subtype": "apply_flag_settings",
                                                        "settings": {"fastMode": enabled},
                                                    }))
                                                    .await;
                                                let result = outcome.map(|_| ()).map_err(|e| e.to_string());
                                                let _ = command_tx.send(Command::FastModeApplied {
                                                    enabled,
                                                    result,
                                                    reply,
                                                });
                                            });
                                        }
                                    }
                                    continue;
                                }
                                let Some(val_str) = value.as_str() else {
                                    let _ = reply.send(Err(format!("Invalid value for config option {config_id}: {value}")));
                                    continue;
                                };
                                let mut valid_value: Option<String> = None;
                                if opt.options.iter().any(|o| o.value == val_str) || opt.current_value == val_str {
                                    valid_value = Some(val_str.to_string());
                                } else if config_id == crate::session_config::MODEL_CONFIG_ID {
                                    if let Some(resolved) = crate::session_config::resolve_model_preference(&ext.config.model_infos, val_str) {
                                        if opt.options.iter().any(|o| o.value == resolved.value) {
                                            valid_value = Some(resolved.value.clone());
                                        }
                                    }
                                    if valid_value.is_none() {
                                        let option_infos: Vec<crate::session_config::ModelInfo> = opt.options.iter().map(|o| {
                                            crate::session_config::ModelInfo {
                                                value: o.value.clone(),
                                                display_name: o.name.clone(),
                                                description: o.description.clone().unwrap_or_default(),
                                                ..Default::default()
                                            }
                                        }).collect();
                                        if let Some(resolved) = crate::session_config::resolve_model_preference(&option_infos, val_str) {
                                            valid_value = Some(resolved.value.clone());
                                        }
                                    }
                                }
                                let Some(resolved_val) = valid_value else {
                                    let _ = reply.send(Err(format!("Invalid value for config option {config_id}: {val_str}")));
                                    continue;
                                };
                                if config_id == crate::session_config::MODE_CONFIG_ID {
                                    match resolved_val.as_str() {
                                        "auto" | "default" | "acceptEdits" | "bypassPermissions" | "dontAsk" | "plan" => {}
                                        _ => {
                                            let _ = reply.send(Err("Invalid Mode".to_string()));
                                            continue;
                                        }
                                    }
                                    if !ext.config.modes.available_modes.iter().any(|m| m.id == resolved_val) {
                                        let _ = reply.send(Err(format!("Mode {resolved_val} is not available in this session")));
                                        continue;
                                    }
                                    let current_model_info = ext.config.model_infos.iter().find(|m| m.value == ext.config.current_model_id);
                                    let is_auto_unavailable = current_model_info.is_some_and(|m| m.supports_auto_mode != Some(true));
                                    let effective_mode = if resolved_val == "auto" && is_auto_unavailable {
                                        crate::session_config::AUTO_MODE_FALLBACK.to_string()
                                    } else {
                                        resolved_val.clone()
                                    };
                                    let control = control.clone();
                                    let command_tx = command_tx.clone();
                                    let requested_mode = resolved_val.clone();
                                    tokio::spawn(async move {
                                        let outcome = control
                                            .send_request(json!({
                                                "subtype": "set_permission_mode",
                                                "mode": &effective_mode,
                                            }))
                                            .await;
                                        let mode_result = match outcome {
                                            Ok(_) => Ok(()),
                                            Err(e) => Err(permission_mode_error(&e)),
                                        };
                                        let _ = command_tx.send(Command::ConfigModeApplied {
                                            requested_mode,
                                            effective_mode,
                                            result: mode_result,
                                            reply,
                                        });
                                    });
                                } else if config_id == crate::session_config::MODEL_CONFIG_ID {
                                    let control = control.clone();
                                    let command_tx = command_tx.clone();
                                    let model_val = resolved_val.clone();
                                    tokio::spawn(async move {
                                        let outcome = control
                                            .send_request(json!({
                                                "subtype": "set_model",
                                                "model": &model_val,
                                            }))
                                            .await;
                                        let model_result = match outcome {
                                            Ok(_) => Ok(()),
                                            Err(e) => Err(e.to_string()),
                                        };
                                        let _ = command_tx.send(Command::ModelApplied {
                                            model_val,
                                            result: model_result,
                                            reply,
                                            notify_client: false,
                                            inline: false,
                                        });
                                    });
                                } else if config_id == crate::session_config::EFFORT_CONFIG_ID {
                                    let control = control.clone();
                                    let command_tx = command_tx.clone();
                                    let effort_val = resolved_val.clone();
                                    let effort_payload = crate::session_config::effort_flag_settings(Some(&effort_val), ext.config.ultracode);
                                    tokio::spawn(async move {
                                        let outcome = control
                                            .send_request(json!({
                                                "subtype": "apply_flag_settings",
                                                "settings": effort_payload,
                                            }))
                                            .await;
                                        let effort_result = match outcome {
                                            Ok(_) => Ok(()),
                                            Err(e) => Err(e.to_string()),
                                        };
                                        let _ = command_tx.send(Command::EffortApplied {
                                            effort_val,
                                            result: effort_result,
                                            reply,
                                        });
                                    });
                                } else if config_id == crate::session_config::AGENT_CONFIG_ID {
                                    // A-03 (`acp-agent.ts:10676-10688`): a live
                                    // agent switch needs no subprocess restart.
                                    // Apply the SDK flag first; `null` clears the
                                    // flag layer back to the standard agent.
                                    let control = control.clone();
                                    let command_tx = command_tx.clone();
                                    let value = resolved_val.clone();
                                    let payload = crate::session_config::agent_flag_settings(&value);
                                    tokio::spawn(async move {
                                        let outcome = control
                                            .send_request(json!({
                                                "subtype": "apply_flag_settings",
                                                "settings": payload,
                                            }))
                                            .await;
                                        let result = outcome.map(|_| ()).map_err(|e| e.to_string());
                                        let _ = command_tx.send(Command::AgentApplied {
                                            value,
                                            result,
                                            reply,
                                        });
                                    });
                                } else {
                                    // `applyConfigOptionValue`'s final `else`
                                    // (`acp-agent.ts:10717-10721`): any other option id
                                    // just records the resolved value and returns the
                                    // options — no SDK round trip, no update.
                                    for opt in &mut ext.config.config_options {
                                        if opt.id == config_id {
                                            opt.current_value = resolved_val.clone();
                                        }
                                    }
                                    let _ = reply.send(Ok(ext.config.config_options.clone()));
                                }
                            }
                            Command::ConfigModeApplied {
                                requested_mode,
                                effective_mode,
                                result,
                                reply,
                            } => {
                                match result {
                                    Err(e) => {
                                        let _ = reply.send(Err(e));
                                    }
                                    Ok(()) => {
                                        let prev_mode = ext.config.modes.current_mode_id.clone();
                                        if effective_mode != "plan" {
                                            ext.config.pre_plan_mode = None;
                                        } else if prev_mode != "plan" {
                                            ext.config.pre_plan_mode = Some(prev_mode);
                                        }
                                        ext.config.modes.current_mode_id = effective_mode.clone();
                                        for opt in &mut ext.config.config_options {
                                            if opt.id == crate::session_config::MODE_CONFIG_ID {
                                                opt.current_value = effective_mode.clone();
                                            }
                                        }
                                        if requested_mode == "auto" && effective_mode != requested_mode && !ext.auto_mode_fallback_warning_shown {
                                            ext.auto_mode_fallback_warning_shown = true;
                                            let _ = update_tx.send(SessionOutbound::RawUpdate(
                                                auto_mode_fallback_update(
                                                    &session_id,
                                                    ext.supports_notices(),
                                                ),
                                            ));
                                        }
                                        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                            "sessionId": session_id,
                                            "update": {
                                                "sessionUpdate": "current_mode_update",
                                                "currentModeId": effective_mode,
                                            }
                                        })));
                                        // Node emits the update BEFORE the response; the
                                        // Flush barrier keeps that wire order (INV-20).
                                        let options = ext.config.config_options.clone();
                                        let (flush_tx, flush_rx) = oneshot::channel();
                                        let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                                        tokio::spawn(async move {
                                            let _ = flush_rx.await;
                                            let _ = reply.send(Ok(options));
                                        });
                                    }
                                }
                            }
                            Command::ModelApplied {
                                model_val,
                                result,
                                reply,
                                notify_client,
                                inline,
                            } => {
                                match result {
                                    Err(e) => {
                                        let _ = reply.send(Err(e));
                                    }
                                    Ok(()) => {
                                        let model_changed = ext.config.current_model_id != model_val;
                                        if model_changed {
                                            ext.config.current_model_id = model_val.clone();
                                            let new_model_info = ext.config.model_infos.iter().find(|m| m.value == model_val).cloned();
                                            // F5b/A-05/B-71: re-seed the context window on a
                                            // model switch with `immediateContextWindow` — the
                                            // cached authoritative window for (backend, resolved
                                            // model id), else the text heuristic, else the
                                            // default (acp-agent.ts:10524-10527). No IPC here.
                                            let seeded = crate::session_model::immediate_context_window(
                                                &ext.usage.provider_cache_key,
                                                &model_val,
                                                new_model_info.as_ref(),
                                            );
                                            ext.usage.context_window = seeded.size;
                                            ext.usage.authoritative = seeded.authoritative;
                                        }
                                        let new_model_info = ext.config.model_infos.iter().find(|m| m.value == model_val).cloned();
                                        // `reconcileForModel` (`session-mode.ts:225-244`):
                                        // the SDK sync is awaited before the effort sync below
                                        // (SDK control requests are serialized).
                                        let mode_downgraded = ext.config.modes.current_mode_id == "auto"
                                            && auto_mode_downgrade_needed(new_model_info.as_ref());
                                        if mode_downgraded {
                                            ext.config.modes.current_mode_id =
                                                crate::session_config::AUTO_MODE_FALLBACK.to_string();
                                        }
                                        // O-31 (`acp-agent.ts:10537-10539`): a
                                        // `model_not_allowed` reason described the model we just
                                        // left, so it must not follow us onto the new one.
                                        if ext.config.fast_mode_disabled_reason.as_deref()
                                            == Some("model_not_allowed")
                                        {
                                            ext.config.fast_mode_disabled_reason = None;
                                        }
                                        let pinned_effort = ext.config.effort_pinned.clone();
                                        let effort_was_pinned = pinned_effort.is_some();
                                        let effort_pinned_for_new = effort_was_pinned && new_model_info.as_ref().is_some_and(|m| {
                                            m.supports_effort == Some(true) && m.supported_effort_levels.as_ref().is_some_and(|lvls| {
                                                lvls.iter().any(|l| Some(l) == pinned_effort.as_ref())
                                            })
                                        });
                                        let seed_effort = crate::session_config::seeded_effort(
                                            effort_pinned_for_new
                                                .then_some(pinned_effort.as_deref())
                                                .flatten(),
                                            &crate::settings::load(&ext.cwd, &ext.env).effective,
                                            ext.config.effort_settings_override.as_ref(),
                                            new_model_info.as_ref(),
                                            &model_val,
                                        );
                                        // MAJOR 3/4 / P4: rebuild config options for the new
                                        // model honouring the client's `useRecommendedValue`
                                        // (AIR) capability, matching the session-start seed.
                                        // O-31: the Fast mode toggle follows the new model and
                                        // keeps the retained user intent; the Agent picker is
                                        // rebuilt from the discovered agents.
                                        let recommended = use_recommended_value(&ext);
                                        let current_effort_before = ext
                                            .config
                                            .config_options
                                            .iter()
                                            .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
                                            .map(|o| o.current_value.clone());
                                        let applied_before =
                                            crate::session_config::applied_effort_before_switch(
                                                ext.config.applied_effort.as_deref(),
                                                recommended,
                                                current_effort_before.as_deref(),
                                                pinned_effort.as_deref(),
                                            );
                                        ext.config.config_options =
                                            rebuild_config_options(&ext, seed_effort.as_deref());
                                        // MAJOR 4 / P4 (acp-agent.js:7975-7989): sync effort on
                                        // every switch for opted-in (AIR) clients; for a legacy
                                        // non-AIR client only when a user pin was clamped away —
                                        // and then send `effortLevel:null` (`toSdkEffortLevel
                                        // (undefined)`) to clear the flag layer, never the
                                        // displayed value.
                                        let should_sync = recommended
                                            || (effort_was_pinned && !effort_pinned_for_new);
                                        let new_effort = should_sync.then(|| {
                                            ext.config
                                                .config_options
                                                .iter()
                                                .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
                                                .map(|o| o.current_value.clone())
                                        }).flatten();
                                        let sync_value = if recommended { new_effort.as_deref() } else { None };
                                        let effort_payload = should_sync.then(|| {
                                            crate::session_config::effort_flag_settings(
                                                sync_value,
                                                ext.config.ultracode,
                                            )
                                        });
                                        let downgraded_to = mode_downgraded
                                            .then_some(crate::session_config::AUTO_MODE_FALLBACK);
                                        // Review PC-1 (3): the fallback updates, then config_option_update for
                                        // an external switch; a user-driven change returns its options in `reply`.
                                        let warn = mode_downgraded && !ext.auto_mode_fallback_warning_shown;
                                        if warn {
                                            ext.auto_mode_fallback_warning_shown = true;
                                        }
                                        let control = control.clone();
                                        let command_tx = command_tx.clone();
                                        let reply_model = model_val.clone();
                                        let pinned_before = pinned_effort.clone();
                                        let downgraded = downgraded_to.map(String::from);
                                        // Node's `syncModelAfterExternalSwitch` runs inline in
                                        // the consumer loop for the refusal fallback and
                                        // detached for the PostModelSwitch hook; only the
                                        // former holds stream frames behind the switch's
                                        // control round trips and `config_option_update`.
                                        if notify_client && inline {
                                            inline_external_switches_in_flight += 1;
                                        }
                                        tokio::spawn(async move {
                                            if let Some(mode) = downgraded.as_deref() {
                                                let _ = control
                                                    .send_request(json!({
                                                        "subtype": "set_permission_mode",
                                                        "mode": mode,
                                                    }))
                                                    .await;
                                            }
                                            let effort_result = match effort_payload {
                                                Some(payload) => Some(
                                                    control
                                                        .send_request(json!({
                                                            "subtype": "apply_flag_settings",
                                                            "settings": payload,
                                                        }))
                                                        .await
                                                        .map(|_| ())
                                                        .map_err(|e| e.to_string()),
                                                ),
                                                None => None,
                                            };
                                            let _ = command_tx.send(Command::ModelSwitchSynced {
                                                model_val: reply_model,
                                                effort_result,
                                                pinned_before,
                                                applied_before,
                                                effort_pinned_for_new,
                                                new_effort,
                                                use_recommended_value: recommended,
                                                downgraded_to: downgraded.clone(),
                                                warn,
                                                notify_client,
                                                inline,
                                                model_changed,
                                                reply,
                                            });
                                        });
                                    }
                                }
                            }
                            Command::ModelSwitchSynced {
                                model_val,
                                effort_result,
                                pinned_before,
                                applied_before,
                                effort_pinned_for_new,
                                new_effort,
                                use_recommended_value,
                                downgraded_to,
                                warn,
                                notify_client,
                                inline,
                                model_changed,
                                reply,
                            } => {
                                if notify_client && inline {
                                    inline_external_switches_in_flight =
                                        inline_external_switches_in_flight.saturating_sub(1);
                                }
                                match effort_result {
                                    Some(Err(error)) => {
                                        // A-09 (`acp-agent.ts:10637-10668`): the switch
                                        // already succeeded, so effort synchronization is
                                        // best-effort. Preserve the old SDK/pin bookkeeping
                                        // and never advertise the unapplied value: restore
                                        // the last applied value when the new model can
                                        // select it, otherwise omit the effort option until
                                        // a later successful switch rebuilds it.
                                        ext.config.effort_pinned = pinned_before.clone();
                                        ext.config.applied_effort = applied_before.clone();
                                        let still_selectable = applied_before.as_deref().is_some_and(|value| {
                                            ext.config
                                                .config_options
                                                .iter()
                                                .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
                                                .is_some_and(|o| {
                                                    o.option_type == "select"
                                                        && o.options.iter().any(|option| option.value == value)
                                                })
                                        });
                                        if still_selectable {
                                            if let Some(option) = ext
                                                .config
                                                .config_options
                                                .iter_mut()
                                                .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
                                            {
                                                option.current_value =
                                                    applied_before.clone().unwrap_or_default();
                                            }
                                        } else {
                                            ext.config.config_options.retain(|o| {
                                                o.id != crate::session_config::EFFORT_CONFIG_ID
                                            });
                                        }
                                        if let Some(logger) = &ext.logger {
                                            logger.error(&format!(
                                                "Failed to synchronize effort after model switch to \"{model_val}\": {error}"
                                            ));
                                        }
                                    }
                                    Some(Ok(())) => {
                                        ext.config.effort_pinned = if effort_pinned_for_new {
                                            pinned_before
                                        } else {
                                            None
                                        };
                                        ext.config.applied_effort =
                                            crate::session_config::applied_effort_after_sync(
                                                use_recommended_value,
                                                new_effort.as_deref(),
                                            );
                                    }
                                    None => {
                                        if effort_pinned_for_new {
                                            ext.config.effort_pinned = pinned_before.clone();
                                            ext.config.applied_effort = pinned_before;
                                        }
                                    }
                                }
                                let updates = if notify_client {
                                    external_switch_updates(
                                        &session_id,
                                        &ext.config.config_options,
                                        downgraded_to.as_deref(),
                                        warn,
                                        ext.supports_notices(),
                                    )
                                } else {
                                    fallback_updates(
                                        &session_id,
                                        downgraded_to.as_deref(),
                                        warn,
                                        ext.supports_notices(),
                                    )
                                };
                                for update in updates {
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                                }
                                // The inline switch is finished: the frames it
                                // held behind its control round trips and its
                                // `config_option_update` enter the stream now, in
                                // their original order (`acp-agent.ts:6702`).
                                if notify_client && inline && inline_external_switches_in_flight == 0
                                {
                                    release_held_frames(&mut replay, &mut held_behind_mcp);
                                }
                                if model_changed {
                                    let control = control.clone();
                                    let tx = command_tx.clone();
                                    let model_id = model_val.clone();
                                    tokio::spawn(async move {
                                        if let Ok(res) = control
                                            .send_request(json!({"subtype": "get_context_usage", "detail": "summary"}))
                                            .await
                                        {
                                            if let Some(raw) = crate::usage::raw_max_tokens(&res) {
                                                let _ = tx.send(Command::UpdateContextWindow { size: raw, model_id });
                                            }
                                        }
                                    });
                                }
                                let options = ext.config.config_options.clone();
                                let (flush_tx, flush_rx) = oneshot::channel();
                                let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                                tokio::spawn(async move {
                                    let _ = flush_rx.await;
                                    let _ = reply.send(Ok(options));
                                });
                            }
                            Command::EffortApplied {
                                effort_val,
                                result,
                                reply,
                            } => {
                                match result {
                                    Err(e) => {
                                        let _ = reply.send(Err(e));
                                    }
                                    Ok(()) => {
                                        for opt in &mut ext.config.config_options {
                                            if opt.id == crate::session_config::EFFORT_CONFIG_ID {
                                                opt.current_value = effort_val.clone();
                                            }
                                        }
                                        ext.config.current_effort = Some(effort_val.clone());
                                        // O-55 `appliedEffortLevel` (`acp-agent.ts:10704`):
                                        // "default" clears the flag layer, handing effort
                                        // back to the CLI's own resolution.
                                        ext.config.applied_effort =
                                            crate::session_config::applied_effort_after_pick(&effort_val);
                                        ext.config.effort_pinned = if effort_val != "default" {
                                            Some(effort_val)
                                        } else {
                                            None
                                        };
                                        let _ = reply.send(Ok(ext.config.config_options.clone()));
                                    }
                                }
                            }
                            Command::FastModeApplied {
                                enabled,
                                result,
                                reply,
                            } => {
                                match result {
                                    Err(e) => {
                                        // A-02: the rejected flag leaves the
                                        // session state and option untouched.
                                        let _ = reply.send(Err(e));
                                    }
                                    Ok(()) => {
                                        ext.config.fast_mode_enabled = enabled;
                                        refresh_fast_mode_option(
                                            &mut ext.config,
                                            enabled,
                                            ext.caps.boolean_config,
                                        );
                                        let _ = reply.send(Ok(ext.config.config_options.clone()));
                                    }
                                }
                            }
                            Command::AgentApplied {
                                value,
                                result,
                                reply,
                            } => {
                                match result {
                                    Err(e) => {
                                        let _ = reply.send(Err(e));
                                    }
                                    Ok(()) => {
                                        ext.config.current_agent = value.clone();
                                        for opt in &mut ext.config.config_options {
                                            if opt.id == crate::session_config::AGENT_CONFIG_ID {
                                                opt.current_value = value.clone();
                                            }
                                        }
                                        let _ = reply.send(Ok(ext.config.config_options.clone()));
                                    }
                                }
                            }
                            Command::GetState { reply } => {
                                // MAJOR 5: return the actor's LIVE modes + config options so a
                                // `session/resume` on a live session reflects `set_config_option
                                // mode` etc. (Node reads the live session state).
                                let _ = reply.send(Ok((
                                    ext.config.modes.clone(),
                                    ext.config.config_options.clone(),
                                )));
                            }
                            Command::SeedTaskState {
                                state,
                                last_published,
                                reply,
                            } => {
                                // PC-2.3: adopt the Task* state rebuilt by the replay.
                                // Node's `forgetPublishedTaskPlan` clears the AIR dedupe
                                // before the replay, and the replayed plans refill it.
                                ext.task_state = state;
                                ext.last_published_task_plan = last_published;
                                let _ = reply.send(());
                            }
                            Command::FailuresSnapshot { reply } => {
                                let _ = reply.send(ext.t2a.failures.clone());
                            }
                            Command::SeedFailures { book, reply } => {
                                ext.t2a.failures = book;
                                let _ = reply.send(());
                            }
                            Command::HookCallback {
                                request_id,
                                callback_id,
                                input,
                                tool_use_id,
                                reply,
                            } => {
                                handle_hook_callback(
                                    &mut tasks,
                                    HookCall {
                                        request_id: &request_id,
                                        callback_id: &callback_id,
                                        input: &input,
                                        tool_use_id: tool_use_id.as_deref(),
                                    },
                                    &mut map_state,
                                    &mut ext,
                                    HookIo {
                                        update_tx: &update_tx,
                                        command_tx: &command_tx,
                                        session_id: &session_id,
                                        live_background: &live_background,
                                    },
                                    reply,
                                );
                            }
                        }
                    }
                    msg = next_stream_line(&mut replay, &mut stream),
                        if !(eof_seen && inline_external_switches_in_flight > 0) => {
                        // B-17: give the runtime a turn when this stretch of the
                        // backlog has run past the message-loop budget, so a
                        // cancel, close or permission answer is never starved
                        // behind a long backlog (`acp-agent.ts:5651-5660`).
                        message_loop_yielder.maybe_yield().await;
                        let Some(line) = msg else {
                            // An inline external switch still in flight at EOF: its
                            // control round trips can never be answered (the child
                            // that reads them is gone), and its replies arrive on
                            // this very stream. `close()` alone would NOT settle
                            // them — it only stops answering inbound requests, and
                            // the pending map lives until the control task exits —
                            // so the switch's `ModelSwitchSynced` would never come
                            // and its `config_option_update` would be lost.
                            // `fail_pending` settles the round trips; the switch
                            // task then publishes its updates (before the held
                            // frames, as the awaited inline sync does upstream,
                            // `acp-agent.ts:6702`) and drops the counter, which
                            // re-enables the stream branch (its gate above).
                            if inline_external_switches_in_flight > 0 {
                                if !eof_seen {
                                    let _ = control.fail_pending();
                                    eof_seen = true;
                                }
                                // What was held behind an unfinished `/mcp` run
                                // still counts: its result carries the turn's
                                // usage and stop reason. It stays queued for
                                // replay until the switch releases the stream.
                                // This move is only a shortcut of the `/mcp`-EOF
                                // path: without it, `ModelSwitchSynced` releases
                                // the frames itself, and a `/mcp` result held
                                // again is released by the `/mcp`-EOF path when
                                // the next `None` arrives, in the same order (M4
                                // in review-T5FA4 survives this block's removal).
                                if !held_behind_mcp.is_empty() {
                                    stream_ended = true;
                                    release_held_frames(&mut replay, &mut held_behind_mcp);
                                }
                                continue;
                            }
                            // What was held behind an unfinished `/mcp` run still counts:
                            // its result carries the turn's usage and stop reason.
                            if !held_behind_mcp.is_empty() {
                                stream_ended = true;
                                release_held_frames(&mut replay, &mut held_behind_mcp);
                                continue;
                            }
                            // Stream EOF / codec death (8.9): settle/reject every turn.
                            // The child is gone: stop answering any inbound request
                            // still queued in the control stream (`sdk.mjs`
                            // `cleanupPerformed`).
                            let _ = control.close();
                            let error_text = error_text.exit_error_text();
                            let air_failures = ext.air_supports("sessionFailure");
                            // `finishLifecycle` at the end of a stream uses
                            // `failed`/`failed` unless a cancel is pending
                            // (`acp-agent.ts:5782-5786`, `8257`). Read it before
                            // the settle lanes below mutate the machine.
                            let stream_end_cancelled = machine.is_cancelled();
                            // V-06: a clean end after `worker_shutting_down` fails
                            // the active turn with `worker_shutdown`; a held,
                            // cancelled-away or absent turn gets the session-scoped
                            // failure instead (`acp-agent.ts:5750-5767`). The
                            // session-scoped failure is published BEFORE
                            // `finishLifecycle`'s interrupt (`acp-agent.ts:5762-5764`
                            // vs `4645`), while an active turn is failed by
                            // `failActiveWithSessionFailure`, which interrupts first.
                            let mut shutdown_events = Vec::new();
                            if std::mem::take(&mut ext.t2a.pending_worker_shutdown)
                                && child_exit != Some(true)
                            {
                                shutdown_events = machine.fail_active_worker_shutdown();
                                if shutdown_events.is_empty() && air_failures {
                                    let update = ext.t2a.failures.publish(
                                        &session_id,
                                        crate::session_failure::ClaudeFailureKind::WorkerShutdown,
                                        &failures::FailureOptions {
                                            session_scoped: true,
                                            ..failures::FailureOptions::default()
                                        },
                                    );
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                                }
                            }
                            // F2 (B-109): `finishLifecycle` starts with
                            // `compaction.interrupt()` (`acp-agent.ts:4645`): an
                            // entity the dead runtime left open gets its
                            // `cancelled` terminal before any turn settles, and
                            // the pending hook summary is dropped.
                            let updates = ext.t2a.compaction.interrupt();
                            publish_compaction_updates(
                                &mut ext,
                                &mut map_state,
                                updates,
                                &update_tx,
                                &session_id,
                            );
                            let events = match child_exit {
                                // The child died: the SDK throws from `next()` and
                                // `failAllTurns` rejects every in-flight turn with that
                                // error, replaced by `lastErrorResultText` when an error
                                // result preceded it (`sdk.mjs` `readMessages`,
                                // `acp-agent.ts:8240-8330`).
                                Some(true) => {
                                    // The query failed (not a cancel): the session's row
                                    // shows `error` until its transcript changes
                                    // (`acp-agent.ts:8245-8248`).
                                    if !machine.is_cancelled() {
                                        let _ = update_tx
                                            .send(SessionOutbound::Board(BoardEvent::QueryFailed));
                                    }
                                    let message = match error_text {
                                        Some(text) => format!("{CHILD_EXIT_PREFIX}{text}"),
                                        None => {
                                            // `processDied`: the session is evicted, so the
                                            // client starts fresh (`acp-agent.ts:8300-8320`).
                                            // Any other stream error keeps the husk.
                                            let _ = update_tx.send(SessionOutbound::ProcessDied);
                                            PROCESS_DIED_MESSAGE.to_string()
                                        }
                                    };
                                    if air_failures {
                                        // An AIR client's active turn ends `end_turn` with
                                        // `transport_lost` metadata; a held, cancelled or
                                        // absent turn gets the session-scoped failure
                                        // instead (`acp-agent.ts:8275-8288`). Only the
                                        // queued turns are rejected.
                                        let events = machine.fail_all_split(
                                            FailureKind::TransportLost,
                                            FailureKind::SessionEnded,
                                            &message,
                                        );
                                        if !events.iter().any(|e| {
                                            matches!(
                                                e,
                                                TurnEvent::Failed {
                                                    kind: FailureKind::TransportLost,
                                                    ..
                                                }
                                            )
                                        }) {
                                            let update = ext.t2a.failures.publish(
                                                &session_id,
                                                crate::session_failure::ClaudeFailureKind::TransportLost,
                                                &failures::FailureOptions {
                                                    session_scoped: true,
                                                    ..failures::FailureOptions::default()
                                                },
                                            );
                                            let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                                        }
                                        events
                                    } else {
                                        machine.fail_all_with(FailureKind::SessionEnded, &message)
                                    }
                                }
                                // A clean exit ends the iterator: the in-flight turn
                                // settles and only the queued ones are rejected
                                // (`acp-agent.ts:5764-5800`).
                                Some(false) => machine.on_stream_end(),
                                // No verdict (no process behind the stream): the
                                // pre-verdict behaviour, with `lastErrorResultText`
                                // replacing the queued turns' error.
                                None => {
                                    let mut events = machine.on_stream_end();
                                    if let Some(text) = error_text {
                                        let message = format!("{CHILD_EXIT_PREFIX}{text}");
                                        for event in &mut events {
                                            if let TurnEvent::Failed {
                                                kind: FailureKind::SessionEnded,
                                                message: rejected,
                                                ..
                                            } = event
                                            {
                                                *rejected = message.clone();
                                            }
                                        }
                                    }
                                    events
                                }
                            };
                            shutdown_events.extend(events);
                            let events = shutdown_events;
                            // F39: a cycle the stream death ended (or failed)
                            // reports its end with this batch.
                            if let Some(end) = machine.take_cycle_end() {
                                map_state.v2.set_cycle_end(end);
                            }
                            settle_events(
                                events,
                                &mut tasks,
                                &mut map_state,
                                &live_background,
                                &mut pending,
                                &update_tx,
                                &session_id,
                                &high_water,
                                &ext.failure_ctx,
                                ext.air.as_ref(),
                                None,
                                &mut ext.titles,
        &mut ext.t2a.failures,
        &mut ext.t2a.usage_turns,
                            &mut ext.file_change_reports,
                            );
                            // 3.5: close every live async task at teardown (`finish_all`).
                            // A cancel ends them `stopped` (and their children
                            // `cancelled`); a crash without a cancel reports
                            // `failed`, as upstream does (`acp-agent.ts:5782-5786`).
                            finish_async_tasks(
                                &mut tasks,
                                &mut map_state,
                                if stream_end_cancelled { "stopped" } else { "failed" },
                                &update_tx,
                                &session_id,
                                ext.logger.as_deref(),
                            );
                            ext.mcp.clear();
                            break;
                        };
                        // The relay's verdict on how the child exited (see `run`): the
                        // stream's last frame before it closes.
                        if let Some(abnormal) = child_exit_of(&line) {
                            child_exit = Some(abnormal);
                            continue;
                        }
                        // The refusal path's external switch is awaited inline:
                        // Node does not read the child's next frame until its
                        // control round trips and its `config_option_update` are
                        // done (`acp-agent.ts:6702`). Those round trips' replies
                        // arrive on THIS stream, so a control line keeps flowing
                        // and every other frame is held (in the same queue a
                        // `/mcp` run uses) until the last inline switch finishes.
                        if !stream_ended && inline_external_switches_in_flight > 0 {
                            if matches!(
                                dispatch::route(&StreamMsg::new(line.clone()), &dispatch::TurnView::default()),
                                Route::Control
                            ) {
                                let _ = control_in_tx.send(line);
                            } else {
                                held_behind_mcp.push(line);
                            }
                            continue;
                        }
                        // A `/mcp` or `/usage` turn's replacement comes from a spawned
                        // control run whose replies arrive on THIS stream, behind the
                        // CLI's output and result. Node awaits the run where the output
                        // shape appears and the result follows; here a frame that beats
                        // the run's completion waits for its [`McpPrepared`] /
                        // [`Command::UsagePrepared`], with every session frame behind it,
                        // while control traffic (the run's own replies) keeps flowing.
                        if !stream_ended
                            && (!held_behind_mcp.is_empty()
                                || holds_for_local_command_run(&line, &machine, &mut ext))
                        {
                            if matches!(
                                dispatch::route(&StreamMsg::new(line.clone()), &dispatch::TurnView::default()),
                                Route::Control
                            ) {
                                let _ = control_in_tx.send(line);
                            } else {
                                held_behind_mcp.push(line);
                            }
                            continue;
                        }
                        error_text.observe(&line);
                        // U-10: with `emitRawSDKMessages`, every message the SDK query
                        // yields is forwarded as `_claude/sdkMessage` BEFORE it is
                        // processed (`acp-agent.ts:5820-5826`), so the raw frame
                        // precedes any `session/update` it maps to.
                        if forwards_raw_frame(&line)
                            && crate::sdk_options::should_emit_raw_message(&raw_sdk_messages, &line)
                        {
                            let _ = update_tx.send(SessionOutbound::ExtNotification {
                                method: "_claude/sdkMessage".to_string(),
                                params: json!({
                                    "sessionId": session_id,
                                    "message": line.clone(),
                                }),
                            });
                        }
                        let is_idle_transition = line.get("type").and_then(Value::as_str) == Some("system")
                            && line.get("subtype").and_then(Value::as_str) == Some("session_state_changed")
                            && line.get("state").and_then(Value::as_str) == Some("idle");
                        let reset = dispatch_stream(
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
                            &mut tasks,
                            &mut ext,
                        )
                        .await;
                        if reset {
                            pending_await_tokens.clear();
                        }
                        for effect in std::mem::take(&mut ext.t2a.effects) {
                            match effect {
                                state::Effect::SyncModel(model) => {
                                    // The refusal fallback awaits its sync before the
                                    // loop reads on (`acp-agent.ts:6702`).
                                    request_external_model_sync(&ext, &command_tx, &model, true);
                                }
                                state::Effect::ExitPlanRestart => {
                                    let Some(plan) = ext.t2a.restart_plan.take() else {
                                        continue;
                                    };
                                    // The old child's frames are the old context's: none of
                                    // them is read from here, and what the old child still
                                    // asked of the client is withdrawn.
                                    let (hold_tx, hold_rx) = mpsc::unbounded_channel();
                                    stream = hold_rx;
                                    replay.clear();
                                    held_behind_mcp.clear();
                                    stream_ended = false;
                                    eof_seen = false;
                                    abort_pending_permissions(
                                        &mut pending_permissions,
                                        &mut parked_permissions,
                                    );
                                    // The control channel moves to the fresh child:
                                    // every client form the old child opened is
                                    // withdrawn (upstream's per-request signal aborts
                                    // on rebind), and the old runtime's awaiting-user
                                    // tokens are stale.
                                    let _ = abort_pending_elicitations(&mut pending_elicitations);
                                    for (_, (reply, response)) in pending_ask.drain() {
                                        let _ = reply.send(response);
                                    }
                                    pending_await_tokens.clear();
                                    let _ = control.close();
                                    let Some(init) = restarter.as_mut() else {
                                        settle_restart_turn!(
                                            plan,
                                            failed "Fresh Claude context was not created".to_string()
                                        );
                                        let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
                                        stream = closed_rx;
                                        continue;
                                    };
                                    let old = match child_guard.take() {
                                        Some(guard) => clear_context::OldChild::Restarted(guard),
                                        None => init
                                            .stop_first
                                            .take()
                                            .map_or(clear_context::OldChild::None, clear_context::OldChild::First),
                                    };
                                    let params = clear_context::RestartParams {
                                        model: ext.config.current_model_id.clone(),
                                        agent: ext
                                            .config
                                            .config_options
                                            .iter()
                                            .find(|option| option.id == crate::session_config::AGENT_CONFIG_ID)
                                            .map_or_else(
                                                || crate::session_config::DEFAULT_AGENT_ID.to_string(),
                                                |option| option.current_value.clone(),
                                            ),
                                        effort: ext.config.effort_pinned.clone(),
                                        mode: plan.reset.mode.clone(),
                                    };
                                    let spawn = clear_context::restart_spawn(
                                        &init.spawn,
                                        &params,
                                        crate::agent::uuid_v4(),
                                    );
                                    next_restart_id += 1;
                                    let bring = clear_context::BringUp {
                                        spawn,
                                        timings: init.timings.clone(),
                                        initialize: init.initialize.clone(),
                                        control: control.clone(),
                                        control_in_tx: control_in_tx.clone(),
                                        deadline: RESTART_DEADLINE,
                                        // B-74: the fresh context keeps the old
                                        // session's Fast mode
                                        // (`continuePlanInFreshContext`).
                                        fast_mode_desired: ext.config.fast_mode_enabled,
                                        fast_mode_supported:
                                            crate::session_config::model_supports_fast_mode(
                                                &ext.config.model_infos,
                                                &ext.config.current_model_id,
                                            ),
                                        logger: ext.logger.clone(),
                                    };
                                    let task = clear_context::spawn_restart(
                                        next_restart_id,
                                        bring,
                                        old,
                                        command_tx.clone(),
                                    );
                                    restart = Some(InFlightRestart {
                                        id: next_restart_id,
                                        plan,
                                        _task: task,
                                        _hold: hold_tx,
                                    });
                                }
                            }
                        }
                        // The stream reached a tool use a request waits for (F16b, B-62).
                        if !parked_permissions.is_empty() {
                            release_parked_permissions(
                                &mut parked_permissions,
                                None,
                                &machine,
                                &command_tx,
                            );
                        }
                        if is_idle_transition {
                            request_title_info(
                                &ext,
                                &command_tx,
                                &session_id,
                                map_state.is_stopped_work(),
                            );
                        }
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
    // The session is over (the CLI exited, the commands closed, or a close
    // finished): a dialog still open on the client is withdrawn, not left
    // behind. Aborting a round-trip drops its reply channel, which makes the
    // forwarding side send `$/cancel_request` (Node's cleanup aborts every
    // pending permission). Parked requests nobody will release are dropped.
    abort_pending_permissions(&mut pending_permissions, &mut parked_permissions);
    // F23a/F23b: a client elicitation or dialog still open is withdrawn with its
    // CANCEL outcome; aborting the task drops the client request's reply, so the
    // forwarding side sends `$/cancel_request`.
    let _ = elicit_cancel_tx.send(true);
    let _ = abort_pending_elicitations(&mut pending_elicitations);
    for (_, (reply, response)) in pending_ask.drain() {
        let _ = reply.send(response);
    }
    if let Some(sink) = final_state_sink {
        let _ = sink.send(Some((
            ext.config.modes.clone(),
            ext.config.config_options.clone(),
        )));
    }
}

/// Abort every pending permission round-trip and forget the parked ones.
/// End every open permission request of a native subagent child as withdrawn
/// (`awaitingUser`'s finally with `signal.aborted`, `native-subagents.ts:633-644`):
/// Claude Code withdrew them when the interrupt stopped the work.
fn withdraw_awaiting_requests(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    tokens: &mut HashMap<String, crate::native_subagents::AwaitToken>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let tokens: Vec<_> = tokens.drain().map(|(_, token)| token).collect();
    for token in tokens {
        let outs = tasks.subagents().awaiting_user_end(token, true);
        publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, None);
    }
}

fn abort_pending_permissions(
    pending_permissions: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    parked_permissions: &mut Vec<ParkedPermission>,
) {
    for handle in pending_permissions.values() {
        handle.abort();
    }
    pending_permissions.clear();
    parked_permissions.clear();
}

/// The client round-trips a session can have open, bundled so the cancel and
/// withdraw helpers take one handle instead of four.
struct PendingClientRequests<'a> {
    permissions: &'a mut HashMap<String, tokio::task::JoinHandle<()>>,
    elicitations:
        &'a mut HashMap<String, (oneshot::Sender<Value>, Value, tokio::task::JoinHandle<()>)>,
    ask: &'a mut HashMap<String, (oneshot::Sender<Value>, Value)>,
    await_tokens: &'a mut HashMap<String, crate::native_subagents::AwaitToken>,
}

/// Withdraw every pending client elicitation/dialog: abort the task waiting on
/// the client (which sends `$/cancel_request`) and answer the CLI with the
/// request's CANCEL outcome.
fn abort_pending_elicitations(
    pending: &mut HashMap<String, (oneshot::Sender<Value>, Value, tokio::task::JoinHandle<()>)>,
) -> Vec<String> {
    let mut ids = Vec::new();
    for (id, (reply, response, handle)) in pending.drain() {
        handle.abort();
        let _ = reply.send(response);
        ids.push(id);
    }
    ids
}

/// The v2 state of a clear-context restart: a turn that was held for its
/// background subagents when the restart took it keeps the work, so its
/// continuation takes it over (`startForeground`, `acp-agent.ts:7119-7130`;
/// M5 of `review-T6G`).
fn resume_held_turn_after_restart(
    map_state: &mut MapState,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    held_at_restart: bool,
    prompt_uuid: &str,
) {
    if held_at_restart {
        map_state.v2.note_resumed(prompt_uuid);
        drain_v2(map_state, update_tx, session_id);
    }
}

/// The parent-cancel prelude shared by `session/cancel` and the close
/// (`cancelTurns`, `acp-agent.ts:8437-8502`):
///
/// - abort every pending permission round-trip (dropping the reply makes the
///   control handler answer the R28 deny), EXCEPT in the RFD form the requests
///   of a child that can keep running after a parent cancel (issue #2308): they
///   stay open until the client answers or the CLI withdraws them;
/// - withdraw every open client elicitation/dialog with its CANCEL outcome;
/// - end AIR's children as cancelled (the RFD form only counts the cancel);
/// - in the non-RFD form clear the per-call eager sessions and field tracker,
///   and end the open child requests as withdrawn.
fn cancel_parent_round_trips(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    pending: &mut PendingClientRequests<'_>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let rfd = tasks.subagents().form() == Some(crate::acp_subagents::SubagentForm::Rfd);
    let keep: std::collections::HashSet<String> = if rfd {
        pending.await_tokens.keys().cloned().collect()
    } else {
        std::collections::HashSet::new()
    };
    let mut withdrawn: Vec<String> = Vec::new();
    pending.permissions.retain(|id, handle| {
        if keep.contains(id) {
            true
        } else {
            handle.abort();
            withdrawn.push(id.clone());
            false
        }
    });
    // An MCP elicitation or dialog is not a child permission request: it is
    // always withdrawn with its CANCEL outcome.
    withdrawn.extend(abort_pending_elicitations(pending.elicitations));
    let drained: Vec<(String, (oneshot::Sender<Value>, Value))> = pending.ask.drain().collect();
    for (id, (reply, response)) in drained {
        if keep.contains(&id) {
            pending.ask.insert(id, (reply, response));
        } else {
            let _ = reply.send(response);
            withdrawn.push(id);
        }
    }
    // The interrupt withdrew these requests: their forms close as withdrawn by
    // a cancel, so the work reports no `running` before its `idle`
    // (`withPendingUserInput`'s finally).
    for id in withdrawn {
        map_state.v2.close_input(&id, true);
    }
    drain_v2(map_state, update_tx, session_id);
    // B-40/B-96 (`acp-agent.ts:8485-8502`): AIR's draft ends every child here
    // as cancelled, so a later update of an ended child's tool call is dropped
    // instead of reaching that child's session. The RFD form only counts the
    // cancel: a background subagent can keep running after a parent cancel.
    let outs = tasks.subagents().parent_cancelled();
    publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, None);
    if !rfd {
        map_state.clear_eager_sessions();
        map_state.clear_tool_tracker();
        withdraw_awaiting_requests(
            tasks,
            map_state,
            pending.await_tokens,
            update_tx,
            session_id,
        );
    }
}

/// Withdraw one request the CLI cancelled (`control_cancel_request`, or a
/// clear-context restart moving the control channel): abort the client
/// round-trip (which sends `$/cancel_request`), forget the pending answer, and
/// end the child's awaiting-user token as withdrawn.
fn withdraw_request(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    pending: &mut PendingClientRequests<'_>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    request_id: &str,
) {
    if let Some((_, _, handle)) = pending.elicitations.remove(request_id) {
        handle.abort();
    }
    pending.ask.remove(request_id);
    if let Some(handle) = pending.permissions.remove(request_id) {
        handle.abort();
    }
    if let Some(token) = pending.await_tokens.remove(request_id) {
        let outs = tasks.subagents().awaiting_user_end(token, true);
        publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, None);
    }
}

/// End the awaiting-user token of a permission request that ended without an
/// answer (the client errored or the request dropped): `awaitingUser`'s
/// `finally` reports the child `running` when its signal was not aborted.
fn end_await_request(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    pending_await_tokens: &mut HashMap<String, crate::native_subagents::AwaitToken>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    request_id: &str,
) {
    if let Some(token) = pending_await_tokens.remove(request_id) {
        let outs = tasks.subagents().awaiting_user_end(token, false);
        publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, None);
    }
}

/// End the prompts held behind a `/remote-control` command as `cancelled`, as
/// the close ends a queued turn (`acp-agent.ts:3906-3912`).
fn cancel_held_remote_control_prompts(held: &mut std::collections::VecDeque<Command>) {
    for command in held.drain(..) {
        match command {
            Command::Prompt { reply, .. } => {
                let _ = reply.send(Ok(PromptReply {
                    stop_reason: "cancelled".to_string(),
                    usage: None,
                    quota: None,
                    updates: Vec::new(),
                    flush_rx: None,
                    ack: None,
                }));
            }
            // A v2 prompt never inserted answers `-32800`
            // (`RequestError.requestCancelled()`).
            Command::PromptV2 { inserted, .. } => {
                let _ = inserted.send(Err(crate::turn_events::V2IntakeError::Cancelled));
            }
            _ => {}
        }
    }
}

/// Kick off the turn-end title lookup off the actor thread. The actor must
/// never block on the filesystem (`session-titles.ts` `onTurnEnd` awaits
/// `getSessionInfo`), so the lookup runs in `spawn_blocking` and re-enters
/// through [`Command::TitleInfo`]. The lookup is restricted to the session's
/// cwd project directory, and reuses the transcript resolved on an earlier
/// turn end (no directory scan once the title is settled).
fn request_title_info(
    ext: &SessionExt,
    command_tx: &mpsc::UnboundedSender<Command>,
    session_id: &str,
    cancelled: bool,
) {
    // While a client title change is in flight, the session info read below
    // could still hold the previous title. Once a client named the session the
    // read still runs, to adopt a title someone else stores.
    if ext.titles.explicit.active() && !ext.titles.explicit.settled_by_client() {
        return;
    }
    let renames = ext.titles.explicit.renames();
    let projects_root = crate::session_store::projects_root_with_env(&ext.env);
    let cwd = ext.cwd.to_string_lossy().into_owned();
    let cached = ext.titles.transcript.clone();
    let session_id = session_id.to_string();
    let cmd_tx = command_tx.clone();
    tokio::spawn(async move {
        let (transcript, info) = tokio::task::spawn_blocking(move || {
            crate::session_titles::load_stored_info(
                &projects_root,
                &cwd,
                &session_id,
                cached.as_deref(),
            )
        })
        .await
        .unwrap_or((None, None));
        let _ = cmd_tx.send(Command::TitleInfo {
            transcript,
            info,
            cancelled,
            renames,
        });
    });
}

/// Run a client title change: `persist` off the actor, then re-enter with its
/// outcome through [`Command::ExplicitPersisted`].
fn start_explicit_persist(
    change: state::ParkedExplicit,
    command_tx: &mpsc::UnboundedSender<Command>,
) {
    let state::ParkedExplicit {
        publish,
        persist,
        reply,
    } = change;
    let command_tx = command_tx.clone();
    tokio::spawn(async move {
        let outcome = persist().await;
        let _ = command_tx.send(Command::ExplicitPersisted {
            outcome,
            publish,
            reply,
        });
    });
}

/// What the off-actor lookup at a turn end read (see [`Command::TitleInfo`]).
struct TitleRead {
    stored: Option<crate::session_store::StoredSessionInfo>,
    cancelled: bool,
    renames: u64,
}

/// The actor-side half of the turn-end title handling, after the transcript
/// lookup re-entered through [`Command::TitleInfo`]. Mirrors
/// `SessionTitles.onTurnEnd`.
fn apply_title_info(
    ext: &mut SessionExt,
    control: &Control,
    command_tx: &mpsc::UnboundedSender<Command>,
    session_id: &str,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    read: TitleRead,
) {
    let TitleRead {
        stored,
        cancelled,
        renames,
    } = read;
    let publish = |title: Option<(String, Option<String>)>| {
        let Some((title, updated_at)) = title else {
            return;
        };
        let mut update = json!({
            "sessionUpdate": "session_info_update",
            "title": title,
        });
        if let Some(up) = updated_at {
            update["updatedAt"] = json!(up);
        }
        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
            "sessionId": session_id,
            "update": update,
        })));
    };
    if ext.titles.explicit.settled_by_client() {
        // No title is generated over the one the client set, but a custom title
        // stored since by someone else (a `/rename`) is adopted. No client change
        // ran during the read, so the stored title is current; an unchanged one
        // is not published again.
        if let Some(info) = &stored {
            if let Some(custom) = &info.custom_title {
                if ext.titles.explicit.renames() == renames {
                    let last_mod = crate::session_store::iso8601(info.mtime_ms);
                    publish(ext.titles.publish_stored(custom, Some(&last_mod)));
                }
            }
        }
        return;
    }
    // A client title change started during the read.
    if ext.titles.explicit.active() {
        return;
    }
    if let Some(ref info) = stored {
        if let Some(ref custom) = info.custom_title {
            let last_mod = crate::session_store::iso8601(info.mtime_ms);
            publish(ext.titles.adopt_custom_title(custom, Some(&last_mod)));
            return;
        }
    }

    let fallback = stored.and_then(|info| {
        info.summary
            .map(|s| (s, Some(crate::session_store::iso8601(info.mtime_ms))))
    });

    if ext.titles.can_request(cancelled) {
        ext.titles.mark_settled();
        ext.titles.explicit.begin_generation();
        let control = control.clone();
        let cmd_tx = command_tx.clone();
        let description = ext.titles.context_trimmed();
        tokio::spawn(async move {
            let req = json!({
                "subtype": "generate_session_title",
                "description": description,
                "persist": true,
            });
            let title = match control.send_request(req).await {
                Ok(res) => res
                    .pointer("/response/response/title")
                    .or_else(|| res.pointer("/response/title"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                Err(_) => None,
            };
            let _ = cmd_tx.send(Command::TitleGenerated { title, fallback });
        });
        return;
    }

    // Only while this session has no title of its own: `summary` degrades to the
    // raw first prompt, which must never overwrite a generated title if the
    // persisted one is slow to show up.
    if let Some((fb_title, fb_mod)) = fallback {
        if !ext.titles.settled {
            publish(ext.titles.publish_stored(&fb_title, fb_mod.as_deref()));
        }
    }
}

/// `prompt()` republishes the session's task snapshot before the user frame
/// when a task is not completed (`acp-agent.ts:3867-3869`). A follow-up prompt
/// therefore shows the plan the client holds, and a completed plan is not
/// republished. The AIR dedupe still applies inside [`publish_task_plan`].
fn publish_prompt_start_task_plan(
    session_id: &str,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
) {
    if ext
        .task_state
        .iter()
        .any(|(_, task)| task.status != "completed")
    {
        publish_task_plan(session_id, ext, update_tx, None);
    }
}

fn publish_task_plan(
    session_id: &str,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    parent_tool_use_id: Option<&str>,
) {
    let entries = crate::hooks::task_state_to_plan_entries(&ext.task_state);

    // U-26 (tools.ts:330-352): the dedupe applies to every AIR client, i.e. every
    // client that declared `_meta.jetbrains.air` as an object (air-extension.ts:119-121),
    // not only one that also lists an `air` capability. Non-AIR clients get every plan.
    if ext.air.is_some() {
        let serialized = serde_json::to_string(&entries).unwrap_or_default();
        if ext.last_published_task_plan.as_deref() == Some(&serialized) {
            return;
        }
        ext.last_published_task_plan = Some(serialized);
    }

    let mut update = serde_json::json!({
        "sessionUpdate": "plan",
        "entries": entries
    });
    // A Task* result of a subagent's frame stamps the plan like every other
    // update of that frame (`stampParentToolUseId`, `acp-agent.ts:13356`); the
    // hook-published plans (TaskCreated/TaskCompleted) stay unstamped.
    if let Some(parent) = parent_tool_use_id {
        let mut meta = serde_json::Map::new();
        crate::tool_meta::stamp_parent(&mut meta, Some(parent));
        update["_meta"] = Value::Object(meta);
    }
    let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
        "sessionId": session_id,
        "update": update
    })));
}

/// Resolve the tool_result blocks of one consolidated `user` frame in BLOCK
/// ORDER, mapping each block as it goes. Node pushes a block's plan/suppressed
/// resolution and its mapped update together, in the order the blocks appear
/// (`toAcpNotifications`, `acp-agent.ts:13232-13253`), so resolving every
/// result in a pass before mapping would reorder a multi-result frame.
fn handle_user_frame_blocks(
    io: SendIo<'_>,
    line: &Value,
    blocks: &[Value],
    map_state: &mut MapState,
    ext: &mut SessionExt,
    turn: Option<&str>,
) {
    let SendIo {
        tasks,
        update_tx,
        session_id,
    } = io;
    // PC-1.5: the frame's `tool_use_result` belongs to a single tool result;
    // with several results it cannot be attributed, so it is null.
    let tool_result_count = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .count();
    let message_tool_use_result = if tool_result_count == 1 {
        line.get("tool_use_result").cloned().unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let parent = line.get("parent_tool_use_id").and_then(Value::as_str);
    // The two pending ExitPlanMode lanes see every user message before it is
    // rendered; an accepted clear-context plan's own result is completed below
    // (`observeExitPlanToolResults`, `acp-agent.ts:8016`).
    let accepted_plan_id =
        ext.t2a
            .exit_plan
            .observe_tool_results(blocks, line.get("tool_result_meta"), |id| {
                map_state
                    .tool_use_from_cache(id)
                    .and_then(|tool_use| tool_use.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
    for block in blocks {
        // The ExitPlanMode call this block finishes, with its input: an approved
        // plan is read from the cache before the mapper drops the entry.
        let exit_plan_call = (block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .then(|| block.get("tool_use_id").and_then(Value::as_str))
            .flatten()
            .and_then(|id| {
                map_state
                    .tool_use_from_cache(id)
                    .map(|tool_use| (id, tool_use))
            })
            .filter(|(_, tool_use)| {
                tool_use.get("name").and_then(Value::as_str) == Some("ExitPlanMode")
            })
            .map(|(id, tool_use)| {
                (
                    id.to_string(),
                    tool_use.get("input").cloned().unwrap_or(Value::Null),
                )
            });
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            if let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) {
                ext.hooks.complete(tool_use_id);
                // PC-1.2 (Node onRelease, tools.ts:401-426): an entry released by the
                // 30 s expiry also finishes its hook tool tracker.
                for (released_id, _) in ext.hooks.prune_expired(std::time::Instant::now()) {
                    map_state.finish_hook_tool_tracker(&released_id);
                    map_state.pending_hooks.remove(&released_id);
                }

                if let Some(tool_use) = map_state.tool_use_from_cache(tool_use_id) {
                    let name = tool_use.get("name").and_then(Value::as_str).unwrap_or("");
                    let is_err = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // PC-1.5 / T-09: a plan-rendered (TodoWrite) or
                    // suppressed (Task*) call a permission prompt surfaced is
                    // resolved here (the branches below never emit a
                    // tool_call_update for it, acp-agent.ts:13218-13245).
                    if map_state.has_emitted(tool_use_id)
                        && !crate::tools::should_emit_tool_call(name)
                    {
                        let non_exec = crate::tool_meta::parse_tool_result_meta(
                            line.get("tool_result_meta").unwrap_or(&Value::Null),
                        );
                        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                            "sessionId": session_id,
                            "update": suppressed_result_resolution(
                                tool_use_id,
                                name,
                                is_err,
                                &block["content"],
                                parent,
                                non_exec.get(tool_use_id),
                            ),
                        })));
                    }
                    if !is_err {
                        let input = tool_use.get("input").unwrap_or(&Value::Null);
                        // T-22/T-23: TaskCreate, TaskUpdate and TaskList mutate
                        // the task state (the parser prefers `tool_use_result`);
                        // TaskGet is read-only and stays silent.
                        if crate::hooks::apply_task_tool_result(
                            &mut ext.task_state,
                            name,
                            input,
                            &message_tool_use_result,
                            &block["content"],
                        ) {
                            publish_task_plan(session_id, ext, update_tx, parent);
                        }
                    }
                }
            }
        }
        let mut updates = map::map_user_frame_block(line, block, map_state);
        let async_tasks = ext.air_supports("asyncTasks");
        for update in &mut updates {
            let SessionUpdate::ToolCallUpdate(update) = update else {
                continue;
            };
            let id = update.tool_call_id.to_string();
            if accepted_plan_id.as_deref() == Some(id.as_str()) {
                exit_plan::accepted_plan_tool_result(update);
            }
            crate::tools::background::mark_backgrounded(
                update,
                ext.t2a.background.backgrounded.contains(&id),
                async_tasks,
            );
            if ext.t2a.background.running_terminal.as_deref() == Some(id.as_str()) {
                crate::tools::background::keep_terminal_running(update);
            }
        }
        note_tool_updates(&updates, true, turn, map_state);
        let filtered = map_state.filter_updates(updates);
        send_updates(tasks, map_state, filtered, update_tx, session_id, turn);
        // U-24: an approved plan of the main conversation becomes the session's
        // markdown plan after the tool call's result. A clear-context approval
        // ends the tool with an error, and the restart publishes its plan.
        if let Some((_, input)) = exit_plan_call {
            let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
            if ext.caps.plan && parent.is_none() && !is_error {
                if let Some(plan) = exit_plan::approved_plan(&input, &message_tool_use_result) {
                    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                        "sessionId": session_id,
                        "update": exit_plan::plan_mode_update(&plan),
                    })));
                }
            }
        }
    }
}

/// The Bash results of a user message that prove a command went to the
/// background (`acp-agent.ts:8027-8057`): announce its task, keep its display
/// terminal running when the client renders terminals (the exit follows when the
/// task ends), read the task output paths of the results, and note which Bash
/// calls the client gets marked as backgrounded. The facts are read while the
/// message's tool results are rendered ([`handle_user_frame_blocks`]).
fn note_background_bash(
    line: &Value,
    tasks: &mut AsyncTaskRuntime,
    machine: &TurnMachine,
    map_state: &MapState,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    ext.t2a.background = state::BackgroundFacts::default();
    let content = line
        .pointer("/message/content")
        .cloned()
        .unwrap_or(Value::Null);
    let tool_use_of = |id: &str| map_state.tool_use_from_cache(id).cloned();
    let task = crate::async_tasks::background_bash_task_from_tool_result(
        &content,
        line.get("tool_use_result"),
        &tool_use_of,
    );
    if let Some(task) = &task {
        let updates = tasks.task_backgrounded(task);
        publish_task_updates(tasks, map_state, updates, update_tx, session_id);
    }
    let tool_call_id = task
        .as_ref()
        .and_then(|task| task.get("toolCallId"))
        .and_then(Value::as_str);
    // The command's terminal runs until its task ends, unless the task has ended
    // already (a fast command can end before the tool result comes).
    if let (Some(task_id), Some(tool_call_id)) = (
        task.as_ref()
            .and_then(|task| task.get("taskId"))
            .and_then(Value::as_str),
        tool_call_id,
    ) {
        if ext.caps.terminal_output && !ext.caps.air_client && machine.is_live_task(task_id) {
            ext.t2a.background.running_terminal = Some(tool_call_id.to_string());
            ext.t2a.background_terminals.insert(
                task_id.to_string(),
                state::BackgroundTerminal {
                    tool_call_id: tool_call_id.to_string(),
                    parent_tool_use_id: line
                        .get("parent_tool_use_id")
                        .and_then(Value::as_str)
                        .filter(|parent| !parent.is_empty())
                        .map(str::to_string),
                },
            );
        }
    }
    let updates = tasks.tool_results(&content);
    publish_task_updates(tasks, map_state, updates, update_tx, session_id);
    ext.t2a.background.backgrounded = crate::async_tasks::backgrounded_bash_tool_call_ids(
        &content,
        &tool_use_of,
        tasks,
        tool_call_id,
    );
}

/// Review PC-1 (8): register a PostToolUse callback on first encounter only (Node `!alreadyCached`,
/// acp-agent.ts:13101). Returns whether the callback was registered now.
fn register_hook_first_encounter(
    hooks: &mut crate::hooks::HookRegistry,
    id: &str,
    name: &str,
    parent: Option<String>,
) -> bool {
    if hooks.has(id) {
        return false;
    }
    hooks.register_once(id.to_string(), name.to_string(), parent);
    true
}

/// Review PC-1 (4): a model switch downgrades auto mode only when the model is listed and
/// does not support it; an unlisted model keeps auto (session-mode.ts:225-232).
fn auto_mode_downgrade_needed(info: Option<&crate::session_config::ModelInfo>) -> bool {
    info.is_some_and(|m| m.supports_auto_mode != Some(true))
}

// Review PC-1 (5): the context-window seed for a switched-to model now lives
// in [`crate::session_model::immediate_context_window`] (F5b/B-71).

/// The auto-mode fallback advisory: a live `notice` for a client on the notice
/// contract, else the bold-label transcript line (`session-mode.ts:287-308`).
fn auto_mode_fallback_update(session_id: &str, supports_notices: bool) -> Value {
    const TITLE: &str = "Auto mode unavailable";
    const REASON: &str =
        "the selected model does not support Auto mode; using Accept edits instead.";
    let notice = crate::notices::SessionNotice::new(
        "warning",
        TITLE,
        Some(crate::notices::sentence_case(REASON)),
    );
    // The transcript override keeps the lowercase reason; the notice itself
    // capitalises the fragment because it stands alone after the title.
    let transcript_notice =
        crate::notices::SessionNotice::new("warning", TITLE, Some(REASON.to_string()));
    let transcript = crate::notices::notice_transcript_text(&transcript_notice);
    json!({
        "sessionId": session_id,
        "update": crate::notices::notice_or_transcript_update(
            &notice,
            supports_notices,
            Some(&transcript),
            None,
        ),
    })
}

/// The current_mode_update and fallback warning of a model-driven auto-mode downgrade.
fn fallback_updates(
    session_id: &str,
    downgraded_to: Option<&str>,
    warn: bool,
    supports_notices: bool,
) -> Vec<Value> {
    let Some(mode) = downgraded_to else {
        return Vec::new();
    };
    let mut out = vec![json!({
        "sessionId": session_id,
        "update": {"sessionUpdate": "current_mode_update", "currentModeId": mode},
    })];
    if warn {
        out.push(auto_mode_fallback_update(session_id, supports_notices));
    }
    out
}

/// Review PC-1 (3): the updates of an external model switch. The fallback updates come first and
/// config_option_update last (Node publishFallbackState runs inside applyConfigOptionValue, then
/// updateConfigOption sends config_option_update: acp-agent.ts:10449, 10683).
fn external_switch_updates(
    session_id: &str,
    config_options: &[crate::session_config::ConfigOption],
    downgraded_to: Option<&str>,
    warn: bool,
    supports_notices: bool,
) -> Vec<Value> {
    let mut out = fallback_updates(session_id, downgraded_to, warn, supports_notices);
    out.push(json!({
        "sessionId": session_id,
        "update": {"sessionUpdate": "config_option_update", "configOptions": config_options},
    }));
    out
}

/// Whether the client opted into the AIR `recommendedValue` capability
/// (`clientSupportsRecommendedConfigValue`, `session_config.rs`).
fn use_recommended_value(ext: &SessionExt) -> bool {
    ext.air
        .as_ref()
        .is_some_and(|a| a.supports("recommendedValue"))
}

/// Rebuild the session's config options from the actor's live state
/// (`buildConfigOptions`, `acp-agent.ts:12497-12562`): the Fast mode toggle
/// follows the current model's support with the retained user intent, and the
/// Agent picker is composed from the discovered agents.
fn rebuild_config_options(
    ext: &SessionExt,
    current_effort: Option<&str>,
) -> Vec<crate::session_config::ConfigOption> {
    let fast_mode = crate::session_config::FastModeOptionState {
        supported: crate::session_config::model_supports_fast_mode(
            &ext.config.model_infos,
            &ext.config.current_model_id,
        ),
        enabled: ext.config.fast_mode_enabled,
        use_boolean_option: ext.caps.boolean_config,
        disabled_reason: ext.config.fast_mode_disabled_reason.clone(),
    };
    crate::session_config::build_config_options_with(&crate::session_config::ConfigOptionsInput {
        modes: &ext.config.modes,
        available_models: &ext.config.available_models,
        current_model_id: &ext.config.current_model_id,
        model_infos: &ext.config.model_infos,
        current_effort,
        agents: &ext.config.agents,
        current_agent: &ext.config.current_agent,
        fast_mode: Some(&fast_mode),
        use_recommended_value: use_recommended_value(ext),
    })
}

/// Replace the Fast mode option in place so it reflects `enabled`
/// (`refreshFastModeOption`, `acp-agent.ts:10759-10770`). A no-op when the
/// option isn't present.
fn refresh_fast_mode_option(
    config: &mut crate::session_config::SessionConfigState,
    enabled: bool,
    use_boolean_option: bool,
) {
    let refreshed = crate::session_config::create_fast_mode_config_option(
        enabled,
        use_boolean_option,
        config.fast_mode_disabled_reason.as_deref(),
    );
    for option in &mut config.config_options {
        if option.id == crate::session_config::FAST_MODE_CONFIG_ID {
            *option = refreshed.clone();
        }
    }
}

/// The live notice/transcript line of a Fast mode flip the SDK forced off
/// (`syncFastModeState`, `acp-agent.ts:10834-10852`).
fn fast_mode_flip_update(session_id: &str, explanation: &str, supports_notices: bool) -> Value {
    let notice = crate::notices::SessionNotice::new(
        "warning",
        "Fast mode turned off",
        Some(format!("{}.", crate::notices::sentence_case(explanation))),
    );
    let transcript = format!("**Fast mode turned off:** {explanation}.");
    json!({
        "sessionId": session_id,
        "update": crate::notices::notice_or_transcript_update(
            &notice,
            supports_notices,
            Some(&transcript),
            None,
        ),
    })
}

/// Reconcile the retained Fast mode toggle with an SDK-reported
/// `fast_mode_state` (`syncFastModeState`, `acp-agent.ts:10805-10860`). Returns
/// the updates to publish: the once-only flip explanation (when the SDK refuses
/// a toggle the user turned on) and the `config_option_update`.
fn sync_fast_mode_state(
    ext: &mut SessionExt,
    state: Option<&str>,
    reason: Option<&str>,
    session_id: &str,
) -> Vec<Value> {
    let has_option = ext
        .config
        .config_options
        .iter()
        .any(|o| o.id == crate::session_config::FAST_MODE_CONFIG_ID);
    let Some(sync) = crate::session_config::reconcile_fast_mode(
        state,
        reason,
        ext.config.fast_mode_enabled,
        ext.config.fast_mode_disabled_reason.as_deref(),
        has_option,
    ) else {
        return Vec::new();
    };
    ext.config.fast_mode_enabled = sync.enabled;
    ext.config.fast_mode_disabled_reason = sync.disabled_reason;
    refresh_fast_mode_option(&mut ext.config, sync.enabled, ext.caps.boolean_config);
    let mut out = Vec::new();
    if let Some(explanation) = sync.explanation {
        out.push(fast_mode_flip_update(
            session_id,
            &explanation,
            ext.supports_notices(),
        ));
    }
    out.push(json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "config_option_update",
            "configOptions": ext.config.config_options,
        },
    }));
    out
}

/// PC-1.1 (acp-agent.ts:10724 syncModelAfterExternalSwitch): the model value a
/// PostModelSwitch hook must sync to, or `None` when the adapter already tracks it.
/// The SDK-reported model maps to an option value (display names resolve); an
/// unlisted id is tracked raw.
fn external_switch_target(
    model_infos: &[crate::session_config::ModelInfo],
    current_model_id: &str,
    to_model: &str,
) -> Option<String> {
    let value = crate::session_config::resolve_model_preference(model_infos, to_model)
        .map_or_else(|| to_model.to_string(), |m| m.value);
    (value != current_model_id).then_some(value)
}

/// `syncModelAfterExternalSwitch` (`acp-agent.ts:10724`): the SDK moved the
/// session to `to_model` by itself (a PostModelSwitch hook, a refusal fallback).
/// The same bookkeeping a user-driven change takes (context window, auto-mode
/// clamp, effort sync) runs in the actor's ModelApplied handler; no SDK
/// set_model is sent (the SDK already switched). Nothing happens when the
/// adapter already tracks the model.
///
/// `inline` is true for the refusal fallback, whose caller awaits the sync
/// before the consumer loop reads on (`acp-agent.ts:6702`); the PostModelSwitch
/// hook runs it detached (`setImmediate`, `acp-agent.ts:11328-11337`), so the
/// stream keeps flowing behind it.
fn request_external_model_sync(
    ext: &SessionExt,
    command_tx: &mpsc::UnboundedSender<Command>,
    to_model: &str,
    inline: bool,
) {
    let Some(target) = external_switch_target(
        &ext.config.model_infos,
        &ext.config.current_model_id,
        to_model,
    ) else {
        return;
    };
    // The reply receiver is discarded.
    let (no_reply, _no_reply_rx) = oneshot::channel();
    let _ = command_tx.send(Command::ModelApplied {
        model_val: target,
        result: Ok(()),
        reply: no_reply,
        notify_client: true,
        inline,
    });
}

/// PC-1.5 (acp-agent.ts:13218-13245): the tool_call_update that resolves a
/// plan-rendered (TodoWrite) or suppressed (Task*) call a permission prompt
/// surfaced. Its status follows the result, its rawOutput is the result
/// content, and its `claudeCode` meta carries the tool name plus the
/// non-execution sidecar and the parent (Node's key order).
fn suppressed_result_resolution(
    tool_use_id: &str,
    name: &str,
    is_error: bool,
    content: &Value,
    parent_tool_use_id: Option<&str>,
    non_exec: Option<&crate::tool_meta::NonExecution>,
) -> Value {
    let meta = crate::tool_meta::meta_with_parent_and_nonexec(name, non_exec, parent_tool_use_id);
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_use_id,
        "status": if is_error { "failed" } else { "completed" },
        "rawOutput": content,
        "_meta": meta,
    })
}

/// One inbound `hook_callback` request's fields.
#[derive(Clone, Copy)]
struct HookCall<'a> {
    request_id: &'a str,
    callback_id: &'a str,
    input: &'a Value,
    tool_use_id: Option<&'a str>,
}

/// JavaScript truthiness of an optional JSON value (`!input.agent_id`): a
/// missing key, `null`, `false`, `0` and `""` are falsy; every object, array and
/// non-empty string is truthy.
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// The channels a hook callback may use: session output, the actor's own command
/// queue (re-entry, e.g. PostModelSwitch), and the session id.
#[derive(Clone, Copy)]
struct HookIo<'a> {
    update_tx: &'a mpsc::UnboundedSender<SessionOutbound>,
    command_tx: &'a mpsc::UnboundedSender<Command>,
    session_id: &'a str,
    /// `liveBackgroundTasks`: the parent Agent/Task call of each subagent, for
    /// the v2 change preview's `parentToolUseId` (`toolUseOwner`,
    /// `acp-agent.ts:9940-9956`).
    live_background: &'a HashMap<String, String>,
}

/// The v2 PreToolUse preview of an Edit or Write (T-105, `previewFileChange`,
/// `acp-agent.ts:9958-10031`): the tool call shows the exact patch of the change
/// before the tool runs, so an auto-approved change is visible before it is done.
///
/// Claude Code runs the tool only after the hook returns, so the file is still
/// as it was before the change — the same patch the approval of a permission
/// request shows. A tool call the client has not seen yet goes out with the
/// patch; one it holds gets a `tool_call_update` whose content is pinned so the
/// streamed input cannot replace it.
fn preview_file_change(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    ext: &SessionExt,
    io: HookIo<'_>,
    input: &Value,
    tool_use_id: &str,
) {
    let HookIo {
        update_tx,
        session_id,
        live_background,
        ..
    } = io;
    let tool_name = input
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    let agent_id = input.get("agent_id").and_then(Value::as_str);
    // `toolUseOwner`: the parent Agent/Task call of the subagent that runs the
    // tool, and the session of the child that was announced for it.
    let parent = agent_id.and_then(|id| live_background.get(id).cloned());
    let tool_call_session_id = agent_id
        .filter(|_| tasks.subagents().form().is_some())
        .and_then(|agent_id| tasks.subagents().child(agent_id))
        .filter(|child| child.announced)
        .map(|child| child.session_id.clone())
        .unwrap_or_else(|| session_id.to_string());
    let cwd = Some(ext.cwd.as_path());
    let Some(content) =
        crate::diff::preview_patch_content(tool_name, &tool_input, cwd, crate::diff::PatchForm::V2)
    else {
        return;
    };
    // Hook callbacks bypass the stream, so the hook can come before the
    // streamed tool_use, as `canUseTool` can: then the tool call goes out with
    // the patch. A v2 client takes the v2 patch, not the v1 input diff that the
    // typed shape carries (`v2ToolCallContent`), so the call's content is
    // replaced before it is sent.
    if !map_state.has_emitted(tool_use_id) {
        let mut surfaced = crate::permission::ensure_tool_call_emitted(
            tool_name,
            &tool_input,
            tool_use_id,
            parent.as_deref(),
            map_state,
            cwd,
        );
        if let Some(update) = surfaced.take() {
            let mut value = serde_json::to_value(&update).unwrap_or(Value::Null);
            if let Some(object) = value.as_object_mut() {
                object.insert("content".to_string(), Value::Array(content.clone()));
            }
            map_state.record_eager_session(tool_use_id, &tool_call_session_id);
            // Raw: the typed `SessionNotification` parse skips a content item
            // that is not a v1 block (`VecSkipError`), so a v2 patch would be
            // dropped on the typed channel.
            let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                "sessionId": tool_call_session_id,
                "update": value,
            })));
        }
        return;
    }
    let Some(mut update) = crate::hooks::build_pre_tool_use_preview_update(
        tool_use_id,
        tool_name,
        &tool_input,
        cwd,
        parent.as_deref(),
    ) else {
        return;
    };
    // `fields.apply(update)`: nothing to send when the client already holds the
    // patch. A v2 patch is not a v1 content block, so the field tracker keeps
    // the v1 input diff it holds: the streamed refinement compares equal to it
    // and is filtered, and the v2 translation would drop a v1 diff anyway.
    map_state.filter_hook_tool_call_update(tool_use_id, &mut update);
    if update.get("content").is_none() {
        return;
    }
    // Raw, as above: the typed channel would drop the v2 patch.
    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
        "sessionId": tool_call_session_id,
        "update": update,
    })));
}

fn handle_hook_callback(
    tasks: &mut AsyncTaskRuntime,
    call: HookCall<'_>,
    map_state: &mut MapState,
    ext: &mut SessionExt,
    io: HookIo<'_>,
    reply: oneshot::Sender<Value>,
) {
    let HookCall {
        request_id,
        callback_id,
        input,
        tool_use_id,
    } = call;
    let HookIo {
        update_tx,
        command_tx,
        session_id,
        ..
    } = io;
    // N5 (`review-T6FBFC`): the callback ids depend on the connection's
    // version. A v2 connection registers PreToolUse first, so its ids are
    // `hook_0` (PreToolUse), `hook_1` (PostToolUse), …; a v1 connection never
    // registers PreToolUse and its ids start at `hook_0` with PostToolUse.
    // Dispatching on the v1 constants ran the wrong handler as soon as the v2
    // payload was sent.
    match crate::hooks::HookRole::from_id(callback_id, map_state.v2.reporting()) {
        Some(crate::hooks::HookRole::PreToolUse) => {
            // T-105 (`acp-agent.ts:9958-10031`): an Edit or Write shows its
            // exact patch before it runs, so an auto-approved change is not
            // invisible until it is done. The hook makes no permission
            // decision; a failure only leaves the preview out.
            if let Some(tool_use_id) = tool_use_id {
                // As in `canUseTool`: the work a cancel stopped shows nothing.
                if !map_state.is_stopped_work() {
                    preview_file_change(tasks, map_state, ext, io, input, tool_use_id);
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        Some(crate::hooks::HookRole::PostToolUse) => {
            // `onEnterPlanMode` -> `updateConfigOption` -> `syncConfig`: the mode
            // the session left is remembered for the ExitPlanMode options.
            let previous_mode = ext.config.modes.current_mode_id.clone();
            let entered = crate::hooks::enter_plan_mode_updates(
                input,
                session_id,
                &mut ext.config.modes,
                &mut ext.config.config_options,
            );
            if !entered.is_empty() {
                exit_plan::track_pre_plan_mode(
                    &mut ext.config.pre_plan_mode,
                    &previous_mode,
                    crate::hooks::PLAN_MODE_ID,
                );
            }
            for update in entered {
                let _ = update_tx.send(SessionOutbound::RawUpdate(update));
            }

            if let Some(id) = tool_use_id {
                let entry = ext.hooks.unregister(id);
                let tool_response = input.get("tool_response").cloned().unwrap_or(Value::Null);
                if let Some(entry) = entry {
                    // T-43: the final Edit/Write change (exact patch for a
                    // `diffPatch` client); AIR keeps its `_meta` across reports,
                    // so it gets the response markers only.
                    let update = crate::hooks::post_tool_use_update(
                        id,
                        &entry.tool_name,
                        entry.parent_tool_use_id.as_deref(),
                        &tool_response,
                        map_state.diff_patch(),
                        map_state.is_air(),
                    )
                    .and_then(|mut update| {
                        map_state.filter_hook_tool_call_update(id, &mut update);
                        map_state.filter_hook_meta(update)
                    });
                    if let Some(update) = update {
                        send_update(
                            tasks,
                            map_state,
                            serde_json::json!({
                                "sessionId": session_id,
                                "update": update
                            }),
                            None,
                            update_tx,
                            session_id,
                            None,
                        );
                    }
                }
                map_state.finish_hook_tool_tracker(id);
                map_state.pending_hooks.remove(id);
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        Some(crate::hooks::HookRole::PostModelSwitch) => {
            if input.get("source").and_then(Value::as_str) != Some("sdk")
                && input.get("source").and_then(Value::as_str) != Some("resume")
            {
                if let Some(to_model) = input.get("to_model").and_then(Value::as_str) {
                    // Detached upstream (`setImmediate`): the hook answers first and
                    // the consumer loop keeps reading the stream.
                    request_external_model_sync(ext, command_tx, to_model, false);
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        Some(crate::hooks::HookRole::PostCompact) => {
            // PC-1.1 (acp-agent.ts:11350-11360): a root-session PostCompact records the
            // compaction summary; subagent compactions (with a TRUTHY agent_id) do
            // not. JS truthiness (`!input.agent_id`): null, "", 0 and false do not
            // skip a root summary. The lifecycle keeps the cleaned summary and
            // rides it on the compaction's terminal update (`recordSummary`).
            if !js_truthy(input.get("agent_id")) {
                if let Some(summary) = input.get("compact_summary") {
                    if let Some(text) = summary.as_str() {
                        ext.last_compaction_summary = Some(text.to_string());
                    }
                    ext.t2a.compaction.record_summary(Some(summary));
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        Some(crate::hooks::HookRole::TaskCreated) => {
            // T-77 (tools.ts:467, 470): an empty task_id or task_subject is rejected
            // before the state is touched or a plan published.
            if let Some(task_id) = input
                .get("task_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                if let Some(task_subject) = input
                    .get("task_subject")
                    .and_then(Value::as_str)
                    .filter(|subject| !subject.is_empty())
                {
                    if !ext.task_state.iter().any(|(id, _)| id == task_id) {
                        ext.task_state.push((
                            task_id.to_string(),
                            crate::hooks::TaskStateEntry {
                                subject: task_subject.to_string(),
                                status: "pending".to_string(),
                                active_form: None,
                                description: input
                                    .get("task_description")
                                    .and_then(Value::as_str)
                                    .map(String::from),
                            },
                        ));
                        publish_task_plan(session_id, ext, update_tx, None);
                    }
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        Some(crate::hooks::HookRole::TaskCompleted) => {
            if let Some(task_id) = input
                .get("task_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                if let Some((_, entry)) = ext.task_state.iter_mut().find(|(id, _)| id == task_id) {
                    if entry.status != "completed" {
                        entry.status = "completed".to_string();
                        publish_task_plan(session_id, ext, update_tx, None);
                    }
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        None => {
            let _ = reply.send(crate::hooks::unknown_hook_callback_response(
                request_id,
                callback_id,
            ));
        }
    }
}

/// Whether one inbound line is yielded by the SDK's query stream, and so is
/// eligible for raw `_claude/sdkMessage` forwarding. The SDK's `readMessages`
/// consumes control traffic, `keep_alive`, `transcript_mirror` and host-only
/// `session_state_changed` frames before the adapter ever sees them
/// (`sdk.mjs` `readMessages`), so they are never forwarded.
fn forwards_raw_frame(line: &Value) -> bool {
    let frame_type = line.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(
        frame_type,
        "keep_alive"
            | "control_response"
            | "control_request"
            | "control_cancel_request"
            | "transcript_mirror"
    ) {
        return false;
    }
    !(frame_type == "system"
        && line.get("subtype").and_then(Value::as_str) == Some("session_state_changed")
        && line.get("sdk_host_only").and_then(Value::as_bool) == Some(true))
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
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>>,
    emitted_assistant_text: &mut bool,
    assistant_had_error: &mut bool,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
    tasks: &mut AsyncTaskRuntime,
    ext: &mut SessionExt,
) -> bool {
    // `active_goal` is emitted by the Claude runtime but is not part of the
    // SDK's public message union; Node handles it before the exhaustive
    // switch (`acp-agent.ts:5935-5942`) and never feeds the turn machine.
    // It is not one of the routed session types, so it is handled here.
    if msg.frame_type() == "active_goal" {
        let goal = crate::goal::GoalSnapshot::from_active_goal(&msg.line);
        if let Some(published) = ext.goal.note_runtime(goal) {
            publish_goal(ext.air.is_some(), &published, update_tx, session_id);
        }
        return false;
    }
    // `command_lifecycle` (`msg_lifecycle_v1`) is not in the SDK's public message
    // union either: Node handles it ahead of the exhaustive switch and `continue`s
    // (`acp-agent.ts:5839-5945`). It feeds the machine's orphan accounting only.
    if msg.frame_type() == "command_lifecycle" {
        let uuid = msg
            .line
            .get("command_uuid")
            .and_then(Value::as_str)
            .unwrap_or("");
        let state = msg.line.get("state").and_then(Value::as_str).unwrap_or("");
        // F1 (B-109): a `started` frame for a turn we still hold proves the
        // interrupted command's tail has drained in the SDK's FIFO stream, so
        // the lifecycle accepts compaction frames again (`compaction.resume()`,
        // `acp-agent.ts:5846-5848`). A `/compact` is echo-less: its frames
        // arrive before the turn activates at its result, and this is the only
        // resume they get.
        if state == "started" && machine.has_unsettled_turn(uuid) {
            ext.t2a.compaction.resume();
        }
        if let Some(diagnostic) = machine.on_command_lifecycle(uuid, state) {
            if let Some(logger) = &ext.logger {
                logger.error(&match diagnostic {
                    crate::turn::LifecycleDiagnostic::StartedAfterTerminal => format!(
                        "Session {session_id}: command_lifecycle \"started\" for {uuid} after its terminal frame; orphan accounting may be off for this cancel."
                    ),
                    crate::turn::LifecycleDiagnostic::UnknownState(other) => format!(
                        "Session {session_id}: unknown command_lifecycle state \"{other}\" for {uuid}; treating as uninformative."
                    ),
                });
            }
        }
        // F39: with `msg_lifecycle_v1`, a fresh turn's `started` is when Claude
        // Code takes its prompt in, before its output and its echo
        // (`reportInserted`, `acp-agent.ts:5855-5860`).
        if state == "started" && machine.inserted_now(uuid) {
            map_state.v2.note_inserted(uuid);
            drain_v2(map_state, update_tx, session_id);
        }
        return false;
    }
    let view = dispatch::TurnView::default();
    let mut reset = false;
    match dispatch::route(&msg, &view) {
        Route::Control => {
            let _ = control_in_tx.send(msg.line);
        }
        Route::Session => {
            reset = msg.frame_type() == "conversation_reset";
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
                tasks,
                ext,
            );
            // An activation sweeps the live entries the level signal ended, so the
            // attribution map follows the machine after every session frame.
            sync_live_tasks(machine, live_background, map_state);
        }
        Route::Ignore => {}
    }
    reset
}

/// Drive the turn machine with one session frame, in stream order (8.8).
#[allow(clippy::too_many_arguments)]
fn handle_session_frame(
    mut line: Value,
    machine: &mut TurnMachine,
    map_state: &mut MapState,
    live_background: &mut HashMap<String, String>,
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>>,
    emitted_assistant_text: &mut bool,
    assistant_had_error: &mut bool,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
    tasks: &mut AsyncTaskRuntime,
    ext: &mut SessionExt,
) {
    let frame_type = line
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match frame_type.as_str() {
        // The CLI re-emits the user message (--replay-user-messages); its echo
        // promotes the queued turn. The turn's own echo is not forwarded.
        "user" => {
            // B-29: record the ACP messageId -> SDK uuid mapping (including a
            // replayed echo) before the echo/tool-result handling.
            if let Some(message_id) = crate::mcp_command::message_id_of(&line) {
                if let Some(uuid) = line
                    .get("uuid")
                    .and_then(Value::as_str)
                    .filter(|uuid| !uuid.is_empty())
                {
                    map_state.record_message_id(&message_id, uuid);
                }
            }
            let mut matched_echo = false;
            let uuid_opt = line.get("uuid").and_then(Value::as_str);
            if let Some(uuid) = uuid_opt {
                // G8: the CLI echoed the submitted `/goal …` command, so a
                // result can now roll the optimistic update back
                // (`acp-agent.ts:7728-7730`).
                ext.goal.note_started(uuid);
                let events = machine.on_echo(uuid);
                start_usage_producers(&events, ext);
                if machine.take_text_stretch_closed() {
                    *emitted_assistant_text = false;
                }
                // Node's `activateTurn` clears `session.cancelled`, ending the
                // stopped-work window (Decision 18).
                if events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::Activated { .. }))
                {
                    // The window ends in `settle_events`, after the cancelled
                    // turn this echo hands off from has closed its tool calls.
                    matched_echo = true;
                    ext.usage.reset_turn_scratch();
                    ext.failure_ctx.reset();
                    *assistant_had_error = false;
                }
                sync_after_activation(&events, machine, live_background, map_state);
                settle_events(
                    events,
                    tasks,
                    map_state,
                    live_background,
                    pending,
                    update_tx,
                    session_id,
                    high_water,
                    &ext.failure_ctx,
                    ext.air.as_ref(),
                    None,
                    &mut ext.titles,
                    &mut ext.t2a.failures,
                    &mut ext.t2a.usage_turns,
                    &mut ext.file_change_reports,
                );
            }
            // Phase 15 (Item 3): a `user` frame also carries tool results. When
            // a tool (e.g. Read) executes, the CLI emits a `user` message whose
            // content holds `tool_result` blocks; Node maps those to the
            // terminal `tool_call_update{status:"completed"}` (+ tool output
            // content) via `toAcpNotifications`. The Rust mapper was only ever
            // fed `stream_event`/`assistant` frames, never this user content,
            // so the completing update was dropped. Mirror upstream's echo-skip
            // (`acp-agent.js:3165-3171`): a user frame whose content is a plain
            // string or a single text block is the turn's own echo and is NOT
            // forwarded; anything else (the `tool_result` blocks) is mapped and
            // emitted.
            let content = line
                .pointer("/message/content")
                .cloned()
                .unwrap_or(Value::Null);
            let is_uuid_matched =
                matched_echo || uuid_opt.is_some_and(|u| machine.foreground_target() == Some(u));
            let is_echo =
                if line.get("isReplay").and_then(Value::as_bool) == Some(true) || is_uuid_matched {
                    // Decision 5 / P3 & Minor I: any replayed user frame or uuid-matched
                    // echo is the CLI's echo of a turn, never a tool result. Node `break`s it
                    // after promoting the turn (`acp-agent.js:5682-5694`).
                    true
                } else {
                    match &content {
                        Value::String(_) => true,
                        Value::Array(blocks) => blocks.len() == 1 && blocks[0]["type"] == "text",
                        _ => false,
                    }
                };
            // Node breaks on a queued turn's echo (7796) and on any `isReplay`
            // frame with a uuid (7798-7812) BEFORE the local-command output
            // handling (7898): a replayed stdout frame is dropped, never
            // published a second time.
            let replay_echo =
                uuid_opt.is_some() && line.get("isReplay").and_then(Value::as_bool) == Some(true);
            // AG-3/T-89 (`acp-agent.ts:8016-8024`): a subagent's successful
            // file-tool edits are recorded for the turn's report before the
            // tool-use cache entries are dropped.
            ext.file_change_reports.note_subagent_edits(
                machine.active_prompt_uuid(),
                line.get("parent_tool_use_id").and_then(Value::as_str),
                &content,
                map_state,
            );
            // The work a cancel stopped ends its tools with error results. Until the
            // client is told that the work ended, they close the tool calls it
            // opened; everything else that work sends is dropped, and so is
            // everything after that report. A background subagent that still runs
            // is not that work: its messages go on (`acp-agent.ts:7818-7830`).
            let stopped_work = machine.is_cancelled()
                && !machine.is_running_background_subagent_output(
                    line.get("parent_tool_use_id").and_then(Value::as_str),
                );
            if stopped_work {
                if !is_echo && !machine.work_end_reported() {
                    send_interrupted_tool_results(
                        tasks,
                        &line,
                        map_state,
                        update_tx,
                        session_id,
                        machine.foreground_target(),
                    );
                }
            } else {
                // S-06 / S-07 (`acp-agent.ts:7898-7947`): a live user frame can
                // carry a local command's output as a plain string. The stdout
                // shape is stripped of its markers and mapped as a user chunk (a
                // marker-only frame is logged instead); the stderr shape is
                // logged as an error and never published. A replayed frame or a
                // uuid-matched echo never reaches here (Node breaks first).
                if !replay_echo && !is_uuid_matched {
                    if let Value::String(text) = &content {
                        if text.contains("<local-command-stdout>") {
                            match crate::transcript::strip_local_command_metadata(content.clone()) {
                                Some(Value::String(stripped)) => {
                                    let mut frame = line.clone();
                                    frame["message"]["content"] = Value::String(stripped);
                                    let updates = map::map_user_frame(&frame, map_state);
                                    note_tool_updates(
                                        &updates,
                                        true,
                                        machine.foreground_target(),
                                        map_state,
                                    );
                                    let filtered = map_state.filter_updates(updates);
                                    send_updates(
                                        tasks,
                                        map_state,
                                        filtered,
                                        update_tx,
                                        session_id,
                                        machine.foreground_target(),
                                    );
                                }
                                _ => {
                                    if let Some(logger) = &ext.logger {
                                        logger.log(text);
                                    }
                                }
                            }
                            return;
                        }
                        if text.contains("<local-command-stderr>") {
                            if let Some(logger) = &ext.logger {
                                logger.error(text);
                            }
                            return;
                        }
                    }
                }
                if !is_echo {
                    // T-12: a SendMessage result that resumed a finished subagent
                    // registers it live again, so the turn holds for it, and the
                    // native subagent runtime opens the new generation
                    // (`acp-agent.ts:8058-8080`).
                    if let Some(agent_id) =
                        frames::resumed_native_subagent_id(line.get("tool_use_result"))
                    {
                        // The SendMessage calls whose result carried the resume;
                        // read before the mapper drops their cache entries.
                        let result_ids: Vec<String> = content
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|block| block["type"].as_str() == Some("tool_result"))
                            .filter_map(|block| {
                                block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                            })
                            .collect();
                        let resume = crate::native_subagents::send_message_resume(
                            &map_state.tool_uses_in_order(),
                            &agent_id,
                            &result_ids,
                        );
                        // In the RFD form the result can arrive after the run it
                        // started ended, and then resumes nothing.
                        let resume_id = resume.as_ref().map(|r| r.tool_use_id.as_str());
                        if !tasks.subagents().is_ended_delegation(&agent_id, resume_id) {
                            machine.resume_live_task(&agent_id);
                            sync_live_tasks(machine, live_background, map_state);
                            let outs = tasks.subagents().send_message_resumed(
                                &agent_id,
                                resume.as_ref().and_then(|r| r.text.as_deref()),
                                resume_id,
                            );
                            publish_subagent_outs(
                                tasks, map_state, outs, update_tx, session_id, None,
                            );
                        }
                    }
                    if let Value::Array(blocks) = &content {
                        map::remember_user_frame_tool_names(&line, map_state);
                        note_background_bash(
                            &line, tasks, machine, map_state, ext, update_tx, session_id,
                        );
                        handle_user_frame_blocks(
                            SendIo {
                                tasks,
                                update_tx,
                                session_id,
                            },
                            &line,
                            blocks,
                            map_state,
                            ext,
                            machine.foreground_target(),
                        );
                    } else {
                        let updates = map::map_user_frame(&line, map_state);
                        note_tool_updates(&updates, true, machine.foreground_target(), map_state);
                        let filtered = map_state.filter_updates(updates);
                        send_updates(
                            tasks,
                            map_state,
                            filtered,
                            update_tx,
                            session_id,
                            machine.foreground_target(),
                        );
                    }
                }
            }
        }
        "result" => {
            // The CLI predicts the next prompt right after this result, so a
            // `prompt_suggestion` from here on belongs to the turn just ended
            // (`acp-agent.ts:6786-6787`).
            ext.t2a.prompt_suggestion_expected = true;
            // Decided BEFORE `on_result` (a folded-prompt result names a pending
            // turn that `on_result` may promote or settle).
            let autonomous = machine.result_is_autonomous(&line);

            // `releaseHeld` is the first statement of upstream's result handler
            // (`acp-agent.ts:6790`): a held task is announced before the `/mcp`
            // chunk or the `/goal` rollback this result brings.
            let released = tasks.release_held();
            emit_task_updates(tasks, map_state, released, update_tx, session_id);

            // N-31 (`acp-agent.ts:6805-6816`): reconcile the Fast mode toggle
            // with the SDK's reported state, gated to user-driven turns like
            // every other side effect; it runs even when the turn errors or was
            // cancelled. An autonomous cycle's state lands on the next user
            // turn's result.
            if !autonomous {
                for update in sync_fast_mode_state(
                    ext,
                    line.get("fast_mode_state").and_then(Value::as_str),
                    line.get("fast_mode_disabled_reason")
                        .and_then(Value::as_str),
                    session_id,
                ) {
                    let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                }
            }

            // G8/N-15 (Decision 16): once the submitted `/goal …` command has
            // produced its own result, no older runtime update can still
            // precede it, so the pending state is cleared; a FAILED command
            // rolls the optimistic update back to the previously published
            // goal (`acp-agent.ts:6853-6870`).
            if !autonomous {
                let failed = line
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || line.get("stop_reason").and_then(Value::as_str) == Some("refusal");
                if let Some(goal) = ext.goal.note_result(failed) {
                    publish_goal(ext.air.is_some(), &goal, update_tx, session_id);
                }
            }

            // Node's order (`acp-agent.ts:6783-7024`, B-13 / N-29): the result first
            // releases a held async task (done at the top of this arm), then
            // `ensureActiveTurn` settles a held turn and promotes the queue head
            // (activation resets the per-turn scratch), and only THEN does the
            // result read the context window and publish its `usage_update` — so
            // an echo-less promotion reports no stale usage from the previous turn.
            let mut begin = machine.on_result_begin(&line);
            let begin_events = std::mem::take(&mut begin.events);
            start_usage_producers(&begin_events, ext);
            // A held turn settled by `ensureActiveTurn` closes the stretch before
            // the result reads `deliveredAssistantText` (`acp-agent.ts:6886`).
            if machine.take_text_stretch_closed() {
                *emitted_assistant_text = false;
            }
            if begin_events
                .iter()
                .any(|e| matches!(e, TurnEvent::Activated { .. }))
            {
                ext.usage.reset_turn_scratch();
                ext.failure_ctx.reset();
                *assistant_had_error = false;
            }
            if !begin_events.is_empty() {
                sync_after_activation(&begin_events, machine, live_background, map_state);
                settle_events(
                    begin_events,
                    tasks,
                    map_state,
                    live_background,
                    pending,
                    update_tx,
                    session_id,
                    high_water,
                    &ext.failure_ctx,
                    ext.air.as_ref(),
                    None,
                    &mut ext.titles,
                    &mut ext.t2a.failures,
                    &mut ext.t2a.usage_turns,
                    &mut ext.file_change_reports,
                );
            }

            // A failure result that no turn can take: Node's `failActive`
            // diagnostic (`acp-agent.ts:5527-5529`). The turn machine silently
            // drops a failure with no unsettled active turn; the log keeps it
            // visible. Auth failures take the sign-out lane, and an AIR client
            // with `sessionFailure` publishes a session-scoped record instead,
            // so neither logs this line.
            let air_session_failure = ext
                .air
                .as_ref()
                .is_some_and(|air| air.supports("sessionFailure"));
            let auth_failure = line
                .get("result")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("Please run /login"))
                || crate::session_failure::provider_failure_category(
                    ext.failure_ctx.error.as_deref(),
                    ext.failure_ctx.was_usage_limit,
                ) == crate::session_failure::ClaudeFailureKind::AuthRequired;
            if !air_session_failure
                && !auth_failure
                && !machine.is_cancelled()
                && machine.active_prompt_uuid().is_none()
                && !machine.result_is_swallowed_diagnostic(&line)
                && !machine.result_is_steer_diagnostic(&line)
                && !ext.t2a.exit_plan.is_interruption_result(&line)
                && line.get("stop_reason").and_then(Value::as_str) != Some("refusal")
                && line.get("stop_reason").and_then(Value::as_str) != Some("max_tokens")
                && line.get("is_error").and_then(Value::as_bool) == Some(true)
            {
                if let Some(logger) = &ext.logger {
                    let message = line
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .or_else(|| {
                            line.get("errors")
                                .and_then(Value::as_array)
                                .and_then(|errors| errors.first())
                                .and_then(Value::as_str)
                        })
                        .unwrap_or("");
                    logger.error(&format!(
                        "Session {session_id}: cannot fail active turn because no unsettled active turn exists: {message}"
                    ));
                }
            }

            // `clearFailuresFromEarlierTurns` (`acp-agent.ts:4714-4721`, called
            // at 7061 and 7211): a `next_attempt` failure of another turn is
            // recovered once a user-turn result arrives. Advisories carry no
            // turn id and `recovery` keeps them out of the sweep. Autonomous
            // results and swallowed empty-interruption diagnostics never reach
            // either call (`7061` is inside `!isAutonomousResult`, `7211`
            // follows the autonomous and diagnostic `break`s), so they leave
            // the failures and the last-notice reuse alone.
            if !autonomous && !machine.result_is_swallowed_diagnostic(&line) {
                let result_turn = machine.active_prompt_uuid().map(str::to_string);
                ext.t2a.failures.clear(|failure| {
                    failure.recovery == failures::Recovery::NextAttempt
                        && failure.turn_id.as_deref() != result_turn.as_deref()
                });
                // V-15: a real model answered, so the account works and the
                // session is signed in. The `auth_status` frame is the primary
                // sign-in signal, but a client can sign the user in out of band
                // (`acp-agent.ts:7294-7309`); a cancelled turn never gets here.
                let is_error = line
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // A refusal settles before this point upstream (`7254-7272`).
                let refused = line.get("stop_reason").and_then(Value::as_str) == Some("refusal");
                if !machine.is_cancelled()
                    && !is_error
                    && !refused
                    && ext.usage.last_model.is_some()
                {
                    ext.t2a.failures.clear(|failure| {
                        matches!(
                            failure.recovery,
                            failures::Recovery::RealModelSuccess | failures::Recovery::AuthStatus
                        ) || (failure.failure.severity == "warning"
                            && failure.turn_id.as_deref() == result_turn.as_deref())
                    });
                }
            }

            // Decision 9 / 2.1 + F5b/N-30: update the context window when the
            // matching `modelUsage` entry has a positive `contextWindow`, and
            // seed the cross-session cache keyed by (backend, resolved model id)
            // plus the assistant message's own spelling (acp-agent.ts:6973-7003).
            if let Some(model_usage) = line.get("modelUsage") {
                if let Some(current_model) = ext.usage.last_model.clone() {
                    if let Some((key, val)) =
                        crate::usage::get_matching_model_usage(model_usage, &current_model)
                    {
                        let cw = val.get("contextWindow").and_then(Value::as_u64);
                        if ext.usage.update_window_if_positive(cw) {
                            let window = cw.unwrap_or_default();
                            let provider_key = &ext.usage.provider_cache_key;
                            crate::session_model::cache_context_window(
                                &crate::session_model::context_window_cache_key(provider_key, key),
                                window,
                            );
                            if key != current_model {
                                crate::session_model::cache_context_window(
                                    &crate::session_model::context_window_cache_key(
                                        provider_key,
                                        &current_model,
                                    ),
                                    window,
                                );
                            }
                        }
                    }
                }
            }

            // Emit usage_update if lastAssistantTotalUsage is recorded
            if let Some(last_total) = ext.usage.last_total {
                let cost = line.get("total_cost_usd");
                let origin = line.get("origin");
                let update = crate::usage::build_result_usage_update(
                    last_total,
                    ext.usage.context_window,
                    ext.usage.last_model.as_deref(),
                    cost,
                    origin,
                );
                let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
                    "sessionId": session_id,
                    "update": update
                })));
            }

            // ExitPlanMode's "keep planning" interrupt (`acp-agent.ts:7054-7141`,
            // `7216-7248`): Claude stops the cycle with an error-shaped
            // diagnostic that only the recorded permission answer can explain,
            // so the turn ends `cancelled` at it instead of failing. A cancel
            // forgets both markers; a result that is not the correlated
            // diagnostic never carries them into a later turn.
            let mut reset_to_continue = None;
            let mut exit_plan_interrupted = false;
            if machine.is_cancelled() {
                ext.t2a.exit_plan.clear();
            } else if autonomous {
                if ext.t2a.exit_plan.is_interruption_result(&line) && machine.active_is_held() {
                    exit_plan_interrupted = true;
                    reset_to_continue =
                        note_exit_plan_interruption(machine, &mut ext.t2a.exit_plan);
                }
            } else if !machine.result_is_swallowed_diagnostic(&line) {
                if ext.t2a.exit_plan.is_interruption_result(&line) {
                    exit_plan_interrupted = true;
                    reset_to_continue =
                        note_exit_plan_interruption(machine, &mut ext.t2a.exit_plan);
                } else if ext.t2a.exit_plan.interruption.is_some() {
                    ext.t2a.exit_plan.clear();
                }
            }
            // An error result ends the turn with an error (a cancel does not): the
            // session's list row shows `error` once idle. The interruption of an
            // ExitPlanMode answer and a swallowed diagnostic never get here
            // upstream (`acp-agent.ts:7286-7292`).
            if !autonomous
                && !machine.is_cancelled()
                && !machine.result_is_swallowed_diagnostic(&line)
                && !exit_plan_interrupted
                && line.get("stop_reason").and_then(Value::as_str) != Some("refusal")
                && !machine.result_is_steer_diagnostic(&line)
            {
                let failed = line
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || line.get("subtype").and_then(Value::as_str) != Some("success");
                let _ = update_tx.send(SessionOutbound::Board(BoardEvent::TurnFailed(failed)));
            }
            // S-05 / S-01 / N-10: the #453 result-text fallback is also the
            // local-command output lane. Only a plain `success` result that
            // reaches upstream's success arm claims the turn's structured
            // replacement for its text (`acp-agent.ts:7351-7375`); every other
            // outcome (cancel, refusal, the steered-error lane, `/login`,
            // `max_tokens`, provider errors) exits before it and keeps its
            // result text. The chunk goes out after the goal rollback and the
            // result `usage_update`, just before the turn settles.
            let compaction_delivered = ext.t2a.compaction_output_delivered;
            if !autonomous
                && machine.result_reaches_forward(&line)
                && machine.should_forward_result(
                    &line,
                    *emitted_assistant_text,
                    compaction_delivered,
                )
            {
                // The turn this result belongs to, post-`ensureActiveTurn`
                // (`session.activeTurn ?? firstUnsettledQueuedTurn()`): a held
                // turn settled at `on_result_begin` hands the result to the
                // promoted head, never to itself.
                if machine.foreground_target().is_some() {
                    let result_text = line
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let take = take_local_command_markdown(machine, ext, &result_text);
                    match take {
                        // The frame was held until the producer re-entered; if
                        // it was not, keep the CLI text rather than drop it.
                        LocalCommandTake::Pending => {}
                        // A mirror of an output shape already delivered: the
                        // replacement is not published twice, and the original
                        // result text is suppressed.
                        LocalCommandTake::Suppress => {
                            if let Some(map) = line.as_object_mut() {
                                map.insert("result".to_string(), Value::String(String::new()));
                            }
                        }
                        LocalCommandTake::Replace(text) => {
                            publish_local_command_chunk(
                                &text,
                                crate::mcp_command::message_id_of(&line),
                                update_tx,
                                session_id,
                                emitted_assistant_text,
                                ext,
                            );
                            if let Some(map) = line.as_object_mut() {
                                map.insert("result".to_string(), Value::String(String::new()));
                            }
                        }
                        // Not a local-command turn (or its producer failed):
                        // the result text goes out through the #453 fallback.
                        LocalCommandTake::Keep => {}
                    }
                }
            }
            // The turn's result is its local-command boundary: the turn's
            // producers are spent (`Turn` goes away when it settles).
            if !autonomous {
                if let Some(uuid) = machine.foreground_target().map(str::to_string) {
                    ext.mcp.remove(&uuid);
                    ext.t2a.usage_turns.remove(&uuid);
                }
            }
            let events = machine.on_result_finish(
                &line,
                *emitted_assistant_text,
                compaction_delivered,
                begin,
            );
            // F39: how a cycle that Claude Code started on its own ended goes
            // out with its `CycleEnded` in the batch below.
            if let Some(end) = machine.take_cycle_end() {
                map_state.v2.set_cycle_end(end);
            }
            // A result closes the compaction lifecycle (`compaction.reset()`,
            // `acp-agent.ts:7477-7489`): an in-progress `compaction_update`
            // entity gets its `cancelled` terminal before the prompt response,
            // and the next turn's stale synthetic frame is no longer suppressed
            // by this turn's output. A result that failed the turn goes through
            // `failActiveWithSessionFailure`, which interrupts first (clearing a
            // pending hook summary the abandoned work left behind,
            // `acp-agent.ts:5565`); the finally's reset is then a no-op.
            if !autonomous {
                let updates = if events.iter().any(|e| matches!(e, TurnEvent::Failed { .. })) {
                    ext.t2a.compaction.interrupt()
                } else {
                    ext.t2a.compaction.reset()
                };
                publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
            }
            // An accepted clear-context plan: the machine handed the pending turn
            // over at this result, with the usage it has earned. The run loop
            // continues it in a fresh context.
            if let (Some(reset), Some(turn)) = (reset_to_continue, machine.take_restart_turn()) {
                ext.t2a.restart_plan = Some(state::RestartPlan {
                    reset,
                    turn,
                    queued: machine.take_restart_queue(),
                });
                ext.t2a.effects.push(state::Effect::ExitPlanRestart);
            }
            // Upstream clears emittedAssistantText in the result finally for
            // non-autonomous results.
            let stretch_closed = machine.take_text_stretch_closed();
            if !autonomous || stretch_closed {
                *emitted_assistant_text = false;
            }
            // Review follow-up 3.17-D: a turn's result is its settle boundary —
            // clear every per-tool-call tracker entry so a tool call that never
            // got a tool_result in this turn does not leak into the next.
            if !autonomous {
                map_state.clear_tool_tracker();
            }
            sync_after_activation(&events, machine, live_background, map_state);
            let incomplete_tool_call = settle_events(
                events,
                tasks,
                map_state,
                live_background,
                pending,
                update_tx,
                session_id,
                high_water,
                &ext.failure_ctx,
                ext.air.as_ref(),
                line.get("uuid").and_then(Value::as_str),
                &mut ext.titles,
                &mut ext.t2a.failures,
                &mut ext.t2a.usage_turns,
                &mut ext.file_change_reports,
            );
            // An `end_turn` result the gate turned into an `IncompleteToolCall`
            // failure is `failActiveWithSessionFailure` upstream, which
            // interrupts (`acp-agent.ts:5469`, `5565`): the pending hook summary
            // is dropped and late compaction frames are ignored. The reset above
            // already published the terminal, so this yields nothing.
            if incomplete_tool_call && !autonomous {
                let updates = ext.t2a.compaction.interrupt();
                publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
            }
        }
        "system" => {
            let subtype = line.get("subtype").and_then(Value::as_str).unwrap_or("");
            match subtype {
                // Plugin load failures (CLI 2.1.283+) have no ACP surface; log
                // them so a missing plugin isn't silent
                // (`acp-agent.ts:5999-6001`). Deduped on the serialized list,
                // like `session.loggedPluginErrors`.
                "init" => {
                    // Latch the lifecycle capabilities so cancel() routes orphan
                    // accounting through the per-uuid map instead of the
                    // coalescing-blind count (`acp-agent.ts:5949-5960`).
                    if let Some(capabilities) = line.get("capabilities").and_then(Value::as_array) {
                        let caps: Vec<&str> =
                            capabilities.iter().filter_map(Value::as_str).collect();
                        machine.latch_capabilities(&caps);
                    }
                    // N-03 (`acp-agent.ts:5961-5975`): a fresh init frame (e.g.
                    // after a reinitialize) can carry an updated Fast mode state;
                    // reconcile it with what session creation seeded.
                    for update in sync_fast_mode_state(
                        ext,
                        line.get("fast_mode_state").and_then(Value::as_str),
                        line.get("fast_mode_disabled_reason")
                            .and_then(Value::as_str),
                        session_id,
                    ) {
                        let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                    }
                    // D5 (Decision 9, `acp-agent.ts:5976-5998`): latch the
                    // terminal-bound slash command names. The session/new
                    // advertisement runs before any init frame is observed, so
                    // a first latch (or a genuine change) re-publishes the
                    // now-filtered list. Advisory only: a failed re-advertise
                    // never fails the turn (Node logs and continues).
                    if let Some(terminal) = line
                        .get("terminal_slash_commands")
                        .and_then(Value::as_array)
                    {
                        let names: Vec<String> = terminal
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect();
                        if ext.terminal_slash_commands.as_ref() != Some(&names) {
                            ext.terminal_slash_commands = Some(names.clone());
                            let commands = Value::Array(ext.commands.clone());
                            emit_commands_changed(&commands, &names, ext, update_tx, session_id);
                        }
                    }
                    if let Some(errors) = line.get("plugin_errors").and_then(Value::as_array) {
                        if !errors.is_empty() {
                            let serialized = serde_json::to_string(errors).unwrap_or_default();
                            if ext.logged_plugin_errors.as_deref() != Some(&serialized) {
                                ext.logged_plugin_errors = Some(serialized);
                                if let Some(logger) = &ext.logger {
                                    for error in errors {
                                        // Node template-literal rendering: a
                                        // missing field prints `undefined`; a
                                        // falsy `path` is omitted.
                                        let plugin = error
                                            .get("plugin")
                                            .and_then(Value::as_str)
                                            .unwrap_or("undefined");
                                        let kind = error
                                            .get("type")
                                            .and_then(Value::as_str)
                                            .unwrap_or("undefined");
                                        let path = error
                                            .get("path")
                                            .and_then(Value::as_str)
                                            .filter(|path| !path.is_empty());
                                        let message = error
                                            .get("message")
                                            .and_then(Value::as_str)
                                            .unwrap_or("undefined");
                                        let mut text =
                                            format!("Plugin {plugin} failed to load ({kind})");
                                        if let Some(path) = path {
                                            text.push_str(&format!(" from {path}"));
                                        }
                                        text.push_str(&format!(": {message}"));
                                        logger.error(&text);
                                    }
                                }
                            }
                        }
                    }
                }
                // N-33: a frame of a run that opened before the latest one seen
                // is late and ignored; the task frames below all check it first.
                "task_progress" | "task_started" | "task_notification" | "task_updated"
                    if is_late_task_frame(&line, subtype, ext) => {}
                "task_started" => {
                    // N-17 (`acp-agent.ts:6428-6477`): register the live task.
                    // For a subagent `task_id` is its agent id and `tool_use_id` the
                    // Agent/Task call that spawned it, so its permission requests
                    // (which reach canUseTool with only `agentID`) attribute to the
                    // parent call. A turn waits only on the subagents it spawned.
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    let is_subagent = is_js_truthy_str(line.get("subagent_type"));
                    let parent = line.get("tool_use_id").and_then(Value::as_str);
                    machine.register_live_task(
                        task_id,
                        parent,
                        is_subagent,
                        crate::turn::TaskPlacement {
                            backgrounded: line.get("is_backgrounded").and_then(Value::as_bool),
                            parent_task_id: line.get("parent_task_id").and_then(Value::as_str),
                            task_type: line.get("task_type").and_then(Value::as_str),
                        },
                    );
                    // 3.7: a task registered for a tool call takes that call out
                    // of the turn's foreground work (`registerLiveTask`).
                    if let Some(parent) = parent.filter(|parent| !parent.is_empty()) {
                        map_state.books.forget_foreground(parent);
                    }
                    // F39/B-60: the task's placement decides whether its
                    // requests block the foreground work (`blocksForeground`).
                    map_state.v2.note_task(
                        task_id,
                        line.get("is_backgrounded")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        line.get("parent_task_id").and_then(Value::as_str),
                    );
                    drain_v2(map_state, update_tx, session_id);
                    sync_live_tasks(machine, live_background, map_state);
                    // 3.5: async-task runtime for AIR `asyncTasks` clients.
                    // Subagent / monitor tasks are ignored inside the runtime.
                    let started = pick_fields(
                        &line,
                        &[
                            "task_id",
                            "task_type",
                            "description",
                            "subagent_type",
                            "is_backgrounded",
                            "workflow_name",
                            "skip_transcript",
                            "tool_use_id",
                        ],
                    );
                    note_hidden_tool_call(&line, tasks, map_state);
                    // T4B hook 3: the subagent runtime sees the frame first (`acp-agent.ts:6458`).
                    let outs = tasks.subagent_task_started(&line);
                    publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, None);
                    let updates = tasks.task_started(&started);
                    publish_task_updates(tasks, map_state, updates, update_tx, session_id);
                }
                "task_progress" => {
                    // 3.5: async-task progress (AIR `asyncTasks` clients only).
                    let progress = pick_fields(
                        &line,
                        &[
                            "task_id",
                            "description",
                            "summary",
                            "last_tool_name",
                            "usage",
                            "tool_use_id",
                        ],
                    );
                    note_hidden_tool_call(&line, tasks, map_state);
                    let updates = tasks.task_progress(&progress);
                    publish_task_updates(tasks, map_state, updates, update_tx, session_id);
                }
                "task_notification" => {
                    // N-18 / N-35 (`acp-agent.ts:6479-6517`): the task settled, so no
                    // further tool call can originate from it.
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    let tool_use_id = line
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .filter(|parent| !parent.is_empty());
                    // T4B hook 4 (`acp-agent.ts:6489`): in the RFD form a resumed
                    // child runs under the call that resumed it, and the end of an
                    // earlier delegation ends nothing. Node returns from the whole
                    // case, so the async-task notification, the stopped-call close
                    // and the live-task settle are skipped too — the live task
                    // still belongs to the new run.
                    if !tasks.subagents().is_stale_end(task_id, tool_use_id) {
                        let notification = pick_fields(
                            &line,
                            &["task_id", "status", "summary", "output_file", "tool_use_id"],
                        );
                        note_hidden_tool_call(&line, tasks, map_state);
                        let outs = tasks.subagent_task_notification(&line);
                        publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, None);
                        let updates = tasks.task_notification(&notification);
                        publish_task_updates(tasks, map_state, updates, update_tx, session_id);
                        if let Some(parent) = tool_use_id {
                            // The subagent streams no more text under this parent.
                            forget_subagent_streams(parent, machine, map_state);
                        }
                        end_background_terminal(task_id, ext, update_tx, session_id);
                        if line.get("status").and_then(Value::as_str) == Some("stopped") {
                            close_stopped_subagent_calls(
                                tasks,
                                tool_use_id,
                                machine,
                                map_state,
                                update_tx,
                                session_id,
                            );
                        }
                        let events = settle_live_task(task_id, machine, live_background, map_state);
                        settle_events(
                            events,
                            tasks,
                            map_state,
                            live_background,
                            pending,
                            update_tx,
                            session_id,
                            high_water,
                            &ext.failure_ctx,
                            ext.air.as_ref(),
                            None,
                            &mut ext.titles,
                            &mut ext.t2a.failures,
                            &mut ext.t2a.usage_turns,
                            &mut ext.file_change_reports,
                        );
                    }
                }
                "task_updated" => {
                    // N-18 / N-36 (`acp-agent.ts:6518-6575`).
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    let patch = line.get("patch").cloned().unwrap_or(Value::Null);
                    let updates = tasks.task_updated(task_id, &patch);
                    publish_task_updates(tasks, map_state, updates, update_tx, session_id);
                    // A task moved to the background no longer blocks the tool call
                    // that spawned it, nor its requests the foreground work.
                    if let Some(backgrounded) = patch.get("is_backgrounded") {
                        machine.set_task_backgrounded(task_id, backgrounded == &Value::Bool(true));
                        // `runsInBackground` reads the live set at use: a cancel
                        // closes only the calls of foreground subagents.
                        sync_live_tasks(machine, live_background, map_state);
                        // F39/B-60: the move changes what the work waits on.
                        map_state
                            .v2
                            .set_task_backgrounded(task_id, backgrounded == &Value::Bool(true));
                        drain_v2(map_state, update_tx, session_id);
                    }
                    // T4B hook 5 (`acp-agent.ts:6531-6575`): upstream ends the
                    // child after `asyncTasks.taskUpdated` and the backgrounded
                    // sync. A `running`/`pending` patch resumes a finished child
                    // with the SendMessage text that resumed it
                    // (`sendMessageResumePrompt(session.toolUseCache, task_id)`).
                    // Only those patches need the prompt; building it copies the
                    // whole tool-use cache, including Write contents.
                    let resume_prompt = match patch.get("status").and_then(Value::as_str) {
                        Some("running" | "pending") => {
                            crate::native_subagents::send_message_resume_prompt(
                                &map_state.tool_uses_in_order(),
                                task_id,
                                &[],
                            )
                        }
                        _ => None,
                    };
                    let outs =
                        tasks.subagent_task_updated(task_id, &patch, resume_prompt.as_deref());
                    publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, None);
                    match patch.get("status").and_then(Value::as_str) {
                        // The terminal-status patch is guaranteed per transition
                        // (the notification is deduplicated), so it prunes too.
                        Some("completed" | "failed" | "killed") => {
                            let parent = machine.live_task_parent(task_id).map(str::to_string);
                            // F39/B-60: a settled subagent's request no longer
                            // blocks the foreground work.
                            map_state.v2.remove_task(task_id);
                            if let Some(parent) = parent.as_deref().filter(|p| !p.is_empty()) {
                                forget_subagent_streams(parent, machine, map_state);
                            }
                            end_background_terminal(task_id, ext, update_tx, session_id);
                            if patch.get("status").and_then(Value::as_str) == Some("killed") {
                                close_stopped_subagent_calls(
                                    tasks,
                                    parent.as_deref(),
                                    machine,
                                    map_state,
                                    update_tx,
                                    session_id,
                                );
                            }
                            let events =
                                settle_live_task(task_id, machine, live_background, map_state);
                            settle_events(
                                events,
                                tasks,
                                map_state,
                                live_background,
                                pending,
                                update_tx,
                                session_id,
                                high_water,
                                &ext.failure_ctx,
                                ext.air.as_ref(),
                                None,
                                &mut ext.titles,
                                &mut ext.t2a.failures,
                                &mut ext.t2a.usage_turns,
                                &mut ext.file_change_reports,
                            );
                        }
                        // The SDK can resume a finished subagent under the same
                        // agent id without a new task_started.
                        Some("running" | "pending") => {
                            machine.resume_live_task(task_id);
                            sync_live_tasks(machine, live_background, map_state);
                        }
                        _ => {}
                    }
                }
                // N-43 (`acp-agent.ts:6733-6777`): the full live background-task
                // set on every membership change, with REPLACE semantics. It only
                // reconciles the live set; it never adds an entry (an entry
                // carries no tool_use_id, so it cannot supply the parent call).
                "background_tasks_changed" => {
                    let level = line.get("tasks").cloned().unwrap_or(Value::Null);
                    let updates = tasks.background_tasks_changed(&level);
                    publish_task_updates(tasks, map_state, updates, update_tx, session_id);
                    let live: std::collections::HashSet<String> = level
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|task| task.get("task_id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect();
                    machine.reconcile_background_tasks(&live);
                    sync_live_tasks(machine, live_background, map_state);
                }
                "compact_boundary" => {
                    // N-08 (`acp-agent.ts:6033-6069`): the boundary ends the
                    // compaction. `enrichTerminal` opens the lifecycle even
                    // without metadata (`context-compaction.ts:326`): this
                    // session delivered a compaction output, so a stale synthetic
                    // assistant frame of an earlier attempt is suppressed and the
                    // #453 result fallback stays off (`hasDeliveredOutput`).
                    let facts = line
                        .get("compact_metadata")
                        .and_then(Value::as_object)
                        .map(|metadata| {
                            crate::context_compaction::metadata_from_boundary(&Value::Object(
                                metadata.clone(),
                            ))
                        })
                        .unwrap_or_default();
                    let uuid = line.get("uuid").and_then(Value::as_str).unwrap_or("");
                    let updates = ext.t2a.compaction.finish(
                        uuid,
                        crate::context_compaction::CompactionStatus::Completed,
                        &facts,
                        true,
                    );
                    publish_compaction_updates(ext, map_state, updates, update_tx, session_id);

                    let used_tokens = line
                        .pointer("/compact_metadata/post_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    ext.usage.last_usage = None;
                    ext.usage.last_total = Some(used_tokens);
                    ext.usage.set_used(used_tokens);
                    let update = crate::usage::build_stream_usage_update(
                        used_tokens,
                        ext.usage.context_window,
                        ext.usage.last_model.as_deref(),
                    );
                    let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
                        "sessionId": session_id,
                        "update": update
                    })));
                }
                "session_state_changed" => {
                    let state = line.get("state").and_then(Value::as_str).unwrap_or("");
                    machine.on_session_state(state);
                    // The session index shows the CLI's own state of this session.
                    use crate::session_index::activity::ActivityState;
                    let activity = match state {
                        "idle" => Some(ActivityState::Idle),
                        "running" => Some(ActivityState::Running),
                        "requires_action" => Some(ActivityState::RequiresAction),
                        _ => None,
                    };
                    if let Some(activity) = activity {
                        let _ = update_tx.send(SessionOutbound::Board(BoardEvent::State(activity)));
                    }
                    if state == "idle" {
                        let events = machine.on_idle();
                        // F39: a cycle that ends at this idle (its result will not
                        // come) reports its end with the batch below.
                        if let Some(end) = machine.take_cycle_end() {
                            map_state.v2.set_cycle_end(end);
                        }
                        // A held turn drained here, or a turn failed for the idle
                        // without a result, closes the stretch too.
                        if machine.take_text_stretch_closed() {
                            *emitted_assistant_text = false;
                        }
                        // F7 (B-109): an idle never resets the lifecycle. A held
                        // or cancelled turn settled here was already closed at
                        // its result/cancel, and a lagging owed idle from a
                        // previous turn must not erase the next turn's compaction
                        // state (`compaction.reset()` runs at the result only,
                        // `acp-agent.ts:7478`). Only the #825 fail interrupts,
                        // through `failActiveWithSessionFailure`
                        // (`acp-agent.ts:5565`).
                        if events.iter().any(|e| matches!(e, TurnEvent::Failed { .. })) {
                            let updates = ext.t2a.compaction.interrupt();
                            publish_compaction_updates(
                                ext, map_state, updates, update_tx, session_id,
                            );
                        }
                        // An interrupt can pre-empt the turn's result entirely
                        // (nothing ran the result-case `finally`), so the cancelled
                        // settle closes the delivery stretch here: stale partial-text
                        // state would suppress the next turn's #453 fallback
                        // (`acp-agent.ts:6176-6183`).
                        if events.iter().any(|e| {
                            matches!(
                                e,
                                TurnEvent::Settled {
                                    stop_reason: crate::turn::StopReason::Cancelled,
                                    ..
                                }
                            )
                        }) {
                            *emitted_assistant_text = false;
                        }
                        settle_events(
                            events,
                            tasks,
                            map_state,
                            live_background,
                            pending,
                            update_tx,
                            session_id,
                            high_water,
                            &ext.failure_ctx,
                            ext.air.as_ref(),
                            None,
                            &mut ext.titles,
                            &mut ext.t2a.failures,
                            &mut ext.t2a.usage_turns,
                            &mut ext.file_change_reports,
                        );
                    }
                }
                // N-10 / S-06 (`acp-agent.ts:6070-6087`): the dedicated system
                // shape of a local command's output. Node awaits the turn's
                // structured replacement at this shape and publishes it (or the
                // CLI's own text); a mirror of an already-delivered output is
                // suppressed, and a cancelled local-command turn publishes
                // nothing. The run loop already held this frame until a pending
                // replacement arrived.
                "local_command_output" => {
                    let content = line
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    // T-100 (`acp-agent.ts:6070-6072`): Claude also emits a
                    // failed manual compaction's error as local-command stdout;
                    // consume that one duplicate after the lifecycle carried it.
                    if ext.t2a.compaction.consume_duplicate_error_output(&content) {
                        return;
                    }
                    let message_id = crate::mcp_command::message_id_of(&line);
                    let cancelled_local = machine
                        .foreground_target()
                        .is_some_and(|uuid| is_local_command_turn(ext, uuid))
                        && machine.is_cancelled();
                    let take = take_local_command_markdown(machine, ext, &content);
                    if cancelled_local {
                        return;
                    }
                    match take {
                        // The run loop holds the frame while the producer is
                        // pending; if it could not (defensive), keep the CLI
                        // text rather than drop the output.
                        LocalCommandTake::Pending | LocalCommandTake::Keep => {
                            publish_local_command_chunk(
                                &content,
                                message_id,
                                update_tx,
                                session_id,
                                emitted_assistant_text,
                                ext,
                            );
                        }
                        LocalCommandTake::Suppress => {}
                        LocalCommandTake::Replace(text) => {
                            publish_local_command_chunk(
                                &text,
                                message_id,
                                update_tx,
                                session_id,
                                emitted_assistant_text,
                                ext,
                            );
                        }
                    }
                }
                "status" => {
                    // A model request that serves none of the prompts a cancel
                    // stopped is new work (a follow-up after a background command
                    // finished): it ends the cancelled window, as an activation
                    // does (`acp-agent.ts:6002-6021`, N-06). The drained tail of
                    // the interrupted command also ends the compaction
                    // interruption (`compaction.resume()`).
                    if machine.on_status(&line) {
                        map_state.end_stopped_work();
                        ext.t2a.compaction.resume();
                    }
                    // N-07 (`acp-agent.ts:6014-6032`): the CLI's `compacting`
                    // status opens the lifecycle, a `compact_result` ends it. The
                    // output counts as delivered from the opening status and from
                    // either terminal result, so the #453 fallback and a stale
                    // synthetic frame stay suppressed even without a boundary.
                    let status = line.get("status").and_then(Value::as_str);
                    let compact_result = line.get("compact_result").and_then(Value::as_str);
                    let uuid = line.get("uuid").and_then(Value::as_str).unwrap_or("");
                    if status == Some("compacting") {
                        let updates = ext.t2a.compaction.start(uuid);
                        publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
                    } else if compact_result == Some("success") {
                        let updates = ext.t2a.compaction.finish(
                            uuid,
                            crate::context_compaction::CompactionStatus::Completed,
                            &crate::context_compaction_meta::CompactionFacts::default(),
                            false,
                        );
                        publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
                    } else if compact_result == Some("failed") {
                        let facts = crate::context_compaction_meta::CompactionFacts {
                            error: line
                                .get("compact_error")
                                .and_then(Value::as_str)
                                .filter(|error| !error.is_empty())
                                .map(String::from),
                            ..crate::context_compaction_meta::CompactionFacts::default()
                        };
                        let updates = ext.t2a.compaction.finish(
                            uuid,
                            crate::context_compaction::CompactionStatus::Failed,
                            &facts,
                            false,
                        );
                        publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
                    }
                }
                // N-13: the SDK failed to persist session history; log it so the
                // data loss is not silent (`acp-agent.ts:6289-6298`).
                "mirror_error" => {
                    if let Some(logger) = &ext.logger {
                        logger.error(&frames::mirror_error_log(&line));
                    }
                }
                // N-14: a free-form notice from the SDK (`acp-agent.ts:6354-6406`).
                "informational" => {
                    let supports_notices = ext.supports_notices();
                    let report = frames::informational_update(
                        &line,
                        supports_notices,
                        &mut ext.t2a.noticed_tool_uses,
                    );
                    if let Some(report) = report {
                        if report.transcript {
                            note_transcript_line(&report.update, emitted_assistant_text, ext);
                        }
                        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                            "sessionId": session_id,
                            "update": report.update,
                        })));
                        if let Some(text) = report.notice_text {
                            machine.note_notice_text(text);
                        }
                    }
                }
                // V-06: the worker announced its shutdown. The signal waits for
                // the transport to end: later frames may be buffered output of
                // the shutting-down worker and prove nothing about a new epoch
                // (`acp-agent.ts:6576-6585`).
                "worker_shutting_down" => {
                    ext.t2a.pending_worker_shutdown = true;
                }
                // N-37 (`acp-agent.ts:6586-6600`): a url-mode MCP elicitation
                // finished server-side; the client dismisses any UI it opened.
                // Only meaningful for a url-capable client; failures are ignored.
                "elicitation_complete" => {
                    // Sent inline in stream order (upstream awaits it), so a
                    // following frame's update cannot overtake it.
                    if ext.caps.elicitation_url {
                        let elicitation_id = line
                            .get("elicitation_id")
                            .or_else(|| line.get("elicitationId"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !elicitation_id.is_empty() {
                            let _ = update_tx.send(SessionOutbound::CompleteElicitation {
                                params: json!({ "elicitationId": elicitation_id }),
                            });
                        }
                    }
                }
                // N-38: a retry of the API call is a warning on the turn, for
                // clients that read AIR session failures (`acp-agent.ts:6606-6631`).
                "api_retry" => {
                    if ext.air_supports("sessionFailure") {
                        if let Some((kind, title)) = frames::api_retry_title(&line) {
                            let options = failures::FailureOptions {
                                turn_id: machine.active_prompt_uuid().map(str::to_string),
                                title: Some(title),
                                warning: true,
                                ..failures::FailureOptions::default()
                            };
                            let update = ext.t2a.failures.publish(session_id, kind, &options);
                            let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                        }
                    }
                }
                // N-39: the SDK retried a refused turn on the fallback model
                // (`acp-agent.ts:6632-6709`).
                "model_refusal_fallback" => {
                    let (delivery, synced) = frames::model_refusal_fallback(
                        &line,
                        ext.supports_notices(),
                        ext.air_supports("sessionFailure"),
                    );
                    match delivery {
                        frames::FallbackDelivery::Update { update, transcript } => {
                            if transcript {
                                note_transcript_line(&update, emitted_assistant_text, ext);
                            }
                            let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                "sessionId": session_id,
                                "update": update,
                            })));
                        }
                        frames::FallbackDelivery::Advisory { title, details } => {
                            let options = failures::FailureOptions {
                                title: Some(title),
                                details,
                                ..failures::FailureOptions::default()
                            };
                            let update = ext.t2a.failures.publish(
                                session_id,
                                crate::session_failure::ClaudeFailureKind::Advisory,
                                &options,
                            );
                            let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                        }
                    }
                    if let Some(model) = synced {
                        ext.t2a.effects.push(state::Effect::SyncModel(model));
                    }
                }
                // N-40: the refusal ends the turn as an error; its `result` streams
                // the explanation. The assistant frame's `stop_details` is the
                // primary source, this banner the backup (`acp-agent.ts:6710-6728`).
                "model_refusal_no_fallback" => {
                    let explanation = line
                        .get("api_refusal_explanation")
                        .filter(|value| !value.is_null())
                        .or_else(|| line.get("content"))
                        .and_then(Value::as_str);
                    if let Some(explanation) = explanation {
                        machine.seed_refusal_explanation(
                            explanation,
                            line.get("refused_user_message_uuid") == Some(&Value::Null),
                        );
                    }
                }
                // T-05 / T-82: a memory recall is a completed `read` tool call
                // (`acp-agent.ts:6259-6265`).
                "memory_recall" => {
                    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                        "sessionId": session_id,
                        "update": frames::memory_recall_update(&line),
                    })));
                }
                // P-01 / T-81: an auto-denied tool call fails with the reason
                // (`acp-agent.ts:6299-6353`).
                "permission_denied" => {
                    let air = map_state.is_air();
                    let tool_use_id = line.get("tool_use_id").and_then(Value::as_str);
                    let eager_session = tool_use_id
                        .and_then(|id| map_state.eager_session(id))
                        .map(|session| session.to_string());
                    let denied = frames::permission_denied_update(
                        &line,
                        map_state,
                        live_background,
                        eager_session.as_deref(),
                        air,
                        ext.caps.subagent_form.is_some(),
                    );
                    if let Some(denied) = denied {
                        let filtered = map_state.filter_updates(vec![denied]);
                        send_updates(
                            tasks,
                            map_state,
                            filtered,
                            update_tx,
                            session_id,
                            machine.foreground_target(),
                        );
                    }
                }
                "commands_changed" => {
                    // D5 (Decision 9, `acp-agent.ts:6266-6288`): the frame's
                    // command list is authoritative and replaces the stored
                    // one; the advertisement filters it with the latched
                    // terminal list. Node also resets `session.skillPaths`
                    // here; the port resolves skill paths per advertisement
                    // (its container scan cache lives for one pass), so the
                    // reset is vacuous.
                    let commands = line.get("commands").cloned().unwrap_or(Value::Null);
                    ext.commands = commands.as_array().cloned().unwrap_or_default();
                    let terminal = ext.terminal_slash_commands.clone().unwrap_or_default();
                    emit_commands_changed(&commands, &terminal, ext, update_tx, session_id);
                }
                _ => {}
            }
        }
        // N-22: a login that finished (not authenticating, no error) retires
        // every signed-out failure (`acp-agent.ts:8228-8232`).
        "auth_status" => {
            let authenticating = line
                .get("isAuthenticating")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !authenticating && line.get("error").is_none() && ext.air_supports("sessionFailure")
            {
                ext.t2a.failures.clear(|failure| {
                    failure.failure.kind == crate::session_failure::ClaudeFailureKind::AuthRequired
                });
            }
        }
        "rate_limit_event" => {
            if let Some(last_total) = ext.usage.last_total {
                let rl_info = line.get("rate_limit_info").unwrap_or(&Value::Null);
                let update = crate::usage::build_rate_limit_usage_update(
                    last_total,
                    ext.usage.context_window,
                    ext.usage.last_model.as_deref(),
                    rl_info,
                );
                let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
                    "sessionId": session_id,
                    "update": update
                })));
            }
        }
        "stream_event" | "assistant" => {
            // B-29: a consolidated assistant message records its ACP messageId
            // -> SDK uuid mapping before any gate can drop it.
            if frame_type == "assistant" {
                if let Some(message_id) = crate::mcp_command::message_id_of(&line) {
                    if let Some(uuid) = line
                        .get("uuid")
                        .and_then(Value::as_str)
                        .filter(|uuid| !uuid.is_empty())
                    {
                        map_state.record_message_id(&message_id, uuid);
                    }
                }
            }
            // F16c (B-61): a model cycle Claude Code started on its own becomes the
            // foreground work at its first main-conversation output
            // (`startAutonomousCycle`, run before the message switch).
            machine.start_autonomous_cycle(&line);
            // F39: on a v2 connection the cycle is the session's foreground work,
            // reported as `running` before its output.
            if machine.runs_autonomous_cycle() {
                map_state.v2.start_cycle();
                drain_v2(map_state, update_tx, session_id);
            }
            // N-32 (`acp-agent.ts:7493-7510`): the API's compaction block streams
            // the retained summary text. `content` is null on the opening block
            // and on a failed compaction, but the block itself still opens the
            // legacy tool call. Only the root conversation's compaction is the
            // session's: a subagent compacting its own context must not touch it.
            // It runs BEFORE the stopped-work gate, like upstream (the heartbeat
            // is the first thing in `case "stream_event"`, `acp-agent.ts:7493`
            // vs the gate at `7612`): a client that cancelled mid-compaction
            // still gets its `in_progress` re-assert, and the summary chunks of
            // an announced entity keep flowing.
            if frame_type == "stream_event" {
                let nested = line.get("parent_tool_use_id").is_none_or(|v| !v.is_null());
                let event_type = line.pointer("/event/type").and_then(Value::as_str);
                let block_content = match event_type {
                    Some("content_block_start")
                        if line
                            .pointer("/event/content_block/type")
                            .and_then(Value::as_str)
                            == Some("compaction") =>
                    {
                        Some(
                            line.pointer("/event/content_block/content")
                                .and_then(Value::as_str),
                        )
                    }
                    Some("content_block_delta")
                        if line.pointer("/event/delta/type").and_then(Value::as_str)
                            == Some("compaction_delta") =>
                    {
                        Some(line.pointer("/event/delta/content").and_then(Value::as_str))
                    }
                    _ => None,
                };
                if !nested {
                    if let Some(content) = block_content {
                        let uuid = line.get("uuid").and_then(Value::as_str).unwrap_or("");
                        let updates = ext.t2a.compaction.heartbeat(uuid, content);
                        publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
                    }
                }
            }
            // F16b: the work a cancel stopped. A consolidated message of a
            // cancelled session is dropped whole (before it can touch the usage
            // snapshot or the failure context), except a running background
            // subagent's; its tool uses are recorded as reached first
            // (`acp-agent.ts:7700-7831`, B-18).
            if frame_type == "assistant" {
                if let Some(content) = line.pointer("/message/content") {
                    map_state.books.record_dispatched(content);
                }
                if machine.gate_assistant(&line) {
                    return;
                }
            } else if line.pointer("/event/type").and_then(Value::as_str) == Some("message_start") {
                // A new message starts on this stream: its drop latch resets with
                // the streamed-content record.
                machine.note_stream_message_start(
                    line.get("parent_tool_use_id").and_then(Value::as_str),
                );
            }
            // Synthetic assistant frames carry the CLI's local-command output. On
            // resume the SDK can replay a stale frame from an earlier compact
            // attempt after a later compaction completed: upstream drops it
            // BEFORE the usage/failure snapshot (`acp-agent.ts:7838-7845`), so a
            // stale usage-limit or error frame can never poison the turn's
            // failure category.
            if frame_type == "assistant"
                && line.get("parent_tool_use_id") == Some(&Value::Null)
                && line.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
                && ext.t2a.compaction_output_delivered
            {
                return;
            }
            // Strict null check per Decision 18 and authoring rules: missing is NOT top-level
            let is_strict_top_level = line.get("parent_tool_use_id") == Some(&Value::Null);
            if is_strict_top_level {
                if frame_type == "stream_event" {
                    if let Some(event) = line.get("event") {
                        let ev_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                        if ev_type == "message_start" {
                            if let Some(msg) = event.get("message") {
                                // MAJOR 1 / P4: Node resets the turn usage snapshot on
                                // every message_start (acp-agent.js:5434-5435); a
                                // message_start WITHOUT `usage` clears the snapshot to
                                // zero (`snapshotFromUsage(undefined)`) rather than
                                // keeping the prior turn's values.
                                let usage_val = msg.get("usage").unwrap_or(&Value::Null);
                                ext.usage.last_usage =
                                    Some(crate::usage::snapshot_from_usage(usage_val));
                                if let Some(model) = msg.get("model").and_then(Value::as_str) {
                                    if model != "<synthetic>" {
                                        ext.usage.last_model = Some(model.to_string());
                                        if !ext.usage.authoritative
                                            && ext.usage.context_window
                                                == crate::usage::DEFAULT_CONTEXT_WINDOW
                                        {
                                            if let Some(inferred) =
                                                crate::usage::infer_context_window_from_model(&[
                                                    model,
                                                ])
                                            {
                                                ext.usage.context_window = inferred;
                                            }
                                        }
                                    }
                                }
                            }
                        } else if ev_type == "message_delta" {
                            if let Some(delta_usage) = event.get("usage") {
                                let next_snap = crate::usage::apply_message_delta(
                                    ext.usage.last_usage.as_ref(),
                                    delta_usage,
                                );
                                ext.usage.last_usage = Some(next_snap);
                            }
                        }
                        if ev_type == "message_start" || ev_type == "message_delta" {
                            if let Some(ref snap) = ext.usage.last_usage {
                                let next_usage = snap.total_tokens();
                                if ext.usage.last_total != Some(next_usage) {
                                    ext.usage.last_total = Some(next_usage);
                                    ext.usage.set_used(next_usage);
                                    let update = crate::usage::build_stream_usage_update(
                                        next_usage,
                                        ext.usage.context_window,
                                        ext.usage.last_model.as_deref(),
                                    );
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(
                                        serde_json::json!({
                                            "sessionId": session_id,
                                            "update": update
                                        }),
                                    ));
                                }
                            }
                        }
                    }
                } else if frame_type == "assistant" {
                    if let Some(msg) = line.get("message") {
                        let model = msg.get("model").and_then(Value::as_str);
                        if model != Some("<synthetic>") {
                            if let Some(usage_val) = msg.get("usage") {
                                let snap = crate::usage::snapshot_from_usage(usage_val);
                                let total = snap.total_tokens();
                                ext.usage.last_usage = Some(snap);
                                ext.usage.last_total = Some(total);
                                ext.usage.set_used(total);
                            }
                        }
                        if let Some(m) = model {
                            if m != "<synthetic>" {
                                ext.usage.last_model = Some(m.to_string());
                            }
                        }

                        // U-13: a refused message names the classifier's
                        // explanation in `stop_details` (`acp-agent.ts:7873-7881`).
                        if msg.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
                            machine.note_refusal_frame(
                                msg.pointer("/stop_details/explanation")
                                    .and_then(Value::as_str),
                            );
                        }

                        // Findings 5, 6, 9:
                        // Only strict top-level assistant frames feed failure ctx.
                        // `was_usage_limit` is reassigned on every top-level assistant frame (can clear).
                        let is_limit =
                            crate::session_failure::is_synthetic_usage_limit_message(msg);
                        ext.failure_ctx.was_usage_limit = is_limit;
                        if let Some(err_val) = line.get("error") {
                            let err_str = err_val
                                .as_str()
                                .map(String::from)
                                .unwrap_or_else(|| err_val.to_string());
                            ext.failure_ctx.error = Some(err_str);
                            *assistant_had_error = true;
                        }
                        // Finding 6, Defect 3: Title update condition is current frame error or limit
                        if line.get("error").is_some() || is_limit {
                            if let Some(text) = crate::session_failure::assistant_message_text(msg)
                            {
                                ext.failure_ctx.title = Some(text);
                            }
                        }
                    } else if let Some(err_val) = line.get("error") {
                        let err_str = err_val
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| err_val.to_string());
                        ext.failure_ctx.error = Some(err_str);
                        *assistant_had_error = true;
                    }
                }
            }
            // `lastAssistantModel` (top-level only): the resumed session's first
            // per-model quota row is keyed by it (`acp-agent.js:5436,5741`).
            if line.get("parent_tool_use_id").is_none_or(Value::is_null) {
                let model = if frame_type == "assistant" {
                    line.pointer("/message/model")
                } else if line.pointer("/event/type").and_then(Value::as_str)
                    == Some("message_start")
                {
                    line.pointer("/event/message/model")
                } else {
                    None
                };
                if let Some(model) = model.and_then(Value::as_str) {
                    machine.note_assistant_model(model);
                }
            }
            // F16b: after the usage bookkeeping, a streamed event of the stopped
            // work is dropped once the client was told the work ended, and so is
            // the rest of a stream that already lost a delta to it
            // (`acp-agent.ts:7600-7628`).
            if frame_type == "stream_event" && machine.gate_stream_event(&line) {
                return;
            }
            if frame_type == "assistant" {
                machine.note_consolidated_consumed(&line);
                if let Some(content) = line.pointer("/message/content") {
                    map_state.books.record_dispatched(content);
                    if let Some(blocks) = content.as_array() {
                        for block in blocks {
                            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                if let Some(id) = block.get("id").and_then(Value::as_str) {
                                    let name = block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("Other");
                                    if name != "TodoWrite" && !crate::tools::is_task_tool(name) {
                                        let parent = line
                                            .get("parent_tool_use_id")
                                            .and_then(Value::as_str)
                                            .map(String::from);
                                        register_hook_first_encounter(
                                            &mut ext.hooks,
                                            id,
                                            name,
                                            parent,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            } else if frame_type == "stream_event"
                && line.pointer("/event/type").and_then(Value::as_str)
                    == Some("content_block_start")
            {
                if let Some(block) = line.pointer("/event/content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        if let Some(id) = block.get("id").and_then(Value::as_str) {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("Other");
                            if name != "TodoWrite" && !crate::tools::is_task_tool(name) {
                                let parent = line
                                    .get("parent_tool_use_id")
                                    .and_then(Value::as_str)
                                    .map(String::from);
                                register_hook_first_encounter(&mut ext.hooks, id, name, parent);
                            }
                        }
                    }
                }
            }
            // S-06 (`acp-agent.ts:7839-7895`): a synthetic top-level assistant
            // message is the other shape of a local command's output. On resume
            // the SDK can replay a stale frame from an earlier compact attempt
            // after a later compaction completed: suppress it while a compaction
            // output was delivered. Otherwise claim the turn's structured
            // replacement; a non-local frame (or a failed producer) maps
            // normally, a mirror publishes nothing, and a cancelled
            // local-command turn publishes nothing.
            if frame_type == "assistant"
                && line.get("parent_tool_use_id") == Some(&Value::Null)
                && line.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
            {
                let text =
                    crate::mcp_command::assistant_text(line.get("message")).unwrap_or_default();
                let cancelled_local = machine
                    .foreground_target()
                    .is_some_and(|uuid| is_local_command_turn(ext, uuid))
                    && machine.is_cancelled();
                let take = take_local_command_markdown(machine, ext, &text);
                if cancelled_local {
                    return;
                }
                match take {
                    // Defensive: the frame was held while the producer ran, so
                    // a `Pending` here means it could not be; map the CLI text
                    // like a non-local frame rather than drop it.
                    LocalCommandTake::Pending => {}
                    LocalCommandTake::Suppress => return,
                    LocalCommandTake::Replace(markdown) => {
                        publish_local_command_chunk(
                            &markdown,
                            crate::mcp_command::message_id_of(&line),
                            update_tx,
                            session_id,
                            emitted_assistant_text,
                            ext,
                        );
                        return;
                    }
                    // Not a local-command turn (or its producer failed): the
                    // synthetic frame maps like any consolidated assistant one.
                    LocalCommandTake::Keep => {}
                }
            }
            // V-04: the CLI's own "Please run /login" reply fails the turn as
            // `auth_required` (`acp-agent.ts:7968-7975`); the client owns the login
            // UI, so the text never renders in the chat.
            if frame_type == "assistant"
                && line
                    .get("message")
                    .is_some_and(frames::is_synthetic_login_message)
            {
                let title = line
                    .get("message")
                    .and_then(crate::session_failure::assistant_message_text)
                    .unwrap_or_default();
                // `failActiveWithSessionFailure("auth_required")` interrupts the
                // compaction of the turn it fails (`acp-agent.ts:5565`).
                if machine.has_active() {
                    let updates = ext.t2a.compaction.interrupt();
                    publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
                }
                let events = machine.fail_active_auth_required(&title);
                if machine.take_text_stretch_closed() {
                    *emitted_assistant_text = false;
                }
                settle_events(
                    events,
                    tasks,
                    map_state,
                    live_background,
                    pending,
                    update_tx,
                    session_id,
                    high_water,
                    &ext.failure_ctx,
                    ext.air.as_ref(),
                    None,
                    &mut ext.titles,
                    &mut ext.t2a.failures,
                    &mut ext.t2a.usage_turns,
                    &mut ext.file_change_reports,
                );
                return;
            }
            // Finding 8: AIR receives this provider condition on the terminal prompt
            // response as a typed failure whose title is the exact assistant
            // error text captured above. Do not duplicate that text as an
            // ordinary assistant message (acp-agent.ts:7980-7989).
            if frame_type == "assistant"
                && line.get("parent_tool_use_id") == Some(&Value::Null)
                && ext
                    .air
                    .as_ref()
                    .is_some_and(|a| a.supports("sessionFailure"))
            {
                let has_error = line.get("error").is_some();
                let is_limit = line
                    .get("message")
                    .map(crate::session_failure::is_synthetic_usage_limit_message)
                    .unwrap_or(false);
                if has_error || is_limit {
                    return;
                }
            }
            // S-06 / S-07 (`acp-agent.ts:7898-7947`): a string-content live
            // frame can carry a local command's output. Strip its markers and
            // map the rest; a marker-only stdout frame is logged, and a stderr
            // frame is logged as an error and never published.
            if let Some(text) = line
                .pointer("/message/content")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                if text.contains("<local-command-stdout>") {
                    match crate::transcript::strip_local_command_metadata(Value::String(
                        text.clone(),
                    )) {
                        Some(Value::String(stripped)) => {
                            if let Some(message) =
                                line.get_mut("message").and_then(Value::as_object_mut)
                            {
                                message.insert("content".to_string(), Value::String(stripped));
                            }
                        }
                        _ => {
                            if let Some(logger) = &ext.logger {
                                logger.log(&text);
                            }
                            return;
                        }
                    }
                } else if text.contains("<local-command-stderr>") {
                    if let Some(logger) = &ext.logger {
                        logger.error(&text);
                    }
                    return;
                }
            }
            let updates = if frame_type == "stream_event" {
                map::map_stream_event(&line, map_state)
            } else {
                // Consolidated assistant message: forward only the text/thinking
                // its stream did not already deliver (`unstreamedRemainder`,
                // Decision 18), plus tool/image/plan blocks.
                map::map_consolidated_assistant(&line, map_state)
            };
            // `sendUpdate` (`acp-agent.ts:4605-4618`): a top-level text chunk
            // that repeats a failed compaction's error, which the lifecycle
            // already carried, is consumed instead of published.
            let updates: Vec<SessionUpdate> = if is_strict_top_level {
                updates
                    .into_iter()
                    .filter(|update| match update {
                        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                            ContentBlock::Text(text) => !ext
                                .t2a
                                .compaction
                                .consume_duplicate_error_output(&text.text),
                            _ => true,
                        },
                        _ => true,
                    })
                    .collect()
            } else {
                updates
            };
            for update in &updates {
                if is_top_level_text(update, &line) {
                    *emitted_assistant_text = true;
                    if let SessionUpdate::AgentMessageChunk(chunk) = update {
                        if let ContentBlock::Text(text) = &chunk.content {
                            ext.titles.on_assistant_text(&text.text);
                        }
                    }
                }
            }
            // 3.7: a subagent's output (`parent_tool_use_id` set) is not this
            // turn's foreground work.
            let top_level = line.get("parent_tool_use_id").is_none_or(Value::is_null);
            note_tool_updates(&updates, top_level, machine.foreground_target(), map_state);
            let filtered = map_state.filter_updates(updates);
            send_updates(
                tasks,
                map_state,
                filtered,
                update_tx,
                session_id,
                machine.foreground_target(),
            );
        }
        // N-20: a tool progress beat updates the call it reports on
        // (`acp-agent.ts:8125-8176`).
        "tool_progress" => {
            let air = map_state.is_air();
            if let Some(beat) = frames::tool_progress_update(&line, map_state, live_background, air)
            {
                let filtered = map_state.filter_updates(vec![beat]);
                send_updates(tasks, map_state, filtered, update_tx, session_id, None);
            }
        }
        // N-44 (`acp-agent.ts:8191-8213`): the SDK switched to a fresh
        // conversation, whose Task* ids, task store and transcript are
        // independent of the previous one. Everything keyed to the old one goes
        // before any follow-up prompt can republish stale state.
        "conversation_reset" => {
            // `finishLifecycle("failed", ...)` starts with
            // `compaction.interrupt()` (`acp-agent.ts:8191-8195`): the old
            // conversation's open entity is closed `cancelled` and its pending
            // hook summary dropped before the reset clears the rest.
            let updates = ext.t2a.compaction.interrupt();
            publish_compaction_updates(ext, map_state, updates, update_tx, session_id);
            // `finishLifecycle("failed", ...)`: live async tasks end as failed.
            finish_async_tasks(
                tasks,
                map_state,
                "failed",
                update_tx,
                session_id,
                ext.logger.as_deref(),
            );
            tasks.clear();
            map_state.clear_resolved_tool_names();
            map_state.clear_eager_sessions();
            map_state.clear_tool_tracker();
            ext.hooks.clear();
            ext.task_state.clear();
            publish_task_plan(session_id, ext, update_tx, None);
            // A reset mounts a fresh transcript, so the cached title no longer
            // describes the session: re-evaluate at the next turn end.
            ext.titles.reset();
            // The follow-up prompt was predicted from the transcript the reset
            // dropped, so it no longer follows from anything
            // (`acp-agent.ts:8214`).
            publish_prompt_suggestion(ext.air.is_some(), &mut ext.t2a, None, update_tx, session_id);
        }
        "prompt_suggestion" => {
            // The CLI predicts the next prompt after the turn's `result`
            // (`acp-agent.ts:8216-8227`). A prediction the user outran — the
            // next prompt was sent before it was read off the stream — answers
            // a turn that is already settled, so it is dropped.
            if !ext.t2a.prompt_suggestion_expected {
                return;
            }
            ext.t2a.prompt_suggestion_expected = false;
            let text = crate::prompt_suggestions::prompt_suggestion_text(
                line.get("suggestion").and_then(Value::as_str),
            );
            publish_prompt_suggestion(ext.air.is_some(), &mut ext.t2a, text, update_tx, session_id);
        }
        _ => {}
    }
}

/// Node `isAutoUnavailable` (`session-mode.ts:324-330`): a listed current model
/// that does not advertise Auto. An unlisted model is not downgraded.
fn model_lacks_auto(config: &crate::session_config::SessionConfigState) -> bool {
    config
        .model_infos
        .iter()
        .find(|m| m.value == config.current_model_id)
        .is_some_and(|m| m.supports_auto_mode != Some(true))
}

/// The client's answer to a pending permission request, as resolved by the
/// actor (the actor owns the live model and mode state).
struct PermissionAnswer<'a> {
    value: &'a Value,
    ctx: &'a permission_options::OptionsContext,
    request_id: &'a str,
    tool_use_id: &'a str,
    tool_input: &'a Value,
}

/// Resolve the client's permission answer the way Node does after
/// `requestPermissionFromClient` (`acp-agent.ts:10219-10237`): evaluate
/// `isAutoUnavailable` against the CURRENT model, apply
/// `applyPermissionFallback` (`session-mode.ts:196-221`), publish the advisory
/// warning for ANY rewrite (once per session), publish the ExitPlanMode mode
/// updates, and return the `control_response` for the CLI.
fn resolve_permission_answer(
    answer: PermissionAnswer<'_>,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) -> Value {
    let mut ctx = answer.ctx.clone();
    ctx.auto_unavailable = model_lacks_auto(&ext.config);
    let (response, fallback) = permission::permission_control_response_with_fallback(
        answer.value,
        &ctx,
        answer.request_id,
        answer.tool_use_id,
        answer.tool_input,
    );
    // A decoded answer that threw (non-selected, unoffered or unknown id) is the
    // SDK's error verdict: Node never reaches the fallback, the ExitPlanMode
    // mode publishing or the pending lanes, so neither does this path.
    if crate::permission_response::is_error_control_response(&response) {
        return response;
    }
    // `publishFallbackWarning` (`session-mode.ts:287-308`) runs whenever the
    // rewrite happened, for any tool, and only once per session.
    if fallback && !ext.auto_mode_fallback_warning_shown {
        ext.auto_mode_fallback_warning_shown = true;
        let _ = update_tx.send(SessionOutbound::RawUpdate(auto_mode_fallback_update(
            session_id,
            ext.supports_notices(),
        )));
    }
    // ExitPlanMode mode effect (Node `acp-agent.js:7665-7677`): an allow with a
    // session `setMode` update publishes current_mode_update +
    // config_option_update to the client (no CLI control request).
    if ctx.tool_name == "ExitPlanMode" && response["response"]["response"]["behavior"] == "allow" {
        if let Some(perms) = response["response"]["response"]["updatedPermissions"].as_array() {
            if let Some(mode) = perms.iter().find_map(|u| {
                if u.get("type").and_then(Value::as_str) == Some("setMode")
                    && u.get("destination").and_then(Value::as_str) == Some("session")
                {
                    u.get("mode").and_then(Value::as_str)
                } else {
                    None
                }
            }) {
                // `updateConfigOption` -> `syncConfig` also tracks `prePlanMode`.
                exit_plan::track_pre_plan_mode(
                    &mut ext.config.pre_plan_mode,
                    &ext.config.modes.current_mode_id,
                    mode,
                );
                ext.config.modes.current_mode_id = mode.to_string();
                for opt in &mut ext.config.config_options {
                    if opt.id == crate::session_config::MODE_CONFIG_ID {
                        opt.current_value = mode.to_string();
                    }
                }
                let mode_sent = update_tx
                    .send(SessionOutbound::RawUpdate(json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "current_mode_update",
                            "currentModeId": mode,
                        }
                    })))
                    .is_ok();
                let config_sent = update_tx
                    .send(SessionOutbound::RawUpdate(json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "config_option_update",
                            "configOptions": ext.config.config_options,
                        }
                    })))
                    .is_ok();
                // Node catches a client failure while publishing the approval's
                // mode and keeps the approval (`acp-agent.ts:10226-10232`). The
                // Rust client channel cannot throw, but a closed one is the same
                // failure: log it and still return the allow.
                if !mode_sent || !config_sent {
                    if let Some(logger) = &ext.logger {
                        logger.error(
                            "Failed to publish mode after plan approval: the session update channel is closed",
                        );
                    }
                }
            }
        }
    }
    // The pending ExitPlanMode lanes (`acp-agent.ts:10238-10260`): a clear-context
    // choice records the reset (in the mode the model can actually run), and a
    // deny that interrupts records the interruption its tool result will carry.
    if ctx.tool_name == "ExitPlanMode" {
        let behavior = &response["response"]["response"];
        let reset_mode = permission::selected_option_id(answer.value)
            .and_then(permission_options::exit_plan_clear_context_mode)
            .map(|mode| {
                if mode == "auto" && ctx.auto_unavailable {
                    "acceptEdits"
                } else {
                    mode
                }
            });
        let plan = answer
            .tool_input
            .get("plan")
            .and_then(Value::as_str)
            .unwrap_or("");
        ext.t2a.exit_plan.note_answer(
            answer.tool_use_id,
            reset_mode.map(|mode| (plan, mode)),
            behavior["behavior"] == "deny" && behavior["interrupt"] == true,
        );
    }
    response
}

/// The result is the interruption of an ExitPlanMode answer: tell the machine
/// what it ends and spend both markers (`acp-agent.ts:7110-7137`,
/// `7219-7238`). A clear-context approval whose reset belongs to the
/// interrupted call, with a turn to carry, hands that turn on (the reset is
/// returned for the restart); a reset with no turn to carry ends nothing.
fn note_exit_plan_interruption(
    machine: &mut TurnMachine,
    state: &mut exit_plan::ExitPlanState,
) -> Option<exit_plan::ContextReset> {
    let matches = state.reset_matches_interruption();
    let reset = state.context_reset.take().filter(|_| matches);
    state.interruption = None;
    match reset {
        Some(reset) if machine.has_active() => {
            machine.note_exit_plan_interruption(crate::turn::ExitPlanVerdict::ContextReset);
            Some(reset)
        }
        Some(_) => None,
        None => {
            machine.note_exit_plan_interruption(crate::turn::ExitPlanVerdict::KeepPlanning);
            None
        }
    }
}

/// Handle an inbound `can_use_tool` control_request (10.1–10.5).
///
/// Emits the `tool_call` if the client has not seen it yet (`ensureToolCallEmitted`,
/// INV-20 / #851), then spawns a task (D14) that sends `session/request_permission`
/// and hands the client's answer to the actor (`Command::PermissionAnswered`),
/// which maps it to the `control_response` (10.3 / 10.4) and resolves `reply`.
/// The actor never awaits the client round-trip inline (INV-27).
#[allow(clippy::too_many_arguments)]
fn handle_permission(
    tasks: &mut AsyncTaskRuntime,
    frame: Value,
    reply: oneshot::Sender<Value>,
    turn: Option<&str>,
    map_state: &mut MapState,
    live_background: &mut HashMap<String, String>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    pending_permissions: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    pending_await_tokens: &mut HashMap<String, crate::native_subagents::AwaitToken>,
    pending_ask: &mut HashMap<String, (oneshot::Sender<Value>, Value)>,
    elicit_cancel: &watch::Receiver<bool>,
    elicitation_form: bool,
    cwd: &std::path::Path,
    logger: Option<&crate::logger::Logger>,
    available_modes: &[String],
    pre_plan_mode: Option<&str>,
    home_dir: Option<&std::path::Path>,
    context_used_percent: Option<u32>,
    command_tx: &mpsc::UnboundedSender<Command>,
) {
    let can = permission::parse_can_use_tool(&frame);
    // Subagent attribution (10.5): the parent Agent/Task tool call that spawned
    // the subagent (`liveBackgroundTasks`).
    let parent = can
        .agent_id
        .as_deref()
        .and_then(|id| live_background.get(id).cloned());
    // U-68 (`acp-agent.ts:10077-10090`): the attribution rests on
    // `task_started.task_id === canUseTool`'s agentID. When the consumer lost
    // the race with `task_started`, the lookup misses and the request goes out
    // unattributed; log it so the miss is observable rather than silent.
    if let (Some(agent_id), None, Some(logger)) = (can.agent_id.as_deref(), &parent, logger) {
        logger.log(&format!(
            "[claude-agent-acp] No parent tool_use recorded for subagent {agent_id}; \
             sending the {} permission request unattributed",
            can.tool_name
        ));
    }
    let request_id = can.request_id.clone();

    // ensureToolCallEmitted (10.2, INV-20): surface the tool_call before the
    // permission request references it, unless the streamed tool_use already did.
    // T-40/T-106: a `diffPatch` client sees the exact patch of an Edit or Write
    // in the approval (`previewPatchContent`); the surfaced tool call carries it
    // too and keeps it against the streamed snippet diff until the final result.
    let preview = map_state.approval_preview(&can.tool_name, &can.tool_input);
    let mut surfaced = permission::ensure_tool_call_emitted(
        &can.tool_name,
        &can.tool_input,
        &can.tool_use_id,
        parent.as_deref(),
        map_state,
        Some(cwd),
    );
    if let Some(preview) = &preview {
        map_state.pin_approval_preview(&can.tool_use_id, surfaced.as_mut(), preview);
    }
    // `toolUseOwner` (`acp-agent.ts:9943-9956`): the session of an eagerly
    // emitted tool call and of its requests is the subagent's announced child
    // session, else the root. The RFD form only announces a child it can place.
    let permission_session_id = can
        .agent_id
        .as_deref()
        .filter(|_| tasks.subagents().form().is_some())
        .and_then(|agent_id| tasks.subagents().child(agent_id))
        .filter(|child| child.announced)
        .map(|child| child.session_id.clone())
        .unwrap_or_else(|| session_id.to_string());
    if let Some(update) = surfaced {
        note_permission_surfaced(&update, &can.tool_name, parent.is_none(), turn, map_state);
        // `ensureToolCallEmitted` records the session the call surfaced in
        // (`acp-agent.ts:9906`), and sends the call there with
        // `client.sessionUpdate` — NOT through `sendUpdate`, so the routing and
        // its foreground bookkeeping do not run for it
        // (`acp-agent.ts:9916`).
        map_state.record_eager_session(&can.tool_use_id, &permission_session_id);
        // The field tracker (and the AIR changed-`_meta` filter) records what the
        // client now holds, so the later streamed refinement and the result do
        // not repeat it.
        for update in map_state.filter_updates(vec![update]) {
            let notif = SessionNotification::new(permission_session_id.clone(), update);
            let value = serde_json::to_value(&notif).unwrap_or(Value::Null);
            let _ = send_notification(value, update_tx);
        }
    }

    // F23a/U-42 (`acp-agent.ts:10093-10112`): AskUserQuestion reaches `canUseTool`
    // as a permission check whenever a callback is registered. A form-capable
    // client gets it as an ACP form elicitation whose answers become the tool's
    // `updatedInput`; it must never reach `build_options`, which refuses it by
    // design (P-58).
    if can.tool_name == "AskUserQuestion" {
        if !elicitation_form {
            // Unreachable with the CLI's `--disallowedTools` gate (O-135); deny
            // rather than let the permission builder panic on model input.
            let _ = reply.send(ask_deny_response(
                &can.request_id,
                &can.tool_use_id,
                permission::DENY_MESSAGE,
            ));
            return;
        }
        let Some(questions) = crate::elicitation::extract_ask_user_questions(&can.tool_input)
        else {
            let _ = reply.send(ask_deny_response(
                &can.request_id,
                &can.tool_use_id,
                "AskUserQuestion called with no valid questions.",
            ));
            return;
        };
        // `withChildAwaitingUser`: an announced RFD child is `requires_action`
        // until the question is answered.
        let (await_token, awaiting_outs) = tasks
            .subagents()
            .awaiting_user_begin(&permission_session_id);
        publish_subagent_outs(tasks, map_state, awaiting_outs, update_tx, session_id, turn);
        if let Some(token) = await_token {
            pending_await_tokens.insert(request_id.clone(), token);
        }
        let create_request = crate::elicitation::ask_user_questions_to_create_request(
            &questions,
            &permission_session_id,
            Some(&can.tool_use_id),
            map_state.is_air(),
        );
        let aborted = ask_aborted_response(&can.request_id);
        pending_ask.insert(request_id.clone(), (reply, aborted));
        // B-60: a question blocks the foreground work like a permission
        // request (`withPendingUserInput`, `acp-agent.ts:10276-10290`).
        map_state
            .v2
            .open_input(request_id.clone(), can.agent_id.clone());
        drain_v2(map_state, update_tx, session_id);
        let update_tx = update_tx.clone();
        let command_tx = command_tx.clone();
        let mut cancel = elicit_cancel.clone();
        let request_id_for_task = can.request_id.clone();
        let tool_use_id = can.tool_use_id.clone();
        let tool_input = can.tool_input.clone();
        let handle = tokio::spawn(async move {
            let (ptx, prx) = oneshot::channel();
            let _ = update_tx.send(SessionOutbound::CreateElicitation {
                params: create_request,
                reply: ptx,
            });
            // Abortable by `session/cancel` / the session close (INV-27): the
            // actor aborts this task and answers the pending request with the
            // CANCEL outcome itself, so this task's late answer is ignored.
            let first = tokio::select! {
                biased;
                () = crate::elicitation::wait_cancelled(&mut cancel) => None,
                res = prx => Some(res),
            };
            let response = match first {
                None => ask_aborted_response(&request_id_for_task),
                Some(Ok(Ok(resp))) => {
                    if *cancel.borrow() {
                        ask_aborted_response(&request_id_for_task)
                    } else {
                        match crate::elicitation::apply_ask_elicitation_response(
                            &resp,
                            &tool_input,
                            &questions,
                        ) {
                            crate::elicitation::AskUserQuestionOutcome::Answered {
                                updated_input,
                            } => ask_allow_response(
                                &request_id_for_task,
                                &tool_use_id,
                                &updated_input,
                            ),
                            crate::elicitation::AskUserQuestionOutcome::Cancel => {
                                ask_aborted_response(&request_id_for_task)
                            }
                        }
                    }
                }
                // A client error denies; a cancel that raced it still aborts.
                Some(Ok(Err(_))) => {
                    if *cancel.borrow() {
                        ask_aborted_response(&request_id_for_task)
                    } else {
                        ask_deny_response(
                            &request_id_for_task,
                            &tool_use_id,
                            "Could not present the question to the user.",
                        )
                    }
                }
                // The forwarding side dropped the reply (the session is gone).
                Some(Err(_)) => ask_aborted_response(&request_id_for_task),
            };
            let _ = command_tx.send(Command::AskUserQuestionAnswered {
                request_id: request_id_for_task,
                response,
            });
        });
        pending_permissions.insert(request_id, handle);
        return;
    }

    // Build the `request_permission` params (10.1) before moving `can` into the
    // spawned round-trip task.
    //
    // `buildClaudePermissionPresentation` (`permissions/presentation.ts:60-136`):
    // the tool call's own heading is reused as the permission heading, a sandbox
    // network request or a computer-use call is named by its subject instead, and
    // a client that is not AIR also sees the JSON input when the call has no
    // content of its own. AIR gets the minimal tool call; a `diffPatch` client's
    // approval carries the exact patch (P-16).
    let subject = crate::tools::permission_subject(
        &can.tool_name,
        &can.tool_input,
        can.display_name.as_deref(),
    );
    let is_computer_use = can.tool_name.starts_with("mcp__computer-use__");
    let fallback_content = if subject.is_some() || is_computer_use {
        let input = serde_json::to_string_pretty(&can.tool_input).unwrap_or_default();
        Some(json!([{
            "type": "content",
            "content": {"type": "text", "text": format!("```json\n{input}\n```")},
        }]))
    } else {
        None
    };
    let tool_call = permission::build_permission_tool_call(&permission::PermissionToolCall {
        tool_name: can.tool_name.clone(),
        tool_input: can.tool_input.clone(),
        tool_use_id: can.tool_use_id.clone(),
        parent_tool_use_id: parent.clone(),
        cwd: Some(cwd.to_path_buf()),
        air: map_state.is_air(),
        plan_file: map_state.plan_file(),
        terminal_output: map_state.has_terminal(&can.tool_name),
        title: subject,
        preview_content: preview.map(Value::Array),
        blocked_path: can.blocked_path.clone(),
        fallback_content,
        mcp_server: can.mcp_server.clone(),
    });
    // 3.6 (Decision 7): build the ACP options from the P7 permission_option
    // module (Node's `buildClaudePermissionOptions`), and the request params.
    let ctx = permission_options::OptionsContext {
        tool_name: can.tool_name.clone(),
        display_name: can.display_name.clone(),
        input: can.tool_input.clone(),
        cwd: cwd.to_path_buf(),
        change_set: permission_options::normalize_change_set(
            &can.suggestions,
            can.no_persistent_rule,
        ),
        no_persistent_rule: can.no_persistent_rule,
        default_to_no: can.default_to_no,
        available_modes: available_modes.to_vec(),
        // Placeholder: the actor sets this from the live model when the answer
        // arrives (`resolve_permission_answer`).
        auto_unavailable: false,
        pre_plan_mode: pre_plan_mode.map(String::from),
        context_used_percent,
        home_dir: home_dir.map(|p| p.to_path_buf()),
    };
    let options = permission_options::build_options(&ctx);
    let mut params = serde_json::Map::new();
    params.insert("options".into(), Value::Array(options));
    // `sessionId: permissionSessionId` (`acp-agent.ts:10199`): the request goes
    // to the session that holds the tool call.
    params.insert(
        "sessionId".into(),
        Value::String(permission_session_id.clone()),
    );
    // Only AIR gets the permission presentation (`_meta.jetbrains.air.permission`).
    if map_state.is_air() {
        params.insert(
            "_meta".into(),
            crate::tools::air_permission_meta(
                &can.tool_name,
                &can.tool_input,
                can.display_name.as_deref(),
                tool_call["title"].as_str().unwrap_or_default(),
                frame
                    .pointer("/request/decision_reason")
                    .and_then(Value::as_str),
                can.default_to_no,
            ),
        );
    }
    // P-51: a v2 permission request has its own `title` and `description`
    // (`AcpPermissionRequest`, `permissions/presentation.ts:12-17`), computed
    // once from the tool call (`v2/permission.ts:12-25`).
    if map_state.v2.reporting() {
        let (title, description) = crate::v2::permission::v2_permission_presentation(
            &can.tool_name,
            &can.tool_input,
            can.display_name.as_deref(),
            tool_call["title"].as_str().unwrap_or_default(),
            frame
                .pointer("/request/decision_reason")
                .and_then(Value::as_str),
        );
        params.insert("title".into(), Value::String(title));
        if let Some(description) = description {
            params.insert("description".into(), Value::String(description));
        }
    }
    params.insert("toolCall".into(), tool_call);
    let params = Value::Object(params);

    // `withChildAwaitingUser` (`acp-agent.ts:9562-9572`): a request of an
    // announced RFD child makes that child `requires_action` until its last open
    // request ends. The root session (and every AIR child) has no such state.
    let (await_token, awaiting_outs) = tasks
        .subagents()
        .awaiting_user_begin(&permission_session_id);
    publish_subagent_outs(tasks, map_state, awaiting_outs, update_tx, session_id, turn);
    if let Some(token) = await_token {
        pending_await_tokens.insert(request_id.clone(), token);
    }

    // Spawn the round-trip (D14): ask the agent layer to send
    // `session/request_permission` and await the client's answer. The answer is
    // handed to the actor (`Command::PermissionAnswered`), which resolves the
    // `control_response` (the `control.rs` on-request task writes the frame).
    // The command carries `reply`, so a `Cancel` that aborts this task after the
    // client answered still delivers the allow (INV-27); the handle is registered
    // so a Cancel before the answer aborts the round-trip (CUJ 2).
    // B-60/NEW-6: the request blocks the foreground work from the moment it is
    // open (`withPendingUserInput`, `v2/agent.ts:338-349`), and its
    // `requires_action` must precede the request on the wire, so the state is
    // drained here — before the round-trip task sends it.
    map_state
        .v2
        .open_input(request_id.clone(), can.agent_id.clone());
    drain_v2(map_state, update_tx, session_id);
    let ctx = Arc::new(ctx);
    let command_tx = command_tx.clone();
    // F3 (`review-T6G2`): the request is queued HERE, synchronously, before the
    // round-trip task is spawned, so no update the actor sends after the spawn
    // can overtake it (upstream sends `requires_action` and the request
    // synchronously, `v2/agent.ts:338-357`). Only the client's answer is awaited
    // in the task.
    let (ptx, prx) = oneshot::channel();
    let _ = update_tx.send(SessionOutbound::RequestPermission { params, reply: ptx });
    let handle = tokio::spawn(async move {
        match prx.await {
            Ok(Ok(value)) => {
                let _ = command_tx.send(Command::PermissionAnswered {
                    value,
                    ctx,
                    request_id: can.request_id.clone(),
                    tool_use_id: can.tool_use_id.clone(),
                    tool_input: can.tool_input.clone(),
                    reply,
                });
            }
            // The request errored or the round-trip was dropped (e.g. the client
            // closed / cancelled). Node's `requestPermissionFromClient` throws
            // "Tool use aborted" when the signal aborted and rethrows the client
            // error otherwise (`acp-agent.ts:9790-9836`); the SDK writes the
            // throw as an error `control_response`, never a deny (P-36).
            Ok(Err(message)) => {
                let _ = reply.send(crate::permission_response::error_control_response(
                    &can.request_id,
                    &message,
                ));
                let _ = command_tx.send(Command::AwaitEnded {
                    request_id: can.request_id.clone(),
                });
            }
            Err(_) => {
                let _ = reply.send(crate::permission_response::error_control_response(
                    &can.request_id,
                    crate::permission_response::TOOL_USE_ABORTED,
                ));
                let _ = command_tx.send(Command::AwaitEnded {
                    request_id: can.request_id.clone(),
                });
            }
        }
    });
    pending_permissions.insert(request_id, handle);
}

/// Process a batch of [`TurnEvent`]s in order (8.8 / review-04 wiring):
/// `FinalText` is emitted as an `agent_message_chunk` before the same batch's
/// `Settled` resolves the prompt.
///
/// `failures` is the session's failure book, so a terminal AIR failure takes
/// its revision from the same controller as an `api_retry` warning
/// (`createSessionFailure` -> `prepare`, `acp-agent.ts:4676-4690`).
#[allow(clippy::too_many_arguments)]
fn handle_events(
    events: &[TurnEvent],
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
    failure_ctx: &crate::session_failure::AssistantFailureCtx,
    air: Option<&AirClient>,
    final_message_id: Option<&str>,
    failures: &mut failures::SessionFailures,
    v2: &mut crate::turn_events::V2State,
) {
    // A `FinalText` precedes its turn's `Settled` in the same batch; collect it
    // and hand it to the prompt task so it can forward the chunk before the
    // response (deterministic, review-04 wiring). A v2 prompt has no prompt
    // task: it is answered at intake, so its texts go out on the session's
    // ordered channel instead (NEW-1, `acp-agent.ts:5301`).
    let mut final_texts: Vec<SessionNotification> = Vec::new();
    for event in events {
        match event {
            TurnEvent::FinalText { text } => {
                // D11 (Decision 10): Node stamps `messageIdForGrouping(result)`
                // = the result frame's `uuid` on the result-text chunk
                // (`acp-agent.ts:5301`); the result line is in scope at the
                // `"result"` arm, which passes it down here.
                let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                if let Some(id) = final_message_id.filter(|id| !id.is_empty()) {
                    chunk = chunk.message_id(MessageId::new(id.to_string()));
                }
                let update = SessionUpdate::AgentMessageChunk(chunk);
                let notif = SessionNotification::new(session_id.to_string(), update);
                final_texts.push(notif);
            }
            // U-13: the refusal explanation (`acp-agent.ts:7254-7263`), grouped
            // under the refusing result like the fallback text, but sent in order
            // with the result's usage update that precedes it.
            TurnEvent::RefusalText { text } => {
                let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                if let Some(id) = final_message_id.filter(|id| !id.is_empty()) {
                    chunk = chunk.message_id(MessageId::new(id.to_string()));
                }
                let update = SessionUpdate::AgentMessageChunk(chunk);
                let _ = update_tx.send(SessionOutbound::Update(SessionNotification::new(
                    session_id.to_string(),
                    update,
                )));
            }
            TurnEvent::Settled {
                prompt_uuid,
                stop_reason,
                usage,
                had_usage,
                model_rows,
            } => {
                high_water.fetch_max(1, Ordering::Relaxed);
                if let Some(reply) = pending.remove(prompt_uuid) {
                    // Capture the raw usage values before `usage` is shadowed
                    // below (2.12 also needs them for `_meta.quota`).
                    let (u_i, u_o, u_cr, u_cw) = (
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cached_read_tokens,
                        usage.cached_write_tokens,
                    );
                    let total_tokens = u_i + u_o + u_cr + u_cw;
                    // Phase 15 (Item 2): upstream distinguishes the two cancel
                    // settlements by WHICH CODE PATH produced them, not by
                    // whether the accumulated usage happens to be zero. A turn
                    // swept from the queue by `cancel()` never ran, so it
                    // reports no `usage` (`turn.resolve({ stopReason:
                    // "cancelled" })`); an ACTIVE or held turn's cancel always
                    // reports `usage` via `settleActive({ ..., usage:
                    // sessionUsage(session) })` — present even when genuinely
                    // all-zero. `had_usage` carries that origin from `turn.rs`;
                    // only the pure queued-sweep is false.
                    let usage = if !had_usage {
                        None
                    } else {
                        Some(PromptUsage {
                            input_tokens: u_i,
                            output_tokens: u_o,
                            cached_read_tokens: u_cr,
                            cached_write_tokens: u_cw,
                            total_tokens,
                        })
                    };
                    // 2.12 (E-6): `_meta.quota` totals on every settle that
                    // reported usage (never on the queued-sweep cancel), with
                    // the per-model rows snapshotted by the turn machine
                    // (`model_usage`, rust-air-gaps Decision 19).
                    let quota = if *had_usage {
                        Some(json!({
                            "quota": {
                                "token_count": {
                                    "totalTokens": total_tokens,
                                    "inputTokens": u_i,
                                    "cachedInputTokens": u_cr,
                                    "cachedWriteTokens": u_cw,
                                    "outputTokens": u_o,
                                    "reasoningOutputTokens": 0,
                                },
                                "model_usage": model_rows.to_json(),
                            }
                        }))
                    } else {
                        None
                    };
                    let (flush_tx, flush_rx) = oneshot::channel();
                    let (ack_tx, ack_rx) = oneshot::channel();
                    // Phase 9 flush barrier: enqueued after every update this
                    // turn emitted, so the forwarding task writes them all
                    // before it resolves `flush_rx`; the prompt task awaits it
                    // before responding, keeping a `tool_call` ahead of the
                    // `result` on the wire. The ack keeps the responses of one
                    // settle batch in settle order.
                    let _ = update_tx.send(SessionOutbound::FlushAcked {
                        done: flush_tx,
                        ack: ack_rx,
                    });
                    let _ = reply.send(Ok(PromptReply {
                        stop_reason: stop_reason.as_str().to_string(),
                        usage,
                        quota,
                        updates: std::mem::take(&mut final_texts),
                        flush_rx: Some(flush_rx),
                        ack: Some(ack_tx),
                    }));
                } else if v2.reporting() {
                    // NEW-1: a v2 prompt was answered at intake, so it has no
                    // `pending` entry; the result-only text (#453) and a
                    // local-only command's output still reach the client as a
                    // chunk (`sendUpdate`, `acp-agent.ts:5301`). It goes out
                    // here, before the turn's `idle` that `after_events` queues
                    // next, so the client sees the text ahead of the state.
                    for notif in std::mem::take(&mut final_texts) {
                        let _ = update_tx.send(SessionOutbound::Update(notif));
                    }
                }
            }
            TurnEvent::Failed {
                prompt_uuid,
                kind,
                message,
                usage,
                model_rows,
                is_structured_output_retries,
            } => {
                let (claude_kind, title) = match kind {
                    FailureKind::ProviderError if *is_structured_output_retries => (
                        crate::session_failure::ClaudeFailureKind::ProviderError,
                        message.clone(),
                    ),
                    FailureKind::ProviderError => {
                        let ck = crate::session_failure::provider_failure_category(
                            failure_ctx.error.as_deref(),
                            failure_ctx.was_usage_limit,
                        );
                        let t = failure_ctx.title.clone().unwrap_or_else(|| message.clone());
                        (ck, t)
                    }
                    FailureKind::BudgetExhausted => (
                        crate::session_failure::ClaudeFailureKind::BudgetExhausted,
                        message.clone(),
                    ),
                    FailureKind::ContextExhausted => (
                        crate::session_failure::ClaudeFailureKind::ContextExhausted,
                        message.clone(),
                    ),
                    FailureKind::IncompleteToolCall | FailureKind::NoResult => (
                        crate::session_failure::ClaudeFailureKind::InternalError,
                        message.clone(),
                    ),
                    FailureKind::AuthRequired => (
                        crate::session_failure::ClaudeFailureKind::AuthRequired,
                        message.clone(),
                    ),
                    // No title: the policy's fallback title speaks
                    // (`failActiveWithSessionFailure("transport_lost")`).
                    FailureKind::TransportLost => (
                        crate::session_failure::ClaudeFailureKind::TransportLost,
                        String::new(),
                    ),
                    FailureKind::WorkerShutdown => (
                        crate::session_failure::ClaudeFailureKind::WorkerShutdown,
                        String::new(),
                    ),
                    _ => (
                        crate::session_failure::ClaudeFailureKind::InternalError,
                        message.clone(),
                    ),
                };

                let air_session_failure =
                    air.as_ref().is_some_and(|a| a.supports("sessionFailure"));
                // Finding 3: assistant `error` mapping to auth_required (authentication_failed /
                // oauth_org_not_allowed) must reject with RequestError.authRequired() -32000 for
                // ALL clients (AIR included). Check the derived claude_kind, not only *kind.
                let auth = claude_kind == crate::session_failure::ClaudeFailureKind::AuthRequired
                    || *kind == FailureKind::AuthRequired;
                // NEW-5: the ONE mapping of a failed turn. The error v1
                // answers the prompt with is also the error a v2 client
                // reads on its `idle` (`jsonRpcError`, `v2/prompt.ts:114-126`),
                // so it is built once here and handed to the v2 state; the
                // copy that re-derived `(kind, message)` is gone.
                let err = failed_turn_error(
                    *kind,
                    message,
                    auth,
                    air_session_failure,
                    failure_ctx.has_error(),
                );
                if v2.reporting() {
                    // F1: the effective kind travels with the error, so the v2
                    // bookkeeping respawns a session whose assistant error said
                    // auth. F5: AIR `sessionFailure` settles the turn end_turn
                    // (no error), as v1 does.
                    let settle_end_turn =
                        air_session_failure && *kind != FailureKind::SessionEnded && !auth;
                    v2.set_failure_rpc(
                        prompt_uuid,
                        (!settle_end_turn).then(|| crate::agent::session_error_jsonrpc(&err)),
                        if auth {
                            FailureKind::AuthRequired
                        } else {
                            *kind
                        },
                    );
                }

                if let Some(reply) = pending.remove(prompt_uuid) {
                    if auth {
                        reply_error_after_flush(update_tx, reply, err);
                    } else if air_session_failure && *kind != FailureKind::SessionEnded {
                        // The one failure controller: an `api_retry` warning of
                        // this turn already holds revision 1 of `<turn>:error`
                        // (`createSessionFailure` -> `prepare` -> `recordActive`).
                        // The answer ends the turn with a failure: the session's list
                        // row shows `error` once idle (`failActiveWithSessionFailure`,
                        // `acp-agent.ts:5558-5562`).
                        let _ =
                            update_tx.send(SessionOutbound::Board(BoardEvent::TurnFailed(true)));
                        let sf_meta = record_air_session_failure(
                            failures,
                            session_id,
                            prompt_uuid,
                            claude_kind,
                            &title,
                        );
                        let (u_i, u_o, u_cr, u_cw) = (
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cached_read_tokens,
                            usage.cached_write_tokens,
                        );
                        let total_tokens = u_i + u_o + u_cr + u_cw;
                        let mut meta_obj = serde_json::Map::new();
                        meta_obj.insert(
                            "quota".to_string(),
                            json!({
                                "token_count": {
                                    "totalTokens": total_tokens,
                                    "inputTokens": u_i,
                                    "cachedInputTokens": u_cr,
                                    "cachedWriteTokens": u_cw,
                                    "outputTokens": u_o,
                                    "reasoningOutputTokens": 0,
                                },
                                "model_usage": model_rows.to_json(),
                            }),
                        );
                        if let Some(jb) = sf_meta.get("jetbrains") {
                            meta_obj.insert("jetbrains".to_string(), jb.clone());
                        }

                        let prompt_usage = PromptUsage {
                            input_tokens: u_i,
                            output_tokens: u_o,
                            cached_read_tokens: u_cr,
                            cached_write_tokens: u_cw,
                            total_tokens,
                        };

                        let (flush_tx, flush_rx) = oneshot::channel();
                        let (ack_tx, ack_rx) = oneshot::channel();
                        let _ = update_tx.send(SessionOutbound::FlushAcked {
                            done: flush_tx,
                            ack: ack_rx,
                        });
                        let _ = reply.send(Ok(PromptReply {
                            stop_reason: "end_turn".to_string(),
                            usage: Some(prompt_usage),
                            quota: Some(Value::Object(meta_obj)),
                            updates: std::mem::take(&mut final_texts),
                            flush_rx: Some(flush_rx),
                            ack: Some(ack_tx),
                        }));
                    } else {
                        reply_error_after_flush(update_tx, reply, err);
                    }
                } else if v2.reporting() {
                    // H2: a v2 prompt has no parked v1 reply, but its AIR
                    // `sessionFailure` still records the failure (the list row
                    // shows `error`) and hands `sessionFailureMeta` to the
                    // `idle` `after_events` queues next
                    // (`failActiveWithSessionFailure`, `acp-agent.ts:5591-5593`).
                    if air_session_failure && *kind != FailureKind::SessionEnded && !auth {
                        let _ =
                            update_tx.send(SessionOutbound::Board(BoardEvent::TurnFailed(true)));
                        let sf_meta = record_air_session_failure(
                            failures,
                            session_id,
                            prompt_uuid,
                            claude_kind,
                            &title,
                        );
                        v2.set_failure_meta(prompt_uuid, sf_meta);
                    }
                    // NEW-1: a v2 prompt is answered at intake, so its
                    // result text still goes out, ahead of the `idle error`
                    // that `after_events` queues next.
                    for notif in std::mem::take(&mut final_texts) {
                        let _ = update_tx.send(SessionOutbound::Update(notif));
                    }
                }
            }
            TurnEvent::Activated { .. } => {
                high_water.fetch_max(1, Ordering::Relaxed);
            }
            // The foreground work of a held turn ended at its result, and a
            // resumed hold starts it again: v2's state only, no prompt answer.
            TurnEvent::Held { .. } | TurnEvent::Resumed { .. } => {}
            // Consumed by the unfinished-tool gate before this point.
            TurnEvent::CycleEnded => {}
        }
    }
}

/// How long an error reply waits for the forwarder to reach its flush barrier.
/// Longer than the forwarder's own bounded ack wait on a settled turn's
/// response (1 s), so a rejection still follows the responses ahead of it. The
/// bound only matters when nothing serves the outbound channel (the forwarder
/// is stuck or a harness holds the receiver unread): the reply is then released
/// instead of waiting for ever.
const ERROR_FLUSH_BOUND: std::time::Duration = std::time::Duration::from_millis(1500);

/// Send `err` to the prompt once every update queued ahead of it has been
/// forwarded: Node awaits each `sendUpdate` before `turn.reject`, so the turn's
/// last updates are on the wire before the error. Released at once when the
/// outbound channel is gone, and after [`ERROR_FLUSH_BOUND`] when it is not served.
///
/// Node settles the turns of a batch, and the batches themselves, in order. The
/// barrier is the same acknowledged one a successful response goes through
/// ([`SessionOutbound::FlushAcked`]): the forwarding task releases it only after
/// the previous response was written, and waits for this error's own
/// acknowledgement ([`PromptFailure::ack`]) before releasing the next. Every
/// response of the session is therefore ordered by the forwarder alone, with no
/// timing involved.
fn reply_error_after_flush(
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    reply: oneshot::Sender<Result<PromptReply, PromptFailure>>,
    err: SessionError,
) {
    // No runtime to wait on (a synchronous caller): nothing can serve the barrier.
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        let _ = reply.send(Err(err.into()));
        return;
    };
    let (flush_tx, flush_rx) = oneshot::channel();
    let (ack_tx, ack_rx) = oneshot::channel();
    if update_tx
        .send(SessionOutbound::FlushAcked {
            done: flush_tx,
            ack: ack_rx,
        })
        .is_err()
    {
        let _ = reply.send(Err(err.into()));
        return;
    }
    runtime.spawn(async move {
        let _ = tokio::time::timeout(ERROR_FLUSH_BOUND, flush_rx).await;
        let _ = reply.send(Err(PromptFailure {
            error: err,
            ack: Some(ack_tx),
        }));
    });
}

/// The SDK's `lastErrorResultText` / `errorTextBeforeModelLessTurns` /
/// `modelLessTurnEnded` bookkeeping (`sdk.mjs` `readMessages`), fed with every
/// frame the CLI writes. A `result` sets the text, every other session frame
/// except `system/session_state_changed` clears it, so a death after the next
/// turn started streaming is a plain process death again.
#[derive(Default)]
struct ErrorTextTracker {
    last_error_result_text: Option<String>,
    error_text_before_model_less_turns: Option<String>,
    model_less_turn_ended: bool,
}

impl ErrorTextTracker {
    fn observe(&mut self, line: &Value) {
        let kind = line.get("type").and_then(Value::as_str).unwrap_or("");
        let subtype = line.get("subtype").and_then(Value::as_str).unwrap_or("");
        match kind {
            // The SDK consumes these before the bookkeeping.
            "control_response"
            | "control_request"
            | "control_cancel_request"
            | "keep_alive"
            | "transcript_mirror"
            | "active_goal"
            | "autocompact_state" => {}
            "system"
                if matches!(
                    subtype,
                    "post_turn_summary" | "task_summary" | "session_metadata"
                ) => {}
            "result" => {
                self.last_error_result_text = error_result_text(line);
                self.model_less_turn_ended = subtype == "success"
                    && line.get("is_error").and_then(Value::as_bool) != Some(true)
                    && line.get("num_turns").and_then(Value::as_i64) == Some(0)
                    && line.get("result").and_then(Value::as_str) == Some("");
                if !self.model_less_turn_ended {
                    self.error_text_before_model_less_turns = self.last_error_result_text.clone();
                }
            }
            "system" if subtype == "session_state_changed" => {}
            _ => {
                self.last_error_result_text = None;
                if kind == "system" && subtype == "init" {
                    self.model_less_turn_ended = false;
                } else if kind != "system" || subtype == "compact_boundary" {
                    self.error_text_before_model_less_turns = None;
                    self.model_less_turn_ended = false;
                }
            }
        }
    }

    /// The text that replaces the exit error (`lastErrorResultText ??
    /// (modelLessTurnEnded ? errorTextBeforeModelLessTurns : undefined)`).
    fn exit_error_text(&self) -> Option<String> {
        self.last_error_result_text.clone().or_else(|| {
            self.model_less_turn_ended
                .then(|| self.error_text_before_model_less_turns.clone())
                .flatten()
        })
    }
}

/// The SDK's `lastErrorResultText` for one `result` frame (`sdk.mjs`
/// `readMessages`): the `result` text for a `success` subtype, else the trimmed
/// non-empty `errors` joined with `"; "`; `None` for a non-error result or an
/// empty diagnostic.
fn error_result_text(result: &Value) -> Option<String> {
    if result.get("is_error").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let subtype = result.get("subtype").and_then(Value::as_str).unwrap_or("");
    let text = if subtype == "success" {
        result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        result
            .get("errors")
            .and_then(Value::as_array)
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default()
    };
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Whether an emitted update is top-level (non-subagent) assistant text — used
/// to set `emitted_assistant_text` (subagent chunks with `parent_tool_use_id`
/// are excluded).
/// Whether `value` is a non-empty string (`!!message.subagent_type`).
fn is_js_truthy_str(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
}

/// The listed fields of a frame, as the object Node's handler builds for the
/// async-task runtime: only what it passes on (`task_notification` passes no
/// `output_file` of a `task_progress`, say).
fn pick_fields(line: &Value, keys: &[&str]) -> Value {
    let mut picked = serde_json::Map::new();
    for key in keys {
        if let Some(value) = line.get(*key) {
            picked.insert((*key).to_string(), value.clone());
        }
    }
    Value::Object(picked)
}

/// `isLateTaskRun` for a `task_*` frame (`acp-agent.ts:469-487`), with Node's
/// log line when the frame is ignored.
fn is_late_task_frame(line: &Value, kind: &str, ext: &mut SessionExt) -> bool {
    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
    let run_id = line.get("run_id").and_then(Value::as_str);
    let late = ext.t2a.task_runs.is_late(task_id, run_id);
    if late {
        if let Some(logger) = &ext.logger {
            logger.log(&format!(
                "Ignoring {kind} for task {task_id} from earlier run {}",
                run_id.unwrap_or_default()
            ));
        }
    }
    late
}

/// The subagent that the Agent call `parent` started streams no more text under
/// it: forget its streamed record and drop latch.
fn forget_subagent_streams(parent: &str, machine: &mut TurnMachine, map_state: &mut MapState) {
    map_state.forget_stream_record(parent);
    machine.forget_dropped_stream(parent);
}

/// End the display terminal of a command that went to the background once its
/// task ends (`endBackgroundTerminal`, `acp-agent.ts:5112-5136`). Claude Code
/// reports no exit code for it, and `completed` does not mean 0: Claude Code
/// accepts some non-zero codes as a success, such as grep's 1.
fn end_background_terminal(
    task_id: &str,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let Some(terminal) = ext.t2a.background_terminals.remove(task_id) else {
        return;
    };
    let mut meta = serde_json::Map::new();
    if let Some(parent) = &terminal.parent_tool_use_id {
        meta.insert("claudeCode".into(), json!({ "parentToolUseId": parent }));
    }
    meta.insert(
        "terminal_exit".into(),
        json!({ "terminal_id": terminal.tool_call_id, "exit_code": null, "signal": null }),
    );
    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "tool_call_update",
            "toolCallId": terminal.tool_call_id,
            "_meta": Value::Object(meta),
        },
    })));
}

/// A task whose tool call ran a tool the client never sees tasks of (`Monitor`)
/// is ignored when its spawn is due: the tool name comes from the tool-use cache,
/// else from what a resolved result kept (`toolNameOf`, `acp-agent.ts:4538`).
fn note_hidden_tool_call(line: &Value, tasks: &mut AsyncTaskRuntime, map_state: &MapState) {
    let Some(id) = line
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let name = map_state
        .tool_use_from_cache(id)
        .and_then(|tool_use| tool_use.get("name"))
        .and_then(Value::as_str)
        .or_else(|| map_state.resolved_tool_name(id));
    if name == Some("Monitor") {
        tasks.hide_tool_call(id);
    }
}

/// Deliver the results that the work a cancel stopped sends for tool calls the
/// client holds open (`sendInterruptedToolResults`, `acp-agent.ts:5336-5369`).
/// Claude Code's interrupt ends each running tool of the main conversation with
/// an error result, before the result that ends the turn; the tool call gets it
/// like any other, so its final status and `nonExecutionKind` reach the client.
/// Nothing else of the message is applied: the other effects of a tool result
/// (async tasks, plan state, the ExitPlanMode flow) belong to the stopped work,
/// which stays dropped.
fn send_interrupted_tool_results(
    tasks: &mut AsyncTaskRuntime,
    line: &Value,
    map_state: &mut MapState,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    turn: Option<&str>,
) {
    let Some(blocks) = line.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    let is_result =
        |block: &&Value| block.get("type").and_then(Value::as_str) == Some("tool_result");
    let results: Vec<Value> = blocks
        .iter()
        .filter(is_result)
        .filter(|block| {
            block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_some_and(|id| map_state.has_emitted(id))
        })
        .cloned()
        .collect();
    if results.is_empty() {
        return;
    }
    let result_count = blocks.iter().filter(is_result).count();
    let mut filtered = line.clone();
    filtered["message"]["content"] = Value::Array(results.clone());
    // `tool_use_result` describes the one result of its message.
    if results.len() != result_count {
        if let Some(object) = filtered.as_object_mut() {
            object.remove("tool_use_result");
        }
    }
    // `options.cancelled` (`acp-agent.ts:5370-5380`): these results belong to
    // the work the cancel stopped, so an error result is an aborted run on v2.
    let updates = map::map_user_frame_cancelled(&filtered, map_state, true);
    let top_level = line.get("parent_tool_use_id").is_none_or(Value::is_null);
    note_tool_updates(&updates, top_level, turn, map_state);
    let filtered = map_state.filter_updates(updates);
    send_updates(tasks, map_state, filtered, update_tx, session_id, turn);
}

/// `closeStoppedSubagentToolCalls` (`acp-agent.ts:5320-5326`): the SDK reported
/// the subagent of the Agent call `agent_tool_use_id` stopped, after a cancel
/// stopped it and the work that waited on it ended. Nothing else ends its calls.
fn close_stopped_subagent_calls(
    tasks: &mut AsyncTaskRuntime,
    agent_tool_use_id: Option<&str>,
    machine: &TurnMachine,
    map_state: &mut MapState,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let Some(agent_tool_use_id) = agent_tool_use_id else {
        return;
    };
    let closing = crate::unfinished::close_stopped_subagent_tool_calls(
        map_state,
        machine.work_end_reported(),
        agent_tool_use_id,
    );
    send_closing(tasks, map_state, closing, update_tx, session_id);
}

/// Send the closing reports of the unfinished-tool check.
///
/// A v2 client gets `cancelled` for a call the turn stopped (`stopped`) and
/// `failed` for one Claude ran without a result (C-24,
/// `AcpToolCallRenderer.unfinished`); v1 has no `cancelled` status, so every
/// other client keeps the v1 `failed` report (with the AIR `_meta` filter).
pub(crate) fn send_closing(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    closing: Vec<crate::unfinished::ClosingReport>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let mut typed = Vec::new();
    for report in closing {
        if map_state.v2.reporting() {
            send_update(
                tasks,
                map_state,
                json!({ "sessionId": session_id, "update": report.wire_json(true) }),
                None,
                update_tx,
                session_id,
                None,
            );
        } else {
            typed.push(report.update);
        }
    }
    if !typed.is_empty() {
        let filtered = map_state.filter_updates(typed);
        send_updates(tasks, map_state, filtered, update_tx, session_id, None);
    }
}

/// `settleLiveTask`: the task is no longer live; a subagent stays resumable. The
/// attribution map follows the machine's live set.
fn settle_live_task(
    task_id: &str,
    machine: &mut TurnMachine,
    live_background: &mut HashMap<String, String>,
    map_state: &mut MapState,
) -> Vec<TurnEvent> {
    let events = machine.on_task_ended(task_id);
    sync_live_tasks(machine, live_background, map_state);
    events
}

/// Refresh what follows the machine's live task set: the Agent/Task call of
/// every live subagent (attribution of its requests and reports) and the calls of
/// the background subagents that still run (what a cancel leaves open).
fn sync_live_tasks(
    machine: &TurnMachine,
    live_background: &mut HashMap<String, String>,
    map_state: &mut MapState,
) {
    *live_background = machine.live_subagent_parents();
    map_state.set_background_subagents(machine.background_subagent_parents());
}

/// An activation sweeps the level-ended task entries out of the machine
/// (`settleLiveBackgroundTask` in `activateTurn`, `acp-agent.ts:4823-4831`):
/// the attribution map and the gate's background exemption follow it.
fn sync_after_activation(
    events: &[TurnEvent],
    machine: &TurnMachine,
    live_background: &mut HashMap<String, String>,
    map_state: &mut MapState,
) {
    if events
        .iter()
        .any(|event| matches!(event, TurnEvent::Activated { .. }))
    {
        sync_live_tasks(machine, live_background, map_state);
    }
}

/// A raw `agent_message_chunk` the adapter itself published (a notice's
/// transcript line): the client was given assistant text, so the delivery
/// record closes over it and the title context reads it (`sendUpdate`,
/// `acp-agent.ts:4615-4622`).
fn note_transcript_line(update: &Value, emitted_assistant_text: &mut bool, ext: &mut SessionExt) {
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("agent_message_chunk") {
        return;
    }
    *emitted_assistant_text = true;
    if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
        ext.titles.on_assistant_text(text);
    }
}

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

/// 3.7: a permission-surfaced top-level call is foreground work, except
/// `SandboxNetworkAccess`: a permission-only callback whose synthetic id never
/// gets a `tool_result` (`acp-agent.ts:9889-9903`). It stays a dispatched call
/// (recorded by `ensure_tool_call_emitted`).
fn note_permission_surfaced(
    update: &SessionUpdate,
    tool_name: &str,
    top_level: bool,
    turn: Option<&str>,
    map_state: &mut MapState,
) {
    note_tool_updates(
        std::slice::from_ref(update),
        top_level && tool_name != "SandboxNetworkAccess",
        turn,
        map_state,
    );
}

/// Keep the unfinished-tool books (3.7) in step with the updates just mapped:
/// a surfaced top-level `tool_call` is foreground work of the current turn
/// (`recordForegroundToolCall`); a final-status `tool_call_update`, or a
/// subagent's call, no longer is (`forgetForegroundToolCall`).
fn note_tool_updates(
    updates: &[SessionUpdate],
    top_level: bool,
    turn: Option<&str>,
    map_state: &mut MapState,
) {
    for update in updates {
        match update {
            SessionUpdate::ToolCall(call) => {
                let id = call.tool_call_id.to_string();
                if top_level {
                    map_state.books.record_foreground(turn, &id);
                } else {
                    // A subagent's call is not the turn's foreground work; Claude
                    // Code still dispatched it (`forgetForegroundToolCall`).
                    map_state.books.forget_foreground(&id);
                }
            }
            SessionUpdate::ToolCallUpdate(upd)
                if matches!(
                    upd.fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                ) =>
            {
                map_state.books.forget(&upd.tool_call_id.to_string());
            }
            // A later stream frame can attribute an eager permission call to a
            // child: it no longer counts as this turn's foreground work
            // (`acp-agent.ts:4592-4604`).
            SessionUpdate::ToolCallUpdate(upd) if !top_level => {
                map_state
                    .books
                    .forget_foreground(&upd.tool_call_id.to_string());
            }
            _ => {}
        }
    }
}

/// Run the unfinished-tool post-filter (3.7, Decision 7) over a batch of turn
/// events, send its closing `tool_call_update`s, then hand the (possibly
/// One report job a settled turn left to run (see [`FileChangeReports::take`]).
pub(crate) struct DeferredReportJob {
    pub request_id: String,
    pub handle: crate::file_change_audit::ReportHandle,
    pub edited: Vec<String>,
    /// Set by a cancel/close that finished the report: the forwarded reply then
    /// reports `cancelled` (`review-T6INT` finding 2).
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by the job when it finished, so the actor can prune the entry.
    pub done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A settled turn's prompt reply parked until its file-change report published:
/// the proxy receiver, the real reply sender, the prompt uuid (the checkpoint's
/// user message id) and the report job.
struct DeferredReport {
    rx: oneshot::Receiver<Result<PromptReply, PromptFailure>>,
    reply: oneshot::Sender<Result<PromptReply, PromptFailure>>,
    prompt_uuid: String,
    job: DeferredReportJob,
}

/// rewritten) events to [`handle_events`].
#[allow(clippy::too_many_arguments)]
fn settle_events(
    events: Vec<TurnEvent>,
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    live_background: &HashMap<String, String>,
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
    failure_ctx: &crate::session_failure::AssistantFailureCtx,
    air: Option<&AirClient>,
    final_message_id: Option<&str>,
    titles: &mut crate::session_titles::SessionTitles,
    failures: &mut failures::SessionFailures,
    usage_turns: &mut crate::usage::UsageTurns,
    reports: &mut FileChangeReports,
) -> bool {
    // Node's `activateTurn` clears `session.cancelled`, ending the stopped-work
    // window whenever a turn activates (whether by user echo or ensure_active_turn).
    // The turn it hands off from settles first and closes its tool calls under the
    // window, so the window ends after the gate.
    let activated = events
        .iter()
        .any(|e| matches!(e, TurnEvent::Activated { .. }));
    let background: std::collections::HashSet<&str> =
        live_background.values().map(String::as_str).collect();
    let (events, closing) = crate::unfinished::gate_events(events, map_state, &background);
    // A turn the gate converted to `IncompleteToolCall` went through
    // `failActiveWithSessionFailure`, which interrupts the compaction
    // (`acp-agent.ts:5469`, `5565`): the caller drops a pending summary.
    let incomplete_tool_call = events.iter().any(|e| {
        matches!(
            e,
            TurnEvent::Failed {
                kind: FailureKind::IncompleteToolCall,
                ..
            }
        )
    });
    send_closing(tasks, map_state, closing, update_tx, session_id);
    if activated {
        map_state.end_stopped_work();
    }
    // Node collects every top-level `agent_message_chunk` in `sendUpdate`
    // (`acp-agent.ts:4618-4622`): the result-text fallback (`FinalText`) is one
    // of them, so it feeds the title context too.
    for event in &events {
        if let TurnEvent::FinalText { text } | TurnEvent::RefusalText { text } = event {
            titles.on_assistant_text(text);
        }
    }
    // A settled turn's `/usage` producer is spent, whatever settled it (a
    // result, a cancel, an idle-abandon or an echo hand-off): drop the entry so
    // the map cannot grow for the session's lifetime.
    for event in &events {
        if let TurnEvent::Settled { prompt_uuid, .. } = event {
            usage_turns.remove(prompt_uuid);
        }
    }
    // AG-3: `settleActive` publishes the turn's file-change report before the
    // prompt response (`acp-agent.ts:5486-5493`). A cancelled or failed turn's
    // `unavailable` report is fire-and-forget (`finish`, `acp-agent.ts:2805`,
    // `2811`); a reported turn's prompt reply is parked until the bounded
    // checkpoint preview has published, then forwarded after a flush barrier so
    // the `session_info_update` precedes the response.
    let mut deferred_reports: Vec<DeferredReport> = Vec::new();
    // review-T6INT finding 3: a v2 prompt has no reply to park, but its report
    // still has to publish before the turn's `idle`.
    let mut v2_reports: Vec<(String, DeferredReportJob)> = Vec::new();
    for event in &events {
        let TurnEvent::Settled {
            prompt_uuid,
            stop_reason,
            ..
        } = event
        else {
            continue;
        };
        let Some(job) = reports.take(prompt_uuid) else {
            continue;
        };
        if stop_reason.as_str() == "cancelled" {
            // M8: the job is finished, so mark it done or `note_prompt`'s
            // prune keeps it in `collecting` for the session's lifetime.
            reports.finish_job(&job, "cancelled");
            continue;
        }
        if let Some(reply) = pending.remove(prompt_uuid) {
            let (tx, rx) = oneshot::channel();
            pending.insert(prompt_uuid.clone(), tx);
            deferred_reports.push(DeferredReport {
                rx,
                reply,
                prompt_uuid: prompt_uuid.clone(),
                job,
            });
        } else if map_state.v2.reporting() {
            v2_reports.push((prompt_uuid.clone(), job));
        } else {
            job.done.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    for event in &events {
        if let TurnEvent::Failed { prompt_uuid, .. } = event {
            if let Some(job) = reports.take(prompt_uuid) {
                // M8: finished, so `note_prompt`'s prune can drop the entry.
                reports.finish_job(&job, "providerError");
            }
        }
    }
    handle_events(
        &events,
        pending,
        update_tx,
        session_id,
        high_water,
        failure_ctx,
        air,
        final_message_id,
        failures,
        &mut map_state.v2,
    );
    if !deferred_reports.is_empty() {
        let workspace = reports.reporter.as_ref().map(|r| r.workspace().clone());
        let timeout = reports
            .reporter
            .as_ref()
            .map_or(crate::file_change_audit::REPORT_TIMEOUT, |r| r.timeout());
        let control = reports.control.clone();
        let publish = reports.publish();
        let log_error = reports.log_error();
        let update_tx = update_tx.clone();
        for deferred in deferred_reports {
            let workspace = workspace.clone();
            let control = control.clone();
            let publish = publish.clone();
            let log_error = log_error.clone();
            let update_tx = update_tx.clone();
            tokio::spawn(async move {
                let DeferredReport {
                    rx,
                    reply,
                    prompt_uuid,
                    job,
                } = deferred;
                let answer = rx.await;
                let Ok(mut answer) = answer else {
                    job.done.store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                };
                // `review-T6INT` finding 1: release the stale barrier
                // `handle_events` queued for this reply first. The forwarder is
                // blocked on its ack, and the report's own barrier would queue
                // behind it: without this the turn's response stalls for the
                // 1 s ack timeout.
                if let Ok(ok) = &mut answer {
                    if let Some(ack) = ok.ack.take() {
                        let _ = ack.send(());
                    }
                }
                if let (Some(workspace), Some(control)) = (workspace, control) {
                    let user_message_id = prompt_uuid.clone();
                    let preview = async move {
                        control
                            .send_request(json!({
                                "subtype": "rewind_files",
                                "user_message_id": user_message_id,
                                "dry_run": true,
                            }))
                            .await
                            .map(|frame| {
                                // `Query.rewindFiles` resolves the response's own
                                // `response` body (`RewindFilesResult`).
                                frame
                                    .get("response")
                                    .and_then(|response| response.get("response"))
                                    .cloned()
                                    .unwrap_or(Value::Null)
                            })
                            .map_err(|error| error.to_string())
                    };
                    crate::file_change_audit::run_report(
                        crate::file_change_audit::ReportJob {
                            workspace,
                            handle: job.handle.clone(),
                            request_id: job.request_id.clone(),
                            edited_paths: job.edited.clone(),
                            publish,
                            log_error,
                            timeout,
                        },
                        preview,
                    )
                    .await;
                    // A fresh barrier for the report: the prompt task awaits it
                    // and the forwarder is never blocked by this task, so the
                    // report precedes the response without a second 1 s stall.
                    let (done_tx, done_rx) = oneshot::channel();
                    let (ack_tx, ack_rx) = oneshot::channel();
                    let _ = update_tx.send(SessionOutbound::FlushAcked {
                        done: done_tx,
                        ack: ack_rx,
                    });
                    if let Ok(ok) = &mut answer {
                        // A cancel that landed during the preview wins: its
                        // `unavailable/cancelled` report already went out and
                        // the turn answers `cancelled` (`review-T6INT` finding 2).
                        if job.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                            ok.stop_reason = "cancelled".to_string();
                        }
                        ok.flush_rx = Some(done_rx);
                        ok.ack = Some(ack_tx);
                    }
                }
                job.done.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = reply.send(answer);
            });
        }
    }
    // F39: the v2 states of the gated events, after the gate closed the tool
    // calls they report and after the v1 responses were ordered, so the client
    // sees the work's output before its state.
    map_state.v2.after_events(&events, failure_ctx.has_error());
    if v2_reports.is_empty() {
        drain_v2(map_state, update_tx, session_id);
    } else {
        // review-T6INT finding 3: the report publishes before the turn's
        // `idle`. The batch's v2 emissions are held until every report job of
        // the batch has run (Node awaits the preview inside `settleActive`).
        let mut emissions = map_state.v2.take_out();
        let workspace = reports.reporter.as_ref().map(|r| r.workspace().clone());
        let timeout = reports
            .reporter
            .as_ref()
            .map_or(crate::file_change_audit::REPORT_TIMEOUT, |r| r.timeout());
        let control = reports.control.clone();
        let publish = reports.publish();
        let log_error = reports.log_error();
        let update_tx = update_tx.clone();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            if let (Some(workspace), Some(control)) = (workspace, control) {
                for (prompt_uuid, job) in v2_reports {
                    let preview_uuid = prompt_uuid.clone();
                    let preview = {
                        let control = control.clone();
                        async move {
                            control
                                .send_request(json!({
                                    "subtype": "rewind_files",
                                    "user_message_id": preview_uuid,
                                    "dry_run": true,
                                }))
                                .await
                                .map(|frame| {
                                    frame
                                        .get("response")
                                        .and_then(|response| response.get("response"))
                                        .cloned()
                                        .unwrap_or(Value::Null)
                                })
                                .map_err(|error| error.to_string())
                        }
                    };
                    crate::file_change_audit::run_report(
                        crate::file_change_audit::ReportJob {
                            workspace: workspace.clone(),
                            handle: job.handle.clone(),
                            request_id: job.request_id.clone(),
                            edited_paths: job.edited.clone(),
                            publish: publish.clone(),
                            log_error: log_error.clone(),
                            timeout,
                        },
                        preview,
                    )
                    .await;
                    // H1 (`review-T6INT` finding 2): a cancel that landed
                    // during the preview wins the turn, as `cancel()` settles
                    // the settling turn `cancelled` (`acp-agent.ts:8627-8685`).
                    // Its `unavailable/cancelled` report already went out; the
                    // held idle must not still say `end_turn`.
                    if job.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                        for emission in emissions.iter_mut() {
                            if let crate::turn_events::V2Emission::Ended {
                                prompt_uuid: ended,
                                outcome,
                            } = emission
                            {
                                if *ended == prompt_uuid {
                                    outcome.stop_reason = crate::turn::StopReason::Cancelled;
                                }
                            }
                        }
                    }
                    job.done.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            } else {
                for (_, job) in v2_reports {
                    job.done.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            emit_v2_emissions(emissions, &update_tx, &session_id);
        });
    }
    incomplete_tool_call
}

/// Send the v2 messages the session owes its client (F39), in order: the
/// prompt's `user_message`, the `state_update`s of its foreground work, and the
/// intake answers (after every update ahead of them, like upstream's `send`
/// then `resolve`).
fn drain_v2(
    map_state: &mut MapState,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let emissions = map_state.v2.take_out();
    emit_v2_emissions(emissions, update_tx, session_id);
}

fn emit_v2_emissions(
    emissions: Vec<crate::turn_events::V2Emission>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    for emission in emissions {
        match emission {
            crate::turn_events::V2Emission::Inserted {
                message_id,
                content,
            } => {
                let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "user_message",
                        "messageId": message_id,
                        "content": content,
                    }
                })));
            }
            crate::turn_events::V2Emission::Foreground(event) => {
                let update = crate::v2::prompt::state_update(&event);
                let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                    "sessionId": session_id,
                    "update": update,
                })));
            }
            crate::turn_events::V2Emission::Ended { outcome, .. } => {
                let update = crate::v2::prompt::state_update(
                    &crate::turn_events::ForegroundEvent::Ended(outcome),
                );
                let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                    "sessionId": session_id,
                    "update": update,
                })));
            }
            crate::turn_events::V2Emission::Answer {
                prompt_uuid,
                reply,
                result,
            } => {
                if let Some(reply) = reply {
                    let _ = update_tx.send(SessionOutbound::V2Intake {
                        prompt_uuid,
                        reply,
                        result,
                    });
                }
            }
        }
    }
}

/// Emit a `commands_changed` / `system/init` command list as an
/// `AvailableCommandsUpdate`, filtered by the latched terminal-bound names. An
/// AIR client also gets the `kind`/`skillPath` meta of its skill commands
/// (`acp-agent.ts:6278-6284`).
fn emit_commands_changed(
    commands: &Value,
    terminal: &[String],
    ext: &SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let air_cwd = ext.air.as_ref().map(|_| ext.cwd.as_path());
    let home = ext.env.get_path("HOME");
    let update =
        map::map_commands_changed_with_skills(commands, terminal, air_cwd, home.as_deref());
    // Decision 8: raw JSON so a command without an argument hint carries
    // `input: null` like Node (the typed update would omit the field).
    emit_raw_updates(
        vec![map::available_commands_json(&update)],
        update_tx,
        session_id,
    );
}

/// Apply a `get_context_usage` answer to the session's window (D1 / Decision
/// 1): only while the window is not authoritative AND the session is still on
/// the model the request was issued for (Node's `stillCurrent`,
/// `acp-agent.ts:7876-7880`). Returns whether the window changed.
fn apply_context_window_answer(
    usage: &mut crate::usage::UsageState,
    current_model_id: &str,
    size: u64,
    answer_model_id: &str,
) -> bool {
    if crate::session_model::context_window_answer_is_current(
        usage.authoritative,
        current_model_id,
        answer_model_id,
    ) {
        usage.update_window_if_positive(Some(size))
    } else {
        false
    }
}

/// The error text of a `control_response` whose `response.subtype` is `error`
/// (the CLI refused the request), else `None`.
fn control_response_error(receipt: &Value) -> Option<String> {
    let response = receipt.get("response")?;
    if response.get("subtype").and_then(Value::as_str) != Some("error") {
        return None;
    }
    Some(
        response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("control request failed")
            .to_string(),
    )
}

/// The error text of a rejected `set_permission_mode`: keep the CLI's
/// non-empty message, else `Invalid Mode` (`session-mode.ts:254-263`).
fn permission_mode_error(err: &crate::control::ControlError) -> String {
    match err {
        crate::control::ControlError::Remote(message) if !message.is_empty() => message.clone(),
        _ => "Invalid Mode".to_string(),
    }
}

/// Send a batch of untyped AIR async-task updates (item 3.4/3.5), each wrapped
/// as `{"sessionId":…,"update":{…}}` in a `RawUpdate` (item 3.3).
fn emit_raw_updates(
    updates: Vec<Value>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    for update in updates {
        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
            "sessionId": session_id,
            "update": update,
        })));
    }
}

/// `client.sessionUpdate`: a value the native subagent runtime publishes as it
/// is (`publish`, `native-subagents.ts:170`).
fn subagent_publish(
    notification: Value,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
) -> Result<(), String> {
    update_tx
        .send(SessionOutbound::RawUpdate(notification))
        .map_err(|error| error.to_string())
}

/// `isFinalToolCallStatus` (`tool-calls/renderer.ts:40-42`): a status that ends
/// the call.
fn is_final_tool_call_status(status: Option<&str>) -> bool {
    matches!(status, Some("completed" | "failed" | "cancelled"))
}

/// `sendUpdate`'s `toolCallId` (`acp-agent.ts:4555-4558`): only a tool call or
/// tool call update has one.
fn notification_tool_call_id(notification: &Value) -> Option<&str> {
    let update = notification.get("update")?;
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("tool_call" | "tool_call_update") => update.get("toolCallId").and_then(Value::as_str),
        _ => None,
    }
}

/// The `_meta.claudeCode.parentToolUseId` of a notification: the Agent/Task
/// call that started the subagent the update belongs to.
fn notification_parent_tool_use_id(notification: &Value) -> Option<&str> {
    notification
        .pointer("/update/_meta/claudeCode/parentToolUseId")
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty())
}

/// The session's sends a `user` frame's block mapping needs: the async-task /
/// native subagent runtime, the update channel and the session id.
struct SendIo<'a> {
    tasks: &'a mut AsyncTaskRuntime,
    update_tx: &'a mpsc::UnboundedSender<SessionOutbound>,
    session_id: &'a str,
}

/// Send one notification the session's `sendUpdate` produced. A notification
/// that still fits the typed `SessionUpdate` goes through the typed channel
/// (the forwarder materialises the ACP tool-call defaults); anything else (the
/// runtime's own AIR/RFD shapes) goes raw with the same materialisation.
fn send_notification(
    notification: Value,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
) -> Result<(), String> {
    if let Ok(typed) = serde_json::from_value::<SessionNotification>(notification.clone()) {
        return update_tx
            .send(SessionOutbound::Update(typed))
            .map_err(|error| error.to_string());
    }
    let mut value = notification;
    if let Some(update) = value.get_mut("update") {
        crate::map::materialize_tool_call_defaults(update);
        crate::map::materialize_diff_old_text(update);
    }
    update_tx
        .send(SessionOutbound::RawUpdate(value))
        .map_err(|error| error.to_string())
}

/// How a routed notification is sent (`sendUpdate`'s tail): a typed update goes
/// through the typed channel; a raw value the caller already had stays verbatim;
/// a value the runtime built is materialised as raw.
enum RoutedPayload {
    Typed(Box<SessionUpdate>),
    /// A raw JSON value the session received as such (a hook update): keep it
    /// verbatim, so explicit nulls and unknown keys survive.
    VerbatimRaw,
    /// A value the native subagent runtime built: materialise the ACP defaults.
    RuntimeRaw,
}

/// The post-routing tail of `sendUpdate` (`acp-agent.ts:4576-4633`): the
/// tool-call bookkeeping and the send. `routed` is what the runtime returned
/// (`None` never reaches here); it is not routed again.
fn deliver_routed(
    map_state: &mut MapState,
    routed: Value,
    payload: RoutedPayload,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    turn: Option<&str>,
) -> Result<(), String> {
    let kind = routed
        .pointer("/update/sessionUpdate")
        .and_then(Value::as_str);
    let status = routed.pointer("/update/status").and_then(Value::as_str);
    let final_status = kind == Some("tool_call_update") && is_final_tool_call_status(status);
    let tool_call_id = notification_tool_call_id(&routed).map(ToString::to_string);
    let parent = notification_parent_tool_use_id(&routed);
    let moved = routed.get("sessionId").and_then(Value::as_str) != Some(session_id);
    if let Some(id) = &tool_call_id {
        if parent.is_some() || moved || final_status {
            // A later stream frame can attribute an eager permission call to a
            // child. It must no longer count as this turn's foreground work.
            map_state.books.forget_foreground(id);
        } else if kind == Some("tool_call") && map_state.has_emitted(id) {
            map_state.books.record_foreground(turn, id);
        }
    }
    // A typed update that the route did not change (it only moved the
    // notification to another session) goes through the typed channel, so the
    // forwarder materialises it exactly as before the routing hook.
    let unchanged = match &payload {
        RoutedPayload::Typed(update) => {
            serde_json::to_value(update.as_ref()).ok() == routed.get("update").cloned()
        }
        RoutedPayload::VerbatimRaw | RoutedPayload::RuntimeRaw => false,
    };
    let sent = match payload {
        RoutedPayload::Typed(update) if unchanged => {
            let routed_session = routed
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or(session_id)
                .to_string();
            update_tx
                .send(SessionOutbound::Update(SessionNotification::new(
                    routed_session,
                    *update,
                )))
                .map_err(|error| error.to_string())
        }
        // A value the caller already had as raw JSON stays verbatim (nulls and
        // unknown keys included), as before the routing hook.
        RoutedPayload::VerbatimRaw => update_tx
            .send(SessionOutbound::RawUpdate(routed))
            .map_err(|error| error.to_string()),
        RoutedPayload::Typed(_) | RoutedPayload::RuntimeRaw => send_notification(routed, update_tx),
    };
    if let Some(id) = tool_call_id.filter(|_| final_status) {
        // The call ended: its eager session and dispatched record go.
        map_state.forget_eager_session(&id);
        map_state.books.forget(&id);
    }
    sent
}

/// The session's `sendUpdate` (`acp-agent.ts:4551-4639`): every
/// `session/update` the session emits passes here. A notification is routed
/// through the native subagent runtime first (child-owned tool calls and
/// subagent output go to the child session; a client without native subagent
/// sessions gets it back unchanged), then sent.
fn send_update(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    notification: Value,
    typed: Option<SessionUpdate>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    turn: Option<&str>,
) {
    let update = notification.get("update").cloned().unwrap_or(Value::Null);
    let kind = notification
        .pointer("/update/sessionUpdate")
        .and_then(Value::as_str);
    let status = notification
        .pointer("/update/status")
        .and_then(Value::as_str);
    let final_status = kind == Some("tool_call_update") && is_final_tool_call_status(status);
    let tool_call_id = notification_tool_call_id(&notification).map(ToString::to_string);
    let parent = notification_parent_tool_use_id(&notification);
    // Before routing, which can buffer or drop the update: a cancel still has
    // to know the parent of a call that is open here. A later update can be the
    // first to name it, for a call that a permission request emitted.
    if let Some(id) = &tool_call_id {
        if final_status {
            map_state.forget_tool_call_parent(id);
        } else if let Some(parent) = parent {
            if map_state.has_emitted(id) {
                map_state.record_tool_call_parent(id, parent);
            }
        }
    }
    let eager = tool_call_id
        .as_deref()
        .and_then(|id| map_state.eager_session(id));
    let crate::native_subagents::RouteResult { outs, routed } =
        tasks.subagents().route(&notification, eager.as_deref());
    publish_subagent_outs(tasks, map_state, outs, update_tx, session_id, turn);
    let Some(routed) = routed else {
        // Native Agent/Task control calls are intentionally not transcript
        // tools. Do not let the mapper's pre-send de-duplication mark make a
        // later permission request believe that a suppressed call exists.
        if let Some(id) = tool_call_id {
            if crate::native_subagents::is_native_subagent_control_update(&update) {
                map_state.unmark_emitted(&id);
            }
        }
        return;
    };
    let payload = match typed {
        Some(update) => RoutedPayload::Typed(Box::new(update)),
        None => RoutedPayload::VerbatimRaw,
    };
    let _ = deliver_routed(map_state, routed, payload, update_tx, session_id, turn);
}

/// Send a batch of typed updates through [`send_update`].
fn send_updates(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    updates: Vec<SessionUpdate>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    turn: Option<&str>,
) {
    for update in updates {
        let notif = SessionNotification::new(session_id.to_string(), update.clone());
        let mut value = serde_json::to_value(&notif).unwrap_or(Value::Null);
        // C-24: a closing report of a call that the turn end or a cancel
        // stopped is `cancelled` on v2 and `failed` elsewhere
        // (`renderer.ts:650-668`), and so is a result Node's `resultStatus`
        // calls aborted (a command whose structured result says `interrupted`,
        // or an error result of the work a cancel stopped,
        // `renderer.ts:193-204`). The typed v1 status has no `cancelled`, so
        // the v2 send rewrites it on a raw value.
        let has_status = value.pointer("/update/status").is_some();
        let stopped_closing = has_status
            && value
                .pointer("/update/toolCallId")
                .and_then(Value::as_str)
                .is_some_and(|id| {
                    map_state.v2_reporting()
                        && value
                            .pointer("/update/sessionUpdate")
                            .and_then(Value::as_str)
                            == Some("tool_call_update")
                        && map_state.take_stopped_closing(id)
                });
        let aborted_result = has_status
            && map_state.v2_reporting()
            && value
                .pointer("/update/toolCallId")
                .and_then(Value::as_str)
                .is_some_and(|id| map_state.take_aborted_result(id));
        if stopped_closing || aborted_result {
            if let Some(status) = value.pointer_mut("/update/status") {
                *status = json!(crate::v2::tool_call_status_for_aborted(true));
            }
            send_update(tasks, map_state, value, None, update_tx, session_id, turn);
            continue;
        }
        send_update(
            tasks,
            map_state,
            value,
            Some(update),
            update_tx,
            session_id,
            turn,
        );
    }
}

/// Drain the native subagent runtime's diagnostics into the session logger
/// (`NativeSubagentRuntime.logs`), so a long session does not accumulate them.
fn log_subagent_logs(tasks: &mut AsyncTaskRuntime, map_state: &MapState) {
    let logs = tasks.subagents().take_logs();
    if let Some(logger) = map_state.logger() {
        for line in logs {
            logger.log(&line);
        }
    }
}

/// Send the publications of the native subagent runtime, in order (T4B): each
/// `Out` is sent through the session's `sendUpdate` tail, with the eager session
/// lookup and the post-routing bookkeeping of the live path.
fn publish_subagent_outs(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    outs: Vec<crate::native_subagents::Out>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    turn: Option<&str>,
) {
    log_subagent_logs(tasks, map_state);
    if outs.is_empty() {
        return;
    }
    let eager: HashMap<String, String> = map_state.eager_snapshot();
    let mut publish = |notification: Value| subagent_publish(notification, update_tx);
    let mut deliver = |notification: Value| {
        deliver_routed(
            map_state,
            notification,
            RoutedPayload::RuntimeRaw,
            update_tx,
            session_id,
            turn,
        )
    };
    let mut eager_session_of = |id: &str| eager.get(id).cloned();
    let _ = tasks.publish_subagent_outs(
        outs,
        crate::async_tasks::SubagentSends {
            publish: &mut publish,
            deliver: &mut deliver,
            eager_session_of: &mut eager_session_of,
        },
    );
}

/// Close the native subagents that still run (`finishAll`, `acp-agent.ts:4637-4660`).
fn finish_all_subagents(
    tasks: &mut AsyncTaskRuntime,
    map_state: &mut MapState,
    state: crate::acp_subagents::SubagentState,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) -> Vec<String> {
    let eager: HashMap<String, String> = map_state.eager_snapshot();
    let mut publish = |notification: Value| subagent_publish(notification, update_tx);
    let mut deliver = |notification: Value| {
        deliver_routed(
            map_state,
            notification,
            RoutedPayload::RuntimeRaw,
            update_tx,
            session_id,
            None,
        )
    };
    let mut eager_session_of = |id: &str| eager.get(id).cloned();
    tasks.finish_all_subagents_with(
        state,
        crate::async_tasks::SubagentSends {
            publish: &mut publish,
            deliver: &mut deliver,
            eager_session_of: &mut eager_session_of,
        },
    )
}

/// Send the updates an async-task call produced. The runtime records a field as
/// published when it returns the update, upstream only after the send resolved
/// (`recordPublished`, `async-tasks.ts:690-704`): when the update channel is
/// gone the call's records are rolled back, so a retry carries the same fields.
fn publish_task_updates(
    tasks: &mut AsyncTaskRuntime,
    map_state: &MapState,
    updates: Vec<Value>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    for update in updates {
        let routed = route_async_task_update(tasks, map_state, update, session_id);
        if update_tx.send(SessionOutbound::RawUpdate(routed)).is_err() {
            tasks.rollback();
            return;
        }
    }
}

/// Route one async-task update to the child session that owns its call
/// (`task.route`, `async-tasks.ts:749-753`), else the root.
fn route_async_task_update(
    tasks: &mut AsyncTaskRuntime,
    map_state: &MapState,
    update: Value,
    session_id: &str,
) -> Value {
    let task_id = update
        .get("asyncTaskId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let eager = tasks
        .tool_call_id_of(&task_id)
        .and_then(|tool_call_id| map_state.eager_session(&tool_call_id));
    tasks.route_task_update(&task_id, update, session_id, eager.as_deref())
}

/// Send a batch of async-task updates through the same route as
/// [`publish_task_updates`] (the user stop, the held release and the stream-end
/// finish: every update an async task sends goes through `task.route`).
fn emit_task_updates(
    tasks: &mut AsyncTaskRuntime,
    map_state: &MapState,
    updates: Vec<Value>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    for update in updates {
        let routed = route_async_task_update(tasks, map_state, update, session_id);
        let _ = update_tx.send(SessionOutbound::RawUpdate(routed));
    }
}

/// `publishPromptSuggestion` (`acp-agent.ts:4220-4238`): publish the follow-up
/// prompt the CLI predicted for the next turn, or `None` to clear the one the
/// client holds. Only AIR renders it, so only AIR gets the update; an
/// unchanged prediction is not re-sent, and a clear before the first prediction
/// goes nowhere (the client holds nothing).
fn publish_prompt_suggestion(
    air: bool,
    state: &mut state::T2aState,
    text: Option<String>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    // A clear ends the window the turn's result opened: whatever prediction is
    // still in flight answers a turn the user has already moved past.
    if text.is_none() {
        state.prompt_suggestion_expected = false;
    }
    // `undefined` and `null` collapse to `None`, so a clear before the first
    // prediction matches and stops here.
    if state.last_published_prompt_suggestion.as_deref() == text.as_deref() {
        return;
    }
    if !air {
        return;
    }
    state.last_published_prompt_suggestion = text.clone();
    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "session_info_update",
            "_meta": crate::prompt_suggestions::prompt_suggestions_meta(text.as_deref()),
        },
    })));
}

/// Publish one goal snapshot (or its removal) as an AIR-only
/// `session_info_update` (rust-air-gaps G8, `publishGoal`,
/// `acp-agent.ts:4199-4212`). `lastPublishedGoal` is updated by the caller's
/// [`crate::goal::GoalState`] regardless of the client; only AIR gets the
/// update on the wire.
fn publish_goal(
    air: bool,
    goal: &Option<crate::goal::GoalSnapshot>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let Some(update) = crate::goal::goal_publish(air, goal.as_ref()) else {
        return;
    };
    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
        "sessionId": session_id,
        "update": update,
    })));
}

mod clear_context;
pub(crate) mod exit_plan;
pub(crate) mod failures;
mod frames;
pub(crate) mod state;

#[cfg(test)]
mod tests_t1fa;

#[cfg(test)]
mod tests_t2a;

#[cfg(test)]
mod tests_t3a;

#[cfg(test)]
mod tests_g4;

#[cfg(test)]
mod tests_t4a;

#[cfg(test)]
mod tests_t4fa;

#[cfg(test)]
mod tests_t4a2;

#[cfg(test)]
mod tests_t4fd;

#[cfg(test)]
mod tests_t5a;

#[cfg(test)]
mod tests_t5a2;

#[cfg(test)]
mod tests_t5fa;

#[cfg(test)]
mod tests_t5fa4;

#[cfg(test)]
mod tests_t5fa5;

#[cfg(test)]
mod tests_t5fa6;

#[cfg(test)]
mod tests_t6a;

#[cfg(test)]
mod tests_t6g;

#[cfg(test)]
mod tests_t6g2;

#[cfg(test)]
mod tests_t7a;

#[cfg(test)]
mod tests_t7a_race_common;

#[cfg(test)]
mod tests_t7a_race_s1;

#[cfg(test)]
mod tests_t7a_race_s2;

#[cfg(test)]
mod tests_t7a_race_s3;

#[cfg(test)]
mod tests_t7a_race_s4;

#[cfg(test)]
mod tests_t7a_race_s5;

#[cfg(test)]
mod tests_t7a_race_s6;

#[cfg(test)]
mod tests_t7a_race_s7;

#[cfg(test)]
mod tests_t1ga;

#[cfg(test)]
pub(crate) mod tests_t1ha;

#[cfg(test)]
mod tests_t8b;

#[cfg(test)]
mod tests_g1;

#[cfg(test)]
mod tests_g3;

#[cfg(test)]
mod tests {
    use super::*;

    // Review PC-1 (8): a PostToolUse callback is registered on first encounter only
    // (acp-agent.ts:13101 !alreadyCached); a later frame for the same id does not re-register.
    #[test]
    fn hook_registration_is_first_encounter_only() {
        let mut hooks = crate::hooks::HookRegistry::new();
        assert!(register_hook_first_encounter(
            &mut hooks, "toolu_F1", "Bash", None
        ));
        assert!(!register_hook_first_encounter(
            &mut hooks,
            "toolu_F1",
            "Bash",
            Some("p".to_string())
        ));
        assert!(hooks.has("toolu_F1"));
    }

    // Review PC-1 (2) / T-09: a subagent's permission-surfaced TodoWrite or Task*
    // resolution carries parentToolUseId and the non-execution sidecar
    // (acp-agent.ts:13218-13245).
    #[test]
    fn suppressed_result_resolution_carries_parent_tool_use_id() {
        let sub = suppressed_result_resolution(
            "toolu_TK3",
            "TaskCreate",
            false,
            &json!("ok"),
            Some("toolu_PARENT"),
            None,
        );
        assert_eq!(
            sub["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_PARENT"
        );
        let root =
            suppressed_result_resolution("toolu_TK4", "TodoWrite", false, &json!("ok"), None, None);
        assert!(root["_meta"]["claudeCode"].get("parentToolUseId").is_none());
        assert_eq!(root["_meta"]["claudeCode"]["toolName"], "TodoWrite");

        let non_exec = crate::tool_meta::NonExecution {
            kind: "interrupted".into(),
            feedback: None,
        };
        let aborted = suppressed_result_resolution(
            "toolu_TK5",
            "TodoWrite",
            true,
            &json!("stopped"),
            None,
            Some(&non_exec),
        );
        assert_eq!(
            aborted["_meta"]["claudeCode"],
            json!({"toolName": "TodoWrite", "nonExecutionKind": "interrupted"})
        );
        assert_eq!(aborted["status"], "failed");
    }

    // G10c review (1): a multi-result user frame resolves the plan/suppressed
    // tool_result in BLOCK ORDER — Node walks the blocks in one pass
    // (`toAcpNotifications`, acp-agent.ts:13232-13253), so a Bash result before
    // a permission-surfaced TodoWrite must emit the Bash completion first.
    #[test]
    fn user_frame_resolution_follows_block_order() {
        let mut map_state = MapState::default();
        map_state.mark_emitted(
            "toolu_BASH".to_string(),
            json!({"type": "tool_use", "id": "toolu_BASH", "name": "Bash",
                   "input": {"command": "ls"}}),
        );
        map_state.mark_emitted(
            "toolu_TODO".to_string(),
            json!({"type": "tool_use", "id": "toolu_TODO", "name": "TodoWrite",
                   "input": {"todos": [{"content": "x", "status": "pending", "activeForm": "x"}]}}),
        );
        let line = json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_BASH", "content": "out"},
                {"type": "tool_result", "tool_use_id": "toolu_TODO",
                 "content": "Todos updated"}
            ]}
        });
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let blocks = line["message"]["content"].as_array().unwrap();
        handle_user_frame_blocks(
            SendIo {
                tasks: &mut AsyncTaskRuntime::new(false),
                update_tx: &update_tx,
                session_id: "s",
            },
            &line,
            blocks,
            &mut map_state,
            &mut ext,
            None,
        );
        let out: Vec<SessionOutbound> = std::iter::from_fn(|| update_rx.try_recv().ok()).collect();
        assert_eq!(out.len(), 2);
        let SessionOutbound::Update(first) = &out[0] else {
            panic!("the Bash completion must come first");
        };
        let first = serde_json::to_value(&first.update).unwrap();
        assert_eq!(first["sessionUpdate"], "tool_call_update");
        assert_eq!(first["toolCallId"], "toolu_BASH");
        assert_eq!(first["status"], "completed");
        let SessionOutbound::RawUpdate(second) = &out[1] else {
            panic!("the TodoWrite resolution must come second");
        };
        assert_eq!(second["update"]["toolCallId"], "toolu_TODO");
        assert_eq!(second["update"]["status"], "completed");
    }

    // G10c review (4) / T-21: the resolution is emitted at the real call site,
    // with the failed status and the non-execution sidecar of a denied
    // TodoWrite (`acp-agent.ts:13218-13245`).
    #[test]
    fn denied_todo_resolution_at_the_tool_result_call_site() {
        let mut map_state = MapState::default();
        map_state.mark_emitted(
            "toolu_DENY".to_string(),
            json!({"type": "tool_use", "id": "toolu_DENY", "name": "TodoWrite",
                   "input": {"todos": [{"content": "x", "status": "pending", "activeForm": "x"}]}}),
        );
        let line = json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_DENY", "is_error": true,
                 "content": "The user doesn't want to proceed with this tool use."}
            ]},
            "tool_result_meta": [
                {"id": "toolu_DENY", "non_execution_kind": "user-rejected"}
            ]
        });
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let blocks = line["message"]["content"].as_array().unwrap();
        handle_user_frame_blocks(
            SendIo {
                tasks: &mut AsyncTaskRuntime::new(false),
                update_tx: &update_tx,
                session_id: "s",
            },
            &line,
            blocks,
            &mut map_state,
            &mut ext,
            None,
        );
        let out: Vec<SessionOutbound> = std::iter::from_fn(|| update_rx.try_recv().ok()).collect();
        assert_eq!(out.len(), 1);
        let SessionOutbound::RawUpdate(resolution) = &out[0] else {
            panic!("expected the raw resolution");
        };
        assert_eq!(resolution["update"]["status"], "failed");
        assert_eq!(resolution["update"]["toolCallId"], "toolu_DENY");
        assert_eq!(
            resolution["update"]["_meta"]["claudeCode"],
            json!({"toolName": "TodoWrite", "nonExecutionKind": "user-rejected"})
        );
    }

    // G10c review (3) / T-76: a Task* result of a subagent's frame stamps the
    // published plan with parentToolUseId (`acp-agent.ts:13356`); the hook
    // publishers stay unstamped.
    #[test]
    fn task_plan_stamps_the_subagent_parent() {
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        ext.task_state.push((
            "1".to_string(),
            crate::hooks::TaskStateEntry {
                subject: "Task".into(),
                status: "pending".into(),
                active_form: None,
                description: None,
            },
        ));
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        publish_task_plan("s", &mut ext, &update_tx, Some("toolu_TASK"));
        let SessionOutbound::RawUpdate(plan) = update_rx.try_recv().unwrap() else {
            panic!("expected a plan");
        };
        assert_eq!(plan["update"]["sessionUpdate"], "plan");
        assert_eq!(
            plan["update"]["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_TASK"
        );
        let keys: Vec<&String> = plan["update"].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["sessionUpdate", "entries", "_meta"]);
    }

    // Review PC-1 (4): an unlisted model keeps auto mode (session-mode.ts:225-232); only a listed
    // model that does not support auto is downgraded.
    #[test]
    fn auto_mode_is_kept_for_an_unlisted_model() {
        assert!(!auto_mode_downgrade_needed(None));
        let no_auto = crate::session_config::ModelInfo {
            value: "m".into(),
            supports_auto_mode: Some(false),
            ..Default::default()
        };
        assert!(auto_mode_downgrade_needed(Some(&no_auto)));
        let auto = crate::session_config::ModelInfo {
            value: "m".into(),
            supports_auto_mode: Some(true),
            ..Default::default()
        };
        assert!(!auto_mode_downgrade_needed(Some(&auto)));
    }

    // F3a (f): the SDK's `lastErrorResultText` (sdk.mjs `readMessages`): the
    // `result` text on a `success` subtype, the trimmed non-empty `errors`
    // joined with `"; "` otherwise, and `None` for a non-error result.
    #[test]
    fn error_result_text_matches_sdk_last_error_result_text() {
        assert_eq!(
            error_result_text(&json!({"type":"result","subtype":"success","is_error":true,
                "result":"boom"})),
            Some("boom".to_string())
        );
        assert_eq!(
            error_result_text(&json!({"type":"result","subtype":"error_during_execution",
                "is_error":true,"errors":["  first  ","", "second"]})),
            Some("first; second".to_string())
        );
        assert_eq!(
            error_result_text(
                &json!({"type":"result","subtype":"success","is_error":false,
                "result":"ok"})
            ),
            None
        );
        assert_eq!(
            error_result_text(&json!({"type":"result","subtype":"success","is_error":true,
                "result":""})),
            None,
            "an empty diagnostic clears the SDK's lastErrorResultText"
        );
    }

    // F3a / A-19: a rejected set_permission_mode keeps the CLI's non-empty
    // message and only falls back to "Invalid Mode" (session-mode.ts:254-263).
    #[test]
    fn permission_mode_error_preserves_cli_message() {
        assert_eq!(
            permission_mode_error(&crate::control::ControlError::Remote(
                "mode not allowed".to_string()
            )),
            "mode not allowed"
        );
        assert_eq!(
            permission_mode_error(&crate::control::ControlError::Remote(String::new())),
            "Invalid Mode"
        );
        assert_eq!(
            permission_mode_error(&crate::control::ControlError::Closed),
            "Invalid Mode"
        );
    }

    // Review PC-1 (5): an unlisted id's context window is inferred from the id itself
    // (acp-agent.ts:13729-13747).
    #[test]
    fn context_window_seed_reads_the_unlisted_id() {
        // F5b/B-71: an unlisted id is seeded from the id alone; the heuristic
        // miss falls back to the default window, not to `None`.
        let hinted =
            crate::session_model::immediate_context_window("", "claude-opus-4-7[1m]", None);
        assert_eq!(hinted.size, 1_000_000);
        assert!(!hinted.authoritative);
        let plain = crate::session_model::immediate_context_window("", "claude-opus-4-7", None);
        assert_eq!(plain.size, crate::usage::DEFAULT_CONTEXT_WINDOW);
        assert!(!plain.authoritative);
    }

    // Review PC-1 (3): on an external switch the mode update and warning come before the
    // config_option_update (Node publishFallbackState runs inside applyConfigOptionValue).
    #[test]
    fn external_switch_updates_publish_config_last() {
        let opts: Vec<crate::session_config::ConfigOption> = Vec::new();
        let updates = external_switch_updates("s", &opts, Some("acceptEdits"), true, false);
        let kinds: Vec<&str> = updates
            .iter()
            .filter_map(|u| u["update"]["sessionUpdate"].as_str())
            .collect();
        assert_eq!(kinds.first(), Some(&"current_mode_update"));
        assert_eq!(kinds.last(), Some(&"config_option_update"));
        // A client on the notice contract gets the live notice instead of the
        // bold transcript line (F2a).
        let noticed = external_switch_updates("s", &opts, Some("acceptEdits"), true, true);
        assert_eq!(noticed[1]["update"]["sessionUpdate"], "notice");
        assert_eq!(noticed[1]["update"]["severity"], "warning");
        assert_eq!(noticed[1]["update"]["title"], "Auto mode unavailable");
        assert_eq!(
            noticed[1]["update"]["description"],
            "The selected model does not support Auto mode; using Accept edits instead."
        );
    }

    // PC-1.5 (acp-agent.ts:13226-13245): a Task* call a permission prompt surfaced is resolved
    // by a tool_call_update, since the Task branches never emit one.
    #[test]
    fn task_permission_resolution_reports_status_and_tool_name() {
        let ok = suppressed_result_resolution(
            "toolu_TK1",
            "TaskCreate",
            false,
            &json!("done"),
            None,
            None,
        );
        assert_eq!(ok["toolCallId"], "toolu_TK1");
        assert_eq!(ok["status"], "completed");
        assert_eq!(ok["rawOutput"], "done");
        assert_eq!(ok["_meta"]["claudeCode"]["toolName"], "TaskCreate");
        let failed = suppressed_result_resolution(
            "toolu_TK2",
            "TaskUpdate",
            true,
            &json!("boom"),
            None,
            None,
        );
        assert_eq!(failed["status"], "failed");
    }

    // U-26 (tools.ts:330-352, air-extension.ts:119-121): the plan dedupe is for every
    // client that declared `_meta.jetbrains.air` as an object, even without a version or
    // capabilities list; a non-AIR client gets every plan.
    #[test]
    fn air_plan_dedupe_uses_meta_presence_not_the_capabilities_list() {
        let caps = json!({"_meta": {"jetbrains": {"air": {}}}});
        let air = crate::air::AirClient::from_client_capabilities(&caps);
        assert!(air.is_some(), "an empty air object is still an AIR client");
        let mut ext = SessionExt::new(
            air,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        ext.task_state.push((
            "1".to_string(),
            crate::hooks::TaskStateEntry {
                subject: "Task".into(),
                status: "pending".into(),
                active_form: None,
                description: None,
            },
        ));
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        publish_task_plan("s", &mut ext, &update_tx, None);
        publish_task_plan("s", &mut ext, &update_tx, None);
        let air_plans = std::iter::from_fn(|| update_rx.try_recv().ok())
            .filter(|out| {
                matches!(
                    out,
                    SessionOutbound::RawUpdate(v)
                        if v["update"]["sessionUpdate"] == "plan"
                )
            })
            .count();
        assert_eq!(
            air_plans, 1,
            "the unchanged plan is skipped for an AIR client"
        );

        let mut plain = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        plain.task_state = ext.task_state.clone();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        publish_task_plan("s", &mut plain, &update_tx, None);
        publish_task_plan("s", &mut plain, &update_tx, None);
        let plain_plans = std::iter::from_fn(|| update_rx.try_recv().ok())
            .filter(|out| {
                matches!(
                    out,
                    SessionOutbound::RawUpdate(v)
                        if v["update"]["sessionUpdate"] == "plan"
                )
            })
            .count();
        assert_eq!(plain_plans, 2, "a non-AIR client gets every plan");
    }

    // T-77 (tools.ts:452-485): TaskCreated rejects an empty task_id or task_subject
    // before touching the task state or publishing a plan.
    #[test]
    fn task_created_hook_rejects_empty_id_or_subject() {
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        let mut map_state = MapState::default();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let mut run = |input: Value, ext: &mut SessionExt| {
            let (reply, _reply_rx) = oneshot::channel();
            handle_hook_callback(
                &mut AsyncTaskRuntime::new(false),
                HookCall {
                    request_id: "r",
                    callback_id: crate::hooks::TASK_CREATED_HOOK_ID,
                    input: &input,
                    tool_use_id: None,
                },
                &mut map_state,
                ext,
                HookIo {
                    update_tx: &update_tx,
                    command_tx: &command_tx,
                    session_id: "s",
                    live_background: &HashMap::new(),
                },
                reply,
            );
        };
        run(json!({"task_id": "", "task_subject": "X"}), &mut ext);
        assert!(ext.task_state.is_empty());
        run(json!({"task_id": "1", "task_subject": ""}), &mut ext);
        assert!(ext.task_state.is_empty());
        run(json!({"task_id": "1", "task_subject": "A"}), &mut ext);
        assert_eq!(ext.task_state.len(), 1);
        assert_eq!(ext.task_state[0].1.subject, "A");
        let plans = std::iter::from_fn(|| update_rx.try_recv().ok())
            .filter(|out| {
                matches!(
                    out,
                    SessionOutbound::RawUpdate(v)
                        if v["update"]["sessionUpdate"] == "plan"
                )
            })
            .count();
        assert_eq!(plans, 1, "only the valid hook publishes a plan");
    }

    // PC-1.1 (acp-agent.ts:10724 syncModelAfterExternalSwitch): a PostModelSwitch hook
    // reports the SDK-switched model; the adapter tracks the option value it maps to,
    // or the raw id when none matches, and does nothing when already tracked.
    #[test]
    fn external_switch_target_maps_to_option_value_and_skips_tracked_model() {
        let models = vec![
            crate::session_config::ModelInfo {
                value: "claude-opus-4-6".into(),
                display_name: "Opus".into(),
                ..Default::default()
            },
            crate::session_config::ModelInfo {
                value: "claude-sonnet-4-5".into(),
                display_name: "Sonnet".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            external_switch_target(&models, "claude-sonnet-4-5", "claude-opus-4-6"),
            Some("claude-opus-4-6".to_string())
        );
        assert_eq!(
            external_switch_target(&models, "claude-opus-4-6", "claude-opus-4-6"),
            None
        );
        assert_eq!(
            external_switch_target(&models, "claude-opus-4-6", "custom-unlisted"),
            Some("custom-unlisted".to_string())
        );
    }

    // PC-1.1: a root-session PostCompact records its summary; a subagent one (agent_id) does not.
    #[test]
    fn post_compact_hook_records_root_summary_only() {
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        let mut map_state = MapState::default();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let mut run = |input: Value, ext: &mut SessionExt| {
            let (reply, _reply_rx) = oneshot::channel();
            handle_hook_callback(
                &mut AsyncTaskRuntime::new(false),
                HookCall {
                    request_id: "r",
                    callback_id: crate::hooks::POST_COMPACT_HOOK_ID,
                    input: &input,
                    tool_use_id: None,
                },
                &mut map_state,
                ext,
                HookIo {
                    update_tx: &update_tx,
                    command_tx: &command_tx,
                    session_id: "s",
                    live_background: &HashMap::new(),
                },
                reply,
            );
        };
        run(json!({"compact_summary": "root summary"}), &mut ext);
        assert_eq!(ext.last_compaction_summary.as_deref(), Some("root summary"));
        run(
            json!({"agent_id": "sub", "compact_summary": "sub summary"}),
            &mut ext,
        );
        assert_eq!(ext.last_compaction_summary.as_deref(), Some("root summary"));
    }

    #[allow(clippy::too_many_arguments)]
    fn test_handle_frame(
        line: Value,
        machine: &mut TurnMachine,
        map_state: &mut MapState,
        live_background: &mut HashMap<String, String>,
        pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>>,
        emitted_assistant_text: &mut bool,
        assistant_had_error: &mut bool,
        update_tx: &mpsc::UnboundedSender<SessionOutbound>,
        session_id: &str,
        high_water: &AtomicUsize,
        tasks: &mut AsyncTaskRuntime,
    ) {
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        super::handle_session_frame(
            line,
            machine,
            map_state,
            live_background,
            pending,
            emitted_assistant_text,
            assistant_had_error,
            update_tx,
            session_id,
            high_water,
            tasks,
            &mut ext,
        );
    }

    /// 8.T7-style unit: a lagging trailing idle after the next echo is absorbed,
    /// not a false #825 fail. Exercises the machine via the actor-facing
    /// `TurnMachine` directly through `handle_session_frame`.
    #[tokio::test]
    async fn lagging_idle_absorbed_not_false_fail() {
        let mut machine = TurnMachine::new();
        let mut _map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (_tx, rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let _ = rx; // update sink unused here
        let (utx, _urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));

        // p1 echoed + result settles it (A's result).
        machine.enqueue(Turn::new("p1".into(), false));
        let _ = machine.on_echo("p1");
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"ok","usage":{}}), false);
        handle_events(
            &events,
            &mut pending,
            &utx,
            "s",
            &hw,
            &crate::session_failure::AssistantFailureCtx::default(),
            None,
            None,
            &mut failures::SessionFailures::default(),
            &mut crate::turn_events::V2State::new(),
        );

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
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","origin":{"kind":"task-notification"},"num_turns":1,"usage":{}}), false);
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

    /// Phase 15 (Item 2) — `handle_events` keys the presence of `usage` in the
    /// prompt reply off the `had_usage` origin flag, not the numeric token sum.
    /// An active turn cancelled before any tokens accumulate reports an all-zero
    /// `usage` object; a queued turn swept by cancel reports no `usage`.
    #[tokio::test]
    async fn item2_cancel_usage_keys_off_had_usage_origin() {
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));

        async fn settle(
            update_tx: &mpsc::UnboundedSender<SessionOutbound>,
            hw: &Arc<AtomicUsize>,
            had_usage: bool,
        ) -> PromptReply {
            let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
                HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p1".to_string(), reply_tx);
            let events = vec![TurnEvent::Settled {
                prompt_uuid: "p1".to_string(),
                stop_reason: crate::turn::StopReason::Cancelled,
                usage: crate::turn::Usage::default(),
                had_usage,
                model_rows: Default::default(),
            }];
            handle_events(
                &events,
                &mut pending,
                update_tx,
                "s1",
                hw,
                &crate::session_failure::AssistantFailureCtx::default(),
                None,
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            reply_rx.await.expect("reply sent").expect("prompt ok")
        }

        // Active turn cancelled with zero tokens: `had_usage = true` -> Some(zero).
        let active = settle(&update_tx, &hw, true).await;
        let u = active
            .usage
            .expect("an active turn's cancel must report an all-zero usage object");
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cached_read_tokens, 0);
        assert_eq!(u.cached_write_tokens, 0);
        assert_eq!(u.total_tokens, 0);

        // 2.12 (E-6): an active turn's settle carries `_meta.quota`; the shape
        // is totals-only (`model_usage: []`, Decision 20).
        let aq = active
            .quota
            .expect("an active turn's settle must carry _meta.quota");
        assert_eq!(
            aq,
            json!({
                "quota": {
                    "token_count": {
                        "totalTokens": 0,
                        "inputTokens": 0,
                        "cachedInputTokens": 0,
                        "cachedWriteTokens": 0,
                        "outputTokens": 0,
                        "reasoningOutputTokens": 0,
                    },
                    "model_usage": [],
                }
            }),
            "2.12: quota is totals-only with model_usage []"
        );

        // Queued turn swept by cancel: `had_usage = false` -> None (both usage
        // and quota).
        let queued = settle(&update_tx, &hw, false).await;
        assert!(
            queued.usage.is_none(),
            "a queued turn swept by cancel must report no usage"
        );
        assert!(
            queued.quota.is_none(),
            "2.12: a queued turn swept by cancel must carry no _meta.quota (inv_37_queued_sweep_has_no_quota)"
        );
    }

    /// 2.12 (inv_37_quota_meta_shape) — the quota totals equal the summed
    /// usage for a non-zero settle, with `cached*`/`reasoning` fields in place.
    #[tokio::test]
    async fn inv_37_quota_meta_shape() {
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        pending.insert("p1".to_string(), reply_tx);
        let events = vec![TurnEvent::Settled {
            prompt_uuid: "p1".to_string(),
            stop_reason: crate::turn::StopReason::EndTurn,
            usage: crate::turn::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_read_tokens: 3,
                cached_write_tokens: 2,
            },
            had_usage: true,
            model_rows: Default::default(),
        }];
        handle_events(
            &events,
            &mut pending,
            &update_tx,
            "s1",
            &hw,
            &crate::session_failure::AssistantFailureCtx::default(),
            None,
            None,
            &mut failures::SessionFailures::default(),
            &mut crate::turn_events::V2State::new(),
        );
        let reply = reply_rx.await.expect("reply sent").expect("prompt ok");
        let quota = reply.quota.expect("quota present");
        assert_eq!(
            quota,
            json!({
                "quota": {
                    "token_count": {
                        "totalTokens": 20, // 10 + 5 + 3 + 2
                        "inputTokens": 10,
                        "cachedInputTokens": 3,
                        "cachedWriteTokens": 2,
                        "outputTokens": 5,
                        "reasoningOutputTokens": 0,
                    },
                    "model_usage": [],
                }
            })
        );
    }

    /// D11 (Decision 10) — the result-text chunk carries the result frame's
    /// `uuid` as its `messageId` (`messageIdForGrouping(result)`,
    /// `acp-agent.ts:5301`).
    #[tokio::test]
    async fn inv_72_final_text_carries_result_uuid() {
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        pending.insert("p1".to_string(), reply_tx);
        let events = vec![
            TurnEvent::FinalText {
                text: "context output".to_string(),
            },
            TurnEvent::Settled {
                prompt_uuid: "p1".to_string(),
                stop_reason: crate::turn::StopReason::EndTurn,
                usage: crate::turn::Usage::default(),
                had_usage: false,
                model_rows: Default::default(),
            },
        ];
        handle_events(
            &events,
            &mut pending,
            &update_tx,
            "s1",
            &hw,
            &crate::session_failure::AssistantFailureCtx::default(),
            None,
            Some("result-uuid-1"),
            &mut failures::SessionFailures::default(),
            &mut crate::turn_events::V2State::new(),
        );
        let reply = reply_rx.await.expect("reply sent").expect("prompt ok");
        let chunk = match &reply.updates[0].update {
            SessionUpdate::AgentMessageChunk(chunk) => chunk,
            other => panic!("expected an agent_message_chunk, got {other:?}"),
        };
        assert_eq!(
            chunk.message_id.as_ref().map(|id| id.0.to_string()),
            Some("result-uuid-1".to_string())
        );
    }

    /// D11 (Decision 10) — a result frame without a `uuid` leaves the chunk
    /// unstamped (the existing `context-echo-less` fixture shape).
    #[tokio::test]
    async fn inv_72_final_text_without_uuid_has_none() {
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        pending.insert("p1".to_string(), reply_tx);
        let events = vec![
            TurnEvent::FinalText {
                text: "context output".to_string(),
            },
            TurnEvent::Settled {
                prompt_uuid: "p1".to_string(),
                stop_reason: crate::turn::StopReason::EndTurn,
                usage: crate::turn::Usage::default(),
                had_usage: false,
                model_rows: Default::default(),
            },
        ];
        handle_events(
            &events,
            &mut pending,
            &update_tx,
            "s1",
            &hw,
            &crate::session_failure::AssistantFailureCtx::default(),
            None,
            None,
            &mut failures::SessionFailures::default(),
            &mut crate::turn_events::V2State::new(),
        );
        let reply = reply_rx.await.expect("reply sent").expect("prompt ok");
        let chunk = match &reply.updates[0].update {
            SessionUpdate::AgentMessageChunk(chunk) => chunk,
            other => panic!("expected an agent_message_chunk, got {other:?}"),
        };
        assert!(chunk.message_id.is_none());
    }

    #[tokio::test]
    async fn inv_54_handle_events_session_failure() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            use crate::air::AirClient;
            use crate::session_failure::AssistantFailureCtx;

            let air_caps = json!({
                "_meta": {
                    "jetbrains": {
                        "air": {
                            "version": 1,
                            "capabilities": ["sessionFailure"]
                        }
                    }
                }
            });
            let air = AirClient::from_client_capabilities(&air_caps);
            assert!(air.as_ref().unwrap().supports("sessionFailure"));

            let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
            let hw = Arc::new(AtomicUsize::new(0));

            // 1. AIR client + IncompleteToolCall -> Ok(PromptReply) with sessionFailure in quota
            let mut pending = HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p1".to_string(), reply_tx);
            let events = vec![TurnEvent::Failed {
                prompt_uuid: "p1".to_string(),
                kind: FailureKind::IncompleteToolCall,
                message: "Claude ended the turn without returning results for tool calls: toolu_1"
                    .to_string(),
                usage: crate::turn::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_read_tokens: 0,
                    cached_write_tokens: 0,
                },
                model_rows: Default::default(),
                is_structured_output_retries: false,
            }];
            handle_events(
                &events,
                &mut pending,
                &update_tx,
                "s1",
                &hw,
                &AssistantFailureCtx::default(),
                air.as_ref(),
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            let reply = reply_rx.await.expect("channel recv").expect("reply is Ok");
            assert_eq!(reply.stop_reason, "end_turn");
            let meta = reply.quota.expect("meta is present");
            let sf = &meta["jetbrains"]["air"]["sessionFailure"];
            assert_eq!(sf["id"], "p1:error");
            assert_eq!(sf["revision"], 1);
            assert_eq!(sf["category"], "service");
            assert_eq!(sf["severity"], "error");
            assert_eq!(
                sf["title"],
                "Claude ended the turn without returning results for tool calls: toolu_1"
            );
            assert_eq!(sf["actions"], json!(["retry", "new_session"]));
            assert_eq!(meta["quota"]["token_count"]["inputTokens"], 10);
            assert_eq!(meta["quota"]["token_count"]["outputTokens"], 5);

            // 2. Non-AIR client -> Err(SessionError::PromptFailed)
            let mut pending = HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p2".to_string(), reply_tx);
            let events = vec![TurnEvent::Failed {
                prompt_uuid: "p2".to_string(),
                kind: FailureKind::ProviderError,
                message: "provider error".to_string(),
                usage: crate::turn::Usage::default(),
                model_rows: Default::default(),
                is_structured_output_retries: false,
            }];
            handle_events(
                &events,
                &mut pending,
                &update_tx,
                "s1",
                &hw,
                &AssistantFailureCtx::default(),
                None,
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            let reply = reply_rx.await.expect("channel recv");
            assert!(matches!(
                reply.map_err(|failure| failure.error),
                Err(SessionError::PromptFailed {
                    kind: FailureKind::ProviderError,
                    ..
                })
            ));

            // 3. AIR client + AuthRequired -> Err(SessionError::PromptFailed) (not converted to sessionFailure)
            let mut pending = HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p3".to_string(), reply_tx);
            let events = vec![TurnEvent::Failed {
                prompt_uuid: "p3".to_string(),
                kind: FailureKind::AuthRequired,
                message: "Authentication required".to_string(),
                usage: crate::turn::Usage::default(),
                model_rows: Default::default(),
                is_structured_output_retries: false,
            }];
            handle_events(
                &events,
                &mut pending,
                &update_tx,
                "s1",
                &hw,
                &AssistantFailureCtx::default(),
                air.as_ref(),
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            let reply = reply_rx.await.expect("channel recv");
            assert!(matches!(
                reply.map_err(|failure| failure.error),
                Err(SessionError::PromptFailed {
                    kind: FailureKind::AuthRequired,
                    ..
                })
            ));

            // 4. Finding 3: AIR client + ProviderError with assistant authentication_failed
            // MUST reject with AuthRequired (-32000), NOT sessionFailure!
            let mut pending = HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p4".to_string(), reply_tx);
            let auth_ctx = AssistantFailureCtx {
                error: Some("authentication_failed".to_string()),
                was_usage_limit: false,
                title: Some("Authentication failed".to_string()),
            };
            let events = vec![TurnEvent::Failed {
                prompt_uuid: "p4".to_string(),
                kind: FailureKind::ProviderError,
                message: "Claude Code returned an error result".to_string(),
                usage: crate::turn::Usage::default(),
                model_rows: Default::default(),
                is_structured_output_retries: false,
            }];
            handle_events(
                &events,
                &mut pending,
                &update_tx,
                "s1",
                &hw,
                &auth_ctx,
                air.as_ref(),
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            let reply = reply_rx.await.expect("channel recv");
            assert!(matches!(
                reply.map_err(|failure| failure.error),
                Err(SessionError::PromptFailed {
                    kind: FailureKind::AuthRequired,
                    ..
                })
            ));

            // 5. Finding 7: AIR failure carries per-model quota rows
            let mut pending = HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p5".to_string(), reply_tx);
            let tally = crate::quota::Tally {
                input: 100,
                output: 50,
                cached_read: 0,
                cached_write: 0,
            };
            let rows = crate::quota::ModelRows::single("claude-sonnet-4-5", tally);
            let events = vec![TurnEvent::Failed {
                prompt_uuid: "p5".to_string(),
                kind: FailureKind::IncompleteToolCall,
                message: "stuck tool".to_string(),
                usage: crate::turn::Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cached_read_tokens: 0,
                    cached_write_tokens: 0,
                },
                model_rows: Arc::new(rows),
                is_structured_output_retries: false,
            }];
            handle_events(
                &events,
                &mut pending,
                &update_tx,
                "s1",
                &hw,
                &AssistantFailureCtx::default(),
                air.as_ref(),
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            let reply = reply_rx.await.expect("channel recv").expect("reply is Ok");
            let meta = reply.quota.expect("meta is present");
            let model_usage = meta["quota"]["model_usage"].as_array().expect("array");
            assert_eq!(model_usage.len(), 1);
            assert_eq!(model_usage[0]["model"], "claude-sonnet-4-5");
            assert_eq!(model_usage[0]["token_count"]["inputTokens"], 100);
            assert_eq!(model_usage[0]["token_count"]["outputTokens"], 50);

            // 6. Finding 10: structured output retries hardcodes provider_error and message title
            let mut pending = HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p6".to_string(), reply_tx);
            let limit_ctx = AssistantFailureCtx {
                error: Some("rate_limit".to_string()),
                was_usage_limit: true,
                title: Some("Rate limited title".to_string()),
            };
            let events = vec![TurnEvent::Failed {
                prompt_uuid: "p6".to_string(),
                kind: FailureKind::ProviderError,
                message: "schema_failed, invalid_json".to_string(),
                usage: crate::turn::Usage::default(),
                model_rows: Default::default(),
                is_structured_output_retries: true,
            }];
            handle_events(
                &events,
                &mut pending,
                &update_tx,
                "s1",
                &hw,
                &limit_ctx,
                air.as_ref(),
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            let reply = reply_rx.await.expect("channel recv").expect("reply is Ok");
            let sf = &reply.quota.unwrap()["jetbrains"]["air"]["sessionFailure"];
            assert_eq!(sf["category"], "service");
            assert_eq!(sf["actions"], json!(["retry"]));
            assert_eq!(sf["title"], "schema_failed, invalid_json");
        })
        .await
        .expect("inv_54_handle_events_session_failure timed out");
    }

    #[tokio::test]
    async fn inv_54_synthetic_usage_limit_actor_turn() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            use crate::air::AirClient;
            use crate::session_failure::AssistantFailureCtx;

            let air_caps = json!({
                "_meta": {
                    "jetbrains": {
                        "air": {
                            "version": 1,
                            "capabilities": ["sessionFailure"]
                        }
                    }
                }
            });
            let air = AirClient::from_client_capabilities(&air_caps);

            let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
            let hw = Arc::new(AtomicUsize::new(0));

            let mut pending = HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p-limit".to_string(), reply_tx);

            let failure_ctx = AssistantFailureCtx {
                error: None,
                was_usage_limit: true,
                title: Some("You've hit your usage limit".to_string()),
            };

            let events = vec![TurnEvent::Failed {
                prompt_uuid: "p-limit".to_string(),
                kind: FailureKind::ProviderError,
                message: "The model provider returned an error.".to_string(),
                usage: crate::turn::Usage {
                    input_tokens: 50,
                    output_tokens: 20,
                    cached_read_tokens: 0,
                    cached_write_tokens: 0,
                },
                model_rows: Default::default(),
                is_structured_output_retries: false,
            }];
            handle_events(
                &events,
                &mut pending,
                &update_tx,
                "s1",
                &hw,
                &failure_ctx,
                air.as_ref(),
                None,
                &mut failures::SessionFailures::default(),
                &mut crate::turn_events::V2State::new(),
            );
            let reply = reply_rx.await.expect("channel recv").expect("reply is Ok");
            assert_eq!(reply.stop_reason, "end_turn");
            let meta = reply.quota.expect("meta is present");
            let sf = &meta["jetbrains"]["air"]["sessionFailure"];
            assert_eq!(sf["id"], "p-limit:error");
            assert_eq!(sf["revision"], 1);
            assert_eq!(sf["category"], "limit");
            assert_eq!(sf["severity"], "error");
            assert_eq!(sf["title"], "You've hit your usage limit");
            assert_eq!(sf["actions"], json!([]));
        })
        .await
        .expect("inv_54_synthetic_usage_limit_actor_turn timed out");
    }

    /// Review G14 finding 4 — the result-text fallback chunk is a top-level
    /// `agent_message_chunk`, so Node collects it for the title context too
    /// (`sendUpdate`, `acp-agent.ts:4618-4622`).
    #[tokio::test]
    async fn inv_80_result_text_feeds_title_context() {
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (reply_tx, _reply_rx) = oneshot::channel();
        pending.insert("p1".to_string(), reply_tx);
        let mut map_state = MapState::default();
        let mut tasks = AsyncTaskRuntime::new(false);
        let live_background = HashMap::new();
        let mut titles = crate::session_titles::SessionTitles::new();
        let events = vec![
            TurnEvent::FinalText {
                text: "the answer text".to_string(),
            },
            TurnEvent::Settled {
                prompt_uuid: "p1".to_string(),
                stop_reason: crate::turn::StopReason::EndTurn,
                usage: crate::turn::Usage::default(),
                had_usage: false,
                model_rows: Default::default(),
            },
        ];
        settle_events(
            events,
            &mut tasks,
            &mut map_state,
            &live_background,
            &mut pending,
            &update_tx,
            "s1",
            &hw,
            &crate::session_failure::AssistantFailureCtx::default(),
            None,
            None,
            &mut titles,
            &mut failures::SessionFailures::default(),
            &mut crate::usage::UsageTurns::default(),
            &mut FileChangeReports::new(
                false,
                std::path::Path::new("/tmp"),
                &[],
                None,
                &update_tx,
                "s1",
                None,
            ),
        );
        assert_eq!(
            titles.context.as_deref(),
            Some("the answer text"),
            "the result-text chunk must feed onAssistantText"
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
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
        test_handle_frame(
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
            &mut AsyncTaskRuntime::new(false),
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

    /// Phase 15 (Item 3) — a plain top-level `Read` tool turn ends with a
    /// terminal `tool_call_update{status:"completed"}` on the wire. The CLI
    /// delivers the tool result in a `user` frame whose `message.content` holds
    /// `tool_result` blocks; previously the `"user"` arm only promoted the echo
    /// and dropped this content, so the completing update never reached the
    /// client. Drives the real actor frame path (`handle_session_frame`).
    #[tokio::test]
    async fn item3_read_tool_turn_ends_with_completed_tool_call_update() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();

        // A prompt turn is active so the frames route normally.
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        // The assistant streams the `Read` tool_use via a stream_event
        // content_block_start — this surfaces the `tool_call` and caches the
        // block so the later tool_result can resolve it.
        test_handle_frame(
            json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_01ABC",
                        "name": "Read",
                        "input": {},
                    }
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut AsyncTaskRuntime::new(false),
        );

        // The tool executes and the CLI emits a `user` frame carrying the
        // `tool_result` block — the terminal completion must be emitted.
        test_handle_frame(
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_01ABC",
                        "content": "This is the first line of the scratch file."
                    }]
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut AsyncTaskRuntime::new(false),
        );

        // Read the outbound updates and find the terminal completing update.
        let mut completed_seen = false;
        while let Ok(item) = urx.try_recv() {
            let SessionOutbound::Update(notif) = item else {
                continue;
            };
            let json = serde_json::to_value(&notif.update).unwrap();
            if json["sessionUpdate"] == "tool_call_update"
                && json["toolCallId"] == "toolu_01ABC"
                && json["status"] == "completed"
            {
                assert!(
                    json["content"].is_array() && !json["content"].as_array().unwrap().is_empty(),
                    "the completing tool_call_update must carry the tool output content"
                );
                assert_eq!(
                    json["_meta"]["claudeCode"]["toolName"],
                    json!("Read"),
                    "the completing update must keep the tool name meta"
                );
                completed_seen = true;
            }
        }
        assert!(
            completed_seen,
            "a plain top-level Read tool turn must emit a terminal tool_call_update{{status:completed}}"
        );
    }

    /// rust-parity-fixes 1.5 (P3, Decision 5) — any `isReplay:true` user frame
    /// (text-only, image+text, or a multi-block steer echo) is NOT forwarded;
    /// a non-replay `tool_result` user frame is still mapped.
    #[tokio::test]
    async fn inv_58_image_echo_not_forwarded() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);

        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        // Register a streamed tool_use so the later tool_result resolves.
        test_handle_frame(
            json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_echo",
                        "name": "Read",
                        "input": {},
                    }
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        while urx.try_recv().is_ok() {}

        // (a) the image prompt echo carries [image, text] and isReplay:true.
        test_handle_frame(
            json!({"type":"user","uuid":"p1","isReplay":true,"message":{"role":"user","content":[
                {"type":"image","source":{"type":"base64","data":"AAA","media_type":"image/png"}},
                {"type":"text","text":"look"}]}}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        // (b) a single-text echo without isReplay stays skipped (Node too).
        test_handle_frame(
            json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"look"}]}}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        // (c) a steered prompt's echo is multi-block and isReplay:true.
        test_handle_frame(
            json!({"type":"user","uuid":"p1","isReplay":true,"message":{"role":"user","content":[
                {"type":"text","text":"a"},{"type":"text","text":"b"}]}}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        assert!(
            urx.try_recv().is_err(),
            "no echo frame may be forwarded, whatever its content"
        );

        // A non-replay tool_result frame is still mapped (the completion is
        // not an echo).
        test_handle_frame(
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_echo","content":"out"}]}}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        let mut completed = false;
        while let Ok(item) = urx.try_recv() {
            if let SessionOutbound::Update(notif) = item {
                let json = serde_json::to_value(&notif.update).unwrap();
                completed |= json["sessionUpdate"] == "tool_call_update"
                    && json["toolCallId"] == "toolu_echo";
            }
        }
        assert!(completed, "a tool_result user frame must still be mapped");
    }

    /// rust-parity-fixes 1.7 (B1/X9, Decision 18) — after a cancel the
    /// consolidated remainder is NOT emitted while the stopped-work window is
    /// open (Node's `session.cancelled` guard, `acp-agent.js:5695-5712`); the
    /// next prompt's activation clears it (Node `activateTurn`).
    #[tokio::test]
    async fn inv_68_no_remainder_after_cancel() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);

        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        // `Command::Cancel` opens the stopped-work window on the map state.
        map_state.begin_stopped_work();

        let frame = json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"late text"}]}});
        test_handle_frame(
            frame.clone(),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        assert!(
            urx.try_recv().is_err(),
            "the remainder must not be emitted after a cancel"
        );
        assert!(
            !emitted_assistant_text,
            "a dropped remainder is not delivered assistant text"
        );

        // The next prompt's echo activates its turn and clears the window.
        machine.enqueue(Turn::new("p2".into(), false));
        test_handle_frame(
            json!({"type":"user","uuid":"p2","message":{"role":"user",
                "content":[{"type":"text","text":"next"}]}}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        test_handle_frame(
            frame,
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        let mut delivered = String::new();
        while let Ok(item) = urx.try_recv() {
            if let SessionOutbound::Update(notif) = item {
                let json = serde_json::to_value(&notif.update).unwrap();
                if json["sessionUpdate"] == "agent_message_chunk" {
                    delivered.push_str(json["content"]["text"].as_str().unwrap_or(""));
                }
            }
        }
        assert_eq!(
            delivered, "late text",
            "activation must clear the stopped-work window"
        );
    }

    /// Minor H: a turn activated through ensure_active_turn on result clears
    /// the stopped-work window and allows consolidated text to be emitted.
    #[tokio::test]
    async fn inv_68_remainder_after_cancel_when_activated_by_result() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);

        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        machine.cancel();
        // Cancel opens the stopped-work window.
        map_state.begin_stopped_work();
        // Trailing idle settles the cancelled turn p1.
        machine.on_idle();

        // Queue p2 without an echo arriving yet.
        machine.enqueue(Turn::new("p2".into(), false));

        // Result arrives for p2, triggering ensure_active_turn and activating p2.
        test_handle_frame(
            json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "user_message_uuid": "p2",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );

        // A consolidated assistant frame for the now-activated turn arrives:
        // its consolidated text IS emitted because the activation cleared the stopped-work window.
        let frame = json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {
                "id": "msg_2",
                "model": "claude-sonnet-4-5",
                "role": "assistant",
                "content": [{"type": "text", "text": "result-promoted text"}]
            }
        });
        test_handle_frame(
            frame,
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );

        let mut delivered = String::new();
        while let Ok(item) = urx.try_recv() {
            if let SessionOutbound::Update(notif) = item {
                let json = serde_json::to_value(&notif.update).unwrap();
                if json["sessionUpdate"] == "agent_message_chunk" {
                    delivered.push_str(json["content"]["text"].as_str().unwrap_or(""));
                }
            }
        }
        assert_eq!(
            delivered, "result-promoted text",
            "activation via ensure_active_turn in result must clear the stopped-work window"
        );
    }

    /// Minor I: Node also drops a uuid-matched echo that has NO isReplay flag.
    #[tokio::test]
    async fn inv_58_uuid_matched_echo_without_is_replay_dropped() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);

        machine.enqueue(Turn::new("p1".into(), false));

        // Multi-block user frame matching "p1" with NO isReplay flag.
        // It should activate the turn and be dropped (suppressed) as an echo.
        test_handle_frame(
            json!({
                "type": "user",
                "uuid": "p1",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "image", "source": {"type": "base64", "data": "AAA", "media_type": "image/png"}},
                        {"type": "text", "text": "look at this"}
                    ]
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );

        // Verify nothing was forwarded to the client.
        assert!(
            urx.try_recv().is_err(),
            "uuid-matched echo without isReplay must be dropped"
        );
    }

    /// rust-parity-fixes 1.7 (Decision 18) — the #453 result-text fallback
    /// still fires when nothing was delivered: a consolidated assistant frame
    /// without the strict-null parent field keeps its text internal.
    #[tokio::test]
    async fn inv_68_result_text_fallback_still_fires() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        pending.insert("p1".into(), reply_tx);
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);

        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        test_handle_frame(
            json!({"type":"assistant",
                "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
                "content":[{"type":"text","text":"internal text"}]}}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );
        assert!(!emitted_assistant_text);

        test_handle_frame(
            json!({"type":"result","subtype":"success","is_error":false,
                "stop_reason":"end_turn","result":"fallback text",
                "usage":{"input_tokens":7,"output_tokens":0}}),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
        );

        let reply = reply_rx.await.expect("the prompt resolves");
        let reply = reply.expect("a success result settles the turn");
        let delivered: String = reply
            .updates
            .iter()
            .filter_map(|notif| match &notif.update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            delivered, "fallback text",
            "the result-text fallback must still fire when nothing was emitted"
        );
        // No assistant chunk went out through the update stream either.
        while let Ok(item) = urx.try_recv() {
            if let SessionOutbound::Update(notif) = item {
                let json = serde_json::to_value(&notif.update).unwrap();
                assert_ne!(json["sessionUpdate"], "agent_message_chunk", "{json}");
            }
        }
    }

    /// 3.7 (E-12) wiring through the real frame path: a streamed-only tool_use
    /// (never in a complete assistant message) is closed `failed` at the
    /// `end_turn` result and the prompt still resolves `end_turn`; a dispatched
    /// one (complete assistant message, no `tool_result`) fails the prompt.
    #[tokio::test]
    async fn inv_41_frame_path_abandoned_vs_dispatched() {
        for dispatched in [false, true] {
            let mut machine = TurnMachine::new();
            let mut map_state = MapState::default();
            let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
                HashMap::new();
            let (reply_tx, reply_rx) = oneshot::channel();
            pending.insert("p1".into(), reply_tx);
            let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
            let hw = Arc::new(AtomicUsize::new(0));
            let (mut text, mut err) = (false, false);
            let mut bg: HashMap<String, String> = HashMap::new();
            machine.enqueue(Turn::new("p1".into(), false));
            machine.on_echo("p1");
            let mut frames = vec![
                json!({"type":"stream_event","event":{"type":"content_block_start",
                "index":0,"content_block":{"type":"tool_use","id":"toolu_X","name":"Bash","input":{}}}}),
            ];
            if dispatched {
                frames.push(
                    json!({"type":"assistant","message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_X","name":"Bash","input":{"command":"ls"}}]}}),
                );
            }
            frames.push(json!({"type":"result","subtype":"success","is_error":false,
                "stop_reason":"end_turn","result":"ok","usage":{}}));
            for f in frames {
                test_handle_frame(
                    f,
                    &mut machine,
                    &mut map_state,
                    &mut bg,
                    &mut pending,
                    &mut text,
                    &mut err,
                    &utx,
                    "s",
                    &hw,
                    &mut AsyncTaskRuntime::new(false),
                );
            }
            let mut closing = None;
            while let Ok(item) = urx.try_recv() {
                if let SessionOutbound::Update(n) = item {
                    let j = serde_json::to_value(&n.update).unwrap();
                    if j["sessionUpdate"] == "tool_call_update" && j["status"] == "failed" {
                        closing = Some(j);
                    }
                }
            }
            let closing = closing.expect("the unfinished call is closed failed");
            let text = closing["content"][0]["content"]["text"]
                .as_str()
                .unwrap_or("");
            let reply = reply_rx.await.expect("prompt resolved");
            if dispatched {
                assert!(text.contains("without returning a result"));
                assert!(matches!(
                    reply.map_err(|failure| failure.error),
                    Err(SessionError::PromptFailed {
                        kind: FailureKind::IncompleteToolCall,
                        ..
                    })
                ));
            } else {
                assert!(text.contains("before it ran"));
                assert!(matches!(reply, Ok(ref r) if r.stop_reason == "end_turn"));
            }
        }
    }

    /// An autonomous result must not wipe the live user turn's field tracker.
    #[tokio::test]
    async fn autonomous_result_keeps_tool_tracker() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, _urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let (mut text, mut err) = (false, false);
        let mut bg: HashMap<String, String> = HashMap::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        let mut frame = |f: Value, m: &mut TurnMachine, ms: &mut MapState| {
            test_handle_frame(
                f,
                m,
                ms,
                &mut bg,
                &mut pending,
                &mut text,
                &mut err,
                &utx,
                "s",
                &hw,
                &mut AsyncTaskRuntime::new(false),
            );
        };
        frame(
            json!({"type":"stream_event","event":{"type":"content_block_start","index":0,
                "content_block":{"type":"tool_use","id":"toolu_X","name":"Bash","input":{}}}}),
            &mut machine,
            &mut map_state,
        );
        assert_eq!(map_state.tracked_tool_calls(), 1);
        frame(
            json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
                "origin":{"kind":"task-notification"},"usage":{}}),
            &mut machine,
            &mut map_state,
        );
        assert_eq!(
            map_state.tracked_tool_calls(),
            1,
            "autonomous result keeps the tracker"
        );
        frame(
            json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","usage":{}}),
            &mut machine,
            &mut map_state,
        );
        assert_eq!(
            map_state.tracked_tool_calls(),
            0,
            "the user turn's result clears it"
        );
    }

    /// 3.12 (R71-10 / R86-6): a steer sent while a permission is awaiting the
    /// user goes out at `priority:"later"` (else `"now"`), and the steered turn
    /// then settles on the steer lane exactly like a `now` steer (echo, result,
    /// idle).
    #[tokio::test]
    async fn inv_43_steer_later_while_permission_open() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::timeout;

        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            Duration::from_secs(30),
            "s1".into(),
            Arc::new(AtomicUsize::new(0)),
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
        let (ltx, mut lrx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut reader = BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let _ = ltx.send(line.trim().to_string());
                    }
                }
            }
        });
        let (preply_tx, mut preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type":"user","uuid":"p1","message":{}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: preply_tx,
        })
        .unwrap();
        stream_tx
            .send(json!({"type":"user","uuid":"p1","isReplay":true,
                "message":{"role":"user","content":[{"type":"text","text":"hi"}]}}))
            .unwrap();
        // Steer #1 with NO permission open -> "now".
        let steer = |uuid: &str| {
            let (sreply_tx, sreply_rx) = oneshot::channel();
            tx.send(Command::Steer {
                uuid: uuid.into(),
                frame: json!({"type":"user","uuid":uuid,"message":{}}),
                is_local_only: false,
                is_prompt_required: false,
                reply: sreply_tx,
            })
            .unwrap();
            sreply_rx
        };
        assert_eq!(steer("s1").await.unwrap()["outcome"], "injected");
        // A permission now awaits the user (its request is never answered).
        let (reply_tx, _reply_rx) = oneshot::channel();
        tx.send(Command::Permission {
            frame: json!({"type":"control_request","request_id":"permreq1","request":{
                "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
                "tool_use_id":"toolu_P","permission_suggestions":[]}}),
            reply: reply_tx,
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(steer("s2").await.unwrap()["outcome"], "injected");

        let mut priorities = Vec::new();
        while priorities.len() < 2 {
            let line = timeout(Duration::from_secs(5), lrx.recv())
                .await
                .expect("a user frame is written")
                .expect("stdin open");
            let v: Value = serde_json::from_str(&line).expect("JSON");
            if matches!(v["uuid"].as_str(), Some("s1" | "s2")) {
                priorities.push((
                    v["uuid"].as_str().unwrap().to_string(),
                    v["priority"].clone(),
                ));
            }
        }
        assert_eq!(
            priorities,
            vec![
                ("s1".to_string(), json!("now")),
                ("s2".to_string(), json!("later"))
            ]
        );

        // The steer lane is the same as for `now`: both echoes, a result and
        // the idle settle p1 end_turn (not at the result).
        for uuid in ["s1", "s2"] {
            stream_tx
                .send(json!({"type":"user","uuid":uuid,"isReplay":true,
                    "message":{"role":"user","content":[{"type":"text","text":"x"}]}}))
                .unwrap();
        }
        // The permission call ran (a permission request records its call as
        // dispatched, `ensureToolCallEmitted`): its result comes before the
        // turn's, else the turn would end with it open (`incomplete_tool_call`).
        stream_tx
            .send(json!({"type":"user","parent_tool_use_id":null,
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_P","content":"ok"}]}}))
            .unwrap();
        stream_tx
            .send(json!({"type":"result","subtype":"success","is_error":false,
                "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            preply_rx.try_recv().is_err(),
            "settles at idle, not at the result"
        );
        stream_tx
            .send(json!({"type":"system","subtype":"session_state_changed","state":"idle"}))
            .unwrap();
        let reply = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("settles at the steer idle")
            .expect("resolved")
            .expect("Ok");
        assert_eq!(reply.stop_reason, "end_turn");
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
                on_request: Some(build_permission_request_handler(
                    tx.clone(),
                    crate::elicitation::ElicitationSupport::default(),
                )),
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
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
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
            prompt_text: String::new(),
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
        // permission round-trip, so the control handler answers with the SDK's
        // error verdict "Tool use aborted" (Node throws it for an aborted
        // signal, `acp-agent.ts:9791`) — not the R28 deny (P-36).
        let answer = find_control_response(&mut lrx).await;
        assert_eq!(answer["type"], "control_response");
        assert_eq!(answer["response"]["subtype"], "error");
        assert_eq!(answer["response"]["error"], json!("Tool use aborted"));
        assert!(answer["response"]["response"].is_null());

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

    /// G2c-fix 1 (INV-27): the client's permission answer is resolved by the
    /// actor and the answer command carries the `control_response` reply. A
    /// `Cancel` that aborts the permission task after the client answered must
    /// therefore still deliver the allow to the CLI: with the old `ack` the task
    /// suspended between the command and the reply, and the abort dropped the
    /// reply, so control.rs wrote the R28 deny while the client and the session
    /// state had already moved to the new mode (client/CLI divergence).
    #[tokio::test]
    async fn permission_answer_survives_cancel_between_the_command_and_the_reply() {
        use tokio::time::timeout;

        let mut map_state = MapState::default();
        let mut live_background: HashMap<String, String> = HashMap::new();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<Command>();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle_permission(
            &mut AsyncTaskRuntime::new(false),
            json!({"type":"control_request","request_id":"permreq1","request":{
                "subtype":"can_use_tool","tool_name":"ExitPlanMode",
                "input":{"plan":"x"},"tool_use_id":"toolu_1",
                "permission_suggestions":[]}}),
            reply_tx,
            None,
            &mut map_state,
            &mut live_background,
            &update_tx,
            "s",
            &mut pending_permissions,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &watch::channel(false).1,
            false,
            std::path::Path::new("/repo"),
            None,
            &[
                "default".to_string(),
                "acceptEdits".to_string(),
                "plan".to_string(),
                "auto".to_string(),
                "bypassPermissions".to_string(),
            ],
            None,
            None,
            None,
            &command_tx,
        );

        // The `session/request_permission` outbound; answer it with the manual
        // approve option (an allow + setMode default).
        let permission_reply = loop {
            let outbound = timeout(Duration::from_secs(5), update_rx.recv())
                .await
                .expect("a permission outbound is emitted")
                .expect("outbound stream open");
            if let SessionOutbound::RequestPermission { reply, .. } = outbound {
                break reply;
            }
        };
        permission_reply
            .send(Ok(
                json!({"outcome":{"outcome":"selected","optionId":"exit-plan-default"}}),
            ))
            .expect("the permission round-trip is waiting");

        // The permission task queues the answer and returns; the reply lives in
        // the command, not in the task.
        let answer = timeout(Duration::from_secs(5), command_rx.recv())
            .await
            .expect("the answer command is queued")
            .expect("command stream open");
        let Command::PermissionAnswered {
            value,
            ctx,
            request_id,
            tool_use_id,
            tool_input,
            reply,
        } = answer
        else {
            panic!("the client's answer must be resolved by the actor");
        };

        // `Command::Cancel` aborts every pending permission task. The task has
        // already returned, and the reply must survive the abort.
        for handle in pending_permissions.values() {
            handle.abort();
        }
        pending_permissions.clear();

        // The actor resolves the answer after the abort: allow + mode update.
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        let response = resolve_permission_answer(
            PermissionAnswer {
                value: &value,
                ctx: &ctx,
                request_id: &request_id,
                tool_use_id: &tool_use_id,
                tool_input: &tool_input,
            },
            &mut ext,
            &update_tx,
            "s",
        );
        assert_eq!(response["response"]["response"]["behavior"], "allow");
        assert_eq!(
            response["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"default","destination":"session"}])
        );
        reply
            .send(response)
            .expect("the reply must survive the aborted permission task");
        let delivered = timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("the CLI reply resolves")
            .expect("the reply is the allow");
        assert_eq!(
            delivered["response"]["response"]["behavior"], "allow",
            "the CLI must still receive the allow after the cancel"
        );
        // The mode update reached the client (the client and the CLI agree).
        let mut saw_mode = false;
        while let Ok(outbound) = update_rx.try_recv() {
            if let SessionOutbound::RawUpdate(v) = outbound {
                if v["update"]["sessionUpdate"] == "current_mode_update" {
                    assert_eq!(v["update"]["currentModeId"], "default");
                    saw_mode = true;
                }
            }
        }
        assert!(saw_mode, "the ExitPlanMode mode update is published");
    }

    /// G2c-fix 3 (Node `acp-agent.ts:10219`): `isAutoUnavailable` is evaluated
    /// when the client's answer arrives, against the model current at that
    /// moment — not when the request was created. A model switch while the
    /// dialog is open must therefore rewrite the Auto allow to Accept edits and
    /// publish the advisory.
    #[tokio::test]
    async fn auto_fallback_uses_the_model_current_when_the_client_answers() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::timeout;

        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let (_stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            Duration::from_secs(30),
            "s1".into(),
            hw,
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
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

        let config_for =
            |model: &str, supports_auto: bool| crate::session_config::SessionConfigState {
                modes: crate::session_config::ModesState {
                    current_mode_id: "default".into(),
                    available_modes: [
                        "default",
                        "acceptEdits",
                        "plan",
                        "auto",
                        "bypassPermissions",
                    ]
                    .into_iter()
                    .map(|id| crate::session_config::ModeInfo {
                        id: id.into(),
                        name: id.into(),
                        description: String::new(),
                        _meta: None,
                    })
                    .collect(),
                },
                current_model_id: model.into(),
                model_infos: vec![crate::session_config::ModelInfo {
                    value: model.into(),
                    display_name: model.into(),
                    supports_auto_mode: Some(supports_auto),
                    ..Default::default()
                }],
                ..Default::default()
            };
        let sync = || {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(Command::GetState { reply: reply_tx }).unwrap();
            reply_rx
        };
        // The session starts on a model that supports Auto.
        tx.send(Command::InitConfig {
            config: Box::new(config_for("model-with-auto", true)),
        })
        .unwrap();
        timeout(Duration::from_secs(5), sync())
            .await
            .expect("InitConfig is processed")
            .expect("GetState reply")
            .expect("state");

        // A permission request is created while the model supports Auto.
        let (preply_tx, preply_rx) = oneshot::channel();
        tx.send(Command::Permission {
            frame: json!({"type":"control_request","request_id":"permreq1","request":{
                "subtype":"can_use_tool","tool_name":"ExitPlanMode",
                "input":{"plan":"x"},"tool_use_id":"toolu_1",
                "permission_suggestions":[]}}),
            reply: preply_tx,
        })
        .unwrap();
        let permission_reply = loop {
            let outbound = timeout(Duration::from_secs(5), update_rx.recv())
                .await
                .expect("a permission outbound is emitted")
                .expect("outbound stream open");
            if let SessionOutbound::RequestPermission { reply, .. } = outbound {
                break reply;
            }
        };

        // While the dialog is open the model switches to one without Auto.
        tx.send(Command::InitConfig {
            config: Box::new(config_for("model-without-auto", false)),
        })
        .unwrap();
        timeout(Duration::from_secs(5), sync())
            .await
            .expect("the second InitConfig is processed")
            .expect("GetState reply")
            .expect("state");

        // The client picks Auto after the switch: the answer-time model lacks
        // it, so Node rewrites to Accept edits and warns.
        permission_reply
            .send(Ok(
                json!({"outcome":{"outcome":"selected","optionId":"exit-plan-auto"}}),
            ))
            .expect("the permission round-trip is waiting");
        let response = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("the permission answer resolves")
            .expect("reply resolved");
        assert_eq!(response["response"]["response"]["behavior"], "allow");
        assert_eq!(
            response["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"acceptEdits","destination":"session"}]),
            "the Auto allow is rewritten against the model current at answer time"
        );

        let mut updates = Vec::new();
        while let Ok(outbound) = update_rx.try_recv() {
            if let SessionOutbound::RawUpdate(v) = outbound {
                updates.push(v);
            }
        }
        let warning = updates
            .iter()
            .position(|v| v["update"]["sessionUpdate"] == "agent_message_chunk")
            .expect("the fallback warning is published");
        let mode = updates
            .iter()
            .position(|v| {
                v["update"]["sessionUpdate"] == "current_mode_update"
                    && v["update"]["currentModeId"] == "acceptEdits"
            })
            .expect("current_mode_update acceptEdits");
        assert!(
            warning < mode,
            "Node publishes the warning before the mode update"
        );
    }

    /// G12 (4a) — the ExitPlanMode answer/cancel race through the REAL run loop
    /// (not the pre-built `Command::PermissionAnswered`): answer
    /// `exit-plan-default`, let the actor publish the mode updates and write the
    /// allow to the CLI, then deliver `Command::Cancel`. The CLI must still have
    /// received the allow (the answer command owns `reply`) and the session must
    /// hold `default` in its live mode state.
    #[tokio::test]
    async fn exit_plan_answer_survives_cancel_in_the_run_loop() {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::timeout;

        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(
            a,
            control_in_rx,
            ControlOptions {
                on_request: Some(build_permission_request_handler(
                    tx.clone(),
                    crate::elicitation::ElicitationSupport::default(),
                )),
            },
        );
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let (_stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
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
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
        let (ltx, mut lrx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut reader = BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    let _ = ltx.send(trimmed.to_string());
                }
            }
        });

        let modes = [
            "default",
            "acceptEdits",
            "plan",
            "auto",
            "bypassPermissions",
        ]
        .into_iter()
        .map(|id| crate::session_config::ModeInfo {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            _meta: None,
        })
        .collect();
        tx.send(Command::InitConfig {
            config: Box::new(crate::session_config::SessionConfigState {
                modes: crate::session_config::ModesState {
                    current_mode_id: "default".into(),
                    available_modes: modes,
                },
                current_model_id: "model-with-auto".into(),
                model_infos: vec![crate::session_config::ModelInfo {
                    value: "model-with-auto".into(),
                    display_name: "model-with-auto".into(),
                    supports_auto_mode: Some(true),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        })
        .unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Command::GetState { reply: reply_tx }).unwrap();
        timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("InitConfig is processed")
            .expect("GetState reply")
            .expect("state");

        // An active turn so the later cancel has something to settle.
        let (preply_tx, preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: preply_tx,
        })
        .unwrap();
        let _echo = timeout(Duration::from_secs(5), lrx.recv())
            .await
            .expect("user frame written to stdin")
            .expect("stream open");

        // The real inbound path: the control handler routes `can_use_tool` to
        // the actor, which spawns the permission round-trip.
        control_in_tx
            .send(json!({
                "type": "control_request",
                "request_id": "permreq1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "ExitPlanMode",
                    "input": {"plan": "x"},
                    "tool_use_id": "toolu_1",
                    "agent_id": null,
                    "permission_suggestions": []
                }
            }))
            .unwrap();
        let permission_reply = loop {
            let outbound = timeout(Duration::from_secs(5), update_rx.recv())
                .await
                .expect("a permission outbound is emitted")
                .expect("outbound stream open");
            if let SessionOutbound::RequestPermission { reply, .. } = outbound {
                break reply;
            }
        };
        permission_reply
            .send(Ok(
                json!({"outcome":{"outcome":"selected","optionId":"exit-plan-default"}}),
            ))
            .expect("the permission round-trip is waiting");

        // The CLI must receive the allow BEFORE the cancel.
        let allow = loop {
            let line = timeout(Duration::from_secs(5), lrx.recv())
                .await
                .expect("a control_response is written to stdin")
                .expect("stream open");
            let value: Value = serde_json::from_str(&line).expect("line is JSON");
            if value.get("type").and_then(Value::as_str) == Some("control_response")
                && value
                    .pointer("/response/response/behavior")
                    .and_then(Value::as_str)
                    == Some("allow")
            {
                break value;
            }
        };
        assert_eq!(
            allow["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"default","destination":"session"}]),
            "the CLI receives the manual-approve setMode allow"
        );

        // A cancel arriving after the answer must not undo the mode change.
        tx.send(Command::Cancel).unwrap();
        let (state_tx, state_rx) = oneshot::channel();
        tx.send(Command::GetState { reply: state_tx }).unwrap();
        let (live_modes, _) = timeout(Duration::from_secs(5), state_rx)
            .await
            .expect("the cancel and GetState are processed")
            .expect("GetState reply")
            .expect("state");
        assert_eq!(
            live_modes.current_mode_id, "default",
            "the answered ExitPlanMode mode survives the cancel"
        );

        let mut saw_mode_update = false;
        while let Ok(outbound) = update_rx.try_recv() {
            if let SessionOutbound::RawUpdate(value) = outbound {
                if value["update"]["sessionUpdate"] == "current_mode_update"
                    && value["update"]["currentModeId"] == "default"
                {
                    saw_mode_update = true;
                }
            }
        }
        assert!(
            saw_mode_update,
            "the answered ExitPlanMode publishes current_mode_update default"
        );

        let reply = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("the prompt turn settles after the cancel")
            .expect("prompt reply resolved")
            .expect("the prompt turn settles Ok");
        assert_eq!(reply.stop_reason, "cancelled");
    }

    /// G12 (4b) — the Auto->acceptEdits advisory (`publishFallbackWarning`) is
    /// published at most ONCE per session: a second rewrite is applied but not
    /// announced again. This is the session-level guard for review-G2c finding 4
    /// (the warning is not tied to ExitPlanMode; the rewrite detection itself is
    /// tool-independent — `permission::tests::fallback_rewrite_applies_to_any_tool`).
    /// Today only ExitPlanMode offers a `setMode auto` option, so the reachable
    /// rewrite is used twice here.
    #[tokio::test]
    async fn fallback_warning_is_published_once_per_session() {
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        ext.config = crate::session_config::SessionConfigState {
            modes: crate::session_config::ModesState {
                current_mode_id: "default".into(),
                available_modes: [
                    "default",
                    "acceptEdits",
                    "plan",
                    "auto",
                    "bypassPermissions",
                ]
                .into_iter()
                .map(|id| crate::session_config::ModeInfo {
                    id: id.into(),
                    name: id.into(),
                    description: String::new(),
                    _meta: None,
                })
                .collect(),
            },
            current_model_id: "model-without-auto".into(),
            model_infos: vec![crate::session_config::ModelInfo {
                value: "model-without-auto".into(),
                display_name: "model-without-auto".into(),
                supports_auto_mode: Some(false),
                ..Default::default()
            }],
            ..Default::default()
        };
        let ctx = crate::permission_options::OptionsContext {
            tool_name: "ExitPlanMode".to_string(),
            display_name: None,
            input: json!({"plan": "x"}),
            cwd: PathBuf::from("/repo"),
            change_set: None,
            no_persistent_rule: false,
            default_to_no: false,
            available_modes: vec![
                "default".into(),
                "acceptEdits".into(),
                "plan".into(),
                "auto".into(),
                "bypassPermissions".into(),
            ],
            auto_unavailable: false,
            pre_plan_mode: None,
            context_used_percent: None,
            home_dir: None,
        };
        let input = json!({"plan": "x"});
        let value = json!({"outcome":{"outcome":"selected","optionId":"exit-plan-auto"}});
        let answer = |request_id: &'static str| PermissionAnswer {
            value: &value,
            ctx: &ctx,
            request_id,
            tool_use_id: "toolu_1",
            tool_input: &input,
        };

        let response = resolve_permission_answer(answer("r1"), &mut ext, &update_tx, "s1");
        assert_eq!(
            response["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"acceptEdits","destination":"session"}]),
            "the Auto allow is rewritten to Accept edits"
        );
        let mut warnings = 0;
        while let Ok(outbound) = update_rx.try_recv() {
            if let SessionOutbound::RawUpdate(value) = outbound {
                if value["update"]["sessionUpdate"] == "agent_message_chunk" {
                    warnings += 1;
                }
            }
        }
        assert_eq!(
            warnings, 1,
            "the advisory is published on the first rewrite"
        );

        let response = resolve_permission_answer(answer("r2"), &mut ext, &update_tx, "s1");
        assert_eq!(
            response["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"acceptEdits","destination":"session"}]),
            "the second Auto allow is rewritten too"
        );
        let mut warnings = 0;
        while let Ok(outbound) = update_rx.try_recv() {
            if let SessionOutbound::RawUpdate(value) = outbound {
                if value["update"]["sessionUpdate"] == "agent_message_chunk" {
                    warnings += 1;
                }
            }
        }
        assert_eq!(
            warnings, 0,
            "the advisory is not repeated on a second rewrite"
        );
    }

    /// G12 (4b, guard) — a non-ExitPlanMode tool whose CLI suggestion is
    /// `setMode auto` offers no option that can carry it, so the client cannot
    /// silently activate Auto there (Node `decodeClaudePermissionResponse`
    /// rejects an unoffered option). The fallback rewrite is unreachable through
    /// the offered-options path; the generic detection is covered by
    /// `permission::tests::fallback_rewrite_applies_to_any_tool`.
    #[tokio::test]
    async fn non_exit_plan_mode_setmode_auto_is_not_offered() {
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        ext.config = crate::session_config::SessionConfigState {
            current_model_id: "model-without-auto".into(),
            model_infos: vec![crate::session_config::ModelInfo {
                value: "model-without-auto".into(),
                display_name: "model-without-auto".into(),
                supports_auto_mode: Some(false),
                ..Default::default()
            }],
            ..Default::default()
        };
        let ctx = crate::permission_options::OptionsContext {
            tool_name: "Bash".to_string(),
            display_name: None,
            input: json!({"command": "echo hi"}),
            cwd: PathBuf::from("/repo"),
            change_set: Some(crate::permission_options::ChangeSet {
                updates: vec![json!({"type":"setMode","mode":"auto","destination":"session"})],
            }),
            no_persistent_rule: false,
            default_to_no: false,
            available_modes: vec!["default".into(), "acceptEdits".into(), "auto".into()],
            auto_unavailable: false,
            pre_plan_mode: None,
            context_used_percent: None,
            home_dir: None,
        };
        let input = json!({"command": "echo hi"});
        let value = json!({"outcome":{"outcome":"selected","optionId":"allow-with-updates"}});
        let response = resolve_permission_answer(
            PermissionAnswer {
                value: &value,
                ctx: &ctx,
                request_id: "r1",
                tool_use_id: "toolu_1",
                tool_input: &input,
            },
            &mut ext,
            &update_tx,
            "s1",
        );
        assert_eq!(
            response["response"]["subtype"], "error",
            "an option that was never offered is the SDK's error verdict, so no silent Auto rewrite"
        );
        assert_eq!(
            response["response"]["error"],
            json!("Permission option was not offered: allow-with-updates")
        );
    }

    /// Review follow-up 3.17-D — the cancel's `cancel_async_message` frames go
    /// out in `take_abandoned()` ORDER with the EXACT uuid VALUES on the wire
    /// (2.13 / E-8), and the `interrupt` follows all of them for 2+ uuids.
    /// Drives the REAL actor + Control over a duplex stdin (like `inv_27`): an
    /// active echoed turn plus two queued turns, then a cancel.
    #[tokio::test]
    async fn inv_38_cancel_async_message_wire_order() {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::timeout;

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
            control_in_tx.clone(),
            update_tx,
            Duration::from_secs(30),
            "s1".into(),
            hw,
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
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

        // Active turn p1 (echoed), then two queued turns A and B (FIFO).
        let (r_p1, _) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type":"user","uuid":"p1","message":{}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: r_p1,
        })
        .unwrap();
        stream_tx
            .send(json!({"type":"user","uuid":"p1","message":{}}))
            .unwrap();
        let (r_a, _) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "A".into(),
            frame: json!({"type":"user","uuid":"A","message":{}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: r_a,
        })
        .unwrap();
        let (r_b, _) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "B".into(),
            frame: json!({"type":"user","uuid":"B","message":{}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: r_b,
        })
        .unwrap();
        // Give the actor a beat to enqueue A and B before the cancel lands.
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(Command::Cancel).unwrap();

        // Collect the `cancel_async_message` uuids in wire order, then confirm
        // the `interrupt` follows all of them.
        let mut uuids: Vec<String> = Vec::new();
        let saw_interrupt;
        loop {
            let line = timeout(Duration::from_secs(5), lrx.recv())
                .await
                .expect("a control_request is written to stdin")
                .expect("stream open");
            let v: Value = serde_json::from_str(&line).expect("line is JSON");
            if v.get("type").and_then(Value::as_str) != Some("control_request") {
                continue;
            }
            let subtype = v
                .pointer("/request/subtype")
                .and_then(Value::as_str)
                .unwrap_or("");
            if subtype == "cancel_async_message" {
                let mu = v
                    .pointer("/request/message_uuid")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                uuids.push(mu);
            } else if subtype == "interrupt" {
                saw_interrupt = true;
                break;
            }
        }
        assert_eq!(
            uuids,
            vec!["A".to_string(), "B".to_string()],
            "the cancel_async_message message_uuids must go out in take_abandoned() order (FIFO queued sweep)"
        );
        assert!(
            saw_interrupt,
            "the interrupt must follow all of the cancel_async_message frames on the wire"
        );
    }

    /// Read stdin lines until the R28 deny `control_response` is seen.
    /// The next `control_response` on the child's stdin, whatever its verdict.
    async fn find_control_response(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
        use std::time::Duration;
        use tokio::time::timeout;
        loop {
            let line = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a control_response is written to stdin")
                .expect("stream open");
            let v: Value = serde_json::from_str(&line).expect("line is JSON");
            if v.get("type").and_then(Value::as_str) == Some("control_response") {
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;

        // The subagent starts, spawned by the Agent/Task call `toolu_PARENT`.
        test_handle_frame(
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
            &mut AsyncTaskRuntime::new(false),
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
        let (command_tx, _command_rx) = mpsc::unbounded_channel::<Command>();
        handle_permission(
            &mut AsyncTaskRuntime::new(false),
            frame,
            reply_tx,
            None,
            &mut map_state,
            &mut live_background,
            &utx,
            "s",
            &mut pending_permissions,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &watch::channel(false).1,
            false,
            std::path::Path::new(""),
            None,
            &[],
            None,
            None,
            None,
            &command_tx,
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
    /// force-cancel grace, returning `(command_tx, stream_tx, high_water)` to
    /// drive it. Used by the phase-11 force-cancel / cancel-idempotency tests
    /// (INV-17, INV-22, 11.T5) and the phase-13 one-active-turn test (INV-14).
    fn spawn_actor(
        grace: Duration,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedSender<Value>,
        Arc<AtomicUsize>,
    ) {
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
            hw.clone(),
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
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
        (tx, stream_tx, hw)
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
        let (tx, stream_tx, _hw) = spawn_actor(GRACE);
        let (preply_tx, preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
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

    /// rust-air-gaps 2.3 (`inv_52_close_with_turn_in_flight`) — `session/close`
    /// with an active turn settles that turn `cancelled`, answers behind the
    /// flush barrier and ends the actor (a later command finds it gone).
    #[tokio::test]
    async fn inv_52_close_with_turn_in_flight() {
        use tokio::time::timeout;
        let (tx, stream_tx, _hw) = spawn_actor(Duration::from_secs(60));
        let (preply_tx, preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: preply_tx,
        })
        .unwrap();
        stream_tx
            .send(json!({"type": "user", "uuid": "p1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (close_tx, close_rx) = oneshot::channel();
        tx.send(Command::Close { reply: close_tx }).unwrap();
        let reply = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("the in-flight prompt settles at close")
            .expect("prompt reply resolved")
            .expect("the turn settles Ok");
        assert_eq!(reply.stop_reason, "cancelled");
        timeout(Duration::from_secs(5), close_rx)
            .await
            .expect("close answers")
            .expect("close reply sent");
        // The actor has ended: its command channel is closed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (late_tx, _late_rx) = oneshot::channel();
        assert!(
            tx.send(Command::Close { reply: late_tx }).is_err(),
            "the actor must have ended after close"
        );
    }

    /// T8B — closing one session does not affect another live session: the
    /// closed actor ends, the other still activates and settles a prompt.
    #[tokio::test]
    async fn t8b_close_leaves_another_session_running() {
        use tokio::time::timeout;
        let (tx_a, _stream_a, _hw_a) = spawn_actor(Duration::from_secs(60));
        let (tx_b, stream_b, _hw_b) = spawn_actor(Duration::from_secs(60));

        let (close_tx, close_rx) = oneshot::channel();
        tx_a.send(Command::Close { reply: close_tx }).unwrap();
        timeout(Duration::from_secs(5), close_rx)
            .await
            .expect("the close answers")
            .expect("close reply sent");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (late_tx, _late_rx) = oneshot::channel();
        assert!(
            tx_a.send(Command::Close { reply: late_tx }).is_err(),
            "the closed actor ended"
        );

        let (preply_tx, preply_rx) = oneshot::channel();
        tx_b.send(Command::Prompt {
            uuid: "p2".into(),
            frame: json!({"type": "user", "uuid": "p2", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: preply_tx,
        })
        .unwrap();
        stream_b
            .send(json!({"type": "user", "uuid": "p2", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        stream_b
            .send(
                json!({"type": "result", "subtype": "success", "is_error": false,
                         "stop_reason": "end_turn",
                         "usage": {"input_tokens": 1, "output_tokens": 1}}),
            )
            .unwrap();
        let reply = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("the other session settles")
            .expect("prompt reply resolved")
            .expect("the turn settles Ok");
        assert_eq!(reply.stop_reason, "end_turn");
    }

    /// V-11 — closing the connection tears down EVERY session (`dispose()`): each
    /// live session gets its close (cancel + interrupt + end the actor), a session
    /// that already ended returns at once, and one wedged session cannot hold the
    /// others beyond the bound.
    #[tokio::test]
    async fn v11_dispose_closes_every_session() {
        let handle = |tx: mpsc::UnboundedSender<Command>| {
            let (a, _b) = tokio::io::duplex(1024);
            let (_control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
            Session {
                tx,
                high_water: Arc::new(AtomicUsize::new(0)),
                session_id: "s".into(),
                commands: Vec::new(),
                account: None,
                control: Control::spawn(a, control_in_rx, ControlOptions { on_request: None }),
                modes: crate::session_config::ModesState {
                    current_mode_id: "default".into(),
                    available_modes: Vec::new(),
                },
                config_options: Vec::new(),
                final_state: watch::channel(None).1,
            }
        };
        // Two live sessions whose stub actors answer a Close, and one already ended.
        let mut closed = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
            let (seen_tx, seen_rx) = oneshot::channel();
            tokio::spawn(async move {
                if let Some(Command::Close { reply }) = rx.recv().await {
                    let _ = seen_tx.send(());
                    let _ = reply.send(());
                }
            });
            closed.push(seen_rx);
            handles.push(handle(tx));
        }
        let (gone_tx, gone_rx) = mpsc::unbounded_channel::<Command>();
        drop(gone_rx);
        handles.push(handle(gone_tx));

        tokio::time::timeout(
            Duration::from_secs(10),
            crate::agent::dispose_sessions(&handles),
        )
        .await
        .expect("dispose finishes");
        for seen in closed {
            tokio::time::timeout(Duration::from_secs(1), seen)
                .await
                .expect("every live session got its close")
                .expect("close seen");
        }
    }

    /// V-08 / V-07 — teardown cancels every turn and answers only after their ends
    /// went out: the active turn settles `cancelled` WITH a usage object (it does
    /// not wait for the interrupt's reply or for its interrupted result), the
    /// queued turn is swept `cancelled` without usage, and the close answers behind
    /// the flush barrier after both prompts resolved.
    #[tokio::test]
    async fn v08_close_waits_for_cancelled_turns_then_settles() {
        use tokio::time::timeout;
        let (tx, stream_tx, _hw) = spawn_actor(Duration::from_secs(60));
        let mut replies = Vec::new();
        for uuid in ["p1", "p2"] {
            let (preply_tx, preply_rx) = oneshot::channel();
            tx.send(Command::Prompt {
                uuid: uuid.into(),
                frame: json!({"type": "user", "uuid": uuid, "message": {}}),
                is_local_only: false,
                prompt_text: String::new(),
                reply: preply_tx,
            })
            .unwrap();
            replies.push(preply_rx);
        }
        stream_tx
            .send(json!({"type": "user", "uuid": "p1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (close_tx, close_rx) = oneshot::channel();
        tx.send(Command::Close { reply: close_tx }).unwrap();
        let p2 = timeout(Duration::from_secs(5), replies.pop().unwrap())
            .await
            .expect("the queued turn settles")
            .expect("reply")
            .expect("ok");
        assert_eq!(p2.stop_reason, "cancelled");
        assert!(p2.usage.is_none(), "a swept queued turn reports no usage");
        let p1 = timeout(Duration::from_secs(5), replies.pop().unwrap())
            .await
            .expect("the active turn settles")
            .expect("reply")
            .expect("ok");
        assert_eq!(p1.stop_reason, "cancelled");
        assert!(p1.usage.is_some(), "an active turn reports usage");
        timeout(Duration::from_secs(5), close_rx)
            .await
            .expect("close answers")
            .expect("close reply sent");
    }

    /// 2.7 — a `_session/steering` steer marked on the active turn defers p1's
    /// settlement to the steer idle lane: the first result alone does NOT settle
    /// p1 (it is steered), and it only resolves `end_turn` after the steered
    /// echo, a second result and the idle. The assertion that p1 is still
    /// pending after the first result FAILS if the `mark_steered` call in the
    /// actor is removed, because then the un-steered first result settles p1
    /// `end_turn` immediately.
    #[tokio::test]
    async fn steer_injected_settles_at_idle() {
        use tokio::time::timeout;
        let (tx, stream_tx, _hw) = spawn_actor(Duration::from_secs(60));
        let (preply_tx, mut preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: preply_tx,
        })
        .unwrap();
        // p1 becomes active once echoed.
        stream_tx
            .send(json!({"type": "user", "uuid": "p1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        // Drain: let the actor process the prompt + echo so p1 is active.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Inject a steer; the actor marks p1 steered and replies `injected`.
        let (sreply_tx, sreply_rx) = oneshot::channel();
        tx.send(Command::Steer {
            uuid: "s1".into(),
            frame: json!({"type": "user", "uuid": "s1", "message": {}}),
            is_local_only: false,
            is_prompt_required: false,
            reply: sreply_tx,
        })
        .unwrap();
        let sreply = timeout(Duration::from_secs(5), sreply_rx)
            .await
            .expect("the steer reply must resolve")
            .expect("steer reply sent");
        assert_eq!(sreply["outcome"], json!("injected"));
        tokio::time::sleep(Duration::from_millis(50)).await;

        // First result while steered must NOT settle p1 `end_turn` yet.
        stream_tx
            .send(
                json!({"type": "result", "subtype": "success", "is_error": false,
                         "stop_reason": "end_turn",
                         "usage": {"input_tokens": 1, "output_tokens": 1}}),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            preply_rx.try_recv().is_err(),
            "p1 must still be pending after the first (steered) result — this \
             fails if the mark_steered call is removed, because the un-steered \
             result would settle p1 end_turn immediately"
        );

        // Steered echo, then a second result carrying the winning usage.
        stream_tx
            .send(json!({"type": "user", "uuid": "s1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "be brief"}]}}))
            .unwrap();
        stream_tx
            .send(
                json!({"type": "result", "subtype": "success", "is_error": false,
                         "stop_reason": "end_turn",
                         "usage": {"input_tokens": 2, "output_tokens": 2}}),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            preply_rx.try_recv().is_err(),
            "p1 must still be pending after the steered echo and second result \
             (it settles at the idle)"
        );

        // The idle settles the steer lane and p1 resolves end_turn.
        stream_tx
            .send(json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}))
            .unwrap();
        let r = timeout(Duration::from_secs(5), preply_rx)
            .await
            .expect("p1 must settle at the steer idle")
            .expect("p1 resolved")
            .expect("p1 Ok");
        assert_eq!(r.stop_reason, "end_turn");
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

        let (tx, stream_tx, _hw) = spawn_actor(G);
        let (preply_tx, mut preply_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
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
        let (tx, stream_tx, _hw) = spawn_actor(Duration::from_millis(300));

        // p1: active, cancel, then the trailing idle settles it `cancelled`.
        let (p1_tx, p1_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
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
            prompt_text: String::new(),
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

    /// 13.T3 (INV-14) — at most ONE active turn per session at every instant.
    /// N prompts on one actor, each echoed and settled in order; the
    /// `high_water` active-turn counter must never exceed 1 (the phase-5 test
    /// was removed in the phase-8 rework; restored here against the current
    /// actor).
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_14_one_active_turn() {
        use tokio::time::timeout;
        let (tx, stream_tx, hw) = spawn_actor(Duration::from_millis(300));
        const N: usize = 10;

        let mut replies = Vec::new();
        for i in 0..N {
            let uuid = format!("p{i}");
            let (rtx, rrx) = oneshot::channel();
            tx.send(Command::Prompt {
                uuid: uuid.clone(),
                frame: json!({"type": "user", "uuid": uuid, "message": {}}),
                is_local_only: false,
                prompt_text: String::new(),
                reply: rtx,
            })
            .unwrap();
            replies.push(rrx);
            // Echo activates this turn; a result settles it — the next turn can
            // then activate. Turns activate strictly one at a time.
            stream_tx
                .send(json!({"type": "user", "uuid": uuid, "isReplay": true,
                             "message": {"role": "user", "content": [{"type": "text", "text": format!("m{i}")}]}}))
                .unwrap();
            stream_tx
                .send(
                    json!({"type": "result", "subtype": "success", "is_error": false,
                             "stop_reason": "end_turn",
                             "usage": {"input_tokens": 1, "output_tokens": 1}}),
                )
                .unwrap();
            // Let the actor catch up so the i-th turn fully settles before the
            // next prompt is enqueued (avoids queueing all N up front).
            tokio::task::yield_now().await;
        }

        let mut reasons = Vec::new();
        for rrx in replies {
            let reply = timeout(Duration::from_secs(5), rrx)
                .await
                .expect("prompt settles (no hang)")
                .expect("prompt resolved")
                .expect("prompt Ok");
            reasons.push(reply.stop_reason);
        }
        assert_eq!(reasons.len(), N, "all prompts settle");
        assert!(
            reasons.iter().all(|r| r == "end_turn"),
            "all turns settle end_turn: {reasons:?}"
        );
        assert_eq!(
            hw.load(Ordering::Relaxed),
            1,
            "INV-14: at most ONE active turn per session (high-water == 1)"
        );
    }

    /// 13.T3 (INV-15) — a cancel racing the session loop's idle `recv` loses no
    /// queued stream message. The actor's single `biased` select must process a
    /// stream message that is already queued when the cancel arrives, never
    /// dropping it: an active turn that has a pending result must still settle
    /// on it (not be left to hang by a cancel that ate the result), and the
    /// actor must remain usable for a follow-up prompt. Restored against the
    /// current actor (the phase-5 test was removed in the phase-8 rework).
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_15_cancel_loses_nothing() {
        use tokio::time::timeout;
        let (tx, stream_tx, _hw) = spawn_actor(Duration::from_millis(300));

        // p1 becomes active.
        let (p1_tx, p1_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p1".into(),
            frame: json!({"type": "user", "uuid": "p1", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: p1_tx,
        })
        .unwrap();
        stream_tx
            .send(json!({"type": "user", "uuid": "p1", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
            .unwrap();
        tokio::task::yield_now().await;

        // Queue a result + trailing idle, then race a cancel right behind them.
        // The result is already in the stream queue when the cancel lands. The
        // actor's `biased` select serves the cancel command first (the command
        // branch wins over the stream branch), so p1 settles `cancelled` and the
        // queued result is reconciled as an orphan — the invariant is that the
        // message is NOT lost: p1 still settles (no hang) and the actor stays
        // alive to process the orphan and a follow-up prompt.
        stream_tx
            .send(
                json!({"type": "result", "subtype": "success", "is_error": false,
                         "stop_reason": "end_turn",
                         "usage": {"input_tokens": 1, "output_tokens": 1}}),
            )
            .unwrap();
        stream_tx
            .send(json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}))
            .unwrap();
        tx.send(Command::Cancel).unwrap();

        let r1 = timeout(Duration::from_secs(5), p1_rx)
            .await
            .expect("p1 settles (queued result not lost, no hang)")
            .expect("p1 resolved")
            .expect("p1 Ok");
        // The race winner is nondeterministic: the `biased` select serves the
        // cancel command when both are ready at the same instant (→ cancelled),
        // but if the result+idle are already queued when the loop reaches the
        // select the stream branch wins (→ end_turn). Either way the turn
        // SETTLES — the invariant is that the racing cancel loses no queued
        // message and leaves nothing hanging — and the orphaned/consumed result
        // never corrupts the actor.
        assert!(
            r1.stop_reason == "end_turn" || r1.stop_reason == "cancelled",
            "a racing cancel must settle the active turn (end_turn or cancelled), got {} (INV-15)",
            r1.stop_reason
        );

        // The actor is still alive for a follow-up prompt (the cancel did not
        // tear it down and the orphaned result did not corrupt it).
        let (p2_tx, p2_rx) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "p2".into(),
            frame: json!({"type": "user", "uuid": "p2", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: p2_tx,
        })
        .unwrap();
        stream_tx
            .send(json!({"type": "user", "uuid": "p2", "isReplay": true,
                         "message": {"role": "user", "content": [{"type": "text", "text": "second"}]}}))
            .unwrap();
        stream_tx
            .send(
                json!({"type": "result", "subtype": "success", "is_error": false,
                         "stop_reason": "end_turn",
                         "usage": {"input_tokens": 1, "output_tokens": 1}}),
            )
            .unwrap();
        let r2 = timeout(Duration::from_secs(5), p2_rx)
            .await
            .expect("follow-up prompt settles")
            .expect("resolved")
            .expect("Ok");
        assert_eq!(r2.stop_reason, "end_turn");
    }

    #[tokio::test]
    async fn inv_61_result_without_prior_assistant_usage_emits_nothing() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );

        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        // Result without prior assistant/stream frame
        handle_session_frame(
            json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "stop_reason": "end_turn",
                "result": "ok",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        // Assert NO usage_update was emitted
        let mut saw_usage_update = false;
        while let Ok(msg) = urx.try_recv() {
            if let SessionOutbound::RawUpdate(v) = msg {
                if v.pointer("/update/sessionUpdate").and_then(Value::as_str)
                    == Some("usage_update")
                {
                    saw_usage_update = true;
                }
            }
        }
        assert!(
            !saw_usage_update,
            "result without prior assistant usage must emit no usage_update"
        );
    }

    #[tokio::test]
    async fn inv_61_compact_boundary_post_tokens() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );

        // Send compact_boundary system frame
        handle_session_frame(
            json!({
                "type": "system",
                "subtype": "compact_boundary",
                "compact_metadata": {
                    "post_tokens": 42000
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        assert_eq!(ext.usage.last_total, Some(42000));
        assert_eq!(ext.usage.used, 42000);

        let mut saw_usage_update = false;
        while let Ok(msg) = urx.try_recv() {
            if let SessionOutbound::RawUpdate(v) = msg {
                if v.pointer("/update/sessionUpdate").and_then(Value::as_str)
                    == Some("usage_update")
                {
                    assert_eq!(
                        v.pointer("/update/used").and_then(Value::as_u64),
                        Some(42000)
                    );
                    assert_eq!(
                        v.pointer("/update/size").and_then(Value::as_u64),
                        Some(200_000)
                    );
                    saw_usage_update = true;
                }
            }
        }
        assert!(
            saw_usage_update,
            "compact_boundary must emit a usage_update with post_tokens"
        );
    }

    #[tokio::test]
    async fn inv_61_subagent_never_touches_usage_state() {
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut live_background: HashMap<String, String> = HashMap::new();
        let mut tasks = AsyncTaskRuntime::new(false);
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );

        // Subagent assistant message (parent_tool_use_id is set)
        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": "toolu_parent_123",
                "message": {
                    "id": "sub_msg_1",
                    "model": "claude-haiku-3-5",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "subagent work"}],
                    "usage": { "input_tokens": 100, "output_tokens": 50 }
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        assert!(ext.usage.last_usage.is_none());
        assert!(ext.usage.last_total.is_none());
        assert!(ext.usage.last_model.is_none());

        // Subagent stream_event message (parent_tool_use_id is set)
        handle_session_frame(
            json!({
                "type": "stream_event",
                "parent_tool_use_id": "toolu_parent_123",
                "event": {
                    "type": "message_start",
                    "message": {
                        "id": "sub_msg_2",
                        "model": "claude-haiku-3-5",
                        "usage": { "input_tokens": 200, "output_tokens": 10 }
                    }
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        assert!(ext.usage.last_usage.is_none());
        assert!(ext.usage.last_total.is_none());
        assert!(ext.usage.last_model.is_none());

        while let Ok(msg) = urx.try_recv() {
            if let SessionOutbound::RawUpdate(v) = msg {
                assert_ne!(
                    v.pointer("/update/sessionUpdate").and_then(Value::as_str),
                    Some("usage_update"),
                    "subagent frames must never emit usage_update"
                );
            }
        }
    }

    #[tokio::test]
    async fn inv_63_model_reselection_keeps_learned_window() {
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        ext.config.current_model_id = "claude-sonnet-4-5".to_string();
        ext.config.model_infos = vec![
            crate::session_config::ModelInfo {
                value: "claude-sonnet-4-5".to_string(),
                display_name: "Claude Sonnet 4.5".to_string(),
                description: "Standard model".to_string(),
                resolved_model: None,
                supports_auto_mode: Some(true),
                supports_effort: Some(true),
                supported_effort_levels: Some(vec!["low".into(), "medium".into(), "high".into()]),
                supports_fast_mode: None,
            },
            crate::session_config::ModelInfo {
                value: "claude-haiku-3-5".to_string(),
                display_name: "Claude Haiku 3.5".to_string(),
                description: "Fast model".to_string(),
                resolved_model: None,
                supports_auto_mode: Some(true),
                supports_effort: Some(false),
                supported_effort_levels: None,
                supports_fast_mode: None,
            },
        ];
        ext.usage.context_window = 500_000;
        ext.usage.authoritative = true;

        // 1. Re-applying the SAME model: model_changed is false -> keeps window
        let model_val = "claude-sonnet-4-5".to_string();
        let model_changed = ext.config.current_model_id != model_val;
        assert!(!model_changed);
        assert_eq!(ext.usage.context_window, 500_000);
        assert!(ext.usage.authoritative);

        // 2. Applying a DIFFERENT model: model_changed is true -> re-seeds and clears authoritative
        let model_val = "claude-haiku-3-5".to_string();
        let model_changed = ext.config.current_model_id != model_val;
        assert!(model_changed);
        if model_changed {
            ext.config.current_model_id = model_val.clone();
            let new_model_info = ext
                .config
                .model_infos
                .iter()
                .find(|m| m.value == model_val)
                .cloned();
            let seed = new_model_info.as_ref().and_then(|m| {
                let texts = [
                    m.resolved_model.as_deref(),
                    Some(m.value.as_str()),
                    Some(m.display_name.as_str()),
                    Some(m.description.as_str()),
                ];
                crate::usage::infer_context_window_from_model(
                    &texts.into_iter().flatten().collect::<Vec<_>>(),
                )
            });
            ext.usage.context_window = seed.unwrap_or(crate::usage::DEFAULT_CONTEXT_WINDOW);
            ext.usage.authoritative = false;
        }
        assert_eq!(
            ext.usage.context_window,
            crate::usage::DEFAULT_CONTEXT_WINDOW
        );
        assert!(!ext.usage.authoritative);

        // 3. UpdateContextWindow updates when NOT authoritative
        assert!(!ext.usage.authoritative);
        assert!(ext.usage.update_window_if_positive(Some(1_000_000)));
        assert_eq!(ext.usage.context_window, 1_000_000);
        assert!(ext.usage.authoritative);

        // Once authoritative, background update does not overwrite
        if !ext.usage.authoritative {
            ext.usage.update_window_if_positive(Some(200_000));
        }
        assert_eq!(ext.usage.context_window, 1_000_000);
    }

    #[test]
    fn inv_63_air_client_without_recommended_value_effort() {
        let caps_no_rec = json!({
            "_meta": {
                "jetbrains": {
                    "air": {
                        "version": 1,
                        "capabilities": ["diffPatch"]
                    }
                }
            }
        });
        let air_no_rec = AirClient::from_client_capabilities(&caps_no_rec).expect("air client");
        assert!(air_no_rec.is_air());
        assert!(!air_no_rec.supports("recommendedValue"));

        let caps_with_rec = json!({
            "_meta": {
                "jetbrains": {
                    "air": {
                        "version": 1,
                        "capabilities": ["recommendedValue", "diffPatch"]
                    }
                }
            }
        });
        let air_with_rec = AirClient::from_client_capabilities(&caps_with_rec).expect("air client");
        assert!(air_with_rec.is_air());
        assert!(air_with_rec.supports("recommendedValue"));

        let modes = crate::session_config::ModesState {
            current_mode_id: "default".into(),
            available_modes: vec![],
        };
        let available_models = vec![crate::session_config::AvailableModel {
            model_id: "m1".into(),
            name: "Model 1".into(),
            description: None,
        }];
        let model_infos = vec![crate::session_config::ModelInfo {
            value: "m1".into(),
            display_name: "Model 1".into(),
            description: "".into(),
            resolved_model: None,
            supports_auto_mode: Some(true),
            supports_effort: Some(true),
            supported_effort_levels: Some(vec!["low".into(), "medium".into(), "high".into()]),
            supports_fast_mode: None,
        }];

        // Without recommendedValue capability -> non-AIR effort behaviour:
        // has "default" entry, current_value is "default".
        let use_rec_false = air_no_rec.supports("recommendedValue");
        let opts_no_rec = crate::session_config::build_config_options(
            &modes,
            &available_models,
            "m1",
            &model_infos,
            None,
            use_rec_false,
        );
        let effort_no_rec = opts_no_rec
            .iter()
            .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
            .expect("effort option");
        assert_eq!(effort_no_rec.current_value, "default");
        assert!(effort_no_rec.options.iter().any(|o| o.value == "default"));

        // With recommendedValue capability -> AIR effort behaviour:
        // concrete recommendation, no "default" entry.
        let use_rec_true = air_with_rec.supports("recommendedValue");
        let opts_with_rec = crate::session_config::build_config_options(
            &modes,
            &available_models,
            "m1",
            &model_infos,
            None,
            use_rec_true,
        );
        let effort_with_rec = opts_with_rec
            .iter()
            .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
            .expect("effort option");
        assert_ne!(effort_with_rec.current_value, "default");
        assert!(!effort_with_rec.options.iter().any(|o| o.value == "default"));
    }

    /// 3.3 (INV-65) — `prompt_suggestion` frames from the CLI are consumed
    /// without error, emitting no updates for a plain (non-AIR) client.
    #[tokio::test]
    async fn inv_65_prompt_suggestion_frame_ignored() {
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut live_background = HashMap::new();
        let mut pending = HashMap::new();
        let mut emitted_text = false;
        let mut assistant_error = false;
        let mut tasks = AsyncTaskRuntime::new(false);
        let mut ext = SessionExt::new(
            None,
            PathBuf::from("/tmp"),
            crate::env_snapshot::EnvSnapshot::empty(),
        );

        handle_session_frame(
            json!({
                "type": "prompt_suggestion",
                "suggestion": "How about running tests?"
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_text,
            &mut assistant_error,
            &update_tx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        // No update is sent to the client
        assert!(update_rx.try_recv().is_err());
    }

    // F5b/N-30: a result's `matchingModelUsage` (>0 guard) updates the live
    // window and seeds the cross-session cache under the resolved modelUsage
    // key and the assistant message's own (bare) spelling.
    #[test]
    fn f5b_result_usage_seeds_cross_session_cache() {
        let provider = "f5b-result-cache";
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        ext.usage.provider_cache_key = provider.to_string();
        ext.usage.last_model = Some("claude-sonnet-5".to_string());
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut live_background = HashMap::new();
        let mut pending = HashMap::new();
        let (utx, _urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted = false;
        let mut had_error = false;
        let mut tasks = AsyncTaskRuntime::new(false);
        handle_session_frame(
            json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "result": "ok",
                "modelUsage": {"claude-sonnet-5[1m]": {"contextWindow": 1000000}},
                "usage": {"input_tokens": 1, "output_tokens": 1,
                          "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted,
            &mut had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert_eq!(ext.usage.context_window, 1_000_000);
        assert!(ext.usage.authoritative);
        assert_eq!(
            crate::session_model::cached_context_window(
                &crate::session_model::context_window_cache_key(provider, "claude-sonnet-5[1m]")
            ),
            Some(1_000_000)
        );
        assert_eq!(
            crate::session_model::cached_context_window(
                &crate::session_model::context_window_cache_key(provider, "claude-sonnet-5")
            ),
            Some(1_000_000)
        );

        // A non-positive window never reaches the cache or overwrites the live
        // window (third-party backends can report 0).
        ext.usage.last_model = Some("claude-opus-5".to_string());
        handle_session_frame(
            json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "result": "ok",
                "modelUsage": {"claude-opus-5": {"contextWindow": 0}},
                "usage": {"input_tokens": 1, "output_tokens": 1,
                          "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted,
            &mut had_error,
            &utx,
            "s",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert_eq!(ext.usage.context_window, 1_000_000);
        assert_eq!(
            crate::session_model::cached_context_window(
                &crate::session_model::context_window_cache_key(provider, "claude-opus-5")
            ),
            None
        );
    }

    // F5b/A-10: the InitConfig seed reads the cross-session cache for the same
    // (backend, resolved model) before falling back to the heuristic, and a
    // cached window marks the live window authoritative.
    #[test]
    fn f5b_init_config_seed_reads_cached_window() {
        let provider = "f5b-init-cache";
        crate::session_model::cache_context_window(
            &crate::session_model::context_window_cache_key(provider, "claude-sonnet-5[1m]"),
            967_000,
        );
        let mut ext = SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        let config = crate::session_config::SessionConfigState {
            current_model_id: "sonnet".to_string(),
            model_infos: vec![crate::session_config::ModelInfo {
                value: "sonnet".to_string(),
                display_name: "Sonnet".to_string(),
                description: String::new(),
                resolved_model: Some("claude-sonnet-5[1m]".to_string()),
                ..Default::default()
            }],
            provider_cache_key: provider.to_string(),
            ..Default::default()
        };
        let seeded = crate::session_model::immediate_context_window(
            &config.provider_cache_key,
            &config.current_model_id,
            config
                .model_infos
                .iter()
                .find(|m| m.value == config.current_model_id),
        );
        ext.usage.provider_cache_key = config.provider_cache_key.clone();
        ext.usage.context_window = seeded.size;
        ext.usage.authoritative = seeded.authoritative;
        assert_eq!(ext.usage.context_window, 967_000);
        assert!(ext.usage.authoritative);
    }

    /// Spawn a bare session actor (no child process) and return its command
    /// sender, outbound receiver, stream sender, and the control writer's
    /// sender (kept alive by the caller).
    pub(super) fn spawn_bare_actor() -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<SessionOutbound>,
        mpsc::UnboundedSender<Value>,
        mpsc::UnboundedSender<Value>,
    ) {
        let (a, _b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
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
            Arc::new(AtomicUsize::new(0)),
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
        (tx, update_rx, stream_tx, control_in_tx)
    }

    /// Barrier: wait until the actor has processed every command sent before
    /// this point.
    async fn actor_barrier(tx: &mpsc::UnboundedSender<Command>) {
        let (reply, rx) = oneshot::channel();
        tx.send(Command::CommandsSnapshot { reply })
            .expect("actor alive");
        let _ = rx.await;
    }

    /// Collect every `available_commands_update` raw update currently queued.
    fn drain_commands(update_rx: &mut mpsc::UnboundedReceiver<SessionOutbound>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(item) = update_rx.try_recv() {
            if let SessionOutbound::RawUpdate(value) = item {
                if value["update"]["sessionUpdate"] == "available_commands_update" {
                    out.push(value["update"]["availableCommands"].clone());
                }
            }
        }
        out
    }

    fn command_names(commands: &Value) -> Vec<String> {
        commands
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c["name"].as_str().map(String::from))
            .collect()
    }

    /// D5 (Decision 9, `acp-agent.ts:5976-5998`) — a `system/init` frame's
    /// `terminal_slash_commands` latches and re-publishes the filtered list.
    #[tokio::test]
    async fn inv_73_init_frame_republishes_filtered_list() {
        let (tx, mut update_rx, stream_tx, _control_in_tx) = spawn_bare_actor();
        tx.send(Command::InitCommands {
            commands: json!([
                {"name": "compact", "description": "Compress the conversation history"},
                {"name": "doctor", "description": "Run a system diagnostics check"},
                {"name": "help", "description": "Display help for commands"},
            ])
            .as_array()
            .cloned()
            .unwrap_or_default(),
        })
        .unwrap();
        actor_barrier(&tx).await;
        stream_tx
            .send(json!({
                "type": "system",
                "subtype": "init",
                "terminal_slash_commands": ["doctor"],
            }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let published = drain_commands(&mut update_rx);
        assert_eq!(published.len(), 1, "a first latch re-advertises once");
        assert_eq!(
            command_names(&published[0]),
            vec!["compact", "help", "mcp", "remote-control"],
            "the latched terminal-bound name is hidden"
        );
    }

    /// D5 (Decision 9) — an init frame whose list equals the latched one
    /// publishes nothing (Node's `JSON.stringify` equality check).
    #[tokio::test]
    async fn inv_73_unchanged_terminal_list_publishes_nothing() {
        let (tx, mut update_rx, stream_tx, _control_in_tx) = spawn_bare_actor();
        tx.send(Command::InitCommands {
            commands: json!([{"name": "doctor", "description": "Run a check"}])
                .as_array()
                .cloned()
                .unwrap_or_default(),
        })
        .unwrap();
        let init = json!({
            "type": "system",
            "subtype": "init",
            "terminal_slash_commands": ["doctor"],
        });
        stream_tx.send(init.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            drain_commands(&mut update_rx).len(),
            1,
            "first latch publishes"
        );
        stream_tx.send(init).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            drain_commands(&mut update_rx).is_empty(),
            "an unchanged terminal list must not re-advertise"
        );
    }

    /// D5 (Decision 9, `acp-agent.ts:6266-6288`) — `commands_changed` replaces
    /// the stored list and filters it with the LATCHED terminal list.
    #[tokio::test]
    async fn inv_73_commands_changed_uses_stored_terminal_list() {
        let (tx, mut update_rx, stream_tx, _control_in_tx) = spawn_bare_actor();
        tx.send(Command::InitCommands {
            commands: json!([{"name": "doctor", "description": "Run a check"}])
                .as_array()
                .cloned()
                .unwrap_or_default(),
        })
        .unwrap();
        stream_tx
            .send(json!({
                "type": "system",
                "subtype": "init",
                "terminal_slash_commands": ["doctor"],
            }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_commands(&mut update_rx);
        stream_tx
            .send(json!({
                "type": "system",
                "subtype": "commands_changed",
                "commands": [
                    {"name": "doctor", "description": "Run a check"},
                    {"name": "help", "description": "Display help"},
                ],
            }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let published = drain_commands(&mut update_rx);
        assert_eq!(published.len(), 1);
        assert_eq!(
            command_names(&published[0]),
            vec!["help", "mcp", "remote-control"],
            "the frame's list replaces the stored one and keeps the terminal filter"
        );
    }

    /// D1 (Decision 1) — the background refresh applies ONCE and marks the
    /// window authoritative; a later authoritative source (`result.modelUsage`)
    /// still overrides, and a second refresh is ignored.
    #[test]
    fn inv_70_refresh_applies_once_and_sets_authoritative() {
        let mut usage = crate::usage::UsageState::default();
        assert!(!usage.authoritative);
        assert_eq!(usage.context_window, crate::usage::DEFAULT_CONTEXT_WINDOW);

        assert!(apply_context_window_answer(
            &mut usage, "m1", 1_000_000, "m1"
        ));
        assert_eq!(usage.context_window, 1_000_000);
        assert!(usage.authoritative);

        // A second refresh after the window became authoritative is dropped.
        assert!(!apply_context_window_answer(
            &mut usage, "m1", 200_000, "m1"
        ));
        assert_eq!(usage.context_window, 1_000_000);

        // `result.modelUsage` still overrides (it is authoritative by nature).
        assert!(usage.update_window_if_positive(Some(500_000)));
        assert_eq!(usage.context_window, 500_000);
        assert!(usage.authoritative);

        // And a refresh answering for a model the session left is dropped.
        assert!(!apply_context_window_answer(
            &mut usage, "m2", 900_000, "m1"
        ));
        assert_eq!(usage.context_window, 500_000);
    }

    /// U-10 — raw forwarding covers every message the SDK query yields, but
    /// not the frames its `readMessages` consumes first: control traffic,
    /// `keep_alive`, `transcript_mirror` and host-only `session_state_changed`.
    #[test]
    fn inv_raw_forwarding_skips_consumed_frames() {
        for frame in [
            json!({"type": "control_request"}),
            json!({"type": "control_response"}),
            json!({"type": "control_cancel_request"}),
            json!({"type": "keep_alive"}),
            json!({"type": "transcript_mirror"}),
            json!({"type": "system", "subtype": "session_state_changed", "sdk_host_only": true}),
        ] {
            assert!(!forwards_raw_frame(&frame), "{frame} must not be forwarded");
        }
        for frame in [
            json!({"type": "user"}),
            json!({"type": "assistant"}),
            json!({"type": "stream_event"}),
            json!({"type": "result"}),
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}),
            json!({"type": "command_lifecycle"}),
            json!({"type": "tool_progress"}),
        ] {
            assert!(forwards_raw_frame(&frame), "{frame} must be forwarded");
        }
    }

    #[tokio::test]
    async fn inv_54_review_findings_actor_frame_context() {
        use crate::air::AirClient;

        let air_caps = json!({
            "_meta": {
                "jetbrains": {
                    "air": {
                        "version": 1,
                        "capabilities": ["sessionFailure"]
                    }
                }
            }
        });
        let air = AirClient::from_client_capabilities(&air_caps);

        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut live_background = HashMap::new();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, mut urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut tasks = AsyncTaskRuntime::new(false);
        let mut ext = SessionExt::new(
            air.clone(),
            std::path::PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );

        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        // Finding 5: subagent assistant frame does NOT feed failure ctx
        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": "toolu_sub",
                "error": "rate_limit",
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "Subagent error"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert!(ext.failure_ctx.error.is_none());
        assert!(!assistant_had_error);

        // Drain any chunks emitted by the subagent frame
        while urx.try_recv().is_ok() {}

        // Finding 5: top-level assistant frame DOES feed failure ctx
        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "error": "rate_limit",
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "Rate limit reached"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert_eq!(ext.failure_ctx.error.as_deref(), Some("rate_limit"));
        assert!(assistant_had_error);

        // Finding 8, Defect 2: AIR client skips chat update (AgentMessageChunk) for top-level assistant with error or synthetic limit
        let mut saw_chunk_err = false;
        while let Ok(msg) = urx.try_recv() {
            if let SessionOutbound::Update(notif) = msg {
                if matches!(notif.update, SessionUpdate::AgentMessageChunk(_)) {
                    saw_chunk_err = true;
                }
            }
        }
        assert!(
            !saw_chunk_err,
            "AIR client must not receive chat text for assistant error frame"
        );

        // Finding 6: synthetic usage limit sets was_usage_limit
        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "message": {
                    "model": "<synthetic>",
                    "content": "You've hit your usage limit"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert!(ext.failure_ctx.was_usage_limit);

        // Defect 2: AIR client also suppresses chat text for synthetic usage limit frame
        let mut saw_chunk_limit = false;
        while let Ok(msg) = urx.try_recv() {
            if let SessionOutbound::Update(notif) = msg {
                if matches!(notif.update, SessionUpdate::AgentMessageChunk(_)) {
                    saw_chunk_limit = true;
                }
            }
        }
        assert!(
            !saw_chunk_limit,
            "AIR client must not receive chat text for synthetic usage limit frame"
        );

        // Defect 2 Control: non-AIR client DOES receive chat chunks for both error and synthetic limit frames
        let (utx_non_air, mut urx_non_air) = mpsc::unbounded_channel();
        let mut ext_non_air = SessionExt::new(
            None,
            std::path::PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        let mut emitted_non_air = false;
        let mut had_err_non_air = false;

        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "error": "rate_limit",
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "Rate limit reached"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_non_air,
            &mut had_err_non_air,
            &utx_non_air,
            "s1",
            &hw,
            &mut tasks,
            &mut ext_non_air,
        );

        let mut saw_non_air_err = false;
        while let Ok(msg) = urx_non_air.try_recv() {
            if let SessionOutbound::Update(notif) = msg {
                if matches!(notif.update, SessionUpdate::AgentMessageChunk(_)) {
                    saw_non_air_err = true;
                }
            }
        }
        assert!(
            saw_non_air_err,
            "control: non-AIR client MUST receive chat chunk for assistant error frame"
        );

        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "message": {
                    "model": "<synthetic>",
                    "content": "You've hit your usage limit"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_non_air,
            &mut had_err_non_air,
            &utx_non_air,
            "s1",
            &hw,
            &mut tasks,
            &mut ext_non_air,
        );

        let mut saw_non_air_limit = false;
        while let Ok(msg) = urx_non_air.try_recv() {
            if let SessionOutbound::Update(notif) = msg {
                if matches!(notif.update, SessionUpdate::AgentMessageChunk(_)) {
                    saw_non_air_limit = true;
                }
            }
        }
        assert!(
            saw_non_air_limit,
            "control: non-AIR client MUST receive chat chunk for synthetic limit frame"
        );

        // Finding 6: subsequent top-level real model frame clears was_usage_limit
        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "Continuing after recovery"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert!(
            !ext.failure_ctx.was_usage_limit,
            "real model frame must clear was_usage_limit"
        );

        // Defect 3: error frame then normal frame then is_error result keeps error text as title
        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "error": "rate_limit",
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "API Error: rate limited"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert_eq!(
            ext.failure_ctx.title.as_deref(),
            Some("API Error: rate limited")
        );

        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "Here is what I found"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );
        assert_eq!(
            ext.failure_ctx.title.as_deref(),
            Some("API Error: rate limited"),
            "subsequent normal frame without error must NOT overwrite failure title"
        );
    }

    #[tokio::test]
    async fn inv_54_defect1_echoless_activation_resets_failure_ctx() {
        use crate::air::AirClient;

        let air_caps = json!({
            "_meta": {
                "jetbrains": {
                    "air": {
                        "version": 1,
                        "capabilities": ["sessionFailure"]
                    }
                }
            }
        });
        let air = AirClient::from_client_capabilities(&air_caps);

        // --- (a) AIR client scenario ---
        let mut machine = TurnMachine::new();
        let mut map_state = MapState::default();
        let mut live_background = HashMap::new();
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx, _urx) = mpsc::unbounded_channel::<SessionOutbound>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;
        let mut assistant_had_error = false;
        let mut tasks = AsyncTaskRuntime::new(false);
        let mut ext = SessionExt::new(
            air.clone(),
            std::path::PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );

        // Turn A: rate limited
        let (reply_a_tx, reply_a_rx) = oneshot::channel();
        pending.insert("pA".into(), reply_a_tx);
        machine.enqueue(Turn::new("pA".into(), false));
        machine.on_echo("pA");

        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "error": "rate_limit",
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "Rate limit reached"
                }
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        handle_session_frame(
            json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "result": "Turn A failed"
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        let reply_a = reply_a_rx
            .await
            .expect("reply A received")
            .expect("prompt A ok with sessionFailure");
        let sf_a =
            &reply_a.quota.as_ref().expect("quota has meta")["jetbrains"]["air"]["sessionFailure"];
        assert_eq!(sf_a["category"], "limit");
        assert_eq!(sf_a["title"], "Rate limit reached");

        // Turn B: echo-less command (e.g. /compact)
        let (reply_b_tx, reply_b_rx) = oneshot::channel();
        pending.insert("pB".into(), reply_b_tx);
        machine.enqueue(Turn::new("pB".into(), true));

        // Result arrives for Turn B with is_error: true
        handle_session_frame(
            json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "errors": ["Echo-less command failed"],
                "result": "Echo-less command failed"
            }),
            &mut machine,
            &mut map_state,
            &mut live_background,
            &mut pending,
            &mut emitted_assistant_text,
            &mut assistant_had_error,
            &utx,
            "s1",
            &hw,
            &mut tasks,
            &mut ext,
        );

        let reply_b = reply_b_rx
            .await
            .expect("reply B received")
            .expect("prompt B ok with sessionFailure");
        let sf_b =
            &reply_b.quota.as_ref().expect("quota has meta")["jetbrains"]["air"]["sessionFailure"];
        assert_eq!(
            sf_b["category"], "service",
            "echo-less command failing after rate_limit turn must have category service (provider_error), not stale limit"
        );
        assert_eq!(
            sf_b["title"], "Echo-less command failed",
            "echo-less command failing must have its own title, not stale title"
        );

        // --- (b) Non-AIR client scenario ---
        let mut machine_na = TurnMachine::new();
        let mut map_state_na = MapState::default();
        let mut live_background_na = HashMap::new();
        let mut pending_na: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>> =
            HashMap::new();
        let (utx_na, _urx_na) = mpsc::unbounded_channel::<SessionOutbound>();
        let mut emitted_assistant_text_na = false;
        let mut assistant_had_error_na = false;
        let mut tasks_na = AsyncTaskRuntime::new(false);
        let mut ext_na = SessionExt::new(
            None,
            std::path::PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );

        // Turn A (non-AIR) gets error frame and fails
        let (reply_na_a_tx, reply_na_a_rx) = oneshot::channel();
        pending_na.insert("pA_na".into(), reply_na_a_tx);
        machine_na.enqueue(Turn::new("pA_na".into(), false));
        machine_na.on_echo("pA_na");

        handle_session_frame(
            json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "error": "rate_limit",
                "message": {
                    "model": "claude-sonnet-4-5",
                    "content": "Rate limit reached"
                }
            }),
            &mut machine_na,
            &mut map_state_na,
            &mut live_background_na,
            &mut pending_na,
            &mut emitted_assistant_text_na,
            &mut assistant_had_error_na,
            &utx_na,
            "s1",
            &hw,
            &mut tasks_na,
            &mut ext_na,
        );

        handle_session_frame(
            json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "result": "Turn A failed"
            }),
            &mut machine_na,
            &mut map_state_na,
            &mut live_background_na,
            &mut pending_na,
            &mut emitted_assistant_text_na,
            &mut assistant_had_error_na,
            &utx_na,
            "s1",
            &hw,
            &mut tasks_na,
            &mut ext_na,
        );

        let err_a = reply_na_a_rx
            .await
            .expect("reply received")
            .expect_err("non-AIR fails with PromptFailed");
        match &err_a.error {
            SessionError::PromptFailed {
                assistant_had_error,
                ..
            } => {
                assert!(
                    *assistant_had_error,
                    "Turn A with error frame must have assistant_had_error: true"
                );
            }
            other => panic!("expected PromptFailed, got {other:?}"),
        }

        // Turn B (non-AIR) echo-less command
        let (reply_na_b_tx, reply_na_b_rx) = oneshot::channel();
        pending_na.insert("pB_na".into(), reply_na_b_tx);
        machine_na.enqueue(Turn::new("pB_na".into(), true));

        handle_session_frame(
            json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "errors": ["Echo-less command failed"],
                "result": "Echo-less command failed"
            }),
            &mut machine_na,
            &mut map_state_na,
            &mut live_background_na,
            &mut pending_na,
            &mut emitted_assistant_text_na,
            &mut assistant_had_error_na,
            &utx_na,
            "s1",
            &hw,
            &mut tasks_na,
            &mut ext_na,
        );

        let err_b = reply_na_b_rx
            .await
            .expect("reply received")
            .expect_err("non-AIR fails with PromptFailed");
        match &err_b.error {
            SessionError::PromptFailed {
                assistant_had_error,
                ..
            } => {
                assert!(
                    !*assistant_had_error,
                    "echo-less command failing must have assistant_had_error: false (no stale errorKind)"
                );
            }
            other => panic!("expected PromptFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inv_54_defect1_enqueue_race_does_not_wipe_failure_ctx() {
        use crate::air::AirClient;

        let air_caps = json!({
            "_meta": {
                "jetbrains": {
                    "air": {
                        "version": 1,
                        "capabilities": ["sessionFailure"]
                    }
                }
            }
        });
        let air = AirClient::from_client_capabilities(&air_caps);

        // Drive the REAL actor loop (`run_loop`): the prompt handlers are where an
        // enqueue-time failure-ctx reset would live, and only a `Command::Prompt`
        // sent through the channel reaches them.
        let (a, _b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let actor = tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            Duration::from_secs(30),
            "s1".into(),
            Arc::new(AtomicUsize::new(0)),
            air,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
        let prompt = |uuid: &str| {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(Command::Prompt {
                uuid: uuid.into(),
                frame: json!({"type":"user","uuid":uuid,"message":{}}),
                is_local_only: false,
                prompt_text: String::new(),
                reply: reply_tx,
            })
            .unwrap();
            reply_rx
        };

        tokio::time::timeout(Duration::from_secs(20), async {
            // Turn A is active and receives a typed rate-limit error.
            let reply_a = prompt("pA");
            stream_tx
                .send(json!({"type":"user","uuid":"pA","isReplay":true,
                    "message":{"role":"user","content":[{"type":"text","text":"a"}]}}))
                .unwrap();
            stream_tx
                .send(json!({
                    "type": "assistant",
                    "parent_tool_use_id": null,
                    "error": "rate_limit",
                    "message": {"model": "claude-sonnet-4-5", "content": "Rate limit reached"}
                }))
                .unwrap();
            // Frames and commands are separate channels: let the actor consume the
            // error frame before B's prompt arrives (B races A's run in prod).
            tokio::time::sleep(Duration::from_millis(100)).await;

            // While A is running, prompt B is enqueued through the prompt handler.
            let _reply_b = prompt("pB");
            tokio::time::sleep(Duration::from_millis(100)).await;

            // A fails: it must still report its own rate_limit failure ctx.
            stream_tx
                .send(json!({
                    "type": "result",
                    "subtype": "error_during_execution",
                    "is_error": true,
                    "result": "Turn A failed"
                }))
                .unwrap();
            let reply_a = reply_a.await.expect("reply A").expect("prompt A ok");
            let sf_a =
                &reply_a.quota.as_ref().expect("quota meta")["jetbrains"]["air"]["sessionFailure"];
            assert_eq!(
                sf_a["category"], "limit",
                "enqueueing prompt B must not wipe prompt A's failure ctx"
            );
            assert_eq!(sf_a["title"], "Rate limit reached");
        })
        .await
        .expect("inv_54_defect1_enqueue_race timed out");
        actor.abort();
    }

    /// A run-loop harness for the stopped-work permission tests (F16b, B-62).
    struct StoppedWorkHarness {
        tx: mpsc::UnboundedSender<Command>,
        stream_tx: mpsc::UnboundedSender<Value>,
        update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
        actor: tokio::task::JoinHandle<()>,
        _writer: tokio::io::DuplexStream,
    }

    impl StoppedWorkHarness {
        /// p1 active, cancelled and ended at its interrupted result: the client was
        /// told the work ended while the cancel window is still open.
        async fn cancelled_and_ended() -> Self {
            let (a, b) = tokio::io::duplex(64 * 1024);
            let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
            let (tx, commands) = mpsc::unbounded_channel::<Command>();
            let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
            let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
            let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
            let actor = tokio::spawn(run_loop(
                commands,
                tx.clone(),
                stream_rx,
                control,
                control_in_tx,
                update_tx,
                Duration::from_secs(30),
                "s1".into(),
                Arc::new(AtomicUsize::new(0)),
                None,
                crate::client_caps::ClientCapabilities::default(),
                std::env::temp_dir(),
                false,
                false,
                crate::sdk_options::RawSdkMessages::Off,
                crate::env_snapshot::EnvSnapshot::empty(),
                None,
                Vec::new(),
            ));
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(Command::Prompt {
                uuid: "p1".into(),
                frame: json!({"type":"user","uuid":"p1","message":{}}),
                is_local_only: false,
                prompt_text: String::new(),
                reply: reply_tx,
            })
            .unwrap();
            stream_tx
                .send(json!({"type":"user","uuid":"p1","isReplay":true,
                    "message":{"role":"user","content":[{"type":"text","text":"a"}]}}))
                .unwrap();
            tx.send(Command::Cancel).unwrap();
            stream_tx
                .send(json!({"type":"result","subtype":"error_during_execution",
                    "is_error":true,"stop_reason":null,"errors":["aborted"],"usage":{}}))
                .unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(10), reply_rx)
                .await
                .expect("p1 settles at its interrupted result")
                .expect("reply")
                .expect("ok");
            assert_eq!(reply.stop_reason, "cancelled");
            Self {
                tx,
                stream_tx,
                update_rx,
                actor,
                _writer: b,
            }
        }

        fn permission(&self, tool_use_id: &str) -> oneshot::Receiver<Value> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(Command::Permission {
                    frame: json!({"type":"control_request","request_id":format!("req-{tool_use_id}"),
                        "request":{"subtype":"can_use_tool","tool_name":"Bash",
                        "input":{"command":"ls"},"tool_use_id":tool_use_id,
                        "permission_suggestions":[]}}),
                    reply: reply_tx,
                })
                .unwrap();
            reply_rx
        }

        fn stream_tool_use(&self, tool_use_id: &str) {
            self.stream_tx
                .send(json!({"type":"stream_event","parent_tool_use_id":null,
                    "event":{"type":"content_block_start","index":0,
                    "content_block":{"type":"tool_use","id":tool_use_id,"name":"Bash","input":{}}}}))
                .unwrap();
        }
    }

    /// Drive `run_loop` with a live control writer and collect the control requests
    /// the CLI would see.
    async fn control_lines_after_cancel(stops_workflows: bool) -> Vec<Value> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, _update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let actor = tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            Duration::from_secs(30),
            "s1".into(),
            Arc::new(AtomicUsize::new(0)),
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
        let (ltx, mut lrx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let mut reader = BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                            let _ = ltx.send(v);
                        }
                    }
                }
            }
        });
        tx.send(Command::StopsWorkflowsOnCancel(stops_workflows))
            .unwrap();
        stream_tx
            .send(
                json!({"type":"system","subtype":"task_started","task_id":"wf-1",
                "task_type":"local_workflow","description":"flow"}),
            )
            .unwrap();
        stream_tx
            .send(
                json!({"type":"system","subtype":"task_started","task_id":"bash-1",
                "task_type":"local_bash","description":"sleep"}),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(Command::Cancel).unwrap();
        let mut seen = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            while let Some(line) = lrx.recv().await {
                seen.push(line);
            }
        })
        .await;
        actor.abort();
        seen
    }

    /// B-42 — after a cancel's interrupt, the live background `local_workflow`
    /// tasks the interrupt spared are stopped (only for a client with the per-task
    /// stop affordance and no async tasks); other task types are left alone.
    #[tokio::test]
    async fn f16c_cancel_stops_background_workflows_after_interrupt() {
        let seen = tokio::time::timeout(Duration::from_secs(20), control_lines_after_cancel(true))
            .await
            .expect("timed out");
        let subtypes: Vec<(&str, &str)> = seen
            .iter()
            .map(|v| {
                (
                    v.pointer("/request/subtype")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    v.pointer("/request/task_id")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )
            })
            .collect();
        let interrupt = subtypes.iter().position(|(s, _)| *s == "interrupt");
        let stop = subtypes.iter().position(|(s, _)| *s == "stop_task");
        assert!(interrupt.is_some() && stop.is_some(), "{subtypes:?}");
        assert!(
            interrupt < stop,
            "stop_task follows the interrupt: {subtypes:?}"
        );
        assert_eq!(
            subtypes
                .iter()
                .filter(|(s, _)| *s == "stop_task")
                .collect::<Vec<_>>(),
            vec![&("stop_task", "wf-1")],
            "only the local_workflow task is stopped"
        );

        let seen = tokio::time::timeout(Duration::from_secs(20), control_lines_after_cancel(false))
            .await
            .expect("timed out");
        assert!(
            seen.iter()
                .all(|v| v.pointer("/request/subtype").and_then(Value::as_str) != Some("stop_task")),
            "no stop_task without the affordance"
        );
    }

    /// B-62 / issue #1061 — a permission request for a tool use the consumer
    /// already dropped as the cancelled work's is denied at once, without showing
    /// the client a tool call or a request.
    #[tokio::test]
    async fn f16b_permission_for_dropped_tool_use_is_denied() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut h = StoppedWorkHarness::cancelled_and_ended().await;
            h.stream_tool_use("toolu_late");
            let reply = h.permission("toolu_late");
            let response = tokio::time::timeout(Duration::from_secs(5), reply)
                .await
                .expect("denied promptly")
                .expect("a deny verdict");
            assert_eq!(response["response"]["response"]["behavior"], "deny");
            assert_eq!(
                response["response"]["response"]["message"],
                "The user cancelled this prompt."
            );
            // No tool_call and no permission request reached the client.
            while let Ok(out) = h.update_rx.try_recv() {
                assert!(
                    !matches!(out, SessionOutbound::RequestPermission { .. }),
                    "the stopped work shows the client no request"
                );
            }
            h.actor.abort();
        })
        .await
        .expect("timed out");
    }

    /// B-62 — a request for a tool use the stream has not shown yet waits for the
    /// consumer to reach it, then is judged: here the stream shows it after the
    /// work-end report, so it is the stopped work's and is denied.
    #[tokio::test]
    async fn f16b_permission_waits_for_the_stream_to_reach_the_tool_use() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let h = StoppedWorkHarness::cancelled_and_ended().await;
            let mut reply = h.permission("toolu_wait");
            assert!(
                tokio::time::timeout(Duration::from_millis(300), &mut reply)
                    .await
                    .is_err(),
                "no verdict before the consumer reaches the tool use"
            );
            h.stream_tool_use("toolu_wait");
            let response = tokio::time::timeout(Duration::from_secs(5), reply)
                .await
                .expect("verdict once the stream reached it")
                .expect("deny");
            assert_eq!(response["response"]["response"]["behavior"], "deny");
            assert_eq!(
                response["response"]["response"]["message"],
                "The user cancelled this prompt."
            );
            h.actor.abort();
        })
        .await
        .expect("timed out");
    }

    /// B-62 — the wait is bounded: a tool use the stream never shows (such as
    /// SandboxNetworkAccess's) is let through after `TOOL_USE_WAIT`, so the
    /// request reaches the client.
    #[tokio::test(start_paused = true)]
    async fn f16b_permission_wait_is_bounded() {
        let mut h = StoppedWorkHarness::cancelled_and_ended().await;
        let _reply = h.permission("toolu_never");
        // Nothing before the bound...
        tokio::time::sleep(TOOL_USE_WAIT - Duration::from_millis(100)).await;
        assert!(!matches!(
            h.update_rx.try_recv(),
            Ok(SessionOutbound::RequestPermission { .. })
        ));
        // ...and the request goes out once it elapsed.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut saw_request = false;
        for _ in 0..50 {
            tokio::task::yield_now().await;
            while let Ok(out) = h.update_rx.try_recv() {
                saw_request |= matches!(out, SessionOutbound::RequestPermission { .. });
            }
            if saw_request {
                break;
            }
        }
        assert!(saw_request, "the unreached tool use is let through");
        h.actor.abort();
    }

    /// T1FA 2 — a parked request the CLI withdrew (its reply receiver is gone) is
    /// dropped when its wait ends: the client sees no tool call and no dialog
    /// (upstream throws "Tool use aborted" before sending anything).
    #[tokio::test(start_paused = true)]
    async fn t1fa_withdrawn_parked_permission_never_reaches_the_client() {
        let mut h = StoppedWorkHarness::cancelled_and_ended().await;
        let reply = h.permission("toolu_withdrawn");
        tokio::task::yield_now().await;
        drop(reply);
        tokio::time::sleep(TOOL_USE_WAIT + Duration::from_millis(500)).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        let mut seen = Vec::new();
        while let Ok(out) = h.update_rx.try_recv() {
            seen.push(out);
        }
        assert!(
            !seen.iter().any(|o| matches!(
                o,
                SessionOutbound::RequestPermission { .. } | SessionOutbound::Update(_)
            )),
            "a withdrawn request must reach the client as nothing"
        );
        h.actor.abort();
    }
}

#[cfg(test)]
mod t8c_tests {
    use super::*;
    use crate::hooks::TaskStateEntry;

    fn ext() -> SessionExt {
        SessionExt::new(
            None,
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        )
    }

    /// `tools.test.ts` > "should not log an error when no callback is
    /// registered for the tool use": a PostToolUse hook for a tool with no
    /// registered callback answers success and sends no update.
    #[test]
    fn t8c_an_unregistered_post_tool_use_hook_is_silent() {
        let mut ext = ext();
        let mut map_state = MapState::default();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let (reply, mut reply_rx) = oneshot::channel();
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "TodoWrite",
            "tool_input": {"todos": []},
            "tool_response": {"success": true}
        });
        handle_hook_callback(
            &mut AsyncTaskRuntime::new(false),
            HookCall {
                request_id: "r",
                callback_id: crate::hooks::POST_TOOL_USE_HOOK_ID,
                input: &input,
                tool_use_id: Some("toolu_todo_no_callback"),
            },
            &mut map_state,
            &mut ext,
            HookIo {
                update_tx: &update_tx,
                command_tx: &command_tx,
                session_id: "s",
                live_background: &HashMap::new(),
            },
            reply,
        );
        let response = reply_rx.try_recv().expect("the hook answered");
        assert_eq!(response["response"]["subtype"], "success");
        assert_eq!(response["response"]["response"]["continue"], true);
        assert!(update_rx.try_recv().is_err(), "no update for a silent hook");
    }

    /// `tools.test.ts` > "createTaskHook": TaskCreated registers a task and
    /// publishes the plan, but does not clobber an existing entry.
    #[test]
    fn t8c_task_created_hook_registers_without_clobbering() {
        let mut ext = ext();
        ext.task_state.push((
            "t-1".into(),
            TaskStateEntry {
                subject: "Investigate flaky test".into(),
                status: "in_progress".into(),
                active_form: None,
                description: None,
            },
        ));
        let mut map_state = MapState::default();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let mut run = |input: Value, ext: &mut SessionExt| {
            let (reply, _reply_rx) = oneshot::channel();
            handle_hook_callback(
                &mut AsyncTaskRuntime::new(false),
                HookCall {
                    request_id: "r",
                    callback_id: crate::hooks::TASK_CREATED_HOOK_ID,
                    input: &input,
                    tool_use_id: None,
                },
                &mut map_state,
                ext,
                HookIo {
                    update_tx: &update_tx,
                    command_tx: &command_tx,
                    session_id: "s",
                    live_background: &HashMap::new(),
                },
                reply,
            );
        };
        run(
            json!({"task_id": "t-1", "task_subject": "Investigate flaky test"}),
            &mut ext,
        );
        assert_eq!(ext.task_state[0].1.status, "in_progress");
        assert!(update_rx.try_recv().is_err(), "no change, no plan");

        run(
            json!({"task_id": "t-2", "task_subject": "New", "task_description": "Repro"}),
            &mut ext,
        );
        assert_eq!(ext.task_state.len(), 2);
        assert_eq!(
            ext.task_state[1].1,
            TaskStateEntry {
                subject: "New".into(),
                status: "pending".into(),
                active_form: None,
                description: Some("Repro".into()),
            }
        );
        let out = update_rx.try_recv().expect("the new task publishes a plan");
        let SessionOutbound::RawUpdate(update) = out else {
            panic!("expected a raw plan update");
        };
        assert_eq!(update["update"]["sessionUpdate"], "plan");
    }

    /// `tool-call-contract.test.ts` > task plans > "publishes a Task* change
    /// once when the hook reports it first": the hook registered the task and
    /// published the plan; the TaskCreate tool result that follows finds the
    /// plan unchanged, so it publishes nothing.
    #[test]
    fn t9a_task_plan_is_published_once_when_the_hook_reports_it_first() {
        let mut ext = ext();
        ext.air = crate::air::AirClient::from_client_capabilities(&json!({
            "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}
        }));
        ext.task_state.push((
            "1".into(),
            TaskStateEntry {
                subject: "Write tests".into(),
                status: "pending".into(),
                active_form: None,
                description: None,
            },
        ));
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        // The TaskCreated hook published the plan.
        publish_task_plan("s", &mut ext, &update_tx, None);
        let first = update_rx.try_recv().expect("the hook publishes the plan");
        let SessionOutbound::RawUpdate(first) = first else {
            panic!("expected a raw plan update");
        };
        assert_eq!(first["update"]["sessionUpdate"], "plan");
        // The TaskCreate result carries the same task; the plan did not change.
        crate::hooks::apply_task_create(
            &mut ext.task_state,
            &json!({"subject": "Write tests"}),
            Some("1"),
        );
        publish_task_plan("s", &mut ext, &update_tx, None);
        assert!(
            update_rx.try_recv().is_err(),
            "no second plan for the same change"
        );
    }

    /// `tools.test.ts` > "createTaskHook" > "marks a task completed on
    /// TaskCompleted".
    #[test]
    fn t8c_task_completed_hook_marks_the_task() {
        let mut ext = ext();
        ext.task_state.push((
            "t-1".into(),
            TaskStateEntry {
                subject: "Investigate flaky test".into(),
                status: "in_progress".into(),
                active_form: None,
                description: None,
            },
        ));
        let mut map_state = MapState::default();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let input = json!({"task_id": "t-1", "task_subject": "Investigate flaky test"});
        let (reply, _reply_rx) = oneshot::channel();
        handle_hook_callback(
            &mut AsyncTaskRuntime::new(false),
            HookCall {
                request_id: "r",
                callback_id: crate::hooks::TASK_COMPLETED_HOOK_ID,
                input: &input,
                tool_use_id: None,
            },
            &mut map_state,
            &mut ext,
            HookIo {
                update_tx: &update_tx,
                command_tx: &command_tx,
                session_id: "s",
                live_background: &HashMap::new(),
            },
            reply,
        );
        assert_eq!(ext.task_state[0].1.status, "completed");
        let out = update_rx
            .try_recv()
            .expect("the completion publishes a plan");
        let SessionOutbound::RawUpdate(update) = out else {
            panic!("expected a raw plan update");
        };
        assert_eq!(update["update"]["sessionUpdate"], "plan");
        assert_eq!(update["update"]["entries"][0]["status"], "completed");
    }

    fn plan_frame(id: &str, structured: Option<Value>, is_error: bool) -> Value {
        let mut frame = json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id,
                 "content": if is_error { "The user chose to keep planning." } else { "Approved." },
                 "is_error": is_error}
            ]}
        });
        if let Some(structured) = structured {
            frame["tool_use_result"] = structured;
        }
        frame
    }

    /// `tools.test.ts` > "does not apply TaskCreate/TaskUpdate when the
    /// tool_result reports an error".
    #[test]
    fn t8c_task_create_result_error_is_not_applied() {
        let mut ext = ext();
        let mut map_state = MapState::default();
        let _ = crate::map::map_consolidated_assistant(
            &json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "create-1", "name": "TaskCreate",
                 "input": {"subject": "A", "description": ""}}
            ]}}),
            &mut map_state,
        );
        let frame = json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "create-1",
                 "content": "task creation failed", "is_error": true}
            ]}
        });
        let blocks = frame["message"]["content"].as_array().unwrap();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        handle_user_frame_blocks(
            SendIo {
                tasks: &mut AsyncTaskRuntime::new(false),
                update_tx: &update_tx,
                session_id: "s",
            },
            &frame,
            blocks,
            &mut map_state,
            &mut ext,
            None,
        );
        assert!(ext.task_state.is_empty());
        assert!(
            std::iter::from_fn(|| update_rx.try_recv().ok()).all(|item| !matches!(
                item,
                SessionOutbound::RawUpdate(ref v) if v["update"]["sessionUpdate"] == "plan"
            )),
            "an errored TaskCreate publishes no plan"
        );
    }

    /// `tools.test.ts` > "toAcpNotifications - the plan of an approved
    /// ExitPlanMode": the plan follows the tool call's result for a client with
    /// plan operations; a rejected, subagent, leader-waiting or empty plan
    /// shows nothing.
    #[test]
    fn t8c_approved_plan_follows_the_result_only_for_a_plan_client() {
        let run = |plan: &str,
                   structured: Option<Value>,
                   is_error: bool,
                   plan_ops: bool,
                   parent: Option<&str>|
         -> Vec<SessionOutbound> {
            let mut ext = ext();
            ext.caps.plan = plan_ops;
            let mut map_state = MapState::default();
            let _ = crate::map::map_consolidated_assistant(
                &json!({"type": "assistant", "parent_tool_use_id": null,
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_plan", "name": "ExitPlanMode",
                     "input": {"plan": plan, "planFilePath": "/plans/p.md"}}
                ]}}),
                &mut map_state,
            );
            let mut frame = plan_frame("toolu_plan", structured, is_error);
            if let Some(parent) = parent {
                frame["parent_tool_use_id"] = json!(parent);
            }
            let blocks = frame["message"]["content"].as_array().unwrap();
            let (update_tx, mut update_rx) = mpsc::unbounded_channel();
            handle_user_frame_blocks(
                SendIo {
                    tasks: &mut AsyncTaskRuntime::new(false),
                    update_tx: &update_tx,
                    session_id: "s",
                },
                &frame,
                blocks,
                &mut map_state,
                &mut ext,
                None,
            );
            std::iter::from_fn(|| update_rx.try_recv().ok()).collect()
        };

        let plans = |out: &[SessionOutbound]| -> Vec<Value> {
            out.iter()
                .filter_map(|item| match item {
                    SessionOutbound::RawUpdate(v) => Some(v["update"].clone()),
                    _ => None,
                })
                .filter(|v| v["sessionUpdate"] == "plan_update")
                .collect()
        };

        // The structured result (with the user's edits) wins, after the tool
        // call's own completion.
        let out = run(
            "Implement it",
            Some(
                json!({"plan": "Implement it carefully", "isAgent": false, "planWasEdited": true}),
            ),
            false,
            true,
            None,
        );
        assert!(matches!(out[0], SessionOutbound::Update(_)));
        assert_eq!(
            plans(&out),
            vec![json!({"sessionUpdate": "plan_update",
                "plan": {"type": "markdown", "planId": "plan-mode",
                    "content": "Implement it carefully"}})]
        );

        // The input plan is trimmed when the structured result names none.
        let out = run(
            "  Implement it  ",
            Some(json!({"plan": null, "isAgent": false})),
            false,
            true,
            None,
        );
        assert_eq!(plans(&out)[0]["plan"]["content"], "Implement it");

        // A client without plan operations gets no plan.
        let out = run("Implement it", None, false, false, None);
        assert!(plans(&out).is_empty());

        // A rejected plan shows nothing.
        let out = run("Implement it", None, true, true, None);
        assert!(plans(&out).is_empty());

        // A subagent's plan and one waiting for its team lead show nothing.
        let out = run("Implement it", None, false, true, Some("toolu_agent"));
        assert!(plans(&out).is_empty());
        let out = run(
            "Implement it",
            Some(json!({"plan": "Implement it", "isAgent": false,
                "awaitingLeaderApproval": true})),
            false,
            true,
            None,
        );
        assert!(plans(&out).is_empty());

        // An empty plan shows nothing.
        let out = run("   ", None, false, true, None);
        assert!(plans(&out).is_empty());
    }
}
