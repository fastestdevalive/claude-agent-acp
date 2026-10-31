//! T4FD — the `session/cancel` fallback of a native subagent's child session
//! (`acp-agent.ts:8383-8408`): an id that is no session of the connection is
//! looked up as a child of an RFD runtime, and its task is stopped with a
//! `stop_task` control request while the parent's work goes on.
//!
//! The client is a real `Client` over a duplex channel; `claude` is the
//! fake-claude transcript harness. Every wait is bounded.

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

fn transcript(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    std::fs::write(&path, text).expect("write transcript");
    path
}

const INIT_STEP: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#;

/// The prompt that launches a subagent, and the child's stop the cancel asks
/// for; the child's end is published only after the `stop_task` matched.
fn subagent_transcript() -> String {
    format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"go"}]}},{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_A","name":"Agent","input":{"description":"Fix","prompt":"Make it pass"}}]}},{"type":"system","subtype":"task_started","task_id":"t1","tool_use_id":"toolu_A","subagent_type":"general-purpose","description":"Fix","prompt":"Make it pass"}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"stop_task","task_id":"t1"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{}}},{"type":"system","subtype":"task_notification","task_id":"t1","status":"cancelled","tool_use_id":"toolu_A"}]}"#,
    )
}

#[tokio::test]
async fn t4fd_cancel_of_a_child_session_stops_its_task() {
    let tmp = std::env::temp_dir().join(format!("t4fd-cancel-child-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let transcript_path = transcript(&tmp, "cancel-child", &subagent_transcript());
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

    let (notif_tx, notif_rx) = mpsc::unbounded_channel::<Value>();
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
        .on_receive_request(
            async move |_request: UntypedMessage, responder, _cx| responder.respond(json!({})),
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_channel, {
            let cwd = cwd.clone();
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({
                            "protocolVersion": 1,
                            "clientCapabilities": { "subagents": {} },
                        }),
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
                let mut notif_rx = notif_rx;
                let prompt = {
                    let connection = connection.clone();
                    let session_id = session_id.clone();
                    tokio::spawn(async move {
                        if let Ok(msg) = UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{ "type": "text", "text": "go" }],
                            }),
                        ) {
                            connection.send_request(msg).block_task().await
                        } else {
                            Err(agent_client_protocol::Error::invalid_params())
                        }
                    })
                };
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                // The announced child session.
                let mut child = None;
                while child.is_none() {
                    if let Ok(Some(value)) =
                        timeout(Duration::from_millis(100), notif_rx.recv()).await
                    {
                        if value["params"]["update"]["sessionUpdate"] == "subagent_update"
                            && value["params"]["update"].get("capabilities").is_some()
                        {
                            child = value["params"]["update"]["sessionId"]
                                .as_str()
                                .map(String::from);
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        prompt.abort();
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("no subagent_announce arrived"));
                    }
                }
                let child = child.expect("the child session id");
                // `session/cancel` for the CHILD session: not a session of this
                // connection, so the fallback looks for a native child.
                connection.send_notification(
                    agent_client_protocol::schema::v1::CancelNotification::new(child.clone()),
                )?;
                // The child's end is emitted only after the `stop_task` matched.
                loop {
                    if let Ok(Some(value)) =
                        timeout(Duration::from_millis(100), notif_rx.recv()).await
                    {
                        if value["params"]["update"]["sessionUpdate"] == "subagent_update"
                            && value["params"]["update"]["sessionId"] == child.as_str()
                        {
                            prompt.abort();
                            return Ok::<Value, agent_client_protocol::Error>(value);
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        prompt.abort();
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("the child was not stopped"));
                    }
                }
            }
        })
        .await;

    assert!(
        result.is_ok(),
        "the child's cancel stopped its task: {result:?}"
    );
    let ended = result.unwrap();
    assert_eq!(
        ended["params"]["update"]["state"]["state"], "idle",
        "the stopped child reports idle: {ended}"
    );
    assert_eq!(
        ended["params"]["update"]["state"]["stopReason"], "cancelled",
        "the stopped child reports cancelled: {ended}"
    );
    drop(serve_task);
    let _ = std::fs::remove_dir_all(&tmp);
}
