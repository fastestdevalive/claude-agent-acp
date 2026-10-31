# Progress: Turn 1, Group G7afix3 (fixes from third Opus review of 9a38128)

Worktree: /private/tmp/claude-501/par-b (detached at 9a38128 + uncommitted fix3 edits).
Review: scratchpad/review-G7afix2.md (authoritative). Previous session's edits were
verified against the working tree, not against its checklist.

## Findings

- [x] **Finding 1 — MANAGED SOURCES vs SDK.** The `[{}]` fail-closed sentinel is gone:
      `read_os_managed_source` returns `Option<Value>` (`None` = unread/absent) and the
      file tier is selected by the new pure `select_managed_docs` (`process.rs:1284`),
      quoting sdk.mjs `e4`/`Sx`/`Oc`/`y4` in comments. A `reg.exe` timeout is
      `RegQuery::Unreadable` -> treated as absent for that resolve. Tests:
      `process::tests::managed_source_unread_os_keeps_file_tier` (None + file -> file,
      no `[{}]`), `settings::tests::test_unreadable_mdm_plist_drops_only_mdm_tier_file_tier_enforced`
      (deniedModels, disableBypassPermissionsMode, managed apiKeyHelper, managed env),
      `process::tests::managed_file_tier_reads_d_sorted_skips_hidden_and_invalid`.
- [x] **Finding 2 — CACHE.** The `OnceLock` now caches only the OS tier
      (`OsSourceRead`: `Absent`/`Present` cached, `Unreadable` never cached) via
      `read_cached_os_source` (`process.rs:1223`); the managed FILE tier is re-read on
      every resolve (`read_managed_file_sources_at`, `process.rs:1257`). Tests:
      `process::tests::os_source_cache_caches_definitive_results_only`,
      `process::tests::managed_file_tier_is_reread_per_resolve`,
      `settings::tests::test_load_with_managed_file_rereads_after_edit`,
      `settings_manager::tests::test_settings_manager_managed_file_hot_reload`
      (edit managed-settings.json after first load -> on_change fires and the new
      content is returned).
- [x] **Finding 3 — HERMETIC INJECTION.** `EnvSnapshot::managed_docs` (injected
      documents), `SettingsManagerOptions.managed_docs` + `managed_file`,
      `ServeOptions.managed_docs`; `load_with_managed`/`load_with_managed_file` have
      real callers (`settings::load`, `SettingsManager`, `agent::spawn_inputs`).
      `EnvSnapshot::empty()`/`from_pairs()` inject an empty tier.
      **CORRECTION (G7a-fix4 review):** the claim that "no test reads the host
      `/Library/...` or `/etc/claude-code`" was FALSE for this commit. The auth
      actor still built `EnvSnapshot::from_process()` in
      `read_api_key_helper`, so `inv_50_actor_session_account_and_cwd_reprobe`,
      `inv_26_transport_parity`, three `wire_transport` tests and the agent
      session tests read the host managed tier. Fixed in G7a-fix4 by threading
      the serve `EnvSnapshot` into the actor and enforcing hermeticity with
      `process::ManagedSourceGuard` (armed by those tests).
      `settings::tests::test_hermetic_load_with_managed_docs` asserts the injected
      source is the only one consulted.
- [x] **Finding 4 — DEBOUNCE TEST.** Poll interval pinned to 20 ms, the two writes are
      60 ms apart (< 100 ms debounce) so each write is observed while the timer is
      armed: debounce -> 2 reloads, no debounce -> 3. **Mutation proof (recorded):**
      temporarily changed `debounce_deadline = Some(Instant::now() + 100ms)` to
      `Some(Instant::now())`; the test FAILED with
      `expected exactly 2 reloads ... got 3`; mutation reverted and the test passes.
- [x] **Finding 5 — UNSAFE.** `lib.rs` is now
      `#![cfg_attr(not(windows), forbid(unsafe_code))]` +
      `#![cfg_attr(windows, deny(unsafe_code))]`, so `forbid` is preserved on every
      non-Windows target and only Windows keeps `deny` with the single scoped
      `#[allow(unsafe_code)]` on the private `windows_job` module (no safe wrapper
      crate exists in `Cargo.lock`). Decision D8 records the exemption and the reason;
      docs updated in `lib.rs`, `process.rs` header, `rust/AGENTS.md`.
- [x] **Finding 6 — JOB OBJECT RACE.** The process-tree comment now states that
      descendants created AFTER assignment inherit the job and that descendants spawned
      in the assignment window escape it; Windows coverage is explicitly NOT claimed to
      be complete. Also in CONCURRENCY.md and Decision D8. `FEATURES.md` line 15 says
      "compile-checked, known assignment window limitation".
- [x] **Finding 7 — WINDOWS CLIPPY.** `map_or` replaced by `is_some_and`; the new
      `RegQuery`/`OsSourceRead` code is clippy-clean for
      `--target x86_64-pc-windows-gnu --all-targets -- -D warnings` (see acceptance).
- [x] **Finding 8 — PROBE.** `is_reaper_binary` no longer has a file-name fast path; it
      runs `--reaper --probe` in a `std::thread` with a 2 s bound (killpg on timeout).
      **CORRECTION (G7a-fix4 review):** it DID block the async worker — the thread was
      joined synchronously from `async fn spawn_raw`. Fixed in G7a-fix4 with
      `block_in_place` on a multi-thread runtime plus a short-TTL cache for
      indeterminate (hanging/spawn-failure) outcomes. It caches definitive outcomes per path
      (`ProbeOutcome::{Pass,Fail,Indeterminate}`; timeouts are retried). Tests in
      `tests/process_transport.rs::test_reaper_probe_handshake_scenarios`: probe-pass +
      cache, direct `--reaper --probe` exits 0 with the marker and no runtime (no
      recursion), probe-fail stray binary -> no reaper, probe-timeout -> no reaper.

## Files changed

- `rust/claude-agent-acp-rs/src/process.rs` (managed sources/cache, reaper probe,
  Windows Job Object module comment, tests)
- `src/settings.rs` (`load_with_managed_file`, hermetic tests)
- `src/settings_manager.rs` (`managed_file` seam, managed hot reload, discriminating
  debounce test)
- `src/env_snapshot.rs`, `src/agent.rs` (injected managed docs plumbing)
- `src/lib.rs` (forbid on non-Windows), `src/bin/claude-agent-acp-rs.rs` (probe entry;
  unchanged in fix3), `tests/process_transport.rs` (probe tests)
- `rust/AGENTS.md`, `porting/parity/{FEATURES,TEST-MAP,DECISIONS-FOR-USER,CONCURRENCY}.md`

## Acceptance (real runs, UPSTREAM_CACHE_DIR=<oracle>)

- `cargo fmt --all --check`: PASS
- `cargo clippy --workspace --all-targets -- -D warnings`: PASS
- `cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings`: PASS
- `cargo test -p claude-agent-acp-rs --lib`: 564 passed, 0 failed
- reaper tests x10 (`--test process_transport -- reaper inv_28`): 10/10 runs
  `5 passed; 0 failed`
- settings tests with `HOME=<empty temp dir>`: 23 passed, 0 failed; temp HOME stays empty
- `cargo test --test differential`: 91 passed, 0 failed
- `cargo test --workspace`: PASS
- `cargo check` x86_64-pc-windows-gnu / aarch64-apple-darwin / x86_64-unknown-linux-musl: PASS
- `bash rust/scripts/rust-gate.sh`: **==> gate OK** (fmt, clippy, build, capture,
  workspace tests, cross-target windows/darwin checks, G9/G10, TEST-MAP+MODULE-MAP
  PASS, PARITY PASS)
- JsonPath guard (`git diff -U0 455c146 ... | grep -c 'JsonPath::new'`): 0
- `bash rust/scripts/parity/gate-parity.sh`: TEST-MAP CHECK PASS, MODULE-MAP CHECK PASS,
  PARITY: PASS
- cfg grep outside process.rs (`cfg(unix)|cfg(windows)|target_os`): prints nothing
- `Cargo.lock` untouched; `agent-client-protocol` still 2.1.0

## Notes

- No new corpus scenario was needed: these fixes are not wire-visible (managed settings
  resolution feeds argv/options and process lifecycle, not ACP frames).
- `apply_managed_policy_env` keeps its explicit-path test helper
  (`apply_managed_policy_env_at`); production now resolves the MDM/file tiers through
  `read_managed_sources`, matching `managed-policy.ts` (no remote-tier slot).
- The Windows assignment window is documented, not closed; nothing claims full
  Windows process-tree coverage.

READY:G7afix3

