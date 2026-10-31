//! 12.T3 (INV-26) — `Channel::duplex()` and stdio yield identical ordered
//! frames.
//!
//! For each corpus script (text-only, single-tool, cancel-mid-turn) the frames
//! captured over an in-memory `Channel::duplex()` transport (the vibe-station
//! shape, D9 / R23) are diffed against the frames captured over stdio via the
//! `acp-recorder`. The agent is the same `serve` implementation in both cases;
//! only the transport differs, so the ordered frames must match.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Channel, Client, ConnectionTo, RawJsonRpcMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::{
    diff_frames, fake_claude_binary, normalize_root_paths, repo_root, run_recorder, Frame, JsonPath,
};
use serde::Deserialize;
use tokio::sync::mpsc;

/// The corpus scripts 12.T3 covers (text-only, single-tool, cancel-mid-turn).
const SCRIPTS: &[&str] = &["text-only", "single-tool", "cancel-mid-turn"];

/// The ignore set shared with the differentials (the fields the port
/// intentionally diverges on).
fn transport_ignore() -> Vec<JsonPath> {
    vec![
        JsonPath::new("result.modes"),
        JsonPath::new("result.configOptions"),
        JsonPath::new("result.models"),
        JsonPath::new("result.authMethods"),
        JsonPath::new("params.update[sessionUpdate=usage_update]"),
    ]
}

/// The subset of a corpus `.acp.json` script the channel driver needs.
#[derive(Debug, Deserialize)]
struct Script {
    #[serde(default = "default_permission")]
    permission: String,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "call", rename_all = "lowercase")]
#[allow(dead_code)]
enum Step {
    #[serde(rename = "initialize")]
    Initialize,
    #[serde(rename = "session/new")]
    NewSession { cwd: Option<PathBuf> },
    #[serde(rename = "session/prompt")]
    Prompt {
        text: String,
        #[serde(default)]
        cancel_after_updates: Option<usize>,
    },
    // Other step kinds (load/wait/etc.) are not exercised by 12.T3's scripts.
    #[serde(other)]
    Other,
}

fn default_permission() -> String {
    "allow".to_string()
}

/// Answer a `session/request_permission` per the script policy (mirrors the
/// recorder's `reply_permission`).
fn reply_permission(request: RequestPermissionRequest, policy: &str) -> RequestPermissionResponse {
    let want_allow = policy != "deny";
    let option = request.options.iter().find(|o| {
        if want_allow {
            o.kind == PermissionOptionKind::AllowOnce || o.kind == PermissionOptionKind::AllowAlways
        } else {
            o.kind == PermissionOptionKind::RejectOnce
                || o.kind == PermissionOptionKind::RejectAlways
        }
    });
    match option {
        Some(option) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(option.option_id.clone()),
        )),
        None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
    }
}

/// Drive `script` over the in-memory channel transport, returning the ordered
/// frames.
async fn run_over_channel(
    script_path: &std::path::Path,
    transcript: &std::path::Path,
) -> Vec<Frame> {
    let root = repo_root();
    let script: Script =
        serde_json::from_str(&std::fs::read_to_string(script_path).expect("read script"))
            .expect("parse script");

    // serve on one end, client on the other, with the bridge inspecting between.
    let (a1, a2) = Channel::duplex();
    let (b1, b2) = Channel::duplex();

    // Shorten the force-cancel backstop so the cancel-mid-turn settle (which
    // relies on it) completes fast, matching `run_recorder`'s
    // CLAUDE_ACP_FORCE_CANCEL_GRACE_MS=1000 on the stdio side (11.T4).
    let timings = claude_agent_acp_rs::process::Timings {
        force_cancel_grace: Duration::from_millis(1000),
        ..claude_agent_acp_rs::process::Timings::default()
    };
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(root.clone()),
        timings,
    };
    let serve_task = tokio::spawn(async move { serve(a1, opts).await });

    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<Frame>();
    let ftx = frame_tx.clone();
    // Bridge a2 (agent end) <-> b2 (client end), capturing every frame.
    let bridge_task = tokio::spawn(async move {
        let _ = Channel::bridge_with_inspection(
            a2,
            b2,
            // a2 -> b2 = agent -> client (Recv).
            move |msg: &RawJsonRpcMessage| {
                if let Ok(v) = serde_json::to_value(msg) {
                    let _ = ftx.send(Frame::recv(v));
                }
                Ok(())
            },
            // b2 -> a2 = client -> agent (Send).
            move |msg: &RawJsonRpcMessage| {
                if let Ok(v) = serde_json::to_value(msg) {
                    let _ = frame_tx.send(Frame::send(v));
                }
                Ok(())
            },
        )
        .await;
    });

    let update_count = Arc::new(AtomicUsize::new(0));
    let policy = script.permission.clone();

    let client_result = Client
        .builder()
        .on_receive_notification(
            {
                let update_count = update_count.clone();
                async move |_notification: SessionNotification, _cx| {
                    update_count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let policy = policy.clone();
                async move |request: RequestPermissionRequest, responder, _connection| {
                    responder.respond(reply_permission(request, &policy))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(b1, {
            let update_count = update_count.clone();
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let update_count = update_count.clone();
                async move { drive_script(&connection, &script, &update_count).await }
            }
        })
        .await;

    // The script is done: give the bridge a moment to flush any trailing
    // notification, then collect every captured frame.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = client_result;
    // Detach the still-running serve/bridge tasks (they hold the spawned
    // `claude` child and the transport; the runtime drops them at test end).
    std::mem::drop(serve_task);
    std::mem::drop(bridge_task);

    let mut frames = Vec::new();
    while let Ok(frame) = frame_rx.try_recv() {
        frames.push(frame);
    }
    frames
}

/// Issue every step of `script` over `connection` (subset of the recorder's
/// `run_script`: initialize / session/new / session/prompt with optional
/// cancel-after-N-updates).
async fn drive_script(
    connection: &ConnectionTo<agent_client_protocol::Agent>,
    script: &Script,
    update_count: &AtomicUsize,
) -> Result<(), agent_client_protocol::Error> {
    let mut session_id: Option<String> = None;
    for step in &script.steps {
        match step {
            Step::Initialize => {
                // Match the recorder's stdio client: the typed InitializeRequest
                // (carries clientCapabilities) so both transports send identical
                // wire params.
                let _resp = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
            }
            Step::NewSession { .. } => {
                let cwd = repo_root();
                let resp = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                session_id = Some(resp.session_id.to_string());
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
            Step::Prompt {
                text,
                cancel_after_updates,
            } => {
                let Some(sid) = session_id.as_ref() else {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("session/prompt before any session/new"));
                };
                let started = update_count.load(Ordering::SeqCst);
                let request = PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new(text.clone()))],
                );
                let sent = connection.send_request(request);
                if let Some(n) = cancel_after_updates {
                    let target = started + n;
                    if !wait_for_updates(update_count, target).await {
                        return Err(agent_client_protocol::Error::internal_error()
                            .data("timed out waiting for updates before cancel"));
                    }
                    connection.send_notification(CancelNotification::new(sid.clone()))?;
                }
                let _response = sent.block_task().await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Wait until the update counter reaches `target` (bounded).
async fn wait_for_updates(count: &AtomicUsize, target: usize) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while count.load(Ordering::SeqCst) < target {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// 12.T3 (INV-26) — text-only, single-tool and cancel scripts over
/// `Channel::duplex()` and over stdio yield identical ordered frames.
#[tokio::test]
async fn inv_26_transport_parity() {
    let root = repo_root();
    let ignore = transport_ignore();
    for name in SCRIPTS {
        let script = root.join(format!("porting/corpus/{name}.acp.json"));
        let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
        let tmp = std::env::temp_dir().join(format!("acp-parity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let out = tmp.join("out.jsonl");

        // stdio frames via the recorder (agent = the real binary).
        let agent_cmd = common::rust_agent_binary();
        let stdio_frames = run_recorder(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
        let stdio_frames = normalize_root_paths(stdio_frames, &root);

        // channel frames via the in-process serve + client.
        let channel_frames = run_over_channel(&script, &transcript).await;
        let channel_frames = normalize_root_paths(channel_frames, &root);

        if let Err(diff) = diff_frames(&channel_frames, &stdio_frames, &ignore) {
            panic!("{name} transport parity failed:\n{diff}");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
