//! `in_process` — the in-memory `Channel::duplex()` wiring (12.3).
//!
//! Mirrors vibe-station's future use (D9 / R23): `Channel::duplex()` gives two
//! connected ends; the agent takes one via [`serve`], and a
//! `Client::builder().connect_with(other_end, …)` takes the other. No subprocess
//! is spawned for the transport — the ACP boundary is entirely in-process.
//!
//! The example drives a minimal initialize → session/new → session/prompt
//! sequence and exits 0. It runs against `fake-claude` when
//! `CLAUDE_CODE_EXECUTABLE` + `FAKE_CLAUDE_SCRIPT` are set (as in 12.T4), or
//! against a real `claude` otherwise.

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use serde_json::json;

/// Environment variables that must reach the spawned `claude` child (they select
/// `fake-claude` in tests). Only these are forwarded to avoid leaking unrelated
/// env into the child.
const PASSTHROUGH_ENV: &[&str] = &[
    "FAKE_CLAUDE_SCRIPT",
    "FAKE_CLAUDE_ARGV_OUT",
    "FAKE_CLAUDE_INIT_OUT",
    "FAKE_CLAUDE_PID_OUT",
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let extra_env = PASSTHROUGH_ENV
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    let opts = ServeOptions {
        extra_env,
        ..ServeOptions::from_env()
    };

    // One in-memory ACP transport, two ends (R23).
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    Client
        .builder()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                // initialize
                let _init = connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({ "protocolVersion": 1 }),
                    )?)
                    .block_task()
                    .await?;
                // session/new
                let created = connection
                    .send_request(UntypedMessage::new("session/new", json!({}))?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
                if session_id.is_empty() {
                    return Err(agent_client_protocol::Error::internal_error()
                        .data("session/new returned no sessionId"));
                }
                // session/prompt
                let _result = connection
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        json!({
                            "sessionId": session_id,
                            "prompt": [ { "type": "text", "text": "hello" } ]
                        }),
                    )?)
                    .block_task()
                    .await?;
                Ok::<(), agent_client_protocol::Error>(())
            },
        )
        .await?;

    serve_task.await??;
    Ok(())
}
