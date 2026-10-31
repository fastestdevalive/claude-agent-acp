//! `claude-agent-acp-rs` — the drop-in stdio ACP binary (item 8.5).
//!
//! Runs [`serve`] over stdio with options from the environment. stdout carries
//! ONLY JSON-RPC ACP frames; all diagnostics go to stderr (never `println!`).
//!
//! Shutdown (12.2): the binary ends when the stdio transport closes (stdin EOF)
//! or on SIGTERM. Either way `serve` returns/drops, the session actors are torn
//! down, and the spawned `claude` child is disposed via the kill ladder — no
//! `claude` outlives the host (INV-6). On SIGTERM the process exits 0 within a
//! bounded deadline. The SIGTERM handler lives in `process.rs` (guard G9).

use agent_client_protocol::Stdio;

use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::process::wait_for_shutdown_signal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    for arg in std::env::args().skip(1) {
        if arg == "--version" || arg == "-V" || arg == "-v" {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            stdout.write_all(claude_agent_acp_rs::VERSION.as_bytes())?;
            stdout.write_all(b"\n")?;
            return Ok(());
        }
    }
    let serve_future = serve(Stdio::new(), ServeOptions::from_env());
    tokio::pin!(serve_future);
    tokio::select! {
        res = &mut serve_future => res?,
        _ = wait_for_shutdown_signal() => {}
    }
    Ok(())
}
