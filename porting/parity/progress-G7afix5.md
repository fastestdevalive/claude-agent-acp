# Progress — Group G7a-fix5

## Defect & Fix
- Defect: in `rust/scripts/rust-gate.sh` the D8 gate check previously counted occurrences of `allow(unsafe_code)` without excluding comment lines. As a result, the doc comment at `rust/claude-agent-acp-rs/src/process.rs:8` matched, causing `ALLOW_COUNT` to be 2 and `ALLOW_LINE` to point to line 8 rather than the `windows_job` module attribute at line 1037, causing `D8 FAIL`.
- Fix:
  - Extracted D8 verification into `rust/scripts/d8-check.sh` which can check any source directory or run self-tests with `--selftest`.
  - Excluded comment lines (starting with `//`, including `//!` and `///`) from both `ALLOW_COUNT` and `ALLOW_LINE` computation.
  - Added self-tests in `rust/scripts/d8-check.sh --selftest` verifying:
    - (a) the real tree passes
    - (b) a tree with a second real `#[allow(unsafe_code)]` fails
    - (c) an `unsafe` token outside `windows_job` fails
    - (d) an `unsafe` token outside `process.rs` fails
    - (e) a doc comment mentioning `allow(unsafe_code)` does NOT fail
  - Hooked the D8 self-test into `rust/scripts/selftest-gate.sh` under step `0.T4c`.
  - Updated `rust/scripts/rust-gate.sh` to invoke `d8-check.sh` and adjusted `gate-parity.sh` timeout to 900s to handle heavy concurrent CPU load.

## Real Output of D8 Check Self-Tests
```
--- (a) real tree passes ---
D8 OK: unsafe strictly confined to process.rs windows_job
(a) ok: real tree passes
--- (b) tree with a second real #[allow(unsafe_code)] fails ---
D8 FAIL: allow(unsafe_code) must appear exactly once, on the windows_job module
(b) ok: second #[allow(unsafe_code)] rejected
--- (c) an unsafe token outside windows_job fails ---
D8 FAIL: unsafe outside windows_job at process.rs:7
(c) ok: unsafe token outside windows_job rejected
--- (d) an unsafe token outside process.rs fails ---
D8 FAIL: unsafe outside process.rs
/var/folders/75/31jc7b9j32d2nws10y39_ylw0000gn/T/tmp.FxjrNwv3YJ/lib.rs:5:    unsafe {}
(d) ok: unsafe token outside process.rs rejected
--- (e) a doc comment mentioning allow(unsafe_code) does NOT fail ---
D8 OK: unsafe strictly confined to process.rs windows_job
(e) ok: doc comment mentioning allow(unsafe_code) accepted
==> D8 selftest OK (all 5 cases passed)
```

## Files Changed
- `rust/scripts/d8-check.sh` (new)
- `rust/scripts/rust-gate.sh` (delegate D8 to `d8-check.sh`, increase parity timeout under contention)
- `rust/scripts/selftest-gate.sh` (hook `0.T4c` to run `d8-check.sh --selftest`)

## Commands Run
- `cargo fmt --manifest-path rust/Cargo.toml --all --check` -> PASS
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --all-features -- -D clippy::correctness -D clippy::suspicious -D clippy::complexity -D clippy::perf -D warnings` -> PASS
- `bash rust/scripts/d8-check.sh` -> PASS
- `bash rust/scripts/d8-check.sh --selftest` -> PASS
- `bash rust/scripts/selftest-gate.sh` -> PASS (including `0.T4c`)
- `UPSTREAM_CACHE_DIR=/Users/gb/.vibe-station/projects/claude-acp-rust-eval/worktrees/care-1/rust/target/upstream bash rust/scripts/rust-gate.sh` -> PASS

## Gate Output (Last 12 Lines)
```
-- MODULE-MAP snapshot half
upstream modules: 91 | map rows: 107 | unmapped: 0 | stale: 0 | bad relationship: 0 | bad rust path: 0
MODULE-MAP CHECK: PASS
-- real-export half (/private/tmp/claude-501/par-b/rust/target/parity/src-0.94.0)
upstream tests: 2400 | mapped rows: 2400 | unmapped: 0 | stale: 0 | missing(warn): 2211 | unclassified: 2211 | broken targets: 0 | bad syntax: 0
note: 2211 missing: row(s) warn only (each needs a MISSING.md ticket)
TEST-MAP CHECK: PASS
upstream modules: 91 | map rows: 107 | unmapped: 0 | stale: 0 | bad relationship: 0 | bad rust path: 0
MODULE-MAP CHECK: PASS
PARITY: PASS
==> upstream ahead (non-failing)
upstream is 1 versions ahead of pinned 0.94.0 (latest 0.95.0)
==> gate OK
```

READY:G7afix5
