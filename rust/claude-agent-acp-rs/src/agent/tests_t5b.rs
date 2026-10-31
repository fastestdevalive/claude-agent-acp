//! T5B lane B unit tests for the resume/load and fork surface.

use super::*;

/// O-160 — `_meta.claudeCode.options.sessionStoreFlush` is a session-defining
/// option (`OPTION_REBUILDS_SESSION.sessionStoreFlush: true`,
/// `acp-agent.ts:1636`): changing it on a live `session/load`/`session/resume`
/// must rebuild the CLI, while an unchanged value does not.
#[test]
fn o160_session_store_flush_rebuilds_the_session() {
    assert!(OPTION_REBUILDS_SESSION.contains(&"sessionStoreFlush"));
    let fp = |value: Value| {
        compute_session_fingerprint(&json!({
            "cwd": "/project",
            "mcpServers": [],
            "_meta": {"claudeCode": {"options": {"sessionStoreFlush": value}}},
        }))
    };
    assert_eq!(fp(json!(true)), fp(json!(true)));
    assert_ne!(fp(json!(true)), fp(json!(false)));
    // Present vs absent is a change too (`undefined` is omitted by JSON).
    let absent = compute_session_fingerprint(&json!({"cwd": "/project", "mcpServers": []}));
    assert_ne!(fp(json!(true)), absent);
}
