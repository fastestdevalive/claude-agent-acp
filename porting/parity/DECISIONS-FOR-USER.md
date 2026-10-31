# Decisions for the user (claude port)

Each entry: what, why, alternatives. The conservative option that keeps parity with upstream was taken;
nothing here blocks work.

## D1 — Parity-program files live under `porting/parity/` and `rust/scripts/parity/`, not `docs/` and `scripts/`

- **What:** the cross-repo brief names `docs/parity/{TEST-MAP,MODULE-MAP,CONCURRENCY,STATUS}.md`,
  `docs/DECISIONS-FOR-USER.md`, `docs/PARITY-PROGRAM.md` and `scripts/parity/test-map-check`.
  This repo uses the same file names at: `porting/parity/{TEST-MAP,MODULE-MAP,CONCURRENCY,STATUS}.md`,
  `porting/parity/DECISIONS-FOR-USER.md`, `porting/parity/PARITY-PROGRAM.md` and
  `rust/scripts/parity/test-map-check` (+ `rust/scripts/upstream-diff.sh`). The shared corpus stays at
  `porting/corpus/` with fixtures in `porting/fixtures/` (the existing Node-fixture differential).
- **Why:** root `docs/` and `scripts/` are upstream-owned files; guard G7 forbids this fork from touching
  anything outside `rust/`, `porting/`, `skills/`, `.vibekit/` (+ root `.gitignore`). The brief itself allows
  claude to keep these under `porting/`.
- **Alternatives:** relax G7 for `docs/parity`, `docs/DECISIONS-FOR-USER.md`, `scripts/parity`
  (needs a change to `rust/scripts/rust-gate.sh` and the `g7_upstream_untouched` test). Say so and it is a
  small follow-up.

## D2 — The pinned upstream source is read from the `upstream` git remote, not `~/code/fastestdevalive/claude-agent-acp-original`

- **What:** that clone is at `v0.78.0`, not the pinned `v0.94.0`. Test and module maps are generated from a
  scratch export of tag `v0.94.0` fetched through this repo's `upstream` remote.
- **Why:** the clone belongs to the user; checking out another tag would change it.
- **Alternatives:** check out `v0.94.0` in that clone (one command) if wanted.

## D3 — Model policy for implementation turns

- **What:** standing-goal rule: deepseek (`mode-1792593073043-0d981`) for implementation turns; agy-medium
  only occasionally or after two failed deepseek respawns; every substitution logged in `STATUS.md`.
  The parity program text says "stop and ask after two failed respawns"; the standing-goal text says
  "use agy-medium and note it" and "never stop to ask" — the later, more specific standing-goal rule is applied.
