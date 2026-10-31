# Concurrency Audit: Shared Mutable State & Deadlock Safety (CONC-2)

Document version: Turn 4 fix round, Group T4FC (CONC-2)  
Scope: `rust/claude-agent-acp-rs` crate

---

## 1. Concurrency Architecture & Policy Summary

The crate follows a shared-nothing actor architecture:

- **No synchronous locks in production code**: there are zero `std::sync::Mutex`,
  `std::sync::RwLock`, `tokio::sync::Mutex`, `tokio::sync::RwLock` or
  `tokio::sync::broadcast` imports/usages in `src/` (verified by
  `tests/conc_no_lock_across_await.rs`, which fails on a synthetic offending
  snippet). `parking_lot` is the permitted replacement but is currently unused.
- **Actor tasks**: session state lives in `session::run_loop`; the registry in
  `agent::run_registry_actor`; child stdin in the `control::spawn` writer task;
  session outbound updates in the forwarder task. They communicate over
  `tokio::sync::mpsc` channels plus `oneshot` replies, and `watch` for
  cancellation/session views.
- **Atomics** are used only for scalar flags/counters, with the orderings listed
  below (mostly `SeqCst` for cancellation, `Relaxed` for monotonic counters).
- **No locks are held across `.await`** because no locks exist. Channel waits do
  cross `.await`; section 4 describes those wait graphs honestly instead of
  claiming deadlock impossibility.

---

## 2. Shared Mutable State Inventory

| Component / File | Shared Mutable State | Synchronization Mechanism | Held Across `.await`? | Deadlock Risk | Mitigation / Verification |
|---|---|---|---|---|---|
| `session.rs` | High-water mark for updates & transcript delivery | `Arc<AtomicUsize>` | N/A (atomic scalar) | None | Monotonic counter via `fetch_max(1, Ordering::Relaxed)` (`session.rs:6727,6969`); reads are `load(Ordering::Relaxed)` (`:1314,11503`). Relaxed is enough: it only orders against itself. |
| `session.rs` | Session Actor state (`MapState`, `SessionState`, `active_turn`, `tool_use_cache`, `pending_permissions`) | Single-threaded actor loop owning state | N/A (actor isolation) | None | Single owner inside `run_loop`; state is never shared across task boundaries. Communicates via `mpsc::UnboundedSender<Command>`. Client round-trips are spawned, never awaited inline (INV-27, `session.rs:2739-2775`). |
| `agent.rs` | Session registry (`Registry.sessions`, `creation_params`, `auth_contexts`, `guard_states`, …) | Actor-local `HashMap`s inside `run_registry_actor` | N/A (actor isolation) | None | Owned exclusively by `run_registry_actor` (`agent.rs:1359`), a dedicated task fed by the connection's `incoming` channel. The notification side keeps a second handle map (`notified`, `agent.rs:1368`), fed by the same actor. The connection loop only forwards requests/notifications (`agent.rs:1128-1160`). |
| `agent.rs` | Request cancellation flag | `Arc<AtomicBool>` | N/A (atomic scalar) | None | Stored `SeqCst` (`agent.rs:1201,3366`), loaded `SeqCst` (`agent.rs:1793,2556,3723`). SeqCst pairs the cancel with the state it aborts. |
| `agent.rs` | Provider update count (`providerUpdate !== null`) | `Arc<AtomicUsize>` | N/A (atomic scalar) | None | `fetch_add(1, SeqCst)` at the start (`agent/recreate.rs:149`) and `fetch_sub(1, SeqCst)` in the `ProviderUpdateBusy` RAII guard's `Drop` (`agent/recreate.rs:151,170-176`); `load(SeqCst)` for `logout` (`agent.rs:3722`). |
| `agent.rs` | Global session ID generator | `static COUNTER: AtomicU64` | N/A (atomic scalar) | None | Monotonically increments via `fetch_add(1, Ordering::Relaxed)` (`agent.rs:291`); uniqueness does not need ordering. |
| `agent.rs` | Provider-update gate | `Arc<tokio::sync::Semaphore>` (`provider_gate`, `GATE_PERMITS`) | Only permits are held; no guard across `.await` | None if permits are always released | A provider update takes all permits (`try_acquire_many_owned`, `agent/recreate.rs:139`); a session request takes one. Gated requests that meet an update in flight are parked in a FIFO queue, not awaited, so the connection keeps delivering cancel/permission answers (`agent.rs:1371-1418`). Owned permits drop with their task. |
| `agent.rs` | Published session view (`sessions_view`) | `Arc<watch::Sender<HashMap<String, (Session, Value)>>>` | N/A (watch channel) | None | Written only by the registry actor (`send_modify`); readers `subscribe()`. A watch borrow is never held across `.await`. |
| `session_index/connection.rs` | Turn incarnation numbers | `Arc<AtomicU64>` | N/A (atomic scalar) | None | Atomically incremented via `fetch_add(1, Ordering::Relaxed)` (`:164`). |
| `control.rs` | Child stdin writer (`T: AsyncWrite + Send + Unpin`) | Single-writer actor task | N/A (actor isolation) | None | Stdin is owned exclusively by the `control_task` (`control.rs:271-290`). Other tasks enqueue via `mpsc::UnboundedSender<Outbound>`. |
| `control.rs` | In-flight control request correlations (`pending`, `control.rs:736`) | `HashMap<String, oneshot::Sender<Result<Value, ControlError>>>` inside the writer task | N/A (actor isolation) | None | Owned exclusively by the control task; entries are removed on `control_response`, `control_cancel_request`, rebind or close. Dropped receivers are harmless (the entry is pruned with the map at task end). |
| `process.rs` | Managed source init guard | `static MANAGED_SOURCE_GUARD: AtomicBool` | N/A (atomic scalar) | None | Plain `store(true/false, SeqCst)` in `ManagedSourceGuard::arm`/`Drop` (`process.rs:1347,1354`) and `load(SeqCst)` (`process.rs:1501`). There is no CAS; the guard is a single arming token, not a contended election. |
| `process.rs` | Probed reaper caches | `static PROBED_REAPER_CACHE: [OnceLock<(PathBuf, bool)>; 8]`, `REGISTERED_REAPER_BIN: OnceLock<PathBuf>`, `OS_MANAGED_CACHE`/`HKCU_CACHE: OnceLock<OsSourceRead>` | N/A (one-shot init) | None | `OnceLock` initializes once; the failure timestamps live in `PROBED_REAPER_FAILURES: [OnceLock<(PathBuf, AtomicU64)>; 8]` (`process.rs:1788`) updated via atomic stores, with a `PROBE_EPOCH` `OnceLock<Instant>` for the monotonic clock. |
| `session_model.rs` | Context-window cache | `OnceLock<std::sync::mpsc::Sender<CacheOp>>` + dedicated OS thread | N/A (message passing) | None | The cache map is owned by a single cache thread (`session_model.rs:1100-1120`); callers send `CacheOp` and read a reply over `std::sync::mpsc`. No lock type is used. |
| `diff.rs` | Diff memo cache | `OnceLock<std::sync::mpsc::Sender<MemoOp>>` + dedicated OS thread | N/A (message passing) | None | Same single-thread-owner pattern (`diff.rs:125-150`); no lock type is used. |
| `process.rs` | Subprocess child reaper/exit listener | `tokio::process::Child` + process supervisor | N/A (async task) | None | `Child::wait()` is awaited within a dedicated supervision task; drops and kills on timeout via `kill_on_drop(true)`. |
| `elicitation.rs` | Client-answer cancellation signals | `tokio::sync::watch::Receiver<bool>` | Only `changed()`/borrow; no guard across `.await` | None | Handlers `select!` the client round-trip against the session abort signal; an abort settles `cancel`/`cancelled`, and OAuth stops polling (`elicitation.rs` `wait_cancelled`). |

---

## 3. Verification & Automated Test Coverage

1. **Static verification of zero locks and zero guards across `.await` (`tests/conc_no_lock_across_await.rs`)**:
   - `test_no_banned_lock_types_in_src`: scans every `src/**/*.rs` line-wise
     (comments stripped, test modules included) for `std::sync::{Mutex,RwLock}`
     and `tokio::sync::{Mutex,RwLock,broadcast}` in any `use`/brace/qualified
     form, including multi-line `use a::{ … };`.
   - `test_no_lock_across_await_in_src`: tracks brace depth and fails when a
     `let guard = ….lock()`/`.read()`/`.write()` binding is still in scope at a
     later `.await`.
   - `checker_flags_banned_lock_types`, `checker_flags_lock_guard_across_await`
     and `checker_accepts_clean_snippets` prove the checker detects violations
     on synthetic input (so a green run is meaningful).

2. **Actor resiliency under child process crash (`tests/conc_adapter_death.rs`)**:
   - `adapter_death_mid_turn_yields_error` spins up an in-process agent against
     `fake-claude` playing a transcript that terminates mid-turn without a
     prompt `result`. It asserts the streamed delta arrived before the child
     died, that the prompt settles with the exact clean-exit shape
     (`stopReason: "end_turn"`, `acp-agent.ts:5764-5800`), and that the client
     closure itself succeeded (a `session/new` failure fails the test).

3. **Non-starvation under backlog (`tests/concurrency.rs`)**:
   - `inv_74_cancel_not_starved_by_backlog`: a 10,000-frame backlog does not
     starve `session/cancel`, which settles `cancelled` within < 250 ms.

---

## 4. Wait Graphs (oneshot waits between actors)

There is no lock-order deadlock because there are no locks. Channel waits can
still form a cycle if a handler awaits something that only it can deliver. The
current graph is:

- **Connection handlers → registry actor**: a handler enqueues an `Incoming`
  and awaits a `served` oneshot (`agent.rs:1135-1146`). The actor releases it
  after dispatch; for `session/prompt` it spawns the settlement waiter and
  releases immediately (`agent.rs:3882-3895`), so the connection is never held
  for a turn.
- **Registry actor → session actor**: request handlers await the session's
  command-reply oneshot. The session actor never calls back into the registry,
  so this edge is one-way.
- **Session actor → control task**: `Control::send_request`/`enqueue_request`
  await a `control_response` oneshot served by the control task. The control
  task never awaits the session actor, so this edge is one-way. Client
  round-trips (`permission`, future `elicitation`/dialog) are spawned tasks,
  never awaited inline in the actor (`session.rs:2739-2775`, INV-27), so a
  pending client answer cannot stall `session/cancel`.
- **Relay/forwarder tasks → session actor / client**: the stream relay pushes
  frames into an unbounded `mpsc`; the forwarder drains `SessionOutbound` and
  sends notifications. Neither awaits the actor.

Consequences and remaining hazards:

- The claim "deadlock is impossible" is **not** made. A future wiring change
  that awaited a client answer inline in the registry actor or the session
  actor would create a cycle: the answer travels through the connection's
  dispatch into the same actor that is blocked. The invariant to keep is
  therefore "no actor awaits a client round-trip inline"; this is why
  `handle_permission` spawns, and why elicitation handlers must be wired the
  same way in a later lane.
- The registry actor parks gated requests rather than awaiting them while a
  provider update holds the semaphore (`agent.rs:1371-1418`), so the gate
  cannot starve cancellation/permission traffic.
- The only unbounded wait on a client is the elicitation/dialog round-trip;
  it is now abortable through the session's `watch` signal (`elicitation.rs`),
  and the OAuth flow stops polling on abort. The prompt error flush is bounded
  by `ERROR_FLUSH_BOUND` (`session.rs:6983`).
