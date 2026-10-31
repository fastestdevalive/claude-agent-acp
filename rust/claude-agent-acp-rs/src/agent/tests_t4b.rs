//! T4B (C-10, C-43): `sessionCapabilities.subagents` follows the AIR client only,
//! and `nativeSubagentSessions` stays advertised for AIR (`acp-agent.ts:2950-2960`).

use super::*;

fn init(capabilities: Value) -> Value {
    let air = crate::air::AirClient::from_client_capabilities(&capabilities);
    let caps = ClientCapabilities::from_value(Some(&capabilities));
    let env = crate::env_snapshot::EnvSnapshot::from_pairs(std::iter::empty::<(&str, &str)>());
    let argv = vec!["dist/index.js".to_string()];
    initialize_response(
        air.as_ref(),
        &caps,
        &env,
        "/usr/bin/node",
        &argv,
        &[],
        false,
    )
}

#[test]
fn t4b_subagents_session_capability_is_air_only() {
    // The RFD client declares `subagents` itself and gets no agent capability.
    let rfd = init(json!({"subagents": {}}));
    assert!(rfd["agentCapabilities"]["sessionCapabilities"]
        .get("subagents")
        .is_none());
    let bare = init(json!({}));
    assert!(bare["agentCapabilities"]["sessionCapabilities"]
        .get("subagents")
        .is_none());
    let air = init(json!({"_meta": {"jetbrains": {"air": {
        "version": 1, "capabilities": ["nativeSubagentSessions"]}}}}));
    assert_eq!(
        air["agentCapabilities"]["sessionCapabilities"]["subagents"],
        json!({})
    );
    let listed = air["_meta"]["jetbrains"]["air"]["capabilities"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        listed.iter().any(|c| c == "nativeSubagentSessions"),
        "{air}"
    );
}

/// T8B — the initialize response advertises the logout capability
/// (`auth.logout: {}`) for every client (`acp-agent.ts` initialize).
#[test]
fn t8b_initialize_advertises_logout() {
    for caps in [json!({}), json!({"subagents": {}})] {
        let response = init(caps);
        assert_eq!(
            response["agentCapabilities"]["auth"]["logout"],
            json!({}),
            "{response}"
        );
    }
}

/// `acp-scenarios.test.ts` > Zed > "keeps promptQueueing and steering": the
/// initialize response carries `agentCapabilities._meta.claudeCode.promptQueueing`
/// and the top-level `_meta.steering.supported` marker for every client.
#[test]
fn t9b_initialize_advertises_prompt_queueing_and_steering() {
    let response = init(json!({}));
    assert_eq!(
        response.pointer("/agentCapabilities/_meta/claudeCode"),
        Some(&json!({"promptQueueing": true}))
    );
    assert_eq!(
        response.pointer("/_meta/steering"),
        Some(&json!({"supported": true}))
    );
}
