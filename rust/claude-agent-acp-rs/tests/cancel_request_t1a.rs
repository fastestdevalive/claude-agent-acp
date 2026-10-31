//! F3b (U-107, P-03) — an in-flight permission request that `session/cancel`
//! aborts is withdrawn from the client with an outbound `$/cancel_request`
//! (`requestPermission(params, signal)`, `acp-agent.ts:2374-2390, 9783-9830`).
//!
//! The recorder cannot observe an agent->client `$/cancel_request`
//! notification, so this drives the real `serve` over a hermetic duplex pipe and
//! reads the agent's raw JSON-RPC lines.

mod common;

use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::timeout;

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
}

impl Harness {
    fn start(transcript: &str) -> Self {
        let root = repo_root();
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![(
                "FAKE_CLAUDE_SCRIPT".to_string(),
                root.join(format!("porting/corpus/{transcript}.transcript.jsonl"))
                    .to_string_lossy()
                    .into_owned(),
            )],
            default_cwd: Some(root),
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
        let n = timeout(Duration::from_secs(15), self.reader.read_line(&mut line))
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
}

/// An open permission request that `session/cancel` withdraws reaches the client
/// as a `$/cancel_request` naming that request's id, so the client can dismiss
/// its prompt; the prompt itself settles `cancelled`.
#[tokio::test]
async fn u107_cancel_sends_cancel_request_for_the_open_permission() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start("permission-allow");
        harness
            .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":1}}))
            .await;
        harness.read_response(1).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
                "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        let session_id = harness.read_response(2).await["result"]["sessionId"].clone();
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
                "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"use a tool"}]}}))
            .await;

        // The agent asks the client for permission; the client never answers.
        let request_id = loop {
            let frame = harness.read_json().await;
            if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                break frame["id"].clone();
            }
        };

        harness
            .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
                "params":{"sessionId":session_id}}))
            .await;

        let mut cancel_request = None;
        let mut prompt_response = None;
        while cancel_request.is_none() || prompt_response.is_none() {
            let frame = harness.read_json().await;
            if frame.get("method").and_then(Value::as_str) == Some("$/cancel_request") {
                cancel_request = Some(frame);
            } else if frame.get("id").and_then(Value::as_i64) == Some(3)
                && frame.get("method").is_none()
            {
                prompt_response = Some(frame);
            }
        }
        let cancel_request = cancel_request.expect("a $/cancel_request");
        assert_eq!(
            cancel_request["params"]["requestId"], request_id,
            "the notification names the withdrawn permission request: {cancel_request}"
        );
        assert!(
            cancel_request.get("id").is_none(),
            "$/cancel_request is a notification"
        );
        assert_eq!(
            prompt_response.expect("the prompt settles")["result"]["stopReason"],
            "cancelled"
        );

        drop(harness.input);
        let _ = timeout(Duration::from_secs(10), harness.task).await;
    })
    .await
    .expect("u107 test timed out");
}
