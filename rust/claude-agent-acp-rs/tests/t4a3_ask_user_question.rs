//! T4A3 STEP 1 (F23a: U-42/U-63/O-135/P-58) — AskUserQuestion wired through the
//! running agent.
//!
//! A form-capable client gets the built-in AskUserQuestion tool as an ACP form
//! elicitation (`elicitation/create`) whose answer becomes the tool's
//! `updatedInput`; the CLI's `can_use_tool` answer is written back from the
//! session actor. A client without elicitation keeps the tool disallowed
//! (argv), byte-identical to today.
//!
//! The client is a real `Client` over a duplex channel; `claude` is the
//! fake-claude transcript harness. Every wait is bounded.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use common::{fake_claude_binary, read_json, repo_root, wait_for_file};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Write a fake-claude transcript and return its path.
fn transcript(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    std::fs::write(&path, text).expect("write transcript");
    path
}

/// The init step shared by every transcript.
const INIT_STEP: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#;

/// The AskUserQuestion tool input used by the form tests.
fn ask_input() -> Value {
    json!({
        "questions": [{
            "question": "Which framework?",
            "header": "Framework",
            "multiSelect": false,
            "options": [{ "label": "Actix" }, { "label": "Axum" }]
        }]
    })
}

/// One transcript step: the prompt's echo + the tool use + the CLI's
/// `can_use_tool` control request for AskUserQuestion.
fn ask_turn_step() -> String {
    let input = ask_input();
    serde_json::to_string(&json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "ask" }],
                },
            },
            {
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_ask",
                        "name": "AskUserQuestion",
                        "input": input,
                    }],
                },
            },
            {
                "type": "control_request",
                "request_id": "perm-1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "AskUserQuestion",
                    "input": input,
                    "tool_use_id": "toolu_ask",
                    "permission_suggestions": [],
                },
            },
        ],
    }))
    .expect("serialize step")
}

/// A final step that asserts the exact `updatedInput` the CLI receives and then
/// finishes the turn.
fn answered_step(expected_answers: Value, annotations: Option<Value>) -> String {
    let mut updated = ask_input();
    updated["answers"] = expected_answers;
    if let Some(annotations) = annotations {
        updated["annotations"] = annotations;
    }
    serde_json::to_string(&json!({
        "expect": {
            "type": "control_response",
            "response": {
                "subtype": "success",
                "response": {
                    "behavior": "allow",
                    "updatedInput": updated,
                    "toolUseID": "toolu_ask",
                },
            },
        },
        "emit": [
            {
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_ask",
                        "content": "answered",
                    }],
                },
            },
            {
                "type": "result",
                "subtype": "success",
                "stop_reason": "end_turn",
                "is_error": false,
                "result": "ok",
                "usage": { "input_tokens": 1, "output_tokens": 1 },
            },
        ],
    }))
    .expect("serialize step")
}

/// What the fake client answers to an `elicitation/create`.
#[derive(Clone, Debug)]
enum Answer {
    Accept(Value),
    Decline,
    /// Never answer: the test cancels the turn while the dialog is open.
    Hang,
}

/// Drive one scenario: initialize (with `caps`), session/new, session/prompt.
/// Returns every session/update notification, every client request, and the
/// prompt response.
async fn drive(
    tmp: &Path,
    transcript_path: &Path,
    caps: Value,
    answer: Answer,
    cancel_after_elicitation: bool,
    argv_out: Option<&Path>,
) -> (Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>) {
    let root = repo_root();
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let mut extra_env = vec![(
        "FAKE_CLAUDE_SCRIPT".to_string(),
        transcript_path.to_string_lossy().into_owned(),
    )];
    if let Some(argv_out) = argv_out {
        extra_env.push((
            "FAKE_CLAUDE_ARGV_OUT".to_string(),
            argv_out.to_string_lossy().into_owned(),
        ));
    }
    let timings = claude_agent_acp_rs::process::Timings {
        force_cancel_grace: Duration::from_millis(1000),
        ..claude_agent_acp_rs::process::Timings::default()
    };
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env,
        default_cwd: Some(root.clone()),
        env: EnvSnapshot::empty(),
        timings,
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Value>();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Value>();
    let (elicit_tx, mut elicit_rx) = mpsc::unbounded_channel::<()>();
    // Notifications and requests arrive on separate channels; this one keeps
    // them in the order the client saw them, so a test can assert "the
    // tool_call came before the question".
    let (seq_tx, mut seq_rx) = mpsc::unbounded_channel::<Value>();

    let answer_for_handler = answer.clone();
    let result = Client
        .builder()
        .on_receive_notification(
            {
                let notif_tx = notif_tx.clone();
                let seq_tx = seq_tx.clone();
                async move |notification: UntypedMessage, _cx| {
                    let value = json!({
                        "method": notification.method,
                        "params": notification.params,
                    });
                    let _ = seq_tx.send(json!({"kind": "notification", "value": value.clone()}));
                    let _ = notif_tx.send(value);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let req_tx = req_tx.clone();
                let elicit_tx = elicit_tx.clone();
                let seq_tx = seq_tx.clone();
                async move |request: UntypedMessage, responder, _cx| {
                    let method = request.method.clone();
                    let params = request.params.clone();
                    let value = json!({"method": method, "params": params});
                    let _ = seq_tx.send(json!({"kind": "request", "value": value.clone()}));
                    let _ = req_tx.send(value);
                    if method == "elicitation/create" {
                        let _ = elicit_tx.send(());
                        match &answer_for_handler {
                            Answer::Accept(content) => {
                                return responder.respond(json!({
                                    "action": "accept",
                                    "content": content,
                                }));
                            }
                            Answer::Decline => {
                                return responder.respond(json!({"action": "decline"}));
                            }
                            Answer::Hang => {
                                // Hold the dialog open forever WITHOUT blocking
                                // the connection's request loop: the responder
                                // stays alive in a spawned task. The test cancels
                                // the turn, which withdraws the request.
                                tokio::spawn(async move {
                                    std::future::pending::<()>().await;
                                    drop(responder);
                                });
                                return Ok(());
                            }
                        }
                    }
                    responder
                        .respond(json!({"outcome": {"outcome": "selected", "optionId": "allow"}}))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_channel, {
            let cwd = cwd.clone();
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
                        json!({ "cwd": cwd.to_string_lossy() }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                let prompt_task = {
                    let connection = connection.clone();
                    let session_id = session_id.clone();
                    tokio::spawn(async move {
                        if let Ok(msg) = UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [{ "type": "text", "text": "ask" }],
                            }),
                        ) {
                            connection.send_request(msg).block_task().await
                        } else {
                            Err(agent_client_protocol::Error::invalid_params())
                        }
                    })
                };
                if cancel_after_elicitation {
                    // Wait for the dialog to be open, then cancel the turn.
                    if timeout(Duration::from_secs(10), elicit_rx.recv())
                        .await
                        .ok()
                        .flatten()
                        .is_none()
                    {
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("no elicitation arrived"));
                    }
                    connection.send_notification(
                        agent_client_protocol::schema::v1::CancelNotification::new(
                            session_id.clone(),
                        ),
                    )?;
                }
                let response = prompt_task.await.unwrap_or_else(|e| {
                    Err(agent_client_protocol::Error::internal_error().data(e.to_string()))
                })?;
                Ok::<Value, agent_client_protocol::Error>(response)
            }
        })
        .await;

    let mut notifications = Vec::new();
    while let Ok(value) = notif_rx.try_recv() {
        notifications.push(value);
    }
    let mut requests = Vec::new();
    while let Ok(value) = req_rx.try_recv() {
        requests.push(value);
    }
    let mut sequence = Vec::new();
    while let Ok(value) = seq_rx.try_recv() {
        sequence.push(value);
    }
    let responses = match result {
        Ok(value) => vec![value],
        Err(error) => vec![json!({"error": serde_json::to_value(error).unwrap_or(Value::Null)})],
    };
    drop(serve_task);
    (notifications, requests, responses, sequence)
}

fn elicitation_requests(requests: &[Value]) -> Vec<&Value> {
    requests
        .iter()
        .filter(|r| r["method"] == "elicitation/create")
        .collect()
}

fn notifications_of<'a>(notifications: &'a [Value], update: &str) -> Vec<&'a Value> {
    notifications
        .iter()
        .filter(|n| {
            n["method"] == "session/update" && n["params"]["update"]["sessionUpdate"] == update
        })
        .collect()
}

/// F23a/U-42 — a form-capable client gets the question as `elicitation/create`
/// and its answer is folded into the tool's `updatedInput`.
#[tokio::test]
async fn t4a3_ask_user_question_form_is_sent_and_answer_applied() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-form-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        ask_turn_step(),
        answered_step(json!({"Which framework?": "Axum"}), None)
    );
    let transcript_path = transcript(&tmp, "form", &text);

    let (notifications, requests, responses, _sequence) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({"question_0": "Axum"})),
        false,
        None,
    )
    .await;

    assert_eq!(responses.len(), 1, "{responses:?}");
    assert_eq!(
        responses[0]["stopReason"], "end_turn",
        "prompt settles: {responses:?}"
    );
    let elicitations = elicitation_requests(&requests);
    assert_eq!(elicitations.len(), 1, "one elicitation: {requests:?}");
    let params = &elicitations[0]["params"];
    assert_eq!(params["mode"], "form");
    assert_eq!(params["message"], "Which framework?");
    assert_eq!(params["toolCallId"], "toolu_ask");
    assert_eq!(params["requestedSchema"]["type"], "object");
    assert_eq!(
        params["requestedSchema"]["properties"]["question_0"]["title"],
        "Framework"
    );
    assert_eq!(
        params["requestedSchema"]["properties"]["question_0"]["oneOf"][1]["const"],
        "Axum"
    );
    // The tool call surfaced before the elicitation (`ensureToolCallEmitted`).
    let tool_calls = notifications_of(&notifications, "tool_call_update");
    assert!(
        !tool_calls.is_empty(),
        "the tool_call is surfaced before the question: {notifications:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/INV-20 (#851) — the CLI asks for permission before the tool_use block
/// streams: the session surfaces the `tool_call` itself (`ensureToolCallEmitted`)
/// before the question, so the client has the call the form references.
#[tokio::test]
async fn t4a3_ask_user_question_surfaces_the_tool_call_before_the_question() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-ensure-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let input = ask_input();
    let ask_step = serde_json::to_string(&json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "ask" }],
                },
            },
            {
                "type": "control_request",
                "request_id": "perm-1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "AskUserQuestion",
                    "input": input,
                    "tool_use_id": "toolu_ask",
                    "permission_suggestions": [],
                },
            },
        ],
    }))
    .expect("serialize step");
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        ask_step,
        answered_step(json!({"Which framework?": "Axum"}), None)
    );
    let transcript_path = transcript(&tmp, "ensure", &text);

    let (notifications, requests, responses, sequence) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({"question_0": "Axum"})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    assert_eq!(elicitation_requests(&requests).len(), 1, "{requests:?}");
    let elicitations = elicitation_requests(&requests);
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    let surfaced = notifications_of(&notifications, "tool_call")
        .into_iter()
        .find(|n| n["params"]["update"]["toolCallId"] == "toolu_ask")
        .unwrap_or_else(|| {
            panic!("the tool call is surfaced before the question: {notifications:?}")
        });
    assert_eq!(
        surfaced["params"]["sessionId"], elicitations[0]["params"]["sessionId"],
        "the call is surfaced in the session the question goes to"
    );
    // ORDER on the wire: the client saw the tool_call before the question.
    let tool_at = sequence
        .iter()
        .position(|entry| {
            entry["kind"] == "notification"
                && entry["value"]["method"] == "session/update"
                && entry["value"]["params"]["update"]["sessionUpdate"] == "tool_call"
                && entry["value"]["params"]["update"]["toolCallId"] == "toolu_ask"
        })
        .unwrap_or_else(|| panic!("the tool call is surfaced before the question: {sequence:?}"));
    let question_at = sequence
        .iter()
        .position(|entry| {
            entry["kind"] == "request" && entry["value"]["method"] == "elicitation/create"
        })
        .expect("the question is sent");
    assert!(
        tool_at < question_at,
        "the tool_call arrives before the elicitation/create: {sequence:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/U-63 — the custom "Other" field is the answer for a single-select
/// question when nothing was picked.
#[tokio::test]
async fn t4a3_ask_user_question_custom_other_answer() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-other-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        ask_turn_step(),
        answered_step(json!({"Which framework?": "Svelte"}), None)
    );
    let transcript_path = transcript(&tmp, "other", &text);

    let (_, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({"question_0_custom": "Svelte"})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    assert_eq!(elicitation_requests(&requests).len(), 1, "{requests:?}");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/U-42 — a custom "Other" beside a pick rides along as the per-question
/// `annotations[question].notes`, not as the answer.
#[tokio::test]
async fn t4a3_ask_user_question_custom_other_is_an_annotation() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-notes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        ask_turn_step(),
        answered_step(
            json!({"Which framework?": "Actix"}),
            Some(json!({"Which framework?": {"notes": "but I use Axum at home"}}))
        )
    );
    let transcript_path = transcript(&tmp, "notes", &text);

    let (_, _, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({
            "question_0": "Actix",
            "question_0_custom": "but I use Axum at home",
        })),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/U-42 — a decline answers the CLI with an allow whose answers are empty
/// (the model is told the user skipped; the turn does not abort).
#[tokio::test]
async fn t4a3_ask_user_question_decline_yields_empty_answers() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-decline-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        ask_turn_step(),
        answered_step(json!({}), None)
    );
    let transcript_path = transcript(&tmp, "decline", &text);

    let (_, _, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Decline,
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a — `session/cancel` while the question is open settles the turn
/// `cancelled`, answers the CLI with the abort (never a decline), and sends
/// `$/cancel_request` for the open dialog.
#[tokio::test]
async fn t4a3_ask_user_question_cancel_is_the_cancel_outcome() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    // The CLI receives the aborted tool use and ends the turn (the actor
    // cancelled it, so any result settles `cancelled`).
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        ask_turn_step(),
        r#"{"expect":{"type":"control_response","response":{"subtype":"error","error":"Tool use aborted"}},"emit":[{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ask","content":"aborted"}]}},{"type":"result","subtype":"success","stop_reason":"cancelled","is_error":false,"result":"","usage":{"input_tokens":1,"output_tokens":1}}]}"#
    );
    let transcript_path = transcript(&tmp, "cancel", &text);

    let (notifications, requests, responses, _sequence) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Hang,
        true,
        None,
    )
    .await;

    assert_eq!(responses.len(), 1, "{responses:?}");
    assert_eq!(
        responses[0]["stopReason"], "cancelled",
        "a cancelled question settles the turn cancelled: {responses:?}"
    );
    assert_eq!(elicitation_requests(&requests).len(), 1, "{requests:?}");
    // The open dialog is withdrawn: the agent drops the pending request, which
    // sends `$/cancel_request` to the client.
    assert!(
        notifications
            .iter()
            .any(|n| n["method"] == "$/cancel_request"),
        "the pending elicitation is cancelled on the wire: {notifications:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/O-135 — without `elicitation.form` AskUserQuestion stays in
/// `--disallowedTools` and no elicitation is sent.
#[tokio::test]
async fn t4a3_ask_user_question_disallowed_without_form_capability() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-nocap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"ask"}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"ok","usage":{"input_tokens":1,"output_tokens":1}}]}"#
    );
    let transcript_path = transcript(&tmp, "nocap", &text);
    let argv_out = tmp.join("argv.json");

    let (_, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({}),
        Answer::Accept(json!({})),
        false,
        Some(&argv_out),
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    assert!(
        elicitation_requests(&requests).is_empty(),
        "no elicitation for a client without the capability: {requests:?}"
    );
    wait_for_file(&argv_out, Duration::from_secs(10)).expect("argv recorded");
    let argv = read_json(&argv_out);
    let argv: Vec<&str> = argv["argv"]
        .as_array()
        .expect("argv array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        argv.iter()
            .any(|a| a.contains("AskUserQuestion") && a.starts_with("--disallowedTools=")),
        "AskUserQuestion is disallowed without elicitation.form: {argv:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/O-135 — with `elicitation.form` AskUserQuestion is not disallowed, and
/// the user's own `disallowedTools` are still merged.
#[tokio::test]
async fn t4a3_ask_user_question_allowed_with_form_capability() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"hi"}]}},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"ok","usage":{"input_tokens":1,"output_tokens":1}}]}"#
    );
    let transcript_path = transcript(&tmp, "cap", &text);
    let argv_out = tmp.join("argv.json");

    let (_, _, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({})),
        false,
        Some(&argv_out),
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    wait_for_file(&argv_out, Duration::from_secs(10)).expect("argv recorded");
    let argv = read_json(&argv_out);
    let argv: Vec<&str> = argv["argv"]
        .as_array()
        .expect("argv array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !argv.iter().any(|a| a.contains("AskUserQuestion")),
        "AskUserQuestion is not disallowed for a form-capable client: {argv:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/U-63 — an AIR client's custom-answer field carries the companion marker
/// under `_meta.jetbrains.air.customAnswer` and the legacy root key.
#[tokio::test]
async fn t4a3_ask_user_question_air_custom_answer_marker() {
    let tmp = std::env::temp_dir().join(format!("t4a3-ask-air-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n{}\n",
        INIT_STEP,
        ask_turn_step(),
        answered_step(json!({"Which framework?": "Axum"}), None)
    );
    let transcript_path = transcript(&tmp, "air", &text);

    let (_, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({
            "elicitation": {"form": {}},
            "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}},
        }),
        Answer::Accept(json!({"question_0": "Axum"})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    let elicitations = elicitation_requests(&requests);
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    let custom = &elicitations[0]["params"]["requestedSchema"]["properties"]["question_0_custom"];
    assert_eq!(
        custom["_meta"]["jetbrains"]["air"]["customAnswer"],
        json!({ "questionId": "question_0", "isCustomAnswer": true }),
        "{custom}"
    );
    assert_eq!(
        custom["_meta"]["_askUserQuestionCustomAnswer"],
        json!({ "questionId": "question_0", "isCustomAnswer": true }),
        "{custom}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// F23a/O-135 — the user's `disallowedTools` are merged with the adapter's
/// AskUserQuestion gate: form-capable keeps the user list only, otherwise
/// AskUserQuestion follows it.
#[test]
fn t4a3_ask_user_question_disallowed_tools_merge() {
    use claude_agent_acp_rs::agent::session_spawn_options;
    use claude_agent_acp_rs::sdk_options::ClaudeOptions;

    let claude = ClaudeOptions {
        disallowed_tools: vec!["Bash".to_string()],
        ..ClaudeOptions::default()
    };
    let with_form = session_spawn_options(
        &ServeOptions::default(),
        Some(std::env::temp_dir()),
        "sess".to_string(),
        claude.clone(),
        "default".to_string(),
        false,
        false,
    );
    assert_eq!(with_form.disallowed_tools, vec!["Bash".to_string()]);

    let without_form = session_spawn_options(
        &ServeOptions::default(),
        Some(std::env::temp_dir()),
        "sess".to_string(),
        claude,
        "default".to_string(),
        false,
        true,
    );
    assert_eq!(
        without_form.disallowed_tools,
        vec!["Bash".to_string(), "AskUserQuestion".to_string()]
    );
}
