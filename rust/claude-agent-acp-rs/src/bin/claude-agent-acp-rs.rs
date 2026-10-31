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

// Decision D8: the bin target is a separate crate root, so it needs its own
// unsafe lint. It never uses FFI; the gate also greps the whole crate.
#![forbid(unsafe_code)]

use std::sync::Arc;

use claude_agent_acp_rs::agent::{serve_handle, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::logger::Logger;
use claude_agent_acp_rs::process::{
    die_by_signal_if_requested, resolve_claude_with_env, run_cli_passthrough,
    wait_for_shutdown_signal,
};
use claude_agent_acp_rs::settings;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `--cli`'s signal-death re-exec (`die_by_signal`): raise the signal before
    // any handler exists so the process is terminated by it.
    die_by_signal_if_requested();

    let raw_args: Vec<String> = std::env::args().collect();

    // Check for supervisor re-exec entry (--reaper <parent_pid> <target_pgid>, F41)
    // or probe check (--reaper --probe, Finding 6).
    // Matched ONLY as argv[1] (finding 8) and executed synchronously without starting
    // a multi-threaded tokio runtime (finding 5).
    if raw_args.get(1).map(String::as_str) == Some("--reaper") {
        if raw_args.get(2).map(String::as_str) == Some("--probe") {
            println!("claude-agent-acp-rs-reaper-v1");
            return Ok(());
        }
        if raw_args.len() >= 4 {
            if let (Ok(parent_pid), Ok(target_pgid)) =
                (raw_args[2].parse::<u32>(), raw_args[3].parse::<u32>())
            {
                claude_agent_acp_rs::process::run_reaper(parent_pid, target_pgid);
                return Ok(());
            }
        }
        std::process::exit(1);
    }

    // Hard recursion guard (Turn 1, Group G7a-fix6): if CLAUDE_ACP_REAPER_GUARD is set,
    // this process was invoked under a reaper/probe context. It must NEVER start an adapter session
    // or spawn any further reaper or probe.
    if std::env::var_os(claude_agent_acp_rs::process::REAPER_GUARD_ENV).is_some() {
        eprintln!("claude-agent-acp-rs: error: CLAUDE_ACP_REAPER_GUARD is set but process was not invoked with --reaper");
        std::process::exit(1);
    }

    if let Ok(exe) = std::env::current_exe() {
        claude_agent_acp_rs::process::set_reaper_binary(exe);
    }

    run_tokio_main(raw_args.into_iter().skip(1).collect())
}

// Runtime decision (F43 / CONC-1, porting/parity/CONCURRENCY.md):
// Explicitly configured as `multi_thread`. Benchmarks measure >2x chunk throughput,
// 2.7x lower cancel latency, and 5.5x lower steering latency compared to `current_thread`.
#[tokio::main(flavor = "multi_thread")]
async fn run_tokio_main(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut env = EnvSnapshot::from_process();

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

    // O-03 / FA3-10: the managed-policy tier's env is applied before any SDK
    // call, and — like Node's `await applyManagedPolicyEnv()` mutating
    // `process.env` (`index.ts:51`) — it is folded into the adapter's own
    // snapshot, not only the child's spawn env. Every later reader
    // (`providers/list`, `ANTHROPIC_MODEL`, `CLAUDE_CONFIG_DIR`, the v2
    // opt-in, executable resolution) sees the policy values.
    let policy = settings::apply_managed_policy_env(&Logger::from_env(&env));
    if !policy.is_empty() {
        env = env.with_vars(policy.clone());
    }

    // `CLAUDE_AGENT_LOGS` -> `<dir>/agent.log` (`index.ts:73-84`), read after
    // the policy mutation like Node.
    let logger = Logger::from_env(&env);
    logger.log("Claude ACP started");

    let mut opts = ServeOptions::from_snapshot(env)
        .with_logger(logger.enabled().then(|| Arc::new(logger.clone())));

    // The spawn env keeps the policy at the LOWEST priority: an explicit
    // `extra_env` (provider env) overrides it, exactly like Node's
    // `{...process.env, ...providerEnv}`.
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
