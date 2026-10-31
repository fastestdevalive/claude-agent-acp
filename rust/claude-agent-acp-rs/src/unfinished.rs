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

/// Per-session books for the unfinished-tool check.
#[derive(Debug, Default)]
pub struct ToolBooks {
    /// `tool_use` ids of complete assistant messages (`dispatchedToolCalls`).
    dispatched: HashSet<String>,
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
                self.dispatched.insert(id.to_string());
            }
        }
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
        self.dispatched.remove(id);
    }

    /// A background task registered for tool call `id` (`registerLiveTask`):
    /// the call may outlive the turn, so it is no longer foreground work. The
    /// `dispatched` record stays.
    pub fn forget_foreground(&mut self, id: &str) {
        for ids in self.foreground.values_mut() {
            ids.retain(|existing| existing != id);
        }
    }

    fn take_turn(&mut self, prompt_uuid: &str) -> Vec<String> {
        self.foreground.remove(prompt_uuid).unwrap_or_default()
    }
}

/// The closing `tool_call_update` for a tool call the turn ended without a
/// result for (`AcpToolCallRenderer.unfinished`, v1 clients: `failed`).
fn unfinished_update(id: &str, ran: bool) -> SessionUpdate {
    let text = if ran {
        ENDED_WITHOUT_RESULT
    } else {
        STOPPED_BEFORE_RAN
    };
    let fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::Failed)
        .content(vec![ToolCallContent::from(ContentBlock::Text(
            TextContent::new(text),
        ))]);
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id.to_string(), fields))
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
) -> (Vec<TurnEvent>, Vec<SessionUpdate>) {
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
                    updates.push(unfinished_update(id, false));
                    state.close_tool_call(id);
                }
                if stuck.is_empty() {
                    out.push(event);
                    continue;
                }
                for id in &stuck {
                    updates.push(unfinished_update(id, true));
                    state.close_tool_call(id);
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
            // Cancelled / other settles and failures never run the check, but
            // their turn's books must not leak.
            TurnEvent::Settled {
                ref prompt_uuid, ..
            }
            | TurnEvent::Failed {
                ref prompt_uuid, ..
            } => {
                state.books.take_turn(prompt_uuid);
                out.push(event);
            }
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

    fn update_json(u: &SessionUpdate) -> Value {
        serde_json::to_value(u).unwrap()
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
