<!--
RULES — read before writing or implementing:
1. FORMAT: Bullets, tables, code, diagrams ONLY — no prose paragraphs
2. REQUIREMENTS: One crisp line each — no verbose descriptions
3. CHECKLIST: Mark items [x] as you complete them — this is your persistent todo list
4. READING TIME: Optimize for fast human scanning — if it's hard to skim, rewrite it
-->

# Plan: parity-closeout (P7-P10 closeout after the Opus review of parity Phase 3)

> Four sub-packages PC-1..PC-4 executed inside `claude-master` stage 2. The common rules, gate and fan-out
> ground rules are in the lead brief (`lead-common.md`); this plan holds the work items and the findings.

## Starting point

- HEAD `aff399a` contains parity Phases 1-2, Phase 3 turn 3a (hooks, promptSuggestions frames, Task plan), turn 3b (session/load replay), item 3.6 (permission option sets), F1 (ExitPlanMode selection handling + offered-option guard).
- Reference: upstream pinned worktree `<scratch>/upstream-wt/v0.94.0` (src/*.ts, src/tests/*.test.ts) and `rust/target/upstream/0.94.0/node_modules/@agentclientprotocol/claude-agent-acp/dist`.
- Parked corpus (restore in PC-3): `.vibekit/feature-plans/pending/parity-closeout/parked-exit-plan/{exit-plan-auto,exit-plan-default}.{acp.json,transcript.jsonl}` and `{...}.{argv.json,frames.jsonl}` fixtures (frames/argv are Node-captured; regenerate with `bash porting/capture.sh exit-plan-auto exit-plan-default` after the fix).

## PC-1 — HOOKS + TASK* PLAN (files: `hooks.rs`, wiring in `session.rs` / `map.rs`; tests; corpus)

Each fix: read the dist first, write a test that fails before and passes after.

- [ ] PC-1.1 `session.rs` ~1958/~1934: PostCompact is acknowledged then ignored — Node records the compaction summary (`acp-agent.js` ~8602); PostModelSwitch only writes the raw `to_model` into the model option (~1945) — Node runs `syncModelAfterExternalSwitch` (context-window re-seed, mode clamping, effort options; `to_model` may not be a valid option value). Port both exactly.
 - OPEN (review PC-1): PostCompact recording is done, but the recorded summary still has no consumer (F20 / FEATURES R-03); the fast-mode reconcile on PostModelSwitch is missing (O-28, M-37). Findings 3-5 are fixed.
- [ ] PC-1.2 `map.rs` ~249/~710: "hook pending" comes from `pending_hooks` set only on first surface, so a Write/Edit already surfaced by a PERMISSION prompt never gets it and its tracker entry is dropped early; Node uses `hasHookCallback`. Also on grace expiry `prune_expired` must call `finish_hook_tool_tracker` (Node `onRelease`); `HookRegistry::clear` is dead code — use or remove.
 - OPEN (review PC-1): `HookRegistry::clear` does not exist (removed as dead code; no tests referenced it). hasHookCallback and the expiry release are done; the teardown release (Node `clearHookCallbacks`, acp-agent.ts:2798/8202/8304/8468/8500/8882) is not wired yet (F9/F18).
- [x] PC-1.3 `hooks.rs` ~611 `inv_64_enter_plan_mode_publishes_mode` is VACUOUS (asserts its own JSON literal): drive the real code path and assert the published mode update; audit every other hooks.rs test for the same flaw.
- [x] PC-1.4 `hooks.rs` ~52: error reply must carry an `error` string like Node's; `register_once` refuses re-register where Node REPLACES; `&line[1..]` ~178 can panic on a multibyte first char — char-safe slicing.
- [x] PC-1.5 Task* (`hooks.rs` ~343/~427): TaskUpdate must use `input.status ?? existing.status` (owner/description-only update must not reset progress); port the `parseTaskUpdateOutput` success gate, `tool_use_result` parsing and TaskList strictness (unparseable output must not wipe state; owner/blocked-by suffixes must not stay in the subject); a Task* call already surfaced by a permission prompt is RESOLVED by Node (`acp-agent.js` ~10154) while Rust leaves it pending — fix.
 - done: status fallback (T-24), success gate + `tool_use_result` preference, TaskList strictness/order, permission-surface resolution and the settle clearing (commit 5c977c3; differential `inv_24_task_perm_settle` passes). T1-G2a added the remaining Task* parsing forms (T-23), the output-id-only TaskCreate (T-25), TaskGet silence (T-22) and the TaskCreated/TaskCompleted empty-field rejection (T-77).
- [x] PC-1.6 Corpus: Node's `tools.js` ~278 says PostToolUse normally follows tool_result but the corpus only tests hook-first: add a Node-captured script with tool_result FIRST then the hook; differential test.
 - done: porting/corpus/hook-result-first (tool_result before the hook; Node-captured fixture; test inv_24_hook_result_first)
 - done: rust-parity-fixes 3.0 reworded (LIVE tee NOT RUN; covered by PC-1.6)
 - done: pending rows are FEATURES O-129 (options.hooks, MISSING F4a), T-02/T-44 (diffPatch), U-32 (marker-only toolResponse, PARTIAL)
- Suggested ownership: subagent A = `hooks.rs` + its tests (PC-1.3/1.4/1.5); lead or ONE subagent at a time = `session.rs`/`map.rs` wiring (PC-1.1/1.2); subagent B = the PC-1.6 script (lead runs the capture).

## PC-2 — SESSION/LOAD REPLAY fixes (files: `transcript.rs`, replay wiring in `agent.rs`/`session.rs`; seeds, corpus)

- [x] PC-2.1 `transcript.rs` ~454: the SDK filter (`Iwe` in `sdk.mjs`) drops every `isMeta` record unless origin kind is channel/observer/observer-activity/slack-ping, or peer WITH `senderTaskId`; Rust keeps all, so caveats/skill expansions replay as `user_message_chunk`. Port the exact filter and `isCompletedLocalCommand`.
  - done: `is_filtered_meta_record`/`meta_origin_kept` (transcript.rs:70-88), `isCompletedLocalCommand` from `vwe` (transcript.rs:309-341); marker-only command without `<command-args>` keeps the command name (B-48 divergence fixed in the same path).
- [x] PC-2.2 `transcript.rs` ~404: leaf = the SDK's terminal-node walk (excluding teamName/progress records), not "latest user/assistant"; the read is a blocking `read_to_string` without cap — cap/stream like the SDK and do not block the async runtime; ~570 the counter is always 0 — fix or remove.
  - done: `select_leaf` (transcript.rs:507-622), streaming read + 5 MiB precompact skip (transcript.rs:350-407), `session_messages_async` via `spawn_blocking` (transcript.rs:633-641); temp-dir counter is a static AtomicUsize.
- [x] PC-2.3 Replay must publish the Task* plan: rebuild `task_state` from replayed TaskCreate/TaskUpdate/TaskList (reuse hooks.rs helpers) and publish the `plan` update as Node does, so a status-only TaskUpdate after load is not dropped.
  - done: `replay_with_task_state` (transcript.rs:837-929) rebuilds the state and emits `plan` (AIR dedupe); `Session::seed_task_state` (session.rs:773-792) seeds the live actor from agent.rs:869/1011.
- [x] PC-2.4 Node-captured corpus for each of the above: seeded transcripts under `porting/corpus/seeds/` (meta records with/without exception origins, a completed local command, a branch where the leaf differs from the latest record, a Task* sequence); keep ordering deterministic (`sleep_ms` pacing of the fake's initialize reply).
  - done: `session-load-replay-{meta,completed-command,branch,task-plan}` (Node fixtures captured); differential `pc2_4_session_load_replay_scenarios`.
- [x] PC-2.5 Re-check replay for session/resume vs session/load and an already-live session; no regression of existing `session-load-replay*` scripts.
  - done: `session-load-replay`, `session-load-replay-live`, `session-resume` and the full corpus stay green (differential 59 passed, 1 ignored).
- Suggested ownership: subagent = `transcript.rs` + tests (PC-2.1/2.2); after merge a subagent adds seeds/corpus (PC-2.4); the lead does PC-2.3 + wiring.

## PC-3 — EXITPLANMODE tool_call rendering + parked corpus (files: `tools.rs` / ExitPlanMode rendering, corpus)

- [x] PC-3.1 The parked scripts expose a real divergence: Rust renders the ExitPlanMode `tool_call` with title "Ready to code?" and the plan text as content; Node's fixture says title "Approve Plan" (see `exit-plan-*.frames.jsonl` for the exact frames). Root-cause in `dist/tool-calls/*` and port exactly.
  - done: `tools.rs` `ExitPlanMode` shape is "Approve Plan"/`switch_mode` with the plan as the display copy (`reporters/interaction.ts:37-48`); `build_tool_call`/streamed `tool_call` share it. Result: title "Exited Plan Mode", one-fence strip (`exit-plan.ts:163-169`), non-AIR error content, AIR rawOutput rule (`map.rs` ExitPlanMode arm). The auto selection also ports `applyPermissionFallback` (auto→acceptEdits + advisory) and Node's always-on option sort, both exposed by the corpus.
- [x] PC-3.2 Restore the four corpus files from `parked-exit-plan/` into `porting/corpus/` (+ regenerate fixtures with capture.sh), add the differential tests, verify both selections (default / auto) end to end incl. `setMode`, `interrupt` for "keep planning".
  - done: `exit-plan-default`/`exit-plan-auto` corpus + Node-captured fixtures; `inv_24_exit_plan_default`/`inv_24_exit_plan_auto` (`2 passed`). The auto transcript's fake `control_response` expectation is `setMode acceptEdits`: Node's `applyPermissionFallback` rewrites Auto to Accept edits before the CLI sees it (a `mode auto` expectation only exercised the out-of-scope process-death path). Keep-planning `interrupt` is covered by `permission::tests::inv_60_exit_plan_control_response`/`inv_60_exit_plan_mode_selection_effects`.

## PC-4 — DOCS + FINAL VERIFICATION of parity Phase 3 (lead only; shared docs)

- [ ] PC-4.1 PARITY.md rows P2/P7/P8/P9/P10 honest ("ported" / "ported with gaps" + `pending` rows with consequences: promptSuggestions publishing, MCP/refusal-fallback elicitation, ExitPlanMode details, AIR replay extras, `options.permissionMode` ignored, shell path-shortening labels, hooks gaps); INVARIANTS rows; plan `rust-parity-fixes` checklist ticks; STATUS.md.
- [ ] PC-4.2 Clean-cache reproduction into a SEPARATE cache dir (never delete `rust/target/upstream/0.94.0`): `rm -rf rust/target/parity/final; CAPTURE_OUT_DIR="$PWD/rust/target/parity/final" bash porting/capture.sh >/dev/null 2>&1; diff -r porting/fixtures rust/target/parity/final && echo STABLE`.
- [ ] PC-4.3 Full gate plain + polluted green; differential 3x; JsonPath count 0; G7 clean.

## Commit boundaries

- One commit each: PC-1, PC-2, PC-3, PC-4 (`chore(sdlc): parity-closeout PC-n <what>`), each after the full gate; the orchestrator re-runs the gate before accepting.
- CHECKPOINT `OPUS-DIFF:PC-1` (hooks/session wiring) and `OPUS-DIFF:PC-2` (replay) before the next sub-package.
