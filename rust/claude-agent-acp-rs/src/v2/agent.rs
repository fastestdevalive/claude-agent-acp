//! The ACP v2 agent surface served by one connection (M-106, M-108, M-109,
//! M-110, M-111, M-112, M-117, M-118, Q-83, C-24; upstream `src/v2/agent.ts`).
//!
//! `ClaudeAcpAgent` speaks ACP v1 types. This surface translates each v2
//! request into the v1 request that the agent serves, and its v1 response into
//! the v2 response, so v1 and v2 share one implementation. A v1 answer is
//! captured through [`Reply::Capture`] instead of going on the wire, so the
//! translation can add what v2 requires (`configId`, `availableCommands`, the
//! foreground-state snapshot) and drop what v2 forbids (`modes`).
//!
//! A v1 request may be answered by a task the handler spawns (a changed-params
//! `resume`, a `fork`), so the capture is awaited off the registry actor: the
//! answer, its translation and the response run in a spawned task, and the
//! actor serves the next request meanwhile.
//!
//! The v2 messages of the connection's sessions are translated by one task per
//! connection ([`V2Outbound`], upstream `V2ClientConnection`): it owns the
//! display terminals, the tool-call `_meta` store and the replay state, and it
//! writes the updates and the answers that must follow them.

use std::collections::{HashMap, HashSet};

use agent_client_protocol::{ConnectionTo, Error, Responder};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::agent::{handle_v1_request, Registry, Reply};
use crate::map;
use crate::session::Session;
use crate::turn_events::{empty_message, message_of_chunk};

use super::initialize::reject_v2_client_fs;
use super::permission::v2_permission_request;
use super::session::{
    v1_fork_session_request, v1_new_session_request, v1_restore_session_request,
    v1_set_session_config_option_request, v2_available_commands, v2_config_options,
    v2_new_session_response, v2_resume_session_response,
};
use super::session_update::{v2_child_state_update, v2_session_update};
use super::terminal::V2Terminals;
use super::tool_call_meta::V2ToolCallMeta;

/// The methods of the v2 agent surface that the router serves itself
/// (`v2/agent.ts:65-132`). `initialize` is served by the v1 handler, which
/// negotiates the protocol and answers a v2 handshake already.
fn is_v2_method(method: &str) -> bool {
    matches!(
        method,
        "session/new"
            | "session/list"
            | "session/resume"
            | "session/fork"
            | "session/close"
            | "session/delete"
            | "session/set_config_option"
            | "session/setConfigOption"
            | "session/prompt"
            | "auth/login"
            | "auth/logout"
            | "providers/list"
            | "providers/set"
            | "providers/disable"
    )
}

/// The v1 method of a v2 provider/auth method (`v2/agent.ts:68-76`). These pass
/// through unchanged: their request and response types are the same in v1 and
/// v2, so no translation is needed and the answer goes straight on the wire.
fn v1_passthrough_method(method: &str) -> Option<&'static str> {
    match method {
        "auth/login" => Some("authenticate"),
        "auth/logout" => Some("logout"),
        "providers/list" => Some("providers/list"),
        "providers/set" => Some("providers/set"),
        "providers/disable" => Some("providers/disable"),
        _ => None,
    }
}

/// The `state_update` that reports `state` as the session's current foreground
/// state (`v2/agent.ts:136-140`).
fn state_snapshot(state: crate::turn_events::ForegroundState) -> Value {
    super::prompt::foreground_state_update(state)
}

/// Serve one request of a negotiated v2 connection (M-106).
///
/// A v2 request becomes the v1 request the agent serves; the v1 answer is
/// translated back. A method that the surface does not serve, and an extension
/// method that does not start with "_" (M-118), get the upstream error rather
/// than a v1 shape.
pub(crate) async fn handle_v2_request(
    registry: &mut Registry,
    method: &str,
    params: Value,
    opts: &crate::agent::ServeOptions,
    responder: Responder<Value>,
    connection: ConnectionTo<agent_client_protocol::Client>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<(), Error> {
    if !is_v2_method(method) {
        // Upstream's v2 agent app registers the listed methods only; every
        // other request, including an extension method that is not tagged
        // `_`-prefixed (M-118, `v2/agent.ts:245-250`), is not found.
        return responder.respond_with_error(
            Error::new(-32601, format!("\"Method not found\": {method}"))
                .data(json!({ "method": method })),
        );
    }

    // The provider and auth methods answer the v1 shape unchanged
    // (`v2/agent.ts:68-76`): their v1 handler is called with the wire responder,
    // because `providers/set`/`providers/disable` hand the responder to a
    // spawned task that must answer the client itself.
    if let Some(v1_method) = v1_passthrough_method(method) {
        let request = agent_client_protocol::UntypedMessage::new(v1_method, params)?;
        return handle_v1_request(
            registry,
            request,
            opts,
            Reply::Wire(responder),
            connection,
            permit,
        )
        .await;
    }

    // The v1 request of each v2 method (`v2/session.ts`). A rejected request
    // never reaches the agent.
    let v1 = match v1_request_for(method, &params) {
        Ok(v1) => v1,
        Err(error) => return responder.respond_with_error(error),
    };

    let v2 = registry.v2_outbound(&connection);

    // A prompt is served through the agent's turn events (`v2/prompt.ts`): the
    // v1 handler answers it at intake, from the task it spawns.
    if v1.method == "session/prompt" {
        let request = agent_client_protocol::UntypedMessage::new(v1.method, v1.params.clone())?;
        return handle_v1_request(
            registry,
            request,
            opts,
            Reply::Wire(responder),
            connection,
            permit,
        )
        .await;
    }

    // v2 gets the session's commands in the setup response, so the v1
    // `available_commands_update` that follows it must not go out
    // (`v2/agent.ts:150-162`; `ClaudeAcpAgent.afterSetupResponse`).
    let sends_commands_update = matches!(
        v1.method,
        "session/new" | "session/load" | "session/resume" | "session/fork"
    );
    if sends_commands_update {
        registry.set_suppress_commands_update(true);
    }
    let sessions_view = registry.sessions_view();
    let (tx, rx) = mpsc::unbounded_channel();
    let request = agent_client_protocol::UntypedMessage::new(v1.method, v1.params.clone())?;
    if let Err(error) = handle_v1_request(
        registry,
        request,
        opts,
        Reply::Capture(tx),
        connection.clone(),
        permit,
    )
    .await
    {
        if sends_commands_update {
            registry.set_suppress_commands_update(false);
        }
        return responder.respond_with_error(error);
    }
    if sends_commands_update {
        registry.set_suppress_commands_update(false);
    }
    // The answer may still be ahead: a `load`/`resume` whose params changed
    // starts the session in a spawned task. The translation and the response
    // run in one too, so the actor is free to serve a `session/close` that
    // cancels that start.
    let opts = opts.clone();
    let method = v1.method;
    let params = v1.params;
    tokio::spawn(async move {
        let mut rx = rx;
        let v1_response = match rx.recv().await {
            Some(Ok(value)) => value,
            Some(Err(error)) => return v2.respond_error(responder, error),
            None => {
                return v2.respond_error(
                    responder,
                    Error::internal_error().data(json!({
                        "details": format!("{method} was not answered"),
                    })),
                )
            }
        };
        let response =
            finish_v2_response(&v2, method, &params, &v1_response, &opts, &sessions_view).await;
        v2.respond(responder, response);
    });
    Ok(())
}

/// The v1 method and request a v2 method means (`v2/session.ts`).
#[derive(Debug)]
pub(super) struct V1Request {
    pub(super) method: &'static str,
    pub(super) params: Value,
}

pub(super) fn v1_request_for(method: &str, params: &Value) -> Result<V1Request, Error> {
    match method {
        "session/new" => Ok(V1Request {
            method: "session/new",
            params: v1_new_session_request(params)?,
        }),
        "session/resume" => {
            let restore = v1_restore_session_request(params)?;
            Ok(V1Request {
                method: restore.method,
                params: restore.request,
            })
        }
        "session/fork" => Ok(V1Request {
            method: "session/fork",
            params: v1_fork_session_request(params)?,
        }),
        "session/set_config_option" | "session/setConfigOption" => Ok(V1Request {
            method: "session/set_config_option",
            params: v1_set_session_config_option_request(params)?,
        }),
        "session/prompt" => {
            // A v2 prompt may hold content blocks that v1 cannot express; such a
            // block is rejected before the turn starts (`v2/prompt.ts:148-167`).
            // The v2 prompt array rides along, because the `user_message` the
            // client gets echoes it (`v2/prompt.ts:44-47`).
            let mut request = crate::v2::prompt::v1_prompt_request(params)?;
            if let Some(object) = request.as_object_mut() {
                object.insert(
                    "_v2Prompt".to_string(),
                    params.get("prompt").cloned().unwrap_or(Value::Null),
                );
            }
            Ok(V1Request {
                method: "session/prompt",
                params: request,
            })
        }
        other => Ok(V1Request {
            method: match other {
                "session/list" => "session/list",
                "session/close" => "session/close",
                "session/delete" => "session/delete",
                _ => "session/list",
            },
            params: params.clone(),
        }),
    }
}

/// Translate the captured v1 answer of `method` into the v2 response
/// (`v2/agent.ts:65-132`), after the updates that must precede it.
async fn finish_v2_response(
    v2: &V2OutboundHandle,
    method: &str,
    params: &Value,
    v1_response: &Value,
    opts: &crate::agent::ServeOptions,
    sessions_view: &watch::Receiver<HashMap<String, (Session, Value)>>,
) -> Result<Value, Error> {
    match method {
        "session/new" => {
            let mut response = v2_new_session_response(v1_response);
            let session_id = session_id_of(v1_response).to_string();
            if let Some(commands) = available_commands(sessions_view, opts, &session_id).await {
                if let Value::Object(map) = &mut response {
                    map.insert("availableCommands".to_string(), Value::Array(commands));
                }
            }
            Ok(response)
        }
        "session/resume" | "session/load" => {
            // v2 requires a snapshot of the foreground state after the replay
            // and before the response (`v2/agent.ts:91-97`).
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            send_state_snapshot(v2, sessions_view, &session_id).await;
            let mut response = v2_resume_session_response(v1_response);
            if let Some(commands) = available_commands(sessions_view, opts, &session_id).await {
                if let Value::Object(map) = &mut response {
                    map.insert("availableCommands".to_string(), Value::Array(commands));
                }
            }
            Ok(response)
        }
        "session/fork" => {
            let session_id = session_id_of(v1_response).to_string();
            // `{ sessionId, ...v2ResumeSessionResponse(response), ...commands }`
            // (`v2/agent.ts:103-113`): `sessionId` comes first.
            let mut response = serde_json::Map::new();
            response.insert("sessionId".to_string(), Value::String(session_id.clone()));
            if let Value::Object(rest) = v2_resume_session_response(v1_response) {
                for (key, value) in rest {
                    response.insert(key, value);
                }
            }
            if let Some(commands) = available_commands(sessions_view, opts, &session_id).await {
                response.insert("availableCommands".to_string(), Value::Array(commands));
            }
            Ok(Value::Object(response))
        }
        "session/set_config_option" => {
            // The response's config options are v2-shaped (O-86,
            // `v2/agent.ts:121-126`).
            let mut response = match v1_response {
                Value::Object(map) => map.clone(),
                other => return Ok(other.clone()),
            };
            let config_options = response
                .shift_remove("configOptions")
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            response.insert(
                "configOptions".to_string(),
                Value::Array(v2_config_options(&config_options)),
            );
            Ok(Value::Object(response))
        }
        "session/close" => {
            // The stores of the session go with it (`v2/agent.ts:114-120`).
            // NEW-8: upstream forgets them on `close` only; `delete` answers the
            // v1 response as it is.
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            v2.forget(&session_id);
            Ok(v1_response.clone())
        }
        // `session/list` answers the same shape in both versions
        // (`v2/agent.ts:84`).
        _ => Ok(v1_response.clone()),
    }
}

/// The `sessionId` of a v1 session response, if any.
fn session_id_of(response: &Value) -> &str {
    response
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Send the foreground-state snapshot of `session_id` (`v2/agent.ts:94-97`),
/// read from the session's own tracker (`foregroundState`).
async fn send_state_snapshot(
    v2: &V2OutboundHandle,
    sessions_view: &watch::Receiver<HashMap<String, (Session, Value)>>,
    session_id: &str,
) {
    let session = sessions_view
        .borrow()
        .get(session_id)
        .map(|(s, _)| s.clone());
    let Some(session) = session else {
        return;
    };
    let Some(state) = session.foreground_state().await else {
        return;
    };
    v2.translate(session_id, state_snapshot(state));
}

/// The `availableCommands` of a setup response, in v2 form
/// (`v2/agent.ts:150-162`). An empty list is left out; a failure to read the
/// commands does not fail the setup.
async fn available_commands(
    sessions_view: &watch::Receiver<HashMap<String, (Session, Value)>>,
    opts: &crate::agent::ServeOptions,
    session_id: &str,
) -> Option<Vec<Value>> {
    let session = sessions_view
        .borrow()
        .get(session_id)
        .map(|(s, _)| s.clone())?;
    // `agent.availableCommands(sessionId)`: the session's live command list,
    // as the v1 `available_commands_update` carries it.
    let commands = session.live_commands().await;
    let update = map::map_commands_changed_with_skills(
        &json!(commands),
        &[],
        None,
        opts.env.get_path("HOME").as_deref(),
    );
    let v1 = map::available_commands_json(&update);
    let commands = v1.get("availableCommands").and_then(Value::as_array)?;
    let v2 = v2_available_commands(commands);
    if v2.is_empty() {
        None
    } else {
        Some(v2)
    }
}

/// The v2 `initialize` answer already comes from the v1 handler; this is the
/// fs rejection the v2 client connection reports (C-47).
pub fn v2_fs_error() -> Error {
    reject_v2_client_fs()
}

/// A message to the connection's v2 outbound task.
pub(crate) enum V2OutboundMsg {
    /// Translate and send one v1 `session/update` of `session_id`.
    Translate { session_id: String, update: Value },
    /// A replay of `session_id` starts: the client's replayed messages are
    /// cleared before their first chunk (`replaying`, `v2/agent.ts:209-241`).
    BeginReplay { session_id: String },
    /// The replay ended: the replayed subagents' last work states go out.
    EndReplay { session_id: String },
    /// Forget the session's terminals and tool-call `_meta` (`v2/agent.ts:116-117`).
    Forget { session_id: String },
    /// The v2 form of a permission request (`requestPermission`,
    /// `v2/agent.ts:347-357`): its subject carries the stored `_meta`. The
    /// outbound task writes the request itself and hands the response future
    /// back unawaited, so the request keeps its FIFO position among the
    /// connection's updates (NEW-6).
    Permission {
        session_id: String,
        params: Value,
        reply: oneshot::Sender<Result<agent_client_protocol::SentRequest<Value>, Error>>,
    },
    /// Answer a request after every update sent before it.
    Respond {
        responder: Responder<Value>,
        response: Result<Value, Error>,
    },
    /// The responder of a v2 prompt, registered before the prompt starts so its
    /// answer is written in the position the session emits it (`v2Prompt`'s
    /// `inserted`, `v2/prompt.ts:35-52`).
    RegisterAnswer {
        prompt_uuid: String,
        responder: Responder<Value>,
    },
    /// The intake answer of a v2 prompt (`inserted`/`ended`/`failed`).
    Answer {
        prompt_uuid: String,
        result: Result<String, crate::turn_events::V2IntakeError>,
    },
}

/// The handle of the connection's v2 outbound task.
#[derive(Clone)]
pub(crate) struct V2OutboundHandle {
    tx: mpsc::UnboundedSender<V2OutboundMsg>,
}

impl V2OutboundHandle {
    /// Translate and send a v1 update of `session_id`, in order with the other
    /// messages of the task.
    pub(crate) fn translate(&self, session_id: &str, update: Value) {
        let _ = self.tx.send(V2OutboundMsg::Translate {
            session_id: session_id.to_string(),
            update,
        });
    }

    /// Start a replay of `session_id`.
    pub(crate) fn begin_replay(&self, session_id: &str) {
        let _ = self.tx.send(V2OutboundMsg::BeginReplay {
            session_id: session_id.to_string(),
        });
    }

    /// End a replay of `session_id`; its replayed subagents' states follow.
    pub(crate) fn end_replay(&self, session_id: &str) {
        let _ = self.tx.send(V2OutboundMsg::EndReplay {
            session_id: session_id.to_string(),
        });
    }

    /// Forget what the connection stores of `session_id`.
    pub(crate) fn forget(&self, session_id: &str) {
        let _ = self.tx.send(V2OutboundMsg::Forget {
            session_id: session_id.to_string(),
        });
    }

    /// Queue the v2 form of a permission request, with the stored `_meta` of its
    /// tool call. The message is queued synchronously, so the request keeps its
    /// FIFO position among the connection's updates; the returned receiver
    /// resolves with the request already written and its response future
    /// unawaited (NEW-6).
    pub(crate) fn permission(
        &self,
        session_id: &str,
        params: Value,
    ) -> oneshot::Receiver<Result<agent_client_protocol::SentRequest<Value>, Error>> {
        let (reply, done) = oneshot::channel();
        let _ = self.tx.send(V2OutboundMsg::Permission {
            session_id: session_id.to_string(),
            params,
            reply,
        });
        done
    }

    /// Answer `responder` with `response`, after every update ahead of it.
    pub(crate) fn respond(&self, responder: Responder<Value>, response: Result<Value, Error>) {
        let _ = self.tx.send(V2OutboundMsg::Respond {
            responder,
            response,
        });
    }

    /// Answer `responder` with an error, after every update ahead of it.
    pub(crate) fn respond_error(&self, responder: Responder<Value>, error: Error) {
        self.respond(responder, Err(error));
    }

    /// Register the responder of the v2 prompt `prompt_uuid` before it starts.
    pub(crate) fn register_answer(&self, prompt_uuid: &str, responder: Responder<Value>) {
        let _ = self.tx.send(V2OutboundMsg::RegisterAnswer {
            prompt_uuid: prompt_uuid.to_string(),
            responder,
        });
    }

    /// Answer the v2 prompt `prompt_uuid` at intake. The first answer wins: a
    /// second one (the prompt task and the session both report it) is dropped.
    pub(crate) fn answer(
        &self,
        prompt_uuid: &str,
        result: Result<String, crate::turn_events::V2IntakeError>,
    ) {
        let _ = self.tx.send(V2OutboundMsg::Answer {
            prompt_uuid: prompt_uuid.to_string(),
            result,
        });
    }
}

/// Start the connection's v2 outbound task (`V2ClientConnection`,
/// `v2/agent.ts:174-402`).
pub(crate) fn spawn_v2_outbound(
    connection: ConnectionTo<agent_client_protocol::Client>,
    logger: Option<std::sync::Arc<crate::logger::Logger>>,
) -> V2OutboundHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<V2OutboundMsg>();
    tokio::spawn(async move {
        let mut out = V2Outbound::new().with_logger(logger);
        while let Some(msg) = rx.recv().await {
            match msg {
                V2OutboundMsg::Translate { session_id, update } => {
                    for notification in out.translate(&session_id, update) {
                        send_notification(&connection, notification);
                    }
                }
                V2OutboundMsg::BeginReplay { session_id } => out.begin_replay(&session_id),
                V2OutboundMsg::EndReplay { session_id } => {
                    for notification in out.end_replay(&session_id) {
                        send_notification(&connection, notification);
                    }
                }
                V2OutboundMsg::Forget { session_id } => out.forget(&session_id),
                V2OutboundMsg::Permission {
                    session_id,
                    params,
                    reply,
                } => {
                    // NEW-6: the request is written here, in FIFO order with the
                    // updates around it; the response future goes back unawaited
                    // (`requestPermission`, `v2/agent.ts:347-357`).
                    let sent = match out.permission_request(&session_id, &params) {
                        Ok(params) => match agent_client_protocol::UntypedMessage::new(
                            "session/request_permission",
                            params,
                        ) {
                            Ok(msg) => Ok(connection.send_request(msg)),
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(sent);
                }
                V2OutboundMsg::Respond {
                    responder,
                    response,
                } => match response {
                    Ok(value) => {
                        let _ = responder.respond(value);
                    }
                    Err(error) => {
                        let _ = responder.respond_with_error(error);
                    }
                },
                V2OutboundMsg::RegisterAnswer {
                    prompt_uuid,
                    responder,
                } => {
                    out.answers.insert(prompt_uuid, responder);
                }
                V2OutboundMsg::Answer {
                    prompt_uuid,
                    result,
                } => {
                    if let Some(responder) = out.answers.remove(&prompt_uuid) {
                        match result {
                            Ok(message_id) => {
                                let _ = responder.respond(json!({ "messageId": message_id }));
                            }
                            Err(error) => {
                                let _ = responder
                                    .respond_with_error(crate::v2::prompt::intake_error(&error));
                            }
                        }
                    }
                }
            }
        }
    });
    V2OutboundHandle { tx }
}

/// Send one `session/update` notification; a failure is logged by omission, as
/// upstream logs rather than rejects (`send`, `v2/agent.ts:243-249`).
fn send_notification(
    connection: &ConnectionTo<agent_client_protocol::Client>,
    notification: Value,
) {
    if let Ok(msg) = agent_client_protocol::UntypedMessage::new("session/update", notification) {
        let _ = connection.send_notification(msg);
    }
}

/// The per-connection v2 stores and the translation of one v1 update
/// (`V2ClientConnection`, `v2/agent.ts:174-402`).
#[derive(Default)]
pub(crate) struct V2Outbound {
    /// The display terminals of the commands the agent runs.
    terminals: V2Terminals,
    /// The `_meta` the client stores for each tool call.
    tool_call_meta: V2ToolCallMeta,
    /// The messages a replay cleared so far, per replaying session.
    replays: HashMap<String, HashSet<String>>,
    /// The session whose replay announced each replaying subagent.
    replay_roots: HashMap<String, String>,
    /// The last work state that a replay reported for each subagent, sent after
    /// the replay ends.
    replayed_child_states: HashMap<String, Value>,
    /// The responders of the v2 prompts whose intake is not answered yet.
    answers: HashMap<String, Responder<Value>>,
    /// Where a translation failure is reported: upstream's `send` logs
    /// `Failed to send a … update` rather than dropping the update silently
    /// (`v2/agent.ts:243-249`; NEW-9).
    logger: Option<std::sync::Arc<crate::logger::Logger>>,
}

impl V2Outbound {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Attach the connection's logger.
    pub(crate) fn with_logger(
        mut self,
        logger: Option<std::sync::Arc<crate::logger::Logger>>,
    ) -> Self {
        self.logger = logger;
        self
    }

    /// Log a failed translation of `update` (`send`'s `catch`, NEW-9).
    fn log_translation_error(&self, session_id: &str, update: &Value, error: &Error) {
        if let Some(logger) = &self.logger {
            let kind = update
                .get("sessionUpdate")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            logger.error(&format!(
                "Session {session_id}: failed to send a {kind} update: {error}"
            ));
        }
    }

    /// Start a replay of `session_id`.
    pub(crate) fn begin_replay(&mut self, session_id: &str) {
        self.replays.entry(session_id.to_string()).or_default();
    }

    /// End the replay of `session_id`, returning the notifications of the
    /// subagents it replayed (`replaying`, `v2/agent.ts:209-241`).
    pub(crate) fn end_replay(&mut self, session_id: &str) -> Vec<Value> {
        let children: Vec<String> = self
            .replay_roots
            .iter()
            .filter(|(_, root)| root.as_str() == session_id)
            .map(|(child, _)| child.clone())
            .collect();
        let mut out = Vec::new();
        for child in &children {
            if let Some(state) = self.replayed_child_states.get(child) {
                out.push(state.clone());
            }
        }
        self.replays.remove(session_id);
        for child in &children {
            self.replays.remove(child);
            self.replay_roots.remove(child);
            self.replayed_child_states.remove(child);
        }
        out
    }

    /// Forget what the connection stores of `session_id`.
    pub(crate) fn forget(&mut self, session_id: &str) {
        self.terminals.forget(session_id);
        self.tool_call_meta.forget(session_id);
    }

    /// The v2 form of a permission request of `session_id`: its subject is the
    /// tool call with the `_meta` the client stores (`requestPermission`,
    /// `v2/agent.ts:347-357`).
    pub(crate) fn permission_request(
        &mut self,
        session_id: &str,
        params: &Value,
    ) -> Result<Value, Error> {
        let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let subject = self.tool_call_meta.subject(session_id, &tool_call);
        let mut request = params.clone();
        if let Some(object) = request.as_object_mut() {
            object.insert("toolCall".to_string(), subject);
        }
        v2_permission_request(&request)
    }

    /// The v2 notifications of one v1 `session/update` of `session_id`
    /// (`sessionUpdate`, `v2/agent.ts:252-303`).
    pub(crate) fn translate(&mut self, session_id: &str, update: Value) -> Vec<Value> {
        // The prompt's `user_message` and the work's `state_update` are v2's
        // own emissions (`v2Prompt`/`v2Foreground`), not v1 updates that need
        // translating: they go out as they are.
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "user_message" || kind == "state_update" {
            return vec![json!({ "sessionId": session_id, "update": update })];
        }
        // A subagent that a replay announces replays too, so its messages are
        // cleared before their first replayed chunk, as the parent's are.
        if update.get("sessionUpdate").and_then(Value::as_str) == Some("subagent_update")
            && self.replays.contains_key(session_id)
        {
            if let Some(child) = update.get("sessionId").and_then(Value::as_str) {
                if !self.replays.contains_key(child) {
                    self.replays.insert(child.to_string(), HashSet::new());
                    let root = self
                        .replay_roots
                        .get(session_id)
                        .cloned()
                        .unwrap_or_else(|| session_id.to_string());
                    self.replay_roots.insert(child.to_string(), root);
                }
            }
        }
        let mut notifications = Vec::new();
        for v2_update in self.v2_updates(session_id, &update) {
            if let Some(clear) = self.replay_clear(session_id, &v2_update) {
                notifications.push(json!({ "sessionId": session_id, "update": clear }));
            }
            notifications.push(json!({ "sessionId": session_id, "update": v2_update }));
        }
        // A subagent's work state also goes to the child's own stream, after
        // the parent's copy. During a replay, only its last state goes, after
        // the replay.
        let child_state = v2_child_state_update(&update).ok().flatten();
        if let Some(child_state) = child_state {
            let child = child_state
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if self.replays.contains_key(session_id) {
                self.replayed_child_states.insert(child, child_state);
            } else {
                notifications.push(child_state);
            }
        }
        notifications
    }

    /// The v2 updates of a v1 update: for a tool call report, its terminal
    /// updates first, then the report with the `_meta` keys it changes, and the
    /// `_meta` patch of a replayed start (`v2Updates`, `v2/agent.ts:305-336`).
    fn v2_updates(&mut self, session_id: &str, update: &Value) -> Vec<Value> {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind != "tool_call" && kind != "tool_call_update" {
            return match v2_session_update(update) {
                Ok(Some(update)) => vec![update],
                Ok(None) => Vec::new(),
                // NEW-9: upstream logs the failed send instead of dropping the
                // update silently (`send`'s `catch`, `v2/agent.ts:243-249`).
                Err(error) => {
                    self.log_translation_error(session_id, update, &error);
                    Vec::new()
                }
            };
        }
        let split = self.terminals.split(session_id, update);
        let Some(report) = split.report else {
            return split.terminal;
        };
        let starts = report.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call");
        let replaying = self.replays.contains_key(session_id);
        let reports = self
            .tool_call_meta
            .apply(session_id, &report, starts, replaying);
        let mut out = split.terminal;
        for (index, reported) in reports.iter().enumerate() {
            let v2_update = if index == 0 {
                match v2_session_update(reported) {
                    Ok(update) => update,
                    Err(error) => {
                        self.log_translation_error(session_id, reported, &error);
                        None
                    }
                }
            } else {
                // The second report is the `_meta` patch of a replayed start,
                // wrapped as a `tool_call_update` (`v2/agent.ts:326-333`).
                let mut wrapped = reported.as_object().cloned().unwrap_or_default();
                wrapped.insert(
                    "sessionUpdate".to_string(),
                    Value::String("tool_call_update".to_string()),
                );
                match v2_session_update(&Value::Object(wrapped)) {
                    Ok(update) => update,
                    Err(error) => {
                        self.log_translation_error(session_id, reported, &error);
                        None
                    }
                }
            };
            if let Some(v2_update) = v2_update {
                out.push(v2_update);
            }
        }
        out
    }

    /// The update that clears the message of a replayed chunk, when it is the
    /// first replayed chunk of that message (`replayClear`,
    /// `v2/agent.ts:336-352`).
    fn replay_clear(&mut self, session_id: &str, update: &Value) -> Option<Value> {
        let cleared = self.replays.get_mut(session_id)?;
        let kind = message_of_chunk(update.get("sessionUpdate").and_then(Value::as_str)?)?;
        let message_id = update.get("messageId").and_then(Value::as_str)?;
        if !cleared.insert(format!("{kind} {message_id}")) {
            return None;
        }
        Some(empty_message(kind, message_id))
    }
}

#[cfg(test)]
mod t6g2_tests {
    use super::*;

    /// NEW-8/M6: the stores are forgotten on `session/close` only
    /// (`v2/agent.ts:114-120`); `session/delete` answers as it is.
    #[tokio::test]
    async fn close_forgets_the_stores_and_delete_does_not() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = V2OutboundHandle { tx };
        let (_watch_tx, view) = watch::channel(HashMap::new());
        let opts = crate::agent::ServeOptions::default();
        let params = json!({"sessionId": "s"});

        let response =
            finish_v2_response(&handle, "session/close", &params, &json!({}), &opts, &view)
                .await
                .expect("the close answers");
        assert_eq!(response, json!({}));
        match rx.try_recv() {
            Ok(V2OutboundMsg::Forget { session_id }) => assert_eq!(session_id, "s"),
            Ok(_) | Err(_) => panic!("a close forgets the session's stores"),
        }

        let response =
            finish_v2_response(&handle, "session/delete", &params, &json!({}), &opts, &view)
                .await
                .expect("the delete answers");
        assert_eq!(response, json!({}));
        assert!(
            rx.try_recv().is_err(),
            "a delete keeps the stores, as upstream does"
        );
    }
}
