//! Hook registry and callback handling (P8, P9, Decision 12).
//!
//! Mirrors `dist/tools.js` (`registerHookCallback`, `completeHookCallback`,
//! `createPostToolUseHook`, `createTaskHook`) and `dist/diff.js`
//! (`toolUpdateFromDiffToolResponse`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, Diff, TextContent, ToolCallContent, ToolCallLocation,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::session_config::{ConfigOption, ModesState, MODE_CONFIG_ID};

pub const POST_TOOL_USE_HOOK_ID: &str = "hook_0";
pub const POST_MODEL_SWITCH_HOOK_ID: &str = "hook_1";
pub const POST_COMPACT_HOOK_ID: &str = "hook_2";
pub const TASK_CREATED_HOOK_ID: &str = "hook_3";
pub const TASK_COMPLETED_HOOK_ID: &str = "hook_4";

/// The payload sent in the `initialize` control_request's `hooks` field.
pub fn init_hooks_payload() -> Value {
    json!({
        "PostToolUse": [{"hookCallbackIds": [POST_TOOL_USE_HOOK_ID]}],
        "PostModelSwitch": [{"hookCallbackIds": [POST_MODEL_SWITCH_HOOK_ID]}],
        "PostCompact": [{"hookCallbackIds": [POST_COMPACT_HOOK_ID]}],
        "TaskCreated": [{"hookCallbackIds": [TASK_CREATED_HOOK_ID]}],
        "TaskCompleted": [{"hookCallbackIds": [TASK_COMPLETED_HOOK_ID]}],
    })
}

/// A successful control_response to a `hook_callback`.
pub fn hook_control_response_success(request_id: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "continue": true
            }
        }
    })
}

fn hook_error_response(request_id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error
        }
    })
}

/// The error control_response for a `hook_callback` whose callback id has no handler
/// (the SDK's own message: `No hook callback found for ID: <id>`).
pub fn unknown_hook_callback_response(request_id: &str, callback_id: &str) -> Value {
    hook_error_response(
        request_id,
        &format!("No hook callback found for ID: {callback_id}"),
    )
}

/// An error control_response when a hook callback cannot be processed.
pub fn hook_control_response_error(request_id: &str) -> Value {
    hook_error_response(request_id, "Hook callback failed")
}

/// Registered callback entry for an active tool use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEntry {
    pub tool_name: String,
    pub parent_tool_use_id: Option<String>,
    pub completed_at: Option<Instant>,
}

/// Actor-owned registry of pending hook callbacks.
#[derive(Debug, Default)]
pub struct HookRegistry {
    callbacks: HashMap<String, HookEntry>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook callback for `tool_use_id`, replacing any callback already
    /// registered for it (Node `registerHookCallback` unregisters first). Returns the
    /// replaced entry, which has left the registry and must be released by the caller
    /// (Node `onRelease`).
    pub fn register_once(
        &mut self,
        tool_use_id: String,
        tool_name: String,
        parent_tool_use_id: Option<String>,
    ) -> Option<HookEntry> {
        self.callbacks.insert(
            tool_use_id,
            HookEntry {
                tool_name,
                parent_tool_use_id,
                completed_at: None,
            },
        )
    }

    /// Whether a callback is registered for `tool_use_id`.
    pub fn has(&self, tool_use_id: &str) -> bool {
        self.callbacks.contains_key(tool_use_id)
    }

    /// Mark that the tool_result for `tool_use_id` has completed, arming the 30-second
    /// lazy retention cleanup window.
    pub fn complete(&mut self, tool_use_id: &str) {
        if let Some(entry) = self.callbacks.get_mut(tool_use_id) {
            if entry.completed_at.is_none() {
                entry.completed_at = Some(Instant::now());
            }
        }
    }

    /// Unregister and return the callback entry for `tool_use_id`.
    pub fn unregister(&mut self, tool_use_id: &str) -> Option<HookEntry> {
        self.callbacks.remove(tool_use_id)
    }

    /// Lazily prune callbacks whose completed_at is 30 seconds or older. Returns the
    /// released entries, which the caller must release (Node `onRelease`).
    pub fn prune_expired(&mut self, now: Instant) -> Vec<(String, HookEntry)> {
        let expired: Vec<String> = self
            .callbacks
            .iter()
            .filter(|(_, entry)| {
                entry.completed_at.is_some_and(|completed| {
                    now.duration_since(completed) >= Duration::from_secs(30)
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| self.callbacks.remove(&id).map(|entry| (id, entry)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.callbacks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.callbacks.is_empty()
    }
}

/// Builds ACP diff content and locations from the `tool_response` of an Edit or Write
/// (mirroring `dist/diff.js:toolUpdateFromDiffToolResponse`).
pub fn tool_update_from_diff_tool_response(
    tool_response: &Value,
) -> (Vec<ToolCallContent>, Vec<ToolCallLocation>) {
    let mut content = Vec::new();
    let mut locations = Vec::new();

    let file_path = tool_response.get("filePath").and_then(Value::as_str);
    if let (Some(path), Some(patches)) = (
        file_path,
        tool_response
            .get("structuredPatch")
            .and_then(Value::as_array),
    ) {
        for patch in patches {
            let new_start = patch
                .get("newStart")
                .and_then(Value::as_u64)
                .map(|n| n as u32);
            if let Some(lines) = patch.get("lines").and_then(Value::as_array) {
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                for line_val in lines {
                    if let Some(line) = line_val.as_str() {
                        if let Some(rest) = line.strip_prefix('-') {
                            old_lines.push(rest);
                        } else if let Some(rest) = line.strip_prefix('+') {
                            new_lines.push(rest);
                        } else if line == "\\ No newline at end of file" {
                            continue;
                        } else if let Some(rest) = line.strip_prefix(' ') {
                            old_lines.push(rest);
                            new_lines.push(rest);
                        } else {
                            // Drop the first character, not the first byte (Node slices one unit).
                            let rest = line.chars().next().map_or("", |c| &line[c.len_utf8()..]);
                            old_lines.push(rest);
                            new_lines.push(rest);
                        }
                    }
                }
                if !old_lines.is_empty() || !new_lines.is_empty() {
                    let mut loc = ToolCallLocation::new(path);
                    if let Some(start) = new_start {
                        loc = loc.line(start);
                    }
                    locations.push(loc);

                    let old_text = if old_lines.is_empty() {
                        None
                    } else {
                        Some(old_lines.join("\n"))
                    };
                    let new_text = new_lines.join("\n");
                    let mut diff = Diff::new(path, new_text);
                    if let Some(old) = old_text {
                        diff = diff.old_text(old);
                    }
                    content.push(ToolCallContent::Diff(diff));
                }
            }
        }
    }

    if content.is_empty()
        && tool_response.get("type").and_then(Value::as_str) == Some("update")
        && tool_response
            .get("content")
            .and_then(Value::as_str)
            .is_some()
    {
        if let Some(path) = file_path {
            locations.push(ToolCallLocation::new(path));
            let new_content = tool_response["content"].as_str().unwrap_or("");
            if let Some(orig) = tool_response.get("originalFile").and_then(Value::as_str) {
                content.push(ToolCallContent::Diff(
                    Diff::new(path, new_content).old_text(orig),
                ));
            } else {
                content.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
                    TextContent::new(format!(
                        "Updated `{}` (previous content too large to diff)",
                        path
                    )),
                ))));
            }
        }
    }

    (content, locations)
}

/// Builds the `tool_call_update` payload for a `PostToolUse` event.
pub fn build_post_tool_use_update(
    tool_use_id: &str,
    tool_name: &str,
    parent_tool_use_id: Option<&str>,
    tool_response: &Value,
) -> Value {
    let mut claude_code = json!({
        "toolName": tool_name,
        "toolResponse": tool_response,
    });
    if let Some(parent) = parent_tool_use_id {
        claude_code["parentToolUseId"] = json!(parent);
    }
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_use_id,
        "_meta": {
            "claudeCode": claude_code,
        }
    });

    if tool_name == "Edit" || tool_name == "Write" {
        let (content, locations) = tool_update_from_diff_tool_response(tool_response);
        if !content.is_empty() {
            update["content"] = serde_json::to_value(&content).unwrap_or(Value::Null);
        }
        if !locations.is_empty() {
            update["locations"] = serde_json::to_value(&locations).unwrap_or(Value::Null);
        }
    }

    update
}

/// An entry in the session's task state snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStateEntry {
    pub subject: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Parses the created task ID from TaskCreate tool output (Node `parseTaskCreateOutput`):
/// a structured `{"task":{"id":"…"}}` carried by the object, its JSON-string form, or a
/// text block, else the human-readable `Task #<id> created successfully: <subject>` form.
pub fn parse_task_create_output(content: &Value) -> Option<String> {
    let structured = json_candidates(content).into_iter().find_map(|value| {
        value
            .pointer("/task/id")
            .and_then(Value::as_str)
            .map(String::from)
    });
    if structured.is_some() {
        return structured;
    }
    tool_output_texts(content).into_iter().find_map(|text| {
        let rest = text.trim().strip_prefix("Task #")?;
        let idx = rest.find(" created successfully: ")?;
        let id = &rest[..idx];
        let subject = &rest[idx + " created successfully: ".len()..];
        // Node's regex needs a non-empty, whitespace-free id (`\S+`) and a
        // non-empty single-line subject (`(.+)`).
        (!id.is_empty()
            && !id.contains(char::is_whitespace)
            && !subject.is_empty()
            && !subject.contains(['\n', '\r']))
        .then(|| id.to_string())
    })
}

/// Applies a TaskCreate tool call to the task state. The id comes only from the
/// output (Node `applyTaskCreate` never falls back to `input.taskId`), and an
/// empty id is rejected like Node's `if (!taskId)`.
pub fn apply_task_create(
    state: &mut Vec<(String, TaskStateEntry)>,
    input: &Value,
    task_id: Option<&str>,
) {
    let Some(id) = task_id.filter(|id| !id.is_empty()) else {
        return;
    };
    let subject = input.get("subject").and_then(Value::as_str).unwrap_or("");
    let active_form = input
        .get("activeForm")
        .and_then(Value::as_str)
        .map(String::from);
    let description = input
        .get("description")
        .and_then(Value::as_str)
        .map(String::from);

    let entry = TaskStateEntry {
        subject: subject.to_string(),
        status: "pending".to_string(),
        active_form,
        description,
    };
    if let Some((_, existing)) = state.iter_mut().find(|(k, _)| k == id) {
        *existing = entry;
    } else {
        state.push((id.to_string(), entry));
    }
}

/// The structured result of a TaskUpdate call (Node `TaskUpdateOutput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskUpdateOutput {
    pub success: bool,
    pub task_id: String,
    pub updated_fields: Vec<String>,
    pub error: Option<String>,
}

/// The text blocks of a tool result (Node `toolOutputTexts`).
fn tool_output_texts(content: &Value) -> Vec<&str> {
    if let Some(s) = content.as_str() {
        return vec![s];
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect()
}

/// Candidate JSON objects in a tool result (Node `parseJsonToolOutput` before its validator):
/// an object as-is, a JSON string, or the JSON carried by each text block.
fn json_candidates(content: &Value) -> Vec<Value> {
    match content {
        Value::Object(_) => vec![content.clone()],
        Value::String(s) => serde_json::from_str(s).ok().into_iter().collect(),
        Value::Array(_) => tool_output_texts(content)
            .into_iter()
            .filter_map(|t| serde_json::from_str(t).ok())
            .collect(),
        _ => Vec::new(),
    }
}

fn structured_task_update(value: &Value) -> Option<TaskUpdateOutput> {
    Some(TaskUpdateOutput {
        success: value.get("success")?.as_bool()?,
        task_id: value.get("taskId")?.as_str()?.to_string(),
        updated_fields: value
            .get("updatedFields")?
            .as_array()?
            .iter()
            .map(|f| f.as_str().map(String::from))
            .collect::<Option<Vec<_>>>()?,
        error: value.get("error").and_then(Value::as_str).map(String::from),
    })
}

/// Parses one TaskUpdate text result: a "not found" or failed delete is an unsuccessful update.
fn task_update_text(text: &str, expected_task_id: Option<&str>) -> Option<TaskUpdateOutput> {
    let text = text.trim();
    let not_found = text
        .strip_prefix("Task #")
        .and_then(|rest| rest.strip_suffix(" not found"))
        .filter(|id| !id.is_empty() && !id.contains(char::is_whitespace));
    let task_id = not_found.or(expected_task_id)?;
    if not_found.is_some() || text == "Failed to delete task" {
        return Some(TaskUpdateOutput {
            success: false,
            task_id: task_id.to_string(),
            updated_fields: Vec::new(),
            error: Some(text.to_string()),
        });
    }
    None
}

/// Parses TaskUpdate output from one tool result (Node `parseTaskUpdateOutput`).
fn parse_task_update_one(
    content: &Value,
    expected_task_id: Option<&str>,
) -> Option<TaskUpdateOutput> {
    json_candidates(content)
        .iter()
        .find_map(structured_task_update)
        .or_else(|| {
            tool_output_texts(content)
                .into_iter()
                .find_map(|text| task_update_text(text, expected_task_id))
        })
}

/// Parses TaskUpdate output, preferring the SDK's message-level `tool_use_result` over the
/// tool_result `content` (Node `parseTaskUpdateOutput(toolUseResult) ?? parseTaskUpdateOutput(content)`).
pub fn parse_task_update_output(
    tool_use_result: &Value,
    content: &Value,
    expected_task_id: Option<&str>,
) -> Option<TaskUpdateOutput> {
    parse_task_update_one(tool_use_result, expected_task_id)
        .or_else(|| parse_task_update_one(content, expected_task_id))
}

/// Applies a TaskUpdate tool result (Node acp-agent TaskUpdate branch). The update is applied
/// unless its output reports a failure or names another task; with no output it falls back to
/// the input. Returns true when the update was applied, i.e. when the plan should be published.
pub fn apply_task_update_output(
    state: &mut Vec<(String, TaskStateEntry)>,
    input: &Value,
    tool_use_result: &Value,
    content: &Value,
) -> bool {
    let task_id = input.get("taskId").and_then(Value::as_str);
    if let Some(output) = parse_task_update_output(tool_use_result, content, task_id) {
        if !(output.success && Some(output.task_id.as_str()) == task_id) {
            return false;
        }
    }
    apply_task_update(state, input);
    true
}

/// Applies a TaskUpdate tool call to the task state. A missing `status` keeps the existing one
/// (Node `input.status ?? existing?.status ?? "pending"`).
pub fn apply_task_update(state: &mut Vec<(String, TaskStateEntry)>, input: &Value) {
    let Some(task_id) = input.get("taskId").and_then(Value::as_str) else {
        return;
    };
    if input.get("status").and_then(Value::as_str) == Some("deleted") {
        state.retain(|(k, _)| k != task_id);
        return;
    }
    let existing_entry = state
        .iter()
        .find(|(k, _)| k == task_id)
        .map(|(_, v)| v.clone());
    let status = input
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| existing_entry.as_ref().map(|e| e.status.as_str()))
        .unwrap_or("pending")
        .to_string();
    let subject = input
        .get("subject")
        .and_then(Value::as_str)
        .or_else(|| existing_entry.as_ref().map(|e| e.subject.as_str()));
    let Some(subj) = subject else { return };

    let active_form = input
        .get("activeForm")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| existing_entry.as_ref().and_then(|e| e.active_form.clone()));
    let description = input
        .get("description")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| existing_entry.as_ref().and_then(|e| e.description.clone()));

    let entry = TaskStateEntry {
        subject: subj.to_string(),
        status: status.to_string(),
        active_form,
        description,
    };
    if let Some((_, ex)) = state.iter_mut().find(|(k, _)| k == task_id) {
        *ex = entry;
    } else {
        state.push((task_id.to_string(), entry));
    }
}

const TASK_STATUSES: [&str; 3] = ["pending", "in_progress", "completed"];

/// A TaskList entry: (id, subject, status).
type TaskListEntry = (String, String, String);

fn structured_task_list(value: &Value) -> Option<Vec<TaskListEntry>> {
    value
        .get("tasks")?
        .as_array()?
        .iter()
        .map(|t| {
            let id = t.get("id")?.as_str()?;
            let subject = t.get("subject")?.as_str()?;
            let status = t.get("status")?.as_str()?;
            TASK_STATUSES
                .contains(&status)
                .then(|| (id.to_string(), subject.to_string(), status.to_string()))
        })
        .collect()
}

/// Parses one TaskList text line `#<id> [<status>] <subject>` (Node `parseTaskListOutput`),
/// stripping the trailing ` [blocked by #a, #b]` and ` (owner)` suffixes from the subject.
fn parse_task_line(line: &str) -> Option<TaskListEntry> {
    let rest = line.strip_prefix('#')?;
    let (id, after_id) = rest.split_once(' ')?;
    if id.is_empty() || id.contains(char::is_whitespace) {
        return None;
    }
    let after_bracket = after_id.strip_prefix('[')?;
    let status = TASK_STATUSES.iter().find(|s| {
        after_bracket
            .strip_prefix(**s)
            .is_some_and(|tail| tail.starts_with("] "))
    })?;
    let subject = after_bracket.get(status.len() + 2..)?;
    if subject.is_empty() {
        return None;
    }
    let mut subject = subject.to_string();

    const BLOCKED_MARKER: &str = " [blocked by ";
    if let Some(start) = subject.rfind(BLOCKED_MARKER).filter(|&i| i > 0) {
        if subject.ends_with(']') {
            let deps = &subject[start + BLOCKED_MARKER.len()..subject.len() - 1];
            let valid = deps
                .split(", ")
                .all(|d| d.len() > 1 && d.starts_with('#') && !d.contains(',') && !d.contains(']'));
            if valid {
                subject.truncate(start);
            }
        }
    }

    if let Some(start) = subject.rfind(" (").filter(|&i| i > 0) {
        if subject.ends_with(')') {
            let owner = &subject[start + 2..subject.len() - 1];
            if !owner.contains('(') && !owner.contains(')') {
                subject.truncate(start);
            }
        }
    }

    Some((id.to_string(), subject, (*status).to_string()))
}

fn text_task_list(text: &str) -> Option<Vec<TaskListEntry>> {
    if text.trim() == "No tasks found" {
        return Some(Vec::new());
    }
    let mut tasks = Vec::new();
    for line in text.trim().split('\n') {
        match parse_task_line(line) {
            Some(entry) => tasks.push(entry),
            // One unmatched line voids the whole text (Node clears and stops).
            None => {
                tasks.clear();
                break;
            }
        }
    }
    (!tasks.is_empty()).then_some(tasks)
}

fn parse_task_list_one(content: &Value) -> Option<Vec<TaskListEntry>> {
    json_candidates(content)
        .iter()
        .find_map(structured_task_list)
        .or_else(|| {
            tool_output_texts(content)
                .into_iter()
                .find_map(text_task_list)
        })
}

/// Parses TaskList output strictly (Node `parseTaskListOutput`), preferring the message-level
/// `tool_use_result` over the tool_result `content`. Returns None when neither is a task list,
/// so the caller must keep the current state.
pub fn parse_task_list(tool_use_result: &Value, content: &Value) -> Option<Vec<TaskListEntry>> {
    parse_task_list_one(tool_use_result).or_else(|| parse_task_list_one(content))
}

/// Parses task list entries from TaskList tool output. Unparseable output yields no entries;
/// use `parse_task_list` to tell that apart from an empty list.
pub fn parse_task_list_output(content: &Value) -> Vec<TaskListEntry> {
    parse_task_list(&Value::Null, content).unwrap_or_default()
}

/// Reconciles task state from a TaskList tool result. Unparseable output leaves the state
/// unchanged. Returns true when the state was reconciled.
pub fn apply_task_list_output(
    state: &mut Vec<(String, TaskStateEntry)>,
    tool_use_result: &Value,
    content: &Value,
) -> bool {
    match parse_task_list(tool_use_result, content) {
        Some(tasks) => {
            apply_task_list(state, tasks);
            true
        }
        None => false,
    }
}

/// Reconciles task state from a TaskList tool output.
pub fn apply_task_list(
    state: &mut Vec<(String, TaskStateEntry)>,
    tasks: Vec<(String, String, String)>,
) {
    let previous: HashMap<String, TaskStateEntry> = state.drain(..).collect();
    for (id, subject, status) in tasks {
        let existing = previous.get(&id);
        state.push((
            id,
            TaskStateEntry {
                subject,
                status,
                active_form: existing.and_then(|e| e.active_form.clone()),
                description: existing.and_then(|e| e.description.clone()),
            },
        ));
    }
}

/// Applies one Task* tool result to the task state, mirroring Node's TaskCreate /
/// TaskUpdate / TaskList branches (acp-agent.ts:13237-13290). Returns whether a plan
/// should be published. TaskGet is read-only: it neither mutates the state nor
/// publishes a plan, so any other tool name returns false.
pub fn apply_task_tool_result(
    state: &mut Vec<(String, TaskStateEntry)>,
    name: &str,
    input: &Value,
    tool_use_result: &Value,
    content: &Value,
) -> bool {
    match name {
        "TaskCreate" => {
            // Node parses the message-level `tool_use_result` first, then the
            // content, and publishes a plan even when the output has no id.
            let task_id = parse_task_create_output(tool_use_result)
                .or_else(|| parse_task_create_output(content));
            apply_task_create(state, input, task_id.as_deref());
            true
        }
        "TaskUpdate" => apply_task_update_output(state, input, tool_use_result, content),
        "TaskList" => apply_task_list_output(state, tool_use_result, content),
        _ => false,
    }
}

/// Formats the current task state as ACP plan entries (`dist/tools.js:taskStateToPlanEntries`).
pub fn task_state_to_plan_entries(state: &[(String, TaskStateEntry)]) -> Vec<Value> {
    state
        .iter()
        .map(|(_, task)| {
            let content = if task.status == "in_progress" && task.active_form.is_some() {
                task.active_form.as_deref().unwrap_or(&task.subject)
            } else {
                &task.subject
            };
            json!({
                "content": content,
                "status": task.status,
                "priority": "medium"
            })
        })
        .collect()
}

pub const ENTER_PLAN_MODE_TOOL: &str = "EnterPlanMode";
pub const PLAN_MODE_ID: &str = "plan";

/// Handles a PostToolUse hook for EnterPlanMode (Node `createPostToolUseHook`'s
/// `onEnterPlanMode`): moves the session to plan mode in place and returns the
/// `current_mode_update` and `config_option_update` payloads to publish, in that order.
/// Returns an empty list for any other hook.
pub fn enter_plan_mode_updates(
    input: &Value,
    session_id: &str,
    modes: &mut ModesState,
    config_options: &mut [ConfigOption],
) -> Vec<Value> {
    if input.get("hook_event_name").and_then(Value::as_str) != Some("PostToolUse")
        || input.get("tool_name").and_then(Value::as_str) != Some(ENTER_PLAN_MODE_TOOL)
    {
        return Vec::new();
    }
    modes.current_mode_id = PLAN_MODE_ID.to_string();
    if let Some(opt) = config_options.iter_mut().find(|o| o.id == MODE_CONFIG_ID) {
        opt.current_value = PLAN_MODE_ID.to_string();
    }
    vec![
        json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "current_mode_update",
                "currentModeId": PLAN_MODE_ID
            }
        }),
        json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "config_option_update",
                "configOptions": serde_json::to_value(&*config_options).unwrap_or(Value::Null)
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_64_registry_register_once() {
        let mut reg = HookRegistry::new();
        assert!(reg
            .register_once("toolu_1".into(), "Bash".into(), None)
            .is_none());
        // Re-registering the same ID replaces the entry and returns the one it displaced
        let replaced = reg
            .register_once("toolu_1".into(), "Edit".into(), Some("p1".into()))
            .expect("replaced entry");
        assert_eq!(replaced.tool_name, "Bash");
        assert_eq!(reg.len(), 1);
        let entry = reg.unregister("toolu_1").expect("present");
        assert_eq!(entry.tool_name, "Edit");
        assert_eq!(entry.parent_tool_use_id.as_deref(), Some("p1"));
    }

    #[test]
    fn pc1_2_prune_expired_returns_released_entries() {
        let mut reg = HookRegistry::new();
        reg.register_once("toolu_1".into(), "Bash".into(), None);
        reg.register_once("toolu_2".into(), "Read".into(), None);
        reg.complete("toolu_1");
        let later = Instant::now() + Duration::from_secs(60);
        let released = reg.prune_expired(later);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].0, "toolu_1");
        assert_eq!(released[0].1.tool_name, "Bash");
        assert!(reg.has("toolu_2"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn inv_64_retention_30s_lazy() {
        let mut reg = HookRegistry::new();
        reg.register_once("toolu_1".into(), "Bash".into(), None);
        let start = Instant::now();

        // Before completion, not expired
        reg.prune_expired(start + Duration::from_secs(60));
        assert!(reg.has("toolu_1"));

        // Complete arms the timer
        reg.complete("toolu_1");

        // After 29 seconds, still retained
        reg.prune_expired(start + Duration::from_secs(29));
        assert!(reg.has("toolu_1"));

        // After 31 seconds, expired and pruned
        reg.prune_expired(start + Duration::from_secs(31));
        assert!(!reg.has("toolu_1"));
    }

    #[test]
    fn pc1_2_register_once_replaces_existing_entry() {
        let mut reg = HookRegistry::new();
        reg.register_once("toolu_1".into(), "Bash".into(), None);
        reg.register_once("toolu_1".into(), "Edit".into(), Some("p1".into()));
        let entry = reg.unregister("toolu_1").expect("present");
        assert_eq!(entry.tool_name, "Edit");
        assert_eq!(entry.parent_tool_use_id.as_deref(), Some("p1"));
    }

    #[test]
    fn pc1_4_diff_line_with_multibyte_first_char_does_not_panic() {
        let resp = json!({
            "filePath": "/src/x.rs",
            "structuredPatch": [{"newStart": 1, "lines": ["é tail"]}]
        });
        let (content, _) = tool_update_from_diff_tool_response(&resp);
        assert_eq!(
            content[0],
            ToolCallContent::Diff(Diff::new("/src/x.rs", " tail").old_text(" tail"))
        );
    }

    #[test]
    fn pc1_5_task_update_without_status_keeps_progress() {
        let mut state = vec![(
            "1".to_string(),
            TaskStateEntry {
                subject: "Build".into(),
                status: "in_progress".into(),
                active_form: None,
                description: None,
            },
        )];
        apply_task_update(
            &mut state,
            &json!({"taskId": "1", "description": "new details"}),
        );
        assert_eq!(state[0].1.status, "in_progress");
        assert_eq!(state[0].1.description.as_deref(), Some("new details"));
    }

    #[test]
    fn inv_64_unknown_callback_error() {
        let resp = unknown_hook_callback_response("req_unknown", "hook_9");
        assert_eq!(
            resp,
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": "req_unknown",
                    "error": "No hook callback found for ID: hook_9"
                }
            })
        );
    }

    #[test]
    fn pc1_4_hook_error_reply_carries_error_string() {
        let resp = hook_control_response_error("req_1");
        assert_eq!(resp["response"]["subtype"], "error");
        assert_eq!(resp["response"]["request_id"], "req_1");
        assert!(resp["response"]["error"]
            .as_str()
            .is_some_and(|e| !e.is_empty()));
    }

    #[test]
    fn inv_64_post_tool_use_update_shape() {
        let tool_response = json!({
            "stdout": "hello",
            "stderr": "",
            "exitCode": 0
        });
        let update = build_post_tool_use_update(
            "toolu_bash1",
            "Bash",
            Some("parent_task_1"),
            &tool_response,
        );

        assert_eq!(update["sessionUpdate"], "tool_call_update");
        assert_eq!(update["toolCallId"], "toolu_bash1");
        assert_eq!(update["_meta"]["claudeCode"]["toolName"], "Bash");
        assert_eq!(
            update["_meta"]["claudeCode"]["parentToolUseId"],
            "parent_task_1"
        );
        assert_eq!(update["_meta"]["claudeCode"]["toolResponse"], tool_response);
    }

    #[test]
    fn inv_64_edit_write_final_diff() {
        // Structured patch with diff
        let resp = json!({
            "filePath": "/src/main.rs",
            "structuredPatch": [{
                "newStart": 10,
                "lines": [
                    " context line",
                    "-old line",
                    "+new line 1",
                    "+new line 2",
                    "\\ No newline at end of file"
                ]
            }]
        });

        let (content, locs) = tool_update_from_diff_tool_response(&resp);
        assert_eq!(content.len(), 1);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path.to_str().unwrap(), "/src/main.rs");
        assert_eq!(locs[0].line, Some(10));

        let update = build_post_tool_use_update("toolu_edit1", "Edit", None, &resp);
        assert_eq!(update["locations"][0]["path"], "/src/main.rs");
        assert_eq!(update["locations"][0]["line"], 10);
        assert_eq!(update["content"][0]["type"], "diff");
        assert_eq!(update["content"][0]["path"], "/src/main.rs");
        assert_eq!(update["content"][0]["oldText"], "context line\nold line");
        assert_eq!(
            update["content"][0]["newText"],
            "context line\nnew line 1\nnew line 2"
        );

        // Write update with empty structuredPatch but originalFile
        let write_large = json!({
            "type": "update",
            "filePath": "/src/large.rs",
            "structuredPatch": [],
            "originalFile": "old file content",
            "content": "new file content"
        });
        let (content_w, locs_w) = tool_update_from_diff_tool_response(&write_large);
        assert_eq!(content_w.len(), 1);
        assert_eq!(locs_w.len(), 1);
        assert_eq!(locs_w[0].path.to_str().unwrap(), "/src/large.rs");
        assert_eq!(
            content_w[0],
            ToolCallContent::Diff(
                Diff::new("/src/large.rs", "new file content").old_text("old file content")
            )
        );

        // Write update without originalFile (too large to diff)
        let write_too_large = json!({
            "type": "update",
            "filePath": "/src/huge.rs",
            "structuredPatch": [],
            "content": "huge content"
        });
        let (content_h, locs_h) = tool_update_from_diff_tool_response(&write_too_large);
        assert_eq!(content_h.len(), 1);
        assert_eq!(locs_h.len(), 1);
        assert_eq!(locs_h[0].path.to_str().unwrap(), "/src/huge.rs");
        assert_eq!(
            content_h[0],
            ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
                "Updated `/src/huge.rs` (previous content too large to diff)"
            ))))
        );
    }

    fn mode_option(current: &str) -> ConfigOption {
        ConfigOption {
            id: MODE_CONFIG_ID.into(),
            name: "Mode".into(),
            description: String::new(),
            category: "mode".into(),
            option_type: "select".into(),
            current_value: current.into(),
            options: Vec::new(),
            _meta: None,
        }
    }

    #[test]
    fn inv_64_enter_plan_mode_publishes_mode() {
        let mut modes = ModesState {
            current_mode_id: "default".into(),
            available_modes: Vec::new(),
        };
        let mut options = vec![mode_option("default")];
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "EnterPlanMode",
            "tool_input": {},
            "tool_response": {"status": "ok"}
        });

        let updates = enter_plan_mode_updates(&input, "sess_1", &mut modes, &mut options);

        assert_eq!(modes.current_mode_id, "plan");
        assert_eq!(options[0].current_value, "plan");
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0]["sessionId"], "sess_1");
        assert_eq!(updates[0]["update"]["sessionUpdate"], "current_mode_update");
        assert_eq!(updates[0]["update"]["currentModeId"], "plan");
        assert_eq!(
            updates[1]["update"]["sessionUpdate"],
            "config_option_update"
        );
        assert_eq!(
            updates[1]["update"]["configOptions"][0]["id"],
            MODE_CONFIG_ID
        );
        assert_eq!(
            updates[1]["update"]["configOptions"][0]["currentValue"],
            "plan"
        );
    }

    #[test]
    fn pc1_3_other_post_tool_use_hooks_publish_nothing() {
        let mut modes = ModesState {
            current_mode_id: "default".into(),
            available_modes: Vec::new(),
        };
        let mut options = vec![mode_option("default")];
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {},
            "tool_response": {"stdout": ""}
        });

        let updates = enter_plan_mode_updates(&input, "sess_1", &mut modes, &mut options);

        assert!(updates.is_empty());
        assert_eq!(modes.current_mode_id, "default");
        assert_eq!(options[0].current_value, "default");
    }

    #[test]
    fn inv_66_task_plan_create_and_update() {
        let mut state = Vec::new();
        let input = json!({
            "subject": "Initial task",
            "description": "Details"
        });
        apply_task_create(&mut state, &input, Some("1"));
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].0, "1");
        assert_eq!(state[0].1.subject, "Initial task");
        assert_eq!(state[0].1.status, "pending");

        // Update task status to in_progress
        let update_input = json!({
            "taskId": "1",
            "status": "in_progress",
            "activeForm": "Working on initial task"
        });
        apply_task_update(&mut state, &update_input);
        assert_eq!(state[0].1.status, "in_progress");
        assert_eq!(
            state[0].1.active_form.as_deref(),
            Some("Working on initial task")
        );

        // Delete task
        let delete_input = json!({
            "taskId": "1",
            "status": "deleted"
        });
        apply_task_update(&mut state, &delete_input);
        assert!(state.is_empty());
    }

    #[test]
    fn inv_66_task_plan_list_reconciliation() {
        let mut state = vec![(
            "1".to_string(),
            TaskStateEntry {
                subject: "Old 1".into(),
                status: "pending".into(),
                active_form: Some("Active 1".into()),
                description: None,
            },
        )];

        let parsed = parse_task_list_output(&json!({
            "tasks": [
                {"id": "1", "subject": "New 1", "status": "completed"},
                {"id": "2", "subject": "Task 2", "status": "pending"}
            ]
        }));
        apply_task_list(&mut state, parsed);
        assert_eq!(state.len(), 2);
        assert_eq!(state[0].0, "1");
        assert_eq!(state[0].1.status, "completed");
        assert_eq!(state[0].1.active_form.as_deref(), Some("Active 1"));
        assert_eq!(state[1].0, "2");
        assert_eq!(state[1].1.status, "pending");
    }

    #[test]
    fn inv_66_task_plan_entries_format() {
        let state = vec![
            (
                "1".to_string(),
                TaskStateEntry {
                    subject: "Task 1".into(),
                    status: "in_progress".into(),
                    active_form: Some("In progress form".into()),
                    description: None,
                },
            ),
            (
                "2".to_string(),
                TaskStateEntry {
                    subject: "Task 2".into(),
                    status: "completed".into(),
                    active_form: None,
                    description: None,
                },
            ),
        ];

        let entries = task_state_to_plan_entries(&state);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["content"], "In progress form");
        assert_eq!(entries[0]["status"], "in_progress");
        assert_eq!(entries[0]["priority"], "medium");
        assert_eq!(entries[1]["content"], "Task 2");
        assert_eq!(entries[1]["status"], "completed");
    }

    fn one_task(status: &str) -> Vec<(String, TaskStateEntry)> {
        vec![(
            "1".to_string(),
            TaskStateEntry {
                subject: "Build".into(),
                status: status.into(),
                active_form: None,
                description: None,
            },
        )]
    }

    #[test]
    fn pc1_5_failed_task_update_output_is_not_applied() {
        let mut state = one_task("pending");
        let input = json!({"taskId": "1", "status": "completed"});
        let tool_use_result =
            json!({"success": false, "taskId": "1", "updatedFields": [], "error": "nope"});
        assert!(!apply_task_update_output(
            &mut state,
            &input,
            &tool_use_result,
            &Value::Null
        ));
        assert_eq!(state[0].1.status, "pending");
    }

    #[test]
    fn pc1_5_task_update_output_for_another_task_is_not_applied() {
        let mut state = one_task("pending");
        let input = json!({"taskId": "1", "status": "completed"});
        let tool_use_result = json!({"success": true, "taskId": "2", "updatedFields": ["status"]});
        assert!(!apply_task_update_output(
            &mut state,
            &input,
            &tool_use_result,
            &Value::Null
        ));
        assert_eq!(state[0].1.status, "pending");
    }

    #[test]
    fn pc1_5_not_found_text_output_is_not_applied() {
        let mut state = one_task("pending");
        let input = json!({"taskId": "1", "status": "completed"});
        let content = json!("Task #1 not found");
        assert!(!apply_task_update_output(
            &mut state,
            &input,
            &Value::Null,
            &content
        ));
        assert_eq!(state[0].1.status, "pending");
    }

    #[test]
    fn pc1_5_confirmed_task_update_output_is_applied() {
        let mut state = one_task("pending");
        let input = json!({"taskId": "1", "status": "completed"});
        let tool_use_result = json!({"success": true, "taskId": "1", "updatedFields": ["status"]});
        assert!(apply_task_update_output(
            &mut state,
            &input,
            &tool_use_result,
            &Value::Null
        ));
        assert_eq!(state[0].1.status, "completed");
    }

    #[test]
    fn pc1_5_task_update_without_output_falls_back_to_input() {
        let mut state = one_task("pending");
        let input = json!({"taskId": "1", "status": "in_progress"});
        assert!(apply_task_update_output(
            &mut state,
            &input,
            &Value::Null,
            &Value::Null
        ));
        assert_eq!(state[0].1.status, "in_progress");
    }

    #[test]
    fn pc1_5_task_list_unparseable_output_keeps_state() {
        let mut state = one_task("in_progress");
        assert!(!apply_task_list_output(
            &mut state,
            &Value::Null,
            &json!("garbage")
        ));
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].1.status, "in_progress");
    }

    #[test]
    fn pc1_5_task_list_no_tasks_clears_state() {
        let mut state = one_task("in_progress");
        assert!(apply_task_list_output(
            &mut state,
            &Value::Null,
            &json!("No tasks found")
        ));
        assert!(state.is_empty());
    }

    #[test]
    fn pc1_5_task_list_strips_owner_and_blocked_by_suffixes() {
        let mut state = Vec::new();
        let content =
            json!("#1 [pending] Write docs (alice) [blocked by #2, #3]\n#2 [completed] Plan");
        assert!(apply_task_list_output(&mut state, &Value::Null, &content));
        assert_eq!(state[0].0, "1");
        assert_eq!(state[0].1.subject, "Write docs");
        assert_eq!(state[1].1.subject, "Plan");
    }

    #[test]
    fn pc1_5_task_list_one_malformed_line_voids_text() {
        let mut state = one_task("pending");
        let content = json!("#1 [pending] A\nnot a task line");
        assert!(!apply_task_list_output(&mut state, &Value::Null, &content));
        assert_eq!(state[0].1.subject, "Build");
    }

    #[test]
    fn pc1_5_task_list_rejects_unknown_status() {
        let mut state = one_task("pending");
        let content = json!({"tasks": [{"id": "1", "subject": "A", "status": "deleted"}]});
        assert!(!apply_task_list_output(&mut state, &Value::Null, &content));
        assert_eq!(state[0].1.subject, "Build");
        let text = json!("#1 [deleted] A");
        assert!(parse_task_list(&Value::Null, &text).is_none());
    }

    #[test]
    fn pc1_5_task_list_prefers_tool_use_result() {
        let mut state = one_task("pending");
        let tool_use_result =
            json!({"tasks": [{"id": "9", "subject": "Nine", "status": "pending"}]});
        assert!(apply_task_list_output(
            &mut state,
            &tool_use_result,
            &json!("garbage")
        ));
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].0, "9");
    }

    // T-25 (tools.ts:274-287): TaskCreate needs the OUTPUT task id; an input
    // `taskId` is never a fallback.
    #[test]
    fn pc1_5_task_create_needs_an_output_id() {
        let mut state = Vec::new();
        let input = json!({"taskId": "1", "subject": "A", "description": "D"});
        apply_task_create(&mut state, &input, None);
        assert!(state.is_empty());
        apply_task_create(&mut state, &input, Some("1"));
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].1.subject, "A");
        assert_eq!(state[0].1.status, "pending");
    }

    // T-23 (tools.ts:126-155): every TaskCreate output form Node accepts: a
    // structured object, its JSON-string form, JSON carried by text blocks
    // (skipping unrelated JSON), and the human-readable text form.
    #[test]
    fn pc1_5_task_create_parses_structured_and_text_output() {
        assert_eq!(
            parse_task_create_output(&json!({"task": {"id": "1", "subject": "X"}})),
            Some("1".to_string())
        );
        assert_eq!(
            parse_task_create_output(&json!(r#"{"task":{"id":"2","subject":"Y"}}"#)),
            Some("2".to_string())
        );
        assert_eq!(
            parse_task_create_output(&json!([
                {"type": "text", "text": r#"{"metadata":"unrelated"}"#},
                {"type": "text", "text": r#"{"task":{"id":"3"}}"#}
            ])),
            Some("3".to_string())
        );
        assert_eq!(
            parse_task_create_output(&json!("Task #42 created successfully: Run tests")),
            Some("42".to_string())
        );
        assert_eq!(parse_task_create_output(&json!("not json")), None);
        assert_eq!(
            parse_task_create_output(&json!([{"type": "text", "text": "not json"}])),
            None
        );
        assert_eq!(
            parse_task_create_output(&json!(r#"{"task":{"subject":"X"}}"#)),
            None
        );
        // Node's regex needs a non-empty, whitespace-free id and a non-empty subject.
        assert_eq!(
            parse_task_create_output(&json!("Task # created successfully: X")),
            None
        );
        assert_eq!(
            parse_task_create_output(&json!("Task #1 created successfully: ")),
            None
        );
    }

    // T-23 (acp-agent.ts:13237-13246): TaskCreate prefers the message-level
    // `tool_use_result` over the tool_result content and still publishes a plan
    // when the output carries no id (Node sets shouldEmitTaskPlan unconditionally).
    #[test]
    fn pc1_5_task_create_prefers_tool_use_result() {
        let mut state = Vec::new();
        let input = json!({"subject": "A", "description": ""});
        assert!(apply_task_tool_result(
            &mut state,
            "TaskCreate",
            &input,
            &json!({"task": {"id": "result"}}),
            &json!({"task": {"id": "content"}})
        ));
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].0, "result");

        let mut state = Vec::new();
        assert!(apply_task_tool_result(
            &mut state,
            "TaskCreate",
            &input,
            &Value::Null,
            &json!("nope")
        ));
        assert!(state.is_empty());
    }

    // T-23 (tools.ts:260-269): a failed delete is an unsuccessful update even
    // though the text names no task.
    #[test]
    fn pc1_5_task_update_failed_delete_text_output_is_not_applied() {
        let mut state = one_task("pending");
        let input = json!({"taskId": "1", "status": "deleted"});
        assert!(!apply_task_update_output(
            &mut state,
            &input,
            &Value::Null,
            &json!("Failed to delete task")
        ));
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].1.status, "pending");
    }

    // T-23 (tools.ts:155-172): TaskUpdate parses a structured JSON string and
    // prefers `tool_use_result` over the content.
    #[test]
    fn pc1_5_task_update_parses_json_string_and_prefers_tool_use_result() {
        let mut state = one_task("pending");
        let input = json!({"taskId": "1", "status": "completed"});
        let string_result = json!(r#"{"success":true,"taskId":"1","updatedFields":["status"]}"#);
        assert!(apply_task_update_output(
            &mut state,
            &input,
            &Value::Null,
            &string_result
        ));
        assert_eq!(state[0].1.status, "completed");

        let mut state = one_task("pending");
        let content = json!({"success": true, "taskId": "1", "updatedFields": ["status"]});
        let tool_use_result =
            json!({"success": false, "taskId": "1", "updatedFields": [], "error": "nope"});
        assert!(!apply_task_update_output(
            &mut state,
            &input,
            &tool_use_result,
            &content
        ));
        assert_eq!(state[0].1.status, "pending");
    }

    // T-23 (tools.ts:173-244): TaskList parses a structured JSON string and an
    // empty structured list clears the state.
    #[test]
    fn pc1_5_task_list_parses_json_string_and_structured_empty() {
        let mut state = one_task("in_progress");
        let string_result = json!(r#"{"tasks":[{"id":"1","subject":"A","status":"completed"}]}"#);
        assert!(apply_task_list_output(
            &mut state,
            &Value::Null,
            &string_result
        ));
        assert_eq!(state[0].1.status, "completed");

        let mut state = one_task("pending");
        assert!(apply_task_list_output(
            &mut state,
            &Value::Null,
            &json!({"tasks": []})
        ));
        assert!(state.is_empty());
    }

    // T-22 (tools.ts:308-321): TaskList replaces the snapshot in output order
    // and keeps the richer local fields of the tasks it still holds.
    #[test]
    fn pc1_5_task_list_uses_the_output_order() {
        let mut state = vec![
            (
                "2".to_string(),
                TaskStateEntry {
                    subject: "Two".into(),
                    status: "pending".into(),
                    active_form: Some("Twoing".into()),
                    description: Some("d2".into()),
                },
            ),
            (
                "1".to_string(),
                TaskStateEntry {
                    subject: "One".into(),
                    status: "pending".into(),
                    active_form: None,
                    description: None,
                },
            ),
            (
                "3".to_string(),
                TaskStateEntry {
                    subject: "Gone".into(),
                    status: "completed".into(),
                    active_form: None,
                    description: None,
                },
            ),
        ];
        let content = json!("#1 [in_progress] One\n#2 [completed] Two\n#4 [pending] Four");
        assert!(apply_task_list_output(&mut state, &Value::Null, &content));
        let ids: Vec<&str> = state.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["1", "2", "4"]);
        assert_eq!(state[0].1.status, "in_progress");
        assert_eq!(state[1].1.active_form.as_deref(), Some("Twoing"));
        assert_eq!(state[1].1.description.as_deref(), Some("d2"));
        assert_eq!(state[2].1.subject, "Four");
    }

    // T-22 (acp-agent.ts:13237-13290): TaskGet is read-only, so its result
    // neither mutates the task state nor publishes a plan.
    #[test]
    fn pc1_5_task_get_result_is_silent() {
        let mut state = one_task("in_progress");
        let input = json!({"taskId": "1"});
        let content = json!({"task": {"id": "1", "subject": "Changed", "status": "completed"}});
        assert!(!apply_task_tool_result(
            &mut state,
            "TaskGet",
            &input,
            &Value::Null,
            &content
        ));
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].1.subject, "Build");
        assert_eq!(state[0].1.status, "in_progress");
    }
}
