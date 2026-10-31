# Parity ledger — claude-agent-acp-rs vs upstream claude-agent-acp

> Mirror version: **0.94.0** (`porting/UPSTREAM_VERSION`). The differential
> (`cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential`)
> is green at **36 tests** (22 pre-existing + `async-task-air`, `async-task-non-air`, `unfinished-tool`, `folded-prompt`, `permission-suggestions`, rust-air-gaps Phase 1: `system-prompt` x7 scripts, `air-tool-meta`, `async-task-stop`, `inv_49_normalize_embedded_uuid`, and Phase 2: `inv_24_model_usage`, `inv_24_auth_status`, `inv_24_logout_auth`, `inv_24_session_close`, `inv_24_session_methods`).
> New and deferred Phase-3 omissions live in section "(C) 0.71-0.94 inventory".

One row per upstream feature. Status:
- `ported` — implemented and gated
- `skipped-deliberate` — not ported, with a one-line reason (D3)
- `pending` — not yet implemented (only a `pending` row blocks a parity claim)

Seeded from `porting/EVALUATION.md` § 2 (A)/(B). "Parity" means parity of this ported subset (D3);
the crate intentionally does not reproduce upstream's full surface.

## (A) Protocol-necessary core — must port

Status updated at phase 13 (codepath audit, 13.2): each row names the Rust file:line that covers
the upstream range, or `pending`/`skipped-deliberate`. `C` = `rust/claude-agent-acp-rs/src`.

| Feature | Source (acp-agent.js) | Status | Rust coverage |
| --- | --- | --- | --- |
| `initialize` capabilities | `693–746` | ported | `C/agent.rs:76-103` (`initialize_response`), `:508-514` |
| `newSession` / `loadSession` + non-optional `createSession` | `747–759`, `788–798`, `4645–4695`, `4696–5209` | ported | `C/agent.rs:515-562`, `:107-122`, `:247-272`, `:446-493`; `C/session.rs:217-283` (`Session::start`) |
| `prompt` (enqueues a Turn, owns no loop) | `973–1037` | ported | `C/agent.rs:563-641`; `C/session.rs:291-321`, `:504-509`; `C/turn.rs:306-308` |
| Turn activate / settle / orphan accounting | `1338–1682` | ported | `C/turn.rs:457-500`, `:778-809`, `:815-875`, `:904-962` (+ orphan `:263-270`, `:411-452`) |
| Consumer loop + abort/EOF race | `1683–1845` | ported | `C/session.rs:436-682` (`run_loop`); `C/turn.rs:742-773` (`on_stream_end`) |
| `result` → stop reasons | `2532–2884` | ported | `C/turn.rs:510-689`, `:90-115`, `:966-974`; `C/agent.rs:605-633` |
| Idle settle via `session_state_changed` | `2049–2163` | ported | `C/turn.rs:695-736` (`on_idle`); `C/session.rs:801-815` |
| Stream → ACP mappers | `6383–6759`, `6760–6864`, `6303–6382` | ported | `C/map.rs:108-334`, `:406-506`, `:509-648`; routing `C/session.rs:687-724`, `:822-862` |
| Prompt content mapping | `6101–6193` | ported | `C/agent.rs:158-240` (`prompt_to_claude`, `format_uri_as_link`) |
| Permission translation | `4069–4275`, `4017–4068`, `447–541` | ported | `C/permission.rs:51-355`, `:359-467`; wiring `C/session.rs:364-396`, `:873-950` |
| `cancel` | `3443–3668`, `6865–6892` | ported | `C/turn.rs:317-390`; `C/session.rs:510-573`; `C/control.rs:214-216`, `:410-416` |
| fs passthrough | `4002–4016` | pending | not ported — no `fs/*` agent→client passthrough (`readTextFile`/`writeTextFile`); the crate's on-request handler answers only `can_use_tool` (`C/session.rs:364-396`) |
| Wiring | `542–572`, `6893–6929` | ported | `C/agent.rs:376-493`, `:497-713`; `C/session.rs` actor |
| Tool shape mapping | `tools.js:16–357`, `423–717` | ported | `C/tools.rs:81-106`, `:116-420`, `:463-565` |

## (B) Optional — safe to skip

| Feature | Source | Status | Reason |
| --- | --- | --- | --- |
| Config options / modes / fast mode | `3724–3860`, `4388–4644`, `5479–5675` | skipped-deliberate | vibe-station does not surface config/mode options (R9) |
| Model resolution + allowlisting | `5676–6045`, `6930–7114` | skipped-deliberate | resolution is a client concern; not needed by vibe-station |
| Elicitation | `elicitation.js`, `4276–4374`, `2396–2481` | skipped-deliberate | optional extension, not consumed |
| File-change audit (JetBrains/AIR) | `acp-agent.js:734` | skipped-deliberate | JetBrains/AIR extension, not consumed |
| Session-failure ext (JetBrains/AIR) | — | skipped-deliberate | JetBrains/AIR extension; AIR pair = largest skippable chunk |
| Auth / gateway / providers | `595–692`, `850–972`, `5307–5392` | skipped-deliberate | client-side concern, out of scope (the `authStatus` push and `logout` are ported - section (C); `providers/*` stay **pending** there) |
| TODO / plan lists | — | skipped-deliberate | not consumed by vibe-station |
| Settings watching | — | skipped-deliberate | requires reimplementing the SDK merge |
| Subagent transcripts | — | skipped-deliberate | not needed for turn settlement |
| Hooks (PostToolUse/TaskCreated/TaskCompleted/PostModelSwitch/PostCompact) and `promptSuggestions` | `acp-agent.js:417,8482,8563,8597` | **pending** | not registered / not requested; see the hooks row in section (C) for the consequence (no predicted follow-up prompt for AIR) |
| Terminal support | — | skipped-deliberate | not needed |
| Goal extension | — | skipped-deliberate | optional extension |
| Custom slash commands | — | skipped-deliberate | not needed |
| MCP server passthrough | — | skipped-deliberate | `mcpServers` ignored (B2) |

## Other deliberate decisions

| Feature | Status | Reason |
| --- | --- | --- |
| Unsupported ACP methods (`authenticate`, `set_mode`, `_session/goal`, …) | skipped-deliberate | respond `-32601` (B1 table) |
| Discarded `SessionUpdate` variants (`SessionInfoUpdate`, `ConfigOptionUpdate`, `UsageUpdate`) | skipped-deliberate | ignored in the differential; vibe-station discards them (`normalize.rs:471-473`) |
| `session/load` history replay via SDK `getSessionMessages` | skipped-deliberate | D15 — resume only via `--resume=<id>`; vibe-station backfills history itself |
| `suppressControlResponse` (SDK-internal JS symbol) | skipped-deliberate | D6 — the port has no handler that declines to answer |
| `cwd` validation on `session/new`/`load` (issue #749, `dist:4676-4702`) | ported | `C/agent.rs` `validate_cwd` (+ `session/new`/`session/load` calls); `inv_validate_cwd_*` (`C/agent.rs`), `inv_cwd_validation_on_session_new_wire` (`C/tests/agent.rs`) |

## Phase 13 — pending omissions (recorded honestly, per orchestrator)

The crate's ported subset is parity-green on the full synthetic corpus (13.T1). The following
upstream behaviours are NOT ported and are recorded as `pending` (not silently marked ported);
none is exercised by the synthetic corpus:

| Omission | Upstream | Status | Reason |
| --- | --- | --- | --- |
| Steering settlement lanes (`steeredEchoes`/`steeredSettle`) | `acp-agent.js` steering | ported | `C/turn.rs:40-52` (steering paragraph), `mark_steered`; unit `steer_settles_at_idle_not_result` etc. (`C/turn/turn_tests.rs`); differential `inv_24_steer_mid_turn`, `inv_24_steer_error_result` (`C/tests/differential.rs`) |
| EOF mid-steer / `on_stream_end` settles `EndTurn` instead of the scratch stopReason | `dist:1806-1808` | pending | `deferredSettle ?? {stopReason, usage}` never reads `steeredSettle`; Rust's `on_stream_end` settles `EndTurn`, not the scratch stopReason — pre-existing gap, recorded not fixed (OQ4) |
| `msg_lifecycle_v1` / command-lifecycle orphan map | `acp-agent.js:2499` (NOTE) | pending | orphan accounting uses the coarse `pending_orphan_results` count, not per-uuid `command_lifecycle`; `C/turn.rs:50-64` |
| `endedPerLevel` sweep at activation | `acp-agent.js:1366-1390` | pending | `live_subagents` entries removed only by terminal frames; `C/turn.rs:50-64` |
| Transcript replay on `session/load` | SDK `getSessionMessages` | pending (skipped-deliberate by D15) | resume only via `--resume=<id>` |
| `suppressControlResponse` | SDK JS symbol | pending (skipped-deliberate by D6) | no handler that declines to answer |
| Grandchildren of a SIGKILLed host | Risk 17 | pending | INV-28 covers `claude` only; MCP/Bash children share the process group, out of scope for v0.1 |
| 150 ms `sleep_ms` pacing | fixture transcripts | pending | timing-based stabiliser for the permission fixtures, not an ordering step (`fake-claude`) |
| `cwd` validation (issue #749) | `dist:4676-4702` | ported | `validate_cwd` in `C/agent.rs` (see above) |
| fs passthrough (`fs/*` agent→client) | `dist:4002-4016` | pending | not ported (above) |

## Phase 13 — corpus provenance (13.1)

All 26 corpus transcripts under `porting/corpus/` are **synthetic (hand-authored)** and their Node
fixtures were recorded against `fake-claude`, NOT a real `claude`. They must be re-recorded from the
real CLI via `porting/record-real.sh` before any parity claim against real `claude` is relied on.
This is a known open item (2.3 / orchestrator constraint), not a failure of 13.T1.

## Phase 13 — reverse audit of upstream comments (13.4)

`grep -nE "issue #|NOTE|Deliberately" src/acp-agent.ts` (v0.70.0) classified:

| Upstream ref | src line(s) | Class |
| --- | --- | --- |
| issue #453 (result-text fallback) | 694, 2474, 3950 | ported — `inv_29_result_text_fallback` (`C/turn/turn_tests.rs:103`) |
| issue #596 (usage-window cache across restart) | 6714 | skipped-deliberate — model/usage resolution (B section) |
| issue #680 (wedged-consumer force-cancel) | 566, 2217, 2755, 4858 | ported — `inv_17_force_cancel_floor` (`C/session.rs:1606`) |
| issue #749 (cwd validation) | 6192 | ported — `validate_cwd` (`C/agent.rs`) |
| issue #773 (result settles non-subagent turn) | 3143 | ported — `settle_clears_active_slot`, `result_for_active_turn_while_queued_settles_active` (`C/turn/turn_tests.rs`) |
| issue #825 (idle without result) | 2141–2153 etc. (10 refs) | ported — `inv_31_idle_without_result_fails` (`C/turn/turn_tests.rs:203`); NoResult `errorKind` fixed in 13.1 |
| issue #844 (cancelled-turn usage) | 3169, 4805 | ported — `held_cancel_reports_deferred_usage` (`C/turn/turn_tests.rs:1018`) |
| issue #845 (restore live model) | 7645 | skipped-deliberate — model resolution (B section) |
| issue #851 (permission refs a seen tool call) | 3345, 5386, 8447 | ported — `inv_20_permission_after_toolcall` (`C/permission.rs:477`) |
| issue #863 (`/login` auth-failed text suppressed → `authRequired`) | 1232, 5210 | ported — `AuthRequired` path in `session_error_to_rpc` (`C/agent.rs:329,345`) + `auth_required_and_error_join`; the replay half is skipped-deliberate (D15) |
| issue #866 (out-of-turn permission deadlock / subagent hold) | 384, 2610, 3893 | ported — `inv_16_subagents_hold_settle` (`C/turn/turn_tests.rs:53`), `inv_27_no_inline_await` (`C/session.rs:1330`) |
| NOTE (orphan-command coalescing ordering) | 2499 | pending — the `msg_lifecycle_v1` orphan map it documents is not ported (above) |
| Deliberately (steer-lane ordering) | 3202 | ported — `steer_settles_at_idle_not_result` etc. (`C/turn/turn_tests.rs`), `inv_24_steer_mid_turn`/`inv_24_steer_error_result` (`C/tests/differential.rs`) |
| Deliberately (idle fails only the ACTIVE turn) | 3232 | ported — `C/turn.rs:727-734` (`on_idle`) |
| Deliberately (fail-OPEN unknown permission kind) | 774 | ported — `C/permission.rs` map_outcome default |
| Deliberately (no quiet-period timer on cancel) | 3466 | ported — force-cancel floor `C/session.rs:536-539` |
| Deliberately (queued turn reports no usage) | 4742 | ported — cancel-sweep usage omission `C/turn.rs` |
| Deliberately (does NOT abort `session.abortController`) | 4935 | ported — teardown via transport EOF / runtime drop (12.2) |
| Deliberately (non-subagent background → false) | 655 | ported — subagent attribution `C/session.rs:461, 873-950` |
| Deliberately (NOT reset on turn activation) / (user wants to know) | 705, 7116 | skipped-deliberate — model/context resolution (B section) |

## Verification pass (verify-01, sonnet, 2026-09-22) — findings recorded, not fixed

| # | Finding | Evidence | Disposition |
|---|---------|----------|-------------|
| 1 | On a client-side malformed JSON-RPC line (ACP client → our binary's stdin — NOT the `claude` child's stdout, which INV-1 already covers silently), the response echoes the raw offending line in `error.data.line` | `agent-client-protocol-2.1.0/src/util.rs:18-24,66-67` (pinned third-party crate, not our code) | Accepted — behavior of the pinned `agent-client-protocol` dependency (D9's exact-version pin), out of scope to patch; not part of the Node-adapter wire-parity contract (that covers only the `claude` subprocess protocol) |
| 2 | The public API has no explicit `shutdown()`/`dispose()` — closing/dropping the caller's `Channel::duplex()` end is the documented-in-comments-only shutdown signal for `serve()` | `C/src/agent.rs:376` | Accepted for v0.1; flag for the vibe-station-side follow-up plan (`acp_connection.rs` integration) so the "drop the Channel end to stop" contract is stated in that plan too |
| 3 | `text-only`, `single-tool`, `cancel-mid-turn` fixtures were re-captured against a freshly built real Node adapter and diff byte-identical to the committed fixtures | verify-01-report.md item 4 | 3 of 16 corpus scripts are now confirmed real-Node-recorded, not merely hand-authored; the other 13 still need `porting/record-real.sh` before a full parity claim |
| 4 | Full-parent-env inheritance to the spawned `claude` (matching Node's `{ ...process.env, ...providerEnv }`, `acp-agent.js:4851`) is empirically confirmed, not just inferred from the absence of `.env_clear()` | `process.rs:440-447`; verify-01-report.md item 6 (custom env var observed reaching the child) | Confirmed working — no action |

## Phase 14 — ported + investigated-not-ported (orchestrator-verified items 1-5)

| Item | Upstream | Status | Evidence / reason |
| --- | --- | --- | --- |
| `cwd` validation (#749) | `dist:4676-4702` | **ported** | `validate_cwd` + `session/new`/`session/load` calls (`C/agent.rs`); `inv_validate_cwd_rejects_relative_and_missing`, `inv_validate_cwd_accepts_absent_and_existing_dir` (`C/agent.rs`), `inv_cwd_validation_on_session_new_wire` (`C/tests/agent.rs`) |
| fs passthrough (`readTextFile`/`writeTextFile`) | `dist:4002-4016` | not ported — **upstream unwired dead code** | agent methods forward `fs/read_text_file`/`fs/write_text_file` to the client, but have ZERO call sites and are never registered as ACP request handlers (`runAcp` has no `agent/fs` method; ACP protocol has no `agent/fs/*` namespace). `canUseTool` never invokes them — file tools run natively in the `claude` CLI; the adapter only forwards the permission ask (`session/request_permission`). The crate's permission-only handling already matches the live adapter; porting would invent an un-triggered feature |
| `msg_lifecycle_v1` / command-lifecycle orphan map | `acp-agent.js:2499` NOTE | not ported — **STOP per plan (risky)** | replacing the coarse `pending_orphan_results` count with the per-uuid `orphanCommands` map requires (a) processing `command_lifecycle` stream frames (CLI 2.1.206+), which neither the crate nor its synthetic corpus produces, and (b) rewriting the review-04/05-verified orphan/reconcile/activate semantics (INV-23/30). Marked not done per plan's explicit "STOP rather than guess" instruction |
| `endedPerLevel` sweep at activation | `acp-agent.js:1366-1390` | not ported — **STOP per plan (risky)** | requires (a) processing `background_tasks_changed` frames the crate/corpus never produce and (b) an `ended_per_level` field + two-phase sweep inside `activate`, touching the phase-6/04-05-verified live_subagents semantics. Same risk class as item 3 |
| Steering settlement lanes (`steeredEchoes`/`steeredSettle`) | `acp-agent.js` steering | **ported** | the lanes thread through the core turn machine at 5 interleaved points (result settle `settleOrDefer`, idle settle 2120-2140, move-on hand-off, trailing-idle `owedTrailingIdles` exclusions, cancel orphan-seeding) — all implemented and gated by `inv_24_steer_mid_turn` / `inv_24_steer_error_result` (`C/tests/differential.rs`) plus the `steer_*` unit tests (`C/turn/turn_tests.rs`) |

## Phase 15 — post-tag real-`claude` parity fixes (verify-03 findings, done)

Closing the three real, reproducible divergences found by the verify-03 real-`claude` pass
(`verify-03-real-claude.md`). The two non-bugs it also flagged — `usage_update`/`session_info_update`
(`skipped-deliberate`, row above) and the permission request-id int-vs-UUID difference — are left
untouched, as instructed.

| Item | Upstream | Status | Evidence |
| --- | --- | --- | --- |
| Proactive `available_commands_update` after `session/new`/`session/load` (real commands, no `commands_changed` needed) | `sendAvailableCommandsUpdate` `dist:4375-4387` → `supportedCommands()` reads `initialization.commands` | **ported** | `Session` now stores the initialize handshake's `commands` (`C/session.rs` `Session.commands`, `Session::start`); `agent.rs` builds the update via the existing `map::map_commands_changed(commands, [])` right after `session/new`/`session/load` (replacing the old empty update). `inv_24_commands_proactive` differential (`C/tests/differential.rs`) + new `commands-proactive` corpus/fixture assert a non-empty `availableCommands` on the wire without any `commands_changed` frame |
| Active-turn cancel reports `usage` even when all-zero; queued-sweep reports no `usage` | `settleActive({..., usage: sessionUsage(session)})` vs `turn.resolve({stopReason:"cancelled"})` | **ported** | added `had_usage: bool` to `TurnEvent::Settled` (`C/turn.rs`): `false` only for the pure queued-sweep, `true` for every active/held settle path; `session.rs` keys off it instead of `total_tokens == 0`. Tests: `cancelled_active_turn_with_no_tokens_reports_zero_usage`, `cancelled_queued_sweep_reports_no_usage` (`C/turn/turn_tests.rs`), `item2_cancel_usage_keys_off_had_usage_origin` (`C/session.rs`) |
| Terminal `tool_call_update{status:"completed"}` (+ tool output content) for a plain top-level tool call | `toAcpNotifications` on the `user` `tool_result` message (`dist:3264-3283`) | **ported** | the `"user"` arm of `handle_session_frame` now maps the user message's `tool_result` content via `map::map_consolidated` (mirroring upstream's echo-skip, `dist:3165-3171`), so the completing update reaches the client. `item3_read_tool_turn_ends_with_completed_tool_call_update` (`C/session.rs`) |

Phase-15 notes:
- The `_meta.claudeCode.toolResponse` refinement frame (`dist:2194`/`toolResponse:`) that Node emits
  for the tool result is **not** reproduced — the phase-15 scope is the `status:"completed"` + content
  update the regression test requires; the `toolResponse` frame is a separate refinement and remains
  a known, non-blocking gap.
- The differential's `availableCommands` `input` field is normalised (Rust omits `input:null` because
  the pinned schema's `AvailableCommand` is `#[skip_serializing_none]`; Node emits it explicitly) —
  same class of crate-inherent serialization default as the `tool_call` `status`/`content` handling
  (`C/tests/common/mod.rs` `normalize_available_commands_input`).

## (C) 0.71-0.94 inventory — Phase-3 (rust-upstream-sync)

The mirror moved from 0.70.0 to 0.94.0. Section (A) line refs above are for the 0.94.0 adapter
(`dist/acp-agent.js` etc., installed at `rust/target/upstream/0.94.0/`). The newly-ported and the
still-`pending` items from Phase 3 are recorded here; a `pending` row is the honest ledger for a
behaviour the crate advertises but does not honour, or that is not yet ported.

| Feature | Upstream ref | Status | Reason / consequence / Rust coverage |
| --- | --- | --- | --- |
| Async tasks (`async_task_spawned`/`progress`/`state_update`) for AIR `asyncTasks` clients | `dist/async-tasks.js` | **ported (with gaps, see the `_session/async_task/stop` and async-task rows below)** | `C/async_tasks.rs` (pure `AsyncTaskRuntime`), wired in `C/session.rs` (task arms + `release_held` on settle + `finish_all` at stream end); `RawUpdate` via `C/agent.rs`. Scripts `async-task-air`/`async-task-non-air` + `inv_24_async_task_air`/`_non_air` differentials + `inv_40_*` unit tests |
| AIR `_session/async_task/stop` | `dist/acp-agent.js` `stopAsyncTask`, `dist/async-tasks.js` `claimStop`/`taskStopped` | **ported** | `Command::StopAsyncTask/TaskStopped/ReleaseStop` (`C/session.rs`), `AsyncTaskRuntime::claim_stop/release_stop/task_stopped` (`C/async_tasks.rs`), param validation `parse_async_task_stop_params` (`C/agent.rs`). Order = state update `stopped`, `**Task stopped by user:** <name>.` chunk, `{stopped:true}`, never before its updates; unknown session / task -> `{stopped:false}`. Script `async-task-stop` + `inv_24_async_task_stop`, tests `inv_49_*` |
| Async-task details not ported | `dist/async-tasks.js` | **pending** | `when_announced_state` drops a running/paused change on a held task where Node defers it; `merge_tool_result_path` is a no-op; `toolResults`, the `HIDDEN_TASK_TOOLS` (Monitor) filter and `backgroundTasksChanged` are not ported |
| AIR capability detection | `dist/air-extension.js` | **ported** | `is_air()` mirrors `isAirClient`; `supports(cap)` mirrors `clientSupportsAirCapability` (`C/air.rs`, Opus MAJOR on Phase 2; tests in `C/air.rs`) |
| `agentInfo.version` composed `<crate>+upstream.<pin>` | Decision 3 | ported (deliberate divergence) | normalizer `normalize_agent_info_version` (`C/tests/common/mod.rs`); the fixture value is rewritten, Rust's compared exactly — a wrong Rust value fails the diff (`inv_35_agent_info_normalizer_does_not_rewrite_rust_value`, `C/tests/version.rs`) |
| Advertised AIR capabilities Rust does not implement | Decision 11 / OQ-1 | **pending** | advertised verbatim (`C/agent.rs` `initialize_response` list) but inert: `agentFileChangeReport` request gets NO reply; `nativeSubagentSessions` gets no child sessions; `_session/goal` returns `-32601`; `diffPatch`/`planFile`/`recommendedValue`/`sessionFailure` inert — each is a `pending` consequence, listed for explicit user sign-off (OQ-1) |
| `authStatus` marker (G2) | `dist/acp-agent.js` E-2, `dist/auth-status.js` | **ported** | `C/auth_status.rs` (`from_cli_status`, `from_account_info`, `merge`, `same_status`, `plan_label`, the AIR-gateway `apiKeyHelper` match) + a single-owner actor (`spawn_auth_actor`): `claude auth status --json` (5 s bound, stdin null, stdout parsed on a non-zero exit) is probed after the `initialize` response, the session account is published at `session/new\|load\|resume\|fork`, and a session cwd that differs from the last probe's re-probes; `_auth/status_update {authStatus}` is pushed only when the payload changed (`apiKeyHelper` read from the user / project / local `settings.json`, last wins). Script `auth-status` + `inv_24_auth_status`, tests `inv_50_*`. Not ported (**pending**, consequence in parentheses): the per-prompt re-probe (a login or logout done in another terminal is not noticed until the next session, `logout` or `initialize`), ACP gateway `authenticate` / `providers/set` overrides (the status never reads `gateway` from an ACP request), the `--hide-claude-auth` guard and its epoch bump, the `vendor` bag |
| `sessionCapabilities` close/resume/list/delete/fork (G6) | E-3, `dist/acp-agent.js` `closeSession`/`resumeSession`/`listSessions`/`deleteSession`/`unstable_forkSession`, `dist/fork-session.js`, SDK `listSessions`/`deleteSession`/`forkSession` | **ported (partial)** | `session/close` cancels like `session/cancel`, sends `interrupt`, ends the actor and answers `{}` (idempotent); `session/resume` is the `session/load` start path (`--resume`; Rust never replays history); `session/list` reads `<CLAUDE_CONFIG_DIR>/projects/<slug>/*.jsonl` (`C/session_store.rs`: slug + hash for >200 chars, newest first, title customTitle > aiTitle > lastPrompt > summary > first prompt, `offset:N` paging of 1000, `cwd` matched on the REAL path); `session/delete` (non-AIR) removes the transcript and its session dir, an unknown session or invalid id succeeds and a loaded session is torn down first; `session/fork` copies the LATEST transcript (fresh uuid per entry, `progress` and sidechain entries dropped, `parentUuid`/`logicalParentUuid` remapped, `sessionId` rewritten, `forkedFrom`, closing `custom-title "<t> (fork)"`) then starts `--resume=<new id>`; `session/prompt` on a closed / unknown session is `-32603 data{details:"Session not found"}`. Scripts `session-close`, `session-resume`, `session-list`, `session-fork` + `inv_24_session_close`, `inv_24_session_methods`, tests `inv_52_*`. **pending** (see the rows below): AIR `session/delete`, fork at a message, git-worktree siblings in a `cwd` listing, the sidecar title store, `compactMetadata` / attachment uuid remapping of a fork, file mode 0600 of the fork file (G9 forbids `cfg(unix)` outside `process.rs`) |
| AIR `session/delete` (archive) | `dist/session-index/service.js` `archiveInsteadOfDelete` (Decision 22) | **pending** | an AIR client (without `sessionIndex`) gets `-32601` where Node ARCHIVES the session (custom-title prefix, transcript kept); hard-deleting would destroy data Node keeps. Consequence: an AIR user cannot mark a session done; the `delete` capability is still advertised |
| Fork at a message (`_meta.jetbrains.air.fork`) | `dist/fork-session.js` `forkPoint` / `resolveFromFullHistory` | **pending** | `session/fork` with a fork point answers `-32602 "Fork point message <id> was not found in session <sid>"` (data `{messageId}`) where Node resolves the message (live map, active history, full history, fingerprint + occurrence) and forks up to it. Consequence: AIR cannot branch from an earlier turn; plain `session/fork` forks the whole latest chain |
| `/mcp` | OQ-9 | **pending** | forwarded to the CLI as a plain prompt (no native MCP handler) |
| `/remote-control` | OQ-9 | skipped-deliberate | advertised for list parity; handler `skipped-deliberate` |
| Cancel family R90-2 / R91-3 / R86-8 | research §2 | **pending** | high-risk, corpus-invisible, needs real-claude evidence (`porting/record-real.sh`) |
| Stamped-result lanes R811-6 / R72-2 | research §2 | **pending** | not ported |
| Subagent transcripts (E-19) not made real | OQ-15 | **pending** | held-turn lanes not exercised by a real subagent; pre-existing |
| Context compaction tool lifecycle R75-2 | research §2 | **pending** | pre-existing |
| AIR Bash tool shaping / `async-task-bash` | OQ-14 | **pending** | pre-existing |
| Unfinished tools (`incomplete_tool_call`, `-32603`) | E-12 / R851-4, `dist/acp-agent.js:3559-3605` | **ported** | `C/unfinished.rs` post-filter (`gate_events`, books keyed by `prompt_uuid`, `MapState.books`), wired in `C/session.rs` (`settle_events`, `note_tool_updates`); `TurnMachine` debt books untouched. Script `unfinished-tool` + `inv_24_unfinished_tool`, tests `inv_41_*`. Not ported: the cancel-time closing of open calls (`closeCancelledToolCalls`, cancel family) - consequence: after a cancel, open tool calls are left without a closing `cancelled` update; on clients with AIR `sessionFailure` Node reports `incomplete_tool_call` as a session failure plus `end_turn`, while Rust sends `-32603` (the `sessionFailure` capability is advertised but inert) |
| Folded prompt (autonomous result answers a pending prompt) | R87-1, `dist/acp-agent.js:3205-3228` `answersPendingPrompt` | **ported** | `TurnMachine::result_is_autonomous` (Node rule: ANY unsettled, non-held turn named in `user_message_uuids`/`user_message_uuid`; wider than the plan's active/queue-head wording). Script `folded-prompt` + `inv_24_folded_prompt`, tests `inv_42_*`. Un-stamped / non-matching results unchanged (`inv_30_*`, `inv_16_*` unmodified) |
| Durable Bash permission (`allow-with-updates`, `user_permanent`) | OQ-3, `dist/permissions/options/shell.js` & `effects.js` | **ported** | `C/permission.rs` `durable_bash` (Node's `normalizeDurablePermissionChangeSet` + commands-only `shellSuggestionsLabel`), `Outcome::AllowWithUpdates`, `allow_with_updates_control_response` (suggestions verbatim, `user_permanent`); recorder policy `allow_always`. Script `permission-suggestions` + `inv_24_permission_suggestions`, tests `inv_47_*`. Also fixed `map_outcome` to read the real nested `result.outcome.{outcome,optionId}` (before, EVERY allow selection was mapped to a deny on the real wire) |
| Durable permission forms Rust does not describe | `dist/permissions/options/shell.js` (read paths / directories), `options/filesystem.js`, `options/tools.js` | **pending** | Bash suggestions carrying `Read` rules or directories, and every non-Bash/PowerShell tool (Edit, Write, WebFetch, Skill, MCP...) get Yes/No only where Node offers a durable "don't ask again" / "allow all edits" choice; the user must re-approve each call |
| Permission option order for `default_to_no` | `dist/permissions/options.js` `declineFirstOptionOrder` | **pending** | Node lists the reject option first when the CLI asks to default to No; Rust always lists Yes first |
| **3.10** Per-model quota rows (`model_usage`) (G10) | R71-8, `dist/acp-agent.js` `normalizeModelUsage`/`modelUsageIncrement`/`addModelUsage`/`resumedFirstResultModelUsage`/`getMatchingModelUsage` (`4946-4965`, `9099-9290`) | **ported** | `C/quota.rs` (pure) + `TurnMachine` side-fields (`accumulated_models` reset with `accumulated_usage`, session-long `last_model_reading` never reset, `awaiting_resume_seed` for a resumed session, `last_assistant_model` fed from top-level `message_start` / `assistant` frames); `TurnEvent::Settled.model_rows` (an `Arc` snapshot, so a held / steered turn keeps the rows its own result recorded); the prompt response renders `_meta.quota.model_usage:[{model,token_count}]` in insertion order, a rewound reading is its own increment, a zero-total model is dropped, an autonomous result advances the reading without touching the turn tally. Script `model-usage` + `inv_24_model_usage`, tests `inv_48_*` (quota + machine). Row order relies on `serde_json`'s `preserve_order` (enabled workspace-wide by feature unification) |
| `auth.logout` (G2 / 2.4) | `dist/acp-agent.js:2025-2075` `logout` | **ported (partial)** | `logout` runs `claude auth logout`, then a FRESH `auth status` read that supersedes any probe already running; an unreadable store pushes `none`; the status push is ordered before the `{}` response; a CLI failure is `-32603 "claude auth logout failed: ..."` with `data.stderr`. Script `logout-auth` + `inv_24_logout_auth`. **pending**: clearing ACP gateway credentials / `providers/set` routing (Rust never stores them), the context-window cache reset |
| `providers/list`, `providers/set`, `providers/disable` | `dist/acp-agent.js:1899-1960`, `defaultProviderConfig` | **pending** | `providers: {}` is advertised but all three return `-32601`. Node's `list` is env-derived (`CLAUDE_CODE_USE_BEDROCK` / `VERTEX`, `ANTHROPIC_*_BASE_URL`) and `set` / `disable` recreate every live session under the new routing, so they were not bounded. Consequence: a client cannot discover or change the model backend through ACP (the process environment is the only control) |
| Hooks (`PostToolUse`, `TaskCreated`, `TaskCompleted`, `PostModelSwitch`, `PostCompact`) and `promptSuggestions` | `dist/acp-agent.js:417,8482` (`promptSuggestions: true`), `8563` (`PostModelSwitch`), `8597` (`PostCompact`), `2504-2526` (`session_info_update._meta.promptSuggestions`) | **pending** | the Rust `initialize` control_request carries no `hooks` and no `promptSuggestions`, so the CLI never predicts a follow-up prompt: an AIR client never receives `session_info_update._meta.promptSuggestions`, and the model-switch / compact hook callbacks Node drives are absent (the `inv_39` initialize comparison drops both keys on BOTH sides) |
| AIR `rawInput` stripping of file text (`fileTextKeys`) | `dist/tool-calls/renderer.js:534-548` | **pending** | for an AIR client Node removes the file-text keys the diff already holds (Write: the content key; Edit: `old_string` / `new_string`, `tool-calls/reporters/file-edit.js:38,95`) from `rawInput`; Rust sends the whole `rawInput`. Consequence: AIR receives large file bodies twice (as `rawInput` text and as diff content) |
| **3.11** `systemPrompt` honouring (G3) | X-4, `dist/client-system-prompt.js` | **ported** | `C/system_prompt.rs` (`client_system_prompt`, `sdk_initialize_fields`), wired into `session/new` and `session/load` after `validate_cwd`, before any spawn; `InitializeOptions.append_system_prompt`. `append` -> `appendSystemPrompt`, `override` -> `systemPrompt:[text]`; unsupported mode / non-text block -> `-32602` with the Node's message and `data`. Scripts `system-prompt-append|override|load|bad-mode|bad-block|combined` + `inv_24_system_prompt`, `inv_39_*`. The `_meta.systemPrompt` extension (string / preset object) stays **pending**: it is ignored |
| AIR `customInstructions` (G5) | `dist/air-extension.js` `airCustomInstructions` | **ported** | `_meta.jetbrains.air.customInstructions` (ungated) joins after the client text with a blank line and rides `appendSystemPrompt` (alone, or with `override` inside `systemPrompt`). Scripts `custom-instructions`, `system-prompt-combined`; `tests/system_prompt.rs::inv_39_initialize_matches_node` compares the Rust `initialize` request with `porting/fixtures/<name>.init.json` (the SDK `hooks` payload and `promptSuggestions` are **pending** - see the hooks row - and dropped on BOTH sides) |
| AIR tool `_meta` (G11) | `dist/tool-calls/renderer.js` `toolUseMeta`, `changed-meta-filter.js` | **ported** | `tools::air_tool_meta` (`commandTitle`, `subagent`, `skill {name,path?}` via `resolve_skill_path`, `mcp {server,tool}`, always stamped `version:1`), top-level `is_mcp_tool_call:true` on AIR `mcp__*` tool calls (also on the permission path), `C/meta_filter.rs` (`ChangedMetaFilter`: unchanged keys stripped from later updates of a call, an emptied update dropped, 2048 calls remembered). Script `air-tool-meta` + `inv_24_air_tool_meta`, tests `inv_51_*` |
| AIR tool rendering beyond `_meta` (found by the `air-tool-meta` capture) | `renderer.js` `toolCall` / `partialRefinement` / `result` | **ported (partial)** | an AIR session sends no `rawInput` while a tool input streams (`tool_call` at the stream start, partial refinements), and the AIR `rawOutput` rule (no `rawOutput` when content carries the result; a read / search naming a path shows viewed files: text content and `rawOutput` dropped). **pending** for AIR: the terminal channel (`terminal_info`/`terminal_exit` need the unadvertised `terminal_output` capability), edit `diffPatch`, `planFile`, `rawInputRendering` content, permission-request tool_call shape (`permissionToolCall` sends only id/title/rawInput), `contextCompaction`, `exit_code` numbers |
| Steer priority `"later"` while a permission is open | R71-10 / R86-6, `dist/acp-agent.js:2646` | **ported** | `C/session.rs` Steer arm (`pending_permissions` non-empty -> `later`); `inv_43_steer_later_while_permission_open`. Elicitations are not ported, so only permissions count |
| **3.13** R811-8 informational frames; R811-9 Write `path`/`file_text` aliases; R77-1 `allowDangerouslySkipPermissions:false`; S-13 `sdk_host_only` idle regression | research §2 | **pending** | pending outright — no work this plan |
| Cancelled / errored permission `decisionClassification` | `dist/permissions/effects.js:5` (`parseClaudePermissionSelection` throws "Tool use aborted") | **pending** | Node throws "Tool use aborted" for a non-selected (cancelled/errored) permission; Rust sends a `deny_control_response` with `decisionClassification:"user_reject"` (`C/session.rs`; D4). Corpus-invisible; the exact wire shape is SDK-internal — recorded, not mirrored (3.17-D) |
| **0.71-0.94 SKIP ranges** (v2 surface, session index, native subagents, auth/gateway/models/config, elicitation, hooks, terminal, replay, forks, notices) | research §2 | skipped-deliberate | section (B); unchanged on the bump |
