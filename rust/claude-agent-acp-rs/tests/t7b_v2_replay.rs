//! T7B STEP 5 — v2 replay order and live notices (B-116, B-50; upstream
//! `src/tests/acp-v2.test.ts:1147-1345`): the history replays before the
//! `session/resume` answer, an approved ExitPlanMode plan replays as a markdown
//! plan, a compact summary replays as one completed compaction entity, and a
//! live warning is a notice.
//!
//! Raw wire harness over a duplex transport, local `fake-claude`, bounded waits.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::session_store::{project_slug, projects_root_with_env};
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t7b-replay-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

/// The CLI's initialize and `set_permission_mode` steps for a session start.
fn transcript(tmp: &Path, name: &str) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {"models": [], "account": {}, "commands": []}
        }}]
    });
    let mode = json!({
        "expect": {"type": "control_request", "request": {"subtype": "set_permission_mode"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ", "response": {}
        }}]
    });
    std::fs::write(&path, format!("{init}\n{mode}\n")).expect("write transcript");
    path
}

fn options(tmp: &Path, name: &str) -> ServeOptions {
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript(tmp, name).to_string_lossy().into_owned(),
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
    }
}

/// A seeded source transcript in the projects directory of `config_dir`.
fn seed_session(tmp: &Path, session_id: &str, messages: &[Value]) {
    let config_dir = tmp.join("claude");
    let env = EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]);
    let cwd = repo_root();
    let dir = projects_root_with_env(&env).join(project_slug(&cwd.to_string_lossy()));
    std::fs::create_dir_all(&dir).expect("project dir");
    let body = messages
        .iter()
        .map(|m| m.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(dir.join(format!("{session_id}.jsonl")), body).expect("seed transcript");
}

fn entry(kind: &str, uuid: &str, parent: Option<&str>, session_id: &str, message: Value) -> Value {
    let mut value = json!({
        "type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": session_id,
        "timestamp": "2026-01-01T00:00:00.000Z", "message": message,
    });
    if kind == "assistant" {
        value["message"]["id"] = json!(format!("msg_{uuid}"));
        value["message"]["role"] = json!("assistant");
        value["message"]["model"] = json!("claude-sonnet-4-5");
    }
    value
}

struct Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl Harness {
    fn start(name: &str, steps: Option<&[Value]>) -> Self {
        let tmp = temp_dir(name);
        let mut opts = options(&tmp, name);
        if let Some(steps) = steps {
            let path = tmp.join("custom.transcript.jsonl");
            let text = steps
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::write(&path, text).expect("write transcript");
            opts.extra_env[0].1 = path.to_string_lossy().into_owned();
        }
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
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

    async fn initialize(&mut self) -> Value {
        self.send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":2,
                "info":{"name":"v2-test-client","version":"1.0.0"},
                "capabilities":{"auth":{"terminal":{}}}}}))
            .await;
        let mut updates = Vec::new();
        loop {
            let frame = self.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(1) && frame.get("method").is_none() {
                return json!({"response": frame, "updates": updates});
            }
            if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(frame["params"]["update"].clone());
            }
        }
    }

    /// Send `request` (with `id`) and read until its answer, collecting the
    /// session updates that precede it.
    async fn request_until_answer(
        &mut self,
        id: i64,
        method: &str,
        params: Value,
    ) -> (Value, Vec<Value>) {
        self.send_json(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        let mut updates = Vec::new();
        loop {
            let frame = self.read_json().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("method").is_none()
            {
                return (frame, updates);
            }
            if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(frame["params"]["update"].clone());
            }
        }
    }

    /// Read updates until the root's `idle` state.
    async fn read_until_idle(&mut self, session_id: &str) -> Vec<Value> {
        let mut updates = Vec::new();
        loop {
            let frame = self.read_json().await;
            if frame.get("method").and_then(Value::as_str) != Some("session/update") {
                continue;
            }
            let on = frame["params"]["sessionId"].as_str().unwrap_or("");
            let update = frame["params"]["update"].clone();
            let idle = on == session_id
                && update["sessionUpdate"] == "state_update"
                && update["state"] == "idle";
            updates.push(update);
            if idle {
                return updates;
            }
        }
    }

    async fn shutdown(mut self) {
        let _ = self.input.shutdown().await;
        let _ = timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// B-116/B-50 — the history replays before the `session/resume` answer: a
/// compact summary is one completed compaction entity at its position, and the
/// foreground state snapshot is the last update before the answer.
#[tokio::test]
async fn b116_v2_resume_replays_a_compact_summary_before_the_answer() {
    let source = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
    let summary = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n1. Primary Request and Intent:\n   Count upward.\n\nContinue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary.";
    let mut harness = Harness::start("compact", None);
    seed_session(
        &harness.tmp,
        source,
        &[
            entry(
                "user",
                "prompt-uuid",
                None,
                source,
                json!({"role":"user","content":[{"type":"text","text":"count"}]}),
            ),
            json!({"type":"user","uuid":"summary-uuid","parentUuid":"prompt-uuid",
                "sessionId":source,"isCompactSummary":true,
                "timestamp":"2026-01-01T00:00:01.000Z",
                "message":{"role":"user","content":[{"type":"text","text":summary}]}}),
            entry(
                "assistant",
                "a-after",
                Some("summary-uuid"),
                source,
                json!({"role":"assistant","content":[{"type":"text","text":"4"}]}),
            ),
        ],
    );
    let init = harness.initialize().await;
    assert_eq!(init["response"]["result"]["protocolVersion"], 2);
    let (answer, updates) = harness
        .request_until_answer(
            3,
            "session/resume",
            json!({"sessionId": source, "cwd": repo_root().to_string_lossy(),
                "replayFrom": {"type": "start"}}),
        )
        .await;
    assert!(answer.get("error").is_none(), "{answer}");

    // The materialized form only: no summary chunks.
    assert!(
        !updates.iter().any(|u| u["sessionUpdate"]
            .as_str()
            .is_some_and(|k| k.ends_with("_chunk")
                && u["content"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("Primary Request")))),
        "the summary is not chat text: {updates:?}"
    );
    let compaction = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("a completed compaction entity");
    assert_eq!(
        compaction,
        &json!({"sessionUpdate":"compaction_update","compactionId":"summary-uuid",
            "status":"completed","summary":[{"type":"text",
                "text":"1. Primary Request and Intent:\n   Count upward."}]})
    );
    // It sits between the messages around it.
    let compaction_pos = updates
        .iter()
        .position(|u| u["sessionUpdate"] == "compaction_update")
        .unwrap();
    assert!(
        updates[..compaction_pos]
            .iter()
            .any(|u| u["sessionUpdate"] == "user_message"),
        "the prompt replays before it: {updates:?}"
    );
    assert!(
        updates[compaction_pos + 1..]
            .iter()
            .any(|u| u["sessionUpdate"] == "agent_message"),
        "the answer after it: {updates:?}"
    );
    // The state snapshot is the last update before the answer.
    assert_eq!(
        updates.last().expect("an update")["sessionUpdate"],
        "state_update"
    );
    assert_eq!(updates.last().unwrap()["state"], "idle");
    harness.shutdown().await;
}

/// B-116 — an approved ExitPlanMode plan replays as the session's markdown plan.
#[tokio::test]
async fn b116_v2_resume_replays_an_approved_plan_as_markdown() {
    let source = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
    let mut harness = Harness::start("plan", None);
    seed_session(
        &harness.tmp,
        source,
        &[
            entry(
                "assistant",
                "a-plan",
                None,
                source,
                json!({"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_plan","name":"ExitPlanMode",
                     "input":{"plan":"# Plan\n1. Do it","planFilePath":"/plans/p.md"}}]}),
            ),
            entry(
                "user",
                "u-plan",
                Some("a-plan"),
                source,
                json!({"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_plan",
                     "content":"User has approved your plan. You can now start coding."}]}),
            ),
        ],
    );
    let init = harness.initialize().await;
    assert_eq!(init["response"]["result"]["protocolVersion"], 2);
    let (answer, updates) = harness
        .request_until_answer(
            3,
            "session/resume",
            json!({"sessionId": source, "cwd": repo_root().to_string_lossy(),
                "replayFrom": {"type": "start"}}),
        )
        .await;
    assert!(answer.get("error").is_none(), "{answer}");
    let plans: Vec<&Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "plan_update")
        .collect();
    assert_eq!(plans.len(), 1, "{updates:?}");
    assert_eq!(
        plans[0]["plan"],
        json!({"type":"markdown","planId":"plan-mode","content":"# Plan\n1. Do it"})
    );
    harness.shutdown().await;
}

/// B-116 — a live warning from Claude Code is a v2 notice, in its position in
/// the turn.
#[tokio::test]
async fn b116_v2_live_warning_is_a_notice() {
    let steps = vec![
        json!({
            "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ",
                "response": {"models": [], "account": {}, "commands": []}
            }}]
        }),
        json!({
            "expect": {"type": "user"},
            "emit": [
                {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
                 "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}},
                {"type": "system", "subtype": "informational", "level": "warning",
                 "content": "Stop hook blocked continuation\nThe hook said no."},
                {"type": "assistant", "message": {"id": "msg_1", "role": "assistant",
                    "model": "claude-sonnet-4-5",
                    "content": [{"type": "text", "text": "Done."}]}},
                {"type": "result", "subtype": "success", "stop_reason": "end_turn",
                 "is_error": false, "result": "Done.",
                 "usage": {"input_tokens": 1, "output_tokens": 1}},
            ],
        }),
    ];
    let mut harness = Harness::start("notice", Some(&steps));
    let init = harness.initialize().await;
    assert_eq!(init["response"]["result"]["protocolVersion"], 2);
    let (created, _) = harness
        .request_until_answer(
            2,
            "session/new",
            json!({"cwd": repo_root().to_string_lossy(), "mcpServers": []}),
        )
        .await;
    let session_id = created["result"]["sessionId"].clone();
    let (answer, mut updates) = harness
        .request_until_answer(
            3,
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type":"text","text":"hi"}]}),
        )
        .await;
    assert!(answer.get("error").is_none(), "{answer}");
    updates.extend(
        harness
            .read_until_idle(session_id.as_str().unwrap_or(""))
            .await,
    );
    let notice = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "notice")
        .expect("a notice");
    assert_eq!(
        notice,
        &json!({"sessionUpdate":"notice","severity":"warning",
            "title":"Stop hook blocked continuation","description":"The hook said no."})
    );
    let notice_pos = updates
        .iter()
        .position(|u| u["sessionUpdate"] == "notice")
        .unwrap();
    let chunk_pos = updates
        .iter()
        .position(|u| u["sessionUpdate"] == "agent_message_chunk")
        .expect("the answer");
    assert!(
        notice_pos < chunk_pos,
        "the notice precedes the answer: {updates:?}"
    );
    harness.shutdown().await;
}
