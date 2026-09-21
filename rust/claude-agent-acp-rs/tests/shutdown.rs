//! Phase-12 shutdown / in-process tests (12.T2, 12.T4, 12.T5).
//!
//! These drive the REAL `claude-agent-acp-rs` binary over stdio against
//! `fake-claude`, asserting that stdin EOF (12.T2) and SIGTERM (12.T5) tear the
//! session down with no orphan `claude` child and exit 0 within a bounded
//! deadline (INV-6, 12.2). 12.T4 runs the `in_process` example.

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
    std::fs::write(
        &transcript,
        std::fs::read_to_string(repo_root().join("porting/corpus/text-only.transcript.jsonl"))
            .expect("read text-only transcript"),
    )
    .expect("write transcript");

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
