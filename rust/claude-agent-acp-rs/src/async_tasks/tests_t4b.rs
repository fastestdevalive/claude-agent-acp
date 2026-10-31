//! T4B: the subagent runtime fed through `AsyncTaskRuntime` (V-17, N-54..N-56).

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Value};

use super::{AsyncTaskRuntime, SubagentSends};
use crate::acp_subagents::{SubagentForm, SubagentState};
use crate::native_subagents::Out;

fn rt(form: SubagentForm) -> AsyncTaskRuntime {
    AsyncTaskRuntime::new(true).with_subagent_form(Some(form), "root")
}

fn launch(id: &str) -> Value {
    json!({"sessionId": "root", "update": {
        "sessionUpdate": "tool_call", "toolCallId": id, "title": "Agent", "status": "pending",
        "rawInput": {"description": "Fix", "prompt": "Make it pass"},
        "_meta": {"claudeCode": {"toolName": "Agent"}, "jetbrains": {"air": {"version": 1, "subagent": true}}}}})
}

fn control(kind: &str, id: &str, parent: Option<&str>, input: Value) -> Value {
    let mut claude = json!({"toolName": "Agent"});
    if let Some(parent) = parent {
        claude["parentToolUseId"] = json!(parent);
    }
    json!({"sessionId": "root", "update": {
        "sessionUpdate": kind, "toolCallId": id, "title": "Agent", "status": "pending",
        "rawInput": input,
        "_meta": {"claudeCode": claude}}})
}

fn output(parent: &str, text: &str) -> Value {
    json!({"sessionId": "root", "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text},
        "_meta": {"claudeCode": {"parentToolUseId": parent}}}})
}

fn publishes(outs: Vec<Out>) -> Vec<Value> {
    outs.into_iter()
        .filter_map(|o| match o {
            Out::Publish(v) => Some(v),
            Out::Deliver(_) | Out::Routed(_) => None,
        })
        .collect()
}

/// What the session's `publish_subagent_outs` did with an [`Out`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Send {
    /// `client.sessionUpdate`: the runtime published it as it is.
    Publish,
    /// The post-routing tail of `sendUpdate`: the runtime (or the loop) routed it.
    Deliver,
}

type Log = Rc<RefCell<Vec<(Send, Value)>>>;

fn recorder() -> Log {
    Rc::new(RefCell::new(Vec::new()))
}

fn apply(r: &mut AsyncTaskRuntime, outs: Vec<Out>, log: &Log) -> Vec<String> {
    let mut publish = {
        let log = log.clone();
        move |n: Value| -> Result<(), String> {
            log.borrow_mut().push((Send::Publish, n));
            Ok(())
        }
    };
    let mut deliver = {
        let log = log.clone();
        move |n: Value| -> Result<(), String> {
            log.borrow_mut().push((Send::Deliver, n));
            Ok(())
        }
    };
    let mut eager_session_of = |_: &str| None;
    r.publish_subagent_outs(
        outs,
        SubagentSends {
            publish: &mut publish,
            deliver: &mut deliver,
            eager_session_of: &mut eager_session_of,
        },
    )
}

fn finish_all(r: &mut AsyncTaskRuntime, state: SubagentState, log: &Log) -> Vec<String> {
    let mut publish = {
        let log = log.clone();
        move |n: Value| -> Result<(), String> {
            log.borrow_mut().push((Send::Publish, n));
            Ok(())
        }
    };
    let mut deliver = {
        let log = log.clone();
        move |n: Value| -> Result<(), String> {
            log.borrow_mut().push((Send::Deliver, n));
            Ok(())
        }
    };
    let mut eager_session_of = |_: &str| None;
    r.finish_all_subagents_with(
        state,
        SubagentSends {
            publish: &mut publish,
            deliver: &mut deliver,
            eager_session_of: &mut eager_session_of,
        },
    )
}

fn kind(n: &Value) -> &str {
    n["update"]["sessionUpdate"].as_str().unwrap_or("")
}

#[test]
fn subagent_task_stream_feeds_the_runtime_and_async_tasks_still_ignore_it() {
    let mut r = rt(SubagentForm::Air);
    r.subagents().route(&launch("agent-1"), None);
    let started = json!({"task_id": "w1", "task_type": "local_agent", "subagent_type": "Explore",
        "description": "Fix", "tool_use_id": "agent-1", "prompt": "Make it pass"});
    let outs = publishes(r.subagent_task_started(&started));
    assert_eq!(outs.len(), 1);
    assert_eq!(outs[0]["update"]["sessionUpdate"], "subagent_spawned");
    assert_eq!(outs[0]["update"]["prompt"], "Make it pass");
    // The async-task runtime itself still publishes nothing for a subagent task.
    assert!(r.task_started(&started).is_empty());
    let done = json!({"task_id": "w1", "status": "completed", "tool_use_id": "agent-1"});
    let outs = publishes(r.subagent_task_notification(&done));
    assert_eq!(outs.len(), 1);
    assert_eq!(outs[0]["update"]["state"], "completed");
}

#[test]
fn task_updated_resumes_and_ends_children() {
    let mut r = rt(SubagentForm::Air);
    r.subagents().route(&launch("agent-1"), None);
    r.subagent_task_started(
        &json!({"task_id": "w1", "subagent_type": "Explore", "tool_use_id": "agent-1"}),
    );
    let ended = publishes(r.subagent_task_updated(
        "w1",
        &json!({"status": "failed", "error": "boom"}),
        None,
    ));
    assert_eq!(ended[0]["update"]["state"], "failed");
    let resumed =
        publishes(r.subagent_task_updated("w1", &json!({"status": "running"}), Some("Again")));
    assert_eq!(resumed[0]["update"]["subagentSessionId"], "w1:generation:2");
    assert_eq!(resumed[0]["update"]["prompt"], "Again");
    assert!(r
        .subagent_task_updated("w1", &json!({"status": "paused"}), None)
        .is_empty());
}

#[test]
fn teardown_reports_children_still_running_as_finished() {
    let mut r = rt(SubagentForm::Air);
    r.subagents().route(&launch("agent-1"), None);
    r.subagent_task_started(
        &json!({"task_id": "w1", "subagent_type": "Explore", "tool_use_id": "agent-1"}),
    );
    let log = recorder();
    let errors = finish_all(&mut r, SubagentState::Cancelled, &log);
    assert!(errors.is_empty());
    let sent = log.borrow().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(kind(&sent[0].1), "subagent_state_update");
    assert_eq!(sent[0].1["update"]["state"], "cancelled");
    // A second teardown has nothing left to close.
    let again = recorder();
    finish_all(&mut r, SubagentState::Cancelled, &again);
    assert!(again.borrow().is_empty());
}

#[test]
fn teardown_collects_publication_errors_and_finishes_the_rest() {
    let mut r = rt(SubagentForm::Air);
    for (task, tool) in [("w1", "a1"), ("w2", "a2")] {
        r.subagents().route(&launch(tool), None);
        r.subagent_task_started(
            &json!({"task_id": task, "subagent_type": "Explore", "tool_use_id": tool}),
        );
    }
    let mut calls = 0;
    let mut publish = |_: Value| -> Result<(), String> {
        calls += 1;
        if calls == 1 {
            Err("down".into())
        } else {
            Ok(())
        }
    };
    let mut deliver = |_: Value| -> Result<(), String> { Ok(()) };
    let mut eager_session_of = |_: &str| None;
    let errors = r.finish_all_subagents_with(
        SubagentState::Failed,
        SubagentSends {
            publish: &mut publish,
            deliver: &mut deliver,
            eager_session_of: &mut eager_session_of,
        },
    );
    assert_eq!(calls, 2);
    assert_eq!(errors, ["down"]);
}

#[test]
fn rfd_teardown_ends_children_idle_with_no_reason() {
    let mut r = rt(SubagentForm::Rfd);
    let l = json!({"sessionId": "root", "update": {"sessionUpdate": "tool_call", "toolCallId": "a1",
        "rawInput": {"description": "d"}, "_meta": {"claudeCode": {"toolName": "Agent"}}}});
    r.subagents().route(&l, None);
    r.subagent_task_started(
        &json!({"task_id": "w1", "subagent_type": "Explore", "tool_use_id": "a1"}),
    );
    let log = recorder();
    finish_all(&mut r, SubagentState::Failed, &log);
    let sent = log.borrow().clone();
    assert_eq!(
        sent.last().unwrap().1["update"]["state"],
        json!({"state": "idle"})
    );
}

#[test]
fn a_disabled_runtime_does_nothing() {
    let mut r = AsyncTaskRuntime::new(true);
    assert!(r
        .subagent_task_started(&json!({"task_id": "w1", "subagent_type": "Explore"}))
        .is_empty());
    let log = recorder();
    assert!(finish_all(&mut r, SubagentState::Failed, &log).is_empty());
}

#[test]
fn a_re_routed_buffered_update_sends_what_its_route_publishes_first() {
    // Node's `routeUpdate` awaits what its route publishes (the announce of a
    // nested child and that child's buffered updates) before it returns the
    // routed notification (`native-subagents.ts:244-246`). A buffered update
    // routed again must keep that order.
    let mut r = rt(SubagentForm::Rfd);
    let log = recorder();
    // A's task starts before its Agent frame: registered, not announced.
    r.subagent_task_started(
        &json!({"task_id": "agent-a", "subagent_type": "Explore", "tool_use_id": "launch-1"}),
    );
    // The nested Agent frame of B (under A's launch) arrives while A is
    // unannounced: it is buffered under A's launch.
    let nested = control(
        "tool_call",
        "launch-2",
        Some("launch-1"),
        json!({"description": "B job", "prompt": "Do B"}),
    );
    let res = r.subagents().route(&nested, None);
    assert!(res.outs.is_empty());
    assert!(res.routed.is_none());
    // B's task starts before that frame reaches the runtime: not announced.
    r.subagent_task_started(
        &json!({"task_id": "agent-b", "subagent_type": "Explore", "tool_use_id": "launch-2"}),
    );
    // B's output waits under its launch id.
    assert!(r
        .subagents()
        .route(&output("launch-2", "b-work"), None)
        .routed
        .is_none());
    // A's Agent frame: announces A and delivers B's buffered frame.
    let res = r.subagents().route(
        &control(
            "tool_call",
            "launch-1",
            None,
            json!({"description": "A job"}),
        ),
        None,
    );
    let mut outs = res.outs;
    if let Some(routed) = res.routed {
        outs.push(Out::Routed(routed));
    }
    apply(&mut r, outs, &log);
    let events = log.borrow().clone();
    let seen: Vec<(Send, String)> = events
        .iter()
        .map(|(send, n)| {
            (
                *send,
                format!("{}:{}", kind(n), n["sessionId"].as_str().unwrap_or("")),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (Send::Publish, "subagent_update:root".to_string()),
            (Send::Publish, "subagent_update:agent-a".to_string()),
            (Send::Publish, "session_message:agent-b".to_string()),
            (Send::Deliver, "agent_message_chunk:agent-b".to_string()),
            (Send::Deliver, "tool_call:agent-a".to_string()),
            (Send::Deliver, "tool_call:root".to_string()),
        ]
    );
    // B is announced under A, before its output.
    assert_eq!(events[1].1["update"]["sessionId"], "agent-b");
}

#[test]
fn a_child_that_starts_before_its_agent_frame_keeps_its_output_when_it_ends() {
    // `finishTask` delivers the buffered output before `finishNativeSubagent`
    // sets the terminal state (`native-subagents.ts:512-521`).
    let mut r = rt(SubagentForm::Air);
    let log = recorder();
    r.subagents().route(&launch("launch-1"), None);
    r.subagent_task_started(
        &json!({"task_id": "agent-a", "subagent_type": "Explore", "tool_use_id": "launch-1"}),
    );
    // B starts before its nested Agent frame: registered, not announced.
    r.subagent_task_started(
        &json!({"task_id": "agent-b", "subagent_type": "Explore", "tool_use_id": "launch-2"}),
    );
    assert!(r
        .subagents()
        .route(&output("launch-2", "b-work"), None)
        .routed
        .is_none());
    let outs = r.subagent_task_notification(
        &json!({"task_id": "agent-b", "status": "completed", "tool_use_id": "launch-2"}),
    );
    apply(&mut r, outs, &log);
    let events = log.borrow().clone();
    let seen: Vec<(Send, String)> = events
        .iter()
        .map(|(send, n)| (*send, kind(n).to_string()))
        .collect();
    assert_eq!(
        seen,
        [
            (Send::Publish, "subagent_spawned".to_string()),
            (Send::Deliver, "agent_message_chunk".to_string()),
            (Send::Publish, "subagent_state_update".to_string()),
        ]
    );
    assert_eq!(events[1].1["sessionId"], "agent-b");
    assert_eq!(events[1].1["update"]["content"]["text"], "b-work");
    assert_eq!(events[2].1["update"]["state"], "completed");
}

#[test]
fn teardown_keeps_the_output_of_a_child_that_starts_before_its_agent_frame() {
    // Same path through `finishAll` (`native-subagents.ts:544-565`).
    let mut r = rt(SubagentForm::Air);
    let log = recorder();
    r.subagents().route(&launch("launch-1"), None);
    r.subagent_task_started(
        &json!({"task_id": "agent-a", "subagent_type": "Explore", "tool_use_id": "launch-1"}),
    );
    r.subagent_task_started(
        &json!({"task_id": "agent-b", "subagent_type": "Explore", "tool_use_id": "launch-2"}),
    );
    assert!(r
        .subagents()
        .route(&output("launch-2", "b-work"), None)
        .routed
        .is_none());
    finish_all(&mut r, SubagentState::Cancelled, &log);
    let events = log.borrow().clone();
    // Children end newest first: B (spawned, its output, cancelled), then A.
    let seen: Vec<(Send, String)> = events
        .iter()
        .map(|(send, n)| (*send, kind(n).to_string()))
        .collect();
    assert_eq!(
        seen,
        [
            (Send::Publish, "subagent_spawned".to_string()),
            (Send::Deliver, "agent_message_chunk".to_string()),
            (Send::Publish, "subagent_state_update".to_string()),
            (Send::Publish, "subagent_state_update".to_string()),
        ]
    );
    assert_eq!(events[1].1["sessionId"], "agent-b");
    assert_eq!(events[2].1["update"]["state"], "cancelled");
}
