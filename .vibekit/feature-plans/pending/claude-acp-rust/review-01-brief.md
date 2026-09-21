You are reviewing a plan. **Read-only: do not edit the plan or any other file except your review output.**
Plan: `.vibekit/feature-plans/pending/claude-acp-rust/plan-claude-acp-rust.md`. Evidence: `porting/EVALUATION.md`.
Write findings to `.vibekit/feature-plans/pending/claude-acp-rust/review-01-plan.md`.

Review for:
1. **Format compliance** — against `~/.claude/skills/planning/FORMAT.md` + `SECTIONS.md`, including its "Checklist before committing a plan" and the self-containment bar ("could haiku implement this cold?").
2. **Citation accuracy** — spot-check **at least 10** citations against the real sources:
   - upstream TS at `v0.70.0`: `src/` in this worktree
   - compiled adapter (the `acp-agent.js:N` refs): `~/.bun/install/cache/@agentclientprotocol/claude-agent-acp@0.70.0@@@1/dist/`
   - SDK bundle (the `sdk.mjs:L:C` refs, L = line, C = char offset): `~/.bun/install/cache/@anthropic-ai/claude-agent-sdk@0.3.232@@@1/sdk.mjs`
   - ACP Rust crate: `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/agent-client-protocol-2.1.0/`
   - vibe-station (`vst-*` paths): `~/code/fastestdevalive/vibe-station/rust/`
3. **Guards & invariants** — is every `INV-*` testable as written? Is any major invariant missing — check especially the turn-settlement hazards in `EVALUATION.md` and research rows R11–R16 (lessons from the vibe-station daemon port)? Can the orchestrator actually do G1–G9 mechanically?
4. **Phase sizing** — each phase is done by ONE fresh `deepseek` implementer turn with only that phase's checklist. Is any phase too big or under-specified for that? Recommend splits.
5. **Technical claims** — D9 (`Channel::duplex()` + `connect_with`; caret `"2.1"` unifying with vibe-station's `=2.1.0`), D12 (the Windows/macOS lifecycle table and cross-target `cargo check` guard), D4/D6 concurrency design.
6. **Consistency** — stale references, contradictions between sections, numbering errors.

Output format: a table `| # | Severity (blocker/major/minor) | Section | Finding | Evidence (path:line) | Recommended change | Resolution |`, then a verdict line: `approve` / `approve-with-changes` / `rework`. Leave the `Resolution` column empty for the author. Stop when the file is written.
