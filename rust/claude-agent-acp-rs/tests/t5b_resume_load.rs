//! T5B lane B — resume/load of an unknown session (M-31) through the running
//! agent with the fake CLI harness.
//!
//! Upstream `tests/session-load.test.ts:68-100`: `loadSession`/`resumeSession`
//! of a session the CLI cannot find reject with `RequestError.resourceNotFound`
//! (-32002). The SDK surfaces the CLI's "No conversation found with session ID"
//! initialization error; the fake transcript reproduces that CLI answer.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::{fake_claude_binary, repo_root};
use serde_json::json;
use tokio::time::timeout;

/// A transcript whose initialize handshake answers the CLI's not-found error.
fn missing_conversation_transcript(tmp: &std::path::Path, session_id: &str) -> PathBuf {
    let transcript = tmp.join("missing-conversation.transcript.jsonl");
    let body = format!(
        "{{\"expect\":{{\"type\":\"control_request\",\"request\":{{\"subtype\":\"initialize\"}}}},\"emit\":[{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"error\",\"request_id\":\"$REQ\",\"error\":\"No conversation found with session ID: {session_id}\"}}}}]}}\n"
    );
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

/// M-31 — `session/load` of an unknown session is `resourceNotFound` (-32002)
/// with `data:{uri}`, never an internal error.
#[tokio::test]
async fn m31_load_unknown_session_is_resource_not_found() {
    let tmp = std::env::temp_dir().join(format!("t5b-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let session_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let transcript = missing_conversation_transcript(&tmp, session_id);
    let err = request_error("session/load", session_id, transcript).await;
    assert_eq!(error_code(&err), -32002, "load must be resourceNotFound");
    assert_eq!(err.message, format!("Resource not found: {session_id}"));
    assert_eq!(err.data, Some(json!({"uri": session_id})));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-31 — the same for `session/resume`.
#[tokio::test]
async fn m31_resume_unknown_session_is_resource_not_found() {
    let tmp = std::env::temp_dir().join(format!("t5b-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let session_id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let transcript = missing_conversation_transcript(&tmp, session_id);
    let err = request_error("session/resume", session_id, transcript).await;
    assert_eq!(error_code(&err), -32002, "resume must be resourceNotFound");
    assert_eq!(err.message, format!("Resource not found: {session_id}"));
    assert_eq!(err.data, Some(json!({"uri": session_id})));
    let _ = std::fs::remove_dir_all(&tmp);
}
