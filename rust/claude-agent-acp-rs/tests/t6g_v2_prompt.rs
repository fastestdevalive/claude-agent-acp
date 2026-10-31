//! T6G STEP 3/6/7 — the ACP v2 prompt path end to end over the real wire
//! (M-113, M-114, Q-86, C-24, B-112; upstream `src/tests/acp-v2.test.ts`).
//!
//! The connection negotiates ACP v2 in `initialize`, creates a session against
//! the local `fake-claude`, and prompts it. The frames are read as they are, so
//! the order is asserted exactly: the prompt's `user_message`, the `running`
//! state, the intake answer, the turn's chunks in their v2 form, and the `idle`
//! state with its stop reason and usage.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use claude_agent_acp_rs::session_store::{project_slug, projects_root_with_env};
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
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

fn echo(text: &str) -> Value {
    json!({"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

fn message_start() -> Value {
    json!({"type": "stream_event", "parent_tool_use_id": null, "event": {
        "type": "message_start",
        "message": {"id": "msg_1", "model": "claude-sonnet-4-5",
            "usage": {"input_tokens": 100, "output_tokens": 0,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}}})
}

fn delta(text: &str) -> Value {
    json!({"type": "stream_event", "parent_tool_use_id": null, "event": {
        "type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": text}}})
}

fn result(text: &str) -> Value {
    json!({
        "type": "result", "subtype": "success", "stop_reason": "end_turn",
        "is_error": false, "result": text,
        "usage": {"input_tokens": 100, "output_tokens": 4,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        "modelUsage": {"claude-sonnet-4-5": {"contextWindow": 200_000}}
    })
}

/// A transcript whose first prompt streams `hi` and ends `end_turn`; a second
/// prompt streams without a result (the cancel test settles it).
fn write_transcript(tmp: &Path, steps: &[Value]) -> PathBuf {
    let path = tmp.join("t6g-v2-prompt.transcript.jsonl");
    let mut text = String::new();
    for step in steps {
        text.push_str(&serde_json::to_string(step).expect("serialize step"));
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write transcript");
    path
}

/// One prompt that streams `hi` and ends `end_turn`.
fn answered_prompt_steps() -> Vec<Value> {
    vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("hi"), message_start(), delta("hi"), result("hi")],
        }),
    ]
}

/// One prompt that runs a Bash tool and a TodoWrite plan, then ends.
fn tool_prompt_steps() -> Vec<Value> {
    vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("run it"),
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_t", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                        "input": {"command": "ls"}}]}},
                {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"}]}},
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_p", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_2", "name": "TodoWrite",
                        "input": {"todos": [{"content": "ship", "status": "pending",
                            "activeForm": "shipping"}]}}]}},
                result("done"),
            ],
        }),
    ]
}

/// One prompt whose Bash tool asks the client for permission.
fn permission_prompt_steps() -> Vec<Value> {
    vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("run it"),
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_t", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                        "input": {"command": "ls"}}]}},
                {"type": "control_request", "request_id": "permreq1", "request": {
                    "subtype": "can_use_tool", "tool_name": "Bash",
                    "input": {"command": "ls"}, "tool_use_id": "toolu_1",
                    "agent_id": null, "permission_suggestions": []}},
            ],
        }),
        json!({
            "expect": {"type": "control_response",
                "response": {"response": {"behavior": "allow"}}},
            "emit": [
                {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"}]}},
                result("done"),
            ],
        }),
    ]
}

/// One prompt that streams without a result, so a cancel settles it.
fn unanswered_prompt_steps() -> Vec<Value> {
    vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("stop"), message_start(), delta("stopping")],
        }),
    ]
}

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str, steps: &[Value]) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t6g-v2-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let config_dir = tmp.join("claude");
        std::fs::create_dir_all(&config_dir).expect("config dir");
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
                    config_dir.to_string_lossy().into_owned(),
                ),
            ],
            default_cwd: Some(repo_root()),
            timings: Timings {
                force_cancel_grace: Duration::from_millis(1000),
                ..Timings::default()
            },
            env: EnvSnapshot::from_pairs([
                (EXPERIMENTAL_V2_ENV.to_string(), "1".to_string()),
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    config_dir.to_string_lossy().into_owned(),
                ),
            ]),
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

    /// v2 `initialize`, then `session/new`; returns the session id.
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
        assert!(created["result"].get("modes").is_none(), "{created}");
        created["result"]["sessionId"].clone()
    }

    /// Send a v2 prompt and read the frames up to its answer, returning the
    /// answer and the updates that preceded it.
    async fn prompt_until_answer(
        &mut self,
        id: i64,
        text: &str,
        session_id: &Value,
    ) -> (Value, Vec<Value>) {
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
            assert_eq!(
                frame.get("method").and_then(Value::as_str),
                Some("session/update"),
                "only session updates precede the answer: {frame}"
            );
            updates.push(frame["params"]["update"].clone());
        }
    }

    /// Read until the `idle` state of the turn, collecting every update.
    async fn read_until_idle(&mut self) -> Vec<Value> {
        let mut updates = Vec::new();
        loop {
            let frame = self.read_json().await;
            assert_eq!(
                frame.get("method").and_then(Value::as_str),
                Some("session/update"),
                "a turn reports its updates only: {frame}"
            );
            let update = frame["params"]["update"].clone();
            let idle = update["sessionUpdate"] == "state_update" && update["state"] == "idle";
            updates.push(update);
            if idle {
                return updates;
            }
        }
    }

    /// Seeds a stored session transcript in the projects directory of the
    /// harness's config dir.
    fn seed_session(&self, session_id: &str) {
        let config_dir = self.tmp.join("claude");
        let env = EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.to_string_lossy().into_owned(),
        )]);
        let cwd = repo_root();
        let dir = projects_root_with_env(&env).join(project_slug(&cwd.to_string_lossy()));
        std::fs::create_dir_all(&dir).expect("project dir");
        let body = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":session_id,
                "timestamp":"2026-01-01T00:00:00.000Z",
                "message":{"role":"user","content":"one"}})
            .to_string(),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":session_id,
                "timestamp":"2026-01-01T00:00:01.000Z",
                "message":{"id":"m1","role":"assistant",
                    "content":[{"type":"text","text":"first"}]}})
            .to_string(),
        ]
        .join("\n")
            + "\n";
        std::fs::write(dir.join(format!("{session_id}.jsonl")), body).expect("seed transcript");
    }

    async fn shutdown(mut self) {
        drop(std::mem::replace(&mut self.input, tokio::io::duplex(1).0));
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// M-113/M-114/U-81/Q-86/C-24 — a v2 prompt is answered at intake and the
/// turn's wire order is exactly `user_message`, `running`, answer, chunks,
/// `idle` with the stop reason and usage.
#[tokio::test]
async fn t6g_v2_prompt_wire_order() {
    let mut harness = Harness::start("order", &answered_prompt_steps());
    let session_id = harness.open_session().await;

    let (answer, before) = harness.prompt_until_answer(3, "hi", &session_id).await;
    let message_id = answer["result"]["messageId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!message_id.is_empty(), "an intake message id: {answer}");

    // 1. The user message and the running state precede the answer (other
    //    session updates of the turn may come before them, as on v1).
    let user_message = before
        .iter()
        .position(|update| update["sessionUpdate"] == "user_message")
        .expect("the prompt's user_message");
    let running = before
        .iter()
        .position(|update| {
            update["sessionUpdate"] == "state_update" && update["state"] == "running"
        })
        .expect("the running state");
    assert_eq!(before[user_message]["messageId"], message_id);
    assert_eq!(before[user_message]["content"][0]["text"], "hi");
    assert!(
        user_message < running,
        "the user_message precedes the running state: {before:?}"
    );
    assert!(
        before
            .iter()
            .all(|update| update["sessionUpdate"] != "agent_message_chunk"),
        "no chunk precedes the answer: {before:?}"
    );

    // 2. The turn streams as v2 chunks (with a messageId) and ends `idle`.
    let after = harness.read_until_idle().await;
    let chunk = after
        .iter()
        .find(|update| update["sessionUpdate"] == "agent_message_chunk")
        .expect("a streamed chunk");
    assert_eq!(
        chunk["messageId"], "msg_1",
        "the chunk carries its message id"
    );
    assert_eq!(chunk["content"]["text"], "hi");
    let idle = after.last().expect("an idle");
    assert_eq!(idle["stopReason"], "end_turn");
    assert_eq!(idle["usage"]["inputTokens"], 100);
    assert_eq!(idle["usage"]["outputTokens"], 4);
    assert_eq!(idle["usage"]["totalTokens"], 104);
    // No v1-shaped tool call or mode update reached the v2 client.
    assert!(
        after
            .iter()
            .all(|update| update["sessionUpdate"] != "current_mode_update"),
        "a v2 client gets no current_mode_update: {after:?}"
    );
    harness.shutdown().await;
}

/// M-113/M-117 — a v2 cancel ends the turn `idle cancelled`, and the session
/// answers a later request (the connection was never blocked by the turn).
#[tokio::test]
async fn t6g_v2_cancel_ends_the_turn_and_the_connection_serves_on() {
    let mut harness = Harness::start("cancel", &unanswered_prompt_steps());
    let session_id = harness.open_session().await;
    let (answer, _) = harness.prompt_until_answer(3, "stop", &session_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");

    harness
        .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
            "params":{"sessionId":session_id}}))
        .await;
    let updates = harness.read_until_idle().await;
    let idle = updates.last().expect("an idle");
    assert_eq!(idle["stopReason"], "cancelled", "{idle}");

    // The connection still serves: a second session starts and answers.
    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
        .await;
    let second = harness.read_response(4).await;
    assert!(second.get("error").is_none(), "{second}");
    let second_id = second["result"]["sessionId"].clone();
    assert_ne!(second_id, session_id);
    let (answer, before) = harness.prompt_until_answer(5, "hi", &second_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    assert_eq!(before[0]["sessionUpdate"], "user_message");
    harness.shutdown().await;
}

/// C-24/U-83/T-101/T-102 — a v2 client gets the turn's tool calls as
/// `tool_call_update` upserts (never a v1 `tool_call`) and its plan as a
/// `plan_update`.
#[tokio::test]
async fn t6g_v2_tool_calls_and_plan_are_translated() {
    let mut harness = Harness::start("tools", &tool_prompt_steps());
    let session_id = harness.open_session().await;
    let (answer, _) = harness.prompt_until_answer(3, "run it", &session_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let updates = harness.read_until_idle().await;
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|update| update["sessionUpdate"].as_str())
        .collect();
    assert!(
        !kinds.contains(&"tool_call"),
        "a v2 client never gets a v1 tool_call: {kinds:?}"
    );
    let bash = updates
        .iter()
        .find(|update| update["toolCallId"] == "toolu_1")
        .expect("the Bash tool call");
    assert_eq!(bash["sessionUpdate"], "tool_call_update");
    assert_eq!(bash["title"], "ls");
    let completed = updates
        .iter()
        .find(|update| update["toolCallId"] == "toolu_1" && update["status"] == "completed")
        .expect("the completed Bash tool call");
    assert_eq!(completed["sessionUpdate"], "tool_call_update");
    let plan = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "plan_update")
        .expect("the plan update");
    assert_eq!(plan["plan"]["planId"], "plan");
    assert_eq!(plan["plan"]["type"], "items");
    assert_eq!(plan["plan"]["entries"][0]["content"], "ship");
    assert!(
        kinds.iter().all(|kind| *kind != "current_mode_update"),
        "no current_mode_update on v2: {kinds:?}"
    );
    harness.shutdown().await;
}

/// P-61/P-62 — a v2 permission request carries a `title` and a `subject`, and
/// its `selected` answer maps back to a v1 allow.
#[tokio::test]
async fn t6g_v2_permission_request_and_answer() {
    let mut harness = Harness::start("permission", &permission_prompt_steps());
    let session_id = harness.open_session().await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"run it"}]}}))
        .await;
    let request = loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            break frame;
        }
        // The prompt is answered at intake, before the tool asks.
        if frame.get("id").and_then(Value::as_i64) == Some(3) && frame.get("method").is_none() {
            assert!(frame["result"]["messageId"].is_string(), "{frame}");
            continue;
        }
        assert_eq!(
            frame.get("method").and_then(Value::as_str),
            Some("session/update"),
            "only session updates precede the request: {frame}"
        );
    };
    let params = &request["params"];
    assert!(
        params["title"]
            .as_str()
            .is_some_and(|title| !title.is_empty()),
        "a v2 request has a title: {params}"
    );
    assert_eq!(params["subject"]["type"], "tool_call");
    assert_eq!(params["subject"]["toolCall"]["toolCallId"], "toolu_1");
    assert!(
        params["subject"]["toolCall"].get("sessionUpdate").is_none(),
        "the subject is a patch, not an update: {params}"
    );
    // The v2 `selected` outcome is the v1 allow.
    harness
        .send_json(json!({"jsonrpc":"2.0","id":request["id"],
            "result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}))
        .await;
    // The turn ends once the tool ran: the answered request reports `running`
    // again, then the result ends it.
    let idle = harness.read_until_idle().await;
    assert_eq!(
        idle.last().expect("an idle")["stopReason"],
        "end_turn",
        "{idle:?}"
    );
    assert!(
        idle
            .iter()
            .any(|update| update["sessionUpdate"] == "state_update"
                && update["state"] == "running"),
        "the answered request resumes the work: {idle:?}"
    );
    harness.shutdown().await;
}

/// T6B HIGH 3 — a v2 `session/resume` whose params changed starts the session
/// off the registry actor, so a `session/close` sent right after it is served
/// while that start runs and cancels it: the resume answers "session ended"
/// instead of registering the recreated session.
#[tokio::test]
async fn t6g_a_v2_resume_recreate_is_cancelled_by_a_close() {
    let mut harness = Harness::start("recreate", &answered_prompt_steps());
    let session_id = harness.open_session().await;
    // A different cwd changes the session's fingerprint: `session/resume` takes
    // the recreate path.
    let other_cwd = std::env::temp_dir()
        .join(format!("acp-t6g-other-cwd-{}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::create_dir_all(&other_cwd);
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/resume",
            "params":{"sessionId":session_id,"cwd":other_cwd,"mcpServers":[]}}))
        .await;
    // Without awaiting the resume, close the session: the close must be served
    // now and cancel the recreate.
    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/close",
            "params":{"sessionId":session_id}}))
        .await;
    let mut resume = None;
    let mut close = None;
    while resume.is_none() || close.is_none() {
        let frame = harness.read_json().await;
        match frame.get("id").and_then(Value::as_i64) {
            Some(3) => resume = Some(frame),
            Some(4) => close = Some(frame),
            _ => {}
        }
    }
    let close = close.expect("the close answered");
    assert_eq!(close["result"], json!({}), "the close answered: {close}");
    let resume = resume.expect("the resume answered");
    let error = resume
        .get("error")
        .unwrap_or_else(|| panic!("the cancelled recreate answers an error: {resume}"));
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("session ended") || message.contains("ended")),
        "the resume reports the ended session: {error}"
    );
    let _ = std::fs::remove_dir_all(&other_cwd);
    harness.shutdown().await;
}

/// B-112 — a v2 `session/resume` with `replayFrom: start` replays the history
/// through the v2 translation: each message is cleared before its first
/// replayed chunk (`replaying`, `v2/agent.ts:209-241`).
#[tokio::test]
async fn t6g_v2_replay_clears_each_message_before_its_first_chunk() {
    let mut harness = Harness::start("replay", &answered_prompt_steps());
    let source = "cccccccc-3333-4333-8333-cccccccccccc";
    harness.seed_session(source);
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":2,
                "info":{"name":"v2-test-client","version":"1.0.0"},
                "capabilities":{"auth":{"terminal":{}}}}}))
        .await;
    harness.read_response(1).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/resume",
            "params":{"sessionId":source,"cwd":repo_root().to_string_lossy(),
                "replayFrom":{"type":"start"}}}))
        .await;
    let mut updates = Vec::new();
    loop {
        let frame = harness.read_json().await;
        if frame.get("id").and_then(Value::as_i64) == Some(2) && frame.get("method").is_none() {
            assert!(frame.get("error").is_none(), "the resume failed: {frame}");
            break;
        }
        if frame.get("method").and_then(Value::as_str) == Some("session/update") {
            updates.push(frame["params"]["update"].clone());
        }
    }
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|update| update["sessionUpdate"].as_str())
        .collect();
    let first_chunk = updates
        .iter()
        .position(|update| update["sessionUpdate"] == "user_message_chunk")
        .expect("the user message replayed as a chunk");
    let clear = updates
        .iter()
        .position(|update| update["sessionUpdate"] == "user_message")
        .expect("the message was cleared before its chunk");
    assert!(
        clear < first_chunk,
        "the clear precedes the chunk: {kinds:?}"
    );
    assert_eq!(updates[clear]["content"], json!([]));
    assert_eq!(
        updates[clear]["messageId"],
        updates[first_chunk]["messageId"]
    );
    // The replayed agent message is cleared too.
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "agent_message"),
        "the agent message was cleared: {kinds:?}"
    );
    harness.shutdown().await;
}
