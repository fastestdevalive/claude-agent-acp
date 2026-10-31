//! T7B STEP 4 — v2 child sessions and requests (B-115; upstream
//! `src/tests/acp-v2.test.ts:3714-4300`): a subagent is announced as a child
//! session, a foreground child's request holds the parent in `requires_action`,
//! and the child's own stream carries its states.
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
    let tmp = std::env::temp_dir().join(format!("acp-t7b-child-{tag}-{}", std::process::id()));
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
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":2,
                "info":{"name":"v2-test-client","version":"1.0.0"},
                "capabilities":{"auth":{"terminal":{}}}}}))
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

    /// Read until a `state_update` `idle` of `session_id`.
    async fn read_until_idle(&mut self, session_id: &str) -> Vec<Value> {
        let mut updates = Vec::new();
        loop {
            let frame = self.read_json().await;
            assert_eq!(
                frame.get("method").and_then(Value::as_str),
                Some("session/update"),
                "only updates until the turn is idle: {frame}"
            );
            let on = frame["params"]["sessionId"].as_str().unwrap_or("");
            let update = frame["params"]["update"].clone();
            let idle = update["sessionUpdate"] == "state_update"
                && update["state"] == "idle"
                && on == session_id;
            updates.push(json!({"sessionId": on, "update": update}));
            if idle {
                return updates;
            }
        }
    }

    /// Read raw frames until the root's `idle`, including notifications such as
    /// `$/cancel_request`.
    async fn read_until_root_idle_raw(&mut self, session_id: &str) -> Vec<Value> {
        let mut frames = Vec::new();
        loop {
            let frame = self.read_json().await;
            let idle = frame.get("method").and_then(Value::as_str) == Some("session/update")
                && frame["params"]["sessionId"].as_str() == Some(session_id)
                && frame["params"]["update"]["sessionUpdate"] == "state_update"
                && frame["params"]["update"]["state"] == "idle";
            frames.push(frame);
            if idle {
                return frames;
            }
        }
    }

    async fn shutdown(mut self) {
        let _ = self.input.shutdown().await;
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// A transcript for one turn whose foreground subagent asks for permission.
fn foreground_child_request_steps() -> Vec<Value> {
    vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
                 "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}},
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "m1", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_agent-1", "name": "Agent",
                        "input": {"description": "Explore the project", "prompt": "Look around"}}]}},
                {"type": "system", "subtype": "task_started", "task_id": "agent-1",
                 "tool_use_id": "toolu_agent-1", "description": "Explore the project",
                 "subagent_type": "Explore", "is_backgrounded": false, "prompt": "Look around"},
                {"type": "assistant", "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "Exploring."}]}},
                {"type": "control_request", "request_id": "perm-child-1", "request": {
                    "subtype": "can_use_tool", "tool_name": "Bash",
                    "input": {"command": "ls"}, "tool_use_id": "toolu_ls_agent-1",
                    "agent_id": "agent-1", "permission_suggestions": []}},
            ],
        }),
        json!({
            "expect": {"type": "control_response",
                "response": {"response": {"behavior": "allow"}}},
            "emit": [
                {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ls_agent-1", "content": "a.txt"}]}},
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                 "is_error": false, "result": "done",
                 "usage": {"input_tokens": 1, "output_tokens": 1}},
            ],
        }),
    ]
}

/// B-115 — the child is announced as a session with `cancel`, its foreground
/// request puts the parent in `requires_action` before the request (which goes
/// to the child's session), and both streams end idle.
#[tokio::test]
async fn b115_v2_child_announced_and_foreground_request() {
    let mut harness = Harness::start("child-request", &foreground_child_request_steps());
    let root = harness.open_session().await;
    let root_id = root.as_str().unwrap_or_default().to_string();

    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":root,"prompt":[{"type":"text","text":"go"}]}}))
        .await;

    let mut updates = Vec::new();
    let request = loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            break frame;
        }
        if frame.get("id").and_then(Value::as_i64) == Some(3) && frame.get("method").is_none() {
            assert!(frame["result"]["messageId"].is_string(), "{frame}");
            continue;
        }
        let on = frame["params"]["sessionId"]
            .as_str()
            .unwrap_or("")
            .to_string();
        updates.push(json!({"sessionId": on, "update": frame["params"]["update"].clone()}));
    };

    // The child announcement: a `subagent_update` on the root, naming the child,
    // with the cancel capability and `running`.
    let announcement = updates
        .iter()
        .find(|u| u["update"]["sessionUpdate"] == "subagent_update")
        .expect("the child is announced");
    assert_eq!(announcement["sessionId"], root_id);
    let child = announcement["update"]["sessionId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!child.is_empty() && child != root_id, "{announcement}");
    assert_eq!(
        announcement["update"]["capabilities"],
        json!({"cancel": {}})
    );
    assert_eq!(announcement["update"]["state"], json!({"state": "running"}));

    // The child's own stream carries its running state after the announcement.
    assert!(
        updates.iter().any(|u| u["sessionId"] == child
            && u["update"]["sessionUpdate"] == "state_update"
            && u["update"]["state"] == "running"),
        "the child stream has its running state: {updates:?}"
    );

    // The foreground child's request holds the parent: `requires_action` is the
    // last state before the request.
    let last_root_state = updates
        .iter()
        .rev()
        .find(|u| u["sessionId"] == root_id && u["update"]["sessionUpdate"] == "state_update")
        .expect("a root state");
    assert_eq!(
        last_root_state["update"]["state"], "requires_action",
        "{updates:?}"
    );
    // The request is on the child's session, and the turn is still answered.
    assert_eq!(request["params"]["sessionId"], child, "{request}");
    // P-51: a v2 permission request carries a title and a tool-call subject.
    assert!(
        request["params"]["title"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "a v2 permission title: {request}"
    );
    assert_eq!(
        request["params"]["subject"]["type"], "tool_call",
        "{request}"
    );
    assert_eq!(
        request["params"]["subject"]["toolCall"]["toolCallId"], "toolu_ls_agent-1",
        "{request}"
    );

    harness
        .send_json(json!({"jsonrpc":"2.0","id":request["id"],
            "result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}))
        .await;
    let idle = harness.read_until_idle(&root_id).await;
    let root_states: Vec<&str> = idle
        .iter()
        .filter(|u| u["sessionId"] == root_id && u["update"]["sessionUpdate"] == "state_update")
        .filter_map(|u| u["update"]["state"].as_str())
        .collect();
    assert_eq!(root_states, vec!["running", "idle"], "{idle:?}");
    assert_eq!(
        idle.iter()
            .rev()
            .find(|u| u["sessionId"] == root_id && u["update"]["sessionUpdate"] == "state_update")
            .expect("root idle")["update"]["stopReason"],
        "end_turn"
    );
    // The child's running state was mirrored to its own stream; this scenario
    // does not report a child idle (upstream reports one only for a state the
    // CLI publishes for the child).
    let child_states: Vec<&str> = idle
        .iter()
        .filter(|u| u["sessionId"] == child && u["update"]["sessionUpdate"] == "state_update")
        .filter_map(|u| u["update"]["state"].as_str())
        .collect();
    assert_eq!(child_states, vec!["running"], "{idle:?}");
    harness.shutdown().await;
}

/// The transcript of a turn whose foreground child's request the client holds
/// and the root cancel withdraws. The CLI's interrupt aborts the request, so
/// the adapter sends it the "Tool use aborted" error and Claude Code stops the
/// child.
fn withdrawn_child_request_steps() -> Vec<Value> {
    vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
                 "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}},
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "m1", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_agent-1", "name": "Agent",
                        "input": {"description": "Explore the project", "prompt": "Look around"}}]}},
                {"type": "system", "subtype": "task_started", "task_id": "agent-1",
                 "tool_use_id": "toolu_agent-1", "description": "Explore the project",
                 "subagent_type": "Explore", "is_backgrounded": false, "prompt": "Look around"},
                {"type": "assistant", "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "Exploring."}]}},
                {"type": "control_request", "request_id": "perm-child-1", "request": {
                    "subtype": "can_use_tool", "tool_name": "Bash",
                    "input": {"command": "ls"}, "tool_use_id": "toolu_ls",
                    "agent_id": "agent-1", "permission_suggestions": []}},
            ],
        }),
        json!({
            "expect": {"type": "control_response", "response": {
                "subtype": "error", "request_id": "perm-child-1",
                "error": "Tool use aborted"}},
            "emit": [
                {"type": "system", "subtype": "task_updated", "task_id": "agent-1",
                 "patch": {"status": "killed"}},
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                 "is_error": false, "result": "stopped",
                 "usage": {"input_tokens": 1, "output_tokens": 1}},
            ],
        }),
    ]
}

/// B-115 — a cancel withdraws the child's open request: the request is
/// cancelled on the wire, the child reports `requires_action` then `idle
/// cancelled` on its own stream, and the turn ends `idle cancelled` with no
/// `running` in between.
#[tokio::test]
async fn b115_v2_cancel_withdraws_a_foreground_child_request() {
    let mut harness = Harness::start("child-withdraw", &withdrawn_child_request_steps());
    let root = harness.open_session().await;
    let root_id = root.as_str().unwrap_or_default().to_string();

    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":root,"prompt":[{"type":"text","text":"go"}]}}))
        .await;

    let mut updates = Vec::new();
    let request = loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            break frame;
        }
        if frame.get("id").and_then(Value::as_i64) == Some(3) && frame.get("method").is_none() {
            continue;
        }
        let on = frame["params"]["sessionId"]
            .as_str()
            .unwrap_or("")
            .to_string();
        updates.push(json!({"sessionId": on, "update": frame["params"]["update"].clone()}));
    };
    let child = request["params"]["sessionId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_ne!(child, root_id);

    harness
        .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
            "params":{"sessionId":root}}))
        .await;
    let mut frames = harness.read_until_root_idle_raw(&root_id).await;
    // The withdrawal may be queued after the turn's last update; give it a
    // bounded moment.
    while let Ok(frame) =
        tokio::time::timeout(Duration::from_millis(300), harness.read_json()).await
    {
        frames.push(frame);
    }

    // The open request is withdrawn on the wire.

    let withdrawn = frames
        .iter()
        .find(|f| f.get("method").and_then(Value::as_str) == Some("$/cancel_request"))
        .expect("the open request is cancelled");
    assert_eq!(
        withdrawn["params"]["requestId"], request["id"],
        "{withdrawn}"
    );

    let mut all = updates;
    for frame in &frames {
        if frame.get("method").and_then(Value::as_str) == Some("session/update") {
            all.push(json!({
                "sessionId": frame["params"]["sessionId"].as_str().unwrap_or(""),
                "update": frame["params"]["update"].clone(),
            }));
        }
    }

    let root_states: Vec<&str> = all
        .iter()
        .filter(|u| u["sessionId"] == root_id && u["update"]["sessionUpdate"] == "state_update")
        .filter_map(|u| u["update"]["state"].as_str())
        .collect();
    assert_eq!(
        root_states,
        vec!["running", "requires_action", "idle"],
        "{all:?}"
    );
    let root_idle = all
        .iter()
        .rev()
        .find(|u| u["sessionId"] == root_id && u["update"]["sessionUpdate"] == "state_update")
        .expect("root idle");
    assert_eq!(root_idle["update"]["stopReason"], "cancelled");

    // The child's own stream: running, requires_action, then idle. (The
    // cancelled stop reason comes from the CLI's `task_updated`; this harness
    // settles the turn on the interrupt before that event, so the RFD's
    // `finishAll` reports the child idle without a stop reason, as upstream
    // does for a child it did not observe ending.)
    let child_states: Vec<String> = all
        .iter()
        .filter(|u| u["sessionId"] == child && u["update"]["sessionUpdate"] == "state_update")
        .map(|u| {
            let state = u["update"]["state"].as_str().unwrap_or("");
            match u["update"]["stopReason"].as_str() {
                Some(reason) => format!("{state} {reason}"),
                None => state.to_string(),
            }
        })
        .collect();
    assert_eq!(
        child_states,
        vec!["running", "requires_action", "idle"],
        "{all:?}"
    );

    // The parent's copy of the child's states: running, requires_action, idle.
    let copies: Vec<String> = all
        .iter()
        .filter(|u| u["sessionId"] == root_id && u["update"]["sessionUpdate"] == "subagent_update")
        .filter_map(|u| {
            let state = u["update"]["state"]["state"].as_str()?;
            Some(match u["update"]["state"]["stopReason"].as_str() {
                Some(reason) => format!("{state} {reason}"),
                None => state.to_string(),
            })
        })
        .collect();
    assert_eq!(
        copies,
        vec!["running", "requires_action", "idle"],
        "{all:?}"
    );
    harness.shutdown().await;
}
