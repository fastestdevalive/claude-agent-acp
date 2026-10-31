# MODULE-MAP — claude-agent-acp upstream module mapping (snapshot v0.94.0)

One row per upstream source module under `src/` (91 files; `acp-agent.ts` split
into its 17 anchor-keyed sections) mapped to the Rust module(s) in
`rust/claude-agent-acp-rs/src/`, with the relationship. Checked by
`rust/scripts/parity/module-map-check`; run it with `--upstream` to validate the
`acp-agent.ts@` section anchors. Missing behaviour is `missing:<roadmap package>`
(the package id used by `porting/parity/FEATURES.md`); its ticket line lives in
`MISSING.md`.

| upstream module | rust module(s) | relationship | note |
|---|---|---|---|
| acp-agent.ts@top | agent.rs, session.rs, dispatch.rs, control.rs | split:agent.rs | module helpers, client wrappers, MCP auth |
| acp-agent.ts@ClaudeAcpAgent | agent.rs, session_methods.rs, session_store.rs, resumed_session.rs | split:agent.rs | ctor, initialize, new/fork/resume/load/list |
| acp-agent.ts@authenticate | auth_status.rs, session_ext.rs | split:auth_status.rs | authenticate arm missing (F32); auth status/logout in auth_status.rs |
| acp-agent.ts@prompt | turn.rs, session.rs, control.rs | split:turn.rs | startTurn, goal, steer, stopAsyncTask |
| acp-agent.ts@runConsumer | turn.rs, map.rs, session.rs | split:turn.rs | stream loop |
| acp-agent.ts@trackOrphanCommand | session.rs, control.rs, process.rs | split:session.rs | cancel, teardown, close, delete |
| acp-agent.ts@setSessionMode | session_config.rs, permission_mode.rs | split:session_config.rs | set mode / config option |
| acp-agent.ts@replaySessionHistory | resumed_session.rs, transcript.rs, session_methods.rs | split:resumed_session.rs | history replay |
| acp-agent.ts@readTextFile | agent.rs | merged:agent.rs | fs/foreground/autonomous bookkeeping |
| acp-agent.ts@requestPermissionFromClient | permission.rs, permission_options.rs | split:permission.rs | permission + elicitation + canUseTool |
| acp-agent.ts@updateConfigOption | session_config.rs | merged:session_config.rs | config option application, fast mode |
| acp-agent.ts@getOrCreateSession | session.rs, session_store.rs, resumed_session.rs | split:session.rs | incl. createSession |
| acp-agent.ts@shouldEmitRawMessage | quota.rs, usage.rs | split:usage.rs | usage/quota/provider helpers |
| acp-agent.ts@discoverCustomAgents | settings.rs, sdk_options.rs | split:settings.rs | buildConfigOptions/commands |
| acp-agent.ts@promptToClaude | turn.rs, tools.rs | split:turn.rs | prompt conversion + tool-call bookkeeping |
| acp-agent.ts@toAcpNotifications | map.rs, tools.rs | split:map.rs | notification mapping + streamEvent |
| acp-agent.ts@runPromptWithCancellation | turn.rs, agent.rs | split:turn.rs | runPrompt/v1AgentApp/runAcp/context-window |
| acp-subagents.ts | acp_subagents.rs | 1:1 | AIR/RFD subagent event builders (T4B); announce/route hooks into session.rs finish with U-11/U-68 |
| air-extension.ts | air.rs | merged:air.rs | AIR extension; every export verified at its call site (Q-03 DONE) |
| async-tasks.ts | async_tasks.rs | merged:async_tasks.rs | async task registry; routeOf wired through subagent routing (Q-04 DONE) |
| auth-status.ts | auth_status.rs | 1:1 | auth status actor, epoch, probe context |
| clear-context-coordinator.ts | session/clear_context.rs | 1:1 | fresh-query clear-context hand-off (B-74/B-75 PARTIAL: Fast-mode reconcile after restart waits for F11) |
| client-system-prompt.ts | system_prompt.rs | 1:1 | client system prompt merge |
| context-compaction-meta.ts | context_compaction_meta.rs | 1:1 | AIR contextCompaction meta (T5A) |
| context-compaction.ts | context_compaction.rs | 1:1 | compaction lifecycle, summary handling, boundary metadata (T5A) |
| diff.ts | diff.rs | 1:1 | line diff (Myers, context 3, budget), diff memo, git patch formatting and ACP v2 diff content ported (T1C, F8) |
| elicitation.ts | elicitation.rs | 1:1 | ACP elicitation mapping, AskUserQuestion form, refusal_fallback dialog, MCP OAuth (T4C) |
| exit-plan.ts | session/exit_plan.rs | 1:1 | ExitPlanMode hand-off, approved plan, keep-planning interrupt |
| file-change-audit.ts | file_change_audit.rs | 1:1 | file-change audit report (T6A2) |
| fork-session.ts | fork_session.rs, message_ids.rs, session_methods.rs | split:fork_session.rs | fork point parse, fork operation, messageId map (T5B) |
| gateway-models.ts | gateway_models.rs | 1:1 | gateway / Bedrock model listing over reqwest (D10) |
| goal-extension.ts | goal.rs | 1:1 | `_session/goal` goal extension (AG-1) |
| hide-claude-auth.ts | hide_claude_auth.rs | 1:1 | account predicates, refusal errors, per-turn guard |
| index.ts | bin/claude-agent-acp-rs.rs | 1:1 | node entrypoint |
| lib.ts | lib.rs | different:Rust crate exports differ from the TS public surface (FX-4) | public exports |
| managed-policy.ts | settings.rs | merged:settings.rs | managed env tier and MDM/registry tiers (F5a) |
| mcp-command.ts | mcp_command.rs | 1:1 | `/mcp` prompt (AG-4) |
| native-subagents.ts | native_subagents.rs, native_subagents/runtime.rs, async_tasks/subagents.rs | split:native_subagents.rs | native subagent child-session registry; history lookups wired (Q-22 DONE) |
| paths.ts | session_store.rs | different:path helpers replaced by session storage | path helpers |
| permissions/effects.ts | permission_effects.rs | 1:1 | permission effects (T6A2) |
| permissions/modes.ts | permission_mode.rs | merged:permission_mode.rs | permission modes |
| permissions/normalization.ts | permission.rs | merged:permission.rs | permission normalisation |
| permissions/options.ts | permission_options.rs | merged:permission_options.rs | option builders |
| permissions/options/filesystem.ts | permission_options.rs | merged:permission_options.rs | filesystem option builders |
| permissions/options/shared.ts | permission_options.rs | merged:permission_options.rs | shared option helpers |
| permissions/options/shell.ts | permission_options.rs | merged:permission_options.rs | shell option builders |
| permissions/options/tools.ts | permission_options.rs | merged:permission_options.rs | tool option builders |
| permissions/presentation.ts | tools.rs, permission.rs | split:tools.rs | permission presentation (shared title helper in tools.rs, T6A2) |
| permissions/response.ts | permission_response.rs | 1:1 | permission response mapping (T6A2) |
| prompt-suggestions.ts | prompt_suggestions.rs | 1:1 | prompt suggestion text and meta (pure; wiring is F24 Turn 4) |
| resumed-session.ts | resumed_session.rs | merged:resumed_session.rs | resumed-session state |
| serve.ts | bin/claude-agent-acp-rs.rs, v2/serve.rs | split:bin/claude-agent-acp-rs.rs | serve entrypoint; protocol router in v2/serve.rs (T5C) |
| session-config-ids.ts | session_config.rs | merged:session_config.rs | config ids |
| session-effort.ts | session_config.rs | merged:session_config.rs | effort config |
| session-failure-extension.ts | session_failure.rs | 1:1 | session failure controller, replay, advisory notices |
| session-index/activity.ts | session_index/activity.rs | 1:1 | session activity board (running/idle/requires_action/error, cost); session-side producers partly lane-A hooks (M-65 PARTIAL) |
| session-index/archive-title.ts | session_index/archive_title.rs | 1:1 | archive prefix, title precedence |
| session-index/connection.ts | session_index/connection.rs | 1:1 | index connection wrappers: delete, fork holds, archive; handlers and call sites wired (Q-42 DONE) |
| session-index/dir-listing.ts | session_index/dir_listing.rs | 1:1 | directory listing caches |
| session-index/explicit-title.ts | session_index/explicit_title.rs | 1:1 | custom-title sidecar, explicit title |
| session-index/first-prompt.ts | session_index/first_prompt.rs | 1:1 | first-prompt title extraction |
| session-index/list-subscriptions.ts | session_index/list_subscriptions.rs | 1:1 | list subscriptions and changes pushes |
| session-index/live-registry.ts | session_index/live_registry.rs | 1:1 | live CLI registry reader |
| session-index/project-dirs.ts | session_index/project_dirs.rs, session_index/nfc.rs, session_index/nfc_data.rs | 1:1 | project dir encoding and match; darwin NFC via UAX #15 tables (M-74/Q-48 DONE) |
| session-index/service.ts | session_index/service.rs | 1:1 | list cursor and scope, rename/archive/delete service |
| session-index/session-index.ts | session_index/mod.rs | 1:1 | index core, ordering, cwd filter |
| session-index/session-info.ts | session_index/session_info.rs | 1:1 | session-info rows and `_meta` |
| session-index/transcript-scan.ts | session_index/transcript_scan.rs | 1:1 | transcript scan, continued-in-session detection |
| session-index/worktrees.ts | session_index/worktrees.rs | 1:1 | git worktree listing without spawning git |
| session-mode.ts | permission_mode.rs | merged:permission_mode.rs | session mode |
| session-model.ts | session_config.rs | merged:session_config.rs | session model |
| session-notices.ts | notices.rs | 1:1 | notice fallback (F2a) |
| session-timing.ts | session_timing.rs | 1:1 | session timing log lines (F2b) |
| session-titles.ts | session_titles.rs | 1:1 | title generation state, context ring, client-title guard |
| settings.ts | settings.rs | merged:settings.rs | settings tiers |
| subagent-history.ts | subagent_history.rs | 1:1 | subagent transcript reader (T6B) |
| subagent-replay.ts | subagent_replay.rs | 1:1 | subagent replay (T6B) |
| tool-calls/background.ts | tools/background.rs | 1:1 | backgrounded Bash tool-call shaping |
| tool-calls/changed-meta-filter.ts | meta_filter.rs | 1:1 | changed-meta filter |
| tool-calls/client-capabilities.ts | client_caps.rs | 1:1 | full capability parse (terminal, AIR, notices, compaction, auth, elicitation); C-25 |
| tool-calls/content.ts | tools.rs | merged:tools.rs | content rendering (`textContent`, `toAcpContentUpdate`, `markdownEscape`, `toDisplayPath`, `resultText`, `structuredResult`) |
| tool-calls/facts.ts | tools.rs | merged:tools.rs | tool facts |
| tool-calls/field-tracker.ts | map.rs | merged:map.rs | field tracker (`MapState::TrackedToolCall`: changed-field filter, merged `_meta`, pinned approval content, entry lifetime; U-25/U-30/U-36) |
| tool-calls/renderer.ts | map.rs, tools.rs, permission.rs | merged:map.rs | renderer: reporter-facts dispatch in map.rs, `toolShape`/`rawInput`/`toolUseContent` facts in tools.rs, `permissionToolCall` in permission.rs; AIR `rawInputRendering`/`planFile` gated on the client capabilities (T-03/T-04/T-85/T-86) |
| tool-calls/reporters/agent.ts | tools.rs | merged:tools.rs | agent reporter |
| tool-calls/reporters/bash.ts | tools.rs | merged:tools.rs | bash reporter |
| tool-calls/reporters/compaction.ts | tool_reporters_compaction.rs | 1:1 | compaction tool call reporter (T5A) |
| tool-calls/reporters/file-edit.ts | tools.rs, hooks.rs, diff.rs | merged:tools.rs | file-edit reporter: Write/Edit facts and diffs, NotebookEdit AIR rendering (T-46), `finalChange` exact patch in hooks.rs |
| tool-calls/reporters/index.ts | tools.rs | merged:tools.rs | reporter registry |
| tool-calls/reporters/interaction.ts | tools.rs, map.rs | merged:tools.rs | interaction reporter: ExitPlanMode (incl. AIR planFile path and plan-file clear call, T-59/T-60) in tools.rs + map.rs |
| tool-calls/reporters/read.ts | tools.rs | merged:tools.rs | read reporter |
| tool-calls/reporters/search.ts | tools.rs | merged:tools.rs | search reporter |
| tool-calls/reporters/web.ts | tools.rs | merged:tools.rs | web reporter |
| tool-result-meta.ts | tool_meta.rs | 1:1 | tool result meta parsing |
| tools.ts | tools.rs | merged:tools.rs | tool definitions |
| turn-events.ts | turn_events.rs | 1:1 | TurnEvents / ForegroundEvents (T6A) |
| usage-markdown.ts | usage_markdown.rs | 1:1 | `/usage` markdown formatter (pure; wiring is F24 Turn 4) |
| utils.ts | event_loop_yield.rs | 1:1 | EventLoopYielder / messageLoopYielder (T5B; B-17 race tests in T7) |
| v2/agent.ts | v2/agent.rs | 1:1 | v2 agent surface and router (T6B) |
| v2/initialize.ts | v2/initialize.rs | 1:1 | v2 initialize mapping and response (T5C) |
| v2/permission.ts | v2/permission.rs | 1:1 | v2 permission mapping (T6C) |
| v2/prompt.ts | v2/prompt.rs | 1:1 | v2 prompt path (T6A) |
| v2/session-update.ts | v2/session_update.rs | 1:1 | v2 session update translation (T6C) |
| v2/session.ts | v2/session.rs | 1:1 | v2 session translation: config shape, extension methods, tool call statuses (T5C) |
| v2/terminal.ts | v2/terminal.rs | 1:1 | v2 display terminals (T6C) |
| v2/tool-call-meta.ts | v2/tool_call_meta.rs | 1:1 | v2 tool-call meta patches (T6C) |
| v2/tool-call.ts | v2/tool_call.rs | 1:1 | v2 tool-call form (T6C) |
