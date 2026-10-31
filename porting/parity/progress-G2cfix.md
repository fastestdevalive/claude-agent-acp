# Progress — G2cfix (fix list from Opus review of 6b9f98f)

Worktree: `/private/tmp/claude-501/par-c` @ eaa201f. Group: G2cfix.

## Checklist

- [x] Read review-G2c.md, upstream `acp-agent.ts`/`session-mode.ts`/`permissions/*`, Rust regions.
- [x] Fix 1: remove the ack race — resolve the permission answer in the actor
      (`Command::PermissionAnswered` carries `reply`), no suspension after the
      client answered; failing-first test for a Cancel between command and reply.
- [x] Fix 2: FEATURES T-59 -> PARTIAL (plan-file release signal not ported,
      `renderer.ts:503-516`; `planFile` advertised at agent.rs:108) `[pkg:F9]`.
- [x] Fix 3: Auto-unavailable decided at ANSWER time from the actor's current
      model (Node `acp-agent.ts:10219`), not at request arrival; test switches
      the model while the dialog is open.
- [x] Fix 4: fallback warning for EVERY rewrite (any tool), not only ExitPlanMode;
      unit test for the rewrite flag.
- [x] fmt / clippy / --lib / differential / workspace / rust-gate.sh / JsonPath
      guard / no new `#[ignore]`.
- [x] READY:G2cfix

## What changed

### Fix 1 — cancel/close race (medium)

`Command::ExitPlanModeApplied { mode, fallback, ack }` is gone. The permission
round-trip now hands the client's answer to the actor as
`Command::PermissionAnswered { value, ctx, request_id, tool_use_id, tool_input,
reply }`; the actor resolves the answer and sends the `control_response` in one
step (`resolve_permission_answer`, session.rs:3013-3098). The command carries
`reply`, so `Command::Cancel` / `Command::Close` aborting the permission task
after the client answered can no longer drop the allow. Node's order is kept:
warning -> current_mode_update -> config_option_update -> reply (the CLI's
`tool_result` follows the reply), and `run_loop`'s biased select keeps the
command ahead of the stream.

Failing-first test: `session::tests::permission_answer_survives_cancel_between_the_command_and_the_reply`
(session.rs:5242). Red before the change (the old command had no `reply` and the
task suspended on `ack`; the test does not compile against the old API), green
after: it answers `exit-plan-default`, receives the queued answer command,
aborts every pending permission task (as Cancel does), then asserts the reply
resolves `allow` and `current_mode_update: default` went out.

### Fix 2 — FEATURES T-59

T-59 is PARTIAL with the exact gap: the `planFileReleased` clear-signal
`tool_call` (`renderer.ts:503-516`, `interaction.ts:53-58,75-81`) is not ported
(tools.rs:921-923) while `agent.rs:108` advertises `planFile`; deferred to T-60,
tagged `[pkg:F9]`. The release signal needs the whole planFile path machinery
(existingPlanFile/resultPlanFile/field tracker), which is T-60's scope, not a
small standalone port.

### Fix 3 — Auto check at answer time

`handle_permission` no longer snapshots `auto_unavailable` when the request
arrives; the ctx carries a placeholder and `resolve_permission_answer` sets it
from `model_lacks_auto(&ext.config)` when the answer is processed. This mirrors
Node's `isAutoUnavailable(session)` after `requestPermissionFromClient`
(`acp-agent.ts:10219`). Test:
`session::tests::auto_fallback_uses_the_model_current_when_the_client_answers`
(session.rs:5372): starts on a model with Auto, opens the dialog, switches to a
model without Auto, then answers `exit-plan-auto`; asserts `acceptEdits` +
warning. Red on the old request-time snapshot, green now.

### Fix 4 — warning for every rewrite

`resolve_permission_answer` publishes `publishFallbackWarning` whenever the
decoded allow carries `setMode auto` and the model lacks Auto — for any tool,
once per session — then the ExitPlanMode mode updates. The flag comes from
`permission::fallback_rewrite_applies` (`permission.rs:516`), derived from the
decoded `updatedPermissions` (Node `session-mode.ts:196-221`), not from the
selected option id. Test:
`permission::tests::fallback_rewrite_applies_to_any_tool` (permission.rs:1101).

## Commands run (all in `/private/tmp/claude-501/par-c`, UPSTREAM_CACHE_DIR per lane rules)

- `cargo fmt --manifest-path rust/Cargo.toml --all` — clean.
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` — clean.
- `cargo test ... -p claude-agent-acp-rs --lib` — **379 passed, 0 failed, 0 ignored**.
- `cargo test ... -p claude-agent-acp-rs --test differential` — **64 passed, 0 failed, 1 ignored** (pre-existing `inv_24_todo_perm_settle`); `exit-plan-default`/`exit-plan-auto` byte-identical to Node.
- `cargo test --manifest-path rust/Cargo.toml --workspace` — all green.
- `bash rust/scripts/rust-gate.sh` — ends `==> gate OK`.
- JsonPath guard (`git diff -U0 455c146 -- .../tests .../acp-recorder .../fake-claude | grep '^+' | grep -c 'JsonPath::new'`) — **0**.
- New `#[ignore]` in the diff — **0**.
- `git status --short`: only `porting/parity/FEATURES.md`, `src/permission.rs`,
  `src/session.rs` (plus this progress file). No `turn.rs`, `control.rs`,
  `session_config.rs`, `usage.rs`, `settings.rs`, `process.rs` touched.

READY:G2cfix
