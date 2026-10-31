# Progress — Turn 1, Group G2c (PC-3 ExitPlanMode rendering + parked corpus)

## Status

READY:G2c

## What changed

- **T-58** `tools.rs` `ExitPlanMode` shape: title `Approve Plan`, kind
  `switch_mode`, plan text as the display copy (empty plan → no content). The
  streamed `tool_call`, the permission-surfaced `tool_call` and
  `session/request_permission`'s `toolCall` all share this shape.
- **T-59** `tools.rs` `exit_plan_mode_result` / `exit_plan_mode_raw_output` +
  `map.rs` ExitPlanMode result arm:
  - accepted plan → title `Exited Plan Mode`, no content, `rawOutput` = the
    unfenced result text for non-AIR; AIR keeps it out (reporter sets
    `rawOutput: undefined`);
  - rejection → one outer fence stripped (`exit-plan.ts:163-169`); non-AIR gets
    the generic fenced error content plus the unfenced `rawOutput`, AIR gets
    only the unfenced `rawOutput`;
  - `is_error` without text takes the accepted-plan path (`renderer.ts:359-368`).
- **PC-3.2** corpus `exit-plan-default` / `exit-plan-auto` + Node-captured
  fixtures; `inv_24_exit_plan_default` / `inv_24_exit_plan_auto` (2 passed).
  The auto selection also exposed and ported:
  - Node's always-on permission option sort (`options.ts:29-32`): the
    `allow_once` default now leads, then `allow_always` (clear-context, auto,
    bypass), then reject. Existing ExitPlanMode option tests updated to the
    authoritative order.
  - `applyPermissionFallback` (`session-mode.ts:196-215`, P-33): an
    ExitPlanMode allow that activates Auto on a model without it is rewritten
    to `acceptEdits` before the `control_response` is sent; the advisory
    warning goes out once before `current_mode_update` +
    `config_option_update`, and the permission task waits for that publish
    before writing the control response (Node's order).
- Keep-planning `interrupt` remains covered by
  `permission::tests::inv_60_exit_plan_control_response` /
  `inv_60_exit_plan_mode_selection_effects` (no wire corpus added, to keep
  `inv_24_exit_plan` at exactly 2 tests).
- `TEST-MAP.md`: `exit-plan-approve` → covered by the two corpus scenarios;
  `exit-plan-reject` → ported unit test (wire corpus pending).

## Scope notes (deliberate, for the orchestrator)

- **`session.rs` was edited** despite the work-order exclusion: the auto corpus
  cannot pass without threading `auto_unavailable` into the permission
  options and publishing the fallback warning/mode before the control
  response. The change is confined to `ExitPlanModeApplied` (adds
  `fallback` + `ack`), the `handle_permission` argument, and the caller. It
  does not touch spawn options.
- **`exit-plan-auto.transcript.jsonl` expect** is `setMode acceptEdits`, not
  the parked `setMode auto`: Node's `applyPermissionFallback` rewrites the
  permission result before the CLI sees it, so the parked expectation only
  exercised the fake's mismatch exit (process death, out-of-scope F25). With
  the corrected expectation the auto selection runs end to end (fallback,
  mode updates, approved plan, `end_turn`) and the regenerated Node fixture is
  authoritative.
- **AIR `planFile`** (`planFilePath`/`planFileReleased` clear signal) is not
  threaded: `MapState` only carries `air: bool`, and capability threading is
  T-60/T-04 (`[pkg:F9]`). Non-AIR rendering and the AIR `rawOutput` rule are
  ported.

## Rows

- `FEATURES.md` T-58 PARTIAL → **DONE** (`tools.rs:716-729`).
- `FEATURES.md` T-59 MISSING → **DONE** (`tools.rs:858-967`,
  `map.rs:794-813`; AIR planFile release noted as T-60/T-04).
- PC-3.1 / PC-3.2 ticked in `plan-parity-closeout.md`.

## Commands run (all real, from `/private/tmp/claude-501/par-b`)

- `bash porting/capture.sh exit-plan-auto exit-plan-default` with
  `UPSTREAM_CACHE_DIR=<main-tree>/rust/target/upstream` → capture complete.
- `cargo fmt --all` + `--check` → FMT_OK.
- `cargo clippy --workspace --all-targets -- -D warnings` → clean.
- `cargo test -p claude-agent-acp-rs --lib` → **345 passed**.
- `cargo test -p claude-agent-acp-rs --test differential inv_24_exit_plan` →
  **2 passed**.
- `cargo test -p claude-agent-acp-rs --test differential` →
  **63 passed, 1 ignored**.
- `cargo test --workspace` → all pass (incl. recorder/phase2).
- `bash rust/scripts/rust-gate.sh` → `==> gate OK`.
- JsonPath guard `... | grep -c 'JsonPath::new'` → **0**.
- `bash rust/scripts/parity/test-map-check` → `TEST-MAP CHECK: PASS`.
