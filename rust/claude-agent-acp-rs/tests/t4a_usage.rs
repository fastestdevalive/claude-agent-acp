//! Turn 4 / lane A (F24, S-01): the structured `/usage` replacement. The agent
//! calls the SDK's experimental `get_usage` control request with a 5 s timeout
//! and replaces the CLI's own output with the rendered Markdown; on timeout,
//! transport error or an incompatible response it keeps the CLI text.
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

fn local_command_output(content: &str) -> Value {
    json!({"type": "system", "subtype": "local_command_output", "content": content})
}

fn success_result(result: &str, output_tokens: u64) -> Value {
    json!({
        "type": "result", "subtype": "success", "stop_reason": "end_turn",
        "is_error": false, "result": result,
        "usage": {"input_tokens": 0, "output_tokens": output_tokens,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        "modelUsage": {"claude-sonnet-4-5": {"contextWindow": 200_000}}
    })
}

fn write_transcript(tmp: &Path, steps: &[Value]) -> PathBuf {
    let path = tmp.join("t4a-usage.transcript.jsonl");
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
        let tmp = std::env::temp_dir().join(format!("acp-t4a-usage-{name}-{}", std::process::id()));
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

    async fn open_session(&mut self) -> Value {
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1}}))
            .await;
        self.read_response(1).await;
        self.send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        self.read_response(2).await["result"]["sessionId"].clone()
    }

    /// Send a prompt and collect every `session/update` frame until the prompt
    /// response arrives. Returns the response and the updates.
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

/// S-01 — the structured response replaces the CLI's raw output, a mirror of
/// the same local-command output is published once, and a later, distinct
/// diagnostic from the same lane still goes out.
#[tokio::test]
async fn t4a_usage_is_replaced_by_the_structured_markdown() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("/usage"),
                local_command_output("Raw Claude Code usage output"),
                local_command_output("Raw Claude Code usage output"),
                local_command_output("Interrupted by user"),
                success_result("", 0),
            ]
        }),
        usage_reply(),
    ];
    let mut harness = Harness::start("structured", &steps);
    let session_id = harness.open_session().await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert!(text.contains("## Usage"), "{text}");
    assert!(text.contains("### This session"), "{text}");
    assert!(text.contains("Interrupted by user"), "{text}");
    assert!(!text.contains("Raw Claude Code usage output"), "{text}");
    harness.finish().await;
}

/// S-01 — a model-generated `/usage` answer is not replaced: the result carries
/// output tokens, so it is not the local command's output.
#[tokio::test]
async fn t4a_usage_does_not_replace_a_model_answer() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("/usage"), success_result("A custom command's model answer", 5)]
        }),
        usage_reply(),
    ];
    let mut harness = Harness::start("model-answer", &steps);
    let session_id = harness.open_session().await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert!(!text.contains("## Usage"), "{text}");
    harness.finish().await;
}

/// S-01 — a `get_usage` that rejects falls back to the CLI's original output.
#[tokio::test]
async fn t4a_usage_error_keeps_the_cli_output() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("/usage"),
                local_command_output("Original Claude Code usage output"),
                success_result("", 0),
            ]
        }),
        json!({
            "expect": {"type": "control_request", "request": {"subtype": "get_usage"}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "error", "request_id": "$REQ", "error": "unsupported"
            }}]
        }),
    ];
    let mut harness = Harness::start("error", &steps);
    let session_id = harness.open_session().await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert_eq!(text, "Original Claude Code usage output");
    harness.finish().await;
}

/// S-01 — a `get_usage` that never answers times out after 5 s and falls back
/// to the CLI's original output.
#[tokio::test]
async fn t4a_usage_timeout_keeps_the_cli_output() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("/usage"),
                local_command_output("Timed out usage output"),
                success_result("", 0),
            ]
        }),
        // The request matches and is never answered.
        json!({
            "expect": {"type": "control_request", "request": {"subtype": "get_usage"}},
            "emit": []
        }),
    ];
    let mut harness = Harness::start("timeout", &steps);
    let session_id = harness.open_session().await;
    let started = tokio::time::Instant::now();
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "the fallback waits for the 5 s timeout"
    );
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert_eq!(text, "Timed out usage output");
    harness.finish().await;
}

/// S-01 — an incompatible `get_usage` response falls back to the CLI's text.
#[tokio::test]
async fn t4a_usage_incompatible_response_keeps_the_cli_output() {
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("/usage"),
                local_command_output("Original Claude Code usage output"),
                success_result("", 0),
            ]
        }),
        json!({
            "expect": {"type": "control_request", "request": {"subtype": "get_usage"}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ", "response": {"broken": true}
            }}]
        }),
    ];
    let mut harness = Harness::start("incompatible", &steps);
    let session_id = harness.open_session().await;
    let (response, updates) = harness.prompt(3, "/usage", &session_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = text_chunks(&updates, "agent_message_chunk").join("\n");
    assert_eq!(text, "Original Claude Code usage output");
    harness.finish().await;
}

/// U-08/N-21 — the follow-up prompt on the wire: an AIR client gets the
/// prediction as a `session_info_update`, and the next prompt clears it; a plain
/// client gets nothing.
#[tokio::test]
async fn t4a_prompt_suggestion_is_published_to_air_and_cleared_by_the_next_prompt() {
    let suggestion = json!({"type":"prompt_suggestion","suggestion":"Run the tests","uuid":"u-s","session_id":"s"});
    let steps = [
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("hello"), success_result("Hello there", 5), suggestion]
        }),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("second"), success_result("Again", 5)]
        }),
    ];
    let mut harness = Harness::start("suggestion", &steps);
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1,
                "clientCapabilities":{"_meta":{"jetbrains":{"air":{"version":1,"capabilities":[]}}}}}}))
        .await;
    harness.read_response(1).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
        .await;
    let session_id = harness.read_response(2).await["result"]["sessionId"].clone();

    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hello"}]}}))
        .await;
    let first = harness.read_until_response_and_quiet(3).await;
    let suggestions = |updates: &[Value]| -> Vec<Value> {
        updates
            .iter()
            .filter(|update| update["sessionUpdate"] == "session_info_update")
            .filter_map(|update| update["_meta"].get("promptSuggestions").cloned())
            .collect()
    };
    assert_eq!(
        suggestions(&first),
        vec![json!({"suggestions":[{"text":"Run the tests"}]})],
        "{first:?}"
    );

    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"second"}]}}))
        .await;
    let second = harness.read_until_response_and_quiet(4).await;
    assert_eq!(
        suggestions(&second),
        vec![json!({"suggestions":[]})],
        "the next prompt clears the prediction: {second:?}"
    );
    harness.finish().await;
}

impl Harness {
    /// Read until the response for `id`, then drain updates until the stream is
    /// quiet for a moment (the prediction follows the prompt response).
    async fn read_until_response_and_quiet(&mut self, id: i64) -> Vec<Value> {
        let mut updates = Vec::new();
        loop {
            let frame = self.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("method").is_none()
            {
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
        updates
    }
}
