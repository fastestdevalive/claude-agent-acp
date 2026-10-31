//! Port of `permissions/response.ts` (Q-33): decode, validate and interpret an
//! ACP `session/request_permission` answer exactly once.
//!
//! Node's `decodeClaudePermissionResponse` (`permissions/response.ts:10-36`):
//!
//! 1. `parseClaudePermissionSelection` (`permissions/effects.ts:20-30`) throws
//!    `Tool use aborted` for every outcome that is not `selected`, and decodes
//!    the ExitPlanMode context-reset mode of a selected id.
//! 2. A selected id that was not among the offered options throws
//!    `Permission option was not offered: <id>`.
//! 3. `applyClaudePermissionSelection` maps the id to the tool's effect and
//!    throws `Unknown permission option: <id>` for an id no effect knows.
//!
//! A throw reaches the CLI the way the SDK writes a failed `can_use_tool`
//! handler (`sdk.mjs` `handleControlRequest`): a `control_response` with
//! `subtype:"error"` and the thrown `message` — never a deny verdict.

use serde_json::{json, Value};

use crate::permission_options::{self, OptionsContext, SelectionOutcome};

/// The message of Node's `throw new Error("Tool use aborted")` (a non-selected
/// outcome, or an aborted signal).
pub const TOOL_USE_ABORTED: &str = "Tool use aborted";

/// The decoded answer: the effect of the selected option and, for ExitPlanMode,
/// the mode the fresh context should resume in (`contextResetMode`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudePermissionDecision {
    pub outcome: SelectionOutcome,
    pub context_reset_mode: Option<String>,
}

/// `decodeClaudePermissionResponse` (`permissions/response.ts:10-36`). `Err`
/// carries the message Node throws, which the CLI must see as an error
/// `control_response`.
pub fn decode_claude_permission_response(
    response: &Value,
    ctx: &OptionsContext,
) -> Result<ClaudePermissionDecision, String> {
    let (option_id, context_reset_mode) = parse_claude_permission_selection(response, ctx)?;
    // `offeredOptions.find((option) => option.optionId === selection.optionId)`:
    // an id the client never received is refused, exactly like Node's throw.
    if !permission_options::option_was_offered(ctx, &option_id) {
        return Err(format!("Permission option was not offered: {option_id}"));
    }
    let outcome = permission_options::apply_selection(&option_id, ctx)?;
    Ok(ClaudePermissionDecision {
        outcome,
        context_reset_mode,
    })
}

/// `parseClaudePermissionSelection` (`permissions/effects.ts:20-30`): the
/// envelope check and the ExitPlanMode context-reset decode.
fn parse_claude_permission_selection(
    response: &Value,
    ctx: &OptionsContext,
) -> Result<(String, Option<String>), String> {
    let outcome = response.get("outcome");
    if outcome
        .and_then(|o| o.get("outcome"))
        .and_then(Value::as_str)
        != Some("selected")
    {
        return Err(TOOL_USE_ABORTED.to_string());
    }
    let option_id = js_string(outcome.and_then(|o| o.get("optionId")));
    let context_reset_mode = if ctx.tool_name == "ExitPlanMode" {
        permission_options::exit_plan_clear_context_mode(&option_id).map(String::from)
    } else {
        None
    };
    Ok((option_id, context_reset_mode))
}

/// `String(value)` for the option id in an error message: a missing id is
/// `undefined`, like Node's template literal.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// The `control_response` the SDK writes when the handler throws
/// (`sdk.mjs` `handleControlRequest`): `subtype:"error"` with the error's
/// `message`, correlated by the request id.
pub fn error_control_response(request_id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error,
        },
    })
}

/// Whether a control response is the SDK's error verdict (`subtype:"error"`).
pub fn is_error_control_response(response: &Value) -> bool {
    response
        .get("response")
        .and_then(|r| r.get("subtype"))
        .and_then(Value::as_str)
        == Some("error")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(tool_name: &str, input: Value, available_modes: Vec<&str>) -> OptionsContext {
        OptionsContext {
            tool_name: tool_name.to_string(),
            display_name: None,
            input,
            cwd: "/workspace".into(),
            change_set: None,
            no_persistent_rule: false,
            default_to_no: false,
            available_modes: available_modes.into_iter().map(String::from).collect(),
            auto_unavailable: false,
            pre_plan_mode: None,
            context_used_percent: None,
            home_dir: None,
        }
    }

    /// `permission-effects.test.ts` > "Claude permission response effects" >
    /// "distinguishes explicit reject from cancellation": a reject is a deny
    /// verdict; a cancelled outcome throws `Tool use aborted`, which the SDK
    /// writes as an error `control_response` (never the R28 deny).
    #[test]
    fn t6a2_distinguishes_explicit_reject_from_cancellation() {
        let bash = ctx("Bash", json!({}), Vec::new());
        let rejected = decode_claude_permission_response(
            &json!({"outcome": {"outcome": "selected", "optionId": "reject"}}),
            &bash,
        )
        .expect("a reject decodes");
        assert_eq!(
            rejected.outcome,
            SelectionOutcome::Deny {
                message: "User refused permission to run tool".to_string(),
                interrupt: false,
            }
        );

        let cancelled =
            decode_claude_permission_response(&json!({"outcome": {"outcome": "cancelled"}}), &bash);
        assert_eq!(cancelled, Err("Tool use aborted".to_string()));
        assert_eq!(
            error_control_response("permreq1", TOOL_USE_ABORTED),
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": "permreq1",
                    "error": "Tool use aborted",
                },
            }),
            "a throw is an error control_response, not a deny"
        );
        assert!(is_error_control_response(&error_control_response(
            "permreq1",
            TOOL_USE_ABORTED
        )));
    }

    /// `permission-effects.test.ts` > "fails closed for an unavailable or
    /// unknown selection": an option that was not offered and an id no effect
    /// knows both throw; the offered-set guard runs first, so an unoffered id
    /// never reaches the effect.
    #[test]
    fn t6a2_fails_closed_for_an_unavailable_or_unknown_selection() {
        let bash = ctx("Bash", json!({}), Vec::new());
        assert_eq!(
            decode_claude_permission_response(
                &json!({"outcome": {"outcome": "selected", "optionId": "allow-with-updates"}}),
                &bash,
            ),
            Err("Permission option was not offered: allow-with-updates".to_string())
        );
        assert_eq!(
            decode_claude_permission_response(
                &json!({"outcome": {"outcome": "selected", "optionId": "future"}}),
                &bash,
            ),
            Err("Permission option was not offered: future".to_string())
        );

        // An offered id the effect does not know (`Bash` offers only the common
        // ids): `applyCommonSelection` throws `Unknown permission option`.
        let mut ctx = bash.clone();
        ctx.tool_name = "EnterPlanMode".to_string();
        assert_eq!(
            permission_options::apply_selection("exit-plan-auto", &ctx),
            Err("Unknown permission option: exit-plan-auto".to_string())
        );
    }

    /// `permission-effects.test.ts` > "returns clear-context semantics with the
    /// decoded permission decision": the decision carries the reset mode beside
    /// the interrupting deny.
    #[test]
    fn t6a2_returns_clear_context_semantics_with_the_decoded_permission_decision() {
        let exit = ctx(
            "ExitPlanMode",
            json!({"plan": "Implement it"}),
            vec!["auto", "default", "acceptEdits"],
        );
        let decision = decode_claude_permission_response(
            &json!({"outcome": {"outcome": "selected", "optionId": "exit-plan-clear-auto"}}),
            &exit,
        )
        .expect("an offered clear-context id decodes");
        assert_eq!(decision.context_reset_mode.as_deref(), Some("auto"));
        assert_eq!(
            decision.outcome,
            SelectionOutcome::Deny {
                message: "User accepted the plan and requested a fresh context".to_string(),
                interrupt: true,
            }
        );

        // A mode choice carries no reset mode.
        let approved = decode_claude_permission_response(
            &json!({"outcome": {"outcome": "selected", "optionId": "exit-plan-auto"}}),
            &exit,
        )
        .expect("an offered mode id decodes");
        assert_eq!(approved.context_reset_mode, None);
    }
}
