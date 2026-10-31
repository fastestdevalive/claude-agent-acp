//! T6B STEP 6 — restore of native subagent agents from before a load/resume
//! (B-98, `native-subagents.ts:721-870`) through the running agent.
//!
//! The session is resumed from a seeded transcript that launched an agent; the
//! fake CLI then sends the SendMessage call that resumed that agent, and the
//! session's native subagent runtime restores it from its subagent transcript
//! and announces it to the client.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::session_store::{project_slug, projects_root_with_env};
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SESSION: &str = "77777777-7777-4777-8777-777777777777";
const AGENT: &str = "abc123";

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t6b-restore-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

/// The project directory of the seeded session.
fn project_dir(config_dir: &Path, cwd: &Path) -> PathBuf {
    let env = EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]);
    projects_root_with_env(&env).join(project_slug(&cwd.to_string_lossy()))
}

/// Seeds the session transcript (with the launch) and the subagent's transcript
/// and metadata.
fn seed_session(config_dir: &Path, cwd: &Path) {
    let dir = project_dir(config_dir, cwd);
    std::fs::create_dir_all(dir.join(SESSION).join("subagents")).expect("subagents dir");
    let lines = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":SESSION,"cwd":cwd.to_string_lossy(),
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"Explore it"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":SESSION,"cwd":cwd.to_string_lossy(),
            "timestamp":"2026-01-01T00:00:01.000Z",
            "message":{"id":"m1","role":"assistant","content":[{
                "type":"tool_use","id":"toolu_agent","name":"Agent",
                "input":{"name":"scout","subagent_type":"Explore","description":"Look around","prompt":"Look around"}}]}}),
        json!({"type":"user","uuid":"u2","parentUuid":"a1","sessionId":SESSION,"cwd":cwd.to_string_lossy(),
            "timestamp":"2026-01-01T00:00:02.000Z",
            "message":{"role":"user","content":[{
                "type":"tool_result","tool_use_id":"toolu_agent","content":"Found it."}]}}),
    ]
    .into_iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(dir.join(format!("{SESSION}.jsonl")), lines).expect("session transcript");
    let subagents = dir.join(SESSION).join("subagents");
    let child = [
        json!({"type":"user","uuid":"c1","parentUuid":null,"sessionId":SESSION,
            "isSidechain":true,"agentId":AGENT,"cwd":cwd.to_string_lossy(),
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"Look around"}}),
        json!({"type":"assistant","uuid":"c2","parentUuid":"c1","sessionId":SESSION,
            "isSidechain":true,"agentId":AGENT,"cwd":cwd.to_string_lossy(),
            "timestamp":"2026-01-01T00:00:01.000Z",
            "message":{"id":"msg_child","role":"assistant","content":[{"type":"text","text":"Child report."}]}}),
    ]
    .into_iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(subagents.join(format!("agent-{AGENT}.jsonl")), child)
        .expect("child transcript");
    std::fs::write(
        subagents.join(format!("agent-{AGENT}.meta.json")),
        json!({"agentType":"Explore","name":"scout","description":"Look around","toolUseId":"toolu_agent"})
            .to_string(),
    )
    .expect("meta");
}

/// initialize, then the prompt's turn: the SendMessage call and its result that
/// resumed the agent.
fn transcript(tmp: &Path) -> PathBuf {
    let path = tmp.join("restore.transcript.jsonl");
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [{"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"}],
                "commands": [], "account": {},
            }
        }}]
    });
    let turn = json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}},
            {"type": "assistant", "uuid": "a-send", "parent_tool_use_id": null,
             "message": {"id": "m-send", "model": "claude-sonnet-4-5", "role": "assistant",
                "content": [{"type": "tool_use", "id": "toolu_send", "name": "SendMessage",
                    "input": {"to": "scout", "message": "Hurry up"}}]}},
            {"type": "user", "uuid": "u-result", "parent_tool_use_id": null,
             "tool_use_result": {"success": true, "resumedAgentId": "abc123"},
             "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_send", "content": "ok"}]}},
            {"type": "result", "subtype": "success", "is_error": false, "result": "ok",
             "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}
        ]
    });
    std::fs::write(&path, format!("{init}\n{turn}\n")).expect("write transcript");
    path
}

#[tokio::test]
async fn b98_resumed_session_restores_an_agent_from_its_transcript() {
    let tmp = temp_dir("e2e");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    seed_session(&config_dir, &cwd);

    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript(&tmp).to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.clone()),
        env: EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.to_string_lossy().into_owned(),
        )]),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    let exchange = timeout(
        Duration::from_secs(90),
        Client
            .builder()
            .on_receive_notification(
                async move |notification: UntypedMessage, _cx| {
                    if notification.method() == "session/update" {
                        let _ = tx.send(notification.params().clone());
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<Agent>| async move {
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({
                                "protocolVersion": 1,
                                "clientCapabilities": {"subagents": {}},
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let resumed = connection
                        .send_request(UntypedMessage::new(
                            "session/resume",
                            json!({
                                "sessionId": SESSION,
                                "cwd": cwd.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    assert_eq!(resumed["sessionId"], SESSION);
                    let _ = connection
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        json!({"sessionId": SESSION, "prompt": [{"type": "text", "text": "hi"}]}),
                    )?)
                    .block_task()
                    .await?;
                    // Wait for the restored agent's announce.
                    let mut seen = Vec::new();
                    while let Ok(Some(value)) = timeout(Duration::from_secs(10), rx.recv()).await {
                        let announced = value["update"]["sessionUpdate"] == "subagent_update"
                            && value["update"]["sessionId"] == AGENT
                            && value["update"]["title"].is_string();
                        seen.push(value);
                        if announced {
                            break;
                        }
                    }
                    Ok::<Vec<Value>, agent_client_protocol::Error>(seen)
                },
            ),
    )
    .await
    .expect("in time")
    .expect("exchange");
    let _ = timeout(Duration::from_secs(10), serve_task).await;

    let announce = exchange
        .iter()
        .find(|value| {
            value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == AGENT
                && value["update"]["title"].is_string()
        })
        .expect("the restored agent is announced");
    assert_eq!(announce["sessionId"], SESSION);
    assert_eq!(announce["update"]["title"], "scout (Explore)");
    assert_eq!(announce["update"]["description"], "Look around");
    let _ = std::fs::remove_dir_all(&tmp);
}
