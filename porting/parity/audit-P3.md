# AUDIT P3 — shared corpus, live smoke, module map (read-only; head ae4b9f4)

Scope: (1) the 23 SHARED.md scenarios and how each is replayed against Rust and the Node
oracle, (2) `tests/live_smoke.rs` vs the plan, (3) 15 random MODULE-MAP rows, (4) the
carry-over items M4/M6/M7/L10/L13 from `audit-T7.md`.

Oracle: Node v0.94.0 at
`…/scratchpad/upstream-wt/v0.94.0`; fixtures under `porting/fixtures/` were recorded from
the npm-pinned Node adapter by `porting/capture.sh` (`node $DIST` + `acp-recorder`), not
from Rust. Corpus/fixture findings were reproduced with real recorder runs against
`rust/target/debug/claude-agent-acp-rs` + `fake-claude` (no real `claude`); code findings
cite `file:line` and were read at HEAD.

## 1. SHARED.md — the 23 scenarios

Replay paths: the 18 `alias` rows have no dedicated shared test; they are replayed by
`differential.rs::inv_24_full_corpus` (every `porting/corpus/*.acp.json`, skipping the five
`T1FC_OWNED`), except `edit-diff-preview` → `differential_t1fc.rs::edit_diff_preview`.
Several also have a per-scenario test (`inv_24_text_only`, `inv_24_permissions`, …).
The 5 `new` rows are replayed twice: `tests/shared_corpus.rs::shared_*` **and**
`inv_24_full_corpus`.

| shared name | Rust replay test | Node oracle | could hide a divergence? |
|---|---|---|---|
| plain | differential.rs:412 `inv_24_text_only` (+full corpus) | fixtures/text-only.frames.jsonl | **YES** — no assistant text frame exists on either side (see P3-01) |
| tools_shell | differential.rs:1302 `inv_24_tool_shapes` | tool-shapes | no; 21 tool_call/21 update, diff/rawInput/kind compared |
| edit_diff | differential_t1fc.rs:135 `edit_diff_preview` | edit-diff-preview | no; diff/oldText/newText + permission reply compared |
| write_new | differential.rs:1388 + differential_t1c.rs:50 | write-new-file-diff | no; structuredPatch compared |
| permission_allow | differential.rs:661 `inv_24_permissions` | permission-allow | no; request+reply+update compared |
| permission_deny | differential.rs:662 `inv_24_permissions` | permission-deny | no; reject reply compared |
| cancel_mid_tool | differential.rs:1220 `inv_24_cancel_during_tool` | cancel-during-tool | no; stopReason cancelled + tool close compared |
| cancel_before_turn | differential.rs:637 `inv_24_cancel_queued_echo_less` | cancel-queued-echo-less | no; 3 prompts, one `cancelled` |
| steer_mid_turn | differential.rs:608 `inv_24_steer_mid_turn` | steer-mid-turn | no; steering frame + 2 messageIds |
| queued_prompt | shared_corpus.rs:57 (+full corpus) | queued_prompt | no; 2 prompt sends before the 1st result |
| resume_recall | shared_corpus.rs:65 (+full corpus) | resume_recall | **YES** — "recall" never reaches the wire (P3-02) |
| session_load_replay | differential.rs:1106 | session-load-replay | no; 10 replay updates incl. user/thought/tool |
| config_options_model_mode | differential.rs:1016 | config-session-new | no; modes+configOptions compared |
| set_config_option | differential.rs:1018 | config-set-model | no; currentValue changes in the response |
| image_input | differential.rs:800 `inv_24_image_prompt_echo` | image-prompt-echo | **YES** — prompt carries no image (P3-03) |
| slash_commands_skills | differential.rs:426 `inv_24_commands_proactive` | commands-proactive | partially: listed only, never executed (P3-04) |
| usage_update | differential.rs:1003 | usage-update | no; 3 usage_update frames compared |
| background_after_prompt | shared_corpus.rs:73 (+full corpus) | background_after_prompt | **YES** — no background lifecycle frame (P3-05) |
| auth_failure | differential.rs:1416 `inv_24_session_failure` | auth-required | no; `-32000 Authentication required` compared |
| session_list_close_delete_fork | shared_corpus.rs:80 (+full corpus) | session_list_close_delete_fork | **YES** — `delete` is never sent (P3-06) |
| initialize_capabilities | differential.rs:1133 `inv_f2a_initialize_capabilities` | initialize-capabilities-plain | no; whole initialize result compared |
| mcp_stdio | differential.rs:832 `inv_57_f4a_spawn_inputs` | mcp-config-stdio | no for the params; no MCP traffic is exercised |
| subagent | shared_corpus.rs:88 (+full corpus) | subagent | **YES** — no child lifecycle frame (P3-07) |

Normalisation the differ applies to all 23 (no ignore paths — `base_ignore()` is `[]`):
uuid/timestamp strings and uuids embedded in longer strings become per-side `$N`
placeholders; `$ROOT` path rewrite; `authMethods._meta["terminal-auth"]` command→`$EXEC`
and its first non-`--` arg dropped; fixture `agentInfo.version` rewritten to the composed
Rust version (Rust's value compared exactly, one agentInfo per side asserted). Numeric
ids and all numbers are compared exactly. Tolerance: none.

### HIGH

- **P3-01 (HIGH)** `porting/parity/SHARED.md:7` — "plain" is claimed as "single prompt
  expecting plain assistant text"; no assistant text is on the wire. `text-only.transcript.jsonl`
  emits `assistant.message` without `id`/`model`, and neither captured stream contains
  a single `agent_message_chunk`: `fixtures/text-only.frames.jsonl` has 7 frames (initialize,
  session/new, available_commands_update, prompt send, result). Reproduced: the recorder
  against the Rust binary yields the same 7 frames, and an extra `sleep` step after the
  prompt adds none. So a Rust bug that drops or corrupts assistant text is invisible here.

- **P3-02 (HIGH)** `SHARED.md:17` — "resume_recall … recall asserted in prompt 2" is false.
  The transcript's prompt-2 assistant text is `banana`, but the fixture contains no
  `agent_message_chunk`; the second prompt result is only `stopReason=end_turn` (12/4
  usage, identical to prompt 1). The test cannot tell whether the resumed session recalled
  anything; a Rust `session/resume` that loses history passes. (The only occurrence of
  "banana" in the fixture is the prompt-1 *send* frame.)

- **P3-03 (HIGH)** `SHARED.md:21` — "image_input … multi-modal prompt carrying base64 image
  content" is false. `corpus/image-prompt-echo.acp.json` sends
  `prompt:[{"type":"text","text":"hello"}]` (no image; verified in the fixture's send
  frame). The only base64 image is fake-claude's hardcoded replay `user` message echoed
  back as an agent chunk, so the client→agent image path (parsing, forwarding, CLI argv) is
  never exercised: dropping the image on the Rust side still diffs clean.

- **P3-04 (HIGH)** `SHARED.md:22` — "slash_commands_skills … slash commands listed in
  session and executed". Only *listing* is covered: `commands-proactive.acp.json` prompts
  `hello`; the fixture is initialize/session/new/available_commands_update/prompt/result.
  No slash-command prompt (`/compact`, `/mcp`, …) is sent, so command execution/rewrite is
  untested by this row.

- **P3-05 (HIGH)** `SHARED.md:24` — "background_after_prompt … full lifecycle (task_started,
  turn result)". The transcript emits `system/task_started` (`bg1`), but the fixture has
  **no** background/task frame at all: 7 frames = initialize/new/available_commands_update
  + prompt send + end_turn result. The scenario is shaped like `plain`; a Rust port that
  drops the whole background-task lifecycle diffs clean. The claimed "task_started" frame
  does not exist on either side.

- **P3-06 (HIGH)** `SHARED.md:26` — "session_list_close_delete_fork … (list, close, delete,
  fork-after-close error)". `corpus/session_list_close_delete_fork.acp.json` has a
  `session/delete` step, but the recorder aborts on the *unexpected* fork error
  (`expect_error` defaults false, `acp-recorder/src/recorder.rs:499-502`) and `finish()`
  still returns the frames, so the fixture ends at the fork error: 11 frames, **0**
  `session/delete`. Reproduced against the Rust binary: 11 frames, no delete, exit 0. The
  delete capability is advertised but never replayed; `session/list` is only the empty-list
  case.

- **P3-07 (HIGH)** `SHARED.md:29` — "subagent … child lifecycle frames diff clean". The
  transcript emits `task_started` (`task_01SUB`) and `task_notification` (completed), but
  the fixture has only the parent `Task` `tool_call` + `tool_call_update` ("Async agent
  launched") + end_turn; there is **no** child lifecycle frame (no child session, no
  announcement, no completion notification). The row's claim is false and the child
  announce/route/notify path is not compared.


### Cross-cutting comparison blind spots (all 23)

- **P3-22 (MEDIUM)** `tests/common/mod.rs:898` — the recorder runs the **Rust** side with
  `CLAUDE_ACP_FORCE_CANCEL_GRACE_MS=1000` (the Node oracle ran with its 30 s default,
  `src/acp-agent.ts:233`). Frames carry no timing, so a Rust cancel that only settles via
  the force-cancel backstop (rather than the CLI interrupt) still diffs clean against a
  Node cancel that settles on the interrupt. Concrete: `cancel-during-tool` /
  `cancel-queued-echo-less` pass even if Rust needs 1 s of backstop while Node settles in
  one frame; the default 30 s grace is never exercised by the corpus.

- **P3-23 (MEDIUM)** `acp-recorder/src/recorder.rs:159-176,499-502` — `finish()` returns the
  frames even when the script aborted on an unexpected step error, so every step after the
  error is silently never replayed while the test still compares "clean". The instance is
  `session_list_close_delete_fork` (P3-06, verified: 11 frames, no `session/delete`), but
  the mechanism applies to any script whose step errors; a future step placed after an
  error is dead coverage that no test reports.

- **P3-24 (LOW)** `tests/common/mod.rs:589-599` (mirrored in `capture.sh:isTimestamp`) — any
  string of ≥11 chars starting `YYYY-MM-DD` is replaced by a first-appearance placeholder
  on both sides. A title/text/tool field that begins with a date ("2026-10-29 report") is
  normalised, so a real content difference there compares equal. No shared scenario carries
  such text today, but the full corpus (237 fixtures) could.

- **P3-25 (LOW)** `tests/common/mod.rs:410-427` — uuid-shaped substrings are replaced
  anywhere, including `rawInput`/`rawOutput`/message text. A divergence in a uuid-bearing
  payload (e.g. the wrong session/message id echoed in text) maps to the same placeholder
  on both sides and is invisible.

- **P3-26 (LOW)** `tests/common/mod.rs:942-977` — `normalize_terminal_auth` rewrites
  `_meta["terminal-auth"].command` to `$EXEC` and drops the first non-`--` arg on both
  sides. The entrypoint divergence is intended, but a wrong first argument (a different
  script path) would be dropped rather than compared; only the flags are checked.

## 2. `tests/live_smoke.rs` (PARITY_LIVE=1 only; not run here)

Six tests: `parity_live_initialize`, `parity_live_new_session`, `parity_live_tiny_prompt`,
`parity_live_tool_permission`, `parity_live_cancel`, `parity_live_resume`. They drive the
built Rust adapter (`rust/target/debug/claude-agent-acp-rs`) with
`CLAUDE_CODE_EXECUTABLE=<real claude>` over stdio JSON-RPC, and assert protocol facts only.
`rust/scripts/rust-gate.sh` runs `cargo test`, so the tests compile and return early
(printing SKIPPED) unless `PARITY_LIVE=1` and `claude`/`PARITY_CLAUDE` resolves.

Coverage vs plan §3 (`PARITY-PROGRAM.md:40-41`: initialize, new session, tiny prompt,
cancel, resume): **complete**, plus an extra tool/permission test. Vs the task's
initialize/new/prompt/cancel/resume/**fork/close** list: **fork and close (and delete,
list, load) are absent**.

### HIGH

- **P3-08 (HIGH)** `tests/live_smoke.rs:429-445` — `parity_live_cancel` passes vacuously on
  EOF: `let Some(line) = line_opt else { break }` leaves the loop without ever evaluating
  `assert_eq!(frame["result"]["stopReason"], "cancelled")`. An adapter that dies (or closes
  stdout) after a cancel is reported green. The only cancellation assertion is inside the
  loop; there is no post-loop `saw_cancel` flag. Reproduced by reading the control flow
  (no run): EOF → break → `client.close()` → test returns.

### MEDIUM

- **P3-09 (MEDIUM)** `tests/live_smoke.rs:453-510` — `parity_live_resume` asserts only that
  the second `session/prompt` returns `end_turn`. It never checks the recalled word
  ("banana") or that any history was replayed, so a `session/resume` that loses the
  transcript passes; the module doc's "resume" claim is only an RPC-success check.

- **P3-10 (MEDIUM)** `tests/live_smoke.rs:313-343` — `parity_live_tiny_prompt`'s
  `session/update` assertion is satisfied by the `available_commands_update` emitted right
  after `session/new`, so the test passes even when the prompt yields zero assistant text.
  A `stopReason` + one `agent_message_chunk` (or a `sessionUpdate` filter) is needed.

- **P3-11 (MEDIUM)** `tests/live_smoke.rs:146-151` — the client request ids start at 1 and
  the adapter's own request ids start at 0 and increment (`porting/fixtures/permission-two-requests.frames.jsonl`
  shows `session/request_permission` ids 0 then 1). `request()` treats any frame whose `id`
  equals the pending client id as the response *before* checking `method`, so a prompt that
  issues its Nth permission request with id == the pending prompt id (e.g. 4) is consumed as
  the prompt result (`result` absent → `Null`), failing the `stopReason` assertion and
  leaving that permission unanswered. Concrete: `parity_live_tool_permission` with a prompt
  that triggers 5 permission requests (ids 0..4) while awaiting client id 4.

- **P3-27 (MEDIUM)** cross-repo consistency (`PARITY-PROGRAM.md:41`, "same env gate and
  test names in both repos"): the codex port (`~/code/fastestdevalive/codex-acp-rust/rust/tests/live_codex_contract.rs`)
  has `live_codex_contract_turn` / `live_acp_full_turn`, not `parity_live_*`, and gates with
  `#[ignore]` + `live_enabled()` that *panics* when `PARITY_LIVE != 1` or the CLI is absent
  (`:42-51`), while this repo's `live_smoke.rs` runs unignored and silently returns
  (SKIPPED) when the CLI is missing. The env var matches; the names and the absent-CLI
  behaviour do not.

### LOW

- **P3-12 (LOW)** `tests/live_smoke.rs:249-272` — `new_session` sends
  `session/set_config_option {configId:"model", value:"haiku"}` and discards the result
  (`let _ =`). "haiku" is a valid family alias (`session_model.rs:51,1283` resolves it to
  `claude-haiku-4-5`), so it probably succeeds, but nothing asserts the model took effect:
  if the option were renamed/rejected the prompt silently runs on the default model and the
  "tiny prompt" cost/latency premise is unverified.

- **P3-13 (LOW)** `tests/live_smoke.rs:7` — the doc says the tests assert "no process left",
  but `close()`/`kill()` discard `wait_pid_gone`'s result (`let _ =`, lines 202, 210), so a
  surviving child is not a failure.
- **P3-14 (LOW)** `tests/live_smoke.rs:68-78` — unlike the differential harness
  (`common/mod.rs:827-898`), the live client does not set a hermetic `HOME` /
  `CLAUDE_CONFIG_DIR`; it drives the host's real `~/.claude`, writes a real transcript and
  can load the user's skills/settings, so results are machine-dependent.
- **P3-15 (LOW)** `tests/live_smoke.rs:238-243` — `initialize` advertises
  `fs.readTextFile/writeTextFile` and `terminal:true`, but `request()` only answers
  `session/request_permission`; any future `fs/*` or `terminal/*` request from the adapter
  would hang to the 120 s timeout. (Today the adapter does not issue them — `src/` has no
  `fs/read_text_file`/`terminal/create` client call — so this is latent.)

## 3. MODULE-MAP — 15 sampled rows (seeded random, across the file)

Row count checked: 107 rows = 90 files + `acp-agent.ts` in 17 sections; upstream `src/`
has 91 non-test `.ts` files (156 total minus 65 under `src/tests/`), so the header's "91
files" is consistent.

| # | upstream module | mapped Rust | exports found? |
|---|---|---|---|
| 68 | session-index/worktrees.ts | session_index/worktrees.rs | yes: `worktreeCounterparts`→`worktree_counterparts:130`, `isInLinkedWorktree`→`is_in_linked_worktree:155`, `repositoryWorktrees`→`repository_worktrees:181` |
| 56 | session-index/archive-title.ts | session_index/archive_title.rs | yes: `ARCHIVED_TITLE_PREFIX:18`, `visibleTitle:74`, `effectiveTitle:125`, `storedTitle:97` |
| 28 | file-change-audit.ts | file_change_audit.rs | yes: `createNativeFileChangeReporter`→`Reporter:315`, `agentFileChangeReportMeta:67`, `recordSubagentFileEdits:78`; constants renamed (`AGENT_FILE_CHANGE_REPORT_MAX_BYTES`→`MAX_REPORT_BYTES:35`, `…TIMEOUT_MS`→`REPORT_TIMEOUT:37`) |
| 81 | tool-calls/facts.ts | tools.rs | types only: `ToolUseFacts`/`ToolResultFacts`/`ToolReporter` have no named Rust structs; `ToolShape:35` + `BashFacts` (`tools/reporters.rs:14`) + `CommandOutcome` (`client_caps.rs`) model them |
| 44 | permissions/options/shared.ts | permission_options.rs | yes: `allowOnce:196`, `reject:204`, `exactLocalAllowRule:516`, `isMcpAllowChangeSet:669`, `PERMISSION_OPTION_ID` ids at `:26` |
| 58 | session-index/dir-listing.ts | session_index/dir_listing.rs | yes: `statFiles`→`stat_files:60`, `DirListings:83` |
| 36 | mcp-command.ts | mcp_command.rs | partial: `parseMcpCommand:136`, `runMcpCommand:845`, `cleanMcpError:435`, `formatMcpStatus:579`; **`MCP_AVAILABLE_COMMAND` is in `map.rs:2211-2218`, not the mapped module** |
| 23 | context-compaction-meta.ts | context_compaction_meta.rs | yes: `CONTEXT_COMPACTION_META_VERSION:14`, `createContextCompactionMeta`→`create_context_compaction_meta:85` |
| 92 | tool-calls/reporters/web.ts | tools.rs | yes (merged): `WebFetchReporter`→`tools.rs:1151` + `tools/reporters.rs` web facts; `WebSearchReporter`→`tools.rs:1177` |
| 87 | tool-calls/reporters/file-edit.ts | tools.rs, hooks.rs, diff.rs | yes: `WriteReporter`/`EditReporter`/`NotebookEditReporter` (`tools.rs:1022,1299`), `normalizeWriteInput`→`normalize_write_input:616` |
| 88 | tool-calls/reporters/index.ts | tools.rs | `reporterFor` has no lookup function; the dispatch is the `tool_shape` match (`tools.rs:865-1300`) — merged, acceptable |
| 54 | session-failure-extension.ts | session_failure.rs | partial: `sessionFailureMeta:148`, `providerFailureCategory:175`, `ClaudeFailureKind:16`, `assistantMessageText:214`; **`createSessionFailureState`/`SessionFailureController` is `session/failures.rs:68`, `airSessionFailureCapabilityMeta` is `agent.rs:192` + `session.rs:13021`, `supportsAirSessionFailures` is `ext.air_supports` in `session.rs`** |
| 11 | acp-agent.ts@getOrCreateSession | session.rs, session_store.rs, resumed_session.rs | **no: `getOrCreateSession`'s fingerprint/recreate logic is `agent.rs:543 compute_session_fingerprint` + the `session/new` arm at `agent.rs:3551`; no `get_or_create` in the three mapped files** |
| 100 | v2/permission.ts | v2/permission.rs | yes: `v2PermissionRequest`→`v2_permission_request:37`, `v1PermissionResponse`→`v1_permission_response:82` |
| 34 | lib.ts | lib.rs | yes (different): `pub use agent::{… ClaudeAcpAgent …}`, `run_acp`, `to_acp_notifications`, `SettingsManager`, `plan_entries`, `to_display_path`, `tool_info_from_tool_use`; `streamEventToAcpNotifications`/`isLocalCommandMetadata` are not re-exported |

### MEDIUM

- **P3-16 (MEDIUM)** `porting/parity/MODULE-MAP.md:24` — the `acp-agent.ts@getOrCreateSession`
  row maps to `session.rs, session_store.rs, resumed_session.rs`, but none of them holds
  that method's behaviour. `getOrCreateSession` (upstream `acp-agent.ts:10859-10908`) is
  the fingerprint-compare / teardown+recreate / else `createSession(resume)` path; the Rust
  equivalent is `compute_session_fingerprint` (`agent.rs:543`) and the `"session/new"` arm
  (`agent.rs:3551-3600`). A bump that changes `getOrCreateSession` would send a porter to
  three files with none of the logic — the map's whole purpose. Fix the row to
  `agent.rs, session.rs` (and check `session_methods.rs`, which only has list/delete/fork).

- **P3-17 (MEDIUM)** `MODULE-MAP.md:67` — `session-failure-extension.ts` is listed `1:1`
  `session_failure.rs`, but that file only holds the pure helpers. The lifecycle
  (`SessionFailureController.prepare/publish/clear/restore`, upstream `:263-449`) is
  `session/failures.rs:68-266`; `airSessionFailureCapabilityMeta` is the literal AIR list
  at `agent.rs:192-200` and the initialize meta at `session.rs:13021`;
  `supportsAirSessionFailures` is `ext.air_supports("sessionFailure")` at
  `session.rs:8060,10434`. The row omits `session/failures.rs` and `agent.rs`, so a bump to
  the controller is not routed to its Rust module.

- **P3-18 (MEDIUM)** `MODULE-MAP.md:49` — `mcp-command.ts | mcp_command.rs | 1:1` is
  incomplete: the exported `MCP_AVAILABLE_COMMAND` (upstream `mcp-command.ts:9-16`, the
  `/mcp` entry of `available_commands_update`) is built inline in `map.rs:2211-2218`, and
  the status read (`readMcpServerStatus`, `:51`) goes through `control.rs:475`
  (`mcp_status`). Only `parse/clean/format/run` live in `mcp_command.rs`.

### LOW

- **P3-19 (LOW)** `MODULE-MAP.md:81` — `tool-calls/facts.ts` is a type-only module; the Rust
  has no `ToolUseFacts`/`ToolResultFacts`/`ToolReporter` types, only `ToolShape` (`tools.rs:35`),
  `BashFacts` (`tools/reporters.rs:14`) and `CommandOutcome` (`client_caps.rs`, outside the
  mapped module). Conceptually merged, but a bump to these interfaces has no single place to
  diff.
- **P3-20 (LOW)** `MODULE-MAP.md:88` — `tool-calls/reporters/index.ts`'s only export
  `reporterFor(toolName)` has no Rust counterpart by name; the dispatch is the `tool_shape`
  match. Fine for behaviour, but a bump to `reporterFor` will not be found by symbol search.
- **P3-21 (LOW)** `MODULE-MAP.md:47` — `lib.ts` is `different`, which is honest, but two
  upstream re-exports (`streamEventToAcpNotifications`, `isLocalCommandMetadata`) have no
  `pub use` in `lib.rs` (`strip_local_command_metadata` is crate-private in `transcript.rs:206`).

## 4. Carry-over from `audit-T7.md` (M4, M6, M7, L10, L13) at head ae4b9f4

All five are **still open**; line numbers moved (the T7 tests now live under `src/`).

- **M4 (MEDIUM, unchanged)** — `src/session/tests_t7a.rs:173-219` still proves only the
  standalone yielder: its local `drain()` calls `maybe_yield()` itself, with no actor. The
  loop-level test `t7a_b63_cancel_settles_behind_a_5000_frame_backlog` (`:99-136`, asserts
  `before < 1_000` at `:130`) still cannot fail if the production yielder is removed:
  `run_loop`'s `tokio::select!` is `biased` and serves the command branch before the stream
  branch (`session.rs:3410`, command arm `:3440`), so a queued `Command::Cancel` is handled
  within a frame or two of arriving regardless of the pause. Production does call
  `message_loop_yielder.maybe_yield()` (`session.rs:5854`), but no test ties that call to the
  cancel latency. Severity unchanged: the evidence, not the code, is the gap.

- **M6 (MEDIUM, unchanged)** — `FileChangeReports::cancel_collecting` (`session.rs:2832-2839`)
  sets `job.cancelled` and publishes the `unavailable/cancelled` report immediately, but the
  spawned deferred-report task still awaits `run_report(...)` before `reply.send(answer)`
  (`session.rs:11227-11310`). `run_report` never checks `handle.is_finished()` before the
  preview (`file_change_audit.rs:407-418`); it always waits `tokio::time::timeout(timeout,
  preview)`. So a cancel at 250 ms of a 600 ms preview still delays the prompt response to
  ~600 ms (up to `REPORT_TIMEOUT` = 2 s), while upstream resolves the settling turn
  immediately (`acp-agent.ts:8627-8685`). The v2 path has the same wait (`session.rs:11324-11397`).
  Wire-visible only as latency (the stopReason is corrected), so MEDIUM stays.

- **M7 (MEDIUM, unchanged)** — `settle_events` still takes the held v2 emissions
  (`session.rs:11324`) and emits them from a `tokio::spawn` (`:11335`, `emit_v2_emissions` at
  `:11397`). The actor returns to its loop immediately, so the next `session/prompt` or
  stream frame can be processed and emit its `user_message` before the previous turn's held
  `idle`. Node awaits the preview inside `settleActive`, so the previous `idle` is always
  ahead of the next turn's frames. Same MEDIUM (client-visible frame order, no deadlock).

- **L10 (LOW, unchanged)** — `TrackedToolCall::record_meta` still stores
  `value.to_string()` (`map.rs:968-976`) where Node compares `JSON.stringify` of the merged
  meta (`field-tracker.ts:156-182`). serde_json prints an integral float as `1.0` and an
  integer as `1`; `JSON.stringify` prints `1` for both. A repeated meta whose number arrives
  once as `1.0` and once as `1` therefore emits a spurious `tool_call_update` on the Rust
  side only. No corpus scenario exercises it. Keep LOW (narrow trigger), but it is a real
  wire-visible extra frame.

- **L13 (LOW, unchanged)** — the deferred v1 reports of one batch are still spawned in a
  `for` loop with one `tokio::spawn` per job (`session.rs:11227-11310`), and each takes and
  releases its own stale `ack` before running its report (`:11250-11252`). Two settled turns
  in one batch therefore release their barriers and reply in report-completion order, not
  settle order; upstream awaits the reports sequentially in `settleActive`. Wire-visible as
  JSON-RPC response order (ids disambiguate), so keep LOW.

DONE
