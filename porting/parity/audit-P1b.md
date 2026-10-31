# AUDIT P1b — sampled `ported:` rows and FEATURES DONE rows

- Head audited: ae4b9f4 (gate green)
- Upstream source of truth: claude-agent-acp v0.94.0 (Node)
- Scope: fixed-stride samples — 24 `ported:` rows from `src/tests/acp-agent.test.ts`,
  12 from `src/tests/tools.test.ts`, 12 from `src/tests/session-index.test.ts`,
  8 from `src/tests/acp-v2.test.ts` (rows of `porting/parity/TEST-MAP.md`), plus 30
  `DONE` rows of `porting/parity/FEATURES.md` (stride 31 across the table).
- Strides: acp-agent every 24th, tools every 17th, session-index every 10th, acp-v2
  every 16th `ported:` row in file order; FEATURES every 31st DONE row.
- Read-only: no source/doc file edited; this file plus `progress-AUDP1b.md` are the only writes.

Question per `ported:` row: does the cited Rust test assert at least the same inputs and
expected values as the upstream test? A test that only proves "no panic", or asserts a
weaker subset, is a finding. For FEATURES rows: does the cited evidence exist, and does
it prove the claim?

Severity: HIGH = false claim, wire-visible bug, deadlock, missing behaviour;
MEDIUM = real gap with narrow impact or a materially weaker assertion;
LOW = wording/evidence weakness.

## Findings

### 1. MEDIUM — TEST-MAP.md:43 (`acp-agent` task plan republish) asserts a weaker snapshot

Row: `src/tests/acp-agent.test.ts | task plan lifecycle > republishes the session task snapshot when a follow-up prompt starts`
Cited: `src/session/tests_g1.rs::prompt_republishes_the_session_task_snapshot` (tests_g1.rs:244).

Upstream (`acp-agent.test.ts:296-323`) seeds task-1 `in_progress` with `activeForm:"Running tests"` and task-2 `pending`, then asserts the plan entries
`[{content:"Running tests",status:"in_progress"},{content:"Write release notes",status:"pending"}]`
plus the full notification shape (sessionId included). The Rust test seeds both tasks `pending` (tests_g1.rs:247-248) and asserts two `pending` entries with `content` = the subject. It never asserts that an `in_progress` task's content is its `activeForm`, nor the `in_progress` status, nor the `sessionId`.

Failure scenario it hides: if the prompt-intake snapshot rendered `content` from `subject` for `in_progress` tasks (or dropped the status), the gate stays green while an AIR client sees "Run tests"/pending where Node sends "Running tests"/in_progress. The TodoWrite test (`t8a_todo_write_plan_entries`) covers activeForm for TodoWrite only, not the task-snapshot republish path.

### 2. LOW — TEST-MAP.md:140 cites a test that does not assert the row's claim

Row: `... usage-limit failure replay > replays every quota failure with its persisted turn id and keeps only the latest active`
Cited: `src/transcript.rs::t3a_replay_limit_followed_by_a_real_answer_is_not_active` (transcript.rs:3446).

That fn only checks that a *single* limit followed by a real answer still emits one `session_info_update` and leaves `book.active()` empty; it never asserts a persisted turn id, two quota failures, or the revision. The claim is actually covered by the neighbouring `t3a_replay_restores_usage_limit_failures_at_their_position` (transcript.rs:3412-3438), which asserts ids `u1:error`/`u2:error`, their positions and `active()[0].failure.id == "u2:error"`. So the behaviour is tested, but the citation points at the wrong (weaker) test; a reader checking the cited fn would wrongly conclude the persisted-id/revision assertions are missing.

### 3. LOW — TEST-MAP.md:171 does not model upstream's `queryClosed` precondition

Row: `... permission request cancellation > clears pending ExitPlanMode state even after the query has closed`
Cited: `src/session/tests_t3a.rs::t3a_cancel_forgets_the_pending_plan_markers` (tests_t3a.rs:304).

Upstream (`acp-agent.test.ts:4750-4775`) sets `session.queryClosed = true` before `cancel()` and asserts both `cancelled === true` and both pending markers undefined. The Rust test cancels a live/echoed turn (`rig.echo("p1"); rig.machine.cancel()`), asserts only that both markers are `None`, and never asserts the turn is cancelled. Rust has no `queryClosed` field (only comments referencing it), so the "even after the query has closed" half of the scenario is untested: an early return in the cancel handler for an ended query would clear nothing, and this test would not notice.

### 4. LOW — TEST-MAP.md:202 omits the `air.version` and the deny outcome

Row: `... canUseTool in bypassPermissions mode > leads with the reject option and forwards the hint when the CLI defaults to no`
Cited: `src/session/tests_t1fa.rs::t8a_default_to_no_leads_with_reject_and_forwards_the_hint` (tests_t1fa.rs:633).

Upstream (`acp-agent.test.ts:5701-5740`) asserts the full `_meta` equality `{jetbrains:{air:{version:1,permission:{version:1,title:"rm -rf build",defaultToNo:true}}}}` and `result` matches `{behavior:"deny"}`. The Rust test asserts `params["_meta"]["jetbrains"]["air"]["permission"]` only (so the outer `air.version:1` is unasserted) and sends `{outcome:"selected",optionId:"reject"}` without checking the mapped result. A regression that drops `air.version` or maps a reject selection to allow stays green.

### 5. MEDIUM — TEST-MAP.md:226 never exercises "without native subagent sessions"

Row: `... subagent permission attribution (issue #851) > keeps AIR Agent child text internal without native subagent sessions`
Cited: `src/map.rs::f6b_air_drops_nested_stream_text_without_the_transcript_capability` (map.rs:4206).

Upstream (`acp-agent.test.ts:6921-6996`) drives a full prompt with an `Agent` tool_use + `task_started` + nested stream delta for AIR and asserts (a) no `subagent_spawned`/`subagent_state_update`, (b) no update text containing "hidden child output", and (c) the parent `toolu_parent` tool_call exists. The Rust test feeds a bare `stream_event` through `MapState`, asserting only that AIR drops it, that `set_forward_subagent_text(true)` forwards it, and that a plain client gets it. It does not assert the absence of native-subagent lifecycle frames, nor that the parent tool_call is still emitted, and it never runs the task-started/native-subagent negotiation path. A regression that announces a native subagent (or loses the parent tool_call) for an AIR client without the transcript capability would keep this test green.

### 6. LOW — TEST-MAP.md:250 omits the streamed-child-text assertions

Row: `... a failed child that the SDK resumes > opens a new generation on a SendMessage result that resumed the agent`
Cited: `src/native_subagents/tests.rs::air_opens_a_new_generation_when_a_failed_child_runs_again` (native_subagents/tests.rs:569).

Upstream (`acp-agent.test.ts:8453-8458`) asserts the full lifecycle, including the `agent_message_chunk` entries routed to `agent-42` and `agent-42:generation:2`, that the serialized updates do not contain "stray" and do contain "resumed". The Rust test asserts the route of one resumed output is `worker-1:generation:2` and the four lifecycle events, but never asserts that the child text is routed to the new generation, that a post-failure "stray" chunk is suppressed, or that "resumed" is delivered. A regression that drops generation-2 text routing (or forwards the stray chunk) keeps this green; the sibling `air_resume_prompt_is_the_send_message_text` covers the prompt, not the chunk routing.

### 7. LOW — TEST-MAP.md:275 never exercises the false→true flip

Row: `... stop reason propagation > only reconciles Fast mode from user-driven results, not task-notification followups`
Cited: `src/session/tests_t5a2.rs::t5a2_result_reconciles_fast_mode_and_ignores_cooldown_and_autonomous_results` (tests_t5a2.rs:664).

Upstream (`acp-agent.test.ts:10185-10250`) starts `fastModeEnabled:false`, feeds a task-notification result with `fast_mode_state:"on"` (must contribute no update), then a user-turn result with `"on"` and asserts `fastModeEnabled === true` and exactly one `config_option_update` carrying `createFastModeConfigOption(true,true)`. The Rust test starts with `fast_mode_enabled = true`, checks the task-notification result is ignored (good) and then only the true→off direction (`fast_mode_state:"off"` → notice + one update). The on-direction flip from false is never asserted here; a regression that reconciles only "off" results would pass.

### 8. MEDIUM — TEST-MAP.md:364 proves only the fingerprint, not the teardown

Row: `... getOrCreateSession param change detection > tears down existing session when mcpServers change`
Cited: `src/agent.rs::b117_session_fingerprint_matches_upstream_param_change_detection` (agent.rs:5225).

Upstream (`acp-agent.test.ts:13776-13800`) drives `resumeSession` with a changed `mcpServers` list and asserts the old session is disposed: `settingsManager.dispose` called, `abortController.signal.aborted === true`, `agent.sessions["s1"] === undefined`, and `createSession` invoked. The Rust test only asserts `compute_session_fingerprint(base) != compute_session_fingerprint(with_server)` (plus the other fingerprint cases). No Rust test drives the resume path with changed `mcpServers` and asserts the live session is dropped/recreated (agent.rs:3762 is the comparison; `tests/t5b_resume_load.rs`, `t1ga_recreate.rs` etc. all use empty `mcpServers`). A regression where a fingerprint mismatch no longer removes the cached session would keep the gate green.

### 9. MEDIUM — TEST-MAP.md:438 asserts the info text but not its `_meta`

Row: `... assembled assistant text fallback > marks info-level notices even when their text has no severity prefix`
Cited: `src/session/tests_t2a.rs::t2a_informational_transcript_line_for_a_plain_client` (tests_t2a.rs:355).

Upstream (`acp-agent.test.ts:17085-17103`) asserts that the info-level frame yields `messageChunkTexts == ["plain hook detail"]` *and* `chunks[0].update._meta == {claudeCode:{kind:"informational",level:"info"}}`. The Rust test feeds a warning line then `{"level":"info","content":"plain info"}`, asserts the two chunk texts, and asserts `_meta` only on `updates[0]` (the warning line). The info chunk's `_meta` is never checked, so a regression that omits the informational meta on info-level lines (the exact subject of the row) stays green.

### 10. MEDIUM — TEST-MAP.md:472 tests stream end without any cancel

Row: `... post-error recovery > rejects (not 'cancelled') a prompt enqueued after a cancel when the stream then ends`
Cited: `src/turn/turn_tests.rs::stream_end_settles_active_rejects_queued` (turn_tests.rs:598).

Upstream (`acp-agent.test.ts:18359-18390`) makes turn 1 active, calls `agent.cancel`, *then* enqueues turn 2, and asserts turn 1 resolves `{stopReason:"cancelled", usage, _meta}` while turn 2 rejects `/start a new session/`. The Rust test never cancels: it echoes p1, enqueues q1, calls `on_stream_end()` and asserts p1 settles `EndTurn` (not `cancelled`) and q1 fails with `SESSION_ENDED_MESSAGE`. The "after a cancel" precondition and the `cancelled` outcome for the active turn — the whole point of the row — are absent, and the failure message ("start a new session") is not checked against the wire text.

### 11. LOW — TEST-MAP.md:497 omits the abandoned turn's streamed text

Row: `... output after a cancel > does not let the next prompt take the result of a prompt the cancel abandoned`
Cited: `src/turn/turn_tests_t1a.rs::f13_result_naming_cancelled_prompt_does_not_settle_next_prompt` (turn_tests_t1a.rs:524).

Upstream (`acp-agent.test.ts:20256-20320`) runs three prompts, cancels with `still_queued: []`, and asserts first and second both `cancelled`, the third `end_turn` with `usage.inputTokens === 0`, and `texts == ["Starting the build.", "Here is the third answer."]` — i.e. the abandoned prompt's streamed "Here is the second answer." must not leak into the third. The Rust test asserts only that a result stamped `p2` does not settle `p3`, then that `p3`'s own result settles it. It never asserts the second prompt's `cancelled` settlement, the third's usage, or that the abandoned text is excluded — the wire-visible half of the row.

### 12. MEDIUM — TEST-MAP.md:522 resolves the hold at idle, not at the result

Row: `... deferred settlement for live background subagents > resolves at the result when the subagent already settled during the turn`
Cited: `src/turn/turn_tests_t2a.rs::t2a_level_signal_ends_and_unends_the_hold` (turn_tests_t2a.rs:134).

Upstream (`acp-agent.test.ts:21709-21730`) asserts the prompt resolves `end_turn` *before* the SDK's trailing idle is yielded (`idleYielded === false`), i.e. the deferral releases at the result once the subagent already settled. The Rust test starts from `held_for_subagent()`, feeds a task level with no subagent, forces `owed_trailing_idles = 0`, and only then asserts `on_idle()` settles p1 `EndTurn`. It proves the hold can release at idle, not that it releases at the result; the result-time release is covered only by `stream_end_settles_held_with_deferred`/`autonomous_followup_*`, not by the cited fn.

### 13. MEDIUM — TEST-MAP.md:546 does not assert the streamed answer is not re-emitted

Row: `... deferred settlement for live background subagents > preserves a queued turn's streamed answer across an interleaved autonomous result`
Cited: `src/turn/turn_tests.rs::autonomous_result_does_not_touch_live_turn` (turn_tests.rs:313).

Upstream (`acp-agent.test.ts:22796-22830`) streams turn 2's answer before its echo, interleaves a task-notification result, then a zero-output result repeating the text, and asserts `events.filter(e => e === "chunk:streamed answer")` has length **1** (the fallback must not re-emit it). The Rust test asserts only that an autonomous result neither fails nor settles a live turn (and that p2's own result settles it). It never models a queued turn with already-delivered text, and never asserts a single delivery; a regression that re-emits the streamed answer via the result fallback would keep this test green.

### 14. LOW — TEST-MAP.md:603 does not assert recovery after the idle failure

Row: `... turn abandoned by the SDK (issue #825) > fails the in-flight prompt when the SDK goes idle without emitting a result`
Cited: `src/turn/turn_tests.rs::inv_31_idle_without_result_fails` (turn_tests.rs:204).

Upstream (`acp-agent.test.ts:25884-25920`) asserts prompt 1 rejects `/without a result/` **and then** that a second prompt on the same stream resolves `end_turn` with `usage.inputTokens === 10` (the session recovers). The Rust test asserts only the failure (`FailureKind::NoResult`, `TURN_NO_RESULT_MESSAGE`) and that the active slot is cleared. It never asserts that the machine/session recovers for the next prompt, so a regression that leaves the machine wedged after the failure would keep this fn green.

### 15. LOW — TEST-MAP.md:654 asserts the parent stamp, not that the beat survives

Row: `... tool_progress heartbeats > keeps a child tool beat in the legacy flattened transcript`
Cited: `src/session/frames.rs::t2a_tool_progress_inside_a_subagent_stamps_the_parent` (frames.rs:693).

Upstream (`acp-agent.test.ts:27567-27585`) runs a `task_started` frame for `agent-1` and a beat for `toolu_inner` whose parent is `toolu_task`, and asserts the produced `toolCallId`s are exactly `["toolu_inner"]` (the beat is kept in the flattened transcript). The Rust test seeds `MapState`/`live` by hand, asserts the `_meta.claudeCode.parentToolUseId` stamp, and that it is absent without a live parent; it never asserts the beat's `toolCallId`/presence in the flattened output, nor drives the `task_started` path that upstream uses.

### 16. MEDIUM — TEST-MAP.md:2158 asserts only the `BashFacts` variant, not the image content

Row: `src/tests/tools.test.ts | Bash terminal output > toolUpdateFromToolResult > with image array tool_result (local Bash image output path) > should surface image content as ACP image content (terminal disabled)`
Cited: `src/tools/reporters.rs::t2a_bash_image_output_is_content` (reporters.rs:472).

Upstream (`tools.test.ts:1152-1168`) asserts the produced update is exactly
`content: [{type:"content",content:{type:"image",data:"iVBORw0KGgo=",mimeType:"image/png"}}]`
and `_meta` is undefined. The Rust test calls `bash_facts(...)` and asserts only
`matches!(facts, BashFacts::Content(Some(_)))` — no data, no mimeType, no ACP block shape, no `_meta` check. A regression that emitted the image data as a text block, dropped the mimeType, or attached terminal `_meta` would keep this green. (The nearby `inv_75_bash_structured_image_and_background_skip` asserts only the `ContentBlock::Image` variant, likewise not the data.)

### 17. MEDIUM — TEST-MAP.md:2194 never asserts the singular title

Row: `src/tests/tools.test.ts | toolInfoFromToolUse - ReportFindings > should report a singular title and empty findings array`
Cited: `src/tools.rs::f6a_report_findings_line_truthiness` (tools.rs:3256).

Upstream (`tools.test.ts:2577-2590`) asserts `title === "Report 1 finding"` for one finding and `"Report findings: none found"` for the empty array. The Rust test asserts `"Report 3 findings"` (plural, three findings) and `"Report findings: none found"`, plus the content lines. The singular pluralization branch — the row's distinguishing expected value — is never asserted, so a regression that renders "Report 1 findings" passes.

### 18. MEDIUM — TEST-MAP.md:2304 does not reproduce the restored mtime

Row: `src/tests/tools.test.ts | Skill tool rendering > _meta.jetbrains.air.skill.path > picks up a rewrite that restored the modification time of the file`
Cited: `src/tests_t8c.rs::t8c_skill_path_picks_up_a_rewrite` (tests_t8c.rs:1125).

Upstream (`tools.test.ts:4727-4740`) pins the file's mtime with `utimesSync`, rewrites the `SKILL.md` with the new declared name, resets the *same* mtime, asserts `statSync(file).mtimeMs === before`, and only then asserts the renamed skill resolves. The Rust test writes the file, asserts `resolve("renamed") == None`, rewrites it (which changes mtime) and asserts `resolve("renamed") == Some(file)`. It never restores/asserts the mtime, so it does not test the property the row names: an mtime-keyed cache would pass this test. The evidence note "the resolver has no mtime cache" is not proven by the cited test.

### 19. MEDIUM — TEST-MAP.md:1826 never tests `sessionArchive`/`sessionRename` alone

Row: `src/tests/session-index.test.ts | sessionIndex negotiation > is advertised with sessionArchive and sessionRename only to an AIR client that declares it`
Cited: `tests/t2b_index.rs::t2b_capabilities_are_advertised_only_after_negotiation` (t2b_index.rs:910).

Upstream (`session-index.test.ts:276-298`) asserts four cases: `air("sessionIndex")` → baseline+index caps, `air("diffPatch")` → baseline, `air("sessionArchive","sessionRename")` → baseline (**declaring archive/rename alone enables nothing**), and a non-AIR client → only `{steering:{supported:true}}`. The Rust test negotiates `air_client()` (which declares `sessionIndex`) and asserts the five capabilities, then negotiates `None` and asserts empty; it never exercises an AIR client that declares only archive/rename (the row's distinguishing case) nor a non-AIR client. A regression that advertised `sessionArchive`/`sessionRename` without `sessionIndex` would pass.

### 20. MEDIUM — TEST-MAP.md:1836 omits the null-record, non-worktree and own-list cases

Row: `... session/list of a sessionIndex client > marks the rows of sessions in a worktree Claude Code made (agentWorktree)`
Cited: `tests/t2b_index.rs::t2b_list_marks_a_worktree_claude_code_made` (t2b_index.rs:423).

Upstream (`session-index.test.ts:557-610`) asserts five rows: made→`true`, enteredExisting→omitted, user-made→omitted, a later `null` record wins→omitted, main checkout with a misleading record→omitted; the field is omitted rather than `false`; and the worktree's own list carries `true`. The Rust test writes only two transcripts (`made` and `enteredExisting`) and asserts `agentWorktree==true` for the first and absent for the second. The `null`-record-wins case, the user-made and main-checkout negatives, and the own-list case are not asserted, so a regression that marked any linked-worktree row (including `left`) would pass.

### 21. LOW — TEST-MAP.md:1850 does not assert the archived row's `_meta` flag

Row: `... _session/archive and _session/unarchive > append AIR's archive title records, are idempotent and keep updatedAt`
Cited: `tests/t2b_index.rs::t2b_archive_and_unarchive_are_idempotent_and_keep_updated_at` (t2b_index.rs:690).

Upstream (`session-index.test.ts:942-980`) asserts the appended `titleRecords`, byte-identical files on the second archive/unarchive, `row.title`, `row.updatedAt === before.updatedAt`, **`row._meta.jetbrains.air.archived === true`**, and that AIR writes no sidecar. The Rust test asserts the returned stored title, `None` on the second call (idempotent), the archived list's `updatedAt` and title, and the restored list; it never checks the `archived` `_meta` flag or the no-sidecar rule. A regression that listed the row without `air.archived` would pass.

### 22. MEDIUM — TEST-MAP.md:1873 does not assert the shared watcher

Row: `... _session/list/subscribe > shares one watcher between two subscriptions of a cwd, each notified`
Cited: `tests/t8d_session_index.rs::t8d_two_subscriptions_share_one_scope_and_are_both_notified` (t8d_session_index.rs:617).

Upstream (`session-index.test.ts:1516-1550`) asserts `handles().watches === 1` (and `watchers` unchanged) after the second subscribe, that both subscription ids are notified, that unsubscribing the first leaves one watcher and only the second is notified, and that unsubscribing both leaves `{watches:0,watchers:0,timers:0}`. The Rust test only asserts that both subscription ids receive a change for a newly written transcript; it never reads the handle counts, so "shares one watcher" (the first half of the row title) is unproven: two watchers would pass.

### 23. MEDIUM — TEST-MAP.md:1924 never tests the non-AIR branch that keeps the prefix

Row: `... the stored title of an archived session at turn end > is published without the prefix to an AIR client only`
Cited: `src/session/tests_t2a.rs::t2a_remembered_title_and_client_mapping_apply_to_the_next_publish` (tests_t2a.rs:2288).

Upstream (`session-index.test.ts:2696-2712`) loops three clients: `air("sessionIndex")` → `"Done"`, `air()` → `"Done"`, `{}` (non-AIR) → `"[archived] Done"`. The Rust test installs one stripping client-title mapping (the AIR case), remembers `"[archived] Old"`, and asserts the snapshot and the next publish `"Old"`. The "only" half — a non-AIR client must keep `[archived] ` — is never exercised, so a regression that stripped the prefix for every client would pass.

### 24. MEDIUM — TEST-MAP.md:1994 tests the listing cache, not the page re-enumeration

Row: `... the pages of one list > enumerates again once a transcript is added or removed`
Cited: `src/session_index/dir_listing.rs::rereads_a_listing_only_when_an_entry_changes` (dir_listing.rs:248).

Upstream (`session-index.test.ts:4557-4580`) drains a 30-session list in pages of 10, adds a transcript after page 1 and removes one after page 2, and asserts `enumerate` was called **3** times, the added row appears immediately after `sessions[25]`, the removed row is absent, and the total is 30. The Rust test only checks `DirListings::names`/`reads`: unchanged directory read once, a file append does not re-read, an added entry re-reads, a fresh directory is read twice. It never drives pagination, the enumerate count, or the position/removal expectations, so a bug in the page loop (e.g. stale cursor after an add) would keep it green.

### 25. MEDIUM — TEST-MAP.md:716 asserts only `protocolVersion`, not the v1 handshake fields

Row: `src/tests/acp-v2.test.ts | ACP protocol routing > serves the unchanged v1 handshake to a v1 client`
Cited: `tests/t6b_v2_router.rs::m106_without_opt_in_the_connection_stays_v1` (t6b_v2_router.rs:558).

Upstream (`acp-v2.test.ts:253-276`) asserts `protocolVersion === 1`, `agentCapabilities.loadSession === true`, `sessionCapabilities.resume` deep-equals `{}`, `authMethods.length > 0` with an `id` on each, and awaits the `_auth/status_update` notification. The Rust test only asserts `init["protocolVersion"] == 1` and then that a v2-shaped `session/new` returns `modes` (i.e. it stays v1). The handshake fields `loadSession`, `sessionCapabilities.resume`, `authMethods` and the auth status notification are never checked; a regression that dropped any of them would pass.

### 26. LOW — TEST-MAP.md:732 does not assert the end event's `_meta` or the state snapshot

Row: `... ACP v2 prompts > ends a resume with replay with one end event for a client with loadReplayEnd`
Cited: `tests/t8d_create_options.rs::t8d_live_load_replay_end_is_sent_after_the_replay` (t8d_create_options.rs:549).

Upstream (`acp-v2.test.ts:1094-1135`) asserts the exact update kind sequence (`user_message`, `user_message_chunk`, `agent_message`, `agent_message_chunk`, `session_info_update`, `state_update`), the end event's exact body `{session_info_update, _meta:{jetbrains:{air:{version:1,loadReplayEnd:true}}}}`, the trailing `state_update idle`, and that a resume without replay sends no end event. The Rust test asserts only `kinds == ["user_message_chunk","end"]` for a user-only history. It does not pin the end event's `_meta`/body, the `state_update` after it, or the no-replay negative (the sibling `..._before_the_response` covers the body for a different path).

### 27. MEDIUM — TEST-MAP.md:749 omits the child session and the ordering

Row: `... the tool calls that a cancel stops > ends the tool call of a background subagent as cancelled when the SDK reports it stopped while the root is idle`
Cited: `src/session/tests_g3.rs::t9a_g3_a_stopped_background_subagents_call_closes_cancelled_on_v2` (tests_g3.rs:298).

Upstream (`acp-v2.test.ts:1988-2065`) asserts the child's `state_update` `{state:"idle",stopReason:"cancelled"}` on session `agent-1`, the `toolu_child` update carries `sessionId:"agent-1"` and `status:"cancelled"`, the child idle comes *before* the tool status, and only one report ended the call. The Rust test builds the map state by hand and asserts only `last_status(notes,"toolu_child") == "cancelled"`; it never asserts the session id, the child idle, the ordering, or the single-report rule, so a regression that reported the cancellation on the root session (or twice) would pass.

### 28. MEDIUM — TEST-MAP.md:765 injects the cancelled cycle end directly, bypassing the cancel

Row: `... ACP v2 model cycles that Claude Code starts on its own > ends a cycle that the client cancels with idle cancelled`
Cited: `src/session/tests_t6a.rs::t6a_a_cancelled_cycle_reports_idle_cancelled` (tests_t6a.rs:318).

Upstream (`acp-v2.test.ts:2724-2755`) drives a prompt, waits for `endedTurn + running + agent_message_chunk`, sends `session/cancel`, and asserts the trace ends with `idle cancelled` (the cancel stops the follow-up cycle). The Rust test calls `v2.start_cycle()`, then `set_cycle_end(ForegroundEnd::Ended(TurnOutcome{Cancelled}))` and `after_events([CycleEnded])`, asserting only `trace == ["idle cancelled"]`. The client cancel and the running/chunk lead-in are not modelled, so a regression where a cancel fails to mark the cycle cancelled (and it instead ends `end_turn`) would still satisfy this test if some other path set `Cancelled`.

### 29. MEDIUM — TEST-MAP.md:781 checks turn-machine internals, not the v2 trace

Row: `... ACP v2 model cycles that Claude Code starts on its own > ends a cycle that the interrupt stopped cancelled, when Claude Code had started it before the cancel arrived`
Cited: `src/turn/turn_tests_t1a.rs::f16c_autonomous_cycle_is_foreground_work` (turn_tests_t1a.rs:1146).

Upstream (`acp-v2.test.ts:3337-3380`) asserts the v2 update trace `endedTurn, running, agent_message_chunk, idle cancelled, running, agent_message_chunk, idle cancelled` — two cycles that the interrupt stopped, both ending `idle cancelled`. The Rust test asserts machine predicates (`runs_autonomous_cycle`, `work_end_reported`, `drops_stopped_work_output`, `gate_stream_event`) around `start_autonomous_cycle`/`cancel`/`on_result`; it never produces or asserts an `idle cancelled` v2 update, so the wire-visible outcome of the row is untested by the cited fn.

## FEATURES.md DONE sample (30 rows, stride 31)

### 30. LOW — 13 DONE rows cite line ranges that do not contain the claimed code

Rows (FEATURES.md line; cited range -> what is actually there at ae4b9f4):
- M-01 (L77): `agent.rs:1044-1068` is `session_rpc_error`/`dropped_intake_error`; the cwd validation is `validate_cwd` at `agent.rs:2311-2350`, `system_prompt::initialize_fields` at `agent.rs:2365` (the `system_prompt.rs:40-41` ref is the `invalid_params` helper).
- U-20 (L207): `map.rs:503-507` is `unmark_emitted`; the string-content mapping is `map_consolidated_ctx` at `map.rs:1241-1251`.
- C-22 (L309): `session.rs:496-500` is the `AskUserQuestionAnswered` command variant and `1719-1723` is `cancel`; the capability check is `session.rs:1230-1235` / `7058-7063`.
- O-120 (L441): `sdk_options.rs:220` is the `project_config_root` field (json-schema is 232-234/894-898); `process.rs:471-476` is `--model` (the push is 492-495).
- O-166 (L472): `sdk_options.rs:771` is plugin parsing and `process.rs:368-370` is `SpawnOptions` fields; the option is `sdk_options.rs:210/965` and the argv push `process.rs:590`.
- R-10 (L551): `session.rs:1633` is `has_unsettled_turn`, `3229` is `AsyncTaskRuntime::new`; elicitation is `session.rs:485-493` / `1933-2050`.
- N-31 (L587): `session.rs:6810-6822` is the tool-result/hook block; the reconcile call sites are `session.rs:7981/8392` and `sync_fast_mode_state` at `7137`.
- B-21 (L655): `turn.rs:735-760` is `mark_resumed`/`note_assistant_model`; autonomous detection is `turn.rs:1639-1650` (used at 874).
- B-53 (L686): `agent.rs:2767` is `CreateElicitation` and `transcript.rs:2671` is child replay grouping; the marker is `transcript.rs:1624-1632`, emitted at `agent.rs:3199-3207/3937-3940`.
- B-84 (L717): `auth_status.rs:560-566` is the `st()` test helper; the epoch drop is `inv_50_actor_bump_epoch_drops_in_flight` (`auth_status.rs:884-900`).
- T-23 (L787): `hooks.rs:303-329` is `HookRegistry`, `415-460` is the diff patch, `551-602` is `post_tool_use_update`; Task parsing is `hooks.rs:641-1014`. The note's test refs (1309/1396/1380/1424) are also wrong; the named tests are at 1723/1794/1810/1838.
- T-55 (L819): `tools.rs:152-154` is `tool_meta`; the AIR flag is `tools.rs:287` (asserted at 2340/2343).
- P-45 (L917): `permission_options.rs:330-359` is the lexical path helpers; computer-use is `permission_options.rs:379-480` (test at 1870).

Failure scenario: a reviewer following any of these citations lands on unrelated code and cannot verify the DONE claim; if the real implementation later regresses, the row still looks evidenced. The behaviours themselves exist at the locations listed above (spot-checked), so this is an evidence-quality defect, not a missing feature.

### 31. LOW — O-77/P-13's secondary citations are stale while the primary one holds

O-77's `system_prompt.rs:132-150` (`MetaPrompt`/`meta_system_prompt`) does prove append-as-preset / string-as-replace, and P-13's `permission_mode.rs:66-88` does prove the settings-default branch. Their other cited ranges are stale: O-77's `agent.rs:687-692` is the `ClaudeOptions` copy, `803` the resumed-mode read, `909` async-task stop, `1031` `rpc_error`; P-13's `agent.rs:390-406` is image-content mapping. So the "applied on session/new, load, resume, fork" half of O-77 and the "explicit > resumed > default" ordering of P-13 are not proven by the cited lines. The actual call sites are `agent.rs:3023/3564/3751/4035` (O-77) and `permission_mode.rs:134-180` + `agent.rs:810` (P-13).

DONE
