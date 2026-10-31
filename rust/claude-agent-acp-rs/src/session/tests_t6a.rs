//! T6A: ACP v2 turn events in the session actor (F39; B-57, B-59, B-60, B-112,
//! B-113, B-114, U-81).
//!
//! The harness drives `handle_session_frame` / `settle_events` with the state a
//! running actor holds, on a v2 session (`map_state.v2` reporting), and reads
//! the outbound items: the v2 updates and the prompt intake answers.

use super::tests_t2a::Rig;
use super::*;
use crate::turn_events::{ForegroundEnd, TurnOutcome, TurnUsage, V2IntakeError};

/// A [`Rig`] on a v2 connection.
fn rig_v2() -> Rig {
    let mut rig = Rig::new();
    rig.map_state.v2.set_reporting(true);
    rig
}

/// Every outbound item since the last call, split into the v2 `session/update`
/// values and the intake answers.
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

/// The kinds and states of the updates, as a readable trace.
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

/// Register a v2 prompt and enqueue it, without a v1 reply.
fn queue_v2(rig: &mut Rig, uuid: &str) -> oneshot::Receiver<Result<String, V2IntakeError>> {
    let (tx, rx) = oneshot::channel();
    rig.map_state
        .v2
        .register(uuid, json!([{"type":"text","text":"hi"}]), tx);
    rig.machine.enqueue(Turn::new(uuid.into(), false));
    rx
}

fn drain(rig: &mut Rig) {
    drain_v2(&mut rig.map_state, &rig.tx, "s");
}

fn echo(rig: &mut Rig, uuid: &str) {
    rig.feed(json!({"type":"user","uuid":uuid,"message":{"role":"user",
        "content":[{"type":"text","text":"hi"}]}}));
}

fn result(rig: &mut Rig, stop_reason: &str) {
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":stop_reason,"result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}}));
}

// ---------------------------------------------------------------- M-113 / U-81

/// acp-v2.test.ts: "answers a prompt once Claude Code takes it in, then reports
/// the turn as state" — the prompt's `user_message`, then `running`, then the
/// intake answer; the turn's end is an `idle` with its stop reason and usage.
#[test]
fn t6a_v2_prompt_is_answered_at_intake_then_reported_as_state() {
    let mut rig = rig_v2();
    let mut intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    let (updates, answers) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["user_message", "running"]);
    assert_eq!(updates[0]["messageId"], "p1");
    assert_eq!(updates[0]["content"], json!([{"type":"text","text":"hi"}]));
    assert_eq!(answers, vec![Ok("p1".to_string())]);
    assert!(intake.try_recv().expect("the intake answered").is_ok());

    result(&mut rig, "end_turn");
    let (updates, answers) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle end_turn"]);
    assert_eq!(updates[0]["usage"]["totalTokens"], 5);
    assert_eq!(updates[0]["usage"]["inputTokens"], 3);
    assert_eq!(updates[0]["usage"]["outputTokens"], 2);
    assert_eq!(
        updates[0]["_meta"]["quota"]["token_count"]["totalTokens"],
        5
    );
    assert!(answers.is_empty(), "the intake was answered already");
}

/// acp-v2.test.ts: "cancels the running turn after its last output, and a queued
/// prompt with -32800" — the queued prompt is rejected without a message.
#[test]
fn t6a_a_queued_prompt_cancelled_before_intake_answers_32800() {
    let mut rig = rig_v2();
    let mut intake = queue_v2(&mut rig, "p2");
    let events = rig.machine.cancel();
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
    let (updates, answers) = v2_out(&mut rig);
    assert!(
        updates.is_empty(),
        "a prompt that never ran reports nothing"
    );
    assert_eq!(answers, vec![Err(V2IntakeError::Cancelled)]);
    assert!(matches!(
        intake.try_recv().expect("the intake answered"),
        Err(V2IntakeError::Cancelled)
    ));
}

/// acp-v2.test.ts: "reports a turn that fails after it was taken in as idle with
/// the error stop reason".
#[test]
fn t6a_a_turn_that_fails_after_intake_reports_idle_error() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.feed(json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":"API Error: 529 Overloaded","usage":{}}));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle error"]);
    assert_eq!(updates[0]["error"]["code"], -32603);
    assert_eq!(
        updates[0]["error"]["message"],
        "Internal error: API Error: 529 Overloaded"
    );
}

/// acp-v2.test.ts: "keeps the auth_required code of a turn that fails for a
/// missing login".
#[test]
fn t6a_a_missing_login_turn_reports_the_auth_code() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.feed(json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":"Not logged in · Please run /login","usage":{}}));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle error"]);
    assert_eq!(updates[0]["error"]["code"], -32000);
    assert_eq!(updates[0]["error"]["message"], "Authentication required");
}

// ---------------------------------------------------------------- B-60

/// turn-events.test.ts / acp-v2.test.ts: "reports waiting on a question as
/// requires_action" — a request that blocks the work makes it wait, and its end
/// reports `running` again.
#[test]
fn t6a_a_request_makes_the_work_wait_then_run_again() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.open_input("req-1", None);
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["requires_action"]);
    rig.map_state.v2.close_input("req-1", false);
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"]);
    result(&mut rig, "end_turn");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle end_turn"]);
}

/// acp-v2.test.ts: "reports no running before idle cancelled for a request that
/// a cancel withdraws" — a withdrawn request reports no `running`.
#[test]
fn t6a_a_withdrawn_request_reports_no_running_before_idle_cancelled() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.open_input("req-1", None);
    drain(&mut rig);
    v2_out(&mut rig);
    // The cancel marks the open request; its end reports nothing.
    let _ = rig.machine.cancel();
    rig.map_state.v2.cancel_inputs();
    // Claude Code aborted the request as it stopped the work.
    rig.map_state.v2.close_input("req-1", true);
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert!(updates.is_empty(), "a withdrawn request reports no running");
    rig.feed(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "stop_reason":null,"terminal_reason":"aborted_tools","usage":{}}),
    );
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle cancelled"]);
}

/// acp-v2.test.ts: "reports running when the user answers the request after the
/// cancel" — a request the cancel did not withdraw reports `running`.
#[test]
fn t6a_a_request_answered_after_the_cancel_reports_running() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.open_input("req-1", None);
    drain(&mut rig);
    v2_out(&mut rig);
    // The user cancels while the request is open, then answers it anyway: the
    // answer was not withdrawn, so the work reports `running` before its idle
    // (acp-v2.test.ts:4480).
    let _ = rig.machine.cancel();
    rig.map_state.v2.cancel_inputs();
    rig.map_state.v2.close_input("req-1", false);
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"]);
}

/// acp-v2.test.ts: "keeps the turn running while a background subagent's request
/// is open" — a background subagent's request does not block the work.
#[test]
fn t6a_a_background_subagents_request_does_not_block_the_work() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.note_task("agent-1", true, None);
    rig.map_state.v2.open_input("req-1", Some("agent-1".into()));
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert!(
        updates.is_empty(),
        "a background subagent's request does not hold the turn"
    );
    rig.map_state.v2.close_input("req-1", false);
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert!(updates.is_empty());
}

// ---------------------------------------------------------------- B-114

/// acp-v2.test.ts: "reports a cycle after the turn ended as running, then idle
/// with the cycle's usage".
#[test]
fn t6a_a_cycle_reports_running_then_idle_with_its_usage() {
    let mut rig = rig_v2();
    rig.map_state.v2.start_cycle();
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"]);
    rig.map_state
        .v2
        .set_cycle_end(ForegroundEnd::Ended(TurnOutcome {
            stop_reason: crate::turn::StopReason::EndTurn,
            usage: Some(TurnUsage {
                total_tokens: 11,
                input_tokens: 7,
                output_tokens: 4,
                thought_tokens: None,
                cached_read_tokens: None,
                cached_write_tokens: None,
            }),
            meta: Some(json!({"quota": {"token_count": {"totalTokens": 11}}})),
        }));
    rig.map_state
        .v2
        .after_events(&[TurnEvent::CycleEnded], false);
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle end_turn"]);
    assert_eq!(updates[0]["usage"]["totalTokens"], 11);
}

/// acp-v2.test.ts: "ends a cycle that the client cancels with idle cancelled".
#[test]
fn t6a_a_cancelled_cycle_reports_idle_cancelled() {
    let mut rig = rig_v2();
    rig.map_state.v2.start_cycle();
    drain(&mut rig);
    v2_out(&mut rig);
    rig.map_state
        .v2
        .set_cycle_end(ForegroundEnd::Ended(TurnOutcome {
            stop_reason: crate::turn::StopReason::Cancelled,
            usage: None,
            meta: None,
        }));
    rig.map_state
        .v2
        .after_events(&[TurnEvent::CycleEnded], false);
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle cancelled"]);
}

// ---------------------------------------------------------------- B-113

/// acp-v2.test.ts: "reports the compaction of a follow-up that Claude Code runs
/// after a cancel" — a prompt that Claude Code takes in during a cycle takes the
/// cycle over: the cycle reports nothing more, and only the turn's end ends the
/// work.
#[test]
fn t6a_a_turn_takes_a_running_cycle_over() {
    let mut rig = rig_v2();
    rig.map_state.v2.start_cycle();
    drain(&mut rig);
    v2_out(&mut rig);
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    let (updates, answers) = v2_out(&mut rig);
    // The cycle reported `running` already; the turn takes it over silently.
    assert_eq!(trace(&updates), vec!["user_message"]);
    assert_eq!(answers.len(), 1);
    result(&mut rig, "end_turn");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle end_turn"]);
}

/// acp-v2.test.ts: "reports a later cycle after the client cancelled one" — a
/// turn's end does not leave the foreground work open.
#[test]
fn t6a_a_turn_end_closes_the_work() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    result(&mut rig, "end_turn");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle end_turn"]);
    // The next cycle starts afresh.
    rig.map_state.v2.start_cycle();
    drain(&mut rig);
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"]);
}

// ---------------------------------------------------------------- v1 inert

/// On v1 the same frames report no v2 state: the wire stays byte-identical.
#[test]
fn t6a_v1_reports_no_v2_state() {
    let mut rig = Rig::new();
    rig.machine.enqueue(Turn::new("p1".into(), false));
    rig.feed(json!({"type":"user","uuid":"p1","message":{"role":"user",
        "content":[{"type":"text","text":"hi"}]}}));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok","usage":{}}));
    let (updates, answers) = v2_out(&mut rig);
    assert!(updates.is_empty());
    assert!(answers.is_empty());
}

/// acp-v2.test.ts: "answers a queued prompt once Claude Code takes it in, after
/// the turn before it" — the second prompt waits for the first turn's end and is
/// answered when its own echo arrives.
#[test]
fn t9a_a_queued_v2_prompt_is_answered_after_the_turn_before_it() {
    let mut rig = rig_v2();
    let mut first = queue_v2(&mut rig, "p1");
    let mut second = queue_v2(&mut rig, "p2");
    echo(&mut rig, "p1");
    let (updates, answers) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["user_message", "running"]);
    assert_eq!(answers, vec![Ok("p1".to_string())]);
    assert!(
        second.try_recv().is_err(),
        "the queued prompt is not answered before its turn"
    );
    result(&mut rig, "end_turn");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle end_turn"]);
    echo(&mut rig, "p2");
    let (updates, answers) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["user_message", "running"]);
    assert_eq!(answers, vec![Ok("p2".to_string())]);
    assert!(first.try_recv().expect("p1 answered").is_ok());
    assert!(second.try_recv().expect("p2 answered").is_ok());
    result(&mut rig, "end_turn");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["idle end_turn"]);
}
