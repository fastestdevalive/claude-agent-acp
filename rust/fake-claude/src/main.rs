//! `fake-claude` binary: replays a recorded stream-json transcript (D13, 2.1).
//!
//! Env:
//! - `FAKE_CLAUDE_SCRIPT` — path to the JSONL transcript (required).
//! - `FAKE_CLAUDE_ARGV_OUT` — optional path; the received argv/env is written
//!   here as JSON.
//! - `FAKE_CLAUDE_INIT_OUT` — optional path; the first `initialize`
//!   control_request received on stdin is written here as JSON.

use std::process::ExitCode;

use fake_claude::{load_transcript, replay};

fn main() -> ExitCode {
    let Some(script) = std::env::var("FAKE_CLAUDE_SCRIPT").ok() else {
        eprintln!("fake-claude: FAKE_CLAUDE_SCRIPT is not set");
        return ExitCode::FAILURE;
    };
    let argv_out = std::env::var("FAKE_CLAUDE_ARGV_OUT").ok();
    let init_out = std::env::var("FAKE_CLAUDE_INIT_OUT").ok();

    let path = std::path::PathBuf::from(&script);
    let transcript = match load_transcript(&path) {
        Ok(t) => t,
        Err(error) => {
            eprintln!("fake-claude: failed to load transcript `{script}`: {error}");
            return ExitCode::FAILURE;
        }
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();
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
