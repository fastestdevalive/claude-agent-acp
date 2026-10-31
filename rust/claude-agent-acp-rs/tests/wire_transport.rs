//! F2c (M-126 / M-52) — transport-level wire parity over a hermetic duplex
//! pipe.
//!
//! The recorder cannot express a raw malformed line or a `$/cancel_request`
//! notification: its `request` step always awaits a response, which a
//! notification never sends. Those two wire cases are therefore covered here
//! by writing raw JSON-RPC lines into the agent's stdin and reading its stdout
//! through [`WireTransport`] (the same wrapper the binary installs).

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

/// One running agent over a duplex pipe, plus the raw client ends.
struct Harness {
    /// client -> agent bytes.
    input: DuplexStream,
    /// agent -> client bytes, line-buffered.
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
}

impl Harness {
    /// Start `serve(WireTransport::new(...))` with the fake `claude` binary.
    fn start(transcript: Option<&str>) -> Self {
        let root = repo_root();
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let mut extra_env = Vec::new();
        if let Some(name) = transcript {
            extra_env.push((
                "FAKE_CLAUDE_SCRIPT".to_string(),
                root.join(format!("porting/corpus/{name}.transcript.jsonl"))
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env,
            default_cwd: Some(root),
            timings: Timings {
                // Match the differential harness: a cancel that needs the
                // force-cancel backstop settles quickly.
                force_cancel_grace: Duration::from_millis(1000),
                ..Timings::default()
            },
            // Hermetic: never read the host environment.
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

    /// Write one raw line (no trailing newline supplied by the caller).
    async fn send_raw(&mut self, line: &str) {
        self.input
            .write_all(line.as_bytes())
            .await
            .expect("write line");
        self.input.write_all(b"\n").await.expect("write newline");
        self.input.flush().await.expect("flush");
    }

    /// Write one JSON-RPC value as a line.
    async fn send_json(&mut self, value: Value) {
        self.send_raw(&serde_json::to_string(&value).expect("serialize"))
            .await;
    }

    /// Read one line of agent output and parse it.
    async fn read_json(&mut self) -> Value {
        let mut line = String::new();
        let n = timeout(Duration::from_secs(15), self.reader.read_line(&mut line))
            .await
            .expect("agent output timed out")
            .expect("read agent line");
        assert!(n > 0, "agent closed stdout");
        serde_json::from_str(&line).expect("agent line is JSON")
    }

    /// Read until the response to `id` arrives (skipping notifications and
    /// agent->client requests).
    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.read_json().await;
            let matches_id = frame.get("id").and_then(Value::as_i64) == Some(id);
            let is_response = frame.get("method").is_none();
            if matches_id && is_response {
                return frame;
            }
        }
    }

    /// `initialize` with client request id 1.
    async fn initialize(&mut self) {
        self.send_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1}
        }))
        .await;
        let response = self.read_response(1).await;
        assert_eq!(response["result"]["protocolVersion"], 1);
    }

    /// `session/new` with client request id 2; returns the session id.
    async fn new_session(&mut self) -> Value {
        self.send_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": repo_root().to_string_lossy(), "mcpServers": []}
        }))
        .await;
        let response = self.read_response(2).await;
        response["result"]["sessionId"].clone()
    }

    /// Stop the agent and release the pipe.
    async fn shutdown(self) {
        drop(self.input);
        let _ = timeout(Duration::from_secs(5), self.task).await;
    }
}

/// M-126 — a malformed line and non-record JSON are answered exactly like
/// Node's `ndJsonStream`, and the stream keeps working afterwards.
#[tokio::test]
async fn wire_parse_error_shapes_match_node() {
    let mut harness = Harness::start(None);

    harness.send_raw("this is not json").await;
    let reply = harness.read_json().await;
    assert_eq!(
        reply,
        json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": "Parse error"}
        }),
        "a malformed line must produce Node's -32700 shape with no data"
    );

    harness.send_raw("42").await;
    let reply = harness.read_json().await;
    assert_eq!(
        reply,
        json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32600, "message": "Invalid request", "data": 42}
        }),
        "non-record JSON must produce Node's -32600 shape with the value in data"
    );

    // Blank lines are skipped and do not desynchronise the stream.
    harness.send_raw("   ").await;
    harness.initialize().await;
    harness.shutdown().await;
}

/// M-126 — `session/request_permission` carries Node's NUMBER id (the first
/// outgoing request is `0`), and the client's numeric reply reaches the crate.
#[tokio::test]
async fn wire_permission_request_id_is_numeric() {
    let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
    let mut harness = Harness::start(Some("permission-allow"));
    harness.initialize().await;
    let session_id = harness.new_session().await;

    harness
        .send_json(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "use a tool"}]
            }
        }))
        .await;

    let permission = loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            break frame;
        }
    };
    assert_eq!(
        permission["id"],
        json!(0),
        "Node allocates outgoing request ids as numbers starting at 0: {permission}"
    );

    // Answer with the same numeric id; the wrapper maps it back to the crate's
    // uuid and the permission round-trip completes.
    harness
        .send_json(json!({
            "jsonrpc": "2.0",
            "id": permission["id"].clone(),
            "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}
        }))
        .await;
    let response = harness.read_response(3).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    harness.shutdown().await;
}

/// M-52 — a `$/cancel_request` that arrives after its prompt settled is
/// ignored: the cancellation watch was dropped with the reply, so it cannot
/// cancel a later turn (Node removes the abort listener in `finally`).
#[tokio::test]
async fn wire_late_cancel_request_is_ignored() {
    let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
    let mut harness = Harness::start(Some("tool-then-text"));
    harness.initialize().await;
    let session_id = harness.new_session().await;

    harness
        .send_json(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "read a file"}]
            }
        }))
        .await;
    let response = harness.read_response(3).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");

    // Late cancel for the settled request id.
    harness
        .send_json(json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": {"requestId": 3}
        }))
        .await;

    // The next turn still runs to completion: fake-claude would abort on a
    // stray interrupt control_request, so a spurious cancel fails the script.
    harness
        .send_json(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "say done"}]
            }
        }))
        .await;
    let response = harness.read_response(4).await;
    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "a late cancel must not affect the next turn: {response}"
    );
    harness.shutdown().await;
}

/// M-52 / WB-05 — `$/cancel_request` for an in-flight prompt routes to the
/// same cancel path as `session/cancel`: the CLI is interrupted and the prompt
/// resolves `cancelled`.
#[tokio::test]
async fn wire_cancel_request_cancels_the_prompt() {
    let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
    let mut harness = Harness::start(Some("cancel-mid-turn"));
    harness.initialize().await;
    let session_id = harness.new_session().await;

    harness
        .send_json(json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "long running"}]
            }
        }))
        .await;

    // Wait for the turn to be streaming before cancelling.
    loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/update") {
            break;
        }
    }

    // The generic protocol-level cancellation for the prompt's request id.
    harness
        .send_json(json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": {"requestId": 42}
        }))
        .await;

    let response = harness.read_response(42).await;
    assert_eq!(
        response["result"]["stopReason"], "cancelled",
        "the prompt must settle cancelled, not run to completion: {response}"
    );
    harness.shutdown().await;
}
