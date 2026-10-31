//! Turn 3 fix round 3, lane FE: a provider update recreates the sessions its
//! snapshot still owns, closed husks included (finding C), but not one another
//! path replaced while it waited (finding B). Hermetic duplex transport,
//! `fake-claude` transcript, every wait bounded.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, serve_hiding_claude_auth, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::net::TcpListener;
use tokio::time::timeout;

fn init_step(account: Value) -> String {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [{"value": "claude-sonnet-4-5", "displayName": "S", "description": "d"}],
                "commands": [], "account": account,
            }
        }}]
    })
    .to_string()
}

/// The turn signs the session out: the CLI answers with the login error, which
/// closes its query and leaves the session registered (a husk).
fn auth_fail_turn() -> String {
    json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "needs auth"}]}},
            {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Please sign in to proceed."}]}},
            {"type": "result", "subtype": "success", "stop_reason": "error", "is_error": true,
             "result": "Please run /login to continue", "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
        ]
    })
    .to_string()
}

/// A turn that stays in flight until the client answers the permission request,
/// then ends.
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

/// The CLI's answer to the archive's `rename_session` (`query.renameSession`).
fn rename_step() -> String {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "rename_session"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ", "response": {}
        }}]
    })
    .to_string()
}

/// The `interrupt` the archive sends before the rename (`session.cancel()`).
fn interrupt_step() -> String {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "interrupt"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {"still_queued": []}
        }}]
    })
    .to_string()
}

fn write_transcript(tmp: &Path, segments: &[Vec<String>]) -> PathBuf {
    let path = tmp.join("t3fe-recreate.transcript.jsonl");
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
        "{\"auth_status_stdout\":{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\",\"apiKeySource\":\"ANTHROPIC_API_KEY\"}}\n",
    );
    std::fs::write(&path, text).expect("write transcript");
    path
}

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t3fe-recreate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

fn options(tmp: &Path, segments: &[Vec<String>]) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                write_transcript(tmp, segments)
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
            force_cancel_grace: Duration::from_millis(200),
            ..Timings::default()
        },
        env: EnvSnapshot::empty(),
        logger: Some(std::sync::Arc::new(
            claude_agent_acp_rs::logger::Logger::to_file(tmp.join("agent.log")),
        )),
    }
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
    fn start(name: &str, segments: &[Vec<String>], hide_claude_auth: bool) -> Self {
        let tmp = temp_dir(name);
        std::fs::create_dir_all(tmp.join("config")).expect("config dir");
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = options(&tmp, segments);
        let task = tokio::spawn(async move {
            if hide_claude_auth {
                serve_hiding_claude_auth(WireTransport::new(agent_in, agent_out), opts).await
            } else {
                serve(WireTransport::new(agent_in, agent_out), opts).await
            }
        });
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

    async fn initialize(&mut self) {
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1}}))
            .await;
        self.read_response(1).await;
    }

    /// An AIR client that declared `sessionIndex`: `_session/archive` is served.
    async fn initialize_air(&mut self) {
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1,
                "clientCapabilities":{"_meta":{"jetbrains":{"air":{
                    "version":1,"capabilities":["sessionIndex"]}}}}}}))
            .await;
        self.read_response(1).await;
    }

    async fn open_session(&mut self, id: i64) -> Value {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        self.read_response(id).await["result"]["sessionId"].clone()
    }

    /// The permission request the turn waits on: its request id.
    async fn permission_request(&mut self) -> Value {
        loop {
            let frame = self.read_json().await;
            if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                return frame["id"].clone();
            }
        }
    }

    fn spawns(&self) -> String {
        std::fs::read_to_string(self.tmp.join("cursor")).unwrap_or_default()
    }

    /// Blocks until the provider update has snapshotted the sessions and is
    /// waiting for a running turn: the agent logs that moment.
    async fn wait_for_update_wait(&self) {
        let log = self.tmp.join("agent.log");
        timeout(Duration::from_secs(20), async {
            loop {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                if text.contains("Waiting for active Claude turn(s) before provider update") {
                    return;
                }
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the update never started waiting for the turn");
    }

    async fn finish(self) {
        drop(self.input);
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn provider_set(id: i64, gateway: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"providers/set",
        "params":{"providerId":"main","apiType":"anthropic","baseUrl":gateway}})
}

fn prompt(id: i64, session_id: &Value, text: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
        "params":{"sessionId":session_id,"prompt":[{"type":"text","text":text}]}})
}

fn permission_answer(request_id: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":request_id,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}})
}

fn response_of(frames: &[Value], id: i64) -> &Value {
    frames
        .iter()
        .find(|frame| frame.get("id").and_then(Value::as_i64) == Some(id))
        .unwrap_or_else(|| panic!("no response {id}"))
}

/// Finding C: a session a sign-out left as a closed husk is recreated by the
/// provider update, like upstream's `this.sessions[id] === session` loop does.
/// The old filter dropped it, so no query was started for it.
#[tokio::test]
async fn t3fe_a_closed_husk_is_recreated_by_the_update() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::ZERO).await;
        let mut harness = Harness::start(
            "husk",
            &[
                vec![
                    init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
                    auth_fail_turn(),
                ],
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
            ],
            true,
        );
        harness.initialize().await;
        let session_id = harness.open_session(2).await;
        harness
            .send_json(prompt(3, &session_id, "needs auth"))
            .await;
        let signed_out = harness.read_response(3).await;
        assert_eq!(signed_out["error"]["code"], -32000, "{signed_out}");
        // Let the sign-out's close finish: the husk's command channel is closed
        // when the update snapshots it.
        tokio::time::sleep(Duration::from_millis(300)).await;

        harness.send_json(provider_set(4, &gateway)).await;
        let switched = harness.read_response(4).await;
        assert_eq!(switched["result"], json!({}), "{switched}");
        assert_eq!(
            harness.spawns().trim(),
            "2",
            "the closed husk was not recreated by the update"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// Finding B: a session a steer respawned (V-16) while the update waited is no
/// longer the one the update snapshotted, so the update must skip it instead of
/// closing the fresh query and replacing it again.
#[tokio::test]
async fn t3fe_a_session_replaced_while_the_update_waits_is_not_recreated_again() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::ZERO).await;
        let mut harness = Harness::start(
            "identity",
            &[
                // The signed-out session.
                vec![
                    init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
                    auth_fail_turn(),
                ],
                // The other session's turn, which the update waits for.
                vec![
                    init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
                    permission_turn(),
                ],
                // The steer's respawn of the signed-out session.
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
                // The update's recreate of the other session.
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
            ],
            true,
        );
        harness.initialize().await;
        let signed_out = harness.open_session(2).await;
        let busy = harness.open_session(3).await;
        harness
            .send_json(prompt(4, &signed_out, "needs auth"))
            .await;
        let failed = harness.read_response(4).await;
        assert_eq!(failed["error"]["code"], -32000, "{failed}");

        harness.send_json(prompt(5, &busy, "use a tool")).await;
        let permission = harness.permission_request().await;
        harness.send_json(provider_set(6, &gateway)).await;
        // The update snapshotted the sessions and now waits for the other
        // session's turn; the steer respawns the signed-out session in that
        // window.
        harness.wait_for_update_wait().await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":7,"method":"_session/steering",
                "params":{"sessionId":signed_out,
                    "prompt":[{"type":"text","text":"also this"}],
                    "_meta":{"steering":{"idleBehavior":"promptRequired"}}}}))
            .await;
        let steer = harness.read_response(7).await;
        assert_eq!(steer["result"]["outcome"], "promptRequired", "{steer}");
        harness.send_json(permission_answer(&permission)).await;
        let frames = harness.read_responses(&[5, 6]).await;
        assert_eq!(response_of(&frames, 5)["result"]["stopReason"], "end_turn");
        assert_eq!(response_of(&frames, 6)["result"], json!({}), "{frames:?}");
        assert_eq!(
            harness.spawns().trim(),
            "4",
            "the replaced session was recreated a second time"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// Finding N1: a session the client closed while the update waited is gone for
/// good; close removes it from the published view before it answers, so the
/// update's identity check fails and it starts no child for it.
#[tokio::test]
async fn t3ff_a_session_closed_while_the_update_waits_is_not_recreated() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::ZERO).await;
        let mut harness = Harness::start(
            "closed",
            &[
                // Session A, whose turn the update waits for.
                vec![
                    init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
                    permission_turn(),
                ],
                // Session B, idle until the client closes it.
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
                // The update's recreate of A (B must not get one).
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
                // A recreate of B, which must never be started.
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
            ],
            true,
        );
        harness.initialize().await;
        let busy = harness.open_session(2).await;
        let closing = harness.open_session(3).await;
        harness.send_json(prompt(4, &busy, "use a tool")).await;
        let permission = harness.permission_request().await;
        harness.send_json(provider_set(5, &gateway)).await;
        harness.wait_for_update_wait().await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":6,"method":"session/close",
                "params":{"sessionId":closing}}))
            .await;
        harness.send_json(permission_answer(&permission)).await;
        let frames = harness.read_responses(&[4, 5, 6]).await;
        assert_eq!(response_of(&frames, 4)["result"]["stopReason"], "end_turn");
        assert_eq!(response_of(&frames, 5)["result"], json!({}), "{frames:?}");
        assert!(response_of(&frames, 6).get("error").is_none(), "{frames:?}");
        assert_eq!(
            harness.spawns().trim(),
            "3",
            "the closed session was recreated by the update"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}

/// Finding F1: `_session/archive` tears the session down as well, so a session
/// the client archived while the update waited must not be recreated from the
/// stale view entry. Without removing it from the view on close, the update
/// starts a child for an archived session.
#[tokio::test]
async fn t3ff_a_session_archived_while_the_update_waits_is_not_recreated() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let gateway = stub_gateway(Duration::ZERO).await;
        let mut harness = Harness::start(
            "archived",
            &[
                // Session A, whose turn the update waits for.
                vec![
                    init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
                    permission_turn(),
                ],
                // Session B, idle until the client archives it.
                vec![
                    init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
                    interrupt_step(),
                    rename_step(),
                ],
                // The update's recreate of A (B must not get one).
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
                // A recreate of B, which must never be started.
                vec![init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"}))],
            ],
            true,
        );
        harness.initialize_air().await;
        let busy = harness.open_session(2).await;
        let archiving = harness.open_session(3).await;
        harness.send_json(prompt(4, &busy, "use a tool")).await;
        let permission = harness.permission_request().await;
        harness.send_json(provider_set(5, &gateway)).await;
        harness.wait_for_update_wait().await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":6,"method":"_session/archive",
                "params":{"sessionId":archiving}}))
            .await;
        let archived = harness.read_response(6).await;
        assert_eq!(archived["result"], json!({}), "{archived}");
        harness.send_json(permission_answer(&permission)).await;
        let frames = harness.read_responses(&[4, 5]).await;
        assert_eq!(response_of(&frames, 4)["result"]["stopReason"], "end_turn");
        assert_eq!(response_of(&frames, 5)["result"], json!({}), "{frames:?}");
        assert_eq!(
            harness.spawns().trim(),
            "3",
            "the archived session was recreated by the update"
        );
        harness.finish().await;
    })
    .await
    .expect("the exchange finished in time");
}
