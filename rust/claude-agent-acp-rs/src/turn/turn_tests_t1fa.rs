//! Turn-machine tests added by the Turn 1 fix round (lane A, T1FA).

use super::*;

fn result_frame() -> Value {
    serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "stop_reason": "end_turn",
        "usage": { "output_tokens": 5, "input_tokens": 3 },
    })
}

fn autonomous(num_turns: Option<u64>) -> Value {
    let mut frame = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "stop_reason": "end_turn",
        "origin": { "kind": "task-notification" },
        "usage": { "output_tokens": 1 },
    });
    if let Some(n) = num_turns {
        frame["num_turns"] = serde_json::json!(n);
    }
    frame
}

fn settled_ids(events: &[TurnEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::Settled { prompt_uuid, .. } => Some(prompt_uuid.as_str()),
            _ => None,
        })
        .collect()
}

/// A held turn `p1` (one live subagent that has since ended) after its result.
fn held_machine() -> TurnMachine {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("t1", true);
    machine.on_result(&result_frame(), false);
    machine.on_task_ended("t1");
    assert!(machine.has_active(), "p1 held for its subagent");
    machine
}

// ---------------------------------------------------------------- finding 1

/// A turn failed for an idle without a result closes the delivery stretch
/// (`failActive`, `acp-agent.ts:5536-5541`).
#[test]
fn t1fa_idle_fail_closes_the_text_stretch() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(!machine.take_text_stretch_closed());
    let events = machine.on_idle();
    assert!(matches!(events[..], [TurnEvent::Failed { .. }]));
    assert!(machine.take_text_stretch_closed());
    assert!(!machine.take_text_stretch_closed(), "taking clears it");
}

/// A held turn draining at idle closes the stretch (`settleActive`, `wasHeld`).
#[test]
fn t1fa_held_drain_at_idle_closes_the_text_stretch() {
    let mut machine = held_machine();
    machine.take_text_stretch_closed();
    let events = machine.on_idle();
    assert_eq!(settled_ids(&events), vec!["p1"]);
    assert!(machine.take_text_stretch_closed());
}

/// A held turn handed off at the next prompt's echo closes the stretch; a
/// non-held previous turn does not (the echo can land mid-message,
/// `acp-agent.ts:7790-7796`).
#[test]
fn t1fa_echo_handoff_closes_the_stretch_only_for_a_held_turn() {
    let mut machine = held_machine();
    machine.take_text_stretch_closed();
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert!(machine.take_text_stretch_closed(), "held hand-off closes");

    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert!(
        !machine.take_text_stretch_closed(),
        "a non-held hand-off keeps the already-streamed text"
    );
}

/// An autonomous result with nothing in flight or queued closes the stretch;
/// with a queued turn it does not (`acp-agent.ts:7155-7164`).
#[test]
fn t1fa_autonomous_result_closes_the_stretch_only_when_idle() {
    let mut machine = TurnMachine::new();
    machine.on_result(&autonomous(Some(1)), true);
    assert!(machine.take_text_stretch_closed());

    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_result(&autonomous(Some(1)), true);
    assert!(
        !machine.take_text_stretch_closed(),
        "a queued turn's pre-echo deltas stay guarded"
    );
}

// ---------------------------------------------------------------- finding 3

/// A held turn settles at an autonomous follow-up result only when it says
/// `num_turns > 0`; a result without the field keeps holding
/// (`acp-agent.ts:7143-7146`).
#[test]
fn t1fa_held_turn_needs_num_turns_to_settle_at_a_followup() {
    let mut machine = held_machine();
    assert!(
        settled_ids(&machine.on_result(&autonomous(None), true)).is_empty(),
        "no num_turns: keeps holding"
    );
    assert!(machine.has_active());
    assert!(settled_ids(&machine.on_result(&autonomous(Some(0)), true)).is_empty());
    assert_eq!(
        settled_ids(&machine.on_result(&autonomous(Some(2)), true)),
        vec!["p1"]
    );
}

// ---------------------------------------------------------------- finding 4

/// Two cancels before the first interrupt receipt, with `init` latching the
/// lifecycle capability in between: each receipt reconciles the lane and turns
/// ITS cancel seeded (upstream's per-`cancel()` locals), so no stale count is
/// left to swallow an echo-less result such as `/compact`.
#[test]
fn t1fa_orphan_accounting_is_per_cancel() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.cancel();
    let first = machine.last_cancel_id();
    assert_eq!(
        machine.pending_orphan_results, 1,
        "p2 seeded the count lane"
    );

    machine.latch_capabilities(&["msg_lifecycle_v1"]);
    machine.enqueue(Turn::new("p3".into(), false));
    machine.cancel();
    let second = machine.last_cancel_id();
    assert_ne!(first, second);
    assert_eq!(machine.orphan_commands().len(), 1, "p3 seeded the map lane");

    // The interrupt dropped everything it listed as not still queued.
    machine.reconcile_cancel_receipt(first, Some(&[]));
    machine.reconcile_cancel_receipt(second, Some(&[]));
    assert_eq!(machine.pending_orphan_results, 0, "p2's count reconciled");
    assert!(
        machine.orphan_commands().is_empty(),
        "p3's entry reconciled"
    );

    machine.on_idle();
    machine.enqueue(Turn::new("compact".into(), true));
    let events = machine.on_result(&result_frame(), false);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Activated { prompt_uuid } if prompt_uuid == "compact")),
        "an echo-less result is not swallowed by a stale credit"
    );
}

// ---------------------------------------------------------------- finding 5

fn main_output() -> Value {
    serde_json::json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "event": { "type": "content_block_delta", "index": 0,
                   "delta": { "type": "text_delta", "text": "x" } },
    })
}

fn lifecycle_machine() -> TurnMachine {
    let mut machine = TurnMachine::new();
    machine.latch_capabilities(&["msg_lifecycle_v1"]);
    machine
}

/// A `started` frame for a queued prompt takes over a running autonomous cycle
/// (`reportInserted` -> `startForeground`), so after a cancel the stopped work's
/// output is dropped like any turn's (`acp-agent.ts:5846-5858`).
#[test]
fn t1fa_started_frame_takes_over_an_autonomous_cycle() {
    let mut machine = lifecycle_machine();
    machine.start_autonomous_cycle(&main_output());
    assert!(machine.runs_autonomous_cycle());
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_command_lifecycle("p1", "started");
    assert!(
        !machine.runs_autonomous_cycle(),
        "the prompt owns the foreground work"
    );

    // The cancel sweeps the queued prompt; the client is told the work ended.
    machine.cancel();
    assert!(machine.drops_stopped_work_output());
    assert!(machine.gate_stream_event(&main_output()));
}

/// While another turn is still active, insertion waits for the prompt's own
/// echo: its `started` frame leaves the foreground alone.
#[test]
fn t1fa_started_frame_waits_while_another_turn_is_active() {
    let mut machine = lifecycle_machine();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.autonomous_cycle = true;
    machine.on_command_lifecycle("p2", "started");
    assert!(machine.runs_autonomous_cycle(), "p1 is still active");
}

// ---------------------------------------------------------------- finding 6

/// Tool-result frames from the stopped work are dropped once the work end was
/// reported (`acp-agent.ts:7818-7831`); a running background subagent's go on.
#[test]
fn t1fa_stopped_work_tool_results_are_dropped_after_the_work_end() {
    let tool_result = serde_json::json!({
        "type": "user", "parent_tool_use_id": null,
        "message": { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }] },
    });
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(!machine.drops_stopped_work_user_frame(&tool_result));
    machine.cancel();
    assert!(
        !machine.drops_stopped_work_user_frame(&tool_result),
        "the turn is still active: the work end was not reported"
    );
    machine.on_idle();
    assert!(machine.drops_stopped_work_user_frame(&tool_result));

    machine.note_task_meta("t1", Some("toolu_agent"), true, true, None);
    machine.on_task_started("t1", true);
    let mut subagent = tool_result.clone();
    subagent["parent_tool_use_id"] = serde_json::json!("toolu_agent");
    assert!(
        !machine.drops_stopped_work_user_frame(&subagent),
        "a running background subagent is not the stopped work"
    );
}
