//! Turn 3 fix round, lane B: a provider switch waits for the running turns off
//! the request handler, so the connection keeps delivering the answers and the
//! cancel those turns need. Each test drives the real `serve` over a hermetic
//! duplex pipe and reads the agent's raw JSON-RPC lines.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::net::TcpListener;
use tokio::time::timeout;

fn initialize_line() -> String {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [{"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"}],
                "account": {"apiKeySource": "ANTHROPIC_API_KEY"},
                "commands": [], "modes": [], "agents": [], "modelInfos": [],
                "toolAliases": {}, "authMethods": [], "configOptions": {},
            }
        }}]
    })
    .to_string()
}

/// A turn that asks the client to run a tool, then ends once the permission was
/// answered (allowed or cancelled).
fn permission_turn() -> String {
    json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "use a tool"}]}},
            {"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01P", "name": "Bash", "input": {"command": "echo hi"}}
            ]}},
            {"type": "control_request", "request_id": "permreq1", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "echo hi"},
                "tool_use_id": "toolu_01P", "agent_id": null, "permission_suggestions": [],
                "title": "Run Bash command", "display_name": "Bash", "description": "Run a command"
            }}
        ]
    })
    .to_string()
        + "\n"
        + &json!({
            "expect": {"type": "control_response", "response": {"response": {"behavior": "allow"}}},
            "emit": [
                {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01P", "is_error": false, "content": "hi"}
                ]}},
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                 "is_error": false, "result": "done",
                 "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
            ]
        })
        .to_string()
}

/// The same turn, but the client cancels it: the CLI answers the `interrupt`.
fn cancelled_turn() -> String {
    json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "use a tool"}]}},
            {"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01P", "name": "Bash", "input": {"command": "echo hi"}}
            ]}},
            {"type": "control_request", "request_id": "permreq1", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "echo hi"},
                "tool_use_id": "toolu_01P", "agent_id": null, "permission_suggestions": [],
                "title": "Run Bash command", "display_name": "Bash", "description": "Run a command"
            }}
        ]
    })
    .to_string()
        + "\n"
        + &json!({
            "expect": {"type": "control_request", "request": {"subtype": "interrupt"}},
            "emit": [
                {"type": "control_response", "response": {"subtype": "success", "request_id": "$REQ"}},
                {"type": "result", "subtype": "success", "stop_reason": "cancelled",
                 "is_error": false, "result": "",
                 "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
            ]
        })
        .to_string()
}

/// Two segments: the first turn, then the initialize of the query the provider
/// update starts under the same session id.
fn write_transcript(tmp: &Path, first: &str) -> PathBuf {
    let path = tmp.join("switch.transcript.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n{{\"segment\":1}}\n{}\n{{\"auth_status_stdout\":{{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\",\"subscriptionType\":\"pro\"}}}}\n",
            initialize_line(),
            first,
            initialize_line()
        ),
    )
    .expect("write transcript");
    path
}

/// A gateway that answers `/v1/models` with one id.
async fn stub_gateway() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let body = json!({ "data": [{ "id": "claude-sonnet-4-5" }] }).to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    base
}

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str, first: &str) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t3fb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![
                (
                    "FAKE_CLAUDE_SCRIPT".to_string(),
                    write_transcript(&tmp, first).to_string_lossy().into_owned(),
                ),
                (
                    "FAKE_CLAUDE_CURSOR".to_string(),
                    tmp.join("cursor").to_string_lossy().into_owned(),
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

    /// Reads frames until both responses are in; `None` when the deadline passes.
    async fn read_two_responses(&mut self, first: i64, second: i64) -> Option<(Value, Value)> {
        let mut a = None;
        let mut b = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while a.is_none() || b.is_none() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let mut line = String::new();
            let Ok(Ok(n)) = timeout(remaining, self.reader.read_line(&mut line)).await else {
                return None;
            };
            assert!(n > 0, "agent closed stdout");
            let frame: Value = serde_json::from_str(&line).expect("agent line is JSON");
            if frame.get("method").is_some() {
                continue;
            }
            match frame.get("id").and_then(Value::as_i64) {
                Some(id) if id == first => a = Some(frame),
                Some(id) if id == second => b = Some(frame),
                _ => {}
            }
        }
        Some((a?, b?))
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

    async fn finish(self) {
        drop(self.input);
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// The permission request the turn is waiting on: its request id.
async fn permission_request(harness: &mut Harness) -> Value {
    loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            return frame["id"].clone();
        }
    }
}

fn permission_answer(request_id: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":request_id,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}})
}

fn provider_set(id: i64, gateway: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"providers/set",
        "params":{"providerId":"main","apiType":"anthropic","baseUrl":gateway}})
}

/// The switch waits for the running turn off the request handler: the client's
/// permission answer is still delivered, the turn ends and then the switch
/// completes.
#[tokio::test]
async fn t3fb_permission_answer_settles_a_turn_while_a_provider_switch_waits() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway().await;
        let mut harness = Harness::start("perm", &permission_turn());
        let session_id = harness.open_session().await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
                "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"use a tool"}]}}))
            .await;
        let request_id = permission_request(&mut harness).await;
        // The switch queues behind the running turn...
        harness.send_json(provider_set(4, &gateway)).await;
        // ...and the answer still reaches the turn.
        harness.send_json(permission_answer(&request_id)).await;
        let (prompt, switch) = harness
            .read_two_responses(3, 4)
            .await
            .expect("the turn and the switch both settle");
        assert_eq!(prompt["result"]["stopReason"], "end_turn", "{prompt}");
        assert_eq!(switch["result"], json!({}), "{switch}");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// `session/cancel` is a notification: it is processed while the switch waits,
/// so the turn settles and the switch then completes.
#[tokio::test]
async fn t3fb_a_cancel_is_processed_while_a_provider_switch_waits() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway().await;
        let mut harness = Harness::start("cancel", &cancelled_turn());
        let session_id = harness.open_session().await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
                "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"use a tool"}]}}))
            .await;
        let _request_id = permission_request(&mut harness).await;
        harness.send_json(provider_set(4, &gateway)).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
                "params":{"sessionId":session_id}}))
            .await;
        let (prompt, switch) = harness
            .read_two_responses(3, 4)
            .await
            .expect("the cancel settles the turn and the switch completes");
        assert_eq!(prompt["result"]["stopReason"], "cancelled", "{prompt}");
        assert_eq!(switch["result"], json!({}), "{switch}");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// M-129: a session a provider switch could not recreate is reported to an
/// AIR-capable client as a session-scoped `internal_error` failure, instead of
/// disappearing silently.
#[tokio::test]
async fn t3fb_a_session_lost_in_a_provider_switch_is_reported() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start("lost", &permission_turn());
        harness
            .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":1,
                    "clientCapabilities":{"_meta":{"jetbrains":{"air":{"version":1,
                        "capabilities":["sessionFailure"]}}}}}}))
            .await;
        harness.read_response(1).await;
        // A session whose cwd is gone cannot come back: a non-auth failure.
        let cwd = harness.tmp.join("cwd");
        std::fs::create_dir_all(&cwd).expect("cwd");
        harness
            .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
                "params":{"cwd":cwd.to_string_lossy(),"mcpServers":[]}}))
            .await;
        let session_id = harness.read_response(2).await["result"]["sessionId"].clone();
        std::fs::remove_dir_all(&cwd).expect("remove cwd");
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"providers/disable",
                "params":{"providerId":"main"}}))
            .await;
        let mut failure = None;
        let mut switch = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while failure.is_none() || switch.is_none() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "the failure and the switch both arrive"
            );
            let mut line = String::new();
            let Ok(Ok(n)) = timeout(remaining, harness.reader.read_line(&mut line)).await else {
                panic!("timed out waiting for the failure report");
            };
            assert!(n > 0, "agent closed stdout");
            let frame: Value = serde_json::from_str(&line).expect("agent line is JSON");
            if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                if frame
                    .pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("session_info_update")
                {
                    failure = Some(frame);
                }
            } else if frame.get("id").and_then(Value::as_i64) == Some(3) {
                switch = Some(frame);
            }
        }
        let switch = switch.expect("the switch answered");
        assert_eq!(switch["result"], json!({}), "{switch}");
        let failure = failure.expect("a session failure update");
        assert_eq!(failure["params"]["sessionId"], session_id, "{failure}");
        let meta = &failure["params"]["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"];
        assert_eq!(meta["category"], "service", "{failure}");
        assert_eq!(meta["severity"], "error", "{failure}");
        let details = meta["details"].as_str().unwrap_or_default();
        assert!(details.contains("does not exist"), "{failure}");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// A turn a `_session/steering` started counts as running: the switch waits for
/// it instead of closing the session under it.
#[tokio::test]
async fn t3fb_a_steered_turn_holds_a_provider_switch() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway().await;
        let mut harness = Harness::start("steer", &permission_turn());
        let session_id = harness.open_session().await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"_session/steering",
                "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"use a tool"}]}}))
            .await;
        let request_id = permission_request(&mut harness).await;
        harness.send_json(provider_set(4, &gateway)).await;
        // The steered turn is still waiting on the client: the switch must not
        // answer before it settles (before the fix it closed the session at once).
        let mut line = String::new();
        let early = timeout(
            Duration::from_millis(700),
            harness.reader.read_line(&mut line),
        )
        .await;
        if let Ok(Ok(n)) = early {
            assert!(n > 0, "agent closed stdout");
            let frame: Value = serde_json::from_str(&line).expect("agent line is JSON");
            assert_ne!(
                frame.get("id").and_then(Value::as_i64),
                Some(4),
                "the switch answered while the steered turn was still running: {frame}"
            );
        }
        harness.send_json(permission_answer(&request_id)).await;
        let switch = harness.read_response(4).await;
        assert_eq!(switch["result"], json!({}), "{switch}");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}
