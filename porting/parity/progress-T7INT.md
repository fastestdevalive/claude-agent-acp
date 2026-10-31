# T7 integration (T7INT) progress

Worktree care-1, branch feat/rust-port at c8c19dd. Lanes: A 889fc0a, B 53f9c31, C 1f90087, D ff8da86.

- [x] 1 cherry-pick A, B, C, D
- [x] 2 wiring patches (A, C, D; B empty)
- [x] 3 DECISIONS-FOR-USER.md: one D11, D7 update, D12
- [x] 4 FEATURES rows + header counts, TEST-MAP, MODULE-MAP (lane D rows)
- [x] 5 CONCURRENCY.md race results + Decision paragraph
- [x] 6 integration gate (fmt, clippy, build, test-map-check, module-map-check)
- [x] 7 STATUS.md TURN 7 (integration) section
- [x] 8 commit

PKG-DONE:1 cherry-picked A bb7d669, B 68bdfb2, C 4a44c24, D 16687ac in order; only conflict was DECISIONS-FOR-USER.md D11/D12 (B vs D), resolved to one merged D11 + D12 and the auto-merged D7 update; .gitignore and src/process.rs left dirty and never staged.
PKG-DONE:2 lane-A and lane-C wiring patches already contained in their commits (git apply --check fails, content present: agent.rs note_prompt_meta, t6a2 <300 ms, lib.rs mod tests_t7c, map.rs plan_file/ShapeOptions, permission.rs ShapeOptions); lane B empty; lane D session_store projectsRoot NFC applied a5d33f5.
PKG-DONE:3 DECISIONS-FOR-USER.md: single D11 (windows-sys Win32_System_Threading + wider D8 unsafe scope, recommendation keep fail-open), D7 updated (0.1.0+upstream.0.94.0 vs 0.94.0, src/index.ts:43-45, -V extra), D12 NFC tables.
PKG-DONE:4 FEATURES rows applied A+B+C+D (71 rows); header line 8 recomputed by hand: 887 rows incl. 91 module rows, DONE 870 / PARTIAL 17 / MISSING 0 (the T6 header was 27 DONE high: real cells were 821/51/15); per-section line recomputed too. TEST-MAP: A 8, B 12 (titles corrected from "ACP v2 sessions" to "ACP v2 prompts", duplicate and the already-ported history-replay row dropped), C 33, D 10 -> unclassified 1112. MODULE-MAP: lane D's 5 rows applied (project-dirs incl. nfc modules, session-index/connection, air-extension, async-tasks, native-subagents).
PKG-DONE:5 CONCURRENCY.md: lane A's CONC-2 results present (§5.1-5.4); added §5.5 Decision (actor model, no lock across await, evidence = race suite + conc_no_lock_across_await + conc_adapter_death) and replaced the stale "will be populated" note in §4.
PKG-DONE:6 CARGO_TARGET_DIR=/private/tmp/claude-501/gate-care1 (kept): cargo fmt --check rc=0; cargo clippy --workspace --all-targets -- -D warnings rc=0; cargo build --workspace rc=0; test-map-check --map PASS (2400/2400, broken 0, unclassified 1112); module-map-check --map PASS (91/107, 0 unmapped/stale/bad). No integration breakage, 0 fix rounds.
PKG-DONE:7 STATUS.md: "TURN 7 (integration)" section (lanes + shas, what each delivered, 17 PARTIAL rows, T6 carry-over closed/still open).
PKG-DONE:8 commit with explicit paths (FEATURES, TEST-MAP, MODULE-MAP, CONCURRENCY, STATUS, progress-T7INT); no push, no tag, no squash.
READY:T7INT
