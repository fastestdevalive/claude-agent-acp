---
title: "rust-parity-tests: Node vs Rust differential corpus additions"
feature: rust-parity-tests
worktree: /home/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1
---

<!--
RULES — read before writing or implementing:
1. FORMAT: Bullets, tables, code, diagrams ONLY — no prose paragraphs
2. REQUIREMENTS: One crisp line each — no verbose descriptions
3. CHECKLIST: Mark items [x] as you complete them — this is your persistent todo list
4. READING TIME: Optimize for fast human scanning — if it's hard to skim, rewrite it
-->

## Goal

Add 2 new fake-claude corpus scripts that cover parity gaps in the existing differential
test suite, then verify both Node and Rust pass them.

## Change Map

```
porting/corpus/
  tool-result-echo.acp.json           NEW — ACP scenario for tool-result user frame
  tool-result-echo.transcript.jsonl   NEW — fake-claude replay with 2 expects
  tool-then-text.acp.json             NEW — ACP scenario: tool turn then plain text turn
  tool-then-text.transcript.jsonl     NEW — fake-claude replay with 3 expects

rust/claude-agent-acp-rs/tests/
  differential.rs                     EDIT — add inv_24_tool_result_echo, inv_24_tool_then_text
```

| File | Today | After |
|------|-------|-------|
| `porting/corpus/tool-result-echo.*` | absent | 2 new files |
| `porting/corpus/tool-then-text.*` | absent | 2 new files |
| `rust/claude-agent-acp-rs/tests/differential.rs` | 17 corpus tests | +2 targeted tests |

## Why These Two

- **`tool-result-echo`** — only scenario that exercises a `user` frame carrying `tool_result`
  content blocks; the phase 15 fix (`map::map_consolidated(MsgRole::User, ...)` in
  `session.rs` `handle_session_frame`) has no regression guard in the synthetic corpus —
  all existing tool scripts (`single-tool`, `multi-tool`) end after the assistant message
  without a tool_result return `user` frame
- **`tool-then-text`** — two sequential prompts where the first uses a tool (distinct from
  `lagging-idle` which is text→text); tests that the session re-enters the activation path
  correctly after a tool-using turn settles

## Key Decisions

1. **Corpus format** — `.transcript.jsonl`: one JSON object per line; each has `expect` +
   `emit`; `"$MATCH.uuid"` in emit substitutes the uuid from the matched stdin frame
   (critical for turn activation — `session.rs` only activates a turn when the echoed user
   frame uuid matches the prompt uuid); `expect` is a REQUIRED field — fake-claude has no
   unconditional emits
2. **Fake-claude `expect` field** — matches by `type` (and optionally nested content fields);
   `type:"user"` matches any user-role stdin frame; `type:"control_request"` with
   `request.subtype:"initialize"` matches the initialize handshake
3. **`tool_result` user frame must NOT use `$MATCH.uuid` or `isReplay:true`** — the adapter
   never writes this frame to stdin; fake-claude emits it as part of the previous step's
   `emit` array; giving it a distinct literal uuid (e.g. `"tool-result-1"`) or omitting
   uuid entirely avoids double-echoing the prompt uuid which would corrupt turn state
4. **All frames for one tool turn go in a single `emit` array** — the adapter writes exactly
   one `user` frame per `session/prompt`; the full sequence (echo → assistant tool_use →
   tool_result user frame → result) must all be in line 2's emit, not split across steps
5. **Inter-turn idle emit uses `lagging-idle` pattern** — emit `{"type":"system","subtype":"session_state_changed","state":"idle"}` INSIDE the second prompt's expect/emit step (see `lagging-idle.transcript.jsonl`); the `state` field is `"state"`, not `"session_state"`
6. **New tests are `#[test]` functions** calling `run_and_diff("tool-result-echo")` /
   `run_and_diff("tool-then-text")` — `inv_24_full_corpus` auto-picks them up by globbing
   the corpus dir, but named tests give clearer failure messages

## Research

- `single-tool.transcript.jsonl` has 2 lines: initialize + (user echo + assistant tool_use +
  result); NO tool_result user frame → phase 15 `map_consolidated(MsgRole::User, ...)` fix
  path never exercised by existing corpus
- `cancel-mid-turn.transcript.jsonl` has 3 lines: emits partial text before cancel →
  `had_usage` path IS exercised there
- `$MATCH.uuid` substitution: `fake-claude/src/lib.rs` `substitute` fn (line ~213)
  replaces `"$MATCH.<field>"` with the field value from the matched stdin frame
- `run_and_diff` helper: `differential.rs:323`; runs Node and Rust each against the
  same fake-claude transcript, normalizes IDs/timestamps, asserts ACP frame equality
- `inv_24_full_corpus` (`differential.rs:314`): globs `porting/corpus/*.acp.json`, auto-picks
  up any new scripts — named `inv_24_*` tests are duplicates for clearer failure messages
- `lagging-idle.transcript.jsonl` line 3: `{"expect":{"type":"user"},"emit":[{"type":"system","subtype":"session_state_changed","state":"idle"}, ...]}` — idle emitted AS PART OF the second user frame step, not as a standalone step

---

## Implementation Phase 1 — `tool-result-echo` corpus + test

Add a corpus script exercising the full tool loop including the return `user` frame.

### Checklist

- [x] 1.1 Write `porting/corpus/tool-result-echo.acp.json`
- [x] 1.2 Write `porting/corpus/tool-result-echo.transcript.jsonl`
- [x] 1.3 Add `fn inv_24_tool_result_echo` to `rust/claude-agent-acp-rs/tests/differential.rs`

### Contracts

**`tool-result-echo.acp.json`** — 2 new files; note `"permission": "allow"` matches other scripts:
```json
{
  "permission": "allow",
  "steps": [
    {"call": "initialize"},
    {"call": "session/new"},
    {"call": "session/prompt", "text": "read a file"}
  ]
}
```

**`tool-result-echo.transcript.jsonl` — exactly 2 lines:**

Line 1 — initialize (copy from `single-tool.transcript.jsonl` line 1 verbatim):
```
{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[...same initialize response as all other scripts...]}
```

Line 2 — ONE step with ALL four frames in the emit array:
- Frame A: prompt echo with `$MATCH.uuid` and `isReplay:true`
- Frame B: assistant message with tool_use (no result yet — this keeps the session active)
- Frame C: tool_result user frame — **distinct literal uuid** (`"uuid":"tool-result-1"`), **no `isReplay`** — this is the frame the phase 15 fix must map
- Frame D: final result

```json
{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"read a file"}]}},{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I will read the file."},{"type":"tool_use","id":"toolu_01RFILE","name":"Read","input":{"file_path":"/tmp/test.txt"}}]}},{"type":"user","uuid":"tool-result-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01RFILE","content":[{"type":"text","text":"hello from file"}]}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"The file contains: hello from file","usage":{"input_tokens":15,"output_tokens":8,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}]}
```

**Why this works:** fake-claude emits all frames in one `emit` array when the matching stdin
frame arrives. The adapter writes exactly one `user` stdin frame per `session/prompt` — that
single frame triggers the whole sequence. Frame C (tool_result user) comes from fake-claude
stdout and is processed by `session.rs::handle_session_frame` — this is the code path the
phase 15 fix added `map_consolidated(MsgRole::User, ...)` to.

**Test fn (`differential.rs`):**
```rust
#[test]
fn inv_24_tool_result_echo() {
    run_and_diff("tool-result-echo");
}
```
Place after `inv_24_commands_proactive` (around line 390 in `differential.rs`).

**Verify phase 1:**
- [x] 1.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs inv_24_tool_result_echo 2>&1 | grep -E "test result|FAILED"`
- [x] 1.T2 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs inv_24_full_corpus 2>&1 | grep -E "test result|FAILED"` — must still pass (now 18 scripts)
- [x] 1.T3 `bash rust/scripts/rust-gate.sh 2>&1 | tail -5` — all tests green

---

## Implementation Phase 2 — `tool-then-text` corpus + test

Add a corpus script: two sequential prompts where the first turn uses a tool (different from
`lagging-idle` which is text→text; tests re-activation after a tool-using turn).

### Checklist

- [x] 2.1 Write `porting/corpus/tool-then-text.acp.json`
- [x] 2.2 Write `porting/corpus/tool-then-text.transcript.jsonl`
- [x] 2.3 Add `fn inv_24_tool_then_text` to `rust/claude-agent-acp-rs/tests/differential.rs`

### Contracts

**`tool-then-text.acp.json`:**
```json
{
  "permission": "allow",
  "steps": [
    {"call": "initialize"},
    {"call": "session/new"},
    {"call": "session/prompt", "text": "read a file"},
    {"call": "session/prompt", "text": "say done"}
  ]
}
```

**`tool-then-text.transcript.jsonl` — 3 lines:**

Line 1 — initialize (copy from `single-tool.transcript.jsonl` line 1 verbatim):
```
{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[...same initialize response as all other scripts...]}
```

Line 2 — turn 1: prompt echo + assistant tool_use + tool_result user frame + result
(identical to `tool-result-echo` line 2 — see Phase 1 contract; use message text `"read a file"`):
```json
{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"read a file"}]}},{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I will read the file."},{"type":"tool_use","id":"toolu_01RFILE","name":"Read","input":{"file_path":"/tmp/test.txt"}}]}},{"type":"user","uuid":"tool-result-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01RFILE","content":[{"type":"text","text":"hello from file"}]}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"The file contains: hello from file","usage":{"input_tokens":15,"output_tokens":8,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}]}
```

Line 3 — turn 2: idle (lagging) + second prompt echo + result
(follow the `lagging-idle` pattern: emit the idle state change FIRST inside the second prompt's step):
```json
{"expect":{"type":"user"},"emit":[{"type":"system","subtype":"session_state_changed","state":"idle"},{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"say done"}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"done","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}]}
```

**Note on `state` vs `session_state`:** the field is `"state":"idle"` (NOT `"session_state":"idle"`) — confirm by reading `lagging-idle.transcript.jsonl` line 3 before writing.

**Test fn (`differential.rs`):**
```rust
#[test]
fn inv_24_tool_then_text() {
    run_and_diff("tool-then-text");
}
```
Place after `inv_24_tool_result_echo`.

**Verify phase 2:**
- [x] 2.T1 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs inv_24_tool_then_text 2>&1 | grep -E "test result|FAILED"` — passed (1 passed)
- [x] 2.T2 `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs inv_24_full_corpus 2>&1 | grep -E "test result|FAILED"` — must still pass (now 19 scripts) — passed
- [x] 2.T3 `bash rust/scripts/rust-gate.sh 2>&1 | tail -5` — final gate, all green — gate OK

---

## Files & Phase Impact

| File | Phase | Change |
|------|-------|--------|
| `porting/corpus/tool-result-echo.acp.json` | 1 | NEW |
| `porting/corpus/tool-result-echo.transcript.jsonl` | 1 | NEW |
| `rust/claude-agent-acp-rs/tests/differential.rs` | 1, 2 | +2 `#[test]` fns |
| `porting/corpus/tool-then-text.acp.json` | 2 | NEW |
| `porting/corpus/tool-then-text.transcript.jsonl` | 2 | NEW |

## Out of scope

- Re-recording real-claude corpus transcripts (`porting/record-real.sh`) — requires human, see security constraint
- Changes to `src/` files — this is a test-only addition
- Any change to `PARITY.md` or `INVARIANTS.md`
