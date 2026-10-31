//! Agent-level tests for the Turn 1 fix round 2 (lane A, T1GA finding 4): the
//! spawned recreate of a live session on a changed-params `session/load` does not
//! escape the connection's bookkeeping. Driven over `Channel::duplex()` with a
//! real `Client`, every wait bounded.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, serve_with_shutdown, ServeOptions};
use common::{fake_claude_binary, repo_root, wait_pid_gone};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::time::timeout;

/// A transcript with only an `initialize` step (every spawn replays it).
/// The child answers `initialize` after `delay_ms`, which keeps a session start
/// in flight long enough to act on it.
fn init_only_transcript(tmp: &Path, delay_ms: u64) -> PathBuf {
    let transcript = tmp.join("init-only.transcript.jsonl");
    std::fs::write(
        &transcript,
        format!("{{\"expect\":{{\"type\":\"control_request\",\"request\":{{\"subtype\":\"initialize\"}}}},\"emit\":[{{\"sleep_ms\":{delay_ms}}},{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{{\"models\":[{{\"value\":\"claude-sonnet-4-5\"}}],\"commands\":[],\"account\":{{}}}}}}}}]}}\n"),
    )
    .expect("write transcript");
    transcript
}

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t1ga-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

/// Serve options whose children replay the init-only transcript and record their
/// pid (the last spawn wins) in `pid_file`.
fn options(tmp: &Path, pid_file: &Path, init_delay_ms: u64) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                init_only_transcript(tmp, init_delay_ms)
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "FAKE_CLAUDE_PID_OUT".to_string(),
                pid_file.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        ..Default::default()
    }
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

/// Finding 4(b): a `session/close` while the session is out of the registry for
/// its recreate cancels the recreate. The load answers an error, the close answers
/// `{}`, the id stays unregistered and the half-started child is gone.
#[tokio::test]
async fn t1ga_close_during_a_recreate_cancels_it() {
    let tmp = temp_dir("close");
    let pid_file = tmp.join("pid");
    let opts = options(&tmp, &pid_file, 300);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let cwd = repo_root().to_string_lossy().into_owned();
    let result = timeout(
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
                    let load = connection.send_request(UntypedMessage::new(
                        "session/load",
                        load_params(&session_id, &cwd),
                    )?);
                    let close = connection.send_request(UntypedMessage::new(
                        "session/close",
                        json!({"sessionId": session_id}),
                    )?);
                    let (load, close) = tokio::join!(load.block_task(), close.block_task());
                    assert!(load.is_err(), "the cancelled recreate answers an error");
                    assert_eq!(close?, json!({}), "the close answers");
                    // Nothing is registered under the id afterwards.
                    let set_mode = connection
                        .send_request(UntypedMessage::new(
                            "session/set_mode",
                            json!({"sessionId": session_id, "modeId": "plan"}),
                        )?)
                        .block_task()
                        .await;
                    assert!(set_mode.is_err(), "the closed session is not registered");
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await;
    assert!(result.is_ok(), "the exchange finished in time");
    result.unwrap().expect("the exchange succeeded");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let pid = read_pid(&pid_file).expect("a child recorded its pid");
    assert!(
        wait_pid_gone(pid, Duration::from_secs(10)),
        "no child of the cancelled recreate is left running"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Finding 4(c): a second `session/load` of the id while its recreate runs is
/// refused instead of starting a second child on the same transcript; the first
/// recreate completes and the session is registered exactly once.
#[tokio::test]
async fn t1ga_second_load_during_a_recreate_is_refused() {
    let tmp = temp_dir("second");
    let pid_file = tmp.join("pid");
    let opts = options(&tmp, &pid_file, 300);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let cwd = repo_root().to_string_lossy().into_owned();
    let result = timeout(
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
                    let first = connection.send_request(UntypedMessage::new(
                        "session/load",
                        load_params(&session_id, &cwd),
                    )?);
                    let second = connection.send_request(UntypedMessage::new(
                        "session/load",
                        load_params(&session_id, &cwd),
                    )?);
                    let (first, second) = tokio::join!(first.block_task(), second.block_task());
                    assert_eq!(
                        first?["sessionId"].as_str().unwrap_or(""),
                        session_id,
                        "the first load answers with the recreated session"
                    );
                    assert!(second.is_err(), "the second load is refused");
                    // Registered: a load with the same params reuses it.
                    let again = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            load_params(&session_id, &cwd),
                        )?)
                        .block_task()
                        .await?;
                    assert_eq!(again["sessionId"].as_str().unwrap_or(""), session_id);
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await;
    assert!(result.is_ok(), "the exchange finished in time");
    result.unwrap().expect("the exchange succeeded");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Finding 4(a): the connection ends while the recreate is starting the new
/// session. The serve loop stops the recreate before it disposes the sessions, so
/// the new `claude` child does not outlive the connection.
#[tokio::test]
async fn t1ga_connection_end_during_a_recreate_leaves_no_child() {
    let tmp = temp_dir("end");
    let pid_file = tmp.join("pid");
    let opts = options(&tmp, &pid_file, 1500);
    let (agent_channel, client_channel) = Channel::duplex();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let serve_task =
        tokio::spawn(async move { serve_with_shutdown(agent_channel, opts, shutdown_rx).await });
    let cwd = repo_root().to_string_lossy().into_owned();
    let pid_for_client = pid_file.clone();
    let result = timeout(
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
                    // Wait until the recreate spawned its new child, then end the
                    // connection while that child is still starting.
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                    while read_pid(&pid_for_client).is_none_or(|pid| pid == first_pid) {
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "the recreate spawned a new child"
                        );
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    let _ = shutdown_tx.send(true);
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await;
    assert!(result.is_ok(), "the exchange finished in time");
    let _ = result.unwrap();
    assert!(
        timeout(Duration::from_secs(30), serve_task).await.is_ok(),
        "the serve loop ended"
    );
    let pid = read_pid(&pid_file).expect("the new child recorded its pid");
    assert!(
        wait_pid_gone(pid, Duration::from_secs(15)),
        "the child of the recreate does not outlive the connection"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
