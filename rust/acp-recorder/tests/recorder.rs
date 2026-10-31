//! Integration test: the recorder drives a trivial in-repo echo agent and
//! records the `initialize` request + response in order (1.T4 / INV-24).

use std::path::{Path, PathBuf};
use std::process::Command;

use acp_recorder::{Direction, Script};

fn workspace_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml")
}

/// Path to the compiled `echo_agent` example binary, building it on demand.
/// Build it before running this test (`cargo build --example echo_agent`)
/// WITHOUT `CARGO_TARGET_DIR`: invoking cargo from inside a running `cargo
/// test` would wait on the parent's target-dir lock.
fn echo_agent_binary() -> PathBuf {
    let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/examples/echo_agent");
    if !bin.exists() {
        let status = Command::new("cargo")
            .args(["build", "--example", "echo_agent", "--manifest-path"])
            .arg(workspace_manifest())
            .status()
            .expect("failed to spawn cargo build for echo_agent");
        assert!(status.success(), "cargo build --example echo_agent failed");
    }
    bin
}

/// 1.T4 — the recorder against the in-repo echo agent records the `initialize`
/// request (client -> agent) followed by its response (agent -> client), in
/// order.
#[tokio::test]
async fn inv_24_recorder_initialize_order() {
    let agent_cmd = echo_agent_binary().to_string_lossy().into_owned();
    let script = Script::parse(r#"{"steps":[{"call":"initialize"}]}"#).unwrap();

    let frames = acp_recorder::record(&agent_cmd, &script)
        .await
        .expect("record against echo agent should succeed");

    assert!(
        frames.len() >= 2,
        "expected at least initialize request + response, got {} frames",
        frames.len()
    );

    // First frame: client -> agent `initialize` request.
    assert_eq!(
        frames[0].direction,
        Direction::Send,
        "first frame is the request"
    );
    assert_eq!(frames[0].json["method"], "initialize");
    assert!(
        frames[0].json.get("id").is_some(),
        "request carries a JSON-RPC id"
    );

    // Second frame: agent -> client response to `initialize`.
    assert_eq!(
        frames[1].direction,
        Direction::Recv,
        "second frame is the response"
    );
    assert_eq!(frames[1].json.get("id"), frames[0].json.get("id"));
    assert!(
        frames[1].json.get("result").is_some(),
        "initialize response carries a result"
    );

    // Order is preserved exactly as exchanged.
    let methods: Vec<Option<&str>> = frames
        .iter()
        .map(|f| f.json.get("method").and_then(|m| m.as_str()))
        .collect();
    assert_eq!(methods[0], Some("initialize"), "request precedes response");
}

/// F1 — a script may carry the WHOLE `clientCapabilities` object and answer an
/// agent->client `elicitation/create` with an `elicit_reply` step. The echo
/// agent only elicits when it actually received `elicitation.form`, so this
/// proves the full object reached the agent and the reply round trip works.
#[tokio::test]
async fn inv_24_recorder_full_capabilities_and_elicit_reply() {
    let agent_cmd = echo_agent_binary().to_string_lossy().into_owned();
    let script = Script::parse(
        r#"{"steps": [
            {"call": "initialize", "client_capabilities": {
                "fs": {"readTextFile": true, "writeTextFile": true},
                "terminal": true,
                "elicitation": {"form": {}}
            }},
            {"call": "session/new"},
            {"call": "session/prompt", "text": "please elicit", "no_wait": true},
            {"call": "elicit_reply", "response": {"action": "accept", "content": {"answer": "yes"}}},
            {"call": "session/wait"}
        ]}"#,
    )
    .unwrap();

    let frames = acp_recorder::record(&agent_cmd, &script)
        .await
        .expect("record with full capabilities + elicit_reply should succeed");

    // The initialize request carried the whole object, not just `_meta`.
    let initialize = frames
        .iter()
        .find(|f| f.direction == Direction::Send && f.json["method"] == "initialize")
        .expect("initialize request frame");
    let caps = &initialize.json["params"]["clientCapabilities"];
    assert_eq!(caps["terminal"], serde_json::json!(true));
    assert_eq!(caps["fs"]["readTextFile"], serde_json::json!(true));
    assert_eq!(
        caps["elicitation"]["form"],
        serde_json::json!({}),
        "elicitation.form advertised as an object"
    );

    // The agent asked the client to elicit (only possible because it saw the
    // capability above).
    let elicitation = frames
        .iter()
        .find(|f| f.direction == Direction::Recv && f.json["method"] == "elicitation/create")
        .expect("agent->client elicitation/create request");
    assert_eq!(
        elicitation.json["params"]["message"],
        serde_json::json!("What should the echo say?")
    );
    assert_eq!(
        elicitation.json["params"]["mode"],
        serde_json::json!("form")
    );
    assert!(
        elicitation.json["params"]["sessionId"].is_string(),
        "elicitation is scoped to the session"
    );

    // The recorder answered it with the scripted `elicit_reply` response.
    let reply = frames
        .iter()
        .find(|f| {
            f.direction == Direction::Send
                && f.json.get("id") == elicitation.json.get("id")
                && f.json.get("result").is_some()
        })
        .expect("client->agent elicitation response");
    assert_eq!(reply.json["result"]["action"], serde_json::json!("accept"));
    assert_eq!(
        reply.json["result"]["content"]["answer"],
        serde_json::json!("yes")
    );

    // The held prompt then settled normally.
    let prompt_response = frames
        .iter()
        .find(|f| f.direction == Direction::Recv && f.json["result"]["stopReason"].is_string())
        .expect("prompt response frame");
    assert_eq!(
        prompt_response.json["result"]["stopReason"],
        serde_json::json!("end_turn")
    );
}
