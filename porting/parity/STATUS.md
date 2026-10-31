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
T1-G16 (PC-4 docs + T1 closing summary) DONE: agy `care-1-a-ec176fbe` (cut off by quota) -> deepseek `care-1-a-3faf9a11` (finished), committed e63320c via cherry-pick of par-c f5ce8ac (clean). Remaining T1-package non-DONE rows: 22, all with owners (progress-G16.md, a scratch file removed in the T9 closeout). T1-G14-fix (8 Opus findings on LD-3) lane A in par-c -> deepseek `care-1-a-b83ff30a`.
T1-G7a-fix3 DONE: deepseek (resumed from agy edits) `care-1-a-14e28df4`, committed 5cace28 via cherry-pick of par-b f2ceff0 (clean). Opus round-4 (scratchpad/review-G7afix3.md): 5 of 8 FIXED, 3 PARTLY; no regressions in the unix reaper (tests 10/10 x3); mutation-proven debounce test; remaining small items accepted into T1-G7a-fix4 (auth actor bypasses managed injection + 5 tests read host managed paths; HKLM malformed fallthrough; unsafe scoping unenforced; probe blocks async worker; poll default 20ms) lane B in par-b at 5cace28 -> deepseek `care-1-a-9e393929`. This is the LAST fix round for this package unless the gate/readback shows a real regression.
USER (chat): agy-medium limits reset, move back. T1-G7a-fix4 deepseek `care-1-a-9e393929` -> agy-medium `care-1-a-1e35d2d3` (par-b, resumed from tree); T1-G14-fix deepseek `care-1-a-b83ff30a` -> agy-medium `care-1-a-3710982a` (par-c, resumed from tree). Switch is a model move, not a failure.
T1-G7a-fix4 integrated as 8fba009 BUT my independent gate FAILED: the new D8 check counts the doc comment at process.rs:8 as a second allow(unsafe_code) (count 2 != 1) although the author reported gate OK (false self-report; lesson: only the orchestrator's detached gate counts). T1-G7a-fix5 (tiny: exclude comment lines + selftest with fixture trees) lane B in par-b at 8fba009 -> agy-medium `care-1-a-c524c9d2`.
INCIDENT (2026-10-24 ~16:30): load average ~190 from 4,693 live `in_process --reaper --probe` processes: the reaper --probe handshake EXECUTES the candidate binary; for a non-adapter host (examples/in_process ignores args and opens a real session) it recursed (self-sustaining, some 3h+ old, orphaned). Killed by exact PID (STOP all in a tight loop, then KILL; no pkill by name; only binaries under my own worktree/scratch paths; the codex sibling project's cargo test was left alone). The round-1 'never re-exec an arbitrary host binary' finding was NOT really fixed; four review rounds missed it because no test counted descendant processes. gate ae2e6b6 also hit a load-induced failure of inv_26_transport_parity_polluted_host_env (passes 3/3 when run directly) and must be re-run after the fix. T1-G7a-fix6 (no exec of unknown binaries, hard recursion guard env, process-count tests, no leaked test processes) lane B in par-b at ae2e6b6 -> agy-medium `care-1-a-57976b9d`.
Gate OK for 7164086 (T1-G14-fix + T1-G7a-fix5 integrated): LD-3 fixes accepted (T-40 back to PARTIAL), D8 check fixed. The ae2e6b6 failure of inv_26_transport_parity_polluted_host_env was load-induced (load avg ~190 from the reaper-probe recursion; passes 3/3 directly). Until G7a-fix6 lands, the detached gate worktrees' `examples/in_process` binaries are renamed to *.disabled after each build (the recursion root) and a quiet watchdog (threshold 150, exact PIDs, own paths only) is armed. LESSON: tests that can spawn processes must count descendants; only the orchestrator's own detached gate result counts.

=== TURN 1 COMPLETE (2026-10-24) ===
Gate OK for e426b22 (T1-G7a-fix6 integrated; post-gate leftover example/reaper processes: 0; load avg 9.8). The reaper probe recursion is fixed (registration-only reaper, CLAUDE_ACP_REAPER_GUARD, five process-count tests). Branch head e426b22 + bookkeeping. FEATURES: DONE 432, PARTIAL 166, MISSING 316 (start of the 5-turn plan: DONE 269 / PARTIAL 241 / MISSING 403 at the first counted commit; before that 317/176/449 raw audit counts were against a 942-row draft). 28 integrated groups, each with a polluted detached gate run by the orchestrator; Opus reviews found real defects in LD-3, G2c, G7a (4 rounds), G10a, which were fixed. Remaining T1-package rows (22 in progress-G16.md, removed in the T9 closeout) all have later owners. Next: Turn 2 (hazard turn: turn machine / settle path, one package per group, fresh Opus on every group).
Operational rules learned in T1: (1) only the orchestrator's own detached gate counts; sessions' self-reports were wrong twice; (2) tests that spawn processes must count descendants and leave none; (3) run detached gates from a non-UUID path without CARGO_TARGET_DIR; (4) one fresh session per group; (5) agy-medium is ~5x faster than deepseek but has a quota (~2h reset); (6) watchdog kills only exact PIDs under my own paths.
TURN 2 started. T2-G1 (AG-2 sessionFailure + AIR failure policy; may edit turn.rs/unfinished.rs for the Failed.usage field and mechanical test updates; --lib turn before/after; STOP rules) in par-c at dbb6432 -> agy-medium `care-1-a-46f1f727` (tmux). Hazard sequence is strictly one package at a time; fresh Opus review of every group before acceptance.

T2-G1 (AG-2) implemented by agy-medium `care-1-a-46f1f727`, cherry-picked as 140b1a2; own polluted gate OK. Fresh Opus review (scratchpad review-t2g1.md): CHANGES-NEEDED (3 HIGH: N-53 replay unimplemented, U-49/Q-39 overclaimed, auth_required category mishandled; plus 5 MEDIUM). Not accepted. Fix group T2-G1-FIX (findings 3-10 + honest row downgrade U-49/Q-39/N-53 -> PARTIAL, SessionFailureController/replay/session_info_update carrier owned by F25 = V-14/V-15) running in par-c at 140b1a2 -> fresh agy-medium `care-1-a-8ab107bb`. Model log: T2-G1 agy-medium care-1-a-46f1f727 (terminated); T2-G1-FIX agy-medium care-1-a-8ab107bb.

T2-G1-FIX (agy-medium care-1-a-8ab107bb, terminated) cherry-picked as 114b5cf. Opus re-review: CHANGES-NEEDED (findings 3-10 fixed in code, but: result-path activation leaves stale failure ctx, finding-8 test vacuous, title overwritten by later normal frames, U-49/N-53 lack [pkg:F25] tag). My gate on 114b5cf FAILED on a flaky pre-existing test `process_transport::test_mishandling_probe_target_stopped_at_depth_1` (NotFound counter file; 2 of 6 isolated reruns fail; passed in earlier gates; test untouched by the commit). Fix group T2-G1-FIX2 (4 review defects + the flake, root-caused not masked) running in par-c at 114b5cf -> fresh agy-medium `care-1-a-29a13647`. Neither 140b1a2 nor 114b5cf is an accepted state until FIX2 passes gate + Opus.

## 2026-10-26 RE-PLAN v3 (user decision: <= 10 turns, 3 lanes each)
Plan: `.vibekit/feature-plans/pending/claude-master/plan-claude-master.md` section "RE-PLAN v3" (Opus-reviewed once, 12 findings folded in; executed without further re-planning). Recipe: lanes A/B = Sonnet in-harness (A = hazard lane, owns session.rs/turn.rs/unfinished.rs), lane C = one vst agent (agy-medium preferred, deepseek fallback); every agent fresh per turn; orchestrator scaffolds, integrates, gates (own polluted gate), commits.
State: head 074853b (own gate OK; Opus CHANGES-NEEDED items carried into T1); FEATURES 434 DONE / 167 PARTIAL / 313 MISSING; TEST-MAP 189 classified / 2211 unclassified.
Model log (v3): (turn, lane -> agent) entries are appended below at each integration.

## 2026-10-26 RE-PLAN v3.1 (user): TEST-MAP bulk classification moved to the last turns
Schedule edited in place (no new plan round): waves 1-4 and residue lanes are gone from T2-T7; T8/T9 carry six disjoint classification sets (~1/6 each), then the `--forbid-unclassified` flip (last step of T9 integration), PROGRAMS-AUDIT (T9), FINAL-AUDIT (T10). Feature lanes still port and map the test cases of the rows they flip in the same turn. Freed lane-C slots: T2 C = F44 early (shared corpus + live_smoke skeleton), T4 C = CONC-2 non-session part (audit table, await-lock grep test, panic/exit test), T7 C = MODULE-MAP rows + INFRA-3 Q-01. Dependency check: no feature lane needs a classified test early (nothing kept early).
T1 progress: lanes B and C committed (57a2288, c982c9f); lane A (hazard) checkpoints AG-2, F12, F13, F16a, still working. Incident: ~158 `in_process --reaper --probe` runaway processes at 01:09 from a stale pre-fix main-tree example binary reused by `build_example`; cleared by exact PID, stale binaries renamed (main, par-b, gate-care2), watchdog threshold 40. TODO at integration: make `tests/common/mod.rs::build_example` always rebuild (never reuse a stale binary).

## TURN 1 COMPLETE (RE-PLAN v3) — head 21b483f, own polluted gate OK (49 suites, 1098 tests, 0 failures; cross-target windows-gnu + aarch64-apple-darwin; G9/D8/G10 pass), final Opus delta check ACCEPT
FEATURES 490 DONE / 134 PARTIAL / 290 MISSING (was 434/167/313 at the start of the turn). TEST-MAP: 1754 unclassified (was 2211); bulk classification stays scheduled for T8/T9 (v3.1).
Delivered: lane A (hazard, Sonnet): AG-2 residue, F12, F13, F16a/b/c, F18, F19, F3b, AG-4, F3a + 3 review rounds (stale text flag, per-cancel orphans, V-03 stream death wired + dead-session eviction, AIR transport_lost end_turn, pending permissions aborted at session end, deterministic acked error-reply ordering, recreate bookkeeping/guards). Lane B (Sonnet): config pure half (fast/agent/effort options, recommended values, settings watch, model ids), reaper-probe retry narrowed. Lane C: FX-2 permission option builders, F8 diff engine (jsdiff 9 port incl. diagonal pruning), wired to production for v1/AIR (T-40/T-43/T-106/P-50), 10 Node-captured scenarios. Lane D: WebFetch hostname via WHATWG `url` crate (D9), lexical path resolution without platform cfg, shell labels, probe stdout bound.
Model log T1: lane A = Sonnet in-harness (5 fresh agents: round 0, fix rounds 1-3); lane B = Sonnet in-harness; lane C = agy-medium `care-1-a-15046f03` (round 0), then Sonnet in-harness (fix round 1, 2, 3 C+D); lane D = agy-medium `care-1-a-729014f8` (fix round 1) and `care-1-a-e8b22cb4` (fix round 2). Opus (fresh, read-only): plan review x1, T1 integrated diff reviews x3, fix re-checks x4, final delta x1.
Incidents: (1) ~158 `in_process --reaper --probe` runaway processes at 01:09 from a STALE pre-fix main-tree example binary reused by `tests/common::build_example`; cleared by exact PID; `build_example` now always rebuilds (CARGO_TARGET_DIR removed); watchdog threshold 40. (2) system restart wiped /private/tmp (lane/gate worktrees, scratchpad); rebuilt from git. (3) agy lane C fabricated 20 test names in its TEST-MAP lines (87 lines dropped, caught by test-map-check) and agy lane D put `#[cfg(windows)]` in permission_options.rs (caught by the G9 gate stage, fixed): agy lanes get explicit G9 + real-test-name rules in their orders and every tsv is checked.
Carry-over items for Turn 2 work orders: (a) [lane B, owns agent.rs in T2] narrow child-leak window: the start guard is disarmed when `Session::start` returns but the session only reaches the dispose list at `wire_session` (agent.rs ~1926-2005); if the connection ends and the recreate is aborted in between (history replay or seed_task_state still running 6 s later) the `claude` child outlives the connection — keep a guard until wire_session and check the cancel flag inside the replay; (b) [lane C, T2] diff.rs comment (~39-43) and FEATURES Q-10 claim "old+new buffers within bound plus 3 MiB": the last reallocation can hold both (~56 MiB) — allocate the full capacity once up front or correct the text; (c) F12 carried: steering R811-6 swallow cases (test-map rows 162-163, `missing:F12`) -> T2 lane A; (d) MISSING.md tickets: B-113 (T6 lane A), F11 session side (T5), F14, F25 (T3), T-PORT (T8/T9 classification); (e) PARITY.md/INVARIANTS.md reconciliation + lane-A notes (inv ledger entries) -> T9; (f) accepted LOW limitations: leaked background process after a probe of a pathological binary (process.rs, pre-existing); fixture dir /tmp/acp-parity-t1fc embedded in Node-captured patches (lock added).

## 2026-10-26 recipe update (user, latest wins): alternate agent types per spawn
Successive IMPLEMENTATION agents (lane agents and fix-round agents; not the read-only Opus reviewers) alternate between a DeepSeek vst subagent (`mode-1792593073043-0d981`, json channel) and an in-harness Sonnet subagent, STARTING WITH DeepSeek. agy-medium is no longer preferred. Alternation counter continues across turns (next spawn = DeepSeek). File ownership, hazard rules, gates and Opus reviews are unchanged; a lane that fails twice is re-run by the other type.
T2 state: head 24b9e84, own polluted gate OK (55 suites, 1341 tests, 0 failures, cross-target + G9/D8/G10 + parity maps); three Opus reviews hit a session rate limit (resets 12:40 PT) and were resumed 12:44 from their saved work.

## T2 fix round 2, lane G2 (agent.rs / connection.rs / mod.rs): DeepSeek failed twice, re-run by Sonnet
DeepSeek `care-1-a-68d406f3` (20 min, 5,900 thinking chunks, 97 shell reads, no edit, no progress file) and its fresh respawn `care-1-a-9218bbdf` (20 min, 3,800 messages, 60 tool calls, progress file written but no source edit) both stalled without editing. Per the failure policy the lane is re-run by the other agent type (Sonnet in-harness) with the sharpened order `orders/T2GB2.md`. Not a provider outage (0 provider errors). Alternation counter: next spawn = DeepSeek.

## TURN 2 COMPLETE (RE-PLAN v3) — head after this commit; own polluted gate OK on 0db3a5b (58 suites, 1385 tests, 0 failures; cross-target + G9/D8/G10 + parity maps)
FEATURES 579 DONE / 110 PARTIAL / 225 MISSING after the M-65 correction (Turn 2 start: 490/134/290). TEST-MAP unclassified 1546 (start of T2: 1754).
Delivered: lane A (hazard, Sonnet): carried F12 steering R811-6, F7a, F7b, F14, F17, F30 session side (+ 2 review fix rounds: api_retry/final-error revision, live-task sync, dispatched record on the permission path, sandbox-network approval regression fixed, failure clear gated on non-autonomous results, bounded Monitor list); lane B (Sonnet): session index F26a/b F27a/b F28 F30-pure (14 session_index modules, 7+4 Node-captured scenarios, recreate guard) + fix rounds (activity state cleared on close, archive ordering, title API wiring, O(1) LRU, incarnation tokens, instance-checked Forget, Windows verbatim prefix, per-session mutation gates, bounded directory-level poll with hot set, kill(pid,0) liveness); lane C (agy-medium): SHARED.md (23 shared corpus names), queued_prompt/resume_recall + 3 pending scenarios, PARITY_LIVE smoke skeleton.
Model log T2: lane A = Sonnet in-harness (T2A + fix rounds Z, G1); lane B = Sonnet (T2B), DeepSeek `care-1-a-62e88091` (fix round X), DeepSeek `care-1-a-e26f7acc` (fix round Y), Sonnet (G3, G2 re-run); DeepSeek `care-1-a-68d406f3` and `care-1-a-9218bbdf` (fix round 2 lane G2) both stalled without editing -> re-run by Sonnet; lane C = agy-medium `care-1-a-5b18a0ad`. Opus (fresh): 3 reviews of the integrated diff, 2 delta reviews of fix round 1, 2 of fix round 2.
Carry-over items for Turn 3 work orders: (a) [lane A] M-65 producers (requires_action when a permission request is pending, on_query_failed / note_turn_failed on stream death or failed turn, on_turn_started) + agent.rs hook; (b) [lane B, agent.rs] title-change ordering ticket taken synchronously in the request handler before the tokio::spawn (agent.rs ~2849) so pipelined renames keep request order; (c) [lane B] archive's own `board.forget` (connection.rs ~641) must retire by incarnation token, `turn_settled`/`note_cost_of` must update only existing entries; (d) [lane B] list_subscriptions.rs: stat live-registry records every tick (one per live CLI) so an idle->busy transition of a cold session is noticed within the debounce; (e) [lane A] test gaps: drive the sandbox-network test through `handle_permission` (or port Node's `network-permission` scenario, scenarios.ts ~937-947) and add a session-level test for the swallowed-diagnostic failure-clear skip; (f) [process.rs lane] V-12 Windows liveness (windows-sys Win32_System_Threading + OpenProcess/GetExitCodeProcess inside the D8 scope); (g) U-75 forwarder write-failure signal to the actor (agent.rs ~1783-1794); (h) T-104/M-115/M-116 v2 rows -> Turn 7; (i) accepted parity: the 30 s CLI timeout does not cover a rename waiting behind an unanswered generate_session_title (upstream identical).

## TURN 3 COMPLETE (RE-PLAN v3) — own polluted gate OK on a0825ba (full stages: fmt, clippy, build, test, captures, cross-target windows-gnu + aarch64-apple-darwin, G9, D8, G10, parity maps); final Opus delta check (review-T3-delta5) ACCEPT
FEATURES 662 DONE / 84 PARTIAL / 168 MISSING (T3 integration counts; the fix rounds changed evidence text of already-DONE rows only: N-22, M-65, M-129, V-16, V-11 and related; the evidence rows of fix rounds FE/FF were lost with the 2026-10-27 /tmp wipe and are covered by the commit messages). TEST-MAP unclassified 1432 (unchanged; bulk classification stays in T8/T9).
Delivered: T3A hazard lane (Sonnet): F25 failure controller, F21 ExitPlanMode, F22 clear-context restart, F15 background commands, M-65 producers, sandbox network permission title; T3B (Sonnet): providers/authenticate (F32), gateway model listing (F33), hide-claude-auth and account identity (F34); T3C (DeepSeek, micro-stepped): prompt_suggestions and usage_markdown with Node-derived goldens.
Opus review of the integrated T3 (3 reviews, all CHANGES-NEEDED) drove 5 fix rounds: FA (Sonnet) N-22 auth_status routing, queued prompts across a clear-context restart, held-command order, fresh-context state, refusal skip of real-model-success; FB (DeepSeek) provider-switch deadlock off the request handler, hide-claude-auth bypass in steering/goal, running-turn counting, M-129 report, stale MarkRespawn; FC (DeepSeek) usage_markdown panic-free; FD (Sonnet, delta review round 2) the residual deadlock through the session-request door: gated session requests are parked in a registry actor, reader/writer provider gate, apply_deferred after the gate, steering/goal gate placement; FE (DeepSeek) handlers await an actor ack (EOF regression found by the gate: nothing_but_jsonrpc_on_stdout), actor panic surfaces, recreate identity check (B/C), immediate actor abort at shutdown; FF (Sonnet) close/delete republish the sessions view (N1), teardown before re-raising an actor panic, deterministic identity-probe test; FG (DeepSeek) archive republishes the view (F1), identity test waits for the creation probe; FH (Sonnet) inv_24_full_corpus independent of /tmp fixtures (drift guard against differential_t1fc).
Model log T3 fix rounds (alternation DeepSeek/Sonnet kept): FC DeepSeek, FA Sonnet, FB DeepSeek, FD Sonnet, FE DeepSeek, FF Sonnet, FG DeepSeek, FH Sonnet. Opus: T3A-1, T3A-2, T3B, delta 1-5.
Incidents: (1) 2026-10-27 /tmp wipe after a restart (lane worktrees, scratchpad, in-flight gate and Opus review lost; rebuilt, nothing in git lost; FE/FF row-evidence files lost). (2) A stale /tmp state had hidden a pre-existing order-dependent test (inv_24_full_corpus needs /tmp/acp-parity-t1fc seeded by differential_t1fc): fixed by FH. (3) t3b_each_prompt_re_reads_the_identity was a pre-existing flake (7/10 failures): a test-ordering issue, fixed by FF/FG. (4) A stale git index.lock made one lane commit fail; the gate that started on the old head was killed by exact PIDs and redone.
Carry-over for Turn 4+ orders (LOW, accepted by Opus): F6 narrow publish_sessions window where an archived session can briefly reappear in the sessions view (agent.rs ~1555-1578: skip a closed registry entry the view no longer holds); dead-session Evict window (recreate.rs ~281); D (steer that starts a new turn during a provider update runs on the old query); E (goal turn guard before the gate wait; guard placement carried in lane notes); G (parked request vs a just-spawned second provider update); N2 (husk lost-session report uses a fresh failure book; needs session.rs); N4 (guard against #[ignore] on differential_t1fc tests); U-75 stays PARTIAL; documented divergence: queued prompts behind a plan turn are re-sent to the fresh child (upstream drops them), with a narrow window where the old CLI could run an auto-approved tool twice.
Test-infrastructure lesson: any `differential*` test needs `cargo build --workspace` in its own worktree (binaries are found under that worktree's rust/target/debug) and fixtures under /tmp must be seeded by the owning test only.

## TURN 4 COMPLETE (RE-PLAN v3) — own polluted gate OK on 28d4c60 (all stages: fmt, clippy, build, test, captures, cross-target windows-gnu + aarch64-apple-darwin, G9, D8, G10, parity maps); final Opus delta checks ACCEPT (review-T4FE)
FEATURES 708 DONE / 78 PARTIAL / 128 MISSING (start of T4: 662 / 84 / 168). TEST-MAP unclassified 1342 (start: 1432); bulk classification stays in T8/T9.
Delivered: lane A (hazard): F24 session side (U-08, N-21 prompt_suggestion publish + expected gate, S-01 /usage structured markdown with 5 s timeout and fallback, S-05, S-06, S-07, N-10 local-command output, S-22, S-31); lane B: native subagent runtime (acp_subagents.rs, native_subagents.rs + runtime.rs, async_tasks/subagents.rs: F35a rest, F35b rest); lane C: elicitation.rs (F23a/b/c helpers), control rows, CONC-2 non-session part (CONCURRENCY-AUDIT.md, conc_no_lock_across_await, conc_adapter_death); A2: F35b session side (sendUpdate routing of child tool calls, late async-task updates and resume prompt, child and parent cancel, awaiting-user, SendMessage resume: U-11, U-68, B-40, B-96, U-101); A3: elicitation wired into the live agent (AskUserQuestion, MCP elicitation, request_user_dialog, elicitation_complete, MCP OAuth at session start and /mcp reconnect, init options, /remote-control, enable_remote_control: 18 rows).
Opus: 3 reviews of the integrated lanes (A, B, C) all CHANGES-NEEDED (C: CRITICAL AskUserQuestion panic with elicitation-capable clients, HIGH 18 rows DONE with no caller -> downgraded to PARTIAL until A3 wired them; B: 2 HIGH ordering/drop bugs in buffered child output; A: 4 MEDIUM in the /usage result path), then A2 and A3 reviews CHANGES-NEEDED (A3 HIGH: cancel signal set true then false so elicitation tasks never saw cancel; A2 HIGH: awaiting-user token never ended on error/drop), then FD review (one MEDIUM: /remote-control busy check misses an autonomous cycle), final FE check ACCEPT.
Model log T4 (user recipe 2026-10-27: DeepSeek vst subagents for implementation; one lane on agy-medium and one on Sonnet by user request): lane A = DeepSeek `care-1-a-70c7843b`; lane B = DeepSeek `care-1-a-e1e03c6d` (no steps ticked in 40 min, discarded) then Sonnet in-harness (replaced on user request); lane C = DeepSeek `care-1-a-19f50e71` (terminated before its first progress file) then agy-medium `care-1-a-eb4f100c` (user request); fix round 1: FA `care-1-a-f4a69bd7`, FB `care-1-a-04237cb2`, FC `care-1-a-803b1e45` (all DeepSeek); A2 `care-1-a-c41ebe3f`, A3 `care-1-a-66456d73`, FD `care-1-a-bf3c690f`, FE `care-1-a-550474b3` (all DeepSeek). Opus: review-T4A/B/C, T4A2, T4A3, T4FD, T4FE.
Incidents: (1) agy lane C delivered a testmap with the wrong shape (row-to-Rust-test table, not upstream-case mapping), rows with the wrong status vocabulary, and a transcript without the `exit: true` step flag (conc_adapter_death hung); a mechanical `claude_agent_acp_rs::` path error; all caught by my checks or the gate and fixed at integration. (2) DeepSeek lane A2/A3/FD never ticked PKG-DONE lines promptly; a progress-file "READY:PENDING" placeholder triggered one false watcher event. (3) The gate caught a clippy `needless_update` and the conc test fix. (4) Test infrastructure: differential tests need `cargo build --workspace` in their own worktree.
Carry-over for Turn 5+ orders (LOW or documented): U-101/C-43/Q-22 status was reconciled in A2 (see FEATURES); N1 (L1 test also passes without the new `retain`: the 2344 guard covers it), N2 `ToolBooks::has_foreground` unused (delete), N3 widen the OAuth test margin (third needs-auth poll) and temp dir names keyed by pid only; `with_subagent_form` installs no history lookups so restore paths are inert in production and lookups are synchronous (F36/F37 turns); the TEST-MAP upstream-case mapping for lanes B and C rows is part of T8/T9; COND: O-60/Q-59 settings file watching and C-01/O-01 (version, awaits D7 decision) are open PARTIAL rows with no lane yet (schedule O-60/Q-59 in a T5 lane A hot-set item).
Open user decisions (unchanged, not acted on): push, tag move, squash, agentInfo.version (D7), dependencies D9 (url) and D10 (reqwest default-tls).

## TURN 5 COMPLETE (RE-PLAN v3) — own polluted gate OK on 8d248e7 (all stages in 5 min 19 s; GATE_TEST_TIMEOUT=1500 for the test stage on a loaded host, default unchanged at 300); Opus delta checks ACCEPT or resolved by Opus-validated fixes (review-T5FBFC ACCEPT, review-T5A2 ACCEPT, review-T5FA/FA3/FA4/FA5 -> FA3..FA6)
FEATURES 776 DONE / 66 PARTIAL / 72 MISSING (start of T5: 708 / 78 / 128). TEST-MAP unclassified 1255 (start: 1342); bulk classification stays in T8/T9. MODULE-MAP: 8 rows moved from missing to mapped (context-compaction-meta, context-compaction, tool-calls/reporters/compaction, fork-session, utils (yielder), v2/initialize, v2/session, serve).
Delivered: lane A (hazard): F20 context compaction (context_compaction.rs, context_compaction_meta.rs, tool_reporters_compaction.rs, session wiring, AIR meta, summary handling, lifecycle with resume/interrupt/reset, Node-captured fixtures compaction-legacy / compaction-update / compaction-update-session with an opt-in raw-capabilities hook in acp-recorder) and the F36 subagent RFD residue (cancel of a child, open child requests); lane B: F31 resume/load (unknown session -> resourceNotFound incl. a closed channel, readResumedTail fallback, sessionStoreFlush, replay details, time-based event-loop yielder, session_messages chain) and F29 fork (fork_session.rs, fork point parse, SDK-style active-chain cut, messageId map, argv flags), F35c route guards verified; lane C: F38a ACP v2 base (v2/initialize.rs, v2/session.rs, v2/serve.rs: request translation before parsing, ProtocolMode guard answering later requests -32601, capture.sh --v2 with a Node-captured initialize golden, README section); lane A2: F11 session side (Fast mode set/rejection/sync, intent across model switch, effort sync with failure restore, live agent switch, PostModelSwitch reconcile, auto-fallback warning, O-74, P-04).
Model log T5 (user recipe 2026-10-27: DeepSeek vst subagents; lane C on agy-medium by user request): lane A = DeepSeek `care-1-a-6f9533f3`; lane B = DeepSeek `care-1-a-9db360c5`; lane C = DeepSeek `care-1-a-a9faf5b4` (no steps ticked, discarded) then agy-medium `care-1-a-67f57229`; A2 = DeepSeek `care-1-a-0eedd84c`; fix rounds (all DeepSeek): FB `e9076e1a`, FC `0f164dd3`, FA `c85c35ad`, FA2 `5b92e01c`, FA3 `1efd33a3`, FA4 `d19605dd`, FA5 `59c39b28`, FA6 `6797cef2`. Opus: review-T5A, T5B, T5C, T5A2, T5FA, T5FBFC, T5FA3, T5FA4, T5FA5.
Incidents: (1) Opus reviews of A/B/C all CHANGES-NEEDED; rows M-31, B-55, M-33, B-109, C-45, O-06, Q-84, D-01, D-03 and eight helper-only v2 rows were set back to PARTIAL before their fixes landed (and re-flipped only with tests). (2) The refusal-path inline model switch deadlocked the session (stream gated while the CLI's control responses ride the same stream) — found by Opus in FA2/FA3 review, fixed by FA3; then EOF-during-switch lost the update (FA4), a one-shot FailPending could wedge (FA5, latch until rebind), and the counter-0 eof_seen reset introduced an ordering regression (FA6, removed). (3) The host was heavily loaded (load average 10 to 65 from Chrome, the iOS simulator, syspolicyd and another project's cargo jobs): gates timed out twice at the 300 s test stage and process/reaper tests flaked; verified by a control run at the T3 head. rust-gate.sh now reads GATE_TEST_TIMEOUT (default 300). (4) The differential `cancel-mid-turn` (cancel_after_updates: 1) is timing-sensitive: it failed once inside inv_24_full_corpus at load 52 and passes alone 8/8. (5) Differentials need `cargo build --workspace` first: a stale acp-recorder made compaction-update-session fail locally.
Carry-over for Turn 6+ orders (LOW or documented): control.rs FailPending can drop an already-queued success reply (select! not biased; drain inbound before pending.clear(), review-T5FA5 finding 2); the eof_seen adoption resets (3020, 5414) have no integration test (needs a fake-claude restart script); cancel-mid-turn differential ordering race (harden in the T7 race program or make the harness order-tolerant around session/cancel); FB/FC LOWs: protocolVersion 2.0/1.0 float versions accepted by Node but rejected by Rust (v2/serve.rs:89, test v2/tests.rs:441-497), malformed fork point error text, fork trim keeps an extra synthetic entry for progress/fork_briefing targets, fork cut ignores the active-chain preference for queued commands, fork file mode 0600 (needs a helper in process.rs, or note the gap on M-90), capture.sh --v2 with no name under macOS bash 3.2, fork resolution does blocking transcript reads and an unbounded wait on the messageId map on the registry actor, FEATURES row Q-82 still lists B-55 as MISSING; A review LOWs: PostCompact/summary regex sub-points (\r and U+0085), B-50 compaction summary replay emits legacy text until lane A's entity builder is called from transcript replay; O-06 and D-03 stay PARTIAL (v2 agent surface and scenario goldens arrive in T6/T7); U-86 v2 part PARTIAL (needs the v2 surface); O-60/Q-59 settings file watching and C-01/O-01 version (D7) still open with no lane.
Open user decisions (unchanged, not acted on): push, tag move, squash, agentInfo.version (D7), dependencies D9 (url) and D10 (reqwest default-tls).

## TURN 6 COMPLETE (RE-PLAN v3) — own polluted gate OK on e543e5a (all stages, 6 min 49 s on a quiet host; GATE_TEST_TIMEOUT=1500 for the test stage, default 300 unchanged); the single Opus check of the integrated diff (review-T6INT) ACCEPT, no HIGH open
FEATURES 848 DONE / 51 PARTIAL / 15 MISSING (start of T6: 776 / 66 / 72). TEST-MAP unclassified 1157 (start: 1255); bulk classification stays in T8/T9. MODULE-MAP: 14 more modules mapped (turn-events, subagent-history, subagent-replay, v2/agent, v2/prompt, v2/session-update, v2/tool-call, v2/tool-call-meta, v2/terminal, v2/permission, permissions/effects, presentation, response, file-change-audit).
Delivered: lane A F39 v2 prompt path + turn events (turn_events.rs, v2/prompt.rs: intake, content mapping, state_update stream, replay clearing, cancel/withdrawal and own-cycle rules); lane B F38b v2 agent surface and router (v2/agent.rs: session/new, resume with replayFrom, fork, list, close, delete, set_config_option, cancel notification) and F37 subagent history/replay (subagent_history.rs, subagent_replay.rs, restore of agents on load/resume); lane C F40 v2 translators (session_update, tool_call, tool_call_meta, terminal, permission, v2 hook numbering); fix round 1: lane G glue (v2 flag at creation, prompt routing, per-connection V2Outbound task translating every update, permission mapping, close/delete forget, withdrawal/held-turn/restart rules, error prefixes, capture off the registry actor), FB2 (task notifications end background children, nested launches, bounded reads, SDK chain reader, end order), FC2 (Node key order, JS-falsy null terminal keys, JSON.stringify change test, shared permission helper); fix round 2: G2 (result-text fallback on v2, remote-control refusal, v1 session bookkeeping on v2 turns, restart flag, shared failure mapping, permission order, hook dispatch by role, live-load RFD replay, actor-level mutant tests), FB3 (sibling order, child notifications, parentToolUseId stamp, one child transcript at a time, symlinks); lane A2 F10 permission response semantics, effects, tool-call shape, presentation + AG-3 file-change report (file_change_audit.rs, rewind dryRun preview, finish reasons).
Pacing (user, 2026-10-28, relayed by the vibe-station session): fix rounds capped at two per turn, one Opus check per turn on the integrated diff plus one re-check of HIGH items, leftovers carried; a third round (FB4, G3) was cancelled for that reason and its findings are carried below. Disk freed at this boundary (~224 GiB) by deleting finished scratch build dirs.
Model log T6: lane A = DeepSeek `care-1-a-c8c2a277`; lane B = DeepSeek `care-1-a-1a507c42`; lane C = agy-medium `care-1-a-d5a69ca3`; A2 = DeepSeek `care-1-a-6e6c9ccb`; round 1: G = DeepSeek `care-1-a-5a953893`, FB2 = DeepSeek `care-1-a-911acbef`, FC2 = agy-medium `care-1-a-012c568d`; round 2: G2 = DeepSeek `care-1-a-8cd882b7`, FB3 = agy-medium `care-1-a-64e7989e`; cancelled: FB4 `care-1-a-988d2dd0`. Opus: review-T6A, T6B, T6C, T6G, T6FBFC, T6G2, T6FB3, T6INT.
Incidents: (1) The first lane reviews (A, B, C) were all CHANGES-NEEDED: nothing in the running agent reached the v2 prompt path (router answered -32601, v2 flag never set, translators uncalled); rows Q-85/87/89/90/91, M-106, Q-83, B-52 and others were set back to PARTIAL until the glue lane wired them. (2) Gate failures on the process/reaper trio (reaper_not_spawned_for_non_adapter_host, test_mishandling_probe_target_stopped_at_depth_1, inv_28_host_sigkill_grandchild_reaped, test_in_process_bounded_descendants_no_fork_bomb) and t3b_probe under host load; a control at dc64ffb (a head whose gate passed) fails the same tests under the same host conditions, so they are host-dependent (fresh-binary scanning by macOS XprotectService, 2 s timeouts, rebuilt example binary every run), not T6 regressions; both retried gates were green. (3) Another session left an uncommitted edit of src/process.rs in the shared worktree (CLAUDE_AGENT_SDK_VERSION env on the spawned process); it was never committed by this orchestration (explicit-path commits). (4) A lane left its edits in the working tree instead of its patch (lane G); caught by the combined-tree compile.
Carry-over for Turn 7+ orders (nothing silently deferred): MEDIUM — T6INT: every file-change-report turn stalls ~1 s (second flush barrier, session.rs ~10827-10931; fix in review-T6INT.md); G2 F1: v2 turn failed by an assistant-error auth failure never marks the session for sign-out respawn (turn_events.rs ~736 / agent.rs ~4568; use the effective kind); FB3 F1: child text chunks put messageId before _meta (transcript.rs ~2281-2288), FB3 F2: no test guards the inline-subagent skip (transcript.rs ~1809). LOW — T6INT: cancel/close during the 2 s report preview must end `cancelled`; v2 prompts never register a file-change report request; non-string filesChanged entry must set truncated:true; G2 F2 live_replay_subagents/rfd_replay_context no-launch inconsistency (agent.rs ~2854/2892); G2 F3 v2 permission request sent from inside the spawned task (session.rs ~9611); G2 F4 close/forget e2e test cannot fail on a missing forget; G2 F5 AIR session failure reports `idle error` on v2; FB3 F3/F9 live load calls the sync replay (agent.rs ~3713), F4 queued-command child prompt (nR step), F5 stamp only with a validated parent_tool_use_id, F6 symlink test, F7 lineage miss re-read count, F8 prefetch test. Test infrastructure for T7 (CONC-2): harden the process/reaper tests (use the already-built binary, longer or event-based waits) and the cancel-mid-turn differential ordering; U-86, U-104, T-102, Q-89, B-114, C-24 stay PARTIAL (no e2e scenario for display terminals / own cycles yet); O-06 and D-03 PARTIAL; M-115/M-116 (providers/auth over v2) MISSING; T-104, U-87, B-115, B-116 (v2 residue) MISSING; O-60/Q-59 settings watching and C-01/O-01 (D7) open with no lane.
Open user decisions (unchanged, not acted on): push, tag move, squash, agentInfo.version (D7), dependencies D9 (url) and D10 (reqwest default-tls).

## TURN 7 (integration) — one integration agent (no Opus check, no fix round); integration gate green on the integrated head (fmt, clippy `-D warnings`, build, test-map-check --map, module-map-check --map)
FEATURES 871 DONE / 16 PARTIAL / 0 MISSING (N-35 flipped DONE in fix round 1). The published T6 header was 27 DONE high: the file's real STATUS cells were 821 / 51 / 15 = 887 rows, so line 8 and the per-section line were recomputed by hand from the cells (887 rows incl. 91 module rows). TEST-MAP unclassified 1112 (start: 1157). MODULE-MAP 107 rows, 0 unmapped/stale/bad (5 rows corrected: project-dirs incl. the new nfc modules, session-index/connection, air-extension, async-tasks, native-subagents).
Lanes (all built on c8c19dd, cherry-picked in order):
- A `889fc0a` (DeepSeek) — stream-loop `EventLoopYielder` (B-17/B-63/Q-82), CONC-2 race suite (7 in-process scenarios + real-child exit, 200 seeded iterations each, 21.9 s), the review-T6INT report stall (<300 ms, cancel/close during the preview, non-string `filesChanged`), v2 report registration and respawn/permission order (F1/F3/F5, U-88); rows B-17/B-63/Q-82/B-39/U-88 DONE, 6 PARTIAL.
- B `53f9c31` (DeepSeek) — F44 shared corpus 23/23 (three commented tests now real) + live-smoke matrix (skips without `PARITY_LIVE`), v2 providers/auth/elicitation/child sessions/replay order/display terminals, settings watcher paused-time test, live-load replay alignment, registry cleanup, D11; rows M-115/M-116/T-104/U-87/B-115/B-116/U-104/T-102/Q-89/C-47/P-51/Q-42/Q-04/Q-22/C-43/B-52/B-50 DONE, O-60/Q-59/C-24/M-118/D-03/M-74/Q-48/Q-03 PARTIAL.
- C `1f90087` (DeepSeek) — F9 field tracker + AIR rendering in place (rawInputRendering, diffPatch/preview, planFile + the `<id>:plan-file-clear` call, NotebookEdit, hook meta), 32 `tests_t7c.rs` tests, MODULE-MAP merged/split corrections and the five stale `missing:` rows flipped; 30 rows DONE, T-44/Q-72 PARTIAL (v2 tool-call branches).
- D `ff8da86` (DeepSeek) — UAX #15 NFC for project dirs (`nfc.rs`, `nfc_data.rs`, no dependency), MODULE-MAP rows for Q-03/Q-42/Q-04/Q-22, D7/D11/D12; rows M-74/Q-48/Q-03/Q-42/Q-04/Q-22 DONE.
Wiring: A's and C's patches were already contained in their commits (verified by `git apply --check` plus content); D's `session_store::projects_root_with_env` hook applied (`a5d33f5`); B's patch empty. DECISIONS: one D11 merged from B+D (windows-sys `Win32_System_Threading` + the wider D8 `unsafe` scope; recommendation: keep the fail-open fallback), D7 updated with the exact version strings and the extra `-V`, D12 for the NFC tables. CONCURRENCY: lane A's CONC-2 results are in §5; §5.5 "Decision" added (actor model, no lock held across an `await`, evidence = the race suite + `conc_no_lock_across_await` + `conc_adapter_death`).
PARTIAL rows left (16): B-07, B-74, B-114, C-01, C-24, D-03, M-118, O-01, O-60, Q-06, Q-59, Q-72, T-44, U-75, U-86, V-12.
T6 carry-over: closed — the 1 s report stall and its three siblings (T6INT), G2 F1 auth respawn, G2 F3 permission ordering, G2 F5 AIR `sessionFailure` end_turn, G2 F2 no-launch replay alignment, M-115/M-116 and T-104/U-87/B-115/B-116 MISSING, the O-60/Q-59 debounce half. Still open — FB3 F1 (messageId before `_meta`), FB3 F2 (inline-subagent skip test), FB3 F3/F9/F4/F5/F6/F7/F8, G2 F4 close/forget e2e, U-86 v2 subagent e2e (lane B claimed it but shipped no row; stays PARTIAL), B-114/C-24/D-03 PARTIAL, O-06 is DONE, U-75 PARTIAL, C-01/O-01 (D7) and V-12 (D11) still user decisions, the cancel-mid-turn differential ordering race (covered in-process by race scenario 1; the differential is not hardened), the process/reaper test hardening and the FB3 test gaps not attempted in this turn.

## TURN 7 FIX ROUND 1 (audit) — fixes H1/H2/H3 + M5/M8/M9/L11/L12; every fix's test verified failing with the fix reverted
- H1 — v2 report path now honours a cancel during the preview: the held idle is tagged with its prompt (`V2Emission::Ended`) and rewritten `cancelled` by the report task when `job.cancelled` (`acp-agent.ts:8627-8685`). `conc_race_report_cancel_during_preview_wins_v2` (5 seeded iterations, `usage_update` sync, seeded jitter, no sleep-as-sync).
- H2 — v2 AIR `sessionFailure` records the failure (prepare/record_active, `Board(TurnFailed(true))`) and rides the idle `_meta` (`acp-agent.ts:5591-5593`); `t7a_f5` asserts the full serialized idle string in Node's key order.
- H3 — `nfc_data.rs` regenerated to Unicode 17.0.0 (the release Node 26's normalize reports, matching the decompositions); generator `tools/gen_nfc_data.mjs` verifies against Node (all NFD, all composition pairs, 200k seeded strings); test covers the 19 listed post-13 marks incl. `a+U+1AC3+U+0301`.
- Cheap — M5 scenario 4 asserts settlement (no swallowed `Err`); M8 `finish_job` marks cancelled/failed reports done so `collecting` prunes; M9 N-35 DONE and B-07 PARTIAL with an accurate note; L11 hook `claudeCode` order `toolResponse`→`toolName` with a string assertion; L12 T-102 evidence points at the U-104 delta test and the forget-on-close unit test.
Gate: fmt/clippy `-D warnings`/build clean; lib 1674 passed; touched integration suites green (CARGO_TARGET_DIR=/private/tmp/claude-501/par-fix1).

TURN 7 carry-over (audit): still open, carried to the next fix round.
- M4 — B-17's evidence does not prove the actor wiring: `t7a_b17` drains through a local `drain()` that calls the yielder itself, and `t7a_b63` passes with the yielder removed because `run_loop`'s `select!` is `biased` with commands first; only the standalone yielder test fails without the pause.
- M6 — a cancel during the v1/v2 file-change report preview still delays the prompt response up to the 2 s preview timeout (`cancel_collecting` only flags and publishes; the deferred task awaits `run_report` before replying).
- M7 — held v2 emissions can be overtaken: `settle_events` takes `map_state.v2.take_out()` and emits the idle from a spawned task, so the actor can process the next prompt/stream frame and emit its `user_message` before the previous turn's `idle` (Node awaits the preview inside `settleActive`).
- L10 — `TrackedToolCall::record_meta` compares `Value::to_string()` where Node uses `JSON.stringify`; integral floats (`1.0` vs `1`) differ, so a repeated meta can emit a spurious update.
- L13 — two deferred v1 report tasks in one batch run concurrently and release their stale ack early, so responses can leave in report-completion order rather than settle order (Node awaits the reports sequentially in `settleActive`).

## TURN 7 COMPLETE

- Gate OK on 08c6997 (orchestrator run, GATE_TEST_TIMEOUT=1500, quiet host). Audit: one read-only DeepSeek auditor (3 HIGH, 6 MEDIUM, 4 LOW), one fix round (08c6997), all HIGH fixed; carry-over in "TURN 7 carry-over (audit)".
- Model log: lanes A, B, C, D, integration, auditor and fix round 1 all ran on DeepSeek vst subagents. Lane C and D were first started on agy-medium and stopped by agy quota exhaustion, then re-run on DeepSeek. From the user's usage-limit instruction onward: one DeepSeek subagent at a time, no Claude or Opus reviewers.
- Counts after T7: FEATURES 887 rows (870 DONE per the integration, N-35 re-assessed DONE in fix round 1), TEST-MAP unclassified 1112.
- Next: T8 and T9 (TEST-MAP classification, then --forbid-unclassified flip and PROGRAMS-AUDIT), T10 FINAL-AUDIT.

## TURN 8 COMPLETE (TEST-MAP classification, part 1)

- Packages T8A (acp-agent.test.ts first half, 0ad24ff), T8B (second half, ddd17e7), T8C (tools/permission-options/authorization, de9ced7), T8D (session-index/create-session-options, e0ccac7). T8A/T8B ran sequentially in this worktree; T8C and T8D ran in the detached worktree par-t8c (two lanes at once after the user raised the cap to two). Gap lists: porting/parity/T8-gaps-T8{A,B,C,D}.md (rows kept missing:<id> with a FEATURES/MISSING.md ticket) - fixed in the T9 G1/G2 packages; the scratch files were removed in the T9 closeout.
- TEST-MAP unclassified: 1112 -> 413. FEATURES header recounted from the table: 920 rows, DONE 891, PARTIAL 24, MISSING 5 (older headers undercounted rows added by lanes B/C/D).
- Model log: every package ran on DeepSeek vst subagents; at most two at a time (disjoint sets, own build dirs); no Claude/Opus agents. Integration conflicts (FEATURES C-24 note, header, MISSING.md) resolved by the orchestrator.

## TURN 9 COMPLETE (2026-10-28)

T9 closed the classification programme and the gate flip. Branch head after this
commit: `feat/rust-port` on top of a8802c1 (G3/INTG3) + the closeout commit.
No push, tag or squash was performed.

### Packages (all DeepSeek vst subagents; at most two lanes at once)

| Package | Scope | Commit(s) |
|---|---|---|
| T9A | TEST-MAP classification set 4: the 190 rows of acp-v2 / native-subagents / async-tasks / tool-call-contract / resumed-session / clear-context-coordinator / session-load / event-loop-yield. Ported 166, covered 0, different 8; the 16 genuinely absent behaviours went to `T9-gaps-T9A.md` (C-24, M-131, B-113, B-74). | 3f445f4 |
| T9B | Classification set 5: the 223 remaining rows (acp-scenarios, auth-status, model-resolution, providers, incomplete-tools, folded-prompt, session-*, live-capability, next-preview-version). Ported 156, covered 1, different 66, missing 0; no gap file. | abd9f2b |
| G1 | T8A/T8B gap fixes (43 PKG-DONE rows: T-09, U-101 replay, U-49, U-68, C-24 v2 cancelled, P-01, M-22, U-11); every product change mutation-checked. U-101 and C-24 stay PARTIAL with honest notes. | 4af3a32 (+ READY 5b4a2c4) |
| G2 | T8C/T8D gap fixes + the 15 auth-status rows: C-24 closing report, M-130 v2 index negotiation, M-131 live-load marker, M-132 failed-start replay, M-133 OS-seeded sidecar names, O-168 excludeDynamicSections, O-169 user-hook merge. | ae5741f |
| INTG1 | G1 cherry-pick integration onto G2; conflicts in `agent.rs`, FEATURES (one C-24 row), MISSING and TEST-MAP resolved with both intents; the G1 closing test moved to `session::send_closing`. | 3758c91 + 8eef6eb |
| G3 | T9A gap fixes (16 rows): M-131 live-load replay marker, C-24 v2 replay diff strip + aborted/interrupted/cancelled statuses, B-113 close withdrawal, B-74/Q-06 Fast-mode restore on a fresh context. | a8802c1 (via INTG3) |
| G4 | T9 reconciliation: all remaining `missing:<id>` rows except G3's acp-v2/clear-context set — F10 21, F25 18, F11 13, T-PORT 11, B-113 6, F34/F21/F14 6 (75 rows: 68 ported, 7 different). | 7eec559, 32481eb, f0eff50, 86bc9f5, 2f2608b, 3e3fbf2, 48c5e14, 6a87e86, 4689c57 |
| INTG3 | G3 cherry-pick integration onto G2+G4; conflicts in `agent.rs` (one shared `loadReplayEnd` send), FEATURES (one row per ID, header recomputed), MISSING and TEST-MAP resolved; G3's closing test uses `send_closing`. | a8802c1 |

### Counts (recomputed from the files, not copied)

- **TEST-MAP**: 2,400 rows — `ported:` 2,087, `covered:` 129, `different:` 184,
  `missing:` 0, `T-UNCLASSIFIED` 0; unmapped 0, stale 0, broken targets 0,
  bad syntax 0. `test-map-check --map --forbid-unclassified` PASS (snapshot and
  real-export halves), and `gate-parity.sh` PASS.
- **FEATURES**: 920 rows (91 module rows), **DONE 905, PARTIAL 15, MISSING 0**.
  Per-section recount from the table cells matches header lines 8-9:
  1 M 106 · 2 U 97 · 3 C 40 · 4 O 141 · 5 S 23 · 6 A 27 / R 9 / N 74 / B 122 ·
  7 T 104 / P 62 · 8 V 21 · 9 Q 91 · 10 D 3.
- **MODULE-MAP**: 107 rows — 1:1 55, merged 31, split 19, different 2,
  `missing:` 0, `not ported:` 0; unmapped 0, stale 0, bad relationship 0,
  bad rust path 0. `module-map-check --map` PASS.

### Gate flip (Decision 7)

- `rust/scripts/parity/gate-parity.sh` now sets `FORBID="--forbid-unclassified"`,
  so both its snapshot half and its real-export half run
  `test-map-check --map --forbid-unclassified`; `rust/scripts/rust-gate.sh`
  (the canonical gate) inherits it through the `==> parity maps` stage.
- Negative check (reverted): one TEST-MAP row was temporarily rewritten to
  `missing:T-UNCLASSIFIED` (with a temporary MISSING.md ticket) — without the
  flag the run warns and PASSes; with the flag it prints
  `UNCLASSIFIED ... (forbidden at this stage)` and exits 1. The real file was
  not modified.
- The checker has no `--forbid-missing` mode (`missing:` rows only warn), so no
  such flag was enabled; MISSING.md is empty because both maps carry zero
  `missing:` rows.
- CI note: this fork has no Rust CI workflow, and `.github/workflows/` is
  upstream-owned (guard G7), so the flip lives in the one canonical gate
  entrypoint (`rust-gate.sh` -> `gate-parity.sh`) that any CI job would run;
  `.github/` was not edited.
- `tests/parity_maps.rs`: `inv_71` now runs `test-map-check --map
  --forbid-unclassified`; `inv_74` rewrites a plain explicit row (the old
  `missing:` seed it relied on no longer exists).

### MISSING.md

- Pruned to an empty table (header + status note). Every ticket whose row left
  `missing:` was removed by the package that ported it: T9A/T9B classification
  and G1..G4 (F10, F25, F11, T-PORT, B-113, F14, F21, F34, C-24 v2 statuses,
  B-74, M-131, M-130..M-133, O-168, O-169). Reality check: TEST-MAP `missing:`
  0, MODULE-MAP `missing:`/`not ported:` 0, FEATURES MISSING 0; both checkers
  PASS with the empty file.

### Housekeeping

- `git rm` of 52 tracked scratch files: 46 `progress-*.md`, `turn-1-progress.md`,
  `T8-gaps-T8{A,B,C,D}.md`, `T9-gaps-T9A.md`. Their content is folded into
  FEATURES/TEST-MAP/STATUS (package rows above).
- `audit-T7.md` kept. Grep confirmed no script under `rust/scripts/`,
  `porting/*.sh` or the READMEs referenced any deleted file; the historical
  STATUS and plan prose references were annotated as removed.
- `progress-T9CLOSE.md` is left untracked (not committed), per the order.

### Remaining PARTIAL FEATURES rows (15, each with a written reason and `[pkg:]`)

| Row | Reason |
|---|---|
| M-118 | v2 untagged extension-method drop is wired and unit-tested, but no production path emits an untagged extension method, so the drop cannot be observed e2e `[pkg:F38b]` |
| U-75 | the forwarder's failed-write rollback is not tied to the failing call, so a late report can undo a later call's records `[pkg:U-75]` |
| U-86 | v2 subagent state validation and the child `state_update` mirror are live but only unit-covered (`t6g_a_replayed_subagent_state_goes_out_after_the_replay`); no e2e scenario drives a subagent on a v2 connection `[pkg:F36]` |
| U-101 | AIR inline-sidechain replay, legacy text filtering and parent stamping are ported/tested; the AIR replay does not read a child's persisted transcript file into the `:replay-subagent:` child `[pkg:G1]` |
| C-01 | everything but `agentInfo.version` matches; the composed version string is the deliberate D7 divergence awaiting the user's decision `[pkg:F2b]` |
| C-24 | v2 cancelled statuses, closing reports and the v2 replay diff strip are done; a streamed live Edit/Write still builds a v1 diff in `tools.rs`, which v2 cannot take `[pkg:F38b] [pkg:G3]` |
| O-01 | `--version/-v/-V` print only the version; only the D7 composed string differs `[pkg:F2b]` |
| O-60 | watcher, 100 ms debounce, `onChange` and `setCwd` are verified (incl. paused time); an active session still reads settings via `settings::load` on a model switch instead of `SettingsManager::getSettings` `[pkg:F5a]` |
| B-07 | `closeCycleToolCalls` exists and is wired on `CycleEnded`; no test exercises the close of an open tool call at a cycle end `[pkg:F16c]` |
| B-114 | a held turn's result and its follow-up running->idle cycle are unit-covered; no e2e scenario drives a cycle Claude Code starts on its own `[pkg:F16c]` |
| T-44 | v1/AIR diffPatch is done; the v2 PostToolUse branch and the v2 no-input-diff rule are not wired because the crate renders v1 and translates to v2 afterwards `[pkg:F9]` |
| V-12 | Windows liveness fails open: `OpenProcess`/`GetExitCodeProcess` need a `windows-sys` feature and a `Cargo.toml` edit, awaiting user decision D11 `[pkg:F26b]` |
| Q-59 | module behaviours complete except the session-side live consumption (O-60) and the auth-probe context (O-67) `[pkg:F5a]` |
| Q-72 | Write/Edit facts, NotebookEdit AIR rendering, created-file diff and the PreToolUse preview hook are done; the `capabilities.v2` branches are not wired because the crate serves v1 tool calls and translates v2 afterwards `[pkg:F9]` |
| D-03 | initialize golden and loud schema check done; `capture.sh --v2` accepts only the initialize name, so no second/third v2 scenario golden can be captured hermetically `[pkg:INFRA-2]` |

### Model log

- Every T9 package (T9A, T9B, G1, G2, G3, G4, INTG1, INTG3) ran on **DeepSeek**
  vst subagents; at most two lanes at once, in disjoint sets with their own
  build dirs. T9A/T9B ran sequentially in this worktree; G1/G3 ran in the
  detached worktree `par-t8c`, G2/G4 in this worktree, and the two INTG packages
  integrated here. No Claude, agy or Opus agents were used in T9 (the user's
  recipe for this turn).
- Session ids were not preserved in the tracked scratch files (the `progress-*`
  files are removed above); the package-to-commit map is the audit trail.

### Carry-over from the T7 audit (still open, unchanged)

- **M4** — B-17's evidence still does not prove the actor wiring: `t7a_b17`
  drains through a local `drain()` that calls the yielder itself, and
  `t7a_b63` passes with the yielder removed because `run_loop`'s `select!` is
  `biased` with commands first; only the standalone yielder test fails without
  the pause.
- **M6** — a cancel during the v1/v2 file-change report preview still delays the
  prompt response up to the 2 s preview timeout (`cancel_collecting` flags and
  publishes; the deferred task still awaits `run_report` before replying).
- **M7** — held v2 emissions can still be overtaken: `settle_events` takes
  `map_state.v2.take_out()` and emits the idle from a spawned task, so the actor
  can process the next prompt/stream frame and emit its `user_message` before
  the previous turn's `idle`.
- **L10** — `TrackedToolCall::record_meta` still compares `Value::to_string()`
  where Node uses `JSON.stringify`; integral floats (`1.0` vs `1`) differ, so a
  repeated meta can emit a spurious update.
- **L13** — two deferred v1 report tasks in one batch still run concurrently
  and release their stale ack early, so responses can leave in
  report-completion order rather than settle order (Node awaits the reports
  sequentially in `settleActive`).

### Checks (this closeout)

- `cargo fmt --check` clean.
- `cargo clippy --workspace --all-targets -D warnings` clean.
- `cargo build --workspace` clean.
- `test-map-check --map --forbid-unclassified` PASS; `module-map-check --map`
  PASS (CARGO_TARGET_DIR=/private/tmp/claude-501/par-t9c-target).
- The full `rust-gate.sh` was deliberately NOT run by this package; the
  orchestrator runs it.

## TURN 9 CLOSED (programs audit)

- Gate OK on e526bda (orchestrator run). TEST-MAP: 2400 rows, 0 missing, 0 unclassified, guard `--forbid-unclassified` enforced by the gate. MISSING.md at zero tickets.
- Programs audit (read-only DeepSeek, reports porting/parity/audit-P1a.md, P1b, P2a, P3): P1a 3 HIGH/5 MEDIUM/6 LOW (false `different:` reasons), P1b 31 findings (weak ported tests, wrong DONE line ranges), P2a 1 HIGH (session start blocking the incoming loop) + MEDIUM race-suite weaknesses, P3 3 HIGH (shared corpus scenarios without assistant text/image, vacuous live-smoke cancel). Fixed by G5a (rows/tests), G5b (session start off the incoming loop, real corpus scenarios, live smoke, race suite), INTG5, G6 (test-side race: wait for the actor exit in close assertions), G7 (G5b regression: forwarded requests answered after the connection began draining; fixed by answering before the drain).
- Fix rounds in T9: G5 (round 1), G6 (round 2), G7 (gate-red regression from G5b, exceeded the cap by exception).
- Carry-over to T10 work orders: any audit item the G5 packages did not close (re-read the four audit reports against the code at the T10 head: P2a F10-F12 LOW, P3 LOW items, P1b LOW items), T7 audit items M4 M6 M7 L10 L13 unless the reports show them closed, and every FEATURES PARTIAL row (see its note).
- Model log: all packages on DeepSeek vst subagents, at most two at a time; no Claude/Opus agents. Programs-audit auditors P1/P2 stalled once (known DeepSeek stall at ~0% CPU) and were respawned with narrower scope and a progress-based stall watcher.

## TURN 10 COMPLETE (final audit) - CANDIDATE HEAD

- Code head 375be16, gate OK (orchestrator run, GATE_TEST_TIMEOUT=1500, quiet host). This section's commit is docs only.
- Counts: FEATURES 920 rows (91 module rows): DONE 905, PARTIAL 15, MISSING 0. TEST-MAP 2400 upstream cases: ported 2102, covered 131, different 170, missing 0 (the `ported` figure counts 2 notes), unclassified 0, `--forbid-unclassified` enforced by the gate. MISSING.md at zero tickets. lib tests: 2108.
- Final audit rounds (all read-only DeepSeek auditors, per-slice, strict "vst-observable" scope from round 3): round 1 (FA1/FA2): 1 HIGH (v2 Edit/Write tool calls dropped), 7 MEDIUM, ~14 LOW; round 2 (FA3/FA3b/FA4): 1 HIGH (verbatimPrompts wire), ~8 MEDIUM, ~14 LOW; round 3 (FA5/FA6/RR1a/RR1b): 2 MEDIUM, 4 LOW; round 4 (RR2): 1 HIGH regression of our own fix (elicitation answer dropped by a steer prune) + 1 LOW; round 5 (RR3): 1 LOW, no HIGH/MEDIUM. All HIGH/MEDIUM fixed with tests that fail before the fix; the last LOW (RR3-01) fixed in FX8. Reports: porting/parity/audit-*.md.
- Fix packages beyond the 2-per-turn cap were allowed by the vibe-station session for the final audit loop (loop ended by the "no HIGH/MEDIUM vst-observable" rule).
- Incidents: DeepSeek stalls (known ~0% CPU) on P1/P2 audit, FX1, FA3, FX4, RR1: each respawned once with narrower scope and progress-based stall rule; no package stalled twice. FX3 committed rust/claude-agent-acp-rs/src/process.rs, which also contained another session's uncommitted edit (the foreign edit is folded into those commits; the diff was all FA3 env/argv work). The foreign uncommitted .gitignore edit was never committed.
- Model log: every package on DeepSeek vst subagents, at most two at a time; no Claude/Opus agents.
- Remaining PARTIAL FEATURES rows (15): see their notes ([pkg:] ids). Open user decisions: push, tag move, squash, D7 agentInfo.version, D9 url crate, D10 reqwest default-tls, D11 windows-sys, D12 NFC generated tables.
