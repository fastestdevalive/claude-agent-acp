You are reviewing ONE file, read-only. Do not edit any file except your output.
Subject: `rust/claude-agent-acp-rs/src/turn.rs` (+ its tests) — the Rust port of the upstream turn-settlement state machine.
Upstream truth: compiled adapter `~/.bun/install/cache/@agentclientprotocol/claude-agent-acp@0.70.0@@@1/dist/acp-agent.js` — activate/settle/orphan `1338-1682`, consumer loop `1683-1845`, idle settle `2049-2163`, result -> stop reason `2532-2884`; issues #453 `1595-1606`, #825 `2141-2153`, #866 `1553-1560`, orphan order `1453`.
Plan context: `.vibekit/feature-plans/pending/claude-acp-rust/plan-claude-acp-rust.md` Phase 6 + Invariants INV-16, 29, 30, 31 + the Turn state diagram in section Architecture.
Write findings to `.vibekit/feature-plans/pending/claude-acp-rust/review-04-turn.md`.

Answer only: does turn.rs faithfully reproduce upstream behaviour, and would it hold up when driven by a real stream? Check specifically:
1. The `result.subtype` -> StopReason table: compare EVERY case with `acp-agent.js:2532-2884` (subtypes, is_error handling, refusal, max_turns, error_during_execution, etc.). List any missing or different mapping.
2. Echo/activation matching: upstream activates a turn only when the echoed user frame's uuid equals the prompt uuid (`~1517`, `~3018`, `~3092`). Does turn.rs key on uuid?
3. Subagent hold (#866): every settle path routes through settle_or_defer; live-subagent set is fed by task_started / task_notification; drain releases a deferred settle.
4. Orphan coalescing order (1453) and INV-30: test really asserts a dead turn's late result is not consumed by the next turn.
5. #453 (result-text fallback) and #825 (idle without result fails the turn) semantics vs upstream; do the tests assert the real behaviour or a weaker one?
6. Anything upstream does in 1338-1845 / 2049-2163 that turn.rs silently omits, and whether it is needed before phases 7-11.
7. Integration gap: turn.rs is not yet wired into session.rs. List exactly what session.rs / agent.rs must do to drive it (this becomes phase 8 instructions).
Output: table `| # | Severity (blocker/major/minor) | Finding | Evidence (path:line) | Recommended change |`, blockers/majors first, then verdict `go` / `go-with-fixes` / `no-go`, then a short bullet list titled "Phase 8 wiring instructions". Stop when written. Target under 20 minutes.
