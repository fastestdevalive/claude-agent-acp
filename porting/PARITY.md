# Parity ledger — claude-agent-acp-rs vs upstream claude-agent-acp

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
| Auth / gateway / providers | `595–692`, `850–972`, `5307–5392` | skipped-deliberate | client-side concern, out of scope |
| TODO / plan lists | — | skipped-deliberate | not consumed by vibe-station |
| Settings watching | — | skipped-deliberate | requires reimplementing the SDK merge |
| Subagent transcripts | — | skipped-deliberate | not needed for turn settlement |
| Hooks (PostToolUse/TaskCreated/TaskCompleted) | — | skipped-deliberate | SDK hooks, not consumed |
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
| Steering settlement lanes (`steeredEchoes`/`steeredSettle`) | `acp-agent.js` steering | pending | documented `C/turn.rs:50-64`; 12.1 ports only the idle/validation + `now`-injection outcomes |
| `msg_lifecycle_v1` / command-lifecycle orphan map | `acp-agent.js:2499` (NOTE) | pending | orphan accounting uses the coarse `pending_orphan_results` count, not per-uuid `command_lifecycle`; `C/turn.rs:50-64` |
| `endedPerLevel` sweep at activation | `acp-agent.js:1366-1390` | pending | `live_subagents` entries removed only by terminal frames; `C/turn.rs:50-64` |
| Transcript replay on `session/load` | SDK `getSessionMessages` | pending (skipped-deliberate by D15) | resume only via `--resume=<id>` |
| `suppressControlResponse` | SDK JS symbol | pending (skipped-deliberate by D6) | no handler that declines to answer |
| Grandchildren of a SIGKILLed host | Risk 17 | pending | INV-28 covers `claude` only; MCP/Bash children share the process group, out of scope for v0.1 |
| 150 ms `sleep_ms` pacing | fixture transcripts | pending | timing-based stabiliser for the permission fixtures, not an ordering step (`fake-claude`) |
| `cwd` validation (issue #749) | `dist:4676-4702` | ported | `validate_cwd` in `C/agent.rs` (see above) |
| fs passthrough (`fs/*` agent→client) | `dist:4002-4016` | pending | not ported (above) |

## Phase 13 — corpus provenance (13.1)

All 17 corpus transcripts under `porting/corpus/` are **synthetic (hand-authored)** and their Node
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
| Deliberately (steer-lane ordering) | 3202 | pending — steered settlement lane not ported |
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
| Steering settlement lanes (`steeredEchoes`/`steeredSettle`) | `acp-agent.js` steering | not ported — **not additive** | investigation shows the lanes are NOT additive: they thread through the core turn machine at 5 interleaved points (result settle `settleOrDefer`, idle settle 2120-2140, move-on hand-off, trailing-idle `owedTrailingIdles` exclusions, cancel orphan-seeding). Porting means the same core-machine surgery the plan cautions against; deferred |

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
