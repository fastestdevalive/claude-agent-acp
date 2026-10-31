# G7b — F5b model policy + cross-session context-window cache

Worktree: `/private/tmp/claude-501/par-b` (HEAD 6b9f98f)
Status: **READY:G7b**

## Files changed
- `rust/claude-agent-acp-rs/src/session_model.rs` (new) — 1:1 port of
  `session-model.ts` (matching/resolution/allowlist/overrides/denied/gateway
  narrowing) plus the `contextWindowCache` / `providerCacheKeyFor` /
  `immediateContextWindow` / `stillCurrent` helpers from `acp-agent.ts`.
- `rust/claude-agent-acp-rs/src/session_config.rs` — model helpers moved out and
  re-exported; `SessionConfigState.provider_cache_key` added.
- `rust/claude-agent-acp-rs/src/settings.rs` — `availableModels` (source-order
  union+dedupe), `modelOverrides`, managed-only `deniedModels`.
- `rust/claude-agent-acp-rs/src/session.rs` — model policy wiring at session
  creation (priority + setModel skip/switch-to-listed + advisory failure),
  cache-aware context-window seeding at creation and on switch, `stillCurrent`
  guard, result-usage cache seeding, `ModelSwitchFailed` error variant.
- `rust/claude-agent-acp-rs/src/usage.rs` — `provider_cache_key` on `UsageState`.
- `rust/claude-agent-acp-rs/src/agent.rs` — refresh tagged with the model id,
  cache cleared on logout, `ModelSwitchFailed` → `Internal error` + `data.details`.
- `rust/claude-agent-acp-rs/src/lib.rs` — module registration.
- `rust/claude-agent-acp-rs/tests/differential.rs` — `f5b_model_policy_corpus`.
- corpus/fixtures: `config-model-allowlist`, `config-model-overrides`,
  `config-model-denied`, `config-model-hook-veto`, `config-model-switch-failure`
  (all generated with `bash porting/capture.sh`).
- `porting/parity/FEATURES.md` rows flipped, `porting/parity/TEST-MAP.md` cases
  marked.

## Rows flipped (all `[pkg:F5b]`)
DONE: U-18, O-20, O-34, O-35, O-36, O-39, O-40, O-41, O-42, O-43, O-44, O-45,
O-46, O-47, A-05, A-06, A-07, A-10, N-30, B-69, B-70, B-71, Q-55.
PARTIAL: O-74 (narrowing implemented + wired; the `providers/set` listing source
arrives with F33, so the picker only narrows once F33 lands).

## Notes
- **PreModelSwitch hook callback (hooks.rs):** upstream registers no
  `PreModelSwitch` callback — the veto is a CLI-side hook that rejects
  `setModel` with `"Model switch blocked by a PreModelSwitch hook: …"`, which
  `is_pre_model_switch_hook_block` recognizes. `hooks.rs` therefore needed no
  change. The advisory path and the loud non-hook failure are both captured
  (`config-model-hook-veto`, `config-model-switch-failure`), so no unit-only
  fallback was needed.
- **`config-model-denied`:** `capture.sh` seeds only under `CLAUDE_CONFIG_DIR`
  (the user tier), and the managed settings path is OS-fixed, so the managed
  tier cannot be seeded for a Node capture. The corpus therefore proves the
  managed-only rule on the wire (a user-tier `deniedModels` is ignored); the
  managed-tier filtering itself is covered by
  `settings::tests::f5b_denied_models_managed_only` and
  `session_model::tests::f5b_denied_models_family_and_prefix`.
- **Cache implementation:** the workspace clippy config disallows
  `std::sync::Mutex`/`RwLock`, so the module-global cache lives on a dedicated
  thread behind `std::sync::mpsc`; reads stay synchronous like Node's `Map`.
- `ANTHROPIC_CUSTOM_MODEL_OPTION` is read from the env snapshot (hermetic);
  the custom-option path is unit-tested (no corpus env value needed).
- turn.rs / turn tests untouched.

## Commands run (all from `/private/tmp/claude-501/par-b`,
`UPSTREAM_CACHE_DIR` pointed at the read-only oracle cache)
- `cargo fmt --all` + `--check` — PASS
- `cargo clippy --workspace --all-targets -- -D warnings` — PASS (no allows)
- `cargo test -p claude-agent-acp-rs --lib` — PASS (381 passed)
- `cargo test -p claude-agent-acp-rs --test differential` — PASS
  (65 passed, 1 ignored; includes `f5b_model_policy_corpus` over 5 scenarios)
- `cargo test --workspace` — PASS
- `bash rust/scripts/rust-gate.sh` — PASS (`==> gate OK`)
- JsonPath guard (`git diff -U0 455c146 ... | grep -c 'JsonPath::new'`) — 0
- `bash rust/scripts/parity/test-map-check` — PASS
