//! Output of the work a cancel stopped (F16b: B-18, B-38, B-62).
//!
//! After `session/cancel` Claude Code still emits what the interrupt flushes, and
//! may run a prompt it had already taken off its queue. The client has been told
//! the stopped work ended once no turn is active; from then on, while the cancel
//! window is open, the consumer drops streamed text/thinking/tool uses and the
//! consolidated messages they belong to (`dropsStoppedWorkOutput`,
//! `acp-agent.ts:9703-9712`), and answers a permission request for a dropped tool
//! use with a deny (issue #1061). A background subagent that still runs is not
//! the work the cancel stopped (agentclientprotocol/agent-client-protocol#2308),
//! so its output goes on (`isRunningBackgroundSubagentOutput`).

use std::collections::HashSet;

use serde_json::Value;

use super::{is_held_turn, TurnMachine};

/// `MAX_REACHED_TOOL_USES` (`acp-agent.ts:12870`).
const MAX_REACHED_TOOL_USES: usize = 1000;

/// What the consumer knows about the stopped-work window.
#[derive(Debug, Default)]
pub(super) struct StoppedWorkState {
    /// Streams (`parent_tool_use_id`, `""` for the main one) that lost a delta to
    /// the drop: they keep dropping until the next message starts or a
    /// consolidated message consumes the record (`droppedStreams`).
    dropped_streams: HashSet<String>,
    /// The tool uses the consumer reached in the stream, each with whether it
    /// dropped it as output of the stopped work (`reachedToolUses`), oldest
    /// first. The first sight of a tool use decides.
    reached: Vec<(String, bool)>,
}

/// How to treat a permission request for a tool use in the stopped-work window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedWorkVerdict {
    /// The consumer dropped the tool use as output of the stopped work: deny it.
    Stopped,
    /// Not stopped work: go on with the request.
    Proceed,
    /// The consumer has not reached the tool use yet while a cancel window may
    /// be open: wait (bounded) for it.
    Wait,
}

/// What `task_started` told us about a task (`liveBackgroundTasks` record).
#[derive(Debug, Clone)]
pub(super) struct TaskMeta {
    pub(super) parent_tool_use_id: Option<String>,
    pub(super) backgrounded: bool,
    pub(super) is_subagent: bool,
    pub(super) task_type: Option<String>,
}

/// Whether `block` is a tool-use block (`isToolUseBlock`): `tool_use`,
/// `server_tool_use` or `mcp_tool_use` with a string id.
fn tool_use_id_of(block: &Value) -> Option<&str> {
    match block.get("type").and_then(Value::as_str) {
        Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {
            block.get("id").and_then(Value::as_str)
        }
        _ => None,
    }
}

impl TurnMachine {
    /// Record what a `task_started` frame says about the task: the Agent/Task
    /// call that started it and whether it runs in the background.
    pub fn note_task_meta(
        &mut self,
        task_id: &str,
        tool_use_id: Option<&str>,
        backgrounded: bool,
        is_subagent: bool,
        task_type: Option<&str>,
    ) {
        self.task_meta.insert(
            task_id.to_string(),
            TaskMeta {
                parent_tool_use_id: tool_use_id.map(str::to_string),
                backgrounded,
                is_subagent,
                task_type: task_type.map(str::to_string),
            },
        );
    }

    /// Whether `session/cancel` stops the background workflows the interrupt
    /// spared (`Session.stopsWorkflowsOnCancel`: the client has the per-task stop
    /// affordance and no `asyncTasks` of its own).
    pub fn set_stops_workflows_on_cancel(&mut self, stops: bool) {
        self.stops_workflows_on_cancel = stops;
    }

    /// The live background workflows a cancel stops after its interrupt
    /// (`stopBackgroundWorkflows`, `acp-agent.ts:8419-8430`). Every background
    /// workflow, whoever launched it, because the interrupt killed them all.
    pub fn background_workflows_to_stop(&self) -> Vec<String> {
        if !self.stops_workflows_on_cancel {
            return Vec::new();
        }
        let mut ids: Vec<String> = self
            .task_meta
            .iter()
            .filter(|(_, meta)| meta.task_type.as_deref() == Some("local_workflow"))
            .map(|(task_id, _)| task_id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Start a model cycle that Claude Code started on its own as the foreground
    /// work when `frame` is its first output: model output of the main
    /// conversation (not a subagent's) while no work runs. That includes a turn
    /// that is held for its background subagents, whose work ended at its result,
    /// so its follow-up is a cycle. Output of the work a cancel stopped starts
    /// nothing (`startAutonomousCycle`, `acp-agent.ts:9674-9693`).
    pub fn start_autonomous_cycle(&mut self, frame: &Value) {
        if self.autonomous_cycle || self.cancelled {
            return;
        }
        if !matches!(
            frame.get("type").and_then(Value::as_str),
            Some("assistant" | "stream_event")
        ) || frame.get("parent_tool_use_id") != Some(&Value::Null)
        {
            return;
        }
        // A turn that Claude Code started is the foreground work already.
        let turn_work_running = self
            .active
            .as_ref()
            .is_some_and(|t| !t.settled && !is_held_turn(t))
            || self.queue.iter().any(|t| !t.settled && t.command_started);
        if turn_work_running {
            return;
        }
        // Without lifecycle frames a queued turn's output can come before its
        // echo, so output while a turn is queued may be that turn's. With them, a
        // turn that Claude Code has not started has no output yet.
        let queued = self.queue.iter().any(|t| !t.settled);
        if queued && !self.msg_lifecycle_v1 {
            return;
        }
        self.autonomous_cycle = true;
    }

    /// Whether a model cycle that Claude Code started on its own is the
    /// session's foreground work (`runsAutonomousCycle`).
    pub fn runs_autonomous_cycle(&self) -> bool {
        self.autonomous_cycle
    }

    /// Whether the client gets the text/thinking of a subagent
    /// (`forwardsSubagentText`); decides which dropped streams latch.
    pub fn set_forwards_subagent_text(&mut self, forwards: bool) {
        self.forwards_subagent_text = forwards;
    }

    /// Whether a cancel is pending: the window after it is open until a turn
    /// activates or new work starts (`session.cancelled`).
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Whether the client has been told that the session's work ended: no turn is
    /// active and no model cycle Claude Code started on its own runs
    /// (`workEndReported`, v1 form).
    pub fn work_end_reported(&self) -> bool {
        !self.has_active() && !self.autonomous_cycle
    }

    /// Whether the consumer drops what it reaches now as output of the work a
    /// cancel stopped (`dropsStoppedWorkOutput`).
    pub fn drops_stopped_work_output(&self) -> bool {
        self.cancelled && self.work_end_reported()
    }

    /// Whether the subagent the Agent call `agent_tool_use_id` started runs in
    /// the background and has not ended (`runsInBackground`).
    pub fn runs_in_background(&self, agent_tool_use_id: &str) -> bool {
        self.task_meta.iter().any(|(task_id, meta)| {
            meta.is_subagent
                && meta.backgrounded
                && meta.parent_tool_use_id.as_deref() == Some(agent_tool_use_id)
                && self.live_subagents.contains_key(task_id)
        })
    }

    /// Whether output of the conversation `parent` (`None` for the main one) is a
    /// background subagent's that still runs
    /// (`isRunningBackgroundSubagentOutput`).
    pub fn is_running_background_subagent_output(&self, parent: Option<&str>) -> bool {
        parent.is_some_and(|parent| self.runs_in_background(parent))
    }

    /// Records that the consumer reached the tool use `tool_use_id`, and whether
    /// it dropped it as output of the stopped work (`recordReachedToolUse`). The
    /// first record stays.
    pub fn record_reached_tool_use(&mut self, tool_use_id: &str, dropped: bool) {
        let reached = &mut self.stopped.reached;
        if reached.iter().any(|(id, _)| id == tool_use_id) {
            return;
        }
        reached.push((tool_use_id.to_string(), dropped));
        if reached.len() > MAX_REACHED_TOOL_USES {
            reached.remove(0);
        }
    }

    /// Whether the consumer dropped the tool use as stopped work, or `None` when
    /// it has not reached it yet (`reachedToolUses.get`).
    pub fn reached_tool_use(&self, tool_use_id: &str) -> Option<bool> {
        self.stopped
            .reached
            .iter()
            .find(|(id, _)| id == tool_use_id)
            .map(|(_, dropped)| *dropped)
    }

    /// What a permission request (or preview hook) for `tool_use_id` should do
    /// (`isStoppedWorkToolUse`, `acp-agent.ts:9733-9742`): a tool use the consumer
    /// already reached decides at once; one it has not reached yet is awaited
    /// while a cancel's window may be open (Claude Code calls `canUseTool` on its
    /// control channel after it streamed the tool use, and the consumer can lag
    /// behind, still seeing a cancelled session while the next prompt's echo — which
    /// ends the window — is still ahead of the tool use). `waited` is set once the
    /// wait is over (the tool use was reached, or the bound ran out).
    pub fn stopped_work_verdict(&self, tool_use_id: &str, waited: bool) -> StoppedWorkVerdict {
        match self.reached_tool_use(tool_use_id) {
            Some(true) => StoppedWorkVerdict::Stopped,
            Some(false) => StoppedWorkVerdict::Proceed,
            None if self.cancelled && !waited => StoppedWorkVerdict::Wait,
            None => StoppedWorkVerdict::Proceed,
        }
    }

    /// A new message starts on the stream of `parent`: its drop latch resets with
    /// the streamed-content record (`droppedStreams.delete` at `message_start`).
    pub fn note_stream_message_start(&mut self, parent: Option<&str>) {
        self.stopped
            .dropped_streams
            .remove(parent.unwrap_or_default());
    }

    /// The gate for a `stream_event` frame, run after its usage bookkeeping
    /// (`acp-agent.ts:7600-7628`). Returns `true` when the event is dropped:
    /// output of the stopped work (once the client was told the work ended), or
    /// the rest of a stream that already lost a delta to it.
    pub fn gate_stream_event(&mut self, line: &Value) -> bool {
        let top_level = line.get("parent_tool_use_id") == Some(&Value::Null);
        let parent = line.get("parent_tool_use_id").and_then(Value::as_str);
        let key = parent.unwrap_or_default().to_string();
        let stopped =
            self.drops_stopped_work_output() && !self.is_running_background_subagent_output(parent);
        if line.pointer("/event/type").and_then(Value::as_str) == Some("content_block_start") {
            if let Some(id) = line
                .pointer("/event/content_block")
                .and_then(tool_use_id_of)
            {
                self.record_reached_tool_use(id, stopped);
            }
        }
        if stopped {
            if top_level || self.forwards_subagent_text {
                self.stopped.dropped_streams.insert(key);
            }
            return true;
        }
        self.stopped.dropped_streams.contains(&key)
    }

    /// The gate for an `assistant` frame (`acp-agent.ts:7700-7831`): records the
    /// tool uses it carries as reached, then reports whether the whole message is
    /// dropped — everything a cancelled session's stopped work sends, except a
    /// running background subagent's output.
    pub fn gate_assistant(&mut self, line: &Value) -> bool {
        let parent = line.get("parent_tool_use_id").and_then(Value::as_str);
        let bg = self.is_running_background_subagent_output(parent);
        let stopped = self.drops_stopped_work_output() && !bg;
        if let Some(blocks) = line.pointer("/message/content").and_then(Value::as_array) {
            for id in blocks.iter().filter_map(tool_use_id_of) {
                self.record_reached_tool_use(id, stopped);
            }
        }
        self.cancelled && !bg
    }

    /// Whether a non-echo `user` frame (the tool results of the stopped work) is
    /// dropped: once the client was told the work ended, nothing that work sends
    /// reaches it, except a running background subagent's messages
    /// (`acp-agent.ts:7818-7831`). Before that report Node closes the open tool
    /// calls with interrupted results (`sendInterruptedToolResults`, F17).
    pub fn drops_stopped_work_user_frame(&self, line: &Value) -> bool {
        let parent = line.get("parent_tool_use_id").and_then(Value::as_str);
        self.drops_stopped_work_output() && !self.is_running_background_subagent_output(parent)
    }

    /// A consolidated message consumed its stream's streamed-content record: the
    /// stream's drop latch resets with it (`droppedStreams.delete`).
    pub fn note_consolidated_consumed(&mut self, line: &Value) {
        let top_level = line.get("parent_tool_use_id") == Some(&Value::Null);
        if top_level || self.forwards_subagent_text {
            let parent = line.get("parent_tool_use_id").and_then(Value::as_str);
            self.stopped
                .dropped_streams
                .remove(parent.unwrap_or_default());
        }
    }

    /// Forget the per-task facts of an ended task.
    pub(super) fn forget_task_meta(&mut self, task_id: &str) {
        self.task_meta.remove(task_id);
    }
}
