//! T5B lane B — `session/fork` end-to-end with the fake CLI harness:
//! M-32 (no fork point copies the latest transcript), M-33/M-90 (a resolved
//! fork point copies up to the message and starts the fork under the cwd
//! project), and the `invalidParams` answer for an unknown point.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::session_store::{project_slug, projects_root_with_env};
use common::{fake_claude_binary, read_json, repo_root, wait_for_file};
use serde_json::{json, Value};
use tokio::time::timeout;

const SOURCE: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";

fn init_transcript(tmp: &Path) -> PathBuf {
    let transcript = tmp.join("init.transcript.jsonl");
    std::fs::write(
        &transcript,
        "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[],\"commands\":[],\"account\":{}}}}]}\n",
    )
    .expect("write transcript");
    transcript
}

fn seed_session(config_dir: &Path, cwd: &Path, session_id: &str, body: &str) -> PathBuf {
    let root = projects_root_with_env(&EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]));
    let slug = project_slug(&cwd.to_string_lossy());
    let dir = root.join(&slug);
    std::fs::create_dir_all(&dir).expect("project dir");
    let path = dir.join(format!("{session_id}.jsonl"));
    std::fs::write(&path, body).expect("seed transcript");
    path
}

fn seed_source(config_dir: &Path, cwd: &Path, body: &str) -> PathBuf {
    seed_session(config_dir, cwd, SOURCE, body)
}

fn source_body() -> String {
    [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"one"}})
        .to_string(),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:01.000Z",
            "message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"first"}]}})
        .to_string(),
        json!({"type":"user","uuid":"u2","parentUuid":"a1","sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:02.000Z",
            "message":{"role":"user","content":"two"}})
        .to_string(),
        json!({"type":"assistant","uuid":"a2","parentUuid":"u2","sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:03.000Z",
            "message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"second"}]}})
        .to_string(),
    ]
    .join("\n")
        + "\n"
}

fn serve_options(
    transcript: &Path,
    config_dir: &Path,
    argv_out: &Path,
    cwd: &Path,
) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_ARGV_OUT".to_string(),
                argv_out.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.to_path_buf()),
        ..ServeOptions::default()
    }
    .with_env(EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]))
}

fn error_code(err: &agent_client_protocol::Error) -> i64 {
    serde_json::to_value(err)
        .ok()
        .and_then(|v| v.get("code").cloned())
        .and_then(|c| c.as_i64())
        .unwrap_or(i64::MAX)
}

fn fork_file(config_dir: &Path, cwd: &Path, fork_id: &str) -> PathBuf {
    let root = projects_root_with_env(&EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]));
    root.join(project_slug(&cwd.to_string_lossy()))
        .join(format!("{fork_id}.jsonl"))
}

fn prompt_transcript(tmp: &Path) -> PathBuf {
    let transcript = tmp.join("prompt.transcript.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[],\"commands\":[],\"account\":{}}}}]}\n",
            "{\"expect\":{\"type\":\"user\"},\"emit\":[",
            "{\"type\":\"user\",\"uuid\":\"$MATCH.uuid\",\"isReplay\":true,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}},",
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parent_tool_use_id\":null,\"message\":{\"id\":\"m1\",\"model\":\"claude-sonnet-4-5\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}},",
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}",
            "]}\n"
        ),
    )
    .expect("write transcript");
    transcript
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("fork file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect()
}

/// M-32/M-90 — a fork without a point copies the latest transcript into a new
/// session under the cwd project and starts it with `--resume=<new id>`.
#[tokio::test]
async fn m32_fork_without_point_copies_latest_transcript() {
    let tmp = std::env::temp_dir().join(format!("t5b-fork-latest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    seed_source(&config_dir, &cwd, &source_body());
    let transcript = init_transcript(&tmp);
    let argv_out = tmp.join("argv.json");
    let opts = serve_options(&transcript, &config_dir, &argv_out, &cwd);

    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let params = json!({"sessionId": SOURCE, "cwd": cwd.to_string_lossy(), "mcpServers": []});
    let response = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                let response = connection
                    .send_request(UntypedMessage::new("session/fork", params)?)
                    .block_task()
                    .await?;
                Ok::<Value, agent_client_protocol::Error>(response)
            },
        )
        .await
        .expect("client run");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let fork_id = response["sessionId"].as_str().expect("fork id").to_string();
    assert_ne!(fork_id, SOURCE);

    wait_for_file(&argv_out, Duration::from_secs(10)).expect("argv recorded");
    let argv: Vec<String> = read_json(&argv_out)["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        argv.iter().any(|a| a == &format!("--resume={fork_id}")),
        "the fork starts as a resumed session: {argv:?}"
    );

    let lines = read_lines(&fork_file(&config_dir, &cwd, &fork_id));
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[0]["message"]["content"], "one");
    assert_eq!(lines[2]["message"]["content"], "two");
    assert_eq!(
        lines[0]["forkedFrom"],
        json!({"sessionId": SOURCE, "messageUuid": "u1"})
    );
    assert_eq!(lines[4]["type"], "custom-title");
    assert_eq!(lines[4]["customTitle"], json!("one (fork)"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-33 — a valid AIR fork point resolves and the copy stops at that message.
#[tokio::test]
async fn m33_fork_point_copies_up_to_the_message() {
    let tmp = std::env::temp_dir().join(format!("t5b-fork-point-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    seed_source(&config_dir, &cwd, &source_body());
    let transcript = init_transcript(&tmp);
    let argv_out = tmp.join("argv.json");
    let opts = serve_options(&transcript, &config_dir, &argv_out, &cwd);

    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let params = json!({
        "sessionId": SOURCE,
        "cwd": cwd.to_string_lossy(),
        "mcpServers": [],
        "_meta": {"jetbrains": {"air": {"fork": {
            "version": 1, "messageId": "m1:segment:2"
        }}}},
    });
    let response = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                let response = connection
                    .send_request(UntypedMessage::new("session/fork", params)?)
                    .block_task()
                    .await?;
                Ok::<Value, agent_client_protocol::Error>(response)
            },
        )
        .await
        .expect("client run");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let fork_id = response["sessionId"].as_str().expect("fork id").to_string();
    let lines = read_lines(&fork_file(&config_dir, &cwd, &fork_id));
    // u1, a1 (the fork point), custom-title: u2/a2 are past the point. The
    // `:segment:2` suffix is stripped to the ACP id `m1`.
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[1]["message"]["id"], "m1");
    assert_eq!(lines[2]["type"], "custom-title");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-33 — an unknown fork point is `invalidParams` (-32602) with
/// `data:{messageId}` and no CLI spawn.
#[tokio::test]
async fn m33_unknown_fork_point_is_invalid_params() {
    let tmp = std::env::temp_dir().join(format!("t5b-fork-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    seed_source(&config_dir, &cwd, &source_body());
    let transcript = init_transcript(&tmp);
    let argv_out = tmp.join("argv.json");
    let opts = serve_options(&transcript, &config_dir, &argv_out, &cwd);

    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let params = json!({
        "sessionId": SOURCE,
        "cwd": cwd.to_string_lossy(),
        "mcpServers": [],
        "_meta": {"jetbrains": {"air": {"fork": {"version": 1, "messageId": "nope"}}}},
    });
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                let response = connection
                    .send_request(UntypedMessage::new("session/fork", params)?)
                    .block_task()
                    .await?;
                Ok::<Value, agent_client_protocol::Error>(response)
            },
        )
        .await;
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let err = result.expect_err("unknown fork point must reject");
    assert_eq!(error_code(&err), -32602);
    assert_eq!(err.data, Some(json!({"messageId": "nope"})));
    assert!(!argv_out.exists(), "no CLI spawn for an unresolved point");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// B-29 — the session records the ACP messageId -> SDK uuid map per
/// consolidated assistant message; `session/fork` resolves the fork point from
/// that live map before reading the transcript (the seeded transcript's own id
/// for `a1` is different).
#[tokio::test]
async fn b29_fork_uses_the_live_message_id_map() {
    let tmp = std::env::temp_dir().join(format!("t5b-live-map-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    let transcript = prompt_transcript(&tmp);
    let argv_out = tmp.join("argv.json");
    let opts = serve_options(&transcript, &config_dir, &argv_out, &cwd);

    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let config_for_client = config_dir.clone();
    let cwd_for_client = cwd.clone();
    let response = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let config_dir = config_for_client.clone();
                let cwd = cwd_for_client.clone();
                async move {
                    let new = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
                        )?)
                        .block_task()
                        .await?;
                    let session_id = new["sessionId"].as_str().expect("session id").to_string();
                    // The persisted transcript does not use the live `m1` id for
                    // `a1`, so only the live map can resolve the fork point.
                    let body = format!(
                        "{}\n{}\n",
                        json!({"type":"user","uuid":"u1","parentUuid":null,
                            "sessionId":session_id,"timestamp":"2026-01-01T00:00:00.000Z",
                            "message":{"role":"user","content":"hi"}}),
                        json!({"type":"assistant","uuid":"a1","parentUuid":"u1",
                            "sessionId":session_id,"timestamp":"2026-01-01T00:00:01.000Z",
                            "message":{"id":"persisted-id","role":"assistant",
                                "content":[{"type":"text","text":"ok"}]}})
                    );
                    seed_session(&config_dir, &cwd, &session_id, &body);
                    connection
                        .send_request(UntypedMessage::new(
                            "session/prompt",
                            json!({"sessionId": session_id,
                                "prompt":[{"type":"text","text":"hi"}]}),
                        )?)
                        .block_task()
                        .await?;
                    let fork = connection
                        .send_request(UntypedMessage::new(
                            "session/fork",
                            json!({
                                "sessionId": session_id,
                                "cwd": cwd.to_string_lossy(),
                                "mcpServers": [],
                                "_meta": {"jetbrains": {"air": {"fork": {
                                    "version": 1, "messageId": "m1"
                                }}}},
                            }),
                        )?)
                        .block_task()
                        .await?;
                    Ok::<Value, agent_client_protocol::Error>(fork)
                }
            },
        )
        .await;
    let _ = timeout(Duration::from_secs(15), serve_task).await;
    let fork = response.expect("the exchange succeeded");
    let fork_id = fork["sessionId"].as_str().expect("fork id");
    let lines = read_lines(&fork_file(&config_dir, &cwd, fork_id));
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[1]["forkedFrom"]["messageUuid"], json!("a1"));
    assert_eq!(lines[2]["type"], "custom-title");
    let _ = std::fs::remove_dir_all(&tmp);
}
