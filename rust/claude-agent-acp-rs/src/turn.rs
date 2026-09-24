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
//! - **Steering** (`steeredEchoes`/`steeredSettle`, `_session/steering`):
//!   the idle lane treats every turn as un-steered; `isSteering` is always
//!   false. Phase that ports R32/steering must add the steer lanes.
//! - **`msg_lifecycle_v1` orphan map** (`session.orphanCommands`): orphan
//!   accounting uses only the coarse `pending_orphan_results` count, not the
//!   per-uuid `command_lifecycle` lane. Coalescing of N queued commands into
//!   one result can leave a stale count; the count self-heals at activation
//!   (`ADP:1366`), bounding the damage. The two-phase `endedPerLevel` registry
//!   sweep at activation (`ADP:1366-1390`) is likewise omitted: `live_subagents`
//!   entries are removed only by their terminal frames.
//!
//! All three are recorded in the plan under Decision D4 and `PARITY.md` as
//! pending, so their absence is intentional, not a silent drift.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeferredSettle {
    stop_reason: StopReason,
    usage: Usage,
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
    },
    /// A turn failed. The actor rejects that prompt's oneshot with the mapped
    /// JSON-RPC error.
    Failed {
        prompt_uuid: String,
        kind: FailureKind,
        message: String,
    },
    /// A turn was activated / promoted to the queue head.
    Activated { prompt_uuid: String },
    /// #453 result-text fallback: assistant text exists only in the `result`
    /// frame (no `stream_event` deltas, no consolidated `assistant` message),
    /// so the actor must forward it as its own message. Emission is phase 7.
    FinalText { text: String },
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
    /// The uuids of the turns swept by [`Self::cancel`]'s queue sweep — the
    /// orphaned turns whose late results may still arrive (upstream's
    /// `orphanedTurns`). Consumed by [`Self::reconcile_orphan_receipt`] to drop
    /// the orphan credits of turns the interrupt's receipt shows as dropped
    /// (`still_queued` absent), so a stale credit can't swallow a later
    /// echo-less result (INV-23, `acp-agent.js:3608-3648`).
    orphaned_uuids: Vec<String>,
    /// Number of trailing `idle` frames still owed by settled/autonomous/
    /// cancelled turns (`owedTrailingIdles`) — absorbed by `on_idle` so a
    /// lagging idle can't false-fail the next live turn (#825).
    owed_trailing_idles: usize,
    /// Whether the active turn was cancelled (its result is dropped; it settles
    /// `cancelled` at the next idle).
    cancelled: bool,
    /// The last `session_state_changed` state (`lastSessionState`), read by
    /// `cancel()`'s held-turn debt rule (`ADP:3571`).
    last_session_state: String,
    /// Per-active-turn token accumulator (`accumulatedUsage`), reset at
    /// activation and snapshot into every [`TurnEvent::Settled`].
    accumulated_usage: Usage,
}

impl TurnMachine {
    /// A fresh machine with no turns.
    pub fn new() -> Self {
        Self::default()
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

    /// Enqueue a prompt turn (it awaits its echo before activation).
    pub fn enqueue(&mut self, turn: Turn) {
        self.queue.push_back(turn);
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

        // Sweep queued turns that haven't started yet (no echo): settle them
        // now and seed an orphan per turn so their late results are skipped,
        // not attributed to the next head (`ADP:3471-3526`).
        let mut remaining = VecDeque::new();
        for turn in self.queue.drain(..) {
            if turn.settled {
                remaining.push_back(turn);
            } else {
                self.pending_orphan_results += 1;
                self.orphaned_uuids.push(turn.prompt_uuid.clone());
                events.push(TurnEvent::Settled {
                    prompt_uuid: turn.prompt_uuid,
                    stop_reason: StopReason::Cancelled,
                    usage: Usage::default(),
                    had_usage: false,
                });
            }
        }
        self.queue = remaining;

        // Inline-settle a held active turn `cancelled` — during the hold the
        // session is usually already idle, so the interrupt may produce no
        // fresh idle for the cancelled-settle path to run on. Pre-count the
        // interrupt's trailer debt unless the session already sits idle
        // (`ADP:3528-3575`).
        if self.active.as_ref().is_some_and(is_held_turn) {
            if self.last_session_state != "idle" {
                self.owed_trailing_idles += 1;
            }
            if let Some(active) = self.active.take() {
                if !active.settled {
                    let prompt_uuid = active.prompt_uuid.clone();
                    // A held turn's cancel reports the usage its result already
                    // recorded (`deferredSettle.usage`), not the live accumulator
                    // (`ADP:3574`; review-05 minor).
                    let usage = active
                        .deferred_settle
                        .map(|d| d.usage)
                        .unwrap_or(self.accumulated_usage);
                    events.push(TurnEvent::Settled {
                        prompt_uuid,
                        stop_reason: StopReason::Cancelled,
                        usage,
                        had_usage: true,
                    });
                }
            }
        }
        events
    }

    /// Force-cancel the active turn and clear the active slot, seeding one
    /// orphan credit for its late `result` (INV-30). This is the **force-cancel
    /// floor** lane (phase 11, `ADP:1719-1752`), NOT `cancel()` — keep that
    /// name for the review's `cancel()`. `abandon_active` is retained as this
    /// method for phase 11.
    pub fn force_cancel(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        if let Some(active) = self.active.as_mut() {
            if !active.settled {
                active.settled = true;
                self.pending_orphan_results += 1;
                events.push(TurnEvent::Settled {
                    prompt_uuid: active.prompt_uuid.clone(),
                    stop_reason: StopReason::Cancelled,
                    usage: self.accumulated_usage,
                    had_usage: true,
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
    /// This is the **legacy count lane** — the Rust port does not carry the
    /// `msg_lifecycle_v1` per-uuid `orphanCommands` map (deferred, review 04
    /// #14), so reconciliation subtracts a count, not uuids.
    ///
    /// Field guard (11.4): `still_queued` is `None` for a bare `{}` success
    /// receipt (or a CLI that resolves `undefined`), which must NOT read as
    /// "everything was dropped" — count-everything behaviour is kept and the
    /// activation-time self-heal bounds the damage.
    pub fn reconcile_orphan_receipt(&mut self, still_queued: Option<&[String]>) {
        // Field guard: guard the FIELD, not just the receipt — a bare `{}`
        // (no `still_queued` array) falls back to count-everything.
        let Some(still_queued) = still_queued else {
            return;
        };
        if self.orphaned_uuids.is_empty() {
            return;
        }
        let dropped = self
            .orphaned_uuids
            .iter()
            .filter(|uuid| !still_queued.contains(uuid))
            .count();
        if dropped > 0 {
            self.pending_orphan_results = self.pending_orphan_results.saturating_sub(dropped);
        }
        self.orphaned_uuids.clear();
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
                let (outcome, usage) = if self.cancelled {
                    // A cancel is pending for the previous turn (its trailing
                    // idle hasn't arrived yet): settle it `cancelled` and record
                    // the interrupt's trailer debt so that lagged idle is
                    // absorbed, not read as the freshly-activated turn ending
                    // (`ADP:3034-3050`).
                    self.owed_trailing_idles += 1;
                    (StopReason::Cancelled, self.accumulated_usage)
                } else if let Some(deferred) = prev.deferred_settle {
                    // A held turn hands off with its recorded outcome and usage
                    // snapshot, not a guessed end_turn (`ADP:3051-3060`).
                    (deferred.stop_reason, deferred.usage)
                } else {
                    (StopReason::EndTurn, self.accumulated_usage)
                };
                let mut prev = prev;
                prev.settled = true;
                let prompt_uuid = prev.prompt_uuid.clone();
                events.push(TurnEvent::Settled {
                    prompt_uuid,
                    stop_reason: outcome,
                    usage,
                    had_usage: true,
                });
            }
        }
        self.activate(queued, &mut events);
        events
    }

    /// A `result` frame for the user's turn. Ports the full `ADP:2532-2884`
    /// lane: the autonomous-origin gate, `ensureActiveTurn`, owed-trailing-idle
    /// debt, usage accumulation, the stop-reason subtype table and the #453
    /// result-text fallback.
    ///
    /// `assistant_text_delivered` tells whether any assistant text reached the
    /// client before this result (the actor tracks `emittedAssistantText`).
    pub fn on_result(&mut self, result: &Value, assistant_text_delivered: bool) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        let is_autonomous = is_autonomous_result(result);
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
        if is_autonomous {
            self.owed_trailing_idles += 1;
            self.settle_deferred_if_drained(&mut events);
            return events;
        }

        // A user-turn result needs an active turn: promote the queue head
        // (echo-less local-only commands / compaction), settling any held turn
        // first (`ADP:1408-1489`; review 04 #2).
        self.ensure_active_turn(&mut events);

        // Every user-turn result terminates a turn and is followed by a
        // trailing idle — record the debt so that idle is absorbed, not read as
        // the next turn being abandoned (#825). The one exclusion: the cancelled
        // ACTIVE turn's own result, which is dropped below and settles at idle
        // (or the echo hand-off) instead (`ADP:2622-2626`).
        if !(self.cancelled && self.active.is_some()) {
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

        // A cancelled turn's result is dropped; it settles at idle.
        if self.cancelled {
            return events;
        }

        // A refusal can arrive on any result subtype (and may set is_error) —
        // handle it before the subtype switch (`ADP:2736-2753`).
        if stop_reason == "refusal" {
            self.settle_or_defer(StopReason::Refusal, &mut events);
            return events;
        }

        // The subtype table, mirroring `ADP:2771-2848` case by case (review 04
        // #6): `max_tokens` is checked before `is_error` for success /
        // error_during_execution only; /login applies to success regardless of
        // is_error; the error_max_* subtypes fail on is_error with their
        // category, else map to max_turn_requests.
        match subtype {
            "success" => {
                if result_text.contains("Please run /login") {
                    self.fail_active(
                        FailureKind::AuthRequired,
                        result_text.to_string(),
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
                if (is_local || (!assistant_text_delivered && output_tokens == 0))
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
    pub fn on_session_state(&mut self, state: &str) {
        self.last_session_state = state.to_string();
    }

    /// A `session_state_changed: idle` frame — the SDK's authoritative turn-over
    /// signal. Absorbs owed trailing idles before failing, in the order
    /// `ADP:2081-2155` (review 04 #3): cancelled → settle; held → absorb debt +
    /// drain; debt > 0 → absorb; else the #825 fail.
    pub fn on_idle(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        self.last_session_state = "idle".to_string();
        if self.cancelled && self.active.as_ref().is_some_and(|t| !t.settled) {
            // A cancelled turn settles at idle (its result was dropped).
            self.settle(StopReason::Cancelled, &mut events);
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
        if self.owed_trailing_idles > 0 {
            // A settled/autonomous turn's lagging trailing idle — absorb it
            // (`ADP:2108-2118`), never read it as the active turn abandoned.
            self.owed_trailing_idles -= 1;
            return events;
        }
        if !self.cancelled && self.active.as_ref().is_some_and(|t| !t.settled) {
            // #825: idle without a result — the model stream dropped mid-turn.
            self.fail_active(
                FailureKind::NoResult,
                TURN_NO_RESULT_MESSAGE.to_string(),
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
        if let Some(active) = self.active.take() {
            if !active.settled {
                let (outcome, usage) = if self.cancelled {
                    (StopReason::Cancelled, self.accumulated_usage)
                } else if let Some(deferred) = active.deferred_settle {
                    // A held turn's result already recorded its outcome and usage
                    // snapshot — the hold settles with those (`ADP:3059`).
                    (deferred.stop_reason, deferred.usage)
                } else {
                    (StopReason::EndTurn, self.accumulated_usage)
                };
                let prompt_uuid = active.prompt_uuid.clone();
                events.push(TurnEvent::Settled {
                    prompt_uuid,
                    stop_reason: outcome,
                    usage,
                    had_usage: true,
                });
            }
        }
        for turn in self.queue.drain(..) {
            if !turn.settled {
                events.push(TurnEvent::Failed {
                    prompt_uuid: turn.prompt_uuid,
                    kind: FailureKind::SessionEnded,
                    message: SESSION_ENDED_MESSAGE.to_string(),
                });
            }
        }
        events
    }

    /// Reject every in-flight turn with `kind` — a held active turn resolves
    /// with its deferred outcome instead, mirroring `failAllTurns`
    /// (`ADP:1658-1682`; review 04 #10).
    pub fn fail_all(&mut self, kind: FailureKind) -> Vec<TurnEvent> {
        let mut events = Vec::new();
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
                    });
                } else {
                    events.push(TurnEvent::Failed {
                        prompt_uuid,
                        kind,
                        message: kind.default_message().to_string(),
                    });
                }
            }
        }
        for turn in self.queue.drain(..) {
            if !turn.settled {
                events.push(TurnEvent::Failed {
                    prompt_uuid: turn.prompt_uuid,
                    kind,
                    message: kind.default_message().to_string(),
                });
            }
        }
        events
    }

    /// Ensure there is an active turn before a user-turn result that carries no
    /// echo to activate it (`ADP:1408-1489`; review 04 #2). Order: (a) a held
    /// active turn settles with its deferred outcome and falls through; (b)
    /// orphan accounting runs BEFORE the head check; (c) promote the queue head.
    fn ensure_active_turn(&mut self, events: &mut Vec<TurnEvent>) {
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
            if let Some(deferred) = active.deferred_settle {
                self.settle_with_usage(deferred.stop_reason, deferred.usage, events);
            }
        }
        if self.pending_orphan_results > 0 {
            self.pending_orphan_results -= 1;
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

    /// Whether the active turn is held open for live subagents (has a stored
    /// outcome and is not settled) — the `isHeldOpen` of `acp-agent.js:125`.
    fn is_held(&self) -> bool {
        self.active.as_ref().is_some_and(is_held_turn)
    }

    /// Whether any subagent this turn spawned is still live — while true the
    /// turn's settlement stays deferred (`turnAwaitingSubagents`).
    fn turn_awaiting_subagents(&self, turn: &Turn) -> bool {
        turn.spawned_task_ids
            .iter()
            .any(|id| self.live_subagents.get(id).copied().unwrap_or(false))
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
        let deferred = self.active.as_ref().and_then(|t| t.deferred_settle);
        if let Some(deferred) = deferred {
            self.settle_with_usage(deferred.stop_reason, deferred.usage, events);
        }
    }

    /// Settle the active turn with `outcome` now — unless subagents it spawned
    /// are still live, in which case store the outcome and hold it open
    /// (`settleOrDefer`, `#866`).
    fn settle_or_defer(&mut self, outcome: StopReason, events: &mut Vec<TurnEvent>) {
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
                });
            }
        }
    }

    /// Settle the active turn exactly once and clear the active slot. Ports
    /// `settleActive` (`ADP:1579-1610`), including `activeTurn = null` (review
    /// 04 #5). Reports the live per-turn accumulator (`accumulatedUsage`).
    fn settle(&mut self, outcome: StopReason, events: &mut Vec<TurnEvent>) {
        self.settle_with_usage(outcome, self.accumulated_usage, events);
    }

    /// Settle the active turn with an explicit usage snapshot — used by the
    /// held-settle lanes, which report the usage the turn's result recorded
    /// (`deferredSettle.usage`) rather than the live accumulator.
    fn settle_with_usage(
        &mut self,
        outcome: StopReason,
        usage: Usage,
        events: &mut Vec<TurnEvent>,
    ) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.settled {
            return;
        }
        active.settled = true;
        let prompt_uuid = active.prompt_uuid.clone();
        self.active = None;
        events.push(TurnEvent::Settled {
            prompt_uuid,
            stop_reason: outcome,
            usage,
            had_usage: true,
        });
    }

    /// Fail the active turn without tearing down the machine (`failActive`,
    /// `ADP:1611-1631`), clearing the active slot (review 04 #5).
    fn fail_active(&mut self, kind: FailureKind, message: String, events: &mut Vec<TurnEvent>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.settled {
            return;
        }
        active.settled = true;
        let prompt_uuid = active.prompt_uuid.clone();
        self.active = None;
        events.push(TurnEvent::Failed {
            prompt_uuid,
            kind,
            message,
        });
    }

    /// Promote `queued` to active, resetting the per-turn accumulator and the
    /// cancelled / orphan-skip flags (`activateTurn`, `ADP:1362-1392`).
    fn activate(&mut self, queued: Turn, events: &mut Vec<TurnEvent>) {
        self.cancelled = false;
        self.pending_orphan_results = 0;
        self.orphaned_uuids.clear();
        self.accumulated_usage = Usage::default();
        let prompt_uuid = queued.prompt_uuid.clone();
        self.active = Some(queued);
        events.push(TurnEvent::Activated { prompt_uuid });
    }
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
