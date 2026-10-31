//! Unfinished-tool post-filter (plan item 3.7; E-12 / R851-4; Decision 7).
//!
//! Node 0.94.0 checks every `end_turn` settle (`dist/acp-agent.js:3559-3605`):
//! a foreground tool call of THAT turn that has no `tool_result` is closed with
//! a `failed` `tool_call_update`, and when Claude Code had dispatched it (its
//! `tool_use` reached a complete assistant message) the prompt additionally
//! fails `-32603` `incomplete_tool_call`.
//!
//! This is a pure post-filter over [`TurnEvent`]s. It reads and clears only its
//! own books ([`ToolBooks`]); the `TurnMachine` debt books are never touched,
//! so `settle_with_usage` / `fail_active` keep identical accounting. The
//! foreground set is KEYED BY `prompt_uuid` because `ensure_active_turn` emits a
//! held turn's `Settled` and the next turn's `Activated` in one batch.

use std::collections::{HashMap, HashSet};

use agent_client_protocol::schema::v1::{
    ContentBlock, SessionUpdate, TextContent, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields,
};
use serde_json::Value;

use crate::map::MapState;
use crate::turn::{FailureKind, StopReason, TurnEvent};

/// Closing text for a tool call Claude abandoned before it ran.
const STOPPED_BEFORE_RAN: &str = "Claude stopped this tool call before it ran.";
/// Closing text for a dispatched tool call the turn ended without a result for.
const ENDED_WITHOUT_RESULT: &str =
    "Claude ended the turn without returning a result for this tool.";
/// Closing text for a tool call a `session/cancel` stopped before it returned.
const CANCELLED_BEFORE_RESULT: &str =
    "The turn was cancelled before this tool call returned a result.";

/// `MAX_DISPATCHED_TOOL_CALLS` (`acp-agent.ts:12858`).
const MAX_DISPATCHED_TOOL_CALLS: usize = 1000;

/// Per-session books for the unfinished-tool check.
#[derive(Debug, Default)]
pub struct ToolBooks {
    /// `tool_use` ids of complete assistant messages (`dispatchedToolCalls`),
    /// oldest first, at most [`MAX_DISPATCHED_TOOL_CALLS`].
    dispatched: Vec<String>,
    /// Foreground tool-call ids surfaced while a turn was current, in
    /// insertion order, keyed by that turn's `prompt_uuid`.
    foreground: HashMap<String, Vec<String>>,
}

impl ToolBooks {
    /// Record the `tool_use` blocks of a consolidated assistant message
    /// (`recordDispatchedToolUses`).
    pub fn record_dispatched(&mut self, content: &Value) {
        let Some(blocks) = content.as_array() else {
            return;
        };
        for block in blocks {
            let is_tool_use = matches!(
                block["type"].as_str(),
                Some("tool_use" | "server_tool_use" | "mcp_tool_use")
            );
            if let (true, Some(id)) = (is_tool_use, block["id"].as_str()) {
                self.record_dispatched_call(id);
            }
        }
    }

    /// `recordDispatchedToolCall`: a repeated id moves to the end, the oldest is
    /// dropped past the cap.
    pub fn record_dispatched_call(&mut self, id: &str) {
        self.dispatched.retain(|existing| existing != id);
        self.dispatched.push(id.to_string());
        if self.dispatched.len() > MAX_DISPATCHED_TOOL_CALLS {
            self.dispatched.remove(0);
        }
    }

    /// Whether Claude Code sent the tool call to the tool runner
    /// (`dispatchedToolCalls.has`).
    pub fn is_dispatched(&self, id: &str) -> bool {
        self.dispatched.iter().any(|existing| existing == id)
    }

    /// Record a surfaced top-level `tool_call` under the current turn
    /// (`recordForegroundToolCall`); no current turn -> nothing to check.
    pub fn record_foreground(&mut self, turn: Option<&str>, id: &str) {
        let Some(turn) = turn else {
            return;
        };
        let ids = self.foreground.entry(turn.to_string()).or_default();
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }

    /// A tool call reached a final status or belongs to a subagent: it is no
    /// longer this turn's foreground work (`forgetForegroundToolCall`).
    pub fn forget(&mut self, id: &str) {
        for ids in self.foreground.values_mut() {
            ids.retain(|existing| existing != id);
        }
        self.dispatched.retain(|existing| existing != id);
    }

    /// A background task registered for tool call `id` (`registerLiveTask`):
    /// the call may outlive the turn, so it is no longer foreground work. The
    /// `dispatched` record stays.
    pub fn forget_foreground(&mut self, id: &str) {
        for ids in self.foreground.values_mut() {
            ids.retain(|existing| existing != id);
        }
    }

    /// Whether any foreground tool call is still open (`foregroundWork.has(sessionId)`,
    /// `acp-agent.ts:3824-3832`): a cycle Claude Code started on its own keeps
    /// one even when `turnQueue` is empty.
    #[must_use]
    pub fn has_foreground(&self) -> bool {
        self.foreground.values().any(|ids| !ids.is_empty())
    }

    fn take_turn(&mut self, prompt_uuid: &str) -> Vec<String> {
        self.foreground.remove(prompt_uuid).unwrap_or_default()
    }
}

/// One closing report of the unfinished-tool check.
///
/// The v1 update always carries `failed` (v1 has no `cancelled` status). The
/// session layer turns it into the v2 `cancelled` status of a call the turn
/// stopped (`AcpToolCallRenderer.unfinished`: `stopped && capabilities.v2`),
/// which is why the sender needs to know whether the call was stopped (C-24).
#[derive(Debug, Clone, PartialEq)]
pub struct ClosingReport {
    /// The v1 `tool_call_update` (status `failed`).
    pub update: SessionUpdate,
    /// `!ran || cancelled` (`unfinished`'s `stopped`): a v2 client gets
    /// `cancelled` for it, every other client `failed`.
    pub stopped: bool,
}

impl ClosingReport {
    /// The wire form of the report. A v2 client gets `cancelled` for a call the
    /// turn stopped (`unfinished`, `renderer.ts:661`); v1 has no `cancelled`
    /// status, so every other client keeps `failed`.
    pub fn wire_json(&self, v2: bool) -> Value {
        let mut value = serde_json::to_value(&self.update).unwrap_or(Value::Null);
        if v2 && self.stopped {
            value["status"] = Value::String("cancelled".to_string());
        }
        value
    }
}

/// The closing `tool_call_update` for a tool call the turn ended without a
/// result for (`AcpToolCallRenderer.unfinished`, v1 clients: `failed`), with
/// the exit of its display terminal (`terminalExit`, T-84) and the Agent/Task
/// call of the subagent that made it (`stampParentToolUseId`).
///
/// The report ends the call: it is forgotten here, before the update is sent,
/// so a late hook or a second close cannot overwrite it (`closeToolCall`,
/// `acp-agent.ts:5222-5235`).
fn closing_report(state: &mut MapState, id: &str, ran: bool, cancelled: bool) -> ClosingReport {
    let text = if !ran {
        STOPPED_BEFORE_RAN
    } else if cancelled {
        CANCELLED_BEFORE_RESULT
    } else {
        ENDED_WITHOUT_RESULT
    };
    // Read before the cache entry goes: it names the tool.
    let tool_name = state
        .tool_use_from_cache(id)
        .and_then(|tool_use| tool_use.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent = state.tool_call_parent(id).map(str::to_string);
    let mut meta = serde_json::Map::new();
    if let Some(exit) = tool_name
        .as_deref()
        .and_then(|name| state.terminal_exit_meta(name, id))
    {
        meta.extend(exit);
    }
    crate::tool_meta::stamp_parent(&mut meta, parent.as_deref());
    // C-24: a call this report closes because the turn end or a cancel stopped
    // it is `cancelled` on v2 (`renderer.ts:650-668`); the session rewrites the
    // typed v1 `failed` status on the v2 send.
    if state.v2_reporting() && (!ran || cancelled) {
        state.note_stopped_closing(id);
    }
    state.close_tool_call(id);
    let fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::Failed)
        .content(vec![ToolCallContent::from(ContentBlock::Text(
            TextContent::new(text),
        ))]);
    let mut update = ToolCallUpdate::new(id.to_string(), fields);
    if !meta.is_empty() {
        update = update.meta(meta);
    }
    ClosingReport {
        update: SessionUpdate::ToolCallUpdate(update),
        stopped: !ran || cancelled,
    }
}

/// End the open tool calls that `include` accepts with the report of a call a
/// cancel stopped and sent no result for (`closeOpenToolCalls`,
/// `acp-agent.ts:5263-5278`). `include` gets the Agent/Task call of the subagent
/// that made the tool call, if a subagent did.
pub fn close_open_tool_calls(
    state: &mut MapState,
    include: &dyn Fn(&MapState, Option<&str>) -> bool,
) -> Vec<ClosingReport> {
    let mut updates = Vec::new();
    for id in state.emitted_ids() {
        let parent = state.tool_call_parent(&id).map(str::to_string);
        if !include(state, parent.as_deref()) {
            continue;
        }
        let ran = state.books.is_dispatched(&id);
        updates.push(closing_report(state, &id, ran, true));
    }
    updates
}

/// Whether the closing at the end of the work a cancel stopped takes a tool call
/// made by the subagent of the Agent call `parent` (`None` for a call of the
/// main conversation): not one of a running background subagent, which the
/// cancel may not have stopped (`closedAtCancel`).
fn closed_at_cancel(state: &MapState, parent: Option<&str>) -> bool {
    parent.is_none_or(|parent| !state.runs_in_background(parent))
}

/// Close the tool calls the work a cancel stopped left open
/// (`closeCancelledToolCalls`): a tool of a subagent (Claude Code writes its
/// interrupted result only to the subagent's own transcript), a call Claude
/// abandoned before it ran, and every call when the stream gives no results at
/// all (the force-cancel backstop). A background subagent keeps its calls open.
pub fn close_cancelled_tool_calls(state: &mut MapState) -> Vec<ClosingReport> {
    close_open_tool_calls(state, &closed_at_cancel)
}

/// The model cycle that Claude Code started on its own ends: close what it left
/// open first (`closeCycleToolCalls`, `acp-agent.ts:5391-5404`). After a cancel
/// that is [`close_cancelled_tool_calls`]; at any other end every call that ran
/// has its result, so what is open is a call of the main conversation that
/// Claude abandoned before it ran. A background subagent's calls are not the
/// cycle's.
pub fn close_cycle_tool_calls(state: &mut MapState) -> Vec<ClosingReport> {
    if state.is_stopped_work() {
        return close_cancelled_tool_calls(state);
    }
    let mut updates = Vec::new();
    for id in state.emitted_ids() {
        if state.tool_call_parent(&id).is_some() || state.books.is_dispatched(&id) {
            continue;
        }
        updates.push(closing_report(state, &id, false, false));
    }
    updates
}

/// Close the tool calls of the subagent that the Agent call `agent_tool_use_id`
/// started, once the SDK reports it stopped after a cancel stopped it and the
/// work that waited on it ended (`closeStoppedSubagentToolCalls`,
/// `acp-agent.ts:5320-5326`). While the work still runs, the stopped subagent's
/// calls get Claude Code's result, or [`close_cancelled_tool_calls`] at the
/// work's end.
pub fn close_stopped_subagent_tool_calls(
    state: &mut MapState,
    work_end_reported: bool,
    agent_tool_use_id: &str,
) -> Vec<ClosingReport> {
    if agent_tool_use_id.is_empty() || !state.is_stopped_work() || !work_end_reported {
        return Vec::new();
    }
    close_open_tool_calls(state, &|_, parent| parent == Some(agent_tool_use_id))
}

/// Apply the check to one batch of events. Returns the (possibly rewritten)
/// events and the closing updates to send BEFORE the settle is delivered.
///
/// `background_parents` are the Agent/Task tool-call ids of live background
/// tasks (exempt: a confirmed background task may outlive the turn).
pub fn gate_events(
    events: Vec<TurnEvent>,
    state: &mut MapState,
    background_parents: &HashSet<&str>,
) -> (Vec<TurnEvent>, Vec<ClosingReport>) {
    let mut updates = Vec::new();
    let mut out = Vec::with_capacity(events.len());
    for event in events {
        match event {
            TurnEvent::Settled {
                ref prompt_uuid,
                stop_reason: StopReason::EndTurn,
                usage,
                ref model_rows,
                ..
            } => {
                let held = state.books.take_turn(prompt_uuid);
                let unfinished: Vec<String> = held
                    .into_iter()
                    .filter(|id| state.has_emitted(id) && !background_parents.contains(id.as_str()))
                    .collect();
                let (stuck, abandoned): (Vec<String>, Vec<String>) = unfinished
                    .into_iter()
                    .partition(|id| state.books.dispatched.contains(id));
                for id in &abandoned {
                    updates.push(closing_report(state, id, false, false));
                }
                if stuck.is_empty() {
                    out.push(event);
                    continue;
                }
                for id in &stuck {
                    updates.push(closing_report(state, id, true, false));
                }
                out.push(TurnEvent::Failed {
                    prompt_uuid: prompt_uuid.clone(),
                    kind: FailureKind::IncompleteToolCall,
                    message: format!(
                        "Claude ended the turn without returning results for tool calls: {}",
                        stuck.join(", ")
                    ),
                    usage,
                    model_rows: model_rows.clone(),
                    is_structured_output_retries: false,
                });
            }
            // A turn a cancel stopped ends with every tool call the work left open
            // closed, the client being told the work ended only after that
            // (`settleActive`, `acp-agent.ts:5480-5484`). A turn swept from the
            // queue (`had_usage: false`) never ran: it opened no call.
            TurnEvent::Settled {
                ref prompt_uuid,
                stop_reason: StopReason::Cancelled,
                had_usage: true,
                ..
            } if state.is_stopped_work() => {
                state.books.take_turn(prompt_uuid);
                updates.extend(close_cancelled_tool_calls(state));
                out.push(event);
            }
            // Other settles and failures never run the check, but their turn's
            // books must not leak.
            TurnEvent::Settled {
                ref prompt_uuid, ..
            }
            | TurnEvent::Failed {
                ref prompt_uuid, ..
            } => {
                state.books.take_turn(prompt_uuid);
                out.push(event);
            }
            // The model cycle Claude Code started on its own ended: what it left
            // open closes before its end is reported.
            TurnEvent::CycleEnded => updates.extend(close_cycle_tool_calls(state)),
            other => out.push(other),
        }
    }
    (out, updates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::Usage;
    use serde_json::json;

    fn settled(uuid: &str, stop_reason: StopReason) -> TurnEvent {
        TurnEvent::Settled {
            prompt_uuid: uuid.to_string(),
            stop_reason,
            usage: Usage::default(),
            had_usage: true,
            model_rows: Default::default(),
        }
    }

    /// Surface `id` as an emitted tool call under `turn`.
    fn surface(state: &mut MapState, turn: &str, id: &str, dispatched: bool) {
        state.mark_emitted(id.to_string(), json!({"type": "tool_use", "id": id}));
        state.books.record_foreground(Some(turn), id);
        if dispatched {
            state
                .books
                .record_dispatched(&json!([{"type": "tool_use", "id": id}]));
        }
    }

    fn update_json(r: &ClosingReport) -> Value {
        serde_json::to_value(&r.update).unwrap()
    }

    #[test]
    fn inv_41_unfinished_dispatched_tool_fails_turn() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", true);
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert_eq!(updates.len(), 1);
        let u = update_json(&updates[0]);
        assert_eq!(u["toolCallId"], "toolu_a");
        assert_eq!(u["status"], "failed");
        assert_eq!(
            u["content"][0]["content"]["text"],
            "Claude ended the turn without returning a result for this tool."
        );
        assert_eq!(
            events,
            vec![TurnEvent::Failed {
                prompt_uuid: "p1".into(),
                kind: FailureKind::IncompleteToolCall,
                message: "Claude ended the turn without returning results for tool calls: toolu_a"
                    .into(),
                usage: Usage::default(),
                model_rows: Default::default(),
                is_structured_output_retries: false,
            }]
        );
        assert!(!state.has_emitted("toolu_a"), "the call is closed");
    }

    #[test]
    fn inv_41_abandoned_streamed_tool_closes_without_failing() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", false);
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(
            update_json(&updates[0])["content"][0]["content"]["text"],
            "Claude stopped this tool call before it ran."
        );
        assert_eq!(events, vec![settled("p1", StopReason::EndTurn)]);
    }

    /// T8C (`tools.test.ts` > "closes a tool call that Claude dropped before it
    /// ran as cancelled on v2, and as failed elsewhere"): `unfinished`'s
    /// `stopped` rule. The abandoned call is `cancelled` for a v2 client and
    /// `failed` for every other client; a call Claude ran but returned no result
    /// for is `failed` on v2 too.
    #[test]
    fn t8c_dropped_tool_call_is_cancelled_on_v2() {
        let mut state = MapState::default();
        state.v2.set_reporting(true);
        surface(&mut state, "p1", "toolu_bash", false);
        let (_, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(
            serde_json::to_string(&updates[0].wire_json(true)).unwrap(),
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_bash","status":"cancelled","content":[{"type":"content","content":{"type":"text","text":"Claude stopped this tool call before it ran."}}]}"#
        );
        // v1 (and AIR, which is not v2) has no cancelled status.
        assert_eq!(updates[0].wire_json(false)["status"], "failed");

        // A tool that ran but returned no result failed, also on v2.
        let mut state = MapState::default();
        state.v2.set_reporting(true);
        surface(&mut state, "p1", "toolu_bash", true);
        let (_, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        let value = updates[0].wire_json(true);
        assert_eq!(value["status"], "failed");
        assert_eq!(
            value["content"][0]["content"]["text"],
            "Claude ended the turn without returning a result for this tool."
        );
    }

    /// T8C (C-24, the T8A half): a call a `session/cancel` stopped is
    /// `cancelled` for a v2 client, `failed` for every other client.
    #[test]
    fn t8c_cancelled_call_is_cancelled_on_v2() {
        let mut state = MapState::default();
        state.v2.set_reporting(true);
        surface(&mut state, "p1", "toolu_bash", true);
        state.begin_stopped_work();
        let updates = close_cancelled_tool_calls(&mut state);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].wire_json(true)["status"], "cancelled");
        assert_eq!(updates[0].wire_json(false)["status"], "failed");
        assert_eq!(
            updates[0].wire_json(true)["content"][0]["content"]["text"],
            "The turn was cancelled before this tool call returned a result."
        );
    }

    /// T8A — at a root cancel a background child's tool call stays open while
    /// the foreground child's and the root's calls close.
    #[test]
    fn t8a_root_cancel_spares_a_background_childs_call() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_root", true);
        surface(&mut state, "p1", "toolu_fg", true);
        state.record_tool_call_parent("toolu_fg", "toolu_agent_fg");
        surface(&mut state, "p1", "toolu_bg", true);
        state.record_tool_call_parent("toolu_bg", "toolu_agent_bg");
        state.set_background_subagents(HashSet::from(["toolu_agent_bg".to_string()]));
        state.begin_stopped_work();
        let updates = close_cancelled_tool_calls(&mut state);
        let ids: Vec<String> = updates
            .iter()
            .map(|r| match &r.update {
                SessionUpdate::ToolCallUpdate(update) => update.tool_call_id.to_string(),
                other => panic!("expected a tool_call_update, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec!["toolu_root".to_string(), "toolu_fg".to_string()]);
        assert!(
            state.has_emitted("toolu_bg"),
            "the background call stays open"
        );
    }

    /// T8A — the stopped subagent's tool call closes in the child's session once
    /// the work that waited on it reports its end, with the cancelled wording.
    #[test]
    fn t8a_stopped_subagent_call_closes_after_the_work_end() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_child", true);
        state.record_tool_call_parent("toolu_child", "toolu_agent");
        state.begin_stopped_work();
        assert!(
            close_stopped_subagent_tool_calls(&mut state, false, "toolu_agent").is_empty(),
            "the work still runs"
        );
        let updates = close_stopped_subagent_tool_calls(&mut state, true, "toolu_agent");
        assert_eq!(updates.len(), 1, "{updates:?}");
        let value = update_json(&updates[0]);
        assert_eq!(value["toolCallId"], "toolu_child");
        assert_eq!(value["status"], "failed");
        assert!(
            value["content"][0]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("The turn was cancelled")),
            "{value}"
        );
        assert_eq!(
            value["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_agent"
        );
    }

    #[test]
    fn inv_41_background_task_tool_exempt() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_task", true);
        let bg: HashSet<&str> = ["toolu_task"].into_iter().collect();
        let (events, updates) =
            gate_events(vec![settled("p1", StopReason::EndTurn)], &mut state, &bg);
        assert!(updates.is_empty());
        assert_eq!(events, vec![settled("p1", StopReason::EndTurn)]);
    }

    #[test]
    fn inv_41_cancelled_turn_not_failed() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", true);
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::Cancelled)],
            &mut state,
            &HashSet::new(),
        );
        assert!(updates.is_empty());
        assert_eq!(events, vec![settled("p1", StopReason::Cancelled)]);
    }

    #[test]
    fn inv_41_cancel_paths_do_not_leak_books() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", true);
        let (_, updates) = gate_events(
            vec![settled("p1", StopReason::Cancelled)],
            &mut state,
            &HashSet::new(),
        );
        assert!(updates.is_empty());
        assert!(
            state.books.foreground.is_empty(),
            "cancelled prompt's entries dropped"
        );
    }

    #[test]
    fn inv_41_held_turn_settle_checked_per_prompt() {
        // One batch: the held turn p1 settles and p2 activates. p2's tool is
        // foreground of p2 only, so p1's check must not see it.
        let mut state = MapState::default();
        surface(&mut state, "p2", "toolu_p2", true);
        let (events, updates) = gate_events(
            vec![
                settled("p1", StopReason::EndTurn),
                TurnEvent::Activated {
                    prompt_uuid: "p2".into(),
                },
            ],
            &mut state,
            &HashSet::new(),
        );
        assert!(updates.is_empty());
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], TurnEvent::Settled { .. }));
        // p2's own settle then fails on its own unfinished tool.
        let (events, _) = gate_events(
            vec![settled("p2", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert!(matches!(
            events[0],
            TurnEvent::Failed {
                kind: FailureKind::IncompleteToolCall,
                ..
            }
        ));
    }

    #[test]
    fn inv_41_registered_background_task_forgets_the_call() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_bg", true);
        state.books.forget_foreground("toolu_bg");
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert!(updates.is_empty());
        assert_eq!(events, vec![settled("p1", StopReason::EndTurn)]);
    }

    #[test]
    fn inv_41_result_and_final_update_forget_the_call() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", true);
        state.books.forget("toolu_a");
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert!(updates.is_empty());
        assert_eq!(events, vec![settled("p1", StopReason::EndTurn)]);
    }
}

#[cfg(test)]
mod tests_t9b {
    use super::*;
    use crate::turn::Usage;
    use serde_json::json;

    fn settled(uuid: &str, stop_reason: StopReason) -> TurnEvent {
        TurnEvent::Settled {
            prompt_uuid: uuid.to_string(),
            stop_reason,
            usage: Usage::default(),
            had_usage: true,
            model_rows: Default::default(),
        }
    }

    fn surface(
        state: &mut MapState,
        turn: &str,
        id: &str,
        input: serde_json::Value,
        dispatched: bool,
    ) {
        state.mark_emitted(
            id.to_string(),
            json!({"type": "tool_use", "id": id, "name": "Bash", "input": input}),
        );
        state.books.record_foreground(Some(turn), id);
        if dispatched {
            state
                .books
                .record_dispatched(&json!([{"type": "tool_use", "id": id}]));
        }
    }

    fn update_json(r: &ClosingReport) -> serde_json::Value {
        serde_json::to_value(&r.update).unwrap()
    }

    /// `incomplete-tools.test.ts` > "fails every unfinished tool and allows a
    /// later prompt to succeed": every stuck call closes once and the books no
    /// longer hold either, so the next turn starts clean.
    #[test]
    fn t9b_every_unfinished_tool_fails_and_books_clear() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", json!({"command": "ls"}), true);
        surface(&mut state, "p1", "toolu_b", json!({"command": "pwd"}), true);
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        let ids: Vec<String> = updates
            .iter()
            .map(|r| match &r.update {
                SessionUpdate::ToolCallUpdate(update) => update.tool_call_id.to_string(),
                other => panic!("expected a tool_call_update, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec!["toolu_a", "toolu_b"]);
        for update in &updates {
            assert_eq!(update_json(update)["status"], "failed");
        }
        assert!(matches!(
            &events[0],
            TurnEvent::Failed {
                kind: FailureKind::IncompleteToolCall,
                message,
                ..
            } if message.contains("toolu_a") && message.contains("toolu_b")
        ));
        assert!(!state.has_emitted("toolu_a"));
        assert!(!state.has_emitted("toolu_b"));
        assert!(state.books.foreground.is_empty());
        assert!(state.books.dispatched.is_empty());
    }

    /// `incomplete-tools.test.ts` > "does not treat a request to run in the
    /// background as a confirmed handoff": only a registered live task exempts
    /// the call; `run_in_background` in the input alone does not.
    #[test]
    fn t9b_a_background_request_is_not_a_handoff() {
        let mut state = MapState::default();
        surface(
            &mut state,
            "p1",
            "toolu_bg",
            json!({"command": "sleep 100", "run_in_background": true}),
            true,
        );
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(update_json(&updates[0])["status"], "failed");
        assert!(matches!(
            events[0],
            TurnEvent::Failed {
                kind: FailureKind::IncompleteToolCall,
                ..
            }
        ));
    }

    /// `incomplete-tools.test.ts` > "closes an abandoned tool_use and still
    /// fails the turn for an unfinished tool": the abandoned call closes
    /// without a failure, the dispatched one fails the prompt.
    #[test]
    fn t9b_abandoned_and_dispatched_tools_close_together() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_abandoned", json!({}), false);
        surface(
            &mut state,
            "p1",
            "toolu_stuck",
            json!({"command": "ls"}),
            true,
        );
        let (events, updates) = gate_events(
            vec![settled("p1", StopReason::EndTurn)],
            &mut state,
            &HashSet::new(),
        );
        assert_eq!(updates.len(), 2);
        let text = |r: &ClosingReport| {
            update_json(r)["content"][0]["content"]["text"]
                .as_str()
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(
            text(&updates[0]),
            "Claude stopped this tool call before it ran."
        );
        assert!(text(&updates[1]).contains("without returning a result"));
        assert!(matches!(
            &events[0],
            TurnEvent::Failed {
                kind: FailureKind::IncompleteToolCall,
                message,
                ..
            } if message.contains("toolu_stuck")
        ));
    }

    /// `incomplete-tools.test.ts` > "preserves an existing SDK failure": the
    /// gate never rewrites a failure the SDK already reported.
    #[test]
    fn t9b_gate_preserves_an_existing_sdk_failure() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", json!({"command": "ls"}), true);
        let failed = TurnEvent::Failed {
            prompt_uuid: "p1".into(),
            kind: FailureKind::ProviderError,
            message: "original failure".into(),
            usage: Usage::default(),
            model_rows: Default::default(),
            is_structured_output_retries: false,
        };
        let (events, updates) = gate_events(vec![failed.clone()], &mut state, &HashSet::new());
        assert_eq!(events, vec![failed]);
        assert!(updates.is_empty());
    }

    /// `incomplete-tools.test.ts` > "does not duplicate tool terminals": a
    /// second cancel sweep finds nothing open.
    #[test]
    fn t9b_cancel_sweep_closes_each_call_once() {
        let mut state = MapState::default();
        surface(&mut state, "p1", "toolu_a", json!({"command": "ls"}), true);
        surface(&mut state, "p1", "toolu_b", json!({"command": "pwd"}), true);
        state.begin_stopped_work();
        let first = close_cancelled_tool_calls(&mut state);
        assert_eq!(first.len(), 2);
        let second = close_cancelled_tool_calls(&mut state);
        assert!(second.is_empty(), "no duplicate terminals: {second:?}");
    }
}
