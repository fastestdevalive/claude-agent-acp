//! T4A3 STEP 5 (F24: S-03/A-27) — `/remote-control [name|off]` answered by the
//! adapter through `enableRemoteControl`, without a model turn.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

const INIT_STEP: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#;

fn transcript(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    std::fs::write(&path, text).expect("write transcript");
    path
}

/// Drive a `/remote-control` prompt. Returns the prompt response and every
/// agent_message_chunk text.
async fn drive_remote(
    tmp: &Path,
    transcript_path: &Path,
    prompt: &str,
    cancel_after: Option<Duration>,
    settle_before_prompt: Option<Duration>,
) -> (Value, Vec<String>) {
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        timings: claude_agent_acp_rs::process::Timings {
            force_cancel_grace: Duration::from_millis(1000),
            ..claude_agent_acp_rs::process::Timings::default()
        },
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Value>();
    let result = Client
        .builder()
        .on_receive_notification(
            {
                let notif_tx = notif_tx.clone();
                async move |notification: UntypedMessage, _cx| {
                    let _ = notif_tx.send(json!({
                        "method": notification.method,
                        "params": notification.params,
                    }));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_channel, {
            let cwd = cwd.clone();
            let prompt = prompt.to_string();
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({ "protocolVersion": 1 }),
                    )?)
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({ "cwd": cwd.to_string_lossy() }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                if let Some(delay) = cancel_after {
                    // Start a turn that stays in flight, then ask for
                    // /remote-control while Claude is working.
                    let started = {
                        let connection = connection.clone();
                        let session_id = session_id.clone();
                        tokio::spawn(async move {
                            let msg = UntypedMessage::new(
                                "session/prompt",
                                json!({
                                    "sessionId": session_id,
                                    "prompt": [{ "type": "text", "text": "hold" }],
                                }),
                            )?;
                            let _ = connection.send_request(msg).block_task().await;
                            Ok::<(), agent_client_protocol::Error>(())
                        })
                    };
                    tokio::time::sleep(delay).await;
                    let response = connection
                        .send_request(UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{ "type": "text", "text": prompt }],
                            }),
                        )?)
                        .block_task()
                        .await;
                    // Stop the held turn so the child is not left running.
                    connection.send_notification(
                        agent_client_protocol::schema::v1::CancelNotification::new(
                            session_id.clone(),
                        ),
                    )?;
                    // Let the cancelled turn settle (bounded) before the test
                    // ends.
                    let _ = timeout(Duration::from_secs(5), started).await;
                    return match response {
                        Ok(value) => Ok(value),
                        Err(error) => Ok(json!({
                            "error": serde_json::to_value(error).unwrap_or(Value::Null)
                        })),
                    };
                }
                if let Some(settle) = settle_before_prompt {
                    // Let an unsolicited frame the CLI emitted at session start
                    // reach the session before the prompt is sent.
                    tokio::time::sleep(settle).await;
                }
                let response = connection
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        json!({
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": prompt }],
                        }),
                    )?)
                    .block_task()
                    .await?;
                Ok::<Value, agent_client_protocol::Error>(response)
            }
        })
        .await;

    let response = result
        .unwrap_or_else(|e| json!({ "error": serde_json::to_value(e).unwrap_or(Value::Null) }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut chunks = Vec::new();
    while let Ok(value) = notif_rx.try_recv() {
        if value["method"] == "session/update"
            && value["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        {
            if let Some(text) = value["params"]["update"]["content"]["text"].as_str() {
                chunks.push(text.to_string());
            }
        }
    }
    drop(serve_task);
    (response, chunks)
}

/// S-03/A-27 — `/remote-control` connects through `enableRemoteControl(true)`
/// and answers with the session URL, bypassing the turn queue.
#[tokio::test]
async fn t4a3_remote_control_connect() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-connect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"control_request","request":{"subtype":"remote_control","enabled":true}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"session_url":"https://claude.ai/code/abc"}}}]}"#
    );
    let transcript_path = transcript(&tmp, "connect", &text);

    let (response, chunks) =
        drive_remote(&tmp, &transcript_path, "/remote-control", None, None).await;

    assert_eq!(response["stopReason"], "end_turn", "{response}");
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    assert_eq!(
        chunks[0],
        "Remote Control is active. Continue this session from claude.ai/code or the Claude mobile app:\n\nhttps://claude.ai/code/abc\n\nRun /remote-control off to disconnect."
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-03/A-27 — `/remote-control off` disconnects through
/// `enableRemoteControl(false)`.
#[tokio::test]
async fn t4a3_remote_control_off() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"control_request","request":{"subtype":"remote_control","enabled":false}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{}}}]}"#
    );
    let transcript_path = transcript(&tmp, "off", &text);

    let (response, chunks) =
        drive_remote(&tmp, &transcript_path, "/remote-control off", None, None).await;

    assert_eq!(response["stopReason"], "end_turn", "{response}");
    assert_eq!(
        chunks,
        vec!["Remote Control is off. Run /remote-control to reconnect.".to_string()],
        "{chunks:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-03 — a name is forwarded to `enableRemoteControl`.
#[tokio::test]
async fn t4a3_remote_control_named_connect() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-name-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"control_request","request":{"subtype":"remote_control","enabled":true,"name":"my session"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"session_url":"https://claude.ai/code/named"}}}]}"#
    );
    let transcript_path = transcript(&tmp, "name", &text);

    let (response, chunks) = drive_remote(
        &tmp,
        &transcript_path,
        "/remote-control my session",
        None,
        None,
    )
    .await;

    assert_eq!(response["stopReason"], "end_turn", "{response}");
    assert!(
        chunks[0].contains("https://claude.ai/code/named"),
        "{chunks:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-03 — the reply bypasses the turn queue, so it is refused with `-32600`
/// while Claude is working.
#[tokio::test]
async fn t4a3_remote_control_refused_while_working() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"hold"}]}},{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}]}"#
    );
    let transcript_path = transcript(&tmp, "busy", &text);

    let (response, _) = drive_remote(
        &tmp,
        &transcript_path,
        "/remote-control",
        Some(Duration::from_millis(300)),
        None,
    )
    .await;

    assert_eq!(
        response["error"]["code"], -32600,
        "a /remote-control while Claude works is invalidRequest: {response}"
    );
    assert_eq!(
        response["error"]["message"],
        "Invalid request: Remote Control can't be changed while Claude is working. Try again when the turn ends.",
        "{response}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-03 — a cycle Claude Code started on its own (a background subagent's
/// follow-up) is foreground work even though no turn is queued
/// (`foregroundWork.has`, `acp-agent.ts:3824-3832`): `/remote-control` is
/// refused with `-32600` rather than interleaving with the cycle's output.
#[tokio::test]
async fn t4a3_remote_control_refused_during_an_autonomous_cycle() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-autonomous-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    // The initialize response and the cycle's first main-conversation output
    // arrive together at session start: no prompt is sent, so no turn exists.
    let text = format!(
        "{}\n",
        r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}},{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_a","model":"claude-sonnet-4-5","role":"assistant","content":[{"type":"text","text":"follow-up after a background subagent"}]}}]}"#
    );
    let transcript_path = transcript(&tmp, "autonomous", &text);

    // Bounded: an unrefused prompt would wait for a control reply the fake
    // never sends.
    let (response, _) = timeout(
        Duration::from_secs(10),
        drive_remote(
            &tmp,
            &transcript_path,
            "/remote-control",
            None,
            Some(Duration::from_millis(500)),
        ),
    )
    .await
    .expect("the refusal answers without a model turn");

    assert_eq!(
        response["error"]["code"], -32600,
        "a /remote-control during an autonomous cycle is invalidRequest: {response}"
    );
    assert_eq!(
        response["error"]["message"],
        "Invalid request: Remote Control can't be changed while Claude is working. Try again when the turn ends.",
        "{response}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-03 — the adapter steps aside when Claude Code offers its own
/// `remote-control` command; the prompt goes to the CLI as a normal turn.
#[tokio::test]
async fn t4a3_remote_control_not_intercepted_when_cli_offers_it() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-offered-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[{"name":"remote-control","description":"own","argumentHint":"[name]"}]}}}]}"#,
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"/remote-control"}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"cli handled it","usage":{"input_tokens":1,"output_tokens":1}}]}"#
    );
    let transcript_path = transcript(&tmp, "offered", &text);

    let (response, chunks) =
        drive_remote(&tmp, &transcript_path, "/remote-control", None, None).await;

    assert_eq!(response["stopReason"], "end_turn", "{response}");
    assert!(
        chunks.is_empty(),
        "the CLI's own command is not shadowed: {chunks:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-03 — a prompt that follows `/remote-control` waits for its reply, so the
/// two responses cannot interleave (`acp-agent.ts:3904`).
#[tokio::test]
async fn t4a3_remote_control_holds_a_following_prompt() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-hold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        // The control reply is delayed, so an unheld prompt would overtake it.
        r#"{"expect":{"type":"control_request","request":{"subtype":"remote_control","enabled":true}},"emit":[{"sleep_ms":500},{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"session_url":"https://claude.ai/code/abc"}}}]}"#,
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"next"}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"ok","usage":{"input_tokens":1,"output_tokens":1}}]}"#
    );
    let transcript_path = transcript(&tmp, "hold", &text);
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let result = Client
        .builder()
        .connect_with(client_channel, {
            let cwd = cwd.clone();
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({ "protocolVersion": 1 }),
                    )?)
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({ "cwd": cwd.to_string_lossy() }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                let (order_tx, mut order_rx) = mpsc::unbounded_channel::<&'static str>();
                let remote = {
                    let connection = connection.clone();
                    let session_id = session_id.clone();
                    let order_tx = order_tx.clone();
                    tokio::spawn(async move {
                        let msg = UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{ "type": "text", "text": "/remote-control" }],
                            }),
                        )?;
                        let _ = connection.send_request(msg).block_task().await;
                        let _ = order_tx.send("remote-control");
                        Ok::<(), agent_client_protocol::Error>(())
                    })
                };
                let next = {
                    let connection = connection.clone();
                    let session_id = session_id.clone();
                    let order_tx = order_tx.clone();
                    tokio::spawn(async move {
                        let msg = UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{ "type": "text", "text": "next" }],
                            }),
                        )?;
                        let _ = connection.send_request(msg).block_task().await;
                        let _ = order_tx.send("next");
                        Ok::<(), agent_client_protocol::Error>(())
                    })
                };
                let _ = timeout(Duration::from_secs(15), remote).await;
                let _ = timeout(Duration::from_secs(15), next).await;
                let first = order_rx.recv().await.unwrap_or("none");
                let second = order_rx.recv().await.unwrap_or("none");
                assert_eq!(
                    (first, second),
                    ("remote-control", "next"),
                    "the following prompt waits for the Remote Control reply"
                );
                Ok::<(), agent_client_protocol::Error>(())
            }
        })
        .await;

    assert!(result.is_ok(), "both prompts settle: {result:?}");
    drop(serve_task);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-03 — a second `/remote-control` while one is in flight is refused with
/// `-32600`, and the CLI sees only the first command.
#[tokio::test]
async fn t4a3_remote_control_refuses_a_second_command() {
    let tmp = std::env::temp_dir().join(format!("t4a3-rc-second-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"control_request","request":{"subtype":"remote_control","enabled":true}},"emit":[{"sleep_ms":500},{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"session_url":"https://claude.ai/code/abc"}}}]}"#
    );
    let transcript_path = transcript(&tmp, "second", &text);
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let result = Client
        .builder()
        .connect_with(client_channel, {
            let cwd = cwd.clone();
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({ "protocolVersion": 1 }),
                    )?)
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({ "cwd": cwd.to_string_lossy() }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                let remote = {
                    let connection = connection.clone();
                    let session_id = session_id.clone();
                    tokio::spawn(async move {
                        let msg = UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{ "type": "text", "text": "/remote-control" }],
                            }),
                        )?;
                        connection.send_request(msg).block_task().await
                    })
                };
                tokio::time::sleep(Duration::from_millis(50)).await;
                let second = connection
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        json!({
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": "/remote-control off" }],
                        }),
                    )?)
                    .block_task()
                    .await;
                let second = second.expect_err("a second command is refused");
                let value = serde_json::to_value(&second).unwrap_or(Value::Null);
                assert_eq!(
                    value["code"], -32600,
                    "a second command is invalidRequest: {value}"
                );
                assert_eq!(
                    value["message"],
                    "Invalid request: Remote Control is already being changed. Try again in a moment.",
                    "{value}"
                );
                let first = timeout(Duration::from_secs(10), remote)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .and_then(Result::ok)
                    .unwrap_or(Value::Null);
                assert_eq!(first["stopReason"], "end_turn", "{first}");
                Ok::<(), agent_client_protocol::Error>(())
            }
        })
        .await;

    assert!(result.is_ok(), "the first command completes: {result:?}");
    drop(serve_task);
    let _ = std::fs::remove_dir_all(&tmp);
}
