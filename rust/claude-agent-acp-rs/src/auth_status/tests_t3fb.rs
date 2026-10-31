//! Turn 3 fix round, lane B: the per-session respawn flag lives as long as the
//! session does (upstream keeps it on the `Session` object), so a late mark must
//! not bring a forgotten entry back.

use super::*;
use std::path::PathBuf;
use tokio::sync::mpsc;

fn actor() -> AuthHandle {
    let (push_tx, mut push_rx) = mpsc::unbounded_channel::<AuthPush>();
    tokio::spawn(async move { while push_rx.recv().await.is_some() {} });
    let probe: ProbeFn = Box::new(|_| Box::pin(async { ProbeOutcome::LoggedOut }));
    spawn_auth_actor_with(
        push_tx,
        probe,
        PathBuf::from("/a"),
        crate::env_snapshot::EnvSnapshot::empty(),
        AuthActorConfig {
            hide_claude_auth: true,
            logger: None,
        },
    )
}

fn create(handle: &AuthHandle, session_id: &str) {
    handle.send(AuthCmd::SessionCreated {
        session_id: Some(session_id.into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
}

/// A mark that lands after the session was forgotten does not recreate the entry.
#[tokio::test]
async fn t3fb_a_late_mark_does_not_resurrect_a_forgotten_session() {
    let handle = actor();
    create(&handle, "s1");
    handle.send(AuthCmd::ForgetSession {
        session_id: "s1".into(),
    });
    handle.send(AuthCmd::MarkRespawn {
        session_id: "s1".into(),
    });
    assert!(
        !handle.needs_respawn("s1").await,
        "a forgotten session is not flagged again"
    );
    // The same mark on a live session still flags it.
    create(&handle, "s2");
    handle.send(AuthCmd::MarkRespawn {
        session_id: "s2".into(),
    });
    assert!(handle.needs_respawn("s2").await);
}

/// A mark for a session this connection never registered (a stale task) is
/// ignored as well.
#[tokio::test]
async fn t3fb_a_mark_for_an_unknown_session_is_ignored() {
    let handle = actor();
    handle.send(AuthCmd::MarkRespawn {
        session_id: "ghost".into(),
    });
    assert!(!handle.needs_respawn("ghost").await);
    // A later registration starts clean.
    create(&handle, "ghost");
    assert!(!handle.needs_respawn("ghost").await);
}
