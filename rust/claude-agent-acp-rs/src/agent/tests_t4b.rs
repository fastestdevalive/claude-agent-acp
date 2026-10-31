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
