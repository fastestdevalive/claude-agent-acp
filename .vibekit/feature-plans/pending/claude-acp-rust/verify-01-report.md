# Functional verification — claude-acp-rust-rs (`feat/rust-port`, `rust-v0.1.0+acp.0.70.0`)

Hands-on pass driving the binary/library like a real consumer would, on top of
the already-green `rust/scripts/rust-gate.sh` (147 tests). Not a re-run of the gate.

## What was actually done

1. **Cold-ish build** — cleared `target/debug/{incremental,build}` and ran
   `cargo build --manifest-path rust/Cargo.toml --workspace --bins`. Compiled
   `fake-claude`, `claude-agent-acp-rs`, `acp-recorder` cleanly in 25s, zero
   warnings printed (no `-A`/allow flags needed to get a clean build).
2. **Hand-driven stdio session** — spawned `rust/target/debug/claude-agent-acp-rs`
   as a raw subprocess (Python, not the test harness), piped raw JSON-RPC lines
   over stdin, read stdout/stderr with background reader threads. Ran
   `initialize` → `session/new` → `session/prompt` against `fake-claude` loaded
   with `porting/corpus/text-only.transcript.jsonl`. Also fed a garbage
   (non-JSON) stdin line and an unknown method, and confirmed clean EOF
   shutdown (exit 0, no hang, no panic).
3. **In-process example** — ran
   `cargo run --manifest-path rust/Cargo.toml --example in_process -p claude-agent-acp-rs`
   with `CLAUDE_CODE_EXECUTABLE`/`FAKE_CLAUDE_SCRIPT` pointed at fake-claude +
   the text-only transcript. Exit 0.
4. **Real differential capture** — `npm run build` (dist/index.js already
   present, rebuilt clean), then `CAPTURE_OUT_DIR=/tmp/capture-out bash
   porting/capture.sh text-only single-tool cancel-mid-turn` to record the
   **real Node adapter** against fake-claude, and `diff`'d byte-for-byte
   against the committed `porting/fixtures/*.frames.jsonl` / `*.argv.json`.
5. **Consumer-fit read** — re-read D9's "vibe-station consumes" row and
   follow-up touch points against the current `ServeOptions`/`Timings`/`serve()`
   shapes in `agent.rs`/`process.rs`.
6. **Env inheritance, empirically** — swapped `CLAUDE_CODE_EXECUTABLE` for a
   throwaway shell script that dumps its full env to a file, drove the binary
   through `session/new` (which spawns the child), and inspected the dumped
   env directly — not a fixture, not `fake-claude`'s allow-listed recorder.

## Findings

| # | Severity | Area | Finding | Evidence | Recommendation |
| - | -------- | ---- | ------- | -------- | --------------- |
| 1 | minor | stdio binary UX | On a malformed (non-JSON) stdin line the binary replies `{"id":null,"error":{"code":-32700,...,"data":{"line":"not json garbage"}}}` — it **echoes the raw offending input line back into an ACP frame** rather than staying silent/logging to stderr. A malicious or buggy client could get its own garbage reflected into a JSON-RPC stream a downstream tool parses. | Hand-drive test, this session: sent `not json garbage`, got that exact frame back on stdout. | Confirm this matches the Node adapter's behavior (if Node does the same, it's parity and fine — no PARITY.md row currently covers "malformed input" explicitly, worth a one-line row); otherwise scrub or size-cap `data.line`. |
| 2 | minor | consumer fit (D9) | `ServeOptions` has no explicit graceful-shutdown/drain method for the in-process (`Channel::duplex()`) path — `serve()` only returns when the transport closes (drop the channel end). This matches the documented design ("let the crate own child teardown") but is easy for a real integrator to miss: nothing in the public API surface documents "drop your `Channel` end to shut this session set down," it's only in doc comments on `bin/claude-agent-acp-rs.rs`. | `rust/claude-agent-acp-rs/src/agent.rs:376` (`serve` signature/body), no separate `shutdown()`/`dispose()` fn in the public surface. | Not a blocker — just flag for the vibe-station-side follow-up plan (`acp_connection.rs`) that closing/dropping the `Channel` end *is* the shutdown signal; make sure that doc line survives into the integration plan. |
| 3 | none (confirmed working) | env inheritance (task item 6) | **Yes, runtime matches Node's full-env-inheritance behavior.** Empirically verified: a custom `MY_CUSTOM_SECRET_VAR` set in the parent process reached the spawned child untouched; `CLAUDE_CODE_ENTRYPOINT=sdk-ts` and `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` were present; a parent-set `NODE_OPTIONS` was absent in the child. Source: `apply_claude_env` (`rust/claude-agent-acp-rs/src/process.rs:440-447`) only calls `cmd.env(...)` and one `cmd.env_remove("NODE_OPTIONS")` — no `.env_clear()` anywhere in `process.rs`, so `tokio::process::Command`'s default (full parent-env inheritance) stands. `ARGV_ENV_ALLOWLIST` (`rust/fake-claude/src/lib.rs:133`) is used only inside `fake-claude`'s own `record_argv_env` (test-fixture writer) — grepped for its only two call sites, both in `fake-claude/src/lib.rs`, never referenced from `claude-agent-acp-rs`. | `process.rs:440-447`; `fake-claude/src/lib.rs:133-173`; hand-run env dump this session. | No action — this is the desired behavior, worth citing directly in PARITY.md/INVARIANTS.md as a proven row since it was previously only inferable from fixture allow-listing, not asserted against the real spawn path. |
| 4 | none (confirmed working) | differential corpus (task item 4) | Re-captured `text-only`, `single-tool`, `cancel-mid-turn` fresh against the **real, just-built Node adapter** (`dist/index.js`) and diffed byte-for-byte against the committed fixtures used by the Rust differential tests — all three identical. This also resolves the phase-2 open item ("(7) All 11 corpus transcripts are hand-authored ... must be re-recorded before phase 13") noted in `PARITY.md`: at least these 3 are demonstrably the real Node-recorded output now, not hand-authored guesses. | `diff porting/fixtures/{text-only,single-tool,cancel-mid-turn}.{frames.jsonl,argv.json} /tmp/capture-out/...` → all "IDENTICAL". | Update `PARITY.md`'s phase-2 deviation note (item 7) to reflect that re-recording has happened (out of scope for me to edit per instructions — flagging for the plan owner). |
| 5 | none (confirmed working) | stdio hygiene / protocol correctness | stdout carried **only** JSON-RPC frames across all three exercises (normal turn, garbage input, unknown method); stderr was empty in the success path; no panic; clean `exit 0` on stdin EOF with no hang. Unknown method correctly returned `-32601`. | Hand-drive transcripts, this session. | None. |
| 6 | none (confirmed working) | consumer fit (D9) | `ServeOptions { claude_path, extra_env, default_cwd, timings: Timings }` and `Timings { stdin_close_wait, term_to_kill_wait, force_cancel_grace, stderr_drain_cap }` cover every field the plan's D9 follow-up line calls out (binary path, env, cwd, grace constants) for `acp_connection.rs` to plug in. | `agent.rs:38-71`; `process.rs:50-70`. | None. |

## Verdict

**works-as-desired**

No blockers or majors found. The two minor items (raw-line echo on parse
error, and the undocumented "closing the Channel = shutdown" contract) are
worth a follow-up note but don't block consumption. Everything else — cold
build, hand-driven stdio session, in-process example, real differential
capture against the actual Node adapter, and empirical full-env-inheritance —
checked out exactly as `PARITY.md`/`INVARIANTS.md` claim.
