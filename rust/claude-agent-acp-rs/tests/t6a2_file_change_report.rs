//! T6A2 STEP 8 (AG-3: A-15/B-80/U-48/T-89) — the native file-change report
//! through the running agent.
//!
//! An AIR client that negotiated `agentFileChangeReport` may ask for a report in
//! a prompt's `_meta`. After the turn's `result` the adapter previews Claude's
//! checkpoint (`rewind_files`, `dry_run: true`), normalises the paths and
//! publishes one `session_info_update` with
//! `_meta.jetbrains.air.agentFileChangeReport` BEFORE the prompt response.
//! A prompt that did not ask for a report inspects no checkpoint.
//!
//! `claude` is the fake-claude transcript harness; every wait is bounded.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

const INIT_STEP: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#;

/// The turn: the prompt echo, an Edit tool use, its successful result, the
/// answer text and the turn's `result`.
fn edit_turn_step() -> String {
    serde_json::to_string(&json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "change a file" }],
                },
            },
            {
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_edit",
                        "name": "Edit",
                        "input": {
                            "file_path": "src/changed.ts",
                            "old_string": "a",
                            "new_string": "b",
                        },
                    }],
                },
            },
            {
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_edit",
                        "content": "Edited",
                    }],
                },
            },
            {
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "Done" }],
                },
            },
            {
                "type": "result",
                "subtype": "success",
                "stop_reason": "end_turn",
                "is_error": false,
                "result": "Done",
                "usage": { "input_tokens": 2, "output_tokens": 2 },
            },
        ],
    }))
    .expect("serialize step")
}

/// The checkpoint preview: the adapter's `rewind_files` dry run, answered.
fn rewind_step() -> String {
    serde_json::to_string(&json!({
        "expect": {
            "type": "control_request",
            "request": { "subtype": "rewind_files", "dry_run": true },
        },
        "emit": [{
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "$REQ",
                "response": {
                    "canRewind": true,
                    "filesChanged": ["src/changed.ts"],
                    "insertions": 4,
                    "deletions": 1,
                },
            },
        }],
    }))
    .expect("serialize step")
}

fn transcript(tmp: &Path, name: &str, steps: &[String]) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    std::fs::write(&path, steps.join("\n") + "\n").expect("write transcript");
    path
}

/// One ordered record of what the client saw: a notification, a client request,
/// or the prompt response.
#[derive(Clone, Debug)]
enum Seen {
    Notification(Value),
    Request,
    PromptResponse(Value),
}

async fn drive(
    tmp: &Path,
    transcript_path: &Path,
    report_request: Option<&str>,
) -> (Vec<Seen>, PathBuf) {
    let root = repo_root();
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(cwd.join("src")).expect("create cwd");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
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

    let (seq_tx, mut seq_rx) = mpsc::unbounded_channel::<Seen>();
    let caps = json!({
        "_meta": {"jetbrains": {"air": {
            "version": 1,
            "capabilities": ["agentFileChangeReport", "sessionFailure"],
        }}}
    });
    let mut prompt_meta = json!({});
    if let Some(request_id) = report_request {
        prompt_meta = json!({
            "jetbrains": {"air": {
                "agentFileChangeReportRequest": {"version": 1, "requestId": request_id}
            }}
        });
    }

    let result = Client
        .builder()
        .on_receive_notification(
            {
                let seq_tx = seq_tx.clone();
                async move |notification: UntypedMessage, _cx| {
                    let _ = seq_tx.send(Seen::Notification(json!({
                        "method": notification.method,
                        "params": notification.params,
                    })));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let seq_tx = seq_tx.clone();
                async move |_request: UntypedMessage, responder, _cx| {
                    let _ = seq_tx.send(Seen::Request);
                    responder.respond(
                        json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_channel, {
            let cwd = cwd.clone();
            let prompt_meta = prompt_meta.clone();
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({ "protocolVersion": 1, "clientCapabilities": caps }),
                    )?)
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({
                            "cwd": cwd.to_string_lossy(),
                            "mcpServers": [],
                        }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                let response = connection
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        json!({
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": "change a file" }],
                            "_meta": prompt_meta,
                        }),
                    )?)
                    .block_task()
                    .await;
                let value = match response {
                    Ok(value) => value,
                    Err(error) => json!({"error": error.to_string()}),
                };
                let _ = seq_tx.send(Seen::PromptResponse(value));
                Ok(())
            }
        })
        .await;
    result.expect("the client connection completes");
    let mut seen = Vec::new();
    while let Ok(item) = seq_rx.try_recv() {
        seen.push(item);
    }
    serve_task.abort();
    (seen, cwd)
}

fn reports(seen: &[Seen]) -> Vec<Value> {
    seen.iter()
        .filter_map(|item| match item {
            Seen::Notification(value) => value
                .pointer("/params/update/_meta/jetbrains/air/agentFileChangeReport")
                .cloned(),
            _ => None,
        })
        .collect()
}

/// A-15/U-48: a turn that asks for a report publishes the checkpoint's paths
/// once, before the prompt response.
#[tokio::test]
async fn t6a2_prompt_report_publishes_the_checkpoint_paths_before_the_response() {
    let tmp = std::env::temp_dir().join(format!("t6a2-e2e-report-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let script = transcript(
        &tmp,
        "report",
        &[INIT_STEP.to_string(), edit_turn_step(), rewind_step()],
    );
    // review-T6INT finding 1: with an instant fake rewind the response must not
    // wait for the old second flush barrier's 1 s ack timeout.
    let started = std::time::Instant::now();
    let (seen, cwd) = timeout(
        Duration::from_secs(30),
        drive(&tmp, &script, Some("request-e2e")),
    )
    .await
    .expect("the scenario is bounded");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(300),
        "a reported turn answers well under the old 1 s stall (elapsed {elapsed:?})"
    );
    let canonical = std::fs::canonicalize(&cwd).expect("canonical cwd");

    let published = reports(&seen);
    assert_eq!(published.len(), 1, "{seen:?}");
    assert_eq!(
        published[0],
        json!({
            "version": 1,
            "requestId": "request-e2e",
            "status": "reported",
            "paths": [canonical.join("src/changed.ts").to_string_lossy()],
            "declaredComplete": false,
            "truncated": false,
        })
    );

    // The checkpoint dry run went to the CLI (the transcript's `rewind_step`
    // asserts `dry_run: true`; without it the report would be unavailable).

    // The report precedes the prompt response (`settleActive` awaits the
    // preview before `turn.settled`).
    let report_index = seen
        .iter()
        .position(|item| match item {
            Seen::Notification(value) => value
                .pointer("/params/update/_meta/jetbrains/air/agentFileChangeReport")
                .is_some(),
            _ => false,
        })
        .expect("a report was published");
    let response_index = seen
        .iter()
        .position(|item| matches!(item, Seen::PromptResponse(_)))
        .expect("the prompt was answered");
    assert!(
        report_index < response_index,
        "the session_info_update precedes the prompt response: {seen:?}"
    );
    let response = match &seen[response_index] {
        Seen::PromptResponse(value) => value.clone(),
        _ => unreachable!(),
    };
    assert_eq!(response["stopReason"], json!("end_turn"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// B-80/`acp-agent.ts:3881`: a prompt without a report request inspects no
/// checkpoint and publishes no report.
#[tokio::test]
async fn t6a2_prompt_without_a_report_request_inspects_no_checkpoint() {
    let tmp = std::env::temp_dir().join(format!("t6a2-e2e-noreport-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    // No rewind step: the transcript would fail if the adapter sent the request.
    let script = transcript(&tmp, "noreport", &[INIT_STEP.to_string(), edit_turn_step()]);
    let (seen, _cwd) = timeout(Duration::from_secs(30), drive(&tmp, &script, None))
        .await
        .expect("the scenario is bounded");
    // The transcript has no rewind step, so a preview request would fail the
    // turn; the report list being empty is the assertion that matters.
    assert!(reports(&seen).is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}
