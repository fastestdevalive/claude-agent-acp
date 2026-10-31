# G12 — ROADMAP F42 (hermetic fixtures) + known flakes + two test-strengthening items

Worktree: /private/tmp/claude-501/par-c @ 2fe7191 (feat/rust-port). Group G12.
No production behaviour changes; tests/infra only.

## Checklist

- [x] F42: remove/replace 150 ms `sleep_ms` pacing in `porting/corpus/permission-*`
      (and `sdkctl-duplicate-request`) with ordered recorder/fake-claude waits;
      keep a sleep only where Node's own capture race needs it (documented).
- [x] Flake A: `printf | grep -q` under `pipefail` in parity scripts (SIGPIPE 141)
      -> here-string / case match; `--selftest` 30x PASS.
- [x] Flake B: `sdkctl-duplicate-request` deterministic (ordered wait, no longer sleeps);
      differential 20x green under CPU load.
- [x] Test strength 4a: ExitPlanMode cancel race run-loop test (answer permission then
      Cancel; mode/state updates + CLI still receives allow).
- [x] Test strength 4b: Auto->acceptEdits fallback on a non-ExitPlanMode tool publishes
      the warning exactly once (session-level test).
- [x] fmt / clippy / --lib / 20x differential / 30x selftest / workspace /
      `rust-gate.sh` OK / JsonPath guard 0 / test-map-check PASS.
- [x] READY:G12 note.

## Notes / decisions

1. F42 hermetic fixtures:
   - Removed 150ms `sleep_ms` pacing from `permission-allow`, `permission-deny`, `permission-edit-always`, `permission-generic-tool`, `permission-suggestions`, `permission-webfetch`, and `sdkctl-duplicate-request`.
   - Kept documented `sleep_ms` in `permission-deny-edit.transcript.jsonl` where Node oracle race emits tool_call before text chunk without pause.
   - Added `Step::WaitRecv { contains }` in `rust/acp-recorder/src/recorder.rs` and `rust/acp-recorder/src/script.rs` to allow ordered wire-condition synchronization without pacing sleeps.
   - Updated `rust/fake-claude/src/lib.rs` to write non-marker frames of each step in a single `write_all` chunk, ensuring host parses all step frames before dispatching.
2. Flake A fix:
   - Fixed `printf '%s\n' ... | grep -q` under pipefail in `rust/scripts/parity/test-map-check` and `module-map-check` using here-strings `<<< "$var"`.
   - Verified with 30 consecutive iterations of `bash rust/scripts/parity/test-map-check --selftest`: 30x `SELFTEST: PASS`.
3. Flake B fix:
   - `sdkctl-duplicate-request.acp.json` converted from sleep to `wait_recv: tool_call_update`, and transcript frames batched in one write in fake-claude.
   - Verified 20/20 PASS in loop under parallel CPU load.
4. Test strengthening:
   - Added `session::tests::exit_plan_answer_survives_cancel_in_the_run_loop` in `src/session.rs` for 4a (ExitPlanMode permission answer survives cancel in real run loop).
   - Added `session::tests::fallback_warning_is_published_once_per_session` in `src/session.rs` for 4b (warning published at most once per session for Auto->acceptEdits fallback rewrite).
   - Added `session::tests::non_exit_plan_mode_setmode_auto_is_not_offered` guard test.

## Acceptance commands & results

- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check`: PASS
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`: PASS (0 warnings)
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`: PASS (498 passed)
- 20x differential test loop: PASS (20x ok, 86 passed each, 0 failures)
- 30x selftest loop (`test-map-check --selftest`): PASS (30x `SELFTEST: PASS`)
- `sdkctl-duplicate-request` 20x under CPU load: PASS (20x ok)
- `cargo test --manifest-path rust/Cargo.toml --workspace`: PASS (all workspace tests passed)
- `JsonPath::new` guard: 0 new occurrences
- `bash rust/scripts/parity/test-map-check`: PASS
- `bash rust/scripts/rust-gate.sh`: PASS (`==> gate OK`)

READY:G12
