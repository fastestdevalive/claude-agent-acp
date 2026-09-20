# Feasibility: a native-Rust replacement for `@agentclientprotocol/claude-agent-acp`

**Question.** Can a Rust-based ACP-driving tool (vibe-station, or anything shaped like it) drop
Bun/Node/npm entirely for Claude support by reimplementing `claude-agent-acp` — and does
`@anthropic-ai/claude-agent-sdk` have to come along for the ride?

**Method.** Everything below is read from the real artifacts on this machine, not from docs or memory:

| Artifact | Path |
| --- | --- |
| ACP adapter (compiled JS) | `~/.bun/install/cache/@agentclientprotocol/claude-agent-acp@0.70.0@@@1/dist/` |
| Anthropic SDK (bundled JS) | `~/.bun/install/cache/@anthropic-ai/claude-agent-sdk@0.3.232@@@1/` |
| Rust ACP SDK (crate source) | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/agent-client-protocol-2.1.0/` |
| Consumer | `~/code/fastestdevalive/vibe-station/rust/vst-agents/` |

Line/character citations into `sdk.mjs` are `file:line:col`, because that file is a 1.32 MB esbuild
bundle whose 155 "lines" are each tens of thousands of characters wide — `wc -l` is meaningless there.

---

## 1. Does `@anthropic-ai/claude-agent-sdk` need to be ported, or can it be bypassed?

**It can be bypassed, and the existing Rust spike is already on the right side of the line — but the
"thin wrapper" framing is only two-thirds right, and the third that is wrong matters.** The layers
that would have made porting mandatory genuinely are not there. `query()` itself is three statements
(`sdk.mjs:152:477`). The exported `query` path never once calls the settings resolver: the full
managed-policy machinery *is* bundled — per-OS managed paths at `sdk.mjs:151:20500`
(`/Library/Application Support/ClaudeCode`, `C:\Program Files\ClaudeCode`, `/etc/claude-code`), macOS
MDM plist reads at `sdk.mjs:151:57103`, Windows/WSL `reg.exe` HKLM/HKCU queries at
`sdk.mjs:151:205007` parsed by `sdk.mjs:151:207005`, and a six-tier precedence merge at
`sdk.mjs:151:197963` — but it is reachable only through the separately exported `resolveSettings`
(`o5`, defined `sdk.mjs:151:209894`, wrapped `151:210662`), and neither the process transport
(`class Rk`, `sdk.mjs:118:3022`) nor the query factory (`mO`, `sdk.mjs:151:213428`) references it.
What `query()` actually does with settings is forward flags: `--setting-sources=…`,
`--managed-settings`, and a `--settings` JSON blob (`sdk.mjs:118:~7300`, merged by `m1` at
`sdk.mjs:118:213`). Every policy tier, MDM source and permission rule is resolved *inside* the
`claude` binary. Likewise absent: there is **no retry, no respawn, no reconnect** anywhere in `Rk` or
the query class `Gg` — a spawn error is latched once at `sdk.mjs:118:11120` and the iterator throws;
the only backoff in the file (`[200,800]`) belongs to the transcript-mirror batcher and only runs if
you pass `options.sessionStore`. And `query()` holds **no session state**: `class Gg`
(`sdk.mjs:118:19873`) carries `pendingControlResponses`, `cancelControllers`, `hookCallbacks`,
`sdkMcpTransports` and friends — pure protocol plumbing, no session id, no history, no compaction.
Resume is flags (`--resume=`, `--fork-session`, `--session-id=`). Prompt injection is literally one
line (`hO`, `sdk.mjs:151:219410`), and it writes `session_id:""` — the SDK does not even track the
real id. Binary resolution is a fixed candidate list over the optional native packages
(`b1`, `sdk.mjs:118:17481`) with glibc/musl ordering from `process.report.getReport().header
.glibcVersionRuntime` (`sdk.mjs:118:17296`) — **no PATH search, no `which claude`.** Worth flagging
as a correction to the prior investigation: `CLAUDE_CODE_EXECUTABLE` **does not exist in SDK
0.3.232** — `grep -c` returns 0 across `sdk.mjs`, `sdk.d.ts` and `bridge.mjs`. vibe-station's
override (`vst-agents/src/claude.rs:420`) works because *the adapter* reads that variable
(`dist/acp-agent.js:233-236`, applied at `4908`) and passes it down as
`options.pathToClaudeCodeExecutable`; the SDK only honors the option. **The third that is wrong:**
the SDK's control-channel implementation (`class Gg`, `sdk.mjs:118:19873`–`124:452`, ~22 KB
minified ≈ 800–1000 lines of source) is *not* thin, and a hand-rolled spawn+parse silently misses
several load-bearing facts. First, the real argv is
`["--output-format","stream-json","--verbose","--input-format","stream-json"]`
(`sdk.mjs:118:4903`) — **`--print` is never passed** (`grep -o -- '"--print"' sdk.mjs` → 0 hits);
non-interactive mode is inferred from stream-json IO plus `CLAUDE_CODE_ENTRYPOINT="sdk-ts"`. Second,
and most consequentially: **`systemPrompt`, `agents`, `hooks`, `skills`, `toolAliases`,
`forwardSubagentText` and `supportedDialogKinds` are sent in the `initialize` *control_request*, not
as CLI flags** (`sdk.mjs:120:~4100`), and the response is what carries `commands`, `models`, `agents`
and `account` back. Any Rust driver wanting a custom system prompt or hooks must therefore implement
the control channel — it is not optional the moment you go past a bare prompt. Third, a deadlock
trap: `hasBidirectionalNeeds()` (`sdk.mjs:118:20530`) suppresses the normal
close-stdin-after-prompt behavior whenever a `canUseTool`, hook, SDK-MCP or elicitation callback is
registered; close stdin early with a permission callback live and the CLI hangs. Beyond that sits a
pile of individually-boring correctness details a reimplementation will rediscover the hard way: the
`suppressControlResponse` sentinel meaning *deliberately never answer* (`sdk.mjs:118:19850`);
`pending_permission_requests` / `pending_user_dialog_requests` replay carried on the `initialize`
response; `keep_alive` frames consumed silently at `sdk.mjs:118:23865` (a naive parser leaks them to
the caller); `control_cancel_request` for in-flight cancellation; the stderr-drain-before-exit race
(`sdk.mjs:118:704`, `786`) plus the 2 KB stderr tail that turns "exit code 1" into an actionable
message; the rule that a `result` with `is_error` *replaces* the raw process error
(`sdk.mjs:118:~24200`); the SIGTERM→SIGKILL ladder (2 s / 5 s, `sdk.mjs:118:~14900`) driven by a
*separate* forwarded abort controller (`sdk.mjs:118:3400`) so the user's signal cannot hard-kill; and
a module-global `process.on("exit")` child reaper (`sdk.mjs:118:806`–`949`). **Verdict: bypass, not
port** — nothing architectural is lost, the spike already proved the happy path, and reimplementing
`Gg`'s subset (`initialize`, `can_use_tool`, `interrupt`, `set_permission_mode`, `hook_callback`) is
a few hundred lines of Rust. The one piece with real design in it, and the only part worth porting
*carefully* rather than reinventing, is the in-process MCP server multiplexed over the control
channel (`class Ik` at `sdk.mjs:118:19590` + `connectSdkMcpServer` + `sendMcpServerMessageToCli` +
`handleMcpControlRequest`, with bidirectional JSON-RPC id correlation and an outbound-id
short-circuit) — and vibe-station does not use that today.

## 2. Is `claude-agent-acp` itself portable, and how much is actually needed?

**Portable, with no technical wall — but the honest scope is not "7,100 lines"; it is roughly 3,150
lines of core inside `acp-agent.js` plus ~630 lines of tool mapping, and for vibe-station
specifically it is meaningfully less than that again.** The package is 11,266 lines of compiled JS,
of which `dist/acp-agent.js` is 7,114 and `dist/tools.js` 1,111; the rest
(`file-change-audit.js` 379, `session-failure-extension.js` 351, `elicitation.js` 304,
`settings.js` 185, `index.js` 98, `utils.js` 81, `goal-extension.js` 50) is almost entirely
skippable. A line-accounted read of `acp-agent.js` splits roughly **44% protocol-necessary / 44%
optional-rich / 12% boilerplate**, and a large fraction of the "core" lines are prose comments — the
turn-lifecycle region `1215–1845` is about half commentary. The protocol-necessary core is:
capabilities in `initialize` (`693–746`, 54 lines); `newSession`/`loadSession` plus the non-optional
slice of `createSession` (`747–759`, `788–798`, `4645–4695`, ~200 of `4696–5209`'s 514); `prompt`
itself, which owns no loop and merely enqueues a `Turn` and awaits a deferred (`973–1037`, 65 lines);
the turn activate/settle/orphan machinery (`1338–1682`, 345); the consumer loop with its abort/EOF
race (`1683–1845`, 163); `result` → stop-reason mapping (`2532–2884`, 353); idle settle via
`session_state_changed` (`2049–2163`, 115); the stream→ACP mappers `toAcpNotifications` (`6383–6759`),
`streamEventToAcpNotifications` (`6760–6864`) and `toolCallNotification` (`6303–6382`), 562 combined;
prompt content mapping (`6101–6193`, 93); permission translation — `canUseTool` (`4069–4275`),
`requestPermissionFromClient`/`ensureToolCallEmitted` (`4017–4068`),
`permissionMetadataForAlwaysAllow` (`447–541`) — 354 combined; `cancel` (`3443–3668` plus
`6865–6892`, 254); fs passthrough (`4002–4016`, 15); and wiring (`542–572`, `6893–6929`, ~250).
Everything else is genuinely optional: model resolution and allowlisting is its own ~560 lines
(`5676–6045`, `6930–7114`); config options / modes / fast mode ~630 (`3724–3860`, `4388–4644`,
`5479–5675`); elicitation ~540 (`elicitation.js` + `4276–4374` + refusal-fallback `2396–2481`);
the JetBrains/AIR file-change-audit ~480 and session-failure ~450 (both negotiated as `_meta`
capabilities at `acp-agent.js:734`, and together the single largest skippable chunk at ~930 lines);
auth/gateway/providers ~390; TODO/plan lists ~270; settings watching ~210 (`settings.js` — and note
it is built on the SDK's `@alpha` `resolveSettings` + `filterEscalatingDefaultMode`,
`settings.js:5,90-91`, so a Rust port must either reimplement the merge or skip it); subagent
transcripts ~200; hooks ~180; terminal ~140; goal extension ~140; custom slash commands ~95; MCP
server passthrough a mere ~60. **The official Rust SDK is real scaffolding, but less of it than the
prior note implied.** `agent-client-protocol` 2.1.0 gives you newline-delimited JSON-RPC over stdio
(`src/stdio.rs:11-85`), the connection loop and `ConnectionTo<…>` with request/response correlation,
cancellation plumbing (`Responder::cancellation()`, `src/jsonrpc.rs:4611`), typed structs for the
entire ACP schema, the full `SessionUpdate` enum
(`agent-client-protocol-schema-1.7.0/src/v1/client.rs:99-159`), the agent→client requests including
`session/request_permission`, `fs/*`, `terminal/*` and `elicitation/create`
(`src/schema/agent_to_client/requests.rs:10-49`), and first-class extension support — `ExtRequest`/
`ExtNotification` with `_`-prefixed methods (`schema/v1/ext.rs:25-96`), an `[ext]` fallback variant
on the request enums, `_meta` on ~195 schema types, plus `on_receive_dispatch`/`add_dynamic_handler`
and derive macros — so both custom extensions (`_session/steering`, `_session/goal`) are expressible.
It is mature: 2.2.0 released 2026-09-18, 4.5 M total downloads, authored by Zed, runtime-agnostic
(tokio is only a dev-dependency), all handler bounds are `Send`, so **no `LocalSet` constraint** —
a change from the 0.x era. Two corrections to the prior architecture note, though. First, the shape:
in 2.x `Agent` is a **zero-sized role marker** (`src/role/acp.rs:291`), not a trait to implement —
you register typed handlers on `Agent::builder()` (`role/acp.rs:335-352`; full 21-line agent at
`examples/simple_agent.rs`). The old 0.4.x `Agent` trait with `initialize`/`prompt`/`cancel` is gone.
Second, and more useful: it provides **zero** Claude-specific help — no stream-json parsing anywhere,
and its only Claude reference is `AcpAgent::claude_agent()` at `src/acp_agent.rs:192-197`, which
shells out to `npx -y @agentclientprotocol/claude-agent-acp`, i.e. the exact thing being replaced.
**The scope-shrinking observation nobody has written down yet:** vibe-station is an ACP *client*
(`acp_connection.rs`; `examples/acp_hello.rs` drives the adapter as a subprocess via
`AcpAgent::from_args`), and it already owns a frozen in-process abstraction — the `AcpTransport`
trait at `acp_transport.rs:115-178` (`initialize`/`new_session`/`load_session`/`send_prompt`/
`cancel_active_prompt`/`steer`/`dispose`). A native driver can implement *that trait* directly and
never serialize ACP over a pipe at all, which deletes the entire agent-side JSON-RPC surface from the
port. And the output surface it must produce is narrower still: `normalize.rs:340-473` consumes only
`AgentMessageChunk`, `AgentThoughtChunk`, `UserMessageChunk`, `ToolCall`, `ToolCallUpdate`,
`CurrentModeUpdate`, `AvailableCommandsUpdate` and `Plan`, and explicitly discards
`SessionInfoUpdate`, `ConfigOptionUpdate` and `UsageUpdate` (`normalize.rs:471-473`). **As for
blockers — I found no technical wall, but I did find a class of risk the prior note did not name.**
The hard part is not message mapping; it is the *turn-settlement state machine*, which is
reverse-engineered from undocumented CLI behavior and self-documents as such. `acp-agent.js:1453`
states outright that its orphan-coalescing ordering argument "is asserted from observed CLI behavior,
not a documented wire contract — if a dead turn's late result could lag past the NEXT turn's dispatch
frames, deleting a zombie and a started entry on one result would double-consume it."
`acp-agent.js:4109-4113` rests subagent attribution on "an undocumented SDK invariant
(`task_started.task_id` === `canUseTool`'s `agentID`…; verified against the bundled CLI)."
`acp-agent.js:1687-1692` documents a genuine concurrency hack — the in-flight `query.next()` must be
held across abort wake-ups because "async generators serialize `next()` calls, so racing a SECOND
`next()` while one is pending would make the abandoned one swallow a message." There is a
force-cancel grace timer existing purely because `query.next()` can wedge and never yield (issue
#680, `acp-agent.js:46`, `1683-1685`, `3595-3605`). There is a documented *unfixable* hole:
`acp-agent.js:2143-2151` explains that a turn abandoned before its echo "still hangs until cancel or
the next prompt; only a timer could tell those apart." There is a latched boolean with a
consciously-accepted wrong case (`1595-1606`, issue #453). There is an incremental JSON-prefix lexer
(`scanStreamedToolInput`/`recoveredToolInput`, `179-232`) that closes a partial tool-input object at a
top-level comma so streamed tool calls can be refined mid-flight. There is dual-lane interrupt
reconciliation for CLIs with vs. without `interrupt_receipt_v1`, guarding the *field* not the receipt
"so a bare `{}` success from a gateway can't read as 'everything was dropped'" (`3608-3648`). There is
a subagent permission deadlock (issue #866, `1553-1560`) forcing every settle path through
`settleOrDefer`. There is even a documented CLI regression worked around by a text heuristic: the
adapter refuses to call `getContextUsage` before a fresh session's first turn because "that control
request is not serviced (~15 s stall, issues #886/#880)" and — critically — "SDK control requests are
serialized over one channel," so it would drag the awaited `setModel` down with it
(`4416-4420`, `7005-7016`). **That serialization constraint is itself a design input a Rust port must
honor.** None of this is a wall; all of it is empirical knowledge that took the upstream project many
releases to accumulate, and a from-scratch port re-enters that discovery loop with no test suite to
match against.

## 3. Bottom line

The three components do not sit on one risk gradient; they sit in three different categories, and
conflating them is what makes this question look like a yes/no.

**(a) Bypassing `claude-agent-sdk` is already proven and clearly correct.** No settings resolution, no
retry, no reconnection, no session state is lost — all of it lives in the `claude` binary. The spike
demonstrated the happy path with zero Node in the loop. The only caveats are corrections to the plan,
not objections to it: use the SDK's real argv (no `--print`), set `CLAUDE_CODE_ENTRYPOINT`, and
budget a few hundred lines for the control-channel subset, because **system prompt, hooks, agents and
skills travel over `initialize` as a control_request, not as flags** — which means the control channel
is on the critical path much earlier than "just for permission prompts."

**(b) Porting the CORE protocol logic is real-but-bounded, and smaller than the headline number.**
~3,150 core lines in `acp-agent.js` + ~630 in `tools.js`, minus what vibe-station provably never
consumes. The Rust ACP SDK removes all framing/schema/extension work. Going in-process behind the
existing `AcpTransport` trait removes the agent-side JSON-RPC surface entirely. The genuine cost
centre is the turn-settlement state machine and its accumulated empirical fixes (#680, #825, #851,
#866, #453, #886/#880), which must be re-derived rather than read off a spec.

**(c) Porting the FULL feature surface is not worth doing, and nothing forces it.** The two AIR
extensions (~930 lines), elicitation (~540), model allowlisting (~560), config/modes (~630) and
settings watching (~210) are all independently droppable. The only item with real design density is
the SDK's in-process MCP server over the control channel — and vibe-station does not use it.

**No new technical wall was found.** The only *new* risks worth adding to the existing
"undocumented, therefore version-fragile" concern are three concrete behavioral constraints:
control requests are **serialized over a single channel** (`acp-agent.js:4416-4420`), so a slow one
head-of-line-blocks the rest; stdin must **not** be closed after the prompt once a permission/hook
callback is registered (`sdk.mjs:118:20530`) or the CLI deadlocks; and `keep_alive` /
`control_cancel_request` / `pending_permission_requests`-replay frames must be handled or the stream
decoder misbehaves in ways that look like Claude bugs.

| Component | Verdict | Rough scope | Confidence |
| --- | --- | --- | --- |
| **(a)** Bypass `claude-agent-sdk` entirely | **Low-risk / already proven.** Spike did it; SDK is a genuinely thin wrapper for spawn+prompt+parse. | ~0 (done) + a few hundred lines for the control-channel subset | High — read the real bundle; `resolveSettings` verified unreachable from `query()` |
| **(b)** Reimplement `claude-agent-acp` CORE in Rust | **Real but bounded engineering.** No blocker; ACP framing is free; in-process `AcpTransport` avoids the agent side. Cost is the undocumented turn-settlement state machine. | ~3.2k JS-equivalent lines; less for vibe-station's 8-of-14 update surface | Medium-high — line-accounted, but comment-heavy JS makes the estimate soft |
| **(c)** Reimplement the FULL feature surface | **Not worth it, and unnecessary.** Every rich feature is independently droppable; ~930 lines of it are vendor-specific (JetBrains/AIR). | ~3.1k more lines, mostly optional | High — each feature's line ranges are isolable |
| **SDK in-process MCP server over control channel** | **The one piece with real design.** Port carefully *if* needed; vibe-station does not need it today. | ~22 KB minified `class Gg`, of which MCP multiplexing is the dense part | High |
| **Genuinely new blockers** | **None found.** Three behavioral constraints to design around (serialized control channel; stdin-close deadlock; `keep_alive`/cancel/replay frames). | — | Medium — absence of evidence over one read |

**Recommendation:** proceed on (a) and (b); explicitly descope (c). Treat the turn-settlement state
machine, not the wire format, as the schedule risk — and before committing implementer time, build a
differential harness that runs identical prompts through the Node path and the Rust path and diffs
the resulting `SessionUpdate` streams, since that is the test suite the upstream project has and a
from-scratch port otherwise does not.
