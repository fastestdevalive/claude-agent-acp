//! T9A — `clear-context-coordinator.test.ts` > continuePlanInFreshContext >
//! "does not turn an automatic displayed effort into a pin during restart":
//! the session shows an effort level the CLI recommended (no user pin), and the
//! fresh context is spawned without an `--effort` flag.
//!
//! The restart replaces the `claude` child while the public prompt stays
//! pending. The fake child records its argv (`FAKE_CLAUDE_ARGV_OUT`), and the
//! second spawn overwrites the record, so the file holds the fresh context's
//! argv when the restart is accepted.

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

/// The initialize reply of a model that supports effort with the recommended
/// `medium` level: the session displays `medium` without a user pin.
const INIT_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[{"value":"claude-sonnet-4-5","displayName":"Claude Sonnet 4.5","supportsEffort":true,"supportedEffortLevels":["low","medium","high"]}],"commands":[],"account":{}}}}"#;

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
    lines.push(
        r#"{"expect":{"type":"control_response","response":{"response":{"behavior":"deny","interrupt":true}}},"emit":[{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_P","is_error":true,"content":"fresh context"}]},"tool_result_meta":[{"id":"toolu_P","non_execution_kind":"user-rejected"}]},{"type":"result","subtype":"error_during_execution","stop_reason":null,"is_error":true,"errors":["[ede_diagnostic] result_type=user stop_reason=tool_use"]}]}"#.to_string(),
    );
    lines.join("\n")
}

/// The fresh child's continuation: it answers the plan message.
fn continuation_steps() -> String {
    let usage = r#"{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#;
    format!(
        r#"{{"expect":{{"type":"user"}},"emit":[{{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{{"role":"user","content":[{{"type":"text","text":"x"}}]}}}},{{"type":"assistant","parent_tool_use_id":null,"message":{{"id":"msg_c","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"text","text":"on it"}}],"usage":{usage}}}}},{{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"done","usage":{usage}}},{{"type":"system","subtype":"session_state_changed","state":"idle"}}]}}"#
    )
}

fn write_transcript(tmp: &Path, second: &str) -> PathBuf {
    let path = tmp.join("clear-context.transcript.jsonl");
    std::fs::write(
        &path,
        format!("{}\n{{\"segment\":1}}\n{}\n", first_segment(), second),
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
    argv_file: PathBuf,
    tmp: PathBuf,
}

impl Harness {
    /// `settings_effort`: an optional `effortLevel` in the user settings file.
    fn start_with(name: &str, second_segment: &str, settings_effort: Option<&str>) -> Self {
        let tmp = std::env::temp_dir().join(format!("acp-t9a-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let config_dir = tmp.join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        if let Some(effort) = settings_effort {
            std::fs::write(
                config_dir.join("settings.json"),
                format!("{{\"effortLevel\":\"{effort}\"}}"),
            )
            .expect("settings");
        }
        let pid_file = tmp.join("pid");
        let argv_file = tmp.join("argv.json");
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
                (
                    "FAKE_CLAUDE_ARGV_OUT".to_string(),
                    argv_file.to_string_lossy().into_owned(),
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
            env: EnvSnapshot::from_pairs([(
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            )]),
            logger: None,
        };
        let task = tokio::spawn(serve(WireTransport::new(agent_in, agent_out), opts));
        Self {
            input: client_in,
            reader: BufReader::new(client_out),
            task,
            pid_file,
            argv_file,
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

    async fn open_session(&mut self) -> (Value, Value) {
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":1}}))
            .await;
        self.read_response(1).await;
        self.send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
            .await;
        let created = self.read_response(2).await;
        (
            created["result"]["sessionId"].clone(),
            created["result"]["configOptions"].clone(),
        )
    }

    async fn prompt(&mut self, id: i64, session_id: &Value, text: &str) {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":text}]}}))
            .await;
    }

    async fn until_second_child(&mut self, first_pid: u32) -> u32 {
        loop {
            let frame = self.read_json().await;
            if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                self.send_json(json!({"jsonrpc":"2.0","id":frame["id"].clone(),
                    "result":{"outcome":{"outcome":"selected","optionId":"exit-plan-clear-auto"}}}))
                    .await;
                break;
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(pid) = read_pid(&self.pid_file).filter(|pid| *pid != first_pid) {
                return pid;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a second child was started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn argv(&self) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(&self.argv_file) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    return value;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fresh child recorded its argv"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
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

/// The session displays the effort level the CLI recommends (`medium`) and has
/// no user pin; the fresh context is spawned without `--effort`, so a restart
/// does not turn the automatic display into a pin.
#[tokio::test]
async fn t9a_automatic_displayed_effort_is_not_pinned_by_a_restart() {
    timeout(Duration::from_secs(90), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let second = format!("{}\n{}", init_step(0), continuation_steps());
        let mut harness = Harness::start_with("effort", &second, Some("high"));
        let (session_id, config_options) = harness.open_session().await;
        let effort = config_options
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["id"] == "effort")
            .expect("the effort option is displayed")
            .clone();
        assert_eq!(effort["currentValue"], "high", "{effort}");
        let first_pid = read_pid(&harness.pid_file).expect("the first child's pid");
        harness.prompt(3, &session_id, "plan").await;
        let second_pid = harness.until_second_child(first_pid).await;
        let response = harness.read_response(3).await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        let argv = harness.argv();
        let args: Vec<&str> = argv["argv"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            !args.iter().any(|arg| arg.starts_with("--effort")),
            "the fresh context carries no effort pin: {args:?}"
        );
        assert_gone(&[first_pid]).await;
        assert!(
            !pid_is_gone(second_pid),
            "the fresh child serves the session"
        );
        harness.finish().await;
        assert_gone(&[second_pid]).await;
    })
    .await
    .expect("test timed out");
}
