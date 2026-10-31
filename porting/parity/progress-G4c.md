# G4c (F2c) — transport wrapper + session method error shapes

Worktree: `/private/tmp/claude-501/par-b` @ 2b73ced (branch feat/rust-port).
Group: G4c. Owner rows: M-52, M-126, M-01, M-20 (+ WB-05 tagged F2c).

## Checklist

- [x] Read upstream: `acp-agent.ts:13528-13552` ($/cancel_request), `10925-10944`
      (Invalid params prefix), `708-738` (parseSteerRequest), `4315-4356` (steer),
      ACP SDK `dist/core/stream.js` + `dist/core/jsonrpc.js` (parse-error shape,
      `protocolErrorResponse`, `nextRequestId++`).
- [x] Read Rust: `codec.rs`, `agent.rs` (serve loop, validate_cwd, steer handler),
      `bin/claude-agent-acp-rs.rs`, tests.
- [x] Wire codec in `codec.rs`:
      - parse line -> PassThrough / ParseError(-32700) / InvalidRequest(-32600)
        exactly like Node's ndJsonStream (`{"jsonrpc":"2.0","id":null,...}`).
      - outgoing request ids rewritten string-uuid -> sequential number (Node
        `nextRequestId++`), incoming responses mapped back; `$/cancel_request`
        requestId mapped out.
- [x] Transport wrapper `ConnectTo<Agent>` over stdio (bin wiring only).
- [x] `agent.rs`: `$/cancel_request` -> cancel the prompt (Responder cancellation).
- [x] `agent.rs`: validate_cwd message prefix `Invalid params: `.
- [x] `agent.rs`: `_session/steering` error shape `Invalid params: <text>` with NO
      data; reject non-string idleBehavior; params-object check.
- [x] Failing-first unit tests (codec shapes/ids, cancel routing, error text,
      idleBehavior).
- [x] Transport-level tests over a duplex pipe (recorder cannot express a raw
      malformed line or a `$/cancel_request` notification -> note why below).
- [x] fmt / clippy / --lib / differential / workspace / rust-gate.sh / JsonPath 0
      / cargo tree agent-client-protocol 2.1.0.
- [x] Flip FEATURES rows M-52, M-126, M-01, M-20 in place (evidence both sides);
      WB-05 removed (its status follows M-52 per DECISIONS-FOR-USER.md).

## Notes

- Recorder expressiveness: `porting/corpus/*.acp.json` steps are
  initialize / session/new / load / prompt / steer / cancel / wait /
  elicit_reply / request / sleep. There is no step that writes a raw malformed
  line and no notification step for `$/cancel_request` (the `request` step
  always awaits a response, which a notification never sends). So those two
  wire cases are covered by transport-level tests over a hermetic duplex pipe
  instead of a Node-captured corpus scenario.
- The permission id change is visible over stdio only; the existing
  `normalize_request_permission_id` normaliser is left untouched (no weakening).
- Found and fixed during verification: the blocking stdio reader drained the
  writer's `(number, uuid)` pairs *before* parking in `read`, so a mapping
  assigned while blocked (every permission request) was missed and the client's
  numeric reply reached the crate untranslated -> every permission corpus hung.
  The drain now happens after `read` returns; regression test
  `codec::tests::inv_52_blocking_reader_drains_ids_after_read` reproduces the
  interleaving deterministically (gate reader parked inside `read`).
- TEST-MAP: the two upstream `runPromptWithCancellation` cases are mapped to
  `wire_cancel_request_cancels_the_prompt` and
  `wire_late_cancel_request_is_ignored`.

## Commands run (all real, all green)

- `cargo fmt --manifest-path rust/Cargo.toml --all` (clean after formatting).
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` -> clean.
- `cargo test -p claude-agent-acp-rs --lib` -> 402 passed.
- `cargo test -p claude-agent-acp-rs --test differential` -> 65 passed, 1 ignored
  (pre-existing ignore, not added here), 21s.
- `cargo test --workspace` -> all suites pass (differential 65/1 ignored,
  wire_transport 4, agent 7, etc.).
- `bash rust/scripts/rust-gate.sh` -> `==> gate OK` (fmt, clippy, bins, cache,
  workspace tests, both cross-target checks, G9, G10, parity maps).
- JsonPath guard: `git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'` -> `0`.
- `cargo tree -p claude-agent-acp-rs | grep agent-client-protocol` -> `v2.1.0`
  (G10: no crate bump; wrapper is our own code).

READY:G4c
