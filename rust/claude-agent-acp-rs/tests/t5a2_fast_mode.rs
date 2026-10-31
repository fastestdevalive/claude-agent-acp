//! T5A2 end-to-end (fake harness): the Fast mode option seeded at session
//! creation, a toggle through `session/set_config_option`, and the husk
//! rejection once the query stream has closed.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::fake_claude_binary;
use serde_json::json;
use tokio::time::timeout;

fn fake_opts(root: &Path, transcript: &Path) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(root.to_path_buf()),
        ..Default::default()
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t5a2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("cwd")).expect("create temp dir");
    std::fs::create_dir_all(tmp.join("config")).expect("create config dir");
    tmp
}

fn init_line(fast_state: &str) -> String {
    format!(
        "{{\"expect\":{{\"type\":\"control_request\",\"request\":{{\"subtype\":\"initialize\"}}}},\"emit\":[{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{{\"models\":[{{\"value\":\"claude-sonnet-4-5\",\"displayName\":\"Claude Sonnet 4.5\",\"supportsFastMode\":true}}],\"commands\":[],\"account\":{{}},\"fast_mode_state\":\"{fast_state}\"}}}}}}]}}\n"
    )
}

/// The session/new background probe must ask for the summary detail: the fake
/// CLI only consumes this step when the request carries `detail:"summary"`
/// (a bare `{subtype}` request would be auto-acked, leaving the step pending and
/// failing the next expectation).
const CONTEXT_LINE: &str = "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"get_context_usage\",\"detail\":\"summary\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"rawMaxTokens\":200000}}}]}\n";

#[tokio::test]
async fn t5a2_fast_mode_is_seeded_and_toggled_end_to_end() {
    let tmp = temp_dir("fast");
    let cwd = tmp.join("cwd");
    let config_dir = tmp.join("config");
    let transcript = tmp.join("fast.transcript.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}{}{}",
            init_line("on"),
            CONTEXT_LINE,
            "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"apply_flag_settings\",\"settings\":{\"fastMode\":false}}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{}}}]}\n"
        ),
    )
    .expect("write transcript");
    let opts = fake_opts(&cwd, &transcript).with_env(
        claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_iter([(
            "CLAUDE_CONFIG_DIR",
            config_dir.to_string_lossy().to_string(),
        )]),
    );
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_for_client = cwd.clone();
    let result = timeout(
        Duration::from_secs(20),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd_for_client = cwd_for_client.clone();
                async move {
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({
                                "protocolVersion": 1,
                                "clientCapabilities": {
                                    "session": {"configOptions": {"boolean": {}}}
                                }
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({ "cwd": cwd_for_client.to_string_lossy() }),
                        )?)
                        .block_task()
                        .await?;
                    let session_id = created["sessionId"].as_str().unwrap().to_string();
                    let fast = created["configOptions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|o| o["id"] == "fast")
                        .expect("the Fast mode option is seeded from initialize")
                        .clone();
                    assert_eq!(fast["type"], "boolean");
                    assert_eq!(fast["currentValue"], true);

                    let toggled = connection
                        .send_request(UntypedMessage::new(
                            "session/set_config_option",
                            json!({
                                "sessionId": session_id,
                                "configId": "fast",
                                "type": "boolean",
                                "value": false,
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let fast = toggled["configOptions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|o| o["id"] == "fast")
                        .expect("the Fast mode option")
                        .clone();
                    assert_eq!(fast["currentValue"], false);
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await
    .expect("the client run finishes");
    result.expect("no protocol error");
    serve_task.abort();
}

#[tokio::test]
async fn t5a2_air_creation_seeds_effort_with_ultracode() {
    let tmp = temp_dir("air-effort");
    let cwd = tmp.join("cwd");
    let config_dir = tmp.join("config");
    std::fs::write(config_dir.join("settings.json"), r#"{"ultracode":true}"#)
        .expect("write settings");
    let transcript = tmp.join("air.transcript.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}{}{}",
            init_line_effort(),
            "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"apply_flag_settings\",\"settings\":{\"effortLevel\":\"medium\",\"ultracode\":true}}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{}}}]}\n",
            CONTEXT_LINE
        ),
    )
    .expect("write transcript");
    let opts = fake_opts(&cwd, &transcript).with_env(
        claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_iter([(
            "CLAUDE_CONFIG_DIR",
            config_dir.to_string_lossy().to_string(),
        )]),
    );
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_for_client = cwd.clone();
    let result = timeout(
        Duration::from_secs(20),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd_for_client = cwd_for_client.clone();
                async move {
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({
                                "protocolVersion": 1,
                                "clientCapabilities": {
                                    "_meta": {"jetbrains": {"air": {
                                        "version": 1,
                                        "capabilities": ["recommendedValue"]
                                    }}}
                                }
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({ "cwd": cwd_for_client.to_string_lossy() }),
                        )?)
                        .block_task()
                        .await?;
                    let effort = created["configOptions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|o| o["id"] == "effort")
                        .expect("the AIR client gets an effort option")
                        .clone();
                    assert_eq!(effort["currentValue"], "medium");
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await
    .expect("the client run finishes");
    result.expect("no protocol error");
    serve_task.abort();
}

/// The initialize reply for an AIR effort scenario: the model supports effort
/// with the recommended `medium` level.
fn init_line_effort() -> String {
    "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[{\"value\":\"claude-sonnet-4-5\",\"displayName\":\"Claude Sonnet 4.5\",\"supportsEffort\":true,\"supportedEffortLevels\":[\"low\",\"medium\",\"high\"]}],\"commands\":[],\"account\":{}}}}]}\n".to_string()
}

#[tokio::test]
async fn t5a2_a_husk_session_rejects_config_and_mode_changes() {
    let tmp = temp_dir("husk");
    let cwd = tmp.join("cwd");
    let config_dir = tmp.join("config");
    let transcript = tmp.join("husk.transcript.jsonl");
    // An error result followed by the child's exit keeps the husk (no
    // ProcessDied): a later config/mode request must answer the session-ended
    // error without touching the closed query.
    std::fs::write(
        &transcript,
        format!(
            "{}{}{}",
            init_line("on"),
            CONTEXT_LINE,
            "{\"expect\":{\"type\":\"user\"},\"emit\":[{\"type\":\"user\",\"uuid\":\"$MATCH.uuid\",\"isReplay\":true,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"go\"}]}},{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}},{\"type\":\"result\",\"subtype\":\"error\",\"is_error\":true,\"stop_reason\":\"error\",\"result\":\"boom\"}],\"exit\":true}\n"
        ),
    )
    .expect("write transcript");
    let opts = fake_opts(&cwd, &transcript).with_env(
        claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_iter([(
            "CLAUDE_CONFIG_DIR",
            config_dir.to_string_lossy().to_string(),
        )]),
    );
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_for_client = cwd.clone();
    let result = timeout(
        Duration::from_secs(20),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd_for_client = cwd_for_client.clone();
                async move {
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({
                                "protocolVersion": 1,
                                "clientCapabilities": {
                                    "session": {"configOptions": {"boolean": {}}}
                                }
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({ "cwd": cwd_for_client.to_string_lossy() }),
                        )?)
                        .block_task()
                        .await?;
                    let session_id = created["sessionId"].as_str().unwrap().to_string();
                    // An unknown session answers `Session not found`.
                    let missing = connection
                        .send_request(UntypedMessage::new(
                            "session/set_config_option",
                            json!({
                                "sessionId": "nope",
                                "configId": "mode",
                                "value": "plan",
                            }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("an unknown session must fail");
                    assert_eq!(i32::from(missing.code), -32603);
                    assert_eq!(
                        missing.data.as_ref().unwrap()["details"],
                        "Session not found"
                    );
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{"type": "text", "text": "go"}],
                            }),
                        )?)
                        .block_task()
                        .await;
                    // The child exited after its error result: the session
                    // lingers as a husk whose query stream is closed. Wait for the
                    // actor to observe EOF by polling a harmless, non-mutating
                    // probe (an unknown mode id answers "Invalid Mode" while the
                    // actor lives, SESSION_ENDED once it is gone) instead of a
                    // fixed sleep.
                    timeout(Duration::from_secs(10), async {
                        loop {
                            let outcome = connection
                                .send_request(UntypedMessage::new(
                                    "session/set_mode",
                                    json!({ "sessionId": session_id, "modeId": "not-a-mode" }),
                                )?)
                                .block_task()
                                .await;
                            if let Err(error) = outcome {
                                if error.message.contains("start a new session") {
                                    break;
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Ok::<(), agent_client_protocol::Error>(())
                    })
                    .await
                    .expect("the husk answers SESSION_ENDED")
                    .expect("no protocol error");
                    let config_err = connection
                        .send_request(UntypedMessage::new(
                            "session/set_config_option",
                            json!({
                                "sessionId": session_id,
                                "configId": "fast",
                                "type": "boolean",
                                "value": true,
                            }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("a husk rejects config changes");
                    assert_eq!(i32::from(config_err.code), -32603);
                    assert!(
                        config_err.message.contains("start a new session"),
                        "the SESSION_ENDED message: {}",
                        config_err.message
                    );
                    let mode_err = connection
                        .send_request(UntypedMessage::new(
                            "session/set_mode",
                            json!({ "sessionId": session_id, "modeId": "plan" }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("a husk rejects mode changes");
                    assert_eq!(i32::from(mode_err.code), -32603);
                    assert!(
                        mode_err.message.contains("start a new session"),
                        "the SESSION_ENDED message: {}",
                        mode_err.message
                    );
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await
    .expect("the client run finishes");
    result.expect("no protocol error");
    serve_task.abort();
}

#[tokio::test]
async fn t5a2_a_rejected_creation_effort_fails_session_new() {
    let tmp = temp_dir("air-effort-rejected");
    let cwd = tmp.join("cwd");
    let config_dir = tmp.join("config");
    let transcript = tmp.join("air-rejected.transcript.jsonl");
    // `await q.applyFlagSettings(...)` with no catch (`acp-agent.ts:11594-11609`):
    // a rejected seed fails `session/new` instead of leaving a value the CLI
    // never applied on screen.
    std::fs::write(
        &transcript,
        format!(
            "{}{}",
            init_line_effort(),
            "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"apply_flag_settings\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"error\",\"request_id\":\"$REQ\",\"error\":\"effort seed rejected\"}}]}\n"
        ),
    )
    .expect("write transcript");
    let opts = fake_opts(&cwd, &transcript).with_env(
        claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_iter([(
            "CLAUDE_CONFIG_DIR",
            config_dir.to_string_lossy().to_string(),
        )]),
    );
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_for_client = cwd.clone();
    let result = timeout(
        Duration::from_secs(20),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd_for_client = cwd_for_client.clone();
                async move {
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({
                                "protocolVersion": 1,
                                "clientCapabilities": {
                                    "_meta": {"jetbrains": {"air": {
                                        "version": 1,
                                        "capabilities": ["recommendedValue"]
                                    }}}
                                }
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({ "cwd": cwd_for_client.to_string_lossy() }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("a rejected effort seed fails session/new");
                    assert_eq!(i32::from(created.code), -32603);
                    assert!(
                        created.message.contains("effort seed rejected"),
                        "the CLI's error text: {}",
                        created.message
                    );
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await
    .expect("the client run finishes");
    result.expect("no protocol error");
    serve_task.abort();
}
