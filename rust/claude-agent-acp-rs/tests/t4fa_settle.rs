//! Turn 4 fix round (lane FA, T4FA): the `/usage` settle path end to end. The
//! structured producer must never change an error result's outcome, the result
//! replacement must follow the result `usage_update`, a replayed local-command
//! output must be dropped, and a crash must report running tasks as `failed`.
//!
//! Each test drives the real `serve` over a hermetic duplex pipe and reads the
//! agent's raw JSON-RPC lines against a fake `claude` transcript.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::timeout;

fn init_step() -> Value {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [{"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"}],
                "account": {"organizationId": "org-1", "subscriptionType": null},
                "commands": [], "modes": [], "agents": [], "modelInfos": [],
                "toolAliases": {}, "authMethods": [], "configOptions": {},
            }
        }}]
    })
}

/// The structured response `get_usage` returns (a shape `parseUsageResponse`
/// accepts).
fn usage_payload() -> Value {
    json!({
        "session": {
            "total_cost_usd": 1.25,
            "total_api_duration_ms": 65_000,
            "total_duration_ms": 120_000,
            "total_lines_added": 0,
            "total_lines_removed": 0,
            "model_usage": {
                "claude-sonnet-4-5": {
                    "inputTokens": 1_000,
                    "outputTokens": 500,
                    "cacheReadInputTokens": 0,
                    "cacheCreationInputTokens": 0
                }
            }
        },
        "subscription_type": "max",
        "rate_limits_available": false,
        "rate_limits": null,
        "behaviors": null
    })
}

fn usage_reply() -> Value {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "get_usage"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ", "response": usage_payload()
        }}]
    })
}

fn echo(text: &str) -> Value {
    json!({"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

fn result(subtype: &str, is_error: bool, stop_reason: &str, text: &str) -> Value {
    json!({
        "type": "result", "subtype": subtype, "stop_reason": stop_reason,
        "is_error": is_error, "result": text,
        "usage": {"input_tokens": 0, "output_tokens": 0,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        "modelUsage": {"claude-sonnet-4-5": {"contextWindow": 200_000}}
    })
}

fn write_transcript(tmp: &Path, steps: &[Value]) -> PathBuf {
    let path = tmp.join("t4fa-settle.transcript.jsonl");
    let mut text = String::new();
    for step in steps {
        text.push_str(&serde_json::to_string(step).expect("serialize step"));
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write transcript");
    path
}

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str, steps: &[Value]) -> Self {
        let tmp =
            std::env::temp_dir().join(format!("acp-t4fa-settle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![
                (
                    "FAKE_CLAUDE_SCRIPT".to_string(),
                    write_transcript(&tmp, steps).to_string_lossy().into_owned(),
                ),
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    tmp.join("config").to_string_lossy().into_owned(),
                ),
            ],
            default_cwd: Some(repo_root()),
            timings: Timings {
                force_cancel_grace: Duration::from_millis(1000),
                ..Timings::default()
            },
            env: EnvSnapshot::empty(),
            logger: None,
        };
        let task = tokio::spawn(serve(WireTransport::new(agent_in, agent_out), opts));
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

    async fn open_session(&mut self, air_async_tasks: bool) -> Value {
        let capabilities = if air_async_tasks {
            json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks"]}}}})
        } else {
            json!({})
        };
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1,"clientCapabilities":capabilities}}))
            .await;
        self.read_response(1).await;
        self.send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        self.read_response(2).await["result"]["sessionId"].clone()
    }

    async fn prompt(&mut self, id: i64, text: &str, session_id: &Value) -> (Value, Vec<Value>) {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":text}]}}))
            .await;
        let mut updates = Vec::new();
        loop {
            let frame = self.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("method").is_none()
            {
                return (frame, updates);
            }
            if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(frame["params"]["update"].clone());
            }
        }
    }

    /// Read until the response for `id`, then drain updates until the stream is
    /// quiet (the terminal async-task state follows the response).
    async fn read_until_response_and_quiet(&mut self, id: i64) -> (Value, Vec<Value>) {
        let mut updates = Vec::new();
        let response;
        loop {
            let frame = self.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("method").is_none()
            {
                response = frame;
                break;
            }
            if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(frame["params"]["update"].clone());
            }
        }
        loop {
            let mut line = String::new();
            match timeout(Duration::from_millis(400), self.reader.read_line(&mut line)).await {
                Ok(Ok(n)) if n > 0 => {
                    let frame: Value = serde_json::from_str(&line).expect("agent line is JSON");
                    if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                        updates.push(frame["params"]["update"].clone());
                    }
                }
                _ => break,
            }
        }
        (response, updates)
    }

    async fn finish(self) {
        drop(self.input);
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn text_chunks(updates: &[Value], kind: &str) -> Vec<String> {
    updates
        .iter()
        .filter(|update| update["sessionUpdate"] == kind)
        .filter_map(|update| update["content"]["text"].as_str().map(str::to_string))
        .collect()
}

/// Finding 1: a `/usage` turn whose CLI answers with the login error still
/// fails as `auth_required`; the structured producer never eats the result text
/// nor publishes a usage chunk.
#[tokio::test]
async fn t4fa_usage_login_error_is_not_swapped() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("/usage"), result("success", true, "error", "Please run /login to continue")]
        }),
        usage_reply(),
    ];
    let mut harness = Harness::start("login", &steps);
    let session_id = harness.open_session(false).await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert!(response.get("error").is_some(), "{response}");
    assert_eq!(
        response["error"]["message"], "Authentication required",
        "{response}"
    );
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert!(!text.contains("## Usage"), "{text}");
    harness.finish().await;
}

/// Finding 1: `max_tokens` on a `/usage` turn keeps its stop reason.
#[tokio::test]
async fn t4fa_usage_max_tokens_is_not_swapped() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("/usage"), result("success", false, "max_tokens", "")]
        }),
        usage_reply(),
    ];
    let mut harness = Harness::start("max-tokens", &steps);
    let session_id = harness.open_session(false).await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "max_tokens", "{response}");
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert!(!text.contains("## Usage"), "{text}");
    harness.finish().await;
}

/// Finding 1: a refusal on a `/usage` turn keeps its stop reason.
#[tokio::test]
async fn t4fa_usage_refusal_is_not_swapped() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("/usage"), result("success", false, "refusal", "")]
        }),
        usage_reply(),
    ];
    let mut harness = Harness::start("refusal", &steps);
    let session_id = harness.open_session(false).await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "refusal", "{response}");
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert!(!text.contains("## Usage"), "{text}");
    harness.finish().await;
}

/// Finding 2: when the CLI carries the command output on the `result` alone,
/// the replacement chunk follows the result `usage_update`.
#[tokio::test]
async fn t4fa_usage_replacement_follows_the_result_usage_update() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("/usage"),
                {"type": "stream_event", "parent_tool_use_id": null, "event": {
                    "type": "message_start",
                    "message": {"id": "m1", "model": "claude-sonnet-4-5",
                        "usage": {"input_tokens": 100, "output_tokens": 0,
                            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}}},
                result("success", false, "end_turn", "Raw usage output"),
            ]
        }),
        usage_reply(),
    ];
    let mut harness = Harness::start("order", &steps);
    let session_id = harness.open_session(false).await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    let usage = kinds
        .iter()
        .rposition(|k| *k == "usage_update")
        .expect("a result usage_update");
    let chunk = kinds
        .iter()
        .position(|k| *k == "agent_message_chunk")
        .expect("the replacement chunk");
    assert!(
        usage < chunk,
        "the replacement must follow the usage_update: {kinds:?}"
    );
    assert!(
        text_chunks(&updates, "agent_message_chunk")
            .iter()
            .any(|t| t.contains("## Usage")),
        "the structured markdown replaced the CLI text: {updates:?}"
    );
    harness.finish().await;
}

/// Finding 3: a replayed `local-command-stdout` user frame is dropped, while a
/// live one still maps.
#[tokio::test]
async fn t4fa_replayed_local_command_output_is_dropped() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("hello"),
                {"type": "user", "uuid": "replay-uuid", "isReplay": true,
                    "message": {"role": "user", "content":
                        "leaked <local-command-stdout>raw output</local-command-stdout>"}},
                {"type": "user", "uuid": "live-uuid",
                    "message": {"role": "user", "content":
                        "shown <local-command-stdout>live output</local-command-stdout>"}},
                result("success", false, "end_turn", "done")
            ]
        }),
    ];
    let mut harness = Harness::start("replay", &steps);
    let session_id = harness.open_session(false).await;
    let (response, updates) = harness.prompt(3, "hello", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = text_chunks(&updates, "user_message_chunk").join("\n");
    assert!(
        !text.contains("leaked"),
        "the replayed frame is dropped: {text}"
    );
    assert!(text.contains("shown"), "the live frame still maps: {text}");
    harness.finish().await;
}

/// Finding 7 (T4A review): a steer that starts a detached turn goes through
/// `prompt()` upstream, so it clears the stale prediction and registers a
/// `/usage` producer.
#[tokio::test]
async fn t4fa_steer_starting_a_turn_runs_the_prompt_preamble() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("hello"),
                result("success", false, "end_turn", "Hi there"),
                {"type": "prompt_suggestion", "suggestion": "Run the tests",
                    "uuid": "u-s", "session_id": "s"},
            ]
        }),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("/usage"), result("success", false, "end_turn", "")]
        }),
        usage_reply(),
    ];
    let mut harness = Harness::start("steer-usage", &steps);
    let session_id = harness.open_session(true).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hello"}]}}))
        .await;
    let (_response, first) = harness.read_until_response_and_quiet(3).await;
    assert!(
        first
            .iter()
            .any(|u| u["sessionUpdate"] == "session_info_update"
                && u["_meta"]["promptSuggestions"]["suggestions"][0]["text"] == "Run the tests"),
        "the prediction was published: {first:?}"
    );

    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"_session/steering",
            "params":{"sessionId":session_id,
                "prompt":[{"type":"text","text":"/usage"}]}}))
        .await;
    let (response, updates) = harness.read_until_response_and_quiet(4).await;
    assert_eq!(
        response["result"]["outcome"], "startedNewTurn",
        "{response}"
    );
    assert!(
        updates
            .iter()
            .any(|u| u["sessionUpdate"] == "session_info_update"
                && u["_meta"]["promptSuggestions"]["suggestions"] == json!([])),
        "the stale prediction was cleared: {updates:?}"
    );
    assert!(
        text_chunks(&updates, "agent_message_chunk")
            .iter()
            .any(|t| t.contains("## Usage")),
        "the steered `/usage` turn got its producer: {updates:?}"
    );
    harness.finish().await;
}

/// Finding 4 (T4B review): a crash without a cancel reports the running tasks
/// `failed`, not `stopped`.
#[tokio::test]
async fn t4fa_crash_reports_running_tasks_as_failed() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("hello"),
                {"type": "system", "subtype": "task_started", "task_id": "task-1",
                    "task_type": "local_bash", "description": "Sleep", "is_backgrounded": true},
            ],
            "exit": true
        }),
    ];
    let mut harness = Harness::start("crash", &steps);
    let session_id = harness.open_session(true).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hello"}]}}))
        .await;
    let (_response, updates) = harness.read_until_response_and_quiet(3).await;
    let states: Vec<Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "async_task_state_update" && u["asyncTaskId"] == "task-1")
        .map(|u| u["state"].clone())
        .collect();
    assert!(
        states.iter().any(|s| s == "failed"),
        "the crashed task reports failed: {updates:?}"
    );
    assert!(
        states.iter().all(|s| s != "stopped"),
        "a crash without a cancel never reports stopped: {states:?}"
    );
    harness.finish().await;
}
