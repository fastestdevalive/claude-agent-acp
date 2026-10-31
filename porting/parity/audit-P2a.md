# AUDIT P2a — the concurrency program (AUDP2a)

Head audited: `ae4b9f4` (`docs(parity): TURN 9 COMPLETE status`), gate green per the
task. Read-only audit: no source or doc file was edited; only this report and
`porting/parity/progress-AUDP2a.md` are written.

Upstream reference (authoritative): Node `@agentclientprotocol/claude-agent-acp`
v0.94.0 at
`/private/tmp/claude-501/-Users-gb--vibe-station-projects-claude-acp-rust-eval-worktrees-care-1/1eead38c-f6eb-4631-9090-0cb461816615/scratchpad/upstream-wt/v0.94.0`.

Files read for this audit:

- `porting/parity/CONCURRENCY.md` (236 lines, CONC-1 + CONC-2 + the Turn 7 race suite)
- `rust/claude-agent-acp-rs/src/session/tests_t7a_race_common.rs` (400)
- `…/tests_t7a_race_s1.rs` … `…/s7.rs` (57/122/71/192/137/187/64)
- `rust/claude-agent-acp-rs/tests/conc_no_lock_across_await.rs` (302)
- `…/conc_adapter_death.rs` (234), `…/conc_race_child_exit.rs` (191)
- `…/conc_race_report_cancel.rs` (504), `…/conc_race_v2_hooks.rs` (543)
- `…/tests/concurrency.rs` (412) — the CONC-1 evidence the doc cites
- `src/session.rs` (`run_loop`, the yielder, the settle barriers), `src/agent.rs`
  (`serve_until`, `run_registry_actor`, `handle_v1_request`, the prompt path)
- `src/v2/agent.rs` (the v2 capture path), `src/session/tests_t7a.rs` (the
  yielder/backlog tests the doc cites)
- `agent-client-protocol` 2.1.0 `src/jsonrpc/incoming_actor.rs` (how the SDK
  drives the handlers this crate registers)

---

## 1. What CONCURRENCY.md decides

1. **§1 Fairness.** Node needed `EventLoopYielder` because V8 drains microtasks to
   exhaustion; Tokio's `tokio::select! { biased; force_cancel_sleep, commands.recv(),
   next_stream_line() }` gives the command branch priority, *and* the yielder is
   still ported and wired into the stream branch (Decision 19/26) with the same
   8 ms `MESSAGE_LOOP_BUDGET_MS`. Two guarantees compose: command-before-frame and
   an 8 ms slice for the rest of the runtime.
2. **§2 Runtime.** `multi_thread` retained and made explicit in
   `src/bin/claude-agent-acp-rs.rs`; justified by a benchmark in
   `tests/concurrency.rs` (10 k chunks, cancel and steer latency under a 10 k
   backlog).
3. **§3 Panic/exit policy.** `panic = "unwind"` in `rust/Cargo.toml`; a panicking
   session actor must settle its in-flight prompt with a clean JSON-RPC error and
   leave sibling sessions running. Panicked sessions stay in `registry.sessions`
   until an explicit close (deferred to CONC-2, which never landed).
4. **§4/§5 Decision (CONC-2).** Every mutable state location is owned by the
   `run_loop` actor, a lock-free `mpsc` channel, or an atomic; no `std`/`tokio`
   mutex is held across an `await`; the evidence is seven seeded in-process race
   scenarios (200 iterations each) plus a real-child exit scenario, and the
   `conc_no_lock_across_await` scanner.

The decision itself (single owner actor, no locks) is sound and, as far as this
audit can check, is implemented. The findings below are about **the evidence and
the claims**, plus one real serialization property the doc does not mention.

---

## 2. The race test files, one line each

Format: seeded? / sleeps as synchronisation? / key assertion / scenario.

1. **`src/session/tests_t7a_race_common.rs`** — harness, no test of its own.
   Seeded (per-iteration `Rng::new(base + i)`, xorshift64); jitter = 0..=max
   `yield_now` + a 1-in-8 sub-5 ms sleep. Sleeps are *not* the synchronisation:
   the only timing device is `updates_until_idle(within)` (a quiet-window drain)
   and the 10 s per-iteration timeout. Key assertion: none; `settle()` returns
   `Err` on a hang or a dropped reply. Scenario: shared.
2. **`s1.rs::t7a_race_1_cancel_vs_result`** — seeded (which side is enqueued
   first), jitter(4), plus a fixed `sleep(2 ms)` "let the loser be processed"
   window. Key assertion `flushes != 1` (one settle ⇒ one response barrier).
   Scenario 1 (cancel-vs-result).
3. **`s2.rs::t7a_race_2_steer_vs_end`** — seeded, jitter(4), `sleep(2 ms)` before
   the flush count in the injected arm; the startedNewTurn arm polls with up to
   200 `yield_now`s. Key assertions: outcome ∈ {injected, startedNewTurn},
   `flushes == 1` (injected) / `== 2` (new turn). Scenario 2 (steer-vs-end) —
   see F4: only the `injected` arm is reachable.
4. **`s3.rs::t7a_race_3_close_during_prompt`** — seeded (order), jitter(4), and a
   `sleep(1 ms)` polling loop (≤100 iterations) to observe the actor gone. Key
   assertions: prompt settles `cancelled`/`end_turn`, close answered, a later
   `Command::Close` finds the actor gone. Scenario 3 (close-during-prompt).
5. **`s4.rs::t7a_race_4_permission_reply_timing`** — seeded (mode 0..3 and which
   answer goes first), jitter(4); no sleep as synchronisation, only deadline
   windows (2 s + a 20 ms extra window). Key assertion: exactly one
   `control_response` per `request_id`, zero extra, and the prompt settles.
   Scenario 4 (permission-reply timing).
6. **`s5.rs::t7a_race_5_notification_order`** — seeded (`a_count`/`b_count`,
   jitter between the `a` deltas only), no sleeps. Key assertion: the exact
   position of every marker (`usage` first, `a*`, `tool:toolu_A`, `b*`, result
   usage, next turn's `c*`). Scenario 5 (notification order).
7. **`s6.rs::t7a_race_6_duplicate_and_late_replies`** — seeded jitter only;
   deadline windows (2 s, 30 ms) are timeouts, not sync. Key assertions: the
   first answer yields exactly one response; a duplicate (after the turn and for
   a request that never existed) yields zero extra; the actor still answers
   `ForegroundState`; a closed session refuses the answer. Scenario 6
   (duplicate/late replies).
8. **`s7.rs::t7a_race_7_child_exit_mid_turn`** — seeded (script length, abnormal
   flag, jitter), no sleeps. Key assertions: clean exit ⇒ `end_turn`, abnormal
   exit ⇒ `SessionError::PromptFailed`, the actor is gone after close. Scenario 7
   (child exit mid-turn), in-process.
9. **`tests/conc_race_child_exit.rs`** — **not** seeded/jittered: `deltas =
   (i*7) % 40`, `CONC_ITERS` default 10 clamped to 1..=25. `wait_pid_gone` polls
   with a 25 ms thread sleep. Key assertion: `stopReason == "end_turn"` and the
   published pid is gone. Scenario 7, process side — clean exit only (see F6).
10. **`tests/conc_adapter_death.rs`** — not seeded, one run, no sleeps. Key
    assertions: `stopReason == "end_turn"`, no error field, the delta was seen
    before the death. Scenario 7, wire level, clean exit.
11. **`tests/conc_race_report_cancel.rs`** — v1 arm is **not** seeded and uses a
    fixed `sleep(250 ms)` to land the cancel inside the 600 ms preview; v2 arm is
    seeded 1..=5 with jitter and synchronises on the result's `usage_update`
    frame. Key assertions: `cancelled`, report `unavailable/cancelled`, exactly
    one `idle` whose `stopReason` is `cancelled`. Report-path race, not one of
    the seven.
12. **`tests/conc_race_v2_hooks.rs`** — not seeded, no sleeps; functional v2
    assertions (F1 auth respawn, F5 sessionFailure, U-88 auth push, F3 permission
    ordering, report-before-idle). Not a race test.
13. **`tests/conc_no_lock_across_await.rs`** — static line scanner over `src/`,
    with synthetic self-tests. Not a race test (see F7 for its holes).

### 2.1 Scenario coverage

Covered: all seven, 1→s1, 2→s2 (partially, F4), 3→s3, 4→s4, 5→s5, 6→s6, 7→s7 +
the two process/wire tests.

**Not covered anywhere in the suite** (grep of all `conc_*.rs` and
`tests_t7a_race_s*.rs` for `Command::Close`/`Permission`/`Steer` combinations):

- `session/close` while a permission request is open — s3 runs no permission, s4
  runs no close. The actor must answer the withdrawn request *and* end.
- child exit while a permission request is open — s7 has no permission; the
  pending permission, the prompt and the teardown all have to settle.
- `steer` racing `cancel` (both commands in the queue at once), and `steer`
  racing `close`.
- `cancel` arriving **before** the prompt is enqueued (the ordering the design
  relies on is "cancel after prompt"; the reverse — a cancel for a session with
  no turn, or a cancel that lands first — is untested).
- two sessions in one connection: every race test drives exactly one session, so
  the registry-level races (provider gate parking vs `session/cancel`, a
  `session/new` while another session's turn is live) are untested. See F2.
- notification order **across a cancel boundary** (s5 only crosses a clean turn
  boundary).
- child exit during the file-change report preview, or during the v2 held-idle
  window (report_cancel covers cancel there, not exit).
- duplicate/late `steer` replies (s6 covers permission replies only).

---

## 3. Findings

### F1 — MEDIUM — the doc's cancel-latency claim is 10× the assertion it cites

`porting/parity/CONCURRENCY.md:38` claims `inv_74_cancel_not_starved_by_backlog`
"settles `cancelled` in < 250 ms". The test asserts a **2500 ms** bound:

- `rust/claude-agent-acp-rs/tests/concurrency.rs:189-192`:
  `assert!(elapsed < Duration::from_millis(2500), …)`.

The test also sleeps 20 ms before sending the cancel (`:162`), and the comment at
`:188` says "Generous bound … well under 2.5s". A reader taking 250 ms from the
doc as the budget would accept a 2.4 s regression as evidence. The property
itself is real; the stated number is not.

### F2 — HIGH — a request handler that awaits child work blocks the whole incoming stream, including `session/cancel` and permission answers

The design comment at `src/agent.rs:1185-1187` states the invariant: *"The
connection's handlers only forward: the connection loop also carries the answers
to the agent's own requests (permissions, `fs/*`, `terminal/*`), so no handler may
wait for something those answers unblock."* The prompt handler honours it
(enqueue synchronously, spawn the settlement, `src/agent.rs:4803-4862`). These
handlers do not:

- `session/new` — `src/agent.rs:3551` → `Session::start(...).await` at `:3628`,
  which spawns the child and awaits the initialize handshake
  (`src/session.rs:1098-1101`, `control.initialize(&opts.initialize).await`);
- `session/load`/`session/resume` — `start_resumed(...).await` at
  `src/agent.rs:4014` (`start_resumed` is `:3005`), and the changed-params arm
  awaits `session.close().await` inside the spawned recreate;
- `session/fork` — `start_resumed(...).await` at `src/agent.rs:4118`;
- `session/prompt` under `--hide-claude-auth` — `respawn_signed_out_session(...).await`
  at `src/agent.rs:4630`/`:4974` → `recreate::recreate_live_session(...).await`
  (`:2252`), a full teardown + child start awaited inline.

Those awaits run on the registry actor (`run_registry_actor` awaits
`handle_request(...)` inline, `src/agent.rs:1471-1478`), and the handler itself
runs inline in the SDK's incoming loop: `agent-client-protocol` 2.1.0
`src/jsonrpc/incoming_actor.rs:143-151` awaits `dispatch_dispatch(...)`, which
awaits `handler.handle_dispatch_from(...)` (`:488-528`), and the crate's request
handler then awaits the actor's `ack` (`src/agent.rs:1196-1206`). The SDK's own
doc says it plainly: *"no other messages can be processed"* while a handler
awaits (`agent-client-protocol-2.1.0/src/jsonrpc.rs:555`).

**Concrete failure scenario.** Session A's turn has a `can_use_tool` request open;
the client answers it while, in the same connection, the user opens a second
workspace (`session/new`). The `session/new` handler is blocked on the new child's
initialize (cold `claude` start: hundreds of ms to seconds). While it is blocked:

1. the client's **response** to `session/request_permission` for A is a JSON-RPC
   response frame, handled in the same incoming loop → not read → A's turn cannot
   proceed;
2. a `session/cancel` for A is a notification, handled in the same loop → not
   read → A's turn keeps streaming after the user pressed stop.

Upstream Node dispatches handlers without awaiting them, so neither happens
there. The doc never mentions this serialization: §1.2/§1.3 prove the guarantee
*inside the session actor* (`select! { biased; }`), which holds, but the cancel
first has to cross the registry actor and the SDK loop. It is a latency/ordering
divergence, bounded by the child start — not a deadlock (none of these handlers
waits for a client answer).

### F3 — MEDIUM — the panic-containment test does not touch the session actor or `agent.rs`

`CONCURRENCY.md:110` presents `inv_panic_isolation_and_prompt_settlement`
(`tests/concurrency.rs:384-412`) as the verification of §3.2 ("the awaiting
prompt handler observes `reply_rx.await` returning `Err(RecvError)` … responds
with -32603"). The test spawns an anonymous task that panics, asserts
`JoinError::is_panic()`, and asserts a oneshot whose sender was dropped yields
`Err`. It never starts a session actor, never calls `Session::start`, never goes
through `agent.rs`'s prompt path, and never checks a JSON-RPC error. Delete the
`unwrap_or_else(|_| Err(SessionError::Closed))` / `respond_with_error` path in
`agent.rs` and this test still passes.

The line reference is stale too: `CONCURRENCY.md:106` cites `agent.rs:1978-2025`
for the prompt handler; that range is `apply_deferred`'s `RegistryMsg` handling
(`:1996` is `RegistryMsg::Evict`). The prompt settle path is now
`src/agent.rs:4849-4861`.

### F4 — MEDIUM — s2's `startedNewTurn` arm is unreachable, so "steer vs turn end" is only tested as "steer during a turn"

`tests_t7a_race_s2.rs:27` sends `Command::Steer` **before** the result frame is
fed (`:30`/`:32`). `run_loop`'s select is `biased;` with the command branch first
(`src/session.rs:3409-3411`), so on the iteration where both are ready the steer
is handled first, against a live turn → `injected`. The turn can only end when
the result frame is processed, which the actor does after the queued steer. So
the `startedNewTurn` arm (`:70-115`) and its `flushes == 2` assertion never run.

`CONCURRENCY.md:170` therefore overstates the evidence: "outcome `injected` or
`startedNewTurn`, never lost; one settle for an injected steer, two for a new
turn". The real race the scenario is named for — a steer that arrives *after* the
turn settled and has to start a new one — is not exercised anywhere.

### F5 — MEDIUM — the race harness's forwarder is not the production forwarder

`CONCURRENCY.md:5.1` says the harness runs "the forwarder a production agent
layer would run". It does not. For `FlushAcked`, the harness sends `done` and
**spawns** the ack wait (`tests_t7a_race_common.rs:184-190`), while production
awaits `ack` **inline** with a 1 s timeout (`src/agent.rs:2691-2694`). Under
production semantics a prompt task that is slow to ack stalls the session's whole
update stream for up to 1 s per settle; the suite never exercises that, so an
ordering/latency regression introduced by the inline wait cannot be caught here.

Secondary: the harness counts *both* `SessionOutbound::Flush` and `FlushAcked`
into `flushes` (`:180-190`), so any unrelated `Flush` barrier emitted by the
actor would make s1/s2's `flushes == 1` fail spuriously. It does not happen today
(no permissions in s1/s2), but the assertion is not measuring "one settle".

### F6 — MEDIUM — the process-side child-exit test checks the pid after aborting the agent, so it cannot prove the exit path reaped the child

`tests/conc_race_child_exit.rs:158-175`:

```rust
let client_res = timeout(10s, client_task).await;
serve_task.abort();
let _ = serve_task.await;
…
if !wait_pid_gone(pid, 5s) && !pid_is_gone(pid) { … }
```

Aborting the serve future drops `serve_until`'s locals — every `Session` and its
`Process`, which is `kill_on_drop(true)` (`src/session.rs:1033-1036`) — so the
child is killed *by the abort*, not necessarily by the exit path. The assertion
"the child pid is gone after every run" (`CONCURRENCY.md:176`) can therefore pass
even if the mid-turn exit path leaks the child. It also only drives the clean
exit (`exit: true`, status 0, `:46`); there is no abnormal-exit process-level
variant.

### F7 — MEDIUM — the no-lock scanner has holes and a false positive, so "no lock across await in `src/`" is weaker than claimed

`tests/conc_no_lock_across_await.rs`:

- `banned_lock_types` (`:71-124`) only flags a line containing the literal
  `std::sync`/`tokio::sync`. `use std::sync::*;` followed by
  `let g = Mutex::new(0);` is invisible: the import line has no `Mutex` token and
  the use line has no `std::sync`.
- `lock_across_await` (`:128-173`) only recognises `let … .lock()/.read()/.write()`
  on one line. A guard obtained through a helper (`fn guard(&self) -> MutexGuard`)
  or held in a struct field is invisible; so is a guard whose binding line also
  contains `.await` (`:146` skips the check when `binding` is true).
- False positive: *any* `let x = foo.read()` arms a "guard". `src/session_index/live_registry.rs:246`
  is `let snapshot = by_session(self.read());` — a plain method named `read`, not
  a lock. It is harmless only because that function contains no `.await`; a
  later `.await` in the same block would fail the gate on legitimate code.

The real tree has **zero** `Mutex`/`RwLock` (verified: `grep -rn "Mutex\|RwLock"
src/ tests/` returns only a doc comment in `src/session_ext.rs:6`), so the
conclusion holds — but it holds by inspection, not because the scanner is sound.

### F8 — LOW — every line reference in §4 and §3.2 points at unrelated code

Checked at head `ae4b9f4`:

| Doc says | Actually at that line | Real location |
|---|---|---|
| `session.rs:444` high-water `Arc<AtomicUsize>` | `prepared: crate::mcp_command::McpPrepared` | `session.rs:953` / `:1030` |
| `session.rs:1222` `pending_permissions` | `patched.display_name = current_model_id.clone();` | `session.rs:3259` |
| `session.rs:3346,5768` `maybe_yield()` | a comment / `reply.send(Ok(ext.config…))` | `session.rs:3398`, `:5854` |
| `agent.rs:952` `settings_managers` | `AuthRequired => None,` | `agent.rs:1603` |
| `agent.rs:1075` `Registry.sessions` | `}` | `agent.rs:1589` |
| `agent.rs:1990` `SessionError::Closed` | `self.auth_contexts` | `agent.rs:4849-4861` |

The §4 table is the doc's audit artefact; none of its "verifiable" pointers can be
verified at the stated line.

### F9 — MEDIUM — the runtime benchmark measures a toy loop, and its key assertion is a spawn/send race

`CONCURRENCY.md:2.2-2.4` reports precise numbers (2.29 ms / 124.79 µs / 52.25 µs)
as the basis for keeping `multi_thread`. They come from `run_runtime_benchmark`
(`tests/concurrency.rs:258-327`), whose cancel and steer arms drive
`run_simulated_actor` (`:210-245`) — a three-branch copy of the select shape with
no replay queue, no `EventLoopYielder`, no JSON decode, no session state. It is
not `run_loop`.

`frames_drained_before_cancel` is 0 because `cmd_tx.send("cancel")` happens on the
same task that just spawned the actor, before the actor's first poll — not because
of the actor's priority. The gate assertion `frames_drained <= 1`
(`:365-374`) is therefore a race between `tokio::spawn` and `cmd_tx.send`; on a
loaded machine the worker can drain a cooperative-budget batch (>1 frame) before
the send, failing the gate. And §2.4's "proving that Tokio's select!{biased}
priority guarantee holds identically in both configurations" is not proven for
the real actor by this test.

### F10 — LOW — `t7a_b17`'s "starves the cancel task" arm has no cancel task

`src/session/tests_t7a.rs:173-205`: the no-pause arm asserts
`drain(&cancel, &mut yielder, false).await == 5_000` with `cancel` set to `false`
and **no spawned task** to set it. That assertion is a tautology — it passes
whatever `maybe_yield` does, and it would pass with the yielder removed. The
pause arm (`at < 50`) is the only arm that can fail, and it tests
`EventLoopYielder` in isolation, never `run_loop`. So `CONCURRENCY.md:40`'s "a
synchronous drain that never pauses starves the cancel task for the whole 5,000
frame backlog … fails without the yield" is only half demonstrated.

### F11 — LOW — s6 silently swallows the duplicate answer's reply

`tests_t7a_race_s6.rs:151`: `let _ = tokio::time::timeout(Duration::from_secs(2), reply_rx).await;`.
If the actor drops or never answers the duplicate `Command::PermissionAnswered`,
the test still passes. The "ignored" behaviour is only checked through the
absence of a second `control_response`, not through the command's own reply.

### F12 — LOW — sleep-based windows in s1/s2/s3 make the suite timing-dependent

- s1 `tests_t7a_race_s1.rs:45` — `sleep(2 ms)` before counting the loser's
  barrier; a loser that settles after 2 ms is counted as "one settle" and the
  double-settle is missed (false pass).
- s2 `:62` — same `sleep(2 ms)`; `:103-109` — a 200-`yield_now` spin for the
  second flush; a slower settle fails with a spurious error.
- s3 `:60-67` — a 100 × `sleep(1 ms)` loop to observe the actor gone; a teardown
  that takes >100 ms fails the test though the behaviour is correct.

These are not the harness's seeded jitter; they are fixed windows that both
weaken the assertion (s1) and add flake risk (s2/s3).

---

## 4. Summary

- The CONC-2 decision (single owner actor, no locks, commands over unbounded
  channels) is implemented: **zero** `std`/`tokio` `Mutex`/`RwLock` in `src/` or
  `tests/` (grep, only a doc comment mentions one).
- The seven scenarios exist, are wired (`src/session.rs:12067-12088`), seeded, and
  their key assertions do fail on the guarded behaviour — with the exceptions in
  F4 (one arm unreachable) and F12 (fixed windows).
- The doc overstates three pieces of evidence (F1 latency, F3 panic containment,
  F9 benchmark) and cites stale lines throughout (F8).
- One real behaviour the doc does not mention: request handlers that await child
  work serialize the entire incoming stream (F2), so `session/cancel` and
  permission answers can be delayed by an unrelated `session/new`/`respawn`.
---

## 5. Commands run (real output)

```
$ cd rust && CARGO_TARGET_DIR=/private/var/folders/…/audp2a-target \
  cargo test -p claude-agent-acp-rs --lib t7a_race_ -- --test-threads=1
running 7 tests
test session::tests_t7a_race_s1::t7a_race_1_cancel_vs_result ... ok
test session::tests_t7a_race_s2::t7a_race_2_steer_vs_end ... ok
test session::tests_t7a_race_s3::t7a_race_3_close_during_prompt ... ok
test session::tests_t7a_race_s4::t7a_race_4_permission_reply_timing ... ok
test session::tests_t7a_race_s5::t7a_race_5_notification_order ... ok
test session::tests_t7a_race_s6::t7a_race_6_duplicate_and_late_replies ... ok
test session::tests_t7a_race_s7::t7a_race_7_child_exit_mid_turn ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 2032 filtered out;
finished in 31.30s
```

All seven pass (31.3 s on this machine, `--test-threads=1`, 200 iterations
each; the doc's 21.9 s is a different machine). Green does not change any finding
above: F4's unreachable arm, F12's fixed windows and F6's abort-before-check are
all invisible to a passing run.

```
$ grep -rn "Mutex\|RwLock" rust/claude-agent-acp-rs/src rust/claude-agent-acp-rs/tests
src/session_ext.rs:6://! parameters. It lives inside the actor task: no `Mutex`. It is read by the
```

One hit, a doc comment. No `std`/`tokio` `Mutex`/`RwLock` exists in the crate.

```
$ grep -rn "run_registry_actor" rust/claude-agent-acp-rs/src
src/agent.rs:1188:    let mut actor = tokio::spawn(run_registry_actor(...));
src/agent.rs:1420:async fn run_registry_actor(
```

Every incoming request/notification is forwarded into `incoming` from the two
`serve_until` handlers (`agent.rs:1192-1222`) and served only by that actor — the
routing is correct; F2 is about *what* the actor awaits, not a bypass.

---

## 6. Verdict

The program's **decision** (single session-state owner actor, commands over
unbounded channels, atomics for the high-water mark, no mutexes) is real and
implemented, and the seven scenarios exist and are seeded. The **evidence** is
weaker than `CONCURRENCY.md` states: one scenario arm is unreachable (F4), three
"proofs" do not exercise the path they are cited for (F1, F3, F9), one process
assertion is confounded by the test's own abort (F6), the harness is not the
production forwarder (F5), and every §4 line pointer is stale (F8). One real
concurrency property is undocumented and diverges from upstream: request handlers
that await child work stall the entire incoming stream, cancels and permission
answers included (F2).

DONE


