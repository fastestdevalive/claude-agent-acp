//! Turn state machine & settlement (the R7 cost centre; items 6.1–6.6, reworked
//! for review 04).
//!
//! A turn is one `session/prompt` from enqueue to settle. This module owns the
//! reverse-engineered state machine that settles a turn — the logic the Node
//! adapter spreads across `dist/acp-agent.js` `1338–1682` (activate/settle/
//! orphan), `1683–1845` (consumer loop), `2049–2163` (idle settle),
//! `2532–2884` (result → stop reason) and `3443–3612` (cancel).
//!
//! The session actor (phase 5, D4) drives a [`TurnMachine`] with the stream
//! frames it classifies as session traffic ([`crate::dispatch`]); the machine
//! returns [`TurnEvent`]s the actor turns into ACP emissions (phase 7+).
//! Nothing here is coupled to the wire: the machine is fed parsed JSON frames
//! and is fully unit-testable on its own.
//!
//! The state diagram (§ Architecture):
//!
//! ```text
//! [*] --> Queued        : session/prompt enqueues
//! Queued --> Activated  : echo of user message received, or ensure_active_turn
//! Queued --> Settled    : cancel sweep (message already pushed) [ADP:3471-3526]
//! Queued --> Failed     : stream end / fail_all (never ran) [ADP:1814-1820]
//! Activated --> Deferred   : result arrives, subagents live (#866)
//! Activated --> Settled    : result arrives, no subagents (#773)
//! Activated --> Failed     : idle without a result (#825)
//! Deferred --> Settled     : followup autonomous result, or held idle drain
//! Activated --> Settled    : stream end (deferred/scratch/cancelled)
//! Deferred --> Settled     : cancel inline-settle [ADP:3538-3575]
//! Activated --> Settled    : force-cancel floor (phase 11)
//! Settled --> [*]
//! ```
//!
//! "Echo" = the CLI re-emits the user message because argv has
//! `--replay-user-messages` (B2). "Orphan" = a turn whose consumer left but
//! whose `result` may still arrive; its result must not be consumed by the
//! next turn (INV-30). `owed_trailing_idles` absorbs the SDK's lagging
//! `session_state_changed: idle` so it is not misread as the next turn being
//! abandoned (#825 false-fail, ADP:2108-2118).
//!
//! ## Steering (`_session/steering`)
//!
//! A steered `priority:"now"` message injected mid-turn marks the active (or
//! queue-head) turn via [`TurnMachine::mark_steered`], which records the steer's
//! uuid in `steered_echoes` and moves any held `deferred_settle` into the steer
//! lane (`steered_settle`). The steer lane then owns settlement (R1-R9,
//! `acp-agent.js:129-132, 1141, 1166-1180, 1560-1564, 2081-2140, 2618-2625,
//! 2756-2764, 3059-3079, 3092-3101, 3459-3463, 3577-3586`): steered results
//! record their outcome without settling, owe no trailing idle, and the single
//! idle spanning the interrupted + steered cycles settles the turn through the
//! subagent gate. Wire parity is gated by `inv_24_steer_mid_turn` /
//! `inv_24_steer_error_result` (`tests/differential.rs`).
//!
//! ## Known unfixable hole: pre-echo abandonment (#825)
//!
//! Idle deliberately fails only the ACTIVE turn. A queued turn that was never
//! echoed is NOT failed on idle, because an idle can legitimately precede the
//! SDK picking up freshly pushed input (the idle was emitted before the SDK
//! read it) — failing the queue head on that race would reject a prompt the SDK
//! is about to run. A turn abandoned before its echo therefore still hangs
//! until cancel or the next prompt; only a timer could tell those apart
//! (`acp-agent.js:2141-2153`). Documented, not fixed.
//!
//! ## Known omissions (deliberate, deferred to their own phases — review 04 #14)
//!
//! - **`msg_lifecycle_v1` orphan map** (`session.orphanCommands`): orphan
//!   accounting uses only the coarse `pending_orphan_results` count, not the
//!   per-uuid `command_lifecycle` lane. Coalescing of N queued commands into
//!   one result can leave a stale count; the count self-heals at activation
//!   (`ADP:1366`), bounding the damage. The two-phase `endedPerLevel` registry
//!   sweep at activation (`ADP:1366-1390`) is likewise omitted: `live_subagents`
//!   entries are removed only by their terminal frames.
//!
//! Both are recorded in the plan under Decision D4 and `PARITY.md` as
//! pending, so their absence is intentional, not a silent drift.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde_json::Value;

use crate::quota::{self, ModelRows, SharedRows};

mod stopped_work;
use stopped_work::{StoppedWorkState, TaskMeta};
pub use stopped_work::{StoppedWorkVerdict, TaskPlacement};

/// Verbatim upstream message for a turn that reached `idle` with no result
/// (`acp-agent.js:54-55`).
pub const TURN_NO_RESULT_MESSAGE: &str = "The turn ended without a result: the agent went idle while this prompt was still in flight (e.g. the model stream dropped mid-turn). Any partial output may be incomplete; please retry.";

/// Verbatim upstream message for a queued turn rejected when the stream ends
/// (`acp-agent.js:282`).
pub const SESSION_ENDED_MESSAGE: &str =
    "The Claude Agent session has ended. Please start a new session.";

/// The `origin.kind` values that mark a result as AUTONOMOUS (not the user's
/// prompt) — `acp-agent.js:114-120`.
const AUTONOMOUS_RESULT_ORIGINS: [&str; 5] = [
    "task-notification",
    "peer",
    "coordinator",
    "observer",
    "observer-activity",
];

/// The ACP `stopReason` a settled turn reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Normal completion — the default.
    EndTurn,
    /// The model hit its output-token limit.
    MaxTokens,
    /// The model refused to answer.
    Refusal,
    /// Budget / turns / structured-output budget exhausted.
    MaxTurnRequests,
    /// The turn was cancelled.
    Cancelled,
}

impl StopReason {
    /// The wire spelling of this stop reason.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::MaxTurnRequests => "max_turn_requests",
            StopReason::Cancelled => "cancelled",
        }
    }
}

/// The reason a turn failed — the category a client maps to a JSON-RPC error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// `Please run /login` (upstream `RequestError.authRequired`).
    AuthRequired,
    /// An `is_error` result from the provider.
    ProviderError,
    /// `error_max_budget_usd` with `is_error`.
    BudgetExhausted,
    /// `error_max_turns` with `is_error`.
    ContextExhausted,
    /// Idle reached with no result (`#825`; `TURN_NO_RESULT_MESSAGE`).
    NoResult,
    /// The stream ended and the turn never ran (`SESSION_ENDED_MESSAGE`).
    SessionEnded,
    /// The Claude process died under the active turn and the client reads AIR
    /// session failures: the turn ends `end_turn` with `transport_lost` failure
    /// metadata instead of a rejection (`failActiveWithSessionFailure(
    /// "transport_lost")`, `acp-agent.ts:8275-8288`). Produced only by
    /// [`TurnMachine::fail_all_split`].
    TransportLost,
    /// The worker announced its shutdown (`worker_shutting_down`) and the
    /// stream then ended under the active turn: the turn ends with a
    /// `worker_shutdown` failure (`failActiveWithSessionFailure(
    /// "worker_shutdown")`, `acp-agent.ts:5750-5760`). Produced only by
    /// [`TurnMachine::fail_active_worker_shutdown`].
    WorkerShutdown,
    /// An `end_turn` settle left a dispatched tool call without a result
    /// (`errorKind: incomplete_tool_call`, E-12). Produced only by the
    /// session-level post-filter (`unfinished.rs`), never by the machine.
    IncompleteToolCall,
}

impl FailureKind {
    /// A default message used by [`TurnMachine::fail_all`] when the caller
    /// supplies no per-turn detail. Result-driven failures carry the result's
    /// own text instead (see `on_result`).
    pub fn default_message(self) -> &'static str {
        match self {
            FailureKind::AuthRequired => "Authentication required. Please run /login.",
            FailureKind::ProviderError => "The model provider returned an error.",
            FailureKind::BudgetExhausted => "The turn exceeded its budget.",
            FailureKind::ContextExhausted => "The context window was exhausted.",
            FailureKind::NoResult => TURN_NO_RESULT_MESSAGE,
            FailureKind::SessionEnded => SESSION_ENDED_MESSAGE,
            FailureKind::TransportLost => "The connection to Claude was lost.",
            FailureKind::WorkerShutdown => "The Claude worker is shutting down.",
            FailureKind::IncompleteToolCall => {
                "Claude ended the turn without returning results for tool calls"
            }
        }
    }
}

/// Token usage attributed to a settled turn — the per-turn `accumulatedUsage`
/// accumulator snapshot reported in every `PromptResponse` (review 04 #14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_read_tokens: u64,
    pub cached_write_tokens: u64,
}

/// The outcome and usage snapshot stored when a turn's settlement is deferred
/// (`Turn.deferredSettle` upstream — `{ stopReason, usage: accumulatedUsage }`,
/// `ADP:1788-1805`). A held turn's result has already recorded its stop reason
/// and the usage accumulated up to that result; the hold settles with this
/// snapshot, not the live accumulator (`ADP:3059`, `ADP:3574`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeferredSettle {
    stop_reason: StopReason,
    usage: Usage,
    /// The per-model rows snapshot taken with `usage` (`_meta.quota.model_usage`).
    models: SharedRows,
}

/// What the interruption result of an ExitPlanMode answer ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitPlanVerdict {
    /// "No, keep planning": the turn ends `cancelled`.
    KeepPlanning,
    /// An accepted plan that continues in a fresh context: the turn leaves the
    /// machine, carrying the usage it has earned (this result's included).
    ContextReset,
}

/// Events the turn machine emits for the session actor to act on. These are
/// internal — ACP emission (session/update, prompt response) is phase 7+.
/// `prompt_uuid` on every settle/fail/activate tells the actor which `oneshot`
/// to resolve or reject (review 04 #11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// A turn settled. The actor resolves that prompt's oneshot with this stop
    /// reason and usage.
    ///
    /// `had_usage` distinguishes the two structurally-different upstream cancel
    /// settlements (phase 15, Item 2): a turn **swept from the queue** by
    /// `cancel()` never activated, so upstream reports NO `usage` field
    /// (`turn.resolve({ stopReason: "cancelled" })`); every other settlement —
    /// an active/held turn's cancel (`settleActive({ ..., usage:
    /// sessionUsage(session) })`) — reports `usage` even when it is genuinely
    /// all-zero. `session.rs` keys off this origin flag, never the numeric
    /// `usage` value, so a real cancel that lands before any tokens accumulate
    /// still reports an all-zero `usage` object.
    Settled {
        prompt_uuid: String,
        stop_reason: StopReason,
        usage: Usage,
        had_usage: bool,
        /// Per-model quota rows (`_meta.quota.model_usage`), snapshotted with
        /// `usage`; empty for the queued-sweep cancel.
        model_rows: SharedRows,
    },
    /// A turn failed. The actor rejects that prompt's oneshot with the mapped
    /// JSON-RPC error.
    Failed {
        prompt_uuid: String,
        kind: FailureKind,
        message: String,
        usage: Usage,
        model_rows: SharedRows,
        is_structured_output_retries: bool,
    },
    /// A turn was activated / promoted to the queue head.
    Activated { prompt_uuid: String },
    /// #453 result-text fallback: assistant text exists only in the `result`
    /// frame (no `stream_event` deltas, no consolidated `assistant` message),
    /// so the actor must forward it as its own message. Emission is phase 7.
    FinalText { text: String },
    /// The classifier's explanation of a refusal (`lastRefusalExplanation`),
    /// streamed ahead of the refusing turn's settle. Unlike [`Self::FinalText`]
    /// it goes out in order with the result's other updates (the usage update
    /// precedes it on the wire), so the actor sends it on the update channel.
    RefusalText { text: String },
    /// A model cycle that Claude Code started on its own ended (at its result, or
    /// at an idle no earlier result owes). Consumed by the unfinished-tool gate,
    /// which closes the calls the cycle left open before its end is reported
    /// (`closeCycleToolCalls`); never reaches the client.
    CycleEnded,
}

/// One `session/prompt` in the machine — queued, active or settled.
#[derive(Debug, Clone)]
pub struct Turn {
    prompt_uuid: String,
    /// Whether this is a local-only slash command (no model invocation), the
    /// `#453` result-text fallback's other trigger (`isLocalOnlyCommand`).
    is_local_only_command: bool,
    settled: bool,
    /// Set when the turn held open for its live subagents: the outcome (and
    /// usage snapshot) its `result` already recorded, settled once the
    /// subagents drain or the user moves on.
    deferred_settle: Option<DeferredSettle>,
    /// Task ids of the subagents this turn spawned (`spawnedTaskIds`).
    spawned_task_ids: HashSet<String>,
    /// The steer uuids whose echoes are still pending for this turn — `Some`
    /// when a `_session/steering` injected a `priority:"now"` message mid-turn
    /// (`Turn.steeredEchoes`). `is_steering_turn` is the R1 predicate.
    ///
    /// 2.13 (E-8): insertion-ordered so a cancel's `take_abandoned()` yields the
    /// pending steered echoes in the order they were injected (`dist/acp-agent.js:6253-6264`).
    steered_echoes: Option<Vec<String>>,
    /// The outcome (and usage snapshot) recorded by a steered turn's result,
    /// settled by the steer idle lane rather than by the result itself
    /// (`Turn.steeredSettle`). Later results overwrite it.
    steered_settle: Option<DeferredSettle>,
    /// Set when a steered echo drains and cleared when a result records
    /// `steered_settle` (`Turn.steeredAwaitingResult`): while set, the recorded
    /// outcome predates the steered work, so an idle must not settle on it.
    steered_awaiting_result: bool,
    /// Every steer this turn has taken, echoed or not (`Turn.steeredUuids`): a
    /// cancel abandons them all, and a stamped result naming one answers it.
    steered_uuids: Vec<String>,
    /// The normalised text of every informational notice shown for this turn
    /// (`Turn.noticeTexts`): a notice is not an answer, so the #453 fallback
    /// skips a result that only repeats one (a hook-blocked turn's result).
    notice_texts: Vec<String>,
    /// `command_lifecycle` `started` was seen for this turn's prompt
    /// (`Turn.commandStarted`): Claude Code dispatched it into a turn.
    command_started: bool,
    /// The prompt's terminal lifecycle frame, once seen and while the turn is
    /// still queued (`Turn.commandFinished`).
    command_finished: Option<CommandFate>,
    /// A result has passed since the command was dispatched with no terminal
    /// frame yet (`Turn.commandResultSeen`): a later cancel must not seed an
    /// orphan entry that waits for it.
    command_result_seen: bool,
    /// The usage (and per-model rows) a turn earned before a session restart
    /// carried it into a fresh machine (`Turn.carriedUsage` /
    /// `carriedModelUsage`): the next activation's scratch starts from it instead
    /// of zero, then forgets it.
    carried: Option<(Usage, ModelRows)>,
    /// The user frame this prompt sent, kept while the turn is unsettled so a
    /// clear-context restart can send it again to the fresh child when the
    /// prompt was still queued behind the plan turn.
    frame: Option<Value>,
}

/// The terminal `command_lifecycle` states (`Turn.commandFinished`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandFate {
    Completed,
    Discarded,
    Refused,
    Cancelled,
}

/// What [`TurnMachine::on_result_begin`] decided before the result's own
/// emissions: whether the result is an autonomous cycle's, and the events of the
/// held-turn hand-off / queue-head activation `ensureActiveTurn` produced.
#[derive(Debug)]
pub struct ResultBegin {
    /// The result belongs to a model cycle Claude Code started on its own.
    pub autonomous: bool,
    /// Events from `ensureActiveTurn` (a held turn's settle, `Activated`).
    pub events: Vec<TurnEvent>,
}

/// One entry of the per-uuid orphan map (`Session.orphanCommands`) on
/// `msg_lifecycle_v1` CLIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanState {
    /// Orphaned before dispatch: no result is known to be coming.
    Pending,
    /// Dispatched into a turn: its (echo-less) result is still to be skipped.
    Started,
    /// Its terminal `cancelled` frame arrived after dispatch: any late result
    /// is a dead turn's.
    Zombie,
}

/// The orphan accounting one `cancel()` seeded, for its interrupt receipt.
#[derive(Debug)]
struct CancelRecord {
    id: u64,
    /// The orphan lane `cancel()` seeded (`lifecycleLane`), captured before the
    /// interrupt is awaited so the receipt reconciliation acts on the same lane
    /// even when `init` latches `msg_lifecycle_v1` meanwhile.
    lifecycle_lane: bool,
    /// The uuids of the queued turns this cancel swept (`orphanedTurns`).
    orphaned: Vec<String>,
}

/// What a `command_lifecycle` frame tells the session to log
/// (`acp-agent.ts:5865-5927`): drift that silently degrades orphan accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleDiagnostic {
    /// `started` arrived after the command's terminal frame.
    StartedAfterTerminal,
    /// A state outside the v1 vocabulary.
    UnknownState(String),
}

impl Turn {
    /// A new, unsent prompt turn.
    pub fn new(prompt_uuid: String, is_local_only_command: bool) -> Self {
        Self {
            prompt_uuid,
            is_local_only_command,
            settled: false,
            deferred_settle: None,
            spawned_task_ids: HashSet::new(),
            steered_echoes: None,
            steered_settle: None,
            steered_awaiting_result: false,
            steered_uuids: Vec::new(),
            notice_texts: Vec::new(),
            command_started: false,
            command_finished: None,
            command_result_seen: false,
            carried: None,
            frame: None,
        }
    }

    /// Keep the frame this prompt sent (see the `frame` field).
    pub fn with_frame(mut self, frame: Value) -> Self {
        self.frame = Some(frame);
        self
    }

    /// The kept prompt frame, once.
    pub fn take_frame(&mut self) -> Option<Value> {
        self.frame.take()
    }

    /// The event that ends a prompt a clear-context restart could not carry on:
    /// it never ran, so it reports no usage (a swept queued turn's settle).
    pub fn queued_settled(&self, stop_reason: StopReason) -> TurnEvent {
        TurnEvent::Settled {
            prompt_uuid: self.prompt_uuid.clone(),
            stop_reason,
            usage: Usage::default(),
            had_usage: false,
            model_rows: SharedRows::default(),
        }
    }

    /// Whether the turn is already settled.
    pub fn is_settled(&self) -> bool {
        self.settled
    }

    /// The turn's prompt uuid (its echo identity).
    pub fn prompt_uuid(&self) -> &str {
        &self.prompt_uuid
    }

    /// Whether this is a local-only command (result text is the command output).
    pub fn is_local_only_command(&self) -> bool {
        self.is_local_only_command
    }

    /// The event that ends a turn a clear-context restart took out of its
    /// machine and could not hand on (`settleCancelledTurn`,
    /// `acp-agent.ts:2800-2806`): it reports the usage the turn had earned
    /// before the restart (`carriedUsage`).
    pub fn restart_settled(&self, stop_reason: StopReason) -> TurnEvent {
        let (usage, rows) = self.carried.clone().unwrap_or_default();
        TurnEvent::Settled {
            prompt_uuid: self.prompt_uuid.clone(),
            stop_reason,
            usage,
            had_usage: true,
            model_rows: Arc::new(rows),
        }
    }
}

/// The driver of the turn state machine — one instance per session.
///
/// Owned by the session actor (D4); only the actor mutates it. Fed with the
/// session frames the actor routes to it; returns the [`TurnEvent`]s the actor
/// must emit. Every lane ports the upstream contract at `ADP` 1338–3612,
/// reworked per review 04.
#[derive(Debug, Default)]
pub struct TurnMachine {
    /// The active (in-flight) turn, if any. `settle`/`fail_active` null it, like
    /// upstream `settleActive`/`failActive` (review 04 #5).
    active: Option<Turn>,
    /// Turns waiting for their echo / result, FIFO.
    queue: VecDeque<Turn>,
    /// Subagent task ids currently live, and whether each is a subagent whose
    /// completion can defer a turn (`liveBackgroundTasks`, `isSubagent`).
    live_subagents: HashMap<String, bool>,
    /// Number of late results still expected from dead (cancelled) turns
    /// (`pendingOrphanResults`). Late results are skipped, never attributed to
    /// a live turn (INV-30). Decremented before the queue-head promotion
    /// (`ADP:1439-1442`).
    pending_orphan_results: usize,
    /// What each [`Self::cancel`] swept, kept until its interrupt receipt arrives
    /// (upstream's `orphanedTurns` and `lifecycleLane`, local to each `cancel()`).
    /// Consumed by [`Self::reconcile_cancel_receipt`] to drop the orphan credits of
    /// turns the interrupt's receipt shows as dropped (`still_queued` absent), so a
    /// stale credit can't swallow a later echo-less result (INV-23,
    /// `acp-agent.js:3608-3648`). Per cancel, so two cancels before the first
    /// receipt each reconcile the lane and turns they seeded.
    cancel_records: Vec<CancelRecord>,
    /// The id of the most recent [`Self::cancel`] (see [`CancelRecord`]).
    cancel_seq: u64,
    /// The uuids of the messages a `session/cancel` abandons, in Node's order:
    /// pending steered echoes first (insertion order), then the swept queued
    /// turns FIFO (E-8, `dist/acp-agent.js:6253-6264`). The actor sends one
    /// `cancel_async_message{message_uuid}` per uuid before the `interrupt`
    /// (2.13). Drained by [`Self::take_abandoned`].
    abandoned_uuids: Vec<String>,
    /// Number of trailing `idle` frames still owed by settled/autonomous/
    /// cancelled turns (`owedTrailingIdles`) — absorbed by `on_idle` so a
    /// lagging idle can't false-fail the next live turn (#825).
    owed_trailing_idles: usize,
    /// The session recognised the result about to be finished as the
    /// diagnostic of an ExitPlanMode "keep planning" interrupt
    /// (`isExitPlanInterruptionResult`): the turn ends `cancelled` at it.
    exit_plan_interruption: Option<ExitPlanVerdict>,
    /// The turn a clear-context restart took out of the machine at its
    /// interruption result, for the session to carry into the fresh context.
    restart_turn: Option<Turn>,
    /// The prompts that were queued behind that turn, in order: they go to the
    /// fresh context with it (see [`Self::take_restart_queue`]).
    restart_queue: Vec<Turn>,
    /// Whether the active turn was cancelled (its result is dropped; it settles
    /// `cancelled` at the next idle).
    cancelled: bool,
    /// The last `session_state_changed` state (`lastSessionState`), read by
    /// `cancel()`'s held-turn debt rule (`ADP:3571`).
    last_session_state: String,
    /// Per-active-turn token accumulator (`accumulatedUsage`), reset at
    /// activation and snapshot into every [`TurnEvent::Settled`].
    accumulated_usage: Usage,
    /// The same tally split by model (`accumulatedModelUsage`), reset at the
    /// same statements as `accumulated_usage`.
    accumulated_models: ModelRows,
    /// The session-long `result.modelUsage` reading (`lastModelUsageReading`):
    /// NEVER reset per turn. `awaiting_resume_seed` stands for Node's
    /// `undefined` (a resumed session's first result has no predecessor).
    last_model_reading: ModelRows,
    awaiting_resume_seed: bool,
    /// The top-level assistant model of the current turn (`lastAssistantModel`),
    /// cleared at activation (`resetTurnScratch`).
    last_assistant_model: Option<String>,
    /// The classifier's explanation of a refusal (`lastRefusalExplanation`),
    /// seeded by the refused assistant frame's `stop_details` or the
    /// `model_refusal_no_fallback` banner and streamed when the turn's result
    /// reports the `refusal` stop reason. Cleared at activation.
    refusal_explanation: Option<String>,
    /// The CLI advertised `msg_lifecycle_v1` at `system`/`init` (`msgLifecycleV1`):
    /// orphan accounting then runs through [`Self::orphan_commands`] instead of
    /// the coalescing-blind count. Never unlatched (init re-emits per turn).
    msg_lifecycle_v1: bool,
    /// The CLI advertised `request_marker_lists_v1` (`requestMarkerListsV1`).
    request_marker_lists_v1: bool,
    /// A model request started since the last result (`modelRequestSinceResult`):
    /// with request markers it tells a live follow-up cycle from a hold, since
    /// CLI 2.1.270 reports `running` for as long as background agents live.
    model_request_since_result: bool,
    /// The per-uuid orphan map (`orphanCommands`), insertion-ordered like the
    /// JS `Map`: `command_lifecycle` terminal frames and the echo-less-result
    /// skip drain it; activation clears it.
    orphan_commands: Vec<(String, OrphanState)>,
    /// Commands whose cancelled cycle may still emit an empty-interruption
    /// `[ede_diagnostic]` result (`pendingEmptyInterruptionDiagnosticCommands`).
    empty_interruption_commands: Vec<String>,
    /// The prompts (and steers) a cancel stopped (`cancelledPrompts`), judged
    /// against late model output and echo-less result stamps; cleared at
    /// activation.
    cancelled_prompts: HashSet<String>,
    /// The stopped-work window's drop latches and reached tool uses (F16b).
    stopped: StoppedWorkState,
    /// What `task_started` said about each live task (`liveBackgroundTasks`
    /// records), read to tell a running background subagent's output from the
    /// work a cancel stopped.
    task_meta: HashMap<String, TaskMeta>,
    /// The settled subagents a later resume can register again, with the
    /// Agent/Task call that started them (`resumableSubagents`), oldest first.
    resumable_subagents: Vec<(String, Option<String>)>,
    /// The client gets a subagent's text (`forwardsSubagentText`).
    forwards_subagent_text: bool,
    /// A model cycle that Claude Code started on its own is the session's
    /// foreground work (F16c, `runsAutonomousCycle`).
    autonomous_cycle: bool,
    /// `session/cancel` stops the background workflows the interrupt spared.
    stops_workflows_on_cancel: bool,
    /// A transition closed the assistant-text delivery stretch (upstream clears
    /// `session.emittedAssistantText` there): a held turn settling, a failed turn,
    /// and an autonomous result with nothing in flight. The actor owns the flag
    /// itself and reads this through [`Self::take_text_stretch_closed`].
    text_stretch_closed: bool,
}

impl TurnMachine {
    /// A fresh machine with no turns.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the session as resumed (`creationOpts.resume !== undefined`): its
    /// first `result.modelUsage` reading already holds the pre-resume history,
    /// so it only seeds the baseline (`lastModelUsageReading: undefined`).
    pub fn mark_resumed(&mut self) {
        self.awaiting_resume_seed = true;
    }

    /// Record the top-level assistant model of the current turn
    /// (`lastAssistantModel`). `<synthetic>` and empty models are ignored.
    pub fn note_assistant_model(&mut self, model: &str) {
        if !model.is_empty() && model != "<synthetic>" {
            self.last_assistant_model = Some(model.to_string());
        }
    }

    /// A refused top-level assistant frame (`stop_reason: "refusal"`) names the
    /// explanation in `stop_details`; a frame without one keeps what an earlier
    /// banner seeded (`acp-agent.ts:7873-7881`).
    pub fn note_refusal_frame(&mut self, explanation: Option<&str>) {
        if let Some(explanation) = explanation {
            self.refusal_explanation = Some(explanation.to_string());
        }
    }

    /// The `model_refusal_no_fallback` banner backs the explanation up when the
    /// frame carried none. `refused_user_message_uuid` is explicitly `null` when
    /// the refused turn was not human-authored: those must not pollute the user
    /// turn's explanation (`acp-agent.ts:6710-6728`).
    pub fn seed_refusal_explanation(&mut self, explanation: &str, not_human_authored: bool) {
        let empty = self
            .refusal_explanation
            .as_deref()
            .is_none_or(str::is_empty);
        if empty && !not_human_authored {
            self.refusal_explanation = Some(explanation.to_string());
        }
    }

    /// The CLI's own "Please run /login" reply ends the active turn with the
    /// auth-required rejection (`failActive(RequestError.authRequired())`,
    /// `acp-agent.ts:5584-5588`). Nothing happens without an unsettled active turn.
    pub fn fail_active_auth_required(&mut self, message: &str) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        self.fail_active(
            FailureKind::AuthRequired,
            message.to_string(),
            false,
            &mut events,
        );
        events
    }

    /// The stream ended after `worker_shutting_down`: an active turn that is not
    /// held open for its background work fails with `worker_shutdown`. A held
    /// turn already has its authoritative outcome and an absent one has nothing
    /// to carry the failure, so both get no event here and the caller publishes
    /// the session-scoped failure instead (`acp-agent.ts:5750-5767`).
    pub fn fail_active_worker_shutdown(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        if self.active.as_ref().is_some_and(is_held_turn) {
            return events;
        }
        self.fail_active(
            FailureKind::WorkerShutdown,
            FailureKind::WorkerShutdown.default_message().to_string(),
            false,
            &mut events,
        );
        events
    }

    /// Mark the result about to be finished as the interruption of an
    /// ExitPlanMode answer (`acp-agent.ts:7110-7141`, `7219-7238`): instead of
    /// reading the error-shaped diagnostic as a provider failure, "keep
    /// planning" ends the turn `cancelled` with the usage its own result
    /// recorded, and an accepted clear-context plan hands the turn on.
    pub fn note_exit_plan_interruption(&mut self, verdict: ExitPlanVerdict) {
        self.exit_plan_interruption = Some(verdict);
    }

    /// The turn a [`ExitPlanVerdict::ContextReset`] result took out of the
    /// machine, once.
    pub fn take_restart_turn(&mut self) -> Option<Turn> {
        self.restart_turn.take()
    }

    /// The unsettled prompts that were queued behind the restart turn, once.
    pub fn take_restart_queue(&mut self) -> Vec<Turn> {
        std::mem::take(&mut self.restart_queue)
    }

    /// Whether the active turn is held open for background work
    /// (`isHeldOpen(session.activeTurn)`).
    pub fn active_is_held(&self) -> bool {
        self.active.as_ref().is_some_and(is_held_turn)
    }

    /// The `prompt_uuid` of the active unsettled turn (`session.activeTurn?.promptUuid`).
    pub fn active_prompt_uuid(&self) -> Option<&str> {
        self.active
            .as_ref()
            .filter(|t| !t.settled)
            .map(|t| t.prompt_uuid.as_str())
    }

    /// Remember the text of an informational notice on the turn it belongs to
    /// (`session.activeTurn ?? session.turnQueue?.[0]`): the #453 fallback skips
    /// a result that only repeats it.
    pub fn note_notice_text(&mut self, normalized: String) {
        if let Some(turn) = self.active.as_mut().or_else(|| self.queue.front_mut()) {
            turn.notice_texts.push(normalized);
        }
    }

    /// Whether a transition since the last call closed the delivery stretch of
    /// assistant text (see the field), clearing the record.
    pub fn take_text_stretch_closed(&mut self) -> bool {
        std::mem::take(&mut self.text_stretch_closed)
    }

    /// A snapshot of the turn's per-model rows.
    fn models_snapshot(&self) -> SharedRows {
        Arc::new(self.accumulated_models.clone())
    }

    /// Advance the session-long `modelUsage` reading for one `result` and
    /// return its per-model increment (`acp-agent.js:4946-4962`). Runs for
    /// autonomous results too; only a user result folds it into the tally.
    fn advance_model_reading(&mut self, result: &Value) -> ModelRows {
        let reading = quota::normalize(result.get("modelUsage"));
        let inc = if self.awaiting_resume_seed {
            self.awaiting_resume_seed = false;
            let matched = self
                .last_assistant_model
                .as_deref()
                .and_then(|m| quota::matching_key(&reading, m));
            quota::resumed_first(
                result.get("usage"),
                matched.as_deref().or(self.last_assistant_model.as_deref()),
            )
        } else {
            quota::increment(&reading, &self.last_model_reading)
        };
        self.last_model_reading = reading;
        inc
    }

    /// Whether there is an active, unsettled turn.
    pub fn has_active(&self) -> bool {
        self.active.as_ref().is_some_and(|t| !t.settled)
    }

    /// Whether a turn is in flight: an active unsettled turn, or any queued
    /// turn awaiting its echo. Mirrors the Node's `turnInFlight` check
    /// (`(session.turnQueue ?? []).find((turn) => !turn.settled)`,
    /// `acp-agent.js:1141`), used by `_session/steering`.
    pub fn has_unsettled(&self) -> bool {
        self.has_active() || self.queue.iter().any(|t| !t.settled)
    }

    /// The `prompt_uuid` of the turn a newly surfaced foreground tool call
    /// belongs to: the active unsettled turn, else the first unsettled queued
    /// turn (Node `recordForegroundToolCall`, `session.activeTurn ??
    /// turnQueue.find(!settled)`). Read-only; touches no debt book (3.7).
    pub fn foreground_target(&self) -> Option<&str> {
        self.active
            .as_ref()
            .filter(|t| !t.settled)
            .or_else(|| self.queue.iter().find(|t| !t.settled))
            .map(|t| t.prompt_uuid.as_str())
    }

    /// Mark a `_session/steering` injected `priority:"now"` message as steered
    /// (R2, `acp-agent.js:1166-1180`). Targets the active turn if there is an
    /// unsettled one, else the first unsettled turn in the queue ("first
    /// unsettled", `acp-agent.js:1141`). Adds `uuid` to the turn's pending
    /// `steered_echoes`; if the turn is held for subagents, moves its recorded
    /// `deferred_settle` into the steer lane so one lane owns settlement.
    /// Returns `false` (and changes nothing) when no target turn exists.
    pub fn mark_steered(&mut self, uuid: &str) -> bool {
        let target = if self.active.as_ref().is_some_and(|t| !t.settled) {
            self.active.as_mut()
        } else {
            let idx = self.queue.iter().position(|t| !t.settled);
            idx.map(|i| &mut self.queue[i])
        };
        let Some(turn) = target else {
            return false;
        };
        turn.steered_echoes
            .get_or_insert_with(Vec::new)
            .push(uuid.to_string());
        turn.steered_uuids.push(uuid.to_string());
        if turn.deferred_settle.is_some() {
            turn.steered_settle = turn.deferred_settle.take();
        }
        true
    }

    /// Enqueue a prompt turn (it awaits its echo before activation).
    pub fn enqueue(&mut self, turn: Turn) {
        self.queue.push_back(turn);
    }

    /// Take the active unsettled turn out of this machine for a session restart
    /// (clear-context, sign-out respawn), stamping it with the usage and per-model
    /// rows it earned so far (`continuePlanInFreshContext`:
    /// `turn.carriedUsage = oldSession.accumulatedUsage`,
    /// `clear-context-coordinator.ts:136-141`). The old machine is left with no
    /// turns and no orphan, hold or cancel bookkeeping — the query it fed is gone.
    pub fn take_active_for_restart(&mut self) -> Option<Turn> {
        let mut turn = self.active.take().filter(|t| !t.settled)?;
        // The fresh query has none of this one's background tasks, so a hold can
        // never drain there: the continuation's own result settles the turn.
        turn.deferred_settle = None;
        turn.carried = Some((self.accumulated_usage, self.accumulated_models.clone()));
        // Upstream drops the queue here (`oldSession.turnQueue = []`), leaving
        // those prompts without a response; they are kept, so the fresh context
        // can run and answer them.
        self.restart_queue = self.queue.drain(..).filter(|t| !t.settled).collect();
        self.pending_orphan_results = 0;
        self.orphan_commands.clear();
        self.cancel_records.clear();
        self.empty_interruption_commands.clear();
        self.cancelled_prompts.clear();
        self.owed_trailing_idles = 0;
        self.live_subagents.clear();
        self.task_meta.clear();
        self.cancelled = false;
        self.autonomous_cycle = false;
        Some(turn)
    }

    /// Put a turn taken by [`Self::take_active_for_restart`] at the head of this
    /// (fresh) machine's queue ("rebase the active turn to the queue head"): its
    /// echo activates it, and its carried usage seeds the scratch.
    pub fn adopt_restarted_turn(&mut self, turn: Turn) {
        self.queue.push_front(turn);
    }

    /// Latch the lifecycle capabilities a `system`/`init` frame advertises
    /// (`acp-agent.ts:5949-5960`). Never unlatches: init re-emits per turn and a
    /// capability cannot be lost mid-CLI.
    pub fn latch_capabilities(&mut self, capabilities: &[&str]) {
        if capabilities.contains(&"msg_lifecycle_v1") {
            self.msg_lifecycle_v1 = true;
        }
        if capabilities.contains(&"request_marker_lists_v1") {
            self.request_marker_lists_v1 = true;
        }
    }

    /// Whether the CLI advertised `request_marker_lists_v1`.
    pub fn request_marker_lists_v1(&self) -> bool {
        self.request_marker_lists_v1
    }

    /// The unsettled turn owning this prompt uuid, in the active slot or the
    /// queue (`findUnsettledTurn`; upstream keeps the active turn in
    /// `turnQueue`).
    fn find_unsettled_turn_mut(&mut self, uuid: &str) -> Option<&mut Turn> {
        self.active
            .iter_mut()
            .chain(self.queue.iter_mut())
            .find(|t| t.prompt_uuid == uuid && !t.settled)
    }

    fn orphan_state(&self, uuid: &str) -> Option<OrphanState> {
        self.orphan_commands
            .iter()
            .find(|(u, _)| u == uuid)
            .map(|(_, s)| *s)
    }

    /// `Map.set`: an existing key keeps its insertion position.
    fn set_orphan_state(&mut self, uuid: &str, state: OrphanState) {
        match self.orphan_commands.iter_mut().find(|(u, _)| u == uuid) {
            Some(entry) => entry.1 = state,
            None => self.orphan_commands.push((uuid.to_string(), state)),
        }
    }

    fn remove_orphan(&mut self, uuid: &str) {
        self.orphan_commands.retain(|(u, _)| u != uuid);
    }

    fn remove_empty_interruption(&mut self, uuid: &str) {
        self.empty_interruption_commands.retain(|u| u != uuid);
    }

    /// Route one orphaned command into the orphan-accounting lane: the per-uuid
    /// map on `msg_lifecycle_v1` CLIs (drained by the command's own terminal
    /// lifecycle frame and the echo-less-result skip), the plain count elsewhere
    /// (`trackOrphanCommand`, `acp-agent.ts:8335-8346`). Every orphan-producing
    /// path — `cancel()`'s queued sweep, the steered-echo seeding and the
    /// force-cancel floor — seeds through here so the lane split stays one
    /// mechanism.
    ///
    /// Known window: `msg_lifecycle_v1` is only learnable from the stream's first
    /// `system`/init, so a cancel that beats that drain seeds the COUNT lane on a
    /// lifecycle-capable CLI (still healed by the next activation's reset).
    fn track_orphan_command(&mut self, uuid: &str, state: OrphanState) {
        if self.msg_lifecycle_v1 {
            self.set_orphan_state(uuid, state);
        } else {
            self.pending_orphan_results += 1;
        }
    }

    /// Seed the orphan accounting of an abandoned or force-cancelled turn by
    /// what its lifecycle frames already told us (`acp-agent.ts:8546-8590`,
    /// `5680-5705`): the consumer may have drained the turn's result and/or
    /// terminal frame before the cancel ran, and an entry seeded for a result or
    /// frame that is already spent would never drain (it would swallow an
    /// unrelated later echo-less result).
    fn seed_orphan_by_fate(&mut self, turn: &Turn) {
        match turn.command_finished {
            // Finished SDK-side and its terminal frame was consumed: nothing is
            // left to skip, and a seeded entry would never drain.
            Some(CommandFate::Completed | CommandFate::Discarded | CommandFate::Refused) => {}
            // Terminal frame already consumed. Dispatched-then-aborted: the dead
            // turn's late result may still come — seed the zombie the frame
            // handler would have made — unless that result already passed.
            // Never dispatched: dropped, no result coming, nothing to track.
            Some(CommandFate::Cancelled) => {
                if turn.command_started && !turn.command_result_seen {
                    self.track_orphan_command(&turn.prompt_uuid, OrphanState::Zombie);
                }
            }
            None => {
                // Dispatched and its turn's result already passed: only the
                // terminal frame is outstanding, which no-ops with no entry.
                if turn.command_started && turn.command_result_seen {
                    return;
                }
                let state = if turn.command_started {
                    OrphanState::Started
                } else {
                    OrphanState::Pending
                };
                self.track_orphan_command(&turn.prompt_uuid, state);
            }
        }
    }

    /// A `command_lifecycle` frame (`msg_lifecycle_v1`, `acp-agent.ts:5839-5945`).
    /// It feeds the orphan accounting only: turn activation and settlement stay
    /// driven by echoes, results and idle. Returns drift worth logging.
    pub fn on_command_lifecycle(&mut self, uuid: &str, state: &str) -> Option<LifecycleDiagnostic> {
        match state {
            "started" => {
                // Remember dispatch on the live turn so a cancel() that orphans
                // it seeds the right state...
                if let Some(turn) = self.find_unsettled_turn_mut(uuid) {
                    turn.command_started = true;
                    // Claude Code took the prompt in: it is the foreground work
                    // from here, and takes over a cycle Claude Code started on its
                    // own (`reportInserted` -> `startForeground`). While another
                    // turn is still active, insertion waits for this turn's echo
                    // (`acp-agent.ts:5846-5858`).
                    if self
                        .active
                        .as_ref()
                        .is_none_or(|a| a.settled || a.prompt_uuid == uuid)
                    {
                        self.autonomous_cycle = false;
                    }
                }
                // ...and promote an already-orphaned command: once dispatched, a
                // bare `cancelled` no longer means "dropped without running".
                match self.orphan_state(uuid) {
                    Some(OrphanState::Pending) => {
                        self.set_orphan_state(uuid, OrphanState::Started);
                    }
                    Some(OrphanState::Zombie) => {
                        return Some(LifecycleDiagnostic::StartedAfterTerminal)
                    }
                    _ => {}
                }
                None
            }
            "completed" | "discarded" | "refused" | "cancelled" => {
                let fate = match state {
                    "completed" => CommandFate::Completed,
                    "discarded" => CommandFate::Discarded,
                    "refused" => CommandFate::Refused,
                    _ => CommandFate::Cancelled,
                };
                // Latch the fate on a still-queued turn so a later cancel()
                // doesn't seed an orphan entry for a command whose
                // one-and-only terminal frame has already been consumed.
                if let Some(turn) = self.find_unsettled_turn_mut(uuid) {
                    turn.command_finished = Some(fate);
                }
                if fate == CommandFate::Cancelled {
                    // Ambiguous by design (dup-over-loss): dropped before
                    // dispatch (no result will ever come — safe to forget) vs
                    // consumed into a turn that was aborted/failed. For the
                    // latter any result the dead turn managed to emit has
                    // already deleted the entry, so a still-"started" entry
                    // means no result was seen since dispatch — it becomes a
                    // zombie for the next echo-less-result skip.
                    match self.orphan_state(uuid) {
                        Some(OrphanState::Pending) => {
                            self.remove_orphan(uuid);
                            self.remove_empty_interruption(uuid);
                        }
                        Some(OrphanState::Started) => {
                            self.set_orphan_state(uuid, OrphanState::Zombie);
                        }
                        _ => {}
                    }
                    return None;
                }
                // Exactly-one-terminal: the command is finished — no echo-less
                // result remains to skip.
                self.remove_orphan(uuid);
                self.remove_empty_interruption(uuid);
                None
            }
            // "queued" carries no fate information; anything else is a state
            // this adapter doesn't know (a CLI that grew the v1 vocabulary).
            "queued" => None,
            other => Some(LifecycleDiagnostic::UnknownState(other.to_string())),
        }
    }

    /// The orphan map entries, oldest first (test and diagnostics seam).
    pub fn orphan_commands(&self) -> &[(String, OrphanState)] {
        &self.orphan_commands
    }

    /// A `session/cancel` notification. Ports `ADP:3443-3612`'s turn-settlement
    /// half (the wire `interrupt` is the actor's job): sweep every queued,
    /// non-active turn to `Settled{Cancelled}` and seed one orphan per turn
    /// (their user messages were already pushed), inline-settle a held active
    /// turn `cancelled`, and mark the session cancelled so the live turn
    /// settles `cancelled` at its trailing idle. Returns the events for the
    /// actor to resolve.
    pub fn cancel(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        self.cancelled = true;

        // Judge late model output and echo-less result stamps against the
        // prompts this cancel stopped, recorded before anything else can run
        // (`cancelledPrompts`, `acp-agent.ts:8452-8457`): every unsettled turn
        // and every steer it took.
        for turn in self.active.iter().chain(self.queue.iter()) {
            if turn.settled {
                continue;
            }
            self.cancelled_prompts.insert(turn.prompt_uuid.clone());
            self.cancelled_prompts
                .extend(turn.steered_uuids.iter().cloned());
        }

        // R9: a steered active turn's pending echoes are interrupts whose
        // messages were already pushed — seed one orphan per pending steered
        // echo so their late results are skipped, not attributed to the next
        // head (`acp-agent.js:3459-3463`). Do NOT push these uuids into
        // the cancel record — reconcile touches only swept turns (INV-23).
        // 2.13 (E-8): they are the first abandoned uuids in Node's order
        // (`dist/acp-agent.js:6253-6264`).
        let steered_pending: Vec<String> = if self.active.as_ref().is_some_and(is_steering_turn) {
            self.active
                .as_ref()
                .and_then(|t| t.steered_echoes.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for uuid in steered_pending {
            self.track_orphan_command(&uuid, OrphanState::Pending);
            self.abandoned_uuids.push(uuid);
        }

        // Captured before anything can await: `init` may latch the capability
        // while the interrupt is in flight, and the receipt reconciliation must
        // act on the lane the seeding used (`lifecycleLane`).
        let lifecycle_lane = self.msg_lifecycle_v1;
        let mut orphaned = Vec::new();

        // Sweep queued turns that haven't started yet (no echo): settle them
        // now and seed the orphan accounting per turn so their late results are
        // skipped, not attributed to the next head (`ADP:3471-3526`). Their
        // prompt uuids are the abandoned tail in FIFO order (2.13 / E-8).
        let mut remaining = VecDeque::new();
        let mut swept = Vec::new();
        for turn in self.queue.drain(..) {
            if turn.settled {
                remaining.push_back(turn);
            } else {
                swept.push(turn);
            }
        }
        self.queue = remaining;
        for turn in swept {
            // Seeded by what the lifecycle frames already told us (the count
            // lane never sees them, so every turn takes the plain-seed path).
            self.seed_orphan_by_fate(&turn);
            // Echo-less `[ede_diagnostic]` hand-off tokens: the cancelled cycle
            // may still emit its empty-interruption diagnostic.
            let spent = matches!(
                turn.command_finished,
                Some(CommandFate::Completed | CommandFate::Discarded | CommandFate::Refused)
            ) || (turn.command_started && turn.command_result_seen)
                || (turn.command_finished == Some(CommandFate::Cancelled) && !turn.command_started);
            if !spent {
                self.empty_interruption_commands
                    .push(turn.prompt_uuid.clone());
            }
            orphaned.push(turn.prompt_uuid.clone());
            self.abandoned_uuids.push(turn.prompt_uuid.clone());
            events.push(TurnEvent::Settled {
                prompt_uuid: turn.prompt_uuid,
                stop_reason: StopReason::Cancelled,
                usage: Usage::default(),
                had_usage: false,
                model_rows: SharedRows::default(),
            });
        }
        self.cancel_seq += 1;
        if !orphaned.is_empty() {
            self.cancel_records.push(CancelRecord {
                id: self.cancel_seq,
                lifecycle_lane,
                orphaned,
            });
        }
        // A prompt or steer that arrived while this cancel awaited is abandoned
        // too: judged against the stopped work from here on.
        self.cancelled_prompts
            .extend(self.abandoned_uuids.iter().cloned());

        // Inline-settle a held active turn `cancelled` — during the hold the
        // session is usually already idle, so the interrupt may produce no
        // fresh idle for the cancelled-settle path to run on. Pre-count the
        // interrupt's trailer debt unless the session already sits idle
        // (`ADP:3528-3575`).
        //
        // A held turn whose follow-up cycle is live (running, or blocked on a
        // permission request) is left to that cycle's interrupted result, like
        // any live turn, or to the idle when no result comes. Since CLI 2.1.270
        // Claude Code reports `running` for as long as background agents live,
        // so the session state cannot tell a live follow-up from a hold; with
        // request markers a follow-up is live when one of its model requests
        // started after the turn's result (`acp-agent.ts:8630-8638`).
        let followup_live = if self.request_marker_lists_v1 {
            self.model_request_since_result
        } else {
            self.last_session_state == "running" || self.last_session_state == "requires_action"
        };
        if self.active.as_ref().is_some_and(is_held_turn) && !followup_live {
            if self.last_session_state != "idle" {
                self.owed_trailing_idles += 1;
            }
            if let Some(active) = self.active.take() {
                if !active.settled {
                    let prompt_uuid = active.prompt_uuid.clone();
                    // A held turn's cancel reports the usage its result already
                    // recorded (`deferredSettle.usage`), not the live accumulator
                    // (`ADP:3574`; review-05 minor).
                    let (usage, model_rows) = match &active.deferred_settle {
                        Some(d) => (d.usage, d.models.clone()),
                        None => (self.accumulated_usage, self.models_snapshot()),
                    };
                    events.push(TurnEvent::Settled {
                        prompt_uuid,
                        stop_reason: StopReason::Cancelled,
                        usage,
                        had_usage: true,
                        model_rows,
                    });
                }
            }
        }
        // R9: a steered active turn with a recorded outcome settles `cancelled`
        // at the interrupt's trailing idle, like any live turn. But the steer
        // lane pays for its last result's trailer by settling on it, which this
        // cancel pre-empts — count that trailer here or it arrives un-owed
        // against the next prompt and false-fails it (#825) (`acp-agent.js:3577-3586`).
        if self.active.as_ref().is_some_and(is_steering_turn)
            && self
                .active
                .as_ref()
                .and_then(|t| t.steered_settle.as_ref())
                .is_some()
        {
            self.owed_trailing_idles += 1;
        }
        events
    }

    /// Drain the uuids a `cancel()` abandoned, in Node's order: pending steered
    /// echoes first (insertion order), then swept queued turns FIFO (E-8,
    /// `dist/acp-agent.js:6253-6264`). The actor sends one
    /// `cancel_async_message{message_uuid}` per uuid before the `interrupt`
    /// (2.13). Idempotent: drains whatever was recorded by the most recent
    /// `cancel()`.
    pub fn take_abandoned(&mut self) -> Vec<String> {
        std::mem::take(&mut self.abandoned_uuids)
    }

    /// Force-cancel the active turn and clear the active slot, seeding one
    /// orphan credit for its late `result` (INV-30). This is the **force-cancel
    /// floor** lane (phase 11, `ADP:1719-1752`), NOT `cancel()` — keep that
    /// name for the review's `cancel()`. `abandon_active` is retained as this
    /// method for phase 11.
    pub fn force_cancel(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        // Seed by what the frames already told us (mirrors cancel()'s queued
        // sweep): the wedged turn WAS dispatched (it is active), so unless its
        // terminal frame or result is already spent it is tracked "started" —
        // its late result is skipped echo-less, and its terminal frame (or that
        // skip plus activation's clear when the frame is lost to the wedge)
        // drains it (`acp-agent.ts:5680-5705`).
        if let Some(active) = self.active.as_ref().filter(|t| !t.settled) {
            let mut turn = active.clone();
            // The active turn is dispatched by definition.
            turn.command_started = true;
            self.seed_orphan_by_fate(&turn);
        }
        if let Some(active) = self.active.as_mut() {
            if !active.settled {
                active.settled = true;
                events.push(TurnEvent::Settled {
                    prompt_uuid: active.prompt_uuid.clone(),
                    stop_reason: StopReason::Cancelled,
                    usage: self.accumulated_usage,
                    had_usage: true,
                    model_rows: self.models_snapshot(),
                });
            }
        }
        self.active = None;
        events
    }

    /// Reconcile the orphan-credit count against the `interrupt` receipt's
    /// `still_queued` list (phase 11, INV-23, `acp-agent.js:3608-3648`).
    ///
    /// On CLIs advertising `interrupt_receipt_v1`, `still_queued` lists exactly
    /// which queued messages survive the interrupt and will still run. An
    /// orphaned turn whose uuid is absent was dropped by the interrupt and will
    /// never emit a result — uncount it now instead of leaving a stale skip that
    /// `activateTurn`'s reset only clears once a later live ECHO arrives (an
    /// echo-less result in between would be wrongly swallowed by the leftover
    /// count).
    ///
    /// Two lanes, split by the lane `cancel()` seeded: the legacy **count lane**
    /// subtracts a count (race-safe against the consumer draining concurrently:
    /// dropped uuids produce no results, so decrements only consume the
    /// still-queued share); the `msg_lifecycle_v1` **map lane** forgets dropped
    /// orphans by uuid — only entries still `Pending`, since an orphan absent
    /// from `still_queued` because it was DISPATCHED (not dropped) has usually
    /// been promoted to `Started` by its lifecycle frame, and its own terminal
    /// frame must stay in charge of its fate (`acp-agent.ts:8761-8778`).
    ///
    /// Field guard (11.4): `still_queued` is `None` for a bare `{}` success
    /// receipt (or a CLI that resolves `undefined`), which must NOT read as
    /// "everything was dropped" — count-everything behaviour is kept and the
    /// activation-time self-heal bounds the damage.
    pub fn reconcile_orphan_receipt(&mut self, still_queued: Option<&[String]>) {
        self.reconcile_cancel_receipt(self.cancel_seq, still_queued);
    }

    /// The id of the most recent [`Self::cancel`], to hand to
    /// [`Self::reconcile_cancel_receipt`] with that cancel's interrupt receipt.
    pub fn last_cancel_id(&self) -> u64 {
        self.cancel_seq
    }

    /// [`Self::reconcile_orphan_receipt`] for the cancel `cancel_id` named: each
    /// cancel reconciles only the turns and the lane it seeded, as upstream's
    /// per-`cancel()` locals do. An id with nothing to reconcile is a no-op.
    pub fn reconcile_cancel_receipt(&mut self, cancel_id: u64, still_queued: Option<&[String]>) {
        let Some(at) = self.cancel_records.iter().position(|r| r.id == cancel_id) else {
            return;
        };
        let record = self.cancel_records.remove(at);
        // Field guard: guard the FIELD, not just the receipt — a bare `{}`
        // (no `still_queued` array) falls back to count-everything.
        let Some(still_queued) = still_queued else {
            return;
        };
        let dropped: Vec<String> = record
            .orphaned
            .iter()
            .filter(|uuid| !still_queued.contains(uuid))
            .cloned()
            .collect();
        for uuid in &dropped {
            self.remove_empty_interruption(uuid);
        }
        if record.lifecycle_lane {
            for uuid in &dropped {
                if self.orphan_state(uuid) == Some(OrphanState::Pending) {
                    self.remove_orphan(uuid);
                }
            }
        } else if !dropped.is_empty() {
            self.pending_orphan_results = self.pending_orphan_results.saturating_sub(dropped.len());
        }
    }

    /// A subagent task started (`task_started`, `subagent_type` set): record it
    /// as live and, if an active turn exists, attribute the spawn to it so the
    /// turn holds open for it (`#866`).
    pub fn on_task_started(&mut self, task_id: &str, is_subagent: bool) {
        self.live_subagents.insert(task_id.to_string(), is_subagent);
        if is_subagent {
            if let Some(active) = self.active.as_mut() {
                if !active.settled {
                    active.spawned_task_ids.insert(task_id.to_string());
                }
            }
        }
    }

    /// A task settled (`task_notification` / terminal `task_updated`): it is no
    /// longer live. Per `ADP:2336-2352`, this ONLY removes the registry entry —
    /// the held turn drains at the followup's autonomous result or at idle, NOT
    /// here (review 04 #8). Returns no events.
    pub fn on_task_ended(&mut self, task_id: &str) -> Vec<TurnEvent> {
        self.live_subagents.remove(task_id);
        self.settle_live_task(task_id);
        Vec::new()
    }

    /// The echo of a queued turn's user message arrives: promote it to active,
    /// handing off any prior active turn (cancelled first, then held, then
    /// end_turn — `ADP:3027-3079`). Returns the hand-off + activation events.
    pub fn on_echo(&mut self, uuid: &str) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        // Find the queued turn owning this uuid.
        let queued_idx = self.queue.iter().position(|t| t.prompt_uuid == uuid);
        let Some(idx) = queued_idx else {
            // A replayed echo with no matching queued turn — unrelated replay,
            // dropped (a steered message's echo; see isReplay handling).
            // R7: a steered message's echo matches no turn (mark_steered creates
            // none) but marks the steered cycle as running — delete the uuid so
            // the idle lane can tell "the answer is still ahead" from "the turn
            // is over" (`acp-agent.js:3092-3101`).
            if self.active.as_ref().is_some_and(is_steering_turn) {
                if let Some(active) = self.active.as_mut() {
                    if let Some(echoes) = active.steered_echoes.as_mut() {
                        // 2.13: insertion-ordered Vec — remove by identity.
                        let before = echoes.len();
                        echoes.retain(|u| u != uuid);
                        // The outcome recorded so far predates the steered
                        // cycle, so the idle lane waits for that cycle's own
                        // result (`steeredAwaitingResult`, `acp-agent.ts:7811`).
                        if echoes.len() != before {
                            active.steered_awaiting_result = true;
                        }
                    }
                }
            }
            return events;
        };
        // `idx` came from `position` above, so the removal cannot fail; handle
        // the `None` gracefully rather than panic (D8).
        let queued = match self.queue.remove(idx) {
            Some(q) => q,
            None => return events,
        };
        if let Some(prev) = self.active.take() {
            if !prev.settled {
                // A held previous turn's settle closes its delivery stretch; a
                // non-held one does not — the echo can land mid-message, so the
                // deltas already streamed belong to the turn being activated
                // (`acp-agent.ts:7790-7796`).
                if prev.deferred_settle.is_some() {
                    self.text_stretch_closed = true;
                }
                let (outcome, usage, model_rows) = if self.cancelled {
                    // A cancel is pending for the previous turn (its trailing
                    // idle hasn't arrived yet): settle it `cancelled` and record
                    // the interrupt's trailer debt so that lagged idle is
                    // absorbed, not read as the freshly-activated turn ending
                    // (`ADP:3034-3050`).
                    self.owed_trailing_idles += 1;
                    (
                        StopReason::Cancelled,
                        self.accumulated_usage,
                        self.models_snapshot(),
                    )
                } else if let Some(deferred) = prev.deferred_settle.clone() {
                    // A held turn hands off with its recorded outcome and usage
                    // snapshot, not a guessed end_turn (`ADP:3051-3060`).
                    (deferred.stop_reason, deferred.usage, deferred.models)
                } else if let Some(steered) = prev.steered_settle.clone() {
                    // R8: the user moving on outranks a steered continuation, but
                    // its last result's stop reason stands. The steer lane normally
                    // pays for that result's trailing idle by settling on it; this
                    // hand-off settles instead, so count the trailer here or it
                    // arrives un-owed against the fresh turn and false-fails it
                    // (#825) (`acp-agent.js:3062-3079`).
                    self.owed_trailing_idles += 1;
                    (steered.stop_reason, steered.usage, steered.models)
                } else {
                    (
                        StopReason::EndTurn,
                        self.accumulated_usage,
                        self.models_snapshot(),
                    )
                };
                let mut prev = prev;
                prev.settled = true;
                let prompt_uuid = prev.prompt_uuid.clone();
                events.push(TurnEvent::Settled {
                    prompt_uuid,
                    stop_reason: outcome,
                    usage,
                    had_usage: true,
                    model_rows,
                });
            }
        }
        self.activate(queued, &mut events);
        events
    }

    /// Whether `result` is an autonomous cycle's (not a user prompt's): an
    /// autonomous origin (`AUTONOMOUS_RESULT_ORIGINS`) that does NOT name a
    /// pending prompt. Claude Code folds a prompt sent while an autonomous
    /// (`task-notification`) turn runs into that turn; the result is still
    /// origin-marked but lists the uuids it answered in `user_message_uuids`
    /// (fallback `user_message_uuid`), and then takes the user lane
    /// (`answersPendingPrompt`, `dist/acp-agent.js:3205-3228`, R87-1). A turn
    /// held for its subagents already has its result and is not answered.
    /// Un-stamped and non-matching results are unchanged. Read-only.
    pub fn result_is_autonomous(&self, result: &Value) -> bool {
        is_autonomous_result(result) && !self.answers_pending_prompt(result)
    }

    /// Whether `on_result_finish` will swallow this user-lane result as the
    /// empty-interruption diagnostic of a cancelled command
    /// (`acp-agent.ts:7177-7197`): the swallow `break`s before
    /// `clearFailuresFromEarlierTurns`. Read-only; mirrors the swallow check.
    pub fn result_is_swallowed_diagnostic(&self, result: &Value) -> bool {
        let is_error = result
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if self.cancelled
            || !is_error
            || !is_empty_user_interruption_diagnostic(result)
            || self.empty_interruption_commands.is_empty()
        {
            return false;
        }
        match result.get("user_message_uuid").and_then(Value::as_str) {
            None => true,
            Some(stamp) => self.empty_interruption_commands.iter().any(|u| u == stamp),
        }
    }

    /// Whether this result is the interrupted cycle's error diagnostic of a
    /// steered turn that still has echoes pending (`acp-agent.ts:7259-7266`):
    /// the turn keeps going, so the result does not fail it. Read-only.
    pub fn result_is_steer_diagnostic(&self, result: &Value) -> bool {
        result
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && self.active.as_ref().is_some_and(|t| {
                is_steering_turn(t) && t.steered_echoes.as_ref().is_some_and(|s| !s.is_empty())
            })
    }

    fn answers_pending_prompt(&self, result: &Value) -> bool {
        let answered: Vec<&str> = match result.get("user_message_uuids").and_then(Value::as_array) {
            Some(uuids) => uuids.iter().filter_map(Value::as_str).collect(),
            None => result
                .get("user_message_uuid")
                .and_then(Value::as_str)
                .into_iter()
                .collect(),
        };
        answered.iter().any(|uuid| {
            let active = self
                .active
                .as_ref()
                .filter(|t| t.prompt_uuid == *uuid && !t.settled);
            match active {
                // Held open: already has its result, so this one does not answer it.
                Some(turn) => !is_held_turn(turn),
                None => self
                    .queue
                    .iter()
                    .any(|t| t.prompt_uuid == *uuid && !t.settled),
            }
        })
    }

    /// A `result` frame for the user's turn. Ports the full `ADP:2532-2884`
    /// lane: the autonomous-origin gate, `ensureActiveTurn`, owed-trailing-idle
    /// debt, usage accumulation, the stop-reason subtype table and the #453
    /// result-text fallback.
    ///
    /// `assistant_text_delivered` tells whether any assistant text reached the
    /// client before this result (the actor tracks `emittedAssistantText`).
    pub fn on_result(&mut self, result: &Value, assistant_text_delivered: bool) -> Vec<TurnEvent> {
        let begin = self.on_result_begin(result);
        self.on_result_finish(result, assistant_text_delivered, begin)
    }

    /// The first half of [`Self::on_result`]: everything upstream runs BEFORE the
    /// result's usage is accumulated and its `usage_update` is published — the
    /// autonomous gate, the orphan bookkeeping and `ensureActiveTurn` (which can
    /// settle a held turn and activate the queue head, resetting the per-turn
    /// scratch). The session actor runs its result-time emissions between the two
    /// halves so they read the post-activation scratch, in Node's order
    /// (`acp-agent.ts:6783-7024`, B-13 / N-29).
    pub fn on_result_begin(&mut self, result: &Value) -> ResultBegin {
        let mut events = Vec::new();
        // The result ends the model request(s) it answers (`modelRequestSinceResult`).
        self.model_request_since_result = false;
        let autonomous = self.result_is_autonomous(result);
        if !autonomous {
            // A user-turn result needs an active turn: promote the queue head
            // (echo-less local-only commands / compaction), settling any held
            // turn first (`ADP:1408-1489`; review 04 #2). The orphan
            // bookkeeping runs first: it covers folded/zombie commands whose
            // shared or late result this is, even when the result is the ACTIVE
            // turn's (`ensure_active_turn` never looks at the map then).
            self.record_result_for_orphan_commands();
            self.ensure_active_turn(result, &mut events);
            self.retire_named_steers(result);
        }
        ResultBegin { autonomous, events }
    }

    /// A result that names a steer consumed it, so the steered work is done:
    /// retire its echo (the CLI may never have replayed it) and, with no other
    /// steer outstanding, leave the steer lane so this result settles the turn
    /// through the ordinary lanes and owes its trailing idle like any other. The
    /// interrupted cycle's result names only the send it answered, so it stays
    /// on the lane. A cancelled turn's steers are `cancel()`'s to account for
    /// (`acp-agent.ts:6811-6853`, issue #1114).
    fn retire_named_steers(&mut self, result: &Value) {
        if self.cancelled {
            return;
        }
        let Some(turn) = self.active.as_mut().filter(|t| is_steering_turn(t)) else {
            return;
        };
        let mut answers_steer = false;
        for uuid in result_stamps(result) {
            if let Some(echoes) = turn.steered_echoes.as_mut() {
                echoes.retain(|e| e != uuid);
            }
            if turn.steered_uuids.iter().any(|s| s == uuid) {
                answers_steer = true;
            }
        }
        let drained = turn.steered_echoes.as_ref().is_some_and(Vec::is_empty);
        if answers_steer && drained {
            turn.steered_echoes = None;
            turn.steered_uuids.clear();
            turn.steered_awaiting_result = false;
            turn.steered_settle = None;
        }
    }

    /// The second half of [`Self::on_result`] (see [`Self::on_result_begin`]).
    pub fn on_result_finish(
        &mut self,
        result: &Value,
        assistant_text_delivered: bool,
        begin: ResultBegin,
    ) -> Vec<TurnEvent> {
        let ResultBegin {
            autonomous: is_autonomous,
            mut events,
        } = begin;
        let exit_plan_interruption = self.exit_plan_interruption.take();
        let is_error = result
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let stop_reason = result
            .get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        let result_text = result.get("result").and_then(Value::as_str).unwrap_or("");
        let subtype = result.get("subtype").and_then(Value::as_str).unwrap_or("");

        // An autonomous result (task-notification followup, or a peer/
        // coordinator/observer cycle) is not the user's prompt's: it owes one
        // trailing idle and may drain a held turn, but must never touch the
        // user-turn lifecycle — orphan accounting, failActive, the fallback or
        // the stop reason (`ADP:2532-2539`, `2713-2727`; review 04 #4).
        // The per-model reading advances for EVERY result (it is the baseline
        // the next increment is measured from) but only a user result folds
        // its increment into the tally (`acp-agent.js:4946-4965`). It does not
        // touch `owed_trailing_idles` or any other lane.
        // Advanced AFTER `ensureActiveTurn`'s activation reset the turn scratch
        // (upstream `acp-agent.ts:4843` vs `4946`): an echo-less promoted first
        // resumed result sees a cleared `lastAssistantModel`.
        let model_increment = self.advance_model_reading(result);
        // CLI 2.1.274+ answers background-task completions that were already
        // queued with ONE model call: every queued notification still gets its
        // own result, but all except the last are placeholders — `num_turns: 0`,
        // emitted BEFORE the shared follow-up runs. A placeholder is not the
        // summary a hold waits for (settling on it would release `session/prompt`
        // with the promised text still ahead), and it ends no cycle. Results
        // without `num_turns` (older fixtures) are treated as real ones.
        let placeholder =
            is_autonomous && result.get("num_turns").and_then(Value::as_u64) == Some(0);
        // A model cycle that Claude Code started on its own ends at its result —
        // also after a cancel. A turn that Claude Code takes in takes the cycle
        // over, so a cycle that still runs owns this result, whatever its origin
        // says (`acp-agent.ts:7026-7056`). A held turn that continues an accepted
        // plan in a fresh context takes the cycle over instead.
        let continues_plan = exit_plan_interruption == Some(ExitPlanVerdict::ContextReset)
            && self.active.as_ref().is_some_and(is_held_turn);
        if !placeholder && !continues_plan && std::mem::take(&mut self.autonomous_cycle) {
            events.push(TurnEvent::CycleEnded);
        }
        if is_autonomous {
            self.owed_trailing_idles += 1;
            if self.cancelled {
                // The cancelled turn ends at the result of the work the interrupt
                // stopped. For a turn held open for its subagents that is its
                // follow-up's aborted result; a follow-up result that is not
                // aborted (a placeholder, or one that ended before the interrupt
                // landed) belongs to other work and drains nothing here
                // (`acp-agent.ts:7084-7105`).
                if self.active.as_ref().is_some_and(is_held_turn) && is_aborted_result(result) {
                    let (usage, rows) = self.cancelled_outcome_usage();
                    self.settle_with_usage(StopReason::Cancelled, usage, rows, &mut events);
                }
                return events;
            }
            // A held turn's follow-up can itself plan: the interrupted cycle's
            // diagnostic carries that cycle's origin and lands here. "No, keep
            // planning" ends the held turn `cancelled` with the usage its own
            // result recorded, still held while other subagents it spawned are
            // live (`acp-agent.ts:7108-7141`).
            if let Some(verdict) =
                exit_plan_interruption.filter(|_| self.active.as_ref().is_some_and(is_held_turn))
            {
                match verdict {
                    ExitPlanVerdict::KeepPlanning => self.settle_held_cancelled(&mut events),
                    ExitPlanVerdict::ContextReset => {
                        self.restart_turn = self.take_active_for_restart()
                    }
                }
                return events;
            }
            // A hold settles only at a follow-up result that says it ran a turn
            // (`num_turns > 0`); one without the field keeps holding, like a
            // placeholder (`acp-agent.ts:7143-7146`, `undefined > 0` is false).
            if result.get("num_turns").and_then(Value::as_u64) > Some(0) {
                self.settle_deferred_if_drained(&mut events);
            }
            // With no turn in flight or queued the stretch holds only autonomous
            // prose: close it, so a replayed next prompt is not suppressed. A live
            // or queued turn's flag may guard the user's already-streamed text
            // (`acp-agent.ts:7155-7164`).
            if self.active.is_none() && !self.queue.iter().any(|t| !t.settled) {
                self.text_stretch_closed = true;
            }
            return events;
        }

        // Every user-turn result terminates a turn and is followed by a
        // trailing idle — record the debt so that idle is absorbed, not read as
        // the next turn being abandoned (#825). That includes the result of a
        // cancelled turn, which settles it below; a result skipped while
        // cancelled with NO active turn (an orphan the SDK still ran) gets no
        // such settle, so its trailer must be counted here too
        // (`acp-agent.ts:6905-6925`).
        //
        // R5: a steered turn's results owe no trailing idle — one idle covers
        // the whole interrupted + steered sequence and that idle settles the
        // turn, so counting either result would leave a debt that swallows it
        // (`acp-agent.js:2618-2625`). A cancelled steered turn's result is the
        // exception: it settles the turn at the cancelled guard.
        if self.cancelled || !self.active.as_ref().is_some_and(is_steering_turn) {
            self.owed_trailing_idles += 1;
        }

        // Accumulate usage into the active turn's tally (activation reset it).
        // Autonomous results were already returned above, so they can't leak
        // into a user turn's tally (`ADP:2633-2639`).
        if let Some(usage) = result.get("usage") {
            self.accumulated_usage.input_tokens += usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.accumulated_usage.output_tokens += usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.accumulated_usage.cached_read_tokens += usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.accumulated_usage.cached_write_tokens += usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        self.accumulated_models = quota::add(&self.accumulated_models, &model_increment);

        // A cancelled turn ends at the result of the work the interrupt stopped:
        // its own result, whose outcome is dropped. Claude Code ends the stopped
        // work first and then goes on with its queue, so the idle can come much
        // later (after a follow-up); the idle, the next echo's hand-off and the
        // force-cancel backstop settle a turn whose result never comes
        // (`acp-agent.ts:7060-7083`).
        if self.cancelled {
            if self.active.as_ref().is_some_and(|t| !t.settled) {
                let (usage, rows) = self.cancelled_outcome_usage();
                self.settle_with_usage(StopReason::Cancelled, usage, rows, &mut events);
            }
            return events;
        }

        // A cancel can race startup before the original prompt's echo. The CLI
        // then replays the cancelled prompt, its interruption marker and the
        // replacement prompt before yielding this error-shaped diagnostic for the
        // OLD cycle. The replacement's echo has already made it active, so
        // treating the diagnostic as its result would reject a healthy prompt.
        // Ignore only the diagnostic's exact empty-user-interruption signature;
        // ordinary provider errors still follow the failure lanes below
        // (`acp-agent.ts:7177-7209`, R72-2).
        if is_error
            && is_empty_user_interruption_diagnostic(result)
            && !self.empty_interruption_commands.is_empty()
        {
            match result.get("user_message_uuid").and_then(Value::as_str) {
                // Older producer: no join key, consume an arbitrary hand-off token.
                None => {
                    self.empty_interruption_commands.remove(0);
                    return events;
                }
                // Exact join: the diagnostic names a cancelled command we handed
                // a token for — swallow it. A stamped but unmatched diagnostic
                // belongs to a send we did NOT cancel (a live turn's real
                // failure): fall through to the ordinary failure lanes.
                Some(stamp) => {
                    if self.empty_interruption_commands.iter().any(|u| u == stamp) {
                        self.remove_empty_interruption(stamp);
                        return events;
                    }
                }
            }
        }
        // Some CLI versions omit the cancelled cycle's diagnostic. Do not let an
        // unused hand-off token swallow a later turn's real interruption failure.
        self.empty_interruption_commands.clear();

        // "No, keep planning" interrupts the turn: Claude represents the
        // intentional stop as an error-shaped diagnostic, which ends the turn
        // `cancelled` (`acp-agent.ts:7216-7238`).
        if let Some(verdict) = exit_plan_interruption {
            match verdict {
                ExitPlanVerdict::KeepPlanning => {
                    self.settle_or_defer(StopReason::Cancelled, &mut events)
                }
                ExitPlanVerdict::ContextReset => {
                    self.restart_turn = self.take_active_for_restart();
                }
            }
            return events;
        }

        // A refusal can arrive on any result subtype (and may set is_error) —
        // handle it before the subtype switch (`ADP:2736-2753`).
        if stop_reason == "refusal" {
            // The refused message carries no visible content: surface the
            // classifier's explanation (when available) before the turn ends
            // (`acp-agent.ts:7254-7263`).
            if let Some(text) = self.refusal_explanation.clone().filter(|t| !t.is_empty()) {
                events.push(TurnEvent::RefusalText { text });
            }
            self.settle_or_defer(StopReason::Refusal, &mut events);
            return events;
        }

        // R4: an error-shaped result on a steered turn with a pending echo is the
        // interrupted cycle's diagnostic, not the steered command's outcome —
        // keep it on the steer lane and let the cycle after the pending echo
        // decide the turn (`acp-agent.js:2756-2764`).
        if is_error
            && self.active.as_ref().is_some_and(|t| {
                is_steering_turn(t) && t.steered_echoes.as_ref().is_some_and(|s| !s.is_empty())
            })
        {
            self.settle_or_defer(StopReason::EndTurn, &mut events);
            return events;
        }

        // The subtype table, mirroring `ADP:2771-2848` case by case (review 04
        // #6): `max_tokens` is checked before `is_error` for success /
        // error_during_execution only; /login applies to an error success result;
        // the error_max_* subtypes fail on is_error with their
        // category, else map to max_turn_requests.
        match subtype {
            "success" => {
                // B-119: only an error result is the CLI's sign-in prompt
                // (`message.is_error && message.result.includes("Please run
                // /login")`, `acp-agent.ts:7310`); a plain success that
                // mentions the command is an ordinary answer.
                if is_error && result_text.contains("Please run /login") {
                    self.fail_active(
                        FailureKind::AuthRequired,
                        result_text.to_string(),
                        false,
                        &mut events,
                    );
                    return events;
                }
                if stop_reason == "max_tokens" {
                    self.settle_or_defer(StopReason::MaxTokens, &mut events);
                    return events;
                }
                if is_error {
                    self.fail_active(
                        FailureKind::ProviderError,
                        result_text.to_string(),
                        false,
                        &mut events,
                    );
                    return events;
                }
                // #453 result-text fallback: forward the result text when no
                // assistant text was delivered and the turn produced no output
                // tokens (the cache-replay signature), or for a local-only
                // command. Only the success arm and only after the max_tokens
                // break (`ADP:2804-2809`; review 04 #13).
                let output_tokens = result
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let is_local = self
                    .active
                    .as_ref()
                    .map(Turn::is_local_only_command)
                    .unwrap_or(false);
                // A notice is not an answer: a result that only repeats the
                // informational text shown for this turn is not forwarded again.
                let repeats_notice = self.active.as_ref().is_some_and(|turn| {
                    turn.notice_texts
                        .contains(&crate::notices::normalize_notice_text(result_text))
                });
                if (is_local
                    || (!assistant_text_delivered && output_tokens == 0 && !repeats_notice))
                    && !result_text.is_empty()
                {
                    events.push(TurnEvent::FinalText {
                        text: result_text.to_string(),
                    });
                }
                self.settle_or_defer(StopReason::EndTurn, &mut events);
            }
            "error_during_execution" => {
                if stop_reason == "max_tokens" {
                    self.settle_or_defer(StopReason::MaxTokens, &mut events);
                    return events;
                }
                if is_error {
                    self.fail_active(
                        FailureKind::ProviderError,
                        error_message(result, subtype),
                        false,
                        &mut events,
                    );
                    return events;
                }
                self.settle_or_defer(StopReason::EndTurn, &mut events);
            }
            "error_max_budget_usd" => {
                if is_error {
                    self.fail_active(
                        FailureKind::BudgetExhausted,
                        error_message(result, subtype),
                        false,
                        &mut events,
                    );
                    return events;
                }
                self.settle_or_defer(StopReason::MaxTurnRequests, &mut events);
            }
            "error_max_turns" => {
                if is_error {
                    self.fail_active(
                        FailureKind::ContextExhausted,
                        error_message(result, subtype),
                        false,
                        &mut events,
                    );
                    return events;
                }
                self.settle_or_defer(StopReason::MaxTurnRequests, &mut events);
            }
            "error_max_structured_output_retries" => {
                if is_error {
                    self.fail_active(
                        FailureKind::ProviderError,
                        error_message(result, subtype),
                        true,
                        &mut events,
                    );
                    return events;
                }
                self.settle_or_defer(StopReason::MaxTurnRequests, &mut events);
            }
            // Unknown subtypes are unreachable upstream (`unreachable()`); be
            // safe and end the turn rather than panic (D8).
            _ => {
                self.settle_or_defer(StopReason::EndTurn, &mut events);
            }
        }
        events
    }

    /// A `session_state_changed` frame arrives: record the session state. The
    /// actor routes `idle` to [`Self::on_idle`]; non-idle states are recorded so
    /// `cancel()`'s held-turn trailer-debt rule (`ADP:3571`) sees a non-idle
    /// state instead of a stale `"idle"` (review-05 row 2).
    ///
    /// A transition INTO `running` (a repeated `running` is not a transition)
    /// proves the idle period before it ended: every idle the CLI was going to
    /// emit for earlier results has been emitted, so debt still outstanding can
    /// never be paid and is swept (`ADP:6088-6110`). CLI 2.1.270+ withholds
    /// `idle` while background agents run, so a held turn's own result leaves one
    /// unpaid unit per hold; left in place each would absorb a later un-owed
    /// idle and mask an issue-#825 detection.
    pub fn on_session_state(&mut self, state: &str) {
        let previous = std::mem::replace(&mut self.last_session_state, state.to_string());
        if state == "running" && previous != "running" {
            self.owed_trailing_idles = 0;
        }
    }

    /// A `system`/`status` frame. A model request that serves none of the prompts
    /// a cancel stopped is new work, such as Claude Code's follow-up after a
    /// background command finished. It comes after the stopped work's result:
    /// Claude Code ends the interrupted turn first, then runs any prompt it had
    /// already taken off its queue (its request names it, so the cancel still
    /// drops it), then its own follow-ups, whose requests name no prompt. That
    /// request reopens the window (`acp-agent.ts:6002-6021`). Returns `true` when
    /// the cancel window was reopened, so the actor ends its stopped-work window.
    pub fn on_status(&mut self, status: &Value) -> bool {
        let requesting = status.get("status").and_then(Value::as_str) == Some("requesting");
        let mut reopened = false;
        if requesting
            && self.cancelled
            && self.request_marker_lists_v1
            && !self.serves_cancelled_work(status)
        {
            self.cancelled = false;
            self.cancelled_prompts.clear();
            reopened = true;
        }
        if requesting {
            self.model_request_since_result = true;
        }
        reopened
    }

    /// Whether the model request that `status` starts may belong to the work a
    /// cancel stopped (`servesCancelledWork`, `acp-agent.ts:11896-11925`). It may
    /// while a cancelled prompt is unsettled (the stopped turn, whose result is
    /// still to come); while the orphan accounting still expects the result of an
    /// abandoned prompt Claude Code may still run (which it can fold into its own
    /// follow-up after that follow-up's first request); while the request names a
    /// cancelled prompt (even when the interrupt receipt left it out of the
    /// accounting); or while it names an unsettled prompt. A prompt that is merely
    /// queued does not make another request belong to the stopped work.
    fn serves_cancelled_work(&self, status: &Value) -> bool {
        // Only reached while `cancelled`, so `cancel()` defined the set (an empty
        // set still honours the orphan clause below, as in JS).
        let unsettled = || {
            self.active
                .iter()
                .chain(self.queue.iter())
                .filter(|t| !t.settled)
        };
        if unsettled().any(|t| self.cancelled_prompts.contains(&t.prompt_uuid)) {
            return true;
        }
        if self.pending_orphan_results > 0 || !self.orphan_commands.is_empty() {
            return true;
        }
        status
            .get("user_message_uuids")
            .and_then(Value::as_array)
            .is_some_and(|served| {
                served.iter().filter_map(Value::as_str).any(|uuid| {
                    self.cancelled_prompts.contains(uuid)
                        || unsettled().any(|t| t.prompt_uuid == uuid)
                })
            })
    }

    /// A `session_state_changed: idle` frame — the SDK's authoritative turn-over
    /// signal. Absorbs owed trailing idles before failing, in the order
    /// `ADP:2081-2155` (review 04 #3): cancelled → settle; held → absorb debt +
    /// drain; debt > 0 → absorb; else the #825 fail.
    pub fn on_idle(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        self.last_session_state = "idle".to_string();
        // A model cycle that Claude Code started on its own normally ends at its
        // result. An idle that no earlier result owes means that result will not
        // come (the stream dropped mid-cycle), so the cycle ends here. An owed idle
        // belongs to an earlier result: it can lag past the cycle's first output
        // (`acp-agent.ts:6110-6125`).
        if self.owed_trailing_idles == 0 && std::mem::take(&mut self.autonomous_cycle) {
            events.push(TurnEvent::CycleEnded);
        }
        if self.cancelled && self.active.as_ref().is_some_and(|t| !t.settled) {
            // A cancelled turn settles at idle (its result was dropped). A held
            // turn's result passed long ago; this idle is the trailer of the
            // followup cycle the cancel interrupted, whose (autonomous) result
            // recorded it as owed (`ADP:6171-6174`).
            if self.active.as_ref().is_some_and(is_held_turn) && self.owed_trailing_idles > 0 {
                self.owed_trailing_idles -= 1;
            }
            // A held turn keeps the usage its own result recorded (#844).
            let (usage, rows) = self.cancelled_outcome_usage();
            self.settle_with_usage(StopReason::Cancelled, usage, rows, &mut events);
            return events;
        }
        if self.active.as_ref().is_some_and(is_held_turn) {
            // A held turn absorbs one outstanding trailer debt, then drains once
            // none of its subagents is left — the fallback when no followup
            // result came (`ADP:2090-2107`). Must NOT fall through to #825.
            if self.owed_trailing_idles > 0 {
                self.owed_trailing_idles -= 1;
            }
            self.settle_deferred_if_drained(&mut events);
            return events;
        }
        if self.active.as_ref().is_some_and(is_steered_settle_ready) {
            // R6: a steered turn whose steered work produced its result settles
            // at the next idle — the only signal spanning the interrupted and
            // steered cycles (`acp-agent.ts:6188-6226`). AHEAD of owed-idle debt:
            // the outcome is final, so whichever idle this is settles the turn,
            // while debt absorption could swallow the only idle it gets (a steer
            // that aborted an autonomous followup collapses its counted trailer
            // into this one idle). The debt stays standing: a genuinely separate
            // trailer still absorbs the idle it belongs to, and one that never
            // comes is swept at the next `running` transition.
            //
            // Via the subagent gate, not settle_active: a steered turn can also
            // have spawned background subagents, which own it from here (settles
            // now if none is live, holds otherwise).
            if let Some(active) = self.active.as_mut() {
                active.deferred_settle = active.steered_settle.take();
                active.steered_echoes = None;
                active.steered_awaiting_result = false;
            }
            self.settle_deferred_if_drained(&mut events);
            return events;
        }
        if self.owed_trailing_idles > 0 {
            // A settled/autonomous turn's lagging trailing idle — absorb it
            // (`ADP:2108-2118`), never read it as the active turn abandoned.
            // Deliberately BEFORE the not-ready steer lane below: an owed idle
            // belongs to an earlier turn and must not reach the #825 fail.
            self.owed_trailing_idles -= 1;
            return events;
        }
        if self.active.as_ref().is_some_and(is_steering_turn) {
            // An idle before the steered echo, or before a result since it,
            // means the answer is still ahead — swallow it, and never let it
            // reach the #825 fail below (INV-31).
            return events;
        }
        if !self.cancelled && self.active.as_ref().is_some_and(|t| !t.settled) {
            // #825: idle without a result — the model stream dropped mid-turn.
            self.fail_active(
                FailureKind::NoResult,
                TURN_NO_RESULT_MESSAGE.to_string(),
                false,
                &mut events,
            );
        }
        events
    }

    /// The stream ended (EOF / codec death). Settle the active turn —
    /// `cancelled` if a cancel is pending, otherwise its deferred outcome, else
    /// the scratch end_turn — and reject every queued turn with
    /// `SESSION_ENDED_MESSAGE` (`ADP:1771-1820`; review 04 #10).
    pub fn on_stream_end(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        self.autonomous_cycle = false;
        if let Some(active) = self.active.take() {
            if !active.settled {
                if active.deferred_settle.is_some() {
                    self.text_stretch_closed = true;
                }
                let (outcome, usage, model_rows) = if self.cancelled {
                    (
                        StopReason::Cancelled,
                        self.accumulated_usage,
                        self.models_snapshot(),
                    )
                } else if let Some(deferred) = active.deferred_settle {
                    // A held turn's result already recorded its outcome and usage
                    // snapshot — the hold settles with those (`ADP:3059`).
                    (deferred.stop_reason, deferred.usage, deferred.models)
                } else {
                    (
                        StopReason::EndTurn,
                        self.accumulated_usage,
                        self.models_snapshot(),
                    )
                };
                let prompt_uuid = active.prompt_uuid.clone();
                events.push(TurnEvent::Settled {
                    prompt_uuid,
                    stop_reason: outcome,
                    usage,
                    had_usage: true,
                    model_rows,
                });
            }
        }
        for turn in self.queue.drain(..) {
            if !turn.settled {
                events.push(TurnEvent::Failed {
                    prompt_uuid: turn.prompt_uuid,
                    kind: FailureKind::SessionEnded,
                    message: SESSION_ENDED_MESSAGE.to_string(),
                    usage: self.accumulated_usage,
                    model_rows: SharedRows::default(),
                    is_structured_output_retries: false,
                });
            }
        }
        events
    }

    /// Reject every in-flight turn with `kind` — a held active turn resolves
    /// with its deferred outcome instead, mirroring `failAllTurns`
    /// (`ADP:1658-1682`; review 04 #10).
    pub fn fail_all(&mut self, kind: FailureKind) -> Vec<TurnEvent> {
        self.fail_all_with(kind, kind.default_message())
    }

    /// [`Self::fail_all`] with the error every rejected turn carries: the stream
    /// died, and `failAllTurns(error)` rejects with the error the SDK threw
    /// (`acp-agent.ts:5602-5650`, `8240-8330`).
    pub fn fail_all_with(&mut self, kind: FailureKind, message: &str) -> Vec<TurnEvent> {
        self.fail_all_split(kind, kind, message)
    }

    /// [`Self::fail_all_with`] where the active turn fails with `active_kind`
    /// and the queued ones with `queued_kind`: an AIR client's active turn ends
    /// with a `transport_lost` failure on its response while the queued turns
    /// are rejected (`acp-agent.ts:8275-8288`).
    pub fn fail_all_split(
        &mut self,
        active_kind: FailureKind,
        queued_kind: FailureKind,
        message: &str,
    ) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        self.autonomous_cycle = false;
        if let Some(active) = self.active.take() {
            if !active.settled {
                let prompt_uuid = active.prompt_uuid.clone();
                if let Some(deferred) = active.deferred_settle {
                    let usage = deferred.usage;
                    events.push(TurnEvent::Settled {
                        prompt_uuid,
                        stop_reason: deferred.stop_reason,
                        usage,
                        had_usage: true,
                        model_rows: deferred.models,
                    });
                } else if self.cancelled {
                    // After a cancel the abort it started can kill the stream, and
                    // that error is the cancel's, not the work's: the active turn
                    // ends `cancelled` (`failAllTurns`, `acp-agent.ts:5625-5626`).
                    events.push(TurnEvent::Settled {
                        prompt_uuid,
                        stop_reason: StopReason::Cancelled,
                        usage: self.accumulated_usage,
                        had_usage: true,
                        model_rows: self.models_snapshot(),
                    });
                } else {
                    events.push(TurnEvent::Failed {
                        prompt_uuid,
                        kind: active_kind,
                        message: message.to_string(),
                        usage: self.accumulated_usage,
                        model_rows: self.models_snapshot(),
                        is_structured_output_retries: false,
                    });
                }
            }
        }
        for turn in self.queue.drain(..) {
            if !turn.settled {
                events.push(TurnEvent::Failed {
                    prompt_uuid: turn.prompt_uuid,
                    kind: queued_kind,
                    message: message.to_string(),
                    usage: self.accumulated_usage,
                    model_rows: SharedRows::default(),
                    is_structured_output_retries: false,
                });
            }
        }
        events
    }

    /// Ensure there is an active turn before a user-turn result that carries no
    /// echo to activate it (`ADP:1408-1489`; review 04 #2). Order: (a) a held
    /// active turn settles with its deferred outcome and falls through; (b)
    /// orphan accounting runs BEFORE the head check; (c) promote the queue head.
    ///
    /// `result` is read for its join key (`user_message_uuid` / `user_message_uuids`,
    /// SDK 0.3.246+ stamps the triggering send's client uuid on results; absent on
    /// older CLIs, synthetic/meta turns and session-scoped failures). When present
    /// it upgrades the orphan-map lane from positional heuristics to an exact
    /// match: a stamp naming an orphaned command consumes the result outright, and
    /// a stamp naming anything else positively refutes "this is a dead turn's
    /// result", so the dup-over-loss one-skip must not eat it.
    fn ensure_active_turn(&mut self, result: &Value, events: &mut Vec<TurnEvent>) {
        // ADP:1408-1412 — if there is already an active turn, only a HELD one is
        // settled here (and falls through to promote the next head); any other
        // active turn is the one this result belongs to, so return immediately
        // (review-05 row 1: without this, a normal echoed turn's own result
        // would settle it and promote the queued head, or consume an orphan
        // credit that belongs to a swept turn).
        if let Some(active) = self.active.as_ref() {
            if !is_held_turn(active) {
                return;
            }
            if let Some(deferred) = active.deferred_settle.clone() {
                self.settle_with_usage(
                    deferred.stop_reason,
                    deferred.usage,
                    deferred.models,
                    events,
                );
            }
        }
        if self.pending_orphan_results > 0 {
            self.pending_orphan_results -= 1;
            return;
        }
        // msg_lifecycle_v1 lane. Turns run sequentially and frames arrive in
        // stream order, so at any result: every "zombie" is from an already-dead
        // turn whose own result already passed (or never existed), every
        // "started" entry was dispatched into THE turn that emitted this result,
        // and a "pending" entry was not dispatched before it. One result
        // therefore covers ALL started and zombie entries at once (N coalesced
        // commands share ONE result); their outstanding terminal frames then
        // no-op on the missing entries. (This ordering argument is asserted from
        // observed CLI behaviour, not a documented wire contract.)
        let stamp = result.get("user_message_uuid").and_then(Value::as_str);
        if !self.orphan_commands.is_empty() {
            let stamped_orphan = stamp.is_some_and(|s| self.orphan_state(s).is_some());
            let mut consumed = false;
            let mut oldest_pending: Option<String> = None;
            // The started/zombie drain applies regardless of the stamp.
            self.orphan_commands.retain(|(uuid, state)| match state {
                OrphanState::Started | OrphanState::Zombie => {
                    consumed = true;
                    false
                }
                OrphanState::Pending => {
                    oldest_pending.get_or_insert_with(|| uuid.clone());
                    true
                }
            });
            if stamped_orphan {
                // Exact join: delete the matched entry even when it is still
                // "pending" (its dispatch frame was lost) and consume the result.
                if let Some(stamp) = stamp {
                    self.remove_orphan(stamp);
                }
                return;
            }
            // A stamp naming a send that is NOT in the map is a live turn's
            // result: skip the consumed-return and the dup-over-loss one-skip
            // and fall through to promote the head.
            if stamp.is_none() {
                if consumed {
                    return;
                }
                if let Some(oldest) = oldest_pending {
                    // No dispatch was seen before this result, so it is very
                    // likely a live turn's — but a lost "started" frame would
                    // mean it IS the orphan's (dup-over-loss): grant each
                    // pending entry exactly one skip, like the count lane did.
                    self.remove_orphan(&oldest);
                    return;
                }
            }
        }
        // A result that names a prompt a cancel stopped is that prompt's, even
        // when the orphan accounting above lost track of it: the interrupt
        // receipt leaves out a prompt that Claude Code had already taken off its
        // queue. It must not settle the prompt sent after the cancel.
        if result_stamps(result)
            .iter()
            .any(|uuid| self.cancelled_prompts.contains(*uuid))
        {
            return;
        }
        let idx = self.queue.iter().position(|t| !t.settled);
        if let Some(idx) = idx {
            // `idx` came from `position` above, so the removal cannot fail;
            // handle the `None` gracefully rather than panic (D8).
            if let Some(queued) = self.queue.remove(idx) {
                self.activate(queued, events);
            }
        }
    }

    /// The usage a cancel reports for the active turn (`cancelledOutcome`,
    /// `acp-agent.ts:11886-11894`): a turn that already has its result (held for
    /// its subagents) keeps the usage that result recorded (issue #844); any
    /// other reports the live accumulator.
    fn cancelled_outcome_usage(&self) -> (Usage, SharedRows) {
        match self
            .active
            .as_ref()
            .and_then(|t| t.deferred_settle.as_ref())
        {
            Some(d) => (d.usage, d.models.clone()),
            None => (self.accumulated_usage, self.models_snapshot()),
        }
    }

    /// Result-time bookkeeping that must run whether or not the result can be
    /// attributed to a turn (`recordResultForOrphanCommands`,
    /// `acp-agent.ts:5004-5018`). (1) Latch `command_result_seen` on every queued
    /// turn whose command is known dispatched with no terminal frame yet — the
    /// emitting turn is the one it was dispatched (possibly folded) into, so its
    /// result has now passed; a later cancel() must not seed an orphan entry that
    /// waits for it. (2) When a turn is ACTIVE the result is attributed to it and
    /// never reaches `ensure_active_turn`, but it still covers the map's started
    /// entries (folded into the active turn) and zombies, so drain them here or
    /// they would linger and swallow a later live echo-less result.
    fn record_result_for_orphan_commands(&mut self) {
        for turn in self.active.iter_mut().chain(self.queue.iter_mut()) {
            if !turn.settled && turn.command_started && turn.command_finished.is_none() {
                turn.command_result_seen = true;
            }
        }
        if self.active.is_some() {
            self.orphan_commands
                .retain(|(_, state)| *state == OrphanState::Pending);
        }
    }

    /// Whether the active turn is held open for live subagents (has a stored
    /// outcome and is not settled) — the `isHeldOpen` of `acp-agent.js:125`.
    fn is_held(&self) -> bool {
        self.active.as_ref().is_some_and(is_held_turn)
    }

    /// Whether any subagent this turn spawned is still live — while true the
    /// turn's settlement stays deferred (`turnAwaitingSubagents`).
    fn turn_awaiting_subagents(&self, turn: &Turn) -> bool {
        // A subagent the level signal ended is kept for attribution only: a hold
        // must not wait on it (`record.isSubagent && !record.endedPerLevel`).
        turn.spawned_task_ids.iter().any(|id| {
            self.live_subagents.get(id).copied().unwrap_or(false)
                && self
                    .task_meta
                    .get(id)
                    .is_none_or(|meta| meta.ended_per_level.is_none())
        })
    }

    /// Settle the active turn's stored deferred outcome once none of its
    /// subagents is live — the single drain rule (`settleDeferredIfDrained`).
    fn settle_deferred_if_drained(&mut self, events: &mut Vec<TurnEvent>) {
        if !self.is_held() {
            return;
        }
        let awaiting = self
            .active
            .as_ref()
            .is_some_and(|t| self.turn_awaiting_subagents(t));
        if awaiting {
            return;
        }
        let deferred = self.active.as_ref().and_then(|t| t.deferred_settle.clone());
        if let Some(deferred) = deferred {
            self.settle_with_usage(
                deferred.stop_reason,
                deferred.usage,
                deferred.models,
                events,
            );
        }
    }

    /// Settle the active turn with `outcome` now — unless subagents it spawned
    /// are still live, in which case store the outcome and hold it open
    /// (`settleOrDefer`, `#866`).
    fn settle_or_defer(&mut self, outcome: StopReason, events: &mut Vec<TurnEvent>) {
        // R3: a steered turn's result never settles here — the steer aborted the
        // cycle this result may belong to, and the steered one is still to come.
        // Record the outcome for the steer idle lane; later results overwrite it,
        // so the last result and its usage win (`acp-agent.js:1560-1564`).
        if self.active.as_ref().is_some_and(is_steering_turn) {
            if let Some(active) = self.active.as_mut() {
                active.steered_settle = Some(DeferredSettle {
                    stop_reason: outcome,
                    usage: self.accumulated_usage,
                    models: Arc::new(self.accumulated_models.clone()),
                });
                active.steered_awaiting_result = false;
            }
            return;
        }
        let awaiting = self
            .active
            .as_ref()
            .is_some_and(|t| !t.settled && self.turn_awaiting_subagents(t));
        if !awaiting {
            self.settle(outcome, events);
            return;
        }
        if let Some(active) = self.active.as_mut() {
            if !active.settled {
                // Snapshot the outcome AND the usage accumulated so far; a held
                // turn settles with this snapshot, not the later accumulator.
                active.deferred_settle = Some(DeferredSettle {
                    stop_reason: outcome,
                    usage: self.accumulated_usage,
                    models: Arc::new(self.accumulated_models.clone()),
                });
            }
        }
    }

    /// `settleOrDefer({ ...heldTurn.deferredSettle, stopReason: "cancelled" })`:
    /// the held turn ends cancelled with the usage its own result recorded, or
    /// keeps holding with that outcome while other subagents it spawned live.
    fn settle_held_cancelled(&mut self, events: &mut Vec<TurnEvent>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let Some(deferred) = active.deferred_settle.clone() else {
            return;
        };
        if self.turn_awaiting_subagents(active) {
            if let Some(active) = self.active.as_mut() {
                active.deferred_settle = Some(DeferredSettle {
                    stop_reason: StopReason::Cancelled,
                    ..deferred
                });
            }
            return;
        }
        self.settle_with_usage(
            StopReason::Cancelled,
            deferred.usage,
            deferred.models,
            events,
        );
    }

    /// Settle the active turn exactly once and clear the active slot. Ports
    /// `settleActive` (`ADP:1579-1610`), including `activeTurn = null` (review
    /// 04 #5). Reports the live per-turn accumulator (`accumulatedUsage`).
    fn settle(&mut self, outcome: StopReason, events: &mut Vec<TurnEvent>) {
        self.settle_with_usage(
            outcome,
            self.accumulated_usage,
            self.models_snapshot(),
            events,
        );
    }

    /// Settle the active turn with an explicit usage snapshot — used by the
    /// held-settle lanes, which report the usage the turn's result recorded
    /// (`deferredSettle.usage`) rather than the live accumulator.
    fn settle_with_usage(
        &mut self,
        outcome: StopReason,
        usage: Usage,
        model_rows: SharedRows,
        events: &mut Vec<TurnEvent>,
    ) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.settled {
            return;
        }
        // Settling a held turn closes its delivery stretch: text streamed since
        // the last boundary was its follow-ups' (`settleActive`, `wasHeld`,
        // `acp-agent.ts:5495-5517`).
        if active.deferred_settle.is_some() {
            self.text_stretch_closed = true;
        }
        active.settled = true;
        let prompt_uuid = active.prompt_uuid.clone();
        self.active = None;
        events.push(TurnEvent::Settled {
            prompt_uuid,
            stop_reason: outcome,
            usage,
            had_usage: true,
            model_rows,
        });
    }

    /// Fail the active turn without tearing down the machine (`failActive`,
    /// `ADP:1611-1631`), clearing the active slot (review 04 #5).
    fn fail_active(
        &mut self,
        kind: FailureKind,
        message: String,
        is_structured_output_retries: bool,
        events: &mut Vec<TurnEvent>,
    ) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.settled {
            return;
        }
        // A failed turn's stretch is over, and the idle-fail lane never sees the
        // result whose `finally` would close it (`failActive`,
        // `acp-agent.ts:5536-5541`).
        self.text_stretch_closed = true;
        active.settled = true;
        let prompt_uuid = active.prompt_uuid.clone();
        self.active = None;
        events.push(TurnEvent::Failed {
            prompt_uuid,
            kind,
            message,
            usage: self.accumulated_usage,
            model_rows: self.models_snapshot(),
            is_structured_output_retries,
        });
    }

    /// Promote `queued` to active, resetting the per-turn accumulator and the
    /// cancelled / orphan-skip flags (`activateTurn`, `ADP:1362-1392`).
    fn activate(&mut self, mut queued: Turn, events: &mut Vec<TurnEvent>) {
        // A turn that Claude Code takes in takes a running cycle over: the work
        // goes on under the turn and only its end ends it (`startForeground`).
        self.autonomous_cycle = false;
        self.cancelled = false;
        self.cancelled_prompts.clear();
        self.pending_orphan_results = 0;
        self.orphan_commands.clear();
        // The scratch starts from what a restart carried over (`resetTurnScratch`,
        // `acp-agent.ts:4781-4791`), else from zero.
        let (carried_usage, carried_models) = queued.carried.take().unwrap_or_default();
        self.accumulated_usage = carried_usage;
        self.accumulated_models = carried_models;
        self.last_assistant_model = None;
        self.refusal_explanation = None;
        // Two-phase sweep of the entries the level signal ended. Local-only
        // commands do not advance the clock: they interact with no tasks, and two
        // quick `/context` calls would burn the whole grace period.
        if !queued.is_local_only_command {
            self.sweep_ended_tasks();
        }
        let prompt_uuid = queued.prompt_uuid.clone();
        self.active = Some(queued);
        events.push(TurnEvent::Activated { prompt_uuid });
    }
}

/// Whether Claude Code ended the work of `result` because an interrupt stopped
/// it, while the model streamed or while a tool ran (`isAbortedResult`).
fn is_aborted_result(result: &Value) -> bool {
    matches!(
        result.get("terminal_reason").and_then(Value::as_str),
        Some("aborted_streaming" | "aborted_tools")
    )
}

/// The prompt uuids a result names (`user_message_uuids`, else the single
/// `user_message_uuid`).
fn result_stamps(result: &Value) -> Vec<&str> {
    match result.get("user_message_uuids").and_then(Value::as_array) {
        Some(uuids) => uuids.iter().filter_map(Value::as_str).collect(),
        None => result
            .get("user_message_uuid")
            .and_then(Value::as_str)
            .into_iter()
            .collect(),
    }
}

/// Claude CLI emits this synthetic result when an interrupted cycle ends on
/// queued user input before producing any assistant content: a hand-off marker,
/// not the outcome of the replacement prompt (`isEmptyUserInterruptionDiagnostic`,
/// `acp-agent.ts:366-380`). Each `(?:^|\s)tok(?:\s|$)` probe is whitespace-token
/// membership.
fn is_empty_user_interruption_diagnostic(result: &Value) -> bool {
    let diagnostic = match result.get("result").and_then(Value::as_str) {
        Some(text) => Some(text),
        None => result
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| {
                errors
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|e| e.starts_with("[ede_diagnostic]"))
            }),
    };
    let Some(diagnostic) = diagnostic else {
        return false;
    };
    let has_token = |token: &str| diagnostic.split_whitespace().any(|t| t == token);
    diagnostic.starts_with("[ede_diagnostic]")
        && has_token("result_type=user")
        && has_token("last_content_type=n/a")
        && has_token("stop_reason=null")
}

/// Whether a result carries an autonomous origin (`AUTONOMOUS_RESULT_ORIGINS`).
fn is_autonomous_result(result: &Value) -> bool {
    let Some(origin) = result.get("origin") else {
        return false;
    };
    let Some(kind) = origin.get("kind").and_then(Value::as_str) else {
        return false;
    };
    AUTONOMOUS_RESULT_ORIGINS.contains(&kind)
}

/// Whether `turn` is held open (has a stored outcome, not yet settled).
fn is_held_turn(turn: &Turn) -> bool {
    turn.deferred_settle.is_some() && !turn.settled
}

/// Whether `turn` is steering — a `_session/steering` injected a `priority:"now"`
/// message and its echo is still pending (`isSteering`, `acp-agent.js:129-132`).
fn is_steering_turn(turn: &Turn) -> bool {
    turn.steered_echoes.is_some() && !turn.settled
}

/// Whether a steered turn's recorded outcome is its steered work's own: every
/// steered message has been picked up and a result arrived after the last one
/// (`isSteeredSettleReady`, `acp-agent.ts:1556-1566`), so the next idle settles it.
fn is_steered_settle_ready(turn: &Turn) -> bool {
    is_steering_turn(turn)
        && turn.steered_echoes.as_ref().is_some_and(|s| s.is_empty())
        && turn.steered_settle.is_some()
        && !turn.steered_awaiting_result
}

/// The error message for an `is_error` result — `message.errors.join(", ")`
/// or the subtype, per `ADP:2818`, `2826`, `2833`, `2840`.
fn error_message(result: &Value, subtype: &str) -> String {
    let joined = result
        .get("errors")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    if joined.is_empty() {
        subtype.to_string()
    } else {
        joined
    }
}

#[cfg(test)]
mod turn_tests;
#[cfg(test)]
mod turn_tests_t1a;
#[cfg(test)]
mod turn_tests_t1fa;
#[cfg(test)]
mod turn_tests_t2a;
#[cfg(test)]
mod turn_tests_t3a;
