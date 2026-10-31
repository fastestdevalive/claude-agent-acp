//! `claude-agent-acp-rs` — the drop-in stdio ACP binary (item 8.5).
//!
//! Runs the ACP agent over stdio with options from the environment. stdout
//! carries ONLY JSON-RPC ACP frames; all diagnostics go to stderr (never
//! `println!`).
//!
//! Entry parity with the Node adapter's `index.ts` (ROADMAP F2b):
//!
//! - `--cli` is checked FIRST: the remaining argv is delegated to the native
//!   `claude` CLI with inherited stdio, signals are forwarded and the exit code
//!   is mirrored. `--version`/`-v`/`-V` apply only when not delegating.
//! - The managed-policy tier's `env` is applied to the spawn environment before
//!   the first SDK call (`managed-policy.ts`).
//! - `CLAUDE_AGENT_LOGS` selects a directory for `agent.log`
//!   (`{iso} pid={pid} {message}` lines, `index.ts:73-84`).
//! - SIGINT and SIGTERM dispose the serve loop, then the process exits 0
//!   (`index.ts:91-104`); stdin EOF does the same via `ServeHandle::closed`.
//! - The spawned `claude` child is disposed via the kill ladder — no `claude`
//!   outlives the host (INV-6). The signal handlers live in `process.rs`
//!   (guard G9).

use std::sync::Arc;

use claude_agent_acp_rs::agent::{serve_handle, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::logger::Logger;
use claude_agent_acp_rs::process::{
    resolve_claude_with_env, run_cli_passthrough, wait_for_shutdown_signal,
};
use claude_agent_acp_rs::settings;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = EnvSnapshot::from_process();

    // `--cli` is checked first (`index.ts:11-13`) so that `--version`/`-v` and
    // every other flag are forwarded to the wrapped native CLI instead of being
    // swallowed by our own version handler.
    if args.iter().any(|arg| arg == "--cli") {
        let forwarded: Vec<String> = args.into_iter().filter(|arg| arg != "--cli").collect();
        let binary = resolve_claude_with_env(None, &env)?;
        let code = run_cli_passthrough(&binary, &forwarded).await;
        std::process::exit(code);
    }

    for arg in &args {
        if arg == "--version" || arg == "-V" || arg == "-v" {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            stdout.write_all(claude_agent_acp_rs::VERSION.as_bytes())?;
            stdout.write_all(b"\n")?;
            return Ok(());
        }
    }

    // `CLAUDE_AGENT_LOGS` -> `<dir>/agent.log` (`index.ts:73-84`).
    let logger = Logger::from_env(&env);
    logger.log("Claude ACP started");

    let mut opts = ServeOptions::from_snapshot(env)
        .with_logger(logger.enabled().then(|| Arc::new(logger.clone())));

    // O-03: the managed-policy tier's env is applied before any SDK call so the
    // spawned `claude` inherits it. It has the LOWEST priority: an explicit
    // `extra_env` (provider env) overrides it, exactly like the Node adapter's
    // `process.env` mutation followed by `{...process.env, ...providerEnv}`.
    let policy = settings::apply_managed_policy_env(&logger);
    if !policy.is_empty() {
        let mut merged = policy;
        merged.extend(opts.extra_env);
        opts.extra_env = merged;
    }

    // The stdio wire wrapper applies Node's `ndJsonStream` rules (parse-error
    // shape, numeric outgoing request ids) in front of the pinned crate
    // (ROADMAP F2c).
    let mut handle = serve_handle(WireTransport::stdio(), opts);
    tokio::select! {
        // The transport closed (stdin EOF / transport error).
        _ = handle.closed() => {}
        // The host asked us to stop: dispose, then exit 0 below.
        _ = wait_for_shutdown_signal() => handle.dispose(),
    }
    // Wait for the serve loop to end. On the signal path this is the dispose
    // completing; the runtime then drops the session actors, disposing every
    // `claude` child (INV-6). On a natural close this returns immediately.
    handle.closed().await;
    Ok(())
}
