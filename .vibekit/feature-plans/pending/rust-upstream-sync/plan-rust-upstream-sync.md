---
title: "rust-upstream-sync: bring the Rust port from upstream 0.70.0 to 0.94.0, pin the mirrored version, add a bump/adoption mechanism"
feature: rust-upstream-sync
worktree: /Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1
research: .vibekit/feature-plans/pending/rust-upstream-sync/research-upstream-0.70-to-0.94.md
---

<!--
RULES — read before writing or implementing:
1. FORMAT: Bullets, tables, code, diagrams ONLY — no prose paragraphs
2. REQUIREMENTS: One crisp line each — no verbose descriptions
3. CHECKLIST: Mark items [x] as you complete them — this is your persistent todo list
4. READING TIME: Optimize for fast human scanning — if it's hard to skim, rewrite it
-->

## Goal

- Rust port mirrors npm `@agentclientprotocol/claude-agent-acp` 0.94.0 (SDK 0.3.303) instead of 0.70.0, on the ported subset, differential green with NO new ignore-paths
- The mirrored npm version lives in ONE file, `porting/UPSTREAM_VERSION`, read by `build.rs` and shell scripts; binary reports `<crate>+upstream.<npm>` in `--version` and `initialize.agentInfo.version`
- Adoption mechanism: `rust/scripts/upstream-diff.sh`, `rust/scripts/upstream-ahead.sh` (gate tail, non-failing), `porting/UPSTREAM-BUMP.md`
- Fixtures are captured from `npm install @agentclientprotocol/claude-agent-acp@<pin>` in a git-ignored cache, not from the repo's own `src/`
- Done = 3 phases verified (`bash rust/scripts/rust-gate.sh` ends `gate OK` + differential suite green at every phase boundary), `PARITY.md` / `INVARIANTS.md` current, every skipped upstream behaviour recorded

## Hard constraints (every phase)

- Only `rust/`, `porting/`, `skills/`, `.vibekit/` and the exempt root `.gitignore` may change — NEVER `src/`, `dist/`, `package.json`, `package-lock.json`, `scripts/`, `.github/`, `docs/`, root `*.md` (guard G7)
- Never run `npm` in the repo root (`npm ci`, `npm run build`): the only allowed npm use is `npm install --prefix <dir>` / `npm ci --prefix <dir>` into a git-ignored cache or temp dir (`--omit=optional` for lock creation), plus `npm view` / `npm pack` inside `rust/target/`
- G9: no `cfg(unix|windows|target_*)` outside `rust/claude-agent-acp-rs/src/process.rs`; G10: `agent-client-protocol` stays at 2.1.0 (enabling a cargo FEATURE of that same version is allowed; do not edit `Cargo.lock` by hand)
- Never print, log, echo or write environment variable VALUES. `*.argv.json` keeps using `ARGV_ENV_ALLOWLIST` (`rust/fake-claude/src/lib.rs:141`); scrub env by NAME only
- `porting/record-real.sh` is human-only: never run it
- Do NOT commit, push, squash, tag, `git stash`; do not kill processes by name; do not touch any real vibe-station daemon
- Scripts must run on macOS bash 3.2: no `mapfile`, no associative arrays, no `${v,,}`; guard empty arrays under `set -u` as `${arr[@]+"${arr[@]}"}`; there is NO `timeout` binary on macOS (scripts use npm `--fetch-timeout`; `rust-gate.sh` gets a portable fallback, item 1.12)
- Cargo always `--manifest-path rust/Cargo.toml`; filter test output with `grep -E "test result|FAILED"`; success = `test result: ok` / `gate OK`
- Every test runs under an explicit timeout (G3); new tests named `inv_NN_<slug>` (next free NN = 35; this plan uses 35-48)
- Escape valve (Decision 18): an item with NO corpus-visible effect may be deferred ONLY by a `pending` row (reason + consequence) in `porting/PARITY.md`. A divergence visible in a regenerated fixture MUST be ported; if it cannot be, STOP and report to the orchestrator — a `pending` row cannot turn a red differential green, and ignore-paths / weakened differentials / skipped fixtures are forbidden
- Gate output must END with `==> gate OK` (Decision 6 keeps `upstream-ahead.sh` output before it)

## Change Map

```
porting/
  upstream-locks/<ver>.package-lock.json + committed lockfile of the cache install
  UPSTREAM_VERSION            + single source of truth: "0.70.0" (P1) -> "0.94.0" (P2)
  UPSTREAM-BUMP.md            + step-by-step bump runbook
  SYNC.md                     ~ replaced by pointer to UPSTREAM-BUMP.md
  capture.sh                  ~ npm-install cache + env scrub (no root build)
  corpus/ fixtures/           ~ transcripts fixed, fixtures regenerated, +4 scripts
  divergence-0.94.0-vs-0.70.0.txt  + research artifact (P1)
  PARITY.md INVARIANTS.md     ~ current
rust/scripts/
  upstream-diff.sh            + npm-tarball + changelog + dist diff
  upstream-ahead.sh           + "upstream is N versions ahead" (never fails)
  upstream-divergence.sh      + capture <ver> vs committed fixtures
  json-diff.mjs               + structural frame differ used by the above
  rust-gate.sh                ~ portable timeout, cache prewarm, upstream-ahead before "gate OK"
  selftest-gate.sh            ~ copies UPSTREAM_VERSION into its temp tree
rust/claude-agent-acp-rs/
  build.rs                    + composes ACP_VERSION_STRING
  Cargo.toml                  ~ version 0.1.0, publish=false, feature unstable_tool_call_name
rust/Cargo.lock               ~ crate version rewritten by cargo
rust/AGENTS.md                ~ wire-truth wording
  src/lib.rs                  ~ pub const VERSION / UPSTREAM_VERSION
  src/bin/claude-agent-acp-rs.rs ~ --version
  src/agent.rs                ~ initialize response, AIR gate, commands, quota, cancel, systemPrompt
  src/air.rs                  + AIR client-capability parser
  src/async_tasks.rs          + AIR async task runtime
  src/map.rs src/tools.rs     ~ tool_call name, commands list, AIR Bash meta
  src/permission.rs           ~ Yes/No options, new control_response fields
  src/process.rs              ~ "--flag=value" argv, new env var
  src/control.rs              ~ initialize options, detached request
  src/session.rs src/turn.rs  ~ cancel_async_message, quota, unfinished tools, async tasks, folded prompt
  src/turn/turn_tests.rs      ~ ordered steered echoes, folded-prompt tests
  tests/*.rs                  ~ version.rs, differential.rs, common/mod.rs, process_transport.rs
rust/fake-claude/src/lib.rs   ~ auto-ack get_context_usage; allowlist
rust/acp-recorder/src/        ~ initialize clientCapabilities, async-task stop step
```

| Today | After this plan |
|-------|-----------------|
| Version `0.1.0+acp.0.70.0` hand-edited in `Cargo.toml:3`; `agentInfo.version` hard-coded `"0.70.0"` (`agent.rs:33`) | `porting/UPSTREAM_VERSION` -> `build.rs` -> `0.1.0+upstream.<pin>` in `--version` and `agentInfo.version` |
| Fixtures built from repo `src/` (root `npm ci` + tsc) in `capture.sh:43-48` | Fixtures from `npm install …@<pin>` in `rust/target/upstream/<pin>/`, host env scrubbed |
| No way to see what upstream changed | `upstream-diff.sh <from> [<to>]`, `upstream-ahead.sh` at gate tail |
| `initialize` hard-codes AIR + goal meta for every client | AIR meta only for a client declaring `_meta.jetbrains.air`; `authStatus`, `systemPrompt` markers |
| Permission options Deny / Allow Once / Always Allow | `allow-once "Yes"` / `reject "No"` + exact Bash durable option |
| Tool updates repeat unchanged fields; non-AIR `_meta` carries `subagent`/`title`/`skill` | Field tracker drops unchanged fields; non-AIR `_meta` = `{claudeCode:{toolName}}` |
| Cancel interrupts only | Cancel first sends `cancel_async_message` per abandoned queued prompt/steer |
| No async-task updates | AIR clients get `async_task_spawned/progress/state_update` |
| Turn ends `end_turn` with unresulted tool calls | Fails `-32603` `incomplete_tool_call` like upstream |

## Research digest (full table: research file)

- R-A Empirical baseline (research §1): the 21-script corpus captured on 0.94.0 differs from the 0.70.0 fixtures by exactly E-1..E-12; `npm install`ed 0.70.0 reproduces all 43 committed fixtures byte-for-byte once the adapter runs under an `env -i` allowlist (F1, F2)
- R-B 144 inventory rows: 68 SKIP, 35 PORT-E, 30 CLI, 6 PORT-N, 5 informational; 17 rows flagged HIGH RISK (research §2, §3)
- R-C `fake-claude` exits on the first unexpected stdin frame (`rust/fake-claude/src/lib.rs:320-327`): 0.94 sends `get_context_usage` (auto-ack, Decision 5) and `cancel_async_message` (transcript step, Decision 9)
- R-D AIR extensions apply only to clients declaring `_meta.jetbrains.air` (`dist/air-extension.js:90-154`); the recorder's `initialize` declares none (`rust/acp-recorder/src/recorder.rs:193-196`)
- R-E `agent-client-protocol` 2.1.0 has cargo feature `unstable_tool_call_name` (`agent-client-protocol-2.1.0/Cargo.toml:53,62`) giving `ToolCall::name(..)`; the permission request and `session/update` for AIR custom variants are built from raw JSON (`C/permission.rs:278`, `C/agent.rs:510-516` `UntypedMessage`)

## Invariants to preserve (explicit)

- INV-30 orphan credit consumed BEFORE queue-head promotion (`C/turn.rs:970` `ensure_active_turn`) — do not reorder
- INV-31 / #825 idle without result fails an UN-steered active turn; steered turns never hit it (`C/turn.rs:829` `on_idle`)
- INV-23 `reconcile_orphan_receipt` touches only `orphaned_uuids` (`C/turn.rs:496`); steered uuids are never pushed there
- `owed_trailing_idles` debt accounting and `had_usage` origin flag (`TurnEvent::Settled`, `C/turn.rs:186-205`): only the cancel queue sweep is `had_usage:false`
- Steering lanes (`C/turn.rs:343` `mark_steered`, `steered_echoes/steered_settle`) behave byte-for-byte as today unless a Phase-3 item says otherwise
- Held (subagent-drain) turns: `is_held_turn` keeps meaning "deferred_settle set"
- INV-27 no actor awaits a client round-trip inline: every new control request is detached or spawned
- Tool-call-before-permission ordering (INV-20) and tool_call-before-result flush (`SessionOutbound::Flush`, `C/session.rs:98`)
- Un-gated, un-steered, non-AIR code paths change only where a Phase item names the divergence (E-n / R-n)

## Key Decisions

1. Single pin file `porting/UPSTREAM_VERSION` (one line, `^[0-9]+\.[0-9]+\.[0-9]+$`); readers: `build.rs`, `capture.sh`, both upstream scripts, `UPSTREAM-BUMP.md`. Env `UPSTREAM_VERSION` overrides it for `capture.sh`/scripts (used by the divergence report; never persisted). — research §1 F2
2. `build.rs` (`rust/claude-agent-acp-rs/build.rs`) reads `$CARGO_MANIFEST_DIR/../../porting/UPSTREAM_VERSION`, emits `cargo:rerun-if-changed`, `cargo:rustc-env=UPSTREAM_NPM_VERSION=<pin>` and `cargo:rustc-env=ACP_VERSION_STRING=<CARGO_PKG_VERSION>+upstream.<pin>`; panics with a clear message on missing/invalid file. `Cargo.toml` version becomes plain `0.1.0`. `selftest-gate.sh` must copy the pin file (its temp tree has no `porting/`).
3. agentInfo: `name`/`title` stay exactly Node's; only `version` = composed string. The differential did NOT ignore `agentInfo` (`tests/differential.rs:256-263`), so a transform-and-assert normalizer is needed: `normalize_agent_info_version(rust_frames, fixture_frames, pin)` in `tests/common/mod.rs` (1) asserts the Node FIXTURE's `result.agentInfo.version` == the pin, (2) rewrites that fixture value to `format!("{CARGO_PKG_VERSION}+upstream.{pin}")`, (3) compares Rust's value EXACTLY (no rewrite), (4) asserts exactly one frame per side carried an `agentInfo` (so it cannot pass vacuously). Adds NO `JsonPath`; recorded in PARITY as a deliberate, user-required divergence. — OQ-2
4. Capture path: `capture.sh` installs the pin into `${UPSTREAM_CACHE_DIR:-rust/target/upstream}/<pin>/` (git-ignored via `rust/.gitignore:1 target/`) ATOMICALLY (install into `<dir>.tmp.$$`, assert version, `mv` into place — a half-finished install must never satisfy the existence check), from a committed lockfile `porting/upstream-locks/<pin>.package-lock.json` (`npm ci --ignore-scripts --omit=optional --omit=dev`; if absent, `npm install` once and copy the generated lock there for the implementer to commit) so transitive deps cannot drift (69 MB; `--omit=optional` drops 270 MB of native binaries, no capture byte changes). The adapter runs under `env -i` with an ALLOWLIST (`PATH`, `TMPDIR`, `LANG`, `LC_*`) plus the explicit `HOME`, `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_EXECUTABLE`, `FAKE_CLAUDE_*` assignments — unsetting only `ANTHROPIC*`/`CLAUDE*` is NOT enough (0.94 also reads `SSH_CONNECTION|SSH_CLIENT|SSH_TTY|NO_BROWSER` (authMethods), `MAX_THINKING_TOKENS` (initialize), `IS_SANDBOX`, `NODE_OPTIONS`); verified: an `env -i` allowlist capture of 0.70.0 reproduces all 43 fixtures. `UPSTREAM_VERSION` env is REFUSED unless `CAPTURE_OUT_DIR` is also set (it must never overwrite committed fixtures with another version). If the cache dir is outside `$ROOT`, `normPath` also maps it to `$UPSTREAM`. `*.argv.json` stays on `ARGV_ENV_ALLOWLIST`. — research §1 F1, F2
5. `fake-claude` auto-acks `control_request{subtype:get_context_usage}` with `{"type":"control_response","response":{"subtype":"success","request_id":<id>,"response":{}}}` WITHOUT consuming a transcript step, only when it does not match the current step's `expect`. Rust never sends it (usage_update is a discarded variant). — E-7
6. `upstream-ahead.sh` runs inside `rust-gate.sh` BEFORE the final `echo "==> gate OK"` line, as `bash … || true`, so the gate's last line stays `gate OK` and its exit status is untouched. It always exits 0.
7. Unfinished-tool failure (E-12, Phase 3) is a post-filter in `session.rs::handle_events` on EVERY `TurnEvent::Settled{EndTurn}` (active, held, deferred, steered — Node checks all end_turn settles, `dist/acp-agent.js:3559-3605`), NOT a change to `TurnMachine` lanes: `settle_with_usage` and `fail_active` keep identical books (`C/turn.rs:1079-1119`, debt recorded in `on_result` first). The foreground tool set is KEYED BY `prompt_uuid` because `ensure_active_turn` emits the held turn's `Settled` and the next turn's `Activated` in one batch (`C/turn.rs:977-995`). Background-task parent tool ids (`live_background`, `C/session.rs:826`) are exempt.
8. Phase-2 transcripts get realistic `tool_result` echoes (E-12 would otherwise fail 7 scripts); this is a corpus fix, not a weakening: every touched transcript is listed in item 2.2 and the new failure path gets its own script (`unfinished-tool`, Phase 3).
9. Cancel (E-8): `TurnMachine::take_abandoned()` returns Node's order — pending steered echoes in insertion order (so `steered_echoes` becomes insertion-ordered, `C/turn.rs:237`), then swept queued turns FIFO (`dist/acp-agent.js:6253-6264`); the Cancel arm sends one `cancel_async_message` per uuid via a NEW synchronous `Control::send_request_detached` (enqueue on the single unbounded writer, `C/control.rs:195-205`; call order preserved; INV-27 holds) after arming the backstop and BEFORE spawning the `interrupt` task (`dist/acp-agent.js:6455-6457`). Transcripts contain the step so a missing message is a loud fake-claude failure; the exact uuid sequence is asserted by `inv_38`.
10. Permissions (E-10, E-11): Phase 2 offers `allow-once "Yes"` + `reject "No"`; Phase 3 (3.9, non-deferrable) adds the exact Bash `allow-with-updates` option and falls back to Yes/No wherever Node cannot describe the whole effect (real `can_use_tool` requests carry `permission_suggestions`, so a Yes/No-only port would be a user-visible regression). Accepted option ids: `allow-once`, `allow-with-updates`, `reject`; anything else = deny. — OQ-3
11. AIR: parse once at `initialize` into `AirClient{version, capabilities}` (`C/air.rs`), store on the connection `Registry`, hand to each `Session`. Non-AIR clients get no `_meta.jetbrains`, no goal, no async-task updates. AIR advertisement = Node's list verbatim (research X-2) even for capabilities Rust does not implement; each unimplemented one is a `pending` PARITY row naming its consequence, and all are listed in the final report for explicit user sign-off. — OQ-1
12. Async tasks (R71-5): a Rust port of the subset of `dist/async-tasks.js` needed by the AIR corpus: spawn held until a tool-call id is known (or released at the prompt result / terminal state), progress dedupe by `changedFields`, state updates, ignored `local_agent`/`local_monitor`/Monitor-tool tasks, finish-all at shutdown. Updates leave through a NEW `SessionOutbound::RawUpdate(Value)` sent as an untyped `session/update` (`UntypedMessage`, same mechanism as `C/agent.rs:510-516`).
13. Differential scripts for new behaviour are generated by `capture.sh` from Node 0.94.0 (never hand-written fixtures); if Node's capture shows no `async_task_*` frame, STOP and fix the transcript fields (`task_id`, `task_type`, `tool_use_id`, `output_file` — both snake_case and camelCase accepted, `dist/async-tasks.js:field()`), not the expectation.
14. SKIP rows stay SKIP: v2 surface (~30 commits), session index, native subagents, auth/gateway/models/config, elicitation, hooks, terminal, replay, forks, notices (research §2). They are recorded in PARITY section B with their release range; nothing is silently dropped.
15. (Phase 1 deviations — Phase 1 complete 2026-10-22)
    - `inv_35_agent_info_normalizer_rejects_stale_pin` (item 1.6) is colocated in `tests/version.rs` (the inv_35 home) rather than in `tests/common/mod.rs`, so the `differential` binary stays at EXACTLY 22 tests (T6) and does not pick up an extra shared `#[cfg(test)]` test. Consequence: `--test version` reports 3 passed (2 named in 1.15 + this unit test), not 2 — T1's "2 pass" is superseded by "3 pass"; all other T-counts hold exactly.
    - Gate requires the `x86_64-pc-windows-gnu` cross-check target; on this host it was absent, so `rustup target add x86_64-pc-windows-gnu` was installed to let the gate reach `gate OK`. No repo file changes from this.
    - Item 1.13 ran against live npm (network available) and produced the real `porting/divergence-0.94.0-vs-0.70.0.txt` (746 lines, 43 fixtures, E-1..E-12 families) rather than a copy of research §1.
16. (Phase 2 deviation notes go here)
    - `inv_36_commands_append_mcp_and_remote_control` (2.5): for the synthetic "CLI already offers a non-terminal `remote-control`" case, the CLI's `remote-control` stays in its original position and only `mcp` is appended after it, so the expected list is `["remote-control","mcp"]` (not `["mcp","remote-control"]`). Both satisfy Node's "no duplicate remote-control"; the real `commands-proactive` fixture (the ground truth) passes on the commands exactly. Matches Node's `getAvailableSlashCommands` which re-appends only `mcp` at the end when the CLI already owns `remote-control`.
    - 2.12 quota: `had_usage` in `TurnEvent::Settled` is matched by ref (`&bool`), so the quota guard is `if *had_usage` in `handle_events`.
    - Node 0.94.0 spawns a second `claude auth status --json` probe process that overwrote the shared FAKE_CLAUDE_ARGV_OUT record (resume-load.argv.json lost `--resume=`); fixed in rust/fake-claude/src/lib.rs `record_argv_env` so a non-session invocation never clobbers the session argv, with tests for both start orders.
    - Node's error tool_result content uses a plain ``` fence (no `console` tag) — ported in tools.rs `tool_result_content`.
    - Tests asserting the old two-token argv form (`--model X`, `--permission-mode Y`) in tests/agent.rs, fake-claude/tests/phase2.rs and C/process.rs were updated to the `--name=value` form (item 2.6).
17. (Phase 3 deviations)
    - Resume turn completed 3.7, 3.8, 3.9 and 3.12; 3.10, 3.11 and 3.13 are `pending` PARITY rows (3.10 touches `TurnMachine::on_result` accounting; 3.11 has no Node oracle for the SDK initialize `systemPrompt`/`appendSystemPrompt` wire fields). Differential: 27 tests (22 + async-task-air/non-air + unfinished-tool + folded-prompt + permission-suggestions).
    - 3.7 keeps its books in `MapState.books` (`unfinished.rs`) instead of "the session actor" so no `handle_session_frame` signature changes; the check runs in `settle_events` ahead of `handle_events`. For `incomplete_tool_call` the prompt rejection waits for a `Flush` barrier (the closing `tool_call_update` must precede the `-32603` on the wire, as in Node). Cancel-time closing of open calls (cancel family) is NOT ported (PARITY row).
    - 3.8 follows Node's `answersPendingPrompt` (ANY unsettled, non-held turn named in `user_message_uuids`/`user_message_uuid`), wider than the plan's "active turn or queue head"; `TurnMachine::result_is_autonomous` is read-only and `session.rs` uses it to clear `emitted_assistant_text`. `inv_42_*` tests were APPENDED to `turn_tests.rs`; no existing test changed.
    - 3.9 fixed a latent bug found by the new script: `permission::map_outcome` read a flat `{outcome:"selected",optionId}` but the wire is `result.outcome.{outcome,optionId}`, so every allow selection had been mapped to a deny (the old transcripts never `expect`ed the `control_response`). Durable options cover Bash/PowerShell commands-only suggestions; read-path / directory forms and non-shell tools fall back to Yes/No (PARITY rows). `default_to_no` option order is not ported.
    - 3.12 counts only permissions (no elicitations are ported) toward `later`.
    - Opus-review follow-ups: `permission-allow`/`permission-deny` transcripts now `expect` `response.response.behavior` (`allow`/`deny`); fixtures regenerated byte-identical, and reverting the `map_outcome` nesting fix makes `inv_24_permissions` fail. Cancel / force-cancel now go through `settle_events` (books dropped, `inv_41_cancel_paths_do_not_leak_books`); the tool tracker is cleared only on non-autonomous results (`autonomous_result_keeps_tool_tracker`); `inv_42_held_turn_naming_result_stays_autonomous` added. PARITY rows added: `_session/async_task/stop` (-32601, canStop:true kept), async-task detail gaps, sessionFailure-client `incomplete_tool_call` shape.
    - `git fetch upstream --tags` was run while verifying UPSTREAM-BUMP.md (adds tags only).
18. Escape valve = Hard-constraints rule. Phase 2 workflow: transcripts, pin bump + capture FIRST (differential turns red), then port from the red list; the differential is intentionally red between 2.3 and 2.14.
19. Field tracker (X-7, B1) and non-AIR `_meta` shrink (X-9, B2) are corpus-visible (`streamed-partial-input`, `subagent-drain/followup`) and therefore Phase-2 non-deferrable; evidence: Node 0.94 drops `kind` / `kind,title,content` from partial-input `tool_call_update`s and `_meta.claudeCode.subagent` from subagent `tool_call`s.
20. Phase-2 quota is totals only, computed in `session.rs` from the existing `Usage`; the only Phase-2 `turn.rs` changes are the insertion-ordered `steered_echoes` and `take_abandoned`. Per-model rows are Phase 3 (deferrable; the corpus always has `model_usage: []`).
21. `/mcp` is forwarded to the CLI as a plain prompt and `/remote-control` is `skipped-deliberate`; both are still advertised in `available_commands_update` for list parity (OQ-9). Subagent transcripts (E-19) are NOT made real (OQ-15).
22. Gate hygiene (M3): `rust-gate.sh` runs a cache PREWARM (`bash porting/capture.sh --install-only`) before `cargo test`, because `capture_is_deterministic_across_runs` (`rust/fake-claude/tests/phase2.rs:38`) runs `capture.sh` inside the gate's 180 s test timeout; the install itself must never happen inside the timed test.

## Implementation Phase 1 — pin file, version string, hermetic capture, adoption scripts (pin stays 0.70.0)

Sized for one implementer turn. Touches `porting/`, `rust/scripts/`, `rust/fake-claude`, build/version plumbing. NO behavioural change to the agent. Order = risk order; the ONLY deferrable item is 1.13 (divergence report).

### Checklist

- [x] 1.0 Read `rust/AGENTS.md`, `rust/scripts/rust-gate.sh`, `rust/scripts/selftest-gate.sh`, `porting/capture.sh`, `rust/fake-claude/src/lib.rs:120-200,250-345`, research §1, Decisions 1-6
- [x] 1.1 Create `porting/UPSTREAM_VERSION` containing exactly `0.70.0` + newline
- [x] 1.2 Create `rust/claude-agent-acp-rs/build.rs` per Contract A; `Cargo.toml:3` version `"0.1.0"`, add `publish = false`, `:5` description drops "v0.70.0" (say "pinned by porting/UPSTREAM_VERSION")
- [x] 1.3 `rust/claude-agent-acp-rs/src/lib.rs:1-22` add `pub const VERSION: &str = env!("ACP_VERSION_STRING");` and `pub const UPSTREAM_VERSION: &str = env!("UPSTREAM_NPM_VERSION");`
- [x] 1.4 `rust/claude-agent-acp-rs/src/agent.rs:33` replace the `AGENT_INFO` string constant with `fn agent_info() -> Value` returning `json!({"name":"@agentclientprotocol/claude-agent-acp","title":"Claude Agent","version": crate::VERSION})`; update `:94` and the doc comment `:73-74`
- [x] 1.5 `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs:15` before `serve`: if any arg in `std::env::args().skip(1)` is `--version`, `-V` or `-v`, write `VERSION` + `\n` to stdout via `std::io::Write` and `return Ok(())` (stdout otherwise carries only JSON-RPC)
- [x] 1.6 `tests/common/mod.rs` add `pub fn normalize_agent_info_version(rust: Vec<Frame>, fixture: Vec<Frame>, pin: &str) -> (Vec<Frame>, Vec<Frame>)` implementing Decision 3 exactly (assert fixture version == pin; rewrite fixture value to `format!("{}+upstream.{pin}", env!("CARGO_PKG_VERSION"))`; Rust value compared unchanged; assert one `agentInfo` frame per side) and `pub fn pinned_upstream_version() -> String` (reads `porting/UPSTREAM_VERSION`, trimmed). Wire it at BOTH diff sites `tests/differential.rs:359` and `:398`. Add NO `JsonPath::new` anywhere; unit test `inv_35_agent_info_normalizer_rejects_stale_pin`
- [x] 1.7 `rust/scripts/selftest-gate.sh:17-20` copy `porting/UPSTREAM_VERSION` into `$TMP/porting/UPSTREAM_VERSION` (mkdir first) so the temp tree still builds
- [x] 1.8 `rust/fake-claude/src/lib.rs` replay loop (`:312-327`): before the `subset_match` failure return, if the frame is a `control_request` whose `request.subtype` is in `const AUTO_ACK_SUBTYPES: [&str; 1] = ["get_context_usage"]` and `!subset_match(&step.expect,&frame)`: write the Decision-5 response line, flush, `continue` (no step consumed); also in the drain branch (`:268-283`). Log under `FAKE_CLAUDE_LOG_FRAMES` like other frames
- [x] 1.9 `rust/fake-claude/src/lib.rs:141` leave `ARGV_ENV_ALLOWLIST` unchanged in Phase 1 (extended in 2.6)
- [x] 1.10 Rewrite `porting/capture.sh` build section (`:35-52`) per Contract B and Decision 4: read pin (`UPSTREAM_VERSION` env — refused unless `CAPTURE_OUT_DIR` is set — else file; validate regex), cache dir, atomic lockfile install, version assertion, `DIST` from cache (`UPSTREAM_DIST` overrides), keep `cargo build` of rust bins; new flag `--install-only` (install + assert, then exit 0, used by the gate prewarm); run the recorder under `env -i` allowlist (`:163-169`); add the `$UPSTREAM` placeholder to `normPath` in both normalizers only when the cache dir is outside `$ROOT`; header comment updated. Do NOT otherwise touch the normalizers
- [x] 1.11 Create `rust/scripts/json-diff.mjs`, `rust/scripts/upstream-diff.sh`, `rust/scripts/upstream-ahead.sh`, `rust/scripts/upstream-divergence.sh` per Contract C; `chmod +x`
- [x] 1.12 `rust/scripts/rust-gate.sh`: (a) define `run_timeout() { secs=$1; shift; if command -v timeout >/dev/null 2>&1; then timeout "$secs" "$@"; elif command -v gtimeout >/dev/null 2>&1; then gtimeout "$secs" "$@"; else perl -e 'alarm shift; exec @ARGV' "$secs" "$@"; fi; }` and use it at `:23` (`run_timeout 180 cargo test …`) — macOS has no `timeout`, so `gate OK` is otherwise unreachable there; (b) before the `==> test` step add `echo "==> prewarm upstream cache"; bash "$ROOT/porting/capture.sh" --install-only` (Decision 22); (c) before `echo "==> gate OK"` insert `bash "$RUST/scripts/upstream-ahead.sh" || true` (Decision 6)
- [x] 1.13 (deferrable) Run `bash rust/scripts/upstream-divergence.sh 0.94.0 > porting/divergence-0.94.0-vs-0.70.0.txt` (stdout only; expect the E-1..E-12 families of research §1; commit as research artifact). If it cannot run (offline), copy research §1 instead and note it
- [x] 1.14 Create `porting/UPSTREAM-BUMP.md` (Contract D) and replace the body of `porting/SYNC.md` with a 5-line pointer to it (the old `+acp.0.N` workflow is obsolete)
- [x] 1.15 Tests (write first, names exact): `tests/version.rs` `inv_35_version_string_composed_from_pin` (runs `env!("CARGO_BIN_EXE_claude-agent-acp-rs") --version`; asserts stdout == `format!("0.1.0+upstream.{}\n", pin)` where pin is read from `porting/UPSTREAM_VERSION`) and `inv_35_agent_info_version_is_composed` (`VERSION` ends with `+upstream.` + `UPSTREAM_VERSION`); fake-claude `auto_acks_get_context_usage_without_consuming_a_step` and `explicit_expect_wins_over_auto_ack` in `rust/fake-claude/tests/`
- [x] 1.16 Update this plan's checklist; record deviations under Key Decision 15

### Contracts

**A — `build.rs`**
```rust
use std::{env, fs, path::Path};
fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| panic!("CARGO_MANIFEST_DIR unset"));
    let pin = Path::new(&dir).join("../../porting/UPSTREAM_VERSION");
    println!("cargo:rerun-if-changed={}", pin.display());
    let raw = fs::read_to_string(&pin).unwrap_or_else(|e| panic!("read {}: {e}", pin.display()));
    let v = raw.trim();
    let ok = v.split('.').count() == 3 && v.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !ok { panic!("porting/UPSTREAM_VERSION must be MAJOR.MINOR.PATCH, got {v:?}"); }
    let krate = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| panic!("CARGO_PKG_VERSION unset"));
    println!("cargo:rustc-env=UPSTREAM_NPM_VERSION={v}");
    println!("cargo:rustc-env=ACP_VERSION_STRING={krate}+upstream.{v}");
}
```

**B — `capture.sh` build section (shape)**
```bash
PIN_FILE="$ROOT/porting/UPSTREAM_VERSION"
if [ -n "${UPSTREAM_VERSION:-}" ] && [ -z "${CAPTURE_OUT_DIR:-}" ]; then
  echo "UPSTREAM_VERSION override requires CAPTURE_OUT_DIR (would overwrite committed fixtures)" >&2; exit 1; fi
PIN="${UPSTREAM_VERSION:-$(tr -d '[:space:]' < "$PIN_FILE")}"
case "$PIN" in [0-9]*.[0-9]*.[0-9]*) ;; *) echo "bad pin" >&2; exit 1;; esac
CACHE="${UPSTREAM_CACHE_DIR:-$ROOT/rust/target/upstream}/$PIN"
PKG="$CACHE/node_modules/@agentclientprotocol/claude-agent-acp"
DIST="${UPSTREAM_DIST:-$PKG/dist/index.js}"
LOCK="$ROOT/porting/upstream-locks/$PIN.package-lock.json"
if [ ! -f "$CACHE/.complete" ]; then            # marker written LAST, after the mv
  TMP="$CACHE.tmp.$$"; rm -rf "$TMP"; mkdir -p "$TMP"
  printf '{"private":true,"dependencies":{"@agentclientprotocol/claude-agent-acp":"%s"}}\n' "$PIN" > "$TMP/package.json"
  if [ -f "$LOCK" ]; then cp "$LOCK" "$TMP/package-lock.json"; CMD=ci; else CMD=install; fi
  npm "$CMD" --prefix "$TMP" --ignore-scripts --omit=optional --omit=dev --no-audit --no-fund >/dev/null
  GOT="$(node -p "JSON.parse(require('fs').readFileSync('$TMP/node_modules/@agentclientprotocol/claude-agent-acp/package.json','utf8')).version")"
  [ "$GOT" = "$PIN" ] || { echo "installed $GOT, pin is $PIN" >&2; rm -rf "$TMP"; exit 1; }
  [ -f "$LOCK" ] || { mkdir -p "$(dirname "$LOCK")"; cp "$TMP/package-lock.json" "$LOCK"; }
  rm -rf "$CACHE"; mkdir -p "$(dirname "$CACHE")"; mv "$TMP" "$CACHE"; : > "$CACHE/.complete"
fi
[ "${1:-}" = "--install-only" ] && exit 0
```
- the recorder call becomes (names only, no env values ever printed):
```bash
env -i PATH="$PATH" TMPDIR="${TMPDIR:-/tmp}" LANG="${LANG:-C}" \
  HOME="$FAKE_HOME" CLAUDE_CONFIG_DIR="$FAKE_HOME/.claude" CLAUDE_CODE_EXECUTABLE="$FAKE" \
  FAKE_CLAUDE_SCRIPT="$transcript" FAKE_CLAUDE_ARGV_OUT="$argv_raw" FAKE_CLAUDE_INIT_OUT="$init_raw" \
  "$RECORDER" "node $DIST" "$script" "$raw"
```
- forward `LC_*` only if set (loop over `compgen -e` names matching `LC_*`); do NOT forward anything else
- `UPSTREAM_DIST` skips the install block (ad-hoc experiments); `--install-only` is parsed before the transcript-name loop

**C — scripts**

| Script | Usage | Behaviour | Exit |
|--------|-------|-----------|------|
| `upstream-ahead.sh` | `upstream-ahead.sh` | pin from file; `npm view @agentclientprotocol/claude-agent-acp versions --json --fetch-retries=0 --fetch-timeout=5000`; count STABLE versions (no `-`) strictly greater than pin using a numeric semver compare in `node -e`; prints `upstream is N versions ahead of pinned X (latest Y)` (N>0), `upstream-ahead: pinned X is the latest stable version` (N=0), or `upstream-ahead: skipped (npm registry unreachable)`; `UPSTREAM_AHEAD_SKIP=1` prints the skip notice at once | ALWAYS 0 |
| `upstream-diff.sh` | `upstream-diff.sh <from> [<to>]` (`<to>` default = latest stable from `npm view`) | work dir `rust/target/upstream/diff-<from>-<to>` (recreated); `npm pack` both, `tar -xzf` each into its own dir; prints (1) `== dependencies` (key/version diff of `package.json` `dependencies`+`engines`), (2) `== dist files` (`diff -rq` of both `dist/` trees excluding `*.d.ts`, `*.map`; plus line counts for new/changed `.js`), (3) `== changelog <to> down to <from>` from `https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/v<to>/CHANGELOG.md` (npm tarballs carry none), fallback `git show v<to>:CHANGELOG.md` after `git fetch upstream --tags`, else a notice; the section is the text between `## [<to>]` and `## [<from>]` (awk); (4) hint lines `git diff v<from> v<to> -- src` and `bash rust/scripts/upstream-divergence.sh <to>` | non-zero only for bad args / pack failure |
| `upstream-divergence.sh` | `upstream-divergence.sh [<ver>]` (default: latest stable) | `UPSTREAM_VERSION=<ver> CAPTURE_OUT_DIR=rust/target/upstream/divergence-<ver> bash porting/capture.sh`, then `node rust/scripts/json-diff.mjs porting/fixtures <out>`; prints per fixture `## <name>` and one line per difference | 0 |
| `json-diff.mjs` | `node json-diff.mjs <oldDir> <newDir>` | for every `*.frames.jsonl` in `<oldDir>`: parse both as JSONL; equal length -> recursive walk printing `[i] <json.path>: <old> -> <new>` (values truncated to 160 chars); unequal length -> print the first differing index with both frames truncated; also compares `*.argv.json` and `initialize.json` as whole objects | 0 |

**D — `UPSTREAM-BUMP.md` outline (numbered steps, exact commands)**: 0 explain the single source of truth and the lockfile (`porting/upstream-locks/`, commit the new one with the bump); 1 `bash rust/scripts/upstream-ahead.sh`; 2 `bash rust/scripts/upstream-diff.sh <pin> <new>` and read changelog; 3 `git fetch upstream --tags && git diff v<pin> v<new> -- src`; 4 `bash rust/scripts/upstream-divergence.sh <new>` (frame-level preview); 5 edit `porting/UPSTREAM_VERSION`; 6 port per class (PORT-E / PORT-N / CLI / SKIP rules, PARITY `pending` escape valve); 7 fix transcripts the new CLI-facing frames require; 8 `bash porting/capture.sh` (regenerates fixtures; review `git diff porting/fixtures`); 9 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential` x3; 10 `bash rust/scripts/rust-gate.sh`; 11 update `PARITY.md` header + rows and `INVARIANTS.md` evidence; 12 optional CI follow-up (`upstream-ahead` as a scheduled workflow) — needs a G7 exemption for `.github/`, out of scope here.

**Verify phase 1:**
- [x] 1.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test version 2>&1 | grep -E "test result|FAILED"` — 2 pass
- [x] 1.T2 `cargo test --manifest-path rust/Cargo.toml -p fake-claude 2>&1 | grep -E "test result|FAILED"` — all pass incl. the 2 new
- [x] 1.T3 EQUIVALENCE (proves the new capture path, before any bump): `rm -rf "$PWD/rust/target/upstream/capture-check" && CAPTURE_OUT_DIR="$PWD/rust/target/upstream/capture-check" bash porting/capture.sh >/dev/null 2>&1; diff -r porting/fixtures rust/target/upstream/capture-check && echo CAPTURE_EQUIVALENT` — must print `CAPTURE_EQUIVALENT` (43 files byte-identical; the cache is created on first run, needs network). If any file differs: STOP, do not edit fixtures, report the diff
- [x] 1.T4 `bash rust/scripts/upstream-ahead.sh; echo "exit=$?"` printed line + `exit=0`; offline simulation `npm_config_registry=http://127.0.0.1:9 bash rust/scripts/upstream-ahead.sh; echo "exit=$?"` prints the skip notice + `exit=0`
- [x] 1.T5 `bash rust/scripts/upstream-diff.sh 0.93.0 0.94.0 2>&1 | head -60` shows the four sections and the 0.94.0 changelog entry
- [x] 1.T6 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential 2>&1 | grep -E "test result|FAILED"` — 22 pass; and `git diff -U0 -- rust/claude-agent-acp-rs/tests/differential.rs | grep '^+' | grep -c 'JsonPath::new'` prints `0`
- [x] 1.T7 `bash rust/scripts/rust-gate.sh 2>&1 | tail -4` — last line `==> gate OK`; this includes `capture_is_deterministic_across_runs` (`rust/fake-claude/tests/phase2.rs:38`) which runs `capture.sh` twice inside the timed `cargo test` (cache already warm via the prewarm step); `bash rust/scripts/selftest-gate.sh 2>&1 | tail -3` ends without `FAILED`
- [x] 1.T9 timeout fallback: `perl -e 'alarm shift; exec @ARGV' 1 sleep 5; echo "exit=$?"` prints a non-zero exit (142) after ~1 s; cold-cache simulation `UPSTREAM_CACHE_DIR=$PWD/rust/target/upstream-cold bash porting/capture.sh --install-only` installs atomically (no `*.tmp.*` dir left behind) and a second call is a no-op
- [x] 1.T10 `UPSTREAM_VERSION=0.93.0 bash porting/capture.sh text-only; echo "exit=$?"` is REFUSED (non-zero, committed fixtures untouched, `git status --short porting/fixtures` empty)
- [x] 1.T8 `git diff --stat -- . ':!rust' ':!porting' ':!.vibekit' ':!.gitignore'` is empty (G7)

## Implementation Phase 2 — bump to 0.94.0; the regenerated differential is the work list

Sized for one implementer turn. WORKFLOW (Decision 18): fix transcripts (2.2) -> bump pin + regenerate fixtures (2.3) -> the differential is now RED and is the work list -> port item by item until green. Every divergence in the regenerated fixtures MUST be ported; an item that cannot be ported is NOT deferred — STOP and report it to the orchestrator. Nothing in this phase is deferrable.

### Checklist

- [x] 2.0 Read research §1 (E-1..E-15, E-19), X-7, X-9, Decisions 3, 5, 8, 9, 10, 11, 18, 19, 20; `C/agent.rs:75-123,531-560,625-700`; `C/map.rs:341-420`; `C/permission.rs:28-80,249-420`; `C/session.rs:505-600,917-998`; `C/tools.rs:60-100`; `C/process.rs:169-243,440-460`; `C/control.rs:98-215`
- [x] 2.1 `rust/claude-agent-acp-rs/Cargo.toml:8` `agent-client-protocol = { version = "2.1", features = ["unstable_tool_call_name"] }`; confirm `cargo tree --manifest-path rust/Cargo.toml -i agent-client-protocol@2.1.0 -e normal` still resolves (G10). `skip_serializing_none` means no `name: null` on the wire
- [x] 2.2 Transcript fixes in `porting/corpus/` (Decision 8): insert user `tool_result` echo(es) `{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"<id>","content":"ok"}]}}` immediately BEFORE the step's `{"type":"result",…}` emit, one per `tool_use` id emitted so far without a result, in: `single-tool`, `multi-tool`, `streamed-partial-input`, `permission-allow` (id `toolu_01PERM`, in the step after the `control_response` expect); `permission-deny` the same BUT the echo is `{"type":"tool_result","tool_use_id":"toolu_01PERM","is_error":true,"content":"User refused permission to run tool"}`; `subagent-drain`, `subagent-followup` (id `toolu_01TASK`, content `"Async agent launched"`, before the first `result` emit; keep their `task_started`/`task_notification` frames untouched). `cancel-queued-echo-less`: insert BEFORE the `interrupt` step ONE step per abandoned message, checking content and order: `{"expect":{"type":"control_request","request":{"subtype":"cancel_async_message","message_uuid":"<value>"}},…}` — the uuid is not known statically, so the step is `{"expect":{"type":"control_request","request":{"subtype":"cancel_async_message"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ"}}]}` and the ORDER/CONTENT check lives in unit test `inv_38` (2.13). Other transcripts untouched
- [x] 2.3 Set `porting/UPSTREAM_VERSION` to `0.94.0`; `bash porting/capture.sh` (installs the 0.94.0 cache; lockfile written to `porting/upstream-locks/0.94.0.package-lock.json`, commit it); `git diff --stat -- porting/fixtures` shows all 21 `frames.jsonl`/`argv.json`/`initialize.json` changed. Run the differential once: it is RED — that list (plus research §1) is the work for 2.4-2.14
- [x] 2.4 E-1..E-4 `C/agent.rs:75-103` `initialize_response(air:&AirClient)`: `"authStatus":{}` in `agentCapabilities._meta`; `"systemPrompt":{"mode":{"override":{}}}` in `sessionCapabilities` (advertised; honouring is Phase 3, PARITY row until then); top-level `_meta` = `{"steering":{"supported":true}}` plus, ONLY when `air.is_air()`: `"jetbrains":{"air":{"version":1,"capabilities":["sessionFailure","agentFileChangeReport","nativeSubagentSessions","asyncTasks","customInstructions","recommendedValue","diffPatch","planFile"],"goal":{"version":1,"controlMethod":"_session/goal","actions":["set","clear"]}}}` and `"subagents":{}` in `sessionCapabilities` (verified against the 0.94.0 adapter, `dist/acp-agent.js:1458-1470`). Create `C/air.rs` (`pub mod air;`): `AirClient{version,capabilities}`, `from_client_capabilities(&Value)` mirrors Node's `isAirClient` — `Some` whenever `_meta.jetbrains.air` is an OBJECT (empty `{}` included; version/array gates are NOT here), `None` when absent/non-object; `is_air()` = true for a constructed value (the `Option` around it carries is-AIR); `supports(&str)` mirrors Node's `clientSupportsAirCapability` — version integer >= 1 AND capabilities array contains it (Opus MAJOR on Phase 2); `C/agent.rs:542-548` parses `params["clientCapabilities"]` onto `Registry` (`:435`)
- [x] 2.5 E-5 `C/map.rs:341-420` `map_commands_changed`: drop CLI entries named `mcp`, append `mcp` (`description:"Show the MCP servers and their status, or reconnect, enable, or disable a server"`, hint `"[reconnect|enable|disable [<server>|all]]"`) then `remote-control` (`description:"Continue this session from claude.ai/code or the Claude mobile app; `off` disconnects"`, hint `"[session name | off]"`) unless the CLI already offers a non-terminal `remote-control` (`S:12610-12650`). Call sites `C/agent.rs:573,614`, `C/session.rs:1142` inherit it. Test `inv_36_commands_append_mcp_and_remote_control`
- [x] 2.6 E-13/E-15 `C/process.rs:169-243` `build_argv` valued flags as `--name=value` (`--permission-prompt-tool=stdio`, `--disallowedTools=<csv>`, `--tools=<csv|default>` (empty -> `--tools=`), `--permission-mode=<m>`, `--model=<m>`); keep `--resume=`, `--setting-sources=`, `--session-id=` and the trailing `""` after `--replay-user-messages`. `:445-455` also `cmd.env("CLAUDE_CODE_SDK_READS_SESSION_STATE","1")`; `rust/fake-claude/src/lib.rs:141` `ARGV_ENV_ALLOWLIST` gains that name (3 entries). Update argv tests (`C/agent.rs:866`, `C/process.rs` `inv_25_*`, `tests/process_transport.rs:25-75`)
- [x] 2.7 E-14/X-6 `C/control.rs:98-170` `InitializeOptions.prompt_suggestions: Option<bool>` (emitted as `promptSuggestions` only when `Some`; live agent leaves `None`); update `inv_11_initialize_matches_adapter` (`:658-690`) to hooks `PostToolUse hook_0, PostModelSwitch hook_1, PostCompact hook_2, TaskCreated hook_3, TaskCompleted hook_4` + `Some(true)` so it equals the regenerated `initialize.json`
- [x] 2.8 E-9 name: `C/map.rs:195-200` `.name(tool_name)` on the typed `ToolCall`; `C/permission.rs:278-322` `build_tool_call` adds `"name"` and `"status":"pending"`; `ensure_tool_call_emitted` (`:324-356`) `.name(..)`
- [x] 2.9 B1/X-7 field tracker (non-deferrable): `C/map.rs` keeps per-tool-call "what the client holds" (`status,title,kind,content,locations,rawInput,rawOutput`, from the `tool_call` and every later update); a `tool_call_update` carries only fields whose value differs (compare serialized JSON); `_meta` always kept whole for non-AIR; an update with no changed field and an unchanged `_meta` is NOT sent (`dist/tool-calls/field-tracker.js`, entry ends after the `tool_result`). Evidence: Node drops `kind` from `streamed-partial-input` frame 7 and `kind,title,content` from frame 8. Tests `inv_45_field_tracker_drops_unchanged`, `inv_45_field_tracker_keeps_meta`
- [x] 2.10 B2/X-9 non-AIR tool `_meta` (non-deferrable): `C/tools.rs:60-100` `claude_code_meta` returns ONLY `{toolName}` for EVERY client, UNCONDITIONALLY (drop `subagent`, Bash `title`, `skill`; evidence cap094c) — Phase 2 does not depend on `AirClient` threading (3.2). AIR tool `_meta` (`jetbrains.air.{commandTitle,subagent,skill}`, `dist/tool-calls/renderer.js:576-590`) is NOT re-added in this plan (OQ-14, PARITY `pending`). Test `inv_46_tool_meta_is_tool_name_only`
- [x] 2.11 E-10/E-11 `C/permission.rs`: `build_request_permission` (`:249-276`) options `[{"optionId":"allow-once","name":"Yes","kind":"allow_once"},{"optionId":"reject","name":"No","kind":"reject_once"}]`, no `_meta.permission`; `map_outcome` (`:51-80`): `allow-once` Allow, `allow-with-updates` (offered only from Phase 3) Allow-with-updates placeholder = Deny until then, others Deny; `allow_control_response`/`deny_control_response` (`:359-390`) gain `"toolUseID"` and `"decisionClassification":"user_temporary"|"user_reject"` (signature gains `tool_use_id`; update `C/session.rs:962-985`); deny message unchanged. Update `inv_21_deny_payload_and_continue` (`:494`) and siblings. Recorder `permission:"allow"` must choose by `kind == allow_once` (fix `rust/acp-recorder/src/recorder.rs` only if it matches ids)
- [x] 2.12 E-6 quota totals only: `C/session.rs` `PromptReply` (`:105`) gains `quota: Option<Value>` built in `session.rs` from the EXISTING `Usage` of `TurnEvent::Settled` iff `had_usage`: `{"quota":{"token_count":{"totalTokens":i+o+cr+cw,"inputTokens":i,"cachedInputTokens":cr,"cachedWriteTokens":cw,"outputTokens":o,"reasoningOutputTokens":0},"model_usage":[]}}`; `C/agent.rs:655-690` writes it as `result["_meta"]`. NO `turn.rs` change here (per-model rows = Phase 3). Tests `inv_37_quota_meta_shape`, `inv_37_queued_sweep_has_no_quota`
- [x] 2.13 E-8 cancel (HIGH RISK): `C/turn.rs:237` `steered_echoes` becomes insertion-ordered (`Vec<String>` or equivalent; keep every existing lane test green); `C/turn.rs:374-458` `cancel()` records abandoned uuids in Node's order — pending steered echoes first (insertion order), then swept queued turns FIFO (`dist/acp-agent.js:6253-6264`) — exposed by `pub fn take_abandoned(&mut self) -> Vec<String>`; do not touch `orphaned_uuids` / `pending_orphan_results` bookkeeping. `C/control.rs:195-215` add `pub fn send_request_detached(&self, payload: Value)` (synchronous enqueue on the unbounded writer; INV-27 holds). `C/session.rs:520-575` Cancel arm: after `machine.cancel()`, after arming the force-cancel backstop, BEFORE spawning the interrupt task: one `cancel_async_message{message_uuid}` per uuid in that order. Test `inv_38_cancel_async_message_sequence_precedes_interrupt` asserts the EXACT uuid sequence (steer then queued A, B) and that `interrupt` is written after them
- [x] 2.14 Regenerate and iterate: `bash porting/capture.sh`, run the differential, fix remaining divergences until green; `grep -c '"version":"0.94.0"'`-style check: `grep -c '"version":"0.94.0"' porting/fixtures/text-only.frames.jsonl` == 1. Update `inv_24_commands_proactive` only via the regenerated fixture. If a divergence needs something this plan does not describe: STOP and report it, do not add an ignore-path
- [x] 2.15 Update this plan's checklist; record deviations under Key Decision 16

**Verify phase 2:**
- [x] 2.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential 2>&1 | grep -E "test result|FAILED"` — 22 pass; run it 3 times — 3x `test result: ok. 22 passed`
- [x] 2.T2 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs 2>&1 | grep -E "test result|FAILED"` — every binary ok (new `inv_36..inv_38`, `inv_45`, `inv_46`; updated `inv_11`, `inv_21`, `inv_25`)
- [x] 2.T3 `git diff -U0 0609bed -- rust/claude-agent-acp-rs/tests | grep '^+' | grep -c 'JsonPath::new'` prints `0` (all of `tests/`, not just `differential.rs`)
- [x] 2.T4 `grep -L '"stopReason":"end_turn"\|"stopReason":"cancelled"' porting/fixtures/*.frames.jsonl` prints exactly `error-result`, `idle-without-result`, `resume-load` (resume-load has no prompt); `grep -l incomplete_tool_call porting/fixtures/*.frames.jsonl` prints nothing
- [x] 2.T5 `CAPTURE_OUT_DIR="$PWD/rust/target/upstream/c1" bash porting/capture.sh >/dev/null 2>&1; CAPTURE_OUT_DIR="$PWD/rust/target/upstream/c2" bash porting/capture.sh >/dev/null 2>&1; diff -r porting/fixtures rust/target/upstream/c1 && diff -r rust/target/upstream/c1 rust/target/upstream/c2 && echo STABLE`
- [x] 2.T6 `bash rust/scripts/rust-gate.sh 2>&1 | tail -4` ends `==> gate OK` (this runs `capture_is_deterministic_across_runs`, `rust/fake-claude/tests/phase2.rs:38`, against the now-warm 0.94.0 cache; the gate prewarm step installed it)
- [x] 2.T7 `git diff --stat -- . ':!rust' ':!porting' ':!.vibekit' ':!.gitignore'` empty (G7); `cargo tree --manifest-path rust/Cargo.toml -i agent-client-protocol@2.1.0 -e normal` succeeds
- [x] 2.T8 `cargo test --manifest-path rust/Cargo.toml -p fake-claude 2>&1 | grep -E "test result|FAILED"` — includes `g7_upstream_untouched` (diffs the tree against tag `v0.70.0`, still the repo's own source version) and `fixtures_are_portable_and_secret_free`

## Implementation Phase 3 — AIR async tasks, unfinished tools, durable permission, folded prompt, new scripts, parity docs

Sized for one implementer turn. CORE (always done): 3.1-3.6, 3.14-3.16. ORDERED TAIL, in this order: 3.7 unfinished tools, 3.8 folded prompt, 3.9 durable Bash permission. DEFERRABLE (no corpus-visible effect): 3.10-3.12. 3.13 is `pending` outright.

One rule for the tail (Decision 18): each of 3.7/3.8/3.9 = port + its corpus script + its acceptance test, added TOGETHER. If the port is not finished the script is NOT added and the feature is a `pending` PARITY row; once a script exists its divergence is corpus-visible and must be ported (or STOP and report). Stop the tail when the turn budget ends.

### Checklist

- [x] 3.0 Read research §2 R71-5/R87-1/R87-2/R851-4/X-1/X-2/X-5, `dist/async-tasks.js` (whole), `dist/air-extension.js:90-154`, `git show v0.94.0:docs/air-extensions.md` ("Async tasks", "Options"), `dist/acp-agent.js:3559-3605`, `C/session.rs:98-104,745-860,999-1088`, `C/map.rs:55-100`, `C/turn.rs:977-995`, Decisions 7, 11-13, 21
- [x] 3.1 Recorder: `rust/acp-recorder/src/script.rs:22-26` `Step::Initialize` becomes `Initialize { #[serde(default)] client_capabilities_meta: Option<Value> }` (`{"call":"initialize","client_capabilities_meta":{"jetbrains":{"air":{"version":1,"capabilities":["asyncTasks"]}}}}`); `recorder.rs:193-196` sets it on the typed request; parse unit test `parses_initialize_with_client_meta`
- [x] 3.2 Thread `AirClient` (2.4) from `Registry` into `SessionOptions` (`C/session.rs:183-205`) and the actor (needed only by async tasks; Phase 2 does not depend on it)
- [x] 3.3 `C/session.rs:89-104` add `SessionOutbound::RawUpdate(Value)`; `C/agent.rs` forwards it as `UntypedMessage::new("session/update", value)` via `connection.send_notification`; value = `{"sessionId":…,"update":{…}}`
- [x] 3.4 New `C/async_tasks.rs` (`pub mod async_tasks;`), Decision 12: `AsyncTaskRuntime{enabled, tasks}` with `task_started`, `task_progress`, `task_updated`, `task_notification`, `release_held`, `finish_all(state)` each returning `Vec<Value>` updates (pure). Port exactly: `field()` camel/snake lookup, `friendlyTaskType`, `isBackgroundTask`, ignore rule (`subagent_type` present / `task_type` `local_agent`|`local_monitor`), hold-until-tool-call-id (`announce`), `changedFields` dedupe, `taskState` mapping, terminal handling (second terminal ignored), payloads `async_task_spawned{asyncTaskId,name,taskType,description,showInTranscript,canStop:true[,outputFilePath][,toolCallId]}`, `async_task_progress`, `async_task_state_update`. Tests `inv_40_async_task_spawn_waits_for_tool_call_id`, `inv_40_async_task_terminal_before_id_spawns_without_tool_call`, `inv_40_async_task_progress_dedupes`, `inv_40_async_task_ignores_subagent_and_monitor`
- [x] 3.5 Wire into `C/session.rs:814-860`: `task_started` (non-subagent), new `task_progress` arm, `task_updated`, `task_notification` -> runtime only when `air.supports("asyncTasks")`; `release_held()` when a prompt settles (before the flush); `finish_all("stopped")` on teardown/stream end. Turn-machine subagent handling (`on_task_started/on_task_ended`) UNCHANGED
- [x] 3.6 Scripts `async-task-air`, `async-task-non-air` (Contract E) -> `bash porting/capture.sh async-task-air async-task-non-air`; add `inv_24_async_task_air`, `inv_24_async_task_non_air` after `tests/differential.rs:448`
- [x] 3.7 E-12/R851-4 unfinished tools — DONE (resume turn): `src/unfinished.rs` (`ToolBooks` in `MapState.books`, `gate_events`), wired in `session.rs` (`note_tool_updates`, `settle_events`), `TurnMachine::foreground_target`, `FailureKind::IncompleteToolCall`; script `unfinished-tool` + `inv_24_unfinished_tool` (Decisions 7, 19): `C/map.rs` `MapState` gains `dispatched: HashSet<String>` (ids in a CONSOLIDATED assistant message); the session actor keeps `foreground: HashMap<prompt_uuid, HashSet<String>>` (ids surfaced as `ToolCall` while that turn is active; removed on `tool_result`). In `handle_events` for EVERY `TurnEvent::Settled{stop_reason:EndTurn}` (active, held-turn, deferred and steered settles alike — Node checks every end_turn settle, `dist/acp-agent.js:3559-3605`; never for cancelled / failed / queued-sweep): `unfinished = foreground[prompt_uuid] \ live_background parent ids`; not-dispatched ids -> `tool_call_update{status:"failed",content:[text "Claude stopped this tool call before it ran."]}` and the turn stays `end_turn`; dispatched ids -> same update with text `"Claude ended the turn without returning a result for this tool."` for each, then fail the prompt `-32603` `Claude ended the turn without returning results for tool calls: <id1>, <id2>` with `data:{"errorKind":"incomplete_tool_call"}` (extend `FailureKind` / `session_error_to_rpc`, `C/agent.rs:319-363`). KEYED BY `prompt_uuid`: `ensure_active_turn` emits the held turn's `Settled` and the next turn's `Activated` in ONE batch (`C/turn.rs:977-995`), so a single "current foreground set" would mix turns. The post-filter changes no `TurnMachine` books (`settle_with_usage` / `fail_active` keep identical debt, `C/turn.rs:1079-1119`). Tests `inv_41_unfinished_dispatched_tool_fails_turn`, `inv_41_abandoned_streamed_tool_closes_without_failing`, `inv_41_background_task_tool_exempt`, `inv_41_cancelled_turn_not_failed`, `inv_41_held_turn_settle_checked_per_prompt` ; THEN (same item) add script `unfinished-tool` (Contract F), `bash porting/capture.sh unfinished-tool`, and `inv_24_unfinished_tool`. Tail rule: no port, no script, PARITY `pending`
- [x] 3.8 R87-1 folded prompt — DONE (resume turn): `TurnMachine::result_is_autonomous` / `answers_pending_prompt` (Node's any-unsettled-non-held rule), script `folded-prompt` + `inv_24_folded_prompt`, tests `inv_42_*` appended to turn_tests.rs (no existing test edited) (ordered tail): `C/turn.rs:91,616` an autonomous-origin result (`task-notification`) whose `user_message_uuids` (fallback `user_message_uuid`) names the uuid of the ACTIVE turn or the QUEUE HEAD answers that turn: settle it with this result's stop reason/usage via the existing settle path (a held-for-subagents turn still waits); un-stamped and non-matching results behave byte-for-byte as today (`inv_30_orphan_result_not_reused`, `inv_16_*` stay green, unmodified). Script `folded-prompt` (Contract G) + `inv_24_folded_prompt`; test `inv_42_folded_prompt_settles`, `inv_42_unmatched_autonomous_result_still_skipped`. If any existing `turn_tests.rs` test needs editing: STOP and report
- [x] 3.9 Durable Bash permission — DONE (resume turn): `permission::durable_bash` / `Outcome::AllowWithUpdates` / `allow_with_updates_control_response`, session wiring, recorder `allow_always`, script `permission-suggestions` + `inv_24_permission_suggestions`, tests `inv_47_*`; also fixed `map_outcome` to read the real nested `result.outcome.{outcome,optionId}` wire shape (ordered tail, OQ-3). (a) Recorder: `rust/acp-recorder/src/script.rs:11-17` `PermissionPolicy` gains `#[serde(rename = "allow_always")] AllowAlways` (the enum is `rename_all="lowercase"`, so the explicit rename is required); `recorder.rs` selects the first option whose `kind == allow_always` for it (policy `allow` keeps choosing `allow_once`); parse test `parses_permission_allow_always`. (b) Port: when `permission_suggestions` is exactly the Bash `addRules` family Node can describe (`dist/permissions/options/shell.js` `shellSuggestionsLabel`: commands-only branch `Yes, and don't ask again for <cmds> commands`, Node 0.94 verified `"Yes, and don't ask again for echo commands"`, kind `allow_always`; `permissionRulePrefix` strips a trailing `:*`; `displayList`: 1 -> a, 2 -> `a and b`, more -> `a, b, and c`, joined text > 50 chars -> `similar`), offer `[allow-once "Yes", allow-with-updates <label>, reject "No"]`; selecting it sends `allow` + `updatedPermissions` = the CLI's suggestions VERBATIM + `decisionClassification:"user_permanent"`; anything Node cannot describe (read paths, directories, non-Bash rules, `matched_ask_rule` present) -> Yes/No only. (c) Script `permission-suggestions` (Contract I), `inv_24_permission_suggestions`, tests `inv_47_bash_durable_label`, `inv_47_unrepresentable_suggestions_fall_back`. Tail rule: no port, no script, PARITY `pending` ("no durable choice; legacy Always Allow removed")
- [x] 3.10 (deferrable, DEFERRED: PARITY `pending` row written) per-model quota rows (R71-8) — **PARITY `pending` (see 3.14 row: "model_usage always []")**: `C/turn.rs` `ModelRow`, `TurnMachine.accumulated_model_usage` (reset with `accumulated_usage`) and `last_model_usage_reading` (`Some(vec![])` new session, `None` resumed); in `on_result` the reading advances BEFORE the autonomous early return (`C/turn.rs:635-639`) without touching `owed_trailing_idles`; the turn tally is non-autonomous only; increment = current - previous per model (below previous = rewind -> reading itself; zero-total models dropped; resumed first reading only seeds); `DeferredSettle` and `Settled` carry the snapshot; `quota.model_usage` rows fill in `session.rs`. Tests `inv_48_model_usage_increment_and_rewind`, `inv_48_autonomous_result_advances_reading_not_tally`. Deferred -> PARITY `pending` ("model_usage always []")
- [x] 3.11 (deferrable, DEFERRED: PARITY `pending` row written; the SDK initialize wire fields have no Node oracle in the corpus) `systemPrompt` honouring (X-4) — **PARITY `pending` (see 3.14 row: "advertised, not honoured")**: `C/agent.rs:549-624` validate on `session/new|load` (`invalid_params -32602`: `Unsupported systemPrompt mode: <m>` / `systemPrompt content must be text, but received a <t> block`, before spawn); forward via `InitializeOptions.system_prompt` (override -> string; append -> `{"type":"preset","preset":"claude_code","append":text}`); test `inv_39_system_prompt_validation`. Deferred -> PARITY `pending` ("advertised, not honoured")
- [x] 3.12 (deferrable) steer priority (R71-10/R86-6) — DONE (resume turn; `inv_43_steer_later_while_permission_open`): `C/session.rs:636-641` `injected["priority"]` = `"later"` while `pending_permissions` is non-empty else `"now"`; actor-level test `inv_43_steer_later_while_permission_open` asserts BOTH the stdin priority and that the turn then settles on the steer lane exactly like a `now` steer (feed echo, result, idle). Deferred -> `pending`
- [x] 3.13 PENDING OUTRIGHT (PARITY row written) (no work in this plan; one PARITY row each in 3.14): R811-8 `informational` frames, R811-9 Write `path`/`file_text` aliases, R77-1 `allowDangerouslySkipPermissions:false` opt-out, S-13 `sdk_host_only` idle regression test
- [x] 3.14 Docs (Contract H): `porting/PARITY.md` — header -> 0.94.0; refresh section (A) line refs; new section (C) "0.71-0.94 inventory" with a row for every research row family: SKIP ranges; each `pending` item WITH its consequence — OQ-1: `agentFileChangeReport` requests get no reply, `customInstructions` silently dropped, `nativeSubagentSessions` gets no child sessions, `_session/goal` returns `-32601`, `diffPatch`/`planFile`/`recommendedValue`/`sessionFailure` inert; `authStatus` marker advertised but `_auth/status_update` never pushed; `sessionCapabilities` close/delete/fork/list/resume advertised but `-32601`; `/mcp` forwarded to the CLI, `/remote-control` `skipped-deliberate`; R71-2 modes renamed (`dontAsk` -> `auto`, new labels) hidden by the `result.modes`/`configOptions` ignores; cancel family R90-2/R91-3/R86-8; R811-6 and R72-2 stamped-result lanes; compaction R75-2; field-tracker for AIR; subagent transcripts not exercising the hold (E-19); the agentInfo version divergence (deliberate, user-required). Fix the stale "17 corpus transcripts" line. `porting/INVARIANTS.md` — rows INV-35..INV-48 as built, Evidence from REAL test output, "INV-24 all N scripts". `porting/UPSTREAM-BUMP.md` final pass (every command run once). `rust/AGENTS.md:24` wire-truth wording; sweep `v0.70.0` doc comments (`C/tools.rs:6`, `C/permission.rs:5`, `C/process.rs:167`, `C/agent.rs:29,73`, `C/map.rs:28`)
- [x] 3.15 Final verification (Verify phase 3); list the OQ-1 consequences in the final report for explicit user sign-off
- [x] 3.16 Update this plan's checklist; record deviations under Key Decision 17
- [x] 3.17 Review follow-ups (Opus reviews of Phase 1 & 2; MAJOR on air.rs is Item 2.4):
  - C (Phase-1 hardening, small/mechanical): capture.sh pin validation -> strict MAJOR.MINOR.PATCH numeric check; trap removing `$TMP` on exit/interrupt; concurrent-install-safe cache (mkdir lock / unique tmp + move only if no `.complete`; never `rm -rf "$CACHE"` while another run may use it); `npm install --userconfig <empty> --registry=https://registry.npmjs.org/` + assert every lockfile `resolved` starts with that registry; upstream-divergence.sh / upstream-diff.sh strict version validation before any `rm -rf` on a path built from the args; rust-gate.sh wraps the prewarm + upstream-ahead steps in `run_timeout` (300/20 s); tests/version.rs test that a WRONG Rust-side agentInfo version FAILS the diff; fake-claude `explicit_expect_wins_over_auto_ack` made non-vacuous
  - D (Phase-2 minors): map.rs field tracker clears per-tool-call entries on cancel and on turn settle (unit test `inv_45_field_tracker_clears_all_on_boundary`); control.rs `send_request_detached` doc comment corrected + correlation entry bounded on stream end/drop; process.rs sets `CLAUDE_CODE_SDK_READS_SESSION_STATE=1` only when the parent env does not already define it; session.rs:1012-1018 read Node's cancel/error permission decision — recorded as a PARITY `pending` row in 3.14 (Node throws "Tool use aborted" vs Rust's user_reject deny; corpus-invisible, SDK-internal wire shape); inv_38/cancel_async_message asserts the message_uuid VALUES on the wire in order and that `interrupt` follows (new `inv_38_cancel_async_message_wire_order` session-level test); 3.9 NOT completed -> removed the `Outcome::AllowAlways` / `allow_always_control_response` / `permission_metadata_for_always_allow` dead code (kept the Yes/No options check as `inv_21_request_options_yes_no_only`)

### Contracts

**E — `async-task-air` / `async-task-non-air`** (same transcript, two `.acp.json`)

`async-task-air.acp.json`
```json
{
  "permission": "allow",
  "steps": [
    {"call": "initialize", "client_capabilities_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks"]}}}},
    {"call": "session/new"},
    {"call": "session/prompt", "text": "run the workflow"}
  ]
}
```
`async-task-non-air.acp.json`: identical but `{"call": "initialize"}`.

`*.transcript.jsonl` — 2 lines: line 1 = `head -1 porting/corpus/text-only.transcript.jsonl` verbatim; line 2 (single step, exact):
```
{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"run the workflow"}]}},{"type":"system","subtype":"task_started","task_id":"wf1","task_type":"local_workflow","description":"Deploy","workflow_name":"Deploy","session_id":"s","uuid":"u1"},{"type":"system","subtype":"task_progress","task_id":"wf1","description":"Deploying","summary":"step 1","session_id":"s","uuid":"u2"},{"type":"system","subtype":"task_notification","task_id":"wf1","status":"completed","summary":"done","output_file":"/tmp/tasks/wf1.output","session_id":"s","uuid":"u3"},{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Workflow done"}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"Workflow done","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}]}
```
- Node 0.94.0 output for this exact transcript was verified while planning (AIR run), in order: `async_task_spawned{asyncTaskId:"wf1",name:"Deploy",taskType:"workflow",description:"Deploy",showInTranscript:true,canStop:true,outputFilePath:"/tmp/tasks/wf1.output"}` (NO `toolCallId`; held until the terminal notification, then released), `async_task_progress{asyncTaskId:"wf1",description:"Deploying",summary:"step 1"}`, `async_task_state_update{asyncTaskId:"wf1",state:"completed",summary:"done"}`, then the prompt response `end_turn` with `_meta.quota`; NON-AIR run: none of the three
- The committed fixture (generated by `capture.sh`) is authoritative; the list is only a sanity check

**F — `unfinished-tool`** (added by 3.7 only): `.acp.json` = text-only's with `"text": "run a command"`; transcript: line 1 initialize (verbatim), line 2 = the ORIGINAL `single-tool` step from `git show 0609bed:porting/corpus/single-tool.transcript.jsonl` (assistant `Bash` tool_use `toolu_01ABC`, NO `tool_result`, then `result end_turn`). Expected fixture tail: `tool_call_update status:failed` then `error -32603 … incomplete_tool_call`

**G — `folded-prompt`**: `.acp.json` steps `initialize, session/new, session/prompt "first", session/prompt "second"`; transcript step 1 echo + result for "first"; step 2 (consumes "second", NO echo) emits only `{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"folded","origin":{"kind":"task-notification"},"user_message_uuids":["$MATCH.uuid"],"usage":{…}}`; Node 0.94.0 was verified to answer the second prompt `end_turn` for this transcript while planning

**H — PARITY row format**: `| <feature> | <upstream ref: release + dist/src file> | pending|skipped-deliberate|ported | <reason / consequence / Rust coverage file:line / test name> |`

**I — `permission-suggestions`**: copy of `permission-allow` (Phase-2 version) with `"permission":"allow_always"` (needs 3.9a) and the `can_use_tool` request carrying `"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"echo:*"}],"behavior":"allow","destination":"localSettings"}]`; expected Node options `allow-once "Yes"`, `allow-with-updates "Yes, and don't ask again for echo commands"` (kind `allow_always`), `reject "No"`; the transcript step that consumes the `control_response` MUST `expect` `{"type":"control_response","response":{"response":{"behavior":"allow","updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"echo:*"}],"behavior":"allow","destination":"localSettings"}]}}}` so the durable reply is actually verified; the captured fixture is authoritative for the ACP frames

**Verify phase 3:**
- [x] 3.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential 2>&1 | grep -E "test result|FAILED"` — 24 pass after the core (22 + async-task-air, async-task-non-air) plus +1 for each completed tail item (`unfinished-tool`, `folded-prompt`, `permission-suggestions`), max 27; run it 3 times — 3x ok
- [x] 3.T2 `grep -c async_task_spawned porting/fixtures/async-task-air.frames.jsonl` >= 1; `grep -c async_task porting/fixtures/async-task-non-air.frames.jsonl` == 0; `(only for tail items completed) `grep -c incomplete_tool_call porting/fixtures/unfinished-tool.frames.jsonl` == 1; `grep -c "don't ask again" porting/fixtures/permission-suggestions.frames.jsonl` >= 1; each missing tail item has a PARITY `pending` row
- [x] 3.T3 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib 'inv_4' 2>&1 | grep -E "test result|FAILED"` and `-p acp-recorder` — all pass
- [x] 3.T4 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn 2>&1 | grep -E "test result|FAILED"` — all pre-existing `steer_*`, `inv_16/23/29/30/31` tests unmodified and passing
- [x] 3.T5 Clean-cache reproduction: `rm -rf rust/target/upstream/0.94.0; CAPTURE_OUT_DIR="$PWD/rust/target/upstream/final" bash porting/capture.sh >/dev/null 2>&1; diff -r porting/fixtures rust/target/upstream/final && echo STABLE`
- [x] 3.T6 `git diff -U0 0609bed -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'` prints `0` (base commit 0609bed; whole `tests/` tree, since normalizers or `base_ignore()` could live anywhere)
- [x] 3.T7 `bash rust/scripts/rust-gate.sh 2>&1 | tail -4` ends `==> gate OK` (includes `capture_is_deterministic_across_runs`); `bash rust/scripts/selftest-gate.sh 2>&1 | tail -3` no FAILED
- [x] 3.T8 every deferred/pending item (3.10-3.13 and any unfinished tail item) has a PARITY row: `grep -n 'model_usage\|systemPrompt\|priority\|informational\|durable' porting/PARITY.md`
- [x] 3.T9 `git diff --stat -- . ':!rust' ':!porting' ':!.vibekit' ':!.gitignore'` empty (G7)

## Files & Phase Impact

| File | Status | Phase | Description / Contract Change |
|------|--------|-------|-------------------------------|
| `porting/UPSTREAM_VERSION` | **New** | 1.1 / 2.3 | one line `MAJOR.MINOR.PATCH` · read by build.rs, capture.sh, scripts |
| `porting/UPSTREAM-BUMP.md` | **New** | 1.14 / 3.13 | Contract D |
| `porting/SYNC.md` | **Modified** | 1.14 | pointer to UPSTREAM-BUMP.md |
| `porting/capture.sh` | **Modified** | 1.10 | Contract B; no root build; env scrub |
| `porting/upstream-locks/{0.70.0,0.94.0}.package-lock.json` | **New** | 1.10 / 2.3 | generated by capture.sh, committed |
| `porting/divergence-0.94.0-vs-0.70.0.txt` | **New** | 1.13 | generated research artifact |
| `porting/corpus/*.transcript.jsonl` (8 files) | **Modified** | 2.2 | tool_result echoes; `cancel_async_message` step |
| `porting/corpus/{async-task-air,async-task-non-air,unfinished-tool,folded-prompt,permission-suggestions}.{acp.json,transcript.jsonl}` | **New** | 3.6 / 3.8 / 3.9 | Contracts E-G, I |
| `porting/fixtures/*` | **Modified/New** | 2.3 / 3.6 | generated by capture.sh only |
| `porting/PARITY.md`, `porting/INVARIANTS.md` | **Modified** | 3.13 | current |
| `rust/AGENTS.md` | **Modified** | 3.13 | wire-truth wording |
| `rust/fake-claude/tests/phase2.rs` | **Unchanged** | 2.T8 | `g7_upstream_untouched` keeps diffing against tag `v0.70.0` |
| `rust/scripts/{upstream-diff,upstream-ahead,upstream-divergence}.sh`, `json-diff.mjs` | **New** | 1.11 | Contract C |
| `rust/scripts/rust-gate.sh` | **Modified** | 1.12 | `run_timeout` fallback, cache prewarm, `upstream-ahead.sh \|\| true` before `gate OK` |
| `rust/scripts/selftest-gate.sh` | **Modified** | 1.7 | copy pin file into temp tree |
| `rust/claude-agent-acp-rs/build.rs` | **New** | 1.2 | Contract A · emits `ACP_VERSION_STRING`, `UPSTREAM_NPM_VERSION` |
| `rust/claude-agent-acp-rs/Cargo.toml` | **Modified** | 1.2 / 2.1 | version `0.1.0`, `publish = false`; feature `unstable_tool_call_name` |
| `rust/Cargo.lock` | **Modified** | 1.2 | crate version rewritten by cargo (not by hand) |
| `rust/claude-agent-acp-rs/src/lib.rs` | **Modified** | 1.3 / 2.4 / 3.4 | `VERSION`, `UPSTREAM_VERSION`, `pub mod air; pub mod async_tasks;` |
| `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs` | **Modified** | 1.5 | `--version` |
| `rust/claude-agent-acp-rs/src/agent.rs` | **Modified** | 1.4, 2.4, 2.12, 3.3, 3.7, 3.11 | Contract: `initialize_response(&AirClient)`, `agent_info()`, `_meta.quota`, systemPrompt, RawUpdate forward, `incomplete_tool_call` mapping |
| `rust/claude-agent-acp-rs/src/air.rs` | **New** | 2.4 | Contract: `AirClient::from_client_capabilities(&Value)`, `supports(&str)` |
| `rust/claude-agent-acp-rs/src/async_tasks.rs` | **New** | 3.4 | Contract: pure `AsyncTaskRuntime` returning `Vec<Value>` updates |
| `rust/claude-agent-acp-rs/src/map.rs` | **Modified** | 2.5, 2.8, 2.9, 3.7 | commands merge, `ToolCall::name`, field tracker, `dispatched` tracking |
| `rust/claude-agent-acp-rs/src/permission.rs` | **Modified** | 2.8, 2.11, 3.9 | Yes/No options, Bash durable option, `toolUseID`/`decisionClassification` |
| `rust/claude-agent-acp-rs/src/process.rs` | **Modified** | 2.6 | `--flag=value` argv, `CLAUDE_CODE_SDK_READS_SESSION_STATE` |
| `rust/claude-agent-acp-rs/src/control.rs` | **Modified** | 2.7, 2.13 | `prompt_suggestions`, `send_request_detached` |
| `rust/claude-agent-acp-rs/src/session.rs` | **Modified** | 2.11-2.13, 3.2-3.12 | cancel_async_message, quota, RawUpdate, async routing, unfinished post-filter |
| `rust/claude-agent-acp-rs/src/turn.rs` (+`turn/turn_tests.rs`) | **Modified** | 2.13, 3.8, 3.10 | Contract: insertion-ordered `steered_echoes`, `take_abandoned() -> Vec<String>`; folded-prompt settle; optional `ModelRow` |
| `rust/claude-agent-acp-rs/src/tools.rs` | **Modified** | 2.10 | `_meta` = toolName only for every client |
| `rust/claude-agent-acp-rs/tests/{version.rs,common/mod.rs,differential.rs,process_transport.rs}` | **New/Modified** | 1.6, 1.15, 2.6, 3.6, 3.8, 3.9 | agentInfo normalizer, version tests, +5 differentials |
| `rust/fake-claude/src/lib.rs` (+`tests/`) | **Modified** | 1.8, 2.6 | auto-ack; allowlist |
| `rust/acp-recorder/src/{script,recorder}.rs` | **Modified** | 2.11 (kind-based option choice), 3.1 | `initialize.client_capabilities_meta`, `permission:"allow_always"` |

## Out of scope

- ACP v2 surface (`CLAUDE_AGENT_ACP_EXPERIMENTAL_V2`), session index / list / rename / archive, native subagent sessions, auth/gateway/providers/models/config options, elicitation, hooks (`toolResponse`), terminal channel, history replay, session fork, notices, file-change report, session failure, goal — all `skipped-deliberate` (PARITY section B), research §2 `SKIP`
- Cancel family R90-2 / R91-3 / R86-8 (HIGH RISK, corpus-invisible, needs real-claude evidence from the human-only `record-real.sh`), stamped-result lanes R811-6 / R72-2, making the subagent transcripts real (E-19, held-turn lanes also moved in 0.94: `request_marker_lists_v1`, "running while agents live") -> `pending`
- Context compaction tool lifecycle (R75-2), AIR-side tool rendering, `/mcp` handler, `_auth/status_update` push -> `pending`; `/remote-control` handler -> `skipped-deliberate`
- AIR `diffPatch`, `planFile`, `rawInputRendering`, `sessionFailure`, `agentFileChangeReport`, `recommendedValue` behaviours (advertised only, OQ-1)
- CI workflow running `upstream-ahead.sh` on a schedule: needs a G7 exemption for `.github/`; mentioned only in `UPSTREAM-BUMP.md`
- Any change to `src/`, `dist/`, `package.json`, `scripts/`, `.github/`, `docs/`; commits, pushes, tags

## System Boundaries

| Boundary | Contract |
|----------|----------|
| Repo ↔ npm registry | `npm ci|install --prefix <tmp> (lockfile `porting/upstream-locks/<pin>.package-lock.json`) for @agentclientprotocol/claude-agent-acp@<pin>` (exact version), `npm view … versions --json`, `npm pack`; failures: offline -> `upstream-ahead.sh` prints skip and exits 0; `capture.sh` exits non-zero with a message (no cache = no capture) |
| Repo ↔ GitHub raw | `https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/v<ver>/CHANGELOG.md`, fallback local git tag; failure = notice only |
| build.rs ↔ pin file | reads `../../porting/UPSTREAM_VERSION`; compile-time `env!` consts; invalid file = build error |
| ACP client ↔ Rust agent | `initialize` request `clientCapabilities._meta.jetbrains.air:{version:int>=1,capabilities:string[]}` (optional) -> response per research X-2 / E-2..E-4; prompt response `_meta.quota`; permission request `options:[{optionId:"allow-once"\|"reject",name,kind}]`; AIR-only `session/update` variants `async_task_spawned/progress/state_update` (`asyncTaskId:string`, `state:"running"\|"paused"\|"completed"\|"failed"\|"stopped"`); error `-32603` `data.errorKind:"incomplete_tool_call"`; errors for `systemPrompt`: `-32602` |
| Rust agent ↔ `claude` | argv `--flag=value`; env `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1`, `CLAUDE_CODE_SDK_READS_SESSION_STATE=1`; control_request `cancel_async_message{message_uuid}` before `interrupt`; `control_response` allow/deny gain `toolUseID`, `decisionClassification`; inbound frames: `system` `task_started/task_progress/task_updated/task_notification`, result `user_message_uuids` (optional) |
| fake-claude ↔ host | in-order replay; auto-ack set = `get_context_usage`; unexpected frame = exit non-zero (loud failure) |

## Open questions (all resolved by Opus review; OQ-1 needs explicit user sign-off in the final report)

| # | Question | Resolution |
|---|----------|------------|
| OQ-1 | Advertise Node's AIR capability list although Rust implements only `asyncTasks`? | RESOLVED: advertise Node's list (differential parity); each consequence written into its `pending` PARITY row (agentFileChangeReport request gets no reply; customInstructions silently dropped; nativeSubagentSessions gets no child sessions; `_session/goal` returns -32601) and listed in the final report for user sign-off |
| OQ-2 | `agentInfo.version` strategy | RESOLVED: transform-and-assert normalizer (Decision 3), no `JsonPath`; PARITY records the deliberate divergence |
| OQ-3 | Legacy "Always Allow" | RESOLVED: Yes/No default + exact Bash durable option, non-deferrable (3.9) with Node-captured `permission-suggestions` script; Yes/No fallback where Node cannot describe the effect |
| OQ-4 | Cancel family R90-2/R91-3/R86-8 | RESOLVED: keep `pending` |
| OQ-5 | Stamped-result items | RESOLVED: R87-1 in Phase 3, non-deferrable, narrowed to "named uuid is the active turn or queue head" (3.8); R811-6 and R72-2 stay `pending` |
| OQ-6 | Extend `ARGV_ENV_ALLOWLIST` | RESOLVED: yes (2.6) |
| OQ-7 | `sessionCapabilities` advertised, methods `-32601` | RESOLVED: keep + PARITY row (3.14) |
| OQ-8 | Unfinished-tool failure | RESOLVED: port, keyed per `prompt_uuid`, every end_turn settle incl. held turns (3.7, Decision 7) |
| OQ-9 | `/mcp` + `/remote-control` advertised | RESOLVED: advertise both; forward `/mcp` to the CLI; `/remote-control` `skipped-deliberate` |
| OQ-10 | cargo feature `unstable_tool_call_name` | RESOLVED: yes (`skip_serializing_none` -> no `name: null`) |
| OQ-11 | Cache wiped by `cargo clean` | RESOLVED: accept; atomic install + gate prewarm (Decisions 4, 22) |
| OQ-12 | Modified transcripts | RESOLVED: accept, with realistic deny echo (`is_error:true`, deny text) |
| OQ-13 | `--version` format | RESOLVED: accept (Node handles `--version` only after `--cli` delegation, `dist/index.js:8-42`) |
| OQ-14 | AIR Bash `tool_call` shaping / `async-task-bash` | RESOLVED: keep `pending` |
| OQ-15 | Subagent transcripts not real | RESOLVED: keep `pending`; 3.11b dropped |

## Inventory summary (for the report)

- Rows: 144 · SKIP 68 · PORT-E 35 · CLI 30 · PORT-N 6 · informational 5
- HIGH RISK rows: E-8, E-12, R71-10, R72-2, R811-6, R851-4, R86-6, R86-8, R86-9, R87-1, R90-2, R91-3, S-3, S-4, S-8, S-12, S-13
- Phases: 1 "pin + hermetic capture + adoption tooling", 2 "bump to 0.94.0 + corpus-visible ports + fixtures", 3 "AIR async tasks + unfinished tools + new scripts + docs"
