# Upstream bump runbook

How to advance the Rust port from one mirrored npm version of
`@agentclientprotocol/claude-agent-acp` to the next.

## 0. Single source of truth

- `porting/UPSTREAM_VERSION` (one line, `MAJOR.MINOR.PATCH`) is the ONLY pin.
  `build.rs`, `capture.sh`, `upstream-ahead.sh`, `upstream-diff.sh` and
  `upstream-divergence.sh` all read it.
- Committed lockfiles live in `porting/upstream-locks/<pin>.package-lock.json`.
  **Commit the new one together with the bump** so transitive deps cannot drift.
- A bump is a single, dedicated change; do not bundle it with feature work.

## 1. See how far behind we are

```sh
bash rust/scripts/upstream-ahead.sh
```

Prints `upstream-ahead: pinned <pin> is the latest stable version` or the list of
newer stable versions; it never fails (offline prints a skip notice).

## 2. Inspect the package delta and changelog

```sh
bash rust/scripts/upstream-diff.sh <pin> <new>
```

Read the changelog section and the dist-file diff. (Example run, pin 0.94.0:
`bash rust/scripts/upstream-diff.sh 0.93.0 0.94.0` ends with the hints
`git diff v0.93.0 v0.94.0 -- src` and `bash rust/scripts/upstream-divergence.sh 0.94.0`.)

## 3. Inspect the source delta

```sh
git fetch upstream --tags      # remote `upstream` = agentclientprotocol/claude-agent-acp
git diff v<pin> v<new> -- src
```

(`git diff v0.93.0 v0.94.0 --stat -- src` was run against the real tags while
writing this runbook.)

## 4. Preview the frame-level divergence (before touching anything)

```sh
bash rust/scripts/upstream-divergence.sh <new>
```

This captures <new> into `rust/target/upstream/divergence-<new>/` and prints every
fixture frame that differs from the committed `porting/fixtures` (`## <file>`
headings followed by `[i] <path>: <old> -> <new>` lines). Run with the CURRENT
pin it must show no difference lines and `diff -r porting/fixtures
rust/target/upstream/divergence-<pin>` prints nothing (verified for 0.94.0).

## 5. Edit the pin

```sh
printf '<new>\n' > porting/UPSTREAM_VERSION
```

## 6. Port the divergences per class

Work the regenerated differential (it turns red) plus `research` §1. Class rules:

- `PORT-E` / `PORT-N` — port into `rust/`
- `CLI` — match the argv/stream-json the Node adapter speaks to `claude`
- `SKIP` — record in `porting/PARITY.md` section B with its release range
- Escape valve: an item with NO corpus-visible effect may be deferred ONLY by a
  `pending` row (reason + consequence) in `porting/PARITY.md`. A divergence
  visible in a regenerated fixture MUST be ported — if it cannot be, STOP and
  report. No ignore-paths / weakened differentials / skipped fixtures.

## 7. Fix transcripts

Add the steps the new CLI-facing frames require to the affected
`porting/corpus/*.transcript.jsonl`.

## 8. Regenerate fixtures

```sh
bash porting/capture.sh
```

Review `git diff porting/fixtures` — every touched file must be intentional.

## 9. Differential green (3x)

```sh
cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential
```

Run it three times; require `test result: ok` every time.

## 10. Gate

```sh
bash rust/scripts/rust-gate.sh
```

Must end with `==> gate OK`.

## 11. Update the parity ledger

- `porting/PARITY.md` — advance the header version, flip/record rows
- `porting/INVARIANTS.md` — refresh evidence

## 12. Optional CI follow-up

Wiring `upstream-ahead` as a scheduled workflow is out of scope here (needs a G7
exemption for `.github/`).
