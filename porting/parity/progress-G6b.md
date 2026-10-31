# Progress — Group G6b (F4b + FX-1)

READY:G6b

## Scope delivered

### F4b
- **U-10 `emitRawSDKMessages` + `_claude/sdkMessage`**: `sdk_options.rs` parses
  `_meta.claudeCode.emitRawSDKMessages` (`true` / `false` / `SDKMessageFilter[]`)
  into `RawSdkMessages` and implements `should_emit_raw_message`
  (`acp-agent.ts:11841-11853`). `session.rs` forwards every message the SDK query
  yields as `SessionOutbound::ExtNotification("_claude/sdkMessage", {sessionId,
  message})` BEFORE processing it (`acp-agent.ts:5820-5826`); frames the SDK's
  `readMessages` consumes first (control traffic, `keep_alive`,
  `transcript_mirror`, host-only `session_state_changed`) are excluded.
  `agent.rs` forwards the ext notification to the ACP client.
- **U-12 / O-85 `forwardSubagentText`**: `initialize_options_for` now computes
  `clientSupportsSubagents || supportsSubagentTranscript || options.forwardSubagentText`
  (`acp-agent.ts:10947-10953`) and threads it into both the initialize request and
  `MapState::set_forward_subagent_text`, so the stream and consolidated paths use
  one gate.
- **O-84 `perTaskStopAffordance`**: defaults on for the subagents RFD form, a
  client `perTaskStopAffordance` wins; serialized in the initialize request only
  when defined.
- **`enableFileCheckpointing`**: effective value (client option OR an AIR
  `agentFileChangeReport` client) becomes the SDK's
  `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=true` child env var
  (`sdk.mjs`; not an initialize field). Note: the FEATURES row O-136 belongs to
  `[pkg:AG-3]`, so it is NOT flipped here; only the client-tunable half landed.
- **Bundled CLI search (`claudeCliPath`)**: `process.rs` gains
  `bundled_cli_candidates` / `find_bundled_cli` / `resolve_claude_path`
  (`@anthropic-ai/claude-agent-sdk-<plat>-<arch>[-musl]/claude[.exe]`, musl
  preference, ancestor `node_modules` walk) between `CLAUDE_CODE_EXECUTABLE` and
  the PATH fallback. All platform cfg stays in `process.rs`.
- **`resourceNotFound` (-32002)**: a resumed session whose initialize fails with
  "No conversation found with session ID" now answers `-32002
  "Resource not found: <id>"` with `data:{uri}` (`acp-agent.ts:8662-8672`).
  Note: the FEATURES row M-31 belongs to `[pkg:F31]` (it also asks for a
  transcript-existence check), so it is NOT flipped here.

### FX-1 argv flags
All emitted in Node's exact order (`sdk.mjs` ProcessTransport):
`--task-budget` after `--max-budget-usd`; `--json-schema` then
`--debug-file`/`--debug` after `--betas`, before `--permission-prompt-tool`;
`--permission-prompts`, `--continue`, `--resume`, `--channels` in that order;
`--session-mirror` after `--include-partial-messages`, before
`--project-config-root`. `--continue` is suppressed by a `sessionStore`, like the
SDK's `continueConversation` ternary.

**`--channels` deviation (deliberate, Node-authoritative):** the pinned SDK's
`ProcessTransport` has a `--channels` branch but never forwards
`Options.channels` into the transport, so the Node adapter emits NO flag. The
captured `porting/fixtures/spawn-channels.argv.json` proves it. The port keeps the
argv-builder branch (unit-tested via `SpawnOptions.channels`) but never populates
it from `_meta`, so `inv_57_argv_matches_node` stays green with no ignore path.
Row O-127 is flipped DONE with that evidence.

## Files changed
- `rust/claude-agent-acp-rs/src/sdk_options.rs` (parse + `RawSdkMessages`)
- `rust/claude-agent-acp-rs/src/process.rs` (argv flags, bundled CLI resolution)
- `rust/claude-agent-acp-rs/src/agent.rs` (`initialize_options_for`,
  `session_spawn_options`, `resumed_start_error`, ext-notification forwarder)
- `rust/claude-agent-acp-rs/src/control.rs` (`perTaskStopAffordance` +
  `enableFileCheckpointing` fields)
- `rust/claude-agent-acp-rs/src/session.rs` (`SessionOutbound::ExtNotification`,
  raw forwarding hook, `forwardsSubagentText` gate)
- `rust/claude-agent-acp-rs/tests/differential.rs` (2 new tests at the end)
- `rust/claude-agent-acp-rs/tests/system_prompt.rs` (new initialize-fields test)
- `porting/corpus/*` + `porting/fixtures/*` (9 new scenarios, via capture.sh)
- `porting/parity/FEATURES.md` (11 rows flipped in place)
- `porting/parity/TEST-MAP.md` (emitRawSDKMessages + create-session-options rows)

## New tests
- `sdk_options`: `inv_57_argv_fx1_order`, `inv_57_argv_debug_file_wins`,
  `inv_57_argv_session_mirror_order`, `inv_57_argv_channels_sdk_builder`,
  `inv_57_fx1_and_f4b_option_parsing`, `inv_57_raw_sdk_message_filter`.
- `process`: `bundled_cli_candidates_match_node`,
  `find_bundled_cli_walks_node_modules`, `resolve_claude_path_precedence`.
- `agent`: `inv_57_initialize_fields_from_client`,
  `inv_57_session_spawn_options_fx1_and_channels_parity`,
  `inv_57_resumed_start_error_maps_resource_not_found`.
- `control`: `initialize_request_carries_per_task_stop_affordance`.
- `session`: `inv_raw_forwarding_skips_consumed_frames`.
- `tests/differential.rs`: `inv_24_f4b_spawn_flags`, `inv_24_raw_sdk_messages`.
- `tests/system_prompt.rs`: `inv_39_initialize_fields_matches_node`.

## Scenarios captured (Node oracle)
`spawn-continue`, `spawn-task-budget`, `spawn-json-schema`, `spawn-debug`,
`spawn-session-mirror`, `spawn-channels`, `spawn-permission-prompts`,
`initialize-fields` (capture_init), `raw-sdk-messages`.

`spawn-debug` exercises `debug:true` (`--debug`); the `debugFile` branch is covered
by the unit test `inv_57_argv_debug_file_wins` (a second corpus scenario would add
no Node-side coverage). `--channels` cannot be expressed as a Node-emitted flag
(see above), so its parity is pinned by the `spawn-channels` fixture + unit test.

## Acceptance (real output)
- `cargo fmt --all --check` → FMT CLEAN.
- `cargo clippy --workspace --all-targets -- -D warnings` → clean.
- `cargo test -p claude-agent-acp-rs --lib` → 468 passed, 0 failed.
- `cargo test --test differential` → 82 passed, 1 ignored (the pre-existing
  HAZARD-IMPL T-21-todowrite ignore), 0 failed.
- `cargo test --workspace` → all green (and again under the polluted env prefix:
  CLAUDE_CODE_ARTIFACT/ANTHROPIC_MODEL/... → all green).
- `bash rust/scripts/rust-gate.sh` → `==> gate OK` (TEST-MAP PASS, MODULE-MAP
  PASS, both cross-target checks OK).
- JsonPath guard: `git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests
  rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'` → `0`.
- `grep -rn 'cfg(unix)\|cfg(windows)\|target_os' rust/claude-agent-acp-rs/src |
  grep -v process.rs` → nothing.
- No new `#[ignore]`, no normaliser weakening, no new ignore paths.

## Notes / residual
- `--debug-file` from the SDK's internal debug-log path (`Y1()`) is not ported:
  it depends on SDK-internal state and has no adapter-visible effect.
- `taskBudget.total` / `debug` / `sessionStore` use JS truthiness, matching the
  SDK's `if (value)` gates; an invalid `emitRawSDKMessages` value is treated as
  off (Node would throw inside the message loop).
- `ClaudeOptions.channels` is parsed but not mapped to `SpawnOptions` (Node
  parity, see above).
