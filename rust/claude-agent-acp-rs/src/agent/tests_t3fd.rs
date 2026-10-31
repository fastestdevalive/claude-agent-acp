//! Turn 3 fix round 2, lane D: registry-level tests of the provider gate and of the
//! session view a provider update reads.

use super::tests_t3fb::rig;
use super::*;

/// Only the requests that start a turn or a session wait for a provider update:
/// a steer and a goal never do (upstream's `steer()` never awaits `providerUpdate`).
#[test]
fn t3fd_only_session_starting_requests_are_gated() {
    for method in [
        "session/new",
        "session/load",
        "session/resume",
        "session/fork",
        "session/prompt",
    ] {
        assert!(is_gated(method), "{method}");
    }
    for method in [
        "_session/steering",
        "_session/goal",
        "session/close",
        "providers/set",
        "initialize",
    ] {
        assert!(!is_gated(method), "{method}");
    }
}

/// A provider update inserts the session it started straight into the view; the
/// registry still holds the closed session it replaced until the `Register` is
/// applied, and publishing must not put the closed one back.
#[tokio::test]
async fn t3fd_publishing_the_registry_keeps_a_session_started_meanwhile() {
    let rig = rig();
    let mut registry = rig.registry;
    let params = json!({"cwd": "/a", "mcpServers": []});
    let (old, old_commands, _old_pipe) = Session::detached_for_test("s");
    drop(old_commands);
    assert!(old.command_sender().is_closed());
    registry.sessions.insert("s".into(), old);
    registry.creation_params.insert("s".into(), params.clone());
    let (fresh, _fresh_commands, _fresh_pipe) = Session::detached_for_test("s");
    registry.sessions_view.send_modify(|view| {
        view.insert("s".into(), (fresh.clone(), params.clone()));
    });
    registry.publish_sessions();
    let view = registry.sessions_view.subscribe();
    let held = view.borrow().get("s").map(|(session, _)| session.clone());
    let held = held.expect("the session is still in the view");
    assert!(
        !held.command_sender().is_closed(),
        "the view holds the session the update started"
    );
}

/// A live registry entry still replaces the view's (the ordinary publish).
#[tokio::test]
async fn t3fd_publishing_the_registry_replaces_an_ordinary_entry() {
    let rig = rig();
    let mut registry = rig.registry;
    let params = json!({"cwd": "/a", "mcpServers": []});
    let (live, _commands, _pipe) = Session::detached_for_test("s");
    registry.sessions.insert("s".into(), live.clone());
    registry.creation_params.insert("s".into(), params);
    registry.publish_sessions();
    let view = registry.sessions_view.subscribe();
    let held = view.borrow().get("s").map(|(session, _)| session.clone());
    let held = held.expect("published");
    assert!(held.command_sender().same_channel(live.command_sender()));
}

fn failure_id(update: &Value) -> String {
    update
        .pointer("/update/_meta/jetbrains/air/sessionFailure/id")
        .and_then(Value::as_str)
        .expect("a failure id")
        .to_string()
}

/// M-129: the lost-session report is published on the session's own failure book,
/// so its id keeps the session's epoch and continues its incident counter.
#[test]
fn t3fd_the_lost_session_report_continues_the_session_failure_book() {
    let mut book = crate::session::failures::SessionFailures::default();
    let earlier = book.publish(
        "s",
        crate::session_failure::ClaudeFailureKind::InternalError,
        &crate::session::failures::FailureOptions {
            session_scoped: true,
            details: Some("earlier".into()),
            ..Default::default()
        },
    );
    let error = Error::internal_error().data(json!({"details": "gone"}));
    let lost = recreate::session_lost_update(Some(book), "s", &error);
    let earlier_id = failure_id(&earlier);
    let lost_id = failure_id(&lost);
    let (earlier_prefix, earlier_incident) = earlier_id.rsplit_once(':').expect("incident");
    let (lost_prefix, lost_incident) = lost_id.rsplit_once(':').expect("incident");
    assert_eq!(earlier_prefix, lost_prefix, "same session epoch");
    assert_eq!(earlier_incident, "1");
    assert_eq!(lost_incident, "2", "the incident counter continues");
    // Without a book of the session the report still goes out, on a fresh one.
    let fresh = recreate::session_lost_update(None, "s", &error);
    assert!(failure_id(&fresh).ends_with(":1"));
}
