//! Event builders for native subagent sessions (`acp-subagents.ts`, Q-02).
//!
//! Two forms exist (`SubagentForm`): the RFD form (`subagent_update` and
//! `session_message`) and AIR's earlier draft (`subagent_spawned` and
//! `subagent_state_update`). Every builder returns the notification JSON
//! `{"sessionId": ..., "update": {...}}` exactly as the Node adapter emits it.

use serde_json::{json, Map, Value};

pub use crate::client_caps::SubagentForm;

/// The AIR capability name for native subagent sessions
/// (`AIR_NATIVE_SUBAGENT_SESSIONS_CAPABILITY`, `air-extension.ts`).
pub const AIR_NATIVE_SUBAGENT_SESSIONS_CAPABILITY: &str = "nativeSubagentSessions";

/// AIR's end state of a child (`SubagentState`, `acp-subagents.ts:58`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    Completed,
    Failed,
    Cancelled,
    Disconnected,
}

impl SubagentState {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Disconnected => "disconnected",
        }
    }
}

/// The JSON-RPC internal error code (`RequestError.internalError().code`).
pub const INTERNAL_ERROR_CODE: i64 = -32603;

/// The RFD work state of a child (`WorkState`, `subagent_update.state`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkState {
    pub state: &'static str,
    pub stop_reason: Option<&'static str>,
    /// `(code, message)` of the JSON-RPC error of a failed child.
    pub error: Option<(i64, String)>,
}

impl WorkState {
    /// A state with no stop reason.
    #[must_use]
    pub fn of(state: &'static str) -> Self {
        Self {
            state,
            stop_reason: None,
            error: None,
        }
    }

    /// The wire object, `{state, stopReason?, error?}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("state".into(), json!(self.state));
        if let Some(reason) = self.stop_reason {
            map.insert("stopReason".into(), json!(reason));
        }
        if let Some((code, message)) = &self.error {
            map.insert("error".into(), json!({"code": code, "message": message}));
        }
        Value::Object(map)
    }
}

/// `{sessionId, update}`.
#[must_use]
pub fn notification(session_id: &str, update: Value) -> Value {
    json!({"sessionId": session_id, "update": update})
}

/// AIR `subagent_spawned` (`announceNativeSubagent`, `native-subagents.ts:1203-1213`).
/// `prompt` is the adapter extension and is absent when unknown.
#[must_use]
pub fn subagent_spawned(
    parent_session_id: &str,
    subagent_session_id: &str,
    name: &str,
    task: &str,
    prompt: Option<&str>,
) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("subagent_spawned"));
    update.insert("subagentSessionId".into(), json!(subagent_session_id));
    update.insert("name".into(), json!(name));
    update.insert("task".into(), json!(task));
    if let Some(prompt) = prompt {
        update.insert("prompt".into(), json!(prompt));
    }
    update.insert("capabilities".into(), json!({}));
    notification(parent_session_id, Value::Object(update))
}

/// AIR `subagent_state_update` (`finishNativeSubagent`, `native-subagents.ts:1235-1242`).
#[must_use]
pub fn subagent_state_update(
    parent_session_id: &str,
    subagent_session_id: &str,
    state: SubagentState,
) -> Value {
    notification(
        parent_session_id,
        json!({
            "sessionUpdate": "subagent_state_update",
            "subagentSessionId": subagent_session_id,
            "state": state.as_str(),
        }),
    )
}

/// RFD announce (`native-subagents.ts:1179-1189`): `subagent_update` with the
/// labels, `capabilities: {cancel: {}}` and state `running`.
#[must_use]
pub fn subagent_announce(
    parent_session_id: &str,
    subagent_session_id: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> Value {
    let mut update = labelled_update(subagent_session_id, title, description);
    update.insert("capabilities".into(), json!({"cancel": {}}));
    update.insert("state".into(), json!({"state": "running"}));
    notification(parent_session_id, Value::Object(update))
}

/// RFD ancestor announce (`restore`, `native-subagents.ts:700-708`): the labels
/// only, with no state and no capabilities.
#[must_use]
pub fn subagent_labels_update(
    parent_session_id: &str,
    subagent_session_id: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> Value {
    notification(
        parent_session_id,
        Value::Object(labelled_update(subagent_session_id, title, description)),
    )
}

fn labelled_update(
    subagent_session_id: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> Map<String, Value> {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("subagent_update"));
    update.insert("sessionId".into(), json!(subagent_session_id));
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        update.insert("title".into(), json!(title));
    }
    if let Some(description) = description.filter(|d| !d.is_empty()) {
        update.insert("description".into(), json!(description));
    }
    update
}

/// RFD work-state report (`reportState`, `native-subagents.ts:903-906`).
#[must_use]
pub fn subagent_state(
    parent_session_id: &str,
    subagent_session_id: &str,
    state: &WorkState,
) -> Value {
    notification(
        parent_session_id,
        json!({
            "sessionUpdate": "subagent_update",
            "sessionId": subagent_session_id,
            "state": state.to_value(),
        }),
    )
}

/// RFD `session_message` into the child's transcript (`publishPrompt`,
/// `native-subagents.ts:1271-1280`).
#[must_use]
pub fn session_message(
    child_session_id: &str,
    message_id: &str,
    sender_session_id: Option<&str>,
    text: &str,
) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("session_message"));
    update.insert("messageId".into(), json!(message_id));
    if let Some(sender) = sender_session_id {
        update.insert("senderSessionId".into(), json!(sender));
    }
    update.insert("recipientSessionId".into(), json!(child_session_id));
    update.insert("content".into(), json!([{"type": "text", "text": text}]));
    notification(child_session_id, Value::Object(update))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawned_shape_with_and_without_prompt() {
        assert_eq!(
            subagent_spawned("root", "t1", "Explore", "do it", Some("do it now")),
            json!({"sessionId": "root", "update": {
                "sessionUpdate": "subagent_spawned", "subagentSessionId": "t1",
                "name": "Explore", "task": "do it", "prompt": "do it now", "capabilities": {}}})
        );
        let bare = subagent_spawned("root", "t1", "Explore", "Delegated task", None);
        assert!(bare["update"].get("prompt").is_none());
        assert_eq!(bare["update"]["capabilities"], json!({}));
    }

    #[test]
    fn state_update_shape() {
        assert_eq!(
            subagent_state_update("root", "t1", SubagentState::Cancelled),
            json!({"sessionId": "root", "update": {
                "sessionUpdate": "subagent_state_update", "subagentSessionId": "t1",
                "state": "cancelled"}})
        );
    }

    #[test]
    fn rfd_announce_and_state_shapes() {
        assert_eq!(
            subagent_announce("root", "t1", Some("Ann (Explore)"), Some("look")),
            json!({"sessionId": "root", "update": {
                "sessionUpdate": "subagent_update", "sessionId": "t1",
                "title": "Ann (Explore)", "description": "look",
                "capabilities": {"cancel": {}}, "state": {"state": "running"}}})
        );
        let failed = WorkState {
            state: "idle",
            stop_reason: Some("error"),
            error: Some((INTERNAL_ERROR_CODE, "boom".into())),
        };
        assert_eq!(
            subagent_state("root", "t1", &failed)["update"]["state"],
            json!({"state": "idle", "stopReason": "error", "error": {"code": -32603, "message": "boom"}})
        );
        assert_eq!(
            WorkState::of("unknown").to_value(),
            json!({"state": "unknown"})
        );
    }

    #[test]
    fn session_message_shape() {
        assert_eq!(
            session_message("t1", "toolu_1", Some("root"), "hi"),
            json!({"sessionId": "t1", "update": {
                "sessionUpdate": "session_message", "messageId": "toolu_1",
                "senderSessionId": "root", "recipientSessionId": "t1",
                "content": [{"type": "text", "text": "hi"}]}})
        );
        let no_sender = session_message("t1", "m", None, "hi");
        assert!(no_sender["update"].get("senderSessionId").is_none());
    }
}
