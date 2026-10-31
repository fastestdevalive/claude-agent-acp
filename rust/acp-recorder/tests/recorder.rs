//! Integration test: the recorder drives a trivial in-repo echo agent and
//! records the `initialize` request + response in order (1.T4 / INV-24).

use std::path::{Path, PathBuf};
use std::process::Command;

use acp_recorder::{Direction, Script};

fn workspace_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml")
}

/// Path to the compiled `echo_agent` example binary, building it on demand.
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
