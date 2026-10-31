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

`rust-parity-fixes` Phase 3 (hooks + PostToolUse/toolResponse, Task plan, `session/load` replay,
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
