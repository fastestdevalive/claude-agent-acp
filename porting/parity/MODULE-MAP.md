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
| auth-status.ts | auth_status.rs | merged:auth_status.rs | auth status actor |
| clear-context-coordinator.ts | - | missing:F22 | fresh-query clear-context hand-off |
| client-system-prompt.ts | system_prompt.rs | 1:1 | client system prompt merge |
| context-compaction-meta.ts | - | missing:F20 | compaction meta lifecycle |
| context-compaction.ts | - | missing:F20 | compaction engine |
| diff.ts | diff.rs | 1:1 | line diff (Myers, context 3, budget), diff memo, git patch formatting and ACP v2 diff content ported (T1C, F8) |
| elicitation.ts | - | missing:F23a | ACP elicitation mapping |
| exit-plan.ts | - | missing:F21 | ExitPlanMode hand-off |
| file-change-audit.ts | - | missing:AG-3 | file-change audit report |
| fork-session.ts | session_methods.rs | merged:session_methods.rs | fork-session helpers |
| gateway-models.ts | - | missing:F33 | gateway / Bedrock model listing |
| goal-extension.ts | - | missing:AG-1 | `_session/goal` |
| hide-claude-auth.ts | - | missing:F34 | account identity / hide-claude-auth |
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
| prompt-suggestions.ts | - | missing:F24 | promptSuggestions frame + publishing |
| resumed-session.ts | resumed_session.rs | merged:resumed_session.rs | resumed-session state |
| serve.ts | bin/claude-agent-acp-rs.rs | merged:bin/claude-agent-acp-rs.rs | serve entrypoint |
| session-config-ids.ts | session_config.rs | merged:session_config.rs | config ids |
| session-effort.ts | session_config.rs | merged:session_config.rs | effort config |
| session-failure-extension.ts | - | missing:AG-2 | sessionFailure capability |
| session-index/activity.ts | - | missing:F26b | activity tracking |
| session-index/archive-title.ts | - | missing:F30 | title precedence and sanitize |
| session-index/connection.ts | - | missing:F26b | delete and fork-point wrappers |
| session-index/dir-listing.ts | - | missing:F26a | directory listing scanner |
| session-index/explicit-title.ts | - | missing:F27a | explicit rename title |
| session-index/first-prompt.ts | session_store.rs | merged:session_store.rs | first-prompt indexing |
| session-index/list-subscriptions.ts | - | missing:F28 | session list subscriptions |
| session-index/live-registry.ts | - | missing:F26b | live session registry |
| session-index/project-dirs.ts | session_store.rs | merged:session_store.rs | project dirs |
| session-index/service.ts | - | missing:F26b | list cursor and scope |
| session-index/session-index.ts | session_store.rs | merged:session_store.rs | index core |
| session-index/session-info.ts | - | missing:F26b | session-info rows and `_meta` |
| session-index/transcript-scan.ts | session_store.rs | merged:session_store.rs | transcript scan |
| session-index/worktrees.ts | - | missing:F26a | worktree listing |
| session-mode.ts | permission_mode.rs | merged:permission_mode.rs | session mode |
| session-model.ts | session_config.rs | merged:session_config.rs | session model |
| session-notices.ts | - | missing:F2a | notice fallback |
| session-timing.ts | - | missing:F2b | session timing log lines |
| session-titles.ts | - | missing:F30 | title sanitisation |
| settings.ts | settings.rs | merged:settings.rs | settings tiers |
| subagent-history.ts | - | missing:F37 | subagent transcript reader |
| subagent-replay.ts | - | missing:F37 | subagent replay |
| tool-calls/background.ts | - | missing:F15 | backgrounded Bash tool-call shaping |
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
| tool-result-meta.ts | - | missing:F7a | tool result meta parsing |
| tools.ts | tools.rs | merged:tools.rs | tool definitions |
| turn-events.ts | turn.rs | merged:turn.rs | turn events |
| usage-markdown.ts | - | missing:F24 | `/usage` markdown formatter |
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
