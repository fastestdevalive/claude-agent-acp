//! Integration tests for `fake-claude` (items 2.T2, 2.T3).

use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use fake_claude::{replay, Transcript};

fn mk_transcript(text: &str) -> Transcript {
    Transcript::parse(text).expect("valid transcript")
}

/// Run `replay` with the given stdin and capture (stdout, result).
fn run_replay(
    transcript: &Transcript,
    stdin: &str,
) -> (String, Result<usize, fake_claude::ReplayError>) {
    let mut out = Vec::new();
    let argv = vec!["fake-claude".to_string(), "--session-id=xyz".to_string()];
    let result = replay(
        transcript,
        &argv,
        None,
        None,
        Cursor::new(stdin.as_bytes().to_vec()),
        &mut out,
    );
    (String::from_utf8(out).unwrap(), result)
}

/// 2.T2 — an unexpected stdin frame fails (non-zero) with the frame in stderr.
#[test]
fn unexpected_stdin_frame_fails_with_frame_in_stderr() {
    let transcript = mk_transcript(
        r#"{"expect":{"type":"control_request","subtype":"initialize"},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ"}}]}"#,
    );
    // The transcript expects a control_request but receives a `user` frame.
    let (_out, result) = run_replay(&transcript, "{\"type\":\"user\",\"uuid\":\"abc\"}\n");

    let err = result.expect_err("a mismatched frame must fail");
    let message = err.to_string();
    assert!(
        message.contains("\"type\":\"user\""),
        "the unexpected frame must appear in stderr, got: {message}"
    );
}

/// The binary itself must exit non-zero on a mismatch, with the frame on stderr.
#[test]
fn binary_exits_nonzero_on_mismatch() {
    let dir = std::env::temp_dir().join(format!("fake-claude-t2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("mismatch.transcript.jsonl");
    std::fs::write(
        &script,
        r#"{"expect":{"type":"user"},"emit":[{"type":"result","subtype":"success"}]}"#,
    )
    .unwrap();
    let bin = fake_claude_binary();
    let output = Command::new(&bin)
        .env("FAKE_CLAUDE_SCRIPT", &script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"{\"type\":\"control_request\",\"request_id\":\"123\"}\n")
                .unwrap();
            child.wait_with_output()
        })
        .unwrap();
    assert!(!output.status.success(), "mismatch must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("control_request"),
        "stderr must name the unexpected frame, got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 2.T3 — `$MATCH.request_id` and `$MATCH.uuid` are substituted from the matched
/// stdin frame.
#[test]
fn match_substitution_from_matched_frame() {
    let transcript = mk_transcript(
        r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ"}}]}"#,
    );
    let (out, result) = run_replay(
        &transcript,
        "{\"type\":\"control_request\",\"request_id\":\"REQID-123\",\"request\":{\"subtype\":\"initialize\"}}\n",
    );
    result.expect("initialize step must match");
    let out: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        out["response"]["request_id"], "REQID-123",
        "$REQ must equal the matched frame's request_id"
    );

    let transcript = mk_transcript(
        r#"{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true}]}"#,
    );
    let (out, result) = run_replay(
        &transcript,
        "{\"type\":\"user\",\"uuid\":\"11111111-1111-1111-1111-111111111111\",\"message\":{}}\n",
    );
    result.expect("user step must match");
    let out: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        out["uuid"], "11111111-1111-1111-1111-111111111111",
        "$MATCH.uuid must echo the matched frame's uuid"
    );
}

/// Both substitutions may appear in the same emit frame and nest.
#[test]
fn nested_substitution_across_fields() {
    let transcript = mk_transcript(
        r#"{"expect":{"type":"control_request"},"emit":[{"type":"x","req":"$REQ","u":"$MATCH.uuid"}]}"#,
    );
    let (out, result) = run_replay(
        &transcript,
        "{\"type\":\"control_request\",\"request_id\":\"A\",\"uuid\":\"B\"}\n",
    );
    result.expect("must match");
    let out: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(out["req"], "A");
    assert_eq!(out["u"], "B");
}

/// 2.1 — the transcript records argv/env to `FAKE_CLAUDE_ARGV_OUT`.
#[test]
fn records_argv_to_arg_out() {
    let dir = std::env::temp_dir().join(format!("fake-claude-argv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("a.transcript.jsonl");
    std::fs::write(
        &script,
        r#"{"expect":{"type":"user"},"emit":[{"type":"result","subtype":"success"}]}"#,
    )
    .unwrap();
    let arg_out = dir.join("argv.json");
    let bin = fake_claude_binary();
    let output = Command::new(&bin)
        .args(["--replay-user-messages", "--session-id=abc"])
        .env("FAKE_CLAUDE_SCRIPT", &script)
        .env("FAKE_CLAUDE_ARGV_OUT", &arg_out)
        .env("FAKE_CLAUDE_INIT_OUT", dir.join("init.json"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"{\"type\":\"user\",\"uuid\":\"u1\"}\n")
                .unwrap();
            child.wait_with_output()
        })
        .unwrap();
    assert!(output.status.success());
    let argv: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&arg_out).unwrap()).unwrap();
    assert_eq!(argv["argv"][0], "--replay-user-messages");
    assert_eq!(argv["argv"][1], "--session-id=abc");
    // Only the allow-listed env vars may be dumped; the rest is discarded.
    let env = argv["env"].as_object().unwrap();
    for key in env.keys() {
        assert!(
            key == "CLAUDE_CODE_ENTRYPOINT" || key == "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS",
            "env must only contain allow-listed vars, got: {key}"
        );
    }
    assert!(
        argv["node_options_present"].is_boolean(),
        "node_options_present must be a boolean"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The binary records the `initialize` control_request to `FAKE_CLAUDE_INIT_OUT`.
#[test]
fn records_initialize_to_init_out() {
    let dir = std::env::temp_dir().join(format!("fake-claude-init-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("init.transcript.jsonl");
    std::fs::write(
        &script,
        r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ"}}]}"#,
    )
    .unwrap();
    let init_out = dir.join("initialize.json");
    let bin = fake_claude_binary();
    let output = Command::new(&bin)
        .env("FAKE_CLAUDE_SCRIPT", &script)
        .env("FAKE_CLAUDE_INIT_OUT", &init_out)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(
                    br#"{"type":"control_request","request_id":"R1","request":{"subtype":"initialize","systemPrompt":"x"}}"#,
                )
                .unwrap();
            child.wait_with_output()
        })
        .unwrap();
    assert!(output.status.success());
    let init: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&init_out).unwrap()).unwrap();
    assert_eq!(init["request"]["subtype"], "initialize");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Path to the compiled `fake-claude` binary, building it on demand.
fn fake_claude_binary() -> std::path::PathBuf {
    let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/fake-claude");
    if !bin.exists() {
        let status = Command::new("cargo")
            .args(["build", "--bin", "fake-claude", "--manifest-path"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml"))
            .status()
            .expect("failed to spawn cargo build for fake-claude");
        assert!(status.success(), "cargo build --bin fake-claude failed");
    }
    bin
}
