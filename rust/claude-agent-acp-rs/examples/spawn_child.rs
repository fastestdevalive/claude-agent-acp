//! Helper binary for 3.T8 (INV-28): spawns a child via the crate, writes the
//! child's pid to a file, then holds the child's stdin open forever.
//!
//! The test SIGKILLs this process. On host death the child's stdin pipe closes,
//! the child sees EOF and exits (INV-28) — so the child is gone shortly after
//! the host, with no group-kill needed from the dead host.
//!
//! Usage: `spawn_child <pid-out-file> <child-binary> [child-args...]`

use claude_agent_acp_rs::process::{spawn_raw, SpawnOptions};

#[tokio::main]
async fn main() {
    if std::env::args().any(|arg| arg == "--reaper")
        || std::env::var_os("CLAUDE_ACP_REAPER_GUARD").is_some()
    {
        eprintln!("examples/spawn_child: error: cannot be used as a reaper binary");
        std::process::exit(1);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: spawn_child <pid-out-file> <child-binary> [child-args...]");
        std::process::exit(2);
    }
    let pid_out = std::path::PathBuf::from(&args[0]);
    let binary = std::path::PathBuf::from(&args[1]);
    let child_argv = args[2..].to_vec();

    let opts = SpawnOptions {
        claude_path: Some(binary.clone()),
        default_cwd: None,
        env_snapshot: claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_process(),
        ..Default::default()
    };
    let process = match spawn_raw(binary, child_argv, &opts).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("spawn_child: spawn failed: {e}");
            std::process::exit(1);
        }
    };
    let Some(pid) = process.pid() else {
        eprintln!("spawn_child: no pid");
        std::process::exit(1);
    };
    std::fs::write(&pid_out, pid.to_string()).expect("write child pid");
    #[cfg(unix)]
    if let Some(reaper_pid) = process.reaper_pid() {
        let reaper_out = pid_out.with_file_name("reaper.pid");
        let _ = std::fs::write(&reaper_out, reaper_pid.to_string());
    }

    // Hold the child's stdin open. On SIGKILL of this process the pipe
    // closes and the child sees EOF.
    // Also guard with a watchdog timeout (30 seconds) so test crashes or aborted
    // gate runs never leave indefinite orphaned processes (Finding 16 / G7a-fix6).
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(30) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
