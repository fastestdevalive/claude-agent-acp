//! Claude message → ACP `SessionUpdate` mapping (Decision D3 / R9, phase 7).
//!
//! This module is the single place that turns a `claude` stdout frame (a
//! consolidated `assistant`/`user` message or a `stream_event`) into the ACP
//! `SessionUpdate` values the agent emits to the client. It is the wire truth
//! for the eight emitted variants (B1, R9) and never produces the three
//! discarded ones.
//!
//! Emitted (`SessionUpdate` variants, R9): `AgentMessageChunk`,
//! `AgentThoughtChunk`, `UserMessageChunk`, `ToolCall`, `ToolCallUpdate`,
//! `CurrentModeUpdate`, `AvailableCommandsUpdate`, `Plan`.
//!
//! Never emitted (`skipped-deliberate`, B1): `SessionInfoUpdate`,
//! `ConfigOptionUpdate`, `UsageUpdate`.
//!
//! The mapper is deliberately **stateful** ([`MapState`]): it must remember
//! which tool calls have already surfaced to the client so a block present in
//! both a `stream_event` and the later consolidated message emits exactly one
//! `ToolCall` (7.7 / INV-18), and it must accumulate partial tool input across
//! `input_json_delta` fragments to refine rather than duplicate a call (7.4 /
//! INV-19).
//!
//! The state lives in a single struct passed `&mut` through the pure mapping
//! functions — the session actor owns one `MapState` and hands it in as it
//! processes each frame. Nothing here is async and nothing here holds a lock;
//! it is a plain transformation unit, so it is exhaustively unit-tested.
//!
//! Source of truth (Node adapter `v0.70.0`, `dist/acp-agent.js`):
//! `toAcpNotifications` at lines 6383–6759, `streamEventToAcpNotifications` at
//! 6760–6864, the partial-JSON lexer at 179–232.

use std::collections::{HashMap, HashSet};

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ContentBlock, ContentChunk,
    CurrentModeUpdate, ImageContent, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    SessionModeId, SessionUpdate, TextContent, ToolCall, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, UnstructuredCommandInput,
};
use serde_json::Value;

use crate::tools;

/// Whether a consolidated message is the assistant's or the user's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgRole {
    /// The assistant message stream.
    Assistant,
    /// The user message stream (echoed back by `--replay-user-messages`).
    User,
}

/// Mutable cross-frame state the mapper threads through its pure functions.
///
/// - `emitted` — tool-call ids for which a `ToolCall` has already been
///   surfaced to the client (streamed block, consolidated message, or a
///   permission request). Used to refine (7.4) and dedupe (7.7).
/// - `tool_use_cache` — the most recent `tool_use` block by id, so a later
///   `tool_result` can resolve the tool name and drop plan-lane tools.
/// - `streamed_inputs` — accumulated partial JSON input per stream lane and
///   content index (7.4). Keyed by `parent_tool_use_id`/`""` then block index.
#[derive(Debug, Default)]
pub struct MapState {
    emitted: HashSet<String>,
    tool_use_cache: HashMap<String, Value>,
    streamed_inputs: HashMap<(String, u64), StreamedInput>,
}

/// Accumulator for a still-streaming tool input (7.4), mirroring the Node
/// adapter's per-`(streamKey, index)` record (`acp-agent.js:6784-6795`).
#[derive(Debug, Clone)]
struct StreamedInput {
    id: String,
    name: String,
    partial_json: String,
    scanned_to: usize,
    in_string: bool,
    escaped: bool,
    object_depth: usize,
    array_depth: usize,
    last_top_level_comma: isize,
    emitted_through_comma: isize,
}

/// Map a consolidated Claude message's `content` into ACP `SessionUpdate`s
/// (7.1, 7.3, 7.5, 7.6). `content` is either a JSON string (a whole-message
/// text) or a JSON array of content blocks — the two shapes `claude` emits.
pub fn map_consolidated(
    content: &Value,
    role: MsgRole,
    state: &mut MapState,
) -> Vec<SessionUpdate> {
    let mut out = Vec::new();
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(text_chunk(s, role));
            }
            return out;
        }
        Value::Array(blocks) => {
            for block in blocks {
                map_block(block, role, state, &mut out);
            }
        }
        _ => {}
    }
    out
}

/// Map one consolidated content block into `out` (7.1, 7.3, 7.5, 7.6).
fn map_block(block: &Value, role: MsgRole, state: &mut MapState, out: &mut Vec<SessionUpdate>) {
    let name = block["name"].as_str().unwrap_or("");
    match block["type"].as_str() {
        Some("tool_use") | Some("server_tool_use") | Some("mcp_tool_use") => {
            if name == "TodoWrite" {
                if let Some(plan) = plan_from_todo_write(block) {
                    out.push(plan);
                }
            } else if is_task_tool(name) {
                // Task* tool_use is suppressed; its plan snapshot is emitted at
                // tool_result time (Node `toAcpNotifications`, isTaskTool branch).
            } else {
                out.push(tool_call_from_use(block, state));
            }
        }
        Some("tool_result")
        | Some("tool_search_tool_result")
        | Some("web_fetch_tool_result")
        | Some("web_search_tool_result")
        | Some("code_execution_tool_result")
        | Some("bash_code_execution_tool_result")
        | Some("text_editor_code_execution_tool_result")
        | Some("mcp_tool_result") => {
            let id = block["tool_use_id"].as_str().unwrap_or("");
            let tool_name = state
                .tool_use_cache
                .get(id)
                .and_then(|v| v["name"].as_str());
            if tool_name == Some("TodoWrite") {
                // Plan-lane tool results never emit a tool_call_update (the
                // plan was already emitted at tool_use time).
                state.tool_use_cache.remove(id);
            } else {
                out.push(tool_update_from_result(block, state));
            }
        }
        _ => {
            if let Some(update) = chunk_from_block(block, role) {
                out.push(update);
            }
        }
    }
}

/// 7.3 — a `tool_use` block becomes a `ToolCall` on first surface, or a
/// refining `ToolCallUpdate` when the id was already emitted (7.7 dedupe).
/// Phase 9: shapes `kind`/`title`/`locations`/`content` and the `_meta`
/// `claudeCode` from `tools::tool_shape`/`tools::tool_meta`.
fn tool_call_from_use(block: &Value, state: &mut MapState) -> SessionUpdate {
    let id = block["id"].as_str().unwrap_or("").to_string();
    let name = block["name"].as_str().unwrap_or("Other").to_string();
    let raw_input = block.get("input").cloned();

    if state.emitted.contains(&id) {
        // Already surfaced (streamed block, or consolidated after streaming) —
        // refine rather than emit a duplicate ToolCall (7.7).
        return tool_call_update_refine(id, &name, raw_input);
    }
    state.emitted.insert(id.clone());
    state.tool_use_cache.insert(id.clone(), block.clone());

    let input = raw_input.clone().unwrap_or(Value::Null);
    let shape = tools::tool_shape(&name, &input);
    SessionUpdate::ToolCall(
        ToolCall::new(id, shape.title)
            .kind(shape.kind)
            .status(ToolCallStatus::Pending)
            .content(shape.content)
            .locations(shape.locations)
            .raw_input(raw_input)
            .meta(tools::tool_meta(&name, &input)),
    )
}

/// 7.3 — a `tool_result` block becomes a completing `ToolCallUpdate`.
fn tool_update_from_result(block: &Value, state: &mut MapState) -> SessionUpdate {
    let id = block["tool_use_id"].as_str().unwrap_or("").to_string();
    let name = state
        .tool_use_cache
        .get(&id)
        .and_then(|v| v["name"].as_str())
        .unwrap_or("Other")
        .to_string();
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };

    // The call is resolved — never emit it again, and drop the cache entry so
    // a long session doesn't retain every tool call.
    state.emitted.remove(&id);
    state.tool_use_cache.remove(&id);

    let raw_output = block.get("content").cloned();
    let result_input = block.get("content").cloned().unwrap_or(Value::Null);
    let mut fields = ToolCallUpdateFields::new().status(status);
    if let Some(content) = tools::tool_result_content(&name, &result_input, is_error) {
        fields = fields.content(content);
    }
    if let Some(raw_output) = raw_output {
        fields = fields.raw_output(raw_output);
    }

    SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id, fields).meta(tools::tool_meta(&name, &result_input)),
    )
}

/// 7.3/7.7 — a refining `ToolCallUpdate` for an already-surfaced tool call.
/// Phase 9: applies the shape (`kind`/`title`/`content`/`locations`) plus the
/// `_meta` `claudeCode`.
fn tool_call_update_refine(id: String, name: &str, raw_input: Option<Value>) -> SessionUpdate {
    let input = raw_input.clone().unwrap_or(Value::Null);
    let shape = tools::tool_shape(name, &input);
    let mut fields = ToolCallUpdateFields::new()
        .kind(shape.kind)
        .title(shape.title)
        .content(shape.content);
    if !shape.locations.is_empty() {
        fields = fields.locations(shape.locations);
    }
    if let Some(raw_input) = raw_input {
        fields = fields.raw_input(raw_input);
    }
    SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id, fields).meta(tools::tool_meta(name, &input)),
    )
}

/// 7.1 — a text/thinking/image block becomes a chunk update.
fn chunk_from_block(block: &Value, role: MsgRole) -> Option<SessionUpdate> {
    match block["type"].as_str() {
        Some("text") | Some("text_delta") => {
            let text = block["text"].as_str()?;
            // Node skips empty text chunks (`toAcpNotifications`: `chunk.text &&
            // ...`); a `content_block_start` text block with `""` must not
            // surface an empty `agent_message_chunk`.
            if text.is_empty() {
                return None;
            }
            Some(text_chunk(text, role))
        }
        Some("thinking") | Some("thinking_delta") => {
            let thinking = block["thinking"].as_str()?;
            Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(thinking)),
            )))
        }
        Some("image") => {
            let source = block.get("source")?;
            if source["type"].as_str() == Some("base64") {
                Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Image(ImageContent::new(
                        source["data"].as_str().unwrap_or(""),
                        source["media_type"].as_str().unwrap_or(""),
                    )),
                )))
            } else {
                // Non-base64 images are dropped (Node: `type === "base64"` gate).
                None
            }
        }
        _ => None,
    }
}

/// 7.1 — a plain string becomes an agent or user message chunk by role.
fn text_chunk(text: &str, role: MsgRole) -> SessionUpdate {
    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    match role {
        MsgRole::Assistant => SessionUpdate::AgentMessageChunk(chunk),
        MsgRole::User => SessionUpdate::UserMessageChunk(chunk),
    }
}

/// 7.5 — a `TodoWrite` tool_use becomes a `Plan` snapshot.
fn plan_from_todo_write(block: &Value) -> Option<SessionUpdate> {
    let todos = block.get("input")?.get("todos")?.as_array()?;
    let entries = todos
        .iter()
        .map(|todo| {
            let content = if todo["status"].as_str() == Some("in_progress")
                && !todo["activeForm"].as_str().unwrap_or("").is_empty()
            {
                todo["activeForm"].as_str().unwrap_or("").to_string()
            } else {
                todo["content"].as_str().unwrap_or("").to_string()
            };
            let status = match todo["status"].as_str() {
                Some("in_progress") => PlanEntryStatus::InProgress,
                Some("completed") => PlanEntryStatus::Completed,
                _ => PlanEntryStatus::Pending,
            };
            PlanEntry::new(content, PlanEntryPriority::Medium, status)
        })
        .collect();
    Some(SessionUpdate::Plan(Plan::new(entries)))
}

/// 7.5 — a `commands_changed` system frame becomes an `AvailableCommandsUpdate`.
///
/// Port of `getAvailableSlashCommands` (`acp-agent.js:6046-6082`): drops
/// terminal-bound commands, renames `(MCP)` names to `mcp:`, and filters the
/// terminal-only unsupported list.
pub fn map_commands_changed(
    commands: &Value,
    terminal_commands: &Value,
) -> AvailableCommandsUpdate {
    const UNSUPPORTED: &[&str] = &[
        "clear",
        "cost",
        "keybindings-help",
        "login",
        "logout",
        "output-style:new",
        "release-notes",
        "todos",
    ];
    let terminal_names: HashSet<&str> = terminal_commands
        .as_array()
        .map(|arr| arr.iter().filter_map(|c| c["name"].as_str()).collect())
        .unwrap_or_default();

    let mut avail = Vec::new();
    if let Some(list) = commands.as_array() {
        for command in list {
            let Some(name) = command["name"].as_str() else {
                continue;
            };
            if terminal_names.contains(name) {
                continue;
            }
            let final_name = match name.strip_suffix(" (MCP)") {
                Some(rest) => format!("mcp:{rest}"),
                None => name.to_string(),
            };
            if UNSUPPORTED.contains(&final_name.as_str()) {
                continue;
            }
            let description = command["description"].as_str().unwrap_or("").to_string();
            let mut built = AvailableCommand::new(final_name, description);
            if let Some(hint) = command.get("argumentHint") {
                let hint = match hint {
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                if !hint.is_empty() {
                    built = built.input(AvailableCommandInput::Unstructured(
                        UnstructuredCommandInput::new(hint),
                    ));
                }
            }
            avail.push(built);
        }
    }
    AvailableCommandsUpdate::new(avail)
}

/// 7.5 — a mode change becomes a `CurrentModeUpdate`.
pub fn map_mode_update(mode_id: impl Into<SessionModeId>) -> SessionUpdate {
    SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(mode_id))
}

/// Map a `stream_event` frame into ACP `SessionUpdate`s (7.2, 7.4).
pub fn map_stream_event(message: &Value, state: &mut MapState) -> Vec<SessionUpdate> {
    let event = &message["event"];
    let stream_key = message["parent_tool_use_id"]
        .as_str()
        .unwrap_or("")
        .to_string();

    match event["type"].as_str() {
        Some("content_block_start") => {
            let block = &event["content_block"];
            let index = event["index"].as_u64().unwrap_or(0);
            if is_tool_use_block(block) {
                register_streamed_input(state, &stream_key, index, block);
            }
            // The block itself is mapped like a one-block consolidated
            // assistant message (7.1/7.3) — the tool_use becomes a ToolCall and
            // is marked emitted so the consolidated message dedupes (7.7).
            map_consolidated(
                &Value::Array(vec![block.clone()]),
                MsgRole::Assistant,
                state,
            )
        }
        Some("content_block_delta") => {
            let delta = &event["delta"];
            if delta["type"].as_str() == Some("input_json_delta") {
                let index = event["index"].as_u64().unwrap_or(0);
                input_json_delta(state, &stream_key, index, delta)
            } else {
                // A text/thinking delta maps like a one-block assistant message.
                map_consolidated(
                    &Value::Array(vec![delta.clone()]),
                    MsgRole::Assistant,
                    state,
                )
            }
        }
        Some("content_block_stop") | Some("message_start") | Some("message_stop") => {
            // A message boundary ends the input stream on this lane; drop any
            // leftover partial input so the next message starts clean.
            state
                .streamed_inputs
                .retain(|(key, _), _| *key != stream_key);
            Vec::new()
        }
        // `ping` and `message_delta` are keep-alive/boundary no-ops.
        _ => Vec::new(),
    }
}

/// Whether a `content_block_start` block is a tool_use block to track.
fn is_tool_use_block(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("tool_use") | Some("server_tool_use") | Some("mcp_tool_use")
    )
}

/// 7.4 — register a new per-lane, per-index partial-input accumulator.
fn register_streamed_input(state: &mut MapState, stream_key: &str, index: u64, block: &Value) {
    state.streamed_inputs.insert(
        (stream_key.to_string(), index),
        StreamedInput {
            id: block["id"].as_str().unwrap_or("").to_string(),
            name: block["name"].as_str().unwrap_or("").to_string(),
            partial_json: String::new(),
            scanned_to: 0,
            in_string: false,
            escaped: false,
            object_depth: 0,
            array_depth: 0,
            last_top_level_comma: -1,
            emitted_through_comma: -1,
        },
    );
}

/// 7.4 — append an `input_json_delta` fragment and refine the pending call when
/// a recoverable top-level comma is crossed. Never emits a duplicate `ToolCall`
/// (INV-19).
fn input_json_delta(
    state: &mut MapState,
    stream_key: &str,
    index: u64,
    delta: &Value,
) -> Vec<SessionUpdate> {
    let Some(streamed) = state
        .streamed_inputs
        .get_mut(&(stream_key.to_string(), index))
    else {
        return Vec::new();
    };
    streamed
        .partial_json
        .push_str(delta["partial_json"].as_str().unwrap_or(""));

    if scan_streamed_input(streamed) {
        // Input complete: the consolidated assistant message replays the block
        // with its full input and refines there; emitting here too would send a
        // duplicate identical update.
        state
            .streamed_inputs
            .remove(&(stream_key.to_string(), index));
        return Vec::new();
    }
    if streamed.last_top_level_comma <= streamed.emitted_through_comma {
        return Vec::new();
    }
    streamed.emitted_through_comma = streamed.last_top_level_comma;
    let comma = streamed.last_top_level_comma as usize;
    let Some(input) = recovered_tool_input(&streamed.partial_json[..comma]) else {
        return Vec::new();
    };
    // Refine, never duplicate: this is a tool_call_update, not a tool_call.
    vec![streamed_input_refinement(streamed, input)]
}

/// Build a refining `ToolCallUpdate` for a partially-recovered tool input.
/// Phase 9: applies `kind`/`title`/`locations` from the shape but never
/// `content` (content built from partial input is misleading; the consolidated
/// message supplies it moments later — mirrors `streamedInputRefinement`).
fn streamed_input_refinement(streamed: &StreamedInput, input: Value) -> SessionUpdate {
    let name = streamed.name.clone();
    let shape = tools::tool_shape(&name, &input);
    let mut fields = ToolCallUpdateFields::new()
        .kind(shape.kind)
        .title(shape.title)
        .raw_input(input);
    if !shape.locations.is_empty() {
        fields = fields.locations(shape.locations);
    }
    SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(streamed.id.clone(), fields)
            .meta(tools::tool_meta(&name, &streamed_input_value(streamed))),
    )
}

/// The accumulated partial input as a JSON value (best-effort; the shape's meta
/// only needs the name/description fields, so an incomplete parse is fine).
fn streamed_input_value(streamed: &StreamedInput) -> Value {
    serde_json::from_str(&streamed.partial_json).unwrap_or(Value::Null)
}

/// The incremental JSON-prefix lexer (7.4), ported from `scanStreamedToolInput`
/// (`acp-agent.js:179-219`). Scans new characters, tracking string/escape state
/// and object/array depth, and records the most recent top-level comma. Returns
/// `true` when the object closes back to depth 0.
fn scan_streamed_input(state: &mut StreamedInput) -> bool {
    let mut complete = false;
    let s = &state.partial_json;
    let mut i = state.scanned_to;
    while i < s.len() {
        let Some(ch) = s[i..].chars().next() else {
            break;
        };
        let char_len = ch.len_utf8();
        if state.in_string {
            if state.escaped {
                state.escaped = false;
            } else if ch == '\\' {
                state.escaped = true;
            } else if ch == '"' {
                state.in_string = false;
            }
        } else {
            match ch {
                '"' => state.in_string = true,
                '{' => state.object_depth += 1,
                '}' => {
                    state.object_depth -= 1;
                    if state.object_depth == 0 {
                        complete = true;
                    }
                }
                '[' => state.array_depth += 1,
                ']' => state.array_depth -= 1,
                ',' if state.object_depth == 1 && state.array_depth == 0 => {
                    state.last_top_level_comma = i as isize;
                }
                _ => {}
            }
        }
        i += char_len;
    }
    state.scanned_to = i;
    complete
}

/// Recover the complete top-level fields before a top-level comma by closing
/// the object at that boundary (`recoveredToolInput`, `acp-agent.js:222-232`).
fn recovered_tool_input(prefix: &str) -> Option<Value> {
    let candidate = format!("{prefix}}}");
    match serde_json::from_str::<Value>(&candidate).ok()? {
        Value::Object(_) => Some(serde_json::from_str(&candidate).ok()?),
        _ => None,
    }
}

/// Whether a tool name is a Task* built-in (`isTaskTool`, `acp-agent.js:6222`).
fn is_task_tool(name: &str) -> bool {
    matches!(name, "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The `sessionUpdate` discriminant of an update, for assertions.
    fn kind(u: &SessionUpdate) -> &'static str {
        match u {
            SessionUpdate::UserMessageChunk(_) => "user_message_chunk",
            SessionUpdate::AgentMessageChunk(_) => "agent_message_chunk",
            SessionUpdate::AgentThoughtChunk(_) => "agent_thought_chunk",
            SessionUpdate::ToolCall(_) => "tool_call",
            SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
            SessionUpdate::Plan(_) => "plan",
            SessionUpdate::AvailableCommandsUpdate(_) => "available_commands_update",
            SessionUpdate::CurrentModeUpdate(_) => "current_mode_update",
            SessionUpdate::ConfigOptionUpdate(_) => "config_option_update",
            SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
            SessionUpdate::UsageUpdate(_) => "usage_update",
            _ => "other",
        }
    }

    /// 7.T1 (INV-18) — table-driven: each of the 8 emitted variants maps 1:1
    /// from a realistic Claude input; the 3 discarded variants are never
    /// produced.
    #[test]
    fn inv_18_variant_mapping() {
        let cases: Vec<(&str, Value, MsgRole, &'static str)> = vec![
            (
                "assistant text",
                json!([{"type": "text", "text": "hi"}]),
                MsgRole::Assistant,
                "agent_message_chunk",
            ),
            (
                "user text",
                json!([{"type": "text", "text": "hi"}]),
                MsgRole::User,
                "user_message_chunk",
            ),
            (
                "thinking",
                json!([{"type": "thinking", "thinking": "reasoning"}]),
                MsgRole::Assistant,
                "agent_thought_chunk",
            ),
            (
                "tool_use",
                json!([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}]),
                MsgRole::Assistant,
                "tool_call",
            ),
            (
                "tool_result",
                json!([{"type": "tool_result", "tool_use_id": "t1", "content": "out"}]),
                MsgRole::Assistant,
                "tool_call_update",
            ),
            (
                "todo_write plan",
                json!([{"type": "tool_use", "id": "t2", "name": "TodoWrite", "input": {"todos": [{"content": "a", "status": "pending"}]}}]),
                MsgRole::Assistant,
                "plan",
            ),
        ];

        for (desc, content, role, expected) in cases {
            let mut state = MapState::default();
            let updates = map_consolidated(&content, role, &mut state);
            assert_eq!(
                updates.len(),
                1,
                "{desc}: expected exactly one update, got {}",
                updates.len()
            );
            assert_eq!(
                kind(&updates[0]),
                expected,
                "{desc}: wrong variant ({:?})",
                updates[0]
            );
        }

        // The two remaining emitted variants are produced by their own pure
        // entry points.
        let mode = map_mode_update("plan");
        assert_eq!(kind(&mode), "current_mode_update");
        let commands =
            map_commands_changed(&json!([{"name": "x", "description": "d"}]), &json!([]));
        assert_eq!(
            kind(&SessionUpdate::AvailableCommandsUpdate(commands)),
            "available_commands_update"
        );

        // The 3 discarded variants are never produced: none of the realistic
        // inputs that would carry session-info/config/usage data map to them.
        let discarded_inputs = vec![
            json!({"type": "system", "subtype": "session_info", "title": "t"}),
            json!({"type": "system", "subtype": "config_option", "configId": "c"}),
            json!({"type": "system", "subtype": "usage", "input": 1}),
            json!([{"type": "compaction", "id": "c1"}]),
            json!([{"type": "document", "source": {}}]),
            json!([{"type": "redacted_thinking", "data": "x"}]),
        ];
        for input in discarded_inputs {
            let mut state = MapState::default();
            let updates = map_consolidated(&input, MsgRole::Assistant, &mut state);
            for u in updates {
                assert!(
                    !matches!(
                        u,
                        SessionUpdate::SessionInfoUpdate(_)
                            | SessionUpdate::ConfigOptionUpdate(_)
                            | SessionUpdate::UsageUpdate(_)
                    ),
                    "a discarded variant was produced: {:?}",
                    u
                );
            }
        }
    }

    /// 7.T2 (INV-19) — a tool input streamed across 5 `input_json_delta`
    /// fragments yields exactly one `ToolCall` plus refining `ToolCallUpdate`s,
    /// never a second `ToolCall`.
    #[test]
    fn inv_19_partial_input_refines() {
        let mut state = MapState::default();

        // content_block_start surfaces the tool_call.
        let start = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}
            }
        });
        let updates = map_stream_event(&start, &mut state);
        assert_eq!(updates.len(), 1, "start surfaces one update");
        assert_eq!(kind(&updates[0]), "tool_call");

        // 5 fragments of `{"command":"ls","description":"list"}`.
        let fragments = [
            r#"{"com"#,
            r#"mand":"ls""#,
            r#","#,
            r#"description":"list""#,
            r#"}"#,
        ];
        let mut tool_calls = 1;
        let mut tool_call_updates = 0;
        for frag in fragments {
            let delta = json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": frag}
                }
            });
            let produced = map_stream_event(&delta, &mut state);
            for u in produced {
                match u {
                    SessionUpdate::ToolCall(_) => tool_calls += 1,
                    SessionUpdate::ToolCallUpdate(_) => tool_call_updates += 1,
                    other => panic!("unexpected update kind: {:?}", kind(&other)),
                }
            }
        }

        assert_eq!(
            tool_calls, 1,
            "INV-19: exactly one ToolCall across the whole stream, never a duplicate"
        );
        assert!(
            tool_call_updates >= 1,
            "INV-19: the fragments must yield at least one refining ToolCallUpdate"
        );
    }

    /// 7.T3 (7.7) — a tool_use block present in both a `stream_event` and the
    /// consolidated message emits exactly one `ToolCall`, not two.
    #[test]
    fn dedupe_stream_and_consolidated_emits_once() {
        let mut state = MapState::default();

        // 1. The streamed path surfaces the tool_call first.
        let start = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "a"}}
            }
        });
        let first = map_stream_event(&start, &mut state);
        assert_eq!(kind(&first[0]), "tool_call");

        // 2. The consolidated assistant message carries the same block. The
        //    dedupe rule (7.7) must refine it, not emit a second tool_call.
        let consolidated = json!([
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "a", "limit": 10}}
        ]);
        let second = map_consolidated(&consolidated, MsgRole::Assistant, &mut state);
        assert_eq!(second.len(), 1, "consolidated dedupes to one update");
        assert_eq!(
            kind(&second[0]),
            "tool_call_update",
            "a block in both stream_event and consolidated emits once (no duplicate tool_call)"
        );

        let total_tool_calls = first
            .iter()
            .filter(|u| matches!(u, SessionUpdate::ToolCall(_)))
            .count()
            + second
                .iter()
                .filter(|u| matches!(u, SessionUpdate::ToolCall(_)))
                .count();
        assert_eq!(
            total_tool_calls, 1,
            "7.7: the block must be surfaced as exactly one ToolCall across both paths"
        );
    }
}
