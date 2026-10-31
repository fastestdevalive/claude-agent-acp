# Progress: Group G7a-fix2

Checklist of Opus Round 2 Review Findings (G7a-fix2):

- [x] 1. CLAIMS vs REALITY: Remove Linux PR_SET_PDEATHSIG claims. Document actual unix design (getppid + stdin EOF). Implement Windows Job Object (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE) in process.rs under cfg(windows). Compile with cross-target x86_64-pc-windows-gnu.
- [x] 2. Remove CLAUDE_MANAGED_SETTINGS_PATH environment override in settings.rs.
- [x] 3. Managed settings injection for all embedders (remove env.is_from_process() gate, provide injection mechanism via `load_with_managed`).
- [x] 4. Follow SDK on unreadable/timeout managed sources (inspect upstream sdk.mjs behavior, do not silently fall through).
- [x] 5. Non-blocking managed source reading (spawn_blocking / async) without synchronous thread::sleep on async workers.
- [x] 6. Reaper binary resolution: positive adapter binary identification, probe/handshake or registered binary; preserve ACP_REAPER_BIN override; test non-recursion.
- [x] 7. Update FEATURES.md summary line 15 (O-60, O-67, Q-59 are PARTIAL, reaper verified on Unix + Windows Job Object compile). Do not touch header count line.
- [x] 8. Debounce test in settings_manager.rs: assert exactly the debounced count (write back-to-back, assert == 2); fails without debounce.
- [x] 9. tests/process_transport.rs ~309: exercise ACP_REAPER_BIN through env snapshot.

## Verification & Acceptance Summary

- **Claims & Docs**: Accurately documented Unix reaper supervisor (`--reaper`, stdin EOF pipe + getppid) and Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) in `process.rs`. Removed Linux `PR_SET_PDEATHSIG` claims across codebase and docs.
- **Windows Job Object**: Implemented in `process.rs` under `#[cfg(windows)]` with safe handle wrappers via `windows-sys` 0.61.2 (`Win32_Foundation`, `Win32_System_JobObjects`). Cross-target compilation verified with `cargo check --target x86_64-pc-windows-gnu`.
- **Environment & Ingestion**: Removed unapproved `CLAUDE_MANAGED_SETTINGS_PATH`. Removed `env.is_from_process()` gate and `from_process` field from `EnvSnapshot`. Default `settings::load` loads managed tiers for all embedders; `settings::load_with_managed` allows explicit document injection.
- **Fail-Closed on Unreadable MDM / Registry**: Returns an empty object (`Value::Object`) when MDM plist or Windows HKLM exists but is unreadable or fails to parse, preventing fallback to lower tiers, mirroring SDK `e4`/`Tx`.
- **Non-blocking / Caching**: Cached managed sources per-process using `std::sync::OnceLock` in `process.rs`, eliminating repetitive disk/subprocess calls during async settings management. Bounded timeout reader joined cleanly.
- **Reaper Handshake & Override**: Implemented `--reaper --probe` handshake returning `claude-agent-acp-rs-reaper-v1` and explicit `set_reaper_binary` in adapter entry point. Tested `ACP_REAPER_BIN` override with a custom binary copy in `tests/process_transport.rs`.
- **Debounce Verification**: Back-to-back write test in `settings_manager.rs` verifies exactly 2 callbacks with no intervening delay, validating debounce behavior.
- **FEATURES.md Integrity**: Line 15 updated with PARTIAL for O-60, O-67, Q-59 and reaper status. Line 8 table counts unchanged.
- **Full Acceptance Suite**:
  - `cargo fmt --all --check` (clean)
  - `cargo clippy --workspace --all-targets -- -D warnings` (clean)
  - `cargo test -p claude-agent-acp-rs --lib` (555 passed)
  - `cargo test --test differential` (91 passed)
  - `cargo test --test process_transport reaper` 10x loop passed (10/10)
  - Hermetic settings test with isolated `HOME` passed
  - JsonPath regex guard check passed (0 matches)
  - Platform guard G9 passed (`cfg(unix)|cfg(windows)` strictly confined to `process.rs`)
  - Canonical gate passed (`UPSTREAM_CACHE_DIR=... bash rust/scripts/rust-gate.sh`)

READY:G7afix2
