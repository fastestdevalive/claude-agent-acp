# Upstream sync

The mirror pin lives in ONE file: `porting/UPSTREAM_VERSION`.

Bumping it, regenerating fixtures and porting divergences follows
[`porting/UPSTREAM-BUMP.md`](UPSTREAM-BUMP.md) — step by step, exact commands.

The old `+acp.0.N` in the crate version and the tag-by-tag workflow here are
obsolete; the version string is now composed from `porting/UPSTREAM_VERSION` by
`build.rs`.

See `rust/scripts/upstream-ahead.sh` (gate tail) and
`rust/scripts/upstream-divergence.sh` (frame-level preview).
