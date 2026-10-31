//! T9A — `session-load.test.ts` rows that need the running agent:
//! a session created but never prompted has no messages and is not resumable,
//! and `session/load` reports the model the resumed transcript is running.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::session_store::{project_slug, projects_root_with_env};
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::time::timeout;

const LOAD_ID: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("acp-t9a-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// The initialize reply: the model that the transcript is running, beside the
/// picker's first (default) model.
fn init_step() -> Value {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {"models": [
                {"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5"},
                {"value": "claude-haiku-4-5", "displayName": "Claude Haiku 4.5"}
            ], "commands": [], "account": {}}
        }}]
    })
}

fn transcript(tmp: &Path, name: &str, steps: &[Value]) -> PathBuf {
    let path = tmp.join(name);
    let body: String = steps.iter().map(|step| step.to_string() + "\n").collect();
    std::fs::write(&path, body).expect("write transcript");
    path
}

fn options(tmp: &Path, cwd: &Path, config: &Path, script: &Path) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                script.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_CURSOR".to_string(),
                tmp.join("cursor").to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.to_path_buf()),
        env: EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config.to_string_lossy().into_owned(),
        )]),
        ..ServeOptions::default()
    }
}

fn seed_transcript(config: &Path, cwd: &Path, id: &str, messages: &[Value]) {
    let root = projects_root_with_env(&EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config.to_string_lossy().into_owned(),
    )]));
    let dir = root.join(project_slug(&cwd.to_string_lossy()));
    std::fs::create_dir_all(&dir).expect("project dir");
    let text: String = messages.iter().map(|m| format!("{m}\n")).collect();
    std::fs::write(dir.join(format!("{id}.jsonl")), text).expect("transcript");
}

fn error_code(err: &agent_client_protocol::Error) -> i64 {
    serde_json::to_value(err)
        .ok()
        .and_then(|v| v.get("code").cloned())
        .and_then(|c| c.as_i64())
        .unwrap_or(i64::MAX)
}

/// `session-load.test.ts` > "SDK: session created but never prompted has no
/// messages and is not resumable": the fresh session has no transcript, so a
/// load of its id is `resourceNotFound` (-32002).
#[tokio::test]
async fn t9a_a_session_created_but_never_prompted_is_not_loadable() {
    timeout(Duration::from_secs(30), async {
        let tmp = temp_dir("never-prompted");
        let cwd = repo_root();
        let config = tmp.join("config");
        std::fs::create_dir_all(&config).expect("config");
        // Segment 0 serves `session/new`; segment 1 is the `--resume` spawn that
        // `session/load` starts, and answers the CLI's not-found error.
        let not_found = json!({
            "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "error", "request_id": "$REQ",
                "error": "No conversation found with session ID: 50a49c8d"
            }}]
        });
        let script = transcript(
            &tmp,
            "never.transcript.jsonl",
            &[init_step(), json!({"segment": 1}), not_found],
        );
        let opts = options(&tmp, &cwd, &config, &script);
        let (agent_channel, client_channel) = Channel::duplex();
        let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
        let cwd_for_client = cwd.clone();
        let result = Client
            .builder()
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({"protocolVersion": 1}),
                        )?)
                        .block_task()
                        .await?;
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({"cwd": cwd_for_client.to_string_lossy(), "mcpServers": []}),
                        )?)
                        .block_task()
                        .await?;
                    let id = created["sessionId"].as_str().unwrap().to_string();
                    // Close without ever prompting: the CLI writes no transcript.
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "session/close",
                            json!({"sessionId": id}),
                        )?)
                        .block_task()
                        .await?;
                    let loaded = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            json!({
                                "sessionId": id,
                                "cwd": cwd_for_client.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await;
                    Ok::<_, agent_client_protocol::Error>(loaded)
                },
            )
            .await
            .expect("client run");
        let _ = timeout(Duration::from_secs(5), serve_task).await;
        let err = result.expect_err("a never-prompted session is not loadable");
        assert_eq!(error_code(&err), -32002, "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    })
    .await
    .expect("test timed out");
}

/// `session-load.test.ts` > "ACP: loadSession reports the model the resumed
/// session is actually running": the transcript's last real assistant model
/// wins over the freshly computed default.
#[tokio::test]
async fn t9a_load_reports_the_model_the_transcript_is_running() {
    timeout(Duration::from_secs(30), async {
        let tmp = temp_dir("load-model");
        let cwd = tmp.join("cwd");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let config = tmp.join("config");
        std::fs::create_dir_all(&config).expect("config");
        seed_transcript(
            &config,
            &cwd,
            LOAD_ID,
            &[
                json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":LOAD_ID,
                    "message":{"role":"user","content":"Reply with only the word OK"}}),
                json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":LOAD_ID,
                    "message":{"id":"m1","role":"assistant","model":"claude-haiku-4-5",
                        "content":[{"type":"text","text":"OK"}]}}),
            ],
        );
        let script = transcript(&tmp, "init.transcript.jsonl", &[init_step()]);
        let opts = options(&tmp, &cwd, &config, &script);
        let (agent_channel, client_channel) = Channel::duplex();
        let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
        let cwd_for_client = cwd.clone();
        let result = Client
            .builder()
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({"protocolVersion": 1}),
                        )?)
                        .block_task()
                        .await?;
                    let loaded = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            json!({
                                "sessionId": LOAD_ID,
                                "cwd": cwd_for_client.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    Ok::<_, agent_client_protocol::Error>(loaded)
                },
            )
            .await
            .expect("client run");
        let _ = timeout(Duration::from_secs(5), serve_task).await;
        let model = result["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["id"] == "model")
            .expect("the model option")
            .clone();
        assert_eq!(model["currentValue"], "claude-haiku-4-5", "{model}");
        let _ = std::fs::remove_dir_all(&tmp);
    })
    .await
    .expect("test timed out");
}
