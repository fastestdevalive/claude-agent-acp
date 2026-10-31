//! Integration test for D-03 v2 scenario harness.
//!
//! Replays the captured v2 `initialize` golden through the crate's REAL v1
//! `initialize_response` (agent.rs) and the v2 mapping, then compares the
//! serialized result with the golden frame. Nothing about the agent's answer is
//! copied from the golden: only the client's request params are replayed.

use serde_json::Value;

use claude_agent_acp_rs::agent::initialize_response;
use claude_agent_acp_rs::client_caps::ClientCapabilities;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::v2::{v1_initialize_request, v2_initialize_response};

fn golden_frames() -> (Value, Value) {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = manifest_dir.join("../../porting/fixtures-v2/initialize.frames.jsonl");

    let content = std::fs::read_to_string(&fixture_path)
        .expect("porting/fixtures-v2/initialize.frames.jsonl must exist from capture");

    let mut send_frame: Option<Value> = None;
    let mut recv_frame: Option<Value> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(line).expect("valid frame json line");
        match parsed.get("direction").and_then(Value::as_str) {
            Some("send") => send_frame = Some(parsed["frame"].clone()),
            Some("recv") => recv_frame = Some(parsed["frame"].clone()),
            _ => {}
        }
    }
    (
        send_frame.expect("golden send frame must be present"),
        recv_frame.expect("golden recv frame must be present"),
    )
}

#[test]
fn test_d03_replay_captured_v2_initialize_golden() {
    let (send, recv) = golden_frames();
    let v2_req_params = &send["params"];
    let golden_v2_result = &recv["result"];

    // The request really is v2-shaped: no `clientCapabilities`, only
    // `capabilities`, so the old hook's parse would see nothing.
    assert!(
        v2_req_params.get("clientCapabilities").is_none(),
        "a v2 client sends `capabilities`, not `clientCapabilities`"
    );
    assert_eq!(v2_req_params["protocolVersion"], 2);

    // 1. Map the v2 request to the v1 initialize request (initialize.ts:20-55).
    let v1_req = v1_initialize_request(v2_req_params);
    assert_eq!(v1_req["protocolVersion"], 1);
    assert_eq!(v1_req["clientInfo"]["name"], "v2-client");
    assert_eq!(v1_req["clientCapabilities"]["fs"]["readTextFile"], false);
    assert_eq!(v1_req["clientCapabilities"]["terminal"], false);
    assert_eq!(v1_req["clientCapabilities"]["auth"]["terminal"], true);

    // 2. Build the agent's v1 response through the REAL function, with the
    //    capabilities parsed from the translated request.
    let caps = ClientCapabilities::from_value(v1_req.get("clientCapabilities"));
    let v1_agent_resp = initialize_response(
        None,
        &caps,
        &EnvSnapshot::empty(),
        "/fake/entrypoint",
        &[],
        &[],
        false,
    );

    // 3. Translate the v1 response back to a v2 initialize response.
    let generated_v2_resp = v2_initialize_response(&v1_agent_resp)
        .expect("v2 initialize response mapping must succeed");

    // The agent info is the crate's own (the Rust version is composed), not a
    // value copied from the golden; only that one field is normalised for the
    // comparison, exactly as the differential harness does.
    assert_eq!(
        generated_v2_resp["info"]["name"],
        golden_v2_result["info"]["name"]
    );
    assert_eq!(
        generated_v2_resp["info"]["title"],
        golden_v2_result["info"]["title"]
    );
    assert_eq!(
        generated_v2_resp["info"]["version"],
        claude_agent_acp_rs::VERSION
    );

    let mut expected = golden_v2_result.clone();
    expected["info"]["version"] = Value::String(claude_agent_acp_rs::VERSION.to_string());

    // 4. Byte comparison (key order included) against the Node golden.
    assert_eq!(
        serde_json::to_string(&generated_v2_resp).expect("serialize"),
        serde_json::to_string(&expected).expect("serialize"),
        "the live v2 initialize result must equal the Node golden"
    );

    // The two terminal auth methods really are there (B1's failing assert).
    let methods = generated_v2_resp["authMethods"]
        .as_array()
        .expect("array of auth methods");
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0]["methodId"], "claude-ai-login");
    assert_eq!(methods[0]["type"], "terminal");
    assert_eq!(methods[1]["methodId"], "console-login");
    assert_eq!(methods[1]["type"], "terminal");

    // Baseline v2 capabilities.
    assert_eq!(
        generated_v2_resp["capabilities"]["providers"],
        Value::Object(Default::default())
    );
    assert_eq!(
        generated_v2_resp["capabilities"]["_meta"]["claudeCode"]["promptQueueing"],
        true
    );
    // `steering` is stripped: it is a v1-only extension.
    assert!(generated_v2_resp.get("_meta").is_none());
}
