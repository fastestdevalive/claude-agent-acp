//! CONC-2 scenario 7, process side: the child exits mid-turn at random points
//! of its script, and no process is left behind.
//!
//! The in-process race suite (`src/session/tests_t7a_race_s7.rs`) covers the
//! frame-level settlement; this test drives the real agent against `fake-claude`
//! and checks the child's pid is gone after every run (the pid file the fake
//! CLI writes under `FAKE_CLAUDE_PID_OUT`).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::{fake_claude_binary, pid_is_gone, wait_pid_gone};
use serde_json::json;

/// A transcript that initializes, takes a prompt, emits `deltas` chunks and
/// exits without a result (`exit_code` `None`: a clean exit; `Some(code)`: an
/// abnormal exit).
fn death_transcript(dir: &Path, deltas: usize, exit_code: Option<i32>) -> PathBuf {
    let path = dir.join("death.transcript.jsonl");
    let mut file = std::fs::File::create(&path).expect("create transcript");
    use std::io::Write;
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "$REQ",
                "response": {"models": [{"value": "claude-sonnet-4-5"}],
                    "commands": [], "account": {}}}
        }]
    });
    writeln!(file, "{}", serde_json::to_string(&init).unwrap()).unwrap();
    let mut emit = vec![json!({
        "type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
        "message": {"role": "user", "content": [{"type": "text", "text": "do work"}]}
    })];
    for i in 0..deltas {
        emit.push(json!({
            "type": "stream_event",
            "event": {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": format!("chunk{i} ")}}
        }));
    }
    let step = json!({"expect": {"type": "user"}, "exit": true,
        "exit_code": exit_code, "emit": emit});
    writeln!(file, "{}", serde_json::to_string(&step).unwrap()).unwrap();
    path
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // A monotonic counter too: two tests in this binary can start in the same
        // clock tick (the clean and abnormal variants use the same tag sequence),
        // and sharing a temp dir would let one overwrite the other's transcript
        // mid-run.
        let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acp-conc-race-exit-{tag}-{}-{unique}-{ordinal}",
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

fn iterations() -> usize {
    std::env::var("CONC_ITERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10)
        .clamp(1, 25)
}

async fn one_run(deltas: usize, exit_code: Option<i32>) -> Result<(), String> {
    let tmp = TempDir::new(&format!("d{deltas}"));
    let cwd = tmp.path().join("workspace");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let transcript = death_transcript(tmp.path(), deltas, exit_code);
    let pid_out = tmp.path().join("child.pid");
    let argv_out = tmp.path().join("argv.json");
    let opts = ServeOptions {
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
                "FAKE_CLAUDE_PID_OUT".to_string(),
                pid_out.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_str = cwd.to_string_lossy().into_owned();
    let client_task = Client.builder().connect_with(
        client_channel,
        move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
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
            let res = tokio::time::timeout(Duration::from_secs(5), prompt_fut)
                .await
                .expect("the prompt must not hang when the child exits mid-turn")
                .expect("join handle ok");
            match exit_code {
                // A clean exit settles the in-flight turn `end_turn`.
                None => {
                    let response = res.expect("a clean child exit settles the in-flight turn");
                    assert_eq!(
                        response["stopReason"].as_str(),
                        Some("end_turn"),
                        "clean child exit mid-turn must settle end_turn: {response:?}"
                    );
                }
                // A non-zero exit fails the turn: the agent answers with its
                // process-died error instead of hanging or claiming success.
                Some(code) => {
                    let error =
                        res.expect_err("an abnormal child exit must fail the in-flight turn");
                    let value = serde_json::to_value(&error).unwrap_or_default();
                    assert_eq!(
                        value["code"], -32603,
                        "an abnormal exit (code {code}) answers internalError: {value}"
                    );
                    assert!(
                        value["message"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("process exited unexpectedly"),
                        "the process-died error text is surfaced: {value}"
                    );
                }
            }
            Ok::<(), agent_client_protocol::Error>(())
        },
    );
    let client_res = tokio::time::timeout(Duration::from_secs(10), client_task).await;
    match client_res {
        Ok(Ok(())) => {}
        other => return Err(format!("the client closure failed: {other:?}")),
    }

    // The child's pid was published before replay; it must be gone BEFORE the
    // serve task is aborted. Aborting drops the session (and its kill-on-drop
    // process), which would kill the child itself and hide a leak in the exit
    // path (P2a F6).
    let pid = std::fs::read_to_string(&pid_out)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    let Some(pid) = pid else {
        return Err("the fake CLI did not publish its pid".into());
    };
    let gone = wait_pid_gone(pid, Duration::from_secs(5)) || pid_is_gone(pid);
    serve_task.abort();
    let _ = serve_task.await;
    if !gone {
        return Err(format!("the child process {pid} outlived the session"));
    }
    Ok(())
}

#[tokio::test]
async fn conc_race_child_exit_mid_turn_leaves_no_process() {
    let n = iterations();
    for i in 0..n {
        // A different script length per iteration: the exit lands at a random
        // point of the turn.
        let deltas = (i * 7) % 40;
        tokio::time::timeout(Duration::from_secs(20), one_run(deltas, None))
            .await
            .unwrap_or_else(|_| Err(format!("iteration {i} timed out")))
            .unwrap_or_else(|error| panic!("iteration {i} (deltas {deltas}) failed: {error}"));
    }
}

/// P2a F6: the abnormal-exit lane at the process level. A child that dies with a
/// non-zero status mid-turn must fail the in-flight turn with the process-died
/// error (not settle `end_turn`, not hang), and must be reaped before the
/// connection is torn down.
#[tokio::test]
async fn conc_race_child_abnormal_exit_mid_turn_fails_the_turn() {
    let n = iterations();
    for i in 0..n {
        let deltas = (i * 7) % 40;
        tokio::time::timeout(Duration::from_secs(20), one_run(deltas, Some(1)))
            .await
            .unwrap_or_else(|_| Err(format!("iteration {i} timed out")))
            .unwrap_or_else(|error| panic!("iteration {i} (deltas {deltas}) failed: {error}"));
    }
}

/// P2a F3/F6: the panic/exit policy at the agent level. One session's child dies
/// abnormally mid-turn; its in-flight prompt must get the process-died JSON-RPC
/// error (-32603), and a SIBLING session must keep answering (a dead session
/// actor never takes the connection down). Deleting the agent's prompt error
/// response makes this hang, so it is not vacuous.
fn two_session_transcript(dir: &Path) -> PathBuf {
    let path = dir.join("sibling.transcript.jsonl");
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "$REQ",
                "response": {"models": [{"value": "claude-sonnet-4-5"}],
                    "commands": [], "account": {}}}
        }]
    });
    let death = json!({
        "expect": {"type": "user"},
        "exit": true,
        "exit_code": 1,
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "die"}]}},
            {"type": "stream_event", "event": {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "working "}}}
        ]
    });
    let answer = json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "hello"}]}},
            {"type": "assistant", "parent_tool_use_id": null,
             "message": {"id": "msg_sibling", "role": "assistant",
                "model": "claude-sonnet-4-5",
                "usage": {"input_tokens": 1, "output_tokens": 1,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                "content": [{"type": "text", "text": "still here"}]}},
            {"type": "result", "subtype": "success", "stop_reason": "end_turn",
             "is_error": false, "result": "still here",
             "usage": {"input_tokens": 1, "output_tokens": 1,
                 "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
             "modelUsage": {"claude-sonnet-4-5": {"contextWindow": 200000}}}
        ]
    });
    let mut text = String::new();
    text.push_str(&serde_json::to_string(&init).unwrap());
    text.push('\n');
    text.push_str(&serde_json::to_string(&death).unwrap());
    text.push('\n');
    text.push_str("{\"segment\":1}\n");
    text.push_str(&serde_json::to_string(&init).unwrap());
    text.push('\n');
    text.push_str(&serde_json::to_string(&answer).unwrap());
    text.push('\n');
    std::fs::write(&path, text).expect("write transcript");
    path
}

#[tokio::test]
async fn conc_race_a_dead_session_leaves_a_sibling_answering() {
    let tmp = TempDir::new("sibling");
    let cwd = tmp.path().join("workspace");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let transcript = two_session_transcript(tmp.path());
    let cursor = tmp.path().join("cursor");
    let pid_out = tmp.path().join("child.pid");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_CURSOR".to_string(),
                cursor.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_PID_OUT".to_string(),
                pid_out.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_str = cwd.to_string_lossy().into_owned();
    let client_task = Client.builder().connect_with(
        client_channel,
        move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
            let a = connection
                .send_request(UntypedMessage::new(
                    "session/new",
                    json!({ "cwd": cwd_str.clone() }),
                )?)
                .block_task()
                .await?["sessionId"]
                .as_str()
                .expect("a session id")
                .to_string();
            let b = connection
                .send_request(UntypedMessage::new(
                    "session/new",
                    json!({ "cwd": cwd_str.clone() }),
                )?)
                .block_task()
                .await?["sessionId"]
                .as_str()
                .expect("b session id")
                .to_string();

            let dying = connection
                .send_request(UntypedMessage::new(
                    "session/prompt",
                    json!({"sessionId": a, "prompt": [{"type": "text", "text": "die"}]}),
                )?)
                .block_task();
            let sibling = connection
                .send_request(UntypedMessage::new(
                    "session/prompt",
                    json!({"sessionId": b, "prompt": [{"type": "text", "text": "hello"}]}),
                )?)
                .block_task();
            let (dying, sibling) = tokio::join!(dying, sibling);
            let dying = dying.expect_err("the dead session's prompt must answer an error");
            let value = serde_json::to_value(&dying).unwrap_or_default();
            assert_eq!(value["code"], -32603, "{value}");
            assert!(
                value["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("process exited unexpectedly"),
                "{value}"
            );
            let sibling = sibling.expect("the sibling session keeps answering");
            assert_eq!(
                sibling["stopReason"].as_str(),
                Some("end_turn"),
                "{sibling:?}"
            );
            Ok::<(), agent_client_protocol::Error>(())
        },
    );
    let client_res = tokio::time::timeout(Duration::from_secs(20), client_task).await;
    serve_task.abort();
    let _ = serve_task.await;
    match client_res {
        Ok(Ok(())) => Ok(()),
        other => Err(format!("the client closure failed: {other:?}")),
    }
    .unwrap_or_else(|error| panic!("{error}"));
}
