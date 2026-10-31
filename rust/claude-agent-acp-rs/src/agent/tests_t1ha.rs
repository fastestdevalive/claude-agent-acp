//! Registry tests added by the Turn 1 fix round 3 (lane A, T1HA).

use super::*;
use crate::session::Command;
use std::time::Duration;

fn registry() -> Registry {
    let (recreate_tx, _recreate_rx) = mpsc::unbounded_channel();
    let (session_tx, _session_rx) = mpsc::unbounded_channel();
    let (dispose_tx, _dispose_rx) = mpsc::unbounded_channel();
    let (deferred_tx, deferred_rx) = mpsc::unbounded_channel();
    Registry {
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
        redeliver_tx: mpsc::unbounded_channel().0,
        index: session_index_connection(&ServeOptions::default()),
        provider: None,
        gateway_auth: None,
        creation_params: std::collections::HashMap::new(),
        auth_contexts: std::collections::HashMap::new(),
        guard_states: std::collections::HashMap::new(),
        hide_claude_auth: false,
        protocol: crate::v2::ProtocolMode::V1,
        suppress_commands_update: false,
        v2_outbound: None,
        logger: None,
        provider_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        provider_updates: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        sessions_view: Arc::new(watch::channel(std::collections::HashMap::new()).0),
    }
}

fn update(session: Session, cancel: Option<Arc<std::sync::atomic::AtomicBool>>) -> RegistryMsg {
    let manager = crate::settings_manager::SettingsManager::new(
        std::env::temp_dir(),
        Some(crate::settings_manager::SettingsManagerOptions {
            env: Some(crate::env_snapshot::EnvSnapshot::empty()),
            ..Default::default()
        }),
    );
    RegistryMsg::Register(Box::new(RegistryUpdate {
        session_id: "s1".to_string(),
        session,
        settings_manager: Arc::new(manager),
        fingerprint: "fp".to_string(),
        cancel,
        creation_params: Value::Null,
        auth_context: SessionAuthContext::default(),
        guard: crate::hide_claude_auth::GuardState::default(),
    }))
}

/// Finding 2: a `session/close` that cancelled the recreate AFTER the start's
/// last cancel check leaves the flag set when the update reaches the handler: the
/// session is closed, not registered under the id the client closed.
#[tokio::test]
async fn t1ha_a_cancelled_recreates_late_register_closes_the_session() {
    let mut registry = registry();
    let (session, mut commands, _writer) = Session::detached_for_test("s1");
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
    registry
        .deferred_tx
        .send(update(session, Some(cancel)))
        .unwrap();
    registry.apply_deferred();
    assert!(!registry.sessions.contains_key("s1"), "not registered");
    assert!(!registry.fingerprints.contains_key("s1"));
    let command = tokio::time::timeout(Duration::from_secs(10), commands.recv())
        .await
        .expect("the session is told to close");
    assert!(matches!(command, Some(Command::Close { .. })));
}

/// The ordinary case is unchanged: an update whose recreate was not cancelled
/// (or that belongs to no recreate) registers.
#[tokio::test]
async fn t1ha_an_uncancelled_register_still_registers() {
    for cancel in [
        None,
        Some(Arc::new(std::sync::atomic::AtomicBool::new(false))),
    ] {
        let mut registry = registry();
        let (session, mut commands, _writer) = Session::detached_for_test("s1");
        registry.deferred_tx.send(update(session, cancel)).unwrap();
        registry.apply_deferred();
        assert!(registry.sessions.contains_key("s1"));
        assert!(commands.try_recv().is_err(), "nothing told it to close");
    }
}

/// A recreate task that panics still clears its "recreating" marker and wakes a
/// waiting `session/close`: the id is not refused for the rest of the connection.
#[tokio::test]
async fn t1ha_a_panicking_recreate_clears_its_marker() {
    let mut registry = registry();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (ended_tx, ended) = watch::channel(false);
    registry.recreating.insert(
        "s1".to_string(),
        Recreating {
            cancel: cancel.clone(),
            ended: ended.clone(),
        },
    );
    let ending = RecreateEnding {
        deferred_tx: registry.deferred_tx.clone(),
        session_id: "s1".to_string(),
        cancel,
        ended_tx,
    };
    let task = tokio::spawn(async move {
        let _ending = ending;
        panic!("the recreate panics");
    });
    assert!(task.await.is_err(), "the task panicked");
    let mut ended = ended;
    tokio::time::timeout(Duration::from_secs(10), ended.wait_for(|ended| *ended))
        .await
        .expect("a waiting close is woken")
        .expect("the signal is open");
    registry.apply_deferred();
    assert!(
        !registry.recreating.contains_key("s1"),
        "the marker is cleared"
    );
}
