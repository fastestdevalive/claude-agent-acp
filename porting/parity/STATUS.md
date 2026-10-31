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
