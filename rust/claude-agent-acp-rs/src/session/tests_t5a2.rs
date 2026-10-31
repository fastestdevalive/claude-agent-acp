//! T5A2: the F11 session side — Fast mode (set, SDK state sync, model-switch
//! intent), effort reconcile after a model switch, the live Agent switch, the
//! PostModelSwitch reconcile, the deferred auto-fallback warning and the child
//! permission routing.
//!
//! These drive the REAL actor loop (`run_loop` + a real `Control` over a
//! duplex stdin): the test answers the actor's `control_request`s and reads the
//! `session/update`s it publishes, so every assertion is on the wire values the
//! running agent produces.

use super::*;
use crate::session_config::{
    build_available_modes, build_config_options_with, model_supports_fast_mode, AgentInfo,
    ConfigOption, ConfigOptionsInput, FastModeOptionState, ModelInfo, ModesState,
    SessionConfigState,
};
use tokio::time::timeout;

const SESSION_ID: &str = "s";

/// A running actor with its control channel, its stream and its outbound
/// updates, plus the lines the actor wrote to the child's stdin.
struct Actor {
    tx: mpsc::UnboundedSender<Command>,
    updates: mpsc::UnboundedReceiver<SessionOutbound>,
    stream: Option<mpsc::UnboundedSender<Value>>,
    control_in: mpsc::UnboundedSender<Value>,
    written: mpsc::UnboundedReceiver<String>,
    stored: Vec<Value>,
}

impl Actor {
    fn new(
        config: SessionConfigState,
        caps: crate::client_caps::ClientCapabilities,
        air: bool,
    ) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let air_client = air
            .then(|| {
                crate::air::AirClient::from_client_capabilities(&json!({
                    "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["recommendedValue"]}}}
                }))
            })
            .flatten();
        let control = Control::spawn(
            a,
            control_in_rx,
            ControlOptions {
                on_request: Some(build_permission_request_handler(
                    tx.clone(),
                    crate::elicitation::ElicitationSupport::default(),
                )),
            },
        );
        let (update_tx, updates) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let (written_tx, written) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            let _ = written_tx.send(trimmed.to_string());
                        }
                    }
                }
            }
        });
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx.clone(),
            update_tx,
            Duration::from_secs(30),
            SESSION_ID.into(),
            Arc::new(AtomicUsize::new(0)),
            air_client,
            caps,
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
        let actor = Self {
            tx,
            updates,
            stream: Some(stream_tx),
            control_in: control_in_tx,
            written,
            stored: Vec::new(),
        };
        actor.seed(config);
        actor
    }

    fn seed(&self, config: SessionConfigState) {
        let _ = self.tx.send(Command::InitConfig { config });
    }

    /// The next `control_request` line the actor wrote, parsed.
    async fn next_request(&mut self) -> Value {
        loop {
            let line = timeout(Duration::from_secs(5), self.written.recv())
                .await
                .expect("the actor writes a control_request")
                .expect("the stdin reader is alive");
            let value: Value = serde_json::from_str(&line).expect("a JSON line");
            if value.get("type").and_then(Value::as_str) == Some("control_request") {
                return value;
            }
        }
    }

    fn respond(&self, request_id: &str, result: Result<Value, &str>) {
        let response = match result {
            Ok(body) => json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id, "response": body},
            }),
            Err(message) => json!({
                "type": "control_response",
                "response": {"subtype": "error", "request_id": request_id, "error": message},
            }),
        };
        // Production carries the CLI's `control_response` on the child's stdout:
        // the codec hands it to the stream branch, which routes a control line to
        // `control_in`. Going through the stream keeps every actor test on that
        // path (an inline switch must let it through while it holds).
        let _ = self
            .stream
            .as_ref()
            .expect("the stream is open")
            .send(response);
    }

    /// Send a raw frame on the child's stdout stream.
    fn send_frame(&self, frame: Value) {
        let _ = self
            .stream
            .as_ref()
            .expect("the stream is open")
            .send(frame);
    }

    /// The child's stdout closes (stream EOF).
    fn close_stream(&mut self) {
        self.stream = None;
    }

    fn respond_ok(&self, request_id: &str) {
        self.respond(request_id, Ok(json!({})));
    }

    /// Send a stream frame and collect the updates it publishes.
    async fn feed(&mut self, frame: Value) -> Vec<Value> {
        self.send_frame(frame);
        self.collect(0).await
    }

    /// Send a client prompt and collect the updates it publishes.
    async fn prompt(&mut self, frame: Value) -> Vec<Value> {
        let uuid = frame["uuid"].as_str().unwrap_or("p1").to_string();
        let (reply, _reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::Prompt {
            uuid,
            frame,
            is_local_only: false,
            prompt_text: String::new(),
            reply,
        });
        self.collect(0).await
    }

    /// Collect updates until `expected` are seen (0: drain what is queued).
    async fn collect(&mut self, expected: usize) -> Vec<Value> {
        let mut out = std::mem::take(&mut self.stored);
        let mut remaining = expected.saturating_sub(out.len());
        loop {
            if remaining == 0 && expected != 0 {
                break;
            }
            match timeout(
                Duration::from_millis(if expected == 0 { 100 } else { 5_000 }),
                self.updates.recv(),
            )
            .await
            {
                Ok(Some(SessionOutbound::Flush(ack))) => {
                    let _ = ack.send(());
                }
                Ok(Some(SessionOutbound::Update(notif))) => {
                    out.push(serde_json::to_value(&notif).unwrap());
                    remaining = remaining.saturating_sub(1);
                }
                Ok(Some(SessionOutbound::RawUpdate(value))) => {
                    out.push(value);
                    remaining = remaining.saturating_sub(1);
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        out
    }

    /// Drain the updates published within `window`, acknowledging Flushes,
    /// without sending anything into the session.
    async fn drain_for(&mut self, window: Duration) -> Vec<Value> {
        let mut out = std::mem::take(&mut self.stored);
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match timeout(left, self.updates.recv()).await {
                Ok(Some(SessionOutbound::Flush(ack))) => {
                    let _ = ack.send(());
                }
                Ok(Some(SessionOutbound::Update(notif))) => {
                    out.push(serde_json::to_value(&notif).unwrap());
                }
                Ok(Some(SessionOutbound::RawUpdate(value))) => out.push(value),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        out
    }

    /// Drive a `SetConfigOption` command to its reply, answering control
    /// requests with `answer` (the default answers success).
    async fn set_config_with(
        &mut self,
        config_id: &str,
        value: Value,
        mut answer: impl FnMut(&Value) -> Result<Value, &'static str>,
    ) -> Result<Vec<ConfigOption>, String> {
        let (reply, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::SetConfigOption {
            config_id: config_id.into(),
            value,
            reply,
        });
        let mut reply_rx = reply_rx;
        loop {
            tokio::select! {
                reply = &mut reply_rx => return reply.expect("the actor replies"),
                line = self.written.recv() => {
                    let Some(line) = line else { panic!("the stdin reader ended") };
                    let request: Value = serde_json::from_str(&line).expect("a JSON line");
                    if request.get("type").and_then(Value::as_str) != Some("control_request") {
                        continue;
                    }
                    let request_id = request["request_id"].as_str().expect("a request id");
                    match answer(&request) {
                        Ok(body) => self.respond(request_id, Ok(body)),
                        Err(message) => self.respond(request_id, Err(message)),
                    }
                }
                update = self.updates.recv() => {
                    match update {
                        Some(SessionOutbound::Flush(ack)) => { let _ = ack.send(()); }
                        Some(SessionOutbound::Update(notif)) => {
                            self.stored.push(serde_json::to_value(&notif).unwrap());
                        }
                        Some(SessionOutbound::RawUpdate(value)) => self.stored.push(value),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn set_config(
        &mut self,
        config_id: &str,
        value: Value,
    ) -> Result<Vec<ConfigOption>, String> {
        self.set_config_with(config_id, value, |_| Ok(json!({})))
            .await
    }

    /// Drive an external `ModelApplied` (`notify_client: true`, the
    /// PostModelSwitch/refusal path) to completion and return the published
    /// updates. `answer` responds to each control request. The switch is
    /// `inline` (the refusal fallback, which holds the stream).
    async fn external_switch(
        &mut self,
        model: &str,
        answer: impl FnMut(&Value) -> Result<Value, &'static str>,
    ) -> Vec<Value> {
        self.external_switch_with(model, true, answer).await
    }

    /// Start an external `ModelApplied` without answering its control requests:
    /// the caller drives the pending round trips itself (stream-hold tests).
    fn start_external_switch(&self, model: &str, inline: bool) {
        let (reply, _reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::ModelApplied {
            model_val: model.into(),
            result: Ok(()),
            reply,
            notify_client: true,
            inline,
        });
    }

    /// Drive a user-driven `ModelApplied` (`notify_client: false`, as
    /// `SetConfigOption model` re-enters) to its reply, acknowledging Flushes.
    async fn user_model_switch(&mut self, model: &str) -> Result<Vec<ConfigOption>, String> {
        let (reply, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::ModelApplied {
            model_val: model.into(),
            result: Ok(()),
            reply,
            notify_client: false,
            inline: false,
        });
        let mut reply_rx = reply_rx;
        loop {
            tokio::select! {
                reply = &mut reply_rx => return reply.expect("the actor replies"),
                line = self.written.recv() => {
                    let Some(line) = line else { panic!("the stdin reader ended") };
                    let request: Value = serde_json::from_str(&line).expect("a JSON line");
                    if request.get("type").and_then(Value::as_str) != Some("control_request") {
                        continue;
                    }
                    let request_id = request["request_id"].as_str().expect("a request id");
                    self.respond_ok(request_id);
                }
                update = self.updates.recv() => {
                    match update {
                        Some(SessionOutbound::Flush(ack)) => { let _ = ack.send(()); }
                        Some(SessionOutbound::Update(notif)) => {
                            self.stored.push(serde_json::to_value(&notif).unwrap());
                        }
                        Some(SessionOutbound::RawUpdate(value)) => self.stored.push(value),
                        _ => {}
                    }
                }
            }
        }
    }

    /// Drive an external `ModelApplied` to completion and return the published
    /// updates. `answer` responds to each control request.
    async fn external_switch_with(
        &mut self,
        model: &str,
        inline: bool,
        mut answer: impl FnMut(&Value) -> Result<Value, &'static str>,
    ) -> Vec<Value> {
        let (reply, _reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::ModelApplied {
            model_val: model.into(),
            result: Ok(()),
            reply,
            notify_client: true,
            inline,
        });
        let mut seen = Vec::new();
        loop {
            tokio::select! {
                line = self.written.recv() => {
                    let Some(line) = line else { panic!("the stdin reader ended") };
                    let request: Value = serde_json::from_str(&line).expect("a JSON line");
                    if request.get("type").and_then(Value::as_str) != Some("control_request") {
                        continue;
                    }
                    let request_id = request["request_id"].as_str().expect("a request id");
                    match answer(&request) {
                        Ok(body) => self.respond(request_id, Ok(body)),
                        Err(message) => self.respond(request_id, Err(message)),
                    }
                }
                update = self.updates.recv() => {
                    match update {
                        Some(SessionOutbound::Flush(ack)) => { let _ = ack.send(()); break; }
                        Some(SessionOutbound::Update(notif)) => {
                            seen.push(serde_json::to_value(&notif).unwrap());
                        }
                        Some(SessionOutbound::RawUpdate(value)) => seen.push(value),
                        _ => {}
                    }
                }
            }
        }
        seen
    }

    /// The actor's live config options (`GetState`).
    async fn config(&mut self) -> Vec<ConfigOption> {
        let (reply, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::GetState { reply });
        let (_, options) = timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("state reply")
            .expect("the actor replies")
            .expect("Ok");
        options
    }
}

fn model(value: &str, supports_fast: bool, effort: Option<&[&str]>, auto: bool) -> ModelInfo {
    ModelInfo {
        value: value.into(),
        display_name: value.into(),
        supports_fast_mode: Some(supports_fast),
        supports_auto_mode: Some(auto),
        supports_effort: effort.map(|_| true),
        supported_effort_levels: effort
            .map(|levels| levels.iter().map(|l| (*l).to_string()).collect()),
        ..Default::default()
    }
}

fn caps(boolean_config: bool, notices: bool) -> crate::client_caps::ClientCapabilities {
    crate::client_caps::ClientCapabilities {
        boolean_config,
        notices,
        ..Default::default()
    }
}

/// A session config state with its option list composed from the fields.
fn base(
    models: Vec<ModelInfo>,
    current: &str,
    _boolean_config: bool,
    recommended: bool,
) -> SessionConfigState {
    let selection = crate::session_model::get_available_models(
        &models, &models, None, None, false, None, false,
    );
    SessionConfigState {
        modes: ModesState {
            current_mode_id: "default".into(),
            available_modes: build_available_modes(false, false),
        },
        available_models: selection.available_models,
        current_model_id: current.into(),
        model_infos: models,
        config_options: Vec::new(),
        current_effort: None,
        effort_pinned: None,
        applied_effort: None,
        allow_bypass: false,
        is_air: recommended,
        ultracode: false,
        pre_plan_mode: None,
        agents: Vec::new(),
        current_agent: "default".into(),
        fast_mode_enabled: false,
        fast_mode_disabled_reason: None,
        pending_auto_fallback_warning: false,
        provider_cache_key: String::new(),
    }
}

/// Compose the option list of a state whose fields a test set directly.
fn composed(mut config: SessionConfigState, boolean_config: bool) -> SessionConfigState {
    let fast = FastModeOptionState {
        supported: model_supports_fast_mode(&config.model_infos, &config.current_model_id),
        enabled: config.fast_mode_enabled,
        use_boolean_option: boolean_config,
        disabled_reason: config.fast_mode_disabled_reason.clone(),
    };
    config.config_options = build_config_options_with(&ConfigOptionsInput {
        modes: &config.modes,
        available_models: &config.available_models,
        current_model_id: &config.current_model_id,
        model_infos: &config.model_infos,
        current_effort: config.current_effort.as_deref(),
        agents: &config.agents,
        current_agent: &config.current_agent,
        fast_mode: Some(&fast),
        use_recommended_value: config.is_air,
    });
    config
}

fn option<'a>(options: &'a [ConfigOption], id: &str) -> Option<&'a ConfigOption> {
    options.iter().find(|o| o.id == id)
}

fn updates_of<'a>(updates: &'a [Value], kind: &str) -> Vec<&'a Value> {
    updates
        .iter()
        .filter(|u| u["update"]["sessionUpdate"] == kind)
        .collect()
}

fn wire_options(update: &Value) -> Vec<ConfigOption> {
    update["update"]["configOptions"]
        .as_array()
        .expect("configOptions")
        .iter()
        .map(|v| serde_json::from_value(v.clone()).expect("a config option"))
        .collect()
}

// ---------------------------------------------------------------- M-37 / A-02

#[tokio::test]
async fn t5a2_fast_mode_toggles_through_boolean_and_select_values() {
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let mut actor = Actor::new(
        composed(base(models.clone(), "claude-sonnet-4-5", true, false), true),
        caps(true, false),
        false,
    );

    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::SetConfigOption {
        config_id: "fast".into(),
        value: json!(true),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "apply_flag_settings");
    assert_eq!(request["request"]["settings"], json!({"fastMode": true}));
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    let fast = option(&options, "fast").expect("the Fast mode option");
    assert_eq!(fast.option_type, "boolean");
    assert_eq!(fast.current_value, "true");

    // The "on"/"off" select fallback for a client without the boolean opt-in.
    let mut select = Actor::new(
        composed(
            base(models.clone(), "claude-sonnet-4-5", false, false),
            false,
        ),
        caps(false, false),
        false,
    );
    let (reply, reply_rx) = oneshot::channel();
    let _ = select.tx.send(Command::SetConfigOption {
        config_id: "fast".into(),
        value: json!("on"),
        reply,
    });
    let request = select.next_request().await;
    assert_eq!(request["request"]["settings"], json!({"fastMode": true}));
    select.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    let fast = option(&options, "fast").expect("the Fast mode option");
    assert_eq!(fast.option_type, "select");
    assert_eq!(fast.current_value, "on");
}

#[tokio::test]
async fn t5a2_fast_mode_rejects_an_invalid_value_and_keeps_state_on_sdk_failure() {
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let mut actor = Actor::new(
        composed(base(models.clone(), "claude-sonnet-4-5", true, false), true),
        caps(true, false),
        false,
    );
    let err = actor
        .set_config("fast", json!("yes"))
        .await
        .expect_err("invalid");
    assert_eq!(err, "Invalid value for config option fast: yes");
    let err = actor
        .set_config("fast", json!(null))
        .await
        .expect_err("invalid");
    assert_eq!(err, "Invalid value for config option fast: null");

    // A rejected SDK flag leaves both the session state and the option untouched.
    let err = actor
        .set_config_with("fast", json!(true), |_| Err("nope"))
        .await
        .expect_err("the SDK rejected the flag");
    assert_eq!(err, "nope");
    let options = actor.config().await;
    assert_eq!(option(&options, "fast").unwrap().current_value, "false");
    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::SetConfigOption {
        config_id: "fast".into(),
        value: json!(true),
        reply,
    });
    let request = actor.next_request().await;
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    assert_eq!(option(&options, "fast").unwrap().current_value, "true");
}

// ------------------------------------------------------- N-03 / N-31 / N-45

#[tokio::test]
async fn t5a2_init_reconciles_the_fast_mode_state() {
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let mut actor = Actor::new(
        composed(base(models.clone(), "claude-sonnet-4-5", true, false), true),
        caps(true, false),
        false,
    );
    let updates = actor
        .feed(json!({"type": "system", "subtype": "init", "fast_mode_state": "on"}))
        .await;
    let configs = updates_of(&updates, "config_option_update");
    assert_eq!(
        configs.len(),
        1,
        "the init frame reconciles the toggle: {updates:?}"
    );
    let options = wire_options(configs[0]);
    assert_eq!(option(&options, "fast").unwrap().current_value, "true");
}

#[tokio::test]
async fn t5a2_result_reconciles_fast_mode_and_ignores_cooldown_and_autonomous_results() {
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let mut config = base(models.clone(), "claude-sonnet-4-5", true, false);
    config.fast_mode_enabled = true;
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    // A rate-limit cooldown is transient: the toggle stays on and nothing is sent.
    let updates = actor
        .feed(
            json!({"type": "result", "subtype": "success", "is_error": false,
            "stop_reason": "end_turn", "fast_mode_state": "cooldown"}),
        )
        .await;
    assert!(updates_of(&updates, "config_option_update").is_empty());
    assert_eq!(
        option(&actor.config().await, "fast").unwrap().current_value,
        "true"
    );

    // A user-driven result reports off with a blocking reason: explain once.
    let updates = actor
        .feed(
            json!({"type": "result", "subtype": "success", "is_error": true,
            "stop_reason": "error", "fast_mode_state": "off",
            "fast_mode_disabled_reason": "extra_usage_disabled"}),
        )
        .await;
    let chunks = updates_of(&updates, "agent_message_chunk");
    assert_eq!(
        chunks[0]["update"]["content"]["text"],
        "**Fast mode turned off:** requires extra usage to be enabled for this account."
    );
    assert_eq!(updates_of(&updates, "config_option_update").len(), 1);
    // The same report again changes nothing the user can see.
    let updates = actor
        .feed(
            json!({"type": "result", "subtype": "success", "is_error": false,
            "stop_reason": "end_turn", "fast_mode_state": "off",
            "fast_mode_disabled_reason": "extra_usage_disabled"}),
        )
        .await;
    assert!(updates.is_empty(), "no repeat notice: {updates:?}");

    // An autonomous result's state is not reconciled.
    let updates = actor
        .feed(
            json!({"type": "result", "subtype": "success", "is_error": false,
            "stop_reason": "end_turn", "fast_mode_state": "on",
            "origin": {"kind": "task-notification"}}),
        )
        .await;
    assert!(updates.is_empty(), "autonomous result ignored: {updates:?}");
}

#[tokio::test]
async fn t5a2_fast_mode_reason_only_and_no_option_preserve_intent() {
    let models = vec![
        model("claude-sonnet-4-5", true, None, true),
        model("claude-haiku-4-5", false, None, true),
    ];
    let mut actor = Actor::new(
        composed(base(models.clone(), "claude-sonnet-4-5", true, false), true),
        caps(true, false),
        false,
    );
    let updates = actor
        .feed(
            json!({"type": "system", "subtype": "init", "fast_mode_state": "off",
            "fast_mode_disabled_reason": "not_first_party"}),
        )
        .await;
    let configs = updates_of(&updates, "config_option_update");
    assert_eq!(configs.len(), 1);
    assert_eq!(
        updates_of(&updates, "agent_message_chunk").len(),
        0,
        "no flip notice"
    );
    let fast = option(&wire_options(configs[0]), "fast").unwrap().clone();
    assert_eq!(fast.current_value, "false");
    assert!(
        fast.description
            .contains("not available on this API provider"),
        "the description carries the reason: {}",
        fast.description
    );

    // A model without Fast mode support: the reported state reflects
    // capability, not intent, so the retained setting is not clobbered.
    let mut config = base(models.clone(), "claude-haiku-4-5", true, false);
    config.fast_mode_enabled = true;
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    let updates = actor
        .feed(json!({"type": "system", "subtype": "init", "fast_mode_state": "off"}))
        .await;
    assert!(
        updates.is_empty(),
        "no Fast mode option: nothing to reconcile: {updates:?}"
    );
}

#[tokio::test]
async fn t5a2_fast_mode_flip_is_a_live_notice_for_notice_clients() {
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let mut config = base(models, "claude-sonnet-4-5", true, false);
    config.fast_mode_enabled = true;
    let mut actor = Actor::new(composed(config, true), caps(true, true), false);
    let updates = actor
        .feed(
            json!({"type": "system", "subtype": "init", "fast_mode_state": "off",
            "fast_mode_disabled_reason": "free"}),
        )
        .await;
    let notices = updates_of(&updates, "notice");
    assert_eq!(
        notices.len(),
        1,
        "an operational advisory, not transcript text: {updates:?}"
    );
    assert_eq!(notices[0]["update"]["severity"], "warning");
    assert_eq!(notices[0]["update"]["title"], "Fast mode turned off");
    assert_eq!(
        notices[0]["update"]["description"],
        "Not available on the free plan."
    );
    assert_eq!(updates_of(&updates, "config_option_update").len(), 1);
    assert_eq!(updates_of(&updates, "agent_message_chunk").len(), 0);
}

// ---------------------------------------------------------------- O-31

#[tokio::test]
async fn t5a2_fast_mode_intent_survives_a_model_switch() {
    let models = vec![
        model("claude-opus-4-5", true, None, true),
        model("claude-haiku-4-5", false, None, true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.fast_mode_enabled = true;
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    let options = actor
        .set_config("model", json!("claude-haiku-4-5"))
        .await
        .expect("the switch succeeds");
    assert!(
        option(&options, "fast").is_none(),
        "the toggle disappears with the model"
    );
    assert_eq!(
        option(&options, "model").unwrap().current_value,
        "claude-haiku-4-5"
    );

    // Back to a supporting model: the retained intent is surfaced again.
    let options = actor
        .set_config("model", json!("claude-opus-4-5"))
        .await
        .expect("the switch succeeds");
    assert_eq!(option(&options, "fast").unwrap().current_value, "true");
}

#[tokio::test]
async fn t5a2_model_not_allowed_reason_does_not_follow_the_switch() {
    let models = vec![
        model("claude-opus-4-5", true, None, true),
        model("claude-sonnet-4-6", true, None, true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.fast_mode_disabled_reason = Some("model_not_allowed".into());
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    let options = actor
        .set_config("model", json!("claude-sonnet-4-6"))
        .await
        .expect("the switch succeeds");
    assert!(
        !option(&options, "fast")
            .unwrap()
            .description
            .contains("selected model"),
        "the old model's reason must not decorate the new option"
    );
}

// ------------------------------------------------------- A-08 / A-09 / O-55

#[tokio::test]
async fn t5a2_air_client_syncs_the_displayed_effort_after_a_model_switch() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", true, Some(&["low", "medium"]), true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", false, true);
    config.current_effort = Some("low".into());
    config.applied_effort = Some("low".into());
    let mut actor = Actor::new(composed(config, false), caps(false, false), true);
    let options = actor
        .set_config_with("model", json!("claude-sonnet-4-6"), |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                assert_eq!(
                    request["request"]["settings"],
                    json!({"effortLevel": "medium"}),
                    "the AIR client applies its concrete displayed effort"
                );
            }
            Ok(json!({}))
        })
        .await
        .expect("the switch succeeds");
    assert_eq!(option(&options, "effort").unwrap().current_value, "medium");
}

#[tokio::test]
async fn t5a2_legacy_pin_clamped_away_clears_the_flag_layer() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high", "max"]),
            true,
        ),
        model("claude-sonnet-4-6", true, Some(&["low", "medium"]), true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", false, false);
    config.current_effort = Some("max".into());
    config.effort_pinned = Some("max".into());
    config.applied_effort = Some("max".into());
    let mut actor = Actor::new(composed(config, false), caps(false, false), false);
    let options = actor
        .set_config_with("model", json!("claude-sonnet-4-6"), |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                assert_eq!(
                    request["request"]["settings"],
                    json!({"effortLevel": null}),
                    "a clamped legacy pin clears the flag layer, never the displayed value"
                );
            }
            Ok(json!({}))
        })
        .await
        .expect("the switch succeeds");
    assert_eq!(option(&options, "effort").unwrap().current_value, "default");
}

#[tokio::test]
async fn t5a2_effort_sync_failure_restores_the_last_applied_value() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", true, Some(&["low", "medium"]), true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", false, true);
    config.current_effort = Some("low".into());
    config.applied_effort = Some("low".into());
    let mut actor = Actor::new(composed(config, false), caps(false, false), true);
    let options = actor
        .set_config_with("model", json!("claude-sonnet-4-6"), |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                return Err("effort sync failed");
            }
            Ok(json!({}))
        })
        .await
        .expect("the model switch itself succeeded");
    assert_eq!(
        option(&options, "effort").unwrap().current_value,
        "low",
        "the last applied value is restored when the new model can select it"
    );
    assert_eq!(
        option(&options, "model").unwrap().current_value,
        "claude-sonnet-4-6"
    );
}

#[tokio::test]
async fn t5a2_effort_sync_failure_drops_the_option_when_the_value_is_unselectable() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", true, Some(&["low", "medium"]), true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", false, true);
    config.current_effort = Some("high".into());
    config.applied_effort = Some("high".into());
    let mut actor = Actor::new(composed(config, false), caps(false, false), true);
    let options = actor
        .set_config_with("model", json!("claude-sonnet-4-6"), |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                return Err("effort sync failed");
            }
            Ok(json!({}))
        })
        .await
        .expect("the model switch itself succeeded");
    assert!(
        option(&options, "effort").is_none(),
        "an unapplied value the new model cannot select drops the option"
    );
}

#[tokio::test]
async fn t5a2_legacy_clamp_failure_retains_the_original_pin() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high", "max"]),
            true,
        ),
        model("claude-sonnet-4-6", true, Some(&["low", "medium"]), true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", false, false);
    config.current_effort = Some("max".into());
    config.effort_pinned = Some("max".into());
    config.applied_effort = Some("max".into());
    let mut actor = Actor::new(composed(config, false), caps(false, false), false);
    let options = actor
        .set_config_with("model", json!("claude-sonnet-4-6"), |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                return Err("effort clear failed");
            }
            Ok(json!({}))
        })
        .await
        .expect("the model switch itself succeeded");
    assert!(
        option(&options, "effort").is_none(),
        "the unapplied pin is not advertised on the new model"
    );

    // Back on a model that supports the pin: it was retained, not cleared.
    let options = actor
        .set_config("model", json!("claude-opus-4-5"))
        .await
        .expect("the switch succeeds");
    assert_eq!(option(&options, "effort").unwrap().current_value, "max");
}

#[tokio::test]
async fn t5a2_switch_to_a_model_without_effort_clears_the_pinned_flag() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high", "max"]),
            true,
        ),
        model("claude-sonnet-4-6", true, None, true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", false, false);
    config.current_effort = Some("max".into());
    config.effort_pinned = Some("max".into());
    config.applied_effort = Some("max".into());
    let mut actor = Actor::new(composed(config, false), caps(false, false), false);
    let options = actor
        .set_config_with("model", json!("claude-sonnet-4-6"), |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                assert_eq!(
                    request["request"]["settings"],
                    json!({"effortLevel": null}),
                    "a pin the new model cannot carry is cleared at the flag layer"
                );
            }
            Ok(json!({}))
        })
        .await
        .expect("the switch succeeds");
    assert!(option(&options, "effort").is_none());
}

#[tokio::test]
async fn t5a2_direct_effort_selection_keeps_state_on_sdk_rejection() {
    let models = vec![model(
        "claude-opus-4-5",
        true,
        Some(&["low", "medium", "high"]),
        true,
    )];
    let mut config = base(models, "claude-opus-4-5", false, false);
    config.current_effort = Some("low".into());
    config.applied_effort = Some("low".into());
    let mut actor = Actor::new(composed(config, false), caps(false, false), false);
    let err = actor
        .set_config_with("effort", json!("high"), |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                return Err("effort rejected");
            }
            Ok(json!({}))
        })
        .await
        .expect_err("the SDK rejected the selection");
    assert_eq!(err, "effort rejected");
    assert_eq!(
        option(&actor.config().await, "effort")
            .unwrap()
            .current_value,
        "low",
        "a rejected direct selection leaves the effort state unchanged"
    );

    let err = actor
        .set_config("effort", json!("bogus"))
        .await
        .expect_err("invalid effort");
    assert_eq!(err, "Invalid value for config option effort: bogus");
}

// ---------------------------------------------------------------- A-03

#[tokio::test]
async fn t5a2_agent_switch_applies_the_flag_and_null_for_default() {
    let models = vec![model("claude-opus-4-5", true, None, true)];
    let mut config = base(models.clone(), "claude-opus-4-5", false, false);
    config.agents = vec![AgentInfo {
        name: "reviewer".into(),
        description: "Reviews".into(),
    }];
    let mut actor = Actor::new(composed(config, false), caps(false, false), false);
    assert!(option(&actor.config().await, "agent").is_some());

    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::SetConfigOption {
        config_id: "agent".into(),
        value: json!("reviewer"),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "apply_flag_settings");
    assert_eq!(request["request"]["settings"], json!({"agent": "reviewer"}));
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    assert_eq!(option(&options, "agent").unwrap().current_value, "reviewer");

    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::SetConfigOption {
        config_id: "agent".into(),
        value: json!("default"),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(request["request"]["settings"], json!({"agent": null}));
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    assert_eq!(option(&options, "agent").unwrap().current_value, "default");

    // A rejected flag leaves the picker untouched.
    let err = actor
        .set_config_with("agent", json!("reviewer"), |_| Err("nope"))
        .await
        .expect_err("the SDK rejected the flag");
    assert_eq!(err, "nope");
    assert_eq!(
        option(&actor.config().await, "agent")
            .unwrap()
            .current_value,
        "default"
    );
}

// ---------------------------------------------------------------- R-01

#[tokio::test]
async fn t5a2_external_switch_reconciles_mode_effort_fast_and_notifies() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", false, Some(&["low", "medium"]), false),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", true, true);
    config.current_effort = Some("low".into());
    config.applied_effort = Some("low".into());
    config.fast_mode_enabled = true;
    config.modes.current_mode_id = "auto".into();
    let mut actor = Actor::new(composed(config, true), caps(true, false), true);
    let updates = actor
        .external_switch("claude-sonnet-4-6", |_| Ok(json!({})))
        .await;
    // Auto is invalidated by the new model: current_mode_update + the warning,
    // then the external switch's config_option_update.
    let modes = updates_of(&updates, "current_mode_update");
    assert_eq!(modes.len(), 1);
    assert_eq!(modes[0]["update"]["currentModeId"], "acceptEdits");
    assert_eq!(
        updates_of(&updates, "agent_message_chunk").len(),
        1,
        "the fallback warning"
    );
    let configs = updates_of(&updates, "config_option_update");
    assert_eq!(
        configs.len(),
        1,
        "the external switch notifies the client: {updates:?}"
    );
    // Upstream publishes the fallback updates, then the switch's
    // `config_option_update` (`acp-agent.ts:10531-10533`, `10606`).
    let index = |kind: &str| {
        updates
            .iter()
            .position(|u| u["update"]["sessionUpdate"] == kind)
            .unwrap_or_else(|| panic!("{kind} in {updates:?}"))
    };
    assert!(
        index("current_mode_update") < index("agent_message_chunk"),
        "the mode update precedes the warning: {updates:?}"
    );
    assert!(
        index("agent_message_chunk") < index("config_option_update"),
        "the warning precedes the switch's config_option_update: {updates:?}"
    );
    let options = wire_options(configs[0]);
    assert_eq!(
        option(&options, "model").unwrap().current_value,
        "claude-sonnet-4-6"
    );
    assert_eq!(option(&options, "effort").unwrap().current_value, "medium");
    // The new model does not support Fast mode: the retained intent stays on.
    assert!(option(&options, "fast").is_none());
    assert!(option(&actor.config().await, "fast").is_none());
}

#[tokio::test]
async fn t5a2_external_switch_publishes_new_state_when_effort_sync_fails() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", true, Some(&["low", "medium"]), true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", true, true);
    config.current_effort = Some("low".into());
    config.applied_effort = Some("low".into());
    let mut actor = Actor::new(composed(config, true), caps(true, false), true);
    let updates = actor
        .external_switch("claude-sonnet-4-6", |request| {
            if request["request"]["subtype"] == "apply_flag_settings" {
                return Err("effort sync failed");
            }
            Ok(json!({}))
        })
        .await;
    let configs = updates_of(&updates, "config_option_update");
    assert_eq!(configs.len(), 1, "the switch still notifies: {updates:?}");
    let options = wire_options(configs[0]);
    assert_eq!(
        option(&options, "model").unwrap().current_value,
        "claude-sonnet-4-6"
    );
    assert_eq!(option(&options, "effort").unwrap().current_value, "low");
}

#[tokio::test]
async fn t5a2_post_model_switch_hook_reconciles_the_external_switch() {
    let models = vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", true, Some(&["low", "medium"]), true),
    ];
    let mut config = base(models.clone(), "claude-opus-4-5", true, true);
    config.current_effort = Some("low".into());
    config.applied_effort = Some("low".into());
    let mut actor = Actor::new(composed(config, true), caps(true, false), true);
    // A PostModelSwitch hook reported a CLI-driven switch (`source: "cli"`).
    let _ = actor.control_in.send(json!({
        "type": "control_request", "request_id": "hook1",
        "request": {"subtype": "hook_callback", "callback_id": "hook_1",
            "input": {"hook_event_name": "PostModelSwitch", "source": "cli",
                      "to_model": "claude-sonnet-4-6"}},
    }));
    let mut seen = Vec::new();
    loop {
        tokio::select! {
            line = actor.written.recv() => {
                let Some(line) = line else { panic!("the stdin reader ended") };
                let request: Value = serde_json::from_str(&line).expect("a JSON line");
                if request.get("type").and_then(Value::as_str) != Some("control_request") {
                    continue;
                }
                let id = request["request_id"].as_str().expect("a request id");
                actor.respond_ok(id);
            }
            update = actor.updates.recv() => {
                match update {
                    Some(SessionOutbound::Flush(ack)) => { let _ = ack.send(()); }
                    Some(SessionOutbound::Update(notif)) => {
                        seen.push(serde_json::to_value(&notif).unwrap());
                    }
                    Some(SessionOutbound::RawUpdate(value)) => seen.push(value),
                    _ => {}
                }
                if seen.iter().any(|u| u["update"]["sessionUpdate"] == "config_option_update") {
                    break;
                }
            }
        }
    }
    let configs = updates_of(&seen, "config_option_update");
    assert_eq!(configs.len(), 1, "the hook's switch is notified: {seen:?}");
    let options = wire_options(configs[0]);
    assert_eq!(
        option(&options, "model").unwrap().current_value,
        "claude-sonnet-4-6"
    );
    assert_eq!(option(&options, "effort").unwrap().current_value, "medium");

    // The adapter's own `setModel` (`source: "sdk"`) already synced: no second
    // reconcile, so the hook adds no config_option_update.
    let mut actor = Actor::new(
        composed(base(models.clone(), "claude-opus-4-5", true, true), true),
        caps(true, false),
        true,
    );
    let _ = actor.control_in.send(json!({
        "type": "control_request", "request_id": "hook2",
        "request": {"subtype": "hook_callback", "callback_id": "hook_1",
            "input": {"hook_event_name": "PostModelSwitch", "source": "sdk",
                      "to_model": "claude-sonnet-4-6"}},
    }));
    // Wait for the hook's own control_response and any update.
    let mut updates = Vec::new();
    let _ = timeout(Duration::from_millis(300), async {
        loop {
            tokio::select! {
                _ = actor.written.recv() => {}
                update = actor.updates.recv() => {
                    if let Some(SessionOutbound::RawUpdate(value)) = update {
                        updates.push(value);
                    }
                }
            }
        }
    })
    .await;
    assert!(
        updates_of(&updates, "config_option_update").is_empty(),
        "an SDK-driven switch is not reconciled twice: {updates:?}"
    );
}

// ---------------------------------------------------------------- P-06

#[tokio::test]
async fn t5a2_initial_auto_clamp_warns_at_the_first_prompt_once() {
    let models = vec![model("claude-haiku-4-5", true, None, false)];
    let mut config = base(models, "claude-haiku-4-5", false, false);
    config.modes.current_mode_id = "acceptEdits".into();
    config.pending_auto_fallback_warning = true;
    let mut actor = Actor::new(composed(config, false), caps(false, false), false);
    let prompt = json!({"type": "user", "uuid": "p1",
        "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}});
    let updates = actor.prompt(prompt.clone()).await;
    let warnings = updates_of(&updates, "agent_message_chunk");
    assert_eq!(
        warnings.len(),
        1,
        "the deferred warning is published: {updates:?}"
    );
    assert!(warnings[0]["update"]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("Auto mode unavailable"));
    // A second prompt does not repeat it.
    let updates = actor.prompt(prompt).await;
    assert!(
        !updates.iter().any(|u| {
            u["update"]["content"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("Auto mode unavailable"))
        }),
        "shown once per session: {updates:?}"
    );
}

#[tokio::test]
async fn t5a2_auto_fallback_warning_is_a_live_notice_for_notice_clients() {
    let models = vec![model("claude-haiku-4-5", true, None, false)];
    let mut config = base(models, "claude-haiku-4-5", false, false);
    config.modes.current_mode_id = "acceptEdits".into();
    config.pending_auto_fallback_warning = true;
    let mut actor = Actor::new(composed(config, false), caps(false, true), false);
    let updates = actor
        .prompt(json!({"type": "user", "uuid": "p1",
            "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}))
        .await;
    let notices = updates_of(&updates, "notice");
    assert_eq!(
        notices.len(),
        1,
        "the notice contract gets a live notice: {updates:?}"
    );
    assert_eq!(notices[0]["update"]["severity"], "warning");
    assert_eq!(notices[0]["update"]["title"], "Auto mode unavailable");
    assert_eq!(updates_of(&updates, "agent_message_chunk").len(), 0);
}

// ---------------------------------------------------------------- P-04

#[tokio::test]
async fn t5a2_a_background_subagent_permission_request_goes_to_the_child_session() {
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let config = composed(base(models, "claude-sonnet-4-5", false, false), false);
    let mut client_caps = caps(false, false);
    client_caps.subagent_form = Some(crate::client_caps::SubagentForm::Rfd);
    let mut actor = Actor::new(config, client_caps, false);
    // A prompt is active so the Agent call can launch a child.
    let _ = actor.tx.send(Command::Prompt {
        uuid: "p1".into(),
        frame: json!({"type": "user", "uuid": "p1",
            "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}),
        is_local_only: false,
        prompt_text: "go".into(),
        reply: oneshot::channel().0,
    });
    // The parent Agent call, then the child's task_started announcement.
    let _ = actor
        .feed(json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"id": "m1", "role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_A", "name": "Agent",
                 "input": {"description": "Fix", "prompt": "Make it pass"}}]}}))
        .await;
    let updates = actor
        .feed(json!({"type": "system", "subtype": "task_started",
            "task_id": "t1", "tool_use_id": "toolu_A",
            "subagent_type": "general-purpose", "description": "Fix",
            "prompt": "Make it pass"}))
        .await;
    let child = updates
        .iter()
        .find(|u| u["update"]["sessionUpdate"] == "subagent_update")
        .map(|u| u["update"]["sessionId"].as_str().unwrap().to_string())
        .expect("the child session is announced");
    assert_ne!(child, SESSION_ID);
    // The child's background Bash approval arrives as a `can_use_tool` request.
    let _ = actor.control_in.send(json!({
        "type": "control_request", "request_id": "perm1",
        "request": {"subtype": "can_use_tool", "tool_name": "Bash",
            "input": {"command": "printf SUBAGENT_APPROVAL_OK > approval.txt"},
            "tool_use_id": "toolu_P", "agent_id": "t1", "permission_suggestions": []},
    }));
    let mut request = None;
    for _ in 0..20 {
        match timeout(Duration::from_secs(5), actor.updates.recv()).await {
            Ok(Some(SessionOutbound::RequestPermission { params, .. })) => {
                request = Some(params);
                break;
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    let params = request.expect("the permission request reaches the client");
    assert_eq!(
        params["sessionId"].as_str().unwrap(),
        child,
        "a background subagent's approval is routed to its child session"
    );
    assert_eq!(
        params["toolCall"]["_meta"]["claudeCode"]["parentToolUseId"],
        "toolu_A"
    );
}

// ------------------------------------------------- T5FA2: stream holds / order

/// A downgrade-plus-effort-sync switch on a legacy client: Auto is invalidated
/// on the new model, so the actor sends `set_permission_mode` and then
/// `apply_flag_settings`.
fn downgrade_models() -> Vec<ModelInfo> {
    vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", false, Some(&["low", "medium"]), false),
    ]
}

/// A strict top-level assistant text frame (`parent_tool_use_id: null`, the
/// only shape whose text the adapter forwards) that publishes a chunk.
fn assistant_text(text: &str) -> Value {
    json!({"type": "assistant", "parent_tool_use_id": null,
        "message": {"id": "m-held", "role": "assistant",
            "content": [{"type": "text", "text": text}]}})
}

#[tokio::test]
async fn t5a2_the_refusal_switch_holds_stream_frames_until_its_config_update() {
    let models = downgrade_models();
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.modes.current_mode_id = "auto".into();
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    // The refusal fallback is inline: its `set_permission_mode` is pending.
    actor.start_external_switch("claude-sonnet-4-6", true);
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "set_permission_mode");
    actor.send_frame(assistant_text("held"));
    let early = actor.drain_for(Duration::from_millis(150)).await;
    assert!(
        updates_of(&early, "agent_message_chunk").is_empty(),
        "the inline switch holds the stream: {early:?}"
    );
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let updates = actor.drain_for(Duration::from_secs(2)).await;
    let configs = updates_of(&updates, "config_option_update");
    assert_eq!(configs.len(), 1, "the switch notifies: {updates:?}");
    let chunk = updates
        .iter()
        .position(|u| {
            u["update"]["sessionUpdate"] == "agent_message_chunk"
                && u["update"]["content"]["text"] == "held"
        })
        .unwrap_or_else(|| {
            panic!("the held frame is processed once the switch settles: {updates:?}")
        });
    let switch = updates
        .iter()
        .position(|u| u["update"]["sessionUpdate"] == "config_option_update")
        .expect("config_option_update");
    assert!(
        switch < chunk,
        "the switch's config_option_update precedes the held frame: {updates:?}"
    );
}

#[tokio::test]
async fn t5a2_a_user_switch_does_not_release_an_external_switch_stream_hold() {
    let models = downgrade_models();
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.modes.current_mode_id = "auto".into();
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    actor.start_external_switch("claude-sonnet-4-6", true);
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "set_permission_mode");
    // A user-driven switch (a no-op on the current model) completes while the
    // external one is still awaiting its round trip. It must not clear the
    // hold: the counter only counts external switches.
    actor
        .user_model_switch("claude-opus-4-5")
        .await
        .expect("the user switch replies");
    actor.send_frame(assistant_text("held"));
    let early = actor.drain_for(Duration::from_millis(150)).await;
    assert!(
        updates_of(&early, "agent_message_chunk").is_empty(),
        "an unrelated user switch releases the hold: {early:?}"
    );
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let updates = actor.drain_for(Duration::from_secs(2)).await;
    let switch = updates
        .iter()
        .position(|u| u["update"]["sessionUpdate"] == "config_option_update")
        .unwrap_or_else(|| panic!("the switch notifies: {updates:?}"));
    let chunk = updates
        .iter()
        .position(|u| {
            u["update"]["sessionUpdate"] == "agent_message_chunk"
                && u["update"]["content"]["text"] == "held"
        })
        .unwrap_or_else(|| panic!("the held frame is processed: {updates:?}"));
    assert!(
        switch < chunk,
        "the switch's config_option_update precedes the held frame: {updates:?}"
    );
}

#[tokio::test]
async fn t5a2_the_hook_switch_does_not_hold_stream_frames() {
    let models = downgrade_models();
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.modes.current_mode_id = "auto".into();
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    // Upstream runs the hook's sync detached (`setImmediate`,
    // `acp-agent.ts:11328-11337`): the consumer loop keeps reading.
    let _ = actor.control_in.send(json!({
        "type": "control_request", "request_id": "hook1",
        "request": {"subtype": "hook_callback", "callback_id": "hook_1",
            "input": {"hook_event_name": "PostModelSwitch", "source": "cli",
                      "to_model": "claude-sonnet-4-6"}},
    }));
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "set_permission_mode");
    actor.send_frame(assistant_text("flowing"));
    let early = actor.drain_for(Duration::from_millis(500)).await;
    assert!(
        updates_of(&early, "agent_message_chunk")
            .iter()
            .any(|u| u["update"]["content"]["text"] == "flowing"),
        "a detached hook switch does not hold the stream: {early:?}"
    );
    assert!(
        updates_of(&early, "config_option_update").is_empty(),
        "the hook's config_option_update waits for its round trip: {early:?}"
    );
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let later = actor.drain_for(Duration::from_secs(2)).await;
    let configs = updates_of(&later, "config_option_update");
    assert_eq!(configs.len(), 1, "the hook's switch notifies: {later:?}");
}

#[tokio::test]
async fn t5a2_auto_clamp_precedes_the_effort_sync_on_a_switch() {
    let models = downgrade_models();
    let mut config = base(models.clone(), "claude-opus-4-5", true, true);
    config.modes.current_mode_id = "auto".into();
    config.current_effort = Some("low".into());
    config.applied_effort = Some("low".into());
    let mut actor = Actor::new(composed(config, true), caps(true, false), true);
    actor.start_external_switch("claude-sonnet-4-6", true);
    let first = actor.next_request().await;
    assert_eq!(
        first["request"]["subtype"], "set_permission_mode",
        "the auto clamp is awaited first (`session-mode.ts:225-244`)"
    );
    actor.respond_ok(first["request_id"].as_str().unwrap());
    let second = actor.next_request().await;
    assert_eq!(
        second["request"]["subtype"], "apply_flag_settings",
        "the effort sync follows the clamp: {second:?}"
    );
    assert_eq!(
        second["request"]["settings"],
        json!({"effortLevel": "medium"})
    );
    actor.respond_ok(second["request_id"].as_str().unwrap());
    let updates = actor.drain_for(Duration::from_secs(2)).await;
    assert_eq!(
        updates_of(&updates, "config_option_update").len(),
        1,
        "the switch completes: {updates:?}"
    );
}

#[tokio::test]
async fn t5a2_state_stays_unchanged_while_the_sdk_request_is_pending() {
    // Fast mode: the option flips only after the CLI accepts the flag.
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let mut actor = Actor::new(
        composed(base(models.clone(), "claude-sonnet-4-5", true, false), true),
        caps(true, false),
        false,
    );
    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::SetConfigOption {
        config_id: "fast".into(),
        value: json!(true),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(request["request"]["settings"], json!({"fastMode": true}));
    let pending = actor.config().await;
    assert_eq!(
        option(&pending, "fast").unwrap().current_value,
        "false",
        "the toggle is unchanged while the flag is in flight"
    );
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    assert_eq!(option(&options, "fast").unwrap().current_value, "true");

    // Effort: the displayed value follows the accepted flag.
    let effort_models = vec![model(
        "claude-sonnet-4-5",
        true,
        Some(&["low", "medium", "high"]),
        true,
    )];
    let mut config = base(effort_models, "claude-sonnet-4-5", true, true);
    config.current_effort = Some("low".into());
    let mut actor = Actor::new(composed(config, true), caps(true, false), true);
    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::SetConfigOption {
        config_id: "effort".into(),
        value: json!("high"),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(
        request["request"]["settings"],
        json!({"effortLevel": "high"})
    );
    let pending = actor.config().await;
    assert_eq!(
        option(&pending, "effort").unwrap().current_value,
        "low",
        "the effort is unchanged while the flag is in flight"
    );
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    assert_eq!(option(&options, "effort").unwrap().current_value, "high");

    // Agent: the picker moves only after the CLI accepts the persona flag.
    let mut config = base(models, "claude-sonnet-4-5", true, false);
    config.agents = vec![AgentInfo {
        name: "reviewer".into(),
        description: "Reviews".into(),
    }];
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::SetConfigOption {
        config_id: "agent".into(),
        value: json!("reviewer"),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(request["request"]["settings"], json!({"agent": "reviewer"}));
    let pending = actor.config().await;
    assert_eq!(
        option(&pending, "agent").unwrap().current_value,
        "default",
        "the agent is unchanged while the flag is in flight"
    );
    actor.respond_ok(request["request_id"].as_str().unwrap());
    let options = timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("reply")
        .expect("the actor replies")
        .expect("Ok");
    assert_eq!(option(&options, "agent").unwrap().current_value, "reviewer");
}

#[tokio::test]
async fn t5a2_an_unknown_option_id_records_the_value_and_replies() {
    let models = vec![model("claude-sonnet-4-5", true, None, true)];
    let mut config = composed(base(models, "claude-sonnet-4-5", true, false), true);
    // Upstream's final `else` (`acp-agent.ts:10717-10721`) records any option id
    // outside mode/model/effort/agent/fast and returns the options.
    config.config_options.push(ConfigOption {
        id: "verbosity".into(),
        name: "Verbosity".into(),
        description: String::new(),
        category: "other".into(),
        option_type: "select".into(),
        current_value: "normal".into(),
        options: vec![
            crate::session_config::ConfigOptionValue {
                value: "normal".into(),
                name: "Normal".into(),
                description: None,
                _meta: None,
            },
            crate::session_config::ConfigOptionValue {
                value: "verbose".into(),
                name: "Verbose".into(),
                description: None,
                _meta: None,
            },
        ],
        _meta: None,
    });
    let mut actor = Actor::new(config, caps(true, false), false);
    let options = actor
        .set_config("verbosity", json!("verbose"))
        .await
        .expect("an unknown-but-present option id replies");
    assert_eq!(
        option(&options, "verbosity").unwrap().current_value,
        "verbose"
    );
    let live = actor.config().await;
    assert_eq!(option(&live, "verbosity").unwrap().current_value, "verbose");
}

// --------------------------------------------- T5FA3: inline switch vs. stream

/// REVIEW REPRO (T5FA finding 1): in production the child's `control_response`
/// arrives on the SAME stream the old `inline_external_switches_in_flight == 0`
/// gate disabled, so the switch could never complete. The response now rides
/// the stream, as the real process does.
#[tokio::test]
async fn t5fa3_an_inline_switch_completes_when_the_control_response_rides_the_stream() {
    let models = downgrade_models();
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.modes.current_mode_id = "auto".into();
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    actor.start_external_switch("claude-sonnet-4-6", true);
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "set_permission_mode");
    actor.respond_ok(request["request_id"].as_str().expect("a request id"));
    let updates = actor.drain_for(Duration::from_secs(3)).await;
    let configs = updates_of(&updates, "config_option_update");
    assert_eq!(configs.len(), 1, "the switch completes: {updates:?}");
    assert_eq!(
        option(&wire_options(configs[0]), "model")
            .unwrap()
            .current_value,
        "claude-sonnet-4-6"
    );
}

/// Frames that arrive while an inline switch runs are held (in their original
/// order) and released after its `config_option_update`.
#[tokio::test]
async fn t5fa3_frames_arriving_during_an_inline_switch_are_released_in_order() {
    let models = downgrade_models();
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.modes.current_mode_id = "auto".into();
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    actor.start_external_switch("claude-sonnet-4-6", true);
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "set_permission_mode");
    actor.send_frame(assistant_text("one"));
    actor.send_frame(assistant_text("two"));
    let early = actor.drain_for(Duration::from_millis(200)).await;
    assert!(
        updates_of(&early, "agent_message_chunk").is_empty(),
        "the inline switch holds the frames: {early:?}"
    );
    actor.respond_ok(request["request_id"].as_str().expect("a request id"));
    let updates = actor.drain_for(Duration::from_secs(3)).await;
    let texts: Vec<&str> = updates
        .iter()
        .filter(|u| u["update"]["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["update"]["content"]["text"].as_str())
        .filter(|text| matches!(*text, "one" | "two"))
        .collect();
    assert_eq!(
        texts,
        vec!["one", "two"],
        "the held frames are released in order: {updates:?}"
    );
    let switch = updates
        .iter()
        .position(|u| u["update"]["sessionUpdate"] == "config_option_update")
        .unwrap_or_else(|| panic!("the switch notifies: {updates:?}"));
    let first_chunk = updates
        .iter()
        .position(|u| {
            u["update"]["sessionUpdate"] == "agent_message_chunk"
                && u["update"]["content"]["text"] == "one"
        })
        .expect("the held frames run");
    assert!(
        switch < first_chunk,
        "the switch's config_option_update precedes the held frames: {updates:?}"
    );
}

/// The child's stdout closes while an inline switch holds frames: the EOF lane
/// fails the switch's pending round trip (`Control::fail_pending`), so its
/// `ModelSwitchSynced` still reaches the loop, publishes its
/// `config_option_update` before the held frames, and then the held frames run
/// (the `/mcp` EOF lane).
#[tokio::test]
async fn t5fa3_eof_while_an_inline_switch_holds_processing_the_held_frames() {
    let models = downgrade_models();
    let mut config = base(models.clone(), "claude-opus-4-5", true, false);
    config.modes.current_mode_id = "auto".into();
    let mut actor = Actor::new(composed(config, true), caps(true, false), false);
    actor.start_external_switch("claude-sonnet-4-6", true);
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "set_permission_mode");
    actor.send_frame(assistant_text("held"));
    let early = actor.drain_for(Duration::from_millis(200)).await;
    assert!(
        updates_of(&early, "agent_message_chunk").is_empty(),
        "the inline switch holds the frame: {early:?}"
    );
    actor.close_stream();
    let after = actor.drain_for(Duration::from_secs(3)).await;
    let chunk = after.iter().position(|u| {
        u["update"]["sessionUpdate"] == "agent_message_chunk"
            && u["update"]["content"]["text"] == "held"
    });
    assert!(
        chunk.is_some(),
        "the held frame runs after the EOF settles the switch: {after:?}"
    );
    let switch = after
        .iter()
        .position(|u| u["update"]["sessionUpdate"] == "config_option_update");
    assert!(
        switch.is_some() && switch < chunk,
        "the switch's config_option_update precedes the held frame: {after:?}"
    );
}
