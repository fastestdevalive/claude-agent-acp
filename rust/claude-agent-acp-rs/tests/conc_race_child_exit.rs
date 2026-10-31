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
/// exits without a result (a clean exit).
fn death_transcript(dir: &Path, deltas: usize) -> PathBuf {
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
    let step = json!({"expect": {"type": "user"}, "exit": true, "emit": emit});
    writeln!(file, "{}", serde_json::to_string(&step).unwrap()).unwrap();
    path
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "acp-conc-race-exit-{tag}-{}-{unique}",
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

async fn one_run(deltas: usize) -> Result<(), String> {
    let tmp = TempDir::new(&format!("d{deltas}"));
    let cwd = tmp.path().join("workspace");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let transcript = death_transcript(tmp.path(), deltas);
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
            let response = res.expect("a clean child exit settles the in-flight turn");
            assert_eq!(
                response["stopReason"].as_str(),
                Some("end_turn"),
                "clean child exit mid-turn must settle end_turn: {response:?}"
            );
            Ok::<(), agent_client_protocol::Error>(())
        },
    );
    let client_res = tokio::time::timeout(Duration::from_secs(10), client_task).await;
    serve_task.abort();
    let _ = serve_task.await;
    match client_res {
        Ok(Ok(())) => {}
        other => return Err(format!("the client closure failed: {other:?}")),
    }

    // The child's pid was published before replay; it must be gone.
    let pid = std::fs::read_to_string(&pid_out)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    let Some(pid) = pid else {
        return Err("the fake CLI did not publish its pid".into());
    };
    if !wait_pid_gone(pid, Duration::from_secs(5)) && !pid_is_gone(pid) {
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
        tokio::time::timeout(Duration::from_secs(20), one_run(deltas))
            .await
            .unwrap_or_else(|_| Err(format!("iteration {i} timed out")))
            .unwrap_or_else(|error| panic!("iteration {i} (deltas {deltas}) failed: {error}"));
    }
}
