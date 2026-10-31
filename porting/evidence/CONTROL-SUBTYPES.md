# SDK control-subtype evidence (F1)

Source of truth: the pinned Agent SDK bundle

`rust/target/upstream/0.94.0/node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs`
(READ-ONLY, minified: 169 lines, so references are `sdk.mjs:<line>:<column>`).

Type declarations come from the same package's `sdk.d.ts`; where a runtime
method is **not** declared there (the adapter reaches it through an optional
shape) that is stated explicitly. Adapter references are the pinned upstream
worktree `src/acp-agent.ts` / `src/session-titles.ts`.

Everything below is **outbound** (SDK host -> CLI) unless marked inbound. The
wire envelope is built by `Query.request` (`sdk.mjs:159:16876`):

```json
{"request_id":"<random base36>","type":"control_request","request":{...}}
```

and the success reply read by the callers is

```json
{"type":"control_response","response":{"subtype":"success","request_id":"...","response":<payload>}}
```

(`sdk.mjs:157:132411`). Callers that return `.response` read `<payload>`; the
`rename_session` caller reads nothing. An SDK-side handler may return the
`suppressControlResponse` symbol (`Pc`, `sdk.mjs:157:124429/124440`) to write
**no** response at all.

---

## `mcp_authenticate`

- Subtype string: `mcp_authenticate`
- Side: SDK host -> CLI (outbound). Issued by `Query.mcpAuthenticate(serverName, redirectUri)`
  at `sdk.mjs:161:1210`; request literal at `sdk.mjs:161:1258`.
- Runtime method: present on `Query`, **not declared in `sdk.d.ts`**
  (not found in sdk.mjs either as a type; the adapter declares its own shape).

Request fields (from the request literal at `sdk.mjs:161:1258`):

| field | type | required | notes |
|---|---|---|---|
| `subtype` | `"mcp_authenticate"` | yes | |
| `serverName` | `string` | yes | camelCase on the wire, unlike most control fields |
| `redirectUri` | `string` | yes (may be `undefined`) | second positional arg; the adapter calls it with one argument, so it serialises as `undefined`/absent |

Response shape (adapter-declared, `src/acp-agent.ts:546-556`):

```ts
{ authUrl?: string; requiresUserAction: boolean; callbackExpected: boolean;
  redirectScheme?: "localhost" | "custom"; callbackPort?: number; state?: string }
```

`authUrl`/`requiresUserAction`/`callbackExpected` are the fields the adapter
actually consumes (`src/acp-agent.ts:2605-2612`); the rest are declared for
future use. Not found in sdk.mjs (it only forwards `.response`).

Adapter references:
- `src/acp-agent.ts:536-556` — `McpOAuthQuery` runtime shape.
- `src/acp-agent.ts:2593-2651` — `authenticateMcpServer`: calls
  `query.mcpAuthenticate(serverName)`, then bridges the returned `authUrl` to an
  ACP **url** elicitation (`mcp-oauth-<uuid>`), and polls `waitForMcpAuthentication`
  while the client has it open.

---

## `remote_control`

- Subtype string: `remote_control`
- Side: SDK host -> CLI (outbound). Issued by
  `Query.enableRemoteControl(enabled, name?, options?)` at `sdk.mjs:159:14495`
  (method); request literal at `sdk.mjs:159:14838`.
- Runtime method: **not declared in `sdk.d.ts`**; adapter declares it
  (`src/acp-agent.ts:568-575`).

Request fields (request literal `sdk.mjs:159:14838`):

| field | type | required | notes |
|---|---|---|---|
| `subtype` | `"remote_control"` | yes | |
| `enabled` | `boolean` | yes | first positional arg |
| `name` | `string` | no | spread only when `name !== undefined` |
| `reattach_session_id` | `string \| undefined` | no (always written) | from `options.reattachSessionId`; written even when `undefined` |
| `keep_session_on_exit` | `boolean` | no | from `options.keepSessionOnExit` |
| `work_secret` | opaque object | no | from `options.workSecret` |

Response shape:
- sdk.mjs reads `response.bridge_session_id` (`sdk.mjs:159:15158`) to re-arm
  the work-secret answerer; it must be a `string` to match.
- Adapter-declared: `{ session_url?: string; connect_url?: string }`
  (`src/acp-agent.ts:568-575`). `session_url` is consumed at
  `src/acp-agent.ts:3975-3980`.

Adapter references:
- `src/acp-agent.ts:562-579` — `RemoteControlQuery` + `supportsRemoteControl`.
- `src/acp-agent.ts:3959-3986` — `runRemoteControlCommand`: `false` to
  disconnect, `true, command.name` to (re)connect, then emits an
  `agent_message_chunk` with the resulting text.

---

## `rename_session`

- Subtype string: `rename_session`
- Side: SDK host -> CLI (outbound). Issued by
  `Query.renameSession(title, sessionId?)` at `sdk.mjs:159:15586`
  (method `sdk.mjs:159:15541`).
- Runtime method: **not declared in `sdk.d.ts`**; adapter declares it as an
  optional query method (`src/session-index/connection.ts:325-339`).

Request fields (request literal `sdk.mjs:159:15586`; type
`SDKControlRenameSessionRequest`, `sdk.d.ts:5067-5076`):

| field | type | required | notes |
|---|---|---|---|
| `subtype` | `"rename_session"` | yes | |
| `title` | `string` | yes | |
| `source` | `"remote" \| "host"` | no | SDK always sends `"host"` (`sdk.mjs:159:15607`); CLI defaults to `"remote"` |
| `session_id` | `string` | no | when given, the CLI refuses if the process has since moved sessions (`/clear`, in-session resume) |

Response shape: none consumed — `Query.renameSession` awaits the request and
returns `Promise<void>` (`sdk.mjs:159:15574-15587`). The CLI still sends the
normal success envelope; `sdk.d.ts` declares no response type for this subtype
(not found in sdk.mjs / sdk.d.ts).

Adapter references:
- `src/acp-agent.ts:3217-3219` — `renameSessionTitle` delegates to the session
  index.
- `src/session-index/connection.ts:309-345` — `retitle`/`persist` calls
  `query.renameSession(title, sessionId)` for a running session; a closed
  session's CLI is gone, so the title is only written through a live query.

---

## `generate_session_title`

- Subtype string: `generate_session_title` (the JS method is
  `generateSessionTitle`; this is the wire subtype — the card's distinction).
- Side: SDK host -> CLI (outbound). Issued by
  `Query.generateSessionTitle(description, {persist?})` at `sdk.mjs:159:15648`
  (method); request literal at `sdk.mjs:159:15749`.
- Runtime method: **not declared in `sdk.d.ts`** (confirmed by the upstream
  smoke test `src/tests/session-titles.test.ts:58-68`, which asserts the bundle
  still contains `async generateSessionTitle(` and
  `subtype:"generate_session_title"`).

Request fields (request literal `sdk.mjs:159:15749`):

| field | type | required | notes |
|---|---|---|---|
| `subtype` | `"generate_session_title"` | yes | |
| `description` | `string` | yes | rolling title context (`appendTitleContext`, capped at `MAX_TITLE_CONTEXT_LENGTH`) |
| `persist` | `boolean` | no | spread only when `options.persist !== undefined`; the adapter always sends `true` |

Response shape: `{ title: string | null }` — sdk.mjs returns
`.response.title` (`sdk.mjs:159:15818`); the adapter wraps it as
`Promise<string | null>` (`src/session-titles.ts:32-38`). A `null` title means
"could not generate"; the adapter falls back to the stored summary.

Adapter references:
- `src/session-titles.ts:32-38` — `TitleCapableQuery` optional shape.
- `src/session-titles.ts:82` — `supportsTitleGeneration` runtime check.
- `src/session-titles.ts:318-333` — `requestGenerateTitle` calls
  `generateSessionTitle(description, { persist: true })`.
- `src/tests/session-titles.test.ts:211` — asserts the exact call
  `(LONG_PROMPT, { persist: true })`.

---

## `get_usage`

- Subtype string: `get_usage`
- Side: SDK host -> CLI (outbound). Issued by
  `Query.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET(options?)`
  at `sdk.mjs:161:2123` (method); request literal `sdk.mjs:161:2211`.
- Method name is deliberately unstable; the adapter keeps it spelled out so an
  SDK rename fails at compile time (`src/acp-agent.ts:508-512`).

Request fields (`sdk.d.ts:4315-4323`):

| field | type | required | notes |
|---|---|---|---|
| `subtype` | `"get_usage"` | yes | |
| `skip_behaviors` | `boolean` | no | spread only when `options.skipBehaviors`; skips the 7-day transcript scan and nulls `behaviors` |

Response shape (`SDKControlGetUsageResponse`, `sdk.d.ts:4327-4390+`):

```ts
{
  session: {
    total_cost_usd: number; total_api_duration_ms: number;
    total_duration_ms: number; total_lines_added: number;
    total_lines_removed: number; model_usage: Record<string, ModelUsage>;
  };
  subscription_type: string | null;   // 'pro' | 'max' | 'team' | 'enterprise' | null
  rate_limits_available: boolean;     // false => rate_limits null
  rate_limits: {
    five_hour?: UsageWindow | null;
    seven_day?: UsageWindow | null;
    seven_day_oauth_apps?: UsageWindow | null;
    seven_day_opus?: UsageWindow | null;
    seven_day_sonnet?: UsageWindow | null;
    // ... (open-ended; see sdk.d.ts)
  } | null;
  // plus the "behaviors" section the request can skip
}
// UsageWindow = { utilization: number | null; resets_at: string | null }
```

Adapter references:
- `src/acp-agent.ts:501-515` — `structuredUsageMarkdown` calls the method under
  a timeout/abort race; any failure keeps Claude Code's own `/usage` output.
- `src/acp-agent.ts:3732` — wired as the `produce` callback of the `/usage`
  local command.

---

## `request_user_dialog` (inbound: CLI -> SDK host)

- Subtype string: `request_user_dialog`
- Side: CLI -> SDK host (**inbound**), handled in
  `Query.processControlRequest` (`sdk.mjs:159:383`); branch at
  `sdk.mjs:159:3236`.
- Request fields (`SDKControlRequestUserDialogRequest`, `sdk.d.ts:5121-5139`):

| field | type | required | notes |
|---|---|---|---|
| `subtype` | `"request_user_dialog"` | yes | |
| `dialog_kind` | `string` | yes | open string union; only kinds declared in `initialize.supportedDialogKinds` are emitted |
| `payload` | `Record<string, unknown>` | yes | shape defined per `dialog_kind`, opaque to the protocol |
| `tool_use_id` | `string` | no | present when tied to a tool invocation |

Response shape (`UserDialogResult`, `sdk.d.ts:10046-10052`):

```ts
{ behavior: 'completed'; result: unknown } | { behavior: 'cancelled' }
```

Handler semantics (`sdk.mjs:159:3236-3263`):
- with `onUserDialog`: the handler's return value is sent as the success
  payload; returning `null` returns `Pc` (no response written);
- **without** `onUserDialog`: logs `No onUserDialog handler for
  request_user_dialog (kind=...) — staying silent ...` and returns `Pc`, so
  **no response is written** (`sdk.mjs:159:3247-3257`).
- `request_user_dialog` is also in `Z4`'s sibling path: the SDK exposes
  `processPendingUserDialogRequests` (`sdk.mjs:159:16737`) to re-arm dialogs
  carried on an initialize response.
- Unrecognised kinds must be answered `{behavior:'cancelled'}` by a handler
  that does have one.

Adapter references:
- `src/acp-agent.ts:10359-10400` — `handleUserDialog`: only
  `REFUSAL_FALLBACK_DIALOG_KIND` is rendered (as an ACP form elicitation);
  every other kind answers `{behavior:'cancelled'}`; client failures also
  answer `cancelled`.
- `src/acp-agent.ts:1666-1667` — `onElicitation: false, onUserDialog: false`
  in the capability probe.

---

## `elicitation` (inbound: CLI -> SDK host)

- Subtype string: `elicitation`
- Side: CLI -> SDK host (**inbound**), handled in
  `Query.processControlRequest`; branch at `sdk.mjs:159:2828`.
- Request fields (`SDKControlElicitationRequest`, `sdk.d.ts:3969-3998`) — these
  are the snake_case fields spelled out in `processControlRequest`:

| field | type | required | notes |
|---|---|---|---|
| `subtype` | `"elicitation"` | yes | |
| `mcp_server_name` | `string` | yes | passed to the handler as `serverName` |
| `message` | `string` | yes | |
| `mode` | `"form" \| "url"` | no | absent means form |
| `url` | `string` | no | url mode only |
| `elicitation_id` | `string` | no | url mode only; correlates `elicitation_complete` |
| `requested_schema` | `Record<string, unknown>` | no | form mode only |
| `property_order` | `string[]` | no | top-level schema property order |
| `title` | `string` | no | from MCP `_meta['anthropic/permissionDisplay']` |
| `display_name` | `string` | no | ditto |
| `description` | `string` | no | ditto |

The handler receives them camelCased (`serverName`, `elicitationId`,
`requestedSchema`, `displayName`, ...) — see the mapping at
`sdk.mjs:159:2829-2855` and `ElicitationRequest`, `sdk.d.ts:734-751`.

Response shape (`ElicitationResult` = MCP `ElicitResult`,
`sdk.d.ts:759`, `@modelcontextprotocol/sdk` `types.d.ts:5381-5394`):

```ts
{ action: 'accept' | 'decline' | 'cancel';   // required
  content?: Record<string, string | number | boolean | string[]>;
  _meta?: Record<string, unknown>; }
```

Handler semantics (`sdk.mjs:159:2828-2876`):
- with `onElicitation`: the handler's value is the success payload; `null`
  returns `Pc` (no response written);
- **without** `onElicitation`: returns `{action:'decline'}` — a real success
  response, unlike the dialog case above.

Adapter references:
- `src/acp-agent.ts:10266-10298` — `handleMcpElicitation`: modes the client did
  not advertise (or requests that cannot be represented) return
  `{action:'decline'}`; an aborted signal returns `{action:'cancel'}`; client
  failures log and return `{action:'decline'}`.
- `src/acp-agent.ts:11115-11123` — `ElicitationSupport` from
  `clientCapabilities.elicitation.form/url`, which also decides whether
  `AskUserQuestion` is disallowed.
- `src/acp-agent.ts:6586-6596` — `elicitation_complete` system message forwards
  an ACP `elicitation/complete` notification for url mode.

---

## Inbound control dispatch, for reference

`Query.handleControlRequest` (`sdk.mjs:157:131866`) wraps
`Query.processControlRequest` (`sdk.mjs:159:383`):

1. skip when `cleanupPerformed` (request arrived after close);
2. skip a duplicate delivery of an in-flight `request_id`
   (`cancelControllers.has`);
3. `Z4 = ["remote_tool_call","remote_plumbing_call","remote_tools_probe",
   "remote_tools_reannounce"]` (`sdk.mjs:157:124177`) are **never answered**
   (`return ... Pc`, "leaving it unanswered", `sdk.mjs:159:384-390`);
4. otherwise dispatch by subtype; an unknown subtype throws
   `Error("Unsupported control request subtype: " + subtype)`
   (`sdk.mjs:159:5306`), which becomes an error `control_response` carrying
   the message.
