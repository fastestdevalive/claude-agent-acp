//! Phase-3 process-transport integration tests (3.T1, 3.T5–3.T8).
//!
//! These exercise [`process::spawn`] / [`process::spawn_raw`] against real
//! child processes and the `fake-claude` binary.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use claude_agent_acp_rs::process::{spawn, spawn_raw, SpawnOptions, Timings};
use common::{
    build_example, fake_claude_binary, normalize_argv_session_id, read_json, repo_root,
    wait_for_file, wait_pid_gone,
};

/// A uuid-shaped string for `--session-id` (normalised away by the comparison).
const SESSION_UUID: &str = "11111111-2222-3333-4444-555566667777";

/// 3.T1 — spawn `fake-claude` for the text-only script; its captured argv
/// equals `porting/fixtures/text-only.argv.json` after uuid normalisation, and
/// its env has `CLAUDE_CODE_ENTRYPOINT` and
/// `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` set and `NODE_OPTIONS` absent.
#[tokio::test]
async fn inv_25_text_only_argv_and_env() {
    let root = repo_root();
    let transcript = root.join("porting/corpus/text-only.transcript.jsonl");
    let fixture = root.join("porting/fixtures/text-only.argv.json");
    assert!(transcript.exists(), "text-only transcript exists");
    assert!(fixture.exists(), "text-only argv fixture exists");

    let tmp = std::env::temp_dir().join(format!("acp-argv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let argv_out = tmp.join("argv.json");

    let opts = SpawnOptions {
        claude_path: Some(fake_claude_binary()),
        session_id: Some(SESSION_UUID.to_string()),
        permission_mode: Some("default".to_string()),
        disallowed_tools: vec!["AskUserQuestion".to_string()],
        include_partial_messages: true,
        permission_prompt_tool: true,
        allow_dangerously_skip_permissions: true,
        replay_user_messages: true,
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_ARGV_OUT".to_string(),
                argv_out.to_string_lossy().into_owned(),
            ),
        ],
        ..Default::default()
    };

    let mut process = spawn(&opts).await.expect("spawn fake-claude");

    wait_for_file(&argv_out, Duration::from_secs(10)).expect("fake-claude records its argv");

    let captured = read_json(&argv_out);
    let mut captured_argv: Vec<String> = captured["argv"]
        .as_array()
        .expect("argv array")
        .iter()
        .map(|v| v.as_str().expect("string arg").to_string())
        .collect();
    normalize_argv_session_id(&mut captured_argv);

    let fixture_argv: Vec<String> = fixture_argv(&fixture);
    assert_eq!(
        captured_argv, fixture_argv,
        "3.T1: spawned argv must equal the text-only fixture (uuid normalised)"
    );

    let env = captured["env"].as_object().expect("env object");
    assert!(
        env.contains_key("CLAUDE_CODE_ENTRYPOINT"),
        "3.T1: CLAUDE_CODE_ENTRYPOINT must be set"
    );
    assert_eq!(
        env.get("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS")
            .and_then(|v| v.as_str()),
        Some("1"),
        "3.T1: CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS must be 1"
    );
    assert_eq!(
        captured["node_options_present"].as_bool(),
        Some(false),
        "3.T1: NODE_OPTIONS must be removed"
    );

    process
        .shutdown(&Timings::default())
        .await
        .expect("shutdown fake-claude");
    let _ = std::fs::remove_dir_all(&tmp);
}

fn fixture_argv(path: &std::path::Path) -> Vec<String> {
    let value = read_json(path);
    value["argv"]
        .as_array()
        .expect("fixture argv array")
        .iter()
        .map(|v| v.as_str().expect("string arg").to_string())
        .collect()
}

/// 3.T5 — a child that writes to stderr then exits yields the full tail in the
/// reported error, within the stderr drain cap (INV-4).
#[tokio::test]
async fn inv_04_stderr_drained() {
    let marker = "STDERR_MARKER_12345";
    let opts = SpawnOptions {
        claude_path: Some(PathBuf::from("/bin/sh")),
        ..Default::default()
    };
    let argv = vec!["-c".to_string(), format!("echo '{marker}' >&2; exit 7")];
    let mut process = spawn_raw(PathBuf::from("/bin/sh"), argv, &opts)
        .await
        .expect("spawn stderr child");

    let status = process
        .wait_with_stderr(&Timings::default())
        .await
        .expect("child exits");
    assert_eq!(status.code(), Some(7), "child exit code is 7");

    let tail = process.stderr_tail();
    assert!(
        tail.contains(marker),
        "3.T5: stderr tail must contain the marker, got: {tail:?}"
    );
}

/// 3.T6 — a child ignoring stdin-close and SIGTERM is SIGTERMed only after
/// `stdin_close_wait` and SIGKILLed only after `term_to_kill_wait` (durations
/// shrunk via `Timings`). Also asserts a user abort (forwarded via stdin EOF)
/// goes through the ladder, never straight to SIGKILL (INV-5).
#[tokio::test]
async fn inv_05_kill_ladder() {
    let tmp = std::env::temp_dir().join(format!("acp-ladder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let term_log = tmp.join("term.log");

    // Child: never reads stdin, ignores SIGTERM, records a "TERM" marker on
    // SIGTERM, loops forever. It can only die by SIGKILL.
    let script = "import signal, sys, time\n\
         def h(signum, frame):\n\
         \x20   open(sys.argv[1], 'a').write('TERM\\n')\n\
         signal.signal(signal.SIGTERM, h)\n\
         while True:\n\
         \x20   time.sleep(1)\n"
        .to_string();
    let opts = SpawnOptions {
        claude_path: Some(PathBuf::from("/usr/bin/python3")),
        ..Default::default()
    };
    let argv = vec![
        "-c".to_string(),
        script,
        term_log.to_string_lossy().into_owned(),
    ];
    let mut process = spawn_raw(PathBuf::from("/usr/bin/python3"), argv, &opts)
        .await
        .expect("spawn ladder child");
    let pid = process.pid().expect("child pid");

    let timings = Timings {
        stdin_close_wait: Duration::from_millis(300),
        term_to_kill_wait: Duration::from_millis(300),
        ..Timings::default()
    };

    let started = Instant::now();
    process
        .shutdown(&timings)
        .await
        .expect("shutdown completes the ladder");
    let elapsed = started.elapsed();

    // The ladder waited both phases before SIGKILL (not straight to SIGKILL).
    assert!(
        elapsed >= timings.stdin_close_wait + timings.term_to_kill_wait,
        "3.T6: ladder must wait both phases, elapsed was {elapsed:?}"
    );

    // SIGTERM was sent before SIGKILL (the user abort went through the ladder).
    let log = std::fs::read_to_string(&term_log).unwrap_or_default();
    assert!(
        log.contains("TERM"),
        "3.T6: SIGTERM must have been delivered before SIGKILL; log={log:?}"
    );

    // The child is gone (killed by the ladder's SIGKILL).
    assert!(
        wait_pid_gone(pid, Duration::from_secs(5)),
        "3.T6: child must be gone after the ladder"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 3.T7 — after `dispose()`, `kill(pid,0)` returns `ESRCH` for the recorded
/// child pid (INV-6).
#[tokio::test]
async fn inv_06_no_orphan_after_dispose() {
    let opts = SpawnOptions {
        claude_path: Some(PathBuf::from("/bin/sh")),
        ..Default::default()
    };
    let mut process = spawn_raw(
        PathBuf::from("/bin/sh"),
        vec!["-c".to_string(), "sleep 100".to_string()],
        &opts,
    )
    .await
    .expect("spawn sleeping child");
    let pid = process.pid().expect("child pid");

    process.dispose(&Timings::default()).await;

    assert!(
        wait_pid_gone(pid, Duration::from_secs(5)),
        "3.T7: child pid {pid} must be gone (ESRCH) after dispose"
    );
}

/// 3.T8 — a helper binary spawns a child via the crate then is SIGKILLed; the
/// child (which exits on stdin EOF) is gone within 5 s (INV-28, host death).
#[cfg(unix)]
#[tokio::test]
async fn inv_28_host_sigkill_no_orphan() {
    let root = repo_root();
    let helper = build_example("spawn_child");

    let tmp = std::env::temp_dir().join(format!("acp-hostkill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let pid_out = tmp.join("child.pid");

    // Spawn the helper: it spawns `/bin/sh -c 'cat > /dev/null'` as the child.
    let mut helper_child = std::process::Command::new(&helper)
        .arg(&pid_out)
        .arg("/bin/sh")
        .arg("-c")
        .arg("cat > /dev/null")
        .spawn()
        .expect("spawn helper binary");
    let helper_pid = helper_child.id();

    wait_for_file(&pid_out, Duration::from_secs(10)).expect("helper records the child pid");
    let child_pid: u32 = std::fs::read_to_string(&pid_out)
        .expect("read child pid")
        .trim()
        .parse()
        .expect("parse child pid");

    // SIGKILL the host (helper).
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    kill(Pid::from_raw(helper_pid as i32), Signal::SIGKILL).expect("SIGKILL the helper");
    helper_child.wait().expect("reap helper");

    // The child exits on stdin EOF (its pipe closed with the dead host) within
    // 5 s — no group-kill from the dead host is needed (INV-28).
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(5)),
        "3.T8: child pid {child_pid} must be gone within 5 s of host SIGKILL"
    );
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = root;
}
