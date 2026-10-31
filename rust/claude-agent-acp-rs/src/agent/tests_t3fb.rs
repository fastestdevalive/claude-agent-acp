//! Turn 3 fix round, lane B: registry-level tests of the deferred provider
//! routing and of forgetting a session that a cancelled start left behind.

use super::*;
use std::path::PathBuf;

pub(super) struct Rig {
    pub(super) registry: Registry,
    auth: AuthHandle,
    _sessions: mpsc::UnboundedReceiver<(String, Session)>,
    _dispose: mpsc::UnboundedReceiver<Session>,
    _recreates: mpsc::UnboundedReceiver<RecreateTask>,
}

pub(super) fn rig() -> Rig {
    let opts = ServeOptions {
        env: crate::env_snapshot::EnvSnapshot::empty(),
        ..ServeOptions::default()
    };
    let (session_tx, sessions) = mpsc::unbounded_channel();
    let (dispose_tx, dispose) = mpsc::unbounded_channel();
    let (deferred_tx, deferred_rx) = mpsc::unbounded_channel();
    let (recreate_tx, recreates) = mpsc::unbounded_channel();
    let (push_tx, _push_rx) = mpsc::unbounded_channel::<AuthPush>();
    let probe: crate::auth_status::ProbeFn =
        Box::new(|_| Box::pin(async { crate::auth_status::ProbeOutcome::LoggedOut }));
    let auth = spawn_auth_actor_with(
        push_tx,
        probe,
        PathBuf::from("/a"),
        opts.env.clone(),
        AuthActorConfig {
            hide_claude_auth: true,
            logger: None,
        },
    );
    let registry = Registry {
        sessions: std::collections::HashMap::new(),
        session_tx,
        dispose_tx,
        air: None,
        caps: ClientCapabilities::default(),
        auth: Some(auth.clone()),
        settings_managers: std::collections::HashMap::new(),
        fingerprints: std::collections::HashMap::new(),
        deferred_tx,
        deferred_rx,
        recreating: std::collections::HashMap::new(),
        recreate_tx,
        index: session_index_connection(&opts),
        provider: None,
        gateway_auth: None,
        creation_params: std::collections::HashMap::new(),
        auth_contexts: std::collections::HashMap::new(),
        guard_states: std::collections::HashMap::new(),
        hide_claude_auth: true,
        provider_gate: Arc::new(tokio::sync::Semaphore::new(GATE_PERMITS as usize)),
        provider_updates: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        sessions_view: Arc::new(watch::channel(std::collections::HashMap::new()).0),
    };
    Rig {
        registry,
        auth,
        _sessions: sessions,
        _dispose: dispose,
        _recreates: recreates,
    }
}

/// A start cancelled after it registered its session leaves no auth entry behind
/// (the `Register` a cancelled recreate left is dropped with a `ForgetSession`).
#[tokio::test]
async fn t3fb_a_cancelled_register_forgets_the_session() {
    let mut rig = rig();
    rig.auth.send(AuthCmd::SessionCreated {
        session_id: Some("s".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    rig.auth.send(AuthCmd::MarkRespawn {
        session_id: "s".into(),
    });
    assert!(rig.auth.needs_respawn("s").await, "the entry exists");
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (session, _commands, _pipe) = Session::detached_for_test("s");
    let _ = rig
        .registry
        .deferred_tx
        .send(RegistryMsg::Register(Box::new(RegistryUpdate {
            session_id: "s".into(),
            session,
            settings_manager: Arc::new(crate::settings_manager::SettingsManager::new(
                PathBuf::from("/a"),
                None,
            )),
            fingerprint: String::new(),
            cancel: Some(cancel),
            creation_params: json!({"cwd": "/a", "mcpServers": []}),
            auth_context: SessionAuthContext::default(),
            guard: crate::hide_claude_auth::GuardState::default(),
        })));
    rig.registry.apply_deferred();
    assert!(
        !rig.auth.needs_respawn("s").await,
        "the cancelled session's auth entry is forgotten"
    );
}

/// The provider routing a spawned update leaves is adopted by the handler before
/// the next request, and the auth actor learns it too.
#[tokio::test]
async fn t3fb_a_deferred_provider_update_is_applied() {
    let mut rig = rig();
    let config = providers::ProviderConfig::new("anthropic", "https://gateway.example", Vec::new());
    let _ = rig.registry.deferred_tx.send(RegistryMsg::SetProvider {
        config: Some(config.clone()),
    });
    rig.registry.apply_deferred();
    assert_eq!(
        rig.registry.provider.as_ref().map(|c| c.base_url.clone()),
        Some("https://gateway.example".to_string())
    );
    // Clearing it goes back to native routing.
    let _ = rig
        .registry
        .deferred_tx
        .send(RegistryMsg::SetProvider { config: None });
    rig.registry.apply_deferred();
    assert!(rig.registry.provider.is_none());
}
