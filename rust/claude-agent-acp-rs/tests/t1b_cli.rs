//! Turn 1 lane B (F2b, O-01): the binary prints its version for `--version`,
//! `-v` and `-V` (`index.ts:43-45` handles `--version` and `-v`; the Rust
//! binary also accepts `-V`), exits 0 and writes nothing else to stdout.
//! The printed string is the composed `<crate>+upstream.<pin>` (DECISIONS D7).

use std::process::{Command, Stdio};

fn run_flag(args: &[&str]) -> (bool, String, String) {
    let child = Command::new(env!("CARGO_BIN_EXE_claude-agent-acp-rs"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run claude-agent-acp-rs");
    (
        child.status.success(),
        String::from_utf8(child.stdout).expect("utf8 stdout"),
        String::from_utf8(child.stderr).expect("utf8 stderr"),
    )
}

#[test]
fn version_flags_print_the_composed_version_and_exit_zero() {
    let expected = format!("{}\n", claude_agent_acp_rs::VERSION);
    for flag in ["--version", "-v", "-V"] {
        let (ok, stdout, _stderr) = run_flag(&[flag]);
        assert!(ok, "{flag} must exit 0");
        assert_eq!(stdout, expected, "{flag} prints only the version");
    }
}

#[test]
fn version_flag_is_found_anywhere_in_argv() {
    let expected = format!("{}\n", claude_agent_acp_rs::VERSION);
    let (ok, stdout, _stderr) = run_flag(&["--something", "-v"]);
    assert!(ok);
    assert_eq!(stdout, expected);
}

#[test]
fn version_string_carries_the_pinned_upstream_version() {
    assert!(
        claude_agent_acp_rs::VERSION.ends_with(&format!(
            "+upstream.{}",
            claude_agent_acp_rs::UPSTREAM_VERSION
        )),
        "the composed version ends with the mirrored npm pin (D7)"
    );
}
