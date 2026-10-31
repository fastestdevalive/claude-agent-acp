# Progress G7a-fix4 (Turn 1, deepseek implementer, worktree /private/tmp/claude-501/par-b)

Base commit: 5cace28. Authoritative review: scratchpad/review-G7afix3.md.

Checklist:
1. [x] Auth actor uses the SAME injected managed source as sessions (auth_status.rs:322-323, 383, 632); unify ServeOptions.managed_docs with session.rs:551 into ONE channel (EnvSnapshot::managed_docs); hermetic tests for inv_50, inv_26, 3x wire_transport; guard test that fails if a test-mode load touches real managed paths; correct false claim in progress-G7afix3.md.
2. [x] Malformed/oversized HKLM registry value must also SKIP the HKCU tier (process.rs:1315, 1447, 1528-1537) matching sdk.mjs; test.
3. [x] Enforce D8 "no unsafe outside windows_job": gate grep in rust/scripts/rust-gate.sh + crate-level lint on bin target; keep D8 text accurate.
4. [x] `--reaper --probe` handshake must not block the async spawn worker: spawn_blocking / block_in_place, cache result per path INCLUDING failures for a short TTL; test failed-probe caching.
5. [x] Restore production settings-watcher poll default to 100 ms as a named constant; debounce test keeps its own short interval.

## Summary of Changes
- `rust/claude-agent-acp-rs/src/auth_status.rs`: Replaced unparameterized `read_api_key_helper` with `read_api_key_helper_with_env(cwd, env)`; threaded `env: EnvSnapshot` into `spawn_auth_actor`, `probe_cli`, and `SessionCreated` handler. Armed `ManagedSourceGuard` in `inv_50_actor_session_account_and_cwd_reprobe`.
- `rust/claude-agent-acp-rs/src/agent.rs`: Removed redundant `managed_docs` from `ServeOptions`, unifying injection into `EnvSnapshot::managed_docs` which is already used by `session.rs` ~551 and passed into `spawn_auth`.
- `rust/claude-agent-acp-rs/src/process.rs`:
  - Added `ManagedSourceGuard` test hook to detect and fail tests touching host managed-policy paths.
  - Windows registry HKLM/HKCU parity: implemented `classify_hklm_value`, `hkcu_tier_applies`, and `OsSourceRead::DidNotLoad` so malformed or oversized HKLM values suppress the HKCU tier per `sdk.mjs Tx`.
  - Added unit tests: `hklm_did_not_load_suppresses_hkcu_tier` and `classify_hklm_value_matches_sdk_did_not_load`.
  - Reaper probe non-blocking execution: wrapped thread join in `tokio::task::block_in_place` on multi-threaded runtime. Added `PROBED_REAPER_FAILURES` cache with `PROBE_FAILURE_TTL` (30s) so hanging/failed probes are cached and do not incur 2 s delays on subsequent spawns.
- `rust/claude-agent-acp-rs/src/settings_manager.rs`: Restored `DEFAULT_WATCHER_POLL_INTERVAL` to 100 ms as a named constant while keeping the debounce test's injected 20 ms interval.
- `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs`: Added `#![forbid(unsafe_code)]` crate-level lint.
- `rust/scripts/rust-gate.sh`: Added gate check verifying no `unsafe` outside `src/process.rs`, exactly one `#[allow(unsafe_code)]` exclusively on `windows_job`, and no `unsafe` outside `windows_job`.
- `porting/parity/DECISIONS-FOR-USER.md`: Updated D8 to describe the gate check enforcement.
- `porting/parity/progress-G7afix3.md`: Corrected the false claim regarding host managed paths and noted the non-blocking probe fix.
- `rust/claude-agent-acp-rs/tests/managed_sources_hermetic.rs`: Added hermeticity test exercising `EnvSnapshot::empty()`, fake injected managed docs, auth status resolution, and verification that direct host reads panic under `ManagedSourceGuard`.
- `rust/claude-agent-acp-rs/tests/process_transport.rs`: Added assertion verifying failed/timed-out probe outcome is cached and resolves in <200 ms.
- `rust/claude-agent-acp-rs/tests/transport_parity.rs` & `tests/wire_transport.rs`: Armed `ManagedSourceGuard` in `inv_26_transport_parity` and wire transport tests.

## Acceptance Results
- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check`: PASS
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`: PASS (clean, 0 warnings)
- `cargo clippy --manifest-path rust/Cargo.toml --target x86_64-pc-windows-gnu --all-targets -- -D warnings`: PASS (clean, 0 warnings)
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`: PASS (584 passed, 0 failed)
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential`: PASS (95 passed, 0 failed)
- Reaper tests x10 (`cargo test -p claude-agent-acp-rs --test process_transport` x10): PASS (10/10 runs passed)
- Five named tests + guard test with clean HOME:
  - `managed_sources_hermetic`: PASS
  - `inv_50_actor_session_account_and_cwd_reprobe`: PASS
  - `inv_26_transport_parity`: PASS
  - `wire_permission_request_id_is_numeric`: PASS
  - `wire_late_cancel_request_is_ignored`: PASS
  - `wire_cancel_request_cancels_the_prompt`: PASS
- `cargo test --manifest-path rust/Cargo.toml --workspace`: PASS
- `bash rust/scripts/rust-gate.sh`: PASS (`==> gate OK`, including D8 unsafe check)
- JsonPath guard check (`git diff -U0 455c146 -- ... | grep -c 'JsonPath::new'`): 0
- `bash rust/scripts/parity/test-map-check`: PASS
- `grep -rEn 'cfg!?\(.*(unix|windows|target_)' ... | grep -v 'src/process.rs:'`: prints nothing
- Cross-target `cargo check` (`x86_64-pc-windows-gnu`, `aarch64-apple-darwin`, `x86_64-unknown-linux-musl`): ALL PASS

READY:G7afix4
