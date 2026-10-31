# AUDIT P1a — every `different:` row in TEST-MAP.md

- Head audited: ae4b9f4 (gate green)
- Upstream source of truth: claude-agent-acp v0.94.0 (Node)
- Scope: all `different:` rows in `porting/parity/TEST-MAP.md`
- Read-only: no source/doc file edited; this file plus `progress-AUDP1a.md` are the only writes.

Question per row: is the stated reason a real architectural difference the Rust port
cannot/should not mirror, or does it hide a missing/untested behaviour?

Severity: HIGH = false claim, wire-visible bug, deadlock, missing behaviour;
MEDIUM = reason overstates or a real behaviour gap with narrow impact;
LOW = wording/evidence weakness.

## Findings

### 1. HIGH — `different:v2 status not served (G10)` is false (2 rows)

Rows (TEST-MAP.md:2256, 2259):
- `src/tests/tools.test.ts | tool_result_meta non-execution stamping > reports an interrupted command as cancelled to a v2 client, and as completed to every other client`
- `... > reports an aborted run of a tool call that only a permission request surfaced as cancelled on v2`

Both reasons claim "v2 status not served (G10)". That is not true at this head: the
v2 surface is served and the `cancelled` rewrite exists end to end.
`src/v2/tool_call.rs:46-70` (`v2_tool_call_patch`), `src/v2/session_update.rs:112-127`
(`rewrite_aborted_status`), `src/map.rs:1678-1693` (`result_aborted` +
`note_aborted_result`), `src/session.rs:11791-11797` (v2 send rewrite), and
`src/unfinished.rs:188`. Tests already pin it: `src/v2/translate_tests.rs:1189`
(`t8c_aborted_result_is_cancelled_on_v2`), `src/unfinished.rs:452,489`
(dropped/cancelled call on v2), `src/tests_t8c.rs:21`
(`t8c_v2_closing_report_is_cancelled_on_the_wire`), `src/v2/tests.rs:364`.

Failure scenario this hides: the reason steers a reader to believe v2 has no status,
so nobody notices that the *interrupted-command-without-error* half of upstream
`tools.test.ts:3832` (structured `tool_use_result.interrupted`, status `completed`
on v1, `cancelled` on v2) has no dedicated test. The code path exists
(`src/map.rs:1825-1831` reads `BashFacts::Command(command).interrupted`), but only
the `nonExecutionKind` variant is asserted. A regression that drops the
`interrupted` flag from `bash_facts` would keep the gate green while a v2 client
sees `completed` for an aborted command.

What would prove it: a map-level test that feeds
`{"stdout":"partial","stderr":"","interrupted":true}` as `ctx.structured` with
`state.v2.set_reporting(true)` and asserts the serialized update carries
`"status":"cancelled"`, plus the v1 assertion `"status":"completed"`; and for 2259,
the permission-surfaced (never-cached tool_use, `was_emitted == true`) path through
`src/map.rs:1543`.
### 2. LOW — Skill container scan is not cached per pass (row TEST-MAP.md:2310)

Row: `src/tests/tools.test.ts | Skill tool rendering > _meta.jetbrains.air.skill.path > lists a container once for the whole resolution pass`
Reason: "the Rust resolver has no SkillScanCache; each resolution lists the containers once in a single pass (tools.rs resolve_skill_path_in), so a cached second pass has no counterpart".

The reason is honest about the absence, but it is not an architectural difference: upstream's
`SkillScanCache` (`src/tool-calls/reporters/interaction.ts:349-411`) is created once per
available-commands pass (`src/acp-agent.ts:12574-12578`) and is production behaviour, not
test-only. Rust's `resolve_skill_path_in` (`src/tools.rs:500-560`) rescans every container
on each call and `src/map.rs:2192` calls it per skill command, so:
- a pass with N skill commands does N directory walks instead of one (perf only), and
- a `SKILL.md` that appears between two commands of the same pass resolves in Rust but not
  in Node (the upstream test's exact assertion, `tools.test.ts:4779-4788`).

Impact is small (AIR `_meta` path only, and a contrived mid-pass file creation), but the
row's `different:` classification means the upstream cache assertion is simply not tested.
What would prove it: a Rust test with a pass-scoped scan cache that resolves "first", writes
"second", and asserts the second is unresolved within the pass and resolved by a fresh pass.
### 3. HIGH — `/mcp` result-only row claims "no settle hold, no corpus" — both false (row TEST-MAP.md:1431)

Row: `src/tests/mcp-command.test.ts | /mcp > replaces the result text when no other shape carries it`
Reason: "result-only shape publishes at the result frame; a result that beats the control run keeps the CLI text (no settle hold, no corpus)".

Both halves of the parenthetical are false at this head:
- A settle hold exists: `src/session.rs:2404-2448` (`holds_for_local_command_run`) holds a
  `/mcp` output shape while `run_started && prepared.is_none()`, and `take_local_command_markdown`
  (`src/session.rs:2515-2568`) returns `Pending` for exactly that state. The producer starts at
  prompt intake (`src/session.rs:3771-3790`), so `run_started` is already true when the first
  output shape arrives.
- A corpus exists: `porting/corpus/mcp-command-result-first.*`, driven by
  `tests/differential_t1a.rs:139` (`t1a_ag4_mcp_result_before_control_run`), whose doc comment
  is literally "a `/mcp` result that reaches the adapter BEFORE the spawned control run answered
  still publishes the produced markdown (the settle waits for the run), not the CLI's text".
  `src/session/tests_t1fa.rs:943` pins the EOF variant.

What the row actually hides: the pure result-only shape (upstream `shapes: ["result"]`, no
`system/local_command_output` frame first) is untested. The captured corpus always emits the
`local_command_output` system frame before the result, so the replacement is delivered there and
the result is a mirror (`Suppress`). Nothing exercises the "first output shape is `result`" path
where the held frame itself must carry the replacement.

What would prove it: a session-level test that sends `McpPrompt` for `/mcp`, echoes the prompt,
then sends only `{"type":"result", ...}` (no `local_command_output`) while the fake CLI's
`mcp_status` reply is delayed; assert one `agent_message_chunk` carrying `format_mcp_status(...)`
and an empty result text.
### 4. MEDIUM — `get_context_usage {detail:"summary"}` is asserted nowhere, and the cited lines are wrong (row TEST-MAP.md:1116)

Row: `src/tests/create-session-options.test.ts | createSession options merging > context window seeding > asks for the summary context usage so the CLI sends no count_tokens requests`
Reason: "the request is built inline with detail "summary" (session.rs:5651 / agent.rs:2553); the fake CLI auto-acks get_context_usage, so no Rust test observes the detail field".

The behaviour is implemented (`src/agent.rs:2573`, `src/session.rs:5703` both send
`{"subtype":"get_context_usage","detail":"summary"}`), but the cited locations are stale:
`session.rs:5651` is effort-sync code and `agent.rs:2553` is the incarnation/board setup; the
actual lines are 5703 and 2573. No test anywhere asserts `detail == "summary"`
(`grep` over `src/` and `tests/` finds only the two call sites).

Failure scenario: a refactor that drops `detail` (or sends the SDK default `full`) keeps the gate
green. The CLI then issues one `messages/count_tokens` per context category on every session/new
and model switch — the exact burst upstream added the field to avoid (`create-session-options.test.ts:1241-1248`).
That is a wire-visible CLI-side regression with no Rust guard.

What would prove it: record the control request in the fake CLI (the harness already acks it) and
assert the frame equals `{"subtype":"get_context_usage","detail":"summary"}` for both the
session/new background probe and the model-switch probe.
### 5. LOW — cancel-during-`/mcp` rows are untested (rows TEST-MAP.md:1433, 1434)

Rows: `/mcp > publishes nothing when a cancel comes during the status read with %j` and
`/mcp > stops the actions and publishes nothing when a cancel comes during a reconnect`.
Reason: "cancel clears the /mcp state so nothing is published; not corpus-captured".

The mechanism is present (`src/session.rs:3965` `ext.mcp.clear()` then
`release_held_frames` then `machine.cancel()`; the released result finds no foreground target
and is dropped), but no test drives it: the only `/mcp` session test is
`src/session/tests_t1fa.rs:943` (EOF), and `grep` finds no cancel-during-`/mcp` case. The
upstream `%j` matrix (assistant/result/idle, result/idle) is exactly the ordering that decides
whether the held frame is published, so an untested path here is a plausible place for a
regression: if the release happens while a queued prompt has been promoted, the cancelled
result can be attributed to the promoted turn and published.

What would prove it: a session test that starts a `/mcp` status read with a delayed
`mcp_status` reply, cancels, then emits `result` + `idle`, and asserts no `agent_message_chunk`
and `stopReason: cancelled`; repeat for `/mcp reconnect all` with a stalled `mcp_reconnect`.
### 6. HIGH — "there is no per-call forced-root argument" is false, and the cited test does not exercise it (row TEST-MAP.md:267)

Row: `src/tests/acp-agent.test.ts | native subagent eager tool ownership > pins a raced child tool lifecycle to the root where permission created it`
Reason: "the Rust runtime routes a child tool lifecycle by its parentToolUseId to the child session; there is no per-call forced-root argument because the eager-session map only forces a call the permission itself created | the child-session route is proven by src/native_subagents/tests.rs::air_keeps_work_of_a_child_tool_call_in_its_generation_and_async_tasks_reach_it_late"

There *is* a per-call forced argument: `NativeSubagentRuntime::route(notification, forced_session_id)`
(`src/native_subagents/runtime.rs:713-716`), and the non-control branch pins the update to that
session and remembers the owner (`runtime.rs:816-828`). The caller passes the eager session:
`src/session.rs:11737-11739` (`map_state.eager_session(id)` → `route(..., eager.as_deref())`).

But no test exercises that path. The cited `air_keeps_work_...` (`tests.rs:623`) calls
`h.route(&call)` with `None` (`tests.rs:66-69`), so it only proves the child-session route.
`grep` finds no `route(..., Some(...))` call and no test that asserts a permission-surfaced call
stays on the root while a later update for the same id also stays on the root.

Failure scenario this hides: the exact upstream case — a permission request created `toolu_child`
on the root while its `parentToolUseId` names a child. If the forced pin regressed, the client
would see the call on the child session while the permission prompt (and its result) stayed on
the root, splitting one tool lifecycle across two sessions.

What would prove it: a runtime test that seeds a child for `toolu_agent`, routes a terminal with
`parentToolUseId: toolu_agent` and `forced_session_id: "root"`, asserts `sessionId == "root"`,
then routes the same update with `None` and asserts `sessionId == "agent-1"` — i.e. the upstream
test verbatim (`acp-agent.test.ts:9824-9859`).
### 7. MEDIUM — "schema validation runs in capture.sh" is false for v1 (rows TEST-MAP.md:683, 684)

Rows: `src/tests/acp-scenarios.test.ts | ACP scenarios > schema check > rejects a value that breaks a standard format` and `... rejects a value that breaks an ACP integer format`
Reason: "schema validation runs in capture.sh (Node, Ajv against the SDK schema); Rust has no schema validator".

`porting/capture.sh` validates exactly one object: the v2 `initialize` result
(`capture.sh:370-410`, Ajv + the pinned SDK's `schema/v2/schema.unstable.json`). It never
validates v1 scenario frames against the ACP schema, and `rust/scripts/rust-gate.sh` only runs
`capture.sh --install-only`. So the Rust port has no ACP-schema validation at all for v1
traffic, and the row's reason misdirects a reader into believing the capture step covers it.
(The neighbouring 673/674 v2 rows are accurate; 686 correctly says Rust has no validator.)

Impact is bounded because the shared-corpus gate diffs frames exactly against Node-recorded
fixtures (`tests/shared_corpus.rs`, `tests/common::diff_frames` with no ignore paths), so any
covered frame is schema-valid by construction. The gap is that a frame emitted outside the
captured scenarios has no schema guard, which is exactly what the upstream sweep provides.

What would prove it: add an Ajv/`jsonschema` pass over the Rust recorder's outbound frames in
the gate (or at least over the fixtures' frames) and a negative fixture that must fail.
### 8. MEDIUM — archiving a query-closed husk without a transcript returns the wrong error (row TEST-MAP.md:1963)

Row: `src/tests/session-index.test.ts | archive of a loaded session without its transcript > is unknown once the session had history, and allowed for a new unwritten one`
Reason: "the connection's may_be_unwritten rule allows a new loaded session without a transcript; there is no SDK holder check | substitute: tests/t2b_index.rs::t2b_archive_and_unarchive_are_idempotent_and_keep_updated_at".

The upstream test (`session-index.test.ts:3630-3664`) covers three refusals and one allowance:
a `queryClosed` session → `-32002`, a `lastTurnEndedAt` session → `-32002`, a
`resumedFromHistory` session → `-32002`, and a *fresh* loaded session with no transcript →
archive allowed (then closed, and a second archive → `-32002`). Rust's `is_unwritten`
(`src/session_index/connection.rs:808-815`) only checks `resumed_from_history` and
`last_turn_ended_at`; it does not check whether the loaded session's query is closed. A husk
(after EOF or close) with no turn end is therefore "unwritten" and takes the loaded-CLI branch
(`connection.rs:746-757`), which renames through a dead control channel and surfaces
`internal_error` (`-32603`, `connection.rs:946-960`) instead of upstream's `-32002`. The cited
substitute test drives `SessionIndexService::retitle` directly with `may_be_unwritten: false`
and never reaches the connection-level rule.

Failure scenario: a client archives a session whose CLI just died and which never ended a turn;
it gets `-32603 Internal error` where the Node adapter answers `-32002 Resource not found`, and
a client that retries on internal errors will retry forever.

What would prove it: a connection-level test that (a) loads a session with no transcript and
archives it (allowed, session closed), (b) does the same after ending the stream (must be
`-32002`), (c) with `resumed_from_history` and (d) after a turn ended (both `-32002`).
### 9. MEDIUM — rename/archive of a running CLI without the control method returns -32603, not -32600 (row TEST-MAP.md:1926)

Row: `src/tests/session-index.test.ts | a title record after a torn last line > is not written for a running session whose CLI cannot take a title`
Reason: "Rust always sends rename_session; a CLI that cannot take it answers an error which surfaces as -32603, not Node's -32600 TypeError from a missing query method | substitute: tests/t2b_index.rs::t2b_rename_and_archive_refuse_an_unknown_session".

Upstream (`session-index.test.ts:2747-2762`) pins `-32600` for both rename and archive when the
loaded session's query has no `renameSession`, and the transcript is untouched. Rust's
`rename_fn` (`src/session_index/connection.rs:946-960`) maps any control failure or
`response.subtype == "error"` to `internal_error` (`-32603`). The reason admits the code differs
but classifies the row as `different:`; it is a wire-visible contract difference, not an
architectural impossibility — the adapter can map "method not found"/unsupported to `-32600`.
This matters when `CLAUDE_CODE_EXECUTABLE` points at an older CLI that lacks `rename_session`.

What would prove it: a fake-CLI test whose `rename_session` answers `{"subtype":"error"}` (or is
absent) and asserts the rename and archive requests fail with `-32600` and the transcript is
byte-identical.
### 10. LOW — worktree-record source precedence is admitted untested (rows TEST-MAP.md:1930, 1931)

Rows: `tail growth > reads the worktree record of the tail, else of an earlier scan, else of the head` and
`tail growth > takes the worktree record of a wider tail over the head's`.
Reasons: "the tail/head worktree record and the earlier-scan carry live in agent_worktree_of/PreviousScan; no separate test walks the three sources" / "there is no separate wider-tail test".

The reasons are honest, but they leave a three-way precedence (`tail` → `earlier scan` → `head`)
with only `the_last_worktree_state_record_decides` as evidence, which does not exercise the
earlier-scan fallback or the wider-tail-vs-head rule. A regression that falls back to the head
when the tail has no worktree record would change the worktree a listed session reports — a
wire-visible `_meta.worktree` field — with no test failing.

What would prove it: one transcript with a worktree record in the head only, one with it in an
earlier scan and a later append with none, and one with a wider tail that has a different record
than the head; assert the chosen worktree for each.
### 11. MEDIUM — loaded-session unarchive report has no test (row TEST-MAP.md:1946)

Row: `archive state (ACP RFD #2161) > is reported to a loaded session on unarchive, which keeps running`
Reason: "unarchive retitles through the loaded session's CLI without closing it; there is no separate loaded-unarchive branch | substitute: tests/t2b_index.rs::t2b_archive_and_unarchive_are_idempotent_and_keep_updated_at".

The behaviour exists: `unarchive` calls `report_archived(..., false, is_loaded)`
(`src/session_index/connection.rs:762`) which publishes `session_info_update` with
`_meta.jetbrains.air.archived: false` (`connection.rs:817-834`), and it retitles through the
loaded session without closing it. But `grep` finds no test asserting that
`session_info_update`/`archived` notification anywhere (`"archived"` appears only in list
filters and the helper), and the cited substitute test exercises `SessionIndexService::retitle`
directly, not the connection report or the "keeps running" assertion.

Failure scenario: an AIR client unarchives a session it has loaded and never receives the
`archived: false` update, so its UI keeps showing the session archived until a list refresh.
Nothing in the suite would catch that.

What would prove it: a connection test with a loaded session and an AIR client that asserts one
`session_info_update` with `archived:false` after `_session/unarchive`, that the CLI rename was
called with the visible title, and that the session was neither closed nor interrupted.
### 12. LOW — the "other path is gone" half of the project-directory collision rule is untested (row TEST-MAP.md:1957)

Row: `paths that share a project directory > are listed together once the other path is gone, as the SDK lists them`
Reason: "the index hides a colliding path only while it exists on disk; the poll re-evaluates it | substitute: src/session_index/mod.rs::a_differing_case_directory_is_the_project_directory_on_darwin_only".

The rule is `collides` + `Scope::hidden` (`src/session_index/mod.rs:323-370`): a transcript whose
cwd is a *different* path encoding to the same directory is hidden only while that path resolves
on disk. `tests/t2b_index.rs:556` covers both-paths-exist (each list shows only its own), and the
cited case test covers case-insensitive platforms. The interesting half — the colliding path is
*gone*, so the transcript is listed under the other path, and it is hidden again once the
directory reappears (`session-index.test.ts:3482-3506`) — is untested. `Scope::hidden`'s
`resolved` map could regress to "always hide" or "never hide" silently.

What would prove it: the upstream scenario verbatim (dotted cwd with no directory → listed under
the dashed path; `mkdir` the dotted path → hidden).
### 13. LOW — providers/set ordering is serialized by a gate, not by a "sequential handler", and untested (rows TEST-MAP.md:1666, 1667)

Rows: `providers > gateway model discovery > applies providers/set and providers/disable in the order they arrive while a listing runs` and `... keeps the later providers/set when an earlier one lists slower`.
Reason: "requests are served in arrival order by the sequential handler; no listing runs concurrently with another request".

The handler does not serve them sequentially: `providers/set` spawns
`recreate::spawn_provider_update` (`src/agent.rs:4482`) and reserves the provider gate
synchronously (`src/agent/recreate.rs:130-183`); a second update queues on
`acquire_many_owned` (`recreate.rs:195-201`) and the gate is held across the model listing, so
listings are serialized — the outcome matches upstream, but the stated mechanism does not, and
the FIFO ordering that makes "the later set wins" true is tokio `Semaphore` fairness, which no
test pins. `grep` finds no test with two provider updates in flight.

What would prove it: two `providers/set` requests with a slow first gateway listing, asserting
`providers/list` ends on the second base URL, and the same with `providers/disable` after a set.
### 14. LOW — the 26 `scripts/next-preview-version.test.mjs` rows point at a file this branch does not have (TEST-MAP.md:2347-2372)

All 26 rows share: "the preview-version calculator is npm release tooling that stays in Node (scripts/next-preview-version.mjs); the Rust crate has no release-version code".

The classification (release tooling, not adapter behaviour) is right, but the reason says the
script "stays in Node" while `scripts/next-preview-version.mjs` does not exist at this head
(`git cat-file -e HEAD:scripts/next-preview-version.mjs` fails; `scripts/` holds only
`release-preflight.sh`). It was added upstream after this branch's base (`0dd5868` is on `main`,
not an ancestor of `feat/rust-port`). So the tooling is absent, not retained; if preview
publishing is still wanted, the script (and its tests) must be brought over, not "left in Node".
No adapter impact.

## Counts

- `different:` rows in TEST-MAP.md: **184** (185 grep hits minus the vocabulary comment line 5)
- rows checked against upstream test + Rust code: **184**
- questionable rows: **44**, in **14** findings
  - HIGH (false claim / wire-visible): 4 rows — 1431, 2256, 2259, 267 (the `-32603` vs
    `-32002` divergence of 1963 is argued under its MEDIUM finding)
  - MEDIUM: 6 rows — 683, 684, 1116, 1926, 1946, 1963
  - LOW: 34 rows — 2310, 1433, 1434, 1666, 1667, 1930, 1931, 1957, plus the 26
    `scripts/next-preview-version.test.mjs` rows

Rows judged sound (not reported): the Node-harness/comparator rows (acp-scenarios-compare
663-671, acp-scenarios 677-689, acp-scenarios-v2 672-675), the SDK-integration rows
(acp-agent 160-164, model-resolution 1475-1476, model-presentation 1449), the JS-only hazard
rows (permission-normalization 1566-1567, permission-presentation 1607), the no-throw/no-stream
rows (acp-agent 169-173, 312, 348, 469-471, 563, 612-618, incomplete-tools 1348, file-change-audit
1292, event-loop-yield 1237-1239), the concurrency-structural rows (native-subagents 1484, 1548,
1550, providers handled above), the delegation rows (session-hooks 1775-1776, managed-policy 1393,
folded-prompt 1302-1305, session-titles 2031, acp-v2 741), the authorization rows (969-994; the
guard refusal is a plain RPC error and the port's session-failure book is only fed by turn
failures), and the remaining session-index rows (polling vs `fs.watch`, no SDK/live-registry
branch, per-path re-scoping) whose cited substitute tests all exist.

DONE
