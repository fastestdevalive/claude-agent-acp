//! The public `serve(transport, ServeOptions)` ACP agent API (Decision D9,
//! items 8.1–8.6).
//!
//! One ACP agent, any transport: the same [`serve`] runs over `Channel::duplex()`
//! and stdio. It is built with [`Agent::builder()`] (R21) and drives the
//! [`crate::session`] actor per ACP session.
//!
//! Handled methods (B1): `initialize`, `session/new`, `session/load`
//! (`--resume`, D15), `session/prompt`. Everything else returns `-32601`.
//!
//! The connection serves requests through a single untyped handler that owns
//! the session registry (`HashMap<sessionId, Session>`). Because ACP requests
//! are dispatched sequentially on the connection loop, the registry needs no
//! lock (G5): the handler mutates its owned map between messages.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use agent_client_protocol::schema::v1::{
    ReadTextFileRequest, ReadTextFileResponse, StopReason, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{Agent, ConnectTo, ConnectionTo, Error, Result, UntypedMessage};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::air::AirClient;
use crate::auth_status::{
    spawn_auth_actor_with, AuthActorConfig, AuthCmd, AuthHandle, AuthPush, SessionAuthContext,
};
use crate::client_caps::ClientCapabilities;
use crate::control::InitializeOptions;
use crate::map;
use crate::process::{SpawnOptions, Timings};
use crate::session::{Session, SessionError, SessionOptions, SessionOutbound};
use crate::session_index::connection::{IndexPaths, LoadedSession, SessionIndexConnection};
use crate::session_methods;
use crate::session_timing::SessionTiming;
use crate::system_prompt;
use crate::v2::v1_permission_response;

/// The `agentInfo` returned by `initialize`, matching the Node adapter (the
/// volatile `version` is composed from `porting/UPSTREAM_VERSION`).
fn agent_info() -> Value {
    json!({
        "name": "@agentclientprotocol/claude-agent-acp",
        "title": "Claude Agent",
        "version": crate::VERSION,
    })
}

/// Options for [`serve`] (Decision D9 / B1).
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Explicit path to the `claude` binary; `None` → `CLAUDE_CODE_EXECUTABLE`
    /// → PATH.
    pub claude_path: Option<PathBuf>,
    /// Extra env vars merged over the inherited environment.
    pub extra_env: Vec<(String, String)>,
    /// Working directory used when `session/new` omits `cwd`.
    pub default_cwd: Option<PathBuf>,
    /// Grace constants for the child lifecycle (injectable for tests).
    pub timings: Timings,
    /// Environment snapshot the adapter reads through (hermetic by default).
    pub env: crate::env_snapshot::EnvSnapshot,
    /// The file logger (`CLAUDE_AGENT_LOGS`, `index.ts:73-84`). `None` (the
    /// default) means diagnostics are not written anywhere.
    pub logger: Option<std::sync::Arc<crate::logger::Logger>>,
}

impl ServeOptions {
    /// Build options from the process environment: `CLAUDE_CODE_EXECUTABLE`
    /// (binary), `CLAUDE_CODE_DEFAULT_CWD` (cwd). Used by the stdio binary.
    /// `CLAUDE_ACP_FORCE_CANCEL_GRACE_MS` overrides the force-cancel backstop
    /// grace (milliseconds; default 30 s, R29) so the differential harness can
    /// shorten a wedged-cancel settle (11.2 / 11.T4).
    pub fn from_env() -> Self {
        Self::from_snapshot(crate::env_snapshot::EnvSnapshot::from_process())
    }

    /// Build options from an explicit environment snapshot.
    pub fn from_snapshot(env: crate::env_snapshot::EnvSnapshot) -> Self {
        let claude_path = env.get_path("CLAUDE_CODE_EXECUTABLE");
        let default_cwd = env.get_path("CLAUDE_CODE_DEFAULT_CWD");
        let mut timings = Timings::default();
        if let Some(ms) = env.get("CLAUDE_ACP_FORCE_CANCEL_GRACE_MS") {
            if let Ok(ms) = ms.parse::<u64>() {
                timings.force_cancel_grace = std::time::Duration::from_millis(ms);
            }
        }
        Self {
            claude_path,
            extra_env: Vec::new(),
            default_cwd,
            timings,
            logger: logger_from_env(&env),
            env,
        }
    }

    #[must_use]
    pub fn with_env(mut self, env: crate::env_snapshot::EnvSnapshot) -> Self {
        self.env = env;
        self
    }

    /// Attach a file logger (the stdio binary builds one from `CLAUDE_AGENT_LOGS`).
    #[must_use]
    pub fn with_logger(mut self, logger: Option<std::sync::Arc<crate::logger::Logger>>) -> Self {
        self.logger = logger;
        self
    }
}

/// Public library agent client wrapper (`ClaudeAcpAgent`, `acp-agent.ts:2757`,
/// `acp-agent.ts:9547-9555`, O-138).
#[derive(Clone)]
pub struct ClaudeAcpAgent {
    connection: ConnectionTo<agent_client_protocol::Client>,
}

impl ClaudeAcpAgent {
    /// Create a new library agent wrapper for an active ACP connection.
    #[must_use]
    pub fn new(connection: ConnectionTo<agent_client_protocol::Client>) -> Self {
        Self { connection }
    }

    /// Access the underlying connection.
    #[must_use]
    pub fn connection(&self) -> &ConnectionTo<agent_client_protocol::Client> {
        &self.connection
    }

    /// Passthrough helper for client `fs/read_text_file` (O-138, `acp-agent.ts:9547-9550`).
    pub async fn read_text_file(
        &self,
        params: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse> {
        self.connection.send_request(params).block_task().await
    }

    /// Passthrough helper for client `fs/write_text_file` (O-138, `acp-agent.ts:9552-9555`).
    pub async fn write_text_file(
        &self,
        params: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse> {
        self.connection.send_request(params).block_task().await
    }
}

/// Node `SessionModeManager.initialize`'s clamp diagnostics
/// (`session-mode.ts:157-177`): the Auto fallback for a model without Auto, or
/// a requested mode that is not in the catalog. `None` when the mode is kept.
pub(crate) fn auto_mode_clamp_log(
    initial_mode: &str,
    auto_clamped: bool,
    current_model_id: &str,
    mode_available: bool,
) -> Option<String> {
    if auto_clamped {
        return Some(format!(
            "permissions.defaultMode \"auto\" is not available for model \"{current_model_id}\"; \
             falling back to \"{}\".",
            crate::session_config::AUTO_MODE_FALLBACK
        ));
    }
    (!mode_available).then(|| {
        format!(
            "permissions.defaultMode \"{initial_mode}\" is not available in this session; \
             falling back to \"default\"."
        )
    })
}

/// The optional file logger for an environment snapshot (`index.ts:73-84`):
/// only when `CLAUDE_AGENT_LOGS` selects a directory.
fn logger_from_env(
    env: &crate::env_snapshot::EnvSnapshot,
) -> Option<std::sync::Arc<crate::logger::Logger>> {
    let logger = crate::logger::Logger::from_env(env);
    logger.enabled().then(|| std::sync::Arc::new(logger))
}

/// The AIR capability list advertised verbatim from Node (research X-2 /
/// `dist/acp-agent.js:1458-1470`). The list is independent of what the client
/// declared. Each capability Rust does not implement is a `pending` PARITY row
/// (OQ-1) — they are advertised for wire parity but stay inert.
const AIR_CAPABILITIES: &[&str] = &[
    "sessionFailure",
    "agentFileChangeReport",
    "nativeSubagentSessions",
    "asyncTasks",
    "customInstructions",
    "recommendedValue",
    "diffPatch",
    "planFile",
];

/// The process entrypoint (`process.execPath`) and the adapter's own
/// arguments (`process.argv.slice(1)` in Node, minus the script entrypoint).
/// Used for `_meta["terminal-auth"]`.
fn process_entrypoint() -> (String, Vec<String>) {
    let executable = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let argv = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    (executable, argv)
}

/// The `initialize` response, byte-matching the Node adapter at the pinned
/// upstream version (modulo ignored volatile fields); `agentInfo.version` is
/// composed from `porting/UPSTREAM_VERSION` (Decision 3).
///
/// `air` is `Some` only for a client that declared `_meta.jetbrains.air`
/// (Decision 11). Non-AIR clients get no `_meta.jetbrains`, no goal and no
/// `sessionCapabilities.subagents` (E-3/E-4); the `authStatus` marker (E-2,
/// R75-3) and the `systemPrompt` advertise (E-3, honouring is Phase 3) are
/// always present.
///
/// `caps` is the full client-capability parse (ROADMAP F2a): it decides the
/// auth methods. `env` is the hermetic snapshot that gates the remote login
/// branch, and `executable`/`argv` are the process entrypoint used by
/// `_meta["terminal-auth"]`.
pub fn initialize_response(
    air: Option<&AirClient>,
    caps: &ClientCapabilities,
    env: &crate::env_snapshot::EnvSnapshot,
    executable: &str,
    argv: &[String],
    index_capabilities: &[&str],
    hide_claude_auth: bool,
) -> Value {
    let is_air = air.is_some();
    let mut session_caps = serde_json::Map::new();
    session_caps.insert("additionalDirectories".into(), json!({}));
    session_caps.insert("close".into(), json!({}));
    session_caps.insert("delete".into(), json!({}));
    session_caps.insert("fork".into(), json!({}));
    session_caps.insert("list".into(), json!({}));
    session_caps.insert("resume".into(), json!({}));
    session_caps.insert("systemPrompt".into(), json!({ "mode": { "override": {} } }));
    if is_air {
        session_caps.insert("subagents".into(), json!({}));
    }

    // WB-04 / F2a: the pure builder lives in `client_caps`; `--hide-claude-auth`
    // drops the claude.ai login method (O-07).
    let auth_methods =
        crate::client_caps::auth_methods(caps, env, executable, argv, hide_claude_auth);

    let mut meta = serde_json::Map::new();
    meta.insert("steering".into(), json!({ "supported": true }));
    if is_air {
        // The session index capabilities follow the others, and only a client that
        // declared `sessionIndex` gets them (`sessionIndex.capabilities()`).
        let air_capabilities: Vec<&str> = AIR_CAPABILITIES
            .iter()
            .chain(index_capabilities)
            .copied()
            .collect();
        meta.insert(
            "jetbrains".into(),
            json!({
                "air": {
                    "version": 1,
                    "capabilities": air_capabilities,
                    "goal": { "version": 1, "controlMethod": "_session/goal", "actions": ["set", "clear"] },
                }
            }),
        );
    }

    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "_meta": {
                "claudeCode": { "promptQueueing": true },
                "authStatus": {},
            },
            "promptCapabilities": { "image": true, "embeddedContext": true },
            "mcpCapabilities": { "http": true, "sse": true },
            "auth": { "logout": {} },
            "providers": {},
            "loadSession": true,
            "sessionCapabilities": Value::Object(session_caps),
        },
        "agentInfo": agent_info(),
        "authMethods": auth_methods,
        "_meta": Value::Object(meta),
    })
}

/// The `session/new` and `session/load` response body (`{sessionId, modes, configOptions}`).
fn session_response(
    session_id: &str,
    modes: &crate::session_config::ModesState,
    config_options: &[crate::session_config::ConfigOption],
) -> Value {
    json!({
        "sessionId": session_id,
        "modes": modes,
        "configOptions": config_options,
    })
}

/// Generate a v4-style random UUID (the adapter uses `randomUUID()`; the
/// differential normalises it to a placeholder).
pub fn uuid_v4() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut x = (n) ^ ((c as u128) << 64) ^ (std::process::id() as u128) << 96;
    let mut b = [0u8; 16];
    for byte in b.iter_mut() {
        *byte = (x & 0xff) as u8;
        x >>= 8;
    }
    // RFC 4122 version 4 + variant bits.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The upstream local-only slash commands (`LOCAL_ONLY_COMMANDS`,
/// `acp-agent.js:288`).
const LOCAL_ONLY_COMMANDS: &[&str] = &["/context", "/heapdump", "/extra-usage"];

/// Whether a prompt's first text block is a local-only slash command (echo-less).
pub fn is_local_only_command(text: &str) -> bool {
    if !text.starts_with('/') {
        return false;
    }
    let head = text.split_whitespace().next().unwrap_or("");
    LOCAL_ONLY_COMMANDS.contains(&head)
}

/// Map an ACP `session/prompt` request's content blocks to a Claude `user`
/// frame (`promptToClaude`, `acp-agent.js:6101-6193`). `session_id` is the ACP
/// session id; `uuid` is stamped into the frame's `uuid` field by the caller.
pub fn prompt_to_claude(params: &Value, session_id: &str, uuid: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut context: Vec<Value> = Vec::new();

    if let Some(prompt) = params.get("prompt").and_then(Value::as_array) {
        for chunk in prompt {
            match chunk.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = chunk.get("text").and_then(Value::as_str).unwrap_or("");
                    // S-08: `/mcp:<server>:<command> [args]` is forwarded as
                    // `/<server>:<command> (MCP) [args]` (`acp-agent.ts:12678-12684`).
                    let text = crate::mcp_command::rewrite_mcp_prompt(text);
                    content.push(json!({"type": "text", "text": text}));
                }
                Some("resource_link") => {
                    let uri = chunk.get("uri").and_then(Value::as_str).unwrap_or("");
                    content.push(json!({"type": "text", "text": format_uri_as_link(uri)}));
                }
                Some("resource") => {
                    let resource = chunk.get("resource").cloned().unwrap_or(Value::Null);
                    if let Some(text) = resource.get("text").and_then(Value::as_str) {
                        let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
                        content.push(json!({"type": "text", "text": format_uri_as_link(uri)}));
                        context.push(json!({
                            "type": "text",
                            "text": format!("\n<context ref=\"{uri}\">\n{text}\n</context>")
                        }));
                    }
                    // Ignore blob resources (unsupported).
                }
                Some("image") => {
                    let data = chunk.get("data").and_then(Value::as_str);
                    let mime = chunk.get("mimeType").and_then(Value::as_str).unwrap_or("");
                    if let Some(data) = data {
                        content.push(json!({
                            "type": "image",
                            "source": {"type": "base64", "data": data, "media_type": mime}
                        }));
                    } else if let Some(uri) = chunk.get("uri").and_then(Value::as_str) {
                        if uri.starts_with("http") {
                            content.push(json!({
                                "type": "image",
                                "source": {"type": "url", "url": uri}
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    content.extend(context);

    json!({
        "type": "user",
        "session_id": session_id,
        "uuid": uuid,
        "message": {
            "role": "user",
            "content": content,
        },
        "parent_tool_use_id": null,
        "origin": { "kind": "human" },
    })
}

/// The prompt's plain ACP `type:"text"` chunks joined by `\n`
/// (`session-titles.ts` `onPrompt`). This is the title context source: the
/// converted SDK frame also carries embedded `<context ref=…>` file contents
/// and `[@name](uri)` links, which Node never feeds to the title generator.
pub fn acp_prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(Value::as_array)
        .map(|chunks| {
            chunks
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Format a URI as a markdown-style link (matches upstream `formatUriAsLink`).
fn format_uri_as_link(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        let name = path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(path);
        format!("[@{name}]({uri})")
    } else if uri.starts_with("zed://") {
        let name = uri
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(uri);
        format!("[@{name}]({uri})")
    } else {
        uri.to_string()
    }
}

/// The `_meta.claudeCode.options` keys whose change on `session/load` or
/// `session/resume` of a live session must rebuild the process
/// (`OPTION_REBUILDS_SESSION`, `acp-agent.ts:1601-1675`, the `true` entries).
/// Almost every option is read only when the process starts. The `false` entries
/// are managed by the adapter, per-call attach controls, live objects that cannot
/// cross JSON-RPC, or without effect on a created session.
const OPTION_REBUILDS_SESSION: &[&str] = &[
    "additionalDirectories",
    "agentProgressSummaries",
    "agents",
    "allowDangerouslySkipPermissions",
    "allowedTools",
    "betas",
    "debug",
    "debugFile",
    "disallowedTools",
    "effort",
    "enableFileCheckpointing",
    "env",
    "executableArgs",
    "extraArgs",
    "fallbackModel",
    "forwardSubagentText",
    "includeHookEvents",
    "managedSettings",
    "maxBudgetUsd",
    "maxThinkingTokens",
    "maxTurns",
    "mcpServers",
    "model",
    "outputFormat",
    "pathToClaudeCodeExecutable",
    "permissionPrompts",
    "permissionPromptToolName",
    "persistSession",
    "perTaskStopAffordance",
    "planModeInstructions",
    "pluginDelivery",
    "plugins",
    "projectConfigRoot",
    "promptSuggestions",
    "sandbox",
    "sessionStoreFlush",
    "settings",
    "settingSources",
    "skills",
    "strictMcpConfig",
    "supportedDialogKinds",
    "systemPrompt",
    "taskBudget",
    "thinking",
    "toolAliases",
    "toolConfig",
    "tools",
    "verbatimPrompts",
];

/// A JSON value with object keys sorted, so key order never changes a
/// fingerprint (`canonicalJson`, `acp-agent.ts:1678-1692`).
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical_json(&map[key])))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// A stable fingerprint of the session-defining params of `session/new|load|resume`
/// (`computeSessionFingerprint`, `acp-agent.ts:1694-1722`): a `load`/`resume` of a
/// live session whose fingerprint changed tears the session down and recreates it
/// so the process picks up the new values. MCP servers are sorted by name and
/// skills are a set, so reordering never triggers a rebuild; only the options of
/// [`OPTION_REBUILDS_SESSION`] count.
pub fn compute_session_fingerprint(params: &Value) -> String {
    let meta = params.get("_meta");
    let claude_code = meta.and_then(|m| m.get("claudeCode"));
    let mut servers: Vec<Value> = params
        .get("mcpServers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    servers.sort_by(|a, b| {
        let name = |v: &Value| {
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        name(a).cmp(&name(b))
    });
    let mut options = serde_json::Map::new();
    if let Some(given) = claude_code
        .and_then(|c| c.get("options"))
        .and_then(Value::as_object)
    {
        for (key, value) in given {
            if OPTION_REBUILDS_SESSION.contains(&key.as_str()) {
                options.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(Value::Array(skills)) = options.get("skills").cloned() {
        // `normalizeSkills`: array order and duplicates do not rebuild the Query.
        let mut set: Vec<String> = skills
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect();
        set.sort();
        set.dedup();
        options.insert(
            "skills".to_string(),
            Value::Array(set.into_iter().map(Value::String).collect()),
        );
    }
    // `undefined` fields are omitted by `JSON.stringify`; so are nulls here.
    let optional = [
        ("cwd", params.get("cwd")),
        ("additionalDirectories", params.get("additionalDirectories")),
        (
            "additionalRoots",
            meta.and_then(|m| m.get("additionalRoots")),
        ),
        ("systemPrompt", meta.and_then(|m| m.get("systemPrompt"))),
        ("clientSystemPrompt", params.get("systemPrompt")),
        (
            "disableBuiltInTools",
            meta.and_then(|m| m.get("disableBuiltInTools")),
        ),
        (
            "emitRawSDKMessages",
            claude_code.and_then(|c| c.get("emitRawSDKMessages")),
        ),
    ];
    let mut fingerprint = serde_json::Map::new();
    for (key, value) in optional {
        if let Some(value) = value.filter(|v| !v.is_null()) {
            fingerprint.insert(key.to_string(), value.clone());
        }
    }
    fingerprint.insert("mcpServers".to_string(), Value::Array(servers));
    fingerprint.insert("options".to_string(), Value::Object(options));
    canonical_json(&Value::Object(fingerprint)).to_string()
}

/// Build the child [`SpawnOptions`] for a session (8.4, rust-parity-fixes 1.3).
///
/// `cwd` comes from the ACP request (falling back to `ServeOptions.default_cwd`
/// then the process cwd); `session_id` is the minted uuid (or resumed id);
/// [`crate::sdk_options::ClaudeOptions`], the resolved permission mode and the
/// bypass availability are resolved by [`spawn_inputs`]. Unknown
/// `_meta.claudeCode.options` keys are ignored (Decision 2).
pub fn session_spawn_options(
    serve: &ServeOptions,
    cwd: Option<PathBuf>,
    session_id: String,
    claude: crate::sdk_options::ClaudeOptions,
    permission_mode: String,
    allow_bypass: bool,
    ask_disallowed: bool,
) -> SpawnOptions {
    let cwd = cwd
        .or_else(|| serve.default_cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    let mut extra_env = claude.env.clone();
    extra_env.extend(serve.extra_env.iter().cloned());
    // The SDK turns `enableFileCheckpointing` into this env var
    // (`sdk.mjs`: `if (H) env.CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING = "true"`),
    // overriding an inherited or user-supplied value (O-136). The callers store
    // the effective value (client option OR an AIR `agentFileChangeReport`
    // client) here.
    if claude.enable_file_checkpointing == Some(true) {
        extra_env.push((
            "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".to_string(),
            "true".to_string(),
        ));
    }
    let mut disallowed_tools = claude.disallowed_tools.clone();
    if ask_disallowed {
        // Decision 4: `AskUserQuestion` always follows the user list.
        disallowed_tools.push("AskUserQuestion".to_string());
    }
    let (sess_id, resume) = if let Some(r) = claude.resume {
        (None, Some(r))
    } else {
        (Some(session_id), None)
    };
    SpawnOptions {
        claude_path: serve.claude_path.clone(),
        extra_env,
        default_cwd: cwd,
        session_id: sess_id,
        resume,
        model: claude.model,
        effort: claude.effort,
        max_turns: claude.max_turns,
        max_budget_usd: claude.max_budget_usd,
        agent: claude.agent,
        betas: claude.betas,
        fallback_model: claude.fallback_model,
        include_hook_events: claude.include_hook_events,
        strict_mcp_config: claude.strict_mcp_config,
        additional_directories: claude.additional_directories,
        allowed_tools: claude.allowed_tools,
        permission_mode: Some(permission_mode),
        setting_sources: claude
            .setting_sources
            .unwrap_or_else(|| vec!["user".into(), "project".into(), "local".into()]),
        disallowed_tools,
        tools: claude.tools,
        thinking: claude.thinking,
        mcp_config: claude.mcp_config,
        executable_args: claude.executable_args,
        plugins: claude.plugins,
        plugin_delivery: claude.plugin_delivery,
        fork_session: claude.fork_session,
        resume_session_at: claude.resume_session_at,
        resume_drops_turn: claude.resume_drops_turn,
        persist_session: claude.persist_session,
        project_config_root: claude.project_config_root,
        extra_args: claude.extra_args,
        settings_arg: claude.settings_arg,
        continue_conversation: claude.continue_conversation,
        task_budget: claude.task_budget,
        json_schema: claude.json_schema,
        debug: claude.debug,
        debug_file: claude.debug_file,
        session_mirror: claude.session_mirror,
        // The SDK's argv builder has a `--channels` branch, but the pinned SDK
        // never forwards `Options.channels` into the process transport
        // (`sdk.mjs` builds the transport options without it), so the Node
        // adapter emits no such flag. `ClaudeOptions.channels` is therefore
        // parse-only: the ACP path stays silent, like Node (verified by the
        // captured `spawn-channels.argv.json`, O-127).
        channels: Vec::new(),
        permission_prompts: claude.permission_prompts,
        managed_settings_arg: claude.managed_settings_arg,
        permission_prompt_tool: true,
        allow_dangerously_skip_permissions: allow_bypass,
        replay_user_messages: true,
        include_partial_messages: true,
        env_snapshot: serve.env.clone(),
    }
}

/// Resolve the spawn inputs for a `session/new|load` request (Decision 2/3/4):
/// `_meta.claudeCode.options`, the programmatic settings tier, thinking, the
/// permission mode (explicit > resumed > settings; Phase 1 has no explicit/
/// resumed source), and whether bypass is available.
///
/// Node IGNORES `_meta.claudeCode.options.permissionMode` (X3): the mode comes
/// from settings only. `allow_bypass` combines root/`IS_SANDBOX`, the settings'
/// `disableBypassPermissionMode` and the client's
/// `allowDangerouslySkipPermissions !== false` (R6).
///
/// The programmatic settings tier (`_meta...settings` + `CLAUDE_MODEL_CONFIG` +
/// `sandbox`) is resolved here because it can fail (unreadable/invalid settings
/// file, invalid `CLAUDE_MODEL_CONFIG`, invalid `pluginDelivery`, unsupported
/// plugin type) exactly where the Node adapter throws before spawning.
///
/// # Errors
/// A settings file cannot be read/parsed, `CLAUDE_MODEL_CONFIG` is invalid, or
/// `pluginDelivery`/`plugins` are unsupported.
pub fn spawn_inputs(
    params: &Value,
    cwd: Option<&std::path::Path>,
    serve: &ServeOptions,
    resumed_id: Option<&str>,
) -> Result<(crate::sdk_options::ClaudeOptions, String, bool), Error> {
    let dir = cwd
        .map(std::path::Path::to_path_buf)
        .or_else(|| serve.default_cwd.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    // ONE injection channel: `EnvSnapshot::managed_docs`. `load` reads the
    // injected documents when the snapshot carries them (`Some`, hermetic in
    // tests) and the platform sources otherwise (production).
    let settings = crate::settings::load(&dir, &serve.env);
    let mut claude = crate::sdk_options::claude_options(params);

    // Programmatic settings tier: `_meta.claudeCode.options.settings` (path or
    // object) merged over `CLAUDE_MODEL_CONFIG`, then the sandbox option.
    claude.settings_arg = crate::sdk_options::resolve_settings_arg(
        &dir,
        &serve.env,
        claude.settings.as_ref(),
        claude.sandbox.as_ref(),
    )
    .map_err(|details| Error::internal_error().data(json!({ "details": details })))?;

    // Thinking: a user `thinking` option overrides `MAX_THINKING_TOKENS`; the
    // legacy `maxThinkingTokens` is the SDK's fallback when neither is set.
    if !claude.has_thinking {
        claude.thinking =
            crate::sdk_options::resolve_thinking(serve.env.get("MAX_THINKING_TOKENS"));
    }
    if claude.thinking.is_none() {
        if let Some(tokens) = claude.max_thinking_tokens {
            claude.thinking = Some(if tokens == 0 {
                crate::sdk_options::ThinkingConfig::Disabled
            } else {
                crate::sdk_options::ThinkingConfig::Enabled {
                    budget_tokens: Some(tokens),
                    display: None,
                }
            });
        }
    }

    // The SDK rejects an unknown `pluginDelivery` and a non-local plugin
    // before spawning (unless plugins are delivered over initialize).
    if claude.invalid_plugin_delivery {
        return Err(Error::internal_error().data(json!({
            "details": "Invalid pluginDelivery. Expected 'argv' or 'initialize'."
        })));
    }
    if claude.plugin_delivery == crate::sdk_options::PluginDelivery::Argv {
        if let Some(plugin) = claude.plugins.iter().find(|plugin| plugin.kind != "local") {
            return Err(Error::internal_error().data(json!({
                "details": format!("Unsupported plugin type: {}", plugin.kind)
            })));
        }
    }

    let is_sandbox = serve
        .env
        .get("IS_SANDBOX")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let allow_bypass = crate::permission_mode::allow_bypass(crate::process::is_root(), is_sandbox)
        && !settings.disable_bypass
        && claude.allow_dangerously_skip_permissions != Some(false);

    let effective_resumed = resumed_id.or(claude.resume.as_deref());
    let resumed_mode = effective_resumed.and_then(|id| {
        let tail = crate::resumed_session::read_resumed_tail_with_env(id, true, &serve.env);
        tail.permission_mode
    });

    let resumed_value = resumed_mode.as_deref().map(Value::from);
    let permission_mode = crate::permission_mode::resolve_initial_permission_mode_logged(
        None,
        resumed_value.as_ref(),
        settings.default_mode_value.as_ref(),
        serve.logger.as_deref(),
        allow_bypass,
    );
    Ok((claude, permission_mode, allow_bypass))
}

/// A validated `_session/steering` request (`parseSteerRequest`,
/// `acp-agent.ts:708-738`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerRequest {
    pub session_id: String,
    /// The content blocks (handed to `prompt_to_claude`).
    pub prompt: Vec<Value>,
    /// `_meta.steering.idleBehavior` when the client opted into the host-owned
    /// fallback (`"promptRequired"`).
    pub prompt_required: bool,
}

/// Port of `parseSteerRequest` (`acp-agent.ts:708-738`).
///
/// Every rejection is `RequestError.invalidParams(undefined, "<text>")`: code
/// `-32602`, message `"Invalid params: <text>"`, and **no** `data` — unlike the
/// older Rust shape that put the text in `data`.
///
/// `params` must be an object (JS arrays are objects too, so an array falls
/// through to the `sessionId` check, exactly like destructuring it); `sessionId`
/// must be a non-empty string; `prompt` must be a non-empty array; and
/// `_meta.steering.idleBehavior`, when present, must be the string
/// `"promptRequired"` (a non-string value is rejected, not ignored).
pub fn parse_steer_request(params: &Value) -> Result<SteerRequest, Error> {
    let invalid = |text: &str| Error::new(-32602, format!("Invalid params: {text}"));
    if params.is_null() || params.is_string() || params.is_number() || params.is_boolean() {
        return Err(invalid("steer params must be an object"));
    }
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return Err(invalid("steer params require a non-empty sessionId"));
    };
    if session_id.is_empty() {
        return Err(invalid("steer params require a non-empty sessionId"));
    }
    let Some(prompt) = params.get("prompt").and_then(Value::as_array) else {
        return Err(invalid("steer params require a non-empty prompt array"));
    };
    if prompt.is_empty() {
        return Err(invalid("steer params require a non-empty prompt array"));
    }
    // `_meta` is read only when it is an object; `steering` likewise. A
    // present-but-non-string `idleBehavior` is not `undefined`, so it is
    // rejected (`acp-agent.ts:729-733`).
    let idle_behavior = params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("steering"))
        .and_then(Value::as_object)
        .and_then(|steering| steering.get("idleBehavior"));
    let prompt_required = match idle_behavior {
        None => false,
        Some(Value::String(value)) if value == "promptRequired" => true,
        Some(_) => return Err(invalid("unsupported steering idleBehavior")),
    };
    Ok(SteerRequest {
        session_id: session_id.to_string(),
        prompt: prompt.clone(),
        prompt_required,
    })
}

/// Decide the `_session/steering` outcome (R32 / B1 row).
///
/// `idle_behavior` is validated by [`parse_steer_request`] before routing, so
/// this only maps whether a turn is in flight to the wire outcome:
///
/// - in flight → `{outcome:"injected"}` (the caller injects at `now` priority),
/// - idle + `promptRequired` → `{outcome:"promptRequired", reason:"noRunningTurn"}`,
/// - idle, no opt-in → `{outcome:"startedNewTurn"}` (detached new turn).
///
/// A non-`promptRequired` value still yields the same `invalidParams` shape if
/// a caller passes one (the unit-test seam).
pub fn steer_outcome(turn_in_flight: bool, idle_behavior: Option<&str>) -> Result<Value, Error> {
    if idle_behavior.is_some_and(|b| b != "promptRequired") {
        return Err(Error::new(
            -32602,
            "Invalid params: unsupported steering idleBehavior",
        ));
    }
    if turn_in_flight {
        Ok(json!({ "outcome": "injected" }))
    } else if idle_behavior == Some("promptRequired") {
        Ok(json!({ "outcome": "promptRequired", "reason": "noRunningTurn" }))
    } else {
        Ok(json!({ "outcome": "startedNewTurn" }))
    }
}

/// Port of `parseAsyncTaskStopRequest` (`acp-agent.js:241`): the trimmed
/// `(sessionId, asyncTaskId)`, else `-32602` with the Node's message and NO
/// `data`.
fn parse_async_task_stop_params(params: &Value) -> Result<(String, String), Error> {
    let invalid = |message: &str| Error::new(-32602, format!("Invalid params: {message}"));
    let Some(object) = params.as_object() else {
        return Err(invalid("async task stop params must be an object"));
    };
    let field = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };
    let session_id = field("sessionId");
    let task_id = field("asyncTaskId");
    if session_id.is_empty() {
        return Err(invalid(
            "async task stop params require a non-empty sessionId",
        ));
    }
    if task_id.is_empty() {
        return Err(invalid(
            "async task stop params require a non-empty asyncTaskId",
        ));
    }
    Ok((session_id, task_id))
}

/// Map a [`SessionError`] to the JSON-RPC error's `(message, data)` (review-04
/// wiring: the actor maps `AuthRequired` → authRequired, else `internalError`
/// with `errorKind` data). The message is the full `"Internal error: <detail>"`
/// wire text; `data` is `None` when upstream omits it (e.g. `session ended`).
pub(crate) fn session_error_to_rpc(err: &SessionError) -> (String, Option<Value>) {
    use crate::turn::FailureKind::*;
    let (message, data) = match err {
        SessionError::PromptFailed {
            kind,
            message,
            assistant_had_error,
        } => {
            let data = match kind {
                AuthRequired => None,
                // `errorKindData(lastAssistantError)` (acp-agent.js:5282) only
                // attaches `data.errorKind` when the assistant frame carried an
                // `error`; otherwise the wire error has no `data`.
                ProviderError if *assistant_had_error => {
                    Some(json!({"errorKind": "provider_error"}))
                }
                BudgetExhausted if *assistant_had_error => {
                    Some(json!({"errorKind": "budget_exhausted"}))
                }
                ContextExhausted if *assistant_had_error => {
                    Some(json!({"errorKind": "context_exhausted"}))
                }
                // `NoResult` is the idle-without-result #825 fail
                // (acp-agent.js:2154), which attaches `errorKindData("no_result")`
                // UNCONDITIONALLY — unlike the provider/usage failures above it
                // does not depend on `lastAssistantError` (phase-13 audit of
                // `idle-without-result`).
                NoResult => Some(json!({"errorKind": "no_result"})),
                // E-12: `errorKindData("incomplete_tool_call")`, unconditional.
                IncompleteToolCall => Some(json!({"errorKind": "incomplete_tool_call"})),
                _ => None,
            };
            let message = if *kind == AuthRequired {
                format!("authRequired: {message}")
            } else {
                message.clone()
            };
            (message, data)
        }
        // A plain `Error` reaches the ACP router as `Internal error` with the
        // message in `data.details` (jsonrpc `errorToResult`), unlike the
        // adapter's own `internalErrorForClient` messages below.
        SessionError::ChildErrorResult { text } => {
            return (
                "Internal error".to_string(),
                Some(json!({"details": format!("Claude Code returned an error result: {text}")})),
            );
        }
        // A defining model-pin failure: Node throws the SDK `Error`, so the
        // router reports `Internal error` with the thrown message as details
        // (F5b/A-06).
        SessionError::ModelSwitchFailed(message) => {
            return (
                "Internal error".to_string(),
                Some(json!({"details": message})),
            );
        }
        // `internalErrorForClient({errorKind:"transport_lost"})` for an AIR client
        // carries no detail: the message stays bare `Internal error`.
        SessionError::TransportLost => {
            return (
                "Internal error".to_string(),
                Some(json!({"errorKind": "transport_lost"})),
            );
        }
        // A plain `Error` thrown by the restart: `Internal error` with the
        // message as its details.
        SessionError::RestartFailed(message) => {
            return (
                "Internal error".to_string(),
                Some(json!({"details": message})),
            );
        }
        SessionError::WorkerShutdown => {
            return (
                "Internal error".to_string(),
                Some(json!({"errorKind": "worker_shutdown"})),
            );
        }
        SessionError::Closed => (crate::turn::SESSION_ENDED_MESSAGE.to_string(), None),
        SessionError::InvalidRequest(message) => (message.clone(), None),
        SessionError::Start(m) => (m.clone(), None),
    };
    (format!("Internal error: {message}"), data)
}

/// Build a flat JSON-RPC error from `session_error_to_rpc`'s `(message, data)`
/// — a single `{code, message, data?}` object matching the Node adapter, never
/// a nested/stringified payload.
fn rpc_error(rpc: (String, Option<Value>)) -> Error {
    let (message, data) = rpc;
    let mut err = Error::new(-32603, message);
    if let Some(data) = data {
        err = err.data(data);
    }
    err
}

/// Convert a `SessionError` to the JSON-RPC wire error (`RequestError.authRequired`
/// code -32000 for `AuthRequired`, else -32603).
pub(crate) fn session_rpc_error(err: &SessionError) -> Error {
    if let SessionError::InvalidRequest(message) = err {
        return Error::new(-32600, message.clone());
    }
    if let SessionError::PromptFailed {
        kind: crate::turn::FailureKind::AuthRequired,
        ..
    } = err
    {
        return Error::new(-32000, "Authentication required");
    }
    rpc_error(session_error_to_rpc(err))
}

/// The intake error of a v2 prompt whose session actor is gone (NEW-7): the
/// session ended before it could take the prompt in, which v1 answers with
/// `internalError(undefined, SESSION_ENDED_MESSAGE)` (`acp-agent.ts:3805-3807`).
/// `-32800` stays reserved for a prompt an explicit cancel ended.
pub(crate) fn dropped_intake_error() -> crate::turn_events::V2IntakeError {
    crate::turn_events::V2IntakeError::Failed(session_error_jsonrpc(&SessionError::Closed))
}

/// The error a failed turn reports as a JSON-RPC error object — the form a v2
/// client reads on its `idle` (`jsonRpcError`, `v2/prompt.ts:114-126`). It is
/// v1's own wire error (`session_rpc_error`), so both versions report the same
/// identity (NEW-5).
pub(crate) fn session_error_jsonrpc(err: &SessionError) -> crate::turn_events::JsonRpcError {
    let error = session_rpc_error(err);
    crate::turn_events::JsonRpcError {
        code: i32::from(error.code),
        message: error.message,
        data: error.data,
    }
}

/// The JSON-RPC error for a failed `session/set_mode` / `session/set_config_option`.
/// A session whose stream already ended is a husk: the request answers the same
/// "session ended" error a prompt does (`requireOpenSession` /
/// `setSessionConfigOption`, `session-mode.ts:318-320`, `acp-agent.ts:8951-8953`)
/// instead of acting on a closed query; any other failure keeps its text.
fn set_request_error(message: String) -> Error {
    if message == crate::session::SESSION_GONE {
        return session_rpc_error(&SessionError::Closed);
    }
    Error::internal_error().data(json!({ "details": message }))
}

/// A resumed session whose CLI cannot find the stored conversation answers
/// `resourceNotFound` (-32002), not `Internal error` (`acp-agent.ts:11421-11430`:
/// `RequestError.resourceNotFound(sessionId)`). Node maps the SDK's
/// "No conversation found with session ID" / "Query closed before response
/// received" initialization errors; the Rust initialize handshake surfaces the
/// CLI's own error text for the first and `ControlError::Closed`'s
/// "control channel closed" for the second (the CLI exits before answering
/// `initialize`), so the checks are the equivalent of the SDK's.
fn resumed_start_error(err: &SessionError, resumed: bool, session_id: &str) -> Error {
    if resumed {
        if let SessionError::Start(message) = err {
            if message.contains("No conversation found with session ID")
                || message.ends_with("control channel closed")
            {
                return Error::new(-32002, format!("Resource not found: {session_id}"))
                    .data(json!({ "uri": session_id }));
            }
        }
    }
    session_rpc_error(err)
}

/// Serve ACP over `transport` (D9). Runs until the transport closes.
pub async fn serve(transport: impl ConnectTo<Agent> + 'static, opts: ServeOptions) -> Result<()> {
    serve_until(transport, opts, None, process_hides_claude_auth()).await
}

/// [`serve`] with `--hide-claude-auth` on whatever the process argv says
/// (`shouldHideClaudeAuth`): the claude.ai login is hidden and no session or turn
/// may be paid by a subscription. For embedders and tests that cannot pass the
/// flag on the command line.
pub async fn serve_hiding_claude_auth(
    transport: impl ConnectTo<Agent> + 'static,
    opts: ServeOptions,
) -> Result<()> {
    serve_until(transport, opts, None, true).await
}

/// Whether the process was started with `--hide-claude-auth`.
fn process_hides_claude_auth() -> bool {
    crate::hide_claude_auth::should_hide_claude_auth(&process_entrypoint().1)
}

/// [`serve`], ending early when `shutdown` (if any) flips to `true`; either way
/// every session the connection created is torn down before it returns.
async fn serve_until(
    transport: impl ConnectTo<Agent> + 'static,
    opts: ServeOptions,
    shutdown: Option<watch::Receiver<bool>>,
    hide_claude_auth: bool,
) -> Result<()> {
    // A `session/cancel` notification must reach the owning session actor. The
    // request handler owns the live registry; it publishes each new session's
    // handle (a cheap clone, not session state) over this channel so the
    // notification handler keeps its own copy — lock-free (guard G5).
    let (session_tx, session_rx) = mpsc::unbounded_channel::<(String, Session)>();
    // Every session this connection created, so that closing the connection can
    // tear each one down (`dispose`, `acp-agent.ts:8917-8920`, V-11).
    let (dispose_tx, mut dispose_rx) = mpsc::unbounded_channel::<Session>();
    let (deferred_tx, deferred_rx) = mpsc::unbounded_channel::<RegistryMsg>();
    // The spawned recreate tasks (a changed-params `load`/`resume` of a live
    // session), so the connection's end can wait for or stop them before it
    // disposes the sessions.
    let (recreate_tx, mut recreate_rx) = mpsc::unbounded_channel::<RecreateTask>();
    let registry = Registry {
        recreating: std::collections::HashMap::new(),
        recreate_tx,
        sessions: std::collections::HashMap::new(),
        session_tx,
        dispose_tx,
        air: None,
        caps: ClientCapabilities::default(),
        auth: None,
        settings_managers: std::collections::HashMap::new(),
        fingerprints: std::collections::HashMap::new(),
        deferred_tx,
        deferred_rx,
        index: session_index_connection(&opts),
        provider: None,
        gateway_auth: None,
        creation_params: std::collections::HashMap::new(),
        auth_contexts: std::collections::HashMap::new(),
        guard_states: std::collections::HashMap::new(),
        hide_claude_auth,
        protocol: crate::v2::ProtocolMode::V1,
        suppress_commands_update: false,
        provider_gate: Arc::new(tokio::sync::Semaphore::new(GATE_PERMITS as usize)),
        provider_updates: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        sessions_view: Arc::new(watch::channel(std::collections::HashMap::new()).0),
        v2_outbound: None,
        logger: opts.logger.clone(),
    };
    // The connection's handlers only forward: the connection loop also carries
    // the answers to the agent's own requests (permissions, `fs/*`, `terminal/*`),
    // so no handler may wait for something those answers unblock. The registry
    // actor owns the registry and parks the requests a provider update holds back.
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Incoming>();
    let notification_tx = incoming_tx.clone();
    let mut actor = tokio::spawn(run_registry_actor(registry, opts, incoming_rx, session_rx));
    let connected = Agent
        .builder()
        .name("claude-agent-acp-rs")
        .on_receive_request(
            async move |req: UntypedMessage,
                        responder: agent_client_protocol::Responder<Value>,
                        connection: ConnectionTo<agent_client_protocol::Client>| {
                let (served, ack) = oneshot::channel();
                let request = Incoming::Request {
                    request: Box::new((req, responder, connection)),
                    served,
                };
                if incoming_tx.send(request).is_err() {
                    return Err(Error::internal_error());
                }
                // A dropped sender means the actor is gone; `serve_until` ends the
                // connection on that, so there is nothing left to answer.
                let _ = ack.await;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: UntypedMessage, _connection| {
                if notification_tx.send(Incoming::Notification(notif)).is_err() {
                    return Err(Error::internal_error());
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport);
    let mut actor_finished = false;
    let mut actor_panic = None;
    let result = match shutdown {
        Some(mut shutdown) => tokio::select! {
            res = connected => res,
            joined = &mut actor => {
                actor_finished = true;
                let (ended, panic) = actor_ended(joined);
                actor_panic = panic;
                ended
            },
            _ = shutdown_requested(&mut shutdown) => Ok(()),
        },
        None => tokio::select! {
            res = connected => res,
            joined = &mut actor => {
                actor_finished = true;
                let (ended, panic) = actor_ended(joined);
                actor_panic = panic;
                ended
            },
        },
    };
    // The connection is gone, so nothing the actor serves can be delivered any
    // more: stop it now instead of letting it start sessions (or wait out the
    // gate) for a client that is no longer there.
    if !actor_finished {
        actor.abort();
        let _ = actor.await;
    }
    // The connection closed (or the owner asked to stop): tear down every session (cancel its work, interrupt
    // and end its actor, which drops the child) instead of relying on the drop
    // cascade, so each prompt in flight still gets its terminal state and each
    // child gets the interrupt before it is killed.
    // A recreate still tearing down or starting its session would register it
    // after the list below was read: tell each to stop, give it the dispose
    // bound to hand its new session to the list (or close it), and stop what
    // stays.
    let mut recreates = Vec::new();
    let mut aborters = Vec::new();
    while let Ok(task) = recreate_rx.try_recv() {
        task.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        aborters.push(task.handle.abort_handle());
        recreates.push(task.handle);
    }
    if tokio::time::timeout(DISPOSE_TIMEOUT, futures::future::join_all(recreates))
        .await
        .is_err()
    {
        for aborter in aborters {
            aborter.abort();
        }
    }
    let mut sessions = Vec::new();
    while let Ok(session) = dispose_rx.try_recv() {
        sessions.push(session);
    }
    dispose_sessions(&sessions).await;
    if let Some(panic) = actor_panic {
        std::panic::resume_unwind(panic);
    }
    result
}

/// How long [`dispose_sessions`] waits for every session's teardown
/// (`TEARDOWN_TURN_END_TIMEOUT_MS`-bounded per session in Node; here the session's
/// own close bound plus slack).
const DISPOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// Tear down all the connection's sessions concurrently (`dispose()`): `close` is
/// idempotent, so a session that already ended or was closed returns at once.
pub(crate) async fn dispose_sessions(sessions: &[Session]) {
    let _ = tokio::time::timeout(
        DISPOSE_TIMEOUT,
        futures::future::join_all(sessions.iter().map(Session::close)),
    )
    .await;
}

/// A handle to a running [`serve`] loop (`serve.ts:29-34`, V-20).
///
/// The owner of the process can stop the adapter cleanly: [`dispose`] asks the
/// loop to end, which drops the connection and its session registry; every
/// session actor's `claude` child is disposed by its `kill_on_drop` handle when
/// the runtime tears the actor down. [`closed`] resolves when the loop ended,
/// either because the transport closed or after [`dispose`].
pub struct ServeHandle {
    shutdown: watch::Sender<bool>,
    /// `None` once [`ServeHandle::closed`] consumed the completion signal, so a
    /// second call returns immediately instead of polling a completed oneshot.
    done: Option<oneshot::Receiver<()>>,
}

impl ServeHandle {
    /// Ask the serve loop to stop and dispose its sessions.
    pub fn dispose(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Wait for the serve loop to end (natural close or after [`dispose`]).
    /// Idempotent: once the loop has ended, later calls return immediately.
    pub async fn closed(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.await;
        }
    }
}

/// Start [`serve`] on the current runtime and return a [`ServeHandle`].
///
/// This is the library-level `serveAcp(...)` shape: `closed` is the connection
/// lifetime, `dispose` is the owner's shutdown request.
pub fn serve_handle(transport: impl ConnectTo<Agent> + 'static, opts: ServeOptions) -> ServeHandle {
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (done_tx, done) = oneshot::channel();
    tokio::spawn(async move {
        let _ = serve_with_shutdown(transport, opts, shutdown_rx).await;
        let _ = done_tx.send(());
    });
    ServeHandle {
        shutdown,
        done: Some(done),
    }
}

/// [`serve`] that also ends when `shutdown` flips to `true`.
pub async fn serve_with_shutdown(
    transport: impl ConnectTo<Agent> + 'static,
    opts: ServeOptions,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    serve_until(transport, opts, Some(shutdown), process_hides_claude_auth()).await
}

/// Resolve once `shutdown` is (or becomes) `true`.
async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

/// The registry actor ended before the connection did: a panic there must end
/// the connection (the old inline handler unwound through it) instead of leaving
/// a client that waits for answers nobody will send.
///
/// The panic payload is handed back, not raised here: the caller tears the
/// connection's sessions down first and then re-raises it.
fn actor_ended(
    joined: std::result::Result<(), tokio::task::JoinError>,
) -> (Result<()>, Option<Box<dyn std::any::Any + Send>>) {
    match joined {
        Err(error) if error.is_panic() => (Err(Error::internal_error()), Some(error.into_panic())),
        _ => (Err(Error::internal_error()), None),
    }
}

/// What the connection's handlers forward to the registry actor.
enum Incoming {
    Request {
        request: Box<ParkedRequest>,
        /// Resolved once the actor has the request: served inline, or parked for
        /// the provider update in flight. The handler waits for it so the
        /// connection does not end before a request it read was served (a
        /// `initialize` followed by EOF still answers), while a parked request
        /// never holds the connection loop.
        served: oneshot::Sender<()>,
    },
    /// Travels the same queue as the requests, so a `session/cancel` is served
    /// after the `session/prompt` that preceded it.
    Notification(UntypedMessage),
}

/// A request waiting for the provider update in flight.
type ParkedRequest = (
    UntypedMessage,
    agent_client_protocol::Responder<Value>,
    ConnectionTo<agent_client_protocol::Client>,
);

/// The requests that start or run a session's turn wait for a provider update in
/// flight (`while (this.providerUpdate) await this.providerUpdate`). `steer` and
/// `goal` do not: upstream's `steer()` never waits, and `goal` waits only where it
/// starts its own turn.
fn is_gated(method: &str) -> bool {
    matches!(
        method,
        "session/new" | "session/load" | "session/resume" | "session/fork" | "session/prompt"
    )
}

/// The registry actor: owns the registry and serves the forwarded requests one at
/// a time. A gated request that meets a provider update in flight is parked instead
/// of awaited, so the connection keeps delivering the permission answers and the
/// `session/cancel` the update waits on; notifications are never parked.
async fn run_registry_actor(
    mut registry: Registry,
    opts: ServeOptions,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    mut session_rx: mpsc::UnboundedReceiver<(String, Session)>,
) {
    let mut parked: std::collections::VecDeque<ParkedRequest> = std::collections::VecDeque::new();
    // The notification side keeps its own session-handle map, fed by the
    // registry's channel.
    let mut notified: std::collections::HashMap<String, Session> = std::collections::HashMap::new();
    loop {
        tokio::select! {
            biased;
            permit = registry.provider_gate.clone().acquire_owned(), if !parked.is_empty() => {
                let Ok(permit) = permit else { break };
                let Some((req, responder, connection)) = parked.pop_front() else {
                    continue;
                };
                let _ = handle_request(
                    &mut registry,
                    req,
                    &opts,
                    responder,
                    connection,
                    Some(permit),
                )
                .await;
            }
            item = incoming.recv() => match item {
                None => break,
                Some(Incoming::Request { request, served }) => {
                    let (req, responder, connection) = *request;
                    let permit = if is_gated(req.method()) {
                        if !parked.is_empty() {
                            parked.push_back((req, responder, connection));
                            // The request is parked; the connection loop may go on
                            // (it is answered once the gate is free).
                            let _ = served.send(());
                            continue;
                        }
                        match registry.provider_gate.clone().try_acquire_owned() {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                parked.push_back((req, responder, connection));
                                let _ = served.send(());
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let _ = handle_request(
                        &mut registry,
                        req,
                        &opts,
                        responder,
                        connection,
                        permit,
                    )
                    .await;
                    let _ = served.send(());
                }
                Some(Incoming::Notification(notif)) => {
                    while let Ok((sid, session)) = session_rx.try_recv() {
                        notified.insert(sid, session);
                    }
                    let _ = handle_notification(&notif, &notified).await;
                }
            },
        }
    }
}

/// Handle an ACP notification. `session/cancel` is routed to the owning session
/// actor (8.9); everything else is ignored.
async fn handle_notification(
    notif: &UntypedMessage,
    sessions: &std::collections::HashMap<String, Session>,
) -> Result<(), Error> {
    if notif.method() == "session/cancel" {
        let session_id = notif
            .params()
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(session) = sessions.get(&session_id) {
            let _ = session.cancel().await;
        } else {
            // `cancel` (`acp-agent.ts:8383-8388`): an id that is no session of
            // this connection may be the child session of a native subagent. The
            // RFD form grants that session a `cancel` capability; stopping its
            // task leaves the parent's work alone.
            for session in sessions.values() {
                if session.cancel_child(session_id.clone()).await {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// The permits of [`Registry::provider_gate`]: a provider update takes all of
/// them, a session request one.
pub(super) const GATE_PERMITS: u32 = 1 << 16;

/// Where a request's answer goes: the wire responder, or a capture channel when
/// the v2 surface translates a v1 answer before sending it (M-106, `v2/agent.ts`).
pub(crate) enum Reply {
    Wire(agent_client_protocol::Responder<Value>),
    Capture(mpsc::UnboundedSender<Result<Value, Error>>),
}

impl Reply {
    /// Answer the request.
    pub(crate) fn respond(self, value: Value) -> Result<(), Error> {
        match self {
            Reply::Wire(responder) => responder.respond(value),
            Reply::Capture(tx) => tx.send(Ok(value)).map_err(|_| Error::internal_error()),
        }
    }

    /// Answer the request with an error.
    pub(crate) fn respond_with_error(self, error: Error) -> Result<(), Error> {
        match self {
            Reply::Wire(responder) => responder.respond_with_error(error),
            Reply::Capture(tx) => tx.send(Err(error)).map_err(|_| Error::internal_error()),
        }
    }

    /// Answer after every update already queued for the connection's v2 outbound
    /// task, so a v2 client sees them before the response.
    pub(crate) fn respond_after(
        self,
        v2: &crate::v2::agent::V2OutboundHandle,
        response: Result<Value, Error>,
    ) -> Result<(), Error> {
        match self {
            Reply::Wire(responder) => {
                v2.respond(responder, response);
                Ok(())
            }
            Reply::Capture(tx) => tx.send(response).map_err(|_| Error::internal_error()),
        }
    }

    /// The wire responder, when this answer goes straight to the client. A
    /// captured answer has none: only the translated v2 methods capture, and
    /// none of them hands its responder to a task.
    pub(crate) fn into_wire(self) -> Option<agent_client_protocol::Responder<Value>> {
        match self {
            Reply::Wire(responder) => Some(responder),
            Reply::Capture(_) => None,
        }
    }

    /// The request's cancellation marker (`$/cancel_request`), on the wire.
    pub(crate) fn cancellation(&self) -> Option<agent_client_protocol::RequestCancellation> {
        match self {
            Reply::Wire(responder) => Some(responder.cancellation()),
            Reply::Capture(_) => None,
        }
    }
}

/// The per-connection session registry, owned by the registry actor (which serves
/// the requests sequentially, so no lock is needed).
pub(crate) struct Registry {
    sessions: std::collections::HashMap<String, Session>,
    /// Publishes each new session's handle to the notification handler.
    session_tx: mpsc::UnboundedSender<(String, Session)>,
    /// Publishes every registered session to the connection's dispose list.
    dispose_tx: mpsc::UnboundedSender<Session>,
    /// The parsed AIR client declaration from `initialize` (Decision 11),
    /// `Some` only when the client declared `_meta.jetbrains.air`.
    air: Option<AirClient>,
    /// The full `initialize.clientCapabilities` parse (ROADMAP F2a), threaded
    /// into every session's [`crate::session_ext::SessionExt`].
    caps: ClientCapabilities,
    /// The auth-status actor, spawned by the first `initialize` (G2).
    auth: Option<AuthHandle>,
    /// Settings managers per session (F5a / O-60).
    settings_managers:
        std::collections::HashMap<String, Arc<crate::settings_manager::SettingsManager>>,
    /// The [`compute_session_fingerprint`] each live session was created with
    /// (`Session.sessionFingerprint`), compared on `load`/`resume` of a live id.
    fingerprints: std::collections::HashMap<String, String>,
    /// Where a `load`/`resume` that finishes off the request handler leaves its
    /// registry entries; drained at the start of every request.
    deferred_tx: mpsc::UnboundedSender<RegistryMsg>,
    deferred_rx: mpsc::UnboundedReceiver<RegistryMsg>,
    /// The ids of sessions a spawned recreate is tearing down and restarting
    /// (they are in no other map meanwhile). A `session/close` of such an id
    /// cancels the recreate; a second `load`/`resume` of it is refused.
    recreating: std::collections::HashMap<String, Recreating>,
    /// Hands each spawned recreate to [`serve_until`], which stops them before
    /// it disposes the connection's sessions.
    recreate_tx: mpsc::UnboundedSender<RecreateTask>,
    /// The session index of this connection (`SessionIndexConnection`).
    index: SessionIndexConnection,
    /// The `providers/set` routing of the `main` provider, if any
    /// (`providerConfig`).
    provider: Option<providers::ProviderConfig>,
    /// The legacy gateway `authenticate` request (`gatewayAuthRequest`).
    gateway_auth: Option<Value>,
    /// The request each live session was created with (`creationParams`), so a
    /// provider switch or a sign-out can recreate it.
    creation_params: std::collections::HashMap<String, Value>,
    /// The probe context of each live session (`authStatusSettings`).
    auth_contexts: std::collections::HashMap<String, crate::auth_status::SessionAuthContext>,
    /// The `--hide-claude-auth` guard state of each live session.
    guard_states: std::collections::HashMap<String, crate::hide_claude_auth::GuardState>,
    /// `--hide-claude-auth`.
    hide_claude_auth: bool,
    /// The protocol version this connection negotiated in `initialize`
    /// (`v2::ProtocolMode`): v1 unless experimental v2 was opted into and the
    /// client asked for version 2 or later (B1/B2).
    protocol: crate::v2::ProtocolMode,
    /// A v2 setup response carries the session's commands, so the v1
    /// `available_commands_update` that follows it is withheld while the v1
    /// handler serves it (`v2/agent.ts:150-162`).
    suppress_commands_update: bool,
    /// The gate of the provider update in flight (`providerUpdate`), a
    /// reader/writer lock over [`GATE_PERMITS`] permits. A provider update runs
    /// off this handler — it waits for the submitted turns and starts every
    /// session anew — and holds every permit meanwhile; a session request that
    /// follows is parked until it ends, as upstream's `while (this.providerUpdate)
    /// await this.providerUpdate` does. A session request, and the recreate a
    /// changed-params `load` runs, hold one permit, so they never exclude each
    /// other, and the FIFO queue keeps a waiting update ahead of later requests.
    provider_gate: Arc<tokio::sync::Semaphore>,
    /// How many provider updates are queued or running (`providerUpdate !== null`
    /// for `logout`'s `hadRoutingOverride`).
    provider_updates: Arc<std::sync::atomic::AtomicUsize>,
    /// The live sessions and their creation params, published for tasks that run
    /// off this handler (a provider update recreates every session and cannot
    /// read the registry).
    sessions_view: Arc<watch::Sender<std::collections::HashMap<String, (Session, Value)>>>,
    /// The outbound translation task of a v2 connection (`V2ClientConnection`):
    /// it owns the display terminals, the tool-call `_meta` and the replay
    /// state of the connection. `None` on v1, which sends v1 updates as they
    /// are.
    v2_outbound: Option<crate::v2::agent::V2OutboundHandle>,
    /// The connection's logger, for the v2 outbound task's own diagnostics
    /// (`Failed to send a … update`, `v2/agent.ts:243-249`; NEW-9).
    logger: Option<std::sync::Arc<crate::logger::Logger>>,
}

impl Registry {
    /// `resolveProviderConfig`: `providers/set` first, else the legacy gateway.
    fn resolve_provider(&self) -> Option<providers::ProviderConfig> {
        self.provider
            .clone()
            .or_else(|| providers::gateway_request_to_provider_config(self.gateway_auth.as_ref()))
    }

    /// Tells the auth actor which routing is in force.
    fn sync_routing(&self) {
        if let Some(auth) = &self.auth {
            auth.send(AuthCmd::SetRouting {
                provider_override: self.provider.is_some(),
                routing_active: self.resolve_provider().is_some(),
            });
        }
    }

    /// Withhold the v1 `available_commands_update` of a setup response: a v2
    /// client gets the commands in the response itself (`v2/agent.ts:150-162`).
    pub(crate) fn set_suppress_commands_update(&mut self, suppress: bool) {
        self.suppress_commands_update = suppress;
    }

    /// The connection's v2 outbound task, started on the first v2 request
    /// (`V2ClientConnection`, `v2/agent.ts:174-402`).
    pub(crate) fn v2_outbound(
        &mut self,
        connection: &ConnectionTo<agent_client_protocol::Client>,
    ) -> crate::v2::agent::V2OutboundHandle {
        let logger = self.logger.clone();
        self.v2_outbound
            .get_or_insert_with(|| crate::v2::agent::spawn_v2_outbound(connection.clone(), logger))
            .clone()
    }

    /// The live sessions and their creation params, as published for the tasks
    /// that run off this handler.
    pub(crate) fn sessions_view(
        &self,
    ) -> watch::Receiver<std::collections::HashMap<String, (Session, Value)>> {
        self.sessions_view.subscribe()
    }

    /// Forgets what the registry keeps of a session that is gone.
    fn forget_session_state(&mut self, session_id: &str) {
        self.creation_params.remove(session_id);
        self.auth_contexts.remove(session_id);
        self.guard_states.remove(session_id);
        if let Some(auth) = &self.auth {
            auth.send(AuthCmd::ForgetSession {
                session_id: session_id.to_string(),
            });
        }
    }

    /// Publishes the live sessions and their creation params for the tasks that
    /// run off this handler (a provider update recreates every session).
    fn publish_sessions(&self) {
        self.sessions_view.send_modify(|view| {
            // A provider update inserts the session it started straight into the
            // view, before its `Register` reaches the registry: the registry still
            // holds the closed session it replaced, which must not overwrite it.
            let started_meanwhile: Vec<_> = view
                .iter()
                .filter(|(id, (live, _))| {
                    !live.command_sender().is_closed()
                        && self
                            .sessions
                            .get(*id)
                            .is_some_and(|held| held.command_sender().is_closed())
                })
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect();
            view.clear();
            for (id, session) in &self.sessions {
                if let Some(params) = self.creation_params.get(id) {
                    view.insert(id.clone(), (session.clone(), params.clone()));
                }
            }
            view.extend(started_meanwhile);
        });
    }

    /// What a provider update needs of the registry, as cheap clones: it runs off
    /// this handler, so it cannot borrow the registry. `routing_active` is the
    /// routing the update installs, which the account guard of the sessions it
    /// recreates reads (`resolveProviderConfig() !== null`).
    fn provider_env(
        &self,
        opts: &ServeOptions,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        routing_active: bool,
    ) -> recreate::ProviderUpdateEnv {
        let mut auth_env = AuthEnv::of(self, opts);
        auth_env.routing_active = routing_active;
        recreate::ProviderUpdateEnv {
            gate: self.provider_gate.clone(),
            sessions: self.sessions_view.subscribe(),
            board: self.index.board().clone(),
            deferred_tx: self.deferred_tx.clone(),
            session_tx: self.session_tx.clone(),
            dispose_tx: self.dispose_tx.clone(),
            auth: self.auth.clone(),
            auth_env,
            air: self.air.clone(),
            caps: self.caps.clone(),
            sessions_view: self.sessions_view.clone(),
            cancel,
            is_v2: self.protocol == crate::v2::ProtocolMode::V2,
            v2_outbound: self.v2_outbound.clone(),
        }
    }
}

/// The session index of a connection over `opts`' environment: the projects root
/// and the CLI's live registry under the configuration directory.
fn session_index_connection(opts: &ServeOptions) -> SessionIndexConnection {
    let config = crate::session_store::config_dir_with_env(&opts.env);
    let logger = opts.logger.clone();
    SessionIndexConnection::new(
        IndexPaths {
            projects_root: config.join("projects"),
            registry_dir: config.join("sessions"),
        },
        Arc::new(move |message, error| {
            if let Some(logger) = &logger {
                logger.error(&format!("[session-index] {message}: {error}"));
            }
        }),
    )
}

/// A recreate in flight: its cancel flag (checked between its steps) and a
/// signal that flips once the task has ended.
struct Recreating {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    ended: watch::Receiver<bool>,
}

/// What a recreate task reports when it ends, however it ends: its id is no
/// longer being recreated and a waiting `session/close` may answer.
struct RecreateEnding {
    deferred_tx: mpsc::UnboundedSender<RegistryMsg>,
    session_id: String,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    ended_tx: watch::Sender<bool>,
}

impl Drop for RecreateEnding {
    fn drop(&mut self) {
        let _ = self.deferred_tx.send(RegistryMsg::RecreateEnded {
            session_id: std::mem::take(&mut self.session_id),
            cancel: self.cancel.clone(),
        });
        let _ = self.ended_tx.send(true);
    }
}

/// Closes a session that [`Session::start`] returned but that never reached the
/// dispose list ([`wire_session`]): the replay and `seed_task_state` that follow
/// the start can still be running when the connection ends and the recreate task
/// is aborted, and the `claude` child must not outlive that. Disarmed by
/// [`wire_session`]'s caller right before the session is published.
struct UnwiredSessionGuard(Option<mpsc::UnboundedSender<crate::session::Command>>);

impl UnwiredSessionGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for UnwiredSessionGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let (reply, _closed) = oneshot::channel();
            let _ = tx.send(crate::session::Command::Close { reply });
        }
    }
}

/// A spawned recreate, as [`serve_until`] tracks it.
struct RecreateTask {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

/// A change to the registry that a task running off the request handler leaves
/// for the handler to apply before it serves the next request.
enum RegistryMsg {
    /// A started session's entries.
    Register(Box<RegistryUpdate>),
    /// The process behind the session died: forget it (`delete this.sessions`).
    Evict {
        session_id: String,
        /// The dead session's command channel, held weakly.
        husk: mpsc::WeakUnboundedSender<crate::session::Command>,
    },
    /// The recreate of `session_id` that owns `cancel` has ended.
    RecreateEnded {
        session_id: String,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    },
    /// The session was torn down after a mutation that succeeded (`archive`
    /// closes it only once its retitle did): forget its registry entries, as
    /// upstream's `teardownSession` deletes it before answering.
    Forget {
        session_id: String,
        /// The torn-down session's command channel, held weakly: a session
        /// loaded under the id since stays.
        husk: mpsc::WeakUnboundedSender<crate::session::Command>,
    },
    /// The provider update finished: adopt its routing (`providerConfig`).
    SetProvider {
        config: Option<providers::ProviderConfig>,
    },
}

/// Whether the registered session is the one whose command channel `husk` holds.
fn is_registered_instance(
    live: &mpsc::UnboundedSender<crate::session::Command>,
    husk: &mpsc::WeakUnboundedSender<crate::session::Command>,
) -> bool {
    husk.upgrade().is_some_and(|tx| tx.same_channel(live))
}

/// A registry entry a session start leaves for the request handler to apply.
struct RegistryUpdate {
    session_id: String,
    session: Session,
    settings_manager: Arc<crate::settings_manager::SettingsManager>,
    fingerprint: String,
    /// The cancel flag of the recreate this session belongs to. Checked when the
    /// handler applies the update, not only before it was sent: a `session/close`
    /// that arrives while the start is still finishing leaves it set, and the
    /// session is then closed instead of registered under an id the client
    /// already closed.
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The request the session was created with (`creationParams`).
    creation_params: Value,
    /// The probe context the session started in.
    auth_context: SessionAuthContext,
    guard: crate::hide_claude_auth::GuardState,
}

/// What a `load`/`resume` start needs of the registry, as cheap clones, so the
/// start can run off the request handler.
struct ResumeEnv {
    air: Option<AirClient>,
    caps: ClientCapabilities,
    auth: Option<AuthHandle>,
    session_tx: mpsc::UnboundedSender<(String, Session)>,
    dispose_tx: mpsc::UnboundedSender<Session>,
    deferred_tx: mpsc::UnboundedSender<RegistryMsg>,
    /// The permission mode of the live session the start recreates, if any.
    live_mode: Option<String>,
    /// Set when the recreate this start belongs to is cancelled (a
    /// `session/close` of the id, or the connection ending).
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// What the session index knows of the sessions this connection runs.
    board: crate::session_index::connection::OwnSessionBoard,
    /// The routing in force when the start was requested (`resolveProviderConfig`).
    provider: Option<providers::ProviderConfig>,
    /// The account side of the start: auth actor, guard flag, routing.
    auth_env: AuthEnv,
    /// Publishes the started session for a provider update running off this
    /// handler (`sessions_view`).
    sessions_view: Arc<watch::Sender<std::collections::HashMap<String, (Session, Value)>>>,
    /// Withhold the v1 `available_commands_update` of the setup response: a v2
    /// client gets the commands in the response itself (`v2/agent.ts:150-162`).
    suppress_commands_update: bool,
    /// The connection serves ACP v2 (M-118).
    is_v2: bool,
    /// The connection's v2 outbound task, if the connection negotiated v2.
    v2_outbound: Option<crate::v2::agent::V2OutboundHandle>,
}

impl Registry {
    /// The clones a `load`/`resume` start needs.
    fn resume_env(&self, live_mode: Option<String>, opts: &ServeOptions) -> ResumeEnv {
        ResumeEnv {
            air: self.air.clone(),
            caps: self.caps.clone(),
            auth: self.auth.clone(),
            session_tx: self.session_tx.clone(),
            dispose_tx: self.dispose_tx.clone(),
            deferred_tx: self.deferred_tx.clone(),
            live_mode,
            cancel: None,
            board: self.index.board().clone(),
            provider: self.resolve_provider(),
            auth_env: AuthEnv::of(self, opts),
            sessions_view: self.sessions_view.clone(),
            suppress_commands_update: self.suppress_commands_update,
            is_v2: self.protocol == crate::v2::ProtocolMode::V2,
            v2_outbound: self.v2_outbound.clone(),
        }
    }

    /// Apply what tasks running off the request handler left behind.
    pub(crate) fn apply_deferred(&mut self) {
        while let Ok(msg) = self.deferred_rx.try_recv() {
            match msg {
                RegistryMsg::Register(update) => {
                    if update
                        .cancel
                        .as_ref()
                        .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::SeqCst))
                    {
                        // Cancelled after its last check: nobody can use the id.
                        let session = update.session;
                        self.forget_session_state(&update.session_id);
                        tokio::spawn(async move { session.close().await });
                        continue;
                    }
                    self.settings_managers
                        .insert(update.session_id.clone(), update.settings_manager);
                    self.fingerprints
                        .insert(update.session_id.clone(), update.fingerprint);
                    self.creation_params
                        .insert(update.session_id.clone(), update.creation_params);
                    self.auth_contexts
                        .insert(update.session_id.clone(), update.auth_context);
                    self.guard_states
                        .insert(update.session_id.clone(), update.guard);
                    self.sessions.insert(update.session_id, update.session);
                }
                RegistryMsg::Evict { session_id, husk } => {
                    // Only the husk itself: a newer session under the same id
                    // (a load after the death) stays.
                    let is_husk = self
                        .sessions
                        .get(&session_id)
                        .is_some_and(|live| is_registered_instance(live.command_sender(), &husk));
                    if is_husk {
                        self.sessions.remove(&session_id);
                        self.settings_managers.remove(&session_id);
                        self.fingerprints.remove(&session_id);
                        self.forget_session_state(&session_id);
                        // The dead session's row shows what the registry and the
                        // transcript tell (`closeQueryStream` → `onOwnSessionChanged`).
                        self.index.on_teardown(&session_id);
                    }
                }
                RegistryMsg::Forget { session_id, husk } => {
                    // The archive closed it already; only the maps remain. Only
                    // the session it closed: one loaded under the id since stays.
                    let is_husk = self
                        .sessions
                        .get(&session_id)
                        .is_some_and(|live| is_registered_instance(live.command_sender(), &husk));
                    if is_husk {
                        self.sessions.remove(&session_id);
                        self.settings_managers.remove(&session_id);
                        self.fingerprints.remove(&session_id);
                        self.forget_session_state(&session_id);
                        self.index.on_teardown(&session_id);
                    }
                }
                RegistryMsg::RecreateEnded { session_id, cancel } => {
                    if self
                        .recreating
                        .get(&session_id)
                        .is_some_and(|r| Arc::ptr_eq(&r.cancel, &cancel))
                    {
                        self.recreating.remove(&session_id);
                    }
                }
                RegistryMsg::SetProvider { config } => {
                    self.provider = config;
                    self.sync_routing();
                }
            }
        }
        self.publish_sessions();
    }

    /// Effective apiKeyHelper for a session, with precedence (O-61, O-67).
    pub fn api_key_helper(
        &self,
        session_id: &str,
        settings_override: Option<&Value>,
    ) -> Option<String> {
        self.settings_managers
            .get(session_id)
            .and_then(|m| m.get_api_key_helper(settings_override))
    }
}

/// Spawn the [`crate::auth_status`] actor: it probes `claude auth status` and
/// forwards each changed status as an `_auth/status_update` ext notification.
fn spawn_auth(
    opts: &ServeOptions,
    hide_claude_auth: bool,
    connection: &ConnectionTo<agent_client_protocol::Client>,
) -> AuthHandle {
    let (push_tx, mut push_rx) = mpsc::unbounded_channel::<AuthPush>();
    let connection = connection.clone();
    tokio::spawn(async move {
        while let Some(item) = push_rx.recv().await {
            match item {
                AuthPush::Status(status) => {
                    if let Ok(msg) = UntypedMessage::new(
                        crate::auth_status::AUTH_STATUS_UPDATE_METHOD,
                        json!({ "authStatus": status }),
                    ) {
                        let _ = connection.send_notification(msg);
                    }
                }
                // Every earlier status is already queued on the connection.
                AuthPush::Barrier(done) => {
                    let _ = done.send(());
                }
            }
        }
    });
    let claude_path = opts.claude_path.clone();
    let extra_env = opts.extra_env.clone();
    // The actor must resolve settings through the SAME injected managed source
    // as the sessions (`EnvSnapshot::managed_docs`); using the host process
    // environment here would leak a managed `apiKeyHelper` into auth status.
    let env = opts.env.clone();
    let probe_env = env.clone();
    let probe: crate::auth_status::ProbeFn = Box::new(move |request| {
        let claude_path = claude_path.clone();
        let extra_env = extra_env.clone();
        let env = probe_env.clone();
        Box::pin(async move {
            crate::auth_status::probe_cli_in(claude_path.as_deref(), &request, &extra_env, &env)
                .await
        })
    });
    spawn_auth_actor_with(
        push_tx,
        probe,
        std::env::current_dir().unwrap_or_default(),
        env,
        AuthActorConfig {
            hide_claude_auth,
            logger: opts.logger.clone(),
        },
    )
}

/// What a session start hands the account side: the connection's auth actor, the
/// routing in force and the `--hide-claude-auth` flag.
#[derive(Clone)]
struct AuthEnv {
    auth: Option<AuthHandle>,
    /// `resolveProviderConfig() !== null`: the subscription guard is off.
    routing_active: bool,
    hide_claude_auth: bool,
    logger: Option<Arc<crate::logger::Logger>>,
}

impl AuthEnv {
    fn of(registry: &Registry, opts: &ServeOptions) -> Self {
        Self {
            auth: registry.auth.clone(),
            routing_active: registry.resolve_provider().is_some(),
            hide_claude_auth: registry.hide_claude_auth,
            logger: opts.logger.clone(),
        }
    }

    /// `claudeSubscriptionGuardActive`.
    fn guard_active(&self) -> bool {
        self.hide_claude_auth && !self.routing_active
    }
}

/// The probe context of a session about to start (`useAuthStatusSettings`'s
/// arguments): its settings manager's cwd, its apiKeyHelper, the programmatic
/// settings and the launch env.
fn session_auth_context(
    cwd: PathBuf,
    settings_arg: Option<&str>,
    launch_env: &[(String, String)],
    opts: &ServeOptions,
) -> SessionAuthContext {
    let settings_override = settings_arg.and_then(|text| serde_json::from_str::<Value>(text).ok());
    let api_key_helper =
        crate::settings::load(&cwd, &opts.env).get_api_key_helper(settings_override.as_ref());
    SessionAuthContext {
        cwd,
        api_key_helper,
        settings_override,
        launch_env: (!launch_env.is_empty()).then(|| launch_env.to_vec()),
    }
}

/// B-91: the per-turn `--hide-claude-auth` guard on the account the CLI cached at
/// `initialize`. The account is reported before the decision, so a refused turn
/// still tells the client which account it was for. `Some` is the refusal.
fn check_turn_guard(
    registry: &mut Registry,
    opts: &ServeOptions,
    session_id: &str,
    session: &Session,
) -> Option<Error> {
    if !AuthEnv::of(registry, opts).guard_active() {
        return None;
    }
    let logger = opts.logger.clone();
    let warn = move |message: &str| {
        if let Some(logger) = &logger {
            logger.error(message);
        }
    };
    let auth = registry.auth.clone();
    let helper = registry
        .auth_contexts
        .get(session_id)
        .and_then(|context| context.api_key_helper.clone());
    let cwd = registry
        .auth_contexts
        .get(session_id)
        .map(|context| context.cwd.clone())
        .unwrap_or_default();
    let state = registry
        .guard_states
        .entry(session_id.to_string())
        .or_default();
    crate::hide_claude_auth::refuse_claude_subscription_turn(
        session_id,
        session.account(),
        state,
        &warn,
        |account| {
            if let Some(auth) = &auth {
                auth.send(AuthCmd::SessionCreated {
                    session_id: None,
                    cwd,
                    account: Some(account.clone()),
                    api_key_helper: helper,
                    epoch: None,
                });
            }
        },
    )
}

/// V-16: a session flagged for a recreate (a sign-out failed its last turn, or a
/// probe found another kind of identity) is recreated now, keeping the husk
/// addressable when creation refuses (`respawnSignedOutSession`). A live turn
/// postpones it; a sign-out closed the query, so it recreates at once.
///
/// `Ok(None)`: no recreate was due. `Ok(Some(session))`: the session the caller
/// must go on with. `Err`: the recreation's refusal.
async fn respawn_signed_out_session(
    registry: &mut Registry,
    opts: &ServeOptions,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: &str,
) -> std::result::Result<Option<Session>, Error> {
    if !registry.hide_claude_auth {
        return Ok(None);
    }
    let flagged = match &registry.auth {
        Some(auth) => auth.needs_respawn(session_id).await,
        None => false,
    };
    if !flagged {
        return Ok(None);
    }
    // `!session.queryClosed && turnQueue.some(!settled)`: a query the sign-out
    // closed has no turn left to wait for, whatever the board still counts.
    let running = match registry.sessions.get(session_id) {
        Some(session) => {
            !session.command_sender().is_closed()
                && recreate::session_has_unsettled_turn(session, registry.index.board(), session_id)
                    .await
        }
        None => false,
    };
    if running {
        return Ok(None);
    }
    if let Some(logger) = &opts.logger {
        logger.log(&format!(
            "Recreating Claude session {session_id} after a sign-out"
        ));
    }
    recreate::recreate_live_session(registry, opts, connection, session_id, true).await?;
    match registry.sessions.get(session_id) {
        Some(session) => Ok(Some(session.clone())),
        None => Err(Error::internal_error().data(json!({ "details": "Session not found" }))),
    }
}

/// The creation-time account step of a started session: publish the account the
/// CLI reported (before the guard may refuse the session, so a refusal still tells
/// the client which account it was for), then apply the `--hide-claude-auth` guard.
/// `Err` is the refusal; the caller closes the session.
fn account_at_creation(
    env: &AuthEnv,
    session_id: &str,
    context: &SessionAuthContext,
    epoch: u64,
    session: &Session,
    guard: &mut crate::hide_claude_auth::GuardState,
) -> std::result::Result<(), Error> {
    let session_message = |registered: bool| AuthCmd::SessionCreated {
        session_id: registered.then(|| session_id.to_string()),
        cwd: context.cwd.clone(),
        account: session.account().cloned(),
        api_key_helper: context.api_key_helper.clone(),
        epoch: Some(epoch),
    };
    // The identity goes out first, whatever the guard decides.
    if let Some(auth) = &env.auth {
        auth.send(session_message(false));
    }
    if env.guard_active() {
        let logger = env.logger.clone();
        let warn = move |message: &str| {
            if let Some(logger) = &logger {
                logger.error(message);
            }
        };
        match session.account() {
            None => crate::hide_claude_auth::warn_guard_degraded(
                session_id,
                guard,
                "the CLI reported no account at initialize",
                &warn,
            ),
            // A refused session is never registered: a husk that is refused again
            // keeps the flag that asks for its recreate.
            Some(account) => {
                if let Some(refusal) = crate::hide_claude_auth::decide(account) {
                    return Err(refusal);
                }
            }
        }
    }
    if let Some(auth) = &env.auth {
        auth.send(session_message(true));
    }
    Ok(())
}

/// Validate a `session/new` / `session/load` `cwd` exactly as the adapter's
/// `validateCwd` (`acp-agent.js:4681-4695`, issue #749): it must be an absolute
/// path to an existing directory, else `invalidParams` (-32602) with the
/// adapter's message and `data:{cwd}` so a client can surface it instead of
/// failing later with an opaque SDK launch error. A `cwd` absent from the
/// request is accepted — the crate falls back to `default_cwd`/process cwd, as
/// the adapter's `createSession` does with `serve.options.default_cwd`.
fn validate_cwd(cwd: Option<&str>) -> Result<(), Error> {
    let Some(cwd) = cwd else { return Ok(()) };
    let path = std::path::Path::new(cwd);
    // `RequestError.invalidParams` renders the message as
    // `Invalid params: <text>` (`acp-agent.ts:10925-10942`, jsonrpc.js:1032).
    let invalid = |text: String| {
        Error::new(-32602, format!("Invalid params: {text}")).data(json!({ "cwd": cwd }))
    };
    if !path.is_absolute() {
        return Err(invalid(format!(
            "`cwd` must be an absolute path, but received: {cwd}"
        )));
    }
    // A single `stat` is a fast syscall, not "large-file I/O"; inline is fine
    // (rust-coding §4 concerns whole-file blocking reads).
    let Ok(meta) = std::fs::metadata(path) else {
        return Err(invalid(format!(
            "`cwd` does not exist on the machine running the agent: {cwd}"
        )));
    };
    if !meta.is_dir() {
        return Err(invalid(format!("`cwd` is not a directory: {cwd}")));
    }
    Ok(())
}

/// Validate the optional `systemPrompt` (after `validate_cwd`, before any spawn)
/// and fold it, the legacy `_meta.systemPrompt` and AIR `customInstructions`
/// into the `initialize` options (rust-air-gaps G3/G5, Decision 10; F4a
/// O-133).
fn initialize_options_for(
    params: &Value,
    caps: &ClientCapabilities,
    air: Option<&AirClient>,
    v2: bool,
) -> Result<InitializeOptions, Error> {
    let (system_prompt, append_system_prompt) = system_prompt::initialize_fields(params)?;
    let opts = crate::sdk_options::claude_options(params);
    // Node defaults `promptSuggestions` on unless the client passes a value
    // (`acp-agent.ts:11235`).
    let prompt_suggestions = opts.prompt_suggestions.unwrap_or(true);
    // `forwardsSubagentText` (`acp-agent.ts:10947-10953`): native subagent
    // sessions, the historical `_meta["subagent-transcript"]` capability, or
    // the client's `forwardSubagentText` option (O-85).
    let forward_subagent_text = caps.subagent_form.is_some()
        || caps.subagent_transcript
        || opts.forward_subagent_text == Some(true);
    // `perTaskStopAffordance` defaults on for the subagents RFD form; the
    // client's own option wins because it is spread after it (O-84). Undefined
    // for every other client, so the field is omitted from the request.
    let per_task_stop_affordance = opts
        .per_task_stop_affordance
        .or((caps.subagent_form == Some(crate::client_caps::SubagentForm::Rfd)).then_some(true));
    // The SDK enables checkpointing for a client that negotiated per-turn
    // file-change reports; otherwise the client's option decides
    // (`acp-agent.ts:11240`, O-136).
    let file_change_reporter = air.is_some_and(|client| client.supports("agentFileChangeReport"));
    let enable_file_checkpointing =
        file_change_reporter || opts.enable_file_checkpointing == Some(true);
    let cc_opts = params
        .get("_meta")
        .and_then(|m| m.get("claudeCode"))
        .and_then(|c| c.get("options"));
    let skills = cc_opts
        .and_then(|o| o.get("skills"))
        .filter(|v| !v.is_null())
        .cloned();
    let verbatim_prompts = cc_opts
        .and_then(|o| o.get("verbatimPrompts"))
        .and_then(Value::as_bool);
    Ok(InitializeOptions {
        system_prompt,
        append_system_prompt,
        // T-105: a v2 connection also registers the PreToolUse hook of the
        // change previews, so an auto-approved Edit or Write shows its exact
        // patch before it runs (`acp-agent.ts:11284-11291`); a v1 client gets
        // the input's diff and waits on no hook.
        hooks: Some(crate::hooks::init_hooks_payload_with_v2(v2)),
        // O-144: `_meta.claudeCode.options.agents` goes to `initialize` verbatim.
        agents: opts.agents,
        prompt_suggestions: Some(prompt_suggestions),
        tool_aliases: opts.tool_aliases,
        tool_config: opts.tool_config,
        skills,
        verbatim_prompts,
        supported_dialog_kinds: caps
            .elicitation_form
            .then(|| vec![crate::elicitation::REFUSAL_FALLBACK_DIALOG_KIND.to_string()]),
        forward_subagent_text,
        per_task_stop_affordance,
        agent_progress_summaries: opts.agent_progress_summaries,
        plan_mode_instructions: opts.plan_mode_instructions,
        enable_file_checkpointing,
    })
}

/// Register a new session: keep its handle in the registry, publish it to the
/// notification handler (lock-free), and spawn a task that forwards its updates
/// to the client as they arrive. The actor emits a `FinalText` chunk before
/// resolving the prompt's oneshot, so the chunk lands ahead of the response.
/// The names of the ACP-provided MCP servers of a setup request
/// (`params.mcpServers`, F23c/A-01).
fn mcp_server_names(params: &Value) -> Vec<String> {
    params
        .get("mcpServers")
        .and_then(Value::as_array)
        .map(|servers| {
            servers
                .iter()
                .filter_map(|server| server.get("name").and_then(Value::as_str))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// `afterSetupResponse`'s MCP half (`acp-agent.ts:10434-10440`): start the
/// background OAuth run for the session's ACP servers.
fn start_setup_mcp_auth(session: &crate::session::Session, params: &Value) {
    session.start_mcp_auth(mcp_server_names(params), true);
}

fn register_session(
    registry: &mut Registry,
    session_id: &str,
    session: Session,
    update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
    connection: &ConnectionTo<agent_client_protocol::Client>,
) {
    registry
        .sessions
        .insert(session_id.to_string(), session.clone());
    wire_session(
        WireTargets {
            session_tx: &registry.session_tx,
            dispose_tx: &registry.dispose_tx,
            deferred_tx: &registry.deferred_tx,
            board: registry.index.board(),
            is_air_client: registry.air.is_some(),
            is_v2: registry.protocol == crate::v2::ProtocolMode::V2,
            v2: registry.v2_outbound.as_ref(),
        },
        session_id,
        session,
        update_rx,
        connection,
    );
    registry.publish_sessions();
}

/// The connection's channels a session is wired to ([`wire_session`]).
struct WireTargets<'a> {
    session_tx: &'a mpsc::UnboundedSender<(String, Session)>,
    dispose_tx: &'a mpsc::UnboundedSender<Session>,
    deferred_tx: &'a mpsc::UnboundedSender<RegistryMsg>,
    /// What the session index shows of the session: its cost comes from the usage
    /// updates.
    board: &'a crate::session_index::connection::OwnSessionBoard,
    /// An AIR client shows an archived title without its archive prefix.
    is_air_client: bool,
    /// The connection negotiated ACP v2 (M-118): an extension notification the
    /// agent sends must start with "_".
    is_v2: bool,
    /// The connection's v2 outbound task: every v1 update it forwards is
    /// translated before it reaches a v2 client.
    v2: Option<&'a crate::v2::agent::V2OutboundHandle>,
}

/// Writes a raw `session/update` through `send`; `false` when the client never got
/// it (the message could not be built, or the write failed).
fn write_raw_update<E>(value: Value, send: impl FnOnce(UntypedMessage) -> Result<(), E>) -> bool {
    UntypedMessage::new("session/update", value)
        .ok()
        .is_some_and(|msg| send(msg).is_ok())
}

/// The `cost.amount` of a `usage_update` (`total_cost_usd` of the result).
fn usage_update_cost(update: &Value) -> Option<f64> {
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("usage_update") {
        return None;
    }
    update.pointer("/cost/amount").and_then(Value::as_f64)
}

/// [`register_session`] without the registry entry: publish the session to the
/// notification handler and the dispose list, and start its update forwarder.
fn wire_session(
    targets: WireTargets<'_>,
    session_id: &str,
    session: Session,
    update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
    connection: &ConnectionTo<agent_client_protocol::Client>,
) {
    let WireTargets {
        session_tx,
        dispose_tx,
        deferred_tx,
        board,
        is_air_client,
        is_v2,
        v2,
    } = targets;
    let v2 = v2.cloned();
    // A title a stored record publishes reads as the client shows it: an AIR
    // client gets an archived title without its archive prefix
    // (`ClaudeAcpAgent.clientTitle`, `acp-agent.ts:3205`).
    session.set_client_title(crate::session_titles::ClientTitle(Arc::new(move |raw| {
        crate::session_index::connection::SessionIndexConnection::client_title(is_air_client, raw)
    })));
    let board = board.clone();
    let board_session_id = session_id.to_string();
    // This run of the session under its id: what it reports later is ignored once
    // the id was closed or loaded again.
    let incarnation = board.begin(session_id);
    let _ = session_tx.send((session_id.to_string(), session.clone()));
    // Weak, so this task (which lives as long as the actor's outbound channel)
    // does not keep the actor's command channel open.
    let evict = (
        deferred_tx.clone(),
        session_id.to_string(),
        session.command_sender().downgrade(),
    );
    let _ = dispose_tx.send(session.clone());

    // Decision 9 / 2.1 + F5b/A-10: background get_context_usage {detail:"summary"}
    // spawned after register. The request is tagged with the model it was issued
    // for, so an answer arriving after a model switch is dropped (`stillCurrent`).
    let control = session.control().clone();
    let cmd_tx = session.command_sender().clone();
    // Weak: the forwarder reports a failed write of a raw update back to the actor
    // (U-75), and must not keep the actor's command channel open.
    let write_failed_tx = session.command_sender().downgrade();
    let model_id = session.current_model_id().unwrap_or_default();
    tokio::spawn(async move {
        if let Ok(res) = control
            .send_request(json!({"subtype": "get_context_usage", "detail": "summary"}))
            .await
        {
            let raw_max = crate::usage::raw_max_tokens(&res);
            if let Some(tokens) = raw_max {
                let _ = cmd_tx.send(crate::session::Command::UpdateContextWindow {
                    size: tokens,
                    model_id,
                });
            }
        }
    });
    let connection = connection.clone();
    let forward_session_id = session_id.to_string();
    tokio::spawn(async move {
        let mut update_rx = update_rx;
        while let Some(item) = update_rx.recv().await {
            match item {
                SessionOutbound::Update(notif) => {
                    let mut value = serde_json::to_value(&notif).unwrap_or(serde_json::Value::Null);
                    if let Some(update) = value.get_mut("update") {
                        crate::map::materialize_tool_call_defaults(update);
                        crate::map::materialize_diff_old_text(update);
                    }
                    // C-24: a v2 client gets the update's v2 form, with the
                    // connection's terminal and `_meta` stores applied.
                    if let Some(v2) = &v2 {
                        if let Some(update) = value.get("update") {
                            v2.translate(&forward_session_id, update.clone());
                        }
                        continue;
                    }
                    if let Ok(msg) = UntypedMessage::new("session/update", value) {
                        let _ = connection.send_notification(msg);
                    }
                }
                // Item 3.3: an untyped AIR `session/update` (async-task
                // spawns/progress/state-updates), value = `{"sessionId",⋯,"update",{⋯}}`.
                SessionOutbound::RawUpdate(mut value) => {
                    // Only the `content[*]` diff blocks of a raw `tool_call` /
                    // `tool_call_update` are materialised; the raw update's
                    // tool data is forwarded verbatim.
                    if let Some(update) = value.get_mut("update") {
                        crate::map::materialize_diff_old_text(update);
                        // The cost of the result, for the session index.
                        if let Some(cost) = usage_update_cost(update) {
                            board.note_cost_of(&board_session_id, incarnation, cost);
                        }
                    }
                    if let Some(v2) = &v2 {
                        let session_id = value
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or(forward_session_id.as_str())
                            .to_string();
                        if let Some(update) = value.get("update") {
                            v2.translate(&session_id, update.clone());
                        }
                        continue;
                    }
                    // A raw update the client never got (the write failed or the
                    // message could not be built) is reported to the actor, which
                    // rolls back the async-task fields it recorded as published.
                    if !write_raw_update(value, |msg| connection.send_notification(msg)) {
                        if let Some(tx) = write_failed_tx.upgrade() {
                            let _ = tx.send(crate::session::Command::UpdateSendFailed);
                        }
                    }
                }
                // F39: the intake answer of a v2 prompt, after every update
                // ahead of it reached the wire (the forwarding task is FIFO).
                SessionOutbound::V2Intake {
                    prompt_uuid,
                    reply,
                    result,
                } => {
                    let _ = reply.send(result.clone());
                    if let Some(v2) = &v2 {
                        v2.answer(&prompt_uuid, result);
                    }
                }
                // M-65: what the session index shows of this session (the CLI's
                // state, a failed turn, a failed query). Ignored once the id was
                // closed or loaded again.
                SessionOutbound::Board(event) => {
                    if board.current(&board_session_id) == Some(incarnation) {
                        event.apply(&board, &board_session_id);
                    }
                }
                // U-10: the raw SDK message ext notification
                // (`client.extNotification("_claude/sdkMessage", {...})`).
                SessionOutbound::ExtNotification { method, params } => {
                    // M-118: an extension method on v2 must start with "_"
                    // (`v2/agent.ts:245-250`); an untagged one is a bug and is
                    // not sent, as the client connection rejects it upstream.
                    if is_v2 && crate::v2::validate_extension_method(&method).is_err() {
                        continue;
                    }
                    if let Ok(msg) = UntypedMessage::new(&method, params) {
                        let _ = connection.send_notification(msg);
                    }
                }
                // Phase 9 flush barrier: every update sent before this marker
                // has been forwarded; resolve the oneshot so the prompt task
                // may write its response (keeps a `tool_call` ahead of the
                // `result` on the wire).
                SessionOutbound::Flush(done) => {
                    let _ = done.send(());
                }
                // The process died: the registry forgets the session before any
                // reply queued behind this marker reaches the client, so the
                // client's next `session/load` starts a new child.
                SessionOutbound::ProcessDied => {
                    let (deferred_tx, session_id, husk) = evict.clone();
                    let _ = deferred_tx.send(RegistryMsg::Evict { session_id, husk });
                }
                // A prompt response barrier that keeps FIFO response order within a
                // settle batch: wait (bounded) until the prompt task queued its
                // response before the next batch member's barrier resolves.
                SessionOutbound::FlushAcked { done, ack } => {
                    let _ = done.send(());
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), ack).await;
                }
                // Phase 10: a permission request from the session actor. Spawn a
                // task that sends `session/request_permission` to the client and
                // resolves the oneshot with the raw `result` (or an error), so the
                // forwarding task is never blocked on the client round-trip (D14).
                SessionOutbound::RequestPermission { params, reply } => {
                    // The actor's permission path already shaped and
                    // materialised the `toolCall` (`permission::build_tool_call`
                    // is client-kind aware); re-materialising here would add a
                    // non-AIR `locations:[]` to an AIR request.
                    let connection = connection.clone();
                    let v2 = v2.clone();
                    // NEW-6: the v2 request is queued to the outbound task HERE,
                    // synchronously, so it keeps its FIFO position among the
                    // connection's updates (an update queued behind this arm
                    // cannot overtake it). P-61: it has a `title` and a `subject`
                    // that carries the tool call's stored `_meta`.
                    let queued = v2.as_ref().map(|v2| {
                        let session_id = params
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        v2.permission(&session_id, params.clone())
                    });
                    tokio::spawn(async move {
                        let mut reply = reply;
                        let result = match queued {
                            Some(sent_rx) => match sent_rx.await {
                                Ok(Ok(sent)) => {
                                    // `requestPermission(params, signal)`: when the
                                    // turn is cancelled the actor drops its receiver
                                    // (it aborts the round-trip), which aborts the
                                    // request — dropping the pending `SentRequest`
                                    // sends `$/cancel_request` so the client can
                                    // dismiss its prompt (`acp-agent.ts:2374-2390`,
                                    // U-107).
                                    tokio::select! {
                                        res = sent.block_task() => res.map_err(|e| e.to_string()),
                                        () = reply.closed() => Err("Tool use aborted".to_string()),
                                    }
                                }
                                Ok(Err(error)) => Err(error.to_string()),
                                Err(_) => Err("Tool use aborted".to_string()),
                            },
                            None => {
                                match UntypedMessage::new("session/request_permission", params) {
                                    Ok(msg) => {
                                        let sent = connection.send_request(msg);
                                        tokio::select! {
                                            res = sent.block_task() => res.map_err(|e| e.to_string()),
                                            () = reply.closed() => Err("Tool use aborted".to_string()),
                                        }
                                    }
                                    Err(e) => Err(e.to_string()),
                                }
                            }
                        };
                        // P-62: any outcome other than `selected` is `cancelled`
                        // in v1 (`v1PermissionResponse`, `v2/permission.ts:52-66`).
                        let result = match (result, &v2) {
                            (Ok(value), Some(_)) => Ok(v1_permission_response(&value)),
                            (other, _) => other,
                        };
                        let _ = reply.send(result);
                    });
                }
                // F23a/F23b: `session/create_elicitation` (the ACP method is
                // `elicitation/create`). Same shape as the permission round trip:
                // spawned here, cancelled by dropping the reply (`$/cancel_request`).
                SessionOutbound::CreateElicitation { params, reply } => {
                    let connection = connection.clone();
                    tokio::spawn(async move {
                        let mut reply = reply;
                        let result = match UntypedMessage::new("elicitation/create", params) {
                            Ok(msg) => {
                                let sent = connection.send_request(msg);
                                tokio::select! {
                                    res = sent.block_task() => res.map_err(|e| e.to_string()),
                                    () = reply.closed() => Err("Tool use aborted".to_string()),
                                }
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        let _ = reply.send(result);
                    });
                }
                // F23b/N-37: `elicitation/complete` is a notification.
                SessionOutbound::CompleteElicitation { params } => {
                    if let Ok(msg) = UntypedMessage::new("elicitation/complete", params) {
                        let _ = connection.send_notification(msg);
                    }
                }
            }
        }
        // The actor ended (the stream died or the session closed): its row shows
        // what the registry and the transcript tell from now on, as
        // `closeQueryStream` does (`acp-agent.ts:2794`).
        board.retire(&board_session_id, incarnation);
    });
}

/// Which resumed-session method is being served. It decides whether the stored
/// history is replayed (`session/load` only) and the session-timing operation
/// name (`session-timing.ts`; a resume creates a session like `session/new`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumedKind {
    Load,
    Resume,
    Fork,
}

impl ResumedKind {
    /// Only `session/load` replays the stored history (`acp-agent.ts:3153-3164`).
    fn replay(self) -> bool {
        self == Self::Load
    }

    /// The `[session/<op>]` operation: `load` and `fork` have their own timers;
    /// a resume creates a session, so it reports as `create`.
    fn operation(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Resume => "create",
            Self::Fork => "fork",
        }
    }
}

/// The zod-shaped `data` of the `-32602` the SDK answers for a `set_config_option`
/// request whose `value` fits neither schema branch: a string `value`, or
/// `type: "boolean"` with a boolean `value` (`error.format()`: every failing
/// branch contributes its message, the boolean branch first). `None` when valid.
fn set_config_option_schema_error(params: &Value) -> Option<Value> {
    let received = |value: Option<&Value>| match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    };
    let value = params.get("value");
    let boolean_branch = params.get("type").and_then(Value::as_str) == Some("boolean");
    let value_is_bool = value.is_some_and(Value::is_boolean);
    let value_is_string = value.is_some_and(Value::is_string);
    if value_is_string || (boolean_branch && value_is_bool) {
        return None;
    }
    let mut value_errors = Vec::new();
    if !value_is_bool {
        value_errors.push(format!(
            "Invalid input: expected boolean, received {}",
            received(value)
        ));
    }
    value_errors.push(format!(
        "Invalid input: expected string, received {}",
        received(value)
    ));
    let mut tree = serde_json::Map::new();
    tree.insert("_errors".to_string(), json!([]));
    tree.insert("value".to_string(), json!({ "_errors": value_errors }));
    if !boolean_branch {
        tree.insert(
            "type".to_string(),
            json!({ "_errors": ["Invalid input: expected \"boolean\""] }),
        );
    }
    Some(Value::Object(tree))
}

/// Whether the resume of a recreated LIVE session failed because Claude Code never
/// wrote its conversation, so it starts a fresh query under the same id instead
/// (`recreateLiveSession`, `acp-agent.ts:4123-4143`). A session that is not loaded
/// here keeps the failure (an unknown session), as does any other error.
fn recreate_falls_back_to_fresh<T>(
    was_live: bool,
    already_fresh: bool,
    result: &Result<T, SessionError>,
) -> bool {
    was_live
        && !already_fresh
        && matches!(
            result,
            Err(SessionError::Start(message))
                if message.contains("No conversation found with session ID")
        )
}

/// The RFD subagent replay context of a load/resume (B-52): a client on the
/// RFD form whose history has an Agent or Task launch gets its subagents
/// replayed into their own sessions (`acp-agent.ts:9223-9234`).
fn rfd_replay_context(
    messages: &[Value],
    session_id: &str,
    env: &ResumeEnv,
    opts: &ServeOptions,
) -> Option<crate::subagent_replay::SubagentReplayContext> {
    if env.caps.subagent_form != Some(crate::client_caps::SubagentForm::Rfd) {
        return None;
    }
    // Only a history with an Agent or Task launch has subagent transcripts to
    // read; the RFD replay still exists, with an empty history
    // (`acp-agent.ts:9216-9234`).
    let history = if has_subagent_launch(messages) {
        crate::subagent_history::subagent_history(session_id, &opts.env)
    } else {
        crate::subagent_history::SubagentHistory::default()
    };
    Some(crate::subagent_replay::SubagentReplayContext {
        history,
        notices: env.caps.notices,
        logger: opts.logger.clone(),
    })
}

/// Whether the history carries an Agent or Task launch.
fn has_subagent_launch(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        let content = message
            .get("message")
            .and_then(|m| m.get("content"))
            .cloned()
            .unwrap_or(Value::Null);
        !crate::native_subagents::subagent_launch_ids(&content).is_empty()
    })
}

/// The RFD subagent replay context of a live `session/load` (B-52): the context
/// [`rfd_replay_context`] builds for a start, with the history read off the
/// registry actor (`spawn_blocking`) so the actor never reads transcripts
/// synchronously.
async fn live_replay_subagents(
    messages: &[Value],
    session_id: &str,
    subagent_form: Option<crate::client_caps::SubagentForm>,
    notices: bool,
    opts: &ServeOptions,
) -> Option<crate::subagent_replay::SubagentReplayContext> {
    if subagent_form != Some(crate::client_caps::SubagentForm::Rfd) {
        return None;
    }
    // A history without a launch still has an (empty) RFD replay, as the
    // start path does (`acp-agent.ts:9226-9234`); only a launch needs the
    // transcript read.
    let history = if has_subagent_launch(messages) {
        let env = opts.env.clone();
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || crate::subagent_history::subagent_history(&id, &env))
            .await
            .unwrap_or_default()
    } else {
        crate::subagent_history::SubagentHistory::default()
    };
    Some(crate::subagent_replay::SubagentReplayContext {
        history,
        notices,
        logger: opts.logger.clone(),
    })
}

/// Whether the recreate `env` belongs to was cancelled.
fn recreate_cancelled(env: &ResumeEnv) -> bool {
    env.cancel
        .as_ref()
        .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::SeqCst))
}

/// Start (or restart) the session `session_id` from its stored transcript
/// (`--resume`) and answer a `session/load`, `session/resume` or `session/fork`
/// request; the new session is registered and its proactive command list sent.
///
/// [`ResumeEnv::live_mode`] is the permission mode of the live session this start
/// recreates (changed-params `load`/`resume`): the recreated session keeps its live
/// mode and falls back to a fresh query under the same id when it was never
/// prompted (`recreateLiveSession`, `acp-agent.ts:4123-4143`). Passed in, not
/// parked in the registry, so a rejected request cannot leave it for a later load
/// to inherit.
///
/// The registry entries of the new session go to [`ResumeEnv::deferred_tx`]; the
/// request handler applies them before it serves the next request.
async fn start_resumed(
    env: &ResumeEnv,
    session_id: String,
    params: &Value,
    opts: &ServeOptions,
    responder: Reply,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    kind: ResumedKind,
) -> Result<(), Error> {
    let replay = kind.replay();
    let started = Instant::now();
    let mut timing =
        SessionTiming::with_started_at(opts.logger.clone(), kind.operation(), &session_id, started);
    let cwd_str = params.get("cwd").and_then(Value::as_str);
    if let Err(e) = validate_cwd(cwd_str) {
        return responder.respond_with_error(e);
    }
    timing.phase("validate-cwd", "");
    let initialize = match initialize_options_for(params, &env.caps, env.air.as_ref(), env.is_v2) {
        Ok(initialize) => initialize,
        Err(e) => return responder.respond_with_error(e),
    };
    let cwd = cwd_str.map(PathBuf::from);
    let mut fresh = false;
    let (start_result, replayed, auth_cwd, mut unwired, auth_context, auth_epoch) = loop {
        let (mut claude, permission_mode, allow_bypass) =
            match spawn_inputs(params, cwd.as_deref(), opts, Some(&session_id)) {
                Ok(inputs) => inputs,
                Err(e) => return responder.respond_with_error(e),
            };
        let permission_mode = env.live_mode.clone().unwrap_or(permission_mode);
        claude.enable_file_checkpointing = Some(initialize.enable_file_checkpointing);
        timing.phase("resume-transcript", "");
        timing.phase("settings", "");
        let raw_sdk_messages = claude.raw_sdk_messages.clone();
        let settings_arg = claude.settings_arg.clone();
        let launch_env = claude.env.clone();
        let ask_disallowed = !env.caps.elicitation_form; // O-135: only a form-capable client keeps AskUserQuestion
        let additional_directories = claude.additional_directories.clone();
        let mut spawn = session_spawn_options(
            opts,
            cwd.clone(),
            session_id.clone(),
            claude,
            permission_mode,
            allow_bypass,
            ask_disallowed,
        );
        if fresh {
            // A new conversation that keeps the old id: `resume` stays unset, so
            // the id reaches the SDK as `options.sessionId`.
            spawn.resume = None;
            spawn.session_id = Some(session_id.clone());
        } else {
            spawn.resume = Some(session_id.clone());
            spawn.session_id = None;
        }
        let auth_cwd = spawn.default_cwd.clone().unwrap_or_default();
        let auth_context =
            session_auth_context(auth_cwd.clone(), settings_arg.as_deref(), &launch_env, opts);
        let auth_epoch = match &env.auth {
            Some(auth) => auth.use_context(auth_context.clone()).await,
            None => 0,
        };
        providers::apply_provider_to_spawn(&mut spawn, env.provider.as_ref());
        let sess_opts = SessionOptions::new(spawn, opts.timings.clone(), initialize.clone())
            .with_air(env.air.clone())
            .with_caps(env.caps.clone())
            .with_raw_sdk_messages(raw_sdk_messages)
            .with_env(opts.env.clone())
            .with_logger(opts.logger.clone())
            .with_gateway_models(providers::gateway_models_of(env.provider.as_ref()))
            .with_v2(env.is_v2)
            .with_additional_directories(additional_directories);
        // PC-2.2 / B-02: the replay runs while the CLI starts (Node
        // `createSessionWhileReplaying`) and reads the transcript off the async
        // runtime; the response waits for both, so the replay frames still
        // precede it.
        let air = env.air.is_some();
        let replay_future = async {
            let messages = crate::transcript::session_messages_async(&opts.env, &session_id).await;
            let mut failure_book = crate::session::failures::SessionFailures::default();
            // B-45: replay restores background task cards for a client that
            // takes AIR async tasks; B-55: the loop yields to the event loop
            // between messages on the upstream `messageLoopYielder` budget.
            let async_tasks_enabled = env.air.as_ref().is_some_and(|a| a.supports("asyncTasks"));
            let mut replay_async_tasks =
                crate::async_tasks::AsyncTaskRuntime::new(async_tasks_enabled);
            let mut replay_message_ids = crate::message_ids::MessageIdMap::default();
            let mut yielder = crate::event_loop_yield::EventLoopYielder::with_clock(
                crate::event_loop_yield::MESSAGE_LOOP_BUDGET_MS,
            );
            // B-52: an RFD client gets the history's subagents replayed into
            // their own sessions right after their launches
            // (`acp-agent.ts:9479-9498`).
            let subagents = rfd_replay_context(&messages, &session_id, env, opts);
            let (notifications, task_state, last_published) =
                crate::transcript::replay_session_async_with_subagents(
                    &messages,
                    &session_id,
                    air,
                    env.caps.plan,
                    crate::transcript::ReplaySinks {
                        failures: env
                            .air
                            .as_ref()
                            .is_some_and(|a| a.supports("sessionFailure"))
                            .then_some(&mut failure_book),
                        async_tasks: Some(&mut replay_async_tasks),
                        message_ids: Some(&mut replay_message_ids),
                        // B-50: a client that takes ID-addressed compaction
                        // updates replays a compact summary as an entity.
                        compaction: env.caps.compaction,
                    },
                    &mut yielder,
                    subagents,
                )
                .await;
            let v2 = env.v2_outbound.clone();
            if let Some(v2) = &v2 {
                v2.begin_replay(&session_id);
            }
            let mut replay_stopped = false;
            for mut value in notifications {
                // A cancelled recreate stops replaying into a connection that is
                // ending.
                if recreate_cancelled(env) {
                    replay_stopped = true;
                    break;
                }
                if let Some(update) = value.get_mut("update") {
                    crate::map::materialize_tool_call_defaults(update);
                    crate::map::materialize_diff_old_text(update);
                }
                if let Some(v2) = &v2 {
                    // B-112: a v2 client gets the history in its v2 form, each
                    // message cleared before its first replayed chunk.
                    let replay_session = value
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or(session_id.as_str())
                        .to_string();
                    if let Some(update) = value.get("update") {
                        v2.translate(&replay_session, update.clone());
                    }
                    continue;
                }
                if let Ok(msg) = UntypedMessage::new("session/update", value) {
                    let _ = connection.send_notification(msg);
                }
            }
            if let Some(v2) = &v2 {
                v2.end_replay(&session_id);
            }
            // B-53: a client with `loadReplayEnd` gets one more update after
            // the whole replay (`acp-agent.ts:9529-9544`). A stopped replay
            // does not get it.
            if env.caps.load_replay_end && !recreate_cancelled(env) && !replay_stopped {
                let marker = crate::transcript::load_replay_end_marker(&session_id);
                match &v2 {
                    Some(v2) => {
                        if let Some(update) = marker.get("update") {
                            v2.translate(&session_id, update.clone());
                        }
                    }
                    None => {
                        if let Ok(msg) = UntypedMessage::new("session/update", marker) {
                            let _ = connection.send_notification(msg);
                        }
                    }
                }
            }
            (task_state, last_published, failure_book, replay_message_ids)
        };
        let no_replay: (
            Vec<(String, crate::hooks::TaskStateEntry)>,
            Option<String>,
            crate::session::failures::SessionFailures,
            crate::message_ids::MessageIdMap,
        ) = (Vec::new(), None, Default::default(), Default::default());
        // The guard is created the moment the start returns, so a session that is
        // waiting for the replay to finish is already covered.
        let start_future = async {
            let result = Session::start(sess_opts, session_id.clone()).await;
            let guard = UnwiredSessionGuard(
                result
                    .as_ref()
                    .ok()
                    .map(|(session, _)| session.command_sender().clone()),
            );
            (result, guard)
        };
        let ((start_result, unwired), replayed) = if replay {
            tokio::join!(start_future, replay_future)
        } else {
            (start_future.await, no_replay)
        };
        if recreate_falls_back_to_fresh(env.live_mode.is_some(), fresh, &start_result) {
            // A live session may never have been prompted, so it may have no stored
            // conversation to resume: it has no history to lose, and the client
            // keeps a session it can use. Any other failure does not start a fresh
            // query.
            if let Some(logger) = &opts.logger {
                logger.log(&format!(
                    "session {session_id} was never persisted; starting a fresh query under the same id"
                ));
            }
            fresh = true;
            continue;
        }
        break (
            start_result,
            replayed,
            auth_cwd,
            unwired,
            auth_context,
            auth_epoch,
        );
    };
    match start_result {
        Ok((session, update_rx)) => {
            timing.phase("session-ready", "");
            // The recreate was cancelled while the child started (the client
            // closed the session, or the connection is ending): the new session
            // is neither registered nor left running.
            if recreate_cancelled(env) {
                drop(update_rx);
                session.close().await;
                return responder.respond_with_error(session_rpc_error(&SessionError::Closed));
            }
            // The account is published before the guard may refuse the session
            // (a refusal is when the client most needs to know the account).
            let mut guard = crate::hide_claude_auth::GuardState::default();
            if let Err(refusal) = account_at_creation(
                &env.auth_env,
                &session_id,
                &auth_context,
                auth_epoch,
                &session,
                &mut guard,
            ) {
                drop(update_rx);
                session.close().await;
                return responder.respond_with_error(refusal);
            }
            // Phase 15 (Item 1): upstream sends a proactive
            // `available_commands_update` with the REAL command list right
            // after session/new / session/load
            // (`sendAvailableCommandsUpdate` → `supportedCommands()`),
            // because a plain session may never emit a `commands_changed`
            // system frame. Build it from the initialize handshake's
            // `commands` (a fresh session has no terminal slash commands).
            let commands = session.commands().to_vec();
            let modes = session.modes().clone();
            let config_options = session.config_options().to_vec();
            // rust-parity-fixes 3.5 (Decision 14, P10): a `session/load` replays
            // the stored history BEFORE the response (Node `loadSession` →
            // `replaySessionHistory`); `session/resume` and `session/fork` do
            // not. The replay frames were sent by `replay_future`; PC-2.3 seeds
            // the Task* state it rebuilt into the live actor so a status-only
            // TaskUpdate after the load still applies.
            if replay {
                let (task_state, last_published, failure_book, replay_message_ids) = replayed;
                let _ = session.seed_task_state(task_state, last_published).await;
                let _ = session.seed_failures(failure_book).await;
                session.seed_message_ids(replay_message_ids);
                timing.phase("replay", "");
            }
            let mgr = Arc::new(crate::settings_manager::SettingsManager::new(
                auth_cwd.clone(),
                Some(crate::settings_manager::SettingsManagerOptions {
                    on_change: None,
                    logger: opts.logger.clone(),
                    env: Some(opts.env.clone()),
                    managed_docs: None, // env snapshot carries the injected managed docs
                    managed_file: None,
                    poll_interval: None,
                }),
            ));
            // The registry entries wait for the request handler; the session's
            // own wiring (notification handler, dispose list, update forwarder)
            // does not need the registry.
            let _ = env
                .deferred_tx
                .send(RegistryMsg::Register(Box::new(RegistryUpdate {
                    session_id: session_id.clone(),
                    session: session.clone(),
                    settings_manager: mgr,
                    fingerprint: compute_session_fingerprint(params),
                    cancel: env.cancel.clone(),
                    creation_params: params.clone(),
                    auth_context,
                    guard,
                })));
            // Published to the dispose list below, which closes it from here on.
            unwired.disarm();
            // A provider update that runs meanwhile recreates this session too.
            env.sessions_view.send_modify(|view| {
                view.insert(session_id.clone(), (session.clone(), params.clone()));
            });
            // It resumed a stored conversation: it has a transcript, unlike a new
            // session before its first turn (`resumedFromHistory`).
            if !fresh {
                env.board
                    .update(&session_id, |own| own.resumed_from_history = true);
            }
            wire_session(
                WireTargets {
                    session_tx: &env.session_tx,
                    dispose_tx: &env.dispose_tx,
                    deferred_tx: &env.deferred_tx,
                    board: &env.board,
                    is_air_client: env.air.is_some(),
                    is_v2: env.is_v2,
                    v2: env.v2_outbound.as_ref(),
                },
                &session_id,
                session.clone(),
                update_rx,
                connection,
            );
            timing.phase("register", "");
            responder.respond(session_response(&session_id, &modes, &config_options))?;
            start_setup_mcp_auth(&session, params);
            // S-22: an AIR client gets `kind`/`skillPath` on its skill commands
            // (`acp-agent.ts:12560-12652`).
            let air_cwd = env.air.as_ref().map(|_| auth_cwd.as_path());
            let home = opts.env.get_path("HOME");
            let update = map::map_commands_changed_with_skills(
                &json!(commands),
                &[],
                air_cwd,
                home.as_deref(),
            );
            // Decision 8: raw JSON with `input: null` for commands without a
            // hint (the typed update would omit the field).
            if !env.suppress_commands_update {
                let value = json!({
                    "sessionId": session_id.clone(),
                    "update": map::available_commands_json(&update),
                });
                if let Ok(msg) = UntypedMessage::new("session/update", value) {
                    let _ = connection.send_notification(msg);
                }
            }
            Ok(())
        }
        Err(e) => responder.respond_with_error(resumed_start_error(&e, true, &session_id)),
    }
}

/// Dispatch one ACP request (8.1). `registry` is owned by the handler closure;
/// it is mutated sequentially by the connection loop.
///
/// A connection that negotiated v2 in `initialize` is served by the v2 agent
/// surface (M-106, `serve.ts:37-51`): each v2 request becomes the v1 request
/// that the agent serves, and its answer is translated back. `initialize`
/// itself stays on the v1 path, which negotiates the version and answers a v2
/// handshake (B1). Without the opt-in, every request is served v1 exactly as
/// before (O-139).
async fn handle_request(
    registry: &mut Registry,
    req: UntypedMessage,
    opts: &ServeOptions,
    responder: agent_client_protocol::Responder<Value>,
    connection: ConnectionTo<agent_client_protocol::Client>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<(), Error> {
    let method = req.method().to_string();
    if registry.protocol == crate::v2::ProtocolMode::V2 && method != "initialize" {
        let params = req.params().clone();
        return crate::v2::agent::handle_v2_request(
            registry, &method, params, opts, responder, connection, permit,
        )
        .await;
    }
    handle_v1_request(
        registry,
        req,
        opts,
        Reply::Wire(responder),
        connection,
        permit,
    )
    .await
}

/// Serve one request on the v1 protocol. Its answer goes through `responder`:
/// the wire responder, or a capture channel when the v2 surface translates it.
pub(crate) async fn handle_v1_request(
    registry: &mut Registry,
    req: UntypedMessage,
    opts: &ServeOptions,
    responder: Reply,
    connection: ConnectionTo<agent_client_protocol::Client>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<(), Error> {
    let method = req.method().to_string();
    let params = req.params().clone();
    // Sessions started off this handler (a recreate) have left their entries.
    registry.apply_deferred();

    match method.as_str() {
        "initialize" => {
            // B1: with experimental v2 negotiated, translate the v2 request to
            // the v1 request the agent reads BEFORE any parsing, so a v2
            // client's `capabilities` are honoured. Without the opt-in the
            // params pass through unchanged (O-139).
            let negotiated = match crate::v2::negotiate_initialize(
                &params,
                crate::v2::is_experimental_v2_enabled(&opts.env),
            ) {
                Ok(negotiated) => negotiated,
                // L4/L5: an invalid version gets the upstream router error
                // instead of a dropped reply.
                Err(error) => {
                    responder.respond_with_error(error)?;
                    return Ok(());
                }
            };
            registry.protocol = negotiated.mode;
            let params = &negotiated.params;
            // Parse the AIR client declaration once (Decision 11); the optional
            // `clientCapabilities._meta.jetbrains.air` gates the AIR advertise
            // (E-4 / X-1). Malformed -> None (a plain client).
            registry.air = params
                .get("clientCapabilities")
                .and_then(AirClient::from_client_capabilities);
            // Parse the full capability set once (ROADMAP F2a): auth methods,
            // notices, config options and the tool-call choices all read it.
            registry.caps = ClientCapabilities::from_value(params.get("clientCapabilities"));
            // T-102/T-104 (`client-capabilities.ts:45`): a v2 client takes the
            // terminal extension even though it declares no `_meta` marker; the
            // v2 surface sends commands' terminals as display terminals.
            if negotiated.mode == crate::v2::ProtocolMode::V2 {
                registry.caps.terminal_output = true;
            }
            // The session index of a client that declared `sessionIndex`.
            let notifier_connection = connection.clone();
            registry.index.set_notifier(Arc::new(move |changes| {
                let connection = notifier_connection.clone();
                Box::pin(async move {
                    let msg = UntypedMessage::new(
                        crate::session_index::list_subscriptions::LIST_CHANGES_METHOD,
                        changes,
                    )?;
                    connection.send_notification(msg)
                })
            }));
            registry.index.negotiate(registry.air.as_ref());
            let (executable, argv) = process_entrypoint();
            let v1_resp = initialize_response(
                registry.air.as_ref(),
                &registry.caps,
                &opts.env,
                &executable,
                &argv,
                &registry.index.capabilities(),
                registry.hide_claude_auth,
            );
            if negotiated.mode == crate::v2::ProtocolMode::V2 {
                match crate::v2::v2_initialize_response(&v1_resp) {
                    Ok(v2_resp) => responder.respond(v2_resp)?,
                    // L5: answer the client instead of dropping the reply.
                    Err(error) => responder.respond_with_error(error)?,
                }
            } else {
                responder.respond(v1_resp)?;
            }
            // G2 (Decision 11): learn the auth identity in the background AFTER
            // the response (Node `initialize` never waits on the CLI probe);
            // `_auth/status_update` is pushed only when the read lands.
            let auth = registry
                .auth
                .get_or_insert_with(|| spawn_auth(opts, registry.hide_claude_auth, &connection))
                .clone();
            auth.probe(std::env::current_dir().unwrap_or_default(), false);
            Ok(())
        }
        "session/new" => {
            // A provider update in flight recreates every session; a session
            // created meanwhile must start under the routing it installs: the
            // actor parked this request until the update ended and handed over
            // the gate's permit (`while (this.providerUpdate) await ...`).
            let _provider_update = permit;
            // Session timing (`session-timing.ts`, B-01): coarse phases at the
            // agent layer (validate-cwd/settings/register).
            let create_started = Instant::now();
            let cwd_str = params.get("cwd").and_then(Value::as_str);
            if let Err(e) = validate_cwd(cwd_str) {
                return responder.respond_with_error(e);
            }
            let initialize = match initialize_options_for(
                &params,
                &registry.caps,
                registry.air.as_ref(),
                registry.protocol == crate::v2::ProtocolMode::V2,
            ) {
                Ok(initialize) => initialize,
                Err(e) => return responder.respond_with_error(e),
            };
            let cwd = cwd_str.map(PathBuf::from);
            let (mut claude, permission_mode, allow_bypass) =
                match spawn_inputs(&params, cwd.as_deref(), opts, None) {
                    Ok(inputs) => inputs,
                    Err(e) => return responder.respond_with_error(e),
                };
            claude.enable_file_checkpointing = Some(initialize.enable_file_checkpointing);
            let session_id = claude.resume.clone().unwrap_or_else(uuid_v4);
            let resumed = claude.resume.is_some();
            let mut timing = SessionTiming::with_started_at(
                opts.logger.clone(),
                "create",
                &session_id,
                create_started,
            );
            timing.phase("validate-cwd", "");
            if resumed {
                timing.phase("resume-transcript", "");
            }
            timing.phase("settings", "");
            let raw_sdk_messages = claude.raw_sdk_messages.clone();
            let settings_arg = claude.settings_arg.clone();
            let launch_env = claude.env.clone();
            let ask_disallowed = !registry.caps.elicitation_form; // O-135: only a form-capable client keeps AskUserQuestion
            let additional_directories = claude.additional_directories.clone();
            let mut spawn = session_spawn_options(
                opts,
                cwd,
                session_id.clone(),
                claude,
                permission_mode,
                allow_bypass,
                ask_disallowed,
            );
            let auth_cwd = spawn.default_cwd.clone().unwrap_or_default();
            // `useAuthStatusSettings`: a context that differs from the last probe's
            // bumps the epoch and re-probes; the account read below is published
            // only while that epoch is still the current one.
            let auth_context =
                session_auth_context(auth_cwd.clone(), settings_arg.as_deref(), &launch_env, opts);
            let auth_epoch = match &registry.auth {
                Some(auth) => auth.use_context(auth_context.clone()).await,
                None => 0,
            };
            let provider = registry.resolve_provider();
            providers::apply_provider_to_spawn(&mut spawn, provider.as_ref());
            let sess_opts = SessionOptions::new(spawn, opts.timings.clone(), initialize)
                .with_air(registry.air.clone())
                .with_caps(registry.caps.clone())
                .with_raw_sdk_messages(raw_sdk_messages)
                .with_env(opts.env.clone())
                .with_logger(opts.logger.clone())
                .with_gateway_models(providers::gateway_models_of(provider.as_ref()))
                .with_v2(registry.protocol == crate::v2::ProtocolMode::V2)
                .with_additional_directories(additional_directories);
            match Session::start(sess_opts, session_id.clone()).await {
                Ok((session, update_rx)) => {
                    let mut guard = crate::hide_claude_auth::GuardState::default();
                    if let Err(refusal) = account_at_creation(
                        &AuthEnv::of(registry, opts),
                        &session_id,
                        &auth_context,
                        auth_epoch,
                        &session,
                        &mut guard,
                    ) {
                        drop(update_rx);
                        session.close().await;
                        return responder.respond_with_error(refusal);
                    }
                    registry
                        .creation_params
                        .insert(session_id.clone(), params.clone());
                    registry
                        .auth_contexts
                        .insert(session_id.clone(), auth_context);
                    registry.guard_states.insert(session_id.clone(), guard);
                    // Phase 15 (Item 1): upstream sends a proactive
                    // `available_commands_update` with the REAL command list right
                    // after session/new / session/load
                    // (`sendAvailableCommandsUpdate` → `supportedCommands()`),
                    // because a plain session may never emit a `commands_changed`
                    // system frame. Build it from the initialize handshake's
                    // `commands` (a fresh session has no terminal slash commands).
                    let commands = session.commands().to_vec();
                    let modes = session.modes().clone();
                    let config_options = session.config_options().to_vec();
                    let mgr = Arc::new(crate::settings_manager::SettingsManager::new(
                        auth_cwd.clone(),
                        Some(crate::settings_manager::SettingsManagerOptions {
                            on_change: None,
                            logger: opts.logger.clone(),
                            env: Some(opts.env.clone()),
                            managed_docs: None, // env snapshot carries the injected managed docs
                            managed_file: None,
                            poll_interval: None,
                        }),
                    ));
                    registry.settings_managers.insert(session_id.clone(), mgr);
                    registry
                        .fingerprints
                        .insert(session_id.clone(), compute_session_fingerprint(&params));
                    register_session(
                        registry,
                        &session_id,
                        session.clone(),
                        update_rx,
                        &connection,
                    );
                    timing.phase("register", "");
                    responder.respond(session_response(&session_id, &modes, &config_options))?;
                    start_setup_mcp_auth(&session, &params);
                    // S-22: an AIR client gets `kind`/`skillPath` on its skill
                    // commands (`acp-agent.ts:12560-12652`).
                    let air_cwd = registry
                        .air
                        .as_ref()
                        .and_then(|_| params.get("cwd").and_then(Value::as_str))
                        .map(PathBuf::from);
                    let home = opts.env.get_path("HOME");
                    let update = map::map_commands_changed_with_skills(
                        &json!(commands),
                        &[],
                        air_cwd.as_deref(),
                        home.as_deref(),
                    );
                    // Decision 8: raw JSON with `input: null` for commands
                    // without a hint (the typed update would omit the field).
                    if !registry.suppress_commands_update {
                        let value = json!({
                            "sessionId": session_id.clone(),
                            "update": map::available_commands_json(&update),
                        });
                        if let Ok(msg) = UntypedMessage::new("session/update", value) {
                            let _ = connection.send_notification(msg);
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    responder.respond_with_error(resumed_start_error(&e, resumed, &session_id))
                }
            }
        }
        // `session/resume` is `session/load` without the history replay, which
        // the Rust port never does — the same start path (G6).
        "session/load" | "session/resume" => {
            // A provider update in flight recreates every session; this start was
            // parked until it ended, and the spawned recreate below keeps its
            // permit until it registers.
            let provider_update = permit;
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let replay = method == "session/load";
            // The id is mid-recreate (its old session is torn down, the new one
            // not yet up): a second start would run a second child on the same
            // transcript. Node serializes these on the awaited teardown; the
            // recreate runs off this handler, so the second request is refused.
            if !registry.sessions.contains_key(&session_id)
                && registry.recreating.contains_key(&session_id)
            {
                return responder.respond_with_error(Error::internal_error().data(json!({
                    "details": format!("Session {session_id} is being recreated")
                })));
            }
            // Follow-up (a): if the request targets an already live session,
            // reuse it without leaking or starting a redundant child process. A
            // `session/load` on a live session replays its history (X8,
            // `acp-agent.js:1583-1587`); `session/resume` does not.
            // `getOrCreateSession`: reject a prompt the agent cannot apply before
            // reusing a live session, then compare the session-defining params
            // with the live session's fingerprint (`acp-agent.ts:10868-10900`).
            let live = registry.sessions.get(&session_id).cloned();
            let mut changed = false;
            if live.is_some() {
                if let Err(e) = initialize_options_for(
                    &params,
                    &registry.caps,
                    registry.air.as_ref(),
                    registry.protocol == crate::v2::ProtocolMode::V2,
                ) {
                    return responder.respond_with_error(e);
                }
                changed = registry
                    .fingerprints
                    .get(&session_id)
                    .is_some_and(|stored| *stored != compute_session_fingerprint(&params));
            }
            if changed {
                // Session-defining params changed (cwd pointed at a git worktree,
                // MCP servers reconfigured, the skill set changed, ...): tear the
                // live session down and recreate it so the process picks up the
                // new values. A recreated live session keeps its LIVE permission
                // mode, not the mode of the transcript.
                registry.sessions.remove(&session_id);
                registry.settings_managers.remove(&session_id);
                registry.fingerprints.remove(&session_id);
                registry.forget_session_state(&session_id);
                // The old session is torn down, as upstream's recreate does
                // (`teardownSession` → `onTeardown`); its row falls back to the
                // transcript until the new session registers.
                registry.index.on_teardown(&session_id);
                // The teardown can take seconds (the close is bounded, not
                // instant): it runs off the connection's sequential handler, so
                // other requests and `session/cancel` are not held up, and the
                // response is still sent after the old session is gone.
                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let mut env = registry.resume_env(None, opts);
                env.cancel = Some(cancel.clone());
                let opts = opts.clone();
                let connection = connection.clone();
                let kind = if replay {
                    ResumedKind::Load
                } else {
                    ResumedKind::Resume
                };
                // The id stays marked until the task ends: while the session is
                // in no registry map, a `session/close` cancels the recreate and
                // a second `load` is refused.
                let (ended_tx, ended) = watch::channel(false);
                registry.recreating.insert(
                    session_id.clone(),
                    Recreating {
                        cancel: cancel.clone(),
                        ended,
                    },
                );
                let ending = RecreateEnding {
                    deferred_tx: registry.deferred_tx.clone(),
                    session_id: session_id.clone(),
                    cancel: cancel.clone(),
                    ended_tx,
                };
                let handle = tokio::spawn(async move {
                    // Dropped however the task ends (a panic included), so the id
                    // never stays marked as being recreated.
                    let _ending = ending;
                    // The provider update gate stays held until this session is
                    // registered, so a switch that follows waits for it.
                    let _provider_update = provider_update;
                    let mut env = env;
                    if let Some(session) = live {
                        let live_mode = match session.live_state().await {
                            Ok((modes, _)) => modes.current_mode_id,
                            Err(_) => session.modes().current_mode_id.clone(),
                        };
                        session.close().await;
                        env.live_mode = Some(live_mode);
                    }
                    if recreate_cancelled(&env) {
                        let _ =
                            responder.respond_with_error(session_rpc_error(&SessionError::Closed));
                    } else {
                        let _ = start_resumed(
                            &env,
                            session_id,
                            &params,
                            &opts,
                            responder,
                            &connection,
                            kind,
                        )
                        .await;
                    }
                });
                let _ = registry.recreate_tx.send(RecreateTask { cancel, handle });
                return Ok(());
            } else if let Some(session) = live.as_ref() {
                if replay {
                    let messages =
                        crate::transcript::session_messages_async(&opts.env, &session_id).await;
                    let typed_failures = registry
                        .air
                        .as_ref()
                        .is_some_and(|a| a.supports("sessionFailure"));
                    let mut failure_book = if typed_failures {
                        session.failures_snapshot().await.unwrap_or_default()
                    } else {
                        Default::default()
                    };
                    // B-52: a live `session/load` replays its subagents into
                    // their own sessions like `start_resumed` does
                    // (`acp-agent.ts:3156-3160` uses `replaySessionHistory` in
                    // both branches). The context's history is read off the
                    // registry actor.
                    let subagents = live_replay_subagents(
                        &messages,
                        &session_id,
                        registry.caps.subagent_form,
                        registry.caps.notices,
                        opts,
                    )
                    .await;
                    let (notifications, task_state, last_published) =
                        crate::transcript::replay_with_failures_rfd(
                            &messages,
                            &session_id,
                            registry.air.is_some(),
                            registry.caps.plan,
                            registry.caps.compaction,
                            typed_failures.then_some(&mut failure_book),
                            subagents,
                        );
                    let v2 = registry.v2_outbound.clone();
                    if let Some(v2) = &v2 {
                        v2.begin_replay(&session_id);
                    }
                    for mut value in notifications {
                        if let Some(update) = value.get_mut("update") {
                            crate::map::materialize_tool_call_defaults(update);
                            crate::map::materialize_diff_old_text(update);
                        }
                        if let Some(v2) = &v2 {
                            let replay_session = value
                                .get("sessionId")
                                .and_then(Value::as_str)
                                .unwrap_or(session_id.as_str())
                                .to_string();
                            if let Some(update) = value.get("update") {
                                v2.translate(&replay_session, update.clone());
                            }
                            continue;
                        }
                        if let Ok(msg) = UntypedMessage::new("session/update", value) {
                            let _ = connection.send_notification(msg);
                        }
                    }
                    if let Some(v2) = &v2 {
                        v2.end_replay(&session_id);
                    }
                    // Node forgets the published plan before a replay and shares
                    // the live `taskState` with it; seed the rebuilt state so the
                    // live actor continues from it.
                    let _ = session.seed_task_state(task_state, last_published).await;
                    if typed_failures {
                        let _ = session.seed_failures(failure_book).await;
                    }
                }
                // MAJOR 5 (acp-agent.js:8186-8190): return the actor's LIVE
                // state — after `set_config_option mode plan` a resume must
                // report currentModeId "plan", not the creation-time copy.
                match session.live_state().await {
                    Ok((modes, config_options)) => {
                        responder.respond(session_response(
                            &session_id,
                            &modes,
                            &config_options,
                        ))?;
                    }
                    Err(_) => {
                        responder.respond(session_response(
                            &session_id,
                            session.modes(),
                            session.config_options(),
                        ))?;
                    }
                }
                // Node's `afterSetupResponse` re-sends the proactive
                // `available_commands_update` after EVERY setup response,
                // including an already-live `session/load` AND `session/resume`
                // (`acp-agent.ts:3144-3149,10434-10439`); B-118. The update
                // follows the response, like Node's `setTimeout(..., 0)`.
                {
                    let commands = session.live_commands().await;
                    // S-22: an AIR client gets `kind`/`skillPath` on its skill
                    // commands (`acp-agent.ts:12560-12652`).
                    let air_cwd = registry
                        .air
                        .as_ref()
                        .and_then(|_| params.get("cwd").and_then(Value::as_str))
                        .map(PathBuf::from);
                    let home = opts.env.get_path("HOME");
                    let update = map::map_commands_changed_with_skills(
                        &json!(commands),
                        &[],
                        air_cwd.as_deref(),
                        home.as_deref(),
                    );
                    if !registry.suppress_commands_update {
                        let value = json!({
                            "sessionId": session_id.clone(),
                            "update": map::available_commands_json(&update),
                        });
                        if let Ok(msg) = UntypedMessage::new("session/update", value) {
                            let _ = connection.send_notification(msg);
                        }
                    }
                }
                return Ok(());
            }
            start_resumed(
                &registry.resume_env(None, opts),
                session_id,
                &params,
                opts,
                responder,
                &connection,
                if replay {
                    ResumedKind::Load
                } else {
                    ResumedKind::Resume
                },
            )
            .await
        }
        "session/fork" => {
            // A fork starts a session like a resumed one; it was parked until a
            // provider update in flight ended.
            let _provider_update = permit;
            // Node order: validate the systemPrompt, copy the transcript, then
            // start the new session like a resumed one (`unstable_forkSession`).
            if let Err(e) = initialize_options_for(
                &params,
                &registry.caps,
                registry.air.as_ref(),
                registry.protocol == crate::v2::ProtocolMode::V2,
            ) {
                return responder.respond_with_error(e);
            }
            let fork_session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // M-33/M-34/M-36: resolve the AIR fork point through the live
            // messageId map, the active chain and the full transcript
            // (`forkSession`, `fork-session.ts:194-237`). An unresolvable point
            // is `invalidParams` before any copy or list change.
            //
            // A malformed point (a non-string `messageId`) is the SDK's
            // TypeError from `fork.messageId?.trim()`, i.e. an internal error,
            // never a silent full fork.
            let fork_point = match crate::fork_session::fork_point(
                params.get("_meta").unwrap_or(&Value::Null),
            ) {
                Ok(point) => point,
                Err(error) => {
                    return responder.respond_with_error(
                        Error::internal_error().data(json!({ "details": error.to_string() })),
                    );
                }
            };
            let up_to = match fork_point {
                None => None,
                Some(point) => {
                    // B-29: prefer the live map (the ids the client saw live).
                    let live_session = registry.sessions.get(&fork_session_id).cloned();
                    let live_map = match live_session {
                        Some(session) => Some(session.message_ids().await),
                        None => None,
                    };
                    let active = crate::transcript::session_messages(&opts.env, &fork_session_id);
                    let full = crate::transcript::full_history(&opts.env, &fork_session_id);
                    match crate::fork_session::resolve_fork_point(
                        &point,
                        live_map.as_ref(),
                        &active,
                        &full,
                    ) {
                        Some((uuid, _)) => Some(uuid),
                        None => {
                            return responder.respond_with_error(
                                Error::new(
                                    -32602,
                                    format!(
                                        "Invalid params: Fork point message {} was not found in session {fork_session_id}",
                                        point.message_id
                                    ),
                                )
                                .data(json!({ "messageId": point.message_id })),
                            );
                        }
                    }
                }
            };
            // The session list holds back this fork until its response is out
            // (`SessionIndexConnection.forking`).
            registry.index.fork_started(&fork_session_id);
            let new_id =
                match session_methods::fork_copy(fork_session_id.clone(), cwd, up_to, &opts.env)
                    .await
                {
                    Ok(id) => id,
                    Err(e) => {
                        registry.index.fork_answered(&fork_session_id);
                        return responder.respond_with_error(e);
                    }
                };
            registry.index.on_fork_created(&fork_session_id, &new_id);
            let started = start_resumed(
                &registry.resume_env(None, opts),
                new_id,
                &params,
                opts,
                responder,
                &connection,
                ResumedKind::Fork,
            )
            .await;
            registry.index.fork_answered(&fork_session_id);
            started
        }
        "session/close" => {
            // Idempotent: an unknown (or already closed) session answers `{}`.
            // The registry entry is removed first, so a later request sees it gone.
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            registry.settings_managers.remove(&session_id);
            registry.fingerprints.remove(&session_id);
            registry.forget_session_state(&session_id);
            // Its row shows what the registry and the transcript tell from now
            // on, as upstream's `teardownSession` (`acp-agent.ts:8886`); a failed
            // query of it stays remembered.
            registry.index.on_teardown(&session_id);
            // The view a waiting provider update reads must not keep it.
            let closed = registry.sessions.remove(&session_id);
            registry.publish_sessions();
            // A session mid-recreate is in no map: closing it cancels the
            // recreate, and the close answers once that has ended (its new
            // child closed, or never started).
            let recreate_ended = registry.recreating.remove(&session_id).map(|recreating| {
                recreating
                    .cancel
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                recreating.ended
            });
            match (closed, recreate_ended) {
                (None, None) => responder.respond(json!({})),
                (session, recreate_ended) => {
                    tokio::spawn(async move {
                        if let Some(session) = session {
                            session.close().await;
                        }
                        if let Some(mut ended) = recreate_ended {
                            let _ = tokio::time::timeout(
                                DISPOSE_TIMEOUT,
                                ended.wait_for(|ended| *ended),
                            )
                            .await;
                        }
                        let _ = responder.respond(json!({}));
                    });
                    Ok(())
                }
            }
        }
        "session/set_mode" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mode_id = params
                .get("modeId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                None => {
                    return responder.respond_with_error(
                        Error::internal_error().data(json!({ "details": "Session not found" })),
                    );
                }
            };
            tokio::spawn(async move {
                match session.set_mode(mode_id).await {
                    Ok(()) => {
                        let _ = responder.respond(json!({}));
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(set_request_error(e));
                    }
                }
            });
            Ok(())
        }
        "session/set_config_option" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let config_id = params
                .get("configId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // The SDK validates the request against its schema before the handler
            // runs: a bare string `value`, or `{type: "boolean", value: <bool>}`.
            if let Some(data) = set_config_option_schema_error(&params) {
                return responder.respond_with_error(
                    Error::new(-32602, "Invalid params".to_string()).data(data),
                );
            }
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            let session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                None => {
                    return responder.respond_with_error(
                        Error::internal_error().data(json!({ "details": "Session not found" })),
                    );
                }
            };
            tokio::spawn(async move {
                match session.set_config_option(config_id, value).await {
                    Ok(config_options) => {
                        let _ = responder.respond(json!({ "configOptions": config_options }));
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(set_request_error(e));
                    }
                }
            });
            Ok(())
        }
        "session/list" => {
            // A `sessionIndex` client gets the bounded, ordered index list.
            let indexed = registry.index.list(params.clone());
            let env = opts.env.clone();
            let is_air_client = registry.air.is_some();
            tokio::spawn(async move {
                let result = match indexed {
                    Some(indexed) => indexed.await,
                    None => session_methods::list(&params, &env, is_air_client).await,
                };
                match result {
                    Ok(result) => {
                        let _ = responder.respond(result);
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(e);
                    }
                }
            });
            Ok(())
        }
        // `session/delete`: a `sessionIndex` client deletes every copy; another AIR
        // client archives instead (it uses delete to mark a session done); every
        // other client gets the SDK delete.
        "session/delete" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Tear a loaded session down first so its transcript is not recreated.
            let loaded = registry.sessions.remove(&session_id);
            registry.publish_sessions();
            registry.settings_managers.remove(&session_id);
            registry.fingerprints.remove(&session_id);
            registry.forget_session_state(&session_id);
            registry.index.on_teardown(&session_id);
            let was_loaded = loaded.is_some();
            let indexed = registry.index.delete_session(
                session_id.clone(),
                was_loaded,
                registry.air.is_some(),
            );
            let env = opts.env.clone();
            tokio::spawn(async move {
                if let Some(session) = loaded {
                    session.close().await;
                }
                let result = match indexed {
                    Some(indexed) => indexed.await,
                    None => session_methods::delete(session_id, &env).await,
                };
                match result {
                    Ok(()) => {
                        let _ = responder.respond(json!({}));
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(e);
                    }
                }
            });
            Ok(())
        }
        "_session/list/subscribe" => match registry.index.subscribe_list(&params) {
            Ok(subscription) => {
                tokio::spawn(async move {
                    match subscription.await {
                        Ok(result) => {
                            let _ = responder.respond(result);
                        }
                        Err(e) => {
                            let _ = responder.respond_with_error(e);
                        }
                    }
                });
                Ok(())
            }
            Err(e) => responder.respond_with_error(e),
        },
        "_session/list/unsubscribe" => match registry.index.unsubscribe_list(&params) {
            Ok(result) => responder.respond(result),
            Err(e) => responder.respond_with_error(e),
        },
        "_session/rename" | "_session/archive" | "_session/unarchive" => {
            let archive = method == "_session/archive";
            let session_id = crate::session_index::service::parse_session_id_request(&params)
                .ok()
                .map(|request| request.session_id);
            let loaded = session_id.as_ref().and_then(|id| {
                let session = registry.sessions.get(id)?.clone();
                let cwd = registry
                    .settings_managers
                    .get(id)
                    .map(|manager| manager.get_cwd().to_string_lossy().into_owned())
                    .unwrap_or_default();
                // The archive forgets the session it closed as soon as it is closed,
                // not at the next request: nothing reaches the dead session, and a
                // load of the id since is not forgotten with it.
                let husk = session.command_sender().downgrade();
                let deferred_tx = registry.deferred_tx.clone();
                let sessions_view = registry.sessions_view.clone();
                let forget_id = id.clone();
                Some(LoadedSession {
                    session,
                    session_id: id.clone(),
                    cwd,
                    on_closed: Some(Arc::new(move || {
                        // The view a waiting provider update reads must not keep
                        // it either: upstream deletes the session from its map
                        // before answering, so the update's identity check fails
                        // and it is not recreated. Only the session this archive
                        // closed, never a newer one loaded under the id.
                        sessions_view.send_modify(|view| {
                            if view.get(&forget_id).is_some_and(|(live, _)| {
                                is_registered_instance(live.command_sender(), &husk)
                            }) {
                                view.remove(&forget_id);
                            }
                        });
                        let _ = deferred_tx.send(RegistryMsg::Forget {
                            session_id: forget_id.clone(),
                            husk: husk.clone(),
                        });
                    })),
                })
            });
            let was_loaded = loaded.is_some();
            let husk = loaded
                .as_ref()
                .map(|loaded| loaded.session.command_sender().downgrade());
            let publish = {
                let connection = connection.clone();
                Arc::new(move |value: Value| {
                    if let Ok(msg) = UntypedMessage::new("session/update", value) {
                        let _ = connection.send_notification(msg);
                    }
                }) as Arc<dyn Fn(Value) + Send + Sync>
            };
            let prepared = match method.as_str() {
                "_session/rename" => registry.index.rename(&params, loaded, publish),
                "_session/archive" => registry.index.archive(&params, loaded, publish),
                _ => registry.index.unarchive(&params, loaded, publish),
            };
            let operation = match prepared {
                Ok(operation) => operation,
                Err(e) => return responder.respond_with_error(e),
            };
            // The archive tears the session down only once its retitle succeeded
            // (upstream `connection.ts:274-291`): a failed rename leaves it loaded
            // and usable. The removal waits for the handler's next request, so the
            // error or result is already out first.
            let teardown = archive && was_loaded;
            let deferred_tx = registry.deferred_tx.clone();
            tokio::spawn(async move {
                match operation.await {
                    Ok(result) => {
                        if teardown {
                            if let (Some(id), Some(husk)) = (session_id, husk) {
                                let _ = deferred_tx.send(RegistryMsg::Forget {
                                    session_id: id,
                                    husk,
                                });
                            }
                        }
                        let _ = responder.respond(result);
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(e);
                    }
                }
            });
            Ok(())
        }
        "authenticate" => {
            // The legacy gateway methods store a provider override that every later
            // session reads. The payload is validated here with the same base URL
            // rule as `providers/set`; a call that carries no gateway at all keeps
            // its historical meaning of installing no override.
            let method_id = params.get("methodId").and_then(Value::as_str);
            if !matches!(method_id, Some("gateway" | "gateway-bedrock")) {
                return responder.respond_with_error(
                    Error::internal_error().data(json!({ "details": "Method not implemented." })),
                );
            }
            if let Err(e) = providers::validate_gateway_payload(&params) {
                return responder.respond_with_error(e);
            }
            let base_url =
                providers::gateway_request_to_provider_config(Some(&params)).map(|c| c.base_url);
            registry.gateway_auth = Some(params);
            let auth = registry
                .auth
                .get_or_insert_with(|| spawn_auth(opts, registry.hide_claude_auth, &connection))
                .clone();
            // The gateway holds the credentials from here on: the epoch bump
            // discards any probe that started before this login, and the gateway
            // status replaces whatever the CLI store reported.
            auth.send(AuthCmd::Authenticate { base_url });
            registry.sync_routing();
            let (done, flushed) = oneshot::channel();
            auth.send(AuthCmd::Barrier { reply: done });
            let _ = flushed.await;
            responder.respond(json!({}))
        }
        "providers/list" => {
            let overridden = registry.provider.is_some();
            let config = registry.provider.clone().unwrap_or_else(|| {
                providers::default_provider_config(&opts.env, registry.gateway_auth.as_ref())
            });
            if let Some(logger) = &opts.logger {
                logger.log(&format!(
                    "[providers/list] apiType={} baseUrl={} overridden={overridden}",
                    config.api_type, config.base_url
                ));
            }
            responder.respond(providers::list_providers_result(&config))
        }
        "providers/set" => {
            let config = match providers::parse_set_provider(&params) {
                Ok(config) => config,
                Err(e) => return responder.respond_with_error(e),
            };
            if let Some(logger) = &opts.logger {
                logger.log(&format!(
                    "[providers/set] apiType={} baseUrl={} sessions={}",
                    config.api_type,
                    config.base_url,
                    registry.sessions.len()
                ));
            }
            // The update waits for the running turns and starts every session anew:
            // it runs off this handler so permission answers, `fs/*`/`terminal/*`
            // replies and `session/cancel` keep flowing while it waits
            // (`enqueueProviderUpdate` awaits its promise, not the event loop).
            recreate::spawn_provider_update(
                registry,
                opts,
                &connection,
                Some(config),
                responder.into_wire(),
            );
            Ok(())
        }
        "providers/disable" => {
            // An unknown provider is an idempotent success.
            if params.get("providerId").and_then(Value::as_str) == Some(providers::PROVIDER_ID) {
                if let Some(logger) = &opts.logger {
                    logger.log(&format!(
                        "[providers/disable] sessions={}",
                        registry.sessions.len()
                    ));
                }
                recreate::spawn_provider_update(
                    registry,
                    opts,
                    &connection,
                    None,
                    responder.into_wire(),
                );
                return Ok(());
            }
            responder.respond(json!({}))
        }
        "logout" => {
            // Clear the in-memory gateway credentials supplied via `authenticate`
            // and any provider routing set via `providers/set`; neither touches the
            // on-disk credential store.
            let had_routing_override = registry.resolve_provider().is_some()
                || registry
                    .provider_updates
                    .load(std::sync::atomic::Ordering::SeqCst)
                    > 0;
            registry.gateway_auth = None;
            registry.provider = None;
            registry.sync_routing();
            let auth = registry
                .auth
                .get_or_insert_with(|| spawn_auth(opts, registry.hide_claude_auth, &connection))
                .clone();
            // Any probe already running read the pre-logout world; the bump makes
            // its answer unpublishable so it cannot resurrect the identity being
            // cleared.
            auth.bump_epoch();
            // F5b/B-69: 1M-context entitlement can differ per account/tier, so
            // windows learned under one login must not seed sessions under the next.
            crate::session_model::clear_context_window_cache();
            // `claude auth logout` can take up to a minute, and the recreate that
            // follows waits for the running turns: both run off this handler so the
            // connection keeps answering the turns it waits for. The gate is taken
            // only when the recreate starts (upstream enqueues the provider update
            // in the `finally`), so a session started during the CLI call does not
            // wait a minute for it.
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // The gateway credentials were cleared above, so the recreated
            // sessions run on native routing (and the guard applies again).
            let env = registry.provider_env(opts, cancel.clone(), registry.gateway_auth.is_some());
            let opts = opts.clone();
            let connection = connection.clone();
            let handle = tokio::spawn(async move {
                // The credentials of the native CLI store can only be cleared by the
                // binary itself; `claude auth logout` is non-interactive and idempotent.
                let logout =
                    crate::auth_status::run_logout(opts.claude_path.as_deref(), &opts.extra_env)
                        .await;
                // The cleared routing is baked into the env of every live query, so
                // recreate them the way `providers/disable` does. Done even when the
                // CLI logout failed, because the override is already gone.
                if had_routing_override {
                    let _ =
                        recreate::run_provider_update(env, None, None, None, &opts, &connection)
                            .await;
                }
                if let Err(e) = logout {
                    let data = if e.stderr.is_empty() {
                        json!({})
                    } else {
                        json!({ "stderr": e.stderr })
                    };
                    let _ = responder.respond_with_error(
                        Error::new(-32603, format!("Internal error: {}", e.message)).data(data),
                    );
                    return;
                }
                // Re-read the store rather than assuming "none": an API key from the
                // env or a key helper survives `auth logout`. The read starts after
                // the credentials were cleared; an unreadable store reports "not
                // logged in". The new status is pushed BEFORE the response, like Node.
                let (reply, done) = oneshot::channel();
                auth.send(AuthCmd::Logout {
                    cwd: std::env::current_dir().unwrap_or_default(),
                    reply,
                });
                let _ = done.await;
                let _ = responder.respond(json!({}));
            });
            let _ = registry.recreate_tx.send(RecreateTask { cancel, handle });
            Ok(())
        }
        "session/prompt" => {
            // A provider update in flight recreates every session; the turn was
            // parked until it ended, like upstream's `startTurn`. The permit is
            // held only until the prompt is enqueued on the session.
            let _provider_update = permit;
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                // Node: `throw new Error("Session not found")` -> -32603 with
                // `data:{details}` (a closed or unknown session).
                None => {
                    return responder.respond_with_error(
                        Error::internal_error().data(json!({ "details": "Session not found" })),
                    );
                }
            };
            // B-88: the one re-read of the identity per user prompt, fired here and
            // never awaited: a prompt must not wait on a CLI process, and its result
            // is pushed on change whenever it lands, mid-turn included. It runs
            // BEFORE the guard on purpose: a refused prompt is exactly the one whose
            // retry follows a sign-in in the terminal.
            if let Some(auth) = registry.auth.clone() {
                match registry.auth_contexts.get(&session_id) {
                    Some(context) => {
                        // The helper is read again: the settings may have changed on
                        // disk since the session started (B-87).
                        let mut context = context.clone();
                        context.api_key_helper = registry
                            .api_key_helper(&session_id, context.settings_override.as_ref());
                        auth.use_context_detached(context.clone());
                        auth.probe(context.cwd.clone(), false);
                    }
                    None => auth.probe(std::env::current_dir().unwrap_or_default(), false),
                }
            }
            // V-16: a session flagged for a recreate (a sign-out failed its last
            // turn, or a probe found another kind of identity) is recreated now,
            // unless a turn is still running on its query. The recreation runs the
            // full creation guard; a refusal keeps the closed husk addressable so
            // the client can sign in and retry on the same session.
            match respawn_signed_out_session(registry, opts, &connection, &session_id).await {
                Ok(Some(fresh)) => session = fresh,
                Ok(None) => {}
                Err(refusal) => return responder.respond_with_error(refusal),
            }
            // B-91: the per-turn `--hide-claude-auth` guard, on the account the CLI
            // cached at `initialize`. The account is reported before the decision,
            // so a refused turn still tells the client which account it was for.
            if let Some(refusal) = check_turn_guard(registry, opts, &session_id, &session) {
                return responder.respond_with_error(refusal);
            }
            let first_text = params
                .get("prompt")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let is_local_only = is_local_only_command(first_text);
            let mcp_command = crate::mcp_command::prompt_mcp_command(&params);
            // F24/S-03 (`acp-agent.ts:3809-3857`): the adapter answers
            // `/remote-control [name|off]` itself when Claude Code does not offer
            // its own non-terminal command. The reply bypasses the turn queue, so
            // it is refused while Claude is working.
            let remote_command = match crate::control::prompt_remote_control_command(&params) {
                Some(command) if session.remote_control_available().await => Some(command),
                _ => None,
            };
            // The busy refusal is made by the actor (`Command::RemoteControlPrompt`),
            // atomically with the turn queue and the foreground work.
            let uuid = uuid_v4();
            // M-113/Q-86: a v2 prompt is answered once Claude Code takes it in
            // (`v2/prompt.ts:30-67`), from a spawned task, so the connection
            // loop is free while the turn runs.
            if registry.protocol == crate::v2::ProtocolMode::V2 {
                let v2 = registry.v2_outbound(&connection);
                let content = params
                    .get("_v2Prompt")
                    .cloned()
                    .or_else(|| params.get("prompt").cloned())
                    .unwrap_or(Value::Null);
                if let Some(command) = remote_command {
                    // `/remote-control`: the adapter answers it without a model
                    // turn. NEW-2: the actor decides first, so a refused command
                    // answers `invalidRequest` BEFORE the `user_message`, as
                    // upstream refuses it before `events.inserted`
                    // (`acp-agent.ts:3824-3838`). Only an accepted command sends
                    // the `user_message` and resolves the prompt; the control
                    // call's text is a v1 chunk that v2 drops (no messageId), so
                    // it is only awaited.
                    let session_id = session_id.clone();
                    let reply = responder;
                    let checked = session.remote_control_checked(command);
                    tokio::spawn(async move {
                        match checked.accepted.await {
                            Ok(Ok(())) => {
                                v2.translate(
                                    &session_id,
                                    json!({
                                        "sessionUpdate": "user_message",
                                        "messageId": uuid.clone(),
                                        "content": content,
                                    }),
                                );
                                let _ = reply.respond_after(&v2, Ok(json!({ "messageId": uuid })));
                                let _ = checked.reply.await;
                            }
                            Ok(Err(error)) => {
                                let _ = reply.respond_after(&v2, Err(session_rpc_error(&error)));
                            }
                            // The actor is gone: the session ended
                            // (`RequestError.internalError(undefined,
                            // SESSION_ENDED_MESSAGE)`, `acp-agent.ts:3805-3807`).
                            Err(_) => {
                                let _ = reply.respond_after(
                                    &v2,
                                    Err(session_rpc_error(&SessionError::Closed)),
                                );
                            }
                        }
                    });
                    return Ok(());
                }
                let frame = prompt_to_claude(&params, &session_id, &uuid);
                // G12 (Decision 21): record the `/mcp` command on the actor
                // BEFORE the prompt.
                if let Some(command) = mcp_command {
                    session.send_mcp(uuid.clone(), command);
                }
                let prompt_text = acp_prompt_text(&params);
                // The answer is written by the connection's v2 outbound task, in
                // the position the session reports it, so it cannot be overtaken
                // by the turn's later updates (`v2Prompt`'s `inserted`).
                let (wire, capture) = match responder {
                    Reply::Wire(responder) => (Some(responder), None),
                    Reply::Capture(tx) => (None, Some(tx)),
                };
                if let Some(responder) = wire {
                    v2.register_answer(&uuid, responder);
                }
                // review-T6INT finding 3: a v2 prompt's `_meta` (a file-change
                // report request) goes to the actor right before the prompt, on
                // the same channel, exactly like the v1 arm below.
                session.note_prompt_meta(
                    uuid.clone(),
                    params.get("_meta").cloned().unwrap_or(Value::Null),
                );
                let (inserted, ended) = session.send_prompt_v2(
                    uuid.clone(),
                    frame,
                    is_local_only,
                    prompt_text,
                    content,
                );
                // NEW-3: a v2 prompt runs the v1 prompt task's bookkeeping too:
                // the session-index row starts `running` before the prompt is
                // sent, and its turn end settles the row (`noteSessionState`,
                // `acp-agent.ts:5561-5572`). An `auth_required` failure under
                // `--hide-claude-auth` marks the query for a respawn and closes
                // it (V-16).
                let board = registry.index.board().clone();
                let incarnation = board.turn_started(&session_id);
                let board_session_id = session_id.clone();
                let sign_out_auth = registry.auth.clone().filter(|_| registry.hide_claude_auth);
                tokio::spawn(async move {
                    // NEW-7: a dropped intake reply means the session ended
                    // before it took the prompt in, which v1 answers with
                    // `internalError(undefined, SESSION_ENDED_MESSAGE)`
                    // (`acp-agent.ts:3805-3807`); only an explicit cancel is
                    // `-32800`.
                    let result = match inserted.await {
                        Ok(result) => result,
                        Err(_) => Err(dropped_intake_error()),
                    };
                    match capture {
                        Some(tx) => {
                            let response = match result {
                                Ok(message_id) => Ok(json!({ "messageId": message_id })),
                                Err(error) => Err(crate::v2::prompt::intake_error(&error)),
                            };
                            let _ = tx.send(response);
                        }
                        // The session also reports this at intake; the first
                        // answer wins, so this only matters when it never did.
                        None => v2.answer(&uuid, result),
                    }
                    // The turn's end, as the v1 prompt task reads it from its
                    // reply: a dropped end is a session that ended, which is not
                    // a failed turn (`SessionError::Closed`).
                    let end = ended.await;
                    let failed = matches!(&end, Ok(crate::turn_events::V2TurnEnd::Failed { .. }));
                    if let Some(auth) = &sign_out_auth {
                        let signed_out = matches!(
                            &end,
                            Ok(crate::turn_events::V2TurnEnd::Failed {
                                kind: crate::turn::FailureKind::AuthRequired,
                                ..
                            })
                        );
                        if signed_out {
                            auth.send(AuthCmd::MarkRespawn {
                                session_id: board_session_id.clone(),
                            });
                            // `markSessionForSignOutRespawn` closes the query at
                            // the sign-out (V-16).
                            let session = session.clone();
                            tokio::spawn(async move { session.close().await });
                        }
                    }
                    if let Some(incarnation) = incarnation {
                        board.turn_settled(&board_session_id, incarnation, failed);
                    }
                });
                return Ok(());
            }
            // Enqueue the prompt on the actor synchronously so a later
            // `session/cancel` notification (processed on the same single-task
            // connection) is always ordered *after* it — the queued-sweep
            // ordering the cancel relies on. Only the settlement is awaited on
            // a spawned task so the connection is never blocked while a turn is
            // in flight (Node's `prompt()` returns immediately).
            let mut reply_rx = if let Some(command) = remote_command {
                session.remote_control(command)
            } else {
                let frame = prompt_to_claude(&params, &session_id, &uuid);
                // G12 (Decision 21): record the `/mcp` command on the actor BEFORE
                // the prompt, so the actor knows the turn's replacement is coming.
                // Both sends are on the same unbounded channel, so the actor sees
                // the command first.
                if let Some(command) = mcp_command {
                    session.send_mcp(uuid.clone(), command);
                }
                let prompt_text = acp_prompt_text(&params);
                // AG-3: the prompt's `_meta` (a file-change report request) goes
                // to the actor right before the prompt, on the same channel.
                session.note_prompt_meta(
                    uuid.clone(),
                    params.get("_meta").cloned().unwrap_or(Value::Null),
                );
                session.send_prompt(uuid, frame, is_local_only, prompt_text)
            };
            // What the session index shows of a session this connection runs: the
            // turn is running until its reply settles (`noteSessionState`).
            let board = registry.index.board().clone();
            let incarnation = board.turn_started(&session_id);
            let board_session_id = session_id.clone();
            // F2c / M-52: Node's `runPromptWithCancellation`
            // (`acp-agent.ts:13528-13552`) listens for `$/cancel_request` on
            // this prompt's request signal and routes it to the same
            // `agent.cancel` path as `session/cancel`. The crate already
            // records the notification on the request's cancellation marker,
            // so watch that marker and cancel the owning session; the prompt
            // then settles `cancelled` (never a `request_cancelled` error).
            let Some(cancellation) = responder.cancellation() else {
                return responder.respond_with_error(Error::internal_error());
            };
            let cancel_session = session.clone();
            let connection = connection.clone();
            let sign_out_auth = registry.auth.clone().filter(|_| registry.hide_claude_auth);
            tokio::spawn(async move {
                let first = tokio::select! {
                    reply = &mut reply_rx => Some(reply),
                    () = cancellation.cancelled() => None,
                };
                let reply = match first {
                    Some(reply) => reply,
                    None => {
                        let _ = cancel_session.cancel().await;
                        (&mut reply_rx).await
                    }
                }
                .unwrap_or_else(|_| Err(SessionError::Closed.into()));
                // The turn failed with an error (not a cancel, not a session that
                // ended).
                let failed = matches!(&reply, Err(failure) if !matches!(failure.error, SessionError::Closed));
                // V-16: a turn that failed with `auth_required` means a sign-out
                // (under `--hide-claude-auth`): the account cached at `initialize`
                // is wrong, so the next prompt recreates the query.
                if let Some(auth) = &sign_out_auth {
                    let signed_out = matches!(&reply, Err(failure) if matches!(
                        failure.error,
                        SessionError::PromptFailed { kind: crate::turn::FailureKind::AuthRequired, .. }
                    ));
                    if signed_out {
                        auth.send(AuthCmd::MarkRespawn {
                            session_id: board_session_id.clone(),
                        });
                        // `markSessionForSignOutRespawn` closes the query at the
                        // sign-out, so the stale CLI cannot keep accepting input;
                        // the recreate at the next turn is idempotent with it.
                        let session = cancel_session.clone();
                        tokio::spawn(async move { session.close().await });
                    }
                }
                if let Some(incarnation) = incarnation {
                    board.turn_settled(&board_session_id, incarnation, failed);
                }
                match reply {
                    Ok(reply) => {
                        // Forward this turn's streaming updates (e.g. a
                        // `FinalText` chunk) before the response — the actor
                        // hands them back with the reply, so the chunk lands
                        // deterministically ahead of it as upstream does.
                        for notif in &reply.updates {
                            let _ = connection.send_notification(notif.clone());
                        }
                        let stop_reason = match reply.stop_reason.as_str() {
                            "max_tokens" => StopReason::MaxTokens,
                            "refusal" => StopReason::Refusal,
                            "max_turn_requests" => StopReason::MaxTurnRequests,
                            "cancelled" => StopReason::Cancelled,
                            _ => StopReason::EndTurn,
                        };
                        let mut result = json!({ "stopReason": stop_reason });
                        if let Some(usage) = reply.usage {
                            if let Some(map) = result.as_object_mut() {
                                map.insert(
                                    "usage".to_string(),
                                    json!({
                                        "inputTokens": usage.input_tokens,
                                        "outputTokens": usage.output_tokens,
                                        "cachedReadTokens": usage.cached_read_tokens,
                                        "cachedWriteTokens": usage.cached_write_tokens,
                                        "totalTokens": usage.total_tokens,
                                    }),
                                );
                            }
                        }
                        // 2.12 (E-6): `result._meta.quota` on the same settles
                        // that reported usage.
                        if let Some(quota) = reply.quota {
                            if let Some(map) = result.as_object_mut() {
                                map.insert("_meta".to_string(), quota);
                            }
                        }
                        // Phase 9: await the flush barrier so every session
                        // update this turn emitted (e.g. a `tool_call`) is
                        // written before the response.
                        if let Some(flush_rx) = reply.flush_rx {
                            let _ = flush_rx.await;
                        }
                        let _ = responder.respond(result);
                        if let Some(ack) = reply.ack {
                            let _ = ack.send(());
                        }
                    }
                    Err(failure) => {
                        let _ = responder.respond_with_error(session_rpc_error(&failure.error));
                        // Release the forwarder's ordering wait for this response.
                        if let Some(ack) = failure.ack {
                            let _ = ack.send(());
                        }
                    }
                }
            });
            Ok(())
        }
        "_session/steering" => {
            // R32 / B1 row. A steered message is injected into the running turn
            // at `now` priority via the control writer, or the host-owned
            // `promptRequired` fallback is reported when idle.
            //
            // `parseSteerRequest` (`acp-agent.ts:708-738`) validates BEFORE any
            // session lookup, and every rejection is `Invalid params: <text>`
            // with no `data`.
            let steer = match parse_steer_request(&params) {
                Ok(steer) => steer,
                Err(e) => return responder.respond_with_error(e),
            };
            // No wait for a provider update in flight: a steer is what a client
            // sends to a running turn, and upstream's `steer()` never awaits
            // `providerUpdate` (`acp-agent.ts:4315-4375`).
            let session_id = steer.session_id;
            let session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                None => {
                    return responder
                        .respond_with_error(Error::internal_error().data("Session not found"));
                }
            };
            // V-16: a sign-out (or a probe's identity change) recreates the query
            // before the steer decides, as upstream's `respawnSignedOutSession`
            // does.
            let session =
                match respawn_signed_out_session(registry, opts, &connection, &session_id).await {
                    Ok(Some(fresh)) => fresh,
                    Ok(None) => session,
                    Err(refusal) => return responder.respond_with_error(refusal),
                };
            // A query a sign-out closed answers "session ended", before the
            // outcome is decided (`session.queryClosed`).
            if session.command_sender().is_closed() {
                return responder.respond_with_error(session_rpc_error(&SessionError::Closed));
            }
            // "A turn is running" = the session has an unsettled turn, which covers
            // a steered turn and one submitted but not yet echoed (`turnQueue`).
            let idle = !recreate::session_has_unsettled_turn(
                &session,
                registry.index.board(),
                &session_id,
            )
            .await;
            // A steer that would start a new turn runs the `--hide-claude-auth`
            // guard here, so the refusal reaches the client instead of a detached
            // prompt (`acp-agent.ts:4343-4349`).
            if idle && !steer.prompt_required {
                if let Some(refusal) = check_turn_guard(registry, opts, &session_id, &session) {
                    return responder.respond_with_error(refusal);
                }
            }
            let first_text = steer
                .prompt
                .first()
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let is_local_only = is_local_only_command(first_text);
            let uuid = uuid_v4();
            let frame = prompt_to_claude(&params, &session_id, &uuid);
            let reply_rx = session.steer(uuid, frame, is_local_only, steer.prompt_required);
            tokio::spawn(async move {
                // The actor is gone: the stream ended (a husk session), so the
                // steer answers the same "session ended" error a prompt does
                // (`acp-agent.ts:4323-4325`).
                match reply_rx.await {
                    Ok(outcome) => {
                        let _ = responder.respond(outcome);
                    }
                    Err(_) => {
                        let _ =
                            responder.respond_with_error(session_rpc_error(&SessionError::Closed));
                    }
                }
            });
            Ok(())
        }
        "_session/async_task/stop" => {
            // G1: stop one background task without cancelling the turn.
            let (session_id, task_id) = match parse_async_task_stop_params(&params) {
                Ok(parsed) => parsed,
                Err(e) => return responder.respond_with_error(e),
            };
            let Some(session) = registry.sessions.get(&session_id).cloned() else {
                return responder.respond(json!({ "stopped": false }));
            };
            // The CLI round-trip is awaited off the connection loop (INV-27).
            tokio::spawn(async move {
                match session.stop_async_task(task_id).await {
                    Ok(stopped) => {
                        let _ = responder.respond(json!({ "stopped": stopped }));
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(session_rpc_error(&e));
                    }
                }
            });
            Ok(())
        }
        "_session/goal" => {
            // G8 (rust-air-gaps 3.1, Decision 16): the AIR goal control method.
            // `goal()` (`acp-agent.ts:4185-4196`) turns the request into a
            // `/goal …` prompt, steers it with `idleBehavior:"promptRequired"`,
            // and prompts it when the session is idle; the response is `{}`
            // once that turn settles (or at once when the steer injected).
            let request = match crate::goal::parse_goal_request(&params) {
                Ok(request) => request,
                Err(e) => return responder.respond_with_error(e),
            };
            // No wait here: `goal` steers first, and a steer never awaits a provider
            // update in flight; the spawned task below waits for it only where the
            // goal starts its own turn (`goal` → `steer` → `prompt` → `startTurn`).
            let Some(session) = registry.sessions.get(&request.session_id).cloned() else {
                // Node's `steer` throws `new Error("Session not found")`; the
                // SDK maps it to `internalError` with `data:{details}`.
                return responder.respond_with_error(
                    Error::internal_error().data(json!({ "details": "Session not found" })),
                );
            };
            // `goal()` runs `steer()` first, which respawns a signed-out session
            // (`respawnSignedOutSession`).
            let session =
                match respawn_signed_out_session(registry, opts, &connection, &request.session_id)
                    .await
                {
                    Ok(Some(fresh)) => fresh,
                    Ok(None) => session,
                    Err(refusal) => return responder.respond_with_error(refusal),
                };
            // `goal()` goes through `prompt()` → `startTurn()` only when the session
            // is idle, and that runs the per-turn guard before it enqueues the turn.
            let idle = !recreate::session_has_unsettled_turn(
                &session,
                registry.index.board(),
                &request.session_id,
            )
            .await;
            if idle {
                if let Some(refusal) =
                    check_turn_guard(registry, opts, &request.session_id, &session)
                {
                    return responder.respond_with_error(refusal);
                }
            }
            let command = request.command_text();
            let prompt_params = json!({
                "sessionId": request.session_id,
                "prompt": [ { "type": "text", "text": command } ],
            });
            let steer_uuid = uuid_v4();
            let frame = prompt_to_claude(&prompt_params, &request.session_id, &steer_uuid);
            let is_local_only = is_local_only_command(&command);
            let steer_rx = session.steer(steer_uuid, frame.clone(), is_local_only, true);
            // The goal's own turn is a running turn: a provider switch must wait
            // for it instead of closing the session under it (`turnQueue`).
            let board = registry.index.board().clone();
            let session_id = request.session_id;
            let gate = registry.provider_gate.clone();
            let sessions_view = registry.sessions_view.subscribe();
            // The steer reply and the prompt's settlement are awaited off the
            // connection loop (INV-27); the response never overtakes the
            // goal's updates because the actor publishes them before either
            // reply resolves.
            tokio::spawn(async move {
                let outcome = match steer_rx.await {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        let _ = responder.respond_with_error(Error::internal_error());
                        return;
                    }
                };
                if outcome.get("outcome").and_then(Value::as_str) == Some("promptRequired") {
                    // Node's `prompt()` generates its own prompt uuid.
                    let prompt_uuid = uuid_v4();
                    // `startTurn` waits for a provider update in flight. The turn
                    // is counted on the board only once the gate is held, or the
                    // update would wait for a turn that waits for the update.
                    let permit = gate.acquire_owned().await.ok();
                    // The update replaced the session: take the one it started.
                    let session = sessions_view
                        .borrow()
                        .get(&session_id)
                        .map(|(live, _)| live.clone())
                        .unwrap_or(session);
                    let incarnation = board.turn_started(&session_id);
                    let reply_rx = session.send_prompt(prompt_uuid, frame, is_local_only, command);
                    drop(permit);
                    let reply = reply_rx
                        .await
                        .unwrap_or_else(|_| Err(SessionError::Closed.into()));
                    let failed = matches!(
                        &reply,
                        Err(failure) if !matches!(failure.error, SessionError::Closed)
                    );
                    if let Some(incarnation) = incarnation {
                        board.turn_settled(&session_id, incarnation, failed);
                    }
                    match reply {
                        Ok(reply) => {
                            if let Some(flush_rx) = reply.flush_rx {
                                let _ = flush_rx.await;
                            }
                            // This steer response is not a prompt response: release
                            // the batch barrier now.
                            if let Some(ack) = reply.ack {
                                let _ = ack.send(());
                            }
                        }
                        Err(failure) => {
                            let _ = responder.respond_with_error(session_rpc_error(&failure.error));
                            if let Some(ack) = failure.ack {
                                let _ = ack.send(());
                            }
                            return;
                        }
                    }
                }
                let _ = responder.respond(json!({}));
            });
            Ok(())
        }
        _ => {
            // (`@agentclientprotocol/sdk/dist/core/jsonrpc.js` `methodNotFound`):
            // `-"Method not found": <method>"` with `data` `{method}`.
            responder.respond_with_error(
                Error::new(-32601, format!("\"Method not found\": {method}"))
                    .data(json!({ "method": method })),
            )
        }
    }
}

/// A `--session-id`/`--resume` argv assertion helper (unit tests).
pub fn session_id_from_argv(argv: &[String]) -> Option<&str> {
    argv.iter().find_map(|a| a.strip_prefix("--session-id="))
}

/// A `--resume` argv assertion helper (unit tests).
pub fn resume_from_argv(argv: &[String]) -> Option<&str> {
    argv.iter().find_map(|a| a.strip_prefix("--resume="))
}

mod providers;
mod recreate;

#[cfg(test)]
mod tests_t1ha;
#[cfg(test)]
mod tests_t2b;
#[cfg(test)]
mod tests_t3b;
#[cfg(test)]
mod tests_t3fb;
#[cfg(test)]
mod tests_t3fd;
#[cfg(test)]
mod tests_t4b;

#[cfg(test)]
mod tests_t5b;

#[cfg(test)]
mod tests_t2gb;

#[cfg(test)]
mod tests {
    use super::*;

    /// B-117 — upstream "getOrCreateSession param change detection": the params
    /// that must NOT rebuild a live session (mcpServers order, reordered/duplicate
    /// skills, option key order, per-call resume controls, creation-only options)
    /// and those that MUST (cwd, mcpServers, skills, any rebuilding option,
    /// additionalDirectories).
    #[test]
    fn b117_session_fingerprint_matches_upstream_param_change_detection() {
        let fp = |params: Value| compute_session_fingerprint(&params);
        let base = json!({"cwd": "/project", "mcpServers": []});

        // "returns cached session when params are unchanged"
        assert_eq!(fp(base.clone()), fp(base.clone()));
        // "tears down existing session when cwd changes"
        assert_ne!(
            fp(base.clone()),
            fp(json!({"cwd": "/new", "mcpServers": []}))
        );
        // "tears down existing session when mcpServers change"
        let with_server = json!({"cwd": "/project", "mcpServers": [
            {"name": "new-server", "command": "node", "args": ["server.js"], "env": []}]});
        assert_ne!(fp(base.clone()), fp(with_server));
        // "treats mcpServers in different order as unchanged"
        let a = json!({"name": "a-server", "command": "node", "args": ["a.js"], "env": []});
        let b = json!({"name": "b-server", "command": "node", "args": ["b.js"], "env": []});
        assert_eq!(
            fp(json!({"cwd": "/project", "mcpServers": [b.clone(), a.clone()]})),
            fp(json!({"cwd": "/project", "mcpServers": [a, b]})),
        );
        // "tears down the existing session when additionalDirectories change"
        assert_ne!(
            fp(base.clone()),
            fp(json!({"cwd": "/project", "mcpServers": [], "additionalDirectories": ["/other"]}))
        );
        // "tears down the existing session when skills change from $label"
        let skills = |value: Option<Value>| {
            let options = match value {
                Some(v) => json!({"skills": v}),
                None => json!({}),
            };
            json!({"cwd": "/project", "mcpServers": [],
                   "_meta": {"claudeCode": {"options": options}}})
        };
        for (previous, next) in [
            (None, Some(json!("all"))),
            (Some(json!("all")), Some(json!([]))),
            (Some(json!([])), None),
            (Some(json!(["pdf"])), Some(json!(["docx"]))),
        ] {
            assert_ne!(
                fp(skills(previous.clone())),
                fp(skills(next.clone())),
                "{previous:?} -> {next:?}"
            );
        }
        // "treats reordered and duplicate skills as unchanged"
        assert_eq!(
            fp(skills(Some(json!(["pdf", "docx"])))),
            fp(skills(Some(json!(["pdf", "docx", "pdf"])))),
        );
        // "tears down the existing session when any other Claude option changes"
        let env = |value: &str| {
            json!({"cwd": "/project", "mcpServers": [],
                   "_meta": {"claudeCode": {"options": {"env": {"CUSTOM_ENV": value}}}}})
        };
        assert_ne!(fp(env("original")), fp(env("changed")));
        // "ignores option key order, per-call resume controls and creation-only options"
        let before = json!({"cwd": "/project", "mcpServers": [], "_meta": {"claudeCode": {
            "options": {"maxTurns": 10, "env": {"A": "1", "B": "2"}}}}});
        let after = json!({"cwd": "/project", "mcpServers": [], "_meta": {"claudeCode": {
            "options": {"env": {"B": "2", "A": "1"}, "maxTurns": 10, "resume": "s1",
                "sessionId": "s1", "forkSession": false, "resumeSessionAt": "msg-1",
                "resumeDropsTurn": "msg-1", "title": "Renamed"}}}});
        assert_eq!(fp(before), fp(after));
        // The session-defining _meta fields count.
        assert_ne!(
            fp(base.clone()),
            fp(json!({"cwd": "/project", "mcpServers": [],
                      "_meta": {"systemPrompt": "be brief"}}))
        );
        assert_ne!(
            fp(base),
            fp(json!({"cwd": "/project", "mcpServers": [],
                      "_meta": {"claudeCode": {"emitRawSDKMessages": true}}}))
        );
    }

    /// B-117 — upstream "starts a fresh query under the same id for a live session
    /// that was never prompted" / "still fails a resume of an unknown session that
    /// has no stored conversation" / "does not start a fresh query when the resume
    /// of a live session fails otherwise".
    #[test]
    fn b117_recreate_falls_back_to_a_fresh_query_only_for_a_live_unpersisted_session() {
        let not_found: Result<(), SessionError> = Err(SessionError::Start(
            "No conversation found with session ID: s1".to_string(),
        ));
        assert!(recreate_falls_back_to_fresh(true, false, &not_found));
        assert!(
            !recreate_falls_back_to_fresh(false, false, &not_found),
            "unknown session"
        );
        assert!(
            !recreate_falls_back_to_fresh(true, true, &not_found),
            "already fresh"
        );
        let other: Result<(), SessionError> =
            Err(SessionError::Start("CLI failed to start".to_string()));
        assert!(!recreate_falls_back_to_fresh(true, false, &other));
        let ok: Result<(), SessionError> = Ok(());
        assert!(!recreate_falls_back_to_fresh(true, false, &ok));
    }

    /// V-10 — a request to a session whose stream ended (a husk) answers the same
    /// "session ended" error a prompt does; any other failure keeps its text.
    #[test]
    fn v10_set_request_on_an_ended_session_answers_session_ended() {
        let ended = set_request_error(crate::session::SESSION_GONE.to_string());
        let prompt_on_ended = session_rpc_error(&SessionError::Closed);
        assert_eq!(
            serde_json::to_value(&ended).unwrap(),
            serde_json::to_value(&prompt_on_ended).unwrap()
        );
        assert!(ended.message.contains(crate::turn::SESSION_ENDED_MESSAGE));
        let other = set_request_error("Invalid Mode".to_string());
        assert_eq!(other.data, Some(json!({"details": "Invalid Mode"})));
    }

    #[test]
    fn uuid_is_v4_shaped() {
        let u = uuid_v4();
        let bytes: Vec<u8> = u
            .chars()
            .filter(|c| *c != '-')
            .collect::<String>()
            .into_bytes();
        assert_eq!(bytes.len(), 32);
        assert_eq!(u.as_bytes()[14], b'4');
        assert_eq!(u.as_bytes()[19] as char, '8', "variant bits (8/9/a/b)");
    }

    /// rust-air-gaps 1.5 — `_session/async_task/stop` param validation: the
    /// Node's `-32602` messages, no `data`, values trimmed.
    #[test]
    fn inv_49_stop_params_errors() {
        let ok =
            parse_async_task_stop_params(&json!({"sessionId": " s1 ", "asyncTaskId": "\tbg1\n"}));
        assert_eq!(ok.unwrap(), ("s1".to_string(), "bg1".to_string()));
        for (params, message) in [
            (
                json!([]),
                "Invalid params: async task stop params must be an object",
            ),
            (
                json!(null),
                "Invalid params: async task stop params must be an object",
            ),
            (
                json!("x"),
                "Invalid params: async task stop params must be an object",
            ),
            (
                json!({"asyncTaskId": "bg1"}),
                "Invalid params: async task stop params require a non-empty sessionId",
            ),
            (
                json!({"sessionId": "  ", "asyncTaskId": "bg1"}),
                "Invalid params: async task stop params require a non-empty sessionId",
            ),
            (
                json!({"sessionId": 7, "asyncTaskId": "bg1"}),
                "Invalid params: async task stop params require a non-empty sessionId",
            ),
            (
                json!({"sessionId": "s1"}),
                "Invalid params: async task stop params require a non-empty asyncTaskId",
            ),
            (
                json!({"sessionId": "s1", "asyncTaskId": ""}),
                "Invalid params: async task stop params require a non-empty asyncTaskId",
            ),
        ] {
            let err = parse_async_task_stop_params(&params).unwrap_err();
            assert_eq!(i32::from(err.code), -32602, "{params}");
            assert_eq!(err.message, message, "{params}");
            assert!(err.data.is_none(), "no data: {params}");
        }
    }

    #[test]
    fn is_local_only_command_detects_context() {
        assert!(is_local_only_command("/context"));
        assert!(is_local_only_command("/context with args"));
        assert!(!is_local_only_command("hello"));
        assert!(!is_local_only_command("/clear")); // not local-only upstream
    }

    /// Review G14 finding 3 — the title context is built from the plain ACP
    /// `type:"text"` chunks (`session-titles.ts` `onPrompt`), never from the
    /// converted SDK frame, whose blocks also carry embedded file contents
    /// (`<context ref=…>`) and resource links (`[@name](uri)`).
    #[test]
    fn inv_80_title_context_from_acp_text_only() {
        let params = json!({"sessionId":"s1","prompt":[
            {"type":"text","text":"explain this file"},
            {"type":"resource","resource":{"uri":"file:///w/a.rs","text":"fn main() {}"}},
            {"type":"resource_link","uri":"file:///w/b.rs","name":"b.rs"}
        ]});
        let text = acp_prompt_text(&params);
        assert_eq!(text, "explain this file");

        let mut titles = crate::session_titles::SessionTitles::new();
        titles.on_prompt(&text);
        let context = titles.context.as_deref().expect("context collected");
        assert_eq!(context, "\nexplain this file\n");
        assert!(!context.contains("<context ref"), "no embedded file text");
        assert!(!context.contains("[@"), "no resource link");

        // The converted frame is exactly the source that must NOT be used.
        let frame = prompt_to_claude(&params, "s1", "u");
        let frame_text = frame["message"]["content"].to_string();
        assert!(frame_text.contains("<context ref=\\\"file:///w/a.rs\\\">"));
        assert!(frame_text.contains("[@b.rs](file:///w/b.rs)"));
    }

    #[test]
    fn prompt_to_claude_maps_blocks() {
        // text
        let params = json!({"sessionId":"s1","prompt":[{"type":"text","text":"hi"}]});
        let frame = prompt_to_claude(&params, "s1", "uuid-1");
        assert_eq!(frame["message"]["role"], "user");
        assert_eq!(frame["uuid"], "uuid-1");
        assert_eq!(frame["message"]["content"][0]["text"], "hi");
        assert_eq!(frame["origin"]["kind"], "human");

        // image
        let params = json!({"prompt":[{"type":"image","data":"QUJD","mimeType":"image/png"}]});
        let frame = prompt_to_claude(&params, "s1", "u");
        assert_eq!(frame["message"]["content"][0]["source"]["type"], "base64");
        assert_eq!(frame["message"]["content"][0]["source"]["data"], "QUJD");

        // resource_link
        let params = json!({"prompt":[{"type":"resource_link","uri":"file:///a.txt"}]});
        let frame = prompt_to_claude(&params, "s1", "u");
        assert!(frame["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[@a.txt]"));

        // resource with text -> content + context
        let params = json!({"prompt":[{"type":"resource","resource":{"uri":"file:///b.txt","text":"body"}}]});
        let frame = prompt_to_claude(&params, "s1", "u");
        let content = frame["message"]["content"].as_array().unwrap();
        assert!(content
            .iter()
            .any(|c| c["text"].as_str().unwrap().contains("[@b.txt]")));
        assert!(content
            .iter()
            .any(|c| c["text"].as_str().unwrap().contains("body")));
    }

    #[test]
    fn session_spawn_options_map_model_permission_session_id() {
        let serve = ServeOptions {
            claude_path: Some(PathBuf::from("/fake")),
            ..Default::default()
        };
        let spawn = session_spawn_options(
            &serve,
            None,
            "uu-1234".to_string(),
            crate::sdk_options::ClaudeOptions {
                model: Some("claude-sonnet-4-5".to_string()),
                ..Default::default()
            },
            "acceptEdits".to_string(),
            false,
            true,
        );
        let argv = crate::process::build_argv(&spawn);
        assert_eq!(session_id_from_argv(&argv), Some("uu-1234"));
        assert!(
            argv.iter().any(|a| a == "--model=claude-sonnet-4-5"),
            "model -> --model=claude-sonnet-4-5: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "--permission-mode=acceptEdits"),
            "permission mode -> --permission-mode=acceptEdits: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "--permission-prompt-tool=stdio"),
            "the adapter always passes --permission-prompt-tool=stdio (E-13): {argv:?}"
        );
    }

    #[test]
    fn resume_maps_to_resume_flag() {
        let serve = ServeOptions::default();
        let mut spawn = session_spawn_options(
            &serve,
            None,
            "sess-id".to_string(),
            crate::sdk_options::ClaudeOptions::default(),
            "default".to_string(),
            false,
            true,
        );
        spawn.resume = Some("sess-id".to_string());
        spawn.session_id = None;
        let argv = crate::process::build_argv(&spawn);
        assert_eq!(resume_from_argv(&argv), Some("sess-id"));
        assert!(session_id_from_argv(&argv).is_none());
    }

    /// Phase 14 — `validate_cwd` (issue #749, `acp-agent.js:4681-4695`): a
    /// relative path, a nonexistent path, and a non-directory all yield
    /// `invalidParams` (-32602) with the adapter's message and `data:{cwd}`.
    #[test]
    fn inv_validate_cwd_rejects_relative_and_missing() {
        let err = validate_cwd(Some("relative/dir")).expect_err("relative must be rejected");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], -32602);
        // F2c / M-01: `RequestError.invalidParams` prefixes the message.
        assert_eq!(
            v["message"],
            "Invalid params: `cwd` must be an absolute path, but received: relative/dir"
        );
        assert_eq!(v["data"]["cwd"], "relative/dir");

        let missing =
            std::env::temp_dir().join(format!("definitely-missing-cwd-{}", std::process::id()));
        let missing_str = missing.to_string_lossy().into_owned();
        let err = validate_cwd(Some(&missing_str)).expect_err("missing must be rejected");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], -32602);
        assert!(v["message"]
            .as_str()
            .unwrap()
            .starts_with("Invalid params: "));
        assert!(v["message"].as_str().unwrap().contains("does not exist"));
        assert_eq!(v["data"]["cwd"], missing_str);

        // A non-directory (a file) must be rejected as "not a directory".
        let file = std::env::temp_dir().join(format!("cwd-file-{}.txt", std::process::id()));
        std::fs::write(&file, "x").expect("write temp file");
        let file_str = file.to_string_lossy().into_owned();
        let err = validate_cwd(Some(&file_str)).expect_err("a file is not a directory");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], -32602);
        assert!(v["message"]
            .as_str()
            .unwrap()
            .starts_with("Invalid params: "));
        assert!(v["message"].as_str().unwrap().contains("not a directory"));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn inv_validate_cwd_accepts_absent_and_existing_dir() {
        // Absent cwd is accepted (the crate falls back to default_cwd/process cwd).
        assert!(validate_cwd(None).is_ok());
        // An existing absolute directory is accepted.
        let dir = std::env::current_dir().unwrap();
        let dir_str = dir.to_string_lossy().into_owned();
        assert!(validate_cwd(Some(&dir_str)).is_ok());
    }

    /// carries `data.errorKind:"no_result"` on the wire, regardless of whether
    /// the assistant frame carried an `error` (acp-agent.js:2154 attaches
    /// `errorKindData("no_result")` unconditionally). Phase-13 audit found the
    /// old `NoResult if assistant_had_error` guard dropped it.
    #[test]
    fn no_result_error_always_carries_error_kind() {
        use crate::turn::FailureKind;
        let err = SessionError::PromptFailed {
            kind: FailureKind::NoResult,
            message: crate::turn::TURN_NO_RESULT_MESSAGE.to_string(),
            assistant_had_error: false,
        };
        let (_msg, data) = session_error_to_rpc(&err);
        let kind = data
            .as_ref()
            .and_then(|d| d.get("errorKind"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            kind,
            Some("no_result"),
            "idle-without-result must attach errorKind no_result even with no assistant error: {data:?}"
        );
    }

    /// F3a (f) — a child that exits after an error `result` rejects with the
    /// SDK's replacement text; the ACP router wraps a plain `Error` as
    /// `Internal error` + `data.details` (jsonrpc `errorToResult`).
    #[test]
    fn child_exit_error_result_wire_shape() {
        let err = SessionError::ChildErrorResult {
            text: "model overloaded".to_string(),
        };
        let (message, data) = session_error_to_rpc(&err);
        assert_eq!(message, "Internal error");
        assert_eq!(
            data,
            Some(json!({"details": "Claude Code returned an error result: model overloaded"}))
        );
    }

    #[test]
    fn inv_54_auth_required_code() {
        use crate::turn::FailureKind;
        let err = SessionError::PromptFailed {
            kind: FailureKind::AuthRequired,
            message: "Authentication required. Please run /login.".to_string(),
            assistant_had_error: false,
        };
        let rpc = session_rpc_error(&err);
        let val = serde_json::to_value(&rpc).unwrap();
        assert_eq!(val["code"], -32000);
        assert_eq!(val["message"], "Authentication required");
        assert!(val.get("data").is_none());
    }

    /// 12.T1 — `_session/steering` outcome decisions (R32 / B1):
    /// idle + `promptRequired` → `{outcome:"promptRequired",
    /// reason:"noRunningTurn"}`; `idleBehavior:"x"` → `invalidParams`.
    #[test]
    fn inv_steering_idle_prompt_required_and_invalid_behavior() {
        // Idle + promptRequired opt-in.
        let outcome = steer_outcome(false, Some("promptRequired")).expect("ok");
        assert_eq!(
            outcome,
            json!({ "outcome": "promptRequired", "reason": "noRunningTurn" }),
            "idle + promptRequired must yield the promptRequired outcome"
        );

        // Bad idleBehavior → invalidParams (code -32602).
        let err = steer_outcome(false, Some("x")).expect_err("must reject bad idleBehavior");
        assert_eq!(
            serde_json::to_value(&err)
                .ok()
                .and_then(|v| v.get("code").cloned())
                .and_then(|c| c.as_i64()),
            Some(-32602),
            "bad idleBehavior must be invalidParams"
        );
        let err = steer_outcome(true, Some("anything-else")).expect_err("must reject");
        assert_eq!(
            serde_json::to_value(&err)
                .ok()
                .and_then(|v| v.get("code").cloned())
                .and_then(|c| c.as_i64()),
            Some(-32602),
            "a non-promptRequired idleBehavior must be invalidParams even with a turn in flight"
        );

        // Absent idleBehavior (the legacy default) is accepted.
        assert!(
            steer_outcome(false, None).is_ok(),
            "absent idleBehavior is valid"
        );
        assert!(
            steer_outcome(true, None).is_ok(),
            "absent idleBehavior is valid"
        );
    }

    /// 12.T1 companion — a turn in flight injects (returns `injected`); the
    /// idle legacy default starts a new turn. Guards the non-error arms of
    /// `steer_outcome` so the R32 table is fully covered.
    #[test]
    fn inv_steering_in_flight_and_idle_default() {
        let injected = steer_outcome(true, None).expect("ok");
        assert_eq!(injected, json!({ "outcome": "injected" }));
        let injected = steer_outcome(true, Some("promptRequired")).expect("ok");
        assert_eq!(injected, json!({ "outcome": "injected" }));

        let started = steer_outcome(false, None).expect("ok");
        assert_eq!(started, json!({ "outcome": "startedNewTurn" }));
    }

    /// F2c / M-20 — `parseSteerRequest` (`acp-agent.ts:708-738`): every
    /// rejection is `Invalid params: <text>` with NO `data`, and a
    /// present-but-non-string `idleBehavior` is rejected instead of ignored.
    #[test]
    fn inv_52_steer_request_error_shape() {
        let ok = parse_steer_request(&json!({
            "sessionId": "s1",
            "prompt": [{"type": "text", "text": "hi"}],
            "_meta": {"steering": {"idleBehavior": "promptRequired"}}
        }))
        .expect("valid steer");
        assert_eq!(ok.session_id, "s1");
        assert_eq!(ok.prompt.len(), 1);
        assert!(ok.prompt_required);

        let default = parse_steer_request(&json!({
            "sessionId": "s1",
            "prompt": [{"type": "text", "text": "hi"}]
        }))
        .expect("valid steer");
        assert!(!default.prompt_required);
        // `_meta`/`steering` that are not objects (and arrays) leave the
        // default: JS reads `idleBehavior` as undefined.
        for meta in [
            json!(null),
            json!({"steering": null}),
            json!({"steering": []}),
        ] {
            let params = json!({
                "sessionId": "s1",
                "prompt": [{"type": "text", "text": "hi"}],
                "_meta": meta
            });
            let steer = parse_steer_request(&params).expect("default steer");
            assert!(!steer.prompt_required, "{params}");
        }

        let steer = |params: &Value| parse_steer_request(params).expect_err("must reject");
        for (params, text) in [
            (json!(null), "steer params must be an object"),
            (json!("x"), "steer params must be an object"),
            (json!(5), "steer params must be an object"),
            (json!(true), "steer params must be an object"),
            // A JS array is an object, so it falls through to `sessionId`.
            (json!([]), "steer params require a non-empty sessionId"),
            (
                json!({"prompt": [{"type": "text", "text": "hi"}]}),
                "steer params require a non-empty sessionId",
            ),
            (
                json!({"sessionId": 7, "prompt": []}),
                "steer params require a non-empty sessionId",
            ),
            (
                json!({"sessionId": "", "prompt": []}),
                "steer params require a non-empty sessionId",
            ),
            (
                json!({"sessionId": "s1"}),
                "steer params require a non-empty prompt array",
            ),
            (
                json!({"sessionId": "s1", "prompt": []}),
                "steer params require a non-empty prompt array",
            ),
            (
                json!({"sessionId": "s1", "prompt": "text"}),
                "steer params require a non-empty prompt array",
            ),
            (
                json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}],
                       "_meta": {"steering": {"idleBehavior": "x"}}}),
                "unsupported steering idleBehavior",
            ),
            (
                json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}],
                       "_meta": {"steering": {"idleBehavior": 7}}}),
                "unsupported steering idleBehavior",
            ),
            (
                json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}],
                       "_meta": {"steering": {"idleBehavior": null}}}),
                "unsupported steering idleBehavior",
            ),
            (
                json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}],
                       "_meta": {"steering": {"idleBehavior": {"a": 1}}}}),
                "unsupported steering idleBehavior",
            ),
        ] {
            let err = steer(&params);
            let v = serde_json::to_value(&err).expect("serializes");
            assert_eq!(v["code"], -32602, "{params}");
            assert_eq!(v["message"], format!("Invalid params: {text}"), "{params}");
            assert!(v.get("data").is_none(), "no data: {params} -> {v}");
        }

        // The `steer_outcome` seam keeps the same wire shape.
        let err = steer_outcome(false, Some("x")).expect_err("must reject");
        let v = serde_json::to_value(&err).expect("serializes");
        assert_eq!(v["code"], -32602);
        assert_eq!(
            v["message"],
            "Invalid params: unsupported steering idleBehavior"
        );
        assert!(v.get("data").is_none(), "no data: {v}");
    }

    /// ROADMAP F2a: the `initialize.authMethods` branches
    /// (`acp-agent.ts:2844-2947`), with the env injected hermetically.
    fn auth_methods(capabilities: Value, env: &[(&str, &str)]) -> Value {
        let caps = ClientCapabilities::from_value(Some(&capabilities));
        let env = crate::env_snapshot::EnvSnapshot::from_pairs(
            env.iter().map(|(key, value)| (*key, *value)),
        );
        let argv = vec!["dist/index.js".to_string()];
        initialize_response(None, &caps, &env, "/usr/bin/node", &argv, &[], false)["authMethods"]
            .clone()
    }

    #[test]
    fn auth_methods_plain_client_are_empty() {
        assert_eq!(auth_methods(json!({}), &[]), json!([]));
    }

    #[test]
    fn auth_terminal_offers_subscription_and_console() {
        let methods = auth_methods(json!({"auth": {"terminal": true}}), &[]);
        assert_eq!(
            methods,
            json!([
                {
                    "description": "Use Claude subscription ",
                    "name": "Claude Subscription",
                    "id": "claude-ai-login",
                    "type": "terminal",
                    "args": ["--cli", "auth", "login", "--claudeai"]
                },
                {
                    "description": "Use Anthropic Console (API usage billing)",
                    "name": "Anthropic Console",
                    "id": "console-login",
                    "type": "terminal",
                    "args": ["--cli", "auth", "login", "--console"]
                }
            ])
        );
    }

    #[test]
    fn meta_terminal_auth_offers_the_launch_command_without_auth_terminal() {
        let methods = auth_methods(json!({"_meta": {"terminal-auth": true}}), &[]);
        assert_eq!(methods.as_array().map(Vec::len), Some(2));
        assert_eq!(methods[0]["id"], "claude-ai-login");
        assert_eq!(
            methods[0]["_meta"]["terminal-auth"],
            json!({
                "command": "/usr/bin/node",
                "args": ["dist/index.js", "--cli", "auth", "login", "--claudeai"],
                "label": "Claude Login"
            })
        );
        assert_eq!(methods[1]["id"], "console-login");
        assert_eq!(
            methods[1]["_meta"]["terminal-auth"],
            json!({
                "command": "/usr/bin/node",
                "args": ["dist/index.js", "--cli", "auth", "login", "--console"],
                "label": "Anthropic Console Login"
            })
        );
    }

    #[test]
    fn gateway_methods_are_gated_on_auth_meta_gateway() {
        let methods = auth_methods(json!({"auth": {"_meta": {"gateway": true}}}), &[]);
        assert_eq!(
            methods,
            json!([
                {
                    "id": "gateway",
                    "name": "Custom model gateway",
                    "description": "Use a custom gateway to authenticate and access models",
                    "_meta": {"gateway": {"protocol": "anthropic"}}
                },
                {
                    "id": "gateway-bedrock",
                    "name": "Custom model gateway",
                    "description": "Use a custom gateway to authenticate and access models",
                    "_meta": {"gateway": {"protocol": "bedrock"}}
                }
            ])
        );
        // A non-strict value does not opt in.
        assert_eq!(
            auth_methods(json!({"auth": {"_meta": {"gateway": "yes"}}}), &[]),
            json!([])
        );
    }

    #[test]
    fn remote_environment_offers_the_tui_login() {
        for key in [
            "NO_BROWSER",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "CLAUDE_CODE_REMOTE",
        ] {
            let methods = auth_methods(json!({"auth": {"terminal": true}}), &[(key, "1")]);
            assert_eq!(
                methods,
                json!([{
                    "description": "Run `claude /login` in the terminal",
                    "name": "Log in with Claude",
                    "id": "claude-login",
                    "type": "terminal",
                    "args": ["--cli"]
                }]),
                "{key} must select the remote login"
            );
        }
        // An empty value is falsy in Node, so it is not remote.
        let methods = auth_methods(json!({"auth": {"terminal": true}}), &[("NO_BROWSER", "")]);
        assert_eq!(methods[0]["id"], "claude-ai-login");
    }

    #[test]
    fn remote_meta_terminal_auth_carries_the_cli_args() {
        let methods = auth_methods(
            json!({"auth": {"terminal": true}, "_meta": {"terminal-auth": true}}),
            &[("NO_BROWSER", "1")],
        );
        assert_eq!(
            methods,
            json!([{
                "description": "Run `claude /login` in the terminal",
                "name": "Log in with Claude",
                "id": "claude-login",
                "type": "terminal",
                "args": ["--cli"],
                "_meta": {
                    "terminal-auth": {
                        "command": "/usr/bin/node",
                        "args": ["dist/index.js", "--cli"],
                        "label": "Claude Login"
                    }
                }
            }])
        );
    }

    #[test]
    fn terminal_methods_precede_the_gateway_methods() {
        let methods = auth_methods(
            json!({"auth": {"terminal": true, "_meta": {"gateway": true}}}),
            &[],
        );
        let ids: Vec<&str> = methods
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|method| method["id"].as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "claude-ai-login",
                "console-login",
                "gateway",
                "gateway-bedrock"
            ]
        );
    }

    /// O-84/O-85/O-136 — `initialize_options_for` derives the three
    /// client-tunable fields exactly like `forwardsSubagentText`,
    /// `perTaskStopAffordance` and `enableFileCheckpointing`
    /// (`acp-agent.ts:10947-10953, 11222-11240`).
    #[test]
    fn inv_57_initialize_fields_from_client() {
        let plain = ClientCapabilities::default();
        let init = initialize_options_for(
            &json!({"_meta": {"claudeCode": {"options": {
                "promptSuggestions": false,
                "forwardSubagentText": true,
                "perTaskStopAffordance": false,
                "skills": ["custom_skill"],
                "verbatimPrompts": true,
            }}}}),
            &plain,
            None,
            false,
        )
        .unwrap();
        assert_eq!(init.prompt_suggestions, Some(false));
        assert!(init.forward_subagent_text);
        assert_eq!(init.per_task_stop_affordance, Some(false));
        assert_eq!(init.skills, Some(json!(["custom_skill"])));
        assert_eq!(init.verbatim_prompts, Some(true));
        assert!(!init.enable_file_checkpointing);
        let request = init.build_request();
        assert_eq!(request["promptSuggestions"], json!(false));
        assert_eq!(request["forwardSubagentText"], json!(true));
        assert_eq!(request["perTaskStopAffordance"], json!(false));
        assert_eq!(request["skills"], json!(["custom_skill"]));
        assert_eq!(request["verbatimPrompts"], json!(true));

        // A plain client: promptSuggestions defaults on, the other two are
        // absent/false.
        let bare = initialize_options_for(&json!({}), &plain, None, false).unwrap();
        assert_eq!(bare.prompt_suggestions, Some(true));
        assert_eq!(bare.per_task_stop_affordance, None);
        assert!(!bare.forward_subagent_text);
        assert!(bare.build_request().get("perTaskStopAffordance").is_none());

        // The subagents RFD form defaults `perTaskStopAffordance` on and
        // forwards nested text.
        let rfd = ClientCapabilities::from_value(Some(&json!({"subagents": {}})));
        let rfd_init = initialize_options_for(&json!({}), &rfd, None, false).unwrap();
        assert_eq!(rfd_init.per_task_stop_affordance, Some(true));
        assert!(rfd_init.forward_subagent_text);

        // The client's own option wins over the RFD default.
        let opt_out = initialize_options_for(
            &json!({"_meta": {"claudeCode": {"options": {"perTaskStopAffordance": false}}}}),
            &rfd,
            None,
            false,
        )
        .unwrap();
        assert_eq!(opt_out.per_task_stop_affordance, Some(false));

        // The historical `_meta["subagent-transcript"]` capability also
        // forwards nested text.
        let transcript = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"subagent-transcript": true}
        })));
        assert!(
            initialize_options_for(&json!({}), &transcript, None, false)
                .unwrap()
                .forward_subagent_text
        );

        // An AIR client that negotiated `agentFileChangeReport` enables
        // checkpointing; the client option alone also does.
        let air = AirClient::from_client_capabilities(&json!({
            "_meta": {"jetbrains": {"air": {
                "version": 1,
                "capabilities": ["agentFileChangeReport"]
            }}}
        }));
        let reporter = initialize_options_for(&json!({}), &plain, air.as_ref(), false).unwrap();
        assert!(reporter.enable_file_checkpointing);
        let opted_in = initialize_options_for(
            &json!({"_meta": {"claudeCode": {"options": {"enableFileCheckpointing": true}}}}),
            &plain,
            None,
            false,
        )
        .unwrap();
        assert!(opted_in.enable_file_checkpointing);
    }

    /// T-105 (`create-session-options.test.ts`: "registers the PreToolUse hook
    /// of the change previews only on a v2 connection"): a v1 client gets the
    /// input's diff and waits on no hook; a v2 client registers the Edit|Write
    /// preview hook as its first callback id.
    #[test]
    fn t6g2_a_v2_connection_registers_the_change_preview_hook() {
        let plain = ClientCapabilities::default();
        let v1 = initialize_options_for(&json!({}), &plain, None, false).unwrap();
        let request = v1.build_request();
        assert!(
            request["hooks"].get("PreToolUse").is_none(),
            "a v1 client registers no PreToolUse hook: {request}"
        );
        assert_eq!(
            request["hooks"]["PostToolUse"],
            json!([{"hookCallbackIds": ["hook_0"]}]),
            "the v1 ids stay"
        );

        let v2 = initialize_options_for(&json!({}), &plain, None, true).unwrap();
        let request = v2.build_request();
        assert_eq!(
            request["hooks"]["PreToolUse"],
            json!([{"matcher": "Edit|Write", "hookCallbackIds": ["hook_0"]}]),
            "a v2 client registers the preview hook: {request}"
        );
        assert_eq!(
            request["hooks"]["PostToolUse"],
            json!([{"hookCallbackIds": ["hook_1"]}]),
            "v2 numbers the callbacks in registration order"
        );
        assert_eq!(
            request["hooks"]["TaskCompleted"],
            json!([{"hookCallbackIds": ["hook_5"]}])
        );
    }

    /// O-108/O-119/O-120/O-121/O-126/O-127/O-132/O-136 — the ACP path maps the
    /// FX-1 options onto the argv, suppresses `--continue` for a session mirror,
    /// never emits `--channels` (the SDK drops `Options.channels`, and the
    /// captured `spawn-channels.argv.json` proves Node does too), and turns
    /// checkpointing into the SDK env var.
    #[test]
    fn inv_57_session_spawn_options_fx1_and_channels_parity() {
        let serve = ServeOptions::default();
        let claude = crate::sdk_options::ClaudeOptions {
            continue_conversation: true,
            task_budget: Some("50000".into()),
            json_schema: Some(r#"{"type":"object"}"#.into()),
            debug: true,
            session_mirror: true,
            channels: vec!["alpha".into()],
            permission_prompts: Some("none".into()),
            enable_file_checkpointing: Some(true),
            ..Default::default()
        };
        let spawn = session_spawn_options(
            &serve,
            None,
            "sess".to_string(),
            claude,
            "default".to_string(),
            false,
            true,
        );
        let argv = crate::process::build_argv(&spawn);
        for flag in [
            "--task-budget=50000",
            "--json-schema={\"type\":\"object\"}",
            "--debug",
            "--session-mirror",
            "--permission-prompts=none",
        ] {
            assert!(argv.iter().any(|a| a == flag), "missing {flag}: {argv:?}");
        }
        assert!(
            !argv.iter().any(|a| a == "--continue"),
            "a session mirror suppresses --continue: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--channels")),
            "Node never forwards Options.channels, so the port must not either: {argv:?}"
        );
        assert!(
            spawn.extra_env.iter().any(|(key, value)| key
                == "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING"
                && value == "true"),
            "checkpointing -> CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING: {:?}",
            spawn.extra_env
        );

        // Without checkpointing the env var is absent.
        let spawn = session_spawn_options(
            &serve,
            None,
            "sess".to_string(),
            crate::sdk_options::ClaudeOptions::default(),
            "default".to_string(),
            false,
            true,
        );
        assert!(!spawn
            .extra_env
            .iter()
            .any(|(key, _)| key == "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING"));
    }

    /// M-31 partial (F4b): a resumed session whose CLI reports
    /// "No conversation found with session ID" answers `resourceNotFound`
    /// (-32002) with `{uri}`; any other start failure keeps the internal-error
    /// shape.
    #[test]
    fn inv_57_resumed_start_error_maps_resource_not_found() {
        let missing = SessionError::Start(
            "initialize handshake: No conversation found with session ID: abc".to_string(),
        );
        let err = resumed_start_error(&missing, true, "abc");
        assert_eq!(i32::from(err.code), -32002);
        assert_eq!(err.message, "Resource not found: abc");
        assert_eq!(err.data, Some(json!({"uri": "abc"})));

        // A fresh session keeps the generic error.
        let err = resumed_start_error(&missing, false, "abc");
        assert_eq!(i32::from(err.code), -32603);

        // A different resume failure is not a missing resource.
        let other = SessionError::Start("initialize handshake: boom".to_string());
        assert_eq!(
            i32::from(resumed_start_error(&other, true, "abc").code),
            -32603
        );
    }

    /// FX-4 (O-138) — ClaudeAcpAgent read_text_file and write_text_file passthrough.
    #[tokio::test]
    async fn inv_o138_claude_acp_agent_passthrough() {
        use agent_client_protocol::schema::v1::{
            ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest, WriteTextFileResponse,
        };
        use agent_client_protocol::{Channel, Client, Responder};

        let (agent_transport, client_transport) = Channel::duplex();

        let client_fut = Client
            .builder()
            .on_receive_request(
                |_req: ReadTextFileRequest, responder: Responder<ReadTextFileResponse>, _cx| async move {
                    responder.respond(ReadTextFileResponse::new("read-success"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                |_req: WriteTextFileRequest, responder: Responder<WriteTextFileResponse>, _cx| async move {
                    responder.respond(WriteTextFileResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client_transport);

        let agent_fut =
            Agent
                .builder()
                .name("agent")
                .connect_with(agent_transport, |connection| async move {
                    let agent = ClaudeAcpAgent::new(connection);
                    let read = agent
                        .read_text_file(ReadTextFileRequest::new("s1", PathBuf::from("/a/b.txt")))
                        .await
                        .expect("read_text_file should succeed");
                    assert_eq!(read.content, "read-success");

                    let write = agent
                        .write_text_file(WriteTextFileRequest::new(
                            "s1",
                            PathBuf::from("/a/b.txt"),
                            "write-payload",
                        ))
                        .await
                        .expect("write_text_file should succeed");
                    let _ = write;
                    Ok::<(), Error>(())
                });

        let (client_res, agent_res) = tokio::join!(client_fut, agent_fut);
        assert!(client_res.is_ok());
        assert!(agent_res.is_ok());
    }

    #[test]
    fn test_format_uri_as_link() {
        assert_eq!(
            format_uri_as_link("file:///path/to/my_file.rs"),
            "[@my_file.rs](file:///path/to/my_file.rs)"
        );
        assert_eq!(
            format_uri_as_link("file://single_file"),
            "[@single_file](file://single_file)"
        );
        assert_eq!(
            format_uri_as_link("zed://workspace/src/lib.rs"),
            "[@lib.rs](zed://workspace/src/lib.rs)"
        );
        assert_eq!(
            format_uri_as_link("https://example.com/index.html"),
            "https://example.com/index.html"
        );
        assert_eq!(
            format_uri_as_link("custom-scheme://foo"),
            "custom-scheme://foo"
        );
    }

    /// Finding A: the registry actor ending before the connection does ends the
    /// connection (the old inline handler unwound through it) instead of leaving
    /// a client that waits for answers nobody will send. A panic is re-raised.
    #[tokio::test]
    async fn t3fe_actor_end_ends_the_connection() {
        let (ended, panic) = actor_ended(Ok(()));
        assert!(ended.is_err() && panic.is_none());

        // A panic is handed back for `serve_until` to re-raise after the
        // teardown (finding N3), not raised from here.
        let panicked = tokio::spawn(async { panic!("registry actor panic") }).await;
        let (ended, panic) = actor_ended(panicked);
        assert!(ended.is_err());
        assert!(panic.is_some(), "the actor panic payload is kept");
    }
}
