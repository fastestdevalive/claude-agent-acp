# Real-claude readiness verification — `claude-agent-acp-rs` (`feat/rust-port`)

Drives **both** the Node adapter (`dist/index.js`) and the Rust binary
(`rust/target/release/claude-agent-acp-rs`) against the **same real `claude` CLI**
(on PATH, `CLAUDE_CODE_EXECUTABLE` unset) over stdio, and compares the ACP
responses in shape/semantics (not byte-identity — real model output is
nondeterministic). All file I/O happened under `/tmp/verify-real-claude/`
(throwaway scratch), never in the repo. Nothing was committed or pushed.

Driver: a hand-rolled stdio JSON-RPC client (`/tmp/verify-real-claude/driver.py`)
that sends `initialize` → `session/new` → scenario, records every frame, tracks
the `sessionId`, and auto-replies to `session/request_permission` with
"allow once". Real `claude` was used throughout (no `fake-claude`).

Both binaries were built once (`npm run build`; `cargo build --release`).

---

## Scenario 1 — Plain text turn

**Prompt:** `reply with exactly the word OK and nothing else`

- **Node:** `initialize` → `session/new` → `session/prompt`. Streamed
  `agent_message_chunk` (text "OK"), then prompt result `{stopReason:"end_turn",
  usage:{inputTokens,outputTokens,cachedReadTokens,cachedWriteTokens,totalTokens}}`,
  followed by a `session_info_update`. Clean exit.
- **Rust:** identical flow. Streamed `agent_message_chunk` (text "OK"), prompt
  result `{stopReason:"end_turn", usage:{same five keys}}`. Clean exit 0.
- Both emit the `agent_message_chunk` update with the same structure
  `{sessionUpdate, content:{type:"text",text}, messageId}` (messageId differs, as
  expected). Both settle `end_turn`. No hang, no panic.
- **Verdict: PASS** (both).

---

## Scenario 2 — File I/O via a tool

**Prompt:** `Read the file scratch.txt in this directory using the Read tool and
report its first line exactly.` (scratch file: `This is the first line of the
scratch file.`)

- **Node:** `tool_call` (title "Read File", kind `read`, `rawInput:{}`) →
  `tool_call_update` (rawInput `{file_path}`, title "Read scratch.txt", kind
  `read`, locations `[{path,line:1}]`) → `tool_call_update` (`{file_path,limit:1}`,
  title "Read scratch.txt (1 - 1)") → `tool_call_update` (carries
  `_meta.claudeCode.toolResponse` with the file content) → `tool_call_update`
  (`status:"completed"`, `rawOutput`/`content` with the read text). Then
  `agent_message_chunk`s and result. Both models correctly reported the file's
  first line → the Read actually executed.
- **Rust:** `tool_call` (same "Read File"/kind `read`) → `tool_call_update`
  (`{file_path}`, locations) → `tool_call_update` (`{file_path,limit:1}`,
  "Read … (1 - 1)") — then **directly** to `agent_message_chunk`s and result,
  with **no** `status:"completed"` terminal `tool_call_update`, **no**
  `_meta.claudeCode.toolResponse` update, and **no** `tool_result` update.
  The model still correctly reported the file's first line, so the read executed
  and the result reached the model — but Rust does **not** re-emit the tool
  completion/result to the ACP client.
- **Verdict: PASS with a real finding** — the tool **executed** on both sides and
  the `tool_call`/progress `tool_call_update` pair (title/kind/locations) is
  structurally identical, but **Rust omits the terminal `tool_call_update`
  (`status:"completed"` + `_meta.claudeCode.toolResponse`) that Node emits**.
  A client reading tool results from the update stream sees nothing on Rust.

---

## Scenario 3 — Steering (`_session/steering`)

Both idle and busy paths were exercised (both reliably triggerable with real
claude).

**Idle path** (prompt settled, then steer with `idleBehavior:"promptRequired"`):
- **Node:** `_session/steering` → `{"outcome":"promptRequired","reason":"noRunningTurn"}`
- **Rust:** `_session/steering` → `{"outcome":"promptRequired","reason":"noRunningTurn"}`
- Identical.

**Busy path** (steer while a long prompt is still running):
- **Node:** `_session/steering` → `{"outcome":"injected"}`; the injected steer
  text appeared inside the running turn's stream.
- **Rust:** `_session/steering` → `{"outcome":"injected"}`; the injected steer
  text appeared inside the running turn's stream.
- Identical outcome and injection behavior.

- **Verdict: PASS** (both) — idle and busy both match, including the `"now"`
  priority injection.

---

## Scenario 4 — Cancel mid-turn

**Prompt:** long essay (kept running), `session/cancel` sent ~10s in.

- **Node:** `session/cancel` → prompt result `{stopReason:"cancelled",
  usage:{inputTokens:0,outputTokens:0,cachedReadTokens:0,cachedWriteTokens:0,totalTokens:0}}`.
  No hang.
- **Rust:** `session/cancel` → prompt result `{stopReason:"cancelled"}` (no
  `usage` field). No hang.
- **Orphan check:** the exact `claude` child PID spawned by each adapter was
  recorded; immediately after teardown it read "alive", but a follow-up confirmed
  **both child PIDs are gone** — the transient "alive" was a reap-timing artifact
  (kill lands slightly after stdin close). Neither side left a persistent orphan.
- **Verdict: PASS with a minor finding** — both cancel cleanly and leave no
  orphan, but on a cancelled result **Node includes an all-zero `usage` object
  while Rust returns only `{stopReason:"cancelled"}`** (usage field omitted).

---

## Scenario 5 — Permission prompt

**Prompt:** `Use the Bash tool to run this exact shell command and nothing else:
rm /tmp/verify-real-claude/scen5/delete-me.txt …` (delete a throwaway file).

- **Node:** `tool_call` (kind `execute`) → `session/request_permission` (id `0`),
  `toolCall` = `{toolCallId, rawInput:{command,description}, title:"rm …", kind:
  "execute", content:[…]}`, `options=[(reject,reject_once),(allow,allow_once),
  (allow_always,allow_always)]`. On "allow once" → tool executed, result
  `stopReason:"end_turn"`.
- **Rust:** identical — `session/request_permission` (id a UUID) with the same
  `toolCall` shape and the same three options `[reject_once, allow_once,
  allow_always]`. On "allow once" → tool executed, result `stopReason:"end_turn"`.
- The file was actually deleted on both (verified: `delete-me.txt` gone).
- **Verdict: PASS** (both) — permission ask shape and options are identical; the
  only difference is the JSON-RPC request id (Node sequential int `0`, Rust
  UUID). Both are valid ids; not a semantic divergence.

---

## Summary

| # | Scenario | Node | Rust | Verdict |
|---|----------|------|------|---------|
| 1 | Plain text turn | `end_turn`, AgentMessageChunk | `end_turn`, AgentMessageChunk | PASS |
| 2 | File I/O via tool | tool_call + progress + terminal `completed`/toolResponse updates | tool_call + progress, **missing terminal completed/result update** | PASS (finding) |
| 3 | Steering (idle + busy) | `promptRequired` / `injected` (injected delivered) | identical | PASS |
| 4 | Cancel mid-turn | `stopReason:cancelled` + all-zero usage; no orphan | `stopReason:cancelled` (no usage); no orphan | PASS (finding) |
| 5 | Permission prompt | request_permission + allow-once proceeds | identical | PASS |

---

## Findings

1. **`available_commands_update` advertises commands on Node, empty on Rust
   (real, 100% reproducible).** Across **every** scenario Node emitted
   `available_commands_update` with the real command set (`59` after
   `session/new`, `57` after prompt) while Rust emitted a single update with
   `availableCommands: []`. Node enumerates the `claude` CLI's available
   commands/skills and advertises them; Rust does not. A client using
   `available_commands_update` sees a real gap.

2. **Rust omits the terminal tool-call completion/result update (real, Scenario 2).**
   Node emits a final `tool_call_update` with `status:"completed"` and
   `_meta.claudeCode.toolResponse` (carrying the tool's output). Rust stops at the
   progress `tool_call_update`s and never emits the completion/result update (and
   never emits a `tool_result` update). The underlying tool still executes and the
   result reaches the model, but the ACP client gets no tool-result update from Rust.

3. **Rust never emits `usage_update` or `session_info_update` (real, all scenarios).**
   Node streams `usage_update` (context usage `used`/`size`, optional `cost`) and
   `session_info_update` (session title/`updatedAt`); Rust emits neither variant.
   The SessionUpdate variant-kind set differs: Node
   `{agent_message_chunk, agent_thought_chunk, available_commands_update,
   session_info_update, tool_call, tool_call_update, usage_update}` vs Rust
   `{agent_message_chunk, agent_thought_chunk, available_commands_update,
   tool_call, tool_call_update}`.

4. **Cancelled result omits `usage` on Rust (minor).** Node returns
   `{stopReason:"cancelled", usage:{…all zeros}}`; Rust returns `{stopReason:"cancelled"}`.

5. **Permission request id differs (benign).** Node uses a sequential integer
   (`id: 0`), Rust a UUID. Both are valid JSON-RPC request ids; not a semantic
   divergence.

---

## Overall verdict

**ready-with-caveats.**

The Rust binary is a faithful, drop-in-equivalent ACP agent for the interactive
turn lifecycle: plain turns, tool calls (they execute and the model uses the
result), steering (both idle `promptRequired` and busy `injected`, with the
`"now"` injection actually delivered into the running turn), cancel (clean
`stopReason:cancelled`, no orphaned `claude`), and permission prompts (identical
request shape and options, allow-once proceeds) all match Node in shape and
semantics. However, it is not fully parity-clean on the **update-notification**
surface: it never emits `usage_update` or `session_info_update`, it emits an
empty `available_commands_update` where Node advertises the real command set, and
it omits the terminal `status:"completed"`/toolResponse update that Node uses to
deliver tool results to the client. None of these break a basic turn, but any
consumer that tracks cost/context usage, session titles, available commands, or
tool results from the update stream will behave differently against the Rust
port. These are parity gaps to close before treating the port as truly
equivalent to the Node adapter.

---

## Real usage incurred (approximate)

**12 real `claude` prompt turns** (5 scenarios × 2 sides, steering run twice per
side for idle+busy), each preceded by a `session/new` that spawns/initializes a
real `claude` (minor additional token cost for initialization). No other real API
usage. Recordings and the driver live under `/tmp/verify-real-claude/` (outside
the repo; not committed).
