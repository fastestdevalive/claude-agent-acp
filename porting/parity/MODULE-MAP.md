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
| acp-subagents.ts | - | missing:F35b | AIR/RFD subagent event builders |
| air-extension.ts | air.rs | merged:air.rs | AIR extension |
| async-tasks.ts | async_tasks.rs | merged:async_tasks.rs | async task registry |
| auth-status.ts | auth_status.rs | 1:1 | auth status actor, epoch, probe context |
| clear-context-coordinator.ts | session/clear_context.rs | 1:1 | fresh-query clear-context hand-off (B-74/B-75 PARTIAL: Fast-mode reconcile after restart waits for F11) |
| client-system-prompt.ts | system_prompt.rs | 1:1 | client system prompt merge |
| context-compaction-meta.ts | - | missing:F20 | compaction meta lifecycle |
| context-compaction.ts | - | missing:F20 | compaction engine |
| diff.ts | diff.rs | 1:1 | line diff (Myers, context 3, budget), diff memo, git patch formatting and ACP v2 diff content ported (T1C, F8) |
| elicitation.ts | - | missing:F23a | ACP elicitation mapping |
| exit-plan.ts | session/exit_plan.rs | 1:1 | ExitPlanMode hand-off, approved plan, keep-planning interrupt |
| file-change-audit.ts | - | missing:AG-3 | file-change audit report |
| fork-session.ts | session_methods.rs | merged:session_methods.rs | fork-session helpers |
| gateway-models.ts | gateway_models.rs | 1:1 | gateway / Bedrock model listing over reqwest (D10) |
| goal-extension.ts | - | missing:AG-1 | `_session/goal` |
| hide-claude-auth.ts | hide_claude_auth.rs | 1:1 | account predicates, refusal errors, per-turn guard |
| index.ts | bin/claude-agent-acp-rs.rs | 1:1 | node entrypoint |
| lib.ts | lib.rs | different:Rust crate exports differ from the TS public surface (FX-4) | public exports |
| managed-policy.ts | - | missing:F5a | managed env tier and MDM/registry tiers |
| mcp-command.ts | - | missing:AG-4 | `/mcp` prompt |
| native-subagents.ts | - | missing:F35a | native subagent child-session registry |
| paths.ts | session_store.rs | different:path helpers replaced by session storage | path helpers |
| permissions/effects.ts | permission.rs | merged:permission.rs | permission effects |
| permissions/modes.ts | permission_mode.rs | merged:permission_mode.rs | permission modes |
| permissions/normalization.ts | permission.rs | merged:permission.rs | permission normalisation |
| permissions/options.ts | permission_options.rs | merged:permission_options.rs | option builders |
| permissions/options/filesystem.ts | permission_options.rs | merged:permission_options.rs | filesystem option builders |
| permissions/options/shared.ts | permission_options.rs | merged:permission_options.rs | shared option helpers |
| permissions/options/shell.ts | permission_options.rs | merged:permission_options.rs | shell option builders |
| permissions/options/tools.ts | permission_options.rs | merged:permission_options.rs | tool option builders |
| permissions/presentation.ts | - | missing:F10 | AIR permission presentation |
| permissions/response.ts | permission.rs | merged:permission.rs | permission response mapping |
| prompt-suggestions.ts | prompt_suggestions.rs | 1:1 | prompt suggestion text and meta (pure; wiring is F24 Turn 4) |
| resumed-session.ts | resumed_session.rs | merged:resumed_session.rs | resumed-session state |
| serve.ts | bin/claude-agent-acp-rs.rs | merged:bin/claude-agent-acp-rs.rs | serve entrypoint |
| session-config-ids.ts | session_config.rs | merged:session_config.rs | config ids |
| session-effort.ts | session_config.rs | merged:session_config.rs | effort config |
| session-failure-extension.ts | session_failure.rs | 1:1 | session failure controller, replay, advisory notices |
| session-index/activity.ts | session_index/activity.rs | 1:1 | session activity board (running/idle/requires_action/error, cost); session-side producers partly lane-A hooks (M-65 PARTIAL) |
| session-index/archive-title.ts | session_index/archive_title.rs | 1:1 | archive prefix, title precedence |
| session-index/connection.ts | session_index/connection.rs | 1:1 | index connection wrappers: delete, fork holds, archive (Q-42 PARTIAL) |
| session-index/dir-listing.ts | session_index/dir_listing.rs | 1:1 | directory listing caches |
| session-index/explicit-title.ts | session_index/explicit_title.rs | 1:1 | custom-title sidecar, explicit title |
| session-index/first-prompt.ts | session_index/first_prompt.rs | 1:1 | first-prompt title extraction |
| session-index/list-subscriptions.ts | session_index/list_subscriptions.rs | 1:1 | list subscriptions and changes pushes |
| session-index/live-registry.ts | session_index/live_registry.rs | 1:1 | live CLI registry reader |
| session-index/project-dirs.ts | session_index/project_dirs.rs | 1:1 | project dir encoding and match (darwin NFC normalisation PARTIAL) |
| session-index/service.ts | session_index/service.rs | 1:1 | list cursor and scope, rename/archive/delete service |
| session-index/session-index.ts | session_index/mod.rs | 1:1 | index core, ordering, cwd filter |
| session-index/session-info.ts | session_index/session_info.rs | 1:1 | session-info rows and `_meta` |
| session-index/transcript-scan.ts | session_index/transcript_scan.rs | 1:1 | transcript scan, continued-in-session detection |
| session-index/worktrees.ts | session_index/worktrees.rs | 1:1 | git worktree listing without spawning git |
| session-mode.ts | permission_mode.rs | merged:permission_mode.rs | session mode |
| session-model.ts | session_config.rs | merged:session_config.rs | session model |
| session-notices.ts | - | missing:F2a | notice fallback |
| session-timing.ts | - | missing:F2b | session timing log lines |
| session-titles.ts | session_titles.rs | 1:1 | title generation state, context ring, client-title guard |
| settings.ts | settings.rs | merged:settings.rs | settings tiers |
| subagent-history.ts | - | missing:F37 | subagent transcript reader |
| subagent-replay.ts | - | missing:F37 | subagent replay |
| tool-calls/background.ts | tools/background.rs | 1:1 | backgrounded Bash tool-call shaping |
| tool-calls/changed-meta-filter.ts | meta_filter.rs | 1:1 | changed-meta filter |
| tool-calls/client-capabilities.ts | agent.rs | merged:agent.rs | client capabilities |
| tool-calls/content.ts | map.rs | merged:map.rs | content rendering |
| tool-calls/facts.ts | tools.rs | merged:tools.rs | tool facts |
| tool-calls/field-tracker.ts | map.rs | merged:map.rs | field tracker |
| tool-calls/renderer.ts | map.rs | merged:map.rs | renderer |
| tool-calls/reporters/agent.ts | tools.rs | merged:tools.rs | agent reporter |
| tool-calls/reporters/bash.ts | tools.rs | merged:tools.rs | bash reporter |
| tool-calls/reporters/compaction.ts | - | missing:F20 | legacy compaction tool call |
| tool-calls/reporters/file-edit.ts | tools.rs | merged:tools.rs | file-edit reporter |
| tool-calls/reporters/index.ts | tools.rs | merged:tools.rs | reporter registry |
| tool-calls/reporters/interaction.ts | tools.rs | merged:tools.rs | interaction reporter |
| tool-calls/reporters/read.ts | tools.rs | merged:tools.rs | read reporter |
| tool-calls/reporters/search.ts | tools.rs | merged:tools.rs | search reporter |
| tool-calls/reporters/web.ts | tools.rs | merged:tools.rs | web reporter |
| tool-result-meta.ts | tool_meta.rs | 1:1 | tool result meta parsing |
| tools.ts | tools.rs | merged:tools.rs | tool definitions |
| turn-events.ts | turn.rs | merged:turn.rs | turn events |
| usage-markdown.ts | usage_markdown.rs | 1:1 | `/usage` markdown formatter (pure; wiring is F24 Turn 4) |
| utils.ts | - | missing:F31 | EventLoopYielder (B-17/B-55) |
| v2/agent.ts | - | missing:F38b | ACP v2 surface |
| v2/initialize.ts | - | missing:F38a | ACP v2 surface |
| v2/permission.ts | - | missing:F40 | ACP v2 surface |
| v2/prompt.ts | - | missing:F39 | ACP v2 surface |
| v2/session-update.ts | - | missing:F39 | ACP v2 surface |
| v2/session.ts | - | missing:F38a | ACP v2 surface |
| v2/terminal.ts | - | missing:F40 | ACP v2 surface |
| v2/tool-call-meta.ts | - | missing:F40 | ACP v2 surface |
| v2/tool-call.ts | - | missing:F40 | ACP v2 surface |
