# Findings behind `plan-parity-closeout.md` (Opus review of parity Phase 3, 2026-10-23)

| # | Area | Finding | Status |
|---|---|---|---|
| 1 | P7 ExitPlanMode | "Yes, manually approve edits" (`exit-plan-default`) denied: option id unknown to `apply_common_selection`; Node also runs `setMode`; "keep planning" must interrupt | selection handling fixed in `aff399a`; rendering divergence + corpus: PC-3 |
| 2 | P7 offered-option guard | Node `response.js:5` rejects an option that was not offered (Bash path-form suggestions, matched ask rules) | fixed in `aff399a` |
| 3 | P7 env | `std::env::var("HOME")` outside `env_snapshot.rs` | fixed in `aff399a` (env_guard test) |
| 4 | P7 minor | SandboxNetworkAccess / computer-use branches; IPv6 host rendering | IPv6 fixed in `aff399a`; remaining branches: PC-4 `pending` rows or port |
| 5 | P9 hooks | PostCompact ignored; PostModelSwitch raw write; hook-pending derivation; `prune_expired` onRelease; vacuous test; error string; re-register; multibyte slice | PC-1.1-PC-1.4 |
| 6 | Task* | status default; success gate; `tool_use_result`; TaskList strictness; permission-surfaced resolve | PC-1.5 |
| 7 | P10 replay | `isMeta` origin exceptions; `isCompletedLocalCommand`; leaf walk; blocking read; Task plan on replay | PC-2 |
| 8 | Docs | PARITY rows 51, 63, 79, 178, 196, 199, 214 stale/contradictory | PC-4.1 |
| 9 | Honesty | item 3.0 ticked "LIVE tee" without a tee | PC-1.7 |
