# Progress — G10c (Turn 1, review fixes for F6b)

Worktree: `/private/tmp/claude-501/par-c` @ f198cfd. Review: scratchpad/review-G10a.md.

## Checklist
- [x] 1. Result-block ordering in session.rs pre-pass (TodoWrite/Task* resolution in block order)
- [x] 2. Nested plan `parentToolUseId` stamping (map.rs, session.rs publish_task_plan)
- [x] 3. T-15/T-16 -> PARTIAL [pkg:F17], fix misleading doc comment map.rs:108-116
- [x] 4. Un-ignore inv_24_todo_perm_settle, drop from HAZARD_PENDING, flip T-21, refresh T-09 evidence, TEST-MAP rows
- [x] 5. LOW: AIR filter missing parent_tool_use_id = nested (map.rs:1562) + unit test
- [x] Acceptance: fmt, clippy, lib, lib turn, differential, workspace, gate, JsonPath 0, test-map-check, turn/unfinished diff empty
- [x] Verify C-42 row NOT claimed DONE by me (FEATURES diff contains no C-42 change)

## What changed
1. **Block order (item 1).** `session.rs` now resolves plan/suppressed tool_results and maps each
   block in one pass (`handle_user_frame_blocks`, session.rs:2206-2305) instead of a pre-pass over
   all results followed by `map_user_frame`. `map.rs` gained `remember_user_frame_tool_names`
   (map.rs:702), `map_user_frame_block` (map.rs:733) and `map_user_block`; `map_user_frame` maps
   block-by-block. Failing-first actor test `user_frame_resolution_follows_block_order`
   (session.rs). Node-captured scenario `todo-perm-multi-result` (Bash result + permission-surfaced
   TodoWrite result in one frame) -> `inv_24_todo_perm_multi_result`.
2. **Nested plans (item 2).** `plan_from_todo_write` takes the frame parent and stamps
   `_meta.claudeCode.parentToolUseId` (map.rs:1288-1318); `publish_task_plan` takes a parent and
   stamps the Task* plan (session.rs:2164-2195), hook publishers pass `None`. Unit tests
   `nested_todo_write_plan_carries_parent_tool_use_id`, `task_plan_stamps_the_subagent_parent`.
   Node-captured scenario `subagent-nested-plan` (nested TodoWrite + TaskCreate) ->
   `inv_24_subagent_nested_plan`.
3. **T-15/T-16 (item 3).** Both rows are PARTIAL `[pkg:F17]` with the exact remaining gap (no Rust
   consumer of `tool_call_parent()` / `resolved_tool_name()`; upstream consumers are
   closeOpenToolCalls/closeCycleToolCalls and asyncTasks.toolNameOf/conversation_reset). The
   misleading `map.rs` doc comments now say the consumers are not ported yet.
4. **T-21 (item 4).** Removed `#[ignore]` from `inv_24_todo_perm_settle` and the `todo-perm-settle`
   entry from `HAZARD_PENDING` in `tests/differential.rs` (the full-corpus test now covers it).
   T-21 -> DONE with both-side evidence; T-09 evidence refreshed (un-ignored corpus + block order +
   nested plan); TEST-MAP rows for the TodoWrite resolution now point at
   `covered:todo-perm-settle` and `denied_todo_resolution_at_the_tool_result_call_site`.
5. **AIR missing parent (item 5).** `drop_air_nested_text` now takes `is_strict_top_level(message)`,
   so a stream frame with the field MISSING is nested and its text is dropped like Node's
   `parent_tool_use_id !== null` gate. Unit test `air_drops_stream_text_when_the_parent_field_is_missing`.

## New files (untracked; the orchestrator must include them in the merge)
- porting/corpus/todo-perm-multi-result.acp.json
- porting/corpus/todo-perm-multi-result.transcript.jsonl
- porting/fixtures/todo-perm-multi-result.frames.jsonl
- porting/fixtures/todo-perm-multi-result.argv.json
- porting/corpus/subagent-nested-plan.acp.json
- porting/corpus/subagent-nested-plan.transcript.jsonl
- porting/fixtures/subagent-nested-plan.frames.jsonl
- porting/fixtures/subagent-nested-plan.argv.json
- porting/parity/progress-G10c.md (this file)

Both fixture sets were generated with `bash porting/capture.sh <names>` against the pinned Node
0.94.0 oracle; no fixture was hand-edited.

## Commands run (real output)
- `cargo fmt --all -- --check` -> FMT_OK
- `cargo clippy --workspace --all-targets -- -D warnings` -> clean
- `cargo test --lib` -> 481 passed, 0 failed, 0 ignored
- `cargo test --lib turn` -> 60 passed
- `cargo test --test differential` -> 84 passed, 0 failed, 0 ignored (includes
  `inv_24_todo_perm_settle` un-ignored, `inv_24_todo_perm_multi_result`, `inv_24_subagent_nested_plan`,
  `inv_24_full_corpus`)
- `cargo test --workspace` -> all binaries pass
- `bash rust/scripts/rust-gate.sh` -> `==> gate OK` (TEST-MAP CHECK PASS, MODULE-MAP CHECK PASS,
  PARITY: PASS)
- JsonPath guard -> 0; `git diff` for turn.rs/turn/unfinished.rs -> empty; no new `#[allow]`/`#[ignore]`

Note: one earlier gate run hit a pre-existing timing flake in `sdkctl-duplicate-request`
(permission request vs completed update ordering under load); it passed standalone and on both
subsequent gate runs. Unrelated to this group (single-result Bash frame, untouched path).

READY:G10c
