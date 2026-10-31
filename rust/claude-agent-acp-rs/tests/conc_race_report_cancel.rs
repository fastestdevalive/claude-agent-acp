//! CONC-2 / review-T6INT finding 2: a `session/cancel` that lands during the
//! bounded file-change report preview wins.
//!
//! The turn settles `end_turn` at its result, then the adapter previews the
//! checkpoint (`rewind_files`, `dry_run: true`). A cancel inside that ≤2 s
//! preview must finish the report `unavailable/cancelled` and answer the prompt
//! `cancelled`, as Node's `cancel()` does (`acp-agent.ts:5486-5491`).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;

const INIT_STEP: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#;

fn edit_turn_step() -> String {
    serde_json::to_string(&json!({
        "expect": { "type": "user" },
        "emit": [
            {"type":"user","uuid":"$MATCH.uuid","isReplay":true,
             "message":{"role":"user","content":[{"type":"text","text":"change a file"}]}},
            {"type":"assistant","message":{"role":"assistant","content":[{
                "type":"tool_use","id":"toolu_edit","name":"Edit",
                "input":{"file_path":"src/changed.ts","old_string":"a","new_string":"b"}}]}},
            {"type":"user","message":{"role":"user","content":[{
                "type":"tool_result","tool_use_id":"toolu_edit","content":"Edited"}]}},
            {"type":"assistant","message":{"role":"assistant",
             "content":[{"type":"text","text":"Done"}]}},
            {"type":"result","subtype":"success","stop_reason":"end_turn",
             "is_error":false,"result":"Done",
             "usage":{"input_tokens":2,"output_tokens":2}}
        ]
    }))
    .expect("serialize step")
}

/// The checkpoint preview answered only after 600 ms: a cancel sent meanwhile
/// lands inside the preview window.
fn slow_rewind_step() -> String {
    serde_json::to_string(&json!({
        "expect": {"type":"control_request",
            "request":{"subtype":"rewind_files","dry_run":true}},
        "emit": [
            {"sleep_ms": 600},
            {"type":"control_response","response":{"subtype":"success","request_id":"$REQ",
                "response":{"canRewind":true,"filesChanged":["src/changed.ts"],
                    "insertions":4,"deletions":1}}}
        ]
    }))
    .expect("serialize step")
}

fn transcript(tmp: &Path, steps: &[String]) -> PathBuf {
    let path = tmp.join("cancel-report.transcript.jsonl");
    std::fs::write(&path, steps.join("\n") + "\n").expect("write transcript");
    path
}

#[tokio::test]
async fn conc_race_report_cancel_during_preview_wins() {
    let tmp = std::env::temp_dir().join(format!("conc-report-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let script = transcript(
        &tmp,
        &[INIT_STEP.to_string(), edit_turn_step(), slow_rewind_step()],
    );

    let root = repo_root();
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(cwd.join("src")).expect("create cwd");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            script.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(root.clone()),
        env: EnvSnapshot::empty(),
        timings: claude_agent_acp_rs::process::Timings {
            force_cancel_grace: Duration::from_millis(1000),
            ..claude_agent_acp_rs::process::Timings::default()
        },
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let (seq_tx, mut seq_rx) = mpsc::unbounded_channel::<Value>();
    let caps = json!({
        "_meta": {"jetbrains": {"air": {
            "version": 1,
            "capabilities": ["agentFileChangeReport", "sessionFailure"],
        }}}
    });
    let prompt_meta = json!({
        "jetbrains": {"air": {
            "agentFileChangeReportRequest": {"version": 1, "requestId": "request-cancel"}
        }}
    });

    let cwd_str = cwd.to_string_lossy().into_owned();
    let client_result = Client
        .builder()
        .on_receive_notification(
            {
                let seq_tx = seq_tx.clone();
                async move |notification: UntypedMessage, _cx| {
                    let _ = seq_tx.send(json!({
                        "method": notification.method,
                        "params": notification.params,
                    }));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({"protocolVersion": 1, "clientCapabilities": caps}),
                    )?)
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({"cwd": cwd_str, "mcpServers": []}),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                let prompt_conn = connection.clone();
                let prompt_session = session_id.clone();
                let prompt = tokio::spawn(async move {
                    prompt_conn
                        .send_request(UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": prompt_session,
                                "prompt": [{"type": "text", "text": "change a file"}],
                                "_meta": prompt_meta,
                            }),
                        )?)
                        .block_task()
                        .await
                });
                // The turn's result settles it, then the preview runs for 600 ms;
                // the cancel lands inside it.
                tokio::time::sleep(Duration::from_millis(250)).await;
                connection.send_notification(UntypedMessage::new(
                    "session/cancel",
                    json!({"sessionId": session_id}),
                )?)?;
                let response = tokio::time::timeout(Duration::from_secs(10), prompt)
                    .await
                    .expect("the prompt settles after the cancel")
                    .expect("join handle ok")
                    .expect("the prompt is answered");
                let _ = seq_tx.send(json!({"promptResponse": response}));
                Ok::<(), agent_client_protocol::Error>(())
            },
        );
    tokio::time::timeout(Duration::from_secs(20), client_result)
        .await
        .expect("the scenario is bounded")
        .expect("the client closure completes");
    serve_task.abort();
    let _ = serve_task.await;

    let mut response = Value::Null;
    let mut report = Value::Null;
    while let Ok(item) = seq_rx.try_recv() {
        if let Some(value) = item.get("promptResponse") {
            response = value.clone();
            continue;
        }
        if let Some(meta) = item.pointer("/params/update/_meta/jetbrains/air/agentFileChangeReport")
        {
            report = meta.clone();
        }
    }
    assert_eq!(
        response["stopReason"].as_str(),
        Some("cancelled"),
        "a cancel during the preview wins the turn: {response:?}"
    );
    assert_eq!(report["status"].as_str(), Some("unavailable"), "{report:?}");
    assert_eq!(report["reason"].as_str(), Some("cancelled"), "{report:?}");
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---- v2 (H1, `review-T7`) ---------------------------------------------------
//
// The v2 path holds the turn's `idle` emissions until the report published, so
// a cancel inside the preview must rewrite the held `idle` to `cancelled`
// exactly as `cancel()` settles the settling turn (`acp-agent.ts:8627-8685`).
// Synchronization is the result's `usage_update` frame (emitted immediately
// before `settle_events` starts the preview), never a fixed sleep; a seeded
// jitter only varies how soon after that observation the cancel is sent.

/// Seeded jitter, matching the CONC-2 harness (`Rng`/`jitter`): yields plus an
/// occasional sub-millisecond sleep, never a synchronization device.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self((seed ^ 0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.next().is_multiple_of(n)
    }
}

async fn jitter(rng: &mut Rng) {
    for _ in 0..rng.below(5) {
        tokio::task::yield_now().await;
    }
    if rng.one_in(4) {
        tokio::time::sleep(Duration::from_micros(rng.below(2_000) as u64)).await;
    }
}

/// The edit turn of the v1 scenario, with assistant usage so the result emits
/// the `usage_update` the client synchronizes on.
fn v2_edit_turn_step() -> String {
    serde_json::to_string(&json!({
        "expect": { "type": "user" },
        "emit": [
            {"type":"user","uuid":"$MATCH.uuid","isReplay":true,
             "message":{"role":"user","content":[{"type":"text","text":"change a file"}]}},
            {"type":"assistant","parent_tool_use_id":null,
             "message":{"role":"assistant","model":"claude-sonnet-4-5",
             "usage":{"input_tokens":2,"output_tokens":2,
                "cache_read_input_tokens":0,"cache_creation_input_tokens":0},
             "content":[{
                "type":"tool_use","id":"toolu_edit","name":"Edit",
                "input":{"file_path":"src/changed.ts","old_string":"a","new_string":"b"}}]}},
            {"type":"user","message":{"role":"user","content":[{
                "type":"tool_result","tool_use_id":"toolu_edit","content":"Edited"}]}},
            {"type":"assistant","parent_tool_use_id":null,
             "message":{"role":"assistant",
             "content":[{"type":"text","text":"Done"}]}},
            {"type":"result","parent_tool_use_id":null,
             "subtype":"success","stop_reason":"end_turn",
             "is_error":false,"result":"Done",
             "usage":{"input_tokens":2,"output_tokens":2}}
        ]
    }))
    .expect("serialize step")
}

struct V2Harness {
    input: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    tmp: PathBuf,
}

impl V2Harness {
    fn start(seed: u64, steps: &[String]) -> Self {
        let tmp = std::env::temp_dir().join(format!(
            "conc-report-cancel-v2-{}-{seed}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("temp dir");
        let config_dir = tmp.join("claude");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let script = transcript(&tmp, steps);
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let opts = ServeOptions {
            claude_path: Some(fake_claude_binary()),
            extra_env: vec![
                (
                    "FAKE_CLAUDE_SCRIPT".to_string(),
                    script.to_string_lossy().into_owned(),
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
            timings: claude_agent_acp_rs::process::Timings {
                force_cancel_grace: Duration::from_millis(1000),
                ..claude_agent_acp_rs::process::Timings::default()
            },
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
        let n = tokio::time::timeout(Duration::from_secs(20), self.reader.read_line(&mut line))
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

    async fn shutdown(mut self) {
        let _ = self.input.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// One seeded v2 iteration: wait for the result's `usage_update`, jitter, then
/// cancel inside the 600 ms preview. Returns the updates seen up to the idle.
async fn v2_iteration(seed: u64) -> Result<Vec<Value>, String> {
    let steps = vec![
        INIT_STEP.to_string(),
        v2_edit_turn_step(),
        slow_rewind_step(),
    ];
    let mut harness = V2Harness::start(seed, &steps);
    let cwd = harness.tmp.join("cwd");
    std::fs::create_dir_all(cwd.join("src")).expect("create cwd");
    let cwd_str = cwd.to_string_lossy().into_owned();

    harness
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":2,
            "info":{"name":"v2-cancel-client","version":"1.0.0"},
            "capabilities":{
                "auth":{"terminal":{}},
                "_meta":{"jetbrains":{"air":{
                    "version":1,
                    "capabilities":["agentFileChangeReport","sessionFailure"],
                }}},
            }}}))
        .await;
    let init = harness.read_response(1).await;
    if init["result"]["protocolVersion"] != 2 {
        return Err(format!("initialize failed: {init}"));
    }
    harness
        .send_json(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":cwd_str,"mcpServers":[]}}))
        .await;
    let created = harness.read_response(2).await;
    let session_id = created["result"]["sessionId"]
        .as_str()
        .ok_or_else(|| format!("session/new failed: {created}"))?
        .to_string();
    harness
        .send_json(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
        "params":{"sessionId":session_id,
            "prompt":[{"type":"text","text":"change a file"}],
            "_meta":{"jetbrains":{"air":{
                "agentFileChangeReportRequest":{"version":1,"requestId":"request-cancel"},
            }}}}}))
        .await;

    let mut updates = Vec::new();
    // Synchronize on the result's `usage_update`: the actor sends it just before
    // `settle_events` starts the checkpoint preview.
    loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            // The v2 prompt's intake answer (`messageId`) may arrive first.
            continue;
        }
        let update = frame["params"]["update"].clone();
        let reached = update["sessionUpdate"] == "usage_update";
        updates.push(update);
        if reached {
            break;
        }
    }

    let mut rng = Rng::new(seed);
    jitter(&mut rng).await;
    harness
        .send_json(json!({"jsonrpc":"2.0","method":"session/cancel",
            "params":{"sessionId":session_id}}))
        .await;

    // The held emissions (report, then the turn's idle) follow the preview.
    loop {
        let frame = harness.read_json().await;
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let update = frame["params"]["update"].clone();
        let idle = update["sessionUpdate"] == "state_update" && update["state"] == "idle";
        updates.push(update);
        if idle {
            break;
        }
    }
    harness.shutdown().await;
    Ok(updates)
}

#[tokio::test]
async fn conc_race_report_cancel_during_preview_wins_v2() {
    for seed in 1..=5u64 {
        let updates = tokio::time::timeout(Duration::from_secs(20), v2_iteration(seed))
            .await
            .unwrap_or_else(|_| panic!("seed {seed}: the scenario timed out"))
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));

        let report = updates
            .iter()
            .find_map(|update| {
                update
                    .pointer("/_meta/jetbrains/air/agentFileChangeReport")
                    .cloned()
            })
            .unwrap_or(Value::Null);
        assert_eq!(
            report["status"].as_str(),
            Some("unavailable"),
            "seed {seed}: the preview's cancel finishes the report: {report:?}"
        );
        assert_eq!(
            report["reason"].as_str(),
            Some("cancelled"),
            "seed {seed}: {report:?}"
        );

        let idles: Vec<&Value> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
            .collect();
        assert_eq!(idles.len(), 1, "seed {seed}: exactly one idle: {updates:?}");
        assert_eq!(
            idles[0]["stopReason"].as_str(),
            Some("cancelled"),
            "seed {seed}: a cancel during the v2 preview settles the turn cancelled, \
             never a stale idle end_turn: {updates:?}"
        );
    }
}
