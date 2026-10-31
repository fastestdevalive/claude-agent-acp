# G7a — F5a (settings manager and managed tiers) + F41 (process-group reaper)

Worktree: `/private/tmp/claude-501/par-c` (HEAD c28eb46)
Status: **READY**

## Scope & Plan
- [x] **Part 1 — F5a Settings Manager & Managed Tiers**:
  - [x] Array merge behavior in `settings.rs` (concatenated and deduplicated in source order, except `fallbackModel` which replaces) per SDK `resolveSettings` (O-57).
  - [x] Managed tiers parsing in `process.rs`:
    - [x] Managed settings file and `managed-settings.d/*.json` directory.
    - [x] macOS MDM plist (`/Library/Managed Preferences/<user>/com.anthropic.claudecode.plist` and device-level) converted via `plutil -convert json -o - -- <path>`.
    - [x] Windows registry (`HKLM\SOFTWARE\Policies\ClaudeCode` and `HKCU\SOFTWARE\Policies\ClaudeCode` value `Settings`) queried via `reg query` or registry parser behind `cfg(windows)`. Pure parsing logic unit-tested in `process.rs`.
  - [x] Argv `--managed-settings` handling in `sdk_options.rs` / `process.rs` (O-122).
  - [x] `SettingsManager` in `settings_manager.rs`:
    - [x] Polling/file watcher with 100 ms debounce over user/project/local/managed paths (O-60).
    - [x] `onChange` callback and `setCwd`.
    - [x] `getApiKeyHelper(settingsOverride)` precedence: managed > `--settings` override > user/project/local (O-61, O-67).
    - [x] `getManagedDeniedModels()` and `getSettings()`.
  - [x] `agent.rs` wiring:
    - [x] Settings manager initialized for sessions with `cwd`.
    - [x] Expose effective `apiKeyHelper` for auth probe.
- [x] **Part 2 — F41 Process-group Reaper**:
  - [x] Platform process-group supervisor in `process.rs`:
    - [x] Unix (Linux/macOS): re-exec'd supervisor (`--reaper`, poll on parent pid via pipe EOF & getppid check) that kills the process group with `SIGKILL` on host exit (PR_SET_PDEATHSIG avoided due to parent-thread semantics on tokio workers).
    - [x] Windows: Job Object with kill on job close (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
    - [x] Guard G9: all platform code strictly in `process.rs`.
  - [x] Binary entry point in `bin/claude-agent-acp-rs.rs`: `--reaper <parent_pid> <target_pgid>`.
  - [x] Unit & acceptance tests:
    - [x] `inv_28_host_sigkill_grandchild_reaped`: SIGKILL host with fake `claude` spawning a `sleep` grandchild; grandchild reaped (ESRCH) within 2 s on macOS.
- [x] **Acceptance & Validation**:
  - [x] `cargo fmt --manifest-path rust/Cargo.toml --all` (passed)
  - [x] `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` (passed, 0 warnings)
  - [x] `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib` (535 passed, 0 failed)
  - [x] Acceptance tests in `process_transport.rs` (7 passed including `inv_28_host_sigkill_grandchild_reaped` and `inv_122_managed_settings_argv`)
  - [x] Flip rows in `porting/parity/FEATURES.md`: O-57, O-60, O-61, O-67, O-122, Q-20, Q-59
  - [x] Update `porting/parity/TEST-MAP.md` (passed `test-map-check`)
  - [x] Run `bash rust/scripts/rust-gate.sh` (passed, gate OK)
  - [x] Cross-target checks: `x86_64-pc-windows-gnu` and `aarch64-apple-darwin` passed
  - [x] Platform guard G9: `cfg(unix)|cfg(windows)|target_os` confined strictly to `process.rs`
  - [x] Set `READY:G7a` in progress file

## Files Changed
- `rust/claude-agent-acp-rs/src/settings.rs`: array merge with `fallbackModel` replace, managed tiers reading, `get_api_key_helper`.
- `rust/claude-agent-acp-rs/src/process.rs`: managed settings path/dir/mdm sources, Windows registry parser, `--managed-settings` argv builder, reaper supervisor (`--reaper`).
- `rust/claude-agent-acp-rs/src/sdk_options.rs`: `managed_settings_arg` field & parsing in `ClaudeOptions`.
- `rust/claude-agent-acp-rs/src/settings_manager.rs`: `SettingsManager` with debounced watcher, `onChange`, `setCwd`, and accessors using watch/mpsc channels.
- `rust/claude-agent-acp-rs/src/lib.rs`: export `pub mod settings_manager;`.
- `rust/claude-agent-acp-rs/src/agent.rs`: register and manage `SettingsManager` instances for sessions, wire `managed_settings_arg` in `session_spawn_options`.
- `rust/claude-agent-acp-rs/src/auth_status.rs`: use `Settings::get_api_key_helper`.
- `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs`: `--reaper` entry point.
- `rust/claude-agent-acp-rs/tests/process_transport.rs`: `inv_28_host_sigkill_grandchild_reaped` and `inv_122_managed_settings_argv`.
- `porting/parity/FEATURES.md`: flipped O-57, O-60, O-61, O-67, O-122, Q-20, Q-59 to DONE with evidence; added F41 note.
- `porting/parity/TEST-MAP.md`: marked ported tests for settings and settings-api-key-helper.

READY:G7a
