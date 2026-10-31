//! The ACP v2 session update translation (U-82, U-83, U-84, U-85, U-86, U-104, Q-87;
//! upstream `src/v2/session-update.ts`).
//!
//! Translates v1 session updates emitted by `ClaudeAcpAgent` into v2 updates:
//! - available_commands_update -> v2AvailableCommands
//! - config_option_update -> v2ConfigOptions
//! - session_info_update, usage_update, notice, compaction_update, compaction_summary_chunk -> pass-through
//! - agent_message_chunk, agent_thought_chunk, user_message_chunk -> require messageId
//! - tool_call, tool_call_update -> v2ToolCallUpdate
//! - plan -> plan_update with planId "plan"
//! - plan_update, plan_removed -> pass-through
//! - subagent_update -> state validation (RFD states)
//! - session_message, session_message_chunk -> pass-through
//! - current_mode_update -> dropped (v2 mode is a config option)
//! - child state_update mirror for subagent_update

use agent_client_protocol::Error;
use serde_json::{Map, Value};

use super::session::{v2_available_commands, v2_config_options};
use super::tool_call::v2_tool_call_update;

/// The id of the plan of a session.
///
/// v1 has one plan per session, which every plan update replaces whole.
/// v2 tracks plans by id, so that plan gets "plan".
pub const SESSION_PLAN_ID: &str = "plan";

const SUBAGENT_STATES: &[&str] = &["running", "requires_action", "unknown", "idle"];

const IDLE_STOP_REASONS: &[&str] = &[
    "end_turn",
    "max_tokens",
    "max_turn_requests",
    "refusal",
    "cancelled",
    "error",
];

/// The v2 form of a subagent's v1 work state.
///
/// Validates that state is one of the RFD's (`running`, `requires_action`, `unknown`, `idle`),
/// and if idle, that the stop reason is one of the valid reasons.
pub fn v2_subagent_state(state: &Value) -> Result<Value, Error> {
    let state_str = state.get("state").and_then(Value::as_str);
    let is_valid_state = state_str.is_some_and(|s| SUBAGENT_STATES.contains(&s));
    let stop_reason_val = state.get("stopReason");

    let is_valid_stop_reason = match stop_reason_val {
        None => true,
        Some(Value::Null) => true,
        Some(Value::String(s)) => IDLE_STOP_REASONS.contains(&s.as_str()),
        _ => false,
    };

    if !is_valid_state || (state_str == Some("idle") && !is_valid_stop_reason) {
        let state_name = state_str.unwrap_or("undefined");
        let suffix = if state_str == Some("idle") {
            let reason_name = match stop_reason_val {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "undefined".to_string(),
            };
            format!(" with the stop reason {reason_name}")
        } else {
            String::new()
        };
        return Err(Error::new(
            -32603,
            format!(
                "The ACP v2 surface does not translate the subagent state {state_name}{suffix}"
            ),
        ));
    }

    Ok(state.clone())
}

/// The child's own `state_update` for a v1 `subagent_update` that reports the
/// child's work state, or `None` when it reports none (U-86).
pub fn v2_child_state_update(update: &Value) -> Result<Option<Value>, Error> {
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("subagent_update") {
        return Ok(None);
    }
    let Some(state) = update.get("state") else {
        return Ok(None);
    };
    if state.is_null() {
        return Ok(None);
    }
    let validated = v2_subagent_state(state)?;

    let mut update_map = Map::new();
    update_map.insert(
        "sessionUpdate".to_string(),
        Value::String("state_update".to_string()),
    );
    if let Value::Object(state_obj) = validated {
        for (k, v) in state_obj {
            update_map.insert(k, v);
        }
    }

    let mut notif_map = Map::new();
    if let Some(session_id) = update.get("sessionId") {
        notif_map.insert("sessionId".to_string(), session_id.clone());
    }
    notif_map.insert("update".to_string(), Value::Object(update_map));
    Ok(Some(Value::Object(notif_map)))
}

/// A v1 `failed` tool result the CLI stamped as aborted becomes v2 `cancelled`
/// (`resultStatus`, `renderer.ts:195-203`: `isError && isAbortedToolResult`).
/// v1 has no `cancelled` status, so only the v2 form is rewritten; the
/// `nonExecutionKind` stays in `_meta` (upstream keeps it on v2 too).
fn rewrite_aborted_status(report: Value) -> Value {
    let aborted = report
        .pointer("/_meta/claudeCode/nonExecutionKind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "interrupted" || kind == "cancelled");
    if !aborted || report.get("status").and_then(Value::as_str) != Some("failed") {
        return report;
    }
    let Value::Object(mut map) = report else {
        return report;
    };
    map.insert("status".to_string(), Value::String("cancelled".to_string()));
    Value::Object(map)
}

/// The v2 form of a v1 session update, or `None` when v2 has no counterpart.
///
/// Throws an error on untranslated update kinds.
pub fn v2_session_update(update: &Value) -> Result<Option<Value>, Error> {
    let Some(update_obj) = update.as_object() else {
        return Ok(None);
    };
    let kind = update_obj
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match kind {
        "available_commands_update" => {
            let mut res = update_obj.clone();
            if let Some(commands) = update_obj
                .get("availableCommands")
                .and_then(Value::as_array)
            {
                res.insert(
                    "availableCommands".to_string(),
                    Value::Array(v2_available_commands(commands)),
                );
            }
            Ok(Some(Value::Object(res)))
        }
        "config_option_update" => {
            let mut res = update_obj.clone();
            if let Some(options) = update_obj.get("configOptions").and_then(Value::as_array) {
                res.insert(
                    "configOptions".to_string(),
                    Value::Array(v2_config_options(options)),
                );
            }
            Ok(Some(Value::Object(res)))
        }
        "session_info_update"
        | "usage_update"
        | "notice"
        | "compaction_update"
        | "compaction_summary_chunk" => Ok(Some(update.clone())),
        "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
            let mut res = update_obj.clone();
            let msg_id = res.shift_remove("messageId");
            let is_valid = match &msg_id {
                Some(Value::String(s)) => !s.is_empty(),
                _ => false,
            };
            if !is_valid {
                return Err(Error::new(
                    -32603,
                    format!("An ACP v2 {kind} needs a messageId"),
                ));
            }
            if let Some(id) = msg_id {
                res.insert("messageId".to_string(), id);
            }
            Ok(Some(Value::Object(res)))
        }
        "tool_call" | "tool_call_update" => {
            let tc = v2_tool_call_update(update)?;
            Ok(Some(rewrite_aborted_status(tc)))
        }
        "plan" => {
            let mut map = Map::new();
            map.insert(
                "sessionUpdate".to_string(),
                Value::String("plan_update".to_string()),
            );
            let mut plan_map = Map::new();
            plan_map.insert("type".to_string(), Value::String("items".to_string()));
            plan_map.insert(
                "planId".to_string(),
                Value::String(SESSION_PLAN_ID.to_string()),
            );
            if let Some(entries) = update_obj.get("entries") {
                plan_map.insert("entries".to_string(), entries.clone());
            }
            map.insert("plan".to_string(), Value::Object(plan_map));
            if let Some(meta) = update_obj.get("_meta") {
                if !meta.is_null() {
                    map.insert("_meta".to_string(), meta.clone());
                }
            }
            Ok(Some(Value::Object(map)))
        }
        "plan_update" | "plan_removed" => Ok(Some(update.clone())),
        "subagent_update" => {
            let mut res = update_obj.clone();
            let state_opt = res.shift_remove("state");
            if let Some(state) = state_opt {
                if !state.is_null() {
                    let validated = v2_subagent_state(&state)?;
                    res.insert("state".to_string(), validated);
                }
            }
            Ok(Some(Value::Object(res)))
        }
        "session_message" | "session_message_chunk" => Ok(Some(update.clone())),
        "current_mode_update" => Ok(None),
        _ => Err(Error::new(
            -32603,
            format!("The ACP v2 surface does not translate {kind} session updates yet"),
        )),
    }
}
