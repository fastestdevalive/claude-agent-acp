//! G1: the replay publish-failure rollback (U-49).

use super::*;
use crate::session::failures::SessionFailures;
use crate::session_failure::ClaudeFailureKind;

/// T8A gap (141): upstream "does not record a restored failure when publishing
/// it fails" — the book keeps no active record and no revision, and the miss
/// is logged.
#[test]
fn a_failed_replay_publish_drops_the_restored_failure() {
    let path = std::env::temp_dir().join(format!(
        "g1-u49-log-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let logger = crate::logger::Logger::to_file(&path);
    let mut book = SessionFailures::default();
    let value = book.restore(
        "s1",
        "s1:history-error:usage-limit-message",
        ClaudeFailureKind::QuotaExhausted,
        "You've hit your individual spend limit",
        true,
    );
    assert_eq!(book.active().len(), 1);
    assert_eq!(
        book.revision_of("s1:history-error:usage-limit-message"),
        Some(1)
    );

    let error = agent_client_protocol::Error::new(-32603, "client disconnected");
    assert!(rollback_unpublished_failure(
        &value,
        &error,
        &mut book,
        "s1",
        Some(&logger),
    ));
    assert!(book.active().is_empty(), "no active record");
    assert_eq!(
        book.revision_of("s1:history-error:usage-limit-message"),
        None,
        "no revision record"
    );
    let log = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    assert!(
        log.contains("failed to publish AIR session failure"),
        "{log}"
    );

    // A notification that carries no failure rolls back nothing.
    let plain = json!({"sessionId":"s1","update":{"sessionUpdate":"plan","entries":[]}});
    assert!(!rollback_unpublished_failure(
        &plain, &error, &mut book, "s1", None,
    ));
}
