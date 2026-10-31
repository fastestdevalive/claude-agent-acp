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
//! Source of truth (Node adapter at the pinned version, `dist/acp-agent.js`):
//! `toAcpNotifications` at lines 6383–6759, `streamEventToAcpNotifications` at
//! 6760–6864, the partial-JSON lexer at 179–232.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ContentBlock, ContentChunk,
    CurrentModeUpdate, ImageContent, MessageId, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, SessionModeId, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    UnstructuredCommandInput,
};
use serde_json::Value;

use crate::meta_filter::ChangedMetaFilter;
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
    /// The Anthropic API message id of the message currently streaming
    /// (`currentStreamMessageId`), captured from `message_start`'s `message.id`
    /// so the streamed chunk updates that follow (whose delta events don't carry
    /// it) are all tagged with the same replay-stable id (`acp-agent.js:2889`).
    current_stream_message_id: String,
    /// Per-tool-call "what the client holds", for the field tracker (2.9 /
    /// X-7). Seeded when a `tool_call` is surfaced (map.rs + permission.rs);
    /// each `tool_call_update` emits only fields whose value differs, then
    /// updates the held state; the entry ends after the `tool_result`.
    tool_tracker: HashMap<String, TrackedToolCall>,
    /// Unfinished-tool books (3.7): dispatched ids + per-turn foreground set.
    pub(crate) books: crate::unfinished::ToolBooks,
    /// The client is AIR (rust-air-gaps G11): tool reports carry
    /// `_meta.jetbrains.air.*`, the AIR-only `is_mcp_tool_call` flag, no
    /// `rawInput` while the input still streams, the AIR `rawOutput` rule, and
    /// pass through the [`ChangedMetaFilter`]. Off for every other client.
    air: bool,
    /// The session working directory (locates a Skill's `SKILL.md`), AIR only.
    cwd: Option<PathBuf>,
    /// True while a `content_block_start` tool_use block is mapped: its input
    /// is still streaming (`inputComplete === false` in the Node renderer).
    input_incomplete: bool,
    /// The AIR changed-`_meta` filter (Decision 25).
    meta_filter: ChangedMetaFilter,
}

impl MapState {
    /// Mark the session as an AIR one (rust-air-gaps G11) with its working
    /// directory. `MapState::default()` stays a non-AIR session.
    pub(crate) fn configure_air(&mut self, cwd: PathBuf) {
        self.air = true;
        self.cwd = Some(cwd);
    }

    /// Whether the client is AIR (see [`MapState::configure_air`]).
    pub(crate) fn is_air(&self) -> bool {
        self.air
    }

    /// The `_meta` of a tool-use report: `{claudeCode:{toolName}}`, plus the AIR
    /// keys for an AIR client.
    pub(crate) fn use_meta(&self, name: &str, input: &Value) -> serde_json::Map<String, Value> {
        if self.air {
            tools::air_tool_meta(name, input, self.cwd.as_deref())
        } else {
            tools::tool_meta(name, input)
        }
    }

    /// Run `updates` through the AIR changed-`_meta` filter (a no-op for any
    /// other client).
    pub(crate) fn filter_updates(&mut self, updates: Vec<SessionUpdate>) -> Vec<SessionUpdate> {
        if !self.air {
            return updates;
        }
        updates
            .into_iter()
            .filter_map(|update| self.meta_filter.apply(update))
            .collect()
    }

    /// Forget a tool call the turn closed without a `tool_result` (3.7,
    /// Node `closeToolCall`): the id is no longer emitted and its cache and
    /// field-tracker entries go.
    pub(crate) fn close_tool_call(&mut self, id: &str) {
        self.emitted.remove(id);
        self.tool_use_cache.remove(id);
        self.tool_tracker.remove(id);
        self.books.forget(id);
    }

    /// Whether a tool-call id has already surfaced to the client (used by
    /// `permission::ensure_tool_call_emitted`, INV-20 / #851).
    pub(crate) fn has_emitted(&self, id: &str) -> bool {
        self.emitted.contains(id)
    }

    /// Mark a tool-call id as surfaced (and cache its tool_use block) so a
    /// later `tool_use` refines rather than emits a duplicate (INV-20).
    pub(crate) fn mark_emitted(&mut self, id: String, block: Value) {
        self.emitted.insert(id.clone());
        self.tool_use_cache.insert(id, block);
    }

    /// Seed the field tracker for a freshly-surfaced `tool_call` (2.9 / X-7).
    pub(crate) fn seed_tool_tracker(&mut self, id: String, tracked: TrackedToolCall) {
        self.tool_tracker.insert(id, tracked);
    }

    /// Apply a `tool_call_update` candidate against what the client holds for
    /// `id` (2.9 / X-7): returns the sub-set of fields that actually changed
    /// (compared by value), updating the held state, or `None` when nothing
    /// changed (`_meta` is always kept whole for non-AIR, so an unchanged
    /// update is not sent).
    pub(crate) fn track_tool_call_update(
        &mut self,
        id: &str,
        fields: &ToolCallUpdateFields,
    ) -> Option<ToolCallUpdateFields> {
        match self.tool_tracker.get_mut(id) {
            Some(entry) => {
                let changed = entry.apply(fields);
                if changed.is_empty() {
                    None
                } else {
                    Some(changed.into_fields())
                }
            }
            // No prior state for this id (X-7: the client holds nothing yet) —
            // everything in the candidate is new, so emit it whole.
            None => Some(fields.clone()),
        }
    }

    /// End a tool call's field-tracker entry after its `tool_result` (2.9 /
    /// X-7: "entry ends after the `tool_result`").
    pub(crate) fn finish_tool_tracker(&mut self, id: &str) {
        self.tool_tracker.remove(id);
    }

    /// Clear EVERY per-tool-call field-tracker entry (review follow-up
    /// 3.17-D). Called on session cancel and when a turn settles, mirroring
    /// Node clearing its field tracker on those boundaries — a tool call that
    /// never gets a `tool_result` (interrupted / refused) must not leak its
    /// tracker entry into the next turn.
    /// Number of live field-tracker entries (diagnostics / tests).
    #[cfg(test)]
    pub(crate) fn tracked_tool_calls(&self) -> usize {
        self.tool_tracker.len()
    }

    pub(crate) fn clear_tool_tracker(&mut self) {
        self.tool_tracker.clear();
    }
}

/// What the client is known to hold for a surfaced tool call (2.9 / X-7),
/// mirroring `dist/tool-calls/field-tracker.js`: the last value sent for each
/// of `status,kind,title,content,locations,rawInput,rawOutput`.
#[derive(Debug, Clone)]
pub(crate) struct TrackedToolCall {
    status: ToolCallStatus,
    kind: ToolKind,
    title: String,
    content: Vec<ToolCallContent>,
    locations: Vec<ToolCallLocation>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
}

impl TrackedToolCall {
    /// The tracker entry for a freshly-emitted `tool_call` (always `Pending`
    /// status, never any raw output).
    pub(crate) fn initial(
        kind: ToolKind,
        title: String,
        content: Vec<ToolCallContent>,
        locations: Vec<ToolCallLocation>,
        raw_input: Option<Value>,
    ) -> Self {
        Self {
            status: ToolCallStatus::Pending,
            kind,
            title,
            content,
            locations,
            raw_input,
            raw_output: None,
        }
    }

    /// Compare `fields` against the held values; record and return the changed
    /// subset (updating the held state).
    fn apply(&mut self, fields: &ToolCallUpdateFields) -> Changed {
        let mut changed = Changed::default();
        if let Some(v) = &fields.status {
            if v != &self.status {
                changed.status = Some(*v);
                self.status = *v;
            }
        }
        if let Some(v) = &fields.kind {
            if v != &self.kind {
                changed.kind = Some(*v);
                self.kind = *v;
            }
        }
        if let Some(v) = &fields.title {
            if v != &self.title {
                changed.title = Some(v.clone());
                self.title = v.clone();
            }
        }
        if let Some(v) = &fields.content {
            if v != &self.content {
                changed.content = Some(v.clone());
                self.content = v.clone();
            }
        }
        if let Some(v) = &fields.locations {
            if v != &self.locations {
                changed.locations = Some(v.clone());
                self.locations = v.clone();
            }
        }
        if let Some(v) = &fields.raw_input {
            if Some(v.clone()) != self.raw_input {
                changed.raw_input = Some(v.clone());
                self.raw_input = Some(v.clone());
            }
        }
        if let Some(v) = &fields.raw_output {
            if Some(v.clone()) != self.raw_output {
                changed.raw_output = Some(v.clone());
                self.raw_output = Some(v.clone());
            }
        }
        changed
    }
}

/// The subset of candidate fields that actually changed for a `tool_call_update`.
#[derive(Debug, Default)]
struct Changed {
    status: Option<ToolCallStatus>,
    kind: Option<ToolKind>,
    title: Option<String>,
    content: Option<Vec<ToolCallContent>>,
    locations: Option<Vec<ToolCallLocation>>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
}

impl Changed {
    /// Whether nothing changed.
    fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.kind.is_none()
            && self.title.is_none()
            && self.content.is_none()
            && self.locations.is_none()
            && self.raw_input.is_none()
            && self.raw_output.is_none()
    }

    fn into_fields(self) -> ToolCallUpdateFields {
        let mut f = ToolCallUpdateFields::new();
        if let Some(v) = self.status {
            f = f.status(v);
        }
        if let Some(v) = self.kind {
            f = f.kind(v);
        }
        if let Some(v) = self.title {
            f = f.title(v);
        }
        if let Some(v) = self.content {
            f = f.content(v);
        }
        if let Some(v) = self.locations {
            f = f.locations(v);
        }
        if let Some(v) = self.raw_input {
            f = f.raw_input(v);
        }
        if let Some(v) = self.raw_output {
            f = f.raw_output(v);
        }
        f
    }
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
                if let Some(update) = tool_call_from_use(block, state) {
                    out.push(update);
                }
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
                if let Some(update) = tool_update_from_result(block, state) {
                    out.push(update);
                }
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
fn tool_call_from_use(block: &Value, state: &mut MapState) -> Option<SessionUpdate> {
    let id = block["id"].as_str().unwrap_or("").to_string();
    let name = block["name"].as_str().unwrap_or("Other").to_string();
    let raw_input = block.get("input").cloned();

    if state.emitted.contains(&id) {
        // Already surfaced (streamed block, or consolidated after streaming) —
        // refine rather than emit a duplicate ToolCall (7.7).
        return tool_call_update_refine(id, &name, raw_input, state);
    }
    state.emitted.insert(id.clone());
    state.tool_use_cache.insert(id.clone(), block.clone());

    let input = raw_input.clone().unwrap_or(Value::Null);
    let shape = tools::tool_shape(&name, &input);
    // AIR gets `rawInput` once it is complete; every other client gets the
    // input as it stands, also the empty input at the stream start.
    let sent_raw_input = if state.air && state.input_incomplete {
        None
    } else {
        raw_input.clone()
    };
    // 2.9 (X-7): seed the field tracker with what the client now holds.
    state.seed_tool_tracker(
        id.clone(),
        TrackedToolCall::initial(
            shape.kind,
            shape.title.clone(),
            shape.content.clone(),
            shape.locations.clone(),
            sent_raw_input.clone(),
        ),
    );
    let mut meta = state.use_meta(&name, &input);
    if state.air && name.starts_with("mcp__") {
        meta.insert("is_mcp_tool_call".to_string(), Value::Bool(true));
    }
    Some(SessionUpdate::ToolCall(
        ToolCall::new(id, shape.title)
            // E-9: the programmatic tool name (e.g. "Bash") is surfaced on the
            // typed `tool_call` (feature `unstable_tool_call_name`).
            .name(name.clone())
            .kind(shape.kind)
            .status(ToolCallStatus::Pending)
            .content(shape.content)
            .locations(shape.locations)
            .raw_input(sent_raw_input)
            .meta(meta),
    ))
}

/// 7.3 — a `tool_result` block becomes a completing `ToolCallUpdate`.
fn tool_update_from_result(block: &Value, state: &mut MapState) -> Option<SessionUpdate> {
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
    let use_input = state
        .tool_use_cache
        .remove(&id)
        .and_then(|use_block| use_block.get("input").cloned())
        .unwrap_or(Value::Null);

    let mut raw_output = block.get("content").cloned();
    let result_input = block.get("content").cloned().unwrap_or(Value::Null);
    let mut content = tools::tool_result_content(&name, &result_input, is_error);
    if state.air {
        // AIR shows a read or a search that names a path as the list of viewed
        // files, which does not show the text of the result: it stays out. AIR
        // gets the raw tool_result only when no other field carries the result
        // (`renderer.js` `result`).
        let viewed_files = !is_error && tools::names_viewed_file(&name, &use_input);
        if viewed_files {
            content = content
                .map(|items| {
                    items
                        .into_iter()
                        .filter(|item| !is_text_content(item))
                        .collect::<Vec<_>>()
                })
                .filter(|items| !items.is_empty());
            raw_output = None;
        } else if content.is_some() {
            raw_output = None;
        }
    }
    let mut fields = ToolCallUpdateFields::new().status(status);
    if let Some(content) = content {
        fields = fields.content(content);
    }
    if let Some(raw_output) = raw_output {
        fields = fields.raw_output(raw_output);
    }

    // 2.9 (X-7): emit only fields the client does not already hold; the
    // status always differs (Pending -> Completed/Failed) so this is `Some`.
    let fields = state.track_tool_call_update(&id, &fields)?;
    // Entry ends after the tool_result.
    state.finish_tool_tracker(&id);
    Some(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id, fields).meta(tools::tool_meta(&name, &result_input)),
    ))
}

/// 7.3/7.7 — a refining `ToolCallUpdate` for an already-surfaced tool call.
/// Phase 9: applies the shape (`kind`/`title`/`content`/`locations`) plus the
/// `_meta` `claudeCode`. 2.9 (X-7): only fields the client does not already
/// hold are emitted; an update with nothing changed is not sent.
fn tool_call_update_refine(
    id: String,
    name: &str,
    raw_input: Option<Value>,
    state: &mut MapState,
) -> Option<SessionUpdate> {
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
    let fields = state.track_tool_call_update(&id, &fields)?;
    Some(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id, fields).meta(state.use_meta(name, &input)),
    ))
}

/// Whether a tool-call content item is a plain text content block.
fn is_text_content(item: &ToolCallContent) -> bool {
    matches!(item, ToolCallContent::Content(c) if matches!(c.content, ContentBlock::Text(_)))
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
/// terminal-bound commands, renames `(MCP)` names to `mcp:`, filters the
/// terminal-only unsupported list, then appends the adapter's own `/mcp` and
/// `/remote-control` entries (E-5): a CLI-provided `mcp` entry is dropped and
/// replaced at the end (the adapter's), and `remote-control` is appended unless
/// the CLI already offers a non-terminal one.
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
    let mut has_non_terminal_remote_control = false;
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
            // E-5: the adapter's own `/mcp` replaces a CLI-provided one, so a
            // CLI `mcp` entry is dropped (the adapter's is appended last).
            if final_name == "mcp" {
                continue;
            }
            if final_name == "remote-control" {
                has_non_terminal_remote_control = true;
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
    // E-5: append the adapter's `/mcp` then `/remote-control` (moved to the end
    // so the adapter's management UI owns them).
    avail.push(
        AvailableCommand::new(
            "mcp",
            "Show the MCP servers and their status, or reconnect, enable, or disable a server",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("[reconnect|enable|disable [<server>|all]]"),
        )),
    );
    if !has_non_terminal_remote_control {
        avail.push(AvailableCommand::new(
            "remote-control",
            "Continue this session from claude.ai/code or the Claude mobile app; `off` disconnects",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("[session name | off]"),
        )));
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
            // The input of a streamed tool_use block is still incomplete here.
            state.input_incomplete = is_tool_use_block(block);
            let mapped = map_consolidated(
                &Value::Array(vec![block.clone()]),
                MsgRole::Assistant,
                state,
            );
            state.input_incomplete = false;
            apply_stream_message_id(mapped, &state.current_stream_message_id)
        }
        Some("content_block_delta") => {
            let delta = &event["delta"];
            if delta["type"].as_str() == Some("input_json_delta") {
                let index = event["index"].as_u64().unwrap_or(0);
                input_json_delta(state, &stream_key, index, delta)
            } else {
                // A text/thinking delta maps like a one-block assistant message.
                apply_stream_message_id(
                    map_consolidated(
                        &Value::Array(vec![delta.clone()]),
                        MsgRole::Assistant,
                        state,
                    ),
                    &state.current_stream_message_id,
                )
            }
        }
        Some("message_start") => {
            // `message_start` carries the Anthropic API message id; capture it
            // so the streamed chunks that follow are tagged with the same id
            // (`acp-agent.js:2889`). Also ends the input stream on this lane
            // (drops leftover partial input).
            state.current_stream_message_id =
                event["message"]["id"].as_str().unwrap_or("").to_string();
            state
                .streamed_inputs
                .retain(|(key, _), _| *key != stream_key);
            Vec::new()
        }
        Some("content_block_stop") | Some("message_stop") => {
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

/// Stamp `message_id` onto the chunk updates a stream-event block produced
/// (agent/user message and thought chunks only — never tool/plan updates),
/// mirroring the Node adapter's `applyMessageId` (`acp-agent.js:6211`). A
/// no-op when the id is empty (e.g. a stream with no `message_start`).
fn apply_stream_message_id(updates: Vec<SessionUpdate>, message_id: &str) -> Vec<SessionUpdate> {
    if message_id.is_empty() {
        return updates;
    }
    let message_id = MessageId::new(message_id.to_string());
    updates
        .into_iter()
        .map(|u| match u {
            SessionUpdate::AgentMessageChunk(c) => {
                SessionUpdate::AgentMessageChunk(c.message_id(message_id.clone()))
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                SessionUpdate::AgentThoughtChunk(c.message_id(message_id.clone()))
            }
            SessionUpdate::UserMessageChunk(c) => {
                SessionUpdate::UserMessageChunk(c.message_id(message_id.clone()))
            }
            other => other,
        })
        .collect()
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
    // Capture the id/name for the refinement, and the meta input, before the
    // mutable tracker borrow (can't hold the `streamed_inputs` borrow across
    // `state.track_tool_call_update`).
    let id = streamed.id.clone();
    let name = streamed.name.clone();
    // Refine, never duplicate: this is a tool_call_update, not a tool_call.
    streamed_input_refinement(state, &id, &name, input)
        .into_iter()
        .collect()
}

/// Build a refining `ToolCallUpdate` for a partially-recovered tool input.
/// Phase 9: applies `kind`/`title`/`locations` from the shape but never
/// `content` (content built from partial input is misleading; the consolidated
/// message supplies it moments later — mirrors `streamedInputRefinement`).
fn streamed_input_refinement(
    state: &mut MapState,
    id: &str,
    name: &str,
    input: Value,
) -> Option<SessionUpdate> {
    let shape = tools::tool_shape(name, &input);
    // The `_meta` reads the recovered partial input (`partialRefinement`).
    let meta = state.use_meta(name, &input);
    let mut fields = ToolCallUpdateFields::new()
        .kind(shape.kind)
        .title(shape.title);
    // AIR gets no `rawInput` while the input streams; it goes out once complete.
    if !state.air {
        fields = fields.raw_input(input);
    }
    if !shape.locations.is_empty() {
        fields = fields.locations(shape.locations);
    }
    // 2.9 (X-7): emit only fields the client does not already hold.
    let fields = state.track_tool_call_update(id, &fields)?;
    Some(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id.to_string(), fields).meta(meta),
    ))
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

    /// 2.5 (E-5) — `map_commands_changed` drops a CLI-provided `mcp` entry,
    /// filters the unsupported terminal commands, and appends the adapter's
    /// `/mcp` and `/remote-control` (E-5); a CLI that already offers a
    /// non-terminal `remote-control` does not get a duplicate.
    #[test]
    fn inv_36_commands_append_mcp_and_remote_control() {
        let names = |u: &AvailableCommandsUpdate| -> Vec<String> {
            serde_json::to_value(u)
                .ok()
                .and_then(|v| v.get("availableCommands").cloned())
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|c| c.get("name").and_then(serde_json::Value::as_str))
                .map(String::from)
                .collect()
        };

        // The commands-proactive CLI list: a plain `mcp`, a `weather (MCP)`,
        // and the unsupported `clear`/`cost`/`todos`.
        let commands = json!([
            {"name": "compact", "description": "Compress the conversation history"},
            {"name": "config", "description": "Manage Claude Code configuration", "argumentHint": "[key] [value]"},
            {"name": "doctor", "description": "Run a system diagnostics check"},
            {"name": "help", "description": "Display help for commands"},
            {"name": "memory", "description": "Manage Claude's memory"},
            {"name": "model", "description": "Switch to a different model"},
            {"name": "mcp", "description": "Manage MCP servers"},
            {"name": "permissions", "description": "Manage permission settings"},
            {"name": "status", "description": "Display session status"},
            {"name": "weather (MCP)", "description": "Weather service via MCP"},
            {"name": "clear", "description": "Clear the conversation"},
            {"name": "cost", "description": "Show usage cost"},
            {"name": "todos", "description": "Manage todos"},
        ]);
        let result = map_commands_changed(&commands, &json!([]));
        assert_eq!(
            names(&result),
            vec![
                "compact",
                "config",
                "doctor",
                "help",
                "memory",
                "model",
                "permissions",
                "status",
                "mcp:weather",
                "mcp",
                "remote-control",
            ],
            "CLI `mcp` is dropped and replaced at the end; unsupported commands filtered"
        );

        // Empty commands -> only the appended mcp + remote-control.
        let empty = map_commands_changed(&json!([]), &json!([]));
        assert_eq!(
            names(&empty),
            vec!["mcp", "remote-control"],
            "empty list yields just the two appended commands (text-only fixture)"
        );

        // A CLI already offering a non-terminal remote-control -> no duplicate:
        // the CLI's entry stays in place and only mcp is appended after it.
        let with_rc = map_commands_changed(
            &json!([{"name": "remote-control", "description": "already here"}]),
            &json!([]),
        );
        assert_eq!(
            names(&with_rc),
            vec!["remote-control", "mcp"],
            "must not append a second remote-control when the CLI already has one"
        );
        // But a terminal-bound remote-control is dropped and the adapter's appended.
        let rc_terminal = map_commands_changed(
            &json!([{"name": "remote-control", "description": "t"}]),
            &json!([{"name": "remote-control", "description": "t"}]),
        );
        assert_eq!(
            names(&rc_terminal),
            vec!["mcp", "remote-control"],
            "a terminal remote-control is replaced by the adapter's"
        );
    }

    /// 2.9 (X-7) — the field tracker: a `tool_call_update` drops fields whose
    /// value is unchanged since the client's last view. Mirrors
    /// `streamed-partial-input`: after the initial `tool_call` (kind execute,
    /// title "Terminal", rawInput {}), the streamed refine changes only the
    /// title + rawInput (kind is dropped); a subsequent refine changes only
    /// rawInput.
    #[test]
    fn inv_45_field_tracker_drops_unchanged() {
        // Initial tool_call (seeds the tracker).
        let mut state = MapState::default();
        let start = json!([{
            "type": "tool_use", "id": "toolu_01STREAM", "name": "Bash", "input": {}
        }]);
        let updates = map_consolidated(&start, MsgRole::Assistant, &mut state);
        debug_assert!(matches!(&updates[0], SessionUpdate::ToolCall(_)));

        // First streamed refine: kind unchanged (dropped), title + rawInput differ.
        let refine = json!({
            "type": "tool_use", "id": "toolu_01STREAM", "name": "Bash",
            "input": {"command": "echo hi"}
        });
        let updates = map_consolidated(&Value::Array(vec![refine]), MsgRole::Assistant, &mut state);
        let SessionUpdate::ToolCallUpdate(u) = &updates[0] else {
            panic!("expected a tool_call_update");
        };
        let fields = &u.fields;
        assert!(fields.title.is_some(), "title changed Terminal -> echo hi");
        assert!(
            fields.raw_input.is_some(),
            "rawInput changed: empty -> command"
        );
        assert!(
            fields.kind.is_none(),
            "unchanged kind must be dropped (X-7)"
        );
        assert!(
            fields.content.is_none(),
            "unchanged content must be dropped"
        );
        assert!(fields.status.is_none(), "unchanged status must be dropped");

        // Second streamed refine ({command,cwd}): only rawInput changes.
        let refine2 = json!({
            "type": "tool_use", "id": "toolu_01STREAM", "name": "Bash",
            "input": {"command": "echo hi", "cwd": "/tmp"}
        });
        let updates =
            map_consolidated(&Value::Array(vec![refine2]), MsgRole::Assistant, &mut state);
        let SessionUpdate::ToolCallUpdate(u2) = &updates[0] else {
            panic!("expected a second tool_call_update");
        };
        assert!(u2.fields.raw_input.is_some(), "rawInput changed again");
        assert!(
            u2.fields.title.is_none(),
            "unchanged title (already 'echo hi') must be dropped"
        );
        assert!(u2.fields.kind.is_none(), "unchanged kind dropped again");

        // A tool_result now emits only the completing fields.
        let result = json!([{
            "type": "tool_result", "tool_use_id": "toolu_01STREAM", "content": "ok"
        }]);
        let updates = map_consolidated(&result, MsgRole::User, &mut state);
        let SessionUpdate::ToolCallUpdate(u3) = &updates[0] else {
            panic!("expected a completing tool_call_update");
        };
        assert!(u3.fields.status.is_some(), "status Pending -> Completed");
        assert!(u3.fields.raw_output.is_some(), "rawOutput appears");
        assert!(u3.fields.content.is_some(), "content appears");
    }

    /// 2.9 (X-7) — `_meta` is kept whole for a non-AIR client even when only
    /// some fields change (and an update with unchanged fields and unchanged
    /// `_meta` is not sent).
    #[test]
    fn inv_45_field_tracker_keeps_meta() {
        let mut state = MapState::default();
        let start = json!([{
            "type": "tool_use", "id": "toolu_META", "name": "Bash", "input": {"command": "ls"}
        }]);
        map_consolidated(&start, MsgRole::Assistant, &mut state);

        // A result update carries the `_meta` {claudeCode:{toolName}} whole.
        let result = json!([{
            "type": "tool_result", "tool_use_id": "toolu_META", "content": "ok"
        }]);
        let updates = map_consolidated(&result, MsgRole::User, &mut state);
        let SessionUpdate::ToolCallUpdate(u) = &updates[0] else {
            panic!("expected a completing tool_call_update");
        };
        let meta = serde_json::to_value(u.meta.as_ref()).unwrap();
        assert_eq!(meta, json!({"claudeCode": {"toolName": "Bash"}}));

        // A replayed identical tool_use whose input is unchanged emits nothing.
        let mut state2 = MapState::default();
        map_consolidated(&start, MsgRole::Assistant, &mut state2);
        let same = json!([{
            "type": "tool_use", "id": "toolu_META", "name": "Bash", "input": {"command": "ls"}
        }]);
        let updates = map_consolidated(&same, MsgRole::Assistant, &mut state2);
        assert!(
            updates.is_empty(),
            "an unchanged update must not be sent (X-7)"
        );
    }

    /// Review follow-up 3.17-D — every per-tool-call field-tracker entry is
    /// cleared on the turn-boundary / cancel, so a tool call that never got a
    /// `tool_result` cannot leak its tracker state into the next turn.
    #[test]
    fn inv_45_field_tracker_clears_all_on_boundary() {
        let mut state = MapState::default();
        let start = json!([{
            "type": "tool_use", "id": "toolu_ORPHAN", "name": "Bash", "input": {"command": "ls"}
        }]);
        let updates = map_consolidated(&start, MsgRole::Assistant, &mut state);
        debug_assert!(matches!(&updates[0], SessionUpdate::ToolCall(_)));
        assert!(!state.tool_tracker.is_empty(), "tracker is seeded");

        // Turn settle / session cancel: clear every per-tool-call entry.
        state.clear_tool_tracker();
        assert!(
            state.tool_tracker.is_empty(),
            "all per-tool-call tracker entries cleared on the boundary"
        );

        // A tool_result for the now-orphaned id treats the call as fresh: it
        // must still OBSERVE a changed status and emit a completing update
        // (without any leaked prior tracker state suppressing it).
        let result = json!([{
            "type": "tool_result", "tool_use_id": "toolu_ORPHAN", "content": "ok"
        }]);
        let updates = map_consolidated(&result, MsgRole::User, &mut state);
        assert!(
            matches!(updates[0], SessionUpdate::ToolCallUpdate(_)),
            "a completing tool_call_update is still emitted after the clear"
        );
    }
}
