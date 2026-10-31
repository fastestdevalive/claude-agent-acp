# porting/parity — parity maps and checkers (claude port)

Parity deliverables for `@agentclientprotocol/claude-agent-acp` **0.94.0**.
The spec is `PARITY-PROGRAM.md`; the roadmap ids are in
`.vibekit/feature-plans/pending/parity-roadmap/ROADMAP.md`.

- `TEST-MAP.md` — every upstream test case -> exactly one status (checked by
  `rust/scripts/parity/test-map-check`).
- `MODULE-MAP.md` — every upstream non-test `*.ts` module (91 files;
  `acp-agent.ts` split into 17 anchor-keyed sections) -> Rust module(s) with an
  exact relationship (checked by `rust/scripts/parity/module-map-check`).
- `MISSING.md` — one ticket line per `missing:<id>`.
- `upstream-tests.v0.94.0.tsv` / `upstream-modules.v0.94.0.tsv` — committed
  snapshots (`file<TAB>title<TAB>hash` / `path<TAB>lines<TAB>sha256`), so the
  gate checks offline.
- `scan-allowlist.tsv` — justified scanner-vs-grep line differences
  (`file<TAB>line<TAB>reason`); only runtime-generated titles are listed.
- `DECISIONS-FOR-USER.md` — policy decisions (D1: these paths, not `docs/`).

## TEST-MAP status vocabulary

Each row is `file | case title | status | note`:

- `ported:<target>` — a Rust test asserts the same behaviour. Target grammar:
  `tests/<file>.rs::<fn>`, `src/<path>.rs::<fn>` (or `::tests::<fn>`), or
  `<crate-dir>/<path>.rs::<fn>` for `acp-recorder` / `fake-claude`. The checker
  requires a real `#[test]`/`#[tokio::test]` fn and rejects `#[ignore]`d tests
  and helpers.
- `covered:<scenario>[+<scenario>]` — both `porting/corpus/<scenario>.acp.json`
  and `<scenario>.transcript.jsonl` must exist.
- `different:<reason>` — deliberate difference with equal observable behaviour
  (reason >= 15 chars); per case only, except a file default when
  `DECISIONS-FOR-USER.md` names that file.
- `missing:<id>` — not mapped yet; `<id>` needs a `MISSING.md` line. Warn only.

Wildcard rows (`*`, `<prefix> > *`) may only be `missing:<id>` (or a
DECISIONS-named `different:`). A default matching zero cases is stale. Rows
whose title is `[dynamic#n]`/`[dynamic-describe#n]` or carries an `.each`
placeholder (`%s`, ...) need a non-empty `note` when `ported:`/`covered:`.

## Checkers

Pure bash 3.2 + awk + git + `shasum -a 256`; no node/python; no network in the
gate.

```sh
bash rust/scripts/parity/fetch-upstream.sh --ver 0.94.0   # -> rust/target/parity/src-0.94.0
bash rust/scripts/parity/test-map-check --upstream "$DIR" --tag v0.94.0 --refresh-snapshot
bash rust/scripts/parity/module-map-check --upstream "$DIR" --tag v0.94.0 --refresh-snapshot
bash rust/scripts/parity/test-map-check --seed             # skeleton (missing:T-UNCLASSIFIED)
bash rust/scripts/parity/test-map-check --upstream "$DIR" --tag v0.94.0 --reconcile
bash rust/scripts/parity/test-map-check --map              # gate (snapshot half)
bash rust/scripts/parity/module-map-check --map
bash rust/scripts/parity/module-map-check --sections       # acp-agent.ts ranges
bash rust/scripts/parity/gate-parity.sh                    # both halves, offline
```

`test-map-check` modes: `--map` (default), `--list`, `--seed`, `--summary`,
`--groups`, `--unresolved`, `--reconcile`, `--diff <old> <new>`,
`--refresh-snapshot`, `--selftest`, `--forbid-unclassified`.
`module-map-check` modes: `--map` (default), `--list`, `--summary`,
`--sections`, `--refresh-snapshot`, `--selftest`.

Env overrides for tests: `TEST_MAP_FILE`, `TEST_SNAPSHOT_FILE`,
`MODULE_MAP_FILE`, `MODULE_SNAPSHOT_FILE`, `MISSING_FILE`, `DECISIONS_FILE`,
`ALLOWLIST_FILE`, `CORPUS_DIR`, `RUST_ROOT`.

`fetch-upstream.sh` prints the export dir only; exit 3 means the tag is
unavailable (offline / no remote), which makes `gate-parity.sh` print the loud
`!!! PARITY REAL-EXPORT SKIPPED` banner. `PARITY_FORCE_OFFLINE=1` forces that
path without touching git (tests only). Generated caches live under
`rust/target/parity/`; `rust/target/upstream/` is never touched.

## Differences from the codex port (`~/code/fastestdevalive/codex-acp-rust`)

| Topic | codex-acp-rust | this repo |
|---|---|---|
| Scripts | `scripts/parity/` | `rust/scripts/parity/` |
| Docs | `docs/parity/` | `porting/parity/` |
| `--upstream` | clone root | export dir with `.tag-name` / `.tag-sha` |
| Cache | `$HOME/.cache/codex-acp-parity` | `rust/target/parity/src-<ver>` (+ `cache/`) |
| Rust test | `tests/parity_test_map.rs` | `tests/parity_maps.rs` |
| `covered:` | kebab -> snake | used as-is; transcript required too |
| `ported:` | allowed on `*` rows; checks `fn <name>` text | wildcard rejected; `#[test]` attribute required, `#[ignore]` rejected |
| Extra | `classify-map.sh` | no classify script |
| New here | — | `module-map-check`, `--groups`, `--unresolved`, `--reconcile`, `--diff`, `--forbid-unclassified`, `note` column, `scan-allowlist.tsv` |

Recommended backport to codex: reject wildcard `ported:`/`covered:` and require
a test attribute on `ported:` targets.

## Current state (Phase 1)

- TEST-MAP: 2,400 cases (2,336 `it`/`test` sites in 58 files + 24 in
  `scripts/next-preview-version.test.mjs` + 40 scenario rows), all seeded
  `missing:T-UNCLASSIFIED`; resolution happens in Phases 2-3.
- MODULE-MAP: 91 modules + 17 `acp-agent.ts` sections = 107 rows; `missing:`
  rows point at the roadmap package ids used by `FEATURES.md`.
- `--reconcile` passes against the 0.94.0 export (scanner line set equals the
  independent grep set; four loop-generated titles are allow-listed).
