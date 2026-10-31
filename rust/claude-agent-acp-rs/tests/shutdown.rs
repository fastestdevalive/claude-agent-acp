//! Phase-12 shutdown / in-process tests (12.T2, 12.T4, 12.T5).
//!
//! These drive the REAL `claude-agent-acp-rs` binary over stdio against
//! `fake-claude`, asserting that stdin EOF (12.T2) and SIGTERM (12.T5) tear the
//! session down with no orphan `claude` child and exit 0 within a bounded
//! deadline (INV-6, 12.2). 12.T4 runs the `in_process` example.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use common::{fake_claude_binary, repo_root, rust_agent_binary, wait_for_file, wait_pid_gone};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// A fake-claude transcript that holds a turn in flight: initialize + a user
/// step that echoes and emits an assistant message but never a `result`, so the
/// prompt stays active until the host shuts down.
const MID_TURN_TRANSCRIPT: &str = r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"commands":[]}}}]}
{"expect":{"type":"user"},"emit":[{"type":"user","uuid":"$MATCH.uuid","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"hello"}]}},{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hi"}]}}]}
"#;

/// A per-test temp directory.
fn tmp_dir(name: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-shutdown-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

/// Read `n` whole JSON-RPC response/notification lines from `reader`.
async fn read_frames(
    reader: &mut tokio::io::BufReader<tokio::process::ChildStdout>,
    n: usize,
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut line = String::new();
    for _ in 0..n {
        line.clear();
        let res = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await;
        let bytes = res
            .expect("read frame within timeout")
            .expect("stream open");
        assert!(bytes > 0, "stdout closed early");
        let v: Value = serde_json::from_str(line.trim()).expect("frame is JSON");
        out.push(v);
    }
    out
}

/// 12.T2 (INV-6) — stdin EOF mid-turn leaves no orphan `claude` and the binary
/// exits 0.
#[cfg(unix)]
#[tokio::test]
async fn inv_06_eof_no_orphan() {
    let tmp = tmp_dir("eof");
    let transcript = tmp.join("mid-turn.transcript.jsonl");
    std::fs::write(&transcript, MID_TURN_TRANSCRIPT).expect("write transcript");
    let pid_out = tmp.join("fake.pid");

    let mut child = tokio::process::Command::new(rust_agent_binary())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLAUDE_CODE_EXECUTABLE", fake_claude_binary())
        .env("FAKE_CLAUDE_SCRIPT", &transcript)
        .env("FAKE_CLAUDE_PID_OUT", &pid_out)
        .spawn()
        .expect("spawn the agent binary");
    let mut stdin = child.stdin.take().expect("take stdin");
    let stdout = child.stdout.take().expect("take stdout");
    let mut reader = tokio::io::BufReader::new(stdout);

    // initialize
    let mut line =
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#
            .to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write initialize");
    read_frames(&mut reader, 1).await;

    // session/new -> parse the sessionId
    let mut line = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write session/new");
    let frames = read_frames(&mut reader, 2).await;
    let session_id = frames
        .iter()
        .find_map(|f| f.get("id").and_then(Value::as_i64).filter(|&i| i == 2))
        .and_then(|_| {
            frames
                .iter()
                .find(|f| f.get("id") == Some(&serde_json::json!(2)))
        })
        .and_then(|f| f.get("result").and_then(|r| r.get("sessionId")))
        .and_then(Value::as_str)
        .expect("sessionId from session/new")
        .to_string();

    // Wait for fake-claude to be spawned and record its pid.
    wait_for_file(&pid_out, Duration::from_secs(10)).expect("fake-claude pid recorded");
    let child_pid: u32 = std::fs::read_to_string(&pid_out)
        .expect("read fake pid")
        .trim()
        .parse()
        .expect("parse fake pid");
    assert!(child_pid > 0, "a real fake-claude pid was recorded");

    // session/prompt -> turn in flight (mid-turn).
    let prompt = serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session_id, "prompt": [{"type":"text","text":"hello"}] }
    });
    let mut line = serde_json::to_string(&prompt).expect("serialize prompt");
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write prompt");
    // Let fake-claude emit the echo + assistant (no result): the turn is now in
    // flight.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // stdin EOF mid-turn.
    drop(stdin);

    // The binary exits 0 within a bounded deadline.
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("binary exits within the deadline")
        .expect("wait for the binary");
    assert_eq!(status.code(), Some(0), "stdin EOF must exit 0");

    // No orphan claude (INV-6): the child pid is gone.
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(5)),
        "child pid {child_pid} must be gone (ESRCH) after stdin EOF"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 12.T4 — `cargo run --example in_process` exits 0.
#[tokio::test]
async fn in_process_example_exits_zero() {
    let tmp = tmp_dir("inproc");
    let transcript = tmp.join("text-only.transcript.jsonl");
    let raw =
        std::fs::read_to_string(repo_root().join("porting/corpus/text-only.transcript.jsonl"))
            .expect("read text-only transcript");
    let mut lines = raw.lines();
    let init_line = lines.next().expect("init line");
    let set_model_line = r#"{"expect":{"type":"control_request","request":{"subtype":"set_model"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ"}}]}"#;
    let mut replay = String::new();
    replay.push_str(init_line);
    replay.push('\n');
    replay.push_str(set_model_line);
    replay.push('\n');
    for line in lines {
        replay.push_str(line);
        replay.push('\n');
    }
    std::fs::write(&transcript, replay).expect("write transcript");

    let root = repo_root();
    let status = std::process::Command::new("cargo")
        .args([
            "run",
            "--manifest-path",
            "rust/Cargo.toml",
            "--example",
            "in_process",
        ])
        .current_dir(&root)
        .env("CLAUDE_CODE_EXECUTABLE", fake_claude_binary())
        .env("FAKE_CLAUDE_SCRIPT", &transcript)
        .env("ANTHROPIC_MODEL", "sonnet")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn cargo run --example in_process");
    assert!(
        status.success(),
        "cargo run --example in_process must exit 0"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 12.T5 — SIGTERM to the binary mid-turn leaves no orphan `claude` and it
/// exits 0 within the bounded deadline.
#[cfg(unix)]
#[tokio::test]
async fn inv_sigterm_mid_turn_no_orphan() {
    let tmp = tmp_dir("sigterm");
    let transcript = tmp.join("mid-turn.transcript.jsonl");
    std::fs::write(&transcript, MID_TURN_TRANSCRIPT).expect("write transcript");
    let pid_out = tmp.join("fake.pid");

    let mut child = tokio::process::Command::new(rust_agent_binary())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLAUDE_CODE_EXECUTABLE", fake_claude_binary())
        .env("FAKE_CLAUDE_SCRIPT", &transcript)
        .env("FAKE_CLAUDE_PID_OUT", &pid_out)
        .spawn()
        .expect("spawn the agent binary");
    let mut stdin = child.stdin.take().expect("take stdin");
    let stdout = child.stdout.take().expect("take stdout");
    let mut reader = tokio::io::BufReader::new(stdout);

    let mut line =
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#
            .to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write initialize");
    read_frames(&mut reader, 1).await;

    let mut line = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write session/new");
    let frames = read_frames(&mut reader, 2).await;
    let session_id = frames
        .iter()
        .find(|f| f.get("id") == Some(&serde_json::json!(2)))
        .and_then(|f| f.get("result").and_then(|r| r.get("sessionId")))
        .and_then(Value::as_str)
        .expect("sessionId from session/new")
        .to_string();

    wait_for_file(&pid_out, Duration::from_secs(10)).expect("fake-claude pid recorded");
    let child_pid: u32 = std::fs::read_to_string(&pid_out)
        .expect("read fake pid")
        .trim()
        .parse()
        .expect("parse fake pid");

    let prompt = serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session_id, "prompt": [{"type":"text","text":"hello"}] }
    });
    let mut line = serde_json::to_string(&prompt).expect("serialize prompt");
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write prompt");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // SIGTERM the binary mid-turn.
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let bin_pid = child.id().expect("binary pid");
    kill(Pid::from_raw(bin_pid as i32), Signal::SIGTERM).expect("SIGTERM the binary");

    // It exits 0 within a bounded deadline.
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("binary exits within the deadline")
        .expect("wait for the binary");
    assert_eq!(status.code(), Some(0), "SIGTERM must exit 0");

    // No orphan claude (INV-6).
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(5)),
        "child pid {child_pid} must be gone (ESRCH) after SIGTERM"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// V-01 — SIGINT mid-turn disposes the sessions, leaves no orphan `claude` and
/// exits 0 (`index.ts:98-104` installs the same `shutdown` handler for SIGINT
/// and SIGTERM).
#[cfg(unix)]
#[tokio::test]
async fn inv_sigint_mid_turn_no_orphan() {
    let tmp = tmp_dir("sigint");
    let transcript = tmp.join("mid-turn.transcript.jsonl");
    std::fs::write(&transcript, MID_TURN_TRANSCRIPT).expect("write transcript");
    let pid_out = tmp.join("fake.pid");

    let mut child = tokio::process::Command::new(rust_agent_binary())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLAUDE_CODE_EXECUTABLE", fake_claude_binary())
        .env("FAKE_CLAUDE_SCRIPT", &transcript)
        .env("FAKE_CLAUDE_PID_OUT", &pid_out)
        .spawn()
        .expect("spawn the agent binary");
    let mut stdin = child.stdin.take().expect("take stdin");
    let stdout = child.stdout.take().expect("take stdout");
    let mut reader = tokio::io::BufReader::new(stdout);

    let mut line =
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#
            .to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write initialize");
    read_frames(&mut reader, 1).await;

    let mut line = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write session/new");
    let frames = read_frames(&mut reader, 2).await;
    let session_id = frames
        .iter()
        .find(|f| f.get("id") == Some(&serde_json::json!(2)))
        .and_then(|f| f.get("result").and_then(|r| r.get("sessionId")))
        .and_then(Value::as_str)
        .expect("sessionId from session/new")
        .to_string();

    wait_for_file(&pid_out, Duration::from_secs(10)).expect("fake-claude pid recorded");
    let child_pid: u32 = std::fs::read_to_string(&pid_out)
        .expect("read fake pid")
        .trim()
        .parse()
        .expect("parse fake pid");

    let prompt = serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session_id, "prompt": [{"type":"text","text":"hello"}] }
    });
    let mut line = serde_json::to_string(&prompt).expect("serialize prompt");
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("write prompt");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // SIGINT the binary mid-turn.
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let bin_pid = child.id().expect("binary pid");
    kill(Pid::from_raw(bin_pid as i32), Signal::SIGINT).expect("SIGINT the binary");

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("binary exits within the deadline")
        .expect("wait for the binary");
    assert_eq!(status.code(), Some(0), "SIGINT must exit 0");

    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(5)),
        "child pid {child_pid} must be gone (ESRCH) after SIGINT"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Write an executable fake CLI script into `dir` (hermetic, `#[cfg(unix)]`).
#[cfg(unix)]
fn write_fake_cli(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fake CLI script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make fake CLI executable");
    path
}

/// O-02 — `--cli` forwards every other argument to the wrapped native CLI
/// (`index.ts:13-42`) and mirrors its exit code.
#[cfg(unix)]
#[tokio::test]
async fn inv_cli_passthrough_forwards_args_and_exit_code() {
    let tmp = tmp_dir("cli-args");
    let args_out = tmp.join("args.txt");
    let fake = write_fake_cli(
        &tmp,
        "fake-claude-cli",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_CLI_ARGS_OUT\"\nexit 7\n",
    );

    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_claude-agent-acp-rs"))
        .args(["--cli", "alpha", "--beta", "gamma"])
        .env("CLAUDE_CODE_EXECUTABLE", &fake)
        .env("FAKE_CLI_ARGS_OUT", &args_out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .expect("run --cli passthrough");

    assert_eq!(
        status.code(),
        Some(7),
        "the wrapped CLI's exit code is mirrored"
    );
    let forwarded = std::fs::read_to_string(&args_out).expect("args recorded");
    let args: Vec<&str> = forwarded.lines().collect();
    assert_eq!(
        args,
        vec!["alpha", "--beta", "gamma"],
        "`--cli` itself is stripped, the rest forwarded verbatim"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// O-02 — `--cli` forwards SIGTERM to the wrapped CLI (`index.ts:18-26`); the
/// child's clean exit is mirrored.
#[cfg(unix)]
#[tokio::test]
async fn inv_cli_passthrough_relays_sigterm() {
    let tmp = tmp_dir("cli-term");
    let ready_out = tmp.join("ready.txt");
    let term_out = tmp.join("term.txt");
    let fake = write_fake_cli(
        &tmp,
        "fake-claude-cli",
        "#!/bin/sh\nprintf 'ready\\n' > \"$FAKE_CLI_READY_OUT\"\n\
         trap 'printf \"term\\n\" > \"$FAKE_CLI_TERM_OUT\"; exit 0' TERM\n\
         while :; do sleep 0.05; done\n",
    );

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_claude-agent-acp-rs"))
        .args(["--cli"])
        .env("CLAUDE_CODE_EXECUTABLE", &fake)
        .env("FAKE_CLI_READY_OUT", &ready_out)
        .env("FAKE_CLI_TERM_OUT", &term_out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn --cli passthrough");

    wait_for_file(&ready_out, Duration::from_secs(10)).expect("fake CLI started");

    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let bin_pid = child.id().expect("binary pid");
    kill(Pid::from_raw(bin_pid as i32), Signal::SIGTERM).expect("SIGTERM the adapter");

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("adapter exits within the deadline")
        .expect("wait for the adapter");
    assert_eq!(status.code(), Some(0), "the child's clean exit is mirrored");
    assert!(
        term_out.exists(),
        "SIGTERM must have been forwarded to the wrapped CLI"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
