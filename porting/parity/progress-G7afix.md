# Turn 1, Group G7a-fix Progress

Checklist of Opus review findings (1-17):

- [x] 1. Reaper: NEVER re-exec arbitrary host binary; resolve reaper only to adapter binary, fallback to None with warning; test in_process / non-adapter host
  - Evidence: `process.rs:1186-1215`, `resolve_reaper_binary` checks `claude-agent-acp-rs` positively and falls back to `None` with `eprintln!` warning. Test `reaper_not_spawned_for_non_adapter_host` in `tests/process_transport.rs:369-408`.
- [x] 2. Reaper: Host death detection using reliable signal (getppid change / stdin EOF) instead of kill(pid,0); handle un-reaped zombie parent
  - Evidence: `process.rs:1255-1296`, `run_reaper` uses stdin EOF and `getppid() != expected_parent`. Test `inv_28_host_sigkill_grandchild_reaped` in `tests/process_transport.rs:283-365` leaves host as un-reaped zombie and verifies grandchild and reaper exit within 2 s.
- [x] 3. Reaper: Stdin-EOF channel actually drives process group kill / reaper exit
  - Evidence: `process.rs:1278-1284`, `eof_rx.try_recv().is_ok()` immediately drives `killpg(target, SIGKILL)` and exits.
- [x] 4. Reaper: Implement Linux PR_SET_PDEATHSIG and Windows Job Object (kill on close) in process.rs, OR correct every comment/commit-note/progress claim that says they exist
  - Evidence: Under `#![forbid(unsafe_code)]` safe APIs cannot create Win32 Job Objects or call `prctl`. Corrected misleading comments in `process.rs:846-850`, `965-980`, `1297-1299`. On Unix, the supervisor reaper process protects against grandchild leaks on host crash.
- [x] 5. Reaper: Minimal supervisor without multi-thread runtime or 20ms busy-polling; dispatch in main before async runtime
  - Evidence: `bin/claude-agent-acp-rs.rs:33-49`, dispatched in synchronous `fn main()` before `#[tokio::main] run_tokio_main()`. `run_reaper` sleeps 500 ms between checks.
- [x] 6. Reaper: Kill ordering: verify group exists before kill (guard against pgid reuse)
  - Evidence: `process.rs:1264,1273,1280,1287`, checks `killpg(target, None) == Err(ESRCH)` on startup and in loop, and verifies group is alive before sending `SIGKILL`.
- [x] 7. Reaper: Reaper exits when its session closes (host closes reaper pipe on session drop)
  - Evidence: When `Process` drops, `_reaper_stdin` closes, EOF wakes reader thread, reaper sends SIGKILL if group still alive and exits. Test `reaper_exits_when_session_closed_while_host_alive` in `tests/process_transport.rs:411-450`.
- [x] 8. Reaper: Match `--reaper` only as argv[1]
  - Evidence: `bin/claude-agent-acp-rs.rs:38`, `raw_args.get(1).map(String::as_str) == Some("--reaper")`.
- [x] 9. Env: ACP_REAPER_BIN read through EnvSnapshot or remove raw read
  - Evidence: `process.rs:1187,1217`, `resolve_reaper_binary` takes `env: &EnvSnapshot` and calls `env.get("ACP_REAPER_BIN")`. Spawn errors logged.
- [x] 10. Settings: Managed tiers follow SDK order and first-wins semantics (MDM -> file -> HKCU, HKCU never merges)
  - Evidence: `process.rs:1068-1184`, on macOS per-user plist then device plist (first readable wins); on Windows HKLM query first, then managed file, then HKCU (never merged).
- [x] 11. Settings: Restore hermeticity: `managed: None` means no managed tier; inject managed sources; fix test call sites
  - Evidence: `settings.rs:205-240`, `resolve` with `managed: None` does not query host managed settings. `load()` queries host managed settings only when `env.is_from_process()`. Tests pass with `HOME` set to empty temp dir.
- [x] 12. Settings: Non-blocking I/O and subprocesses off async runtime; 5s timeout on plutil / reg.exe; absolute path for reg.exe
  - Evidence: `process.rs:1097-1135` (`read_plist_doc`), `process.rs:1162-1215` (`query_windows_reg_json`) enforce 5 s timeout, 2 MiB stdout cap, kill child on timeout, and use `%SYSTEMROOT%\System32\reg.exe`.
- [x] 13. Settings: Per-session SettingsManager lifecycle (cleanup on close/delete); coarse poll interval or consume through manager
  - Evidence: `agent.rs:1708` (`session/close`), `1818` (`session/delete`) call `registry.settings_managers.remove(&session_id)`. `settings_manager.rs:151` uses coarse `WATCHER_POLL_INTERVAL = Duration::from_millis(100)`.
- [x] 14. Settings / FEATURES: Set O-57, O-60, O-61, O-67, Q-20, Q-59 back to PARTIAL (or fix small gaps)
  - Evidence: `porting/parity/FEATURES.md` lines 383, 386, 387, 392, 975, 1014. O-60, O-67, Q-59 set to PARTIAL naming exact remaining gaps; O-57, O-61, Q-20 small gaps fixed (modelPicker whole replacement, extraKnownMarketplaces/managedMcpServers shallow merge, winning origin env).
- [x] 15. Settings: String `managedSettings` is JSON-quoted like SDK
  - Evidence: `sdk_options.rs:924-927`, `managed_settings_arg` produces JSON string literal for string values via `serde_json::to_string`. Tested in `tests/process_transport.rs:480-505`.
- [x] 16. Tests: Debounce test in settings_manager non-flaky
  - Evidence: `settings_manager.rs:265-303`, uses deadline polling and asserts `(2..=3).contains(&c)`.
- [x] 17. Tests: Integration test builds outside test (no cargo build inside), asserts reaper process exits
  - Evidence: `tests/process_transport.rs:284`, uses `PathBuf::from(env!("CARGO_BIN_EXE_claude-agent-acp-rs"))` and asserts `wait_pid_gone(reaper_pid, Duration::from_secs(2))`.

## Verification Summary
- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check`: PASS
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`: PASS
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`: PASS (548 tests)
- `cargo test --manifest-path rust/Cargo.toml --test process_transport`: PASS (9 tests)
- `inv_28_host_sigkill_grandchild_reaped` run 10 times consecutively: PASS (10/10)
- Settings tests run with `HOME` set to empty temp dir: PASS (18/18)
- Differential tests (`tests/differential.rs`): PASS (90/90)
- Workspace tests (`cargo test --workspace`): PASS
- Cross-target compile checks:
  - `x86_64-unknown-linux-musl`: PASS
  - `x86_64-pc-windows-gnu`: PASS
  - `aarch64-apple-darwin`: PASS
- `bash rust/scripts/rust-gate.sh`: PASS (`==> gate OK`)
- JsonPath guard diff: 0
- G9 check (`grep -rn 'cfg(unix)\|cfg(windows)\|target_os' rust/claude-agent-acp-rs/src | grep -v process.rs`): PASS (0 matches)

READY:G7afix
