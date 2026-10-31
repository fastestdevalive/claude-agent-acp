# Parity program status — claude port (rust/claude-agent-acp-rs)

Pinned counterpart: `@agentclientprotocol/claude-agent-acp` **0.94.0** (`porting/UPSTREAM_VERSION`).
Upstream source for module/test mapping: tag `v0.94.0` of `agentclientprotocol/claude-agent-acp`
(read from the `upstream` git remote of this repo into a scratch export outside the repo; the
`~/code/fastestdevalive/claude-agent-acp-original` clone is at v0.78.0 and is NOT touched).

## Upstream reference checkouts (READ THESE for every module you port)

The local clone `~/code/fastestdevalive/claude-agent-acp-original` (checked-out HEAD left at its old
`becd854` / v0.78.0 — untouched, working tree clean) was fetched (`git fetch --all --tags --prune`) and two
`git worktree` checkouts were created OUTSIDE both repos, under
`/private/tmp/claude-501/-Users-gb--vibe-station-projects-claude-acp-rust-eval-worktrees-care-1/5645c96c-b7c3-4262-abc5-87d07ff3c56c/scratchpad/upstream-wt/`:

| Purpose | Path (under `upstream-wt/`) | Ref / sha |
|---|---|---|
| **Pinned parity source of truth** — every FEATURES / TEST-MAP / MODULE-MAP row refers to this tree: read `src/*.ts` and `src/tests/*.test.ts` | `v0.94.0/` | tag `v0.94.0` = `7bcbfc7a5ca7ab9c7eece4f0749ccaf49c7ba7e1` |
| Information only (next bump) | `main/` | `origin/main` = `59e292e29f773cc6f03c0999993cbe8a2d97a7e3` (2026-10-23; its package.json still says 0.94.0) |

Compiled reference for runtime behaviour: `rust/target/upstream/0.94.0/node_modules/@agentclientprotocol/claude-agent-acp/dist`
(read-only; never delete). Haiku lead/subagent prompts must name the exact upstream `src/*.ts` (and the matching
`src/tests/*.test.ts`) they read before writing Rust.

The mirror pin is NOT bumped (stays 0.94.0). Latest stable upstream tag is `v0.94.0`; newer tags are
`v0.94.1-preview.1..9` only.

### Delta note: 0.94.0 → upstream main (what the next bump brings; not ported, not in scope now)

9 commits, 39 files (+3655/-315): `feat: serve experimental ACP v2` (4d378f9), `fix: keep a session running
while its background tasks are open` (e9f986b, IJAI-3744), `fix: publish turns Claude Code starts after end_turn
as an async task in v1` (a1d0eee), `feat: show messages typed in other Remote Control clients` (5b11f47, new
`task-notifications.ts`), `feat: end a turn at its result for AIR clients with backgroundSubagents` (9b3a5c8),
`fix: publish the session title as soon as the CLI has it` (50cd76e), `feat: report RFD delegations as linked
session messages` (59e292e). Changed source: acp-agent.ts, acp-subagents.ts, air-extension.ts, async-tasks.ts,
clear-context-coordinator.ts, index.ts, native-subagents.ts, permissions/options/{shared,tools}.ts, serve.ts,
session-index/{activity,service,session-index,transcript-scan}.ts, session-titles.ts, subagent-replay.ts;
added: session-index/background-tasks.ts, task-notifications.ts. Tests grew: background-tasks (+587),
remote-user-messages (+205), native-subagents (+247), session-titles, acp-v2, incomplete-tools. Run
`bash rust/scripts/upstream-diff.sh 0.94.0 <next>` at bump time.

Location note: the cross-repo brief names `docs/parity/*` and `scripts/parity/*`. Root `docs/` and
`scripts/` are upstream-owned (guard G7), so this repo keeps the same file names under
`porting/parity/` and `rust/scripts/parity/` — see `porting/parity/DECISIONS-FOR-USER.md` D1.

## Done criteria (all five must be true)

1. [ ] Every divergence vst's live tests found is fixed (reports in
       `/Users/gb/.vibe-station/projects/vibe-station/worktrees/vs-275/.vibekit/reports/`).
   - [x] P1 spawn contract (betas / extraArgs / env) — 4a54919
   - [x] P2 argv (`--disallowedTools=AskUserQuestion`, `--permission-mode`, `--allow-dangerously-skip-permissions`) — 4a54919
   - [x] P3 image prompt echo — 4a54919
   - [x] P5 empty thought chunks — 4a54919
   - [x] P4 `usage_update` (+ context-window seeding, authoritative-window rule, 1M model rows) — parity Phase 2 (gaps recorded as `pending` in PARITY.md: cross-session window cache, background refresh after switch if not ported)
   - [x] P6 modes / configOptions / set_config_option / set_mode / config_option_update / effort / `options.resume` — parity Phase 2 (`session/set_model` stays -32601 exactly like Node 0.94.0; fingerprint-mismatch recreate recorded `pending`)
   - [ ] P7 Edit/Write `allow_always` options — parity Phase 3
   - [ ] P8 `availableCommands[].input` (done 4a54919), hooks/promptSuggestions fields — parity Phase 3
   - [ ] P9 PostToolUse hook, structuredPatch diff, `_meta.claudeCode.toolResponse` — parity Phase 3
   - [ ] P10 `session/load` history replay — parity Phase 3
   - [ ] vs-275 recheck report for claude (none yet in the reports dir)
2. [ ] Every item in PARITY.md / INVARIANTS.md marked pending / not ported / GAP is ported
       (only technically impossible exceptions, each with a written reason in DECISIONS-FOR-USER.md).
   - [ ] command-lifecycle orphan map (`msg_lifecycle_v1`)
   - [ ] `endedPerLevel` sweep
   - [ ] `suppressControlResponse`
   - [ ] grandchildren of a killed host
   - [ ] process dying mid-steer stop reason
   - [ ] ACP elicitation + `AskUserQuestion` via form elicitation, MCP elicitation
   - [ ] goal extension, session-failure extension, recommended-config-values extension, permission extension
   - [ ] native subagent sessions, AIR asyncTasks / air-tool-meta (partly done) / file-change-audit /
         session-titles / fork-session (done) / exit-plan / context-compaction / clear-context-coordinator /
         hide-claude-auth / usage-markdown
   - [ ] providers/* and AIR session/delete + fork-at-message
   - [ ] cancel family, stamped-result lanes, compaction, per-model quota edge cases
3. [ ] Parity + concurrency programs complete (TEST-MAP zero `missing`, MODULE-MAP no unmapped upstream
       module, all shared corpus scenarios recorded + replayed, live smoke passes, concurrency decision + race suite).
4. [ ] Independent Opus audit round finds nothing real (fresh Opus, given only the upstream tag + this repo).
5. [ ] vibe-station verifier (vs-275) re-runs the live matrix against the candidate head: zero open divergences.

## Open-item count

Open: see checklist above (updated every commit). Open work packages: parity Phase 2 (verify + commit),
parity Phase 3, parity-infra (TEST-MAP / MODULE-MAP / scripts), corpus scenarios, concurrency program,
feature packages F1..Fn (derived from MODULE-MAP), audit rounds.

## Current work package

T1-G2a / PC-1 leftovers — AIR plan dedupe and Task* parsing/hooks
(feature rows U-26, T-22, T-23, T-25, T-77; PC-1.2 remainder).

Delivered in this package:
- U-26: `publish_task_plan` gates the plan dedupe on the AIR client itself
  (`_meta.jetbrains.air` is an object), not on an `air` capability list;
  non-AIR clients get every plan (`session.rs`; test
  `air_plan_dedupe_uses_meta_presence_not_the_capabilities_list`).
- T-25: `apply_task_create` uses only the output task id; the input `taskId`
  fallback is gone (`hooks.rs`; test `pc1_5_task_create_needs_an_output_id`).
- T-77: TaskCreated rejects an empty `task_id` or `task_subject`; TaskCompleted
  rejects an empty `task_id` (`session.rs`; test
  `task_created_hook_rejects_empty_id_or_subject`).
- T-23: TaskCreate parsing accepts structured JSON (object, JSON string, text
  blocks) and the text form with Node's `\S+`/`(.+)` checks; TaskUpdate and
  TaskList forms audited (JSON string, structured empty list, failed delete,
  not-found); tests in `hooks.rs`.
- T-22: TaskGet stays silent and TaskList replaces the snapshot in output
  order; permission-surfaced Task* settle clearing was already fixed in
  5c977c3 (`inv_24_task_perm_settle` passes).
- PC-1.2 remainder: `HookRegistry::clear` was already removed as dead code; no
  tests referenced it. The Node teardown `clearHookCallbacks` wiring remains
  F9/F18.

FEATURES.md: U-26, T-22, T-23, T-25 and T-77 flipped to DONE (counts DONE 269,
PARTIAL 241, MISSING 403). Closeout plan: PC-1.5 ticked; PC-1.1/PC-1.2 keep
their honest open notes.

Verification (T1-G2a): `cargo test -p claude-agent-acp-rs --lib` 300 passed;
`cargo test --test differential` 58 passed 1 ignored; clippy
`--workspace --all-targets -D warnings` clean; `cargo fmt --check` clean;
`rust-gate.sh` ends `==> gate OK`; the JsonPath guard prints 0.

### T1-G1a (previous package, INFRA-1)

parity-infra Phase 1 machinery (plan
`.vibekit/feature-plans/pending/parity-infra/plan-parity-infra.md`, items 1.1-1.9,
1.11). G0 (fmt-clean the PC-1 WIP) is done; PC-1.5 settle fix and the
`todo-perm-settle` repro stay uncommitted as before.

Delivered in that package:
- `rust/scripts/parity/{fetch-upstream.sh,scan_tests.awk,test-map-check,module-map-check,gate-parity.sh}`
  (+ `selftest/fixture.test.ts`), wired into `rust/scripts/rust-gate.sh` as
  `==> parity maps (TEST-MAP + MODULE-MAP)`.
- Committed snapshots `porting/parity/upstream-tests.v0.94.0.tsv` (2,400 rows)
  and `upstream-modules.v0.94.0.tsv` (91 rows).
- `porting/parity/TEST-MAP.md` seeded: 2,400 cases, all
  `missing:T-UNCLASSIFIED` (2,336 `it`/`test` sites in 58 files + 24 in
  `scripts/next-preview-version.test.mjs` + 40 scenario rows).
- `porting/parity/MODULE-MAP.md`: 91 modules + 17 `acp-agent.ts@` sections =
  107 rows; `missing:` rows use the FEATURES roadmap ids.
- `porting/parity/MISSING.md`, `porting/parity/README.md`,
  `porting/parity/scan-allowlist.tsv` (4 loop-generated titles).
- `rust/claude-agent-acp-rs/tests/parity_maps.rs`:
  `inv_70_parity_selftests`, `inv_71_parity_maps_pass_offline`,
  `inv_72_unmapped_test_fails`, `inv_73_unmapped_module_fails`,
  `inv_74_broken_ported_target_fails`.

Verification (T1-G1a): both `--selftest` print `SELFTEST: PASS`;
`test-map-check --reconcile` prints `RECONCILE: PASS` against the 0.94.0 export;
`test-map-check --map` and `module-map-check --map` PASS (snapshot + export
halves); `cargo test --test parity_maps` ok; `rust-gate.sh` ends `==> gate OK`.
Gate flip to `--forbid-unclassified` stays for Phase 3 (Decision 7).

Lessons kept: gate from a non-UUID path without CARGO_TARGET_DIR; inline env
prefixes only.

`claude-master` (plan `.vibekit/feature-plans/pending/claude-master/plan-claude-master.md`), stage 1: FEAT-1 done (12 read-only audits, in-harness). FEAT-2 done: `porting/parity/FEATURES.md` has 10 sections, 912 audit rows (274 DONE, 235 PARTIAL, 403 MISSING incl. 91 module rows; round 6), 20 dedupe pairs merged, every non-DONE row tagged `[pkg:]`, 5 FX packages (K6) and 13 numbered wire bugs (WB-01..WB-14, no WB-06; WB-08 has sub-items a-g) with owners and acceptance tests. The FEATURES-AUDIT checkpoint runs in rounds (round 6 pending the next Opus audit).
Open-item count: 649 non-DONE rows in FEATURES.md (247 PARTIAL + 402 MISSING), counting the 91 module rows (round 9: A-21, B-33 implemented in round 8; T-24 DONE and R-03 PARTIAL in PC-1; the checklist is accepted, so further changes go with the row they touch).

Gate lessons (kept as gate rules):
- `acp-recorder/tests/recorder.rs` hardcodes `../target/debug/examples/echo_agent`: a custom `CARGO_TARGET_DIR` breaks `inv_24_recorder_initialize_order` (ENOENT). Detached and lead gates must NOT set `CARGO_TARGET_DIR`; they use the worktree's own `rust/target`.
- Detached gates must run from a path with no UUID-like segment (e.g. `/private/tmp/claude-501/gate-<name>`), else `inv_57` and `inv_24` fail spuriously.
- Gate env prefixes must be written inline (`env VAR=... cargo test ...`), never via a shell variable: zsh does not word-split `$P`, so the polluted run silently never executes.
Baseline (B0, plain + polluted gate green): 398 workspace tests passed, 0 failed.

Previous work package (superseded): `rust-parity-fixes` Phase 3 (hooks + PostToolUse/toolResponse, Task plan, `session/load` replay,
`allow_always` permission options, docs) → then `parity-infra` (committed plan 3f33c41: TEST-MAP,
MODULE-MAP, check scripts, bump workflow) → then the feature packages of
`.vibekit/feature-plans/pending/parity-roadmap/ROADMAP.md` (62 packages F1..F46 + sub-packages; honest
estimate ~250 turns including retries/reviews — this is a multi-week programme, worked in the roadmap's
order; hazardous turn.rs group strictly sequenced).

## Last commit

parity Phase 2 commit (see `git log`; previous: 3f33c41 parity-infra plan, 4a54919 parity Phase 1).

Hermeticity lesson (kept as a gate rule): all tests must pass with a polluted host env
(`CLAUDE_*`, `ANTHROPIC_*` exported); the adapter reads the environment through `env_snapshot.rs`.

## Model log (who did which turn)

| Work | Model |
|---|---|
| Plans (all), orchestration | Sonnet (this session) |
| Plan / diff reviews (all) | Opus |
| steering lanes phases 1-3 | deepseek (phase 1: turn.rs), Sonnet in-harness (phases 2-3) |
| upstream sync phases 1-2 | deepseek |
| upstream sync phase 3 | deepseek (started), Sonnet in-harness (finished, user-directed switch) |
| air-gaps phase 1, phase 2 | Sonnet in-harness (user-directed) |
| parity phase 1 | deepseek (items 1.0-1.1), ds-4.1-free (1.2-1.8), agy-medium (1.9-1.10, follow-ups) — user-directed mode switches |
| parity phase 2 | agy-medium (2.0-2.4), ds-4.1-free (no progress, free limit hit), deepseek (2.5-2.10), agy-medium (set_model test fix; Opus follow-ups; env-snapshot hermeticity + round-2 majors) — user-directed: agy-medium preferred, deepseek only as fallback |
| plans parity-infra, parity-roadmap | Sonnet planners (in-harness), Opus reviews (2 rounds for parity-infra; 1 round + revision for the roadmap) |

| parity phase 3 | agy-medium (read-only start, quota hit before any edit); deepseek: turn 3a (hooks, promptSuggestions frames, Task plan), turn 3b (session/load replay) + tiny clippy-fix turn, item 3.6 (permission option sets); deepseek F1 (ExitPlanMode blocker + offered-option guard) in flight |
| live matrix (orchestrator) | Sonnet ran the vs-275 driver on both adapters; Sonnet analysis agent compared transcripts (porting/parity/LIVE-FINDINGS-2026-10-23.md) |

MODEL RECIPE (user change, claude port only): from the next implementation turn after the running deepseek
turn finishes, implementation turns go to a HAIKU subagent in TERMINAL mode
(`mode-1789474690627-p55ag`, `--channel=tmux`), narrow file-scoped prompts with a one-line DONE/BLOCKED
summary; a turn that fails twice escalates to agy-medium (`mode-1784608587031-sd4sy`, while its limits
allow) or to the orchestrator. Sonnet plans/orchestrates, Opus reviews every plan and risky diff, the
orchestrator runs the gate before each commit. Turns done by haiku-terminal are logged below.

| Turn | Model |
|---|---|
| claude-master B0 (baseline gate) | haiku-terminal lead (Haiku 5.5, master run), 0 subagents |
| claude-master FEAT-1 (12 read-only FEATURES audit groups G1..G11, G2a/G2b) | haiku-terminal lead; first attempt on 12 vst haiku-terminal subagents (`mode-1789474690627-p55ag`) terminated on user request; re-run as 12 in-harness general-purpose subagents |

MODEL RECIPE v2 (user change, supersedes the haiku/agy recipe above): EVERY implementation turn (hazard and Opus-review fixes
included) is a deepseek vst subagent (`mode-1792593073043-0d981`); haiku leads and agy-medium are dropped; the Sonnet
orchestrator implements nothing (spawns, gates, commits, Opus-reviews). Five turns T1..T5 per `plan-claude-master.md`
"RE-PLAN v2". A group that fails twice on deepseek (first attempt + one fresh respawn) stops the work and goes to the user.

| Turn / group | Subagent (deepseek) |
|---|---|
| haiku lead (stages 0-2: B0, FEATURES audit rounds 1-7, PC-1 first commits, A-21/B-33) | haiku-terminal lead `care-1-a-9273ce87` (terminated on the recipe change); PC-1.5 settle fix implemented by the Sonnet orchestrator BEFORE the recipe change |
| T1-G0 (fmt-clean the PC-1 WIP) | `care-1-a-1b762e58` (reused session; terminated) |
| T1-G1a (INFRA-1 TEST-MAP/MODULE-MAP machinery) | `care-1-a-1b762e58` (same reused session; terminated) |
| T1-G1b (F1 recorder/fake-claude) attempt 1 | `care-1-a-1b762e58` (same reused session, stopped mid-group by the user's correction; edits kept) |
| T1-G1b resume 1 | `care-1-a-239879fa` (FRESH session; STALLED: printed tool calls as text for the sdk.mjs extraction, terminated; failure 1 of 2 for G1b) |
| T1-G1b resume 2 | `care-1-a-96b28b01` (FRESH session, completed G1b; commit 779835d) |
| T1-G2a (PC-1 leftovers: U-26, T-22, T-23, T-25, T-77, HookRegistry::clear) | `care-1-a-a231c81a` (fresh; terminated after READY) |
| T1-G2b (PC-2 session/load replay) lane A, main tree | `care-1-a-e7d0b681` (fresh) |
| T1-G3 (F3a SDK control layer) lane B, isolated worktree `/private/tmp/claude-501/par-b` | `care-1-a-dc4ad8d8` (fresh; terminated after READY; committed on the branch as d058a58 via cherry-pick of par-b 7def054, integrated gate OK) |
| T1-G4a (F2a client caps, notices, authMethods) lane B | `care-1-a-9cd9e8d5` (fresh; terminated after READY; committed ffb7701 via cherry-pick of par-b 3612c7c; one trivial merge conflict in tests/differential.rs resolved by keeping both appended tests) |
| T1-G2c (PC-3 ExitPlanMode rendering + parked corpus) lane B, worktree par-b at ffb7701 | `care-1-a-4f15cdf6` (fresh; terminated after READY; committed 6b9f98f via cherry-pick of par-b e7c2cab, clean; diff also touched session.rs outside its stated scope -> Opus review) |
| T1-G7b (F5b model policy) lane B, par-b at 6b9f98f attempt 1 | `care-1-a-e13a31f7` (fresh; STALLED: no tool call, nothing written; terminated; failure 1 of 2) |
| T1-G7b respawn | `care-1-a-9b0be772` (fresh; died in seconds with provider.transport ConnectionRefused = infrastructure error, not a model failure; terminated) |
| T1-G7b respawn 2 | `care-1-a-3377a36b` (fresh; same ConnectionRefused; terminated) |
| T1-G2b (PC-2 replay) | `care-1-a-e7d0b681` (fresh; terminated after READY; committed abcf596; first gate hit the flaky test-map-check selftest, rerun OK) |
| T1-G6a (F4a spawn options) lane A, worktree `/private/tmp/claude-501/par-c` attempt 1 | `care-1-a-1e73bf10` (fresh; STALLED: announced a file write and ended its turn with no tool call; terminated; failure 1 of 2 for G6a) |
| T1-G6a respawn (bad prompt, terminated unused) | `care-1-a-85f11554` (orchestrator prompt-assembly error, no work done, not counted as a failure) |
| T1-G6a respawn 2 | `care-1-a-5ad06e4a` (fresh; completed G6a; terminated; committed 27e8816 via cherry-pick of par-c 98da88d, applied cleanly) |
| T1-G5a (F2b process entry) lane A, worktree par-c at 27e8816 attempt 1 | `care-1-a-14f4973d` (fresh; STALLED: wrote logger.rs/session_timing.rs and 9 edits then went idle with nothing ticked; terminated; failure 1 of 2) |
| T1-G5a resume | `care-1-a-1247ce27` (fresh; died in seconds with provider.transport ConnectionRefused = infrastructure error; terminated) |
| T1-G5a resume 2 | `care-1-a-f9ca6ba0` (fresh; same ConnectionRefused; terminated; WIP stays in /private/tmp/claude-501/par-c) |

RULE (user correction): ONE FRESH deepseek subagent per commit unit (group), spawned for that group only and terminated after its result is consumed; never send a follow-up group to an old session; a stall = fresh respawn resuming at the first unticked checkpoint. Each group row above lists its own subagent id from here on.

PARALLELISM (user decision): max 2 deepseek sessions at once, each in its own isolated git worktree (lane A = main tree, lane B = `/private/tmp/claude-501/par-b`-style detached worktree), file-disjoint groups only; the orchestrator merges the lane diff (`git apply --3way`), recomputes FEATURES header counts, gates once and commits one group at a time.

KNOWN FLAKE (to fix in a small deepseek group): `rust/scripts/parity/test-map-check --selftest` `check_fail` uses `printf '%s\n' "$outf" | grep -q "$marker"` under `set -o pipefail`; grep -q can close the pipe early so printf gets SIGPIPE and the pipeline fails at random (seen once in the G2b gate). Replace with a here-string/`case` match; check module-map-check and gate-parity.sh for the same pattern.

PENDING FIX (Opus review of G2c, report scratchpad/review-G2c.md): G2c-fix = (1) drop the `ack` oneshot on `Command::ExitPlanModeApplied` or move reply+response into the command so the run loop publishes and replies in one step (cancel race: abort of the pending permission task while the mode updates already went out); (2) T-59 -> PARTIAL (plan-file release renderer.ts:503-516 not ported); (3) decide the model-lacks-Auto check at answer time (acp-agent.ts:10219) not at request time; (4) emit the fallback warning for every auto->acceptEdits rewrite, not only ExitPlanMode. Queued until a deepseek slot frees.

PAUSED (infrastructure): the deepseek vst mode's backend `http://10.0.0.192:8888/v1` (DeepSeek Local / TensorFold, from ~/.config/opencode/opencode.jsonc) refuses connections (host pings, port 8888 closed) -> every deepseek session dies at its first model call with provider.transport ConnectionRefused. No model switch (user rule). Work resumes when the endpoint answers. In-flight state: branch head 6b9f98f+docs (all committed groups gated OK); par-c holds uncommitted G5a WIP (F2b: logger.rs, session_timing.rs, edits in 9 files, progress-G5a.md); par-b clean at 6b9f98f; queued: G7b (F5b), G2c-fix (cancel race, T-59 PARTIAL), flaky test-map-check selftest fix.

RESUMED after the deepseek backend returned (http 200 stable): T1-G7b (F5b) -> `care-1-a-4c876eb9`, T1-G5a (F2b resume in par-c) -> `care-1-a-74c8161b` (both fresh).

T1-G5a (F2b) DONE: session `care-1-a-74c8161b` (resume 2), committed eaa201f via cherry-pick of par-c 3f6a921 (clean). T1-G2c-fix (Opus review fixes) lane A in par-c at eaa201f -> `care-1-a-d9302e44` (fresh).
T1-G7b (F5b) DONE: session `care-1-a-4c876eb9`, committed 2b73ced via cherry-pick of par-b d3e388c (conflict in settings.rs tests module: both groups appended adjacent tests; resolved by keeping both, settings:: tests 9 passed). T1-G4c (F2c transport wrapper, error shapes) lane B in par-b at 2b73ced -> `care-1-a-d15c927a` (fresh).
T1-G2c-fix DONE: session `care-1-a-d9302e44`, committed 504692f via cherry-pick of par-c 927506e (clean); gate + Opus review pending. T1-G9b (LD-2 tool-call content/attribution) lane A in par-c at 504692f -> `care-1-a-1772d47c` (fresh).
Opus review of T1-G2c-fix (504692f): ACCEPT. Open low items for a later test-strengthening group: (1) the cancel-race test aborts finished tasks and cannot fail at runtime -> add a run-loop test (answer, then Cancel, assert mode/state/CLI allow); (2) add a session-level test that an Auto->acceptEdits rewrite on a non-ExitPlanMode tool publishes the warning once. FEATURES counts fixed to DONE 343, PARTIAL 211, MISSING 359 (T-59 PARTIAL).
T1-G9b (LD-2) DONE: session `care-1-a-1772d47c` (READY:G9b with full verification, cut off by an orchestrator-session restart right after), committed 6a5b5c5 via cherry-pick of par-c b05bf84 (clean). T1-G4c (F2c): first session `care-1-a-d15c927a` cut off by the same restart (infrastructure, not a model failure; WIP kept in par-b) -> resumed in `care-1-a-0a7c1b43` (fresh).
T1-G9a (LD-1 live divergences part 1) lane A in par-c at 6a5b5c5 -> `care-1-a-465431cc` (fresh).
T1-G9a (LD-1) DONE: session `care-1-a-465431cc`, committed 61577cb via cherry-pick of par-c 90bb82c (clean). T1-G10a (F6b tool-call result bugs/parent stamping) lane A in par-c at 61577cb -> `care-1-a-e80d16af` (fresh).
T1-G4c (F2c) DONE: sessions `care-1-a-d15c927a` (cut off) + `care-1-a-0a7c1b43` (resume), committed 1203831 via cherry-pick of par-b 6510c53 (clean; adds futures dep, agent-client-protocol stays 2.1.0). T1-G10a (F6b) DONE: `care-1-a-e80d16af`, committed 4947469 via cherry-pick of par-c df37450 (clean); gate + Opus review pending. T1-G6b (F4b + FX-1) lane B in par-b at 4947469 -> `care-1-a-bec359c6`; T1-G10b (F6a reporters) lane A in par-c at 4947469 -> `care-1-a-b826c4e5` (both fresh).
Gate OK for 4947469 (G4c + G10a). Opus review of G10a (report scratchpad/review-G10a.md): core sound (inv_24_todo_perm_settle really passes; fixtures byte-identical to Node recaptures). QUEUED group G10c (fixes, fresh deepseek when a lane frees): (1) emit the TodoWrite/Task* resolution in block order with the frame's other results (session.rs ~2608-2651 vs acp-agent.ts:13232); (2) C-42 forwardSubagentText:true to the CLI from the capability + the _meta.claudeCode.options.forwardSubagentText option (overlaps G6b: check it first); (3) stamp parentToolUseId on plans from nested TodoWrite/Task* (map.rs ~849-852, session.rs ~2164); (4) T-15/T-16 -> PARTIAL (memories not read outside tests; upstream reads them on cancel/async task lookups) ; (5) un-ignore inv_24_todo_perm_settle, remove todo-perm-settle from HAZARD_PENDING (differential.rs ~291), flip T-21 DONE, T-09 evidence; TEST-MAP rows for the TodoWrite resolution should point at a real resolution test; (6) AIR stream frame without parent_tool_use_id is nested in Node (map.rs ~1493).
T1-G10b (F6a) DONE: `care-1-a-b826c4e5`, committed f198cfd via cherry-pick of par-c e4264f1 (clean); gate pending. T1-G10c (F6b review fixes + un-ignore inv_24_todo_perm_settle) lane A in par-c at f198cfd -> `care-1-a-7b05f41f` (fresh).
T1-G6b (F4b+FX-1) DONE: `care-1-a-bec359c6`, committed 7fcdb39 via cherry-pick of par-b 8b31340 (mechanical conflict at end of tests/differential.rs resolved by keeping both tests; gate pending). T1-G11a (AG-4 /mcp command) lane B in par-b at 7fcdb39 -> `care-1-a-161a33ac` (fresh).
T1-G10c DONE: `care-1-a-7b05f41f`, committed 2fe7191 via cherry-pick of par-c 391bb8d (clean; gate pending); inv_24_todo_perm_settle un-ignored, T-21 DONE. T1-G12 (F42 hermetic fixtures + flake A test-map-check pipefail + flake B sdkctl-duplicate-request + two test-strength items from the G2c-fix review) lane A in par-c at 2fe7191 -> `care-1-a-1c7c51a0` (fresh).

USER OVERRIDE (chat, 2026-10-24): agy-medium (`mode-1784608587031-sd4sy`, limits reset) may be used again for one lane; deepseek stays on the other. T1-G12 deepseek session `care-1-a-1c7c51a0` (no file edit for 90 min, edits kept in par-c) terminated and continued on agy-medium `care-1-a-bdabb96d` (tmux). T1-G11a stays on deepseek `care-1-a-161a33ac`.
T1-G11a (AG-4 /mcp) DONE: `care-1-a-161a33ac`, committed d3bc560 via cherry-pick of par-b 580aa9e (clean; gate pending). Hazard residue for T2: a /mcp result-only prompt whose result beats the control run keeps the CLI text (Node holds the turn; needs a settle deferral) -> TEST-MAP `different:` row. T1-G11b (AG-1 _session/goal) lane B in par-b at d3bc560 -> `care-1-a-0d2769c3` (deepseek, fresh).
T1-G12 DONE: deepseek `care-1-a-1c7c51a0` (stalled, 3h) then agy-medium `care-1-a-bdabb96d` (finished in ~35 min); committed c28eb46 via cherry-pick of par-c 85081a0 (clean; session.rs change verified test-only; gate pending). T1-G7a (F5a settings manager/managed tiers + F41 process-group reaper; platform cfg only in process.rs) lane A in par-c at c28eb46 -> agy-medium `care-1-a-0ce9ce3c` (tmux).
T1-G7a DONE: agy-medium `care-1-a-0ce9ce3c`, committed f4e0c15 via cherry-pick of par-c 613fcf4 (clean; gate + Opus review of the reaper pending). T1-G13 (T1 leftovers sweep: F6a 10, F3a 3, F2a 1, F2b 2, F5b 1, AG-4 1, LD-3 3, FX-3 2, FX-4 3, FX-5 1; F3b + version rows excluded) lane A in par-c at f4e0c15 -> agy-medium `care-1-a-821e3fc0` (tmux).
Gate OK for f4e0c15 (G7a) BUT Opus review (scratchpad/review-G7a.md): CHANGES-NEEDED, 17 findings (7 high): reaper re-exec fallback could recurse (in_process), kill(pid,0) zombie/pid-reuse host detection, stdin-EOF path dead, PR_SET_PDEATHSIG/Job Object claimed but absent, managed tiers merged instead of first-wins, settings tests no longer hermetic, per-session SettingsManager leak. Queued fix group G7a-fix (prompt t1-g7afix.md) -> next free lane. Do NOT treat F41/F5a rows as final until it lands.
T1-G11b (AG-1 _session/goal) DONE: deepseek `care-1-a-0d2769c3`, committed fa86584 via cherry-pick of par-b f6c1586 (clean; gate pending). T1-G7a-fix (17 Opus findings on the reaper/settings manager) lane B in par-b at fa86584 -> agy-medium `care-1-a-eab8ceab` (tmux).
T1-G13 DONE: agy-medium `care-1-a-821e3fc0`, committed 9b07233 (conflict at end of tests/differential.rs resolved by keeping both). T1-G7a-fix DONE: agy-medium `care-1-a-eab8ceab`, committed 9e9fe1c (FEATURES.md Q-19/Q-20 row conflict resolved: G13 Q-19 DONE + fix's Q-20 text); gate + second Opus review pending. T1-G14 (LD-3 wire materialiser, normaliser deletion, session titles first cut) lane A in par-c at 9e9fe1c -> agy-medium `care-1-a-ec9e8e3f`; T1-G15 (CONC-1 concurrency decision + panic policy) lane B in par-b at 9e9fe1c -> agy-medium `care-1-a-1f6c243c` (both tmux).
Opus round-2 review of 9e9fe1c (scratchpad/review-G7afix.md): CHANGES-NEEDED, 9 findings (13/17 originals fixed; unix reaper safe; PR_SET_PDEATHSIG correctly NOT implemented - parent-thread semantics): remove false Job Object/PDEATHSIG claims and IMPLEMENT the Windows Job Object; remove the non-upstream CLAUDE_MANAGED_SETTINGS_PATH env override; managed file read for from_pairs embedders; follow the SDK on unreadable MDM/HKLM; spawn_blocking for plutil/reg; positive reaper binary identification; FEATURES summary line; strict debounce test; ACP_REAPER_BIN test. Queued group G7a-fix2 (prompt t1-g7afix2-body.md).
T1-G15 (CONC-1) DONE: agy-medium `care-1-a-1f6c243c`, committed 2682af9 via cherry-pick of par-b d055b2e (clean; gate pending): multi_thread kept by measurement, inv_74 cancel-not-starved, panic=unwind, CONC-2 gets the session-supervisor/registry cleanup. T1-G7a-fix2 (9 findings of the round-2 review) lane B in par-b at 2682af9 -> agy-medium `care-1-a-bf6817ea` (tmux).
T1-G7a-fix2 DONE: agy-medium `care-1-a-bf6817ea`, committed 9a38128 via cherry-pick of par-b 317e13d (clean, gate OK). Opus round-3 (scratchpad/review-G7afix2.md): CHANGES-NEEDED, 8 findings (fail-closed {} was wrong vs SDK; OnceLock cache breaks hot reload; injection API unused so tests non-hermetic; debounce test cannot detect missing debounce (mutation-proven by reviewer); forbid(unsafe_code) silently relaxed; Job Object assigned after child start; Windows-only clippy error; --probe never runs). T1-G7a-fix3 lane B in par-b at 9a38128 -> agy-medium `care-1-a-0ba8f878` (tmux). Unix reaper itself reviewed SAFE.
T1-G14 (LD-3) DONE: agy-medium `care-1-a-ec9e8e3f`, committed 180ace9 via cherry-pick of par-c 33e3a7e (clean; gate + Opus review pending; both test normalisers deleted, 95 differential scenarios green). T1-G16 (PC-4 docs + T1 closing verification, docs only) lane A in par-c at 180ace9 -> agy-medium `care-1-a-ec176fbe` (tmux).
Gate OK for 180ace9 (G14) but Opus review CHANGES-NEEDED (scratchpad/review-G14.md): fixtures byte-identical to Node, materialiser coverage OK; 8 findings: diff fix-up rewrites rawInput/rawOutput/_meta (Medium), blocking project-dir scan on the actor each idle (Medium), title context from converted message incl embedded files (Medium), result text not collected, batch id restore keeps last mapping, id maps never pruned, AIR permission toolCall gets locations:[], T-40 DONE overclaimed + nonexistent test citations. Queued group G14-fix (prompt t1-g14fix-body.md).
agy-medium hit its usage quota (resets in ~2h19m) mid-work; per the user's recipe (agy-medium preferred, deepseek fallback) both interrupted groups resume on deepseek from their saved trees (quota stop, not a model failure): T1-G7a-fix3 (agy `care-1-a-0ba8f878`, 15 files edited) -> deepseek `care-1-a-14e28df4` in par-b; T1-G16 (agy `care-1-a-ec176fbe`, docs edited) -> deepseek `care-1-a-3faf9a11` in par-c.
T1-G16 (PC-4 docs + T1 closing summary) DONE: agy `care-1-a-ec176fbe` (cut off by quota) -> deepseek `care-1-a-3faf9a11` (finished), committed e63320c via cherry-pick of par-c f5ce8ac (clean). Remaining T1-package non-DONE rows: 22, all with owners (see progress-G16.md). T1-G14-fix (8 Opus findings on LD-3) lane A in par-c -> deepseek `care-1-a-b83ff30a`.
T1-G7a-fix3 DONE: deepseek (resumed from agy edits) `care-1-a-14e28df4`, committed 5cace28 via cherry-pick of par-b f2ceff0 (clean). Opus round-4 (scratchpad/review-G7afix3.md): 5 of 8 FIXED, 3 PARTLY; no regressions in the unix reaper (tests 10/10 x3); mutation-proven debounce test; remaining small items accepted into T1-G7a-fix4 (auth actor bypasses managed injection + 5 tests read host managed paths; HKLM malformed fallthrough; unsafe scoping unenforced; probe blocks async worker; poll default 20ms) lane B in par-b at 5cace28 -> deepseek `care-1-a-9e393929`. This is the LAST fix round for this package unless the gate/readback shows a real regression.
USER (chat): agy-medium limits reset, move back. T1-G7a-fix4 deepseek `care-1-a-9e393929` -> agy-medium `care-1-a-1e35d2d3` (par-b, resumed from tree); T1-G14-fix deepseek `care-1-a-b83ff30a` -> agy-medium `care-1-a-3710982a` (par-c, resumed from tree). Switch is a model move, not a failure.
T1-G7a-fix4 integrated as 8fba009 BUT my independent gate FAILED: the new D8 check counts the doc comment at process.rs:8 as a second allow(unsafe_code) (count 2 != 1) although the author reported gate OK (false self-report; lesson: only the orchestrator's detached gate counts). T1-G7a-fix5 (tiny: exclude comment lines + selftest with fixture trees) lane B in par-b at 8fba009 -> agy-medium `care-1-a-c524c9d2`.
INCIDENT (2026-10-24 ~16:30): load average ~190 from 4,693 live `in_process --reaper --probe` processes: the reaper --probe handshake EXECUTES the candidate binary; for a non-adapter host (examples/in_process ignores args and opens a real session) it recursed (self-sustaining, some 3h+ old, orphaned). Killed by exact PID (STOP all in a tight loop, then KILL; no pkill by name; only binaries under my own worktree/scratch paths; the codex sibling project's cargo test was left alone). The round-1 'never re-exec an arbitrary host binary' finding was NOT really fixed; four review rounds missed it because no test counted descendant processes. gate ae2e6b6 also hit a load-induced failure of inv_26_transport_parity_polluted_host_env (passes 3/3 when run directly) and must be re-run after the fix. T1-G7a-fix6 (no exec of unknown binaries, hard recursion guard env, process-count tests, no leaked test processes) lane B in par-b at ae2e6b6 -> agy-medium `care-1-a-57976b9d`.
Gate OK for 7164086 (T1-G14-fix + T1-G7a-fix5 integrated): LD-3 fixes accepted (T-40 back to PARTIAL), D8 check fixed. The ae2e6b6 failure of inv_26_transport_parity_polluted_host_env was load-induced (load avg ~190 from the reaper-probe recursion; passes 3/3 directly). Until G7a-fix6 lands, the detached gate worktrees' `examples/in_process` binaries are renamed to *.disabled after each build (the recursion root) and a quiet watchdog (threshold 150, exact PIDs, own paths only) is armed. LESSON: tests that can spawn processes must count descendants; only the orchestrator's own detached gate result counts.

=== TURN 1 COMPLETE (2026-10-24) ===
Gate OK for e426b22 (T1-G7a-fix6 integrated; post-gate leftover example/reaper processes: 0; load avg 9.8). The reaper probe recursion is fixed (registration-only reaper, CLAUDE_ACP_REAPER_GUARD, five process-count tests). Branch head e426b22 + bookkeeping. FEATURES: DONE 432, PARTIAL 166, MISSING 316 (start of the 5-turn plan: DONE 269 / PARTIAL 241 / MISSING 403 at the first counted commit; before that 317/176/449 raw audit counts were against a 942-row draft). 28 integrated groups, each with a polluted detached gate run by the orchestrator; Opus reviews found real defects in LD-3, G2c, G7a (4 rounds), G10a, which were fixed. Remaining T1-package rows (22 in progress-G16.md) all have later owners. Next: Turn 2 (hazard turn: turn machine / settle path, one package per group, fresh Opus on every group).
Operational rules learned in T1: (1) only the orchestrator's own detached gate counts; sessions' self-reports were wrong twice; (2) tests that spawn processes must count descendants and leave none; (3) run detached gates from a non-UUID path without CARGO_TARGET_DIR; (4) one fresh session per group; (5) agy-medium is ~5x faster than deepseek but has a quota (~2h reset); (6) watchdog kills only exact PIDs under my own paths.
