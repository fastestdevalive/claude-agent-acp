//! AIR async-task runtime (Decision 12 / Item 3.4).
//!
//! A Rust port of the subset of `dist/async-tasks.js` needed by the AIR corpus:
//! spawn held until a tool-call id is known (or released at the prompt result /
//! terminal state), progress dedupe by `changedFields`, state updates, the
//! ignored `local_agent` / `local_monitor` (and Monitor-tool) tasks, and
//! finish-all at shutdown.
//!
//! The runtime is PURE: every method returns the `Vec<Value>` of AIR `update`
//! objects it produced (e.g. `{sessionUpdate:"async_task_spawned", ...}`), in
//! emission order. The session actor wraps each in
//! `SessionOutbound::RawUpdate({"sessionId":…,"update":{…}})` and forwards it as
//! an untyped `session/update`. `AsyncTaskRuntime` therefore carries no
//! connection / publish handle and no session id.
//!
//! Mirrors Node: `field()` camel/snake lookup, `friendlyTaskType`,
//! `isBackgroundTask`, the ignore rule, the hold-until-tool-call-id (`announce`),
//! `changedFields` dedupe, `taskState` mapping, and the terminal handling
//! (`async_task_spawned/progress/state_update` payloads).

use std::collections::HashMap;

use serde_json::{json, Value};

/// The subset of `dist/async-tasks.js` needed by the AIR corpus.
#[derive(Debug, Default)]
pub struct AsyncTaskRuntime {
    /// Whether the client declared AIR `asyncTasks` (`air.supports("asyncTasks")`).
    enabled: bool,
    /// Active tasks keyed by id (holds terminal records too, per Node). A
    /// terminal record is kept on purpose, announced or not: the Bash result
    /// that proves a command went to the background can arrive after the
    /// command's terminal edge.
    tasks: HashMap<String, Task>,
    /// The task ids in the order they were first seen (a JS `Map` iterates in
    /// insertion order, and the order of the updates a sweep publishes follows).
    order: Vec<String>,
    /// The task output paths in the text of each tool result, by tool call id
    /// (`outputPathsByToolCall`). A task whose id and tool call are known takes
    /// its output path from here; the result can come before or after the SDK
    /// names the task. Oldest first, at most [`MAX_TOOL_RESULT_PATHS`].
    output_paths: Vec<(String, Vec<String>)>,
    /// What the last public call changed, so a failed send can be undone
    /// ([`Self::rollback`]): the runtime returns its updates instead of sending
    /// them, and Node records a published field only after the send succeeded.
    journal: Vec<Undo>,
    /// The tool calls of a tool whose tasks the client never sees (`Monitor`
    /// streams its output to the model only): the session reports them from the
    /// tool-use cache (`toolNameOf`, `acp-agent.ts:4538`), and a task that
    /// names one is ignored when its spawn is due.
    /// Oldest first, at most [`MAX_HIDDEN_TOOL_CALLS`] (the 1000-capped
    /// `resolvedToolNames` upstream reads).
    hidden_tool_calls: Vec<String>,
    /// The native subagent runtime that the subagent tasks feed (the tasks this
    /// runtime ignores, `async_tasks/subagents.rs`). Disabled by default.
    subagents: crate::native_subagents::NativeSubagentRuntime,
}

/// `MAX_TOOL_RESULT_PATHS` (`async-tasks.ts:48`): how many tool results the
/// runtime remembers output paths of.
const MAX_TOOL_RESULT_PATHS: usize = 500;

/// `MAX_RESOLVED_TOOL_NAMES` (`acp-agent.ts:12927`): how many tool calls the
/// session remembers a name for.
const MAX_HIDDEN_TOOL_CALLS: usize = 1000;

/// One undoable change of the last public call.
#[derive(Debug, Clone)]
enum Undo {
    /// A published field: the previous recorded value (`None` = not recorded).
    Published {
        task: String,
        key: String,
        previous: Option<String>,
    },
    /// A task that turned terminal.
    Finished {
        task: String,
        state: String,
        summary: Option<String>,
        source: Option<TerminalSource>,
    },
}

/// Where a terminal state came from (`TerminalSource`): the SDK's own edge, the
/// `background_tasks_changed` level, or a shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalSource {
    Event,
    Level,
    Shutdown,
}

/// An update of a held task that waits for its spawn (`AsyncTask.deferred`).
#[derive(Debug, Clone)]
enum Deferred {
    Progress(Value),
    State {
        state: String,
        error: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct Task {
    /// A `task_started` was seen (`startedObserved`); a level-only task was not.
    started_observed: bool,
    /// The task was recovered from the level signal alone (`panelOnlyRecovery`):
    /// it shows in the panel, never as a transcript card, and a late
    /// `task_started` keeps it that way.
    panel_only_recovery: bool,
    terminal_source: Option<TerminalSource>,
    id: String,
    name: String,
    task_type: String,
    description: String,
    show_in_transcript: bool,
    announced: bool,
    held: bool,
    /// The updates of a held task, sent after its spawn in the order they came.
    deferred: Vec<Deferred>,
    ignored: bool,
    /// `running` | `paused` | `completed` | `failed` | `stopped`.
    state: String,
    terminal_summary: Option<String>,
    output_file_path: Option<String>,
    tool_call_id: Option<String>,
    /// Key -> JSON string of the last published value (the client-held state).
    published: HashMap<String, String>,
    /// A user stop request is in flight (`claim_stop` .. `release_stop`).
    stopping: bool,
    /// The "Task stopped by user" notice was already produced for this task.
    stop_announced: bool,
    /// The route of the task's updates to the child session that owns its tool
    /// call (`AsyncTask.route`, `async-tasks.ts:632`): resolved once (`??=`),
    /// then kept for every later update.
    route: Option<crate::native_subagents::runtime::ToolCallRoute>,
}

impl Default for Task {
    fn default() -> Self {
        Task {
            started_observed: false,
            panel_only_recovery: false,
            terminal_source: None,
            id: String::new(),
            name: "Background task".to_string(),
            task_type: "task".to_string(),
            description: "Background task".to_string(),
            show_in_transcript: true,
            announced: false,
            held: false,
            deferred: Vec::new(),
            ignored: false,
            state: "running".to_string(),
            terminal_summary: None,
            output_file_path: None,
            tool_call_id: None,
            published: HashMap::new(),
            stopping: false,
            stop_announced: false,
            route: None,
        }
    }
}

/// What a user-requested stop produced (Node `taskStopped`).
#[derive(Debug, Default, PartialEq)]
pub struct StopOutcome {
    /// The AIR `update` objects (the `stopped` state update), in order.
    pub updates: Vec<Value>,
    /// The `name` for the acknowledgement notice (`**Task stopped by user:**
    /// <name>.`); `None` when the task is unknown / ignored / already announced.
    pub notice_name: Option<String>,
}

impl AsyncTaskRuntime {
    /// A runtime enabled only for an AIR client that advertised `asyncTasks`.
    pub fn new(enabled: bool) -> Self {
        AsyncTaskRuntime {
            enabled,
            tasks: HashMap::new(),
            order: Vec::new(),
            output_paths: Vec::new(),
            journal: Vec::new(),
            hidden_tool_calls: Vec::new(),
            subagents: crate::native_subagents::NativeSubagentRuntime::default(),
        }
    }

    /// The tool call `id` ran a tool whose tasks stay off the client
    /// (`HIDDEN_TASK_TOOLS`): a task started by it is ignored at its spawn.
    pub fn hide_tool_call(&mut self, id: &str) {
        if self.hidden_tool_calls.iter().any(|hidden| hidden == id) {
            return;
        }
        self.hidden_tool_calls.push(id.to_string());
        if self.hidden_tool_calls.len() > MAX_HIDDEN_TOOL_CALLS {
            self.hidden_tool_calls.remove(0);
        }
    }

    /// Undo what the last public call recorded, after its updates failed to
    /// reach the client (Node records a field as published only once the send
    /// succeeded, `recordPublished`, and `finish` restores the task's previous
    /// state when the terminal publication throws, `async-tasks.ts:673-692`): a
    /// retry then carries the same fields again.
    pub fn rollback(&mut self) {
        for undo in std::mem::take(&mut self.journal).into_iter().rev() {
            match undo {
                Undo::Published {
                    task,
                    key,
                    previous,
                } => {
                    if let Some(task) = self.tasks.get_mut(&task) {
                        match previous {
                            Some(value) => task.published.insert(key, value),
                            None => task.published.remove(&key),
                        };
                    }
                }
                Undo::Finished {
                    task,
                    state,
                    summary,
                    source,
                } => {
                    if let Some(task) = self.tasks.get_mut(&task) {
                        task.state = state;
                        task.terminal_summary = summary;
                        task.terminal_source = source;
                    }
                }
            }
        }
    }

    /// Record `fields` as held by the client, journaling the previous values.
    fn record(&mut self, id: &str, fields: &[(String, Value)]) {
        for (key, value) in fields {
            if value.is_null() {
                continue;
            }
            let Ok(serialized) = serde_json::to_string(value) else {
                continue;
            };
            let Some(task) = self.tasks.get_mut(id) else {
                continue;
            };
            let previous = task.published.insert(key.clone(), serialized);
            self.journal.push(Undo::Published {
                task: id.to_string(),
                key: key.clone(),
                previous,
            });
        }
    }

    /// Handle a `task_started` frame (Node `taskStarted`).
    pub fn task_started(&mut self, message: &Value) -> Vec<Value> {
        self.journal.clear();
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let Some(task_id) = non_blank_string(field(message, "taskId", "task_id")) else {
            return out;
        };
        if is_subagent_task(message) || is_monitor_task(message) {
            self.ignore(&task_id);
            return out;
        }
        // Node calls `this.task(taskId)` (create-if-absent) before any merge.
        self.task_mut(&task_id);
        let (previous_output, previous_tool) = self.snapshot_meta(&task_id);
        let was_announced = self.is_announced(&task_id);
        self.merge_started(&task_id, message);
        if is_background_task(
            field(message, "isBackgrounded", "is_backgrounded"),
            field(message, "taskType", "task_type"),
        ) || self.is_held(&task_id)
        {
            out.extend(self.announce(&task_id, false));
        }
        let (output, tool) = self.snapshot_meta(&task_id);
        if was_announced && (output != previous_output || tool != previous_tool) {
            out.extend(self.publish_metadata(&task_id));
        }
        out
    }

    /// A task whose command went to the background, proved by the structured
    /// Bash tool result (Node `taskBackgrounded`): announce it now, even when
    /// its `task_started` said it ran in the foreground or its terminal edge
    /// already passed (the record is kept for this).
    pub fn task_backgrounded(&mut self, message: &Value) -> Vec<Value> {
        self.journal.clear();
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let Some(task_id) = non_blank_string(field(message, "taskId", "task_id")) else {
            return out;
        };
        self.task_mut(&task_id);
        if is_subagent_task(message) || is_monitor_task(message) {
            self.ignore(&task_id);
            return out;
        }
        let (previous_output, previous_tool) = self.snapshot_meta(&task_id);
        let was_announced = self.is_announced(&task_id);
        self.merge_started(&task_id, message);
        out.extend(self.announce(&task_id, false));
        let (output, tool) = self.snapshot_meta(&task_id);
        if was_announced && (output != previous_output || tool != previous_tool) {
            out.extend(self.publish_metadata(&task_id));
        }
        out
    }

    /// Whether a task of the tool call went to the background, so the client
    /// gets it (Node `isBackgroundedToolCall`).
    pub fn is_backgrounded_tool_call(&self, tool_call_id: &str) -> bool {
        self.tasks.values().any(|task| {
            task.tool_call_id.as_deref() == Some(tool_call_id)
                && !task.ignored
                && (task.announced || task.held)
        })
    }

    /// Whether the runtime publishes tasks at all (the client declared
    /// `asyncTasks`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Reads the tool results in `content` for the output path of a known task
    /// (Node `toolResults`). Only a path that ends with `tasks/<task id>.output`
    /// in the result of the task's own tool call counts. A tool result never
    /// creates a task.
    pub fn tool_results(&mut self, content: &Value) -> Vec<Value> {
        self.journal.clear();
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let Some(blocks) = content.as_array() else {
            return out;
        };
        for result in blocks.iter().filter_map(tool_result_block) {
            let paths = result
                .content
                .and_then(text_content)
                .map(|text| task_output_paths(&text))
                .unwrap_or_default();
            if paths.is_empty() {
                continue;
            }
            self.output_paths
                .retain(|(call, _)| *call != result.tool_use_id);
            self.output_paths.push((result.tool_use_id.clone(), paths));
            if self.output_paths.len() > MAX_TOOL_RESULT_PATHS {
                self.output_paths.remove(0);
            }
            for id in self.ids_where(|task| {
                task.tool_call_id.as_deref() == Some(result.tool_use_id.as_str())
                    && !task.ignored
                    && task.output_file_path.is_none()
            }) {
                self.merge_tool_result_path(&id);
                let (output, _) = self.snapshot_meta(&id);
                if output.is_some() && self.is_announced(&id) {
                    out.extend(self.publish_metadata(&id));
                }
            }
        }
        out
    }

    /// Handle a `task_progress` frame (Node `taskProgress`).
    pub fn task_progress(&mut self, message: &Value) -> Vec<Value> {
        self.journal.clear();
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let Some(task_id) = non_blank_string(field(message, "taskId", "task_id")) else {
            return out;
        };
        let (ignored, announced_or_held) = self.progressable(&task_id);
        if !(announced_or_held && !ignored) {
            return out;
        }
        let (previous_output, previous_tool) = self.snapshot_meta(&task_id);
        self.merge_output_file_path(&task_id, message);
        self.merge_tool_call_id(&task_id, message);
        let (output, tool) = self.snapshot_meta(&task_id);
        let output_changed = output != previous_output;
        if is_terminal(&self.state_of(&task_id)) {
            if output_changed {
                out.extend(self.publish_metadata(&task_id));
            }
            return out;
        }
        let usage = task_usage(message.get("usage"));
        let description = non_blank_string(message.get("description"));
        let summary = non_blank_string(message.get("summary"));
        let last_tool_name = non_blank_string(field(message, "lastToolName", "last_tool_name"));
        let mut progress = serde_json::Map::new();
        if let Some(d) = &description {
            progress.insert("description".into(), json!(d));
        }
        if let Some(s) = &summary {
            progress.insert("summary".into(), json!(s));
        }
        if let Some(l) = &last_tool_name {
            progress.insert("lastToolName".into(), json!(l));
        }
        if let Some(u) = usage {
            progress.insert("usage".into(), u);
        }
        if output_changed && output.is_some() {
            progress.insert(
                "outputFilePath".into(),
                json!(output.clone().unwrap_or_default()),
            );
        }
        if tool != previous_tool && tool.is_some() {
            progress.insert("toolCallId".into(), json!(tool.clone().unwrap_or_default()));
        }
        out.extend(self.when_announced(&task_id, Value::Object(progress)));
        let (_, now_tool) = self.snapshot_meta(&task_id);
        if self.is_held(&task_id) && now_tool.is_some() {
            out.extend(self.announce(&task_id, false));
        }
        out
    }

    /// Handle a `task_notification` frame (Node `taskNotification`).
    pub fn task_notification(&mut self, message: &Value) -> Vec<Value> {
        self.journal.clear();
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let Some(task_id) = non_blank_string(field(message, "taskId", "task_id")) else {
            return out;
        };
        let Some(state) = task_state(non_blank_string(message.get("status")).as_deref()) else {
            return out;
        };
        if state == "running" || state == "paused" {
            return out;
        }
        // Node calls `this.task(taskId)` (create-if-absent) before merging.
        self.task_mut(&task_id);
        let was_terminal = is_terminal(&self.state_of(&task_id));
        // The SDK's edge corrects a state the level signal guessed.
        let corrects_level_state = was_terminal
            && self.tasks.get(&task_id).and_then(|t| t.terminal_source)
                == Some(TerminalSource::Level);
        let (previous_output, _) = self.snapshot_meta(&task_id);
        self.merge_output_file_path(&task_id, message);
        self.merge_tool_call_id(&task_id, message);
        out.extend(self.finish(
            &task_id,
            &state,
            non_blank_string(message.get("summary")),
            TerminalSource::Event,
        ));
        let (output, _) = self.snapshot_meta(&task_id);
        if was_terminal
            && !corrects_level_state
            && self.is_announced(&task_id)
            && output != previous_output
        {
            out.extend(self.publish_metadata(&task_id));
        }
        out
    }

    /// Handle a `task_updated` frame (Node `taskUpdated(task_id, patch)`).
    pub fn task_updated(&mut self, task_id: &str, patch: &Value) -> Vec<Value> {
        self.journal.clear();
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let task_id = non_blank_string(Some(&json!(task_id)));
        let Some(task_id) = task_id else {
            return out;
        };
        if patch.is_null() {
            return out;
        }
        // Node calls `this.task(taskId)` (create-if-absent) before merging.
        self.task_mut(&task_id);
        let (previous_output, _) = self.snapshot_meta(&task_id);
        self.merge_patch(&task_id, patch);
        if field(patch, "isBackgrounded", "is_backgrounded") == Some(&Value::Bool(true)) {
            out.extend(self.announce(&task_id, false));
        }
        let Some(state) = task_state(non_blank_string(patch.get("status")).as_deref()) else {
            let announced = self.is_announced(&task_id);
            let (output, _) = self.snapshot_meta(&task_id);
            if announced && output != previous_output {
                out.extend(self.publish_metadata(&task_id));
            }
            return out;
        };
        if state == "running" || state == "paused" {
            if is_terminal(&self.state_of(&task_id)) {
                let announced = self.is_announced(&task_id);
                let (output, _) = self.snapshot_meta(&task_id);
                if announced && output != previous_output {
                    out.extend(self.publish_metadata(&task_id));
                }
                return out;
            }
            if self.state_of(&task_id) != state {
                self.set_state(&task_id, &state);
                let error = non_blank_string(patch.get("error"));
                out.extend(self.when_announced_state(&task_id, &state, error));
            } else {
                let announced = self.is_announced(&task_id);
                let (output, _) = self.snapshot_meta(&task_id);
                if announced && output != previous_output {
                    out.extend(self.publish_metadata(&task_id));
                }
            }
            return out;
        }
        out.extend(self.finish(
            &task_id,
            &state,
            non_blank_string(patch.get("error")),
            TerminalSource::Event,
        ));
        out
    }

    /// Reconcile the SDK's replace-semantics background task level
    /// (`backgroundTasksChanged`, `async-tasks.ts:325-373`). It does not create
    /// unknown tasks' transcript cards because this level normally precedes
    /// `task_started` and carries no tool-call attribution; it can promote a
    /// known foreground task and terminate an announced task whose edge was lost.
    pub fn background_tasks_changed(&mut self, tasks: &Value) -> Vec<Value> {
        self.journal.clear();
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let Some(items) = tasks.as_array() else {
            return out;
        };
        let mut live = std::collections::HashSet::new();
        for item in items {
            if !item.is_object() {
                continue;
            }
            let Some(task_id) = non_blank_string(field(item, "taskId", "task_id")) else {
                continue;
            };
            live.insert(task_id.clone());
            self.task_mut(&task_id);
            let (ignored, state) = {
                let task = self.task_mut(&task_id);
                (task.ignored, task.state.clone())
            };
            if ignored || is_terminal(&state) {
                continue;
            }
            let level_task_type = field(item, "taskType", "task_type").and_then(Value::as_str);
            if matches!(level_task_type, Some("local_agent" | "local_monitor")) {
                self.ignore(&task_id);
                continue;
            }
            let task = self.task_mut(&task_id);
            if !task.started_observed {
                // The replace-level proves liveness but carries neither
                // transcript policy nor full identity. Recover it for the Async
                // Tasks panel only; creating a transcript card here would be
                // irreversible if a late task_started says skip_transcript=true.
                task.panel_only_recovery = true;
                task.show_in_transcript = false;
            }
            self.merge_level(&task_id, item);
            out.extend(self.announce(&task_id, false));
        }
        let lost = self.ids_where(|task| {
            (task.announced || task.held)
                && !task.ignored
                && !is_terminal(&task.state)
                && !live.contains(&task.id)
        });
        for id in lost {
            // This replace-semantics level is itself the authoritative liveness
            // boundary. Close immediately so a lost terminal edge cannot leave a
            // permanent running card; a late task_notification still corrects
            // the best-effort stopped state to completed/failed.
            out.extend(self.finish(&id, "stopped", None, TerminalSource::Level));
        }
        out
    }

    fn merge_level(&mut self, id: &str, level: &Value) {
        let Some(task) = self.tasks.get_mut(id) else {
            return;
        };
        if let Some(raw) = field(level, "taskType", "task_type") {
            task.task_type = friendly_task_type(raw.as_str().unwrap_or("task")).to_string();
        }
        if let Some(description) = non_blank_string(level.get("description")) {
            task.description = description.clone();
            task.name = description;
        }
    }

    /// Release held tasks without a tool-call id (Node `releaseHeld`), called
    /// when the prompt result ends the model turn.
    pub fn release_held(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        for id in self.ids_where(|task| task.held) {
            out.extend(self.announce(&id, true));
        }
        out
    }

    /// The ids of the tasks `keep` accepts, in the order they were first seen.
    fn ids_where(&self, keep: impl Fn(&Task) -> bool) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| self.tasks.get(*id).is_some_and(&keep))
            .cloned()
            .collect()
    }

    /// Whether a user stop may start for `id` (Node `canStop`): announced, not
    /// ignored, not already being stopped, not terminal.
    pub fn can_stop(&self, id: &str) -> bool {
        self.tasks
            .get(id)
            .is_some_and(|t| t.announced && !t.ignored && !t.stopping && !is_terminal(&t.state))
    }

    /// The tool call that started the task `id` (`task.toolCallId`), for the
    /// native subagent route of the task's updates (`AsyncTaskRuntime.send`,
    /// `async-tasks.ts:752`).
    #[must_use]
    pub fn tool_call_id_of(&self, id: &str) -> Option<String> {
        self.tasks
            .get(id)
            .and_then(|task| task.tool_call_id.clone())
    }

    /// `AsyncTaskRuntime.send` (`async-tasks.ts:749-753`): the update goes to
    /// the child session that owns the task's tool call, else the root. The
    /// route is resolved once (`task.route ??= routeOf(...)`) and kept on the
    /// task, so a later update still lands in the same child after the eager
    /// entry was forgotten or `child_by_tool_call` evicted it.
    pub fn route_task_update(
        &mut self,
        task_id: &str,
        update: Value,
        root_session_id: &str,
        eager_session_id: Option<&str>,
    ) -> Value {
        let notification = json!({"sessionId": root_session_id, "update": update});
        let kept = self.tasks.get(task_id).and_then(|task| task.route.clone());
        let route = kept.or_else(|| {
            self.tool_call_id_of(task_id).and_then(|tool_call_id| {
                self.subagents
                    .route_of_tool_call(&tool_call_id, eager_session_id)
            })
        });
        let Some(route) = route else {
            return notification;
        };
        if let Some(task) = self.tasks.get_mut(task_id) {
            if task.route.is_none() {
                task.route = Some(route.clone());
            }
        }
        self.subagents
            .apply_route(route, &notification)
            .unwrap_or(notification)
    }

    /// Claim the stop of `id` synchronously (Node `claimStop`). `true` means the
    /// caller now owns the stop and must end with [`Self::task_stopped`] or
    /// [`Self::release_stop`].
    pub fn claim_stop(&mut self, id: &str) -> bool {
        if !self.can_stop(id) {
            return false;
        }
        if let Some(task) = self.tasks.get_mut(id) {
            task.stopping = true;
        }
        true
    }

    /// Give the stop claim back after a failed `stop_task` (Node `releaseStop`);
    /// a terminal task keeps its flag.
    pub fn release_stop(&mut self, id: &str) {
        if let Some(task) = self.tasks.get_mut(id) {
            if !is_terminal(&task.state) {
                task.stopping = false;
            }
        }
    }

    /// The CLI confirmed the stop (Node `taskStopped`): finish the task as
    /// `stopped` (no terminal summary) and name the one acknowledgement notice.
    /// The notice is NOT gated on the terminal state (the SDK's own
    /// `task_notification` routinely wins the race); `stop_announced` keeps it
    /// to one line per task.
    pub fn task_stopped(&mut self, id: &str) -> StopOutcome {
        let Some(task) = self.tasks.get_mut(id) else {
            return StopOutcome::default();
        };
        if !task.announced || task.ignored || task.stop_announced {
            return StopOutcome::default();
        }
        task.stop_announced = true;
        let name = task.name.clone();
        StopOutcome {
            updates: self.finish(id, "stopped", None, TerminalSource::Event),
            notice_name: Some(name),
        }
    }

    /// Drop every task record (Node `clear`, `async-tasks.ts:488-491`), after a
    /// conversation reset or at teardown: the fresh conversation's task ids are
    /// independent of the old ones.
    pub fn clear(&mut self) {
        self.tasks.clear();
        self.order.clear();
        self.subagents.clear();
        self.output_paths.clear();
        self.hidden_tool_calls.clear();
    }

    /// Finish every announced/held non-terminal task (Node `finishAll`), called
    /// at stream end / teardown.
    pub fn finish_all(&mut self, state: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let ids =
            self.ids_where(|t| (t.announced || t.held) && !t.ignored && !is_terminal(&t.state));
        for id in ids {
            // Node wraps each in a try/catch; a failure is surfaced at the end.
            out.extend(self.finish(&id, state, None, TerminalSource::Shutdown));
        }
        out
    }

    /// [`Self::finish_all`] with a fallible publisher (Node `finishAll`,
    /// `async-tasks.ts:380-393`): every live task is finished even when an earlier
    /// publication failed, the errors are collected (Node aggregates them), and a
    /// task whose terminal publication failed gets its previous state back — so a
    /// later call retries it instead of treating it as already finished
    /// (`finish`'s `catch`, `async-tasks.ts:683-688`). Returns the error messages.
    pub fn finish_all_with(
        &mut self,
        state: &str,
        publish: &mut dyn FnMut(Value) -> Result<(), String>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.enabled {
            return errors;
        }
        let ids =
            self.ids_where(|t| (t.announced || t.held) && !t.ignored && !is_terminal(&t.state));
        for id in ids {
            let previous = self.tasks.get(&id).cloned();
            let updates = self.finish(&id, state, None, TerminalSource::Shutdown);
            let mut failed = None;
            for update in updates {
                if let Err(error) = publish(update) {
                    failed = Some(error);
                    break;
                }
            }
            if let Some(error) = failed {
                if let Some(previous) = previous {
                    self.tasks.insert(id.clone(), previous);
                }
                errors.push(error);
            }
        }
        errors
    }

    /// [`Self::finish_all_with`] with every terminal update routed to the child
    /// session that owns the task's tool call (`AsyncTaskRuntime.send`), so the
    /// stream-end finish lands where the task's earlier updates went.
    pub fn finish_all_with_routed(
        &mut self,
        state: &str,
        root_session_id: &str,
        eager_session_of: &mut dyn FnMut(&str) -> Option<String>,
        publish: &mut dyn FnMut(Value) -> Result<(), String>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.enabled {
            return errors;
        }
        let ids =
            self.ids_where(|t| (t.announced || t.held) && !t.ignored && !is_terminal(&t.state));
        for id in ids {
            let previous = self.tasks.get(&id).cloned();
            let updates = self.finish(&id, state, None, TerminalSource::Shutdown);
            let mut failed = None;
            for update in updates {
                let eager = self
                    .tool_call_id_of(&id)
                    .and_then(|tool_call_id| eager_session_of(&tool_call_id));
                let routed = self.route_task_update(&id, update, root_session_id, eager.as_deref());
                if let Err(error) = publish(routed) {
                    failed = Some(error);
                    break;
                }
            }
            if let Some(error) = failed {
                if let Some(previous) = previous {
                    self.tasks.insert(id.clone(), previous);
                }
                errors.push(error);
            }
        }
        errors
    }

    fn task_mut(&mut self, id: &str) -> &mut Task {
        if !self.tasks.contains_key(id) {
            self.order.push(id.to_string());
        }
        self.tasks.entry(id.to_string()).or_insert_with(|| Task {
            id: id.to_string(),
            ..Task::default()
        })
    }

    fn snapshot_meta(&self, id: &str) -> (Option<String>, Option<String>) {
        match self.tasks.get(id) {
            Some(t) => (t.output_file_path.clone(), t.tool_call_id.clone()),
            None => (None, None),
        }
    }

    fn is_announced(&self, id: &str) -> bool {
        self.tasks.get(id).map(|t| t.announced).unwrap_or(false)
    }

    fn is_held(&self, id: &str) -> bool {
        self.tasks.get(id).map(|t| t.held).unwrap_or(false)
    }

    fn progressable(&self, id: &str) -> (bool, bool) {
        match self.tasks.get(id) {
            Some(t) => (t.ignored, t.announced || t.held),
            None => (false, false),
        }
    }

    fn state_of(&self, id: &str) -> String {
        self.tasks
            .get(id)
            .map(|t| t.state.clone())
            .unwrap_or_default()
    }

    fn set_state(&mut self, id: &str, state: &str) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.state = state.to_string();
        }
    }

    fn merge_started(&mut self, id: &str, message: &Value) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.started_observed = true;
        }
        if let Some(v) = field(message, "taskType", "task_type") {
            let ft = friendly_task_type(v.as_str().unwrap_or("task")).to_string();
            if let Some(t) = self.tasks.get_mut(id) {
                t.task_type = ft;
            }
        }
        let skip = field(message, "skipTranscript", "skip_transcript");
        if let Some(t) = self.tasks.get_mut(id) {
            if t.panel_only_recovery {
                // A level-only recovery was already announced without a
                // transcript entry. Keep that decision monotonic: a late
                // task_started can enrich the panel, but cannot retroactively
                // create or mutate transcript UI.
                t.show_in_transcript = false;
            } else if let Some(skip) = skip {
                t.show_in_transcript = skip != &Value::Bool(true);
            }
        }
        let task_type = self
            .tasks
            .get(id)
            .map(|t| t.task_type.clone())
            .unwrap_or_default();
        let desc = non_blank_string(message.get("description"));
        let workflow = non_blank_string(field(message, "workflowName", "workflow_name"));
        let is_default_desc = self
            .tasks
            .get(id)
            .map(|t| t.description == "Background task")
            .unwrap_or(false);
        if let Some(d) = desc {
            if let Some(t) = self.tasks.get_mut(id) {
                t.description = d.clone();
                t.name = workflow.clone().unwrap_or(d);
            }
        } else if is_default_desc {
            if let Some(t) = self.tasks.get_mut(id) {
                let d = default_description(&task_type);
                t.description = d.clone();
                t.name = workflow.clone().unwrap_or(d);
            }
        } else if let Some(t) = self.tasks.get_mut(id) {
            t.name = workflow.clone().unwrap_or_else(|| t.description.clone());
        }
        if let Some(t) = non_blank_string(field(message, "toolCallId", "tool_use_id")) {
            if let Some(task) = self.tasks.get_mut(id) {
                task.tool_call_id = Some(t);
            }
        }
        self.merge_output_file_path(id, message);
        self.merge_tool_result_path(id);
    }

    fn merge_output_file_path(&mut self, id: &str, value: &Value) {
        if let Some(p) = non_blank_string(field(value, "outputFilePath", "output_file")) {
            if let Some(t) = self.tasks.get_mut(id) {
                t.output_file_path = Some(p);
            }
        }
    }

    fn merge_tool_call_id(&mut self, id: &str, value: &Value) {
        let existing = self.tasks.get(id).and_then(|t| t.tool_call_id.clone());
        if existing.is_none() {
            if let Some(t) = non_blank_string(field(value, "toolCallId", "tool_use_id")) {
                if let Some(task) = self.tasks.get_mut(id) {
                    task.tool_call_id = Some(t);
                }
            }
        }
        self.merge_tool_result_path(id);
    }

    /// Takes the output path from the result of the task's tool call when no
    /// structured source gave one. A structured path later replaces it
    /// (`mergeToolResultPath`).
    fn merge_tool_result_path(&mut self, id: &str) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if task.output_file_path.is_some() {
            return;
        }
        let Some(tool_call) = task.tool_call_id.as_deref() else {
            return;
        };
        let path = self
            .output_paths
            .iter()
            .find(|(call, _)| call == tool_call)
            .and_then(|(_, paths)| output_file_path_of(paths, id));
        if let Some(path) = path {
            if let Some(task) = self.tasks.get_mut(id) {
                task.output_file_path = Some(path);
            }
        }
    }

    fn merge_patch(&mut self, id: &str, patch: &Value) {
        if let Some(d) = non_blank_string(patch.get("description")) {
            if let Some(t) = self.tasks.get_mut(id) {
                t.description = d.clone();
                t.name = d;
            }
        }
        self.merge_output_file_path(id, patch);
    }

    /// Port of Node `whenAnnounced`: send now if announced, else defer if held.
    fn when_announced(&mut self, id: &str, progress: Value) -> Vec<Value> {
        let announced = self.is_announced(id);
        let held = self.is_held(id);
        if announced {
            let mut out = Vec::new();
            out.extend(self.publish_progress(id, progress));
            out
        } else if held {
            if let Some(t) = self.tasks.get_mut(id) {
                t.deferred.push(Deferred::Progress(progress));
            }
            Vec::new()
        } else {
            Vec::new()
        }
    }

    /// [`Self::when_announced`] for a state update: a held task's state change
    /// waits for the spawn and follows it in order.
    fn when_announced_state(&mut self, id: &str, state: &str, error: Option<String>) -> Vec<Value> {
        if self.is_announced(id) {
            self.publish_state(id, state, error)
        } else {
            if self.is_held(id) {
                if let Some(task) = self.tasks.get_mut(id) {
                    task.deferred.push(Deferred::State {
                        state: state.to_string(),
                        error,
                    });
                }
            }
            Vec::new()
        }
    }

    fn publish_metadata(&mut self, id: &str) -> Vec<Value> {
        let (output, tool) = self.snapshot_meta(id);
        if output.is_none() && tool.is_none() {
            return Vec::new();
        }
        if is_terminal(&self.state_of(id)) {
            self.publish_state(id, &self.state_of(id), self.terminal_summary(id))
        } else {
            let mut meta = serde_json::Map::new();
            if let Some(p) = &output {
                meta.insert("outputFilePath".into(), json!(p));
            }
            if let Some(t) = &tool {
                meta.insert("toolCallId".into(), json!(t));
            }
            self.publish_progress(id, Value::Object(meta))
        }
    }

    fn terminal_summary(&self, id: &str) -> Option<String> {
        self.tasks.get(id).and_then(|t| t.terminal_summary.clone())
    }

    fn publish_progress(&mut self, id: &str, update: Value) -> Vec<Value> {
        let changed = changed_fields(self.tasks.get(id), &update);
        let empty = changed.as_object().map(|m| m.is_empty()).unwrap_or(true);
        if empty {
            return Vec::new();
        }
        let mut final_update = serde_json::Map::new();
        final_update.insert("sessionUpdate".into(), json!("async_task_progress"));
        final_update.insert("asyncTaskId".into(), json!(id));
        for (k, v) in changed.as_object().into_iter().flat_map(|m| m.iter()) {
            final_update.insert(k.clone(), v.clone());
        }
        self.record_changed(id, &changed);
        vec![Value::Object(final_update)]
    }

    /// [`Self::record`] for a `changedFields` object.
    fn record_changed(&mut self, id: &str, changed: &Value) {
        let fields: Vec<(String, Value)> = changed
            .as_object()
            .into_iter()
            .flat_map(|object| object.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect();
        self.record(id, &fields);
    }

    fn publish_state(&mut self, id: &str, state: &str, summary: Option<String>) -> Vec<Value> {
        let meta = json!({
            "outputFilePath": self.snapshot_meta(id).0,
            "toolCallId": self.snapshot_meta(id).1,
        });
        let changed = changed_fields(self.tasks.get(id), &meta);
        let mut update = serde_json::Map::new();
        update.insert("sessionUpdate".into(), json!("async_task_state_update"));
        update.insert("asyncTaskId".into(), json!(id));
        update.insert("state".into(), json!(state));
        if let Some(s) = &summary {
            update.insert("summary".into(), json!(s));
        }
        for (k, v) in changed.as_object().into_iter().flat_map(|m| m.iter()) {
            update.insert(k.clone(), v.clone());
        }
        self.record_changed(id, &changed);
        vec![Value::Object(update)]
    }

    /// Port of Node `announce(task, options)`.
    fn announce(&mut self, id: &str, without_tool_call: bool) -> Vec<Value> {
        let mut out = Vec::new();
        let hidden = self
            .tasks
            .get(id)
            .and_then(|task| task.tool_call_id.as_deref())
            .is_some_and(|tool_call| {
                self.hidden_tool_calls
                    .iter()
                    .any(|hidden| hidden == tool_call)
            });
        {
            let task = self.task_mut(id);
            if task.announced || task.ignored {
                return out;
            }
            if hidden {
                // A task that a `Monitor` tool call started is ignored: Monitor
                // streams its output to the model only.
                task.ignored = true;
                task.held = false;
                task.deferred.clear();
                return out;
            }
            if task.tool_call_id.is_none() && !without_tool_call && !is_terminal(&task.state) {
                task.held = true;
                return out;
            }
            // Build and push the spawn.
            let mut spawn = serde_json::Map::new();
            spawn.insert("sessionUpdate".into(), json!("async_task_spawned"));
            spawn.insert("asyncTaskId".into(), json!(task.id));
            spawn.insert("name".into(), json!(task.name));
            spawn.insert("taskType".into(), json!(task.task_type));
            spawn.insert("description".into(), json!(task.description));
            spawn.insert("showInTranscript".into(), json!(task.show_in_transcript));
            spawn.insert("canStop".into(), json!(true));
            if let Some(p) = &task.output_file_path {
                spawn.insert("outputFilePath".into(), json!(p));
            }
            if let Some(t) = &task.tool_call_id {
                spawn.insert("toolCallId".into(), json!(t));
            }
            out.push(Value::Object(spawn));
            task.announced = true;
            task.held = false;
        }
        // The spawn carries these: the client holds them now.
        let (description, output, tool) = {
            let task = self.task_mut(id);
            (
                task.description.clone(),
                task.output_file_path.clone(),
                task.tool_call_id.clone(),
            )
        };
        self.record(
            id,
            &[
                ("description".to_string(), json!(description)),
                ("outputFilePath".to_string(), json!(output)),
                ("toolCallId".to_string(), json!(tool)),
            ],
        );
        // Replay deferred progress payloads, then a terminal-state publish.
        let deferred = std::mem::take(&mut self.task_mut(id).deferred);
        for update in deferred {
            match update {
                Deferred::Progress(progress) => out.extend(self.publish_progress(id, progress)),
                Deferred::State { state, error } => {
                    out.extend(self.publish_state(id, &state, error));
                }
            }
        }
        if is_terminal(&self.state_of(id)) {
            out.extend(self.publish_state(id, &self.state_of(id), self.terminal_summary(id)));
        }
        out
    }

    /// Port of Node `finish`.
    fn finish(
        &mut self,
        id: &str,
        state: &str,
        summary: Option<String>,
        source: TerminalSource,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        {
            let task = self.task_mut(id);
            if task.ignored {
                return out;
            }
            if is_terminal(&task.state)
                // A level event can precede a late terminal edge (older CLIs sent
                // the level first): correct its best-effort stopped state when
                // that edge arrives.
                && !(task.terminal_source == Some(TerminalSource::Level)
                    && source == TerminalSource::Event)
            {
                return out;
            }
            let undo = Undo::Finished {
                task: id.to_string(),
                state: std::mem::replace(&mut task.state, state.to_string()),
                summary: std::mem::replace(&mut task.terminal_summary, summary),
                source: task.terminal_source.replace(source),
            };
            self.journal.push(undo);
        }
        if self.is_announced(id) {
            out.extend(self.publish_state(id, state, self.terminal_summary(id)));
        } else if self.is_held(id) {
            out.extend(self.announce(id, false));
        }
        out
    }

    /// Mark a task ignored (Node `ignore`).
    fn ignore(&mut self, id: &str) {
        let task = self.task_mut(id);
        if task.announced {
            return;
        }
        task.ignored = true;
        task.held = false;
        task.deferred.clear();
    }
}

// ---------------------------------------------------------------------------
// Pure helpers mirroring `dist/async-tasks.js`.
// ---------------------------------------------------------------------------

/// `value[camel]` if defined, else `value[snake]`.
fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    value.get(camel).or_else(|| value.get(snake))
}

/// A non-blank trimmed string, else `None` (Node `nonBlankString`).
fn non_blank_string(value: Option<&Value>) -> Option<String> {
    let s = value?.as_str()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn friendly_task_type(value: &str) -> &str {
    match value {
        "local_bash" => "shell",
        "local_workflow" => "workflow",
        "local_monitor" | "mcp" => "monitor",
        v if !v.trim().is_empty() => v,
        _ => "task",
    }
}

fn default_description(task_type: &str) -> String {
    if task_type == "task" {
        return "Background task".to_string();
    }
    let mut chars = task_type.chars();
    match chars.next() {
        Some(first) => {
            let mut s = String::new();
            s.extend(first.to_uppercase());
            s.push_str(chars.as_str());
            s
        }
        None => "Background task".to_string(),
    }
}

/// Node `isBackgroundTask`: explicit bool wins, else a non-(bash|agent) task is
/// considered background.
fn is_background_task(is_backgrounded: Option<&Value>, task_type: Option<&Value>) -> bool {
    match is_backgrounded {
        Some(Value::Bool(true)) => return true,
        Some(Value::Bool(false)) => return false,
        _ => {}
    }
    let tt = task_type.and_then(Value::as_str).unwrap_or("");
    tt != "local_bash" && tt != "local_agent"
}

/// Node `isMonitorTask`: `task_type == "local_monitor"`.
fn is_monitor_task(message: &Value) -> bool {
    field(message, "taskType", "task_type").and_then(Value::as_str) == Some("local_monitor")
}

/// Node `isSubagentTask`: `subagent_type` present OR `task_type == "local_agent"`.
fn is_subagent_task(message: &Value) -> bool {
    field(message, "subagentType", "subagent_type").is_some()
        || field(message, "taskType", "task_type").and_then(Value::as_str) == Some("local_agent")
}

/// Node `taskState`: map a CLI status to the AIR state.
fn task_state(value: Option<&str>) -> Option<String> {
    match value {
        Some("pending") | Some("running") => Some("running".to_string()),
        Some("paused") => Some("paused".to_string()),
        Some("completed") => Some("completed".to_string()),
        Some("failed") => Some("failed".to_string()),
        Some("killed") | Some("cancelled") | Some("stopped") => Some("stopped".to_string()),
        _ => None,
    }
}

fn is_terminal(state: &str) -> bool {
    state == "completed" || state == "failed" || state == "stopped"
}

/// Node `taskUsage`: only when all three numeric fields are present.
fn task_usage(value: Option<&Value>) -> Option<Value> {
    let record = value.filter(|value| value.is_object())?;
    let total = number_field(record, "totalTokens", "total_tokens")?;
    let tool_uses = number_field(record, "toolUses", "tool_uses")?;
    let duration = number_field(record, "durationMs", "duration_ms")?;
    // The numbers are copied as they came: an integer stays an integer.
    Some(json!({
        "totalTokens": total,
        "toolUses": tool_uses,
        "durationMs": duration,
    }))
}

fn number_field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    field(value, camel, snake).filter(|number| number.is_number())
}

/// Node `changedFields`: the fields whose serialized value differs from what
/// the client holds. `null` (Node `undefined`) is not a field.
fn changed_fields(task: Option<&Task>, fields: &Value) -> Value {
    let mut changed = serde_json::Map::new();
    let Some(obj) = fields.as_object() else {
        return Value::Object(changed);
    };
    let some_task = task;
    for (key, value) in obj {
        if value.is_null() {
            continue;
        }
        let serialized = serde_json::to_string(value).unwrap_or_default();
        let held = some_task
            .and_then(|t| t.published.get(key))
            .map(|s| s == &serialized)
            .unwrap_or(false);
        if !held {
            changed.insert(key.clone(), value.clone());
        }
    }
    Value::Object(changed)
}

/// The Bash tool call and the structured result of a tool use in the session's
/// cache, as the background helpers read them: the tool name and input.
pub type ToolUseOf<'a> = &'a dyn Fn(&str) -> Option<Value>;

/// One `tool_result` block of a user message (`toolResultBlock`).
struct ToolResultBlock<'a> {
    tool_use_id: String,
    content: Option<&'a Value>,
    is_error: bool,
}

fn tool_result_block(value: &Value) -> Option<ToolResultBlock<'_>> {
    if value.get("type").and_then(Value::as_str) != Some("tool_result") {
        return None;
    }
    let tool_use_id = non_blank_string(field(value, "toolUseId", "tool_use_id"))?;
    Some(ToolResultBlock {
        tool_use_id,
        content: value.get("content"),
        is_error: field(value, "isError", "is_error") == Some(&Value::Bool(true)),
    })
}

/// The name a tool call ran, from the session's tool-use cache.
fn tool_name(tool_use: &Value) -> Option<&str> {
    tool_use.get("name").and_then(Value::as_str)
}

/// Recovers a background Bash task from the structured `backgroundTaskId` of
/// its tool result (`backgroundBashTaskFromToolResult`,
/// `async-tasks.ts:780-870`). The text of a tool result never gives a task id: a
/// foreground command can print any text. The result is the `task_started`-shaped
/// message for [`AsyncTaskRuntime::task_backgrounded`].
pub fn background_bash_task_from_tool_result(
    content: &Value,
    tool_use_result: Option<&Value>,
    tool_use_of: ToolUseOf<'_>,
) -> Option<Value> {
    let blocks = content.as_array()?;
    let tool_results: Vec<ToolResultBlock<'_>> =
        blocks.iter().filter_map(tool_result_block).collect();
    let bash_results: Vec<&ToolResultBlock<'_>> = tool_results
        .iter()
        .filter(|result| {
            tool_use_of(&result.tool_use_id)
                .as_ref()
                .and_then(|tool_use| tool_name(tool_use).map(str::to_string))
                .as_deref()
                == Some("Bash")
        })
        .collect();
    if bash_results.is_empty() {
        return None;
    }
    let mut background_results = Vec::new();
    if let Some(value) = tool_use_result {
        structured_background_results(value, tool_use_of, None, &mut background_results);
    }
    let mut task_ids: Vec<&str> = Vec::new();
    for result in &background_results {
        if !task_ids.contains(&result.task_id.as_str()) {
            task_ids.push(&result.task_id);
        }
    }
    if task_ids.len() != 1 {
        return None;
    }
    let task_id = task_ids[0].to_string();
    let hinted: Vec<&str> = background_results
        .iter()
        .filter(|result| result.task_id == task_id)
        .filter_map(|result| result.tool_use_id.as_deref())
        .collect();
    let matches: Vec<&ToolResultBlock<'_>> = if !hinted.is_empty() {
        bash_results
            .iter()
            .copied()
            .filter(|result| hinted.contains(&result.tool_use_id.as_str()))
            .collect()
    } else {
        let by_path: Vec<&ToolResultBlock<'_>> = bash_results
            .iter()
            .copied()
            .filter(|result| async_task_output_file_path(result.content, &task_id).is_some())
            .collect();
        if !by_path.is_empty() {
            by_path
        } else if bash_results.len() == 1 {
            bash_results.clone()
        } else {
            Vec::new()
        }
    };
    let [matched] = matches.as_slice() else {
        return None;
    };
    let command = tool_use_of(&matched.tool_use_id)
        .as_ref()
        .and_then(|tool_use| tool_use.get("input"))
        .and_then(|input| non_blank_string(input.get("command")));
    let mut message = serde_json::Map::new();
    message.insert("taskId".into(), json!(task_id));
    message.insert("taskType".into(), json!("local_bash"));
    if let Some(command) = command {
        message.insert("description".into(), json!(command));
    }
    message.insert("isBackgrounded".into(), json!(true));
    if let Some(path) = async_task_output_file_path(matched.content, &task_id) {
        message.insert("outputFilePath".into(), json!(path));
    }
    message.insert("toolCallId".into(), json!(matched.tool_use_id));
    Some(Value::Object(message))
}

/// The Bash tool calls in `content` whose command went to the background
/// (`backgroundedBashToolCallIds`, `async-tasks.ts:870-900`). Only structured
/// data counts: the task from the structured tool result, a task the runtime
/// knows for the tool call, or a `run_in_background` input of a call that did
/// not fail.
pub fn backgrounded_bash_tool_call_ids(
    content: &Value,
    tool_use_of: ToolUseOf<'_>,
    runtime: &AsyncTaskRuntime,
    structured_tool_call_id: Option<&str>,
) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    let Some(blocks) = content.as_array().filter(|_| runtime.enabled()) else {
        return ids;
    };
    for result in blocks.iter().filter_map(tool_result_block) {
        let Some(tool_use) = tool_use_of(&result.tool_use_id) else {
            continue;
        };
        if tool_name(&tool_use) != Some("Bash") {
            continue;
        }
        let run_in_background = tool_use
            .get("input")
            .and_then(|input| input.get("run_in_background"))
            == Some(&Value::Bool(true));
        if structured_tool_call_id == Some(result.tool_use_id.as_str())
            || runtime.is_backgrounded_tool_call(&result.tool_use_id)
            || (run_in_background && !result.is_error)
        {
            ids.insert(result.tool_use_id);
        }
    }
    ids
}

/// A `backgroundTaskId` found in a structured tool result, with the tool call it
/// was attributed to.
struct StructuredBackground {
    task_id: String,
    tool_use_id: Option<String>,
}

fn structured_background_results(
    value: &Value,
    tool_use_of: ToolUseOf<'_>,
    hinted_tool_use_id: Option<&str>,
    out: &mut Vec<StructuredBackground>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                structured_background_results(item, tool_use_of, hinted_tool_use_id, out);
            }
        }
        Value::Object(map) => {
            let direct_task_id =
                non_blank_string(field(value, "backgroundTaskId", "background_task_id"));
            let direct_tool_use_id = non_blank_string(field(value, "toolUseId", "tool_use_id"))
                .or_else(|| hinted_tool_use_id.map(str::to_string));
            if let Some(task_id) = direct_task_id {
                out.push(StructuredBackground {
                    task_id,
                    tool_use_id: direct_tool_use_id.clone(),
                });
            }
            for (key, child) in map {
                if key == "backgroundTaskId" || key == "background_task_id" {
                    continue;
                }
                let hint = if tool_use_of(key).is_some() {
                    Some(key.as_str())
                } else {
                    direct_tool_use_id.as_deref()
                };
                structured_background_results(child, tool_use_of, hint, out);
            }
        }
        _ => {}
    }
}

fn async_task_output_file_path(content: Option<&Value>, task_id: &str) -> Option<String> {
    let text = text_content(content?)?;
    output_file_path_of(&task_output_paths(&text), task_id)
}

/// The text of a tool result content (`textContent`).
fn text_content(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(text_content).collect();
            (!parts.is_empty()).then(|| parts.concat())
        }
        Value::Object(map) => match map.get("text").and_then(Value::as_str) {
            Some(text) => Some(text.to_string()),
            None => map.get("content").and_then(text_content),
        },
        _ => None,
    }
}

/// The output path of the task `task_id` among `paths`, if one names it
/// (`outputFilePathOf`).
fn output_file_path_of(paths: &[String], task_id: &str) -> Option<String> {
    let posix = format!("/tasks/{task_id}.output");
    let windows = format!("\\tasks\\{task_id}.output");
    paths
        .iter()
        .find(|path| path.ends_with(&posix) || path.ends_with(&windows))
        .cloned()
}

fn is_separator(c: char) -> bool {
    c == '/' || c == '\\'
}

/// An absolute POSIX, Windows drive or UNC path (`ABSOLUTE_PATH`).
fn is_absolute_path(path: &str) -> bool {
    let mut chars = path.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some('/'), _, _) => true,
        (Some('\\'), Some('\\'), _) => true,
        (Some(drive), Some(':'), Some(sep)) => drive.is_ascii_alphabetic() && is_separator(sep),
        _ => false,
    }
}

/// The end of a task output path (`TASK_OUTPUT_SUFFIX`): `tasks/<id>.output`
/// after a separator, followed by a sentence period, whitespace or the end of
/// the text. Returns the byte range of the match starting at or after `from`.
fn next_task_output_suffix(text: &str, from: usize) -> Option<(usize, usize)> {
    let tail = text.get(from..)?;
    for (offset, c) in tail.char_indices() {
        if !is_separator(c) {
            continue;
        }
        let start = from + offset;
        let after = &text[start + 1..];
        let Some(rest) = after.strip_prefix("tasks") else {
            continue;
        };
        let mut rest_chars = rest.chars();
        if !rest_chars.next().is_some_and(is_separator) {
            continue;
        }
        let name_start = start + 1 + "tasks".len() + 1;
        let name = &text[name_start..];
        // The shortest name (one or more characters that are neither whitespace
        // nor a separator) that `.output` and the lookahead follow.
        for (index, ch) in name.char_indices() {
            if ch.is_whitespace() || is_separator(ch) {
                break;
            }
            let end_of_name = index + ch.len_utf8();
            let after_name = &name[end_of_name..];
            let Some(tail_after) = after_name.strip_prefix(".output") else {
                continue;
            };
            let boundary = match tail_after.chars().next() {
                None => true,
                Some(next) if next.is_whitespace() => true,
                Some('.') => !tail_after[1..]
                    .chars()
                    .next()
                    .is_some_and(|word| word.is_ascii_alphanumeric() || word == '_'),
                Some(_) => false,
            };
            if boundary {
                return Some((start, name_start + end_of_name + ".output".len()));
            }
        }
    }
    None
}

/// The task output paths in a tool result text (`taskOutputPaths`): each text of
/// Claude Code puts the path after "Output is being written to: ", so a path
/// starts after the nearest `": "` before its end. A drive colon has no space
/// after it, so a path can hold spaces and a drive. The search for the start stops
/// at a line break and at the end of the previous path, so a path never crosses a
/// line or holds another path. A path that is not absolute does not count.
fn task_output_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut previous_end = 0;
    let mut from = 0;
    while let Some((index, end)) = next_task_output_suffix(text, from) {
        let mut path_start = None;
        let before = &text[previous_end..index];
        // Walk back for ": " (a colon followed by a space) without crossing a line.
        let bytes = text.as_bytes();
        for (offset, c) in before.char_indices().rev() {
            if c == '\n' || c == '\r' {
                break;
            }
            let at = previous_end + offset;
            if c == ':' && bytes.get(at + 1) == Some(&b' ') {
                path_start = Some(at + 2);
                break;
            }
        }
        previous_end = end;
        from = end;
        let Some(start) = path_start else {
            continue;
        };
        if start > end {
            continue;
        }
        let path = &text[start..end];
        if is_absolute_path(path) {
            paths.push(path.to_string());
        }
    }
    paths
}

mod subagents;
pub use subagents::SubagentSends;

#[cfg(test)]
mod tests_t4b;

#[cfg(test)]
mod tests_t3a;

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> AsyncTaskRuntime {
        AsyncTaskRuntime::new(true)
    }

    #[test]
    fn inv_40_async_task_spawn_waits_for_tool_call_id() {
        let mut r = rt();
        let started = json!({"task_id":"t1","task_type":"local_workflow","description":"Deploy"});
        assert!(r.task_started(&started).is_empty(), "held, no spawn yet");
        // Terminal notification releases the held task: spawn + state update.
        let notif = json!({"task_id":"t1","status":"completed","summary":"done"});
        let updates = r.task_notification(&notif);
        assert_eq!(updates.len(), 2, "spawn + terminal state update");
        assert_eq!(
            updates[0]["sessionUpdate"].as_str().unwrap(),
            "async_task_spawned"
        );
        assert_eq!(updates[0]["asyncTaskId"], "t1");
        assert_eq!(updates[0]["name"], "Deploy");
        assert_eq!(updates[0]["taskType"], "workflow");
        assert!(updates[0].get("toolCallId").is_none());
        assert_eq!(updates[1]["sessionUpdate"], "async_task_state_update");
        assert_eq!(updates[1]["state"], "completed");
    }

    #[test]
    fn inv_40_async_task_terminal_before_id_spawns_without_tool_call() {
        let mut r = rt();
        let started = json!({"task_id":"t1","task_type":"local_workflow","description":"Deploy"});
        r.task_started(&started);
        let updates = r.finish_all("stopped");
        assert_eq!(updates.len(), 2, "spawn + terminal state update");
        assert_eq!(updates[0]["sessionUpdate"], "async_task_spawned");
        assert!(updates[0].get("toolCallId").is_none());
        assert_eq!(updates[1]["sessionUpdate"], "async_task_state_update");
        assert_eq!(updates[1]["state"], "stopped");
    }

    /// V-18 — upstream "finishes remaining tasks and can retry a task whose
    /// terminal publication failed" (`async-tasks.test.ts:1039-1081`): a failing
    /// publish for `first` does not stop `second` from finishing, the failure is
    /// reported, and a second `finish_all` retries `first`.
    #[test]
    fn v18_finish_all_continues_and_retries_a_failed_terminal_publication() {
        let mut r = rt();
        for id in ["first", "second"] {
            let started = json!({"task_id": id, "task_type": "local_bash",
                "description": id, "is_backgrounded": true});
            r.task_started(&started);
        }
        let mut published: Vec<Value> = Vec::new();
        let mut fail_first = true;
        let mut publish = |update: Value| -> Result<(), String> {
            if fail_first
                && update["sessionUpdate"] == "async_task_state_update"
                && update["asyncTaskId"] == "first"
            {
                fail_first = false;
                return Err("client disconnected".to_string());
            }
            published.push(update);
            Ok(())
        };
        let errors = r.finish_all_with("failed", &mut publish);
        assert_eq!(errors, vec!["client disconnected".to_string()]);
        let errors = r.finish_all_with("failed", &mut publish);
        assert!(errors.is_empty(), "the retry succeeds: {errors:?}");
        let terminal_ids: Vec<&str> = published
            .iter()
            .filter(|u| u["sessionUpdate"] == "async_task_state_update")
            .filter_map(|u| u["asyncTaskId"].as_str())
            .collect();
        assert_eq!(terminal_ids, vec!["second", "first"]);
        // Finished tasks are not finished again.
        assert!(r.finish_all_with("failed", &mut |_| Ok(())).is_empty());
    }

    #[test]
    fn inv_40_async_task_progress_dedupes_and_defers_until_announce() {
        let mut r = rt();
        let started = json!({"task_id":"t1","task_type":"local_workflow","description":"Deploy"});
        r.task_started(&started);
        let progress = json!({"task_id":"t1","description":"Deploying","summary":"step 1"});
        r.task_progress(&progress);
        let notif = json!({"task_id":"t1","status":"completed","summary":"done"});
        let updates = r.task_notification(&notif);
        // spawn + deferred progress + state update.
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0]["sessionUpdate"], "async_task_spawned");
        assert_eq!(updates[1]["sessionUpdate"], "async_task_progress");
        assert_eq!(updates[1]["description"], "Deploying");
        assert_eq!(updates[1]["summary"], "step 1");
        assert_eq!(updates[2]["sessionUpdate"], "async_task_state_update");
        assert_eq!(updates[2]["state"], "completed");
        assert_eq!(updates[2]["summary"], "done");
    }

    #[test]
    fn inv_40_async_task_ignores_subagent_and_monitor() {
        let mut r = rt();
        let subagent = json!({"task_id":"s1","task_type":"local_agent","description":"child"});
        assert!(r.task_started(&subagent).is_empty());
        let monitor = json!({"task_id":"m1","task_type":"local_monitor"});
        assert!(r.task_started(&monitor).is_empty());
        // A monitor task never spawns even at terminal.
        let notif = json!({"task_id":"m1","status":"completed"});
        assert!(r.task_notification(&notif).is_empty());
    }

    #[test]
    fn inv_40_async_task_progress_after_announce_dedupes_repeated_value() {
        let mut r = rt();
        let started = json!({"task_id":"t1","task_type":"local_workflow","description":"Deploy"});
        r.task_started(&started);
        let progress = json!({"task_id":"t1","description":"Deploying","summary":"step 1"});
        r.task_progress(&progress);
        let notif = json!({"task_id":"t1","status":"completed","summary":"done"});
        let updates = r.task_notification(&notif);
        assert_eq!(updates.len(), 3);
        // A second progress with the same description after announce is deduped out.
        let progress2 = json!({"task_id":"t1","description":"Deploying"});
        // The task is terminal now; progress publishes only metadata changes.
        let more = r.task_progress(&progress2);
        assert!(more.is_empty());
    }

    #[test]
    fn inv_40_async_task_updated_running_mid_stream() {
        let mut r = rt();
        let started = json!({"task_id":"t1","task_type":"local_workflow","description":"Deploy"});
        r.task_started(&started);
        // A running update with no tool call id is held.
        let patch = json!({"status":"running","description":"Deploying"});
        assert!(r.task_updated("t1", &patch).is_empty());
    }

    fn announced(id: &str) -> AsyncTaskRuntime {
        let mut r = rt();
        let started = json!({"task_id":id,"task_type":"local_workflow","description":"Sleep","tool_use_id":"toolu_BG1"});
        let spawned = r.task_started(&started);
        assert_eq!(spawned[0]["sessionUpdate"], "async_task_spawned");
        r
    }

    /// rust-air-gaps 1.5 (Decision 12) — a stop is claimed once, released on
    /// failure, refused for unknown / unannounced / ignored / terminal tasks.
    #[test]
    fn inv_49_stop_claim_release() {
        let mut r = announced("bg1");
        assert!(r.can_stop("bg1"));
        assert!(r.claim_stop("bg1"), "first claim wins");
        assert!(
            !r.claim_stop("bg1"),
            "a second claim is refused while stopping"
        );
        r.release_stop("bg1");
        assert!(r.claim_stop("bg1"), "release gives the claim back");
        assert!(!r.claim_stop("nope"), "unknown task");

        // A held (unannounced) task cannot be stopped.
        let mut held = rt();
        held.task_started(&json!({"task_id":"h1","task_type":"local_workflow"}));
        assert!(!held.claim_stop("h1"));

        // Ignored subagent / monitor tasks cannot be stopped.
        let mut ignored = rt();
        ignored.task_started(&json!({"task_id":"s1","task_type":"local_agent"}));
        assert!(!ignored.claim_stop("s1"));

        // A terminal task keeps its flag on release and cannot be re-claimed.
        let mut done = announced("t9");
        assert!(done.claim_stop("t9"));
        done.task_notification(&json!({"task_id":"t9","status":"completed"}));
        done.release_stop("t9");
        assert!(!done.claim_stop("t9"));

        // A disabled (non-AIR) runtime never has a task to stop.
        let mut off = AsyncTaskRuntime::new(false);
        off.task_started(&json!({"task_id":"bg1","task_type":"local_workflow","tool_use_id":"x"}));
        assert!(!off.claim_stop("bg1"));
    }

    /// rust-air-gaps 1.5 — `task_stopped` finishes the task as `stopped` (no
    /// summary), names the notice once, and still answers when the SDK's own
    /// `task_notification` already made the task terminal.
    #[test]
    fn inv_49_task_stopped_emits_state_then_notice_once() {
        let mut r = announced("bg1");
        assert!(r.claim_stop("bg1"));
        let out = r.task_stopped("bg1");
        assert_eq!(out.notice_name.as_deref(), Some("Sleep"));
        assert_eq!(out.updates.len(), 1);
        assert_eq!(out.updates[0]["sessionUpdate"], "async_task_state_update");
        assert_eq!(out.updates[0]["state"], "stopped");
        assert!(
            out.updates[0].get("summary").is_none(),
            "no terminal summary"
        );
        // Once per task.
        assert_eq!(r.task_stopped("bg1"), StopOutcome::default());

        // Terminal already (notification won the race): the notice still goes,
        // the state update does not repeat.
        let mut raced = announced("bg2");
        assert!(raced.claim_stop("bg2"));
        raced.task_notification(&json!({"task_id":"bg2","status":"stopped"}));
        let out = raced.task_stopped("bg2");
        assert_eq!(out.notice_name.as_deref(), Some("Sleep"));
        assert!(out.updates.is_empty());

        // Unknown task: nothing.
        assert_eq!(r.task_stopped("zzz"), StopOutcome::default());
    }

    /// T2FZ-5d — the hidden tool calls (`Monitor`) are capped at 1000, oldest
    /// first, and a conversation reset (`clear`) forgets them.
    #[test]
    fn t2a_hidden_tool_calls_are_bounded_and_cleared() {
        let mut r = AsyncTaskRuntime::new(true);
        for index in 0..1005 {
            r.hide_tool_call(&format!("m{index}"));
        }
        r.hide_tool_call("m1004");
        assert_eq!(r.hidden_tool_calls.len(), 1000);
        assert!(!r.hidden_tool_calls.iter().any(|id| id == "m0"));
        assert!(r.hidden_tool_calls.iter().any(|id| id == "m1004"));
        r.clear();
        assert!(r.hidden_tool_calls.is_empty());
    }

    fn started(id: &str) -> Value {
        json!({"task_id": id, "task_type": "local_bash", "description": "build",
            "is_backgrounded": true, "tool_use_id": "bash-tool"})
    }

    /// U-75 — a field counts as published only once its send succeeded: after a
    /// failed send the rollback lets the retry carry the same fields again.
    #[test]
    fn t2a_rollback_after_a_failed_send_repeats_the_changed_fields() {
        let mut r = rt();
        r.task_started(&started("build"));
        let note = json!({"task_id":"build","status":"completed","summary":"Done",
            "output_file":"/tmp/build.output"});
        let first = r.task_notification(&note);
        assert_eq!(first[0]["state"], "completed");
        assert_eq!(first[0]["outputFilePath"], "/tmp/build.output");
        // The send failed: nothing reached the client.
        r.rollback();
        let retry = r.task_notification(&note);
        assert_eq!(retry.len(), 1, "the terminal state is published again");
        assert_eq!(retry[0]["state"], "completed");
        assert_eq!(retry[0]["outputFilePath"], "/tmp/build.output");
        // Without a rollback the same notification publishes nothing new.
        assert!(r.task_notification(&note).is_empty());
    }

    /// U-75 — progress fields are deduped against what the client holds and a
    /// rolled-back beat is sent again.
    #[test]
    fn t2a_progress_dedupes_and_rolls_back() {
        let mut r = rt();
        r.task_started(&started("build"));
        let beat = json!({"task_id":"build","summary":"step 1","last_tool_name":"Bash",
            "usage":{"total_tokens":10,"tool_uses":1,"duration_ms":100}});
        let first = r.task_progress(&beat);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0]["usage"],
            json!({"totalTokens":10,"toolUses":1,"durationMs":100})
        );
        assert!(r.task_progress(&beat).is_empty(), "nothing new");
        let again = json!({"task_id":"build","summary":"step 2",
            "usage":{"total_tokens":10,"tool_uses":1,"duration_ms":100}});
        let second = r.task_progress(&again);
        assert_eq!(second[0]["summary"], "step 2");
        assert!(second[0].get("usage").is_none(), "the usage did not change");
        r.rollback();
        assert_eq!(r.task_progress(&again)[0]["summary"], "step 2");
    }

    /// N-66 — usage needs all three numbers, integers stay integers; held beats
    /// follow the spawn once the tool call id arrives.
    #[test]
    fn t2a_progress_usage_validation_and_held_beats() {
        assert_eq!(
            task_usage(Some(&json!({"total_tokens": 1, "tool_uses": 2}))),
            None
        );
        assert_eq!(task_usage(Some(&json!([1, 2, 3]))), None);
        assert_eq!(
            task_usage(Some(
                &json!({"totalTokens": 1, "toolUses": 2, "durationMs": 3.5})
            )),
            Some(json!({"totalTokens": 1, "toolUses": 2, "durationMs": 3.5}))
        );

        let mut r = rt();
        // No tool call id yet: the spawn is held and its beats wait.
        r.task_started(
            &json!({"task_id":"wf","task_type":"local_workflow","description":"Deploy"}),
        );
        assert!(r
            .task_progress(&json!({"task_id":"wf","summary":"early"}))
            .is_empty());
        let released =
            r.task_progress(&json!({"task_id":"wf","summary":"later","tool_use_id":"tc"}));
        assert_eq!(released[0]["sessionUpdate"], "async_task_spawned");
        assert_eq!(released[0]["toolCallId"], "tc");
        assert_eq!(released[1]["summary"], "early");
        assert_eq!(released[2]["summary"], "later");
    }

    /// N-70 — the workflow name names the task; `skip_transcript` decides the
    /// transcript card.
    #[test]
    fn t2a_started_workflow_name_and_skip_transcript() {
        let mut r = rt();
        let out = r.task_started(&json!({"task_id":"wf","task_type":"local_workflow",
            "description":"Deploy","workflow_name":"deploy-prod","skip_transcript":true,
            "tool_use_id":"tc"}));
        assert_eq!(out[0]["name"], "deploy-prod");
        assert_eq!(out[0]["description"], "Deploy");
        assert_eq!(out[0]["showInTranscript"], false);
        assert_eq!(out[0]["taskType"], "workflow");
    }

    /// N-70 / N-43 — a level-only task is panel-only and a late `task_started`
    /// cannot create the transcript card; the level then closes a lost task as
    /// stopped and the SDK's own edge corrects it.
    #[test]
    fn t2a_level_recovery_is_panel_only_and_the_late_edge_corrects_it() {
        let mut r = rt();
        assert!(
            r.background_tasks_changed(
                &json!([{"task_id":"wf","task_type":"local_workflow","description":"Deploy"}])
            )
            .is_empty(),
            "held until the tool call id arrives"
        );
        let spawned = r.task_started(&json!({"task_id":"wf","task_type":"local_workflow",
            "description":"Deploy","skip_transcript":false,"tool_use_id":"tc"}));
        assert_eq!(
            spawned[0]["showInTranscript"], false,
            "the late task_started cannot create a card"
        );

        let closed = r.background_tasks_changed(&json!([]));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0]["state"], "stopped");
        // The level's best-effort state is corrected by the SDK's edge.
        let corrected =
            r.task_notification(&json!({"task_id":"wf","status":"completed","summary":"ok"}));
        assert_eq!(corrected.len(), 1);
        assert_eq!(corrected[0]["state"], "completed");
        assert_eq!(corrected[0]["summary"], "ok");
        // A second edge changes nothing.
        assert!(r
            .task_notification(&json!({"task_id":"wf","status":"failed"}))
            .is_empty());
    }

    /// N-43 — the level promotes a known foreground task, ignores agents and
    /// monitors, and never closes a task that is listed.
    #[test]
    fn t2a_level_signal_handles_known_ignored_and_listed_tasks() {
        let mut r = rt();
        r.task_started(&started("bash"));
        let level = json!([
            {"task_id": "bash", "task_type": "local_bash"},
            {"task_id": "agent", "task_type": "local_agent"},
            {"task_id": "mon", "task_type": "local_monitor"},
        ]);
        assert!(r.background_tasks_changed(&level).is_empty());
        let closed = r.background_tasks_changed(&json!([{"task_id": "other"}]));
        assert_eq!(closed.len(), 1, "only the announced bash task is closed");
        assert_eq!(closed[0]["asyncTaskId"], "bash");
        assert!(r.background_tasks_changed(&json!("not a list")).is_empty());
    }

    /// V-19 — `clear` drops every record: the same id is a fresh task afterwards.
    #[test]
    fn t2a_clear_drops_every_task_record() {
        let mut r = rt();
        r.task_started(&started("build"));
        r.clear();
        assert!(
            r.finish_all("failed").is_empty(),
            "nothing is live any more"
        );
        let respawn = r.task_started(&started("build"));
        assert_eq!(respawn[0]["sessionUpdate"], "async_task_spawned");
    }
}
