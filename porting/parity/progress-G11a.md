# G11a = AG-4 (`/mcp`) — progress

Worktree `/private/tmp/claude-501/par-b` (HEAD 7fcdb39). Branch `feat/rust-port`.
Scope: `/mcp` status + reconnect/enable/disable, the `promptToClaude` rewrite,
the available-command entry, and the control wrappers.

## Checklist
- [x] Read plan Phase 3 / Decision 21, FEATURES rows, upstream `mcp-command.ts` + tests
- [x] `mcp_command.rs`: parse / format / clean / run + `inv_56_*` units (goldens
      from the installed `dist/mcp-command.js` via `node -e`)
- [x] `control.rs`: `mcp_status` / `mcp_reconnect` / `mcp_toggle` wrappers
- [x] `agent.rs`: `promptToClaude` `/mcp:s:c` rewrite (S-08) + single-text `/mcp` parse
- [x] `session.rs`: `McpPrompt`/`McpPrepared` commands, spawn the control run after
      the prompt frame, hold `local_command_output` / synthetic-assistant text,
      publish the replacement at the result frame with the output shape's
      `messageIdForGrouping`, cancelled turn drops it
- [x] `map.rs` `/mcp` entry: already present (S-14) — verified, no change needed
- [x] corpus `mcp-command-status`, `mcp-command-reconnect`, `mcp-prompt-rewrite` +
      `bash porting/capture.sh` (Node-captured; stable on re-capture)
- [x] differential tests appended at the END of `tests/differential.rs`
- [x] FEATURES rows S-02/S-08/S-14/S-15/S-17/S-18/S-20/A-23/A-24/Q-21 flipped
      with evidence on both sides
- [x] TEST-MAP `src/tests/mcp-command.test.ts` rows classified (51 rows)
- [x] fmt / clippy / lib / turn / differential / workspace / gate / JsonPath guard /
      test-map / module-map

## Files changed
- `rust/claude-agent-acp-rs/src/mcp_command.rs` (new): pure parse / format / clean
  / action-targets / render + the spawned `run_mcp_command`, `McpTurns` actor
  state, `message_id_of` / `assistant_text`; 31 `inv_56_*` units.
- `rust/claude-agent-acp-rs/src/agent.rs`: `rewrite_mcp_prompt` call in
  `prompt_to_claude` (S-08); `prompt_mcp_command(&params)` + `send_mcp` before
  `send_prompt`.
- `rust/claude-agent-acp-rs/src/session.rs`: `Command::McpPrompt` /
  `Command::McpPrepared`; prompt arm spawns the run after `send_user`; `Cancel` /
  `Close` / stream-EOF clear the state; `local_command_output` and synthetic
  assistant shapes held; result arm publishes the replacement, suppresses the
  mirrored result text on a success result, keeps an error result's text.
- `rust/claude-agent-acp-rs/src/control.rs`: `mcp_status`, `mcp_reconnect`,
  `mcp_toggle` (wire `mcp_status` / `mcp_reconnect{serverName}` /
  `mcp_toggle{serverName,enabled}`, per `sdk.mjs`).
- `rust/claude-agent-acp-rs/src/session_ext.rs`: one additive field
  `mcp: McpTurns` (Decision 7; the work order's file list omitted this file).
- `rust/claude-agent-acp-rs/src/lib.rs`: `pub mod mcp_command`.
- `rust/claude-agent-acp-rs/tests/differential.rs`: `inv_24_mcp_command_status`,
  `inv_24_mcp_command_reconnect`, `inv_24_mcp_prompt_rewrite`.
- `porting/corpus/mcp-{command-status,command-reconnect,prompt-rewrite}.{acp.json,transcript.jsonl}`.
- `porting/fixtures/mcp-*.{frames,argv}.json` (captured; not hand-edited).
- `porting/parity/FEATURES.md`, `porting/parity/TEST-MAP.md`.

## Rows
S-02 PARTIAL (status + actions + `reconnected` tracking; OAuth follow-up needs
F23 URL elicitation — A-01/A-25), S-08 DONE, S-14 DONE, S-15 DONE, S-17 DONE,
S-18 DONE, S-20 DONE, A-23 DONE, A-24 DONE, Q-21 DONE.

## Deviations / notes
- The `/mcp` control run starts right after the prompt frame (not at the first
  output shape). The fake transcript steps then expect the control requests; the
  replacement is published at the result frame, which is wire-identical to Node
  (Node awaits the producer at the output shape, then publishes the chunk before
  the result). The corpus paces the result with `{"sleep_ms":100}` (same device as
  `async-task-stop`) so the prepared markdown always lands before it.
- A **result-only** prompt whose result beats the control run keeps the CLI text
  (Node holds the turn). Fixing that needs a settle deferral, which the HARD rule
  forbids (turn.rs / unfinished.rs / settle path); it is recorded as a TEST-MAP
  `different:` row and not corpus-covered.
- `McpPrepared.reconnected` records the reconnect targets for the OAuth
  afterDelivery hook; the hook itself is not ported (needs F23 elicitation), so
  S-02 stays PARTIAL with that exact gap.
- `local_command_output` is still dropped for non-`/mcp` turns (pre-existing S-06
  gap; out of this group's scope).
- No `turn.rs` / `turn/turn_tests.rs` / `unfinished.rs` edits; the settle path is
  untouched.

## Commands run (all pass)
- `cargo fmt --manifest-path rust/Cargo.toml --all` (+ `--check`) -> clean
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` -> clean
- `cargo test ... -p claude-agent-acp-rs --lib` -> 521 passed
- `cargo test ... -p claude-agent-acp-rs --lib turn` -> 60 passed
- `cargo test ... -p claude-agent-acp-rs --lib inv_56` -> 31 passed
- `cargo test ... -p claude-agent-acp-rs --test differential` -> 86 passed, 1 ignored (pre-existing)
- `cargo test ... --workspace` -> all green
- `bash rust/scripts/rust-gate.sh` -> `==> gate OK` (TEST-MAP / MODULE-MAP / PARITY PASS)
- `git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'` -> 0
- `git diff 455c146 -- .../turn.rs .../turn .../unfinished.rs | wc -l` -> 0
- re-capture into a scratch `CAPTURE_OUT_DIR` -> byte-identical fixtures

READY:G11a
