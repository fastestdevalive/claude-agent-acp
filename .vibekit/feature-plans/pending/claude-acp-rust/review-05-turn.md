# Review 05 — `turn.rs` re-review (after review-04 rework)

Scope: blockers #1–#4 and majors #6 and #9 from review-04, plus any new major regressions. `ADP` = `claude-agent-acp@0.70.0 dist/acp-agent.js`.

**Checked and fixed:**
- **#1 (idle drain):** fixed. `settle_deferred_if_drained` now takes `&mut Vec`, and `held_idle_drain_emits_settled` would fail if the drain were removed.
- **#3 (owed trailing idles):** the idle order matches `ADP:2081-2118`. The result, echo hand-off and autonomous increments match. `lagging_idle_after_next_echo_absorbed` would fail without the debt.
- **#4 (autonomous results):** the lane matches `ADP:2689-2717`. Both autonomous tests would fail if the lane were deleted.
- **#6 (stop-reason grid):** matches `ADP:2762-2842` case by case, and the grid test covers it.
- **#9 (cancel):** the sweep, orphan credits, inline settle of a held turn and the queue filter match `ADP:3453-3584` (non-lifecycle lane).

**Still wrong:**
- **#2 (`ensure_active_turn`):** only half fixed (row 1 below).
- **Held-cancel debt rule (`ADP:3571`):** cannot fire in practice (row 2).
- **Panics:** none found. Every removal is guarded, and there is no `unwrap` outside tests.

| # | Severity | Finding | Evidence | Change |
|---|---|---|---|---|
| 1 | blocker (new regression, #2 incomplete) | **`ensure_active_turn` is missing upstream's early return, `if activeTurn && !isHeldOpen(activeTurn) return`.** It runs on *every* user-turn result, including a normal result for an active turn that was echoed. It then (a) takes the orphan credit that belongs to a swept queued turn, or (b) calls `activate(queue head)`, which overwrites `self.active` **without settling it**. **Timeline 1:** A is echoed and active; B is already pushed and queued (the phase-8 wiring pushes immediately); A's result arrives. B is activated, A is dropped, and A's result settles **B**. A's oneshot never resolves (a hang), and B is misreported. **Timeline 2 (INV-30 broken):** A is active; B is queued; cancel sweeps B and adds orphan=1; A's own result arrives before idle. The Rust code uses up B's credit on A's result, so B's late result then promotes or settles the next echo-less prompt. No test covers "result for an active turn while another turn is queued", so every test passes. There is also no test for step (a): a held turn followed by an echo-less result. | `turn.rs:735-754` vs `ADP:1408-1412` (`if (session.activeTurn) { if (!isHeldOpen(...)) return; settleActive(deferred) }`). Tests: `inv_30_orphan_result_not_reused` only sends p2's result *after* the idle settled p1. `cancel_sweeps_...` has no active turn at result time. | Restructure the function: `if let Some(a) = &self.active { if !is_held_turn(a) { return; } settle(deferred) }`, then orphan, then head. Add tests: (i) A active + B queued → A's result settles **A**, B stays queued, and B's echo activates B. (ii) A active + B queued → cancel → A's result → idle → p3 (echo-less) enqueued → B's late result must not activate p3. (iii) A held + `/context` queued → its result settles A with the deferred outcome, then promotes and settles `/context`. Each test must fail against the current code. |
| 2 | major | **The `last_session_state` debt rule for a held-turn cancel is dead.** The field is only ever written to `"idle"` (in `on_idle`), and no API records non-idle states. After the first idle it stays `"idle"` forever, so `cancel()` never owes the interrupt's trailer when a held turn is cancelled mid-followup (state `running`). That un-owed idle then hits the next echoed prompt and falls into the #825 path, which fails it: the false-fail that `ADP:3571` exists to prevent. No test drives this path. | `turn.rs:261`, `turn.rs:318`, `turn.rs:627`; `ADP:2050` (`session.lastSessionState = message.state` for **every** state), `ADP:3571` | Add `on_session_state(state: &str)` and route every `session_state_changed` frame to it, calling `on_idle` only for `idle`. Or make `on_idle` part of a general state setter. Test: held turn → state `running` → cancel → B enqueued + echoed → idle → B must **not** fail. |

Minor points, not blocking:
- `on_idle` clears `cancelled` after the cancelled settle; upstream leaves it set until the next `activateTurn`. As a result, an orphan's late result after that idle leaves the cancelled guard and can emit a stray `FinalText` with no active turn. Fix: drop `self.cancelled = false` in `on_idle`, or also gate `FinalText` on `self.active.is_some()`.
- A held turn's cancel reports `accumulated_usage`, where upstream reports `deferredSettle.usage`. The two are the same today.

## Verdict

**no-go.** Row 1 is a hang or misattribution on the most common real-stream timeline once prompts are pushed immediately, and it breaks INV-30 again. It is a small fix (restore the early return), but it must land together with the three tests above. Row 2 should land in the same pass.
