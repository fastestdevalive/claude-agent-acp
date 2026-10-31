# G10a progress (ROADMAP F6b)

Worktree: /private/tmp/claude-501/par-c, base HEAD 61577cb (branch feat/rust-port).
Group: G10a = F6b (tool-call result bugs, parent stamping, subagent transcript).

## Result: READY:G10a

## Checklist (all done)
- [x] Read upstream: tool-calls/renderer.ts, acp-agent.ts 12806-12945, 13083-13340, 13424-13480, 7660-7690, 4470-4500, 10947-10953
- [x] Read Rust: map.rs, tools.rs, tool_meta.rs, hooks.rs plan entries, session.rs tool_result call site
- [x] Failing-first unit tests (map.rs, hooks.rs, session.rs)
- [x] Implementation
- [x] Corpus scenarios captured with capture.sh (Node oracle) + run_and_diff tests
- [x] Rows flipped in FEATURES.md (in place; header counts untouched)
- [x] TEST-MAP rows updated for the ported/covered upstream cases
- [x] Full gate green

## Files changed
- `rust/claude-agent-acp-rs/src/map.rs`
  - T-19: every tool_use cached by id before branching (TodoWrite included).
  - T-09/T-20: tool_result arm rewritten to Node order: clear emitted, take cache;
    untracked + surfaced -> `{status, rawOutput}` with NO `_meta`; untracked +
    never surfaced -> dropped; TodoWrite result silent; Task* result silent
    (books/tracker cleared).
  - U-23: an emitted call with `input_complete == false` returns no update
    (empty stream-start input cannot refine a permission-surfaced call).
  - T-14/N-47/T-07: `tools::should_emit_tool_call` gate on the streamed partial
    input refinement.
  - T-15: insertion-ordered `tool_call_parents` memory (1000 cap, delete+set
    semantics), recorded on tool_call/refinement/partial-refinement and dropped
    on a final result / close.
  - T-16: insertion-ordered `resolved_tool_names` memory (1000 cap), filled from
    the tool-use cache on every user frame before the result prunes it.
  - C-42: `forward_subagent_text` flag; nested assistant text/thinking is
    forwarded (streamed + consolidated remainder) when the client gets the
    transcript, and stamped with `_meta.claudeCode.parentToolUseId`; AIR keeps
    nested streamed text internal without the capability (`acp-agent.ts:7680`).
  - `stamp_chunk_parent` stamps nested message/thought chunks (Node 13476-13480).
- `rust/claude-agent-acp-rs/src/tools.rs`: `should_emit_tool_call` (port of
  `shouldEmitToolCall`, acp-agent.ts:12820-12824).
- `rust/claude-agent-acp-rs/src/hooks.rs`: U-27 empty-activeForm truthiness in
  `task_state_to_plan_entries` (tools.ts:324).
- `rust/claude-agent-acp-rs/src/session.rs`: MapState gets the transcript flag
  from `ClientCapabilities`; the tool_result call site resolves a
  permission-surfaced TodoWrite OR Task* call (was Task* only) and carries the
  non-execution sidecar (`meta_with_parent_and_nonexec`).
- `rust/claude-agent-acp-rs/tests/differential.rs`: 5 new `run_and_diff` tests
  appended at the end.
- `porting/corpus/` + `porting/fixtures/`: `todo-write-result`,
  `task-tools-stream`, `subagent-nested-tool`, `edit-error-result`,
  `subagent-text-transcript` (Node-captured via capture.sh; every divergence
  ported, no normaliser weakened).
- `porting/parity/FEATURES.md`: U-23, U-27, C-42, N-47, T-07, T-09, T-14, T-15,
  T-16, T-19, T-20 flipped to DONE with upstream+Rust evidence; T-76 evidence
  refreshed (already DONE).
- `porting/parity/TEST-MAP.md`: 15 rows for the ported/covered upstream cases
  (TodoWrite permission resolution, untracked result, streamed suppression,
  Task* plan snapshots, scenarios subagent-nested/todo-write/task-create-update/
  subagent-transcript-extension).

## Scope notes
- `hooks.rs` was edited although it is not in the work order's file list: U-27's
  empty-activeForm fallback lives only in `task_state_to_plan_entries` there
  (the row's own evidence names hooks.rs:313-316,447). Change is ~6 lines.
- C-42 residual: the Node adapter also sends the SDK option
  `forwardSubagentText: true` in the initialize control_request when the
  capability is set; Rust still hardcodes `false` in `agent.rs`
  (`initialize_options_for`). `agent.rs` is off-limits for this group (F2c), so
  that half is left to the agent.rs owner. The client-visible forwarding half
  (the row's actual behaviour) is complete and differential-covered.
- T-21 (permission-surfaced TodoWrite settle, HAZARD) is NOT touched:
  unfinished.rs / turn.rs / settle code are unmodified. The map-level clearing
  (emitted/cache/books) incidentally makes the ignored `inv_24_todo_perm_settle`
  differential pass now (verified with `--ignored`); it stays `#[ignore]`d and
  excluded from the full corpus, as the other group owns un-ignoring it.

## Commands run (all real output)
- `cargo fmt --manifest-path rust/Cargo.toml --all` -> clean (gate's `--check` passes)
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` -> PASS
- `cargo test -p claude-agent-acp-rs --lib` -> 447 passed, 0 failed
- `cargo test -p claude-agent-acp-rs --lib turn` -> 60 passed, 0 failed
- `cargo test -p claude-agent-acp-rs --test differential` -> 80 passed, 0 failed, 1 ignored
  (5 new: inv_24_todo_write_result, inv_24_task_tools_stream,
  inv_24_subagent_nested_tool, inv_24_edit_error_result,
  inv_24_subagent_text_transcript; `inv_24_full_corpus` green)
- `bash porting/capture.sh <5 names>` -> captured + normalised
- `bash rust/scripts/rust-gate.sh` -> `==> gate OK`
- JsonPath guard: `git diff -U0 455c146 -- ...tests ...acp-recorder ...fake-claude | grep '^+' | grep -c 'JsonPath::new'` -> 0
- No new `#[ignore]` in the diff; `git diff -- src/turn.rs src/turn src/unfinished.rs` -> empty
- `test-map-check --map` -> PASS (broken targets 0, bad syntax 0)

## Environment
- UPSTREAM_CACHE_DIR=/Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1/rust/target/upstream (read-only)
- No CARGO_TARGET_DIR override (build dir is this worktree's rust/target).
