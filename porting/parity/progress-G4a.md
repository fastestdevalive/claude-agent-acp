# G4a — ROADMAP F2a (client capabilities, notices, initialize.authMethods)

READY:G4a

## Scope implemented

- `rust/claude-agent-acp-rs/src/client_caps.rs` (new): the full
  `initialize.clientCapabilities` parse — `terminal_output`/`terminal_output_delta`,
  `diffPatch`, `rawInputRendering`, `planFile`, `recommendedValue`,
  `session.configOptions.boolean`, `session.notices`, `session.compaction`,
  `subagents` (RFD/AIR form), `_meta["subagent-transcript"]`, `elicitation.form`/`url`,
  AIR `loadReplayEnd`, `auth.terminal`, `_meta["terminal-auth"]`,
  `auth._meta.gateway`. Parsed once at `initialize` and stored on `SessionExt`.
- `rust/claude-agent-acp-rs/src/notices.rs` (new): `SessionNotice`, the
  `notice` update, the bold-label `agent_message_chunk` fallback with optional
  `_meta`, and the shaping helpers (`noticePlainText`, `splitNoticeText`,
  `sentenceCase`, `normalizeNoticeText`, 256-char UTF-16 title cap).
- `initialize.authMethods`: non-remote subscription + Console terminal logins,
  the `NO_BROWSER`/`SSH_*`/`CLAUDE_CODE_REMOTE` remote `claude-login` branch,
  `_meta["terminal-auth"]` (`command`/`args`/`label`), and the
  `auth._meta.gateway` gateway/gateway-bedrock methods. The env is read only
  through `env_snapshot.rs` (`EnvSnapshot::flag`); hide-claude-auth is F34 and
  treated as not hidden.
- Notices replace the hard-coded sites: the task-stopped acknowledgement
  (`session.rs`) and the Auto-mode fallback warning (all three sites plus
  `fallback_updates`/`external_switch_updates`).
- AIR `version` parsing accepts a JSON float `1.0` (Number.isInteger parity).

## Files changed

- `rust/claude-agent-acp-rs/src/{client_caps.rs (new), notices.rs (new), lib.rs,
  agent.rs, air.rs, session_ext.rs, session.rs, env_snapshot.rs}`.
- `rust/claude-agent-acp-rs/tests/{common/mod.rs, differential.rs}` — per-script
  env injection and the `terminal-auth` entrypoint normaliser.
- `porting/capture.sh` — per-script `env` injection and the same
  `terminal-auth` normaliser.
- `porting/corpus/initialize-capabilities-{plain,auth-terminal,terminal-auth-meta,
  gateway,no-browser,air,notices}.{acp.json,transcript.jsonl}` + captured
  `porting/fixtures/*.{frames,argv,init}.json`.
- `porting/parity/FEATURES.md` (rows U-02, U-03, U-04, U-16, C-15, C-16, C-17,
  C-18, C-19, C-21, C-34, C-41, O-90, T-01, Q-56, Q-64; C-23 stays PARTIAL —
  capability negotiated, renderer wiring pending and `map.rs` is out of scope).
- `porting/parity/TEST-MAP.md` (session-notices cases + gateway/terminal-auth
  authorization/scenario rows).

## Rows flipped

U-02, U-03, U-04, U-16, C-15, C-16, C-17, C-18, C-19, C-21, C-34, C-41, O-90,
T-01, Q-56, Q-64 → DONE. C-23 → PARTIAL (negotiation done; terminal rendering
still pending).

## Commands (real output)

- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check` -> exit 0.
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`
  -> clean.
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`
  -> 338 passed, 0 failed.
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential`
  -> 60 passed, 0 failed, 1 ignored (`inv_24_todo_perm_settle`, pre-existing).
- `cargo test --manifest-path rust/Cargo.toml --workspace` -> all suites green.
- `UPSTREAM_CACHE_DIR=<main-tree>/rust/target/upstream bash rust/scripts/rust-gate.sh`
  -> `==> gate OK`.
- JsonPath guard (`git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests
  rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'`)
  -> prints `0`.

## Failing-first evidence

- `notices::tests::*` and `client_caps::tests::*` are new modules; they fail
  against the pre-change tree by construction.
- The corpus scenarios are non-vacuous: the Node fixtures carry the actual
  authMethods/notice/AIR frames; the differential fails if the Rust side omits
  a method or sends the transcript fallback where Node sends a notice.
- `air::tests::float_integer_version_is_accepted_like_number_is_integer` fails
  against the old `as_u64` parse.

## Notes

- `C-23` is the only PARTIAL: `terminal_output`/`terminal_output_delta` are now
  negotiated and exposed on `SessionExt`, but the `renderer.ts` terminal
  content/`terminal_info`/`terminal_output`/`terminal_exit` wiring lives in
  `map.rs`, which this turn is not allowed to edit.
- The `_meta["terminal-auth"]` `command`/entrypoint arg is volatile by design
  (Node `process.execPath` vs the Rust binary); `capture.sh` and the
  differential normalise it to `$EXEC` and drop the leading entrypoint
  argument, while every flag stays compared exactly. This is a normaliser, not
  an ignore path.
