//! Phase-4 control-channel integration test (4.T6, INV-12) against `fake-claude`.
//!
//! Verifies the control channel's single writer (Decision D6) drives a real
//! `claude`-shaped child end-to-end and, crucially, that stdin stays open after
//! a turn settles (Decision D7): a second user frame written to the *same*
//! stdin is answered by the fake.

mod common;

use std::time::Duration;

use claude_agent_acp_rs::control::{Control, ControlOptions, InitializeOptions};
use claude_agent_acp_rs::process::{spawn, SpawnOptions, Timings};
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SESSION_UUID: &str = "11111111-2222-3333-4444-555566667777";

/// 4.T6 (INV-12) — after a `result` frame, a second `user` frame is written to
/// the same stdin and the fake answers it (stdin stays open, D7).
#[tokio::test]
async fn inv_12_stdin_stays_open() {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("acp-control-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let transcript_path = tmp.join("two-prompts.transcript.jsonl");

    // A transcript with an initialize step and TWO user prompts, so the fake
    // proves it is still reading stdin after the first turn settles.
    let transcript = "\
{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[],\"commands\":[],\"account\":{}}}}]}
{\"expect\":{\"type\":\"user\"},\"emit\":[{\"type\":\"user\",\"uuid\":\"$MATCH.uuid\",\"isReplay\":true,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}},{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\",\"is_error\":false,\"result\":\"first reply\"}]}
{\"expect\":{\"type\":\"user\"},\"emit\":[{\"type\":\"user\",\"uuid\":\"$MATCH.uuid\",\"isReplay\":true,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}},{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\",\"is_error\":false,\"result\":\"second reply\"}]}
";
    std::fs::write(&transcript_path, transcript).expect("write transcript");

    let opts = SpawnOptions {
        claude_path: Some(fake_claude_binary()),
        session_id: Some(SESSION_UUID.to_string()),
        replay_user_messages: true,
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
        )],
        ..Default::default()
    };
    let mut process = spawn(&opts).await.expect("spawn fake-claude");

    let stdin = process.take_stdin().expect("take child stdin");
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<Value>();
    let control = Control::spawn(stdin, inbound_rx, ControlOptions::default());

    // Route the child's stdout: forward control-traffic frames to the control
    // task; session frames (user echoes / result) to `results`.
    let (route_tx, mut results) = mpsc::unbounded_channel::<Value>();
    let mut lines = std::mem::replace(&mut process.lines, mpsc::unbounded_channel().1);
    tokio::spawn(async move {
        while let Some(frame) = lines.recv().await {
            match frame.get("type").and_then(Value::as_str) {
                Some(
                    "keep_alive"
                    | "control_response"
                    | "control_request"
                    | "control_cancel_request",
                ) => {
                    let _ = inbound_tx.send(frame);
                }
                _ => {
                    let _ = route_tx.send(frame);
                }
            }
        }
    });

    // Handshake: initialize -> parse the response.
    let init = InitializeOptions::default();
    let _info = timeout(Duration::from_secs(10), control.initialize(&init))
        .await
        .expect("initialize handshake settles")
        .expect("initialize ok");

    // First prompt: user frame -> result.
    let first = user_frame("first", "msg-1");
    control.send_user(first).await.expect("send first user");
    let r1 = timeout(Duration::from_secs(10), next_result(&mut results))
        .await
        .expect("first result within timeout");
    assert_eq!(r1["result"], "first reply");

    // Second prompt on the SAME stdin: still answered -> stdin stayed open (D7).
    let second = user_frame("second", "msg-2");
    control.send_user(second).await.expect("send second user");
    let r2 = timeout(Duration::from_secs(10), next_result(&mut results))
        .await
        .expect("second result within timeout");
    assert_eq!(r2["result"], "second reply");

    // Teardown (the fake exits on stdin EOF; the ladder is a safety net).
    process
        .shutdown(&Timings::default())
        .await
        .expect("shutdown");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = root;
}

/// A minimal `{type:"user", ...}` frame with a uuid the fake echoes back.
fn user_frame(text: &str, uuid: &str) -> Value {
    json!({
        "type": "user",
        "session_id": "",
        "uuid": uuid,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        }
    })
}

/// Consume the next `result` frame from the routed stream.
async fn next_result(results: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    loop {
        let frame = results.recv().await.expect("stream open");
        if frame.get("type").and_then(Value::as_str) == Some("result") {
            return frame;
        }
    }
}
