//! Live smoke tests (item F44 part 2).
//!
//! Gated by env `PARITY_LIVE=1`, skipped when unset or `claude` CLI is absent.
//! Drives the installed `claude` end to end through the built adapter:
//! initialize, new session, tiny prompt, cancel, and resume.
//!
//! Asserts protocol-level facts only (valid responses, a stopReason, no process left).
//! Never prints environment values.

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use common::{rust_agent_binary, wait_pid_gone};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const LIVE: &str = "PARITY_LIVE";
const TIMEOUT: Duration = Duration::from_secs(120);

fn claude_path() -> Option<PathBuf> {
    let custom = std::env::var("PARITY_CLAUDE").unwrap_or_else(|_| "claude".to_string());
    let which = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {custom}"))
        .output()
        .ok()?;
    if !which.status.success() {
        return None;
    }
    let resolved = String::from_utf8_lossy(&which.stdout).trim().to_string();
    if resolved.is_empty() {
        None
    } else {
        Some(PathBuf::from(resolved))
    }
}

fn enabled() -> Option<PathBuf> {
    if std::env::var(LIVE).as_deref() != Ok("1") {
        println!(
            "SKIPPED: parity live tests require {LIVE}=1 (set {LIVE}=1 with claude on PATH to run)"
        );
        return None;
    }
    match claude_path() {
        Some(path) => Some(path),
        None => {
            println!("SKIPPED: parity live tests require `claude` CLI on PATH (or PARITY_CLAUDE)");
            None
        }
    }
}

struct LiveClient {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    frames: Vec<Value>,
    pid: Option<u32>,
}

impl LiveClient {
    async fn start(claude_bin: &PathBuf, cwd: &std::path::Path) -> Result<Self, String> {
        let bin = rust_agent_binary();
        if !bin.exists() {
            return Err(format!("agent binary not found at {}", bin.display()));
        }
        let mut cmd = Command::new(&bin);
        cmd.current_dir(cwd)
            .env("CLAUDE_CODE_EXECUTABLE", claude_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "missing stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "missing stdout".to_string())?;
        let reader = BufReader::new(stdout).lines();

        Ok(LiveClient {
            child,
            stdin,
            lines: reader,
            next_id: 1,
            frames: Vec::new(),
            pid,
        })
    }

    async fn send(&mut self, value: &Value) -> Result<(), String> {
        let text = value.to_string();
        self.stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| e.to_string())?;
        self.stdin.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&req).await?;

        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::from_millis(1));
            let line_opt = tokio::time::timeout(remaining, self.lines.next_line())
                .await
                .map_err(|_| format!("timeout waiting for response to {method}"))?
                .map_err(|e| format!("io error waiting for response: {e}"))?;

            let Some(line) = line_opt else {
                return Err(format!("EOF from agent waiting for {method} response"));
            };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            self.frames.push(frame.clone());

            // An agent->client request is answered even when its id collides with
            // the pending client request id: the two id spaces are separate (the
            // adapter's own requests start at 0 and increment). Checked BEFORE the
            // response branch, or the Nth permission request would be consumed as
            // this request's response (P3-11).
            if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                if let Some(req_id) = frame.get("id") {
                    let options = frame
                        .get("params")
                        .and_then(|p| p.get("options"))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let opt_id = options
                        .iter()
                        .find(|o| o.get("kind").and_then(Value::as_str) == Some("allow_once"))
                        .or_else(|| options.first())
                        .and_then(|o| o.get("optionId"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "outcome": {
                                "outcome": "selected",
                                "optionId": opt_id
                            }
                        }
                    });
                    self.send(&reply).await?;
                }
                continue;
            }

            // Check if this is the response to our request
            if frame.get("method").is_none() && frame.get("id").and_then(Value::as_u64) == Some(id)
            {
                if let Some(err) = frame.get("error") {
                    return Err(format!("{method} failed: {err}"));
                }
                return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(&msg).await
    }

    async fn close(mut self) {
        drop(self.stdin);
        let wait_res = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
        if wait_res.is_err() {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        if let Some(pid) = self.pid {
            let _ = wait_pid_gone(pid, Duration::from_secs(5));
        }
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        if let Some(pid) = self.pid {
            let _ = wait_pid_gone(pid, Duration::from_secs(5));
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "claude-live-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status();
    dir
}

async fn init(client: &mut LiveClient) -> Result<Value, String> {
    client
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": true
                },
                "clientInfo": { "name": "parity-live", "version": "0" }
            }),
        )
        .await
}

async fn new_session(client: &mut LiveClient, cwd: &std::path::Path) -> Result<String, String> {
    let result = client
        .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
        .await?;
    let sid = result
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "session/new had no sessionId".to_string())?;

    // Configure model haiku through normal session config
    let _ = client
        .request(
            "session/set_config_option",
            json!({
                "sessionId": sid,
                "configId": "model",
                "value": "haiku"
            }),
        )
        .await;

    Ok(sid)
}

#[tokio::test]
async fn parity_live_initialize() {
    tokio::time::timeout(TIMEOUT, async {
        let Some(claude) = enabled() else { return };
        let cwd = scratch("init");
        let mut client = LiveClient::start(&claude, &cwd)
            .await
            .expect("start client");
        let result = init(&mut client).await.expect("initialize");
        assert_eq!(result["protocolVersion"], json!(1));
        assert_eq!(
            result["agentInfo"]["name"],
            json!("@agentclientprotocol/claude-agent-acp")
        );
        client.close().await;
        let _ = std::fs::remove_dir_all(&cwd);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn parity_live_new_session() {
    tokio::time::timeout(TIMEOUT, async {
        let Some(claude) = enabled() else { return };
        let cwd = scratch("new");
        let mut client = LiveClient::start(&claude, &cwd)
            .await
            .expect("start client");
        init(&mut client).await.expect("initialize");
        let session = new_session(&mut client, &cwd).await.expect("session/new");
        assert!(!session.is_empty());
        client.close().await;
        let _ = std::fs::remove_dir_all(&cwd);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn parity_live_tiny_prompt() {
    tokio::time::timeout(TIMEOUT, async {
        let Some(claude) = enabled() else { return };
        let cwd = scratch("prompt");
        let mut client = LiveClient::start(&claude, &cwd)
            .await
            .expect("start client");
        init(&mut client).await.expect("initialize");
        let session = new_session(&mut client, &cwd).await.expect("session/new");
        let before = client.frames.len();
        let resp = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": "reply with the single word ok" }]
                }),
            )
            .await
            .expect("prompt");
        assert_eq!(resp["stopReason"], json!("end_turn"));
        // The prompt must produce assistant text: the `session/update` that
        // follows `session/new` (`available_commands_update`) would satisfy a
        // bare "any update" assertion (P3-10).
        assert!(
            client.frames[before..].iter().any(|f| {
                f.get("method").and_then(Value::as_str) == Some("session/update")
                    && f.pointer("/params/update/sessionUpdate")
                        .and_then(Value::as_str)
                        == Some("agent_message_chunk")
            }),
            "expected an agent_message_chunk for the tiny prompt"
        );
        client.close().await;
        let _ = std::fs::remove_dir_all(&cwd);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn parity_live_tool_permission() {
    tokio::time::timeout(TIMEOUT, async {
        let Some(claude) = enabled() else { return };
        let cwd = scratch("tool");
        let mut client = LiveClient::start(&claude, &cwd)
            .await
            .expect("start client");
        init(&mut client).await.expect("initialize");
        let session = new_session(&mut client, &cwd).await.expect("session/new");
        let resp = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{
                        "type": "text",
                        "text": "Run the shell command `printf hi > live-tool.txt` and then reply with the single word done"
                    }]
                }),
            )
            .await
            .expect("prompt");
        assert_eq!(resp["stopReason"], json!("end_turn"));
        // The default permission mode must have asked the client before the
        // command ran, and request() answers it with allow_once.
        assert!(
            client
                .frames
                .iter()
                .any(|f| f.get("method").and_then(Value::as_str)
                    == Some("session/request_permission")),
            "expected a permission request for the Bash tool"
        );
        assert!(
            client
                .frames
                .iter()
                .any(|f| f.get("method").and_then(Value::as_str) == Some("session/update")
                    && f.pointer("/params/update/sessionUpdate")
                        .and_then(Value::as_str)
                        .is_some_and(|k| k == "tool_call" || k == "tool_call_update")),
            "expected a tool call update on the session stream"
        );
        client.close().await;
        let _ = std::fs::remove_dir_all(&cwd);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn parity_live_cancel() {
    tokio::time::timeout(TIMEOUT, async {
        let Some(claude) = enabled() else { return };
        let cwd = scratch("cancel");
        let mut client = LiveClient::start(&claude, &cwd)
            .await
            .expect("start client");
        init(&mut client).await.expect("initialize");
        let session = new_session(&mut client, &cwd)
            .await
            .expect("session/new");

        let id = client.next_id;
        client.next_id += 1;
        client
            .send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": {
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": "Count from 1 to 1000 with 1 second delay between each" }]
                }
            }))
            .await
            .expect("send prompt");

        client
            .notify("session/cancel", json!({ "sessionId": session }))
            .await
            .expect("send cancel");

        let deadline = tokio::time::Instant::now() + TIMEOUT;
        // P3-08: a stream that ends (EOF) before the prompt's response must fail
        // the test, not pass vacuously.
        let mut saw_cancel = false;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::from_millis(1));
            let line_opt = tokio::time::timeout(remaining, client.lines.next_line())
                .await
                .expect("timeout waiting for cancel settlement")
                .expect("reading stream");
            let Some(line) = line_opt else { break };
            if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                if frame.get("method").is_none()
                    && frame.get("id").and_then(Value::as_u64) == Some(id)
                {
                    saw_cancel = frame["result"]["stopReason"] == json!("cancelled");
                    break;
                }
            }
        }
        assert!(
            saw_cancel,
            "the cancelled prompt never settled with stopReason `cancelled` (the stream ended first)"
        );
        client.close().await;
        let _ = std::fs::remove_dir_all(&cwd);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn parity_live_resume() {
    tokio::time::timeout(TIMEOUT, async {
        let Some(claude) = enabled() else { return };
        let cwd = scratch("resume");
        let mut client = LiveClient::start(&claude, &cwd)
            .await
            .expect("start client");
        init(&mut client).await.expect("initialize");
        let session = new_session(&mut client, &cwd)
            .await
            .expect("session/new");

        client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": "Remember: the secret word is banana. Reply with only: stored" }]
                }),
            )
            .await
            .expect("store prompt");
        client.kill().await;

        let mut client = LiveClient::start(&claude, &cwd)
            .await
            .expect("restart client");
        init(&mut client).await.expect("initialize");
        client
            .request(
                "session/resume",
                json!({
                    "sessionId": session,
                    "cwd": cwd,
                    "mcpServers": []
                }),
            )
            .await
            .expect("resume session");

        let before = client.frames.len();
        let resp = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": "What is the secret word I told you? Reply with only that word." }]
                }),
            )
            .await
            .expect("recall prompt");
        assert_eq!(resp["stopReason"], json!("end_turn"));
        // P3-09: the resumed session must actually recall the stored word. An
        // RPC-success check alone passes for a resume that loses the history.
        assert!(
            client.frames[before..].iter().any(|f| {
                f.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("agent_message_chunk")
                    && f.pointer("/params/update/content/text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.to_lowercase().contains("banana"))
            }),
            "the resumed session did not recall the stored word `banana`"
        );
        client.close().await;
        let _ = std::fs::remove_dir_all(&cwd);
    })
    .await
    .expect("test timed out");
}
