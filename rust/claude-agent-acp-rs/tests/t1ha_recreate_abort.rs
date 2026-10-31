//! Agent-level test for the Turn 1 fix round 3 (lane A, T1HA finding 4): a
//! recreate that is still inside its session start when the connection ends is
//! aborted after the dispose bound; the `claude` child it already spawned must
//! not outlive the connection. The CLI's `initialize` takes far longer than the
//! 6 s bound; the clock is paused once the child is spawned, so the test does not
//! wait for it.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve_with_shutdown, ServeOptions};
use common::{fake_claude_binary, pid_is_gone, repo_root};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::time::timeout;

/// Segment 0 (the first spawn) answers `initialize` at once; segment 1 (the
/// recreate's spawn) takes `slow_ms`.
fn two_spawn_transcript(tmp: &Path, slow_ms: u64) -> PathBuf {
    let reply = "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[{\"value\":\"claude-sonnet-4-5\"}],\"commands\":[],\"account\":{}}}}";
    let step = |sleep_ms: u64| {
        format!("{{\"expect\":{{\"type\":\"control_request\",\"request\":{{\"subtype\":\"initialize\"}}}},\"emit\":[{{\"sleep_ms\":{sleep_ms}}},{reply}]}}\n")
    };
    let transcript = tmp.join("two-spawn.transcript.jsonl");
    std::fs::write(
        &transcript,
        format!("{}{{\"segment\":1}}\n{}", step(0), step(slow_ms)),
    )
    .expect("write transcript");
    transcript
}

fn read_pid(pid_file: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

fn load_params(session_id: &str, cwd: &str) -> Value {
    json!({
        "sessionId": session_id,
        "cwd": cwd,
        "mcpServers": [],
        // Different from the session/new params: the live session is recreated.
        "additionalDirectories": ["/tmp"],
    })
}

#[tokio::test(flavor = "current_thread")]
async fn t1ha_aborting_a_recreate_mid_initialize_leaves_no_child() {
    let tmp = std::env::temp_dir().join(format!("acp-t1ha-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let pid_file = tmp.join("pid");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                two_spawn_transcript(&tmp, 60_000)
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "FAKE_CLAUDE_CURSOR".to_string(),
                tmp.join("cursor").to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_PID_OUT".to_string(),
                pid_file.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let serve_task =
        tokio::spawn(async move { serve_with_shutdown(agent_channel, opts, shutdown_rx).await });
    let cwd = repo_root().to_string_lossy().into_owned();
    let pid_for_client = pid_file.clone();
    let second_pid = timeout(
        Duration::from_secs(60),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd = cwd.clone();
                async move {
                    let new = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({"cwd": cwd, "mcpServers": []}),
                        )?)
                        .block_task()
                        .await?;
                    let session_id = new["sessionId"].as_str().unwrap_or("").to_string();
                    let first_pid = read_pid(&pid_for_client).expect("the first child's pid");
                    let _load = connection.send_request(UntypedMessage::new(
                        "session/load",
                        load_params(&session_id, &cwd),
                    )?);
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                    let second_pid = loop {
                        match read_pid(&pid_for_client) {
                            Some(pid) if pid != first_pid => break pid,
                            _ => {}
                        }
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "the recreate spawned a new child"
                        );
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    };
                    // The new child is stuck in `initialize`. End the connection
                    // with the clock paused: the 6 s the serve loop waits for the
                    // recreate pass at once and the task is aborted mid-start.
                    tokio::time::pause();
                    let _ = shutdown_tx.send(true);
                    Ok::<u32, agent_client_protocol::Error>(second_pid)
                }
            },
        ),
    )
    .await
    .expect("the exchange finished in time")
    .expect("the exchange succeeded");
    assert!(
        timeout(Duration::from_secs(120), serve_task).await.is_ok(),
        "the serve loop ended"
    );
    tokio::time::resume();
    // The aborted start's guard closes the actor, which drops its child.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !pid_is_gone(second_pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child of the aborted recreate outlives the connection"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
