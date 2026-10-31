# G10b progress (ROADMAP F6a)

Worktree: /private/tmp/claude-501/par-c, base HEAD 4947469 (branch feat/rust-port).
Group: G10b = F6a tool-call reporters catch-up.

## Result: READY:G10b

## Checklist (all done)
- [x] Read upstream reporters/*, content.ts, facts.ts, renderer.ts, tools.test.ts
- [x] Read Rust tools.rs / map.rs / permission.rs / client_caps.rs
- [x] Reconcile every owned row against HEAD 4947469 (below)
- [x] Failing-first unit tests in tools.rs (+ map.rs for AIR paths)
- [x] Implement missing reporter behaviour in tools.rs / map.rs
- [x] Corpus `tool-shapes` authored + captured with capture.sh; differential test appended
- [x] Port every divergence in the (re)captured fixture
- [x] Flip FEATURES.md rows in place (no header counts); TEST-MAP rows
- [x] Full gate green

## Reconciliation against HEAD 4947469

Already done before this group (verified by reading the code):
- T-35 Read title: offset-only already yielded "(from line N)" and `limit:0`
  no suffix (tools.rs:540-548) -> DONE, regression test added.
- T-70 image URL placeholder -> DONE already (row already DONE).
- T-107 failed Edit/Write/Skill result -> DONE already (row already DONE).
- T-86 non-AIR display copy always present (matches upstream for non-AIR);
  the AIR `rawInputRendering` half needs a capability threaded through
  session.rs (off-limits) -> left PARTIAL with the exact gap.
- M-46 lives in agent.rs (off-limits) -> left PARTIAL.
- O-155/O-161 live in control.rs/agent.rs (off-limits) -> left MISSING.

Ported in this group:
- T-29 Bash description: command_description fallback + empty-string truthiness
  (`shell_description`, js truthiness).
- T-30 PowerShell reporter (tool shape AND console result).
- T-38 Read raw image/document blocks via the full `toAcpContentBlock` port.
- T-47 Glob path/pattern truthiness.
- T-48 Grep: file_path alias, `-l`/`-c` flag forms, JS truthiness for every flag,
  `-A/-B/-C` `!== undefined` template interpolation.
- T-49 WebFetch "(from char N)" + truthy prompt display.
- T-50 WebSearch `Search "<q>"` / "Web search", no domain suffixes.
- T-52 Agent/Task effort suffix unless `subagent_type: "fork"`.
- T-61 AskUserQuestion AIR fixed title via `tool_shape_with(air)` (map path).
- T-62 TodoWrite empty array -> "Update TODOs: ".
- T-64 ReportFindings truthy line (0 has no suffix) and no dropped entries.
- T-66 "Other" JSON fence without a newline before the closing fence.
- T-67 SendMessage/TaskStop/ListAgents/Monitor AIR unfenced error text.
- T-69 content.ts block types (document, tool-reference, tool-search, web-search,
  web-fetch, code-execution, bash-code-execution, text-editor variants, unknown
  default JSON, single typed object, bare object dropped).
- T-74 markdownEscape counts line-start backtick runs only.
- Also ported the reporter truthiness of Write/Edit (empty path/content absent)
  and Skill (empty name) while touching those arms.

## Residual gaps (left PARTIAL, with the exact reason)
- T-86: AIR `rawInputRendering` gate. `ClientCapabilities.raw_input_rendering`
  exists but `MapState` only receives the AIR bool; the wiring point is
  session.rs:1135-1141, which this group may not edit. Non-AIR display copies
  (the row's main behaviour) match upstream.
- Q-68 renderer.ts: terminal channel (T-33/F15), permission/plan-file rendering
  (T-59/T-60/F9) and the AIR rawInputRendering gate (T-86) remain.
- Q-69 agent.ts: toolResult trailer/hand-back/partial-note is T-54 (F7a).
- Q-70 bash.ts: terminal-only exit-code parse / terminal channel is T-31/T-33
  (F15); non-terminal reporter matches.
- Q-74 interaction.ts: ExitPlanMode AIR planFile/planFileReleased is T-59/T-60 (F9).
- Q-77 web.ts: structured WebSearch hit result is T-51 (F7a).
- Q-79 tools.ts: aggregate; remaining T-51/T-54 (F7a) and T-59/T-60 (F9).
- M-46 (agent.rs), O-155/O-161 (control.rs/agent.rs): out of G10b file scope.

## Wire caveat for T-47
The pinned `agent-client-protocol` 2.1.0 crate serialises `Vec` with
`skip_serializing_if = "Vec::is_empty"`, so Rust cannot emit `"locations":[]`
where Node does for an empty Glob path (T-78 already tracks the crate's
empty-array omission, and the existing differential normaliser already covers
`content: []`). The corpus therefore uses a non-empty Glob path; the empty-path
title/locations behaviour is covered by `f6a_glob_truthiness` and does not
weaken any normaliser. No new JsonPath/ignore was added.

## Files changed
- `rust/claude-agent-acp-rs/src/tools.rs`
  - `js_truthy` / `js_string` (JS truthiness and template interpolation),
    `shell_description` used by Bash/PowerShell, `grep_output_mode`,
    `grep_path`, `tool_shape_with(name,input,cwd,air)` (the 3-arg `tool_shape`
    delegates with `air=false` for the permission path).
  - Bash/PowerShell, Read, Write, Edit, Glob, Grep, WebFetch, WebSearch,
    Agent/Task, TodoWrite, ReportFindings, AskUserQuestion, Skill, Other arms.
  - `is_agent_control_tool` / `agent_control_error_content`; PowerShell result
    uses `bash_result_content`.
  - Full `toAcpContentBlock` port (`acp_content_block`, `document_placeholder`,
    `format_byte_size`), `generic_content` = `toAcpContentUpdate`,
    `markdown_escape` line-start runs only, `image_content` error fencing.
  - 20 new unit tests (`f6a_*`) + updated WebSearch table title.
- `rust/claude-agent-acp-rs/src/map.rs`
  - `tool_shape_with(..., state.air)` at the tool_call / refinement / partial
    refinement sites; AIR agent-control error content in
    `tool_update_from_result`.
  - 3 new map tests (`f6a_*`).
- `rust/claude-agent-acp-rs/tests/differential.rs`: `inv_24_tool_shapes`.
- `porting/corpus/tool-shapes.{acp.json,transcript.jsonl}` +
  `porting/fixtures/tool-shapes.{frames.jsonl,argv.json}` (capture.sh; Node
  oracle; every divergence ported).
- `porting/parity/FEATURES.md`: T-29, T-30, T-35, T-38, T-47, T-48, T-49, T-50,
  T-52, T-61, T-62, T-64, T-66, T-67, T-69, T-74 DONE; T-86 refreshed PARTIAL;
  Q-65/Q-66/Q-73/Q-75/Q-76 DONE; Q-68/Q-69/Q-70/Q-74/Q-77/Q-79 PARTIAL with
  exact remaining gaps; M-46/O-155/O-161 notes say they are out of scope.
- `porting/parity/TEST-MAP.md`: Read image/mixed, failed status, ExitPlanMode
  plan, Grep/Glob/WebSearch/TodoWrite/ReportFindings undefined-input rows and
  the two ReportFindings rows mapped to the new tests / tool-shapes.

## Commands run (real output)
- `bash porting/capture.sh tool-shapes` (UPSTREAM_CACHE_DIR set) -> capture complete
- `cargo fmt --all` + `--check` -> clean
- `cargo clippy --workspace --all-targets -- -D warnings` -> PASS
- `cargo test -p claude-agent-acp-rs --lib` -> 476 passed, 0 failed
- `cargo test -p claude-agent-acp-rs --lib turn` -> 60 passed, 0 failed
- `cargo test -p claude-agent-acp-rs --test differential` -> 81 passed, 1 ignored
- `cargo test --workspace` -> all crates pass (incl. capture determinism)
- `bash rust/scripts/rust-gate.sh` -> `==> gate OK` (TEST-MAP CHECK PASS,
  MODULE-MAP CHECK PASS, PARITY PASS)
- JsonPath guard: `git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests
  rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'`
  -> 0
- No new `#[ignore]` (`git diff 4947469 ... | grep -c '#\[ignore\]'` -> 0)
