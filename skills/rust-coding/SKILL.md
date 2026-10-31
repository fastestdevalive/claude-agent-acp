---
name: rust-coding
description: Rust coding standards and safety rules for the vibe-station daemon/CLI Rust port — extends `coding` + `coding-agent-guardrails`. Load before writing or reviewing any .rs file under rust/.
version: 0.1.0
triggers:
  - "rust coding"
  - "/rust-coding"
globs:
  - "rust/**/*.rs"
  - "rust/**/Cargo.toml"
---

# Rust coding skill

Rust-specific rules for the `rust/` Cargo workspace (`daemon-rust-port`): `rust/vst-daemon`, `rust/vst-cli`, and every `rust/vst-*` library crate, all flat siblings under one workspace root at `rust/Cargo.toml`. `desktop/src-tauri/` is a separate, standalone Cargo project outside this workspace (not a sibling under `rust/`, so no exclude is even needed — see the arch doc's Target Structure). Applies on top of, never instead of, the `coding` and `coding-agent-guardrails` skills — read those first if not already loaded.

Sources consulted: [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html), tokio's own blocking-work guidance ([`spawn_blocking` docs](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html), [tokio-rs/tokio#8470](https://github.com/tokio-rs/tokio/issues/8470)), `clippy` pedantic/nursery lint groups.

## Layering

```
coding-agent-guardrails (base)
    │
    ├── coding (universal runtime rules)
    │   │
    │   └── rust-coding (this skill)
    │       ├── memory/concurrency safety
    │       ├── error handling (thiserror/anyhow split)
    │       ├── async/blocking hygiene
    │       ├── API shape (rust-lang API guidelines subset)
    │       ├── lint/tooling gate
    │       └── FFI/wire-compat rules (this port's contract requirement)
```

## 1. Safety — no exceptions without a written reason

- `#![forbid(unsafe_code)]` at the crate root for every crate under `rust/` (both binary crates and every `vst-*` library) **except** where a crate has a specific, documented need (e.g. a PTY/FFI boundary) — that crate gets `#![deny(unsafe_code)]` instead and every `unsafe` block carries a `// SAFETY:` comment stating the invariant that makes it sound, not just what it does.
- Never use `unwrap()`/`expect()` on anything derived from network input, file I/O, or subprocess output. `expect()` is allowed only on invariants the type system already guarantees (e.g. `Mutex::lock()` on a non-poisoned mutex in single-threaded test code) — and even then prefer `unwrap_or_else`/`?` in library code; `expect` with a message is acceptable in `main.rs`/tests.
- No `std::mem::transmute`. No raw pointer arithmetic outside a crate that has opted into `unsafe_code` for a stated FFI reason.
- Panics are for programmer errors (broken invariants), never for expected failure modes (bad input, missing file, closed socket) — those return `Result`.

## 2. Error handling — thiserror in libraries, anyhow at the edges

- Every `rust/vst-*` **library** crate defines its own error enum with `thiserror::Error` — callers match on variants; never a bare `String` or `Box<dyn Error>` as a library's public error type.
- The two **binary** crates (`vst-daemon`, `vst-cli`) may use `anyhow::Result` in `main`/top-level command dispatch, where the point is reporting to a human/log, not programmatic matching.
- Never swallow an error silently (`let _ = fallible_call()`) — if an error is genuinely ignorable, write `.ok();` with a comment saying why, so it reads as a decision, not an omission.
- Preserve error context across `?` — use `.context("...")` (anyhow) or a `#[from]`/`#[source]` variant (thiserror), never a bare `?` that discards which layer failed when it crosses a boundary in `Design Details → System Boundaries` (see the arch doc).

## 3. Concurrency — mirror the daemon's existing invariants, don't relax them

> This port is translating code with specific, previously-debugged concurrency invariants (see `AGENTS.md` in the repo root and the arch doc's Gotchas table). The Rust type system can *enforce* these, so it must — not merely happen to preserve them.

- **Keyed locks stay keyed.** A lock that was per-`(connection, sessionId)` in the TS code (`withSessionLock`) must be a per-key lock in Rust (e.g. a sharded map of `Arc<tokio::sync::Mutex<()>>`, or an actor-per-key model) — never collapsed into one global `Mutex`/`RwLock` "to simplify," even when a reviewer or the implementing model is under time pressure. A coarser lock is a silent behavior change, not a refactor.
- **Single-writer-per-field invariants are enforced by module visibility, not by comment.** If only one poller/service may write a field (see the daemon's two-axis lifecycle/PR status model), that field's setter is `pub(crate)`/`pub(super)` scoped to the owning module; every other module gets a read-only accessor. If you find yourself needing a second `pub` setter to "make it compile," stop — that's the bug this rule exists to catch, not a paperwork obstacle.
- **`Send`/`Sync` bounds are signal, not noise.** If the compiler won't let a type cross a `tokio::spawn` boundary, that is usually telling you the sharing model is wrong (needs an `Arc`, a channel, or restructuring — not a `Rc<RefCell<_>>` smuggled through `unsafe impl Send`). `unsafe impl Send`/`Sync` is banned outright in this workspace; if you believe you need it, stop and escalate instead of writing it.
- Prefer message-passing (`tokio::sync::mpsc`/`oneshot`) over shared-mutable-state where the existing TS code's shape allows it — it's usually an easier, more literal translation of "one thing owns this, others ask it" than trying to recreate JS's single-threaded-event-loop illusion with locks.

## 4. Async/blocking hygiene

- **Never call a synchronous, potentially-slow function directly inside an `async fn` body that runs on a tokio worker** — this includes `rusqlite` calls, synchronous file I/O on large files, and any CPU-bound loop. Wrap it in `tokio::task::spawn_blocking`, or route it through a dedicated worker thread (preferred for `rusqlite` specifically, given SQLite's single-writer nature — see the arch doc's Gotcha #4).
- Do not spawn one `spawn_blocking` per tiny SQLite call in a hot path — batch related blocking work into one `spawn_blocking` call (per tokio's own guidance) or, better, funnel all writes through one dedicated writer task/thread reached via an `mpsc` request channel.
- `.await` inside a held `std::sync::Mutex` guard is forbidden — it will deadlock the executor under contention. Use `tokio::sync::Mutex` if the critical section must hold across an `.await`; otherwise drop the guard before awaiting.
- No busy-polling loops (`loop { if cond { break } }` with no yield/sleep) — use a channel, a `Notify`, or a real wait primitive.

## 5. API shape (Rust API Guidelines subset that matters here)

- **Naming:** conversions follow `as_`/`to_`/`into_` conventions (cheap borrow / expensive copy / consuming, respectively) — don't invent ad-hoc method names for conversions that have a conventional prefix.
- **Type safety over primitives:** don't pass `bool`/`String`/`u64` where a newtype or enum states the meaning — e.g. a session id is `SessionId(String)` or similar, not a bare `String`, so it can't be swapped with a worktree id at a call site by mistake. This directly matters for this port: the wire protocol has many string-typed ids (`connectionId`, `sessionId`, `worktreeId`) that were easy to transpose in the original TS too — don't reintroduce that footgun in a language that can prevent it.
- **Every wire-facing id newtype is `#[serde(transparent)]`.** A newtype without it serializes as `{"0": "..."}` instead of the bare string the wire-compat requirement (arch doc F1, and this skill's §6) demands — this is the specific, easy-to-miss interaction between §5's newtype rule and §6's byte-identical-JSON rule.
- **Struct fields private by default**, accessed via constructors/methods — keeps future-proofing options open per the API guidelines' "sealed" pattern, and matches this workspace's module-visibility rule in §3.
- **`Debug` on every public type**, with meaningful (non-empty) output — required for this port specifically because debugging a cheap-model-generated diff without working `Debug` output is much harder than it needs to be.
- Validate function arguments at the boundary and return `Result`/`Option` rather than documenting "caller must not pass X" — dependability over trust.

## 6. Wire-compatibility (specific to this port, not a general Rust rule)

- Every `serde`-derived struct in `vst-types` that represents a WS/REST payload must produce **byte-identical JSON field names/shapes** to the existing `daemon/src/types.ts`/`protocol.ts` definitions it replaces — this is a hard product requirement (F1 in the arch doc), not a style preference.
- Use `#[serde(rename_all = "camelCase")]` (or per-field `#[serde(rename = "...")]`) to match the TS wire format exactly — Rust idiomatic `snake_case` field names stay in the struct definition, the wire format doesn't change.
- Any divergence discovered between the TS shape and what feels "more idiomatic" in Rust is not your call to make silently — flag it in the part's plan/PR, don't just ship the "nicer" shape.

## 7. Lint/tooling gate — every part's Verify step, no exceptions

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D clippy::correctness -D clippy::suspicious -D clippy::complexity -D clippy::perf -D warnings
cargo test -p <crate>
```

- **Do not add a bare `-D warnings` on top of an enabled `pedantic` group** — `-D warnings` promotes *every* warn-level lint to a hard error, pedantic included, and the first real session will either be unable to finish or will reach for a blanket `#![allow(clippy::pedantic)]` that defeats the point. The command above avoids this: `[workspace.lints.clippy] pedantic = "warn"` lives in `rust/Cargo.toml` (informational, never blocks) and the gate denies only the four groups worth hard-blocking on, explicitly, plus plain `rustc` warnings via the trailing `-D warnings` (which does not touch clippy's own warn-level lints once they're governed by `[workspace.lints.clippy]` instead of the CLI flag). One canonical invocation lives in `rust/scripts/rust-gate.sh` — every part and every integrator run uses that script, never a hand-typed variant.
- `cargo deny check` (licenses + advisories) once the workspace has real third-party dependencies (part `00` should add a `deny.toml` with the workspace's license policy) — catches an incompatible license or a yanked/vulnerable crate before it's load-bearing.
- A part is not "done" (per the arch doc's phase recipe step 5) until all three gate commands are clean — a cheap model reporting "tests pass" without having actually run `clippy`/`fmt` is a common failure mode; the reviewing agent re-runs them, never trusts the subagent's say-so.

## 8. `rusqlite` query-result lifetime idiom — fix the pattern once, not per occurrence

> Learned live during the port (part `01-storage`, `transcript.rs`): this exact error recurred across 8+ methods in one file, each fixed independently by rebuilding after every single fix. That's the single most expensive failure mode observed so far — the fix is the same in every case, apply it everywhere the first time you see it.

The error `` `stmt` does not live long enough `` / `` temporary value dropped while borrowed `` on a `Statement::query_map(...).collect()` chain is not N different bugs — it's one idiom violated N times. It happens whenever a `MappedRows` iterator (which borrows the `Statement`) is the tail expression of a block that also owns the `Statement`:

```rust
// WRONG — MappedRows borrows `stmt`; `stmt` drops at block end while the
// collect() temporary is still alive. This shape fails in EVERY method that
// has it, not just the one the compiler happens to report first.
fn read_all(&self) -> rusqlite::Result<Vec<Row>> {
    let mut stmt = self.conn.prepare("SELECT ...")?;
    stmt.query_map([], |r| Row::try_from(r))?
        .collect::<rusqlite::Result<Vec<_>>>()
}
```

```rust
// RIGHT — bind the collected (owned) result to a local BEFORE returning it,
// so the borrow of `stmt` ends before `stmt` itself is dropped.
fn read_all(&self) -> rusqlite::Result<Vec<Row>> {
    let mut stmt = self.conn.prepare("SELECT ...")?;
    let rows = stmt
        .query_map([], |r| Row::try_from(r))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
```

- **The moment this error appears once in a file, grep that same file for every other `.prepare(...)` + `.query_map(...)` chain and apply the same fix to all of them in one pass** — do not wait for the compiler to report them one at a time across separate rebuilds.
- Same fix applies inside a closure that returns `Result<Vec<_>>` from a `stmt.query_map(...)?.collect()` tail expression — bind to a local inside the closure before the closure's final `Ok(...)`.

## 9. Bounded retry/poll loops — a hang is a failure, same severity as a panic

> Learned live during the port (part `01-storage`, `migration.rs`): two tests hung indefinitely (still running past 60s, never completing) rather than failing fast — worse than a compile error, because nothing tells you it's stuck without actively watching it run.

- Any loop ported from TS code that originally relied on an external async event (a filesystem watcher, a chokidar callback, an `await`ed retry) must get an explicit bound in Rust: a max-attempt count, or a `tokio::time::timeout(...)`. Never translate a "retry until it succeeds" TS pattern into an unconditional `loop {}` / `while !done {}` in Rust without also porting *why* it was expected to terminate (what advances the condition, and what happens if it doesn't).
- A test that can hang is not just "slow" — treat it exactly like a failing test for gate purposes (Phase recipe step 5). If a test's own name implies a retry ("retried_once_fixed", "not_reprocessed") verify by inspection that the condition it's waiting on is actually satisfied by the test's fixture, not just "eventually true in production."
- When running tests yourself (as the implementer, in step 5, or as the integrator in step 7), always run with an explicit outer timeout (`timeout 120 cargo test ...`) — never trust a bare `cargo test` invocation to fail fast on a hang.

## 10. Self-deadlock via re-entrant lock acquisition — the actual cause of `01-storage`'s hang (§9 was a wrong guess)

> Correction, not an addition: §9's "unbounded retry loop" hypothesis was the working theory when this crate's tests were observed hanging, but it was **wrong**. The real cause, confirmed after the fact: a test held the `MutexGuard<Connection>` returned by a `raw_conn()`-style accessor across a second call that itself tried to lock the *same* `std::sync::Mutex` internally (e.g. a second `migrate_manifests`-style call routed through `spawn_blocking`) — the guard was never dropped before the re-entrant lock attempt, so the second lock acquisition waited forever on a lock its own caller was still holding. §9's guidance about bounding retry loops is still correct and worth keeping, but treat it as one of two known hang causes, not the only one.

- **Never hold a lock guard across a call to a method that might lock the same mutex again.** If a type exposes both a raw/guarded accessor (`fn raw_conn(&self) -> MutexGuard<Connection>`) and higher-level methods that lock internally, using both in the same scope is a live deadlock risk — scope the guard tightly and drop it (an explicit `drop(guard)`, or a nested block) before calling anything that isn't guaranteed to avoid the same mutex.
- **Prefer not exposing a raw guarded accessor at all** where a narrower, single-purpose method would do — the deadlock surface only exists because a caller *can* hold the guard across another call; if the type's public API never hands out a guard, this class of bug can't occur.
- When a test (or any code) appears to hang rather than fail, and §9's bounded-retry angle doesn't pan out on inspection, check for exactly this shape next: a guard held live while a call reachable from that same scope also needs the same lock.

## What NOT to do (common cheap-model failure modes on this specific port)

- Do not "fix" a borrow-checker error by sprinkling `.clone()` until it compiles — check first whether the clone reintroduces a stale-read race the original lock was preventing (see §3).
- Do not collapse two independently-owned fields into one struct with a single lock "for simplicity" if the TS code kept them independently owned for a stated reason (the two-axis status model is the canonical example — see the arch doc's Gotcha #3).
- Do not invent a new wire shape because it's more idiomatic Rust — see §6.
- Do not reach for `unsafe` to work around a lifetime/ownership fight — restructure the ownership instead; if truly stuck, escalate rather than reaching for `unsafe impl Send`/raw pointers.
- Do not fix the same compiler error shape one occurrence at a time, rebuilding between each — if it's the second time you've seen the identical error message in one file, grep for every other instance of the pattern and fix them all in this pass (see §8 for the specific `rusqlite` case this bit the port on).
- Do not let a test run past a minute without an explicit timeout around it — a hang is a bug, not "still working" (see §9).
