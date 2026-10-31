//! T7A STEP 6 — the v2 hooks of `review-T6G2` (F1, F3, F5) and `review-T6INT`
//! finding 3, over the real wire (ACP v2).
//!
//! * F1: a v2 turn failed by an assistant-error auth failure marks the query for
//!   a sign-out respawn (the effective kind, not the raw one).
//! * F5: an AIR `sessionFailure` on v2 ends the turn `end_turn`, not `idle error`.
//! * T6INT 3: a v2 prompt's file-change report publishes before the turn's
//!   `idle`.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::serve;
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use claude_agent_acp_rs::ServeOptions;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::timeout;

fn init_step(account: Value) -> Value {
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
}

fn echo(text: &str) -> Value {
    json!({"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

fn result(text: &str) -> Value {
    json!({
        "type": "result", "subtype": "success", "stop_reason": "end_turn",
        "is_error": false, "result": text,
        "usage": {"input_tokens": 100, "output_tokens": 4,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
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

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str, steps: &[Value], hide_claude_auth: bool) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t7a-v2-{name}-{}", std::process::id()));
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

    async fn open_session(&mut self, air: bool, cwd: &Path) -> Value {
        let caps = if air {
            json!({
                "auth": {"terminal": {}},
                "_meta": {"jetbrains": {"air": {
                    "version": 1,
                    "capabilities": ["agentFileChangeReport", "sessionFailure"],
                }}},
            })
        } else {
            json!({"auth": {"terminal": {}}})
        };
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":2,
                "info":{"name":"v2-test-client","version":"1.0.0"},
                "capabilities":caps}}))
            .await;
        let init = self.read_response(1).await;
        assert_eq!(init["result"]["protocolVersion"], 2, "{init}");
        self.send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":cwd.to_string_lossy(),"mcpServers":[]}}))
            .await;
        let created = self.read_response(2).await;
        assert!(
            created.get("error").is_none(),
            "session/new failed: {created}"
        );
        created["result"]["sessionId"].clone()
    }

    async fn prompt_until_answer(
        &mut self,
        id: i64,
        text: &str,
        session_id: &Value,
        meta: Value,
    ) -> (Value, Vec<Value>) {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":text}],
                "_meta":meta}}))
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

    async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// F1 (`review-T6G2`): an assistant frame whose `error` is
/// `authentication_failed` derives `claude_kind = AuthRequired` while the event
/// kind stays `ProviderError`. Under `--hide-claude-auth` the v2 bookkeeping must
/// still mark the query for the sign-out respawn and close it, so the next prompt
/// recreates the query instead of answering "session ended".
#[tokio::test]
async fn t7a_f1_v2_assistant_auth_error_marks_the_query_for_a_respawn() {
    let account = json!({"apiKeySource": "ANTHROPIC_API_KEY"});
    let steps = vec![
        init_step(account.clone()),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("needs auth"),
                {"type": "assistant", "parent_tool_use_id": null,
                 "error": "authentication_failed",
                 "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "Please sign in to proceed."}]}},
                {"type": "result", "subtype": "success", "stop_reason": "error",
                 "is_error": true, "result": "API Error: 401 Unauthorized",
                 "usage": {"input_tokens": 1, "output_tokens": 1,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}},
            ],
        }),
        // The recreated query of the next prompt.
        json!({"segment": 1}),
        init_step(account),
        json!({
            "expect": {"type": "user"},
            "emit": [echo("again"), result("hi")],
        }),
        json!({"auth_status_stdout": {"loggedIn": true, "apiProvider": "firstParty",
            "apiKeySource": "ANTHROPIC_API_KEY"}}),
    ];
    let mut harness = Harness::start("f1-auth-respawn", &steps, true);
    let session_id = harness.open_session(false, &repo_root()).await;
    let (answer, _) = harness
        .prompt_until_answer(3, "needs auth", &session_id, json!({}))
        .await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let idle = harness.read_until_idle().await;
    let idle = idle.last().expect("an idle");
    assert_eq!(idle["stopReason"], "error", "{idle}");
    assert_eq!(idle["error"]["code"], -32000, "authRequired: {idle}");

    // The next prompt recreates the closed query: the fake's second segment
    // answers it, so the sign-out respawn ran.
    let (answer, _) = harness
        .prompt_until_answer(4, "again", &session_id, json!({}))
        .await;
    assert!(
        answer.get("error").is_none(),
        "the recreated query answers: {answer}"
    );
    let idle = harness.read_until_idle().await;
    assert_eq!(idle.last().expect("an idle")["stopReason"], "end_turn");
    harness.shutdown().await;
}

/// F5 (`review-T6G2`): with AIR `sessionFailure`, a failed v2 turn ends
/// `end_turn` like v1/upstream (`failActiveWithSessionFailure` does not fail the
/// turn), instead of an `idle error`.
#[tokio::test]
async fn t7a_f5_v2_air_session_failure_ends_the_turn() {
    let account = json!({"apiKeySource": "ANTHROPIC_API_KEY"});
    let steps = vec![
        init_step(account),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("break"),
                {"type": "assistant", "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "The API failed."}]}},
                {"type": "result", "subtype": "error_during_execution",
                 "stop_reason": "error", "is_error": true,
                 "result": "API Error: 500 Internal Server Error",
                 "usage": {"input_tokens": 1, "output_tokens": 1,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}},
            ],
        }),
    ];
    let mut harness = Harness::start("f5-air-failure", &steps, false);
    let session_id = harness.open_session(true, &repo_root()).await;
    let (answer, _) = harness
        .prompt_until_answer(3, "break", &session_id, json!({}))
        .await;
    let message_id = answer["result"]["messageId"]
        .as_str()
        .unwrap_or_else(|| panic!("a v2 prompt answers its message id: {answer}"));
    let idle = harness.read_until_idle().await;
    let idle = idle.last().expect("an idle");
    assert_eq!(
        idle["stopReason"], "end_turn",
        "an AIR sessionFailure ends the v2 turn end_turn: {idle}"
    );
    assert!(idle.get("error").is_none(), "{idle}");
    // H2 (`review-T7`): `failActiveWithSessionFailure` records the failure and
    // settles the turn with `sessionFailureMeta` in the idle's `_meta`
    // (`acp-agent.ts:5591-5593`; `turnOutcome` merges quota then the extra meta,
    // `acp-agent.ts:11870-11880`). The full serialized string pins the key order
    // Node emits: sessionUpdate, state, stopReason, usage, _meta; inside _meta
    // quota before jetbrains; inside sessionFailure id, revision, category,
    // severity, title, actions (`session-failure-extension.ts:168-179`).
    let expected = format!(
        concat!(
            r#"{{"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn","#,
            r#""usage":{{"inputTokens":1,"outputTokens":1,"cachedReadTokens":0,"#,
            r#""cachedWriteTokens":0,"totalTokens":2}},"#,
            r#""_meta":{{"quota":{{"token_count":{{"totalTokens":2,"inputTokens":1,"#,
            r#""cachedInputTokens":0,"cachedWriteTokens":0,"outputTokens":1,"#,
            r#""reasoningOutputTokens":0}},"model_usage":[]}},"#,
            r#""jetbrains":{{"air":{{"version":1,"sessionFailure":{{"id":"{}:error","#,
            r#""revision":1,"category":"service","severity":"error","#,
            r#""title":"error_during_execution","actions":["retry"]}}}}}}}}}}"#,
        ),
        message_id
    );
    assert_eq!(
        serde_json::to_string(idle).expect("serialize the idle"),
        expected,
        "the AIR sessionFailure rides the v2 idle in Node's exact shape: {idle}"
    );
    harness.shutdown().await;
}

/// U-88 (`acp-v2.test.ts:5399-5414`): the connection's auth status is pushed
/// after a v2 `initialize`, as it is for v1.
#[tokio::test]
async fn t7a_u88_v2_pushes_the_auth_status_after_initialize() {
    let steps = vec![
        init_step(json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
        json!({"auth_status_stdout": {"loggedIn": true, "email": "user@example.com",
            "subscriptionType": "max", "apiProvider": "firstParty"}}),
    ];
    let mut harness = Harness::start("u88-auth-push", &steps, false);
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":2,
                "info":{"name":"v2-test-client","version":"1.0.0"}}}))
        .await;
    let init = harness.read_response(1).await;
    assert_eq!(init["result"]["protocolVersion"], 2, "{init}");
    let update = loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("_auth/status_update") {
            break frame;
        }
    };
    let status = &update["params"]["authStatus"];
    assert_eq!(status["kind"].as_str(), Some("account"), "{update}");
    assert_eq!(
        status["account"]["email"].as_str(),
        Some("user@example.com"),
        "{update}"
    );
    harness.shutdown().await;
}

/// F3 (`review-T6G2`): the v2 permission request is queued synchronously, so an
/// update the actor emits in the same batch (here a parallel assistant text)
/// cannot overtake it. Without the fix the request is sent from inside the
/// spawned round-trip task, after the actor's next update.
#[tokio::test]
async fn t7a_f3_v2_permission_request_precedes_the_same_batch_update() {
    let account = json!({"apiKeySource": "ANTHROPIC_API_KEY"});
    let steps = vec![
        init_step(account),
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
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_p", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "text", "text": "parallel update"}]}},
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
    ];
    let mut harness = Harness::start("f3-permission-order", &steps, false);
    let session_id = harness.open_session(false, &repo_root()).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"run it"}],
                "_meta":{}}}))
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
        updates.push(frame["params"]["update"].clone());
    };
    // The request is queued synchronously with `requires_action`: no update the
    // actor emits after handling the permission can land between them.
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
    harness
        .send_json(json!({"jsonrpc":"2.0","id":request["id"],
            "result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}))
        .await;
    let idle = harness.read_until_idle().await;
    assert_eq!(idle.last().expect("an idle")["stopReason"], "end_turn");
    harness.shutdown().await;
}

/// `review-T6INT` finding 3: a v2 prompt that asks for a file-change report
/// publishes it before the turn's `idle` (Node awaits the preview inside
/// `settleActive`), and the prompt's `_meta` request is registered at all.
#[tokio::test]
async fn t7a_v2_report_publishes_before_idle() {
    let account = json!({"apiKeySource": "ANTHROPIC_API_KEY"});
    let steps = vec![
        init_step(account),
        json!({
            "expect": {"type": "user"},
            "emit": [
                echo("change a file"),
                {"type": "assistant", "message": {"role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_edit", "name": "Edit",
                        "input": {"file_path": "src/changed.ts",
                            "old_string": "a", "new_string": "b"}}]}},
                {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_edit",
                     "content": "Edited"}]}},
                result("Done"),
            ],
        }),
        json!({
            "expect": {"type": "control_request",
                "request": {"subtype": "rewind_files", "dry_run": true}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ",
                "response": {"canRewind": true, "filesChanged": ["src/changed.ts"],
                    "insertions": 4, "deletions": 1}
            }}],
        }),
    ];
    let mut harness = Harness::start("v2-report", &steps, false);
    // The report resolves the preview's path inside the session's cwd, so the
    // file must exist there before the session starts.
    let cwd = harness.tmp.join("cwd");
    std::fs::create_dir_all(cwd.join("src")).expect("create cwd");
    std::fs::write(cwd.join("src/changed.ts"), "x").expect("write file");
    let session_id = harness.open_session(true, &cwd).await;
    let (answer, _) = harness
        .prompt_until_answer(
            6,
            "change a file",
            &session_id,
            json!({"jetbrains": {"air": {"agentFileChangeReportRequest":
                {"version": 1, "requestId": "v2-report-1"}}}}),
        )
        .await;
    assert!(answer["result"]["messageId"].is_string(), "{answer}");
    let updates = harness.read_until_idle().await;
    let report_index = updates.iter().position(|update| {
        update
            .pointer("/_meta/jetbrains/air/agentFileChangeReport")
            .is_some()
    });
    let idle_index = updates
        .iter()
        .position(|update| update["sessionUpdate"] == "state_update" && update["state"] == "idle");
    let report_index = report_index.expect("a report was published before idle");
    let idle_index = idle_index.expect("an idle");
    assert!(
        report_index < idle_index,
        "the report precedes the idle: {updates:?}"
    );
    let report = &updates[report_index]["_meta"]["jetbrains"]["air"]["agentFileChangeReport"];
    assert_eq!(report["status"].as_str(), Some("reported"), "{report:?}");
    harness.shutdown().await;
}
