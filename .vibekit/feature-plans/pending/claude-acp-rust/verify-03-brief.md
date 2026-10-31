You are the implementer for a **real-claude readiness verification** (not a plan phase — an ad hoc check) of the crate on this branch (`feat/rust-port`, tag `rust-v0.1.0+acp.0.70.0` plus later commits). Repo: this worktree; `C` = `rust/claude-agent-acp-rs`.

## Goal
Drive BOTH the Node adapter (`dist/index.js`) and the Rust binary (`C/src/bin/claude-agent-acp-rs.rs`) against the SAME **real** `claude` CLI (available on PATH: `claude`, do not use `fake-claude` for this task) for a handful of real turns, and assert the Rust side's ACP responses are equivalent in *shape and semantics* to what Node produces — not byte-identical (real model output is nondeterministic: text, ids, timing all vary run to run).

## Rules
- Read `rust/AGENTS.md`; load skills `coding-agent-guardrails`, `coding`; read `skills/rust-coding/SKILL.md`. Root AGENTS.md/CLAUDE.md are upstream TS rules and do not apply.
- **This uses a REAL Anthropic account/credentials via the local `claude` CLI** — it will make real API calls and cost real usage. Keep the scenario count small (the list below, no more). Use a SCRATCH temp directory for any file I/O scenario — never read/write files inside this git worktree.
- **Never commit anything from this task, and never push.** This is a report-only exercise. If you produce recordings, write them under `/tmp/verify-real-claude/` (outside the repo) or a git-ignored scratch dir, never under `porting/fixtures/` or `porting/corpus/` (those are the separate, deliberate `record-real.sh` corpus — do not touch them or conflate this with that).
- **Never print environment variable values, API keys, or tokens** in your report or anywhere. If the `claude` CLI needs credentials, assume they're already configured in this environment; do not ask for or print them.
- Do NOT modify `src/`, `package.json`, or any Rust source file — this is a verification task, not an implementation task. If you find a REAL bug, describe it precisely in the report (do not fix it).
- If `claude` errors immediately (auth/network/rate-limit), or the check can't run for any reason, say so plainly in the report as `BLOCKED — <reason>` for that scenario and move on to the next; do not fabricate results. Write `BLOCKED.md` in the repo root only if EVERY scenario is blocked.
- Every command under `timeout` (real model turns can take 30-90s; use a generous timeout like 120s per turn, not per scenario overall).

## Setup
1. Build both sides once: `npm ci && npm run build` (Node, if `dist/` absent) and `cargo build --manifest-path rust/Cargo.toml --workspace --bins --release` (Rust; release is faster to run repeatedly).
2. You will drive each side as an ACP agent over stdio using `rust/target/release/acp-recorder` (or hand-rolled stdio JSON-RPC if simpler) pointed at, respectively, `node dist/index.js` and `rust/target/release/claude-agent-acp-rs`, with `CLAUDE_CODE_EXECUTABLE` **unset** (or pointed at the real `claude` on PATH) so both talk to the real CLI.
3. Use a fresh scratch working directory (`mktemp -d`) as the ACP session's `cwd` for every scenario, with 2-3 small throwaway text files you create for the file-I/O scenario. Never point `cwd` at this repo.

## Scenarios (run against BOTH Node and Rust, real claude, small/cheap prompts)
1. **Plain text turn** — a simple question with a short expected answer (e.g. "reply with exactly the word OK and nothing else"). Compare: both settle with `stopReason: end_turn` (or equivalent), both emit `AgentMessageChunk` updates, no hang, no panic/crash.
2. **File I/O via a tool** — ask it to read a small file you created in the scratch dir and report its contents (e.g. "read scratch.txt and tell me its first line"), or write a small file. Compare: both emit a `ToolCall`/`ToolCallUpdate` pair (Read or Write/Edit) with a sane `title`/`kind`/`locations`, both actually perform the file op (verify with a plain `cat`/`ls` after), both settle normally.
3. **Steering (`_session/steering`)** — start a turn, then while it's running (or once idle, per the plan's two idleBehavior paths) send a `_session/steering` request with `_meta.steering.idleBehavior` behavior matching the plan's B1 row; compare the outcome shape (`promptRequired`/injection ack) between Node and Rust. If you can't get real-claude timing right to hit "busy" vs "idle" reliably, test whichever path you can reliably trigger and say which one.
4. **Cancel mid-turn** — send `session/cancel` shortly after a `session/prompt` that's likely to run for a few seconds (e.g. ask it to write a longer response). Compare: both settle with a cancelled-shaped `stopReason`, no hang, `claude` process for that turn is not left orphaned (`pgrep -f claude` after, or check the recorded pid is gone).
5. **Permission prompt** (if reachable without `--dangerously-skip-permissions`/`bypassPermissions` — check current default mode) — a command/tool call that would normally need a permission ask; compare the `session/request_permission` shape between Node and Rust, and that "allow once" lets it proceed on both. If your `claude`/environment defaults to auto-allow and you can't trigger a real prompt, say so and skip with a note instead of guessing.

## What "equivalent" means for the comparison (write this precisely per scenario)
- Same *set of SessionUpdate variant kinds* used for the same kind of event (a tool call still looks like a tool call on both sides).
- Same ACP-level outcome (`stopReason`, error vs success, permission asked vs not) for a semantically equivalent prompt/response.
- NOT the same text content, ids, or timings — those are expected to differ with a real model.
- Any place they diverge in KIND (e.g. Node asks for permission, Rust doesn't; or Rust never emits the ToolCall at all) is a real finding — call it out with the exact frames from both sides as evidence.

## Report
Write to `.vibekit/feature-plans/pending/claude-acp-rust/verify-03-real-claude.md`:
- One `## Scenario N` section per scenario above: what you sent, what Node did (summarized structurally, not the raw model text), what Rust did (same), PASS / FAIL / BLOCKED, and evidence (the specific frame types/fields that matched or diverged — trim any secrets/long text).
- A summary table `| # | Scenario | Node | Rust | Verdict |`.
- Overall verdict: `ready` / `ready-with-caveats` / `not-ready`, one paragraph justifying it.
- List anything costed real API usage roughly (rough count of real turns run), so the human knows the cost incurred.
Stop when the report is written. Do not commit, do not push.
