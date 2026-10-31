//! T4A3 STEP 4 (F23c: A-01/A-25/S-02) — MCP OAuth: the `mcp_authenticate`
//! control request, the URL elicitation, the status poll (10 min bound) and the
//! `elicitation/complete` on every outcome.
//!
//! The helper tests use paused time; the wiring test drives the real agent with
//! the fake-claude transcript harness and a real client.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::elicitation::{authenticate_single_mcp_server, MCP_OAUTH_TIMEOUT};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

/// A-01 — success: the URL dialog is presented, the poll reports `connected`
/// and `elicitation/complete` closes it.
#[tokio::test(start_paused = true)]
async fn t4a3_oauth_success_presents_and_completes() {
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let (elicit_tx, mut elicit_rx) = mpsc::unbounded_channel::<Value>();
    let (complete_tx, mut complete_rx) = mpsc::unbounded_channel::<Value>();
    let result = authenticate_single_mcp_server(
        "sess",
        "srv",
        || async { Ok(json!({"requiresUserAction": true, "authUrl": "https://auth.example"})) },
        || async { Ok(Some("connected".to_string())) },
        move |params| {
            let tx = elicit_tx.clone();
            async move {
                let _ = tx.send(params);
                Ok(json!({"action": "accept"}))
            }
        },
        move |params| {
            let tx = complete_tx.clone();
            async move {
                let _ = tx.send(params);
                Ok(())
            }
        },
        cancel_rx,
    )
    .await;

    assert!(result.expect("oauth settles"));
    let request = elicit_rx.try_recv().expect("a url elicitation is sent");
    assert_eq!(request["mode"], "url");
    assert_eq!(request["message"], "Authenticate with MCP server srv");
    assert_eq!(request["url"], "https://auth.example");
    assert!(
        request["elicitationId"]
            .as_str()
            .is_some_and(|id| id.starts_with("mcp-oauth-")),
        "{request}"
    );
    let complete = complete_rx.try_recv().expect("completeElicitation is sent");
    assert_eq!(complete["elicitationId"], request["elicitationId"]);
}

/// A-01 — a missing `requiresUserAction` means no dialog and no polling.
#[tokio::test(start_paused = true)]
async fn t4a3_oauth_without_user_action_returns_true() {
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let polls = Arc::new(AtomicUsize::new(0));
    let polls_for_poll = polls.clone();
    let result = authenticate_single_mcp_server(
        "sess",
        "srv",
        || async { Ok(json!({})) },
        move || {
            let polls = polls_for_poll.clone();
            async move {
                polls.fetch_add(1, Ordering::SeqCst);
                Ok(Some("pending".to_string()))
            }
        },
        |_params| async { Ok(json!({"action": "accept"})) },
        |_params| async { Ok(()) },
        cancel_rx,
    )
    .await;

    assert!(result.expect("oauth settles"));
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "no poll without user action"
    );
}

/// A-01 — a poll that never connects times out after 10 minutes and still
/// dismisses the client dialog.
#[tokio::test(start_paused = true)]
async fn t4a3_oauth_timeout_still_completes() {
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let (complete_tx, mut complete_rx) = mpsc::unbounded_channel::<Value>();
    let started = tokio::time::Instant::now();
    let result = authenticate_single_mcp_server(
        "sess",
        "srv",
        || async { Ok(json!({"requiresUserAction": true, "authUrl": "https://auth.example"})) },
        || async { Ok(Some("pending".to_string())) },
        |_params| async { Ok(json!({"action": "accept"})) },
        move |params| {
            let tx = complete_tx.clone();
            async move {
                let _ = tx.send(params);
                Ok(())
            }
        },
        cancel_rx,
    )
    .await;

    assert!(!result.expect("oauth settles"));
    assert!(
        started.elapsed() >= MCP_OAUTH_TIMEOUT,
        "the poll kept its 10 min bound: {:?}",
        started.elapsed()
    );
    assert!(
        complete_rx.try_recv().is_ok(),
        "the URL dialog is dismissed on timeout"
    );
}

/// A-01 — the session's abort signal tears the poll down without completing.
#[tokio::test(start_paused = true)]
async fn t4a3_oauth_close_aborts_the_poll() {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (complete_tx, mut complete_rx) = mpsc::unbounded_channel::<Value>();
    // Drop the sender before the helper runs: the session is gone.
    drop(cancel_tx);
    let result = authenticate_single_mcp_server(
        "sess",
        "srv",
        || async { Ok(json!({"requiresUserAction": true, "authUrl": "https://auth.example"})) },
        || async { Ok(Some("pending".to_string())) },
        |_params| async { Ok(json!({"action": "accept"})) },
        move |params| {
            let tx = complete_tx.clone();
            async move {
                let _ = tx.send(params);
                Ok(())
            }
        },
        cancel_rx,
    )
    .await;

    assert!(!result.expect("oauth settles"));
    assert!(
        complete_rx.try_recv().is_err(),
        "an aborted flow completes nothing"
    );
}

fn transcript(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(format!("{name}.transcript.jsonl"));
    std::fs::write(&path, text).expect("write transcript");
    path
}

const INIT_STEP: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#;

/// A-01/S-02 — `session/new` with an ACP server starts the OAuth flow in the
/// background: `mcp_status` -> `mcp_authenticate` -> URL elicitation -> status
/// poll -> `elicitation/complete`.
#[tokio::test]
async fn t4a3_session_start_runs_mcp_oauth() {
    let tmp = std::env::temp_dir().join(format!("t4a3-oauth-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let text = format!(
        "{}\n{}\n{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"needs-auth"}]}}}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_authenticate"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"requiresUserAction":true,"authUrl":"https://auth.example","callbackExpected":true}}}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"connected"}]}}}]}"#,
    );
    let transcript_path = transcript(&tmp, "oauth", &text);
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Value>();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Value>();
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
                async move |request: UntypedMessage, responder, _cx| {
                    let _ = req_tx.send(json!({
                        "method": request.method,
                        "params": request.params,
                    }));
                    responder.respond(json!({"action": "accept", "content": {}}))
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
                        json!({
                            "protocolVersion": 1,
                            "clientCapabilities": { "elicitation": { "url": {} } },
                        }),
                    )?)
                    .block_task()
                    .await?;
                connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({
                            "cwd": cwd.to_string_lossy(),
                            "mcpServers": [{ "name": "srv", "command": "srv-mcp", "args": [] }],
                        }),
                    )?)
                    .block_task()
                    .await?;
                // Wait (bounded) for the completion notification.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if let Ok(Some(value)) =
                        timeout(Duration::from_millis(100), notif_rx.recv()).await
                    {
                        if value["method"] == "elicitation/complete" {
                            return Ok::<Value, agent_client_protocol::Error>(value);
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("no elicitation/complete arrived"));
                    }
                }
            }
        })
        .await;

    assert!(result.is_ok(), "the OAuth flow completes: {result:?}");
    let complete = result.unwrap();
    let mut requests = Vec::new();
    while let Ok(value) = req_rx.try_recv() {
        requests.push(value);
    }
    let elicitations: Vec<&Value> = requests
        .iter()
        .filter(|r| r["method"] == "elicitation/create")
        .collect();
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    assert_eq!(elicitations[0]["params"]["mode"], "url");
    assert_eq!(elicitations[0]["params"]["url"], "https://auth.example");
    assert_eq!(
        complete["params"]["elicitationId"], elicitations[0]["params"]["elicitationId"],
        "the completion dismisses the dialog the flow opened"
    );

    drop(serve_task);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// F1 — a `session/cancel` while an MCP OAuth URL flow is open does NOT tear the
/// flow down (upstream ends OAuth on session close only, `acp-agent.ts:2614-2617`):
/// the client's dialog is answered and `elicitation/complete` still arrives.
#[tokio::test]
async fn t4a3_cancel_does_not_end_the_mcp_oauth_flow() {
    let tmp = std::env::temp_dir().join(format!("t4a3-oauth-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let text = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n",
        INIT_STEP,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"needs-auth"}]}}}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_authenticate"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"requiresUserAction":true,"authUrl":"https://auth.example","callbackExpected":true}}}]}"#,
        // The first status poll still says needs-auth: completion can only come
        // from the SECOND poll, 1 s later, well after the cancel. A cancel that
        // ended the flow would leave no `elicitation/complete` at all.
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"needs-auth"}]}}}]}"#,
        // `session/cancel` interrupts the CLI; the flow must keep polling after
        // it (the fake leaves the interrupt unanswered).
        r#"{"expect":{"type":"control_request","request":{"subtype":"interrupt"}},"emit":[]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"connected"}]}}}]}"#,
    );
    let transcript_path = transcript(&tmp, "oauth-cancel", &text);
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let (notif_tx, notif_rx) = mpsc::unbounded_channel::<Value>();
    let (req_tx, req_rx) = mpsc::unbounded_channel::<Value>();
    // Every notification, kept after the flow ends so the test can prove the
    // cancel did not send `$/cancel_request` for the open URL dialog.
    let (audit_tx, mut audit_rx) = mpsc::unbounded_channel::<Value>();
    let result = Client
        .builder()
        .on_receive_notification(
            {
                let notif_tx = notif_tx.clone();
                let audit_tx = audit_tx.clone();
                async move |notification: UntypedMessage, _cx| {
                    let value = json!({
                        "method": notification.method,
                        "params": notification.params,
                    });
                    let _ = audit_tx.send(value.clone());
                    let _ = notif_tx.send(value);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let req_tx = req_tx.clone();
                async move |request: UntypedMessage, responder, _cx| {
                    let _ = req_tx.send(json!({
                        "method": request.method,
                        "params": request.params,
                    }));
                    // Keep the URL dialog open across the cancel.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    responder.respond(json!({"action": "accept"}))
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
                        json!({
                            "protocolVersion": 1,
                            "clientCapabilities": { "elicitation": { "url": {} } },
                        }),
                    )?)
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({
                            "cwd": cwd.to_string_lossy(),
                            "mcpServers": [{ "name": "srv", "command": "srv-mcp", "args": [] }],
                        }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                let mut notif_rx = notif_rx;
                let mut req_rx = req_rx;
                let mut cancelled = false;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if !cancelled {
                        if let Ok(Some(value)) =
                            timeout(Duration::from_millis(50), req_rx.recv()).await
                        {
                            if value["method"] == "elicitation/create" {
                                // Let the first status poll (sent right after the
                                // authenticate response) be answered before the
                                // cancel's `interrupt` reaches the CLI, so the
                                // scripted steps stay ordered.
                                tokio::time::sleep(Duration::from_millis(400)).await;
                                connection.send_notification(
                                    agent_client_protocol::schema::v1::CancelNotification::new(
                                        session_id.clone(),
                                    ),
                                )?;
                                cancelled = true;
                            }
                        }
                    }
                    if let Ok(Some(value)) =
                        timeout(Duration::from_millis(50), notif_rx.recv()).await
                    {
                        if value["method"] == "elicitation/complete" {
                            return Ok::<Value, agent_client_protocol::Error>(value);
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("no elicitation/complete arrived after the cancel"));
                    }
                }
            }
        })
        .await;

    assert!(
        result.is_ok(),
        "a cancel does not end the OAuth flow: {result:?}"
    );
    // The open URL dialog must survive the cancel: no `$/cancel_request` for it
    // BEFORE `elicitation/complete`. (One may follow the complete, when the
    // request that was already answered is dropped.)
    let mut notifications = Vec::new();
    while let Ok(value) = audit_rx.try_recv() {
        notifications.push(value);
    }
    let complete_at = notifications
        .iter()
        .position(|n| n["method"] == "elicitation/complete")
        .expect("the complete was seen");
    let cancel_before = notifications
        .iter()
        .take(complete_at)
        .any(|n| n["method"] == "$/cancel_request");
    assert!(
        !cancel_before,
        "the cancel leaves the OAuth dialog open: {notifications:?}"
    );
    drop(serve_task);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// S-02/A-01 — `/mcp reconnect <server>` starts OAuth for the ACP servers it
/// reconnected, as session start does.
#[tokio::test]
async fn t4a3_mcp_reconnect_starts_oauth() {
    let tmp = std::env::temp_dir().join(format!("t4a3-oauth-reconnect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let text = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        INIT_STEP,
        // The session-start OAuth check: the server is already connected, so
        // only the reconnect's follow-up starts a flow.
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"connected"}]}}}]}"#,
        // The `/mcp` turn: echo, the CLI's own text, and a result held until the
        // replacement is ready.
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"/mcp reconnect srv"}]}},{"type":"system","subtype":"local_command_output","uuid":"$MATCH.uuid","content":"Reconnect, enable, and disable aren't available in this session."},{"type":"result","subtype":"success","stop_reason":"end_turn","is_error":false,"result":"done","usage":{"input_tokens":1,"output_tokens":1}}]}"#,
        // The run's first status read.
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"needs-auth"}]}}}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_reconnect","serverName":"srv"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{}}}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"needs-auth"}]}}}]}"#,
        // The OAuth run: status, authenticate, poll.
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"needs-auth"}]}}}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_authenticate","serverName":"srv"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"requiresUserAction":true,"authUrl":"https://auth.example","callbackExpected":true}}}]}"#,
        r#"{"expect":{"type":"control_request","request":{"subtype":"mcp_status"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"mcpServers":[{"name":"srv","status":"connected"}]}}}]}"#,
    );
    let transcript_path = transcript(&tmp, "reconnect", &text);
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript_path.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Value>();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Value>();
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
                async move |request: UntypedMessage, responder, _cx| {
                    let _ = req_tx.send(json!({
                        "method": request.method,
                        "params": request.params,
                    }));
                    responder.respond(json!({"action": "accept", "content": {}}))
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
                        json!({
                            "protocolVersion": 1,
                            "clientCapabilities": { "elicitation": { "url": {} } },
                        }),
                    )?)
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({
                            "cwd": cwd.to_string_lossy(),
                            "mcpServers": [{ "name": "srv", "command": "srv-mcp", "args": [] }],
                        }),
                    )?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                let _prompt = connection
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        json!({
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": "/mcp reconnect srv" }],
                        }),
                    )?)
                    .block_task()
                    .await?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if let Ok(Some(value)) =
                        timeout(Duration::from_millis(100), notif_rx.recv()).await
                    {
                        if value["method"] == "elicitation/complete" {
                            return Ok::<Value, agent_client_protocol::Error>(value);
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("no elicitation/complete arrived"));
                    }
                }
            }
        })
        .await;

    assert!(
        result.is_ok(),
        "a reconnect starts the OAuth flow and completes it: {result:?}"
    );
    let complete = result.unwrap();
    let mut requests = Vec::new();
    while let Ok(value) = req_rx.try_recv() {
        requests.push(value);
    }
    let elicitations: Vec<&Value> = requests
        .iter()
        .filter(|r| r["method"] == "elicitation/create")
        .collect();
    assert_eq!(elicitations.len(), 1, "{requests:?}");
    assert_eq!(elicitations[0]["params"]["mode"], "url");
    assert_eq!(
        complete["params"]["elicitationId"],
        elicitations[0]["params"]["elicitationId"]
    );

    drop(serve_task);
    let _ = std::fs::remove_dir_all(&tmp);
}
