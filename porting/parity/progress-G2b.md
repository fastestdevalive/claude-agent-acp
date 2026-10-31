# Progress — Turn 1, Group G2b (PC-2 session/load replay)

## Status

READY:G2b

## Scope completed

- PC-2.1 SDK `Iwe` meta filter + `isCompletedLocalCommand`
  - `transcript.rs`: `meta_origin_kept` / `is_filtered_meta_record` drop every
    `isMeta` record unless origin kind is channel/observer/observer-activity/
    slack-ping, or a peer WITH `senderTaskId` (the adapter then hides the
    surviving origins). Caveats/skill expansions no longer replay as
    `user_message_chunk`.
  - `completed_local_command_indices` ports `vwe` and marks the output records
    of a completed local command with `isCompletedLocalCommand`.
  - Same path: a marker-only command without a `<command-args>` tag now keeps
    the command name (the B-48 divergence; row not flipped, it is `[pkg:F31]`).
- PC-2.2 SDK terminal-node leaf walk + capped streaming read
  - `select_leaf` ports `ed`: branch tips (excluding teamName/progress/
    fork-briefing attachments) walk up to the nearest user/assistant, with the
    SDK fallback; a tip that is not a user/assistant can select an earlier
    leaf than the newest record.
  - `read_records_capped` streams with `BufReader` (no `read_to_string`) and,
    above the SDK's 5 MiB `$L`, keeps only records from the last
    `compact_boundary` (precompact skip). `session_messages_async` runs the
    read in `spawn_blocking`.
  - The temp-dir counter (always 0) is now a static `AtomicUsize`.
- PC-2.3 replay Task* plan
  - `replay_with_task_state` rebuilds `task_state` from replayed
    TaskCreate/TaskUpdate/TaskList via the `hooks.rs` helpers (no
    `tool_use_result` on replay, matching Node) and emits `plan` updates with
    the AIR dedupe.
  - `Session::seed_task_state` seeds the live actor from `agent.rs` (new load
    and already-live load), so a status-only TaskUpdate after the load applies.
- PC-2.4 Node-captured corpus (fixtures captured with `porting/capture.sh`)
  - `session-load-replay-meta` (caveat/skill/channel/peer+`senderTaskId`/
    peer-without-sender, kept compact summary)
  - `session-load-replay-completed-command`
  - `session-load-replay-branch` (attachment tip; leaf is `a1`, not the newest
    `u3`)
  - `session-load-replay-task-plan` (TaskCreate/TaskUpdate/TaskList replay +
    status-only TaskUpdate after the load)
  - differential: `pc2_4_session_load_replay_scenarios`; the four are also in
    the full-corpus test.
- PC-2.5 no regression: `session-load-replay`, `session-load-replay-live`,
  `session-resume` stay green; full differential 59 passed / 1 ignored.

## FEATURES rows flipped

- `B-02` PARTIAL → DONE (`agent.rs:787-877`, `transcript.rs:633-641`): replay
  now runs concurrently with the CLI start (`tokio::join!`) and before the
  response, with the transcript read off the runtime.

Not flipped (not tagged `[pkg:PC-2]`): B-43 stays DONE (adapter-level meta
hide kept); B-48 remains PARTIAL although the missing-`<command-args>`
divergence was fixed; B-50/B-55/B-56 untouched (compaction entity, event-loop
yield, queue-command/branch-turn walk are still not ported).

## TEST-MAP

No direct upstream test case was ported: PC-2 follows the SDK `sdk.mjs`
(`Iwe`/`ed`/`nR`) and the adapter's `replaySessionHistory`; the TEST-MAP
snapshot rows are all `missing:T-UNCLASSIFIED` and are owned by INFRA-1
classification, so none were touched.

## Files changed

- `rust/claude-agent-acp-rs/src/transcript.rs`
- `rust/claude-agent-acp-rs/src/agent.rs` (replay wiring + concurrent start)
- `rust/claude-agent-acp-rs/src/session.rs` (`Command::SeedTaskState`,
  `Session::seed_task_state`)
- `rust/claude-agent-acp-rs/tests/differential.rs`
- `porting/corpus/session-load-replay-{meta,branch,completed-command,task-plan}.{acp.json,transcript.jsonl}`
- `porting/corpus/seeds/session-load-replay-{meta,branch,completed-command,task-plan}.jsonl`
- `porting/fixtures/session-load-replay-{meta,branch,completed-command,task-plan}.{frames.jsonl,argv.json}` (capture.sh)
- `porting/parity/FEATURES.md` (B-02)
- `.vibekit/feature-plans/pending/parity-closeout/plan-parity-closeout.md` (PC-2 ticks)

No commits (orchestrator commits). No `control.rs` / `turn.rs` /
`unfinished.rs` / `async_tasks.rs` / `hooks.rs` changes; `hooks.rs` helpers
called only.

## Commands run

- `cargo test --lib` → ok, 306 passed.
- `cargo test --test differential` → ok, 59 passed, 1 ignored (was 58 + 1).
- `cargo test --test differential pc2_4` → ok.
- `cargo test --workspace` → ok (all targets; differential 59 passed).
- `cargo fmt --all -- --check` → ok.
- `cargo clippy --workspace --all-targets -- -D warnings` → ok.
- `bash rust/scripts/rust-gate.sh` → `==> gate OK`.
- JsonPath guard → `0`; no new `#[ignore]`.
- `bash porting/capture.sh <four names>` → capture complete.
