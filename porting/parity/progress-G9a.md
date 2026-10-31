# G9a — LD-1 (live divergences part 1) — progress

Worktree: /private/tmp/claude-501/par-c (HEAD 6a5b5c5, branch feat/rust-port)
Group: G9a. Rows: U-09, S-04, N-04, B-118 (flipped in place). D10/D17 pure fn only (rows not flipped).

## Checklist

- [x] RECONCILE HEAD vs plan Phase 1
- [x] 1.1 usage::raw_max_tokens + units (inv_70_*)
- [x] 1.2 wiring both refresh sites + apply_context_window_answer + actor/unit test
- [x] 1.3 corpus usage-context-refresh + differential
- [x] 1.4 D9 — NOT done (see Recon: latch needs agent.rs Registry / spawn plumbing outside this group's file scope)
- [x] 1.5 D11 final-text messageId + units + corpus local-command-message-id
- [x] 1.6 D5 terminal commands + units + corpus terminal-commands
- [x] 1.7 D10/D17 — pure fn auth_context_changed + inv_78_context_change_rules only (schedule not wired; outside scope)
- [x] FEATURES rows flipped in place (U-09, S-04, N-04, B-118 = DONE)
- [x] TEST-MAP rows classified (terminal-commands, usage-context-refresh, inv_70 unit)
- [x] fmt / clippy / --lib / turn / differential / workspace (polluted) / rust-gate OK / JsonPath 0

## Recon: what had already landed at HEAD 6a5b5c5

- **D1** already fixed by 716f9d1: `control::raw_max_tokens` reads `frame.response.response`
  (`control.rs:332-340`), used at `agent.rs` (register_session refresh) and `session.rs` (model switch).
  F5b also landed the once-only rule (`UsageState::authoritative`) and the model-unchanged
  check (`session_model::context_window_answer_is_current`). Remaining work done here:
  `usage::raw_max_tokens` wrapper rejecting `0`/missing/non-numeric, both call sites use it,
  `apply_context_window_answer` extracted + `inv_70_*` units, corpus `usage-context-refresh`.
- **D9**: `process::CLAUDE_AGENT_SDK_VERSION` and `compute_claude_env_defaults` landed with F4a
  (O-167 DONE). The per-connection latch (`Registry.sdk_version_latched`: first session keeps a
  host value, later spawns/probes get the constant unconditionally) is NOT implemented: it needs
  a Registry field in `agent.rs` and a `latched` flag threaded through the spawn path, which are
  outside this group's allowed file regions. Left open, noted here (O-167 stays DONE, no row flip).
- **D11**: not done before; implemented here.
- **D5 / N-04 / U-09 / S-04**: not done before; implemented here.
- **D10/D17**: not done before. Only the pure `auth_status::auth_context_changed` + its unit test
  were added. The probe schedule (prompt probe, probe env, session-account barrier) needs
  `agent.rs` auth/initialize/prompt wiring and `session.rs` prompt-arm changes, outside this
  group's file regions — left open, noted here.

## Implementation notes

- D11: `handle_events`/`settle_events` take `final_message_id: Option<&str>`; only the `"result"`
  arm passes `line["uuid"]`; the `FinalText` chunk gets `.message_id(...)`. `turn.rs` untouched.
  Corpus `local-command-message-id` uses the result frame alone (no `local_command_output` frame):
  the `local_command_output` arm is still MISSING (N-10 / F24), so including it would make the
  differential red for a reason outside this group. The fixture proves the result-text chunk's
  messageId (`grep -c agent_message_chunk` == `grep -c messageId`).
- D5: `SessionExt.commands: Vec<Value>` + `terminal_slash_commands: Option<Vec<String>>`;
  `Command::InitCommands` seeds them from the initialize handshake; `Command::CommandsSnapshot`
  reads them back (B-118). `map_commands_changed(commands, &[String])` filters by NAME.
  `system/init` latches and re-advertises on a first/real change; `commands_changed` replaces the
  stored list and re-advertises with the latched terminal list. Node resets `session.skillPaths`
  on `commands_changed`; Rust has no skill-path cache (no AIR skill meta), so the reset is vacuous
  (noted on U-09).
- B-118: an already-live `session/load` AND `session/resume` re-send
  `available_commands_update` from the actor's current list (`session.live_commands()`).
- D1 corpus: init model `claude-haiku-3-5` (no `1m`), refresh reply `rawMaxTokens:1000000`,
  `sleep_ms:150`; the first `usage_update` is already `size:1000000` (3 occurrences).

## Commands run (real output)

- `cargo fmt --all` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test -p claude-agent-acp-rs --lib` — `438 passed; 0 failed`.
- filtered `inv_70|inv_72|inv_73` (run per prefix) — ok (4 + 2 + 4).
- `cargo test -p claude-agent-acp-rs --lib turn` — `60 passed; 0 failed`.
- `cargo test -p claude-agent-acp-rs --test differential` — `75 passed; 1 ignored`
  (the pre-existing HAZARD-IMPL T-21 ignore); the three new `inv_24_*` green.
- polluted `cargo test --workspace` — every suite `ok`.
- `bash rust/scripts/rust-gate.sh` — `==> gate OK`, TEST-MAP/MODULE-MAP PASS.
- JsonPath guard: `git diff -U0 455c146 -- rust/.../tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'` → `0`.
- `turn.rs` / `turn/turn_tests.rs` / `unfinished.rs` diff vs HEAD: empty.

READY:G9a
