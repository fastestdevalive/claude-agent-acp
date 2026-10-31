//! The replay of subagents to a client on the subagents RFD form
//! (`subagent-replay.ts`, F37 / Q-61).
//!
//! Claude Code keeps each subagent's history in its own transcript, and the
//! subagent's metadata file names the Agent or Task call that launched it (see
//! [`crate::subagent_history`]). The replay announces each child right after
//! that call, under the session id it had live (the SDK agent id), and sends the
//! child's history to the child session. Every child's end goes out before the
//! replay ends, since no subagent from before the reload still runs.
//!
//! A launch without metadata has no known id, so it is not announced: the RFD
//! forbids inventing one. Its Agent call stays an ordinary tool call of the
//! parent, and a notice says that subagent history is missing.
//!
//! Node's class awaits each publication; this port is a synchronous state
//! machine that returns the publications in order (like
//! [`crate::native_subagents::runtime`]), so the owner publishes them and never
//! holds a lock across an await.

use serde_json::{json, Map, Value};

use crate::acp_subagents::{session_message, subagent_labels_update, SubagentState, WorkState};
use crate::native_subagents::{
    is_native_subagent_control_tool, native_subagent_state, rfd_end_state, rfd_labels,
};
use crate::notices::{notice_update, SessionNotice};
use crate::subagent_history::{SubagentHistory, SubagentRecord};

/// A child that the replay shows: its launch has metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayChild {
    /// The Agent or Task tool use that launched it.
    pub launch_id: String,
    /// Its session id: the SDK agent id, as live.
    pub session_id: String,
    pub record: SubagentRecord,
    /// The session that made the launch.
    pub parent_session_id: String,
    /// The input of the launch call.
    pub input: Value,
    /// How the launch's tool result ended a foreground child's run.
    pub launch_end: Option<SubagentState>,
    /// The state of the child's last persisted task notification.
    pub notified: Option<SubagentState>,
    pub announced: bool,
    pub prompt_sent: bool,
}

/// A `SendMessage` call of the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct SendMessageCall {
    pub tool_use_id: String,
    pub to: String,
    pub message: String,
    pub sender_session_id: String,
    pub delivered: bool,
}

/// What a load/resume replay needs to replay a session's subagents (B-52):
/// the session's subagent history, whether the client takes notices, and the
/// adapter logger.
#[derive(Debug, Clone)]
pub struct SubagentReplayContext {
    pub history: SubagentHistory,
    pub notices: bool,
    pub logger: Option<std::sync::Arc<crate::logger::Logger>>,
}

/// The replay of a session's subagents on the RFD form (`RfdSubagentReplay`).
#[derive(Debug, Clone)]
pub struct RfdSubagentReplay {
    history: SubagentHistory,
    /// The children by the id of the tool use that launched each.
    children: std::collections::BTreeMap<String, ReplayChild>,
    /// The launch ids in registration (scan) order: `finish` ends the children
    /// in reverse insertion order, as the upstream `Map` iteration does
    /// (`subagent-replay.ts:185`).
    order: Vec<String>,
    /// The launch id of each agent id.
    by_agent_id: std::collections::BTreeMap<String, String>,
    send_messages: Vec<SendMessageCall>,
    /// Launches without metadata, which the replay leaves out.
    omitted: std::collections::BTreeSet<String>,
    /// Children whose transcript could not be read: only their association is
    /// replayed.
    without_history: usize,
}

impl RfdSubagentReplay {
    /// A replay over the session's subagent `history`.
    #[must_use]
    pub fn new(history: SubagentHistory) -> Self {
        Self {
            history,
            children: std::collections::BTreeMap::new(),
            order: Vec::new(),
            by_agent_id: std::collections::BTreeMap::new(),
            send_messages: Vec::new(),
            omitted: std::collections::BTreeSet::new(),
            without_history: 0,
        }
    }

    /// Registers the launches, the launch results, and the SendMessage calls in
    /// `messages`, the transcript of the session `owner_session_id`.
    pub fn scan(&mut self, messages: &[Value], owner_session_id: &str) {
        for message in messages {
            let Some(content) = message
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let Some(id) = block.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        if is_native_subagent_control_tool(
                            block.get("name").and_then(Value::as_str),
                        ) {
                            self.register_launch(id, input_of(block), owner_session_id);
                        } else if block.get("name").and_then(Value::as_str) == Some("SendMessage") {
                            let input = input_of(block);
                            if let (Some(to), Some(text)) = (
                                input.get("to").and_then(Value::as_str),
                                input.get("message").and_then(Value::as_str),
                            ) {
                                self.send_messages.push(SendMessageCall {
                                    tool_use_id: id.to_string(),
                                    to: to.to_string(),
                                    message: text.to_string(),
                                    sender_session_id: owner_session_id.to_string(),
                                    delivered: false,
                                });
                            }
                        }
                    }
                    // The result of a launch (`subagent-replay.ts:82-87`): a
                    // background launch answers at once, so its result is not
                    // the end. Independent of the block's `id`, and only a
                    // `tool_result` (no `mcp_tool_result`).
                    Some("tool_result") => {
                        let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str)
                        else {
                            continue;
                        };
                        if let Some(child) = self.children.get_mut(tool_use_id) {
                            if !child.record.background {
                                child.launch_end = Some(replay_subagent_terminal_state(block));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The child that the tool use `tool_use_id` launched, when the replay shows
    /// it.
    #[must_use]
    pub fn child_of_launch(&self, tool_use_id: &str) -> Option<&ReplayChild> {
        self.children.get(tool_use_id)
    }

    /// Announces `child` on its parent, as live: running, with the parent's
    /// labels. Returns the notifications to publish.
    pub fn announce(&mut self, launch_id: &str) -> Vec<Value> {
        let Some(child) = self.children.get_mut(launch_id) else {
            return Vec::new();
        };
        if child.announced {
            return Vec::new();
        }
        let labels = rfd_labels(
            child
                .record
                .name
                .as_deref()
                .or_else(|| child.input.get("name").and_then(Value::as_str)),
            child
                .record
                .agent_type
                .as_deref()
                .or_else(|| child.input.get("subagent_type").and_then(Value::as_str)),
            child
                .record
                .description
                .as_deref()
                .or_else(|| child.input.get("description").and_then(Value::as_str)),
        );
        let mut notification = subagent_labels_update(
            &child.parent_session_id,
            &child.session_id,
            labels.title.as_deref(),
            labels.description.as_deref(),
        );
        if let Some(update) = notification
            .get_mut("update")
            .and_then(Value::as_object_mut)
        {
            update.insert("state".to_string(), json!({"state": "running"}));
        }
        child.announced = true;
        vec![notification]
    }

    /// The rest of a child's user message to replay, or `None` when nothing is
    /// left, after its text went out as the child's incoming `session_message`.
    ///
    /// A child session has no user, so the text of its user messages is never a
    /// user message. The first text is the prompt of the launch. A message that
    /// a SendMessage call delivered (`origin.kind: "coordinator"`) is another
    /// incoming message. Any other text is harness text, such as "[Request
    /// interrupted by user for tool use]", which the live child session does
    /// not show. Tool results stay.
    pub fn child_content(
        &mut self,
        launch_id: &str,
        message: &Value,
        content: &Value,
    ) -> (Vec<Value>, Option<Value>) {
        let mut out = Vec::new();
        let text = text_of(content);
        if !text.trim().is_empty() {
            if is_delivery(message) {
                out.extend(self.send_delivery(launch_id, message, &text));
            } else if let Some(child) = self.children.get(launch_id) {
                if !child.prompt_sent {
                    out.extend(self.send_prompt(launch_id, &text));
                }
            }
        }
        let Some(blocks) = content.as_array() else {
            return (out, None);
        };
        let rest: Vec<Value> = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) != Some("text"))
            .cloned()
            .collect();
        let rest = if rest.is_empty() {
            None
        } else {
            Some(Value::Array(rest))
        };
        (out, rest)
    }

    /// Ends the replay of `child`'s history. `message_count` is the length of
    /// its transcript: when it is empty, the replay has only the child's
    /// association, and the prompt comes from the launch call.
    pub fn child_replayed(&mut self, launch_id: &str, message_count: usize) -> Vec<Value> {
        if message_count == 0 {
            self.without_history += 1;
        }
        let prompt = self
            .children
            .get(launch_id)
            .filter(|child| !child.prompt_sent)
            .and_then(|child| child.input.get("prompt").and_then(Value::as_str))
            .map(ToString::to_string);
        match prompt {
            Some(prompt) => self.send_prompt(launch_id, &prompt),
            None => Vec::new(),
        }
    }

    /// Takes a persisted task notification. Returns whether it was a child's:
    /// its task id is the agent id.
    pub fn task_notification(&mut self, task_id: &str, status: &str) -> bool {
        let Some(launch_id) = self.by_agent_id.get(task_id).cloned() else {
            return false;
        };
        if let Some(child) = self.children.get_mut(&launch_id) {
            if let Some(state) = native_subagent_state(status) {
                child.notified = Some(state);
            }
        }
        true
    }

    /// Sends the end of every announced child, and a notice when subagent
    /// history is missing. The end is the state of the child's last task
    /// notification, else `cancelled` for a child the user stopped, else the
    /// result of its foreground launch, else `idle` with no stop reason.
    pub fn finish(&mut self, supports_notices: bool, root_session_id: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let launches: Vec<String> = self.order.clone();
        for launch_id in launches.into_iter().rev() {
            let Some(child) = self.children.get(&launch_id) else {
                continue;
            };
            if !child.announced {
                continue;
            }
            let end = child.notified.or(if child.record.stopped_by_user {
                Some(SubagentState::Cancelled)
            } else {
                child.launch_end
            });
            let state = match end {
                Some(state) => rfd_end_state(state, None),
                None => WorkState::of("idle"),
            };
            let mut update = Map::new();
            update.insert("sessionUpdate".to_string(), json!("subagent_update"));
            update.insert("sessionId".to_string(), json!(child.session_id));
            update.insert("state".to_string(), state.to_value());
            out.push(json!({
                "sessionId": child.parent_session_id,
                "update": Value::Object(update),
            }));
        }
        let missing = self.omitted.len() + self.without_history;
        if missing > 0 && supports_notices {
            let description = if missing == 1 {
                "The history of 1 subagent of this session could not be restored.".to_string()
            } else {
                format!("The history of {missing} subagents of this session could not be restored.")
            };
            out.push(json!({
                "sessionId": root_session_id,
                "update": notice_update(&SessionNotice::new(
                    "info",
                    "Some subagent history is not available",
                    Some(description),
                )),
            }));
        }
        out
    }

    /// The launches without metadata that the replay leaves out.
    #[must_use]
    pub fn omitted(&self) -> &std::collections::BTreeSet<String> {
        &self.omitted
    }

    /// The children whose transcript could not be read.
    #[must_use]
    pub fn without_history(&self) -> usize {
        self.without_history
    }

    fn register_launch(&mut self, tool_use_id: &str, input: Value, parent_session_id: &str) {
        if self.children.contains_key(tool_use_id) {
            return;
        }
        let Some(record) = self.history.launched_by(tool_use_id).cloned() else {
            self.omitted.insert(tool_use_id.to_string());
            return;
        };
        self.by_agent_id
            .insert(record.agent_id.clone(), tool_use_id.to_string());
        self.order.push(tool_use_id.to_string());
        self.children.insert(
            tool_use_id.to_string(),
            ReplayChild {
                launch_id: tool_use_id.to_string(),
                session_id: record.agent_id.clone(),
                record,
                parent_session_id: parent_session_id.to_string(),
                input,
                launch_end: None,
                notified: None,
                announced: false,
                prompt_sent: false,
            },
        );
    }

    /// The prompt of the launch, with the launch's id, as live.
    fn send_prompt(&mut self, launch_id: &str, text: &str) -> Vec<Value> {
        let Some(child) = self.children.get_mut(launch_id) else {
            return Vec::new();
        };
        child.prompt_sent = true;
        let (session_id, sender) = (child.session_id.clone(), child.parent_session_id.clone());
        vec![session_message(&session_id, launch_id, Some(&sender), text)]
    }

    /// A message that a SendMessage call delivered. Claude Code frames it for
    /// the child's model, so the call it came from is the first undelivered
    /// SendMessage to the child whose message the delivered text contains. With
    /// that call, the message has the call's id and text and its sender, as
    /// live; without it, its transcript id and the delivered text.
    fn send_delivery(&mut self, launch_id: &str, message: &Value, text: &str) -> Vec<Value> {
        let Some(child) = self.children.get(launch_id) else {
            return Vec::new();
        };
        let names: Vec<&str> = [
            Some(child.session_id.as_str()),
            child.record.name.as_deref(),
            child.input.get("name").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .collect();
        let call = self.send_messages.iter().position(|candidate| {
            !candidate.delivered
                && names.contains(&candidate.to.as_str())
                && text.contains(candidate.message.trim())
        });
        if let Some(index) = call {
            self.send_messages[index].delivered = true;
            let call = self.send_messages[index].clone();
            let session_id = child.session_id.clone();
            return vec![session_message(
                &session_id,
                &call.tool_use_id,
                Some(&call.sender_session_id),
                &call.message,
            )];
        }
        let message_id = message
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let session_id = child.session_id.clone();
        vec![session_message(&session_id, &message_id, None, text)]
    }
}

/// The end of a foreground launch from its tool result: an error result that
/// names a stop is a cancellation, any other error a failure
/// (`replaySubagentTerminalState`).
#[must_use]
pub fn replay_subagent_terminal_state(block: &Value) -> SubagentState {
    if block.get("is_error") != Some(&Value::Bool(true)) {
        return SubagentState::Completed;
    }
    let text = replay_content_text(block.get("content").unwrap_or(&Value::Null)).to_lowercase();
    if has_stop_word(&text) {
        SubagentState::Cancelled
    } else {
        SubagentState::Failed
    }
}

/// `/\b(?:cancelled|canceled|interrupted|stopped|killed)\b/`.
fn has_stop_word(text: &str) -> bool {
    const WORDS: [&str; 5] = ["cancelled", "canceled", "interrupted", "stopped", "killed"];
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let chars: Vec<char> = text.chars().collect();
    for word in WORDS {
        let letters: Vec<char> = word.chars().collect();
        let mut start = 0;
        while start + letters.len() <= chars.len() {
            if chars[start..start + letters.len()] == letters[..] {
                let before_ok = start == 0 || !is_word(chars[start - 1]);
                let after = start + letters.len();
                let after_ok = after == chars.len() || !is_word(chars[after]);
                if before_ok && after_ok {
                    return true;
                }
            }
            start += 1;
        }
    }
    false
}

/// The text of a tool result's content, recursively (`replayContentText`).
fn replay_content_text(value: &Value) -> String {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            Value::Object(map) => {
                for key in ["text", "content", "message"] {
                    if let Some(item) = map.get(key) {
                        walk(item, out);
                    }
                }
            }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    walk(value, &mut parts);
    parts.join(" ")
}

/// The input of a tool use block (`inputOf`).
fn input_of(block: &Value) -> Value {
    match block.get("input") {
        Some(input) if input.is_object() => input.clone(),
        _ => Value::Object(Map::new()),
    }
}

/// The text of user message content: a string, or its text blocks (`textOf`).
fn text_of(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A message that a SendMessage call delivered to the child (`isDelivery`).
fn is_delivery(message: &Value) -> bool {
    message
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        == Some("coordinator")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent_history::SubagentRecord;
    use std::collections::BTreeMap;

    fn record(agent_id: &str, name: &str, background: bool, stopped: bool) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            name: Some(name.to_string()),
            agent_type: Some("general-purpose".to_string()),
            description: Some("Work".to_string()),
            background,
            stopped_by_user: stopped,
            tool_use_id: None,
            parent_agent_id: None,
        }
    }

    fn history(records: &[(&str, SubagentRecord)]) -> SubagentHistory {
        let mut agents = BTreeMap::new();
        for (launch, record) in records {
            agents.insert((*launch).to_string(), record.clone());
        }
        SubagentHistory {
            agents,
            ..Default::default()
        }
    }

    fn launch_message(launch_id: &str) -> Value {
        json!({
            "type": "assistant",
            "uuid": "a1",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": launch_id,
                    "name": "Agent",
                    "input": {
                        "name": "scout",
                        "subagent_type": "Explore",
                        "description": "Look around",
                        "prompt": "Look around",
                    },
                }],
            },
        })
    }

    /// B-104: the announce carries the labels and `running`, with no
    /// capabilities, on the parent.
    #[test]
    fn announce_is_labelled_running_on_the_parent() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        replay.scan(&[launch_message("toolu_agent")], "root");
        let out = replay.announce("toolu_agent");
        assert_eq!(
            out,
            vec![json!({"sessionId": "root", "update": {
                "sessionUpdate": "subagent_update",
                "sessionId": "abc123",
                "title": "scout (general-purpose)",
                "description": "Work",
                "state": {"state": "running"}}})]
        );
        // Announcing twice is a no-op.
        assert!(replay.announce("toolu_agent").is_empty());
    }

    /// B-105: a launch without metadata is left out and a notice says so.
    #[test]
    fn a_launch_without_metadata_is_omitted_with_a_notice() {
        let mut replay = RfdSubagentReplay::new(history(&[]));
        replay.scan(&[launch_message("toolu_lost")], "root");
        assert!(replay.child_of_launch("toolu_lost").is_none());
        assert_eq!(replay.omitted().len(), 1);
        let out = replay.finish(true, "root");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["update"],
            json!({
                "sessionUpdate": "notice",
                "severity": "info",
                "title": "Some subagent history is not available",
                "description": "The history of 1 subagent of this session could not be restored.",
            })
        );
        // A client without notices gets none.
        assert!(RfdSubagentReplay::new(history(&[]))
            .finish(false, "root")
            .is_empty());
    }

    /// B-105: every announced child ends before the replay ends, children first
    /// in reverse order.
    #[test]
    fn children_end_before_the_replay_ends() {
        let mut replay = RfdSubagentReplay::new(history(&[
            ("toolu_a", record("agent-a", "a", false, false)),
            ("toolu_b", record("agent-b", "b", false, true)),
        ]));
        replay.scan(
            &[launch_message("toolu_a"), launch_message("toolu_b")],
            "root",
        );
        replay.announce("toolu_a");
        replay.announce("toolu_b");
        let out = replay.finish(false, "root");
        assert_eq!(out.len(), 2);
        // Reverse order, and the stopped child is cancelled.
        assert_eq!(out[0]["update"]["sessionId"], "agent-b");
        assert_eq!(
            out[0]["update"]["state"],
            json!({"state": "idle", "stopReason": "cancelled"})
        );
        assert_eq!(out[1]["update"]["sessionId"], "agent-a");
        assert_eq!(out[1]["update"]["state"], json!({"state": "idle"}));
    }

    /// B-106: a foreground launch's result maps to completed / failed / cancelled.
    #[test]
    fn the_launch_result_maps_to_the_child_end() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        let result = json!({
            "type": "user",
            "uuid": "u1",
            "message": {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_agent",
                "content": "The tool use was interrupted by the user",
                "is_error": true,
            }]},
        });
        replay.scan(&[launch_message("toolu_agent"), result.clone()], "root");
        replay.announce("toolu_agent");
        let out = replay.finish(false, "root");
        assert_eq!(
            out[0]["update"]["state"],
            json!({"state": "idle", "stopReason": "cancelled"})
        );

        // A background launch's result is not the end.
        let mut background = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", true, false),
        )]));
        background.scan(&[launch_message("toolu_agent"), result], "root");
        background.announce("toolu_agent");
        assert_eq!(
            background.finish(false, "root")[0]["update"]["state"],
            json!({"state": "idle"})
        );
    }

    /// B-106: the terminal state of a tool result.
    #[test]
    fn terminal_state_of_a_result() {
        let ok = json!({"is_error": false, "content": "done"});
        assert_eq!(
            replay_subagent_terminal_state(&ok),
            SubagentState::Completed
        );
        let stopped = json!({"is_error": true, "content": "The agent was stopped"});
        assert_eq!(
            replay_subagent_terminal_state(&stopped),
            SubagentState::Cancelled
        );
        let failed = json!({"is_error": true, "content": "boom"});
        assert_eq!(
            replay_subagent_terminal_state(&failed),
            SubagentState::Failed
        );
        // A word inside another word is not a stop.
        let not_stopped = json!({"is_error": true, "content": "unstoppable failure"});
        assert_eq!(
            replay_subagent_terminal_state(&not_stopped),
            SubagentState::Failed
        );
        // Nested content is read too.
        let nested = json!({"is_error": true, "content": [{"text": "killed"}]});
        assert_eq!(
            replay_subagent_terminal_state(&nested),
            SubagentState::Cancelled
        );
    }

    /// B-104: the first text of the child's history is its incoming prompt, with
    /// the launch id; tool results stay.
    #[test]
    fn the_first_text_is_the_child_prompt() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        replay.scan(&[launch_message("toolu_agent")], "root");
        let message = json!({"type": "user", "uuid": "c1", "message": {"role": "user",
            "content": [{"type": "text", "text": "Look around"}]}});
        let (out, rest) =
            replay.child_content("toolu_agent", &message, &message["message"]["content"]);
        assert_eq!(
            out,
            vec![json!({"sessionId": "abc123", "update": {
                "sessionUpdate": "session_message",
                "messageId": "toolu_agent",
                "senderSessionId": "root",
                "recipientSessionId": "abc123",
                "content": [{"type": "text", "text": "Look around"}]}})]
        );
        assert!(rest.is_none());
        // A second text is harness text: no second prompt.
        let again = replay.child_content("toolu_agent", &message, &message["message"]["content"]);
        assert!(again.0.is_empty());
    }

    /// B-104: tool results stay in the child's replay; the text does not.
    #[test]
    fn tool_results_stay_after_the_prompt() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        replay.scan(&[launch_message("toolu_agent")], "root");
        let content = json!([
            {"type": "text", "text": "Look around"},
            {"type": "tool_result", "tool_use_id": "t", "content": "ok"}
        ]);
        let message =
            json!({"type": "user", "uuid": "c1", "message": {"role": "user", "content": content}});
        let (out, rest) = replay.child_content("toolu_agent", &message, &content);
        assert_eq!(out.len(), 1);
        assert_eq!(
            rest,
            Some(json!([{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]))
        );
    }

    /// B-105: an empty child transcript falls back to the launch's prompt.
    #[test]
    fn an_empty_child_transcript_sends_the_launch_prompt() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        replay.scan(&[launch_message("toolu_agent")], "root");
        replay.announce("toolu_agent");
        let out = replay.child_replayed("toolu_agent", 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["update"]["messageId"], "toolu_agent");
        assert_eq!(out[0]["update"]["content"][0]["text"], "Look around");
        assert_eq!(replay.without_history(), 1);
    }

    /// B-104: a SendMessage delivery takes the call's id and sender.
    #[test]
    fn a_delivery_takes_the_send_message_call() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        let send = json!({"type": "assistant", "uuid": "s1", "message": {"role": "assistant",
            "content": [{"type": "tool_use", "id": "toolu_send", "name": "SendMessage",
                "input": {"to": "scout", "message": "Hurry up"}}]}});
        replay.scan(&[launch_message("toolu_agent"), send], "root");
        let message = json!({"type": "user", "uuid": "d1", "origin": {"kind": "coordinator"},
            "message": {"role": "user", "content": [{"type": "text",
                "text": "The coordinator sent a message while you were working: Hurry up"}]}});
        let (out, _) =
            replay.child_content("toolu_agent", &message, &message["message"]["content"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["update"]["messageId"], "toolu_send");
        assert_eq!(out[0]["update"]["senderSessionId"], "root");
        assert_eq!(out[0]["update"]["content"][0]["text"], "Hurry up");
    }

    /// B-105: a persisted task notification ends the child it names.
    #[test]
    fn a_task_notification_ends_its_child() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", true, false),
        )]));
        replay.scan(&[launch_message("toolu_agent")], "root");
        replay.announce("toolu_agent");
        assert!(replay.task_notification("abc123", "killed"));
        assert!(!replay.task_notification("someone-else", "completed"));
        let out = replay.finish(false, "root");
        assert_eq!(
            out[0]["update"]["state"],
            json!({"state": "idle", "stopReason": "cancelled"})
        );
    }

    /// T6FB2 MEDIUM 8: `finish` ends the children in reverse *registration*
    /// order (the upstream `Map` iteration, `subagent-replay.ts:185`), not in
    /// reverse lexicographic launch-id order.
    #[test]
    fn children_end_in_reverse_registration_order() {
        let mut replay = RfdSubagentReplay::new(history(&[
            ("toolu_B", record("agent-b", "b", false, false)),
            ("toolu_A", record("agent-a", "a", false, false)),
        ]));
        // B is launched before A, so its launch id sorts after A's.
        replay.scan(
            &[launch_message("toolu_B"), launch_message("toolu_A")],
            "root",
        );
        replay.announce("toolu_B");
        replay.announce("toolu_A");
        let out = replay.finish(false, "root");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["update"]["sessionId"], "agent-a");
        assert_eq!(out[1]["update"]["sessionId"], "agent-b");
    }

    /// T6FB2 LOW 13: a `tool_result` is the launch end independent of its
    /// `id` field; an `mcp_tool_result` is not (`subagent-replay.ts:82-87`).
    #[test]
    fn a_tool_result_ends_a_launch_even_with_an_id() {
        let mut replay = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        let result = json!({
            "type": "user",
            "uuid": "u1",
            "message": {"role": "user", "content": [{
                "type": "tool_result",
                "id": "block-id",
                "tool_use_id": "toolu_agent",
                "content": "boom",
                "is_error": true,
            }]},
        });
        replay.scan(&[launch_message("toolu_agent"), result], "root");
        replay.announce("toolu_agent");
        assert_eq!(
            replay.finish(false, "root")[0]["update"]["state"],
            json!({"state": "idle", "stopReason": "error"})
        );

        let mut mcp = RfdSubagentReplay::new(history(&[(
            "toolu_agent",
            record("abc123", "scout", false, false),
        )]));
        let mcp_result = json!({
            "type": "user",
            "uuid": "u1",
            "message": {"role": "user", "content": [{
                "type": "mcp_tool_result",
                "tool_use_id": "toolu_agent",
                "content": "boom",
                "is_error": true,
            }]},
        });
        mcp.scan(&[launch_message("toolu_agent"), mcp_result], "root");
        mcp.announce("toolu_agent");
        assert_eq!(
            mcp.finish(false, "root")[0]["update"]["state"],
            json!({"state": "idle"})
        );
    }
}
