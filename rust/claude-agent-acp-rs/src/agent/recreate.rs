//! Recreating live sessions in place: a provider switch (`providers/set`,
//! `providers/disable`, `logout`) bakes new routing into the environment of every
//! query, and a sign-out under `--hide-claude-auth` leaves a query whose cached
//! account is wrong. Both close the query and resume the conversation under the
//! same session id (upstream `enqueueProviderUpdate`, `recreateSignedOutQuery`).

use super::providers::{self, ProviderConfig};
use super::*;
use crate::gateway_models::{self, GatewayListingError};
use crate::session::failures::{FailureOptions, SessionFailures};
use crate::session_failure::ClaudeFailureKind;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use tokio::sync::{watch, OwnedSemaphorePermit};

/// What a provider update needs of the connection, as cheap clones: it runs off
/// the request handler (it waits for the submitted turns, lists gateway models
/// and starts every session anew), so it cannot borrow the registry.
pub(super) struct ProviderUpdateEnv {
    /// The gate of the update in flight, held until it ends.
    pub gate: Arc<tokio::sync::Semaphore>,
    /// The live sessions and their creation params, as last published by the
    /// request handler.
    pub sessions: watch::Receiver<HashMap<String, (Session, Value)>>,
    /// What the session index shows of the sessions this connection runs.
    pub board: crate::session_index::connection::OwnSessionBoard,
    pub deferred_tx: mpsc::UnboundedSender<RegistryMsg>,
    pub session_tx: mpsc::UnboundedSender<(String, Session)>,
    pub dispose_tx: mpsc::UnboundedSender<Session>,
    pub auth: Option<AuthHandle>,
    pub auth_env: AuthEnv,
    pub air: Option<AirClient>,
    pub caps: ClientCapabilities,
    /// Publishes the sessions the update starts for the next update
    /// (`sessions_view`).
    pub sessions_view: Arc<watch::Sender<HashMap<String, (Session, Value)>>>,
    /// Set when the connection is ending: the update stops between steps.
    pub cancel: Arc<AtomicBool>,
    /// The connection serves ACP v2 (M-118).
    pub is_v2: bool,
    /// The connection's v2 outbound task, when the connection negotiated v2.
    pub v2_outbound: Option<crate::v2::agent::V2OutboundHandle>,
}

impl ProviderUpdateEnv {
    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// `listProviderModels`: the model ids the gateway in `config` lists, read before
/// `providers/set` returns so the sessions it recreates already offer only those,
/// and for `bedrock` the region prefix picked from them. Setting the same gateway
/// again reuses the previous listing; `vertex` gateways are not listed.
pub(super) async fn list_provider_models(
    current: Option<&ProviderConfig>,
    config: &mut ProviderConfig,
    logger: &Option<Arc<crate::logger::Logger>>,
) -> std::result::Result<(), GatewayListingError> {
    // Reuse needs the very same headers, so a rotated token lists again, and a
    // failed listing then fails that `providers/set`.
    if let Some(current) = current {
        if current.listed_models.is_some()
            && current.api_type == config.api_type
            && current.base_url == config.base_url
            && providers::same_headers(&current.headers, &config.headers)
        {
            config.listed_models = current.listed_models.clone();
            config.bedrock_region_prefix = current.bedrock_region_prefix.clone();
            return Ok(());
        }
    }
    match config.api_type.as_str() {
        "anthropic" => {
            config.listed_models =
                gateway_models::list_gateway_models(&config.base_url, &config.headers, logger)
                    .await?;
        }
        "bedrock" => {
            let Some(model_ids) =
                gateway_models::list_bedrock_models(&config.base_url, &config.headers, logger)
                    .await?
            else {
                return Ok(());
            };
            let region = gateway_models::select_bedrock_region(&model_ids);
            if region.model_ids.len() < model_ids.len() {
                if let Some(logger) = logger {
                    logger.log(&format!(
                        "[providers/set] Bedrock models span several region prefixes; using {} and dropping {} id(s) of other prefixes",
                        region.region_prefix.as_deref().unwrap_or("undefined"),
                        model_ids.len() - region.model_ids.len()
                    ));
                }
            }
            config.listed_models = Some(region.model_ids);
            config.bedrock_region_prefix = region.region_prefix;
        }
        _ => {}
    }
    Ok(())
}

/// Whether a session still has a turn to settle: a prompt on the board
/// (`prompts_in_flight`) or an unsettled turn of its own actor, which covers the
/// turns a steer started without a `session/prompt` (`turnQueue.some(!settled)`).
pub(super) async fn session_has_unsettled_turn(
    session: &Session,
    board: &crate::session_index::connection::OwnSessionBoard,
    session_id: &str,
) -> bool {
    if board
        .get(session_id)
        .is_some_and(|own| own.prompts_in_flight > 0)
    {
        return true;
    }
    session.has_unsettled_turn().await
}

/// `enqueueProviderUpdate` off the request handler: replaces the routing of the
/// `main` provider with `config` (`None`: native routing), then recreates every
/// loaded session so its query inherits the new environment.
///
/// It runs as a spawned task behind [`ProviderUpdateEnv::gate`], so the
/// connection keeps delivering permission answers, `fs/*` and `terminal/*`
/// replies and `session/cancel` while it waits for the turns to settle. A session
/// request that follows waits on the gate, as upstream's `providerUpdate`
/// promise does.
pub(super) fn spawn_provider_update(
    registry: &Registry,
    opts: &ServeOptions,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    config: Option<ProviderConfig>,
    responder: Option<agent_client_protocol::Responder<Value>>,
) {
    // Reserve the gate NOW: a session request that follows this one waits behind
    // the update, like upstream's stored `providerUpdate` promise. A permit the
    // previous update (or a running session request) holds makes the task queue.
    let reserved = registry
        .provider_gate
        .clone()
        .try_acquire_many_owned(GATE_PERMITS)
        .ok();
    let previous = registry.provider.clone();
    // The routing the update installs is what its recreated sessions see: a
    // `providers/set` override switches the subscription guard off, clearing it
    // switches the guard back on.
    let routing_active = config.is_some() || registry.gateway_auth.is_some();
    let cancel = Arc::new(AtomicBool::new(false));
    let env = registry.provider_env(opts, cancel.clone(), routing_active);
    registry
        .provider_updates
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let busy = ProviderUpdateBusy(registry.provider_updates.clone());
    let opts = opts.clone();
    let connection = connection.clone();
    let handle = tokio::spawn(async move {
        let _busy = busy;
        let result = run_provider_update(env, reserved, config, previous, &opts, &connection).await;
        if let Some(responder) = responder {
            match result {
                Ok(()) => {
                    let _ = responder.respond(json!({}));
                }
                Err(error) => {
                    let _ = responder.respond_with_error(error);
                }
            }
        }
    });
    let _ = registry.recreate_tx.send(RecreateTask { cancel, handle });
}

/// Keeps the count of provider updates in flight (`providerUpdate !== null`).
struct ProviderUpdateBusy(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ProviderUpdateBusy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The body of [`spawn_provider_update`], and the recreation half of `logout`.
pub(super) async fn run_provider_update(
    env: ProviderUpdateEnv,
    reserved: Option<OwnedSemaphorePermit>,
    mut config: Option<ProviderConfig>,
    previous: Option<ProviderConfig>,
    opts: &ServeOptions,
    connection: &ConnectionTo<agent_client_protocol::Client>,
) -> std::result::Result<(), Error> {
    let _permit = match reserved {
        Some(permit) => permit,
        None => match env.gate.clone().acquire_many_owned(GATE_PERMITS).await {
            Ok(permit) => permit,
            // The connection is gone; nobody is left to answer.
            Err(_) => return Ok(()),
        },
    };
    // A failed listing fails this `providers/set` before the provider replaces the
    // current one.
    if let Some(config) = config.as_mut() {
        if let Err(error) = list_provider_models(previous.as_ref(), config, &opts.logger).await {
            return Err(Error::new(
                -32603,
                format!(
                    "Internal error: Model discovery for provider \"{}\" failed: {}",
                    providers::PROVIDER_ID,
                    error.0
                ),
            )
            .data(json!({
                "providerId": providers::PROVIDER_ID,
                "apiType": config.api_type,
                "baseUrl": config.base_url,
            })));
        }
        if let Some(logger) = &opts.logger {
            logger.log(&format!(
                "[providers/set] listedModels={} bedrockRegionPrefix={}",
                config
                    .listed_models
                    .as_ref()
                    .map_or("none".to_string(), |m| m.len().to_string()),
                config.bedrock_region_prefix.as_deref().unwrap_or("none")
            ));
        }
    }
    // The sessions that exist when the update runs; a session created meanwhile
    // waits on the gate, so it is not in this list and starts under the new
    // routing itself. A closed entry (a sign-out husk, or a session whose CLI
    // ended) is recreated like upstream's update does; the identity check below
    // skips an entry another path replaced while the update waited.
    let sessions: Vec<(String, Session, Value)> = env
        .sessions
        .borrow()
        .iter()
        .map(|(id, (session, params))| (id.clone(), session.clone(), params.clone()))
        .collect();
    let mut waiting = false;
    for (id, session, _) in &sessions {
        if session_has_unsettled_turn(session, &env.board, id).await {
            waiting = true;
            break;
        }
    }
    if waiting {
        if let Some(logger) = &opts.logger {
            logger.log("Waiting for active Claude turn(s) before provider update");
        }
        loop {
            if env.cancelled() {
                return Ok(());
            }
            let mut unsettled = false;
            for (id, session, _) in &sessions {
                if session_has_unsettled_turn(session, &env.board, id).await {
                    unsettled = true;
                    break;
                }
            }
            if !unsettled {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    // The request handler adopts the routing before it serves the next request;
    // the sessions below already start under it.
    let _ = env.deferred_tx.send(RegistryMsg::SetProvider {
        config: config.clone(),
    });
    for (session_id, old, params) in sessions {
        if env.cancelled() {
            return Ok(());
        }
        // Gone while the update waited: `session/close` and `session/delete`
        // republish the view when they remove the session, so a closed or deleted
        // session is no longer in it. Skip it, and skip a session another path
        // replaced (a steer or goal respawn, V-16: the stale handle fails the
        // identity check, `v0.94.0 :11752`). A closed husk that is still registered
        // (a sign-out) stays in the view and is recreated like upstream's
        // `this.sessions[id] === session` loop does. The board cannot tell the
        // cases apart: a husk's row is retired when its actor ends, too. A dead
        // session whose `Evict` is not applied yet is still in the view until the
        // next request applies it (a known window).
        let current = env
            .sessions
            .borrow()
            .get(&session_id)
            .is_some_and(|(live, _)| live.command_sender().same_channel(old.command_sender()));
        if !current {
            continue;
        }
        if let Some(logger) = &opts.logger {
            logger.log(&format!(
                "Recreating Claude session {session_id} for provider update"
            ));
        }
        let live_mode = match old.live_state().await {
            Ok((modes, _)) => modes.current_mode_id,
            Err(_) => old.modes().current_mode_id.clone(),
        };
        // The lost-session report is published on the session's own failure book
        // (`session.sessionFailureState`), which the close ends.
        let failures = if env
            .air
            .as_ref()
            .is_some_and(|air| air.supports("sessionFailure"))
        {
            old.failures_snapshot().await.ok()
        } else {
            None
        };
        old.close().await;
        // The old session is torn down, as upstream's `closeQueryStream` +
        // `delete this.sessions` does: its row falls back to the transcript
        // until the new session registers.
        env.board.forget(&session_id);
        if let Err(error) = start_update_session(
            &env,
            opts,
            connection,
            &session_id,
            &params,
            live_mode,
            config.as_ref(),
        )
        .await
        {
            // One session that cannot come back must not abort the switch. The
            // `--hide-claude-auth` guard makes this a normal outcome of
            // `providers/disable`: the override kept a subscription account
            // usable, and creation refuses without it. The session is already
            // gone.
            report_session_lost(&env, connection, &session_id, &error, opts, failures);
            let _ = env.deferred_tx.send(RegistryMsg::Forget {
                session_id: session_id.clone(),
                husk: old.command_sender().downgrade(),
            });
        }
    }
    Ok(())
}

/// `reportSessionLostOnProviderUpdate`: a session that a provider update could not
/// recreate is gone, so a capable client is told why instead of meeting "Session
/// not found" on its next call.
fn report_session_lost(
    env: &ProviderUpdateEnv,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: &str,
    error: &Error,
    opts: &ServeOptions,
    failures: Option<SessionFailures>,
) {
    if let Some(logger) = &opts.logger {
        logger.error(&format!(
            "Session {session_id}: could not be recreated for the provider update: {}",
            error.message
        ));
    }
    // The sign-out refusal is the client's own auth flow to answer.
    if i32::from(error.code) == -32000 {
        return;
    }
    if !env
        .air
        .as_ref()
        .is_some_and(|air| air.supports("sessionFailure"))
    {
        return;
    }
    let update = session_lost_update(failures, session_id, error);
    if let Ok(msg) = UntypedMessage::new("session/update", update) {
        let _ = connection.send_notification(msg);
    }
}

/// The failure update that tells a client its session is lost. The session is gone
/// from the registry by now, so the usual identity check would drop the row; the
/// book of the session itself keeps the id's epoch and incident counter (a fresh
/// book only when the session could not give it).
pub(super) fn session_lost_update(
    failures: Option<SessionFailures>,
    session_id: &str,
    error: &Error,
) -> Value {
    let reason = error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    failures.unwrap_or_default().publish(
        session_id,
        ClaudeFailureKind::InternalError,
        &FailureOptions {
            session_scoped: true,
            details: Some(error.message.clone()),
            reason,
            ..FailureOptions::default()
        },
    )
}

/// Starts the query of `session_id` anew under the routing in force, without
/// answering a request (`createSession(creationParams, {resume})` in
/// `enqueueProviderUpdate`). The registry entries go to the deferred channel; the
/// request handler applies them before it serves the next request.
async fn start_update_session(
    env: &ProviderUpdateEnv,
    opts: &ServeOptions,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: &str,
    params: &Value,
    live_mode: String,
    provider: Option<&ProviderConfig>,
) -> std::result::Result<(), Error> {
    validate_cwd(params.get("cwd").and_then(Value::as_str))?;
    let initialize = initialize_options_for(params, &env.caps, env.air.as_ref(), env.is_v2)?;
    let cwd = params.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    let mut fresh = false;
    let (session, update_rx, auth_context, epoch) = loop {
        let (mut claude, _mode, allow_bypass) =
            spawn_inputs(params, cwd.as_deref(), opts, Some(session_id))?;
        claude.enable_file_checkpointing = Some(initialize.enable_file_checkpointing);
        let raw_sdk_messages = claude.raw_sdk_messages.clone();
        let settings_arg = claude.settings_arg.clone();
        let launch_env = claude.env.clone();
        let ask_disallowed = !env.caps.elicitation_form; // O-135: only a form-capable client keeps AskUserQuestion
        let additional_directories = claude.additional_directories.clone();
        let mut spawn = session_spawn_options(
            opts,
            cwd.clone(),
            session_id.to_string(),
            claude,
            live_mode.clone(),
            allow_bypass,
            ask_disallowed,
        );
        if fresh {
            spawn.resume = None;
            spawn.session_id = Some(session_id.to_string());
        } else {
            spawn.resume = Some(session_id.to_string());
            spawn.session_id = None;
        }
        let auth_cwd = spawn.default_cwd.clone().unwrap_or_default();
        let auth_context =
            session_auth_context(auth_cwd, settings_arg.as_deref(), &launch_env, opts);
        let epoch = match &env.auth {
            Some(auth) => auth.use_context(auth_context.clone()).await,
            None => 0,
        };
        providers::apply_provider_to_spawn(&mut spawn, provider);
        let sess_opts = SessionOptions::new(spawn, opts.timings.clone(), initialize.clone())
            .with_air(env.air.clone())
            .with_caps(env.caps.clone())
            .with_raw_sdk_messages(raw_sdk_messages)
            .with_env(opts.env.clone())
            .with_logger(opts.logger.clone())
            .with_gateway_models(providers::gateway_models_of(provider))
            .with_v2(env.is_v2)
            .with_additional_directories(additional_directories);
        let started = Session::start(sess_opts, session_id.to_string()).await;
        if recreate_falls_back_to_fresh(true, fresh, &started) {
            if let Some(logger) = &opts.logger {
                logger.log(&format!(
                    "session {session_id} was never persisted; starting a fresh query under the same id"
                ));
            }
            fresh = true;
            continue;
        }
        match started {
            Ok((session, update_rx)) => break (session, update_rx, auth_context, epoch),
            Err(e) => return Err(resumed_start_error(&e, true, session_id)),
        }
    };
    let mut guard = crate::hide_claude_auth::GuardState::default();
    if let Err(refusal) = account_at_creation(
        &env.auth_env,
        session_id,
        &auth_context,
        epoch,
        &session,
        &mut guard,
    ) {
        drop(update_rx);
        session.close().await;
        return Err(refusal);
    }
    let manager = Arc::new(crate::settings_manager::SettingsManager::new(
        auth_context.cwd.clone(),
        Some(crate::settings_manager::SettingsManagerOptions {
            on_change: None,
            logger: opts.logger.clone(),
            env: Some(opts.env.clone()),
            managed_docs: None,
            managed_file: None,
            poll_interval: None,
        }),
    ));
    let _ = env
        .deferred_tx
        .send(RegistryMsg::Register(Box::new(RegistryUpdate {
            session_id: session_id.to_string(),
            session: session.clone(),
            settings_manager: manager,
            fingerprint: compute_session_fingerprint(params),
            cancel: None,
            creation_params: params.clone(),
            auth_context,
            guard,
        })));
    if !fresh {
        env.board
            .update(session_id, |own| own.resumed_from_history = true);
    }
    let published = session.clone();
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
        session_id,
        session,
        update_rx,
        connection,
    );
    // The next update (or a `logout` queued behind this one) recreates this
    // session, not the one it replaced.
    env.sessions_view.send_modify(|view| {
        view.insert(session_id.to_string(), (published, params.clone()));
    });
    Ok(())
}
