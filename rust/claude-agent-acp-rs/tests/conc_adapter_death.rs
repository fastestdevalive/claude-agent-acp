//! Concurrency test for adapter child process termination mid-turn (CONC-2).
//!
//! Asserts that when the child process exits mid-turn without emitting a prompt
//! `result`, the pending prompt request settles with the exact shape the port
//! gives a clean child exit: the in-flight turn resolves `end_turn`
//! (`acp-agent.ts:5764-5800`), with the streamed delta already forwarded.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::fake_claude_binary;
use serde_json::json;

/// A transcript where fake-claude initializes, accepts a prompt, emits partial
/// output, and immediately terminates (exits) without emitting a prompt
/// `result`. The exit is clean (status 0), so the relay reports `abnormal:false`
/// and the in-flight turn settles `end_turn` instead of being rejected.
fn make_death_mid_turn_transcript(dir: &Path) -> PathBuf {
    let transcript_path = dir.join("death-mid-turn.transcript.jsonl");
    let mut file = std::fs::File::create(&transcript_path).expect("create transcript");
    use std::io::Write;

    // Step 1: initialize
    let init_step = json!({
        "expect": {
            "type": "control_request",
            "request": {"subtype": "initialize"}
        },
        "emit": [{
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "$REQ",
                "response": {
                    "models": [{"value": "claude-sonnet-4-5"}],
                    "commands": [],
                    "account": {}
                }
            }
        }]
    });
    writeln!(file, "{}", serde_json::to_string(&init_step).unwrap()).unwrap();

    // Step 2: user prompt -> emits one stream delta then exits (no result!)
    let prompt_step = json!({
        "expect": {"type": "user"},
        "exit": true,
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "do work"}]
                }
            },
            {
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "text_delta",
                        "text": "Starting some work before dying abruptly..."
                    }
                }
            }
        ]
    });
    writeln!(file, "{}", serde_json::to_string(&prompt_step).unwrap()).unwrap();

    transcript_path
}

fn fake_opts_for_transcript(transcript: &Path, argv_out: &Path, cwd: &Path) -> ServeOptions {
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
        ],
        default_cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    }
}

/// A unique temp dir, removed on drop even when the test panics.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "acp-conc-death-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const DELTA_TEXT: &str = "Starting some work before dying abruptly...";

#[tokio::test]
async fn adapter_death_mid_turn_yields_error() {
    let tmp = TempDir::new("mid-turn");

    let cwd = tmp.path().join("workspace");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let transcript = make_death_mid_turn_transcript(tmp.path());
    let argv_out = tmp.path().join("argv.json");

    let opts = fake_opts_for_transcript(&transcript, &argv_out, &cwd);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    // The delta notification proves the turn was mid-stream when the child died.
    let delta_seen = Arc::new(AtomicBool::new(false));
    let delta_seen_in = delta_seen.clone();

    let cwd_str = cwd.to_string_lossy().into_owned();
    let client_task = Client
        .builder()
        .on_receive_notification(
            async move |notification: UntypedMessage, _cx| {
                if notification.method() == "session/update" {
                    let text = serde_json::to_string(notification.params()).unwrap_or_default();
                    if text.contains(DELTA_TEXT) {
                        delta_seen_in.store(true, Ordering::SeqCst);
                    }
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                // 1. Create session
                let init_resp = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({ "cwd": cwd_str }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = init_resp["sessionId"]
                    .as_str()
                    .expect("sessionId string")
                    .to_string();

                // 2. Submit prompt
                let prompt_conn = connection.clone();
                let prompt_session_id = session_id.clone();
                let prompt_fut = tokio::spawn(async move {
                    prompt_conn
                        .send_request(UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": prompt_session_id,
                                "prompt": [{"type": "text", "text": "do work"}]
                            }),
                        )?)
                        .block_task()
                        .await
                });

                // 3. The prompt settles within the bounded deadline.
                let res = tokio::time::timeout(Duration::from_secs(5), prompt_fut)
                    .await
                    .expect("prompt must not hang when the child process dies mid-turn")
                    .expect("join handle ok");

                // 4. Exact shape for a clean child exit mid-turn: the in-flight
                //    turn resolves `end_turn` (`acp-agent.ts:5764-5800`), it is not
                //    a rejection.
                let response = res.expect("a clean child exit settles the in-flight turn");
                assert_eq!(
                    response["stopReason"].as_str(),
                    Some("end_turn"),
                    "clean child exit mid-turn must settle end_turn, got: {response:?}"
                );
                assert!(
                    response.get("error").is_none() && response.get("isError").is_none(),
                    "a clean exit is not an error response: {response:?}"
                );

                Ok::<(), agent_client_protocol::Error>(())
            },
        );

    let client_res = tokio::time::timeout(Duration::from_secs(10), client_task).await;

    // The client closure's own result must be checked: a `session/new` failure
    // (or any `?`) inside it must fail the test instead of being swallowed.
    assert!(
        matches!(client_res, Ok(Ok(()))),
        "client closure completed successfully: {client_res:?}"
    );
    assert!(
        delta_seen.load(Ordering::SeqCst),
        "the streamed delta arrived before the child died mid-turn"
    );

    serve_task.abort();
    let _ = serve_task.await;
}
