//! Native subagent sessions (`native-subagents.ts`, F35a/F35b).
//!
//! The pure helpers live here (end-state mapping, display labels, control-tool
//! detection, SendMessage resume lookups); the connection-local registry is
//! [`runtime::NativeSubagentRuntime`], a synchronous state machine that returns
//! the ordered publications instead of awaiting them, so no lock is ever held
//! across an await.

use serde_json::Value;

use crate::acp_subagents::{SubagentState, WorkState, INTERNAL_ERROR_CODE};

pub mod runtime;
#[cfg(test)]
mod tests;

pub use runtime::{
    AwaitToken, HistoricalSubagent, NativeSubagent, NativeSubagentRuntime, Out, RouteResult,
    TaskStarted,
};

/// What the SDK says about the end of a task, beyond its status
/// (`TaskEnd`, `native-subagents.ts:65-75`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskEnd {
    /// The SDK's text for a failure.
    pub error: Option<String>,
    /// The SDK's reason for an unusual end, such as `worker_restart`.
    pub reason: Option<String>,
    /// The adapter knows that the work ended, but not why.
    pub unexplained: bool,
}

/// `nativeSubagentState` (`native-subagents.ts:1397-1404`).
#[must_use]
pub fn native_subagent_state(status: &str) -> Option<SubagentState> {
    match status {
        "completed" => Some(SubagentState::Completed),
        "failed" => Some(SubagentState::Failed),
        "disconnected" => Some(SubagentState::Disconnected),
        "killed" | "cancelled" | "stopped" => Some(SubagentState::Cancelled),
        _ => None,
    }
}

/// `rfdEndState` (`native-subagents.ts:1300-1319`): the RFD work state at the
/// end of a child's work.
#[must_use]
pub fn rfd_end_state(state: SubagentState, end: Option<&TaskEnd>) -> WorkState {
    if end.is_some_and(|e| e.unexplained || e.reason.as_deref() == Some("worker_restart")) {
        return WorkState::of("idle");
    }
    match state {
        SubagentState::Completed => WorkState {
            state: "idle",
            stop_reason: Some("end_turn"),
            error: None,
        },
        SubagentState::Cancelled => WorkState {
            state: "idle",
            stop_reason: Some("cancelled"),
            error: None,
        },
        SubagentState::Failed => WorkState {
            state: "idle",
            stop_reason: Some("error"),
            error: end
                .and_then(|e| e.error.clone())
                .filter(|e| !e.is_empty())
                .map(|message| (INTERNAL_ERROR_CODE, message)),
        },
        // "disconnected": the child can no longer be observed.
        SubagentState::Disconnected => WorkState::of("unknown"),
    }
}

/// The RFD labels of a child (`rfdLabels`, `native-subagents.ts:1326-1343`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RfdLabels {
    pub title: Option<String>,
    pub role: Option<String>,
    pub description: Option<String>,
}

/// `rfdLabels`: the title is the name with the role after it when both are
/// known and differ, else whichever is known. The description is never the prompt.
#[must_use]
pub fn rfd_labels(name: Option<&str>, role: Option<&str>, description: Option<&str>) -> RfdLabels {
    let agent_name = non_blank(name);
    let agent_role = non_blank(role);
    let title = match (&agent_name, &agent_role) {
        (Some(n), Some(r)) if n != r => Some(format!("{n} ({r})")),
        _ => agent_name.clone().or_else(|| agent_role.clone()),
    };
    RfdLabels {
        title,
        role: agent_role,
        description: non_blank(description),
    }
}

/// `subagentDisplayName` (`native-subagents.ts:1520-1531`).
#[must_use]
pub fn subagent_display_name(
    explicit_name: Option<&str>,
    description: Option<&str>,
    kind: Option<&str>,
    task_id: &str,
) -> String {
    for value in [explicit_name, description, kind].into_iter().flatten() {
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    let chars: Vec<char> = task_id.chars().collect();
    let suffix: String = if chars.len() > 8 {
        chars[chars.len() - 8..].iter().collect()
    } else {
        task_id.to_string()
    };
    format!("Agent {suffix}")
}

/// `subagentDescription` (`native-subagents.ts:1533-1538`).
#[must_use]
pub fn subagent_description(prompt: Option<&str>, description: Option<&str>) -> String {
    for value in [prompt, description].into_iter().flatten() {
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    "Delegated task".to_string()
}

/// `promptText` (`native-subagents.ts:1592`): the text unchanged when it is a
/// non-blank string.
#[must_use]
pub fn prompt_text(value: Option<&str>) -> Option<String> {
    value
        .filter(|v| !v.trim().is_empty())
        .map(ToString::to_string)
}

/// `nonBlankString` (`native-subagents.ts:1600`): trimmed, `None` when blank.
#[must_use]
pub fn non_blank(value: Option<&str>) -> Option<String> {
    value
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().to_string())
}

/// `isNativeSubagentControlTool` (`native-subagents.ts:1421`).
#[must_use]
pub fn is_native_subagent_control_tool(tool_name: Option<&str>) -> bool {
    matches!(tool_name, Some("Agent" | "Task"))
}

/// `isNativeSubagentControlUpdate` (`native-subagents.ts:1405-1419`): a
/// `tool_call`/`tool_call_update` that carries AIR's subagent marker or whose
/// tool is Agent or Task.
#[must_use]
pub fn is_native_subagent_control_update(update: &Value) -> bool {
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if kind != Some("tool_call") && kind != Some("tool_call_update") {
        return false;
    }
    let air_marked = update
        .pointer("/_meta/jetbrains/air/subagent")
        .is_some_and(|v| v == &Value::Bool(true));
    let tool_name = update
        .pointer("/_meta/claudeCode/toolName")
        .and_then(Value::as_str);
    air_marked || is_native_subagent_control_tool(tool_name)
}

/// `subagentLaunchIds` (`subagent-history.ts:175-186`): the ids of the Agent
/// and Task calls that launched subagents in message `content`.
#[must_use]
pub fn subagent_launch_ids(content: &Value) -> Vec<String> {
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter(|b| is_native_subagent_control_tool(b.get("name").and_then(Value::as_str)))
        .filter_map(|b| b.get("id").and_then(Value::as_str).map(ToString::to_string))
        .collect()
}

/// `resumedNativeSubagentId` (`native-subagents.ts:1349-1353`): the agent id a
/// successful SendMessage result resumed.
#[must_use]
pub fn resumed_native_subagent_id(tool_use_result: &Value) -> Option<String> {
    if tool_use_result.get("success") != Some(&Value::Bool(true)) {
        return None;
    }
    non_blank(
        tool_use_result
            .get("resumedAgentId")
            .and_then(Value::as_str),
    )
}

/// A tool use the SDK recorded: its name and input.
#[derive(Debug, Clone)]
pub struct ToolUseRecord {
    pub name: String,
    pub input: Value,
}

/// `sendMessageResumePrompt` (`native-subagents.ts:1360-1366`).
#[must_use]
pub fn send_message_resume_prompt(
    tool_uses: &[(String, ToolUseRecord)],
    agent_id: &str,
    result_tool_use_ids: &[String],
) -> Option<String> {
    send_message_resume(tool_uses, agent_id, result_tool_use_ids).and_then(|r| r.text)
}

/// The SendMessage call that resumed an agent, and its text when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendMessageResume {
    pub tool_use_id: String,
    pub text: Option<String>,
}

/// `sendMessageResume` (`native-subagents.ts:1373-1395`). `tool_uses` is in
/// insertion order; the fallback scan runs latest-first.
#[must_use]
pub fn send_message_resume(
    tool_uses: &[(String, ToolUseRecord)],
    agent_id: &str,
    result_tool_use_ids: &[String],
) -> Option<SendMessageResume> {
    let find = |id: &str| {
        tool_uses
            .iter()
            .rev()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v)
    };
    let mut without_text: Option<String> = None;
    for id in result_tool_use_ids {
        let Some(tool_use) = find(id) else { continue };
        if tool_use.name != "SendMessage" {
            continue;
        }
        let text = prompt_text(tool_use.input.get("message").and_then(Value::as_str));
        if text.is_some() {
            return Some(SendMessageResume {
                tool_use_id: id.clone(),
                text,
            });
        }
        if without_text.is_none() {
            without_text = Some(id.clone());
        }
    }
    if !result_tool_use_ids.is_empty() {
        return without_text.map(|tool_use_id| SendMessageResume {
            tool_use_id,
            text: None,
        });
    }
    for (id, tool_use) in tool_uses.iter().rev() {
        if tool_use.name != "SendMessage" {
            continue;
        }
        if tool_use.input.get("to").and_then(Value::as_str) != Some(agent_id) {
            continue;
        }
        let text = prompt_text(tool_use.input.get("message").and_then(Value::as_str));
        return Some(SendMessageResume {
            tool_use_id: id.clone(),
            text,
        });
    }
    None
}
