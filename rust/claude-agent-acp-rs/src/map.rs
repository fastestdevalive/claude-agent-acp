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
    /// updates the held state; the entry ends after the `tool_result` (or after
    /// PostToolUse hook callback when a hook is pending, M3/Decision 12).
    tool_tracker: HashMap<String, TrackedToolCall>,
    /// Tool calls with a pending PostToolUse hook (M3/Decision 12).
    pub(crate) pending_hooks: HashSet<String>,
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
    /// Per-stream record of the text/thinking actually streamed as deltas, so a
    /// consolidated assistant message forwards only the un-streamed remainder
    /// (rust-parity-fixes 1.7 / Decision 18). Keyed by `parent_tool_use_id`
    /// (`""` for the top level), like Node's `streamedBlocksByParent`
    /// (`acp-agent.js:2742-2750`).
    streamed_blocks: HashMap<String, Vec<StreamedBlock>>,
    /// Whether the stopped-work window after a cancel is open, so a late
    /// consolidated assistant message must not emit its remainder (Node's
    /// `session.cancelled`, `acp-agent.js:5695-5712`). Opened on cancel and
    /// cleared when the next turn activates.
    stopped_work: bool,
}

impl MapState {
    /// Store the session working directory for EVERY client (Decision 6). It
    /// relativises the tool titles/locations (`toDisplayPath`) and locates an
    /// AIR skill's `SKILL.md`; Node passes `session.cwd` at every report site.
    pub(crate) fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = Some(cwd);
    }

    /// Mark the session as an AIR one (rust-air-gaps G11) with its working
    /// directory. `MapState::default()` stays a non-AIR session.
    pub(crate) fn configure_air(&mut self, cwd: PathBuf) {
        self.air = true;
        self.set_cwd(cwd);
    }

    /// Open the stopped-work window after a cancel (Decision 18): a late
    /// consolidated assistant message emits no remainder until the next turn
    /// activates.
    pub(crate) fn begin_stopped_work(&mut self) {
        self.stopped_work = true;
    }

    /// Clear the stopped-work window when a turn activates (Node
    /// `activateTurn` clears `session.cancelled`).
    pub(crate) fn end_stopped_work(&mut self) {
        self.stopped_work = false;
    }

    /// Clear the streamed text/thinking record for one lane (a new
    /// `message_start`, Node `acp-agent.js:5429-5430`).
    fn clear_streamed_blocks(&mut self, key: &str) {
        self.streamed_blocks.remove(key);
    }

    /// Append a non-empty text/thinking delta to a lane's record, extending the
    /// last entry when it continues the same block
    /// (`acp-agent.js:5542-5548`).
    fn record_streamed_delta(&mut self, key: &str, index: u64, kind: StreamedKind, text: &str) {
        let blocks = self.streamed_blocks.entry(key.to_string()).or_default();
        if let Some(last) = blocks.last_mut() {
            if last.index == index && last.kind == kind {
                last.text.push_str(text);
                return;
            }
        }
        blocks.push(StreamedBlock {
            index,
            kind,
            text: text.to_string(),
        });
    }

    /// Take (and reset) a lane's streamed record for a consolidated message
    /// (`acp-agent.js:5835-5838`).
    fn take_streamed_blocks(&mut self, key: &str) -> Vec<StreamedBlock> {
        self.streamed_blocks.remove(key).unwrap_or_default()
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
        self.pending_hooks.remove(id);
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
        // PC-1.2: the PostToolUse hook is pending for every non-TodoWrite, non-Task call,
        // including one surfaced by a permission prompt (Node registers it per tool_use).
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        if name != "TodoWrite" && !tools::is_task_tool(name) {
            self.pending_hooks.insert(id.clone());
        }
        self.emitted.insert(id.clone());
        self.tool_use_cache.insert(id, block);
    }

    /// Retrieve the cached tool_use block for an id, if present.
    pub(crate) fn tool_use_from_cache(&self, id: &str) -> Option<&Value> {
        self.tool_use_cache.get(id)
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

    /// Mark the tool_result of a tool call in the field tracker (M3/Decision 12).
    /// If a hook is pending, keep the entry and mark `result_seen = true`.
    pub(crate) fn finish_result_tool_tracker(&mut self, id: &str, hook_pending: bool) {
        if let Some(entry) = self.tool_tracker.get_mut(id) {
            if hook_pending {
                entry.result_seen = true;
            } else {
                self.tool_tracker.remove(id);
            }
        }
    }

    /// Mark the end of the PostToolUse hook callback in the field tracker (M3/Decision 12).
    /// The entry is removed if the tool_result was already seen.
    pub(crate) fn finish_hook_tool_tracker(&mut self, id: &str) {
        if let Some(entry) = self.tool_tracker.get(id) {
            if entry.result_seen {
                self.tool_tracker.remove(id);
            }
        }
    }

    /// Filters unchanged `content` or `locations` from a PostToolUse hook update,
    /// mirroring Node's `ToolCallFieldTracker.apply` behavior.
    pub(crate) fn filter_hook_tool_call_update(&mut self, id: &str, update: &mut Value) {
        let Some(entry) = self.tool_tracker.get_mut(id) else {
            return;
        };
        if let Some(content_val) = update.get("content") {
            if let Ok(new_content) =
                serde_json::from_value::<Vec<ToolCallContent>>(content_val.clone())
            {
                if new_content == entry.content {
                    if let Some(obj) = update.as_object_mut() {
                        obj.remove("content");
                    }
                } else {
                    entry.content = new_content;
                }
            }
        }
        if let Some(loc_val) = update.get("locations") {
            if let Ok(new_locs) = serde_json::from_value::<Vec<ToolCallLocation>>(loc_val.clone()) {
                if new_locs == entry.locations {
                    if let Some(obj) = update.as_object_mut() {
                        obj.remove("locations");
                    }
                } else {
                    entry.locations = new_locs;
                }
            }
        }
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
        self.pending_hooks.clear();
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
    pub(crate) result_seen: bool,
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
            result_seen: false,
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

/// The kind of a block that streamed as deltas (Node's `chunk.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamedKind {
    /// A `text_delta` block.
    Text,
    /// A `thinking_delta` block.
    Thinking,
}

/// One entry of the per-lane record of what already reached the client as
/// streamed deltas (`streamedBlocksByParent`, `acp-agent.js:5542-5548`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamedBlock {
    /// The content-block index the deltas belonged to.
    index: u64,
    /// Text or thinking.
    kind: StreamedKind,
    /// The concatenated streamed text.
    text: String,
}

/// The per-frame attribution the mapper threads through one message
/// (Decision 8):
///
/// - `parent` — the frame's `parent_tool_use_id` when it is a STRING (D14);
///   `null` / missing means the main conversation, never a stamp.
/// - `structured` — the message-level `tool_use_result`, honoured only when the
///   message holds exactly one `tool_result` block (D4 / R6).
/// - `non_exec` — the parsed `tool_result_meta` sidecar by tool-call id (D15).
#[derive(Debug, Default)]
struct FrameCtx {
    parent: Option<String>,
    structured: Option<Value>,
    non_exec: HashMap<String, crate::tool_meta::NonExecution>,
}

impl FrameCtx {
    /// The attribution of a frame: its string parent id only.
    fn of_frame(frame: &Value) -> Self {
        Self {
            parent: frame
                .get("parent_tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Self::default()
        }
    }

    /// The attribution of a `user` frame: parent, the single-result structured
    /// `tool_use_result` and the `tool_result_meta` sidecar.
    fn of_user_frame(frame: &Value, content: &Value) -> Self {
        let mut ctx = Self::of_frame(frame);
        let tool_result_count = match content {
            Value::Array(blocks) => blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
                .count(),
            _ => 0,
        };
        if tool_result_count == 1 {
            ctx.structured = frame.get("tool_use_result").cloned();
        }
        ctx.non_exec = crate::tool_meta::parse_tool_result_meta(
            frame.get("tool_result_meta").unwrap_or(&Value::Null),
        );
        ctx
    }
}

/// Map a consolidated Claude message's `content` into ACP `SessionUpdate`s
/// (7.1, 7.3, 7.5, 7.6) with no frame attribution. `content` is either a JSON
/// string (a whole-message text) or a JSON array of content blocks — the two
/// shapes `claude` emits.
pub fn map_consolidated(
    content: &Value,
    role: MsgRole,
    state: &mut MapState,
) -> Vec<SessionUpdate> {
    map_consolidated_ctx(content, role, state, &FrameCtx::default())
}

/// [`map_consolidated`] with the frame attribution of a `user` / `assistant` /
/// `stream_event` frame.
fn map_consolidated_ctx(
    content: &Value,
    role: MsgRole,
    state: &mut MapState,
    ctx: &FrameCtx,
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
                map_block(block, role, state, &mut out, ctx);
            }
        }
        _ => {}
    }
    out
}

/// Map a consolidated `user` frame (a `tool_result` echo) the way Node does
/// (`toAcpNotifications`, `acp-agent.js:9955-10280`): the parent attribution,
/// the single-result `tool_use_result` and the `tool_result_meta` sidecar ride
/// along to the tool updates. The echo-skip itself stays in the session actor.
pub fn map_user_frame(frame: &Value, state: &mut MapState) -> Vec<SessionUpdate> {
    let content = frame
        .pointer("/message/content")
        .cloned()
        .unwrap_or(Value::Null);
    let ctx = FrameCtx::of_user_frame(frame, &content);
    map_consolidated_ctx(&content, MsgRole::User, state, &ctx)
}

/// Map a consolidated `assistant` message the way Node does (rust-parity-fixes
/// 1.7 / Decision 18, `acp-agent.js:5829-5844`).
///
/// Only a STRICT top-level frame — one whose `parent_tool_use_id` is PRESENT
/// and `null` — forwards text/thinking, and only the tail its stream did not
/// already cover ([`unstreamed_remainder`]). A frame with the field MISSING
/// counts as nested (Node's `=== null` gate), so its text/thinking stay
/// internal, as they do for a subagent frame. In either case the frame consumes
/// its lane's streamed record so the next message starts fresh. Images, tool
/// uses, tool results and plans map unchanged.
pub fn map_consolidated_assistant(frame: &Value, state: &mut MapState) -> Vec<SessionUpdate> {
    let key = frame["parent_tool_use_id"].as_str().unwrap_or("");
    let content = frame
        .pointer("/message/content")
        .cloned()
        .unwrap_or(Value::Null);
    let streamed = state.take_streamed_blocks(key);
    let content = if is_strict_top_level(frame) && !state.stopped_work {
        unstreamed_remainder(&content, &streamed)
    } else {
        // Nested text/thinking stays internal for a client that does not get
        // subagent text, and a post-cancel remainder is dropped.
        drop_text_thinking(&content)
    };
    let updates = map_consolidated_ctx(
        &content,
        MsgRole::Assistant,
        state,
        &FrameCtx::of_frame(frame),
    );
    // Node groups every chunk under the message id (`messageIdForGrouping`,
    // `acp-agent.js:9789-9800`): the API message id, else the SDK frame uuid.
    let message_id = frame
        .pointer("/message/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| {
            frame
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        })
        .unwrap_or("");
    apply_stream_message_id(updates, message_id)
}

/// Node's top-level gate: `message.parent_tool_use_id === null`, which a
/// MISSING field does not satisfy (`acp-agent.js:5432,5731,5829`).
fn is_strict_top_level(frame: &Value) -> bool {
    frame.get("parent_tool_use_id").is_some_and(Value::is_null)
}

/// Port of `unstreamedRemainder` (`acp-agent.js:519-545`): keep the blocks the
/// stream did not already deliver, in document order.
///
/// A text/thinking block is diffed against the next streamed block of the same
/// type: a non-empty streamed prefix is consumed and only the tail is kept (the
/// whole block when it never streamed, the tail when the stream was cut short,
/// nothing when it streamed in full). Empty assembled blocks are dropped;
/// everything else passes through untouched.
fn unstreamed_remainder(content: &Value, streamed: &[StreamedBlock]) -> Value {
    let Value::Array(blocks) = content else {
        return content.clone();
    };
    let mut kept = Vec::with_capacity(blocks.len());
    let mut stream_pos = 0;
    for item in blocks {
        let kind = match item["type"].as_str() {
            Some("text") => StreamedKind::Text,
            Some("thinking") => StreamedKind::Thinking,
            _ => {
                kept.push(item.clone());
                continue;
            }
        };
        let field = match kind {
            StreamedKind::Text => "text",
            StreamedKind::Thinking => "thinking",
        };
        let full = item[field].as_str().unwrap_or("");
        // Empty assembled blocks carry nothing: drop them.
        if full.is_empty() {
            continue;
        }
        if let Some(streamed) = streamed.get(stream_pos) {
            if streamed.kind == kind
                && !streamed.text.is_empty()
                && full.starts_with(&streamed.text)
            {
                stream_pos += 1;
                let remainder = &full[streamed.text.len()..];
                if remainder.is_empty() {
                    continue;
                }
                let mut truncated = item.clone();
                truncated[field] = Value::String(remainder.to_string());
                kept.push(truncated);
                continue;
            }
        }
        kept.push(item.clone());
    }
    Value::Array(kept)
}

/// Drop text/thinking blocks from a consolidated message, keeping every other
/// block (`acp-agent.js:5841-5844`).
fn drop_text_thinking(content: &Value) -> Value {
    let Value::Array(blocks) = content else {
        return content.clone();
    };
    Value::Array(
        blocks
            .iter()
            .filter(|b| !matches!(b["type"].as_str(), Some("text") | Some("thinking")))
            .cloned()
            .collect(),
    )
}

/// Map one consolidated content block into `out` (7.1, 7.3, 7.5, 7.6).
fn map_block(
    block: &Value,
    role: MsgRole,
    state: &mut MapState,
    out: &mut Vec<SessionUpdate>,
    ctx: &FrameCtx,
) {
    let name = block["name"].as_str().unwrap_or("");
    match block["type"].as_str() {
        Some("tool_use") | Some("server_tool_use") | Some("mcp_tool_use") => {
            if name == "TodoWrite" {
                if let Some(plan) = plan_from_todo_write(block) {
                    out.push(plan);
                }
            } else if is_task_tool(name) {
                // Task* tool_use is suppressed (no tool_call); keep the cache
                // entry so the tool_result path can resolve the tool name and
                // input (X12/Decision 12).
                state.tool_use_cache.insert(
                    block["id"].as_str().unwrap_or("").to_string(),
                    block.clone(),
                );
            } else {
                if let Some(update) = tool_call_from_use(block, state, ctx) {
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
            } else if tool_name.is_some_and(tools::is_task_tool) {
                // Task* tool results are suppressed too (no tool_call_update);
                // the plan update is published by the actor at tool_result time
                // (X12/Decision 12).
                state.tool_use_cache.remove(id);
                // PC-1.5: a permission-surfaced Task* call is resolved by a raw update the
                // unfinished-tool books never see. Node clears the emitted marker on every
                // tool_result (acp-agent.ts:13171) and forgets the call on a final status
                // (acp-agent.ts:4592-4603), so the turn must not report it as stuck.
                state.emitted.remove(id);
                state.books.forget(id);
                // Review PC-1 (c): finishResult (acp-agent.ts:13181) for every tool_result.
                state.finish_result_tool_tracker(id, false);
            } else {
                if let Some(update) = tool_update_from_result(block, state, ctx) {
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
fn tool_call_from_use(
    block: &Value,
    state: &mut MapState,
    ctx: &FrameCtx,
) -> Option<SessionUpdate> {
    let id = block["id"].as_str().unwrap_or("").to_string();
    let name = block["name"].as_str().unwrap_or("Other").to_string();
    let raw_input = block.get("input").cloned();

    if state.emitted.contains(&id) {
        // Already surfaced (streamed block, or consolidated after streaming) —
        // refine rather than emit a duplicate ToolCall (7.7).
        return tool_call_update_refine(id, &name, raw_input, state, ctx);
    }
    state.emitted.insert(id.clone());
    state.tool_use_cache.insert(id.clone(), block.clone());
    if name != "TodoWrite" && !tools::is_task_tool(&name) {
        state.pending_hooks.insert(id.clone());
    }

    let input = raw_input.clone().unwrap_or(Value::Null);
    let shape = tools::tool_shape(&name, &input, state.cwd.as_deref());
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
    crate::tool_meta::stamp_parent(&mut meta, ctx.parent.as_deref());
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
fn tool_update_from_result(
    block: &Value,
    state: &mut MapState,
    ctx: &FrameCtx,
) -> Option<SessionUpdate> {
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

    let result_input = block.get("content").cloned().unwrap_or(Value::Null);
    let result_meta = crate::tool_meta::meta_with_parent_and_nonexec(
        &name,
        ctx.non_exec.get(&id),
        ctx.parent.as_deref(),
    );
    if name == "ExitPlanMode" {
        // The reporter (`reporters/interaction.ts:51-72`) owns the title, the
        // display copy and the raw output; an accepted plan's title replaces
        // "Approve Plan", and AIR keeps the approval text out of `rawOutput`.
        let result = tools::exit_plan_mode_result(block.get("content"), is_error, state.air);
        let mut fields = ToolCallUpdateFields::new().status(status);
        if let Some(title) = result.title {
            fields = fields.title(title);
        }
        if let Some(content) = result.content {
            fields = fields.content(content);
        }
        if let Some(raw_output) = result.raw_output {
            fields = fields.raw_output(raw_output);
        }
        let fields = state.track_tool_call_update(&id, &fields)?;
        let hook_pending = state.pending_hooks.contains(&id);
        state.finish_result_tool_tracker(&id, hook_pending);
        return Some(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(id, fields).meta(result_meta),
        ));
    }

    let mut raw_output = block.get("content").cloned();
    let mut content = tools::tool_result_content(
        &name,
        &use_input,
        ctx.structured.as_ref(),
        &result_input,
        is_error,
    );
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
    // Entry ends after the tool_result (or kept if hook is pending, M3/Decision 12).
    let hook_pending = state.pending_hooks.contains(&id);
    state.finish_result_tool_tracker(&id, hook_pending);
    Some(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id, fields).meta(result_meta),
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
    ctx: &FrameCtx,
) -> Option<SessionUpdate> {
    let input = raw_input.clone().unwrap_or(Value::Null);
    let shape = tools::tool_shape(name, &input, state.cwd.as_deref());
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
    let mut meta = state.use_meta(name, &input);
    crate::tool_meta::stamp_parent(&mut meta, ctx.parent.as_deref());
    Some(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id, fields).meta(meta),
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
            // Node drops signature-only / empty thinking blocks (`chunk.thinking`
            // truthiness, `acp-agent.js:10007-10019`, P5).
            if thinking.is_empty() {
                return None;
            }
            Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(thinking)),
            )))
        }
        Some("image") => {
            // Node maps an image by ROLE and keeps a URL image's `uri`
            // (`acp-agent.js:9994`): a base64 source carries data+mimeType, a
            // url source carries only `uri`.
            let source = block.get("source")?;
            let base64 = source["type"].as_str() == Some("base64");
            let image = ImageContent::new(
                if base64 {
                    source["data"].as_str().unwrap_or("")
                } else {
                    ""
                },
                if base64 {
                    source["media_type"].as_str().unwrap_or("")
                } else {
                    ""
                },
            )
            .uri(if base64 {
                None
            } else {
                source["url"].as_str().map(String::from)
            });
            let chunk = ContentChunk::new(ContentBlock::Image(image));
            Some(match role {
                MsgRole::Assistant => SessionUpdate::AgentMessageChunk(chunk),
                MsgRole::User => SessionUpdate::UserMessageChunk(chunk),
            })
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
    terminal_commands: &[String],
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
    // D5 (Decision 9): `system/init.terminal_slash_commands` is an array of
    // NAMES (`acp-agent.ts:12583-12586` filters by `command.name`), not objects.
    let terminal_names: HashSet<&str> = terminal_commands.iter().map(String::as_str).collect();

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

/// Serialise an `AvailableCommandsUpdate` to the raw wire JSON Node emits
/// (rust-parity-fixes 1.8 / Decision 8).
///
/// The typed crate type skips `input` when a command has none
/// (`#[skip_serializing_none]`), while the Node adapter always spreads it — a
/// command without an argument hint carries `input: null`
/// (`acp-agent.js:9645-9651`). Emitting the raw value keeps the
/// `available_commands_update` byte-identical, with no test-side normaliser.
pub fn available_commands_json(update: &AvailableCommandsUpdate) -> Value {
    let mut value = serde_json::to_value(update).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert(
            "sessionUpdate".into(),
            Value::String("available_commands_update".into()),
        );
        if let Some(commands) = map
            .get_mut("availableCommands")
            .and_then(Value::as_array_mut)
        {
            for command in commands {
                if let Value::Object(command) = command {
                    command.entry("input").or_insert(Value::Null);
                }
            }
        }
    }
    value
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
    // A subagent's stream frames stamp `parentToolUseId` on every update
    // (`stampParentToolUseId`, `acp-agent.js:10363`).
    let ctx = FrameCtx::of_frame(message);

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
            let mapped = map_consolidated_ctx(
                &Value::Array(vec![block.clone()]),
                MsgRole::Assistant,
                state,
                &ctx,
            );
            state.input_incomplete = false;
            apply_stream_message_id(mapped, &state.current_stream_message_id)
        }
        Some("content_block_delta") => {
            let delta = &event["delta"];
            if delta["type"].as_str() == Some("input_json_delta") {
                let index = event["index"].as_u64().unwrap_or(0);
                input_json_delta(state, &stream_key, index, delta, &ctx)
            } else {
                // Record the text/thinking that reaches the client so the
                // consolidated message can diff against it (Decision 18).
                // Empty deltas are skipped: they would create an entry the
                // consolidated handler's `text.length > 0` guard can never
                // consume, stalling the cursor (`acp-agent.js:5536-5548`).
                let index = event["index"].as_u64().unwrap_or(0);
                let (kind, text) = match delta["type"].as_str() {
                    Some("text_delta") => (StreamedKind::Text, delta["text"].as_str()),
                    Some("thinking_delta") => (StreamedKind::Thinking, delta["thinking"].as_str()),
                    _ => (StreamedKind::Text, None),
                };
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    state.record_streamed_delta(&stream_key, index, kind, text);
                }
                // A text/thinking delta maps like a one-block assistant message.
                apply_stream_message_id(
                    map_consolidated_ctx(
                        &Value::Array(vec![delta.clone()]),
                        MsgRole::Assistant,
                        state,
                        &ctx,
                    ),
                    &state.current_stream_message_id,
                )
            }
        }
        Some("message_start") => {
            // `message_start` carries the Anthropic API message id; capture it
            // so the streamed chunks that follow are tagged with the same id
            // (`acp-agent.js:2889`). Also ends the input stream on this lane
            // (drops leftover partial input) and clears the streamed-content
            // record: block indices restart at 0 each message, so leftover
            // entries would otherwise truncate this message (`acp-agent.js:5429`).
            state.current_stream_message_id =
                event["message"]["id"].as_str().unwrap_or("").to_string();
            state
                .streamed_inputs
                .retain(|(key, _), _| *key != stream_key);
            state.clear_streamed_blocks(&stream_key);
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
    ctx: &FrameCtx,
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
    streamed_input_refinement(state, &id, &name, input, ctx)
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
    ctx: &FrameCtx,
) -> Option<SessionUpdate> {
    let shape = tools::tool_shape(name, &input, state.cwd.as_deref());
    // The `_meta` reads the recovered partial input (`partialRefinement`).
    let mut meta = state.use_meta(name, &input);
    crate::tool_meta::stamp_parent(&mut meta, ctx.parent.as_deref());
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

    // Review PC-1 (c): a Task* tool_result releases the field tracker entry, as Node's
    // finishResult does for every tool_result (acp-agent.ts:13181; field-tracker.ts:127-132).
    #[test]
    fn task_tool_result_releases_the_field_tracker() {
        let mut state = MapState::default();
        let task =
            json!({"type": "tool_use", "id": "toolu_TR1", "name": "TaskUpdate", "input": {}});
        state.mark_emitted("toolu_TR1".to_string(), task);
        state.seed_tool_tracker(
            "toolu_TR1".to_string(),
            TrackedToolCall::initial(
                ToolKind::Other,
                "task".to_string(),
                Vec::new(),
                Vec::new(),
                None,
            ),
        );
        assert!(state.tool_tracker.contains_key("toolu_TR1"));
        let result = json!({"type": "tool_result", "tool_use_id": "toolu_TR1", "content": "ok"});
        let mut out = Vec::new();
        map_block(
            &result,
            MsgRole::User,
            &mut state,
            &mut out,
            &FrameCtx::default(),
        );
        assert!(!state.tool_tracker.contains_key("toolu_TR1"));
    }

    // PC-1.2 (acp-agent.ts ensureToolCallEmitted, 9799-9811; tools.ts hasHookCallback): a call
    // first surfaced by a permission prompt still has its PostToolUse hook pending, except
    // TodoWrite and Task* (which have no hook).
    #[test]
    fn permission_surfaced_write_keeps_hook_pending() {
        let mut state = MapState::default();
        let write = json!({"type": "tool_use", "id": "toolu_PW1", "name": "Write", "input": {}});
        state.mark_emitted("toolu_PW1".to_string(), write);
        assert!(state.pending_hooks.contains("toolu_PW1"));
        let todo = json!({"type": "tool_use", "id": "toolu_PT1", "name": "TodoWrite", "input": {}});
        state.mark_emitted("toolu_PT1".to_string(), todo);
        assert!(!state.pending_hooks.contains("toolu_PT1"));
    }
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
        let commands = map_commands_changed(&json!([{"name": "x", "description": "d"}]), &[]);
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

    /// rust-parity-fixes 1.5 (P3) — an image maps by ROLE and keeps a URL
    /// image's `uri`; a base64 source carries `data`+`mimeType` and no `uri`
    /// (Node `acp-agent.js:9994`).
    #[test]
    fn inv_58_image_role_and_uri() {
        let mut state = MapState::default();
        let assistant = map_consolidated(
            &json!([{"type":"image","source":{"type":"base64","data":"AAA","media_type":"image/png"}}]),
            MsgRole::Assistant,
            &mut state,
        );
        assert_eq!(assistant.len(), 1);
        let v = serde_json::to_value(&assistant[0]).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["content"]["type"], "image");
        assert_eq!(v["content"]["data"], "AAA");
        assert_eq!(v["content"]["mimeType"], "image/png");
        assert!(v["content"].get("uri").is_none(), "{v}");

        let user = map_consolidated(
            &json!([{"type":"image","source":{"type":"url","url":"https://example.test/a.png"}}]),
            MsgRole::User,
            &mut state,
        );
        assert_eq!(user.len(), 1);
        let v = serde_json::to_value(&user[0]).unwrap();
        assert_eq!(v["sessionUpdate"], "user_message_chunk");
        assert_eq!(v["content"]["type"], "image");
        assert_eq!(v["content"]["uri"], "https://example.test/a.png");
        assert_eq!(v["content"]["data"], "");
        assert_eq!(v["content"]["mimeType"], "");
    }

    /// rust-parity-fixes 1.6 (P5, Decision 6) — empty / signature-only thinking
    /// produces no `agent_thought_chunk`, on the consolidated and delta paths;
    /// non-empty thinking still maps (Node `acp-agent.js:10007-10019`).
    #[test]
    fn inv_59_empty_thinking_dropped() {
        for content in [
            json!([{"type": "thinking", "thinking": ""}]),
            json!([{"type": "thinking", "thinking": "", "signature": "sig"}]),
            json!([{"type": "thinking_delta", "thinking": ""}]),
        ] {
            let mut state = MapState::default();
            let updates = map_consolidated(&content, MsgRole::Assistant, &mut state);
            assert!(
                updates.is_empty(),
                "empty thinking must not emit: {updates:?}"
            );
        }

        let mut state = MapState::default();
        let delta = json!({"type":"stream_event","event":{"type":"content_block_delta",
            "index":0,"delta":{"type":"thinking_delta","thinking":""}}});
        assert!(
            map_stream_event(&delta, &mut state).is_empty(),
            "an empty thinking_delta must not emit"
        );

        let non_empty = json!([{"type": "thinking", "thinking": "reasoning"}]);
        let mut state = MapState::default();
        let updates = map_consolidated(&non_empty, MsgRole::Assistant, &mut state);
        assert_eq!(updates.len(), 1);
        assert_eq!(kind(&updates[0]), "agent_thought_chunk");
    }

    /// rust-parity-fixes 1.7 (B1/X9, Decision 18) — a streamed block that
    /// covers the consolidated text in full produces nothing; consuming it
    /// resets the record, so the next message is not diffed against it
    /// (`unstreamedRemainder`, `acp-agent.js:519-545,5835-5838`).
    #[test]
    fn inv_68_unstreamed_remainder_full_stream_covers_it() {
        let mut state = MapState::default();
        for text in ["Hello ", "there"] {
            let delta = json!({"type":"stream_event","parent_tool_use_id":null,
                "event":{"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":text}}});
            let _ = map_stream_event(&delta, &mut state);
        }
        let frame = json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"Hello there"}]}});
        assert!(
            map_consolidated_assistant(&frame, &mut state).is_empty(),
            "text already streamed in full must not be forwarded again"
        );
        // The record was consumed: a second identical message is not deduped
        // against the first message's stream.
        let again = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(again.len(), 1);
        assert_eq!(kind(&again[0]), "agent_message_chunk");
    }

    /// rust-parity-fixes 1.7 (Decision 18) — a stream cut short mid-block
    /// forwards only the un-streamed tail (`acp-agent.js:535-543`).
    #[test]
    fn inv_68_unstreamed_remainder_partial_stream() {
        let mut state = MapState::default();
        let delta = json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"Hello "}}});
        let _ = map_stream_event(&delta, &mut state);
        let frame = json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"Hello there"}]}});
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(updates.len(), 1, "{updates:?}");
        let v = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["content"]["text"], "there", "{v}");
        assert_eq!(v["messageId"], "msg_1", "{v}");
    }

    /// rust-parity-fixes 1.7 (Decision 18) — with no streamed record a
    /// consolidated assistant text is forwarded whole (a non-streaming
    /// gateway); thinking is diffed the same way on its own lane.
    #[test]
    fn inv_68_unstreamed_remainder_none_and_thinking() {
        let mut state = MapState::default();
        let frame = json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"thinking","thinking":"Think more"},
                {"type":"text","text":"Hello there"}]}});
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(updates.len(), 2, "{updates:?}");
        assert_eq!(kind(&updates[0]), "agent_thought_chunk");
        assert_eq!(kind(&updates[1]), "agent_message_chunk");

        // A streamed thinking block only consumes the matching thinking block.
        let mut state = MapState::default();
        let delta = json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"Think "}}});
        let _ = map_stream_event(&delta, &mut state);
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(updates.len(), 2, "{updates:?}");
        let thought = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(thought["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(thought["content"]["text"], "more", "{thought}");
        let text = serde_json::to_value(&updates[1]).unwrap();
        assert_eq!(text["content"]["text"], "Hello there", "{text}");
    }

    /// rust-parity-fixes 1.7 (Decision 18) — the diff walks the streamed
    /// record in document order: a second block of the same type is forwarded
    /// whole when the first consumed the record.
    #[test]
    fn inv_68_unstreamed_remainder_two_blocks_in_order() {
        let mut state = MapState::default();
        let delta = json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"A"}}});
        let _ = map_stream_event(&delta, &mut state);
        let frame = json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"A"},{"type":"text","text":"B"}]}});
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(updates.len(), 1, "{updates:?}");
        let v = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(v["content"]["text"], "B", "{v}");
    }

    /// rust-parity-fixes 1.7 (Decision 18) — an empty delta must not create a
    /// record entry the consolidated handler can never consume (#793).
    #[test]
    fn inv_68_unstreamed_remainder_empty_delta_does_not_stall() {
        let mut state = MapState::default();
        let empty = json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":""}}});
        let _ = map_stream_event(&empty, &mut state);
        let delta = json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"Hello"}}});
        let _ = map_stream_event(&delta, &mut state);
        let frame = json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"Hello"}]}});
        assert!(
            map_consolidated_assistant(&frame, &mut state).is_empty(),
            "an empty thinking delta must not stall the diff cursor"
        );
    }

    /// rust-parity-fixes 1.7 (Decision 18, STRICT-NULL) — a frame whose
    /// `parent_tool_use_id` is MISSING counts as nested: its text/thinking are
    /// dropped; an explicit `null` forwards them (`acp-agent.js:5829`).
    #[test]
    fn inv_68_missing_parent_field_emits_nothing() {
        let mut state = MapState::default();
        let frame = json!({"type":"assistant",
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"Hello there"},
                {"type":"thinking","thinking":"reasoning"}]}});
        assert!(
            map_consolidated_assistant(&frame, &mut state).is_empty(),
            "a missing parent_tool_use_id must not count as top level"
        );
    }

    /// rust-parity-fixes 1.7 (Decision 18) — an explicit `null` parent
    /// forwards the whole text, and images stay mapped on either path.
    #[test]
    fn inv_68_explicit_null_emits_remainder() {
        let mut state = MapState::default();
        let frame = json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"Hello there"},
                {"type":"image","source":{"type":"base64","data":"AAA","media_type":"image/png"}}]}});
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(updates.len(), 2, "{updates:?}");
        assert_eq!(kind(&updates[0]), "agent_message_chunk");
        assert_eq!(kind(&updates[1]), "agent_message_chunk");
        let text = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(text["content"]["text"], "Hello there", "{text}");
        let image = serde_json::to_value(&updates[1]).unwrap();
        assert_eq!(image["content"]["type"], "image", "{image}");
    }

    /// rust-parity-fixes 1.7 (Decision 18) — a nested assistant frame keeps
    /// only its tool/image blocks (text and thinking stay internal); its own
    /// stream lane is consumed.
    #[test]
    fn inv_68_unstreamed_remainder_subagent_text_dropped() {
        let mut state = MapState::default();
        let delta = json!({"type":"stream_event","parent_tool_use_id":"toolu_parent",
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"nested"}}});
        let _ = map_stream_event(&delta, &mut state);
        let frame = json!({"type":"assistant","parent_tool_use_id":"toolu_parent",
            "message":{"id":"msg_2","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"nested more"},
                {"type":"thinking","thinking":"hmm"},
                {"type":"tool_use","id":"t_nested","name":"Bash","input":{"command":"ls"}}]}});
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert!(
            updates.iter().all(|u| !matches!(
                u,
                SessionUpdate::AgentMessageChunk(_) | SessionUpdate::AgentThoughtChunk(_)
            )),
            "nested text/thinking must stay internal: {updates:?}"
        );
        assert_eq!(kind(&updates[0]), "tool_call");
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
        let result = map_commands_changed(&commands, &[]);
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

        // Decision 8: the raw wire JSON always carries `input` — `null` for a
        // command without an argument hint (the typed update omits it).
        let wire = available_commands_json(&result);
        let cmds = wire["availableCommands"].as_array().expect("array");
        let compact = cmds
            .iter()
            .find(|c| c["name"] == "compact")
            .expect("compact");
        assert_eq!(compact["input"], Value::Null, "{compact}");
        let config = cmds.iter().find(|c| c["name"] == "config").expect("config");
        assert_eq!(config["input"]["hint"], "[key] [value]", "{config}");
        assert_eq!(wire["sessionUpdate"], "available_commands_update");

        // Empty commands -> only the appended mcp + remote-control.
        let empty = map_commands_changed(&json!([]), &[]);
        assert_eq!(
            names(&empty),
            vec!["mcp", "remote-control"],
            "empty list yields just the two appended commands (text-only fixture)"
        );

        // A CLI already offering a non-terminal remote-control -> no duplicate:
        // the CLI's entry stays in place and only mcp is appended after it.
        let with_rc = map_commands_changed(
            &json!([{"name": "remote-control", "description": "already here"}]),
            &[],
        );
        assert_eq!(
            names(&with_rc),
            vec!["remote-control", "mcp"],
            "must not append a second remote-control when the CLI already has one"
        );
        // But a terminal-bound remote-control is dropped and the adapter's appended.
        let rc_terminal = map_commands_changed(
            &json!([{"name": "remote-control", "description": "t"}]),
            &["remote-control".to_string()],
        );
        assert_eq!(
            names(&rc_terminal),
            vec!["mcp", "remote-control"],
            "a terminal remote-control is replaced by the adapter's"
        );
    }

    /// D5 (Decision 9) — `terminal_slash_commands` is a list of NAMES from
    /// `system/init`; those commands are hidden from the advertisement (S-04)
    /// and the adapter's own remote-control entry stands in for the CLI's.
    #[test]
    fn inv_73_terminal_commands_filter_by_name() {
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
        let commands = json!([
            {"name": "compact", "description": "Compress the conversation history"},
            {"name": "doctor", "description": "Run a system diagnostics check"},
            {"name": "color", "description": "Set the terminal color"},
            {"name": "help", "description": "Display help for commands"},
        ]);
        let terminal = vec!["doctor".to_string(), "color".to_string()];
        let result = map_commands_changed(&commands, &terminal);
        assert_eq!(
            names(&result),
            vec!["compact", "help", "mcp", "remote-control"],
            "terminal-bound names are filtered by name before the MCP rename"
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

    /// M3 / Decision 12 (INV-64) — the field tracker entry is retained after
    /// `tool_result` when a PostToolUse hook is pending, so hook updates can
    /// deduplicate content against what the client holds. The tracker entry is
    /// only dropped once the hook callback finishes.
    #[test]
    fn inv_64_tracker_kept_until_hook() {
        let mut state = MapState::default();
        let start = json!([{
            "type": "tool_use", "id": "toolu_HK1", "name": "Bash", "input": {"command": "ls"}
        }]);
        let _ = map_consolidated(&start, MsgRole::Assistant, &mut state);
        assert_eq!(state.tracked_tool_calls(), 1);
        assert!(state.pending_hooks.contains("toolu_HK1"));

        // tool_result arrives with hook pending
        let result_chunk = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_HK1",
            "content": "output"
        });
        let _ = tool_update_from_result(&result_chunk, &mut state, &FrameCtx::default());

        // Tracker entry is KEPT because hook is pending
        assert_eq!(state.tracked_tool_calls(), 1);

        // Hook arrives and finishes
        state.finish_hook_tool_tracker("toolu_HK1");

        // Now tracker entry is removed
        assert_eq!(state.tracked_tool_calls(), 0);
    }

    /// D14 (inv_76) — a subagent's `tool_call` and its refining / completing
    /// updates carry `_meta.claudeCode.parentToolUseId`; the top-level call does
    /// not, and `null`/missing means no stamp (`acp-agent.js:552,9958,10363`).
    #[test]
    fn inv_76_parent_stamped_on_tool_call_and_updates() {
        let mut state = MapState::default();
        let child_frame = json!({
            "type": "assistant",
            "parent_tool_use_id": "toolu_TASK",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_CHILD", "name": "Read",
                 "input": {"file_path": "/work/a.txt"}}
            ]}
        });
        let updates = map_consolidated_assistant(&child_frame, &mut state);
        let SessionUpdate::ToolCall(tc) = &updates[0] else {
            panic!("expected a tool_call, got {:?}", updates[0]);
        };
        let meta = serde_json::to_value(tc.meta.clone().unwrap_or_default()).unwrap();
        assert_eq!(meta["claudeCode"]["parentToolUseId"], "toolu_TASK");
        assert_eq!(meta["claudeCode"]["toolName"], "Read");

        // The completing update keeps the parent stamp.
        let result_frame = json!({
            "type": "user",
            "parent_tool_use_id": "toolu_TASK",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_CHILD", "content": "file"}
            ]}
        });
        let updates = map_user_frame(&result_frame, &mut state);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected a tool_call_update, got {:?}", updates[0]);
        };
        let meta = serde_json::to_value(update.meta.clone().unwrap_or_default()).unwrap();
        assert_eq!(meta["claudeCode"]["parentToolUseId"], "toolu_TASK");

        // A top-level frame carries no parent key at all.
        let mut state = MapState::default();
        let top_frame = json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_TOP", "name": "Bash",
                 "input": {"command": "ls"}}
            ]}
        });
        let updates = map_consolidated_assistant(&top_frame, &mut state);
        let SessionUpdate::ToolCall(tc) = &updates[0] else {
            panic!("expected a tool_call");
        };
        let meta = serde_json::to_value(tc.meta.clone().unwrap_or_default()).unwrap();
        assert!(
            meta["claudeCode"].get("parentToolUseId").is_none(),
            "{meta}"
        );

        // A stream event of the subagent stamps the parent too.
        let mut state = MapState::default();
        let start = json!({
            "type": "stream_event",
            "parent_tool_use_id": "toolu_TASK",
            "event": {"type": "content_block_start", "index": 0,
                      "content_block": {"type": "tool_use", "id": "toolu_S",
                                        "name": "Bash", "input": {}}}
        });
        let updates = map_stream_event(&start, &mut state);
        let SessionUpdate::ToolCall(tc) = &updates[0] else {
            panic!("expected a tool_call");
        };
        let meta = serde_json::to_value(tc.meta.clone().unwrap_or_default()).unwrap();
        assert_eq!(meta["claudeCode"]["parentToolUseId"], "toolu_TASK");
    }

    /// D15 (inv_77) — the `tool_result_meta` sidecar stamps
    /// `nonExecutionKind` / `userFeedback` on the completing update, in Node's
    /// key order, and an unknown kind is preserved (`acp-agent.js:10124,10162`).
    #[test]
    fn inv_77_non_execution_kind_and_feedback() {
        let mut state = MapState::default();
        let use_frame = json!([{
            "type": "tool_use", "id": "toolu_DENY", "name": "Edit",
            "input": {"file_path": "/work/a.txt", "old_string": "a", "new_string": "b"}
        }]);
        let _ = map_consolidated(&use_frame, MsgRole::Assistant, &mut state);
        let frame = json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_DENY", "is_error": true,
                 "content": "The user doesn't want to proceed with this tool use."}
            ]},
            "tool_result_meta": [
                {"id": "toolu_DENY", "non_execution_kind": "user-rejected",
                 "user_feedback": "not now"}
            ]
        });
        let updates = map_user_frame(&frame, &mut state);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected a tool_call_update, got {:?}", updates[0]);
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
        // The denied Edit shows the fenced error text.
        let content = update.fields.content.clone().unwrap_or_default();
        assert_eq!(content.len(), 1);
        let meta = serde_json::to_value(update.meta.clone().unwrap_or_default()).unwrap();
        let keys: Vec<&String> = meta["claudeCode"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["toolName", "nonExecutionKind", "userFeedback"],
            "Node's key order: toolName, nonExecutionKind, userFeedback"
        );
        assert_eq!(meta["claudeCode"]["toolName"], "Edit");
        assert_eq!(meta["claudeCode"]["nonExecutionKind"], "user-rejected");
        assert_eq!(meta["claudeCode"]["userFeedback"], "not now");
    }

    #[test]
    fn inv_77_unknown_kind_preserved() {
        let mut state = MapState::default();
        let use_frame = json!([{
            "type": "tool_use", "id": "toolu_X", "name": "Bash", "input": {"command": "ls"}
        }]);
        let _ = map_consolidated(&use_frame, MsgRole::Assistant, &mut state);
        let frame = json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_X", "content": "ok"}
            ]},
            "tool_result_meta": [
                {"id": "toolu_X", "non_execution_kind": "some-future-kind"}
            ]
        });
        let updates = map_user_frame(&frame, &mut state);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected a tool_call_update");
        };
        let meta = serde_json::to_value(update.meta.clone().unwrap_or_default()).unwrap();
        assert_eq!(meta["claudeCode"]["nonExecutionKind"], "some-future-kind");
        assert!(meta["claudeCode"].get("userFeedback").is_none());
    }

    /// D4 — the single-`tool_result` guard: a message with two results carries
    /// no structured result, one result does (`acp-agent.js:9962-9971`).
    #[test]
    fn inv_75_user_frame_structured_single_result_guard() {
        let mut state = MapState::default();
        let use_frame = json!([{
            "type": "tool_use", "id": "toolu_R1", "name": "Read", "input": {"file_path": "/w/a"}
        }]);
        let _ = map_consolidated(&use_frame, MsgRole::Assistant, &mut state);
        let frame = json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_R1",
                 "content": "     1\traw\n<system-reminder>x</system-reminder>"}
            ]},
            "tool_use_result": {"type": "text",
                "file": {"content": "clean", "startLine": 1, "numLines": 1, "totalLines": 1}}
        });
        let updates = map_user_frame(&frame, &mut state);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected a tool_call_update");
        };
        let content = update.fields.content.clone().unwrap_or_default();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```\n1\tclean\n```");

        // Two tool_result blocks: the sidecar cannot be attributed.
        let mut state = MapState::default();
        for id in ["toolu_A", "toolu_B"] {
            let _ = map_consolidated(
                &json!([{"type": "tool_use", "id": id, "name": "Read",
                         "input": {"file_path": "/w/a"}}]),
                MsgRole::Assistant,
                &mut state,
            );
        }
        let frame = json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_A", "content": "one"},
                {"type": "tool_result", "tool_use_id": "toolu_B", "content": "two"}
            ]},
            "tool_use_result": {"type": "text", "file": {"content": "clean"}}
        });
        let updates = map_user_frame(&frame, &mut state);
        for update in &updates {
            let SessionUpdate::ToolCallUpdate(update) = update else {
                continue;
            };
            let content = update.fields.content.clone().unwrap_or_default();
            let ToolCallContent::Content(c) = &content[0] else {
                panic!("expected content");
            };
            let ContentBlock::Text(t) = &c.content else {
                panic!("expected text");
            };
            assert_eq!(t.text.as_str(), "```\none\n```");
            break;
        }
    }
}
