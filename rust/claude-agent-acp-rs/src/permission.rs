//! Permission translation (phase 10): turn an inbound `can_use_tool`
//! control_request from `claude` into an ACP `session/request_permission` and
//! map the client's outcome back to the `control_response` the CLI expects.
//!
//! Wire truth is the Node adapter at the pinned version (`porting/UPSTREAM_VERSION`, currently 0.94.0) (`dist/acp-agent.js`): the
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
//! - `allow-with-updates` (3.9, Bash/PowerShell only) → `{behavior:"allow",
//!   updatedInput, updatedPermissions: <the CLI's suggestions verbatim>,
//!   decisionClassification:"user_permanent"}`; offered only when the
//!   suggestions are exactly what Node's `shellSuggestionsLabel` can describe.
//! - `reject` (or any other selection) → R28 deny payload (10.4).
//! - `cancelled` → R28 deny payload too (the tool is not allowed; the turn is
//!   left to settle on the next `result`, INV-21).

use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall, ToolCallStatus};
use serde_json::{json, Value};

use crate::map::{MapState, TrackedToolCall};
use crate::permission_options::{self, SelectionOutcome};
use crate::tools;

/// The R28 deny payload — the `canUseTool` return for a rejection, written as
/// the `control_response` so the CLI does not interrupt (INV-21).
pub(crate) const DENY_MESSAGE: &str = "User refused permission to run tool";

/// The outcome a client selected for a `session/request_permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `allow_once` — allow this one tool call.
    Allow,
    /// `allow-with-updates` — allow and persist the CLI's suggested rules (3.9).
    /// Only honoured when the durable option was actually offered.
    AllowWithUpdates,
    /// `reject_once` (or any non-allow selection) — refuse this tool call.
    Deny,
    /// The client cancelled the request (or the request errored/closed).
    Cancelled,
}

/// Map a `session/request_permission` response (`{outcome:{...}}`) to an
/// [`Outcome`]. A JSON-RPC error is mapped by the caller (the round-trip
/// resolves `Err`); this is only for a successfully-decoded response.
///
/// 2.11 (E-10) / 3.9: option ids are `allow-once` / `allow-with-updates` /
/// `reject`; anything else is a deny.
pub fn map_outcome(response: &Value) -> Outcome {
    // The wire shape is `{"outcome": {"outcome": "selected", "optionId": ".."}}`
    // (the response `result`); the flat `{"outcome": "selected", "optionId"}`
    // form is also accepted.
    let (kind, option) = match response.get("outcome") {
        Some(Value::Object(nested)) => (
            nested.get("outcome").and_then(Value::as_str).unwrap_or(""),
            nested.get("optionId").and_then(Value::as_str).unwrap_or(""),
        ),
        Some(Value::String(kind)) => (
            kind.as_str(),
            response
                .get("optionId")
                .and_then(Value::as_str)
                .unwrap_or(""),
        ),
        _ => ("", ""),
    };
    match kind {
        "cancelled" => Outcome::Cancelled,
        "selected" => match option {
            "allow-once" => Outcome::Allow,
            "allow-with-updates" => Outcome::AllowWithUpdates,
            // `reject` and any unknown id is a deny.
            _ => Outcome::Deny,
        },
        // A malformed or non-outcome response is treated as a deny.
        _ => Outcome::Deny,
    }
}

/// Build the `session/request_permission` params for a `can_use_tool` request.
///
/// `tool_call` is the `{toolCallId, rawInput, kind, title, content, ...}`
/// object describing the tool (see [`build_tool_call`]).
///
/// Options (E-10, 3.9): `[allow-once "Yes" allow_once, (allow-with-updates
/// <label> allow_always,) reject "No" reject_once]`; the middle option is
/// present only when `durable` is (see [`durable_bash`]). No `_meta.permission`.
pub fn build_request_permission(
    session_id: &str,
    tool_call: Value,
    durable: Option<&Durable>,
) -> Value {
    let mut options =
        vec![json!({ "kind": "allow_once", "name": "Yes", "optionId": "allow-once" })];
    if let Some(durable) = durable {
        options.push(json!({
            "kind": "allow_always",
            "name": durable.label,
            "optionId": "allow-with-updates",
        }));
    }
    options.push(json!({ "kind": "reject_once", "name": "No", "optionId": "reject" }));
    json!({
        "options": options,
        "sessionId": session_id,
        "toolCall": tool_call,
    })
}

/// The durable ("don't ask again") choice Node's Bash/PowerShell permission can
/// describe (3.9, `dist/permissions/options/shell.js`): the option label and
/// the CLI's suggestions, returned VERBATIM as `updatedPermissions` when chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Durable {
    /// The `allow-with-updates` option name, e.g.
    /// `Yes, and don't ask again for echo commands`.
    pub label: String,
    /// The validated `permission_suggestions` array.
    pub updates: Vec<Value>,
}

const MAX_UPDATES: usize = 100;
const MAX_RULES_PER_UPDATE: usize = 100;
const MAX_DIRECTORIES_PER_UPDATE: usize = 100;
const MAX_TOOL_NAME_LENGTH: usize = 512;
const MAX_RULE_CONTENT_LENGTH: usize = 10_000;
const MAX_DIRECTORY_LENGTH: usize = 4_096;

/// Node's `safePlainString`: non-blank, bounded, no control characters.
fn safe_plain_string(value: &Value, max_len: usize) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    !text.trim().is_empty()
        && text.encode_utf16().count() <= max_len
        && !text.trim().chars().any(|c| {
            let code = c as u32;
            code <= 0x08
                || code == 0x0b
                || code == 0x0c
                || (0x0e..=0x1f).contains(&code)
                || code == 0x7f
        })
}

/// Node's `isKnownUpdate` (`normalization.js`).
fn is_known_update(update: &Value) -> bool {
    let Some(obj) = update.as_object() else {
        return false;
    };
    let destination_ok = matches!(
        obj.get("destination").and_then(Value::as_str),
        Some("session" | "cliArg" | "userSettings" | "projectSettings" | "localSettings")
    );
    if !destination_ok {
        return false;
    }
    match obj.get("type").and_then(Value::as_str) {
        Some(kind @ ("addRules" | "replaceRules" | "removeRules")) => {
            let Some(rules) = obj.get("rules").and_then(Value::as_array) else {
                return false;
            };
            rules.len() <= MAX_RULES_PER_UPDATE
                && (kind == "replaceRules" || !rules.is_empty())
                && rules.iter().all(|rule| {
                    rule.as_object().is_some_and(|r| {
                        r.get("toolName")
                            .is_some_and(|t| safe_plain_string(t, MAX_TOOL_NAME_LENGTH))
                            && r.get("ruleContent")
                                .is_none_or(|c| safe_plain_string(c, MAX_RULE_CONTENT_LENGTH))
                    })
                })
                && matches!(
                    obj.get("behavior").and_then(Value::as_str),
                    Some("allow" | "deny" | "ask")
                )
        }
        Some("setMode") => matches!(
            obj.get("mode").and_then(Value::as_str),
            Some("default" | "acceptEdits" | "bypassPermissions" | "plan" | "dontAsk" | "auto")
        ),
        Some("addDirectories" | "removeDirectories") => obj
            .get("directories")
            .and_then(Value::as_array)
            .is_some_and(|dirs| {
                !dirs.is_empty()
                    && dirs.len() <= MAX_DIRECTORIES_PER_UPDATE
                    && dirs
                        .iter()
                        .all(|d| safe_plain_string(d, MAX_DIRECTORY_LENGTH))
            }),
        _ => false,
    }
}

/// Node's `permissionRulePrefix`: strip a trailing `:*`.
fn rule_prefix(value: &str) -> &str {
    value.strip_suffix(":*").unwrap_or(value)
}

/// Node's `displayList` (`shell.js`).
fn display_list(values: &[&str]) -> String {
    if values.join(", ").encode_utf16().count() > 50 {
        return "similar".to_string();
    }
    match values {
        [one] => (*one).to_string(),
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
        [] => String::new(),
    }
}

/// The durable Bash/PowerShell choice for a `can_use_tool` request, or `None`
/// when Node would offer only Yes/No (3.9; Decision 10).
///
/// `None` when: the tool is not `Bash`/`PowerShell`; the request carries a
/// `matched_ask_rule` or `suppress_always_allow_rule`
/// (`noPersistentRule`); the suggestions fail Node's validation
/// (`normalizeDurablePermissionChangeSet`); they are not representable
/// (`shellSuggestionsLabel`: only `allow` rules of this tool or `Read`, with
/// plain rule content, and session-scoped directories); or they carry read
/// paths / directories. Node would describe the path forms with a
/// path-shortening label that this port does not reproduce, so those fall
/// back to Yes/No (PARITY section C).
pub fn durable_bash(
    tool_name: &str,
    suggestions: &Value,
    no_persistent_rule: bool,
) -> Option<Durable> {
    if !matches!(tool_name, "Bash" | "PowerShell") || no_persistent_rule {
        return None;
    }
    let updates = suggestions.as_array()?;
    if updates.is_empty() || updates.len() > MAX_UPDATES || !updates.iter().all(is_known_update) {
        return None;
    }
    // shellSuggestionsLabel: representable?
    let mut commands: Vec<&str> = Vec::new();
    for update in updates {
        match update["type"].as_str() {
            Some("addDirectories") => return None, // path forms: Yes/No fallback
            Some("addRules") if update["behavior"] == "allow" => {
                for rule in update["rules"].as_array()? {
                    let name = rule["toolName"].as_str()?;
                    let content = rule["ruleContent"]
                        .as_str()
                        .filter(|c| !c.trim().is_empty())?;
                    if name == "Read" {
                        return None; // read paths: Yes/No fallback
                    }
                    if name != tool_name {
                        return None;
                    }
                    let prefix = rule_prefix(content);
                    if !prefix.is_empty() && !commands.contains(&prefix) {
                        commands.push(prefix);
                    }
                }
            }
            _ => return None,
        }
    }
    if commands.is_empty() {
        return None;
    }
    Some(Durable {
        label: format!(
            "Yes, and don't ask again for {} commands",
            display_list(&commands)
        ),
        updates: updates.clone(),
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
    cwd: Option<&std::path::Path>,
    air: bool,
) -> Value {
    let shape = tools::tool_shape(tool_name, tool_input, cwd);
    if air {
        // Node's AIR `permissionToolCall` (`renderer.ts:333-341`) sends only
        // the id, the title and `rawInput` (without the file text that a diff
        // holds); the client already holds the streamed tool call. It never
        // sends `name`/`status`/`kind`/`content`/`locations`/`_meta`.
        let mut tc = serde_json::Map::new();
        tc.insert("toolCallId".into(), Value::String(tool_use_id.to_string()));
        tc.insert("title".into(), Value::String(shape.title.clone()));
        tc.insert(
            "rawInput".into(),
            tools::air_raw_input(tool_name, tool_input),
        );
        return Value::Object(tc);
    }
    let mut tc = serde_json::Map::new();
    tc.insert("toolCallId".into(), Value::String(tool_use_id.to_string()));
    // E-9: the request_permission `toolCall` carries the programmatic tool name
    // and an explicit `status:"pending"` (the pinned crate's pending is the
    // default, so it is spelled out here), unlike the pre-0.94.0 shape.
    tc.insert("name".into(), Value::String(tool_name.to_string()));
    tc.insert(
        "status".into(),
        serde_json::to_value(ToolCallStatus::Pending).unwrap_or(Value::Null),
    );
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
    let mut val = Value::Object(tc);
    crate::map::materialize_tool_call_defaults(&mut val);
    crate::map::materialize_diff_old_text(&mut val);
    val
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
    cwd: Option<&std::path::Path>,
) -> Option<SessionUpdate> {
    // "A permission request comes only for a tool call that is about to run"
    // (`acp-agent.ts:9888-9889`): dispatched even when already emitted.
    state.books.record_dispatched_call(tool_use_id);
    if state.has_emitted(tool_use_id) {
        return None;
    }
    state.mark_emitted(
        tool_use_id.to_string(),
        json!({"type": "tool_use", "id": tool_use_id, "name": tool_name, "input": tool_input}),
    );

    // The same shape and `rawInput` the streamed path gives this client
    // (`AcpToolCallRenderer.toolCall`): AIR facts, a `diffPatch` client's Write
    // without its input diff, and for AIR the input without the file text that
    // the diff holds. The streamed refinement then carries the same `rawInput`,
    // so no extra update follows.
    let mut shape = tools::tool_shape_full(
        tool_name,
        tool_input,
        cwd,
        state.is_air(),
        false,
        state.diff_patch(),
    );
    state.apply_terminal_content(tool_name, tool_use_id, &mut shape);
    let raw_input = state.raw_input(tool_name, tool_input);
    // 2.9 (X-7): a surfaced `tool_call` seeds the field tracker.
    state.seed_tool_tracker(
        tool_use_id.to_string(),
        TrackedToolCall::initial(
            shape.kind,
            shape.title.clone(),
            shape.content.clone(),
            shape.locations.clone(),
            Some(raw_input.clone()),
        ),
    );
    let mut meta = state.use_meta(tool_name, tool_input);
    state.apply_terminal_info(tool_name, tool_use_id, &mut meta);
    if state.is_air() && tool_name.starts_with("mcp__") {
        meta.insert("is_mcp_tool_call".into(), Value::Bool(true));
    }
    if let Some(parent) = parent_tool_use_id {
        if let Some(cc) = meta.get_mut("claudeCode").and_then(Value::as_object_mut) {
            cc.insert("parentToolUseId".into(), Value::String(parent.to_string()));
        }
        // A call the permission flow surfaced for a subagent names its parent
        // (`recordToolCallParent` in `sendUpdate`, `acp-agent.ts:4556-4571`).
        state.record_tool_call_parent(tool_use_id, parent);
    }
    Some(SessionUpdate::ToolCall(
        ToolCall::new(tool_use_id.to_string(), shape.title)
            // E-9: surface the programmatic tool name on the emitted ToolCall.
            .name(tool_name.to_string())
            .kind(shape.kind)
            .status(ToolCallStatus::Pending)
            .content(shape.content)
            .locations(shape.locations)
            .raw_input(raw_input)
            .meta(meta),
    ))
}

/// The R28 deny `control_response`: `{behavior:"deny", message:...}` with no
/// `interrupt` (INV-21 / 10.4). 2.11 (E-11): gains `toolUseID` and
/// `decisionClassification:"user_reject"`; the deny message is unchanged.
pub fn deny_control_response(request_id: &str, tool_use_id: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "deny",
                "message": DENY_MESSAGE,
                "toolUseID": tool_use_id,
                "decisionClassification": "user_reject",
            },
        },
    })
}

/// An allow `control_response` for a one-shot allowance. 2.11 (E-11): gains
/// `toolUseID` and `decisionClassification:"user_temporary"`.
pub fn allow_control_response(request_id: &str, tool_use_id: &str, updated_input: &Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input,
                "toolUseID": tool_use_id,
                "decisionClassification": "user_temporary",
            },
        },
    })
}

/// The durable allow `control_response` (3.9): the CLI's suggestions are
/// returned verbatim as `updatedPermissions`, classified `user_permanent`.
pub fn allow_with_updates_control_response(
    request_id: &str,
    tool_use_id: &str,
    updated_input: &Value,
    updated_permissions: &[Value],
) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input,
                "updatedPermissions": updated_permissions,
                "toolUseID": tool_use_id,
                "decisionClassification": "user_permanent",
            },
        },
    })
}

/// An allow with `updatedPermissions` classified `user_temporary`: Node's shape
/// for an ExitPlanMode `default` setMode allow (`effects.js` `allow(..., false)`
/// with `updatedPermissions` present).
pub fn allow_temporary_with_updates_control_response(
    request_id: &str,
    tool_use_id: &str,
    updated_input: &Value,
    updated_permissions: &[Value],
) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input,
                "updatedPermissions": updated_permissions,
                "toolUseID": tool_use_id,
                "decisionClassification": "user_temporary",
            },
        },
    })
}

/// A deny `control_response` that interrupts the turn (`interrupt:true`):
/// Node's shape for ExitPlanMode's "keep planning" and clear-context choices
/// (`effects.js` `deny(..., true)`). The R28 deny does NOT interrupt.
pub fn deny_with_interrupt_control_response(
    request_id: &str,
    tool_use_id: &str,
    message: &str,
) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "deny",
                "message": message,
                "interrupt": true,
                "toolUseID": tool_use_id,
                "decisionClassification": "user_reject",
            },
        },
    })
}

/// Build the `control_response` for a `session/request_permission` result using
/// the P7 selection effects (3.6, Decision 7). Applies
/// [`permission_options::apply_selection`] to the client's selected option id;
/// a cancelled / malformed / unknown selection degrades to the R28 deny.
pub fn permission_control_response(
    response: &Value,
    ctx: &permission_options::OptionsContext,
    request_id: &str,
    tool_use_id: &str,
    input: &Value,
) -> Value {
    permission_control_response_with_fallback(response, ctx, request_id, tool_use_id, input).0
}

/// Node's `applyPermissionFallback` (`session-mode.ts:196-221`): whether an
/// allow's decoded `updatedPermissions` carries a `setMode auto` that the model
/// cannot honour. The caller rewrites it to Accept edits and publishes the
/// advisory warning for ANY tool (`acp-agent.ts:10219-10223`).
pub(crate) fn fallback_rewrite_applies(
    ctx: &permission_options::OptionsContext,
    outcome: &SelectionOutcome,
) -> bool {
    ctx.auto_unavailable
        && matches!(
            outcome,
            SelectionOutcome::Allow {
                updated_permissions: Some(updates),
                ..
            } if updates.iter().any(|update| {
                update.get("type").and_then(Value::as_str) == Some("setMode")
                    && update.get("mode").and_then(Value::as_str) == Some("auto")
            })
        )
}

/// Like [`permission_control_response`], but also reports whether Node's
/// `applyPermissionFallback` rewrote an Auto allow to Accept edits. The actor
/// uses the flag to publish the fallback warning for every rewrite (not only
/// the ExitPlanMode path).
pub fn permission_control_response_with_fallback(
    response: &Value,
    ctx: &permission_options::OptionsContext,
    request_id: &str,
    tool_use_id: &str,
    input: &Value,
) -> (Value, bool) {
    let option_id = match selected_option_id(response) {
        Some(id) => id,
        None => return (deny_control_response(request_id, tool_use_id), false),
    };
    // Node `decodeClaudePermissionResponse` (`permissions/response.js:5-8`): a
    // selected option id that was never offered is rejected exactly like Node's
    // thrown `Permission option was not offered` — refuse the tool.
    if !permission_options::option_was_offered(ctx, option_id) {
        return (deny_control_response(request_id, tool_use_id), false);
    }
    let selection = permission_options::apply_selection(option_id, ctx);
    let fallback = match &selection {
        Ok(outcome) => fallback_rewrite_applies(ctx, outcome),
        Err(_) => false,
    };
    let selection = selection.map(|outcome| {
        // Node `applyPermissionFallback` (`session-mode.ts:196-215`) runs on
        // every permission result: an allow that activates Auto on a model
        // without it is rewritten to Accept edits. The caller publishes the
        // fallback warning before the mode update.
        match outcome {
            SelectionOutcome::Allow {
                updated_permissions,
                permanent,
            } => SelectionOutcome::Allow {
                updated_permissions: updated_permissions.map(|updates| {
                    updates
                        .into_iter()
                        .map(|mut update| {
                            if ctx.auto_unavailable
                                && update.get("type").and_then(Value::as_str) == Some("setMode")
                                && update.get("mode").and_then(Value::as_str) == Some("auto")
                            {
                                if let Some(object) = update.as_object_mut() {
                                    object.insert("mode".into(), json!("acceptEdits"));
                                }
                            }
                            update
                        })
                        .collect()
                }),
                permanent,
            },
            other => other,
        }
    });
    let value = match selection {
        Ok(SelectionOutcome::Allow {
            updated_permissions,
            permanent,
        }) => match (permanent, updated_permissions) {
            // `permanent && Some(perms)` is every durable allow (allow-with-updates,
            // skill exact/prefix, WebFetch/generic generated rules).
            (true, Some(perms)) => {
                allow_with_updates_control_response(request_id, tool_use_id, input, &perms)
            }
            // `!permanent && Some(perms)` is an ExitPlanMode `default` setMode
            // allow: allow with updatedPermissions but classified user_temporary.
            (false, Some(perms)) => allow_temporary_with_updates_control_response(
                request_id,
                tool_use_id,
                input,
                &perms,
            ),
            // `!permanent && None` is the one-shot `allow-once`.
            (false, None) => allow_control_response(request_id, tool_use_id, input),
            (_, _) => deny_control_response(request_id, tool_use_id),
        },
        Ok(SelectionOutcome::Deny { interrupt, message }) => {
            // ExitPlanMode's "keep planning" / clear-context choices interrupt the
            // turn (Node `effects.js`); the plain R28 deny does not.
            if interrupt {
                deny_with_interrupt_control_response(request_id, tool_use_id, &message)
            } else {
                deny_control_response(request_id, tool_use_id)
            }
        }
        Err(_) => deny_control_response(request_id, tool_use_id),
    };
    (value, fallback)
}

/// Extract the chosen `optionId` from a `session/request_permission` response,
/// or `None` when the outcome was not `selected` (cancelled or malformed).
pub(crate) fn selected_option_id(response: &Value) -> Option<&str> {
    let nested = match response.get("outcome") {
        Some(Value::Object(nested)) => nested,
        _ => return None,
    };
    if nested.get("outcome").and_then(Value::as_str) != Some("selected") {
        return None;
    }
    nested.get("optionId").and_then(Value::as_str)
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
    /// `matched_ask_rule` present or `suppress_always_allow_rule == true`: no
    /// durable choice may be offered (Node `noPersistentRule`).
    pub no_persistent_rule: bool,
    /// The optional `display_name` label (Node `displayName`).
    pub display_name: Option<String>,
    /// Node's `defaultToNo` hint: lead with the reject options.
    pub default_to_no: bool,
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
        no_persistent_rule: req.get("matched_ask_rule").is_some_and(|v| !v.is_null())
            || req.get("suppress_always_allow_rule") == Some(&Value::Bool(true)),
        display_name: req
            .get("display_name")
            .and_then(Value::as_str)
            .map(String::from),
        default_to_no: req.get("default_to_no") == Some(&Value::Bool(true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rust-air-gaps 1.4 — the permission-path `tool_call` of an AIR session
    /// carries the AIR `_meta` (`commandTitle`, `mcp`, `is_mcp_tool_call`); a
    /// non-AIR session keeps `{claudeCode:{toolName}}` only.
    #[test]
    fn inv_51_permission_tool_call_carries_air_meta() {
        let meta_of = |update: Option<SessionUpdate>| -> Value {
            match update {
                Some(SessionUpdate::ToolCall(tc)) => Value::Object(tc.meta.unwrap_or_default()),
                other => panic!("expected a ToolCall, got {other:?}"),
            }
        };
        let mut air = MapState::default();
        air.configure_air(std::env::temp_dir());
        let bash = ensure_tool_call_emitted(
            "Bash",
            &json!({"command": "ls", "description": "List"}),
            "t1",
            Some("parent"),
            &mut air,
            None,
        );
        assert_eq!(
            meta_of(bash),
            json!({"claudeCode": {"toolName": "Bash", "parentToolUseId": "parent"},
                   "jetbrains": {"air": {"version": 1, "commandTitle": "List"}}})
        );
        let mcp = ensure_tool_call_emitted("mcp__s__t", &json!({}), "t2", None, &mut air, None);
        assert_eq!(
            meta_of(mcp),
            json!({"claudeCode": {"toolName": "mcp__s__t"},
                   "jetbrains": {"air": {"version": 1, "mcp": {"server": "s", "tool": "t"}}},
                   "is_mcp_tool_call": true})
        );
        let mut plain = MapState::default();
        let mcp = ensure_tool_call_emitted("mcp__s__t", &json!({}), "t3", None, &mut plain, None);
        assert_eq!(
            meta_of(mcp),
            json!({"claudeCode": {"toolName": "mcp__s__t"}})
        );
    }

    /// T1GA finding 5 — an AIR client's permission-surfaced `tool_call` and the
    /// request's own `toolCall` carry the `rawInput` the streamed path would send
    /// (a Write without its file text), and the tracker is seeded with it, so the
    /// streamed refinement that follows adds no `rawInput` update. A Write without
    /// a file path keeps its text (`fileTextKeys` is set only with a path).
    #[test]
    fn t1ga_early_permission_tool_call_is_air_shaped() {
        let input = json!({"file_path": "/work/a.txt", "content": "secret text\n"});
        let mut air = MapState::default();
        air.configure_air(std::env::temp_dir());
        let surfaced = ensure_tool_call_emitted("Write", &input, "t1", None, &mut air, None);
        let Some(SessionUpdate::ToolCall(tc)) = surfaced else {
            panic!("expected a ToolCall");
        };
        assert_eq!(tc.raw_input, Some(json!({"file_path": "/work/a.txt"})));
        // The streamed tool use of the same call: the same shape and `rawInput`,
        // so nothing differs from what the client holds.
        let streamed = air.tool_shape("Write", &input);
        assert_eq!(streamed.title, tc.title);
        assert_eq!(
            air.raw_input("Write", &input),
            json!({"file_path": "/work/a.txt"})
        );

        let request = build_tool_call("Write", &input, "t1", None, None, true);
        assert_eq!(request["rawInput"], json!({"file_path": "/work/a.txt"}));
        let pathless = json!({"content": "kept"});
        let request = build_tool_call("Write", &pathless, "t2", None, None, true);
        assert_eq!(request["rawInput"], pathless);

        // A plain client still gets the whole input.
        let mut plain = MapState::default();
        let surfaced = ensure_tool_call_emitted("Write", &input, "t3", None, &mut plain, None);
        let Some(SessionUpdate::ToolCall(tc)) = surfaced else {
            panic!("expected a ToolCall");
        };
        assert_eq!(tc.raw_input, Some(input));
    }

    /// 10.T1 (INV-20) — a request for an unseen tool id first emits the
    /// `ToolCall`; a second call for the same id is a no-op (the streamed
    /// `tool_use` refines it instead).
    #[test]
    fn inv_20_permission_after_toolcall() {
        let mut state = MapState::default();
        let input = json!({"command": "echo hi"});
        let first =
            ensure_tool_call_emitted("Bash", &input, "toolu_01PERM", None, &mut state, None);
        assert!(
            matches!(&first, Some(SessionUpdate::ToolCall(tc)) if tc.tool_call_id.to_string() == "toolu_01PERM"),
            "an unseen tool id must first emit a ToolCall"
        );
        let second =
            ensure_tool_call_emitted("Bash", &input, "toolu_01PERM", None, &mut state, None);
        assert!(
            second.is_none(),
            "a re-emission for an already-surfaced id must be a no-op (the streamed path refines)"
        );
    }

    /// 10.T2 (INV-21) — a deny maps to the exact R28 payload (with the 2.11
    /// `toolUseID` / `decisionClassification:"user_reject"` additions; the deny
    /// message is unchanged).
    #[test]
    fn inv_21_deny_payload_and_continue() {
        let denied = map_outcome(&json!({"outcome": "selected", "optionId": "reject"}));
        assert_eq!(denied, Outcome::Deny);
        // 2.11 / 3.9: `allow-once` is Allow; `allow-with-updates` is its own
        // outcome; anything else is Deny.
        assert_eq!(
            map_outcome(&json!({"outcome": "selected", "optionId": "allow-once"})),
            Outcome::Allow
        );
        assert_eq!(
            map_outcome(&json!({"outcome": "selected", "optionId": "allow-with-updates"})),
            Outcome::AllowWithUpdates
        );
        assert_eq!(
            map_outcome(&json!({"outcome": "selected", "optionId": "bogus"})),
            Outcome::Deny
        );
        let cancelled = map_outcome(&json!({"outcome": "cancelled"}));
        assert_eq!(cancelled, Outcome::Cancelled);
        // The real wire shape nests the outcome (`result.outcome.{outcome,optionId}`).
        assert_eq!(
            map_outcome(&json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}})),
            Outcome::Allow
        );
        assert_eq!(
            map_outcome(
                &json!({"outcome": {"outcome": "selected", "optionId": "allow-with-updates"}})
            ),
            Outcome::AllowWithUpdates
        );
        assert_eq!(
            map_outcome(&json!({"outcome": {"outcome": "cancelled"}})),
            Outcome::Cancelled
        );
        let response = deny_control_response("permreq1", "toolu_01PERM");
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
                        "toolUseID": "toolu_01PERM",
                        "decisionClassification": "user_reject",
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
            None,
            false,
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
            None,
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

    /// Review G14 finding 7 — Node's AIR `permissionToolCall`
    /// (`renderer.ts:333-341`) sends only `toolCallId`, `title` and `rawInput`
    /// (with the file text a diff already holds removed); no `locations:[]`,
    /// no `name`/`status`/`kind`/`content`/`_meta`.
    #[test]
    fn inv_79_air_permission_tool_call_has_node_shape() {
        let tc = build_tool_call(
            "Read",
            &json!({"file_path": "/w/a.txt"}),
            "toolu_air",
            None,
            Some(std::path::Path::new("/w")),
            true,
        );
        assert_eq!(
            tc,
            json!({
                "toolCallId": "toolu_air",
                "title": "Read a.txt",
                "rawInput": {"file_path": "/w/a.txt"},
            }),
            "AIR must not get name/status/kind/content/locations/_meta"
        );
        assert!(tc.get("locations").is_none());

        // Write's file text (the content alias that is present) is dropped.
        let write = build_tool_call(
            "Write",
            &json!({"file_path": "/w/a.txt", "file_text": "hello"}),
            "toolu_w",
            None,
            Some(std::path::Path::new("/w")),
            true,
        );
        assert_eq!(
            write["rawInput"],
            json!({"file_path": "/w/a.txt"}),
            "AIR rawInput drops the Write content key"
        );

        // Edit's snippet is dropped when the diff is built.
        let edit = build_tool_call(
            "Edit",
            &json!({"file_path": "/w/a.txt", "old_string": "a", "new_string": "b"}),
            "toolu_e",
            None,
            Some(std::path::Path::new("/w")),
            true,
        );
        assert_eq!(edit["rawInput"], json!({"file_path": "/w/a.txt"}));

        // Non-AIR keeps the whole tool call (and the materialised defaults).
        let non_air = build_tool_call(
            "Read",
            &json!({"file_path": "/w/a.txt"}),
            "toolu_n",
            None,
            Some(std::path::Path::new("/w")),
            false,
        );
        assert_eq!(non_air["status"], "pending");
        assert_eq!(non_air["name"], "Read");
        assert_eq!(
            non_air["locations"],
            json!([{"path": "/w/a.txt", "line": 1}])
        );
    }

    /// 2.11 (E-10): without a durable choice the request offers only Yes/No
    /// (no allow-always, no `_meta.permission`).
    #[test]
    fn inv_21_request_options_yes_no_only() {
        let params = build_request_permission(
            "sess1",
            build_tool_call(
                "Bash",
                &json!({"command":"ls"}),
                "toolu_1",
                None,
                None,
                false,
            ),
            None,
        );
        assert_eq!(
            params["options"],
            json!([
                { "kind": "allow_once", "name": "Yes", "optionId": "allow-once" },
                { "kind": "reject_once", "name": "No", "optionId": "reject" },
            ]),
            "2.11: request_permission options are Yes/No only"
        );
        assert!(
            params["options"][0].get("_meta").is_none(),
            "no _meta.permission on the options"
        );
    }

    fn add_rules(rules: Value) -> Value {
        json!([{"type":"addRules","rules":rules,"behavior":"allow","destination":"localSettings"}])
    }

    /// 3.9 — Node 0.94 verified label for a commands-only Bash suggestion, the
    /// three-option order and the verbatim `updatedPermissions` reply.
    #[test]
    fn inv_47_bash_durable_label() {
        let sug = add_rules(json!([{"toolName":"Bash","ruleContent":"echo:*"}]));
        let durable = durable_bash("Bash", &sug, false).expect("durable offered");
        assert_eq!(durable.label, "Yes, and don't ask again for echo commands");
        let params = build_request_permission("s", json!({}), Some(&durable));
        assert_eq!(
            params["options"],
            json!([
                { "kind": "allow_once", "name": "Yes", "optionId": "allow-once" },
                { "kind": "allow_always", "name": "Yes, and don't ask again for echo commands", "optionId": "allow-with-updates" },
                { "kind": "reject_once", "name": "No", "optionId": "reject" },
            ])
        );
        // displayList: 2 -> "a and b", 3 -> "a, b, and c", >50 chars -> "similar".
        let two = add_rules(json!([
            {"toolName":"Bash","ruleContent":"ls:*"},
            {"toolName":"Bash","ruleContent":"pwd"},
        ]));
        assert_eq!(
            durable_bash("Bash", &two, false).unwrap().label,
            "Yes, and don't ask again for ls and pwd commands"
        );
        let three = add_rules(json!([
            {"toolName":"Bash","ruleContent":"a"},
            {"toolName":"Bash","ruleContent":"b"},
            {"toolName":"Bash","ruleContent":"c"},
        ]));
        assert_eq!(
            durable_bash("Bash", &three, false).unwrap().label,
            "Yes, and don't ask again for a, b, and c commands"
        );
        let long = add_rules(json!([
            {"toolName":"Bash","ruleContent":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
        ]));
        assert_eq!(
            durable_bash("Bash", &long, false).unwrap().label,
            "Yes, and don't ask again for similar commands"
        );
        let resp = allow_with_updates_control_response(
            "r1",
            "toolu_1",
            &json!({"command":"echo hi"}),
            &durable.updates,
        );
        assert_eq!(resp["response"]["response"]["updatedPermissions"], sug);
        assert_eq!(
            resp["response"]["response"]["decisionClassification"],
            "user_permanent"
        );
        assert_eq!(resp["response"]["response"]["toolUseID"], "toolu_1");
    }

    /// 3.9 — anything Node cannot describe (or a request that forbids a
    /// persistent rule) offers Yes/No only.
    #[test]
    fn inv_47_unrepresentable_suggestions_fall_back() {
        let bash = json!({"toolName":"Bash","ruleContent":"echo:*"});
        let ok = add_rules(json!([bash]));
        assert!(durable_bash("Bash", &ok, false).is_some());
        assert!(
            durable_bash("Bash", &ok, true).is_none(),
            "matched ask rule / suppress"
        );
        assert!(
            durable_bash("Write", &ok, false).is_none(),
            "non-shell tool"
        );
        assert!(durable_bash("Bash", &json!([]), false).is_none(), "empty");
        assert!(
            durable_bash("Bash", &Value::Null, false).is_none(),
            "not an array"
        );
        // read paths / directories / non-allow / other tool / no content.
        let read = add_rules(json!([{"toolName":"Read","ruleContent":"/tmp/**"}]));
        assert!(durable_bash("Bash", &read, false).is_none());
        let dirs =
            json!([{"type":"addDirectories","directories":["/tmp"],"destination":"session"}]);
        assert!(durable_bash("Bash", &dirs, false).is_none());
        let deny = json!([{"type":"addRules","rules":[bash.clone()],"behavior":"deny","destination":"localSettings"}]);
        assert!(durable_bash("Bash", &deny, false).is_none());
        let other = add_rules(json!([{"toolName":"WebFetch","ruleContent":"domain:x"}]));
        assert!(durable_bash("Bash", &other, false).is_none());
        let bare = add_rules(json!([{"toolName":"Bash"}]));
        assert!(durable_bash("Bash", &bare, false).is_none());
        // Invalid destination fails Node's normalization.
        let bad =
            json!([{"type":"addRules","rules":[bash],"behavior":"allow","destination":"nowhere"}]);
        assert!(durable_bash("Bash", &bad, false).is_none());
    }

    /// Fix 1 + Fix 2 — `permission_control_response` maps an ExitPlanMode
    /// selection to Node's control_response (setMode for mode choices, an
    /// interrupt for "keep planning") and rejects an option id that was never
    /// offered.
    #[test]
    fn inv_60_exit_plan_control_response() {
        let ctx = |input: Value| permission_options::OptionsContext {
            tool_name: "ExitPlanMode".to_string(),
            display_name: None,
            input,
            cwd: "/repo".into(),
            change_set: None,
            no_persistent_rule: false,
            default_to_no: false,
            available_modes: vec![
                "default".into(),
                "acceptEdits".into(),
                "plan".into(),
                "auto".into(),
                "bypassPermissions".into(),
            ],
            auto_unavailable: false,
            pre_plan_mode: None,
            context_used_percent: None,
            home_dir: None,
        };
        let resp =
            |option_id: &str| json!({"outcome": {"outcome": "selected", "optionId": option_id}});
        // exit-plan-default -> allow + setMode default, user_temporary (was a
        // regression: it used to DENY).
        let value = permission_control_response(
            &resp("exit-plan-default"),
            &ctx(json!({})),
            "r1",
            "toolu_1",
            &json!({"plan": "x"}),
        );
        assert_eq!(value["response"]["response"]["behavior"], "allow");
        assert_eq!(
            value["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"default","destination":"session"}])
        );
        assert_eq!(
            value["response"]["response"]["decisionClassification"],
            "user_temporary"
        );
        assert!(value["response"]["response"]["interrupt"].is_null());

        // exit-plan-auto -> allow + setMode auto, user_permanent.
        let value = permission_control_response(
            &resp("exit-plan-auto"),
            &ctx(json!({})),
            "r1",
            "toolu_1",
            &json!({}),
        );
        assert_eq!(
            value["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"auto","destination":"session"}])
        );
        assert_eq!(
            value["response"]["response"]["decisionClassification"],
            "user_permanent"
        );

        // P-33: when the model lacks Auto, Node's `applyPermissionFallback`
        // rewrites the allow's `setMode auto` to Accept edits; the
        // classification stays permanent.
        let mut fallback_ctx = ctx(json!({}));
        fallback_ctx.auto_unavailable = true;
        let value = permission_control_response(
            &resp("exit-plan-auto"),
            &fallback_ctx,
            "r1",
            "toolu_1",
            &json!({}),
        );
        assert_eq!(
            value["response"]["response"]["updatedPermissions"],
            json!([{"type":"setMode","mode":"acceptEdits","destination":"session"}])
        );
        assert_eq!(
            value["response"]["response"]["decisionClassification"],
            "user_permanent"
        );

        // "keep planning" (reject) -> deny with interrupt:true.
        let value = permission_control_response(
            &resp("reject"),
            &ctx(json!({})),
            "r1",
            "toolu_1",
            &json!({}),
        );
        assert_eq!(value["response"]["response"]["behavior"], "deny");
        assert_eq!(value["response"]["response"]["interrupt"], true);

        // An option id that was never offered is rejected (Node `response.js`):
        // `exit-plan-bypass` is only offered when bypass is an available mode.
        let no_bypass_ctx = permission_options::OptionsContext {
            available_modes: vec!["default".into(), "acceptEdits".into(), "plan".into()],
            ..ctx(json!({}))
        };
        let value = permission_control_response(
            &resp("exit-plan-bypass"),
            &no_bypass_ctx,
            "r1",
            "toolu_1",
            &json!({}),
        );
        assert_eq!(value["response"]["response"]["behavior"], "deny");
        assert!(value["response"]["response"]["updatedPermissions"].is_null());
    }

    /// G2c-fix 4: Node's `applyPermissionFallback` rewrites and warns for ANY
    /// tool (`acp-agent.ts:10219-10223`, `session-mode.ts:196-221`), not only
    /// ExitPlanMode. The flag is derived from the decoded `updatedPermissions`,
    /// so a CLI-suggested `setMode auto` on any tool is reported.
    #[test]
    fn fallback_rewrite_applies_to_any_tool() {
        let ctx = |auto_unavailable: bool| permission_options::OptionsContext {
            tool_name: "Bash".to_string(),
            display_name: None,
            input: json!({"command": "ls"}),
            cwd: "/repo".into(),
            change_set: None,
            no_persistent_rule: false,
            default_to_no: false,
            available_modes: vec!["default".into(), "acceptEdits".into(), "auto".into()],
            auto_unavailable,
            pre_plan_mode: None,
            context_used_percent: None,
            home_dir: None,
        };
        let set_mode = |mode: &str| permission_options::SelectionOutcome::Allow {
            updated_permissions: Some(vec![json!({
                "type": "setMode",
                "mode": mode,
                "destination": "session",
            })]),
            permanent: true,
        };
        assert!(fallback_rewrite_applies(&ctx(true), &set_mode("auto")));
        assert!(!fallback_rewrite_applies(&ctx(false), &set_mode("auto")));
        assert!(!fallback_rewrite_applies(
            &ctx(true),
            &set_mode("acceptEdits")
        ));
        let deny = permission_options::SelectionOutcome::Deny {
            message: "no".into(),
            interrupt: false,
        };
        assert!(!fallback_rewrite_applies(&ctx(true), &deny));
    }
}
