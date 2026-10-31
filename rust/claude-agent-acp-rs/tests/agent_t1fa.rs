//! Agent-level tests for the Turn 1 fix round (lane A, T1FA), driving `serve` over
//! an in-memory `Channel::duplex()` transport with a real `Client`.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::{fake_claude_binary, repo_root};
use serde_json::json;
use tokio::time::timeout;

/// A transcript with only an `initialize` step (every spawn replays it).
fn init_only_transcript(tmp: &std::path::Path) -> std::path::PathBuf {
    let transcript = tmp.join("init-only.transcript.jsonl");
    std::fs::write(
        &transcript,
        "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[{\"value\":\"claude-sonnet-4-5\"}],\"commands\":[],\"account\":{}}}}]}\n",
    )
    .expect("write transcript");
    transcript
}

/// B-117 (finding 15): the teardown of a live session on a changed-params
/// `session/load` runs off the connection's sequential handler. A request sent
/// right behind the load is answered BEFORE the load's response, which still
/// arrives only once the old session is gone and the new one is registered.
#[tokio::test]
async fn t1fa_changed_params_load_does_not_block_other_requests() {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("acp-agent-t1fa-b117-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let transcript = init_only_transcript(&tmp);

    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(root.clone()),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let cwd = root.to_string_lossy().into_owned();
    let result = timeout(
        Duration::from_secs(60),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd = cwd.clone();
                async move {
                    let new = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({"cwd": cwd, "mcpServers": []}),
                        )?)
                        .block_task()
                        .await?;
                    let session_id = new["sessionId"].as_str().unwrap_or("").to_string();

                    let order = Arc::new(AtomicUsize::new(0));
                    let load_order = order.clone();
                    let other_order = order.clone();
                    let load = connection.send_request(UntypedMessage::new(
                        "session/load",
                        json!({
                            "sessionId": session_id,
                            "cwd": cwd,
                            "mcpServers": [],
                            "additionalDirectories": ["/tmp"],
                        }),
                    )?);
                    let other =
                        connection.send_request(UntypedMessage::new("t1fa/unknown", json!({}))?);
                    let (load_result, other_result) = tokio::join!(
                        async move {
                            let result = load.block_task().await;
                            (result, load_order.fetch_add(1, Ordering::SeqCst))
                        },
                        async move {
                            let result = other.block_task().await;
                            (result, other_order.fetch_add(1, Ordering::SeqCst))
                        },
                    );
                    let (load_result, load_at) = load_result;
                    let (other_result, other_at) = other_result;
                    assert!(other_result.is_err(), "an unknown method is rejected");
                    let load_result = load_result?;
                    assert_eq!(
                        load_result["sessionId"].as_str().unwrap_or(""),
                        session_id,
                        "the load answers with the recreated session"
                    );
                    assert!(
                        other_at < load_at,
                        "the request behind the load is served while the old session is torn down"
                    );

                    // The recreated session is registered: a second load of the
                    // same params reuses it instead of finding nothing.
                    let again = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            json!({
                                "sessionId": session_id,
                                "cwd": cwd,
                                "mcpServers": [],
                                "additionalDirectories": ["/tmp"],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    assert_eq!(again["sessionId"].as_str().unwrap_or(""), session_id);
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        ),
    )
    .await;
    assert!(result.is_ok(), "the exchange finished in time");
    assert!(result.unwrap().is_ok(), "the exchange succeeded");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let _ = std::fs::remove_dir_all(&tmp);
}
