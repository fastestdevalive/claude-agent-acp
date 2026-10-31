//! T6G: the v2 foreground-work fixes and the v1 failure mapping, at the session
//! actor's level (B-60, B-57, B-59, F39; upstream `acp-v2.test.ts`).

use super::tests_t2a::Rig;
use super::*;
use crate::turn::StopReason;
use crate::turn_events::{ForegroundEnd, JsonRpcError, V2IntakeError};

/// A [`Rig`] on a v2 connection.
fn rig_v2() -> Rig {
    let mut rig = Rig::new();
    rig.map_state.v2.set_reporting(true);
    rig
}

fn trace(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|update| {
            let kind = update["sessionUpdate"].as_str().unwrap_or("");
            match kind {
                "state_update" => match update["stopReason"].as_str() {
                    Some(reason) => format!("{} {reason}", update["state"].as_str().unwrap_or("")),
                    None => update["state"].as_str().unwrap_or("").to_string(),
                },
                _ => kind.to_string(),
            }
        })
        .collect()
}

fn v2_out(rig: &mut Rig) -> (Vec<Value>, Vec<Result<String, V2IntakeError>>) {
    let mut updates = Vec::new();
    let mut answers = Vec::new();
    while let Ok(item) = rig.rx.try_recv() {
        match item {
            SessionOutbound::RawUpdate(value) => updates.push(value["update"].clone()),
            SessionOutbound::V2Intake {
                prompt_uuid: _,
                reply,
                result,
            } => {
                let _ = reply.send(result.clone());
                answers.push(result);
            }
            SessionOutbound::Update(notif) => {
                updates.push(serde_json::to_value(&notif.update).unwrap());
            }
            _ => {}
        }
    }
    (updates, answers)
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

fn subagent_started(task: &str) -> Value {
    json!({"type":"system","subtype":"task_started","task_id":task,
        "tool_use_id":"toolu_AG","description":"Research","subagent_type":"general-purpose",
        "task_type":"local_agent","is_backgrounded":true})
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

// ---------------------------------------------------------------- A HIGH 3

/// acp-v2.test.ts: "ends a turn held for its background subagents at its
/// result, and reports its follow-up as a cycle of its own" — v1 still holds
/// the answer, but the work ends at the result (`settleOrDefer`,
/// `acp-agent.ts:5203-5213`).
#[test]
fn t6g_a_held_turn_ends_its_work_at_its_result() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description":"Research","prompt":"x"}),
    );
    rig.feed(subagent_started("sub1"));
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    let states: Vec<String> = trace(&updates)
        .into_iter()
        .filter(|kind| kind.starts_with("idle") || kind == "running")
        .collect();
    assert_eq!(
        states,
        vec!["idle end_turn"],
        "the work ends at the result, though the answer is held: {updates:?}"
    );
    assert!(rig.machine.has_active(), "v1 still holds the turn open");

    // The follow-up Claude Code starts after the subagent finished is its own
    // work: `running`, then its result ends it.
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"sub1",
        "status":"completed","tool_use_id":"toolu_AG"}),
    );
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_f","role":"assistant","model":"claude-sonnet-4-5",
        "content":[{"type":"text","text":"done"}]}}));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        trace(&updates)
            .into_iter()
            .filter(|kind| kind == "running" || kind.starts_with("idle"))
            .collect::<Vec<_>>(),
        vec!["running"],
        "{updates:?}"
    );
    rig.feed(autonomous_result());
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        trace(&updates)
            .into_iter()
            .filter(|kind| kind.starts_with("idle"))
            .collect::<Vec<_>>(),
        vec!["idle end_turn"],
        "{updates:?}"
    );
    assert!(!rig.machine.has_active(), "the follow-up settled the hold");
}

/// acp-v2.test.ts: "a steer of a held turn resumes its work" — the steer makes
/// the turn's continuation the work again (`startForeground`,
/// `acp-agent.ts:4380-4386`).
#[test]
fn t6g_a_steer_of_a_held_turn_resumes_its_work() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description":"Research","prompt":"x"}),
    );
    rig.feed(subagent_started("sub1"));
    rig.feed(result_frame("end_turn"));
    v2_out(&mut rig);
    assert!(rig.machine.mark_steered("steer-1"));
    assert_eq!(rig.machine.take_steer_resumed().as_deref(), Some("p1"));
    rig.map_state.v2.note_resumed("p1");
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"], "{updates:?}");
}

// ---------------------------------------------------------------- A MEDIUM

/// acp-v2.test.ts: "a clear-context restart carries no open request or task of
/// the old context" — the withdrawn forms do not keep the work waiting.
#[test]
fn t6g_a_restart_withdraws_the_old_contexts_open_forms() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.note_task("sub1", false, None);
    rig.map_state.v2.open_input("req-1", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["requires_action"]);

    rig.map_state.v2.reset_for_fresh_context();
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    // NEW-4: the CLI aborted the old context's forms with the old query, not
    // with a cancel, so the work reports `running` for its continuation
    // (`acp-agent.ts:9600-9604`).
    assert_eq!(
        trace(&updates),
        vec!["running"],
        "a withdrawn form reports running: {updates:?}"
    );
    // The work runs again in the fresh context, not stuck on the old forms.
    rig.map_state.v2.open_input("req-2", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["requires_action"]);
    rig.map_state.v2.close_input("req-2", false);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"]);
}

/// acp-v2.test.ts: "reports running when the user answers the request after the
/// cancel" (4480) — the answer is not a withdrawal, so the work goes on.
#[test]
fn t6g_an_answer_after_a_cancel_reports_running() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.open_input("req-1", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    v2_out(&mut rig);
    rig.map_state.v2.cancel_inputs();
    rig.map_state.v2.close_input("req-1", false);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"]);
}

/// acp-v2.test.ts: "reports no running before idle cancelled for a request that
/// a cancel withdraws" (4566) — the CLI aborted it as it stopped the work.
#[test]
fn t6g_a_withdrawn_request_reports_no_running() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.open_input("req-1", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    v2_out(&mut rig);
    rig.map_state.v2.cancel_inputs();
    rig.map_state.v2.close_input("req-1", true);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert!(updates.is_empty(), "no running: {updates:?}");
}

// ---------------------------------------------------------------- A LOW

/// A parent chain that cycles still blocks the work (`blocksForeground`,
/// `acp-agent.ts:11973-11982`).
#[test]
fn t6g_a_cyclic_parent_chain_blocks_the_work() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.note_task("a", false, Some("b"));
    rig.map_state.v2.note_task("b", false, Some("a"));
    rig.map_state.v2.open_input("req-1", Some("a".to_string()));
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["requires_action"], "{updates:?}");
}

/// v1 sessions keep no v2 bookkeeping: an open request or task of a v1 session
/// is not recorded (no growth over the session's life).
#[test]
fn t6g_a_v1_session_records_no_v2_state() {
    let mut rig = Rig::new();
    rig.map_state.v2.open_input("req-1", None);
    rig.map_state.v2.note_task("sub1", false, None);
    rig.map_state
        .v2
        .set_cycle_end(ForegroundEnd::Ended(crate::turn_events::TurnOutcome {
            stop_reason: StopReason::EndTurn,
            usage: None,
            meta: None,
        }));
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, answers) = v2_out(&mut rig);
    assert!(updates.is_empty());
    assert!(answers.is_empty());
}

// ---------------------------------------------------------------- A MEDIUM 6

/// acp-v2.test.ts:2138 — a failed turn's v2 `idle` carries the v1 error:
/// `Internal error: API Error: 529 Overloaded`.
#[test]
fn t6g_a_failed_turn_reports_the_v1_error_message() {
    // NEW-5: v1 and v2 read one mapping (`session_error_jsonrpc`).
    let error = crate::agent::session_error_jsonrpc(&SessionError::PromptFailed {
        kind: crate::turn::FailureKind::ProviderError,
        message: "API Error: 529 Overloaded".to_string(),
        assistant_had_error: false,
    });
    assert_eq!(error.code, -32603);
    assert_eq!(error.message, "Internal error: API Error: 529 Overloaded");
    assert_eq!(error.data, None);
    // An assistant frame that carried an `error` adds its kind.
    let error = crate::agent::session_error_jsonrpc(&SessionError::PromptFailed {
        kind: crate::turn::FailureKind::ProviderError,
        message: "API Error: 529 Overloaded".to_string(),
        assistant_had_error: true,
    });
    assert_eq!(error.data, Some(json!({"errorKind": "provider_error"})));
    let error = crate::agent::session_error_jsonrpc(&SessionError::PromptFailed {
        kind: crate::turn::FailureKind::AuthRequired,
        message: "x".to_string(),
        assistant_had_error: false,
    });
    assert_eq!(error.code, -32000);
    assert_eq!(error.message, "Authentication required");
}

/// The v2 `idle error` of a failed turn carries that same error
/// (`v2Foreground.failed`, `v2/prompt.ts:102-109`).
#[test]
fn t6g_a_failed_turn_reports_the_v1_error_as_idle() {
    let error = JsonRpcError {
        code: -32603,
        message: "Internal error: API Error: 529 Overloaded".to_string(),
        data: None,
    };
    let update = crate::v2::prompt::failed_update(&error);
    assert_eq!(update["state"], "idle");
    assert_eq!(update["stopReason"], "error");
    assert_eq!(update["error"]["message"], error.message);
}

// ---------------------------------------------------------------- G4 / B-113

/// The upstream turn-events trace: `inserted <uuid>` for a `user_message`, the
/// foreground state (with its stop reason), and the update kinds.
fn b113_trace(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|update| {
            let kind = update["sessionUpdate"].as_str().unwrap_or("");
            match kind {
                "user_message" => {
                    format!("inserted {}", update["messageId"].as_str().unwrap_or(""))
                }
                "state_update" => match update["stopReason"].as_str() {
                    Some(reason) => format!("{} {reason}", update["state"].as_str().unwrap_or("")),
                    None => update["state"].as_str().unwrap_or("").to_string(),
                },
                "agent_message_chunk" => "chunk".to_string(),
                _ => String::new(),
            }
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn apply_turn_events(rig: &mut Rig, events: Vec<TurnEvent>) {
    rig.map_state.v2.after_events(&events, false);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
}

/// Feed one frame through the real `dispatch_stream` (the lifecycle branch lives
/// there, not in `handle_session_frame`).
async fn feed_stream(rig: &mut Rig, line: Value) {
    let (control_tx, _control_rx) = mpsc::unbounded_channel();
    dispatch_stream(
        StreamMsg::new(line),
        &control_tx,
        &mut rig.machine,
        &mut rig.map_state,
        &mut rig.live_background,
        &mut rig.pending,
        &mut rig.emitted_assistant_text,
        &mut rig.assistant_had_error,
        &rig.tx,
        "s",
        &rig.high_water,
        &mut rig.tasks,
        &mut rig.ext,
    )
    .await;
}

fn lifecycle_started(uuid: &str) -> Value {
    json!({"type":"command_lifecycle","command_uuid":uuid,"state":"started"})
}

/// `turn-events.test.ts` > "reports a turn inserted when Claude Code starts it,
/// before output that comes before its echo": the `started` lifecycle inserts
/// the turn (and answers the intake) before the answer's chunk, which itself
/// precedes the echo.
#[tokio::test]
async fn g4_b113_inserted_before_output_that_precedes_the_echo() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    feed_stream(&mut rig, lifecycle_started("p1")).await;
    let (updates, answers) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["inserted p1", "running"]);
    assert_eq!(answers, vec![Ok("p1".to_string())]);

    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_answer","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"Hello"}],
            "usage":{"input_tokens":1,"output_tokens":1}}}));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["chunk"]);

    echo(&mut rig, "p1");
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["idle end_turn"]);
}

/// `turn-events.test.ts` > "reports a turn inserted only after a held turn
/// before it ended": a second prompt's `started` does not insert while the
/// first turn is held; its echo hands the first turn off and inserts it.
#[tokio::test]
async fn g4_b113_inserted_only_after_a_held_turn_before_it_ended() {
    let mut rig = rig_v2();
    let _first = queue_v2(&mut rig, "p1");
    feed_stream(&mut rig, lifecycle_started("p1")).await;
    echo(&mut rig, "p1");
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description":"Research","prompt":"x"}),
    );
    rig.feed(subagent_started("sub1"));
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        b113_trace(&updates),
        vec!["inserted p1", "running", "idle end_turn"],
        "the held turn's work ends at its result"
    );
    assert!(rig.machine.has_active(), "v1 still holds the turn");

    let _second = queue_v2(&mut rig, "p2");
    feed_stream(&mut rig, lifecycle_started("p2")).await;
    let (updates, _) = v2_out(&mut rig);
    assert!(
        updates.is_empty(),
        "the held turn owns the foreground until the hand-off: {updates:?}"
    );

    echo(&mut rig, "p2");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["inserted p2", "running"]);
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["idle end_turn"]);
}

/// `turn-events.test.ts` > "reports waiting on a request that Claude Code opens
/// for a started turn before its echo": the inserted turn waits at once, then
/// resumes when the request is answered, all before its echo.
#[tokio::test]
async fn g4_b113_waits_on_a_request_opened_before_the_echo() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    feed_stream(&mut rig, lifecycle_started("p1")).await;
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["inserted p1", "running"]);

    rig.map_state.v2.open_input("req-bg", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["requires_action"]);

    rig.map_state.v2.close_input("req-bg", false);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["running"]);

    echo(&mut rig, "p1");
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["idle end_turn"]);
}

/// `turn-events.test.ts` > "awaits the user while any request in the session is
/// open, also one opened before the turn": the new turn starts `running` then
/// `requires_action` for the earlier request, stays waiting through its own
/// answered request, and resumes only when the earlier one is answered too.
#[tokio::test]
async fn g4_b113_awaits_a_request_opened_before_the_turn() {
    let mut rig = rig_v2();
    let _first = queue_v2(&mut rig, "p1");
    feed_stream(&mut rig, lifecycle_started("p1")).await;
    echo(&mut rig, "p1");
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        b113_trace(&updates),
        vec!["inserted p1", "running", "idle end_turn"]
    );

    // A request no active turn opened, such as a background task's.
    rig.map_state.v2.open_input("req-bg", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert!(updates.is_empty(), "no work to report: {updates:?}");

    let _second = queue_v2(&mut rig, "p2");
    feed_stream(&mut rig, lifecycle_started("p2")).await;
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        b113_trace(&updates),
        vec!["inserted p2", "running", "requires_action"]
    );

    // Its own request opens and is answered; the earlier one still blocks it.
    rig.map_state.v2.open_input("req-own", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert!(updates.is_empty(), "already waiting: {updates:?}");
    rig.map_state.v2.close_input("req-own", false);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert!(updates.is_empty(), "still waiting: {updates:?}");

    rig.map_state.v2.close_input("req-bg", false);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["running"]);

    echo(&mut rig, "p2");
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(b113_trace(&updates), vec!["idle end_turn"]);
}

/// `turn-events.test.ts` > "does not insert a turn that a cancel ended while the
/// previous turn was handed off": the swept queued turn's intake answers
/// `cancelled` and no `user_message` is emitted for it.
#[test]
fn g4_b113_a_cancelled_queued_turn_is_not_inserted() {
    let mut rig = rig_v2();
    let _first = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    let _second = queue_v2(&mut rig, "p2");
    v2_out(&mut rig);

    let events = rig.machine.cancel();
    apply_turn_events(&mut rig, events);
    let (updates, answers) = v2_out(&mut rig);
    assert!(
        updates
            .iter()
            .all(|update| update["sessionUpdate"] != "user_message"),
        "the cancelled turn is never inserted: {updates:?}"
    );
    assert!(
        answers
            .iter()
            .any(|answer| answer == &Err(V2IntakeError::Cancelled)),
        "the swept turn's intake answers cancelled: {answers:?}"
    );
}
