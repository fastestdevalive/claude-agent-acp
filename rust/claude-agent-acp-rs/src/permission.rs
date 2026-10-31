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

/// Everything `AcpToolCallRenderer.permissionToolCall` reads
/// (`tool-calls/renderer.ts:307-348`, `permissions/presentation.ts:60-136`).
#[derive(Debug, Clone, Default)]
pub struct PermissionToolCall {
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
    /// The subagent attribution of the request's owner (`toolUseOwner`).
    pub parent_tool_use_id: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    /// Whether the client is AIR: its `toolCall` is the minimal shape.
    pub air: bool,
    /// Whether the client reads the ExitPlanMode plan from `rawInput.planFilePath`
    /// (`jetbrains.air` `planFile`).
    pub plan_file: bool,
    /// Whether the client renders a command's terminal (`hasTerminal`): the
    /// content is then the terminal marker, not the display copy.
    pub terminal_output: bool,
    /// `options.title`: the subject of a network/computer-use request, else
    /// the tool call's own heading.
    pub title: Option<String>,
    /// `previewContent`: the exact patch a `diffPatch`/v2 client negotiated.
    pub preview_content: Option<Value>,
    /// `blockedPath`: a path the tool may not touch, added as an extra location
    /// when the tool call does not already carry it (P-53).
    pub blocked_path: Option<String>,
    /// `fallbackContent`: the JSON input shown when the call has no content.
    pub fallback_content: Option<Value>,
    /// The CLI's `mcpServer {name, source}` (SDK 0.3.274+).
    pub mcp_server: Option<Value>,
}

/// Build the `toolCall` object carried in a `session/request_permission`
/// (port of `AcpToolCallRenderer.permissionToolCall`, `renderer.ts:307-348`).
///
/// A client that is not AIR gets the whole tool call again (name, status,
/// rawInput, title, kind, content, locations, `_meta`); AIR gets the minimal
/// shape (`toolCallId`, `title`, `rawInput`, plus the preview content, the extra
/// locations and `_meta` when the request has them). The tool is shaped by
/// `tools::tool_shape`, so the heading and the facts match the streamed
/// `tool_call` (P-16).
pub fn build_permission_tool_call(input: &PermissionToolCall) -> Value {
    let cwd = input.cwd.as_deref();
    let shape = tools::tool_shape(&input.tool_name, &input.tool_input, cwd);
    let title = input.title.clone().unwrap_or_else(|| shape.title.clone());
    let meta = permission_tool_call_meta(
        input.air,
        &input.tool_name,
        input.parent_tool_use_id.as_deref(),
        input.mcp_server.as_ref(),
    );
    // `presentation.ts:95-106`: the blocked path is compacted to one line of at
    // most 4096 characters, and added only when the tool call's own locations do
    // not already name it.
    let blocked_path = input
        .blocked_path
        .as_deref()
        .and_then(|path| tools::human_text(path, 4_096, true));
    let extra_locations: Vec<Value> = match &blocked_path {
        Some(path)
            if !shape
                .locations
                .iter()
                .any(|location| location.path.to_string_lossy() == *path) =>
        {
            vec![json!({ "path": path })]
        }
        _ => Vec::new(),
    };
    if input.air {
        // Node's AIR branch (`renderer.ts:333-346`): the id, the title and
        // `rawInput` (without the file text a diff already holds); the client
        // already holds the streamed tool call. The preview patch, the extra
        // locations and `_meta` ride only when the request adds them.
        let mut tc = serde_json::Map::new();
        tc.insert(
            "toolCallId".into(),
            Value::String(input.tool_use_id.clone()),
        );
        tc.insert("title".into(), Value::String(title));
        tc.insert(
            "rawInput".into(),
            tools::air_raw_input_with(&input.tool_name, &input.tool_input, input.plan_file),
        );
        if let Some(preview) = &input.preview_content {
            tc.insert("content".into(), preview.clone());
        }
        if !extra_locations.is_empty() {
            let mut locations: Vec<Value> = shape
                .locations
                .iter()
                .filter_map(|location| serde_json::to_value(location).ok())
                .collect();
            locations.extend(extra_locations);
            tc.insert("locations".into(), Value::Array(locations));
        }
        if let Some(meta) = meta {
            tc.insert("_meta".into(), meta);
        }
        return Value::Object(tc);
    }
    let mut tc = serde_json::Map::new();
    tc.insert(
        "toolCallId".into(),
        Value::String(input.tool_use_id.clone()),
    );
    // E-9: the request_permission `toolCall` carries the programmatic tool name
    // and an explicit `status:"pending"` (the pinned crate's pending is the
    // default, so it is spelled out here), unlike the pre-0.94.0 shape.
    tc.insert("name".into(), Value::String(input.tool_name.clone()));
    tc.insert(
        "status".into(),
        serde_json::to_value(ToolCallStatus::Pending).unwrap_or(Value::Null),
    );
    tc.insert("rawInput".into(), input.tool_input.clone());
    // Node's key order (`renderer.ts:315-330`): title before kind.
    tc.insert("title".into(), Value::String(title));
    tc.insert(
        "kind".into(),
        serde_json::to_value(shape.kind).unwrap_or(Value::Null),
    );
    // `toolUseContent` (`renderer.ts:750-757`): the terminal marker of a
    // command the client renders, else the display copy / change the shape
    // already holds; the preview patch wins over both.
    let mut content = input.preview_content.clone().unwrap_or_else(|| {
        if input.terminal_output && crate::map::is_command_tool(&input.tool_name) {
            json!([{ "type": "terminal", "terminalId": input.tool_use_id }])
        } else {
            serde_json::to_value(&shape.content).unwrap_or_else(|_| json!([]))
        }
    });
    if content.as_array().is_some_and(Vec::is_empty) {
        if let Some(fallback) = &input.fallback_content {
            content = fallback.clone();
        }
    }
    tc.insert("content".into(), content);
    if !shape.locations.is_empty() || !extra_locations.is_empty() {
        let mut locations: Vec<Value> = shape
            .locations
            .iter()
            .filter_map(|location| serde_json::to_value(location).ok())
            .collect();
        locations.extend(extra_locations);
        tc.insert("locations".into(), Value::Array(locations));
    }
    if let Some(meta) = meta {
        tc.insert("_meta".into(), meta);
    }
    let mut val = Value::Object(tc);
    crate::map::materialize_tool_call_defaults(&mut val);
    crate::map::materialize_diff_old_text(&mut val);
    val
}

/// The `_meta` of a permission request's `toolCall` (`acp-agent.ts:10160-10168`):
/// `claudeCode` carries `toolName` and `parentToolUseId` for every client that
/// is not AIR, and the CLI's `mcpServer {name, source}` for every client that
/// has one. AIR already holds the tool name and the parent call, so its
/// `claudeCode` object carries only the MCP server (P-57).
fn permission_tool_call_meta(
    air: bool,
    tool_name: &str,
    parent_tool_use_id: Option<&str>,
    mcp_server: Option<&Value>,
) -> Option<Value> {
    if mcp_server.is_none() && (parent_tool_use_id.is_none() || air) {
        return None;
    }
    let mut claude_code = serde_json::Map::new();
    if !air {
        claude_code.insert("toolName".into(), Value::String(tool_name.to_string()));
        if let Some(parent) = parent_tool_use_id {
            claude_code.insert("parentToolUseId".into(), Value::String(parent.to_string()));
        }
    }
    if let Some(server) = mcp_server {
        let mut object = serde_json::Map::new();
        if let Some(name) = server.get("name").filter(|v| !v.is_null()) {
            object.insert("name".into(), name.clone());
        }
        if let Some(source) = server.get("source").filter(|v| !v.is_null()) {
            object.insert("source".into(), source.clone());
        }
        claude_code.insert("mcpServer".into(), Value::Object(object));
    }
    Some(json!({ "claudeCode": claude_code }))
}

/// The pre-presentation call shape: the whole tool call (or AIR's minimal one)
/// without a preview, extra locations or `_meta` beyond the parent attribution.
pub fn build_tool_call(
    tool_name: &str,
    tool_input: &Value,
    tool_use_id: &str,
    parent_tool_use_id: Option<&str>,
    cwd: Option<&std::path::Path>,
    air: bool,
) -> Value {
    build_permission_tool_call(&PermissionToolCall {
        tool_name: tool_name.to_string(),
        tool_input: tool_input.clone(),
        tool_use_id: tool_use_id.to_string(),
        parent_tool_use_id: parent_tool_use_id.map(String::from),
        cwd: cwd.map(std::path::Path::to_path_buf),
        air,
        ..Default::default()
    })
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
        tools::ShapeOptions {
            air: state.is_air(),
            diff_patch: state.diff_patch(),
            raw_input_rendering: state.caps_raw_input_rendering(),
            plan_file: state.plan_file(),
            ..tools::ShapeOptions::default()
        },
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
/// a cancelled / malformed / unknown selection is the SDK's error verdict
/// (`permissions/response.ts`, P-35/P-36), never a deny.
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
    // Node's `decodeClaudePermissionResponse` (`permissions/response.ts:10-36`):
    // a non-selected outcome, an id that was never offered, and an id no effect
    // knows all THROW. The SDK turns the throw into an error `control_response`
    // (`sdk.mjs` `handleControlRequest`), not into a deny verdict (P-35/P-36).
    let decoded = match crate::permission_response::decode_claude_permission_response(response, ctx)
    {
        Ok(decoded) => decoded,
        Err(error) => {
            return (
                crate::permission_response::error_control_response(request_id, &error),
                false,
            )
        }
    };
    let fallback = fallback_rewrite_applies(ctx, &decoded.outcome);
    let selection = match decoded.outcome {
        // Node `applyPermissionFallback` (`session-mode.ts:196-215`) runs on
        // every permission result: an allow that activates Auto on a model
        // without it is rewritten to Accept edits. The caller publishes the
        // fallback warning before the mode update.
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
    };
    let value = match selection {
        SelectionOutcome::Allow {
            updated_permissions,
            permanent,
        } => match (permanent, updated_permissions) {
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
        SelectionOutcome::Deny { interrupt, message } => {
            // ExitPlanMode's "keep planning" / clear-context choices interrupt the
            // turn (Node `effects.js`); the plain R28 deny does not.
            if interrupt {
                deny_with_interrupt_control_response(request_id, tool_use_id, &message)
            } else {
                deny_control_response(request_id, tool_use_id)
            }
        }
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
    /// The CLI's `blocked_path`: a path the tool may not touch, shown as an
    /// extra location on the permission `toolCall` (P-53).
    pub blocked_path: Option<String>,
    /// The CLI's `decision_reason`: shown as the `Reason:` description of the
    /// permission prompt (P-52).
    pub decision_reason: Option<String>,
    /// The CLI's `mcp_server {name, source}` of an `mcp__*` tool (SDK
    /// 0.3.274+), forwarded in `_meta.claudeCode` (P-57).
    pub mcp_server: Option<Value>,
}

/// JavaScript truthiness for a wire value (`sdk.mjs` spreads `matched_ask_rule`
/// only when it is truthy).
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(boolean) => *boolean,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
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
        // `matchedAskRule !== undefined` (`acp-agent.ts:10127`) is true only when
        // the SDK mapped the field, and the SDK spreads it only for a truthy
        // `matched_ask_rule` (`sdk.mjs` `processControlRequest`): a JSON null,
        // false, 0 or "" is absent, while `{}` or any other value is present
        // (P-41).
        no_persistent_rule: req.get("matched_ask_rule").is_some_and(js_truthy)
            || req.get("suppress_always_allow_rule") == Some(&Value::Bool(true)),
        display_name: req
            .get("display_name")
            .and_then(Value::as_str)
            .map(String::from),
        default_to_no: req.get("default_to_no") == Some(&Value::Bool(true)),
        blocked_path: req
            .get("blocked_path")
            .and_then(Value::as_str)
            .map(String::from),
        decision_reason: req
            .get("decision_reason")
            .and_then(Value::as_str)
            .map(String::from),
        // The SDK spreads `mcpServer` only for a truthy `mcp_server`
        // (`sdk.mjs` `processControlRequest`).
        mcp_server: req
            .get("mcp_server")
            .filter(|value| js_truthy(value))
            .cloned(),
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

    /// `acp-agent.test.ts` > "does not promise an ineffective always-allow
    /// fallback for a forced ask rule": a `matched_ask_rule` present on the
    /// wire suppresses every persistent option (P-41). The SDK maps the field
    /// only when it is truthy (`sdk.mjs` `processControlRequest`), so a null,
    /// false, 0 or "" is absent while `{}` is present.
    #[test]
    fn t6a2_matched_ask_rule_presence_test() {
        let can = |value: Value| {
            parse_can_use_tool(&json!({
                "type": "control_request",
                "request_id": "r1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "input": {"command": "terraform destroy"},
                    "tool_use_id": "tool-1",
                    "matched_ask_rule": value,
                }
            }))
        };
        assert!(
            can(json!({"source": "projectSettings", "tool_name": "Bash"})).no_persistent_rule,
            "a matched ask rule forces the prompt and suppresses the durable choice"
        );
        assert!(
            can(json!({})).no_persistent_rule,
            "an empty object is truthy"
        );
        for absent in [Value::Null, json!(false), json!(0), json!("")] {
            assert!(
                !can(absent.clone()).no_persistent_rule,
                "{absent} is falsy in JS, so the SDK never maps matchedAskRule"
            );
        }
        // `suppress_always_allow_rule === true` is its own, exact test.
        assert!(
            parse_can_use_tool(&json!({
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "input": {"command": "rm -rf build"},
                    "tool_use_id": "tool-1",
                    "suppress_always_allow_rule": true,
                }
            }))
            .no_persistent_rule
        );
        assert!(
            !parse_can_use_tool(&json!({
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "input": {"command": "rm -rf build"},
                    "tool_use_id": "tool-1",
                    "suppress_always_allow_rule": false,
                }
            }))
            .no_persistent_rule
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
        // The rejection is the SDK's error verdict (`subtype:"error"`), not a
        // deny (P-35).
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
        assert_eq!(value["response"]["subtype"], "error");
        assert_eq!(
            value["response"]["error"],
            json!("Permission option was not offered: exit-plan-bypass")
        );
        assert!(value["response"]["response"].is_null());
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
