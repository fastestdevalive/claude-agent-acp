//! Turn-machine tests added by Turn 1 / lane A (F12, F13, F16a-c, F18, F19).
//! Kept apart from `turn_tests.rs`, whose `inv_16|23|29|30|31|steer_*` cases
//! are the hazard-lane regression net and stay untouched.

use super::*;

fn result_frame(subtype: &str, is_error: bool) -> Value {
    serde_json::json!({
        "type": "result",
        "subtype": subtype,
        "is_error": is_error,
        "stop_reason": "end_turn",
        "usage": { "output_tokens": 5, "input_tokens": 3 },
    })
}

fn autonomous_result(subtype: &str) -> Value {
    serde_json::json!({
        "type": "result",
        "subtype": subtype,
        "is_error": false,
        "stop_reason": "end_turn",
        "origin": { "kind": "task-notification" },
        "usage": { "output_tokens": 1 },
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

// ---------------------------------------------------------------- F12 (N-11, N-12)

/// N-11 — a transition INTO `running` sweeps the unpayable trailing-idle debt
/// (`acp-agent.ts:6088-6110`); a repeated `running` is not a transition.
#[test]
fn f12_running_transition_sweeps_owed_debt() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_session_state("running");
    machine.on_result(&result_frame("success", false), false);
    assert_eq!(machine.owed_trailing_idles, 1, "p1's result owes an idle");

    // The CLI withheld the idle (2.1.270+ while background agents run), then
    // started a new cycle: the debt can never be paid.
    machine.on_session_state("idle_pending");
    machine.on_session_state("running");
    assert_eq!(machine.owed_trailing_idles, 0, "running transition sweeps");

    // A repeated `running` leaves fresh debt alone.
    machine.on_result(&autonomous_result("success"), true);
    assert_eq!(machine.owed_trailing_idles, 1);
    machine.on_session_state("running");
    assert_eq!(
        machine.owed_trailing_idles, 1,
        "running -> running is not a transition"
    );
}

/// N-11 — the swept debt no longer masks an issue-#825 detection: the idle that
/// follows a fresh turn with no result fails it instead of being absorbed.
#[test]
fn f12_swept_debt_does_not_mask_825() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_result(&result_frame("success", false), false);
    assert_eq!(machine.owed_trailing_idles, 1);

    machine.on_session_state("running");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    let events = machine.on_idle();
    assert_eq!(
        failed(&events),
        vec![("p2", FailureKind::NoResult)],
        "with the debt swept, p2's un-owed idle is the #825 signature"
    );
}

/// N-12 — a steered turn whose steered work has produced its result settles at
/// the next idle EVEN with owed debt outstanding (the ready lane runs ahead of
/// debt absorption); the debt is left standing.
#[test]
fn f12_steered_ready_settles_ahead_of_owed_debt() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(machine.mark_steered("s1"));
    machine.on_result(&result_frame("success", false), false);
    machine.on_echo("s1");
    machine.on_result(&result_frame("success", false), false);
    machine.owed_trailing_idles = 1;

    let events = machine.on_idle();
    assert_eq!(settled(&events), vec![("p1", StopReason::EndTurn)]);
    assert_eq!(
        machine.owed_trailing_idles, 1,
        "the ready lane leaves the debt standing"
    );
}

/// N-12 — an idle after the steered echo drained but BEFORE a result since it
/// must not settle on the interrupted cycle's stale outcome
/// (`steeredAwaitingResult`).
#[test]
fn f12_steered_awaiting_result_blocks_idle_settle() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(machine.mark_steered("s1"));
    machine.on_result(&result_frame("success", false), false);
    machine.on_echo("s1");

    let events = machine.on_idle();
    assert!(
        settled(&events).is_empty() && failed(&events).is_empty(),
        "the steered cycle's own result is still ahead"
    );
    assert!(machine.has_active());

    machine.on_result(&result_frame("success", false), false);
    let events = machine.on_idle();
    assert_eq!(settled(&events), vec![("p1", StopReason::EndTurn)]);
}

/// N-12 — a cancelled held turn absorbs one outstanding trailer debt at its idle
/// and settles with the usage its own result recorded (#844).
#[test]
fn f12_cancelled_held_idle_absorbs_debt_and_keeps_usage() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("t1", true);
    machine.on_result(&result_frame("success", false), false);
    assert_eq!(
        machine.owed_trailing_idles, 1,
        "held result owes its trailer"
    );
    machine.accumulated_usage.output_tokens += 100;
    machine.cancelled = true;

    let events = machine.on_idle();
    match &events[..] {
        [TurnEvent::Settled {
            stop_reason, usage, ..
        }] => {
            assert_eq!(*stop_reason, StopReason::Cancelled);
            assert_eq!(usage.output_tokens, 5, "the held result's recorded usage");
        }
        other => panic!("expected one cancelled settle, got {other:?}"),
    }
    assert_eq!(machine.owed_trailing_idles, 0, "held cancel absorbs a debt");
}

// ------------------------------------------------ F13 (N-01, N-02, B-06, B-32)

fn error_result() -> Value {
    serde_json::json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "stop_reason": null,
        "errors": ["aborted"],
        "usage": { "output_tokens": 9, "input_tokens": 999 },
    })
}

fn stamped(mut result: Value, uuid: &str) -> Value {
    result["user_message_uuid"] = serde_json::json!(uuid);
    result
}

/// A lifecycle-capable machine with `p1` active and `queued` prompts waiting.
fn lifecycle_machine(queued: &[&str]) -> TurnMachine {
    let mut machine = TurnMachine::new();
    machine.latch_capabilities(&["interrupt_receipt_v1", "msg_lifecycle_v1"]);
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    for uuid in queued {
        machine.enqueue(Turn::new((*uuid).into(), false));
    }
    machine
}

/// Cancel, queue the echo-less `/compact` (`pc`), settle p1 at its idle, and
/// return the machine ready for the orphan frames.
fn cancel_then_compact(machine: &mut TurnMachine) {
    let events = machine.cancel();
    assert!(
        settled(&events)
            .iter()
            .all(|(_, reason)| *reason == StopReason::Cancelled),
        "cancel sweeps queued turns cancelled"
    );
    machine.enqueue(Turn::new("pc".into(), true));
    let events = machine.on_idle();
    assert_eq!(settled(&events), vec![("p1", StopReason::Cancelled)]);
}

/// The echo-less `/compact` result settles `pc` with its OWN result.
fn assert_compact_settles(machine: &mut TurnMachine) {
    let events = machine.on_result(&result_frame("success", false), false);
    assert_eq!(
        settled(&events),
        vec![("pc", StopReason::EndTurn)],
        "the compact result must settle pc, not be swallowed by a stale orphan"
    );
    assert!(machine.orphan_commands().is_empty());
}

/// N-02 — `init` latches the capabilities and never unlatches.
#[test]
fn f13_init_latches_capabilities_and_never_unlatches() {
    let mut machine = TurnMachine::new();
    assert!(!machine.msg_lifecycle_v1 && !machine.request_marker_lists_v1);
    machine.latch_capabilities(&["interrupt_receipt_v1"]);
    assert!(!machine.msg_lifecycle_v1 && !machine.request_marker_lists_v1);
    machine.latch_capabilities(&["msg_lifecycle_v1", "request_marker_lists_v1"]);
    assert!(machine.msg_lifecycle_v1 && machine.request_marker_lists_v1);
    machine.latch_capabilities(&[]);
    assert!(machine.msg_lifecycle_v1 && machine.request_marker_lists_v1);
}

/// Upstream "drains BOTH coalesced orphans on their one shared result".
#[test]
fn f13_coalesced_orphans_drain_on_one_shared_result() {
    let mut machine = lifecycle_machine(&["p2", "p3"]);
    machine.cancel();
    machine.enqueue(Turn::new("pc".into(), true));
    assert_eq!(machine.orphan_commands().len(), 2);
    assert_eq!(machine.pending_orphan_results, 0, "count lane untouched");
    machine.on_idle();
    machine.on_command_lifecycle("p2", "started");
    machine.on_command_lifecycle("p3", "started");
    let events = machine.on_result(&error_result(), false);
    assert!(
        settled(&events).is_empty(),
        "the shared orphan result is skipped"
    );
    assert!(machine.orphan_commands().is_empty(), "drains BOTH entries");
    machine.on_command_lifecycle("p2", "completed");
    machine.on_command_lifecycle("p3", "completed");
    assert_compact_settles(&mut machine);
}

/// Upstream "forgets an orphan whose `cancelled` frame precedes any `started`".
#[test]
fn f13_cancelled_before_started_forgets_the_orphan() {
    let mut machine = lifecycle_machine(&["p2"]);
    cancel_then_compact(&mut machine);
    assert_eq!(machine.orphan_commands().len(), 1);
    machine.on_command_lifecycle("p2", "cancelled");
    assert!(machine.orphan_commands().is_empty());
    assert_compact_settles(&mut machine);
}

/// Upstream "skips a zombie orphan's late result exactly once".
#[test]
fn f13_zombie_orphan_result_skipped_exactly_once() {
    let mut machine = lifecycle_machine(&["p2"]);
    cancel_then_compact(&mut machine);
    machine.on_command_lifecycle("p2", "started");
    machine.on_command_lifecycle("p2", "cancelled");
    assert_eq!(
        machine.orphan_commands(),
        &[("p2".to_string(), OrphanState::Zombie)]
    );
    let events = machine.on_result(&error_result(), false);
    assert!(
        settled(&events).is_empty(),
        "the zombie's late result is skipped"
    );
    assert!(
        machine.orphan_commands().is_empty(),
        "and consumes the zombie"
    );
    assert_compact_settles(&mut machine);
}

/// Upstream "forgets a pending orphan the interrupt receipt reports dropped
/// (lifecycle lane)".
#[test]
fn f13_receipt_forgets_dropped_pending_orphan_on_lifecycle_lane() {
    let mut machine = lifecycle_machine(&["p2"]);
    cancel_then_compact(&mut machine);
    assert_eq!(machine.orphan_commands().len(), 1);
    machine.reconcile_orphan_receipt(Some(&[]));
    assert!(machine.orphan_commands().is_empty());
    assert_eq!(machine.pending_orphan_results, 0, "count lane untouched");
    assert_compact_settles(&mut machine);
}

/// The receipt reconciles the lane `cancel()` SEEDED, even when `init` latches
/// the capability while the interrupt is in flight.
#[test]
fn f13_receipt_acts_on_the_lane_cancel_seeded() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.cancel(); // count lane: init not seen yet
    assert_eq!(machine.pending_orphan_results, 1);
    machine.latch_capabilities(&["msg_lifecycle_v1"]);
    machine.reconcile_orphan_receipt(Some(&[]));
    assert_eq!(
        machine.pending_orphan_results, 0,
        "the count lane seeded the orphan, so the count lane uncounts it"
    );
}

/// Upstream "uses a result's user_message_uuid to consume exactly the orphan it
/// names".
#[test]
fn f13_stamped_result_consumes_exactly_the_named_orphan() {
    let mut machine = lifecycle_machine(&["p2", "p3"]);
    cancel_then_compact(&mut machine);
    assert_eq!(machine.orphan_commands().len(), 2);
    let events = machine.on_result(&stamped(error_result(), "p3"), false);
    assert!(settled(&events).is_empty());
    assert_eq!(
        machine.orphan_commands(),
        &[("p2".to_string(), OrphanState::Pending)],
        "p3 consumed by name; p2 left"
    );
    // /compact's live result is stamped with its own send: the leftover pending
    // entry must not trigger the dup-over-loss one-skip.
    let events = machine.on_result(&stamped(result_frame("success", false), "pc"), false);
    assert_eq!(settled(&events), vec![("pc", StopReason::EndTurn)]);
    assert!(
        machine.orphan_commands().is_empty(),
        "p2's leftover is swept by pc's activation"
    );
}

/// Upstream "deletes (not zombifies) an orphan whose `cancelled` frame arrives
/// AFTER its error result".
#[test]
fn f13_cancelled_frame_after_error_result_does_not_zombify() {
    let mut machine = lifecycle_machine(&["p2"]);
    cancel_then_compact(&mut machine);
    machine.on_command_lifecycle("p2", "started");
    machine.on_result(&error_result(), false);
    machine.on_command_lifecycle("p2", "cancelled");
    assert!(machine.orphan_commands().is_empty(), "no phantom zombie");
    assert_compact_settles(&mut machine);
}

/// Upstream "drains ALL coalesced orphans of an aborted turn on its single error
/// result".
#[test]
fn f13_aborted_coalesced_orphans_drain_on_single_error_result() {
    let mut machine = lifecycle_machine(&["p2", "p3"]);
    cancel_then_compact(&mut machine);
    machine.on_command_lifecycle("p2", "started");
    machine.on_command_lifecycle("p3", "started");
    machine.on_result(&error_result(), false);
    machine.on_command_lifecycle("p2", "cancelled");
    machine.on_command_lifecycle("p3", "cancelled");
    assert!(machine.orphan_commands().is_empty());
    assert_compact_settles(&mut machine);
}

/// Upstream "does not seed an orphan for a queued command whose terminal frame
/// already passed".
#[test]
fn f13_no_orphan_for_command_whose_terminal_frame_passed() {
    let mut machine = lifecycle_machine(&["p2"]);
    machine.on_command_lifecycle("p2", "started");
    machine.on_command_lifecycle("p2", "completed");
    machine.cancel();
    assert!(machine.orphan_commands().is_empty());
    assert_eq!(machine.pending_orphan_results, 0);
}

/// Upstream "stops blocking on a started orphan whose terminal frame never
/// arrives".
#[test]
fn f13_started_orphan_drained_by_its_result_without_terminal_frame() {
    let mut machine = lifecycle_machine(&["p2"]);
    cancel_then_compact(&mut machine);
    machine.on_command_lifecycle("p2", "started");
    machine.on_result(&error_result(), false);
    assert!(machine.orphan_commands().is_empty());
    assert_compact_settles(&mut machine);
}

/// Upstream "consumes an orphan's result that arrives while the turn queue is
/// empty".
#[test]
fn f13_orphan_result_with_empty_queue_is_consumed() {
    let mut machine = lifecycle_machine(&["p2"]);
    machine.cancel();
    machine.on_idle();
    machine.on_command_lifecycle("p2", "started");
    let events = machine.on_result(&error_result(), false);
    assert!(settled(&events).is_empty());
    machine.on_command_lifecycle("p2", "cancelled");
    assert!(machine.orphan_commands().is_empty());
    machine.enqueue(Turn::new("pc".into(), true));
    assert_compact_settles(&mut machine);
}

/// Upstream "does not zombify a command folded into the active turn whose
/// shared result was the active turn's".
#[test]
fn f13_active_turns_result_covers_started_entries() {
    let mut machine = lifecycle_machine(&["p2"]);
    machine.on_command_lifecycle("p2", "started");
    machine.cancel();
    assert_eq!(
        machine.orphan_commands(),
        &[("p2".to_string(), OrphanState::Started)]
    );
    machine.enqueue(Turn::new("pc".into(), true));
    // The fold's shared result arrives while p1 is still active (cancelled).
    let events = machine.on_result(&result_frame("success", false), false);
    assert_eq!(
        settled(&events),
        vec![("p1", StopReason::Cancelled)],
        "p1 ends at the shared result, carrying its usage"
    );
    assert!(machine.orphan_commands().is_empty(), "covers p2's entry");
    machine.on_idle();
    machine.on_command_lifecycle("p2", "cancelled");
    assert!(machine.orphan_commands().is_empty(), "no phantom");
    assert_compact_settles(&mut machine);
}

/// Upstream "does not seed a zombie for a fold-aborted command whose shared
/// result passed BEFORE the cancel".
#[test]
fn f13_no_zombie_when_shared_result_passed_before_cancel() {
    let mut machine = lifecycle_machine(&["p2"]);
    machine.on_command_lifecycle("p2", "started");
    machine.on_result(&error_result(), false); // p1 fails; p2's result has passed
    machine.on_idle();
    machine.on_command_lifecycle("p2", "cancelled");
    machine.cancel();
    assert!(machine.orphan_commands().is_empty());
    machine.enqueue(Turn::new("pc".into(), true));
    assert_compact_settles(&mut machine);
}

/// Upstream "does not seed an orphan on force-cancel when the wedged turn's
/// result and terminal frame already passed" (B-04).
#[test]
fn f13_force_cancel_seeds_by_fate() {
    // Result and terminal frame already passed: nothing to track.
    let mut machine = lifecycle_machine(&[]);
    machine.on_command_lifecycle("p1", "started");
    machine.cancel();
    machine.on_result(&error_result(), false);
    machine.on_command_lifecycle("p1", "cancelled");
    machine.force_cancel();
    assert!(machine.orphan_commands().is_empty());

    // Wedged turn that was dispatched, nothing spent yet: tracked "started".
    let mut machine = lifecycle_machine(&[]);
    machine.cancel();
    machine.force_cancel();
    assert_eq!(
        machine.orphan_commands(),
        &[("p1".to_string(), OrphanState::Started)]
    );

    // Count lane (no capability): the plain +1 credit is kept.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.force_cancel();
    assert_eq!(machine.pending_orphan_results, 1);
}

/// `started` after the terminal frame is flagged for logging; unknown states are
/// flagged, `queued` and `refused` are not (upstream "ignores command_lifecycle
/// frames without logging" / "treats ... 'refused' as terminal").
#[test]
fn f13_lifecycle_diagnostics() {
    let mut machine = lifecycle_machine(&["p2"]);
    assert_eq!(machine.on_command_lifecycle("p2", "queued"), None);
    assert_eq!(machine.on_command_lifecycle("p2", "refused"), None);
    assert_eq!(
        machine.on_command_lifecycle("p2", "bogus"),
        Some(LifecycleDiagnostic::UnknownState("bogus".into()))
    );
    let mut machine = lifecycle_machine(&["p2"]);
    machine.cancel();
    machine.on_command_lifecycle("p2", "started");
    machine.on_command_lifecycle("p2", "cancelled");
    assert_eq!(
        machine.on_command_lifecycle("p2", "started"),
        Some(LifecycleDiagnostic::StartedAfterTerminal)
    );
}

/// B-06 — a result that names a prompt a cancel stopped is that prompt's even
/// when the orphan accounting lost track of it (upstream "does not let the next
/// prompt take the result of a prompt the cancel abandoned").
#[test]
fn f13_result_naming_cancelled_prompt_does_not_settle_next_prompt() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    // The queued p2 is swept and settles cancelled at the cancel.
    let cancel_events = machine.cancel();
    assert_eq!(
        settled(&cancel_events),
        vec![("p2", StopReason::Cancelled)],
        "the queued prompt is swept cancelled"
    );
    // The receipt left p2 out: the count credit is uncounted.
    machine.reconcile_orphan_receipt(Some(&[]));
    // The cancelled p1 settles cancelled at the idle.
    let idle_events = machine.on_idle();
    assert_eq!(
        settled(&idle_events),
        vec![("p1", StopReason::Cancelled)],
        "the active prompt ends cancelled"
    );
    machine.enqueue(Turn::new("p3".into(), true));
    // p2's late stamped result carries its own (large) usage: it is not p3's.
    let mut late = stamped(result_frame("success", false), "p2");
    late["usage"] = serde_json::json!({"input_tokens": 999, "output_tokens": 0});
    let events = machine.on_result(&late, false);
    assert!(
        settled(&events).is_empty(),
        "p2's stamped result is not p3's"
    );
    // p3 settles end_turn with its own usage, not the abandoned 999.
    let mut third = stamped(result_frame("success", false), "p3");
    third["usage"] = serde_json::json!({"input_tokens": 0, "output_tokens": 0});
    let events = machine.on_result(&third, false);
    assert_eq!(settled(&events), vec![("p3", StopReason::EndTurn)]);
    let usage = events.iter().find_map(|e| match e {
        TurnEvent::Settled { usage, .. } => Some(*usage),
        _ => None,
    });
    assert_eq!(
        usage.map(|u| u.input_tokens),
        Some(0),
        "the abandoned prompt's usage is not attributed to the next turn"
    );
}

/// R72-2 — the empty-interruption `[ede_diagnostic]` result of a cancelled
/// cycle consumes exactly the token it names and never settles the replacement.
#[test]
fn f13_empty_interruption_diagnostic_is_swallowed() {
    let diagnostic = |uuid: Option<&str>| {
        let mut v = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],
            "usage": { "output_tokens": 0, "input_tokens": 0 },
        });
        if let Some(uuid) = uuid {
            v["user_message_uuid"] = serde_json::json!(uuid);
        }
        v
    };
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.cancel();
    assert_eq!(machine.empty_interruption_commands, vec!["p2".to_string()]);
    machine.enqueue(Turn::new("p3".into(), false));
    machine.on_echo("p3");
    machine.pending_orphan_results = 0;
    let events = machine.on_result(&diagnostic(Some("p2")), false);
    assert!(
        events.is_empty() && machine.has_active(),
        "the diagnostic must not fail the replacement p3"
    );
    assert!(machine.empty_interruption_commands.is_empty());

    // A diagnostic-shaped result naming a send we did NOT cancel is a real
    // failure and clears stale tokens.
    machine.empty_interruption_commands.push("px".into());
    let events = machine.on_result(&diagnostic(Some("pz")), false);
    assert_eq!(failed(&events), vec![("p3", FailureKind::ProviderError)]);
    assert!(machine.empty_interruption_commands.is_empty());
}

/// T2GA-2 — `result_is_swallowed_diagnostic` names exactly the results the
/// swallow in `on_result_finish` consumes (read-only, so the session can skip
/// the failure clear for them).
#[test]
fn t2ga_swallowed_diagnostic_predicate_matches_the_swallow() {
    let diagnostic = |uuid: Option<&str>| {
        let mut v = serde_json::json!({
            "type": "result", "subtype": "error_during_execution", "is_error": true,
            "errors": ["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],
            "usage": { "output_tokens": 0, "input_tokens": 0 },
        });
        if let Some(uuid) = uuid {
            v["user_message_uuid"] = serde_json::json!(uuid);
        }
        v
    };
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.cancel();
    machine.enqueue(Turn::new("p3".into(), false));
    machine.on_echo("p3");
    machine.pending_orphan_results = 0;
    assert!(machine.result_is_swallowed_diagnostic(&diagnostic(Some("p2"))));
    assert!(machine.result_is_swallowed_diagnostic(&diagnostic(None)));
    assert!(!machine.result_is_swallowed_diagnostic(&diagnostic(Some("pz"))));
    let ordinary = serde_json::json!({"type": "result", "subtype": "success",
        "is_error": false, "result": "ok", "user_message_uuid": "p2"});
    assert!(!machine.result_is_swallowed_diagnostic(&ordinary));
    machine.on_result(&diagnostic(Some("p2")), false);
    assert!(
        !machine.result_is_swallowed_diagnostic(&diagnostic(Some("p2"))),
        "the token is spent"
    );
}

/// A receipt that drops the swept turn also drops its diagnostic token.
#[test]
fn f13_receipt_drop_forgets_diagnostic_token() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.cancel();
    assert_eq!(machine.empty_interruption_commands.len(), 1);
    machine.reconcile_orphan_receipt(Some(&[]));
    assert!(machine.empty_interruption_commands.is_empty());
}

// ------------------------------------------------ F16a (N-06, N-29, B-04, B-13, B-35, B-36)

fn aborted_autonomous_result() -> Value {
    serde_json::json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "terminal_reason": "aborted_streaming",
        "origin": { "kind": "task-notification" },
        "errors": ["aborted"],
        "usage": { "output_tokens": 40, "input_tokens": 0 },
    })
}

/// A held turn `p1` (one live subagent) after its own result: usage 3 in / 5 out.
fn held_machine() -> TurnMachine {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("t1", true);
    machine.on_result(&result_frame("success", false), false);
    assert!(machine.has_active(), "p1 held for its subagent");
    machine
}

/// Upstream "answers a cancelled prompt at its interrupted result": a cancelled
/// turn ends at the result of the work the interrupt stopped, not at the idle;
/// the trailing idle is then owed (absorbed), and a lagging idle does not touch
/// the next prompt.
#[test]
fn f16a_cancelled_turn_settles_at_its_interrupted_result() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.cancel();

    let events = machine.on_result(&error_result(), false);
    match &events[..] {
        [TurnEvent::Settled {
            prompt_uuid,
            stop_reason,
            usage,
            had_usage,
            ..
        }] => {
            assert_eq!(prompt_uuid, "p1");
            assert_eq!(*stop_reason, StopReason::Cancelled);
            assert_eq!(usage.output_tokens, 9, "the interrupted result's usage");
            assert!(*had_usage);
        }
        other => panic!("expected the cancelled settle, got {other:?}"),
    }
    assert_eq!(
        machine.owed_trailing_idles, 1,
        "the result owes its trailer"
    );

    // The next prompt is not disturbed by the lagging idle.
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert!(machine.on_idle().is_empty(), "p1's trailer is absorbed");
    assert!(machine.has_active(), "p2 keeps running");
}

/// Held turn + live follow-up (request markers: a model request since the
/// result): cancel does NOT settle inline; the follow-up's ABORTED autonomous
/// result settles it `cancelled` with the usage its own result recorded.
#[test]
fn f16a_held_cancel_with_live_followup_waits_for_aborted_result() {
    let mut machine = held_machine();
    machine.latch_capabilities(&["request_marker_lists_v1"]);
    machine.on_status(&serde_json::json!({"status": "requesting"}));
    assert!(machine.model_request_since_result);
    machine.accumulated_usage.output_tokens += 100;

    let events = machine.cancel();
    assert!(
        settled(&events).is_empty(),
        "a live follow-up is left to its interrupted result"
    );
    assert!(machine.has_active());

    // A placeholder / non-aborted follow-up result belongs to other work.
    let events = machine.on_result(&autonomous_result("success"), true);
    assert!(settled(&events).is_empty(), "not the stopped work's result");
    assert!(machine.has_active());

    let events = machine.on_result(&aborted_autonomous_result(), true);
    match &events[..] {
        [TurnEvent::Settled {
            stop_reason, usage, ..
        }] => {
            assert_eq!(*stop_reason, StopReason::Cancelled);
            assert_eq!(usage.output_tokens, 5, "the held result's recorded usage");
        }
        other => panic!("expected the cancelled settle, got {other:?}"),
    }
}

/// Without request markers the follow-up is "live" while the session is
/// `running`/`requires_action`; otherwise the hold is settled inline and the
/// trailer pre-counted unless the session sits idle.
#[test]
fn f16a_held_cancel_followup_live_gate_follows_session_state() {
    // running -> live -> not settled inline.
    let mut machine = held_machine();
    machine.on_session_state("running");
    assert!(settled(&machine.cancel()).is_empty());

    let mut machine = held_machine();
    machine.on_session_state("requires_action");
    assert!(settled(&machine.cancel()).is_empty());

    // idle -> a plain hold -> settled inline, no pre-counted trailer.
    let mut machine = held_machine();
    machine.on_session_state("idle");
    machine.owed_trailing_idles = 0;
    assert_eq!(
        settled(&machine.cancel()),
        vec![("p1", StopReason::Cancelled)]
    );
    assert_eq!(machine.owed_trailing_idles, 0, "idle: nothing to pre-count");

    // request markers: no model request since the result -> a hold, inline.
    let mut machine = held_machine();
    machine.latch_capabilities(&["request_marker_lists_v1"]);
    machine.on_session_state("running"); // stale `running` must not matter
    assert_eq!(
        settled(&machine.cancel()),
        vec![("p1", StopReason::Cancelled)]
    );
    // The `running` transition swept the held result's unpaid trailer (F12), so
    // the cancel's pre-count is the only debt.
    assert_eq!(
        machine.owed_trailing_idles, 1,
        "non-idle: pre-counts a trailer"
    );
}

/// The result clears `model_request_since_result` (a hold again).
#[test]
fn f16a_result_clears_model_request_marker() {
    let mut machine = held_machine();
    machine.on_status(&serde_json::json!({"status": "requesting"}));
    assert!(machine.model_request_since_result);
    machine.on_result(&autonomous_result("success"), true);
    assert!(!machine.model_request_since_result);
}

/// N-06 — a `status: requesting` that serves none of the stopped prompts reopens
/// the window; one that names a cancelled prompt, or arrives while an orphan is
/// outstanding, does not. Needs `request_marker_lists_v1`.
#[test]
fn f16a_status_requesting_reopens_cancelled_window() {
    let requesting =
        |uuids: &[&str]| serde_json::json!({"status": "requesting", "user_message_uuids": uuids});
    let build = || {
        let mut machine = TurnMachine::new();
        machine.latch_capabilities(&["request_marker_lists_v1"]);
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        machine.cancel();
        // p1 ended at its interrupted result (settled); work is over.
        machine.on_result(&error_result(), false);
        machine
    };

    let mut machine = build();
    assert!(machine.cancelled);
    assert!(
        machine.on_status(&requesting(&[])),
        "unrelated follow-up reopens"
    );
    assert!(!machine.cancelled);
    assert!(machine.cancelled_prompts.is_empty());

    let mut machine = build();
    assert!(
        !machine.on_status(&requesting(&["p1"])),
        "a request naming a cancelled prompt stays inside the stopped work"
    );
    assert!(machine.cancelled);

    // Without the capability the window never reopens this way.
    let mut machine = build();
    machine.request_marker_lists_v1 = false;
    assert!(!machine.on_status(&requesting(&[])));
    assert!(machine.cancelled);

    // An outstanding orphan keeps the window open.
    let mut machine = build();
    machine.pending_orphan_results = 1;
    assert!(!machine.on_status(&requesting(&[])));

    // A queued (not yet active) prompt named by the request also serves the cancel.
    let mut machine = build();
    machine.enqueue(Turn::new("p9".into(), false));
    assert!(!machine.on_status(&requesting(&["p9"])));
}

// ------------------------------------------------ F16b (B-18, B-38, B-62)

fn text_delta_event(parent: serde_json::Value) -> Value {
    serde_json::json!({
        "type": "stream_event",
        "parent_tool_use_id": parent,
        "event": {"type": "content_block_delta", "index": 0,
                  "delta": {"type": "text_delta", "text": "late"}},
    })
}

fn tool_use_start_event(id: &str, parent: serde_json::Value) -> Value {
    serde_json::json!({
        "type": "stream_event",
        "parent_tool_use_id": parent,
        "event": {"type": "content_block_start", "index": 1,
                  "content_block": {"type": "tool_use", "id": id, "name": "Bash", "input": {}}},
    })
}

/// A cancelled session with no turn left: the client was told the work ended.
fn stopped_machine() -> TurnMachine {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.cancel();
    machine.on_result(&error_result(), false); // p1 ends at its interrupted result
    assert!(machine.is_cancelled() && !machine.has_active());
    machine
}

/// B-18 — streamed events of the stopped work stream through until the client was
/// told the work ended (the cancelled turn is still active), and are dropped after.
#[test]
fn f16b_stream_events_drop_only_after_work_end_is_reported() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(
        !machine.gate_stream_event(&text_delta_event(Value::Null)),
        "no cancel"
    );
    machine.cancel();
    assert!(
        !machine.gate_stream_event(&text_delta_event(Value::Null)),
        "the cancelled turn is still active: what the interrupt flushes goes out"
    );
    machine.on_result(&error_result(), false);
    assert!(
        machine.gate_stream_event(&text_delta_event(Value::Null)),
        "after the work-end report the stopped work is dropped"
    );
}

/// B-18 — a stream that lost a delta keeps dropping until the next message starts
/// or a consolidated message consumes the record, even if the window ends first.
#[test]
fn f16b_dropped_stream_latches_until_message_start() {
    let mut machine = stopped_machine();
    assert!(machine.gate_stream_event(&text_delta_event(Value::Null)));
    // The next prompt's echo ends the window mid-block...
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert!(!machine.is_cancelled());
    // ...but the stream that lost a delta still drops until its record resets.
    assert!(machine.gate_stream_event(&text_delta_event(Value::Null)));
    machine.note_stream_message_start(None);
    assert!(!machine.gate_stream_event(&text_delta_event(Value::Null)));

    // A consolidated message resets the latch too.
    let mut machine = stopped_machine();
    machine.gate_stream_event(&text_delta_event(Value::Null));
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    machine.note_consolidated_consumed(&serde_json::json!({"parent_tool_use_id": null}));
    assert!(!machine.gate_stream_event(&text_delta_event(Value::Null)));
}

/// B-18 — a subagent's stream only latches when its text is forwarded
/// (`forwardsSubagentText`), and a running BACKGROUND subagent's output is not
/// the stopped work at all.
#[test]
fn f16b_subagent_streams_and_background_exception() {
    let sub = serde_json::json!("toolu_agent");
    let mut machine = stopped_machine();
    assert!(
        machine.gate_stream_event(&text_delta_event(sub.clone())),
        "dropped..."
    );
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert!(
        !machine.gate_stream_event(&text_delta_event(sub.clone())),
        "...but without forwardsSubagentText it does not latch"
    );

    let mut machine = stopped_machine();
    machine.set_forwards_subagent_text(true);
    machine.gate_stream_event(&text_delta_event(sub.clone()));
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    assert!(
        machine.gate_stream_event(&text_delta_event(sub.clone())),
        "latched"
    );

    // A running background subagent keeps streaming through a cancel.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("agent-1", true);
    machine.note_task_meta("agent-1", Some("toolu_agent"), true, true, None);
    machine.cancel();
    machine.on_result(&error_result(), false);
    assert!(machine.drops_stopped_work_output());
    assert!(!machine.gate_stream_event(&text_delta_event(sub.clone())));
    assert!(!machine.gate_assistant(&serde_json::json!({"parent_tool_use_id": "toolu_agent"})));
    assert!(
        machine.gate_assistant(&serde_json::json!({"parent_tool_use_id": "toolu_other"})),
        "another conversation's output is still dropped"
    );
    machine.on_task_ended("agent-1");
    assert!(
        machine.gate_stream_event(&text_delta_event(sub)),
        "ended: dropped again"
    );
}

/// B-18 — a consolidated message of a cancelled session is dropped whole, even
/// before the client was told the work ended; its tool uses are recorded as
/// reached with whether the window dropped them.
#[test]
fn f16b_assistant_dropped_whole_and_tool_uses_recorded() {
    let assistant = |id: &str| {
        serde_json::json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {"content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {}}]},
        })
    };
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(!machine.gate_assistant(&assistant("toolu_live")));
    assert_eq!(machine.reached_tool_use("toolu_live"), Some(false));

    machine.cancel();
    assert!(
        machine.gate_assistant(&assistant("toolu_early")),
        "cancelled: dropped whole"
    );
    assert_eq!(
        machine.reached_tool_use("toolu_early"),
        Some(false),
        "the work end is not reported yet, so the window did not drop its tool use"
    );
    machine.on_result(&error_result(), false);
    assert!(machine.gate_assistant(&assistant("toolu_late")));
    assert_eq!(machine.reached_tool_use("toolu_late"), Some(true));
    // The first record of a tool use stays.
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    machine.gate_assistant(&assistant("toolu_late"));
    assert_eq!(machine.reached_tool_use("toolu_late"), Some(true));
}

/// B-18 — a streamed tool_use start is recorded too.
#[test]
fn f16b_streamed_tool_use_start_is_recorded() {
    let mut machine = stopped_machine();
    assert!(machine.gate_stream_event(&tool_use_start_event("toolu_s", Value::Null)));
    assert_eq!(machine.reached_tool_use("toolu_s"), Some(true));
}

/// B-62 — the verdict for a permission request: decided at once for a tool use
/// the consumer reached; awaited while a cancel's window is open and the tool use
/// is not reached yet; otherwise it goes on.
#[test]
fn f16b_stopped_work_verdict() {
    use StoppedWorkVerdict::*;
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert_eq!(
        machine.stopped_work_verdict("toolu_x", false),
        Proceed,
        "no cancel"
    );
    machine.cancel();
    assert_eq!(machine.stopped_work_verdict("toolu_x", false), Wait);
    assert_eq!(
        machine.stopped_work_verdict("toolu_x", true),
        Proceed,
        "wait is over"
    );
    machine.on_result(&error_result(), false);
    machine.gate_stream_event(&tool_use_start_event("toolu_x", Value::Null));
    assert_eq!(machine.stopped_work_verdict("toolu_x", false), Stopped);
    machine.record_reached_tool_use("toolu_ok", false);
    assert_eq!(machine.stopped_work_verdict("toolu_ok", false), Proceed);
}

/// B-38 — the prompts and steers a cancel stopped are judged against late output:
/// a cancel records every unsettled turn and its steers; an activation clears them.
#[test]
fn f16b_cancelled_prompts_recorded_and_cleared_at_activation() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.mark_steered("s1");
    machine.enqueue(Turn::new("p2".into(), false));
    machine.cancel();
    for uuid in ["p1", "s1", "p2"] {
        assert!(machine.cancelled_prompts.contains(uuid), "{uuid}");
    }
    machine.on_result(&error_result(), false);
    machine.enqueue(Turn::new("p3".into(), false));
    machine.on_echo("p3");
    assert!(machine.cancelled_prompts.is_empty());
}

// ------------------------------------------------ F19 (B-14)

/// B-14 — a turn carried into a fresh machine by a session restart keeps the usage
/// (and per-model rows) it earned before: the next activation's scratch starts
/// from it, then forgets it (`carriedUsage`, `acp-agent.ts:4781-4791`).
#[test]
fn f19_restarted_turn_carries_its_usage_into_the_next_scratch() {
    let mut old = TurnMachine::new();
    old.enqueue(Turn::new("p1".into(), false));
    old.on_echo("p1");
    old.accumulated_usage = Usage {
        input_tokens: 11,
        output_tokens: 22,
        cached_read_tokens: 3,
        cached_write_tokens: 4,
    };
    old.owed_trailing_idles = 2;
    old.pending_orphan_results = 1;
    let turn = old.take_active_for_restart().expect("an active turn");
    assert!(!old.has_active() && !old.has_unsettled());
    assert_eq!(old.owed_trailing_idles, 0, "the old query's debt is gone");
    assert_eq!(old.pending_orphan_results, 0);

    let mut fresh = TurnMachine::new();
    fresh.adopt_restarted_turn(turn);
    fresh.enqueue(Turn::new("later".into(), false));
    assert_eq!(fresh.foreground_target(), Some("p1"), "rebased to the head");
    // The continuation's echo activates it; its own result adds on top.
    fresh.on_echo("p1");
    let events = fresh.on_result(&result_frame("success", false), false);
    match &events[..] {
        [TurnEvent::Settled { usage, .. }] => {
            assert_eq!(usage.input_tokens, 11 + 3);
            assert_eq!(usage.output_tokens, 22 + 5);
            assert_eq!(usage.cached_read_tokens, 3);
            assert_eq!(usage.cached_write_tokens, 4);
        }
        other => panic!("expected the settle, got {other:?}"),
    }
    // The carry is consumed by the first activation only.
    fresh.on_echo("later");
    let events = fresh.on_result(&result_frame("success", false), false);
    match &events[..] {
        [TurnEvent::Settled { usage, .. }] => assert_eq!(usage.input_tokens, 3),
        other => panic!("expected the settle, got {other:?}"),
    }
    // A settled or absent turn has nothing to carry.
    assert!(fresh.take_active_for_restart().is_none());
}

// ------------------------------------------------ F18 (V-03)

/// V-03 — `failAllTurns` after a cancel: the abort the cancel started can kill the
/// stream, and that error is the cancel's, not the work's, so the active turn ends
/// `cancelled`; queued turns (which came after the cancel) are rejected; a held
/// turn keeps its recorded outcome.
#[test]
fn v03_fail_all_after_cancel_settles_the_active_turn_cancelled() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.cancel();
    machine.enqueue(Turn::new("p2".into(), false)); // enqueued after the cancel
    let events = machine.fail_all(FailureKind::SessionEnded);
    assert_eq!(settled(&events), vec![("p1", StopReason::Cancelled)]);
    assert_eq!(failed(&events), vec![("p2", FailureKind::SessionEnded)]);

    // Without a cancel the active turn is rejected.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let events = machine.fail_all(FailureKind::SessionEnded);
    assert_eq!(failed(&events), vec![("p1", FailureKind::SessionEnded)]);

    // A held turn resolves with its recorded outcome, cancelled or not.
    let mut machine = held_machine();
    machine.cancelled = true;
    let events = machine.fail_all(FailureKind::SessionEnded);
    assert_eq!(settled(&events), vec![("p1", StopReason::EndTurn)]);
}

// ------------------------------------------------ F16c (B-07, B-42, B-61)

fn main_output(kind: &str) -> Value {
    serde_json::json!({"type": kind, "parent_tool_use_id": null})
}

/// B-61 — a main-conversation output with no turn in flight starts a model cycle
/// Claude Code began on its own: it is foreground work, so after a cancel the
/// work end is not reported until the cycle ends and its output still flows.
#[test]
fn f16c_autonomous_cycle_is_foreground_work() {
    let mut machine = TurnMachine::new();
    // A subagent's output starts nothing.
    machine.start_autonomous_cycle(
        &serde_json::json!({"type": "assistant", "parent_tool_use_id": "t"}),
    );
    assert!(!machine.runs_autonomous_cycle());
    machine.start_autonomous_cycle(&main_output("assistant"));
    assert!(machine.runs_autonomous_cycle());
    assert!(
        !machine.work_end_reported(),
        "a running cycle is unreported work"
    );

    // Cancelled: the cycle's stream is not yet dropped (the work end is not
    // reported) and its result ends it; afterwards the stopped work is dropped.
    machine.cancel();
    assert!(!machine.drops_stopped_work_output());
    assert!(!machine.gate_stream_event(&text_delta_event(Value::Null)));
    machine.on_result(&autonomous_result("success"), true);
    assert!(
        !machine.runs_autonomous_cycle(),
        "the cycle ends at its result"
    );
    assert!(machine.drops_stopped_work_output());
    assert!(machine.gate_stream_event(&text_delta_event(Value::Null)));
}

/// B-61 — output of the work a cancel stopped starts no cycle; a turn's work, a
/// held turn's follow-up, queued turns and subagent output are told apart.
#[test]
fn f16c_cycle_start_conditions() {
    // A running turn owns the work.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.start_autonomous_cycle(&main_output("stream_event"));
    assert!(!machine.runs_autonomous_cycle());

    // A held turn's follow-up is a cycle (the turn's work ended at its result).
    machine.on_task_started("t1", true);
    machine.on_result(&result_frame("success", false), false);
    assert!(machine.has_active());
    machine.start_autonomous_cycle(&main_output("assistant"));
    assert!(machine.runs_autonomous_cycle());

    // A cancel's stopped work starts nothing.
    let mut machine = TurnMachine::new();
    machine.cancel();
    machine.start_autonomous_cycle(&main_output("assistant"));
    assert!(!machine.runs_autonomous_cycle());

    // A queued turn: its output may come before its echo on a CLI without
    // lifecycle frames; with them, only once Claude Code started it.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.start_autonomous_cycle(&main_output("assistant"));
    assert!(
        !machine.runs_autonomous_cycle(),
        "no lifecycle frames: maybe the queued turn's"
    );
    let mut machine = TurnMachine::new();
    machine.latch_capabilities(&["msg_lifecycle_v1"]);
    machine.enqueue(Turn::new("p1".into(), false));
    machine.start_autonomous_cycle(&main_output("assistant"));
    assert!(
        machine.runs_autonomous_cycle(),
        "lifecycle CLI: not started, so not its output"
    );
    let mut machine = TurnMachine::new();
    machine.latch_capabilities(&["msg_lifecycle_v1"]);
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_command_lifecycle("p1", "started");
    machine.start_autonomous_cycle(&main_output("assistant"));
    assert!(
        !machine.runs_autonomous_cycle(),
        "the queued turn was started: its output"
    );
}

/// B-07 — the cycle ends at a result (not at a `num_turns: 0` placeholder), at an
/// idle no earlier result owes, and when a turn takes it over.
#[test]
fn f16c_cycle_end_lanes() {
    let placeholder = || {
        let mut v = autonomous_result("success");
        v["num_turns"] = serde_json::json!(0);
        v
    };
    let started = || {
        let mut machine = TurnMachine::new();
        machine.start_autonomous_cycle(&main_output("assistant"));
        assert!(machine.runs_autonomous_cycle());
        machine
    };
    let mut machine = started();
    machine.on_result(&placeholder(), true);
    assert!(
        machine.runs_autonomous_cycle(),
        "a placeholder ends nothing"
    );
    machine.on_result(&autonomous_result("success"), true);
    assert!(!machine.runs_autonomous_cycle());

    // An idle that no result owes ends it; an owed one (lagging trailer) does not.
    let mut machine = started();
    machine.owed_trailing_idles = 1;
    machine.on_idle();
    assert!(
        machine.runs_autonomous_cycle(),
        "an owed idle belongs to an earlier result"
    );
    machine.on_idle();
    assert!(!machine.runs_autonomous_cycle());

    // A turn that activates takes it over.
    let mut machine = started();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(!machine.runs_autonomous_cycle());

    // Stream end ends it.
    let mut machine = started();
    machine.on_stream_end();
    assert!(!machine.runs_autonomous_cycle());
}

/// F16c — a placeholder result (`num_turns: 0`) does not drain a held turn: the
/// real follow-up result does.
#[test]
fn f16c_placeholder_result_holds_the_turn() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("t1", true);
    machine.on_result(&result_frame("success", false), false);
    machine.on_task_ended("t1");
    let mut placeholder = autonomous_result("success");
    placeholder["num_turns"] = serde_json::json!(0);
    assert!(settled(&machine.on_result(&placeholder, true)).is_empty());
    assert!(machine.has_active(), "held through the placeholder");
    let mut real = autonomous_result("success");
    real["num_turns"] = serde_json::json!(1);
    assert_eq!(
        settled(&machine.on_result(&real, true)),
        vec![("p1", StopReason::EndTurn)]
    );
}

/// B-42 — the live `local_workflow` tasks a cancel stops, only for a client with
/// the per-task stop affordance.
#[test]
fn f16c_background_workflows_to_stop() {
    let mut machine = TurnMachine::new();
    machine.on_task_started("wf-1", false);
    machine.note_task_meta("wf-1", None, true, false, Some("local_workflow"));
    machine.on_task_started("bash-1", false);
    machine.note_task_meta("bash-1", None, true, false, Some("local_bash"));
    assert!(
        machine.background_workflows_to_stop().is_empty(),
        "flag off"
    );
    machine.set_stops_workflows_on_cancel(true);
    assert_eq!(
        machine.background_workflows_to_stop(),
        vec!["wf-1".to_string()]
    );
    machine.on_task_ended("wf-1");
    assert!(machine.background_workflows_to_stop().is_empty());
}

/// B-13 / N-29 — the begin half activates the queue head for an echo-less result
/// BEFORE the result's own emissions, so the actor reads a reset scratch; the
/// finish half then settles it.
#[test]
fn f16a_result_begin_activates_before_emissions() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("pc".into(), true));
    let begin = machine.on_result_begin(&result_frame("success", false));
    assert!(!begin.autonomous);
    assert!(
        begin
            .events
            .iter()
            .any(|e| matches!(e, TurnEvent::Activated { prompt_uuid } if prompt_uuid == "pc")),
        "ensureActiveTurn promotes the echo-less head first"
    );
    assert!(machine.has_active(), "not settled until the finish half");
    let events = machine.on_result_finish(&result_frame("success", false), false, false, begin);
    assert_eq!(settled(&events), vec![("pc", StopReason::EndTurn)]);
}

/// Follow-up (d) — the per-model reading advances AFTER activation reset the
/// turn scratch: an echo-less promoted first resumed result no longer sees the
/// previous turn's assistant model.
#[test]
fn f16a_model_reading_advances_after_activation_reset() {
    let mut machine = TurnMachine::new();
    machine.mark_resumed();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.note_assistant_model("claude-sonnet-4-5");
    machine.on_result(&result_frame("success", false), false);
    // The resumed seed was consumed by p1's result; a fresh machine with a
    // stale model and an echo-less head is the case that matters.
    let mut machine = TurnMachine::new();
    machine.mark_resumed();
    machine.note_assistant_model("stale-model");
    machine.enqueue(Turn::new("pc".into(), true));
    machine.on_result(&result_frame("success", false), false);
    assert_eq!(
        machine.last_assistant_model, None,
        "activation cleared lastAssistantModel before the reading advanced"
    );
}
