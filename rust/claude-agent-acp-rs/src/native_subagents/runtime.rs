//! The connection-local native subagent registry (`NativeSubagentRuntime`,
//! `native-subagents.ts:127-1159`).
//!
//! Node's runtime is async: it awaits each `publish`/`deliver`. This port is a
//! synchronous state machine: every method mutates the registry and returns the
//! publications it would make, in order, as [`Out`] values. The owner executes
//! them one after another (so ordering is exactly Node's) and never holds a lock
//! across an await. Node's single-flight promises (`announcePromise`,
//! `terminalPromise`, `taskFinishPromises`) only deduplicate concurrent callers,
//! which cannot overlap here. The history lookups (`lineage`, `launchedAgent`)
//! are synchronous closures for the same reason.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Map, Value};

use super::{
    is_native_subagent_control_update, native_subagent_state, non_blank, prompt_text,
    rfd_end_state, rfd_labels, subagent_description, subagent_display_name, TaskEnd,
};
use crate::acp_subagents::{
    session_message, subagent_announce, subagent_labels_update, subagent_spawned, subagent_state,
    subagent_state_update, SubagentForm, SubagentState, WorkState,
};

const MAX_PENDING_PARENTS: usize = 64;
const MAX_PENDING_UPDATES: usize = 256;
const MAX_PENDING_UPDATES_PER_PARENT: usize = 32;
/// The number of child tool calls whose owning child session the runtime remembers.
const MAX_CHILD_TOOL_CALLS: usize = 2048;

/// One publication the owner must perform, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// `this.publish(notification)`: sent as it is.
    Publish(Value),
    /// `deliver(notification)`: an earlier buffered update, re-routed by the
    /// owner's child-aware `sendUpdate` (the owner routes it, sends the route's
    /// own publications first, then the routed value through the post-routing
    /// tail of `sendUpdate`).
    Deliver(Value),
    /// `deliver(notification)` for an update the runtime already routed, because
    /// routing it later would see the state that the runtime is about to change
    /// (the end of a child). The owner sends it through the post-routing tail of
    /// `sendUpdate`, without routing it again.
    Routed(Value),
}

/// The result of [`NativeSubagentRuntime::route`]: the publications that must
/// precede the routed notification, then the notification itself (`None` is
/// Node's `null`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RouteResult {
    pub outs: Vec<Out>,
    pub routed: Option<Value>,
}

/// An agent of the session's subagent history (`HistoricalSubagent`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoricalSubagent {
    pub agent_id: String,
    pub launch_id: String,
    pub parent_agent_id: Option<String>,
    pub name: Option<String>,
    pub agent_type: Option<String>,
    pub description: Option<String>,
}

/// `SubagentLineage`: the agent and its ancestors, root first; empty when the
/// history cannot place it.
pub type LineageFn = Box<dyn Fn(&str) -> Vec<HistoricalSubagent> + Send + Sync>;
/// `LaunchedAgent`: the agent that the Agent/Task call `launchId` launched.
pub type LaunchedAgentFn = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A child generation (`NativeSubagent`, `native-subagents.ts:10-53`).
#[derive(Debug, Clone, Default)]
pub struct NativeSubagent {
    pub session_id: String,
    pub parent_session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub name: String,
    pub task: String,
    pub prompt: Option<String>,
    pub title: Option<String>,
    pub role: Option<String>,
    pub description: Option<String>,
    pub work_state: Option<&'static str>,
    pub missing_error: bool,
    pub open_requests: u32,
    pub cancels: u32,
    pub prompt_ids: HashSet<String>,
    pub ended_runs: HashSet<String>,
    pub announced: bool,
    pub terminal_state: Option<SubagentState>,
}

/// The `task_started` facts the runtime needs (`TaskStarted`).
#[derive(Debug, Clone, Default)]
pub struct TaskStarted {
    pub task_id: String,
    pub tool_use_id: Option<String>,
    pub subagent_type: Option<String>,
    pub description: Option<String>,
    pub prompt: Option<String>,
}

impl TaskStarted {
    /// Reads a `system/task_started` message (camelCase or snake_case keys).
    #[must_use]
    pub fn from_message(message: &Value) -> Option<Self> {
        let get = |a: &str, b: &str| {
            message
                .get(a)
                .or_else(|| message.get(b))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        };
        Some(Self {
            task_id: get("taskId", "task_id").filter(|s| !s.is_empty())?,
            tool_use_id: get("toolUseId", "tool_use_id"),
            subagent_type: get("subagentType", "subagent_type").filter(|s| !s.is_empty()),
            description: get("description", "description"),
            prompt: get("prompt", "prompt"),
        })
    }
}

/// Opaque state of one open permission/question request (`awaitingUser`).
///
/// The token names the child generation and the runtime that produced it: a
/// runtime replaced or cleared while the request was open (a clear-context
/// restart) makes the token stale, and [`NativeSubagentRuntime::awaiting_user_end`]
/// then does nothing instead of reporting state for another child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitToken {
    epoch: u64,
    child: usize,
    session_id: String,
    cancels_at_open: u32,
}

/// The route of a tool call's later async-task work (`routeOfToolCall`). It
/// carries the runtime epoch and the child session, like [`AwaitToken`]: a route
/// kept across a clear-context restart is stale and applies to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRoute {
    epoch: u64,
    owner: usize,
    session_id: String,
}

/// Hands out runtime epochs: every runtime (and every `clear`) gets a new one,
/// so a token or route from another runtime never resolves.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

fn next_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Default)]
struct Identity {
    name: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    subagent_type: Option<String>,
}

struct GenFields {
    parent_session_id: String,
    parent_tool_use_id: Option<String>,
    name: String,
    task: String,
    prompt: Option<String>,
    title: Option<String>,
    role: Option<String>,
    description: Option<String>,
}

/// Insertion-ordered map that evicts its oldest entry past a cap (the JS `Map`
/// delete+set idiom of `rememberSendMessage` / `rememberToolCallOwner`).
///
/// A `HashMap` answers the lookups in constant time; a `VecDeque` of
/// `(key, sequence)` records the insertion order. Re-setting a key pushes a new
/// sequence number and leaves the old record behind as stale; eviction pops the
/// front and drops the key only when its record is still the current one, so a
/// re-set key keeps its newest position. The deque is compacted when it grows
/// past twice the cap, which bounds it without a scan per update.
#[derive(Debug, Clone)]
struct LruMap<V> {
    entries: HashMap<String, (V, u64)>,
    order: VecDeque<(String, u64)>,
    next_sequence: u64,
}

impl<V: Clone> LruMap<V> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            next_sequence: 0,
        }
    }
    fn get(&self, key: &str) -> Option<&V> {
        self.entries.get(key).map(|(value, _)| value)
    }
    fn has(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
    fn set_recent(&mut self, key: &str, value: V) {
        self.next_sequence += 1;
        let sequence = self.next_sequence;
        self.entries.insert(key.to_string(), (value, sequence));
        self.order.push_back((key.to_string(), sequence));
        while self.entries.len() > MAX_CHILD_TOOL_CALLS {
            let Some((oldest, sequence)) = self.order.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&oldest)
                .is_some_and(|(_, current)| *current == sequence)
            {
                self.entries.remove(&oldest);
            }
        }
        if self.order.len() > MAX_CHILD_TOOL_CALLS * 2 {
            let entries = &self.entries;
            self.order.retain(|(key, sequence)| {
                entries
                    .get(key)
                    .is_some_and(|(_, current)| current == sequence)
            });
        }
    }
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

/// The connection-local registry. See the module docs for the execution model.
pub struct NativeSubagentRuntime {
    form: Option<SubagentForm>,
    root_session_id: String,
    /// Identifies this runtime (and every `clear`) for [`AwaitToken`] and
    /// [`ToolCallRoute`]: an arena index only means something within one epoch.
    epoch: u64,
    lineage: Option<LineageFn>,
    launched_agent: Option<LaunchedAgentFn>,
    arena: Vec<NativeSubagent>,
    /// `nativeSubagentsByTaskId` in insertion order.
    children: Vec<(String, usize)>,
    task_by_tool_use: HashMap<String, String>,
    parent_by_tool_use: HashMap<String, String>,
    identity_by_tool_use: HashMap<String, Identity>,
    control_by_tool_use: HashMap<String, Value>,
    child_by_parent_tool_use: HashMap<String, usize>,
    child_by_tool_call: LruMap<usize>,
    sender_by_tool_use: LruMap<String>,
    generation_by_task_id: HashMap<String, u64>,
    pending: HashMap<String, Vec<Value>>,
    pending_count: usize,
    unresolved_launches: HashSet<String>,
    unrestorable_agents: HashSet<String>,
    last_restored_generation: u64,
    parent_cancels: u32,
    logs: Vec<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn with_session(notification: &Value, session_id: &str) -> Value {
    let mut out = notification.clone();
    if let Some(map) = out.as_object_mut() {
        map.insert("sessionId".into(), json!(session_id));
    }
    out
}

fn update_kind(update: &Value) -> Option<&str> {
    update.get("sessionUpdate").and_then(Value::as_str)
}

fn parent_tool_use_of(update: &Value) -> Option<String> {
    update
        .pointer("/_meta/claudeCode/parentToolUseId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn subagent_identity(input: Option<&Value>) -> Option<Identity> {
    let value = input?.as_object()?;
    let text = |k: &str| value.get(k).and_then(Value::as_str);
    let identity = Identity {
        name: non_blank(text("name")),
        description: non_blank(text("description")),
        prompt: prompt_text(text("prompt")),
        subagent_type: non_blank(text("subagent_type")),
    };
    (identity.name.is_some()
        || identity.description.is_some()
        || identity.prompt.is_some()
        || identity.subagent_type.is_some())
    .then_some(identity)
}

fn merge_identity(previous: Option<&Identity>, next: Identity) -> Identity {
    Identity {
        name: next.name.or_else(|| previous.and_then(|p| p.name.clone())),
        description: next
            .description
            .or_else(|| previous.and_then(|p| p.description.clone())),
        prompt: next
            .prompt
            .or_else(|| previous.and_then(|p| p.prompt.clone())),
        subagent_type: next
            .subagent_type
            .or_else(|| previous.and_then(|p| p.subagent_type.clone())),
    }
}

fn is_failed_tool_call_update(update: &Value) -> bool {
    matches!(update_kind(update), Some("tool_call" | "tool_call_update"))
        && update.get("status").and_then(Value::as_str) == Some("failed")
}

fn is_late_async_task_update(notification: &Value) -> bool {
    matches!(
        notification
            .pointer("/update/sessionUpdate")
            .and_then(Value::as_str),
        Some("async_task_spawned" | "async_task_progress" | "async_task_state_update")
    )
}

/// `Object.assign({}, ...values)` over JSON objects.
fn assign(values: &[Option<&Value>]) -> Map<String, Value> {
    let mut merged = Map::new();
    for value in values.iter().flatten() {
        if let Some(map) = value.as_object() {
            for (k, v) in map {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    merged
}

/// `ordinaryToolMeta` (`native-subagents.ts:1500-1518`).
fn ordinary_tool_meta(values: &[Option<&Value>]) -> Value {
    let mut merged = assign(values);
    let claude_values: Vec<Option<&Value>> = values
        .iter()
        .map(|v| v.and_then(|m| m.get("claudeCode")))
        .collect();
    let claude_code = assign(&claude_values);
    merged.insert("claudeCode".into(), Value::Object(claude_code));
    let air = merged
        .get("jetbrains")
        .and_then(|j| j.get("air"))
        .and_then(Value::as_object)
        .cloned();
    if let Some(mut air) = air {
        if air.contains_key("subagent") {
            air.remove("subagent");
            let mut jetbrains = merged
                .get("jetbrains")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            jetbrains.insert("air".into(), Value::Object(air));
            merged.insert("jetbrains".into(), Value::Object(jetbrains));
        }
    }
    Value::Object(merged)
}

/// `failedControlFallback` (`native-subagents.ts:1446-1498`).
fn failed_control_fallback(initial: Option<&Value>, terminal: &Value, session_id: &str) -> Value {
    let terminal_update = &terminal["update"];
    if !matches!(
        update_kind(terminal_update),
        Some("tool_call" | "tool_call_update")
    ) {
        return with_session(terminal, session_id);
    }
    let tool_name = terminal_update
        .pointer("/_meta/claudeCode/toolName")
        .and_then(Value::as_str);
    let initial_update = initial
        .map(|i| &i["update"])
        .filter(|u| update_kind(u) == Some("tool_call"));
    let Some(initial_update) = initial_update else {
        let mut update = terminal_update.as_object().cloned().unwrap_or_default();
        update.insert("sessionUpdate".into(), json!("tool_call"));
        update.insert("status".into(), json!("failed"));
        if let Some(name) = tool_name {
            update.insert("name".into(), json!(name));
        }
        let title = terminal_update
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map_or_else(
                || {
                    if tool_name == Some("Task") {
                        "Task"
                    } else {
                        "Agent"
                    }
                    .to_string()
                },
                ToString::to_string,
            );
        update.insert("title".into(), json!(title));
        update.insert(
            "_meta".into(),
            ordinary_tool_meta(&[terminal_update.get("_meta")]),
        );
        let mut out = with_session(terminal, session_id);
        out["update"] = Value::Object(update);
        return out;
    };
    let initial = initial.unwrap_or(&Value::Null);
    let mut update = initial_update.as_object().cloned().unwrap_or_default();
    for (k, v) in terminal_update.as_object().cloned().unwrap_or_default() {
        update.insert(k, v);
    }
    update.insert("sessionUpdate".into(), json!("tool_call"));
    update.insert("status".into(), json!("failed"));
    let terminal_title = terminal_update
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty());
    match terminal_title {
        Some(t) => {
            update.insert("title".into(), json!(t));
        }
        None => match initial_update.get("title") {
            Some(t) => {
                update.insert("title".into(), t.clone());
            }
            None => {
                update.remove("title");
            }
        },
    }
    let initial_meta = initial_update.get("_meta");
    let terminal_meta = terminal_update.get("_meta");
    let mut meta = assign(&[initial_meta, terminal_meta]);
    if let Some(ordinary) = ordinary_tool_meta(&[initial_meta, terminal_meta]).as_object() {
        for (k, v) in ordinary {
            meta.insert(k.clone(), v.clone());
        }
    }
    update.insert("_meta".into(), Value::Object(meta));
    let mut out = initial.clone();
    out["sessionId"] = json!(session_id);
    out["update"] = Value::Object(update);
    out
}

impl NativeSubagentRuntime {
    /// `form` is `None` for a client without native subagent sessions.
    #[must_use]
    pub fn new(form: Option<SubagentForm>, root_session_id: &str) -> Self {
        Self {
            form,
            root_session_id: root_session_id.to_string(),
            epoch: next_epoch(),
            lineage: None,
            launched_agent: None,
            arena: Vec::new(),
            children: Vec::new(),
            task_by_tool_use: HashMap::new(),
            parent_by_tool_use: HashMap::new(),
            identity_by_tool_use: HashMap::new(),
            control_by_tool_use: HashMap::new(),
            child_by_parent_tool_use: HashMap::new(),
            child_by_tool_call: LruMap::new(),
            sender_by_tool_use: LruMap::new(),
            generation_by_task_id: HashMap::new(),
            pending: HashMap::new(),
            pending_count: 0,
            unresolved_launches: HashSet::new(),
            unrestorable_agents: HashSet::new(),
            last_restored_generation: 0,
            parent_cancels: 0,
            logs: Vec::new(),
        }
    }

    /// Adds the history lookups used to restore agents from before a load/resume.
    #[must_use]
    pub fn with_history(
        mut self,
        lineage: Option<LineageFn>,
        launched_agent: Option<LaunchedAgentFn>,
    ) -> Self {
        self.lineage = lineage;
        self.launched_agent = launched_agent;
        self
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.form.is_some()
    }

    #[must_use]
    pub fn form(&self) -> Option<SubagentForm> {
        self.form
    }

    /// Log lines the runtime produced (Node's `logger.log`); drains the buffer.
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.logs)
    }

    /// The child generation of a task id, if any.
    #[must_use]
    pub fn child(&self, task_id: &str) -> Option<&NativeSubagent> {
        self.child_idx(task_id).map(|i| &self.arena[i])
    }

    /// The number of unannounced updates buffered.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// The number of child generations in the arena (tests: `clear` releases
    /// them instead of growing it for the whole session).
    #[cfg(test)]
    pub(crate) fn arena_len(&self) -> usize {
        self.arena.len()
    }

    // ----- registry helpers ------------------------------------------------

    fn child_idx(&self, task_id: &str) -> Option<usize> {
        self.children
            .iter()
            .find(|(k, _)| k == task_id)
            .map(|(_, i)| *i)
    }

    fn set_child(&mut self, task_id: &str, idx: usize) {
        if let Some(entry) = self.children.iter_mut().find(|(k, _)| k == task_id) {
            entry.1 = idx;
        } else {
            self.children.push((task_id.to_string(), idx));
        }
    }

    fn child_of_session(&self, session_id: &str) -> Option<usize> {
        if session_id == self.root_session_id {
            return None;
        }
        self.children
            .iter()
            .map(|(_, i)| *i)
            .find(|i| self.arena[*i].session_id == session_id)
    }

    fn is_live_session(&self, session_id: &str) -> bool {
        if session_id == self.root_session_id {
            return true;
        }
        self.children
            .iter()
            .map(|(_, i)| *i)
            .find(|i| self.arena[*i].session_id == session_id)
            .is_some_and(|i| self.arena[i].terminal_state.is_none())
    }

    fn resumed_parent_session_id(&self, previous: usize) -> String {
        let parent = &self.arena[previous].parent_session_id;
        if self.is_live_session(parent) {
            parent.clone()
        } else {
            self.root_session_id.clone()
        }
    }

    fn remember_tool_call_owner(&mut self, tool_call_id: &str, child: usize) {
        self.child_by_tool_call.set_recent(tool_call_id, child);
    }

    fn cleanup_control(&mut self, tool_use_id: &str) {
        self.identity_by_tool_use.remove(tool_use_id);
        self.control_by_tool_use.remove(tool_use_id);
        self.parent_by_tool_use.remove(tool_use_id);
    }

    fn take_pending(&mut self, parent_tool_use_id: &str) -> Vec<Value> {
        let updates = self.pending.remove(parent_tool_use_id).unwrap_or_default();
        self.pending_count -= updates.len();
        updates
    }

    fn deliver_pending(&mut self, parent_tool_use_id: Option<&str>, outs: &mut Vec<Out>) {
        if let Some(id) = parent_tool_use_id {
            for pending in self.take_pending(id) {
                outs.push(Out::Deliver(pending));
            }
        }
    }

    /// Delivers the buffered updates of `parent_tool_use_id`, routing each one
    /// now. Node's `deliver` is the session's `sendUpdate`, which routes the
    /// update before sending it (`acp-agent.ts:4551-4633`); where the routing
    /// must happen before the runtime changes state (the end of a child), the
    /// owner gets an [`Out::Routed`] for the post-routing tail instead.
    fn deliver_pending_routed(&mut self, parent_tool_use_id: Option<&str>, outs: &mut Vec<Out>) {
        let Some(id) = parent_tool_use_id else { return };
        for pending in self.take_pending(id) {
            self.route_delivery(pending, outs);
        }
    }

    /// Routes one buffered update, appending what its route publishes (its own
    /// deliveries resolved in turn, depth first) and then the routed update.
    fn route_delivery(&mut self, notification: Value, outs: &mut Vec<Out>) {
        enum Step {
            Route(Value),
            Send(Out),
        }
        let mut stack = vec![Step::Route(notification)];
        while let Some(step) = stack.pop() {
            match step {
                Step::Send(out) => outs.push(out),
                Step::Route(notification) => {
                    let RouteResult {
                        outs: nested,
                        routed,
                    } = self.route(&notification, None);
                    if let Some(routed) = routed {
                        stack.push(Step::Send(Out::Routed(routed)));
                    }
                    for out in nested.into_iter().rev() {
                        stack.push(match out {
                            Out::Deliver(nested) => Step::Route(nested),
                            other => Step::Send(other),
                        });
                    }
                }
            }
        }
    }

    /// `buffer` (`native-subagents.ts:1046-1061`): caps 64 parents / 256 updates
    /// / 32 per parent; an update over a cap is dropped with a log line.
    fn buffer(&mut self, parent_tool_use_id: &str, notification: Value) {
        let per_parent = self.pending.get(parent_tool_use_id).map(Vec::len);
        if self.pending_count >= MAX_PENDING_UPDATES
            || (per_parent.is_none() && self.pending.len() >= MAX_PENDING_PARENTS)
            || per_parent.unwrap_or(0) >= MAX_PENDING_UPDATES_PER_PARENT
        {
            self.logs.push(format!(
                "Session {}: dropping unattributed subagent update for {parent_tool_use_id}; pending buffer limit reached",
                self.root_session_id
            ));
            return;
        }
        self.pending
            .entry(parent_tool_use_id.to_string())
            .or_default()
            .push(notification);
        self.pending_count += 1;
    }

    /// `discardPending`.
    pub fn discard_pending(&mut self, parent_tool_use_id: &str) {
        self.take_pending(parent_tool_use_id);
    }

    fn forget_restores(&mut self) {
        self.unresolved_launches.clear();
        self.unrestorable_agents.clear();
    }

    /// `clear` (`native-subagents.ts:1012-1026`).
    pub fn clear(&mut self) {
        self.epoch = next_epoch();
        self.arena.clear();
        self.children.clear();
        self.child_by_tool_call.clear();
        self.task_by_tool_use.clear();
        self.parent_by_tool_use.clear();
        self.identity_by_tool_use.clear();
        self.control_by_tool_use.clear();
        self.child_by_parent_tool_use.clear();
        self.sender_by_tool_use.clear();
        self.generation_by_task_id.clear();
        self.pending.clear();
        self.pending_count = 0;
        self.forget_restores();
    }

    // ----- routing ----------------------------------------------------------

    /// `route` (`native-subagents.ts:191-199`): routes a session notification
    /// `{sessionId, update}`; `forced_session_id` is the session a permission
    /// request created the tool call in.
    pub fn route(&mut self, notification: &Value, forced_session_id: Option<&str>) -> RouteResult {
        let result = self.route_update(notification, forced_session_id);
        if let Some(routed) = &result.routed {
            self.remember_send_message(routed);
        }
        result
    }

    fn route_update(&mut self, notification: &Value, forced: Option<&str>) -> RouteResult {
        let mut outs = Vec::new();
        let update = &notification["update"];
        let ptu = parent_tool_use_of(update);
        let is_control = is_native_subagent_control_update(update);

        if !self.enabled() {
            return RouteResult {
                outs,
                routed: Some(notification.clone()),
            };
        }

        if is_control {
            // Node keys by `undefined` when the update has no `toolCallId`, so
            // every id-less call shares one entry; `""` is that same bucket here.
            let tool_call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if update_kind(update) == Some("tool_call") {
                self.control_by_tool_use
                    .insert(tool_call_id.clone(), notification.clone());
            }
            if let Some(identity) = subagent_identity(update.get("rawInput")) {
                let merged = merge_identity(self.identity_by_tool_use.get(&tool_call_id), identity);
                self.identity_by_tool_use
                    .insert(tool_call_id.clone(), merged);
            }
            let parent_child = ptu
                .as_deref()
                .and_then(|p| self.child_by_parent_tool_use.get(p).copied());
            let parent_session_id: Option<String> = if ptu.is_some() {
                parent_child.map(|i| self.arena[i].session_id.clone())
            } else {
                Some(self.root_session_id.clone())
            };
            // The RFD form: a call made inside a child that is not announced yet
            // establishes no parent; the update is buffered below and routed
            // again once the child is announced.
            let parent_pending = self.form == Some(SubagentForm::Rfd)
                && ptu.is_some()
                && !parent_child.is_some_and(|i| self.arena[i].announced);
            let child = self.child_by_parent_tool_use.get(&tool_call_id).copied();
            if !parent_pending {
                let parent = parent_session_id
                    .clone()
                    .unwrap_or_else(|| self.root_session_id.clone());
                self.parent_by_tool_use
                    .insert(tool_call_id.clone(), parent.clone());
                if let Some(c) = child.filter(|c| !self.arena[*c].announced) {
                    self.arena[c].parent_session_id = parent;
                    let identity = self.identity_by_tool_use.get(&tool_call_id).cloned();
                    self.apply_identity(c, identity.as_ref());
                    self.announce(c, &mut outs);
                    self.deliver_pending(Some(&tool_call_id), &mut outs);
                }
            }
            if self.form == Some(SubagentForm::Air) {
                if child.is_none() && is_failed_tool_call_update(update) {
                    self.take_pending(&tool_call_id);
                    let initial = self.control_by_tool_use.get(&tool_call_id).cloned();
                    self.cleanup_control(&tool_call_id);
                    let routed = if let Some(forced) = forced {
                        with_session(notification, forced)
                    } else {
                        let session = parent_session_id
                            .clone()
                            .unwrap_or_else(|| self.root_session_id.clone());
                        failed_control_fallback(initial.as_ref(), notification, &session)
                    };
                    return RouteResult {
                        outs,
                        routed: Some(routed),
                    };
                }
                return RouteResult {
                    outs,
                    routed: forced.map(|f| with_session(notification, f)),
                };
            }
            // The RFD form: the Agent/Task tool call stays a tool call of the
            // session that made it.
        }

        let tool_call_id = matches!(update_kind(update), Some("tool_call" | "tool_call_update"))
            .then(|| update.get("toolCallId").and_then(Value::as_str))
            .flatten()
            .map(ToString::to_string);

        // A permission request may have created the tool call before native
        // child ownership was known: keep every later update in that session.
        if let Some(forced) = forced {
            let forced_child = self.child_of_session(forced);
            if let (Some(id), Some(child)) = (&tool_call_id, forced_child) {
                self.remember_tool_call_owner(id, child);
            }
            return RouteResult {
                outs,
                routed: Some(with_session(notification, forced)),
            };
        }

        let owner = tool_call_id
            .as_deref()
            .and_then(|id| self.child_by_tool_call.get(id).copied());
        if let Some(owner) = owner {
            let routed = self.route_to_child(owner, notification, tool_call_id.as_deref());
            return RouteResult { outs, routed };
        }

        if let Some(ptu) = ptu {
            let child = self.child_by_parent_tool_use.get(&ptu).copied();
            if child.is_none_or(|c| !self.arena[c].announced) {
                self.buffer(&ptu, notification.clone());
                // An id that this process never saw is the launch call of an
                // agent from before a load or resume.
                if child.is_none()
                    && self.form == Some(SubagentForm::Air)
                    && self.is_unseen_launch(&ptu)
                {
                    self.restore_launch(&ptu, &mut outs);
                }
                return RouteResult { outs, routed: None };
            }
            let routed =
                child.and_then(|c| self.route_to_child(c, notification, tool_call_id.as_deref()));
            return RouteResult { outs, routed };
        }

        RouteResult {
            outs,
            routed: Some(notification.clone()),
        }
    }

    /// `toChild`: routes an update to the child session, never to the root.
    fn route_to_child(
        &mut self,
        child: usize,
        notification: &Value,
        tool_call_id: Option<&str>,
    ) -> Option<Value> {
        if self.form == Some(SubagentForm::Air) && self.arena[child].terminal_state.is_some() {
            self.logs.push(format!(
                "Session {}: ignoring late update for terminal subagent {}",
                self.root_session_id, self.arena[child].session_id
            ));
            return None;
        }
        if let Some(id) = tool_call_id {
            self.remember_tool_call_owner(id, child);
        }
        Some(with_session(notification, &self.arena[child].session_id))
    }

    /// `routeOfToolCall` (`native-subagents.ts:942-957`): the route of work that
    /// a child tool call started. `None` means the root session owns the call.
    #[must_use]
    pub fn route_of_tool_call(
        &self,
        tool_call_id: &str,
        eager_session_id: Option<&str>,
    ) -> Option<ToolCallRoute> {
        if !self.enabled() {
            return None;
        }
        let owner = self
            .child_by_tool_call
            .get(tool_call_id)
            .copied()
            .or_else(|| eager_session_id.and_then(|s| self.child_of_session(s)));
        owner.map(|owner| ToolCallRoute {
            epoch: self.epoch,
            owner,
            session_id: self.arena[owner].session_id.clone(),
        })
    }

    /// Applies a [`ToolCallRoute`] to a notification: async-task updates still
    /// reach a finished child; everything else follows [`Self::route_to_child`].
    /// A route from another runtime, or from before a `clear`, applies to
    /// nothing.
    pub fn apply_route(&mut self, route: ToolCallRoute, notification: &Value) -> Option<Value> {
        let child = self.resolve(route.epoch, route.owner, &route.session_id)?;
        if is_late_async_task_update(notification) {
            return Some(with_session(notification, &self.arena[child].session_id));
        }
        self.route_to_child(child, notification, None)
    }

    /// The arena index of `child`, when the token still names the same runtime
    /// and the same child generation.
    fn resolve(&self, epoch: u64, child: usize, session_id: &str) -> Option<usize> {
        if epoch != self.epoch {
            return None;
        }
        self.arena
            .get(child)
            .filter(|c| c.session_id == session_id)
            .map(|_| child)
    }

    /// `rememberSendMessage` (`native-subagents.ts:963-974`).
    fn remember_send_message(&mut self, notification: &Value) {
        let update = &notification["update"];
        if update_kind(update) != Some("tool_call") {
            return;
        }
        if update
            .pointer("/_meta/claudeCode/toolName")
            .and_then(Value::as_str)
            != Some("SendMessage")
        {
            return;
        }
        // Node stores `undefined` for an update without a `toolCallId`, so every
        // such call shares one entry; `""` is that same bucket here.
        let id = update
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session = notification
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.sender_by_tool_use.set_recent(id, session.to_string());
    }

    /// The session that made a SendMessage call.
    #[must_use]
    pub fn sender_of(&self, tool_use_id: &str) -> Option<&str> {
        self.sender_by_tool_use.get(tool_use_id).map(String::as_str)
    }

    /// How many SendMessage senders are remembered.
    #[must_use]
    pub fn sender_count(&self) -> usize {
        self.sender_by_tool_use.len()
    }

    // ----- identity ---------------------------------------------------------

    fn apply_identity(&mut self, child: usize, identity: Option<&Identity>) {
        let Some(identity) = identity else { return };
        let c = &mut self.arena[child];
        if identity.name.is_some() || identity.description.is_some() {
            c.name = subagent_display_name(
                identity.name.as_deref(),
                identity.description.as_deref(),
                identity.subagent_type.as_deref(),
                &c.session_id,
            );
        }
        if identity.prompt.is_some() || identity.description.is_some() {
            c.task =
                subagent_description(identity.prompt.as_deref(), identity.description.as_deref());
        }
        if c.prompt.is_none() {
            c.prompt = identity.prompt.clone();
        }
        let role = identity.subagent_type.clone().or_else(|| c.role.clone());
        let labels = rfd_labels(
            identity.name.as_deref(),
            role.as_deref(),
            identity.description.as_deref(),
        );
        c.title = labels.title.or_else(|| c.title.take());
        c.role = labels.role.or_else(|| c.role.take());
        c.description = labels.description.or_else(|| c.description.take());
    }

    // ----- lifecycle --------------------------------------------------------

    /// `taskStarted` (`native-subagents.ts:304-385`): opens a child generation.
    pub fn task_started(&mut self, task: &TaskStarted) -> Vec<Out> {
        let mut outs = Vec::new();
        if !self.enabled() {
            return outs;
        }
        let tool_use_id = task.tool_use_id.as_deref();
        let known = if self.form == Some(SubagentForm::Rfd) {
            self.child_idx(&task.task_id)
                .or_else(|| self.restore(&task.task_id, tool_use_id, &mut outs))
        } else {
            None
        };
        if let Some(known) = known {
            // A child keeps its session for every delegation to it.
            self.resume_child(
                known,
                &task.task_id,
                prompt_text(task.prompt.as_deref()),
                tool_use_id,
                &mut outs,
            );
            return outs;
        }
        // The SDK starts an agent from before a restart under the id of the
        // SendMessage call that resumed it.
        if self.form == Some(SubagentForm::Air)
            && task.subagent_type.is_some()
            && tool_use_id.is_some_and(|id| self.sender_by_tool_use.has(id))
            && self.child_idx(&task.task_id).is_none()
        {
            self.restore_agent(
                &task.task_id,
                prompt_text(task.prompt.as_deref()),
                &mut outs,
            );
            if let (Some(restored), Some(tuid)) = (self.child_idx(&task.task_id), tool_use_id) {
                self.task_by_tool_use
                    .insert(tuid.to_string(), task.task_id.clone());
                self.child_by_parent_tool_use
                    .insert(tuid.to_string(), restored);
                return outs;
            }
        }
        if task.subagent_type.is_none() {
            if let Some(tuid) = tool_use_id {
                self.take_pending(tuid);
                self.cleanup_control(tuid);
            }
            return outs;
        }
        let previous = self.child_idx(&task.task_id);
        if previous.is_some_and(|p| self.arena[p].terminal_state.is_none()) {
            return outs;
        }
        // A SendMessage resume reuses the finished generation's tool id, whose
        // control state is already cleaned up.
        let known_parent = tool_use_id
            .and_then(|id| self.parent_by_tool_use.get(id).cloned())
            .or_else(|| previous.map(|p| self.resumed_parent_session_id(p)));
        let identity = tool_use_id.and_then(|id| self.identity_by_tool_use.get(id).cloned());
        let id_name = identity.as_ref().and_then(|i| i.name.as_deref());
        let id_description = identity
            .as_ref()
            .and_then(|i| i.description.as_deref())
            .or(task.description.as_deref());
        let id_type = identity
            .as_ref()
            .and_then(|i| i.subagent_type.as_deref())
            .or(task.subagent_type.as_deref());
        let id_prompt = identity.as_ref().and_then(|i| i.prompt.as_deref());
        let labels = rfd_labels(id_name, id_type, id_description);
        let fields = GenFields {
            parent_session_id: known_parent
                .clone()
                .unwrap_or_else(|| self.root_session_id.clone()),
            parent_tool_use_id: task.tool_use_id.clone(),
            name: subagent_display_name(id_name, id_description, id_type, &task.task_id),
            task: subagent_description(id_prompt.or(task.prompt.as_deref()), id_description),
            prompt: prompt_text(task.prompt.as_deref())
                .or_else(|| id_prompt.map(ToString::to_string)),
            title: labels.title,
            role: labels.role,
            description: labels.description,
        };
        let announce = known_parent.is_some() || task.tool_use_id.is_none();
        self.open_generation(&task.task_id, previous, fields, announce, false, &mut outs);
        outs
    }

    /// `taskResumed` (`native-subagents.ts:401-428`): a finished child resumed
    /// by a running patch or a SendMessage `resumedAgentId` opens a new generation.
    pub fn task_resumed(&mut self, task_id: &str, prompt: Option<&str>) -> Vec<Out> {
        let mut outs = Vec::new();
        if !self.enabled() {
            return outs;
        }
        let Some(previous) = self.child_idx(task_id) else {
            if self.form == Some(SubagentForm::Air) {
                self.restore_agent(task_id, prompt_text(prompt), &mut outs);
            }
            return outs;
        };
        if self.form == Some(SubagentForm::Rfd) {
            self.resume_child(previous, task_id, None, None, &mut outs);
            return outs;
        }
        if self.child_idx(task_id) != Some(previous)
            || self.arena[previous].terminal_state.is_none()
        {
            return outs;
        }
        let fields = GenFields {
            parent_session_id: self.resumed_parent_session_id(previous),
            parent_tool_use_id: self.arena[previous].parent_tool_use_id.clone(),
            name: self.arena[previous].name.clone(),
            task: self.arena[previous].task.clone(),
            prompt: prompt_text(prompt),
            title: None,
            role: None,
            description: None,
        };
        self.open_generation(task_id, Some(previous), fields, true, false, &mut outs);
        outs
    }

    /// `sendMessageResumed` (`native-subagents.ts:436-449`).
    pub fn send_message_resumed(
        &mut self,
        task_id: &str,
        prompt: Option<&str>,
        tool_use_id: Option<&str>,
    ) -> Vec<Out> {
        if self.form != Some(SubagentForm::Rfd) {
            return self.task_resumed(task_id, prompt);
        }
        let mut outs = Vec::new();
        let child = self
            .child_idx(task_id)
            .or_else(|| self.restore(task_id, tool_use_id, &mut outs));
        let Some(child) = child else { return outs };
        if self.is_ended_delegation(task_id, tool_use_id) {
            return outs;
        }
        self.resume_child(child, task_id, prompt_text(prompt), tool_use_id, &mut outs);
        outs
    }

    /// `isEndedDelegation`.
    #[must_use]
    pub fn is_ended_delegation(&self, task_id: &str, tool_use_id: Option<&str>) -> bool {
        let Some(tuid) = tool_use_id else {
            return false;
        };
        if self.form != Some(SubagentForm::Rfd) {
            return false;
        }
        self.child(task_id)
            .is_some_and(|c| c.ended_runs.contains(tuid))
    }

    /// `isStaleEnd`.
    #[must_use]
    pub fn is_stale_end(&self, task_id: &str, tool_use_id: Option<&str>) -> bool {
        let Some(tuid) = tool_use_id else {
            return false;
        };
        if self.form != Some(SubagentForm::Rfd) {
            return false;
        }
        let Some(idx) = self.child_idx(task_id) else {
            return false;
        };
        self.arena[idx]
            .parent_tool_use_id
            .as_deref()
            .is_some_and(|p| p != tuid)
            && self.child_by_parent_tool_use.get(tuid) == Some(&idx)
    }

    /// `finishTask` (`native-subagents.ts:480-535`).
    pub fn finish_task(
        &mut self,
        task_id: &str,
        status: &str,
        tool_use_id: Option<&str>,
        end: Option<&TaskEnd>,
    ) -> Vec<Out> {
        if !self.enabled() {
            return Vec::new();
        }
        match native_subagent_state(status) {
            Some(state) => self.finish_state(task_id, state, tool_use_id, end),
            None => Vec::new(),
        }
    }

    fn finish_state(
        &mut self,
        task_id: &str,
        state: SubagentState,
        tool_use_id: Option<&str>,
        end: Option<&TaskEnd>,
    ) -> Vec<Out> {
        let mut outs = Vec::new();
        let child = match tool_use_id {
            Some(tuid) => self.child_by_parent_tool_use.get(tuid).copied(),
            None => self.child_idx(task_id),
        };
        let Some(child) = child else { return outs };
        if tool_use_id
            .is_some_and(|t| self.task_by_tool_use.get(t).map(String::as_str) != Some(task_id))
        {
            return outs;
        }
        let rfd = self.form == Some(SubagentForm::Rfd);
        if rfd {
            if self.is_stale_end(task_id, tool_use_id) {
                return outs;
            }
            if !self.arena[child].announced {
                self.withdraw(task_id, child);
                return outs;
            }
        }
        let done = if rfd {
            self.arena[child].work_state == Some("idle")
        } else {
            self.arena[child].terminal_state.is_some()
        };
        if done {
            if rfd {
                self.supply_error(
                    child,
                    state,
                    end.and_then(|e| e.error.as_deref()),
                    &mut outs,
                );
            }
            return outs;
        }
        self.announce(child, &mut outs);
        let parent_tool_use = self.arena[child].parent_tool_use_id.clone();
        // Node delivers the child's buffered output inside `finishTask`, before
        // `finishNativeSubagent` sets `terminalState` (`native-subagents.ts:512-521`).
        // Route it here: an `Out::Deliver` routed after this call would be a late
        // update of a terminal child and would be dropped.
        self.deliver_pending_routed(parent_tool_use.as_deref(), &mut outs);
        if rfd {
            self.report_state(child, &rfd_end_state(state, end), &mut outs);
            let has_error = end.is_some_and(|e| e.error.as_deref().is_some_and(|s| !s.is_empty()));
            let unexplained = end.is_some_and(|e| e.unexplained);
            self.arena[child].missing_error =
                state == SubagentState::Failed && !has_error && !unexplained;
            if let Some(p) = &parent_tool_use {
                self.arena[child].ended_runs.insert(p.clone());
            }
        } else {
            self.finish_native_subagent(task_id, state, &mut outs);
        }
        if let Some(p) = parent_tool_use {
            self.cleanup_control(&p);
        }
        outs
    }

    /// `finishAll` (`native-subagents.ts:544-576`): ends every child that still
    /// runs because the stream ended or the conversation was reset. AIR reports
    /// `state` for each; the RFD form reports idle with no stop reason.
    pub fn finish_all(&mut self, state: SubagentState) -> Vec<Out> {
        let mut outs = Vec::new();
        let end = (self.form == Some(SubagentForm::Rfd)).then(|| TaskEnd {
            unexplained: true,
            ..TaskEnd::default()
        });
        if self.enabled() {
            let ids: Vec<String> = self.children.iter().rev().map(|(k, _)| k.clone()).collect();
            for id in ids {
                outs.extend(self.finish_state(&id, state, None, end.as_ref()));
            }
        }
        self.pending.clear();
        self.pending_count = 0;
        self.identity_by_tool_use.clear();
        self.control_by_tool_use.clear();
        self.parent_by_tool_use.clear();
        self.forget_restores();
        outs
    }

    /// `parentCancelled` (`native-subagents.ts:577-580`).
    pub fn parent_cancelled(&mut self) -> Vec<Out> {
        if self.form == Some(SubagentForm::Air) {
            self.finish_all(SubagentState::Cancelled)
        } else {
            self.parent_cancels += 1;
            Vec::new()
        }
    }

    /// `cancelChild` (`native-subagents.ts:594-603`): the client sent
    /// `session/cancel` for a child session. Returns the task id whose task the
    /// caller must stop (`Query.stopTask`), or `None` when `session_id` is no
    /// announced child of an RFD runtime.
    pub fn cancel_child(&mut self, session_id: &str) -> Option<String> {
        if self.form != Some(SubagentForm::Rfd) {
            return None;
        }
        let (task_id, idx) = self
            .children
            .iter()
            .find(|(_, i)| self.arena[*i].session_id == session_id && self.arena[*i].announced)
            .map(|(k, i)| (k.clone(), *i))?;
        self.arena[idx].cancels += 1;
        Some(task_id)
    }

    /// Start of `awaitingUser` (`native-subagents.ts:619-632`): a permission or
    /// question request of `session_id` opens. Returns the token for
    /// [`Self::awaiting_user_end`] when the session is an announced RFD child.
    pub fn awaiting_user_begin(&mut self, session_id: &str) -> (Option<AwaitToken>, Vec<Out>) {
        let mut outs = Vec::new();
        let child = (self.form == Some(SubagentForm::Rfd))
            .then(|| self.child_of_session(session_id))
            .flatten()
            .filter(|c| self.arena[*c].announced);
        let Some(child) = child else {
            return (None, outs);
        };
        let cancels_at_open = self.cancels_above(child);
        self.arena[child].open_requests += 1;
        if self.arena[child].open_requests == 1 && self.arena[child].work_state == Some("running") {
            self.report_state(child, &WorkState::of("requires_action"), &mut outs);
        }
        (
            Some(AwaitToken {
                epoch: self.epoch,
                child,
                session_id: self.arena[child].session_id.clone(),
                cancels_at_open,
            }),
            outs,
        )
    }

    /// End of `awaitingUser` (`native-subagents.ts:633-644`). `aborted` is
    /// Claude Code's abort signal for the request. A token whose runtime was
    /// replaced or cleared does nothing: the request belongs to a child of the
    /// old runtime, which no longer exists here.
    pub fn awaiting_user_end(&mut self, token: AwaitToken, aborted: bool) -> Vec<Out> {
        let mut outs = Vec::new();
        let Some(child) = self.resolve(token.epoch, token.child, &token.session_id) else {
            return outs;
        };
        self.arena[child].open_requests = self.arena[child].open_requests.saturating_sub(1);
        let stopped = aborted && self.cancels_above(child) != token.cancels_at_open;
        if self.arena[child].open_requests == 0
            && self.arena[child].work_state == Some("requires_action")
            && !stopped
        {
            self.report_state(child, &WorkState::of("running"), &mut outs);
        }
        outs
    }

    fn cancels_above(&self, child: usize) -> u32 {
        let mut cancels = self.parent_cancels;
        let mut seen = HashSet::new();
        let mut next = Some(child);
        while let Some(idx) = next {
            if !seen.insert(idx) {
                break;
            }
            cancels += self.arena[idx].cancels;
            next = self.child_of_session(&self.arena[idx].parent_session_id);
        }
        cancels
    }

    // ----- restore ----------------------------------------------------------

    /// `restore` (`native-subagents.ts:667-714`), the RFD form.
    fn restore(
        &mut self,
        agent_id: &str,
        tool_use_id: Option<&str>,
        outs: &mut Vec<Out>,
    ) -> Option<usize> {
        let tool_use_id = tool_use_id?;
        if self.lineage.is_none() || !self.sender_by_tool_use.has(tool_use_id) {
            return None;
        }
        let lineage = self
            .lineage
            .as_ref()
            .map(|f| f(agent_id))
            .unwrap_or_default();
        for entry in lineage {
            let existing = self.child_idx(&entry.agent_id);
            let idx = if let Some(idx) = existing {
                idx
            } else {
                let parent_session_id = match &entry.parent_agent_id {
                    Some(parent) => self
                        .child(parent)
                        .map_or_else(|| parent.clone(), |c| c.session_id.clone()),
                    None => self.root_session_id.clone(),
                };
                let labels = rfd_labels(
                    entry.name.as_deref(),
                    entry.agent_type.as_deref(),
                    entry.description.as_deref(),
                );
                self.arena.push(NativeSubagent {
                    session_id: entry.agent_id.clone(),
                    parent_session_id,
                    parent_tool_use_id: Some(entry.launch_id.clone()),
                    name: subagent_display_name(
                        entry.name.as_deref(),
                        entry.description.as_deref(),
                        entry.agent_type.as_deref(),
                        &entry.agent_id,
                    ),
                    task: subagent_description(None, entry.description.as_deref()),
                    title: labels.title,
                    role: labels.role,
                    description: labels.description,
                    ..NativeSubagent::default()
                });
                let idx = self.arena.len() - 1;
                self.set_child(&entry.agent_id, idx);
                self.task_by_tool_use
                    .insert(entry.launch_id.clone(), entry.agent_id.clone());
                self.child_by_parent_tool_use
                    .insert(entry.launch_id.clone(), idx);
                idx
            };
            if self.arena[idx].announced {
                continue;
            }
            if entry.agent_id == agent_id {
                self.announce(idx, outs);
            } else {
                let c = &self.arena[idx];
                outs.push(Out::Publish(subagent_labels_update(
                    &c.parent_session_id,
                    &c.session_id,
                    c.title.as_deref(),
                    c.description.as_deref(),
                )));
                self.arena[idx].announced = true;
            }
            self.deliver_pending(Some(&entry.launch_id), outs);
        }
        self.child_idx(agent_id)
    }

    /// `isUnseenLaunch` (AIR form).
    fn is_unseen_launch(&self, launch_id: &str) -> bool {
        self.launched_agent.is_some()
            && !self.unresolved_launches.contains(launch_id)
            && self.is_unseen_call(launch_id)
    }

    fn is_unseen_call(&self, id: &str) -> bool {
        !self.task_by_tool_use.contains_key(id)
            && !self.child_by_parent_tool_use.contains_key(id)
            && !self.control_by_tool_use.contains_key(id)
            && !self.parent_by_tool_use.contains_key(id)
            && !self.identity_by_tool_use.contains_key(id)
    }

    /// `restoreLaunch` (AIR form): restores the agent that call `launch_id` made.
    fn restore_launch(&mut self, launch_id: &str, outs: &mut Vec<Out>) {
        let agent = self.launched_agent.as_ref().and_then(|f| f(launch_id));
        if let Some(agent) = agent {
            self.restore_agent(&agent, None, outs);
        }
        let Some(child) = self.child_by_parent_tool_use.get(launch_id).copied() else {
            self.unresolved_launches.insert(launch_id.to_string());
            return;
        };
        if self.arena[child].announced {
            self.deliver_pending(Some(launch_id), outs);
        }
    }

    /// `restoreAgent` (AIR form, `native-subagents.ts:781-823`).
    fn restore_agent(&mut self, agent_id: &str, prompt: Option<String>, outs: &mut Vec<Out>) {
        if self.lineage.is_none()
            || self.child_idx(agent_id).is_some()
            || self.unrestorable_agents.contains(agent_id)
        {
            return;
        }
        let entries = self
            .lineage
            .as_ref()
            .map(|f| f(agent_id))
            .unwrap_or_default();
        let Some(entry) = entries.last().filter(|e| e.agent_id == agent_id) else {
            self.unrestorable_agents.insert(agent_id.to_string());
            return;
        };
        let parent = entry
            .parent_agent_id
            .as_deref()
            .and_then(|p| self.child_idx(p));
        let parent_session_id = match parent {
            Some(p) if self.is_live_session(&self.arena[p].session_id) => {
                self.arena[p].session_id.clone()
            }
            _ => self.root_session_id.clone(),
        };
        let fields = GenFields {
            parent_session_id,
            parent_tool_use_id: Some(entry.launch_id.clone()),
            name: subagent_display_name(
                entry.name.as_deref(),
                entry.description.as_deref(),
                entry.agent_type.as_deref(),
                agent_id,
            ),
            task: subagent_description(None, entry.description.as_deref()),
            prompt,
            title: None,
            role: None,
            description: None,
        };
        self.open_generation(agent_id, None, fields, true, true, outs);
    }

    /// `resumeChild` (`native-subagents.ts:834-862`), the RFD form.
    fn resume_child(
        &mut self,
        child: usize,
        task_id: &str,
        prompt: Option<String>,
        tool_use_id: Option<&str>,
        outs: &mut Vec<Out>,
    ) {
        if !self.arena[child].announced {
            return;
        }
        if let Some(tuid) = tool_use_id {
            if self.arena[child].parent_tool_use_id.as_deref() != Some(tuid) {
                self.task_by_tool_use
                    .insert(tuid.to_string(), task_id.to_string());
                self.child_by_parent_tool_use
                    .insert(tuid.to_string(), child);
                self.arena[child].parent_tool_use_id = Some(tuid.to_string());
            }
        }
        let working = matches!(
            self.arena[child].work_state,
            Some("running" | "requires_action")
        );
        if !working {
            let state = if self.arena[child].open_requests > 0 {
                "requires_action"
            } else {
                "running"
            };
            self.report_state(child, &WorkState::of(state), outs);
        }
        if let (Some(prompt), Some(tuid)) = (prompt, tool_use_id) {
            let sender = self.sender_by_tool_use.get(tuid).cloned();
            self.publish_prompt(child, &prompt, Some(tuid), sender.as_deref(), outs);
        }
        self.deliver_pending(tool_use_id, outs);
    }

    /// `withdraw`: a child that ends before its parent is known stays unexposed.
    fn withdraw(&mut self, task_id: &str, child: usize) {
        self.children.retain(|(k, _)| k != task_id);
        if let Some(p) = self.arena[child].parent_tool_use_id.clone() {
            self.take_pending(&p);
            self.cleanup_control(&p);
            self.child_by_parent_tool_use.remove(&p);
            self.task_by_tool_use.remove(&p);
        }
    }

    fn supply_error(
        &mut self,
        child: usize,
        state: SubagentState,
        error: Option<&str>,
        outs: &mut Vec<Out>,
    ) {
        let Some(error) = error.filter(|e| !e.is_empty()) else {
            return;
        };
        if state != SubagentState::Failed || !self.arena[child].missing_error {
            return;
        }
        let end = TaskEnd {
            error: Some(error.to_string()),
            ..TaskEnd::default()
        };
        self.report_state(
            child,
            &rfd_end_state(SubagentState::Failed, Some(&end)),
            outs,
        );
    }

    // ----- publications -----------------------------------------------------

    /// `announceNativeSubagent` (`native-subagents.ts:1169-1222`).
    fn announce(&mut self, child: usize, outs: &mut Vec<Out>) {
        let Some(form) = self.form else { return };
        if self.arena[child].announced {
            return;
        }
        if form == SubagentForm::Rfd {
            self.arena[child].work_state = Some("running");
            let c = &self.arena[child];
            outs.push(Out::Publish(subagent_announce(
                &c.parent_session_id,
                &c.session_id,
                c.title.as_deref(),
                c.description.as_deref(),
            )));
            self.arena[child].announced = true;
            if let Some(prompt) = self.arena[child].prompt.clone() {
                let tuid = self.arena[child].parent_tool_use_id.clone();
                let sender = self.arena[child].parent_session_id.clone();
                self.publish_prompt(child, &prompt, tuid.as_deref(), Some(&sender), outs);
            }
            return;
        }
        let c = &self.arena[child];
        outs.push(Out::Publish(subagent_spawned(
            &c.parent_session_id,
            &c.session_id,
            &c.name,
            &c.task,
            c.prompt.as_deref(),
        )));
        self.arena[child].announced = true;
    }

    /// `finishNativeSubagent` (AIR, `native-subagents.ts:1224-1251`).
    fn finish_native_subagent(&mut self, task_id: &str, state: SubagentState, outs: &mut Vec<Out>) {
        let Some(child) = self.child_idx(task_id) else {
            return;
        };
        if self.arena[child].terminal_state.is_some() {
            return;
        }
        self.announce(child, outs);
        let c = &self.arena[child];
        outs.push(Out::Publish(subagent_state_update(
            &c.parent_session_id,
            &c.session_id,
            state,
        )));
        self.arena[child].terminal_state = Some(state);
    }

    /// `publishPrompt` (`native-subagents.ts:1260-1281`).
    fn publish_prompt(
        &mut self,
        child: usize,
        prompt: &str,
        tool_use_id: Option<&str>,
        sender: Option<&str>,
        outs: &mut Vec<Out>,
    ) {
        let c = &mut self.arena[child];
        let message_id = tool_use_id.map_or_else(
            || format!("{}:prompt:{}", c.session_id, c.prompt_ids.len() + 1),
            ToString::to_string,
        );
        if !c.prompt_ids.insert(message_id.clone()) {
            return;
        }
        outs.push(Out::Publish(session_message(
            &c.session_id,
            &message_id,
            sender,
            prompt,
        )));
    }

    fn report_state(&mut self, child: usize, state: &WorkState, outs: &mut Vec<Out>) {
        self.arena[child].work_state = Some(state.state);
        self.arena[child].missing_error = false;
        let c = &self.arena[child];
        outs.push(Out::Publish(subagent_state(
            &c.parent_session_id,
            &c.session_id,
            state,
        )));
    }

    /// `openGeneration` (`native-subagents.ts:1089-1121`).
    fn open_generation(
        &mut self,
        task_id: &str,
        previous: Option<usize>,
        fields: GenFields,
        announce: bool,
        restored: bool,
        outs: &mut Vec<Out>,
    ) {
        let session_id = self.next_child_session_id(task_id, previous.is_some(), restored);
        self.arena.push(NativeSubagent {
            session_id,
            parent_session_id: fields.parent_session_id,
            parent_tool_use_id: fields.parent_tool_use_id,
            name: fields.name,
            task: fields.task,
            prompt: fields.prompt,
            title: fields.title,
            role: fields.role,
            description: fields.description,
            ..NativeSubagent::default()
        });
        let idx = self.arena.len() - 1;
        let tool_use_id = self.arena[idx].parent_tool_use_id.clone();
        self.set_child(task_id, idx);
        if let Some(tuid) = &tool_use_id {
            self.task_by_tool_use
                .insert(tuid.clone(), task_id.to_string());
            self.child_by_parent_tool_use.insert(tuid.clone(), idx);
            self.control_by_tool_use.remove(tuid);
        }
        if !announce {
            return;
        }
        self.announce(idx, outs);
        self.deliver_pending(tool_use_id.as_deref(), outs);
    }

    /// `nextChildSessionId` (`native-subagents.ts:1139-1158`).
    fn next_child_session_id(
        &mut self,
        task_id: &str,
        has_previous: bool,
        restored: bool,
    ) -> String {
        if restored {
            let generation = now_ms().max(self.last_restored_generation + 1);
            self.last_restored_generation = generation;
            self.generation_by_task_id
                .insert(task_id.to_string(), generation);
            return format!("{task_id}:generation:{generation}");
        }
        if !has_previous {
            self.generation_by_task_id.insert(task_id.to_string(), 1);
            return task_id.to_string();
        }
        let generation = self
            .generation_by_task_id
            .get(task_id)
            .copied()
            .unwrap_or(1)
            + 1;
        self.generation_by_task_id
            .insert(task_id.to_string(), generation);
        format!("{task_id}:generation:{generation}")
    }
}

impl Default for NativeSubagentRuntime {
    /// A runtime for a client without native subagent sessions.
    fn default() -> Self {
        Self::new(None, "")
    }
}

impl std::fmt::Debug for NativeSubagentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSubagentRuntime")
            .field("form", &self.form)
            .field("root_session_id", &self.root_session_id)
            .field("children", &self.children.len())
            .field("pending_count", &self.pending_count)
            .finish_non_exhaustive()
    }
}
