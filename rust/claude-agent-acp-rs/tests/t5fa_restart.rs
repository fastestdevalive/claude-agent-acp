//! T5FA — F3 (B-109): a clear-context restart closes the old context's
//! compaction lifecycle and starts a NEW one, exactly as the fresh consumer
//! upstream builds (`acp-agent.ts:4542`): the old context's pending hook summary
//! does not carry into the fresh context's first compaction.
//!
//! End to end over a hermetic duplex pipe with a fake `claude`: the first child
//! compacts (terminal `completed`), then reports a PostCompact summary that
//! waits for the NEXT compaction, then accepts the ExitPlanMode clear-context
//! choice and ends the cycle with the interruption diagnostic; the fresh child
//! compacts again.

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

const USAGE: &str = r#"{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#;

const INIT_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[{"value":"claude-sonnet-4-5"}],"commands":[],"account":{}}}}"#;

fn init_step() -> String {
    format!(
        r#"{{"expect":{{"type":"control_request","request":{{"subtype":"initialize"}}}},"emit":[{INIT_REPLY}]}}"#
    )
}

/// The first child: a plan turn that first compacts, then reports a PostCompact
/// summary (the entity is already terminal, so it waits for the next
/// compaction), then asks to exit the plan and ends with the clear-context
/// interruption diagnostic.
fn first_segment() -> String {
    let mut lines = vec![init_step()];
    lines.push(format!(
        r#"{{"expect":{{"type":"user"}},"emit":[{{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{{"role":"user","content":[{{"type":"text","text":"plan"}}]}}}},{{"type":"system","subtype":"status","uuid":"pre-reset","status":"compacting"}},{{"type":"system","subtype":"status","uuid":"pre-reset","compact_result":"success"}},{{"type":"assistant","parent_tool_use_id":null,"message":{{"id":"msg_plan","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"tool_use","id":"toolu_P","name":"ExitPlanMode","input":{{"plan":"ship it"}}}}],"usage":{USAGE}}}}},{{"sleep_ms":100}},{{"type":"control_request","request_id":"permreq1","request":{{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{{"plan":"ship it"}},"tool_use_id":"toolu_P","agent_id":null,"permission_suggestions":[]}}}}]}}"#
    ));
    // The clear-context answer, the tool result, then the PostCompact hook: the
    // entity is terminal, so its summary waits for the next compaction.
    lines.push(
        r#"{"expect":{"type":"control_response","response":{"response":{"behavior":"deny","interrupt":true}}},"emit":[{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_P","is_error":true,"content":"fresh context"}]},"tool_result_meta":[{"id":"toolu_P","non_execution_kind":"user-rejected"}]},{"sleep_ms":100},{"type":"control_request","request_id":"hk2","request":{"subtype":"hook_callback","callback_id":"hook_2","input":{"hook_event_name":"PostCompact","agent_id":null,"compact_summary":"<summary>\nstale summary\n</summary>"}}}]}"#.to_string(),
    );
    lines.push(format!(
        r#"{{"expect":{{"type":"control_response","response":{{"request_id":"hk2","response":{{"continue":true}}}}}},"emit":[{{"type":"result","subtype":"error_during_execution","stop_reason":null,"is_error":true,"errors":["[ede_diagnostic] result_type=user stop_reason=tool_use"],"usage":{USAGE},"modelUsage":{{"claude-sonnet-4-5":{{"contextWindow":200000}}}}}}]}}"#
    ));
    lines.join("\n")
}

/// The fresh child: its first compaction, which must NOT carry the old
/// context's pending summary.
fn second_segment() -> String {
    let mut lines = vec![init_step()];
    lines.push(format!(
        r#"{{"expect":{{"type":"user"}},"emit":[{{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{{"role":"user","content":[{{"type":"text","text":"x"}}]}}}},{{"type":"system","subtype":"status","uuid":"post-reset","status":"compacting"}},{{"type":"system","subtype":"status","uuid":"post-reset","compact_result":"success"}},{{"type":"assistant","parent_tool_use_id":null,"message":{{"id":"msg_c","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"text","text":"on it"}}],"usage":{USAGE}}}}},{{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"done","usage":{USAGE},"modelUsage":{{"claude-sonnet-4-5":{{"contextWindow":200000}}}}}},{{"type":"system","subtype":"session_state_changed","state":"idle"}}]}}"#
    ));
    lines.join("\n")
}

/// The clear-context plan turn of the Fast mode restore (B-74): the old child
/// starts with Fast mode off; the fresh child's own initialize reports it on,
/// so the adapter must apply the old toggle (`fastMode:false`) before the
/// continuation reaches the fresh child.
fn fast_mode_transcript(tmp: &Path) -> PathBuf {
    let path = tmp.join("restart-fast-mode.transcript.jsonl");
    let init = |fast_mode_state: Option<&str>| {
        let mut response = json!({
            "models": [{"value": "claude-sonnet-4-5",
                "displayName": "Claude Sonnet 4.5", "supportsFastMode": true}],
            "commands": [],
            "account": {},
        });
        if let Some(state) = fast_mode_state {
            response["fast_mode_state"] = json!(state);
        }
        json!({
            "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ", "response": response
            }}],
        })
    };
    let first = vec![
        init(None),
        json!({
            "expect": {"type": "user"},
            "emit": [
                {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
                    "message": {"role": "user", "content": [{"type": "text", "text": "plan"}]}},
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_plan", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "tool_use", "id": "toolu_P", "name": "ExitPlanMode",
                        "input": {"plan": "ship it"}}],
                    "usage": {"input_tokens": 10, "output_tokens": 5,
                        "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}},
                {"type": "control_request", "request_id": "permreq1", "request": {
                    "subtype": "can_use_tool", "tool_name": "ExitPlanMode",
                    "input": {"plan": "ship it"}, "tool_use_id": "toolu_P",
                    "agent_id": null, "permission_suggestions": []}},
            ],
        }),
        json!({
            "expect": {"type": "control_response",
                "response": {"response": {"behavior": "deny", "interrupt": true}}},
            "emit": [
                {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_P", "is_error": true,
                        "content": "fresh context"}]},
                    "tool_result_meta": [{"id": "toolu_P", "non_execution_kind": "user-rejected"}]},
                {"type": "result", "subtype": "error_during_execution", "stop_reason": null,
                    "is_error": true,
                    "errors": ["[ede_diagnostic] result_type=user stop_reason=tool_use"],
                    "usage": {"input_tokens": 10, "output_tokens": 5,
                        "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                    "modelUsage": {"claude-sonnet-4-5": {"contextWindow": 200000}}},
            ],
        }),
    ];
    let second = vec![
        init(Some("on")),
        json!({
            "expect": {"type": "control_request", "request": {
                "subtype": "apply_flag_settings", "settings": {"fastMode": false}}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ", "response": {}
            }}],
        }),
        json!({
            "expect": {"type": "user"},
            "emit": [
                {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
                    "message": {"role": "user", "content": [
                        {"type": "text", "text": "Implement the following plan:\n\nship it"}]}},
                {"type": "assistant", "parent_tool_use_id": null, "message": {
                    "id": "msg_c", "role": "assistant", "model": "claude-sonnet-4-5",
                    "content": [{"type": "text", "text": "on it"}],
                    "usage": {"input_tokens": 10, "output_tokens": 5,
                        "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}},
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                    "is_error": false, "result": "done",
                    "usage": {"input_tokens": 10, "output_tokens": 5,
                        "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                    "modelUsage": {"claude-sonnet-4-5": {"contextWindow": 200000}}},
                {"type": "system", "subtype": "session_state_changed", "state": "idle"},
            ],
        }),
    ];
    let lines = |steps: &[Value]| {
        steps
            .iter()
            .map(|step| step.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    std::fs::write(
        &path,
        format!("{}\n{{\"segment\":1}}\n{}\n", lines(&first), lines(&second)),
    )
    .expect("write transcript");
    path
}

fn write_transcript(tmp: &Path) -> PathBuf {
    let path = tmp.join("restart-compaction.transcript.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{{\"segment\":1}}\n{}\n",
            first_segment(),
            second_segment()
        ),
    )
    .expect("write transcript");
    path
}

/// The same plan turn, but its compaction entity is still open when the
/// clear-context choice is accepted: the restart has a live entity to close.
fn first_segment_open_entity() -> String {
    first_segment().replace(
        r#"{"type":"system","subtype":"status","uuid":"pre-reset","compact_result":"success"},"#,
        "",
    )
}

fn write_open_entity_transcript(tmp: &Path) -> PathBuf {
    let path = tmp.join("restart-open-entity.transcript.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{{\"segment\":1}}\n{}\n",
            first_segment_open_entity(),
            second_segment()
        ),
    )
    .expect("write transcript");
    path
}

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str) -> Self {
        Self::start_with(name, write_transcript)
    }

    /// A restart whose first child still has an open compaction entity.
    fn start_open_entity(name: &str) -> Self {
        Self::start_with(name, write_open_entity_transcript)
    }

    /// A restart whose fresh child reports Fast mode on while the old session
    /// has it off (B-74).
    fn start_fast_mode(name: &str) -> Self {
        Self::start_with(name, fast_mode_transcript)
    }

    fn start_with(name: &str, transcript: fn(&Path) -> PathBuf) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t5fa-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![
                (
                    "FAKE_CLAUDE_SCRIPT".to_string(),
                    transcript(&tmp).to_string_lossy().into_owned(),
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
        let n = timeout(Duration::from_secs(30), self.reader.read_line(&mut line))
            .await
            .expect("agent output timed out")
            .expect("read agent line");
        assert!(n > 0, "agent closed stdout");
        serde_json::from_str(&line).expect("agent line is JSON")
    }

    async fn finish(self) {
        drop(self.input);
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// The compaction updates of every `session/update` collected so far.
fn compaction_updates(updates: &[Value]) -> Vec<Value> {
    updates
        .iter()
        .filter(|update| {
            matches!(
                update["sessionUpdate"].as_str(),
                Some("compaction_update" | "compaction_summary_chunk")
            )
        })
        .cloned()
        .collect()
}

#[tokio::test]
async fn t5fa_restart_starts_a_fresh_compaction_lifecycle() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start("restart");
        // A compaction-capable, non-AIR client (the ACP compaction contract).
        harness
            .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":1,
                    "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},
                        "terminal":false,"session":{"compaction":{}},"auth":{"terminal":false}}}}))
            .await;
        let mut updates = Vec::new();
        loop {
            let frame = harness.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(1)
                && frame.get("method").is_none()
            {
                break;
            }
        }
        harness
            .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
                "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        let session_id = loop {
            let frame = harness.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(2)
                && frame.get("method").is_none()
            {
                assert!(
                    frame.get("error").is_none(),
                    "session/new failed: {frame}"
                );
                break frame["result"]["sessionId"].clone();
            }
        };
        harness
            .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
                "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"plan"}]}}))
            .await;

        // The plan dialog: answer the clear-context choice.
        loop {
            let frame = harness.read_json().await;
            if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(frame["params"]["update"].clone());
                continue;
            }
            if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                harness
                    .send_json(json!({"jsonrpc":"2.0","id":frame["id"].clone(),
                        "result":{"outcome":{"outcome":"selected","optionId":"exit-plan-clear-auto"}}}))
                    .await;
                continue;
            }
            if frame.get("id").and_then(Value::as_i64) == Some(3) && frame.get("method").is_none() {
                assert!(
                    frame.get("error").is_none(),
                    "the prompt failed: {frame} (updates: {updates:?})"
                );
                break;
            }
        }
        assert_eq!(
            compaction_updates(&updates)
                .iter()
                .map(|u| (u["sessionUpdate"].clone(), u["status"].clone()))
                .collect::<Vec<_>>(),
            vec![
                (json!("compaction_update"), json!("in_progress")),
                (json!("compaction_update"), json!("completed")),
                (json!("compaction_update"), json!("in_progress")),
                (json!("compaction_update"), json!("completed")),
            ],
            "the old context's entity, then a NEW lifecycle for the fresh context: {updates:?}"
        );
        let last = compaction_updates(&updates).pop().expect("a terminal update");
        assert_eq!(last["compactionId"], "post-reset");
        assert!(
            last.get("summary").is_none(),
            "the old context's pending summary must not carry: {last}"
        );
        assert!(
            updates
                .iter()
                .filter(|u| u["sessionUpdate"] == "compaction_summary_chunk")
                .count()
                == 0,
            "no summary chunks were streamed"
        );

        harness.finish().await;
    })
    .await
    .expect("test timed out");
}

/// The initialize / session-new / prompt handshake and the plan turn: answers
/// the clear-context choice and returns every `session/update` the agent sent
/// before the prompt response.
async fn drive_plan_turn(harness: &mut Harness) -> Vec<Value> {
    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1,
                "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},
                    "terminal":false,"session":{"compaction":{}},"auth":{"terminal":false}}}}))
        .await;
    loop {
        let frame = harness.read_json().await;
        if frame.get("id").and_then(Value::as_i64) == Some(1) && frame.get("method").is_none() {
            break;
        }
    }
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
        .await;
    let session_id = loop {
        let frame = harness.read_json().await;
        if frame.get("id").and_then(Value::as_i64) == Some(2) && frame.get("method").is_none() {
            assert!(frame.get("error").is_none(), "session/new failed: {frame}");
            break frame["result"]["sessionId"].clone();
        }
    };
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"plan"}]}}))
        .await;
    let mut updates = Vec::new();
    loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/update") {
            updates.push(frame["params"]["update"].clone());
            continue;
        }
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            harness
                .send_json(json!({"jsonrpc":"2.0","id":frame["id"].clone(),
                    "result":{"outcome":{"outcome":"selected","optionId":"exit-plan-clear-auto"}}}))
                .await;
            continue;
        }
        if frame.get("id").and_then(Value::as_i64) == Some(3) && frame.get("method").is_none() {
            assert!(
                frame.get("error").is_none(),
                "the prompt failed: {frame} (updates: {updates:?})"
            );
            break;
        }
    }
    updates
}

/// B-74 — `continuePlanInFreshContext` restores the old session's Fast mode on
/// the fresh session when its own initialize reports another state
/// (`applyFastMode`, `clear-context-coordinator.ts:143-152`): the old session is
/// off, the fresh one starts on, so the adapter applies `fastMode:false` before
/// the continuation reaches the fresh child (the fake transcript expects it in
/// that order; a missing request fails the restart).
#[tokio::test]
async fn t9a_g3_restores_disabled_fast_mode_on_a_fresh_context() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start_fast_mode("restart-fast-mode");
        let updates = drive_plan_turn(&mut harness).await;
        let text = updates
            .iter()
            .find(|update| {
                update["sessionUpdate"] == "agent_message_chunk"
                    && update["content"]["text"] == "on it"
            })
            .unwrap_or_else(|| panic!("the fresh context answered: {updates:?}"));
        assert_eq!(text["content"]["text"], "on it");
        let fast = updates
            .iter()
            .rev()
            .find_map(|update| {
                update["configOptions"].as_array().and_then(|options| {
                    options
                        .iter()
                        .find(|option| option["id"] == "fast")
                        .cloned()
                })
            })
            .expect("the restart publishes the config options");
        assert_eq!(
            fast["currentValue"], "off",
            "the old session's Fast mode is restored: {fast}"
        );
        harness.finish().await;
    })
    .await
    .expect("test timed out");
}

/// The restart call site interrupts the old lifecycle (`acp-agent.ts:7488`):
/// an entity the old context left open gets its `cancelled` terminal (exactly
/// once) before the fresh context's own entity.
#[tokio::test]
async fn t5fa3_the_restart_closes_an_open_compaction_entity() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let mut harness = Harness::start_open_entity("restart-open");
        let updates = drive_plan_turn(&mut harness).await;
        let updates = compaction_updates(&updates);
        let cancelled: Vec<&Value> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "compaction_update" && u["status"] == "cancelled")
            .collect();
        assert_eq!(
            cancelled.len(),
            1,
            "the open entity is closed exactly once: {updates:?}"
        );
        assert_eq!(cancelled[0]["compactionId"], "pre-reset");
        assert_eq!(
            updates
                .iter()
                .map(|u| (u["sessionUpdate"].clone(), u["status"].clone()))
                .collect::<Vec<_>>(),
            vec![
                (json!("compaction_update"), json!("in_progress")),
                (json!("compaction_update"), json!("cancelled")),
                (json!("compaction_update"), json!("in_progress")),
                (json!("compaction_update"), json!("completed")),
            ],
            "the old entity closes, then a NEW lifecycle for the fresh context: {updates:?}"
        );
        let last = updates.last().expect("a terminal update");
        assert_eq!(last["compactionId"], "post-reset");
        assert!(
            last.get("summary").is_none(),
            "the old context's summary must not carry: {last}"
        );

        harness.finish().await;
    })
    .await
    .expect("test timed out");
}
