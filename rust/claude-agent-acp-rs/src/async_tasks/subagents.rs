//! The native subagent half of the task stream (`acp-agent.ts:6436-6580`).
//!
//! `AsyncTaskRuntime` ignores subagent tasks (`isSubagentTask`); Node feeds the
//! same `task_started` / `task_notification` / `task_updated` frames to
//! `NativeSubagentRuntime` first. The runtime lives inside `AsyncTaskRuntime` so
//! the session actor that already owns the task runtime gets it for free; the
//! methods below return the ordered publications ([`Out`]) for the actor to send.

use std::collections::VecDeque;

use serde_json::Value;

use super::AsyncTaskRuntime;
use crate::acp_subagents::{SubagentForm, SubagentState};
use crate::native_subagents::{NativeSubagentRuntime, Out, RouteResult, TaskEnd, TaskStarted};

/// The sends [`AsyncTaskRuntime::publish_subagent_outs`] performs, in order.
///
/// The session supplies them from its `sendUpdate` (`acp-agent.ts:4551-4633`):
/// `publish` is `client.sessionUpdate`, `deliver` is the rest of `sendUpdate`
/// after its `route`, and `eager_session_of` is the lookup that `sendUpdate`
/// passes to `route` as the forced session. Splitting them keeps the routing
/// (and the order of the publications a route makes) in the runtime, where the
/// state it reads lives.
pub struct SubagentSends<'a> {
    /// `client.sessionUpdate` for a notification the runtime publishes as it is.
    pub publish: &'a mut dyn FnMut(Value) -> Result<(), String>,
    /// The post-routing tail of `sendUpdate` for a routed notification: the
    /// tool-call fields and titles, the foreground bookkeeping, and the eager
    /// session cleanup. It does not route again.
    pub deliver: &'a mut dyn FnMut(Value) -> Result<(), String>,
    /// `session.eagerToolCallSessions.get(toolCallId)`: the session that a
    /// permission request created the tool call in, before the runtime routed it.
    pub eager_session_of: &'a mut dyn FnMut(&str) -> Option<String>,
}

/// `sendUpdate`'s `toolCallId` (`acp-agent.ts:4555-4558`): only a tool call or
/// tool call update has one, and an absent id is no lookup at all.
fn tool_call_id(notification: &Value) -> Option<&str> {
    let update = notification.get("update")?;
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("tool_call" | "tool_call_update") => update.get("toolCallId").and_then(Value::as_str),
        _ => None,
    }
}

impl AsyncTaskRuntime {
    /// Installs the native subagent runtime for a client that gets native
    /// subagent sessions (`form`), rooted at `root_session_id`.
    #[must_use]
    pub fn with_subagents(mut self, runtime: NativeSubagentRuntime) -> Self {
        self.subagents = runtime;
        self
    }

    /// Convenience over [`Self::with_subagents`] without history lookups.
    #[must_use]
    pub fn with_subagent_form(self, form: Option<SubagentForm>, root_session_id: &str) -> Self {
        self.with_subagents(NativeSubagentRuntime::new(form, root_session_id))
    }

    /// The native subagent runtime (routing, cancel and request hooks).
    pub fn subagents(&mut self) -> &mut NativeSubagentRuntime {
        &mut self.subagents
    }

    /// `subagents.taskStarted(...)` for a `system/task_started` frame.
    pub fn subagent_task_started(&mut self, message: &Value) -> Vec<Out> {
        match TaskStarted::from_message(message) {
            Some(task) => self.subagents.task_started(&task),
            None => Vec::new(),
        }
    }

    /// The `task_notification` half: stale ends are ignored, the end is reported
    /// with the SDK's `reason`, and the child's buffered updates are discarded.
    pub fn subagent_task_notification(&mut self, message: &Value) -> Vec<Out> {
        let task_id = message.get("task_id").and_then(Value::as_str).unwrap_or("");
        let tool_use_id = message
            .get("tool_use_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if self.subagents.is_stale_end(task_id, tool_use_id) {
            return Vec::new();
        }
        let status = message.get("status").and_then(Value::as_str).unwrap_or("");
        let end = TaskEnd {
            reason: message
                .get("reason")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            ..TaskEnd::default()
        };
        let outs = self
            .subagents
            .finish_task(task_id, status, tool_use_id, Some(&end));
        if let Some(id) = tool_use_id {
            self.subagents.discard_pending(id);
        }
        outs
    }

    /// The `task_updated` half: a terminal status ends the child (a failure
    /// carries the patch's `error`); a `running`/`pending` patch resumes a
    /// finished child with the SendMessage text `resume_prompt` (from
    /// `send_message_resume_prompt` over the session's tool-use cache).
    pub fn subagent_task_updated(
        &mut self,
        task_id: &str,
        patch: &Value,
        resume_prompt: Option<&str>,
    ) -> Vec<Out> {
        match patch.get("status").and_then(Value::as_str) {
            Some(status @ ("completed" | "failed" | "killed")) => {
                let end = TaskEnd {
                    error: (status == "failed")
                        .then(|| patch.get("error").and_then(Value::as_str))
                        .flatten()
                        .map(ToString::to_string),
                    ..TaskEnd::default()
                };
                self.subagents
                    .finish_task(task_id, status, None, Some(&end))
            }
            Some("running" | "pending") => self.subagents.task_resumed(task_id, resume_prompt),
            _ => Vec::new(),
        }
    }

    /// `finishAll` of the subagents at stream end / reset (`finishLifecycle`,
    /// `acp-agent.ts:4637-4660`): each child that still runs is closed with
    /// `state`. Every [`Out`] is handed to `sends`; a failed publication does not
    /// stop the rest, and the messages are returned (Node aggregates them).
    pub fn finish_all_subagents_with(
        &mut self,
        state: SubagentState,
        sends: SubagentSends<'_>,
    ) -> Vec<String> {
        let outs = self.subagents.finish_all(state);
        self.publish_subagent_outs(outs, sends)
    }

    /// Sends a list of [`Out`] through `sends`, in order (see
    /// [`SubagentSends`] for the two sends).
    ///
    /// An [`Out::Deliver`] is a buffered update that Node hands to `deliver` =
    /// `sendUpdate`, which routes it again. Node's `routeUpdate` awaits the
    /// publications its route makes (the announce of a nested child and the
    /// child's own buffered updates) before it returns the routed notification
    /// (`native-subagents.ts:244-246`), so those go first here too: the routed
    /// value is queued behind them, never sent in the same step.
    pub fn publish_subagent_outs(
        &mut self,
        outs: Vec<Out>,
        sends: SubagentSends<'_>,
    ) -> Vec<String> {
        let SubagentSends {
            publish,
            deliver,
            eager_session_of,
        } = sends;
        let mut errors = Vec::new();
        let mut queue: VecDeque<Out> = outs.into();
        while let Some(out) = queue.pop_front() {
            match out {
                Out::Publish(notification) => {
                    if let Err(error) = publish(notification) {
                        errors.push(error);
                    }
                }
                Out::Routed(notification) => {
                    // Routed by the runtime already (the end of a child): only
                    // the post-routing tail of `sendUpdate` is left to do.
                    if let Err(error) = deliver(notification) {
                        errors.push(error);
                    }
                }
                Out::Deliver(notification) => {
                    let forced = tool_call_id(&notification).and_then(&mut *eager_session_of);
                    let RouteResult { outs, routed } =
                        self.subagents.route(&notification, forced.as_deref());
                    if let Some(routed) = routed {
                        queue.push_front(Out::Routed(routed));
                    }
                    for extra in outs.into_iter().rev() {
                        queue.push_front(extra);
                    }
                }
            }
        }
        errors
    }
}
