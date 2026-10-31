# G15 — CONC-1 (concurrency decision measured + panic policy) progress

Worktree: `/private/tmp/claude-501/par-b` @ 9e9fe1c (feat/rust-port). Group G15.

## Checklist

- [x] (1) Upstream fairness decision & proof:
  - [x] Analyze upstream `EventLoopYielder` / `messageLoopYielder.maybeYield()` (acp-agent.ts:5650-5662, utils.ts; tests event-loop-yield.test.ts)
  - [x] Prove fairness in tokio with unit/actor test `inv_74_cancel_not_starved_by_backlog` (10k buffered frames, cancel must be handled before backlog drains past N frames)
  - [x] Verify whether starvation exists or if `select! { biased; ... }` guarantees non-starvation (0 frames drained before cancel handled)
- [x] (2) Measured runtime decision (single-thread `current_thread`/LocalSet vs multi-thread):
  - [x] Reproducible benchmark for streaming chunk latency and steering/cancel latency under backlog (`tests/concurrency.rs`)
  - [x] Benchmark measurements for both single-thread and multi-thread recorded in `porting/parity/CONCURRENCY.md`
  - [x] Decision documented in `porting/parity/CONCURRENCY.md` and explicitly configured as `#[tokio::main(flavor = "multi_thread")]` in `bin/claude-agent-acp-rs.rs`
- [x] (3) Panic and exit policy:
  - [x] Release profile panic setting in `rust/Cargo.toml` (`[profile.release] panic = "unwind"` with justification in CONCURRENCY.md)
  - [x] Multi-session containment / panic isolation analysis & test (`inv_panic_isolation_and_prompt_settlement`)
- [x] (4) Shared mutable state audit table skeleton:
  - [x] Audit every shared mutable state in adapter vs Node counterpart in `CONCURRENCY.md`
- [x] (5) FEATURES.md / progress updates:
  - [x] Added note line under Section 8 (Recovery, supervision, teardown) for `V-21 [pkg:CONC-1]`
- [x] (6) Verification:
  - [x] `cargo fmt --manifest-path rust/Cargo.toml --all` (PASS)
  - [x] `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` (PASS)
  - [x] `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib` (555 passed, PASS)
  - [x] New starvation test x10 green (10/10 PASS)
  - [x] Differential test (91 passed, 0 failed, count unchanged, PASS)
  - [x] `bash rust/scripts/rust-gate.sh` ends `==> gate OK` (PASS)
  - [x] JsonPath guard == 0 (PASS)
  - [x] test-map-check PASS (PASS)

---

## Benchmark Summary

- **Multi-thread**:
  - 10k chunks streaming: 2.29 ms (0.229 µs/chunk)
  - Cancel latency under 10k backlog: 124.79 µs (0 frames drained before cancel)
  - Steer latency under 10k backlog: 52.25 µs
- **Current-thread**:
  - 10k chunks streaming: 4.82 ms (0.482 µs/chunk)
  - Cancel latency under 10k backlog: 341.21 µs (0 frames drained before cancel)
  - Steer latency under 10k backlog: 288.54 µs

Multi-thread runtime achieves >2x chunk throughput, 2.7x lower cancel latency, and 5.5x lower steering latency. Both runtimes guarantee 0-frame starvation under Tokio's `select! { biased; ... }`.

## Files Changed
- `porting/parity/CONCURRENCY.md` (new)
- `porting/parity/progress-G15.md` (new)
- `rust/claude-agent-acp-rs/tests/concurrency.rs` (new)
- `rust/Cargo.toml` (`[profile.release] panic = "unwind"`)
- `rust/claude-agent-acp-rs/src/bin/claude-agent-acp-rs.rs` (`#[tokio::main(flavor = "multi_thread")]`)
- `porting/parity/FEATURES.md` (added row V-21 [pkg:CONC-1])

READY:G15
