//! The recorder: drives an ACP agent over stdio and records its ordered frames.
//!
//! [`record`] spawns any ACP agent command, issues the calls described by a
//! [`Script`], and returns every JSON-RPC frame exchanged in both directions
//! in order (item 1.1, 1.2).

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest,
    PermissionOptionKind, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Client, ConnectionTo, LineDirection};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::frames::{Direction, Frame};
use crate::script::{PermissionPolicy, Script, Step};

/// How long [`record`] waits for a `cancel_after_updates` threshold before
/// giving up (a bound; a hang is a failure).
const UPDATE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// After `session/new` / `session/load` the adapter emits a trailing
/// `available_commands_update` via `setTimeout(0)`. Its position relative to
/// the next step (e.g. `session/prompt`) would otherwise race, making the
/// captured frame order non-deterministic. Pause long enough for that timer to
/// fire before the next step so the order is stable across captures (2.T1).
const POST_CREATE_SETTLE: Duration = Duration::from_millis(75);

/// Errors produced by the recorder.
#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to parse agent command: {0}")]
    AgentCommand(#[source] agent_client_protocol::Error),
    #[error("ACP protocol error: {0}")]
    Protocol(#[from] agent_client_protocol::Error),
}

/// Spawn `agent_cmd`, issue `script`, and return the ordered frames exchanged.
pub async fn record(agent_cmd: &str, script: &Script) -> Result<Vec<Frame>, Error> {
    let agent = parse_agent(agent_cmd)?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let frame_tx = tx.clone();
    let agent = agent.with_debug(move |line, direction| {
        let frame_direction = match direction {
            LineDirection::Stdin => Direction::Send,
            LineDirection::Stdout => Direction::Recv,
            LineDirection::Stderr => {
                if std::env::var_os("ACP_RECORDER_STDERR").is_some() {
                    eprintln!("[agent-stderr] {line}");
                }
                return;
            }
        };
        let Ok(json) = serde_json::from_str(line) else {
            return;
        };
        let _ = frame_tx.send(Frame {
            direction: frame_direction,
            json,
        });
    });

    // Shared count of `session/update` notifications, used to implement the
    // "cancel after N updates" prompt option.
    let update_count = Arc::new(AtomicUsize::new(0));
    let permission = script.permission;

    let outcome = Client
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
            async move |request: RequestPermissionRequest, responder, _connection| {
                responder.respond(reply_permission(request, permission))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, {
            let update_count = update_count.clone();
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let update_count = update_count.clone();
                async move { run_script(connection, script, update_count).await }
            }
        })
        .await;

    drop(tx);

    let mut frames = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        frames.push(frame);
    }

    // The ordered frames are the deliverable even when the script's final step
    // errored (e.g. an `error-result` corpus script, whose `session/prompt`
    // legitimately returns a JSON-RPC error). Only fail hard when the agent
    // never got far enough to exchange any frame (a real startup failure).
    finish(outcome, frames)
}

/// Decide the outcome of a recording given the script's `outcome` and the
/// frames captured so far.
fn finish(
    outcome: Result<(), agent_client_protocol::Error>,
    frames: Vec<Frame>,
) -> Result<Vec<Frame>, Error> {
    match outcome {
        Ok(()) => Ok(frames),
        Err(error) if frames.is_empty() => Err(Error::Protocol(error)),
        Err(_) => Ok(frames),
    }
}

fn parse_agent(agent_cmd: &str) -> Result<AcpAgent, Error> {
    AcpAgent::from_str(agent_cmd).map_err(Error::AgentCommand)
}

/// Answer a `session/request_permission` per the script policy.
fn reply_permission(
    request: RequestPermissionRequest,
    policy: PermissionPolicy,
) -> RequestPermissionResponse {
    match policy {
        PermissionPolicy::Allow => {
            let option = request
                .options
                .iter()
                .find(|o| {
                    o.kind == PermissionOptionKind::AllowOnce
                        || o.kind == PermissionOptionKind::AllowAlways
                })
                .or_else(|| request.options.first());
            match option {
                Some(option) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                    SelectedPermissionOutcome::new(option.option_id.clone()),
                )),
                None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
            }
        }
        PermissionPolicy::Deny => {
            let option = request.options.iter().find(|o| {
                o.kind == PermissionOptionKind::RejectOnce
                    || o.kind == PermissionOptionKind::RejectAlways
            });
            match option {
                Some(option) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                    SelectedPermissionOutcome::new(option.option_id.clone()),
                )),
                None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
            }
        }
    }
}

/// Issue every step of `script` over `connection`.
async fn run_script(
    connection: ConnectionTo<agent_client_protocol::Agent>,
    script: &Script,
    update_count: Arc<AtomicUsize>,
) -> Result<(), agent_client_protocol::Error> {
    let mut session_id: Option<agent_client_protocol::schema::v1::SessionId> = None;

    for step in &script.steps {
        match step {
            Step::Initialize => {
                let _response = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
            }
            Step::NewSession { cwd } => {
                let cwd = cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
                let response = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                session_id = Some(response.session_id);
                tokio::time::sleep(POST_CREATE_SETTLE).await;
            }
            Step::LoadSession {
                session_id: sid,
                cwd,
            } => {
                let cwd = cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
                let response = connection
                    .send_request(LoadSessionRequest::new(sid.clone(), cwd))
                    .block_task()
                    .await?;
                // `session/load` keeps the loaded session id active (the response
                // carries modes/configOptions, not a new id).
                let _ = response;
                session_id = Some(sid.clone().into());
                tokio::time::sleep(POST_CREATE_SETTLE).await;
            }
            Step::Prompt {
                text,
                cancel_after_updates,
            } => {
                let Some(sid) = session_id.as_ref() else {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("session/prompt before any session/new"));
                };
                let request = PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new(text.clone()))],
                );
                let started_at = update_count.load(Ordering::SeqCst);
                let sent = connection.send_request(request);
                if let Some(n) = cancel_after_updates {
                    let target = started_at + n;
                    if !wait_for_updates(&update_count, target, UPDATE_WAIT_TIMEOUT).await {
                        return Err(agent_client_protocol::Error::internal_error()
                            .data(format!("timed out waiting for {n} updates before cancel")));
                    }
                    connection.send_notification(CancelNotification::new(sid.clone()))?;
                }
                let _response = sent.block_task().await?;
            }
            Step::Cancel => {
                let Some(sid) = session_id.as_ref() else {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("session/cancel before any session/new"));
                };
                connection.send_notification(CancelNotification::new(sid.clone()))?;
            }
        }
    }

    Ok(())
}

/// Wait until the update counter reaches `target` (bounded by `timeout`, never
/// hangs).
async fn wait_for_updates(count: &AtomicUsize, target: usize, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if count.load(Ordering::SeqCst) >= target {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(UPDATE_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::Script;

    #[test]
    fn allow_selects_first_option() {
        let request = RequestPermissionRequest::new(
            "s1",
            tool_call(),
            vec![
                agent_client_protocol::schema::v1::PermissionOption::new(
                    "allow",
                    "Allow",
                    PermissionOptionKind::AllowOnce,
                ),
                agent_client_protocol::schema::v1::PermissionOption::new(
                    "reject",
                    "Reject",
                    PermissionOptionKind::RejectOnce,
                ),
            ],
        );
        let response = reply_permission(request, PermissionPolicy::Allow);
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(ref s) if s.option_id.to_string() == "allow"
        ));
    }

    #[test]
    fn deny_selects_reject_option() {
        let request = RequestPermissionRequest::new(
            "s1",
            tool_call(),
            vec![
                agent_client_protocol::schema::v1::PermissionOption::new(
                    "allow",
                    "Allow",
                    PermissionOptionKind::AllowOnce,
                ),
                agent_client_protocol::schema::v1::PermissionOption::new(
                    "reject",
                    "Reject",
                    PermissionOptionKind::RejectAlways,
                ),
            ],
        );
        let response = reply_permission(request, PermissionPolicy::Deny);
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(ref s) if s.option_id.to_string() == "reject"
        ));
    }

    #[test]
    fn deny_with_no_reject_cancels() {
        let request = RequestPermissionRequest::new(
            "s1",
            tool_call(),
            vec![agent_client_protocol::schema::v1::PermissionOption::new(
                "allow",
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        );
        let response = reply_permission(request, PermissionPolicy::Deny);
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Cancelled
        ));
    }

    fn tool_call() -> agent_client_protocol::schema::v1::ToolCallUpdate {
        agent_client_protocol::schema::v1::ToolCallUpdate::new(
            "tool-1",
            agent_client_protocol::schema::v1::ToolCallUpdateFields::new(),
        )
    }

    #[tokio::test]
    async fn wait_for_updates_returns_when_target_reached() {
        let count = Arc::new(AtomicUsize::new(3));
        assert!(wait_for_updates(&count, 3, Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn wait_for_updates_gives_up_after_timeout() {
        let count = Arc::new(AtomicUsize::new(0));
        assert!(!wait_for_updates(&count, usize::MAX, Duration::from_millis(50)).await);
    }

    #[test]
    fn post_create_settle_is_positive() {
        assert!(POST_CREATE_SETTLE > Duration::ZERO);
        assert!(POST_CREATE_SETTLE < Duration::from_secs(1));
    }

    #[test]
    fn finish_keeps_frames_on_script_error() {
        let frames = vec![Frame::recv(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {"code": -32603, "message": "Internal error: boom"}
        }))];
        let outcome: Result<(), agent_client_protocol::Error> =
            Err(agent_client_protocol::Error::internal_error().data("boom"));
        let result = finish(outcome, frames.clone());
        assert!(
            result.is_ok(),
            "captured frames survive a script-level error"
        );
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn finish_fails_when_no_frames_captured() {
        let outcome: Result<(), agent_client_protocol::Error> =
            Err(agent_client_protocol::Error::internal_error().data("no frames"));
        assert!(finish(outcome, vec![]).is_err());
    }

    #[test]
    fn finish_ok_passes_frames_through() {
        let frames = vec![Frame::send(serde_json::json!({"method": "initialize"}))];
        let result = finish(Ok(()), frames.clone());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn script_round_trip() {
        let script = Script::parse(r#"{"steps":[{"call":"initialize"}]}"#).unwrap();
        assert_eq!(script.steps.len(), 1);
    }
}
