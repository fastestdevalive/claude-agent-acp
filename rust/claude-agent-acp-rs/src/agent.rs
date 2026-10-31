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

use agent_client_protocol::schema::v1::StopReason;
use agent_client_protocol::{Agent, ConnectTo, ConnectionTo, Error, Result, UntypedMessage};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::air::AirClient;
use crate::auth_status::{spawn_auth_actor, AuthCmd, AuthHandle, AuthPush};
use crate::control::InitializeOptions;
use crate::map;
use crate::process::{SpawnOptions, Timings};
use crate::session::{Session, SessionError, SessionOptions, SessionOutbound};
use crate::session_methods;
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
            env,
        }
    }

    #[must_use]
    pub fn with_env(mut self, env: crate::env_snapshot::EnvSnapshot) -> Self {
        self.env = env;
        self
    }
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

/// The `initialize` response, byte-matching the Node adapter at the pinned
/// upstream version (modulo ignored volatile fields); `agentInfo.version` is
/// composed from `porting/UPSTREAM_VERSION` (Decision 3).
///
/// `air` is `Some` only for a client that declared `_meta.jetbrains.air`
/// (Decision 11). Non-AIR clients get no `_meta.jetbrains`, no goal and no
/// `sessionCapabilities.subagents` (E-3/E-4); the `authStatus` marker (E-2,
/// R75-3) and the `systemPrompt` advertise (E-3, honouring is Phase 3) are
/// always present.
fn initialize_response(air: Option<&AirClient>) -> Value {
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
        "authMethods": [],
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

/// Format a URI as a markdown-style link (subset of `formatUriAsLink`).
fn format_uri_as_link(uri: &str) -> String {
    let name = uri
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(uri);
    format!("[{name}]({uri})")
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
        extra_args: claude.extra_args,
        permission_prompt_tool: true,
        allow_dangerously_skip_permissions: allow_bypass,
        replay_user_messages: true,
        include_partial_messages: true,
        env_snapshot: serve.env.clone(),
        ..SpawnOptions::default()
    }
}

/// Resolve the spawn inputs for a `session/new|load` request (Decision 2/3/4):
/// `_meta.claudeCode.options`, the permission mode (explicit > resumed >
/// settings; Phase 1 has no explicit/resumed source), and whether bypass is
/// available.
///
/// Node IGNORES `_meta.claudeCode.options.permissionMode` (X3): the mode comes
/// from settings only. `allow_bypass` combines root/`IS_SANDBOX`, the settings'
/// `disableBypassPermissionsMode` and the client's
/// `allowDangerouslySkipPermissions !== false` (R6).
pub fn spawn_inputs(
    params: &Value,
    cwd: Option<&std::path::Path>,
    serve: &ServeOptions,
    resumed_id: Option<&str>,
) -> (crate::sdk_options::ClaudeOptions, String, bool) {
    let dir = cwd
        .map(std::path::Path::to_path_buf)
        .or_else(|| serve.default_cwd.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let settings = crate::settings::load(&dir, &serve.env);
    let claude = crate::sdk_options::claude_options(params);
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

    let permission_mode = crate::permission_mode::resolve_initial_permission_mode(
        None,
        resumed_mode.as_deref(),
        settings.default_mode.as_deref(),
        allow_bypass,
    );
    (claude, permission_mode, allow_bypass)
}

/// Decide the `_session/steering` outcome (R32 / B1 row).
///
/// Validates `idleBehavior` (`_meta.steering.idleBehavior`): any value other
/// than `"promptRequired"` → `invalidParams` ("unsupported steering
/// idleBehavior"). Then, given whether a turn is in flight, maps to the wire
/// outcome:
///
/// - in flight → `{outcome:"injected"}` (the caller injects at `now` priority),
/// - idle + `promptRequired` → `{outcome:"promptRequired", reason:"noRunningTurn"}`,
/// - idle, no opt-in → `{outcome:"startedNewTurn"}` (detached new turn).
///
/// `idle_behavior` is `None` when `_meta.steering.idleBehavior` is absent.
pub fn steer_outcome(turn_in_flight: bool, idle_behavior: Option<&str>) -> Result<Value, Error> {
    if idle_behavior.is_some_and(|b| b != "promptRequired") {
        return Err(Error::invalid_params().data("unsupported steering idleBehavior"));
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
        auth: None,
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
    /// The auth-status actor, spawned by the first `initialize` (G2).
    auth: Option<AuthHandle>,
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
    let probe: crate::auth_status::ProbeFn = Box::new(move |cwd| {
        let claude_path = claude_path.clone();
        let extra_env = extra_env.clone();
        Box::pin(async move {
            crate::auth_status::probe_cli(claude_path.as_deref(), &cwd, &extra_env).await
        })
    });
    spawn_auth_actor(push_tx, probe, std::env::current_dir().unwrap_or_default())
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
    if !path.is_absolute() {
        return Err(Error::new(
            -32602,
            format!("`cwd` must be an absolute path, but received: {cwd}"),
        )
        .data(json!({ "cwd": cwd })));
    }
    // A single `stat` is a fast syscall, not "large-file I/O"; inline is fine
    // (rust-coding §4 concerns whole-file blocking reads).
    let Ok(meta) = std::fs::metadata(path) else {
        return Err(Error::new(
            -32602,
            format!("`cwd` does not exist on the machine running the agent: {cwd}"),
        )
        .data(json!({ "cwd": cwd })));
    };
    if !meta.is_dir() {
        return Err(
            Error::new(-32602, format!("`cwd` is not a directory: {cwd}"))
                .data(json!({ "cwd": cwd })),
        );
    }
    Ok(())
}

/// Validate the optional `systemPrompt` (after `validate_cwd`, before any spawn)
/// and fold it plus AIR `customInstructions` into the `initialize` options
/// (rust-air-gaps G3/G5, Decision 10).
fn initialize_options_for(params: &Value) -> Result<InitializeOptions, Error> {
    let client_prompt = system_prompt::client_system_prompt(params.get("systemPrompt"))?;
    let custom = system_prompt::air_custom_instructions(params.get("_meta"));
    let (system_prompt, append_system_prompt) =
        system_prompt::sdk_initialize_fields(client_prompt.as_ref(), custom.as_deref());
    let opts = crate::sdk_options::claude_options(params);
    let prompt_suggestions = opts.prompt_suggestions.unwrap_or(true);
    Ok(InitializeOptions {
        system_prompt,
        append_system_prompt,
        hooks: Some(crate::hooks::init_hooks_payload()),
        prompt_suggestions: Some(prompt_suggestions),
        forward_subagent_text: false,
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

    // Decision 9 / 2.1: background get_context_usage {detail:"summary"} spawned after register.
    let control = session.control().clone();
    let cmd_tx = session.command_sender().clone();
    tokio::spawn(async move {
        if let Ok(res) = control
            .send_request(json!({"subtype": "get_context_usage", "detail": "summary"}))
            .await
        {
            let raw_max = crate::control::raw_max_tokens(&res);
            if let Some(tokens) = raw_max {
                if tokens > 0 {
                    let _ =
                        cmd_tx.send(crate::session::Command::UpdateContextWindow { size: tokens });
                }
            }
        }
    });
    let connection = connection.clone();
    tokio::spawn(async move {
        let mut update_rx = update_rx;
        while let Some(item) = update_rx.recv().await {
            match item {
                SessionOutbound::Update(notif) => {
                    let _ = connection.send_notification(notif);
                }
                // Item 3.3: an untyped AIR `session/update` (async-task
                // spawns/progress/state-updates), value = `{"sessionId",⋯,"update",{⋯}}`.
                SessionOutbound::RawUpdate(value) => {
                    if let Ok(msg) = UntypedMessage::new("session/update", value) {
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
    replay: bool,
) -> Result<(), Error> {
    let cwd_str = params.get("cwd").and_then(Value::as_str);
    if let Err(e) = validate_cwd(cwd_str) {
        return responder.respond_with_error(e);
    }
    let initialize = match initialize_options_for(params) {
        Ok(initialize) => initialize,
        Err(e) => return responder.respond_with_error(e),
    };
    let cwd = cwd_str.map(PathBuf::from);
    let (claude, permission_mode, allow_bypass) =
        spawn_inputs(params, cwd.as_deref(), opts, Some(&session_id));
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
        .with_env(opts.env.clone());
    match Session::start(sess_opts, session_id.clone()).await {
        Ok((session, update_rx)) => {
            publish_session_account(registry, auth_cwd, &session);
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
            register_session(registry, &session_id, session, update_rx, connection);
            // rust-parity-fixes 3.5 (Decision 14, P10): a `session/load` replays
            // the stored history BEFORE the response (Node `loadSession` →
            // `replaySessionHistory`); `session/resume` and `session/fork` do
            // not. Each replayed message becomes a `session/update` (user /
            // assistant chunks, tool calls/results).
            if replay {
                let messages = crate::transcript::session_messages(&opts.env, &session_id);
                for value in crate::transcript::replay_notifications(&messages, &session_id) {
                    if let Ok(msg) = UntypedMessage::new("session/update", value) {
                        let _ = connection.send_notification(msg);
                    }
                }
            }
            responder.respond(session_response(&session_id, &modes, &config_options))?;
            let update = map::map_commands_changed(&json!(commands), &json!([]));
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
        Err(e) => responder.respond_with_error(rpc_error(session_error_to_rpc(&e))),
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
            responder.respond(initialize_response(registry.air.as_ref()))?;
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
            let cwd_str = params.get("cwd").and_then(Value::as_str);
            if let Err(e) = validate_cwd(cwd_str) {
                return responder.respond_with_error(e);
            }
            let initialize = match initialize_options_for(&params) {
                Ok(initialize) => initialize,
                Err(e) => return responder.respond_with_error(e),
            };
            let cwd = cwd_str.map(PathBuf::from);
            let (claude, permission_mode, allow_bypass) =
                spawn_inputs(&params, cwd.as_deref(), opts, None);
            let session_id = claude.resume.clone().unwrap_or_else(uuid_v4);
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
                .with_env(opts.env.clone());
            match Session::start(sess_opts, session_id.clone()).await {
                Ok((session, update_rx)) => {
                    publish_session_account(registry, auth_cwd, &session);
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
                    register_session(registry, &session_id, session, update_rx, &connection);
                    responder.respond(session_response(&session_id, &modes, &config_options))?;
                    let update = map::map_commands_changed(&json!(commands), &json!([]));
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
                Err(e) => responder.respond_with_error(rpc_error(session_error_to_rpc(&e))),
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
                    let messages = crate::transcript::session_messages(&opts.env, &session_id);
                    for value in crate::transcript::replay_notifications(&messages, &session_id) {
                        if let Ok(msg) = UntypedMessage::new("session/update", value) {
                            let _ = connection.send_notification(msg);
                        }
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
                // `available_commands_update` after an already-live
                // `session/load` (`acp-agent.js:1593`), after the response.
                if replay {
                    let commands = session.commands().to_vec();
                    let update = map::map_commands_changed(&json!(commands), &json!([]));
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
                replay,
            )
            .await
        }
        "session/fork" => {
            // Node order: validate the systemPrompt, copy the transcript, then
            // start the new session like a resumed one (`unstable_forkSession`).
            if let Err(e) = initialize_options_for(&params) {
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
                false,
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
            let uuid = uuid_v4();
            let frame = prompt_to_claude(&params, &session_id, &uuid);
            // Enqueue the prompt on the actor synchronously so a later
            // `session/cancel` notification (processed on the same single-task
            // connection) is always ordered *after* it — the queued-sweep
            // ordering the cancel relies on. Only the settlement is awaited on
            // a spawned task so the connection is never blocked while a turn is
            // in flight (Node's `prompt()` returns immediately).
            let reply_rx = session.send_prompt(uuid, frame, is_local_only);
            let connection = connection.clone();
            tokio::spawn(async move {
                let reply = reply_rx.await.unwrap_or_else(|_| Err(SessionError::Closed));
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
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if session_id.is_empty() {
                return responder.respond_with_error(
                    Error::invalid_params().data("steer params require a non-empty sessionId"),
                );
            }
            let prompt = params.get("prompt").and_then(Value::as_array);
            if prompt.is_none_or(Vec::is_empty) {
                return responder.respond_with_error(
                    Error::invalid_params().data("steer params require a non-empty prompt array"),
                );
            }
            let idle_behavior = params
                .pointer("/_meta/steering/idleBehavior")
                .and_then(Value::as_str);
            // Reject an unsupported `idleBehavior` before routing to the actor
            // (the R32 invalidParams case; `steer_outcome` guards it too).
            if idle_behavior.is_some_and(|b| b != "promptRequired") {
                return responder.respond_with_error(
                    Error::invalid_params().data("unsupported steering idleBehavior"),
                );
            }
            let session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                None => {
                    return responder
                        .respond_with_error(Error::internal_error().data("Session not found"));
                }
            };
            let first_text = prompt
                .and_then(|a| a.first())
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let is_local_only = is_local_only_command(first_text);
            let is_prompt_required = idle_behavior == Some("promptRequired");
            let uuid = uuid_v4();
            let frame = prompt_to_claude(&params, &session_id, &uuid);
            let reply_rx = session.steer(uuid, frame, is_local_only, is_prompt_required);
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
        _ => {
            // Unhandled method (8.1): -32601. Mirror Node's SDK router
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
            .contains("[a.txt]"));

        // resource with text -> content + context
        let params = json!({"prompt":[{"type":"resource","resource":{"uri":"file:///b.txt","text":"body"}}]});
        let frame = prompt_to_claude(&params, "s1", "u");
        let content = frame["message"]["content"].as_array().unwrap();
        assert!(content
            .iter()
            .any(|c| c["text"].as_str().unwrap().contains("[b.txt]")));
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
        assert_eq!(
            v["message"],
            "`cwd` must be an absolute path, but received: relative/dir"
        );
        assert_eq!(v["data"]["cwd"], "relative/dir");

        let missing =
            std::env::temp_dir().join(format!("definitely-missing-cwd-{}", std::process::id()));
        let missing_str = missing.to_string_lossy().into_owned();
        let err = validate_cwd(Some(&missing_str)).expect_err("missing must be rejected");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], -32602);
        assert!(v["message"].as_str().unwrap().contains("does not exist"));
        assert_eq!(v["data"]["cwd"], missing_str);

        // A non-directory (a file) must be rejected as "not a directory".
        let file = std::env::temp_dir().join(format!("cwd-file-{}.txt", std::process::id()));
        std::fs::write(&file, "x").expect("write temp file");
        let file_str = file.to_string_lossy().into_owned();
        let err = validate_cwd(Some(&file_str)).expect_err("a file is not a directory");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], -32602);
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
}
