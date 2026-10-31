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
      msg = stream.recv() => { ... }
  }
  ```
- **Cooperative points**: Every iteration of `tokio::select!` polls the branches. Because the branches are `biased;`, Tokio evaluates them in strict syntactic order:
  1. `force_cancel_sleep` (deadline check)
  2. `cmd = commands.recv()` (command channel carrying `Command::Cancel`, `Command::Prompt`, `Command::Steer`)
  3. `msg = stream.recv()` (inbound frame stream from the child process stdout codec)
- **Starvation impossibility**: Even if `stream` contains an unbounded backlog of 10,000 buffered frames, on the very next iteration after `Command::Cancel` is enqueued into `commands`, branch (2) is polled **before** branch (3). The actor processes the cancel immediately, settling the active turn and aborting permissions. At most **1 frame** can be processed between cancel enqueue and cancel handling.
- **Decision**: `EventLoopYielder` and `messageLoopYielder.maybeYield()` are **NOT ported as code** in Rust. The Tokio runtime's channel primitives and `biased; select!` provide a stronger, deterministic non-starvation guarantee without artificial timers, sleep budgets, or runtime overhead.

### 1.3 Verification & Proof (`inv_74_cancel_not_starved_by_backlog`)
The non-starvation guarantee is verified by integration and actor tests in `rust/claude-agent-acp-rs/tests/concurrency.rs`:
- `inv_74_cancel_not_starved_by_backlog`: Runs an end-to-end ACP agent over duplex channels against `fake-claude`. The child emits an immediate backlog of 10,000 `stream_event` chunks upon prompt receipt. While streaming is actively underway, the client sends `session/cancel`.
- **Result**: The prompt is settled with `stopReason: "cancelled"` in < 250ms (well under the 2,500ms bound) without waiting for the 10,000 frame backlog to drain. 10 consecutive executions passed 10/10 with zero flakes.
- Because frames drained before cancel is measured as 0, no synthetic `tokio::task::yield_now()` every K frames is needed.

---

## 2. Measured Runtime Decision (`multi_thread` vs `current_thread`)

### 2.1 State of the Binary Before Benchmark
Prior to this evaluation, `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs` used default `#[tokio::main]`, which defaults to Tokio's multi-threaded work-stealing runtime (`multi_thread`).

### 2.2 Benchmark Methodology
A dedicated, reproducible benchmark was authored in `rust/claude-agent-acp-rs/tests/concurrency.rs` (`bench_tokio_runtime_comparison` and `run_runtime_benchmark`). It isolates:
1. **Streaming chunk throughput & latency**: Time to produce, transmit over unbounded channel, and consume 10,000 discrete chunks.
2. **Cancel latency under 10k backlog**: Time from `cmd_tx.send("cancel")` to actor settlement while 10,000 frames are pre-buffered in the stream queue, and the count of backlog frames drained before cancel execution.
3. **Steering latency under 10k backlog**: Time from `cmd_tx.send("steer")` to steer handling under a 10,000-frame backlog.

Command to run:
```bash
cargo test --manifest-path rust/Cargo.toml --test concurrency bench_tokio_runtime_comparison -- --nocapture
```

### 2.3 Measured Benchmark Numbers

| Metric | Multi-Thread (`multi_thread`) | Single-Thread (`current_thread`) | Advantage |
|---|---|---|---|
| **10k Chunks Total Time** | **2.29 ms** | 4.82 ms | Multi-thread (2.1x faster) |
| **Average Latency per Chunk** | **0.229 µs** | 0.482 µs | Multi-thread (2.1x lower latency) |
| **Cancel Latency under 10k Backlog** | **124.79 µs** | 341.21 µs | Multi-thread (2.7x faster) |
| **Frames Drained Before Cancel** | **0 frames** | **0 frames** | Parity (both instantaneous) |
| **Steer Latency under 10k Backlog** | **52.25 µs** | 288.54 µs | Multi-thread (5.5x faster) |

### 2.4 Analysis and Decision
- **Why `multi_thread` wins**: In the ACP architecture, multiple independent asynchronous actors execute simultaneously:
  1. Stdio / transport reader and writer tasks (`codec::spawn_codec`)
  2. Child process stderr tailing and SIGCHLD supervision
  3. Session actor loop (`session::run_loop`)
  4. Agent request / notification dispatch (`agent::serve`)
  Under `current_thread`, all tasks compete cooperatively for single-thread execution slices. Heavy frame deserialization or JSON processing in one task delays scheduling in another. Under `multi_thread`, Tokio dispatches I/O and processing across worker threads, providing >2x higher streaming throughput and 2.7x-5.5x lower control latency.
- **Starvation invariance**: Both runtimes drain exactly **0 frames** before cancel is handled, proving that Tokio's `select! { biased; ... }` priority guarantee holds identically in both single-threaded and multi-threaded configurations.
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
2. **Clean JSON-RPC Errors**: If `panic = "abort"` is used, any unexpected panic aborts the OS process (`SIGABRT`). The stdio transport closes abruptly without sending a JSON-RPC response, leaving the host client hanging until external timeout. Under `panic = "unwind"`, Tokio catches the unwind at the task boundary (`JoinHandle` returns `Err(JoinError::Panic)`). The channel drop converts into `Err(SessionError::Closed)` in `agent.rs:1990`, and `responder.respond_with_error(...)` returns a clean JSON-RPC internal error (-32603) to the client immediately.
3. **Deterministic Cleanup (RAII)**: Unwinding ensures destructors run: child process handles with `kill_on_drop(true)` terminate orphaned `claude` subprocesses, closing file descriptors and releasing temporary resources.

### 3.2 Session Actor Panic Containment
- **Current Behavior**:
  - The session actor is spawned via `tokio::spawn(run(...))`.
  - When the actor panics, the task unwinds. Tokio catches the panic and marks the `JoinHandle` terminated.
  - The actor's oneshot sender `pending: HashMap<String, oneshot::Sender<...>>` is dropped upon unwinding.
  - In `agent.rs:1978-2025`, the awaiting prompt handler observes `reply_rx.await` returning `Err(RecvError)`.
  - `agent.rs` handles this via `.unwrap_or_else(|_| Err(SessionError::Closed))` and responds with:
    `responder.respond_with_error(rpc_error(session_error_to_rpc(&e)))`
  - The in-flight prompt settles immediately with an error rather than hanging. Sibling sessions continue execution unaffected.
- **Test**: `inv_panic_isolation_and_prompt_settlement` in `tests/concurrency.rs` verifies that a task panic does not take down the runtime, yields `JoinError::is_panic()`, and cleanly drops the reply channel without hanging.
- **CONC-2 Scope (Future Refinement)**:
  - While prompt settlement and process survival are verified, the panicked session's entry currently remains in `registry.sessions` until explicit `session/close` or agent shutdown.
  - In CONC-2 (Stage 12), a session supervisor / join task will watch the actor `JoinHandle`, log the panic reason via `Logger`, and remove the dead session from `Registry.sessions` proactively, emitting a `SessionEnded` failure notification if configured.

---

## 4. Shared Mutable State Audit Table (Skeleton)

The following audit skeleton categorizes every mutable state location across the adapter architecture, comparing Node's implementation to the Rust port:

| State Component | Node Representation (`@agentclientprotocol/claude-agent-acp`) | Rust Representation (`claude-agent-acp-rs`) | Concurrency Mechanism & Thread-Safety |
|---|---|---|---|
| **Session Registry** | `sessions: Map<string, ACPAgentSession>` | `Registry.sessions: HashMap<String, Session>` (`agent.rs:1075`) | Owned by single-task request handler. Cloned handles passed to notification handler via lock-free `mpsc::unbounded_channel` (guard G5). |
| **Settings Managers** | `settingsManagers: Map<string, SettingsManager>` | `Registry.settings_managers: HashMap<String, SettingsManager>` (`agent.rs:952`) | Stored in `Registry`; file watcher runs in separate task with debounced reload channel. |
| **Active Turn State** | `activeTurn: Turn \| null`, `queuedTurns: Turn[]`, `heldTurn: Turn \| null` | `TurnMachine` (`turn.rs`) | Owned exclusively by `session.rs` actor task `run_loop`. No multi-threaded lock required. |
| **Simultaneous Turn High-Water Mark** | `highWaterMark: number` | `Arc<AtomicUsize>` (`session.rs:444`) | Thread-safe atomic counter; incremented and checked across prompt enqueue / settlement. |
| **Pending Prompt Oneshot** | `pendingPrompts: Map<string, {resolve, reject}>` | `pending: HashMap<String, oneshot::Sender<Result<PromptReply, SessionError>>>` | Owned exclusively by actor `run_loop`. One-shot channels provide single-producer, single-consumer safe transfer. |
| **Pending Permissions** | `openUserInputs: Map<string, {resolve, reject}>` | `pending_permissions: HashMap<String, JoinHandle<()>>` (`session.rs:1222`) | Owned by actor `run_loop`. Handles are aborted on `Command::Cancel` / `Command::Close`. |
| **Async / Background Tasks** | `liveBackgroundTasks: Map<string, string>`, `taskRuntime: AsyncTaskRuntime` | `live_background: HashMap<String, String>`, `tasks: AsyncTaskRuntime` | Owned exclusively by actor `run_loop`. |
| **Modes & Config State** | `session.config`, `session.modes`, `session.prePlanMode` | `SessionExt.config`, `ModesState` (`session_ext.rs`, `session_config.rs`) | Owned exclusively by actor `run_loop`. Modified only via `Command::ModeApplied` / `Command::SetConfigOption`. |
| **Context Window Cache** | `providerContextWindows: Map<string, number>` | `UsageState.context_window`, `session_model.rs` cache | Seeded at session start; updated via `Command::UpdateContextWindow`. |
| **Request ID Rewriter** | `ndJsonStream` counter | `RequestIds` (`codec.rs:172`) | Owned by line-codec transport; assigns numeric wire IDs and tracks correlation. |
| **Child Process PGID / Reaper** | Node child process tracking | `process.rs` reaper pipe & supervisor PGID | Managed via separate supervisor process (`--reaper`) on Unix (INV-6, F41); on Windows via Job Object with `KILL_ON_JOB_CLOSE` (with documented post-spawn assignment race window, Decision D8). |
| **Auth & Provider State** | `currentAuthStatus: AuthStatus \| null`, `authEpoch: number` | `Registry.auth: Option<AuthStatus>`, `auth_status.rs` | Stored in `Registry`; probed per-prompt and cached. |

*(Note: In CONC-2, this skeleton will be populated with field-level cross-references, line numbers, and the >=200-iteration race suite results across all shared state transitions).*
