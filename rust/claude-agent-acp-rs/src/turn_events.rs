//! A prompt turn and the foreground work of a session, as the adapter
//! understands them, independent of the ACP version (Q-80, B-57, B-59, F39).
//!
//! Upstream: `src/turn-events.ts:1-100`. ACP v1 answers `session/prompt` when
//! the turn ends or fails, and ACP v2 when the prompt is inserted; v2 also
//! reports the session's work as state. The agent reports both through the
//! [`TurnEvents`] and [`ForegroundEvents`] traits, which it calls synchronously
//! while it updates its own state.
//!
//! The bookkeeping itself is pure: [`ForegroundTracker`] is the upstream
//! `foregroundWork` map entry (owner, `awaitingUser`, dedupe), and
//! [`ForegroundInputs`] is the `openUserInputs` set of the session.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::turn::{FailureKind, StopReason, TurnEvent};

/// The constant owner of a model cycle that Claude Code started on its own
/// (`AUTONOMOUS_CYCLE`, `acp-agent.ts:1526`).
pub const AUTONOMOUS_CYCLE: &str = "autonomous-cycle";

/// The tokens that the main agent loop of a turn used (`TurnUsage`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnUsage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thought_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub cached_write_tokens: Option<u64>,
}

impl TurnUsage {
    /// The wire shape of the usage in an `idle` state update: the key order of
    /// `sessionUsage` (`acp-agent.ts:11855-11862`), which puts `totalTokens`
    /// last.
    pub fn to_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        out.insert("inputTokens".into(), json!(self.input_tokens));
        out.insert("outputTokens".into(), json!(self.output_tokens));
        if let Some(thought) = self.thought_tokens {
            out.insert("thoughtTokens".into(), json!(thought));
        }
        if let Some(read) = self.cached_read_tokens {
            out.insert("cachedReadTokens".into(), json!(read));
        }
        if let Some(write) = self.cached_write_tokens {
            out.insert("cachedWriteTokens".into(), json!(write));
        }
        out.insert("totalTokens".into(), json!(self.total_tokens));
        Value::Object(out)
    }
}

/// How a turn ended (`TurnOutcome`): its stop reason, the usage of its main
/// agent loop, and extension data such as the quota breakdown.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub stop_reason: StopReason,
    pub usage: Option<TurnUsage>,
    pub meta: Option<Value>,
}

/// A JSON-RPC error, as v1 answers a failed prompt with it (`jsonRpcError`).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// The wire shape of the error in an `idle` state update.
    pub fn to_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        out.insert("code".into(), json!(self.code));
        out.insert("message".into(), Value::String(self.message.clone()));
        if let Some(data) = &self.data {
            out.insert("data".into(), data.clone());
        }
        Value::Object(out)
    }
}

/// The agent reports how each prompt is answered through this
/// (`TurnEvents`, `turn-events.ts:13-38`).
pub trait TurnEvents {
    /// Claude Code took the prompt in as the user message `message_id`.
    /// Reported at most once per turn, and never after [`Self::ended`] or
    /// [`Self::failed`].
    fn inserted(&self, message_id: &str);
    /// The turn ended.
    fn ended(&self, outcome: &TurnOutcome);
    /// The turn failed with `error`, the JSON-RPC error v1 answers the prompt
    /// with.
    fn failed(&self, error: &JsonRpcError);
}

/// The foreground work of a session (`ForegroundEvents`, `turn-events.ts:56-76`).
pub trait ForegroundEvents {
    /// The work started, or works again after it waited for the user.
    fn running(&self);
    /// The work waits for the user: a permission request or a question is open.
    fn awaiting_user(&self);
    /// The work ended.
    fn ended(&self, outcome: &TurnOutcome);
    /// The work failed with the JSON-RPC error v1 answers the prompt with.
    fn failed(&self, error: &JsonRpcError);
}

/// The events of a connection that reports no foreground work (v1):
/// `UNREPORTED_FOREGROUND` (`acp-agent.ts:1532-1537`). The agent still tracks
/// the work, which decides what the window after a cancel drops.
pub struct UnreportedForeground;

impl ForegroundEvents for UnreportedForeground {
    fn running(&self) {}
    fn awaiting_user(&self) {}
    fn ended(&self, _outcome: &TurnOutcome) {}
    fn failed(&self, _error: &JsonRpcError) {}
}

/// What owns the session's foreground work: a prompt's turn, or a model cycle
/// that Claude Code started on its own (`ForegroundOwner`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForegroundOwner {
    Turn(String),
    AutonomousCycle,
}

/// The foreground state of a session as v2 names it (`foregroundState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundState {
    Idle,
    Running,
    RequiresAction,
}

/// One foreground event, in the order the tracker produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum ForegroundEvent {
    Running,
    AwaitingUser,
    Ended(TurnOutcome),
    Failed(JsonRpcError),
}

/// How the foreground work ended (`endForeground`'s `end`).
#[derive(Debug, Clone, PartialEq)]
pub enum ForegroundEnd {
    Ended(TurnOutcome),
    Failed(JsonRpcError),
}

/// The session's foreground work (`foregroundWork` entry, `acp-agent.ts:2745`):
/// its owner, whether it awaits the user, and the dedupe of the state reports.
#[derive(Debug, Default)]
pub struct ForegroundTracker {
    owner: Option<ForegroundOwner>,
    awaiting_user: bool,
}

impl ForegroundTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The owner of the work, if any.
    pub fn owner(&self) -> Option<&ForegroundOwner> {
        self.owner.as_ref()
    }

    /// Whether the work awaits the user.
    pub fn awaiting_user(&self) -> bool {
        self.awaiting_user
    }

    /// The state as v2 names it (`foregroundState`): `idle` while no work
    /// runs, `requires_action` while it awaits the user, `running` otherwise.
    pub fn state(&self) -> ForegroundState {
        if self.owner.is_none() {
            ForegroundState::Idle
        } else if self.awaiting_user {
            ForegroundState::RequiresAction
        } else {
            ForegroundState::Running
        }
    }

    /// Make `owner` the foreground work (`startForeground`): it reports
    /// `running`, then waits at once if a blocking request is open. If work
    /// already runs, it goes on under `owner` and reports nothing new.
    pub fn start(
        &mut self,
        owner: ForegroundOwner,
        open_request_blocks: bool,
    ) -> Vec<ForegroundEvent> {
        if self.owner.is_some() {
            self.owner = Some(owner);
            return Vec::new();
        }
        self.owner = Some(owner);
        self.awaiting_user = false;
        let mut out = vec![ForegroundEvent::Running];
        if open_request_blocks {
            self.awaiting_user = true;
            out.push(ForegroundEvent::AwaitingUser);
        }
        out
    }

    /// End the work with `end`, if `owner` is that work (`endForeground`).
    pub fn end(&mut self, owner: &ForegroundOwner, end: ForegroundEnd) -> Vec<ForegroundEvent> {
        if self.owner.as_ref() != Some(owner) {
            return Vec::new();
        }
        self.owner = None;
        self.awaiting_user = false;
        vec![match end {
            ForegroundEnd::Ended(outcome) => ForegroundEvent::Ended(outcome),
            ForegroundEnd::Failed(error) => ForegroundEvent::Failed(error),
        }]
    }

    /// Report whether the work awaits the user (`syncAwaitingUser`): exactly
    /// while a request that blocks it is open. When the last request ends, the
    /// work reports `running` again unless a cancel withdrew that request.
    pub fn sync_awaiting_user(
        &mut self,
        waiting: bool,
        withdrawn_by_cancel: bool,
    ) -> Vec<ForegroundEvent> {
        if self.owner.is_none() || waiting == self.awaiting_user {
            return Vec::new();
        }
        self.awaiting_user = waiting;
        if waiting {
            vec![ForegroundEvent::AwaitingUser]
        } else if !withdrawn_by_cancel {
            vec![ForegroundEvent::Running]
        } else {
            Vec::new()
        }
    }
}

/// A permission or elicitation request that awaits user input
/// (`OpenUserInput`, `acp-agent.ts:953-960`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenUserInput {
    /// The subagent that asked: absent for the main conversation.
    pub agent_id: Option<String>,
    /// Set by a cancel that comes while the request is open.
    pub cancelled: bool,
}

/// The session's open user-input requests (`Session.openUserInputs`).
#[derive(Debug, Default)]
pub struct ForegroundInputs {
    open: Vec<(String, OpenUserInput)>,
}

impl ForegroundInputs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any request is open.
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// The number of open requests.
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Mark the request `request_id` open for `agent_id` (`agentID` absent for
    /// the main conversation).
    pub fn open(&mut self, request_id: impl Into<String>, agent_id: Option<String>) {
        let request_id = request_id.into();
        if self.open.iter().any(|(id, _)| id == &request_id) {
            return;
        }
        self.open.push((
            request_id,
            OpenUserInput {
                agent_id,
                cancelled: false,
            },
        ));
    }

    /// End the request `request_id`. Returns `Some(withdrawn_by_cancel)`, or
    /// `None` when it was not open.
    pub fn close(&mut self, request_id: &str) -> Option<bool> {
        let idx = self.open.iter().position(|(id, _)| id == request_id)?;
        let (_, input) = self.open.remove(idx);
        Some(input.cancelled)
    }

    /// Drop every open request (a fresh context withdrew them all).
    pub fn clear(&mut self) {
        self.open.clear();
    }

    /// Mark every open request as withdrawn by a cancel (`for (const input of
    /// session.openUserInputs ?? []) input.cancelled = true`).
    pub fn mark_cancelled(&mut self) {
        for (_, input) in &mut self.open {
            input.cancelled = true;
        }
    }

    /// Whether any open request blocks the foreground work, judged by
    /// `blocks` (`blocksForeground`).
    pub fn any_blocks(&self, blocks: impl Fn(Option<&str>) -> bool) -> bool {
        self.open
            .iter()
            .any(|(_, input)| blocks(input.agent_id.as_deref()))
    }
}

/// Why a v2 prompt was answered with an error instead of its message id
/// (`v2/prompt.ts:43-56`).
#[derive(Debug, Clone, PartialEq)]
pub enum V2IntakeError {
    /// The turn ended `cancelled` before Claude Code took the prompt in
    /// (`RequestError.requestCancelled()`, -32800).
    Cancelled,
    /// The turn ended before it was taken in.
    Ended(StopReason),
    /// The turn failed with the JSON-RPC error v1 answers the prompt with.
    Failed(JsonRpcError),
}

/// The oneshot that answers a v2 prompt at intake.
pub type V2InsertedSender = oneshot::Sender<Result<String, V2IntakeError>>;

/// One v2 message the session owes its client, in order (`v2/prompt.ts`).
#[derive(Debug)]
pub enum V2Emission {
    /// The prompt was taken in: its `user_message` (content is the v2 prompt
    /// array), which the client sees before the intake answer.
    Inserted { message_id: String, content: Value },
    /// A foreground state report (`v2Foreground`).
    Foreground(ForegroundEvent),
    /// The `idle` of a turn that ended, tagged with its prompt: a cancel that
    /// lands during the bounded file-change report preview settles the turn
    /// `cancelled` (`acp-agent.ts:8627-8685`), so the task that runs the report
    /// can rewrite this turn's idle before it reaches the client.
    Ended {
        prompt_uuid: String,
        outcome: TurnOutcome,
    },
    /// The intake answer of a prompt: its message id, or why it ended before
    /// Claude Code took it in. Sent after every update ahead of it.
    Answer {
        /// The prompt whose intake this answers (its message id).
        prompt_uuid: String,
        reply: Option<V2InsertedSender>,
        result: Result<String, V2IntakeError>,
    },
}

/// How a v2 prompt's turn ended, reported to the task that runs the turn's v1
/// bookkeeping (NEW-3: the session-index row, the `auth_required` respawn).
#[derive(Debug, Clone, PartialEq)]
pub enum V2TurnEnd {
    /// The turn settled with this stop reason.
    Settled(StopReason),
    /// The turn failed; `kind` decides the sign-out respawn and `error` is the
    /// JSON-RPC error v1 answers the prompt with.
    Failed {
        kind: FailureKind,
        error: JsonRpcError,
    },
}

/// A v2 prompt in the session: its content (the v2 `prompt` array, which the
/// `user_message` echoes) and the intake reply, until Claude Code takes it in.
#[derive(Debug)]
pub struct V2Turn {
    pub content: Value,
    pub reply: Option<V2InsertedSender>,
    pub inserted: bool,
    /// The turn's end, awaited by the task that runs its v1 bookkeeping.
    pub end: Option<oneshot::Sender<V2TurnEnd>>,
    /// The `jetbrains.air.sessionFailure` record the turn's AIR failure
    /// published: its `idle` carries it (`sessionFailureMeta`, F5).
    pub failure_meta: Option<Value>,
}

/// The v2 state of one session (F39): the foreground work, its open requests,
/// the prompt intake replies, and the messages owed to the client.
///
/// The session actor feeds it from the turn events it processes, after the
/// unfinished-tool gate, so the states it reports match what the client was
/// told of the turn's tool calls. `tasks` mirrors the background placement that
/// decides whether a request blocks the foreground work (`blocksForeground`).
#[derive(Debug, Default)]
pub struct V2State {
    reporting: bool,
    turns: HashMap<String, V2Turn>,
    foreground: ForegroundTracker,
    inputs: ForegroundInputs,
    /// `task_id -> (backgrounded, parent_task_id)`.
    tasks: HashMap<String, (bool, Option<String>)>,
    cycle_end: Option<ForegroundEnd>,
    /// The error of each failed turn, built once by the session (NEW-5).
    failure_rpcs: HashMap<String, (Option<JsonRpcError>, FailureKind)>,
    out: Vec<V2Emission>,
}

impl V2State {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this session reports v2 state (an ACP v2 connection).
    pub fn reporting(&self) -> bool {
        self.reporting
    }

    /// Mark the session as serving ACP v2 (`foregroundEvents` present upstream).
    pub fn set_reporting(&mut self, on: bool) {
        self.reporting = on;
    }

    /// Register a v2 prompt before it is enqueued.
    pub fn register(&mut self, uuid: impl Into<String>, content: Value, reply: V2InsertedSender) {
        self.turns.insert(
            uuid.into(),
            V2Turn {
                content,
                reply: Some(reply),
                inserted: false,
                end: None,
                failure_meta: None,
            },
        );
    }

    /// Give the v2 prompt `uuid` the oneshot that reports its turn's end to the
    /// task running the v1 bookkeeping (NEW-3).
    pub fn set_turn_end(&mut self, uuid: &str, end: oneshot::Sender<V2TurnEnd>) {
        if let Some(turn) = self.turns.get_mut(uuid) {
            turn.end = Some(end);
        }
    }

    /// Give the v2 turn `uuid` the `sessionFailureMeta` record of its AIR
    /// failure: the `idle` it settles `end_turn` with carries it in `_meta`
    /// (`failActiveWithSessionFailure`, `acp-agent.ts:5591-5593`).
    pub fn set_failure_meta(&mut self, uuid: &str, meta: Value) {
        if let Some(turn) = self.turns.get_mut(uuid) {
            turn.failure_meta = Some(meta);
        }
    }

    /// The JSON-RPC error a failed turn reports on its `idle` (NEW-5): the one
    /// the session built from v1's own mapping, else the shared mapping of the
    /// event (a session that fed the event without the agent's failure book).
    /// Record the error a failed turn's v2 `idle` reports, with the EFFECTIVE
    /// failure kind: the session derives auth from the assistant error too, so a
    /// `ProviderError` whose assistant frame said `authentication_failed` is
    /// `AuthRequired` for the sign-out respawn (`review-T6G2` F1). `None` means
    /// the turn settles `end_turn` instead (AIR `sessionFailure`, F5).
    pub fn set_failure_rpc(
        &mut self,
        prompt_uuid: &str,
        error: Option<JsonRpcError>,
        kind: FailureKind,
    ) {
        self.failure_rpcs
            .insert(prompt_uuid.to_string(), (error, kind));
    }

    fn take_failure_rpc(
        &mut self,
        prompt_uuid: &str,
        kind: FailureKind,
        message: &str,
        assistant_had_error: bool,
    ) -> (Option<JsonRpcError>, FailureKind) {
        self.failure_rpcs.remove(prompt_uuid).unwrap_or_else(|| {
            (
                Some(crate::agent::session_error_jsonrpc(
                    &crate::session::SessionError::PromptFailed {
                        kind,
                        message: message.to_string(),
                        assistant_had_error,
                    },
                )),
                kind,
            )
        })
    }

    /// Record what `task_started` said about a task's placement
    /// (`liveBackgroundTasks`).
    pub fn note_task(&mut self, task_id: &str, backgrounded: bool, parent_task_id: Option<&str>) {
        if !self.reporting {
            return;
        }
        self.tasks.insert(
            task_id.to_string(),
            (backgrounded, parent_task_id.map(str::to_string)),
        );
        self.sync_awaiting_user(false);
    }

    /// A task moved to the background (or back): its requests may stop blocking
    /// the foreground work.
    pub fn set_task_backgrounded(&mut self, task_id: &str, backgrounded: bool) {
        if !self.reporting {
            return;
        }
        if let Some((flag, _)) = self.tasks.get_mut(task_id) {
            *flag = backgrounded;
        }
        self.sync_awaiting_user(false);
    }

    /// A task ended: a request of it no longer has a record, so it does not
    /// block the foreground work (`blocksForeground`).
    pub fn remove_task(&mut self, task_id: &str) {
        if !self.reporting {
            return;
        }
        self.tasks.remove(task_id);
        self.sync_awaiting_user(false);
    }

    /// Whether a request of `agent_id` (absent for the main conversation)
    /// blocks the foreground work (`blocksForeground`).
    pub fn blocks_foreground(&self, agent_id: Option<&str>) -> bool {
        let mut seen = HashSet::new();
        let mut id = agent_id.map(str::to_string);
        while let Some(current) = id {
            // A parent chain that cycles still counts as blocking: upstream
            // leaves the loop on a seen id and reports `true`
            // (`acp-agent.ts:11973-11982`).
            if !seen.insert(current.clone()) {
                break;
            }
            let Some((backgrounded, parent)) = self.tasks.get(&current) else {
                return false;
            };
            if *backgrounded {
                return false;
            }
            id = parent.clone();
        }
        true
    }

    /// The foreground state as v2 names it.
    pub fn foreground_state(&self) -> ForegroundState {
        self.foreground.state()
    }

    /// A request opened: the work may now wait for the user.
    pub fn open_input(&mut self, request_id: impl Into<String>, agent_id: Option<String>) {
        if !self.reporting {
            return;
        }
        self.inputs.open(request_id, agent_id);
        self.sync_awaiting_user(false);
    }

    /// A request ended. `aborted_by_cli` says that Claude Code withdrew it
    /// (the request's signal aborted), which is not an answer: the work reports
    /// `running` again only when the request was answered by someone else while
    /// a cancel had marked it (`withdrawnByCancel = signal.aborted &&
    /// input.cancelled`, `acp-agent.ts:9595-9603`).
    pub fn close_input(&mut self, request_id: &str, aborted_by_cli: bool) {
        if !self.reporting {
            return;
        }
        let Some(cancelled) = self.inputs.close(request_id) else {
            return;
        };
        self.sync_awaiting_user(cancelled && aborted_by_cli);
    }

    /// The old context's open forms were withdrawn with its control channel and
    /// its task placements are gone: a fresh context has neither
    /// (`continuePlanInFreshContext`, `acp-agent.ts:4530-4560`).
    ///
    /// The withdrawn forms are not a cancel: the CLI aborts them with the old
    /// query, without a cancel signal, so `withdrawnByCancel = signal.aborted &&
    /// input.cancelled` is false and the work reports `running` for the
    /// continuation (NEW-4, `acp-agent.ts:9600-9604`).
    pub fn reset_for_fresh_context(&mut self) {
        if !self.reporting {
            return;
        }
        self.inputs.clear();
        self.tasks.clear();
        self.sync_awaiting_user(false);
    }

    /// A cancel marks every open request as withdrawn (not answered).
    pub fn cancel_inputs(&mut self) {
        if !self.reporting {
            return;
        }
        self.inputs.mark_cancelled();
    }

    /// A turn's result arrived while subagents it spawned are live: v1 holds
    /// its answer (`deferredSettle`), v2 ends the work at the result
    /// (`settleOrDefer`, `acp-agent.ts:5203-5213`).
    pub fn note_held(&mut self, prompt_uuid: &str, end: ForegroundEnd) {
        if !self.reporting {
            return;
        }
        let events = self
            .foreground
            .end(&ForegroundOwner::Turn(prompt_uuid.to_string()), end);
        self.push_foreground(events);
    }

    /// A held turn resumed: a steer of it (`acp-agent.ts:4380-4386`) or the
    /// plan restart (`acp-agent.ts:7119-7130`) makes its continuation the
    /// work again.
    pub fn note_resumed(&mut self, prompt_uuid: &str) {
        if !self.reporting {
            return;
        }
        let waiting = self
            .inputs
            .any_blocks(|agent| self.blocks_foreground(agent));
        let events = self
            .foreground
            .start(ForegroundOwner::Turn(prompt_uuid.to_string()), waiting);
        self.push_foreground(events);
    }

    /// Report the work waiting on the open requests.
    fn sync_awaiting_user(&mut self, withdrawn_by_cancel: bool) {
        if !self.reporting {
            return;
        }
        let waiting = self
            .inputs
            .any_blocks(|agent| self.blocks_foreground(agent));
        let events = self
            .foreground
            .sync_awaiting_user(waiting, withdrawn_by_cancel);
        self.push_foreground(events);
    }

    /// The first main-conversation output of a cycle Claude Code started on its
    /// own (`startAutonomousCycle`): it becomes the work when nothing runs.
    pub fn start_cycle(&mut self) {
        if !self.reporting || self.foreground.owner().is_some() {
            return;
        }
        let waiting = self
            .inputs
            .any_blocks(|agent| self.blocks_foreground(agent));
        let events = self
            .foreground
            .start(ForegroundOwner::AutonomousCycle, waiting);
        self.push_foreground(events);
    }

    /// Record how the cycle ended, for its [`TurnEvent::CycleEnded`].
    pub fn set_cycle_end(&mut self, end: ForegroundEnd) {
        if !self.reporting {
            return;
        }
        self.cycle_end = Some(end);
    }

    /// Claude Code took a prompt in (`reportInserted`): the `user_message` and
    /// the intake answer, then the `running` state.
    pub fn note_inserted(&mut self, uuid: &str) {
        if !self.reporting {
            return;
        }
        let Some(turn) = self.turns.get_mut(uuid) else {
            return;
        };
        if turn.inserted {
            return;
        }
        turn.inserted = true;
        let content = turn.content.clone();
        let reply = turn.reply.take();
        self.out.push(V2Emission::Inserted {
            message_id: uuid.to_string(),
            content,
        });
        let waiting = self
            .inputs
            .any_blocks(|agent| self.blocks_foreground(agent));
        let events = self
            .foreground
            .start(ForegroundOwner::Turn(uuid.to_string()), waiting);
        self.push_foreground(events);
        // The answer follows the `user_message` and the `running` state it
        // started: `inserted()` sends the message, then the work reports.
        self.out.push(V2Emission::Answer {
            prompt_uuid: uuid.to_string(),
            reply,
            result: Ok(uuid.to_string()),
        });
    }

    /// Process the gated turn events in order. `assistant_had_error` is the
    /// turn's `lastAssistantError` (the assistant frame carried an `error`),
    /// which decides whether a failure's wire error carries `data.errorKind`.
    pub fn after_events(&mut self, events: &[TurnEvent], assistant_had_error: bool) {
        if !self.reporting {
            return;
        }
        for event in events {
            match event {
                TurnEvent::Activated { prompt_uuid } => self.note_inserted(prompt_uuid),
                TurnEvent::Settled {
                    prompt_uuid,
                    stop_reason,
                    usage,
                    had_usage,
                    model_rows,
                } => {
                    let Some(mut turn) = self.turns.remove(prompt_uuid) else {
                        continue;
                    };
                    if let Some(end) = turn.end.take() {
                        let _ = end.send(V2TurnEnd::Settled(*stop_reason));
                    }
                    if turn.inserted {
                        let outcome = settle_outcome(*stop_reason, *usage, *had_usage, model_rows);
                        let events = self.foreground.end(
                            &ForegroundOwner::Turn(prompt_uuid.clone()),
                            ForegroundEnd::Ended(outcome),
                        );
                        self.push_foreground_for(prompt_uuid, events);
                    } else {
                        let error = if *stop_reason == StopReason::Cancelled {
                            V2IntakeError::Cancelled
                        } else {
                            V2IntakeError::Ended(*stop_reason)
                        };
                        self.out.push(V2Emission::Answer {
                            prompt_uuid: prompt_uuid.clone(),
                            reply: turn.reply,
                            result: Err(error),
                        });
                    }
                }
                TurnEvent::Failed {
                    prompt_uuid,
                    kind,
                    message,
                    usage,
                    model_rows,
                    ..
                } => {
                    let Some(mut turn) = self.turns.remove(prompt_uuid) else {
                        self.failure_rpcs.remove(prompt_uuid);
                        continue;
                    };
                    let (error, effective_kind) =
                        self.take_failure_rpc(prompt_uuid, *kind, message, assistant_had_error);
                    if let Some(end) = turn.end.take() {
                        match &error {
                            Some(error) => {
                                let _ = end.send(V2TurnEnd::Failed {
                                    kind: effective_kind,
                                    error: error.clone(),
                                });
                            }
                            // AIR `sessionFailure`: v1 answers `end_turn`, so the
                            // v2 turn ends like a settled turn (`review-T6G2` F5).
                            None => {
                                let _ = end.send(V2TurnEnd::Settled(StopReason::EndTurn));
                            }
                        }
                    }
                    if turn.inserted {
                        let end = match error {
                            Some(error) => ForegroundEnd::Failed(error),
                            None => {
                                // AIR `sessionFailure`: the failure's record rides
                                // the idle's `_meta` (`sessionFailureMeta`,
                                // `acp-agent.ts:5591-5593`).
                                let mut outcome =
                                    settle_outcome(StopReason::EndTurn, *usage, true, model_rows);
                                if let Some(meta) = turn.failure_meta.take() {
                                    let mut merged =
                                        outcome.meta.take().unwrap_or_else(|| json!({}));
                                    if let Some(jetbrains) = meta.get("jetbrains") {
                                        merged["jetbrains"] = jetbrains.clone();
                                    }
                                    outcome.meta = Some(merged);
                                }
                                ForegroundEnd::Ended(outcome)
                            }
                        };
                        let events = self
                            .foreground
                            .end(&ForegroundOwner::Turn(prompt_uuid.clone()), end);
                        self.push_foreground_for(prompt_uuid, events);
                    } else {
                        // A turn that never reached the client has no message
                        // id to answer with: an end_turn failure of one is an
                        // intake error either way.
                        let error = error.unwrap_or_else(|| {
                            crate::agent::session_error_jsonrpc(
                                &crate::session::SessionError::PromptFailed {
                                    kind: effective_kind,
                                    message: message.clone(),
                                    assistant_had_error,
                                },
                            )
                        });
                        self.out.push(V2Emission::Answer {
                            prompt_uuid: prompt_uuid.clone(),
                            reply: turn.reply,
                            result: Err(V2IntakeError::Failed(error)),
                        });
                    }
                }
                TurnEvent::Held { prompt_uuid, end } => self.note_held(prompt_uuid, end.clone()),
                TurnEvent::Resumed { prompt_uuid } => self.note_resumed(prompt_uuid),
                TurnEvent::CycleEnded => {
                    if let Some(end) = self.cycle_end.take() {
                        let events = self.foreground.end(&ForegroundOwner::AutonomousCycle, end);
                        self.push_foreground(events);
                    }
                }
                _ => {}
            }
        }
        // A cycle can end without its own `CycleEnded` event (the stream died,
        // or every turn failed): its recorded end still goes out.
        if let Some(end) = self.cycle_end.take() {
            let events = self.foreground.end(&ForegroundOwner::AutonomousCycle, end);
            self.push_foreground(events);
        }
    }

    fn push_foreground(&mut self, events: Vec<ForegroundEvent>) {
        for event in events {
            self.out.push(V2Emission::Foreground(event));
        }
    }

    /// Push the foreground reports of `prompt_uuid`'s turn. Its `idle` keeps
    /// the prompt id so a cancel during the report preview can rewrite it.
    fn push_foreground_for(&mut self, prompt_uuid: &str, events: Vec<ForegroundEvent>) {
        for event in events {
            match event {
                ForegroundEvent::Ended(outcome) => self.out.push(V2Emission::Ended {
                    prompt_uuid: prompt_uuid.to_string(),
                    outcome,
                }),
                other => self.out.push(V2Emission::Foreground(other)),
            }
        }
    }

    /// The v2 messages owed to the client, in order.
    pub fn take_out(&mut self) -> Vec<V2Emission> {
        std::mem::take(&mut self.out)
    }
}

/// The outcome of a settled turn as v2 reports it (`turnOutcome`): its stop
/// reason, the usage snapshot, and the `_meta.quota` breakdown when it had
/// usage.
pub fn settle_outcome(
    stop_reason: StopReason,
    usage: crate::turn::Usage,
    had_usage: bool,
    model_rows: &crate::quota::SharedRows,
) -> TurnOutcome {
    let total = usage.input_tokens
        + usage.output_tokens
        + usage.cached_read_tokens
        + usage.cached_write_tokens;
    let (usage, meta) = if had_usage {
        (
            Some(TurnUsage {
                total_tokens: total,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                thought_tokens: None,
                cached_read_tokens: Some(usage.cached_read_tokens),
                cached_write_tokens: Some(usage.cached_write_tokens),
            }),
            Some(json!({
                "quota": {
                    "token_count": {
                        "totalTokens": total,
                        "inputTokens": usage.input_tokens,
                        "cachedInputTokens": usage.cached_read_tokens,
                        "cachedWriteTokens": usage.cached_write_tokens,
                        "outputTokens": usage.output_tokens,
                        "reasoningOutputTokens": 0,
                    },
                    "model_usage": model_rows.to_json(),
                }
            })),
        )
    } else {
        (None, None)
    };
    TurnOutcome {
        stop_reason,
        usage,
        meta,
    }
}

/// The message update that a replayed chunk belongs to (`MESSAGE_OF_CHUNK`,
/// `v2/agent.ts:383-387`).
pub fn message_of_chunk(session_update: &str) -> Option<&'static str> {
    match session_update {
        "user_message_chunk" => Some("user_message"),
        "agent_message_chunk" => Some("agent_message"),
        "agent_thought_chunk" => Some("agent_thought"),
        _ => None,
    }
}

/// The message with no content that clears a replayed message (`emptyMessage`,
/// `v2/agent.ts:389-402`).
pub fn empty_message(kind: &str, message_id: &str) -> Value {
    // `{ ...message, sessionUpdate }`: the message's fields come first
    // (`emptyMessage`, `v2/agent.ts:389-402`).
    json!({
        "messageId": message_id,
        "content": [],
        "sessionUpdate": kind,
    })
}

/// The messages a replay has cleared so far (`V2ClientConnection.replays`),
/// per replaying session. A v2 client may still hold a replayed message, and
/// chunks append, so before the first replayed chunk of each message the client
/// gets the message with no content, which clears it (`replayClear`,
/// `v2/agent.ts:326-336`).
#[derive(Debug, Default)]
pub struct ReplayClearer {
    cleared: HashSet<String>,
}

impl ReplayClearer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the message of `message_id` was already cleared.
    pub fn cleared(&self, kind: &str, message_id: &str) -> bool {
        self.cleared.contains(&format!("{kind} {message_id}"))
    }

    /// The clearing message for the first replayed chunk of a message, if any.
    pub fn clear(&mut self, update: &Value) -> Option<Value> {
        let kind = message_of_chunk(update.get("sessionUpdate").and_then(Value::as_str)?)?;
        let message_id = update.get("messageId").and_then(Value::as_str)?;
        if !self.cleared.insert(format!("{kind} {message_id}")) {
            return None;
        }
        Some(empty_message(kind, message_id))
    }

    /// The updates to send for `update`: the clearing message (if this is the
    /// first replayed chunk of its message), then the update.
    pub fn apply(&mut self, update: Value) -> Vec<Value> {
        match self.clear(&update) {
            Some(clear) => vec![clear, update],
            None => vec![update],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(stop: StopReason) -> TurnOutcome {
        TurnOutcome {
            stop_reason: stop,
            usage: Some(TurnUsage {
                total_tokens: 5,
                input_tokens: 3,
                output_tokens: 2,
                ..TurnUsage::default()
            }),
            meta: None,
        }
    }

    #[test]
    fn t6a_inserted_is_reported_once_before_the_end() {
        // turn-events.test.ts: "reports a turn as inserted under the uuid of its
        // user message, then ended" — at the unit level: the tracker reports the
        // insertion once, then the end.
        let mut tracker = ForegroundTracker::new();
        let events = tracker.start(ForegroundOwner::Turn("p1".into()), false);
        assert_eq!(events, vec![ForegroundEvent::Running]);
        let events = tracker.end(
            &ForegroundOwner::Turn("p1".into()),
            ForegroundEnd::Ended(outcome(StopReason::EndTurn)),
        );
        assert_eq!(
            events,
            vec![ForegroundEvent::Ended(outcome(StopReason::EndTurn))]
        );
        assert_eq!(tracker.state(), ForegroundState::Idle);
    }

    #[test]
    fn t6a_work_that_starts_while_a_request_is_open_waits_at_once() {
        // turn-events.test.ts: "reports work that starts while a request is
        // open as waiting at once".
        let mut tracker = ForegroundTracker::new();
        let events = tracker.start(ForegroundOwner::Turn("p1".into()), true);
        assert_eq!(
            events,
            vec![ForegroundEvent::Running, ForegroundEvent::AwaitingUser]
        );
        assert_eq!(tracker.state(), ForegroundState::RequiresAction);
    }

    #[test]
    fn t6a_waiting_then_resuming_after_a_request_is_answered() {
        // turn-events.test.ts: "reports waiting on a permission request, then
        // resuming".
        let mut tracker = ForegroundTracker::new();
        let mut events = tracker.start(ForegroundOwner::Turn("p1".into()), false);
        events.extend(tracker.sync_awaiting_user(true, false));
        events.extend(tracker.sync_awaiting_user(false, false));
        events.extend(tracker.end(
            &ForegroundOwner::Turn("p1".into()),
            ForegroundEnd::Ended(outcome(StopReason::EndTurn)),
        ));
        assert_eq!(
            events,
            vec![
                ForegroundEvent::Running,
                ForegroundEvent::AwaitingUser,
                ForegroundEvent::Running,
                ForegroundEvent::Ended(outcome(StopReason::EndTurn)),
            ]
        );
    }

    #[test]
    fn t6a_a_request_that_a_cancel_withdrew_reports_no_running() {
        // acp-v2.test.ts: "reports no running before idle cancelled for a
        // child's request that a cancel withdraws".
        let mut tracker = ForegroundTracker::new();
        tracker.start(ForegroundOwner::Turn("p1".into()), false);
        tracker.sync_awaiting_user(true, false);
        let events = tracker.sync_awaiting_user(false, true);
        assert!(events.is_empty(), "a withdrawn request reports no running");
        let events = tracker.end(
            &ForegroundOwner::Turn("p1".into()),
            ForegroundEnd::Ended(outcome(StopReason::Cancelled)),
        );
        assert_eq!(
            events,
            vec![ForegroundEvent::Ended(outcome(StopReason::Cancelled))]
        );
    }

    #[test]
    fn t6a_a_request_answered_after_the_cancel_reports_running() {
        // acp-v2.test.ts: "reports running when the user answers the request
        // after the cancel": the cancel marked the request, but the user's
        // answer is not the CLI's withdrawal, so the work goes on.
        let mut state = V2State::new();
        state.set_reporting(true);
        state.register("p1", json!([]), oneshot::channel().0);
        state.note_inserted("p1");
        state.open_input("r1", None);
        state.cancel_inputs();
        state.close_input("r1", false);
        let events: Vec<ForegroundEvent> = state
            .take_out()
            .into_iter()
            .filter_map(|emission| match emission {
                V2Emission::Foreground(event) => Some(event),
                V2Emission::Ended { outcome, .. } => Some(ForegroundEvent::Ended(outcome)),
                _ => None,
            })
            .collect();
        assert_eq!(
            events,
            vec![
                ForegroundEvent::Running,
                ForegroundEvent::AwaitingUser,
                ForegroundEvent::Running
            ]
        );
    }

    #[test]
    fn t6a_a_cycle_that_a_turn_takes_over_reports_nothing_new() {
        // turn-events.ts: "A prompt that Claude Code takes in while a cycle runs
        // takes the cycle over: the cycle reports nothing more."
        let mut tracker = ForegroundTracker::new();
        let events = tracker.start(ForegroundOwner::AutonomousCycle, false);
        assert_eq!(events, vec![ForegroundEvent::Running]);
        let events = tracker.start(ForegroundOwner::Turn("p1".into()), false);
        assert!(events.is_empty());
        assert_eq!(tracker.owner(), Some(&ForegroundOwner::Turn("p1".into())));
        // Only the owner's end ends the work.
        let events = tracker.end(
            &ForegroundOwner::AutonomousCycle,
            ForegroundEnd::Ended(outcome(StopReason::EndTurn)),
        );
        assert!(events.is_empty());
        let events = tracker.end(
            &ForegroundOwner::Turn("p1".into()),
            ForegroundEnd::Ended(outcome(StopReason::EndTurn)),
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn t6a_open_inputs_keep_their_withdrawn_flag() {
        let mut inputs = ForegroundInputs::new();
        inputs.open("r1", None);
        inputs.open("r2", Some("agent-1".into()));
        assert_eq!(inputs.len(), 2);
        inputs.mark_cancelled();
        assert_eq!(inputs.close("r1"), Some(true));
        assert_eq!(inputs.close("r2"), Some(true));
        assert_eq!(inputs.close("r2"), None);
        assert!(inputs.is_empty());
    }

    #[test]
    fn t6a_blocks_foreground_reads_each_open_request() {
        let mut inputs = ForegroundInputs::new();
        inputs.open("r1", None);
        inputs.open("r2", Some("background".into()));
        assert!(inputs.any_blocks(|agent| agent.is_none()));
        assert!(!inputs.any_blocks(|_| false));
    }

    #[test]
    fn t6a_replay_clears_each_message_before_its_first_chunk() {
        // acp-v2.test.ts: "replays the history before it answers session/resume,
        // clearing each message first".
        let mut clearer = ReplayClearer::new();
        let chunk = json!({"sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": {"type": "text", "text": "hi"}});
        assert_eq!(
            clearer.apply(chunk.clone()),
            vec![
                json!({"sessionUpdate": "user_message", "messageId": "u1", "content": []}),
                chunk
            ]
        );
        let second = json!({"sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": {"type": "text", "text": "!"}});
        assert_eq!(clearer.apply(second.clone()), vec![second]);
        let agent = json!({"sessionUpdate": "agent_message_chunk", "messageId": "a1",
            "content": {"type": "text", "text": "Hello"}});
        assert_eq!(
            clearer.apply(agent.clone()),
            vec![
                json!({"sessionUpdate": "agent_message", "messageId": "a1", "content": []}),
                agent
            ]
        );
        // Other kinds are passed through.
        let state = json!({"sessionUpdate": "state_update", "state": "idle"});
        assert_eq!(clearer.apply(state.clone()), vec![state]);
    }

    #[test]
    fn t6a_usage_json_omits_unknown_counters() {
        let usage = TurnUsage {
            total_tokens: 11,
            input_tokens: 7,
            output_tokens: 4,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        };
        assert_eq!(
            usage.to_json(),
            json!({"totalTokens": 11, "inputTokens": 7, "outputTokens": 4})
        );
    }
}

#[cfg(test)]
mod t9a_tests {
    use super::*;

    /// A reporting session with the turn `p1` already inserted.
    fn rig() -> V2State {
        let mut state = V2State::new();
        state.set_reporting(true);
        state.register("p1", json!([]), oneshot::channel().0);
        state.note_inserted("p1");
        // The insertion's `running` is not part of the case under test.
        let _ = emissions(&mut state);
        state
    }

    fn emissions(state: &mut V2State) -> Vec<String> {
        state
            .take_out()
            .into_iter()
            .filter_map(|emission| match emission {
                V2Emission::Foreground(ForegroundEvent::Running) => Some("running".into()),
                V2Emission::Foreground(ForegroundEvent::AwaitingUser) => {
                    Some("requires_action".into())
                }
                V2Emission::Foreground(ForegroundEvent::Ended(outcome)) => {
                    Some(format!("idle {}", outcome.stop_reason.as_str()))
                }
                V2Emission::Ended { outcome, .. } => {
                    Some(format!("idle {}", outcome.stop_reason.as_str()))
                }
                _ => None,
            })
            .collect()
    }

    /// `acp-v2.test.ts` > "reports a cycle that waits on a question as
    /// requires_action": a cycle Claude Code starts on its own waits for a
    /// foreground subagent's question.
    #[test]
    fn t9a_a_cycle_that_waits_on_a_question_reports_requires_action() {
        let mut state = V2State::new();
        state.set_reporting(true);
        state.note_task("agent-1", false, None);
        state.open_input("r1", Some("agent-1".into()));
        state.start_cycle();
        assert_eq!(emissions(&mut state), ["running", "requires_action"]);
    }

    /// `acp-v2.test.ts` > "keeps the turn running for a foreground subagent that
    /// a background subagent launched": the foreground child of a background
    /// parent does not hold the turn.
    #[test]
    fn t9a_a_foreground_subagent_under_a_background_parent_does_not_block() {
        let mut state = rig();
        state.note_task("agent-1", true, None);
        state.note_task("agent-2", false, Some("agent-1"));
        state.open_input("r1", Some("agent-2".into()));
        assert!(emissions(&mut state).is_empty());
    }

    /// `acp-v2.test.ts` > "waits for a request once its subagent, which asked
    /// before its task_started, turns out to run in the foreground".
    #[test]
    fn t9a_a_request_waits_when_its_subagent_turns_out_foreground() {
        let mut state = rig();
        state.open_input("r1", Some("agent-1".into()));
        assert!(emissions(&mut state).is_empty(), "no task record yet");
        state.note_task("agent-1", false, None);
        assert_eq!(emissions(&mut state), ["requires_action"]);
    }

    /// `acp-v2.test.ts` > "stops waiting for a request once its foreground
    /// subagent settles", and once Claude Code moves it to the background.
    #[test]
    fn t9a_a_foreground_subagent_settling_or_backgrounding_reports_running() {
        for settle in [true, false] {
            let mut state = rig();
            state.note_task("agent-1", false, None);
            state.open_input("r1", Some("agent-1".into()));
            assert_eq!(emissions(&mut state), ["requires_action"]);
            if settle {
                state.remove_task("agent-1");
            } else {
                state.set_task_backgrounded("agent-1", true);
            }
            assert_eq!(emissions(&mut state), ["running"]);
        }
    }

    /// `acp-v2.test.ts` > "reports nothing for a background subagent's request
    /// that a cancel withdraws".
    #[test]
    fn t9a_a_background_subagents_withdrawn_request_reports_nothing() {
        let mut state = rig();
        state.note_task("agent-1", true, None);
        state.open_input("r1", Some("agent-1".into()));
        state.cancel_inputs();
        state.close_input("r1", true);
        assert!(emissions(&mut state).is_empty());
    }

    /// `acp-v2.test.ts` > "reports no running for a turn's request that a cancel
    /// withdraws along with a background subagent's".
    #[test]
    fn t9a_a_turn_request_withdrawn_with_a_background_one_reports_no_running() {
        let mut state = rig();
        state.note_task("agent-1", true, None);
        state.open_input("r1", None);
        assert_eq!(emissions(&mut state), ["requires_action"]);
        state.open_input("r2", Some("agent-1".into()));
        state.cancel_inputs();
        state.close_input("r1", true);
        state.close_input("r2", true);
        assert_eq!(emissions(&mut state), Vec::<String>::new());
    }

    /// `acp-v2.test.ts` > "reports running when Claude Code withdraws %s without
    /// a cancel, since the work goes on".
    #[test]
    fn t9a_a_withdrawn_request_without_a_cancel_reports_running() {
        let mut state = rig();
        state.open_input("r1", None);
        assert_eq!(emissions(&mut state), ["requires_action"]);
        state.close_input("r1", true);
        assert_eq!(emissions(&mut state), ["running"]);
    }

    /// `acp-v2.test.ts` > "reports running when Claude Code withdraws a request
    /// that opened after the cancel".
    #[test]
    fn t9a_a_request_opened_after_the_cancel_reports_running() {
        let mut state = rig();
        state.cancel_inputs();
        state.open_input("r1", None);
        assert_eq!(emissions(&mut state), ["requires_action"]);
        state.close_input("r1", true);
        assert_eq!(emissions(&mut state), ["running"]);
    }

    /// `acp-v2.test.ts` > "two requests that are open together": the last one to
    /// end decides whether the work runs again.
    #[test]
    fn t9a_two_open_requests_let_the_last_decide() {
        // Both withdrawn: no running.
        let mut state = rig();
        state.open_input("r1", None);
        state.open_input("r2", None);
        state.cancel_inputs();
        state.close_input("r1", true);
        state.close_input("r2", true);
        assert_eq!(emissions(&mut state), ["requires_action"]);

        // The user answers the one the cancel did not withdraw, last: running.
        let mut state = rig();
        state.open_input("r1", None);
        state.open_input("r2", None);
        state.cancel_inputs();
        state.close_input("r1", true);
        state.close_input("r2", false);
        assert_eq!(emissions(&mut state), ["requires_action", "running"]);

        // The cancel withdraws the one that ends last: no running.
        let mut state = rig();
        state.open_input("r1", None);
        state.open_input("r2", None);
        state.cancel_inputs();
        state.close_input("r1", false);
        state.close_input("r2", true);
        assert_eq!(emissions(&mut state), ["requires_action"]);
    }

    /// `acp-v2.test.ts` > "reports no running before idle cancelled for a request
    /// of a cycle that Claude Code started, which a cancel withdraws".
    #[test]
    fn t9a_a_cancelled_cycle_request_reports_no_running_before_idle_cancelled() {
        let mut state = V2State::new();
        state.set_reporting(true);
        state.start_cycle();
        assert_eq!(emissions(&mut state), ["running"]);
        state.open_input("r1", None);
        assert_eq!(emissions(&mut state), ["requires_action"]);
        state.cancel_inputs();
        state.close_input("r1", true);
        assert!(emissions(&mut state).is_empty());
        state.set_cycle_end(ForegroundEnd::Ended(TurnOutcome {
            stop_reason: StopReason::Cancelled,
            usage: None,
            meta: None,
        }));
        state.after_events(&[TurnEvent::CycleEnded], false);
        assert_eq!(emissions(&mut state), ["idle cancelled"]);
    }

    /// `acp-v2.test.ts` > "does not report the next turn as waiting for a
    /// question that its client never settled": the withdrawn request does not
    /// survive into the next turn.
    #[test]
    fn t9a_a_withdrawn_question_does_not_wait_into_the_next_turn() {
        let mut state = rig();
        state.open_input("r1", None);
        assert_eq!(emissions(&mut state), ["requires_action"]);
        state.cancel_inputs();
        state.close_input("r1", true);
        state.note_held(
            "p1",
            ForegroundEnd::Ended(TurnOutcome {
                stop_reason: StopReason::Cancelled,
                usage: None,
                meta: None,
            }),
        );
        assert_eq!(emissions(&mut state), ["idle cancelled"]);
        // The next turn reports no requires_action.
        state.register("p2", json!([]), oneshot::channel().0);
        state.note_inserted("p2");
        assert_eq!(emissions(&mut state), ["running"]);
    }
}
