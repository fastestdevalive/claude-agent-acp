//! The ExitPlanMode lanes of a session (`src/exit-plan.ts`, 0.94.0): the two
//! pending markers a permission answer leaves for the stream to correlate, the
//! reconciliation of a user message with them, the approved plan that becomes
//! the session's markdown plan, and the result that stands for a
//! "keep planning" interrupt.

use agent_client_protocol::schema::v1::{ToolCallStatus, ToolCallUpdate};
use serde_json::{json, Value};

use crate::tool_meta::parse_tool_result_meta;

/// The id of the plan an approved ExitPlanMode publishes
/// (`PLAN_MODE_PLAN_ID`): each approval replaces the plan before it.
pub const PLAN_MODE_PLAN_ID: &str = "plan-mode";

/// `pendingExitPlanModeInterruption`: the `interrupt: true` deny the permission
/// answer sent, and whether its tool result has come back on the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interruption {
    pub tool_use_id: String,
    pub tool_result_seen: bool,
}

/// `pendingExitPlanContextReset` (`ClearContextReset`): the accepted plan that
/// continues in a fresh context, and the mode that context starts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextReset {
    pub tool_use_id: String,
    pub plan: String,
    pub mode: String,
}

/// The two pending ExitPlanMode lanes of a session.
#[derive(Debug, Default, Clone)]
pub struct ExitPlanState {
    pub interruption: Option<Interruption>,
    pub context_reset: Option<ContextReset>,
}

impl ExitPlanState {
    /// Forget both markers (a cancel, or a result that did not match).
    pub fn clear(&mut self) {
        self.interruption = None;
        self.context_reset = None;
    }

    /// The permission answer for an ExitPlanMode call (`acp-agent.ts:10238-10260`):
    /// a clear-context choice records the reset, and any deny with `interrupt`
    /// records the interruption its result will carry.
    pub fn note_answer(
        &mut self,
        tool_use_id: &str,
        reset: Option<(&str, &str)>,
        interrupted: bool,
    ) {
        if let Some((plan, mode)) = reset {
            let plan = plan.trim();
            if !plan.is_empty() {
                self.context_reset = Some(ContextReset {
                    tool_use_id: tool_use_id.to_string(),
                    plan: plan.to_string(),
                    mode: mode.to_string(),
                });
            }
        }
        if interrupted {
            self.interruption = Some(Interruption {
                tool_use_id: tool_use_id.to_string(),
                tool_result_seen: false,
            });
        }
    }

    /// `observeExitPlanToolResults`: reconcile a user message with the two
    /// pending lanes. A tool result the CLI marked `user-rejected` for an
    /// ExitPlanMode call (re)creates the interruption with its result seen (a
    /// resumed query can lose the marker the permission answer set); the result
    /// of the pending interruption marks it seen. Returns the id of the
    /// accepted plan whose rendered update must be completed.
    pub fn observe_tool_results(
        &mut self,
        blocks: &[Value],
        tool_result_meta: Option<&Value>,
        tool_name_of: impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        if let Some(meta) = tool_result_meta {
            let meta = parse_tool_result_meta(meta);
            if !meta.is_empty() {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result")
                        || block.get("is_error").and_then(Value::as_bool) != Some(true)
                    {
                        continue;
                    }
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if meta
                        .get(id)
                        .is_some_and(|entry| entry.kind == "user-rejected")
                        && tool_name_of(id).as_deref() == Some("ExitPlanMode")
                    {
                        self.interruption = Some(Interruption {
                            tool_use_id: id.to_string(),
                            tool_result_seen: true,
                        });
                        break;
                    }
                }
            }
        }
        if let Some(pending) = self.interruption.as_mut() {
            if contains_tool_result_for(blocks, &pending.tool_use_id) {
                pending.tool_result_seen = true;
            }
        }
        self.context_reset
            .as_ref()
            .filter(|reset| contains_tool_result_for(blocks, &reset.tool_use_id))
            .map(|reset| reset.tool_use_id.clone())
    }

    /// `isExitPlanInterruptionResult` against the pending interruption.
    pub fn is_interruption_result(&self, result: &Value) -> bool {
        self.interruption
            .as_ref()
            .is_some_and(|pending| is_interruption_result(result, pending))
    }

    /// Whether the pending reset belongs to the pending interruption's call.
    pub fn reset_matches_interruption(&self) -> bool {
        match (&self.context_reset, &self.interruption) {
            (Some(reset), Some(pending)) => reset.tool_use_id == pending.tool_use_id,
            _ => false,
        }
    }
}

fn contains_tool_result_for(blocks: &[Value], tool_use_id: &str) -> bool {
    blocks.iter().any(|block| {
        block.get("type").and_then(Value::as_str) == Some("tool_result")
            && block.get("tool_use_id").and_then(Value::as_str) == Some(tool_use_id)
    })
}

/// `syncConfig`'s plan bookkeeping: the mode before a switch into plan mode is
/// remembered (the clear-context options lead with bypass when the session was
/// in bypass before), and any other mode forgets it.
pub fn track_pre_plan_mode(pre_plan_mode: &mut Option<String>, previous: &str, next: &str) {
    if next != "plan" {
        *pre_plan_mode = None;
    } else if previous != "plan" {
        *pre_plan_mode = Some(previous.to_string());
    }
}

/// `executionDiagnostic`: the `[ede_diagnostic]` text of an error result.
pub fn execution_diagnostic(result: &Value) -> Option<String> {
    if result.get("subtype").and_then(Value::as_str) == Some("success") {
        return result
            .get("result")
            .and_then(Value::as_str)
            .filter(|text| text.starts_with("[ede_diagnostic]"))
            .map(str::to_string);
    }
    result
        .get("errors")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .find(|error| error.starts_with("[ede_diagnostic]"))
        .map(str::to_string)
}

/// Whether `diagnostic` carries `key=value` as a whitespace-delimited word
/// (`/(?:^|\s)key=value(?:\s|$)/`).
fn has_word(diagnostic: &str, word: &str) -> bool {
    diagnostic
        .split(|c: char| c.is_whitespace())
        .any(|part| part == word)
}

/// `isExitPlanInterruptionResult`: the error-shaped diagnostic Claude emits for
/// the cycle a recorded ExitPlanMode `interrupt: true` answer stopped.
pub fn is_interruption_result(result: &Value, pending: &Interruption) -> bool {
    if !pending.tool_result_seen || result.get("is_error").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    execution_diagnostic(result).is_some_and(|diagnostic| {
        has_word(&diagnostic, "result_type=user") && has_word(&diagnostic, "stop_reason=tool_use")
    })
}

/// `approvedPlan`: the plan, trimmed, that a successful ExitPlanMode result
/// approved. The structured `output` carries the user's edits and wins over the
/// `input`; a subagent's plan and one waiting for its team lead (only known from
/// the output) and an empty plan give nothing.
pub fn approved_plan(input: &Value, output: &Value) -> Option<String> {
    if output.get("isAgent").and_then(Value::as_bool) == Some(true)
        || output
            .get("awaitingLeaderApproval")
            .and_then(Value::as_bool)
            == Some(true)
    {
        return None;
    }
    let plan = match output.get("plan").and_then(Value::as_str) {
        Some(plan) => Some(plan),
        None => input.get("plan").and_then(Value::as_str),
    }?;
    let trimmed = plan.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `planModeUpdate`: the update that shows an approved plan to a client that
/// takes plan operations.
pub fn plan_mode_update(plan: &str) -> Value {
    json!({
        "sessionUpdate": "plan_update",
        "plan": { "type": "markdown", "planId": PLAN_MODE_PLAN_ID, "content": plan },
    })
}

/// `acceptedPlanToolResult`: the update of the accepted plan's tool call ends
/// `completed` with neither raw output nor content.
pub fn accepted_plan_tool_result(update: &mut ToolCallUpdate) {
    update.fields.raw_output = None;
    update.fields.content = None;
    update.fields.status = Some(ToolCallStatus::Completed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected_meta(id: &str) -> Value {
        json!([{"id": id, "non_execution_kind": "user-rejected"}])
    }

    fn error_block(id: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "is_error": true, "content": "no"})
    }

    fn diagnostic(text: &str) -> Value {
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
            "errors": [text]})
    }

    /// T-88 — the structured output wins over the input, is trimmed, and an
    /// empty plan, a subagent plan and a leader-approval wait give nothing.
    #[test]
    fn t3a_approved_plan_prefers_output_and_skips_subagent_and_empty() {
        let input = json!({"plan": "  from input  "});
        assert_eq!(
            approved_plan(&input, &json!({"plan": "\nfrom output\n"})).as_deref(),
            Some("from output")
        );
        assert_eq!(
            approved_plan(&input, &Value::Null).as_deref(),
            Some("from input")
        );
        assert_eq!(approved_plan(&json!({"plan": "   "}), &Value::Null), None);
        assert_eq!(approved_plan(&json!({}), &json!({"plan": 3})), None);
        assert_eq!(
            approved_plan(&input, &json!({"plan": "p", "isAgent": true})),
            None
        );
        assert_eq!(
            approved_plan(
                &input,
                &json!({"plan": "p", "awaitingLeaderApproval": true})
            ),
            None
        );
    }

    /// U-24 / U-38 — the approved plan rides a `plan_update` under the fixed id.
    #[test]
    fn t3a_plan_mode_update_shape() {
        assert_eq!(
            plan_mode_update("do it"),
            json!({"sessionUpdate": "plan_update",
                "plan": {"type": "markdown", "planId": "plan-mode", "content": "do it"}})
        );
    }

    /// B-76 / P-02 — only an error result whose diagnostic names a user result
    /// type and a tool_use stop reason, after the tool result was seen, is the
    /// interruption.
    #[test]
    fn t3a_interruption_result_needs_seen_result_and_both_diagnostic_words() {
        let seen = Interruption {
            tool_use_id: "t".into(),
            tool_result_seen: true,
        };
        let good = diagnostic(
            "[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use",
        );
        assert!(is_interruption_result(&good, &seen));
        let unseen = Interruption {
            tool_result_seen: false,
            ..seen.clone()
        };
        assert!(!is_interruption_result(&good, &unseen));
        for text in [
            "[ede_diagnostic] result_type=assistant stop_reason=tool_use",
            "[ede_diagnostic] result_type=user stop_reason=end_turn",
            "[ede_diagnostic] xresult_type=user stop_reason=tool_use",
            "result_type=user stop_reason=tool_use",
        ] {
            assert!(!is_interruption_result(&diagnostic(text), &seen), "{text}");
        }
        let mut not_error = good.clone();
        not_error["is_error"] = json!(false);
        assert!(!is_interruption_result(&not_error, &seen));
        // The diagnostic may also be the text of a success-shaped result.
        let success = json!({"type": "result", "subtype": "success", "is_error": true,
            "result": "[ede_diagnostic] result_type=user stop_reason=tool_use"});
        assert!(is_interruption_result(&success, &seen));
    }

    /// P-02 — the CLI's `user-rejected` marker on an ExitPlanMode error result
    /// recreates the interruption (a resumed query loses the permission marker);
    /// another tool, or a non-error block, does not.
    #[test]
    fn t3a_rejected_exit_plan_result_recreates_the_interruption() {
        let names = |id: &str| (id == "plan_1").then(|| "ExitPlanMode".to_string());
        let mut state = ExitPlanState::default();
        state.observe_tool_results(
            &[error_block("other")],
            Some(&rejected_meta("other")),
            names,
        );
        assert!(state.interruption.is_none());
        state.observe_tool_results(
            &[json!({"type": "tool_result", "tool_use_id": "plan_1", "content": "ok"})],
            Some(&rejected_meta("plan_1")),
            names,
        );
        assert!(state.interruption.is_none());
        state.observe_tool_results(
            &[error_block("plan_1")],
            Some(&rejected_meta("plan_1")),
            names,
        );
        assert_eq!(
            state.interruption,
            Some(Interruption {
                tool_use_id: "plan_1".into(),
                tool_result_seen: true
            })
        );
    }

    /// B-76 — the pending interruption sees its tool result; and the accepted
    /// plan id is returned for a clear-context reset's own result only.
    #[test]
    fn t3a_tool_result_marks_the_interruption_seen_and_names_the_accepted_plan() {
        let no_names = |_: &str| None;
        let mut state = ExitPlanState::default();
        state.note_answer("plan_1", Some(("  the plan ", "auto")), true);
        assert_eq!(
            state.context_reset,
            Some(ContextReset {
                tool_use_id: "plan_1".into(),
                plan: "the plan".into(),
                mode: "auto".into()
            })
        );
        assert!(!state.interruption.as_ref().unwrap().tool_result_seen);
        assert_eq!(
            state.observe_tool_results(&[error_block("unrelated")], None, no_names),
            None
        );
        assert!(!state.interruption.as_ref().unwrap().tool_result_seen);
        assert_eq!(
            state.observe_tool_results(&[error_block("plan_1")], None, no_names),
            Some("plan_1".to_string())
        );
        assert!(state.interruption.as_ref().unwrap().tool_result_seen);
        assert!(state.reset_matches_interruption());
        state.clear();
        assert!(state.interruption.is_none() && state.context_reset.is_none());
    }

    /// P-10 — entering plan mode remembers the mode it left; leaving forgets it;
    /// staying in plan keeps it.
    #[test]
    fn t3a_pre_plan_mode_tracks_entry_and_exit() {
        let mut pre = None;
        track_pre_plan_mode(&mut pre, "bypassPermissions", "plan");
        assert_eq!(pre.as_deref(), Some("bypassPermissions"));
        track_pre_plan_mode(&mut pre, "plan", "plan");
        assert_eq!(pre.as_deref(), Some("bypassPermissions"));
        track_pre_plan_mode(&mut pre, "plan", "default");
        assert_eq!(pre, None);
    }

    /// T-87 — the accepted plan's update ends completed with no output.
    #[test]
    fn t3a_accepted_plan_tool_result_completes_and_drops_output() {
        use agent_client_protocol::schema::v1::ToolCallUpdateFields;
        let mut update = ToolCallUpdate::new(
            "plan_1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Failed)
                .raw_output(json!("error text")),
        );
        accepted_plan_tool_result(&mut update);
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
        assert!(update.fields.raw_output.is_none());
        assert!(update.fields.content.is_none());
    }
}
