# G3 — ROADMAP F3a "SDK control layer" (progress)

READY:G3

## Scope implemented (a)-(f)

All in `rust/claude-agent-acp-rs/src/control.rs` (primary) with the minimal
wiring in `session.rs` / `agent.rs`:

- (a) `request_user_dialog` with no handler and the `Z4` subtypes
  (`remote_tool_call`, `remote_plumbing_call`, `remote_tools_probe`,
  `remote_tools_reannounce`) are never answered. `SUPPRESSED_SUBTYPES` is
  short-circuited before any handler runs; `request_user_dialog` returns `None`
  from the adapter handler.
- (b) `elicitation` with no handler answers the SDK's SUCCESS
  `{action:"decline"}`.
- (c) every error response carries an `error` string
  (`Unsupported control request subtype: X`; callback-not-provided for
  `can_use_tool`/`hook_callback` when no handler exists).
- (d) a duplicate delivery of an in-flight `request_id` is skipped
  (`inflight.contains_key`); the id is released on both the response and the
  silent path (`Outbound::Finished`), matching the SDK's `finally`.
- (e) after close (`Control::close`, `sdk.mjs` `cleanupPerformed`) inbound
  requests are drained without a response. The actor calls `control.close()`
  on stream EOF (child gone).
- (f) `lastErrorResultText` tracking: a `result` frame sets/clears the SDK's
  diagnostic (success -> `result`, else trimmed non-empty `errors` joined with
  `"; "`); at stream EOF queued prompts are rejected with
  `SessionError::ChildErrorResult`, which `session_error_to_rpc` maps to the
  ACP router's plain-Error shape (`message:"Internal error"`,
  `data.details:"Claude Code returned an error result: <text>"`).

Outbound (A-44): `send_request` now resolves only on `response.subtype ==
"success"` and otherwise rejects with `ControlError::Remote(response.error)`.
`set_model` / `set_permission_mode` / `apply_flag_settings` propagate the
rejection; `ModeApplied` / `ConfigModeApplied` / `ModelApplied` /
`EffortApplied` keep their state updates conditional on success. A rejected
`set_permission_mode` keeps the CLI's non-empty message and only falls back to
`"Invalid Mode"` when empty (`session-mode.ts:254-263`). `interrupt` already
ignores a rejection (matching Node's fire-and-forget cancel path).

## Files changed

- `rust/claude-agent-acp-rs/src/control.rs` — handler contract `Option<Value>`,
  `ControlError::Remote`, SDK default responses, `SUPPRESSED_SUBTYPES`,
  duplicate/after-close handling, error-verdict rejection, 11 new unit tests.
- `rust/claude-agent-acp-rs/src/session.rs` — `build_permission_request_handler`
  SDK dispatch, mode error mapping, `last_error_result_text` +
  `error_result_text`, EOF rewrite, `control.close()`, 2 unit tests.
- `rust/claude-agent-acp-rs/src/agent.rs` — `SessionError::ChildErrorResult`
  mapping in `session_error_to_rpc` + unit test.
- `rust/claude-agent-acp-rs/tests/differential.rs` — `inv_24_sdkctl_scenarios`.
- `porting/corpus/sdkctl-{dialog-silent,z4-silent,elicitation-decline,unknown-subtype-error,duplicate-request}.{acp.json,transcript.jsonl}`
  + their captured `porting/fixtures/*.{frames,argv}.json` (via `capture.sh`).
- `porting/parity/FEATURES.md` — flipped A-18, A-19, A-20, A-44, R-07 to DONE;
  M-38/M-39/M-42 left PARTIAL with "F3a part done" evidence (not my packages).

## Commands (real output)

- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check` -> exit 0.
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`
  -> clean.
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`
  -> 302 passed, 0 failed (INV-9/INV-10 green).
- differential (`--test differential`) -> 59 passed, 0 failed, 1 ignored
  (`inv_24_todo_perm_settle`, pre-existing HAZARD).
- `cargo test --manifest-path rust/Cargo.toml --workspace` -> all suites green
  (incl. recorder/process/shutdown/differential).
- `UPSTREAM_CACHE_DIR=<main-tree>/rust/target/upstream bash rust/scripts/rust-gate.sh`
  -> `==> gate OK`.
- JsonPath guard (`git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests
  rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'`)
  -> prints `0`.

## Failing-first evidence

- Unit tests (d)(e)(f) fail against the old code by construction (old
  `route_inbound` answered every inbound request; `send_request` resolved every
  response; no `lastErrorResultText`).
- The silent corpus scenarios are non-vacuous: temporarily restoring the old
  error reply for dialog/Z4 made `inv_24_sdkctl_scenarios` fail with
  `sdkctl-dialog-silent differential failed: frame 8 differs ... session has
  ended` (the stray `control_response` made fake-claude mismatch). Reverted.

## Note on (f) corpus coverage

No corpus scenario is possible for (f) with the current fake-claude: the SDK's
`readMessages` only replaces the child-exit error on a **transport error**
(non-zero exit / signal), while fake-claude always exits 0 after `exit:true`.
A normal stdout EOF takes the SDK's `done` path (SESSION_ENDED_MESSAGE), which
the existing `eof-mid-turn` corpus already pins. (f) is therefore covered by
units: `error_result_text_matches_sdk_last_error_result_text`,
`child_exit_error_result_wire_shape`, plus the EOF rewrite in `run_loop`.
