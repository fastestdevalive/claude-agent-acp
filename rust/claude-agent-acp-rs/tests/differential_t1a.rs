//! Turn 1 / lane A differentials: Node-captured fixtures for the settle-path
//! rows (F12, F13, F16a-c, F18, F19, F3b, AG-4, F3a). Each test runs the Rust
//! binary against the scenario's fake-`claude` transcript and diffs the ordered
//! ACP frames against the committed Node fixture, with NO ignore paths.

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, repo_root, run_recorder, run_recorder_with_argv,
    rust_agent_binary,
};

static NEXT_DIFF_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run the Rust binary against `name`'s corpus script via the recorder +
/// fake-claude and diff its frames against the committed Node fixture.
fn run_and_diff(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.frames.jsonl"));

    assert!(script.exists(), "missing script {script:?}");
    assert!(transcript.exists(), "missing transcript {transcript:?}");
    assert!(fixture.exists(), "missing fixture {fixture:?}");

    let tmp = std::env::temp_dir().join(format!(
        "acp-diff-t1a-{name}-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let out = tmp.join("out.jsonl");

    let agent_cmd = rust_agent_binary();
    assert!(
        agent_cmd.exists(),
        "rust agent binary missing: {}",
        agent_cmd.display()
    );
    let frames = run_recorder(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
    let frames = normalize_terminal_auth(normalize_root_paths(frames, &root));
    let expected = normalize_terminal_auth(parse_fixture_frames(&fixture));
    let (frames, expected) =
        normalize_agent_info_version(frames, expected, &pinned_upstream_version());
    if let Err(diff) = diff_frames(&frames, &expected, &[]) {
        panic!("{name} differential failed:\n{diff}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// F13 (N-01, N-02, B-06, B-32): on a `msg_lifecycle_v1` CLI two cancelled queued
/// commands folded into ONE turn emit one shared orphan result; the per-uuid map
/// drains both entries on it, so the next echo-less command's own result is not
/// swallowed (a count lane would go stale at 1 and hang it).
#[test]
fn t1a_f13_lifecycle_coalesced() {
    run_and_diff("lifecycle-coalesced");
}

/// F16a (N-06, B-35, B-36, N-29, B-13): a cancel during a held turn's LIVE
/// follow-up is left to that follow-up's interrupted (aborted) result, which
/// settles the turn `cancelled` with the usage its own result recorded; the
/// follow-up's next request names no prompt, so it reopens the window and that
/// later follow-up is delivered.
#[test]
fn t1a_f16a_cancel_held_live_followup() {
    run_and_diff("cancel-held-live-followup");
}

/// F16b (B-18): once the cancelled turn ended at its interrupted result, the late
/// streamed text, consolidated message and tool use of the abandoned work are
/// dropped (they never reach the client after the cancel was reported).
#[test]
fn t1a_f16b_cancel_stream_drop() {
    run_and_diff("cancel-stream-drop");
}

/// F16c (B-07, B-61): a model cycle Claude Code starts on its own (no turn echoed)
/// is foreground work, so after a cancel the cycle's own streamed output still
/// reaches the client until the cycle's result ends it; what comes after is the
/// stopped work and is dropped.
#[test]
fn t1a_f16c_cancel_autonomous_cycle() {
    run_and_diff("cancel-autonomous-cycle");
}

/// Run `name` and compare the argv of the LAST `claude` spawn with the Node
/// fixture (the frames cannot tell a reused session from a recreated one).
fn run_and_diff_argv(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let tmp = std::env::temp_dir().join(format!(
        "acp-argv-t1a-{name}-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let out = tmp.join("out.jsonl");
    let agent_cmd = rust_agent_binary();
    let (_frames, rust_argv) =
        run_recorder_with_argv(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
    let expected = common::read_json(&root.join(format!("porting/fixtures/{name}.argv.json")));
    assert_eq!(
        rust_argv, expected,
        "{name} argv diverges from the Node fixture"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// F19 (B-117): a `session/resume` of a LIVE session with unchanged
/// session-defining params reuses it (no second process); with changed params
/// (`additionalDirectories` here) the session is torn down and recreated under
/// the same id with `--resume`, so the process picks up the new values.
#[test]
fn t1a_f19_resume_live_fingerprint() {
    run_and_diff("resume-live-fingerprint-same");
    run_and_diff_argv("resume-live-fingerprint-same");
    run_and_diff("resume-live-fingerprint-changed");
    run_and_diff_argv("resume-live-fingerprint-changed");
}

/// F18 (V-10): after the stream ended the session stays as a husk: `set_mode`,
/// `set_config_option`, `_session/steering` and `session/prompt` all answer the
/// same "session ended" error instead of acting on a closed query.
#[test]
fn t1a_f18_husk_after_eof() {
    run_and_diff("husk-after-eof");
}

/// AG-4 (S-02): a `/mcp` result that reaches the adapter BEFORE the spawned
/// control run answered still publishes the produced markdown (the settle waits
/// for the run), not the CLI's text.
#[test]
fn t1a_ag4_mcp_result_before_control_run() {
    run_and_diff("mcp-command-result-first");
}

/// F3a (M-38, M-39): the error shapes of `set_mode` (invalid / unavailable mode) and
/// of the `mode` branch of `set_config_option` (invalid value, unknown option).
#[test]
fn t1a_f3a_config_set_mode_errors() {
    run_and_diff("config-set-mode-errors");
}

/// F3a (M-38): `set_config_option mode=auto` on a model without Auto clamps to the
/// fallback mode: the advisory notice, `current_mode_update`, no
/// `config_option_update`, and the response carries the options.
#[test]
fn t1a_f3a_config_set_mode_auto_clamp() {
    run_and_diff("config-set-mode-auto-config");
}

/// F3a (M-42): `set_config_option effort`: the SDK flag is applied first, a level
/// pins, `default` un-pins (`effortLevel: null`), and no `config_option_update`.
#[test]
fn t1a_f3a_config_set_effort_default() {
    run_and_diff("config-set-effort-default");
}

/// F12 (N-11): a transition into `running` sweeps the unpayable owed idle, so the
/// idle that ends the next turn without a result fails it (#825) instead of being
/// absorbed (which would leave the prompt hanging).
#[test]
fn t1a_f12_idle_running_sweep() {
    run_and_diff("idle-running-sweep");
}
