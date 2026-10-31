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
    /// A defining `setModel` failure on a fresh session (`session-model.ts:574`):
    /// Node throws the SDK error, which the ACP router reports as
    /// `Internal error` with the message in `data.details`.
    #[error("{0}")]
    ModelSwitchFailed(String),
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
    /// Register a `/mcp` prompt (rust-air-gaps G12, Decision 21) before the
    /// `Prompt` it belongs to. The actor spawns the control run after pushing
    /// the prompt frame, then replaces the CLI text with the produced Markdown.
    McpPrompt {
        uuid: String,
        command: crate::mcp_command::McpCommand,
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
    /// The bounded wait for the consumer to reach `tool_use_id` ran out.
    ToolUseWaitElapsed { tool_use_id: String },
    /// Whether `session/cancel` also stops the background `local_workflow` tasks
    /// the interrupt spared (`Session.stopsWorkflowsOnCancel`, F16c / B-42).
    StopsWorkflowsOnCancel(bool),
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
    /// `_session/async_task/stop` (rust-air-gaps G1, Decision 12): claim the
    /// stop synchronously, then ask the CLI to `stop_task` from a spawned task.
    /// `reply` resolves `Ok(true)` once the updates are on the wire, `Ok(false)`
    /// for an unknown / unstoppable task, `Err(message)` when the CLI refused.
    StopAsyncTask {
        task_id: String,
        reply: oneshot::Sender<Result<bool, String>>,
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
    },
    /// Initialize session config state after initialize handshake.
    InitConfig {
        config: crate::session_config::SessionConfigState,
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
    ModelApplied {
        model_val: String,
        result: Result<(), String>,
        reply: oneshot::Sender<Result<Vec<crate::session_config::ConfigOption>, String>>,
        notify_client: bool,
    },
    /// Re-entry after `apply_flag_settings` control request.
    EffortApplied {
        effort_val: String,
        result: Result<(), String>,
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
        }
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
        let resumed = opts.spawn.resume.is_some();
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
        let (models, narrowed_by_gateway) =
            crate::session_model::narrow_models_for_gateway(settings_models, None, None);
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
        let mut effective_mode = if initial_mode == "auto" && is_auto_unavailable {
            crate::session_config::AUTO_MODE_FALLBACK.to_string()
        } else {
            initial_mode.to_string()
        };
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

        let current_effort = opts.spawn.effort.clone().or_else(|| {
            crate::session_config::settings_effort_for_model(
                settings.model_settings.as_ref(),
                settings.effort_level.as_deref(),
                current_model_info.as_ref(),
                &current_model_id,
            )
        });

        let config_options = crate::session_config::build_config_options(
            &modes,
            &selection.available_models,
            &current_model_id,
            &model_infos,
            current_effort.as_deref(),
            use_recommended_value,
        );

        // MAJOR 3 / P4 (acp-agent.js:8804-8807): Node seeds `apply_flag_settings
        // {effortLevel}` at session creation ONLY when the client opts in via the
        // AIR `useRecommendedValue` capability. A legacy (non-AIR) client gets no
        // effort flag at creation — the CLI resolves persisted effort itself.
        if use_recommended_value {
            if let Some(effort_opt) = config_options
                .iter()
                .find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID)
            {
                if effort_opt.current_value != "default" {
                    let _ = control
                        .send_request(json!({
                            "subtype": "apply_flag_settings",
                            "settings": {
                                "effortLevel": &effort_opt.current_value,
                            }
                        }))
                        .await;
                }
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
            ultracode: settings.ultracode,
            pre_plan_mode: None,
            provider_cache_key,
        };

        let _ = tx.send(Command::InitConfig {
            config: config_state,
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

    /// Register a `/mcp` prompt with the actor (rust-air-gaps G12). Sent before
    /// [`Self::send_prompt`] on the same channel, so the actor has the command
    /// when the prompt arrives.
    pub fn send_mcp(&self, uuid: String, command: crate::mcp_command::McpCommand) {
        let _ = self.tx.send(Command::McpPrompt { uuid, command });
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

/// Build the inbound `control_request` handler injected into the control
/// channel (phase 10). A `can_use_tool` request is routed to the session actor
/// via [`Command::Permission`] (which owns the `MapState` needed to
/// `ensure_tool_call_emitted`); the reply resolves with the `control_response`.
///
/// The handler models the SDK's `processControlRequest` dispatch for the
/// subtypes the adapter does not own (F3a): the `Z4` machine-only subtypes and
/// a `request_user_dialog` with no dialog handler return `None` (write
/// nothing); an `elicitation` with no handler answers the SDK default
/// `{action:"decline"}`; every other subtype answers an `error` response
/// carrying the thrown message (`Unsupported control request subtype: X`).
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
                // 2.11 (E-11): the fallback deny carries the tool_use_id; read
                // it before `frame` is moved into the command.
                let tool_use_id = frame
                    .pointer("/request/tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let _ = command_tx.send(Command::Permission {
                    frame,
                    reply: reply_tx,
                });
                Some(reply_rx.await.unwrap_or_else(|_| {
                    permission::deny_control_response(&request_id, &tool_use_id)
                }))
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
            } else if crate::control::SUPPRESSED_SUBTYPES.contains(&subtype)
                || subtype == "request_user_dialog"
            {
                // F3a (a): `Z4` is never answered; a dialog with no handler
                // stays silent so a capable client (F23b) or the worker's park
                // deadline settles it.
                None
            } else if subtype == "elicitation" {
                // F3a (b): the SDK's auto-decline is a SUCCESS verdict.
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
    mut process: crate::process::Process,
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
    let relay = relay_stream(stream, relay_tx, async {
        match tokio::time::timeout(CHILD_EXIT_WAIT, process.wait_with_stderr(&timings)).await {
            Ok(Ok(status)) => Some(!status.success()),
            _ => None,
        }
    });
    let actor = run_loop(
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
    );
    tokio::select! {
        _ = actor => {}
        _ = relay => {}
    }
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
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    logger: Option<&crate::logger::Logger>,
) {
    let errors = tasks.finish_all_with("stopped", &mut |update| {
        update_tx
            .send(SessionOutbound::RawUpdate(json!({
                "sessionId": session_id,
                "update": update,
            })))
            .map_err(|error| error.to_string())
    });
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
fn release_held_frames(replay: &mut std::collections::VecDeque<Value>, held: &mut Vec<Value>) {
    for frame in held.drain(..).rev() {
        replay.push_front(frame);
    }
}

/// Whether `line` is the result of a `/mcp` turn whose control run has started but
/// not finished: it must wait for the run's replacement (AG-4, S-02).
fn holds_for_mcp_run(
    line: &Value,
    machine: &TurnMachine,
    mcp: &crate::mcp_command::McpTurns,
) -> bool {
    if line.get("type").and_then(Value::as_str) != Some("result")
        || machine.result_is_autonomous(line)
    {
        return false;
    }
    machine
        .foreground_target()
        .and_then(|uuid| mcp.get(uuid))
        .is_some_and(|turn| turn.run_started && turn.prepared.is_none() && !turn.delivered)
}

/// How long a `can_use_tool` request waits for the consumer to reach its tool use
/// while a cancel's window may be open (`TOOL_USE_WAIT_MS`, `acp-agent.ts:437`).
const TOOL_USE_WAIT: Duration = Duration::from_millis(2_000);

/// A `can_use_tool` request parked until the consumer reaches its tool use.
type ParkedPermission = (String, Value, oneshot::Sender<Value>);

/// The `tool_use_id` of a `can_use_tool` control request.
fn can_use_tool_id(frame: &Value) -> String {
    frame
        .pointer("/request/tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
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
) {
    let mut ext = SessionExt::new(air, cwd, env)
        .with_caps(caps)
        .with_logger(logger);
    let mut machine = TurnMachine::new();
    if resumed {
        machine.mark_resumed();
    }
    let mut map_state = MapState::default();
    // Decision 6: every client gets the session cwd so tool titles/locations
    // are cwd-relative (Node passes `session.cwd` at every report site).
    map_state.set_cwd(ext.cwd.clone());
    // T-40: the client's `diffPatch` capability gates a Write's input diff and
    // turns on the exact patch of the approval preview and the PostToolUse hook.
    map_state.set_diff_patch(ext.caps.diff_patch);
    if ext.air.is_some() {
        map_state.configure_air(ext.cwd.clone());
    }
    // C-42: a client that gets a subagent's transcript (the historical
    // `_meta["subagent-transcript"]` capability), native subagent sessions, or
    // that passed `forwardSubagentText` receives the nested assistant
    // text/thinking, stamped with its parent (`forwardsSubagentText`,
    // `acp-agent.ts:10947-10953`).
    map_state.set_forward_subagent_text(forward_subagent_text);
    machine.set_forwards_subagent_text(forward_subagent_text);
    // AIR async-task runtime (item 3.4/3.5): enabled only for a client that
    // advertised the AIR `asyncTasks` capability.
    let mut tasks = AsyncTaskRuntime::new(ext.air_supports("asyncTasks"));
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
    // Pending permission round-trips (phase 10): `request_id -> JoinHandle`.
    // `Command::Cancel` aborts every one so a pending permission resolves as
    // `Cancelled` (R28 deny written back to the control writer, INV-21 / 27;
    // CUJ 2).
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // `can_use_tool` requests waiting for the consumer to reach their tool use in
    // the stopped-work window (F16b, B-62).
    let mut parked_permissions: Vec<ParkedPermission> = Vec::new();
    // Stream frames to process before reading more of the stream, and the session
    // frames held behind a `/mcp` result that waits for its control run (AG-4).
    let mut replay: std::collections::VecDeque<Value> = std::collections::VecDeque::new();
    let mut held_behind_mcp: Vec<Value> = Vec::new();
    // The stream closed while frames were still held behind an unfinished `/mcp`
    // run: they are processed without waiting (the run's replacement will never
    // come), so their results settle their turns.
    let mut stream_ended = false;
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
                    settle_events(
                        events,
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
                    );
                    // The cancelled turn's result may never come (that is why the
                    // backstop fired): close its delivery stretch here so partial
                    // streamed text cannot suppress the next turn's #453 fallback.
                    emitted_assistant_text = false;
                }
            }
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    break;
                };
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
                        machine.set_stops_workflows_on_cancel(stops);
                    }
                    Command::FinalStateSink(sink) => {
                        final_state_sink = Some(sink);
                    }
                    Command::Prompt {
                        uuid,
                        frame,
                        is_local_only,
                        prompt_text,
                        reply,
                    } => {
                        // The title context comes from the plain ACP text
                        // chunks (Node `onPrompt(params.prompt)`), never from
                        // the converted frame, whose blocks carry embedded
                        // `<context ref=…>` file contents and `[@name](uri)`
                        // links.
                        ext.titles.on_prompt(&prompt_text);
                        // G8 (Decision 16): a submitted `/goal …` prompt
                        // publishes its optimistic AIR update before the CLI's
                        // own frames are read (`publishGoalFromPrompt`,
                        // `acp-agent.ts:3952-3954`). Read the text before the
                        // frame moves to the writer.
                        let first_text = crate::goal::first_prompt_text(&frame).to_string();
                        let _ = control.send_user(frame).await;
                        machine.enqueue(Turn::new(uuid.clone(), is_local_only));
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
                                let prepared =
                                    crate::mcp_command::run_mcp_command(&control, &command).await;
                                let _ = command_tx.send(Command::McpPrepared {
                                    uuid: prompt_uuid,
                                    prepared,
                                });
                            });
                        }
                    }
                    Command::McpPrompt { uuid, command } => {
                        ext.mcp.insert(&uuid, command);
                    }
                    Command::McpPrepared { uuid, prepared } => {
                        if let Some(turn) = ext.mcp.get_mut(&uuid) {
                            turn.prepared = Some(prepared);
                        }
                        // The replacement is ready: the result (and the session
                        // frames queued behind it) that waited for it go on, in
                        // stream order.
                        release_held_frames(&mut replay, &mut held_behind_mcp);
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
                        // A request still waiting for its tool use is withdrawn by
                        // the interrupt: dropping `reply` makes control.rs answer
                        // with the R28 deny.
                        parked_permissions.clear();
                        // Review follow-up 3.17-D: clear every per-tool-call field
                        // tracker entry on cancel, so interrupted/refused tool calls
                        // that never emit a tool_result do not leak tracker entries.
                        map_state.clear_tool_tracker();
                        // G12: a cancelled `/mcp` turn publishes nothing, and its
                        // spawned control run's late result is dropped. What waited
                        // for that run goes on (the cancelled result is dropped).
                        ext.mcp.clear();
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
                        // Cancel exactly as `session/cancel` does (pending
                        // permissions refuse, swept turns settle, abandoned
                        // messages are cancelled), then interrupt.
                        abort_pending_permissions(
                            &mut pending_permissions,
                            &mut parked_permissions,
                        );
                        map_state.clear_tool_tracker();
                        ext.mcp.clear();
                        release_held_frames(&mut replay, &mut held_behind_mcp);
                        let events = machine.cancel();
                        settle_events(
                            events,
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
                    Command::CloseDone { reply } => {
                        let events = machine.on_stream_end();
                        settle_events(
                            events,
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
                        );
                        finish_async_tasks(&mut tasks, &update_tx, &session_id, ext.logger.as_deref());
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
                        handle_permission(
                            frame,
                            reply,
                            machine.foreground_target(),
                            &mut map_state,
                            &mut live_background,
                            &update_tx,
                            &session_id,
                            &mut pending_permissions,
                            &ext.cwd,
                            &available_modes,
                            pre_plan_mode,
                            home_dir.as_deref(),
                            None,
                            &command_tx,
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
                                machine.mark_steered(&uuid);
                                let _ = control.send_user(injected).await;
                                if let Some(goal) = ext.goal.note_prompt(&first_text, &uuid) {
                                    publish_goal(ext.air.is_some(), &goal, &update_tx, &session_id);
                                }
                            }
                            Some("startedNewTurn") => {
                                // Idle, no opt-in: start a detached turn like a
                                // normal prompt (it streams updates and settles
                                // internally; nothing awaits its oneshot).
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
                    Command::TaskStopped { task_id, reply } => {
                        let stopped = tasks.task_stopped(&task_id);
                        emit_raw_updates(stopped.updates, &update_tx, &session_id);
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
                    Command::TitleInfo {
                        transcript,
                        info,
                        cancelled,
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
                            info,
                            cancelled,
                        );
                    }
                    Command::TitleGenerated { title, fallback } => {
                        if let Some((sanitized_title, updated_at)) = ext.titles.on_title_generated(
                            title.as_deref(),
                            fallback
                                .as_ref()
                                .map(|(t, m)| (t.as_str(), m.as_deref())),
                        ) {
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
                        ext.config = config;
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
                                let mut mode_downgraded = false;
                                if ext.config.modes.current_mode_id == "auto"
                                    && auto_mode_downgrade_needed(new_model_info.as_ref())
                                {
                                    {
                                        ext.config.modes.current_mode_id = crate::session_config::AUTO_MODE_FALLBACK.to_string();
                                        let fallback = crate::session_config::AUTO_MODE_FALLBACK.to_string();
                                        let control = control.clone();
                                        tokio::spawn(async move {
                                            let _ = control.send_request(json!({
                                                "subtype": "set_permission_mode",
                                                "mode": &fallback,
                                            })).await;
                                        });
                                        mode_downgraded = true;
                                    }
                                }
                                let pinned_effort = ext.config.effort_pinned.clone();
                                let effort_was_pinned = pinned_effort.is_some();
                                let effort_pinned_for_new = effort_was_pinned && new_model_info.as_ref().is_some_and(|m| {
                                    m.supports_effort == Some(true) && m.supported_effort_levels.as_ref().is_some_and(|lvls| {
                                        lvls.iter().any(|l| Some(l) == pinned_effort.as_ref())
                                    })
                                });
                                let seed_effort = if effort_pinned_for_new {
                                    pinned_effort.clone()
                                } else {
                                    let settings = crate::settings::load(&ext.cwd, &ext.env);
                                    crate::session_config::settings_effort_for_model(
                                        settings.model_settings.as_ref(),
                                        settings.effort_level.as_deref(),
                                        new_model_info.as_ref(),
                                        &model_val,
                                    )
                                };
                                // MAJOR 3/4 / P4: rebuild config options for the new
                                // model honouring the client's `useRecommendedValue`
                                // (AIR) capability, matching the session-start seed.
                                let use_recommended_value = ext
                                    .air
                                    .as_ref()
                                    .is_some_and(|a| a.supports("recommendedValue"));
                                ext.config.config_options = crate::session_config::build_config_options(
                                    &ext.config.modes,
                                    &ext.config.available_models,
                                    &ext.config.current_model_id,
                                    &ext.config.model_infos,
                                    seed_effort.as_deref(),
                                    use_recommended_value,
                                );
                                // MAJOR 4 / P4 (acp-agent.js:7975-7989): sync effort on
                                // every switch for opted-in (AIR) clients; for a legacy
                                // non-AIR client only when a user pin was clamped away —
                                // and then send `effortLevel:null` (`toSdkEffortLevel
                                // (undefined)`) to clear the flag layer, never the
                                // displayed value.
                                let should_sync = use_recommended_value
                                    || (effort_was_pinned && !effort_pinned_for_new);
                                if should_sync {
                                    let new_effort_opt = ext.config.config_options.iter().find(|o| o.id == crate::session_config::EFFORT_CONFIG_ID);
                                    let new_effort = new_effort_opt.map(|o| o.current_value.clone());
                                    let sync_value = if use_recommended_value { new_effort.as_deref() } else { None };
                                    let effort_payload = crate::session_config::effort_flag_settings(sync_value, ext.config.ultracode);
                                    let control = control.clone();
                                    tokio::spawn(async move {
                                        let _ = control.send_request(json!({
                                            "subtype": "apply_flag_settings",
                                            "settings": effort_payload,
                                        })).await;
                                    });
                                    ext.config.effort_pinned = if effort_pinned_for_new { pinned_effort } else { None };
                                }
                                // Review PC-1 (3): the fallback updates, then config_option_update for
                                // an external switch; a user-driven change returns its options in `reply`.
                                let warn = mode_downgraded && !ext.auto_mode_fallback_warning_shown;
                                if warn {
                                    ext.auto_mode_fallback_warning_shown = true;
                                }
                                let downgraded_to = mode_downgraded.then_some(crate::session_config::AUTO_MODE_FALLBACK);
                                let updates = if notify_client {
                                    external_switch_updates(
                                        &session_id,
                                        &ext.config.config_options,
                                        downgraded_to,
                                        warn,
                                        ext.supports_notices(),
                                    )
                                } else {
                                    fallback_updates(
                                        &session_id,
                                        downgraded_to,
                                        warn,
                                        ext.supports_notices(),
                                    )
                                };
                                for update in updates {
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(update));
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
                        }
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
                                // `appliedEffortLevel`: "default" clears the flag layer,
                                // handing effort back to the CLI's own resolution.
                                ext.config.applied_effort =
                                    (effort_val != "default").then(|| effort_val.clone());
                                ext.config.effort_pinned = if effort_val != "default" {
                                    Some(effort_val)
                                } else {
                                    None
                                };
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
                    Command::HookCallback {
                        request_id,
                        callback_id,
                        input,
                        tool_use_id,
                        reply,
                    } => {
                        handle_hook_callback(
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
                            },
                            reply,
                        );
                    }
                }
            }
            msg = next_stream_line(&mut replay, &mut stream) => {
                let Some(line) = msg else {
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
                    let events = match child_exit {
                        // The child died: the SDK throws from `next()` and
                        // `failAllTurns` rejects every in-flight turn with that
                        // error, replaced by `lastErrorResultText` when an error
                        // result preceded it (`sdk.mjs` `readMessages`,
                        // `acp-agent.ts:8240-8330`).
                        Some(true) => {
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
                                    emit_session_transport_lost(&update_tx, &session_id);
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
                    settle_events(
                        events,
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
                    );
                    // 3.5: close every live async task at teardown (`finish_all`).
                    finish_async_tasks(&mut tasks, &update_tx, &session_id, ext.logger.as_deref());
                    ext.mcp.clear();
                    break;
                };
                // The relay's verdict on how the child exited (see `run`): the
                // stream's last frame before it closes.
                if let Some(abnormal) = child_exit_of(&line) {
                    child_exit = Some(abnormal);
                    continue;
                }
                // A `/mcp` turn's replacement comes from a spawned control run whose
                // replies arrive on THIS stream, behind the CLI's output and result.
                // Node awaits the run where the output shape appears and the result
                // follows; here a result that beats the run's completion waits for
                // its [`McpPrepared`], with every session frame behind it, while
                // control traffic (the run's own replies) keeps flowing.
                if !stream_ended
                    && (!held_behind_mcp.is_empty()
                        || holds_for_mcp_run(&line, &machine, &ext.mcp))
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
                    &mut tasks,
                    &mut ext,
                )
                .await;
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
    if let Some(sink) = final_state_sink {
        let _ = sink.send(Some((
            ext.config.modes.clone(),
            ext.config.config_options.clone(),
        )));
    }
}

/// Abort every pending permission round-trip and forget the parked ones.
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
        });
    });
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
    stored: Option<crate::session_store::StoredSessionInfo>,
    cancelled: bool,
) {
    if let Some(ref info) = stored {
        if let Some(ref custom) = info.custom_title {
            let last_mod = crate::session_store::iso8601(info.mtime_ms);
            if let Some((title, updated_at)) =
                ext.titles.adopt_custom_title(custom, Some(&last_mod))
            {
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
            }
            return;
        }
    }

    let fallback = stored.and_then(|info| {
        info.summary
            .map(|s| (s, Some(crate::session_store::iso8601(info.mtime_ms))))
    });

    if ext.titles.can_request(cancelled) {
        ext.titles.mark_settled();
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

    if let Some((fb_title, fb_mod)) = fallback {
        if !ext.titles.settled {
            let sanitized = crate::session_store::sanitize_title(&fb_title);
            if ext.titles.publish_needed(&sanitized) {
                ext.titles.last_title = Some(sanitized.clone());
                let mut update = json!({
                    "sessionUpdate": "session_info_update",
                    "title": sanitized,
                });
                if let Some(up) = fb_mod {
                    update["updatedAt"] = json!(up);
                }
                let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                    "sessionId": session_id,
                    "update": update,
                })));
            }
        }
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
    line: &Value,
    blocks: &[Value],
    map_state: &mut MapState,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    turn: Option<&str>,
) {
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
    for block in blocks {
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
        let updates = map::map_user_frame_block(line, block, map_state);
        note_tool_updates(&updates, true, turn, map_state);
        emit_updates(&map_state.filter_updates(updates), update_tx, session_id);
    }
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
struct HookCall<'a> {
    request_id: &'a str,
    callback_id: &'a str,
    input: &'a Value,
    tool_use_id: Option<&'a str>,
}

/// The channels a hook callback may use: session output, the actor's own command
/// queue (re-entry, e.g. PostModelSwitch), and the session id.
struct HookIo<'a> {
    update_tx: &'a mpsc::UnboundedSender<SessionOutbound>,
    command_tx: &'a mpsc::UnboundedSender<Command>,
    session_id: &'a str,
}

fn handle_hook_callback(
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
    } = io;
    match callback_id {
        crate::hooks::POST_TOOL_USE_HOOK_ID => {
            for update in crate::hooks::enter_plan_mode_updates(
                input,
                session_id,
                &mut ext.config.modes,
                &mut ext.config.config_options,
            ) {
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
                        let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
                            "sessionId": session_id,
                            "update": update
                        })));
                    }
                }
                map_state.finish_hook_tool_tracker(id);
                map_state.pending_hooks.remove(id);
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        crate::hooks::POST_MODEL_SWITCH_HOOK_ID => {
            if input.get("source").and_then(Value::as_str) != Some("sdk")
                && input.get("source").and_then(Value::as_str) != Some("resume")
            {
                if let Some(to_model) = input.get("to_model").and_then(Value::as_str) {
                    if let Some(target) = external_switch_target(
                        &ext.config.model_infos,
                        &ext.config.current_model_id,
                        to_model,
                    ) {
                        // The same bookkeeping a user-driven change takes (context
                        // window, auto-mode clamp, effort sync) runs in the actor's
                        // ModelApplied handler; no SDK set_model is sent (the SDK
                        // already switched). The reply receiver is discarded.
                        let (no_reply, _no_reply_rx) = oneshot::channel();
                        let _ = command_tx.send(Command::ModelApplied {
                            model_val: target,
                            result: Ok(()),
                            reply: no_reply,
                            notify_client: true,
                        });
                    }
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        crate::hooks::POST_COMPACT_HOOK_ID => {
            // PC-1.1 (acp-agent.ts:11350-11360): a root-session PostCompact records the
            // compaction summary; subagent compactions (with agent_id) do not.
            if input.get("agent_id").is_none() {
                if let Some(summary) = input.get("compact_summary").and_then(Value::as_str) {
                    ext.last_compaction_summary = Some(summary.to_string());
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        crate::hooks::TASK_CREATED_HOOK_ID => {
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
        crate::hooks::TASK_COMPLETED_HOOK_ID => {
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
        _ => {
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
) {
    // `active_goal` is emitted by the Claude runtime but is not part of the
    // SDK's public message union; Node handles it before the exhaustive
    // switch (`acp-agent.ts:5935-5942`) and never feeds the turn machine.
    // It is not one of the routed session types, so it is handled here.
    if msg.frame_type() == "active_goal" {
        let goal = crate::goal::GoalSnapshot::from_active_goal(&msg.line);
        if let Some(published) = ext.goal.note_runtime(goal) {
            publish_goal(ext.air.is_some(), &published, update_tx, session_id);
        }
        return;
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
        return;
    }
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
                tasks,
                ext,
            );
        }
        Route::Ignore => {}
    }
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
    let frame_type = line.get("type").and_then(Value::as_str).unwrap_or("");
    match frame_type {
        // The CLI re-emits the user message (--replay-user-messages); its echo
        // promotes the queued turn. The turn's own echo is not forwarded.
        "user" => {
            let mut matched_echo = false;
            let uuid_opt = line.get("uuid").and_then(Value::as_str);
            if let Some(uuid) = uuid_opt {
                // G8: the CLI echoed the submitted `/goal …` command, so a
                // result can now roll the optimistic update back
                // (`acp-agent.ts:7728-7730`).
                ext.goal.note_started(uuid);
                let events = machine.on_echo(uuid);
                if machine.take_text_stretch_closed() {
                    *emitted_assistant_text = false;
                }
                // Node's `activateTurn` clears `session.cancelled`, ending the
                // stopped-work window (Decision 18).
                if events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::Activated { .. }))
                {
                    map_state.end_stopped_work();
                    matched_echo = true;
                    ext.usage.reset_turn_scratch();
                    ext.failure_ctx.reset();
                    *assistant_had_error = false;
                }
                settle_events(
                    events,
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
            if !is_echo && !machine.drops_stopped_work_user_frame(&line) {
                if let Value::Array(blocks) = &content {
                    map::remember_user_frame_tool_names(&line, map_state);
                    handle_user_frame_blocks(
                        &line,
                        blocks,
                        map_state,
                        ext,
                        update_tx,
                        session_id,
                        machine.foreground_target(),
                    );
                } else {
                    let updates = map::map_user_frame(&line, map_state);
                    note_tool_updates(&updates, true, machine.foreground_target(), map_state);
                    emit_updates(&map_state.filter_updates(updates), update_tx, session_id);
                }
            }
        }
        "result" => {
            // Decided BEFORE `on_result` (a folded-prompt result names a pending
            // turn that `on_result` may promote or settle).
            let autonomous = machine.result_is_autonomous(&line);

            // G12 (Decision 21): a `/mcp` turn's CLI output shape was held
            // back above; publish its replacement (or the kept CLI text) here,
            // ahead of the result's usage update and the prompt response. The
            // chunk keeps the message id of the shape that carried the CLI
            // text (`messageIdForGrouping`). A held output shape is published
            // even for a non-success result (Node publishes it at the output
            // frame); a result-only turn follows Node's success-path
            // `shouldForwardResult`, and a non-success result keeps its text
            // for the failure message.
            if !autonomous {
                if let Some(uuid) = machine.foreground_target().map(str::to_string) {
                    let result_is_success = line.get("subtype").and_then(Value::as_str)
                        == Some("success")
                        && line.get("is_error").and_then(Value::as_bool) != Some(true)
                        && !matches!(
                            line.get("stop_reason").and_then(Value::as_str),
                            Some("max_tokens" | "refusal")
                        );
                    let mcp_chunk = ext.mcp.get_mut(&uuid).and_then(|turn| {
                        if turn.delivered {
                            return None;
                        }
                        if turn.original_output.is_none() && !result_is_success {
                            return None;
                        }
                        let result_text = line.get("result").and_then(Value::as_str).unwrap_or("");
                        let original = turn
                            .original_output
                            .clone()
                            .unwrap_or_else(|| result_text.to_string());
                        let rendered = turn
                            .prepared
                            .as_ref()
                            .and_then(|prepared| prepared.render(&original));
                        let text = rendered.unwrap_or(original);
                        let message_id = turn
                            .message_id
                            .clone()
                            .or_else(|| crate::mcp_command::message_id_of(&line));
                        turn.delivered = true;
                        Some((text, message_id, result_is_success))
                    });
                    if let Some((text, message_id, suppress_result)) = mcp_chunk {
                        // A published top-level message chunk feeds the title
                        // context (Node's `sendUpdate`, `acp-agent.ts:4618-4622`).
                        ext.titles.on_assistant_text(&text);
                        let mut chunk =
                            ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                        if let Some(id) = message_id.filter(|id| !id.is_empty()) {
                            chunk = chunk.message_id(MessageId::new(id));
                        }
                        let _ = update_tx.send(SessionOutbound::Update(SessionNotification::new(
                            session_id.to_string(),
                            SessionUpdate::AgentMessageChunk(chunk),
                        )));
                        if suppress_result {
                            if let Some(map) = line.as_object_mut() {
                                map.insert("result".to_string(), Value::String(String::new()));
                            }
                        }
                    }
                    ext.mcp.remove(&uuid);
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
            // releases a held async task, then `ensureActiveTurn` settles a held
            // turn and promotes the queue head (activation resets the per-turn
            // scratch), and only THEN does the result read the context window and
            // publish its `usage_update` — so an echo-less promotion reports no
            // stale usage from the previous turn.
            emit_raw_updates(tasks.release_held(), update_tx, session_id);
            let mut begin = machine.on_result_begin(&line);
            let begin_events = std::mem::take(&mut begin.events);
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
                settle_events(
                    begin_events,
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
                );
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

            let events = machine.on_result_finish(&line, *emitted_assistant_text, begin);
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
            settle_events(
                events,
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
            );
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
                            emit_commands_changed(&commands, &names, update_tx, session_id);
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
                "task_started" => {
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    let is_subagent = line.get("subagent_type").is_some();
                    machine.on_task_started(task_id, is_subagent);
                    // The Agent/Task call that started it and whether it runs in the
                    // background: a cancel does not stop a background subagent's
                    // work, so its output is not dropped (`liveBackgroundTasks`).
                    machine.note_task_meta(
                        task_id,
                        line.get("tool_use_id").and_then(Value::as_str),
                        line.get("is_backgrounded").and_then(Value::as_bool) == Some(true),
                        is_subagent,
                        line.get("task_type").and_then(Value::as_str),
                    );
                    // 3.7: a task registered for a tool call takes that call out
                    // of the turn's foreground work (`registerLiveTask`).
                    if let Some(parent) = line.get("tool_use_id").and_then(Value::as_str) {
                        map_state.books.forget_foreground(parent);
                    }
                    // Subagent attribution (10.5): record the Agent/Task tool
                    // call (`tool_use_id`) that spawned this task, so a
                    // subagent's `can_use_tool` is attributed to it.
                    if is_subagent {
                        if let Some(parent) = line.get("tool_use_id").and_then(Value::as_str) {
                            live_background.insert(task_id.to_string(), parent.to_string());
                        }
                    }
                    // 3.5: async-task runtime for AIR `asyncTasks` clients.
                    // Subagent / monitor tasks are ignored inside the runtime.
                    emit_raw_updates(tasks.task_started(&line), update_tx, session_id);
                }
                "task_progress" => {
                    // 3.5: async-task progress (AIR `asyncTasks` clients only).
                    emit_raw_updates(tasks.task_progress(&line), update_tx, session_id);
                }
                "task_notification" | "task_updated" => {
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    if is_terminal_task_update(&line) {
                        let events = machine.on_task_ended(task_id);
                        live_background.remove(task_id);
                        settle_events(
                            events,
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
                        );
                    }
                    // 3.5: async-task terminal / update. A `task_notification`
                    // releases a held task (spawn + deferred progress + state in
                    // Node order); a `task_updated` feeds the patch.
                    if subtype == "task_notification" {
                        emit_raw_updates(tasks.task_notification(&line), update_tx, session_id);
                    } else {
                        let patch = line.get("patch").cloned().unwrap_or(Value::Null);
                        emit_raw_updates(
                            tasks.task_updated(task_id, &patch),
                            update_tx,
                            session_id,
                        );
                    }
                }
                "compact_boundary" => {
                    if let Some(compact_meta) =
                        line.get("compact_metadata").and_then(Value::as_object)
                    {
                        let mut raw_output = serde_json::Map::new();
                        let trigger = compact_meta
                            .get("trigger")
                            .and_then(Value::as_str)
                            .map(|t| if t == "auto" { "automatic" } else { "manual" })
                            .unwrap_or("manual");
                        raw_output.insert("trigger".into(), json!(trigger));
                        if let Some(pre) = compact_meta.get("pre_tokens") {
                            raw_output.insert("preTokens".into(), pre.clone());
                        }
                        if let Some(post) = compact_meta.get("post_tokens") {
                            raw_output.insert("postTokens".into(), post.clone());
                        }
                        if let Some(dur) = compact_meta.get("duration_ms") {
                            raw_output.insert("durationMs".into(), dur.clone());
                        }
                        let mut tool_call_update = serde_json::Map::new();
                        tool_call_update.insert("sessionUpdate".into(), json!("tool_call"));
                        if let Some(uuid) = line.get("uuid").and_then(Value::as_str) {
                            tool_call_update.insert("toolCallId".into(), json!(uuid));
                        }
                        tool_call_update.insert("title".into(), json!("Compact conversation"));
                        tool_call_update.insert("kind".into(), json!("think"));
                        tool_call_update.insert("status".into(), json!("completed"));
                        tool_call_update.insert("rawOutput".into(), Value::Object(raw_output));
                        tool_call_update.insert(
                            "_meta".into(),
                            json!({
                                "claudeCode": {
                                    "toolName": "compact"
                                }
                            }),
                        );
                        let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
                            "sessionId": session_id,
                            "update": Value::Object(tool_call_update)
                        })));
                    }

                    let used_tokens = line
                        .pointer("/compact_metadata/post_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    ext.usage.last_usage = None;
                    ext.usage.last_total = Some(used_tokens);
                    ext.usage.used = used_tokens;
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
                    if state == "idle" {
                        let events = machine.on_idle();
                        // A held turn drained here, or a turn failed for the idle
                        // without a result, closes the stretch too.
                        if machine.take_text_stretch_closed() {
                            *emitted_assistant_text = false;
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
                        );
                    }
                }
                // G12 (Decision 21 / R11): a `/mcp` turn's CLI output is held
                // back here and replaced by the produced Markdown at the result
                // frame, exactly as Node awaits the producer at this shape.
                "local_command_output" => {
                    let content = line.get("content").and_then(Value::as_str).unwrap_or("");
                    let message_id = crate::mcp_command::message_id_of(&line);
                    if let Some(uuid) = machine.foreground_target() {
                        if let Some(turn) = ext.mcp.get_mut(uuid) {
                            if turn.original_output.is_none() {
                                turn.original_output = Some(content.to_string());
                                turn.message_id = message_id;
                            }
                        }
                    }
                }
                "status" => {
                    // A model request that serves none of the prompts a cancel
                    // stopped is new work (a follow-up after a background command
                    // finished): it ends the cancelled window, as an activation
                    // does (`acp-agent.ts:6002-6021`, N-06).
                    if machine.on_status(&line) {
                        map_state.end_stopped_work();
                    }
                }
                "commands_changed" => {
                    // D5 (Decision 9, `acp-agent.ts:6266-6288`): the frame's
                    // command list is authoritative and replaces the stored
                    // one; the advertisement filters it with the latched
                    // terminal list. Node also resets `session.skillPaths`
                    // here; the Rust port has no skill-path cache (no AIR
                    // skill meta is emitted), so the reset is vacuous.
                    let commands = line.get("commands").cloned().unwrap_or(Value::Null);
                    ext.commands = commands.as_array().cloned().unwrap_or_default();
                    let terminal = ext.terminal_slash_commands.clone().unwrap_or_default();
                    emit_commands_changed(&commands, &terminal, update_tx, session_id);
                }
                _ => {}
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
            // F16c (B-61): a model cycle Claude Code started on its own becomes the
            // foreground work at its first main-conversation output
            // (`startAutonomousCycle`, run before the message switch).
            machine.start_autonomous_cycle(&line);
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
                                    ext.usage.used = next_usage;
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
                                ext.usage.used = total;
                            }
                        }
                        if let Some(m) = model {
                            if m != "<synthetic>" {
                                ext.usage.last_model = Some(m.to_string());
                            }
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
            // G12 (Decision 21): a synthetic top-level assistant message is the
            // other shape of a local command's output. Hold its text (and its
            // message id) back for the replacement; do not map or emit it.
            if frame_type == "assistant"
                && line.get("parent_tool_use_id") == Some(&Value::Null)
                && line.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
            {
                let text = crate::mcp_command::assistant_text(line.get("message"));
                if let Some(uuid) = machine.foreground_target() {
                    if let Some(turn) = ext.mcp.get_mut(uuid) {
                        if turn.original_output.is_none() {
                            turn.original_output = text;
                            turn.message_id = crate::mcp_command::message_id_of(&line);
                        }
                        return;
                    }
                }
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
            let updates = if frame_type == "stream_event" {
                map::map_stream_event(&line, map_state)
            } else {
                // Consolidated assistant message: forward only the text/thinking
                // its stream did not already deliver (`unstreamedRemainder`,
                // Decision 18), plus tool/image/plan blocks.
                map::map_consolidated_assistant(&line, map_state)
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
            emit_updates(&map_state.filter_updates(updates), update_tx, session_id);
        }
        "conversation_reset" => {
            ext.task_state.clear();
            publish_task_plan(session_id, ext, update_tx, None);
            ext.titles.reset();
        }
        "prompt_suggestion" => {
            // Consumed without error (Item 3.3 / INV-65). Publishing is AIR-only and skipped.
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
                ext.config.modes.current_mode_id = mode.to_string();
                for opt in &mut ext.config.config_options {
                    if opt.id == crate::session_config::MODE_CONFIG_ID {
                        opt.current_value = mode.to_string();
                    }
                }
                let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "current_mode_update",
                        "currentModeId": mode,
                    }
                })));
                let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "config_option_update",
                        "configOptions": ext.config.config_options,
                    }
                })));
            }
        }
    }
    response
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
    frame: Value,
    reply: oneshot::Sender<Value>,
    turn: Option<&str>,
    map_state: &mut MapState,
    live_background: &mut HashMap<String, String>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    pending_permissions: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    cwd: &std::path::Path,
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
    if let Some(update) = surfaced {
        // 3.7: a permission-surfaced top-level call is foreground work.
        note_tool_updates(
            std::slice::from_ref(&update),
            parent.is_none(),
            turn,
            map_state,
        );
        emit_updates(
            &map_state.filter_updates(vec![update]),
            update_tx,
            session_id,
        );
    }

    // Build the `request_permission` params (10.1) before moving `can` into the
    // spawned round-trip task.
    let mut tool_call = permission::build_tool_call(
        &can.tool_name,
        &can.tool_input,
        &can.tool_use_id,
        parent.as_deref(),
        Some(cwd),
        map_state.is_air(),
    );
    if let Some(preview) = preview {
        tool_call["content"] = Value::Array(preview);
    }
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
    params.insert("sessionId".into(), Value::String(session_id.to_string()));
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
    params.insert("toolCall".into(), tool_call);
    let params = Value::Object(params);

    // Spawn the round-trip (D14): ask the agent layer to send
    // `session/request_permission` and await the client's answer. The answer is
    // handed to the actor (`Command::PermissionAnswered`), which resolves the
    // `control_response` (the `control.rs` on-request task writes the frame).
    // The command carries `reply`, so a `Cancel` that aborts this task after the
    // client answered still delivers the allow (INV-27); the handle is registered
    // so a Cancel before the answer aborts the round-trip (CUJ 2).
    let update_tx = update_tx.clone();
    let ctx = Arc::new(ctx);
    let command_tx = command_tx.clone();
    let handle = tokio::spawn(async move {
        let (ptx, prx) = oneshot::channel();
        let _ = update_tx.send(SessionOutbound::RequestPermission { params, reply: ptx });
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
            // closed / cancelled): refuse the tool (R28 deny) so the turn settles
            // on the next `result` (INV-21).
            Ok(Err(_)) | Err(_) => {
                let _ = reply.send(permission::deny_control_response(
                    &can.request_id,
                    &can.tool_use_id,
                ));
            }
        }
    });
    pending_permissions.insert(request_id, handle);
}

/// Process a batch of [`TurnEvent`]s in order (8.8 / review-04 wiring):
/// `FinalText` is emitted as an `agent_message_chunk` before the same batch's
/// `Settled` resolves the prompt.
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
) {
    // A `FinalText` precedes its turn's `Settled` in the same batch; collect it
    // and hand it to the prompt task so it can forward the chunk before the
    // response (deterministic, review-04 wiring).
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
                if let Some(reply) = pending.remove(prompt_uuid) {
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
                    if claude_kind == crate::session_failure::ClaudeFailureKind::AuthRequired
                        || *kind == FailureKind::AuthRequired
                    {
                        reply_error_after_flush(
                            update_tx,
                            reply,
                            SessionError::PromptFailed {
                                kind: FailureKind::AuthRequired,
                                message: message.clone(),
                                assistant_had_error: failure_ctx.has_error(),
                            },
                        );
                    } else if air_session_failure && *kind != FailureKind::SessionEnded {
                        let policy = crate::session_failure::failure_policy(claude_kind);
                        let final_title = if title.is_empty() {
                            policy.fallback_title.to_string()
                        } else {
                            title
                        };
                        let failure = crate::session_failure::PublishedSessionFailure {
                            id: format!("{prompt_uuid}:error"),
                            revision: 1,
                            kind: claude_kind,
                            category: policy.category,
                            severity: "error",
                            title: final_title,
                            details: None,
                            reason: None,
                            actions: policy.actions.iter().map(|s| (*s).to_string()).collect(),
                        };
                        let sf_meta = crate::session_failure::session_failure_meta(&failure);
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
                        // F3a (f): the end-of-stream rejection rewritten from
                        // `lastErrorResultText` keeps its own SDK error identity.
                        let err = match message.strip_prefix(CHILD_EXIT_PREFIX) {
                            // An AIR client reads the queued turns' rejection as
                            // `internalErrorForClient({errorKind:"transport_lost"})`
                            // (`acp-agent.ts:8325-8330`).
                            Some(_) if air_session_failure => SessionError::TransportLost,
                            Some(text) => SessionError::ChildErrorResult {
                                text: text.to_string(),
                            },
                            None => SessionError::PromptFailed {
                                kind: *kind,
                                message: message.clone(),
                                assistant_had_error: failure_ctx.has_error(),
                            },
                        };
                        reply_error_after_flush(update_tx, reply, err);
                    }
                }
            }
            TurnEvent::Activated { .. } => {
                high_water.fetch_max(1, Ordering::Relaxed);
            }
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

/// Publish the session-scoped `transport_lost` failure an AIR client gets when
/// the process dies with no turn to carry it (`publishSessionFailure(
/// "transport_lost", {turnScoped:false})`, `acp-agent.ts:8289-8296`): a
/// `session_info_update` whose `_meta` carries the failure.
fn emit_session_transport_lost(
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    use crate::session_failure::{ClaudeFailureKind, PublishedSessionFailure};
    let policy = crate::session_failure::failure_policy(ClaudeFailureKind::TransportLost);
    let failure = PublishedSessionFailure {
        id: format!("{session_id}:session-error:{}:1", crate::agent::uuid_v4()),
        revision: 1,
        kind: ClaudeFailureKind::TransportLost,
        category: policy.category,
        severity: "error",
        title: policy.fallback_title.to_string(),
        details: None,
        reason: None,
        actions: policy.actions.iter().map(|s| (*s).to_string()).collect(),
    };
    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "session_info_update",
            "_meta": crate::session_failure::session_failure_meta(&failure),
        },
    })));
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
                    map_state.books.forget(&id);
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
            _ => {}
        }
    }
}

/// Run the unfinished-tool post-filter (3.7, Decision 7) over a batch of turn
/// events, send its closing `tool_call_update`s, then hand the (possibly
/// rewritten) events to [`handle_events`].
#[allow(clippy::too_many_arguments)]
fn settle_events(
    events: Vec<TurnEvent>,
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
) {
    // Node's `activateTurn` clears `session.cancelled`, ending the stopped-work
    // window whenever a turn activates (whether by user echo or ensure_active_turn).
    if events
        .iter()
        .any(|e| matches!(e, TurnEvent::Activated { .. }))
    {
        map_state.end_stopped_work();
    }
    let background: std::collections::HashSet<&str> =
        live_background.values().map(String::as_str).collect();
    let (events, closing) = crate::unfinished::gate_events(events, map_state, &background);
    emit_updates(&map_state.filter_updates(closing), update_tx, session_id);
    // Node collects every top-level `agent_message_chunk` in `sendUpdate`
    // (`acp-agent.ts:4618-4622`): the result-text fallback (`FinalText`) is one
    // of them, so it feeds the title context too.
    for event in &events {
        if let TurnEvent::FinalText { text } = event {
            titles.on_assistant_text(text);
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
    );
}

/// Emit a `commands_changed` / `system/init` command list as an
/// `AvailableCommandsUpdate`, filtered by the latched terminal-bound names.
fn emit_commands_changed(
    commands: &Value,
    terminal: &[String],
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
) {
    let update = map::map_commands_changed(commands, terminal);
    // Decision 8: raw JSON so a command without an argument hint carries
    // `input: null` like Node (the typed update would omit the field).
    emit_raw_updates(
        vec![map::available_commands_json(&update)],
        update_tx,
        session_id,
    );
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

#[cfg(test)]
mod tests_t1fa;

#[cfg(test)]
mod tests_t1ga;

#[cfg(test)]
mod tests_t1ha;

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
            &line,
            blocks,
            &mut map_state,
            &mut ext,
            &update_tx,
            "s",
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
            &line,
            blocks,
            &mut map_state,
            &mut ext,
            &update_tx,
            "s",
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
            None,
            crate::client_caps::ClientCapabilities::default(),
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
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
            std::path::Path::new("/repo"),
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
            config: config_for("model-with-auto", true),
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
            config: config_for("model-without-auto", false),
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
                on_request: Some(build_permission_request_handler(tx.clone())),
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
            config: crate::session_config::SessionConfigState {
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
            },
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
            response["response"]["response"]["behavior"], "deny",
            "an option that was never offered is refused, so no silent Auto rewrite"
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
            frame,
            reply_tx,
            None,
            &mut map_state,
            &mut live_background,
            &utx,
            "s",
            &mut pending_permissions,
            std::path::Path::new(""),
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
    fn spawn_bare_actor() -> (
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
