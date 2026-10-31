# Turn 1, Group G6a — ROADMAP F4a (spawn options / session creation inputs)

Worktree: `/private/tmp/claude-501/par-c` @ a35bc3b (detached, branch feat/rust-port).
Progress file for the orchestrator: this file only (FEATURES rows flipped in place,
header counts untouched, STATUS.md untouched).

## Checklist

- [x] Read FEATURES.md F4a rows + upstream `acp-agent.ts` regions and SDK `sdk.mjs`
      ProcessTransport argv/env (0.3.303, oracle cache READ-ONLY)
- [x] Read current Rust: sdk_options.rs, process.rs, agent.rs (spawn region),
      system_prompt.rs, settings.rs, env_snapshot.rs
- [x] Failing-first unit tests per option (sdk_options.rs / process.rs /
      system_prompt.rs test modules)
- [x] mcpServers -> `--mcp-config` (stdio/http/sse, headers/env lists, user
      `options.mcpServers` merge; exact Node key order) — WB-13 / O-131
- [x] `_meta.claudeCode.options.{settings(path|object),plugins,sandbox,thinking,
      persistSession,strictMcpConfig,executableArgs,pluginDelivery,
      projectConfigRoot}` — O-49/O-159/O-156/O-158/O-166/O-124/O-62
- [x] `MAX_THINKING_TOKENS` -> thinking + `CLAUDE_MODEL_CONFIG` -> settings tier
      (invalid JSON / non-object throws) — O-62/O-48
- [x] `_meta.systemPrompt` (string replaces; object merged into the preset; ACP
      `systemPrompt` wins) — O-133
- [x] `_meta.disableBuiltInTools` -> `tools: []` — O-134
- [x] argv flags `--thinking=adaptive/disabled`, `--max-thinking-tokens`,
      `--thinking-display`, `--plugin-dir`, `--plugin-dir-no-mcp`,
      `--await-initialize`, `--no-session-persistence`, `--project-config-root`,
      `executableArgs` prefix — O-117/O-118/O-123/O-124/O-156/O-166
- [x] SDK default child env: `CLAUDE_AGENT_SDK_VERSION` when unset, `DEBUG`
      stripped unless `DEBUG_CLAUDE_AGENT_SDK` truthy — O-167
- [x] Session hooks via the programmatic settings tier (`settings.hooks`) —
      O-129/O-130 (the CLI merges the inline `--settings` with the file tiers;
      settings files are untouched)
- [x] Corpus scenarios captured with `bash porting/capture.sh <names>`:
      `mcp-config-stdio`, `mcp-config-http`, `spawn-settings-object`,
      `spawn-plugins`, `spawn-sandbox`, `spawn-thinking`,
      `spawn-persist-false`, `system-prompt-meta-string`,
      `system-prompt-meta-preset`
- [x] `run_and_diff` tests (`inv_57_f4a_spawn_inputs`) + the two meta system
      prompts added to `inv_39_initialize_matches_node` INIT_SCRIPTS
- [x] Flip FEATURES.md rows in place: O-48, O-49, O-62, O-117, O-118, O-123,
      O-124, O-129, O-130, O-131, O-133, O-134, O-156, O-158, O-159, O-166,
      O-167 (all DONE; `[pkg:F4a]` tags dropped from DONE rows)
- [x] fmt / clippy / lib / differential / workspace / gate / JsonPath guard

## Files changed

- `rust/claude-agent-acp-rs/src/sdk_options.rs` — typed parser extended with
  thinking, mcp servers (+ exact-order JSON serialiser), plugins, settings,
  sandbox (`F4`/`U4`), executableArgs, projectConfigRoot, persistSession
  (`Option<bool>`), disableBuiltInTools, pluginDelivery, CLAUDE_MODEL_CONFIG,
  MAX_THINKING_TOKENS; 15 new unit tests.
- `rust/claude-agent-acp-rs/src/process.rs` — `SpawnOptions` fields
  (`thinking`, `mcp_config`, `executable_args`, `plugins`, `plugin_delivery`,
  `persist_session`, `settings_arg`), argv emission in Node order,
  `CLAUDE_AGENT_SDK_VERSION` default + DEBUG normalisation; tests updated.
- `rust/claude-agent-acp-rs/src/agent.rs` — `spawn_inputs` now resolves the
  programmatic settings tier (fallible, Node throws before spawn), thinking
  precedence, plugin validation, and returns `Result`; `session_spawn_options`
  maps the new fields; `initialize_options_for` uses the new
  `system_prompt::initialize_fields`.
- `rust/claude-agent-acp-rs/src/system_prompt.rs` — `_meta.systemPrompt`
  precedence (`meta_system_prompt`, `initialize_fields`) + tests.
- `rust/claude-agent-acp-rs/tests/differential.rs` — `inv_57_f4a_spawn_inputs`.
- `rust/claude-agent-acp-rs/tests/system_prompt.rs` — two meta system-prompt
  scripts added to `INIT_SCRIPTS`.
- `porting/corpus/<9 names>.{acp.json,transcript.jsonl}` + captured
  `porting/fixtures/<name>.{frames.jsonl,argv.json}` (+ `.init.json` for the
  two system-prompt scripts).
- `porting/parity/FEATURES.md` — F4a rows flipped in place.

## Notes / intentional scope choices

- `pluginDelivery:"initialize"` emits `--await-initialize` (argv side). The
  `plugins` field of the initialize control_request lives in
  `control.rs::InitializeOptions`, which belongs to the G3 lane and is
  explicitly out of scope for this worktree; the argv scenario
  (`spawn-plugins`) uses the default `argv` delivery.
- `_meta.systemPrompt` objects with `excludeDynamicSections` cannot be emitted
  for the same reason (`InitializeOptions` has no such field); `append` is
  forwarded and covered by `system-prompt-meta-preset`.
- `serde_json` in this workspace already has `preserve_order` enabled via the
  `agent-client-protocol` dependency tree, so parsed JSON keeps JS insertion
  order; the mcp-server serialiser is nevertheless hand-written to lock the
  SDK's exact `{type,...}` key order for `--mcp-config`.

## Commands run (real output)

- `cargo fmt --manifest-path rust/Cargo.toml --all` -> OK, `-- --check` OK
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` -> OK
- `cargo test -p claude-agent-acp-rs --lib` -> 317 passed
- `cargo test --test differential inv_57_f4a_spawn_inputs` -> 1 passed
- `cargo test --test differential inv_57_argv_matches_node` -> 1 passed
- `cargo test --test system_prompt` -> 1 passed
- `cargo test --workspace` -> all suites green (incl. fake-claude phase2
  fixture portability; the `MCP_TOKEN` name was renamed to avoid the
  TOKEN marker guard)
- `bash rust/scripts/rust-gate.sh` -> `==> gate OK` (fmt, clippy
  `--all-features`, bins, capture prewarm, workspace tests, both cross-target
  checks, G9/G10, parity maps)
- `git diff -U0 455c146 -- ...tests ... | grep '^+' | grep -c 'JsonPath::new'` -> 0
- `bash porting/capture.sh <9 names>` -> captured + normalised

READY:G6a
