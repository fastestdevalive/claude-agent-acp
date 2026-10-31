//! T6G2: the v2 behaviour fixes of turn 6, fix round 2, over the real wire
//! (NEW-1, NEW-2, NEW-3, NEW-6, NEW-8, T-105, B-52; upstream
//! `src/tests/acp-v2.test.ts`).
//!
//! The connection negotiates ACP v2 in `initialize`, creates or resumes a
//! session against the local `fake-claude`, and prompts it. The frames are read
//! as they are, so their order is asserted exactly.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::serve;
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use claude_agent_acp_rs::session_store::{project_slug, projects_root_with_env};
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use claude_agent_acp_rs::ServeOptions;
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

fn write_transcript(tmp: &Path, name: &str, steps: &[Value]) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    let mut text = String::new();
    for step in steps {
        text.push_str(&serde_json::to_string(step).expect("serialize step"));
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write transcript");
    path
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
        Self::start_with(name, steps, false)
    }

    /// A connection that hides the claude.ai login (`--hide-claude-auth`).
    fn start_hiding_auth(name: &str, steps: &[Value]) -> Self {
        Self::start_with(name, steps, true)
    }

    fn start_with(name: &str, steps: &[Value], hide_claude_auth: bool) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t6g2-v2-{name}-{}", std::process::id()));
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
                    write_transcript(&tmp, name, steps)
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    config_dir.to_string_lossy().into_owned(),
                ),
                // A recreated child continues the transcript where the previous
                // one stopped (`segment` steps).
                (
                    "FAKE_CLAUDE_CURSOR".to_string(),
                    tmp.join("cursor").to_string_lossy().into_owned(),
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
        let transport = WireTransport::new(agent_in, agent_out);
        let task = tokio::spawn(async move {
            if hide_claude_auth {
                claude_agent_acp_rs::agent::serve_hiding_claude_auth(transport, opts).await
            } else {
                serve(transport, opts).await
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

    /// The child transcript and meta of a subagent, next to `session_id`'s
    /// transcript.
    fn seed_child(&self, session_id: &str, agent: &str, prompt: &str, text: &str) {
        let config_dir = self.tmp.join("claude");
        let env = EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.to_string_lossy().into_owned(),
        )]);
        let cwd = repo_root();
        let dir = projects_root_with_env(&env)
            .join(project_slug(&cwd.to_string_lossy()))
            .join(session_id)
            .join("subagents");
        std::fs::create_dir_all(&dir).expect("subagents dir");
        let child = [
            json!({"type":"user","uuid":"c1","parentUuid":null,"sessionId":session_id,
                "isSidechain":true,"agentId":agent,"cwd":cwd.to_string_lossy(),
                "timestamp":"2026-01-01T00:00:00.000Z",
                "message":{"role":"user","content":prompt}}),
            json!({"type":"assistant","uuid":"c2","parentUuid":"c1","sessionId":session_id,
                "isSidechain":true,"agentId":agent,"cwd":cwd.to_string_lossy(),
                "timestamp":"2026-01-01T00:00:01.000Z",
                "message":{"id":"msg_child","role":"assistant",
                    "content":[{"type":"text","text":text}]}}),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        std::fs::write(dir.join(format!("agent-{agent}.jsonl")), child).expect("child transcript");
        std::fs::write(
            dir.join(format!("agent-{agent}.meta.json")),
            json!({"agentType":"Explore","name":"scout","description":"Look around",
                "toolUseId":"toolu_agent"})
            .to_string(),
        )
        .expect("meta");
    }

    /// The `session/list` row of `session_id`.
    async fn list_row(&mut self, id: i64, session_id: &str) -> Value {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":"session/list",
            "params":{"cwd":repo_root().to_string_lossy()}}))
            .await;
        let listed = self.read_response(id).await;
        listed["result"]["sessions"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["sessionId"] == json!(session_id))
                    .cloned()
            })
            .unwrap_or_else(|| panic!("the session is listed: {listed}"))
    }

    /// Seeds a stored session transcript in the projects directory of the
    /// harness's config dir.
    fn seed_session(&self, session_id: &str, lines: &[Value]) {
        let config_dir = self.tmp.join("claude");
        let env = EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.to_string_lossy().into_owned(),
        )]);
        let cwd = repo_root();
        let dir = projects_root_with_env(&env).join(project_slug(&cwd.to_string_lossy()));
        std::fs::create_dir_all(&dir).expect("project dir");
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(dir.join(format!("{session_id}.jsonl")), body).expect("seed transcript");
    }

    async fn shutdown(mut self) {
        drop(std::mem::replace(&mut self.input, tokio::io::duplex(1).0));
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn kinds(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|update| {
            let kind = update["sessionUpdate"].as_str().unwrap_or("");
            match kind {
                "state_update" => match update["stopReason"].as_str() {
                    Some(reason) => format!("{} {reason}", update["state"].as_str().unwrap_or("")),
                    None => update["state"].as_str().unwrap_or("").to_string(),
                },
                _ => kind.to_string(),
            }
        })
        .collect()
}

/// NEW-1 (`acp-v2.test.ts`: "gives a messageId to the chunks that the agent
/// builds from a command's output and a result"): a turn that answers only in
/// its `result` reaches a v2 client as a chunk before the `idle`, grouped under
/// the result frame's id.
#[tokio::test]
async fn t6g2_result_only_text_reaches_a_v2_client() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("hi"),
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                 "is_error": false, "result": "ONLY-IN-RESULT", "uuid": "res-uuid-1",
                 "usage": {"input_tokens": 0, "output_tokens": 0}},
            ],
        }),
    ];
    let mut harness = Harness::start("result-text", &steps);
    let session_id = harness.open_session().await;
    let (answer, before) = harness.prompt_until_answer(3, "hi", &session_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    assert_eq!(
        kinds(&before),
        vec!["user_message", "running"],
        "the prompt's message and state precede the answer: {before:?}"
    );
    let after = harness.read_until_idle().await;
    assert_eq!(
        kinds(&after),
        vec!["agent_message_chunk", "idle end_turn"],
        "the result text precedes the idle: {after:?}"
    );
    let chunk = &after[0];
    assert_eq!(chunk["content"]["text"], "ONLY-IN-RESULT");
    assert_eq!(
        chunk["messageId"], "res-uuid-1",
        "the chunk is grouped under the result's uuid"
    );
    assert_eq!(
        serde_json::to_string(chunk).unwrap(),
        r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ONLY-IN-RESULT"},"messageId":"res-uuid-1"}"#
    );
    harness.shutdown().await;
}

/// NEW-2 (`acp-agent.ts:3824-3838`): `/remote-control` while a turn runs is
/// refused with `invalidRequest` BEFORE any `user_message`.
#[tokio::test]
async fn t6g2_a_busy_session_refuses_remote_control_before_the_user_message() {
    let mut harness = Harness::start("remote-busy", &unanswered_prompt_steps());
    let session_id = harness.open_session().await;
    let (answer, _) = harness.prompt_until_answer(3, "stop", &session_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");

    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt",
            "params":{"sessionId":session_id,
                "prompt":[{"type":"text","text":"/remote-control"}]}}))
        .await;
    let mut updates = Vec::new();
    let refusal = loop {
        let frame = harness.read_json().await;
        if frame.get("id").and_then(Value::as_i64) == Some(4) && frame.get("method").is_none() {
            break frame;
        }
        if let Some(update) = frame.get("params").and_then(|p| p.get("update")) {
            updates.push(update.clone());
        }
    };
    let error = refusal
        .get("error")
        .unwrap_or_else(|| panic!("the busy command is refused: {refusal}"));
    assert_eq!(error["code"], -32600, "invalidRequest: {error}");
    assert_eq!(
        error["message"],
        "Remote Control can't be changed while Claude is working. Try again when the turn ends."
    );
    assert!(
        updates
            .iter()
            .all(|update| update["sessionUpdate"] != "user_message"),
        "no user_message for a refused command: {updates:?}"
    );
    harness.shutdown().await;
}

/// NEW-2: an idle session accepts `/remote-control`: the `user_message` and the
/// answer follow the command's control round-trip.
#[tokio::test]
async fn t6g2_an_idle_session_accepts_remote_control() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "control_request",
                "request": {"subtype": "remote_control", "enabled": true}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ",
                "response": {"session_url": "https://claude.ai/code/session-1"}
            }}],
        }),
    ];
    let mut harness = Harness::start("remote-idle", &steps);
    let session_id = harness.open_session().await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,
                "prompt":[{"type":"text","text":"/remote-control"}]}}))
        .await;
    let mut updates = Vec::new();
    let answer = loop {
        let frame = harness.read_json().await;
        if frame.get("id").and_then(Value::as_i64) == Some(3) && frame.get("method").is_none() {
            break frame;
        }
        if let Some(update) = frame.get("params").and_then(|p| p.get("update")) {
            updates.push(update.clone());
        }
    };
    assert!(
        answer.get("error").is_none(),
        "the command was accepted: {answer}"
    );
    let message_id = answer["result"]["messageId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!message_id.is_empty(), "{answer}");
    assert_eq!(
        kinds(&updates),
        vec!["user_message"],
        "the accepted command echoes the prompt: {updates:?}"
    );
    assert_eq!(updates[0]["messageId"], message_id);
    assert_eq!(updates[0]["content"][0]["text"], "/remote-control");
    harness.shutdown().await;
}

/// NEW-6 (`withPendingUserInput`, `v2/agent.ts:338-349`): `requires_action`
/// precedes the permission request, which precedes every later update.
#[tokio::test]
async fn t6g2_requires_action_precedes_the_permission_request() {
    let mut harness = Harness::start("permission-order", &permission_prompt_steps());
    let session_id = harness.open_session().await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"run it"}]}}))
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
        let update = frame["params"]["update"].clone();
        updates.push(update);
    };
    assert_eq!(
        updates.last().map(|update| update["state"].as_str()),
        Some(Some("requires_action")),
        "requires_action is the last update before the request: {updates:?}"
    );
    assert_eq!(
        updates
            .iter()
            .filter(|update| update["state"] == "requires_action")
            .count(),
        1,
        "one requires_action: {updates:?}"
    );
    // The request keeps its FIFO position: the answered request's own updates
    // come after it.
    harness
        .send_json(json!({"jsonrpc":"2.0","id":request["id"],
            "result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}))
        .await;
    let idle = harness.read_until_idle().await;
    assert_eq!(idle.last().expect("an idle")["stopReason"], "end_turn");
    harness.shutdown().await;
}

/// T-105 (`acp-agent.ts:9958-10031`, `11284-11291`): a v2 connection's
/// PreToolUse hook (`hook_0`) shows the exact patch of an Edit before it runs,
/// and the streamed input does not replace it.
#[tokio::test]
async fn t6g2_a_v2_pretool_use_hook_shows_the_patch_before_the_edit() {
    let tmp = std::env::temp_dir().join(format!("acp-t6g2-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let file = tmp.join("app.ts");
    std::fs::write(&file, "line 1\nconst value = 1;\nline 3\n").expect("write file");
    let input = json!({
        "file_path": file.to_string_lossy(),
        "old_string": "const value = 1;",
        "new_string": "const value = 2;",
    });
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("edit it"),
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_edit", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_edit", "name": "Edit",
                        "input": input}]}},
                {"type": "control_request", "request_id": "hookreq1", "request": {
                    "subtype": "hook_callback", "callback_id": "hook_0",
                    "tool_use_id": "toolu_edit",
                    "input": {"hook_event_name": "PreToolUse", "tool_name": "Edit",
                        "tool_input": input, "tool_use_id": "toolu_edit"}}},
            ],
        }),
        json!({
            "expect": {"type": "control_response",
                "response": {"request_id": "hookreq1", "response": {"continue": true}}},
            "emit": [
                {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_edit",
                        "content": "The file has been updated."}]}},
                result("done"),
            ],
        }),
    ];
    let mut harness = Harness::start("pretooluse", &steps);
    let session_id = harness.open_session().await;
    let (answer, _) = harness.prompt_until_answer(3, "edit it", &session_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let updates = harness.read_until_idle().await;
    let edits: Vec<&Value> = updates
        .iter()
        .filter(|update| update["toolCallId"] == "toolu_edit")
        .collect();
    let with_patch = edits
        .iter()
        .position(|update| {
            update["content"]
                .as_array()
                .is_some_and(|content| content.iter().any(|item| item.get("changes").is_some()))
        })
        .unwrap_or_else(|| panic!("the Edit shows its exact patch: {updates:?}"));
    let patch = edits[with_patch]["content"][0]["patch"]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        patch.contains("-const value = 1;\n+const value = 2;"),
        "the patch precedes the Edit: {patch}"
    );
    let completed = edits
        .iter()
        .position(|update| update["status"] == "completed")
        .unwrap_or_else(|| panic!("the Edit completed: {updates:?}"));
    assert!(
        with_patch < completed,
        "the patch arrives before the result: {updates:?}"
    );
    // The streamed Edit's own `tool_call` is a v1 diff, which a v2 client
    // cannot take (`v2ToolCallContent`): it is dropped until the v2 diff
    // plumbing of the tool-call shape lands (see lane-G2-notes.md).
    assert!(
        edits[with_patch + 1..]
            .iter()
            .all(|update| update.get("content").is_none()),
        "the stream did not replace the patch: {updates:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
    harness.shutdown().await;
}

/// M-130: a v2 AIR client that declared `sessionIndex` is not routed through the
/// index (`sessionIndex.negotiate(request, {v2})` enables nothing), so its
/// `session/list` is the plain SDK list: the row carries no
/// `_meta.jetbrains.air.state`.
#[tokio::test]
async fn t6g2_a_v2_session_index_is_not_routed() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("stop"), message_start(), delta("stopping")],
        }),
    ];
    let mut harness = Harness::start("index-v2", &steps);
    let source = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
    harness.seed_session(source, &seeded_transcript(source));
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":2,
            "info":{"name":"v2-test-client","version":"1.0.0"},
            "capabilities":{
                "auth":{"terminal":{}},
                "_meta":{"jetbrains":{"air":{
                    "version":1, "capabilities":["sessionIndex"]}}}
            }}}))
        .await;
    harness.read_response(1).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/resume",
            "params":{"sessionId":source,"cwd":repo_root().to_string_lossy()}}))
        .await;
    let resumed = harness.read_response(2).await;
    assert!(
        resumed.get("error").is_none(),
        "the resume failed: {resumed}"
    );
    let row = harness.list_row(3, source).await;
    assert!(
        row["_meta"].pointer("/jetbrains/air/state").is_none(),
        "ACP v2 routes no session index, so the row carries no air state: {row}"
    );
    assert_eq!(row["sessionId"], json!(source), "{row}");
    harness.shutdown().await;
}

/// `acp-v2.test.ts` > "reports a turn that fails after it was taken in as idle
/// with the error stop reason": the v2 `idle` carries `stopReason: "error"`.
/// ACP v2 routes no session index (M-130), so the row carries no air state; the
/// NEW-3 bookkeeping itself is covered on v1.
#[tokio::test]
async fn t6g2_a_failed_v2_turn_marks_its_row_error() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("fail"),
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                 "is_error": true, "result": "API Error: 529 Overloaded",
                 "usage": {"input_tokens": 10, "output_tokens": 2}},
            ],
        }),
    ];
    let mut harness = Harness::start("index-failed-v2", &steps);
    let source = "dddddddd-4444-4444-8444-dddddddddddd";
    harness.seed_session(source, &seeded_transcript(source));
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":2,
            "info":{"name":"v2-test-client","version":"1.0.0"},
            "capabilities":{
                "auth":{"terminal":{}},
                "_meta":{"jetbrains":{"air":{
                    "version":1, "capabilities":["sessionIndex"]}}}
            }}}))
        .await;
    harness.read_response(1).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/resume",
            "params":{"sessionId":source,"cwd":repo_root().to_string_lossy()}}))
        .await;
    let resumed = harness.read_response(2).await;
    assert!(
        resumed.get("error").is_none(),
        "the resume failed: {resumed}"
    );
    let (answer, _) = harness.prompt_until_answer(3, "fail", &json!(source)).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let idle = harness.read_until_idle().await;
    assert_eq!(
        idle.last().expect("an idle")["stopReason"],
        "error",
        "the failed turn's idle: {idle:?}"
    );
    let row = harness.list_row(4, source).await;
    assert!(
        row["_meta"].pointer("/jetbrains/air/state").is_none(),
        "ACP v2 routes no session index, so the row carries no air state: {row}"
    );
    harness.shutdown().await;
}

/// NEW-3 (`noteSessionState`, `acp-agent.ts:5561, 5572`): a turn's row is
/// `running` while it runs and `idle` when it settled. The session index is an
/// AIR extension, so the client declares it; ACP v2 routes no index (M-130), so
/// this runs on v1.
#[tokio::test]
async fn t6g2_a_v1_turn_runs_the_session_index_bookkeeping() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("stop"), message_start(), delta("stopping")],
        }),
    ];
    let mut harness = Harness::start("index-v1", &steps);
    let source = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
    harness.seed_session(source, &seeded_transcript(source));
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":1,
            "clientCapabilities":{
                "_meta":{"jetbrains":{"air":{
                    "version":1, "capabilities":["sessionIndex"]}}}
            }}}))
        .await;
    harness.read_response(1).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/resume",
            "params":{"sessionId":source,"cwd":repo_root().to_string_lossy()}}))
        .await;
    let resumed = harness.read_response(2).await;
    assert!(
        resumed.get("error").is_none(),
        "the resume failed: {resumed}"
    );

    // Prompt without waiting for its answer: the row says `running`.
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":source,"prompt":[{"type":"text","text":"stop"}]}}))
        .await;
    let row = harness.list_row(4, source).await;
    assert_eq!(
        row["_meta"]["jetbrains"]["air"]["state"], "running",
        "the turn's row is running: {row}"
    );

    // A cancel ends it: the row is idle again.
    harness
        .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
            "params":{"sessionId":source}}))
        .await;
    let answer = harness.read_response(3).await;
    assert_eq!(
        answer["result"]["stopReason"], "cancelled",
        "the cancel settles the turn: {answer}"
    );
    let row = harness.list_row(5, source).await;
    assert_eq!(
        row["_meta"]["jetbrains"]["air"]["state"], "idle",
        "the settled turn's row is idle: {row}"
    );
    harness.shutdown().await;
}

/// NEW-3: a turn that failed marks its row `error`
/// (`turn_settled(failed)`, `acp-agent.ts:5572`).
#[tokio::test]
async fn t6g2_a_v1_failed_turn_marks_its_row_error() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("fail"),
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                 "is_error": true, "result": "API Error: 529 Overloaded",
                 "usage": {"input_tokens": 10, "output_tokens": 2}},
            ],
        }),
    ];
    let mut harness = Harness::start("index-failed-v1", &steps);
    let source = "dddddddd-4444-4444-8444-dddddddddddd";
    harness.seed_session(source, &seeded_transcript(source));
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":1,
            "clientCapabilities":{
                "_meta":{"jetbrains":{"air":{
                    "version":1, "capabilities":["sessionIndex"]}}}
            }}}))
        .await;
    harness.read_response(1).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/resume",
            "params":{"sessionId":source,"cwd":repo_root().to_string_lossy()}}))
        .await;
    let resumed = harness.read_response(2).await;
    assert!(
        resumed.get("error").is_none(),
        "the resume failed: {resumed}"
    );
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":source,"prompt":[{"type":"text","text":"fail"}]}}))
        .await;
    let answer = harness.read_response(3).await;
    assert!(answer.get("error").is_some(), "the turn fails: {answer}");
    let row = harness.list_row(4, source).await;
    assert_eq!(
        row["_meta"]["jetbrains"]["air"]["state"], "error",
        "a failed turn's row is error: {row}"
    );
    harness.shutdown().await;
}

/// The seeded transcript a resume needs: one user and one assistant message.
fn seeded_transcript(session_id: &str) -> Vec<Value> {
    vec![
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":session_id,
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"one"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":session_id,
            "timestamp":"2026-01-01T00:00:01.000Z",
            "message":{"id":"m1","role":"assistant",
                "content":[{"type":"text","text":"first"}]}}),
    ]
}

/// B-52: a live `session/load` replays the history's subagents into their own
/// sessions, as `start_resumed` does (`acp-agent.ts:3156-3160`). The RFD form
/// is v2's baseline, so every v2 client gets it.
#[tokio::test]
async fn t6g2_a_live_load_replays_a_background_child() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("hi"), message_start(), delta("hi"), result("hi")],
        }),
    ];
    let mut harness = Harness::start("live-load", &steps);
    let session_id = harness.open_session().await;
    let sid = session_id.as_str().unwrap_or_default().to_string();
    let (answer, _) = harness.prompt_until_answer(3, "hi", &session_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let _ = harness.read_until_idle().await;

    // The stored history of the live session: one Agent launch, with the
    // child's own transcript next to it.
    harness.seed_session(
        &sid,
        &[
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":sid,
                "cwd":repo_root().to_string_lossy(),
                "timestamp":"2026-01-01T00:00:00.000Z",
                "message":{"role":"user","content":"Explore it"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":sid,
                "cwd":repo_root().to_string_lossy(),
                "timestamp":"2026-01-01T00:00:01.000Z",
                "message":{"id":"m1","role":"assistant","content":[{
                    "type":"tool_use","id":"toolu_agent","name":"Agent",
                    "input":{"name":"scout","subagent_type":"Explore",
                        "description":"Look around","prompt":"Look around"}}]}}),
        ],
    );
    harness.seed_child(&sid, "abc123", "Look around", "Child report.");

    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/resume",
            "params":{"sessionId":sid,"cwd":repo_root().to_string_lossy(),
                "replayFrom":{"type":"start"}}}))
        .await;
    let mut updates = Vec::new();
    loop {
        let frame = harness.read_json().await;
        if frame.get("id").and_then(Value::as_i64) == Some(4) && frame.get("method").is_none() {
            assert!(frame.get("error").is_none(), "the load failed: {frame}");
            break;
        }
        if let Some(update) = frame.get("params").and_then(|p| p.get("update")) {
            updates.push(update.clone());
        }
    }
    let announce = updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "subagent_update"
                && update["sessionId"] == "abc123"
                && update["state"]["state"] == "running"
        })
        .unwrap_or_else(|| panic!("the child is announced: {updates:?}"));
    assert_eq!(announce["title"], "scout (Explore)");
    let child_text = updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "Child report."
        })
        .unwrap_or_else(|| panic!("the child's own message is replayed: {updates:?}"));
    assert!(
        child_text["messageId"].as_str().is_some(),
        "the replayed chunk carries its message id: {child_text}"
    );
    harness.shutdown().await;
}

/// B-113 — a `session/close` withdraws the open request like a cancel: the
/// work reports no `running` before its idle cancelled
/// (`acp-v2.test.ts:4426`, `cancelTurns` marks the inputs first). The
/// elicitation is the request `cancel_parent_round_trips` drains (a permission
/// is aborted before it), so its withdrawn close is the one that would report
/// `running` without the cancel mark.
#[tokio::test]
async fn t9a_g3_a_close_withdraws_a_held_elicitation_without_running() {
    let steps = vec![
        init_step(),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("go"),
                {"type": "control_request", "request_id": "elicit-1", "request": {
                    "subtype": "elicitation", "serverName": "srv",
                    "message": "Enter a value", "mode": "form",
                    "requestedSchema": {"type": "object",
                        "properties": {"val": {"type": "string"}}}}},
            ],
        }),
    ];
    let mut harness = Harness::start("close-withdraw", &steps);
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":2,
                "info":{"name":"v2-test-client","version":"1.0.0"},
                "capabilities":{"auth":{"terminal":{}},"elicitation":{"form":{}}}}}))
        .await;
    let init = harness.read_response(1).await;
    assert_eq!(init["result"]["protocolVersion"], 2, "{init}");
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
        .await;
    let created = harness.read_response(2).await;
    assert!(
        created.get("error").is_none(),
        "session/new failed: {created}"
    );
    let session_id = created["result"]["sessionId"].clone();
    let sid = session_id.as_str().unwrap_or_default().to_string();
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"go"}]}}))
        .await;

    let mut trace = Vec::new();
    loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let update = frame["params"]["update"].clone();
        if update["sessionUpdate"] == "state_update" {
            let state = update["state"].as_str().unwrap_or("");
            match update["stopReason"].as_str() {
                Some(reason) => trace.push(format!("{state} {reason}")),
                None => trace.push(state.to_string()),
            }
        } else if matches!(
            update["sessionUpdate"].as_str(),
            Some("user_message" | "user_message_chunk" | "agent_message_chunk")
        ) {
            trace.push(update["sessionUpdate"].as_str().unwrap_or("").to_string());
        }
        if trace.last().is_some_and(|last| last == "requires_action") {
            break;
        }
    }

    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/close",
            "params":{"sessionId":sid}}))
        .await;
    loop {
        let frame = harness.read_json().await;
        if frame.get("id").and_then(Value::as_i64) == Some(4) && frame.get("method").is_none() {
            assert!(frame.get("error").is_none(), "the close failed: {frame}");
            break;
        }
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let update = frame["params"]["update"].clone();
        if update["sessionUpdate"] == "state_update" {
            let state = update["state"].as_str().unwrap_or("");
            match update["stopReason"].as_str() {
                Some(reason) => trace.push(format!("{state} {reason}")),
                None => trace.push(state.to_string()),
            }
        } else if matches!(
            update["sessionUpdate"].as_str(),
            Some("user_message" | "user_message_chunk" | "agent_message_chunk")
        ) {
            trace.push(update["sessionUpdate"].as_str().unwrap_or("").to_string());
        }
    }
    assert_eq!(
        trace,
        vec![
            "user_message",
            "running",
            "requires_action",
            "idle cancelled"
        ],
        "no running after the close withdrew the request"
    );
    harness.shutdown().await;
}

/// NEW-8/M6: `session/close` forgets the connection's stores, so a session
/// recreated under the same id starts them afresh (`v2/agent.ts:114-120`).
#[tokio::test]
async fn t6g2_session_close_forgets_the_tool_call_stores() {
    let tool_steps = |text: &str| {
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo(text),
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_t", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                        "input": {"command": "ls"}}]}},
                result("done"),
            ],
        })
    };
    let steps = vec![
        init_step(),
        tool_steps("run it"),
        // The recreated session's own initialize handshake, then its prompt.
        init_step(),
        tool_steps("run it again"),
    ];
    let mut harness = Harness::start("forget", &steps);
    let session_id = harness.open_session().await;
    let sid = session_id.as_str().unwrap_or_default().to_string();
    let (answer, _) = harness.prompt_until_answer(3, "run it", &session_id).await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let first = harness.read_until_idle().await;
    assert!(
        first
            .iter()
            .any(|update| { update["toolCallId"] == "toolu_1" && update["name"] == "Bash" }),
        "the first session names the tool once: {first:?}"
    );

    harness
        .send_json(json!({"jsonrpc":"2.0","id":4,"method":"session/close",
            "params":{"sessionId":sid}}))
        .await;
    let closed = harness.read_response(4).await;
    assert_eq!(closed["result"], json!({}), "{closed}");

    // The session is recreated under the same id (the resume needs a stored
    // transcript).
    harness.seed_session(
        &sid,
        &[
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":sid,
                "cwd":repo_root().to_string_lossy(),
                "timestamp":"2026-01-01T00:00:00.000Z",
                "message":{"role":"user","content":"one"}}),
        ],
    );
    harness
        .send_json(json!({"jsonrpc":"2.0","id":5,"method":"session/resume",
            "params":{"sessionId":sid,"cwd":repo_root().to_string_lossy()}}))
        .await;
    let resumed = harness.read_response(5).await;
    assert!(
        resumed.get("error").is_none(),
        "the resume failed: {resumed}"
    );
    let (answer, _) = harness
        .prompt_until_answer(6, "run it again", &session_id)
        .await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let second = harness.read_until_idle().await;
    assert!(
        second
            .iter()
            .any(|update| { update["toolCallId"] == "toolu_1" && update["name"] == "Bash" }),
        "the recreated session names the tool again (the store was forgotten): {second:?}"
    );
    harness.shutdown().await;
}

/// NEW-3 (V-16, `acp-agent.ts:5572`): a v2 turn that fails `auth_required` under
/// `--hide-claude-auth` marks the query for a sign-out respawn and closes it, so
/// the next prompt recreates the query instead of answering "session ended".
#[tokio::test]
async fn t6g2_an_auth_failure_marks_the_query_for_a_respawn() {
    let account = json!({"apiKeySource": "ANTHROPIC_API_KEY"});
    let init = |account: Value| {
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
    };
    let steps = vec![
        init(account.clone()),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("needs auth"),
                {"type": "assistant", "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "Please sign in to proceed."}]}},
                {"type": "result", "subtype": "success", "stop_reason": "error",
                 "is_error": true, "result": "Please run /login to continue",
                 "usage": {"input_tokens": 1, "output_tokens": 1,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}},
            ],
        }),
        // The recreated query of the next prompt.
        json!({"segment": 1}),
        init(account.clone()),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("again"), message_start(), delta("hi"), result("hi")],
        }),
        json!({"auth_status_stdout": {"loggedIn": true, "apiProvider": "firstParty",
            "apiKeySource": "ANTHROPIC_API_KEY"}}),
    ];
    let mut harness = Harness::start_hiding_auth("auth-respawn", &steps);
    let session_id = harness.open_session().await;
    let (answer, _) = harness
        .prompt_until_answer(3, "needs auth", &session_id)
        .await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let idle = harness.read_until_idle().await;
    let idle = idle.last().expect("an idle");
    assert_eq!(idle["stopReason"], "error", "{idle}");
    assert_eq!(idle["error"]["code"], -32000, "authRequired: {idle}");

    // The next prompt recreates the closed query: the fake's second segment
    // answers it, so the sign-out respawn ran.
    let (answer, _) = harness.prompt_until_answer(4, "again", &session_id).await;
    assert!(
        answer.get("error").is_none(),
        "the recreated query answers: {answer}"
    );
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let idle = harness.read_until_idle().await;
    assert_eq!(idle.last().expect("an idle")["stopReason"], "end_turn");
    harness.shutdown().await;
}
