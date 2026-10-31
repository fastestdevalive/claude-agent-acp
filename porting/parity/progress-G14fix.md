# Group G14fix — fixes from Opus review of 180ace9 (LD-3)

- Base: `13a78e2`
- Worktree: `/private/tmp/claude-501/par-c`
- Status: READY

## Checklist

- [x] 1. `materialize_diff_old_text` only touches `content[*]` of tool_call / tool_call_update
- [x] 2. Title lookup async, cwd-scoped, cached once settled (no actor blocking)
- [x] 3. Title context from plain ACP text chunks, not converted SDK frame
- [x] 4. Result-text chunk calls `on_assistant_text`
- [x] 5. Batch request-id rewrite restores every member id
- [x] 6. Prune request-id correlation maps; delete dead helpers
- [x] 7. AIR permission `toolCall` has no `locations:[]`
- [x] 8. FEATURES T-40 PARTIAL + evidence/citation fixes (T-40/T-42/T-78/INV-79)
- [x] 9. fmt / clippy / lib tests / turn tests / differential x3 / transport / gate / guards

## Summary of Changes

### Findings Resolved
1. **`materialize_diff_old_text` scoped to diff content blocks**:
   - `map.rs`: `materialize_diff_old_text` only walks `content[*]` of `tool_call` / `tool_call_update` and permission `toolCall` (by `toolCallId`). Does not touch `rawInput`, `rawOutput`, or `_meta.claudeCode.toolResponse`.
   - `agent.rs`: RawUpdate forwarder only passes `update` to `materialize_diff_old_text`. `SessionOutbound::RequestPermission` avoids redundant second materialization.
   - Tested by unit test `inv_79_diff_only_in_content` and Node-captured differential corpus `mcp-diff-raw-verbatim`.
2. **Off-actor async title lookup**:
   - `session.rs`: Turn-end title transcript resolution runs via `tokio::task::spawn_blocking` and re-enters the actor through `Command::TitleInfo`.
   - `session_titles.rs` & `session_store.rs`: Scoped to cwd project directory via `find_transcript_for_dir`, and caches the resolved transcript path once settled so later idle turns avoid filesystem scans.
   - Tested by `inv_80_settled_title_uses_cached_transcript_no_scan`.
3. **Title context from plain ACP text chunks**:
   - `agent.rs`: Added `acp_prompt_text` to extract only plain ACP `type:"text"` blocks (matching Node `session-titles.ts:131-140`), passed into `Command::Prompt`.
   - Never feeds embedded file text (`<context ref=...>`) or resource links (`[@name](...)`) into title generation.
   - Tested by `inv_80_title_context_from_acp_text_only`.
4. **Top-level result text feeds assistant title text**:
   - `session.rs`: `TurnEvent::FinalText` chunk in `settle_events` calls `titles.on_assistant_text(text)`, matching Node collecting all top-level assistant text chunks.
   - Tested by `inv_80_result_text_feeds_title_context`.
5. **Batch request-id rewrite restores all member IDs**:
   - `codec.rs`: `rewrite_outgoing` returns `Vec<(u64, String)>` so all mapped numeric IDs for every batch member are sent over `assigned_tx` to the incoming reader map.
   - Tested by `inv_79_batch_entries_mapped` with separate maps for both directions.
6. **Correlation maps pruned on reply and capped**:
   - `codec.rs`: Drops correlation entries on incoming response restoration via `by_number.remove(&id)`; correlation map size capped with FIFO eviction (`MAX_TRACKED_IDS = 4096`). Deleted dead helpers (`assign`, `restore`, `IdMap`, `normalize_*`).
   - Tested by `inv_79_correlation_maps_pruned_and_capped`.
7. **AIR permission `toolCall` aligned with Node shape**:
   - `permission.rs`: Node's AIR `permissionToolCall` shape sends only `toolCallId`, `title`, and `rawInput` (with file text stripped for Write/Edit); no `locations:[]`, `status`, `name`, `content`, `kind`, or `_meta`. Non-AIR keeps full shape.
   - Tested by `inv_79_air_permission_tool_call_has_node_shape`.
8. **FEATURES and INVARIANTS evidence & citations**:
   - `porting/parity/FEATURES.md`: Reverted T-40 to PARTIAL with explicit gap note naming unwired diffPatch/v2/replay gate and T-44 MISSING. Fixed fabricated test names and code references for T-40, T-42, T-78.
   - `porting/INVARIANTS.md`: Updated INV-79 and INV-80 citations to exact real test names and code lines.
   - `porting/PARITY.md`: Updated D2, D6, D7, D8 notes and evidence.

### Files Changed
- `rust/claude-agent-acp-rs/src/map.rs`
- `rust/claude-agent-acp-rs/src/permission.rs`
- `rust/claude-agent-acp-rs/src/agent.rs`
- `rust/claude-agent-acp-rs/src/codec.rs`
- `rust/claude-agent-acp-rs/src/session.rs`
- `rust/claude-agent-acp-rs/src/session_titles.rs`
- `rust/claude-agent-acp-rs/src/session_store.rs`
- `rust/claude-agent-acp-rs/tests/differential.rs`
- `rust/scripts/rust-gate.sh`
- `porting/corpus/mcp-diff-raw-verbatim.acp.json`
- `porting/corpus/mcp-diff-raw-verbatim.transcript.jsonl`
- `porting/fixtures/mcp-diff-raw-verbatim.argv.json`
- `porting/fixtures/mcp-diff-raw-verbatim.frames.jsonl`
- `porting/INVARIANTS.md`
- `porting/PARITY.md`
- `porting/parity/FEATURES.md`
- `porting/parity/progress-G14fix.md`

### Commands Run and Status
- `cargo fmt --manifest-path rust/Cargo.toml --all -- --check`: PASS
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`: PASS (0 warnings)
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`: PASS (578 passed, 0 failed, 0 ignored)
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib turn`: PASS (60 passed, 0 failed)
- `cargo test --manifest-path rust/Cargo.toml --test differential` (x3 runs): PASS (96 passed, 0 failed, 0 ignored on all 3 runs)
- `cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test shutdown --test control_transport --test transport_parity`: PASS
- `cargo test --manifest-path rust/Cargo.toml --workspace -- --test-threads=4`: PASS
- `git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'`: PASS (output: 0)
- `bash rust/scripts/parity/test-map-check`: PASS (upstream tests: 2400 | mapped rows: 2400 | broken targets: 0 | bad syntax: 0)
- Hazard files diff (`turn.rs`, `turn/`, `unfinished.rs`): PASS (empty diff)
- `UPSTREAM_CACHE_DIR=... bash rust/scripts/rust-gate.sh`: PASS (`==> gate OK`)

READY:G14fix
