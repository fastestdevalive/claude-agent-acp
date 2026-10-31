# Turn 1 — progress

## G0 — make the existing uncommitted work tree gate-clean

Scope: formatting only. The PC-1 review fix in `rust/claude-agent-acp-rs/src/map.rs`
(`finish_result_tool_tracker` call + `task_tool_result_releases_the_field_tracker`) and the
TodoWrite repro (`inv_24_todo_perm_settle` + `HAZARD_PENDING`, corpus/fixtures
`todo-perm-settle.*`) were left functionally untouched. The only edit was `cargo fmt`
re-wrapping the long `json!` line in the new map.rs unit test.

`cargo fmt --all` rewrapped:

```rust
let task =
    json!({"type": "tool_use", "id": "toolu_TR1", "name": "TaskUpdate", "input": {}});
```

### Commands (real output)

`cargo fmt --manifest-path rust/Cargo.toml --all` → exit 0 (silent).

`cargo fmt --manifest-path rust/Cargo.toml --all -- --check` → exit 0, printed nothing.

`CARGO_TARGET_DIR=<scratchpad>/target-t1g0 cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`

```
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.06s
CLIPPY_EXIT=0
```

`cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`

```
test result: ok. 290 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.61s
LIB_TEST_EXIT=0
```

New unit test present:

```
test map::tests::task_tool_result_releases_the_field_tracker ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 289 filtered out
```

Repo binaries rebuilt into `rust/target/debug` (the differential suite spawns them from there;
built without `CARGO_TARGET_DIR`): `cargo build --manifest-path rust/Cargo.toml` → `Finished dev`, exit 0.

`CARGO_TARGET_DIR=<scratchpad>/target-t1g0 cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential`

```
test inv_24_todo_perm_settle ... ignored, HAZARD-IMPL T-21-todowrite: settle path (map.rs TodoWrite arm, session.rs permission-surfaced check)
test result: ok. 58 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 14.47s
DIFF_TEST_EXIT=0
```

Notes:

- The ignored test stays ignored in G0, as instructed; the orchestrator handles it in Turn 2.
- `git status --short` still shows exactly the pre-existing set: `.gitignore` (untouched),
  the four `todo-perm-settle.*` files (untouched), `src/map.rs`, `tests/differential.rs`.
- No new `#[ignore]`, no corpus exclusions added by this group.

READY:G0

## G1a — INFRA-1 (parity-infra Phase 1 machinery)

Files created:
- `rust/scripts/parity/fetch-upstream.sh` — `--ver/--offline`, exports tag to
  `rust/target/parity/src-<ver>` with `.tag-name`/`.tag-sha`; exit 3 when the
  tag is unavailable; `PARITY_FORCE_OFFLINE=1` forces exit 3 without git.
- `rust/scripts/parity/scan_tests.awk` — whole-file single-record scanner
  (RS=`\001`), emits `file<TAB>line<TAB>title`; handles multi-line calls,
  `.each/.skipIf/.runIf/.for`, nested describes, template literals (incl. nested
  backticks), `[dynamic#n]`/`[dynamic-describe#n]`, duplicate `#n`, `|`->`¦`,
  regex `.test(` and strings/comments ignored.
- `rust/scripts/parity/selftest/fixture.test.ts` — all extractor constructs.
- `rust/scripts/parity/test-map-check` — modes `--map --list --seed --summary
  --groups --unresolved --reconcile --diff --refresh-snapshot --selftest
  --forbid-unclassified`; env overrides; `ported:` target grammar + real test
  attribute (rejects `#[ignore]`/helpers); `covered:` requires both corpus files;
  default-row rules; `.each`/loop `note` rule; MISSING ticket check.
- `rust/scripts/parity/module-map-check` — `--map --list --summary --sections
  --refresh-snapshot --selftest`; 17 `acp-agent.ts@` anchors computed from the
  export (sequential anchor search so `readTextFile` resolves to the class
  method at 9547, not the interface at 2381); relationship/rust-path/ticket/
  DECISIONS rules.
- `rust/scripts/parity/gate-parity.sh` — snapshot halves always, real-export half
  when the offline export exists, loud SKIP banner on exit 3.
- `porting/parity/{TEST-MAP.md,MODULE-MAP.md,MISSING.md,README.md,scan-allowlist.tsv}`
- `porting/parity/upstream-tests.v0.94.0.tsv` (2,400 rows) and
  `porting/parity/upstream-modules.v0.94.0.tsv` (91 rows).
- `rust/claude-agent-acp-rs/tests/parity_maps.rs` — `inv_70_parity_selftests`,
  `inv_71_parity_maps_pass_offline`, `inv_72_unmapped_test_fails`,
  `inv_73_unmapped_module_fails`, `inv_74_broken_ported_target_fails`.

Files modified:
- `rust/scripts/rust-gate.sh` — one new step `==> parity maps (TEST-MAP +
  MODULE-MAP)` before `==> upstream ahead`.
- `porting/parity/STATUS.md` — "Current work package" only.

Scope note: the work order's explicit file list omitted the two committed
snapshots and `scan-allowlist.tsv`; both are required by Phase 1 items 1.2/1.5
and Decision 2 (the gate fails without them), so they were created under
`porting/parity/`. Plan 1.10's edits to `DECISIONS-FOR-USER.md`,
`porting/INVARIANTS.md` and `porting/PARITY.md` were NOT made (outside the work
order's allowed files); flagged for the orchestrator.

Scanner note: the codex scanner silently dropped the 7 cases in the two
`live-capability-*-audit.test.ts` files because it mishandled the nested template
literal at `live-capability-events-audit.test.ts:69` (`` `${a ? `/${b}` : ""}` ``).
Fixed here with a recursive `skip_tmpl`/`skip_interp`; the export now yields
exactly 2,400 rows (2,336 + 24 + 40).

Choices where the plan was ambiguous (kept the gate green and the maps honest):
- Caches live under `rust/target/parity/cache/` (work order) rather than
  `rust/target/upstream/cache/` (Decision 11); `rust/target/upstream/` untouched.
- `.each`/loop `note` enforcement keys on `[dynamic#n]`/`[dynamic-describe#n]`
  titles and `%s`-style placeholders (the snapshot has no flag column).
- Module `missing:` ids use the exact FEATURES `[pkg:]` owner, which for four
  extension modules is `AG-1..AG-4` (no F-id exists for them).
- A `different:` default is allowed only when DECISIONS names the file; the
  seeded map has no defaults at all.

### Commands (real output)

```
$ bash rust/scripts/parity/test-map-check --selftest | tail -1
SELFTEST: PASS

$ bash rust/scripts/parity/module-map-check --selftest | tail -1
SELFTEST: PASS

$ bash rust/scripts/parity/test-map-check --upstream rust/target/parity/src-0.94.0 --tag v0.94.0 --reconcile
RECONCILE: PASS

$ bash rust/scripts/parity/test-map-check --upstream "$DIR" --tag v0.94.0 --map | tail -2
upstream tests: 2400 | mapped rows: 2400 | unmapped: 0 | stale: 0 | missing(warn): 2400 | unclassified: 2400 | broken targets: 0 | bad syntax: 0
TEST-MAP CHECK: PASS

$ bash rust/scripts/parity/module-map-check --upstream "$DIR" --tag v0.94.0 --map | tail -1
MODULE-MAP CHECK: PASS

$ bash rust/scripts/parity/module-map-check --sections --upstream "$DIR" --tag v0.94.0 | grep -c '^acp-agent.ts@'
17
$ ... | tail -1
acp-agent.ts@runPromptWithCancellation 13532-13804

$ bash rust/scripts/parity/fetch-upstream.sh --ver 0.99.0 --offline; echo $?
3

$ PARITY_FORCE_OFFLINE=1 bash rust/scripts/parity/gate-parity.sh 2>&1 | grep -c 'PARITY REAL-EXPORT SKIPPED'
1   (exit 0)

$ cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test parity_maps
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.02s

$ cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
CLIPPY_EXIT=0

$ cargo test ... --lib
test result: ok. 290 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo test ... --test differential
test result: ok. 58 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out

$ bash rust/scripts/rust-gate.sh 2>&1 | tail -6
MODULE-MAP CHECK: PASS
PARITY: PASS
==> upstream ahead (non-failing)
upstream is 1 versions ahead of pinned 0.94.0 (latest 0.95.0)
==> gate OK
GATE_EXIT=0
```

`git status --short` (G7): only `rust/`, `porting/` and the pre-existing
`.gitignore` change; the G7 filter command prints nothing. No new `#[ignore]`,
no new corpus exclusions, no `src/*.rs` changes.

READY:G1a

## G1b — ROADMAP F1 (evidence kit and recorder/fake extensions)

Files changed (all inside the group's scope):
- `rust/acp-recorder/src/script.rs` — `Step::Initialize.client_capabilities:
  Option<Value>` (whole object, additive) and `Step::ElicitReply { response }`;
  unit tests `parses_initialize_with_whole_client_capabilities`,
  `parses_initialize_with_both_capability_forms`, `parses_elicit_reply_actions`.
- `rust/acp-recorder/src/recorder.rs` — builds a `ClientCapabilities` from the
  whole object, merges the legacy explicit `_meta` over it, and queues
  agent->client `elicitation/create` requests for the script's `elicit_reply`
  step (30 s bound; cancel reply when no script is listening).
- `rust/acp-recorder/examples/echo_agent.rs` — elicits only when the client
  advertised `elicitation.form`.
- `rust/acp-recorder/tests/recorder.rs` —
  `inv_24_recorder_full_capabilities_and_elicit_reply`: asserts the whole
  capability object reached the agent, the `elicitation/create` frame, and the
  scripted accept response.
- `rust/fake-claude/src/lib.rs` — `FAKE_CLAUDE_ENV_ALLOWLIST` (6 names; no value
  ever printed or persisted), `{"segment": N}` transcript markers +
  `next_segment_index` (`$FAKE_CLAUDE_CURSOR` advances the spawn ordinal),
  `replay_segment`, and `$REQ.<n>` backward indexing of earlier matched frames
  via `substitute_with`.
- `rust/fake-claude/src/main.rs` — reads only allow-listed names; replays the
  segment chosen for this spawn.
- `rust/fake-claude/tests/fake_claude.rs` — unit/integration tests for the
  allowlist, segment parsing/slicing, multi-spawn (with and without the cursor),
  and `$REQ.1`/`$REQ.2`.
- `porting/evidence/CONTROL-SUBTYPES.md` (new) — evidence for
  `mcp_authenticate`, `remote_control`, `rename_session`,
  `generate_session_title`, `get_usage`, `request_user_dialog`, `elicitation`,
  with request fields (name/type/required), response shapes, `sdk.mjs`
  line:column references and pinned-upstream adapter line references.

### CONTROL-SUBTYPES.md extraction method

Minified `sdk.mjs` has 169 lines, so each reference is `line:column`. Each
subtype was located with `grep -o '.\{0,180\}<name>.\{0,220\}'` (grep caps
repetition at 255) and then the full function body was read with
`awk 'NR==<line> {print substr($0, <col>, <len>)}'`. Field types/requiredness
come from the same package's `sdk.d.ts` where declared; where a runtime method
is **not** declared (`mcpAuthenticate`, `enableRemoteControl`, `renameSession`,
`generateSessionTitle`) the doc says so and cites the adapter's own optional
shape. Inbound semantics (`Pc` suppress symbol, `Z4` never-answered list,
`{action:"decline"}` for a handler-less elicitation, silence for a handler-less
`request_user_dialog`, `Unsupported control request subtype: X`) are recorded in
the doc's last two sections (F3a consumes them).

### `$REQ.n`

The F1 card's `$REQ.n` helper was already implemented by the earlier session
(`substitute_with` + `history` in `replay_segment`) and covered by
`req_placeholder_indexes_backwards` and
`multi_spawn_*`. No further `$REQ` helper was required: the recorder scripts
(`porting/corpus/*.acp.json`) contain no `$REQ` use at all — it is a
fake-claude-transcript placeholder, not an ACP-script one.

### Commands (real output)

`cargo build --manifest-path rust/Cargo.toml --example echo_agent` (no
`CARGO_TARGET_DIR`, needed by `tests/recorder.rs`)

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.04s
BUILD_EXIT=0
```

`cargo test --manifest-path rust/Cargo.toml -p acp-recorder -p fake-claude`

```
test result: ok. 32 passed; 0 failed; 0 ignored  (acp-recorder lib)
test result: ok. 10 passed; 0 failed; 0 ignored  (fake-claude lib)
test result: ok. 2 passed; 0 failed; 0 ignored   (recorder.rs integration)
test result: ok. 16 passed; 0 failed; 0 ignored  (fake-claude.rs integration)
test result: ok. 5 passed; 0 failed; 0 ignored   (phase2.rs)
TEST_EXIT=0
```

`cargo test --manifest-path rust/Cargo.toml -p acp-recorder --test recorder`

```
running 2 tests
test inv_24_recorder_initialize_order ... ok
test inv_24_recorder_full_capabilities_and_elicit_reply ... ok
test result: ok. 2 passed; 0 failed; 0 ignored
```

`cargo test --manifest-path rust/Cargo.toml --workspace`

```
32 + 3 + 2 + 290 + 7 + 1 + 58 + 1 + 5 + 5 + 3 + 1 + 1 + 2 + 4 + 10 + 16 + 5
= 446 passed; 0 failed; 1 ignored (inv_24_todo_perm_settle, unchanged)
WS_EXIT=0
```

`CARGO_TARGET_DIR=<scratchpad>/target-t1g1b cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.37s
CLIPPY_EXIT=0
```

`cargo fmt --manifest-path rust/Cargo.toml --all -- --check` → exit 0 (silent).

`cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential`

```
test result: ok. 58 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
DIFF_EXIT=0
```

`bash rust/scripts/rust-gate.sh`

```
TEST-MAP CHECK: PASS
MODULE-MAP CHECK: PASS
PARITY: PASS
==> upstream ahead (non-failing)
upstream is 1 versions ahead of pinned 0.94.0 (latest 0.95.0)
==> gate OK
GATE_EXIT=0
```

`git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'`

```
0
```

FEATURES.md: no row flipped. F1 has no open row tag in FEATURES.md, and every
item delivered here is tooling/evidence (recorder steps, fake-claude replay
modes, a control-subtype reference doc) rather than adapter behaviour, so no
row's gap is exactly closed by this group.

`git status --short` shows only `rust/`, `porting/` (incl. the new
`porting/evidence/`) and the pre-existing `.gitignore` change; no new
`#[ignore]`, no corpus/fixture edits, no `src/**` changes.

READY:G1b

## G2a — PC-1 leftovers (U-26, T-22, T-23, T-25, T-77, PC-1.2 remainder)

Files changed (only the group's scope):
- `rust/claude-agent-acp-rs/src/hooks.rs`
  - `parse_task_create_output` (303-329) now mirrors Node `parseTaskCreateOutput`:
    structured `{"task":{"id":…}}` in an object, a JSON string, or text blocks
    (skipping unrelated JSON), then the text form with Node's regex checks
    (`\S+` id, non-empty single-line subject).
  - `apply_task_create` (331-335) uses only the output id; the `input.taskId`
    fallback is gone and an empty id is rejected (Node `if (!taskId)`).
  - `apply_task_tool_result` (676-690) is the single Task* tool-result entry:
    TaskCreate (tool_use_result preferred, plan always published), TaskUpdate
    (success gate), TaskList (strict, output order), TaskGet/others silent.
  - 8 new unit tests (lines 1294-1510).
- `rust/claude-agent-acp-rs/src/session.rs`
  - `publish_task_plan` (1881-1900) gates the dedupe on `ext.air.is_some()`
    (the parsed `_meta.jetbrains.air` object), not on an `air` capability.
  - TaskCreated (2130-2161) rejects an empty `task_id` or `task_subject`;
    TaskCompleted (2162-2175) rejects an empty `task_id`.
  - The tool-result loop (2359-2369) delegates to `apply_task_tool_result`, so
    TaskGet no longer needs a special case and TaskCreate reads
    `tool_use_result` first.
  - 2 new unit tests (3321-3430).
- `porting/parity/FEATURES.md`: U-26, T-22, T-23, T-25, T-77 → DONE with
  file:line evidence on both sides; counts header 269 DONE / 241 PARTIAL /
  403 MISSING; Round-9 note added.
- `porting/parity/STATUS.md`: Current work package → T1-G2a.
- `.vibekit/feature-plans/pending/parity-closeout/plan-parity-closeout.md`:
  PC-1.5 ticked; PC-1.1/PC-1.2 notes made honest.

Rows flipped: U-26, T-22, T-23, T-25, T-77 (all DONE).

### PC-1.2 remainder

`HookRegistry::clear` does NOT exist at HEAD 779835d — it was already removed
as dead code by an earlier commit (the closeout plan's OPEN note says so). No
test referenced it, so there was nothing to wire or delete. I left the Node
teardown `clearHookCallbacks` wiring (acp-agent.ts:2798/8202/8304/8468/8500/8882)
as the remaining PC-1.2 gap, owned by F9/F18; the closeout plan now says that
explicitly.

### Failing-first evidence (real output)

The new tests were written before the implementation. The first run failed to
compile because the new helper did not exist:

```
error[E0425]: cannot find function `apply_task_tool_result` in this scope
  --> claude-agent-acp-rs/src/hooks.rs:1322:17
```

The behavioural tests were then checked against the pre-fix code by
temporarily restoring the old gate/fallback (and restoring the fix afterwards):

```
thread 'session::tests::air_plan_dedupe_uses_meta_presence_not_the_capabilities_list'
panicked: the unchanged plan is skipped for an AIR client
  left: 2  right: 1

thread 'session::tests::task_created_hook_rejects_empty_id_or_subject'
panicked: assertion failed: ext.task_state.is_empty()

thread 'hooks::tests::pc1_5_task_create_needs_an_output_id'
panicked: assertion failed: state.is_empty()
```

T-23's structured-JSON-string and text-block assertions likewise cannot pass
against the old parser, which only read `content.pointer("/task/id")` and the
first text block's raw text.

### Commands (real output)

`cargo fmt --manifest-path rust/Cargo.toml --all` → exit 0; `-- --check` → exit 0.

`cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`
(`CARGO_TARGET_DIR=<scratchpad>/target-g2a`) → `Finished`, CLIPPY_EXIT=0.

`cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --lib`
(`CARGO_TARGET_DIR=<scratchpad>/target-g2a`):

```
test result: ok. 300 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
LIB_EXIT=0
```

The 10 new tests:

```
test hooks::tests::pc1_5_task_get_result_is_silent ... ok
test hooks::tests::pc1_5_task_create_needs_an_output_id ... ok
test hooks::tests::pc1_5_task_create_prefers_tool_use_result ... ok
test hooks::tests::pc1_5_task_update_failed_delete_text_output_is_not_applied ... ok
test hooks::tests::pc1_5_task_list_parses_json_string_and_structured_empty ... ok
test hooks::tests::pc1_5_task_list_uses_the_output_order ... ok
test hooks::tests::pc1_5_task_update_parses_json_string_and_prefers_tool_use_result ... ok
test session::tests::task_created_hook_rejects_empty_id_or_subject ... ok
test session::tests::air_plan_dedupe_uses_meta_presence_not_the_capabilities_list ... ok
test hooks::tests::pc1_5_task_create_parses_structured_and_text_output ... ok
```

`cargo build --manifest-path rust/Cargo.toml --workspace --bins` (repo target,
needed by the differential suite) → `Finished`, exit 0.

`cargo test --manifest-path rust/Cargo.toml -p claude-agent-acp-rs --test differential`
(`CARGO_TARGET_DIR=<scratchpad>/target-g2a`):

```
test result: ok. 58 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
DIFF_EXIT=0
```

`bash rust/scripts/rust-gate.sh` (no `CARGO_TARGET_DIR`):

```
TEST-MAP CHECK: PASS
MODULE-MAP CHECK: PASS
PARITY: PASS
==> upstream ahead (non-failing)
upstream is 1 versions ahead of pinned 0.94.0 (latest 0.95.0)
==> gate OK
GATE_EXIT=0
```

JsonPath guard:

```
$ git diff -U0 455c146 -- rust/claude-agent-acp-rs/tests rust/acp-recorder rust/fake-claude | grep '^+' | grep -c 'JsonPath::new'
0
```

`git status --short`: only `.gitignore` (pre-existing, untouched), the two Rust
source files, and the three `porting/`/`.vibekit/` docs. No new `#[ignore]`, no
corpus/fixture edits, no `turn.rs`/`unfinished.rs`/`async_tasks.rs` changes.

Notes/assumptions:
- T-22's permission-surfaced settle was already fixed by 5c977c3
  (`inv_24_task_perm_settle` passes, not ignored), so only TaskGet silence and
  TaskList ordering needed tests.
- `apply_task_tool_result` also keeps Node's "TaskCreate publishes a plan even
  without an output id" behaviour; the state itself stays untouched.
- The TaskCreate input-null edge (`!input` in Node) is not exercised by the
  adapter path (the tool_use always carries an input object); left as-is.

READY:G2a
