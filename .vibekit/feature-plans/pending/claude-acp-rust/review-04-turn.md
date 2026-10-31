# Review 04 — `turn.rs` vs upstream turn settlement

Subject: `rust/claude-agent-acp-rs/src/turn.rs` (666 lines, incl. tests).
Upstream: `claude-agent-acp@0.70.0 dist/acp-agent.js` (`ADP` below).

**Short answer:** No. The machine gets the easy paths right: uuid-keyed echo activation, the
`settle_or_defer` gate, the refusal-first check, and the non-error subtype table. When driven by
a real stream it would hang or misreport turns in at least four common timelines: echo-less
results, a lagging idle, followup results, and a held turn that drains at idle. The phase 6 tests
pass partly because they assert weaker or different behaviour than upstream.

## Findings

| # | Severity | Finding | Evidence (path:line) | Recommended change |
|---|---|---|---|---|
| 1 | blocker | **The held-turn idle drain drops its `Settled` event.** `on_idle` calls `self.settle_deferred_if_drained()` and discards the `Vec<TurnEvent>` it returns. The turn is marked `settled = true`, but the actor never receives `Settled`, so the prompt's oneshot is never resolved and the prompt hangs forever. This is the upstream fallback drain path when no followup result comes. | `turn.rs:337-340`, `turn.rs:369-383`; upstream `ADP:2090-2107` | `events.extend(self.settle_deferred_if_drained())`. Add a test: result → defer → `on_task_ended` while a *second* subagent is live → end that one via `on_task_ended` with no settle expected, then `on_idle` → expect `Settled`. Better still, make `settle_deferred_if_drained` take `&mut Vec<TurnEvent>` like every other helper, so this class of bug can't happen. |
| 2 | blocker | **No `ensureActiveTurn`.** A result that arrives with no active turn only decrements the orphan counter. It never promotes the queue head. Upstream promotes the first unsettled queued turn when the orphan count is 0 (`ADP:1446-1490`). Echo-less turns therefore never activate or settle: local-only commands (`/context`), `/compact`, and any result that arrives before its echo. The same happens when `active` is `Some(settled)`, because `settle`/`fail_active` never clear `active` (finding 5). Those prompts hang. | `turn.rs:255-259`, `turn.rs:405-428`; upstream `ADP:1400-1490`, `ADP:2556-2558` | Port `ensure_active_turn()` and call it first in `on_result`, in this order: (a) if the active turn is held, settle it with its `deferred_settle` and fall through; (b) if `pending_orphan_results > 0`, decrement and return (this runs **before** the head check — the `ADP:1453` order); (c) promote `queue.front()` via `activate`. `Turn::is_local_only_command` already exists but is useless without this. |
| 3 | blocker | **No owed-trailing-idle accounting (`owedTrailingIdles`).** Upstream settles at the result (#773) and records one debt per user-turn result, so the lagging trailing idle is absorbed. `turn.rs` has no debt counter. Real timeline: A's result settles A → the user sends B → B's echo activates B → A's late idle arrives → `on_idle` sees an active, unsettled B and **fails B** with "idle without result". This is the exact #825 false-fail that upstream documents at length. The INV-31 guard is correct only on paper. | `turn.rs:329-349`; upstream `ADP:2051-2063`, `2108-2118`, `2590-2627`, `3046`, `3074`, `3572-3584` | Add `owed_trailing_idles: usize`. Increment it on every user-turn result, except the cancelled *active* turn's own result. Also increment it on every autonomous result, on a cancelled echo hand-off, and on a held-turn cancel when `last_session_state != idle`. In `on_idle`, check in this order: cancelled → settle Cancelled; held → decrement debt if > 0, then drain; debt > 0 → decrement and return; only then #825-fail. Add a test: settle A, activate B, idle → B must **not** fail. |
| 4 | blocker | **Autonomous results (`origin.kind` ∈ task-notification, peer, coordinator, observer, observer-activity) are not recognised.** Every result goes down the user-turn lane. Consequences: (a) a subagent followup result overwrites a held turn's deferred outcome, or settles it with the followup's stop reason, when it should settle with the stored outcome; (b) a followup that lands after the next prompt is active settles or fails **that** prompt (`is_error`, `Please run /login`); (c) the #453 fallback injects background prose as a `FinalText`; (d) with no active turn, it consumes an orphan credit that belongs to a real orphan. | `turn.rs:253-324`; upstream `ADP:114-120`, `2539`, `2689-2717` | At the top of `on_result`, check `origin.kind`. For an autonomous result: owe one trailing idle, call `settle_deferred_if_drained()` (emitting its events), and return. Never touch the orphan counter, fail, the fallback, or the stop reason. |
| 5 | major | **Settled and failed turns stay in `active`.** `settle`/`fail_active` set `settled = true` but leave `active = Some(turn)`. Upstream nulls `activeTurn` in `settleActive`/`failActive`. This is the root cause of the "result with a settled active turn is neither orphan-accounted nor promoted" part of finding 2. It also lets `on_result` still emit `FinalText` for an already-settled turn, because the fallback runs before `settle_or_defer`'s settled check. | `turn.rs:405-428`, `turn.rs:313-320`; upstream `ADP:1579-1587`, `1624-1627` | Set `self.active = None` in `settle` and `fail_active` (`abandon_active` already does). Remove the `settled` checks that then become dead code. |
| 6 | major | **Stop-reason table: `is_error` is checked before `max_tokens`, and `max_tokens` applies to every subtype.** Upstream, for `success` and `error_during_execution`, checks `stop_reason == "max_tokens"` **before** `is_error`. A result with `is_error` and `max_tokens` therefore ends `max_tokens`, but `turn.rs` fails it. Upstream does **not** apply the `max_tokens` override to `error_max_budget_usd` / `error_max_turns` / `error_max_structured_output_retries` (those give `max_turn_requests`, or fail when `is_error`), but `turn.rs` returns `MaxTokens` for them. | `turn.rs:292-303`; upstream `ADP:2762-2771`, `2798-2830` | Replace the flat order with a `match subtype` that mirrors `ADP:2762-2842` case by case: `success` → /login check, then max_tokens, then is_error, then fallback, end_turn; `error_during_execution` → max_tokens, then is_error, then end_turn; each `error_max_*` → is_error fails with its category, otherwise max_turn_requests. Extend 6.T5 to drive `on_result` with the full (subtype × is_error × stop_reason) grid, not only `stop_reason_for_subtype`. |
| 7 | major | **Failure shape loses what the client needs.** Upstream's `Please run /login` check applies only to `success`, and applies even when `is_error` is set. `turn.rs` requires `!is_error` and applies the check to every subtype. `error_message` takes the **first** error, where upstream uses `errors.join(", ") \|\| subtype`, and for `success`+`is_error` upstream uses `message.result`. `Failed { message }` has no kind, but upstream uses `RequestError.authRequired()` for /login and `internalError` with `errorKindData(...)`, plus the categories `budget_exhausted`, `context_exhausted`, `provider_error` and `no_result` with `TURN_NO_RESULT_MESSAGE`. Phase 8 cannot build the correct JSON-RPC error from a bare string. | `turn.rs:284-295`, `turn.rs:343-345`, `turn.rs:491-504`; upstream `ADP:54`, `2763-2766`, `2771-2774`, `2804-2828`, `2157` | Change to `Failed { kind: FailureKind, message: String }`, where `FailureKind` is AuthRequired, ProviderError, BudgetExhausted, ContextExhausted, NoResult or SessionEnded. Join errors with `", "`, fall back to the subtype, and use `result` for `success`. Copy the `TURN_NO_RESULT_MESSAGE` text verbatim. |
| 8 | major | **The #866 drain fires at `task_notification`, but upstream waits for the followup result or idle.** Upstream `task_notification` only deletes the registry entry (`ADP:2336-2340`). The held turn settles at the followup's autonomous result (`ADP:2689-2705`) or at idle, so the model's promised summary streams *inside* the turn. `on_task_ended` settles immediately, so the summary lands out of turn. That is the stranded-output half of #864/#866. | `turn.rs:221-224`; upstream `ADP:2336-2352`, `2689-2705`, `2090-2107` | `on_task_ended` should only remove the live entry and return nothing. The drain happens in the autonomous-result lane (finding 4) and the idle lane (finding 1). Rewrite 6.T1 so that the second `task_ended` does **not** settle, the followup result does, and an idle is the fallback. |
| 9 | major | **Cancel semantics are not the upstream ones, and the INV-30 test is vacuous.** Upstream `cancel()` does four things: (a) settles every *queued, non-active* turn `cancelled` immediately and seeds one orphan per turn, because their user messages were already pushed; (b) inline-settles a held active turn `cancelled`; (c) leaves a live active turn to settle at idle, at the echo hand-off, or at the force-cancel floor; (d) in the backstop case only, tracks the active turn as an orphan. `abandon_active` seeds an orphan for the **active** turn, which is the force-cancel lane, not cancel, and there is no queued-turn sweep. The 6.T3 test would pass with `seed_orphan` deleted: with no active turn the result is dropped anyway, and `activate` zeroes the credit before p2's result. So it never exercises a credit protecting a live turn. | `turn.rs:181-203`, `turn.rs:449-452`, `turn.rs:592-627`; upstream `ADP:1453`, `ADP:1718-1766`, `ADP:3443-3584` | Add `cancel()` → `Vec<TurnEvent>`, mirroring `ADP:3453-3584` (sweep queued turns into `Settled{Cancelled}` plus orphan credits, inline-settle a held turn, set `cancelled`). Keep `abandon_active` as `force_cancel()` for phase 11. Rewrite 6.T3 as the real INV-30 timeline: p1 active, p2 queued (pushed), cancel → p2 cancelled and credited; idle settles p1; p3 (echo-less, e.g. `/compact`) enqueued; p2's late result arrives → p3 must **not** activate or settle; p3's own result then activates and settles p3. This needs finding 2. |
| 10 | major | **No stream-end / stream-death lane.** Upstream EOF (`ADP:1771-1842`) settles the active turn: `cancelled` if a cancel is pending, otherwise its deferred outcome, otherwise the scratch outcome. It rejects every queued turn with `SESSION_ENDED_MESSAGE`. `failAllTurns` (`ADP:1646-1673`) rejects every turn except a held one, which resolves with its deferred outcome. Without these, a `claude` crash leaves every pending `session/prompt` hanging, which breaks REQ-5. | not present in `turn.rs`; upstream `ADP:1646-1673`, `1771-1842` | Add `on_stream_end()` and `fail_all(kind)` returning per-turn events. Test both with a held turn and a queued turn. |
| 11 | major | **Events carry no turn identity.** `Settled` / `Failed` do not say *which* turn. A hand-off in `activate` emits `Settled` (for the old turn) and `Activated` (for the new one) in one batch, and the cancel sweep (finding 9) settles several queued turns at once. The actor cannot map these to the correct `oneshot`. | `turn.rs:81-94`, `turn.rs:431-455` | Add `prompt_uuid: String` to `Settled`, `Failed` and `Activated`. |
| 12 | minor | **Echo hand-off ordering.** Upstream checks `cancelled` before held (`ADP:3027-3060`). `turn.rs` prefers `deferred_settle` over `Cancelled`. That path is rarely reached, because cancel inline-settles held turns (finding 9). The echo hand-off also does not owe a trailing idle when it cancels (`ADP:3046`). | `turn.rs:436-440` | Check cancelled first and add `owed_trailing_idles += 1` there (with finding 3). |
| 13 | minor | **The #453 fallback is wider than upstream.** It fires for `max_tokens` results (upstream breaks before the fallback), for non-`success` subtypes, and on settled or autonomous results (findings 4 and 5). The `!result_text.is_empty()` guard is a harmless addition. The machine also does not own `emittedAssistantText` clearing. Upstream clears it in held settles, `failActive`, the abort lane and the result `finally`, and forgetting one clear in the actor suppresses the next turn's fallback. | `turn.rs:305-320`; upstream `ADP:1588-1603`, `1628-1633`, `2779-2782`, `2877-2881` | Move the fallback inside the `success` arm after the max_tokens check. Either move the `emitted_assistant_text` flag into `TurnMachine`, with a `note_assistant_text()` setter and clears at every upstream site, or document each clear point for the actor. |
| 14 | minor | **Not ported: usage, steering, msg_lifecycle_v1, endedPerLevel sweep.** `Settled` has no `usage` (upstream `accumulatedUsage` is reset at activation and returned in every PromptResponse, including cancelled ones per #844). There is no steering lane (`steeredEchoes`/`steeredSettle`) and no `command_lifecycle` orphan map. The two-phase `endedPerLevel` sweep at activation is missing. | `turn.rs:81-94`, `turn.rs:431-455`; upstream `ADP:1346-1351`, `1366-1386`, `1406-1445`, `1537-1541`, `2107-2135` | Usage is needed for phase 8 (the PromptResponse shape): add an accumulator reset in `activate` and `usage` on `Settled`. Steering, lifecycle_v1 and endedPerLevel can wait for their own phase, but record them in the plan as known omissions so their absence is intentional. |
| 15 | minor | **Test fidelity.** Tests use `#[tokio::test(multi_thread)]` on fully synchronous code (harmless; plan 6.T6 asks for it). There are no tests for the lagging idle, the echo-less result, the autonomous result, the held-idle drain, stream end, the is_error/max_tokens grid, `deliveredAssistantText = true` suppression, or `is_local_only_command`. 6.T1 and 6.T3 assert behaviour that differs from upstream (findings 8 and 9). | `turn.rs:506-666` | Add one test per blocker and major above. Keep each test asserting the event *and* `has_active()`. |

### Question-by-question

1. **Subtype table.** The non-error mapping (`success`/`error_during_execution` → end_turn,
   `error_max_*` → max_turn_requests) is correct, and refusal is handled first on any subtype,
   which is also correct. The mappings that differ are in findings 6 and 7: max_tokens is checked
   after is_error and applied to all subtypes; /login requires `!is_error` and fires on every
   subtype; the error text uses the first error instead of the join; failure categories are
   missing. `Cancelled` is also missing as a result-time outcome: `turn.rs` drops the cancelled
   result, which is correct, but has no usage to report (#844).
2. **Echo matching: yes, it keys on the uuid.** `on_echo` finds the queued turn by
   `prompt_uuid == uuid` (`turn.rs:232`), and an unmatched echo is dropped. That matches
   `findUnsettledTurn` (`ADP:3022`, `3092-3101`). The caller must pass only `type=="user"` frames
   that carry a `uuid`. The session must also stamp that uuid on the outbound user frame, which it
   does not do today (see wiring).
3. **Subagent hold.** `on_result` routes the refusal and normal outcomes through
   `settle_or_defer` (fail paths bypass it, which matches upstream). The live set is fed through
   `on_task_started` and `on_task_ended`. The drain is wrong in two places: the idle drain
   discards its event (finding 1), and the drain fires at the notification instead of the
   followup result (finding 8).
4. **Orphan order and INV-30.** The decrement-before-promote order cannot be checked, because
   the promote step does not exist (finding 2). The test is vacuous (finding 9).
5. **#453 / #825.** The #453 trigger condition matches upstream but is too wide (finding 13).
   The #825 idle-fail matches in isolation but false-fails under a lagging idle (finding 3). The
   tests assert the weaker, isolated behaviour.
6. **Silent omissions needed before phases 7-11:** ensureActiveTurn, owedTrailingIdles,
   autonomous origin, stream end / fail-all, cancel's queued sweep, `active = None` on settle,
   usage, and turn identity in events. These are all required before phase 8. Steering,
   lifecycle_v1 and endedPerLevel can be deferred if documented.

## Verdict

**no-go.** Findings 1–4 each cause a hang or a wrong settle on ordinary real-stream timelines.
Fix them together with 5, 9 and 11 before wiring (they are interdependent). Findings 6, 7, 8
and 10 must land before phase 8 exposes stop reasons and errors to a client.

## Phase 8 wiring instructions

- **Stamp and push immediately.** Give each `Command::Prompt` a fresh uuid. Write it into the
  outbound `user` frame's `uuid` field, `enqueue(Turn::new(uuid, is_local_only))`, and send it to
  `claude` **at once**, not gated behind the active turn. Upstream pushes every prompt
  immediately. The echo, hand-off, `ensureActiveTurn` and cancel-orphan logic all assume the
  queued turns' messages are already in the SDK. Remove the phase-5 `active`/`queue`/
  `completions` fake from `session.rs:177-230`, and keep the high-water counter keyed on
  `Activated` / `Settled` events.
- **Keep a map from uuid to reply.** Resolve the matching `oneshot` for every `Settled{uuid, stop_reason, usage}`
  and reject it for every `Failed{uuid, kind, message}`. `kind` maps to the ACP error: `AuthRequired` → authRequired, otherwise internalError with `errorKind` data.
- **Route session frames in `dispatch_stream` (`session.rs:280`), in stream order:**
  - `user` with a `uuid` → `on_echo(uuid)`. An `isReplay` frame with no match is dropped. The
    turn's own echo is not forwarded to the client.
  - `result` → `on_result(frame, emitted_assistant_text)` (with `origin` read for autonomous
    results). Then clear `emitted_assistant_text` for non-autonomous results, as in the upstream
    `finally`.
  - `system/task_started` → `on_task_started(task_id, subagent_type.is_some())`.
    `system/task_notification`, and `system/task_updated` with status completed/failed/killed →
    `on_task_ended(task_id)`.
  - `system/session_state_changed` → record `last_session_state`. On `idle` → `on_idle()`.
  - `stream_event` / `assistant` → phase 7 mapper. Set `emitted_assistant_text = true` when
    top-level text is emitted (subagent chunks with `parent_tool_use_id` excluded).
- **`Command::Cancel`** → `machine.cancel()`: resolve the swept queued turns `cancelled`, then
  send `interrupt` via `control.rs`. Phase 11 arms the force-cancel floor, which calls
  `force_cancel()` (today's `abandon_active`).
- **Stream EOF / codec error** → `on_stream_end()` / `fail_all(...)`. Resolve or reject every
  returned turn, then mark the session closed so later prompts reject up front.
- **`FinalText{uuid, text}`** → emit it as `agent_message_chunk` to the client (phase 7 mapper)
  *before* the same batch's `Settled` resolves the prompt.
- **Integration tests.** Drive the actor with recorded frame sequences for these timelines:
  lagging idle after the next echo; `/context` with no echo; subagent hold → followup result;
  cancel with a queued prompt, then an echo-less next prompt; and EOF mid-turn.
