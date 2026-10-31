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
use crate::auth_status::{spawn_auth_actor, AuthCmd, AuthHandle, AuthPush};
use crate::client_caps::ClientCapabilities;
use crate::control::InitializeOptions;
use crate::map;
use crate::process::{SpawnOptions, Timings};
use crate::session::{Session, SessionError, SessionOptions, SessionOutbound};
use crate::session_methods;
use crate::session_timing::SessionTiming;
use crate::system_prompt;

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

/// The terminal auth methods, mirroring `initialize` (`acp-agent.ts:2869-2947`).
///
/// The remote branch (`NO_BROWSER`/`SSH_*`/`CLAUDE_CODE_REMOTE`) offers the
/// TUI login instead of the browser redirect; the non-remote branch offers the
/// subscription and Console logins. A client that only declared
/// `_meta["terminal-auth"]` gets the same methods with the launch command.
fn terminal_auth_methods(
    caps: &ClientCapabilities,
    env: &crate::env_snapshot::EnvSnapshot,
    executable: &str,
    argv: &[String],
) -> Vec<Value> {
    let supports_terminal = caps.auth_terminal;
    let supports_meta = caps.meta_terminal_auth;
    if !supports_terminal && !supports_meta {
        return Vec::new();
    }

    // `--hide-claude-auth` belongs to F34; until then Claude auth is not
    // hidden.
    let hidden = false;
    let is_remote = env.flag("NO_BROWSER")
        || env.flag("SSH_CONNECTION")
        || env.flag("SSH_CLIENT")
        || env.flag("SSH_TTY")
        || env.flag("CLAUDE_CODE_REMOTE");

    let terminal_auth = |extra: &[&str], label: &str| -> Value {
        let args: Vec<String> = argv
            .iter()
            .cloned()
            .chain(extra.iter().map(|arg| (*arg).to_string()))
            .collect();
        json!({
            "command": executable,
            "args": args,
            "label": label,
        })
    };

    if is_remote {
        let mut method = json!({
            "description": "Run `claude /login` in the terminal",
            "name": "Log in with Claude",
            "id": "claude-login",
            "type": "terminal",
            "args": ["--cli"],
        });
        if supports_meta {
            method["_meta"] = json!({
                "terminal-auth": terminal_auth(&["--cli"], "Claude Login"),
            });
        }
        if !hidden && (supports_terminal || supports_meta) {
            return vec![method];
        }
        return Vec::new();
    }

    let mut claude_login = json!({
        "description": "Use Claude subscription ",
        "name": "Claude Subscription",
        "id": "claude-ai-login",
        "type": "terminal",
        "args": ["--cli", "auth", "login", "--claudeai"],
    });
    let mut console_login = json!({
        "description": "Use Anthropic Console (API usage billing)",
        "name": "Anthropic Console",
        "id": "console-login",
        "type": "terminal",
        "args": ["--cli", "auth", "login", "--console"],
    });
    if supports_meta {
        claude_login["_meta"] = json!({
            "terminal-auth": terminal_auth(
                &["--cli", "auth", "login", "--claudeai"],
                "Claude Login",
            ),
        });
        console_login["_meta"] = json!({
            "terminal-auth": terminal_auth(
                &["--cli", "auth", "login", "--console"],
                "Anthropic Console Login",
            ),
        });
    }

    let mut methods = Vec::new();
    if !hidden && (supports_terminal || supports_meta) {
        methods.push(claude_login);
    }
    if supports_terminal || supports_meta {
        methods.push(console_login);
    }
    methods
}

/// The custom-gateway auth methods, offered only when the client declared
/// `auth._meta.gateway === true` (`acp-agent.ts:2844-2867`).
fn gateway_auth_methods() -> Vec<Value> {
    vec![
        json!({
            "id": "gateway",
            "name": "Custom model gateway",
            "description": "Use a custom gateway to authenticate and access models",
            "_meta": {"gateway": {"protocol": "anthropic"}},
        }),
        json!({
            "id": "gateway-bedrock",
            "name": "Custom model gateway",
            "description": "Use a custom gateway to authenticate and access models",
            "_meta": {"gateway": {"protocol": "bedrock"}},
        }),
    ]
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
fn initialize_response(
    air: Option<&AirClient>,
    caps: &ClientCapabilities,
    env: &crate::env_snapshot::EnvSnapshot,
    executable: &str,
    argv: &[String],
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

    let mut auth_methods = terminal_auth_methods(caps, env, executable, argv);
    if caps.auth_gateway {
        auth_methods.extend(gateway_auth_methods());
    }

    let mut meta = serde_json::Map::new();
    meta.insert("steering".into(), json!({ "supported": true }));
    if is_air {
        meta.insert(
            "jetbrains".into(),
            json!({
                "air": {
                    "version": 1,
                    "capabilities": AIR_CAPABILITIES,
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
fn session_error_to_rpc(err: &SessionError) -> (String, Option<Value>) {
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
        SessionError::Closed => (crate::turn::SESSION_ENDED_MESSAGE.to_string(), None),
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

/// A resumed session whose CLI cannot find the stored conversation answers
/// `resourceNotFound` (-32002), not `Internal error` (`acp-agent.ts:8662-8672`:
/// `RequestError.resourceNotFound(sessionId)`). Node maps the SDK's
/// "No conversation found with session ID" / "Query closed before response
/// received" initialization errors; the Rust initialize handshake surfaces the
/// CLI's own error text, so the substring check is equivalent for the first.
fn resumed_start_error(err: &SessionError, resumed: bool, session_id: &str) -> Error {
    if resumed {
        if let SessionError::Start(message) = err {
            if message.contains("No conversation found with session ID") {
                return Error::new(-32002, format!("Resource not found: {session_id}"))
                    .data(json!({ "uri": session_id }));
            }
        }
    }
    rpc_error(session_error_to_rpc(err))
}

/// Serve ACP over `transport` (D9). Runs until the transport closes.
pub async fn serve(transport: impl ConnectTo<Agent> + 'static, opts: ServeOptions) -> Result<()> {
    // A `session/cancel` notification must reach the owning session actor. The
    // request handler owns the live registry; it publishes each new session's
    // handle (a cheap clone, not session state) over this channel so the
    // notification handler keeps its own copy — lock-free (guard G5).
    let (session_tx, mut session_rx) = mpsc::unbounded_channel::<(String, Session)>();
    let mut registry = Registry {
        sessions: std::collections::HashMap::new(),
        session_tx,
        air: None,
        caps: ClientCapabilities::default(),
        auth: None,
        settings_managers: std::collections::HashMap::new(),
    };
    Agent
        .builder()
        .name("claude-agent-acp-rs")
        .on_receive_request(
            async move |req: UntypedMessage, responder, connection| {
                handle_request(&mut registry, req, &opts, responder, connection).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                // The notification handler keeps its own session-handle map,
                // fed by the request handler's channel.
                let mut sessions: std::collections::HashMap<String, Session> =
                    std::collections::HashMap::new();
                async move |notif: UntypedMessage, _connection| {
                    while let Ok((sid, session)) = session_rx.try_recv() {
                        sessions.insert(sid, session);
                    }
                    handle_notification(&notif, &sessions).await
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
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
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    tokio::select! {
        res = serve(transport, opts) => res,
        _ = shutdown_requested(&mut shutdown) => Ok(()),
    }
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
        }
    }
    Ok(())
}

/// The per-connection session registry, owned by the request handler (the
/// connection dispatches requests sequentially, so no lock is needed).
struct Registry {
    sessions: std::collections::HashMap<String, Session>,
    /// Publishes each new session's handle to the notification handler.
    session_tx: mpsc::UnboundedSender<(String, Session)>,
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
}

#[allow(dead_code)]
impl Registry {
    /// Settings manager for a session, if registered.
    pub fn settings_manager(
        &self,
        session_id: &str,
    ) -> Option<&Arc<crate::settings_manager::SettingsManager>> {
        self.settings_managers.get(session_id)
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
    let probe: crate::auth_status::ProbeFn = Box::new(move |cwd| {
        let claude_path = claude_path.clone();
        let extra_env = extra_env.clone();
        let env = probe_env.clone();
        Box::pin(async move {
            crate::auth_status::probe_cli(claude_path.as_deref(), &cwd, &extra_env, &env).await
        })
    });
    spawn_auth_actor(
        push_tx,
        probe,
        std::env::current_dir().unwrap_or_default(),
        env,
    )
}

/// Hand a created session's account and cwd to the auth actor (publish the
/// session identity; re-probe when the cwd differs from the last probe's).
fn publish_session_account(registry: &Registry, cwd: PathBuf, session: &Session) {
    if let Some(auth) = &registry.auth {
        auth.send(AuthCmd::SessionCreated {
            cwd,
            account: session.account().cloned(),
        });
    }
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
    Ok(InitializeOptions {
        system_prompt,
        append_system_prompt,
        hooks: Some(crate::hooks::init_hooks_payload()),
        prompt_suggestions: Some(prompt_suggestions),
        tool_aliases: opts.tool_aliases,
        tool_config: opts.tool_config,
        forward_subagent_text,
        per_task_stop_affordance,
        enable_file_checkpointing,
        ..InitializeOptions::default()
    })
}

/// Register a new session: keep its handle in the registry, publish it to the
/// notification handler (lock-free), and spawn a task that forwards its updates
/// to the client as they arrive. The actor emits a `FinalText` chunk before
/// resolving the prompt's oneshot, so the chunk lands ahead of the response.
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
    let _ = registry
        .session_tx
        .send((session_id.to_string(), session.clone()));

    // Decision 9 / 2.1 + F5b/A-10: background get_context_usage {detail:"summary"}
    // spawned after register. The request is tagged with the model it was issued
    // for, so an answer arriving after a model switch is dropped (`stillCurrent`).
    let control = session.control().clone();
    let cmd_tx = session.command_sender().clone();
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
                    }
                    if let Ok(msg) = UntypedMessage::new("session/update", value) {
                        let _ = connection.send_notification(msg);
                    }
                }
                // U-10: the raw SDK message ext notification
                // (`client.extNotification("_claude/sdkMessage", {...})`).
                SessionOutbound::ExtNotification { method, params } => {
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
                    tokio::spawn(async move {
                        let result = match UntypedMessage::new("session/request_permission", params)
                        {
                            Ok(msg) => {
                                let sent = connection.send_request(msg);
                                sent.block_task().await.map_err(|e| e.to_string())
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        let _ = reply.send(result);
                    });
                }
            }
        }
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

/// Start (or restart) the session `session_id` from its stored transcript
/// (`--resume`) and answer a `session/load`, `session/resume` or `session/fork`
/// request; the new session is registered and its proactive command list sent.
async fn start_resumed(
    registry: &mut Registry,
    session_id: String,
    params: &Value,
    opts: &ServeOptions,
    responder: agent_client_protocol::Responder<Value>,
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
    let initialize = match initialize_options_for(params, &registry.caps, registry.air.as_ref()) {
        Ok(initialize) => initialize,
        Err(e) => return responder.respond_with_error(e),
    };
    let cwd = cwd_str.map(PathBuf::from);
    let (mut claude, permission_mode, allow_bypass) =
        match spawn_inputs(params, cwd.as_deref(), opts, Some(&session_id)) {
            Ok(inputs) => inputs,
            Err(e) => return responder.respond_with_error(e),
        };
    claude.enable_file_checkpointing = Some(initialize.enable_file_checkpointing);
    timing.phase("resume-transcript", "");
    timing.phase("settings", "");
    let raw_sdk_messages = claude.raw_sdk_messages.clone();
    let mut spawn = session_spawn_options(
        opts,
        cwd,
        session_id.clone(),
        claude,
        permission_mode,
        allow_bypass,
        true,
    );
    spawn.resume = Some(session_id.clone());
    spawn.session_id = None;
    let auth_cwd = spawn.default_cwd.clone().unwrap_or_default();
    let sess_opts = SessionOptions::new(spawn, opts.timings.clone(), initialize)
        .with_air(registry.air.clone())
        .with_caps(registry.caps.clone())
        .with_raw_sdk_messages(raw_sdk_messages)
        .with_env(opts.env.clone())
        .with_logger(opts.logger.clone());
    // PC-2.2 / B-02: the replay runs while the CLI starts (Node
    // `createSessionWhileReplaying`) and reads the transcript off the async
    // runtime; the response waits for both, so the replay frames still precede
    // it.
    let air = registry.air.is_some();
    let replay_future = async {
        let messages = crate::transcript::session_messages_async(&opts.env, &session_id).await;
        let (notifications, task_state, last_published) =
            crate::transcript::replay_with_task_state(&messages, &session_id, air);
        for mut value in notifications {
            if let Some(update) = value.get_mut("update") {
                crate::map::materialize_tool_call_defaults(update);
                crate::map::materialize_diff_old_text(update);
            }
            if let Ok(msg) = UntypedMessage::new("session/update", value) {
                let _ = connection.send_notification(msg);
            }
        }
        (task_state, last_published)
    };
    let no_replay: (Vec<(String, crate::hooks::TaskStateEntry)>, Option<String>) =
        (Vec::new(), None);
    let (start_result, replayed) = if replay {
        tokio::join!(Session::start(sess_opts, session_id.clone()), replay_future)
    } else {
        (
            Session::start(sess_opts, session_id.clone()).await,
            no_replay,
        )
    };
    match start_result {
        Ok((session, update_rx)) => {
            timing.phase("session-ready", "");
            publish_session_account(registry, auth_cwd.clone(), &session);
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
                let (task_state, last_published) = replayed;
                let _ = session.seed_task_state(task_state, last_published).await;
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
            registry.settings_managers.insert(session_id.clone(), mgr);
            register_session(registry, &session_id, session, update_rx, connection);
            timing.phase("register", "");
            responder.respond(session_response(&session_id, &modes, &config_options))?;
            let update = map::map_commands_changed(&json!(commands), &[]);
            // Decision 8: raw JSON with `input: null` for commands without a
            // hint (the typed update would omit the field).
            let value = json!({
                "sessionId": session_id.clone(),
                "update": map::available_commands_json(&update),
            });
            if let Ok(msg) = UntypedMessage::new("session/update", value) {
                let _ = connection.send_notification(msg);
            }
            Ok(())
        }
        Err(e) => responder.respond_with_error(resumed_start_error(&e, true, &session_id)),
    }
}

/// Dispatch one ACP request (8.1). `registry` is owned by the handler closure;
/// it is mutated sequentially by the connection loop.
async fn handle_request(
    registry: &mut Registry,
    req: UntypedMessage,
    opts: &ServeOptions,
    responder: agent_client_protocol::Responder<Value>,
    connection: ConnectionTo<agent_client_protocol::Client>,
) -> Result<(), Error> {
    let method = req.method().to_string();
    let params = req.params().clone();

    match method.as_str() {
        "initialize" => {
            let _version = params
                .get("protocolVersion")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            // Parse the AIR client declaration once (Decision 11); the optional
            // `clientCapabilities._meta.jetbrains.air` gates the AIR advertise
            // (E-4 / X-1). Malformed -> None (a plain client).
            registry.air = params
                .get("clientCapabilities")
                .and_then(AirClient::from_client_capabilities);
            // Parse the full capability set once (ROADMAP F2a): auth methods,
            // notices, config options and the tool-call choices all read it.
            registry.caps = ClientCapabilities::from_value(params.get("clientCapabilities"));
            let (executable, argv) = process_entrypoint();
            responder.respond(initialize_response(
                registry.air.as_ref(),
                &registry.caps,
                &opts.env,
                &executable,
                &argv,
            ))?;
            // G2 (Decision 11): learn the auth identity in the background AFTER
            // the response (Node `initialize` never waits on the CLI probe);
            // `_auth/status_update` is pushed only when the read lands.
            let auth = registry
                .auth
                .get_or_insert_with(|| spawn_auth(opts, &connection))
                .clone();
            auth.probe(std::env::current_dir().unwrap_or_default(), false);
            Ok(())
        }
        "session/new" => {
            // Session timing (`session-timing.ts`, B-01): coarse phases at the
            // agent layer (validate-cwd/settings/register).
            let create_started = Instant::now();
            let cwd_str = params.get("cwd").and_then(Value::as_str);
            if let Err(e) = validate_cwd(cwd_str) {
                return responder.respond_with_error(e);
            }
            let initialize =
                match initialize_options_for(&params, &registry.caps, registry.air.as_ref()) {
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
            let spawn = session_spawn_options(
                opts,
                cwd,
                session_id.clone(),
                claude,
                permission_mode,
                allow_bypass,
                true,
            );
            let auth_cwd = spawn.default_cwd.clone().unwrap_or_default();
            let sess_opts = SessionOptions::new(spawn, opts.timings.clone(), initialize)
                .with_air(registry.air.clone())
                .with_caps(registry.caps.clone())
                .with_raw_sdk_messages(raw_sdk_messages)
                .with_env(opts.env.clone())
                .with_logger(opts.logger.clone());
            match Session::start(sess_opts, session_id.clone()).await {
                Ok((session, update_rx)) => {
                    publish_session_account(registry, auth_cwd.clone(), &session);
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
                    register_session(registry, &session_id, session, update_rx, &connection);
                    timing.phase("register", "");
                    responder.respond(session_response(&session_id, &modes, &config_options))?;
                    let update = map::map_commands_changed(&json!(commands), &[]);
                    // Decision 8: raw JSON with `input: null` for commands
                    // without a hint (the typed update would omit the field).
                    let value = json!({
                        "sessionId": session_id.clone(),
                        "update": map::available_commands_json(&update),
                    });
                    if let Ok(msg) = UntypedMessage::new("session/update", value) {
                        let _ = connection.send_notification(msg);
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
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let replay = method == "session/load";
            // Follow-up (a): if the request targets an already live session,
            // reuse it without leaking or starting a redundant child process. A
            // `session/load` on a live session replays its history (X8,
            // `acp-agent.js:1583-1587`); `session/resume` does not.
            if let Some(session) = registry.sessions.get(&session_id) {
                if replay {
                    let messages =
                        crate::transcript::session_messages_async(&opts.env, &session_id).await;
                    let (notifications, task_state, last_published) =
                        crate::transcript::replay_with_task_state(
                            &messages,
                            &session_id,
                            registry.air.is_some(),
                        );
                    for mut value in notifications {
                        if let Some(update) = value.get_mut("update") {
                            crate::map::materialize_tool_call_defaults(update);
                            crate::map::materialize_diff_old_text(update);
                        }
                        if let Ok(msg) = UntypedMessage::new("session/update", value) {
                            let _ = connection.send_notification(msg);
                        }
                    }
                    // Node forgets the published plan before a replay and shares
                    // the live `taskState` with it; seed the rebuilt state so the
                    // live actor continues from it.
                    let _ = session.seed_task_state(task_state, last_published).await;
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
                    let update = map::map_commands_changed(&json!(commands), &[]);
                    let value = json!({
                        "sessionId": session_id.clone(),
                        "update": map::available_commands_json(&update),
                    });
                    if let Ok(msg) = UntypedMessage::new("session/update", value) {
                        let _ = connection.send_notification(msg);
                    }
                }
                return Ok(());
            }
            start_resumed(
                registry,
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
            // Node order: validate the systemPrompt, copy the transcript, then
            // start the new session like a resumed one (`unstable_forkSession`).
            if let Err(e) = initialize_options_for(&params, &registry.caps, registry.air.as_ref()) {
                return responder.respond_with_error(e);
            }
            let fork_session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(message_id) = session_methods::fork_point_message_id(&params) {
                return responder.respond_with_error(
                    Error::new(
                        -32602,
                        format!(
                            "Invalid params: Fork point message {message_id} was not found in session {fork_session_id}"
                        ),
                    )
                    .data(json!({ "messageId": message_id })),
                );
            }
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let new_id = match session_methods::fork_copy(fork_session_id, cwd, &opts.env).await {
                Ok(id) => id,
                Err(e) => return responder.respond_with_error(e),
            };
            start_resumed(
                registry,
                new_id,
                &params,
                opts,
                responder,
                &connection,
                ResumedKind::Fork,
            )
            .await
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
            match registry.sessions.remove(&session_id) {
                Some(session) => {
                    tokio::spawn(async move {
                        session.close().await;
                        let _ = responder.respond(json!({}));
                    });
                    Ok(())
                }
                None => responder.respond(json!({})),
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
                        let _ = responder.respond_with_error(
                            Error::internal_error().data(json!({ "details": e })),
                        );
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
            let value = match params.get("value") {
                Some(v) => v.clone(),
                None => {
                    return responder.respond_with_error(Error::internal_error().data(json!({
                        "details": format!("Invalid value for config option {config_id}: undefined")
                    })));
                }
            };
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
                        let _ = responder.respond_with_error(
                            Error::internal_error().data(json!({ "details": e })),
                        );
                    }
                }
            });
            Ok(())
        }
        "session/list" => {
            let env = opts.env.clone();
            tokio::spawn(async move {
                match session_methods::list(&params, &env).await {
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
        // An AIR client archives with `session/delete` in Node; hard-deleting
        // would destroy data Node keeps, so AIR keeps -32601 (Decision 22).
        "session/delete" if registry.air.is_none() => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Tear a loaded session down first so its transcript is not recreated.
            let loaded = registry.sessions.remove(&session_id);
            registry.settings_managers.remove(&session_id);
            let env = opts.env.clone();
            tokio::spawn(async move {
                if let Some(session) = loaded {
                    session.close().await;
                }
                match session_methods::delete(session_id, &env).await {
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
        "logout" => {
            // G2 / 2.4: `claude auth logout`, then a fresh read of the store
            // (pushed as `_auth/status_update` BEFORE the response, like Node).
            let auth = registry
                .auth
                .get_or_insert_with(|| spawn_auth(opts, &connection))
                .clone();
            auth.bump_epoch();
            let claude_path = opts.claude_path.clone();
            let extra_env = opts.extra_env.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    crate::auth_status::run_logout(claude_path.as_deref(), &extra_env).await
                {
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
                let (reply, done) = tokio::sync::oneshot::channel();
                auth.send(AuthCmd::Logout {
                    cwd: std::env::current_dir().unwrap_or_default(),
                    reply,
                });
                let _ = done.await;
                // F5b/B-69: 1M-context entitlement can differ per account/tier,
                // so windows learned under one login must not seed sessions
                // under the next.
                crate::session_model::clear_context_window_cache();
                let _ = responder.respond(json!({}));
            });
            Ok(())
        }
        "session/prompt" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                // Node: `throw new Error("Session not found")` -> -32603 with
                // `data:{details}` (a closed or unknown session).
                None => {
                    return responder.respond_with_error(
                        Error::internal_error().data(json!({ "details": "Session not found" })),
                    );
                }
            };
            let first_text = params
                .get("prompt")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let is_local_only = is_local_only_command(first_text);
            let mcp_command = crate::mcp_command::prompt_mcp_command(&params);
            let uuid = uuid_v4();
            let frame = prompt_to_claude(&params, &session_id, &uuid);
            // G12 (Decision 21): record the `/mcp` command on the actor BEFORE
            // the prompt, so the actor knows the turn's replacement is coming.
            // Both sends are on the same unbounded channel, so the actor sees
            // the command first.
            if let Some(command) = mcp_command {
                session.send_mcp(uuid.clone(), command);
            }
            // Enqueue the prompt on the actor synchronously so a later
            // `session/cancel` notification (processed on the same single-task
            // connection) is always ordered *after* it — the queued-sweep
            // ordering the cancel relies on. Only the settlement is awaited on
            // a spawned task so the connection is never blocked while a turn is
            // in flight (Node's `prompt()` returns immediately).
            let prompt_text = acp_prompt_text(&params);
            let mut reply_rx = session.send_prompt(uuid, frame, is_local_only, prompt_text);
            // F2c / M-52: Node's `runPromptWithCancellation`
            // (`acp-agent.ts:13528-13552`) listens for `$/cancel_request` on
            // this prompt's request signal and routes it to the same
            // `agent.cancel` path as `session/cancel`. The crate already
            // records the notification on the request's cancellation marker,
            // so watch that marker and cancel the owning session; the prompt
            // then settles `cancelled` (never a `request_cancelled` error).
            let cancellation = responder.cancellation();
            let cancel_session = session.clone();
            let connection = connection.clone();
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
                .unwrap_or_else(|_| Err(SessionError::Closed));
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
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(rpc_error(session_error_to_rpc(&e)));
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
            let session_id = steer.session_id;
            let session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                None => {
                    return responder
                        .respond_with_error(Error::internal_error().data("Session not found"));
                }
            };
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
                let outcome = reply_rx
                    .await
                    .unwrap_or_else(|_| json!({ "outcome": "error" }));
                let _ = responder.respond(outcome);
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
                        let _ = responder.respond_with_error(rpc_error(session_error_to_rpc(&e)));
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
            let Some(session) = registry.sessions.get(&request.session_id).cloned() else {
                // Node's `steer` throws `new Error("Session not found")`; the
                // SDK maps it to `internalError` with `data:{details}`.
                return responder.respond_with_error(
                    Error::internal_error().data(json!({ "details": "Session not found" })),
                );
            };
            let command = request.command_text();
            let prompt_params = json!({
                "sessionId": request.session_id,
                "prompt": [ { "type": "text", "text": command } ],
            });
            let steer_uuid = uuid_v4();
            let frame = prompt_to_claude(&prompt_params, &request.session_id, &steer_uuid);
            let is_local_only = is_local_only_command(&command);
            let steer_rx = session.steer(steer_uuid, frame.clone(), is_local_only, true);
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
                    match session
                        .prompt(prompt_uuid, frame, is_local_only, command)
                        .await
                    {
                        Ok(reply) => {
                            if let Some(flush_rx) = reply.flush_rx {
                                let _ = flush_rx.await;
                            }
                        }
                        Err(e) => {
                            let _ =
                                responder.respond_with_error(rpc_error(session_error_to_rpc(&e)));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        initialize_response(None, &caps, &env, "/usr/bin/node", &argv)["authMethods"].clone()
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
                "perTaskStopAffordance": false
            }}}}),
            &plain,
            None,
        )
        .unwrap();
        assert_eq!(init.prompt_suggestions, Some(false));
        assert!(init.forward_subagent_text);
        assert_eq!(init.per_task_stop_affordance, Some(false));
        assert!(!init.enable_file_checkpointing);
        let request = init.build_request();
        assert_eq!(request["promptSuggestions"], json!(false));
        assert_eq!(request["forwardSubagentText"], json!(true));
        assert_eq!(request["perTaskStopAffordance"], json!(false));

        // A plain client: promptSuggestions defaults on, the other two are
        // absent/false.
        let bare = initialize_options_for(&json!({}), &plain, None).unwrap();
        assert_eq!(bare.prompt_suggestions, Some(true));
        assert_eq!(bare.per_task_stop_affordance, None);
        assert!(!bare.forward_subagent_text);
        assert!(bare.build_request().get("perTaskStopAffordance").is_none());

        // The subagents RFD form defaults `perTaskStopAffordance` on and
        // forwards nested text.
        let rfd = ClientCapabilities::from_value(Some(&json!({"subagents": {}})));
        let rfd_init = initialize_options_for(&json!({}), &rfd, None).unwrap();
        assert_eq!(rfd_init.per_task_stop_affordance, Some(true));
        assert!(rfd_init.forward_subagent_text);

        // The client's own option wins over the RFD default.
        let opt_out = initialize_options_for(
            &json!({"_meta": {"claudeCode": {"options": {"perTaskStopAffordance": false}}}}),
            &rfd,
            None,
        )
        .unwrap();
        assert_eq!(opt_out.per_task_stop_affordance, Some(false));

        // The historical `_meta["subagent-transcript"]` capability also
        // forwards nested text.
        let transcript = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"subagent-transcript": true}
        })));
        assert!(
            initialize_options_for(&json!({}), &transcript, None)
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
        let reporter = initialize_options_for(&json!({}), &plain, air.as_ref()).unwrap();
        assert!(reporter.enable_file_checkpointing);
        let opted_in = initialize_options_for(
            &json!({"_meta": {"claudeCode": {"options": {"enableFileCheckpointing": true}}}}),
            &plain,
            None,
        )
        .unwrap();
        assert!(opted_in.enable_file_checkpointing);
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
}
