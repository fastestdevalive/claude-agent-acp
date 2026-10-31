# Progress: Turn 2, Group T2-G1 (AG-2: sessionFailure + AIR failure policy)

- [x] Baseline recorded: `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn` (60 passed)
- [x] 1. Pure `session_failure.rs`:
  - [x] 13 ClaudeFailureKind policy table (`AIR_FAILURE_POLICY`)
  - [x] `session_failure_meta` formatting
  - [x] `provider_failure_category`
  - [x] `USAGE_LIMIT_ERROR_PREFIXES` + `is_synthetic_usage_limit_message` + assistant text extractor
  - [x] `inv_54_*` unit tests
- [x] 2. `AssistantFailureCtx`:
  - [x] Define `AssistantFailureCtx { error: Option<String>, was_usage_limit: bool, title: Option<String> }`
  - [x] Replace `assistant_had_error: bool` in `SessionExt` with `AssistantFailureCtx`
  - [x] Update `session.rs` where assistant frame arrives to record `error`, `was_usage_limit`, `title`
  - [x] Reset failure ctx at activation / turn scratch
- [x] 3. `TurnEvent::Failed` gains `usage: Usage`:
  - [x] Update `TurnEvent::Failed` definition
  - [x] Update 4 production sites in `turn.rs`
  - [x] Update `unfinished.rs` conversion
  - [x] Mechanical updates to `turn/turn_tests.rs`
  - [x] Re-run `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn` (count >= 60, no failures)
- [x] 4. Error mapping and `handle_events`:
  - [x] `auth_required` JSON-RPC error code becomes -32000 "Authentication required" (`inv_54_auth_required_code`)
  - [x] When `air.supports("sessionFailure")` and kind != `AuthRequired` / `SessionEnded`:
    answer `end_turn` + `_meta.quota` (with model rows) + `_meta.jetbrains.air.sessionFailure`
  - [x] Otherwise unchanged -32603
- [x] 5. Capability C-30 advertised for AIR clients in initialize
- [x] 6. Scenarios and differential test:
  - [x] Author corpus scenarios: `auth-required`, `session-failure-unfinished`, `session-failure-provider-error`
  - [x] Run `bash porting/capture.sh` to produce fixtures
  - [x] Add `inv_24_session_failure` in `tests/differential.rs`
  - [x] Usage-limit replay (N-53) unit/actor test
- [x] 7. Parity rows flipped in `porting/parity/FEATURES.md` (U-49, C-30, N-53, V-13, Q-39)
- [x] 8. Final verification & gates:
  - [x] `cargo fmt --manifest-path rust/Cargo.toml --all --check`
  - [x] `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`
  - [x] `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib` (597 passed, 0 failed)
  - [x] `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn` (baseline 60 -> 61 passed, 0 failed)
  - [x] Differential test x3 (`inv_24_session_failure` passed 3/3 runs)
  - [x] JsonPath guard check (`git diff -U0 455c146 -- ... | grep -c 'JsonPath::new'` -> 0)
  - [x] `bash rust/scripts/rust-gate.sh` (ends `==> gate OK`)
  - [x] Process hygiene check (`ps -Ao pid,command | grep -E ...` -> clean, 0 processes)

## Gate Output (last 12 lines)
```
==> cross-target check: aarch64-apple-darwin
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.87s
==> G9: cfg confined to process.rs
==> D8: unsafe confined to process.rs windows_job
D8 OK: unsafe strictly confined to process.rs windows_job
==> G10: dependency pin (agent-client-protocol 2.1.0)
==> parity maps (TEST-MAP + MODULE-MAP)
...
MODULE-MAP CHECK: PASS
PARITY: PASS
==> upstream ahead (non-failing)
upstream is 1 versions ahead of pinned 0.94.0 (latest 0.95.0)
==> gate OK
```

## Status: READY:T2G1
Turn 2, Group T2-G1 (AG-2) parity work completed cleanly. All 5 parity rows flipped in `FEATURES.md`. Full gate passed. Zero leftover processes.
