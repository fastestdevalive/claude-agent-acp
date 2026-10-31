# Shared Corpus Alignment (Cross-Repo Parity)

Mapping of the 23 shared corpus scenarios (defined in `porting/parity/PARITY-PROGRAM.md` and shared with the Codex ACP port) to scenario scripts in this repository.

| shared name | scenario file here | status (exact \| alias \| new) | note |
|---|---|---|---|
| plain | porting/corpus/text-only.acp.json | alias | Basic text turn: single prompt expecting plain assistant text and success result |
| tools_shell | porting/corpus/tool-shapes.acp.json | alias | Covers shell (Bash), Read, Edit, and Write tool calls with permissions and responses |
| edit_diff | porting/corpus/edit-diff-preview.acp.json | alias | Edit tool call with structuredPatch unified diff emission under PostToolUse hook |
| write_new | porting/corpus/write-new-file-diff.acp.json | alias | Write tool call creating a new file with structuredPatch diff updates |
| permission_allow | porting/corpus/permission-allow.acp.json | alias | Permission request evaluated and answered with allow decision |
| permission_deny | porting/corpus/permission-deny.acp.json | alias | Permission request evaluated and answered with deny decision |
| cancel_mid_tool | porting/corpus/cancel-during-tool.acp.json | alias | Session cancellation while tool execution is in flight |
| cancel_before_turn | porting/corpus/cancel-queued-echo-less.acp.json | alias | Cancellation of a queued turn before execution starts |
| steer_mid_turn | porting/corpus/steer-mid-turn.acp.json | alias | Steering message injected during an active streamed turn |
| queued_prompt | porting/corpus/queued_prompt.acp.json | new | Two back-to-back prompts with no_wait; second prompt queues and runs after first turn finishes |
| resume_recall | porting/corpus/resume_recall.acp.json | new | Memory stored in prompt 1, session closed and resumed, recall asserted in prompt 2 |
| session_load_replay | porting/corpus/session-load-replay.acp.json | alias | Load existing session with seeded history and replay turns |
| config_options_model_mode | porting/corpus/config-session-new.acp.json | alias | Model and mode configuration options advertised in session/new |
| set_config_option | porting/corpus/config-set-model.acp.json | alias | Updating config options via session/set_config_option RPC |
| image_input | porting/corpus/image-prompt-echo.acp.json | alias | Multi-modal prompt carrying base64 image content |
| slash_commands_skills | porting/corpus/commands-proactive.acp.json | alias | Slash commands listed in session and executed |
| usage_update | porting/corpus/usage-update.acp.json | alias | Token usage tracking and metadata updates on turn completion |
| background_after_prompt | porting/corpus/background_after_prompt.acp.json | new | Background task started during turn; full lifecycle (task_started, turn result) diffs clean against the Node fixture — tests/shared_corpus.rs::shared_background_after_prompt |
| auth_failure | porting/corpus/auth-required.acp.json | alias | Unauthenticated session state yielding error result requiring login |
| session_list_close_delete_fork | porting/corpus/session_list_close_delete_fork.acp.json | new | Session lifecycle methods (list, close, delete, fork-after-close error) diff clean against the Node fixture — tests/shared_corpus.rs::shared_session_list_close_delete_fork |
| initialize_capabilities | porting/corpus/initialize-capabilities-plain.acp.json | alias | Protocol version negotiation and client/agent capabilities handshake |
| mcp_stdio | porting/corpus/mcp-config-stdio.acp.json | alias | MCP server configured over stdio in session initialization |
| subagent | porting/corpus/subagent.acp.json | new | Task tool call launches a child agent; child lifecycle frames diff clean against the Node fixture — tests/shared_corpus.rs::shared_subagent |
