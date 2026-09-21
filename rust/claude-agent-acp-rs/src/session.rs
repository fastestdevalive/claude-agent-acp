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

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::control::{Control, ControlOptions, InitializeOptions};
use crate::dispatch::{self, Route, StreamMsg};
use crate::map::{self, MapState, MsgRole};
use crate::process::{self, SpawnOptions, Timings};
use crate::turn::{FailureKind, StopReason, Turn, TurnEvent, TurnMachine};

/// Errors produced by the session actor.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session closed")]
    Closed,
    #[error("session/prompt failed: {message}")]
    PromptFailed { kind: FailureKind, message: String },
    #[error("failed to start the session: {0}")]
    Start(String),
}

/// The result of a settled `session/prompt`.
#[derive(Debug, Clone)]
pub struct PromptReply {
    /// The turn's ACP `stopReason`.
    pub stop_reason: String,
    /// Token usage reported on the response (the fixture's
    /// `result.usage` shape).
    pub usage: Option<PromptUsage>,
    /// The turn's streaming updates (e.g. a `FinalText` `agent_message_chunk`)
    /// that must be forwarded to the client *before* the prompt response.
    pub updates: Vec<SessionNotification>,
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
    ) -> Result<(Session, mpsc::UnboundedReceiver<SessionNotification>), SessionError> {
        // Spawn the child and its stack: process -> control (stdin) + codec
        // (stdout lines) + stderr tail.
        let mut process = process::spawn(&opts.spawn)
            .await
            .map_err(|e| SessionError::Start(e.to_string()))?;
        let stdin = process
            .take_stdin()
            .ok_or_else(|| SessionError::Start("child stdin unavailable".into()))?;
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let control = Control::spawn(stdin, control_in_rx, ControlOptions::default());

        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionNotification>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
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

/// The session actor task. Owns all session state; nothing else mutates it.
#[allow(clippy::too_many_arguments)]
async fn run(
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut stream: mpsc::UnboundedReceiver<Value>,
    _process: crate::process::Process,
    control: Control,
    control_in_tx: mpsc::UnboundedSender<Value>,
    update_tx: mpsc::UnboundedSender<SessionNotification>,
    _timings: Timings,
    session_id: String,
    high_water: Arc<AtomicUsize>,
) {
    let mut machine = TurnMachine::new();
    let mut map_state = MapState::default();
    // uuid -> prompt reply oneshot (8.7).
    let mut pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>> =
        HashMap::new();
    let mut emitted_assistant_text = false;

    loop {
        tokio::select! {
            biased;
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    break;
                };
                match cmd {
                    Command::Prompt { uuid, frame, is_local_only, reply } => {
                        let _ = control.send_user(frame).await;
                        machine.enqueue(Turn::new(uuid.clone(), is_local_only));
                        pending.insert(uuid, reply);
                    }
                    Command::Cancel => {
                        // 8.9: settle swept queued turns, then interrupt.
                        let events = machine.cancel();
                        handle_events(&events, &mut pending, &update_tx, &session_id, &high_water);
                        // Send the `interrupt` on a spawned task: awaiting the
                        // correlated control_response here would block the actor's
                        // single-task loop (which is what routes that response back
                        // to the Control task), deadlocking it. Fire-and-forget —
                        // a late response is dropped, not held.
                        let control = control.clone();
                        tokio::spawn(async move {
                            let _ = control
                                .send_request(json!({"subtype": "interrupt"}))
                                .await;
                        });
                    }
                }
            }
            msg = stream.recv() => {
                let Some(line) = msg else {
                    // Stream EOF / codec death (8.9): settle/reject every turn.
                    let events = machine.on_stream_end();
                    handle_events(&events, &mut pending, &update_tx, &session_id, &high_water);
                    break;
                };
                dispatch_stream(
                    StreamMsg::new(line),
                    &control_in_tx,
                    &mut machine,
                    &mut map_state,
                    &mut pending,
                    &mut emitted_assistant_text,
                    &update_tx,
                    &session_id,
                    &high_water,
                )
                .await;
            }
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
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
    emitted_assistant_text: &mut bool,
    update_tx: &mpsc::UnboundedSender<SessionNotification>,
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
                pending,
                emitted_assistant_text,
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
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
    emitted_assistant_text: &mut bool,
    update_tx: &mpsc::UnboundedSender<SessionNotification>,
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
                handle_events(&events, pending, update_tx, session_id, high_water);
            }
        }
        "result" => {
            let events = machine.on_result(&line, *emitted_assistant_text);
            // Upstream clears emittedAssistantText in the result finally for
            // non-autonomous results.
            if !is_autonomous(&line) {
                *emitted_assistant_text = false;
            }
            handle_events(&events, pending, update_tx, session_id, high_water);
        }
        "system" => {
            let subtype = line.get("subtype").and_then(Value::as_str).unwrap_or("");
            match subtype {
                "task_started" => {
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    let is_subagent = line.get("subagent_type").is_some();
                    machine.on_task_started(task_id, is_subagent);
                }
                "task_notification" | "task_updated" => {
                    let task_id = line.get("task_id").and_then(Value::as_str).unwrap_or("");
                    if is_terminal_task_update(&line) {
                        let events = machine.on_task_ended(task_id);
                        handle_events(&events, pending, update_tx, session_id, high_water);
                    }
                }
                "session_state_changed" => {
                    let state = line.get("state").and_then(Value::as_str).unwrap_or("");
                    machine.on_session_state(state);
                    if state == "idle" {
                        let events = machine.on_idle();
                        handle_events(&events, pending, update_tx, session_id, high_water);
                    }
                }
                "commands_changed" => {
                    emit_commands_changed(&line, update_tx, session_id);
                }
                _ => {}
            }
        }
        "stream_event" | "assistant" => {
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

/// Process a batch of [`TurnEvent`]s in order (8.8 / review-04 wiring):
/// `FinalText` is emitted as an `agent_message_chunk` before the same batch's
/// `Settled` resolves the prompt.
fn handle_events(
    events: &[TurnEvent],
    pending: &mut HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>,
    _update_tx: &mpsc::UnboundedSender<SessionNotification>,
    session_id: &str,
    high_water: &AtomicUsize,
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
                    let _ = reply.send(Ok(PromptReply {
                        stop_reason: stop_reason.as_str().to_string(),
                        usage,
                        updates: std::mem::take(&mut final_texts),
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
    update_tx: &mpsc::UnboundedSender<SessionNotification>,
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
    let _ = update_tx.send(notif);
}

/// Send a batch of updates to the client as `session/update` notifications.
fn emit_updates(
    updates: &[SessionUpdate],
    update_tx: &mpsc::UnboundedSender<SessionNotification>,
    session_id: &str,
) {
    for update in updates {
        let notif = SessionNotification::new(session_id.to_string(), update.clone());
        let _ = update_tx.send(notif);
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
        let (_tx, rx) = mpsc::unbounded_channel::<SessionNotification>();
        let _ = rx; // update sink unused here
        let (utx, _urx) = mpsc::unbounded_channel::<SessionNotification>();
        let hw = Arc::new(AtomicUsize::new(0));

        // p1 echoed + result settles it (A's result).
        machine.enqueue(Turn::new("p1".into(), false));
        let _ = machine.on_echo("p1");
        let events = machine.on_result(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"ok","usage":{}}), false);
        handle_events(&events, &mut pending, &utx, "s", &hw);

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
        let (utx, _urx) = mpsc::unbounded_channel::<SessionNotification>();
        let hw = Arc::new(AtomicUsize::new(0));
        let mut emitted_assistant_text = false;

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
            &mut pending,
            &mut emitted_assistant_text,
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
}
