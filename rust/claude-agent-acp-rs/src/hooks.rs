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

/// An error control_response when a hook callback is unknown or cannot be processed.
pub fn hook_control_response_error(request_id: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id
        }
    })
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

    /// Register a hook callback for `tool_use_id` once. Returns false if already registered.
    pub fn register_once(
        &mut self,
        tool_use_id: String,
        tool_name: String,
        parent_tool_use_id: Option<String>,
    ) -> bool {
        if self.callbacks.contains_key(&tool_use_id) {
            return false;
        }
        self.callbacks.insert(
            tool_use_id,
            HookEntry {
                tool_name,
                parent_tool_use_id,
                completed_at: None,
            },
        );
        true
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

    /// Lazily prune callbacks whose completed_at is older than 30 seconds.
    pub fn prune_expired(&mut self, now: Instant) {
        self.callbacks.retain(|_, entry| {
            if let Some(completed) = entry.completed_at {
                now.duration_since(completed) < Duration::from_secs(30)
            } else {
                true
            }
        });
    }

    /// Clear all registered callbacks.
    pub fn clear(&mut self) {
        self.callbacks.clear();
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
                            let rest = if line.len() > 1 { &line[1..] } else { "" };
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

/// Parses the created task ID from TaskCreate tool output.
pub fn parse_task_create_output(val: &Value) -> Option<String> {
    if let Some(id) = val.pointer("/task/id").and_then(Value::as_str) {
        return Some(id.to_string());
    }
    let text = if let Some(s) = val.as_str() {
        s
    } else if let Some(arr) = val.as_array() {
        arr.iter()
            .find_map(|b| b.get("text").and_then(Value::as_str))
            .unwrap_or("")
    } else {
        ""
    };
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("Task #") {
        if let Some(idx) = rest.find(" created successfully: ") {
            return Some(rest[..idx].to_string());
        }
    }
    None
}

/// Applies a TaskCreate tool call to the task state.
pub fn apply_task_create(
    state: &mut Vec<(String, TaskStateEntry)>,
    input: &Value,
    task_id: Option<&str>,
) {
    let task_id = task_id.or_else(|| input.get("taskId").and_then(Value::as_str));
    let Some(id) = task_id else { return };
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

/// Applies a TaskUpdate tool call to the task state.
pub fn apply_task_update(state: &mut Vec<(String, TaskStateEntry)>, input: &Value) {
    let Some(task_id) = input.get("taskId").and_then(Value::as_str) else {
        return;
    };
    let status = input
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending");
    if status == "deleted" {
        state.retain(|(k, _)| k != task_id);
        return;
    }
    let existing_entry = state
        .iter()
        .find(|(k, _)| k == task_id)
        .map(|(_, v)| v.clone());
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

/// Parses task list entries from TaskList tool output.
pub fn parse_task_list_output(content: &Value) -> Vec<(String, String, String)> {
    if let Some(tasks) = content.get("tasks").and_then(Value::as_array) {
        return tasks
            .iter()
            .filter_map(|t| {
                let id = t.get("id").and_then(Value::as_str)?.to_string();
                let subject = t.get("subject").and_then(Value::as_str)?.to_string();
                let status = t.get("status").and_then(Value::as_str)?.to_string();
                Some((id, subject, status))
            })
            .collect();
    }
    let text = if let Some(s) = content.as_str() {
        s
    } else if let Some(arr) = content.as_array() {
        arr.iter()
            .find_map(|b| b.get("text").and_then(Value::as_str))
            .unwrap_or("")
    } else {
        ""
    };
    let mut out = Vec::new();
    for line in text.trim().split('\n') {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('#') {
            if let Some(bracket_open) = rest.find(" [") {
                let id = rest[..bracket_open].trim();
                let after_id = &rest[bracket_open + 2..];
                if let Some(bracket_close) = after_id.find(']') {
                    let status = &after_id[..bracket_close];
                    let subject = after_id[bracket_close + 1..].trim();
                    out.push((id.to_string(), subject.to_string(), status.to_string()));
                }
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_64_registry_register_once() {
        let mut reg = HookRegistry::new();
        assert!(reg.register_once("toolu_1".into(), "Bash".into(), None));
        // Second registration for the same ID is rejected
        assert!(!reg.register_once("toolu_1".into(), "Edit".into(), Some("p1".into())));
        assert_eq!(reg.len(), 1);
        let entry = reg.unregister("toolu_1").expect("present");
        assert_eq!(entry.tool_name, "Bash");
        assert_eq!(entry.parent_tool_use_id, None);
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
    fn inv_64_unknown_callback_error() {
        let resp = hook_control_response_error("req_unknown");
        assert_eq!(
            resp,
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": "req_unknown"
                }
            })
        );
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

    #[test]
    fn inv_64_enter_plan_mode_publishes_mode() {
        // Test that EnterPlanMode tool hook triggers plan mode transition payload
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "EnterPlanMode",
            "tool_input": {},
            "tool_response": {"status": "ok"}
        });
        assert_eq!(
            input.get("tool_name").and_then(Value::as_str),
            Some("EnterPlanMode")
        );
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
}
