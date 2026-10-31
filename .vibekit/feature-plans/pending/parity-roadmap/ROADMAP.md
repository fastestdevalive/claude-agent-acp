---
title: "parity-roadmap: every remaining gap between the Rust port and claude-agent-acp 0.94.0, as ordered work packages F1..F46"
feature: parity-roadmap
worktree: /Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1
base: 4a54919 (branch feat/rust-port; parity Phase 2 is UNCOMMITTED in the tree = "WT"; Rust citations are `git show HEAD:<path>`)
pin: porting/UPSTREAM_VERSION = 0.94.0 (tag v0.94.0); upstream TS cited as `file.ts:line` of the v0.94.0 export (`git archive v0.94.0`, scratch, outside the repo)
consumers: .vibekit/feature-plans/pending/parity-infra (MISSING.md ids = F-ids below), porting/parity/STATUS.md
---

<!--
RULES: bullets/tables only; one crisp line each; a package = at most 3 weak-model implementer turns, each verified by the orchestrator.
-->

# Roadmap: full parity with @agentclientprotocol/claude-agent-acp 0.94.0

> Standing goal: FULL parity and FULL feature set. Nothing is "deliberately not ported" because it is AIR/JetBrains-only or unused by vibe-station. Only technically impossible items may remain, each with a written reason (section 7).

## 0. Headline numbers

- Upstream source modules at v0.94.0: **91 files** under `src/` (excl. `src/tests/`), 58 test files / ~2330 test cases; `acp-agent.ts` (13804 lines) is split into **17 sections** (same split as parity-infra 1.6) = **107 module rows** in section 1.
- Rust port today (HEAD): 25 source files; crate covers the turn machine, spawn contract, permissions (Bash durable), async tasks, auth status, quota rows, session list/close/delete/fork(latest)/resume, systemPrompt, AIR tool `_meta`. Parity Phase 2 (usage_update, modes/configOptions, set_*, effort) is in the working tree.
- Status of the 107 rows (section 1): done 8 / partial 49 / missing 50 (count line at the end of section 1; sections S7, S11, S14 are in-flight P2 plus residue).
- Already planned (NOT duplicated here, referenced as A-*): **A-P2** (parity P2, implemented, uncommitted, in review), **A-P3** (parity-fixes Phase 3), **A-AG3** (air-gaps Phase 3), **A-INFRA** (parity-infra plan: TEST-MAP, MODULE-MAP, bump flow, gate wiring).
- New work: **62 packages** = F1..F46 with 11 of them split into letter sub-packages (F2a-c, F4a-b, F5a-b, F6a-b, F7a-b, F16a-c, F23a-c, F26a-b, F27a-b, F35a-c, F38a-b) plus the new **F3a** (SDK control layer; old F3 = F3b) (section 3), ordered in section 4.
- **Turn estimate (honest):** per package 2-3 turns as annotated; sum = ~160 implementer turns for F1..F44 (F1 3, F2 6, F3 4, F4 4, F5 4, F6 5, F7 5, F8-F15 24, F16 8, F17-F22 17, F23 7, F24-F25 6, F26 5, F27 6, F28-F34 21, F35 8, F36-F37 6, F38 5, F39-F40 6, F41-F44 10) + F45 TEST-MAP burn-down ~20 batch turns = **~180**; add ~30% for failed/retried turns and ~20 Opus review turns for the hazardous sequence => **~250 total turns**. Wall-clock assumptions: 1 turn = ~45 min including orchestrator verification; wave 1/2/6-9 run up to 2 packages in parallel (~10 turns/day), the hazardous sequence runs 1 package/day (review included) => ~6-7 weeks for the hazardous sequence alone, ~14-18 weeks total calendar time at one orchestrator.

### Already-planned items (A-*)

| id | plan | covers (exact) |
|----|------|----------------|
| A-P2 | rust-parity-fixes Phase 2 (WT) | `usage_update` x4 sites; `session/new\|load` modes+configOptions; `session/set_mode`, `session/set_config_option`; `config_option_update`/`current_mode_update`; effort option; `options.resume`, `settingSources`; resumed-session tail; unhandled-method error shape. Follow-up (a)(b)(c) done, (d) model-row resume ordering -> F12 |
| A-P3 | rust-parity-fixes Phase 3 (3.0-3.9) | 3.1 hooks registry (PostToolUse, PostModelSwitch, PostCompact no-op, TaskCreated/Completed) + `hook_callback` + initialize `hooks`/`promptSuggestions:true` + `_meta.claudeCode.toolResponse` + Edit/Write `structuredPatch` final diff; 3.3 `prompt_suggestion` frame consumed (publishing NOT planned -> F25); 3.4 Task* plan lane + `plan` update; 3.5 `session/load` replay (basic, `transcript.rs`); 3.6 permission options (`allow_always` Edit/Write/Read/WebFetch/Skill/MCP, `defaultToNo` order); 3.9 final live matrix |
| A-AG3 | rust-air-gaps Phase 3 | 3.1 goal (`_session/goal`, `active_goal`, `/goal`); 3.2 `sessionFailure` (turn-scoped, `TurnEvent::Failed.usage`, `auth_required` -32000); 3.3 file-change report (`rewind_files`, cancel-wins); 3.4 `/mcp` (`mcp_status/reconnect/toggle`, `local_command_output` minimal layer) |
| A-INFRA | parity-infra | `porting/parity/{TEST-MAP,MODULE-MAP,MISSING}.md`, `test-map-check`, `module-map-check`, `upstream-diff.sh` module hints + test diff, gate wiring |
| (done) | rust-steering-lanes, rust-upstream-sync, air-gaps P1/P2 | steering lanes, unfinished tools, folded prompt, async tasks, stop, systemPrompt, AIR tool meta, quota rows, authStatus, logout (partial), session close/resume/list/delete(non-AIR)/fork(latest) |

## 1. Upstream module -> Rust status

Legend: status `done | partial | missing`; pkg = F-id or A-id that closes the rest; `P2w` = in WT. Lines are upstream TS lines.

### 1a. `acp-agent.ts` sections (17, boundaries from parity-infra 1.6)

| # | section (lines) | status | what exactly is missing | pkg |
|---|-----------------|--------|-------------------------|-----|
| S1 | helpers, types, client wrappers, MCP OAuth bridge (1-2681) | partial | `isLateTaskRun`/`MAX_TASK_RUN_IDS` (444-480); `/usage` markdown replacement `structuredUsageMarkdown` (497-534); `parseRemoteControlCommand`/`claudeCodeOffersCommand` (580-609); MCP OAuth `startMcpAuthentication`..`waitForMcpAuthentication` (518-681, url elicitation `mcp-oauth-<uuid>`, 1 s poll, 10 min); `AcpClient` wrappers `createElicitation/completeElicitation/readTextFile/writeTextFile` + `$/cancel_request` on abort (2372-2517); `ChangedMetaClient` (2449); `computeSessionFingerprint`/`OPTION_REBUILDS_SESSION` (1595-1730); `emitRawSDKMessages` filter (1731-1773); gateway/provider types (1774-1845); `stripLocalCommandMetadata`/`isReplayHiddenMetaMessage`/`taskNotificationsOf` replay helpers (2087-2371); `claudeCliPath` SDK-bundled binary search (2036-2086); subagent-transcript capability gating (1911) | F14, F24, F3, F23, F19, F4, F32, A-P3, F6 |
| S2 | class ctor, `initialize`, `newSession`, fork/resume/load/list, index delegations (2682-3238) | partial | `authMethods` always `[]` (2883-3002: terminal login methods `claude-ai-login`/`console-login`/`claude-login` gated on `auth.terminal` or `_meta["terminal-auth"]`, gateway methods gated on `auth._meta.gateway`); session-index capabilities (`sessionIndex`,`sessionArchive`,`sessionRename`,`sessionListSubscribe`,`sessionWorktrees`, 3019); ACP `mcpServers` ignored (no `--mcp-config`, 3036); `providerUpdate` gate (3037); live-session fingerprint reuse/rebuild (resume/load of a live id spawns a 2nd child today); fork `forking()` hold-back/`onForkCreated`/fork point (3113); session-index `listSessions` (`hideArchived`, `_meta.jetbrains.air.list`, 3177); `subscribeSessionList/unsubscribe/rename/archive/unarchive/clientTitle` (3205-3225) | F2, F4, F19, F26, F27, F28, F29 |
| S3 | `authenticate`, auth-status machinery, providers, logout, local-command markdown (3239-3764) | partial | `authenticate` gateway/gateway-bedrock (3239, -32602 messages); per-prompt re-probe + `--settings` override + `apiKeyHelper` precedence (3263-3502); `authEpoch`; account-kind change -> sign-out respawn (3485); `providers/list|set|disable` (3503-3630); logout: clear gateway/provider override, `authEpoch++`, `contextWindowCache.clear()`, `enqueueProviderUpdate(undefined)` (3660); `localCommandMarkdown` `/usage` + `/mcp` dispatch (3723) | F32, F34, F24, A-AG3 3.4 |
| S4 | `prompt`/`startTurn`, subscription guard, `goal`, `steer`, `stopAsyncTask` (3765-4426) | partial | provider gate; per-prompt auth re-probe; `/remote-control` (3865-3992); `claudeSubscriptionGuard`/sign-out respawn/`recreateLiveSession` (3993-4184); `publishFallbackWarning`; `publishTaskPlan` (4240, A-P3 3.4); prompt-suggestion clear/publish (4220); `fileChangeReporter.request`; `titles.onPrompt`; `goal`/`publishGoal*` (4185-4313, A-AG3 3.1); steer on a guard-refused turn. `steer`/`stopAsyncTask` core done | F24, F34, F19, A-P3, A-AG3, F30 |
| S5 | `runConsumer` stream loop (4427-8334) | partial | see 1d (per-frame table) | F12-F18, F20, F25, F7, F15 |
| S6 | cancel, teardown, dispose, close, delete (8335-8935) | partial | `cancelTurns` flags `cancelledPrompts`, `localCommandAbort`, `exitPlan.cancel`, `contextCompaction.interrupt`, `fileChangeReporter.finish` (8437-8470); `stopBackgroundWorkflows` (8419); `cancelChildSession` (8396); `followupLive` held-turn gate (8630-8696); `orphanCommands` receipt lane (8554-8611, 8754-8787); `queryClosed`/`tornDown` flags, turn-completion wait with force-settle, `abortController`, `dispose` (8824-8930); AIR `deleteSession` = archive (8932) | F16, F18, F13, F36, F27 |
| S7 | `setSessionMode`, `setSessionConfigOption` (8936-9036) | P2w | `queryClosed` guard; boolean fast-mode `value`; `resolveFastModeEnabled`; `resolveModelPreference` fallback with `resolvedModel` | A-P2, F11 |
| S8 | `replaySessionHistory` (9037-9545) | missing | all (basic = A-P3 3.5); AIR extras: usage-limit failure restore (`SessionFailureController`), compaction summary -> `compaction_update`, `forwardSubagentText`, task-plan re-publish, subagent replay (AIR+RFD), `replayTerminalStates`, event-loop yield, `loadReplayEnd` `session_info_update` meta | A-P3 3.5, F31, F37 |
| S9 | fs passthrough, awaiting-user, foreground/autonomous cycle bookkeeping (9547-9789) | partial | `readTextFile/writeTextFile` agent->client helpers (9547-9555; upstream dead code, never invoked); `openUserInputs`/`withdrawnByCancel`/`blocksForeground`/`withChildAwaitingUser` (9562-9634; Rust: any pending permission => steer `later`); `startForeground`/`startAutonomousCycle`/`workEndReported`/`dropsStoppedWorkOutput`/`isStoppedWorkToolUse`/`toolUseWaiters` (9636-9788, issue #1061) | F3, F14, F16 |
| S10 | permission request, elicitation, `canUseTool`, hooks (9790-10440) | partial | `elicitFromClient` (9849); `ensureToolCallEmitted` extras (9868-9938: `recordDispatched/Foreground`, eager child session, SandboxNetworkAccess exemption, `previewContent` pin, rollback); `toolUseOwner` nativeSubagent id (9940); `fileChangePreviewHook` PreToolUse (9969, v2 only); `canUseTool` (10033-10268): "Session not found" deny, stopped-work deny "The user cancelled this prompt.", presentation (`title/displayName/description/decisionReason/blockedPath/defaultToNo`), `_meta.claudeCode.mcpServer`, per-tool options (A-P3 3.6), `matchedAskRule`/`suppressAlwaysAllowRule`, `applyPermissionFallback`, ExitPlanMode setMode/clear-context, "Tool use aborted" throw, `decodeClaudePermissionResponse`; AskUserQuestion form (10091-10113, 10309-10364); `handleMcpElicitation` (10270); `handleUserDialog` refusal fallback (10366) | F10, F9, A-P3 3.6, F23, F21, F22, F3 |
| S11 | config-option application, fast mode, context window refresh (10441-10858) | P2w+partial | `updateConfigOption` from ExitPlanMode/EnterPlanMode/external switch; `refreshContextWindowInBackground` `stillCurrent` guard + re-run after model change; `applyConfigOptionValue` agent branch (`applyFlagSettings{agent}`), fast-mode branch, `fastModeDisabledReason` reset, `appliedEffortLevel`, `useRecommendedValue`, `reconcileForModel`+`publishFallbackState`; `syncModelAfterExternalSwitch` (10724, via PostModelSwitch hook); fast mode `refreshFastModeOption`/`applyFastMode`/`syncFastModeState` (10759-10857) | A-P2, F11, A-P3 3.1 |
| S12 | `getOrCreateSession`, `createSession`, provider update (10859-11840) | partial | fingerprint reuse/rebuild (10859-10921); ACP `mcpServers` merge (11021-11046); `_meta.systemPrompt` string/preset (11048-11078); thinking from `MAX_THINKING_TOKENS`, `CLAUDE_MODEL_CONFIG` (11108-11114); elicitation-dependent `disallowedTools` (11116-11124); `disableBuiltInTools`; `forwardSubagentText` (10947); `taskState` (A-P3); file-change reporter (A-AG3 3.3); provider env / `settings` path-or-object / `apiKeyHelper:""` (11174-11245); `perTaskStopAffordance`, `promptSuggestions`, `enableFileCheckpointing`, `onElicitation/onUserDialog/supportedDialogKinds` options (11250-11330); hooks registration (A-P3 3.1) + PreToolUse (v2); `resourceNotFound` mapping (11431-11450); subscription guard (11452-11485); allowlist/overrides/denied/gateway models (11487-11541); agents picker (11577-11592); fast-mode/effort seed (11594-11661); session record fields; `enqueueProviderUpdate` recreate-all (11724-11841) | F4, F19, F5, F23, F34, F32, F33, F11, A-AG3, A-P3 |
| S13 | usage/quota/provider-env helpers (11841-12340) | partial | quota/model rows done (`quota.rs`). Missing: `autonomousCycleEnd`, `isAbortedResult` (`terminal_reason aborted_*`), `servesCancelledWork`, `blocksForeground`, `settlingOutcome` in `cancelledOutcome` (11886-12033); `gatewayRequestToProviderConfig`/`createEnvForProvider`/`isValidBaseUrl`/`sameHeaders`/`PROVIDER_ROUTING_ENV_VARS` (12214-12310) | F16, F14, F32 |
| S14 | `buildConfigOptions`, fast-mode/agent options, slash commands (12341-12669) | P2w+partial | `discoverCustomAgents` + `agent` option (12337-12354, 12496); fast-mode option builders (12355-12495); `getAvailableSlashCommands` AIR skill `_meta.jetbrains.air{kind:"skill",skillPath}`, `{hint:""}` rule, `/remote-control` only when supported (12560-12650) | F11, F24 |
| S15 | `promptToClaude`, message-id grouping, tool-call bookkeeping (12670-12945) | partial | `/mcp:server:cmd` -> `/server:cmd (MCP)` rewrite; `formatUriAsLink` `[@name](uri)` for file:///zed:// else bare uri; empty `data` -> `uri` fallback (12653-12760); `shouldEmitToolCall` guard, `recordToolCallParent`, `rememberResolvedToolNames` (12806-12945). `messageIdForGrouping`/`applyMessageId` done | F24, F6 |
| S16 | `toAcpNotifications`, `streamEventToAcpNotifications` (12946-13531) | partial | `parentToolUseId` stamping on nested messages; `toolUseResult`/`toolResultMeta` threading; Task*/TodoWrite cache + resolve; `alreadyEmitted && inputComplete===false` break; ExitPlanMode -> `plan_update`; error-result content for Edit/Write/Skill/ExitPlanMode; `terminal_*`; `shouldEmitToolCall` on refinements (13480); hook registration (A-P3) | F6, F7, F9, F15, A-P3 |
| S17 | `runPromptWithCancellation`, `v1AgentApp`, `runAcp`, context-window cache (13532-13804) | partial | `$/cancel_request` -> cancel (13532-13550); methods `authenticate`, `providers/*`, `_session/goal`, 5 session-index methods, v2 router; cross-session `contextWindowCache`, `providerCacheKeyFor`, `immediateContextWindow`, `cacheContextWindow` (13647-13755); `resolveThinkingConfig`, `parseModelConfig` (13750-13782) | F2, A-AG3, F32, F26-F28, F38, F5 |

### 1b. Other top-level modules

| module (LOC) | status | what exactly is missing | pkg |
|---|---|---|---|
| acp-subagents.ts (140) | missing | AIR `subagent_spawned`/`subagent_state_update` and RFD `subagent_update`/`session_message` builders | F35, F36 |
| air-extension.ts (175) | partial | `withAirMeta` ad hoc only; `AIR_CONTEXT_COMPACTION_KEY`, `AIR_PERMISSION_KEY`, legacy custom-answer key not emitted; session-index capability family not advertised; `loadReplayEnd`/`rawInputRendering`/`recommendedValue`/`diffPatch`/`planFile` not read from client (advertised-but-inert) | F2, F26, F9, F11, F31 |
| async-tasks.ts (1051) | partial | `taskBackgrounded` (173); `backgroundTasksChanged` replace-level + stop-closing (326); `toolResults` output-path recovery (456, cap 500); `isBackgroundedToolCall` (442); structured Bash result path `backgroundBashTaskFromToolResult`/`backgroundedBashToolCallIds`/`structuredBackgroundResults` (785-960); `routeOf` child routing; `toolNameOf` Monitor ignore by name; `notices`; `clear()`; `when_announced_state` defers running/paused (Rust drops); `merge_tool_result_path` no-op | F15, F14, F35 |
| auth-status.ts (358) | partial | per-prompt re-probe; `gatewayAuthStatus(baseUrl)` host detail; `authenticate`-driven status; `vendor` bag; `getApiKeyHelper` precedence managed > `--settings` > files (Rust: user/project/local only); epoch tied to hide-auth | F34, F32 |
| clear-context-coordinator.ts (164) | missing | `continuePlanInFreshContext` fresh-query hand-off (close query, restart under same public id with new private id, carry usage, move turn to fresh queue, `Implement the following plan:` push, held-turn variant #1167) | F22 |
| client-system-prompt.ts (74) | done | (`_meta.systemPrompt` string/preset extension -> F4) | F4 |
| context-compaction-meta.ts (30) / context-compaction.ts (427) | missing | `ContextCompactionLifecycle` (`compaction_update` or legacy "Compact conversation" tool call; in_progress/chunks/terminal/cancelled; heartbeat from `compaction_delta`; `consumeDuplicateErrorOutput`; synthetic assistant drop after delivery; interrupt/resume/reset; `pendingSummary` from PostCompact); AIR `contextCompaction` meta; WT has only a `compact_boundary` `tool_call` stub | F20 |
| diff.ts (748) | missing | jsdiff `structuredPatch`, `gitPatchText`, `quoteGitPath`, `readPatchSource`, memo, `previewPatchContent`, `patchUpdateFromDiffToolResponse`, `v2DiffContent`, non-AIR final diff, Write-update fallback text | F8, F9, F40 |
| elicitation.ts (476) | missing | `askUserQuestionsToCreateRequest`/`extractAskUserQuestions`/`applyAskElicitationResponse`, `mcpElicitationToCreateRequest`/`createElicitationResponseToElicitResult`, `refusalFallbackToCreateRequest`/`refusalFallbackResultFromResponse`, `ElicitationSupport`, AIR custom answer meta | F23 |
| exit-plan.ts (272) | missing | `acceptedPlanToolResult`, `observeExitPlanToolResults`, `isExitPlanInterruptionResult` (`[ede_diagnostic]`), `approvedPlan`/`planModeUpdate` (`plan_update`), `exitPlanModeRawOutput`, `ExitPlanCoordinator` interrupt path | F21, F22 |
| file-change-audit.ts (392) | missing | whole module (`agentFileChangeReportRequest`, `rewindFiles` dry run, path normalisation, report `session_info_update`, cancel-wins, subagent edits) | A-AG3 3.3 |
| fork-session.ts (239) | partial | fork point `_meta.jetbrains.air.fork{messageId,messageFingerprint,messageOccurrence}`, `:segment:N` strip, live `messageIdToUuid`, `assistantGroups`+sha256 fingerprint+occurrence, `upToMessageId`; Rust answers -32602 for any fork point | F29 |
| gateway-models.ts (223) | missing | `listGatewayModels`, `listBedrockModels`, region prefix, `GatewayListingError`, 10 s timeout, 20-page cap | F33 |
| goal-extension.ts (83) | missing | `_session/goal`, `parseGoalRequest` -32602 messages, `goalUpdateFromPrompt`, `toGoalSnapshot` (advertised-but-inert) | A-AG3 3.1 |
| hide-claude-auth.ts (287) | missing | `--hide-claude-auth`, `billsClaudeSubscription`/`holdsNonSubscriptionCredential`, `refuseClaudeSubscriptionTurn`, `authRequired` data `{reason:"claude_subscription_not_supported"}`, sign-out respawn | F34, F2 |
| index.ts (108) | partial | `--cli` passthrough (spawn bundled CLI, forward SIGINT/SIGTERM/SIGHUP, re-raise); `CLAUDE_AGENT_LOGS` file logger; SIGINT (Rust handles SIGTERM only); `applyManagedPolicyEnv`; `CLAUDE_AGENT_ACP_EXPERIMENTAL_V2=1` | F2, F38 |
| lib.ts (32) | done (n/a) | Node library exports; analogue = `lib.rs` | - |
| managed-policy.ts (34) | missing | managed tier `env` applied to child env; macOS MDM + Windows registry tiers | F2, F5 |
| mcp-command.ts (392) | missing | `/mcp` status/reconnect/enable/disable + markdown, `cleanMcpError` | A-AG3 3.4 |
| native-subagents.ts (1602) | missing | child session registry, `route`/`routeOfToolCall`, buffering (64/256/32), generations, `finishTask`, `parentCancelled`, `cancelChild`, `awaitingUser`, restore, AIR + RFD forms | F35, F36, F37 |
| paths.ts (7) / utils.ts (180) | done (n/a) | replaced by `session_store.rs` / tokio channels | - |
| prompt-suggestions.ts (44) | missing | publish `session_info_update._meta.promptSuggestions` (AIR), armed after user result, clear on prompt, dedupe (frame consumption = A-P3 3.3) | F24 |
| resumed-session.ts (243) | partial | WT tail scan (P2w); `readResumedSession` messages (A-P3), `resumedModelFromTranscript` non-tail path, `withModel` fallback | A-P2, A-P3 |
| serve.ts (51) | partial | v1 serve done; `acpProtocolRouter` v2 | F38 |
| session-config-ids.ts (4) | P2w | constants unnamed | A-P2 |
| session-effort.ts (130) | partial | `mergeEffortSettings` programmatic tier; `useRecommendedValue` inert (callers pass `false`) | F11 |
| session-failure-extension.ts (486) | missing | policy table (13 kinds), `SessionFailureController`, ids, revisions, `providerFailureCategory`, `isSyntheticUsageLimitMessage`; session-wide pushes (worker_shutdown, transport_lost, advisory) | A-AG3 3.2, F25 |
| session-mode.ts (405) | partial | notice vs transcript fallback line; `autoModeFallbackWarningPending`; `prePlanMode`; `applyPermissionFallback`; `reconcileForModel` after model switch | A-P2, F10, F11 |
| session-model.ts (608) | partial | `isDeniedModel`/`normalizeModelId`; `applyAvailableModelsAllowlist` (`availableModels`, `modelOverrides`, `ANTHROPIC_CUSTOM_MODEL_OPTION`); `narrowToGatewayModels`; `CLAUDE_MODEL_CONFIG`; `PreModelSwitch` veto (Rust: set_model failure fatal; Node advisory); `modelBeforeSwitch` readback; `recommendedValue` option | F5, F11 |
| session-notices.ts (96) | missing | `clientSupportsNotices`, `sessionUpdate:"notice"`, transcript fallback, 256-char title split | F2 |
| session-timing.ts (27) | missing | `[session/<op>] ... durationMs totalMs` log lines (Rust has no logger) | F2 |
| session-titles.ts (354) | missing | `SessionTitles` context ring, `generateSessionTitle`, `session_info_update{title}`, `setExplicitTitle`, reset on clear-context | F30 |
| settings.ts (242) | partial | `fs.watch` 4 files, 100 ms debounce, `managedDeniedModels`, `managedApiKeyHelper`, `getApiKeyHelper(override)`, `setCwd`; (merge + `filterEscalatingDefaultMode` done) | F5 |
| subagent-history.ts (323) / subagent-replay.ts (346) | missing | `<session>/subagents/agent-<id>.jsonl` + `.meta.json` reader, lineage, `RfdSubagentReplay` | F37 |
| tool-result-meta.ts (32) | missing | `parseToolResultMeta`, `isAbortedToolResult`, `_meta.claudeCode.{nonExecutionKind,userFeedback}` | F7 |
| tools.ts (485) | partial | Task* state parsers/`taskStateToPlanEntries`/`changedTaskPlanEntries`/`createTaskHook` (A-P3 3.4); hook registry/PostToolUse (A-P3 3.1); shapes (F6) | A-P3, F6 |
| turn-events.ts (100) | partial | `TurnOutcome` covered by `Settled`; `ForegroundEvents` (running/awaitingUser/ended/failed) missing (v2 only) | F39 |
| usage-markdown.ts (215) | missing | `formatUsageResponse`, `get_usage` (5 s) | F24 |

### 1c. `permissions/`, `tool-calls/`, `session-index/`, `v2/`

| module (LOC) | status | what exactly is missing | pkg |
|---|---|---|---|
| permissions/modes.ts (60) | done | - | - |
| permissions/normalization.ts (130) | done | - | - |
| permissions/options.ts (100) | partial | Rust builds Yes / durable-Bash / No (`permission.rs:92-103`); missing the per-tool switch and decline-first order (`defaultToNo`) | A-P3 3.6 |
| permissions/options/shell.ts (121) | partial | read-path / directory suggestions; only plain command rules | A-P3 3.6 |
| permissions/options/filesystem.ts (186), options/tools.ts (178), options/shared.ts (110) | missing/partial | Edit/Write/Read/Glob/Grep/NotebookEdit session `allow_always`; WebFetch domain; Skill exact/prefix; SandboxNetworkAccess host; MCP/computer-use; generated fallback; ids `allow-skill-exact`,`allow-skill-prefix`,`exit-plan-*` | A-P3 3.6, F21 |
| permissions/effects.ts (223) / response.ts (36) | partial | `parseClaudePermissionSelection` throws "Tool use aborted"; "Permission option was not offered"; Skill/WebFetch/generated rules; ExitPlanMode `setMode` + `interrupt:true` deny + `contextResetMode` | F10, F21 |
| permissions/presentation.ts (137) | missing | AIR `_meta.jetbrains.air.permission{title,description,defaultToNo}`; AIR minimal toolCall; `Reason:`; `blockedPath`; SandboxNetworkAccess/computer-use/ExitPlanMode titles; JSON fallback content; `previewContent` | F10, F9 |
| tool-calls/background.ts (74) | missing | `backgroundedBashToolCall`, `withRunningTerminal` | F15 |
| tool-calls/changed-meta-filter.ts (108) | done | - | - |
| tool-calls/client-capabilities.ts (87) | partial | `terminal_output`, `terminal_output_delta`, `diffPatch`, `rawInputRendering`, `planFile`, `recommendedValue`, boolean config, v2 flag, `session.notices`, `session.compaction`, `subagents`, `subagent-transcript`, `elicitation.form/url` not parsed | F2 |
| tool-calls/content.ts (294) | partial | ~18 of 20 block types (document, tool_reference, web_search/fetch, code-execution, `[image: url]`); `markdownEscape` leading-whitespace; `toolResponseMarkers` | F6 |
| tool-calls/facts.ts (129) | partial | exit-code/facts extraction | F6 |
| tool-calls/field-tracker.ts (182) | partial | `mergedMeta` comparison (a `_meta`-only change is wrongly dropped); `pinContent`/`replacePinnedContent`; `finishResult`/`finishHook` (Rust clears at settle) | F9, A-P3 3.1 |
| tool-calls/renderer.ts (818) | partial | AIR terminal channel; `planFile`; `rawInputRendering`; `fileTextKeys` stripping (534-548); `diffPatch`; permission toolCall shape; `contextCompaction`; `progress`/`memoryRecall`/`permissionDenied`/`unfinished({terminalExit})`; `hookResult`; `exit_code` | F6, F7, F9, F15, F20 |
| tool-calls/reporters/agent.ts (226) | missing | `(N effort)` title suffix (not fork); structured `AgentOutput`; `<usage>`/`agentId:` trailer strip; hand-back unwrap | F6 |
| reporters/bash.ts (133) | partial | structured stdout/stderr, abort/persisted-output notes, backgroundTaskId, `bash_code_execution_result`, `command_description`, `exit_code`, image output | F6, F15 |
| reporters/compaction.ts (85) | missing | "Compact conversation" lifecycle + meta | F20 |
| reporters/file-edit.ts (220) | partial | Write `path`/`file_text`/`file_content` aliases (R811-9); Edit `oldText:null` for empty; `fileTextKeys`; NotebookEdit AIR; `hookResult`/`finalChange`/`createdFileDiff` | F6, F8, F9 |
| reporters/index.ts (52) | partial | PowerShell + agent-control tools (SendMessage, TaskStop, ListAgents, Monitor) have no reporter | F6 |
| reporters/interaction.ts (490) | partial | ExitPlanMode title "Approve Plan" + result "Exited Plan Mode"; AskUserQuestion AIR fixed title; Generic `""` -> "Unknown Tool"; AgentControl AIR unfenced error | F6, F21 |
| reporters/read.ts (84) / search.ts (67) / web.ts (56) | partial | Read ` (from line N)`, `limit==0`, structured numbered rebuild/banner; Grep `-l`/`-c`/`file_path`/truthy flags; WebFetch ` (from char N)`; WebSearch `Search "q"` title + structured hits | F6, F7 |
| session-index/ (14 files, 5560 LOC): activity, archive-title, connection, dir-listing, explicit-title, first-prompt, list-subscriptions, live-registry, project-dirs, service, session-index, session-info, transcript-scan, worktrees | missing (project-dirs/first-prompt/transcript-scan partial in `session_store.rs`) | `session/list` `_meta.jetbrains.air.list{limit<=200,includeWorktrees,archived}`, cursor v5, rows `_meta.jetbrains.air{archived,lastPromptAt,model,forkedFrom,agentWorktree,state,lastTurnEndedAt,cost}`; `_session/rename`, `_session/archive`, `_session/unarchive`; `_session/list/subscribe\|unsubscribe` + `_session/list/changes` (128 cap, 1 row/s, 150 ms debounce/1 s wait, 10 s rescan); live registry, `ForkHolds`, sidecar `custom-title.json` (0600/0700), `rename_session` control; real delete (-32002 unknown) vs archive-instead-of-delete (AIR) | F26, F27, F28, F29 |
| v2/agent.ts (406), initialize.ts (129), permission.ts (50), prompt.ts (183), session-update.ts (149), session.ts (182), terminal.ts (130), tool-call-meta.ts (130), tool-call.ts (68) | missing | the whole experimental ACP v2 surface (`CLAUDE_AGENT_ACP_EXPERIMENTAL_V2=1`): protocol router, v2 initialize, `configId`/`groupId`/`replayFrom`, `state_update`, prompt returns at echo with `{messageId}`, `messageId` on chunks, `plan_update`, `changes` diff content, terminal updates, permission `subject` | F38, F39, F40 |

### 1d. `runConsumer` (acp-agent.ts 4427-8334) per-frame/per-lane status

| group (lines) | status | missing | pkg |
|---|---|---|---|
| `sendUpdate` chokepoint, child routing, titles hook, notices (4408-4640) | partial | `subagents.route`, `eagerToolCallSessions`, `titles.onAssistantText`, `compaction.consumeDuplicateErrorOutput`, `clientSupportsNotices` | F35, F30, F20, F2 |
| session failure plumbing, `lastTurnFailed` (4666-4721, 5554-5596) | missing | AIR `end_turn`+meta; kinds `worker_shutdown`,`transport_lost`,`advisory`; `clearFailuresFromEarlierTurns` | A-AG3 3.2, F25 |
| `resetTurnScratch`/`activateTurn` (4764-4841) | partial | endedPerLevel two-phase sweep (4821-4832); `orphanCommands.clear`; `cancelledPrompts` reset; `carriedUsage`; `compaction.resume` | F14, F13, F22 |
| `ensureActiveTurn` (4876-4987) | partial | per-uuid `orphanCommands` lane (4930-4973); exact join on `user_message_uuid(s)` (R72-2); `cancelledPrompts` guard (4981) | F13, F12 |
| command-result bookkeeping (5001-5061) | partial | `commandResultSeen` latch, orphan-map drain | F13 |
| live-task registry (444-480, 5093-5215) | partial | record fields `backgrounded/parentTaskId/taskType/endedPerLevel`; `resumableSubagents`+SendMessage resume; `run_id` late-frame drop; non-subagent tasks in registry | F14 |
| tool-call closing (5222-5405) | missing | `closeToolCall`, `closeOpenToolCalls`, `closeCancelledToolCalls`, `closeStoppedSubagentToolCalls`, `sendInterruptedToolResults`, `closeCycleToolCalls`, `terminalExit`, `recordReachedToolUse` | F17 |
| `settleActive` (5410-5519) | partial | `settling`/`settlingOutcome`; `closeCancelledToolCalls`; report; `localCommandAbort`; `wasHeld` EAT clear | F16, F17, A-AG3 |
| `failActive`/`failAllTurns` (5523-5634) | partial | EAT clear on fail; `cancelledOutcome` for cancelled active turn; `endAutonomousCycle` | F12 |
| abort/force-cancel lane (5679-5742) | partial | orphan seeding by command state (5697-5721); `compaction.interrupt`; EAT clear 5730 | F13, F12 |
| EOF/`done` (5749-5817) | partial | `pendingWorkerShutdown`; scratch `stopReason` (5796, EOF mid-steer); `closeQueryStream` | F12, F25, F18 |
| `command_lifecycle` (5839-5933) | missing | whole `msg_lifecycle_v1` map (queued/started/completed/cancelled/discarded/refused; zombie/pending/started); init capability latch | F13 |
| `active_goal` (5938) | missing | publishRuntimeGoal | A-AG3 3.1 |
| `system/init` (5949-6000) | missing | capability latches (`msg_lifecycle_v1`,`request_marker_lists_v1`); `syncFastModeState`; `terminal_slash_commands` re-advertise; `plugin_errors` log | F13, F11, F24 |
| `system/status`, `compact_boundary` (6002-6068) | partial | compaction lifecycle R75-2; `status:requesting` un-cancel (6010-6020); `modelRequestSinceResult` | F20, F16 |
| `local_command_output` (6070-6087) | missing | -> `agent_message_chunk{messageId}`; `/usage`,`/mcp` replacement | F7, F24, A-AG3 3.4 |
| `session_state_changed` (6088-6258) | partial | running-transition owed sweep (6094-6115); `sdk_host_only` drop (S-13: the SDK drops these frames, Rust over-reacts); steered-ready before owed debt (6188); held-cancel debt decrement (6159); `closeCycleToolCalls`/`endAutonomousCycle` at idle; `titles.onTurnEnd` | F12, F17, F30 |
| `memory_recall`, `commands_changed`, `mirror_error`, hooks, `files_persisted` (6259-6411) | partial | `memory_recall` update; `commands_changed` uses `skillPaths`/`terminal_slash_commands`/AIR cwd; `mirror_error` log | F7, F24 |
| `permission_denied` (6299-6353) | missing | `tool_call_update` failed via renderer; `unregisterHookCallback` | F7 |
| `informational` (6354-6406) | missing | `_meta.claudeCode{kind:"informational",level}`, `**Warning:** ` prefix, `noticeTexts` | F7 |
| `task_progress/started/notification/updated` (6412-6575) | partial | `isLateTaskRun`, `subagents.*`, `endBackgroundTerminal`, `closeStoppedSubagentToolCalls`, `is_backgrounded` patch, `resumeLiveTask` | F14, F15, F35 |
| `worker_shutting_down`, `api_retry`, `model_refusal_*`, `elicitation_complete`, `control_request_progress` (6576-6732) | missing | unmapped; `completeElicitation`; `syncModelAfterExternalSwitch`; `lastRefusalExplanation` | F7, F25, F23 |
| `background_tasks_changed` (6733-6777) | missing | `backgroundTasksChanged` + `endedPerLevel` marking | F14, F15 |
| `result` preamble (6783-6870) | partial | `promptSuggestionExpected`; goal rollback; `syncFastModeState`; steer retire by `user_message_uuids` (R811-6) | F24, A-AG3, F11, F12 |
| `result` debt/usage (6872-7024) | partial | `owesTrailingIdle` for cancelled (6918); `cacheContextWindow`; `lastTotalCostUsd` | F12, F5 |
| `result` cycle end / cancelled guard / autonomous lane (7026-7166) | missing/partial | `autonomousCycleEnd`; result-time cancelled settle (R90-2); `terminal_reason aborted_*`; `num_turns===0` placeholder hold (7150); exit-plan interruption (7037-7139) | F16, F21, F22 |
| `result` diagnostics/refusal/steer/subtype (7177-7442) | partial | empty-interruption diagnostic swallow (R72-2); refusal explanation chunk; `lastTurnFailed`; `deliveredCompactionOutput`; `takeLocalCommandMarkdown` | F13, F7, F20, F24 |
| `result` settle/finally (7443-7490) | partial | `compaction.reset` | F20 |
| `stream_event` (7493-7696) | partial | `dropsStoppedWorkOutput`+`droppedStreams`; `recordReachedToolUse`; compaction heartbeat; AIR nested-text gate; `forwardsSubagentText`; stopped-work window gates only the consolidated message in Rust | F16, F20, F6 |
| `user`/`assistant` (7697-8124) | partial | `messageIdToUuid`; `pendingGoalUpdate.started`; cancelled guard (7818-7831); `<local-command-stdout>` strip; `<synthetic>` handling (usage-limit, login -> `auth_required`); `stop_details` refusal; `backgroundBashTask*`/`toolResults`; `resumedNativeSubagentId`; `recordSubagentFileEdits`; `observeExitPlanToolResults`; `withRunningTerminal` | F29, A-AG3, F16, F7, F15, F35, F21 |
| `tool_progress` (8125-8176) | missing | heartbeat ids -> `parent_tool_use_id`; `renderer.progress` | F7 |
| `rate_limit_event`, `conversation_reset`, `tool_use_summary`, `prompt_suggestion`, `auth_status` (8177-8232) | partial | reset: `finishLifecycle`,`taskState.clear`,plan republish,`titles.reset`; suggestion publish; `auth_status` -> `sessionFailures.clear` | F7, F24, A-P3 |
| catch path (8240-8318) | missing | `processDied` match; `transport_lost`; settle-cancelled on error; eviction message | F25 |

### 1e. Count line

- 91 files = 107 rows (90 other files + 17 `acp-agent.ts` sections). Other 90 files: **done 8** (permissions/modes, permissions/normalization, changed-meta-filter, client-system-prompt*, paths, utils, lib, session-config-ids[P2w]) / **partial 33** / **missing 49** (23 top-level, 3 permissions, 3 tool-calls, 11 session-index, 9 v2). `acp-agent.ts` sections: **partial 13** (S1-S6, S9, S10, S12, S13, S15-S17), **P2w + partial rest 3** (S7, S11, S14), **missing 1** (S8). (`*` = `_meta.systemPrompt` extension left to F4.)
- Package count and turn estimate: see section 0 (62 packages, ~165 implementer turns excluding burn-down).

## 2. Hazard glossary (turn.rs / session settle path)

- **O** orphan-before-head: `pending_orphan_results` credit consumed BEFORE queue-head promotion (`ensure_active_turn` early return, INV-30, review-05 #1).
- **I** `owed_trailing_idles` debt accounting (INV-31 / #825).
- **E** `ensure_active_turn` conditional early return when a turn is already active.
- **H** `had_usage` origin flag on `TurnEvent::Settled` (only the queued-sweep cancel is `false`).
- **S** steering lanes (`mark_steered`, `steered_echoes`/`steered_settle`, held turns, INV-23 `reconcile_orphan_receipt` touches only `orphaned_uuids`).
- **EAT** `emitted_assistant_text` actor flag (INV-29 result-text fallback).
- Rule for every hazardous package: `cargo test -p claude-agent-acp-rs --lib turn` before AND after; `turn_tests.rs` only gets added lines or mechanical field additions; a divergence visible in a Node-captured fixture MUST be ported, never ignored (no new `JsonPath`); STOP + report when an existing `inv_16|23|29|30|31|steer_*` test needs a non-mechanical edit.

## 3. Work packages F1..F46 (+ sub-packages and F3a)

Format: scope / upstream refs / Rust files / acceptance / deps / risk / evidence. "corpus" = Node-captured differential scenario (`bash porting/capture.sh <name>` + `inv_24_<name>`). Weak-model rule inside every package: pure module + unit tests first, then wiring, then corpus+capture+differential together.

Conventions (revision 2, after Opus review):
- **Sizing:** every package is <= 3 weak-model turns. The 11 packages that were really 2-3 packages (F2, F4, F5, F6, F7, F16, F23, F26, F27, F35, F38) carry a "Split" bullet with letter suffixes (`F2a`, `F2b`, ...); each letter is its own package with its own verification. The new SDK-control-layer package is **F3a** (the old F3 is now F3b in the split table). Total: **62 packages**, turn estimate in section 0.
- **Evidence (supersedes every `evidence: y` below):** NO package is blocked on a real-`claude` recording. Frames are AUTHORED into fake-claude transcripts from the cited upstream test file:line (section 6) and the outbound subtypes are recorded by Node's own SDK during `capture.sh`. Where a package line still says `evidence: y`, read it as "authored transcript from the section-6 source; real recording optional-later". `record-real.sh` stays human-only.
- **Hazard protocol:** packages in the strict sequence of section 4 step 4 (now including F7a/F7b, F24, F25) get `--lib turn` before and after, one Opus review each, and never run in parallel with another package that edits `session.rs`/`turn.rs`/`map.rs`.

### Wave 1 - foundations (no turn.rs)

#### F1 - Evidence kit and recorder/fake extensions
- scope: (no real-CLI dependency) extend `acp-recorder` `Step::Initialize.client_capabilities` (whole object, additive), multi-spawn transcripts in `fake-claude` (second process after a restart), `$REQ.n` helpers, elicitation reply step (`elicit_reply`), `FAKE_CLAUDE` env allowlist; extract control-subtype names + payloads for `mcp_authenticate`, `remote_control`, `rename_session`, `generate_session_title {description, persist}` (the JS name `generateSessionTitle` is the Query method; the wire subtype is `generate_session_title`), `get_usage`, `request_user_dialog`, `elicitation` (inbound payload fields are spelled out in `sdk.mjs` `processControlRequest`) from `sdk.mjs` (`rust/target/upstream/0.94.0/node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs`) into `porting/evidence/CONTROL-SUBTYPES.md`.
- upstream: sdk.mjs (minified), `acp-agent.ts:536-575` (Query type extensions), `session-titles.ts:32-82`.
- rust: `rust/acp-recorder/src/{script,recorder}.rs`, `rust/fake-claude/src/{lib,main}.rs`, `porting/record-real.sh`, `porting/capture.sh`.
- accept: fake-claude/recorder unit tests; recorder can drive an elicitation-capable client against Node (`capture.sh` smoke).
- deps: none. risk: low. evidence: n (optional-later: human-run `record-real.sh` for corpus realism only).

#### F2 - Process entry, `initialize` completeness, client capabilities, notices, logger
- **Split:** F2a (2 turns) = `client_caps.rs` full capability parse + notices + `initialize.authMethods` (terminal/gateway, `_meta.terminal-auth`) + corpus `initialize-capabilities` variants; F2b (2 turns) = `index.ts` parity (`--cli` passthrough + signal forwarding, SIGINT, `CLAUDE_AGENT_LOGS` logger + session-timing, `--hide-claude-auth` parse, managed-policy env, shutdown/`dispose` API); F2c (2 turns) = transport wrapper: `$/cancel_request` -> cancel, `session/request_permission` id as a number, malformed-JSON-line error shape equal to Node's ndJsonStream (wrap the transport; no exception, see section 7).
- scope: `--cli` passthrough + signal forwarding; SIGINT; `CLAUDE_AGENT_LOGS` file logger + session-timing lines; `--hide-claude-auth` argv parse (state in F34); managed-policy `env` into child env; `initialize.authMethods` (terminal methods incl. `NO_BROWSER/SSH_*/CLAUDE_CODE_REMOTE` branch, `_meta.terminal-auth` with normalised execPath/argv, gateway methods); full client-capability parse (`client_caps.rs`: terminal_output(_delta), diffPatch, rawInputRendering, planFile, recommendedValue, boolean config, `session.notices`, `session.compaction`, `subagents`, `subagent-transcript`, `elicitation.form/url`, `loadReplayEnd`, auth.terminal, `auth._meta.gateway`); notices (`sessionUpdate:"notice"`, replace the hard-coded bold line at `session.rs` auto-fallback sites); `$/cancel_request` -> cancel (check crate API); investigate the `session/request_permission` id type (number in Node, crate-assigned string in Rust) and malformed-JSON-line error shape vs Node's ndJsonStream; `dispose`/shutdown API; `AIR_CAPABILITIES` session-index family (advertised only when F26 lands).
- upstream: `index.ts:10-108`, `acp-agent.ts:2829-3034`, `tool-calls/client-capabilities.ts`, `session-notices.ts`, `session-timing.ts`, `managed-policy.ts`, `13532-13550`.
- rust: `bin/claude-agent-acp-rs.rs`, `process.rs` (cfg sites only here, G9), `agent.rs` (`initialize_response`), `air.rs`, new `client_caps.rs`, `notices.rs`, `logger.rs`, `settings.rs`.
- accept: corpus `initialize-capabilities` variants (plain / `auth.terminal` / `_meta.terminal-auth` / `auth._meta.gateway` / `NO_BROWSER=1` / notices client / AIR) compared to Node (execPath/args normalised); `tests/shutdown.rs` SIGINT + `--cli` with a fake `claude`; units for each capability getter.
- deps: F1 (recorder client caps). risk: low-med (touches `initialize_response`, which every fixture compares). evidence: n.

#### F3a - SDK control layer (`sdk.mjs` in-process layer the Node adapter sits on) - 2 turns, EARLY
- scope: port exactly what Node's SDK does between the `claude` stdout and the adapter's callbacks, in `control.rs` (`route_inbound`, `spawn_inbound_handler`, `respond_error`, HEAD `control.rs:332-345`): (a) a `request_user_dialog` with NO `onUserDialog` handler (= every client without `elicitation.form`) gets **no response at all** (sdk.mjs `processControlRequest`: "No onUserDialog handler ... staying silent", returns the `suppressControlResponse` symbol `Pc`); the same for the subtypes `remote_tool_call`, `remote_plumbing_call`, `remote_tools_probe`, `remote_tools_reannounce` (`Z4`, "leaving it unanswered") - so those are never answered even WITH handlers; (b) an `elicitation` with no handler gets a SUCCESS response `{action:"decline"}`; (c) every SDK error response carries an `error` string (e.g. `"Unsupported control request subtype: X"`; Rust sends `{subtype:"error"}` with NO `error` field today); (d) skip a duplicate delivery of an in-flight `request_id` (`cancelControllers.has`); (e) skip requests that arrive after close (`cleanupPerformed`); (f) a child-exit error becomes `Error("Claude Code returned an error result: <text>")` (sdk.mjs `readMessages`, `lastErrorResultText`) - used by `session_error_to_rpc`; also model the response-writing rule "handler returns None => write nothing" as the general `Option<response>` handler contract (the 0.94.0 adapter callbacks never return null, but the SDK path (a) is reachable TODAY by every client without form elicitation).
- upstream: sdk.mjs `processControlRequest` / `handleControlRequest` / `Z4` (`rust/target/upstream/0.94.0/node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs`), `acp-agent.ts:10366-10403` (adapter side of dialogs), `acp-agent.test.ts:753` (`onUserDialog` unset).
- rust: `control.rs`, `session.rs` (`build_permission_request_handler`), `session_error_to_rpc` in `agent.rs`.
- accept: fake-claude transcript steps that EMIT those control_requests and `expect` NO `control_response` (a bounded wait then the next frame) / `{action:"decline"}` / an `error` string; Node-captured fixtures `sdkctl-dialog-silent`, `sdkctl-z4-silent`, `sdkctl-elicitation-decline`, `sdkctl-unknown-subtype-error`, `sdkctl-duplicate-request`; units for (d)(e)(f).
- deps: F1 (fake-claude "expect nothing" step). risk: low-med (touches the inbound handler shared by `can_use_tool`; INV-9/INV-10 stay green). evidence: n.

#### F3b - Control-plane plumbing (client requests, declining handlers, user-input accounting) - 2 turns (was F3)
- scope: outbound agent->client request channel (`elicitation/create`, `elicitation/complete` notification, `fs/read_text_file`/`fs/write_text_file` thin helpers - upstream dead code, kept for surface parity), abort race + `$/cancel_request` to the client for an aborted request; (the inbound decline/silence machinery is F3a; F3b only consumes it); `openUserInputs` set (permissions + elicitations + dialogs) replacing the "any pending permission" test in the Steer arm; `blocksForeground` (subagent requests do not defer a foreground steer, R86-6); cancel replies per request kind.
- upstream: `acp-agent.ts:2372-2517, 9547-9634, 9790-9858, 11960-11985, 4298`.
- rust: `agent.rs` (`SessionOutbound`), `session.rs` (`pending_permissions`, Steer arm), `control.rs` (inbound spawn handler), `session_ext.rs`.
- accept: units (`inv_NN_open_user_inputs_*`, steer `later` only for foreground-blocking requests); corpus `steer-while-subagent-permission` (steer priority) after F14; `tests/agent.rs` for client request round trip.
- deps: F2. risk: med (S: steering `later` decision). evidence: n.

#### F4 - Spawn options and session creation inputs
- **Split:** F4a (2 turns) = ACP `mcpServers` -> `--mcp-config`, `options.{mcpServers,settings,plugins,sandbox,thinking,persistSession,strictMcpConfig}`, `CLAUDE_AGENT_SDK_VERSION`, thinking/`CLAUDE_MODEL_CONFIG` env, argv fixtures; F4b (2 turns) = `_meta.systemPrompt`, `disableBuiltInTools`, `emitRawSDKMessages`+`_claude/sdkMessage`, `forwardSubagentText`, initialize fields (`promptSuggestions`,`perTaskStopAffordance`,`enableFileCheckpointing`), `claudeCliPath` emulation, `resourceNotFound` mapping.
- scope: ACP `mcpServers` (stdio/http/sse, header and env lists) -> `--mcp-config`; `_meta.claudeCode.options.{mcpServers,settings(path|object),plugins,sandbox,thinking,persistSession,strictMcpConfig,hooks(JSON-only keys)}`; `CLAUDE_AGENT_SDK_VERSION` env; `MAX_THINKING_TOKENS` -> thinking; `CLAUDE_MODEL_CONFIG` -> settings; `_meta.systemPrompt` (string / preset object); `disableBuiltInTools`; `emitRawSDKMessages` + `_claude/sdkMessage` ext notification + `SDKMessageFilter`; `forwardSubagentText` (`InitializeOptions.forward_subagent_text`, currently never set); elicitation-dependent `--disallowedTools` (drop `AskUserQuestion` when `elicitation.form`, wired with F23); `promptSuggestions`, `perTaskStopAffordance`, `enableFileCheckpointing` initialize/argv fields; bundled-CLI binary search emulating `claudeCliPath` (`@anthropic-ai/claude-agent-sdk-<plat>-<arch>[-musl]/claude`); `resourceNotFound` (-32002) mapping for "No conversation found".
- upstream: `acp-agent.ts:1731-1773, 2036-2086, 10947-10953, 11021-11133, 11226-11330, 11431-11450`, SDK argv order (sdk.mjs).
- rust: `sdk_options.rs`, `process.rs`, `agent.rs` (`session_spawn_options`, `initialize_options_for`), `system_prompt.rs`, `control.rs`.
- accept: argv/init fixtures: corpus `mcp-config-stdio`, `mcp-config-http`, `spawn-settings-object`, `spawn-plugins`, `spawn-sandbox`, `spawn-thinking`, `spawn-persist-false`, `system-prompt-meta-string`, `system-prompt-meta-preset` (all `inv_57_argv_matches_node` + `inv_39_initialize_matches_node` green with no key drop); `mcp_stdio` shared scenario.
- deps: F1. risk: low-med (argv ordering). evidence: n for argv (Node-captured); y for one real `--mcp-config` stdio tool call (F1).

#### F5 - Settings manager and model policy
- **Split:** F5a (2 turns) = `SettingsManager` watcher + managed tiers (file, MDM plist, Windows registry; exact keys read from the `sdk.mjs` `resolveSettings` bundle) + `getApiKeyHelper` precedence + `managedDenied*`; F5b (2 turns) = model policy (`availableModels`, `modelOverrides`, denied, `PreModelSwitch` veto) + cross-session context-window cache. Scripted frames for `PreModelSwitch`: `create-session-options.test.ts:1548`.
- scope: `fs` watcher (`notify` crate or poll) with 100 ms debounce over user/project/local/managed; `managedDeniedModels`, `managedApiKeyHelper`, `getApiKeyHelper(override)` precedence (managed > `--settings` > files); `setCwd`; macOS MDM plist + Windows registry managed tiers (platform code only in `process.rs`); `availableModels` allowlist, `modelOverrides`, `ANTHROPIC_CUSTOM_MODEL_OPTION` exemption, `isDeniedModel`/`normalizeModelId`, `narrowToGatewayModels` hook (list source = F33), `PreModelSwitch` veto + advisory failure + `modelBeforeSwitch` readback; cross-session `contextWindowCache` + `providerCacheKeyFor` + `immediateContextWindow` + `cacheContextWindow` + `stillCurrent` guard + re-run after model change; `apiKeyHelper` for auth probe (feeds F34).
- upstream: `settings.ts:24-242`, `managed-policy.ts`, `session-model.ts:211-608`, `acp-agent.ts:10477-10499, 13647-13755, 11487-11541`.
- rust: `settings.rs`, `session_config.rs` (WT), `usage.rs` (WT), `session.rs`, `agent.rs`.
- accept: units on temp dirs (change a file -> one `on_change`); corpus `config-model-allowlist`, `config-model-overrides`, `config-model-denied` (seed settings via `seed_files`, fake `initialize` models); `PreModelSwitch` block message needs evidence.
- deps: A-P2 committed. risk: med (settings feed argv/mode/model). evidence: y for MDM keys + PreModelSwitch hook-block text.

### Wave 2 - rendering and config (map.rs / tools.rs / permission.rs; no turn.rs)

#### F6 - Tool-shape catch-up to 0.94.0 and result bug fixes
- **Split:** F6a (3 turns) = reporters (titles/kinds/locations/aliases/cwd-relative paths/new reporters/`content.ts` block types); F6b (2 turns) = the three result bugs (TodoWrite/Task* stray updates, untracked result fallback, `shouldEmitToolCall`), `parentToolUseId` stamping, error-result content, subagent-transcript extension. Run F6b FIRST (bug fixes, small), then F6a.
- scope: reporters agent/bash/read/search/web/file-edit/interaction/index (titles, kinds, locations, aliases, `cwd`-relative `toDisplayPath` for all clients, PowerShell, SendMessage/TaskStop/ListAgents/Monitor reporters, ExitPlanMode "Approve Plan"/"Exited Plan Mode", WebSearch `Search "q"`, Read offset/limit, Grep aliases, Write `path/file_text/file_content`, Edit `oldText:null`, empty-string truthiness); `content.ts` block types; fixes: TodoWrite/Task* stray `tool_call_update` `_meta.toolName:"Other"` and stray partial refinements (`shouldEmitToolCall` guard, upstream 12821/13480); untracked tool_result falls back to `{status,rawOutput}` or nothing (13193-13215); `parentToolUseId` stamping on nested subagent updates (1899); error-result content (fenced) for Edit/Write/Skill/ExitPlanMode; subagent-transcript extension (nested assistant text forwarded stamped, gate 1911/7621/7994).
- upstream: `tool-calls/reporters/*.ts`, `tool-calls/content.ts`, `renderer.ts:206-231`, `acp-agent.ts:12806-12945, 13176-13330, 13480`.
- rust: `tools.rs`, `map.rs`, `permission.rs` (`build_tool_call`), `session.rs` (cwd into `MapState`).
- accept: corpus `tool-shapes` (one tool_use per tool incl. PowerShell, ExitPlanMode, WebSearch, Read offset-only, relative paths), `todo-write-result`, `task-tools-stream`, `subagent-nested-tool`, `edit-error-result`, `subagent-text-transcript`; table units. Rust shapes are 0.70-era; the differential only exercises Bash/Read today, so each divergence is currently invisible.
- deps: A-P3 3.4 (Task plan lane; coordinate), F2 (cwd/caps). risk: med (many fixtures change; port, never ignore). evidence: n.

#### F7 - Structured results and single-frame mappers (HAZARD E: serialized, first package of the hazardous sequence; NOT parallel with wave 1/2)
- **Split:** F7a (2 turns) = `tool_use_result`/`tool_result_meta` threading, structured Bash/Read/Agent/WebSearch results, `tool_progress`, `memory_recall`, `permission_denied`; F7b (3 turns) = `informational` (+ the #453 EAT gate), `api_retry`, `model_refusal_*`, `worker_shutting_down` (publish only; failure classification is **F25**), `control_request_progress`, `conversation_reset`, `tool_use_summary`, `auth_status` frame, refusal explanation, synthetic login/usage-limit, basic `local_command_output`. New frames can precede the echo, so each carries a unit proving it cannot reach `ensure_active_turn` ahead of a queued head.
- scope: read `tool_use_result` / `tool_result_meta` from user frames (`parseToolResultMeta`, `isAbortedToolResult`, `nonExecutionKind`, `userFeedback`); Bash/Read/Agent/WebSearch `toolResult` structured forms; frames `tool_progress` (heartbeat ids), `memory_recall`, `permission_denied`, `informational` (R811-8), `api_retry`, `model_refusal_fallback/no_fallback`, `worker_shutting_down` (publish only; failure classification is F25), `control_request_progress`, `conversation_reset` (`finishLifecycle`, `taskState.clear`, plan republish), `tool_use_summary`, `auth_status` frame, `mirror_error`/`files_persisted` log, `permission_denied`, refusal explanation chunk, synthetic login/usage-limit assistant handling, basic `local_command_output` -> `agent_message_chunk{messageId}` + `<local-command-stdout>` strip; dispatch routes (`dispatch::route` currently `Ignore`s `tool_progress`, `command_lifecycle`, `active_goal`, `prompt_suggestion`, `auth_status`, `conversation_reset`, `tool_use_summary`).
- upstream: `acp-agent.ts:6070-6087, 6259-6411, 6576-6732, 8125-8232, 7253-7272, 7873-7881, 7968-7989`, `tool-result-meta.ts`, `renderer.ts:542-723`.
- rust: `session.rs` frame arms, `dispatch.rs`, `map.rs`, `tools.rs`.
- accept: one corpus script per frame (`frame-tool-progress`, `frame-memory-recall`, `frame-permission-denied`, `frame-informational`, `frame-api-retry`, `frame-refusal-fallback`, `frame-conversation-reset`, `frame-local-command-output`, `structured-bash-result`, `structured-read-result`).
- deps: F6. risk: med (E: new frames can precede echo; EAT gate for informational/#453 fallback). evidence: y (frame shapes: tool_progress beats, informational levels, permission_denied, api_retry, model_refusal_*, local_command_output vs synthetic assistant).

#### F8 - Diff engine core (non-AIR)
- scope: byte-exact port of jsdiff `structuredPatch` (Myers, context 3, middle-slice trim) and `gitPatchText`/`quoteGitPath`/`readPatchSource`; non-AIR final diff `toolUpdateFromDiffToolResponse` (Edit/Write `structuredPatch` -> `content[diff]` + `locations`), Write-over-existing `originalFile`, created-file diff; patch memo; `_meta.claudeCode.toolResponse` content interplay with A-P3 3.1 `hookResult`.
- upstream: `diff.ts:1-748`, `reporters/file-edit.ts:102-223`, `acp-agent.ts` Write branches.
- rust: new `diff.rs`, `tools.rs`, `map.rs`, `hooks.rs` (A-P3).
- accept: units against goldens generated by running the installed `dist/diff.js` through `node -e` (many cases); corpus `edit-diff`, `write-new`, `write-existing` (hook-driven; = shared `edit_diff`, `write_new`); upstream `diff-patch.test.ts` (42) and `diff-memo.test.ts` ported.
- deps: A-P3 3.1/3.2. risk: med (jsdiff hunk selection; a different diff crate picks different hunks; the 100 ms jsdiff timeout is not reproducible - section 7). evidence: n.

#### F9 - AIR rendering: diffPatch, planFile, rawInputRendering, fileTextKeys, field-tracker pin
- scope: AIR `diffPatch` (`_meta.jetbrains.air.diffPatch`, preview + hook + created file), `planFile`/`planFilePath` + `:plan-file-clear` synthetic call, `rawInputRendering`, `fileTextKeys` rawInput stripping (renderer.ts:534-548), `ToolCallFieldTracker` `mergedMeta` fix + `pinContent`/`replacePinnedContent`/`finishResult`/`finishHook` (keep until the hook fires or 30 s), `fileChangePreviewHook` PreToolUse (v2-only gate, build now behind capability), `previewPatchContent` in permission requests; read `diffPatch/planFile/rawInputRendering` from client caps.
- upstream: `diff.ts`, `tool-calls/renderer.ts`, `field-tracker.ts`, `acp-agent.ts:9969-10031, 9868-9938`.
- rust: `map.rs`, `tools.rs`, `permission.rs`, `meta_filter.rs`, `hooks.rs`.
- accept: AIR corpus `air-edit-diffpatch`, `air-write-existing-hook`, `air-plan-file`, `air-raw-input-rendering`; units for tracker semantic (`field-tracker.test.ts` 9 ported).
- deps: F8, F2, A-P3 3.1. risk: med. evidence: n.

#### F10 - Permission presentation, effects, fallback (beyond A-P3 3.6)
- scope: `parseClaudePermissionSelection` -> "Tool use aborted" on cancelled/errored outcome (replaces Rust `user_reject` deny, PARITY row `decisionClassification`), `decodeClaudePermissionResponse` "Permission option was not offered"; presentation: AIR `_meta.jetbrains.air.permission{version,title,description,defaultToNo}`, AIR minimal toolCall, `Reason:` description, `blockedPath` extra location, SandboxNetworkAccess/computer-use/ExitPlanMode titles, JSON fallback content, non-AIR `_meta.claudeCode.{toolName,parentToolUseId,mcpServer{name,source}}`; `canUseTool` denies ("Session not found", stopped-work "The user cancelled this prompt."); `matchedAskRule`/`suppressAlwaysAllowRule`; `applyPermissionFallback` (auto -> acceptEdits + `publishFallbackWarning` as notice) + `prePlanMode` tracking; `ensureToolCallEmitted` extras (rollback on failure, eager child session id).
- upstream: `permissions/{effects,response,presentation}.ts`, `session-mode.ts:150-215`, `acp-agent.ts:9790-9938, 10033-10268`.
- rust: `permission.rs`, `session.rs::handle_permission`, `session_config.rs`.
- accept: corpus `permission-air-meta`, `permission-aborted`, `permission-mcp-server`, `permission-blocked-path`, `permission-auto-fallback`; units from `permission-presentation.test.ts` (27), `permission-effects.test.ts` (12) ported.
- deps: A-P3 3.6, F2 (notices), F9 (previewContent). risk: med. evidence: n (wire shape of aborted permission is SDK-internal: capture once with F1).

#### F11 - Config-option completion: agent picker, fast mode, recommendedValue, fallback warnings
- scope: `discoverCustomAgents` + `agent` option + `applyFlagSettings{agent}`; fast-mode option (`createFastModeConfigOption`, boolean when client supports, `resolveFastModeEnabled`, `fast_mode_state` frames at init+result, `fastModeDisabledReason`, "Fast mode turned off" notice); `recommendedValue` (`useRecommendedValue` true when the client declares it) for model + effort; `mergeEffortSettings` tier; `autoModeFallbackWarningPending`/`publishFallbackState`; `reconcileForModel`; `syncModelAfterExternalSwitch` hook handler (PostModelSwitch, A-P3 hook); `queryClosed` guard on set_*. NOTE PARITY says agent/fast mode are "not reproducible": they ARE - author the fake `initialize` with `agents:[...]` and `supportsFastMode:true`.
- upstream: `acp-agent.ts:10441-10857, 12324-12550, 11577-11661`, `session-effort.ts`, `session-mode.ts`, `session-model.ts` (recommended).
- rust: `session_config.rs` (WT), `session.rs`, `agent.rs`.
- accept: corpus `config-set-agent`, `config-set-fast-mode`, `config-recommended-value`, `config-fast-mode-state`, `config-model-switch-external`; `fast-mode-config.test.ts` (29) + `recommended-config-values.test.ts` (9) ported.
- deps: A-P2 committed, A-P3 3.1, F2, F5. risk: low-med. evidence: y (exact `fast_mode_state` frame names).

### Wave 3 - turn machine / session settle path (HAZARDOUS GROUP; schedule consecutively, extra Opus review after each, `--lib turn` before+after)

#### F12 - Turn lanes (pure TurnMachine fixes)
- scope: ignore `session_state_changed` with `sdk_host_only:true` (S-13; PARITY's "treat as real idle" is wrong - the SDK drops them); running-transition sweep of owed idles (6094-6115); steered-ready lane BEFORE owed debt (6188-6226) + `steeredAwaitingResult`; result `user_message_uuids` naming a steer retires its echo (R811-6, 6839-6852); EOF mid-steer settles with the scratch stopReason not `EndTurn` (5796); EAT clears at every upstream site (5516, 5541, 5730, 6168, 7162, held settle); cancelled result counts toward owed debt (6918); `cancelledPrompts` / stamp guard in `ensure_active_turn` (4974-4981); follow-up (d) model-row resume ordering (advance reading AFTER `resetTurnScratch`, upstream 4843 vs 4946); `failAllTurns` cancelled outcome.
- upstream: `acp-agent.ts` lines above; research S-13, R811-6, R72-2 (heuristic part).
- rust: `turn.rs`, `turn/turn_tests.rs`, `session.rs`, `dispatch.rs`.
- accept: unit tests that fail on HEAD first; corpus `idle-sdk-host-only`, `steer-result-names-steer`, `eof-mid-steer`, `model-usage-resume-first` (Node-captured; `spawn-options-resume` infra exists), `result-text-fallback-held`.
- deps: A-AG3 3.2 (touches `Failed.usage`), F3. risk: **HIGH** (O, I, E, H, S, EAT). evidence: y for S-13 (when `sdk_host_only` idles are emitted) and R811-6 ordering.

#### F13 - `msg_lifecycle_v1` command-lifecycle orphan map and R72-2
- scope: route `command_lifecycle` frames; per-uuid map (queued/started/completed/cancelled/discarded/refused; pending/started/zombie) replacing the coarse `pending_orphan_results` count; `init` capability latch (5954); `Turn.command*`; empty-interruption `[ede_diagnostic]` result consumes exactly the token it names (R72-2, 366-380, 7177-7209); cancel seeding lanes (8554-8611, 8761-8778); receipt reconcile split (INV-23); `trackOrphanCommand` (8335).
- upstream: `acp-agent.ts:366-380, 4930-5061, 5690-5722, 5839-5933, 7177-7209, 8335, 8554-8611, 8754-8787`.
- rust: `turn.rs`, `session.rs`, `dispatch.rs`.
- accept: corpus `lifecycle-coalesced`, `lifecycle-cancel-orphan`, `empty-interruption-diagnostic`; existing `inv_30|inv_23` unmodified and green.
- deps: F12, F1 evidence. risk: **HIGH** (O, I, E). evidence: **y** (upstream's own NOTE: ordering "asserted from observed CLI behavior"; cancel with 2-3 queued prompts + interrupt receipt).

#### F14 - Live-task registry levels, `background_tasks_changed`, real subagent transcripts
- scope: task record fields (`backgrounded`,`parentTaskId`,`taskType`,`endedPerLevel`); `background_tasks_changed` route + endedPerLevel marking + two-phase activation sweep (4821-4832); `isLateTaskRun`/`run_id`; `resumableSubagents` + SendMessage resume; `task_updated` `is_backgrounded` patch; non-subagent tasks in registry; E-19: make `subagent-drain`/`subagent-followup` use `system`+`subtype` frames so the hold (INV-16) is differential-covered; shared scenarios `background_after_prompt`, `subagent`.
- upstream: `acp-agent.ts:444-480, 4821-4832, 5093-5215, 6412-6575, 6733-6777`.
- rust: `turn.rs`, `session.rs`, `async_tasks.rs`, `dispatch.rs`.
- accept: corpus `background-after-prompt`, `background-tasks-changed`, `ended-per-level`, `subagent-drain-real`, `subagent-followup-real`; `inv_16` unchanged.
- deps: F12. risk: **HIGH** (I drain, held turns). evidence: y (`run_id` ordering, levels).

#### F15 - AIR async-task completions, terminal channel, Bash AIR shaping
- scope: `taskBackgrounded` (173), `backgroundTasksChanged` consumer (326), `toolResults` output-path recovery (456), `isBackgroundedToolCall`, structured Bash result path, Monitor ignore by tool name, `when_announced_state` deferral, `merge_tool_result_path`; AIR terminal channel (`terminal_info`, `terminal_output`/`_delta`, `terminal_exit`, AIR `exit_code` rule, console fallback), `withRunningTerminal`/`backgroundedBashToolCall` (`asyncTasks.backgrounded`), terminal stays running until task end (R90-7), `endBackgroundTerminal`.
- upstream: `async-tasks.ts:142-195, 326-490, 785-960`, `tool-calls/background.ts`, `renderer.ts:384-433, 675-690`, `reporters/bash.ts`, `acp-agent.ts:8027-8120`.
- rust: `async_tasks.rs`, `map.rs`, `session.rs`, `air.rs`.
- accept: corpus `async-task-bash-structured`, `async-task-level`, `bash-terminal-air`, `bash-background-air`; `async-tasks.test.ts` (45) ported.
- deps: F14, F2, F6. risk: med-high (I via task drain). evidence: n (fake frames suffice).

#### F16 - Cancel family core (R90-2)
- **Split:** F16a (3 turns) = result-time cancelled settle + `terminal_reason aborted_*` + held-cancel `followupLive` gate + `status:requesting` un-cancel/`servesCancelledWork` (the settle-path core); F16b (3 turns) = abandoned-prompt stream-delta drop (`droppedStreams`/`dropsStoppedWorkOutput`), user/assistant cancelled guard, `toolUseWaiters` (#1061); F16c (2 turns) = autonomous-cycle tracking (`startAutonomousCycle`, `workEndReported`, `autonomousCycleEnd`, `isAbortedResult`, `num_turns===0` hold), `stopBackgroundWorkflows`, `cancelChildSession` unknown id. Order a -> b -> c, one Opus review each.
- scripted frames: `acp-agent.test.ts:155` (`request_marker_lists_v1`), `:7337` (`terminal_reason`), `incomplete-tools.test.ts:1339`, `turn-events.test.ts:157` (`command_lifecycle`).
- scope: result-time cancelled settle; `terminal_reason aborted_streaming|aborted_tools`; held-cancel `followupLive` (`requestMarkerListsV1`/`modelRequestSinceResult`, 8630-8696) instead of always-inline; `status:requesting` un-cancel (6010-6020) + `servesCancelledWork`; stream-delta drop for abandoned prompts (`droppedStreams`, `dropsStoppedWorkOutput`); user/assistant cancelled guard (7818-7831); autonomous-cycle tracking (`startAutonomousCycle`, `workEndReported`, `autonomousCycleEnd`, `isAbortedResult`, `num_turns===0` hold); `stopBackgroundWorkflows` + `cancelChildSession` unknown-id path; `toolUseWaiters` (issue #1061); R86-8.
- upstream: `acp-agent.ts:5979-6020, 7026-7166, 7610-7631, 7818-7831, 8383-8789, 9636-9788, 11886-12033`.
- rust: `turn.rs`, `session.rs`, `map.rs`, `dispatch.rs`.
- accept: corpus `cancel-held-live-followup`, `cancel-aborted-result`, `cancel-stream-drop`, `cancel-before-turn`, `cancel-mid-tool` (shared `cancel_before_turn`, `cancel_mid_tool`).
- deps: F13, F14, F1. risk: **HIGHEST** (O, I, E, H, S all). evidence: **y** (interrupted result shapes, `terminal_reason`, `request_marker_lists_v1`, cancel-then-follow-up timeline).

#### F17 - Tool-call closing at cancel/cycle end (R91-3)
- scope: `closeToolCall`, `closeOpenToolCalls`, `closeCancelledToolCalls` (before the cancelled settle), `closeStoppedSubagentToolCalls`, `sendInterruptedToolResults` (`cancelled:true`), `closeCycleToolCalls`, `terminalExit`, `recordReachedToolUse`; extend `unfinished.rs` beyond `end_turn` settles.
- upstream: `acp-agent.ts:5222-5405, 5480-5484, 7336-7368`.
- rust: `unfinished.rs`, `map.rs`, `tools.rs`, `session.rs`.
- accept: corpus `cancel-open-tool-closed`, `cancel-subagent-tool-closed`, `cycle-abandoned-tool`; `incomplete-tools.test.ts` (48) ported.
- deps: F16. risk: high (H, settle ordering). evidence: y (interrupted `tool_result` shape incl. `nonExecutionKind`).

#### F18 - Teardown, closed-stream semantics, dispose
- scope: `queryClosed` + `tornDown` flags (later prompt/cancel/set_* fail with `SESSION_ENDED_MESSAGE`), wait for each turn's completion with `TEARDOWN_TURN_END_TIMEOUT_MS` + force-settle, `endAutonomousCycle`, `dispose` on connection close (explicit teardown of the registry), `closeSession` idempotence, answer `session/close` without waiting for the interrupt reply (R851-2), `abortController` equivalent.
- upstream: `acp-agent.ts:8824-8930, 8847-8915`.
- rust: `session.rs` (Close/EOF arms), `agent.rs`, `bin/claude-agent-acp-rs.rs`.
- accept: units `inv_NN_close_*`; corpus `session-close-with-turn`, `prompt-after-eof`; `tests/shutdown.rs`.
- deps: F16. risk: high (EOF settle). evidence: n.

#### F19 - Session recreate primitive (process swap) and live-session fingerprint reuse
- scope: make `run_loop` own a replaceable `{stream, control, control_in_tx, process}`; `recreate_session(resume|fresh, same public id)`; `computeSessionFingerprint` compare on resume/load of a live id (reuse blindly today -> spawn a 2nd child); rebuild when `OPTION_REBUILDS_SESSION` keys differ, keeping live permission mode; `reuseSessionId`/`publicSessionId`; `TurnMachine` ops "rebase active to queue head" + "seed carried usage"; reset `owed_trailing_idles`/`pending_orphan_results`/`live_*` on swap.
- upstream: `acp-agent.ts:1595-1730, 10859-11003, 4080-4184, 11724-11841`, `clear-context-coordinator.ts:105-164`.
- rust: `session.rs`, `turn.rs`, `agent.rs`, `process.rs`.
- accept: two-process fake-claude corpus `recreate-same-id`, `resume-live-fingerprint-same`, `resume-live-fingerprint-changed`; turn units for rebase/seed.
- deps: F18, F13. risk: **HIGH** (O, I, E, H, S: swaps the process under a live turn queue). evidence: n (fake multi-spawn, F1).

#### F20 - Context compaction lifecycle (R75-2, R78-2, R80-1)
- scope: `ContextCompactionLifecycle` (start/finish/heartbeat/reset/interrupt/resume), `compaction_update` (client `session.compaction`) vs legacy "Compact conversation" tool call, AIR `contextCompaction` meta, `compaction_summary_chunk`, PostCompact `recordSummary` + `compactionSummaryText`, duplicate failed-command stdout suppression, synthetic assistant drop after delivery, `deliveredCompactionOutput` in the #453 gate, `content_block_start{compaction}`/`compaction_delta` heartbeats; replace the WT `compact_boundary` stub.
- upstream: `context-compaction.ts`, `context-compaction-meta.ts`, `reporters/compaction.ts`, `acp-agent.ts:4542-4650, 6019-6060, 7470-7515, 7842-7848, 11347`.
- rust: new `context_compaction.rs`, `session.rs`, `map.rs`, `air.rs`.
- accept: corpus `compaction-tool-call`, `compaction-update`, `compaction-failed`, `compaction-air`, `compaction-cancelled`, `compaction-legacy`; upstream scenarios `compaction-*` snapshots; `context-compaction.test.ts` (32).
- deps: A-P3 3.1 (PostCompact), F2, F16. risk: high (I, E on compact frames). evidence: y.

#### F21 - ExitPlanMode (non hand-off half)
- scope: `acceptedPlanToolResult`, `observeExitPlanToolResults`, interruption path (`deny{interrupt:true}` -> `pendingExitPlanModeInterruption`, result settles `cancelled` not error), `plan_update` (`plan-mode`, plan-capable clients), `exitPlanModeRawOutput`, permission options `exit-plan-*` (default/acceptEdits/auto/bypass, offered order, `contextUsedPercent` label), `setMode` publish (`current_mode_update`+`config_option_update`), `prePlanMode`.
- upstream: `exit-plan.ts:12-169`, `permissions/effects.ts:110-159`, `acp-agent.ts:6802-7250, 13317-13369`.
- rust: `session.rs`, `map.rs`, `tools.rs`, `permission.rs`, `turn.rs` (intercept before `on_result`).
- accept: corpus `exit-plan-accept-mode`, `exit-plan-reject-interrupt`, `exit-plan-plan-update` (upstream scenarios `exit-plan-approve|reject`).
- deps: F10, F16. risk: high (E, I: interruption result must be intercepted before `owed_trailing_idles += 1`). evidence: y (`[ede_diagnostic]` string).

#### F22 - Clear-context hand-off (`pendingExitPlanContextReset`)
- scope: `continuePlanInFreshContext` (close old query, restart under the same public id with a new private id, carry usage/model usage onto the turn, restore fast mode, move the turn into the fresh queue, `Implement the following plan:\n\n<plan>` with the same prompt uuid, held-turn variant #1167, `cancel` aborts an in-progress restart); session-id mapping so `session_store` list/load of the public id finds the right transcript.
- upstream: `clear-context-coordinator.ts:1-164`, `exit-plan.ts:171-272`, `acp-agent.ts:2777-2818, 4781-4790, 11400`.
- rust: `session.rs`, `turn.rs`, new `clear_context.rs`, `session_store.rs`.
- accept: two-process corpus `exit-plan-clear-context`, `exit-plan-clear-context-held`; `clear-context-coordinator.test.ts` (11).
- deps: F19, F21. risk: **HIGH** (all hazards). evidence: y (diagnostic string; real restart ordering).

### Wave 4 - elicitation

#### F23 - ACP elicitation (AskUserQuestion form, MCP form/url, refusal fallback) and MCP OAuth bridge
- **Split:** F23a (2 turns) = capability parse, AskUserQuestion form (and the no-form disallowed path, exact as `acp-agent.ts:8384/11116`), `elicitation/create` client request via F3b; F23b (3 turns) = MCP `onElicitation` (form+url), `request_user_dialog` refusal fallback (+ `supportedDialogKinds`; no-handler silence is F3a), `elicitation_complete`; F23c (2 turns) = MCP OAuth bridge (`mcp_status` poll, `mcp_authenticate`, `mcp-oauth-<uuid>`, `/mcp reconnect` re-auth).
- scripted frames: `elicitation.test.ts` (36 cases), `acp-agent.test.ts:753`, `mcp-command.test.ts:717` (`mcpAuthenticate`), `:153` (`mcpServerStatus`); inbound fields from sdk.mjs `processControlRequest`.
- scope: parse `elicitation.form|url`; AskUserQuestion via `elicitation/create` form (fields `question_<n>`, `_custom`, enum `_meta["_claude/askUserQuestionOption"]`, AIR custom-answer meta; accept/decline/cancel -> allow-with-`updatedInput` / `answers:{}` / "Tool use aborted"; "AskUserQuestion called with no valid questions."; "Could not present the question to the user."); WITHOUT `elicitation.form` the tool stays disallowed exactly as upstream (`--disallowedTools=AskUserQuestion`, 8384/11116); MCP `elicitation` inbound control request (form+url, mode gating -> decline, url mode needs url, `elicitation_id` uuid, cancel on abort); `request_user_dialog` `refusal_fallback_prompt` (+ `supportedDialogKinds` in initialize, other kinds answered `cancelled`); `system/elicitation_complete` -> `elicitation/complete` (url clients); MCP OAuth bridge (`mcp_status` poll 1 s / 10 min, `mcp_authenticate` authUrl, `mcp-oauth-<uuid>`, `afterSetupResponse`), `/mcp reconnect` re-auth (`afterDelivery`).
- upstream: `elicitation.ts:1-476`, `acp-agent.ts:2518-2681, 6586-6600, 9849-9858, 10091-10113, 10270-10403, 11116-11124`.
- rust: new `elicitation.rs`, `session.rs`, `agent.rs`, `control.rs`, `sdk_options.rs`, `session_ext.rs`.
- accept: corpus (recorder `elicit_reply`): `ask-user-question-form`, `ask-user-question-decline`, `ask-user-question-no-form` (disallowed), `mcp-elicitation-form`, `mcp-elicitation-url`, `refusal-fallback`, `elicitation-complete`, `steer-during-form` (priority `later`), `mcp-oauth-url`; `elicitation.test.ts` (36) ported; shared nothing.
- deps: F1, F2, F3, F4, F14. risk: med-high (S steer `later`; cancel reply per kind). evidence: **y** (inbound subtype names/payloads for `elicitation`, `request_user_dialog`, `mcp_authenticate`).
- NOTE two turns of 3 are the OAuth bridge; if over budget split as F23a/F23b without renumbering the rest.

### Wave 5 - local commands and failures (DESCRIBED here, EXECUTED inside the hazardous sequence: F25 then F24 after F18, see section 4 step 4)

#### F24 - Local commands, `/usage`, `/remote-control`, prompt suggestions, command metadata (HAZARD E/H: in the strict hazardous sequence; scheduled after F18, NOT in wave 5)
- scripted frames: `acp-agent.test.ts:2333` (`getUsage`), `:2368` (`local_command_output`), `remote-control.test.ts:26` (`enableRemoteControl`), `mcp-command.test.ts:129`.
- scope: local-command layer (`Turn.localCommand*`, `takeLocalCommandMarkdown` exact-mirror suppression, `startsAtActivation`, `afterDelivery`, cancelled turn drops it) shared with A-AG3 3.4; `/usage` via `get_usage` (5 s) + `formatUsageResponse`; `/remote-control [name]|off` (`enableRemoteControl` control request, 3 error shapes + 3 chunk texts, handled before enqueue, listed only when supported); prompt-suggestion publish (`session_info_update._meta.promptSuggestions`, AIR, armed after user result, clear on prompt, dedupe); `getAvailableSlashCommands` AIR skill `_meta`, `{hint:""}`, `terminal_slash_commands`, `skillPaths`; `promptToClaude` fixes (`/mcp:s:c` rewrite, `[@name](uri)`); `commands_changed` skill paths.
- upstream: `usage-markdown.ts`, `prompt-suggestions.ts`, `acp-agent.ts:485-534, 567-629, 3723-3764, 3865-3992, 4220-4240, 12560-12760`.
- rust: new `usage_markdown.rs`, `remote_control.rs`, `session.rs`, `agent.rs`, `map.rs`, `control.rs`.
- accept: corpus `local-command-usage`, `remote-control-on`, `remote-control-off`, `remote-control-busy`, `prompt-suggestion-air`, `slash-commands-skills`, `prompt-mcp-rewrite`; `usage-markdown` + `remote-control.test.ts` (14) + `prompt-suggestions.test.ts` (7) ported.
- deps: A-AG3 3.4, A-P3 3.3, F7, F2. risk: high (E: local-command turns, `had_usage`). evidence: y (`get_usage`, `remote_control` response `session_url`).

#### F25 - Session failure completion and catch path (HAZARD O/I/E: EOF/catch settle; in the strict hazardous sequence right after F18, NOT in wave 5)
- scripted frames: `acp-agent.test.ts:11757` (`worker_shutting_down`), `:12135` (`model_refusal_*`), `:10840` (`api_retry`).
- scope: session-wide failure pushes (`worker_shutdown`, `transport_lost`, advisory/notice kinds, `auth_required` -> sign-out respawn mark), `lastNotice`, `processDied` string matching, settle-cancelled on error, eviction message "process exited unexpectedly", `pendingWorkerShutdown` at EOF, `clearFailuresFromEarlierTurns`, `auth_status` clears failures; usage-limit restore on replay (hook for F31).
- upstream: `session-failure-extension.ts:329-486`, `acp-agent.ts:3868, 4659, 4705, 5554-5596, 5749-5768, 6052, 8240-8318`.
- rust: `session_failure.rs` (A-AG3), `session.rs`, `agent.rs`.
- accept: corpus `session-failure-worker-shutdown`, `session-failure-transport-lost`, `session-failure-advisory`.
- deps: A-AG3 3.2, F12, F7. risk: med-high (EOF/settle). evidence: y (worker_shutting_down frame).

### Wave 6 - sessions

#### F26 - Session index core (list, caps, rows)
- **Split:** F26a (3 turns) = pure scanners (`project-dirs`, `transcript-scan`, `first-prompt`, `archive-title`, `worktrees`, `dir-listing`) + units ported from `session-index-units.test.ts`; F26b (2 turns) = `SessionIndex.list`, cursor v5, row `_meta`, `session-info`, `activity`, `live-registry`, capability advertising, `hideArchived`.
- scope: pure scanners (`project-dirs`, `transcript-scan`, `first-prompt`, `archive-title`, `worktrees`, `dir-listing`), `SessionIndex.list` with `_meta.jetbrains.air.list{limit<=200,includeWorktrees,archived}`, cursor v5 (base64url JSON bound to cwd+filters), row `_meta.jetbrains.air{archived,lastPromptAt,model,forkedFrom,agentWorktree,state,lastTurnEndedAt,cost}`, `session-info`, `activity`, `live-registry`, capability advertising (`sessionIndex`,`sessionArchive`,`sessionRename`,`sessionListSubscribe`,`sessionWorktrees`, only when the client declared `sessionIndex`, never under v2), `hideArchived`.
- upstream: `session-index/{session-index,service:1-350,session-info,activity,live-registry,project-dirs,transcript-scan,first-prompt,archive-title,worktrees,dir-listing}.ts`, `air-extension.ts:15-45`.
- rust: new `session_index/*.rs`, `session_store.rs`, `agent.rs`, `air.rs`.
- accept: ported `session-index-units.test.ts` (49); corpus `session-index-list`, `session-index-archived-filter`, `session-index-worktrees` (seeded transcripts via `seed_files`).
- deps: F2. risk: low-med. evidence: n.

#### F27 - Session index mutations: rename, archive, unarchive, delete
- **Split:** F27a (3 turns) = `_session/rename`, sidecar `custom-title.json`, explicit-title, `rename_session` control request (scripted: `session-index.test.ts:226,320`), `session_info_update{title}`; F27b (3 turns) = archive/unarchive, AIR `session/delete` = archive, real delete for index clients, archive of a loaded session, `ForkHolds`.
- scope: `_session/rename`, `_session/archive`, `_session/unarchive`; AIR `session/delete` = archive-instead-of-delete (replaces Rust -32601); real delete for `sessionIndex` clients (-32002 unknown); sidecar `custom-title.json` (atomic, 0600 file in 0700 dir via helper in `process.rs`, G9), `explicit-title`, `[archived] ` prefix on custom-title/agent-name records in every transcript copy; `rename_session` control request for a live session; archive of a loaded session = interrupt+retitle+teardown; `session_info_update` `{title}` / `_meta.jetbrains.air.archived`; `ForkHolds` (fork hidden from list until its response).
- upstream: `session-index/service.ts:305-754`, `connection.ts:200-487`, `explicit-title.ts`, `acp-agent.ts:3205-3225, 8932`.
- rust: `session_index/*`, `control.rs` (`rename_session`), `session.rs`, `session_methods.rs`, `agent.rs`.
- accept: corpus `session-rename`, `session-archive`, `session-unarchive`, `session-delete-air-archive`, `session-delete-index-real` comparing on-disk bytes; `session-index.test.ts` (168) ported in batches.
- deps: F26, F1. risk: med (live-session archive uses `Session::close`). evidence: y (`rename_session` live path).

#### F28 - Session list subscriptions
- scope: `_session/list/subscribe` (128 cap -32602 `too_many_subscriptions`), `unsubscribe` (idempotent), `_session/list/changes` (1 row/s/session, 150 ms debounce, 1 s max wait, 10 s rescan), registry/projects watchers (`notify`), rename push (R891-1).
- upstream: `session-index/list-subscriptions.ts:1-1333`, `service.ts:454-754`.
- rust: new module, `Cargo.toml` (`notify`), `agent.rs`.
- accept: tokio paused-time units (deterministic); one corpus with a seeded dir mutation after subscribe if stable, else units only (timing).
- deps: F26, F27. risk: med. evidence: n.

#### F29 - Fork at message and complete fork rewrite
- scope: `_meta.jetbrains.air.fork{version:1,messageId,messageFingerprint,messageOccurrence}`; `forkPointMessageIdCandidates` (`:segment:N` strip); live `messageIdToUuid` map (+ backfill on replay); `assistantGroups`+sha256 fingerprint+occurrence-on-branch; `upToMessageId`; compaction-aware chain rewrite (`compactMetadata`, attachment uuid remap); fork file mode 0600 (helper in `process.rs`); promptable-at-once fork (R90-6); `forking()` hold-back/`onForkCreated`.
- upstream: `fork-session.ts:74-239`, `acp-agent.ts:3113-3143, 12766-12805`, SDK `forkSession`.
- rust: `session_store.rs`, `session_methods.rs`, `session.rs`, `map.rs`.
- accept: corpus `session-fork-at-message`, `session-fork-compacted`; units for fingerprint/occurrence.
- deps: A-P3 3.5 (replay backfills ids), F26. risk: med. evidence: y (uuid-to-messageId grouping, `upToMessageId` semantics).

#### F30 - Session titles
- scope: `SessionTitles` (context ring 1000 chars, 10-char minimum, `onPrompt/onAssistantText/onTurnEnd/reset`), `generateSessionTitle` control request, `session_info_update{title,updatedAt}`, `clientTitle`, `getSessionInfo` customTitle/summary fallback, reset on clear-context.
- upstream: `session-titles.ts:1-354`, `acp-agent.ts:4408-4640, 6168`.
- rust: new `session_titles.rs`, `session.rs`, `map.rs`, `session_store.rs`.
- accept: corpus `session-title-generated`, `session-title-explicit-blocks-generated`; `session-titles.test.ts` (11).
- deps: F27, F1. risk: med-high (post-settle async update must not disturb I/O ordering; run after F22). evidence: **y** (`generateSessionTitle` subtype).

#### F31 - History replay extras
- scope: AIR replay extras on top of A-P3 3.5: usage-limit failure restore, compaction summary -> `compaction_update`, `forwardSubagentText`, task plan re-publish, `replayTerminalStates`, event-loop yield between messages, `loadReplayEnd` meta, `isSyntheticLoginMessage`/hidden meta/task-notification skips, strip system reminders from replayed prompts (R77-4), marker-only slash prompts (R811-5), `readResumedSession` messages path.
- upstream: `acp-agent.ts:2087-2371, 9037-9545`, `resumed-session.ts`, `session-load.test.ts`.
- rust: `transcript.rs` (A-P3), `map.rs`, `agent.rs`.
- accept: corpus `session-load-replay-air`, `session-load-replay-compaction`, `session-load-replay-usage-limit`; upstream scenario `session-load-replay` snapshots (air/plain/zed).
- deps: A-P3 3.5, F20, F25. risk: low-med. evidence: n.

### Wave 7 - auth and providers

#### F32 - authenticate, providers/list|set|disable, logout completion, provider env
- scope: `authenticate` (gateway / gateway-bedrock, -32602 messages exact, "Method not implemented."); `providers/list|set|disable` (provider id `main`, env-derived `current`, -32602 for unknown id/apiType/baseUrl, vertex needs `_meta.claudeCode.vertex.{projectId,region}`); `createEnvForProvider`, `PROVIDER_ROUTING_ENV_VARS`, `isValidBaseUrl`, `sameHeaders`; logout completion (clear gateway/provider override, `authEpoch++`, clear context cache); `settings` path-or-object + `apiKeyHelper:""` at create (11174-11245).
- upstream: `acp-agent.ts:1774-1845, 3226-3262, 3503-3722, 11174-11245, 12214-12310`.
- rust: new `providers.rs`, `agent.rs`, `auth_status.rs`, `process.rs`.
- accept: units on exact -32602 messages; corpus `providers-list-env`, `providers-set-gateway-spawn-env`, `authenticate-gateway`, `logout-clears-provider`; `providers.test.ts` (34).
- deps: F2, F4, F5. risk: med. evidence: n.

#### F33 - Gateway model listing and provider-update recreate
- scope: `gateway-models.ts` (`/v1/models` pagination, Bedrock `/foundation-models`, region prefix, 10 s timeout, 20-page cap, credential-URL rejection) with an HTTP client crate; `narrowToGatewayModels` source; `enqueueProviderUpdate` (wait turns, close, resume every live session with the same id under new env), `reportSessionLostOnProviderUpdate`.
- upstream: `gateway-models.ts`, `acp-agent.ts:11724-11841, 3523-3630`.
- rust: new `gateway_models.rs`, `agent.rs`, `session.rs`.
- accept: local HTTP stub units; corpus `providers-set-recreates-sessions`; `gateway-models.test.ts` (28).
- deps: F32, F19, F5. risk: med-high (session recreation between turns). evidence: n.

#### F34 - hide-claude-auth guard, per-prompt re-probe, sign-out respawn
- scope: `--hide-claude-auth`, `billsClaudeSubscription`/`holdsNonSubscriptionCredential`, create-time guard on `accountInfo`, per-turn `refuseClaudeSubscriptionTurn` (shared `pendingRun`), `authRequired` -32000 data `{reason:"claude_subscription_not_supported"}`, `markSessionForSignOutRespawn`/`respawnSignedOutSession`/`recreateLiveSession`, per-prompt auth re-probe (`useAuthStatusSettings`+probe at `startTurn`), `--settings` override on probe, hide terminal auth methods in `initialize`, auth epoch, `vendor` bag, "Air Gateway" labels.
- upstream: `hide-claude-auth.ts:61-287`, `acp-agent.ts:3263-3502, 3993-4184, 4170, 5572`.
- rust: `auth_status.rs`, `session.rs`, `agent.rs`, `bin/*`.
- accept: corpus with fake `accountInfo` for the 3 verdicts (subscription / none / API key) + `auth-required` interplay; units for the verdict tables.
- deps: F19, F2, F5, F32. risk: high (recreate at a turn boundary; E). evidence: y (`accountInfo` fields `tokenSource`,`apiKeySource`).

### Wave 8 - subagents

#### F35 - Native subagent sessions, core (AIR form)
- **Split:** F35a (3 turns) = pure runtime (registry maps, generations, buffering 64/256/32, `route`/`routeOfToolCall`, `finishTask` states) ported with `native-subagents.test.ts` unit cases, no wiring; F35b (3 turns) = wiring into `session.rs`/`map.rs` sendUpdate chokepoint + AIR `subagent_spawned`/`subagent_state_update` + corpus from the AIR scenarios; F35c (2 turns) = child permission/question sessions, eager tool-call child id, `routeOf` into `AsyncTaskRuntime`, `toolUseOwner`.
- scope: child session registry (`children`,`taskByToolUse`,`parentByToolUse`,`childByParentToolUse`,`childByToolCall` cap 2048,`senderByToolUse`,`generationByTaskId`), `route`/`routeOfToolCall` (late async-task updates still reach finished child), pending-parent buffering (64/256/32), `taskStarted` (needs `subagent_type`), `finishTask` states, `finishAll`, `parentCancelled`, eager tool-call child session, `subagent_spawned`/`subagent_state_update`, child permission/question sessions, `routeOf` into `AsyncTaskRuntime`, `toolUseOwner`.
- upstream: `native-subagents.ts:127-660, 890-1260`, `acp-subagents.ts`, `acp-agent.ts:4506-4640, 9940-9967`.
- rust: new `native_subagents.rs`, `session.rs`, `permission.rs`, `async_tasks.rs`, `map.rs`.
- accept: corpus from upstream scenarios `subagent-native-sessions`, `-nested`, `-late-child-update`, `-agent-async` compared with `__snapshots__/air/*`; `native-subagents.test.ts` (71) in two batches.
- deps: F14, F6, F23. risk: **HIGH** (child output leaves the root feed: must not set EAT or `had_usage`; `task_notification` must feed both `on_task_ended` and `finishTask` in Node's order so `inv_16` holds). evidence: n for AIR form.

#### F36 - Native subagents RFD form, child cancel, awaiting-user
- scope: RFD `subagent_update{capabilities:{cancel:{}},state}` + `session_message`; `cancelChild` via `stop_task`, `session/cancel` on a child session id, `awaitingUser` (`requires_action`), `rfdEndState`, `sendMessageResumed`/`resumedNativeSubagentId`, `sessionCapabilities.subagents` for non-AIR (client cap `subagents:{}`), keep background subagents after parent cancel (R91-2), `stopsWorkflowsOnCancel`.
- upstream: `native-subagents.ts:401-660, 1284-1400`, `acp-agent.ts:8396-8435, 8471-8502`.
- rust: `native_subagents.rs`, `session.rs`, `agent.rs`, recorder `client_capabilities` (F1).
- accept: corpus with `subagents:{}` vs Node; upstream `__snapshots__/v2/subagent-*` shapes.
- deps: F35, F16. risk: high (cancel family). evidence: y (child `session/cancel` semantics).

#### F37 - Subagent history, replay, restore
- scope: `subagentHistory` (`<session>/subagents/agent-<id>.jsonl` + `.meta.json`), lineage, `RfdSubagentReplay`, `restore`/`restoreLaunch`/`restoreAgent`, background task stops restored on replay (R851-3), resumed subagent generations (R812-1), `stripSubagentTextAndThinking`.
- upstream: `subagent-history.ts`, `subagent-replay.ts`, `native-subagents.ts:667-880`.
- rust: new `subagent_history.rs`, `transcript.rs`.
- accept: seeded transcripts + `session-load-replay-subagents`; `subagent-history.test.ts` (12).
- deps: A-P3 3.5, F35, F36. risk: low-med. evidence: n.

### Wave 9 - ACP v2 (experimental surface, opt-in env)

#### F38 - v2 router, initialize, session methods
- **Split:** F38a (3 turns) = crate feature + `protocol_router` + gating env + pure translators `initialize.ts`/`session.ts` with units; F38b (2 turns) = v2 session methods wiring (`auth/logout`, `providers/*`, replay-from) and recorder v2 variant.
- scope: enable crate feature `unstable_protocol_v2` (present in agent-client-protocol 2.1.0; verified by research), `protocol_router().with_v1(..).with_v2(..)` gated on `CLAUDE_AGENT_ACP_EXPERIMENTAL_V2=1`; translators `initialize.ts`, `session.ts` (`id`->`configId`, `groupId`, `replayFrom`), no `steering`, no session-index caps under v2, `auth/logout`, `providers/*`, `systemPrompt` `append` mode advertisement parity check.
- upstream: `serve.ts`, `v2/{initialize,session,agent}.ts`, `index.ts:75`.
- rust: `Cargo.toml`, new `v2/*.rs`, `bin/*`, `agent.rs`.
- accept: pure `Value`->`Value` translator units; v2 recorder variant vs Node (`Client.v2()`); `acp-v2.test.ts` (126) in batches.
- deps: F2, F32, F26. risk: med. evidence: n.

#### F39 - v2 prompt and foreground events
- scope: `ForegroundEvents` (running/requires_action/idle{stopReason,usage,_meta,error}); `session/prompt` responds when the user echo is inserted with `{messageId}` and the end arrives as `state_update`; `openUserInputs` -> `requires_action`; autonomous cycles; `messageId` on every chunk; `replayClear`.
- upstream: `v2/prompt.ts`, `turn-events.ts`, `acp-agent.ts:9636-9788`.
- rust: `session.rs`, `turn.rs`, `v2/*`.
- accept: v2 corpus (`__snapshots__/v2/*` 40 scenarios as expected output).
- deps: F38, F16, F23. risk: **HIGH** (prompt response timing touches I, H, S). evidence: y (foreground-state semantics).

#### F40 - v2 update translators
- scope: `session-update.ts` (plan_update/plan_removed, notice, compaction), `tool-call.ts`, `tool-call-meta.ts` (diff `changes`, `v2DiffContent`), `terminal.ts`, `permission.ts` (`subject`,`title`).
- upstream: `v2/{session-update,tool-call,tool-call-meta,terminal,permission}.ts`, `diff.ts` v2 parts.
- rust: `v2/*.rs`, `map.rs`.
- accept: replay all `__snapshots__/v2/*.jsonl` scenarios.
- deps: F39, F9, F15, F20, F23, F35, F36. risk: med. evidence: n.

### Wave 10 - residual and infrastructure

#### F41 - Process-group reaper (grandchildren of a killed host)
- scope: spawn `claude` in its own process group; on host exit (any cause incl. SIGKILL) kill the group: Linux `PR_SET_PDEATHSIG`; macOS/other: a re-exec'd supervisor (`--reaper`, kqueue/poll on parent pid) that kills the group; Windows Job Object; all platform code in `process.rs` (G9). Node itself leaves grandchildren after SIGKILL of the host, so this exceeds Node parity and is done because INV-28 / Risk 17 is a listed scope item.
- rust: `process.rs`, `bin/*`.
- accept: `inv_28_*` extension: SIGKILL host with a `claude` that spawns a `sleep` grandchild; grandchild gone (`ESRCH`) within 2 s; macOS + Linux.
- deps: none. risk: med (platform). evidence: n.

#### F42 - Hermetic fixtures (remove timing stabilisers) - 2 turns, scheduled BEFORE the hazardous wave (section 4 step 3)
- scope: remove 150 ms `sleep_ms` pacing from permission fixtures (use ordered `expect` steps), keep `sleep_ms` only where Node's own race needs it (documented); all new scripts follow.
- rust: `rust/fake-claude`, `porting/corpus/permission-*`.
- accept: differential green 20x in a loop; fixtures byte-identical or re-captured with a justification.
- deps: none. risk: low. evidence: n.

#### F43 - Concurrency-fidelity program (cross-repo Part B)
- decision to record (was unowned): upstream's fairness yield inside the live consumer loop (`acp-agent.ts:5650-5662`, `messageLoopYielder.maybeYield()`, plus `EventLoopYielder` in `utils.ts` and the replay yield) exists so a buffered backlog cannot starve timers and `session/cancel`. Under tokio it is NOT ported as code: `run_loop` is a `select!` over cancel/steer/command channels and the frame stream, every `recv().await` is a cooperative point, and a unit `inv_NN_cancel_not_starved_by_backlog` (10k buffered frames, then `session/cancel` must be handled before the backlog drains past N frames) proves the equivalent guarantee; if the measured benchmark shows starvation, add `tokio::task::yield_now()` every K frames (K from the benchmark). Upstream tests: `event-loop-yield.test.ts` (10 cases) are ported as the race-suite seeds.
- scope: `porting/parity/CONCURRENCY.md`: measured decision single-thread (`current_thread`/`LocalSet`) vs multi-thread with benchmark numbers (streaming chunk latency, steering); audit of every shared mutable state vs Node counterparts; deterministic race suite >= 200 iterations (cancel vs result, steer vs end-of-turn, close during prompt, permission reply timing, notification ordering, late/duplicate responses, child exits mid-turn) asserting Node's outcome with cited lines; panic/exit policy (release profile `panic`, other sessions, clean error rather than hang when the adapter dies mid-turn).
- rust: `bin/*`, `rust/Cargo.toml` profile, tests.
- accept: race suite results (iterations, pass counts) recorded in STATUS.md.
- deps: F12-F19 (races exist only once the lanes exist), A-INFRA. risk: med. evidence: n.

#### F44 - Shared corpus completion and live smoke
- scope: add the shared-scenario scripts no feature package owns (section 5): `edit_diff`, `write_new` aliases, `cancel_before_turn`, `queued_prompt`, `resume_recall`, `initialize_capabilities`, `mcp_stdio` wiring; `porting/corpus/SHARED.md` manifest mapping shared names -> script names; record/replay status per scenario; `PARITY_LIVE=1` gated live smoke (initialize, new, tiny prompt, cancel, resume; skipped when `claude` absent; same test names as codex); final live matrix (A-P3 3.9 extended with the new scenarios).
- rust: `tests/live_smoke.rs`, `porting/corpus/*`, `porting/parity/STATUS.md`.
- accept: all 23 shared names resolve to a green differential; live smoke passes on the host.
- deps: most F. risk: low. evidence: y (live).

#### F45 - TEST-MAP burn-down (every upstream test case)
- scope: after A-INFRA generates TEST-MAP, resolve every remaining `missing:` row by porting the upstream test to Rust (unit or corpus) or `covered:`; batches of <= 3 turns sized by `MISSING.md` counts (acp-agent.test.ts 620, acp-v2 126, tools 211, session-index 168, create-session-options 102, mcp-command 51, ...). Closing criterion: `test-map-check` reports zero `missing`.
- deps: all feature F packages (a row can only flip once its feature exists). risk: low. evidence: n.

#### F46 - Final audit and verification rounds (process, not code)
- scope: fresh-Opus audit with only the tag + repo; vibe-station `vs-275` live matrix against the candidate head; zero open divergences; `DECISIONS-FOR-USER.md` entries for section 7 exceptions.
- deps: all. risk: n/a. evidence: y.

## 4. Recommended order

1. **Wave 0 (already planned, in this order):** A-P2 verify+commit -> A-INFRA phase 1 (TEST-MAP/MODULE-MAP skeleton; makes every later `missing:` row traceable to an F-id) -> A-P3 (3.1 hooks, 3.2 corpus, 3.3, 3.4, 3.5 replay, 3.6 options) -> A-AG3 3.1 goal, 3.3 file report, 3.4 `/mcp` -> A-AG3 3.2 sessionFailure LAST in this wave (it edits `TurnEvent::Failed`; F12 must rebase on it).
2. **Wave 1 foundations** (<= 2 in parallel, disjoint files): F1 -> F3a (SDK control layer, early) -> F2a/F2b/F2c -> F3b -> F4a/F4b -> F5a/F5b.
3. **Wave 2 rendering and hygiene** (no turn.rs; parallel only with wave 1): F6b -> F6a -> F8 -> F9 -> F10 -> F11, and **F42 (hermetic fixtures) here, BEFORE the hazardous wave**, so timing flakiness never muddies a differential. F7 is NOT in this wave.
4. **Hazardous settle-path sequence, strictly sequential, one Opus review after EACH package, `--lib turn` before/after, nothing else editing `session.rs`/`turn.rs`/`map.rs` meanwhile:** F7a -> F7b (frames that can precede the echo, serialized) -> F12 -> F13 -> F14 -> F15 -> F16a -> F16b -> F16c -> F17 -> F18 -> **F25 (EOF/catch settle)** -> **F24 (local-command turns, `had_usage`)** -> F19 -> F20 -> F21 -> F22. (No package waits on a real recording; frames are authored per section 6.)
5. **Wave 4 elicitation:** F23a -> F23b -> F23c (touches the steer `later` decision: after step 4).
6. **Wave 6 sessions:** F26a -> F26b -> F27a -> F27b -> F28 -> F29 -> F30 -> F31 (F30 after F22).
7. **Wave 7 auth:** F32 -> F33 -> F34. **Wave 8 subagents:** F35a -> F35b -> F35c -> F36 -> F37.
8. **Wave 9 v2:** F38a -> F38b -> F39 -> F40 (last feature work: it needs diff, terminal, plan, elicitation, replay, providers and subagent RFD).
9. **Wave 10:** F41 any time; F43 after step 4; F44, F45, F46 last.

Dependency graph (arrows = must precede): F1->{F3a,F2a,F4a,F23b,F27a,F30,F36}; F3a->{F3b,F23b}; F2->{F3b,F6a,F9,F10,F11,F15,F20,F24,F26b,F32,F34,F38a}; F3b->{F12,F23a}; F4->{F23a,F32}; F5->{F11,F32,F33,F34}; F6->{F7a,F15,F35a}; F8->F9->F10; F42->F7a; F7b->F12->{F13,F14,F25}; F13->{F16a,F19}; F14->{F15,F16a,F23a,F35a}; F16c->{F17,F18,F20,F21,F36,F39}; F18->{F25,F19}; F19->{F22,F33,F34}; F21->F22; F23->{F35a,F39}; F26->{F27,F28,F29,F38a}; F27->{F28,F30}; F32->{F33,F34,F38a}; F35->{F36,F37}; F38->F39->F40.

Overlap with in-flight plans (do not duplicate): hooks/PostToolUse/toolResponse/Task plan/replay/permission options = A-P3; goal/sessionFailure/file report//mcp = A-AG3; TEST-MAP/MODULE-MAP = A-INFRA. F-packages only add what those plans explicitly leave out (the "not planned" column in section 1).

Parallelism guidance: wave 1+2 packages have disjoint files except `agent.rs`/`session.rs`; run at most two at a time. The hazardous sequence (step 4) MUST NOT overlap itself or any package editing `session.rs`/`turn.rs`/`map.rs`.

## 5. Shared corpus scenarios (cross-repo program) -> `porting/corpus`

Existing = committed unless marked WT (uncommitted/untracked in the tree). Upstream also owns a 36-scenario scripted harness (`src/tests/acp-scenarios/scenarios.ts`, snapshots `__snapshots__/{air,v2}/`, `origin-main/{plain,zed}/`) usable as an extra oracle for F8-F9, F15, F20, F21, F31, F35-F40.

| shared name | existing script(s) | gap -> package that adds it |
|---|---|---|
| plain | `text-only` | none |
| tools_shell | `single-tool`, `multi-tool`, `tool-then-text`, `streamed-partial-input` | Bash/Read only; add `tool-shapes` (F6) |
| edit_diff | - | add `edit-diff` (F8) |
| write_new | - | add `write-new`, `write-existing` (F8); AIR variants F9 |
| permission_allow | `permission-allow`, `permission-suggestions` | none (more forms: A-P3 3.6 scripts) |
| permission_deny | `permission-deny` | none |
| cancel_mid_tool | `cancel-mid-turn` (not mid-tool) | add `cancel-mid-tool` (F16/F17) |
| cancel_before_turn | `cancel-queued-echo-less` (partial) | add `cancel-before-turn` (F16) |
| steer_mid_turn | `steer-mid-turn`, `steer-error-result` | none (F12 adds lane variants) |
| queued_prompt | `lagging-idle`, `tool-then-text`, `cancel-queued-echo-less` (2-3 prompts, indirect) | add `queued-prompt` (F44) |
| resume_recall | `resume-load`, `session-resume`, `spawn-options-resume` (WT) | add `resume-recall` (F44 / A-P3) |
| session_load_replay | - | `session-load-replay`, `-live` (A-P3 3.5); AIR/compaction variants F31 |
| config_options_model_mode | `config-session-new`, `config-set-mode` (WT) | none |
| set_config_option | `config-set-model`, `config-set-effort`, `config-invalid`, `config-set-model-rpc` (WT) | add `config-set-agent`, `config-set-fast-mode` (F11) |
| image_input | `image-prompt-echo` | none |
| slash_commands_skills | `commands-proactive` | add `slash-commands-skills` (F24) |
| usage_update | `usage-update`, `usage-compact`, `usage-rate-limit`, `usage-no-usage` (WT) | none |
| background_after_prompt | `async-task-air`, `async-task-non-air`, `async-task-stop`, `subagent-followup` (synthetic frames) | add `background-after-prompt` (F14) |
| auth_failure | `error-result`, `auth-status`, `logout-auth` | add `auth-required` (A-AG3 3.2) |
| session_list_close_delete_fork | `session-list`, `session-close`, `session-fork`, `session-resume` | AIR delete/fork-at-message: F27, F29 |
| initialize_capabilities | `initialize.json` in every script; `system-prompt-*` init fixtures | add `initialize-capabilities` variants (F2) |
| mcp_stdio | - | add `mcp-config-stdio` (F4); `mcp-status` (A-AG3 3.4) |
| subagent | `subagent-drain`, `subagent-followup` (E-19: not real frames) | `subagent-*-real` (F14), native sessions (F35-F36) |

Pre-existing scripts outside the shared names (kept): `air-tool-meta`, `async-task-stop`, `auth-status`, `cancel-queued-echo-less`, `commands-proactive`, `consolidated-text*`, `context-echo-less`, `custom-instructions`, `eof-mid-turn`, `folded-prompt`, `idle-without-result`, `lagging-idle`, `model-usage`, `spawn-*` (9), `system-prompt-*` (7), `thinking-empty`, `tool-result-echo`, `unfinished-tool`.

## 6. Source of the scripted frames (NO package needs a real-`claude` recording)

Basis: the upstream harness (`src/tests/acp-scenarios/harness.ts:1-18`) feeds the adapter scripted SDK messages that match the CLI stdout one-to-one (minus frames the SDK filters), so upstream's own tests already script every frame the formerly "evidence-gated" packages need. Outbound control subtypes are written by Node's own SDK into fake-claude during `capture.sh` (the capture records the exact wire form); inbound payload fields are spelled out in `sdk.mjs` `processControlRequest`. Each package therefore uses an AUTHORED transcript that the Node adapter replays through our fake-claude; the Rust side must produce the same ACP frames.

| package(s) | frames / behaviour | scripted in (upstream v0.94.0 `src/tests/`) |
|---|---|---|
| F13, F16a-c | `command_lifecycle`, `request_marker_lists_v1`, `status:requesting` | `acp-agent.test.ts:141,155`; `turn-events.test.ts:157`; `acp-v2.test.ts:3074,3157` |
| F16, F17 | `terminal_reason aborted_*`, interrupted results | `acp-agent.test.ts:7337`; `incomplete-tools.test.ts:1339` |
| F13, F21, F22 | `[ede_diagnostic]` empty-interruption result | `acp-agent.test.ts:21274` |
| F7a, F17 | `nonExecutionKind` / `tool_result_meta` | `acp-agent.test.ts:7935`; `incomplete-tools.test.ts:680`; `tools.test.ts:3768` |
| F12 | `sdk_host_only` idle | verified in `sdk.mjs` `readMessages` (the SDK drops these frames); author a transcript line `{"type":"system","subtype":"session_state_changed","state":"idle","sdk_host_only":true}` and let Node's capture show the outcome |
| F14, F15 | `background_tasks_changed`, `task_updated` (`run_id`,`is_backgrounded`) | `acp-agent.test.ts:21063,6803`; `async-tasks.test.ts:776`; `live-capability-session-audit.test.ts:317,338` |
| F7a/F7b | `tool_progress`, `memory_recall`, `permission_denied`, `informational`, `api_retry`, `model_refusal_*`, `conversation_reset`, `local_command_output` | `acp-agent.test.ts:6290,17720,27588,17067,10840,12135,352,2368`; `acp-scenarios/scenarios.ts:452,1081,1100` |
| F25 | `worker_shutting_down` | `acp-agent.test.ts:11757`; `live-capability-events-audit.test.ts:273` |
| F11 | `fast_mode_state` | `acp-agent.test.ts:10197`; `fast-mode-config.test.ts` |
| F5b | `PreModelSwitch` | `create-session-options.test.ts:1548`; `gateway-models.test.ts:396` |
| F34 | `accountInfo`, `tokenSource`/`apiKeySource` | `authorization.test.ts:427,759`; `auth-status.test.ts:135,281` |
| F20 | `compact_boundary`, PostCompact | `acp-agent.test.ts:8506`; `context-compaction.test.ts` |
| F24 | `get_usage`, `enableRemoteControl`, `local_command_output` | `acp-agent.test.ts:2333,2368`; `remote-control.test.ts:26`; `mcp-command.test.ts:129` |
| F23a-c | `elicitation`, `request_user_dialog`, `mcpAuthenticate`, `mcpServerStatus` | `elicitation.test.ts`; `acp-agent.test.ts:753`; `mcp-command.test.ts:153,717`; `acp-v2.test.ts:61` |
| F27a, F30 | `rename_session`, `generate_session_title` | `session-index.test.ts:226,320,914`; `session-titles.test.ts:52` |
| F36, F35 | `subagent_type` task frames, child cancel | `acp-agent.test.ts:985`; `native-subagents.test.ts:1869`; `incomplete-tools.test.ts:262` |
| F29 | uuid-to-messageId grouping, `upToMessageId` | `fork-session` cases in `session-index.test.ts`, `acp-agent.test.ts` `messageIdForGrouping` cases |
| F3a | SDK control layer | `sdk.mjs` `processControlRequest`/`handleControlRequest`; `acp-agent.test.ts:753` |

- Real recordings (`porting/record-real.sh`, human-only) are **optional-later and non-blocking**: wanted only for corpus realism (PARITY "corpus provenance": the 26+ transcripts are synthetic) and for F44 (live smoke) / F46 (audit).

## 7. Items that stay open

Challenged one by one: almost nothing is impossible. Only the first row is truly impossible; every other former exception is now a package.

| item | status | technical reason / resolution |
|---|---|---|
| `options.hooks`, `canUseTool`, `onElicitation`, `abortController` (JS function options) | **impossible over a JSON wire** | functions cannot be serialised into ACP `_meta`/JSON-RPC. Equivalent effect: the CLI-side equivalents are ported (`hook_callback` -> A-P3 3.1, `can_use_tool` -> A-P3/F10, `elicitation` -> F23, abort -> `session/cancel`/F16); JSON keys of `options` are honoured (F4a) |
| in-process SDK MCP servers (`type:"sdk"`) | **impossible** | the server is a JS object living in the Node adapter process and answers `mcp_message` control requests from JS code; a Rust binary has no such object. Equivalent effect: stdio/http/sse MCP servers (F4a); a host needing in-process tools runs them as a stdio server |
| `claudeCliPath` via `import.meta.resolve` | **package F4b** | emulate: search the same `node_modules/@anthropic-ai/claude-agent-sdk-<plat>-<arch>[-musl]/claude` layouts, then `CLAUDE_CODE_EXECUTABLE`, then PATH |
| `_meta.terminal-auth.command/args` | **package F2a** | equivalent values (Rust binary + `--cli ...`, F2b implements `--cli`); the differential normalises the two Node-process strings; this is a choice of values, not a limitation |
| jsdiff 100 ms patch timeout | **package F8** | implement the same wall-clock timeout (`Instant` check inside the diff loop); fall back exactly as `diff.ts` does on timeout |
| `session/request_permission` id type; malformed-line error shape | **package F2c** | wrap the transport (custom line codec in front of the crate) so ids are numbers and parse errors match Node's ndJsonStream |
| MDM plist / Windows registry managed tiers | **package F5a** | read the keys from the `sdk.mjs` `resolveSettings` bundle; platform code in `process.rs` |
| `agentInfo.version` = `<crate>+upstream.<pin>` (Decision 3) | **user decision** | a choice, not a limitation: keep the deliberate divergence unless the user asks for the plain npm version; vibe-station doctor currently relies on it |
| upstream fairness yield (`messageLoopYielder`, `acp-agent.ts:5650-5662`) | **documented n/a + test (F43)** | tokio `select!` is cooperative at every await; see F43 decision |

Everything else in PARITY.md / INVARIANTS.md maps to a package in section 8; nothing is left "skipped-deliberate".

## 8. PARITY.md / INVARIANTS.md pending / not-ported / GAP -> package

| item (ledger row) | closed by |
|---|---|
| (A) fs passthrough `fs/*` agent->client (upstream dead code: only call sites are `ClientConnection` wrappers `acp-agent.ts:2471-2475, 9548-9553`, no handler invokes them) | F3 (helpers, kept for surface parity; no wire-observable behaviour exists upstream) |
| (B) Config options / modes / fast mode | A-P2, F11 |
| (B) Model resolution + allowlisting | A-P2, F5 |
| (B) Elicitation | F23 |
| (B) File-change audit | A-AG3 3.3 |
| (B) Session-failure ext | A-AG3 3.2, F25 |
| (B) Auth / gateway / providers | F32, F33, F34 |
| (B) TODO / plan lists | A-P3 3.4, F6 |
| (B) Settings watching | F5 |
| (B) Subagent transcripts | F35-F37 |
| (B) Hooks (PostToolUse/TaskCreated/TaskCompleted/PostModelSwitch/PostCompact) + `promptSuggestions` | A-P3 3.1/3.3, F11 (PostModelSwitch handler), F20 (PostCompact), F24 (publish) |
| (B) Terminal support | F15 |
| (B) Goal extension | A-AG3 3.1 |
| (B) Custom slash commands | F24 |
| (B) MCP server passthrough | F4 |
| Unsupported ACP methods `authenticate`, `set_mode`, `_session/goal` | F32, A-P2, A-AG3 3.1 |
| Discarded SessionUpdate variants SessionInfoUpdate / ConfigOptionUpdate / UsageUpdate | F30, A-AG3, F24 (SessionInfo); A-P2 (others, done in WT) |
| `session/load` history replay (D15) | A-P3 3.5, F31, F37 |
| `suppressControlResponse` (D6) | F3a. CORRECTION: the adapter's callbacks never return null, BUT the SDK itself sends no response for `request_user_dialog` when no handler is set and for the `Z4` subtypes (sdk.mjs `processControlRequest`), a path reachable today by every client without form elicitation; Rust must stay silent there |
| EOF mid-steer / `on_stream_end` stop reason | F12 |
| `msg_lifecycle_v1` orphan map | F13 |
| `endedPerLevel` sweep | F14 |
| Grandchildren of a SIGKILLed host (INV-28 / Risk 17) | F41 |
| 150 ms `sleep_ms` pacing | F42 |
| (C) Async-task details (`when_announced_state`, `merge_tool_result_path`, `toolResults`, Monitor filter, `backgroundTasksChanged`) | F15, F14 |
| (C) Advertised-but-inert AIR caps: `agentFileChangeReport`, `sessionFailure`, goal | A-AG3; `nativeSubagentSessions`/`subagents` F35-F36; `diffPatch`/`planFile`/`rawInputRendering` F9; `recommendedValue` F11; `loadReplayEnd` F31; session-index family F26 |
| (C) authStatus: per-prompt re-probe, ACP gateway `authenticate`/`providers/set` overrides, `--hide-claude-auth` + epoch, `vendor` bag | F34, F32 |
| (C) AIR `session/delete` archive | F27 |
| (C) Fork at a message; git-worktree siblings; sidecar title store; `compactMetadata`/attachment remap; fork file mode 0600 | F29; F26; F27; F29; F29 (helper in `process.rs`) |
| (C) `/mcp` | A-AG3 3.4 |
| (C) `/remote-control` (skipped-deliberate) | F24 |
| (C) Cancel family R90-2 / R91-3 / R86-8 | F16, F17 |
| (C) Stamped-result lanes R811-6 / R72-2 | F12, F13 |
| (C) Subagent transcripts (E-19) not real | F14 |
| (C) Context compaction tool lifecycle R75-2 (+R78-2, R80-1) | F20 |
| (C) AIR Bash shaping / `async-task-bash` (OQ-14) | F15 |
| (C) Unfinished tools: cancel-time closing `closeCancelledToolCalls`; `sessionFailure` for `incomplete_tool_call` | F17; A-AG3 3.2 |
| (C) Durable permission forms; option order `default_to_no` | A-P3 3.6 |
| (C) `providers/list|set|disable` | F32 |
| (C) AIR `rawInput` `fileTextKeys` stripping | F9 |
| (C) `_meta.systemPrompt` (string / preset) | F4 |
| (C) R811-8 informational frames | F7 |
| (C) R811-9 Write `path`/`file_text` aliases | F6 |
| (C) R77-1 `allowDangerouslySkipPermissions:false` | done (D-section X-rows) |
| (C) S-13 `sdk_host_only` idle regression | F12 |
| (C) Cancelled/errored permission `decisionClassification` ("Tool use aborted") | F10 |
| (C) 0.71-0.94 SKIP ranges (v2, session index, native subagents, auth/gateway/models/config, elicitation, hooks, terminal, replay, forks, notices) | F38-F40; F26-F28; F35-F37; F32-F34/F5/F11; F23; A-P3; F15; A-P3/F31; F29; F2 |
| (D) options `mcpServers`, `settings`, `hooks`, `plugins`, `sandbox`, `thinking`, `persistSession:false`, `CLAUDE_AGENT_SDK_VERSION` | F4 (`hooks` JSON keys + A-P3 3.1) |
| (D) P2 client elicitation / AskUserQuestion form / refusal fallback | F23 |
| (E) agent picker; fast-mode toggle | F11 |
| (E) `getAvailableModels` allowlist / `modelOverrides` / `deniedModels` / gateway narrowing | F5, F33 |
| (E) follow-up (d) model-row resume ordering | F12 |
| Phase-15 note: `toolResponse` refinement frame not reproduced | A-P3 3.1 |
| Reverse-audit: #596 usage-window cache across restart; #845 restore live model; "NOT reset on turn activation"/"user wants to know" | F5 (context-window cache); A-P2 (resumed model); F5 |
| verify-01 #1 malformed-line `error.data.line`; #2 no explicit `shutdown()`/`dispose()` | F2 (section 7 exception if the crate forces it); F18/F2 |
| verify-03 non-bug: permission request id int-vs-string | F2 / section 7 |
| INVARIANTS: INV-44 (unused number) | n/a |
| vs-275 findings P1-P3,P5,P8 (input:null) done in 4a54919; P4,P6 A-P2; P7,P9,P10 A-P3; P2b elicitation F23 | as listed |
| Cross-repo program: TEST-MAP zero `missing` | A-INFRA + F45; concurrency program F43; live smoke F44; Opus audit + vst re-run F46 |

## 9. Capability / RPC / notification matrix (upstream `initialize` + dispatch vs Rust HEAD)

### 9a. `initialize` response (acp-agent.ts:2963-3033)

| field | Rust HEAD | gap -> pkg |
|---|---|---|
| `protocolVersion:1` | match | - |
| `agentCapabilities._meta.claudeCode.promptQueueing`, `._meta.authStatus:{}` | match | - |
| `promptCapabilities{image,embeddedContext}` | match | - |
| `mcpCapabilities{http,sse}` | advertised but `mcpServers` ignored (inert) | F4 |
| `auth.logout:{}` | match, logout partial | F32 |
| `providers:{}` | advertised; 3 methods -32601 | F32 |
| `loadSession:true` | advertised; no history replay | A-P3 3.5 |
| `sessionCapabilities`: `additionalDirectories, close, delete, fork, list, resume, systemPrompt{mode:{override:{}}}` | match (`delete` -32601 for AIR; fork point errors) | F27, F29 |
| `sessionCapabilities.subagents:{}` (AIR) | advertised, inert | F35-F36 |
| `agentInfo{name,title:"Claude Agent",version}` | match (version deliberately composed, section 7) | - |
| `authMethods` | always `[]` | F2 |
| top-level `_meta.steering.supported` | match | - |
| `_meta.jetbrains.air.capabilities` (8) | match list | inert ones: A-AG3, F9, F11, F35 |
| `_meta.jetbrains.air` session-index family (5, only when client declared `sessionIndex`, never v2) | missing | F26 |
| `_meta.jetbrains.air.goal{version,controlMethod,actions}` | advertised, `_session/goal` -32601 | A-AG3 3.1 |

### 9b. Client -> agent requests and notifications

| method | upstream | Rust HEAD | pkg |
|---|---|---|---|
| `initialize` | yes | yes | F2 (authMethods) |
| `session/new`, `session/load`, `session/resume` | yes | yes (no replay, no fingerprint reuse, no `mcpServers`) | A-P3, F19, F4 |
| `session/fork` | yes (fork point) | latest only | F29 |
| `session/list`, `session/close` | yes | yes (no AIR list meta) | F26 |
| `session/delete` | yes (AIR archives) | non-AIR only | F27 |
| `session/set_mode`, `session/set_config_option` | yes | WT | A-P2 |
| `session/set_model` | NOT routed (-32601 in Node) | -32601 (match) | - |
| `authenticate` | yes | -32601 | F32 |
| `providers/list|set|disable` | yes | -32601 | F32 |
| `logout` | yes | partial | F32 |
| `session/prompt` (+ `$/cancel_request` cancel) | yes | yes (no `$/cancel_request`) | F2 |
| `session/cancel` (notification) | yes | yes | F16 |
| `_session/steering` | yes | yes | - |
| `_session/async_task/stop` | yes | yes | - |
| `_session/goal` | yes | -32601 | A-AG3 3.1 |
| `_session/list/subscribe`, `_session/list/unsubscribe` | yes (sessionIndex) | missing | F28 |
| `_session/rename`, `_session/archive`, `_session/unarchive` | yes (sessionIndex) | missing | F27 |
| v2 namespace (`CLAUDE_AGENT_ACP_EXPERIMENTAL_V2=1`) | yes | missing | F38-F40 |

### 9c. Agent -> client

| message | upstream | Rust HEAD | pkg |
|---|---|---|---|
| `session/update` kinds: agent/user/thought chunks, tool_call(_update), plan, available_commands_update, current_mode_update, config_option_update, usage_update, session_info_update | all | chunks/tool_call/commands/usage/mode/config done (WT: usage/mode/config); `plan` from TodoWrite already emitted (`map.rs:847` `plan_from_todo_write`), only the Task* plan is open (A-P3 3.4); `session_info_update` missing | A-P3, F30, F24, A-AG3 |
| `session/update` `notice` (`sessionUpdate:"notice"`) | yes | missing | F2 |
| `session/update` `compaction_update` | yes | missing | F20 |
| AIR updates `async_task_spawned/progress/state_update` | yes | yes | F15 (rest) |
| AIR/RFD updates `subagent_spawned/subagent_state_update/subagent_update/session_message` | yes | missing | F35-F36 |
| `session/request_permission` | yes | yes (options Yes/No + Bash durable) | A-P3 3.6, F10 |
| `elicitation/create` (request), `elicitation/complete` (notification) | yes | missing | F23 |
| `fs/read_text_file`, `fs/write_text_file` | wrappers exist, never invoked | missing | F3 |
| `_auth/status_update` | yes | yes | F34 |
| `_session/list/changes` | yes | missing | F28 |
| `_claude/sdkMessage` (`emitRawSDKMessages`) | yes | missing | F4 |
| `$/cancel_request` to the client on an aborted request | yes | missing | F3 |

### 9d. Adapter <-> `claude` control protocol

| subtype | direction | upstream use | Rust HEAD | pkg |
|---|---|---|---|---|
| `initialize` (hooks, promptSuggestions, systemPrompt, agents, skills, supportedDialogKinds, forwardSubagentText ...) | out | yes | partial (no hooks/promptSuggestions until A-P3) | A-P3, F4, F23 |
| `interrupt`, `cancel_async_message` | out | yes | yes | - |
| `set_model`, `set_permission_mode`, `apply_flag_settings`, `get_context_usage` | out | yes | WT | A-P2 |
| `stop_task` | out | yes (async stop, child cancel, workflows) | yes (async stop only) | F16, F36 |
| `rewind_files` | out | yes | missing | A-AG3 3.3 |
| `mcp_status`, `mcp_reconnect`, `mcp_toggle` | out | yes | missing | A-AG3 3.4 |
| `mcp_authenticate` | out | yes (OAuth) | missing | F23 |
| `get_usage` | out | yes (`/usage`) | missing | F24 |
| `remote_control` | out | yes | missing | F24 |
| `rename_session`, `generate_session_title {description,persist}` | out | yes | missing | F27a, F30 |
| `can_use_tool` | in | yes | yes | A-P3 3.6, F10 |
| `hook_callback` | in | yes | missing | A-P3 3.1 |
| `elicitation` (no handler) | in | SDK success `{action:"decline"}` (sdk.mjs `processControlRequest`) | Rust sends `{subtype:"error"}` without an `error` field (`control.rs:332-345`) | F3a; with handler F23b |
| `request_user_dialog` (no `onUserDialog` = client without `elicitation.form`) | in | NO response at all | Rust sends `{subtype:"error"}` | F3a; with handler F23b |
| `remote_tool_call`, `remote_plumbing_call`, `remote_tools_probe`, `remote_tools_reannounce` (`Z4`) | in | never answered ("leaving it unanswered") | Rust sends `{subtype:"error"}` | F3a |
| any other unsupported inbound subtype (`mcp_message`, `oauth_token_refresh`, `ui_*`, `remote_control_work_secret`) | in | error response WITH an `error` string, e.g. `"Unsupported control request subtype: X"` (or handler-specific "... callback is not provided") | `{subtype:"error"}` with no `error` field | F3a |
| duplicate in-flight `request_id` / request after close | in | skipped, no response | not skipped | F3a |
| stream frames `command_lifecycle`, `background_tasks_changed`, `active_goal`, `prompt_suggestion`, `tool_progress`, `conversation_reset`, `tool_use_summary`, `auth_status` | in | yes | `Route::Ignore` | F13, F14, A-AG3, F24, F7 |

## 10. Risk summary

- Riskiest (all of O/I/E/H/S): **F16** (cancel family), **F13** (command-lifecycle orphan map), **F19** (process-swap primitive), **F22** (clear-context hand-off), **F39** (v2 prompt-at-echo), **F12** (turn lanes), plus **A-AG3 3.2** (`TurnEvent::Failed.usage`).
- High: F14, F17, F18, F20, F21, F34, F35a-c, F36, F24, F25, F7b.
- Everything touching `turn.rs` or the settle path: F7a, F7b, F12, F13, F14, F16a-c, F17, F18, F19, F21, F22, F24, F25, F39 (+ A-AG3 3.2); keep the group strictly sequential (section 4 step 4).
- Evidence gating: none. All frames are authored from upstream test file:line (section 6); F1 only supplies fake-claude/recorder steps (including "expect no response"). Real recordings are optional-later (F44/F46).
