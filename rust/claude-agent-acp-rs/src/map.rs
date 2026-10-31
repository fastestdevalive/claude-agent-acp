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
//! `CurrentModeUpdate`, `AvailableCommandsUpdate`, `Plan`, `SessionInfoUpdate` (via raw session_info_update).
//!
//! Never emitted (`skipped-deliberate`, B1): `ConfigOptionUpdate`, `UsageUpdate`.
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
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ContentBlock, ContentChunk,
    CurrentModeUpdate, ImageContent, MessageId, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, SessionModeId, SessionUpdate, Terminal, TextContent, ToolCall,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, UnstructuredCommandInput,
};
use serde_json::{json, Value};

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
    emitted: OrderedIds,
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
    /// The client negotiated AIR `diffPatch` (T-40): a Write's live tool call
    /// shows no input diff, the approval preview and the PostToolUse hook carry
    /// the exact patch instead. Never set for a replay or an ACP v2 client.
    diff_patch: bool,
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
    /// The Agent/Task tool_use id that started the subagent making a tool call
    /// (`recordToolCallParent`, `acp-agent.ts:12828-12839`), in insertion order
    /// with a 1000-entry cap. Upstream's cancel paths (`closeOpenToolCalls` /
    /// `closeCycleToolCalls`, `acp-agent.ts:5263-5277,5401`) read it to keep a
    /// call with the subagent it belongs to; those consumers are NOT ported yet
    /// (F17), so nothing outside the tests reads this memory today.
    tool_call_parents: Vec<(String, String)>,
    /// The tool name of a tool result kept after the tool-use cache drops the
    /// entry (`rememberResolvedToolNames`, `acp-agent.ts:12927-12944`), in
    /// insertion order with a 1000-entry cap. Upstream reads it in
    /// `asyncTasks.toolNameOf` (`acp-agent.ts:4538`) and clears it on
    /// `conversation_reset` (`acp-agent.ts:8199`); those consumers are NOT
    /// ported yet (F17), so nothing outside the tests reads this memory today.
    resolved_tool_names: Vec<(String, String)>,
    /// B-29: the ACP messageId -> SDK uuid map (`Session.messageIdToUuid`,
    /// `acp-agent.ts:7712-7719`). Fork reads it to resolve an AIR fork point.
    message_ids: crate::message_ids::MessageIdMap,
    /// Whether the client gets the text/thinking of a subagent
    /// (`forwardsSubagentText`, `acp-agent.ts:10947-10953`): the historical
    /// `_meta["subagent-transcript"]` capability, native subagent sessions, or
    /// the `forwardSubagentText` session option. Off by default.
    forward_subagent_text: bool,
    /// The client choices of a tool report (`ClientCapabilities`): the terminal
    /// channel of a command (`renderer.ts:173-175`).
    caps: crate::client_caps::ClientCapabilities,
    /// The `_meta.claudeCode` keys the last progress beat of each open tool call
    /// carried, so a repeated beat is dropped (the field tracker's merged-meta
    /// record, `field-tracker.ts:146-182`).
    progress_meta: HashMap<String, serde_json::Map<String, Value>>,
    /// The Agent/Task calls of the live background subagents (see
    /// [`MapState::set_background_subagents`]).
    background_subagents: HashSet<String>,
    /// Tool-call ids whose closing report (`unfinished::closing_report`) closed
    /// a call that the turn end or a cancel stopped. Node's renderer reports
    /// such a call `cancelled` to a v2 client and `failed` to every other one
    /// (`AcpToolCallRenderer.unfinished`, `renderer.ts:650-668`); the typed v1
    /// status has no `cancelled`, so the session rewrites it on the v2 send.
    stopped_closing: Vec<String>,
    /// Tool-call ids whose result Node reports `cancelled` to a v2 client:
    /// a command result whose structured output says `interrupted`, or an
    /// error result of the work a cancel stopped (`resultStatus`,
    /// `renderer.ts:193-204`). The typed v1 status has no `cancelled`, so the
    /// session rewrites it on the v2 send.
    aborted_results: Vec<String>,
    /// The ACP session that a permission request created a tool call in
    /// (`session.eagerToolCallSessions`, `acp-agent.ts:1266`): the child
    /// session of an announced subagent, else the root. `sendUpdate` passes it
    /// to the native subagent routing as the forced session, and the final
    /// status of the call drops it (`acp-agent.ts:4560,4630`). Insertion
    /// ordered with the tool-memory cap.
    eager_tool_call_sessions: Vec<(String, String)>,
    /// The tool-use cache's ids in first-insertion order: a JavaScript object
    /// keeps the position of an existing key on re-assignment, and
    /// `sendMessageResume` scans it latest-first.
    tool_use_order: Vec<String>,
    /// The adapter logger, for the diagnostics the native subagent runtime
    /// records (`NativeSubagentRuntime.logs`): they are drained at the session
    /// boundary so a long session does not accumulate them.
    logger: Option<std::sync::Arc<crate::logger::Logger>>,
    /// ACP v2 session state (F39): the foreground work, its open requests, the
    /// prompt intake replies, and the messages owed to the client
    /// (`foregroundWork`, `openUserInputs`, `v2/prompt.ts`). Inert on v1.
    pub(crate) v2: crate::turn_events::V2State,
}

/// A command tool: its output goes to the terminal channel (`BashReporter`,
/// shared with PowerShell, `reporters/index.ts:23`).
pub(crate) fn is_command_tool(name: &str) -> bool {
    matches!(name, "Bash" | "PowerShell")
}

/// An insertion-ordered set of ids, like a JavaScript `Set`: a repeated insert
/// keeps the first position, and iteration follows insertion order (a cancel
/// closes the open tool calls in the order they were announced).
#[derive(Debug, Default, Clone)]
struct OrderedIds {
    ids: Vec<String>,
}

impl OrderedIds {
    fn insert(&mut self, id: String) -> bool {
        if self.contains(&id) {
            return false;
        }
        self.ids.push(id);
        true
    }

    fn remove(&mut self, id: &str) -> bool {
        match self.ids.iter().position(|existing| existing == id) {
            Some(position) => {
                self.ids.remove(position);
                true
            }
            None => false,
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.ids.iter().any(|existing| existing == id)
    }
}

/// `MAX_TOOL_CALL_PARENTS` / `MAX_RESOLVED_TOOL_NAMES` (`acp-agent.ts:12826,12925`).
const MAX_TOOL_MEMORY: usize = 1000;

/// Insert `(id, value)` at the end of an insertion-ordered memory, dropping the
/// oldest entry past the cap. Node's `Map.delete` + `Map.set` moves a repeated
/// key to the end.
fn remember(memory: &mut Vec<(String, String)>, id: &str, value: &str) {
    if let Some(pos) = memory.iter().position(|(key, _)| key == id) {
        memory.remove(pos);
    }
    memory.push((id.to_string(), value.to_string()));
    if memory.len() > MAX_TOOL_MEMORY {
        memory.remove(0);
    }
}

/// Look up an insertion-ordered memory.
fn recall<'a>(memory: &'a [(String, String)], id: &str) -> Option<&'a str> {
    memory
        .iter()
        .find(|(key, _)| key == id)
        .map(|(_, value)| value.as_str())
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

    /// Record whether the client negotiated `diffPatch` (`capabilities.diffPatch`
    /// of the tool-call capabilities, read from `ClientCapabilities::diff_patch`).
    pub(crate) fn set_diff_patch(&mut self, diff_patch: bool) {
        self.diff_patch = diff_patch;
    }

    /// Whether the client negotiated `diffPatch`.
    pub(crate) fn diff_patch(&self) -> bool {
        self.diff_patch
    }

    /// Whether the client reads the ExitPlanMode plan from
    /// `rawInput.planFilePath` (`jetbrains.air` `planFile`).
    pub(crate) fn plan_file(&self) -> bool {
        self.caps.plan_file
    }

    /// Whether the client renders `rawInput` itself (`jetbrains.air`
    /// `rawInputRendering`): it gets no display copy of the input.
    pub(crate) fn caps_raw_input_rendering(&self) -> bool {
        self.caps.raw_input_rendering
    }

    /// Record the client's tool-report capabilities (the terminal channel).
    pub(crate) fn set_client_caps(&mut self, caps: crate::client_caps::ClientCapabilities) {
        self.caps = caps;
    }

    /// Whether the tool call `name` gets a display terminal: a command, for a
    /// client that renders terminals (`hasTerminal`, `renderer.ts:173-175`).
    pub(crate) fn has_terminal(&self, name: &str) -> bool {
        self.caps.has_terminal(is_command_tool(name))
    }

    /// Put the display terminal into the content of a command's tool call: the
    /// terminal marker replaces the display copy (`toolUseContent`,
    /// `renderer.ts:750-757`).
    pub(crate) fn apply_terminal_content(
        &self,
        name: &str,
        id: &str,
        shape: &mut tools::ToolShape,
    ) {
        if self.has_terminal(name) {
            shape.content = vec![ToolCallContent::Terminal(Terminal::new(id.to_string()))];
        }
    }

    /// Record the `_meta.claudeCode` keys of a progress beat and tell whether any
    /// differs from what the last beat of `id` carried (the merged-meta record of
    /// `ToolCallFieldTracker`, `field-tracker.ts:146-182`).
    pub(crate) fn progress_meta_changed(
        &mut self,
        id: &str,
        claude_code: &serde_json::Map<String, Value>,
    ) -> bool {
        let seen = self.progress_meta.entry(id.to_string()).or_default();
        let mut changed = false;
        for (key, value) in claude_code {
            if seen.get(key) != Some(value) {
                seen.insert(key.clone(), value.clone());
                changed = true;
            }
        }
        changed
    }

    /// The Agent/Task calls of the background subagents that still run
    /// (`runsInBackground`): the machine's live set, refreshed after every
    /// session frame. A cancel leaves such a subagent's calls open.
    pub(crate) fn set_background_subagents(&mut self, parents: HashSet<String>) {
        self.background_subagents = parents;
    }

    /// Whether the subagent that the Agent call `parent` started runs in the
    /// background and has not ended (`runsInBackground`).
    pub(crate) fn runs_in_background(&self, parent: &str) -> bool {
        self.background_subagents.contains(parent)
    }

    /// The `_meta` that ends the display terminal of a command whose tool call
    /// gets no result (`AcpToolCallRenderer.terminalExit`, T-84): `None` for a
    /// tool that is not a command, or a client that renders no terminal.
    pub(crate) fn terminal_exit_meta(
        &self,
        name: &str,
        id: &str,
    ) -> Option<serde_json::Map<String, Value>> {
        match self.caps.terminal_exit_meta(is_command_tool(name), id)? {
            Value::Object(meta) => Some(meta),
            _ => None,
        }
    }

    /// Announce the display terminal of a command's first report
    /// (`_meta.terminal_info`, `renderer.ts:246`).
    pub(crate) fn apply_terminal_info(
        &self,
        name: &str,
        id: &str,
        meta: &mut serde_json::Map<String, Value>,
    ) {
        if let Some(Value::Object(info)) = self.caps.terminal_info_meta(is_command_tool(name), id) {
            meta.extend(info);
        }
    }

    /// The tool shape of a streamed or surfaced tool call for this client
    /// (`tool_shape_full`: AIR facts, and the `diffPatch` gating of a Write's
    /// input diff). The session replay path never goes through the live map.
    pub(crate) fn tool_shape(&self, name: &str, input: &Value) -> tools::ToolShape {
        tools::tool_shape_full(
            name,
            input,
            self.cwd.as_deref(),
            tools::ShapeOptions {
                air: self.air,
                diff_patch: self.diff_patch,
                raw_input_rendering: self.caps.raw_input_rendering,
                plan_file: self.caps.plan_file,
                ..tools::ShapeOptions::default()
            },
        )
    }

    /// The `rawInput` of a tool use for this client: an AIR client gets it
    /// without the file text that the tool call's diff holds, and a `planFile`
    /// client gets the plan file path in place of the plan text.
    pub(crate) fn raw_input(&self, name: &str, input: &Value) -> Value {
        if self.air {
            tools::air_raw_input_with(name, input, self.caps.plan_file)
        } else {
            input.clone()
        }
    }

    /// The exact patch the approval of an Edit or Write shows (upstream
    /// `previewPatchContent` in `canUseTool`), for a `diffPatch` client only.
    /// `None` keeps the standard tool-call content.
    pub(crate) fn approval_preview(&self, tool_name: &str, input: &Value) -> Option<Vec<Value>> {
        if !self.diff_patch {
            return None;
        }
        crate::diff::preview_patch_content(
            tool_name,
            input,
            self.cwd.as_deref(),
            crate::diff::PatchForm::Air,
        )
    }

    /// Show `preview` as the content of tool call `id` and pin it
    /// (`ensureToolCallEmitted` with `previewContent`, upstream
    /// `pinContent`): a tool call that is surfaced now carries it, and the
    /// streamed input that follows must not replace it with the standard diff of
    /// the Edit snippet. Only a final result (`tool_result` or the PostToolUse
    /// hook) replaces it.
    pub(crate) fn pin_approval_preview(
        &mut self,
        id: &str,
        surfaced: Option<&mut SessionUpdate>,
        preview: &[Value],
    ) {
        let Ok(content) =
            serde_json::from_value::<Vec<ToolCallContent>>(Value::Array(preview.to_vec()))
        else {
            return;
        };
        if let Some(SessionUpdate::ToolCall(call)) = surfaced {
            call.content = content.clone();
        }
        if let Some(entry) = self.tool_tracker.get_mut(id) {
            entry.content = content;
            entry.content_pinned = true;
        }
    }

    /// Whether the client gets a subagent's text/thinking
    /// (`forwardsSubagentText`): the `_meta["subagent-transcript"]` capability
    /// (C-42) or native subagent sessions. Node also forwards it for the
    /// `forwardSubagentText` session option (F2/F5b).
    pub(crate) fn set_forward_subagent_text(&mut self, forward: bool) {
        self.forward_subagent_text = forward;
    }

    /// Record the Agent/Task call that started the subagent making `id`
    /// (`recordToolCallParent`). A repeated id moves to the end of the
    /// insertion-ordered memory.
    pub(crate) fn record_tool_call_parent(&mut self, id: &str, parent: &str) {
        remember(&mut self.tool_call_parents, id, parent);
    }

    /// Forget the parent of a tool call that reached a final status
    /// (`session.toolCallParents?.delete`, `acp-agent.ts:4570`).
    pub(crate) fn forget_tool_call_parent(&mut self, id: &str) {
        self.tool_call_parents.retain(|(key, _)| key != id);
    }

    /// The Agent/Task call of the subagent that made `id`, if recorded
    /// (`session.toolCallParents?.get`, `acp-agent.ts:5268`).
    pub fn tool_call_parent(&self, id: &str) -> Option<&str> {
        recall(&self.tool_call_parents, id)
    }

    /// Remember the session a permission request created the tool call `id` in
    /// (`session.eagerToolCallSessions.set(toolCallId, notificationSessionId)`,
    /// `acp-agent.ts:9906`).
    pub(crate) fn record_eager_session(&mut self, id: &str, session: &str) {
        remember(&mut self.eager_tool_call_sessions, id, session);
    }

    /// `session.eagerToolCallSessions.get(toolCallId)` (`acp-agent.ts:4560`).
    pub(crate) fn eager_session(&self, id: &str) -> Option<String> {
        recall(&self.eager_tool_call_sessions, id).map(ToString::to_string)
    }

    /// Drop the eager session of a call that reached a final status
    /// (`session.eagerToolCallSessions.delete`, `acp-agent.ts:4630`).
    pub(crate) fn forget_eager_session(&mut self, id: &str) {
        self.eager_tool_call_sessions.retain(|(key, _)| key != id);
    }

    /// A copy of the eager-session map, for the runtime's forced-session lookup
    /// while the session's own `sendUpdate` tail holds the state mutably.
    pub(crate) fn eager_snapshot(&self) -> HashMap<String, String> {
        self.eager_tool_call_sessions.iter().cloned().collect()
    }

    /// `session.eagerToolCallSessions?.clear()` (cancel, reset, teardown).
    pub(crate) fn clear_eager_sessions(&mut self) {
        self.eager_tool_call_sessions.clear();
    }

    /// Attach the adapter logger for the native subagent runtime's diagnostics.
    pub(crate) fn set_logger(&mut self, logger: Option<std::sync::Arc<crate::logger::Logger>>) {
        self.logger = logger;
    }

    /// The adapter logger, if any.
    pub(crate) fn logger(&self) -> Option<&std::sync::Arc<crate::logger::Logger>> {
        self.logger.as_ref()
    }

    /// Forget that a tool call surfaced to the client
    /// (`session.emittedToolCalls.delete`, `acp-agent.ts:4588`): a native
    /// Agent/Task control call the routing suppressed is not a tool call the
    /// client holds, so a later permission request must be able to surface it.
    pub(crate) fn unmark_emitted(&mut self, id: &str) {
        self.emitted.remove(id);
    }

    /// The tool-use cache in first-insertion order, as
    /// `sendMessageResume` scans it (`Object.entries(toolUseCache)`).
    pub fn tool_uses_in_order(&self) -> Vec<(String, crate::native_subagents::ToolUseRecord)> {
        self.tool_use_order
            .iter()
            .filter_map(|id| {
                let tool_use = self.tool_use_cache.get(id)?;
                Some((
                    id.clone(),
                    crate::native_subagents::ToolUseRecord {
                        name: tool_use
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        input: tool_use.get("input").cloned().unwrap_or(Value::Null),
                    },
                ))
            })
            .collect()
    }

    /// Cache a `tool_use` block by id, keeping its first-insertion position
    /// (Node's `toolUseCache[chunk.id] = chunk`).
    fn cache_tool_use(&mut self, id: String, block: Value) {
        if !self.tool_use_cache.contains_key(&id) {
            self.tool_use_order.push(id.clone());
        }
        self.tool_use_cache.insert(id, block);
    }

    /// Drop a cached `tool_use` (`delete toolUseCache[id]`).
    fn uncache_tool_use(&mut self, id: &str) {
        self.tool_use_cache.remove(id);
        self.tool_use_order.retain(|existing| existing != id);
    }

    /// Keep the tool name of a tool result before the tool-use cache drops it
    /// (`rememberResolvedToolNames`).
    fn remember_resolved_tool_name(&mut self, id: &str, name: &str) {
        remember(&mut self.resolved_tool_names, id, name);
    }

    /// The remembered tool name of a resolved tool call, if any
    /// (`session.resolvedToolNames?.get`, `acp-agent.ts:4538`).
    pub fn resolved_tool_name(&self, id: &str) -> Option<&str> {
        recall(&self.resolved_tool_names, id)
    }

    /// Forget the streamed text/thinking record of the subagent that the Agent
    /// call `parent` started (`streamedBlocksByParent.delete`).
    pub(crate) fn forget_stream_record(&mut self, parent: &str) {
        self.streamed_blocks.remove(parent);
    }

    /// Forget the remembered tool names (`session.resolvedToolNames?.clear()`,
    /// `acp-agent.ts:8199`) after a conversation reset.
    pub(crate) fn clear_resolved_tool_names(&mut self) {
        self.resolved_tool_names.clear();
    }

    /// B-29: record one consolidated message's ACP id -> SDK uuid mapping.
    pub fn record_message_id(&mut self, message_id: &str, uuid: &str) {
        self.message_ids.set(message_id, uuid);
    }

    /// B-29: the session's map, for `session/fork` (a clone; no lock is held).
    pub fn message_ids(&self) -> crate::message_ids::MessageIdMap {
        self.message_ids.clone()
    }

    /// B-54: merge the history replay's map into the session's.
    pub fn seed_message_ids(&mut self, map: crate::message_ids::MessageIdMap) {
        self.message_ids.extend(map);
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

    /// True if the session was cancelled (stopped work window is open).
    /// Whether this session reports ACP v2 state (an ACP v2 connection).
    pub(crate) fn v2_reporting(&self) -> bool {
        self.v2.reporting()
    }

    pub(crate) fn is_stopped_work(&self) -> bool {
        self.stopped_work
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
        self.uncache_tool_use(id);
        self.tool_tracker.remove(id);
        self.progress_meta.remove(id);
        self.pending_hooks.remove(id);
        self.tool_call_parents.retain(|(key, _)| key != id);
        self.books.forget(id);
    }

    /// The tool calls the client holds open, in the order they were announced
    /// (`[...session.emittedToolCalls]`).
    pub(crate) fn emitted_ids(&self) -> Vec<String> {
        self.emitted.ids.clone()
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
        self.cache_tool_use(id, block);
    }

    /// Retrieve the cached tool_use block for an id, if present.
    pub(crate) fn tool_use_from_cache(&self, id: &str) -> Option<&Value> {
        self.tool_use_cache.get(id)
    }

    /// Seed the field tracker for a freshly-surfaced `tool_call` (2.9 / X-7).
    pub(crate) fn seed_tool_tracker(&mut self, id: String, tracked: TrackedToolCall) {
        self.tool_tracker.insert(id, tracked);
    }

    /// Record the merged `_meta` keys of a surfaced `tool_call`, so a later
    /// update that repeats them is not sent (`ToolCallFieldTracker.apply` on a
    /// `tool_call`, `field-tracker.ts:64-77`).
    pub(crate) fn seed_tool_meta(&mut self, id: &str, meta: &serde_json::Map<String, Value>) {
        if let Some(entry) = self.tool_tracker.get_mut(id) {
            entry.record_meta(Some(meta));
        }
    }

    /// [`MapState::track_tool_call_update`] with the candidate `_meta`: an
    /// update whose fields are unchanged is still sent when a merged `_meta`
    /// key changed (or a key outside the merged namespaces is present).
    pub(crate) fn track_tool_call_update_with_meta(
        &mut self,
        id: &str,
        fields: &ToolCallUpdateFields,
        meta: Option<&serde_json::Map<String, Value>>,
    ) -> Option<ToolCallUpdateFields> {
        match self.tool_tracker.get_mut(id) {
            Some(entry) => {
                let meta_changed = entry.record_meta(meta);
                let changed = entry.apply(fields, false);
                if changed.is_empty() && !meta_changed {
                    None
                } else {
                    Some(changed.into_fields())
                }
            }
            None => Some(fields.clone()),
        }
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
        self.track_update(id, fields, false)
    }

    /// [`MapState::track_tool_call_update`] for a final result (`tool_result`):
    /// it may replace a pinned approval patch (`replacePinnedContent`).
    pub(crate) fn track_final_tool_call_update(
        &mut self,
        id: &str,
        fields: &ToolCallUpdateFields,
    ) -> Option<ToolCallUpdateFields> {
        self.track_update(id, fields, true)
    }

    fn track_update(
        &mut self,
        id: &str,
        fields: &ToolCallUpdateFields,
        replace_pinned: bool,
    ) -> Option<ToolCallUpdateFields> {
        match self.tool_tracker.get_mut(id) {
            Some(entry) => {
                let changed = entry.apply(fields, replace_pinned);
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
                self.progress_meta.remove(id);
            }
        }
    }

    /// Mark the end of the PostToolUse hook callback in the field tracker (M3/Decision 12).
    /// The entry is removed if the tool_result was already seen.
    pub(crate) fn finish_hook_tool_tracker(&mut self, id: &str) {
        if let Some(entry) = self.tool_tracker.get(id) {
            if entry.result_seen {
                self.tool_tracker.remove(id);
                self.progress_meta.remove(id);
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
                // The final change of the tool replaces a pinned approval patch.
                entry.content_pinned = false;
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

    /// Run a raw PostToolUse update through the AIR changed-`_meta` filter, which
    /// upstream applies to every final notification: the `toolName` the
    /// `tool_call` already sent goes, and an update with nothing left is `None`.
    /// Any other client gets the update as it is.
    pub(crate) fn filter_hook_meta(&mut self, update: Value) -> Option<Value> {
        if !self.air {
            return Some(update);
        }
        let Ok(typed) = serde_json::from_value::<SessionUpdate>(update.clone()) else {
            return Some(update);
        };
        let filtered = self.meta_filter.apply(typed)?;
        Some(serde_json::to_value(filtered).unwrap_or(update))
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
        self.progress_meta.clear();
        self.pending_hooks.clear();
        self.stopped_closing.clear();
        self.aborted_results.clear();
    }

    /// Remember that `id`'s result is one Node calls `aborted`
    /// (`resultStatus`): a v2 client gets `cancelled` for it.
    pub(crate) fn note_aborted_result(&mut self, id: &str) {
        if !self.aborted_results.iter().any(|existing| existing == id) {
            self.aborted_results.push(id.to_string());
        }
    }

    /// Take the aborted mark of `id` (the report that carries it is being sent
    /// now).
    pub(crate) fn take_aborted_result(&mut self, id: &str) -> bool {
        let Some(position) = self
            .aborted_results
            .iter()
            .position(|existing| existing == id)
        else {
            return false;
        };
        self.aborted_results.remove(position);
        true
    }

    /// Remember that `id`'s closing report is one Node would call `stopped`
    /// (`AcpToolCallRenderer.unfinished`): a v2 client gets `cancelled` for it.
    pub(crate) fn note_stopped_closing(&mut self, id: &str) {
        if !self.stopped_closing.iter().any(|existing| existing == id) {
            self.stopped_closing.push(id.to_string());
        }
    }

    /// Take the stopped-closing mark of `id` (the report that carries it is
    /// being sent now).
    pub(crate) fn take_stopped_closing(&mut self, id: &str) -> bool {
        let Some(position) = self
            .stopped_closing
            .iter()
            .position(|existing| existing == id)
        else {
            return false;
        };
        self.stopped_closing.remove(position);
        true
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
    /// The `claudeCode` and `jetbrains.air` keys the client holds, merged by
    /// key (`field-tracker.ts:22-32,146-182`).
    merged_meta: HashMap<String, String>,
    /// The content is an exact approval patch (`contentPinned`).
    content_pinned: bool,
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
            merged_meta: HashMap::new(),
            content_pinned: false,
            result_seen: false,
        }
    }

    /// Record the merged `_meta` keys of an update; returns whether any key
    /// changed (`recordMergedMeta`, `field-tracker.ts:156-182`). A `_meta` key
    /// outside the merged namespaces always counts as a change.
    fn record_meta(&mut self, meta: Option<&serde_json::Map<String, Value>>) -> bool {
        let Some(meta) = meta else {
            return false;
        };
        let mut changed = meta
            .keys()
            .any(|key| key != "claudeCode" && key != "jetbrains");
        let mut record = |key: String, value: &Value, changed: &mut bool| {
            let json = value.to_string();
            if self.merged_meta.get(&key) != Some(&json) {
                self.merged_meta.insert(key, json);
                *changed = true;
            }
        };
        if let Some(values) = meta.get("claudeCode").and_then(Value::as_object) {
            for (key, value) in values {
                record(format!("claudeCode.{key}"), value, &mut changed);
            }
        }
        if let Some(jetbrains) = meta.get("jetbrains").and_then(Value::as_object) {
            for (key, value) in jetbrains {
                if key == "air" {
                    if let Some(air) = value.as_object() {
                        for (air_key, air_value) in air {
                            record(format!("jetbrains.air.{air_key}"), air_value, &mut changed);
                        }
                    }
                } else {
                    record(format!("jetbrains.{key}"), value, &mut changed);
                }
            }
        }
        changed
    }

    /// Compare `fields` against the held values; record and return the changed
    /// subset (updating the held state).
    fn apply(&mut self, fields: &ToolCallUpdateFields, replace_pinned: bool) -> Changed {
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
            // An exact approval patch stays until a final result replaces it.
            if self.content_pinned && !replace_pinned {
                // dropped: the client keeps the pinned patch
            } else {
                self.content_pinned = false;
                if v != &self.content {
                    changed.content = Some(v.clone());
                    self.content = v.clone();
                }
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
    /// The frame is a result of the work a cancel stopped
    /// (`options.cancelled`, `acp-agent.ts:12984-12990`): an error result then
    /// ends a run the cancel aborted, whatever kind the SDK stamped on it.
    cancelled: bool,
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
    fn of_user_frame(frame: &Value, content: &Value, cancelled: bool) -> Self {
        let mut ctx = Self::of_frame(frame);
        ctx.cancelled = cancelled;
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

/// [`map_consolidated`] with only the frame's parent attribution
/// (`parentToolUseId`): replay maps a nested message's blocks with its parent so
/// every update carries `_meta.claudeCode.parentToolUseId`
/// (`stampParentToolUseId`, `acp-agent.ts:13356`). Replay has no structured
/// `tool_use_result` or `tool_result_meta` sidecar.
pub fn map_consolidated_with_parent(
    content: &Value,
    role: MsgRole,
    state: &mut MapState,
    parent: Option<&str>,
) -> Vec<SessionUpdate> {
    map_consolidated_ctx(
        content,
        role,
        state,
        &FrameCtx {
            parent: parent.map(str::to_string),
            ..FrameCtx::default()
        },
    )
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
    map_user_frame_cancelled(frame, state, false)
}

/// [`map_user_frame`] for a result of the work a cancel stopped
/// (`toAcpNotifications` with `options.cancelled`, `acp-agent.ts:12984-12990`,
/// sent by `sendInterruptedToolResults`): an error result then ends a run the
/// cancel aborted, which a v2 client gets as `cancelled`.
pub fn map_user_frame_cancelled(
    frame: &Value,
    state: &mut MapState,
    cancelled: bool,
) -> Vec<SessionUpdate> {
    remember_user_frame_tool_names(frame, state);
    let content = frame
        .pointer("/message/content")
        .cloned()
        .unwrap_or(Value::Null);
    let ctx = FrameCtx::of_user_frame(frame, &content, cancelled);
    let updates = match &content {
        // Map one block at a time so a caller that owns extra updates (the
        // actor's plan/suppressed resolution, session.rs) can interleave them
        // in block order like Node's `toAcpNotifications`
        // (`acp-agent.ts:13232-13253`).
        Value::Array(blocks) => blocks
            .iter()
            .flat_map(|block| map_user_block(block, state, &ctx))
            .collect(),
        _ => map_consolidated_ctx(&content, MsgRole::User, state, &ctx),
    };
    let message_id = crate::mcp_command::message_id_of(frame).unwrap_or_default();
    apply_stream_message_id(updates, &message_id)
}

/// Keep the tool names of a user frame's results before the cache drops them
/// (`rememberResolvedToolNames`, `acp-agent.ts:8020`). The actor calls it once
/// per frame before mapping the blocks one by one ([`map_user_frame_block`]).
pub fn remember_user_frame_tool_names(frame: &Value, state: &mut MapState) {
    let content = frame
        .pointer("/message/content")
        .cloned()
        .unwrap_or(Value::Null);
    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        if block["type"].as_str() != Some("tool_result") {
            continue;
        }
        let Some(id) = block["tool_use_id"].as_str() else {
            continue;
        };
        if let Some(name) = state
            .tool_use_cache
            .get(id)
            .and_then(|tool_use| tool_use["name"].as_str())
            .filter(|name| !name.is_empty())
        {
            let name = name.to_string();
            state.remember_resolved_tool_name(id, &name);
        }
    }
}

/// Map ONE block of a consolidated `user` frame with the frame attribution, so
/// the actor can interleave the resolution updates it owns in block order
/// (`acp-agent.ts:13232`). [`remember_user_frame_tool_names`] must run once per
/// frame first.
pub fn map_user_frame_block(
    frame: &Value,
    block: &Value,
    state: &mut MapState,
) -> Vec<SessionUpdate> {
    let content = frame
        .pointer("/message/content")
        .cloned()
        .unwrap_or(Value::Null);
    let ctx = FrameCtx::of_user_frame(frame, &content, false);
    let updates = map_user_block(block, state, &ctx);
    let message_id = crate::mcp_command::message_id_of(frame).unwrap_or_default();
    apply_stream_message_id(updates, &message_id)
}

/// Map one consolidated `user` block into `out` (7.1, 7.3, 7.5, 7.6).
fn map_user_block(block: &Value, state: &mut MapState, ctx: &FrameCtx) -> Vec<SessionUpdate> {
    let mut out = Vec::new();
    map_block(block, MsgRole::User, state, &mut out, ctx);
    out
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
    let forward = state.forward_subagent_text;
    let content = if state.stopped_work {
        // A post-cancel remainder is dropped.
        drop_text_thinking(&content)
    } else if is_strict_top_level(frame) || forward {
        // The main conversation always forwards its text; a client that gets a
        // subagent's transcript (C-42) forwards the nested one too
        // (`message.parent_tool_use_id === null || forwardsSubagentText()`,
        // `acp-agent.ts:7994`).
        unstreamed_remainder(&content, &streamed)
    } else {
        // Nested text/thinking stays internal for a client that does not get
        // subagent text.
        drop_text_thinking(&content)
    };
    let ctx = FrameCtx::of_frame(frame);
    let updates = stamp_chunk_parent(
        map_consolidated_ctx(&content, MsgRole::Assistant, state, &ctx),
        ctx.parent.as_deref(),
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
            // Node caches EVERY tool_use by id before branching, TodoWrite
            // included (`toolUseCache[chunk.id] = chunk`, `acp-agent.ts:13083`,
            // T-19), so a later tool_result always finds its name/input.
            state.cache_tool_use(
                block["id"].as_str().unwrap_or("").to_string(),
                block.clone(),
            );
            if name == "TodoWrite" {
                if let Some(plan) = plan_from_todo_write(block, ctx.parent.as_deref()) {
                    out.push(plan);
                }
            } else if is_task_tool(name) {
                // Task* tool_use is suppressed (no tool_call); the cache entry
                // lets the tool_result path resolve the tool name and input
                // (X12/Decision 12).
            } else if let Some(update) = tool_call_from_use(block, state, ctx) {
                out.push(update);
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
            let id = block["tool_use_id"].as_str().unwrap_or("").to_string();
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let status = if is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            // Node clears the emitted marker on EVERY tool_result
            // (`acp-agent.ts:13171`), so a permission-surfaced call is resolved
            // exactly once.
            let was_emitted = state.emitted.remove(&id);
            let cached = state.tool_use_cache.remove(&id);
            state.tool_use_order.retain(|existing| existing != &id);
            let Some(tool_use) = cached else {
                // T-20: a result whose tool_use never reached the cache. The
                // permission flow may have surfaced the tool_call before the
                // assistant message was dropped, so resolve that call with
                // `{status, rawOutput}` and no `claudeCode` meta (the tool name
                // is unknown here). Without a surfaced call there is nothing
                // pending: Node logs and drops it (`acp-agent.ts:13193-13215`).
                if was_emitted {
                    note_aborted_result(
                        state,
                        &id,
                        result_aborted(is_error, false, ctx.cancelled, ctx.non_exec.get(&id)),
                    );
                    let mut fields = ToolCallUpdateFields::new().status(status);
                    if let Some(content) = block.get("content") {
                        fields = fields.raw_output(content.clone());
                    }
                    state.books.forget(&id);
                    state.forget_tool_call_parent(&id);
                    out.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        id.clone(),
                        fields,
                    )));
                }
                // The call is fully resolved — finishResult for every
                // tool_result (`acp-agent.ts:13181`).
                state.finish_result_tool_tracker(&id, false);
                return;
            };
            let name = tool_use["name"].as_str().unwrap_or("").to_string();
            // A permission request may have surfaced a plan-rendered (TodoWrite)
            // or suppressed (Task*) tool as a real tool_call so the request
            // referenced a known id (issue #851). Those branches never emit a
            // tool_call_update, so the actor resolves the surfaced call at the
            // tool_result call site (`acp-agent.ts:13218-13245`, session.rs).
            if tools::is_task_tool(&name) {
                // Task* tool results are suppressed too (no tool_call_update);
                // the plan update is published by the actor at tool_result time
                // (X12/Decision 12).
                // PC-1.5: the call is resolved now, so the unfinished-tool books
                // must not report it as stuck.
                state.books.forget(&id);
                // Review PC-1 (c): finishResult for the suppressed result (no
                // hook is registered for Task*).
                state.finish_result_tool_tracker(&id, false);
            } else if name != "TodoWrite" {
                out.extend(tool_update_from_result(block, &tool_use, state, ctx));
            } else {
                // Plan-lane tool results never emit a tool_call_update (the plan
                // was already emitted at tool_use time).
                state.books.forget(&id);
                // Review PC-1 (c): finishResult (`acp-agent.ts:13181`) for the
                // suppressed result (no hook is registered for TodoWrite).
                state.finish_result_tool_tracker(&id, false);
            }
            state.forget_tool_call_parent(&id);
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
        // A permission request surfaced the tool call with its complete input;
        // the empty input at the stream start has nothing to add
        // (`alreadyEmitted && inputComplete === false`, `acp-agent.ts:13162`,
        // U-23). Otherwise refine rather than emit a duplicate ToolCall (7.7).
        if state.input_incomplete {
            return None;
        }
        return tool_call_update_refine(id, &name, raw_input, state, ctx);
    }
    state.emitted.insert(id.clone());
    state.cache_tool_use(id.clone(), block.clone());
    // T-15: remember the Agent/Task call that started the subagent making this
    // call (`recordToolCallParent`).
    if let Some(parent) = ctx.parent.as_deref() {
        state.record_tool_call_parent(&id, parent);
    }
    if name != "TodoWrite" && !tools::is_task_tool(&name) {
        state.pending_hooks.insert(id.clone());
    }

    let input = raw_input.clone().unwrap_or(Value::Null);
    let mut shape = state.tool_shape(&name, &input);
    state.apply_terminal_content(&name, &id, &mut shape);
    // AIR gets `rawInput` once it is complete; every other client gets the
    // input as it stands, also the empty input at the stream start.
    let sent_raw_input = if state.air && state.input_incomplete {
        None
    } else {
        raw_input.as_ref().map(|raw| state.raw_input(&name, raw))
    };
    // 2.9 (X-7): seed the field tracker with what the client now holds.
    let mut meta = state.use_meta(&name, &input);
    crate::tool_meta::stamp_parent(&mut meta, ctx.parent.as_deref());
    state.apply_terminal_info(&name, &id, &mut meta);
    if state.air && name.starts_with("mcp__") {
        meta.insert("is_mcp_tool_call".to_string(), Value::Bool(true));
    }
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
    state.seed_tool_meta(&id, &meta);
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

/// Whether Node's `resultStatus` calls a finished result aborted
/// (`renderer.ts:193-204`): a command whose structured result says
/// `interrupted`, or an error result of the work a cancel stopped / one the SDK
/// stamped `interrupted`/`cancelled`. Only a v2 client maps that to
/// `cancelled`; the typed v1 status keeps `failed`/`completed`.
fn result_aborted(
    is_error: bool,
    interrupted: bool,
    cancelled: bool,
    non_exec: Option<&crate::tool_meta::NonExecution>,
) -> bool {
    interrupted || (is_error && (cancelled || crate::tool_meta::is_aborted(non_exec)))
}

/// Mark `id`'s result for the v2 `cancelled` rewrite (`send_updates`).
fn note_aborted_result(state: &mut MapState, id: &str, aborted: bool) {
    if aborted && state.v2_reporting() {
        state.note_aborted_result(id);
    }
}

/// 7.3 — a `tool_result` block becomes a completing `ToolCallUpdate`. The
/// caller already took the cached tool_use and cleared the emitted marker (Node
/// does both at the top of its `tool_result` case).
fn tool_update_from_result(
    block: &Value,
    tool_use: &Value,
    state: &mut MapState,
    ctx: &FrameCtx,
) -> Vec<SessionUpdate> {
    if let Some(updates) = terminal_command_updates(block, tool_use, state, ctx) {
        return updates;
    }
    tool_update_from_result_single(block, tool_use, state, ctx)
}

/// The reports of a finished command on the terminal channel
/// (`renderer.ts:391-418,455-480`): the output as a report of its own first
/// (`terminal_output(_delta)`), then the status report carrying
/// `terminal_exit` (and the terminal content again for a client that is not AIR).
/// `None` when the client renders no terminal for it, or the result is not a
/// command (image output): the ordinary result report applies.
fn terminal_command_updates(
    block: &Value,
    tool_use: &Value,
    state: &mut MapState,
    ctx: &FrameCtx,
) -> Option<Vec<SessionUpdate>> {
    let name = tool_use["name"].as_str().unwrap_or("Other");
    if !state.has_terminal(name) {
        return None;
    }
    let id = block["tool_use_id"].as_str().unwrap_or("").to_string();
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw = block.get("content").cloned().unwrap_or(Value::Null);
    let tools::reporters::BashFacts::Command(command) =
        tools::reporters::bash_facts(ctx.structured.as_ref(), &raw, is_error)
    else {
        return None;
    };
    let terminal = state.caps.terminal_result(
        tool_use.get("id").and_then(Value::as_str),
        Some(&id),
        &command,
        is_error,
    )?;
    let mut updates = Vec::new();
    // The output report. A subagent's command is stamped with its parent too.
    let mut output_meta = terminal
        .output_meta
        .as_object()
        .cloned()
        .unwrap_or_default();
    crate::tool_meta::stamp_parent(&mut output_meta, ctx.parent.as_deref());
    updates.push(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id.clone(), ToolCallUpdateFields::new()).meta(output_meta),
    ));
    let mut meta = crate::tool_meta::meta_with_parent_and_nonexec(
        name,
        ctx.non_exec.get(&id),
        ctx.parent.as_deref(),
    );
    if let Some(exit) = terminal.exit_meta.get("terminal_exit") {
        meta.insert("terminal_exit".to_string(), exit.clone());
    }
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    note_aborted_result(
        state,
        &id,
        result_aborted(
            is_error,
            command.interrupted,
            ctx.cancelled,
            ctx.non_exec.get(&id),
        ),
    );
    let mut fields = ToolCallUpdateFields::new().status(status);
    if let Some(content) = terminal
        .content
        .and_then(|block| serde_json::from_value::<ToolCallContent>(block).ok())
    {
        fields = fields.content(vec![content]);
    }
    if let Some(fields) = state.track_final_tool_call_update(&id, &fields) {
        updates.push(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(id.clone(), fields).meta(meta),
        ));
    }
    let hook_pending = state.pending_hooks.contains(&id);
    state.finish_result_tool_tracker(&id, hook_pending);
    Some(updates)
}

/// [`tool_update_from_result`] without the terminal channel: one report, plus
/// the AIR plan-file clear signal of an approved ExitPlanMode.
fn tool_update_from_result_single(
    block: &Value,
    tool_use: &Value,
    state: &mut MapState,
    ctx: &FrameCtx,
) -> Vec<SessionUpdate> {
    let id = block["tool_use_id"].as_str().unwrap_or("").to_string();
    let name = tool_use["name"].as_str().unwrap_or("Other").to_string();
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };

    let use_input = tool_use.get("input").cloned().unwrap_or(Value::Null);

    let result_input = block.get("content").cloned().unwrap_or(Value::Null);
    let result_meta = crate::tool_meta::meta_with_parent_and_nonexec(
        &name,
        ctx.non_exec.get(&id),
        ctx.parent.as_deref(),
    );
    // Node's `resultStatus` reads the command facts: a structured result that
    // says `interrupted` is an aborted run even without an error.
    let interrupted = is_command_tool(&name)
        && matches!(
            tools::reporters::bash_facts(ctx.structured.as_ref(), &result_input, is_error),
            tools::reporters::BashFacts::Command(command) if command.interrupted
        );
    let aborted = result_aborted(is_error, interrupted, ctx.cancelled, ctx.non_exec.get(&id));
    if name == "ExitPlanMode" {
        // The reporter (`reporters/interaction.ts:51-72`) owns the title, the
        // display copy and the raw output; an accepted plan's title replaces
        // "Approve Plan", and AIR keeps the approval text out of `rawOutput`.
        // A `planFile` client gets the path of the plan file in place of the
        // plan text, and an approved plan clears it in a call of its own.
        let plan_file = if state.caps.plan_file {
            tools::existing_plan_file(use_input.get("planFilePath")).or_else(|| {
                ctx.structured
                    .as_ref()
                    .and_then(|structured| tools::existing_plan_file(structured.get("filePath")))
            })
        } else {
            None
        };
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
        if let Some(path) = &plan_file {
            fields = fields.raw_input(tools::plan_file_input(&use_input, path));
        }
        let mut updates = Vec::new();
        if let Some(fields) = state.track_final_tool_call_update(&id, &fields) {
            note_aborted_result(state, &id, aborted);
            updates.push(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(id.clone(), fields).meta(result_meta),
            ));
        }
        let hook_pending = state.pending_hooks.contains(&id);
        state.finish_result_tool_tracker(&id, hook_pending);
        if plan_file.is_some() && !is_error {
            updates.push(plan_file_clear_update(&id));
        }
        return updates;
    }

    let mut raw_output = block.get("content").cloned();
    let mut content = tools::tool_result_content(
        &name,
        &use_input,
        ctx.structured.as_ref(),
        &result_input,
        is_error,
    );
    // An AIR client gets the error of an agent-control tool as plain text,
    // without the fence (`AgentControlReporter.errorResult`,
    // `reporters/interaction.ts:209-219`); every other client keeps the fenced
    // fallback.
    if state.air && is_error && tools::is_agent_control_tool(&name) {
        content = tools::agent_control_error_content(&result_input);
    }
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
    if state.air && name == "NotebookEdit" {
        // `NotebookEditReporter.toolResult` (`reporters/file-edit.ts:203-212`):
        // the result text repeats the new cell source that the input holds, so
        // it stays out. A deletion has no source, so its text is shown.
        if tools::notebook_source_present(&use_input) {
            content = None;
        }
        raw_output = None;
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
    let Some(fields) = state.track_final_tool_call_update(&id, &fields) else {
        state.finish_result_tool_tracker(&id, state.pending_hooks.contains(&id));
        return Vec::new();
    };
    note_aborted_result(state, &id, aborted);
    // Entry ends after the tool_result (or kept if hook is pending, M3/Decision 12).
    let hook_pending = state.pending_hooks.contains(&id);
    state.finish_result_tool_tracker(&id, hook_pending);
    vec![SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id, fields).meta(result_meta),
    )]
}

/// The call that clears the plan file of an approved plan for AIR
/// (`renderer.ts:503-516`): a `tool_call` of its own under the
/// `<id>:plan-file-clear` id, with a blank path as its clear signal.
fn plan_file_clear_update(id: &str) -> SessionUpdate {
    SessionUpdate::ToolCall(
        ToolCall::new(format!("{id}:plan-file-clear"), "Exited Plan Mode")
            .kind(ToolKind::SwitchMode)
            .status(ToolCallStatus::Completed)
            .raw_input(json!({"planFilePath": ""})),
    )
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
    // T-15: a refinement can be the first update to name the subagent's parent
    // for a call the permission flow surfaced.
    if let Some(parent) = ctx.parent.as_deref() {
        state.record_tool_call_parent(&id, parent);
    }
    let input = raw_input.clone().unwrap_or(Value::Null);
    let mut shape = state.tool_shape(name, &input);
    state.apply_terminal_content(name, &id, &mut shape);
    let mut fields = ToolCallUpdateFields::new()
        .kind(shape.kind)
        .title(shape.title)
        .content(shape.content);
    if !shape.locations.is_empty() {
        fields = fields.locations(shape.locations);
    }
    if let Some(raw_input) = raw_input {
        fields = fields.raw_input(state.raw_input(name, &raw_input));
    }
    let mut meta = state.use_meta(name, &input);
    crate::tool_meta::stamp_parent(&mut meta, ctx.parent.as_deref());
    let fields = state.track_tool_call_update_with_meta(&id, &fields, Some(&meta))?;
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

/// 7.5 — a `TodoWrite` tool_use becomes a `Plan` snapshot. A nested (subagent)
/// frame's plan carries `_meta.claudeCode.parentToolUseId` like every other
/// update of that frame (`stampParentToolUseId`, `acp-agent.ts:13356`).
fn plan_from_todo_write(block: &Value, parent: Option<&str>) -> Option<SessionUpdate> {
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
    let plan = Plan::new(entries);
    let plan = match parent {
        Some(parent) => {
            let mut meta = serde_json::Map::new();
            crate::tool_meta::stamp_parent(&mut meta, Some(parent));
            plan.meta(meta)
        }
        None => plan,
    };
    Some(SessionUpdate::Plan(plan))
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
    map_commands_changed_with_skills(commands, terminal_commands, None, None)
}

/// The AIR `_meta` key of a skill command (`AIR_KIND_KEY`).
pub const AIR_SKILL_KIND: &str = "kind";
/// The AIR `_meta` key of a skill command's absolute `SKILL.md` path
/// (`AIR_SKILL_PATH_KEY`).
pub const AIR_SKILL_PATH: &str = "skillPath";

/// [`map_commands_changed`] with the AIR skill meta (`getAvailableSlashCommands`,
/// `acp-agent.ts:12560-12652`): an AIR client gets `_meta.jetbrains.air` with
/// `kind: "skill"` and the absolute `SKILL.md` path of every skill command it
/// can locate. Built-in commands and MCP prompts never carry it.
pub fn map_commands_changed_with_skills(
    commands: &Value,
    terminal_commands: &[String],
    air_skill_cwd: Option<&Path>,
    home: Option<&Path>,
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
            let mcp_prompt = name.ends_with(" (MCP)");
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
            // AIR gets the kind and the `SKILL.md` path of each skill command
            // (`acp-agent.ts:12623-12640`): not for a built-in command or an
            // MCP prompt, and only when the file exists on disk.
            if let Some(cwd) = air_skill_cwd {
                if !mcp_prompt && command.get("builtin").and_then(Value::as_bool) != Some(true) {
                    if let Some(path) = crate::tools::resolve_skill_path_in(name, Some(cwd), home) {
                        let meta = crate::air::with_air_meta(
                            Some(&crate::air::with_air_meta(
                                None,
                                AIR_SKILL_KIND,
                                json!("skill"),
                            )),
                            AIR_SKILL_PATH,
                            json!(path.to_string_lossy()),
                        );
                        built = built.meta(meta.as_object().cloned());
                    }
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

/// The tool reporters that unconditionally define `locations` on `tool_call`:
/// - `Read` (`reporters/read.js:17`)
/// - `Write` (`reporters/file-edit.js:14`)
/// - `Edit` (`reporters/file-edit.js:80`)
/// - `NotebookEdit` (`reporters/file-edit.js:167`)
/// - `Glob` (`reporters/search.js:13`)
///
/// Other reporters (Bash, Grep, WebSearch, etc.) leave `locations` undefined.
const LOCATION_DEFAULT_TOOLS: [&str; 5] = ["Read", "Write", "Edit", "NotebookEdit", "Glob"];

/// Materialise defaults on a serialized typed `SessionUpdate::ToolCall` (Decision 4 / D2).
///
/// The pinned `agent-client-protocol` crate omits `status:"pending"`, empty
/// `content:[]`, `kind:"other"` and empty `locations:[]` because of
/// `skip_serializing_if = is_default` / `Vec::is_empty`. Node's `renderer.toolCall()`
/// always explicitly emits:
/// - `status: "pending"`
/// - `content: []`
/// - `kind: "other"` (when not already set)
/// - `locations: []` if missing and the tool is one of the 5 reporters above.
pub fn materialize_tool_call_defaults(update: &mut Value) {
    let Some(map) = update.as_object_mut() else {
        return;
    };
    if let Some(su) = map.get("sessionUpdate").and_then(Value::as_str) {
        if su != "tool_call" {
            return;
        }
    } else if !map.contains_key("toolCallId") {
        return;
    }
    map.entry("status")
        .or_insert_with(|| Value::String("pending".to_string()));
    map.entry("content")
        .or_insert_with(|| Value::Array(Vec::new()));
    map.entry("kind")
        .or_insert_with(|| Value::String("other".to_string()));

    if !map.contains_key("locations") {
        let tool_name = map
            .get("_meta")
            .and_then(|m| m.get("claudeCode"))
            .and_then(|c| c.get("toolName"))
            .and_then(Value::as_str)
            .or_else(|| map.get("name").and_then(Value::as_str));
        if let Some(name) = tool_name {
            if LOCATION_DEFAULT_TOOLS.contains(&name) {
                map.insert("locations".to_string(), Value::Array(Vec::new()));
            }
        }
    }
}

/// Materialise `oldText: null` for `{type: "diff"}` content blocks where
/// `oldText` is omitted (Decision 4 / D8).
///
/// Rust's `Diff` struct has `old_text: Option<String>`, so serde omits the field
/// when it is `None` (such as creating a new file or empty `old_string`). Node's
/// plain object diffs explicitly include `oldText: null`.
///
/// Only the `content[*]` entries of a `tool_call` / `tool_call_update` (or the
/// permission `toolCall`, recognised by its `toolCallId`) are touched. The
/// `rawInput`, `rawOutput` and `_meta.claudeCode.toolResponse` fields carry the
/// tool's own data verbatim — a `{"type":"diff"}` object inside them (an MCP
/// tool input or output, a hook's tool response) must stay byte-identical to
/// Node's.
pub fn materialize_diff_old_text(tool_call: &mut Value) {
    let Some(map) = tool_call.as_object_mut() else {
        return;
    };
    let is_tool_call = match map.get("sessionUpdate").and_then(Value::as_str) {
        Some("tool_call" | "tool_call_update") => true,
        Some(_) => false,
        // The permission request's `toolCall` has no `sessionUpdate`.
        None => map.contains_key("toolCallId"),
    };
    if !is_tool_call {
        return;
    }
    let Some(content) = map.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in content {
        let Value::Object(entry) = entry else {
            continue;
        };
        // A v2 diff (`changes`/`patch`) has no `oldText`: v2 forbids the v1
        // diff keys, so it is left as the agent built it.
        if entry.get("type").and_then(Value::as_str) == Some("diff")
            && !entry.contains_key("changes")
            && !entry.contains_key("oldText")
        {
            entry.insert("oldText".to_string(), Value::Null);
        }
    }
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
            apply_stream_message_id(
                stamp_chunk_parent(
                    drop_air_nested_text(state, mapped, is_strict_top_level(message)),
                    ctx.parent.as_deref(),
                ),
                &state.current_stream_message_id,
            )
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
                let mapped = map_consolidated_ctx(
                    &Value::Array(vec![delta.clone()]),
                    MsgRole::Assistant,
                    state,
                    &ctx,
                );
                apply_stream_message_id(
                    stamp_chunk_parent(
                        drop_air_nested_text(state, mapped, is_strict_top_level(message)),
                        ctx.parent.as_deref(),
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
        Some("message_stop") => {
            // A message boundary ends the input stream on this lane; drop any
            // leftover partial input so the next message starts clean.
            state
                .streamed_inputs
                .retain(|(key, _), _| *key != stream_key);
            Vec::new()
        }
        Some("content_block_stop") => {
            // content_block_stop drops only that block's streamed input (N-48,
            // acp-agent.ts:13511-13515).
            if let Some(idx) = event.get("index").and_then(Value::as_u64) {
                state.streamed_inputs.remove(&(stream_key, idx));
            }
            Vec::new()
        }
        // `ping` and `message_delta` are keep-alive/boundary no-ops.
        _ => Vec::new(),
    }
}

/// Stamp `_meta.claudeCode.parentToolUseId` onto the message/thought chunks a
/// nested frame produced (`stampParentToolUseId` runs on every update of a
/// frame with a string `parent_tool_use_id`, `acp-agent.ts:13476-13480`).
/// A top-level frame (`None`) is left untouched.
/// AIR gets no STREAMED subagent text without the transcript capability (or
/// native subagent sessions): the nested text/thinking stays internal to the
/// Agent tool call (`acp-agent.ts:7680-7692`). Every other client gets it, like
/// upstream, and a client that forwards the transcript gets it too.
/// Node's gate is `message.parent_tool_use_id !== null`, so a frame with the
/// field MISSING counts as nested (dropped), not top level; only a present
/// `null` is the main conversation.
fn drop_air_nested_text(
    state: &MapState,
    updates: Vec<SessionUpdate>,
    top_level: bool,
) -> Vec<SessionUpdate> {
    if !state.air || state.forward_subagent_text || top_level {
        return updates;
    }
    updates
        .into_iter()
        .filter(|update| {
            !matches!(
                update,
                SessionUpdate::AgentMessageChunk(_) | SessionUpdate::AgentThoughtChunk(_)
            )
        })
        .collect()
}

fn stamp_chunk_parent(updates: Vec<SessionUpdate>, parent: Option<&str>) -> Vec<SessionUpdate> {
    let Some(parent) = parent else {
        return updates;
    };
    updates
        .into_iter()
        .map(|update| {
            let (mut chunk, tag) = match update {
                SessionUpdate::AgentMessageChunk(chunk) => (chunk, 0u8),
                SessionUpdate::AgentThoughtChunk(chunk) => (chunk, 1),
                SessionUpdate::UserMessageChunk(chunk) => (chunk, 2),
                other => return other,
            };
            let mut meta = chunk.meta.take().unwrap_or_default();
            crate::tool_meta::stamp_parent(&mut meta, Some(parent));
            chunk.meta = Some(meta);
            match tag {
                0 => SessionUpdate::AgentMessageChunk(chunk),
                1 => SessionUpdate::AgentThoughtChunk(chunk),
                _ => SessionUpdate::UserMessageChunk(chunk),
            }
        })
        .collect()
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
    // TodoWrite and the Task* tools never surfaced a tool_call to refine, so
    // their streamed input stays out of the tool feed (`acp-agent.ts:13468-13470`,
    // T-14/N-47).
    if !tools::should_emit_tool_call(&name) {
        return Vec::new();
    }
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
    // T-15: the partial refinement is a `tool_call_update`, so a subagent call
    // records its Agent/Task parent here too (`recordToolCallParent`).
    if let Some(parent) = ctx.parent.as_deref() {
        state.record_tool_call_parent(id, parent);
    }
    let shape = state.tool_shape(name, &input);
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
    let fields = state.track_tool_call_update_with_meta(id, &fields, Some(&meta))?;
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
            // A `tool_result` only maps when its `tool_use` is cached and was
            // surfaced (T-20); seed that state for the variant case.
            if content.as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b["type"].as_str() == Some("tool_result"))
            }) {
                state.tool_use_cache.insert(
                    "t1".to_string(),
                    json!({"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}),
                );
                state.emitted.insert("t1".to_string());
            }
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
        let tool_use = state.tool_use_cache.remove("toolu_HK1").unwrap();
        state.emitted.remove("toolu_HK1");
        let _ = tool_update_from_result(&result_chunk, &tool_use, &mut state, &FrameCtx::default());

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

    /// T8A — `Agent` and the legacy `Task` are subagent launches for an AIR
    /// client (`toolUseMeta` adds `jetbrains.air.subagent`), and a nested launch
    /// carries its parent tool call.
    #[test]
    fn t8a_agent_and_legacy_task_are_subagent_launches() {
        for name in ["Agent", "Task"] {
            let mut state = MapState::default();
            state.configure_air(std::env::temp_dir());
            let frame = json!({
                "type": "assistant",
                "parent_tool_use_id": null,
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": format!("toolu_{name}"),
                    "name": name,
                    "input": {"description": "Explore", "prompt": "Inspect the project"},
                }]},
            });
            let updates = map_consolidated_assistant(&frame, &mut state);
            assert_eq!(updates.len(), 1, "{name}: {updates:?}");
            let value = serde_json::to_value(&updates[0]).unwrap();
            assert_eq!(value["sessionUpdate"], "tool_call");
            assert_eq!(value["_meta"]["claudeCode"]["toolName"], name);
            assert_eq!(value["_meta"]["jetbrains"]["air"]["subagent"], true);
        }

        let mut state = MapState::default();
        state.configure_air(std::env::temp_dir());
        let frame = json!({
            "type": "assistant",
            "parent_tool_use_id": "outer-agent",
            "message": {"role": "assistant", "content": [{
                "type": "tool_use",
                "id": "nested-agent",
                "name": "Agent",
                "input": {"description": "Review tests", "prompt": "Inspect tests"},
            }]},
        });
        let updates = map_consolidated_assistant(&frame, &mut state);
        let value = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "tool_call");
        assert_eq!(value["_meta"]["claudeCode"]["toolName"], "Agent");
        assert_eq!(
            value["_meta"]["claudeCode"]["parentToolUseId"],
            "outer-agent"
        );
        assert_eq!(value["_meta"]["jetbrains"]["air"]["subagent"], true);
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

    /// F6b / T-09 + T-19: Node caches EVERY tool_use by id, TodoWrite included
    /// (`acp-agent.ts:13083`), so its tool_result is resolved silently instead
    /// of hitting the untracked path. The plan shows `activeForm` for an
    /// in_progress entry, but an EMPTY activeForm falls back to the content
    /// (U-27, `tools.ts:70`).
    #[test]
    fn f6b_todo_write_is_cached_and_its_result_is_silent() {
        let mut state = MapState::default();
        let use_block = json!([{
            "type": "tool_use", "id": "toolu_TODO", "name": "TodoWrite",
            "input": {"todos": [
                {"content": "Write tests", "status": "in_progress", "activeForm": "Writing tests"},
                {"content": "Empty form", "status": "in_progress", "activeForm": ""},
                {"content": "Ship", "status": "pending", "activeForm": "Shipping"}
            ]}
        }]);
        let updates = map_consolidated(&use_block, MsgRole::Assistant, &mut state);
        assert_eq!(updates.len(), 1);
        let plan = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(plan["sessionUpdate"], "plan");
        assert_eq!(plan["entries"][0]["content"], "Writing tests");
        assert_eq!(plan["entries"][1]["content"], "Empty form");
        assert_eq!(plan["entries"][2]["content"], "Ship");
        assert!(
            state.tool_use_cache.contains_key("toolu_TODO"),
            "TodoWrite must be cached like every other tool_use (T-19)"
        );
        assert!(
            !state.has_emitted("toolu_TODO"),
            "TodoWrite is never a tool_call"
        );

        let result = json!([{
            "type": "tool_result", "tool_use_id": "toolu_TODO", "content": "Todos updated"
        }]);
        let updates = map_consolidated(&result, MsgRole::User, &mut state);
        assert!(
            updates.is_empty(),
            "a TodoWrite result must stay silent: {updates:?}"
        );
        assert!(state.tool_use_cache.is_empty());
        assert!(!state.has_emitted("toolu_TODO"));
    }

    /// G10c review (3) / T-76: a nested (subagent) TodoWrite's plan carries
    /// `_meta.claudeCode.parentToolUseId` like every other update of its frame
    /// (`stampParentToolUseId`, `acp-agent.ts:13356`); a top-level plan carries
    /// no `_meta` at all.
    #[test]
    fn nested_todo_write_plan_carries_parent_tool_use_id() {
        let todos = json!([{"content": "x", "status": "pending", "activeForm": "x"}]);
        let mut state = MapState::default();
        let frame = json!({
            "type": "assistant", "parent_tool_use_id": "toolu_TASK",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_TODO", "name": "TodoWrite",
                 "input": {"todos": todos.clone()}}
            ]}
        });
        let updates = map_consolidated_assistant(&frame, &mut state);
        let SessionUpdate::Plan(plan) = &updates[0] else {
            panic!("expected a plan, got {:?}", updates[0]);
        };
        let meta = serde_json::to_value(plan.meta.clone().unwrap_or_default()).unwrap();
        assert_eq!(meta["claudeCode"]["parentToolUseId"], "toolu_TASK");

        let mut state = MapState::default();
        let top = json!({
            "type": "assistant", "parent_tool_use_id": null,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_TODO2", "name": "TodoWrite",
                 "input": {"todos": todos}}
            ]}
        });
        let updates = map_consolidated_assistant(&top, &mut state);
        let SessionUpdate::Plan(plan) = &updates[0] else {
            panic!("expected a plan");
        };
        assert!(plan.meta.is_none(), "a top-level plan carries no _meta");
    }

    /// G10c review (7): Node's AIR gate is `message.parent_tool_use_id !== null`,
    /// so a stream frame with the field MISSING is nested and its text is
    /// dropped; only a present `null` is the main conversation.
    #[test]
    fn air_drops_stream_text_when_the_parent_field_is_missing() {
        let delta = json!({
            "type": "stream_event",
            "event": {"type": "content_block_delta", "index": 0,
                      "delta": {"type": "text_delta", "text": "nested"}}
        });
        let mut state = MapState::default();
        state.configure_air(PathBuf::new());
        let updates = map_stream_event(&delta, &mut state);
        assert!(
            updates.is_empty(),
            "a missing parent field is nested for AIR: {updates:?}"
        );

        let mut top = delta.clone();
        top["parent_tool_use_id"] = Value::Null;
        let mut state = MapState::default();
        state.configure_air(PathBuf::new());
        let updates = map_stream_event(&top, &mut state);
        assert_eq!(
            updates.len(),
            1,
            "a present null parent is top level: {updates:?}"
        );
    }

    /// F6b / T-14 + N-47 + T-07: TodoWrite and Task* never surface a tool_call,
    /// so their streamed input must never refine one (`shouldEmitToolCall`,
    /// `acp-agent.ts:12820-12824,13468-13470`).
    #[test]
    fn f6b_streamed_todo_and_task_input_never_refine() {
        let cases = [
            ("TodoWrite", r#"{"todos":[{"content":"x"}],"other":"#),
            ("TaskCreate", r#"{"subject":"s","description":"#),
            ("TaskUpdate", r#"{"taskId":"1","status":"#),
            ("TaskList", r#"{"a":1,"b":"#),
            ("TaskGet", r#"{"taskId":"1","x":"#),
        ];
        for (name, partial) in cases {
            let mut state = MapState::default();
            let start = json!({
                "type": "stream_event", "parent_tool_use_id": null,
                "event": {"type": "content_block_start", "index": 0,
                          "content_block": {"type": "tool_use", "id": "toolu_P",
                                            "name": name, "input": {}}}
            });
            let started = map_stream_event(&start, &mut state);
            assert!(
                started.is_empty(),
                "{name} start must stay silent: {started:?}"
            );
            let delta = json!({
                "type": "stream_event", "parent_tool_use_id": null,
                "event": {"type": "content_block_delta", "index": 0,
                          "delta": {"type": "input_json_delta", "partial_json": partial}}
            });
            let refined = map_stream_event(&delta, &mut state);
            assert!(
                refined.is_empty(),
                "{name} partial input must not refine a tool_call: {refined:?}"
            );
        }
    }

    /// F6b / T-20: an untracked tool_result resolves only a call the permission
    /// flow surfaced (`wasEmitted`), with `{status, rawOutput}` and no
    /// `claudeCode` meta; otherwise it is dropped (`acp-agent.ts:13193-13215`).
    #[test]
    fn f6b_untracked_result_resolves_only_a_surfaced_call() {
        let result = json!([{
            "type": "tool_result", "tool_use_id": "toolu_GHOST", "content": "late output"
        }]);
        let mut state = MapState::default();
        let updates = map_consolidated(&result, MsgRole::User, &mut state);
        assert!(
            updates.is_empty(),
            "an uncached result that never surfaced must be dropped: {updates:?}"
        );

        let mut state = MapState::default();
        state.emitted.insert("toolu_GHOST".to_string());
        let updates = map_consolidated(&result, MsgRole::User, &mut state);
        assert_eq!(updates.len(), 1, "{updates:?}");
        let value = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "tool_call_update");
        assert_eq!(value["toolCallId"], "toolu_GHOST");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["rawOutput"], "late output");
        assert!(
            value.get("_meta").is_none(),
            "the tool name is unknown, so no claudeCode meta: {value}"
        );
        assert!(!state.has_emitted("toolu_GHOST"));
    }

    /// F6b / U-23: a call a permission request surfaced with its complete input
    /// must not be refined by the empty input at the stream start
    /// (`alreadyEmitted && inputComplete === false`, `acp-agent.ts:13162`).
    #[test]
    fn f6b_surfaced_call_ignores_the_empty_stream_start_input() {
        let mut state = MapState::default();
        state.mark_emitted(
            "toolu_W".to_string(),
            json!({"type": "tool_use", "id": "toolu_W", "name": "Write",
                   "input": {"file_path": "/w/a", "content": "hi"}}),
        );
        let start = json!({
            "type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "content_block_start", "index": 0,
                      "content_block": {"type": "tool_use", "id": "toolu_W",
                                        "name": "Write", "input": {}}}
        });
        let updates = map_stream_event(&start, &mut state);
        assert!(
            updates.is_empty(),
            "the empty stream-start input adds nothing: {updates:?}"
        );

        // The consolidated message replays the block with its complete input
        // and refines the surfaced call.
        let frame = json!({
            "type": "assistant", "parent_tool_use_id": null,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_W", "name": "Write",
                 "input": {"file_path": "/w/a", "content": "hi"}}
            ]}
        });
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(updates.len(), 1, "{updates:?}");
        assert!(matches!(updates[0], SessionUpdate::ToolCallUpdate(_)));
    }

    /// T8A — the streamed refinement of a call the eager permission path
    /// surfaced restores its parent attribution (`parentToolUseId`) when the
    /// eager emission raced `task_started`.
    #[test]
    fn t8a_refinement_restores_the_raced_parent_attribution() {
        let mut state = MapState::default();
        state.mark_emitted(
            "toolu_sub".to_string(),
            json!({"type": "tool_use", "id": "toolu_sub", "name": "Bash",
                   "input": {"command": "ls"}}),
        );
        let frame = json!({
            "type": "assistant", "parent_tool_use_id": "toolu_parent",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_sub", "name": "Bash",
                 "input": {"command": "ls"}}
            ]}
        });
        let updates = map_consolidated_assistant(&frame, &mut state);
        assert_eq!(updates.len(), 1, "{updates:?}");
        let value = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "tool_call_update");
        assert_eq!(value["toolCallId"], "toolu_sub");
        assert_eq!(
            value["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_parent"
        );
    }

    /// F6b / T-15: a nested subagent tool call remembers its Agent/Task parent
    /// in an insertion-ordered memory capped at 1000 (`recordToolCallParent`,
    /// `acp-agent.ts:12828-12839`), and a final result forgets it.
    #[test]
    fn f6b_tool_call_parent_memory_records_and_forgets() {
        let mut state = MapState::default();
        let child = json!({
            "type": "assistant", "parent_tool_use_id": "toolu_TASK",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_CHILD", "name": "Read",
                 "input": {"file_path": "/w/a"}}
            ]}
        });
        let _ = map_consolidated_assistant(&child, &mut state);
        assert_eq!(state.tool_call_parent("toolu_CHILD"), Some("toolu_TASK"));

        let result = json!({
            "type": "user", "parent_tool_use_id": "toolu_TASK",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_CHILD", "content": "file"}
            ]}
        });
        let _ = map_user_frame(&result, &mut state);
        assert_eq!(
            state.tool_call_parent("toolu_CHILD"),
            None,
            "a final status forgets the parent"
        );

        // The cap drops the oldest entry.
        for index in 0..=MAX_TOOL_MEMORY {
            let frame = json!({
                "type": "assistant", "parent_tool_use_id": "toolu_TASK",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": format!("toolu_{index}"), "name": "Read",
                     "input": {"file_path": "/w/a"}}
                ]}
            });
            let _ = map_consolidated_assistant(&frame, &mut state);
        }
        assert_eq!(state.tool_call_parent("toolu_0"), None, "oldest dropped");
        assert_eq!(
            state.tool_call_parent(&format!("toolu_{MAX_TOOL_MEMORY}")),
            Some("toolu_TASK")
        );
    }

    /// F6b / T-16: the tool name of a tool result is remembered before the
    /// tool-use cache drops it (`rememberResolvedToolNames`,
    /// `acp-agent.ts:12927-12944`), in an insertion-ordered memory capped at
    /// 1000.
    #[test]
    fn f6b_resolved_tool_name_memory_survives_the_cache_prune() {
        let mut state = MapState::default();
        let use_block = json!([{
            "type": "tool_use", "id": "toolu_R", "name": "Read",
            "input": {"file_path": "/w/a"}
        }]);
        let _ = map_consolidated(&use_block, MsgRole::Assistant, &mut state);
        let result = json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_R", "content": "file"}
            ]}
        });
        let _ = map_user_frame(&result, &mut state);
        assert!(
            !state.tool_use_cache.contains_key("toolu_R"),
            "cache pruned"
        );
        assert_eq!(state.resolved_tool_name("toolu_R"), Some("Read"));

        // The cap drops the oldest name.
        for index in 0..=MAX_TOOL_MEMORY {
            let id = format!("toolu_N{index}");
            let _ = map_consolidated(
                &json!([{"type": "tool_use", "id": id, "name": "Bash",
                         "input": {"command": "ls"}}]),
                MsgRole::Assistant,
                &mut state,
            );
            let _ = map_user_frame(
                &json!({
                    "type": "user",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": id, "content": "ok"}
                    ]}
                }),
                &mut state,
            );
        }
        assert_eq!(state.resolved_tool_name("toolu_R"), None, "oldest dropped");
        assert_eq!(
            state.resolved_tool_name(&format!("toolu_N{MAX_TOOL_MEMORY}")),
            Some("Bash")
        );
    }

    /// F6b / C-42: a client that advertised `_meta["subagent-transcript"]`
    /// gets the nested assistant text and thinking, stamped with the parent
    /// (`forwardsSubagentText`, `acp-agent.ts:7994,13476-13480`). Without it,
    /// the nested text stays internal.
    #[test]
    fn f6b_subagent_text_forwarded_when_the_transcript_capability_is_set() {
        let frame = json!({
            "type": "assistant", "parent_tool_use_id": "toolu_TASK",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Look at the layout."},
                {"type": "text", "text": "The repo has two packages."}
            ]}
        });

        let mut plain = MapState::default();
        let updates = map_consolidated_assistant(&frame, &mut plain);
        assert!(
            updates.is_empty(),
            "nested text stays internal: {updates:?}"
        );

        let mut transcript = MapState::default();
        transcript.set_forward_subagent_text(true);
        let updates = map_consolidated_assistant(&frame, &mut transcript);
        assert_eq!(updates.len(), 2, "{updates:?}");
        let thought = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(thought["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(thought["content"]["text"], "Look at the layout.");
        assert_eq!(
            thought["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_TASK"
        );
        let text = serde_json::to_value(&updates[1]).unwrap();
        assert_eq!(text["sessionUpdate"], "agent_message_chunk");
        assert_eq!(text["content"]["text"], "The repo has two packages.");
        assert_eq!(text["_meta"]["claudeCode"]["parentToolUseId"], "toolu_TASK");
    }

    /// F6b / C-42: AIR keeps nested streamed text internal unless it also gets
    /// the transcript (native subagent sessions or the capability), while a
    /// plain client still receives it (`acp-agent.ts:7680-7692`).
    #[test]
    fn f6b_air_drops_nested_stream_text_without_the_transcript_capability() {
        let delta = json!({
            "type": "stream_event", "parent_tool_use_id": "toolu_TASK",
            "event": {"type": "content_block_delta", "index": 0,
                      "delta": {"type": "text_delta", "text": "internal"}}
        });

        let mut air = MapState::default();
        air.configure_air(PathBuf::from("/w"));
        assert!(
            map_stream_event(&delta, &mut air).is_empty(),
            "AIR keeps it internal"
        );

        let mut air_forwarding = MapState::default();
        air_forwarding.configure_air(PathBuf::from("/w"));
        air_forwarding.set_forward_subagent_text(true);
        let updates = map_stream_event(&delta, &mut air_forwarding);
        assert_eq!(updates.len(), 1, "{updates:?}");

        let mut plain = MapState::default();
        let updates = map_stream_event(&delta, &mut plain);
        assert_eq!(updates.len(), 1, "a plain client gets the streamed text");
    }

    /// The single text of a completing update's content.
    fn update_text(update: &ToolCallUpdate) -> &str {
        let content = update
            .fields
            .content
            .as_ref()
            .and_then(|items| items.first())
            .expect("a content item");
        let ToolCallContent::Content(content) = content else {
            panic!("expected content, got {content:?}");
        };
        let ContentBlock::Text(text) = &content.content else {
            panic!("expected text, got {:?}", content.content);
        };
        text.text.as_str()
    }

    /// F6a / T-67 — AIR's agent-control error result is plain text; every other
    /// client keeps the fenced error text
    /// (`AgentControlReporter.errorResult`, `reporters/interaction.ts:209-219`).
    #[test]
    fn f6a_air_agent_control_error_is_unfenced() {
        let use_frame = json!([{
            "type": "tool_use", "id": "toolu_MSG", "name": "SendMessage",
            "input": {"to": "a", "message": "hi"}
        }]);
        let result = json!([{
            "type": "tool_result", "tool_use_id": "toolu_MSG",
            "content": "denied", "is_error": true
        }]);

        let mut air = MapState::default();
        air.configure_air(PathBuf::from("/w"));
        map_consolidated(&use_frame, MsgRole::Assistant, &mut air);
        let updates = map_consolidated(&result, MsgRole::User, &mut air);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected a completing tool_call_update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
        assert_eq!(update_text(update), "denied", "AIR gets no fence");

        let mut plain = MapState::default();
        map_consolidated(&use_frame, MsgRole::Assistant, &mut plain);
        let updates = map_consolidated(&result, MsgRole::User, &mut plain);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected a completing tool_call_update");
        };
        assert_eq!(update_text(update), "```\ndenied\n```");
    }

    /// F6a / T-61 — an AIR `AskUserQuestion` tool call gets the fixed title; a
    /// non-AIR client gets the single question
    /// (`reporters/interaction.ts:94-116`).
    #[test]
    fn f6a_air_ask_user_question_title() {
        let frame = json!([{
            "type": "tool_use", "id": "toolu_ASK", "name": "AskUserQuestion",
            "input": {"questions": [{"question": "ok?"}]}
        }]);

        let mut air = MapState::default();
        air.configure_air(PathBuf::from("/w"));
        let updates = map_consolidated(&frame, MsgRole::Assistant, &mut air);
        let SessionUpdate::ToolCall(call) = &updates[0] else {
            panic!("expected a tool_call");
        };
        assert_eq!(call.title, "Asking for your input");

        let mut plain = MapState::default();
        let updates = map_consolidated(&frame, MsgRole::Assistant, &mut plain);
        let SessionUpdate::ToolCall(call) = &updates[0] else {
            panic!("expected a tool_call");
        };
        assert_eq!(call.title, "ok?");
    }

    /// F6a / T-29 / T-30 — the streamed Bash/PowerShell title and display copy
    /// go through the shared shell reporter.
    #[test]
    fn f6a_powershell_tool_call_shape() {
        let frame = json!([{
            "type": "tool_use", "id": "toolu_PS", "name": "PowerShell",
            "input": {"command": "Get-ChildItem", "command_description": "List files"}
        }]);
        let mut state = MapState::default();
        let updates = map_consolidated(&frame, MsgRole::Assistant, &mut state);
        let SessionUpdate::ToolCall(call) = &updates[0] else {
            panic!("expected a tool_call");
        };
        assert_eq!(call.kind, ToolKind::Execute);
        assert_eq!(call.title, "Get-ChildItem");
        assert_eq!(call.content.len(), 1);
    }

    /// FX-3 / N-48 — content_block_stop drops only its own block's streamed input.
    #[test]
    fn content_block_stop_drops_only_its_block() {
        let mut state = MapState::default();
        let start0 = json!({
            "type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "content_block_start", "index": 0,
                      "content_block": {"type": "tool_use", "id": "toolu_0",
                                        "name": "Bash", "input": {}}}
        });
        let start1 = json!({
            "type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "content_block_start", "index": 1,
                      "content_block": {"type": "tool_use", "id": "toolu_1",
                                        "name": "Bash", "input": {}}}
        });
        let _ = map_stream_event(&start0, &mut state);
        let _ = map_stream_event(&start1, &mut state);
        assert_eq!(state.streamed_inputs.len(), 2);

        // content_block_stop for index 0 drops only index 0
        let stop0 = json!({
            "type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "content_block_stop", "index": 0}
        });
        let _ = map_stream_event(&stop0, &mut state);
        assert_eq!(state.streamed_inputs.len(), 1);
        assert!(state.streamed_inputs.contains_key(&("".to_string(), 1)));
        assert!(!state.streamed_inputs.contains_key(&("".to_string(), 0)));

        // message_stop clears remaining inputs on the lane
        let stop_msg = json!({
            "type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "message_stop"}
        });
        let _ = map_stream_event(&stop_msg, &mut state);
        assert!(state.streamed_inputs.is_empty());
    }

    /// FX-3 / M-51 — user chunks get stamped with the frame uuid.
    #[test]
    fn m51_user_chunk_message_id_grouping() {
        let mut state = MapState::default();
        let frame = json!({
            "type": "user",
            "uuid": "u-user-chunk-42",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello 1"},
                    {"type": "text", "text": "hello 2"}
                ]
            }
        });
        let updates = map_user_frame(&frame, &mut state);
        assert_eq!(updates.len(), 2);
        for update in &updates {
            let SessionUpdate::UserMessageChunk(chunk) = update else {
                panic!("expected UserMessageChunk");
            };
            assert_eq!(
                chunk.message_id.as_ref().map(|m| m.0.as_ref()),
                Some("u-user-chunk-42")
            );
        }

        let block_updates = map_user_frame_block(
            &frame,
            &json!({"type": "text", "text": "single"}),
            &mut state,
        );
        assert_eq!(block_updates.len(), 1);
        let SessionUpdate::UserMessageChunk(chunk) = &block_updates[0] else {
            panic!("expected UserMessageChunk");
        };
        assert_eq!(
            chunk.message_id.as_ref().map(|m| m.0.as_ref()),
            Some("u-user-chunk-42")
        );
    }

    #[test]
    fn inv_79_defaults_only_on_tool_call() {
        let mut tool_call = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_123",
            "title": "echo hi"
        });
        materialize_tool_call_defaults(&mut tool_call);
        assert_eq!(tool_call["status"], "pending");
        assert_eq!(tool_call["content"], json!([]));
        assert_eq!(tool_call["kind"], "other");

        let mut tool_update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_123",
            "title": "echo hi"
        });
        materialize_tool_call_defaults(&mut tool_update);
        assert!(tool_update.get("status").is_none());
        assert!(tool_update.get("content").is_none());
        assert!(tool_update.get("kind").is_none());
    }

    #[test]
    fn inv_79_locations_only_for_listed_tools() {
        for tool in ["Read", "Write", "Edit", "NotebookEdit", "Glob"] {
            let mut tc = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "toolu_1",
                "name": tool,
                "title": tool
            });
            materialize_tool_call_defaults(&mut tc);
            assert_eq!(
                tc["locations"],
                json!([]),
                "tool {tool} must have locations default"
            );
        }

        for tool in ["Bash", "Grep", "WebSearch", "mcp__server__tool"] {
            let mut tc = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "toolu_2",
                "name": tool,
                "title": tool
            });
            materialize_tool_call_defaults(&mut tc);
            assert!(
                tc.get("locations").is_none(),
                "tool {tool} must not have locations default"
            );
        }
    }

    #[test]
    fn inv_79_existing_values_untouched() {
        let mut tc = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_1",
            "name": "Read",
            "title": "Read a.txt",
            "status": "completed",
            "kind": "read",
            "content": [{"type": "text", "text": "foo"}],
            "locations": [{"path": "a.txt", "line": 5}]
        });
        materialize_tool_call_defaults(&mut tc);
        assert_eq!(tc["status"], "completed");
        assert_eq!(tc["kind"], "read");
        assert_eq!(tc["content"], json!([{"type": "text", "text": "foo"}]));
        assert_eq!(tc["locations"], json!([{"path": "a.txt", "line": 5}]));
    }

    #[test]
    fn inv_79_old_text_null_added_not_overwritten() {
        let mut diff_missing = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_write",
            "content": [{
                "type": "diff",
                "path": "new.txt",
                "newText": "hello"
            }]
        });
        materialize_diff_old_text(&mut diff_missing);
        assert_eq!(diff_missing["content"][0]["oldText"], Value::Null);

        let mut diff_existing = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_edit",
            "content": [{
                "type": "diff",
                "path": "existing.txt",
                "oldText": "old",
                "newText": "hello"
            }]
        });
        materialize_diff_old_text(&mut diff_existing);
        assert_eq!(diff_existing["content"][0]["oldText"], "old");

        let mut nested = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_1",
            "content": [
                {
                    "type": "diff",
                    "path": "file.rs",
                    "newText": "fn main() {}"
                }
            ]
        });
        materialize_diff_old_text(&mut nested);
        assert_eq!(nested["content"][0]["oldText"], Value::Null);
    }

    /// Review G14 finding 1: a `{"type":"diff"}` object inside the tool's own
    /// data (`rawInput`, `rawOutput`, `_meta.claudeCode.toolResponse`) is Node's
    /// to send verbatim; only the `content[*]` diff blocks gain `oldText`.
    #[test]
    fn inv_79_diff_only_in_content() {
        let mut update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "mcp__srv__diff_tool",
            "rawInput": {
                "patch": {"type": "diff", "path": "a.txt", "newText": "x"}
            },
            "rawOutput": {
                "result": {"type": "diff", "path": "b.txt", "newText": "y"}
            },
            "_meta": {
                "claudeCode": {
                    "toolResponse": {"type": "diff", "path": "c.txt", "newText": "z"}
                }
            },
            "content": [
                {"type": "diff", "path": "d.txt", "newText": "w"}
            ]
        });
        materialize_diff_old_text(&mut update);
        assert_eq!(update["content"][0]["oldText"], Value::Null);
        assert!(update["rawInput"]["patch"].get("oldText").is_none());
        assert!(update["rawOutput"]["result"].get("oldText").is_none());
        assert!(update["_meta"]["claudeCode"]["toolResponse"]
            .get("oldText")
            .is_none());

        // A permission `toolCall` (no `sessionUpdate`) has the same rule.
        let mut permission_tool_call = json!({
            "toolCallId": "toolu_perm",
            "title": "Write a.txt",
            "rawInput": {"content": {"type": "diff", "newText": "x"}},
            "content": [{"type": "diff", "path": "a.txt", "newText": "x"}]
        });
        materialize_diff_old_text(&mut permission_tool_call);
        assert_eq!(permission_tool_call["content"][0]["oldText"], Value::Null);
        assert!(permission_tool_call["rawInput"]["content"]
            .get("oldText")
            .is_none());

        // A non-tool update is left alone entirely.
        let mut other = json!({
            "sessionUpdate": "plan",
            "entries": [{"type": "diff", "newText": "x"}]
        });
        materialize_diff_old_text(&mut other);
        assert!(other["entries"][0].get("oldText").is_none());
    }

    #[test]
    fn inv_79_raw_tool_call_not_touched() {
        let mut plan = json!({
            "sessionUpdate": "plan",
            "entries": []
        });
        materialize_tool_call_defaults(&mut plan);
        assert!(plan.get("status").is_none());
        assert!(plan.get("content").is_none());
        assert!(plan.get("kind").is_none());
    }
}

/// T-40/T-43/T-106 through the real mapping path: the `diffPatch` capability
/// gates the Write input diff, the approval preview and the pinned content.
#[cfg(test)]
mod diff_patch_tests {
    use super::*;
    use serde_json::json;

    fn tool_use_frame(id: &str, name: &str, input: Value) -> Value {
        json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}
            ]}
        })
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-map-diff-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn first_tool_call_content(updates: &[SessionUpdate]) -> Vec<Value> {
        let SessionUpdate::ToolCall(call) = &updates[0] else {
            panic!("expected a tool_call, got {:?}", updates[0]);
        };
        call.content
            .iter()
            .map(|block| serde_json::to_value(block).unwrap())
            .collect()
    }

    #[test]
    fn write_tool_call_input_diff_follows_the_diff_patch_capability() {
        let input = json!({"file_path": "/work/a.ts", "content": "a\n"});

        let mut plain = MapState::default();
        let updates =
            map_consolidated_assistant(&tool_use_frame("t1", "Write", input.clone()), &mut plain);
        let content = first_tool_call_content(&updates);
        assert_eq!(content.len(), 1, "{content:?}");
        assert_eq!(content[0]["type"], "diff");
        assert_eq!(content[0]["newText"], "a\n");

        let mut patch = MapState::default();
        patch.set_diff_patch(true);
        let updates = map_consolidated_assistant(&tool_use_frame("t1", "Write", input), &mut patch);
        assert!(first_tool_call_content(&updates).is_empty());
    }

    #[test]
    fn edit_tool_call_keeps_the_snippet_diff_for_a_diff_patch_client() {
        let mut patch = MapState::default();
        patch.set_diff_patch(true);
        let input = json!({"file_path": "/work/a.ts", "old_string": "a", "new_string": "b"});
        let updates = map_consolidated_assistant(&tool_use_frame("t1", "Edit", input), &mut patch);
        let content = first_tool_call_content(&updates);
        assert_eq!(content[0]["oldText"], "a");
        assert_eq!(content[0]["newText"], "b");
    }

    #[test]
    fn approval_preview_needs_the_capability_and_resolves_against_the_cwd() {
        let dir = temp_dir("preview");
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let input = json!({"file_path": "sub/../a.txt", "old_string": "two", "new_string": "2"});

        let mut state = MapState::default();
        state.set_cwd(dir.clone());
        assert!(state.approval_preview("Edit", &input).is_none());

        state.set_diff_patch(true);
        let preview = state.approval_preview("Edit", &input).expect("preview");
        let text = preview[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"]
            .as_str()
            .unwrap();
        let name = dir
            .join("a.txt")
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string();
        // The patch names the resolved file, not the raw `sub/../a.txt`.
        assert!(
            text.starts_with(&format!("diff --git a/{name} b/{name}\n")),
            "{text}"
        );
        assert!(text.contains("-two\n+2\n"), "{text}");
        assert!(state.approval_preview("Read", &input).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_approval_patch_survives_the_streamed_snippet_and_yields_to_the_result() {
        let dir = temp_dir("pin");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let file_str = file.to_string_lossy().to_string();
        let input = json!({"file_path": &file_str, "old_string": "two", "new_string": "2"});

        let mut state = MapState::default();
        state.set_cwd(dir.clone());
        state.set_diff_patch(true);
        // The tool call is surfaced by the stream with the snippet diff.
        let updates =
            map_consolidated_assistant(&tool_use_frame("t1", "Edit", input.clone()), &mut state);
        assert_eq!(first_tool_call_content(&updates)[0]["oldText"], "two");

        // The approval pins the exact patch over it.
        let preview = state.approval_preview("Edit", &input).expect("preview");
        state.pin_approval_preview("t1", None, &preview);

        // The snippet diff of a streamed refinement is dropped...
        let snippet: Vec<ToolCallContent> = serde_json::from_value(json!([
            {"type": "diff", "path": &file_str, "oldText": "two", "newText": "2x"}
        ]))
        .unwrap();
        let refine = ToolCallUpdateFields::new().content(snippet.clone());
        assert!(state.track_tool_call_update("t1", &refine).is_none());

        // ...and a final result replaces it.
        let fin = state
            .track_final_tool_call_update("t1", &refine)
            .expect("the final result replaces the pinned patch");
        assert_eq!(fin.content, Some(snippet));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pin_replaces_the_content_of_a_tool_call_surfaced_by_the_approval() {
        let mut state = MapState::default();
        let preview = vec![json!({
            "type": "diff", "path": "/work/a.ts", "oldText": null, "newText": "",
            "_meta": {"jetbrains": {"air": {"diffPatch": {"version": 1, "format": "git_patch", "text": "p"}}}}
        })];
        let mut surfaced = crate::permission::ensure_tool_call_emitted(
            "Edit",
            &json!({"file_path": "/work/a.ts", "old_string": "a", "new_string": "b"}),
            "t1",
            None,
            &mut state,
            None,
        );
        state.pin_approval_preview("t1", surfaced.as_mut(), &preview);
        let content = first_tool_call_content(std::slice::from_ref(surfaced.as_ref().unwrap()));
        assert_eq!(
            content[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"],
            "p"
        );
    }
}

#[cfg(test)]
mod tests_t8b;
