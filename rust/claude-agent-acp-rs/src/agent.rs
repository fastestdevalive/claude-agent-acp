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

use agent_client_protocol::schema::v1::{
    AvailableCommandsUpdate, SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::{Agent, ConnectTo, ConnectionTo, Error, Result, UntypedMessage};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::control::InitializeOptions;
use crate::process::{SpawnOptions, Timings};
use crate::session::{Session, SessionError, SessionOptions};

/// The `_meta`/agentCapabilities that match the Node adapter at v0.70.0.
///
/// These are constant wire values captured in `porting/fixtures/*.frames.jsonl`;
/// the differential ignores the volatile `modes`/`configOptions`/`authMethods`.
const AGENT_INFO: &str = "{\"name\":\"@agentclientprotocol/claude-agent-acp\",\"title\":\"Claude Agent\",\"version\":\"0.70.0\"}";

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
}

impl ServeOptions {
    /// Build options from the process environment: `CLAUDE_CODE_EXECUTABLE`
    /// (binary), `CLAUDE_CODE_DEFAULT_CWD` (cwd). Used by the stdio binary.
    pub fn from_env() -> Self {
        let claude_path = std::env::var_os("CLAUDE_CODE_EXECUTABLE").map(PathBuf::from);
        let default_cwd = std::env::var_os("CLAUDE_CODE_DEFAULT_CWD").map(PathBuf::from);
        Self {
            claude_path,
            default_cwd,
            ..Self::default()
        }
    }
}

/// The `initialize` response, byte-matching the Node adapter at v0.70.0
/// (modulo ignored volatile fields).
fn initialize_response() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "_meta": { "claudeCode": { "promptQueueing": true } },
            "promptCapabilities": { "image": true, "embeddedContext": true },
            "mcpCapabilities": { "http": true, "sse": true },
            "auth": { "logout": {} },
            "providers": {},
            "loadSession": true,
            "sessionCapabilities": {
                "additionalDirectories": {},
                "close": {},
                "delete": {},
                "fork": {},
                "list": {},
                "resume": {}
            }
        },
        "agentInfo": serde_json::from_str::<Value>(AGENT_INFO).unwrap_or(Value::Null),
        "authMethods": [],
        "_meta": {
            "jetbrains": { "air": { "version": 1, "capabilities": ["sessionFailure", "agentFileChangeReport"] } },
            "steering": { "supported": true },
            "goal": { "version": 1, "controlMethod": "_session/goal", "actions": ["set", "clear"] }
        }
    })
}

/// The `session/new` and `session/load` response body (modes/configOptions are
/// ignored by the differential).
fn session_response(session_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "modes": {
            "currentModeId": "default",
            "availableModes": [
                {"id": "default", "name": "Manual", "description": "Standard behavior, prompts for dangerous operations"},
                {"id": "acceptEdits", "name": "Accept Edits", "description": "Auto-accept file edit operations"},
                {"id": "plan", "name": "Plan Mode", "description": "Planning mode, no actual tool execution"},
                {"id": "dontAsk", "name": "Don't Ask", "description": "Don't prompt for permissions, deny if not pre-approved"},
                {"id": "bypassPermissions", "name": "Bypass Permissions", "description": "Bypass all permission checks"}
            ]
        },
        "configOptions": []
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

/// Build the child [`SpawnOptions`] for a session (8.4).
///
/// `cwd`, `model` and `permission_mode` come from the ACP request; `session_id`
/// is the minted uuid (or resumed id); a default `cwd` falls back to
/// `ServeOptions.default_cwd` then the process cwd.
pub fn session_spawn_options(
    serve: &ServeOptions,
    cwd: Option<PathBuf>,
    session_id: String,
    model: Option<String>,
    permission_mode: Option<String>,
) -> SpawnOptions {
    let cwd = cwd
        .or_else(|| serve.default_cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    SpawnOptions {
        claude_path: serve.claude_path.clone(),
        extra_env: serve.extra_env.clone(),
        default_cwd: cwd,
        session_id: Some(session_id),
        model,
        permission_mode,
        replay_user_messages: true,
        include_partial_messages: true,
        ..SpawnOptions::default()
    }
}

/// Read `model` / `permission_mode` from `_meta.claudeCode.options` if present.
fn options_from_meta(params: &Value) -> (Option<String>, Option<String>) {
    let opts = params
        .get("_meta")
        .and_then(|m| m.get("claudeCode"))
        .and_then(|c| c.get("options"));
    let model = opts
        .and_then(|o| o.get("model"))
        .and_then(Value::as_str)
        .map(String::from);
    let permission = opts
        .and_then(|o| o.get("permissionMode"))
        .and_then(Value::as_str)
        .map(String::from);
    (model, permission)
}

/// Map a [`SessionError`] to the JSON-RPC error's `(message, data)` (review-04
/// wiring: the actor maps `AuthRequired` → authRequired, else `internalError`
/// with `errorKind` data). The message is the full `"Internal error: <detail>"`
/// wire text; `data` is `None` when upstream omits it (e.g. `session ended`).
fn session_error_to_rpc(err: &SessionError) -> (String, Option<Value>) {
    use crate::turn::FailureKind::*;
    let (message, data) = match err {
        SessionError::PromptFailed { kind, message } => {
            let data = match kind {
                AuthRequired => None,
                ProviderError => Some(json!({"errorKind": "provider_error"})),
                BudgetExhausted => Some(json!({"errorKind": "budget_exhausted"})),
                ContextExhausted => Some(json!({"errorKind": "context_exhausted"})),
                NoResult => Some(json!({"errorKind": "no_result"})),
                SessionEnded => None,
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
}

/// Register a new session: keep its handle in the registry, publish it to the
/// notification handler (lock-free), and spawn a task that forwards its updates
/// to the client as they arrive. The actor emits a `FinalText` chunk before
/// resolving the prompt's oneshot, so the chunk lands ahead of the response.
fn register_session(
    registry: &mut Registry,
    session_id: &str,
    session: Session,
    update_rx: mpsc::UnboundedReceiver<SessionNotification>,
    connection: &ConnectionTo<agent_client_protocol::Client>,
) {
    registry
        .sessions
        .insert(session_id.to_string(), session.clone());
    let _ = registry.session_tx.send((session_id.to_string(), session));
    let connection = connection.clone();
    tokio::spawn(async move {
        let mut update_rx = update_rx;
        while let Some(notif) = update_rx.recv().await {
            let _ = connection.send_notification(notif);
        }
    });
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
            responder.respond(initialize_response())
        }
        "session/new" => {
            let session_id = uuid_v4();
            let cwd = params.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            let (model, permission_mode) = options_from_meta(&params);
            let spawn =
                session_spawn_options(opts, cwd, session_id.clone(), model, permission_mode);
            let sess_opts =
                SessionOptions::new(spawn, opts.timings.clone(), InitializeOptions::default());
            match Session::start(sess_opts, session_id.clone()).await {
                Ok((session, update_rx)) => {
                    register_session(registry, &session_id, session, update_rx, &connection);
                    responder.respond(session_response(&session_id))?;
                    // Upstream emits an empty available_commands_update right
                    // after session/new / session/load via setTimeout (parity);
                    // it lands after the response.
                    let _ = connection.send_notification(empty_commands_update(&session_id));
                    Ok(())
                }
                Err(e) => responder.respond_with_error(rpc_error(session_error_to_rpc(&e))),
            }
        }
        "session/load" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let cwd = params.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            let (model, permission_mode) = options_from_meta(&params);
            let mut spawn =
                session_spawn_options(opts, cwd, session_id.clone(), model, permission_mode);
            spawn.resume = Some(session_id.clone());
            spawn.session_id = None;
            let sess_opts =
                SessionOptions::new(spawn, opts.timings.clone(), InitializeOptions::default());
            match Session::start(sess_opts, session_id.clone()).await {
                Ok((session, update_rx)) => {
                    register_session(registry, &session_id, session, update_rx, &connection);
                    responder.respond(session_response(&session_id))?;
                    // Upstream emits an empty available_commands_update right
                    // after session/new / session/load via setTimeout (parity);
                    // it lands after the response.
                    let _ = connection.send_notification(empty_commands_update(&session_id));
                    Ok(())
                }
                Err(e) => responder.respond_with_error(rpc_error(session_error_to_rpc(&e))),
            }
        }
        "session/prompt" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let session = match registry.sessions.get(&session_id) {
                Some(s) => s.clone(),
                None => {
                    return responder
                        .respond_with_error(Error::invalid_params().data("no such session"));
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
                        let _ = responder.respond(result);
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(rpc_error(session_error_to_rpc(&e)));
                    }
                }
            });
            Ok(())
        }
        _ => {
            // Unhandled method (8.1): -32601.
            responder.respond_with_error(
                Error::method_not_found().data(format!("method {method} not found")),
            )
        }
    }
}

/// An empty `available_commands_update` notification (upstream emits one after
/// `session/new` / `session/load`).
fn empty_commands_update(session_id: &str) -> SessionNotification {
    SessionNotification::new(
        session_id.to_string(),
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![])),
    )
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
            Some("claude-sonnet-4-5".to_string()),
            Some("acceptEdits".to_string()),
        );
        let argv = crate::process::build_argv(&spawn);
        assert_eq!(session_id_from_argv(&argv), Some("uu-1234"));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--model", "claude-sonnet-4-5"]),
            "model -> --model: {argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["--permission-mode", "acceptEdits"]),
            "permission mode -> --permission-mode: {argv:?}"
        );
    }

    #[test]
    fn resume_maps_to_resume_flag() {
        let serve = ServeOptions::default();
        let mut spawn = session_spawn_options(&serve, None, "sess-id".to_string(), None, None);
        spawn.resume = Some("sess-id".to_string());
        spawn.session_id = None;
        let argv = crate::process::build_argv(&spawn);
        assert_eq!(resume_from_argv(&argv), Some("sess-id"));
        assert!(session_id_from_argv(&argv).is_none());
    }
}
