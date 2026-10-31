//! `fake-claude` binary: replays a recorded stream-json transcript (D13, 2.1).
//!
//! Env (only the `FAKE_CLAUDE_*` names in [`fake_claude::FAKE_CLAUDE_ENV_ALLOWLIST`]
//! are ever read; nothing prints a variable VALUE):
//! - `FAKE_CLAUDE_SCRIPT` — path to the JSONL transcript (required).
//! - `FAKE_CLAUDE_ARGV_OUT` — optional path; the received argv/env is written
//!   here as JSON.
//! - `FAKE_CLAUDE_INIT_OUT` — optional path; the first `initialize`
//!   control_request received on stdin is written here as JSON.
//! - `FAKE_CLAUDE_PID_OUT` — optional path; this process's pid is written here
//!   as text at startup (phase 12 orphan tests).
//! - `FAKE_CLAUDE_LOG_FRAMES` — when set, stdin/drain frames are logged to stderr.
//! - `FAKE_CLAUDE_CURSOR` — optional path to a spawn cursor; a multi-segment
//!   transcript (lines `{"segment": N}`) replays segment 0 on the first spawn,
//!   segment 1 on the second, ... (F1 multi-spawn).

use std::process::ExitCode;

use fake_claude::{fake_claude_env, load_transcript, next_segment_index, replay_segment, run_auth};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // `claude auth ...` mode: never touches the argv record or the pid file.
    if argv.first().map(String::as_str) == Some("auth") {
        return auth_main(&argv);
    }
    let Some(script) =
        fake_claude_env("FAKE_CLAUDE_SCRIPT").map(|value| value.to_string_lossy().into_owned())
    else {
        eprintln!("fake-claude: FAKE_CLAUDE_SCRIPT is not set");
        return ExitCode::FAILURE;
    };
    let argv_out = fake_claude_env("FAKE_CLAUDE_ARGV_OUT").map(std::path::PathBuf::from);
    let init_out = fake_claude_env("FAKE_CLAUDE_INIT_OUT").map(std::path::PathBuf::from);
    let pid_out = fake_claude_env("FAKE_CLAUDE_PID_OUT").map(std::path::PathBuf::from);

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

    let segment = next_segment_index(&transcript);
    let result = replay_segment(
        &transcript,
        segment,
        &argv,
        argv_out.as_deref(),
        init_out.as_deref(),
        stdin.lock(),
        stdout.lock(),
    );

    match result {
        Ok(outcome) => {
            if outcome.steps == 0 {
                eprintln!("fake-claude: transcript had no steps");
                ExitCode::FAILURE
            } else {
                match outcome.exit_code {
                    Some(code) => ExitCode::from(code as u8),
                    None => ExitCode::SUCCESS,
                }
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
    let transcript = fake_claude_env("FAKE_CLAUDE_SCRIPT")
        .map(std::path::PathBuf::from)
        .and_then(|path| load_transcript(&path).ok());
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from);
    let outcome = run_auth(argv, transcript.as_ref(), config_dir.as_deref());
    print!("{}", outcome.stdout);
    if outcome.code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
