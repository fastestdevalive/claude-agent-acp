# Live Node-vs-Rust findings (run of 2026-10-23, HEAD 8a28082)

Method: vibe-station's driver (`scripts/parity/live-acp-scenarios.mjs`, 17 scenarios, real `claude`, haiku)
run against upstream 0.94.0 under bun and against the Rust release binary; transcripts compared
semantically (timings, token counts, wording, ids, temp paths ignored). Raw output lives outside the repo
(scratch); this file is the durable summary. Every row becomes a regression test (corpus scenario or unit
test) in package `rust-live-divergences`.

| ID | Sev | Node | Rust | Likely location |
|---|---|---|---|---|
| D1 | HIGH | `usage_update.size` = 1000000 from the first frame (vst sends `betas:["context-1m-2025-08-07"]`) | 200000 on every first-turn frame; 1000000 only on the final cost frame | `session.rs` window seed (InitConfig) and the background `get_context_usage` (agent.rs ~722); cause to be established from Node's source (betas-driven window?) |
| D2 | MED | initial `tool_call` carries `status:"pending"`, `content:[]` (and `locations:[]` for Read/Edit/Write) | omits all three (pinned ACP crate skips defaults) | `map.rs` ToolCall serialisation; materialise defaults on the wire and drop the differential's default-normaliser |
| D3 | MED | tool/permission titles use relative paths (`Edit a.txt`) | absolute paths | `tools.rs:451` `display_path` stub |
| D4 | MED | Read content rebuilt from the structured result | raw text (adds `4\t` line numbers, can leak system-reminders) | `tools.rs:810` |
| D5 | MED | second `available_commands_update` (79 commands: drops `color`, `doctor`, `focus`, `reload-plugins` = `terminal_slash_commands`) | stays at 83 | missing `terminal_slash_commands` handling |
| D6 | MED | emits `session_info_update` (AI session title) | never | `map.rs:13` marks it skipped |
| D7 | LOW | JSON-RPC id of `session/request_permission` is an integer | UUID string | transport wrapper (crate assigns the id) |
| D8 | LOW | Write diff content has `oldText:null` | omitted | `map.rs` / tool content |
| D9 | LOW | env key `CLAUDE_AGENT_SDK_VERSION` set on the child | absent | `process.rs` / `sdk_options.rs` |
| D10 | LOW | 3 `auth status` probes, 3 `_auth/status_update` pushes per scenario | 1 probe, 2 pushes | `auth_status.rs` probe schedule |
| D11 | LOW | every `/context` chunk has `messageId` | second chunk has none | `map.rs` local-command output |
| D12 | n/a | `agentInfo.version` `0.94.0` | `0.1.0+upstream.0.94.0` | deliberate (user-required) |
| D13 | n/a | steer reply wording | differs | model nondeterminism |
| D14 | LOW | untested: subagent `parentToolUseId` attribution (model made no child tool call in the Rust run) | — | needs a deterministic corpus scenario |

Identical in the live runs: initialize result, session/new, session/load, the session spawn argv,
permission option sets.

Coverage gaps of the driver (to be closed by corpus scenarios): Bash was auto-allowed in both runs so the
permission-deny path was not exercised live; `sleep` was blocked by the CLI before the cancel arrived so
cancel-during-tool was not truly exercised.

## Resolutions in LD-3 (Group G14)

- **D2 (resolved):** Wire-level typed-update materialiser `map::materialize_tool_call_defaults` ensures initial `tool_call` carries `status:"pending"`, `content:[]`, and file-tool `locations:[]`. Differential test normaliser `normalize_tool_call_defaults` deleted. Corpus: `tool-call-empty-locations`. Units: `inv_79_materialize_tool_call_defaults_*`.
- **D6 (resolved, first cut):** `session_titles.rs` + session loop wiring port `SessionTitles`. Collects rolling context up to 1000 characters, sends `generate_session_title` control request to CLI at `session_state_changed: idle`, adopts stored titles, publishes `session_info_update`. Corpus: `session-title`. Units: `inv_80_*`.
- **D7 (resolved):** Sequential numeric request IDs (`0`, `1`, ...) assigned to agent-initiated outbound JSON-RPC requests via `codec::RequestIds` in `codec.rs`. Differential test normaliser `normalize_request_permission_id` deleted. Corpus: `permission-two-requests`. Units: `inv_79_request_ids_*`.
- **D8 (resolved):** `oldText: null` materialized on typed file Diff updates (`map::materialize_diff_old_text`) for Write new files and Edit with empty `old_string`. Corpus: `write-new-file-diff`. Units: `inv_79_materialize_diff_old_text_*`.
