//! Turn 3 fix round 2, lane D: the provider gate never makes the connection wait
//! for something only the connection can deliver. A request that meets a provider
//! update in flight is parked by the registry actor instead of being awaited
//! inside the ACP handler, so the permission answers and the `session/cancel` the
//! update waits on keep flowing. Each test drives the real `serve` over a hermetic
//! duplex pipe and reads the agent's raw JSON-RPC lines; every wait is bounded.

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

fn initialize_step(sleep_ms: Option<u64>) -> String {
    let mut emit = Vec::new();
    if let Some(ms) = sleep_ms {
        emit.push(json!({ "sleep_ms": ms }));
    }
    emit.push(json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": "$REQ",
        "response": {
            "models": [{"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"}],
            "account": {"apiKeySource": "ANTHROPIC_API_KEY"},
            "commands": [], "modes": [], "agents": [], "modelInfos": [],
            "toolAliases": {}, "authMethods": [], "configOptions": {},
        }
    }}));
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": emit,
    })
    .to_string()
}

/// A turn that asks the client to run a tool, then ends once the permission was
/// answered.
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

/// A turn that just answers.
fn answering_turn(text: &str) -> String {
    json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": text}]}},
            {"type": "assistant", "message": {"role": "assistant",
                "content": [{"type": "text", "text": "on it"}]}},
            {"type": "result", "subtype": "success", "stop_reason": "end_turn",
             "is_error": false, "result": "on it",
             "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
        ]
    })
    .to_string()
}

/// The transcript: the lines of each segment, one segment per spawned query.
fn write_transcript(tmp: &Path, segments: &[Vec<String>]) -> PathBuf {
    let path = tmp.join("gate.transcript.jsonl");
    let mut text = String::new();
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            text.push_str(&format!("{{\"segment\":{index}}}\n"));
        }
        for line in segment {
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str(
        "{\"auth_status_stdout\":{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\",\"subscriptionType\":\"pro\"}}\n",
    );
    std::fs::write(&path, text).expect("write transcript");
    path
}

/// A gateway that answers `/v1/models` with one id after `delay`.
async fn stub_gateway(delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let body = json!({ "data": [{ "id": "claude-sonnet-4-5" }] }).to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(delay).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
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
    fn start(name: &str, segments: &[Vec<String>]) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t3fd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![
                (
                    "FAKE_CLAUDE_SCRIPT".to_string(),
                    write_transcript(&tmp, segments)
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "FAKE_CLAUDE_CURSOR".to_string(),
                    tmp.join("cursor").to_string_lossy().into_owned(),
                ),
                (
                    "FAKE_CLAUDE_ARGV_OUT".to_string(),
                    tmp.join("argv.json").to_string_lossy().into_owned(),
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

    /// Reads frames until every response in `ids` is in, in arrival order.
    async fn read_responses(&mut self, ids: &[i64]) -> Vec<Value> {
        let mut found: Vec<Value> = Vec::new();
        while found.len() < ids.len() {
            let frame = self.read_json().await;
            if frame.get("method").is_some() {
                continue;
            }
            let id = frame.get("id").and_then(Value::as_i64);
            if id.is_some_and(|id| ids.contains(&id)) {
                found.push(frame);
            }
        }
        found
    }

    async fn open_session(&mut self, id: i64) -> Value {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        self.read_response(id).await["result"]["sessionId"].clone()
    }

    async fn initialize(&mut self) {
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1}}))
            .await;
        self.read_response(1).await;
    }

    async fn finish(self) {
        drop(self.input);
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn response_of(frames: &[Value], id: i64) -> &Value {
    frames
        .iter()
        .find(|frame| frame.get("id").and_then(Value::as_i64) == Some(id))
        .unwrap_or_else(|| panic!("no response {id}"))
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

fn prompt(id: i64, session_id: &Value, text: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
        "params":{"sessionId":session_id,"prompt":[{"type":"text","text":text}]}})
}

/// The switch waits for the running turn; a `session/prompt` that follows it used
/// to block the ACP handler on the gate, so the permission answer that would
/// settle the turn was never read and the connection hung for good. Now the prompt
/// is parked: all three answer, and the parked prompt runs on the recreated query.
#[tokio::test]
async fn t3fd_a_prompt_behind_a_pending_switch_does_not_deadlock_the_connection() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::ZERO).await;
        let mut harness = Harness::start(
            "deadlock",
            &[
                vec![initialize_step(None), permission_turn()],
                vec![initialize_step(None), answering_turn("again")],
            ],
        );
        harness.initialize().await;
        let session_id = harness.open_session(2).await;
        harness
            .send_json(prompt(3, &session_id, "use a tool"))
            .await;
        let request_id = permission_request(&mut harness).await;
        harness.send_json(provider_set(4, &gateway)).await;
        harness.send_json(prompt(5, &session_id, "again")).await;
        harness.send_json(permission_answer(&request_id)).await;
        let frames = harness.read_responses(&[3, 4, 5]).await;
        assert_eq!(response_of(&frames, 3)["result"]["stopReason"], "end_turn");
        assert_eq!(response_of(&frames, 4)["result"], json!({}));
        assert_eq!(
            response_of(&frames, 5)["result"]["stopReason"],
            "end_turn",
            "the parked prompt runs on the recreated query: {frames:?}"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// A steer is what a client sends to a running turn: it never waits for a provider
/// switch that waits for that very turn.
#[tokio::test]
async fn t3fd_a_steer_returns_while_a_provider_switch_waits_for_its_turn() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::ZERO).await;
        // The injected message is a user frame the CLI reads before the interrupt:
        // give it a step of its own.
        let turn = permission_turn();
        let (first, _answer) = turn.split_once('\n').expect("two steps");
        let injected = json!({"expect": {"type": "user"}, "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "also this"}]}}
        ]})
        .to_string();
        let interrupted = json!({
            "expect": {"type": "control_request", "request": {"subtype": "interrupt"}},
            "emit": [
                {"type": "control_response", "response": {"subtype": "success", "request_id": "$REQ"}},
                {"type": "result", "subtype": "success", "stop_reason": "cancelled",
                 "is_error": false, "result": "",
                 "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
            ]
        })
        .to_string();
        let mut harness = Harness::start(
            "steer",
            &[
                vec![
                    initialize_step(None),
                    first.to_string(),
                    injected,
                    interrupted,
                ],
                vec![initialize_step(None)],
            ],
        );
        harness.initialize().await;
        let session_id = harness.open_session(2).await;
        harness.send_json(prompt(3, &session_id, "use a tool")).await;
        let _request_id = permission_request(&mut harness).await;
        harness.send_json(provider_set(4, &gateway)).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":5,"method":"_session/steering",
                "params":{"sessionId":session_id,
                    "prompt":[{"type":"text","text":"also this"}]}}))
            .await;
        // The steer answers while the turn still waits on the permission and the
        // switch still waits for the turn.
        let steer = harness.read_response(5).await;
        assert_eq!(steer["result"]["outcome"], "injected", "{steer}");
        harness
            .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
                "params":{"sessionId":session_id}}))
            .await;
        let frames = harness.read_responses(&[3, 4]).await;
        assert_eq!(response_of(&frames, 3)["result"]["stopReason"], "cancelled");
        assert_eq!(response_of(&frames, 4)["result"], json!({}));
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// No turn is running: the switch recreates the session while a prompt arrives. The
/// prompt waits and then runs on the recreated query, not on the closed one.
#[tokio::test]
async fn t3fd_a_prompt_after_a_switch_runs_on_the_recreated_query() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::from_millis(300)).await;
        let mut harness = Harness::start(
            "recreated",
            &[
                vec![initialize_step(None)],
                vec![initialize_step(None), answering_turn("hello")],
            ],
        );
        harness.initialize().await;
        let session_id = harness.open_session(2).await;
        harness.send_json(provider_set(3, &gateway)).await;
        harness.send_json(prompt(4, &session_id, "hello")).await;
        let frames = harness.read_responses(&[3, 4]).await;
        assert_eq!(response_of(&frames, 3)["result"], json!({}));
        assert_eq!(
            response_of(&frames, 4)["result"]["stopReason"],
            "end_turn",
            "{frames:?}"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// A session created while the switch lists the gateway starts under the routing
/// the switch installs (the CLI is launched with the gateway in its settings).
#[tokio::test]
async fn t3fd_a_session_created_during_a_switch_starts_under_the_new_routing() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::from_millis(500)).await;
        let mut harness = Harness::start("routing", &[vec![initialize_step(None)]]);
        harness.initialize().await;
        harness.send_json(provider_set(2, &gateway)).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/new",
                "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        let frames = harness.read_responses(&[2, 3]).await;
        assert_eq!(response_of(&frames, 2)["result"], json!({}));
        assert!(response_of(&frames, 3)["result"]["sessionId"].is_string());
        let argv = std::fs::read_to_string(harness.tmp.join("argv.json")).expect("argv record");
        assert!(
            argv.contains(&gateway),
            "the session was launched without the new routing: {argv}"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// A changed-params `session/resume` of one session recreates it off the handler
/// for as long as its query takes to start; another session's prompt is not held
/// back by it.
#[tokio::test]
async fn t3fd_a_slow_resume_does_not_hold_back_another_sessions_prompt() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start(
            "slowload",
            &[
                vec![initialize_step(None)],
                vec![initialize_step(None), answering_turn("hi b")],
                // The recreated query of the first session is slow to initialize.
                vec![initialize_step(Some(3000))],
            ],
        );
        harness.initialize().await;
        let first = harness.open_session(2).await;
        let second = harness.open_session(3).await;
        let other_cwd = harness.tmp.join("other");
        std::fs::create_dir_all(&other_cwd).expect("other cwd");
        harness
            .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/resume",
                "params":{"sessionId":first,"cwd":other_cwd.to_string_lossy(),"mcpServers":[]}}))
            .await;
        harness.send_json(prompt(5, &second, "hi b")).await;
        let frames = harness.read_responses(&[4, 5]).await;
        assert_eq!(
            frames[0].get("id").and_then(Value::as_i64),
            Some(5),
            "the prompt of the other session answers before the slow resume: {frames:?}"
        );
        assert_eq!(frames[0]["result"]["stopReason"], "end_turn");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// `providers.test.ts` > "switches every loaded session through proxy
/// replacements and back to native routing": each `providers/set` and the
/// `providers/disable` recreate both live sessions (the cursor advances by two
/// per update), and the last spawn carries native routing.
#[tokio::test]
async fn t9b_switches_every_loaded_session_through_replacements_and_back() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let first = stub_gateway(Duration::from_millis(0)).await;
        let second = stub_gateway(Duration::from_millis(0)).await;
        let segments: Vec<Vec<String>> = (0..8).map(|_| vec![initialize_step(None)]).collect();
        let mut harness = Harness::start("every-session", &segments);
        let cursor = |harness: &Harness| -> u64 {
            std::fs::read_to_string(harness.tmp.join("cursor"))
                .expect("cursor")
                .trim()
                .parse()
                .expect("cursor number")
        };
        harness.initialize().await;
        let _first_session = harness.open_session(2).await;
        let _second_session = harness.open_session(3).await;
        assert_eq!(cursor(&harness), 2);

        harness.send_json(provider_set(4, &first)).await;
        harness.read_response(4).await;
        assert_eq!(cursor(&harness), 4, "both sessions recreated");
        harness.send_json(provider_set(5, &second)).await;
        harness.read_response(5).await;
        assert_eq!(cursor(&harness), 6, "both sessions recreated again");
        harness
            .send_json(json!({"jsonrpc":"2.0","id":6,"method":"providers/disable",
                "params":{"providerId":"main"}}))
            .await;
        harness.read_response(6).await;
        assert_eq!(
            cursor(&harness),
            8,
            "both sessions recreated for native routing"
        );

        let argv = std::fs::read_to_string(harness.tmp.join("argv.json")).expect("argv record");
        assert!(!argv.contains(&first), "{argv}");
        assert!(!argv.contains(&second), "{argv}");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// `providers.test.ts` > "clears a providers/set that is still listing when
/// logout arrives": the logout's own update wins the gate, so the listing that
/// was in flight cannot leave the override installed.
#[tokio::test]
async fn t9b_logout_clears_a_provider_set_that_is_still_listing() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let slow = stub_gateway(Duration::from_millis(300)).await;
        let mut harness = Harness::start("logout-listing", &[vec![initialize_step(None)]]);
        std::fs::create_dir_all(harness.tmp.join("config")).expect("config dir");
        harness.initialize().await;
        harness.send_json(provider_set(2, &slow)).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"logout","params":{}}))
            .await;
        let frames = harness.read_responses(&[2, 3]).await;
        assert_eq!(response_of(&frames, 2)["result"], json!({}), "{frames:?}");
        assert_eq!(response_of(&frames, 3)["result"], json!({}), "{frames:?}");
        harness
            .send_json(json!({"jsonrpc":"2.0","id":4,"method":"providers/list","params":{}}))
            .await;
        let listed = harness.read_response(4).await;
        assert_eq!(
            listed["result"]["providers"][0]["current"],
            json!({"apiType": "anthropic", "baseUrl": "https://api.anthropic.com"}),
            "{listed}"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// `providers.test.ts` > "recreates live sessions without the cleared provider
/// on logout": the live session is respawned on native routing after logout.
#[tokio::test]
async fn t9b_logout_recreates_live_sessions_without_the_provider() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::from_millis(0)).await;
        let segments: Vec<Vec<String>> = (0..2).map(|_| vec![initialize_step(None)]).collect();
        let mut harness = Harness::start("logout-recreate", &segments);
        std::fs::create_dir_all(harness.tmp.join("config")).expect("config dir");
        let cursor = |harness: &Harness| -> u64 {
            std::fs::read_to_string(harness.tmp.join("cursor"))
                .expect("cursor")
                .trim()
                .parse()
                .expect("cursor number")
        };
        harness.initialize().await;
        harness.send_json(provider_set(2, &gateway)).await;
        harness.read_response(2).await;
        let _session = harness.open_session(3).await;
        assert_eq!(cursor(&harness), 1);
        harness
            .send_json(json!({"jsonrpc":"2.0","id":4,"method":"logout","params":{}}))
            .await;
        harness.read_response(4).await;
        assert_eq!(cursor(&harness), 2, "the live session was recreated");
        let argv = std::fs::read_to_string(harness.tmp.join("argv.json")).expect("argv record");
        assert!(!argv.contains(&gateway), "{argv}");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// `providers.test.ts` > "leaves live sessions alone on logout without a routing
/// override": no provider was set, so logout recreates nothing.
#[tokio::test]
async fn t9b_logout_leaves_live_sessions_alone_without_a_override() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let segments: Vec<Vec<String>> = (0..2).map(|_| vec![initialize_step(None)]).collect();
        let mut harness = Harness::start("logout-native", &segments);
        std::fs::create_dir_all(harness.tmp.join("config")).expect("config dir");
        let cursor = |harness: &Harness| -> u64 {
            std::fs::read_to_string(harness.tmp.join("cursor"))
                .expect("cursor")
                .trim()
                .parse()
                .expect("cursor number")
        };
        harness.initialize().await;
        let _session = harness.open_session(2).await;
        assert_eq!(cursor(&harness), 1);
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"logout","params":{}}))
            .await;
        harness.read_response(3).await;
        assert_eq!(cursor(&harness), 1, "no recreate without an override");
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// A turn that stays open until the CLI is interrupted.
fn open_turn(text: &str) -> String {
    json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": text}]}},
            {"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": "working"}
            ]}}
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

/// P2a F2: a cold `session/new` start must not stop the connection reading
/// `session/cancel` for another session. Session B's child start is held for 3 s
/// (a `sleep_ms` step in the fake CLI) while A's cancel is sent. A's prompt must
/// settle before B's `session/new` answers: before the fix the handler awaited
/// the start inline, so the cancel was only read after B answered.
#[tokio::test]
async fn t3fd_a_cold_session_start_does_not_delay_another_sessions_cancel() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start(
            "cold-cancel",
            &[
                vec![initialize_step(None), open_turn("work")],
                vec![initialize_step(Some(3000))],
            ],
        );
        harness.initialize().await;
        let a = harness.open_session(2).await;
        harness.send_json(prompt(3, &a, "work")).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/new",
                "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        harness
            .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
                "params":{"sessionId":a}}))
            .await;
        let frames = harness.read_responses(&[3, 4]).await;
        assert_eq!(
            frames[0]["id"], 3,
            "A's cancel was served while B's child started: {frames:?}"
        );
        assert_eq!(response_of(&frames, 3)["result"]["stopReason"], "cancelled");
        assert!(
            response_of(&frames, 4)["result"]["sessionId"].is_string(),
            "B's session/new answered"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// P2a F2: the same for a permission answer — a response frame the SDK routes to
/// the agent's own request. A cold `session/new` must not stop the connection
/// reading it while another session's turn waits on it.
#[tokio::test]
async fn t3fd_a_cold_session_start_does_not_delay_another_sessions_permission_answer() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start(
            "cold-permission",
            &[
                vec![initialize_step(None), permission_turn()],
                vec![initialize_step(Some(3000))],
            ],
        );
        harness.initialize().await;
        let a = harness.open_session(2).await;
        harness.send_json(prompt(3, &a, "use a tool")).await;
        let request_id = permission_request(&mut harness).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/new",
                "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        harness.send_json(permission_answer(&request_id)).await;
        let frames = harness.read_responses(&[3, 4]).await;
        assert_eq!(
            frames[0]["id"], 3,
            "A's permission answer was served while B's child started: {frames:?}"
        );
        assert_eq!(response_of(&frames, 3)["result"]["stopReason"], "end_turn");
        assert!(
            response_of(&frames, 4)["result"]["sessionId"].is_string(),
            "B's session/new answered"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// The registry entry of a spawned `session/new` start is applied before the
/// request that follows its response: the prompt runs on the new session instead
/// of answering "Session not found".
#[tokio::test]
async fn t3fd_a_prompt_after_a_cold_session_new_runs_on_the_registered_session() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start(
            "cold-prompt",
            &[vec![initialize_step(Some(500)), answering_turn("hello")]],
        );
        harness.initialize().await;
        let b = harness.open_session(2).await;
        harness.send_json(prompt(3, &b, "hello")).await;
        let response = harness.read_response(3).await;
        assert_eq!(
            response["result"]["stopReason"], "end_turn",
            "the prompt after the start runs on the new session: {response}"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}
