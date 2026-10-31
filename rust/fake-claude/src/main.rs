//! `fake-claude` binary: replays a recorded stream-json transcript (D13, 2.1).
//!
//! Env:
//! - `FAKE_CLAUDE_SCRIPT` — path to the JSONL transcript (required).
//! - `FAKE_CLAUDE_ARGV_OUT` — optional path; the received argv/env is written
//!   here as JSON.
//! - `FAKE_CLAUDE_INIT_OUT` — optional path; the first `initialize`
//!   control_request received on stdin is written here as JSON.
//! - `FAKE_CLAUDE_PID_OUT` — optional path; this process's pid is written here
//!   as text at startup (phase 12 orphan tests).

use std::process::ExitCode;

use fake_claude::{load_transcript, replay, run_auth};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // `claude auth ...` mode: never touches the argv record or the pid file.
    if argv.first().map(String::as_str) == Some("auth") {
        return auth_main(&argv);
    }
    let Some(script) = std::env::var("FAKE_CLAUDE_SCRIPT").ok() else {
        eprintln!("fake-claude: FAKE_CLAUDE_SCRIPT is not set");
        return ExitCode::FAILURE;
    };
    let argv_out = std::env::var("FAKE_CLAUDE_ARGV_OUT").ok();
    let init_out = std::env::var("FAKE_CLAUDE_INIT_OUT").ok();
    let pid_out = std::env::var("FAKE_CLAUDE_PID_OUT").ok();

    // Publish this child's pid before replay starts so a phase-12 orphan test
    // can assert it is gone (ESRCH) after the host tears down (INV-6).
    if let Some(pid_out) = pid_out {
        let _ = std::fs::write(pid_out, std::process::id().to_string());
    }

    let path = std::path::PathBuf::from(&script);
    let transcript = match load_transcript(&path) {
        Ok(t) => t,
        Err(error) => {
            eprintln!("fake-claude: failed to load transcript `{script}`: {error}");
            return ExitCode::FAILURE;
        }
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    let result = replay(
        &transcript,
        &argv,
        argv_out.as_deref().map(std::path::Path::new),
        init_out.as_deref().map(std::path::Path::new),
        stdin.lock(),
        stdout.lock(),
    );

    match result {
        Ok(steps) => {
            if steps == 0 {
                eprintln!("fake-claude: transcript had no steps");
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("fake-claude: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Serve a `claude auth ...` invocation.
fn auth_main(argv: &[String]) -> ExitCode {
    let transcript = std::env::var("FAKE_CLAUDE_SCRIPT")
        .ok()
        .and_then(|path| load_transcript(std::path::Path::new(&path)).ok());
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from);
    let outcome = run_auth(argv, transcript.as_ref(), config_dir.as_deref());
    print!("{}", outcome.stdout);
    if outcome.code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
