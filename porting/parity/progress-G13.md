# G13 — T1 leftovers sweep progress

Worktree: `/private/tmp/claude-501/par-c` @ f4e0c15 (feat/rust-port). Group G13.

## Checklist

- [x] FX-3: M-51 - messageIdForGrouping: assistant keyed by API message.id, else uuid
- [x] FX-3: N-48 - stream content_block_stop drops only that block's streamed input
- [x] FX-4: O-83 - lib.ts public exports (ClaudeAcpAgent, runAcp, toAcpNotifications, SettingsManager, AUTH_STATUS_UPDATE_METHOD, ...)
- [x] FX-4: Q-19 - module lib.ts
- [x] FX-4: O-138 - readTextFile / writeTextFile passthrough on the public library agent
- [x] FX-5: O-37 - versioned model display names (family + version, context suffix placement)
- [x] F6a: M-46 - promptToClaude: resource_link and resource text as link via formatUriAsLink
- [x] F6a: O-155 - _meta.claudeCode.options.toolAliases
- [x] F6a: O-161 - _meta.claudeCode.options.toolConfig
- [x] F6a: Q-68 - module tool-calls/renderer.ts (PARTIAL - verified remaining gaps owned by F15/F9/T-86)
- [x] F6a: Q-69 - module tool-calls/reporters/agent.ts (PARTIAL - verified toolResult trailer/hand-back owned by F7a T-54)
- [x] F6a: Q-70 - module tool-calls/reporters/bash.ts (PARTIAL - verified terminal exit-code/channel owned by F15 T-31/T-33)
- [x] F6a: Q-74 - module tool-calls/reporters/interaction.ts (PARTIAL - verified ExitPlanMode AIR planFile owned by F9 T-59/T-60)
- [x] F6a: Q-77 - module tool-calls/reporters/web.ts (PARTIAL - verified WebSearch hit result owned by F7a T-51)
- [x] F6a: Q-79 - module tools.ts (PARTIAL - verified remaining gaps owned by F7a and F9)
- [x] F6a: T-86 - non-AIR display copy of input in content, not next to a terminal (PARTIAL - verified remaining gap owned by session.rs AIR gate & F15)
- [x] F3a: M-38 - session/set_config_option mode branch: validate value, alias via options/currentValue, Auto clamp, current_mode_update, no config_option_update (PARTIAL - F3a scope complete; remaining gaps outside F3a)
- [x] F3a: M-39 - session/set_mode: parse, availability, Auto clamp, current_mode_update when clamped, config_option_update (PARTIAL - F3a scope complete; remaining gaps outside F3a)
- [x] F3a: M-42 - set_config_option effort: apply flag first, default un-pins, other values pin, no config_option_update (PARTIAL - F3a scope complete; remaining gaps outside F3a)
- [x] F2a: C-23 - command display terminal for terminal-capable clients (terminal_info, output, exit) (PARTIAL - renderer wiring owned by F15)
- [x] F5b: O-74 - Model picker narrowed to gateway-listed models (PARTIAL - narrowing wired; gated on providers/set F33)
- [x] AG-4: S-02 - /mcp structured markdown, reconnect and enable/disable through control API, OAuth follow-up (PARTIAL - status/reconnect/enable/disable ported; OAuth follow-up owned by F23c)
- [x] LD-3: T-40 - Write input diff: oldText null, newText content; gated off for diffPatch clients, v2, and replay keeps it (PARTIAL - aliases ported; wire oldText:null is phase 3 Decision-4 materialiser)
- [x] LD-3: T-42 - Edit input diff: old_string and new_string; oldText null when old_string empty (PARTIAL - typed diff ported; wire oldText:null is phase 3 Decision-4 materialiser)
- [x] LD-3: T-78 - tool_call shape: name, status pending, title, kind, content, locations, rawInput (PARTIAL - fields set; wire omission of status:pending is phase 3 Decision-4 materialiser)

## Implementation & Test Evidence

### FX-3: M-51, N-48
- Stamped `message_id_of(frame)` on user frames in `map.rs` (`map_user_frame` and `map_user_frame_block`).
- Made `content_block_stop` remove only its own block index `state.streamed_inputs.remove(&(stream_key, idx))` instead of clearing all inputs for the stream lane.
- Created corpus scenario: `porting/corpus/stream-user-chunks.acp.json` and captured against Node oracle: `bash porting/capture.sh stream-user-chunks` -> `porting/fixtures/stream-user-chunks.frames.jsonl`.
- Added differential test `inv_24_stream_user_chunks` in `tests/differential.rs`.
- Unit tests: `map::tests::content_block_stop_drops_only_its_block`, `map::tests::m51_user_chunk_message_id_grouping`.
- Status in FEATURES.md: flipped to `DONE`.

### FX-4: O-83, Q-19, O-138
- Re-exported public API surface in `lib.rs`: `ClaudeAcpAgent`, `serve`, `run_acp`, `serve_handle`, `ServeHandle`, `ServeOptions`, `SettingsManager`, `SettingsManagerOptions`, `AuthStatus`, `AuthStatusAccount`, `AUTH_STATUS_UPDATE_METHOD`, `to_acp_notifications`, `to_display_path`, `tool_info_from_tool_use`, `plan_entries`, `ClaudePlanEntry`.
- Added `ClaudeAcpAgent::read_text_file` and `ClaudeAcpAgent::write_text_file` delegating to connection requests in `agent.rs`.
- Added unit test in `agent.rs`: `inv_o138_claude_acp_agent_passthrough`.
- Added integration test in `tests/parity_maps.rs`: `inv_75_lib_rs_export_surface`.
- Status in FEATURES.md: flipped O-83, Q-19, O-138 to `DONE`.

### FX-5: O-37
- Added unit test `session_model::tests::model_presentation_goldens` covering all golden cases from upstream `src/tests/model-presentation.test.ts`.
- Status in FEATURES.md: flipped to `DONE`.

### F6a: M-46, O-155, O-161, Q-68, Q-69, Q-70, Q-74, Q-77, Q-79, T-86
- M-46: Updated `format_uri_as_link` in `agent.rs` to match upstream `formatUriAsLink`: returns `[@name](uri)` for `file://` and `zed://`, and returns the raw `uri` otherwise. Added unit test `test_format_uri_as_link`.
- O-155 & O-161:
  - Added `pub tool_aliases: Option<Value>` and `pub tool_config: Option<Value>` to `ClaudeOptions` in `sdk_options.rs`.
  - Parsed `toolAliases` and `toolConfig` in `claude_options`.
  - Added `pub tool_config: Option<Value>` to `InitializeOptions` in `control.rs` and serialized both `toolAliases` and `toolConfig` in `build_request()`.
  - Passed both fields into `InitializeOptions` in `agent.rs::initialize_options_for`.
  - Added unit tests: `sdk_options::tests::inv_o155_o161_tool_aliases_and_tool_config` and `control::tests::initialize_request_carries_tool_aliases_and_tool_config`.
- Q-68, Q-69, Q-70, Q-74, Q-77, Q-79, T-86: Audited and verified. Remaining gaps are accurately assigned to F7a, F9, F15.
- Status in FEATURES.md: flipped M-46, O-155, O-161 to `DONE`.

### F3a: M-38, M-39, M-42
- Audited against `session.rs`. F3a control response handling is complete; remaining gaps are outside F3a.

### F2a (C-23), F5b (O-74), AG-4 (S-02), LD-3 (T-40, T-42, T-78)
- Audited against current codebase.
- C-23 terminal renderer is part of F15.
- O-74 gateway narrowing is gated on providers/set (F33).
- S-02 OAuth elicitation is part of F23c.
- T-40, T-42, T-78 wire nulls/pending are handled by Decision-4 materialiser (phase 3).

READY:G13
