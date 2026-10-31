# Upstream sync runbook

Keep the fork's `parity` branch at parity with an upstream **release tag**, advancing tag-by-tag.
A `pending` row in `porting/PARITY.md` is the only thing that blocks a parity claim (D3).

## Workflow (5 steps)

1. **Take the next upstream tag only — never `upstream/main`** (D1, D11).
2. `git diff vOLD..vNEW -- src/` — inspect the delta between the two tags.
3. **If it touches a `ported` row of `porting/PARITY.md`, port the delta** into `rust/`.
   A delta to a `skipped-deliberate` row is re-verified, not ported; a delta to a `pending` row is ignored.
4. **Re-capture fixtures and run the differential** — `porting/capture.sh` then the differential harness
   against the new tag's Node adapter.
5. **Merge the tag into `parity`**, bump the `+acp.0.N` build metadata in the crate version,
   update `porting/PARITY.md` (advance the version in the header, flip any newly-completed rows),
   and tag `rust-vX+acp.0.N`.

## First run

`v0.70.0 → v0.79.0` (R18) — the crate is at `0.1.0+acp.0.70.0`; this cycle brings it to `0.1.0+acp.0.79.0`.
