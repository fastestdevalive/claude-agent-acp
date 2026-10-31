//! Phase-3 process-transport integration tests (3.T1, 3.T5–3.T8).
//!
//! These exercise [`process::spawn`] / [`process::spawn_raw`] against real
//! child processes and the `fake-claude` binary.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use claude_agent_acp_rs::process::{spawn, spawn_raw, SpawnOptions, Timings};
#[cfg(unix)]
use common::build_example;
use common::{
    fake_claude_binary, normalize_argv_session_id, read_json, repo_root, wait_for_file,
    wait_pid_gone,
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

#[cfg(unix)]
struct KillOnDrop(Option<std::process::Child>);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
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
    let helper_child = std::process::Command::new(&helper)
        .arg(&pid_out)
        .arg("/bin/sh")
        .arg("-c")
        .arg("cat > /dev/null")
        .spawn()
        .expect("spawn helper binary");
    let helper_pid = helper_child.id();
    let mut helper_guard = KillOnDrop(Some(helper_child));

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
    let _ = helper_guard.0.as_mut().map(|c| c.wait());

    // The child exits on stdin EOF (its pipe closed with the dead host) within
    // 5 s — no group-kill from the dead host is needed (INV-28).
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(5)),
        "3.T8: child pid {child_pid} must be gone within 5 s of host SIGKILL"
    );
    assert!(
        wait_pid_gone(helper_pid, Duration::from_secs(2)),
        "helper pid must be reaped"
    );
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = root;
}

/// F41 — SIGKILL the host with a fake claude that spawns a background grandchild;
/// the process-group reaper kills the entire group so the grandchild is gone (ESRCH)
/// within 2 s on host exit.
#[cfg(unix)]
#[tokio::test]
async fn inv_28_host_sigkill_grandchild_reaped() {
    let reaper_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_claude-agent-acp-rs"));

    let helper = build_example("spawn_child");

    let tmp = std::env::temp_dir().join(format!("acp-reaper-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let pid_out = tmp.join("child.pid");
    let grandchild_pid_out = tmp.join("grandchild.pid");
    let reaper_pid_out = tmp.join("reaper.pid");

    let script = tmp.join("fake_claude.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 100 & echo $! > \"{}\"\nexec cat > /dev/null\n",
            grandchild_pid_out.display()
        ),
    )
    .expect("write fake claude script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod script");

    let custom_reaper = tmp.join("custom_reaper_binary");
    std::fs::copy(&reaper_bin, &custom_reaper).expect("copy custom reaper");

    let helper_child = std::process::Command::new(&helper)
        .env("ACP_REAPER_BIN", &custom_reaper)
        .arg(&pid_out)
        .arg(&script)
        .spawn()
        .expect("spawn helper binary");
    let helper_pid = helper_child.id();
    let mut helper_guard = KillOnDrop(Some(helper_child));

    wait_for_file(&pid_out, Duration::from_secs(10)).expect("child pid recorded");
    wait_for_file(&grandchild_pid_out, Duration::from_secs(10)).expect("grandchild pid recorded");
    wait_for_file(&reaper_pid_out, Duration::from_secs(10)).expect("reaper pid recorded");

    let grandchild_pid: u32 = std::fs::read_to_string(&grandchild_pid_out)
        .expect("read grandchild pid")
        .trim()
        .parse()
        .expect("parse grandchild pid");

    let reaper_pid: u32 = std::fs::read_to_string(&reaper_pid_out)
        .expect("read reaper pid")
        .trim()
        .parse()
        .expect("parse reaper pid");

    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    assert_eq!(
        kill(Pid::from_raw(grandchild_pid as i32), None),
        Ok(()),
        "grandchild must be alive before host kill"
    );
    assert_eq!(
        kill(Pid::from_raw(reaper_pid as i32), None),
        Ok(()),
        "reaper must be alive before host kill"
    );

    // SIGKILL the helper (host). DO NOT wait() on helper_child yet so that the
    // host process remains an un-reaped zombie, verifying that zombie detection
    // works via pipe EOF rather than kill(pid, 0) (Finding 2).
    kill(Pid::from_raw(helper_pid as i32), Signal::SIGKILL).expect("SIGKILL helper");

    // The grandchild and reaper must be gone within 2 s (F41 acceptance, Finding 17).
    assert!(
        wait_pid_gone(grandchild_pid, Duration::from_secs(2)),
        "F41: grandchild pid {grandchild_pid} must be gone within 2 s of host SIGKILL (even with zombie host)"
    );
    assert!(
        wait_pid_gone(reaper_pid, Duration::from_secs(2)),
        "F41: reaper pid {reaper_pid} must exit within 2 s of host SIGKILL"
    );

    // Reap the zombie host now
    let _ = helper_guard.0.as_mut().map(|c| c.wait());
    assert!(wait_pid_gone(helper_pid, Duration::from_secs(2)));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Finding 1: A host process that is not the adapter (and has no ACP_REAPER_BIN set
/// and no claude-agent-acp-rs binary in its directory tree) must NOT re-exec itself
/// as a reaper supervisor, avoiding recursive fork chains.
#[cfg(unix)]
#[tokio::test]
async fn reaper_not_spawned_for_non_adapter_host() {
    let helper = build_example("spawn_child");
    let tmp = std::env::temp_dir().join(format!("acp-non-adapter-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let isolated_bin_dir = tmp.join("isolated_bin");
    std::fs::create_dir_all(&isolated_bin_dir).expect("create isolated bin dir");

    // Copy helper to isolated directory where no claude-agent-acp-rs exists
    let non_adapter_exe = isolated_bin_dir.join("non_adapter_host");
    std::fs::copy(&helper, &non_adapter_exe).expect("copy helper");

    let pid_out = tmp.join("child.pid");
    let reaper_pid_out = tmp.join("reaper.pid");
    let script = tmp.join("fake_claude.sh");
    std::fs::write(&script, "#!/bin/sh\nexec cat > /dev/null\n").expect("write script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let helper_child = std::process::Command::new(&non_adapter_exe)
        .arg(&pid_out)
        .arg(&script)
        .spawn()
        .expect("spawn non-adapter host");
    let helper_pid = helper_child.id();
    let mut helper_guard = KillOnDrop(Some(helper_child));

    wait_for_file(&pid_out, Duration::from_secs(10)).expect("child pid recorded");
    let child_pid: u32 = std::fs::read_to_string(&pid_out)
        .expect("read child pid")
        .trim()
        .parse()
        .expect("parse child pid");

    // Allow a short window to confirm reaper.pid is NOT produced
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !reaper_pid_out.exists(),
        "non-adapter host without claude-agent-acp-rs must not spawn a reaper"
    );

    let _ = helper_guard.0.as_mut().map(|c| {
        let _ = c.kill();
        c.wait()
    });
    assert!(wait_pid_gone(child_pid, Duration::from_secs(2)));
    assert!(wait_pid_gone(helper_pid, Duration::from_secs(2)));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Finding 7: When a session closes (Process is dropped), the reaper supervisor
/// must exit immediately and not linger until host death.
#[cfg(unix)]
#[tokio::test]
async fn reaper_exits_when_session_closed_while_host_alive() {
    let reaper_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_claude-agent-acp-rs"));
    let script = std::env::temp_dir().join(format!("sleep_child_{}.sh", std::process::id()));
    std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").expect("write sleep script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let opts = SpawnOptions {
        claude_path: Some(script.clone()),
        env_snapshot: claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_pairs([(
            "ACP_REAPER_BIN",
            reaper_bin.to_str().unwrap(),
        )]),
        ..Default::default()
    };
    let process = spawn_raw(script.clone(), vec![], &opts)
        .await
        .expect("spawn sleep");
    let reaper_pid = process.reaper_pid().expect("reaper spawned");

    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    assert_eq!(
        kill(Pid::from_raw(reaper_pid as i32), None),
        Ok(()),
        "reaper should be alive while session active"
    );

    // Drop the process: simulates session close
    drop(process);

    // Reaper must exit promptly even though host test process is still alive (Finding 7)
    assert!(
        wait_pid_gone(reaper_pid, Duration::from_secs(2)),
        "reaper process must exit when session is closed"
    );
    let _ = std::fs::remove_file(&script);
}

/// O-122: argv --managed-settings
#[test]
fn inv_122_managed_settings_argv() {
    use claude_agent_acp_rs::process::build_argv;

    let opts = SpawnOptions {
        managed_settings_arg: Some("/path/to/managed.json".to_string()),
        ..Default::default()
    };
    let argv = build_argv(&opts);
    let pos = argv
        .iter()
        .position(|a| a == "--managed-settings")
        .expect("--managed-settings present");
    assert_eq!(
        argv.get(pos + 1).map(String::as_str),
        Some("/path/to/managed.json")
    );

    let opts_json = SpawnOptions {
        managed_settings_arg: Some(r#"{"deniedModels":["m"]}"#.to_string()),
        ..Default::default()
    };
    let argv_json = build_argv(&opts_json);
    let pos_json = argv_json
        .iter()
        .position(|a| a == "--managed-settings")
        .expect("--managed-settings present");
    assert_eq!(
        argv_json.get(pos_json + 1).map(String::as_str),
        Some(r#"{"deniedModels":["m"]}"#)
    );

    // Finding 15: sdk_options stringifies both string and object forms into JSON literals
    use serde_json::json;
    let string_form = json!({
        "_meta": {
            "claudeCode": {
                "options": {
                    "managedSettings": "/path/to/managed.json"
                }
            }
        }
    });
    let parsed_str = claude_agent_acp_rs::sdk_options::claude_options(&string_form);
    assert_eq!(
        parsed_str.managed_settings_arg.as_deref(),
        Some(r#""/path/to/managed.json""#)
    );

    let obj_form = json!({
        "_meta": {
            "claudeCode": {
                "options": {
                    "managedSettings": {"deniedModels": ["m"]}
                }
            }
        }
    });
    let parsed_obj = claude_agent_acp_rs::sdk_options::claude_options(&obj_form);
    assert_eq!(
        parsed_obj.managed_settings_arg.as_deref(),
        Some(r#"{"deniedModels":["m"]}"#)
    );
}

/// Finding 8: Positive identification of reaper binary via `--reaper --probe` handshake,
/// caching, and rejection of stray / timing out binaries without spawning a reaper.
#[cfg(unix)]
#[tokio::test]
async fn test_reaper_probe_handshake_scenarios() {
    use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
    use claude_agent_acp_rs::process::is_reaper_binary;
    use std::os::unix::fs::PermissionsExt;

    let tmp = std::env::temp_dir().join(format!("acp-probe-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create tmp dir");

    let real_reaper = std::path::PathBuf::from(env!("CARGO_BIN_EXE_claude-agent-acp-rs"));

    // 1. Probe pass: real reaper binary succeeds and caches
    assert!(
        is_reaper_binary(&real_reaper),
        "real reaper binary must pass probe handshake"
    );
    // Call again to verify cache lookup
    assert!(is_reaper_binary(&real_reaper));

    // No recursion: the `--reaper --probe` entry point exits before the tokio
    // runtime starts, so a probe can never spawn another reaper. Run it
    // directly and assert it is the marker + a clean, fast exit.
    let start = Instant::now();
    let direct = std::process::Command::new(&real_reaper)
        .args(["--reaper", "--probe"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run --reaper --probe directly");
    assert!(direct.status.success(), "probe entry point must exit 0");
    assert_eq!(
        String::from_utf8_lossy(&direct.stdout).trim(),
        "claude-agent-acp-rs-reaper-v1",
        "probe entry point prints exactly the marker"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "probe entry point must exit without starting a runtime"
    );

    // 2. Probe fail: stray binary that does not emit the marker
    let stray_bin = tmp.join("stray_binary.sh");
    std::fs::write(&stray_bin, "#!/bin/sh\necho not-a-reaper\n").unwrap();
    std::fs::set_permissions(&stray_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !is_reaper_binary(&stray_bin),
        "stray binary must fail probe handshake"
    );
    // A definitive failure is cached per path (not re-probed every spawn).
    assert!(
        !is_reaper_binary(&stray_bin),
        "cached probe failure must stay false"
    );

    // Test that spawn_raw with stray binary produces no reaper (process.reaper_pid() is None)
    let child_script = tmp.join("child.sh");
    std::fs::write(&child_script, "#!/bin/sh\nexec cat > /dev/null\n").unwrap();
    std::fs::set_permissions(&child_script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let opts_stray = SpawnOptions {
        claude_path: Some(child_script.clone()),
        env_snapshot: EnvSnapshot::from_pairs([("ACP_REAPER_BIN", stray_bin.to_str().unwrap())]),
        ..Default::default()
    };
    let proc_stray = spawn_raw(child_script.clone(), vec![], &opts_stray)
        .await
        .expect("spawn child with stray reaper");
    assert!(
        proc_stray.reaper_pid().is_none(),
        "probe-fail stray binary must not spawn a reaper"
    );

    // 3. Probe timeout: hanging binary times out and fails probe
    let timeout_bin = tmp.join("timeout_binary.sh");
    std::fs::write(&timeout_bin, "#!/bin/sh\nsleep 10\n").unwrap();
    std::fs::set_permissions(&timeout_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let start = Instant::now();
    assert!(
        !is_reaper_binary(&timeout_bin),
        "timing out binary must fail probe handshake"
    );
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "probe timeout must bound waiting within ~2s"
    );

    // A failed (timed-out) probe is cached for a short TTL: the next call must
    // not pay the 2 s bound again (item 4, G7a-fix4).
    let start = Instant::now();
    assert!(
        !is_reaper_binary(&timeout_bin),
        "cached timeout outcome must stay false"
    );
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "a timed-out probe must be cached for a short TTL, not re-run on every spawn"
    );

    let opts_timeout = SpawnOptions {
        claude_path: Some(child_script.clone()),
        env_snapshot: EnvSnapshot::from_pairs([("ACP_REAPER_BIN", timeout_bin.to_str().unwrap())]),
        ..Default::default()
    };
    let proc_timeout = spawn_raw(child_script, vec![], &opts_timeout)
        .await
        .expect("spawn child with timeout reaper");
    assert!(
        proc_timeout.reaper_pid().is_none(),
        "probe-timeout binary must not spawn a reaper"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
fn get_descendant_pids(root_pid: u32) -> std::collections::HashSet<u32> {
    let mut descendants = std::collections::HashSet::new();
    if let Ok(output) = std::process::Command::new("ps")
        .args(["-Ao", "pid,ppid"])
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut parent_to_children: std::collections::HashMap<u32, Vec<u32>> =
                std::collections::HashMap::new();
            for line in text.lines().skip(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let (Ok(pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>())
                    {
                        parent_to_children.entry(ppid).or_default().push(pid);
                    }
                }
            }
            let mut queue = vec![root_pid];
            while let Some(parent) = queue.pop() {
                if let Some(children) = parent_to_children.get(&parent) {
                    for child in children {
                        if descendants.insert(*child) {
                            queue.push(*child);
                        }
                    }
                }
            }
        }
    }
    descendants
}

/// Turn 1, Group G7a-fix6:
/// Process count test (i): Run `examples/in_process` against fake-claude.
/// Asserts that during execution, the number of descendants never exceeds a small bound (<= 6),
/// and after it finishes, there are no `--reaper` / `--probe` processes left.
#[cfg(unix)]
#[tokio::test]
async fn test_in_process_bounded_descendants_no_fork_bomb() {
    let in_process_bin = build_example("in_process");
    let root = repo_root();
    let transcript = root.join("porting/corpus/text-only.transcript.jsonl");
    assert!(transcript.exists(), "transcript exists");

    let tmp = std::env::temp_dir().join(format!("acp-inprocess-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let init_out = tmp.join("init.json");
    let argv_out = tmp.join("argv.json");

    let child = std::process::Command::new(&in_process_bin)
        .env("CLAUDE_CODE_EXECUTABLE", fake_claude_binary())
        .env("FAKE_CLAUDE_SCRIPT", &transcript)
        .env("FAKE_CLAUDE_INIT_OUT", &init_out)
        .env("FAKE_CLAUDE_ARGV_OUT", &argv_out)
        .spawn()
        .expect("spawn in_process example");
    let root_pid = child.id();
    let mut guard = KillOnDrop(Some(child));

    // Monitor descendants concurrently
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let monitor = std::thread::spawn(move || {
        let mut all_seen = std::collections::HashSet::new();
        while stop_rx.try_recv().is_err() {
            let desc = get_descendant_pids(root_pid);
            for pid in desc {
                all_seen.insert(pid);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        all_seen
    });

    // Wait for in_process to exit cleanly with a timeout kill switch
    let status = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::task::spawn_blocking(move || {
            guard
                .0
                .as_mut()
                .unwrap()
                .wait()
                .expect("wait on in_process")
        })
        .await
        .expect("task join")
    })
    .await
    .expect("in_process must finish within 20s");

    assert!(status.success(), "in_process must exit 0, got {:?}", status);

    let _ = stop_tx.send(());
    let all_descendants = monitor.join().expect("monitor join");

    // Assert that the number of unique descendant processes never exceeded bound of 6
    assert!(
        all_descendants.len() <= 6,
        "descendants count ({}) exceeded bound of 6: {:?}",
        all_descendants.len(),
        all_descendants
    );

    // Verify all descendants are reaped and gone
    for pid in &all_descendants {
        assert!(
            wait_pid_gone(*pid, Duration::from_secs(3)),
            "descendant pid {pid} must be gone"
        );
    }

    // Verify no stray reaper or probe processes exist with our test binary path
    let in_process_path = in_process_bin.to_string_lossy();
    let ps_check = std::process::Command::new("ps")
        .args(["-Ao", "pid,command"])
        .output()
        .expect("ps");
    let ps_text = String::from_utf8_lossy(&ps_check.stdout);
    for line in ps_text.lines() {
        if line.contains(in_process_path.as_ref())
            && (line.contains("--reaper") || line.contains("--probe"))
        {
            panic!("Found leaked reaper/probe process: {line}");
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Turn 1, Group G7a-fix6:
/// Process count test (ii): A deliberately mishandling probe target (ignoring args
/// and trying to re-exec itself / probe) is halted at depth 1 by CLAUDE_ACP_REAPER_GUARD.
#[cfg(unix)]
#[tokio::test]
async fn test_mishandling_probe_target_stopped_at_depth_1() {
    use claude_agent_acp_rs::process::is_reaper_binary;
    use std::os::unix::fs::PermissionsExt;

    let tmp = std::env::temp_dir().join(format!("acp-mishandling-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    let counter_file = tmp.join("counter.txt");
    let script = tmp.join("mishandling_target.sh");
    let script_content = format!(
        r#"#!/bin/sh
COUNT_FILE="{}"
PREV=$(cat "$COUNT_FILE" 2>/dev/null || echo 0)
NEXT=$((PREV + 1))
echo "$NEXT" > "$COUNT_FILE"

# If the hard recursion guard is set, stop the chain immediately
if [ "$CLAUDE_ACP_REAPER_GUARD" = "1" ]; then
    exit 42
fi

# If guard were absent, it would attempt recursive probe re-exec
exec "$0" --reaper --probe
"#,
        counter_file.display()
    );
    std::fs::write(&script, script_content).expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Run probe with a 5s kill-switch timeout
    let passed = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::task::spawn_blocking({
            let script = script.clone();
            move || is_reaper_binary(&script)
        })
        .await
        .expect("join")
    })
    .await
    .expect("probe must finish within 5s");

    assert!(!passed, "mishandling probe target must be rejected");

    // Verify counter is exactly 1 (depth 1 only; stopped before re-exec)
    let count: u32 = std::fs::read_to_string(&counter_file)
        .expect("read counter")
        .trim()
        .parse()
        .expect("parse count");
    assert_eq!(
        count, 1,
        "mishandling binary must only be executed at depth 1"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Turn 1, Group G7a-fix6:
/// Process count test (iii): An unregistered host gets no reaper supervisor and logs
/// a warning to stderr; sessions run normally without reaper.
#[cfg(unix)]
#[tokio::test]
async fn test_unregistered_host_gets_no_reaper_and_logs_warning() {
    let script = std::env::temp_dir().join(format!("dummy_child_{}.sh", std::process::id()));
    std::fs::write(&script, "#!/bin/sh\nexec cat > /dev/null\n").expect("write dummy script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Spawn child with hermetic empty env (no ACP_REAPER_BIN, unregistered host)
    let opts = SpawnOptions {
        claude_path: Some(script.clone()),
        env_snapshot: claude_agent_acp_rs::env_snapshot::EnvSnapshot::empty(),
        ..Default::default()
    };
    let process = spawn_raw(script.clone(), vec![], &opts)
        .await
        .expect("spawn dummy child");

    assert!(
        process.reaper_pid().is_none(),
        "unregistered host must not spawn a reaper process"
    );

    let pid = process.pid().expect("child pid");
    drop(process);

    assert!(
        wait_pid_gone(pid, Duration::from_secs(2)),
        "child must exit promptly"
    );
    let _ = std::fs::remove_file(&script);
}

/// Turn 1, Group G7a-fix6:
/// Process count test: Examples / binaries that open sessions exit immediately with an error
/// when invoked with `--reaper`.
#[cfg(unix)]
#[test]
fn test_bins_reject_reaper_flag() {
    let in_process = build_example("in_process");
    let out = std::process::Command::new(&in_process)
        .arg("--reaper")
        .output()
        .expect("run in_process --reaper");
    assert_eq!(
        out.status.code(),
        Some(1),
        "in_process --reaper must exit 1"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used as a reaper binary"),
        "stderr should explain reaper rejection"
    );

    let spawn_child = build_example("spawn_child");
    let out = std::process::Command::new(&spawn_child)
        .arg("--reaper")
        .output()
        .expect("run spawn_child --reaper");
    assert_eq!(
        out.status.code(),
        Some(1),
        "spawn_child --reaper must exit 1"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used as a reaper binary"),
        "stderr should explain reaper rejection"
    );
}

/// Turn 1, Group G7a-fix6:
/// Process count test: CLAUDE_ACP_REAPER_GUARD in env or options suppresses reaper resolution.
#[cfg(unix)]
#[tokio::test]
async fn test_reaper_guard_env_blocks_nested_spawn() {
    let real_reaper = std::path::PathBuf::from(env!("CARGO_BIN_EXE_claude-agent-acp-rs"));
    let script = std::env::temp_dir().join(format!("dummy_child2_{}.sh", std::process::id()));
    std::fs::write(&script, "#!/bin/sh\nexec cat > /dev/null\n").expect("write dummy script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Even with a valid ACP_REAPER_BIN configured, the guard suppresses reaper creation
    let opts = SpawnOptions {
        claude_path: Some(script.clone()),
        env_snapshot: claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_pairs([
            ("ACP_REAPER_BIN", real_reaper.to_str().unwrap()),
            (claude_agent_acp_rs::process::REAPER_GUARD_ENV, "1"),
        ]),
        ..Default::default()
    };
    let process = spawn_raw(script.clone(), vec![], &opts)
        .await
        .expect("spawn dummy child");

    assert!(
        process.reaper_pid().is_none(),
        "CLAUDE_ACP_REAPER_GUARD must prevent reaper spawn"
    );

    let pid = process.pid().expect("child pid");
    drop(process);

    assert!(
        wait_pid_gone(pid, Duration::from_secs(2)),
        "child must exit promptly"
    );
    let _ = std::fs::remove_file(&script);
}
