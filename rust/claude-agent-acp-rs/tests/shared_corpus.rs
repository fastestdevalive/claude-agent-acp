//! Shared differential corpus scenarios (item F44 part 1).
//!
//! Replays the scenarios defined in `porting/parity/SHARED.md` against fake-claude
//! and diffs the ordered ACP frames against the Node fixture recorded at the pinned
//! upstream version (0.94.0), with NO ignore paths.

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, repo_root, run_recorder, rust_agent_binary,
};

static NEXT_DIFF_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn run_and_diff(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.frames.jsonl"));

    assert!(script.exists(), "missing script {script:?}");
    assert!(transcript.exists(), "missing transcript {transcript:?}");
    assert!(fixture.exists(), "missing fixture {fixture:?}");

    let tmp = std::env::temp_dir().join(format!(
        "acp-diff-shared-{name}-{}-{}",
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

/// Shared scenario: queued_prompt (F44)
/// Two consecutive prompts with `no_wait`; second prompt queues and executes
/// after the first completes.
#[test]
fn shared_queued_prompt() {
    run_and_diff("queued_prompt");
}

/// Shared scenario: resume_recall (F44)
/// Session created, state stored in prompt 1, session closed and resumed, recall
/// asserted in prompt 2.
#[test]
fn shared_resume_recall() {
    run_and_diff("resume_recall");
}

/// Shared scenario: background_after_prompt (F44)
/// A turn starts a background task; the turn result arrives first and the
/// background lifecycle is reported on the session stream.
#[test]
fn shared_background_after_prompt() {
    run_and_diff("background_after_prompt");
}

/// Shared scenario: session_list_close_delete_fork (F44)
/// Session lifecycle methods: list, close, fork (after close), delete.
#[test]
fn shared_session_list_close_delete_fork() {
    run_and_diff("session_list_close_delete_fork");
}

/// Shared scenario: subagent (F44)
/// A Task tool call launches a child agent; the child lifecycle is reported on
/// the session stream.
#[test]
fn shared_subagent() {
    run_and_diff("subagent");
}
