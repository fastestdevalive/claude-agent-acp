# T1-G11b — AG-1 (`_session/goal`, /goal interception, active_goal) progress

Group: G11b (rust-air-gaps Phase 3 item 3.1, Decision 16, Contract C).
Worktree: `/private/tmp/claude-501/par-b` at d3bc560. Rows: M-93, U-07, C-14, S-21, N-15, N-52, Q-16 (+ WB-14 note).

## Checklist

- [x] Read work order, plan (Phase 3.1 / Decision 16 / Contract C), upstream `goal-extension.ts`,
      `acp-agent.ts` goal sites (2470-2563, 3952-3954, 4199-4283, 4387, 5935-5942, 6853-6870, 7728-7730),
      `src/tests/acp-agent.test.ts` goal tests, oracle `dist/goal-extension.js`.
- [x] `src/goal.rs` (new) pure module + `inv_53_*` units (parse errors, prompt interception,
      snapshot mapping, optimistic publish/pending, stale-runtime suppression, rollback).
- [x] `lib.rs` `pub mod goal;`; `SessionExt.goal` field.
- [x] Actor hooks (`session.rs`): Prompt/Steer optimistic publish, user-echo `started`,
      result rollback, `active_goal` frame handling.
- [x] `agent.rs` `_session/goal` arm (`{}`, -32602 messages, -32603 unknown session).
- [x] Corpus `goal-set-clear` (Contract C) + `bash porting/capture.sh goal-set-clear`.
- [x] `inv_24_goal` in `tests/differential.rs`; wire test `inv_53_goal_params_wire` in `tests/agent.rs`.
- [x] FEATURES rows M-93/U-07/C-14/S-21/N-15/N-52/Q-16 flipped with evidence; WB-14 note updated;
      TEST-MAP rows ported.
- [x] Local gate: fmt, clippy -D warnings, `--lib`, `--lib turn` (60), `--lib inv_53`, differential,
      workspace, `rust-gate.sh` gate OK, JsonPath guard 0, test-map-check PASS, hazard diff empty.

## Files changed

- `rust/claude-agent-acp-rs/src/goal.rs` (new): constants, `GoalSnapshot` (+`to_json`,
  `from_active_goal`), `PromptGoal`/`goal_update_from_prompt`, `GoalRequest`/`parse_goal_request`/
  `command_text`, `goal_update_value`/`goal_publish`, `GoalState` (`note_prompt`, `note_started`,
  `note_runtime`, `note_result`), `first_prompt_text`; 12 `inv_53_*` units.
- `src/lib.rs`: `pub mod goal;`.
- `src/session_ext.rs`: `SessionExt.goal: GoalState`.
- `src/session.rs`: Prompt arm optimistic publish; Steer arm (injected + startedNewTurn) publish;
  user-echo `note_started`; result-arm rollback (`is_error || refusal`, started only);
  `active_goal` handled in `dispatch_stream` before routing; `publish_goal` helper.
- `src/agent.rs`: `_session/goal` arm (parse -> steer(promptRequired) -> prompt when idle ->
  `{}` behind the prompt's flush); AIR goal capability already advertised (unchanged).
- `tests/differential.rs`: `inv_24_goal`.
- `tests/agent.rs`: `inv_53_goal_params_wire` (-32602 texts, no data; unknown session -32603/details).
- `porting/corpus/goal-set-clear.{acp.json,transcript.jsonl}` + captured
  `porting/fixtures/goal-set-clear.{frames,argv}.json` (Node oracle, Contract C).
- `porting/parity/FEATURES.md` (7 rows DONE + WB-14 closed), `porting/parity/TEST-MAP.md`
  (10 upstream goal cases mapped), this file.

## Deviations (Decision 16 / plan notes)

- `active_goal` is handled in `session.rs::dispatch_stream` before `dispatch::route` instead of
  adding it to `dispatch.rs`: Node handles this frame before its exhaustive switch
  (`acp-agent.ts:5935-5942`), and `dispatch.rs` is outside this group's file scope. No
  dispatch.rs change; the frame still reaches the raw `_claude/sdkMessage` forwarder first.
- `GoalSnapshot` keeps `iterations`, `last_reason` and `set_at` as verbatim
  `serde_json::Value`s, because Node copies them without conversion. Contract C's `set_at` is a
  timestamp string; both normalisers (`capture.sh` and `tests/common`) map it to a `$N`
  placeholder, so the fixture and the Rust frames compare equal.
- Unknown session: Node's `goal()` reaches `steer()`, which throws `new Error("Session not
  found")` -> SDK `internalError` with `data:{details}`. The arm returns exactly that
  (verified against the Node oracle with a scratch script). The older `_session/steering` arm's
  string `data` shape is pre-existing and outside this group.
- `PromptGoal::Set` boxes the snapshot (clippy `large_enum_variant`; no `#[allow]`).
- No AIR-less corpus variant: the plan lists none. The AIR-only gate is unit-tested
  (`inv_53_goal_publish_air_only`); the AIR corpus is Contract C.
- TEST-MAP "cancels the running turn without clearing the persistent goal" is `different:`
  (the Rust cancel path never writes goal state; parity is structural, upstream asserts absence).
- No edits to `turn.rs`, `turn/turn_tests.rs`, `unfinished.rs`, `dispatch.rs`; hazard diff empty.

## Commands (real output)

- `cargo fmt --manifest-path rust/Cargo.toml --all` + `--check` -> clean.
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` -> clean.
- `cargo test ... -p claude-agent-acp-rs --lib` -> `test result: ok. 539 passed`.
- `cargo test ... -p claude-agent-acp-rs --lib turn` -> `test result: ok. 60 passed`.
- `cargo test ... -p claude-agent-acp-rs --lib inv_53` -> `test result: ok. 12 passed`.
- `cargo test ... -p claude-agent-acp-rs --test differential` -> `ok. 90 passed` (3 runs, all green).
- `cargo test ... --workspace` -> every suite `test result: ok`.
- `bash rust/scripts/rust-gate.sh` (UPSTREAM_CACHE_DIR=main-tree cache) -> `==> gate OK`.
- `bash rust/scripts/parity/test-map-check` -> `TEST-MAP CHECK: PASS`.
- JsonPath guard -> `0`; G7 diff empty; hazard-file diff empty.
- `grep -c '"goal"' porting/fixtures/goal-set-clear.frames.jsonl` -> `5` (>= 4).

READY:G11b
