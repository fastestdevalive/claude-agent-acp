# Group G14 (LD-3: Live Divergences Part 3) Progress

- Base: `9e9fe1c`
- Worktree: `/private/tmp/claude-501/par-c`
- Status: COMPLETED

## Checklist

- [x] 3.0 Reconcile with HEAD (F2c request IDs in codec.rs, D8 oldText:null, D6 first-cut session titles)
- [x] 3.1 Pure functions + units: `map::materialize_tool_call_defaults`, `map::materialize_diff_old_text` + `inv_79_*` units
- [x] 3.2 Wiring: wire layer and forwarder / permission / hooks / transcript typed-update materialisers
- [x] 3.3 Corpus authoring: `tool-call-empty-locations`, `write-new-file-diff`, `permission-two-requests` via `capture.sh`
- [x] 3.4 Delete test normalisers `normalize_tool_call_defaults` & `normalize_request_permission_id`, verify full differential
- [x] 3.5 D6 session titles first cut: `session_titles.rs` + `inv_80_*` units, wiring, script `session-title` via `capture.sh`
- [x] 3.6 Docs & row flips: T-40, T-42, T-78 in FEATURES.md, LIVE-FINDINGS-2026-10-23.md, PARITY.md, INVARIANTS.md
- [x] 3.7 Clean-cache reproduction verification
- [x] 3.8 Selftest gate & full verification (fmt, clippy, tests, diff x3, gate OK)

## Implementation & Test Evidence

### 3.1 & 3.2 Typed update materialisers & Request IDs
- Implemented `map::materialize_tool_call_defaults`: adds wire defaults (`status: "pending"`, `content: []`, and `locations: []` for file tools `Read`, `Write`, `Edit`, `NotebookEdit`, `Glob`) on typed `SessionUpdate::ToolCall` and `RequestPermission` updates.
- Implemented `map::materialize_diff_old_text`: adds `oldText: null` to typed file `Diff` updates when `old_text` is `None` (for new file writes and edits with empty old string).
- Wired in `agent.rs` (`register_session` updates forwarder), `permission.rs` (`build_tool_call`), `transcript.rs` (`replay_content`).
- Implemented `codec::RequestIds` in `codec.rs`: sequential integer IDs (`0`, `1`, ...) assigned to agent-initiated outbound JSON-RPC requests, original string IDs restored on response, cancel request ID rewrite.
- Unit tests: 11 passing `inv_79_*` tests in `map.rs` and `codec.rs`.

### 3.3 & 3.4 Corpus authoring & Test normaliser deletion
- Created corpus scenarios and captured fixtures via `capture.sh`:
  - `tool-call-empty-locations`: tool calls with empty inputs gain empty `locations: []` on file tools.
  - `write-new-file-diff`: Write file diff carries `oldText: null`.
  - `permission-two-requests`: two sequential permission requests receive sequential integer IDs `0` and `1`.
- Added differential tests in `tests/differential.rs`: `inv_24_tool_call_empty_locations`, `inv_24_write_new_file_diff`, `inv_24_permission_two_requests`.
- Deleted test normalisers `normalize_tool_call_defaults` and `normalize_request_permission_id` from `tests/common/mod.rs` and `tests/differential.rs`. Verified 0 grep hits for both names. All 95 differential corpus scenarios pass without test normalisation.

### 3.5 Session titles first cut (D6)
- Implemented `session_titles.rs` (`SessionTitles`): rolling context up to 1000 characters collected from prompt and assistant text, sends `generate_session_title` control request to CLI at `session_state_changed: idle`, adopts stored titles, publishes `session_info_update`.
- Unit tests: 6 passing `inv_80_*` tests in `session_titles.rs`.
- Wired into `session.rs`: prompt text collection, assistant text collection, `session_state_changed: idle` turn end trigger, `TitleGenerated` command handling, and `conversation_reset`.
- Authored corpus scenario `session-title` and captured fixture via `capture.sh`.
- Added differential test `inv_24_session_title` in `tests/differential.rs`.

### 3.6 - 3.8 Verification
- Updated `porting/parity/FEATURES.md`: flipped rows `T-40`, `T-42`, `T-78` in place with test evidence; header counts preserved.
- Updated `porting/parity/LIVE-FINDINGS-2026-10-23.md`, `porting/PARITY.md`, `porting/INVARIANTS.md` (`INV-79`, `INV-80`).
- Verified clean cache reproduction: all 4 new fixtures matched byte-for-byte.
- Verified `selftest-gate.sh`: OK.
- Verified `rust-gate.sh`: fmt, clippy, build, all 572 unit tests, integration tests, 95 differential tests, windows and mac cross-target checks, cfg checks, dependency pin check, parity maps check all PASSED.
- Verified untouched files: `turn.rs`, `turn/turn_tests.rs`, and `unfinished.rs` have 0 diff.
- Verified 0 instances of `JsonPath::new` and 0 new `#[ignore]`.

READY:G14
