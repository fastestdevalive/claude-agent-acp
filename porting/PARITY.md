# Parity ledger — claude-agent-acp-rs vs upstream claude-agent-acp

One row per upstream feature. Status:
- `ported` — implemented and gated
- `skipped-deliberate` — not ported, with a one-line reason (D3)
- `pending` — not yet implemented (only a `pending` row blocks a parity claim)

Seeded from `porting/EVALUATION.md` § 2 (A)/(B). "Parity" means parity of this ported subset (D3);
the crate intentionally does not reproduce upstream's full surface.

## (A) Protocol-necessary core — must port

| Feature | Source (acp-agent.js) | Status | Note |
| --- | --- | --- | --- |
| `initialize` capabilities | `693–746` | pending | |
| `newSession` / `loadSession` + non-optional `createSession` | `747–759`, `788–798`, `4645–4695`, `4696–5209` | pending | |
| `prompt` (enqueues a Turn, owns no loop) | `973–1037` | pending | |
| Turn activate / settle / orphan accounting | `1338–1682` | pending | |
| Consumer loop + abort/EOF race | `1683–1845` | pending | |
| `result` → stop reasons | `2532–2884` | pending | |
| Idle settle via `session_state_changed` | `2049–2163` | pending | |
| Stream → ACP mappers | `6383–6759`, `6760–6864`, `6303–6382` | pending | |
| Prompt content mapping | `6101–6193` | pending | |
| Permission translation | `4069–4275`, `4017–4068`, `447–541` | pending | |
| `cancel` | `3443–3668`, `6865–6892` | pending | |
| fs passthrough | `4002–4016` | pending | |
| Wiring | `542–572`, `6893–6929` | pending | |
| Tool shape mapping | `tools.js:16–357`, `423–717` | pending | |

## (B) Optional — safe to skip

| Feature | Source | Status | Reason |
| --- | --- | --- | --- |
| Config options / modes / fast mode | `3724–3860`, `4388–4644`, `5479–5675` | skipped-deliberate | vibe-station does not surface config/mode options (R9) |
| Model resolution + allowlisting | `5676–6045`, `6930–7114` | skipped-deliberate | resolution is a client concern; not needed by vibe-station |
| Elicitation | `elicitation.js`, `4276–4374`, `2396–2481` | skipped-deliberate | optional extension, not consumed |
| File-change audit (JetBrains/AIR) | `acp-agent.js:734` | skipped-deliberate | JetBrains/AIR extension, not consumed |
| Session-failure ext (JetBrains/AIR) | — | skipped-deliberate | JetBrains/AIR extension; AIR pair = largest skippable chunk |
| Auth / gateway / providers | `595–692`, `850–972`, `5307–5392` | skipped-deliberate | client-side concern, out of scope |
| TODO / plan lists | — | skipped-deliberate | not consumed by vibe-station |
| Settings watching | — | skipped-deliberate | requires reimplementing the SDK merge |
| Subagent transcripts | — | skipped-deliberate | not needed for turn settlement |
| Hooks (PostToolUse/TaskCreated/TaskCompleted) | — | skipped-deliberate | SDK hooks, not consumed |
| Terminal support | — | skipped-deliberate | not needed |
| Goal extension | — | skipped-deliberate | optional extension |
| Custom slash commands | — | skipped-deliberate | not needed |
| MCP server passthrough | — | skipped-deliberate | `mcpServers` ignored (B2) |

## Other deliberate decisions

| Feature | Status | Reason |
| --- | --- | --- |
| Unsupported ACP methods (`authenticate`, `set_mode`, `_session/goal`, …) | skipped-deliberate | respond `-32601` (B1 table) |
| Discarded `SessionUpdate` variants (`SessionInfoUpdate`, `ConfigOptionUpdate`, `UsageUpdate`) | skipped-deliberate | ignored in the differential; vibe-station discards them (`normalize.rs:471-473`) |
| `session/load` history replay via SDK `getSessionMessages` | skipped-deliberate | D15 — resume only via `--resume=<id>`; vibe-station backfills history itself |
| `suppressControlResponse` (SDK-internal JS symbol) | skipped-deliberate | D6 — the port has no handler that declines to answer |
