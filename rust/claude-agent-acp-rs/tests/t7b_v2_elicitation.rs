//! T7B STEP 4 — elicitation over the v2 surface (U-87; upstream
//! `src/v2/agent.ts:353-369`: elicitation is the same in v1 and v2, the params
//! are relayed unchanged and a completion is a notification).
//!
//! Hermetic duplex connection, local `fake-claude`, every wait bounded.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, Error, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

const INIT_STEP: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t7b-elicit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

fn transcript(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    std::fs::write(&path, text).expect("write transcript");
    path
}

/// One turn: the prompt echo, the CLI's inbound control request, then (after
/// the agent's answer) the result frame.
fn control_turn(control: Value, response_expect: Value) -> String {
    let first = json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "go" }],
                },
            },
            control,
        ],
    });
    let second = json!({
        "expect": { "type": "control_response", "response": response_expect },
        "emit": [
            {
                "type": "result",
                "subtype": "success",
                "stop_reason": "end_turn",
                "is_error": false,
                "result": "ok",
                "usage": { "input_tokens": 1, "output_tokens": 1 },
            },
        ],
    });
    format!("{}\n{}\n", first, second)
}

fn mcp_form_control() -> Value {
    json!({
        "type": "control_request",
        "request_id": "elicit-1",
        "request": {
            "subtype": "elicitation",
            "serverName": "srv",
            "message": "Enter a value",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "val": { "type": "string" } },
            },
        },
    })
}

fn options(tmp: &Path, transcript_path: &Path) -> ServeOptions {
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript_path.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::from_pairs([
            (EXPERIMENTAL_V2_ENV.to_string(), "1".to_string()),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ]),
        ..Default::default()
    }
}

/// What the client saw: requests it was asked and notifications it got.
#[derive(Default)]
struct Seen {
    requests: Vec<Value>,
    notifications: Vec<Value>,
    prompt: Option<Result<Value, Error>>,
}

/// Runs a v2 session whose prompt is answered by `answer`, collecting the
/// client requests/notifications.
async fn drive(tmp: &Path, transcript_path: &Path, caps: Value, answer: Value) -> Seen {
    let opts = options(tmp, transcript_path);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Value>();
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Value>();
    let cwd = repo_root();
    let result = timeout(
        Duration::from_secs(60),
        Client
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
                {
                    let req_tx = req_tx.clone();
                    async move |request: UntypedMessage, responder, _cx| {
                        let method = request.method.clone();
                        let params = request.params.clone();
                        let _ = req_tx.send(json!({"method": method, "params": params}));
                        responder.respond(answer.clone())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<Agent>| async move {
                    connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({
                                "protocolVersion": 2,
                                "info": {"name": "v2-elicit", "version": "1"},
                                "capabilities": caps,
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
                        )?)
                        .block_task()
                        .await?;
                    let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                    let response = connection
                        .send_request(UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{ "type": "text", "text": "go" }],
                            }),
                        )?)
                        .block_task()
                        .await;
                    Ok::<_, Error>(response)
                },
            ),
    )
    .await;
    // Let a trailing `elicitation/complete` notification flush.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut seen = Seen::default();
    while let Ok(value) = req_rx.try_recv() {
        seen.requests.push(value);
    }
    while let Ok(value) = notif_rx.try_recv() {
        seen.notifications.push(value);
    }
    seen.prompt = Some(result.expect("in time").expect("exchange"));
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    seen
}

/// U-87 — a form elicitation over v2: the client gets `elicitation/create` with
/// the params relayed unchanged, its accept content answers the CLI, and the
/// prompt resolves with a v2 `messageId`.
#[tokio::test]
async fn u87_v2_form_elicitation_is_relayed_unchanged() {
    let tmp = temp_dir("form");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            mcp_form_control(),
            json!({
                "subtype": "success",
                "request_id": "elicit-1",
                "response": { "action": "accept", "content": { "val": "hi" } },
            })
        )
    );
    let path = transcript(&tmp, "form", &text);
    let seen = drive(
        &tmp,
        &path,
        json!({"elicitation": {"form": {}}}),
        json!({"action": "accept", "content": {"val": "hi"}}),
    )
    .await;

    let elicitations: Vec<&Value> = seen
        .requests
        .iter()
        .filter(|r| r["method"] == "elicitation/create")
        .collect();
    assert_eq!(elicitations.len(), 1, "{:?}", seen.requests);
    let params = &elicitations[0]["params"];
    // Relayed unchanged, including the MCP server's schema.
    assert_eq!(params["message"], "Enter a value");
    assert_eq!(params["mode"], "form");
    assert_eq!(
        params["requestedSchema"],
        json!({"type": "object", "properties": {"val": {"type": "string"}}})
    );

    let prompt = seen.prompt.expect("prompt result");
    let prompt = prompt.expect("the turn succeeded");
    assert!(
        prompt.get("messageId").and_then(Value::as_str).is_some(),
        "a v2 prompt answers a messageId: {prompt}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// U-87 — a url elicitation completion over v2 is an `elicitation/complete`
/// notification with the same params.
#[tokio::test]
async fn u87_v2_elicitation_complete_notification() {
    let tmp = temp_dir("complete");
    let step = json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "go" }],
                },
            },
            {
                "type": "system",
                "subtype": "elicitation_complete",
                "elicitation_id": "elic-777",
            },
            {
                "type": "result",
                "subtype": "success",
                "stop_reason": "end_turn",
                "is_error": false,
                "result": "ok",
                "usage": { "input_tokens": 1, "output_tokens": 1 },
            },
        ],
    });
    let text = format!("{}\n{}\n", INIT_STEP, step);
    let path = transcript(&tmp, "complete", &text);
    let seen = drive(
        &tmp,
        &path,
        json!({"elicitation": {"url": {}}}),
        json!({"action": "accept"}),
    )
    .await;

    let completes: Vec<&Value> = seen
        .notifications
        .iter()
        .filter(|n| n["method"] == "elicitation/complete")
        .collect();
    assert_eq!(completes.len(), 1, "{:?}", seen.notifications);
    assert_eq!(completes[0]["params"]["elicitationId"], "elic-777");
    assert!(seen.prompt.expect("prompt result").is_ok());
    let _ = std::fs::remove_dir_all(&tmp);
}
