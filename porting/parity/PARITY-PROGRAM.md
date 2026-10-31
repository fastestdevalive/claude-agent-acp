# PARITY + CONCURRENCY PROGRAM (verbatim spec from the user; same brief goes to the claude port and the codex port)

> Location note for this repo (claude port): the brief names `docs/parity/*`, `docs/PARITY-PROGRAM.md`,
> `docs/DECISIONS-FOR-USER.md` and `scripts/parity/*`. This repo keeps the SAME file names under
> `porting/parity/` and `rust/scripts/parity/` because root `docs/` and `scripts/` are upstream-owned (guard G7);
> see `porting/parity/DECISIONS-FOR-USER.md` D1 and `porting/parity/README.md` for the mapping.

GOAL: make production behaviour differences between our Rust port and its npm counterpart (claude:
@agentclientprotocol/claude-agent-acp 0.94.0; codex: @agentclientprotocol/codex-acp 2.2.2) detectable by tests we
run on every change and every upstream bump. Two parts: (A) a parity test program, (B) a concurrency-fidelity
program, because Node is a single-threaded event loop (code between two awaits is atomic) while the ports run on
tokio (check what runtime flavor you actually use; codex-acp-rust uses plain #[tokio::main] = multi-thread).
Logical races Node cannot have (cancel vs completion, steer vs end-of-turn, reordered notifications, permission
reply between two state checks, close during a prompt) become possible. Rust prevents data races, not these.

CONSISTENCY RULE: both repos use the SAME file names, status vocabulary, script CLIs, scenario names and docs
layout below. Read the other port's checkout to align (claude: ~/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1,
codex: ~/code/fastestdevalive/codex-acp-rust). Where one repo already has a better mechanism (claude has porting/ with
PARITY.md, INVARIANTS.md, a Node-fixture differential test), adapt the other to it rather than inventing a second dialect;
if names must differ, say why in the plan. Copy this brief's spec into docs/PARITY-PROGRAM.md verbatim in each repo
(claude may keep it under porting/ with a pointer).

## (A) PARITY TEST PROGRAM - deliverables in each repo
1. TEST-MAP: `docs/parity/TEST-MAP.md` mapping EVERY upstream test (and every behaviour the upstream tests assert) to
   exactly one status: `ported:<rust test>` | `covered:<corpus scenario>` | `different:<reason>` | `missing:<TODO id>`.
   Upstream test list comes from the upstream git repo at the pinned tag (claude: ~/code/fastestdevalive/claude-agent-acp-original,
   35 files in src/tests; codex: clone agentclientprotocol/codex-acp at the pinned tag into a scratch cache OUTSIDE the repo;
   npm tarballs have no tests). A script `scripts/parity/test-map-check` (same name/CLI in both: `--upstream <path> --tag <tag>`)
   FAILS the gate when an upstream test is unmapped, and warns on `missing`. Resolve every `missing` that affects behaviour vst
   relies on (see vst's findings reports in /Users/gb/.vibe-station/projects/vibe-station/worktrees/vs-275/.vibekit/reports/*port-findings.md);
   leave others as explicit `missing` with a ticket line.
2. DIFFERENTIAL CORPUS: scenario scripts in `corpus/` with a shared name set (plain, tools_shell, edit_diff, write_new,
   permission_allow, permission_deny, cancel_mid_tool, cancel_before_turn, steer_mid_turn, queued_prompt, resume_recall,
   session_load_replay, config_options_model_mode, set_config_option, image_input, slash_commands_skills, usage_update,
   background_after_prompt, auth_failure, session_list_close_delete_fork, initialize_capabilities, mcp_stdio, subagent).
   Node fixtures recorded from the pinned npm version; a replay test diffs normalised update streams. Normalisation rules
   documented once; NO ignore-paths without a justification entry in the doc; any ignore added during this task needs Opus
   sign-off. Codex needs a recording step against the real node adapter + real host codex (use vst's
   scripts/parity/live-acp-codex.mjs in vs-275 as the starting driver); replay must NOT need codex installed.
3. LIVE SMOKE: a gated test (`PARITY_LIVE=1`, skipped when the real CLI is absent) that drives the installed claude / codex
   end to end (initialize, new session, tiny prompt, cancel, resume). Same env gate and test names in both repos.
3b. MODULE-MAP: `docs/parity/MODULE-MAP.md` (same name/format in both repos): one table row per upstream module (claude:
   each file in src/*.ts of the pinned tag, incl. acp-agent.ts split by section/function group; codex: each `// src/<File>.ts`
   section of the pinned dist/index.js or, better, the upstream git source at the pinned tag) -> the Rust module(s) that implement
   it, with relationship `1:1` | `merged:<into>` | `split:<into>` | `not ported:<reason>` | `different:<reason>`. Do NOT restructure
   the Rust code to mirror TypeScript; the map is what makes bumps cheap. Extend scripts/upstream-diff.sh so a bump prints, for
   each changed upstream module, the Rust module(s) to look at, and fail the check when an upstream module has no MODULE-MAP row.
4. BUMP WORKFLOW: extend scripts/upstream-diff.sh so a bump also diffs the upstream TEST files between old and new tags and
   lists new/changed/removed tests, and test-map-check fails until each is mapped. Update the bump docs.

## (B) CONCURRENCY-FIDELITY PROGRAM - deliverables in each repo
1. DECISION (measured, written in docs/parity/CONCURRENCY.md): run the adapter on a single-threaded tokio runtime
   (current_thread, or one LocalSet) so interleavings happen only at await points like Node; benchmark a representative
   session (throughput, latency of streaming chunks, steering) and record numbers. If you keep multi-thread, justify with
   data and add the equivalent guards below.
2. STRUCTURE: per-session state changes go through ONE ordered queue/actor; exactly one stdout writer and one child-stdin
   writer (claude already has INV-8; add the equivalent to codex). No lock held across .await; document the lock order if any
   locks remain. Audit and list every shared mutable state (Arc<Mutex/RwLock>, atomics, channels) in CONCURRENCY.md with the
   Node counterpart variable and why it's safe.
3. RACE SUITE: a deterministic stress test per race (cancel vs result, steer vs end-of-turn, close during prompt, permission
   reply timing, notification ordering, late/duplicate responses, child exits mid-turn), each run >= 200 iterations and, where
   possible, with tokio `start_paused` / loom-style control, asserting the SAME outcome Node produces (derive expected outcomes
   from the Node source and from a recorded Node run, cite the lines).
4. PANIC / EXIT POLICY: the vibe-station workspace builds release with panic = "abort"; check the ports' own profile and decide +
   document what happens to other sessions in the same adapter process when one panics, and that vst gets a clean error rather
   than a silent hang when the adapter dies mid-turn.

DELIVERABLE (per the brief): final commit sha, gate results, TEST-MAP summary counts per status, the corpus list with
record/replay status, CONCURRENCY.md decision with benchmark numbers, race-suite results (iterations and pass counts), and a list
of divergences these new tests exposed (fixed or open).
