//! Permission translation (phase 10): turn an inbound `can_use_tool`
//! control_request from `claude` into an ACP `session/request_permission` and
//! map the client's outcome back to the `control_response` the CLI expects.
//!
//! Wire truth is the Node adapter at `v0.70.0` (`dist/acp-agent.js`): the
//! `canUseTool` callback (`:4069-4275`) builds the `request_permission`
//! params, and the deny payload is the R28 constant
//! `{behavior:"deny", message:"User refused permission to run tool"}` with no
//! `interrupt`.
//!
//! Every item here is a pure function — the session actor owns the async
//! round-trip (D14): it spawns a task that sends `session/request_permission`
//! and never awaits a client reply inline (INV-27).
//!
//! ## Outcome mapping (10.3)
//! - `allow` → `{behavior:"allow", updatedInput}`
//! - `allow_always` → `{behavior:"allow", updatedInput, updatedPermissions}`,
//!   where the `updatedPermissions` default carries an `addRules` suggestion and
//!   the `_meta.permission` of the "Always Allow" option carries the matching
//!   rule changes (10.T6).
//! - `reject` (or any other selection) → R28 deny payload (10.4).
//! - `cancelled` → R28 deny payload too (the tool is not allowed; the turn is
//!   left to settle on the next `result`, INV-21).

use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall, ToolCallStatus};
use serde_json::{json, Value};

use crate::map::MapState;
use crate::tools;

/// The R28 deny payload — the `canUseTool` return for a rejection, written as
/// the `control_response` so the CLI does not interrupt (INV-21).
const DENY_MESSAGE: &str = "User refused permission to run tool";

/// The outcome a client selected for a `session/request_permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `allow_once` — allow this one tool call.
    Allow,
    /// `allow_always` — allow and remember (rule additions).
    AllowAlways,
    /// `reject_once` (or any non-allow selection) — refuse this tool call.
    Deny,
    /// The client cancelled the request (or the request errored/closed).
    Cancelled,
}

/// Map a `session/request_permission` response (`{outcome:{...}}`) to an
/// [`Outcome`]. A JSON-RPC error is mapped by the caller (the round-trip
/// resolves `Err`); this is only for a successfully-decoded response.
pub fn map_outcome(response: &Value) -> Outcome {
    let kind = response
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "cancelled" => Outcome::Cancelled,
        "selected" => {
            let option = response
                .get("optionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            match option {
                "allow_always" => Outcome::AllowAlways,
                "allow" => Outcome::Allow,
                // Any other selection (reject, reject_always, ...) is a deny.
                _ => Outcome::Deny,
            }
        }
        // A malformed or non-outcome response is treated as a deny.
        _ => Outcome::Deny,
    }
}

/// Build the `_meta.permission` rule-update object for the "Always Allow"
/// option (port of `permissionMetadataForAlwaysAllow`, `acp-agent.js:447-525`).
///
/// `suggestions` is the `can_use_tool` `permission_suggestions` array; when
/// empty the default single `addRules` suggestion for `tool_name` is used.
/// Returns `{version:1, changes:[...]}`.
pub fn permission_metadata_for_always_allow(suggestions: &Value, tool_name: &str) -> Value {
    let effective = if suggestions.get(0).is_some() {
        suggestions.clone()
    } else {
        json!([{
            "type": "addRules",
            "rules": [{ "toolName": tool_name }],
            "behavior": "allow",
            "destination": "session",
        }])
    };
    let effective = effective.as_array().cloned().unwrap_or_default();

    let mut changes = Vec::new();
    for update in &effective {
        let op = update.get("type").and_then(Value::as_str).unwrap_or("");
        match op {
            "addRules" | "removeRules" | "replaceRules" => {
                let operation = match op {
                    "removeRules" => "remove",
                    "replaceRules" => "replace",
                    _ => "add",
                };
                let behavior = update
                    .get("behavior")
                    .and_then(Value::as_str)
                    .unwrap_or("allow");
                let destination = update
                    .get("destination")
                    .and_then(Value::as_str)
                    .unwrap_or("session");
                let rules = update
                    .get("rules")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut targets = Vec::new();
                for rule in &rules {
                    let mut target = serde_json::Map::new();
                    target.insert("type".into(), Value::String("tool".into()));
                    target.insert(
                        "toolName".into(),
                        rule.get("toolName").cloned().unwrap_or(Value::Null),
                    );
                    if let Some(rc) = rule.get("ruleContent").and_then(Value::as_str) {
                        target.insert(
                            "matcher".into(),
                            json!({
                                "type": "provider_rule",
                                "provider": "claudeCode",
                                "value": rc,
                            }),
                        );
                    }
                    targets.push(Value::Object(target));
                }
                let rendered = rules
                    .iter()
                    .map(|rule| {
                        if let Some(rc) = rule.get("ruleContent").and_then(Value::as_str) {
                            format!(
                                "{} calls matching {}",
                                rule.get("toolName").and_then(Value::as_str).unwrap_or(""),
                                rc
                            )
                        } else {
                            format!(
                                "all {} calls",
                                rule.get("toolName").and_then(Value::as_str).unwrap_or("")
                            )
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let verb = match operation {
                    "add" => match behavior {
                        "deny" => "Deny".to_string(),
                        "ask" => "Ask before".to_string(),
                        _ => "Allow".to_string(),
                    },
                    "remove" => format!("Remove {behavior} rules for"),
                    _ => format!("Replace {behavior} rules with"),
                };
                changes.push(json!({
                    "type": "policy_rule",
                    "operation": operation,
                    "ruleBehavior": behavior,
                    "description": format!("{verb} {rendered}"),
                    "lifetime": permission_lifetime(destination),
                    "targets": targets,
                }));
            }
            "addDirectories" | "removeDirectories" => {
                let operation = if op == "removeDirectories" {
                    "remove"
                } else {
                    "add"
                };
                let destination = update
                    .get("destination")
                    .and_then(Value::as_str)
                    .unwrap_or("session");
                let dirs = update
                    .get("directories")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let dir_list = dirs
                    .iter()
                    .map(|d| d.as_str().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(", ");
                changes.push(json!({
                    "type": "policy_rule",
                    "operation": operation,
                    "ruleBehavior": "allow",
                    "description": if operation == "add" {
                        format!("Allow filesystem access under {dir_list}")
                    } else {
                        format!("Remove additional filesystem access under {dir_list}")
                    },
                    "lifetime": permission_lifetime(destination),
                    "targets": dirs.iter().map(|d| json!({
                        "type": "filesystem",
                        "matcher": { "type": "directory", "path": d },
                    })).collect::<Vec<_>>(),
                }));
            }
            "setMode" => {
                let destination = update
                    .get("destination")
                    .and_then(Value::as_str)
                    .unwrap_or("session");
                changes.push(json!({
                    "type": "permission_mode",
                    "operation": "set",
                    "provider": "claudeCode",
                    "mode": update.get("mode").cloned().unwrap_or(Value::Null),
                    "description": format!(
                        "Set Claude Code permission mode to {}",
                        update.get("mode").and_then(Value::as_str).unwrap_or("")
                    ),
                    "lifetime": permission_lifetime(destination),
                }));
            }
            _ => {}
        }
    }
    json!({ "version": 1, "changes": changes })
}

/// Port of `permissionLifetime` (`acp-agent.js:440-448`): the `lifetime`
/// object for a rule's `destination`.
fn permission_lifetime(destination: &str) -> Value {
    match destination {
        "user" => json!({ "scope": "user" }),
        "workspace" => json!({ "scope": "workspace" }),
        "project" => json!({ "scope": "persistent", "storage": "project" }),
        "localSettings" => json!({ "scope": "persistent", "storage": "project_local" }),
        // "session" (and anything unknown) default to a session-scoped lifetime.
        _ => json!({ "scope": "session" }),
    }
}

/// Build the `session/request_permission` params for a `can_use_tool` request.
///
/// `tool_call` is the `{toolCallId, rawInput, kind, title, content, ...}`
/// object describing the tool (see [`build_tool_call`]).
pub fn build_request_permission(
    session_id: &str,
    tool_call: Value,
    tool_name: &str,
    suggestions: &Value,
) -> Value {
    let meta = permission_metadata_for_always_allow(suggestions, tool_name);
    json!({
        "options": [
            { "kind": "reject_once", "name": "Deny", "optionId": "reject" },
            { "kind": "allow_once", "name": "Allow Once", "optionId": "allow" },
            {
                "kind": "allow_always",
                "name": "Always Allow",
                "optionId": "allow_always",
                "_meta": { "permission": meta },
            },
        ],
        "sessionId": session_id,
        "toolCall": tool_call,
    })
}

/// Build the `toolCall` object carried in a `session/request_permission`
/// (port of the `canUseTool` `toolCall` field, `acp-agent.js:4194-4211`).
///
/// The tool is shaped by `tools::tool_shape`, so `kind`/`title`/`content`/
/// `locations` match the streamed `tool_call`. A `parentToolUseId` (subagent
/// attribution) is carried in `_meta.claudeCode`, mirroring the streamed path.
pub fn build_tool_call(
    tool_name: &str,
    tool_input: &Value,
    tool_use_id: &str,
    parent_tool_use_id: Option<&str>,
) -> Value {
    let shape = tools::tool_shape(tool_name, tool_input);
    let mut tc = serde_json::Map::new();
    tc.insert("toolCallId".into(), Value::String(tool_use_id.to_string()));
    tc.insert("rawInput".into(), tool_input.clone());
    tc.insert(
        "kind".into(),
        serde_json::to_value(shape.kind).unwrap_or(Value::Null),
    );
    tc.insert("title".into(), Value::String(shape.title.clone()));
    tc.insert(
        "content".into(),
        serde_json::to_value(&shape.content).unwrap_or_else(|_| json!([])),
    );
    if !shape.locations.is_empty() {
        tc.insert(
            "locations".into(),
            serde_json::to_value(&shape.locations).unwrap_or_else(|_| json!([])),
        );
    }
    if let Some(parent) = parent_tool_use_id {
        tc.insert(
            "_meta".into(),
            json!({
                "claudeCode": {
                    "toolName": tool_name,
                    "parentToolUseId": parent,
                }
            }),
        );
    }
    Value::Object(tc)
}

/// Emit the `tool_call` a permission request references if the client has not
/// seen it yet (INV-20 / #851), port of `ensureToolCallEmitted`
/// (`acp-agent.js:4047-4071`).
///
/// Returns the `ToolCall` to surface, or `None` when the id was already
/// emitted (the streamed `tool_use` will refine it instead). The id is marked
/// emitted so the streamed path never emits a duplicate.
pub fn ensure_tool_call_emitted(
    tool_name: &str,
    tool_input: &Value,
    tool_use_id: &str,
    parent_tool_use_id: Option<&str>,
    state: &mut MapState,
) -> Option<SessionUpdate> {
    if state.has_emitted(tool_use_id) {
        return None;
    }
    state.mark_emitted(
        tool_use_id.to_string(),
        json!({"type": "tool_use", "id": tool_use_id, "name": tool_name, "input": tool_input}),
    );

    let shape = tools::tool_shape(tool_name, tool_input);
    let mut meta = tools::tool_meta(tool_name, tool_input);
    if let Some(parent) = parent_tool_use_id {
        if let Some(cc) = meta.get_mut("claudeCode").and_then(Value::as_object_mut) {
            cc.insert("parentToolUseId".into(), Value::String(parent.to_string()));
        }
    }
    Some(SessionUpdate::ToolCall(
        ToolCall::new(tool_use_id.to_string(), shape.title)
            .kind(shape.kind)
            .status(ToolCallStatus::Pending)
            .content(shape.content)
            .locations(shape.locations)
            .raw_input(tool_input.clone())
            .meta(meta),
    ))
}

/// The R28 deny `control_response`: `{behavior:"deny", message:...}` with no
/// `interrupt` (INV-21 / 10.4).
pub fn deny_control_response(request_id: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "deny",
                "message": DENY_MESSAGE,
            },
        },
    })
}

/// An allow `control_response` for a one-shot allowance.
pub fn allow_control_response(request_id: &str, updated_input: &Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input,
            },
        },
    })
}

/// An allow-always `control_response` carrying the `updatedPermissions` rule
/// additions (10.T6).
pub fn allow_always_control_response(
    request_id: &str,
    updated_input: &Value,
    tool_name: &str,
    suggestions: &Value,
) -> Value {
    let updated_permissions = if suggestions.get(0).is_some() {
        suggestions.clone()
    } else {
        json!([{
            "type": "addRules",
            "rules": [{ "toolName": tool_name }],
            "behavior": "allow",
            "destination": "session",
        }])
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input,
                "updatedPermissions": updated_permissions,
            },
        },
    })
}

/// Parse the fields of a `can_use_tool` `control_request` frame.
pub struct CanUseTool {
    /// The tool name.
    pub tool_name: String,
    /// The tool input object.
    pub tool_input: Value,
    /// The `tool_use_id` this request references.
    pub tool_use_id: String,
    /// The subagent `agent_id`, if the request originates inside a subagent.
    pub agent_id: Option<String>,
    /// The `permission_suggestions` array.
    pub suggestions: Value,
    /// The `request_id` to echo in the `control_response`.
    pub request_id: String,
}

/// Parse a `can_use_tool` `control_request` frame into its fields.
pub fn parse_can_use_tool(frame: &Value) -> CanUseTool {
    let req = frame.get("request").cloned().unwrap_or(Value::Null);
    let agent_id = req
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);
    CanUseTool {
        tool_name: req
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("Other")
            .to_string(),
        tool_input: req.get("input").cloned().unwrap_or(Value::Null),
        tool_use_id: req
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        agent_id,
        suggestions: req
            .get("permission_suggestions")
            .cloned()
            .unwrap_or_else(|| json!([])),
        request_id: frame
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10.T1 (INV-20) — a request for an unseen tool id first emits the
    /// `ToolCall`; a second call for the same id is a no-op (the streamed
    /// `tool_use` refines it instead).
    #[test]
    fn inv_20_permission_after_toolcall() {
        let mut state = MapState::default();
        let input = json!({"command": "echo hi"});
        let first = ensure_tool_call_emitted("Bash", &input, "toolu_01PERM", None, &mut state);
        assert!(
            matches!(&first, Some(SessionUpdate::ToolCall(tc)) if tc.tool_call_id.to_string() == "toolu_01PERM"),
            "an unseen tool id must first emit a ToolCall"
        );
        let second = ensure_tool_call_emitted("Bash", &input, "toolu_01PERM", None, &mut state);
        assert!(
            second.is_none(),
            "a re-emission for an already-surfaced id must be a no-op (the streamed path refines)"
        );
    }

    /// 10.T2 (INV-21) — a deny maps to the exact R28 payload.
    #[test]
    fn inv_21_deny_payload_and_continue() {
        let denied = map_outcome(&json!({"outcome": "selected", "optionId": "reject"}));
        assert_eq!(denied, Outcome::Deny);
        let cancelled = map_outcome(&json!({"outcome": "cancelled"}));
        assert_eq!(cancelled, Outcome::Cancelled);
        let response = deny_control_response("permreq1");
        assert_eq!(
            response,
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "permreq1",
                    "response": {
                        "behavior": "deny",
                        "message": "User refused permission to run tool",
                    },
                },
            })
        );
        assert!(
            response["response"]["response"]["interrupt"].is_null(),
            "the R28 deny must not carry an interrupt"
        );
    }

    /// 10.T5 — a subagent's permission request is attributed to its parent tool
    /// call: `build_tool_call` and `ensure_tool_call_emitted` carry the
    /// `parentToolUseId` in `_meta.claudeCode`.
    #[test]
    fn subagent_permission_attributed_to_parent() {
        let tc = build_tool_call(
            "Bash",
            &json!({"command":"ls"}),
            "toolu_SUB",
            Some("toolu_PARENT"),
        );
        assert_eq!(
            tc["_meta"]["claudeCode"]["parentToolUseId"],
            json!("toolu_PARENT")
        );
        assert_eq!(tc["_meta"]["claudeCode"]["toolName"], json!("Bash"));

        let mut state = MapState::default();
        let emitted = ensure_tool_call_emitted(
            "Bash",
            &json!({"command":"ls"}),
            "toolu_SUB",
            Some("toolu_PARENT"),
            &mut state,
        );
        let SessionUpdate::ToolCall(tc) = emitted.unwrap() else {
            panic!("expected a ToolCall");
        };
        let meta = serde_json::to_value(tc.meta).unwrap();
        assert_eq!(
            meta["claudeCode"]["parentToolUseId"],
            json!("toolu_PARENT"),
            "a subagent tool_call must be attributed to its parent"
        );
    }

    /// 10.T6 — allow-always emits the `_meta.permission` rule additions.
    #[test]
    fn allow_always_emits_permission_rule_additions() {
        let meta = permission_metadata_for_always_allow(&json!([]), "Bash");
        assert_eq!(meta["version"], 1);
        let change = &meta["changes"][0];
        assert_eq!(change["type"], "policy_rule");
        assert_eq!(change["operation"], "add");
        assert_eq!(change["ruleBehavior"], "allow");
        assert_eq!(change["description"], "Allow all Bash calls");
        assert_eq!(change["lifetime"], json!({"scope": "session"}));
        assert_eq!(
            change["targets"],
            json!([{"type": "tool", "toolName": "Bash"}])
        );

        let response =
            allow_always_control_response("r1", &json!({"command":"ls"}), "Bash", &json!([]));
        assert_eq!(response["response"]["response"]["behavior"], "allow");
        assert_eq!(
            response["response"]["response"]["updatedPermissions"],
            json!([{
                "type": "addRules",
                "rules": [{ "toolName": "Bash" }],
                "behavior": "allow",
                "destination": "session",
            }])
        );

        let params = build_request_permission(
            "sess1",
            build_tool_call("Bash", &json!({"command":"ls"}), "toolu_1", None),
            "Bash",
            &json!([]),
        );
        assert_eq!(params["options"][2]["optionId"], "allow_always");
        assert_eq!(
            params["options"][2]["_meta"]["permission"], meta,
            "the Always Allow option carries the _meta.permission rule additions"
        );
    }
}
