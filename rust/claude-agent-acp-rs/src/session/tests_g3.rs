//! G3: the T9A gap rows that need a product fix, ported with the upstream
//! cases' inputs and expected wire values (`src/tests/acp-v2.test.ts`).
//!
//! Session-level cases drive `handle_session_frame` / `send_updates` with the
//! state a running actor holds ([`tests_t2a::Rig`]); the replay, live-load and
//! clear-context cases drive the real actor over a fake `claude` (see
//! `tests/t8d_create_options.rs`, `tests/t7b_v2_replay.rs` and
//! `tests/t5fa_restart.rs`).

use super::tests_t2a::Rig;
use super::*;
use crate::turn_events::V2IntakeError;
use serde_json::{json, Value};

/// The cancel path of the running actor (`tests_t2a::Rig::cancel`).
fn cancel(rig: &mut Rig) {
    let events = rig.machine.cancel();
    rig.map_state.begin_stopped_work();
    settle_events(
        events,
        &mut rig.tasks,
        &mut rig.map_state,
        &rig.live_background,
        &mut rig.pending,
        &rig.tx,
        "s",
        &rig.high_water,
        &rig.ext.failure_ctx,
        rig.ext.air.as_ref(),
        None,
        &mut rig.ext.titles,
        &mut rig.ext.t2a.failures,
        &mut rig.ext.t2a.usage_turns,
        &mut crate::session::FileChangeReports::default(),
    );
}

/// The `result` frame of a cancelled turn (`tests_t2a::Rig::interrupt_result`).
fn interrupt_result(rig: &mut Rig) {
    rig.feed(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "stop_reason":"end_turn","terminal_reason":"aborted_tools","result":"",
        "usage":{"input_tokens":1,"output_tokens":0}}),
    );
}

/// A [`Rig`] on a v2 connection.
fn rig_v2() -> Rig {
    let mut rig = Rig::new();
    rig.map_state.v2.set_reporting(true);
    rig
}

/// Every outbound notification since the last call, as `{"sessionId", "update"}`.
fn notifications(rig: &mut Rig) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(item) = rig.rx.try_recv() {
        match item {
            SessionOutbound::Update(notif) => {
                let mut value = serde_json::to_value(&notif).unwrap();
                if let Some(update) = value.get_mut("update") {
                    crate::map::materialize_tool_call_defaults(update);
                }
                out.push(value);
            }
            SessionOutbound::RawUpdate(value) => out.push(value),
            _ => {}
        }
    }
    out
}

/// The last status report of `id` in `notes`.
fn last_status(notes: &[Value], id: &str) -> Option<Value> {
    notes
        .iter()
        .rev()
        .find(|value| {
            value["update"]["toolCallId"] == json!(id) && value["update"].get("status").is_some()
        })
        .map(|value| value["update"]["status"].clone())
}

/// Register and enqueue a v2 prompt once (the caller feeds its echo).
fn queue_v2(rig: &mut Rig, uuid: &str) -> oneshot::Receiver<Result<String, V2IntakeError>> {
    let (tx, rx) = oneshot::channel();
    rig.map_state
        .v2
        .register(uuid, json!([{"type":"text","text":"hi"}]), tx);
    rig.machine.enqueue(Turn::new(uuid.into(), false));
    rx
}

fn echo(rig: &mut Rig, uuid: &str) {
    rig.feed(json!({"type":"user","uuid":uuid,"message":{"role":"user",
        "content":[{"type":"text","text":"hi"}]}}));
}

fn result_frame(stop_reason: &str) -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":stop_reason,"result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

fn autonomous_result() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"follow-up","origin":{"kind":"task-notification"},
        "num_turns":1,"usage":{"input_tokens":1,"output_tokens":1}})
}

// ---------------------------------------------------------------- C-24 (745)

/// acp-v2.test.ts: "reports a command whose result says interrupted as
/// cancelled" — a successful command whose structured output says `interrupted`
/// is an aborted run, so v2 gets `cancelled` and v1 keeps `completed`.
#[test]
fn t9a_g3_a_command_result_that_says_interrupted_is_cancelled_on_v2() {
    for (v2, expected) in [(true, "cancelled"), (false, "completed")] {
        let mut rig = if v2 { rig_v2() } else { Rig::new() };
        rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_bash","role":"assistant","model":"claude-sonnet-4-6",
            "content":[{"type":"tool_use","id":"toolu_bash","name":"Bash",
                "input":{"command":"make"}}]}}));
        rig.updates();
        rig.feed(json!({"type":"user","parent_tool_use_id":null,
            "uuid":"u1",
            "tool_use_result":{"stdout":"","stderr":"Command aborted before execution",
                "interrupted":true,"isImage":false},
            "message":{"role":"user","content":[{"type":"tool_result",
                "tool_use_id":"toolu_bash",
                "content":"Command aborted before execution<error>Command was aborted before completion</error>"}]}}));
        let notes = notifications(&mut rig);
        assert_eq!(
            last_status(&notes, "toolu_bash"),
            Some(json!(expected)),
            "{notes:?}"
        );
    }
}

// ---------------------------------------------------------------- C-24 (746/759)

/// acp-v2.test.ts: "ends the tool call that a cancelled cycle runs with Claude
/// Code's result, before idle cancelled" and "ends a stopped tool call and its
/// subagent's as cancelled": a result of the work a cancel stopped is an
/// aborted run even when the SDK stamped it `user-rejected`, so v2 gets
/// `cancelled` (v1 keeps `failed`).
#[test]
fn t9a_g3_an_error_result_of_cancelled_work_is_cancelled_on_v2() {
    for (v2, expected) in [(true, "cancelled"), (false, "failed")] {
        let mut rig = if v2 { rig_v2() } else { Rig::new() };
        rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_agent","role":"assistant","model":"claude-opus-5",
            "content":[{"type":"tool_use","id":"toolu_cycle","name":"Bash",
                "input":{"command":"make"}}]}}));
        rig.updates();
        let line = json!({"type":"user","parent_tool_use_id":null,"uuid":"u1",
            "tool_result_meta":[{"id":"toolu_cycle","non_execution_kind":"user-rejected"}],
            "message":{"role":"user","content":[{"type":"tool_result",
                "tool_use_id":"toolu_cycle","is_error":true,"content":"no"}]}});
        send_interrupted_tool_results(
            &mut rig.tasks,
            &line,
            &mut rig.map_state,
            &rig.tx,
            "s",
            None,
        );
        let notes = notifications(&mut rig);
        assert_eq!(
            last_status(&notes, "toolu_cycle"),
            Some(json!(expected)),
            "{notes:?}"
        );
    }
}

// ---------------------------------------------------------------- C-24 (746/747/749/760/761/762/764)

/// acp-v2.test.ts: "the tool calls that a cancel stops" — the closing report of
/// a call the cancel stopped is `cancelled` on v2, including a subagent's
/// command that Claude Code sends no result for, before the idle.
#[test]
fn t9a_g3_a_cancelled_turn_closes_its_open_calls_as_cancelled_on_v2() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_t","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_A","name":"Bash",
            "input":{"command":"sleep 100"}}]}}));
    rig.feed(json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"toolu_B","name":"Read","input":{}}}}));
    rig.updates();
    cancel(&mut rig);
    interrupt_result(&mut rig);
    let notes = notifications(&mut rig);
    assert_eq!(
        last_status(&notes, "toolu_A"),
        Some(json!("cancelled")),
        "{notes:?}"
    );
    assert_eq!(
        last_status(&notes, "toolu_B"),
        Some(json!("cancelled")),
        "{notes:?}"
    );
    let idle = notes
        .iter()
        .find(|value| value["update"]["state"] == json!("idle"))
        .expect("an idle");
    assert_eq!(idle["update"]["stopReason"], json!("cancelled"));
}

/// acp-v2.test.ts: "ends the command of a foreground subagent that the cycle
/// runs, which Claude Code sends no result for, before idle cancelled" — the
/// subagent's command closes `cancelled` in the child's session before the
/// root's idle.
#[test]
fn t9a_g3_a_cancelled_turns_foreground_subagent_command_closes_cancelled_on_v2() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_agent","role":"assistant","model":"claude-opus-5",
        "content":[{"type":"tool_use","id":"toolu_agent","name":"Agent",
            "input":{"description":"Run a slow command","subagent_type":"general-purpose",
                "prompt":"Run it"}}]}}));
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"agent-1",
        "tool_use_id":"toolu_agent","description":"Run a slow command",
        "subagent_type":"general-purpose","task_type":"local_agent",
        "is_backgrounded":false}),
    );
    rig.feed(
        json!({"type":"assistant","parent_tool_use_id":"toolu_agent",
        "message":{"id":"msg_sub","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_child","name":"Bash",
            "input":{"command":"make"}}]}}),
    );
    rig.updates();
    cancel(&mut rig);
    interrupt_result(&mut rig);
    let notes = notifications(&mut rig);
    assert_eq!(
        last_status(&notes, "toolu_child"),
        Some(json!("cancelled")),
        "{notes:?}"
    );
    let idle = notes
        .iter()
        .find(|value| value["update"]["state"] == json!("idle"))
        .expect("an idle");
    assert_eq!(idle["update"]["stopReason"], json!("cancelled"));
}

/// acp-v2.test.ts: "closes the calls of a held turn's follow-up before idle
/// cancelled, and leaves a running background subagent's open" — a background
/// subagent's command stays open at the root's cancel.
#[test]
fn t9a_g3_a_cancelled_turn_keeps_a_background_subagents_call_open_on_v2() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_agent","role":"assistant","model":"claude-opus-5",
        "content":[{"type":"tool_use","id":"toolu_agent","name":"Agent",
            "input":{"description":"Run it","subagent_type":"general-purpose",
                "prompt":"Run it","run_in_background":true}}]}}));
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"agent-1",
        "tool_use_id":"toolu_agent","description":"Run it",
        "subagent_type":"general-purpose","task_type":"local_agent",
        "is_backgrounded":true}),
    );
    rig.feed(
        json!({"type":"assistant","parent_tool_use_id":"toolu_agent",
        "message":{"id":"msg_sub","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_child","name":"Bash",
            "input":{"command":"make"}}]}}),
    );
    rig.updates();
    cancel(&mut rig);
    interrupt_result(&mut rig);
    let notes = notifications(&mut rig);
    assert_eq!(
        last_status(&notes, "toolu_child"),
        None,
        "the background subagent's call stays open: {notes:?}"
    );
}

/// acp-v2.test.ts: "ends the tool call of a background subagent as cancelled
/// when the SDK reports it stopped while the root is idle" — the stopped
/// subagent's command closes `cancelled`.
#[test]
fn t9a_g3_a_stopped_background_subagents_call_closes_cancelled_on_v2() {
    let mut rig = rig_v2();
    rig.map_state.mark_emitted(
        "toolu_child".to_string(),
        json!({"type":"tool_use",
            "id":"toolu_child","name":"Bash","input":{"command":"make"}}),
    );
    rig.map_state
        .books
        .record_dispatched(&json!([{"type":"tool_use","id":"toolu_child","name":"Bash"}]));
    rig.map_state
        .record_tool_call_parent("toolu_child", "toolu_agent");
    rig.map_state.begin_stopped_work();
    let closing = crate::unfinished::close_stopped_subagent_tool_calls(
        &mut rig.map_state,
        true,
        "toolu_agent",
    );
    send_closing(&mut rig.tasks, &mut rig.map_state, closing, &rig.tx, "s");
    let notes = notifications(&mut rig);
    assert_eq!(
        last_status(&notes, "toolu_child"),
        Some(json!("cancelled")),
        "{notes:?}"
    );
}

// ---------------------------------------------------------------- C-24 (750/758)

/// acp-v2.test.ts: "reports a tool call that Claude dropped before it ran as
/// cancelled" — a streamed tool_use that never reaches a complete assistant
/// message closes `cancelled` before the idle, and is not a turn failure.
#[test]
fn t9a_g3_a_dropped_call_closes_cancelled_before_the_idle_on_v2() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    rig.feed(json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"toolu_dropped","name":"Bash","input":{}}}}));
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_answer","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"text","text":"Here is the answer."}]}}));
    rig.feed(result_frame("end_turn"));
    let notes = notifications(&mut rig);
    assert_eq!(
        last_status(&notes, "toolu_dropped"),
        Some(json!("cancelled")),
        "{notes:?}"
    );
    let last = notes
        .iter()
        .rev()
        .find(|value| value["update"].get("sessionUpdate").is_some())
        .expect("an update");
    assert_eq!(last["update"]["state"], json!("idle"));
    assert_eq!(last["update"]["stopReason"], json!("end_turn"));
}

/// acp-v2.test.ts: "closes a tool call that Claude dropped before it ran in a
/// cycle as cancelled, before the cycle's idle" — the cycle's abandoned call
/// closes `cancelled` before its idle.
#[test]
fn t9a_g3_a_dropped_call_in_a_cycle_closes_cancelled_before_its_idle_on_v2() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    notifications(&mut rig);
    rig.feed(result_frame("end_turn"));
    notifications(&mut rig);
    rig.feed(json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"toolu_dropped","name":"Bash","input":{}}}}));
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_followup","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"text","text":"The build passed."}]}}));
    rig.feed(autonomous_result());
    let notes = notifications(&mut rig);
    assert_eq!(
        last_status(&notes, "toolu_dropped"),
        Some(json!("cancelled")),
        "{notes:?}"
    );
    let last = notes
        .iter()
        .rev()
        .find(|value| value["update"].get("sessionUpdate").is_some())
        .expect("an update");
    assert_eq!(last["update"]["state"], json!("idle"));
    assert_eq!(last["update"]["stopReason"], json!("end_turn"));
}
