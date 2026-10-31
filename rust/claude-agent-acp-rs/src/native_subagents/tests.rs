//! Ports of `src/tests/native-subagents.test.ts` onto the synchronous runtime.
//! Node's raced single-flight cases (`announceNativeSubagent` called twice
//! concurrently) have no analogue: the runtime cannot overlap its own calls.

use serde_json::{json, Value};

use super::runtime::{HistoricalSubagent, LaunchedAgentFn, LineageFn};
use super::*;
use crate::acp_subagents::{SubagentForm, SubagentState};

struct H {
    rt: NativeSubagentRuntime,
    published: Vec<Value>,
    delivered: Vec<Value>,
}

impl H {
    fn new(form: Option<SubagentForm>) -> Self {
        Self {
            rt: NativeSubagentRuntime::new(form, "root"),
            published: Vec::new(),
            delivered: Vec::new(),
        }
    }
    fn with_history(
        form: SubagentForm,
        lineage: LineageFn,
        launched: Option<LaunchedAgentFn>,
    ) -> Self {
        Self {
            rt: NativeSubagentRuntime::new(Some(form), "root")
                .with_history(Some(lineage), launched),
            published: Vec::new(),
            delivered: Vec::new(),
        }
    }
    fn apply(&mut self, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Publish(v) => self.published.push(v),
                Out::Deliver(v) | Out::Routed(v) => self.delivered.push(v),
            }
        }
    }
    /// Applies the publications the way the session's publish loop does
    /// (`publish_subagent_outs`): a `Deliver` is routed again, what its route
    /// publishes goes first, and only then the routed value; a `Routed` value
    /// goes straight to the delivery path.
    fn apply_live(&mut self, outs: Vec<Out>) {
        let mut queue: std::collections::VecDeque<Out> = outs.into();
        while let Some(out) = queue.pop_front() {
            match out {
                Out::Publish(v) => self.published.push(v),
                Out::Routed(v) => self.delivered.push(v),
                Out::Deliver(v) => {
                    let r = self.rt.route(&v, None);
                    if let Some(routed) = r.routed {
                        queue.push_front(Out::Routed(routed));
                    }
                    for extra in r.outs.into_iter().rev() {
                        queue.push_front(extra);
                    }
                }
            }
        }
    }
    fn route(&mut self, n: &Value) -> Option<Value> {
        let r = self.rt.route(n, None);
        self.apply(r.outs);
        r.routed
    }
    fn started(&mut self, t: TaskStarted) {
        let o = self.rt.task_started(&t);
        self.apply(o);
    }
    fn finish(&mut self, task: &str, status: &str, tool: Option<&str>) {
        let o = self.rt.finish_task(task, status, tool, None);
        self.apply(o);
    }
    fn resumed(&mut self, task: &str, prompt: Option<&str>) {
        let o = self.rt.task_resumed(task, prompt);
        self.apply(o);
    }
    fn send_resumed(&mut self, task: &str, prompt: Option<&str>, tool: Option<&str>) {
        let o = self.rt.send_message_resumed(task, prompt, tool);
        self.apply(o);
    }
    fn kinds(&self) -> Vec<String> {
        self.published
            .iter()
            .map(|n| n["update"]["sessionUpdate"].as_str().unwrap().to_string())
            .collect()
    }
    fn states(&self) -> Vec<Value> {
        self.published
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "subagent_update")
            .filter_map(|n| n["update"].get("state").cloned())
            .collect()
    }
}

fn task(id: &str, tool: &str, kind: Option<&str>, description: Option<&str>) -> TaskStarted {
    TaskStarted {
        task_id: id.into(),
        tool_use_id: Some(tool.into()),
        subagent_type: kind.map(Into::into),
        description: description.map(Into::into),
        prompt: None,
    }
}

fn control(kind: &str, status: &str, tool_call_id: &str, parent: Option<&str>) -> Value {
    let mut claude = json!({"toolName": "Agent"});
    if let Some(p) = parent {
        claude["parentToolUseId"] = json!(p);
    }
    json!({"sessionId": "root", "update": {
        "sessionUpdate": kind, "toolCallId": tool_call_id, "title": "Investigate failure",
        "status": status,
        "rawInput": {"description": "Investigate failure", "prompt": "Find the cause"},
        "_meta": {"claudeCode": claude, "jetbrains": {"air": {"version": 1, "subagent": true}}}}})
}

fn output(parent: &str, text: &str) -> Value {
    json!({"sessionId": "root", "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text},
        "_meta": {"claudeCode": {"parentToolUseId": parent}}}})
}

fn session_of(n: &Option<Value>) -> Option<String> {
    n.as_ref()
        .map(|v| v["sessionId"].as_str().unwrap().to_string())
}

// ----- pure helpers ----------------------------------------------------------

#[test]
fn child_end_state_mapping() {
    assert_eq!(
        native_subagent_state("completed"),
        Some(SubagentState::Completed)
    );
    assert_eq!(native_subagent_state("failed"), Some(SubagentState::Failed));
    assert_eq!(
        native_subagent_state("disconnected"),
        Some(SubagentState::Disconnected)
    );
    for s in ["killed", "cancelled", "stopped"] {
        assert_eq!(native_subagent_state(s), Some(SubagentState::Cancelled));
    }
    assert_eq!(native_subagent_state("running"), None);
    let v = |s, e: Option<&TaskEnd>| rfd_end_state(s, e).to_value();
    assert_eq!(
        v(SubagentState::Completed, None),
        json!({"state": "idle", "stopReason": "end_turn"})
    );
    assert_eq!(
        v(SubagentState::Cancelled, None),
        json!({"state": "idle", "stopReason": "cancelled"})
    );
    assert_eq!(
        v(SubagentState::Disconnected, None),
        json!({"state": "unknown"})
    );
    assert_eq!(
        v(SubagentState::Failed, None),
        json!({"state": "idle", "stopReason": "error"})
    );
    let err = TaskEnd {
        error: Some("boom".into()),
        ..TaskEnd::default()
    };
    assert_eq!(
        v(SubagentState::Failed, Some(&err)),
        json!({"state": "idle", "stopReason": "error", "error": {"code": -32603, "message": "boom"}})
    );
    let unexplained = TaskEnd {
        unexplained: true,
        ..TaskEnd::default()
    };
    assert_eq!(
        v(SubagentState::Completed, Some(&unexplained)),
        json!({"state": "idle"})
    );
    let restart = TaskEnd {
        reason: Some("worker_restart".into()),
        ..TaskEnd::default()
    };
    assert_eq!(
        v(SubagentState::Failed, Some(&restart)),
        json!({"state": "idle"})
    );
}

#[test]
fn display_labels() {
    assert_eq!(
        subagent_display_name(Some(" n "), Some("d"), Some("t"), "id"),
        "n"
    );
    assert_eq!(
        subagent_display_name(Some(" "), Some("d"), Some("t"), "id"),
        "d"
    );
    assert_eq!(
        subagent_display_name(None, None, Some("Explore"), "id"),
        "Explore"
    );
    assert_eq!(
        subagent_display_name(None, None, None, "abcdefghijkl"),
        "Agent efghijkl"
    );
    assert_eq!(
        subagent_display_name(None, None, None, "short"),
        "Agent short"
    );
    assert_eq!(subagent_description(Some("p"), Some("d")), "p");
    assert_eq!(subagent_description(Some("  "), Some(" d ")), "d");
    assert_eq!(subagent_description(None, None), "Delegated task");
    let l = rfd_labels(Some("builder"), Some("Explore"), Some("Fix"));
    assert_eq!(l.title.as_deref(), Some("builder (Explore)"));
    assert_eq!(l.role.as_deref(), Some("Explore"));
    assert_eq!(l.description.as_deref(), Some("Fix"));
    // A role equal to the name, or no name, gives the bare label.
    assert_eq!(
        rfd_labels(Some("Explore"), Some("Explore"), None)
            .title
            .as_deref(),
        Some("Explore")
    );
    assert_eq!(
        rfd_labels(None, Some("Explore"), None).title.as_deref(),
        Some("Explore")
    );
    assert_eq!(
        rfd_labels(Some("n"), None, None).title.as_deref(),
        Some("n")
    );
    assert_eq!(rfd_labels(None, None, Some(" ")), RfdLabels::default());
}

#[test]
fn control_tool_detection() {
    assert!(is_native_subagent_control_tool(Some("Agent")));
    assert!(is_native_subagent_control_tool(Some("Task")));
    assert!(!is_native_subagent_control_tool(Some("SendMessage")));
    assert!(!is_native_subagent_control_tool(None));
    let by_name =
        json!({"sessionUpdate": "tool_call", "_meta": {"claudeCode": {"toolName": "Task"}}});
    assert!(is_native_subagent_control_update(&by_name));
    let by_air = json!({"sessionUpdate": "tool_call_update",
        "_meta": {"jetbrains": {"air": {"subagent": true}}}});
    assert!(is_native_subagent_control_update(&by_air));
    let other =
        json!({"sessionUpdate": "tool_call", "_meta": {"claudeCode": {"toolName": "Bash"}}});
    assert!(!is_native_subagent_control_update(&other));
    let chunk = json!({"sessionUpdate": "agent_message_chunk", "_meta": {"claudeCode": {"toolName": "Agent"}}});
    assert!(!is_native_subagent_control_update(&chunk));
    let content = json!([
        {"type": "tool_use", "id": "a", "name": "Agent"},
        {"type": "tool_use", "id": "b", "name": "Bash"},
        {"type": "text", "id": "c", "name": "Task"},
        {"type": "tool_use", "id": "d", "name": "Task"}
    ]);
    assert_eq!(subagent_launch_ids(&content), vec!["a", "d"]);
    assert!(subagent_launch_ids(&json!("x")).is_empty());
}

fn uses(entries: &[(&str, &str, Value)]) -> Vec<(String, ToolUseRecord)> {
    entries
        .iter()
        .map(|(id, name, input)| {
            (
                (*id).to_string(),
                ToolUseRecord {
                    name: (*name).to_string(),
                    input: input.clone(),
                },
            )
        })
        .collect()
}

#[test]
fn finds_the_send_message_text_that_resumed_the_agent() {
    let tu = uses(&[
        (
            "old",
            "SendMessage",
            json!({"to": "worker-1", "message": "First"}),
        ),
        (
            "other",
            "SendMessage",
            json!({"to": "worker-2", "message": "Other"}),
        ),
        (
            "latest",
            "SendMessage",
            json!({"to": "worker-1", "message": "Second"}),
        ),
        ("bash", "Bash", json!({"command": "ls"})),
    ]);
    assert_eq!(
        send_message_resume_prompt(&tu, "worker-1", &[]).as_deref(),
        Some("Second")
    );
    assert_eq!(
        send_message_resume_prompt(&tu, "worker-1", &["old".into()]).as_deref(),
        Some("First")
    );
    assert_eq!(
        send_message_resume_prompt(&tu, "worker-1", &["bash".into()]),
        None
    );
    assert_eq!(send_message_resume_prompt(&tu, "worker-3", &[]), None);
}

#[test]
fn names_the_send_message_call_even_without_a_text() {
    let tu = uses(&[
        (
            "blank",
            "SendMessage",
            json!({"to": "worker-1", "message": " "}),
        ),
        ("bash", "Bash", json!({"command": "ls"})),
    ]);
    let blank = SendMessageResume {
        tool_use_id: "blank".into(),
        text: None,
    };
    assert_eq!(
        send_message_resume(&tu, "worker-1", &["blank".into()]),
        Some(blank.clone())
    );
    assert_eq!(send_message_resume(&tu, "worker-1", &[]), Some(blank));
    assert_eq!(send_message_resume(&tu, "worker-1", &["bash".into()]), None);
    assert_eq!(
        send_message_resume_prompt(&tu, "worker-1", &["blank".into()]),
        None
    );
}

#[test]
fn resumed_agent_id_needs_success() {
    let ok = json!({"success": true, "message": "Resuming agent worker-1", "resumedAgentId": "worker-1"});
    assert_eq!(resumed_native_subagent_id(&ok).as_deref(), Some("worker-1"));
    assert_eq!(
        resumed_native_subagent_id(&json!({"success": false, "resumedAgentId": "worker-1"})),
        None
    );
    assert_eq!(resumed_native_subagent_id(&json!("x")), None);
}

// ----- AIR form ----------------------------------------------------------------

fn air_with_launch(tool: &str) -> H {
    let mut h = H::new(Some(SubagentForm::Air));
    assert_eq!(h.route(&control("tool_call", "pending", tool, None)), None);
    h
}

#[test]
fn air_falls_back_to_one_ordinary_failed_tool_call_when_no_child_starts() {
    let mut h = H::new(Some(SubagentForm::Air));
    assert_eq!(
        h.route(&control("tool_call", "pending", "agent-tool", None)),
        None
    );
    let fallback = h
        .route(&control("tool_call_update", "failed", "agent-tool", None))
        .unwrap();
    assert_eq!(fallback["sessionId"], "root");
    let u = &fallback["update"];
    assert_eq!(u["sessionUpdate"], "tool_call");
    assert_eq!(u["toolCallId"], "agent-tool");
    assert_eq!(u["title"], "Investigate failure");
    assert_eq!(u["status"], "failed");
    assert_eq!(u["_meta"]["claudeCode"]["toolName"], "Agent");
    assert!(u["_meta"]["jetbrains"]["air"].get("subagent").is_none());
}

#[test]
fn air_failed_fallback_without_an_initial_call_is_a_tool_call() {
    let mut h = H::new(Some(SubagentForm::Air));
    let mut failed = control("tool_call_update", "failed", "agent-tool", None);
    failed["update"]["title"] = json!("");
    let fallback = h.route(&failed).unwrap();
    assert_eq!(fallback["update"]["sessionUpdate"], "tool_call");
    assert_eq!(fallback["update"]["name"], "Agent");
    assert_eq!(fallback["update"]["title"], "Agent");
    assert_eq!(fallback["update"]["status"], "failed");
}

#[test]
fn air_publishes_spawn_and_terminal_in_order_with_the_prompt() {
    let mut h = air_with_launch("agent-tool");
    h.started(task(
        "worker-1",
        "agent-tool",
        Some("Explore"),
        Some("Investigate failure"),
    ));
    h.finish("worker-1", "completed", None);
    assert_eq!(h.kinds(), ["subagent_spawned", "subagent_state_update"]);
    assert_eq!(
        h.published[0],
        json!({"sessionId": "root", "update": {
            "sessionUpdate": "subagent_spawned", "subagentSessionId": "worker-1",
            "name": "Investigate failure", "task": "Find the cause",
            "prompt": "Find the cause", "capabilities": {}}})
    );
    assert_eq!(h.published[1]["update"]["state"], "completed");
}

fn spawned_for(task_prompt: Option<&str>, raw_input: Value) -> Value {
    let mut h = H::new(Some(SubagentForm::Air));
    let mut launch = control("tool_call", "pending", "agent-tool", None);
    launch["update"]["rawInput"] = raw_input;
    h.route(&launch);
    let mut t = task(
        "worker-1",
        "agent-tool",
        Some("Explore"),
        Some("Investigate failure"),
    );
    t.prompt = task_prompt.map(Into::into);
    h.started(t);
    h.published
        .iter()
        .find(|n| n["update"]["sessionUpdate"] == "subagent_spawned")
        .unwrap()["update"]
        .clone()
}

#[test]
fn spawn_prompt_is_the_exact_task_started_prompt() {
    let raw = json!({"description": "Investigate failure", "prompt": "Find the cause"});
    let u = spawned_for(Some("  Trace the crash.\n\nReport the stack.\n"), raw);
    assert_eq!(u["prompt"], "  Trace the crash.\n\nReport the stack.\n");
}

#[test]
fn spawn_prompt_falls_back_to_the_agent_input() {
    let raw = json!({"description": "Investigate failure", "prompt": "Find the cause"});
    assert_eq!(spawned_for(None, raw)["prompt"], "Find the cause");
}

#[test]
fn spawn_prompt_is_omitted_when_absent() {
    let u = spawned_for(Some("   "), json!({"description": "Investigate failure"}));
    assert!(u.get("prompt").is_none());
    assert_eq!(u["task"], "Investigate failure");
}

#[test]
fn air_new_generation_when_the_sdk_restarts_the_same_task_id() {
    let mut h = H::new(Some(SubagentForm::Air));
    h.route(&control("tool_call", "pending", "launch-1", None));
    h.started(task(
        "worker-1",
        "launch-1",
        Some("Explore"),
        Some("First run"),
    ));
    assert_eq!(
        session_of(&h.route(&output("launch-1", "first"))).as_deref(),
        Some("worker-1")
    );
    h.finish("worker-1", "completed", None);
    h.route(&control("tool_call", "pending", "launch-2", None));
    h.started(task(
        "worker-1",
        "launch-2",
        Some("Explore"),
        Some("Second run"),
    ));
    assert_eq!(
        session_of(&h.route(&output("launch-2", "second"))).as_deref(),
        Some("worker-1:generation:2")
    );
    h.finish("worker-1", "failed", Some("launch-1"));
    let terminals = h
        .kinds()
        .iter()
        .filter(|k| *k == "subagent_state_update")
        .count();
    assert_eq!(terminals, 1);
    assert_eq!(
        session_of(&h.route(&output("launch-2", "still running"))).as_deref(),
        Some("worker-1:generation:2")
    );
    assert_eq!(h.route(&output("launch-1", "late")), None);
    assert!(h
        .rt
        .take_logs()
        .iter()
        .any(|l| l.contains("ignoring late update for terminal subagent")));
    h.finish("worker-1", "completed", Some("launch-2"));
    let ids = |kind: &str, key: &str| -> Vec<String> {
        h.published
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == kind)
            .map(|n| n["update"][key].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids("subagent_spawned", "subagentSessionId"),
        ["worker-1", "worker-1:generation:2"]
    );
    assert_eq!(
        ids("subagent_state_update", "subagentSessionId"),
        ["worker-1", "worker-1:generation:2"]
    );
}

#[test]
fn air_send_message_restart_is_announced_immediately() {
    let mut h = air_with_launch("launch-1");
    h.started(task(
        "worker-1",
        "launch-1",
        Some("Explore"),
        Some("First run"),
    ));
    h.finish("worker-1", "completed", Some("launch-1"));
    assert_eq!(h.kinds(), ["subagent_spawned", "subagent_state_update"]);
    h.started(task(
        "worker-1",
        "launch-1",
        Some("Explore"),
        Some("Resumed run"),
    ));
    let spawned: Vec<_> = h
        .published
        .iter()
        .filter(|n| n["update"]["sessionUpdate"] == "subagent_spawned")
        .map(|n| {
            n["update"]["subagentSessionId"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(spawned, ["worker-1", "worker-1:generation:2"]);
    h.finish("worker-1", "completed", Some("launch-1"));
    assert_eq!(h.kinds().last().unwrap(), "subagent_state_update");
    assert_eq!(
        h.published.last().unwrap()["update"]["subagentSessionId"],
        "worker-1:generation:2"
    );
}

fn finished_worker(state: &str) -> H {
    let mut h = air_with_launch("launch-1");
    h.started(task(
        "worker-1",
        "launch-1",
        Some("Explore"),
        Some("First run"),
    ));
    h.finish("worker-1", state, None);
    h
}

#[test]
fn air_opens_a_new_generation_when_a_failed_child_runs_again() {
    let mut h = finished_worker("failed");
    h.resumed("worker-1", None);
    assert_eq!(
        session_of(&h.route(&output("launch-1", "resumed"))).as_deref(),
        Some("worker-1:generation:2")
    );
    h.resumed("worker-1", None);
    h.finish("worker-1", "completed", None);
    let lifecycle: Vec<(String, String)> = h
        .published
        .iter()
        .map(|n| {
            (
                n["update"]["sessionUpdate"].as_str().unwrap().to_string(),
                n["update"]["subagentSessionId"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(
        lifecycle,
        [
            ("subagent_spawned".to_string(), "worker-1".to_string()),
            ("subagent_state_update".into(), "worker-1".into()),
            ("subagent_spawned".into(), "worker-1:generation:2".into()),
            (
                "subagent_state_update".into(),
                "worker-1:generation:2".into()
            ),
        ]
    );
    assert_eq!(h.published[2]["update"]["name"], "Investigate failure");
    assert_eq!(h.published[2]["update"]["task"], "Find the cause");
}

#[test]
fn air_resume_prompt_is_the_send_message_text() {
    let mut h = finished_worker("completed");
    h.resumed("worker-1", Some("Check the tests too"));
    let last = h.published.last().unwrap();
    assert_eq!(last["update"]["sessionUpdate"], "subagent_spawned");
    assert_eq!(last["update"]["subagentSessionId"], "worker-1:generation:2");
    assert_eq!(last["update"]["prompt"], "Check the tests too");
    let mut h = finished_worker("completed");
    h.resumed("worker-1", None);
    assert!(h.published.last().unwrap()["update"]
        .get("prompt")
        .is_none());
}

#[test]
fn air_keeps_work_of_a_child_tool_call_in_its_generation_and_async_tasks_reach_it_late() {
    let mut h = air_with_launch("launch-1");
    h.started(task("worker-1", "launch-1", Some("Explore"), Some("run")));
    let call = json!({"sessionId": "root", "update": {
        "sessionUpdate": "tool_call", "toolCallId": "bash-1", "title": "Bash",
        "_meta": {"claudeCode": {"toolName": "Bash", "parentToolUseId": "launch-1"}}}});
    assert_eq!(session_of(&h.route(&call)).as_deref(), Some("worker-1"));
    h.finish("worker-1", "completed", None);
    let route =
        h.rt.route_of_tool_call("bash-1", None)
            .expect("child owns the call");
    let late_task = json!({"sessionId": "root", "update": {"sessionUpdate": "async_task_progress", "asyncTaskId": "t"}});
    let routed = h.rt.apply_route(route.clone(), &late_task).unwrap();
    assert_eq!(routed["sessionId"], "worker-1");
    let late_text =
        json!({"sessionId": "root", "update": {"sessionUpdate": "agent_message_chunk"}});
    assert_eq!(h.rt.apply_route(route, &late_text), None);
    assert!(h.rt.route_of_tool_call("other", None).is_none());
}

#[test]
fn air_finish_all_ends_running_children_and_discards_pending() {
    let mut h = air_with_launch("launch-1");
    h.started(task("worker-1", "launch-1", Some("Explore"), Some("run")));
    // Output under an unknown parent stays buffered.
    assert_eq!(h.route(&output("unknown", "x")), None);
    assert_eq!(h.rt.pending_count(), 1);
    let outs = h.rt.finish_all(SubagentState::Disconnected);
    h.apply(outs);
    assert_eq!(
        h.published.last().unwrap()["update"]["state"],
        "disconnected"
    );
    assert_eq!(h.rt.pending_count(), 0);
    // Idempotent: nothing left to end.
    assert!(h.rt.finish_all(SubagentState::Cancelled).is_empty());
}

#[test]
fn air_finish_delivers_buffered_output_before_the_terminal_state() {
    // `finishTask` delivers the pending updates before `finishNativeSubagent`
    // sets `terminalState` (`native-subagents.ts:512-521`); a delivery routed
    // after the state would be a late update and be dropped.
    let mut h = air_with_launch("launch-1");
    h.started(task("agent-a", "launch-1", Some("Explore"), Some("A")));
    h.started(task("agent-b", "launch-2", Some("Explore"), Some("B")));
    assert_eq!(h.route(&output("launch-2", "b-work")), None);
    let outs =
        h.rt.finish_task("agent-b", "completed", Some("launch-2"), None);
    h.apply_live(outs);
    assert_eq!(h.delivered.len(), 1);
    assert_eq!(h.delivered[0]["sessionId"], "agent-b");
    assert_eq!(h.delivered[0]["update"]["content"]["text"], "b-work");
    assert_eq!(h.kinds().last().unwrap(), "subagent_state_update");
    assert_eq!(
        h.rt.child("agent-b").unwrap().terminal_state,
        Some(SubagentState::Completed)
    );
}

#[test]
fn air_parent_cancel_ends_every_child_as_cancelled() {
    let mut h = air_with_launch("launch-1");
    h.started(task("worker-1", "launch-1", Some("Explore"), Some("run")));
    let outs = h.rt.parent_cancelled();
    h.apply(outs);
    assert_eq!(h.published.last().unwrap()["update"]["state"], "cancelled");
    assert_eq!(h.rt.cancel_child("worker-1"), None);
}

#[test]
fn disabled_runtime_passes_everything_through() {
    let mut h = H::new(None);
    let n = output("whatever", "x");
    assert_eq!(h.route(&n), Some(n.clone()));
    h.started(task("w", "t", Some("Explore"), None));
    assert!(h.published.is_empty());
    assert!(h.rt.finish_all(SubagentState::Cancelled).is_empty());
}

#[test]
fn nested_child_announced_under_live_parent_only() {
    let mut h = air_with_launch("launch-1");
    h.started(task("agent-a", "launch-1", Some("Explore"), Some("A")));
    // A nested launch made inside A's session.
    h.route(&control(
        "tool_call",
        "pending",
        "launch-2",
        Some("launch-1"),
    ));
    h.started(task("agent-b", "launch-2", Some("Explore"), Some("B")));
    let spawned = h
        .published
        .iter()
        .filter(|n| n["update"]["sessionUpdate"] == "subagent_spawned")
        .map(|n| {
            (
                n["sessionId"].as_str().unwrap(),
                n["update"]["subagentSessionId"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(spawned, [("root", "agent-a"), ("agent-a", "agent-b")]);
}

#[test]
fn nested_child_started_before_its_launch_frame_waits_for_the_frame() {
    // The nested child's task started before the spawning Agent frame: it is
    // registered but not announced, and never goes to the root meanwhile.
    let mut h = air_with_launch("launch-1");
    h.started(task("agent-a", "launch-1", Some("Explore"), Some("A")));
    h.started(task("agent-b", "launch-2", Some("Explore"), Some("B")));
    assert_eq!(h.kinds(), ["subagent_spawned"]);
    h.route(&control(
        "tool_call",
        "pending",
        "launch-2",
        Some("launch-1"),
    ));
    let last = h.published.last().unwrap();
    assert_eq!(last["sessionId"], "agent-a");
    assert_eq!(last["update"]["subagentSessionId"], "agent-b");
}

// ----- RFD form ----------------------------------------------------------------

fn rfd_launch(tool: &str, input: Value, parent: Option<&str>) -> Value {
    let mut claude = json!({"toolName": "Agent"});
    if let Some(p) = parent {
        claude["parentToolUseId"] = json!(p);
    }
    json!({"sessionId": "root", "update": {
        "sessionUpdate": "tool_call", "toolCallId": tool, "title": "Agent",
        "status": "pending", "rawInput": input, "_meta": {"claudeCode": claude}}})
}

fn send_message(tool: &str, parent: Option<&str>) -> Value {
    let mut claude = json!({"toolName": "SendMessage"});
    if let Some(p) = parent {
        claude["parentToolUseId"] = json!(p);
    }
    json!({"sessionId": "root", "update": {
        "sessionUpdate": "tool_call", "toolCallId": tool, "title": "SendMessage",
        "status": "pending", "_meta": {"claudeCode": claude}}})
}

fn running_worker(extra: Value) -> (H, Option<Value>) {
    let mut h = H::new(Some(SubagentForm::Rfd));
    let mut input = json!({"description": "Fix the build", "prompt": "Make it pass"});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        input[k] = v;
    }
    let control = h.route(&rfd_launch("launch-1", input, None));
    h.started(task("worker-1", "launch-1", Some("Explore"), None));
    (h, control)
}

#[test]
fn rfd_keeps_the_agent_tool_call_in_the_session_that_made_it() {
    let (mut h, control) = running_worker(json!({}));
    let control = control.unwrap();
    assert_eq!(control["sessionId"], "root");
    assert_eq!(control["update"]["toolCallId"], "launch-1");
    let nested = h.route(&rfd_launch(
        "launch-2",
        json!({"description": "Dig"}),
        Some("launch-1"),
    ));
    assert_eq!(session_of(&nested).as_deref(), Some("worker-1"));
}

#[test]
fn rfd_announces_the_child_with_labels_then_sends_the_prompt() {
    let (h, _) = running_worker(json!({"name": "builder"}));
    assert_eq!(
        h.published,
        vec![
            json!({"sessionId": "root", "update": {
                "sessionUpdate": "subagent_update", "sessionId": "worker-1",
                "title": "builder (Explore)", "description": "Fix the build",
                "capabilities": {"cancel": {}}, "state": {"state": "running"}}}),
            json!({"sessionId": "worker-1", "update": {
                "sessionUpdate": "session_message", "messageId": "launch-1",
                "senderSessionId": "root", "recipientSessionId": "worker-1",
                "content": [{"type": "text", "text": "Make it pass"}]}}),
        ]
    );
}

#[test]
fn rfd_titles_a_child_without_a_name_by_its_role() {
    let (h, _) = running_worker(json!({}));
    assert_eq!(h.published[0]["update"]["title"], "Explore");
    assert_eq!(h.published[0]["update"]["description"], "Fix the build");
}

#[test]
fn rfd_keeps_the_session_of_an_idle_child_which_a_resume_runs_again() {
    let (mut h, _) = running_worker(json!({}));
    h.finish("worker-1", "completed", None);
    assert_eq!(
        session_of(&h.route(&output("launch-1", "x"))).as_deref(),
        Some("worker-1")
    );
    h.route(&send_message("send-1", None));
    h.send_resumed("worker-1", Some("Check the tests too"), Some("send-1"));
    h.send_resumed("worker-1", Some("Check the tests too"), Some("send-1"));
    h.finish("worker-1", "completed", None);
    assert_eq!(
        h.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "idle", "stopReason": "end_turn"}),
            json!({"state": "running"}),
            json!({"state": "idle", "stopReason": "end_turn"}),
        ]
    );
    let messages: Vec<&Value> = h
        .published
        .iter()
        .filter(|n| n["update"]["sessionUpdate"] == "session_message")
        .collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1],
        &json!({"sessionId": "worker-1", "update": {
            "sessionUpdate": "session_message", "messageId": "send-1",
            "senderSessionId": "root", "recipientSessionId": "worker-1",
            "content": [{"type": "text", "text": "Check the tests too"}]}})
    );
}

#[test]
fn rfd_moves_the_work_of_a_resume_to_the_child_under_the_resuming_call() {
    let (mut h, _) = running_worker(json!({}));
    h.finish("worker-1", "completed", Some("launch-1"));
    h.route(&send_message("send-1", None));
    // Output of the resumed run can arrive before the SDK's task_started.
    assert_eq!(h.route(&output("send-1", "working")), None);
    let mut t = TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("send-1".into()),
        prompt: Some("Check the tests too".into()),
        ..TaskStarted::default()
    };
    h.started(t.clone());
    h.send_resumed("worker-1", Some("Check the tests too"), Some("send-1"));
    assert_eq!(h.delivered, vec![output("send-1", "working")]);
    assert_eq!(
        session_of(&h.route(&output("send-1", "working"))).as_deref(),
        Some("worker-1")
    );
    // The end of the first delegation, again, does not end the second.
    h.finish("worker-1", "completed", Some("launch-1"));
    assert_eq!(h.states().last().unwrap(), &json!({"state": "running"}));
    h.finish("worker-1", "completed", Some("send-1"));
    assert_eq!(h.states().len(), 4);
    let ids: Vec<&str> = h
        .published
        .iter()
        .filter(|n| n["update"]["sessionUpdate"] == "session_message")
        .map(|n| n["update"]["messageId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["launch-1", "send-1"]);
    t.prompt = None;
}

#[test]
fn rfd_ignores_a_send_message_result_after_the_run_it_started_ended() {
    let (mut h, _) = running_worker(json!({}));
    h.finish("worker-1", "completed", Some("launch-1"));
    let t = TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("send-1".into()),
        prompt: Some("Check the tests too".into()),
        ..TaskStarted::default()
    };
    h.started(t);
    h.finish("worker-1", "failed", None);
    h.finish("worker-1", "failed", Some("send-1"));
    assert!(h.rt.is_ended_delegation("worker-1", Some("send-1")));
    h.send_resumed("worker-1", Some("Check the tests too"), Some("send-1"));
    assert_eq!(
        h.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "idle", "stopReason": "end_turn"}),
            json!({"state": "running"}),
            json!({"state": "idle", "stopReason": "error"}),
        ]
    );
}

#[test]
fn rfd_ends_a_run_under_an_id_the_child_used_before() {
    let (mut h, _) = running_worker(json!({}));
    h.finish("worker-1", "completed", Some("launch-1"));
    h.started(TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("send-1".into()),
        ..TaskStarted::default()
    });
    h.finish("worker-1", "completed", Some("send-1"));
    h.started(TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("launch-1".into()),
        ..TaskStarted::default()
    });
    assert!(h.rt.is_stale_end("worker-1", Some("send-1")));
    assert!(!h.rt.is_stale_end("worker-1", Some("launch-1")));
    h.finish("worker-1", "completed", Some("launch-1"));
    assert_eq!(
        h.states().last().unwrap(),
        &json!({"state": "idle", "stopReason": "end_turn"})
    );
    assert_eq!(h.states().len(), 6);
}

#[test]
fn rfd_running_patch_reports_running_without_a_guessed_prompt() {
    let (mut h, _) = running_worker(json!({}));
    h.finish("worker-1", "completed", Some("launch-1"));
    h.resumed("worker-1", Some("An old message"));
    assert_eq!(h.states().last().unwrap(), &json!({"state": "running"}));
    let n = h
        .published
        .iter()
        .filter(|n| n["update"]["sessionUpdate"] == "session_message")
        .count();
    assert_eq!(n, 1);
    assert!(!h.rt.is_stale_end("worker-1", Some("launch-1")));
}

#[test]
fn rfd_resumes_a_child_with_an_open_request_as_requires_action() {
    let (mut h, _) = running_worker(json!({}));
    let (token, outs) = h.rt.awaiting_user_begin("worker-1");
    h.apply(outs);
    h.finish("worker-1", "completed", Some("launch-1"));
    h.started(TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("send-1".into()),
        ..TaskStarted::default()
    });
    let outs = h.rt.awaiting_user_end(token.unwrap(), false);
    h.apply(outs);
    assert_eq!(
        h.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "requires_action"}),
            json!({"state": "idle", "stopReason": "end_turn"}),
            json!({"state": "requires_action"}),
            json!({"state": "running"}),
        ]
    );
}

#[test]
fn rfd_names_the_session_that_sent_a_resumes_prompt() {
    let (mut h, _) = running_worker(json!({}));
    h.route(&rfd_launch(
        "launch-2",
        json!({"description": "Dig"}),
        Some("launch-1"),
    ));
    h.started(task("worker-2", "launch-2", Some("Explore"), None));
    h.finish("worker-2", "completed", Some("launch-2"));
    h.route(&send_message("send-root", None));
    h.send_resumed("worker-2", Some("From the root"), Some("send-root"));
    h.finish("worker-2", "completed", Some("send-root"));
    h.route(&send_message("send-worker", Some("launch-1")));
    h.send_resumed("worker-2", Some("From worker-1"), Some("send-worker"));
    h.finish("worker-2", "completed", Some("send-worker"));
    h.send_resumed("worker-2", Some("From someone"), Some("send-unknown"));
    let senders: Vec<(String, Option<String>)> = h
        .published
        .iter()
        .filter(|n| {
            n["sessionId"] == "worker-2" && n["update"]["sessionUpdate"] == "session_message"
        })
        .map(|n| {
            (
                n["update"]["messageId"].as_str().unwrap().to_string(),
                n["update"]["senderSessionId"]
                    .as_str()
                    .map(ToString::to_string),
            )
        })
        .collect();
    assert_eq!(
        senders,
        [
            ("send-root".to_string(), Some("root".to_string())),
            ("send-worker".into(), Some("worker-1".into())),
            ("send-unknown".into(), None),
        ]
    );
}

#[test]
fn rfd_ends_running_children_with_the_stream_as_idle_with_no_reason() {
    let (mut h, _) = running_worker(json!({}));
    let outs = h.rt.finish_all(SubagentState::Failed);
    h.apply(outs);
    assert_eq!(h.states().last().unwrap(), &json!({"state": "idle"}));
}

#[test]
fn rfd_reports_a_child_that_can_no_longer_be_observed_as_unknown() {
    let (mut h, _) = running_worker(json!({}));
    h.finish("worker-1", "disconnected", None);
    assert_eq!(h.states().last().unwrap(), &json!({"state": "unknown"}));
}

#[test]
fn rfd_failed_child_error_text_may_arrive_later() {
    let (mut h, _) = running_worker(json!({}));
    let o = h.rt.finish_task("worker-1", "failed", None, None);
    h.apply(o);
    assert_eq!(
        h.states().last().unwrap(),
        &json!({"state": "idle", "stopReason": "error"})
    );
    let end = TaskEnd {
        error: Some("boom".into()),
        ..TaskEnd::default()
    };
    let o = h.rt.finish_task("worker-1", "failed", None, Some(&end));
    h.apply(o);
    assert_eq!(
        h.states().last().unwrap(),
        &json!({"state": "idle", "stopReason": "error", "error": {"code": -32603, "message": "boom"}})
    );
}

#[test]
fn rfd_parent_cancel_reports_nothing_and_child_cancel_stops_the_task() {
    let (mut h, _) = running_worker(json!({}));
    let before = h.published.len();
    let outs = h.rt.parent_cancelled();
    assert!(outs.is_empty());
    assert_eq!(h.published.len(), before);
    assert_eq!(h.rt.cancel_child("worker-1").as_deref(), Some("worker-1"));
    assert_eq!(h.rt.cancel_child("not-a-child"), None);
    let mut air = air_with_launch("l");
    air.started(task("w", "l", Some("Explore"), Some("d")));
    assert_eq!(air.rt.cancel_child("w"), None);
}

#[test]
fn rfd_request_withdrawn_by_a_cancel_does_not_report_running() {
    let (mut h, _) = running_worker(json!({}));
    let (token, outs) = h.rt.awaiting_user_begin("worker-1");
    h.apply(outs);
    h.rt.cancel_child("worker-1");
    let outs = h.rt.awaiting_user_end(token.unwrap(), true);
    h.apply(outs);
    assert_eq!(
        h.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "requires_action"})
        ]
    );
    // Withdrawn without a cancel: the child goes on, so it runs again.
    let (token, outs) = h.rt.awaiting_user_begin("worker-1");
    h.apply(outs);
    let outs = h.rt.awaiting_user_end(token.unwrap(), true);
    h.apply(outs);
    assert_eq!(h.states().last().unwrap(), &json!({"state": "running"}));
}

#[test]
fn rfd_nested_child_whose_parent_is_not_announced_waits_and_never_goes_under_root() {
    let mut h = H::new(Some(SubagentForm::Rfd));
    // The nested launch comes from a child (launch-1) that was never announced.
    assert_eq!(
        h.route(&rfd_launch(
            "launch-2",
            json!({"description": "Dig"}),
            Some("launch-1")
        )),
        None
    );
    h.started(task("worker-2", "launch-2", Some("Explore"), None));
    assert!(h.published.is_empty());
}

#[test]
fn rfd_child_that_ends_before_its_parent_is_known_stays_unexposed() {
    let mut h = H::new(Some(SubagentForm::Rfd));
    h.started(task("worker-2", "launch-2", Some("Explore"), None));
    h.finish("worker-2", "completed", Some("launch-2"));
    assert!(h.published.is_empty());
    assert!(h.rt.child("worker-2").is_none());
}

// ----- restore from the history ------------------------------------------------

fn entry(id: &str, launch: &str, parent: Option<&str>) -> HistoricalSubagent {
    HistoricalSubagent {
        agent_id: id.into(),
        launch_id: launch.into(),
        parent_agent_id: parent.map(Into::into),
        name: None,
        agent_type: Some("Explore".into()),
        description: Some(format!("{id} job")),
    }
}

#[test]
fn rfd_restores_an_agent_with_its_ancestors_parent_first() {
    let lineage: LineageFn = Box::new(|id| {
        if id == "child" {
            vec![
                entry("parent", "launch-p", None),
                entry("child", "launch-c", Some("parent")),
            ]
        } else {
            Vec::new()
        }
    });
    let mut h = H::with_history(SubagentForm::Rfd, lineage, None);
    h.route(&send_message("send-1", None));
    // The resumed run's output waits under the launch id.
    assert_eq!(h.route(&output("launch-c", "waiting")), None);
    h.started(TaskStarted {
        task_id: "child".into(),
        tool_use_id: Some("send-1".into()),
        prompt: Some("Go on".into()),
        ..TaskStarted::default()
    });
    // Ancestor: labels only, no state, under the root.
    assert_eq!(
        h.published[0],
        json!({"sessionId": "root", "update": {
            "sessionUpdate": "subagent_update", "sessionId": "parent",
            "title": "Explore", "description": "parent job"}})
    );
    assert_eq!(h.published[1]["sessionId"], "parent");
    assert_eq!(h.published[1]["update"]["sessionId"], "child");
    assert_eq!(
        h.published[1]["update"]["state"],
        json!({"state": "running"})
    );
    assert_eq!(h.delivered, vec![output("launch-c", "waiting")]);
}

#[test]
fn rfd_reads_no_history_for_an_unknown_call_or_a_known_child() {
    let lineage: LineageFn = Box::new(|_| panic!("history must not be read"));
    let mut h = H::with_history(SubagentForm::Rfd, lineage, None);
    // No SendMessage call recorded for the id: no restore, nothing exposed.
    h.started(TaskStarted {
        task_id: "x".into(),
        tool_use_id: Some("not-a-send".into()),
        ..TaskStarted::default()
    });
    assert!(h.published.is_empty());
}

#[test]
fn air_restores_an_agent_from_a_resume_signal_with_a_timestamp_generation() {
    let lineage: LineageFn = Box::new(|id| {
        if id == "agent-x" {
            vec![entry("agent-x", "launch-x", None)]
        } else {
            Vec::new()
        }
    });
    let mut h = H::with_history(SubagentForm::Air, lineage, None);
    assert_eq!(h.route(&output("launch-x", "waiting")), None);
    h.resumed("agent-x", Some("Again"));
    let spawned = &h.published[0];
    let sid = spawned["update"]["subagentSessionId"].as_str().unwrap();
    assert!(sid.starts_with("agent-x:generation:"), "{sid}");
    assert_eq!(spawned["update"]["prompt"], "Again");
    assert_eq!(h.delivered.len(), 1);
    // An agent the history cannot place opens nothing, and is looked up once.
    h.resumed("agent-y", None);
    assert_eq!(h.published.len(), 1);
}

#[test]
fn air_looks_up_an_unknown_launch_id_only_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let launched: LaunchedAgentFn = Box::new(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        None
    });
    let mut h = H::with_history(SubagentForm::Air, Box::new(|_| Vec::new()), Some(launched));
    assert_eq!(h.route(&output("launch-z", "a")), None);
    assert_eq!(h.route(&output("launch-z", "b")), None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // A new Agent call of this process is known: no history lookup.
    h.route(&control("tool_call", "pending", "launch-new", None));
    assert_eq!(h.route(&output("launch-new", "c")), None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// ----- buffering caps ----------------------------------------------------------

#[test]
fn buffering_caps_drop_overflow_with_a_log_line() {
    let mut h = air_with_launch("launch-1");
    // 32 per parent.
    for i in 0..40 {
        assert_eq!(h.route(&output("p-one", &format!("m{i}"))), None);
    }
    assert_eq!(h.rt.pending_count(), 32);
    assert!(h
        .rt
        .take_logs()
        .iter()
        .any(|l| l.contains("pending buffer limit reached")));
    // 64 parents.
    for p in 0..70 {
        h.route(&output(&format!("parent-{p}"), "x"));
    }
    assert_eq!(h.rt.pending_count(), 32 + 63);
    // 256 updates overall: fill remaining parents to the total cap.
    for p in 1..64 {
        for i in 0..32 {
            h.route(&output(&format!("parent-{p}"), &format!("{i}")));
        }
    }
    assert_eq!(h.rt.pending_count(), 256);
    h.route(&output("parent-2", "over"));
    assert_eq!(h.rt.pending_count(), 256);
    h.rt.discard_pending("p-one");
    assert_eq!(h.rt.pending_count(), 256 - 32);
}

#[test]
fn send_message_senders_are_bounded() {
    let mut h = H::new(Some(SubagentForm::Rfd));
    for i in 0..2050 {
        h.route(&send_message(&format!("s{i}"), None));
    }
    assert_eq!(h.rt.sender_count(), 2048);
    assert_eq!(h.rt.sender_of("s0"), None);
    assert_eq!(h.rt.sender_of("s2049"), Some("root"));
}

#[test]
fn clear_forgets_everything() {
    let (mut h, _) = running_worker(json!({}));
    h.rt.clear();
    assert!(h.rt.child("worker-1").is_none());
    assert_eq!(h.rt.pending_count(), 0);
}

#[test]
fn clear_releases_the_generations() {
    let (mut h, _) = running_worker(json!({}));
    assert_eq!(h.rt.arena_len(), 1);
    h.rt.clear();
    assert_eq!(h.rt.arena_len(), 0);
    h.route(&rfd_launch("launch-2", json!({"description": "d"}), None));
    h.started(task("worker-2", "launch-2", Some("Explore"), None));
    assert_eq!(h.rt.arena_len(), 1);
}

#[test]
fn a_reset_sender_stays_recent_past_the_cap() {
    let mut h = H::new(Some(SubagentForm::Rfd));
    for i in 0..2048 {
        h.route(&send_message(&format!("s{i}"), None));
    }
    // A re-set call is the newest again (the JS `Map` delete+set), so the next
    // insert evicts s1, not s0.
    h.route(&send_message("s0", None));
    h.route(&send_message("s2048", None));
    assert_eq!(h.rt.sender_count(), 2048);
    assert_eq!(h.rt.sender_of("s0"), Some("root"));
    assert_eq!(h.rt.sender_of("s1"), None);
    assert_eq!(h.rt.sender_of("s2048"), Some("root"));
}

// ----- stale tokens and routes ------------------------------------------------

#[test]
fn a_request_token_of_another_runtime_does_nothing() {
    let (mut h, _) = running_worker(json!({}));
    let (token, _) = h.rt.awaiting_user_begin("worker-1");
    let token = token.expect("announced child");
    // A runtime replaced by a clear-context restart has its own child at the
    // same arena index, waiting on its own request.
    let mut fresh = NativeSubagentRuntime::new(Some(SubagentForm::Rfd), "root");
    fresh.route(
        &rfd_launch("launch-1", json!({"description": "d"}), None),
        None,
    );
    fresh.task_started(&task("worker-1", "launch-1", Some("Explore"), None));
    let (_, begun) = fresh.awaiting_user_begin("worker-1");
    assert_eq!(begun.len(), 1);
    assert!(fresh.awaiting_user_end(token.clone(), false).is_empty());
    // A clear makes the runtime's own tokens stale too.
    h.rt.clear();
    assert!(h.rt.awaiting_user_end(token, false).is_empty());
}

#[test]
fn a_tool_call_route_of_another_runtime_applies_to_nothing() {
    let mut h = air_with_launch("launch-1");
    h.started(task("worker-1", "launch-1", Some("Explore"), Some("run")));
    let call = json!({"sessionId": "root", "update": {
        "sessionUpdate": "tool_call", "toolCallId": "bash-1",
        "_meta": {"claudeCode": {"toolName": "Bash", "parentToolUseId": "launch-1"}}}});
    h.route(&call);
    let route =
        h.rt.route_of_tool_call("bash-1", None)
            .expect("child owns the call");
    let mut fresh = NativeSubagentRuntime::new(Some(SubagentForm::Air), "root");
    fresh.route(&control("tool_call", "pending", "launch-1", None), None);
    fresh.task_started(&task("worker-9", "launch-1", Some("Explore"), Some("run")));
    assert!(fresh
        .apply_route(route.clone(), &output("launch-1", "late"))
        .is_none());
    // The runtime that made the route still applies it; after a clear it is
    // stale instead of out of bounds.
    assert!(h
        .rt
        .apply_route(route.clone(), &output("launch-1", "late"))
        .is_some());
    h.rt.clear();
    assert!(h
        .rt
        .apply_route(route, &output("launch-1", "late"))
        .is_none());
}
