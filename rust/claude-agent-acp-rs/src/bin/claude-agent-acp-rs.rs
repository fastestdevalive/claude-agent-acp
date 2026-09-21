//! `claude-agent-acp-rs` — the drop-in stdio ACP binary (item 8.5).
//!
//! Runs [`serve`] over stdio with options from the environment. stdout carries
//! ONLY JSON-RPC ACP frames; all diagnostics go to stderr (never `println!`).

use agent_client_protocol::Stdio;

use claude_agent_acp_rs::agent::{serve, ServeOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Stdio is the ACP transport; ServeOptions are read from the environment
    // (CLAUDE_CODE_EXECUTABLE for the claude binary path).
    serve(Stdio::new(), ServeOptions::from_env()).await?;
    Ok(())
}
