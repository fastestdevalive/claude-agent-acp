//! `_meta.claudeCode.options` -> typed spawn inputs (rust-parity-fixes
//! Decision 2, P1/P2; ROADMAP F4a).
//!
//! Node spreads `userProvidedOptions` (`_meta.claudeCode.options`) into the SDK
//! options and the SDK maps them to `claude` argv/env. This module is the pure
//! typed mapper: it parses the option keys into a [`ClaudeOptions`], which
//! `agent.rs` folds into [`crate::process::SpawnOptions`]; the argv order and
//! extraArgs/env encoding are exercised by the unit tests here.
//!
//! The ACP `mcpServers` array and the programmatic settings tier are resolved
//! here too: `mcpServers` becomes the SDK's `--mcp-config` JSON (WB-13) and
//! `_meta.claudeCode.options.settings` (path or object) is merged with
//! `CLAUDE_MODEL_CONFIG` and the `sandbox` option into the `--settings` extra
//! arg, exactly like `acp-agent.ts:11175-11205` + the SDK's `F4`/`U4`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The SDK's `ThinkingConfig` (`_meta.claudeCode.options.thinking`, or the
/// legacy `MAX_THINKING_TOKENS` env var via [`resolve_thinking`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingConfig {
    /// `{type:"adaptive", display?}` -> `--thinking=adaptive`.
    Adaptive { display: Option<String> },
    /// `{type:"enabled", budgetTokens?, display?}` -> `--max-thinking-tokens=<n>`
    /// (or `--thinking=adaptive` when the budget is absent).
    Enabled {
        budget_tokens: Option<u64>,
        display: Option<String>,
    },
    /// `{type:"disabled"}` -> `--thinking=disabled`.
    Disabled,
}

/// `_meta.claudeCode.options.pluginDelivery` (SDK default `argv`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PluginDelivery {
    #[default]
    Argv,
    Initialize,
}

/// One optional filter field, mirroring JavaScript's `===` comparison.
///
/// `undefined` (an absent key) is a wildcard; any other value must equal the
/// message field, so a present non-string value matches nothing
/// (`acp-agent.ts:11841-11853`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterField {
    /// The key is absent (`undefined`): match any message value.
    Any,
    /// The key holds a string: match exactly.
    Exact(String),
    /// The key is present but not a string (`null`, number, object): never
    /// equals the string message field.
    Never,
}

/// One entry of the `emitRawSDKMessages` filter list (`SDKMessageFilter`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSdkMessageFilter {
    /// `type` — matched exactly against the SDK message's `type`.
    pub type_: String,
    /// `subtype` — absent is a wildcard, a string must match, anything else
    /// matches nothing.
    pub subtype: FilterField,
    /// `origin` — same semantics against the message's `origin.kind`.
    pub origin: FilterField,
}

/// `_meta.claudeCode.emitRawSDKMessages` (`acp-agent.ts:1731-1773`): whether
/// every raw SDK message is forwarded as the `_claude/sdkMessage` ext
/// notification, only the filtered ones, or none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RawSdkMessages {
    /// `false` / absent (the default): emit nothing.
    #[default]
    Off,
    /// `true`: emit every message.
    All,
    /// An `SDKMessageFilter[]`: emit a message matching at least one filter.
    Filters(Vec<RawSdkMessageFilter>),
}

/// `shouldEmitRawMessage` (`acp-agent.ts:11841-11853`).
pub fn should_emit_raw_message(config: &RawSdkMessages, message: &Value) -> bool {
    match config {
        RawSdkMessages::Off => false,
        RawSdkMessages::All => true,
        RawSdkMessages::Filters(filters) => {
            let type_ = message.get("type").and_then(Value::as_str).unwrap_or("");
            let subtype = message.get("subtype").and_then(Value::as_str);
            let origin = message.pointer("/origin/kind").and_then(Value::as_str);
            filters.iter().any(|filter| {
                filter.type_ == type_
                    && filter_field_matches(&filter.subtype, subtype)
                    && filter_field_matches(&filter.origin, origin)
            })
        }
    }
}

fn filter_field_matches(field: &FilterField, message: Option<&str>) -> bool {
    match field {
        FilterField::Any => true,
        FilterField::Exact(wanted) => message == Some(wanted.as_str()),
        FilterField::Never => false,
    }
}

fn parse_filter_field(value: Option<&Value>) -> FilterField {
    match value {
        None => FilterField::Any,
        Some(Value::String(text)) => FilterField::Exact(text.clone()),
        Some(_) => FilterField::Never,
    }
}

/// Parse `emitRawSDKMessages`; an unusable value (not a boolean or array) is
/// treated as off (Node would throw from `config.some`, but the option only
/// reaches a message loop that cannot recover anyway).
fn parse_raw_sdk_messages(value: Option<&Value>) -> RawSdkMessages {
    match value {
        Some(Value::Bool(true)) => RawSdkMessages::All,
        Some(Value::Array(filters)) => RawSdkMessages::Filters(
            filters
                .iter()
                .filter_map(|filter| {
                    let object = filter.as_object()?;
                    Some(RawSdkMessageFilter {
                        type_: object.get("type").and_then(Value::as_str)?.to_string(),
                        subtype: parse_filter_field(object.get("subtype")),
                        origin: parse_filter_field(object.get("origin")),
                    })
                })
                .collect(),
        ),
        _ => RawSdkMessages::Off,
    }
}

/// One `_meta.claudeCode.options.plugins` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// `type` (only `local` is supported by the SDK).
    pub kind: String,
    /// `path` for a `local` plugin.
    pub path: Option<String>,
    /// `skipMcpDiscovery` -> `--plugin-dir-no-mcp` instead of `--plugin-dir`.
    pub skip_mcp_discovery: bool,
}

/// One ACP `mcpServers` entry mapped to the SDK's `McpServerConfig`
/// (`acp-agent.ts:11029-11053`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServer {
    /// `{type:"stdio", command, args?, env?}`.
    Stdio {
        command: String,
        args: Option<Vec<String>>,
        env: Option<Vec<(String, String)>>,
    },
    /// `{type:"http", url?, headers?}`. A missing `url` is kept as absent, as
    /// Node forwards `url: undefined` (dropped by `JSON.stringify`).
    Http {
        url: Option<String>,
        headers: Option<Vec<(String, String)>>,
    },
    /// `{type:"sse", url?, headers?}`.
    Sse {
        url: Option<String>,
        headers: Option<Vec<(String, String)>>,
    },
}

/// Parsed `_meta.claudeCode.options`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeOptions {
    /// `model` as `if (model) String(model)` (FA1-17): `""`/`0`/`false`/`null`
    /// are falsy and emit no flag.
    pub model: Option<String>,
    pub betas: Vec<String>,
    /// `extraArgs` in insertion order; `None` value = a bare `--key`.
    pub extra_args: Vec<(String, Option<String>)>,
    /// User env overrides (merged over inherited; `CLAUDE_CODE_EMIT_...` last).
    pub env: Vec<(String, String)>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub tools: Option<Vec<String>>,
    /// `toolAliases` (O-155).
    pub tool_aliases: Option<Value>,
    /// `toolConfig` (O-161): Node's SDK only uses it to derive env vars; it is
    /// never forwarded in the initialize request (FA1-15).
    pub tool_config: Option<Value>,
    /// `effort` as `if (effort) String(effort)` (FA1-17).
    pub effort: Option<String>,
    /// `maxTurns` as the SDK's `String(value)` (FA1-08); `None` for a falsy
    /// value (`0`, `false`, `""`, `null`, `NaN`), which emits no flag.
    pub max_turns: Option<String>,
    /// `maxBudgetUsd` as JS `String()`; `null` is present and becomes `null`
    /// (FA3-06: Node guards with `!== void 0`, not truthiness).
    pub max_budget_usd: Option<String>,
    /// `fallbackModel` as `if (fallbackModel) String(fallbackModel)` (FA1-17).
    pub fallback_model: Option<String>,
    /// `agent` as `if (agent) String(agent)` (FA1-17).
    pub agent: Option<String>,
    /// `agents` (O-144): custom subagent definitions, passed to the SDK's
    /// `initialize` request verbatim. Only a truthy value is sent.
    pub agents: Option<Value>,
    /// `hooks` (O-169): the client's own hook tables, merged with the adapter's
    /// ACP hooks in the `initialize` request (`acp-agent.ts:11280-11390`).
    /// Only an object is usable; a non-object is dropped like Node's spread of
    /// a non-object (`...null` is a no-op, other primitives spread nothing).
    pub hooks: Option<Value>,
    pub include_hook_events: bool,
    pub additional_directories: Vec<PathBuf>,
    /// `persistSession`; only an explicit `false` emits `--no-session-persistence`.
    pub persist_session: Option<bool>,
    pub strict_mcp_config: bool,
    /// `promptSuggestions` verbatim (FA3b-05): Node spreads the raw client
    /// value over its `true` default, so `null`/`0`/a string reach the CLI
    /// (`acp-agent.ts:11103,11233-11236`).
    pub prompt_suggestions: Option<Value>,
    pub allow_dangerously_skip_permissions: Option<bool>,
    pub setting_sources: Option<Vec<String>>,
    pub resume: Option<String>,
    /// `thinking` option (SDK `ThinkingConfig`); `has_thinking` records whether
    /// the key was present, so an explicit (even invalid) value suppresses the
    /// `MAX_THINKING_TOKENS` fallback exactly like the Node spread.
    pub thinking: Option<ThinkingConfig>,
    pub has_thinking: bool,
    /// Legacy `maxThinkingTokens` (SDK fallback when `thinking` is absent).
    pub max_thinking_tokens: Option<u64>,
    /// `executableArgs`, prepended to the child argv.
    pub executable_args: Vec<String>,
    /// `plugins` entries.
    pub plugins: Vec<Plugin>,
    /// `pluginDelivery`; [`PluginDelivery::Argv`] unless the client says
    /// `initialize`.
    pub plugin_delivery: PluginDelivery,
    /// Set when `pluginDelivery` is neither `argv` nor `initialize` (the SDK
    /// throws before spawning).
    pub invalid_plugin_delivery: bool,
    /// Set when the official ACP `additionalDirectories` (or the legacy
    /// `_meta.additionalRoots` fallback) is present but not iterable: Node's
    /// `[...value]` throws, so session creation fails instead of falling back.
    pub invalid_additional_directories: bool,
    /// `projectConfigRoot` -> `--project-config-root=<r>`. The SDK guards with
    /// `!== undefined`, so a present `null` becomes `--project-config-root=null`
    /// (FA1-17).
    pub project_config_root: Option<String>,
    /// `_meta.disableBuiltInTools === true` -> `tools: []` unless
    /// `options.tools` is present (legacy shorthand).
    pub disable_built_in_tools: bool,
    /// Raw `_meta.claudeCode.options.settings` (a path string or an object).
    pub settings: Option<Value>,
    /// Raw `_meta.claudeCode.options.sandbox` (merged into the settings tier).
    pub sandbox: Option<Value>,
    /// The pre-serialised `--mcp-config` payload (`{"mcpServers":{...}}`), or
    /// `None` when no server was supplied.
    pub mcp_config: Option<String>,
    /// `settings` extra-arg value (JSON), resolved by `spawn_inputs`.
    pub settings_arg: Option<String>,
    /// `--managed-settings` extra-arg value (path or JSON), O-122.
    pub managed_settings_arg: Option<String>,
    /// `continue` -> `--continue` (O-108). Suppressed by a `sessionStore`
    /// exactly like the SDK's `continueConversation: sessionStore ? void 0`.
    pub continue_conversation: bool,
    /// `taskBudget.total` -> `--task-budget=<n>` (O-119), pre-formatted the
    /// way `String(number)` prints it.
    pub task_budget: Option<String>,
    /// `outputFormat:{type:"json_schema",schema}` -> `--json-schema=<json>`
    /// (O-120); absent for any other output format.
    pub json_schema: Option<String>,
    /// `debug` -> `--debug` (O-121).
    pub debug: bool,
    /// `debugFile` -> `--debug-file=<path>`, which wins over `debug` (O-121).
    pub debug_file: Option<String>,
    /// A truthy `sessionStore` -> `--session-mirror` (O-126); the SDK maps
    /// `sessionMirror: !!sessionStore`.
    pub session_mirror: bool,
    /// `channels` -> one `--channels=<c>` per entry (O-127).
    pub channels: Vec<String>,
    /// `permissionPrompts` -> `--permission-prompts=<v>` (O-132).
    pub permission_prompts: Option<String>,
    /// `perTaskStopAffordance` (O-84); `None` lets the RFD default apply.
    /// `perTaskStopAffordance` verbatim (FA3-09): any defined value, including
    /// `null`/`false`, reaches the initialize payload.
    pub per_task_stop_affordance: Option<Value>,
    /// `agentProgressSummaries` (O-153): the SDK sends it in `initialize` so the
    /// CLI summarises a subagent's progress; undefined (`None`) is omitted.
    pub agent_progress_summaries: Option<bool>,
    /// `planModeInstructions` (O-154): replaces the CLI's plan-mode prompt; the
    /// SDK sends it in `initialize`, undefined (`None`) is omitted.
    pub plan_mode_instructions: Option<String>,
    /// `enableFileCheckpointing` (O-136); also set for an AIR
    /// `agentFileChangeReport` client. Reaches the CLI as the
    /// `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` env var.
    pub enable_file_checkpointing: Option<bool>,
    /// `forwardSubagentText` (O-85): flattened child transcript.
    pub forward_subagent_text: Option<bool>,
    /// `emitRawSDKMessages` (U-10).
    pub raw_sdk_messages: RawSdkMessages,
    /// `forkSession` -> `--fork-session` when truthy (O-125).
    pub fork_session: bool,
    /// `resumeSessionAt` -> `--resume-session-at=<uuid>` when non-empty
    /// (O-125).
    pub resume_session_at: Option<String>,
    /// `resumeDropsTurn` -> `--resume-drops-turn=<v>` whenever the key is
    /// present, even `false`/`null` (`!== undefined`, O-125).
    pub resume_drops_turn: Option<String>,
}

/// `String(value)` for the one `--resume-drops-turn` value.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// JavaScript `String(value)` for values the SDK interpolates into argv
/// (`Be("--max-turns", p)`); numbers use JS's shortest round-trip form.
pub(crate) fn js_argv_text(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.to_string()
            } else if let Some(integer) = number.as_u64() {
                integer.to_string()
            } else {
                number
                    .as_f64()
                    .filter(|float| float.is_finite())
                    .map_or_else(|| number.to_string(), |float| format!("{float}"))
            }
        }
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_argv_text(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JavaScript truthiness (`if (value)`), shared with `agent.rs`'s FA1-07/FA1-17
/// fallback-model guard.
pub fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(boolean)) => *boolean,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// `if (value) String(value)` for the option flags the SDK guards with JS
/// truthiness and interpolates with `String()` (`sdk.mjs`
/// `ProcessTransport.initialize`): `""`, `0`, `false`, `null` and a missing key
/// emit no flag (FA1-17), while any other value is stringified like JS.
fn opt_js_string(opts: &Value, key: &str) -> Option<String> {
    let value = opts.get(key)?;
    js_truthy(Some(value)).then(|| js_argv_text(value))
}

/// JavaScript strict equality (`===`) over parsed JSON primitives, used by the
/// SDK's `fallbackModel === model` guard (FA1-07): numbers compare by value
/// (`5 === 5.0`), strings/bools by value, and a mixed-type pair (e.g. `5` vs
/// `"5"`) is never equal. Objects and arrays are distinct references, so two
/// different JSON values are never `===`.
pub fn js_strict_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

fn opt_str(opts: &Value, key: &str) -> Option<String> {
    opts.get(key).and_then(Value::as_str).map(String::from)
}

fn opt_str_ary(opts: &Value, key: &str) -> Vec<String> {
    opts.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// `[{name, value}]` -> `[(name, value)]` in array order (Node
/// `Object.fromEntries`).
fn name_value_pairs(value: &Value) -> Vec<(String, String)> {
    value
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry.get("name").and_then(Value::as_str)?.to_string(),
                        entry.get("value").and_then(Value::as_str)?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `params.mcpServers` (ACP) into SDK server configs.
///
/// Mirrors `acp-agent.ts:11029-11053`: an entry with `type:"http"|"sse"` is an
/// HTTP/SSE server, an entry WITHOUT a `type` key is stdio, and any other
/// explicit type is ignored.
fn parse_mcp_servers(value: Option<&Value>) -> Vec<(String, McpServer)> {
    let Some(servers) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for server in servers {
        let Some(object) = server.as_object() else {
            continue;
        };
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some(kind @ ("http" | "sse")) => {
                // Node keeps the server even without a `url` (`acp-agent.ts:11033-11040`).
                let url = object.get("url").and_then(Value::as_str).map(String::from);
                let headers = object.get("headers").map(name_value_pairs);
                let config = if kind == "http" {
                    McpServer::Http { url, headers }
                } else {
                    McpServer::Sse { url, headers }
                };
                out.push((name.to_string(), config));
            }
            None if !object.contains_key("type") => {
                let Some(command) = object.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let args = object.get("args").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>()
                });
                let env = object.get("env").map(name_value_pairs);
                out.push((
                    name.to_string(),
                    McpServer::Stdio {
                        command: command.to_string(),
                        args,
                        env,
                    },
                ));
            }
            _ => {}
        }
    }
    out
}

/// JSON-escape one string (Node `JSON.stringify`).
fn json_string(value: &str) -> String {
    serde_json::to_string(&Value::String(value.to_string()))
        .unwrap_or_else(|_| String::from("\"\""))
}

/// Serialise `[(key, value)]` in insertion order (Node object key order).
fn pairs_json(pairs: &[(String, String)]) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&json_string(key));
        out.push(':');
        out.push_str(&json_string(value));
    }
    out.push('}');
    out
}

/// Serialise one SDK `McpServerConfig` with the exact key order Node builds
/// (`{type,url,headers}` / `{type,command,args,env}`); absent optionals are
/// omitted like `JSON.stringify` does with `undefined`.
fn server_json(server: &McpServer) -> String {
    match server {
        McpServer::Http { url, headers } => {
            let mut out = String::from("{\"type\":\"http\"");
            if let Some(url) = url {
                out.push_str(",\"url\":");
                out.push_str(&json_string(url));
            }
            if let Some(headers) = headers {
                out.push_str(",\"headers\":");
                out.push_str(&pairs_json(headers));
            }
            out.push('}');
            out
        }
        McpServer::Sse { url, headers } => {
            let mut out = String::from("{\"type\":\"sse\"");
            if let Some(url) = url {
                out.push_str(",\"url\":");
                out.push_str(&json_string(url));
            }
            if let Some(headers) = headers {
                out.push_str(",\"headers\":");
                out.push_str(&pairs_json(headers));
            }
            out.push('}');
            out
        }
        McpServer::Stdio { command, args, env } => {
            let mut out = format!("{{\"type\":\"stdio\",\"command\":{}", json_string(command));
            if let Some(args) = args {
                out.push_str(",\"args\":");
                out.push_str(&serde_json::to_string(args).unwrap_or_else(|_| String::from("[]")));
            }
            if let Some(env) = env {
                out.push_str(",\"env\":");
                out.push_str(&pairs_json(env));
            }
            out.push('}');
            out
        }
    }
}

/// Merge `_meta.claudeCode.options.mcpServers` (SDK-format object) with the ACP
/// `mcpServers` array into the `--mcp-config` JSON payload.
///
/// Node spreads the user object first, then the ACP servers, so a same-named
/// ACP server overrides the user one (`acp-agent.ts:11252-11255`). An empty
/// merge yields no flag (`Object.keys(O).length > 0`).
fn mcp_config_json(user: &Map<String, Value>, acp: &[(String, McpServer)]) -> Option<String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for (name, value) in user {
        let serialized = acp
            .iter()
            .find(|(acp_name, _)| acp_name == name)
            .map(|(_, server)| server_json(server))
            .unwrap_or_else(|| {
                serde_json::to_string(value).unwrap_or_else(|_| String::from("null"))
            });
        entries.push((name.clone(), serialized));
    }
    for (name, server) in acp {
        if !user.contains_key(name) {
            entries.push((name.clone(), server_json(server)));
        }
    }
    if entries.is_empty() {
        return None;
    }
    let mut body = String::new();
    for (index, (name, serialized)) in entries.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        body.push_str(&json_string(name));
        body.push(':');
        body.push_str(serialized);
    }
    Some(format!("{{\"mcpServers\":{{{body}}}}}"))
}

/// Parse the SDK `thinking` option.
fn parse_thinking(value: &Value) -> Option<ThinkingConfig> {
    let display = value
        .get("display")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);
    match value.get("type").and_then(Value::as_str) {
        Some("adaptive") => Some(ThinkingConfig::Adaptive { display }),
        Some("enabled") => Some(ThinkingConfig::Enabled {
            budget_tokens: value.get("budgetTokens").and_then(Value::as_u64),
            display,
        }),
        Some("disabled") => Some(ThinkingConfig::Disabled),
        _ => None,
    }
}

/// JS `Number.parseInt(raw, 10)`: leading whitespace and an optional sign are
/// skipped, then the decimal-digit prefix is consumed; no digits -> `None`.
fn js_parse_int_radix10(raw: &str) -> Option<i64> {
    let trimmed = raw.trim_start();
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let magnitude: i64 = digits[..end].parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// `resolveThinkingConfig` (`acp-agent.ts:13756-13768`): unset -> `None`;
/// `0` -> disabled; a positive integer -> a fixed token budget; anything else
/// (NaN / negative) is ignored.
pub fn resolve_thinking(raw: Option<&str>) -> Option<ThinkingConfig> {
    let parsed = js_parse_int_radix10(raw?)?;
    if parsed < 0 {
        return None;
    }
    if parsed == 0 {
        Some(ThinkingConfig::Disabled)
    } else {
        Some(ThinkingConfig::Enabled {
            budget_tokens: u64::try_from(parsed).ok(),
            display: None,
        })
    }
}

/// `parseModelConfig` (`acp-agent.ts:13769-13780`): `CLAUDE_MODEL_CONFIG` must
/// be a JSON object; only `modelOverrides`/`availableModels` are kept, and an
/// empty result means "no tier".
///
/// # Errors
/// The raw value is not valid JSON, or is not a JSON object.
fn parse_model_config(raw: Option<&str>) -> Result<Option<Value>, String> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    // FA3-13: Node lets `JSON.parse` throw, so the ACP `data.details` is the
    // engine text — never a `CLAUDE_MODEL_CONFIG: ` prefix. (The exact parser
    // wording is engine-version-specific; only the non-object message is the
    // adapter's own.)
    let parsed: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    if !parsed.is_object() {
        return Err(String::from("CLAUDE_MODEL_CONFIG must be a JSON object"));
    }
    let mut out = Map::new();
    for key in ["modelOverrides", "availableModels"] {
        if let Some(value) = parsed.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    Ok(if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    })
}

/// Path keys whose arrays are concatenated by the SDK's `U4` sandbox merge.
const SANDBOX_ARRAY_MERGE: [&str; 5] = [
    "filesystem.denyRead",
    "filesystem.denyWrite",
    "network.deniedDomains",
    "credentials.files",
    "credentials.envVars",
];
/// Path keys that must be replaced, never deep-merged (`U4`).
const SANDBOX_NO_MERGE: [&str; 2] = ["ripgrep", "network.tlsTerminate"];

/// The SDK's array union (FA3-11): `[...new Set([...existing, ...overlay])]`.
///
/// JS `Set` dedupes primitives by value and objects/arrays by reference. JSON
/// tiers are parsed into fresh objects, so an object/array is never the same
/// reference as one from another tier (or another literal in the same array):
/// every occurrence survives, while primitive duplicates — including ones
/// already present in the lower tier — collapse to the first occurrence.
pub(crate) fn js_array_union(existing: &[Value], overlay: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(existing.len() + overlay.len());
    for item in existing.iter().chain(overlay.iter()) {
        match item {
            Value::Array(_) | Value::Object(_) => out.push(item.clone()),
            _ => {
                if !out.iter().any(|seen| seen == item) {
                    out.push(item.clone());
                }
            }
        }
    }
    out
}

/// Deep merge of a settings-file `sandbox` object with the `sandbox` option
/// (`U4`): objects merge recursively, the listed array keys concatenate with
/// duplicates removed, the `no-merge` keys and everything else replace.
fn merge_sandbox(base: &Value, overlay: &Value, path: &str) -> Value {
    let mut merged = match base {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    let Some(overlay) = overlay.as_object() else {
        return Value::Object(merged);
    };
    for (key, value) in overlay {
        if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
            continue;
        }
        let full = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let existing = merged.get(key).cloned();
        if SANDBOX_ARRAY_MERGE.contains(&full.as_str()) {
            if let (Some(Value::Array(existing)), Value::Array(overlay)) = (&existing, value) {
                merged.insert(key.clone(), Value::Array(js_array_union(existing, overlay)));
                continue;
            }
        }
        if !SANDBOX_NO_MERGE.contains(&full.as_str()) {
            if let (Some(existing @ Value::Object(_)), Value::Object(_)) = (&existing, value) {
                merged.insert(key.clone(), merge_sandbox(existing, value, &full));
                continue;
            }
        }
        merged.insert(key.clone(), value.clone());
    }
    Value::Object(merged)
}

/// The SDK's `F4(extraArgs, sandbox)`: fold the sandbox option into the
/// programmatic settings tier and return the resulting settings document.
fn apply_sandbox(settings: &Value, sandbox: &Value) -> Value {
    let mut document = match settings {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    let mut merged = merge_sandbox(document.get("sandbox").unwrap_or(&Value::Null), sandbox, "");

    let credentials_deny = sandbox
        .get("credentials")
        .and_then(|c| c.get("files"))
        .and_then(Value::as_array)
        .is_some_and(|files| {
            files
                .iter()
                .any(|file| file.get("mode").and_then(Value::as_str) == Some("deny"))
        });
    let has_filesystem = sandbox.get("filesystem").is_some() || credentials_deny;
    let filesystem_disabled_is_explicit = sandbox
        .get("filesystem")
        .and_then(|f| f.get("disabled"))
        .is_some();
    if has_filesystem && !filesystem_disabled_is_explicit {
        if let Some(filesystem) = merged.get_mut("filesystem").and_then(Value::as_object_mut) {
            if filesystem.get("disabled") == Some(&Value::Bool(true)) {
                filesystem.remove("disabled");
            }
        }
    }

    let network = sandbox.get("network");
    let restricts_network = network.is_some_and(|network| {
        network.get("allowedDomains").is_some()
            || network
                .get("deniedDomains")
                .and_then(Value::as_array)
                .is_some_and(|domains| !domains.is_empty())
            || network.get("strictAllowlist") == Some(&Value::Bool(true))
    });
    if restricts_network {
        if let (Some(network), Some(merged_network)) = (
            network,
            merged.get_mut("network").and_then(Value::as_object_mut),
        ) {
            for key in ["httpProxyPort", "socksProxyPort"] {
                if network.get(key).is_none() {
                    merged_network.remove(key);
                }
            }
        }
    }

    if sandbox.get("enabled") == Some(&Value::Bool(true)) {
        if let Value::Object(map) = &mut merged {
            if map.get("failIfUnavailable").is_none() {
                map.insert(String::from("failIfUnavailable"), Value::Bool(true));
            }
        }
    }

    document.insert(String::from("sandbox"), merged);
    Value::Object(document)
}

/// Resolve the programmatic settings tier (`acp-agent.ts:11175-11205`):
/// `_meta.claudeCode.options.settings` (a file path resolved against `cwd`, or
/// an inline object) is merged over `CLAUDE_MODEL_CONFIG`, then the `sandbox`
/// option is folded in by the SDK's `F4`. The result is the `--settings` value.
///
/// # Errors
/// The settings file cannot be read / parsed, or `CLAUDE_MODEL_CONFIG` is
/// invalid (the Node adapter throws before spawning).
pub fn resolve_settings_arg(
    cwd: &Path,
    env: &crate::env_snapshot::EnvSnapshot,
    settings: Option<&Value>,
    sandbox: Option<&Value>,
) -> Result<Option<String>, String> {
    let provided = match settings.filter(|value| !matches!(value, Value::Null | Value::Bool(false)))
    {
        None => None,
        Some(Value::String(path)) => {
            let path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                cwd.join(path)
            };
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("failed to read settings file: {error}"))?;
            Some(
                serde_json::from_str::<Value>(&text)
                    .map_err(|error| format!("invalid settings JSON: {error}"))?,
            )
        }
        Some(value) => Some(value.clone()),
    };

    let model_config = parse_model_config(env.get("CLAUDE_MODEL_CONFIG"))?;
    let mut configured = match (model_config, provided) {
        (Some(model_config), Some(provided)) => {
            let mut merged = model_config.as_object().cloned().unwrap_or_default();
            if let Value::Object(provided) = provided {
                for (key, value) in provided {
                    merged.insert(key, value);
                }
            }
            Some(Value::Object(merged))
        }
        (Some(model_config), None) => Some(model_config),
        (None, provided) => provided,
    };

    if let Some(sandbox) = sandbox.filter(|value| value.is_object()) {
        let base = configured
            .take()
            .unwrap_or_else(|| Value::Object(Map::new()));
        configured = Some(apply_sandbox(&base, sandbox));
    }

    Ok(
        configured
            .map(|value| serde_json::to_string(&value).unwrap_or_else(|_| String::from("{}"))),
    )
}

/// Parse `_meta.claudeCode.options.plugins`.
fn parse_plugins(opts: &Value) -> Vec<Plugin> {
    opts.get("plugins")
        .and_then(Value::as_array)
        .map(|plugins| {
            plugins
                .iter()
                .filter_map(|plugin| {
                    let object = plugin.as_object()?;
                    Some(Plugin {
                        kind: object
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        path: object.get("path").and_then(Value::as_str).map(String::from),
                        skip_mcp_discovery: object
                            .get("skipMcpDiscovery")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `_meta.claudeCode.options` (or `&Value::Null` when absent).
pub fn claude_options(params: &Value) -> ClaudeOptions {
    let opts = params
        .get("_meta")
        .and_then(|m| m.get("claudeCode"))
        .and_then(|c| c.get("options"))
        .cloned()
        .unwrap_or(Value::Null);

    let extra_args = opts
        .get("extraArgs")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(k, v)| {
                    // FA3-05: a `null` value is a bare flag; everything else is
                    // JS `String(v)` (`sdk.mjs` `oy`), so objects are
                    // `[object Object]` and arrays are comma-joined.
                    let value = (!v.is_null()).then(|| js_argv_text(v));
                    (k.clone(), value)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let env = opts
        .get("env")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut additional_directories = opt_str_ary(&opts, "additionalDirectories")
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    // `params.additionalDirectories ?? sessionMeta?.additionalRoots ?? []`,
    // then `[...value]` (`acp-agent.ts:11142-11143`): a present official field
    // wins even when malformed, a string spreads into its characters, and a
    // non-iterable value fails session creation instead of falling back.
    let official = params
        .get("additionalDirectories")
        .filter(|value| !value.is_null());
    let legacy = params
        .get("_meta")
        .and_then(|meta| meta.get("additionalRoots"))
        .filter(|value| !value.is_null());
    let (spread, invalid_additional_directories) = match official.or(legacy) {
        None => (Vec::new(), false),
        Some(Value::Array(items)) => (
            items
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect(),
            false,
        ),
        Some(Value::String(text)) => (
            text.chars().map(|c| PathBuf::from(c.to_string())).collect(),
            false,
        ),
        Some(_) => (Vec::new(), true),
    };
    additional_directories.extend(spread);

    let disable_built_in_tools = params
        .get("_meta")
        .and_then(|m| m.get("disableBuiltInTools"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let tools = match opts.get("tools") {
        // An explicit array wins over `disableBuiltInTools` (`??` in Node).
        Some(Value::Array(tools)) => Some(
            tools
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>(),
        ),
        // A non-array `tools` value (e.g. a preset object) is the SDK default.
        Some(_) => None,
        None if disable_built_in_tools => Some(Vec::new()),
        None => None,
    };

    let thinking_present = opts.get("thinking").is_some();
    let thinking = opts
        .get("thinking")
        .filter(|value| value.is_object() || value.is_boolean())
        .and_then(parse_thinking);

    let user_mcp_servers = opts
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let acp_mcp_servers = parse_mcp_servers(params.get("mcpServers"));

    let plugin_delivery_raw = opts.get("pluginDelivery");
    let (plugin_delivery, invalid_plugin_delivery) = match plugin_delivery_raw {
        None => (PluginDelivery::Argv, false),
        Some(Value::String(value)) if value == "argv" => (PluginDelivery::Argv, false),
        Some(Value::String(value)) if value == "initialize" => (PluginDelivery::Initialize, false),
        // Any other value (including `null`) makes the SDK throw.
        Some(_) => (PluginDelivery::Argv, true),
    };

    // `--task-budget`: `Be("--task-budget", taskBudget.total)` (`sdk.mjs`
    // ProcessTransport), i.e. JS `String()` of whatever `total` is. FA3-04: a
    // missing total is `undefined`, `null` is `null`, and a float uses JS's
    // shortest form (`50000.0` -> `50000`).
    let task_budget = opts
        .get("taskBudget")
        .filter(|value| js_truthy(Some(value)))
        .map(|value| {
            value
                .get("total")
                .map_or_else(|| "undefined".to_string(), js_argv_text)
        });

    // `--json-schema`: the SDK derives it from `outputFormat` only
    // (`av = outputFormat?.type === "json_schema" ? outputFormat.schema : undefined`).
    let json_schema = opts
        .get("outputFormat")
        .filter(|output| output.get("type").and_then(Value::as_str) == Some("json_schema"))
        .and_then(|output| output.get("schema"))
        .filter(|schema| js_truthy(Some(schema)))
        .and_then(|schema| serde_json::to_string(schema).ok());

    let debug = js_truthy(opts.get("debug"));
    let debug_file = opt_str(&opts, "debugFile").filter(|path| !path.is_empty());
    let session_mirror = js_truthy(opts.get("sessionStore"));
    let continue_conversation = js_truthy(opts.get("continue"));
    let permission_prompts = opt_str(&opts, "permissionPrompts").filter(|value| !value.is_empty());

    // `ProcessTransport.initialize` (`sdk.mjs`): `skills === "all" ? ["Skill"] :
    // skills.map(s => \`Skill(${s})\`)`, appended to the user's `allowedTools`
    // with a set-dedupe against the original list (FA1-06).
    let allowed_tools = {
        let mut tools = opt_str_ary(&opts, "allowedTools");
        let additions: Vec<String> = match opts.get("skills") {
            Some(Value::String(skill)) if skill == "all" => vec!["Skill".to_string()],
            Some(Value::Array(skills)) => skills
                .iter()
                .filter_map(Value::as_str)
                .map(|skill| format!("Skill({skill})"))
                .collect(),
            _ => Vec::new(),
        };
        let original: std::collections::HashSet<String> = tools.iter().cloned().collect();
        for addition in additions {
            if !original.contains(&addition) {
                tools.push(addition);
            }
        }
        tools
    };

    ClaudeOptions {
        model: opt_js_string(&opts, "model"),
        betas: opt_str_ary(&opts, "betas"),
        extra_args,
        env,
        allowed_tools,
        disallowed_tools: opt_str_ary(&opts, "disallowedTools"),
        tools,
        // FA3-09: the SDK sends `toolAliases` whenever the key is defined, so a
        // `null` value stays on the wire.
        tool_aliases: opts.get("toolAliases").cloned(),
        tool_config: opts.get("toolConfig").filter(|v| !v.is_null()).cloned(),
        effort: opt_js_string(&opts, "effort"),
        max_turns: opts
            .get("maxTurns")
            .filter(|value| js_truthy(Some(value)))
            .map(js_argv_text),
        // FA3-06: `if(f!==void 0) W.push(Be("--max-budget-usd", f))` — a
        // present `null`/string/number is stringified by JS `String()`.
        max_budget_usd: opts.get("maxBudgetUsd").map(js_argv_text),
        fallback_model: opt_js_string(&opts, "fallbackModel"),
        agent: opt_js_string(&opts, "agent"),
        // FA3-09: `buildInitializeRequest` sends `agents: initConfig.agents`
        // whenever the key is defined (`null`/`false` included).
        agents: opts.get("agents").cloned(),
        // O-169: Node spreads `userProvidedOptions?.hooks`; an absent or
        // non-object value contributes nothing.
        hooks: opts.get("hooks").filter(|value| value.is_object()).cloned(),
        // FA3-07: the SDK guards these with JS truthiness
        // (`if(this.options.includeHookEvents)` / `if(O)`), not `=== true`.
        include_hook_events: js_truthy(opts.get("includeHookEvents")),
        additional_directories,
        persist_session: opts.get("persistSession").and_then(Value::as_bool),
        strict_mcp_config: js_truthy(opts.get("strictMcpConfig")),
        prompt_suggestions: opts.get("promptSuggestions").cloned(),
        allow_dangerously_skip_permissions: opts
            .get("allowDangerouslySkipPermissions")
            .and_then(Value::as_bool),
        // FA3-02: an explicitly empty array is NOT the same as an absent key.
        // Node pushes `--setting-sources=` for `settingSources: []`
        // (`if(L!==void 0) ... L.join(",")`); the adapter's
        // `["user","project","local"]` default only applies when the client did
        // not provide the option at all.
        setting_sources: opts
            .get("settingSources")
            .map(|_| opt_str_ary(&opts, "settingSources")),
        // FA3-06: `if(w)` skips an empty-string resume (no `--resume=`).
        resume: opt_str(&opts, "resume").filter(|value| !value.is_empty()),
        thinking,
        has_thinking: thinking_present,
        max_thinking_tokens: opts.get("maxThinkingTokens").and_then(Value::as_u64),
        executable_args: opt_str_ary(&opts, "executableArgs"),
        plugins: parse_plugins(&opts),
        plugin_delivery,
        invalid_plugin_delivery,
        invalid_additional_directories,
        project_config_root: opts.get("projectConfigRoot").map(js_argv_text),
        disable_built_in_tools,
        settings: opts.get("settings").cloned(),
        sandbox: opts.get("sandbox").cloned(),
        mcp_config: mcp_config_json(&user_mcp_servers, &acp_mcp_servers),
        settings_arg: None,
        managed_settings_arg: opts
            .get("managedSettings")
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::to_string(v).ok()),
        continue_conversation,
        task_budget,
        json_schema,
        debug,
        debug_file,
        session_mirror,
        channels: opt_str_ary(&opts, "channels"),
        permission_prompts,
        per_task_stop_affordance: opts.get("perTaskStopAffordance").cloned(),
        agent_progress_summaries: opts.get("agentProgressSummaries").and_then(Value::as_bool),
        plan_mode_instructions: opt_str(&opts, "planModeInstructions"),
        enable_file_checkpointing: opts.get("enableFileCheckpointing").and_then(Value::as_bool),
        forward_subagent_text: opts.get("forwardSubagentText").and_then(Value::as_bool),
        raw_sdk_messages: parse_raw_sdk_messages(
            params
                .get("_meta")
                .and_then(|meta| meta.get("claudeCode"))
                .and_then(|claude| claude.get("emitRawSDKMessages")),
        ),
        fork_session: js_truthy(opts.get("forkSession")),
        resume_session_at: opt_str(&opts, "resumeSessionAt").filter(|value| !value.is_empty()),
        resume_drops_turn: opts.get("resumeDropsTurn").map(js_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{build_argv, SpawnOptions};
    use serde_json::json;

    fn base_spawn() -> SpawnOptions {
        SpawnOptions {
            session_id: Some("sess".into()),
            permission_prompt_tool: true,
            include_partial_messages: true,
            replay_user_messages: true,
            ..SpawnOptions::default()
        }
    }

    /// FA3-07 — `includeHookEvents`/`strictMcpConfig` are JS-truthy, so a
    /// number/string enables the flag like Node (`persistSession` stays strict
    /// `=== false`, and `allowDangerouslySkipPermissions` is already folded to
    /// a boolean by the adapter).
    #[test]
    fn fa3_07_boolean_options_use_js_truthiness() {
        let o = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "includeHookEvents": 1,
                "strictMcpConfig": "yes",
                "persistSession": 1,
            }}}
        }));
        assert!(o.include_hook_events);
        assert!(o.strict_mcp_config);
        assert_eq!(o.persist_session, None);
        let mut spawn = base_spawn();
        spawn.include_hook_events = o.include_hook_events;
        spawn.strict_mcp_config = o.strict_mcp_config;
        spawn.persist_session = o.persist_session;
        let argv = build_argv(&spawn);
        assert!(
            argv.iter().any(|a| a == "--include-hook-events"),
            "{argv:?}"
        );
        assert!(argv.iter().any(|a| a == "--strict-mcp-config"), "{argv:?}");
        assert!(
            !argv.iter().any(|a| a == "--no-session-persistence"),
            "only literal false persists off: {argv:?}"
        );
    }

    /// FA3-06 — an empty-string `resume` emits no flag; `maxBudgetUsd` is
    /// guarded by `!== void 0`, so `null` and strings reach argv.
    #[test]
    fn fa3_06_empty_resume_and_present_max_budget() {
        let o = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "resume": "",
                "maxBudgetUsd": null,
            }}}
        }));
        assert_eq!(o.resume, None);
        assert_eq!(o.max_budget_usd.as_deref(), Some("null"));

        let mut spawn = base_spawn();
        spawn.resume = o.resume;
        spawn.max_budget_usd = o.max_budget_usd;
        let argv = build_argv(&spawn);
        assert!(
            !argv.iter().any(|a| a.starts_with("--resume=")),
            "empty resume is skipped: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "--max-budget-usd=null"),
            "null is present: {argv:?}"
        );

        let numeric = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"maxBudgetUsd": "10"}}}
        }));
        assert_eq!(numeric.max_budget_usd.as_deref(), Some("10"));
    }

    /// FA3-05 — `extraArgs` values use JS `String()`: objects are
    /// `[object Object]`, arrays are comma-joined, `null` is a bare flag.
    #[test]
    fn fa3_05_extra_args_js_string() {
        let o = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"extraArgs": {
                "obj": {"a": 1},
                "arr": [1, 2],
                "flag": null,
            }}}}
        }));
        assert_eq!(
            o.extra_args,
            vec![
                ("obj".to_string(), Some("[object Object]".to_string())),
                ("arr".to_string(), Some("1,2".to_string())),
                ("flag".to_string(), None),
            ]
        );
        let mut spawn = base_spawn();
        spawn.extra_args = o.extra_args;
        let argv = build_argv(&spawn);
        let obj = argv.iter().position(|a| a == "--obj").expect("--obj");
        assert_eq!(argv[obj + 1], "[object Object]", "{argv:?}");
        let arr = argv.iter().position(|a| a == "--arr").expect("--arr");
        assert_eq!(argv[arr + 1], "1,2", "{argv:?}");
        assert!(argv.iter().any(|a| a == "--flag"), "{argv:?}");
    }

    /// FA3-04 — `taskBudget.total` is stringified like JS `String()`: missing
    /// is `undefined`, `null`/booleans are their literal text, floats lose the
    /// `.0`.
    #[test]
    fn fa3_04_task_budget_js_string() {
        let cases = [
            (json!({}), Some("undefined")),
            (json!({"total": null}), Some("null")),
            (json!({"total": true}), Some("true")),
            (json!({"total": 50000.0}), Some("50000")),
            (json!({"total": "12"}), Some("12")),
        ];
        for (task_budget, expected) in cases {
            let o = claude_options(&json!({
                "_meta": {"claudeCode": {"options": {"taskBudget": task_budget}}}
            }));
            assert_eq!(o.task_budget.as_deref(), expected);
            let mut spawn = base_spawn();
            spawn.task_budget = o.task_budget;
            if let Some(value) = expected {
                let argv = build_argv(&spawn);
                assert!(
                    argv.iter().any(|a| a == &format!("--task-budget={value}")),
                    "argv: {argv:?}"
                );
            }
        }
    }

    /// FA3-11 — sandbox array keys use the same JS `Set` union: duplicate
    /// primitives collapse, object entries are distinct references.
    #[test]
    fn fa3_11_sandbox_array_union_matches_js_set() {
        let base = json!({
            "sandbox": {"filesystem": {"denyRead": ["/a", "/a"]},
                        "credentials": {"files": [{"path": "/k"}]}}
        });
        let overlay = json!({
            "filesystem": {"denyRead": ["/a", "/b"]},
            "credentials": {"files": [{"path": "/k"}]}
        });
        let merged = merge_sandbox(&base["sandbox"], &overlay, "");
        assert_eq!(
            merged["filesystem"]["denyRead"],
            json!(["/a", "/b"]),
            "duplicate primitives collapse across the concatenation"
        );
        assert_eq!(
            merged["credentials"]["files"]
                .as_array()
                .expect("array")
                .len(),
            2,
            "the same object in two sources is a distinct reference"
        );
    }

    /// FA3-02 — an explicit `settingSources: []` yields `--setting-sources=`,
    /// while an absent key stays absent (the adapter default is applied later,
    /// in `agent.rs`).
    #[test]
    fn fa3_02_empty_setting_sources_is_kept() {
        let empty = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"settingSources": []}}}
        }));
        assert_eq!(empty.setting_sources, Some(Vec::new()));
        let mut spawn = base_spawn();
        spawn.setting_sources = empty.setting_sources.unwrap();
        let argv = build_argv(&spawn);
        assert!(
            argv.iter().any(|a| a == "--setting-sources="),
            "empty list is explicit: {argv:?}"
        );

        let absent = claude_options(&json!({}));
        assert_eq!(absent.setting_sources, None);
    }

    #[test]
    fn inv_57_options_to_argv_order() {
        let mut opts = base_spawn();
        opts.model = Some("haiku".into());
        opts.betas = vec!["b1".into(), "b2".into()];
        opts.allowed_tools = vec!["Read".into()];
        opts.disallowed_tools = vec!["Write".into()];
        opts.tools = Some(vec!["Glob".into()]);
        opts.effort = Some("medium".into());
        opts.max_turns = Some("5".into());
        opts.max_budget_usd = Some("1.5".to_string());
        opts.fallback_model = Some("so".into());
        opts.agent = Some("ag".into());
        opts.include_hook_events = true;
        opts.strict_mcp_config = true;
        opts.permission_mode = Some("plan".into());
        opts.allow_dangerously_skip_permissions = true;
        opts.additional_directories = vec![std::path::PathBuf::from("/d1")];

        let argv = build_argv(&opts);
        assert_eq!(
            argv,
            vec![
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--effort=medium",
                "--max-turns=5",
                "--max-budget-usd=1.5",
                "--model=haiku",
                "--agent=ag",
                "--betas=b1,b2",
                "--permission-prompt-tool=stdio",
                "--allowedTools=Read",
                "--disallowedTools=Write",
                "--tools=Glob",
                "--setting-sources=user,project,local",
                "--strict-mcp-config",
                "--permission-mode=plan",
                "--allow-dangerously-skip-permissions",
                "--fallback-model=so",
                "--include-hook-events",
                "--include-partial-messages",
                "--add-dir=/d1",
                "--session-id=sess",
                "--replay-user-messages",
                "",
            ]
        );
    }

    #[test]
    fn inv_57_extra_args_encoding() {
        // null -> one `--key` token; plain -> `--key`,`value`; dash (len>1) -> `--key=value`;
        // single dash '-' (len==1) -> `--key`,`-`; empty string -> `--key`, `""`;
        // numbers and booleans -> `--key`,`str`.
        let mut opts = base_spawn();
        opts.extra_args = vec![
            ("chrome".into(), None),
            ("flag".into(), Some("v".into())),
            ("dash".into(), Some("-v".into())),
            ("single_dash".into(), Some("-".into())),
            ("empty".into(), Some(String::new())),
            ("num".into(), Some("42".into())),
            ("bool".into(), Some("true".into())),
        ];
        let argv = build_argv(&opts);
        // Find the extraArgs region (after --session-id=sess).
        let idx = argv.iter().position(|a| a == "--session-id=sess").unwrap();
        assert_eq!(
            &argv[idx + 1..],
            &[
                "--chrome".to_string(),
                "--flag".to_string(),
                "v".to_string(),
                "--dash=-v".to_string(),
                "--single_dash".to_string(),
                "-".to_string(),
                "--empty".to_string(),
                String::new(),
                "--num".to_string(),
                "42".to_string(),
                "--bool".to_string(),
                "true".to_string(),
                "--replay-user-messages".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn inv_57_extra_args_types_parsing() {
        let params = json!({
            "_meta": {
                "claudeCode": {
                    "options": {
                        "extraArgs": {
                            "null_key": null,
                            "str_key": "val",
                            "bool_key": true,
                            "num_key": 123
                        }
                    }
                }
            }
        });
        let opts = claude_options(&params);
        let mut map: std::collections::BTreeMap<String, Option<String>> =
            opts.extra_args.into_iter().collect();
        assert_eq!(map.remove("null_key"), Some(None));
        assert_eq!(map.remove("str_key"), Some(Some("val".into())));
        assert_eq!(map.remove("bool_key"), Some(Some("true".into())));
        assert_eq!(map.remove("num_key"), Some(Some("123".into())));
    }

    #[test]
    fn inv_57_env_precedence() {
        // User env is merged by `apply_claude_env` (process.rs) AFTER inherited;
        // the parse side simply surfaces option env verbatim.
        let params = serde_json::json!({
            "_meta": {"claudeCode": {"options": {
                "env": {"CLAUDE_CODE_ARTIFACT": "1"}
            }}}
        });
        let opts = claude_options(&params);
        assert_eq!(
            opts.env,
            vec![("CLAUDE_CODE_ARTIFACT".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn inv_57_options_parser_fields() {
        let params = serde_json::json!({
            "_meta": {"claudeCode": {"options": {
                "model": "haiku",
                "betas": ["ctx-1m"],
                "extraArgs": {"chrome": null},
                "allowedTools": ["Read"],
                "disallowedTools": ["Bash"],
                "tools": ["Glob", "Grep"],
                "effort": "high",
                "maxTurns": 9,
                "maxBudgetUsd": 2.0,
                "fallbackModel": "sonnet",
                "agent": "builder",
                "includeHookEvents": true,
                "additionalDirectories": ["/x"],
                "persistSession": true,
                "strictMcpConfig": true,
                "promptSuggestions": false,
                "allowDangerouslySkipPermissions": false,
                "settingSources": ["user", "project"],
                "resume": "abc"
            }}}
        });
        let o = claude_options(&params);
        assert_eq!(o.model.as_deref(), Some("haiku"));
        assert_eq!(o.betas, vec!["ctx-1m"]);
        assert_eq!(o.extra_args, vec![("chrome".to_string(), None)]);
        assert_eq!(o.allowed_tools, vec!["Read"]);
        assert_eq!(o.disallowed_tools, vec!["Bash"]);
        assert_eq!(o.tools, Some(vec!["Glob".to_string(), "Grep".to_string()]));
        assert_eq!(o.effort.as_deref(), Some("high"));
        assert_eq!(o.max_turns, Some("9".to_string()));
        assert_eq!(o.max_budget_usd.as_deref(), Some("2"));
        assert_eq!(o.fallback_model.as_deref(), Some("sonnet"));
        assert_eq!(o.agent.as_deref(), Some("builder"));
        assert!(o.include_hook_events);
        assert_eq!(o.additional_directories, vec![PathBuf::from("/x")]);
        assert_eq!(o.persist_session, Some(true));
        assert!(o.strict_mcp_config);
        assert_eq!(o.prompt_suggestions, Some(serde_json::json!(false)));
        assert_eq!(o.allow_dangerously_skip_permissions, Some(false));
        assert_eq!(
            o.setting_sources,
            Some(vec!["user".to_string(), "project".to_string()])
        );
        assert_eq!(o.resume.as_deref(), Some("abc"));
    }

    /// O-144 / FA3-09 — `_meta.claudeCode.options.agents` rides to `initialize`
    /// verbatim whenever the key is defined (Node drops only `undefined`).
    #[test]
    fn o144_agents_option_passthrough() {
        let defs = serde_json::json!({
            "reviewer": {"description": "Reviews code", "prompt": "You review.", "tools": ["Read"]}
        });
        let with = claude_options(&serde_json::json!({
            "_meta": {"claudeCode": {"options": {"agents": defs.clone()}}}
        }));
        assert_eq!(with.agents, Some(defs));
        // FA3-09: `buildInitializeRequest` sends `agents: initConfig.agents`
        // whenever the key is defined (`null`/`false`/`0` included); only an
        // absent key is `None`.
        assert_eq!(claude_options(&serde_json::json!({})).agents, None);
        for value in [
            serde_json::Value::Null,
            serde_json::json!(false),
            serde_json::json!(0),
            serde_json::json!({}),
        ] {
            let present = claude_options(&serde_json::json!({
                "_meta": {"claudeCode": {"options": {"agents": value}}}
            }));
            assert_eq!(present.agents, Some(value.clone()), "{value}");
        }
    }

    #[test]
    fn inv_57_additional_directories_sources() {
        // (a) params.additionalDirectories merged after options.additionalDirectories
        let params = json!({
            "additionalDirectories": ["/p1", "/p2"],
            "_meta": {
                "claudeCode": {
                    "options": {
                        "additionalDirectories": ["/opt1"]
                    }
                }
            }
        });
        let o = claude_options(&params);
        assert_eq!(
            o.additional_directories,
            vec![
                PathBuf::from("/opt1"),
                PathBuf::from("/p1"),
                PathBuf::from("/p2")
            ]
        );

        // (b) fallback to _meta.additionalRoots when params.additionalDirectories is absent
        let params_roots = json!({
            "_meta": {
                "additionalRoots": ["/root1"],
                "claudeCode": {
                    "options": {
                        "additionalDirectories": ["/opt1"]
                    }
                }
            }
        });
        let o = claude_options(&params_roots);
        assert_eq!(
            o.additional_directories,
            vec![PathBuf::from("/opt1"), PathBuf::from("/root1")]
        );
    }

    /// WB-13 / O-131: ACP stdio servers reach the SDK `--mcp-config` payload in
    /// the SDK's exact key order, and the flag sits after `--tools`, before
    /// `--setting-sources`.
    #[test]
    fn inv_57_mcp_config_stdio() {
        let params = json!({
            "mcpServers": [{
                "name": "echo",
                "command": "/bin/echo",
                "args": ["hi"],
                "env": [{"name": "TOKEN", "value": "secret"}]
            }]
        });
        let o = claude_options(&params);
        assert_eq!(
            o.mcp_config.as_deref(),
            Some(
                r#"{"mcpServers":{"echo":{"type":"stdio","command":"/bin/echo","args":["hi"],"env":{"TOKEN":"secret"}}}}"#
            )
        );

        let mut spawn = base_spawn();
        spawn.mcp_config = o.mcp_config.clone();
        let argv = build_argv(&spawn);
        let config = argv
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config token");
        assert_eq!(argv[config + 1], o.mcp_config.unwrap());
        let tools = argv.iter().position(|a| a == "--tools=default").unwrap();
        let sources = argv
            .iter()
            .position(|a| a == "--setting-sources=user,project,local")
            .unwrap();
        assert!(tools < config && config < sources);
    }

    /// O-131: http and sse servers keep their headers in array order; optional
    /// `args`/`env`/`headers` are omitted exactly like `JSON.stringify`.
    #[test]
    fn inv_57_mcp_config_http_sse() {
        let params = json!({
            "mcpServers": [
                {"type": "http", "name": "remote", "url": "https://x.test/mcp",
                 "headers": [{"name": "Authorization", "value": "Bearer t"}]},
                {"type": "sse", "name": "events", "url": "https://y.test/sse"}
            ]
        });
        let o = claude_options(&params);
        assert_eq!(
            o.mcp_config.as_deref(),
            Some(
                r#"{"mcpServers":{"remote":{"type":"http","url":"https://x.test/mcp","headers":{"Authorization":"Bearer t"}},"events":{"type":"sse","url":"https://y.test/sse"}}}"#
            )
        );
    }

    /// FA1-12 (`acp-agent.ts:11029-11040`): an `http`/`sse` entry without a
    /// `url` is still forwarded (Node keeps the server with `url: undefined`,
    /// which `JSON.stringify` drops) instead of being silently omitted from
    /// `--mcp-config`.
    #[test]
    fn fa1_12_http_server_without_url_is_forwarded() {
        let params = json!({
            "mcpServers": [
                {"type": "http", "name": "h"},
                {"type": "sse", "name": "s",
                 "headers": [{"name": "A", "value": "b"}]}
            ]
        });
        let o = claude_options(&params);
        assert_eq!(
            o.mcp_config.as_deref(),
            Some(r#"{"mcpServers":{"h":{"type":"http"},"s":{"type":"sse","headers":{"A":"b"}}}}"#)
        );
    }

    /// O-131: `_meta.claudeCode.options.mcpServers` (SDK-format object) merges
    /// with the ACP list; a same-named ACP server overrides the user entry.
    #[test]
    fn inv_57_mcp_config_user_merge() {
        let params = json!({
            "mcpServers": [{"name": "echo", "command": "/bin/echo"}],
            "_meta": {"claudeCode": {"options": {"mcpServers": {
                "echo": {"type": "stdio", "command": "/bin/false"},
                "user": {"type": "http", "url": "https://u.test"}
            }}}}
        });
        let o = claude_options(&params);
        assert_eq!(
            o.mcp_config.as_deref(),
            Some(
                r#"{"mcpServers":{"echo":{"type":"stdio","command":"/bin/echo"},"user":{"type":"http","url":"https://u.test"}}}"#
            )
        );

        // No servers at all -> no flag (Node `Object.keys(O).length > 0`).
        assert_eq!(claude_options(&json!({"mcpServers": []})).mcp_config, None);
    }

    /// O-131: an explicit `type:"stdio"` (not http/sse) is ignored by the
    /// Node adapter's `else if (!("type" in server))` branch.
    #[test]
    fn inv_57_mcp_config_ignores_explicit_stdio_type() {
        let params = json!({
            "mcpServers": [{"type": "stdio", "name": "x", "command": "/bin/echo"}]
        });
        assert_eq!(claude_options(&params).mcp_config, None);
    }

    /// O-62 / O-117: `MAX_THINKING_TOKENS` -> thinking; JS `parseInt(raw, 10)`
    /// semantics; 0 disables, a positive value is a budget, junk is ignored.
    #[test]
    fn inv_57_thinking_from_env() {
        assert_eq!(resolve_thinking(None), None);
        assert_eq!(resolve_thinking(Some("0")), Some(ThinkingConfig::Disabled));
        assert_eq!(
            resolve_thinking(Some("2048")),
            Some(ThinkingConfig::Enabled {
                budget_tokens: Some(2048),
                display: None
            })
        );
        assert_eq!(
            resolve_thinking(Some(" 4096abc")),
            Some(ThinkingConfig::Enabled {
                budget_tokens: Some(4096),
                display: None
            })
        );
        assert_eq!(
            resolve_thinking(Some("+7")),
            Some(ThinkingConfig::Enabled {
                budget_tokens: Some(7),
                display: None
            })
        );
        assert_eq!(resolve_thinking(Some("-1")), None);
        assert_eq!(resolve_thinking(Some("abc")), None);
        assert_eq!(resolve_thinking(Some("")), None);
    }

    /// O-62 / O-117 / O-118: the SDK argv encoding of every thinking shape.
    #[test]
    fn inv_57_thinking_argv() {
        let mut spawn = base_spawn();
        spawn.thinking = Some(ThinkingConfig::Adaptive { display: None });
        let argv = build_argv(&spawn);
        assert_eq!(argv[5], "--thinking=adaptive");

        spawn.thinking = Some(ThinkingConfig::Disabled);
        let argv = build_argv(&spawn);
        assert_eq!(argv[5], "--thinking=disabled");
        assert!(!argv.iter().any(|a| a.starts_with("--thinking-display")));

        spawn.thinking = Some(ThinkingConfig::Enabled {
            budget_tokens: Some(1024),
            display: Some("summary".into()),
        });
        let argv = build_argv(&spawn);
        assert_eq!(argv[5], "--max-thinking-tokens=1024");
        assert_eq!(argv[6], "--thinking-display=summary");

        spawn.thinking = Some(ThinkingConfig::Enabled {
            budget_tokens: None,
            display: None,
        });
        let argv = build_argv(&spawn);
        assert_eq!(argv[5], "--thinking=adaptive");
    }

    /// O-49/O-129: `--settings` follows `--replay-user-messages` (Node appends
    /// the tier to the SDK `extraArgs`), unless the user already supplied a
    /// `settings` extra arg (then it is overridden in place).
    #[test]
    fn inv_57_settings_extra_arg_order() {
        let mut spawn = base_spawn();
        spawn.settings_arg = Some(r#"{"model":"haiku"}"#.into());
        let argv = build_argv(&spawn);
        let replay = argv
            .iter()
            .position(|a| a == "--replay-user-messages")
            .unwrap();
        let settings = argv.iter().position(|a| a == "--settings").unwrap();
        assert!(replay < settings);
        assert_eq!(argv[settings + 1], r#"{"model":"haiku"}"#);

        let mut spawn = base_spawn();
        spawn.extra_args = vec![("settings".into(), Some("user.json".into()))];
        spawn.settings_arg = Some(r#"{"model":"haiku"}"#.into());
        let argv = build_argv(&spawn);
        let session = argv.iter().position(|a| a == "--session-id=sess").unwrap();
        let settings = argv.iter().position(|a| a == "--settings").unwrap();
        assert_eq!(argv[settings + 1], r#"{"model":"haiku"}"#);
        // The overridden key keeps the user position (before replay).
        assert!(
            session < settings
                && settings
                    < argv
                        .iter()
                        .position(|a| a == "--replay-user-messages")
                        .unwrap()
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--settings").count(),
            1,
            "settings must be emitted once: {argv:?}"
        );
    }

    /// `_meta.claudeCode.options.thinking` overrides `MAX_THINKING_TOKENS`,
    /// including an explicit invalid value (which yields no flag).
    #[test]
    fn inv_57_thinking_option_override() {
        let o = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"thinking": {
                "type": "enabled", "budgetTokens": 512, "display": "detailed"
            }}}}
        }));
        assert!(o.has_thinking);
        assert_eq!(
            o.thinking,
            Some(ThinkingConfig::Enabled {
                budget_tokens: Some(512),
                display: Some("detailed".into())
            })
        );

        let invalid = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"thinking": {"type": "bogus"}}}}
        }));
        assert!(invalid.has_thinking);
        assert_eq!(invalid.thinking, None);
    }

    /// O-49 / O-48: a settings object is serialised as the `--settings` value;
    /// `CLAUDE_MODEL_CONFIG` is merged underneath it.
    #[test]
    fn inv_57_settings_object_and_model_config() {
        let cwd = std::env::temp_dir();
        let env = crate::env_snapshot::EnvSnapshot::empty();
        let settings = json!({"model": "haiku"});
        let arg = resolve_settings_arg(&cwd, &env, Some(&settings), None).unwrap();
        assert_eq!(arg.as_deref(), Some(r#"{"model":"haiku"}"#));

        let env = crate::env_snapshot::EnvSnapshot::from_pairs([(
            "CLAUDE_MODEL_CONFIG",
            r#"{"modelOverrides":{"claude-opus-4-6":"claude-opus-4-6"}}"#,
        )]);
        let arg = resolve_settings_arg(&cwd, &env, Some(&settings), None).unwrap();
        assert_eq!(
            arg.as_deref(),
            Some(r#"{"modelOverrides":{"claude-opus-4-6":"claude-opus-4-6"},"model":"haiku"}"#)
        );

        // No settings and no model config -> no flag.
        assert_eq!(
            resolve_settings_arg(&cwd, &crate::env_snapshot::EnvSnapshot::empty(), None, None)
                .unwrap(),
            None
        );
    }

    /// O-48: an invalid `CLAUDE_MODEL_CONFIG` throws before spawning; a
    /// non-object is rejected with the adapter's message.
    #[test]
    fn inv_57_model_config_errors() {
        let cwd = std::env::temp_dir();
        let env = crate::env_snapshot::EnvSnapshot::from_pairs([("CLAUDE_MODEL_CONFIG", "{oops")]);
        let details = resolve_settings_arg(&cwd, &env, None, None).unwrap_err();
        // FA3-13: the engine's own parse message, with no port prefix.
        assert!(
            !details.starts_with("CLAUDE_MODEL_CONFIG"),
            "no prefix: {details}"
        );
        assert!(!details.is_empty(), "a parse error is reported");

        let env = crate::env_snapshot::EnvSnapshot::from_pairs([("CLAUDE_MODEL_CONFIG", "[1]")]);
        assert_eq!(
            resolve_settings_arg(&cwd, &env, None, None).unwrap_err(),
            "CLAUDE_MODEL_CONFIG must be a JSON object"
        );

        // An empty object contributes no tier.
        let env = crate::env_snapshot::EnvSnapshot::from_pairs([("CLAUDE_MODEL_CONFIG", "{}")]);
        assert_eq!(resolve_settings_arg(&cwd, &env, None, None).unwrap(), None);
    }

    /// O-49: a settings path is resolved against the session cwd and parsed.
    #[test]
    fn inv_57_settings_file_path() {
        let root = std::env::temp_dir().join(format!(
            "acp-sdk-settings-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("my.json"), r#"{"effortLevel":"high"}"#).unwrap();
        let env = crate::env_snapshot::EnvSnapshot::empty();
        let arg = resolve_settings_arg(&root, &env, Some(&json!("my.json")), None).unwrap();
        assert_eq!(arg.as_deref(), Some(r#"{"effortLevel":"high"}"#));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// O-159: the sandbox option is folded into the settings tier by the SDK's
    /// `F4`/`U4` merge (arrays concatenate, `failIfUnavailable` defaults on).
    /// `serde_json`'s `preserve_order` keeps the JS insertion order.
    #[test]
    fn inv_57_sandbox_merge() {
        let cwd = std::env::temp_dir();
        let env = crate::env_snapshot::EnvSnapshot::empty();
        let arg = resolve_settings_arg(
            &cwd,
            &env,
            None,
            Some(&json!({"enabled": true, "network": {"deniedDomains": ["a.test"]}})),
        )
        .unwrap();
        assert_eq!(
            arg.as_deref(),
            Some(
                r#"{"sandbox":{"enabled":true,"network":{"deniedDomains":["a.test"]},"failIfUnavailable":true}}"#
            )
        );

        // Arrays under the merge keys concatenate with duplicates removed; the
        // settings file's `sandbox` object is the merge base.
        let settings = json!({"sandbox": {"filesystem": {"denyRead": ["/a"]}}});
        let arg = resolve_settings_arg(
            &cwd,
            &env,
            Some(&settings),
            Some(&json!({"filesystem": {"denyRead": ["/a", "/b"]}})),
        )
        .unwrap();
        assert_eq!(
            arg.as_deref(),
            Some(r#"{"sandbox":{"filesystem":{"denyRead":["/a","/b"]}}}"#)
        );
    }

    /// O-134: `_meta.disableBuiltInTools` is a legacy shorthand for `tools: []`;
    /// an explicit `options.tools` wins.
    #[test]
    fn inv_57_disable_built_in_tools() {
        let o = claude_options(&json!({"_meta": {"disableBuiltInTools": true}}));
        assert_eq!(o.tools, Some(Vec::new()));
        assert!(o.disable_built_in_tools);
        let mut spawn = base_spawn();
        spawn.tools = o.tools;
        assert!(build_argv(&spawn).iter().any(|a| a == "--tools="));

        let explicit = claude_options(&json!({
            "_meta": {"disableBuiltInTools": true,
                      "claudeCode": {"options": {"tools": ["Read"]}}}
        }));
        assert_eq!(explicit.tools, Some(vec!["Read".to_string()]));

        // Absent -> the SDK default preset.
        assert_eq!(claude_options(&json!({})).tools, None);
    }

    /// O-123: `plugins` -> `--plugin-dir` / `--plugin-dir-no-mcp` after
    /// `--add-dir`; `pluginDelivery:"initialize"` with plugins -> `--await-initialize`.
    #[test]
    fn inv_57_plugin_args() {
        let params = json!({
            "_meta": {"claudeCode": {"options": {
                "plugins": [
                    {"type": "local", "path": "/p1"},
                    {"type": "local", "path": "/p2", "skipMcpDiscovery": true}
                ]
            }}}
        });
        let o = claude_options(&params);
        assert_eq!(o.plugins.len(), 2);
        assert_eq!(o.plugin_delivery, PluginDelivery::Argv);
        let mut spawn = base_spawn();
        spawn.plugins = o.plugins;
        let argv = build_argv(&spawn);
        let add_dir = argv.iter().position(|a| a == "--add-dir=/d1");
        let p1 = argv.iter().position(|a| a == "--plugin-dir=/p1").unwrap();
        let p2 = argv
            .iter()
            .position(|a| a == "--plugin-dir-no-mcp=/p2")
            .unwrap();
        assert!(p1 < p2 && p2 < argv.iter().position(|a| a == "--session-id=sess").unwrap());
        if let Some(add_dir) = add_dir {
            assert!(add_dir < p1);
        }

        let init = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "pluginDelivery": "initialize",
                "plugins": [{"type": "local", "path": "/p1"}]
            }}}
        }));
        assert_eq!(init.plugin_delivery, PluginDelivery::Initialize);
        let mut spawn = base_spawn();
        spawn.plugins = init.plugins;
        spawn.plugin_delivery = init.plugin_delivery;
        let argv = build_argv(&spawn);
        assert!(argv.iter().any(|a| a == "--await-initialize"));
        assert!(!argv.iter().any(|a| a.starts_with("--plugin-dir")));

        let bad = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"pluginDelivery": "wat"}}}
        }));
        assert!(bad.invalid_plugin_delivery);
    }

    /// O-124 / O-156 / O-166: argv flags `--no-session-persistence`,
    /// `--project-config-root` and the `executableArgs` prefix.
    #[test]
    fn inv_57_persist_project_root_executable_args() {
        let params = json!({
            "_meta": {"claudeCode": {"options": {
                "persistSession": false,
                "projectConfigRoot": "/proj",
                "executableArgs": ["--inspect"]
            }}}
        });
        let o = claude_options(&params);
        assert_eq!(o.persist_session, Some(false));
        assert_eq!(o.project_config_root.as_deref(), Some("/proj"));
        assert_eq!(o.executable_args, vec!["--inspect".to_string()]);

        let mut spawn = base_spawn();
        spawn.persist_session = o.persist_session;
        spawn.project_config_root = o.project_config_root;
        spawn.executable_args = o.executable_args;
        let argv = build_argv(&spawn);
        assert_eq!(argv[0], "--inspect");
        assert_eq!(argv[1], "--output-format");
        let root = argv
            .iter()
            .position(|a| a == "--project-config-root=/proj")
            .unwrap();
        let session = argv.iter().position(|a| a == "--session-id=sess").unwrap();
        let persist = argv
            .iter()
            .position(|a| a == "--no-session-persistence")
            .unwrap();
        assert!(root < session && session < persist);

        // `persistSession: true`/absent emits nothing.
        let mut spawn = base_spawn();
        spawn.persist_session = None;
        assert!(!build_argv(&spawn)
            .iter()
            .any(|a| a == "--no-session-persistence"));
    }

    /// O-108/O-119/O-120/O-121/O-132 — the FX-1 argv flags in the SDK's exact
    /// order: `--task-budget` after `--max-budget-usd`; `--json-schema` and
    /// `--debug` after `--betas` and before `--permission-prompt-tool`;
    /// `--permission-prompts` then `--continue` then `--resume`.
    #[test]
    fn inv_57_argv_fx1_order() {
        let mut opts = base_spawn();
        opts.max_budget_usd = Some("1.5".to_string());
        opts.task_budget = Some("50000".into());
        opts.model = Some("haiku".into());
        opts.json_schema = Some(r#"{"type":"object"}"#.into());
        opts.debug = true;
        opts.permission_prompts = Some("none".into());
        opts.continue_conversation = true;
        opts.resume = Some("abc".into());

        let argv = build_argv(&opts);
        let idx = |needle: &str| {
            argv.iter()
                .position(|a| a == needle)
                .unwrap_or_else(|| panic!("missing {needle}: {argv:?}"))
        };
        assert!(idx("--max-budget-usd=1.5") < idx("--task-budget=50000"));
        assert!(idx("--task-budget=50000") < idx("--model=haiku"));
        assert!(idx("--model=haiku") < idx("--json-schema={\"type\":\"object\"}"));
        assert!(idx("--json-schema={\"type\":\"object\"}") < idx("--debug"));
        assert!(idx("--debug") < idx("--permission-prompt-tool=stdio"));
        assert!(idx("--permission-prompt-tool=stdio") < idx("--permission-prompts=none"));
        assert!(idx("--permission-prompts=none") < idx("--continue"));
        assert!(idx("--continue") < idx("--resume=abc"));
        assert_eq!(
            argv.iter().filter(|a| *a == "--continue").count(),
            1,
            "one --continue token: {argv:?}"
        );
    }

    /// O-121 — `debugFile` wins over `debug` and emits `--debug-file=<path>`.
    #[test]
    fn inv_57_argv_debug_file_wins() {
        let mut opts = base_spawn();
        opts.debug = true;
        opts.debug_file = Some("/var/log/claude.txt".into());
        let argv = build_argv(&opts);
        assert!(argv.iter().any(|a| a == "--debug-file=/var/log/claude.txt"));
        assert!(!argv.iter().any(|a| a == "--debug"), "{argv:?}");

        // Only `debug` set -> the bare flag.
        let mut opts = base_spawn();
        opts.debug = true;
        assert!(build_argv(&opts).iter().any(|a| a == "--debug"));
    }

    /// O-126/O-108 — `--session-mirror` follows `--include-partial-messages`
    /// and precedes `--project-config-root`; a `sessionStore` suppresses
    /// `--continue` exactly like the SDK's `continueConversation` ternary.
    #[test]
    fn inv_57_argv_session_mirror_order() {
        let mut opts = base_spawn();
        opts.session_mirror = true;
        opts.project_config_root = Some("/proj".into());
        opts.continue_conversation = true;
        let argv = build_argv(&opts);
        let idx = |needle: &str| argv.iter().position(|a| a == needle).unwrap();
        assert!(idx("--include-partial-messages") < idx("--session-mirror"));
        assert!(idx("--session-mirror") < idx("--project-config-root=/proj"));
        assert!(
            !argv.iter().any(|a| a == "--continue"),
            "sessionStore suppresses --continue: {argv:?}"
        );
    }

    /// O-127 — the SDK's argv builder emits one `--channels` per entry after
    /// `--resume` and before `--allowedTools`; the adapter never reaches it
    /// (the SDK does not forward `Options.channels`), so the ACP path stays
    /// silent (covered by the `spawn-channels` Node fixture).
    #[test]
    fn inv_57_argv_channels_sdk_builder() {
        let mut opts = base_spawn();
        opts.resume = Some("abc".into());
        opts.allowed_tools = vec!["Read".into()];
        opts.channels = vec!["alpha".into(), "beta".into()];
        let argv = build_argv(&opts);
        let resume = argv.iter().position(|a| a == "--resume=abc").unwrap();
        let alpha = argv.iter().position(|a| a == "--channels=alpha").unwrap();
        let beta = argv.iter().position(|a| a == "--channels=beta").unwrap();
        let allowed = argv
            .iter()
            .position(|a| a == "--allowedTools=Read")
            .unwrap();
        assert!(resume < alpha && alpha < beta && beta < allowed);
    }

    /// The FX-1 / F4b option keys parse from `_meta.claudeCode.options`, and
    /// `emitRawSDKMessages` from `_meta.claudeCode`.
    #[test]
    fn inv_57_fx1_and_f4b_option_parsing() {
        let params = json!({
            "_meta": {"claudeCode": {
                "emitRawSDKMessages": [
                    {"type": "assistant"},
                    {"type": "system", "subtype": "init"},
                    {"type": "result", "origin": "human"}
                ],
                "options": {
                    "continue": true,
                    "taskBudget": {"total": 50000},
                    "outputFormat": {"type": "json_schema", "schema": {"type": "object"}},
                    "debug": true,
                    "debugFile": "/tmp/d.txt",
                    "sessionStore": {"kind": "memory"},
                    "channels": ["alpha"],
                    "permissionPrompts": "none",
                    "perTaskStopAffordance": false,
                    "agentProgressSummaries": true,
                    "planModeInstructions": "Plan with care.",
                    "enableFileCheckpointing": true,
                    "forwardSubagentText": true
                }
            }}
        });
        let o = claude_options(&params);
        assert!(o.continue_conversation);
        assert_eq!(o.task_budget.as_deref(), Some("50000"));
        assert_eq!(o.json_schema.as_deref(), Some(r#"{"type":"object"}"#));
        assert!(o.debug);
        assert_eq!(o.debug_file.as_deref(), Some("/tmp/d.txt"));
        assert!(o.session_mirror);
        assert_eq!(o.channels, vec!["alpha".to_string()]);
        assert_eq!(o.permission_prompts.as_deref(), Some("none"));
        assert_eq!(o.per_task_stop_affordance, Some(json!(false)));
        assert_eq!(o.agent_progress_summaries, Some(true));
        assert_eq!(o.plan_mode_instructions.as_deref(), Some("Plan with care."));
        assert_eq!(o.enable_file_checkpointing, Some(true));
        assert_eq!(o.forward_subagent_text, Some(true));
        assert_eq!(
            o.raw_sdk_messages,
            RawSdkMessages::Filters(vec![
                RawSdkMessageFilter {
                    type_: "assistant".into(),
                    subtype: FilterField::Any,
                    origin: FilterField::Any
                },
                RawSdkMessageFilter {
                    type_: "system".into(),
                    subtype: FilterField::Exact("init".into()),
                    origin: FilterField::Any
                },
                RawSdkMessageFilter {
                    type_: "result".into(),
                    subtype: FilterField::Any,
                    origin: FilterField::Exact("human".into())
                }
            ])
        );

        // A non-`json_schema` output format emits no schema.
        let plain = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "outputFormat": {"type": "text"}
            }}}
        }));
        assert_eq!(plain.json_schema, None);

        // FA3-04: a missing `total` is JS `undefined` on the wire.
        let absent = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "taskBudget": {},
                "continue": false,
                "sessionStore": null,
                "debugFile": ""
            }}}
        }));
        // FA3-04: `String(taskBudget.total)` is `String(undefined)` when the
        // object carries no `total`.
        assert_eq!(absent.task_budget.as_deref(), Some("undefined"));
        assert!(!absent.continue_conversation);
        assert!(!absent.session_mirror);
        assert_eq!(absent.debug_file, None);

        // `true` / `false` / absent forms.
        assert_eq!(
            claude_options(&json!({"_meta": {"claudeCode": {"emitRawSDKMessages": true}}}))
                .raw_sdk_messages,
            RawSdkMessages::All
        );
        assert_eq!(
            claude_options(&json!({"_meta": {"claudeCode": {"emitRawSDKMessages": false}}}))
                .raw_sdk_messages,
            RawSdkMessages::Off
        );
        assert_eq!(
            claude_options(&json!({})).raw_sdk_messages,
            RawSdkMessages::Off
        );
    }

    /// U-10 — `shouldEmitRawMessage` (`acp-agent.ts:11841-11853`): `true` emits
    /// everything, `false` nothing, and a filter list matches on type, optional
    /// subtype and optional `origin.kind`.
    #[test]
    fn inv_57_raw_sdk_message_filter() {
        let assistant = json!({"type": "assistant"});
        let init = json!({"type": "system", "subtype": "init"});
        let human = json!({"type": "result", "origin": {"kind": "human"}});

        assert!(should_emit_raw_message(&RawSdkMessages::All, &assistant));
        assert!(!should_emit_raw_message(&RawSdkMessages::Off, &assistant));

        let filters = RawSdkMessages::Filters(vec![
            RawSdkMessageFilter {
                type_: "system".into(),
                subtype: FilterField::Exact("init".into()),
                origin: FilterField::Any,
            },
            RawSdkMessageFilter {
                type_: "result".into(),
                subtype: FilterField::Any,
                origin: FilterField::Exact("human".into()),
            },
        ]);
        assert!(!should_emit_raw_message(&filters, &assistant));
        assert!(should_emit_raw_message(&filters, &init));
        assert!(should_emit_raw_message(&filters, &human));
        // A subtype filter does not match a message without that subtype.
        assert!(!should_emit_raw_message(
            &filters,
            &json!({"type": "system"})
        ));
        // An origin filter does not match a message whose origin differs.
        assert!(!should_emit_raw_message(
            &filters,
            &json!({"type": "result", "origin": {"kind": "model"}})
        ));
    }

    #[test]
    fn inv_o155_o161_tool_aliases_and_tool_config() {
        let params = json!({
            "_meta": {
                "claudeCode": {
                    "options": {
                        "toolAliases": {
                            "Read": "MyReader"
                        },
                        "toolConfig": {
                            "Bash": {
                                "timeout": 30
                            }
                        }
                    }
                }
            }
        });
        let opts = claude_options(&params);
        assert_eq!(opts.tool_aliases, Some(json!({ "Read": "MyReader" })));
        assert_eq!(opts.tool_config, Some(json!({ "Bash": { "timeout": 30 } })));

        let empty = claude_options(&json!({}));
        assert_eq!(empty.tool_aliases, None);
        assert_eq!(empty.tool_config, None);
    }

    /// FA1-05 — `shouldEmitRawMessage` compares with `===`
    /// (`acp-agent.ts:11841-11853`): only an absent (`undefined`) filter field
    /// is a wildcard. A present non-string `subtype`/`origin` (JSON `null`,
    /// number, object) equals no message field, so the filter matches nothing.
    #[test]
    fn fa1_05_raw_sdk_filter_non_string_fields_match_nothing() {
        let config = parse_raw_sdk_messages(Some(&json!([
            {"type": "system", "subtype": 5}
        ])));
        assert!(!should_emit_raw_message(
            &config,
            &json!({"type": "system", "subtype": "init"})
        ));
        assert!(!should_emit_raw_message(
            &config,
            &json!({"type": "system"})
        ));

        let config = parse_raw_sdk_messages(Some(&json!([
            {"type": "result", "origin": null}
        ])));
        assert!(!should_emit_raw_message(
            &config,
            &json!({"type": "result", "origin": {"kind": "model"}})
        ));
        assert!(!should_emit_raw_message(
            &config,
            &json!({"type": "result"})
        ));

        // An absent field is still the wildcard the SDK compares to undefined.
        let config = parse_raw_sdk_messages(Some(&json!([{"type": "system"}])));
        assert!(should_emit_raw_message(
            &config,
            &json!({"type": "system", "subtype": "init"})
        ));
        assert!(should_emit_raw_message(&config, &json!({"type": "system"})));
    }

    /// FA1-06 — the SDK expands `skills` into the spawn `--allowedTools` list
    /// (`sdk.mjs` `ProcessTransport.initialize`):
    /// `skills === "all" ? ["Skill"] : skills.map(s => \`Skill(${s})\`)`,
    /// appended to the user's tools with a set-dedupe against the originals.
    #[test]
    fn fa1_06_skills_are_appended_to_allowed_tools() {
        let opts = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "allowedTools": ["Read", "Skill(pdf)"],
                "skills": ["pdf", "docx"]
            }}}
        }));
        assert_eq!(
            opts.allowed_tools,
            vec!["Read", "Skill(pdf)", "Skill(docx)"],
            "Skill(pdf) is already present and is not duplicated"
        );

        let opts = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"skills": "all"}}}
        }));
        assert_eq!(opts.allowed_tools, vec!["Skill"]);

        // The expanded list reaches the exact argv token.
        let mut spawn = base_spawn();
        spawn.allowed_tools = opts.allowed_tools;
        let argv = build_argv(&spawn);
        assert!(argv.iter().any(|a| a == "--allowedTools=Skill"), "{argv:?}");
    }

    /// FA1-07 — `fallbackModel === model` throws inside
    /// `ProcessTransport.initialize` (`sdk.mjs`), before any spawn.
    #[test]
    fn fa1_07_equal_fallback_model_is_rejected() {
        // Exercised at the `spawn_inputs` seam in `agent.rs` (the options are
        // parsed here); this keeps the parse-side contract explicit.
        let opts = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "model": "same-model",
                "fallbackModel": "same-model"
            }}}
        }));
        assert_eq!(opts.model.as_deref(), Some("same-model"));
        assert_eq!(opts.fallback_model.as_deref(), Some("same-model"));
    }

    /// FA1-08 — `if (maxTurns)` (`sdk.mjs`): `0` is falsy and emits no flag;
    /// non-integers are truthy and are stringified into `--max-turns=<value>`.
    #[test]
    fn fa1_08_max_turns_follows_js_truthiness() {
        let zero = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {"maxTurns": 0}}}
        }));
        assert_eq!(zero.max_turns, None, "0 is falsy, no turn limit");
        let mut spawn = base_spawn();
        spawn.max_turns = zero.max_turns;
        let argv = build_argv(&spawn);
        assert!(
            !argv.iter().any(|a| a.starts_with("--max-turns")),
            "{argv:?}"
        );

        // Fractional and string values are truthy and reach the flag.
        for (raw, expected) in [
            (json!(5.5), "--max-turns=5.5"),
            (json!("5"), "--max-turns=5"),
        ] {
            let opts = claude_options(&json!({
                "_meta": {"claudeCode": {"options": {"maxTurns": raw}}}
            }));
            let mut spawn = base_spawn();
            spawn.max_turns = opts.max_turns;
            let argv = build_argv(&spawn);
            assert!(
                argv.iter().any(|a| a == expected),
                "{raw} -> {expected}: {argv:?}"
            );
        }
    }

    /// FA1-17 — `model`/`effort`/`agent`/`fallbackModel` follow the SDK's
    /// `if (value)` truthiness, so `""` emits no flag (Node never spawns with an
    /// empty `--model=`), while `projectConfigRoot:null` is emitted because that
    /// flag is guarded with `!== undefined`. Truthy non-strings use
    /// `String(value)` exactly like the SDK's template interpolation.
    #[test]
    fn fa1_17_empty_option_strings_and_null_project_root_match_the_sdk() {
        let empty = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "model": "",
                "effort": "",
                "agent": "",
                "fallbackModel": "",
                "projectConfigRoot": null
            }}}
        }));
        assert_eq!(empty.model, None, "empty model emits no --model=");
        assert_eq!(empty.effort, None);
        assert_eq!(empty.agent, None);
        assert_eq!(empty.fallback_model, None);
        assert_eq!(
            empty.project_config_root.as_deref(),
            Some("null"),
            "a present null projectConfigRoot is `!== undefined`, so it emits =null"
        );

        // Node's argv for the same options: no empty flags, one =null flag.
        let spawn = SpawnOptions {
            project_config_root: empty.project_config_root,
            ..Default::default()
        };
        assert_eq!(
            build_argv(&spawn),
            vec![
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--tools=default",
                "--setting-sources=user,project,local",
                "--project-config-root=null",
            ]
        );

        // Truthy non-strings are stringified like JS `String(value)`.
        let values = claude_options(&json!({
            "_meta": {"claudeCode": {"options": {
                "model": 5,
                "effort": true,
                "agent": ["a", "b"],
                "fallbackModel": 2.5,
                "projectConfigRoot": false
            }}}
        }));
        assert_eq!(values.model.as_deref(), Some("5"));
        assert_eq!(values.effort.as_deref(), Some("true"));
        assert_eq!(values.agent.as_deref(), Some("a,b"));
        assert_eq!(values.fallback_model.as_deref(), Some("2.5"));
        assert_eq!(values.project_config_root.as_deref(), Some("false"));

        let spawn = SpawnOptions {
            model: values.model,
            effort: values.effort,
            agent: values.agent,
            fallback_model: values.fallback_model,
            project_config_root: values.project_config_root,
            ..Default::default()
        };
        let argv = build_argv(&spawn);
        for flag in [
            "--model=5",
            "--effort=true",
            "--agent=a,b",
            "--fallback-model=2.5",
            "--project-config-root=false",
        ] {
            assert!(argv.iter().any(|a| a == flag), "missing {flag}: {argv:?}");
        }
        assert!(
            !argv.iter().any(|a| a == "--model="),
            "no empty --model= token: {argv:?}"
        );
    }

    /// FA1-09 — the adapter merges `{...extraArgs, "replay-user-messages": ""}`
    /// (`acp-agent.ts:11274-11277`): a user-supplied key is overwritten in
    /// place, at its original position, not appended a second time.
    #[test]
    fn fa1_09_user_replay_user_messages_is_overwritten_in_place() {
        let mut opts = base_spawn();
        opts.replay_user_messages = true;
        opts.extra_args = vec![
            ("replay-user-messages".to_string(), Some("true".to_string())),
            ("chrome".to_string(), None),
        ];
        let argv = build_argv(&opts);
        assert_eq!(
            argv.iter()
                .filter(|a| a.as_str() == "--replay-user-messages")
                .count(),
            1,
            "one pair only: {argv:?}"
        );
        let idx = argv
            .iter()
            .position(|a| a == "--replay-user-messages")
            .unwrap();
        assert_eq!(argv[idx + 1], "", "the merged value is the empty string");
        assert_eq!(argv[idx + 2], "--chrome", "position is preserved");
        assert!(
            !argv.iter().any(|a| a == "true"),
            "the user value is overwritten: {argv:?}"
        );
    }
}
