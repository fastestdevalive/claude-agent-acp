# G5a — ROADMAP F2b (process entry parity: --cli, signals, logger, managed policy, shutdown, session timing)

STATUS: DONE

## Checklist

- [x] `logger.rs` (new): CLAUDE_AGENT_LOGS file logger, `{iso} pid={pid} {msg}` line, error -> stderr + file
- [x] `session_timing.rs` (new): `[session/<op>] sessionId= phase= durationMs= totalMs=` lines
- [x] `permission_mode.rs` + `settings.rs`: invalid-mode logging (non-string/empty/unknown/bypass clamp)
- [x] `settings.rs`: managed-policy `env` tier applied before serve (best-effort)
- [x] `process.rs`: SIGINT+SIGTERM shutdown signal, `--cli` passthrough (signals/exit mirroring)
- [x] `agent.rs`: ServeHandle (dispose/closed), timing call sites for new/load/resume/fork, logger threaded
- [x] `session.rs`/`session_ext.rs`: init `plugin_errors` logging (dedupe per session)
- [x] `bin`: --cli first, logger init, managed policy, ServeHandle + signals
- [x] failing-first unit tests (logger line, timing line, permission log, managed env)
- [x] `tests/shutdown.rs`: SIGINT exit 0, SIGTERM exit 0, --cli passthrough/exit code/SIGTERM relay
- [x] FEATURES rows O-02 O-03 O-05 O-64 N-05 B-01 V-01 V-20 Q-18 Q-25 Q-57 flipped
- [x] DECISIONS-FOR-USER note for C-01/O-01 version divergence (D7)
- [x] fmt / clippy / lib / differential / workspace / rust-gate / JsonPath guard / cfg guard

## Files changed

- `rust/claude-agent-acp-rs/src/logger.rs` (new): `Logger` (file optional, pid, ISO-8601 ms line),
  `Logger::from_env` via `EnvSnapshot`, `error` = stderr + file.
- `rust/claude-agent-acp-rs/src/session_timing.rs` (new): `SessionTiming` with `phase`/`phase_at`,
  duration-since-previous-phase + total, rounded ms, detail suffix.
- `rust/claude-agent-acp-rs/src/permission_mode.rs`: `resolve_permission_mode_logged` (non-string,
  empty, unknown, bypass clamp) and `resolve_initial_permission_mode_logged` (resumed-transcript
  rejection); existing pure entry points delegate with no logger.
- `rust/claude-agent-acp-rs/src/settings.rs`: `default_mode_value` (raw value for O-64),
  `managed_policy_env`, `apply_managed_policy_env` + testable `apply_managed_policy_env_at`,
  `env_entries`.
- `rust/claude-agent-acp-rs/src/process.rs`: `wait_for_shutdown_signal` selects SIGINT/SIGTERM;
  `run_cli_passthrough` (unix: inherit stdio, relay SIGINT/SIGTERM/SIGHUP, mirror exit / 128+N;
  windows: Ctrl-C kills the child). `Signal::{Interrupt,Hangup}` are `cfg(unix)`.
- `rust/claude-agent-acp-rs/src/agent.rs`: `ServeOptions.logger`, `ServeHandle` (idempotent
  `closed`, `dispose` via watch), `serve_handle`, `serve_with_shutdown`, `ResumedKind`, session
  timing call sites in session/new (create), session/load/resume/fork.
- `rust/claude-agent-acp-rs/src/session.rs` / `session_ext.rs`: system/init `plugin_errors`
  logging with per-session dedupe (`logged_plugin_errors`) and Node's template rendering.
- `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs`: `--cli` checked first (argv forwarded,
  exit mirrored), logger from env, managed-policy env merged under provider env, ServeHandle +
  SIGINT/SIGTERM dispose.
- `rust/claude-agent-acp-rs/src/lib.rs`: module registration.
- `rust/claude-agent-acp-rs/tests/shutdown.rs`: added SIGINT exit-0/no-orphan, `--cli` args +
  exit-code mirroring, `--cli` SIGTERM relay (hermetic shell fake CLI in a temp dir).
- `rust/claude-agent-acp-rs/tests/transport_parity.rs`: `logger: None` on the test `ServeOptions`.
- `porting/parity/FEATURES.md`: 11 rows flipped in place (C-01/O-01 left untouched per the work order).
- `porting/parity/TEST-MAP.md`: session-timing, managed-policy and resolve-permission-mode upstream
  cases marked `ported:` (one `different:` for the SDK-vs-file managed-tier resolution).
- `porting/parity/DECISIONS-FOR-USER.md`: D7 (composed version string vs bare npm pin).

## Commands run (real output)

All from `/private/tmp/claude-501/par-c` with
`UPSTREAM_CACHE_DIR=/Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1/rust/target/upstream`.

- `cargo build -p claude-agent-acp-rs` — PASS (first build).
- `cargo fmt --all` — PASS.
- `cargo clippy --workspace --all-targets -- -D warnings` — PASS.
- `cargo test -p claude-agent-acp-rs --lib` — PASS (375 passed; added 1 settings test, plus the
  logger/timing/permission tests from the resumed tree).
- `cargo test -p claude-agent-acp-rs --test shutdown` — PASS (6/6: EOF, SIGTERM, SIGINT, in-process,
  `--cli` args/exit, `--cli` SIGTERM relay).
- `cargo test -p claude-agent-acp-rs --test differential` — PASS (62 passed, 1 pre-existing ignore
  `inv_24_todo_perm_settle`).
- `cargo test --workspace` — PASS.
- `bash rust/scripts/rust-gate.sh` — PASS, ends `==> gate OK` (fmt/clippy/build/tests/cross-target
  checks/G9/G10/TEST-MAP/MODULE-MAP).
- JsonPath guard: `git diff -U0 455c146 -- ... | grep '^+' | grep -c 'JsonPath::new'` → `0`.
- cfg guard: `grep -rn 'cfg(unix)\|cfg(windows)\|target_os' rust/claude-agent-acp-rs/src | grep -v process.rs`
  → no output.

## Notes / limitations

- Managed-policy env: upstream calls `resolveSettings({settingSources: []})`, which also picks up
  macOS MDM and Windows HKLM/HKCU sources. The Rust port reads the platform managed-settings.json
  file only (documented in FEATURES O-03 and TEST-MAP `different:`); the env is applied at entry as
  the lowest-priority spawn env (provider `extra_env` wins), and a malformed/unreadable policy logs
  and the adapter still starts. The real process environment is not mutated (the adapter reads env
  only through `env_snapshot.rs`). The OS policy path is not writable from a hermetic test, so the
  unit tests drive `apply_managed_policy_env_at` with an explicit temp path; the binary's call site
  is the one-line `apply_managed_policy_env(&logger)`.
- Session timing phases are emitted at the agent orchestration layer (validate-cwd, resume-transcript,
  settings, session-ready, replay, register). The session-internal upstream phases
  (prepare-query/sdk-initialize/models/modes/agents) live inside `session.rs`, outside this group's
  scope.
- `plugin_errors` logging is log-only and has no dedicated hermetic test (it would need a full session
  actor + fake CLI harness); it is exercised by the system/init arm and deduped per session.
- With `CLAUDE_AGENT_LOGS` unset the adapter attaches no logger, so diagnostics are not emitted;
  the file logger and all its tests are hermetic (temp dirs) and never read the host env directly.
- `--cli` on Windows is best-effort (Ctrl-C kills the child, exit 1); unix is the parity target.

READY:G5a
