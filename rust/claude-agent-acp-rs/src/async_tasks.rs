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
    /// Active tasks keyed by id (holds terminal records too, per Node).
    tasks: HashMap<String, Task>,
}

#[derive(Debug)]
struct Task {
    id: String,
    name: String,
    task_type: String,
    description: String,
    show_in_transcript: bool,
    announced: bool,
    held: bool,
    /// Deferred progress payloads to replay once the task is announced.
    deferred: Vec<Value>,
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
}

impl Default for Task {
    fn default() -> Self {
        Task {
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
        }
    }

    /// Handle a `task_started` frame (Node `taskStarted`).
    pub fn task_started(&mut self, message: &Value) -> Vec<Value> {
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

    /// Handle a `task_progress` frame (Node `taskProgress`).
    pub fn task_progress(&mut self, message: &Value) -> Vec<Value> {
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
        let (previous_output, _) = self.snapshot_meta(&task_id);
        self.merge_output_file_path(&task_id, message);
        self.merge_tool_call_id(&task_id, message);
        out.extend(self.finish(&task_id, &state, non_blank_string(message.get("summary"))));
        let (output, _) = self.snapshot_meta(&task_id);
        if was_terminal && self.is_announced(&task_id) && output != previous_output {
            out.extend(self.publish_metadata(&task_id));
        }
        out
    }

    /// Handle a `task_updated` frame (Node `taskUpdated(task_id, patch)`).
    pub fn task_updated(&mut self, task_id: &str, patch: &Value) -> Vec<Value> {
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
        out.extend(self.finish(&task_id, &state, non_blank_string(patch.get("error"))));
        out
    }

    /// Release held tasks without a tool-call id (Node `releaseHeld`), called
    /// when the prompt result ends the model turn.
    pub fn release_held(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let held: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, t)| t.held)
            .map(|(id, _)| id.clone())
            .collect();
        for id in held {
            out.extend(self.announce(&id, true));
        }
        out
    }

    /// Whether a user stop may start for `id` (Node `canStop`): announced, not
    /// ignored, not already being stopped, not terminal.
    pub fn can_stop(&self, id: &str) -> bool {
        self.tasks
            .get(id)
            .is_some_and(|t| t.announced && !t.ignored && !t.stopping && !is_terminal(&t.state))
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
            updates: self.finish(id, "stopped", None),
            notice_name: Some(name),
        }
    }

    /// Finish every announced/held non-terminal task (Node `finishAll`), called
    /// at stream end / teardown.
    pub fn finish_all(&mut self, state: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, t)| (t.announced || t.held) && !t.ignored && !is_terminal(&t.state))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            // Node wraps each in a try/catch; a failure is surfaced at the end.
            out.extend(self.finish(&id, state, None));
        }
        out
    }

    fn task_mut(&mut self, id: &str) -> &mut Task {
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
        if let Some(v) = field(message, "taskType", "task_type") {
            let ft = friendly_task_type(v.as_str().unwrap_or("task")).to_string();
            if let Some(t) = self.tasks.get_mut(id) {
                t.task_type = ft;
            }
        }
        if let Some(skip) = field(message, "skipTranscript", "skip_transcript") {
            if let Some(t) = self.tasks.get_mut(id) {
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

    fn merge_tool_result_path(&mut self, id: &str) {
        // Structured tool-result output-path recovery is out of the corpus
        // subset; mergeToolResultPath needs the tool-result path map, which the
        // AIR corpus never provides (the task's own output_file carries it).
        let _ = id;
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
                t.deferred.push(progress);
            }
            Vec::new()
        } else {
            Vec::new()
        }
    }

    /// Very like `when_announced`, but for a state update (only meaningful once
    /// announced; a held task's state is not deferred in the corpus subset).
    fn when_announced_state(&mut self, id: &str, state: &str, error: Option<String>) -> Vec<Value> {
        let announced = self.is_announced(id);
        if announced {
            self.publish_state(id, state, error)
        } else {
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
        record_published(self.tasks.get_mut(id), &changed);
        vec![Value::Object(final_update)]
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
        record_published(self.tasks.get_mut(id), &changed);
        vec![Value::Object(update)]
    }

    /// Port of Node `announce(task, options)`.
    fn announce(&mut self, id: &str, without_tool_call: bool) -> Vec<Value> {
        let mut out = Vec::new();
        {
            let task = self.task_mut(id);
            if task.announced || task.ignored {
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
            record_published_pairs(
                task,
                &[
                    ("description", Some(task.description.clone())),
                    ("outputFilePath", task.output_file_path.clone()),
                    ("toolCallId", task.tool_call_id.clone()),
                ],
            );
            task.announced = true;
            task.held = false;
        }
        // Replay deferred progress payloads, then a terminal-state publish.
        let deferred = std::mem::take(&mut self.task_mut(id).deferred);
        for progress in deferred {
            out.extend(self.publish_progress(id, progress));
        }
        if is_terminal(&self.state_of(id)) {
            out.extend(self.publish_state(id, &self.state_of(id), self.terminal_summary(id)));
        }
        out
    }

    /// Port of Node `finish`. `source` is always "event"/"shutdown" in the
    /// corpus subset (no background-tasks "level" replace semantics).
    fn finish(&mut self, id: &str, state: &str, summary: Option<String>) -> Vec<Value> {
        let mut out = Vec::new();
        {
            let task = self.task_mut(id);
            if task.ignored {
                return out;
            }
            if is_terminal(&task.state) {
                return out;
            }
            task.state = state.to_string();
            task.terminal_summary = summary;
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
    let record = value?;
    let total = number_field(record, "totalTokens", "total_tokens");
    let tool_uses = number_field(record, "toolUses", "tool_uses");
    let duration = number_field(record, "durationMs", "duration_ms");
    if total.is_none() || tool_uses.is_none() || duration.is_none() {
        return None;
    }
    Some(json!({
        "totalTokens": total.unwrap_or_default(),
        "toolUses": tool_uses.unwrap_or_default(),
        "durationMs": duration.unwrap_or_default(),
    }))
}

fn number_field(value: &Value, camel: &str, snake: &str) -> Option<f64> {
    field(value, camel, snake).and_then(Value::as_f64)
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

fn record_published(task: Option<&mut Task>, changed: &Value) {
    if let (Some(task), Some(obj)) = (task, changed.as_object()) {
        for (key, value) in obj {
            if value.is_null() {
                continue;
            }
            if let Ok(s) = serde_json::to_string(value) {
                task.published.insert(key.clone(), s);
            }
        }
    }
}

fn record_published_pairs(task: &mut Task, pairs: &[(&str, Option<String>)]) {
    for (key, value) in pairs {
        if let Some(v) = value {
            if let Ok(s) = serde_json::to_string(v) {
                task.published.insert((*key).to_string(), s);
            }
        }
    }
}

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
}
