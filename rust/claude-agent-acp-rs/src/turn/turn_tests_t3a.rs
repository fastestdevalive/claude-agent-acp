//! Turn-machine tests added by Turn 3 / lane A (session failures, plan mode,
//! clear-context restart, background tasks). Kept apart from `turn_tests.rs`,
//! whose `inv_16|23|29|30|31|steer_*` cases stay untouched.

use super::*;

fn failed(events: &[TurnEvent]) -> Vec<(&str, FailureKind)> {
    events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::Failed {
                prompt_uuid, kind, ..
            } => Some((prompt_uuid.as_str(), *kind)),
            _ => None,
        })
        .collect()
}

/// B-119: only an error result carrying "Please run /login" is the CLI's
/// sign-in prompt; a successful answer that mentions the command is not.
#[test]
fn t3a_login_text_without_is_error_is_an_ordinary_success() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let events = machine.on_result(
        &serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "stop_reason": "end_turn",
            "result": "Run /login (Please run /login) when asked",
            "usage": {"output_tokens": 3},
        }),
        false,
    );
    assert!(
        failed(&events).is_empty(),
        "a non-error success naming /login is not an auth failure: {events:?}"
    );
}

/// B-119: the error result is the auth failure.
#[test]
fn t3a_login_text_with_is_error_fails_auth_required() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let events = machine.on_result(
        &serde_json::json!({
            "type": "result", "subtype": "success", "is_error": true,
            "result": "Please run /login", "usage": {"output_tokens": 1},
        }),
        false,
    );
    assert_eq!(failed(&events), vec![("p1", FailureKind::AuthRequired)]);
}

/// V-06 — a worker shutdown at stream end fails the active turn that is not
/// held; a held turn and an absent turn give no event (the caller publishes the
/// session-scoped failure instead).
#[test]
fn t3a_worker_shutdown_fails_only_an_unheld_active_turn() {
    let mut machine = TurnMachine::new();
    assert!(machine.fail_active_worker_shutdown().is_empty(), "no turn");
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let events = machine.fail_active_worker_shutdown();
    assert_eq!(failed(&events), vec![("p1", FailureKind::WorkerShutdown)]);
    assert!(
        machine.fail_active_worker_shutdown().is_empty(),
        "the turn is settled now"
    );
}

fn plain_result() -> Value {
    serde_json::json!({"type": "result", "subtype": "success", "is_error": false,
        "stop_reason": "end_turn", "num_turns": 1,
        "usage": {"input_tokens": 10, "output_tokens": 5}})
}

fn autonomous_diagnostic() -> Value {
    serde_json::json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
        "origin": {"kind": "task-notification"}, "num_turns": 1,
        "usage": {"input_tokens": 1, "output_tokens": 1}})
}

fn settled_with(events: &[TurnEvent]) -> Vec<(String, StopReason, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::Settled {
                prompt_uuid,
                stop_reason,
                usage,
                ..
            } => Some((prompt_uuid.clone(), *stop_reason, usage.output_tokens)),
            _ => None,
        })
        .collect()
}

/// B-76 — the keep-planning diagnostic of a plain turn settles it `cancelled`
/// with the usage the diagnostic carried (not as a provider failure), and owes
/// the trailing idle like any user-turn result.
#[test]
fn t3a_exit_plan_interruption_settles_the_turn_cancelled_and_owes_its_idle() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let diagnostic = serde_json::json!({"type": "result", "subtype": "error_during_execution",
        "is_error": true, "usage": {"input_tokens": 3, "output_tokens": 4}});
    let begin = machine.on_result_begin(&diagnostic);
    machine.note_exit_plan_interruption(ExitPlanVerdict::KeepPlanning);
    let events = machine.on_result_finish(&diagnostic, false, begin);
    assert_eq!(
        settled_with(&events),
        vec![("p1".to_string(), StopReason::Cancelled, 4)]
    );
    assert!(failed(&events).is_empty());
    // The trailing idle is absorbed, not read as an abandoned next turn.
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert!(machine.on_idle().is_empty(), "p2 stays active");
    assert!(machine.has_active());
}

/// B-76 — a held turn's follow-up cycle that planned ends the turn `cancelled`
/// with the usage its own result recorded, once nothing it spawned is live.
#[test]
fn t3a_exit_plan_interruption_in_a_held_follow_up_ends_the_held_turn_cancelled() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("t1", true);
    machine.on_result(&plain_result(), false);
    machine.on_task_ended("t1");
    assert!(machine.active_is_held());
    let diagnostic = autonomous_diagnostic();
    let begin = machine.on_result_begin(&diagnostic);
    assert!(begin.autonomous);
    machine.note_exit_plan_interruption(ExitPlanVerdict::KeepPlanning);
    let events = machine.on_result_finish(&diagnostic, false, begin);
    assert_eq!(
        settled_with(&events),
        vec![("p1".to_string(), StopReason::Cancelled, 5)],
        "the usage of the turn's own result, not the diagnostic's"
    );
}

/// B-76 — an autonomous diagnostic the session did not mark leaves the held
/// turn held (it only settles at a follow-up that says it ran a turn), and a
/// mark left by an earlier result does not carry over to a later one.
#[test]
fn t3a_unmarked_autonomous_diagnostic_leaves_the_held_turn_held() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("t1", true);
    machine.on_result(&plain_result(), false);
    // The subagent is still live: the hold cannot drain at the diagnostic.
    let mut placeholder = autonomous_diagnostic();
    placeholder["num_turns"] = serde_json::json!(0);
    machine.note_exit_plan_interruption(ExitPlanVerdict::KeepPlanning);
    let begin = machine.on_result_begin(&placeholder);
    let _ = machine.on_result_finish(&placeholder, false, begin);
    let diagnostic = autonomous_diagnostic();
    let begin = machine.on_result_begin(&diagnostic);
    let events = machine.on_result_finish(&diagnostic, false, begin);
    assert!(settled_with(&events).is_empty(), "{events:?}");
    assert!(machine.active_is_held());
}

/// B-74 / B-14 — an accepted clear-context plan takes the turn out of the
/// machine at the interruption result, carrying the usage that result added;
/// nothing settles and no failure is raised, and the interruption's trailing
/// idle is still owed.
#[test]
fn t3a_context_reset_hands_the_turn_on_with_the_result_usage() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let diagnostic = serde_json::json!({"type": "result", "subtype": "error_during_execution",
        "is_error": true, "usage": {"input_tokens": 3, "output_tokens": 4}});
    let begin = machine.on_result_begin(&diagnostic);
    machine.note_exit_plan_interruption(ExitPlanVerdict::ContextReset);
    let events = machine.on_result_finish(&diagnostic, false, begin);
    assert!(events.is_empty(), "nothing settles or fails: {events:?}");
    assert!(!machine.has_active(), "the turn left the machine");
    let turn = machine.take_restart_turn().expect("the turn is handed on");
    assert!(machine.take_restart_turn().is_none(), "once");
    match turn.restart_settled(StopReason::Cancelled) {
        TurnEvent::Settled {
            prompt_uuid, usage, ..
        } => {
            assert_eq!(prompt_uuid, "p1");
            assert_eq!((usage.input_tokens, usage.output_tokens), (3, 4));
        }
        other => panic!("a settle: {other:?}"),
    }
}

/// B-74 — a held turn's follow-up that planned and cleared the context hands the
/// held turn on with its hold dropped: the fresh query has none of its
/// background tasks, so the hold could never drain there.
#[test]
fn t3a_context_reset_in_a_held_follow_up_drops_the_hold() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("t1", true);
    machine.on_result(&plain_result(), false);
    assert!(machine.active_is_held());
    let diagnostic = autonomous_diagnostic();
    let begin = machine.on_result_begin(&diagnostic);
    machine.note_exit_plan_interruption(ExitPlanVerdict::ContextReset);
    let events = machine.on_result_finish(&diagnostic, false, begin);
    assert!(settled_with(&events).is_empty() && failed(&events).is_empty());
    let turn = machine
        .take_restart_turn()
        .expect("the held turn is handed on");
    assert!(!machine.has_active());
    // The hold is gone: adopting it into a fresh machine, its echo activates it
    // as a plain turn that the continuation's own result settles.
    let mut fresh = TurnMachine::new();
    fresh.adopt_restarted_turn(turn);
    fresh.on_echo("p1");
    assert!(!fresh.active_is_held());
    let events = fresh.on_result(&plain_result(), false);
    let settled = settled_with(&events);
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].0, "p1");
    assert_eq!(settled[0].1, StopReason::EndTurn);
    // The usage of both contexts: the held turn's own result (5 output tokens)
    // and the continuation's.
    assert_eq!(settled[0].2, 10);
}

// ------------------------------------------------ T3FA

/// F22 — the prompts queued behind the plan turn are not dropped by the
/// restart: they come out of the old machine in order and, adopted into the
/// fresh one behind the plan turn, run and settle there.
#[test]
fn t3fa_prompts_queued_behind_the_plan_turn_survive_the_restart() {
    let mut old = TurnMachine::new();
    old.enqueue(Turn::new("plan".into(), false));
    old.on_echo("plan");
    old.enqueue(Turn::new("q1".into(), false).with_frame(serde_json::json!({"n": 1})));
    old.enqueue(Turn::new("q2".into(), false).with_frame(serde_json::json!({"n": 2})));
    let turn = old.take_active_for_restart().expect("the plan turn");
    let mut queued = old.take_restart_queue();
    assert!(old.take_restart_queue().is_empty(), "once");
    let uuids: Vec<&str> = queued.iter().map(Turn::prompt_uuid).collect();
    assert_eq!(uuids, ["q1", "q2"], "in order");
    assert_eq!(queued[0].take_frame(), Some(serde_json::json!({"n": 1})));
    assert!(!old.has_unsettled(), "the old machine keeps nothing");

    let mut fresh = TurnMachine::new();
    fresh.adopt_restarted_turn(turn);
    for turn in queued {
        fresh.enqueue(turn);
    }
    for uuid in ["plan", "q1", "q2"] {
        fresh.on_echo(uuid);
        let events = fresh.on_result(&plain_result(), false);
        let settled = settled_with(&events);
        assert_eq!(settled.len(), 1, "{uuid}: {events:?}");
        assert_eq!(settled[0].0, uuid);
    }
}

/// F22 — a held turn's follow-up that planned and cleared the context takes the
/// running cycle over: no cycle-end is reported for it (`continuesPlan`), while
/// "keep planning" still ends the cycle.
#[test]
fn t3fa_context_reset_of_a_held_follow_up_reports_no_cycle_end() {
    let run = |verdict: ExitPlanVerdict| {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        machine.on_task_started("t1", true);
        machine.on_result(&plain_result(), false);
        assert!(machine.active_is_held());
        machine.autonomous_cycle = true;
        let diagnostic = autonomous_diagnostic();
        let begin = machine.on_result_begin(&diagnostic);
        machine.note_exit_plan_interruption(verdict);
        machine.on_result_finish(&diagnostic, false, begin)
    };
    let ended = |events: &[TurnEvent]| events.iter().any(|e| matches!(e, TurnEvent::CycleEnded));
    assert!(!ended(&run(ExitPlanVerdict::ContextReset)));
    assert!(ended(&run(ExitPlanVerdict::KeepPlanning)));
}

/// M-65 — the steer diagnostic check mirrors the machine's steer lane: only an
/// error result while the steered turn still has echoes pending.
#[test]
fn t3fa_steer_diagnostic_needs_an_error_and_a_pending_echo() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let error = serde_json::json!({"type": "result", "subtype": "success", "is_error": true});
    assert!(!machine.result_is_steer_diagnostic(&error), "not steered");
    assert!(machine.mark_steered("s1"));
    assert!(machine.result_is_steer_diagnostic(&error));
    assert!(!machine.result_is_steer_diagnostic(&plain_result()));
}
