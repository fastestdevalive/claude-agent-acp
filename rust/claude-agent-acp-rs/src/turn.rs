//! Turn state machine & settlement (the R7 cost centre; items 6.1–6.6).
//!
//! A turn is one `session/prompt` from enqueue to settle. This module owns the
//! reverse-engineered state machine that settles a turn — the logic the Node
//! adapter spreads across `dist/acp-agent.js` `1338–1682` (activate/settle/
//! orphan), `1683–1845` (consumer loop), `2049–2163` (idle settle) and
//! `2532–2884` (result → stop reason).
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
//! Queued --> Activated  : echo of user message received
//! Queued --> Abandoned  : cancel / next prompt before echo (known hole #825)
//! Activated --> Streaming
//! Streaming --> Deferred   : result arrives, subagents live
//! Streaming --> Settled    : result arrives, no subagents
//! Streaming --> Settled    : idle without result fails the turn
//! Deferred --> Settled     : last subagent drains
//! Streaming --> Settled    : force-cancel floor fires (phase 11)
//! Abandoned --> Settled    : reconciled as orphan
//! Settled --> [*]
//! ```
//!
//! "Echo" = the CLI re-emits the user message because argv has
//! `--replay-user-messages` (B2). "Orphan" = a turn whose consumer left but
//! whose `result` may still arrive; its result must not be consumed by the
//! next turn (INV-30).
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

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

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

/// Events the turn machine emits for the session actor to act on. These are
/// internal — ACP emission (session/update, prompt response) is phase 7+.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// The active turn settled. The actor resolves the prompt with this
    /// stop reason.
    Settled { stop_reason: StopReason },
    /// The active turn failed (idle without a result, an `is_error` result,
    /// `Please run /login`, …). The actor rejects the prompt with this error.
    Failed { message: String },
    /// The active turn moved to the queue head / was activated.
    Activated,
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
    /// Set when the turn held open for its live subagents: the outcome its
    /// `result` already recorded, settled once the subagents drain.
    deferred_settle: Option<StopReason>,
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
/// must emit.
#[derive(Debug, Default)]
pub struct TurnMachine {
    /// The active (in-flight) turn, if any.
    active: Option<Turn>,
    /// Turns waiting for their echo / result, FIFO.
    queue: VecDeque<Turn>,
    /// Subagent task ids currently live, and whether each is a subagent whose
    /// completion can defer a turn (`liveBackgroundTasks`, `isSubagent`).
    live_subagents: HashMap<String, bool>,
    /// Number of late results still expected from dead (cancelled) turns
    /// (`pendingOrphanResults`). Late results are skipped, never attributed
    /// to a live turn (INV-30).
    pending_orphan_results: usize,
    /// Whether the active turn was cancelled (its result is dropped at the
    /// cancel guard; it settles at idle instead).
    cancelled: bool,
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

    /// Enqueue a prompt turn (it awaits its echo before activation).
    pub fn enqueue(&mut self, turn: Turn) {
        self.queue.push_back(turn);
    }

    /// Mark the active turn cancelled. Its `result` is dropped and the turn
    /// settles `cancelled` at the next idle (phase 11 arms the force-cancel
    /// floor; this is the settle-lane half).
    pub fn cancel_active(&mut self) {
        self.cancelled = true;
    }

    /// Abandon the active turn as cancelled and clear the active slot, seeding
    /// one orphan credit for its late `result` (INV-30). Models cancel() on a
    /// turn whose user message was already pushed to `claude`: the SDK still
    /// runs it and emits a result with no turn to match, so the credit keeps
    /// that late result from being attributed to the next prompt.
    pub fn abandon_active(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        if let Some(active) = self.active.as_mut() {
            if !active.settled {
                active.settled = true;
                self.pending_orphan_results += 1;
                events.push(TurnEvent::Settled {
                    stop_reason: StopReason::Cancelled,
                });
            }
        }
        self.active = None;
        events
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
    /// longer live. If the active turn held open solely for it, drain it.
    pub fn on_task_ended(&mut self, task_id: &str) -> Vec<TurnEvent> {
        self.live_subagents.remove(task_id);
        self.settle_deferred_if_drained()
    }

    /// The echo of a queued turn's user message arrives: promote it to active,
    /// handing off any prior active turn. Returns any events (e.g. the prior
    /// turn settling on hand-off).
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
        self.activate(queued, &mut events);
        events
    }

    /// A `result` frame for the user's turn. Handles the stop-reason table,
    /// `is_error`, the `#453` result-text fallback, and settle-or-defer.
    ///
    /// `assistant_text_delivered` tells whether any assistant text reached the
    /// client before this result (the actor tracks `emittedAssistantText`).
    pub fn on_result(&mut self, result: &Value, assistant_text_delivered: bool) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        let Some(active) = self.active.as_mut() else {
            // A result with no active turn is an orphan (see 6.6): consume one
            // pending-orphan credit, never promote it onto the next turn.
            self.consume_orphan_result();
            return events;
        };

        // A cancelled turn's result is dropped; it settles at idle.
        if self.cancelled {
            return events;
        }

        let is_error = result
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let stop_reason = result
            .get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or("");

        // A refusal can arrive on any result subtype (and may set is_error) —
        // handle it before the subtype switch (`acp-agent.js:2736-2753`).
        if stop_reason == "refusal" {
            let outcome = StopReason::Refusal;
            self.settle_or_defer(outcome, &mut events);
            return events;
        }

        // `Please run /login` on a success result is an auth failure.
        let result_text = result.get("result").and_then(Value::as_str).unwrap_or("");
        if !is_error && result_text.contains("Please run /login") {
            self.fail_active("auth_required: Please run /login".to_string(), &mut events);
            return events;
        }

        let subtype = result.get("subtype").and_then(Value::as_str).unwrap_or("");
        if is_error {
            // An is_error result becomes an error, not a stop reason (6.5).
            self.fail_active(error_message(result, subtype), &mut events);
            return events;
        }

        // max_tokens overrides the subtype default.
        let outcome = if stop_reason == "max_tokens" {
            StopReason::MaxTokens
        } else {
            stop_reason_for_subtype(subtype)
        };

        // #453 result-text fallback: forward the result text when no assistant
        // text was delivered and the turn produced no output tokens (the
        // cache-replay signature), or for a local-only command.
        let output_tokens = result
            .get("usage")
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let is_local = active.is_local_only_command;
        if (is_local || (!assistant_text_delivered && output_tokens == 0))
            && !result_text.is_empty()
        {
            events.push(TurnEvent::FinalText {
                text: result_text.to_string(),
            });
        }

        self.settle_or_defer(outcome, &mut events);
        events
    }

    /// A `session_state_changed: idle` frame — the SDK's authoritative turn-over
    /// signal. An unsettled active turn that reaches idle without a result is
    /// the #825 signature and is failed. A held (deferred) turn drains here.
    pub fn on_idle(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        if self.cancelled {
            // A cancelled turn settles at idle (its result was dropped).
            self.cancelled = false;
            self.settle(StopReason::Cancelled, &mut events);
            return events;
        }
        if self.is_held(&mut events) {
            self.settle_deferred_if_drained();
            return events;
        }
        if self.active.as_ref().is_some_and(|t| !t.settled) {
            // #825: idle without a result — the model stream dropped mid-turn.
            self.fail_active(
                "SDK went idle without emitting a result for the active turn".to_string(),
                &mut events,
            );
        }
        events
    }

    /// Whether the active turn is held open for live subagents (has a stored
    /// outcome and is not settled) — the `isHeldOpen` of `acp-agent.js:125`.
    fn is_held(&self, _events: &mut Vec<TurnEvent>) -> bool {
        self.active
            .as_ref()
            .is_some_and(|t| t.deferred_settle.is_some() && !t.settled)
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
    fn settle_deferred_if_drained(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        if self.is_held(&mut events) {
            let awaiting = self
                .active
                .as_ref()
                .is_some_and(|t| self.turn_awaiting_subagents(t));
            if !awaiting {
                if let Some(outcome) = self.active.as_ref().and_then(|t| t.deferred_settle) {
                    self.settle(outcome, &mut events);
                }
            }
        }
        events
    }

    /// Settle the active turn with `outcome` now — unless subagents it spawned
    /// are still live, in which case store the outcome and hold it open
    /// (`settleOrDefer`, `#866`).
    fn settle_or_defer(&mut self, outcome: StopReason, events: &mut Vec<TurnEvent>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.settled {
            return;
        }
        if self.turn_awaiting_subagents(active) {
            if let Some(active) = self.active.as_mut() {
                active.deferred_settle = Some(outcome);
            }
        } else {
            self.settle(outcome, events);
        }
    }

    /// Settle the active turn exactly once and drop it from the queue.
    fn settle(&mut self, outcome: StopReason, events: &mut Vec<TurnEvent>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.settled {
            return;
        }
        active.settled = true;
        events.push(TurnEvent::Settled {
            stop_reason: outcome,
        });
    }

    /// Fail the active turn without tearing down the machine (`failActive`).
    fn fail_active(&mut self, message: String, events: &mut Vec<TurnEvent>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.settled {
            return;
        }
        active.settled = true;
        events.push(TurnEvent::Failed { message });
    }

    /// Promote `queued` to active, handing off any prior active turn.
    fn activate(&mut self, queued: Turn, events: &mut Vec<TurnEvent>) {
        if let Some(prev) = self.active.take() {
            if !prev.settled {
                // Hand off the previous turn as end_turn (unless held/deferred;
                // those settle with their real outcome).
                let outcome = prev.deferred_settle.unwrap_or(if self.cancelled {
                    StopReason::Cancelled
                } else {
                    StopReason::EndTurn
                });
                // Mark the prior turn settled via the same path.
                let mut prev = prev;
                prev.settled = true;
                events.push(TurnEvent::Settled {
                    stop_reason: outcome,
                });
            }
        }
        // Activation resets the cancelled flag so a turn enqueued after a prior
        // cancel isn't treated as cancelled, and clears stale orphan credits.
        self.cancelled = false;
        self.pending_orphan_results = 0;
        self.active = Some(queued);
        events.push(TurnEvent::Activated);
    }

    /// A late result arrived with no active turn (or the active turn was
    /// cancelled): consume one pending-orphan credit so the next live turn's
    /// echo-less result is not swallowed (INV-30).
    fn consume_orphan_result(&mut self) {
        if self.pending_orphan_results > 0 {
            self.pending_orphan_results -= 1;
        }
    }

    /// Seed an orphan credit for a cancelled turn whose late `result` may still
    /// arrive (phase 11 calls this; the credit keeps that result from being
    /// misattributed to the next prompt).
    pub fn seed_orphan(&mut self) {
        self.pending_orphan_results += 1;
    }
}

/// The non-error `result.subtype` → [`StopReason`] table
/// (`acp-agent.js:2771-2848`). `max_tokens`/`refusal` are handled separately
/// by the caller.
pub fn stop_reason_for_subtype(subtype: &str) -> StopReason {
    match subtype {
        "success" | "error_during_execution" => StopReason::EndTurn,
        "error_max_budget_usd" | "error_max_turns" | "error_max_structured_output_retries" => {
            StopReason::MaxTurnRequests
        }
        // Unknown subtypes are unreachable upstream (`unreachable()`); be safe
        // and end the turn rather than panic (D8).
        _ => StopReason::EndTurn,
    }
}

/// The error message to surface for an `is_error` result (`acp-agent.js` joins
/// `message.errors` or falls back to the subtype).
fn error_message(result: &Value, subtype: &str) -> String {
    result
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .into_iter()
                .next()
        })
        .map(str::to_string)
        .unwrap_or_else(|| subtype.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_frame(subtype: &str, is_error: bool) -> Value {
        serde_json::json!({
            "type": "result",
            "subtype": subtype,
            "is_error": is_error,
            "stop_reason": "end_turn",
            "usage": { "output_tokens": 5 },
        })
    }

    /// 6.T1 (INV-16) — a turn with 2 live subagents does not settle until both
    /// drain (#866): the result defers, the first drain still defers, the
    /// second drain settles.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_16_subagents_hold_settle() {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        // Two subagents spawn.
        machine.on_task_started("sub1", true);
        machine.on_task_started("sub2", true);

        // A result arrives while both are live: it must DEFER, not settle.
        let events = machine.on_result(&result_frame("success", false), false);
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, TurnEvent::Settled { .. })),
            "turn must not settle while both subagents are live"
        );

        // Drain the first subagent: still held open.
        let events = machine.on_task_ended("sub1");
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, TurnEvent::Settled { .. })),
            "turn must still be held open after the first subagent drains"
        );

        // Drain the second: the deferred outcome settles the turn.
        let events = machine.on_task_ended("sub2");
        assert!(
            events.iter().any(|e| matches!(
                e,
                TurnEvent::Settled {
                    stop_reason: StopReason::EndTurn
                }
            )),
            "turn must settle once all subagents drain"
        );
    }

    /// 6.T2 (INV-29) — result text present only in the `result` frame yields a
    /// `TurnEvent::FinalText` (the #453 fallback); emission is phase 7.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_29_result_text_fallback() {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        // Cache-replay signature: no assistant text delivered, output_tokens==0.
        let result = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "the whole answer",
            "usage": { "output_tokens": 0 },
        });
        let events = machine.on_result(&result, false);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::FinalText { text } if text == "the whole answer")),
            "the result-text fallback must yield a FinalText event"
        );
    }

    /// 6.T3 (INV-30) — a dead turn's late `result` is not consumed by the next
    /// turn: the orphan credit swallows it instead of attributing it to the
    /// fresh turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_30_orphan_result_not_reused() {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        // p1 is abandoned (cancelled after its message was already pushed); its
        // late result is owed as an orphan.
        machine.abandon_active();

        // p1's late result arrives while no new turn is active: consumed as an
        // orphan, never attributed to any turn.
        let events = machine.on_result(&result_frame("success", false), false);
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, TurnEvent::Settled { .. })),
            "a dead turn's late result must be consumed as an orphan, not settled"
        );
        assert!(!machine.has_active());

        // The next prompt is enqueued and activated via its echo, and its own
        // result settles it — not swallowed by a leftover orphan credit.
        machine.enqueue(Turn::new("p2".into(), false));
        machine.on_echo("p2");
        assert!(machine.has_active());
        let events = machine.on_result(&result_frame("success", false), false);
        assert!(
            events.iter().any(|e| matches!(
                e,
                TurnEvent::Settled {
                    stop_reason: StopReason::EndTurn
                }
            )),
            "the next turn's own result must settle it"
        );
    }

    /// 6.T4 (INV-31) — idle without a `result` fails the active turn instead of
    /// hanging (#825).
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_31_idle_without_result_fails() {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");

        // No result ever arrived; the SDK goes idle.
        let events = machine.on_idle();
        assert!(
            events.iter().any(|e| matches!(e, TurnEvent::Failed { .. })),
            "idle without a result must fail the active turn"
        );
    }

    /// 6.T5 — table test, one case per `result.subtype` → expected `StopReason`.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_stop_reason_table() {
        let cases = [
            ("success", StopReason::EndTurn),
            ("error_during_execution", StopReason::EndTurn),
            ("error_max_budget_usd", StopReason::MaxTurnRequests),
            ("error_max_turns", StopReason::MaxTurnRequests),
            (
                "error_max_structured_output_retries",
                StopReason::MaxTurnRequests,
            ),
        ];
        for (subtype, expected) in cases {
            assert_eq!(
                stop_reason_for_subtype(subtype),
                expected,
                "subtype {subtype:?} maps to {expected:?}"
            );
        }
    }
}
