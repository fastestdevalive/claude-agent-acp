//! T6B STEP 6 — replay of native subagent child sessions on `session/load`
//! (B-52, `acp-agent.ts:9479-9498`) through the running agent.
//!
//! An RFD client that loads a session with an Agent launch gets each child
//! announced right after its launch and its own transcript replayed into the
//! child's session; every child's end goes out before the replay ends.

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

const SESSION: &str = "88888888-8888-4888-8888-888888888888";
const AGENT: &str = "abc123";

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t6b-replay-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

fn project_dir(config_dir: &Path, cwd: &Path) -> PathBuf {
    let env = EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]);
    projects_root_with_env(&env).join(project_slug(&cwd.to_string_lossy()))
}

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

fn transcript(tmp: &Path) -> PathBuf {
    let path = tmp.join("load.transcript.jsonl");
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
    std::fs::write(&path, format!("{init}\n")).expect("write transcript");
    path
}

#[tokio::test]
async fn b52_load_replays_a_subagent_into_its_own_session() {
    let tmp = temp_dir("load");
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
                                "clientCapabilities": {"subagents": {}, "session": {"notices": {}}},
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let loaded = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            json!({
                                "sessionId": SESSION,
                                "cwd": cwd.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    assert_eq!(loaded["sessionId"], SESSION);
                    // Drain what followed the response too.
                    let mut seen = Vec::new();
                    while let Ok(Some(value)) = timeout(Duration::from_millis(200), rx.recv()).await
                    {
                        seen.push(value);
                    }
                    Ok::<Vec<Value>, agent_client_protocol::Error>(seen)
                },
            ),
    )
    .await
    .expect("in time")
    .expect("exchange");
    let _ = timeout(Duration::from_secs(10), serve_task).await;

    let kinds: Vec<(String, String)> = exchange
        .iter()
        .map(|value| {
            (
                value["sessionId"].as_str().unwrap_or("").to_string(),
                value["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect();

    // The child is announced right after its launch, on the root.
    let announce = exchange
        .iter()
        .position(|value| {
            value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == AGENT
                && value["update"]["state"]["state"] == "running"
        })
        .expect("the child is announced");
    assert_eq!(exchange[announce]["sessionId"], SESSION);
    assert_eq!(exchange[announce]["update"]["title"], "scout (Explore)");
    assert_eq!(exchange[announce]["update"]["description"], "Look around");

    // Its prompt and its own message go to the child's session.
    let prompt = exchange
        .iter()
        .position(|value| {
            value["sessionId"] == AGENT
                && value["update"]["sessionUpdate"] == "session_message"
                && value["update"]["messageId"] == "toolu_agent"
        })
        .expect("the child's prompt");
    assert_eq!(prompt, announce + 1, "the prompt follows the announce");
    assert_eq!(
        exchange[prompt]["update"]["content"][0]["text"],
        "Look around"
    );
    assert_eq!(exchange[prompt]["update"]["senderSessionId"], SESSION);
    let child_text = exchange
        .iter()
        .position(|value| {
            value["sessionId"] == AGENT
                && value["update"]["sessionUpdate"] == "agent_message_chunk"
                && value["update"]["messageId"] == "msg_child"
        })
        .expect("the child's message");
    assert_eq!(
        exchange[child_text]["update"]["content"]["text"],
        "Child report."
    );

    // The child's end comes before the replay ends: it is the last subagent
    // update, after the child's own message.
    let end = exchange
        .iter()
        .rposition(|value| {
            value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == AGENT
        })
        .expect("the child's end");
    assert_eq!(exchange[end]["sessionId"], SESSION);
    assert_eq!(
        exchange[end]["update"]["state"],
        json!({"state": "idle", "stopReason": "end_turn"})
    );
    assert!(end > child_text, "the child's end follows its history");
    assert!(
        kinds
            .iter()
            .any(|(id, kind)| id == SESSION && kind == "user_message_chunk"),
        "the root history replayed: {kinds:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// B-105: a launch without metadata is left out and a notice says that subagent
/// history is missing (the client advertised notices).
#[tokio::test]
async fn b105_a_launch_without_metadata_gets_a_notice() {
    let tmp = temp_dir("notice");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    // A session with a launch but no subagents directory at all.
    let dir = project_dir(&config_dir, &cwd);
    std::fs::create_dir_all(&dir).expect("project dir");
    let lines = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":SESSION,"cwd":cwd.to_string_lossy(),
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"Explore it"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":SESSION,"cwd":cwd.to_string_lossy(),
            "timestamp":"2026-01-01T00:00:01.000Z",
            "message":{"id":"m1","role":"assistant","content":[{
                "type":"tool_use","id":"toolu_lost","name":"Agent",
                "input":{"description":"Work","prompt":"Work"}}]}}),
    ]
    .into_iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(dir.join(format!("{SESSION}.jsonl")), lines).expect("session transcript");

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
            .connect_with(client_channel, move |connection: ConnectionTo<Agent>| async move {
                let _ = connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({
                            "protocolVersion": 1,
                            "clientCapabilities": {"subagents": {}, "session": {"notices": {}}},
                        }),
                    )?)
                    .block_task()
                    .await?;
                let _ = connection
                    .send_request(UntypedMessage::new(
                        "session/load",
                        json!({"sessionId": SESSION, "cwd": cwd.to_string_lossy(), "mcpServers": []}),
                    )?)
                    .block_task()
                    .await?;
                let mut seen = Vec::new();
                while let Ok(Some(value)) = timeout(Duration::from_millis(200), rx.recv()).await {
                    seen.push(value);
                }
                Ok::<Vec<Value>, agent_client_protocol::Error>(seen)
            }),
    )
    .await
    .expect("in time")
    .expect("exchange");
    let _ = timeout(Duration::from_secs(10), serve_task).await;

    let notice = exchange
        .iter()
        .find(|value| value["update"]["sessionUpdate"] == "notice")
        .expect("a notice about missing subagent history");
    assert_eq!(notice["sessionId"], SESSION);
    assert_eq!(
        notice["update"]["title"],
        "Some subagent history is not available"
    );
    assert_eq!(
        notice["update"]["description"],
        "The history of 1 subagent of this session could not be restored."
    );
    // The launch is not announced: the RFD forbids inventing an id.
    assert!(
        !exchange
            .iter()
            .any(|value| value["update"]["sessionUpdate"] == "subagent_update"),
        "no invented child: {exchange:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Runs `serve` for one `session/load` of the seeded `SESSION` and returns the
/// `session/update` notifications that followed the response.
async fn load_exchange(tmp: &Path, config_dir: &Path, cwd: &Path) -> Vec<Value> {
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript(tmp).to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.to_path_buf()),
        env: EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.to_string_lossy().into_owned(),
        )]),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let cwd = cwd.to_path_buf();
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
                                "clientCapabilities": {"subagents": {}, "session": {"notices": {}}},
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let loaded = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            json!({
                                "sessionId": SESSION,
                                "cwd": cwd.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    assert_eq!(loaded["sessionId"], SESSION);
                    let mut seen = Vec::new();
                    while let Ok(Some(value)) = timeout(Duration::from_millis(200), rx.recv()).await
                    {
                        seen.push(value);
                    }
                    Ok::<Vec<Value>, agent_client_protocol::Error>(seen)
                },
            ),
    )
    .await
    .expect("in time")
    .expect("exchange");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    exchange
}

/// Writes `records` as a JSONL transcript, linking each record to the previous
/// one; with `agent`, marks them as that agent's sidechain records.
fn write_jsonl(path: &Path, records: Vec<Value>, agent: Option<&str>) {
    let mut parent: Option<String> = None;
    let mut body = String::new();
    for mut record in records {
        if let Some(object) = record.as_object_mut() {
            object.insert("parentUuid".to_string(), json!(parent));
            if let Some(agent) = agent {
                object.insert("isSidechain".to_string(), json!(true));
                object.insert("agentId".to_string(), json!(agent));
            }
        }
        parent = record
            .get("uuid")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        body.push_str(&record.to_string());
        body.push('\n');
    }
    std::fs::write(path, body).expect("transcript");
}

fn record_line(kind: &str, uuid: &str, content: Value, cwd: &Path) -> Value {
    json!({
        "type": kind,
        "uuid": uuid,
        "sessionId": SESSION,
        "cwd": cwd.to_string_lossy(),
        "timestamp": "2026-01-01T00:00:00.000Z",
        "message": {"role": kind, "content": content},
    })
}

fn launch_block(launch_id: &str, name: &str, prompt: &str) -> Value {
    json!([{
        "type": "tool_use",
        "id": launch_id,
        "name": "Agent",
        "input": {"name": name, "subagent_type": "Explore",
            "description": prompt, "prompt": prompt},
    }])
}

/// T6FB2 HIGH 2: A launches B and B launches C; on load every child is
/// announced and its history sent, and the ends come children-first
/// (`acp-agent.ts:9479-9493` recurses the same replay).
#[tokio::test]
async fn b52_load_replays_nested_subagents() {
    let tmp = temp_dir("nested");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    let dir = project_dir(&config_dir, &cwd);
    let subagents = dir.join(SESSION).join("subagents");
    std::fs::create_dir_all(&subagents).expect("subagents dir");
    write_jsonl(
        &dir.join(format!("{SESSION}.jsonl")),
        vec![
            record_line("user", "u1", json!("Go"), &cwd),
            record_line(
                "assistant",
                "a1",
                launch_block("toolu_b", "b", "B work"),
                &cwd,
            ),
            record_line(
                "user",
                "u2",
                json!([{"type": "tool_result", "tool_use_id": "toolu_b", "content": "B done"}]),
                &cwd,
            ),
        ],
        None,
    );
    write_jsonl(
        &subagents.join("agent-B.jsonl"),
        vec![
            record_line("user", "b1", json!("B work"), &cwd),
            record_line(
                "assistant",
                "b2",
                launch_block("toolu_c", "c", "C work"),
                &cwd,
            ),
            record_line(
                "assistant",
                "b3",
                json!([{"type": "text", "text": "B report"}]),
                &cwd,
            ),
        ],
        Some("B"),
    );
    write_jsonl(
        &subagents.join("agent-C.jsonl"),
        vec![
            record_line("user", "c1", json!("C work"), &cwd),
            record_line(
                "assistant",
                "c2",
                json!([{"type": "text", "text": "C report"}]),
                &cwd,
            ),
        ],
        Some("C"),
    );
    std::fs::write(
        subagents.join("agent-B.meta.json"),
        json!({"agentType": "Explore", "name": "b", "description": "B work",
            "toolUseId": "toolu_b"})
        .to_string(),
    )
    .expect("meta");
    std::fs::write(
        subagents.join("agent-C.meta.json"),
        json!({"agentType": "Explore", "name": "c", "description": "C work",
            "toolUseId": "toolu_c"})
        .to_string(),
    )
    .expect("meta");

    let exchange = load_exchange(&tmp, &config_dir, &cwd).await;
    let index = |predicate: &dyn Fn(&Value) -> bool| -> usize {
        exchange.iter().position(predicate).expect("present")
    };
    let announce_b = index(&|value| {
        value["sessionId"] == SESSION
            && value["update"]["sessionUpdate"] == "subagent_update"
            && value["update"]["sessionId"] == "B"
            && value["update"]["state"]["state"] == "running"
    });
    let prompt_b = index(&|value| {
        value["sessionId"] == "B"
            && value["update"]["sessionUpdate"] == "session_message"
            && value["update"]["messageId"] == "toolu_b"
    });
    let announce_c = index(&|value| {
        value["sessionId"] == "B"
            && value["update"]["sessionUpdate"] == "subagent_update"
            && value["update"]["sessionId"] == "C"
            && value["update"]["state"]["state"] == "running"
    });
    let prompt_c = index(&|value| {
        value["sessionId"] == "C"
            && value["update"]["sessionUpdate"] == "session_message"
            && value["update"]["messageId"] == "toolu_c"
    });
    let c_text = index(&|value| {
        value["sessionId"] == "C" && value["update"]["content"]["text"] == "C report"
    });
    let end_c = index(&|value| {
        value["sessionId"] == "B"
            && value["update"]["sessionUpdate"] == "subagent_update"
            && value["update"]["sessionId"] == "C"
            && value["update"]["state"]["state"] == "idle"
    });
    let end_b = index(&|value| {
        value["sessionId"] == SESSION
            && value["update"]["sessionUpdate"] == "subagent_update"
            && value["update"]["sessionId"] == "B"
            && value["update"]["state"]["state"] == "idle"
    });
    assert!(announce_b < prompt_b, "{exchange:?}");
    assert!(prompt_b < announce_c, "{exchange:?}");
    assert!(announce_c < prompt_c, "{exchange:?}");
    assert!(prompt_c < c_text, "{exchange:?}");
    assert!(c_text < end_c, "{exchange:?}");
    assert!(end_c < end_b, "{exchange:?}");
    assert_eq!(
        exchange[end_b]["update"]["state"],
        json!({"state": "idle", "stopReason": "end_turn"})
    );
    assert_eq!(exchange[end_c]["update"]["state"], json!({"state": "idle"}));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// T6FB2 HIGH 1: a background child's persisted `<task-notification>` ends it
/// `completed`/`failed`, not `idle` (`acp-agent.ts:9261`).
#[tokio::test]
async fn b52_load_ends_a_background_child_with_its_notification() {
    let tmp = temp_dir("notified");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    let dir = project_dir(&config_dir, &cwd);
    let subagents = dir.join(SESSION).join("subagents");
    std::fs::create_dir_all(&subagents).expect("subagents dir");
    write_jsonl(
        &dir.join(format!("{SESSION}.jsonl")),
        vec![
            record_line("user", "u1", json!("Go"), &cwd),
            record_line(
                "assistant",
                "a1",
                launch_block("toolu_bg", "bg", "Work"),
                &cwd,
            ),
            // A background launch answers at once; the error result is not its end.
            record_line(
                "user",
                "u2",
                json!([{"type": "tool_result", "tool_use_id": "toolu_bg",
                    "content": "stopped", "is_error": true}]),
                &cwd,
            ),
            json!({
                "type": "user", "uuid": "u3", "parentUuid": "u2", "sessionId": SESSION,
                "cwd": cwd.to_string_lossy(),
                "origin": {"kind": "task-notification"},
                "message": {"role": "user", "content": [{"type": "text",
                    "text": "<task-notification>\n<task-id>bg1</task-id>\n<status>completed</status>\n</task-notification>"}]},
            }),
        ],
        None,
    );
    write_jsonl(
        &subagents.join("agent-bg1.jsonl"),
        vec![
            record_line("user", "c1", json!("Work"), &cwd),
            record_line(
                "assistant",
                "c2",
                json!([{"type": "text", "text": "Done"}]),
                &cwd,
            ),
        ],
        Some("bg1"),
    );
    std::fs::write(
        subagents.join("agent-bg1.meta.json"),
        json!({"agentType": "Explore", "name": "bg", "description": "Work",
            "toolUseId": "toolu_bg", "requestShape": "background"})
        .to_string(),
    )
    .expect("meta");

    let exchange = load_exchange(&tmp, &config_dir, &cwd).await;
    assert!(
        exchange.iter().any(|value| {
            value["sessionId"] == SESSION
                && value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "bg1"
                && value["update"]["state"]["state"] == "running"
        }),
        "the background child is announced: {exchange:?}"
    );
    let end = exchange
        .iter()
        .rfind(|value| {
            value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "bg1"
        })
        .expect("the child ends");
    assert_eq!(end["sessionId"], SESSION);
    assert_eq!(
        end["update"]["state"],
        json!({"state": "idle", "stopReason": "end_turn"})
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
