# T7 audit (read-only; head 091cce2; `git diff c8c19dd 091cce2 -- rust/`)
Read CLAUDE.md, STATUS TURN 7, upstream v0.94.0 (Node authoritative). Sampled >20 of the 49 changed DONE
rows (incl. B-39/B-07/B-114/B-74/Q-06/N-35/U-88/U-75/T-104/U-87/U-104/T-102/Q-89/M-74/Q-48), the
7-scenario race suite, lane A's session/turn_events/agent changes, and `nfc` against Node 26 (all singles,
every `"a"+cp` pair, 200k random sequences; e+U+0301, Hangul jamo, U+212B are correct).
## HIGH
- H1 — v2 file-change report drops a cancel during the preview. `settle_events` builds the v2 idle from the
  already-settled `end_turn` outcome (`turn_events.rs:712-723`), takes the emissions, and the report task
  (`session.rs:11091-11127`) runs `run_report` but never reads `job.cancelled` (v1 does, `11056-11058`).
  Scenario: v2 prompt with `agentFileChangeReportRequest`, result end_turn, 600 ms preview, `session/cancel`
  at 250 ms → `unavailable/cancelled` report + `idle stopReason=end_turn`; upstream resolves the settling
  turn `cancelled` immediately (`acp-agent.ts:8627-8685`, `turn.resolve`→`events.ended`, `3932-3937`).
  `conc_race_report_cancel.rs` is v1-only, so this is untested.
- H2 — v2 AIR `sessionFailure` loses its failure payload/bookkeeping. The F5 path (`session.rs:10256-10264`)
  sets `set_failure_rpc(None)`, but `failures.prepare`/`record_active` and `Board(TurnFailed)` remain inside
  `if let Some(reply) = pending.remove(...)` (`10284-10297`), never true for v2. The idle comes from
  `settle_outcome`, whose `_meta` has only `quota` (`turn_events.rs:764-775`, `832-875`); upstream records the
  failure and passes `sessionFailureMeta` into the idle (`acp-agent.ts:5591-5593`). Scenario: v2 AIR provider
  error → idle end_turn without `_meta.jetbrains.air.sessionFailure`; index row stays non-error.
  `t7a_f5_...` asserts only `stopReason`.
- H3 — `nfc` combining classes are Unicode 13 while decompositions are Unicode 16 (`nfc_data.rs:3059`,
  header 1-5; `nfc.rs:23-40`). 28 post-13 marks are treated as starters (U+0899, U+08CF..U+08D2, U+1AC3,
  U+1AC4, U+1ACA, U+1ADD, U+1AE6, U+1AEB, U+1DFA, U+10EFA.., U+113CE.., U+11F41/42, U+1612F):
  `nfc("a\u{1AC3}\u{301}")` = `a\u{1AC3}\u{301}` where Node gives `\u{e1}\u{1AC3}`. On darwin
  `encode_project_path` then sanitizes a different name, so `projectDirsOf` misses a session whose cwd holds
  such a mark. M-74/Q-48 are DONE on tests that only exercise common compositions.
## MEDIUM
- M4 — B-17's evidence does not prove the actor wiring. `t7a_b17_...` (`tests_t7a.rs:169`) drains through a
  local `drain()` that calls the yielder itself; `run_loop`'s `select!` is `biased` with commands first
  (`session.rs:3356`), so `t7a_b63_...` (`tests_t7a.rs:96-138`, `before < 1000`) passes with the yielder
  removed (cancel handled after ≤1 frame). "The loop-level test fails without the pause" holds only for the
  standalone yielder test.
- M5 — race scenario 4 cannot fail on its own claim: `if let Ok(Ok(reply)) = settle(...)`
  (`tests_t7a_race_s4.rs:170`) swallows `Err` (timeout/dropped) and `Ok(Err(failure))`, so "the prompt still
  settles, never hangs" is not asserted.
- M6 — a cancel during the v1 report preview delays the prompt response up to the 2 s preview timeout.
  `cancel_collecting` only sets the flag and publishes (`session.rs:2784-2791`); the deferred task still awaits
  `run_report` before replying (`10985-11063`). Scenario: 600 ms preview, cancel at 250 ms → response ~600 ms;
  upstream resolves the settling turn immediately (`acp-agent.ts:8627-8685`). The v2 path has the same delay.
- M7 — held v2 emissions can be overtaken: `settle_events` takes `map_state.v2.take_out()` and emits the idle
  from a spawned task (`session.rs:11078-11130`); the actor can process the next prompt/stream frame and emit
  its `user_message` before the previous turn's `idle`. Node awaits the preview inside `settleActive`.
- M8 — `collecting` leaks on cancelled/failed reports: the cancelled branch (`session.rs:10933-10936`) and
  Failed loop (`10953-10956`) call `finish` but never store `job.done`; `note_prompt`'s prune drops only done
  entries (`2696-2699`), so one entry per cancelled/failed reported turn accumulates until a cancel/close.
- M9 — stale PARTIAL notes: B-07 names `closeCycleToolCalls` as missing, but it exists and is wired on
  `CycleEnded` (`unfinished.rs:213,322`); N-35 names `isStaleEnd`/`finishTask`/`discardPending` as absent, but
  they run on `task_notification` (`native_subagents/runtime.rs:1161,1179,683`; `async_tasks/subagents.rs:77-100`;
  `session.rs:8389`). Both should be re-assessed for DONE.
## LOW
- L10 — `TrackedToolCall::record_meta` compares `Value::to_string()` (`map.rs:905-931`) where Node uses
  `JSON.stringify`; integral floats (`1.0` vs `1`) differ, so a repeated meta can emit a spurious update.
- L11 — hook `_meta.claudeCode` key order: Rust writes `toolName` then `toolResponse` (`hooks.rs:563-573`);
  Node writes `toolResponse` then `toolName` (`renderer.ts:560-574`). Value-equal tests do not catch it.
- L12 — T-102's cited evidence (`tests/t7b_v2_terminal.rs:235`) is the T-104 test: announcement/output/exit
  only, not `terminal_output_delta`→chunk (U-104, `:336`) or forget-on-close (`v2/terminal.rs:189`, no test).
- L13 — two deferred v1 report tasks in one batch run concurrently and release their stale ack early
  (`session.rs:11000-11010`), so responses can leave in report-completion order, not settle order; Node awaits
  the reports sequentially in `settleActive`.
No deadlock found: `rx` is resolved before the spawn, the report timeout bounds the preview, and no actor path awaits client work.
