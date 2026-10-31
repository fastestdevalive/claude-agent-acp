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
    let output = fake_claude_cmd(&bin)
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
    let output = fake_claude_cmd(&bin)
        .args(["--replay-user-messages", "--session-id=abc"])
        .env("FAKE_CLAUDE_SCRIPT", &script)
        .env("FAKE_CLAUDE_ARGV_OUT", &arg_out)
        .env("FAKE_CLAUDE_INIT_OUT", dir.join("init.json"))
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")
        .env("UNRECORDED_SECRET", "discard-me")
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

/// rust-parity-fixes Phase 1 (item 1.1) — `CLAUDE_CODE_ARTIFACT` joins the env
/// allow-list: when the adapter forwards it (a client option env key), the argv
/// record persists it; non-allow-listed vars stay discarded (none leaks).
#[test]
fn records_claude_code_artifact_env() {
    let dir = std::env::temp_dir().join(format!("fake-claude-artifact-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("b.transcript.jsonl");
    std::fs::write(
        &script,
        r#"{"expect":{"type":"user"},"emit":[{"type":"result","subtype":"success"}]}"#,
    )
    .unwrap();
    let arg_out = dir.join("argv.json");
    let bin = fake_claude_binary();
    let output = fake_claude_cmd(&bin)
        .args(["--session-id=abc"])
        .env("FAKE_CLAUDE_SCRIPT", &script)
        .env("FAKE_CLAUDE_ARGV_OUT", &arg_out)
        .env("CLAUDE_CODE_ARTIFACT", "1")
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
    assert_eq!(argv["env"]["CLAUDE_CODE_ARTIFACT"], "1");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 2.6/2.14 — the 0.94+ auth probe (`argv ["auth", ...]`) never clobbers the
/// session argv record: `record_argv_env` keeps exactly one argv record (the
/// session spawn) whether the probe runs before or after the session.
#[test]
fn auth_probe_does_not_clobber_session_argv() {
    let dir = std::env::temp_dir().join(format!("fake-claude-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let session_argv = vec![
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--resume=sess-1234-abcd".to_string(),
    ];
    let probe_argv = vec![
        "auth".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ];

    // Probe first, then session: the final record is the session.
    let out1 = dir.join("argv-a.json");
    fake_claude::record_argv_env(&out1, &probe_argv).unwrap();
    fake_claude::record_argv_env(&out1, &session_argv).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out1).unwrap()).unwrap();
    assert_eq!(
        v["argv"][1], "stream-json",
        "the session argv must be the record even when the probe ran first"
    );
    assert!(
        v["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--resume=sess-1234-abcd"),
        "probe-then-session must record the session argv (--resume=...)"
    );

    // Session first, then probe: the probe must NOT overwrite the session record.
    let out2 = dir.join("argv-b.json");
    fake_claude::record_argv_env(&out2, &session_argv).unwrap();
    fake_claude::record_argv_env(&out2, &probe_argv).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out2).unwrap()).unwrap();
    assert_eq!(
        v["argv"][0], "--output-format",
        "a later auth probe must not clobber the session argv record"
    );

    // A probe alone records nothing.
    let out3 = dir.join("argv-c.json");
    fake_claude::record_argv_env(&out3, &probe_argv).unwrap();
    assert!(!out3.exists(), "the auth probe alone must record nothing");

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
    let output = fake_claude_cmd(&bin)
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

/// 1.15 — a `control_request` whose subtype is `get_context_usage` (sent by the
/// 0.94+ adapter after session creation) that does NOT match the current step's
/// `expect` is auto-acked (Decision 5) WITHOUT consuming a transcript step; the
/// replay then continues and consumes the real expected step.
#[test]
fn auto_acks_get_context_usage_without_consuming_a_step() {
    let transcript = mk_transcript(
        r#"{"expect":{"type":"user"},"emit":[{"type":"result","subtype":"success"}]}"#,
    );
    let mut out = Vec::new();
    let steps = replay(
        &transcript,
        &[],
        None,
        None,
        Cursor::new(
            b"{\"type\":\"control_request\",\"request_id\":\"r1\",\"request\":{\"subtype\":\"get_context_usage\"}}\n{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{}}\n"
                .to_vec(),
        ),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        steps, 1,
        "auto-acked get_context_usage must not consume a transcript step"
    );
    let stdout = String::from_utf8(out).unwrap();
    let mut lines = stdout.lines();
    let auto_ack: serde_json::Value =
        serde_json::from_str(lines.next().expect("first stdout line is the auto-ack")).unwrap();
    assert_eq!(auto_ack["type"], "control_response");
    assert_eq!(auto_ack["response"]["subtype"], "success");
    assert_eq!(auto_ack["response"]["request_id"], "r1");
    assert_eq!(auto_ack["response"]["response"], serde_json::json!({}));
    // The transcript's own emit follows the auto-ack.
    let emit: serde_json::Value =
        serde_json::from_str(lines.next().expect("second stdout line is the step emit")).unwrap();
    assert_eq!(emit["type"], "result");
    assert_eq!(emit["subtype"], "success");
    assert!(lines.next().is_none(), "no further frames expected");
}

/// 1.15 — when the transcript EXPLICITLY expects the `get_context_usage` as a
/// step, the auto-ack must NOT fire: the frame is consumed normally as the
/// step, its emit is written exactly once, and no extra auto-ack response is
/// produced. Non-vacuous: the step's emit carries content that DISTINGUISHES it
/// from the auto-ack (which always writes `response:{}`), so the assertion
/// fails if the auto-ack path fired instead of the explicit step.
#[test]
fn explicit_expect_wins_over_auto_ack() {
    let transcript = mk_transcript(
        r#"{"expect":{"type":"control_request","request":{"subtype":"get_context_usage"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"marker":"explicit-step"}}}]}"#,
    );
    let mut out = Vec::new();
    let steps = replay(
        &transcript,
        &[],
        None,
        None,
        Cursor::new(
            b"{\"type\":\"control_request\",\"request_id\":\"rid\",\"request\":{\"subtype\":\"get_context_usage\"}}\n"
                .to_vec(),
        ),
        &mut out,
    )
    .unwrap();
    assert_eq!(steps, 1, "the explicit step must be consumed exactly once");
    let stdout = String::from_utf8(out).unwrap();
    let emitted: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("exactly one emitted frame");
    assert_eq!(
        emitted["response"]["request_id"], "rid",
        "$REQ must substitute the matched frame's request_id"
    );
    assert_eq!(
        emitted["response"]["response"]["marker"], "explicit-step",
        "the step's own emit (with its distinguishing content) is written — NOT a bare auto-ack (which would write an empty response object)"
    );
}

/// Path to the `fake-claude` binary built for this test run. Cargo exports
/// `CARGO_BIN_EXE_<name>` for integration tests of a binary crate, so this is
/// always the binary compiled from the current source (a stale
/// `../target/debug/fake-claude` from an earlier build must never be replayed).
fn fake_claude_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-claude"))
}

/// Spawn fake-claude under a scrubbed environment mirroring `capture.sh`'s `env -i`.
fn fake_claude_cmd(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        cmd.env("TMPDIR", tmpdir);
    } else {
        cmd.env("TMPDIR", "/tmp");
    }
    if let Some(lang) = std::env::var_os("LANG") {
        cmd.env("LANG", lang);
    } else {
        cmd.env("LANG", "C");
    }
    for (k, v) in std::env::vars() {
        if k.starts_with("LC_") {
            cmd.env(k, v);
        }
    }
    #[cfg(windows)]
    for key in ["SystemRoot", "SYSTEMROOT", "COMSPEC", "PATHEXT"] {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }
    cmd
}

/// rust-air-gaps 1.1 — `auth status` prints the transcript's special line
/// (exit 0); `auth logout` drops a marker so the next `status` reports
/// `{"loggedIn":false}` with exit 1; no transcript line means exit 1, silent.
#[test]
fn auth_logout_flips_status() {
    let dir = std::env::temp_dir().join(format!("fake-claude-auth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let transcript = mk_transcript(
        "{\"auth_status_stdout\":{\"loggedIn\":true,\"email\":\"a@b.c\"}}\n{\"expect\":{\"type\":\"user\"},\"emit\":[]}\n",
    );
    assert_eq!(transcript.steps.len(), 1, "the auth line is not a step");
    let status = vec![
        "auth".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ];
    let logout = vec!["auth".to_string(), "logout".to_string()];

    let first = fake_claude::run_auth(&status, Some(&transcript), Some(&dir));
    assert_eq!(first.code, 0);
    let value: serde_json::Value = serde_json::from_str(first.stdout.trim()).unwrap();
    assert_eq!(value["email"], "a@b.c");

    let out = fake_claude::run_auth(&logout, Some(&transcript), Some(&dir));
    assert_eq!(out.code, 0);
    let after = fake_claude::run_auth(&status, Some(&transcript), Some(&dir));
    assert_eq!(after.code, 1);
    assert_eq!(after.stdout.trim(), "{\"loggedIn\":false}");

    let bare = mk_transcript("{\"expect\":{},\"emit\":[]}\n");
    let silent = fake_claude::run_auth(&status, Some(&bare), Some(&dir.join("other")));
    assert_eq!((silent.code, silent.stdout.as_str()), (1, ""));
    let _ = std::fs::remove_dir_all(&dir);
}

/// rust-air-gaps 1.1 — `$REQ.1` resolves to the `request_id` of the frame the
/// previous step matched, so a later step can answer a deferred request.
#[test]
fn req_prev_placeholder_answers_deferred_request() {
    let transcript = mk_transcript(concat!(
        r#"{"expect":{"type":"control_request","request":{"subtype":"rewind_files"}},"emit":[]}"#,
        "\n",
        r#"{"expect":{"type":"control_request","request":{"subtype":"interrupt"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ"}},{"type":"control_response","response":{"subtype":"success","request_id":"$REQ.1"}}]}"#,
        "\n"
    ));
    let stdin = concat!(
        r#"{"type":"control_request","request_id":"rid-rewind","request":{"subtype":"rewind_files"}}"#,
        "\n",
        r#"{"type":"control_request","request_id":"rid-int","request":{"subtype":"interrupt"}}"#,
        "\n"
    );
    let (out, result) = run_replay(&transcript, stdin);
    assert_eq!(result.unwrap(), 2);
    let frames: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(frames[0]["response"]["request_id"], "rid-int");
    assert_eq!(frames[1]["response"]["request_id"], "rid-rewind");
}

/// rust-air-gaps 1.1 — `$CWD` expands (inside longer strings too) to the
/// canonical current directory.
#[test]
fn cwd_placeholder_expands() {
    let transcript = mk_transcript(
        r#"{"expect":{"type":"user"},"emit":[{"path":"$CWD/a.txt","bare":"$CWD","n":1}]}"#,
    );
    let (out, result) = run_replay(&transcript, "{\"type\":\"user\"}\n");
    assert_eq!(result.unwrap(), 1);
    let frame: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    assert_eq!(frame["bare"], cwd.as_str());
    assert_eq!(frame["path"], format!("{cwd}/a.txt"));
    assert_eq!(frame["n"], 1);
}

/// F1 — `$REQ.n` resolves the n-th previous matched frame's `request_id`
/// (`$REQ.1` = previous, `$REQ.2` = the one before).
#[test]
fn req_n_placeholder_indexes_backwards() {
    let transcript = mk_transcript(concat!(
        r#"{"expect":{"type":"control_request","request":{"subtype":"a"}},"emit":[{"type":"x","req":"$REQ"}]}"#,
        "\n",
        r#"{"expect":{"type":"control_request","request":{"subtype":"b"}},"emit":[{"type":"y","req":"$REQ"}]}"#,
        "\n",
        r#"{"expect":{"type":"control_request","request":{"subtype":"c"}},"emit":[{"type":"z","prev1":"$REQ.1","prev2":"$REQ.2"}]}"#,
        "\n"
    ));
    let stdin = concat!(
        r#"{"type":"control_request","request_id":"R-A","request":{"subtype":"a"}}"#,
        "\n",
        r#"{"type":"control_request","request_id":"R-B","request":{"subtype":"b"}}"#,
        "\n",
        r#"{"type":"control_request","request_id":"R-C","request":{"subtype":"c"}}"#,
        "\n"
    );
    let (out, result) = run_replay(&transcript, stdin);
    assert_eq!(result.unwrap(), 3);
    let frames: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(frames[2]["prev1"], "R-B");
    assert_eq!(frames[2]["prev2"], "R-A");
}

/// F1 — spawn a fake-claude process with an optional spawn cursor.
fn run_fake_claude_with(script: &Path, cursor: Option<&Path>, stdin: &str) -> (String, bool) {
    use std::io::Write as _;
    use std::process::Stdio;

    let bin = fake_claude_binary();
    let mut cmd = fake_claude_cmd(&bin);
    cmd.env("FAKE_CLAUDE_SCRIPT", script);
    if let Some(cursor) = cursor {
        cmd.env("FAKE_CLAUDE_CURSOR", cursor);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn fake-claude");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for fake-claude");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.success(),
    )
}

/// F1 multi-spawn: a second process after a restart reads the NEXT transcript
/// segment (the spawn cursor advances), while the first reads segment 0.
#[test]
fn multi_spawn_reads_next_segment() {
    let dir = std::env::temp_dir().join(format!("fake-claude-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("multi.transcript.jsonl");
    std::fs::write(
        &script,
        concat!(
            r#"{"expect":{"type":"user","message":{"content":[{"type":"text","text":"first"}]}},"emit":[{"type":"result","subtype":"success","n":1}]}"#,
            "\n",
            r#"{"segment":1}"#,
            "\n",
            r#"{"expect":{"type":"user","message":{"content":[{"type":"text","text":"second"}]}},"emit":[{"type":"result","subtype":"success","n":2}]}"#,
            "\n"
        ),
    )
    .unwrap();
    let cursor = dir.join("spawn.cursor");

    let (out1, ok1) = run_fake_claude_with(
        &script,
        Some(&cursor),
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n",
    );
    assert!(ok1, "first spawn must succeed, got: {out1}");
    let frame1: serde_json::Value = serde_json::from_str(out1.lines().next().unwrap()).unwrap();
    assert_eq!(frame1["n"], 1, "first spawn replays segment 0");

    let (out2, ok2) = run_fake_claude_with(
        &script,
        Some(&cursor),
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n",
    );
    assert!(ok2, "second spawn must succeed, got: {out2}");
    let frame2: serde_json::Value = serde_json::from_str(out2.lines().next().unwrap()).unwrap();
    assert_eq!(frame2["n"], 2, "second spawn replays segment 1");
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap().trim(),
        "2",
        "cursor advanced once per spawn"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// F1 — without `FAKE_CLAUDE_CURSOR` a multi-segment transcript still replays
/// segment 0 on every spawn (existing fixtures unchanged, no repo writes).
#[test]
fn multi_spawn_without_cursor_replays_first_segment() {
    let dir = std::env::temp_dir().join(format!("fake-claude-nocursor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("multi.transcript.jsonl");
    std::fs::write(
        &script,
        concat!(
            r#"{"expect":{"type":"user","message":{"content":[{"type":"text","text":"first"}]}},"emit":[{"type":"result","subtype":"success","n":1}]}"#,
            "\n",
            r#"{"segment":1}"#,
            "\n",
            r#"{"expect":{"type":"user","message":{"content":[{"type":"text","text":"second"}]}},"emit":[{"type":"result","subtype":"success","n":2}]}"#,
            "\n"
        ),
    )
    .unwrap();
    let stdin =
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n";
    for _ in 0..2 {
        let (out, ok) = run_fake_claude_with(&script, None, stdin);
        assert!(ok, "spawn must succeed, got: {out}");
        let frame: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(frame["n"], 1, "segment 0 replays on every spawn");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
