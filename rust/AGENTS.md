# rust/ — Rust port rules

## Scope

- This file **overrides** the root `AGENTS.md` / `CLAUDE.md` for everything under `rust/**`.
- The root rules are upstream TypeScript rules — they do **not** apply here. **Never run `npm`** (no `npm run check`, no `npm run build`, no conventional-commit PR-title rules).
- Only Rust work lives here: a `rust/` Cargo workspace and its `porting/` sibling ledger.

## Protocol

Every implementer phase follows, in order:

1. Read this file first.
2. Load `skills/rust-coding/SKILL.md`, plus the `coding-agent-guardrails` and `coding` skills.
3. Write the `N.T*` tests first (named exactly as the phase block names them), before or alongside the implementation.
4. Run `bash rust/scripts/rust-gate.sh` (from the fork root) and paste the **real output** in the final report — a hang or a skipped check is a failure.
5. If you must deviate from the plan, add a short bullet under the relevant Decision in the plan's `## Key Decisions`.
6. **Never** touch `src/`, `package.json`, or any other upstream file (guard G7). Never push.
7. Name invariant tests `inv_NN_<slug>` (e.g. `inv_24_order_sensitive`).

## Skill mapping

- `rust-coding` §1–5, 7, 9, 10 apply.
- §6 wire truth = the Node adapter's ACP frames at `v0.70.0` (the compiled `dist/acp-agent.js`), **not** `daemon/src/types.ts`.
- §8 (rusqlite) is not applicable.

## Rules

- Session state lives in a **single session-state owner actor** — no `Arc<Mutex<Session>>` (Decision D4, guard G5).
- Only `process.rs` may use `cfg(unix)` / `cfg(windows)` (guard G9); `#![forbid(unsafe_code)]` everywhere (D12, `rust-coding` §1).
- Every test runs under an explicit `timeout` (guard G3, `rust-coding` §9) — a hang is a failure, same severity as a panic.
- Dependency pin: `agent-client-protocol` held at `2.1.0` with schema `1.7.0` (guard G10, Decision D9).
