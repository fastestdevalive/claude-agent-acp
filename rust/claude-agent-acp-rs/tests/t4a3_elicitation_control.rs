//! T4A3 STEP 2/3 (F23b: U-40/U-41/U-43/R-08/R-10, N-37) — the CLI's inbound
//! `elicitation` and `request_user_dialog` control requests, forwarded to a
//! capable client over ACP, and the `elicitation_complete` system message.
//!
//! The client is a real `Client` over a duplex channel; `claude` is the
//! fake-claude transcript harness. Every wait is bounded.

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

fn transcript(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    std::fs::write(&path, text).expect("write transcript");
    path
}

#[derive(Clone, Debug)]
enum Answer {
    Accept(Value),
    Decline,
    Error,
    Hang,
}

/// What the fake client does with the CLI's inbound control request: `frame` is
/// the frame the agent emits, `answer` is the client's answer to an
/// `elicitation/create`.
async fn drive(
    tmp: &Path,
    transcript_path: &Path,
    caps: Value,
    answer: Answer,
    cancel: bool,
    init_out: Option<&Path>,
) -> (Vec<Value>, Vec<Value>, Vec<Value>, Option<Value>) {
    let root = repo_root();
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let mut extra_env = vec![(
        "FAKE_CLAUDE_SCRIPT".to_string(),
        transcript_path.to_string_lossy().into_owned(),
    )];
    if let Some(init_out) = init_out {
        extra_env.push((
            "FAKE_CLAUDE_INIT_OUT".to_string(),
            init_out.to_string_lossy().into_owned(),
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

    let answer_for_handler = answer.clone();
    let result = Client
        .builder()
        .on_receive_notification(
            {
                let notif_tx = notif_tx.clone();
                async move |notification: UntypedMessage, _cx| {
                    let _ = notif_tx.send(json!({
                        "method": notification.method,
                        "params": notification.params,
                    }));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let req_tx = req_tx.clone();
                let elicit_tx = elicit_tx.clone();
                async move |request: UntypedMessage, responder, _cx| {
                    let method = request.method.clone();
                    let params = request.params.clone();
                    let request_id = serde_json::to_value(responder.id()).unwrap_or(Value::Null);
                    let _ = req_tx.send(json!({
                        "method": method,
                        "params": params,
                        "requestId": request_id,
                    }));
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
                            Answer::Error => {
                                return responder.respond_with_error(
                                    agent_client_protocol::Error::internal_error()
                                        .data("client refused the dialog"),
                                );
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
                    responder.respond(json!({}))
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
                                "prompt": [{ "type": "text", "text": "go" }],
                            }),
                        ) {
                            connection.send_request(msg).block_task().await
                        } else {
                            Err(agent_client_protocol::Error::invalid_params())
                        }
                    })
                };
                if cancel {
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

    // Give the forwarding task a bounded moment to flush a trailing
    // notification (e.g. `elicitation/complete` spawned off the actor).
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut notifications = Vec::new();
    while let Ok(value) = notif_rx.try_recv() {
        notifications.push(value);
    }
    let mut requests = Vec::new();
    while let Ok(value) = req_rx.try_recv() {
        requests.push(value);
    }
    let responses = match result {
        Ok(value) => vec![value],
        Err(error) => vec![json!({"error": serde_json::to_value(error).unwrap_or(Value::Null)})],
    };
    let init = init_out.and_then(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    });
    drop(serve_task);
    (notifications, requests, responses, init)
}

fn elicitation_requests(requests: &[Value]) -> Vec<&Value> {
    requests
        .iter()
        .filter(|r| r["method"] == "elicitation/create")
        .collect()
}

/// One turn: the prompt echo, the CLI's inbound control request, then (after the
/// agent's answer) the tool result and a result frame.
fn control_turn(control: Value, response_expect: Value) -> String {
    let first = json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "go" }],
                },
            },
            control,
        ],
    });
    let second = json!({
        "expect": { "type": "control_response", "response": response_expect },
        "emit": [
            {
                "type": "result",
                "subtype": "success",
                "stop_reason": "end_turn",
                "is_error": false,
                "result": "ok",
                "usage": { "input_tokens": 1, "output_tokens": 1 },
            },
        ],
    });
    format!(
        "{}\n{}\n",
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    )
}

fn mcp_form_control() -> Value {
    json!({
        "type": "control_request",
        "request_id": "elicit-1",
        "request": {
            "subtype": "elicitation",
            "serverName": "srv",
            "message": "Enter a value",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "val": { "type": "string" } },
            },
        },
    })
}

fn mcp_url_control() -> Value {
    json!({
        "type": "control_request",
        "request_id": "elicit-2",
        "request": {
            "subtype": "elicitation",
            "serverName": "srv",
            "message": "Log in",
            "mode": "url",
            "url": "https://example.com/auth",
            "elicitationId": "elic-123",
        },
    })
}

fn dialog_control(payload: Value) -> Value {
    json!({
        "type": "control_request",
        "request_id": "dialog-1",
        "request": {
            "subtype": "request_user_dialog",
            "dialog_kind": "refusal_fallback_prompt",
            "payload": payload,
        },
    })
}

fn refusal_payload() -> Value {
    json!({
        "originalModel": "claude-fable",
        "fallbackModel": "claude-opus",
        "apiRefusalCategory": "cyber",
        "guidanceText": "Try again",
    })
}

/// R-10/U-40 — a form elicitation is forwarded and its accept content is the
/// MCP result.
#[tokio::test]
async fn t4a3_mcp_elicitation_form_accept() {
    let tmp = std::env::temp_dir().join(format!("t4a3-elicit-form-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            mcp_form_control(),
            json!({
                "subtype": "success",
                "request_id": "elicit-1",
                "response": { "action": "accept", "content": { "val": "hi" } },
            })
        )
    );
    let transcript_path = transcript(&tmp, "form", &text);

    let (_, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({"val": "hi"})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    let elicitations = elicitation_requests(&requests);
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    let params = &elicitations[0]["params"];
    assert_eq!(params["mode"], "form");
    assert_eq!(params["message"], "Enter a value");
    assert_eq!(params["requestedSchema"]["type"], "object");
    assert_eq!(
        params["requestedSchema"]["properties"]["val"]["type"],
        "string"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-10/U-40 — a url elicitation carries the server's id and url.
#[tokio::test]
async fn t4a3_mcp_elicitation_url_accept() {
    let tmp = std::env::temp_dir().join(format!("t4a3-elicit-url-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            mcp_url_control(),
            json!({
                "subtype": "success",
                "request_id": "elicit-2",
                "response": { "action": "accept", "content": {} },
            })
        )
    );
    let transcript_path = transcript(&tmp, "url", &text);

    let (_, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"url": {}}}),
        Answer::Accept(json!({})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    let elicitations = elicitation_requests(&requests);
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    let params = &elicitations[0]["params"];
    assert_eq!(params["mode"], "url");
    assert_eq!(params["url"], "https://example.com/auth");
    assert_eq!(params["elicitationId"], "elic-123");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-10 — a decline maps to `{action:"decline"}`.
#[tokio::test]
async fn t4a3_mcp_elicitation_decline() {
    let tmp = std::env::temp_dir().join(format!("t4a3-elicit-decline-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            mcp_form_control(),
            json!({
                "subtype": "success",
                "request_id": "elicit-1",
                "response": { "action": "decline" },
            })
        )
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

/// R-10 — a client error is a decline, not a cancel.
#[tokio::test]
async fn t4a3_mcp_elicitation_client_error_declines() {
    let tmp = std::env::temp_dir().join(format!("t4a3-elicit-error-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            mcp_form_control(),
            json!({
                "subtype": "success",
                "request_id": "elicit-1",
                "response": { "action": "decline" },
            })
        )
    );
    let transcript_path = transcript(&tmp, "error", &text);

    let (_, _, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Error,
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-10 — a cancel while the dialog is open answers `{action:"cancel"}` and
/// withdraws the client request.
#[tokio::test]
async fn t4a3_mcp_elicitation_cancel_is_cancel() {
    let tmp = std::env::temp_dir().join(format!("t4a3-elicit-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            mcp_form_control(),
            json!({
                "subtype": "success",
                "request_id": "elicit-1",
                "response": { "action": "cancel" },
            })
        )
    );
    let transcript_path = transcript(&tmp, "cancel", &text);

    let started = std::time::Instant::now();
    let (notifications, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Hang,
        true,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "cancelled", "{responses:?}");
    let elicitations = elicitation_requests(&requests);
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    let cancelled = notifications
        .iter()
        .find(|n| n["method"] == "$/cancel_request")
        .expect("the open elicitation is cancelled on the wire");
    assert_eq!(
        cancelled["params"]["requestId"], elicitations[0]["requestId"],
        "the cancelled request is the elicitation/create: {notifications:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the cancel withdraws the form at once, not after a client timeout: {:?}",
        started.elapsed()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-08/U-43 — the refusal-fallback dialog is rendered as a consent form and
/// an explicit retry maps to `{behavior:"completed", result:"retry_fallback"}`.
#[tokio::test]
async fn t4a3_user_dialog_retry_fallback() {
    let tmp = std::env::temp_dir().join(format!("t4a3-dialog-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            dialog_control(refusal_payload()),
            json!({
                "subtype": "success",
                "request_id": "dialog-1",
                "response": { "behavior": "completed", "result": "retry_fallback" },
            })
        )
    );
    let transcript_path = transcript(&tmp, "retry", &text);

    let (_, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({"choice": "retry_fallback"})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    let elicitations = elicitation_requests(&requests);
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    let params = &elicitations[0]["params"];
    assert_eq!(params["mode"], "form");
    assert_eq!(
        params["message"],
        "claude-fable declined this request (cyber). Retry with claude-opus?\n\nTry again"
    );
    assert_eq!(
        params["requestedSchema"]["properties"]["choice"]["oneOf"][0]["const"],
        "retry_fallback"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-08/U-43 — anything but an explicit retry keeps the refusal.
#[tokio::test]
async fn t4a3_user_dialog_keep_refusal() {
    let tmp = std::env::temp_dir().join(format!("t4a3-dialog-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            dialog_control(refusal_payload()),
            json!({
                "subtype": "success",
                "request_id": "dialog-1",
                "response": { "behavior": "completed", "result": "cancelled" },
            })
        )
    );
    let transcript_path = transcript(&tmp, "keep", &text);

    let (_, _, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({"choice": "cancelled"})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-08/U-43 — a malformed payload cancels the dialog without rendering it.
#[tokio::test]
async fn t4a3_user_dialog_malformed_payload_cancels() {
    let tmp = std::env::temp_dir().join(format!("t4a3-dialog-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let text = format!(
        "{}\n{}\n",
        INIT_STEP,
        control_turn(
            dialog_control(json!({"originalModel": 3})),
            json!({
                "subtype": "success",
                "request_id": "dialog-1",
                "response": { "behavior": "cancelled" },
            })
        )
    );
    let transcript_path = transcript(&tmp, "bad", &text);

    let (_, requests, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    assert!(
        elicitation_requests(&requests).is_empty(),
        "a malformed payload is never rendered: {requests:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-08/R-10 — without elicitation capability the SDK defaults are kept: an
/// MCP elicitation is declined and a dialog gets no reply.
#[tokio::test]
async fn t4a3_no_capability_keeps_sdk_defaults() {
    let tmp = std::env::temp_dir().join(format!("t4a3-elicit-nocap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let init_out = tmp.join("init.json");
    // The elicitation is auto-declined; the dialog stays unanswered and the
    // result (emitted in the same step) ends the turn.
    let step = json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "go" }],
                },
            },
            mcp_form_control(),
            dialog_control(refusal_payload()),
            {
                "type": "result",
                "subtype": "success",
                "stop_reason": "end_turn",
                "is_error": false,
                "result": "ok",
                "usage": { "input_tokens": 1, "output_tokens": 1 },
            },
        ],
    });
    let text = format!("{}\n{}\n", INIT_STEP, serde_json::to_string(&step).unwrap());
    let transcript_path = transcript(&tmp, "nocap", &text);

    let (_, requests, responses, init) = drive(
        &tmp,
        &transcript_path,
        json!({}),
        Answer::Accept(json!({})),
        false,
        Some(&init_out),
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    assert!(
        elicitation_requests(&requests).is_empty(),
        "no client elicitation without the capability: {requests:?}"
    );
    let init = init.expect("initialize control_request captured");
    assert!(
        init["request"].get("supportedDialogKinds").is_none(),
        "a client without form elicitation gets no supportedDialogKinds: {init}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// R-08 — a form-capable client's initialize declares the refusal-fallback
/// dialog kind.
#[tokio::test]
async fn t4a3_supported_dialog_kinds_declared_for_form_client() {
    let tmp = std::env::temp_dir().join(format!("t4a3-dialog-kinds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let init_out = tmp.join("init.json");
    let step = json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "go" }],
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
    });
    let text = format!("{}\n{}\n", INIT_STEP, serde_json::to_string(&step).unwrap());
    let transcript_path = transcript(&tmp, "kinds", &text);

    let (_, _, responses, init) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"form": {}}}),
        Answer::Accept(json!({})),
        false,
        Some(&init_out),
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    let init = init.expect("initialize control_request captured");
    assert_eq!(
        init["request"]["supportedDialogKinds"],
        json!(["refusal_fallback_prompt"]),
        "the initialize declares the dialog kind: {init}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// N-37 — a system `elicitation_complete` calls `elicitation/complete` for a
/// url-capable client.
#[tokio::test]
async fn t4a3_elicitation_complete_system_message() {
    let tmp = std::env::temp_dir().join(format!("t4a3-complete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let step = json!({
        "expect": { "type": "user" },
        "emit": [
            {
                "type": "user",
                "uuid": "$MATCH.uuid",
                "isReplay": true,
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "go" }],
                },
            },
            {
                "type": "system",
                "subtype": "elicitation_complete",
                "elicitation_id": "elic-777",
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
    });
    let text = format!("{}\n{}\n", INIT_STEP, serde_json::to_string(&step).unwrap());
    let transcript_path = transcript(&tmp, "complete", &text);

    let (notifications, _, responses, _) = drive(
        &tmp,
        &transcript_path,
        json!({"elicitation": {"url": {}}}),
        Answer::Accept(json!({})),
        false,
        None,
    )
    .await;

    assert_eq!(responses[0]["stopReason"], "end_turn", "{responses:?}");
    let completes: Vec<&Value> = notifications
        .iter()
        .filter(|n| n["method"] == "elicitation/complete")
        .collect();
    assert_eq!(completes.len(), 1, "{notifications:?}");
    assert_eq!(completes[0]["params"]["elicitationId"], "elic-777");

    let _ = std::fs::remove_dir_all(&tmp);
}
