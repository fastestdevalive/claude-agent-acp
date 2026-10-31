---
title: "rust-air-gaps: close the advertised-but-inert AIR / extension capabilities of the Rust port (mirror 0.94.0)"
feature: rust-air-gaps
worktree: /Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1
base: eaf2446 (branch feat/rust-port)
research: .vibekit/feature-plans/pending/rust-upstream-sync/research-upstream-0.70-to-0.94.md
parent: .vibekit/feature-plans/pending/rust-upstream-sync/plan-rust-upstream-sync.md
---

<!--
RULES — read before writing or implementing:
1. FORMAT: Bullets, tables, code, diagrams ONLY — no prose paragraphs
2. REQUIREMENTS: One crisp line each — no verbose descriptions
3. CHECKLIST: Mark items [x] as you complete them — this is your persistent todo list
4. READING TIME: Optimize for fast human scanning — if it's hard to skim, rewrite it
-->

# Plan: rust-air-gaps

> Port 11 of the 12 `pending` PARITY (C) behaviours; native subagent sessions and the named sub-parts are split out with consequences.

## Goal

- The 12 listed gaps do what Node 0.94.0 does on the wire; capabilities still advertised but inert afterwards (`auth.logout`/`providers` if unported, `diffPatch`, `planFile`, `recommendedValue`, `nativeSubagentSessions`/`subagents`, AIR delete, fork-at-message) each get a PARITY `pending` row with its consequence
- Each port has a Node-captured corpus fixture (`bash porting/capture.sh <name>`) where fake-claude can show it, else a named unit/actor test
- Differential stays green with NO new `JsonPath` ignore and NO weakened comparison; `porting/PARITY.md` (C) rows flip `pending` -> `ported`
- Done = 3 phases verified (`bash rust/scripts/rust-gate.sh` ends `==> gate OK`, differential green 3x at each phase boundary)

## Hard constraints (every phase)

- G7: only `rust/`, `porting/`, `.vibekit/`, root `.gitignore` may change — NEVER `src/`, `dist/`, `package.json`, `scripts/`, `.github/`, `docs/`, root `*.md`
- G9: `cfg(unix|windows|target_*)` only in `rust/claude-agent-acp-rs/src/process.rs`; G10: `agent-client-protocol` stays 2.1.0 (new methods use `UntypedMessage`, like `_session/steering` at `agent.rs:779`)
- Never print, log or write env VALUES; `porting/record-real.sh` is human-only — never run it; never run `npm` in the repo root
- Do NOT commit, push, tag, `git stash`; cargo always `--manifest-path rust/Cargo.toml`; filter `grep -E "test result|FAILED"`; success = `test result: ok` / `==> gate OK`
- `rust/clippy.toml` bans `std::sync::Mutex|RwLock`, `tokio::sync::Mutex|RwLock|broadcast` — state lives in actors / sequential handlers (G5)
- Escape valve (parent plan Decision 18): a divergence visible in a regenerated fixture MUST be ported; if it cannot be, STOP and report — a `pending` row never turns a red differential green
- New tests: `inv_NN_<slug>` (reserved: `inv_39` systemPrompt, `inv_48` model rows; free from `inv_49`); differential tests `inv_24_<script>`; every test runs under the gate's 180 s timeout (G3)
- Weak-model rule: each phase = infra/pure modules FIRST (unit-tested), then wiring, then corpus script + `capture.sh` + differential test TOGETHER; if a script's Node capture does not show the expected frame, fix the transcript, never the expectation

## Requirements — the 12 gaps

| # | Gap | Verdict | Size | Phase |
|---|-----|---------|------|-------|
| G1 | `_session/async_task/stop` returns -32601 | PORT | S-M | 1 |
| G2 | `authStatus` advertised, `_auth/status_update` never pushed | PORT | M | 2 |
| G3 | `systemPrompt` session option ignored | PORT | S | 1 |
| G4 | AIR `agentFileChangeReport` request gets no reply | PORT (every non-cancelled settle) | M-L | 3 |
| G5 | AIR `customInstructions` dropped | PORT (rides on G3 plumbing) | S | 1 |
| G6 | `sessionCapabilities` close/delete/fork/list/resume -> -32601 | PORT close, resume, list, delete (non-AIR), fork (latest); SPLIT AIR-delete + AIR fork-point | M-L | 2 |
| G7 | `nativeSubagentSessions` gets no child sessions | SPLIT -> `pending` (see Decision 18) | XL | - |
| G8 | `_session/goal` returns -32601 | PORT | M | 3 |
| G9 | AIR `sessionFailure` shape (Node: `end_turn` + failure meta; Rust: -32603) | PORT for every turn-scoped kind; `auth_required` keeps JSON-RPC but gets Node's -32000; session-wide pushes `pending` | M-L | 3 (after G10) |
| G10 | per-model quota rows (`model_usage` always `[]`) | PORT (first item of Phase 2; G9 depends on it) | M | 2 |
| G11 | AIR tool `_meta` (`commandTitle`, `subagent`, `skill`, `mcp`) not emitted | PORT with `is_mcp_tool_call` and the AIR `ChangedMetaFilter` | M | 1 |
| G12 | `/mcp` forwarded to the CLI as plain text | PORT (status + reconnect/enable/disable) with `local_command_output` rendering | L | 3 |

## Change Map

```
porting/
  corpus/                      + 18 scripts (.acp.json + .transcript.jsonl), 2 seed dirs
  fixtures/                    + frames/argv per script, + <name>.init.json for 5 scripts
  capture.sh                   ~ per-script init fixture when "capture_init": true
  PARITY.md INVARIANTS.md      ~ (C) rows flipped, INV-39/48 + new INVs
rust/acp-recorder/src/
  script.rs recorder.rs        ~ generic request/sleep steps, seed_files, capture_init, placeholders
rust/fake-claude/src/
  lib.rs main.rs               ~ $CWD placeholder, `auth status` mode, env allowlist
rust/claude-agent-acp-rs/
  src/lib.rs                   ~ +mods auth_status, session_ext, system_prompt, session_store, meta_filter, goal, file_report, session_failure, quota, mcp_command
  src/agent.rs                 ~ new request arms, system prompt validation, auth probe wiring, caps
  src/control.rs               ~ InitializeOptions.append_system_prompt
  src/session.rs               ~ Commands (stop/close/goal hooks), SessionExt plumbing, settle decorations
  src/turn.rs                  ~ ONLY model-rows side-fields (Phase 2) and Failed.usage (Phase 3), HIGH RISK
  src/map.rs src/tools.rs      ~ AIR tool _meta, local_command_output
  src/permission.rs            ~ AIR tool _meta on the permission-path tool_call
  src/async_tasks.rs           ~ claim_stop / release_stop / task_stopped
  src/unfinished.rs            ~ gate_events returns the settled usage with the Failed event
  tests/common/mod.rs          ~ hermetic HOME, init capture, uuid-substring normalizer
  tests/differential.rs        ~ +18 inv_24_* tests
  tests/*.rs                   ~ wire tests for new methods
```

| Today | After this plan |
|-------|-----------------|
| `_session/async_task/stop`, `_session/goal`, `session/close\|delete\|fork\|list\|resume` -> -32601 (`agent.rs:834`) | handled with Node-identical results and errors |
| `InitializeOptions::default()` at `agent.rs:625,668` — `systemPrompt`, `customInstructions` dropped | validated (-32602) and forwarded as `systemPrompt:[text]` / `appendSystemPrompt` |
| `_auth/status_update` never sent | pushed after `initialize` from `claude auth status --json` and from the session account |
| `quota.model_usage` always `[]` (`session.rs:1225`) | per-model increment rows (Node `modelUsageIncrement`) |
| AIR `end_turn` failure = JSON-RPC -32603 | AIR `sessionFailure` clients get `end_turn` + `_meta.jetbrains.air.sessionFailure` |
| `/mcp` text comes from the CLI | `mcp_status` rendered as Node markdown; reconnect/enable/disable via control requests |
| AIR file-change report request ignored | `session_info_update` carrying `agentFileChangeReport` before the prompt response |
| AIR tool `_meta` = `{claudeCode:{toolName}}` only | + `jetbrains.air.{commandTitle,subagent,skill,mcp}` |

## Research digest (Node 0.94.0 oracle, probed in this planning pass)

- R1 Stop (probe): `_session/async_task/stop` on an announced task emits, in order, `async_task_state_update{state:"stopped"}` (no summary), `agent_message_chunk "**Task stopped by user:** <name>."`, then the response `{"stopped":true}`; unknown task / session -> `{"stopped":false}` (`dist/acp-agent.js:2668-2682`, `dist/async-tasks.js:266-304`)
- R2 Stop wire: the CLI request is `{subtype:"stop_task",task_id}`; invalid params are `-32602` with message `Invalid params: async task stop params require a non-empty sessionId|asyncTaskId` and no `data` (`acp-agent.js:241-255`)
- R3 System prompt (probe, `dist/client-system-prompt.js`): `append` -> initialize `appendSystemPrompt:"<text>"`; `override` -> initialize `systemPrompt:["<text>"]`; `customInstructions` (`_meta.jetbrains.air.customInstructions`, ungated) joins as `\n\n` after the client text and alone becomes `appendSystemPrompt`; argv is unchanged
- R4 System prompt errors (probe): `-32602` `Invalid params: Unsupported systemPrompt mode: weird` data `{systemPrompt:{mode}}`; `Invalid params: systemPrompt content must be text, but received a image block` data `{systemPrompt:{content:{type}}}`; both before any spawn
- R5 Auth (probe): after the `initialize` response Node pushes ext notification `_auth/status_update {authStatus:{kind:"account",label:"Claude Pro",account:{plan,email,organization}}}` once `claude auth status --json` answers (`acp-agent.js:1303,1777-1862`, `dist/auth-status.js`)
- R6 File report (probe): AIR client declaring `agentFileChangeReport` + prompt `_meta.jetbrains.air.agentFileChangeReportRequest{version:1,requestId}` -> at end_turn Node sends `rewind_files{user_message_id:<promptUuid>,dry_run:true}`, then `session_info_update _meta.jetbrains.air.agentFileChangeReport{version,requestId,status:"reported",paths,declaredComplete:false,truncated}` BEFORE the prompt response; paths are canonicalised, workspace-bound, deduped (`dist/file-change-audit.js`)
- R7 Goal (probe): `_session/goal set|clear` = steer (`promptRequired`) else `prompt` of `/goal <objective>`; AIR gets `session_info_update _meta.jetbrains.air.goal` optimistically, then again from the CLI `active_goal` frame; response `{}` (`acp-agent.js:2470-2563,4033-4040`)
- R8 Session methods (probe): `session/close` idempotent `{}`; `session/prompt` on a closed/unknown session = `-32603` data `{details:"Session not found"}`; `session/resume` = `{sessionId,modes,configOptions}`; `session/delete` of an unknown id = `{}`; `session/list` = `{sessions:[{sessionId,cwd,title,updatedAt}]}` (+ `nextCursor:"offset:N"`) read from `<CLAUDE_CONFIG_DIR>/projects/<slug>/*.jsonl`; `cwd` filter matches the slug of the REAL path
- R9 Session failure (probe): AIR `sessionFailure` + unfinished tool -> `tool_call_update failed`, then response `{stopReason:"end_turn",usage,_meta:{quota,jetbrains:{air:{version:1,sessionFailure:{id:"<promptUuid>:error",revision:1,category:"service",severity:"error",title,actions:["retry","new_session"]}}}}}` (`dist/session-failure-extension.js`, `acp-agent.js:3683-3716`)
- R10 Fork: Node copies and rewrites the transcript through the SDK (`fork-session.js:111`; new uuids, `forkedFrom`, title `<t> (fork)`) then resumes the new id; Rust mirrors the latest-fork case in Decision 15 (the fork-at-message and compaction-aware chain rewrite stay `pending`)
- R11 Rust has NO local-command machinery: `local_command_output` frames are not mapped and `system` subtypes outside task_*/commands_changed are dropped (`session.rs:912-990`)
- R12 Corpus harness facts: frames of both adapters are compared positionally (`tests/differential.rs:329-364`); only `usage_update` and four `result.*` paths are ignored (`:253-259`); Rust `normalize` replaces WHOLE-string uuids only while `capture.sh` also replaces embedded ones (`tests/common/mod.rs:500`, `capture.sh:195-199`)

## Invariants to preserve (explicit)

- INV-30 orphan credit consumed BEFORE queue-head promotion (`turn.rs:1052` `ensure_active_turn` early return untouched)
- INV-31 / #825 idle without result fails only an UN-steered active turn (`turn.rs:911` `on_idle`)
- INV-23 `reconcile_orphan_receipt` touches only `orphaned_uuids`; steered uuids never join it
- `owed_trailing_idles` debt accounting and the `had_usage` origin flag (`TurnEvent::Settled`, `turn.rs:186-210`): only the cancel queue sweep is `had_usage:false`
- Steering lanes (`mark_steered`, `steered_echoes/steered_settle`) behave byte-for-byte as today
- Held (subagent-drain) turns: `is_held_turn` keeps meaning "deferred_settle set"
- INV-27 no actor awaits a client round-trip inline: every new CLI / client call is spawned or detached and re-enters via a `Command`
- INV-20 tool_call-before-permission and the `SessionOutbound::Flush` ordering (`session.rs:98`): every update must be on the wire before the response that follows it
- Un-gated, non-AIR, non-new-method paths change only where an item names the divergence

## Key Decisions

1. One generic recorder step replaces N bespoke ones: `Step::Request{method,params,after_recv_contains?,capture_session_id?,expect_error?}` plus `Step::Sleep{ms}`; string values in `params` support `$SESSION`, `$CWD`, `$CWD_SLUG`. — Where: `rust/acp-recorder/src/script.rs:22-72`, `recorder.rs:~190-330`
2. The recorder waits for a wire condition lock-free: the debug tap (`recorder.rs:~60`) also forwards every `Recv` raw line over a second unbounded channel; `run_script` drains it into a local `Vec<String>` and `after_recv_contains` polls that vec (no `Mutex`, clippy-clean). — Where: `recorder.rs:record()`
3. Hermetic HOME: `run_recorder` (tests) and `capture.sh` both give the agent a fresh `HOME`/`CLAUDE_CONFIG_DIR`; `Script.seed_files:[{path,from}]` (path under `CLAUDE_CONFIG_DIR`, `from` under `porting/corpus/`, `$CWD`/`$CWD_SLUG`/`$SESSION` expanded) are written by the recorder before it spawns the agent, so Node and Rust see identical transcripts. — Where: `tests/common/mod.rs:709`, `capture.sh:168-170,310-320`, `script.rs`
4. Per-script initialize fixture: a script with `"capture_init": true` writes `porting/fixtures/<name>.init.json` (and never `initialize.json`); `tests/common` gets `run_recorder_with_init()` returning the normalised first `initialize` control_request; a test compares Rust's to the fixture exactly. — Where: `capture.sh:301-322`, `tests/common/mod.rs`, new `tests/system_prompt.rs`
5. Rust `normalize()` also replaces uuid SUBSTRINGS with first-appearance placeholders, mirroring `capture.sh replaceUuids`; required for `"<promptUuid>:error"` ids. — Where: `tests/common/mod.rs:500-520`
6. fake-claude gains exactly: `$CWD` in `emit` strings (= canonical `current_dir()`), an `auth` argv mode (`argv[0]=="auth"` prints the transcript's special line `{"auth_status_stdout":{…}}` and exits 0, else exits 1 with no output, never touching the argv record), and `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` in `ARGV_ENV_ALLOWLIST`. — Where: `rust/fake-claude/src/lib.rs:141,162-175,312-330`, `main.rs`
7. One new actor-owned struct `SessionExt` (`src/session_ext.rs`) carries `control`, `air`, `cwd`, and (later) goal / report / failure / model-ledger state; it is threaded as ONE extra `&mut SessionExt` parameter through `handle_session_frame` (`session.rs:824`), `settle_events` (`:1345`) and `handle_events` (`:~1146`) — added in Phase 1 so later phases add fields, not parameters. — Where: `src/session_ext.rs` (new), `session.rs`
8. Every new request that waits on the CLI (stop_task, rewind_files, mcp_*) is spawned; the spawned task re-enters the actor with a `Command` (`TaskStopped`, `ReportReady`, …) so the actor emits updates in order, then replies through a `Flush` barrier so the response never overtakes its updates. — Where: `session.rs:154` Command enum, `SessionOutbound::Flush` (`:98`)
9. Error wire shape for new Node `RequestError.invalidParams(data,msg)`: `Error::new(-32602, "Invalid params: <msg>")` plus optional `.data(..)` (same builder as `validate_cwd`, `agent.rs:~354`), NOT `Error::invalid_params().data(str)`. — Where: `agent.rs`
10. G3/G5 system prompt: pure module `system_prompt.rs` (`client_system_prompt(&Value)->Result<Option<ClientPrompt>,Error>`, `sdk_initialize_fields(prompt, custom)->(Option<Value>,Option<String>)`); `InitializeOptions` gains `append_system_prompt: Option<String>` and `build_request()` (`control.rs:137`) emits `appendSystemPrompt`; override sets `system_prompt = Some(json!([text]))`. `_meta.systemPrompt` (string / preset object) stays `pending` (not in the 12). — Where: `src/system_prompt.rs`, `control.rs:116-160`, `agent.rs:615-690`
11. G2 auth: pure `auth_status.rs` (port of `dist/auth-status.js`: `fromCliStatus`, `fromAccountInfo`, `mergeAuthStatus`, `sameAuthStatus`, planLabel, provider labels) plus ONE owning task `AuthActor` (mpsc `AuthCmd::{Probe{cwd,fresh},Account(Value)}`) holding `current` status — no Mutex; the probe runs `<resolved claude> auth status --json` (`process::resolve_binary`, `process.rs:77`) with stdin null, 5 s timeout, stdout parsed even on non-zero exit, and pushes `UntypedMessage::new("_auth/status_update", {"authStatus":..})` only when `!same_auth_status`. `apiKeyHelper` is read from `settings.json` (user, project, local; last wins). Gateway / provider-override / epoch / `--hide-claude-auth` parts have no Rust counterpart and stay `skipped-deliberate`. — Where: `src/auth_status.rs`, `agent.rs:602` (spawn after `responder.respond`), `agent.rs:615,651` (account push + fresh probe when cwd differs from the last probe cwd)
12. G1 stop: `Command::StopAsyncTask{task_id,reply}` -> actor `tasks.claim_stop()` synchronously; true -> spawn `control.send_request({"subtype":"stop_task","task_id":id})`; Ok -> `Command::TaskStopped{task_id,reply}` -> `tasks.task_stopped()` emits `async_task_state_update stopped` then the `**Task stopped by user:** <name>.` chunk (`agent_message_chunk`, Rust has no `notices`), enqueue Flush, reply `{stopped:true}`; Err -> `release_stop`, JSON-RPC -32603. — Where: `async_tasks.rs:35,279` (`stopping`, `stop_announced` fields + 3 methods), `session.rs:154,671`, `agent.rs:834`
13. G11 AIR tool meta: `MapState` gains `air: bool` + `cwd: Option<PathBuf>` (set once in `run_loop`, `session.rs:~497`; `MapState::default()` in tests stays non-AIR); `tools::tool_meta(name,input,air_cwd:Option<&Path>)` adds `jetbrains.air` keys exactly as `AcpToolCallRenderer.toolUseMeta` (`dist/tool-calls/renderer.js:566-590`): `commandTitle` from `shellDescription` (Bash/PowerShell, `reporters/bash.js:8`), `subagent:true` for `Agent`/`Task`, `skill:{name,path?}` via a port of `resolveSkillPath` (`reporters/interaction.js:358`), `mcp:{server,tool}` via `mcpToolName`. Every caller passes it: `map.rs:425,469,497,864`, `permission.rs:369`. See Decision 25 for the AIR-only filter and `is_mcp_tool_call`. — Where: `tools.rs:84-95`, `map.rs:64`
14. G6 session methods: `session_store.rs` (pure fs, `CLAUDE_CONFIG_DIR` else `$HOME/.claude`): project-dir slug (non-alphanumeric -> `-` of the canonical path, long paths hashed exactly as the SDK — read `sdk.mjs` `listSessions`/`deleteSession`), `find_transcripts(id)`, `list(dir?, offset, page=SESSION_LIST_PAGE_SIZE)` (newest mtime first, `title` = customTitle > aiTitle > last prompt > first prompt, sanitized like `sanitizeTitle`), `delete(id)`. `session/close` = `Command::Close` (cancel + interrupt like `Command::Cancel`, wait for the settles through Flush, then break `run_loop`, drop child), idempotent; `session/resume` = the `session/load` arm refactored into a shared fn; unknown-session `session/prompt` aligned to Node `-32603 data{details:"Session not found"}` (`agent.rs:693-702`). — Where: `src/session_store.rs` (new), `agent.rs:591-834`, `session.rs:154,565`
15. G6 fork: Rust copies the latest transcript itself (Node `fork-session.js:111` + SDK `forkSession`): fresh uuid per entry, `parentUuid`/`logicalParentUuid` remapped, `sessionId` rewritten, `forkedFrom:{sessionId,messageUuid}`, closing `custom-title "<title> (fork)"`, `progress` entries dropped; then the normal `--resume=<new>` start; verified by a `session/list` after the fork. AIR `_meta.jetbrains.air.fork` (fork at a message) returns `-32602 "Fork point message <id> was not found in session <sid>"` and is a `pending` row. No CLI `--fork-session`. — Where: `session_store.rs`, `agent.rs`
16. G8 goal: `GoalState{last_published, pending{command_uuid,expected,previous,started}}` in `SessionExt`; hooks live in the actor (not the machine): Prompt/Steer arms (`session.rs:559,671`) detect `^/goal(\s+…)?$` and publish; the `active_goal` frame arm in `handle_session_frame` (before the turn machine, `session.rs:841` match) runs `publishRuntimeGoal`; the user-echo arm marks `started`; the `result` arm rolls back on `is_error || stop_reason=="refusal" || "Please run /login"` when `started`. Publishing = `RawUpdate session_info_update` for AIR clients only. `_session/goal` itself = steer(`promptRequired`) then prompt, responding `{}` from a spawned task. — Where: `src/goal.rs`, `session.rs`, `agent.rs:834`
17. G9 failure: `session_failure.rs` ports `AIR_FAILURE_POLICY`, `sessionFailureMeta`, `providerFailureCategory`; `TurnEvent::Failed` gains `usage: Usage` (the machine's `accumulated_usage` at the failing instant — 4 production sites `turn.rs:1001,1028,1038,1196`, tests updated mechanically) so `handle_events` can answer `end_turn` + `_meta.quota` + `_meta.jetbrains.air.sessionFailure{id:"<prompt_uuid>:error",revision:1,..}`. Kind / title come from `AssistantFailureCtx` (Decision 26). Gate: `air.supports("sessionFailure")` and kind != `AuthRequired`/`SessionEnded`; otherwise unchanged -32603; `AuthRequired` becomes code -32000. The `unfinished.rs` conversion forwards the settled usage into the Failed event. — Where: `session_failure.rs`, `turn.rs`, `unfinished.rs:116-170`, `session.rs:1247`
18. G7 split-out: native subagent sessions stay `pending` — missing piece = (a) a port of `dist/native-subagents.js` (1284 lines: child session registry, `routeOfToolCall`, buffered pending parents, child cancel via `stop_task`, history restore) that sits in front of EVERY `sendUpdate`, and (b) a REAL-CLI recorded subagent stream (`parent_tool_use_id`, agent ids) because the corpus subagent transcripts are synthetic (parent plan OQ-15 / E-19) so no honest Node oracle exists from fake-claude; the `nativeSubagentSessions` capability and `sessionCapabilities.subagents` stay advertised for fixture parity (OQ-1). — Where: PARITY (C) row only
19. G10 model rows: `quota.rs` ports `normalizeModelUsage`, `modelUsageIncrement`, `addModelUsage`, `resumedFirstResultModelUsage`; `TurnMachine` gets `accumulated_models: ModelRows` and `last_model_reading: Option<ModelRows>` beside `accumulated_usage` (`turn.rs:330`); `accumulated_models` resets/carries at the same statements as `accumulated_usage` (`turn.rs:1209`, carried model usage applied at activation) but `last_model_reading` is a SESSION-long reading and is NEVER reset per turn (`Some(empty)` for a new session, `None` for a resumed one), advanced in `on_result` BEFORE the autonomous early return without touching `owed_trailing_idles`; `DeferredSettle` (`turn.rs:183`, Copy -> Clone) and `Settled` carry `Arc<ModelRows>`; `handle_events` renders `model_usage:[{model,token_count}]`. STOP rule: if any pre-existing `steer_*`/`inv_16|23|29|30|31` test needs a non-mechanical edit, revert the item and record `pending`. — Where: `turn.rs`, `session.rs:1195-1232`, `quota.rs`
20. G4 report: `file_report.rs` (pure: request-id parse, path normalisation incl. 1024 paths / 4096 chars / 256 KiB caps, workspace roots, canonical-from-existing-ancestor, `fit`) + actor state `reports: HashMap<prompt_uuid, ReportState{request_id, edited_paths}>` in `SessionExt`; env `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=true` added to `SpawnOptions.extra_env` when `air.supports("agentFileChangeReport")` (`agent.rs:293`); on EVERY non-cancelled `Settled` (EndTurn, max_tokens, refusal, max_turn_requests) of a turn with a request the `Settled` arm hands `(PromptReply, report)` to a spawned task that sends `rewind_files` (2 s timeout), enqueues the `RawUpdate`, THEN a Flush, THEN resolves the reply; a `Command::Cancel` arriving while a preview is in flight signals the task through a per-prompt abort oneshot kept in `SessionExt.reports`, the task publishes `unavailable{cancelled}` and resolves the reply `stopReason:"cancelled"` (Node cancel-wins, `acp-agent.js:3619-3622`); other terminal paths publish `status:"unavailable"` with reason `cancelled|providerError|timeout|invalidOutput`; subagent `Edit|Write|MultiEdit|NotebookEdit` tool_results feed `edited_paths`. — Where: `file_report.rs`, `session.rs:1146-1240,841-880`, `agent.rs:293,693`
21. G12 `/mcp`: new `mcp_command.rs` (port of `parseMcpCommand`, `formatMcpStatus`, `cleanMcpError`, `runMcpCommand` against a `McpQuery` trait over `Control`: `mcp_status`, `mcp_reconnect{serverName}`, `mcp_toggle{serverName,enabled}`) plus a minimal local-command layer in `session.rs`: `system/local_command_output` frames become `agent_message_chunk` (R11), and for a turn whose prompt parses as `/mcp` the first output is replaced by the produced markdown (exact-mirror suppression as `takeLocalCommandMarkdown`, `acp-agent.js:3243-3260`; cancelled turn drops it). STOP rule: ship `status` first; actions only if the status script is green. — Where: `src/mcp_command.rs`, `session.rs:912-990`, `map.rs`
25. AIR meta filter: `meta_filter.rs` ports `dist/tool-calls/changed-meta-filter.js` (applied for AIR sessions in `emit_updates`, `session.rs:1389`, after all other mapping) and the AIR-only top-level `is_mcp_tool_call:true` of `renderer.js:109-111`; non-AIR output is unchanged. — Where: `meta_filter.rs`, `map.rs:425`, `session.rs:1389`
26. Ordering: G10 (model rows) lands before G9 because the failure `end_turn` response carries `_meta.quota` with the rows (`turnOutcome`, `acp-agent.js:9032`); G9 needs `AssistantFailureCtx` (kind from `lastAssistantError`, usage-limit flag, title `lastAssistantFailureTitle ?? result`, `acp-agent.js:5267,5736-5744`, `session-failure-extension.js:329`) instead of the `assistant_had_error` bool. — Where: `session_ext.rs`, `session.rs:1146`
27. `logout` / `providers`: port only the bounded wire-visible parts (Phase 2.4); the rest is a `pending` row naming the consequence. — Where: `agent.rs`
22. AIR delete: `session/delete` of an AIR client archives in Node (`session-index/service.js:305` `archiveInsteadOfDelete`); hard-deleting would destroy data Node keeps, so for AIR clients Rust keeps -32601 and the PARITY row says why; non-AIR clients get the SDK delete. — Where: `agent.rs` delete arm
23. (Deviation notes from implementer turns go here, one bullet per deviation, citing the item number)
    - 1.0/1.1 Decision 24 is referenced by the Phase 1 items but does not exist in this plan (the numbering jumps 23 -> 25); the infra followed Decisions 1-6 only. The recorder placeholder `$CWD` is the canonical cwd; `seed_files` content (not only the path) also expands `$CWD`/`$CWD_SLUG`/`$SESSION`; `Step::Request` has no `no_wait` (Phase 3.3 needs one for the file-report scripts).
    - 1.1 `capture.sh`: a script whose request fails before any CLI spawn (`system-prompt-bad-mode|bad-block`) has no argv / initialize record, so `capture.sh` skips their normalisation instead of failing (no argv fixture for those two). `capture_init` is read with `node -p` from the script JSON. All 26 pre-existing fixtures stayed byte-identical under the per-script HOME (1.T5).
    - 1.1 `tests/common`: `run_recorder` now runs under a hermetic `HOME`/`CLAUDE_CONFIG_DIR` per call; `normalize_init` mirrors `capture.sh normalize_object` (including its `^[a-z0-9]{8,20}$` -> placeholder rule, which also turns `subtype:"initialize"` into `$1`).
    - 1.3 Extra script `system-prompt-combined` (append + `customInstructions`, `capture_init`) so the Change Map's 5 `<name>.init.json` files exist (append, override, load, custom-instructions, combined). `inv_39_initialize_matches_node` compares the Rust `initialize` request with the fixture after dropping `hooks` and `promptSuggestions` on BOTH sides: the live agent never sends them (PARITY: hooks skipped-deliberate; `prompt_suggestions` stays `None`), and every other key is compared exactly. A non-array `systemPrompt.content` is treated as empty (Node would throw a TypeError).
    - 1.4 The `air-tool-meta` Node capture showed AIR divergences beyond `_meta` that the plan did not list: no `rawInput` on a streaming `tool_call` / partial refinement, and the AIR `rawOutput` rule (+ viewed-files for read/search). They are fixture-visible, so they were ported (AIR-only, gated by `MapState.air`; non-AIR output is unchanged) rather than hidden by reshaping the transcript; `MapState` gained `air`, `cwd`, `input_incomplete`, `meta_filter`. The partial refinement `_meta` now reads the recovered partial input for AIR (Node `partialRefinement`). The remaining AIR renderer branches stay `pending` (PARITY row). `tools::tool_meta` kept its 2-arg signature (existing `inv_46`); the AIR variant is `tools::air_tool_meta`.
    - 1.4 `tests/common` `normalize_tool_call_defaults` also materialises an omitted `kind` as `"other"` on `tool_call` frames (both sides): the pinned crate omits the default `ToolKind::Other`, the Node always sends it (Skill / MCP tool calls). Same mechanism and scope as the existing `status`/`content` defaults; no ignore path.
    - 1.4 The filter runs in `MapState::filter_updates` (not inside `emit_updates`), because `MapState` owns the per-session AIR flag; it covers the stream / consolidated, permission-path and unfinished-tool emissions.
    - 2.1 `TurnEvent::Settled` gained `model_rows: Arc<ModelRows>` and `DeferredSettle` lost `Copy` (carries the same `Arc`, so a held / steered turn keeps the rows its own result recorded, like its usage). `turn_tests.rs` is unmodified (209+ added lines only; every existing pattern already used `..`). The resumed flag reaches the machine as a new `resumed: bool` parameter of `run` / `run_loop` (`spawn.resume.is_some()`; four in-file test call sites pass `false`); `last_assistant_model` is fed by `note_assistant_model` from top-level `message_start` / `assistant` frames and cleared at activation (`resetTurnScratch`). Node's `carriedModelUsage` is never set in 0.94, so activation just resets the tally. The corpus script `model-usage` has 3 prompts (increment, dropped zero-total model, rewind) instead of 2, with two models in non-alphabetical order to pin insertion order.
    - 2.2 The actor is spawned lazily by the first `initialize` (it needs the connection) and talks to a forwarder task through `AuthPush::{Status, Barrier}` (the barrier lets a response wait behind its notification, needed by 2.4). The per-prompt re-probe, gateway / provider overrides, the hide-auth guard and the `vendor` bag are not ported (PARITY). `process.rs` gained `resolve_claude` and `run_cli` (one-shot CLI runs; the only place for platform specifics, G9).
    - 2.3 `session/close` waits for its `interrupt` request at most 500 ms (`Command::Close` -> spawned write -> `Command::CloseDone`) so the child is not killed before the frame is written; the Node fixture does not show whether Node sends the interrupt when idle, and the differential is green either way. The session-method arms live in `agent.rs` with the filesystem work in the new `session_methods.rs` / `session_store.rs` (`agent.rs` stays under the 1500-line guardrail). Not ported (PARITY): git-worktree siblings in a `cwd` listing, the sidecar title store, `vi` command-tag prompt filtering, `compactMetadata` / attachment uuid remapping, the 0600 file mode of the fork file (G9). `session/resume` of an already-live session starts a second child (as `session/load` does today).
    - 2.4 `providers/list|set|disable` were NOT ported (env-derived list shape, set / disable recreate live sessions): PARITY `pending` with the consequence. `logout` is ported with the post-logout fresh read; the `notLoggedIn` push is confirmed by the Node capture.
    - 2.6 `handle_events`, `settle_events` and `handle_session_frame` no longer take `&mut SessionExt` (it was only passed along); `SessionExt` keeps `air` + `cwd` (read by `run_loop`) and Phase 3 re-adds fields and the parameter where an item reads them. The `initialize` subtype is asserted on the raw value because the short-id rule is mirrored from `capture.sh` and cannot be tightened without rewriting committed fixtures.
    - 1.5 `SessionError::Start(message)` carries a CLI `stop_task` failure to the JSON-RPC layer (`-32603`); a `control_response` with `subtype:"error"` counts as a failure.

## Gap register (feasibility · acceptance · Rust target · risk)

| Gap | Acceptance (fixture = Node-captured via `bash porting/capture.sh <name>`) | Rust target (file:line today) | Risk |
|-----|------------------------------------------------------------------------------------------------------------|------|------|
| G1 stop | script `async-task-stop` (AIR asyncTasks; prompt `no_wait`; `request` stop `after_recv_contains:"async_task_spawned"`): fixture shows spawned, state stopped, notice chunk, `{stopped:true}`, prompt `end_turn`; unit `inv_49_stop_claim_release`, `inv_49_stop_params_errors` | `agent.rs:834`, `session.rs:154,671`, `async_tasks.rs:35,279` | LOW |
| G2 auth (Phase 2) | script `auth-status` (transcript special line `auth_status_stdout`; steps initialize, sleep 600, session/new): fixture has `_auth/status_update`; units `inv_50_*` table for `fromCliStatus/fromAccountInfo/merge/same/planLabel` (expected values from the Node functions run via `node -e` against the installed dist) | `agent.rs:602,615`, new `auth_status.rs` | MED (timing; probe spawn) |
| G3 systemPrompt | scripts `system-prompt-append`, `system-prompt-override`, `system-prompt-load`, `system-prompt-bad-mode`, `system-prompt-bad-block` (`capture_init`) + `inv_39_system_prompt_validation`, `inv_39_initialize_fields_match_node` | `agent.rs:615-690`, `control.rs:116-160`, new `system_prompt.rs` | LOW |
| G4 report | script `file-change-report` (AIR agentFileChangeReport; transcript step expecting `rewind_files`; paths use `$CWD`) + `file-change-report-cancel-wins` (fake withholds the `rewind_files` answer until `interrupt`); units `inv_55_*` for caps / dedupe / outside-root / control chars / ids | `session.rs:1146-1240,841-880`, `agent.rs:293`, new `file_report.rs` | HIGH (settle-path deferral) |
| G5 customInstructions | script `custom-instructions` (AIR, `_meta…customInstructions`, `capture_init`) -> `appendSystemPrompt` | same as G3 | LOW |
| G6 sessions | scripts `session-close`, `session-resume`, `session-list` (seeded; list, delete, list), `session-fork` (seeded); units `inv_52_*` (slug, titles, paging, delete, close-with-turn-in-flight) | `agent.rs:591-834`, `session.rs:154,565`, new `session_store.rs`| MED (fs semantics) |
| G7 native sub | none — `pending` | PARITY row | - |
| G8 goal | script `goal-set-clear` (AIR; `request` `_session/goal` set, clear; transcript with `active_goal` frames) + units `inv_53_*` (parse errors, optimistic publish, rollback on failed result) | `agent.rs:834`, `session.rs:559,671,841,881`, new `goal.rs` | MED |
| G9 failure | scripts `session-failure-unfinished` (AIR sessionFailure + `unfinished-tool` transcript), `session-failure-provider-error` (+ `error-result` transcript); units `inv_54_*` policy table, auth_required stays -32603, non-AIR unchanged | `turn.rs:1001,1028,1038,1196`, `unfinished.rs:116`, `session.rs:1247` | HIGH (touches `TurnEvent`) |
| G10 model rows | script `model-usage` (2 prompts, `modelUsage` in results incl. a rewind); units `inv_48_*` (increment, rewind, resumed seed, autonomous leaves tally alone, zero-total dropped) | `turn.rs:170,183,330,698,1209`, `session.rs:1195-1232`, new `quota.rs` | HIGH (turn machine) |
| G11 tool meta | script `air-tool-meta` (AIR; Bash with description, Agent, Skill, `mcp__srv__tool`, results returned) ; units `inv_51_*` (skill path resolution on a temp tree) | `tools.rs:84`, `map.rs:64,425,469,497,864`, `permission.rs:369` | LOW-MED |
| G12 /mcp | scripts `mcp-status` then `mcp-reconnect` (transcripts expect `mcp_status` / `mcp_reconnect` control requests; Node fixture decides the CLI frames); units `inv_56_*` (parse, format, cleanError) | `session.rs:912-990`, `map.rs`, new `mcp_command.rs` | HIGH (new local-command path in the settle area) |

## System Boundaries

| Boundary | Contract |
|----------|----------|
| ACP client -> agent | `_session/async_task/stop {sessionId:string,asyncTaskId:string}` -> `{stopped:bool}`; errors -32602 (messages in R2) |
| ACP client -> agent | `_session/goal {sessionId,action:"set"\|"clear",objective?:string}` -> `{}`; errors -32602: `goal params must be an object`, `goal params require a non-empty sessionId`, `goal action must be "set" or "clear"`, `goal action "set" requires a non-empty objective` (each prefixed `Invalid params: `) |
| ACP client -> agent | `session/new\|load\|resume\|fork` optional `systemPrompt {mode:"append"\|"override",content:[{type:"text",text:string}]}` and `_meta.jetbrains.air.customInstructions:string`; errors in R4 |
| ACP client -> agent | `session/close {sessionId}` -> `{}` (idempotent); `session/resume {sessionId,cwd,mcpServers}` -> `{sessionId,modes,configOptions}`; `session/delete {sessionId}` -> `{}`; `session/list {cwd?:string\|null,cursor?:"offset:N"}` -> `{sessions:[{sessionId,cwd,title,updatedAt:iso8601}],nextCursor?}`; `session/fork {sessionId,cwd,mcpServers?}` -> `{sessionId,modes,configOptions}` |
| agent -> ACP client | ext notification `_auth/status_update {authStatus:{kind:"none"\|"gateway"\|"external"\|"api_key"\|"account",label:string,detail?:string,account?:{plan,email?,organization?}}}` |
| agent -> ACP client | `session/update` raw variants: `session_info_update` with `_meta.jetbrains.air.{goal\|agentFileChangeReport}`; `async_task_state_update{state:"stopped"}` |
| agent -> ACP client | prompt response `_meta.quota.model_usage:[{model:string,token_count:{totalTokens,inputTokens,cachedInputTokens,cachedWriteTokens,outputTokens,reasoningOutputTokens}}]` and `_meta.jetbrains.air.sessionFailure{id,revision:int,category,severity,title,actions:string[]}` |
| agent -> `claude` | control_requests `stop_task{task_id}`, `rewind_files{user_message_id,dry_run:true}`, `mcp_status`, `mcp_reconnect{serverName}`, `mcp_toggle{serverName,enabled}`; initialize `systemPrompt:[string]` / `appendSystemPrompt:string`; argv `--resume=<forked id>` (the fork is a Rust-side transcript copy, Decision 15); env `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=true`; subprocess `claude auth status --json` (stdout JSON `{loggedIn:bool,apiProvider?,subscriptionType?,apiKeySource?,email?,orgName?}`) |
| fake-claude | in-order replay; auto-ack set unchanged (`get_context_usage`); `$REQ`, `$MATCH.uuid`, NEW `$CWD`; NEW `auth` argv mode; unexpected frame = exit non-zero |

## Implementation Phase 1 — recorder/fake infra, systemPrompt + customInstructions, AIR tool _meta (+ changed-meta filter), async-task stop

Sized for one turn; ORDER = risk order. If the turn ends early, finish the item in progress (script + capture + differential TOGETHER) and stop; unfinished items stay unchecked.

- [x] 1.0 Read `rust/AGENTS.md`, §Hard constraints and Decisions 1-13, 24-25, `rust/acp-recorder/src/{script,recorder}.rs`, `rust/fake-claude/src/lib.rs:141-340`, `porting/capture.sh:160-330`, `tests/common/mod.rs:500-520,709-760`, `dist/client-system-prompt.js`, `dist/async-tasks.js:225-310`, `dist/tool-calls/changed-meta-filter.js`
- [x] 1.1 Infra (Decisions 1-6, 24): `script.rs` add `Step::Request`, `Step::Sleep`, `Script.seed_files` (with explicit `mtime` seconds), `Script.capture_init`; recorder implements them (scripts needing `systemPrompt` / prompt `_meta` use `Request` for `session/new|load|prompt`, there is no bespoke field); `fake-claude` `$CWD`, `$REQ.1` (request_id of the control_request matched by the PREVIOUS step, for a deferred response), `auth` mode with state (`auth status` prints the transcript line; `auth logout` creates marker `$CLAUDE_CONFIG_DIR/.fake-claude-logged-out`, exit 0; while the marker exists `auth status` prints `{"loggedIn":false}` and exits 1), replay skips the `auth_status_stdout` line, allowlist, each with its own `rust/fake-claude/tests/` test (`auth_logout_flips_status`, `req_prev_placeholder_answers_deferred_request`, `cwd_placeholder_expands`); `capture.sh` per-SCRIPT `FAKE_HOME` (move `mktemp -d` into the loop at `:168-170,305-320`, remove it after each script) + per-script init fixture; `tests/common` hermetic `HOME/CLAUDE_CONFIG_DIR` per `run_recorder`, `run_recorder_with_init`, uuid-substring `normalize` (`inv_49_normalize_embedded_uuid`). Gate: all 26 existing fixtures byte-identical after the per-script HOME change
- [x] 1.2 `SessionExt` (Decision 7): `src/session_ext.rs` with `control`, `air`, `cwd`; thread `&mut SessionExt` through `handle_session_frame`, `settle_events`, `handle_events` and every in-file test caller; no behaviour change
- [x] 1.3 G3+G5 (Decision 10): `src/system_prompt.rs` + `inv_39_system_prompt_validation`, `inv_39_sdk_fields`; wire into `session/new` (`agent.rs:615`) and `session/load` (`:651`) AFTER `validate_cwd` (Node order, `acp-agent.js:1488`); scripts `system-prompt-append|override|load|bad-mode|bad-block`, `custom-instructions` (`capture_init`); `tests/system_prompt.rs::inv_39_initialize_matches_node`; `inv_24_system_prompt`
- [x] 1.4 G11 (Decision 13 + 25): `tools.rs` `air_tool_meta` + `resolve_skill_path` + `inv_51_*`; `MapState.air/cwd`; all 5 `tool_meta` callers; top-level `is_mcp_tool_call:true` on `mcp__*` tool_calls for AIR (`renderer.js:109-111`); new `meta_filter.rs` = port of `ChangedMetaFilter` applied in `emit_updates` (`session.rs:1389`) for AIR sessions only (strips unchanged `claudeCode`/`jetbrains.air` keys and flags `is_mcp_tool_call`/`terminal_info` on `tool_call_update`, never `terminal_output*`/`mcp_output_delta`, DROPS an update left empty; `TOOL_CALL_LIMIT` 2048) with `inv_51_meta_filter_*` units; script `air-tool-meta` (Bash+description, Agent, Skill, `mcp__srv__tool` + a second update of the same call); `inv_24_air_tool_meta`; `inv_46_tool_meta_is_tool_name_only` stays green
- [x] 1.5 G1 (Decision 12): `async_tasks.rs` stop methods + units, `Command::StopAsyncTask/TaskStopped/ReleaseStop`, `agent.rs` arm with Node param validation (`inv_49_stop_params_errors`); script `async-task-stop` (Contract A); `inv_24_async_task_stop`
- [x] 1.6 Docs: PARITY (C) rows G1, G3, G5, G11 -> `ported`; INVARIANTS INV-39, INV-49, INV-51; deviations under Decision 23

### Contract A — `async-task-stop`

- `.acp.json`: `{"permission":"allow","steps":[{"call":"initialize","client_capabilities_meta":{"jetbrains":{"air":{"version":1,"capabilities":["asyncTasks"]}}}},{"call":"session/new"},{"call":"session/prompt","text":"start bg","no_wait":true},{"call":"request","method":"_session/async_task/stop","params":{"sessionId":"$SESSION","asyncTaskId":"bg1"},"after_recv_contains":"async_task_spawned"},{"call":"session/wait"}]}`
- transcript line 1 = `head -1 porting/corpus/text-only.transcript.jsonl`; step 2 = user echo (`$MATCH.uuid`, text "start bg"), assistant "starting", `task_started` (`task_id:"bg1"`, `task_type:"local_workflow"`, `description:"Sleep"`, `tool_use_id:"toolu_BG1"`), no result
- step 3 `expect` `{"type":"control_request","request":{"subtype":"stop_task","task_id":"bg1"}}`, emit ONLY the success `control_response` (`request_id:"$REQ"`), then `{"sleep_ms":400}`, THEN `task_notification` (`status:"stopped"`), assistant "stopped", `result end_turn` — the sleep keeps the notification from beating the stop response (Node's own race, `async-tasks.js:285-290`)
- Node order (verified): spawned -> stop request -> state_update stopped -> chunk "**Task stopped by user:** Sleep." -> `{stopped:true}` -> prompt `end_turn`

### Verify phase 1

- [x] 1.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential 2>&1 | grep -E "test result|FAILED"` -> ok; run 3 times
- [x] 1.T2 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test system_prompt 2>&1 | grep -E "test result|FAILED"` and `--lib 'inv_39|inv_49|inv_51'` -> ok
- [x] 1.T3 `cargo test --manifest-path rust/Cargo.toml -p acp-recorder -p fake-claude 2>&1 | grep -E "test result|FAILED"` -> ok
- [x] 1.T4 `grep -c 'Task stopped by user' porting/fixtures/async-task-stop.frames.jsonl` == 1; `grep -c '"appendSystemPrompt"' porting/fixtures/system-prompt-append.init.json` == 1; `grep -c '"systemPrompt"' porting/fixtures/system-prompt-override.init.json` == 1; `grep -c 'commandTitle' porting/fixtures/air-tool-meta.frames.jsonl` >= 1; `grep -c 'is_mcp_tool_call' porting/fixtures/air-tool-meta.frames.jsonl` >= 1
- [x] 1.T5 `CAPTURE_OUT_DIR=$PWD/rust/target/upstream/p1 bash porting/capture.sh >/dev/null 2>&1; for f in $(git ls-files porting/fixtures); do cmp -s $f rust/target/upstream/p1/$(basename $f) || echo CHANGED $f; done` prints nothing. If an EXISTING fixture changes under the per-script HOME, regenerate it, require the differential green, and record the reason under a Key Decision 23 bullet; an unexplained change still STOPs
- [x] 1.T6 `bash rust/scripts/rust-gate.sh 2>&1 | tail -4` ends `==> gate OK`; `git diff --stat -- . ':!rust' ':!porting' ':!.vibekit' ':!.gitignore'` empty (G7); `git diff -U0 eaf2446 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'` prints `0`

## Implementation Phase 2 — auth status, per-model quota rows, session methods, logout / providers

Sized for one turn. G10 touches `turn.rs` (HIGH) so it goes FIRST while the turn is fresh and because G9 needs its rows (Decision 26); the rest is leaf code.

- [x] 2.0 Read Decisions 11, 14, 15, 19, 22, 27, `dist/auth-status.js`, `acp-agent.js:1900-1910,2025-2075,4935-4975,9195-9260`, `fork-session.js`, SDK `forkSession`/`listSessions` (`sdk.mjs`: grep `forkedFrom`, `listSessions`), review-04/05 turn notes
- [x] 2.1 G10 (Decision 19): `quota.rs` + `inv_48_*` units BEFORE touching the machine (increment, rewind, resumed seed needing `last_assistant_model`, autonomous advances the reading but not the tally, zero-total dropped; the reading lives for the whole SESSION and is never reset per turn; `carried_model_usage` applied at activation, `acp-agent.js:~2968`); machine side-fields; `handle_events` rendering; script `model-usage` (2 prompts, `modelUsage` in results incl. a rewind); `inv_24_model_usage`; run `--lib turn` before and after
- [x] 2.2 G2 (Decision 11): `auth_status.rs` + `inv_50_*` tables, `AuthActor`, probe after the `initialize` response (`agent.rs:602`), account push + fresh probe at `session/new|load`; scripts `auth-status` (Contract B); `inv_24_auth_status`
- [x] 2.3 G6 (Decisions 14, 15, 22): `session_store.rs` + `inv_52_*` units (temp config dir); `Command::Close` (sends `interrupt` even when idle — transcript expects it), `session/close|resume|list|delete|fork` arms, unknown-session prompt error alignment; fork copies the latest transcript in Rust (new uuids for every entry, `parentUuid`/`logicalParentUuid` remapped, `sessionId` rewritten, `forkedFrom:{sessionId,messageUuid}`, final `custom-title` record `"<title> (fork)"`; port the `ed()`-free linear case: drop `progress` entries, keep order) then starts the CLI with `--resume=<new>`; scripts `session-close`, `session-resume`, `session-list` (seeded, explicit mtimes; list, delete, list), `session-fork` (seed, fork, then `session/list` proving the fork row with its title); `inv_24_session_methods`
- [x] 2.4 (logout ported; `providers/*` -> PARITY `pending`, not bounded: env-derived list, set/disable recreate sessions) `auth.logout` and `providers` (advertised at `agent.rs:~127`): port `logout` (`acp-agent.js:2025-2075`: `claude auth logout` subprocess, then fresh probe / `notLoggedIn` push) and `providers/list|set|disable` ONLY if Node's list result for a client without provider support is a fixed shape (read `unstable_listProviders`, `:1900`); script `logout-auth` (AIR-less; `auth-status` transcript + a `logout` request; the fake's marker makes the fresh probe answer `loggedIn:false`, so Node's captured fixture shows the `notLoggedIn` push — if Node shows none, drop the claim and keep unit tests); anything not bounded -> PARITY `pending` with its consequence (3.5)
- [x] 2.5 Docs: PARITY (C) rows G2, G6, G10 `ported`; AIR-delete + fork-at-message `pending`; INVARIANTS INV-48, INV-50, INV-52; deviations under Decision 23
- [x] 2.6 Phase-1 review follow-ups: (a) PARITY hooks / `promptSuggestions` rows corrected to `pending` with the AIR consequence (PostModelSwitch, PostCompact, `session_info_update._meta.promptSuggestions`); (b) `emit_raw_updates` doc comment moved back; (c) dead `SessionExt.control` and the unused `ext` parameter of `handle_events` / `settle_events` / `handle_session_frame` removed; (d) `capture.sh` deletes a stale `<name>.argv.json` / `<name>.init.json` a script no longer produces; (e) `system_prompt.rs` mirrors Node's `TypeError` (-32603) for a missing / non-iterable `content` and a null block (`inv_39_type_errors_mirror_node`); (f) `meta_filter.rs` keeps `jetbrains:{}` without an `air` key (`inv_51_meta_filter_keeps_jetbrains_without_air`); (g) PARITY `fileTextKeys` row; (h) `run_recorder_with_init` asserts the raw `initialize` subtype before the lossy short-id normalisation

### Contract B — `auth-status`

- transcript = `text-only` line 1 + special line `{"auth_status_stdout":{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"pro","email":"a@b.c","orgName":"Org"}}`; `.acp.json` steps `initialize`, `{"call":"sleep","ms":600}`, `session/new`
- Node frame (verified): `_auth/status_update {authStatus:{kind:"account",label:"Claude Pro",account:{plan:"pro",email:"a@b.c",organization:"Org"}}}` between the initialize response and `session/new`

### Verify phase 2

- [x] 2.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential 2>&1 | grep -E "test result|FAILED"` -> ok; run 3 times
- [x] 2.T2 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn 2>&1 | grep -E "test result|FAILED"` -> ok, `git diff eaf2446 -- rust/claude-agent-acp-rs/src/turn/turn_tests.rs` shows only added lines or field additions
- [x] 2.T3 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib 'inv_48|inv_50|inv_52' 2>&1 | grep -E "test result|FAILED"` and `--test agent` -> ok
- [x] 2.T4 `grep -c '_auth/status_update' porting/fixtures/auth-status.frames.jsonl` == 1; `grep -c 'Session not found' porting/fixtures/session-close.frames.jsonl` == 1; `grep -c '(fork)' porting/fixtures/session-fork.frames.jsonl` >= 1; `grep -c '"model":' porting/fixtures/model-usage.frames.jsonl` >= 1
- [x] 2.T5 `CAPTURE_OUT_DIR=$PWD/rust/target/upstream/p2 bash porting/capture.sh >/dev/null 2>&1; for f in $(git ls-files porting/fixtures); do cmp -s $f rust/target/upstream/p2/$(basename $f) || echo CHANGED $f; done` prints nothing
- [x] 2.T6 `bash rust/scripts/rust-gate.sh 2>&1 | tail -4` ends `==> gate OK`; G7 diff empty; `JsonPath::new` count `0` (commands of 1.T6)

## Implementation Phase 3 — HIGH RISK: sessionFailure, file-change report, `/mcp`, goal

Sized for one turn; order = rising risk; STOP rules per item. Read `.vibekit/feature-plans/pending/claude-acp-rust/review-04-turn.md` and `review-05-turn.md` first; before and after EVERY `turn.rs` edit run `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn` (all pre-existing tests unmodified except mechanical field additions).

- [ ] 3.0 Read Decisions 16-17, 20-21, 26, `turn.rs:170-215,1001-1040,1161-1215`, `unfinished.rs:100-230`, `session.rs:1146-1300`, `dist/session-failure-extension.js`, `acp-agent.js:3545-3720,5240-5340,5736-5744,2060-2130,3243-3260`, `dist/file-change-audit.js`, `dist/mcp-command.js`, `dist/goal-extension.js`
- [x] 3.1 Goal (Decision 16): `goal.rs` + `inv_53_*` units BEFORE wiring; `SessionExt.goal`; actor hooks; `_session/goal` arm; script `goal-set-clear` (Contract C); `inv_24_goal` (commit fa86584, T1-G11b)
- [ ] 3.2 G9 (Decisions 17, 26): NOT DONE — `session_failure.rs` / `inv_54_*` do not exist yet; carry in a later T1 turn (owner: rust-air-gaps phase 3, G9). Replace `assistant_had_error: bool` by `AssistantFailureCtx{error:Option<String>, was_usage_limit:bool, title:Option<String>}` in `SessionExt` (fed from the `assistant` frame `error` field and `isSyntheticUsageLimitMessage`: model `<synthetic>` + text starting with an SDK `USAGE_LIMIT_ERROR_PREFIXES` entry — copy the prefixes from the SDK, `grep -o USAGE_LIMIT_ERROR_PREFIXES sdk.mjs`); `session_failure.rs` policy + `providerFailureCategory` + `inv_54_*` units first; `TurnEvent::Failed.usage` (4 sites); `unfinished.rs` forwards the settled usage; `handle_events` answers `end_turn` + `_meta.quota` (WITH model rows from 2.1) + `sessionFailure`; `auth_required` keeps the JSON-RPC path but its code becomes Node's `-32000` "Authentication required" (`RequestError.authRequired`): port with `inv_54_auth_required_code` and script `auth-required` (result "Please run /login"); scripts `session-failure-unfinished`, `session-failure-provider-error`; `inv_24_session_failure`
- [ ] 3.3 G4 (Decision 20): NOT DONE — `file_report.rs` / `inv_55_*` do not exist yet; carry in a later T1 turn (owner: rust-air-gaps phase 3, G4). `file_report.rs` + `inv_55_*`; spawn env; request-id capture; the report runs on EVERY non-cancelled settle (EndTurn, max_tokens, refusal, max_turn_requests — `settleActive`, `acp-agent.js:3552-3620`), a cancel arriving during the 2 s preview wins and publishes `unavailable{cancelled}`; deferral wrapper; unavailable paths; subagent edit recording; scripts `file-change-report` and `file-change-report-cancel-wins` (steps: AIR initialize, session/new, `session/prompt` `no_wait` with the report `_meta` via `Request`, `{"call":"sleep","ms":400}`, `session/cancel`, `session/wait`; transcript: result for the prompt, step expecting `rewind_files` that emits nothing, then a step expecting `interrupt` that emits the interrupt success and, via `$REQ.1`, the late `rewind_files` response — the withheld response makes the 400 ms cancel deterministic); `inv_24_file_change_report`, `inv_55_cancel_during_preview_wins`
- [x] 3.4 G12 (Decision 21): `mcp_command.rs` + `inv_56_*` (parse table, `formatMcpStatus` goldens produced by importing the installed `dist/mcp-command.js` with `node -e`), `local_command_output` mapping, `/mcp` replacement; scripts `mcp-command-status` then `mcp-command-reconnect`; `inv_24_mcp_status` (commit d3bc560, T1-G11a)
- [ ] 3.5 Docs and final: PARTIAL — goal + `/mcp` docs landed (INV-53, INV-56 in `porting/INVARIANTS.md`; commits fa86584 / d3bc560); G4 and G9 docs remain blocked on 3.2 / 3.3 (owner: rust-air-gaps phase 3). (PARITY rows, each with its CONSEQUENCE): ported G4, G9, G12, G8; `pending`: G7 native subagents (`nativeSubagentSessions` + `sessionCapabilities.subagents` advertised, no child sessions), AIR `session/delete` (-32601), fork-at-message, `diffPatch`, `planFile`, `recommendedValue`, `rawInputRendering` (advertised or recognised, inert), `auth.logout` and `providers` (whatever 2.4 did not port), `_meta.systemPrompt`, session-wide failure pushes (worker shutdown, transport lost, advisory/warning notices: `acp-agent.js:3868,4659,4705,6052`), file-report ordering overlap with a following turn (OQ-5; cancel-wins is ported and tested); INVARIANTS INV-53..INV-56; update the PARITY header test count; deviations under Decision 23

### Contract C — `goal-set-clear`

- `.acp.json`: AIR initialize (`capabilities:[]`), `session/new`, `request _session/goal {sessionId:"$SESSION",action:"set",objective:"ship it"}`, `request _session/goal {sessionId:"$SESSION",action:"clear"}`
- transcript = `text-only` line 1; step 2 echo `/goal ship it`, `{"type":"active_goal","value":{"condition":"ship it","iterations":0,"last_reason":null,"set_at":"2026-01-01T00:00:00Z"}}`, assistant, result; step 3 echo `/goal clear`, `{"type":"active_goal","value":null}`, result
- Node (verified): set -> `goal{objective,status:"active",controlMethod}` then `goal{…iterations:0,lastReason:null,createdAt,…}` then `{}`; clear -> `goal:null` twice then `{}`

### Verify phase 3

> T1-G16: 3.T5 re-run at HEAD `180ace9` (STABLE). 3.T1/3.T2/3.T6 pass for the whole workspace, but 3.T3/3.T4/3.T7 cannot be met until 3.2 (G9) and 3.3 (G4) land; they stay unticked (owner: rust-air-gaps phase 3).

- [ ] 3.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential 2>&1 | grep -E "test result|FAILED"` -> ok; run 3 times
- [ ] 3.T2 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn 2>&1 | grep -E "test result|FAILED"` -> ok; same diff check as 2.T2
- [ ] 3.T3 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib 'inv_53|inv_54|inv_55|inv_56' 2>&1 | grep -E "test result|FAILED"` -> ok
- [ ] 3.T4 `grep -c 'sessionFailure' porting/fixtures/session-failure-unfinished.frames.jsonl` >= 1; `grep -c 'agentFileChangeReport' porting/fixtures/file-change-report.frames.jsonl` >= 1; `grep -c 'cancelled' porting/fixtures/file-change-report-cancel-wins.frames.jsonl` >= 1; `grep -c '"goal"' porting/fixtures/goal-set-clear.frames.jsonl` >= 4; `grep -c 'Connected' porting/fixtures/mcp-status.frames.jsonl` >= 1; `grep -c '32000' porting/fixtures/auth-required.frames.jsonl` >= 1
- [x] 3.T5 clean-cache reproduction (the `rm -rf` deliberately wipes only the git-ignored Node capture cache, to prove fixtures reproduce from a fresh install): `rm -rf rust/target/upstream/0.94.0; CAPTURE_OUT_DIR="$PWD/rust/target/upstream/final" bash porting/capture.sh >/dev/null 2>&1; diff -r porting/fixtures rust/target/upstream/final && echo STABLE` (reproduced into `rust/target/parity/final` without touching the shared oracle; STABLE — T1-G16)
- [ ] 3.T6 `bash rust/scripts/rust-gate.sh 2>&1 | tail -4` ends `==> gate OK`; `bash rust/scripts/selftest-gate.sh 2>&1 | tail -3` no FAILED; G7 diff empty; `JsonPath::new` count `0`
- [ ] 3.T7 every inert-but-advertised capability has a PARITY `pending` row with a consequence: `for k in logout providers diffPatch planFile recommendedValue nativeSubagentSessions subagents 'AIR session/delete' 'fork-at-message'; do grep -c "$k" porting/PARITY.md; done` prints no `0`

## Files & Phase Impact

| File | Status | Phase | Contract / change |
|------|--------|-------|-------------------|
| `rust/acp-recorder/src/script.rs` | modified | 1 | `Step::Request`, `Step::Sleep`, `seed_files`, `capture_init` |
| `rust/acp-recorder/src/recorder.rs` | modified | 1 | request/sleep execution, recv-line channel, placeholders, seeding |
| `rust/fake-claude/src/lib.rs`, `main.rs` | modified | 1 | `$CWD`, `auth` mode, env allowlist |
| `rust/fake-claude/tests/*.rs` | modified | 1 | tests for the three additions |
| `porting/capture.sh` | modified | 1 | `<name>.init.json` for `capture_init` scripts |
| `porting/corpus/*` (18 scripts, 2 seed dirs) | new | 1-3 | see Gap register |
| `porting/fixtures/*` | new | 1-3 | Node-captured only |
| `rust/claude-agent-acp-rs/src/lib.rs` | modified | 1-3 | new `pub mod` lines |
| `src/session_ext.rs` | new | 1 | Decision 7 |
| `src/system_prompt.rs` | new | 1 | Decision 10 |
| `src/auth_status.rs` | new | 2 | Decision 11 |
| `src/meta_filter.rs` | new | 1 | Decision 25 |
| `src/session_store.rs` | new | 2 | Decision 14 |
| `src/goal.rs` | new | 3 | Decision 16 |
| `src/session_failure.rs` | new | 3 | Decision 17 |
| `src/file_report.rs` | new | 3 | Decision 20 |
| `src/quota.rs` | new | 2 | Decision 19 |
| `src/mcp_command.rs` | new | 3 | Decision 21 |
| `src/agent.rs` | modified | 1-3 | request arms (`:591-834`), caps, env |
| `src/control.rs` | modified | 1 | `append_system_prompt` |
| `src/session.rs` | modified | 1-3 | Commands, `SessionExt`, hooks, Failed/Settled arms |
| `src/turn.rs` | modified | 2-3 | model side-fields (2), `Failed.usage` (3) ONLY |
| `src/unfinished.rs` | modified | 3 | forward usage |
| `src/map.rs`, `src/tools.rs`, `src/permission.rs` | modified | 1, 3 | AIR meta; `local_command_output` |
| `src/async_tasks.rs` | modified | 1 | stop methods |
| `tests/common/mod.rs` | modified | 1 | hermetic HOME, init capture, uuid substring |
| `tests/differential.rs` | modified | 1-3 | `inv_24_*` additions |
| `tests/system_prompt.rs`, `tests/agent.rs` | new / modified | 1-2 | wire tests |
| `porting/PARITY.md`, `porting/INVARIANTS.md` | modified | 1-3 | rows flipped, INVs added |

## Out of scope

- G7 native subagent sessions (Decision 18), AIR `session/delete` archive and AIR `_meta.jetbrains.air.fork` fork points (Decisions 15, 22) -> `pending` rows with the missing piece named
- Adjacent advertised AIR capabilities not among the 12: `diffPatch`, `planFile`, `recommendedValue`, `rawInputRendering`, `sessionIndex` family, `_meta.systemPrompt`, gateway / provider auth, elicitation, hooks, history replay -> stay as today (PARITY)
- Cancel family R90-2 / R91-3 / R86-8, stamped-result lanes, `endedPerLevel`, `msg_lifecycle_v1` (HIGH RISK, real-claude evidence needed)
- Any `src/`, `dist/`, `package.json`, `scripts/`, `.github/`, `docs/` change; commits, pushes, tags

## Open questions

| # | Question | Recommendation |
|---|----------|----------------|
| OQ-1 | Keep advertising `nativeSubagentSessions` / `sessionCapabilities.subagents` while G7 is `pending`? | Keep (fixture parity); disclosed in the final report as a known inert capability |
| OQ-2 | AIR `session/delete`: keep -32601 vs hard delete? | Keep -32601; hard delete destroys data Node archives; port the archive in a follow-up if AIR is a target consumer |
| OQ-3 | Fork verification without the real CLI | RESOLVED: Rust copies the transcript (Decision 15) and the script lists the fork afterwards; no human smoke |
| OQ-4 | Phase 3 `TurnEvent::Failed.usage` and `TurnMachine` model side-fields touch `turn.rs` | Accept; G10 lands first (Decision 26); STOP rules in Decisions 17, 19 |
| OQ-5 | File-change report ordering when the next turn's frames arrive during the 2 s `rewind_files` wait (Node blocks its consumer) | Accept the spawned design; PARITY records the overlap; cancel-wins is ported and tested (3.3) |
| OQ-6 | Should `session/list` honour AIR `sessionIndex`? | No; Rust never advertises it, so AIR gets the plain SDK listing like every client |
| OQ-7 | `claude auth status --json` runs on every `initialize`; hosts without a `claude` on PATH | Probe failure is silent (no push), same as Node `return undefined` |
| OQ-8 | Phases are large for one weak-model turn | Rebalanced: P1 infra + 3 items, P2 four items (model rows first), P3 four items; leftovers carry to a resume turn, never downgraded without a STOP report |
