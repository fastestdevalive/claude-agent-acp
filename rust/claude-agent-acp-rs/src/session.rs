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
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCallStatus,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::air::AirClient;
use crate::async_tasks::AsyncTaskRuntime;
use crate::control::{Control, ControlOptions, InitializeOptions, RequestHandler};
use crate::dispatch::{self, Route, StreamMsg};
use crate::map::{self, MapState, MsgRole};
use crate::permission;
use crate::permission_options;
use crate::process::{self, SpawnOptions, Timings};
use crate::session_ext::SessionExt;
use crate::turn::{FailureKind, Turn, TurnEvent, TurnMachine};

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
    /// An untyped AIR `session/update` (item 3.3), value = `{"sessionId",⋯,"update",{⋯}}`,
    /// forwarded as `UntypedMessage::new("session/update", value)` (async-task
    /// spawns/progress/state-updates).
    RawUpdate(Value),
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
}

impl std::fmt::Debug for PromptReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptReply")
            .field("stop_reason", &self.stop_reason)
            .field("usage", &self.usage)
            .field("quota", &self.quota)
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
pub(crate) enum Command {
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
    /// Update context window size from background get_context_usage or initialize info.
    UpdateContextWindow { size: u64 },
    /// Initialize session config state after initialize handshake.
    InitConfig {
        config: crate::session_config::SessionConfigState,
    },
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
    /// ExitPlanMode's setMode selection effect: Node `publishCurrent` +
    /// `updateConfigOption` after an ExitPlanMode allow-with-setMode (no CLI
    /// control request — the mode is applied to session state only, and
    /// `current_mode_update` + `config_option_update` are published to the
    /// client).
    ExitPlanModeApplied { mode: String },
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
    /// Environment snapshot for hermetic adapter reads.
    pub env: crate::env_snapshot::EnvSnapshot,
}

impl SessionOptions {
    /// Build options from a `ServeOptions`-style config for a fresh session.
    pub fn new(spawn: SpawnOptions, timings: Timings, initialize: InitializeOptions) -> Self {
        Self {
            spawn,
            timings,
            initialize,
            air: None,
            env: crate::env_snapshot::EnvSnapshot::default(),
        }
    }

    /// Set the parsed AIR client (item 3.2).
    #[must_use]
    pub fn with_air(mut self, air: Option<AirClient>) -> Self {
        self.air = air;
        self
    }

    /// Set the environment snapshot.
    #[must_use]
    pub fn with_env(mut self, env: crate::env_snapshot::EnvSnapshot) -> Self {
        self.env = env;
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
            opts.spawn
                .default_cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default(),
            resumed,
            opts.env.clone(),
        ));

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
        let models = sdk_models.clone();

        let cwd = opts
            .spawn
            .default_cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let settings = crate::settings::load(&cwd, &opts.env);

        let is_resumed = opts.spawn.resume.is_some();
        // MAJOR 2 / P4: read the resumed tail WITH the model (acp-agent.js:8290-8293),
        // so the resumed-model hint actually reaches `get_available_models` and
        // re-selects the model the resumed transcript last used.
        let resumed_model_hint = opts.spawn.resume.as_ref().and_then(|id| {
            crate::resumed_session::read_resumed_tail_with_env(id, true, &opts.env).model
        });

        let (available_models, current_model_id, needs_set_model) =
            crate::session_config::get_available_models(
                &models,
                &sdk_models,
                settings.model.as_deref(),
                opts.env.get("ANTHROPIC_MODEL"),
                is_resumed,
                resumed_model_hint.as_deref(),
            );

        if needs_set_model {
            let outcome = control
                .send_request(json!({
                    "subtype": "set_model",
                    "model": &current_model_id,
                }))
                .await;
            if !is_resumed {
                if let Err(e) = outcome {
                    return Err(SessionError::Start(format!("set_model failed: {e}")));
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
            &available_models,
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
            available_models: available_models.clone(),
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
        };

        let _ = tx.send(Command::InitConfig {
            config: config_state,
        });

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

    /// The `commands` array from the `initialize` handshake — the source for
    /// the proactive `available_commands_update` (phase 15, Item 1).
    pub fn commands(&self) -> &[Value] {
        &self.commands
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
            .map_err(|_| "Session ended".to_string())?;
        reply_rx.await.map_err(|_| "Session ended".to_string())?
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
            .map_err(|_| "Session ended".to_string())?;
        reply_rx.await.map_err(|_| "Session ended".to_string())?
    }

    /// Fetch the actor's LIVE modes + config options (MAJOR 5).
    pub async fn live_state(
        &self,
    ) -> Result<
        (
            crate::session_config::ModesState,
            Vec<crate::session_config::ConfigOption>,
        ),
        String,
    > {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::GetState { reply })
            .map_err(|_| "Session ended".to_string())?;
        reply_rx.await.map_err(|_| "Session ended".to_string())?
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
                reply_rx.await.unwrap_or_else(|_| {
                    permission::deny_control_response(&request_id, &tool_use_id)
                })
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
                reply_rx
                    .await
                    .unwrap_or_else(|_| crate::hooks::hook_control_response_error(&request_id))
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
    air: Option<AirClient>,
    cwd: PathBuf,
    resumed: bool,
    env: crate::env_snapshot::EnvSnapshot,
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
        air,
        cwd,
        resumed,
        env,
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
    air: Option<AirClient>,
    cwd: PathBuf,
    resumed: bool,
    env: crate::env_snapshot::EnvSnapshot,
) {
    let mut ext = SessionExt::new(air, cwd, env);
    let mut machine = TurnMachine::new();
    if resumed {
        machine.mark_resumed();
    }
    let mut map_state = MapState::default();
    if ext.air.is_some() {
        map_state.configure_air(ext.cwd.clone());
    }
    // AIR async-task runtime (item 3.4/3.5): enabled only for a client that
    // advertised the AIR `asyncTasks` capability.
    let mut tasks = AsyncTaskRuntime::new(ext.air_supports("asyncTasks"));
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
                    settle_events(
                        events,
                        &mut map_state,
                        &live_background,
                        &mut pending,
                        &update_tx,
                        &session_id,
                        &high_water,
                        assistant_had_error,                    );
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
                        // Review follow-up 3.17-D: clear every per-tool-call field
                        // tracker entry on cancel, so interrupted/refused tool calls
                        // that never emit a tool_result do not leak tracker entries.
                        map_state.clear_tool_tracker();
                        // 8.9: settle swept queued turns, then interrupt.
                        let events = machine.cancel();
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
                        assistant_had_error,                    );
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
                        let control = control.clone();
                        let command_tx = command_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(receipt) = control
                                .send_request(json!({"subtype": "interrupt"}))
                                .await
                            {
                                let still_queued = crate::control::still_queued_of(&receipt);
                                let _ = command_tx
                                    .send(Command::InterruptReceipt { still_queued });
                            }
                        });
                    }
                    Command::Close { reply } => {
                        // Cancel exactly as `session/cancel` does (pending
                        // permissions refuse, swept turns settle, abandoned
                        // messages are cancelled), then interrupt.
                        for handle in pending_permissions.values() {
                            handle.abort();
                        }
                        pending_permissions.clear();
                        map_state.clear_tool_tracker();
                        let events = machine.cancel();
                        settle_events(
                            events,
                            &mut map_state,
                            &live_background,
                            &mut pending,
                            &update_tx,
                            &session_id,
                            &high_water,
                            assistant_had_error,
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
                        // The interrupt is written on a spawned task (INV-27) and
                        // bounded, so the child is not killed before it is on the
                        // wire; the actor keeps routing frames meanwhile.
                        let control = control.clone();
                        let command_tx = command_tx.clone();
                        tokio::spawn(async move {
                            let _ = tokio::time::timeout(
                                Duration::from_millis(500),
                                control.send_request(json!({"subtype": "interrupt"})),
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
                            assistant_had_error,
                        );
                        emit_raw_updates(tasks.finish_all("stopped"), &update_tx, &session_id);
                        let (flush_tx, flush_rx) = oneshot::channel();
                        let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                        tokio::spawn(async move {
                            let _ = flush_rx.await;
                            let _ = reply.send(());
                        });
                        break;
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
                        let available_modes: Vec<String> = ext
                            .config
                            .modes
                            .available_modes
                            .iter()
                            .map(|m| m.id.clone())
                            .collect();
                        let pre_plan_mode = ext.config.pre_plan_mode.as_deref();
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
                            // injects nothing for a stopped task. Rust has no
                            // `notices`, so it lands in the transcript.
                            let notice = SessionNotification::new(
                                session_id.clone(),
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    ContentBlock::Text(TextContent::new(format!(
                                        "**Task stopped by user:** {name}."
                                    ))),
                                )),
                            );
                            let _ = update_tx.send(SessionOutbound::Update(notice));
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
                    Command::UpdateContextWindow { size } => {
                        // MAJOR 1 / P4 (acp-agent.js:7884-7891): the background
                        // get_context_usage answer only refines a NON-authoritative
                        // window. Once the window is authoritative (seeded from the
                        // model row, learned from `result.modelUsage`/cache), the
                        // background result is dropped.
                        if !ext.usage.authoritative {
                            ext.usage.update_window_if_positive(Some(size));
                        }
                    }
                    Command::InitConfig { config } => {
                        // MAJOR 1 / P4: seed the context window at session creation
                        // from the current model's row (id/displayName/description
                        // heuristic -> 1M, else the default), mirroring Node's
                        // `immediateContextWindow` (acp-agent.js:10577-10585). The
                        // cross-session `contextWindowCache` is a PARITY pending row.
                        if let Some(info) = config
                            .model_infos
                            .iter()
                            .find(|m| m.value == config.current_model_id)
                        {
                            let texts = [
                                info.resolved_model.as_deref(),
                                Some(info.value.as_str()),
                                Some(info.display_name.as_str()),
                                Some(info.description.as_str()),
                            ];
                            let seed = crate::usage::infer_context_window_from_model(
                                &texts.into_iter().flatten().collect::<Vec<_>>(),
                            );
                            if let Some(size) = seed {
                                if !ext.usage.authoritative {
                                    ext.usage.context_window = size;
                                }
                            }
                        }
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
                                Err(_) => Err("Invalid Mode".to_string()),
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
                                        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                            "sessionId": session_id,
                                            "update": {
                                                "sessionUpdate": "agent_message_chunk",
                                                "content": {
                                                    "type": "text",
                                                    "text": "**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead.",
                                                }
                                            }
                                        })));
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
                    Command::ExitPlanModeApplied { mode } => {
                        // Node `publishCurrent` + `updateConfigOption` after an
                        // ExitPlanMode allow-with-setMode (`acp-agent.js:7665-7677`):
                        // update the session mode/config state and publish both
                        // updates to the client. No CLI control request is issued
                        // (unlike `session/set_mode`).
                        ext.config.modes.current_mode_id = mode.clone();
                        for opt in &mut ext.config.config_options {
                            if opt.id == crate::session_config::MODE_CONFIG_ID {
                                opt.current_value = mode.clone();
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
                                    Err(_) => Err("Invalid Mode".to_string()),
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
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                        "sessionId": session_id,
                                        "update": {
                                            "sessionUpdate": "agent_message_chunk",
                                            "content": {
                                                "type": "text",
                                                "text": "**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead.",
                                            }
                                        }
                                    })));
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
                                    // MAJOR 1 / P4: re-seed the context window on a model
                                    // switch (acp-agent.js:7917-7919) — sized from the new
                                    // model's row heuristic unless already authoritative.
                                    let seed = context_window_seed(new_model_info.as_ref(), &model_val);
                                    ext.usage.context_window =
                                        seed.unwrap_or(crate::usage::DEFAULT_CONTEXT_WINDOW);
                                    ext.usage.authoritative = false;
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
                                    external_switch_updates(&session_id, &ext.config.config_options, downgraded_to, warn)
                                } else {
                                    fallback_updates(&session_id, downgraded_to, warn)
                                };
                                for update in updates {
                                    let _ = update_tx.send(SessionOutbound::RawUpdate(update));
                                }
                                if model_changed {
                                    let control = control.clone();
                                    let tx = command_tx.clone();
                                    tokio::spawn(async move {
                                        if let Ok(res) = control
                                            .send_request(json!({"subtype": "get_context_usage", "detail": "summary"}))
                                            .await
                                        {
                                            if let Some(raw) = crate::control::raw_max_tokens(&res) {
                                                if raw > 0 {
                                                    let _ = tx.send(Command::UpdateContextWindow { size: raw });
                                                }
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
            msg = stream.recv() => {
                let Some(line) = msg else {
                    // Stream EOF / codec death (8.9): settle/reject every turn.
                    let events = machine.on_stream_end();
                    settle_events(
                        events,
                        &mut map_state,
                        &live_background,
                        &mut pending,
                        &update_tx,
                        &session_id,
                        &high_water,
                        assistant_had_error,                    );
                    // 3.5: close every live async task at teardown (`finish_all`).
                    emit_raw_updates(tasks.finish_all("stopped"), &update_tx, &session_id);
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
                    &mut tasks,
                    &mut ext,
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

fn publish_task_plan(
    session_id: &str,
    ext: &mut SessionExt,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
) {
    let entries = crate::hooks::task_state_to_plan_entries(&ext.task_state);

    if ext.air_supports("air") {
        let serialized = serde_json::to_string(&entries).unwrap_or_default();
        if ext.last_published_task_plan.as_deref() == Some(&serialized) {
            return;
        }
        ext.last_published_task_plan = Some(serialized);
    }

    let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "plan",
            "entries": entries
        }
    })));
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

/// Review PC-1 (5): the context-window seed for a switched-to model. A listed model is read from
/// its fields; an unlisted id is inferred from the id itself (acp-agent.ts:13729-13747).
fn context_window_seed(
    info: Option<&crate::session_config::ModelInfo>,
    model_val: &str,
) -> Option<u64> {
    match info {
        Some(m) => {
            let texts = [
                m.resolved_model.as_deref(),
                Some(m.value.as_str()),
                Some(m.display_name.as_str()),
                Some(m.description.as_str()),
            ];
            crate::usage::infer_context_window_from_model(
                &texts.into_iter().flatten().collect::<Vec<_>>(),
            )
        }
        None => crate::usage::infer_context_window_from_model(&[model_val]),
    }
}

/// The current_mode_update and fallback warning of a model-driven auto-mode downgrade.
fn fallback_updates(session_id: &str, downgraded_to: Option<&str>, warn: bool) -> Vec<Value> {
    let Some(mode) = downgraded_to else {
        return Vec::new();
    };
    let mut out = vec![json!({
        "sessionId": session_id,
        "update": {"sessionUpdate": "current_mode_update", "currentModeId": mode},
    })];
    if warn {
        out.push(json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "text",
                    "text": "**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead.",
                }
            }
        }));
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
) -> Vec<Value> {
    let mut out = fallback_updates(session_id, downgraded_to, warn);
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

/// PC-1.5 (acp-agent.ts:13226-13245): the tool_call_update that resolves a Task* call a
/// permission prompt surfaced. Its status follows the result; its rawOutput is the result content.
fn task_permission_resolution(
    tool_use_id: &str,
    name: &str,
    is_error: bool,
    content: &Value,
    parent_tool_use_id: Option<&str>,
) -> Value {
    let mut claude_code = json!({"toolName": name});
    if let Some(parent) = parent_tool_use_id {
        claude_code["parentToolUseId"] = json!(parent);
    }
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_use_id,
        "status": if is_error { "failed" } else { "completed" },
        "rawOutput": content,
        "_meta": {"claudeCode": claude_code},
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
                    let mut update = crate::hooks::build_post_tool_use_update(
                        id,
                        &entry.tool_name,
                        entry.parent_tool_use_id.as_deref(),
                        &tool_response,
                    );
                    map_state.filter_hook_tool_call_update(id, &mut update);
                    let _ = update_tx.send(SessionOutbound::RawUpdate(serde_json::json!({
                        "sessionId": session_id,
                        "update": update
                    })));
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
            if let Some(task_id) = input.get("task_id").and_then(Value::as_str) {
                if let Some(task_subject) = input.get("task_subject").and_then(Value::as_str) {
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
                        publish_task_plan(session_id, ext, update_tx);
                    }
                }
            }
            let _ = reply.send(crate::hooks::hook_control_response_success(request_id));
        }
        crate::hooks::TASK_COMPLETED_HOOK_ID => {
            if let Some(task_id) = input.get("task_id").and_then(Value::as_str) {
                if let Some((_, entry)) = ext.task_state.iter_mut().find(|(id, _)| id == task_id) {
                    if entry.status != "completed" {
                        entry.status = "completed".to_string();
                        publish_task_plan(session_id, ext, update_tx);
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
    tasks: &mut AsyncTaskRuntime,
    ext: &mut SessionExt,
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
                let events = machine.on_echo(uuid);
                // Node's `activateTurn` clears `session.cancelled`, ending the
                // stopped-work window (Decision 18).
                if events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::Activated { .. }))
                {
                    map_state.end_stopped_work();
                    matched_echo = true;
                    ext.usage.reset_turn_scratch();
                }
                settle_events(
                    events,
                    map_state,
                    live_background,
                    pending,
                    update_tx,
                    session_id,
                    high_water,
                    *assistant_had_error,
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
            if !is_echo {
                if let Value::Array(blocks) = &content {
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
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                            if let Some(tool_use_id) =
                                block.get("tool_use_id").and_then(Value::as_str)
                            {
                                ext.hooks.complete(tool_use_id);
                                // PC-1.2 (Node onRelease, tools.ts:401-426): an entry released by the
                                // 30 s expiry also finishes its hook tool tracker.
                                for (released_id, _) in
                                    ext.hooks.prune_expired(std::time::Instant::now())
                                {
                                    map_state.finish_hook_tool_tracker(&released_id);
                                    map_state.pending_hooks.remove(&released_id);
                                }

                                if let Some(tool_use) = map_state.tool_use_from_cache(tool_use_id) {
                                    let name =
                                        tool_use.get("name").and_then(Value::as_str).unwrap_or("");
                                    let is_err = block
                                        .get("is_error")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    // PC-1.5: a Task* call a permission prompt surfaced is resolved here
                                    // (the Task branches below never emit a tool_call_update).
                                    if map_state.has_emitted(tool_use_id)
                                        && crate::tools::is_task_tool(name)
                                    {
                                        let _ = update_tx.send(SessionOutbound::RawUpdate(json!({
                                            "sessionId": session_id,
                                            "update": task_permission_resolution(
                                                tool_use_id,
                                                name,
                                                is_err,
                                                &block["content"],
                                                line.get("parent_tool_use_id").and_then(Value::as_str),
                                            ),
                                        })));
                                    }
                                    if !is_err {
                                        let input = tool_use.get("input").unwrap_or(&Value::Null);
                                        if name == "TaskCreate" {
                                            let parsed_id = crate::hooks::parse_task_create_output(
                                                &block["content"],
                                            );
                                            crate::hooks::apply_task_create(
                                                &mut ext.task_state,
                                                input,
                                                parsed_id.as_deref(),
                                            );
                                            publish_task_plan(session_id, ext, update_tx);
                                        } else if name == "TaskUpdate" {
                                            // PC-1.5: the success gate and status fallback live in hooks.rs.
                                            if crate::hooks::apply_task_update_output(
                                                &mut ext.task_state,
                                                input,
                                                &message_tool_use_result,
                                                &block["content"],
                                            ) {
                                                publish_task_plan(session_id, ext, update_tx);
                                            }
                                        } else if name == "TaskList" {
                                            // PC-1.5: unparseable output leaves the state untouched.
                                            if crate::hooks::apply_task_list_output(
                                                &mut ext.task_state,
                                                &message_tool_use_result,
                                                &block["content"],
                                            ) {
                                                publish_task_plan(session_id, ext, update_tx);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                let updates = map::map_consolidated(&content, MsgRole::User, map_state);
                note_tool_updates(&updates, true, machine.foreground_target(), map_state);
                emit_updates(&map_state.filter_updates(updates), update_tx, session_id);
            }
        }
        "result" => {
            // Decided BEFORE `on_result` (a folded-prompt result names a pending
            // turn that `on_result` may promote or settle).
            let autonomous = machine.result_is_autonomous(&line);

            // Decision 9 / 2.1: update context window if matchingModelUsage has contextWindow > 0
            if let Some(model_usage) = line.get("modelUsage") {
                if let Some(ref current_model) = ext.usage.last_model {
                    if let Some((_key, val)) =
                        crate::usage::get_matching_model_usage(model_usage, current_model)
                    {
                        let cw = val.get("contextWindow").and_then(Value::as_u64);
                        ext.usage.update_window_if_positive(cw);
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

            let events = machine.on_result(&line, *emitted_assistant_text);
            if events
                .iter()
                .any(|e| matches!(e, TurnEvent::Activated { .. }))
            {
                ext.usage.reset_turn_scratch();
            }
            // Upstream clears emittedAssistantText in the result finally for
            // non-autonomous results.
            if !autonomous {
                *emitted_assistant_text = false;
            }
            // 3.5: release any held async task without a tool-call id when the
            // prompt result ends the model turn, BEFORE the flush barrier (the
            // Settled arm enqueues the flush after these spawns).
            emit_raw_updates(tasks.release_held(), update_tx, session_id);
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
                            *assistant_had_error,
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
                        settle_events(
                            events,
                            map_state,
                            live_background,
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
                    }
                }
            }
            // The Node snapshots `lastAssistantError` from a top-level `error`
            // on the assistant frame (`acp-agent.ts:4333-4335`); record it so
            // the turn's failure can attach `data.errorKind` only when present.
            if frame_type == "assistant" && line.get("error").is_some() {
                *assistant_had_error = true;
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
            if frame_type == "assistant" {
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
                }
            }
            // 3.7: a subagent's output (`parent_tool_use_id` set) is not this
            // turn's foreground work.
            let top_level = line.get("parent_tool_use_id").is_none_or(Value::is_null);
            note_tool_updates(&updates, top_level, machine.foreground_target(), map_state);
            emit_updates(&map_state.filter_updates(updates), update_tx, session_id);
        }
        "prompt_suggestion" => {
            // Consumed without error (Item 3.3 / INV-65). Publishing is AIR-only and skipped.
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
    if let Some(update) = permission::ensure_tool_call_emitted(
        &can.tool_name,
        &can.tool_input,
        &can.tool_use_id,
        parent.as_deref(),
        map_state,
    ) {
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
    let tool_call = permission::build_tool_call(
        &can.tool_name,
        &can.tool_input,
        &can.tool_use_id,
        parent.as_deref(),
    );
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
        pre_plan_mode: pre_plan_mode.map(String::from),
        context_used_percent,
        home_dir: home_dir.map(|p| p.to_path_buf()),
    };
    let options = permission_options::build_options(&ctx);
    let mut params = serde_json::Map::new();
    params.insert("options".into(), Value::Array(options));
    params.insert("sessionId".into(), Value::String(session_id.to_string()));
    params.insert("toolCall".into(), tool_call);
    let params = Value::Object(params);

    // Spawn the round-trip (D14): ask the agent layer to send
    // `session/request_permission` and await the outcome, map it, resolve `reply`
    // (the `control.rs` on-request task writes the response frame). The handle is
    // registered so `Command::Cancel` can abort it (CUJ 2, INV-27).
    let update_tx = update_tx.clone();
    let ctx = Arc::new(ctx);
    let command_tx = command_tx.clone();
    let handle = tokio::spawn(async move {
        let (ptx, prx) = oneshot::channel();
        let _ = update_tx.send(SessionOutbound::RequestPermission { params, reply: ptx });
        let response = match prx.await {
            Ok(Ok(value)) => permission::permission_control_response(
                &value,
                &ctx,
                &can.request_id,
                &can.tool_use_id,
                &can.tool_input,
            ),
            // The request errored or the round-trip was dropped (e.g. the client
            // closed / cancelled): refuse the tool (R28 deny) so the turn settles
            // on the next `result` (INV-21).
            Ok(Err(_)) | Err(_) => {
                permission::deny_control_response(&can.request_id, &can.tool_use_id)
            }
        };
        // ExitPlanMode mode effect (Node `acp-agent.js:7665-7677`): an allow with
        // a session `setMode` update publishes current_mode_update +
        // config_option_update to the client (no CLI control request). Routed
        // through the actor so session mode/config state is updated (INV-27).
        if ctx.tool_name == "ExitPlanMode"
            && response["response"]["response"]["behavior"] == "allow"
        {
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
                    let _ = command_tx.send(Command::ExitPlanModeApplied {
                        mode: mode.to_string(),
                    });
                }
            }
        }
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
                    // Phase 9 flush barrier: enqueued after every update this
                    // turn emitted, so the forwarding task writes them all
                    // before it resolves `flush_rx`; the prompt task awaits it
                    // before responding, keeping a `tool_call` ahead of the
                    // `result` on the wire.
                    let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                    let _ = reply.send(Ok(PromptReply {
                        stop_reason: stop_reason.as_str().to_string(),
                        usage,
                        quota,
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
                    let err = SessionError::PromptFailed {
                        kind: *kind,
                        message: message.clone(),
                        assistant_had_error,
                    };
                    if *kind == FailureKind::IncompleteToolCall {
                        // E-12: the closing `tool_call_update`s (and the turn's
                        // `tool_call`) must reach the wire BEFORE the error
                        // response, so reject only after the flush barrier.
                        let (flush_tx, flush_rx) = oneshot::channel();
                        let _ = update_tx.send(SessionOutbound::Flush(flush_tx));
                        tokio::spawn(async move {
                            let _ = flush_rx.await;
                            let _ = reply.send(Err(err));
                        });
                    } else {
                        let _ = reply.send(Err(err));
                    }
                }
            }
            TurnEvent::Activated { .. } => {
                high_water.fetch_max(1, Ordering::Relaxed);
            }
        }
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
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
    update_tx: &mpsc::UnboundedSender<SessionOutbound>,
    session_id: &str,
    high_water: &AtomicUsize,
    assistant_had_error: bool,
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
    handle_events(
        &events,
        pending,
        update_tx,
        session_id,
        high_water,
        assistant_had_error,
    );
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

    // Review PC-1 (2): a subagent's permission-surfaced Task* resolution carries parentToolUseId
    // (acp-agent.ts:13233-13238).
    #[test]
    fn task_permission_resolution_carries_parent_tool_use_id() {
        let sub = task_permission_resolution(
            "toolu_TK3",
            "TaskCreate",
            false,
            &json!("ok"),
            Some("toolu_PARENT"),
        );
        assert_eq!(
            sub["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_PARENT"
        );
        let root = task_permission_resolution("toolu_TK4", "TaskCreate", false, &json!("ok"), None);
        assert!(root["_meta"]["claudeCode"].get("parentToolUseId").is_none());
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

    // Review PC-1 (5): an unlisted id's context window is inferred from the id itself
    // (acp-agent.ts:13729-13747).
    #[test]
    fn context_window_seed_reads_the_unlisted_id() {
        assert_eq!(
            context_window_seed(None, "claude-opus-4-7[1m]"),
            Some(1_000_000)
        );
        assert_eq!(context_window_seed(None, "claude-opus-4-7"), None);
    }

    // Review PC-1 (3): on an external switch the mode update and warning come before the
    // config_option_update (Node publishFallbackState runs inside applyConfigOptionValue).
    #[test]
    fn external_switch_updates_publish_config_last() {
        let opts: Vec<crate::session_config::ConfigOption> = Vec::new();
        let updates = external_switch_updates("s", &opts, Some("acceptEdits"), true);
        let kinds: Vec<&str> = updates
            .iter()
            .filter_map(|u| u["update"]["sessionUpdate"].as_str())
            .collect();
        assert_eq!(kinds.first(), Some(&"current_mode_update"));
        assert_eq!(kinds.last(), Some(&"config_option_update"));
    }

    // PC-1.5 (acp-agent.ts:13226-13245): a Task* call a permission prompt surfaced is resolved
    // by a tool_call_update, since the Task branches never emit one.
    #[test]
    fn task_permission_resolution_reports_status_and_tool_name() {
        let ok = task_permission_resolution("toolu_TK1", "TaskCreate", false, &json!("done"), None);
        assert_eq!(ok["toolCallId"], "toolu_TK1");
        assert_eq!(ok["status"], "completed");
        assert_eq!(ok["rawOutput"], "done");
        assert_eq!(ok["_meta"]["claudeCode"]["toolName"], "TaskCreate");
        let failed =
            task_permission_resolution("toolu_TK2", "TaskUpdate", true, &json!("boom"), None);
        assert_eq!(failed["status"], "failed");
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
        pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
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
            let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
            handle_events(&events, &mut pending, update_tx, "s1", hw, false);
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        handle_events(&events, &mut pending, &update_tx, "s1", &hw, false);
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
            let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
                    reply,
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
            std::env::temp_dir(),
            false,
            crate::env_snapshot::EnvSnapshot::empty(),
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
            std::env::temp_dir(),
            false,
            crate::env_snapshot::EnvSnapshot::empty(),
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
            std::env::temp_dir(),
            false,
            crate::env_snapshot::EnvSnapshot::empty(),
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
            reply: r_a,
        })
        .unwrap();
        let (r_b, _) = oneshot::channel();
        tx.send(Command::Prompt {
            uuid: "B".into(),
            frame: json!({"type":"user","uuid":"B","message":{}}),
            is_local_only: false,
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
            std::env::temp_dir(),
            false,
            crate::env_snapshot::EnvSnapshot::empty(),
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
        let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
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
}
