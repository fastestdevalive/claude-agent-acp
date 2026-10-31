# Progress — T2G1FIX2 (Turn 2, Group T2-G1-FIX2)

## Checklist
- [x] Baseline turn count verified: `cargo test --lib turn` (61 tests passed).
- [x] Item 5 FLAKE reproduced and resolved:
  - Cause: `test_mishandling_probe_target_stopped_at_depth_1` used `$(cat "$COUNT_FILE" 2>/dev/null || echo 0)` which required two subshell/forks, and script file was not synced before exec, while `probe_reaper_binary` in `src/process.rs` only allowed 5 spawn retries (100ms) for transient busy/lock conditions.
  - Fix: in `tests/process_transport.rs`, sync file write (`f.sync_all()`) and use pure POSIX `sh` builtins (`if [ -f "$COUNT_FILE" ]; then read -r PREV < "$COUNT_FILE"; else PREV=0; fi`) eliminating external forks; in `src/process.rs`, increase transient spawn retries to 50 (1s) to cleanly handle transient executable locks without weakening probe semantics.
  - Verified: 30 consecutive passing runs of `test_mishandling_probe_target_stopped_at_depth_1` and 5 consecutive passing runs of `tests/process_transport.rs`.
- [x] Item 4 Defect 4: Added missing `[pkg:F25]` tag to `FEATURES.md` rows U-49 and N-53 (rows only; header counts untouched).
- [x] Item 1 Defect 1: When `machine.on_result` activates a turn (result-path / echo-less activation, `session.rs` ~3211-3216), reset `ext.failure_ctx` and `*assistant_had_error` before `settle_events`.
  - Failing-first tests:
    - (a) `inv_54_defect1_echoless_activation_resets_failure_ctx`: Echo-less command failing after a rate_limited turn gets its own category/title (AIR) and no stale errorKind (non-AIR).
    - (b) `inv_54_defect1_enqueue_race_does_not_wipe_failure_ctx`: Enqueue race: prompt B queued while A is running does NOT wipe A's failure ctx.
- [x] Item 2 Defect 2: Rewrote finding-8 test (`inv_54_review_findings_actor_frame_context`) so it observes `SessionOutbound::Update` with `SessionUpdate::AgentMessageChunk`, asserts suppression for AIR+sessionFailure for both `error` and `<synthetic>` usage-limit frames, and includes control asserting non-AIR client DOES get the chunk. Deleted redundant vacuous `inv_54_usage_limit_without_error_has_no_data` in `agent.rs`. Revert-tested to guarantee non-vacuous assertions.
- [x] Item 3 Defect 3: Fixed `session.rs` title update condition to check current frame (`line.get("error").is_some() || is_limit`).
  - Failing-first test: `inv_54_defect3_error_frame_then_normal_frame_keeps_error_title`: error frame followed by normal frame followed by `is_error` result keeps error text as the title.
- [x] Acceptance verification: fmt, clippy, `--lib`, `--lib turn` (remains 61), differential x3, gate OK.

## Verification Logs

### Rust formatting & Clippy
- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check`: clean (0 diff)
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`: clean (0 warnings)

### Tests & Turn Count
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn`: 61 passed; 0 failed
- `cargo test --manifest-path rust/Cargo.toml --lib`: 601 passed; 0 failed

### Differential Tests (x3)
- Run 1: 97 passed; 0 failed; finished in 38.19s
- Run 2: 97 passed; 0 failed; finished in 33.18s
- Run 3: 97 passed; 0 failed; finished in 30.44s

### Flake Verification
- `test_mishandling_probe_target_stopped_at_depth_1`: 30 consecutive passes (0 failures)
- `cargo test --test process_transport`: 5 consecutive passes (0 failures)

### Canonical Gate (`rust-gate.sh`)
```
-- MODULE-MAP snapshot half
upstream modules: 91 | map rows: 107 | unmapped: 0 | stale: 0 | bad relationship: 0 | bad rust path: 0
MODULE-MAP CHECK: PASS
-- real-export half (/private/tmp/claude-501/par-c/rust/target/parity/src-0.94.0)
upstream tests: 2400 | mapped rows: 2400 | unmapped: 0 | stale: 0 | missing(warn): 2211 | unclassified: 2211 | broken targets: 0 | bad syntax: 0
note: 2211 missing: row(s) warn only (each needs a MISSING.md ticket)
TEST-MAP CHECK: PASS
upstream modules: 91 | map rows: 107 | unmapped: 0 | stale: 0 | bad relationship: 0 | bad rust path: 0
MODULE-MAP CHECK: PASS
PARITY: PASS
==> upstream ahead (non-failing)
upstream is 1 versions ahead of pinned 0.94.0 (latest 0.95.0)
==> gate OK
```

### Hygiene Checks
- `git diff -U0 HEAD | grep -c 'JsonPath::new'`: 0 matches
- Process hygiene (`ps -Ao pid,command | grep -E 'examples/in_process|spawn_child|--reaper' | grep -v grep`): 0 orphan processes

READY:T2G1FIX2
