# MISSING — open parity tickets (module rows + TEST-MAP ids)

Every `missing:<id>` in `MODULE-MAP.md` / `TEST-MAP.md` needs a line here.
Module ids are the roadmap package ids used by `porting/parity/FEATURES.md`
(`F*`, `AG-*`); `T-UNCLASSIFIED` is the Phase 1 seed value (Decision 7) and is
removed when the gate flips to `--forbid-unclassified` in Phase 3.

| id | description | work package |
|---|---|---|
| T-UNCLASSIFIED | Phase 1 seed row: upstream test not classified yet | parity-infra Phase 2-3 |
| F2a | session notices / permission-surfaced session plumbing | F2a |
| F2b | session timing log lines; invalid-mode logging | F2b |
| F5a | managed-policy tier and MDM/registry policy | F5a |
| F7a | tool-result meta parsing | F7a |
| F8 | diff engine (only partial patch metadata exists in hooks.rs, T-43) | F8 |
| F10 | AIR permission presentation | F10 |
| F15 | backgrounded Bash tool-call shaping | F15 |
| F20 | context compaction engine, meta lifecycle, legacy compaction tool call | F20 |
| F21 | ExitPlanMode hand-off | F21 |
| F22 | clear-context coordinator (fresh-query hand-off) | F22 |
| F23a | ACP elicitation mapping | F23a |
| F24 | promptSuggestions frame/publishing and `/usage` markdown | F24 |
| F26a | session-index directory listing, project dirs, worktrees | F26a |
| F26b | session-index activity, connection, live registry, service, session-info | F26b |
| F27a | explicit session rename title | F27a |
| F28 | session list subscriptions | F28 |
| F30 | session title precedence, sanitisation, archive title | F30 |
| F31 | event-loop yielder between replayed messages (B-55) | F31 |
| F32 | `authenticate` arm (gateway payload, auth epoch, logout) | F32 |
| F33 | gateway / Bedrock model listing | F33 |
| F34 | account identity and hide-claude-auth | F34 |
| F35a | native subagent child-session registry | F35a |
| F35b | AIR/RFD subagent event builders | F35b |
| F37 | subagent transcript reader and replay | F37 |
| F38a | ACP v2 initialize / session surface | F38a |
| F38b | ACP v2 agent surface | F38b |
| F39 | ACP v2 prompt / session-update / turn-events | F39 |
| F40 | ACP v2 permission / terminal / tool-call surface | F40 |
| AG-1 | goal extension (`_session/goal`) | AG-1 |
| AG-2 | session-failure extension | AG-2 |
| AG-3 | file-change audit report | AG-3 |
| AG-4 | `/mcp` prompt command | AG-4 |
| B-113 | v2 cancel and withdrawal rules (acp-v2 cancel tests; row B-113) | F39 (Turn 6, lane A) |
| F11 | config-option session side: fast mode, agent switch, effort sync, husk rejection, PostModelSwitch reconcile | F11 (Turn 5, lane A) |
| F12 | steering R811-6 swallow cases (test-map rows 162-163) not yet ported | F12 (carried to Turn 2, lane A) |
| F14 | background-task changedFields dedupe / live task tests | F14 (Turn 2, lane A) |
| F25 | session failure controller, session_info_update carrier, usage-limit replay tests | F25 (Turn 3, lane A) |
| T-PORT | upstream test whose behaviour exists in Rust but is not yet ported as a Rust test | TEST-MAP classification (Turns 8-9) |
