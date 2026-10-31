//! T5FB lane FB — resume/load of an unknown session whose CLI exits before it
//! answers `initialize` (MEDIUM-1 of the T5B review).
//!
//! Upstream `acp-agent.ts:11421-11430`: the SDK raises "Query closed before
//! response received" when the child closes before the initialize response
//! arrives, and a resume maps it to `RequestError.resourceNotFound` (-32002).
//! The Rust handshake surfaces the same condition as `ControlError::Closed`
//! ("control channel closed"); without the mapping the client gets -32603.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::{fake_claude_binary, repo_root};
use serde_json::json;
use tokio::time::timeout;

/// A fake CLI that expects the initialize request and then exits without
/// answering it, like `claude --resume <unknown>` printing to stderr and dying.
fn exit_without_answering_transcript(tmp: &std::path::Path) -> PathBuf {
    let transcript = tmp.join("exit-before-initialize.transcript.jsonl");
    let body = "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[],\"exit\":true}\n";
    std::fs::write(&transcript, body).expect("write transcript");
    transcript
}

fn fake_opts(transcript: &std::path::Path) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(repo_root()),
        ..Default::default()
    }
}

fn error_code(err: &agent_client_protocol::Error) -> i64 {
    serde_json::to_value(err)
        .ok()
        .and_then(|v| v.get("code").cloned())
        .and_then(|c| c.as_i64())
        .unwrap_or(i64::MAX)
}

async fn request_error(
    method: &'static str,
    session_id: &str,
    transcript: PathBuf,
) -> agent_client_protocol::Error {
    let opts = fake_opts(&transcript);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let root = repo_root();
    let id = session_id.to_string();
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let root = root.clone();
                let id = id.clone();
                async move {
                    let params = json!({
                        "sessionId": id,
                        "cwd": root.to_string_lossy(),
                        "mcpServers": [],
                    });
                    let response = connection
                        .send_request(UntypedMessage::new(method, params)?)
                        .block_task()
                        .await;
                    Ok::<_, agent_client_protocol::Error>(response.expect_err("must reject"))
                }
            },
        )
        .await
        .expect("client run");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    result
}

/// M-31 — `session/load` of an unknown session whose CLI exits before the
/// initialize response is `resourceNotFound` (-32002), not internal error.
#[tokio::test]
async fn m31_load_unknown_session_with_closed_channel_is_resource_not_found() {
    let tmp = std::env::temp_dir().join(format!("t5fb-load-closed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let session_id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let transcript = exit_without_answering_transcript(&tmp);
    let err = request_error("session/load", session_id, transcript).await;
    assert_eq!(
        error_code(&err),
        -32002,
        "a closed initialize channel on load must be resourceNotFound, got {err:?}"
    );
    assert_eq!(err.message, format!("Resource not found: {session_id}"));
    assert_eq!(err.data, Some(json!({"uri": session_id})));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-31 — the same for `session/resume`.
#[tokio::test]
async fn m31_resume_unknown_session_with_closed_channel_is_resource_not_found() {
    let tmp = std::env::temp_dir().join(format!("t5fb-resume-closed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let session_id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    let transcript = exit_without_answering_transcript(&tmp);
    let err = request_error("session/resume", session_id, transcript).await;
    assert_eq!(
        error_code(&err),
        -32002,
        "a closed initialize channel on resume must be resourceNotFound, got {err:?}"
    );
    assert_eq!(err.message, format!("Resource not found: {session_id}"));
    assert_eq!(err.data, Some(json!({"uri": session_id})));
    let _ = std::fs::remove_dir_all(&tmp);
}
