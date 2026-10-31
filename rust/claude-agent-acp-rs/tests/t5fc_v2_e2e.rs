//! T5FC STEP 3 (B1/B2/B3) + T6B: the v2 initialize path end to end over the
//! real in-process channel, and the post-initialize v2 surface. T6B replaced
//! the T5 guard (every later request -32601) with the real v2 router, so
//! `session/new` is served now.
//!
//! The client replays the captured golden request; the agent runs its real
//! `serve`/`handle_request` path with the opt-in injected through the env
//! snapshot (no `std::env::set_var`). The response is compared with the Node
//! golden, and a later `session/new` must be answered with the upstream-shaped
//! `-32601`, never a v1 session.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::timeout;

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
        send_frame.expect("golden send frame"),
        recv_frame.expect("golden recv frame"),
    )
}

fn v2_options() -> ServeOptions {
    // No real `claude`: the explicit path does not exist, so the background
    // auth probe fails fast and spawns nothing. The opt-in comes from the
    // startup env snapshot, exactly like the binary's `EnvSnapshot::from_process`.
    ServeOptions {
        claude_path: Some(PathBuf::from("/nonexistent/t5fc-fake-claude")),
        env: EnvSnapshot::from_pairs([(EXPERIMENTAL_V2_ENV.to_string(), "1".to_string())]),
        ..Default::default()
    }
}

#[tokio::test]
async fn t5fc_v2_initialize_matches_golden_then_later_requests_are_method_not_found() {
    let (send, recv) = golden_frames();
    let golden_params = send["params"].clone();
    let golden_result = recv["result"].clone();

    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, v2_options()).await });

    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<String>();

    let result = timeout(
        Duration::from_secs(30),
        Client
            .builder()
            .on_receive_notification(
                async move |notification: UntypedMessage, _cx| {
                    let _ = notify_tx.send(notification.method().to_string());
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<Agent>| async move {
                    // 1. The golden's own v2 initialize request.
                    let init = connection
                        .send_request(UntypedMessage::new("initialize", golden_params)?)
                        .block_task()
                        .await?;

                    // Byte comparison with the golden, normalising only the
                    // composed Rust agentInfo.version (the differential does the
                    // same; every other field is compared exactly, order too).
                    let mut expected = golden_result.clone();
                    expected["info"]["version"] =
                        Value::String(claude_agent_acp_rs::VERSION.to_string());
                    assert_eq!(
                        serde_json::to_string(&init).expect("serialize"),
                        serde_json::to_string(&expected).expect("serialize"),
                        "the live v2 initialize result must equal the Node golden"
                    );
                    assert_eq!(init["protocolVersion"], 2);
                    assert_eq!(
                        init["authMethods"].as_array().map(Vec::len),
                        Some(2),
                        "the two terminal auth methods are advertised: {init}"
                    );

                    // 2. After negotiating v2, `session/new` is served by the
                    //    v2 router (T6B), not the T5 method-not-found guard.
                    //    The configured CLI path does not exist, so the spawn
                    //    fails with an internal error instead.
                    let err = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            serde_json::json!({ "cwd": "/tmp", "mcpServers": [] }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("session/new is served on v2");
                    assert_ne!(i32::from(err.code), -32601, "{err:?}");
                    Ok::<(), agent_client_protocol::Error>(())
                },
            ),
    )
    .await;

    result
        .expect("the v2 exchange finished in time")
        .expect("the v2 exchange succeeded");
    let _ = timeout(Duration::from_secs(10), serve_task).await;

    // B2: no v1-shaped session notification was sent on this connection.
    let mut notifications = Vec::new();
    while let Ok(method) = notify_rx.try_recv() {
        notifications.push(method);
    }
    assert!(
        !notifications.iter().any(|m| m == "session/update"),
        "a v2 connection must not receive v1 session updates: {notifications:?}"
    );
}

/// G4 / B-113 — `turn-events.test.ts` > "rejects a prompt that it refuses before
/// starting a turn, and reports nothing": a v2 `session/prompt` for a session
/// the registry does not hold is the internal error "Session not found", and no
/// turn event is reported.
#[tokio::test]
async fn g4_b113_v2_prompt_rejects_an_unknown_session() {
    let (send, _recv) = golden_frames();
    let golden_params = send["params"].clone();
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, v2_options()).await });

    let result = timeout(
        Duration::from_secs(30),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(UntypedMessage::new("initialize", golden_params)?)
                    .block_task()
                    .await?;
                let err = connection
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        serde_json::json!({
                            "sessionId": "no-such-session",
                            "prompt": [{ "type": "text", "text": "hello" }],
                        }),
                    )?)
                    .block_task()
                    .await
                    .expect_err("an unknown session is refused before a turn starts");
                assert_eq!(i32::from(err.code), -32603, "{err:?}");
                assert!(
                    serde_json::to_string(&err)
                        .unwrap()
                        .contains("Session not found"),
                    "{err:?}"
                );
                Ok::<(), agent_client_protocol::Error>(())
            },
        ),
    )
    .await;
    result
        .expect("the v2 prompt drive finished in time")
        .expect("the v2 prompt drive succeeded");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
}
