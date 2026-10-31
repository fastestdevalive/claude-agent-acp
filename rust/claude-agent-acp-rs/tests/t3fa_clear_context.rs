//! Turn 3 fix round (F22): prompts queued behind the plan turn survive a
//! clear-context restart and each gets its response from the fresh child.
//!
//! Each test drives the real `serve` over a hermetic duplex pipe and reads the
//! agent's raw JSON-RPC lines. The first child is a fake `claude` that asks to
//! exit the plan and answers the clear-context choice with the interruption
//! diagnostic; the second child (segment 1 of the transcript) is the case under
//! test.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use common::{fake_claude_binary, pid_is_gone, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::timeout;

const INIT_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[{"value":"claude-sonnet-4-5"}],"commands":[],"account":{}}}}"#;

fn init_step(sleep_ms: u64) -> String {
    format!(
        r#"{{"expect":{{"type":"control_request","request":{{"subtype":"initialize"}}}},"emit":[{{"sleep_ms":{sleep_ms}}},{INIT_REPLY}]}}"#
    )
}

/// The first child: answers `initialize`, asks to exit the plan, then, once the
/// client chose to clear the context, ends the cycle with the interruption
/// diagnostic.
fn first_segment() -> String {
    let usage = r#"{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#;
    let mut lines = vec![init_step(0)];
    lines.push(format!(
        r#"{{"expect":{{"type":"user"}},"emit":[{{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{{"role":"user","content":[{{"type":"text","text":"plan"}}]}}}},{{"type":"assistant","parent_tool_use_id":null,"message":{{"id":"msg_plan","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"tool_use","id":"toolu_P","name":"ExitPlanMode","input":{{"plan":"ship it"}}}}],"usage":{usage}}}}},{{"sleep_ms":100}},{{"type":"control_request","request_id":"permreq1","request":{{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{{"plan":"ship it"}},"tool_use_id":"toolu_P","agent_id":null,"permission_suggestions":[]}}}}]}}"#
    ));
    lines.push(format!(
        r#"{{"expect":{{"type":"control_response","response":{{"response":{{"behavior":"deny","interrupt":true}}}}}},"emit":[{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_P","is_error":true,"content":"fresh context"}}]}},"tool_result_meta":[{{"id":"toolu_P","non_execution_kind":"user-rejected"}}]}},{{"type":"result","subtype":"error_during_execution","stop_reason":null,"is_error":true,"errors":["[ede_diagnostic] result_type=user stop_reason=tool_use"],"usage":{usage},"modelUsage":{{"claude-sonnet-4-5":{{"contextWindow":200000}}}}}}]}}"#
    ));
    lines.join("\n")
}

/// The fresh child's continuation: it answers the plan message.
fn continuation_steps() -> String {
    let usage = r#"{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#;
    format!(
        r#"{{"expect":{{"type":"user"}},"emit":[{{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{{"role":"user","content":[{{"type":"text","text":"x"}}]}}}},{{"type":"assistant","parent_tool_use_id":null,"message":{{"id":"msg_c","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"text","text":"on it"}}],"usage":{usage}}}}},{{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"done","usage":{usage},"modelUsage":{{"claude-sonnet-4-5":{{"contextWindow":200000}}}}}},{{"type":"system","subtype":"session_state_changed","state":"idle"}}]}}"#
    )
}

fn write_transcript(tmp: &Path, second: &str) -> PathBuf {
    let path = tmp.join("clear-context.transcript.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{{\"segment\":1}}\n{}\n",
            first_segment_with_queue(),
            second
        ),
    )
    .expect("write transcript");
    path
}

fn read_pid(pid_file: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    pid_file: PathBuf,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str, second_segment: &str) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t3a-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let pid_file = tmp.join("pid");
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![
                (
                    "FAKE_CLAUDE_SCRIPT".to_string(),
                    write_transcript(&tmp, second_segment)
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "FAKE_CLAUDE_CURSOR".to_string(),
                    tmp.join("cursor").to_string_lossy().into_owned(),
                ),
                (
                    "FAKE_CLAUDE_PID_OUT".to_string(),
                    pid_file.to_string_lossy().into_owned(),
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
            pid_file,
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

    /// Read until the response to request `id`, answering a permission request
    /// with the clear-context choice on the way.
    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.read_json().await;
            if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                self.send_json(json!({"jsonrpc":"2.0","id":frame["id"].clone(),
                    "result":{"outcome":{"outcome":"selected","optionId":"exit-plan-clear-auto"}}}))
                    .await;
                continue;
            }
            if frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("method").is_none()
            {
                return frame;
            }
        }
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

    async fn prompt(&mut self, id: i64, session_id: &Value, text: &str) {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":text}]}}))
            .await;
    }

    async fn finish(self) {
        drop(self.input);
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

async fn assert_gone(pids: &[u32]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    for pid in pids {
        while !pid_is_gone(*pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "child {pid} outlives the restart"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// The first child of this test also takes the two prompts queued behind the
/// plan turn (their frames reach it while the dialog is open) and says nothing.
fn first_segment_with_queue() -> String {
    let mut lines: Vec<String> = first_segment().lines().map(str::to_string).collect();
    let silent = r#"{"expect":{"type":"user"},"emit":[]}"#;
    lines.insert(2, silent.to_string());
    lines.insert(2, silent.to_string());
    lines.join("\n")
}

#[tokio::test]
async fn t3fa_prompts_queued_behind_the_plan_turn_are_answered_after_the_restart() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let second = format!(
            "{}\n{}\n{}\n{}",
            init_step(0),
            continuation_steps(),
            continuation_steps(),
            continuation_steps()
        );
        let mut harness = Harness::start("queued", &second);
        let session_id = harness.open_session().await;
        let first_pid = read_pid(&harness.pid_file).expect("the first child's pid");
        harness.prompt(3, &session_id, "plan").await;
        // The plan dialog is open: queue two prompts behind the plan turn, and
        // only then choose to clear the context.
        let request = loop {
            let frame = harness.read_json().await;
            if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                break frame;
            }
        };
        harness.prompt(4, &session_id, "queued one").await;
        harness.prompt(5, &session_id, "queued two").await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        harness
            .send_json(json!({"jsonrpc":"2.0","id":request["id"].clone(),
                "result":{"outcome":{"outcome":"selected","optionId":"exit-plan-clear-auto"}}}))
            .await;
        for id in [3, 4, 5] {
            let response = harness.read_response(id).await;
            assert_eq!(
                response["result"]["stopReason"], "end_turn",
                "{id}: {response}"
            );
        }
        assert_gone(&[first_pid]).await;
        let second_pid = read_pid(&harness.pid_file).expect("the fresh child's pid");
        harness.finish().await;
        assert_gone(&[second_pid]).await;
    })
    .await
    .expect("test timed out");
}
