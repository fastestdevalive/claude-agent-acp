# G16 Progress: PC-4 + T1 Closing Documentation & Verification

Status: READY:G16
Worktree: `/private/tmp/claude-501/par-c`
Base commit: `180ace9` (branch head)
Resumed by a fresh session after a quota cut; the prior session's edits were
inspected against the diff, kept, and completed here.

## Checklist

- [x] Write progress-G16.md early (resumed; checklist below tracks both sessions)
- [x] Task 1: PC-4.1 Documentation updates
  - [x] Update `porting/PARITY.md` rows P2/P7/P8/P9/P10 and pending rows / consequences
  - [x] Grep `inv_` tests and verify every invariant family has an `INVARIANTS.md` row
  - [x] Tick finished items in `.vibekit/feature-plans/pending/parity-closeout/plan-parity-closeout.md` with commit shas
  - [x] Tick finished items in `.vibekit/feature-plans/pending/rust-parity-fixes/plan-rust-parity-fixes.md` with commit shas
  - [x] Tick finished items in `.vibekit/feature-plans/pending/rust-live-divergences/plan-rust-live-divergences.md` with commit shas
  - [x] Tick finished items in `.vibekit/feature-plans/pending/rust-air-gaps/plan-rust-air-gaps.md` with commit shas
- [x] Task 2: PC-4.2 Clean-cache reproduction
  - [x] Run `capture.sh` into `rust/target/parity/final` and check diff against `porting/fixtures`
  - [x] Document diffs or note STABLE
- [x] Task 3: T1 Closing Summary
  - [x] List T1 groups with commit SHAs from `git log --oneline` since `779835d`
  - [x] Count remaining non-DONE rows per package in `porting/parity/FEATURES.md` with reasons
- [x] Task 4: Verification
  - [x] Run `bash rust/scripts/rust-gate.sh` with `UPSTREAM_CACHE_DIR`
  - [x] Run `bash rust/scripts/parity/test-map-check`
  - [x] Run `bash rust/scripts/parity/module-map-check`
  - [x] Verify JsonPath guard: `git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'` -> 0
- [x] Final readiness check: `READY:G16`

## PC-4.1 — documentation

`porting/PARITY.md` (section F, new rows):

- P2 -> `ported with gaps`; new `pending` row "MCP / refusal-fallback elicitation (P2)"
  (consequence: refusal fallback is not triggered through ACP elicitation).
- P7 -> `ported with gaps`; new `pending` rows "ExitPlanMode details (P7)" and
  "Shell path-shortening labels in durable options (P7)" with consequences.
- P8 -> `ported with gaps`; new `pending` row "`promptSuggestions` publishing (P8)"
  (consequence: no `session_info_update._meta.promptSuggestions`).
- P9 -> `ported with gaps`; new `pending` row "Hooks gaps (P9)" (PostCompact has no
  consumer, PostModelSwitch fast-mode reconcile missing, teardown clear, spawn-time
  `options.hooks`).
- P10 -> `ported with gaps`; new `pending` row "AIR replay extras (P10)".
- New row for `_meta.claudeCode.options.permissionMode` being deliberately ignored.

`porting/INVARIANTS.md`: every `inv_NN_*` test family present in the crate has a
row. Verified by extracting the test-name prefixes from `src/` + `tests/` and the
`INV-NN` rows from the file: both sets are `1..80, 122` (54/55 have no tests and no
rows, because G9/G4 are not ported yet). Added in this change: INV-53, INV-56,
INV-60, INV-64..INV-69, INV-70..INV-78, INV-122 (INV-79/80 were already present).

Plans:

- `parity-closeout`: PC-4.1/4.2/4.3 ticked (PC-4.3 evidence below).
- `rust-parity-fixes`: 3.7/3.8 and verify 3.T1..3.T7 ticked where the git history
  and the verification commands prove them; 3.9 (final live matrix) stays with
  the orchestrator.
- `rust-live-divergences`: phases 1/2/3 implementation and their verify items
  ticked with commit shas; 1.T5..1.T7, 2.T5..2.T7, 3.T1..3.T7 are covered by the
  gate run below; 3.9 and 3.T8 stay with the orchestrator.
- `rust-air-gaps`: 3.1 goal (fa86584) and 3.4 `/mcp` (d3bc560) ticked, 3.T5
  clean-cache reproduction ticked. 3.2 (G9 session failure) and 3.3 (G4 file
  report) are NOT done (`session_failure.rs` / `file_report.rs` absent, no
  `inv_54_*` / `inv_55_*`); 3.5 is partial; 3.T3/3.T4/3.T7 cannot be met until
  those land and are left unticked with pointers.

## PC-4.2 — clean-cache reproduction

```
rm -rf rust/target/parity/final
CAPTURE_OUT_DIR="$PWD/rust/target/parity/final" \
  UPSTREAM_CACHE_DIR=/Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1/rust/target/upstream \
  bash porting/capture.sh >/dev/null 2>&1
diff -r porting/fixtures rust/target/parity/final && echo STABLE
```

Result: capture exit 0, `STABLE` printed. The shared oracle
`rust/target/upstream/0.94.0` was not touched.

## T1 closing summary

Groups landed since `779835d` (T1 commit list, newest first):

| Group | Commit | Subject |
| --- | --- | --- |
| T1-G14 | `180ace9` | LD-3 wire materialiser, numeric request ids, session titles, normaliser deletion |
| T1-G7a-fix2 | `9a38128` | reaper/settings round-2 fixes (Windows Job Object, no env override, probe handshake) |
| T1-G15 | `2682af9` | CONC-1 measured runtime decision, cancel-not-starved test, panic policy |
| T1-G7a-fix | `9e9fe1c` | reaper/settings review fixes (safe reaper, first-wins managed tiers, hermetic settings) |
| T1-G13 | `9b07233` | leftovers sweep (user chunk ids, block-stop scoping, exports, model names, tool aliases) |
| T1-G11b | `fa86584` | `_session/goal` set/clear, `/goal` interception, `active_goal`, AIR goal capability |
| T1-G7a | `f4e0c15` | F5a settings manager + managed tiers, F41 process-group reaper |
| T1-G12 | `c28eb46` | hermetic permission fixtures, flaky fixes, cancel-race/fallback-warning tests |
| T1-G11a | `d3bc560` | `/mcp` command: status, reconnect, enable/disable, prompt rewrite |
| T1-G10c | `2fe7191` | F6b review fixes (result order, nested plan stamping, TodoWrite settle) |
| T1-G6b | `7fcdb39` | F4b/FX-1 remaining spawn options |
| T1-G10b | `f198cfd` | F6a tool-call reporters catch-up |
| T1-G10a | `4947469` | F6b tool-call result bugs, parent stamping, subagent transcript |
| T1-G4c | `1203831` | F2c transport wrapper (cancel_request, numeric ids, ndjson errors) |
| T1-G9a | `61577cb` | LD-1 terminal slash commands, resume re-send, usage window, local-command ids |
| T1-G9b | `6a5b5c5` | LD-2 structured Read/Bash results, display paths, tool attribution |
| T1-G2c-fix | `504692f` | ExitPlanMode cancel race, answer-time Auto check, fallback warnings |
| T1-G7b | `2b73ced` | F5b model policy (allowlist, overrides, denied, PreModelSwitch veto, window cache) |
| T1-G5a | `eaa201f` | F2b process entry parity (`--cli`, signals, logger, session timing, managed policy) |
| T1-G2c | `6b9f98f` | ExitPlanMode tool_call/result rendering and parked corpus |
| T1-G6a | `27e8816` | F4a spawn options (ACP mcpServers, settings tier, thinking, plugins, sandbox, meta) |
| T1-G4a | `ffb7701` | F2a client capabilities, session notices, initialize authMethods |
| T1-G3 | `d058a58` | F3a SDK control layer (silence, decline, error strings, rejected replies) |
| T1-G2b | `abcf596` | session/load replay (SDK meta filter, terminal-node leaf, task plan rebuild) |
| T1-G2a | `a35bc3b` | PC-1 leftovers (AIR plan dedupe gate, Task* output parsing, hook id checks) |

Bookkeeping-only commits in the same range: `677ebca`, `724f365`, `366bc38`,
`4f02b9f`, `2b9b587`, `f345c97`, `d9aacf3`, `119e281`, `643e98f`, `7a9dba1`,
`95d8c82`, `d3abb73`, `83669e8`, `ab3b0f4`, `1457d82`, `22bbf19`, `1f231c0`
(STATUS/FEATURES count updates).

### Remaining non-DONE rows owned by T1 packages

Counts (from `porting/parity/FEATURES.md`, rows whose note carries `[pkg:X]`):

| Package | Remaining | Rows |
| --- | ---: | --- |
| F2a | 1 | C-23 |
| F2b | 2 | C-01, O-01 |
| F2c | 0 | - |
| F3a | 3 | M-38, M-39, M-42 |
| F3b | 2 | U-107, P-03 |
| F4a | 0 | - |
| F4b | 0 | - |
| F5a | 3 | O-60, O-67, Q-59 |
| F5b | 1 | O-74 |
| F6a | 7 | T-86, Q-68, Q-69, Q-70, Q-74, Q-77, Q-79 |
| F6b | 0 | - |
| LD-1 | 0 | - |
| LD-2 | 0 | - |
| LD-3 | 0 | - |
| PC-1 | 0 | - |
| PC-2 | 0 | - |
| PC-3 | 0 | - |
| AG-1 | 0 | - |
| AG-4 | 1 | S-02 |
| FX-1 | 0 | - |
| FX-3 | 0 | - |
| FX-4 | 0 | - |
| FX-5 | 0 | - |
| CONC-1 | 0 | - |
| INFRA-1 | 0 | - |
| **Total** | **22** | |

Per-row one-line reasons:
- F2a C-23 (PARTIAL): `terminal_output(_delta)` negotiated/stored, but the
  renderer terminal content/meta wiring is pending (owner: F15 terminal channel).
- F2b C-01 (PARTIAL): `agentInfo.version` is `<crate>+upstream.0.94.0`, not the
  bare `0.94.0` (deliberate, dedupe; same for O-01 `-V/--version`).
- F2b O-01 (PARTIAL): version string composition follows C-01, not Node's exact
  `0.94.0`.
- F3a M-38 (PARTIAL): the F3a slice (error verdict rejects, state untouched) is
  done; the remaining gap is outside F3a (mode-catalog/config-option surface).
- F3a M-39 (PARTIAL): error verdict rejects and the CLI message is kept; the
  remaining gap is outside F3a.
- F3a M-42 (PARTIAL): rejected `apply_flag_settings` fails and leaves state
  untouched; the remaining gap is outside F3a.
- F3b U-107 (MISSING): outbound `$/cancel_request` to the client is not emitted
  when an in-flight permission/elicitation is aborted (owner: F2c transport).
- F3b P-03 (PARTIAL): cancel aborts pending permissions, but Node also marks
  every open user input cancelled and Rust keeps no such list.
- F5a O-60 (PARTIAL): settings watcher/debounce/lifecycle landed; an active
  session does not yet consume dynamic watcher changes and managed-tier files are
  not watched.
- F5a O-67 (PARTIAL): effective settings + managed fallback are wired; the CLI
  auth probe passes no session-level override.
- F5a Q-59 (PARTIAL): SettingsManager watcher/managed tiers/helper landed; the
  session dynamic-watcher consumption is tracked under O-60.
- F5b O-74 (PARTIAL): narrowing is implemented/wired; the picker narrows only
  once `providers/set` supplies `listedModels` (owner: F33).
- F6a T-86 (PARTIAL): non-AIR display copies use reporter truthiness; the AIR
  `rawInputRendering` gate remains (owner: F15/AIR rendering).
- F6a Q-68 (PARTIAL): reporter-facts dispatch ported; the terminal channel,
  permission/plan-file rendering remain (T-33/F15, T-59/T-60).
- F6a Q-69 (PARTIAL): toolUse half complete; the toolResult trailer/hand-back/
  partial-note half is T-54 (F7a).
- F6a Q-70 (PARTIAL): shell toolUse + non-terminal result complete; terminal-only
  exit-code parse and terminal channel are T-31/T-33 (F15).
- F6a Q-74 (PARTIAL): interaction reporters complete except the ExitPlanMode AIR
  `planFile`/`planFileReleased` half (owner: F7a/AIR).
- F6a Q-77 (PARTIAL): WebFetch/WebSearch toolUse facts complete; structured
  WebSearch hits are T-51 (F7a).
- F6a Q-79 (PARTIAL): reporter merge complete; remaining `tools.ts` gaps are the
  Agent/Task result (T-54/F7a) and WebSearch hits (T-51/F7a).
- AG-4 S-02 (PARTIAL): `/mcp` status/reconnect/enable/disable ported
  (`mcp-command-status`, `mcp-command-reconnect`); the OAuth follow-up
  (`mcpAuthenticate` + auth URL) is not ported.
- LD-1/LD-2/LD-3, PC-1/PC-2/PC-3, AG-1, FX-1/FX-3/FX-4/FX-5, CONC-1, INFRA-1,
  F2c, F4a/F4b, F6b: no non-DONE rows remain.

Not-T1-owned residual packages (for the orchestrator's carry decision): AG-2 5,
AG-3 9, CONC-2 3, F10 18, F11 25, F12 2, F13 4, F14 13, F15 15, F16a 8,
F16b 3, F16c 4, F17 9, F18 6, F19 2, F20 18, F21 11, F22 3, F23a 6, F23b 6,
F23c 2, F24 21, F25 6, F26a 13, F26b 20, F27a 4, F27b 6, F28 4, F29 9,
F30 9, F31 12, F32 11, F33 8, F34 20, F35a 13, F35b 8, F35c 6, F36 7,
F37 12, F38a 14, F38b 7, F39 11, F40 15, F7a 11, F7b 8, F8 4, F9 17,
FX-2 11, INFRA-2 1, INFRA-3 1.

## Verification

All commands run in `/private/tmp/claude-501/par-c` with
`UPSTREAM_CACHE_DIR=/Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1/rust/target/upstream`
(read-only oracle; never deleted).

- `bash rust/scripts/rust-gate.sh` -> exit 0, tail:
  `TEST-MAP CHECK: PASS`, `MODULE-MAP CHECK: PASS`, `PARITY: PASS`,
  `==> upstream ahead (non-failing)`, `==> gate OK`. The log holds 25
  `test result: ok` lines and no `FAILED`.
- Polluted run (the plans' prefix sets these variables: `CLAUDE_CODE_ARTIFACT`,
  `ANTHROPIC_MODEL`, `CLAUDE_AGENT_SDK_VERSION`, `CLAUDE_CODE_ENTRYPOINT`,
  `CLAUDE_CODE_EXECUTABLE`, `CLAUDE_CODE_MESSAGING_SOCKET`, `IS_SANDBOX`,
  `NO_BROWSER`, `SSH_CONNECTION`, `MAX_THINKING_TOKENS`, `CLAUDE_CONFIG_DIR`
  — names only) -> see "Polluted gate" below.
- `bash rust/scripts/parity/test-map-check` -> last line
  `TEST-MAP CHECK: PASS` (2400/2400 mapped, 0 unmapped/stale/broken).
- `bash rust/scripts/parity/module-map-check` -> last line
  `MODULE-MAP CHECK: PASS` (91 modules, 107 rows, 0 unmapped/stale/bad).
- JsonPath guard
  `git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'`
  -> `0`.
- G7 guard `git diff --stat -- . ':!rust' ':!porting' ':!skills' ':!.vibekit' ':!.gitignore'`
  -> empty.
- `git diff 8a28082 --stat -- rust/claude-agent-acp-rs/src/turn.rs rust/claude-agent-acp-rs/src/turn/turn_tests.rs`
  -> empty (hazardous files untouched).
- Differential fixture greps (3.T3/3.T4): `locations":[]` 4,
  `oldText":null` 3, permission ids 0 and 1 present, one
  `session_info_update` in `session-title`; normaliser symbols count 0 in both
  `tests/common/mod.rs` and `tests/differential.rs`.

### Polluted gate

The same `bash rust/scripts/rust-gate.sh` under the polluted prefix -> exit 0,
tail `PARITY: PASS`, `==> gate OK`; 25 `test result: ok` lines, no `FAILED`.

### Differential 3x

`cargo test ... --test differential` run three times -> `test result: ok. 95
passed; 0 failed` each time (27.12s / 26.91s / 26.88s).

### FEATURES header note

The header counts in `porting/parity/FEATURES.md` (`DONE 429, PARTIAL 168,
MISSING 316`) are stale by three rows relative to the row statuses measured
here (`DONE 433, PARTIAL 165, MISSING 316`, 913 rows). Per the lane rules the
header was not edited; the orchestrator recomputes counts.

## Acceptance

- Docs edits are limited to `porting/PARITY.md`, `porting/INVARIANTS.md`, the
  four pending plans under `.vibekit/feature-plans/pending/`, and the new
  `porting/parity/progress-G16.md` (shown by `git status --short`); no `rust/`
  source file was touched.
- Clean-cache reproduction printed `STABLE`.
- Plain and polluted gates: `==> gate OK`.
- `test-map-check` and `module-map-check`: `PASS`.
- JsonPath guard: `0`; G7 diff: empty.

READY:G16
