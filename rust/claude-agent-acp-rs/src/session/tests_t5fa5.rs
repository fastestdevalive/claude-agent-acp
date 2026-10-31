//! T5FA5: the EOF-wait state is sticky but self-healing.
//!
//! * `Control::fail_pending` latches until `rebind`, so a request enqueued
//!   after the child's stdout EOF (the switch task had not polled yet) settles
//!   `Closed` instead of waiting for a reply that can never come.
//! * `eof_seen` is cleared only when a fresh child is adopted, so a later EOF
//!   re-enters the lane only after a restart, and an inline switch on a
//!   rebound child is not gated by a stale EOF.
//!
//! The rig drives the REAL actor loop (`run_loop` + a real `Control` over a
//! duplex stdin) and observes everything on the wire.

use super::*;
use crate::session_config::{
    build_available_modes, build_config_options_with, model_supports_fast_mode, ConfigOptionsInput,
    FastModeOptionState, ModelInfo, ModesState, SessionConfigState,
};
use tokio::time::timeout;

const SESSION_ID: &str = "s";

/// A running actor with its control channel, its stream and its outbound
/// updates, plus the lines the actor wrote to the child's stdin.
pub(super) struct Actor {
    tx: mpsc::UnboundedSender<Command>,
    updates: mpsc::UnboundedReceiver<SessionOutbound>,
    stream: Option<mpsc::UnboundedSender<Value>>,
    /// Kept alive so the control task's inbound stream does not close before
    /// the actor ends (never read).
    _control_in: mpsc::UnboundedSender<Value>,
    written: mpsc::UnboundedReceiver<String>,
}

impl Actor {
    pub(super) fn new(
        config: SessionConfigState,
        caps: crate::client_caps::ClientCapabilities,
    ) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
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
        let written = stdin_lines(b);
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control.clone(),
            control_in_tx.clone(),
            update_tx,
            Duration::from_secs(30),
            SESSION_ID.into(),
            Arc::new(AtomicUsize::new(0)),
            None,
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
            _control_in: control_in_tx,
            written,
        };
        let _ = actor.tx.send(Command::InitConfig {
            config: Box::new(config),
        });
        actor
    }

    /// The next `control_request` line the actor wrote, parsed.
    pub(super) async fn next_request(&mut self) -> Value {
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

    /// Send one frame on the child's stdout (used by `tests_t5fa6`).
    pub(super) fn rv_send(&self, frame: Value) {
        self.stream
            .as_ref()
            .expect("stream open")
            .send(frame)
            .unwrap();
    }

    /// The child's stdout closes (stream EOF).
    pub(super) fn close_stream(&mut self) {
        self.stream = None;
    }

    /// Start an external `ModelApplied` (`notify_client: true`) without
    /// answering its control requests: the caller drives the pending round
    /// trips itself.
    pub(super) fn start_external_switch(&self, model: &str, inline: bool) {
        let (reply, _reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::ModelApplied {
            model_val: model.into(),
            result: Ok(()),
            reply,
            notify_client: true,
            inline,
        });
    }

    /// Drain the updates published within `window`, acknowledging Flushes,
    /// without sending anything into the session.
    pub(super) async fn drain_for(&mut self, window: Duration) -> Vec<Value> {
        let (out, _) = self.drain_until_closed(window).await;
        out
    }

    /// Like [`Actor::drain_for`], and report whether the actor ended (its
    /// updates sender dropped) inside the window.
    pub(super) async fn drain_until_closed(&mut self, window: Duration) -> (Vec<Value>, bool) {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return (out, false);
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
                Ok(None) => return (out, true),
                Err(_) => return (out, false),
            }
        }
    }
}

/// Spawn the stdin line reader of one duplex half.
fn stdin_lines(reader: tokio::io::DuplexStream) -> mpsc::UnboundedReceiver<String> {
    let (lines_tx, lines) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = lines_tx.send(trimmed.to_string());
                    }
                }
            }
        }
    });
    lines
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
fn base(models: Vec<ModelInfo>, current: &str, recommended: bool) -> SessionConfigState {
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
        effort_settings_override: None,
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

fn updates_of<'a>(updates: &'a [Value], kind: &str) -> Vec<&'a Value> {
    updates
        .iter()
        .filter(|u| u["update"]["sessionUpdate"] == kind)
        .collect()
}

/// A legacy client on Auto with a pinned `high` effort:
/// * opus -> sonnet: Auto is invalidated (a `set_permission_mode`) and the pin
///   is clamped away (an `apply_flag_settings`).
/// * sonnet -> haiku: only the pin is clamped (`apply_flag_settings`), because
///   the mode already fell back.
fn pin_clamp_models() -> Vec<ModelInfo> {
    vec![
        model(
            "claude-opus-4-5",
            true,
            Some(&["low", "medium", "high"]),
            true,
        ),
        model("claude-sonnet-4-6", false, Some(&["low", "medium"]), false),
        model("claude-haiku-4-5", true, Some(&["low"]), true),
    ]
}

pub(super) fn pin_clamp_actor() -> Actor {
    let models = pin_clamp_models();
    let mut config = base(models.clone(), "claude-opus-4-5", false);
    config.modes.current_mode_id = "auto".into();
    config.effort_pinned = Some("high".into());
    Actor::new(composed(config, true), caps(true, false))
}

/// T5FA4 review R2: EOF queued before the switch task ever runs. The request
/// it enqueues afterwards must settle `Closed` (the latch), so the switch still
/// publishes its `config_option_update` instead of wedging the actor.
#[tokio::test]
async fn t5fa5_eof_before_the_switch_task_runs() {
    for i in 0..20 {
        let mut actor = pin_clamp_actor();
        actor.start_external_switch("claude-sonnet-4-6", true);
        actor.close_stream();
        let after = actor.drain_for(Duration::from_secs(2)).await;
        assert_eq!(
            updates_of(&after, "config_option_update").len(),
            1,
            "iter {i}: {after:?}"
        );
    }
}

/// T5FA4 review R3: the multi-thread runtime variant of R2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t5fa5_eof_before_the_switch_task_runs_mt() {
    for i in 0..20 {
        let mut actor = pin_clamp_actor();
        actor.start_external_switch("claude-sonnet-4-6", true);
        actor.close_stream();
        let after = actor.drain_for(Duration::from_secs(2)).await;
        assert_eq!(
            updates_of(&after, "config_option_update").len(),
            1,
            "iter {i}: {after:?}"
        );
    }
}

/// T5FA4 review R1: which requests does the downgrade switch send? It must be
/// one `set_permission_mode` first, and its update must still be delivered
/// after the EOF.
#[tokio::test]
async fn t5fa5_eof_after_the_first_request_still_delivers_the_update() {
    let mut actor = pin_clamp_actor();
    actor.start_external_switch("claude-sonnet-4-6", true);
    let request = actor.next_request().await;
    assert_eq!(request["request"]["subtype"], "set_permission_mode");
    actor.close_stream();
    let after = actor.drain_for(Duration::from_secs(2)).await;
    assert_eq!(
        updates_of(&after, "config_option_update").len(),
        1,
        "the switch update is delivered after EOF: {after:?}"
    );
}
