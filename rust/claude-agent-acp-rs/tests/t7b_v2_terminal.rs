//! T7B STEP 5 — the v2 display terminal of a command in the background
//! (T-104, T-102, U-104, Q-89; upstream `src/tests/acp-v2.test.ts:4681-4830`).
//!
//! A command's terminal is announced on its Bash report, keeps running after a
//! backgrounded call's result, and ends when the command's task ends (or at the
//! result when the task already ended).
//!
//! Raw wire harness over a duplex transport, local `fake-claude`, bounded waits.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t7b-terminal-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

fn init_step() -> Value {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {"models": [], "account": {}, "commands": []}
        }}]
    })
}

fn write_transcript(tmp: &Path, name: &str, steps: &[Value]) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    let text = steps
        .iter()
        .map(|step| step.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, text).expect("write transcript");
    path
}

fn bash_use() -> Value {
    json!({"type": "assistant", "parent_tool_use_id": null, "message": {
        "id": "msg_bg", "role": "assistant", "model": "claude-sonnet-4-5",
        "content": [{"type": "tool_use", "id": "toolu_bg", "name": "Bash",
            "input": {"command": "make test", "run_in_background": true}}]}})
}

fn task_started() -> Value {
    json!({"type": "system", "subtype": "task_started", "task_id": "bg1",
        "task_type": "local_bash", "description": "make test",
        "tool_use_id": "toolu_bg", "is_backgrounded": true})
}

fn backgrounded() -> Value {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_bg",
         "content": "Command running in background with ID: bg1."}]},
        "tool_use_result": {"stdout": "", "stderr": "", "interrupted": false,
            "isImage": false, "backgroundTaskId": "bg1"}})
}

fn task_ended() -> Vec<Value> {
    vec![
        json!({"type": "system", "subtype": "task_updated", "task_id": "bg1",
            "patch": {"status": "failed"}}),
        json!({"type": "system", "subtype": "task_notification", "task_id": "bg1",
            "tool_use_id": "toolu_bg", "status": "failed",
            "output_file": "/tmp/tasks/bg1.output",
            "summary": "Background command \"make test\" failed with exit code 3"}),
    ]
}

fn result() -> Value {
    json!({"type": "result", "subtype": "success", "stop_reason": "end_turn",
        "is_error": false, "result": "done",
        "usage": {"input_tokens": 1, "output_tokens": 1}})
}

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str, steps: &[Value]) -> Self {
        let tmp = temp_dir(name);
        let config_dir = tmp.join("claude");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![
                (
                    "FAKE_CLAUDE_SCRIPT".to_string(),
                    write_transcript(&tmp, name, steps)
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    config_dir.to_string_lossy().into_owned(),
                ),
            ],
            default_cwd: Some(repo_root()),
            env: EnvSnapshot::from_pairs([
                (EXPERIMENTAL_V2_ENV.to_string(), "1".to_string()),
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    config_dir.to_string_lossy().into_owned(),
                ),
            ]),
            ..Default::default()
        };
        let transport = WireTransport::new(agent_in, agent_out);
        let task = tokio::spawn(async move { serve(transport, opts).await });
        Self {
            input: client_in,
            reader: BufReader::new(client_out),
            task,
            tmp,
        }
    }

    async fn send_json(&mut self, value: Value) {
        let mut line = serde_json::to_string(&value).expect("serialize");
        line.push('\n');
        self.input
            .write_all(line.as_bytes())
            .await
            .expect("write line");
        self.input.flush().await.expect("flush");
    }

    async fn read_json(&mut self) -> Value {
        let mut line = String::new();
        let n = timeout(Duration::from_secs(20), self.reader.read_line(&mut line))
            .await
            .expect("agent output timed out")
            .expect("read agent line");
        assert!(n > 0, "agent closed stdout");
        serde_json::from_str(&line).expect("agent line is JSON")
    }

    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("method").is_none()
            {
                return frame;
            }
        }
    }

    async fn open_session(&mut self) -> Value {
        self.open_session_with(json!({"auth":{"terminal":{}}}))
            .await
    }

    async fn open_session_with(&mut self, capabilities: Value) -> Value {
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":2,
                "info":{"name":"v2-test-client","version":"1.0.0"},
                "capabilities":capabilities}}))
            .await;
        let init = self.read_response(1).await;
        assert_eq!(init["result"]["protocolVersion"], 2, "{init}");
        self.send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        let created = self.read_response(2).await;
        assert!(
            created.get("error").is_none(),
            "session/new failed: {created}"
        );
        created["result"]["sessionId"].clone()
    }

    /// Read updates until `idles` root `idle` states were seen.
    async fn read_until_idles(&mut self, session_id: &str, idles: usize) -> Vec<Value> {
        let mut updates = Vec::new();
        let mut seen = 0;
        loop {
            let frame = self.read_json().await;
            if frame.get("method").is_none() {
                // A prompt answer; the updates around it are what we collect.
                continue;
            }
            assert_eq!(
                frame.get("method").and_then(Value::as_str),
                Some("session/update"),
                "only updates until the turns are idle: {frame}"
            );
            let on = frame["params"]["sessionId"].as_str().unwrap_or("");
            let update = frame["params"]["update"].clone();
            if on == session_id
                && update["sessionUpdate"] == "state_update"
                && update["state"] == "idle"
            {
                seen += 1;
            }
            updates.push(update);
            if seen == idles {
                return updates;
            }
        }
    }

    async fn shutdown(mut self) {
        let _ = self.input.shutdown().await;
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// T-104 — a background command's terminal is announced on the Bash report,
/// keeps running after the call's result, and ends when the task ends (after
/// the first turn's idle).
#[tokio::test]
async fn t104_v2_background_terminal_keeps_running_until_its_task_ends() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                task_started(),
                bash_use(),
                backgrounded(),
                result(),
                task_ended()[0].clone(),
                task_ended()[1].clone(),
            ],
        }),
        json!({
            "expect": {"type": "user"},
            "emit": [
                {"type": "assistant", "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "The tests failed."}]}},
                result(),
            ],
        }),
    ];
    let mut harness = Harness::start("keeps-running", &steps);
    let root = harness.open_session().await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":root,"prompt":[{"type":"text","text":"hi"}]}}))
        .await;
    let mut updates = harness
        .read_until_idles(root.as_str().unwrap_or(""), 1)
        .await;
    // The command's task ends after the turn: keep reading until its exit.
    loop {
        let frame = match timeout(Duration::from_secs(3), harness.read_json()).await {
            Ok(frame) => frame,
            Err(_) => break,
        };
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let update = frame["params"]["update"].clone();
        let exit =
            update["sessionUpdate"] == "terminal_update" && update.get("exitStatus").is_some();
        updates.push(update);
        if exit {
            break;
        }
    }

    let terminal: Vec<&Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "terminal_update")
        .collect();
    assert_eq!(
        terminal.len(),
        3,
        "announcement, output and exit: {updates:?}"
    );
    assert_eq!(terminal[0]["command"], "make test", "{:?}", terminal[0]);
    assert_eq!(terminal[0]["terminalId"], "toolu_bg");
    let exit = terminal
        .iter()
        .find(|u| u.get("exitStatus").is_some())
        .expect("an exit");
    assert_eq!(
        exit["exitStatus"],
        json!({}),
        "a background command reports no exit code: {exit:?}"
    );

    let idle_pos = updates
        .iter()
        .position(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
        .expect("an idle");
    let exit_pos = updates
        .iter()
        .position(|u| u["sessionUpdate"] == "terminal_update" && u.get("exitStatus").is_some())
        .expect("an exit");
    assert!(
        exit_pos > idle_pos,
        "the terminal outlives the turn's idle: {updates:?}"
    );

    let statuses: Vec<&str> = updates
        .iter()
        .filter(|u| {
            u["sessionUpdate"] == "tool_call_update"
                && u["toolCallId"] == "toolu_bg"
                && u.get("status").is_some()
        })
        .filter_map(|u| u["status"].as_str())
        .collect();
    assert_eq!(statuses, vec!["pending", "completed"], "{updates:?}");
    harness.shutdown().await;
}

/// U-104 — a v2 client that appends `terminal_output_delta` gets the command's
/// output as `terminal_output_chunk` frames (base64) and the exit as a
/// `terminal_update`.
#[tokio::test]
async fn u104_v2_terminal_output_chunks_for_a_delta_client() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [task_started(), bash_use(), backgrounded(), result()],
        }),
    ];
    let mut harness = Harness::start("delta", &steps);
    let root = harness
        .open_session_with(json!({
            "auth": {"terminal": {}},
            "_meta": {"terminal_output_delta": true},
        }))
        .await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":root,"prompt":[{"type":"text","text":"hi"}]}}))
        .await;
    let updates = harness
        .read_until_idles(root.as_str().unwrap_or(""), 1)
        .await;

    let chunks: Vec<&Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "terminal_output_chunk")
        .collect();
    assert_eq!(chunks.len(), 1, "{updates:?}");
    assert_eq!(chunks[0]["terminalId"], "toolu_bg");
    assert_eq!(
        chunks[0]["data"], "Q29tbWFuZCBydW5uaW5nIGluIGJhY2tncm91bmQgd2l0aCBJRDogYmcxLg==",
        "the command output as base64: {updates:?}"
    );
    // The command still runs: no exit rides the turn's updates.
    assert!(
        !updates
            .iter()
            .any(|u| u["sessionUpdate"] == "terminal_update" && u.get("exitStatus").is_some()),
        "the background command keeps running: {updates:?}"
    );
    harness.shutdown().await;
}

/// T-104 — when the task already ended before the Bash result, the terminal
/// ends at the result (before the turn's idle).
#[tokio::test]
async fn t104_v2_background_terminal_ends_at_the_result_when_the_task_ended() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                task_started(),
                bash_use(),
                task_ended()[0].clone(),
                task_ended()[1].clone(),
                backgrounded(),
                result(),
            ],
        }),
    ];
    let mut harness = Harness::start("ended-first", &steps);
    let root = harness.open_session().await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":root,"prompt":[{"type":"text","text":"hi"}]}}))
        .await;
    let updates = harness
        .read_until_idles(root.as_str().unwrap_or(""), 1)
        .await;

    let terminal: Vec<&Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "terminal_update")
        .collect();
    assert_eq!(
        terminal.len(),
        3,
        "an announcement, output and an exit: {updates:?}"
    );
    let exit_pos = updates
        .iter()
        .position(|u| u["sessionUpdate"] == "terminal_update" && u.get("exitStatus").is_some())
        .expect("an exit");
    let idle_pos = updates
        .iter()
        .position(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
        .expect("an idle");
    assert!(
        exit_pos < idle_pos,
        "the terminal ends at the result: {updates:?}"
    );
    harness.shutdown().await;
}
