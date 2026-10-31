# Progress — Group G7a-fix6: Reaper Probe Handshake Recursion & Orphan Prevention

## Defect Summary
The reaper supervisor binary resolution mechanism suffered from a potential fork-bomb defect: when an unknown binary (or example/test host) invoked `spawn_raw`, `resolve_reaper_binary` fell back to `current_exe()` and parent directory searches, probe-executing `--reaper --probe`. If the target binary mishandled flags or spawned sessions itself, unbounded recursion occurred. Furthermore, tests and examples lacked hard timeouts and proper RAII reaping guards, leaving orphan processes in background gate runs.

## Implemented Fixes

### 1. Positive Identification Without Executing Unknown Binaries
- Modified `rust/claude-agent-acp-rs/src/process.rs`:
  - Removed candidate fallback (current_exe / directory scanning) completely from `resolve_reaper_binary`.
  - A reaper binary is resolved ONLY if:
    (a) Explicitly overridden via `ACP_REAPER_BIN` (passing probe handshake).
    (b) Explicitly registered by adapter entry via `set_reaper_binary`.
  - Unregistered hosts (embedders, examples) get NO reaper, log a one-time warning via `AtomicBool`, and run normally.
- Updated `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs`:
  - Main adapter registers its own `current_exe()` via `set_reaper_binary` on startup before running tokio main.

### 2. Hard Recursion Guard
- Added `pub const REAPER_GUARD_ENV: &str = "CLAUDE_ACP_REAPER_GUARD"`.
- `probe_reaper_binary` and `spawn_reaper` pass `CLAUDE_ACP_REAPER_GUARD=1` in child command env.
- `resolve_reaper_binary`, `is_reaper_binary`, and `spawn_reaper` immediately abort if `CLAUDE_ACP_REAPER_GUARD` is set in env or snapshot.
- `src/bin/claude-agent-acp-rs.rs`: if `CLAUDE_ACP_REAPER_GUARD` is set but process was not invoked with `--reaper`, it prints an error and exits 1 immediately.
- `examples/in_process.rs` and `examples/spawn_child.rs`: check argv for `--reaper` or `CLAUDE_ACP_REAPER_GUARD` and exit 1 immediately.

### 3. Orphan Prevention in Test Helpers and Examples
- Replaced indefinite sleep loops in `examples/spawn_child.rs` with a bounded watchdog loop (30s) to prevent immortal orphans if test runners abort.
- In `rust/claude-agent-acp-rs/src/process.rs`: in `spawn_reaper`, spawned an async tokio task `tokio::spawn(async move { let _ = child.wait().await; });` to automatically reap the supervisor child when it exits, preventing defunct/zombie processes.
- In `rust/claude-agent-acp-rs/tests/common/mod.rs`: updated `pid_is_gone` to call `waitpid(pid, WNOHANG)` for children of the current process, reaping zombie children on poll.
- Added RAII `KillOnDrop` child guards in `rust/claude-agent-acp-rs/tests/process_transport.rs` around all spawned test processes.
- Replaced disallowed `std::sync::Mutex` in `tests/common/mod.rs` with an `AtomicBool` spinlock `acquire_build_lock()`.

### 4. Process Hygiene & Zero Orphan Enforcement
- All process checks performed strictly via PID tracking and exact binary path filtering in `ps -Ao pid,command`.
- No `pkill` or `killall` by name was ever used.

### 5. Documentation & Parity Notes
- Updated comments and D8-neighboring documentation in `process.rs`.
- Updated `porting/parity/FEATURES.md` (row G7a/F41 note updated in place without touching header counts).

## Process-Count Tests Added (`tests/process_transport.rs`)
1. `test_in_process_bounded_descendants_no_fork_bomb`: verifies running in-process host with fake claude leaves 0 leaked reaper/probe processes and has bounded descendants (<= 6).
2. `test_mishandling_probe_target_stopped_at_depth_1`: verifies a mishandling probe target attempting to recurse is halted at depth 1 by `CLAUDE_ACP_REAPER_GUARD`.
3. `test_unregistered_host_gets_no_reaper_and_logs_warning`: verifies unregistered hosts get `reaper_pid().is_none()`.
4. `test_bins_reject_reaper_flag`: verifies example binaries reject `--reaper` flag immediately with exit code 1.
5. `test_reaper_guard_env_blocks_nested_spawn`: verifies setting `CLAUDE_ACP_REAPER_GUARD` in env suppresses reaper spawn.

## Acceptance Gates & Test Results

1. **fmt check**:
   ```
   cargo fmt --manifest-path rust/Cargo.toml --all --check
   PASS (clean, code 0)
   ```

2. **clippy (workspace + warnings)**:
   ```
   cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --all-features -- -D clippy::correctness -D clippy::suspicious -D clippy::complexity -D clippy::perf -D warnings
   PASS (Finished dev profile, code 0)
   ```

3. **clippy (windows target)**:
   ```
   cargo clippy --manifest-path rust/Cargo.toml --target x86_64-pc-windows-gnu -- -D warnings
   PASS (Finished dev profile, code 0)
   ```

4. **lib tests**:
   ```
   cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib
   PASS: 583 passed; 0 failed; 0 ignored
   ```

5. **5 process-count tests x5 iterations**:
   ```
   PASS: 5/5 iterations clean, 0 failures
   ```

6. **reaper tests x10 iterations**:
   ```
   PASS: 10/10 iterations clean, 0 failures (6/6 tests passing each run)
   ```

7. **differential tests**:
   ```
   cargo test --manifest-path rust/Cargo.toml --test differential
   PASS: 95 passed; 0 failed; 0 ignored
   ```

8. **cross-target checks**:
   ```
   cargo check --manifest-path rust/Cargo.toml --workspace --target x86_64-pc-windows-gnu
   cargo check --manifest-path rust/Cargo.toml --workspace --target aarch64-apple-darwin
   cargo check --manifest-path rust/Cargo.toml --workspace --target x86_64-unknown-linux-musl
   PASS (all 3 targets finished with code 0)
   ```

9. **D8 unsafe confinement**:
   ```
   bash rust/scripts/d8-check.sh
   PASS: D8 OK: unsafe strictly confined to process.rs windows_job
   ```

10. **JsonPath guard**:
   ```
   git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'
   PASS: 0
   ```

11. **Leftover process check**:
   ```
   ps -Ao pid,command | grep -E 'examples/in_process|spawn_child|--reaper' | grep -v grep
   PASS: empty (0 processes)
   ```

12. **Canonical Gate**:
   ```
   UPSTREAM_CACHE_DIR=/Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1/rust/target/upstream bash rust/scripts/rust-gate.sh
   ```
   Last 12 real lines of output:
   ```
   note: 2211 missing: row(s) warn only (each needs a MISSING.md ticket)
   TEST-MAP CHECK: PASS
   upstream modules: 91 | map rows: 107 | unmapped: 0 | stale: 0 | bad relationship: 0 | bad rust path: 0
   MODULE-MAP CHECK: PASS
   PARITY: PASS
   ==> upstream ahead (non-failing)
   upstream is 1 versions ahead of pinned 0.94.0 (latest 0.95.0)
   ==> gate OK
   ```

READY:G7afix6
