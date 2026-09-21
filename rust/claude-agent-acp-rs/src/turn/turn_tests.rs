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

fn result_frame_usage(subtype: &str, is_error: bool, usage: Value) -> Value {
    serde_json::json!({
        "type": "result",
        "subtype": subtype,
        "is_error": is_error,
        "stop_reason": "end_turn",
        "usage": usage,
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

fn settled_events(events: &[TurnEvent]) -> Vec<(&str, StopReason)> {
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

/// 6.T1 (INV-16) — a turn with 2 live subagents does not settle until they
/// drain AND a followup result (or idle) settles it. The second `task_ended`
/// must NOT settle (review 04 #8): `on_task_ended` only removes the live
/// entry; the drain happens at the followup's autonomous result.
#[tokio::test(flavor = "multi_thread")]
async fn inv_16_subagents_hold_settle() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(machine.has_active());

    machine.on_task_started("sub1", true);
    machine.on_task_started("sub2", true);

    // A result arrives while both are live: it must DEFER, not settle.
    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        settled_events(&events).is_empty(),
        "turn must not settle while both subagents are live"
    );
    assert!(machine.has_active(), "held turn still active");

    // Drain the first subagent: still held open.
    let events = machine.on_task_ended("sub1");
    assert!(events.is_empty(), "task_ended emits no events");
    assert!(
        machine.has_active(),
        "still held after first subagent drains"
    );

    // Drain the second: on_task_ended must NOT settle it — the summary is
    // promised to stream inside the turn.
    let events = machine.on_task_ended("sub2");
    assert!(
        settled_events(&events).is_empty(),
        "second task_ended must not settle the turn (review 04 #8)"
    );
    assert!(
        machine.has_active(),
        "turn still held open until followup/idle"
    );

    // The followup autonomous result is what drains the held turn.
    let events = machine.on_result(&autonomous_result("success"), true);
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::EndTurn)],
        "the followup result must settle the held turn with its deferred outcome"
    );
    assert!(!machine.has_active());
}

/// 6.T2 (INV-29) — result text present only in the `result` frame yields a
/// `TurnEvent::FinalText` (the #453 fallback); emission is phase 7.
#[tokio::test(flavor = "multi_thread")]
async fn inv_29_result_text_fallback() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");

    // Cache-replay signature: no assistant text delivered, output_tokens==0.
    let result = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "the whole answer",
        "usage": { "output_tokens": 0 },
    });
    let events = machine.on_result(&result, false);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::FinalText { text } if text == "the whole answer")),
        "the result-text fallback must yield a FinalText event"
    );
    assert!(
        settled_events(&events).contains(&("p1", StopReason::EndTurn)),
        "the turn must also settle end_turn"
    );
    assert!(
        !machine.has_active(),
        "the turn settles, clearing the active slot"
    );
}

/// 6.T3 (INV-30) — the real orphan-coalescing timeline (review 04 #9): p1
/// active, p2 queued (pushed), cancel sweeps p2 cancelled + credited; idle
/// settles p1; p3 (echo-less) enqueued; p2's late result must not activate
/// or settle p3; p3's own result then activates and settles it.
#[tokio::test(flavor = "multi_thread")]
async fn inv_30_orphan_result_not_reused() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("p2".into(), false)); // pushed, not yet echoed

    // Cancel: p2 (queued) settles cancelled + seeds an orphan; p1 stays
    // active to settle at idle.
    let events = machine.cancel();
    assert_eq!(
        settled_events(&events),
        vec![("p2", StopReason::Cancelled)],
        "cancel must sweep the queued turn to cancelled"
    );
    assert!(machine.has_active(), "p1 still active, settles at idle");

    // p1's trailing idle settles it.
    let events = machine.on_idle();
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::Cancelled)],
        "idle settles the cancelled active turn"
    );
    assert!(!machine.has_active());

    // p3 (echo-less local-only command, e.g. /compact) is enqueued.
    machine.enqueue(Turn::new("p3".into(), true));

    // p2's late result arrives with no active turn: the orphan credit
    // absorbs it — p3 must NOT be activated or settled by it.
    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        settled_events(&events).is_empty(),
        "p2's late result must not settle p3"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TurnEvent::Activated { .. })),
        "p2's late result must not activate p3 (orphan order, ADP:1453)"
    );
    assert!(
        !machine.has_active(),
        "p3 not active after the orphan result"
    );

    // p3's own result activates (echo-less promote) and settles it.
    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Activated { prompt_uuid } if prompt_uuid == "p3")),
        "p3's own result must activate it via ensure_active_turn"
    );
    assert_eq!(
        settled_events(&events),
        vec![("p3", StopReason::EndTurn)],
        "p3's own result must settle it"
    );
    assert!(!machine.has_active());
}

/// 6.T4 (INV-31) — idle without a `result` fails the active turn instead of
/// hanging (#825), with the verbatim upstream `TURN_NO_RESULT_MESSAGE`.
#[tokio::test(flavor = "multi_thread")]
async fn inv_31_idle_without_result_fails() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    assert!(machine.has_active());

    // No result ever arrived; the SDK goes idle.
    let events = machine.on_idle();
    let failed = events.iter().find_map(|e| match e {
        TurnEvent::Failed {
            prompt_uuid,
            kind,
            message,
        } => Some((prompt_uuid.as_str(), *kind, message.clone())),
        _ => None,
    });
    assert_eq!(
        failed,
        Some((
            "p1",
            FailureKind::NoResult,
            TURN_NO_RESULT_MESSAGE.to_string()
        )),
        "idle without a result must fail the active turn with NoResult"
    );
    assert!(
        !machine.has_active(),
        "a failed turn clears the active slot"
    );
}

/// Blocker #1 (review) — the held-turn idle drain must EMIT its `Settled`
/// event (the actor needs it to resolve the oneshot), not discard it.
#[tokio::test(flavor = "multi_thread")]
async fn held_idle_drain_emits_settled() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("sub", true);
    machine.on_result(&result_frame("success", false), false); // defers

    // End the subagent via task_ended — no settle expected (review 04 #8).
    machine.on_task_ended("sub");
    assert!(machine.has_active());

    // The held turn drains at idle (fallback when no followup comes).
    let events = machine.on_idle();
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::EndTurn)],
        "the held-idle drain must emit Settled so the prompt resolves"
    );
    assert!(!machine.has_active());
}

/// Blocker #2 (review) — an echo-less local-only command result promotes the
/// queue head via `ensure_active_turn` and settles it.
#[tokio::test(flavor = "multi_thread")]
async fn echo_less_result_promotes_head() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), true)); // local-only, no echo
    assert!(!machine.has_active());

    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Activated { prompt_uuid } if prompt_uuid == "p1")),
        "ensure_active_turn must promote the queue head on an echo-less result"
    );
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::EndTurn)],
        "the promoted echo-less turn must settle"
    );
    assert!(!machine.has_active());
}

/// Blocker #3 (review) — a lagging trailing idle must not false-fail the
/// next active turn (#825): A settles, B activates, A's late idle is
/// absorbed via `owed_trailing_idles`.
#[tokio::test(flavor = "multi_thread")]
async fn lagging_idle_after_next_echo_absorbed() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("A".into(), false));
    machine.on_echo("A");
    machine.on_result(&result_frame("success", false), false); // settles A
    assert!(!machine.has_active());

    // The user sends B; B's echo activates it.
    machine.enqueue(Turn::new("B".into(), false));
    machine.on_echo("B");
    assert!(machine.has_active());

    // A's late idle arrives — must be absorbed, NOT fail B.
    let events = machine.on_idle();
    assert!(
        settled_events(&events).is_empty()
            && !events.iter().any(|e| matches!(e, TurnEvent::Failed { .. })),
        "A's lagging idle must not fail B (#825 false-fail)"
    );
    assert!(machine.has_active(), "B still active and healthy");
}

/// Blocker #4 (review) — an autonomous followup result must not fail the
/// next live turn or settle it with the followup's outcome: it only drains
/// a held turn with its stored outcome and owes a trailing idle.
#[tokio::test(flavor = "multi_thread")]
async fn autonomous_result_does_not_touch_live_turn() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_result(&result_frame("success", false), false); // settles p1
    assert!(!machine.has_active());

    // The next prompt is active.
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");

    // An autonomous followup with is_error / /login text lands — must NOT
    // fail p2 or settle it.
    let followup = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "result": "Please run /login",
        "origin": { "kind": "task-notification" },
        "usage": { "output_tokens": 1 },
    });
    let events = machine.on_result(&followup, true);
    assert!(
        settled_events(&events).is_empty()
            && !events.iter().any(|e| matches!(e, TurnEvent::Failed { .. })),
        "an autonomous result must never fail or settle the live turn"
    );
    assert!(machine.has_active(), "p2 still active");

    // p2's own result settles it normally.
    let events = machine.on_result(&result_frame("success", false), false);
    assert_eq!(
        settled_events(&events),
        vec![("p2", StopReason::EndTurn)],
        "p2's own result settles it"
    );
    assert!(!machine.has_active());
}

/// Blocker #4b (review) — an autonomous followup drains a held turn with its
/// STORED outcome, not the followup's own stop reason / error.
#[tokio::test(flavor = "multi_thread")]
async fn autonomous_followup_settles_held_with_stored_outcome() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("sub", true);
    machine.on_result(&result_frame("success", false), false); // defers, EndTurn
    machine.on_task_ended("sub"); // no settle (review 04 #8)
    assert!(machine.has_active());

    // A followup that would normally fail (is_error) — as an autonomous
    // result it must settle the held turn with its STORED EndTurn outcome.
    let followup = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "result": "some provider error",
        "origin": { "kind": "task-notification" },
        "usage": { "output_tokens": 1 },
    });
    let events = machine.on_result(&followup, true);
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::EndTurn)],
        "a held turn must settle with its stored outcome, not the followup's"
    );
    assert!(!machine.has_active());
}

/// Major #5 (review) — a settled turn clears the active slot, so a later
/// echo-less result is neither orphan-accounted nor promoted wrongly.
#[tokio::test(flavor = "multi_thread")]
async fn settle_clears_active_slot() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_result(&result_frame("success", false), false);
    assert!(
        !machine.has_active(),
        "settle must clear the active slot (ADP:1592)"
    );

    // A fail must clear it too.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    machine.on_result(
        &serde_json::json!({
            "type": "result", "subtype": "success", "is_error": true,
            "result": "boom", "usage": {"output_tokens": 1},
        }),
        false,
    );
    assert!(
        !machine.has_active(),
        "fail must clear the active slot (ADP:1623)"
    );
}

/// Major #6 (review) — the full (subtype × is_error × stop_reason) grid,
/// mirroring `ADP:2771-2848`: max_tokens wins over is_error for
/// success/error_during_execution; /login applies to success regardless of
/// is_error; the error_max_* subtypes fail on is_error with their category.
#[tokio::test(flavor = "multi_thread")]
async fn result_subtype_grid() {
    fn run(
        subtype: &str,
        is_error: bool,
        stop_reason: &str,
    ) -> (Option<StopReason>, Option<FailureKind>) {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        let frame = serde_json::json!({
            "type": "result", "subtype": subtype, "is_error": is_error,
            "stop_reason": stop_reason,
            "errors": ["e1", "e2"],
            "usage": {"output_tokens": 5},
        });
        let events = machine.on_result(&frame, false);
        let settle = settled_events(&events).first().map(|(_, s)| *s);
        let fail = events.iter().find_map(|e| match e {
            TurnEvent::Failed { kind, .. } => Some(*kind),
            _ => None,
        });
        (settle, fail)
    }

    // success + max_tokens wins over is_error -> MaxTokens.
    let (s, f) = run("success", true, "max_tokens");
    assert_eq!(s, Some(StopReason::MaxTokens));
    assert_eq!(f, None);
    // success + no max_tokens + is_error -> ProviderError.
    let (s, f) = run("success", true, "end_turn");
    assert_eq!(s, None);
    assert_eq!(f, Some(FailureKind::ProviderError));
    // success + no error -> EndTurn.
    let (s, f) = run("success", false, "end_turn");
    assert_eq!(s, Some(StopReason::EndTurn));
    assert_eq!(f, None);

    // error_during_execution + max_tokens -> MaxTokens.
    let (s, f) = run("error_during_execution", true, "max_tokens");
    assert_eq!(s, Some(StopReason::MaxTokens));
    assert_eq!(f, None);
    // error_during_execution + is_error -> ProviderError.
    let (s, f) = run("error_during_execution", true, "end_turn");
    assert_eq!(s, None);
    assert_eq!(f, Some(FailureKind::ProviderError));
    // error_during_execution, no error -> EndTurn.
    let (s, f) = run("error_during_execution", false, "end_turn");
    assert_eq!(s, Some(StopReason::EndTurn));
    assert_eq!(f, None);

    // error_max_*: max_tokens does NOT override (review 04 #6); is_error
    // fails with the category, else max_turn_requests.
    let (s, f) = run("error_max_budget_usd", true, "max_tokens");
    assert_eq!(s, None);
    assert_eq!(f, Some(FailureKind::BudgetExhausted));
    let (s, f) = run("error_max_budget_usd", false, "end_turn");
    assert_eq!(s, Some(StopReason::MaxTurnRequests));
    assert_eq!(f, None);

    let (s, f) = run("error_max_turns", true, "end_turn");
    assert_eq!(s, None);
    assert_eq!(f, Some(FailureKind::ContextExhausted));
    let (s, f) = run("error_max_turns", false, "end_turn");
    assert_eq!(s, Some(StopReason::MaxTurnRequests));
    assert_eq!(f, None);

    let (s, f) = run("error_max_structured_output_retries", true, "end_turn");
    assert_eq!(s, None);
    assert_eq!(f, Some(FailureKind::ProviderError));
    let (s, f) = run("error_max_structured_output_retries", false, "end_turn");
    assert_eq!(s, Some(StopReason::MaxTurnRequests));
    assert_eq!(f, None);

    // refusal wins on any subtype, even with is_error.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let events = machine.on_result(
        &serde_json::json!({
            "type": "result", "subtype": "success", "is_error": true,
            "stop_reason": "refusal", "usage": {"output_tokens": 1},
        }),
        false,
    );
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::Refusal)],
        "refusal is handled before the subtype switch"
    );
}

/// Major #7 (review) — /login maps to AuthRequired even with is_error, and
/// error messages join with ", " rather than taking the first.
#[tokio::test(flavor = "multi_thread")]
async fn auth_required_and_error_join() {
    // /login on success with is_error still fails AuthRequired.
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
    let fail = events.iter().find_map(|e| match e {
        TurnEvent::Failed { kind, message, .. } => Some((*kind, message.clone())),
        _ => None,
    });
    assert_eq!(
        fail,
        Some((FailureKind::AuthRequired, "Please run /login".to_string())),
        "/login fails AuthRequired even when is_error is set"
    );
    assert!(!machine.has_active());

    // error_during_execution joins errors with ", ".
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    let events = machine.on_result(
        &serde_json::json!({
            "type": "result", "subtype": "error_during_execution", "is_error": true,
            "errors": ["alpha", "beta"], "usage": {"output_tokens": 1},
        }),
        false,
    );
    let fail = events.iter().find_map(|e| match e {
        TurnEvent::Failed { message, .. } => Some(message.clone()),
        _ => None,
    });
    assert_eq!(
        fail,
        Some("alpha, beta".to_string()),
        "errors join with ', ' (ADP:2818)"
    );
}

/// Major #9 (review) — `cancel()` sweeps queued turns to `Settled{Cancelled}`
/// plus orphan credits, and inline-settles a held active turn `cancelled`.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_sweeps_queued_and_inline_settles_held() {
    // Two queued, one active-held.
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("sub", true);
    machine.on_result(&result_frame("success", false), false); // p1 defers
    machine.enqueue(Turn::new("q1".into(), false));
    machine.enqueue(Turn::new("q2".into(), false));

    let events = machine.cancel();
    assert_eq!(
        settled_events(&events),
        vec![
            ("q1", StopReason::Cancelled),
            ("q2", StopReason::Cancelled),
            ("p1", StopReason::Cancelled),
        ],
        "cancel sweeps queued turns and inline-settles the held active turn"
    );
    assert!(!machine.has_active(), "no active turn after cancel");

    // The orphan credit swallows q1's late result (must not promote anything).
    machine.enqueue(Turn::new("next".into(), false));
    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        settled_events(&events).is_empty()
            && !events
                .iter()
                .any(|e| matches!(e, TurnEvent::Activated { .. })),
        "q1's late result is consumed as an orphan, not attributed to 'next'"
    );
}

/// Major #10 (review) — `on_stream_end()` settles the active turn and
/// rejects queued turns with `SESSION_ENDED_MESSAGE`.
#[tokio::test(flavor = "multi_thread")]
async fn stream_end_settles_active_rejects_queued() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("q1".into(), false));

    let events = machine.on_stream_end();
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::EndTurn)],
        "stream end settles the active turn with the scratch outcome"
    );
    let failed = events.iter().filter_map(|e| match e {
        TurnEvent::Failed { kind, message, .. } => Some((*kind, message.as_str())),
        _ => None,
    });
    assert_eq!(
        failed.collect::<Vec<_>>(),
        vec![(FailureKind::SessionEnded, SESSION_ENDED_MESSAGE)],
        "queued turns are rejected with SESSION_ENDED_MESSAGE"
    );
    assert!(!machine.has_active());
}

/// Major #10b (review) — a held active turn settles with its deferred
/// outcome on stream end, not a failure.
#[tokio::test(flavor = "multi_thread")]
async fn stream_end_settles_held_with_deferred() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.on_task_started("sub", true);
    machine.on_result(&result_frame("success", false), false); // defers
    machine.on_task_ended("sub");

    let events = machine.on_stream_end();
    assert_eq!(
        settled_events(&events),
        vec![("p1", StopReason::EndTurn)],
        "a held turn resolves with its deferred outcome on stream end"
    );
}

/// Major #10c (review) — `fail_all` rejects every turn except a held one,
/// which resolves with its deferred outcome.
#[tokio::test(flavor = "multi_thread")]
async fn fail_all_rejects_all_but_held() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    machine.enqueue(Turn::new("q1".into(), false));

    let events = machine.fail_all(FailureKind::SessionEnded);
    let failed = events.iter().filter_map(|e| match e {
        TurnEvent::Failed {
            kind, prompt_uuid, ..
        } => Some((prompt_uuid.as_str(), *kind)),
        _ => None,
    });
    assert_eq!(
        failed.collect::<Vec<_>>(),
        vec![
            ("p1", FailureKind::SessionEnded),
            ("q1", FailureKind::SessionEnded)
        ],
        "fail_all rejects the active and queued turns"
    );
    assert!(!machine.has_active());
}

/// Major #11 (review) — `Settled`/`Failed`/`Activated` all carry the
/// `prompt_uuid` so the actor can resolve the right oneshot.
#[tokio::test(flavor = "multi_thread")]
async fn events_carry_prompt_uuid() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("abc".into(), false));
    let events = machine.on_echo("abc");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Activated { prompt_uuid } if prompt_uuid == "abc")),
        "Activated must carry the prompt uuid"
    );

    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        events.iter().any(|e| matches!(
            e,
            TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "abc"
        )),
        "Settled must carry the prompt uuid"
    );

    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("xyz".into(), false));
    machine.on_echo("xyz");
    let events = machine.on_result(
        &serde_json::json!({
            "type": "result", "subtype": "success", "is_error": true,
            "result": "boom", "usage": {"output_tokens": 1},
        }),
        false,
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            TurnEvent::Failed { prompt_uuid, .. } if prompt_uuid == "xyz"
        )),
        "Failed must carry the prompt uuid"
    );
}

/// Minor #12 (review) — echo hand-off checks `cancelled` before held and
/// owes a trailing idle when it cancels.
#[tokio::test(flavor = "multi_thread")]
async fn echo_handoff_cancelled_first_and_owes_idle() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("A".into(), false));
    machine.on_echo("A");
    machine.cancel(); // cancels A (active, not held -> stays for idle)
    assert!(
        machine.has_active(),
        "a non-held cancelled turn stays active"
    );

    // B's echo hands off A cancelled.
    machine.enqueue(Turn::new("B".into(), false));
    let events = machine.on_echo("B");
    assert_eq!(
        settled_events(&events),
        vec![("A", StopReason::Cancelled)],
        "echo hand-off settles a cancelled turn 'cancelled' (ADP:3034)"
    );
    assert!(machine.has_active(), "B is now active");

    // The interrupt's trailing idle is owed and absorbed, not read as B
    // abandoned (#825 false-fail).
    let events = machine.on_idle();
    assert!(
        !events.iter().any(|e| matches!(e, TurnEvent::Failed { .. })),
        "A's owed trailing idle must not fail B"
    );
    assert!(machine.has_active(), "B still active and healthy");
}

/// Minor #14 (review) — the per-turn usage accumulator is reset at
/// activation and reported on `Settled`.
#[tokio::test(flavor = "multi_thread")]
async fn usage_reset_at_activation_and_reported_on_settled() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let usage = serde_json::json!({
        "input_tokens": 10, "output_tokens": 4,
        "cache_read_input_tokens": 2, "cache_creation_input_tokens": 1,
    });
    let events = machine.on_result(&result_frame_usage("success", false, usage), false);
    let reported = events.iter().find_map(|e| match e {
        TurnEvent::Settled { usage, .. } => Some(*usage),
        _ => None,
    });
    assert_eq!(
        reported,
        Some(Usage {
            input_tokens: 10,
            output_tokens: 4,
            cached_read_tokens: 2,
            cached_write_tokens: 1,
        }),
        "Settled must carry the accumulated usage"
    );

    // A second turn starts with a zeroed accumulator.
    machine.enqueue(Turn::new("p2".into(), false));
    machine.on_echo("p2");
    let events = machine.on_result(
        &serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "usage": {"input_tokens": 7, "output_tokens": 0},
        }),
        false,
    );
    let reported = events.iter().find_map(|e| match e {
        TurnEvent::Settled { usage, .. } => Some(*usage),
        _ => None,
    });
    assert_eq!(
        reported,
        Some(Usage {
            input_tokens: 7,
            output_tokens: 0,
            cached_read_tokens: 0,
            cached_write_tokens: 0,
        }),
        "activation resets the accumulator before the next turn's result"
    );
}

/// Minor #15 / #13 (review) — the #453 fallback fires only for the success
/// arm and is suppressed when `assistant_text_delivered` is true.
#[tokio::test(flavor = "multi_thread")]
async fn fallback_suppressed_when_text_delivered() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("p1".into(), false));
    machine.on_echo("p1");
    let result = serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "result": "answer", "usage": {"output_tokens": 0},
    });
    // assistant text already delivered -> no fallback.
    let events = machine.on_result(&result, true);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TurnEvent::FinalText { .. })),
        "no fallback when assistant text was delivered"
    );
    assert!(
        settled_events(&events).contains(&("p1", StopReason::EndTurn)),
        "the turn still settles"
    );
}

/// 6.T5 — the non-error subtype table (kept for the named 6.T5 verify item;
/// the full grid is covered by `result_subtype_grid`).
#[tokio::test(flavor = "multi_thread")]
async fn inv_stop_reason_table() {
    fn run(subtype: &str) -> Option<StopReason> {
        let mut machine = TurnMachine::new();
        machine.enqueue(Turn::new("p1".into(), false));
        machine.on_echo("p1");
        let events = machine.on_result(&result_frame(subtype, false), false);
        settled_events(&events).first().map(|(_, s)| *s)
    }
    assert_eq!(run("success"), Some(StopReason::EndTurn));
    assert_eq!(run("error_during_execution"), Some(StopReason::EndTurn));
    assert_eq!(
        run("error_max_budget_usd"),
        Some(StopReason::MaxTurnRequests)
    );
    assert_eq!(run("error_max_turns"), Some(StopReason::MaxTurnRequests));
    assert_eq!(
        run("error_max_structured_output_retries"),
        Some(StopReason::MaxTurnRequests)
    );
}

/// Review-05 row 1(i): A active + B queued -> A's result settles A, B stays
/// queued, and B's echo activates B. Without the `ensure_active_turn` early
/// return this would settle/misattribute B and hang A.
#[tokio::test(flavor = "multi_thread")]
async fn result_for_active_turn_while_queued_settles_active() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("pA".into(), false));
    machine.enqueue(Turn::new("pB".into(), false));
    // A is echoed -> active.
    machine.on_echo("pA");
    assert!(machine.has_active(), "A is the active turn");

    // A's own result arrives while B is queued.
    let events = machine.on_result(&result_frame("success", false), false);
    assert_eq!(
        settled_events(&events),
        vec![("pA", StopReason::EndTurn)],
        "A's result must settle A, not promote/settle B"
    );
    // B stays queued (not activated).
    assert!(!machine.has_active(), "B must not be active yet");
    // B's echo activates B.
    let events = machine.on_echo("pB");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Activated { prompt_uuid } if prompt_uuid == "pB")),
        "B's echo must activate B"
    );
    assert!(machine.has_active());
}

/// Review-05 row 1(ii): A active + B queued -> cancel -> A's result -> idle ->
/// p3 (echo-less) enqueued -> B's late result must not activate p3 (INV-30).
#[tokio::test(flavor = "multi_thread")]
async fn orphan_late_result_never_activates_echo_less() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("pA".into(), false));
    machine.enqueue(Turn::new("pB".into(), false));
    machine.on_echo("pA");

    // Cancel sweeps B -> orphan credit.
    machine.cancel();

    // A's own result arrives (dropped, cancelled), then idle settles A.
    machine.on_result(&result_frame("success", false), false);
    let events = machine.on_idle();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "pA")),
        "A settles at idle"
    );

    // p3 (echo-less local-only) enqueued.
    machine.enqueue(Turn::new("p3".into(), true));

    // B's late result arrives: it is an orphan, must NOT activate/settle p3.
    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "p3")),
        "B's orphaned result must never settle p3"
    );
    assert!(
        !machine.has_active(),
        "B's orphaned result must not activate p3"
    );
}

/// Review-05 row 1(iii): A held + /context queued -> its result settles A with
/// the deferred outcome, then promotes and settles /context.
#[tokio::test(flavor = "multi_thread")]
async fn held_turn_result_promotes_and_settles_local_only() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("pA".into(), false));
    machine.on_echo("pA");
    // A spawns a subagent -> a result defers A.
    machine.on_task_started("sub1", true);
    let events = machine.on_result(&result_frame("success", false), false);
    assert!(
        settled_events(&events).is_empty(),
        "A defers while subagent live"
    );
    assert!(machine.has_active());

    // /context queued (echo-less).
    machine.enqueue(Turn::new("ctx".into(), true));

    // The followup autonomous result drains A (deferred end_turn), then a user
    // result for /context promotes and settles it.
    machine.on_result(&autonomous_result("success"), false);
    let events = machine.on_result(
        &serde_json::json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"context out","usage":{"output_tokens":0}}),
        false,
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Settled { prompt_uuid, .. } if prompt_uuid == "ctx")),
        "the /context result must settle the promoted local-only turn"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, TurnEvent::FinalText { .. })));
}

/// Review-05 row 2: a held turn cancelled while the session is `running` owes
/// an interrupt trailer so the next echoed prompt's trailing idle is absorbed,
/// not a false #825 fail.
#[tokio::test(flavor = "multi_thread")]
async fn held_cancel_debt_uses_running_state() {
    let mut machine = TurnMachine::new();
    machine.enqueue(Turn::new("pA".into(), false));
    machine.on_echo("pA");
    // A defers (held) and the session moves to running (a followup).
    machine.on_task_started("sub1", true);
    machine.on_result(&result_frame("success", false), false);
    machine.on_session_state("running");

    // Cancel inline-settles the held turn; because the session is NOT idle it
    // owes one trailing idle.
    machine.cancel();

    // B enqueued + echoed (next prompt).
    machine.enqueue(Turn::new("pB".into(), false));
    machine.on_echo("pB");

    // The interrupt's trailing idle arrives while B is active + unsettled.
    let events = machine.on_idle();
    assert!(
        events.is_empty(),
        "the owed trailing idle must be absorbed, not fail B (#825 false-fail)"
    );
    assert!(machine.has_active(), "B must remain active");
}
