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

## D4 — FEATURES.md routine decisions (claude-master FEAT-2 and round-2 audit corrections)

- **What:** (a) B-117 (load/resume fingerprint recreate) is owned by F19, the card that describes it; the independent audit suggested F31. (b) Q-01 (module-level acp-agent.ts) is owned by F2a as a placeholder; its behaviour is owned per row. (c) Duplicate rows merged to one kept row with cross-references, status taken conservatively. (d) O-128 split owners for keys with no ROADMAP card (`forkSession`, `resumeSessionAt`, `skills`, `sessionMirror`) are the nearest card, not card-owned; FX-1 owns the keys with no card.
- **Why:** F19's card matches the behaviour exactly; a module-level row cannot have one behavioural owner; conservative status avoids overstating parity.
- **Alternatives:** B-117 to F31 (follow the audit); keep duplicate rows separate. Both are one-line changes if the user prefers.

## D5 — FEATURES.md round-3 routine decisions (second independent audit of d226db6)

- **What:** (a) An advertisement row (capability in `initialize`) is DONE when the advertisement matches upstream, and its behaviour is a separate row (MISSING or PARTIAL) that carries the gap; the advertisement rows C-05, C-06, C-09 are DONE on that basis. A capability that advertises more than is served (C-12) stays PARTIAL. (b) Q-01 (module-level acp-agent.ts) is a roll-up row owned by INFRA-3, closed by the final flip; it is not owned by F2a. (c) WB-05 follows M-52 (F2c) and WB-07 follows the AIR builders (F35b).
- **Why:** an advertised capability whose behaviour is missing is one gap, reported once, in the behaviour row; a module-level row cannot be owned by one behaviour package.
- **Alternatives:** make C-05, C-06, C-09 PARTIAL (count the gap twice); give Q-01 a roll-up status computed from sections 1-8.

## D6 — FEATURES.md round-4 routine decisions (third independent audit of b010c1d)

- **What:** (a) Amends D5: an advertisement row whose methods are unserved is PARTIAL (C-14 goal, C-49 providers); an advertisement whose behaviour is served or separately tracked is DONE (C-05, C-06, C-09). (b) U-26 and P-10 were downgraded to PARTIAL as the round-3 audit asked. The round-3 flips of O-10 and O-20 to DONE were refuted by verification against upstream and Rust, so they stay PARTIAL with the verified gap. (c) FX-6 is removed: B-20 is DONE and no row remains for it. (d) Module rows are PARTIAL where a behaviour row they cite is PARTIAL (14 rows), so module status and behaviour status agree.
- **Why:** an unserved advertisement is a live wire gap and must not read as DONE; a refuted DONE claim must not be published; a module row cannot be DONE while its behaviour is MISSING or PARTIAL.
- **Alternatives:** keep C-49 as DONE with the gap only in M-128 (one gap, reported once); keep module rows MISSING and add a note.

## D7 — The version string stays composed (`<crate>+upstream.0.94.0`), not the plain npm pin

- **What:** C-01 (`agentInfo.version`) and O-01 (`--version`/`-v`/`-V`) report
  `<CARGO_PKG_VERSION>+upstream.0.94.0` (`build.rs`, `agent.rs`), while the Node
  adapter reports the bare package version `0.94.0`. The differential normalises
  the fixture side to the composed value; this is a deliberate divergence
  awaiting a user decision, not a port gap.
- **Why:** the composed string keeps the Rust crate's own version and the pinned
  upstream tag visible in one place; collapsing it to `0.94.0` would make the
  Rust crate claim the npm package's identity. The Node fixture remains
  authoritative for every other field.
- **Alternatives:** print the bare pin (`0.94.0`) from the binary and
  `agentInfo.version` to match Node byte-for-byte (one-line change in `build.rs`
  and the normaliser in `tests/common/mod.rs`).

## D8 — Unsafe code exemption for Windows Job Object and assignment window limitation

- **What:**
  1. Amends D12 / R33 **for Windows only**: the crate keeps `#![forbid(unsafe_code)]` on every
     non-Windows target (`#![cfg_attr(not(windows), forbid(unsafe_code))]`). On Windows the
     crate-level lint is `#![cfg_attr(windows, deny(unsafe_code))]` with `#[allow(unsafe_code)]`
     strictly scoped to the private `windows_job` module inside `src/process.rs`
     (`cfg(windows)`) for Win32 Job Object FFI (`CreateJobObjectW`, `SetInformationJobObject`,
     `AssignProcessToJobObject`, `TerminateJobObject`, `CloseHandle`).
  2. Documents the Windows Job Object assignment window: descendants spawned between
     `Command::spawn()` and `AssignProcessToJobObject` escape `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
     Windows coverage is therefore NOT claimed to be complete; the Unix reaper path is unaffected.
- **Why:**
  1. No safe wrapper crate was present in `Cargo.lock` (`win32job` and similar are not locked;
     adding one would change the dependency set). Windows Job Objects are required on Windows to
     ensure child process trees are terminated on host death. In Rust, crate-level `forbid` cannot
     coexist with an inner module `allow` (compiler error E0453). Keeping `forbid` on every
     non-Windows target and `deny` + one scoped `allow` on Windows is the narrowest exemption that
     compiles. The exemption is enforced, not just documented: `rust/scripts/rust-gate.sh` fails
     if any `unsafe` token appears outside `src/process.rs`, if `allow(unsafe_code)` appears more
     than once or is not the attribute of the private `windows_job` module, or if any `unsafe`
     token in `process.rs` lies outside that module; the bin target
     (`src/bin/claude-agent-acp-rs.rs`) is its own crate root and carries
     `#![forbid(unsafe_code)]`.
  2. Standard library and `tokio::process::Command` do not support `CREATE_SUSPENDED` or
     `PROC_THREAD_ATTRIBUTE_JOB_LIST` without replacing `Command::spawn` entirely with raw Win32
     `CreateProcessW` FFI.
- **Alternatives:** Add an external safe wrapper crate (e.g. `win32job`) to `Cargo.toml`; or rewrite
  Windows process spawning using raw `CreateProcessW` FFI with `CREATE_SUSPENDED` (closing the
  assignment window); or accept the documented window.

## D9 — The `url` crate is a direct dependency (WebFetch hostname option)

- **What:** `claude-agent-acp-rs` depends on `url = "2"` (WHATWG URL parser; transitively `idna`, `percent-encoding`, the ICU data crates). It is used only to compute the hostname of the WebFetch "don't ask again for <hostname>" permission option, exactly as upstream does with `new URL(..).hostname`.
- **Why:** a hand-rolled parser mis-reads `https://evil.com\@trusted.com/` (WHATWG treats the backslash as a path separator, so the fetched host is `evil.com`; the earlier Rust code offered and saved `domain:trusted.com`). Opus review of Turn 1 rated this HIGH. Tabs/newlines, invalid ports and IDN (punycode) also need the real algorithm. The crate was already in the local cargo registry cache (resolved with `--offline`).
- **Alternatives:** keep a hand-written parser that refuses anything unusual (no option offered) — smaller dependency tree, but then IDN and exotic URL forms diverge from upstream permanently. Say so and it is a contained follow-up (`permission_options.rs` only).
- **Guard G10 is unaffected:** it covers only `agent-client-protocol`, which stays at 2.1.0.
