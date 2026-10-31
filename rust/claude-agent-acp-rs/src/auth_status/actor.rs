//! The auth-status actor: the last-reported identity of the connection, the auth
//! epoch, the probe context and the per-session account kinds of the
//! `--hide-claude-auth` guard (the `ClaudeAcpAgent` auth members of upstream
//! `acp-agent.ts:3225-3500`). State lives only inside the task, no lock.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{
    auth_context_changed, from_account_info, merge, not_logged_in, same_status, AuthContext,
    AuthStatus, ProbeOutcome, ProbeRequest,
};
use crate::logger::Logger;

/// A boxed probe: the request in, the mapped status out.
pub type ProbeFn =
    Box<dyn Fn(ProbeRequest) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send>> + Send + Sync>;

/// The settings context of a session, as `useAuthStatusSettings` receives it.
#[derive(Debug, Clone, Default)]
pub struct SessionAuthContext {
    /// The cwd of the session's settings manager.
    pub cwd: PathBuf,
    /// `settingsManager.getApiKeyHelper(settingsOverride)`.
    pub api_key_helper: Option<String>,
    /// The programmatic settings of the session (`--settings` of the probe).
    pub settings_override: Option<Value>,
    /// `_meta.claudeCode.options.env`, `None` when the session had none.
    pub launch_env: Option<Vec<(String, String)>>,
}

impl SessionAuthContext {
    /// The comparison key of `useAuthStatusSettings`.
    fn key(&self) -> AuthContext {
        AuthContext {
            cwd: self.cwd.clone(),
            api_key_helper: self.api_key_helper.clone(),
            env: self.launch_env.as_ref().map(|env| {
                let map: serde_json::Map<String, Value> = env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                Value::Object(map).to_string()
            }),
        }
    }
}

/// Messages of the auth-status actor ([`spawn_auth_actor`]).
#[derive(Debug)]
pub enum AuthCmd {
    /// Re-read the identity from the CLI. `fresh` forces a new run even when
    /// one is in flight.
    Probe { cwd: PathBuf, fresh: bool },
    /// Bump epoch to invalidate in-flight probes (e.g. before logout subprocess).
    BumpEpoch,
    /// `useAuthStatusSettings`: the probe context of the session about to run (or
    /// prompted). A change of cwd, apiKeyHelper or launch env bumps the epoch and
    /// re-probes; `reply` gets the epoch afterwards.
    UseContext {
        context: SessionAuthContext,
        reply: Option<oneshot::Sender<u64>>,
    },
    /// ACP gateway `authenticate`: the gateway holds the credentials from here on.
    /// Bumps the epoch and reports the gateway.
    Authenticate { base_url: Option<String> },
    /// The routing state of the connection: a client provider override
    /// (`providers/set`) and whether any routing (override or valid gateway
    /// `authenticate`) is active, which switches the subscription guard off.
    SetRouting {
        provider_override: bool,
        routing_active: bool,
    },
    /// A session was created: publish its account (if informative) and register
    /// the kind of identity it started on. `epoch` is the epoch the session's
    /// probe context was established in: an account read under an older epoch
    /// describes a world that no longer exists and is not published.
    SessionCreated {
        session_id: Option<String>,
        cwd: PathBuf,
        account: Option<Value>,
        api_key_helper: Option<String>,
        epoch: Option<u64>,
    },
    /// Resolves once every push queued before it was handed to the forwarder, so a
    /// response can be ordered behind the status it caused.
    Barrier { reply: oneshot::Sender<()> },
    /// The session is gone.
    ForgetSession { session_id: String },
    /// `markSessionForSignOutRespawn`: the identity behind the session's cached
    /// account is gone (a sign-out failed its turn). Only under the flag.
    MarkRespawn { session_id: String },
    /// Whether the session is flagged for the recreate at its next turn.
    NeedsRespawn {
        session_id: String,
        reply: oneshot::Sender<bool>,
    },
    /// `logout` cleared the credentials: drop every probe that started before
    /// (its answer is stale), re-read the store with a fresh probe and, when it
    /// cannot be read, report "not logged in". `reply` resolves once the new
    /// status (if any) was pushed.
    Logout {
        cwd: PathBuf,
        reply: oneshot::Sender<()>,
    },
    /// A spawned probe finished (internal). `epoch` is the actor epoch the
    /// probe started in; `reply` is set for the post-logout read.
    ProbeDone {
        outcome: ProbeOutcome,
        epoch: u64,
        reply: Option<oneshot::Sender<()>>,
    },
}

/// Cheap handle to the actor.
#[derive(Debug, Clone)]
pub struct AuthHandle {
    tx: mpsc::UnboundedSender<AuthCmd>,
}

impl AuthHandle {
    /// Fire-and-forget; a stopped actor is ignored.
    pub fn send(&self, cmd: AuthCmd) {
        let _ = self.tx.send(cmd);
    }

    /// Bump epoch before running logout (in-flight probes dropped).
    pub fn bump_epoch(&self) {
        self.send(AuthCmd::BumpEpoch);
    }

    /// Run a fresh probe in `cwd`.
    pub fn probe(&self, cwd: PathBuf, fresh: bool) {
        self.send(AuthCmd::Probe { cwd, fresh });
    }

    /// `useAuthStatusSettings`; resolves to the auth epoch afterwards (0 when the
    /// actor is gone).
    pub async fn use_context(&self, context: SessionAuthContext) -> u64 {
        let (reply, epoch) = oneshot::channel();
        self.send(AuthCmd::UseContext {
            context,
            reply: Some(reply),
        });
        epoch.await.unwrap_or(0)
    }

    /// `useAuthStatusSettings` without waiting.
    pub fn use_context_detached(&self, context: SessionAuthContext) {
        self.send(AuthCmd::UseContext {
            context,
            reply: None,
        });
    }

    /// Whether `session_id` is flagged for a recreate at its next turn.
    pub async fn needs_respawn(&self, session_id: &str) -> bool {
        let (reply, flagged) = oneshot::channel();
        self.send(AuthCmd::NeedsRespawn {
            session_id: session_id.to_string(),
            reply,
        });
        flagged.await.unwrap_or(false)
    }
}

/// What the actor hands its forwarder, in wire order.
#[derive(Debug)]
pub enum AuthPush {
    /// The `authStatus` object of a changed status.
    Status(Value),
    /// Resolved by the forwarder once every earlier push was sent, so a
    /// response can be ordered behind its notifications.
    Barrier(oneshot::Sender<()>),
}

/// Options of the actor beyond the probe.
#[derive(Clone, Default)]
pub struct AuthActorConfig {
    /// `--hide-claude-auth`: probes mark sessions whose kind of identity changed.
    pub hide_claude_auth: bool,
    pub logger: Option<Arc<Logger>>,
}

/// `gatewayAuthStatus(baseUrl)`: the gateway the ACP `authenticate` set.
pub fn gateway_auth_status(base_url: Option<&str>) -> AuthStatus {
    let mut status = AuthStatus::new("gateway", "Custom model gateway");
    let host = base_url
        .filter(|url| !url.is_empty())
        .map(|url| match url::Url::parse(url) {
            Ok(parsed) => match (parsed.host_str(), parsed.port()) {
                (Some(host), Some(port)) => format!("{host}:{port}"),
                (Some(host), None) => host.to_string(),
                (None, _) => url.to_string(),
            },
            Err(_) => url.to_string(),
        });
    status.detail = host;
    status
}

/// What the actor knows of one session.
#[derive(Default)]
struct SessionAuth {
    /// The kind of identity the session started on (`Session.accountKind`).
    kind: Option<String>,
    /// `Session.needsSignOutRespawn`.
    needs_respawn: bool,
}

struct State {
    config: AuthActorConfig,
    push: mpsc::UnboundedSender<AuthPush>,
    current: Option<AuthStatus>,
    in_flight: usize,
    epoch: u64,
    context: Option<SessionAuthContext>,
    /// The key of the last context, `AuthContext::default()` at first.
    context_key: AuthContext,
    gateway_auth: bool,
    provider_override: bool,
    routing_active: bool,
    sessions: HashMap<String, SessionAuth>,
    logged_probe_timeout: bool,
    logged_overridden_account: bool,
    logged_uninformative_account: bool,
}

impl State {
    fn log(&self, message: &str) {
        if let Some(logger) = &self.config.logger {
            logger.log(message);
        }
    }

    fn log_error(&self, message: &str) {
        if let Some(logger) = &self.config.logger {
            logger.error(message);
        }
    }

    /// `setAuthStatus`: pushed only when the payload changed.
    fn set(&mut self, next: AuthStatus) {
        if same_status(self.current.as_ref(), &next) {
            return;
        }
        let _ = self.push.send(AuthPush::Status(next.to_json()));
        self.current = Some(next);
    }

    /// `claudeSubscriptionGuardActive`.
    fn guard_active(&self) -> bool {
        self.config.hide_claude_auth && !self.routing_active
    }

    /// `markSessionsWhoseAccountKindChanged`: a probe that finds another kind of
    /// identity proves the account the session started on stale. It flags the
    /// session, nothing more: the recreate runs at the next turn boundary.
    fn mark_sessions_whose_kind_changed(&mut self, kind: &str) {
        if !self.guard_active() {
            return;
        }
        let mut marked = Vec::new();
        for (id, session) in &mut self.sessions {
            let Some(started) = &session.kind else {
                continue;
            };
            if session.needs_respawn {
                continue;
            }
            if started != kind {
                session.needs_respawn = true;
                marked.push(id.clone());
            }
        }
        for id in marked {
            self.log(&respawn_message(&id));
        }
    }

    fn request(&self, cwd: PathBuf) -> ProbeRequest {
        match &self.context {
            Some(context) => ProbeRequest {
                cwd: context.cwd.clone(),
                settings_override: context.settings_override.clone(),
                launch_env: context.launch_env.clone(),
                logger: self.config.logger.clone(),
            },
            None => ProbeRequest {
                cwd,
                logger: self.config.logger.clone(),
                ..ProbeRequest::default()
            },
        }
    }

    /// `publishSessionAccountIdentity`.
    fn publish_session_account(&mut self, account: &Value, api_key_helper: Option<&str>) {
        if self.provider_override {
            if !self.logged_overridden_account {
                self.logged_overridden_account = true;
                self.log(
                    "[authStatus] client provider override active; keeping agent-owned probe state",
                );
            }
            return;
        }
        if self.gateway_auth {
            return;
        }
        match from_account_info(account, api_key_helper) {
            Some(status) => self.set(status),
            None => {
                if !self.logged_uninformative_account {
                    self.logged_uninformative_account = true;
                    self.log(
                        "[authStatus] session account carries no identity signal; keeping probe",
                    );
                }
            }
        }
    }
}

fn respawn_message(session_id: &str) -> String {
    format!(
        "Session {session_id}: the identity behind the cached account changed (sign-out, or a probe that found a different login); the query is recreated on the next turn"
    )
}

/// Spawn the actor. `push` receives every changed status (and barriers) for
/// the forwarder; `probe` reads the CLI; `env` is the SAME injected managed
/// source the sessions use, so the `apiKeyHelper` precedence (managed > user >
/// project > local) never falls back to the host process environment.
pub fn spawn_auth_actor(
    push: mpsc::UnboundedSender<AuthPush>,
    probe: ProbeFn,
    initial_cwd: PathBuf,
    env: crate::env_snapshot::EnvSnapshot,
) -> AuthHandle {
    spawn_auth_actor_with(push, probe, initial_cwd, env, AuthActorConfig::default())
}

/// [`spawn_auth_actor`] with the options of the `--hide-claude-auth` guard and a
/// logger.
pub fn spawn_auth_actor_with(
    push: mpsc::UnboundedSender<AuthPush>,
    probe: ProbeFn,
    initial_cwd: PathBuf,
    env: crate::env_snapshot::EnvSnapshot,
    config: AuthActorConfig,
) -> AuthHandle {
    let _ = env;
    let (tx, mut rx) = mpsc::unbounded_channel::<AuthCmd>();
    let self_tx = tx.clone();
    tokio::spawn(async move {
        let mut state = State {
            config,
            push,
            current: None,
            in_flight: 0,
            epoch: 0,
            context: None,
            context_key: AuthContext {
                cwd: initial_cwd.clone(),
                api_key_helper: None,
                env: None,
            },
            gateway_auth: false,
            provider_override: false,
            routing_active: false,
            sessions: HashMap::new(),
            logged_probe_timeout: false,
            logged_overridden_account: false,
            logged_uninformative_account: false,
        };
        let spawn_probe = |state: &State, cwd: PathBuf, reply: Option<oneshot::Sender<()>>| {
            let fut = probe(state.request(cwd));
            let done = self_tx.clone();
            let epoch = state.epoch;
            tokio::spawn(async move {
                let _ = done.send(AuthCmd::ProbeDone {
                    outcome: fut.await,
                    epoch,
                    reply,
                });
            });
        };
        while let Some(cmd) = rx.recv().await {
            match cmd {
                AuthCmd::BumpEpoch => {
                    state.epoch += 1;
                }
                AuthCmd::Probe { cwd, fresh } => {
                    // ACP gateway auth bypasses the CLI credential store, so a
                    // probe would report an identity that is not the one in use.
                    if state.gateway_auth {
                        continue;
                    }
                    if state.in_flight > 0 && !fresh {
                        continue;
                    }
                    state.in_flight += 1;
                    spawn_probe(&state, cwd, None);
                }
                AuthCmd::UseContext { context, reply } => {
                    let key = context.key();
                    let changed = auth_context_changed(&state.context_key, &key);
                    let cwd = context.cwd.clone();
                    state.context = Some(context);
                    state.context_key = key;
                    if changed {
                        // The epoch bump discards any probe that read the old
                        // context, and the session account may carry no identity
                        // signal (an apiKeyHelper), so only a probe in the new
                        // context can classify it.
                        state.epoch += 1;
                        if !state.gateway_auth {
                            state.in_flight += 1;
                            spawn_probe(&state, cwd, None);
                        }
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send(state.epoch);
                    }
                }
                AuthCmd::Authenticate { base_url } => {
                    state.gateway_auth = true;
                    // Discards any probe that started before this login: its
                    // answer is now stale.
                    state.epoch += 1;
                    let status = gateway_auth_status(base_url.as_deref());
                    state.set(status);
                }
                AuthCmd::SetRouting {
                    provider_override,
                    routing_active,
                } => {
                    state.provider_override = provider_override;
                    state.routing_active = routing_active;
                }
                AuthCmd::Logout { cwd, reply } => {
                    state.gateway_auth = false;
                    state.epoch += 1;
                    state.in_flight += 1;
                    spawn_probe(&state, cwd, Some(reply));
                }
                AuthCmd::ProbeDone {
                    outcome,
                    epoch: started,
                    reply,
                } => {
                    state.in_flight = state.in_flight.saturating_sub(1);
                    if outcome == ProbeOutcome::TimedOut && !state.logged_probe_timeout {
                        state.logged_probe_timeout = true;
                        state.log_error(
                            "claude auth status did not answer within 5 s; the identity is not refreshed",
                        );
                    }
                    if reply.is_some() {
                        // The post-logout read: an unreadable store means the
                        // old identity is stale, so report "not logged in".
                        // On probe timeout, keep current status instead of
                        // pushing notLoggedIn.
                        match outcome {
                            ProbeOutcome::Status(st) => {
                                let next = merge(state.current.as_ref(), &st);
                                state.set(next);
                                state.mark_sessions_whose_kind_changed(&st.kind);
                            }
                            ProbeOutcome::LoggedOut => {
                                state.set(not_logged_in());
                            }
                            ProbeOutcome::TimedOut => {
                                if state.current.is_none() {
                                    state.set(not_logged_in());
                                }
                            }
                        }
                    } else if let ProbeOutcome::Status(st) = outcome {
                        if started == state.epoch {
                            let merged = merge(state.current.as_ref(), &st);
                            state.set(merged);
                            state.mark_sessions_whose_kind_changed(&st.kind);
                        } else {
                            // An `authenticate`, `logout` or context change landed
                            // while this probe was running; the newer state wins.
                            state.log(
                                "[authStatus] discarding a probe that predates the latest auth change",
                            );
                        }
                    }
                    if let Some(reply) = reply {
                        // Resolve behind the forwarder so the logout response
                        // cannot overtake the status it caused.
                        let _ = state.push.send(AuthPush::Barrier(reply));
                    }
                }
                AuthCmd::SessionCreated {
                    session_id,
                    cwd: _,
                    account,
                    api_key_helper,
                    epoch,
                } => {
                    if let Some(account) = &account {
                        // Publish only a read that belongs to the current epoch.
                        if epoch.is_none_or(|e| e == state.epoch) {
                            state.publish_session_account(account, api_key_helper.as_deref());
                        }
                    }
                    if let Some(id) = session_id {
                        let kind = account
                            .as_ref()
                            .and_then(|a| from_account_info(a, api_key_helper.as_deref()))
                            .map(|status| status.kind);
                        state.sessions.insert(
                            id,
                            SessionAuth {
                                kind,
                                needs_respawn: false,
                            },
                        );
                    }
                }
                AuthCmd::Barrier { reply } => {
                    let _ = state.push.send(AuthPush::Barrier(reply));
                }
                AuthCmd::ForgetSession { session_id } => {
                    state.sessions.remove(&session_id);
                }
                AuthCmd::MarkRespawn { session_id } => {
                    if !state.config.hide_claude_auth {
                        continue;
                    }
                    // A mark that lands after the session was forgotten (a prompt
                    // task that settled once it was closed) must not bring the entry
                    // back: upstream keeps the flag on the `Session`, which dies
                    // with it.
                    let Some(session) = state.sessions.get_mut(&session_id) else {
                        continue;
                    };
                    if session.needs_respawn {
                        continue;
                    }
                    session.needs_respawn = true;
                    state.log(&respawn_message(&session_id));
                }
                AuthCmd::NeedsRespawn { session_id, reply } => {
                    let flagged = state
                        .sessions
                        .get(&session_id)
                        .is_some_and(|session| session.needs_respawn);
                    let _ = reply.send(flagged);
                }
            }
        }
    });
    AuthHandle { tx }
}
