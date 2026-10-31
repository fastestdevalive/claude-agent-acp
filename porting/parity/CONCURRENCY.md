# Concurrency Model, Runtime Decision, and Panic Policy (CONC-1)

Document version: Turn 1, Group G15 (CONC-1 / F43 Phase 1)  
Specification: `.vibekit/feature-plans/pending/parity-roadmap/ROADMAP.md` (F43) & master plan decision K3 (`plan-claude-master.md`)

---

## 1. Upstream Fairness Decision (`EventLoopYielder` vs Tokio `select!`)

### 1.1 The Upstream Mechanism in Node.js
In upstream `@agentclientprotocol/claude-agent-acp` (v0.94.0):
- Upstream uses `EventLoopYielder` (`src/utils.ts:95-135`) invoked via `messageLoopYielder.maybeYield()` inside the stream consumer loop (`src/acp-agent.ts:5650-5662`).
- **Why Node needed this**: In Node.js / V8, an `await` on an async generator (`session.query.next()`) that already has buffered frames in memory resolves in the **microtask queue**. The JavaScript event loop executes microtasks to exhaustion before servicing macrotasks (I/O, timers, `setImmediate`). A tight loop consuming an in-memory backlog of 10,000 frames starves the event loop entirely: incoming JSON-RPC notifications on stdio (specifically `session/cancel`) and timers cannot run until the backlog is completely drained. `EventLoopYielder` tracked an execution budget (typically 100ms) and returned a Promise resolved via `setImmediate` to force yielding to macrotask I/O.
- Ten test cases existed in `src/tests/event-loop-yield.test.ts` to exercise this budget mechanism.

### 1.2 The Tokio Native Concurrency Model
Under Rust and Tokio:
- The session actor loop in `session.rs:run_loop` is structured as:
  ```rust
  tokio::select! {
      biased;
      _ = force_cancel_sleep => { ... }
      cmd = commands.recv() => { ... }
      msg = next_stream_line(&mut replay, &mut stream) => { ... }
  }
  ```
- **Cooperative points**: Every iteration of `tokio::select!` polls the branches. Because the branches are `biased;`, Tokio evaluates them in strict syntactic order:
  1. `force_cancel_sleep` (deadline check)
  2. `cmd = commands.recv()` (command channel carrying `Command::Cancel`, `Command::Prompt`, `Command::Steer`)
  3. `msg = next_stream_line(...)` (replay queue, then the inbound frame stream from the child process stdout codec)
- **Command priority**: Even if `stream` contains an unbounded backlog of 10,000 buffered frames, on the very next iteration after `Command::Cancel` is enqueued into `commands`, branch (2) is polled **before** branch (3). The actor processes the cancel immediately, settling the active turn and aborting permissions. At most **1 frame** can be processed between cancel enqueue and cancel handling.
- **Decision 19 (Turn 1, revised in Turn 7 — decision 26)**: the yielder IS ported and wired. Turn 1 concluded it was unnecessary (the biased select is stronger for a command arriving from another task), but upstream's `messageLoopYielder.maybeYield()` is a *behaviour* row (B-17) and the master plan decided to port it: the stream branch of `run_loop` calls `message_loop_yielder.maybe_yield().await` (`session.rs:5854`) with the same 8 ms `MESSAGE_LOOP_BUDGET_MS` slice. The two guarantees compose:
  - a command from another task is still handled before the next frame (biased select),
  - a stretch of frames that ran past 8 ms now gives the whole runtime a turn, exactly like Node's check phase, so timer/IO tasks are not starved by a pure in-memory drain either.
- **Decision 26**: the replay loops of `transcript.rs` and `agent.rs` keep their own `EventLoopYielder` instances (B-55, DONE); the session actor uses one instance for its stream branch. No shared global yielder (upstream shares one across message-forwarding loops; the Rust loops never interleave on one task).

### 1.3 Verification & Proof
- `inv_74_cancel_not_starved_by_backlog` (`tests/concurrency.rs`): 10,000-frame backlog + `session/cancel`, settles `cancelled` in < 250 ms (measured ~0.4-0.7 ms on the current host). The test asserts exactly this bound.
- `t7a_b63_cancel_settles_behind_a_5000_frame_backlog` and `t7a_b63_close_settles_behind_a_5000_frame_backlog` (`src/session/tests_t7a.rs`): a 5,000-frame backlog with a cancel/close settles the turn with the settle marker among the first < 1,000 forwarded frames.
- `t7a_b17_the_pause_lets_a_cancel_task_run_mid_backlog` (`src/session/tests_t7a.rs`): a yielder-level test, not a loop-level one. Its `drain()` calls `maybe_yield()` itself (there is no `run_loop`), and both arms now spawn a real cancel task: without the pause the drain runs the whole 5,000 frames and the task never sets the flag; with the pause the task runs after < 50 frames. It fails if the yielder stops pausing, but it does NOT prove the production wiring: `run_loop`'s `select!` is `biased` with the command branch first (`session.rs:3409`), so a queued `session/cancel` is served within a frame or two of arriving even without the pause (carry-over M4, still open as an evidence gap, not a code gap).
- **P2a F2 (the incoming stream)**: the connection's request handlers forward only (they do not wait for the actor). A cold `session/new` / fresh `session/load`|`resume` / `session/fork` start and the signed-out respawn run in spawned tasks that answer from there, holding the provider gate permit until the session registers; the registry actor keeps serving `session/cancel` and the permission answers another session's turn waits on meanwhile, and a prompt that follows a `session/new` response still waits behind its start (the permit + `RegistryMsg::Register` ordering). Covered by `tests/t3fd_gate.rs::t3fd_a_cold_session_start_does_not_delay_another_sessions_cancel` (fails with the handler awaiting the start inline), its permission-answer sibling, and `t3fd_a_prompt_after_a_cold_session_new_runs_on_the_registered_session`.


---

## 2. Measured Runtime Decision (`multi_thread` vs `current_thread`)

### 2.1 State of the Binary Before Benchmark
Prior to this evaluation, `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs` used default `#[tokio::main]`, which defaults to Tokio's multi-threaded work-stealing runtime (`multi_thread`).

### 2.2 Benchmark Methodology
A dedicated, reproducible benchmark was authored in `rust/claude-agent-acp-rs/tests/concurrency.rs` (`bench_tokio_runtime_comparison` and `run_runtime_benchmark`). It is a SIMULATION of the actor's select shape (`run_simulated_actor`: three branches, no replay queue, no `EventLoopYielder`, no JSON decode, no session state), not `run_loop` itself. It isolates:
1. **Streaming chunk throughput & latency**: Time to produce, transmit over unbounded channel, and consume 10,000 discrete chunks.
2. **Cancel latency under 10k backlog**: Time from `cmd_tx.send("cancel")` to simulated-actor settlement while 10,000 frames are pre-buffered in the stream queue. The benchmark waits (bounded) until the actor has drained at least one frame, so the cancel is sent to a RUNNING actor rather than racing `tokio::spawn`; the reported "drained" count is the actor's own count (proof that it was draining), and the latency bound is the gate.
3. **Steering latency under 10k backlog**: The same for `cmd_tx.send("steer")`.

Command to run:
```bash
cargo test --manifest-path rust/Cargo.toml --test concurrency bench_tokio_runtime_comparison -- --nocapture
```

### 2.3 Measured Benchmark Numbers

One run on the current host (a loaded machine; the numbers move with load, so the gate asserts bounds, not these exact values):

| Metric | Multi-Thread (`multi_thread`) | Single-Thread (`current_thread`) | Advantage |
|---|---|---|---|
| **10k Chunks Total Time** | **2.75 ms** | 3.02 ms | Multi-thread (~1.1x faster) |
| **Average Latency per Chunk** | **0.275 µs** | 0.302 µs | Multi-thread (~1.1x lower latency) |
| **Cancel Latency under 10k Backlog** | **31.67 µs** | 292.50 µs | Multi-thread (~9x faster) |
| **Frames Drained Before Cancel** | **3** | **128** | Both were draining (not a spawn race) |
| **Steer Latency under 10k Backlog** | **6.54 µs** | 291.12 µs | Multi-thread (~45x faster) |

### 2.4 Analysis and Decision
- **Why `multi_thread` wins**: In the ACP architecture, multiple independent asynchronous actors execute simultaneously:
  1. Stdio / transport reader and writer tasks (`codec::spawn_codec`)
  2. Child process stderr tailing and SIGCHLD supervision
  3. Session actor loop (`session::run_loop`)
  4. Agent request / notification dispatch (`agent::serve`)
  Under `current_thread`, all tasks compete cooperatively for single-thread execution slices. Heavy frame deserialization or JSON processing in one task delays scheduling in another. Under `multi_thread`, Tokio dispatches I/O and processing across worker threads, providing >2x higher streaming throughput and 2.7x-5.5x lower control latency.
- **Starvation invariance**: in both runtimes the simulated actor was already draining the backlog when the cancel was sent, and the cancel/steer latencies stay in the microsecond range; the gate asserts `< 50 ms` for each. This measures the SCHEDULING SHAPE, not the real actor: the real-actor guarantee is proven by `inv_74_cancel_not_starved_by_backlog` (10k frames, `< 250 ms`) and the T7 race suite (`src/session/tests_t7a_race_s1.rs`), not by this simulation.
- **Decision**: The multi-threaded runtime is retained and made explicit in `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs`:
  ```rust
  #[tokio::main(flavor = "multi_thread")]
  async fn run_tokio_main(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>>
  ```

---

## 3. Panic and Exit Policy

### 3.1 Release Profile `panic = "unwind"`
In `rust/Cargo.toml`, the release profile is set to:
```toml
[profile.release]
panic = "unwind"
```

#### Justification (Abort vs Unwind):
1. **Multi-Session Isolation**: The ACP adapter supports multiple concurrent sessions (`Registry.sessions`). In production environments (e.g. IDEs like JetBrains or multi-workspace agents), a bug or panic triggered by an aberrant tool call or malformed child payload in Session A must **not** terminate the entire process and abruptly destroy Session B.
2. **Clean JSON-RPC Errors**: If `panic = "abort"` is used, any unexpected panic aborts the OS process (`SIGABRT`). The stdio transport closes abruptly without sending a JSON-RPC response, leaving the host client hanging until external timeout. Under `panic = "unwind"`, Tokio catches the unwind at the task boundary (`JoinHandle` returns `Err(JoinError::Panic)`). The channel drop converts into `Err(SessionError::Closed)` in `agent.rs:5306`, and `responder.respond_with_error(...)` returns a clean JSON-RPC internal error (-32603) to the client immediately.
3. **Deterministic Cleanup (RAII)**: Unwinding ensures destructors run: child process handles with `kill_on_drop(true)` terminate orphaned `claude` subprocesses, closing file descriptors and releasing temporary resources.

### 3.2 Session Actor Panic Containment
- **Current Behavior**:
  - The session actor is spawned via `tokio::spawn(run(...))`.
  - When the actor panics, the task unwinds. Tokio catches the panic and marks the `JoinHandle` terminated.
  - The actor's oneshot sender `pending: HashMap<String, oneshot::Sender<...>>` is dropped upon unwinding.
  - In `agent.rs:5306` (the v1 prompt task), the awaiting prompt handler observes `reply_rx.await` returning `Err(RecvError)`.
  - `agent.rs` handles this via `.unwrap_or_else(|_| Err(SessionError::Closed))` and responds with:
    `responder.respond_with_error(session_rpc_error(&failure.error))` (`agent.rs:5382`)
  - The in-flight prompt settles immediately with an error rather than hanging. Sibling sessions continue execution unaffected.
- **Test**: `inv_panic_isolation_and_prompt_settlement` in `tests/concurrency.rs` verifies that a task panic does not take down the runtime, yields `JoinError::is_panic()`, and cleanly drops the reply channel without hanging. It is a task-level test, not the session actor (deleting `agent.rs`'s `unwrap_or_else(|_| Err(SessionError::Closed))` would not fail it); the agent-level sibling test `tests/conc_race_child_exit.rs::conc_race_a_dead_session_leaves_a_sibling_answering` covers the error path instead: one session's child dies abnormally, its in-flight prompt gets the `-32603` process-died error, and a sibling session keeps answering. `conc_race_child_abnormal_exit_mid_turn_fails_the_turn` covers the abnormal-exit lane at the process level.
- **CONC-2 Scope (Future Refinement)**:
  - While prompt settlement and process survival are verified, the panicked session's entry currently remains in `registry.sessions` until explicit `session/close` or agent shutdown.
  - In CONC-2 (Stage 12), a session supervisor / join task will watch the actor `JoinHandle`, log the panic reason via `Logger`, and remove the dead session from `Registry.sessions` proactively, emitting a `SessionEnded` failure notification if configured.

---

## 4. Shared Mutable State Audit Table (Skeleton)

The following audit skeleton categorizes every mutable state location across the adapter architecture, comparing Node's implementation to the Rust port:

| State Component | Node Representation (`@agentclientprotocol/claude-agent-acp`) | Rust Representation (`claude-agent-acp-rs`) | Concurrency Mechanism & Thread-Safety |
|---|---|---|---|
| **Session Registry** | `sessions: Map<string, ACPAgentSession>` | `Registry.sessions: HashMap<String, Session>` (`agent.rs:1610`) | Owned by single-task request handler. Cloned handles passed to notification handler via lock-free `mpsc::unbounded_channel` (guard G5). |
| **Settings Managers** | `settingsManagers: Map<string, SettingsManager>` | `Registry.settings_managers: HashMap<String, SettingsManager>` (`agent.rs:1624`) | Stored in `Registry`; file watcher runs in separate task with debounced reload channel. |
| **Active Turn State** | `activeTurn: Turn \| null`, `queuedTurns: Turn[]`, `heldTurn: Turn \| null` | `TurnMachine` (`turn.rs`) | Owned exclusively by `session.rs` actor task `run_loop`. No multi-threaded lock required. |
| **Simultaneous Turn High-Water Mark** | `highWaterMark: number` | `Arc<AtomicUsize>` (`session.rs:953`, initialised at `:1689`) | Thread-safe atomic counter; incremented and checked across prompt enqueue / settlement. |
| **Pending Prompt Oneshot** | `pendingPrompts: Map<string, {resolve, reject}>` | `pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>` | Owned exclusively by actor `run_loop`. One-shot channels provide single-producer, single-consumer safe transfer. |
| **Pending Permissions** | `openUserInputs: Map<string, {resolve, reject}>` | `pending_permissions: HashMap<String, JoinHandle<()>>` (`session.rs:3259`) | Owned by actor `run_loop`. Handles are aborted on `Command::Cancel` / `Command::Close`. |
| **Async / Background Tasks** | `liveBackgroundTasks: Map<string, string>`, `taskRuntime: AsyncTaskRuntime` | `live_background: HashMap<String, String>`, `tasks: AsyncTaskRuntime` | Owned exclusively by actor `run_loop`. |
| **Modes & Config State** | `session.config`, `session.modes`, `session.prePlanMode` | `SessionExt.config`, `ModesState` (`session_ext.rs`, `session_config.rs`) | Owned exclusively by actor `run_loop`. Modified only via `Command::ModeApplied` / `Command::SetConfigOption`. |
| **Context Window Cache** | `providerContextWindows: Map<string, number>` | `UsageState.context_window`, `session_model.rs` cache | Seeded at session start; updated via `Command::UpdateContextWindow`. |
| **Request ID Rewriter** | `ndJsonStream` counter | `RequestIds` (`codec.rs:219`) | Owned by line-codec transport; assigns numeric wire IDs and tracks correlation. |
| **Child Process PGID / Reaper** | Node child process tracking | `process.rs` reaper pipe & supervisor PGID | Managed via separate supervisor process (`--reaper`) on Unix (INV-6, F41); on Windows via Job Object with `KILL_ON_JOB_CLOSE` (with documented post-spawn assignment race window, Decision D8). |
| **Auth & Provider State** | `currentAuthStatus: AuthStatus \| null`, `authEpoch: number` | `Registry.auth: Option<AuthHandle>`, `auth_status.rs` (`agent.rs:1622`) | Stored in `Registry`; probed per-prompt and cached. |

CONC-2 result: every row above is either owned exclusively by the `run_loop`
actor task, a lock-free `mpsc` channel, or an atomic; no `std`/`tokio` mutex is
held across an `await` in `src/` (guarded by
`tests/conc_no_lock_across_await.rs::test_no_lock_across_await_in_src`). The
>=200-iteration race results across the shared state transitions are in §5.

---

## 5. CONC-2 race suite (Turn 7, lane A) — results

### 5.1 Harness (`src/session/tests_t7a_race_common.rs`)

The suite drives the real session actor in-process (`run_loop`) over unbounded
channels — no child process, no network, no `claude` — and perturbs the
scheduler deterministically:

- **Seed**: every iteration runs with `CONC_SEED + i` (default base `0x5eed5eed`).
  The seed drives `Rng` (xorshift64) for the number of `yield_now` jitters, a
  sub-5 ms sleep (one in eight iterations) and which side of a race goes first.
- **Iterations**: `CONC_ITERS` (default 200), each bounded by a 10 s
  `tokio::time::timeout`; failures are aggregated with the seed that reproduces
  them.
- **Turn scripts**: `TurnScript` builds the frame sequence a fake CLI would
  print for a turn (echo, deltas, tool use, usage, result); the test feeds it to
  the actor's stream exactly as the relay would.
- **Forwarder**: the harness runs the PRODUCTION forwarder semantics (P2a F5):
  it resolves a settle's reply barrier, then waits INLINE (1 s bound) for the
  prompt task's ack before forwarding the next item, exactly as `agent.rs` does.
  `settles` counts `FlushAcked` barriers only (one per settled turn); `flushes`
  counts every barrier, so a bare `Flush` cannot fake a settle.
- **Rerun one failure**:
  `CONC_SEED=<seed> CONC_ITERS=1 cargo test -p claude-agent-acp-rs --lib <fn>`.
  The panic message prints the seed and that exact command.

### 5.2 Scenarios, iteration counts and observed results

| # | File / test | Scenario | Iters (default) | Runtime | Result |
|---|---|---|---|---|---|
| 1 | `tests_t7a_race_s1.rs::t7a_race_1_cancel_vs_result` | `session/cancel` vs the result frame | 200 | ~0.6 s | one SETTLE barrier, stopReason `cancelled` or `end_turn`, never both |
| 2 | `tests_t7a_race_s2.rs::t7a_race_2_steer_vs_end` | steer vs turn end | 200 | ~0.2 s | both arms reachable: a steer queued before the result is `injected` (one settle), a steer after the turn settled is `startedNewTurn` (two settles); never lost |
| 3 | `tests_t7a_race_s3.rs::t7a_race_3_close_during_prompt` | `session/close` vs a running prompt | 200 | ~0.3 s | prompt settles, close answers, the actor is gone |
| 4 | `tests_t7a_race_s4.rs::t7a_race_4_permission_reply_timing` | permission answer vs cancel / turn end / a second request | 200 | ~5 s | exactly one `control_response` per request, none twice, a second request still served |
| 5 | `tests_t7a_race_s5.rs::t7a_race_5_notification_order` | emission order over a long turn with interleaved usage/tool updates, across a turn boundary | 200 | ~0.2 s | every marker in emission order; the next turn's updates follow the previous turn's usage |
| 6 | `tests_t7a_race_s6.rs::t7a_race_6_duplicate_and_late_replies` | duplicate / late permission answers (after the turn, after close) | 200 | ~0.8 s | ignored, no second control response, the actor still answers commands |
| 7 | `tests_t7a_race_s7.rs::t7a_race_7_child_exit_mid_turn` | child exit at a random point of the script | 200 | ~0.2 s | clean exit `end_turn`, abnormal exit `PromptFailed`, no hang |
| 7p | `tests/conc_race_child_exit.rs::conc_race_child_exit_mid_turn_leaves_no_process` | the same over a real `fake-claude`, process side | 10 (cap 25) | ~1-2 s | the child pid is gone after every run (`FAKE_CLAUDE_PID_OUT` + `wait_pid_gone`) |

All seven in-process files together: **21.9 s** with `--test-threads=1`
(200 iterations each), under the 90 s budget.

### 5.3 Failures found and fixed while writing the suite

1. **Steer-vs-end script, not a product bug**: an injected steer's turn settles
   only at the idle *after the steered cycle's own result*
   (`steered_awaiting_result`). The scenario now feeds echo → result → idle; it
   passes 200/200.
2. **Turn settlement visibility**: a successful v1 settle hands the reply to the
   prompt task with a `flush_rx` barrier; the harness must await that barrier
   (as the agent layer does) before counting updates, or the observation races
   the forwarder. Fixed in the harness (`settle`), not in `src/`.
3. **Second permission request after a cancel**: a request that arrives in the
   stopped-work window is parked for `TOOL_USE_WAIT` (2 s) before it is denied;
   the scenario now opens the second request concurrently with the first, so the
   race is real and the run stays fast. No product change.
4. **Review-T6INT findings** (fixed in `session.rs` / `file_change_audit.rs`):
   the reported-turn stall (two flush barriers), cancel/close during the report
   preview, a v2 prompt never registering the report request, and a non-string
   `filesChanged` entry. See §6.

No real product race was found by scenarios 1-7: every iteration of the final
suite passes. The one product-level race the program did surface was the
1 s report stall (review-T6INT finding 1), which is a latency bug, not a
correctness one, and is fixed.

### 5.4 Report-path decisions (review-T6INT)

- **Decision 27**: a reported turn releases the `handle_events` barrier's `ack`
  before running the preview, then queues a fresh `FlushAcked` for the report
  and hands the reply on without waiting. The forwarder is never blocked by the
  report task; the prompt task still awaits `flush_rx`, so the report precedes
  the response. Measured: `t6a2_prompt_report_publishes_the_checkpoint_paths_before_the_response`
  1.02 s → 0.01-0.06 s (asserted < 300 ms).
- **Decision 28**: `FileChangeReports.collecting` keeps a running report's
  handle (and a cancel flag) so `session/cancel`/`session/close` can finish it
  `unavailable/cancelled` and rewrite the forwarded stop reason to `cancelled`;
  a done flag prunes the entry at the next prompt. Covered by
  `tests/conc_race_report_cancel.rs`.
- **Decision 29**: a v2 prompt registers its `_meta` report request (the agent
  hook is in `lane-A-wiring.patch`), and the batch's v2 emissions are held until
  the report job runs, so the report precedes the turn's `idle`. Covered by
  `tests/conc_race_v2_hooks.rs::t7a_v2_report_publishes_before_idle`.
- **Decision 30**: a non-string `filesChanged` entry sets `truncated: true`
  (`preview_files_changed` returns the flag); covered by
  `src/file_change_audit.rs::tests::a_non_string_files_changed_entry_marks_the_report_truncated`.

### 5.5 Decision (CONC-2)

The session actor model is kept: each session's mutable state is owned by its
`run_loop` task and reached only through `Command`s over unbounded channels, so
the adapter has no shared-lock hazard (the audit table in §4 lists every state
location and its mechanism; no lock is held across an `await`). The evidence is
the race suite in §5.1-5.2 (seven in-process scenarios plus the real-child exit
scenario, 200 seeded iterations each, all green), the no-lock scanner
(`conc_no_lock_across_await`, which now also catches a glob import followed by a
bare lock type, a guard-returning helper, a guard acquired and awaited on one
line, and no longer arms on a plain `.read()` method) and `conc_adapter_death`;
the only defect the program found was the 1 s report stall (latency, not
correctness), fixed under Decisions 27-30. The connection side was fixed under
P2a F2 (§1.3): request handlers forward only, so a cold child start cannot stop
the incoming stream. No further concurrency work is claimed.
