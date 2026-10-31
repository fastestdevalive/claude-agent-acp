# Progress — T2G1FIX (Turn 2, Group T2-G1-FIX)

## Checklist
- [x] Record baseline `cargo test --lib turn` count (61 tests passed).
- [x] Fix Finding 4: remove enqueue-time `failure_ctx.reset()` in `session.rs` (lines ~1301, ~1598); retain reset on activation (~3015).
- [x] Fix Finding 5: only strict top-level assistant frames feed failure ctx in `session.rs`.
- [x] Fix Finding 6 & 9: `was_usage_limit` re-evaluates per top-level assistant frame; `has_error()` checks `error.is_some()`; limit without `error` does not set `assistant_had_error`.
- [x] Fix Finding 8: AIR clients skip chat mapping for assistant frame with `error` or synthetic usage limit (`session.rs`).
- [x] Fix Finding 3: assistant `error` mapping to `AuthRequired` rejects with `RequestError.authRequired` (-32000) for all clients (including AIR).
- [x] Fix Finding 7: `TurnEvent::Failed` carries `model_rows` so AIR failure responses carry per-model `_meta.quota.model_usage`.
- [x] Fix Finding 10: `error_max_structured_output_retries` hardcodes `provider_error` and `title = errors.join || subtype`.
- [x] Fix `active_usage_limit_message` uuid source to match upstream transcript entry `entry["uuid"]`, add comment for F25, add unit test.
- [x] FEATURES.md honesty: flip U-49, Q-39, N-53 to PARTIAL with detailed evidence of what is done vs what is owned by F25. Keep V-13 and C-30 DONE. Header counts untouched.
- [x] Add failing-first unit tests for each finding before/along with implementation.
- [x] Verify `cargo test -p claude-agent-acp-rs --lib turn` count does not drop (baseline: 61 passed; post-edit: 61 passed).
- [x] Run full test suite, clippy, fmt, and gate checks.

## Verification & Acceptance
- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check`: clean (exit code 0)
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`: clean (exit code 0)
- `cargo test --manifest-path rust/Cargo.toml --lib`: 600 tests passed
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn`: 61 passed (baseline held)
- `differential x3`:
  - Run 1: 97 passed in 29.55s
  - Run 2: 97 passed in 29.47s
  - Run 3: 97 passed in 29.11s
- `cargo test --manifest-path rust/Cargo.toml --workspace`: all workspace tests passed
- JsonPath guard (`grep -rn "JsonPath" ... | wc -l`): 0
- Parity map check (`bash rust/scripts/parity/test-map-check --map`): PASS
- Process hygiene check (`ps -Ao pid,command | grep -E 'examples/in_process|spawn_child|--reaper' | grep -v grep`): clean (0 processes)

## Gate Run Output (last 16 lines)
```
==> parity maps (TEST-MAP + MODULE-MAP)
-- TEST-MAP snapshot half
upstream tests: 2400 | mapped rows: 2400 | unmapped: 0 | stale: 0 | missing(warn): 2211 | unclassified: 2211 | broken targets: 0 | bad syntax: 0
note: 2211 missing: row(s) warn only (each needs a MISSING.md ticket)
TEST-MAP CHECK: PASS
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

READY:T2G1FIX
