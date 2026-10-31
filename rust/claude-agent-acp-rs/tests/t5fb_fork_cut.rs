//! T5FB lane FB — `session/fork` with a fork point on a transcript that has a
//! rewind branch and a superseded compaction boundary (M-33/M-90), and a
//! malformed fork point (a non-string `messageId`, the SDK's TypeError).
//!
//! Upstream `fork-session.ts` resolves the point, then the SDK `IF` cuts the
//! transcript and runs `sve` so only the live chain's compact boundaries
//! survive. `forkPoint` calls `fork.messageId?.trim()`, so a number throws
//! instead of silently forking the whole transcript.

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

fn projects_root(config_dir: &Path) -> PathBuf {
    projects_root_with_env(&EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]))
}

fn seed_session(config_dir: &Path, cwd: &Path, session_id: &str, body: &str) -> PathBuf {
    let dir = projects_root(config_dir).join(project_slug(&cwd.to_string_lossy()));
    std::fs::create_dir_all(&dir).expect("project dir");
    let path = dir.join(format!("{session_id}.jsonl"));
    std::fs::write(&path, body).expect("seed transcript");
    path
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

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("fork file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect()
}

/// A transcript with a rewind branch off `a1` carrying a superseded compact
/// boundary, and the live continuation `u4/a4` after it.
fn rewind_body() -> String {
    let entry = |kind: &str, uuid: &str, parent: &str, text: &str| {
        json!({"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": SOURCE,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {"role": kind, "content": [{"type": "text", "text": text}]}})
    };
    [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"one"}}),
        entry("assistant", "a1", "u1", "first"),
        json!({"type":"user","uuid":"u2","parentUuid":"a1","sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"two"}}),
        entry("assistant", "a2", "u2", "second"),
        json!({"type":"user","uuid":"u3","parentUuid":"a1","sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"rewound"}}),
        entry("assistant", "a3", "u3", "rewritten"),
        json!({"type":"system","subtype":"compact_boundary","uuid":"cb1",
            "parentUuid":"a3","sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:04.000Z",
            "compactMetadata":{"preservedMessages":{"anchorUuid":"gone",
                "uuids":["gone-1"]}}}),
        json!({"type":"user","uuid":"u4","parentUuid":"a2","sessionId":SOURCE,
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"three"}}),
        entry("assistant", "a4", "u4", "third"),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

/// M-33 — the fork cut follows the live chain: the rewind branch and its
/// superseded compact boundary are not copied.
#[tokio::test]
async fn m33_fork_point_cut_follows_the_live_chain() {
    let tmp = std::env::temp_dir().join(format!("t5fb-fork-cut-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    seed_session(&config_dir, &cwd, SOURCE, &rewind_body());
    let transcript = init_transcript(&tmp);
    let argv_out = tmp.join("argv.json");
    let opts = serve_options(&transcript, &config_dir, &argv_out, &cwd);

    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let params = json!({
        "sessionId": SOURCE,
        "cwd": cwd.to_string_lossy(),
        "mcpServers": [],
        "_meta": {"jetbrains": {"air": {"fork": {"version": 1, "messageId": "a4"}}}},
    });
    let response = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new("session/fork", params)?)
                    .block_task()
                    .await
            },
        )
        .await
        .expect("client run");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let fork_id = response["sessionId"].as_str().expect("fork id").to_string();
    let path = projects_root(&config_dir)
        .join(project_slug(&cwd.to_string_lossy()))
        .join(format!("{fork_id}.jsonl"));
    let lines = read_lines(&path);
    let forked: Vec<&str> = lines
        .iter()
        .filter_map(|line| line["forkedFrom"]["messageUuid"].as_str())
        .collect();
    assert_eq!(forked, ["u1", "a1", "u2", "a2", "u4", "a4"], "{lines:?}");
    assert_eq!(lines.len(), 7, "{lines:?}");
    assert!(!lines.iter().any(|line| line["type"] == json!("system")));
    assert_eq!(lines[6]["type"], json!("custom-title"));
    wait_for_file(&argv_out, Duration::from_secs(10)).expect("argv recorded");
    let argv = read_json(&argv_out)["argv"].clone();
    assert!(argv.as_array().is_some(), "the fork starts a CLI");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// INFO (T5B review) — the unknown-fork-point error carries upstream's text
/// ("Fork point message <id> was not found in session <id>") and `{messageId}`.
#[tokio::test]
async fn m33_unknown_fork_point_answers_the_upstream_message() {
    let tmp = std::env::temp_dir().join(format!("t5fb-fork-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    seed_session(&config_dir, &cwd, SOURCE, &rewind_body());
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
                connection
                    .send_request(UntypedMessage::new("session/fork", params)?)
                    .block_task()
                    .await
            },
        )
        .await;
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let err = result.expect_err("an unknown fork point must reject");
    assert_eq!(error_code(&err), -32602, "{err:?}");
    assert_eq!(err.data, Some(json!({"messageId": "nope"})));
    assert_eq!(
        err.message,
        format!("Invalid params: Fork point message nope was not found in session {SOURCE}")
    );
    assert!(!argv_out.exists(), "no CLI spawn for an unresolved point");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// LOW-5 — a non-string `messageId` is the SDK's TypeError: an internal error
/// (-32603), not a silent full fork, with no CLI spawn and no fork file.
#[tokio::test]
async fn low5_non_string_fork_point_is_an_internal_error() {
    let tmp = std::env::temp_dir().join(format!("t5fb-fork-bad-point-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let cwd = repo_root();
    let seeded = seed_session(&config_dir, &cwd, SOURCE, &rewind_body());
    let transcript = init_transcript(&tmp);
    let argv_out = tmp.join("argv.json");
    let opts = serve_options(&transcript, &config_dir, &argv_out, &cwd);

    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let params = json!({
        "sessionId": SOURCE,
        "cwd": cwd.to_string_lossy(),
        "mcpServers": [],
        "_meta": {"jetbrains": {"air": {"fork": {"version": 1, "messageId": 7}}}},
    });
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new("session/fork", params)?)
                    .block_task()
                    .await
            },
        )
        .await;
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let err = result.expect_err("a malformed fork point must reject");
    assert_eq!(error_code(&err), -32603, "{err:?}");
    let details = serde_json::to_value(&err)
        .ok()
        .and_then(|v| v.get("data").cloned())
        .and_then(|data| data.get("details").cloned())
        .and_then(|details| details.as_str().map(String::from))
        .unwrap_or_default();
    assert!(
        details.contains("messageId") && details.contains("trim"),
        "the SDK's TypeError text: {details}"
    );
    assert!(!argv_out.exists(), "no CLI spawn for a malformed point");
    let files: Vec<String> = std::fs::read_dir(seeded.parent().expect("project dir"))
        .expect("read project dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        files,
        vec![format!("{SOURCE}.jsonl")],
        "no fork transcript is written"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
