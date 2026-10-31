---
title: "research: upstream claude-agent-acp 0.70.0 -> 0.94.0 behavioural-change inventory"
feature: rust-upstream-sync
worktree: /Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1
---

<!--
RULES — read before writing or implementing:
1. FORMAT: Bullets, tables, code, diagrams ONLY — no prose paragraphs
2. REQUIREMENTS: One crisp line each — no verbose descriptions
3. CHECKLIST: Mark items [x] as you complete them — this is your persistent todo list
4. READING TIME: Optimize for fast human scanning — if it's hard to skim, rewrite it
-->

## How to read this file

- Evidence base: CHANGELOG (`git show v0.94.0:CHANGELOG.md`, tags fetched with `git fetch upstream --tags`), commit subjects `git log v0.70.0..v0.94.0 -- src`, `src/` at both tags, `dist/` of both npm tarballs, SDK tarballs 0.3.232 / 0.3.303
- EMPIRICAL baseline (section 1): the existing 21-script corpus was captured against an `npm install`ed 0.94.0 adapter and diffed against the committed 0.70.0 fixtures — those divergences are facts, not inferences
- Classes: `PORT-E` = port into the existing Rust subset · `PORT-N` = port new (async tasks / AIR) · `SKIP` = deliberate skip (PARITY.md section B category named in the row) · `CLI` = changes the argv / stream-json we speak to `claude`
- `Ph`: plan phase that does the work (`1`/`2`/`3`) or `pend` = recorded `pending` in PARITY.md with reason (never silently dropped)
- Risk `HIGH` = could change turn-settlement semantics (turn.rs lanes, cancel, steer, orphan accounting)
- Locations: `dist/X` = npm dist file, `S:NNNN` = `src/acp-agent.ts` line at tag v0.94.0, `C/` = `rust/claude-agent-acp-rs/src/`
- Row ids (`R71-1`, `E-3`, `S-4`) are cited by the plan

## 1. Empirical divergences: existing corpus, Node 0.94.0 vs committed 0.70.0 fixtures

Method (reproducible; same steps become `capture.sh`, plan Phase 1):

- `npm install --prefix <dir> --ignore-scripts --omit=optional --omit=dev --no-audit --no-fund --no-package-lock @agentclientprotocol/claude-agent-acp@<ver>` (69 MB; `--omit=optional` drops the 270 MB native `claude` binaries and changes no capture byte)
- Run `porting/capture.sh` logic with `DIST=<dir>/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js` and `CAPTURE_OUT_DIR=<scratch>`
- FACT F1: the host shell exports `ANTHROPIC_MODEL` and `CLAUDE_*` vars; unscrubbed they leak into the Node adapter (it sent `set_model` -> fake-claude mismatch -> every 0.70.0 capture differed). `capture.sh` MUST run the adapter under `env -i` with an allowlist (`PATH`, `TMPDIR`, `LANG`, `LC_*` + explicit assignments): 0.94 also reads `SSH_*`/`NO_BROWSER` (authMethods), `MAX_THINKING_TOKENS` (initialize), `IS_SANDBOX`, `NODE_OPTIONS`; verified that an `env -i` capture of 0.70.0 reproduces all fixtures
- FACT F2: with F1 applied, `npm install`ed 0.70.0 (SDK 0.3.232) reproduces ALL 43 committed `porting/fixtures/*` byte-for-byte, so the npm-install capture path is equivalent to building `src/`
- FACT F3: the captures are deterministic (two runs of 0.94.0 identical)
- FACT F4: 0.94.0 `engines.node >=22`; local node is v26.12.0 (OK)
- FACT F5: fake-claude replays strictly in order and exits on the first unexpected stdin frame; 0.94.0 sends two frames the 0.70.0 transcripts do not expect (E-7, E-8)

| # | Divergence (ACP wire unless noted) | Evidence | Class | Rust target | Risk | Ph |
|---|-----------------------------------|----------|-------|-------------|------|----|
| E-1 | `initialize.agentInfo.version` `0.70.0` -> `0.94.0` | all 21 fixtures | CLI-adjacent (version pin) | `C/agent.rs:33` `AGENT_INFO` composed by build.rs | none | 1 |
| E-2 | `initialize.agentCapabilities._meta.authStatus: {}` added | text-only [1] | PORT-E | `C/agent.rs:75-103` | none | 2 |
| E-3 | `initialize.agentCapabilities.sessionCapabilities.systemPrompt: {mode:{override:{}}}` added | text-only [1] | PORT-E | `C/agent.rs:75-103` | none | 2 |
| E-4 | `initialize._meta.jetbrains` (air caps) and `_meta.goal` REMOVED for a non-AIR client (recorder's `clientCapabilities` has no `_meta.jetbrains.air`); `_meta.steering` stays | text-only [1] | PORT-E | gate on parsed client caps, `C/agent.rs:91-100`, `:542-548` (today `initialize` ignores params) | none | 2 |
| E-5 | `available_commands_update`: adapter appends `/mcp` (`input.hint "[reconnect\|enable\|disable [<server>\|all]]"`) and `/remote-control` (`input.hint "[session name \| off]"`); a CLI-provided `mcp` entry is removed (adapter's replaces it, moved to the end); input-less CLI commands keep `input: null` | commands-proactive [4], every other script [4] | PORT-E | `C/map.rs` `map_commands_changed`, `C/agent.rs` proactive update (`inv_24_commands_proactive`) | none | 2 |
| E-6 | prompt response gains `_meta.quota = {token_count:{totalTokens,inputTokens,cachedInputTokens,cachedWriteTokens,outputTokens,reasoningOutputTokens:0}, model_usage:[{model,token_count}...]}` on every settle that reports `usage` (not on queued-sweep cancel, which has no usage) | 15 of 21 scripts | PORT-E | `C/turn.rs` `Usage` + `C/agent.rs:605-633` reply builder | LOW | 2 |
| E-7 | stdin: `control_request{subtype:get_context_usage, detail:"summary"}` after session creation (background context-window probe) | fake-claude mismatch on first user frame | CLI | fake-claude must auto-ack; Rust does NOT send it | none | 1 |
| E-8 | stdin: `control_request{subtype:cancel_async_message, message_uuid}` for each queued prompt / pending steer a cancel abandons, BEFORE `interrupt` (not awaited) | cancel-queued-echo-less | CLI | `C/session.rs:520-575` Cancel arm + `C/turn.rs:374` | HIGH (cancel/orphan) | 2 |
| E-9 | `tool_call.name` (programmatic tool name, e.g. `"Bash"`) added; `tool_call_update` unchanged | tool-result-echo [6], tool-then-text [6] | PORT-E | enable cargo feature `unstable_tool_call_name` of `agent-client-protocol` 2.1.0 (same version, G10 holds) -> `ToolCall::name(..)` at `C/map.rs:195` | none | 2 |
| E-10 | `session/request_permission`: options `[allow-once "Yes" allow_once, reject "No" reject_once]` (was `[reject "Deny", allow "Allow Once", allow_always "Always Allow" + _meta.permission]`); `toolCall` gains `name` and `status:"pending"`; selected option id `allow-once` (was `allow`) | permission-allow/deny | PORT-E | `C/permission.rs:51-80` `map_outcome`, `:249-276`, `:278-322`; recorder maps `permission:"allow"` by option kind | MED | 2 |
| E-11 | CLI-bound permission `control_response` gains `toolUseID` + `decisionClassification` (`user_temporary` allow-once, `user_permanent` allow-with-updates, `user_reject` deny); deny message unchanged `"User refused permission to run tool"` | `src/permissions/effects.ts:34-58` | CLI | `C/permission.rs:359-420` | LOW | 2 |
| E-12 | A turn that ends `end_turn` while a dispatched foreground tool call has no `tool_result` now FAILS: `tool_call_update{status:"failed", content:[text "Claude ended the turn without returning a result for this tool."]}` per tool, then JSON-RPC error `-32603` `"Internal error: Claude ended the turn without returning results for tool calls: <ids>"` `data:{errorKind:"incomplete_tool_call"}` | single-tool, multi-tool, permission-*, streamed-partial-input, subagent-* (their transcripts lack `tool_result`) | PORT-E | session.rs post-filter on `TurnEvent::Settled` (plan Decision 7) | HIGH | 3 |
| E-13 | argv switches to `--flag=value` for valued flags: `--permission-prompt-tool=stdio`, `--disallowedTools=AskUserQuestion`, `--tools=default`, `--permission-mode=default` (+`--model=`, `--resume=`); `--setting-sources=`/`--session-id=` already `=`; trailing `""` after `--replay-user-messages` unchanged | all `*.argv.json` | CLI | `C/process.rs:169-243` `build_argv` | none | 2 |
| E-14 | `initialize` control_request: hooks now `PostToolUse, PostModelSwitch, PostCompact, TaskCreated, TaskCompleted` (was 3), `promptSuggestions:true` added; `forwardSubagentText:false` unchanged | `initialize.json` | CLI | `C/control.rs:98-170` `InitializeOptions` (+`prompt_suggestions`), unit test `inv_11_initialize_matches_adapter` `C/control.rs:658` | none | 2 |
| E-15 | child env: `CLAUDE_CODE_SDK_READS_SESSION_STATE=1` set by SDK 0.3.303 (not visible in fixtures today: not in `ARGV_ENV_ALLOWLIST`, `rust/fake-claude/src/lib.rs:141`) | SDK findings S-3 | CLI | `C/process.rs:445-455`; extend allowlist | MED (CLI idle semantics) | 2 |
| E-16 | user stdin frame keeps `origin:{kind:"human"}`, `parent_tool_use_id:null` (no change; Rust already sends `origin`, `C/agent.rs:227`) | raw stdin dump | CLI | none | none | - |
| E-17 | Scripts unchanged on the ACP wire apart from E-1..E-6: text-only, error-result, idle-without-result, resume-load, cancel-mid-turn, eof-mid-turn, lagging-idle, context-echo-less, steer-mid-turn, steer-error-result, tool-result-echo, tool-then-text | cmp output | - | - | - | - |
| E-18 | Tool-using transcripts that never return a `tool_result` additionally gain the completed-tool `tool_call_update` once a `tool_result` is added (matches existing Rust behaviour, phase-15 item 3) | corp2 experiment | PORT-E (none) | none | none | 2 |
| E-19 | `subagent-drain` / `subagent-followup` emit top-level `{"type":"task_started"}` / `{"type":"task_notification"}` (not `system` + `subtype`): both Node and Rust ignore them, so the subagent hold (INV-16) is covered by unit tests only, not by the differential | transcripts | - | fix in plan 3.11b | HIGH if fixed (held-turn lanes) | 3 (deferrable) |

## 2. Per-release inventory 0.71.0 .. 0.94.0

### 0.71.0 (2026-08-31)

| # | Change | Location | Class | Rust target / reason | Risk | Ph |
|---|--------|----------|-------|----------------------|------|----|
| R71-1 | AI-generated session title | `dist/session-titles.js` (295), `session_info_update` | SKIP (SessionInfoUpdate is a discarded variant) | none | none | - |
| R71-2 | Claude modes aligned (`auto` replaces `dontAsk`, Claude Code labels), clear-context planning (ExitPlanMode "clear" options, query restart under the same ACP turn) | `dist/session-mode.js`, `clear-context-coordinator.js`, `exit-plan.js`, `permissions/modes.js` | SKIP (config/modes; the rename is HIDDEN by the existing `result.modes`/`configOptions` ignores, so it gets an explicit PARITY row, plan 3.14) | optional refresh of static JSON `C/agent.rs:106` | none | pend |
| R71-3 | SDK bump 0.3.238 | package.json | CLI | see section 3 | - | 2 |
| R71-4 | Native subagent sessions (`nativeSubagentSessions`): Agent/Task as child ACP sessions | `dist/native-subagents.js` (1284), `acp-subagents.js` | SKIP (subagent transcripts, AIR) | none | none | pend |
| R71-5 | AIR `asyncTasks`: `async_task_spawned` / `async_task_progress` / `async_task_state_update` session updates for non-subagent background work; Bash `tool_call_update._meta.jetbrains.air.asyncTasks.backgrounded:true` | `dist/async-tasks.js` (866), `docs/air-extensions.md` "Async tasks" | PORT-N | new `C/async_tasks.rs`, `C/session.rs` frame routing `:814-845`, `C/map.rs` | MED | 3 |
| R71-6 | `_session/async_task/stop` request -> SDK `stopTask` (`control_request{subtype:"stop_task",task_id}`), `{stopped:bool}`, notice line | `S:4388-4402` | PORT-N | `C/agent.rs:704` request arm + `C/control.rs` | LOW | 3 |
| R71-7 | Permission mode `kind` meta (`_meta.jetbrains.air.kind`) | `dist/session-mode.js` | SKIP (AIR/modes) | none | none | - |
| R71-8 | Per-model token usage `_meta.quota` on prompt responses | `S:11866-12140` | PORT-E | = E-6 | LOW | 2 |
| R71-9 | Message-specific session forks (`session/fork` `_meta.jetbrains.air.fork`) | `dist/fork-session.js` (143) | SKIP (Rust answers `-32601`) | none | none | - |
| R71-10 | Defer steering while user input is pending: steer `priority:"later"` when a permission/elicitation is open, else `now` | `S:4369`, `S:9576` | PORT-E / CLI | `C/session.rs:636-641` (pending permission count already tracked in actor) | HIGH (steer lane) | 3 |
| R71-11 | OAuth2 for MCP server connections | commit 0cbbaf3 | SKIP (auth, MCP passthrough) | none | none | - |
| R71-12 | zod min-version fix | package.json | SKIP (npm-only) | none | none | - |

### 0.72.0 .. 0.76.0

| # | Change | Location | Class | Rust target / reason | Risk | Ph |
|---|--------|----------|-------|----------------------|------|----|
| R72-1 | Per-model effort settings | `dist/session-effort.js` | SKIP (config/models) | none | none | - |
| R72-2 | `result.user_message_uuid` exact join key: empty-interruption diagnostic consumes exactly the token it names; `ensureActiveTurn` consumes the named orphan | `S:5901-5916`, `S:7180`, `S:1513` | PORT-E | `C/turn.rs:970` `ensure_active_turn`, `:616` `on_result` | HIGH (INV-23/30 orphan accounting) | pend (heuristic lane remains valid, see plan OQ-5) |
| R72-3 | SDK 0.3.252; 0.73.0 SDK bump | package.json | CLI | section 3 | - | 2 |
| R74-1 | `--hide-claude-auth` refuses claude.ai subscriptions | `dist/hide-claude-auth.js` (223) | SKIP (auth) | none | none | - |
| R75-1 | Usage statistics rendered as Markdown for exact `/usage` (and `/mcp`) local commands | `dist/usage-markdown.js` (155) | SKIP (formatting; command still runs through the CLI) | verify `/usage` stays prompt-pass-through | none | - |
| R75-2 | Context compaction surfaced as an ACP tool lifecycle (synthetic tool call + updates for `status: compacting` / `compact_boundary`) | `dist/context-compaction.js` (341), `tool-calls/reporters/compaction.js`, `S:6033` | PORT-E | new frames for ALL clients on `/compact`; not in corpus | MED | pend |
| R75-3 | `authStatus` extension: capability marker `agentCapabilities._meta.authStatus:{}` + `_auth/status_update` push after a background CLI auth probe | `dist/auth-status.js` (285), `S:1293-1300` | PORT-E (marker only) | marker = E-2; push -> SKIP (auth), recorded pending (marker advertised, never pushed) | none | 2 / pend |
| R75-4 | Restore session forks, faster `session/load` | `dist/resumed-session.js` | SKIP (D15) | none | none | - |
| R76-1 | Recommended config values | `dist/session-model.js` | SKIP (config/models) | none | none | - |

### 0.77.0 .. 0.80.0

| # | Change | Location | Class | Rust target / reason | Risk | Ph |
|---|--------|----------|-------|----------------------|------|----|
| R77-1 | Honor host opt-out `_meta.claudeCode.options.allowDangerouslySkipPermissions:false` (omits `--allow-dangerously-skip-permissions`) | `dist/permissions/modes.js`, `S:~8256` | CLI | `C/agent.rs:274-300` `options_from_meta`, `C/process.rs:225` | LOW | 3 |
| R77-2 | Multi-select answers kept with custom answer | `dist/elicitation.js` | SKIP (elicitation) | none | none | - |
| R77-3 | TaskList output parsed without ambiguous regex (plan lane for Task* tools) | `dist/tools.js` | PORT-E | `C/map.rs:650` Task* plan lane; verify only | none | 3 |
| R77-4 | Strip injected system reminders from replayed prompts | `dist/resumed-session.js` | SKIP (D15 replay) | none | none | - |
| R77-5 | SDK 0.3.270 | package.json | CLI | section 3 | - | 2 |
| R77-6 | Tool names on tool calls (`tool_call.name`) | `dist/tool-calls/renderer.js`, `S` toAcpNotifications | PORT-E | = E-9 | none | 2 |
| R77-7 | BREAKING: `claudeCode.options.agent` no longer forwarded, agent config option removed | `dist/acp-agent.js` | SKIP (config) | Rust forwards only model/permissionMode (`C/agent.rs:274`) | none | - |
| R78-1 | File changes reported from Claude checkpoints; AIR diff counts from structured patches | `dist/file-change-audit.js` (379->290) | SKIP (file-change audit, JetBrains/AIR) | none | none | - |
| R78-2 | Experimental ACP `compaction_update` (client declares `session.compaction`) | `dist/context-compaction.js` | SKIP (extension) | tied to R75-2 | none | pend |
| R78-3 | AskUserQuestion keeps picked option with custom text | `dist/elicitation.js` | SKIP (elicitation) | none | none | - |
| R79-1 | SDK 0.3.274 | package.json | CLI | section 3 | - | 2 |
| R79-2 | Shell commands shown in permission prompts (title = full command) | `dist/permissions/presentation.js` | PORT-E | already equal in corpus (`title:"echo hi"`); keep green | none | 2 |
| R80-1 | Interrupted compaction lifecycles closed | `dist/context-compaction.js` | SKIP | tied to R75-2 | none | pend |
| R80-2 | SDK 0.3.278 | package.json | CLI | section 3 | - | 2 |
| R80-3 | Prefer `terminal_output_delta` | `dist/tool-calls/client-capabilities.js` | SKIP (terminal support) | none | none | - |

### 0.81.0 .. 0.85.1

| # | Change | Location | Class | Rust target / reason | Risk | Ph |
|---|--------|----------|-------|----------------------|------|----|
| R81-1 | Experimental session notices for live advisories | `dist/session-notices.js` (69) | SKIP | none | none | - |
| R811-1 | Model on usage updates; preserve context usage when quota exhausted | `S` usage_update | SKIP (UsageUpdate discarded) | none | none | - |
| R811-2 | Recreate resumed sessions when any session option changes; recreate query when skills change | `dist/resumed-session.js`, `settings.js` | SKIP (D15, skills, settings watching) | none | none | - |
| R811-3 | Native Vertex endpoint defaults; managed-policy tier unreadable must not abort startup | `dist/managed-policy.js` (31), `index.js:46-50` | SKIP (auth/settings); Rust inherits env only | none | none | - |
| R811-4 | Login result text gated on error state (`#863`) | `S` result handling | PORT-E | verify `authRequired` mapping `C/agent.rs:329,345` unchanged for success result with login text | LOW | 3 |
| R811-5 | Marker-only slash skill prompts replayed | `dist/resumed-session.js` | SKIP (replay) | none | none | - |
| R811-6 | Settle steered turns on the result that answers the steer: result `user_message_uuids` (fallback `user_message_uuid`) naming a steer retires its echo; turn leaves the steer lane and settles at that result; idle fallback counts only a result recorded after the last echo (`steeredAwaitingResult`); a ready steered turn takes the idle ahead of owed-idle debt | `S:~890-930` (Turn fields), `S:4274-4311` idle lanes, `S:4844-4860` result lane, commit 9392c69 | PORT-E | `C/turn.rs` steer lanes `:343` `mark_steered`, `:616` `on_result`, `:829` `on_idle` | HIGH (just-ported steering lanes) | 3 (deferrable -> pend) |
| R811-7 | `permissions.disableBypassPermissionsMode`; bypass offered alongside auto in plan approval; effective mode published after plan approval | `dist/permissions/modes.js` | SKIP (modes/config) | none | none | - |
| R811-8 | Informational SDK frames tagged `_meta.claudeCode.{kind:"informational",level}` (text unchanged: non-`info` levels prefixed `**Warning:** `); Rust never handled `informational` at all (pre-existing gap) | `S:6354-6400` (0.70: `S:3386`) | PORT-E | `C/session.rs:814-860` + `C/map.rs` | LOW | 3 (deferrable -> pend) |
| R811-9 | Write tool calls render `path`/`file_text` aliases | `dist/tool-calls/reporters/file-edit.js` | PORT-E | `C/tools.rs` Write shape (`file_path`/`content` only today) | LOW | 3 |
| R812-1 | Resumed subagent generations announced; clear-context plans continued in background followup | `dist/native-subagents.js`, `clear-context-coordinator.js` | SKIP | none | none | - |
| R82-1 | AIR tool call contract: `ToolReporter` per tool + `AcpToolCallRenderer` + `ToolCallFieldTracker` (drops unchanged top-level fields; an update with nothing new is not sent, for EVERY client); AIR-only: `commandTitle`, `subagent`, `skill`, `mcp` meta, `rawInputRendering`, exact git patches (`diffPatch`), plan file | `dist/tool-calls/*` (~2.7k lines), `tools.js` 1111->350, `diff.js` (619), `docs/air-extensions.md` "Tool call contract" | PORT-E (field tracker for non-AIR) / PORT-N (AIR Bash subset) / SKIP (diffPatch, planFile, rawInputRendering, skill, mcp meta) | field-tracker: no corpus divergence observed (E-17/E-18) -> pend with verify; AIR Bash `commandTitle` + `asyncTasks.backgrounded` in `C/tools.rs`/`C/map.rs` | MED | 3 / pend |
| R82-2 | ACP auth errors without access updates | `dist/auth-status.js` | SKIP (auth) | none | none | - |
| R83-1 | SDK 0.3.283; 0.84.0 SDK update | package.json | CLI | section 3 | - | 2 |
| R85-1 | Legacy custom-answer key to AIR | `dist/elicitation.js` | SKIP | none | none | - |
| R85-2 | SDK 0.3.286; adapt to SDK 0.3.284 replay + ultracode changes | package.json, `S` | CLI / SKIP (replay) | section 3 | - | 2 |
| R851-1 | Opt-in experimental ACP v2 handshake (`CLAUDE_AGENT_ACP_EXPERIMENTAL_V2=1`, `dist/v2/*`, `dist/serve.js`) | `dist/serve.js`, `dist/v2/*` (~1.4k lines) | SKIP (separate protocol surface; Rust is v1-only). Covers ~30 later "…on the experimental ACP v2 surface" commits in 0.86-0.92 | none | none | - |
| R851-2 | Answer `session/close` without waiting for the interrupt reply | `S:8923` | SKIP (Rust has no `session/close`; hard-coded capability advertised, see OQ-7) | none | none | pend |
| R851-3 | Background task stops restored on replay | `dist/subagent-replay.js` | SKIP (replay) | none | none | - |
| R851-4 | FAIL turns with unfinished tools (= E-12); streamed `tool_use` never dispatched is closed (`renderer.unfinished({ran:false})`) without failing the turn; background-task tool ids exempt | `S:5400-5480` `settleActive`, `S:5380` `closeCycleToolCalls` | PORT-E | session.rs `Settled` post-filter; exempt ids = `live_background` parents (`C/session.rs:826`) | HIGH | 3 |

### 0.86.0 .. 0.89.1

| # | Change | Location | Class | Rust target / reason | Risk | Ph |
|---|--------|----------|-------|----------------------|------|----|
| R86-1 | Context-window probe uses `get_context_usage {detail:"summary"}` (no per-category `count_tokens`) | `S:7858-7900` | CLI | fake-claude auto-ack (E-7); Rust never probes (usage_update discarded) | none | 1 |
| R86-2 | `/mcp` answered in chat (MCP server status + reconnect) and listed in `available_commands` | `dist/mcp-command.js` (326) | PORT-E (list entry) / SKIP (handler) | list = E-5; handler recorded pending (Rust forwards `/mcp` text to claude) | LOW | 2 / pend |
| R86-3 | Command exit code reported only when the tool result says it | `dist/tool-calls/reporters/bash.js` | SKIP (terminal) | none | none | - |
| R86-4 | Main-thread agent config option restored | `dist/session-config-ids.js` | SKIP (config) | none | none | - |
| R86-5 | Force-cancel firing during an awaited client update ends the cancelled turn | `S` cancel | SKIP (Rust actor never awaits a client update inline, INV-27) | none | none | - |
| R86-6 | A subagent's permission requests count as the turn waiting on the user (`openUserInputs`, `blocksForeground`) | `S:11990-12020`, `S:1187` | PORT-E | feeds R71-10 `later` decision | HIGH (steer) | 3 |
| R86-7 | Closed session's turns end before `session/close` answers | `S` closeSession | SKIP (no `session/close`) | none | none | pend |
| R86-8 | Held turn cancelled mid-followup ends at its interrupt's idle (not inline) | commit d3e673b, `S:~3528-3575` | PORT-E | `C/turn.rs:374-458` cancel inline-settle | HIGH (cancel) | pend |
| R86-9 | Drop queued prompts a cancel ends from Claude Code's queue (`cancel_async_message` per uuid before `interrupt`, not awaited) | commit d83f28a, `S:8352-8383` | CLI / PORT-E | = E-8 | HIGH | 2 |
| R86-10 | Experimental ACP v2: lifecycle, prompts, turn events, tool calls, plans, permissions, diffs, terminals, sessions, replay, providers, fork, notices, compaction (~20 commits) | `dist/v2/*` | SKIP (see R851-1) | none | none | - |
| R86-11 | `turn-events.ts` internal refactor (inserted/ended/failed callbacks) | `src/turn-events.ts` | SKIP (internal) | none | none | - |
| R87-1 | Settle prompts folded into a task-notification cycle: a `task-notification`-origin result whose `user_message_uuids` names a pending prompt answers it (held-for-subagents turn still waits) | commit 390b0b9, `S:1513` | PORT-E | `C/turn.rs:91` `AUTONOMOUS_RESULT_ORIGINS`, `:616` `on_result` | HIGH (a pending prompt can hang) | pend (see OQ-5) |
| R87-2 | Async task routing, Monitor tasks ignored, structured task ids, abandoned tool calls | `dist/async-tasks.js`, commit 0e83a06 | PORT-N | = R71-5 | MED | 3 |
| R87-3 | SDK bumps (0.3.287 + minor group) | package.json | CLI | section 3 | - | 2 |
| R88-1 | SDK 0.3.293 | package.json | CLI | section 3 | - | 2 |
| R89-1 | AIR `sessionIndex` extension (bounded `session/list`, rename/archive, subscriptions, worktrees) | `dist/session-index/*` (~4.3k lines) | SKIP (session list/AIR) | none | none | - |
| R89-2 | `@agentclientprotocol/sdk` 1.8.0 | package.json | SKIP (npm dep; arbiter = captured wire) | none | none | - |
| R89-3 | Custom instructions via AIR `session/new` meta (`_meta.jetbrains.air.customInstructions`) | `dist/air-extension.js:110` | SKIP (AIR config) | none | none | - |
| R89-4 | Logout recreates live sessions when it clears a provider override | `S` auth | SKIP (auth/providers) | none | none | - |
| R891-1 | Renamed sessions pushed to list subscribers | `dist/session-index/list-subscriptions.js` | SKIP | none | none | - |

### 0.90.0 .. 0.94.0

| # | Change | Location | Class | Rust target / reason | Risk | Ph |
|---|--------|----------|-------|----------------------|------|----|
| R90-1 | SDK 0.3.296 | package.json | CLI | section 3 | - | 2 |
| R90-2 | Cancel family: close open tool calls of a held/settling turn a cancel ends; end tool calls of a cancelled turn BEFORE reporting the cancel; drop streamed output of prompts a cancel abandoned; end a cancelled turn at its interrupted result (`terminal_reason aborted_streaming/aborted_tools`) and deliver CC's follow-up after a cancel; `request_marker_lists_v1` `status:requesting` frames | commits 32c23db, 53df1c7, 1137df3, b007ac7; `S:5305`, `S:11989`, `S:5957` | PORT-E | `C/turn.rs:374` cancel, `C/session.rs` Cancel arm; needs new frames not in corpus | HIGH | pend (corpus-invisible; see OQ-4) |
| R90-3 | Subagents RFD for non-AIR clients; subagent session id on replay; requires_action | `dist/native-subagents.js` | SKIP (subagent transcripts) | none | none | - |
| R90-4 | IDE session hooks accepted without dropping model settings; PostToolUse routed to subagent child session | `dist/settings.js` | SKIP (hooks) | none | none | - |
| R90-5 | Central helpers shown as "JetBrains Air Gateway" | `dist/auth-status.js` | SKIP (auth) | none | none | - |
| R90-6 | Forked session promptable at once | `dist/fork-session.js` | SKIP (fork) | none | none | - |
| R90-7 | Terminal of a background command stays running until its task ends | `dist/tool-calls/reporters/bash.js` | SKIP (terminal) | none | none | - |
| R91-1 | agentWorktree rows; `session/delete` without transcript; models narrowed to `providers/set` gateway list | `dist/session-index/worktrees.js`, `gateway-models.js` | SKIP (session list, models) | none | none | - |
| R91-2 | Keep background subagents running after a parent cancel (clients that can cancel them); cancel one subagent via `session/cancel` on its child session; deliver output of a subagent a cancel did not stop | `dist/native-subagents.js` | SKIP (subagent RFD clients) | none | none | - |
| R91-3 | Close tool calls of a cycle Claude Code started and a cancel stopped / of a stopped background subagent / abandoned at cycle end; end terminal of an unresulted command; show nothing of a tool cancelled work asks to run after the cancel was reported | commits 594aa8c, b4788d6, 1fd217f, 9a1b90f, ebf8b49, 764f339 | PORT-E | part of the cancel family (R90-2) | HIGH | pend |
| R91-4 | Stop waiting for a question the client leaves open after CC cancels it | `dist/elicitation.js` | SKIP (elicitation) | none | none | - |
| R91-5 | SDK 0.3.300 | package.json | CLI | section 3 | - | 2 |
| R92-1 | `@agentclientprotocol/sdk` 1.9.0 + v2 clarifications; resumed subagents restored | package.json | SKIP | none | none | - |
| R92-2 | MCP server + tool added to AIR tool calls (`_meta.jetbrains.air.mcp`) | `dist/tool-calls/*` | SKIP (AIR; Bash-only AIR subset ported) | none | none | - |
| R92-3 | Predicted follow-up prompt `session_info_update._meta.promptSuggestions` (AIR only); initialize `promptSuggestions:true` | `dist/prompt-suggestions.js` (36) | SKIP (AIR meta) / CLI (E-14: only the control-request field) | Rust leaves `prompt_suggestions` unset in the live agent | none | 2 |
| R92-4 | Subagent file edits in file-change report | `dist/file-change-audit.js` | SKIP | none | none | - |
| R93-1 | `/remote-control` command (adapter-handled via SDK `enableRemoteControl`), listed in `available_commands` | `S:555-600`, `S:3900-3980` | PORT-E (list entry) / SKIP (handler) | list = E-5; handler recorded pending | LOW | 2 / pend |
| R93-2 | Fork held back from session list until `session/fork` answered | `dist/session-index/live-registry.js` | SKIP | none | none | - |
| R94-1 | AIR `loadReplayEnd` after history replay | `dist/air-extension.js:70` | SKIP (replay, AIR) | none | none | - |
| R94-2 | OpenRouter model ids in gateway listing; SKILL.md resolved by declared name | `dist/gateway-models.js` | SKIP (models/skills) | none | none | - |
| R94-3 | `@agentclientprotocol/sdk` 2.0.0 | package.json | SKIP (npm dep; wire arbiter = fixtures) | none | none | - |
| R94-4 | Claude Code 2.1.303 / SDK 0.3.303 | package.json | CLI | section 3 | - | 2 |

### 2b. Findings not tied to one release (read from `dist/` / `docs/air-extensions.md`)

| # | Finding | Location | Class | Rust target / reason | Risk | Ph |
|---|---------|----------|-------|----------------------|------|----|
| X-1 | AIR gating: every AIR extension applies only when `initialize.clientCapabilities._meta.jetbrains.air` is an OBJECT (`isAirClient`, `dist/air-extension.js:110-112` — empty `{}` counts, no version gate at this level); a specific capability is honoured only when it is declared in a `capabilities` array AND `version>=1` (`clientSupportsAirCapability`, `dist/air-extension.js:133-143`); malformed (non-object) air -> none | `dist/air-extension.js:90-154` | PORT-N | new `C/air.rs` (`from_client_capabilities` = `isAirClient`, `supports(cap)` = `clientSupportsAirCapability`, `is_air()` = true for a constructed value), stored per session from `initialize` (`C/agent.rs:542-548` ignores params today) | LOW | 2 (parse + initialize gate), 3 (use) |
| X-2 | AIR `initialize._meta.jetbrains.air` = `{version:1, capabilities:[sessionFailure, agentFileChangeReport, nativeSubagentSessions, asyncTasks, customInstructions, recommendedValue, diffPatch, planFile] (8; the doc table omits customInstructions; + sessionIndex family only when declared) and AIR-only `sessionCapabilities.subagents:{}` (verified by running the 0.94.0 adapter with an AIR initialize), goal:{version:1,controlMethod:"_session/goal",actions:["set","clear"]}}`; list is independent of what the client declared (`dist/acp-agent.js:1458-1470`) | `docs/air-extensions.md` "Agent declaration" | PORT-N | `C/agent.rs:91-100`; advertises caps Rust does not implement (OQ-1) | LOW | 2 |
| X-3 | `sessionCapabilities` advertises `close, delete, fork, list, resume` but Rust answers `-32601` to those methods (pre-existing, hard-coded to fixture) | `C/agent.rs:75-103`, `:759` | SKIP | PARITY row (OQ-7) | none | 3 (docs) |
| X-4 | Client `systemPrompt` on `session/new|load` (`{mode:"append"|"override", content:[text..]}`) -> SDK initialize `systemPrompt`; invalid mode/non-text -> `invalid_params` before spawn | `dist/client-system-prompt.js` | PORT-E | `C/agent.rs:549-583`, `C/control.rs:116-143` `system_prompt` | LOW | 3 (E-3 advertises it from Ph2) |
| X-5 | Permission option set per tool: `allow-once "Yes"`, optional `allow-with-updates` (label from tool builder, e.g. Bash `Yes, and don't ask again for <cmds> commands`), `reject "No"`; order allow_once, allow_always, reject; `defaultToNo` reverses | `dist/permissions/options*.js`, `docs/air-extensions.md` "Options" | PORT-E | Ph2 = Yes/No only (corpus has `permission_suggestions:[]`); Bash durable option Ph3 deferrable | MED | 2 / 3 |
| X-6 | The Rust live agent registers NO hooks (InitializeOptions::default); the hooks in E-14 exist only in the `inv_11` unit test | `C/session.rs` (InitializeOptions::default()), `C/control.rs:670` | CLI | update test only | none | 2 |
| X-7 | Tool-call field tracker (every client): `tool_call_update` omits top-level fields unchanged since the previous report of the same tool call (`status,title,kind,content,locations,rawInput,rawOutput`); an update with nothing new and unchanged `_meta` is not sent; `_meta` kept whole for non-AIR. CORPUS-VISIBLE: `streamed-partial-input` frame 7 loses `kind`, frame 8 loses `kind,title,content` (still after the tool_result transcript fix) — red in `inv_24_tools` and `inv_24_full_corpus` | `dist/tool-calls/field-tracker.js` (167), cap094b evidence | PORT-E | `C/map.rs` consolidated/refine paths (plan 2.9) | LOW | 2 (non-deferrable) |
| X-9 | Non-AIR tool-call `_meta` shrank to `{claudeCode:{toolName}}`: `subagent:true`, Bash `title` (description) and `skill` moved to AIR-only `_meta.jetbrains.air.{subagent,commandTitle,skill}`. CORPUS-VISIBLE: `subagent-drain`/`subagent-followup` frame 6 `_meta.claudeCode.subagent: true -> absent`; Rust still emits all three | `dist/tool-calls/renderer.js:576-590`, `C/tools.rs:85-95`, cap094c evidence | PORT-E | `C/tools.rs:60-100` `claude_code_meta` (plan 2.10) | LOW | 2 (non-deferrable) |
| X-8 | Non-AIR extra upstream fields (unchanged from 0.70): first streamed `tool_call` has `rawInput:{}`, partial refinements carry partial input, `rawOutput` repeats raw `tool_result`, PostToolUse hook sends full `toolResponse` (Rust registers no hooks -> `toolResponse` never sent; recorded known gap `PARITY.md` phase-15 notes) | `docs/air-extensions.md` "Claude tools and ACP fields" | SKIP (hooks) | none | none | - |

## 3. SDK 0.3.232 -> 0.3.303: CLI-facing contract

Source: `scratchpad/sdk-diff-findings.md` (diff of `sdk.d.ts` / `sdk.mjs`; the `claude` binary is not in the package, so CLI-side behaviour behind new flags/env is inferred from `.d.ts` docs and from the empirical capture above)

| # | Item | 0.3.232 | 0.3.303 | Visible to fake-claude | Settlement risk | Class / Ph |
|---|------|---------|---------|------------------------|-----------------|------------|
| S-1 | argv style | mostly `--flag value` | `--flag=value`; value starting with `-` forces `=`; `--tools=` for empty | y (argv fixtures) | none | CLI, Ph2 (= E-13) |
| S-2 | new argv flags | - | `--permission-prompts=host\|none`, `--project-config-root`, `--await-initialize` (only when the matching SDK option is set; the adapter sets none -> absent from captured argv) | n (not in capture) | none | SKIP |
| S-3 | child env | `CLAUDE_CODE_ENTRYPOINT=sdk-ts`, `CLAUDE_AGENT_SDK_VERSION` | adds `CLAUDE_CODE_SDK_READS_SESSION_STATE=1` (tells the CLI the host consumes `session_state_changed`; SDK run-end now keys off idle) | y if allow-listed | HIGH (CLI idle semantics) | CLI, Ph2 (set it + extend `ARGV_ENV_ALLOWLIST`, OQ-6) |
| S-4 | `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS` | adapter sets `1` | adapter sets `1` (SDK does not) | y | HIGH if absent | CLI, already set `C/process.rs:450` |
| S-5 | initialize request | hooks, promptSuggestions, forwardSubagentText, systemPrompt, agents, skills, toolAliases | adds `sdkMcpServerConfigs/Manifests`, `systemPromptSnapshot`, `perTaskStopAffordance`, `rapidFollowupPreempt`, `forwardNestedSubagentToolCalls`, `workspaceTrust`, `plugins` (undefined fields dropped; adapter sets none of these) | n | low | SKIP |
| S-6 | hook event names | no `PreModelSwitch`/`PostModelSwitch` | added; adapter registers `PostModelSwitch`, `PostCompact` | y (initialize.json) | none | CLI, Ph2 (= E-14) |
| S-7 | `get_context_usage` | `{subtype}` | `+detail:'summary'|'full'` | y | none | CLI, Ph1 (fake-claude auto-ack) |
| S-8 | `interrupt` | `{still_queued:[..]}` receipt | receipt unchanged; new opt-in `cancel_queued:true` -> `cancelled[]` (adapter does NOT use it; it uses per-uuid `cancel_async_message` instead, commit d83f28a) | y | HIGH | CLI, Ph2 (= E-8) |
| S-9 | write ordering | user frames may precede initialize | SDK writes nothing before `initialize` is written (`writeAfterInitialize`) | y (order) | low | Rust awaits the initialize handshake in `Session::start` (`C/session.rs:274-282`) before any prompt — verify only |
| S-10 | new outbound subtypes | - | `get_hooks_listing, get_task_output, list_permission_rules, mcp_read_resource, reload_output_styles, update_settings, set_prompt_suggestions_paused, get_status, chrome_*, dialogs…` (host-initiated; adapter uses a subset) | n | none | SKIP |
| S-11 | inbound `control_request` | can_use_tool, hook_callback, mcp_message, elicitation, request_user_dialog, oauth_token_refresh, host_auth_token_refresh | + `remote_control_work_secret`, five `ui_*` (SDK leaves unanswered); `can_use_tool` gains `mcp_server{name,source}`, `default_to_no` | y if emitted | none | SKIP (all optional; Rust answers unhandled inbound subtypes with the default error response, `C/control.rs:425-445`, `C/session.rs:380`) |
| S-12 | `result` frames | `user_message_uuid` | + `user_message_uuids[]`, `result_index`, `queued_turn_count`, `resume_reason`, `local_command`, timings; error results + `startup_failure_reason` | y | HIGH-ish: extra result frames (startup failure results arrive with no turn) | PORT-E, Ph3/pend (R811-6, R72-2) |
| S-13 | `session_state_changed` | `{state: idle|running|requires_action}` | unchanged shape; runtime may add untyped `sdk_host_only:true` on idle (SDK hides such frames from its consumers; a raw host must still treat as a real idle) | y | HIGH | Rust already ignores unknown fields; add regression unit test Ph3 |
| S-14 | `system/init` | capabilities list | + `mcp_call_result_not_for_model_v1`, `mcp_read_resource_v1`, `mcp_tool_ui_meta_v1`, `plugin_errors`, `effort`, `view_mode`; `request_marker_lists_v1` read by adapter | y | none | SKIP |
| S-15 | background-task frames | `task_started/progress/notification/updated`, `background_tasks_changed` | + `run_id`, `is_backgrounded`, `spawn_depth`, `parent_task_id`, `subagent_type`, `ambient`, `reason:'worker_restart'` | y | low | PORT-N, Ph3 (async tasks read `is_backgrounded`, `task_type`, `tool_use_id`, `output_file`) |
| S-16 | new system subtype `session_metadata`; `conversation_reset` gains trigger | - | y | none | SKIP |
| S-17 | stream_event/assistant frames | - | + `user_message_uuid(s)`, `agent_id`, `usage_report` | y | low | SKIP |
| S-18 | framing, `keep_alive`, correlation | NDJSON, inbound `keep_alive` dropped | same (+ buffer of early unmatched responses, max 1024) | y | none | none (Rust matches: `C/control.rs:385-423` `route_inbound`) |

## 4. Counts (rows with a class)

- 144 rows with an id: SKIP 68 · PORT-E 35 · CLI 30 · PORT-N 6 · informational/none 5 (E-16, E-17, E-18, E-19, S-9..)
- HIGH RISK rows: E-8, E-12, R71-10, R72-2, R811-6, R851-4, R86-6, R86-8, R86-9, R87-1, R90-2, R91-3, S-3, S-4, S-8, S-12, S-13 (+ E-19 if its transcripts are made real)
- Summary block with the same numbers: `plan-rust-upstream-sync.md` -> "Inventory summary"
