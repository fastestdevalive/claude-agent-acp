//! Turn-machine tests added by Turn 2 / lane A (carried F12 steering R811-6).
//! Kept apart from `turn_tests.rs`, whose `inv_16|23|29|30|31|steer_*` cases
//! stay untouched.

use super::*;

fn stamped_result(uuid: &str, uuids: &[&str]) -> Value {
    serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "stop_reason": "end_turn",
        "user_message_uuid": uuid,
        "user_message_uuids": uuids,
        "usage": { "output_tokens": 5, "input_tokens": 10 },
    })
}

fn settled(events: &[TurnEvent]) -> Vec<(&str, StopReason)> {
    events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::Settled {
                prompt_uuid,
                stop_reason,
                ..
            } => Some((prompt_uuid.as_str(), *stop_reason)),
            _ => None,
        })
        .collect()
}

/// Issue #1114 — the CLI never replays the steered message's echo; a result
/// stamped with the steer's uuid proves the steered work ran: it settles the
/// turn right there and owes its trailing idle like any settling result.
#[test]
fn t2a_result_naming_the_steer_settles_without_its_echo() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(machine.mark_steered("s1"));

    // The interrupted cycle's result names only the original send: stays on the lane.
    let events = machine.on_result(&stamped_result("p1", &["p1"]), false);
    assert!(
        settled(&events).is_empty(),
        "interrupted result must not settle"
    );
    assert_eq!(machine.owed_trailing_idles, 0);

    // No echo for the steer; its own result names it.
    let events = machine.on_result(&stamped_result("s1", &["s1"]), false);
    assert_eq!(settled(&events), vec![("p1", StopReason::EndTurn)]);
    assert_eq!(machine.owed_trailing_idles, 1, "owes its trailing idle");

    machine.on_idle();
    assert_eq!(machine.owed_trailing_idles, 0, "the trailer is absorbed");
}

/// CLI 2.1.286 — a steer that joined the running cycle: ONE result stamped with
/// both the prompt and the steer, with or without a mid-cycle echo.
#[test]
fn t2a_steer_joined_the_running_cycle_settles_at_the_one_result() {
    for echo in [false, true] {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        assert!(machine.mark_steered("s1"));
        if echo {
            machine.on_echo("s1");
        }
        let events = machine.on_result(&stamped_result("p1", &["p1", "s1"]), false);
        assert_eq!(
            settled(&events),
            vec![("p1", StopReason::EndTurn)],
            "echo={echo}"
        );
        assert_eq!(machine.owed_trailing_idles, 1, "echo={echo}");
        assert_eq!(machine.accumulated_usage.input_tokens, 10, "one cycle ran");
    }
}

// ---------------------------------------------------------------- F14

fn plain_result() -> Value {
    serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false, "stop_reason": "end_turn",
        "usage": { "output_tokens": 5, "input_tokens": 3 },
    })
}

fn background_placement() -> TaskPlacement<'static> {
    TaskPlacement {
        backgrounded: Some(true),
        parent_task_id: Some("root"),
        task_type: Some("local_agent"),
    }
}

/// A turn p1 held open for the background subagent `t1` of the Agent call `AG`.
fn held_for_subagent() -> TurnMachine {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.register_live_task("t1", Some("AG"), true, background_placement());
    machine.on_result(&plain_result(), false);
    assert!(machine.has_active(), "p1 is held for t1");
    machine
}

/// N-17 — a registered task keeps its parent call, placement and parent task.
#[test]
fn t2a_register_live_task_records_the_placement() {
    let mut machine = TurnMachine::new();
    machine.register_live_task("t1", Some("AG"), true, background_placement());
    assert_eq!(machine.live_task_parent("t1"), Some("AG"));
    assert_eq!(machine.parent_task_id("t1"), Some("root"));
    assert!(machine.runs_in_background("AG"));
    assert_eq!(
        machine
            .live_subagent_parents()
            .get("t1")
            .map(String::as_str),
        Some("AG")
    );
    // A shell task is live too, but attributes no subagent.
    machine.register_live_task("sh", None, false, TaskPlacement::default());
    assert!(!machine.live_subagent_parents().contains_key("sh"));
}

/// N-43 — a level without the subagent ends the hold (attribution stays); a level
/// that lists it un-ends it, and the idle then holds again.
#[test]
fn t2a_level_signal_ends_and_unends_the_hold() {
    let mut machine = held_for_subagent();
    let none = std::collections::HashSet::new();
    assert!(machine.reconcile_background_tasks(&none).is_empty());
    assert!(
        !machine.runs_in_background("AG"),
        "a level-ended subagent is no running background subagent"
    );
    assert_eq!(
        machine.live_task_parent("t1"),
        Some("AG"),
        "attribution stays"
    );
    machine.owed_trailing_idles = 0;
    let events = machine.on_idle();
    assert_eq!(
        settled(&events),
        vec![("p1", StopReason::EndTurn)],
        "the hold released"
    );

    let mut machine = held_for_subagent();
    machine.reconcile_background_tasks(&none);
    let live: std::collections::HashSet<String> = ["t1".to_string()].into();
    machine.reconcile_background_tasks(&live);
    assert!(machine.runs_in_background("AG"), "the level proves it live");
    machine.owed_trailing_idles = 0;
    assert!(settled(&machine.on_idle()).is_empty(), "still held");
}

/// `acp-agent.test.ts`: "resolves at the result when the subagent already settled
/// during the turn" — a subagent that ended before the result does not hold the
/// turn: its own result settles `end_turn` (no trailing idle needed).
#[test]
fn t2a_result_settles_when_the_subagent_already_settled() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.register_live_task("t1", Some("AG"), true, background_placement());
    let ended = machine.on_task_ended("t1");
    assert!(
        settled(&ended).is_empty(),
        "the turn still runs after its subagent ends"
    );
    let events = machine.on_result(&plain_result(), false);
    assert_eq!(
        settled(&events),
        vec![("p1", StopReason::EndTurn)],
        "the result settles the turn; the trailing idle is not needed"
    );
    assert!(!machine.has_active());
}

/// N-43 — an absent task that is not a subagent is dropped from the live set.
#[test]
fn t2a_level_signal_drops_absent_shell_tasks() {
    let mut machine = TurnMachine::new();
    machine.register_live_task("sh", None, false, TaskPlacement::default());
    machine.register_live_task("sub", Some("AG"), true, TaskPlacement::default());
    let dropped = machine.reconcile_background_tasks(&std::collections::HashSet::new());
    assert_eq!(dropped, vec!["sh".to_string()]);
    assert_eq!(machine.live_task_parent("sub"), Some("AG"));
    assert_eq!(machine.live_task_parent("sh"), None);
}

/// N-43 — the activation sweep arms an ended entry at the first activation and
/// deletes it at the second; a local-only command does not advance it.
#[test]
fn t2a_activation_sweep_deletes_ended_entries_on_the_second_turn() {
    let mut machine = TurnMachine::new();
    machine.register_live_task("sub", Some("AG"), true, TaskPlacement::default());
    machine.reconcile_background_tasks(&std::collections::HashSet::new());

    machine.enqueue(Turn::new("local".into(), true));
    machine.on_echo("local");
    machine.on_result(&plain_result(), false);
    assert_eq!(
        machine.live_task_parent("sub"),
        Some("AG"),
        "a local command does not sweep"
    );

    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert_eq!(
        machine.live_task_parent("sub"),
        Some("AG"),
        "armed at the first activation"
    );
    machine.on_result(&plain_result(), false);
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert_eq!(
        machine.live_task_parent("sub"),
        None,
        "deleted at the second"
    );
    assert!(
        machine.resume_live_task("sub"),
        "a swept subagent stays resumable"
    );
}

/// N-43 — a corrective inclusive level between the two activations rescues it.
#[test]
fn t2a_a_corrective_level_disarms_the_sweep() {
    let mut machine = TurnMachine::new();
    machine.register_live_task("sub", Some("AG"), true, TaskPlacement::default());
    machine.reconcile_background_tasks(&std::collections::HashSet::new());
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_result(&plain_result(), false);
    let live: std::collections::HashSet<String> = ["sub".to_string()].into();
    machine.reconcile_background_tasks(&live);
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert_eq!(machine.live_task_parent("sub"), Some("AG"));
}

/// T-12 / N-36 — a settled subagent registers again when resumed: the active
/// turn holds for it, it runs in the background, and a repeat changes nothing.
#[test]
fn t2a_resumed_subagent_holds_the_active_turn_again() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.register_live_task("t1", Some("AG"), true, background_placement());
    machine.on_task_ended("t1");
    assert_eq!(machine.live_task_parent("t1"), None);

    assert!(machine.resume_live_task("t1"));
    assert!(
        !machine.resume_live_task("t1"),
        "a repeated signal changes nothing"
    );
    assert!(machine.runs_in_background("AG"));
    machine.on_result(&plain_result(), false);
    assert!(machine.has_active(), "the resumed subagent holds p1");

    assert!(!machine.resume_live_task("never-seen"));
}

/// N-36 — `is_backgrounded` in a patch updates the record.
#[test]
fn t2a_backgrounded_patch_updates_the_record() {
    let mut machine = TurnMachine::new();
    machine.register_live_task("t1", Some("AG"), true, TaskPlacement::default());
    assert!(!machine.runs_in_background("AG"));
    machine.set_task_backgrounded("t1", true);
    assert!(machine.runs_in_background("AG"));
    machine.set_task_backgrounded("t1", false);
    assert!(!machine.runs_in_background("AG"));
}

/// A result naming only the original send (the interrupted cycle's) while the
/// steer's echo is still pending keeps the steer lane.
#[test]
fn t2a_result_naming_another_prompt_keeps_the_steer_lane() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(machine.mark_steered("s1"));
    let events = machine.on_result(&stamped_result("p1", &["p1"]), false);
    assert!(settled(&events).is_empty());
    assert!(machine.has_active());
}
